//! Remote-control backend: pairing, the end-to-end encrypted transport, the
//! persisted device store, and the frame types a server/bridge exchanges.
//!
//! This module is deliberately GPUI- and transport-free. It is byte-compatible
//! with the Tauri app's `remote_proto.rs` / `remote_store.rs` / `remote.rs`:
//!
//! * Pairing uses a single-use 32-byte secret with a 10-minute TTL. The secret
//!   travels only in the QR-code URL fragment; the device seals `{id,key,name}`
//!   under it and the server registers that device.
//! * After the handshake every frame is a NaCl secretbox (XSalsa20-Poly1305)
//!   under `SHA-512(device_key ‖ client_nonce ‖ server_nonce)[..32]`, with
//!   implicit per-direction counter nonces (`Half`). The first client frame is
//!   the two-byte `READY` proof. Replayed/reordered frames fail to open.
//! * Server→client sealed frames are `b'T' + JSON` (control) or `b'B' + bytes`
//!   (terminal output). Client→server sealed frames are raw input with the
//!   historical NUL sentinels for resize / completion / tab switch / new tab.
//!
//! # TODO (intentionally not in this pass)
//!
//! * No `cloudflared` quick-tunnel management.
//! * No axum/HTTP/WebSocket server and no browser client page.
//!
//! The API is shaped so a future server can own a [`DeviceStore`] and a
//! [`ServerHandshake`], feed it the plaintext JSON handshake frames, then use
//! [`Established::tx`] / [`Established::rx`] with [`ServerFrame::seal`] /
//! [`ServerFrame::open`] for the encrypted part. A TCP/WebSocket loop can be
//! implemented entirely on top of this module without changing its types.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context as _, Result};
use base64::{
    engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL},
    Engine as _,
};
use crypto_secretbox::{
    aead::{Aead, KeyInit},
    Key, Nonce, XSalsa20Poly1305,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha512};

// ---------------------------------------------------------------------------
// Transport constants and crypto
// ---------------------------------------------------------------------------

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
/// Default pairing-code lifetime (10 minutes).
pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);

/// Direction bytes mixed into counter nonces so the two halves never reuse a
/// nonce under the shared session key.
pub const CLIENT_TO_SERVER: u8 = 0;
pub const SERVER_TO_CLIENT: u8 = 1;

/// First sealed client frame, proving the device holds its key.
pub const READY: [u8; 2] = [0x00, 0x05];

pub const FRAME_CONTROL: u8 = b'T';
pub const FRAME_OUTPUT: u8 = b'B';

/// Sentinels for the historical in-band client control messages.
pub const SENTINEL_RESIZE: [u8; 2] = [0x00, 0x01];
pub const SENTINEL_COMPLETION: [u8; 2] = [0x00, 0x02];
pub const SENTINEL_SWITCH: [u8; 2] = [0x00, 0x03];
pub const SENTINEL_NEW_TAB: [u8; 2] = [0x00, 0x04];

pub fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("getrandom");
    buf
}

/// NaCl secretbox seal (24-byte nonce).
pub fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], msg: &[u8]) -> Vec<u8> {
    XSalsa20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(nonce), msg)
        .expect("secretbox encrypt")
}

/// NaCl secretbox open; `None` for a forged, replayed or reordered frame.
pub fn open(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], boxed: &[u8]) -> Option<Vec<u8>> {
    XSalsa20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(nonce), boxed)
        .ok()
}

/// `SHA-512(device_key ‖ nc ‖ ns)[..32]`, exactly the original session key.
pub fn session_key(device_key: &[u8; KEY_LEN], nc: &[u8], ns: &[u8]) -> [u8; KEY_LEN] {
    let mut h = Sha512::new();
    h.update(device_key);
    h.update(nc);
    h.update(ns);
    let digest = h.finalize();
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&digest[..KEY_LEN]);
    out
}

fn counter_nonce(dir: u8, ctr: u64) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[0] = dir;
    n[16..].copy_from_slice(&ctr.to_be_bytes());
    n
}

/// One direction of an established channel. Frames must be opened in the order
/// they were sealed; a replayed, dropped or reordered frame fails.
#[derive(Clone, Debug)]
pub struct Half {
    key: [u8; KEY_LEN],
    dir: u8,
    ctr: u64,
}

