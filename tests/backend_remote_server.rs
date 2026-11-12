//! End-to-end tests for the remote-control HTTP/WebSocket server.
//!
//! `tokio-tungstenite` is not a dependency of this crate, so the client side is
//! a hand-rolled RFC 6455 speaker over a blocking `TcpStream` (client frames
//! masked, server frames not) driving the real protocol helpers from
//! `spritx_terminal::backend::remote`. Everything runs on loopback with an ephemeral
//! port and a temporary device store, so no user config is touched.
//!
//! `Sec-WebSocket-Accept` is not verified: checking it needs SHA-1 and this
//! crate only depends on `sha2`. The 101 response line and the `Upgrade` /
//! `Sec-WebSocket-Accept` header *names* are checked instead.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spritx_terminal::backend::remote::{
    ClientFrame, Control, DeviceStore, Half, ServerFrame, TabInfo, CLIENT_TO_SERVER, KEY_LEN,
    NONCE_LEN, READY, client_session, new_device_id, pair_request, random, to_array, unb64,
};
use spritx_terminal::backend::remote_server::{
    Bind, MAX_CONNECTIONS, RemoteInput, RemoteServer, ServerOptions, listing_json,
};

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

fn temp_store(tag: &str) -> DeviceStore {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "spritx_terminal_remote_srv_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the temp store");
    DeviceStore::new(root)
}

/// A running server with two shared panes: leaf 7 ("shell") and leaf 8 ("api").
struct Harness {
    server: RemoteServer,
}

impl Harness {
    fn start(tag: &str) -> Self {
        Self::with_store(temp_store(tag))
    }

    fn with_store(store: DeviceStore) -> Self {
        let server = RemoteServer::start(ServerOptions {
            port: 0,
            bind: Bind::Loopback,
            store: Some(store),
            tunnel: false,
            ..ServerOptions::default()
        })
        .expect("start the remote server");
        server.set_tabs(vec![
            TabInfo {
                id: 7,
                title: "shell".into(),
                tab: "spritx".into(),
                tab_id: 1,
                activity: Default::default(),
            },
            TabInfo {
                id: 8,
                title: "api".into(),
                tab: "spritx".into(),
                tab_id: 1,
                activity: Default::default(),
            },
        ]);
        server.set_active_leaf(7);
        Self { server }
    }

    /// Pair a device against the live single-use code and connect it.
    fn pair_and_connect(&self, name: &str) -> (WsClient, Session, Device) {
        let secret = self.server.pairing_secret().expect("a pairing is open");
        let device = Device::new();
        let mut client = WsClient::connect(self.server.port());
        client.expect_server_hello();
        client.send_text(&pair_request(&secret, &device.id, &device.key, name));
        assert!(matches!(client.recv_control(), Control::Paired));
        let session = client.authenticate(&device.id, &device.key);
        (client, session, device)
    }

    /// Connect an already-paired device (no pairing code involved).
    fn connect_paired(&self, device_id: &str, device_key: &[u8; KEY_LEN]) -> (WsClient, Session) {
        let mut client = WsClient::connect(self.server.port());
        client.expect_server_hello();
        let session = client.authenticate(device_id, device_key);
        (client, session)
    }

    /// Drain the desktop-action queue until a predicate matches. The bridge
    /// tasks push frames asynchronously, so this is how the tests stay
    /// deterministic without sleeping on guesses.
    fn wait_for_input(&self, want: impl Fn(&RemoteInput) -> bool) -> RemoteInput {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            for input in self.server.take_inputs() {
                if want(&input) {
                    return input;
                }
                seen.push(input);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no matching remote input within the deadline, saw {seen:?}");
    }

    /// Consume the tabs frame a freshly attached client is announced with. The
    /// pane's screen itself now arrives as the raw replay tail, so tests that
    /// want initial content must `ingest` it before connecting.
    fn accept_attach(&self, client: &mut WsClient, session: &mut Session) {
        let (items, active) = client.expect_tabs(session);
        assert_eq!(items.len(), 2, "both panes are shared");
        assert_eq!(active, Some(7), "the desktop's active pane");
    }

    fn clients(&self) -> usize {
        // The client counter is live; the rest of the status may be cached.
        self.server.status().clients
    }

    fn wait_for_clients(&self, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.clients() == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("client count is {}, wanted {want}", self.clients());
    }
}

/// A paired device, so a test can reconnect without a new pairing code.
struct Device {
    id: String,
    key: [u8; KEY_LEN],
}

impl Device {
    fn new() -> Self {
        Self {
            id: new_device_id(),
            key: random::<KEY_LEN>(),
        }
    }
}

/// The two sealed halves of an established channel.
struct Session {
    tx: Half,
    rx: Half,
}

// ---------------------------------------------------------------------------
// A minimal RFC 6455 client
// ---------------------------------------------------------------------------

struct WsClient {
    socket: TcpStream,
    mask_counter: u32,
}

impl WsClient {
    fn connect(port: u16) -> Self {
        let mut socket =
            TcpStream::connect(("127.0.0.1", port)).expect("connect to the remote server");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set a read timeout");
        let key = "AAECAwQFBgcICQoLDA0ODw==";
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        socket.write_all(request.as_bytes()).expect("send the upgrade");
        socket.flush().expect("flush the upgrade");

        let head = read_until_double_newline(&mut socket);
        let status = head.lines().next().unwrap_or_default().to_ascii_lowercase();
        assert!(status.starts_with("http/1.1 101"), "{status}");
        let lowered = head.to_ascii_lowercase();
        assert!(lowered.contains("upgrade: websocket"), "{head:?}");
        assert!(
            lowered.contains("sec-websocket-accept:"),
            "{head:?}"
        );
        Self {
            socket,
            mask_counter: 0x5050_5051,
        }
    }

