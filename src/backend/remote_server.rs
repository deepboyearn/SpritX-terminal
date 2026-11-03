//! HTTP + WebSocket server for remote control (pairs with `backend::remote`).
//!
//! This is the GPUI port of the Tauri app's `src-tauri/src/remote.rs`: an opt-in
//! axum server that serves the mobile client page, upgrades `/ws`, runs the
//! pairing / challenge handshake from [`crate::backend::remote`] and bridges one
//! terminal pane per connection bidirectionally. Everything after the handshake
//! is a NaCl secretbox frame, so the HTTP layer, a LAN sniffer and the
//! cloudflared tunnel provider only ever see ciphertext.
//!
//! # Threading model
//!
//! GPUI owns the PTY sessions, so the server never touches a
//! [`crate::terminal::TerminalSession`] directly. `RemoteServer` is a cheap
//! handle onto an [`std::sync::Arc`]'d [`ServerState`]; a tokio runtime runs on
//! one background thread and exchanges small values with the GPUI thread through
//! mutexes and channels that live in that shared state:
//!
//! * **output** — `SpritXApp::pump_terminals` already drains each session's PTY
//!   mailbox ([`pump_bytes`](crate::terminal::TerminalSession::pump_bytes)), so the GPUI thread hands the very
//!   same bytes to [`RemoteServer::ingest`], which fans them out (clone per
//!   connection) to the bridges watching that leaf and appends them to a
//!   bounded replay tail. Both consumers see every byte exactly once; the UI's
//!   `vt100` parser is untouched. A client that attaches, or switches pane, is
//!   rebuilt from that raw tail — so its first paint keeps the colours,
//!   attributes and alternate-screen state the desktop is showing. That tail is
//!   *bounded*, so it is deliberately not the only source of truth: the modes a
//!   child sets once (mouse reporting above all) are published per pane
//!   ([`RemoteServer::set_modes`]) and put back in front of the tail, because a
//!   page that reset its terminal cannot find a long-running fullscreen TUI's
//!   `?1000h` in it and would otherwise leave every tap and swipe dead.
//! * **input / resize / tab switch** — pushed onto a queue the GPUI thread
//!   drains every pump tick ([`RemoteServer::take_inputs`]).
//! * **completion** — replies are sealed by the bridge's own writer task, which
//!   is the only holder of the server→client [`Half`],
//!   so per-direction frame counters can never interleave.
//!
//! # Binding
//!
//! Sharing is meaningless without a reachable address, so the default bind is
//! `0.0.0.0` like the original server: phones on the LAN must be able to connect.
//! Exposure is made safe by the protocol rather than by the socket — pairing is
//! single-use with a 10-minute secret carried only in the QR URL *fragment*
//! (never sent to the server), every post-handshake frame is authenticated, and
//! devices can be revoked (which cuts their live connections). Set
//! [`Bind::Loopback`] to restrict the listener to `127.0.0.1`, which is also
//! the right choice when only a cloudflared tunnel is used.
//!
//! # Client page
//!
//! The static page is public (it holds no secret) and documents the protocol,
//! but it does **not** implement XSalsa20-Poly1305: shipping a half-tested JS
//! implementation of the transport would be worse than not shipping one. The
//! page therefore performs the JSON handshake (it prints the `server` frame) and
//! then refuses to continue: it never accepts or sends plaintext terminal data.
//! Fail-closed — an encrypted SpritX↔SpritX client is the supported remote driver.
//!
//! # Not in this pass
//!
//! * No JS implementation of the cipher in the page (see above), and no QR
//!   *rendering*: `qrcode` is not a dependency of this crate, so the dialog
//!   shows a code preview plus the real pairing link, which is copyable.
//! * `cloudflared` is used if present and never downloaded.
//! * Input and resize are applied by the UI pump, so a keystroke crosses at
//!   most one pump tick (30 ms).
//! * The Tauri server's SpritX↔SpritX *outbound* client (`remote_client.rs`) has
//!   its own module and is not part of this server.

use std::collections::HashMap;
use std::io::{self, BufRead, Read};
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State as AxumState;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};

use crate::backend::remote::{
    ClientFrame, Control, Device, DeviceStore, Established, Half, ServerFrame, ServerHandshake,
    ServerStep, TabInfo, KEY_LEN,
};
use crate::terminal::SessionModes;

/// Default port when the app has not configured one.
pub const DEFAULT_PORT: u16 = 4530;
/// A client must finish the handshake within this delay.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How much recent output is replayed to a freshly attached client.
pub const DEFAULT_REPLAY_BYTES: usize = 64 * 1024;
/// Frames queued for one client before its bridge is cut: a client that cannot
/// keep up must not be able to grow the queue without bound.
pub const OUTBOX_CAPACITY: usize = 256;
/// `u64::MAX` — "no pane shared yet".
const NO_LEAF: u64 = u64::MAX;

/// Desktop wake signal. The transport tasks run on the server's own runtime, so
/// without this a remote keystroke would sit in the input queue until the
/// desktop's next 500 ms poll; notifying makes it event-driven.
static REMOTE_WAKE: OnceLock<Arc<Notify>> = OnceLock::new();

/// Register the desktop's wake signal (idempotent).
pub fn set_remote_wake(signal: Arc<Notify>) {
    let _ = REMOTE_WAKE.set(signal);
}

fn signal_remote_wake() {
    if let Some(signal) = REMOTE_WAKE.get() {
        signal.notify_one();
    }
}

/// Which address the listener binds. See the module docs for the trade-off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// `0.0.0.0` — reachable from the LAN (default).
    Lan,
    /// `127.0.0.1` — this machine only (use with a tunnel).
    Loopback,
}

impl Bind {
    pub fn host(&self) -> &'static str {
        match self {
            Bind::Lan => "0.0.0.0",
            Bind::Loopback => "127.0.0.1",
        }
    }
}

/// Knobs for [`RemoteServer::start`].
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Preferred port; `0` picks an ephemeral one. A taken port falls back to
    /// any free one, and the actual port is reported by [`RemoteServer::status`].
    pub port: u16,
    pub bind: Bind,
    /// Device store to use. Defaults to the app's `remote/` config directory.
    pub store: Option<DeviceStore>,
    /// Start a cloudflared quick tunnel as soon as the server is up.
    pub tunnel: bool,
    pub handshake_timeout: Duration,
    pub replay_bytes: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            bind: Bind::Lan,
            store: None,
            tunnel: false,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            replay_bytes: DEFAULT_REPLAY_BYTES,
        }
    }
}

/// One thing a remote client asked the desktop to do. The GPUI thread applies
/// these to its own sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteInput {
    /// Raw keystrokes for `leaf`'s pane.
    Write { leaf: u64, bytes: Vec<u8> },
    Resize { leaf: u64, cols: u16, rows: u16 },
    /// Ask the desktop for a fresh tab, then point this connection at it.
    NewTab { conn: u64 },
    /// Path completion request for the token being typed.
    Completion { conn: u64, leaf: u64, token: String },
}

/// A paired device, for the UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub last_seen: Option<i64>,
    /// Live connections this device holds right now.
    pub connections: usize,
}

/// Everything the remote UI needs to render one frame.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub running: bool,
    pub port: u16,
    /// LAN URL without the pairing secret.
    pub url: String,
    /// LAN URL carrying the current single-use pairing secret (QR target).
    pub pair_url: Option<String>,
    /// Public `https://….trycloudflare.com/` URL once the tunnel is up.
    pub public_url: Option<String>,
    pub public_pair_url: Option<String>,
    /// Seconds left on the current pairing code.
    pub pairing_expires_in: Option<u64>,
    pub clients: usize,
    pub devices: Vec<DeviceStatus>,
    pub server_name: String,
    pub tunnel_requested: bool,
    pub tunnel_available: bool,
    pub bind_all: bool,
    /// Why the last start or tunnel attempt failed, if it did.
    pub last_error: Option<String>,
}

/// A live, authenticated bridge: one device watching (and driving) one pane.
struct ConnSlot {
    device_id: String,
    /// The pane list *and* the pane this connection drives, announced together.
    view: watch::Sender<ConnView>,
    /// Cuts this connection (device revocation).
    kill: broadcast::Sender<()>,
    /// Sealed-and-ready frames, in order, for the connection's writer task.
    /// Bounded: see [`OUTBOX_CAPACITY`].
    out: mpsc::Sender<OutMsg>,
    /// A screen rebuild queued for the next view announcement. It rides the
    /// announcement rather than the outbox so a client is never told about the
    /// pane it is on *after* being fed that pane's history.
    pending_replay: Arc<Mutex<Vec<u8>>>,
}

enum OutMsg {
    Output(Vec<u8>),
    /// JSON [`Control`] text, sealed as a `T` frame by the writer task.
    Control(String),
}

/// The published panes, and which one the *desktop* is on (the pane a new
/// connection is bridged to).
#[derive(Clone, Debug, PartialEq, Eq)]
struct TabsState {
    items: Vec<TabInfo>,
    active: Option<u64>,
}

/// What one connection is told: the shared panes and the pane it drives. It is
/// a single value so a client never sees a half-updated (torn) view.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConnView {
    items: Vec<TabInfo>,
    active: Option<u64>,
}

/// Mutable server lifecycle state, guarded by one fast mutex.
struct Runtime {
    running: bool,
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    /// Aborts every live bridge. Axum's graceful shutdown only stops accepting
    /// new connections, so without this a connected client keeps driving.
    kill: Option<broadcast::Sender<()>>,
    tunnel: Option<Child>,
    tunnel_requested: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::idle()
    }
}

impl Runtime {
    fn idle() -> Self {
        Self {
            running: false,
            port: 0,
            shutdown: None,
            kill: None,
            tunnel: None,
            tunnel_requested: false,
        }
    }
}

/// Shared server state. The runtime thread and its handlers hold `Weak` refs so
/// dropping the last [`RemoteServer`] handle tears the server down.
pub struct ServerState {
    options: ServerOptions,
    handshake: Mutex<ServerHandshake>,
    runtime: Mutex<Runtime>,
    conns: Mutex<HashMap<u64, ConnSlot>>,
    next_conn: AtomicU64,
    clients: AtomicUsize,
    /// The desktop's active leaf at share time: the pane new connections get.
    active_leaf: AtomicU64,
    tabs: Mutex<TabsState>,
    /// Recent output per leaf, replayed to a client when it attaches.
    replay: Mutex<HashMap<u64, Vec<u8>>>,
    /// The terminal modes each leaf's session is in — mouse reporting,
    /// alternate screen, bracketed paste, application keys. They live in the
    /// desktop's parsed `vt100` screen and a client cannot recover them from
    /// the bytes it is sent: a child sets each one once, and a late attach only
    /// replays a bounded tail. Published here so they can be put back in front
    /// of that tail.
    modes: Mutex<HashMap<u64, SessionModes>>,
    inputs: Mutex<Vec<RemoteInput>>,
    public_base: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
    status_cache: Mutex<Option<(Instant, RemoteStatus)>>,
}

/// Cheap, cloneable handle used from the GPUI thread.
#[derive(Clone)]
pub struct RemoteServer {
    state: Arc<ServerState>,
}

impl RemoteServer {
    /// Bind the listener and start the runtime. Fails only if no port at all
    /// could be bound.
    pub fn start(options: ServerOptions) -> Result<Self> {
        let store = match options.store.clone() {
            Some(store) => store,
            None => DeviceStore::open_default()
                .context("open the remote device store")?,
        };
        let handshake =
            ServerHandshake::from_store(store).context("load the remote server identity")?;
        let std_listener = bind_listener(options.bind, options.port)?;
        let port = std_listener
            .local_addr()
            .context("read the bound socket address")?
            .port();

        let tabs = Mutex::new(TabsState {
            items: Vec::new(),
            active: None,
        });
        let state = Arc::new(ServerState {
            options,
            handshake: Mutex::new(handshake),
            runtime: Mutex::new(Runtime {
                running: true,
                port,
                ..Runtime::idle()
            }),
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            clients: AtomicUsize::new(0),
            active_leaf: AtomicU64::new(NO_LEAF),
            tabs,
            replay: Mutex::new(HashMap::new()),
            modes: Mutex::new(HashMap::new()),
            inputs: Mutex::new(Vec::new()),
            public_base: Mutex::new(None),
            last_error: Mutex::new(None),
            status_cache: Mutex::new(None),
        });

        let (kill_tx, _) = broadcast::channel::<()>(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        {
            let mut runtime = state.runtime.lock().expect("runtime lock");
            runtime.shutdown = Some(shutdown_tx);
            runtime.kill = Some(kill_tx);
        }

        let weak = Arc::downgrade(&state);
        std::thread::Builder::new()
            .name("spritx-remote".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(serve(std_listener, weak, shutdown_rx));
            })
            .context("spawn the remote server thread")?;

        let server = Self { state };
        // A share always opens with a fresh single-use pairing code.
        server.new_pairing();
        if server.options().tunnel {
            server.start_tunnel();
        }
        Ok(server)
    }

