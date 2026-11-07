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
use std::sync::OnceLock;

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
        writer()
            .send(WriterMessage::Append(self.path.clone(), line))
            .map_err(|e| e.to_string())?;
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
        writer().send(WriterMessage::Clear(self.path.clone(), tx)).map_err(|e| e.to_string())?;
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

enum WriterMessage {
    Append(PathBuf, String),
    Clear(PathBuf, Sender<Result<(), String>>),
}

/// Background JSONL writer. Keeps one file handle open and appends queued
/// lines off the UI thread.
fn writer() -> &'static Sender<WriterMessage> {
    static WRITER: OnceLock<Sender<WriterMessage>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<WriterMessage>();
        std::thread::Builder::new()
            .name("spritx-history-writer".into())
            .spawn(move || {
                let mut open: Option<(PathBuf, std::fs::File)> = None;
                while let Ok(message) = rx.recv() {
                    let (path, line) = match message {
                        WriterMessage::Append(path, line) => (path, line),
                        WriterMessage::Clear(path, reply) => {
                            if open.as_ref().is_some_and(|(p, _)| p == &path) {
                                open = None;
                            }
                            let result = match std::fs::remove_file(&path) {
                                Ok(()) => Ok(()),
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                                Err(e) => Err(e.to_string()),
                            };
                            let _ = reply.send(result);
                            continue;
                        }
                    };
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
            })
            .map_err(|e| eprintln!("[spritx] could not spawn history writer thread: {e}"))
            .ok();
        tx
    })
}

/// Append to the default store (no-op when no config directory exists).
pub fn append(entry: HistoryEntry) -> Result<(), String> {
    match HistoryStore::open_default() {
        Some(store) => store.append(entry),
        None => Ok(()),
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
