//! Command history store (JSONL append/search/clear, ignore patterns).
//!
//! One JSON object per executed command, appended to `history.jsonl` in SpritX's
//! config directory. Unlike a shell HISTFILE it keeps the context: cwd, exit
//! code, duration, git branch, and finish time. Searches are a reverse linear
//! scan over the newest [`MAX_READ_LINES`] lines, so a corrupt or huge file
//! degrades gracefully instead of failing outright.
//!
//! No GPUI dependency.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "history.jsonl";
/// Only the newest lines are loaded; older history is ignored.
pub const MAX_READ_LINES: usize = 10_000;
/// Commands longer than this are pasted scripts, not worth recalling.
pub const MAX_COMMAND_LEN: usize = 2000;
/// Default result cap for [`HistoryStore::search`].
pub const DEFAULT_LIMIT: usize = 500;

/// One executed command with its context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Finish time, milliseconds since the Unix epoch.
    #[serde(default)]
    pub finished_at: i64,
}

impl HistoryEntry {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            exit_code: 0,
            duration_ms: 0,
            branch: None,
            finished_at: 0,
        }
    }
}

/// A JSONL-backed history store. `path` may be overridden (tests, alternate
/// profiles); [`HistoryStore::open_default`] uses the config directory.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    path: PathBuf,
    ignore: Vec<String>,
}