    fn options(&self) -> ServerOptions {
        self.state.options.clone()
    }

    pub fn is_running(&self) -> bool {
        self.state.runtime.lock().map(|r| r.running).unwrap_or(false)
    }

    /// How many clients are attached right now. A plain atomic load, unlike
    /// [`RemoteServer::status`], which formats strings for the UI — the pane
    /// renderer asks this on every frame (see `render_live_terminal`).
    pub fn client_count(&self) -> usize {
        self.state.clients.load(Ordering::Relaxed)
    }

    /// The port actually bound (0 when stopped).
    pub fn port(&self) -> u16 {
        self.state.runtime.lock().map(|r| r.port).unwrap_or(0)
    }

    /// Stop accepting, cut every live bridge, kill the tunnel and drop the
    /// pairing code.
    pub fn stop(&self) {
        let (shutdown, kill, tunnel) = {
            let mut runtime = match self.state.runtime.lock() {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            runtime.running = false;
            runtime.port = 0;
            runtime.tunnel_requested = false;
            (
                runtime.shutdown.take(),
                runtime.kill.take(),
                runtime.tunnel.take(),
            )
        };
        if let Some(tx) = shutdown {
            let _ = tx.send(());
        }
        // Stopping the share must revoke access: no bridge may keep driving.
        if let Some(kill) = kill {
            let _ = kill.send(());
        }
        if let Some(child) = tunnel {
            // Let the close frame reach the peer before removing its transport.
            // Never wait for a network client on the GPUI event loop.
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_millis(150);
                while state.clients.load(Ordering::Relaxed) > 0 && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(2));
                }
                reap(child);
            });
        }
        *self.state.public_base.lock().expect("public base lock") = None;
        if let Ok(mut handshake) = self.state.handshake.lock() {
            handshake.close_pairing();
        }
        self.state.conns.lock().expect("conns lock").clear();
        self.state.replay.lock().expect("replay lock").clear();
        self.state.inputs.lock().expect("inputs lock").clear();
        {
            let mut runtime = self.state.runtime.lock().expect("runtime lock");
            runtime.running = false;
            runtime.port = 0;
            runtime.tunnel_requested = false;
        }
        self.state.invalidate_status();
    }

    /// Publish the panes remote clients may pick, and which one the desktop is
    /// on. A connection whose pane vanished falls back to the first shared one.
    pub fn set_tabs(&self, tabs: Vec<TabInfo>) {
        let current = self.state.active_leaf.load(Ordering::Relaxed);
        let keeps = current != NO_LEAF && tabs.iter().any(|tab| tab.id == current);
        let active = if keeps { Some(current) } else { None };
        *self.state.tabs.lock().expect("tabs lock") = TabsState {
            items: tabs.clone(),
            active,
        };
        // Panes that are gone stop pinning a mode entry for the life of the
        // process.
        self.state
            .modes
            .lock()
            .expect("modes lock")
            .retain(|id, _| tabs.iter().any(|tab| tab.id == *id));
        // Every live connection gets the list and, if its pane is gone, a new
        // target — as one write, so no client can observe a torn view.
        let mut conns = self.state.conns.lock().expect("conns lock");
        for slot in conns.values_mut() {
            let previous = slot.view.borrow().active;
            let chosen = if previous.is_some_and(|id| tabs.iter().any(|tab| tab.id == id)) {
                previous
            } else {
                active.or(tabs.first().map(|tab| tab.id))
            };
            slot.view.send_replace(ConnView {
                items: tabs.clone(),
                active: chosen,
            });
        }
        self.state.invalidate_status();
    }

    /// The pane a *new* connection is bridged to (the desktop's active leaf).
    pub fn set_active_leaf(&self, leaf: u64) {
        self.state.active_leaf.store(leaf, Ordering::Relaxed);
    }

    /// Publish the modes `leaf`'s session is in (the parsed `vt100` state of its
    /// terminal: see [`SessionModes`]).
    ///
    /// A child sets these once — when it starts, or when it opens its
    /// fullscreen screen — and an attaching client is rebuilt from a bounded
    /// replay tail that no longer holds those sequences. Without them its
    /// terminal is not the one the child is talking to: a mouse-owning TUI gets
    /// no taps or swipes at all, and a fullscreen app is painted into the wrong
    /// buffer. [`ServerState::replay_for`] puts them back in front of that tail.
    ///
    /// Cheap and idempotent, so the UI pump can call it every tick.
    pub fn set_modes(&self, leaf: u64, modes: SessionModes) {
        let mut current = self.state.modes.lock().expect("modes lock");
        // A session in no special mode needs no entry at all.
        if modes == SessionModes::default() {
            current.remove(&leaf);
        } else if current.get(&leaf) != Some(&modes) {
            current.insert(leaf, modes);
        }
    }

    /// Forward one PTY output chunk for `leaf` to every bridge watching it and
    /// remember it for late-attaching clients. Cheap: called from the UI pump.
    pub fn ingest(&self, leaf: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _mem = crate::memprof::scope(crate::memprof::Region::Remote);
        let limit = self.state.options.replay_bytes;
        if limit > 0 {
            let mut replay = self.state.replay.lock().expect("replay lock");
            let tail = replay.entry(leaf).or_default();
            tail.extend_from_slice(bytes);
            if tail.len() > limit {
                let drop = tail.len() - limit;
                tail.drain(..drop);
            }
        }
        let mut conns = self.state.conns.lock().expect("conns lock");
        let mut stalled: Vec<u64> = Vec::new();
        for (id, slot) in conns.iter_mut() {
            if slot.view.borrow().active != Some(leaf) {
                continue;
            }
            match slot.out.try_send(OutMsg::Output(bytes.to_vec())) {
                // A closed bridge task needs no cleanup here: the bridge itself
                // deregisters.
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                // A client that stops reading gets cut rather than buffered.
                Err(mpsc::error::TrySendError::Full(_)) => stalled.push(*id),
            }
        }
        for id in stalled {
            if let Some(slot) = conns.remove(&id) {
                let _ = slot.kill.send(());
            }
        }
    }

    /// Answer one connection's completion request with a JSON control frame
    /// (`{"t":"ls",…}`, see [`listing_json`]). It is sealed by that
    /// connection's own writer task, so frame counters stay in order.
    pub fn reply_control(&self, conn: u64, json: String) {
        let conns = self.state.conns.lock().expect("conns lock");
        if let Some(slot) = conns.get(&conn) {
            let _ = slot.out.try_send(OutMsg::Control(json));
        }
    }

    /// Drain what remote clients asked the desktop to do.
    pub fn take_inputs(&self) -> Vec<RemoteInput> {
        let mut guard = match self.state.inputs.lock() {
            Ok(guard) => guard,
            Err(_) => return Vec::new(),
        };
        std::mem::take(&mut *guard)
    }

    /// Open a fresh single-use pairing code and return its QR URL.
    pub fn new_pairing(&self) -> Option<String> {
        // Take the base URL first: `base_url` locks the runtime, and status
        // locks the runtime before the handshake, so the order must match.
        let base = self.base_url();
        let url = {
            let mut handshake = self.state.handshake.lock().ok()?;
            handshake.open_pairing().url(&base)
        };
        self.state.invalidate_status();
        Some(url)
    }

    /// The pairing URL if a code is currently open (and not expired).
    pub fn pair_url(&self) -> Option<String> {
        let base = self.base_url();
        let handshake = self.state.handshake.lock().ok()?;
        Some(handshake.pairing()?.url(&base))
    }

    /// The open pairing code's URL fragment, if any.
    fn pairing_fragment(&self) -> Option<String> {
        let handshake = self.state.handshake.lock().ok()?;
        Some(handshake.pairing()?.fragment())
    }

    /// Seconds left on the open pairing code.
    fn pairing_expires_in(&self) -> Option<u64> {
        let handshake = self.state.handshake.lock().ok()?;
        let pairing = handshake.pairing()?;
        Some(pairing.expires_in().as_secs())
    }

    /// Open a pairing code with a custom TTL and return its secret. Production
    /// callers want [`RemoteServer::new_pairing`]; tests use a zero TTL to
    /// exercise the expired-code path.
    pub fn open_pairing_with_ttl(&self, ttl: Duration) -> Option<[u8; KEY_LEN]> {
        let secret = {
            let mut handshake = self.state.handshake.lock().ok()?;
            let pairing = handshake.open_pairing_with_ttl(ttl);
            let mut bytes = [0u8; KEY_LEN];
            bytes.copy_from_slice(pairing.secret());
            bytes
        };
        self.state.invalidate_status();
        Some(secret)
    }

    /// The raw pairing secret, for a SpritX↔SpritX client and for tests. The secret
    /// is single-use: a successful `pair` frame consumes it.
    pub fn pairing_secret(&self) -> Option<[u8; KEY_LEN]> {
        let handshake = self.state.handshake.lock().ok()?;
        let pairing = handshake.pairing()?;
        let mut secret = [0u8; KEY_LEN];
        secret.copy_from_slice(pairing.secret());
        Some(secret)
    }

    /// Forget a device and cut its live connections. `Ok(true)` when it existed.
    pub fn revoke(&self, device_id: &str) -> Result<bool> {
        let existed = {
            let handshake = self.state.handshake.lock().map_err(|_| anyhow!("store poisoned"))?;
            handshake.store().revoke(device_id)?
        };
        let dead: Vec<u64> = {
            let conns = self.state.conns.lock().expect("conns lock");
            conns.iter()
                .filter(|(_, slot)| slot.device_id == device_id)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in dead {
            let slot = self.state.conns.lock().expect("conns lock").remove(&id);
            if let Some(slot) = slot {
                let _ = slot.kill.send(());
            }
        }
        self.state.invalidate_status();
        Ok(existed)
    }

    /// Forget every paired device and cut all live bridges.
    pub fn forget_devices(&self) -> Result<()> {
        {
            let handshake = self.state.handshake.lock().map_err(|_| anyhow!("store poisoned"))?;
            handshake.store().forget()?;
        }
        let dead: Vec<u64> = self
            .state
            .conns
            .lock()
            .expect("conns lock")
            .keys()
            .copied()
            .collect();
        for id in dead {
            self.state.conn_kill(id);
        }
        self.state.invalidate_status();
        Ok(())
    }

    /// Spawn `cloudflared tunnel …` for the running server. Returns whether the
    /// child started; the public URL appears in [`RemoteServer::status`] once
    /// the tunnel registers. No download/install happens here.
    pub fn start_tunnel(&self) -> bool {
        let port = self.port();
        if port == 0 {
            *self.state.last_error.lock().expect("error lock") =
                Some("start remote sharing before the tunnel".to_string());
            self.state.invalidate_status();
            return false;
        }
        if self.state.runtime.lock().expect("runtime lock").tunnel.is_some() {
            return true;
        }
        match spawn_tunnel(port, &self.state) {
            Ok(child) => {
                let mut runtime = self.state.runtime.lock().expect("runtime lock");
                runtime.tunnel = Some(child);
                runtime.tunnel_requested = true;
                drop(runtime);
                *self.state.last_error.lock().expect("error lock") = None;
                self.state.invalidate_status();
                true
            }
            Err(error) => {
                *self.state.last_error.lock().expect("error lock") =
                    Some(error.to_string());
                let mut runtime = self.state.runtime.lock().expect("runtime lock");
                runtime.tunnel_requested = false;
                drop(runtime);
                self.state.invalidate_status();
                false
            }
        }
    }

    pub fn stop_tunnel(&self) {
        let child = self.state.runtime.lock().expect("runtime lock").tunnel.take();
        if let Some(child) = child {
            reap(child);
        }
        *self.state.public_base.lock().expect("public base lock") = None;
        {
            let mut runtime = self.state.runtime.lock().expect("runtime lock");
            runtime.tunnel_requested = false;
        }
        self.state.invalidate_status();
    }

    pub fn tunnel_available(&self) -> bool {
        cloudflared_available()
    }

    /// Reachable base URL, e.g. `http://192.168.1.42:4530`. A loopback-bound
    /// server reports `127.0.0.1`, since that is all a client could use.
    pub fn base_url(&self) -> String {
        let port = self.port();
        if port == 0 {
            return String::new();
        }
        let host = match self.state.options.bind {
            Bind::Loopback => "127.0.0.1".to_string(),
            Bind::Lan => local_ip().unwrap_or_else(|| "127.0.0.1".into()),
        };
        format!("http://{host}:{port}")
    }

    /// Cached view of the server for the UI (device file I/O at most every
    /// 250 ms, since `status` is called while rendering).
    pub fn status(&self) -> RemoteStatus {
        if let Ok(cache) = self.state.status_cache.lock() {
            if let Some((at, status)) = cache.as_ref() {
                if at.elapsed() < Duration::from_millis(250) {
                    // The live client count is cheap, so refresh it always.
                    let mut fresh = status.clone();
                    fresh.clients = self.state.clients.load(Ordering::Relaxed);
                    return fresh;
                }
            }
        }
        let status = self.compute_status();
        if let Ok(mut cache) = self.state.status_cache.lock() {
            *cache = Some((Instant::now(), status.clone()));
        }
        status
    }

    /// Recompute the status immediately (use after an action worth showing).
    pub fn status_now(&self) -> RemoteStatus {
        self.state.invalidate_status();
        self.status()
    }

    fn compute_status(&self) -> RemoteStatus {
        let (running, port, tunnel_requested) = {
            let runtime = self.state.runtime.lock().expect("runtime lock");
            (runtime.running, runtime.port, runtime.tunnel_requested)
        };
        let url = if running { self.base_url() } else { String::new() };
        let public_url = self
            .state
            .public_base
            .lock()
            .expect("public lock")
            .clone()
            .map(|base| format!("{base}/"));
        let pair_url = if running { self.pair_url() } else { None };
        let pairing_fragment = if running { self.pairing_fragment() } else { None };
        let expires = if running { self.pairing_expires_in() } else { None };
        let per_device: HashMap<String, usize> = {
            let conns = self.state.conns.lock().expect("conns lock");
            let mut counts = HashMap::new();
            for slot in conns.values() {
                *counts.entry(slot.device_id.clone()).or_default() += 1;
            }
            counts
        };
        let devices: Vec<DeviceStatus> = {
            let handshake = match self.state.handshake.lock() {
                Ok(handshake) => handshake,
                Err(_) => return RemoteStatus::offline(),
            };
            let mut devices: Vec<DeviceStatus> = handshake
                .store()
                .devices()
                .into_iter()
                .map(|device: Device| DeviceStatus {
                    connections: per_device.get(&device.id).copied().unwrap_or(0),
                    id: device.id,
                    name: device.name,
                    created: device.created,
                    last_seen: device.last_seen,
                })
                .collect();
            devices.sort_by(|a, b| {
                b.last_seen
                    .unwrap_or(b.created)
                    .cmp(&a.last_seen.unwrap_or(a.created))
            });
            devices
        };
        let server_name = {
            self.state
                .handshake
                .lock()
                .map(|handshake| handshake.server_name().to_string())
                .unwrap_or_else(|_| "SpritX".to_string())
        };
        RemoteStatus {
            running,
            port,
            pair_url,
            public_pair_url: match (&public_url, &pairing_fragment) {
                (Some(base), Some(fragment)) => {
                    Some(format!("{}{fragment}", base.trim_end_matches('/')))
                }
                _ => None,
            },
            pairing_expires_in: expires,
            url,
            public_url,
            tunnel_requested,
            tunnel_available: cloudflared_available(),
            clients: self.state.clients.load(Ordering::Relaxed),
            devices,
            server_name,
            bind_all: matches!(self.state.options.bind, Bind::Lan),
            last_error: self.state.last_error.lock().expect("error lock").clone(),
        }
    }
}