    /// `hello` → `challenge` → sealed `READY`, exactly like a paired SpritX.
    fn authenticate(&mut self, device_id: &str, device_key: &[u8; KEY_LEN]) -> Session {
        let nc = random::<NONCE_LEN>();
        self.send_text(&Control::hello_request(device_id, &nc).to_json());
        let Control::Challenge { ns } = self.recv_control() else {
            panic!("expected a challenge frame");
        };
        let ns = to_array::<NONCE_LEN>(&unb64(&ns).expect("base64 nonce"))
            .expect("a 24-byte nonce");
        let (mut tx, rx) = client_session(device_key, &nc, &ns);
        self.send_binary(&tx.seal(&READY));
        Session { tx, rx }
    }

    fn send_text(&mut self, text: &str) {
        self.send_frame(0x1, text.as_bytes());
    }

    fn send_binary(&mut self, bytes: &[u8]) {
        self.send_frame(0x2, bytes);
    }

    fn send_close(&mut self) {
        self.send_frame(0x8, &[]);
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        self.mask_counter = self.mask_counter.wrapping_add(1);
        let mask = self.mask_counter.to_be_bytes();
        let mut out = vec![0x80 | opcode];
        match payload.len() {
            0..=125 => out.push(0x80 | payload.len() as u8),
            len @ 126..=0xffff => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4])
            .collect();
        out.extend_from_slice(&masked);
        self.socket.write_all(&out).expect("write a ws frame");
        self.socket.flush().expect("flush a ws frame");
    }