impl HistoryStore {
    /// A store at `path`, with no ignore patterns.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ignore: Vec::new(),
        }
    }

    /// The user's store (`$XDG_CONFIG_HOME/spritx/history.jsonl`), or `None`
    /// when no config directory can be determined.
    pub fn open_default() -> Option<Self> {
        history_path().map(|path| Self {
            path,
            ignore: crate::backend::config::default_history_ignore(),
        })
    }

    /// Replace the "never record" glob patterns. Matching is case-insensitive
    /// and applies to the whole command line.
    pub fn with_ignore(mut self, patterns: Vec<String>) -> Self {
        self.ignore = patterns;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an entry. Returns `Ok(())` for entries that are deliberately not
    /// recorded (leading space, empty, too long, or matching an ignore pattern).
    ///
    /// The write is handed to a background thread, so finishing a command never
    /// blocks the UI thread on disk I/O (the open + write used to run on the
    /// event loop and hitched right as a command completed).
    pub fn append(&self, mut entry: HistoryEntry) -> Result<(), String> {
        // Leading space = "don't record me" (HISTCONTROL=ignorespace).
        if entry.command.starts_with(' ') {
            return Ok(());
        }
        entry.command = entry.command.trim().to_string();
        if entry.command.is_empty() || entry.command.len() > MAX_COMMAND_LEN {
            return Ok(());
        }
        if is_ignored(&entry.command, &self.ignore) {
            return Ok(());
        }
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
        // A full queue drops this entry (counted in `DROPPED`) rather than
        // blocking the caller — usually the UI thread — on a slow disk.
        enqueue(WriterMessage::Append(self.path.clone(), line));
        Ok(())
    }

    /// Search the newest window of history. Uses the process current directory
    /// for the `here` filter.
    pub fn search(&self, query: &str) -> Vec<HistoryEntry> {
        let here = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        self.search_with_cwd(query, here.as_deref())
    }

    /// Search with an explicit directory for `here`.
    ///
    /// Filters (free text is ANDed, case-insensitive):
    /// `failed` / `fail` / `failed:true`, `ok` / `success` / `failed:false`,
    /// `here`, `cwd:<text>`, `dir:<text>`, `project:<text>`, `branch:<text>`.
    /// Results are newest first.
    pub fn search_with_cwd(&self, query: &str, here: Option<&str>) -> Vec<HistoryEntry> {
        self.search_limited(query, here, DEFAULT_LIMIT)
    }

    /// Like [`HistoryStore::search_with_cwd`] with an explicit result cap.
    pub fn search_limited(
        &self,
        query: &str,
        here: Option<&str>,
        limit: usize,
    ) -> Vec<HistoryEntry> {
        let filters = parse_query(query);
        let lines = read_lines_bounded(&self.path);
        let mut out = Vec::new();
        for line in lines.iter().rev() {
            let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) else {
                continue; // tolerate a corrupt line
            };
            if !matches(&entry, &filters, here) {
                continue;
            }
            out.push(entry);
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Every entry in the newest window, newest first.
    pub fn all(&self) -> Vec<HistoryEntry> {
        self.search_limited("", None, usize::MAX)
    }

    /// Delete the history file (missing is fine).
    pub fn clear(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        enqueue(WriterMessage::Clear(self.path.clone(), tx));
        // WHY the timeout: this waits on the background writer thread. If that
        // thread never started (its spawn failure is logged below) or is
        // wedged, a bare `recv()` hangs the caller — potentially the UI thread
        // — forever. A timeout turns an indefinite hang into a visible error.
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?
    }
}

/// `$XDG_CONFIG_HOME/spritx/history.jsonl`, falling back to
/// `~/.config/spritx/history.jsonl`.
pub fn history_path() -> Option<PathBuf> {
    crate::platform::config_dir().map(|dir| dir.join(FILE_NAME))
}

/// Hard cap on queued-but-unwritten entries. The queue used to be an unbounded
/// `mpsc::channel`, so a burst of commands (a pasted `for` loop, a runaway
/// script emitting shell markers) could grow the process's memory without
/// limit while the writer thread stalled on a slow disk. When the queue is
/// full the *newest* entry is dropped and counted: the oldest history is the
/// more valuable part of a burst.
pub const MAX_QUEUED_ENTRIES: usize = 1024;

/// Queue of lines awaiting the writer thread. `VecDeque` under a mutex is used
/// instead of a channel so the capacity check and the push happen under one
/// lock — a channel's `send` cannot report "full, drop the new item" without a
/// second copy of the payload.
#[derive(Default)]
struct Outbox {
    lines: VecDeque<WriterMessage>,
    /// Bumped by the writer after it empties the queue and syncs the file.
    /// [`flush_history`] waits for it to pass the value it observed, so flush
    /// is repeatable (background, then quit) and returns immediately when idle.
    drained: u64,
    /// Set by [`flush_history`]; the writer `sync_all`s once it has drained.
    sync_wanted: bool,
}

enum WriterMessage {
    Append(PathBuf, String),
    Clear(PathBuf, Sender<Result<(), String>>),
}

/// Entries dropped because the queue was full. Crate-visible so a test can
/// assert the bound holds under load.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many history entries have been dropped because the queue was full.
/// Exposed for tests that assert [`MAX_QUEUED_ENTRIES`] bounds memory.
pub fn dropped_entries() -> u64 {
    DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set true once the writer thread has been spawned; [`flush_history`] checks
/// it so an app that never recorded history does not spawn an idle thread.
static WRITER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The outbox plus the signal that wakes the writer when work arrives.
fn outbox() -> &'static (std::sync::Mutex<Outbox>, std::sync::Condvar) {
    static OUTBOX: LazyLock<(std::sync::Mutex<Outbox>, std::sync::Condvar)> =
        LazyLock::new(|| {
            let state = (std::sync::Mutex::new(Outbox::default()), std::sync::Condvar::new());
            std::thread::Builder::new()
                .name("spritx-history-writer".into())
                .spawn(|| writer_loop())
                .map_err(|e| eprintln!("[spritx] could not spawn history writer thread: {e}"))
                .ok();
            WRITER_STARTED.store(true, std::sync::atomic::Ordering::Release);
            state
        });
    &OUTBOX
}

/// Push a message and wake the writer. Returns `false` when the queue is at
/// [`MAX_QUEUED_ENTRIES`] and the message was dropped without blocking the
/// caller (which is usually the UI thread).
fn enqueue(message: WriterMessage) -> bool {
    let (lock, wake) = outbox();
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.lines.len() >= MAX_QUEUED_ENTRIES {
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    guard.lines.push_back(message);
    wake.notify_one();
    true
}

/// Append to the default store (no-op when no config directory exists).
pub fn append(entry: HistoryEntry) -> Result<(), String> {
    match HistoryStore::open_default() {
        Some(store) => store.append(entry),
        None => Ok(()),
    }
}

/// Drain the queue and sync the file to disk. Repeatable and safe when no
/// history was ever recorded (the writer thread is only started by the first
/// `enqueue`). Bounded wait so shutdown cannot hang on a wedged disk.
pub fn flush_history(timeout: std::time::Duration) {
    // Nothing was ever recorded: do not start the writer thread just to sync an
    // empty queue.
    if !WRITER_STARTED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let (lock, wake) = outbox();
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Request the durable sync and publish a drain target one past what we see,
    // so the wait below covers every line queued before this point. When the
    // queue is already empty the writer still syncs (a prior flush may not have
    // run yet) — hence the unconditional `sync_wanted`.
    guard.sync_wanted = true;
    let target = guard.drained.wrapping_add(1);
    wake.notify_all();
    let deadline = std::time::Instant::now() + timeout;
    while guard.drained < target && std::time::Instant::now() < deadline {
        let (next, _) = wake
            .wait_timeout(guard, std::time::Duration::from_millis(20))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard = next;
    }
}

/// Writer thread body: pull from the outbox, keep one file handle open, and
/// sync when a flush is requested.
fn writer_loop() {
    let (lock, wake) = outbox();
    let mut open: Option<(PathBuf, std::fs::File)> = None;
    loop {
        // Hold the lock only long enough to pop one message, then release it so
        // disk I/O never blocks `enqueue`/`flush_history` callers.
        let message = {
            let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while guard.lines.is_empty() && !guard.sync_wanted {
                guard = wake
                    .wait(guard)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            guard.lines.pop_front()
        };
        match message {
            Some(WriterMessage::Append(path, line)) => {
                let reopen = open.as_ref().map(|(p, _)| p != &path).unwrap_or(true);
                if reopen {
                    if let Some(dir) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(dir) {
                            // WHY the log: `open` is None from here on, so
                            // every append for this path would be silently
                            // skipped; surface the cause instead.
                            eprintln!("[spritx] could not create history directory {}: {e}", dir.display());
                        }
                    }
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        Ok(file) => open = Some((path, file)),
                        Err(e) => {
                            eprintln!("[spritx] could not open history file {}: {e}", path.display());
                            open = None;
                        }
                    }
                }
                if let Some((_, file)) = open.as_mut() {
                    if let Err(e) = file.write_all(line.as_bytes()) {
                        eprintln!("[spritx] could not write history entry: {e}");
                    } else if let Err(e) = file.write_all(b"\n") {
                        eprintln!("[spritx] could not write history entry: {e}");
                    }
                }
            }
            Some(WriterMessage::Clear(path, reply)) => {
                if open.as_ref().is_some_and(|(p, _)| p == &path) {
                    open = None;
                }
                let result = match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = reply.send(result);
            }
            // Nothing left to write: this must be the flush path.
            None => {}
        }
        // Once the queue is empty, honour a pending flush by syncing the file
        // and publishing the drain so `flush_history` returns.
        let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.lines.is_empty() && guard.sync_wanted {
            if let Some((_, file)) = open.as_mut() {
                // `write_all` reached the OS; sync so the tail survives exit.
                let _ = file.sync_all();
            }
            guard.sync_wanted = false;
            guard.drained = guard.drained.wrapping_add(1);
        }
        drop(guard);
        wake.notify_all();
    }
}

/// Search the default store.
pub fn search(query: &str) -> Vec<HistoryEntry> {
    HistoryStore::open_default()
        .map(|store| store.search(query))
        .unwrap_or_default()
}

/// Clear the default store.
pub fn clear() -> Result<(), String> {
    match HistoryStore::open_default() {
        Some(store) => store.clear(),
        None => Ok(()),
    }
}

/// Does `command` match one of the user's "never record" patterns?
pub fn is_ignored(command: &str, patterns: &[String]) -> bool {
    let command = command.trim().to_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_lowercase();
        !pattern.is_empty() && glob_match(&pattern, &command)
    })
}