impl Drop for ServerState {
    fn drop(&mut self) {
        // The last handle went away (app quit without an explicit stop): cut the
        // bridges and never leave an orphaned cloudflared behind.
        let runtime = std::mem::take(&mut *self.runtime.lock().expect("runtime lock"));
        if let Some(kill) = runtime.kill {
            let _ = kill.send(());
        }
        if let Some(child) = runtime.tunnel {
            reap(child);
        }
    }
}

impl RemoteStatus {
    /// The status of a server that is not running.
    pub fn offline() -> Self {
        Self {
            running: false,
            port: 0,
            url: String::new(),
            pair_url: None,
            public_url: None,
            public_pair_url: None,
            pairing_expires_in: None,
            clients: 0,
            devices: Vec::new(),
            server_name: "SpritX".to_string(),
            tunnel_requested: false,
            tunnel_available: false,
            bind_all: false,
            last_error: None,
        }
    }
}

fn bind_listener(bind: Bind, port: u16) -> Result<StdTcpListener> {
    let host = bind.host();
    StdTcpListener::bind((host, port))
        .or_else(|_| StdTcpListener::bind((host, 0)))
        .with_context(|| format!("bind {host}:{port}"))
}

/// Kill a tunnel child now and reap it without blocking the caller.
fn reap(mut child: Child) {
    let _ = child.kill();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

// ---------------------------------------------------------------------------
// HTTP / WebSocket
// ---------------------------------------------------------------------------

/// Axum state: a `Weak` ref so the router never keeps the server alive.
#[derive(Clone)]
struct WeakState(Weak<ServerState>);

async fn serve(
    std_listener: StdTcpListener,
    state: Weak<ServerState>,
    shutdown: oneshot::Receiver<()>,
) {
    if std_listener.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
        return;
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/favicon.ico", get(|| async { axum::http::StatusCode::NO_CONTENT }))
        .with_state(WeakState(state));
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
        })
        .await;
}

/// The client page. Static and public — it holds no secret until a device
/// pairs, so caching it would only serve stale builds.
async fn index() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(INDEX_HTML),
    )
        .into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, AxumState(weak): AxumState<WeakState>) -> Response {
    match weak.0.upgrade() {
        Some(state) => ws.on_upgrade(move |socket| bridge(socket, state)),
        None => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

type WsSink = futures_util::stream::SplitSink<WebSocket, Message>;
type WsStream = futures_util::stream::SplitStream<WebSocket>;

async fn send_text(sink: &mut WsSink, text: String) -> bool {
    sink.send(Message::Text(text.into())).await.is_ok()
}

/// Next JSON text frame of the handshake (`None` on close / binary / garbage).
async fn next_json(stream: &mut WsStream) -> Option<String> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(text)) => return Some(text.to_string()),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Binary(_)) | Ok(Message::Close(_)) => return None,
            Err(_) => return None,
        }
    }
}

async fn next_binary(stream: &mut WsStream) -> Option<Vec<u8>> {
    loop {
        match stream.next().await? {
            Ok(Message::Binary(bytes)) => return Some(bytes.to_vec()),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Text(_)) | Ok(Message::Close(_)) => return None,
            Err(_) => return None,
        }
    }
}

/// Pairing / authentication. Drives the protocol state machine in
/// [`ServerHandshake`], which is shared by all connections: each `handle` call
/// is one text frame, and the single-use pairing secret is consumed inside the
/// mutex, so a second device with the same QR code is denied.
async fn handshake(
    sink: &mut WsSink,
    stream: &mut WsStream,
    state: &ServerState,
) -> Option<Established> {
    if !send_text(sink, state.hello_json()).await {
        return None;
    }
    loop {
        let text = next_json(stream).await?;
        let step = {
            let mut handshake = state.handshake.lock().ok()?;
            handshake.handle(&text)
        };
        match step {
            ServerStep::Reply(reply) => {
                if !send_text(sink, reply).await {
                    return None;
                }
            }
            ServerStep::Denied { reply } => {
                let _ = send_text(sink, reply).await;
                return None;
            }
            ServerStep::Established { reply, mut session } => {
                if !send_text(sink, reply).await {
                    return None;
                }
                // The first sealed frame must prove the device holds its key.
                let first = next_binary(stream).await?;
                if !session.accept_ready(&first) {
                    return None;
                }
                return Some(session);
            }
        }
    }
}

/// One remote connection for the lifetime of its bridge.
struct ClientGuard<'a>(&'a ServerState);

impl<'a> ClientGuard<'a> {
    fn new(state: &'a ServerState) -> Self {
        state.clients.fetch_add(1, Ordering::Relaxed);
        Self(state)
    }
}

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Run one client: handshake, then bridge its pane.
async fn bridge(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();
    let mut global_kill = state.runtime_kill();
    let session = {
        let timeout = state.options.handshake_timeout;
        tokio::select! {
            biased;
            _ = global_kill.recv() => return,
            result = tokio::time::timeout(timeout, handshake(&mut sink, &mut stream, &state)) => match result {
                Ok(Some(session)) => session,
                _ => return,
            }
        }
    };
    let _clients = ClientGuard::new(&state);

    let conn_id = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (kill_tx, kill_rx) = broadcast::channel::<()>(1);
    let (out_tx, out_rx) = mpsc::channel::<OutMsg>(OUTBOX_CAPACITY);
    let view = state.initial_view();
    let target = view.active;
    let (view_tx, view_rx) = watch::channel(view);
    let pending_replay = Arc::new(Mutex::new(Vec::new()));
    state.conns.lock().expect("conns lock").insert(
        conn_id,
        ConnSlot {
            device_id: session.device_id.clone(),
            view: view_tx,
            kill: kill_tx.clone(),
            out: out_tx.clone(),
            pending_replay: Arc::clone(&pending_replay),
        },
    );
    // Rebuild this client's screen from the pane's own history so it does not
    // start from a blank terminal. The tail is already buffered here, so no
    // round trip to the GPUI thread is needed.
    if let Some(leaf) = target {
        state.queue_replay(conn_id, leaf);
    }

    let mut writer = tokio::spawn(output_task(
        sink,
        view_rx,
        out_rx,
        session.tx,
        pending_replay,
        global_kill.resubscribe(),
    ));
    let mut reader = tokio::spawn(input_task(
        stream,
        Arc::clone(&state),
        conn_id,
        session.rx,
    ));

    let mut kill_conn = kill_rx;
    let mut global = global_kill;
    tokio::select! {
        _ = &mut writer => reader.abort(),
        _ = &mut reader => writer.abort(),
        _ = kill_conn.recv() => { writer.abort(); reader.abort(); }
        _ = global.recv() => {
            reader.abort();
            // The writer owns the socket and sends the close frame. Bound the
            // flush so a stalled peer cannot retain a bridge indefinitely.
            if tokio::time::timeout(Duration::from_millis(100), &mut writer).await.is_err() {
                writer.abort();
            }
        }
    }
    state.conns.lock().expect("conns lock").remove(&conn_id);
}

/// The single writer of a connection: seals every server→client frame in order
/// so the per-direction nonce counter cannot interleave.
async fn output_task(
    mut sink: WsSink,
    view_rx: watch::Receiver<ConnView>,
    out_rx: mpsc::Receiver<OutMsg>,
    tx: Half,
    pending_replay: Arc<Mutex<Vec<u8>>>,
    mut stop: broadcast::Receiver<()>,
) {
    tokio::select! {
        biased;
        _ = stop.recv() => {},
        _ = output_frames(&mut sink, view_rx, out_rx, tx, pending_replay) => return,
    }
    let close = Message::Close(Some(axum::extract::ws::CloseFrame {
        code: 1001,
        reason: "Sharing stopped".into(),
    }));
    let _ = tokio::time::timeout(Duration::from_millis(75), sink.send(close)).await;
}

