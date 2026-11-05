//! End-to-end tests for the SpritX↔SpritX client: a real [`RemoteServer`] on
//! loopback, driven by the real [`remote_client`] against a real pairing code.
//!
//! These exist because the port shipped for a while advertising "another SpritX"
//! with no client behind it at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use spritx_terminal::backend::remote::{DeviceStore, TabInfo};
use spritx_terminal::backend::remote_client::{self, RemoteCommand, RemoteLink};
use spritx_terminal::backend::remote_server::{Bind, RemoteInput, RemoteServer, ServerOptions};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The peer store root is process-global, so these tests take turns.
fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Isolate the peer store for one test and hold the lock until it ends.
fn isolated_peers(tag: &str) -> MutexGuard<'static, ()> {
    let guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
    remote_client::set_peers_root(temp_root(tag));
    guard
}

fn temp_root(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "spritx_terminal_client_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the temp root");
    root
}

/// A server sharing one pane, leaf 7.
fn server(tag: &str) -> RemoteServer {
    let server = RemoteServer::start(ServerOptions {
        port: 0,
        bind: Bind::Loopback,
        store: Some(DeviceStore::new(temp_root(tag))),
        tunnel: false,
        ..ServerOptions::default()
    })
    .expect("start the remote server");
    server.set_tabs(vec![TabInfo {
        id: 7,
        title: "shell".into(),
        tab: "peer".into(),
        tab_id: 1,
        activity: Default::default(),
    }]);
    server.set_active_leaf(7);
    server
}

/// Poll until `check` passes, so the tests never depend on a fixed sleep.
fn within(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}

fn wait_online(link: &RemoteLink) {
    assert!(
        within(|| link.is_online()),
        "the link never came up: {:?}",
        link.error()
    );
}

/// Drain everything the link has received since the last call.
fn read_outcome(link: &RemoteLink) -> Vec<u8> {
    let (pending, dirty) = link.output_buffers();
    let mut guard = pending.lock().expect("pending lock");
    if guard.is_empty() {
        return Vec::new();
    }
    let bytes = std::mem::take(&mut *guard);
    dirty.store(false, Ordering::Relaxed);
    bytes
}

/// Accumulate link output until `needle` shows up.
fn awaits(link: &RemoteLink, needle: &[u8]) -> bool {
    let mut seen = Vec::new();
    let found = AtomicBool::new(false);
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline && !found.load(Ordering::Relaxed) {
        seen.extend(read_outcome(link));
        if seen.len() >= needle.len() && seen.windows(needle.len()).any(|w| w == needle) {
            found.store(true, Ordering::Relaxed);
        } else {
            sleep(Duration::from_millis(20));
        }
    }
    found.load(Ordering::Relaxed)
}

#[test]
fn pairs_authenticates_and_reports_the_peer() {
    let _isolated = isolated_peers("pair");
    let server = server("pair");
    let url = server.pair_url().expect("a pairing code is open");
    let link = remote_client::connect(&url).expect("accepts the pairing link");

    wait_online(&link);

    // The secret must never appear in a label the UI can render.
    let shown = remote_client::display_target(&url);
    assert!(!shown.contains("#p="), "display leaks the pairing secret: {shown}");
    assert!(shown.contains(&server.port().to_string()), "keeps the port");

    assert_eq!(remote_client::peers().len(), 1, "the peer is remembered");
    link.close();
    server.stop();
}

#[test]
fn output_flows_from_the_peer_into_the_pane() {
    let _isolated = isolated_peers("out");
    let server = server("out");
    let url = server.pair_url().expect("pairing");
    let link = remote_client::connect(&url).expect("connect");
    wait_online(&link);

    server.ingest(7, b"hello from the other SpritX");
    assert!(
        awaits(&link, b"other SpritX"),
        "peer output never reached the pane buffer"
    );
    link.close();
    server.stop();
}

#[test]
fn keystrokes_flow_to_the_peer() {
    let _isolated = isolated_peers("in");
    let server = server("in");
    let url = server.pair_url().expect("pairing");
    let link = remote_client::connect(&url).expect("connect");
    wait_online(&link);

    link.send(RemoteCommand::Input(b"ls -la\n".to_vec()))
        .expect("the link accepts input");
    let delivered = within(|| {
        server.take_inputs().into_iter().any(|input| {
            matches!(input, RemoteInput::Write { leaf: 7, ref bytes } if bytes == b"ls -la\n")
        })
    });
    assert!(delivered, "keystrokes never arrived at the peer's pane");
    link.close();
    server.stop();
}

#[test]
fn a_reconnect_needs_no_new_pairing_code() {
    let _isolated = isolated_peers("reconnect");
    let server = server("reconnect");
    let url = server.pair_url().expect("pairing");
    let link = remote_client::connect(&url).expect("connect");
    wait_online(&link);

    // The code is single-use and already spent, so a bare base URL must still
    // work: the peer is remembered by server id.
    let base = format!("http://127.0.0.1:{}/", server.port());
    let second = remote_client::connect(&base).expect("accepts a bare address");
    wait_online(&second);

    link.close();
    second.close();
    server.stop();
}

#[test]
fn a_bad_address_fails_with_a_real_error() {
    let _isolated = isolated_peers("bad");
    let error = match remote_client::connect("not a link at all") {
        Ok(_) => panic!("a meaningless string must not parse as an address"),
        Err(error) => error,
    };
    assert!(
        !error.to_string().trim().is_empty(),
        "a bad address must explain itself"
    );
    assert!(remote_client::display_target("not a link at all").is_empty());
}

#[test]
fn forgetting_a_peer_stops_silent_reauthentication() {
    let _isolated = isolated_peers("forget");
    let server = server("forget");
    let url = server.pair_url().expect("pairing");
    let link = remote_client::connect(&url).expect("connect");
    wait_online(&link);
    link.close();

    let remembered = remote_client::peers().pop().expect("one peer stored");
    assert!(
        remote_client::forget_peer(&remembered.server_id).expect("forgetting works"),
        "the peer was not remembered"
    );
    assert!(remote_client::peers().is_empty(), "the store is empty now");
    assert!(
        !remote_client::forget_peer(&remembered.server_id).expect("second forget is fine"),
        "forgetting twice must not claim a second removal"
    );

    let base = format!("http://127.0.0.1:{}/", server.port());
    let orphan = remote_client::connect(&base).expect("the address still parses");
    let refused = within(|| {
        orphan
            .error()
            .is_some_and(|note| note.to_lowercase().contains("not paired"))
    });
    assert!(
        refused,
        "an unpaired reconnect must say it needs pairing: {:?}",
        orphan.error()
    );
    assert!(!orphan.is_online(), "it must not silently connect");
    orphan.close();
    server.stop();
}

/// A pane opening another SpritX's terminal must show it styled from the first
/// paint, not a colourless dump that only looks right after the next command.
#[test]
fn a_new_pane_is_rebuilt_with_the_peer_styling() {
    let _isolated = isolated_peers("styled");
    let server = server("styled");
    server.ingest(7, b"\x1b[32mgreen\x1b[0m prompt$ ");

    let url = server.pair_url().expect("pairing");
    let link = remote_client::connect(&url).expect("connect");
    wait_online(&link);

    assert!(
        awaits(&link, b"\x1b[32mgreen\x1b[0m"),
        "the peer's colours must survive into the first paint, got {:?}",
        String::from_utf8_lossy(&read_outcome(&link))
    );
    link.close();
    server.stop();
}