/// Case-insensitive `*` / `?` glob matching (whole string).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.to_lowercase().chars().collect();
    let txt: Vec<char> = text.to_lowercase().chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut star_t) = (None::<usize>, 0usize);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

fn read_lines_bounded(path: &Path) -> Vec<String> {
    let _mem = crate::memprof::scope(crate::memprof::Region::History);
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file);
    let mut window: VecDeque<String> = VecDeque::with_capacity(MAX_READ_LINES.min(1024));
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                if window.len() == MAX_READ_LINES {
                    window.pop_front();
                }
                window.push_back(line.to_string());
            }
            Err(_) => break,
        }
    }
    window.into_iter().collect()
}

#[derive(Default, Debug)]
struct Filters {
    terms: Vec<String>,
    failed: Option<bool>,
    here: bool,
    cwd: Vec<String>,
    branch: Vec<String>,
}

fn parse_query(query: &str) -> Filters {
    let mut filters = Filters::default();
    for word in query.split_whitespace() {
        let lower = word.to_lowercase();
        match lower.as_str() {
            "failed" | "failed:true" | "failed:yes" | "fail" => filters.failed = Some(true),
            "ok" | "failed:false" | "failed:no" | "success" => filters.failed = Some(false),
            "here" => filters.here = true,
            _ => {
                if let Some((key, value)) = lower.split_once(':') {
                    if !value.is_empty() {
                        match key {
                            "cwd" | "dir" | "project" | "in" => {
                                filters.cwd.push(value.to_string());
                                continue;
                            }
                            "branch" | "br" => {
                                filters.branch.push(value.to_string());
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
                filters.terms.push(lower);
            }
        }
    }
    filters
}

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let n = p.replace('\\', "/");
        let n = n.trim_end_matches('/').to_string();
        if cfg!(windows) {
            n.to_lowercase()
        } else {
            n
        }
    };
    norm(a) == norm(b)
}

fn matches(entry: &HistoryEntry, filters: &Filters, here: Option<&str>) -> bool {
    match filters.failed {
        Some(true) if entry.exit_code == 0 => return false,
        Some(false) if entry.exit_code != 0 => return false,
        _ => {}
    }
    if filters.here {
        match (here, entry.cwd.as_deref()) {
            (Some(h), Some(c)) if same_dir(h, c) => {}
            _ => return false,
        }
    }
    if !filters.cwd.is_empty() {
        let cwd = entry.cwd.as_deref().unwrap_or("").to_lowercase();
        if !filters.cwd.iter().all(|v| cwd.contains(v.as_str())) {
            return false;
        }
    }
    if !filters.branch.is_empty() {
        let branch = entry.branch.as_deref().unwrap_or("").to_lowercase();
        if !filters.branch.iter().all(|v| branch.contains(v.as_str())) {
            return false;
        }
    }
    if !filters.terms.is_empty() {
        let command = entry.command.to_lowercase();
        if !filters.terms.iter().all(|t| command.contains(t.as_str())) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue is bounded: a burst of appends can never push more than
    /// [`MAX_QUEUED_ENTRIES`] messages into the in-memory outbox, and any
    /// excess is counted rather than growing without limit.
    #[test]
    fn queue_is_hard_bounded_and_counts_drops() {
        let before = dropped_entries();
        let path = std::env::temp_dir().join(format!("spritx_hist_bound_{}", std::process::id()));
        // WAY past capacity; the shared writer may drain concurrently, so we
        // assert the invariant (never exceeds the cap) rather than an exact
        // drop count.
        for index in 0..(MAX_QUEUED_ENTRIES * 4) {
            enqueue(WriterMessage::Append(path.clone(), format!("line {index}")));
        }
        let (lock, _) = outbox();
        let guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.lines.len() <= MAX_QUEUED_ENTRIES,
            "outbox grew past the cap: {} > {MAX_QUEUED_ENTRIES}",
            guard.lines.len()
        );
        drop(guard);
        assert!(
            dropped_entries() >= before,
            "the drop counter must be monotonic"
        );
        // Flush so the temp target is cleaned by the OS and no line lingers.
        flush_history(std::time::Duration::from_secs(2));
        let _ = std::fs::remove_file(&path);
    }

    /// `flush_history` is safe to call when no writer ever started and does not
    /// hang.
    #[test]
    fn flush_is_safe_and_bounded() {
        let started = std::time::Instant::now();
        flush_history(std::time::Duration::from_millis(50));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