impl Half {
    pub fn new(key: [u8; KEY_LEN], dir: u8) -> Self {
        Self { key, dir, ctr: 0 }
    }

    pub fn direction(&self) -> u8 {
        self.dir
    }

    pub fn counter(&self) -> u64 {
        self.ctr
    }

    pub fn seal(&mut self, msg: &[u8]) -> Vec<u8> {
        let out = seal(&self.key, &counter_nonce(self.dir, self.ctr), msg);
        self.ctr += 1;
        out
    }

    pub fn open(&mut self, boxed: &[u8]) -> Option<Vec<u8>> {
        let out = open(&self.key, &counter_nonce(self.dir, self.ctr), boxed)?;
        self.ctr += 1;
        Some(out)
    }
}

/// Client-side halves: `(tx, rx)` — `tx` seals with CLIENT_TO_SERVER, `rx`
/// opens SERVER_TO_CLIENT frames.
pub fn client_session(
    device_key: &[u8; KEY_LEN],
    nc: &[u8; NONCE_LEN],
    ns: &[u8; NONCE_LEN],
) -> (Half, Half) {
    let key = session_key(device_key, nc, ns);
    (
        Half::new(key, CLIENT_TO_SERVER),
        Half::new(key, SERVER_TO_CLIENT),
    )
}

/// Server-side halves: `(tx, rx)` — `tx` seals with SERVER_TO_CLIENT, `rx`
/// opens CLIENT_TO_SERVER frames.
pub fn server_session(
    device_key: &[u8; KEY_LEN],
    nc: &[u8; NONCE_LEN],
    ns: &[u8; NONCE_LEN],
) -> (Half, Half) {
    let key = session_key(device_key, nc, ns);
    (
        Half::new(key, SERVER_TO_CLIENT),
        Half::new(key, CLIENT_TO_SERVER),
    )
}

// ---------------------------------------------------------------------------
// Base64 helpers (same alphabets as the original)
// ---------------------------------------------------------------------------

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    B64.decode(s.as_bytes()).ok()
}

pub fn b64url(bytes: &[u8]) -> String {
    B64URL.encode(bytes)
}

pub fn unb64url(s: &str) -> Option<Vec<u8>> {
    B64URL.decode(s.trim_end_matches('=').as_bytes()).ok()
}

pub fn to_array<const N: usize>(v: &[u8]) -> Option<[u8; N]> {
    v.try_into().ok()
}

