//! SpritX ↔ SpritX: open a terminal of *another* SpritX in a local pane, over that
//! SpritX's remote-control channel. Same end-to-end encrypted protocol the phone
//! page uses — see [`crate::backend::remote`].
//!
//! One connection is opened per local pane. Pairing happens once (the pairing
//! link carries the one-time secret); afterwards the peer is remembered by its
//! server id, so a new address — a new tunnel URL, a new LAN IP — reconnects
//! without re-pairing. A dropped link (Wi-Fi change, sleep, peer restart) is
//! retried with backoff in place, and the server replays the screen on reconnect.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::backend::remote::{
    b64, client_session, host_name, new_device_id, pair_request, random, to_array, unb64, unb64url,
    ClientFrame, Control, Half, ServerFrame, KEY_LEN, NONCE_LEN, READY,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Give up on the initial TCP + handshake this far in, so a dead address surfaces
/// as an error instead of a pane that never paints.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PING_EVERY: Duration = Duration::from_secs(15);
/// No frame at all for this long (pongs included) means the link is dead.
const DEAD_AFTER: Duration = Duration::from_secs(45);
const START_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Desktop wake signal. The connection task runs on its own runtime, so without
/// this inbound output would sit until the desktop's next 500 ms poll.
static CLIENT_WAKE: OnceLock<Arc<Notify>> = OnceLock::new();

/// Register the desktop's wake signal (idempotent).
pub fn set_client_wake(signal: Arc<Notify>) {
    let _ = CLIENT_WAKE.set(signal);
}

fn signal_client_wake() {
    if let Some(signal) = CLIENT_WAKE.get() {
        signal.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Remembered peers
// ---------------------------------------------------------------------------

/// A SpritX we have paired with. Keyed by the *remote* server id, so the address
/// can change underneath us without losing the pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub server_id: String,
    pub server_name: String,
    pub device_id: String,
    /// Base64 device key, minted at pairing.
    pub key: String,
    /// Last known http base, without the pairing fragment.
    pub url: String,
}

#[derive(Default, Serialize, Deserialize)]
struct PeersFile {
    #[serde(default)]
    peers: BTreeMap<String, Peer>,
}

/// Where `peers.json` lives. Overridable so tests never touch a real config dir.
static PEERS_ROOT_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Point the peer store at `dir` instead of the user's config directory.
pub fn set_peers_root(dir: impl Into<PathBuf>) {
    *PEERS_ROOT_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
}

/// Where `peers.json` lives. Overridable so tests never touch a real config dir.
fn peers_root() -> PathBuf {
    if let Some(override_root) = PEERS_ROOT_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        return override_root.clone();
    }
    crate::backend::config::config_path()
        .and_then(|p| p.parent().map(|d| d.join("remote")))
        .unwrap_or_default()
}

fn peers_path() -> PathBuf {
    peers_root().join("peers.json")
}

fn read_peers() -> PeersFile {
    std::fs::read_to_string(peers_path())
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn write_peers(file: &PeersFile) -> Result<()> {
    let path = peers_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create the remote peers directory")?;
    }
    crate::backend::config::write_atomic(&path, &serde_json::to_string_pretty(file)?)
        .context("write the remote peers file")?;
    Ok(())
}

fn peers_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Every SpritX this machine has paired with, newest name first.
pub fn peers() -> Vec<Peer> {
    let _guard = peers_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<Peer> = read_peers().peers.into_values().collect();
    list.sort_by(|a, b| a.server_name.cmp(&b.server_name));
    list
}

pub fn peer_for(server_id: &str) -> Option<Peer> {
    let _guard = peers_lock().lock().unwrap_or_else(|e| e.into_inner());
    read_peers().peers.get(server_id).cloned()
}

fn save_peer(peer: Peer) -> Result<()> {
    let _guard = peers_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut file = read_peers();
    file.peers.insert(peer.server_id.clone(), peer);
    write_peers(&file)
}

/// Forget a SpritX. Existing connections keep running until their pane closes;
/// this only stops the next one from authenticating silently.
pub fn forget_peer(server_id: &str) -> Result<bool> {
    let _guard = peers_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut file = read_peers();
    let existed = file.peers.remove(server_id).is_some();
    if existed {
        write_peers(&file)?;
    }
    Ok(existed)
}

fn device_name() -> String {
    format!("SpritX — {}", host_name())
}

// ---------------------------------------------------------------------------
// Address parsing
// ---------------------------------------------------------------------------

/// Where to dial, plus the one-time secret if the input carried one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    ws_url: String,
    /// The http(s) base without the pairing fragment — what gets persisted.
    http_url: String,
    pair: Option<[u8; KEY_LEN]>,
}