async fn output_frames(
    mut sink: &mut WsSink,
    mut view_rx: watch::Receiver<ConnView>,
    mut out_rx: mpsc::Receiver<OutMsg>,
    mut tx: Half,
    pending_replay: Arc<Mutex<Vec<u8>>>,
) {
    // Take whatever screen rebuild the last view change queued, and send it
    // immediately after the announcement that goes with it.
    macro_rules! drain_replay {
        () => {{
            let queued = pending_replay
                .lock()
                .map(|mut pending| std::mem::take(&mut *pending))
                .unwrap_or_default();
            if queued.is_empty() {
                true
            } else {
                send_sealed(&mut sink, &mut tx, ServerFrame::Output(queued)).await
            }
        }};
    }

    // Remember what this client was last told: a tab switch that also changes
    // the published list must not announce the same list twice.
    let mut announced: Option<String> = None;
    let view = view_rx.borrow_and_update().clone();
    if !announce(&mut sink, &mut tx, view, &mut announced).await {
        return;
    }
    if !drain_replay!() {
        return;
    }
    loop {
        tokio::select! {
            msg = out_rx.recv() => {
                let Some(msg) = msg else { break };
                let frame = match msg {
                    OutMsg::Output(bytes) => ServerFrame::Output(bytes),
                    OutMsg::Control(json) => match Control::from_json(&json) {
                        Some(control) => ServerFrame::Control(control),
                        None => continue,
                    },
                };
                if !send_sealed(&mut sink, &mut tx, frame).await { break; }
            }
            changed = view_rx.changed() => {
                if changed.is_err() { break; }
                let view = view_rx.borrow_and_update().clone();
                if !announce(&mut sink, &mut tx, view, &mut announced).await { break; }
                if !drain_replay!() { break; }
            }
        }
    }
    let _ = sink.send(Message::Close(None)).await;
}

/// Send the pane list unless it is byte-identical to what this client last got.
/// Skipping happens before sealing, so the frame counters stay in step.
async fn announce(
    sink: &mut WsSink,
    tx: &mut Half,
    view: ConnView,
    announced: &mut Option<String>,
) -> bool {
    let control = Control::tabs(view.items, view.active);
    let json = control.to_json();
    if announced.as_deref() == Some(json.as_str()) {
        return true;
    }
    *announced = Some(json);
    send_sealed(sink, tx, ServerFrame::Control(control)).await
}

async fn send_sealed(sink: &mut WsSink, tx: &mut Half, frame: ServerFrame) -> bool {
    sink.send(Message::Binary(frame.seal(tx).into())).await.is_ok()
}

/// Opens client frames (any frame that fails to authenticate kills the
/// connection) and translates them into desktop actions.
async fn input_task(
    mut stream: WsStream,
    state: Arc<ServerState>,
    conn_id: u64,
    mut rx: Half,
) {
    loop {
        let Some(message) = stream.next().await else {
            break;
        };
        let boxed = match message {
            Ok(Message::Binary(bytes)) => bytes,
            // Post-handshake text is not part of the protocol.
            Ok(Message::Text(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => break,
        };
        let Some(frame) = rx.open(&boxed).map(|plain| ClientFrame::decode(&plain)) else {
            break;
        };
        match frame {
            ClientFrame::Input(bytes) => {
                if let Some(leaf) = state.conn_target(conn_id) {
                    state.push_input(RemoteInput::Write { leaf, bytes });
                }
            }
            ClientFrame::Resize { cols, rows } => {
                if let Some(leaf) = state.conn_target(conn_id) {
                    state.push_input(RemoteInput::Resize { leaf, cols, rows });
                }
            }
            ClientFrame::Completion(token) => {
                if let Some(leaf) = state.conn_target(conn_id) {
                    state.push_input(RemoteInput::Completion { conn: conn_id, leaf, token });
                }
            }
            ClientFrame::NewTab => state.push_input(RemoteInput::NewTab { conn: conn_id }),
            // Only to a pane the desktop actually shares; a client switching
            // never moves another connection or the desktop.
            ClientFrame::SwitchTab(id) => state.switch_target(conn_id, id),
        }
    }
    state.conn_kill(conn_id);
}

// ---------------------------------------------------------------------------
// State helpers used by the transport tasks
// ---------------------------------------------------------------------------

impl ServerState {
    /// Drop the cached status so the next read recomputes it.
    fn invalidate_status(&self) {
        if let Ok(mut cache) = self.status_cache.lock() {
            *cache = None;
        }
    }

    fn hello_json(&self) -> String {
        match self.handshake.lock() {
            Ok(handshake) => handshake.hello_json(),
            Err(_) => Control::denied("storage").to_json(),
        }
    }

    /// What a new connection sees: the shared panes, bridged to the desktop's
    /// active leaf at share time (else the first shared pane).
    fn initial_view(&self) -> ConnView {
        let tabs = self.tabs.lock().expect("tabs lock");
        let active = {
            let current = self.active_leaf.load(Ordering::Relaxed);
            if current != NO_LEAF && tabs.items.iter().any(|tab| tab.id == current) {
                Some(current)
            } else {
                tabs.active.or(tabs.items.first().map(|tab| tab.id))
            }
        };
        ConnView {
            items: tabs.items.clone(),
            active: active.filter(|id| tabs.items.iter().any(|tab| &tab.id == id)),
        }
    }

    /// The pane this connection currently drives.
    fn conn_target(&self, conn_id: u64) -> Option<u64> {
        let conns = self.conns.lock().ok()?;
        let slot = conns.get(&conn_id)?;
        let active: Option<u64> = slot.view.borrow().active;
        active
    }

    /// Everything a client needs to rebuild `leaf`'s screen: the modes the
    /// child's own history can no longer supply, then that history.
    ///
    /// The history is a bounded tail and a child sets its modes once, so they
    /// are usually not in it. Putting them back at the head of the stream is
    /// what makes the client's terminal the one the child is talking to at all
    /// (see [`RemoteServer::set_modes`]).
    fn replay_for(&self, leaf: u64) -> Vec<u8> {
        let tail = self
            .replay
            .lock()
            .expect("replay lock")
            .get(&leaf)
            .cloned()
            .unwrap_or_default();
        let modes = self
            .modes
            .lock()
            .ok()
            .and_then(|modes| modes.get(&leaf).copied());
        let mut out = modes.map(|modes| modes.restore_sequence()).unwrap_or_default();
        out.extend_from_slice(&tail);
        out
    }

    /// Queue `leaf`'s raw history to rebuild `conn`'s screen on its next view
    /// announcement. Riding the announcement rather than the outbox keeps the
    /// client told about its pane *before* it is fed that pane's history.
    ///
    /// The tail is the *raw* PTY byte stream, so it carries colours, attributes
    /// and alternate-screen state. Dumping the parsed cell grid as plain text
    /// instead — what this used to do via the desktop — threw all of that away
    /// and left a freshly attached client painting an unstyled screen until the
    /// next command produced output.
    fn queue_replay(&self, conn: u64, leaf: u64) {
        let replay = self.replay_for(leaf);
        let conns = self.conns.lock().expect("conns lock");
        if let Some(slot) = conns.get(&conn) {
            if let Ok(mut pending) = slot.pending_replay.lock() {
                *pending = replay;
            }
        }
    }

    /// Repoint one connection at a shared pane, and rebuild its screen from that
    /// pane's history. Without the rebuild the client keeps showing the pane it
    /// just left until new output happens to arrive.
    fn switch_target(&self, conn_id: u64, leaf: u64) {
        // Take the replay lock first: `ingest` locks replay before conns, and
        // the opposite order here would deadlock against it.
        let replay = self.replay_for(leaf);
        let mut conns = match self.conns.lock() {
            Ok(conns) => conns,
            Err(_) => return,
        };
        let Some(slot) = conns.get_mut(&conn_id) else {
            return;
        };
        let view = slot.view.borrow().clone();
        if !view.items.iter().any(|tab| tab.id == leaf) || view.active == Some(leaf) {
            return;
        }
        if let Ok(mut pending) = slot.pending_replay.lock() {
            *pending = replay;
        }
        slot.view.send_replace(ConnView {
            active: Some(leaf),
            ..view
        });
    }

    fn push_input(&self, input: RemoteInput) {
        // Serialize the access check with stop(), before touching the queue.
        let Ok(runtime) = self.runtime.lock() else { return; };
        if !runtime.running { return; }
        if let Ok(mut queue) = self.inputs.lock() {
            queue.push(input);
        }
        signal_remote_wake();
    }

    fn runtime_kill(&self) -> broadcast::Receiver<()> {
        match self.runtime.lock() {
            Ok(runtime) => runtime
                .kill
                .as_ref()
                .map(|tx| tx.subscribe())
                .unwrap_or_else(closed_channel),
            Err(_) => closed_channel(),
        }
    }

    fn conn_kill(&self, conn_id: u64) {
        let slot = self.conns.lock().ok().and_then(|mut c| c.remove(&conn_id));
        if let Some(slot) = slot {
            let _ = slot.kill.send(());
        }
    }
}

/// A channel that is already closed, so `recv()` returns immediately.
fn closed_channel() -> broadcast::Receiver<()> {
    let (tx, rx) = broadcast::channel::<()>(1);
    drop(tx);
    rx
}

// ---------------------------------------------------------------------------
// cloudflared quick tunnel
// ---------------------------------------------------------------------------

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// SpritX's own install directory for a bundled cloudflared.
fn spritx_bin_dir() -> Option<String> {
    if cfg!(windows) {
        std::env::var("LOCALAPPDATA").ok().map(|dir| format!("{dir}\\spritx\\bin"))
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME").ok().map(|h| format!("{h}/Library/Application Support/spritx/bin"))
    } else {
        std::env::var("HOME").ok().map(|h| format!("{h}/.local/share/spritx/bin"))
    }
}

/// Resolve the binary; a GUI process's PATH can be minimal, so check the usual
/// install locations before falling back to a PATH lookup.
fn cloudflared_bin() -> String {
    let name = exe_name();
    if let Some(dir) = spritx_bin_dir() {
        let path = format!("{dir}/{name}");
        if std::path::Path::new(&path).is_file() {
            return path;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let path = format!("{home}/.local/bin/{name}");
        if std::path::Path::new(&path).is_file() {
            return path;
        }
    }
    for dir in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin"] {
        let path = format!("{dir}/{name}");
        if std::path::Path::new(&path).is_file() {
            return path;
        }
    }
    name.to_string()
}

/// Is a `cloudflared` binary usable (installed in SpritX's directory or on PATH)?
///
/// Probed at most once per [`ENV_PROBE_TTL`]: the answer feeds `status`, which
/// the UI polls several times a second, and each probe forks a child process.
pub fn cloudflared_available() -> bool {
    static CACHE: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
    let slot = CACHE.get_or_init(|| Mutex::new(None));
    cached_probe(slot, || {
        Command::new(cloudflared_bin())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Environment probes are cached this long. Long enough to stop a render loop
/// from hammering the OS, short enough that installing cloudflared or switching
/// networks is picked up without a restart.
const ENV_PROBE_TTL: Duration = Duration::from_secs(30);

fn cached_probe<T: Clone>(slot: &Mutex<Option<(Instant, T)>>, probe: impl Fn() -> T) -> T {
    if let Ok(guard) = slot.lock() {
        if let Some((at, value)) = guard.as_ref() {
            if at.elapsed() < ENV_PROBE_TTL {
                return value.clone();
            }
        }
    }
    let value = probe();
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((Instant::now(), value.clone()));
    }
    value
}

/// Extract a `https://*.trycloudflare.com` base URL from a cloudflared log line.
fn extract_trycloudflare(line: &str) -> Option<String> {
    let pos = line.find("https://")?;
    let rest = &line[pos..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let base = &rest[..end];
    if base.contains(".trycloudflare.com") {
        Some(base.to_string())
    } else {
        None
    }
}

fn spawn_tunnel(port: u16, state: &Arc<ServerState>) -> Result<Child> {
    if !cloudflared_available() {
        return Err(anyhow!(
            "cloudflared was not found — install it to share outside the LAN"
        ));
    }
    let mut child = Command::new(cloudflared_bin())
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn cloudflared")?;

    // cloudflared logs the URL on stderr; read both streams so a wording change
    // can't hide it, and keep draining or its pipe blocks and the tunnel drops.
    let mut readers: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(Box::new(out));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(Box::new(err));
    }
    // Publishing the URL the instant it is printed can hit a Cloudflare 1033 —
    // the QUIC connection registers a moment later — so hold it until the
    // "registered" log line appears, with an 8 s fallback.
    let pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let weak = Arc::downgrade(state);
    for reader in readers {
        let pending = Arc::clone(&pending);
        let weak = weak.clone();
        std::thread::spawn(move || {
            let reader = io::BufReader::new(reader);
            for line in reader.lines().map_while(Result::ok) {
                if pending.lock().map(|p| p.is_none()).unwrap_or(false) {
                    if let Some(base) = extract_trycloudflare(&line) {
                        if let Ok(mut slot) = pending.lock() {
                            *slot = Some(base);
                        }
                    }
                }
                let low = line.to_lowercase();
                if low.contains("registered tunnel connection")
                    || low.contains("connection registered")
                    || low.contains("registered tunnel")
                {
                    publish(&weak, &pending);
                }
            }
        });
    }
    {
        let pending = Arc::clone(&pending);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(8));
            publish(&weak, &pending);
        });
    }
    Ok(child)
}

fn publish(weak: &Weak<ServerState>, pending: &Arc<Mutex<Option<String>>>) {
    let base = pending.lock().ok().and_then(|slot| slot.clone());
    let Some(base) = base else { return };
    let Some(state) = weak.upgrade() else { return };
    let published = {
        let mut public = match state.public_base.lock() {
            Ok(public) => public,
            Err(_) => return,
        };
        if public.is_none() {
            *public = Some(base);
            true
        } else {
            false
        }
    };
    if published {
        state.invalidate_status();
    }
}

/// Best-effort LAN IP: bind a UDP socket "to" a public address (no packets go
/// out) and read which local interface the OS picked.
/// The LAN address clients should use. Cached: it backs `base_url`, which every
/// status compute calls, and probing it opens a socket each time.
fn local_ip() -> Option<String> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<String>)>>> = OnceLock::new();
    let slot = CACHE.get_or_init(|| Mutex::new(None));
    cached_probe(slot, || {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("8.8.8.8:80").ok()?;
        socket.local_addr().ok().map(|addr| addr.ip().to_string())
    })
}