/// Random device id (24 lowercase hex chars), as the browser client generates.
pub fn new_device_id() -> String {
    random::<12>().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

/// A single-use pairing code. The secret is carried in the QR URL fragment;
/// the caller (server) must drop/replace it as soon as a device pairs, which
/// [`ServerHandshake`] does automatically.
#[derive(Clone, Debug)]
pub struct Pairing {
    secret: [u8; KEY_LEN],
    expires: SystemTime,
}

impl Pairing {
    /// A fresh code valid for [`PAIRING_TTL`].
    pub fn new() -> Self {
        Self::with_ttl(PAIRING_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            secret: random(),
            expires: SystemTime::now() + ttl,
        }
    }

    pub fn secret(&self) -> &[u8; KEY_LEN] {
        &self.secret
    }

    pub fn expires_in(&self) -> Duration {
        self.expires
            .duration_since(SystemTime::now())
            .unwrap_or_default()
    }

    pub fn is_expired(&self) -> bool {
        self.expires_in().is_zero()
    }

    /// URL fragment for the QR code: `#p=<base64url secret>`.
    pub fn fragment(&self) -> String {
        format!("#p={}", b64url(&self.secret))
    }

    /// Full pairing URL, e.g. `http://192.168.1.5:4530/#p=…`.
    pub fn url(&self, base: &str) -> String {
        format!("{}/{}", base.trim_end_matches('/'), self.fragment())
    }
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

/// Decoded pairing payload (server side).
#[derive(Clone, Debug)]
pub struct PairPayload {
    pub id: String,
    pub key: [u8; KEY_LEN],
    pub name: String,
}

/// Sealed pairing request (client side): registers `device_id`/`device_key`
/// under the pairing secret. Returns the JSON text frame.
pub fn pair_request(
    secret: &[u8; KEY_LEN],
    device_id: &str,
    device_key: &[u8; KEY_LEN],
    name: &str,
) -> String {
    let nonce = random::<NONCE_LEN>();
    let payload = serde_json::json!({
        "id": device_id,
        "key": b64(device_key),
        "name": name,
    });
    let boxed = seal(secret, &nonce, payload.to_string().as_bytes());
    serde_json::json!({
        "t": "pair",
        "n": b64(&nonce),
        "box": b64(&boxed),
    })
    .to_string()
}

/// Decode and open a pairing request; `None` for a wrong/expired secret or a
/// malformed payload. Ids are restricted to ASCII alphanumerics (≤ 64 chars).
pub fn open_pair_request(secret: &[u8; KEY_LEN], msg: &Value) -> Option<PairPayload> {
    let nonce = to_array::<NONCE_LEN>(&unb64(msg.get("n")?.as_str()?)?)?;
    let boxed = unb64(msg.get("box")?.as_str()?)?;
    let plain = open(secret, &nonce, &boxed)?;
    let v: Value = serde_json::from_slice(&plain).ok()?;
    let id = sanitize_id(v.get("id")?.as_str()?)?;
    let key = to_array::<KEY_LEN>(&unb64(v.get("key")?.as_str()?)?)?;
    let name = sanitize_name(v.get("name").and_then(Value::as_str).unwrap_or(""));
    Some(PairPayload { id, key, name })
}

fn sanitize_id(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(id.to_string())
}

fn sanitize_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "?".to_string()
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// Control frames
// ---------------------------------------------------------------------------

/// A terminal/pane exposed to remote clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub id: u64,
    pub title: String,
    /// Title of the tab this pane lives in.
    #[serde(default)]
    pub tab: String,
    /// Stable tab id (two tabs may share a title).
    #[serde(default)]
    pub tab_id: u64,
}

/// Plaintext handshake / in-channel JSON control frames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Control {
    /// `server → client` first frame.
    #[serde(rename = "server")]
    Server { v: u8, id: String, name: String },
    /// `client → server` pairing request.
    #[serde(rename = "pair")]
    Pair {
        n: String,
        #[serde(rename = "box")]
        boxed: String,
    },
    /// `server → client` pairing accepted.
    #[serde(rename = "paired")]
    Paired,
    /// `client → server` authentication start.
    #[serde(rename = "hello")]
    Hello { id: String, nc: String },
    /// `server → client` nonce challenge.
    #[serde(rename = "challenge")]
    Challenge { ns: String },
    /// `server → client` refusal.
    #[serde(rename = "denied")]
    Denied { reason: String },
    /// `server → client` pane list inside the encrypted channel.
    #[serde(rename = "tabs")]
    Tabs {
        items: Vec<TabInfo>,
        active: Option<u64>,
    },
    /// `server → client` directory-completion result for the browser client.
    #[serde(rename = "ls")]
    Ls { items: Vec<(String, bool)> },
}

impl Control {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("control frame serializes")
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    pub fn server_hello(id: impl Into<String>, name: impl Into<String>) -> Self {
        Control::Server {
            v: 2,
            id: id.into(),
            name: name.into(),
        }
    }

    pub fn hello_request(device_id: impl Into<String>, nc: &[u8; NONCE_LEN]) -> Self {
        Control::Hello {
            id: device_id.into(),
            nc: b64(nc),
        }
    }

    pub fn paired() -> Self {
        Control::Paired
    }

    pub fn challenge(ns: &[u8; NONCE_LEN]) -> Self {
        Control::Challenge { ns: b64(ns) }
    }

    pub fn denied(reason: impl Into<String>) -> Self {
        Control::Denied {
            reason: reason.into(),
        }
    }

    pub fn tabs(items: Vec<TabInfo>, active: Option<u64>) -> Self {
        Control::Tabs { items, active }
    }
}

/// Denial reasons used by the protocol.
pub mod reason {
    pub const PAIRING: &str = "pairing";
    pub const UNKNOWN: &str = "unknown";
    pub const STORAGE: &str = "storage";
    pub const PROTOCOL: &str = "protocol";
}