    /// Read `count` bytes, or `None` when the peer closed the connection first.
    fn read_exact(&mut self, count: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; count];
        let mut filled = 0;
        while filled < count {
            match self.socket.read(&mut buf[filled..]) {
                Ok(0) => return None,
                Ok(n) => filled += n,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!("no frame from the server within the read timeout");
                }
                Err(error) => panic!("read a ws frame: {error}"),
            }
        }
        Some(buf)
    }

    /// One server data frame; `None` once the connection is closed. Ping/pong
    /// frames are skipped. Split frames are not expected from this server.
    fn recv_payload(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            let head = self.read_exact(2)?;
            let opcode = head[0] & 0x0f;
            assert_eq!(head[0] & 0x40, 0, "no fragmentation expected");
            assert_eq!(head[1] & 0x80, 0, "server frames are not masked");
            let mut len = (head[1] & 0x7f) as usize;
            if len == 126 {
                let bytes: [u8; 2] = self.read_exact(2)?[..].try_into().expect("2 bytes");
                len = u16::from_be_bytes(bytes) as usize;
            } else if len == 127 {
                let bytes: [u8; 8] = self.read_exact(8)?[..].try_into().expect("8 bytes");
                len = u64::from_be_bytes(bytes) as usize;
            }
            let payload = if len == 0 {
                Vec::new()
            } else {
                self.read_exact(len)?
            };
            match opcode {
                0x9 | 0xa => continue,
                0x8 => return None,
                opcode => return Some((opcode, payload)),
            }
        }
    }

    /// A plaintext JSON handshake frame.
    fn recv_control(&mut self) -> Control {
        let (opcode, payload) = self.recv_payload().expect("a control frame (the server closed)");
        assert_eq!(opcode, 0x1, "expected a text frame, got {opcode:#x}");
        let text = String::from_utf8(payload).expect("UTF-8 control frame");
        Control::from_json(&text).unwrap_or_else(|| panic!("not control json: {text}"))
    }

    /// The `server` hello frame the server must send first.
    fn expect_server_hello(&mut self) {
        match self.recv_control() {
            Control::Server { v, id, name } => {
                assert_eq!(v, 2, "protocol version");
                assert!(!id.is_empty(), "the server announces its id");
                assert!(!name.is_empty(), "the server announces its name");
            }
            other => panic!("expected the server hello, got {other:?}"),
        }
    }

    /// A sealed server frame, opened with the connection's receive half. A frame
    /// that fails to open would be a protocol bug, so it panics.
    fn recv_frame(&mut self, session: &mut Session) -> ServerFrame {
        let (opcode, payload) = self.recv_payload().expect("a sealed frame (the server closed)");
        assert_eq!(opcode, 0x2, "expected a binary frame, got {opcode:#x}");
        ServerFrame::open(&mut session.rx, &payload).expect("every server frame authenticates")
    }

    fn expect_output(&mut self, session: &mut Session, want: &[u8]) {
        match self.recv_frame(session) {
            ServerFrame::Output(bytes) => assert_eq!(bytes, want, "terminal output"),
            other => panic!("expected an output frame, got {other:?}"),
        }
    }

    fn expect_tabs(&mut self, session: &mut Session) -> (Vec<TabInfo>, Option<u64>) {
        match self.recv_frame(session) {
            ServerFrame::Control(Control::Tabs { items, active }) => (items, active),
            other => panic!("expected a tabs control frame, got {other:?}"),
        }
    }

    fn expect_denied(&mut self, reason: &str) {
        match self.recv_control() {
            Control::Denied { reason: got } => assert_eq!(got, reason),
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    /// Assert the server sends nothing for `wait` — the way a test checks that
    /// output for another pane does not leak into this connection.
    fn expect_quiet(&mut self, wait: Duration) {
        self.socket.set_read_timeout(Some(wait)).expect("set timeout");
        let mut probe = [0u8; 1];
        let outcome = self.socket.read(&mut probe);
        self.socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("restore timeout");
        match outcome {
            Ok(0) => panic!("the server closed the connection"),
            Ok(n) => panic!("unexpected frame traffic ({n} bytes)"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("read failed: {error}"),
        }
    }

    /// The connection must be gone (or go away) from now on.
    fn expect_closed(&mut self) {
        match self.recv_payload() {
            None => {}
            Some((opcode, payload)) => panic!("expected a closed socket, got {opcode:#x} {payload:?}"),
        }
    }
}

fn read_until_double_newline(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1];
    while socket.read(&mut buf).unwrap_or(0) == 1 {
        bytes.push(buf[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A plain (non-upgrade) HTTP GET, returning the whole response.
fn http_get(port: u16, path: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    socket.write_all(request.as_bytes()).expect("send the request");
    socket.flush().expect("flush the request");
    let mut response = String::new();
    let mut buf = [0u8; 4096];
    loop {
        match socket.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("read the response: {error}"),
        }
    }
    response
}

/// The HTTP response head for a raw WebSocket upgrade attempt, without
/// requiring the request to succeed. This exposes connection-cap rejection.
fn raw_ws_upgrade_head(port: u16) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect for upgrade probe");
    socket.set_read_timeout(Some(Duration::from_secs(3))).expect("set probe timeout");
    let request = format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).expect("send upgrade probe");
    socket.flush().expect("flush upgrade probe");
    read_until_double_newline(&mut socket)
}

fn listing_names(cwd: &std::path::Path, token: &str) -> Vec<String> {
    let json = listing_json(cwd.to_str(), token);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("listing JSON");
    assert_eq!(parsed["t"], "ls", "listing shape: {json}");
    parsed["items"]
        .as_array()
        .expect("listing items array")
        .iter()
        .map(|item| item[0].as_str().expect("entry name").to_string())
        .collect()
}

/// Preserve additions while keeping this file's existing test identities intact.
///
/// These are the remote-server regression additions.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------


#[test]
fn index_page_is_served_and_fails_closed() {
    let harness = Harness::start("index");
    let response = http_get(harness.server.port(), "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{}", &response[..60]);
    assert!(response.to_ascii_lowercase().contains("cache-control: no-store"));
    assert!(response.contains("SpritX Remote"));
    // The page speaks the JSON handshake but must not run a plaintext terminal.
    assert!(response.contains("/ws"));
    assert!(
        response.contains("will not fall back to plaintext"),
        "the page must document that it refuses to downgrade"
    );
    assert!(response.contains("<link rel=\"icon\" href=\"/favicon.ico\""));
    assert!(response.contains("<img src=\"/app-icon.png\" alt=\"SpritX\""));
    for (path, content_type) in [("/favicon.ico", "image/x-icon"), ("/app-icon.png", "image/png")] {
        let asset = http_get(harness.server.port(), path);
        assert!(asset.starts_with("HTTP/1.1 200"), "missing app artwork: {path}");
        assert!(asset.to_ascii_lowercase().contains(&format!("content-type: {content_type}")));
    }
}

#[test]
fn full_handshake_bridges_output_and_input() {
    let harness = Harness::start("bridge");
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);

    // PTY output the desktop ingests reaches the client as sealed `B` frames.
    harness.server.ingest(7, b"$ echo hi\r\n");
    client.expect_output(&mut session, b"$ echo hi\r\n");
    // Output of another pane does not.
    harness.server.ingest(8, b"other pane");
    client.expect_quiet(Duration::from_millis(200));

    // Sealed input is handed to the desktop for *this* connection's pane.
    client.send_binary(&ClientFrame::Input(b"ls\r".to_vec()).seal(&mut session.tx));
    let input = harness.wait_for_input(|input| matches!(input, RemoteInput::Write { .. }));
    match input {
        RemoteInput::Write {
            conn: 1,
            leaf: 7,
            generation: 0,
            bytes,
        } => assert_eq!(bytes, b"ls\r"),
        other => panic!("expected a write for leaf 7, got {other:?}"),
    }

    // Resize frames carry the geometry of the client's terminal.
    client.send_binary(&ClientFrame::Resize { cols: 120, rows: 40 }.seal(&mut session.tx));
    let input = harness.wait_for_input(|input| matches!(input, RemoteInput::Resize { .. }));
    assert_eq!(
        input,
        RemoteInput::Resize {
            leaf: 7,
            cols: 120,
            rows: 40
        }
    );

    // A completion request reaches the desktop with the token being typed.
    client.send_binary(&ClientFrame::Completion("src/".into()).seal(&mut session.tx));
    let input = harness.wait_for_input(|input| matches!(input, RemoteInput::Completion { .. }));
    match input {
        RemoteInput::Completion { conn: _, leaf, token } => {
            assert_eq!((leaf, token.as_str()), (7, "src/"));
        }
        other => panic!("expected a completion request, got {other:?}"),
    }

    // Tab switching is applied server-side, re-announced, and the newly
    // selected pane's history is replayed so the client is not left staring at
    // the screen of the pane it just left.
    client.send_binary(&ClientFrame::SwitchTab(8).seal(&mut session.tx));
    let (items, active) = client.expect_tabs(&mut session);
    assert_eq!(active, Some(8));
    assert_eq!(items.len(), 2);
    let ServerFrame::Output(snapshot) = client.recv_frame(&mut session) else { panic!("missing pane snapshot"); };
    let mut screen = vt100::Parser::new(std::num::NonZeroU16::new(24).unwrap(), std::num::NonZeroU16::new(80).unwrap(), 0);
    screen.process(&snapshot);
    assert_eq!(screen.screen().contents(), "other pane");
    harness.server.ingest(8, b"now the api pane");
    client.expect_output(&mut session, b"now the api pane");
    harness.server.ingest(7, b"no longer watched");
    client.expect_quiet(Duration::from_millis(200));

    // Switching to a pane that is not shared is ignored.
    client.send_binary(&ClientFrame::SwitchTab(999).seal(&mut session.tx));
    client.expect_quiet(Duration::from_millis(200));
    harness.server.ingest(8, b"still the api pane");
    client.expect_output(&mut session, b"still the api pane");

    // New tab is a request the desktop fulfils.
    client.send_binary(&ClientFrame::NewTab.seal(&mut session.tx));
    harness.wait_for_input(|input| matches!(input, RemoteInput::NewTab { .. }));

    assert_eq!(harness.clients(), 1);
}

#[test]
fn publishing_new_tabs_reaches_the_client() {
    let harness = Harness::start("tabs");
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);

    // The desktop opened a third pane.
    harness.server.set_tabs(vec![
        TabInfo {
            id: 7,
            title: "shell".into(),
            tab: "spritx".into(),
            tab_id: 1,
            activity: Default::default(),
        },
        TabInfo {
            id: 8,
            title: "api".into(),
            tab: "spritx".into(),
            tab_id: 1,
            activity: Default::default(),
        },
        TabInfo {
            id: 9,
            title: "docs".into(),
            tab: "spritx".into(),
            tab_id: 2,
            activity: Default::default(),
        },
    ]);
    let (items, active) = client.expect_tabs(&mut session);
    assert_eq!(items.len(), 3);
    assert_eq!(active, Some(7));
}