// ---------------------------------------------------------------------------
// Completion listings (best-effort, like the Tauri server)
// ---------------------------------------------------------------------------

/// JSON directory listing matching `token`: `{"t":"ls","items":[["name",isDir],…]}`.
pub fn listing_json(cwd: Option<&str>, token: &str) -> String {
    let items: Vec<serde_json::Value> = list_entries(cwd, token)
        .into_iter()
        .map(|(name, is_dir)| serde_json::json!([name, is_dir]))
        .collect();
    serde_json::json!({ "t": "ls", "items": items }).to_string()
}

fn list_entries(cwd: Option<&str>, token: &str) -> Vec<(String, bool)> {
    let cwd = match cwd {
        Some(dir) if !dir.is_empty() => dir,
        _ => return Vec::new(),
    };
    let token = crate::backend::files::expand_tilde(token).to_string_lossy().into_owned();
    #[cfg(windows)]
    let token = token.replace('\\', "/");
    let (dir_part, prefix) = match token.rsplit_once('/') {
        // Keep the separator: / and C:/ must remain roots, not relative paths.
        Some((dir, prefix)) => (format!("{dir}/"), prefix.to_string()),
        None => (String::new(), token.clone()),
    };
    let dir_path = {
        let path = std::path::Path::new(&dir_part);
        if dir_part.is_empty() {
            std::path::PathBuf::from(cwd)
        } else if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(cwd).join(path)
        }
    };
    let Ok(entries) = std::fs::read_dir(&dir_path) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<(String, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !show_hidden {
            continue;
        }
        if !prefix.is_empty() && !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((name, is_dir));
        if out.len() >= 200 {
            break;
        }
    }
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out.truncate(60);
    out
}

// ---------------------------------------------------------------------------
// Mobile client page
// ---------------------------------------------------------------------------