/// Client→server frames. The raw-input variants match the historical sentinels
/// so an existing browser client keeps working unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFrame {
    /// Raw terminal input.
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// Path-completion request (word being typed).
    Completion(String),
    SwitchTab(u64),
    NewTab,
}

impl ClientFrame {
    /// Unsealed payload (before secretbox).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientFrame::Input(data) => data.clone(),
            ClientFrame::Resize { cols, rows } => {
                let mut out = SENTINEL_RESIZE.to_vec();
                out.extend_from_slice(format!("{cols}x{rows}").as_bytes());
                out
            }
            ClientFrame::Completion(token) => {
                let mut out = SENTINEL_COMPLETION.to_vec();
                out.extend_from_slice(token.as_bytes());
                out
            }
            ClientFrame::SwitchTab(id) => {
                let mut out = SENTINEL_SWITCH.to_vec();
                out.extend_from_slice(id.to_string().as_bytes());
                out
            }
            ClientFrame::NewTab => SENTINEL_NEW_TAB.to_vec(),
        }
    }

    pub fn decode(data: &[u8]) -> Self {
        if let Some(rest) = data.strip_prefix(&SENTINEL_RESIZE) {
            if let Ok(s) = std::str::from_utf8(rest) {
                if let Some((c, r)) = s.split_once('x') {
                    if let (Ok(cols), Ok(rows)) = (c.parse::<u16>(), r.parse::<u16>()) {
                        return ClientFrame::Resize { cols, rows };
                    }
                }
            }
        }
        if let Some(rest) = data.strip_prefix(&SENTINEL_COMPLETION) {
            return ClientFrame::Completion(String::from_utf8_lossy(rest).into_owned());
        }
        if let Some(rest) = data.strip_prefix(&SENTINEL_SWITCH) {
            if let Ok(id) = String::from_utf8_lossy(rest).parse::<u64>() {
                return ClientFrame::SwitchTab(id);
            }
        }
        if data == SENTINEL_NEW_TAB {
            return ClientFrame::NewTab;
        }
        ClientFrame::Input(data.to_vec())
    }

    pub fn seal(&self, half: &mut Half) -> Vec<u8> {
        half.seal(&self.encode())
    }

    pub fn open(half: &mut Half, boxed: &[u8]) -> Option<Self> {
        half.open(boxed).map(|plain| ClientFrame::decode(&plain))
    }
}

/// Server→client sealed frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerFrame {
    Control(Control),
    /// Terminal output (or a screen snapshot replay).
    Output(Vec<u8>),
}

impl ServerFrame {
    /// Unsealed `kind + payload` bytes.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerFrame::Control(control) => {
                let json = control.to_json();
                let mut out = Vec::with_capacity(json.len() + 1);
                out.push(FRAME_CONTROL);
                out.extend_from_slice(json.as_bytes());
                out
            }
            ServerFrame::Output(bytes) => {
                let mut out = Vec::with_capacity(bytes.len() + 1);
                out.push(FRAME_OUTPUT);
                out.extend_from_slice(bytes);
                out
            }
        }
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        match data.split_first() {
            Some((&FRAME_CONTROL, rest)) => {
                let text = std::str::from_utf8(rest).ok()?;
                Control::from_json(text).map(ServerFrame::Control)
            }
            Some((&FRAME_OUTPUT, rest)) => Some(ServerFrame::Output(rest.to_vec())),
            _ => None,
        }
    }

    pub fn seal(&self, half: &mut Half) -> Vec<u8> {
        half.seal(&self.encode())
    }

    pub fn open(half: &mut Half, boxed: &[u8]) -> Option<Self> {
        half.open(boxed).and_then(|plain| ServerFrame::decode(&plain))
    }
}

// ---------------------------------------------------------------------------
// Persisted device store
// ---------------------------------------------------------------------------

/// A device allowed to drive this SpritX (server side). Keys are stored like SSH
/// keys: plaintext in a 0600 file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Base64 device key.
    pub key: String,
    /// Milliseconds since the Unix epoch.
    pub created: i64,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

impl Device {
    pub fn key_bytes(&self) -> Option<[u8; KEY_LEN]> {
        to_array::<KEY_LEN>(&unb64(&self.key)?)
    }
}