#[test]
fn losing_the_shared_pane_falls_back_to_another() {
    let harness = Harness::start("fallback");
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);

    // The client switches to pane 8, then the desktop closes that pane.
    client.send_binary(&ClientFrame::SwitchTab(8).seal(&mut session.tx));
    client.expect_tabs(&mut session);
    client.expect_output(&mut session, b"\x1bc");
    harness.server.set_tabs(vec![TabInfo {
        id: 7,
        title: "shell".into(),
        tab: "spritx".into(),
        tab_id: 1,
        activity: Default::default(),
    }]);
    let (items, active) = client.expect_tabs(&mut session);
    assert_eq!((items.len(), active), (1, Some(7)));
    client.expect_output(&mut session, b"\x1bc");
    harness.server.ingest(7, b"back home");
    client.expect_output(&mut session, b"back home");
}

#[test]
fn pairing_code_is_single_use() {
    let harness = Harness::start("pairing");
    let secret = harness.server.pairing_secret().expect("a pairing is open");
    let device_id = new_device_id();
    let device_key = random::<KEY_LEN>();

    let mut first = WsClient::connect(harness.server.port());
    first.expect_server_hello();
    first.send_text(&pair_request(&secret, &device_id, &device_key, "Phone A"));
    assert!(matches!(first.recv_control(), Control::Paired));

    // The code is spent as soon as its sealed payload opens.
    assert!(
        harness.server.pairing_secret().is_none(),
        "the code must be consumed"
    );
    let mut second = WsClient::connect(harness.server.port());
    second.expect_server_hello();
    second.send_text(&pair_request(&secret, &new_device_id(), &random::<KEY_LEN>(), "Phone B"));
    second.expect_denied("pairing");

    // The registered device can still authenticate.
    first.authenticate(&device_id, &device_key);
    let devices = harness.server.status_now().devices;
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, device_id);
    assert_eq!(devices[0].name, "Phone A");
}

