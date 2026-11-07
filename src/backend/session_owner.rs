//! A user-local PTY owner. Desktop clients can disconnect without ending the child.
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use crate::terminal::{SessionConfig, TerminalSession};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const MAX_FRAME: u64 = 4 * 1024 * 1024;
const TAIL_LIMIT: usize = 512 * 1024;
const CONNECTION_GRACE: Duration = Duration::from_secs(5);
/// WHY: the owner token is a 256-bit bearer secret. `String`/`str` equality
/// can exit at the first differing byte, so its timing leaks how many leading
/// bytes matched. Accumulate every byte difference for equal-length inputs
/// instead; a length mismatch still fails fast, but that reveals only length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// WHY: `take(MAX_FRAME).read_line()` is a bounded read, not a frame-length
/// check. If the line is longer, it succeeds with exactly `MAX_FRAME` bytes
/// and no trailing newline, leaving the remainder in the underlying reader.
/// Parsing the next loop's bytes as a fresh frame would desynchronize the
/// protocol, so drain through the newline (bounded) before returning the
/// prefix. The read timeout on the socket bounds the drain as well.
fn read_line_bounded<R: Read>(reader: &mut R, limit: u64) -> Result<String> {
    let mut line = String::new();
    let mut limited = BufReader::new((&mut *reader).take(limit));
    limited.read_line(&mut line)?;
    if line.len() as u64 >= limit && !line.ends_with('\n') {
        // Read one byte at a time: a chunk read could swallow bytes past
        // the newline that belong to the next frame, and a generic `Read`
        // has no way to push them back. Byte-by-byte stops exactly at the
        // newline, preserving the next frame. Bounded by `remaining`.
        let mut byte = [0u8; 1];
        let mut remaining = limit.saturating_mul(2);
        while remaining > 0 {
            let read = reader.read(&mut byte)?;
            if read == 0 {
                break;
            }
            remaining -= 1;
            if byte[0] == b'\n' {
                break;
            }
        }
        bail!("Frame exceeds {limit} byte limit");
    }
    Ok(line)
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub port: u16,
    pub token: String,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub disconnected: bool,
    pub activation: u64,
    pub process_exit: Option<i32>,
    pub running: bool,
    pub started_at_ms: Option<i64>,
    pub command: Option<String>,
    pub completion: u64,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize)]
enum Action {
    Attach,
    Detach,
    Focus,
    Read { offset: Option<u64> },
    Input(Vec<u8>),
    Resize(u16, u16),
    Kill,
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    client: String,
    action: Action,
}

#[derive(Default, Serialize, Deserialize)]
struct Reply {
    attached: bool,
    bytes: Vec<u8>,
    snapshot: Option<String>,
    next: u64,
    cols: u16,
    rows: u16,
    pid: Option<u32>,
    status: Status,
    error: Option<String>,
}

impl Connection {
    fn request(&self, client: &str, action: Action) -> Result<Reply> {
        let address = ([127, 0, 0, 1], self.port).into();
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        write_frame(
            &mut stream,
            &Request {
                token: self.token.clone(),
                client: client.into(),
                action,
            },
        )?;
        // WHY: `Read::take(MAX_FRAME)` can return Ok(MAX_FRAME) without a
        // newline for an oversized frame. Bounded reading plus drain makes
        // that an explicit protocol error here instead of a shortened reply
        // that could be mistaken for valid JSON by the caller.
        let data = read_line_bounded(&mut stream, MAX_FRAME)?;
        let reply: Reply = serde_json::from_str(&data)?;
        if let Some(error) = &reply.error {
            bail!("{error}");
        }
        Ok(reply)
    }

    pub fn unavailable(&self) -> bool {
        matches!(TcpStream::connect_timeout(&([127,0,0,1], self.port).into(), Duration::from_millis(400)),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused)
    }
}

#[derive(Default)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub snapshot: Option<(u16, u16, String)>,
    next: Option<u64>,
}

pub struct Client {
    output: Arc<Mutex<Output>>,
    pub connection: Connection,
    pub status: Arc<Mutex<Status>>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub initial: String,
    stop: Arc<AtomicBool>,
    client_id: String,
}