#[derive(Serialize, Deserialize)]
struct ServerIdentity {
    id: String,
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Host name shown to clients during pairing.
pub fn host_name() -> String {
    if let Ok(name) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    #[cfg(unix)]
    {
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
    }
    "SpritX".to_string()
}

/// JSON device/server-identity store rooted at a configurable directory (the
/// `remote/` folder under the app config dir by default).
#[derive(Clone, Debug)]
pub struct DeviceStore {
    root: PathBuf,
}

impl DeviceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$XDG_CONFIG_HOME/spritx/remote` (or `~/.config/spritx/remote`).
    pub fn default_root() -> Option<PathBuf> {
        crate::backend::config::config_path()
            .and_then(|p| p.parent().map(|d| d.join("remote")))
    }

    pub fn open_default() -> Result<Self> {
        let root = Self::default_root().ok_or_else(|| anyhow!("no config directory"))?;
        Ok(Self::new(root))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn read_json<T: for<'de> Deserialize<'de> + Default>(&self, name: &str) -> T {
        std::fs::read_to_string(self.path(name))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let body = serde_json::to_string_pretty(value)?;
        crate::backend::config::write_atomic(&self.path(name), &body)
            .with_context(|| format!("write {}", self.path(name).display()))?;
        Ok(())
    }

    /// Stable random id for this SpritX, created on first use.
    pub fn server_id(&self) -> Result<String> {
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        let existing: Option<ServerIdentity> = std::fs::read_to_string(self.path("server.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        if let Some(identity) = existing {
            if !identity.id.is_empty() {
                return Ok(identity.id);
            }
        }
        let id = new_device_id();
        self.write_json("server.json", &ServerIdentity { id: id.clone() })?;
        Ok(id)
    }

    pub fn devices(&self) -> Vec<Device> {
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        self.read_json("devices.json")
    }

    pub fn get(&self, id: &str) -> Option<Device> {
        self.devices().into_iter().find(|d| d.id == id)
    }

    /// Register (or re-register) a device. The id must be ASCII alphanumeric;
    /// control characters are stripped from the name.
    pub fn register(&self, id: &str, key: &[u8; KEY_LEN], name: &str) -> Result<Device> {
        let id = sanitize_id(id).ok_or_else(|| anyhow!("invalid device id"))?;
        let device = Device {
            id,
            name: sanitize_name(name),
            key: b64(key),
            created: now_ms(),
            last_seen: None,
        };
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<Device> = self.read_json("devices.json");
        all.retain(|d| d.id != device.id);
        all.push(device.clone());
        self.write_json("devices.json", &all)?;
        Ok(device)
    }

    /// Revoke a device. Returns `true` if it existed.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<Device> = self.read_json("devices.json");
        let before = all.len();
        all.retain(|d| d.id != id);
        if all.len() == before {
            return Ok(false);
        }
        self.write_json("devices.json", &all)?;
        Ok(true)
    }

    /// Forget every paired device (keeps the server identity).
    pub fn forget(&self) -> Result<()> {
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        self.write_json("devices.json", &Vec::<Device>::new())
    }

    pub fn touch(&self, id: &str) {
        let _guard = store_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<Device> = self.read_json("devices.json");
        if let Some(device) = all.iter_mut().find(|d| d.id == id) {
            device.last_seen = Some(now_ms());
            let _ = self.write_json("devices.json", &all);
        }
    }
}

// ---------------------------------------------------------------------------
// Server-side handshake
// ---------------------------------------------------------------------------

/// An authenticated connection: the device id plus the two sealed halves.
#[derive(Debug)]
pub struct Established {
    pub device_id: String,
    /// Server→client half.
    pub tx: Half,
    /// Client→server half.
    pub rx: Half,
}

impl Established {
    /// Open the client's first sealed frame and require it to equal [`READY`].
    pub fn accept_ready(&mut self, boxed: &[u8]) -> bool {
        self.rx.open(boxed).as_deref() == Some(&READY)
    }
}

/// Next action for the transport loop after feeding a text frame.
#[derive(Debug)]
pub enum ServerStep {
    /// Send this JSON text frame and wait for the next one.
    Reply(String),
    /// Send the challenge text frame, then treat further messages as binary;
    /// verify the client's first frame with [`Established::accept_ready`].
    Established { reply: String, session: Established },
    /// Send this denial text frame, then close.
    Denied { reply: String },
}

/// Server-side protocol state machine. It owns the persisted devices and at
/// most one open pairing code (single use).
pub struct ServerHandshake {
    devices: DeviceStore,
    server_id: String,
    server_name: String,
    pairing: Option<Pairing>,
}

impl ServerHandshake {
    pub fn new(
        devices: DeviceStore,
        server_id: impl Into<String>,
        server_name: impl Into<String>,
    ) -> Self {
        Self {
            devices,
            server_id: server_id.into(),
            server_name: server_name.into(),
            pairing: None,
        }
    }

    /// Build from the store's persisted identity and the OS host name.
    pub fn from_store(devices: DeviceStore) -> Result<Self> {
        let server_id = devices.server_id()?;
        Ok(Self::new(devices, server_id, host_name()))
    }

    pub fn store(&self) -> &DeviceStore {
        &self.devices
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The `server` hello frame that must be sent first.
    pub fn hello(&self) -> Control {
        Control::server_hello(self.server_id.clone(), self.server_name.clone())
    }

    pub fn hello_json(&self) -> String {
        self.hello().to_json()
    }

    /// Open a fresh pairing code (replacing any existing one).
    pub fn open_pairing(&mut self) -> Pairing {
        self.open_pairing_with_ttl(PAIRING_TTL)
    }

    pub fn open_pairing_with_ttl(&mut self, ttl: Duration) -> Pairing {
        let pairing = Pairing::with_ttl(ttl);
        self.pairing = Some(pairing.clone());
        pairing
    }

    pub fn close_pairing(&mut self) {
        self.pairing = None;
    }

    pub fn pairing(&self) -> Option<&Pairing> {
        self.pairing
            .as_ref()
            .filter(|pairing| !pairing.is_expired())
    }

    /// Handle one plaintext handshake frame.
    pub fn handle(&mut self, text: &str) -> ServerStep {
        let value: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return denied(reason::PROTOCOL),
        };
        match value.get("t").and_then(Value::as_str) {
            Some("pair") => self.handle_pair(&value),
            Some("hello") => self.handle_hello(&value),
            _ => denied(reason::PROTOCOL),
        }
    }

    fn handle_pair(&mut self, value: &Value) -> ServerStep {
        let secret = match self.pairing.as_ref() {
            Some(pairing) if !pairing.is_expired() => *pairing.secret(),
            _ => {
                // Expired codes are dropped; reuse is impossible anyway.
                self.pairing = None;
                return denied(reason::PAIRING);
            }
        };
        let Some(payload) = open_pair_request(&secret, value) else {
            return denied(reason::PAIRING);
        };
        // Single use: consume the code as soon as the sealed payload opens.
        self.pairing = None;
        match self.devices.register(&payload.id, &payload.key, &payload.name) {
            Ok(_device) => ServerStep::Reply(Control::Paired.to_json()),
            Err(_) => denied(reason::STORAGE),
        }
    }

    fn handle_hello(&mut self, value: &Value) -> ServerStep {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return denied(reason::PROTOCOL);
        };
        let Some(nc) = value
            .get("nc")
            .and_then(Value::as_str)
            .and_then(unb64)
            .and_then(|bytes| to_array::<NONCE_LEN>(&bytes))
        else {
            return denied(reason::PROTOCOL);
        };
        let Some(device) = self.devices.get(id) else {
            return denied(reason::UNKNOWN);
        };
        let Some(key) = device.key_bytes() else {
            return denied(reason::UNKNOWN);
        };
        let ns = random::<NONCE_LEN>();
        let (tx, rx) = server_session(&key, &nc, &ns);
        self.devices.touch(id);
        ServerStep::Established {
            reply: Control::challenge(&ns).to_json(),
            session: Established {
                device_id: device.id,
                tx,
                rx,
            },
        }
    }
}

fn denied(reason: &str) -> ServerStep {
    ServerStep::Denied {
        reply: Control::denied(reason).to_json(),
    }
}