#[test]
fn an_expired_pairing_code_is_refused() {
    let harness = Harness::start("expired");
    // A code with a zero TTL is already expired when it is handed out.
    let secret = harness
        .server
        .open_pairing_with_ttl(Duration::ZERO)
        .expect("an expired code is still returned for the test");
    let mut client = WsClient::connect(harness.server.port());
    client.expect_server_hello();
    client.send_text(&pair_request(&secret, &new_device_id(), &random::<KEY_LEN>(), "Phone"));
    client.expect_denied("pairing");
    assert!(harness.server.status_now().devices.is_empty());
}

#[test]
fn unknown_and_unauthenticated_clients_are_denied() {
    let harness = Harness::start("denied");
    let mut client = WsClient::connect(harness.server.port());
    client.expect_server_hello();
    let ghost = new_device_id();
    client.send_text(&Control::hello_request(&ghost, &random::<NONCE_LEN>()).to_json());
    client.expect_denied("unknown");

    // Garbage is refused rather than trusted.
    let mut other = WsClient::connect(harness.server.port());
    other.expect_server_hello();
    other.send_text("not json at all");
    other.expect_denied("protocol");

    // A wrong `READY` proof is a hard drop, not a denial to argue about.
    let secret = harness.server.pairing_secret().unwrap();
    let device_id = new_device_id();
    let device_key = random::<KEY_LEN>();
    let mut third = WsClient::connect(harness.server.port());
    third.expect_server_hello();
    third.send_text(&pair_request(&secret, &device_id, &device_key, "Phone"));
    assert!(matches!(third.recv_control(), Control::Paired));
    let nc = random::<NONCE_LEN>();
    third.send_text(&Control::hello_request(&device_id, &nc).to_json());
    let Control::Challenge { ns } = third.recv_control() else {
        panic!("expected a challenge")
    };
    let ns = to_array::<NONCE_LEN>(&unb64(&ns).unwrap()).unwrap();
    let (mut tx, _rx) = client_session(&device_key, &nc, &ns);
    third.send_binary(&tx.seal(b"not ready"));
    third.expect_closed();

    harness.wait_for_clients(0);
}

#[test]
fn revoke_cuts_a_live_connection() {
    let store = temp_store("revoke");
    let harness = Harness::with_store(store.clone());
    let (mut client, mut session, device) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);
    assert_eq!(harness.clients(), 1);
    assert_eq!(store.devices().len(), 1);

    assert!(harness.server.revoke(&device.id).expect("revoke succeeds"));
    // Stopping access must also stop the bridge that is already running.
    client.expect_closed();
    harness.wait_for_clients(0);

    let status = harness.server.status_now();
    assert!(status.devices.is_empty(), "the device is forgotten");
    assert_eq!(status.clients, 0);
    assert!(!harness.server.revoke(&device.id).expect("revoke is idempotent"));

    // And it can no longer authenticate.
    let mut again = WsClient::connect(harness.server.port());
    again.expect_server_hello();
    again.send_text(&Control::hello_request(&device.id, &random::<NONCE_LEN>()).to_json());
    again.expect_denied("unknown");
}