impl Client {
    pub fn attach(
        connection: Connection,
        pending: Arc<Mutex<Output>>,
        dirty: Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
        let client_id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let started = std::time::Instant::now();
        loop {
            match connection.request(&client_id, Action::Attach) {
                Ok(_) => break,
                Err(error)
                    if error.to_string().contains("attached to another window")
                        && started.elapsed() < Duration::from_millis(3500) =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        }
        let first = connection.request(&client_id, Action::Read { offset: None })?;
        let status = Arc::new(Mutex::new(first.status.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        pending.lock().unwrap().next = Some(first.next);
        let client = Self {
            output: pending.clone(),
            connection: connection.clone(),
            status: status.clone(),
            pid: first.pid,
            cols: first.cols,
            rows: first.rows,
            initial: first.snapshot.unwrap_or_default(),
            stop: stop.clone(),
            client_id: client_id.clone(),
        };
        std::thread::spawn(move || {
            let mut failed_since = None;
            while !stop.load(Ordering::Relaxed) {
                let mut queue = pending.lock().unwrap();
                let offset = queue.next;
                match connection.request(&client_id, Action::Read { offset }) {
                    Ok(reply) => {
                        failed_since = None;
                        queue.next = Some(reply.next);
                        *status.lock().unwrap() = reply.status;
                        if let Some(snapshot) = reply.snapshot {
                            queue.bytes.clear();
                            queue.snapshot = Some((reply.cols, reply.rows, snapshot));
                        }
                        if queue.bytes.len() + reply.bytes.len() <= 2 * 1024 * 1024 {
                            queue.bytes.extend(reply.bytes);
                        } else {
                            // Ask for a fresh screen on the next poll if the UI stalled.
                            queue.next = None;
                        }
                        if !queue.bytes.is_empty() || queue.snapshot.is_some() {
                            dirty.store(true, Ordering::Relaxed);
                            crate::terminal::signal_wake();
                        }
                        drop(queue);
                    }
                    Err(error) => {
                        drop(queue);
                        let unavailable = connection.unavailable();
                        let since = *failed_since.get_or_insert_with(std::time::Instant::now);
                        let disconnected = unavailable || since.elapsed() >= CONNECTION_GRACE;
                        {
                            let mut current = status.lock().unwrap();
                            if disconnected && !current.disconnected {
                                eprintln!("[spritx] session owner connection failed: {error}");
                            }
                            current.disconnected = disconnected;
                            if unavailable {
                                if current.running {
                                    current.completion += 1;
                                }
                                current.running = false;
                                current.exit_code = Some(-1);
                                current.process_exit = Some(-1);
                            }
                        }
                        dirty.store(true, Ordering::Relaxed);
                        crate::terminal::signal_wake();
                        if unavailable {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        let _ = connection.request(&client_id, Action::Attach);
                    }
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        Ok(client)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(32 * 1024) {
            self.connection
                .request(&self.client_id, Action::Input(chunk.to_vec()))?;
        }
        Ok(())
    }
    pub fn take_output(&self) -> (Option<(u16, u16, String)>, Vec<u8>) {
        let Ok(mut output) = self.output.try_lock() else { return (None, Vec::new()); };
        (output.snapshot.take(), std::mem::take(&mut output.bytes))
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        // Serialize resize with polling. The replacement consumes all bytes up
        // to `next`, so old-width output cannot be parsed in the new grid.
        let mut output = self.output.lock().unwrap();
        let reply = self.connection.request(&self.client_id, Action::Resize(cols, rows))?;
        output.bytes.clear();
        output.next = Some(reply.next);
        output.snapshot = Some((reply.cols, reply.rows, reply.snapshot.unwrap_or_default()));
        Ok(())
    }
    pub fn kill(&self) {
        let _ = self.connection.request(&self.client_id, Action::Kill);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.connection.request(&self.client_id, Action::Detach);
    }
}

pub fn secure_directory(root: &Path) -> Result<()> {
    static SECURED: std::sync::OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    let mut secured = SECURED.get_or_init(Default::default).lock().unwrap();
    if secured.contains(root) && root.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let user = format!(
            r"{}\{}",
            std::env::var("USERDOMAIN")?,
            std::env::var("USERNAME")?
        );
        let grant = format!("{user}:(OI)(CI)F");
        let result = std::process::Command::new("icacls")
            .arg(root)
            .args(["/inheritance:r", "/grant:r", &grant])
            .creation_flags(0x08000000)
            .output()?;
        if !result.status.success() {
            bail!("Could not restrict session credentials to the current user");
        }
    }
    secured.insert(root.to_owned());
    Ok(())
}

pub fn start(config: &SessionConfig) -> Result<Connection> {
    let root = crate::platform::config_dir()
        .context("Config directory unavailable")?
        .join("sessions");
    start_at(config, &std::env::current_exe()?, &root)
}

pub fn start_at(config: &SessionConfig, executable: &Path, root: &Path) -> Result<Connection> {
    secure_directory(&root)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
    let key: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(format!("{key}.json"));
    let mut command = std::process::Command::new(executable);
    command
        .arg("--session-owner")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP disables Ctrl+C handling and that ignore
        // state is inherited by shells/Node inside the ConPTY. The PTY already
        // isolates its console; only hide the helper's own console window.
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut input = child.stdin.take().context("Owner stdin unavailable")?;
    serde_json::to_writer(&mut input, &(key, config))?;
    input.write_all(
        b"
",
    )?;
    drop(input);
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Ok(data) = std::fs::read(&path) {
            let connection = serde_json::from_slice(&data)?;
            let _ = std::fs::remove_file(&path);
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(connection);
        }
        if let Some(status) = child.try_wait()? {
            bail!("Session owner exited: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!("Session owner did not start")
}

fn collect_output(
    session: &mut TerminalSession, tracker: &mut crate::backend::osc::BlockTracker,
    status: &mut Status, tail: &mut VecDeque<u8>, next: &mut u64,
    command_started: &mut Option<std::time::Instant>,
) {
    if let Some(bytes) = session.pump_bytes() {
            for event in tracker.feed(&bytes).events {
                use crate::backend::osc::OscEvent;
                match event.event {
                    OscEvent::CommandLine(command) => status.command = Some(command),
                    OscEvent::OutputStart => {
                        status.running = true;
                        status.started_at_ms = Some(crate::backend::workspace::now_ms());
                        status.exit_code = None;
                        *command_started = Some(std::time::Instant::now());
                    }
                    OscEvent::OutputEnd { exit_code } => {
                        if status.running {
                            status.completion += 1;
                        }
                        status.running = false;
                        status.exit_code = Some(exit_code);
                    }
                    _ => {}
                }
            }
            if let Some(cwd) = tracker.current_cwd() { status.cwd = Some(cwd.to_string()); }
            tracker.take_blocks();
            *next += bytes.len() as u64;
            tail.extend(bytes);
            while tail.len() > TAIL_LIMIT {
                tail.pop_front();
            }
        }
}

pub fn run(path: &Path) -> Result<()> {
    let mut stdin = std::io::stdin();
    let line = read_line_bounded(&mut stdin, MAX_FRAME)?;
    let (token, config): (String, SessionConfig) = serde_json::from_str(&line)?;
    let mut session = TerminalSession::spawn(config.clone())?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let connection = Connection {
        port: listener.local_addr()?.port(),
        token,
    };
    crate::backend::config::write_atomic(path, &serde_json::to_string(&connection)?)?;
    let mut tail = VecDeque::new();
    let mut next = 0u64;
    let mut tracker = crate::backend::osc::BlockTracker::default();
    let mut status = Status {
        cwd: session.launch_config().and_then(|config| config.cwd),
        ..Default::default()
    };
    let mut ended_at = None;
    let mut command_started = None;
    let display_name = session
        .launch_config()
        .and_then(|config| config.display_name)
        .unwrap_or_else(|| "Command".into());
    if session
        .launch_config()
        .is_some_and(|config| config.track_process)
    {
        status.running = true;
        status.started_at_ms = Some(crate::backend::workspace::now_ms());
        status.command = Some(config.display_name.clone().unwrap_or_else(||
            Path::new(&config.program).file_stem().unwrap_or_default().to_string_lossy().into_owned()));
        command_started = Some(std::time::Instant::now());
    }
    let mut notified = 0;
    let mut last_read = std::time::Instant::now();
    let mut lease: Option<String> = None;
    let mut ever_attached = false;
    let started_at = std::time::Instant::now();
    let mut checked_preferences = std::time::Instant::now();
    loop {
        if checked_preferences.elapsed() >= Duration::from_secs(1) {
            checked_preferences = std::time::Instant::now();
            if !ever_attached && started_at.elapsed() > Duration::from_secs(30) {
                session.kill();
                return Ok(());
            }
            if ever_attached && last_read.elapsed() > Duration::from_secs(3) {
                let prefs = crate::backend::config::AppConfig::load().behavior;
                if !prefs.keep_sessions || !prefs.restore_workspace {
                    session.kill();
                    return Ok(());
                }
            }
        }
        collect_output(&mut session, &mut tracker, &mut status, &mut tail, &mut next, &mut command_started);
        if let Some(exit_code) = session.exit_code() {
            if ended_at.is_none() {
                ended_at = Some(std::time::Instant::now());
                if status.running {
                    status.completion += 1;
                }
                status.running = false;
                status.exit_code = Some(exit_code);
                status.process_exit = Some(exit_code);
            }
            if ended_at.is_some_and(|at| at.elapsed() > Duration::from_secs(300)) {
                return Ok(());
            }
        }
        if status.completion > notified {
            notified = status.completion;
            let elapsed = command_started
                .take()
                .map(|at| at.elapsed())
                .unwrap_or_default();
            let connection = connection.clone();
            let project = status.cwd.clone().unwrap_or_default();
            crate::platform::command_completed(
                &project,
                &display_name,
                status.exit_code.unwrap_or(-1),
                elapsed,
                move || {
                    let attached = connection
                        .request("", Action::Focus)
                        .map(|reply| reply.attached)
                        .unwrap_or(false);
                    if !attached {
                        let mut command =
                            std::process::Command::new(std::env::current_exe().unwrap_or_default());
                        #[cfg(windows)]
                        {
                            use std::os::windows::process::CommandExt;
                            command.creation_flags(0x08000000);
                        }
                        let _ = command
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                    }
                },
            );
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let line = match read_line_bounded(&mut stream, MAX_FRAME) {
                    Ok(line) => line,
                    Err(_) => continue,
                };
                let Ok(request) = serde_json::from_str::<Request>(&line) else {
                    continue;
                };
                // WHY: constant-time compare prevents byte-by-byte timing recovery
                // of the 256-bit token; mismatch still drops silently as before.
                if !constant_time_eq(request.token.as_bytes(), connection.token.as_bytes()) {
                    continue;
                }
                let (cols, rows) = session.size();
                let mut reply = Reply {
                    attached: lease.is_some() && last_read.elapsed() < Duration::from_secs(2),
                    next,
                    cols,
                    rows,
                    pid: session.pid(),
                    status: status.clone(),
                    ..Default::default()
                };
                let mut kill = false;
                let attached_elsewhere =
                    lease.as_ref().is_some_and(|lease| *lease != request.client)
                        && last_read.elapsed() < Duration::from_secs(3);
                let denied = match request.action {
                    Action::Focus => false,
                    Action::Attach => attached_elsewhere,
                    _ => lease.as_deref() != Some(request.client.as_str()),
                };
                if denied {
                    reply.error = Some(
                        "Session is attached to another window or the client lease expired".into(),
                    );
                    let _ = write_frame(&mut stream, &reply);
                    continue;
                }
                match request.action {
                    Action::Attach => {
                        ever_attached = true;
                        lease = Some(request.client);
                        last_read = std::time::Instant::now();
                    }
                    Action::Detach => {
                        lease = None;
                    }
                    Action::Focus => {
                        status.activation += 1;
                    }
                    Action::Read { offset } => {
                        last_read = std::time::Instant::now();
                        if let Some(offset) = offset
                            .filter(|offset| *offset <= next && next - offset <= tail.len() as u64)
                        {
                            reply.bytes = tail
                                .iter()
                                .skip(tail.len() - (next - offset) as usize)
                                .copied()
                                .collect();
                        } else {
                            reply.snapshot = Some(session.saved_screen());
                        }
                    }
                    Action::Input(bytes) => {
                        if let Err(error) = session.write(&bytes) {
                            reply.error = Some(error.to_string());
                        }
                    }
                    Action::Resize(cols, rows) => {
                        if let Err(error) = session.resize(cols, rows) {
                            reply.error = Some(error.to_string());
                        }
                        collect_output(&mut session, &mut tracker, &mut status, &mut tail, &mut next, &mut command_started);
                        reply.snapshot = Some(session.saved_screen());
                    }
                    Action::Kill => {
                        session.kill();
                        kill = true;
                    }
                }
                (reply.cols, reply.rows) = session.size();
                reply.next = next;
                reply.status = status.clone();
                let _ = write_frame(&mut stream, &reply);
                if kill {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn recovery_snapshots_replace_history_and_resize_updates_dimensions() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let connection = Connection { port: listener.local_addr().unwrap().port(), token: "snapshot-test".into() };
        let snapshot = (0..30).map(|n| format!("LINE_{n}\r\n")).collect::<String>() + "READY";
        let server = std::thread::spawn(move || {
            let (mut cols, mut rows) = (40, 8);
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                if let Action::Resize(c, r) = request.action { cols = c; rows = r; }
                let reply = Reply { cols, rows, snapshot: Some(snapshot.clone()), ..Default::default() };
                let _ = write_frame(&mut stream, &reply);
                if matches!(request.action, Action::Kill) { break; }
            }
        });
        let mut session = TerminalSession::attach_persistent(SessionConfig::default(), connection).unwrap();
        let original = session.scrollback_len();
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(40));
            session.pump();
            assert_eq!(session.scrollback_len(), original, "recovery appended duplicate history");
        }
        session.resize(60, 12).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while session.size() != (60, 12) && std::time::Instant::now() < until { session.pump(); }
        assert_eq!(session.size(), (60, 12));
        assert!(session.screen_text().contains("READY"));
        session.kill();
        server.join().unwrap();
    }

    #[test]
    fn temporary_slow_reply_keeps_running_and_unreachable_owner_reports_disconnect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let connection = Connection {
            port: listener.local_addr().unwrap().port(),
            token: "test-health".into(),
        };
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = reads.clone();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream.set_nodelay(true).unwrap();
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let request: Request = serde_json::from_str(&line).unwrap();
                let reply = Reply {
                    cols: 80,
                    rows: 24,
                    status: Status {
                        running: true,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                if matches!(request.action, Action::Read { .. }) {
                    let count = reads.fetch_add(1, Ordering::SeqCst);
                    if count == 1 {
                        // The client read timeout expires once, while the owner remains alive.
                        std::thread::sleep(Duration::from_millis(2300));
                    }
                }
                let _ = write_frame(&mut stream, &reply);
                if matches!(request.action, Action::Kill) {
                    break;
                }
            }
        });
        let client = Client::attach(
            connection,
            Arc::new(Mutex::new(Output::default())),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let start = std::time::Instant::now();
        let mut false_disconnect = false;
        while observed_reads.load(Ordering::SeqCst) < 3 && start.elapsed() < Duration::from_secs(6)
        {
            false_disconnect |= client.status.lock().unwrap().disconnected;
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            observed_reads.load(Ordering::SeqCst) >= 3,
            "Polling must recover after a slow reply"
        );
        assert!(
            !false_disconnect,
            "One slow reply must not flip a running session to disconnected"
        );
        assert!(client.status.lock().unwrap().running);
        client.kill();
        server.join().unwrap();
        let start = std::time::Instant::now();
        while !client.status.lock().unwrap().disconnected
            && start.elapsed() < Duration::from_secs(8)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let state = client.status.lock().unwrap().clone();
        assert!(state.disconnected);
    }

    #[test]
    fn constant_time_eq_matches_only_equal_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn oversized_frame_fails_without_desync() {
        // WHY: `Read::take(1).read_line` returns Ok(1) for "a\n", and without
        // the drain the next read would start at "\n" or "b\n" misaligned.
        let mut bounded = std::io::Cursor::new([b'a', b'\n', b'b', b'\n']);
        assert!(read_line_bounded(&mut bounded, 1).is_err());
        assert_eq!(bounded.position(), 2);
        // The drain must stop at the newline, not swallow the next frame.
        let mut remaining = String::new();
        bounded.read_line(&mut remaining).unwrap();
        assert_eq!(remaining, "b\n");
    }
}