/// Accepts what the pairing panel shows: `http://192.168.1.10:4530/#p=…`,
/// `https://x.trycloudflare.com/`, or a bare `host:port`.
fn parse_target(input: &str) -> Result<Target> {
    let input = input.trim();
    if input.is_empty() {
        return Err(anyhow!("paste the pairing link shown by the other SpritX"));
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("http://{input}")
    };
    let mut url = url::Url::parse(&with_scheme)
        .map_err(|_| anyhow!("that is not a valid address or pairing link"))?;
    let pair = url
        .fragment()
        .into_iter()
        .flat_map(|fragment| fragment.split('&'))
        .find_map(|pair| pair.strip_prefix("p="))
        .and_then(unb64url)
        .and_then(|bytes| to_array::<KEY_LEN>(&bytes));
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(anyhow!("unsupported scheme '{other}'")),
    };
    if url.host_str().is_none() {
        return Err(anyhow!("the link has no host"));
    }
    url.set_fragment(None);
    url.set_query(None);
    url.set_path("/");
    let mut http = url.clone();
    let _ = http.set_scheme(if scheme == "wss" { "https" } else { "http" });
    let mut ws = url;
    let _ = ws.set_scheme(scheme);
    ws.set_path("/ws");
    Ok(Target {
        ws_url: ws.to_string(),
        http_url: http.to_string(),
        pair,
    })
}

/// A one-line label for an address: host and port only. The pairing secret lives
/// in the URL fragment and must never reach a title, toast or log.
pub fn display_target(input: &str) -> String {
    let Ok(target) = parse_target(input) else {
        return String::new();
    };
    let rest = target
        .http_url
        .strip_prefix("https://")
        .or_else(|| target.http_url.strip_prefix("http://"))
        .unwrap_or(target.http_url.as_str());
    rest.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// Commands in flight
// ---------------------------------------------------------------------------

/// What the local pane asks the link to carry.
///
/// Deliberately narrow: this drives whichever pane the peer considers active.
/// Picking a specific peer pane is not exposed yet, so nothing here pretends to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// Stop reconnecting and tear the link down.
    Close,
}

impl RemoteCommand {
    /// The wire frame for a command, or `None` for the shutdown signal.
    fn into_frame(self) -> Option<ClientFrame> {
        match self {
            RemoteCommand::Input(bytes) => Some(ClientFrame::Input(bytes)),
            RemoteCommand::Resize { cols, rows } => Some(ClientFrame::Resize { cols, rows }),
            RemoteCommand::Close => None,
        }
    }
}

/// Shared state between the UI thread and the connection task.
struct LinkState {
    /// `None` until the peer identifies itself in its hello frame.
    server: Mutex<Option<(String, String)>>,
    /// A live, authenticated socket.
    online: AtomicBool,
    /// The user closed the pane; stop reconnecting.
    closing: AtomicBool,
    /// Why the link is unusable right now. `None` while online.
    error: Mutex<Option<String>>,
    /// Output lands here; the desktop pump parses it exactly like PTY bytes.
    pending: Arc<Mutex<Vec<u8>>>,
    dirty: Arc<AtomicBool>,
}

impl LinkState {
    fn new() -> Self {
        Self {
            server: Mutex::new(None),
            online: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            error: Mutex::new(Some("connecting…".to_string())),
            pending: Arc::new(Mutex::new(Vec::new())),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    fn push_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.extend_from_slice(bytes);
        }
        self.dirty.store(true, Ordering::Relaxed);
        signal_client_wake();
    }

    fn set_error(&self, message: &str) {
        self.online.store(false, Ordering::Relaxed);
        if let Ok(mut error) = self.error.lock() {
            *error = Some(message.to_string());
        }
        signal_client_wake();
    }

    fn set_online(&self, server_id: String, server_name: String) {
        if let Ok(mut server) = self.server.lock() {
            *server = Some((server_id, server_name));
        }
        if let Ok(mut error) = self.error.lock() {
            *error = None;
        }
        self.online.store(true, Ordering::Relaxed);
    }
}

/// A live SpritX↔SpritX link backing one local pane. Dropping the last handle asks
/// the task to stop.
pub struct RemoteLink {
    commands: mpsc::UnboundedSender<RemoteCommand>,
    state: Arc<LinkState>,
}

impl RemoteLink {
    /// The peer's name, or `None` until its hello frame arrives.
    pub fn server_name(&self) -> Option<String> {
        self.server().map(|(_, name)| name)
    }

    pub fn server_id(&self) -> Option<String> {
        self.server().map(|(id, _)| id)
    }

    fn server(&self) -> Option<(String, String)> {
        self.state
            .server
            .lock()
            .ok()
            .and_then(|server| server.clone())
    }

    pub fn is_online(&self) -> bool {
        self.state.online.load(Ordering::Relaxed)
    }