#[test]
fn stop_sends_an_immediate_explicit_close_to_every_client() {
    let harness = Harness::start("stop_close_frame");
    let (mut first, mut a, _) = harness.pair_and_connect("one");
    harness.accept_attach(&mut first, &mut a);
    assert!(harness.server.has_clients_for_leaf(7));
    assert!(!harness.server.has_clients_for_leaf(8), "an unviewed pane keeps its desktop geometry");
    harness.server.new_pairing();
    let (mut second, mut b, _) = harness.pair_and_connect("two");
    harness.accept_attach(&mut second, &mut b);
    let start = Instant::now();
    harness.server.stop();
    assert!(start.elapsed() < Duration::from_millis(100), "stop blocked the caller");
    assert!(!harness.server.has_clients_for_leaf(7));
    for client in [&mut first, &mut second] {
        client.socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let head = client.read_exact(2).expect("prompt close frame, not an abrupt EOF");
        assert_eq!(head[0] & 0x0f, 8);
        let payload = client.read_exact((head[1] & 0x7f) as usize).unwrap();
        assert_eq!(&payload[..2], &1001u16.to_be_bytes());
        assert_eq!(&payload[2..], b"Sharing stopped");
    }
    assert!(harness.server.take_inputs().is_empty());
}

#[test]
fn stop_cancels_a_client_waiting_in_the_handshake() {
    let harness = Harness::start("stop_handshake");
    let mut client = WsClient::connect(harness.server.port());
    client.expect_server_hello();
    client.socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    harness.server.stop();
    // An idle unauthenticated client must not wait for the handshake timeout.
    let mut byte = [0];
    match client.socket.read(&mut byte) {
        Ok(0) => {},
        Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {},
        other => panic!("handshake did not close immediately: {other:?}"),
    }
}

#[test]
fn stop_cuts_live_connections_and_clears_pairing() {
    let harness = Harness::start("stop");
    let port = harness.server.port();
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);

    harness.server.stop();
    client.expect_closed();
    let status = harness.server.status_now();
    assert!(!status.running);
    assert_eq!(status.port, 0, "the port is only shown while sharing");
    assert!(status.pair_url.is_none());
    assert!(status.url.is_empty());
    assert_eq!(status.clients, 0);
    assert!(harness.server.pairing_secret().is_none());

    // After a stop the listener is gone; at worst a stale socket may accept the
    // TCP connection but must never complete a WebSocket handshake.
    if let Ok(mut probe) = TcpStream::connect(("127.0.0.1", port)) {
        let _ = probe.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = probe.write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\nSec-WebSocket-Version: 13\r\n\r\n");
        let mut buf = Vec::new();
        let _ = probe.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(!text.contains("101"), "{text}");
    }
}

#[test]
fn status_reports_urls_pairing_and_devices() {
    let harness = Harness::start("status");
    let status = harness.server.status_now();
    assert!(status.running);
    assert!(status.port > 0);
    assert!(!status.bind_all, "this test binds loopback only");
    assert_eq!(status.url, format!("http://127.0.0.1:{}", status.port));
    let pair_url = status.pair_url.clone().expect("a share starts paired-ready");
    assert!(pair_url.starts_with(&status.url), "{pair_url} vs {}", status.url);
    assert!(pair_url.contains("#p="), "the secret travels in the fragment");
    // The fragment is where the QR URL keeps its one-time secret.
    assert_eq!(
        pair_url.split('#').nth(1).unwrap().split('=').nth(1).unwrap().len(),
        43,
        "base64url of 32 bytes"
    );
    assert!(status.public_url.is_none(), "no tunnel was requested");
    assert!(!status.tunnel_requested);
    let ttl = status.pairing_expires_in.expect("a pairing is open");
    assert!((500..=600).contains(&ttl), "pairing TTL is 10 minutes, got {ttl}");
    assert!(!status.server_name.is_empty());

    let (mut client, mut session, device) = harness.pair_and_connect("iPhone");
    harness.accept_attach(&mut client, &mut session);
    let status = harness.server.status_now();
    assert_eq!(status.clients, 1);
    assert_eq!(status.devices.len(), 1);
    assert_eq!(status.devices[0].id, device.id);
    assert_eq!(status.devices[0].name, "iPhone");
    assert_eq!(status.devices[0].connections, 1);
    assert!(status.devices[0].last_seen.is_some(), "hello touches the device");
    // That pairing was consumed by this device.
    assert!(status.pair_url.is_none());

    // A fresh link reopens pairing.
    let url = harness.server.new_pairing().expect("a new pairing url");
    assert!(url.contains("#p="), "{url}");
    let reopened = harness.server.status_now();
    assert_eq!(reopened.pair_url.as_deref(), Some(url.as_str()));
    assert!(reopened.pairing_expires_in.is_some());
    // …without disturbing the live client.
    assert_eq!(reopened.clients, 1);
    harness.server.ingest(7, b"still streaming");
    client.expect_output(&mut session, b"still streaming");
}