/// The mobile / browser client served at `/`.
///
/// It is the full remote-control UI: it reads the one-time pairing secret from
/// the URL fragment, performs the sealed pairing handshake, derives the session
/// key with `SHA-512(device_key ‖ nc ‖ ns)[..32]`, and then drives the terminal
/// over NaCl secretbox (XSalsa20-Poly1305) frames. xterm.js renders the grid and
/// tweetnacl provides the cipher; both are loaded from jsDelivr with SRI, the
/// same way the Tauri app served this page.
const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no"/>
<title>SpritX Remote</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css" integrity="sha384-tStR1zLfWgsiXCF3IgfB3lBa8KmBe/lG287CL9WCeKgQYcp1bjb4/+mwN6oti4Co" crossorigin="anonymous"/>
<style>
  html,body{margin:0;height:100%;background:#0e1014;overflow:hidden;overscroll-behavior:none;font:13px system-ui,sans-serif;-webkit-tap-highlight-color:transparent;-webkit-touch-callout:none}
  #app{position:fixed;left:0;top:0;right:0;bottom:0;display:flex;flex-direction:column;overflow:hidden}
  #bar{flex:0 0 34px;display:flex;align-items:center;justify-content:space-between;gap:8px;font-size:12px;color:#fff;background:#1b2330;padding:env(safe-area-inset-top, 0px) 10px 0 10px}
  #msg{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
  #dc{background:#3a2330;color:#ff9bb0;border:1px solid #6b3347;border-radius:5px;padding:3px 9px;font-size:12px;touch-action:manipulation;user-select:none;-webkit-user-select:none}
  /* --- messaging-style terminal picker (shown on connect) --- */
  #pick{position:absolute;left:0;top:0;right:0;bottom:0;z-index:30;background:#0e1014;display:none;flex-direction:column;padding-top:env(safe-area-inset-top, 0px);padding-bottom:env(safe-area-inset-bottom, 0px)}
  #pick.show{display:flex}
  #pick-head{flex:0 0 auto;display:flex;align-items:center;gap:10px;padding:12px 14px;border-bottom:1px solid #1d2530}
  #pick-title{flex:1 1 auto;font-size:16px;font-weight:600;color:#e8f2ff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  #pick-back{flex:0 0 auto;background:#1a2230;color:#cbd5e1;border:1px solid #283242;border-radius:7px;width:32px;height:32px;font-size:15px;line-height:1;touch-action:manipulation}
  #pick-new{background:#1a2230;color:#4ea1ff;border:1px solid #283242;border-radius:7px;width:32px;height:32px;font-size:18px;font-weight:700;line-height:1;touch-action:manipulation}
  #pick-list{flex:1 1 auto;overflow-y:auto;-webkit-overflow-scrolling:touch;overscroll-behavior:contain}
  .pick-item{display:flex;align-items:center;gap:12px;padding:12px 14px;border-bottom:1px solid #161d27;touch-action:manipulation;user-select:none;-webkit-user-select:none}
  .pick-item:active{background:#141b25}
  .pick-item.on{background:#111b2b}
  .pick-av{flex:0 0 auto;width:40px;height:40px;border-radius:50%;background:#1b2635;color:#8ab4ff;display:flex;align-items:center;justify-content:center;font-size:15px;font-weight:600;border:1px solid #28384d}
  .pick-item.on .pick-av{background:#16243a;color:#e8f2ff;border-color:#4ea1ff}
  .pick-item.add .pick-av{background:transparent;border-style:dashed;color:#4ea1ff}
  .pick-main{flex:1 1 auto;min-width:0}
  .pick-name{font-size:14px;color:#e6e6e6;font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  .pick-sub{font-size:11.5px;color:#7f8b9c;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  .pick-item.on .pick-sub{color:#56d364}
  .pick-empty{padding:22px 14px;color:#5e6a7a;font-size:13px;text-align:center}
  #pick-foot{flex:0 0 auto;padding:8px 14px;font-size:11.5px;color:#5e6a7a;border-top:1px solid #161d27}
  #back,#fdec,#finc{background:#1a2230;color:#cbd5e1;border:1px solid #283242;border-radius:5px;padding:4px 9px;font-size:14px;line-height:1;touch-action:manipulation;user-select:none;-webkit-user-select:none}
  #dim{flex:0 0 auto;font-size:11px;color:#7f8b9c;font-variant-numeric:tabular-nums;padding:0 5px}
  #msg{flex:1 1 auto;min-width:0}
  #term{flex:1 1 auto;min-height:0;padding:2px 0 2px 2px;overflow:hidden;touch-action:none}
  .xterm{height:100%}
  .xterm .xterm-screen{width:100% !important}
  .xterm .xterm-viewport{overflow-y:auto !important;scrollbar-width:none;-ms-overflow-style:none}
  .xterm .xterm-viewport::-webkit-scrollbar{display:none;width:0;height:0}
  #assist{flex:0 0 auto;background:#141a24;border-top:1px solid #232c3a;padding-bottom:env(safe-area-inset-bottom, 0px)}
  #chips{display:flex;gap:6px;overflow-x:auto;padding:5px 6px;white-space:nowrap;-webkit-overflow-scrolling:touch}
  #chips:empty{display:none}
  .chip{flex:0 0 auto;background:#1f2937;color:#cbd5e1;border:1px solid #2b3647;border-radius:6px;padding:5px 9px;font-size:13px;font-family:ui-monospace,monospace;touch-action:manipulation;user-select:none;-webkit-user-select:none}
  .chip.d{color:#8ab4ff;border-color:#33405a}
  #keys{display:flex;gap:5px;overflow-x:auto;padding:5px 6px;white-space:nowrap;-webkit-overflow-scrolling:touch}
  .key{flex:0 0 auto;background:#222b39;color:#e6e6e6;border:1px solid #2f3a4c;border-radius:6px;padding:7px 11px;font-size:13px;min-width:30px;text-align:center;user-select:none;-webkit-user-select:none;touch-action:manipulation}
  .key:active{background:#2d3a4d}
  .key.on{background:#2b6cff;border-color:#2b6cff;color:#fff}
  #overlay{display:none;position:fixed;inset:0;z-index:50;background:rgba(8,10,14,0.93);align-items:center;justify-content:center;padding:26px}
  #overlay .box{max-width:330px;text-align:center;color:#e6e6e6}
  #ov-icon{margin-bottom:10px;color:#ffb454}
  #ov-icon svg{width:42px;height:42px}
  .dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:#56d364;margin-right:6px;vertical-align:1px}
  #ov-title{font-size:19px;font-weight:600;margin-bottom:6px}
  #ov-sub{font-size:13px;color:#9aa4b2;margin-bottom:20px;line-height:1.45}
  #reconnect{background:#101a2b;color:#e8f2ff;border:1px solid #4ea1ff;border-radius:10px;padding:12px 26px;font-size:15px;font-weight:600;letter-spacing:.02em;touch-action:manipulation;user-select:none;-webkit-user-select:none;box-shadow:0 0 0 1px rgba(78,161,255,.25), 0 0 20px rgba(78,161,255,.45), inset 0 0 14px rgba(78,161,255,.12);animation:rcGlow 2.4s ease-in-out infinite;transition:background .15s}
  #reconnect:active{background:#16243a;animation:none;box-shadow:0 0 0 1px rgba(78,161,255,.55), 0 0 30px rgba(78,161,255,.7), inset 0 0 16px rgba(78,161,255,.2)}
  @keyframes rcGlow{0%,100%{box-shadow:0 0 0 1px rgba(78,161,255,.22), 0 0 14px rgba(78,161,255,.32), inset 0 0 12px rgba(78,161,255,.10)}50%{box-shadow:0 0 0 1px rgba(78,161,255,.4), 0 0 28px rgba(78,161,255,.62), inset 0 0 16px rgba(78,161,255,.16)}}
</style>
</head>
<body>
<div id="app">
  <div id="bar"><button id="back" title="Terminals" style="display:none">&#9776;</button><span id="msg">Connecting…</span><span id="dim"></span><button id="fdec" title="Smaller text (more columns)">A&#8722;</button><button id="finc" title="Bigger text">A+</button><button id="dc" style="display:none">Disconnect</button></div>
  <div id="term"></div>
  <div id="assist"><div id="chips"></div><div id="keys"></div></div>
  <div id="pick"><div id="pick-head"><button id="pick-back" title="Back" style="display:none">&#8592;</button><span id="pick-title">Terminals</span><button id="pick-new" title="New terminal">+</button></div><div id="pick-list"></div><div id="pick-foot"></div></div>
</div>
<div id="overlay"><div class="box"><div id="ov-icon"></div><div id="ov-title">Connection lost</div><div id="ov-sub"></div><button id="reconnect">Reconnect</button></div></div>
<script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js" integrity="sha384-J4qzUjBl1FxyLsl/kQPQIOeINsmp17OHYXDOMpMxlKX53ZfYsL+aWHpgArvOuof9" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js" integrity="sha384-XGqKrV8Jrukp1NITJbOEHwg01tNkuXr6uB6YEj69ebpYU3v7FvoGgEg23C1Gcehk" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/@xterm/addon-webgl@0.19.0/lib/addon-webgl.min.js" integrity="sha384-XZQCtld4fwfpp4rw06rg+kVzIfVqv4G0UG7Em8oCawdlHA4L92O9pA7fJrDpLNG3" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/tweetnacl@1.0.3/nacl-fast.min.js" integrity="sha384-05+sicyRJQ56XpL4U9HJ8YbtSzFDvAg7apPKOGV6A0JsAJKFM68jp5oLnUjG5mEp" crossorigin="anonymous"></script>
<script>
(function(){
  var $=function(id){return document.getElementById(id);};
  // --- end-to-end encryption (see remote_proto.rs for the protocol) ---
  // The pairing secret travels in the URL fragment, which browsers never send
  // to the server (nor to the tunnel). Once paired, this device's own key
  // lives in localStorage, per server id.
  // A refused handshake stops this page: it will not fall back to plaintext.
  // A silently unencrypted shell is worse than a dead one.
  var CREDS_KEY='spritx.remote.v2';
  var pairSecret=null;
  (function(){ var m=/[#&]p=([A-Za-z0-9_-]+)/.exec(location.hash); if(m){ pairSecret=b64urld(m[1]); } })();
  function b64e(u8){ var s=''; for(var i=0;i<u8.length;i++) s+=String.fromCharCode(u8[i]); return btoa(s); }
  function b64d(str){ var s=atob(str), u=new Uint8Array(s.length); for(var i=0;i<s.length;i++) u[i]=s.charCodeAt(i); return u; }
  function b64urld(str){ str=str.replace(/-/g,'+').replace(/_/g,'/'); while(str.length%4) str+='='; return b64d(str); }
  function cat(){ var n=0,i; for(i=0;i<arguments.length;i++) n+=arguments[i].length; var o=new Uint8Array(n), k=0; for(i=0;i<arguments.length;i++){ o.set(arguments[i],k); k+=arguments[i].length; } return o; }
  function loadCreds(){ try{ return JSON.parse(localStorage.getItem(CREDS_KEY)||'{}')||{}; }catch(e){ return {}; } }
  function saveCreds(c){ try{ localStorage.setItem(CREDS_KEY, JSON.stringify(c)); }catch(e){} }
  function hexId(){ var b=nacl.randomBytes(12), s=''; for(var i=0;i<b.length;i++) s+=('0'+b[i].toString(16)).slice(-2); return s; }
  function deviceName(){ var u=navigator.userAgent; return /iPhone/.test(u)?'iPhone':/iPad/.test(u)?'iPad':/Android/.test(u)?'Android':/Mac/.test(u)?'Mac':/Windows/.test(u)?'Windows':/Linux/.test(u)?'Linux':'Browser'; }
  function nonce(dir, ctr){ var n=new Uint8Array(24), hi=Math.floor(ctr/4294967296), lo=ctr>>>0; n[0]=dir; n[16]=hi>>>24&255; n[17]=hi>>>16&255; n[18]=hi>>>8&255; n[19]=hi&255; n[20]=lo>>>24&255; n[21]=lo>>>16&255; n[22]=lo>>>8&255; n[23]=lo&255; return n; }
  function Half(key, dir){ this.key=key; this.dir=dir; this.ctr=0; }
  Half.prototype.seal=function(m){ var b=nacl.secretbox(m, nonce(this.dir,this.ctr), this.key); this.ctr++; return b; };
  Half.prototype.open=function(b){ var m=nacl.secretbox.open(b, nonce(this.dir,this.ctr), this.key); if(!m) return null; this.ctr++; return m; };
  var enc=new TextEncoder(), dec=new TextDecoder();
  var txh=null, rxh=null, ready=false;
  // Every client→server frame after the handshake is sealed.
  function wsSend(data){ if(!ws || ws.readyState!==1 || !ready) return; var bytes = typeof data==='string' ? enc.encode(data) : data; ws.send(txh.seal(bytes)); }
  var msg=$('msg'), dc=$('dc'), termEl=$('term'), app=$('app'), chipsEl=$('chips'), keysEl=$('keys');
  var pickEl=$('pick'), pickList=$('pick-list'), pickFoot=$('pick-foot'), backBtn=$('back');
  var pickBack=$('pick-back'), pickTitle=$('pick-title');
  var pickItems=[], pickActive=null, pickView='tabs', pickTab=null, pickTabName='';
  var lastActive=null;
  var closedByUser=false;
  var pingTimer=null;
  var small = Math.min(window.innerWidth, window.innerHeight) < 480;
  // Font size the user picked (the phone default is small so more columns —
  // and therefore wide CLI tools — fit without wrapping).
  var FONT_KEY='spritx.remote.font';
  var baseFont = (function(){ var v=parseFloat(localStorage.getItem(FONT_KEY)); return (v>=6 && v<=24) ? v : (small?11:13); })();
  var term = new Terminal({ cursorBlink:true, fontSize: baseFont, fontFamily:'ui-monospace,monospace', scrollback:2000, theme:{background:'#0e1014',foreground:'#e6e6e6'} });
  var fit = new FitAddon.FitAddon();
  term.loadAddon(fit); term.open(termEl);
  // The default renderer redraws the grid through the DOM: a fullscreen TUI
  // that repaints on every wheel notch (which is exactly what a finger swipe
  // produces) then costs the browser thousands of layout/style operations per
  // frame, and on a phone that is the difference between a smooth scroll and a
  // stuck one. WebGL draws the same grid on the GPU. Devices without WebGL2 —
  // and a context that is later lost (backgrounded tab, driver reset) — keep or
  // get back the default renderer, so this is an upgrade, never a dependency.
  try{
    if(typeof WebglAddon !== 'undefined'){
      var gl = new WebglAddon.WebglAddon();
      // Disposing on loss makes xterm build its default renderer again.
      gl.onContextLoss(function(){ try{ gl.dispose(); }catch(_){} });
      term.loadAddon(gl);
    }
  }catch(_){}
  // A phone has no hardware keyboard, so focusing the terminal raises the
  // software one, which then covers the pane until focus goes elsewhere. A
  // desktop wants the opposite: click-to-focus is how you type. Same code, two
  // defaults — `touchUI` decides which one, and taps inside the app are judged
  // against the caret (see `onCaretLine`).
  var touchUI = !!(window.matchMedia && window.matchMedia('(hover: none)').matches);
  function focusTerm(){ term.focus(); if(term.textarea) term.textarea.focus(); }
  function blurTerm(){ if(term.textarea && document.activeElement === term.textarea) term.textarea.blur(); }
  // Whether the running app owns the pointer. The desktop puts the session's
  // modes back at the head of the replay it sends on attach (see set_modes in
  // remote_server.rs), so this parser knows about them even though the app set
  // them before this page ever connected.
  function mouseActive(){
    try{ return term.modes.mouseTrackingMode!=='none'; }catch(_){ return false; }
  }
  // Whether a touch at `clientY` landed on the app's input line — the row it
  // put the caret on. An app that owns the pointer draws its own input box
  // around that caret (border, input, border), so there the row above and below
  // count as well; without one (a shell prompt) everything from the caret's line
  // down is where typing goes. Any other tap is a click on the app's UI, and
  // must not raise the keyboard: the panel it opens would be buried under it.
  function onCaretLine(clientY){
    var cell = cellMetrics();
    var el = term.element || termEl;
    if(!cell || !el) return true;          // no metrics yet: behave as before
    var b = term.buffer.active;
    var row = Math.floor((clientY - el.getBoundingClientRect().top) / cell.height);
    // The caret's row in the viewport. A caret scrolled off the bottom pins to
    // the last row rather than leaving no tappable line at all.
    var caret = b.cursorY - b.baseY;
    caret = Math.max(0, Math.min(term.rows - 1, caret));
    return mouseActive() ? Math.abs(row - caret) <= 1 : row >= caret - 1;
  }
  // Fit immediately (before the first paint) and again on the next frame, so a
  // fresh load/refresh already shows the right size — no A−/A+ tap needed.
  layout();
  requestAnimationFrame(function(){ layout(); });
  var proto = location.protocol === 'https:' ? 'wss' : 'ws';
  var ws;

  // --- line tracking (best-effort, for path completion) ---
  var curLine='';
  function track(s){
    // Walk char-by-char; on ESC skip the full CSI/OSC sequence instead of
    // bailing on the entire string — arrow keys from the key bar or horizontal
    // swipes would otherwise freeze curLine for the rest of the session.
    for(var i=0;i<s.length;i++){
      var code=s.charCodeAt(i);
      if(code===27){                              // ESC — skip escape sequence
        i++;                                      // consume '[', ']', 'O', etc.
        if(i<s.length && s.charCodeAt(i)===93){  // OSC: skip until ST or BEL
          while(i<s.length && s.charCodeAt(i)!==7 && !(s.charCodeAt(i)===92 && i>0 && s.charCodeAt(i-1)===27)) i++;
        } else {                                  // CSI / SS3: skip until letter
          while(i<s.length-1){ var ec=s.charCodeAt(i+1); if((ec>=64&&ec<=90)||(ec>=97&&ec<=122)) break; i++; }
          i++;                                    // consume the final letter
        }
        continue;
      }
      if(code===13||code===10){ curLine=''; }                    // Enter
      else if(code===127||code===8){ curLine=curLine.slice(0,-1); } // backspace
      else if(code===3||code===21){ curLine=''; }                // ^C / ^U
      else if(code>=32){ curLine+=s[i]; }
    }
  }
  // Split on unescaped whitespace so paths like "my\ file" stay whole.
  function curToken(){
    var esc=curLine.replace(/\\ /g,'\x01');     // temporarily hide escaped spaces
    if(esc.indexOf(' ')<0) return null;
    var p=esc.split(/ +/);
    if(p.length<2) return null;
    return p[p.length-1].replace(/\x01/g,'\\ '); // restore them
  }

  // --- send helper (used by keyboard + buttons) ---
  function send(s){ wsSend(s); track(s); scheduleComplete(); }

  // --- completion chips ---
  var ct; function scheduleComplete(){ clearTimeout(ct); ct=setTimeout(reqComplete,140); }
  function reqComplete(){ var t=curToken(); if(t===null){ chipsEl.innerHTML=''; chipsEl.style.display='none'; relayout(); return; } wsSend(String.fromCharCode(0,2)+t); }
  function renderChips(items){
    chipsEl.innerHTML='';
    if(!items || !items.length){ chipsEl.style.display='none'; relayout(); return; }
    chipsEl.style.display='flex';
    // #assist is flex:0 0 auto and #term flex:1 1 auto: the chips row takes its
    // height off the terminal's box, so the grid has to be re-fitted to it —
    // otherwise the bottom rows (a TUI's prompt) drop out of the clipped box.
    relayout();
    var t=curToken(); var base = t===null?'':t.split('/').pop();
    items.forEach(function(it){
      var name=it[0], isDir=it[1];
      var c=document.createElement('div'); c.className='chip'+(isDir?' d':''); c.textContent=name+(isDir?'/':'');
      bindTap(c, function(){
        var suffix = name.indexOf(base)===0 ? name.slice(base.length) : name;
        suffix = suffix.replace(/([ \t\n\r"'`$\\\(\)\[\]\{\}<>;*?#&|!~])/g, '\\$1');
        if(isDir) suffix += '/';
        send(suffix);
        // Completing a path is an explicit "I am typing" action, so the
        // keyboard belongs up here (and on iOS the hidden textarea has to be
        // focused directly for it to stay up).
        focusTerm();
      });
      chipsEl.appendChild(c);
    });
  }

  // --- tab list from the desktop (drives the picker; the old strip is gone) ---
  function onTabs(items, active){
    if(active!==lastActive){
      // clear() wipes scrollback so old pane history does not bleed through
      // when the user scrolls up after a tab switch.
      try{term.clear();}catch(e){}
      try{term.reset();}catch(e){}
      curLine=''; chipsEl.innerHTML=''; lastActive=active;
    }
    pickItems = items||[]; pickActive = active; renderPicker();
  }

  // --- messaging-style terminal picker (the landing screen) ---
  // Two levels: tabs first, then the terminals inside the tapped tab.
  function openPicker(){ pickView='tabs'; pickEl.classList.add('show'); backBtn.style.display='none'; renderPicker(); }
  function closePicker(){ pickEl.classList.remove('show'); backBtn.style.display=''; }
  function groupTabs(){
    var g=[], map={};
    pickItems.forEach(function(it){
      var key = (it.tabId!=null ? ('t'+it.tabId) : (it.tab || it.title || 'Terminal'));
      if(!map[key]){ map[key]={ key:key, name: it.tab || it.title || 'Terminal', panes:[] }; g.push(map[key]); }
      map[key].panes.push(it);
    });
    return g;
  }
  function makeRow(avText, name, sub, onRow){
    var row=document.createElement('div'); row.className='pick-item'+(onRow&&onRow.on?' on':'');
    var av=document.createElement('div'); av.className='pick-av'; av.textContent=avText;
    var main=document.createElement('div'); main.className='pick-main';
    var nm=document.createElement('div'); nm.className='pick-name'; nm.textContent=name;
    var sb=document.createElement('div'); sb.className='pick-sub'; sb.textContent=sub;
    main.appendChild(nm); main.appendChild(sb);
    row.appendChild(av); row.appendChild(main);
    if(onRow) bindTap(row, onRow.fn);
    return row;
  }
  function renderPicker(){
    pickTitle.textContent = pickView==='panes' ? pickTabName : 'Terminals';
    pickBack.style.display = pickView==='panes' ? '' : 'none';
    pickList.innerHTML='';
    if(pickView==='panes') renderPanes(); else renderTabList();
  }
  function renderTabList(){
    var groups = groupTabs();
    if(!groups.length){
      var e=document.createElement('div'); e.className='pick-empty'; e.textContent='No terminal yet…';
      pickList.appendChild(e);
    }
    groups.forEach(function(gr, i){
      var live = gr.panes.some(function(p){ return p.id===pickActive; });
      var n = gr.panes.length;
      pickList.appendChild(makeRow(String(i+1), gr.name,
        live ? '● live now' : (n + (n===1?' terminal':' terminals')),
        { on: live, fn: function(){ openTabGroup(gr); } }));
    });
    var add=makeRow('+','New terminal','start a fresh shell',{ fn: function(){ wsSend(String.fromCharCode(0,4)); closePicker(); pickTerminalFocus(); } });
    add.className='pick-item add';
    pickList.appendChild(add);
    pickFoot.textContent = groups.length ? (groups.length + (groups.length===1?' tab':' tabs') + ' · ' + pickItems.length + ' terminal' + (pickItems.length===1?'':'s')) : '';
  }
  function renderPanes(){
    var gr = groupTabs().filter(function(g){ return g.key===pickTab; })[0];
    var panes = gr ? gr.panes : [];
    panes.forEach(function(it, i){
      var live = it.id===pickActive;
      pickList.appendChild(makeRow(String(i+1), 'Terminal ' + (i+1),
        live ? '● live now' : 'tap to open',
        { on: live, fn: function(){ chooseTerminal(it.id); } }));
    });
    pickFoot.textContent = panes.length + (panes.length===1?' terminal':' terminals');
  }
  function pickTerminalFocus(){
    // Picking a terminal is not "I want to type": on a phone the keyboard would
    // cover the pane that was just opened. It comes up on an input-box tap (or
    // any key) instead.
    if(!touchUI) focusTerm();
  }
  function openTabGroup(gr){
    if(gr.panes.length===1){ chooseTerminal(gr.panes[0].id); return; }
    pickView='panes'; pickTab=gr.key; pickTabName=gr.name; renderPicker();
  }
  function chooseTerminal(id){
    if(id==null) return;
    // Use string coercion for the comparison: JSON-parsed u64 IDs and the
    // stored pickActive are both numbers but may differ in type after null
    // transitions; String() comparison is safe for all values including null.
    if(String(id)!==String(pickActive)){ pickActive=id; wsSend(String.fromCharCode(0,3)+id); }
    closePicker(); pickTerminalFocus(); renderPicker(); settle();
  }

  // --- extra keys (Termux-style) ---
  var ctrlPending=false, ctrlBtn=null;
  var KEYS=[['Esc','\x1b'],['Tab','\t'],['Ctrl','CTRL'],['←','\x1b[D'],['↑','\x1b[A'],['↓','\x1b[B'],['→','\x1b[C'],['Home','\x1b[H'],['End','\x1b[F'],['|','|'],['~','~'],['/','/'],['-','-'],['_','_'],['^C','\x03'],['^L','\x0c'],['^D','\x04']];
  // Trigger on a tap, but let a drag scroll the row. touchstart/move stay
  // passive (so native scroll works); only a no-move touchend fires the action
  // (and preventDefault keeps the keyboard open). A recent touch also blocks
  // the synthesized mouse events browsers send right after a tap, without
  // permanently disabling mouse clicks on hybrid devices.
  function bindTap(el, fn){
    var sx=0, sy=0, moved=false, lastTouch=0;
    el.addEventListener('touchstart', function(e){
      if(e.touches.length!==1){ moved=true; return; }
      lastTouch=Date.now(); var t=e.touches[0]; sx=t.clientX; sy=t.clientY; moved=false;
    }, {passive:true});
    el.addEventListener('touchmove', function(e){
      if(e.touches.length!==1){ moved=true; return; }
      var t=e.touches[0]; if(Math.abs(t.clientX-sx)>8 || Math.abs(t.clientY-sy)>8) moved=true;
    }, {passive:true});
    el.addEventListener('touchend', function(e){ if(!moved){ e.preventDefault(); fn(); } }, {passive:false});
    el.addEventListener('mousedown', function(e){ if(Date.now()-lastTouch<700) return; e.preventDefault(); fn(); });
  }
  KEYS.forEach(function(k){
    var b=document.createElement('div'); b.className='key'; b.textContent=k[0];
    if(k[1]==='CTRL'){ ctrlBtn=b; bindTap(b,function(){ ctrlPending=!ctrlPending; b.classList.toggle('on',ctrlPending); }); }
    else bindTap(b,function(){ send(k[1]); focusTerm(); });
    keysEl.appendChild(b);
  });

  // The desktop answers device queries using the host's cursor. Replaying the
  // same bytes in a browser must not inject a second answer into the shell.
  term.parser.registerCsiHandler({final:'n'},function(p){ return p[0]===5 || p[0]===6; });
  term.parser.registerCsiHandler({prefix:'?',final:'n'},function(p){ return p[0]===6; });
  term.parser.registerCsiHandler({final:'c'},function(p){ return !p.length || p[0]===0; });
  term.parser.registerCsiHandler({prefix:'>',final:'c'},function(p){ return !p.length || p[0]===0; });

  term.onData(function(d){
    if(ctrlPending){
      if(d.length===1){
        var code=d.toUpperCase().charCodeAt(0);
        if(code>=64 && code<=95) d=String.fromCharCode(code & 0x1f);
      }
      ctrlPending=false; if(ctrlBtn) ctrlBtn.classList.remove('on');
    }
    send(d);
  });

  // Touch on the terminal. xterm.js only wires its own touch scrolling when the
  // running app has NOT enabled mouse reporting; in mouse-reporting mode (e.g.
  // Pi fullscreen) it leaves touch to us, which is why a finger swipe did
  // nothing. There a wheel event routed to the terminal is the one path that
  // does the right thing (a wheel report to the app, or arrow keys on the
  // alternate screen), so a vertical swipe synthesizes wheel events and a tap
  // synthesizes a left click — with no app owning the mouse, xterm's own
  // scrolling keeps the gesture and we stay out of its way. Horizontal swipes
  // still move the cursor with ←/→. Decide the axis on the first move, then
  // lock it; `mouseActive()` below says whether the app owns the pointer. A
  // stationary tap decides the software keyboard either way — up only on the
  // app's input line (see `onCaretLine`), down for a click on its own UI.
  (function(){
    var x0=0,y0=0,lastX=0,lastY=0,moved=false,mode=0,STEP=16;   // mode: 0=undecided,1=horizontal,2=vertical,-1=multi-touch
    function termTarget(){ return term.element || termEl; }
    function synthWheel(x,y,dy){
      try{ termTarget().dispatchEvent(new WheelEvent('wheel',{bubbles:true,cancelable:true,view:window,clientX:x,clientY:y,deltaY:dy,deltaMode:WheelEvent.DOM_DELTA_PIXEL})); }catch(_){}
    }
    function synthClick(x,y){
      var el=termTarget();
      try{ el.dispatchEvent(new MouseEvent('mousedown',{bubbles:true,cancelable:true,view:window,clientX:x,clientY:y,button:0,buttons:1})); }catch(_){}
      try{ el.dispatchEvent(new MouseEvent('mouseup',{bubbles:true,cancelable:true,view:window,clientX:x,clientY:y,button:0,buttons:0})); }catch(_){}
    }
    // Capture phase: with no app owning the mouse xterm cancels the touch
    // (stopPropagation) for its own scrolling, which would otherwise keep these
    // listeners from ever seeing a tap.
    termEl.addEventListener('touchstart',function(e){
      if(e.touches.length!==1){ mode=-1; return; }
      var t=e.touches[0]; x0=t.clientX; y0=t.clientY; lastX=t.clientX; lastY=t.clientY; moved=false; mode=0;
    },{passive:true,capture:true});
    termEl.addEventListener('touchmove',function(e){
      if(mode===-1 || e.touches.length!==1) return;
      var t=e.touches[0], dx=t.clientX-x0, dy=t.clientY-y0;
      if(mode===0){ if(Math.abs(dx)>10 && Math.abs(dx)>Math.abs(dy)*1.3){ mode=1; lastX=t.clientX; } else if(Math.abs(dy)>10){ mode=2; lastY=t.clientY; } }
      if(mode===1){
        e.preventDefault(); moved=true;
        var d=t.clientX-lastX;
        // Use send() (not wsSend()) so curLine is updated on every
        // horizontal swipe arrow — otherwise completion state drifts.
        while(d>=STEP){ send('\x1b[C'); lastX+=STEP; d-=STEP; }
        while(d<=-STEP){ send('\x1b[D'); lastX-=STEP; d+=STEP; }
      } else if(mode===2){
        moved=true;
        // When the app has not taken the mouse, xterm's own touch handling
        // scrolls the viewport (and does it with the finger exactly), so we
        // leave the event alone — driving the wheel too would move it twice.
        // When the app *has* taken it, xterm handles no touch at all, and a
        // wheel event routed to the terminal is the one path that does the
        // right thing: a wheel report to the app, or arrow keys on the
        // alternate screen. Finger-up means newer output, matching a
        // wheel-down (deltaY>0), and the mapping is one notch per cell of the
        // finger's own travel: no momentum, because the app acts on every
        // notch and a fling would keep taking those actions after the finger
        // is gone.
        if(mouseActive()){
          e.preventDefault();
          var w=t.clientY-lastY; lastY=t.clientY;
          if(w!==0) synthWheel(t.clientX,t.clientY,-w);
        }
      }
    },{passive:false});
    termEl.addEventListener('touchend',function(e){
      if(mode===-1){ mode=0; return; }
      var tap = mode===0 && !moved && e.touches.length===0 && e.changedTouches.length===1;
      if(!tap){ mode=0; return; }
      // A stationary tap opens links / activates the app's own controls while
      // the app owns the mouse. preventDefault suppresses the delayed synthetic
      // mouse events browsers send after a tap, so it is not reported twice.
      // The click lands where the finger went *down*: a rolling tap moves a few
      // pixels before it lifts, and the point it happened to lift over is a cell
      // away often enough to hit the wrong control.
      if(mouseActive()){
        e.preventDefault();
        synthClick(x0,y0);
      }
      // Either way the tap decides the keyboard: on the input line it means "I
      // want to type", anywhere else it is a click on the app's UI (or a look at
      // output) and the keyboard has to get out of the way.
      if(onCaretLine(y0)) focusTerm(); else blurTerm();
      mode=0;
    },{passive:false,capture:true});
  })();

  var overlay=$('overlay'), ovTitle=$('ov-title'), ovSub=$('ov-sub'), ovIcon=$('ov-icon');
  var WARN_SVG='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>';
  var PLUG_SVG='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v6M15 2v6"/><path d="M6 8h12v3a6 6 0 0 1-6 6 6 6 0 0 1-6-6z"/><line x1="12" y1="17" x2="12" y2="22"/></svg>';
  // kind: 'user' (Disconnect), 'lost' (network), or a {title, sub, retry} object.
  function showOverlay(kind){
    var o = kind==='user' ? {icon:PLUG_SVG, title:'Disconnected', sub:"You are disconnected from the remote terminal.", retry:true}
          : kind==='lost' ? {icon:WARN_SVG, title:'Connection lost', sub:"The tunnel or the network dropped.", retry:true}
          : kind;
    ovIcon.innerHTML = o.icon || WARN_SVG;
    ovTitle.textContent = o.title; ovSub.textContent = o.sub;
    $('reconnect').style.display = o.retry ? '' : 'none';
    overlay.style.display='flex';
  }
  function hideOverlay(){ overlay.style.display='none'; }
  var fatal=null; // a handshake refusal: shown instead of "connection lost"
  function connect(){
    if(ws){
      try {
        ws.onopen = ws.onclose = ws.onmessage = ws.onerror = null;
        ws.close();
      } catch(_) {}
    }
    clearInterval(pingTimer); pingTimer = null;
    closedByUser=false; fatal=null; ready=false; txh=rxh=null; hideOverlay(); msg.textContent='Connecting…';
    try{ term.reset(); }catch(e){}   // clear so the server's replay rebuilds cleanly
    curLine=''; lastActive=null;
    pickItems=[]; pickActive=null; pickView='tabs'; pickTab=null; pickTabName='';
    measuredOnce = false;
    var serverId=null, deviceKey=null, nc=null;
    ws = new WebSocket(proto + '://' + location.host + '/ws');
    ws.binaryType = 'arraybuffer';
    function hello(){
      var c=loadCreds()[serverId];
      if(!c){ fatal={title:'Device not paired', sub:'Open the pairing link (or scan its QR code) shown in SpritX under the phone icon, then reload this page.', retry:false}; ws.close(); return; }
      deviceKey=b64d(c.key); nc=nacl.randomBytes(24);
      ws.send(JSON.stringify({t:'hello', id:c.id, nc:b64e(nc)}));
    }
    ws.onmessage = function(e){
      if(typeof e.data === 'string'){
        var m; try{ m=JSON.parse(e.data); }catch(_){ return; }
        if(m.t==='server'){
          serverId=m.id; document.title='SpritX — '+(m.name||'Remote');
          if(pairSecret){
            var id=hexId(), key=nacl.randomBytes(32), n=nacl.randomBytes(24);
            var payload=enc.encode(JSON.stringify({id:id, key:b64e(key), name:deviceName()}));
            var all=loadCreds(); all[serverId]={id:id, key:b64e(key), pending:true}; saveCreds(all);
            ws.send(JSON.stringify({t:'pair', n:b64e(n), box:b64e(nacl.secretbox(payload, n, pairSecret))}));
            msg.textContent='Pairing…';
          } else hello();
        } else if(m.t==='paired'){
          pairSecret=null;
          var all=loadCreds(); if(all[serverId]) delete all[serverId].pending; saveCreds(all);
          // Drop the one-time secret from the address bar (and from bookmarks).
          try{ history.replaceState(null,'',location.pathname+location.search); }catch(_){}
          hello();
        } else if(m.t==='challenge'){
          var s=nacl.hash(cat(deviceKey, nc, b64d(m.ns))).subarray(0,32);
          txh=new Half(s,0); rxh=new Half(s,1); ready=true;
          wsSend(new Uint8Array([0,5]));
          msg.innerHTML='<i class="dot"></i>Connected (encrypted)'; dc.style.display=''; settle(); openPicker();
          // Keep the WebSocket alive through Cloudflare's ~100s idle timeout.
          // Browsers cannot send raw WS pings; an empty text frame is ignored
          // by the server's input_task (Text frames are skipped post-handshake)
          // and costs nothing on the PTY side.
          clearInterval(pingTimer);
          pingTimer = setInterval(function(){ if(ws && ws.readyState===1) ws.send(''); }, 30000);
        } else if(m.t==='denied'){
          if(m.reason==='unknown'){ var a=loadCreds(); delete a[serverId]; saveCreds(a); fatal={title:'Device revoked', sub:"This device is no longer authorized. Scan the QR code shown by SpritX to pair it again.", retry:false}; }
          else if(m.reason==='pairing'){ var b=loadCreds(); if(b[serverId] && b[serverId].pending) delete b[serverId]; saveCreds(b); pairSecret=null; fatal={title:'QR code expired', sub:'This QR code expired or was already used. Show a new QR code in SpritX.', retry:false}; }
          else fatal={title:'Connection refused', sub:'SpritX refused the connection.', retry:true};
        }
        return;
      }
      if(!ready) return;
      var plain=rxh.open(new Uint8Array(e.data));
      if(!plain){ fatal={title:'Encryption error', sub:'Invalid frame received — connection closed.', retry:true}; ws.close(); return; }
      if(plain[0]===84){ // 'T'
        try{ var c=JSON.parse(dec.decode(plain.subarray(1))); if(c.t==='ls'){ renderChips(c.items||[]); } else if(c.t==='tabs'){ onTabs(c.items, c.active); } }catch(_){}
        return;
      }
      term.write(plain.subarray(1));
    };
    ws.onclose = function(event){ if(event && event.reason==='Sharing stopped'){ fatal={title:'Sharing stopped', sub:'The desktop stopped remote control.', retry:true}; } clearInterval(pingTimer); pingTimer=null; ready=false; msg.textContent = closedByUser ? 'Disconnected' : fatal ? fatal.title : 'Connection lost'; dc.style.display='none'; showOverlay(fatal || (closedByUser ? 'user' : 'lost')); };
    ws.onerror = function(){ msg.textContent='Connection error'; };
  }
  bindTap(dc, function(){ closedByUser = true; if(ws) ws.close(); });
  bindTap(backBtn, function(){ openPicker(); });
  bindTap(pickBack, function(){ pickView='tabs'; renderPicker(); });
  bindTap($('pick-new'), function(){ wsSend(String.fromCharCode(0,4)); closePicker(); pickTerminalFocus(); });
  function setFont(f){
    baseFont = Math.max(6, Math.min(24, Math.round(f*10)/10));
    term.options.fontSize = baseFont;
    try{ localStorage.setItem(FONT_KEY, String(baseFont)); }catch(e){}
    // Re-fitting recomputes cols and sends the resize, so the PTY widens with a
    // smaller font and wide output stops wrapping. settle() re-fits once xterm
    // has re-measured the new cell size (a single fit can use stale metrics).
    layout(); settle();
  }
  bindTap($('fdec'), function(){ setFont(baseFont-1); });
  bindTap($('finc'), function(){ setFont(baseFont+1); });
  bindTap($('reconnect'), function(){ connect(); });
  connect();

  function sendResize(){ wsSend(String.fromCharCode(0,1)+term.cols+'x'+term.rows); }
  function showDim(){ var d=$('dim'); if(d) d.textContent = term.cols+'×'+term.rows; }
  // The cell size xterm is *painting* with. It rounds the font's measured cell
  // to whole device pixels (11px font at dpr 1.25: a 6.6×14 font cell becomes
  // 8×18 device px = 6.4×14.4 CSS px), so no independent measurement of the
  // font agrees with it: a hidden span reported 6.7×11 for that same grid. Both
  // errors hurt, in opposite directions — too few columns (dead space at the
  // right) and, because the row height was the bigger error, rows reaching past
  // #term's bottom edge, clipped away exactly where a full-screen TUI keeps its
  // prompt. FitAddon sizes from this same field.
  function cellMetrics(){
    try{
      var c = term._core._renderService.dimensions.css.cell;
      if (c && c.width > 0 && c.height > 0) return c;
    }catch(e){}
    return null;
  }
  function layout(){
    var vv = window.visualViewport;
    var h = vv ? vv.height : window.innerHeight, top = vv ? vv.offsetTop : 0;
    app.style.height = h + 'px'; app.style.transform = 'translateY(' + top + 'px)';
    // Size the grid against the box xterm paints into (.xterm fills #term's
    // content box) instead of rebuilding that box from innerWidth/innerHeight:
    // cols/rows then land inside it by construction, so the last row and the
    // last column are always the ones actually on screen.
    var box = (term.element || termEl).getBoundingClientRect();
    var cell = cellMetrics();
    if (cell && box.width > 16 && box.height > 8) {
      var cols = Math.max(2, Math.floor(box.width / cell.width));
      var rows = Math.max(1, Math.floor(box.height / cell.height));
      if (cols !== term.cols || rows !== term.rows) {
        try{ term.resize(cols, rows); }catch(e){ try{ fit.fit(); }catch(_){} }
      }
    } else {
      try{ fit.fit(); }catch(e){}
    }
    sendResize();
    showDim();
  }
  var rt; function relayout(){ clearTimeout(rt); rt=setTimeout(layout,60); }
  // xterm measures its glyph cell once, as early as `open()`, and holds that
  // value until an option change makes it look again — a first measurement
  // taken before the font is ready would size every later fit. Re-fit a few
  // times as the font and the viewport settle. All calls are idempotent.
  function settle(){
    [0,80,220,500,1000,1800].forEach(function(d){ setTimeout(layout, d); });
    // Setting fontFamily/fontSize back to the value they already have is what
    // makes xterm re-measure (this is why tapping A−/A+ used to "fix" the
    // layout); it does so without a visible change.
    if(!measuredOnce){
      measuredOnce = true;
      setTimeout(function(){
        var fs = term.options.fontSize || 12;
        try{ term.options.fontSize = fs + 0.25; term.options.fontSize = fs; }catch(e){}
        layout();
      }, 250);
    }
  }
  var measuredOnce = false;
  window.addEventListener('resize', relayout);
  window.addEventListener('orientationchange', function(){ relayout(); settle(); });
  window.addEventListener('focus', function(){ relayout(); });
  document.addEventListener('visibilitychange', function(){ if(!document.hidden){ relayout(); settle(); } });
  if (window.visualViewport){ window.visualViewport.addEventListener('resize', relayout); window.visualViewport.addEventListener('scroll', relayout); }
  window.addEventListener('load', settle);
  if (document.fonts && document.fonts.ready) { document.fonts.ready.then(function(){ layout(); }).catch(function(){}); }
  settle();
})();
</script>
</body>
</html>"##;