    /// Why the link is currently down, if it is. `Some("connecting…")` while the
    /// first handshake is still in flight.
    pub fn error(&self) -> Option<String> {
        self.state
            .error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub fn send(&self, command: RemoteCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("the remote link has closed"))
    }

    pub fn close(&self) {
        self.state.closing.store(true, Ordering::Relaxed);
        let _ = self.commands.send(RemoteCommand::Close);
    }

    /// The buffers a [`crate::terminal::TerminalSession`] needs to render this
    /// link as a terminal pane.
    pub fn output_buffers(&self) -> (Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
        (self.state.pending.clone(), self.state.dirty.clone())
    }
}

impl Drop for RemoteLink {
    fn drop(&mut self) {
        self.state.closing.store(true, Ordering::Relaxed);
        let _ = self.commands.send(RemoteCommand::Close);
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

struct Session {
    ws: Ws,
    tx: Half,
    rx: Half,
    server_id: String,
    server_name: String,
}

/// Errors are plain text; the UI shows them verbatim in a toast.
const ERR_PROTOCOL: &str = "the peer spoke an unexpected protocol";

async fn next_json(ws: &mut Ws) -> Option<serde_json::Value> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(text)) => return serde_json::from_str(text.as_str()).ok(),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}

fn denial(v: &serde_json::Value) -> Option<&str> {
    (v.get("t")?.as_str()? == "denied")
        .then(|| v.get("reason").and_then(|r| r.as_str()).unwrap_or(""))
}

/// Connect, pair when the target carries a secret, then authenticate and signal
/// readiness.
async fn open_session(target: &Target, name: &str) -> Result<Session, String> {
    let connect = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(target.ws_url.as_str()))
        .await
        .map_err(|_| "the peer did not answer in time".to_string())?
        .map_err(|error| format!("could not reach the peer: {error}"))?;
    let (mut ws, _) = connect;

    let hello = next_json(&mut ws).await.ok_or_else(|| ERR_PROTOCOL.to_string())?;
    let server_id = hello
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ERR_PROTOCOL.to_string())?
        .to_string();
    let server_name = hello
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("SpritX")
        .to_string();

    let peer = match (target.pair, peer_for(&server_id)) {
        (Some(secret), _) => {
            let device_id = new_device_id();
            let key = random::<KEY_LEN>();
            ws.send(Message::Text(pair_request(&secret, &device_id, &key, name).into()))
                .await
                .map_err(|error| format!("could not send the pairing request: {error}"))?;
            let reply = next_json(&mut ws).await.ok_or_else(|| ERR_PROTOCOL.to_string())?;
            if denial(&reply).is_some() {
                return Err(
                    "pairing was refused — the code is single-use and may already be spent"
                        .to_string(),
                );
            }
            if reply.get("t").and_then(|t| t.as_str()) != Some("paired") {
                return Err(ERR_PROTOCOL.into());
            }
            let peer = Peer {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                device_id,
                key: b64(&key),
                url: target.http_url.clone(),
            };
            if let Err(error) = save_peer(peer.clone()) {
                tracing_store_failure(&error);
            }
            peer
        }
        (None, Some(mut known)) => {
            // A tunnel URL or LAN IP changes routinely; keep the newest one.
            if known.url != target.http_url || known.server_name != server_name {
                known.url = target.http_url.clone();
                known.server_name = server_name.clone();
                if let Err(error) = save_peer(known.clone()) {
                    tracing_store_failure(&error);
                }
            }
            known
        }
        (None, None) => {
            return Err(
                "this SpritX is not paired yet — open the pairing link that carries the code"
                    .to_string(),
            )
        }
    };

    let key = unb64(&peer.key)
        .and_then(|key| to_array::<KEY_LEN>(&key))
        .ok_or_else(|| "the stored key for this peer is unreadable".to_string())?;
    let nc = random::<NONCE_LEN>();
    ws.send(Message::Text(Control::hello_request(&peer.device_id, &nc).to_json().into()))
        .await
        .map_err(|error| format!("could not start authentication: {error}"))?;
    let reply = next_json(&mut ws).await.ok_or_else(|| ERR_PROTOCOL.to_string())?;
    if let Some(reason) = denial(&reply) {
        if reason == "unknown" {
            // The peer forgot us; silently re-pairing would hide a real change.
            if let Err(error) = forget_peer(&server_id) {
                tracing_store_failure(&error);
            }
            return Err("this SpritX was revoked on the peer — pair again".to_string());
        }
        return Err(format!("the peer refused this device ({reason})"));
    }
    let ns = reply
        .get("ns")
        .and_then(|v| v.as_str())
        .and_then(unb64)
        .filter(|value| value.len() == NONCE_LEN)
        .ok_or_else(|| ERR_PROTOCOL.to_string())?;
    let ns = to_array::<NONCE_LEN>(&ns).ok_or_else(|| ERR_PROTOCOL.to_string())?;

    let (mut tx, rx) = client_session(&key, &nc, &ns);
    ws.send(Message::Binary(tx.seal(&READY).into()))
        .await
        .map_err(|error| format!("could not signal readiness: {error}"))?;

    Ok(Session {
        ws,
        tx,
        rx,
        server_id,
        server_name,
    })
}