#[test]
fn a_forged_or_replayed_frame_kills_the_connection() {
    let harness = Harness::start("forgery");
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);

    // Sealed under a key this connection never negotiated.
    let mut alien = Half::new(random::<KEY_LEN>(), CLIENT_TO_SERVER);
    client.send_binary(&ClientFrame::Input(b"id\r".to_vec()).seal(&mut alien));
    client.expect_closed();
    harness.wait_for_clients(0);

    // A replay of a good frame is equally fatal: the counter has moved on.
    harness.server.new_pairing();
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel 2");
    harness.accept_attach(&mut client, &mut session);
    let frame = ClientFrame::Input(b"echo one\r".to_vec()).seal(&mut session.tx);
    client.send_binary(&frame.clone());
    harness.wait_for_input(|input| matches!(input, RemoteInput::Write { .. }));
    client.send_binary(&frame);
    client.expect_closed();
}

#[test]
fn two_clients_share_one_pane_and_keep_their_own_count() {
    let harness = Harness::start("fanout");
    let secret = harness.server.pairing_secret().unwrap();
    let first_device = Device::new();
    let (first_id, first_key) = (first_device.id.clone(), first_device.key);
    let mut first = WsClient::connect(harness.server.port());
    first.expect_server_hello();
    first.send_text(&pair_request(&secret, &first_id, &first_key, "Phone A"));
    assert!(matches!(first.recv_control(), Control::Paired));
    let mut first_session = first.authenticate(&first_id, &first_key);
    harness.accept_attach(&mut first, &mut first_session);

    // Pairing is single-use, so the second device gets its own code.
    let second_secret = {
        harness.server.new_pairing();
        harness.server.pairing_secret().unwrap()
    };
    let second_id = new_device_id();
    let second_key = random::<KEY_LEN>();
    let mut second = WsClient::connect(harness.server.port());
    second.expect_server_hello();
    second.send_text(&pair_request(&second_secret, &second_id, &second_key, "Phone B"));
    assert!(matches!(second.recv_control(), Control::Paired));
    let mut second_session = second.authenticate(&second_id, &second_key);
    harness.accept_attach(&mut second, &mut second_session);

    // One PTY chunk, both bridges: the fan-out clones per connection.
    harness.server.ingest(7, b"shared bytes");
    first.expect_output(&mut first_session, b"shared bytes");
    second.expect_output(&mut second_session, b"shared bytes");
    assert_eq!(harness.clients(), 2);
    assert_eq!(
        harness.server.status_now().devices[0].connections
            + harness.server.status_now().devices[1].connections,
        2
    );

    // Reving one device leaves the other streaming.
    assert!(harness.server.revoke(&first_id).unwrap());
    first.expect_closed();
    harness.server.ingest(7, b"only B now");
    second.expect_output(&mut second_session, b"only B now");
    let status = harness.server.status_now();
    assert_eq!(status.devices.len(), 1);
    assert_eq!(status.devices[0].id, second_id);
}

#[test]
fn a_reconnecting_device_needs_no_new_pairing() {
    let harness = Harness::start("reconnect");
    let device = {
        let (mut client, mut session, device) = harness.pair_and_connect("Pixel");
        harness.accept_attach(&mut client, &mut session);
        client.send_close();
        device
    };
    let (mut client, mut session) = harness.connect_paired(&device.id, &device.key);
    harness.accept_attach(&mut client, &mut session);
    harness.server.ingest(7, b"live");
    client.expect_output(&mut session, b"live");
}

#[test]
fn listing_helper_matches_the_original_completion_shape() {
    let dir = std::env::temp_dir().join(format!("spritx_remote_ls_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
    std::fs::write(dir.join("setup.sh"), b"x").expect("write setup.sh");
    std::fs::write(dir.join("readme.md"), b"x").expect("write readme.md");
    std::fs::write(dir.join(".hidden"), b"x").expect("write .hidden");

    let json = listing_json(dir.to_str(), "s");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["t"], "ls");
    assert!(json.contains("[\"src\",true]"), "{json}");
    assert!(json.contains("setup.sh"), "{json}");
    assert!(!json.contains("readme.md"), "{json}");
    assert!(!json.contains(".hidden"), "{json}");
    assert!(
        listing_json(None, "anything").contains("\"items\":[]"),
        "no cwd means no suggestions"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A freshly attached client must receive the pane's raw byte history, escape
/// sequences and all. Rebuilding it from a plain-text dump of the cell grid made
/// the first paint lose every colour, so a phone connecting to a prompt showing
/// a rainbow powerline got a monochrome one until the next command ran.
#[test]
fn attaching_replays_styled_bytes_not_plain_text() {
    let harness = Harness::start("replay");
    // A late client must retain current cells and styles even after more
    // than a replay-tail's worth of output, including a split UTF-8 character.
    harness.server.ingest(7, &vec![b'x'; 600_000]);
    harness.server.ingest(7, b"\x1b[?1049h\x1b[H\x1b[2J\x1b[31mRED\x1b[0m \x1b[1mBOLD\x1b[0m ");
    harness.server.ingest(7, &[0xce]);
    harness.server.ingest(7, &[0xbb]);
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);
    let ServerFrame::Output(bytes) = client.recv_frame(&mut session) else { panic!("missing snapshot"); };
    let mut parser = vt100::Parser::new(std::num::NonZeroU16::new(24).unwrap(), std::num::NonZeroU16::new(80).unwrap(), 0);
    parser.process(&bytes);
    assert!(parser.screen().alternate_screen());
    assert_eq!(parser.screen().contents(), "RED BOLD λ");
    assert_eq!(parser.screen().cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
    assert!(parser.screen().cell(0, 4).unwrap().bold());
}

/// A client that attaches to a pane which has produced nothing yet gets the tab
/// list and then silence, rather than an empty frame or a hang.
#[test]
fn attaching_to_a_quiet_pane_sends_no_replay_frame() {
    let harness = Harness::start("replay-empty");
    let (mut client, mut session, _) = harness.pair_and_connect("Pixel");
    harness.accept_attach(&mut client, &mut session);
    client.expect_quiet(Duration::from_millis(200));
}

// ---------------------------------------------------------------------------
// Containment and connection-cap regression tests
// ---------------------------------------------------------------------------

fn unique_temp_root(tag: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "spritx_remote_containment_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create containment test root");
    root
}

#[test]
fn listing_tokens_cannot_escape_the_pane_cwd() {
    let root = unique_temp_root("cwd");
    let cwd = root.join("inside");
    let outside = root.join("outside");
    std::fs::create_dir_all(&cwd).expect("create in-cwd directory");
    std::fs::create_dir_all(&cwd.join("child")).expect("create in-cwd child directory");
    std::fs::create_dir_all(&outside).expect("create outside directory");
    std::fs::write(cwd.join("child").join("inside_marker.txt"), b"inside").expect("write in-cwd marker");
    std::fs::write(cwd.join("cwd_marker.txt"), b"inside").expect("write cwd marker");
    std::fs::write(outside.join("outside_secret.txt"), b"outside").expect("write outside marker");

    // Pre-fix the token was used directly as the read_dir path, so both the
    // absolute outside directory and a ../ token leaked outside_secret.txt.
    let absolute_outside = format!("{}/", outside.to_string_lossy().replace('\\', "/"));
    assert!(
        listing_names(&cwd, &absolute_outside).is_empty(),
        "an absolute token outside cwd must yield no entries"
    );
    assert!(
        listing_names(&cwd, "../outside/").is_empty(),
        "a ../ token escaping cwd must yield no entries"
    );

    assert_eq!(listing_names(&cwd, ""), vec!["child", "cwd_marker.txt"]);
    assert_eq!(listing_names(&cwd, "child/"), vec!["inside_marker.txt"]);
    let absolute_inside = format!("{}/child/", cwd.to_string_lossy().replace('\\', "/"));
    assert_eq!(listing_names(&cwd, &absolute_inside), vec!["inside_marker.txt"]);

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn listing_symlink_escape_is_rejected() {
    let root = unique_temp_root("symlink");
    let cwd = root.join("inside");
    let outside = root.join("outside");
    std::fs::create_dir_all(&cwd).expect("create in-cwd directory");
    std::fs::create_dir_all(&outside).expect("create outside directory");
    std::fs::write(outside.join("symlink_secret.txt"), b"outside").expect("write outside marker");
    std::os::unix::fs::symlink(&outside, cwd.join("escape")).expect("create escape symlink");

    assert!(
        listing_names(&cwd, "escape/").is_empty(),
        "canonicalization must resolve the symlink and reject its outside target"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ws_connection_cap_rejects_excess_and_refills_after_close() {
    let harness = Harness::start("connection-cap");
    let port = harness.server.port();
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        held.push(WsClient::connect(port));
    }

    let overflow = raw_ws_upgrade_head(port);
    assert!(
        overflow.lines().next().unwrap_or_default().to_ascii_lowercase().starts_with("http/1.1 503"),
        "the overflow upgrade must be refused with 503, got {overflow:?}"
    );

    drop(held.pop());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut refilled = None;
    while Instant::now() < deadline {
        let head = raw_ws_upgrade_head(port);
        if head.lines().next().unwrap_or_default().to_ascii_lowercase().starts_with("http/1.1 101") {
            refilled = Some(head);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(refilled.is_some(), "closing one connection must free a slot for a later upgrade");
}