fn tracing_store_failure(error: &anyhow::Error) {
    // Losing the peer file must not break a live session, but it must not be
    // silent either: the next reconnect will look like an unknown device.
    eprintln!("spritx: could not update the remote peer store: {error:#}");
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

async fn pump(session: &mut Session, state: &Arc<LinkState>, commands: &mut mpsc::UnboundedReceiver<RemoteCommand>) {
    let mut keeper = tokio::time::interval(PING_EVERY);
    // The interval fires immediately the first time; that is harmless.
    keeper.tick().await;
    let mut last_inbound = Instant::now();

    loop {
        if state.closing.load(Ordering::Relaxed) {
            return;
        }
        tokio::select! {
            inbound = session.ws.next() => match inbound {
                Some(Ok(Message::Binary(boxed))) => {
                    last_inbound = Instant::now();
                    match ServerFrame::open(&mut session.rx, &boxed) {
                        Some(ServerFrame::Output(bytes)) => state.push_output(&bytes),
                        Some(ServerFrame::Control(control)) => {
                            // In-channel JSON the peer sends that this client
                            // has no use for. Everything else is a surprise
                            // worth stopping for.
                            if !matches!(control, Control::Tabs { .. } | Control::Ls { .. }) {
                                state.set_error(&format!(
                                    "unexpected frame from the peer: {control:?}"
                                ));
                                return;
                            }
                        }
                        // A frame we cannot open is either replay noise after a
                        // reconnect or a key mismatch; both mean re-dial.
                        None => return,
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    last_inbound = Instant::now();
                    if session.ws.send(Message::Pong(payload)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Pong(_))) => last_inbound = Instant::now(),
                Some(Ok(Message::Close(frame))) => {
                    if frame.as_ref().is_some_and(|f| f.reason == "Sharing stopped") {
                        state.closing.store(true, Ordering::Relaxed);
                    }
                    state.set_error("the remote connection closed");
                    return;
                }
                Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            command = commands.recv() => {
                let command = match command {
                    Some(command) => command,
                    // The pane let go of the link.
                    None => return,
                };
                if matches!(command, RemoteCommand::Close) {
                    return;
                }
                let sealed = command.into_frame().expect("not Close").seal(&mut session.tx);
                if session.ws.send(Message::Binary(sealed.into())).await.is_err() {
                    return;
                }
            }
            _ = keeper.tick() => {
                if last_inbound.elapsed() > DEAD_AFTER {
                    state.set_error("the peer stopped answering");
                    return;
                }
                if session.ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Keep one link up: dial, carry frames until it breaks, then retry with
/// exponential backoff until the pane lets go. Failures are recorded in `state`
/// rather than returned, because the caller is a UI thread that cannot wait.
async fn run(
    target: Target,
    name: String,
    state: Arc<LinkState>,
    inbox: &mut mpsc::UnboundedReceiver<RemoteCommand>,
) {
    let mut backoff = START_BACKOFF;
    loop {
        if state.closing.load(Ordering::Relaxed) {
            break;
        }
        match open_session(&target, &name).await {
            Ok(mut session) => {
                backoff = START_BACKOFF;
                state.set_online(session.server_id.clone(), session.server_name.clone());
                pump(&mut session, &state, inbox).await;
                if !state.closing.load(Ordering::Relaxed) {
                    state.set_error("the link dropped; reconnecting…");
                }
            }
            Err(error) => {
                state.set_error(&format!("{error} — retrying"));
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        tokio::time::sleep(backoff).await;
    }
    state.set_error("closed");
}

/// Open a link to another SpritX from a pairing link, an `http(s)://` base, or a
/// bare `host:port`. Returns as soon as the address parses; the handshake and
/// every later reconnect happen on the link's own thread.
pub fn connect(input: &str) -> Result<RemoteLink> {
    let target = parse_target(input)?;
    let name = device_name();
    let (commands, mut inbox) = mpsc::unbounded_channel::<RemoteCommand>();
    let state = Arc::new(LinkState::new());
    let task_state = state.clone();

    std::thread::Builder::new()
        .name("spritx-remote-client".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            else {
                task_state.set_error("could not start the network runtime");
                return;
            };
            runtime.block_on(run(target, name, task_state, &mut inbox));
        })
        .context("spawn the remote client thread")?;

    Ok(RemoteLink { commands, state })
}
