//! OSC 133 / OSC 7 / OSC 7733 parser and command-block extraction.
//!
//! This is a GPUI-free port of `src-tauri/src/osc.rs`: a streaming parser that
//! strips the shell-integration markers from a PTY byte stream while recording
//! where each event fired, plus [`BlockTracker`], which turns that event stream
//! into finished [`CommandBlock`]s for the blocks panel.

use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub enum OscEvent {
    PromptStart,
    PromptEnd,
    OutputStart,
    OutputEnd { exit_code: i32 },
    CommandLine(String),
    Cwd(String),
    /// SpritX-specific (OSC 7733): the shell's alias list, base64-encoded as
    /// newline-separated `name\tvalue` rows. Emitted once at shell startup so
    /// the autocomplete can suggest the user's aliases.
    Aliases(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimedEvent {
    pub event: OscEvent,
    /// Byte position in `FeedResult::passthrough` at which this event fired.
    /// Bytes `[prev_idx .. this.passthrough_idx]` belong to the segment that
    /// preceded this event.
    pub passthrough_idx: usize,
}

/// Output of [`OscParser::feed`]. Contains the chunk minus the OSC 133 markers
/// that are stripped, and the list of events with their byte positions.
/// Non-stripped OSC sequences (including OSC 133;A/B and OSC 7) are preserved
/// in `passthrough` so they reach the screen.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedResult {
    pub passthrough: Vec<u8>,
    pub events: Vec<TimedEvent>,
}

/// Upper bound on a single OSC payload. Generous enough to hold a base64 alias
/// dump (OSC 7733) for a large oh-my-zsh setup, while bounding memory against a
/// genuinely runaway sequence.
pub const MAX_OSC_LEN: usize = 1 << 16; // 64 KiB

/// Streaming parser for OSC 133 (FinalTerm shell integration) sequences.
///
/// Recognises `ESC ] 133 ; X [ ; args ] ST` where ST is BEL (0x07) or `ESC \`.
/// Maintains state across [`feed`](Self::feed) calls so sequences can straddle
/// PTY reads.
pub struct OscParser {
    state: State,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Idle,
    SawEsc,
    InOsc,
    InOscSawEsc,
}

impl Default for OscParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OscParser {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            buf: Vec::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> FeedResult {
        let mut passthrough = Vec::with_capacity(data.len());
        let mut events = Vec::new();
        let mut i = 0;
        while i < data.len() {
            // Hot path: 100% of the stream funnels through this parser, and
            // the dominant case (builds, cat…) is long ESC-free spans — copy
            // them in one memcpy instead of a per-byte state machine walk.
            if let State::Idle = self.state {
                match data[i..].iter().position(|&c| c == 0x1b) {
                    Some(off) => {
                        passthrough.extend_from_slice(&data[i..i + off]);
                        self.state = State::SawEsc;
                        i += off + 1;
                    }
                    None => {
                        passthrough.extend_from_slice(&data[i..]);
                        break;
                    }
                }
                continue;
            }
            let b = data[i];
            match self.state {
                State::Idle => {
                    // Unreachable (handled above); kept as a safe fallback.
                    if b == 0x1b {
                        self.state = State::SawEsc;
                    } else {
                        passthrough.push(b);
                    }
                }
                State::SawEsc => {
                    if b == b']' {
                        self.state = State::InOsc;
                        self.buf.clear();
                    } else {
                        passthrough.push(0x1b);
                        passthrough.push(b);
                        self.state = State::Idle;
                    }
                }
                State::InOsc => {
                    if b == 0x07 {
                        finalize_osc(&self.buf, &mut passthrough, &mut events);
                        self.buf.clear();
                        self.state = State::Idle;
                    } else if b == 0x1b {
                        self.state = State::InOscSawEsc;
                    } else {
                        self.buf.push(b);
                        if self.buf.len() > MAX_OSC_LEN {
                            flush_runaway(&self.buf, &mut passthrough);
                            self.buf.clear();
                            self.state = State::Idle;
                        }
                    }
                }
                State::InOscSawEsc => {
                    if b == b'\\' {
                        finalize_osc(&self.buf, &mut passthrough, &mut events);
                        self.buf.clear();
                        self.state = State::Idle;
                    } else {
                        // Push BOTH bytes before the overflow check so the
                        // current byte isn't lost when the flush triggers.
                        self.buf.push(0x1b);
                        self.buf.push(b);
                        if self.buf.len() > MAX_OSC_LEN {
                            flush_runaway(&self.buf, &mut passthrough);
                            self.buf.clear();
                            self.state = State::Idle;
                        } else {
                            self.state = State::InOsc;
                        }
                    }
                }
            }
            i += 1;
        }
        FeedResult {
            passthrough,
            events,
        }
    }
}

fn finalize_osc(buf: &[u8], passthrough: &mut Vec<u8>, events: &mut Vec<TimedEvent>) {
    let event = parse_osc(buf);
    // OSC 133;A (PromptStart) and 133;B (PromptEnd) are forwarded so the
    // screen can capture their buffer positions:
    //   - A pins the prompt's buffer line for the "click block → scroll" feature.
    //   - B marks the column where the command line begins, which the
    //     autocomplete uses as its anchor.
    // The remaining 133 markers are pure meta — strip them so they don't end up
    // in captured output or render artifacts. OSC 7 (Cwd) is also forwarded.
    let consume = matches!(
        event,
        Some(
            OscEvent::OutputStart
                | OscEvent::OutputEnd { .. }
                | OscEvent::CommandLine(_)
                | OscEvent::Aliases(_)
        )
    );
    if let Some(event) = event {
        events.push(TimedEvent {
            event,
            passthrough_idx: passthrough.len(),
        });
    }
    if !consume {
        passthrough.push(0x1b);
        passthrough.push(b']');
        passthrough.extend_from_slice(buf);
        passthrough.push(0x07);
    }
}

fn flush_runaway(buf: &[u8], passthrough: &mut Vec<u8>) {
    passthrough.push(0x1b);
    passthrough.push(b']');
    passthrough.extend_from_slice(buf);
}

fn parse_osc(buf: &[u8]) -> Option<OscEvent> {
    let s = std::str::from_utf8(buf).ok()?;

    if let Some(rest) = s.strip_prefix("133;") {
        let (kind, payload) = match rest.split_once(';') {
            Some((k, p)) => (k, Some(p)),
            None => (rest, None),
        };
        return match kind {
            "A" => Some(OscEvent::PromptStart),
            "B" => Some(OscEvent::PromptEnd),
            "C" => Some(OscEvent::OutputStart),
            "D" => {
                let code = payload
                    .and_then(|s| s.split(';').next())
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                Some(OscEvent::OutputEnd { exit_code: code })
            }
            "E" => Some(OscEvent::CommandLine(payload.unwrap_or("").to_string())),
            _ => None,
        };
    }

    if let Some(rest) = s.strip_prefix("7733;") {
        return Some(OscEvent::Aliases(rest.to_string()));
    }

    if let Some(rest) = s.strip_prefix("7;") {
        // Format usually: file://hostname/path/to/dir — tolerate plain paths.
        let path = if let Some(after_scheme) = rest.strip_prefix("file://") {
            // Skip the hostname segment (everything before the first '/').
            match after_scheme.find('/') {
                Some(slash) => &after_scheme[slash..],
                None => after_scheme,
            }
        } else {
            rest
        };
        let decoded = normalize_cwd(percent_decode(path));
        return Some(OscEvent::Cwd(decoded));
    }

    None
}

/// `file://host/C:/path` decodes to `/C:/path`; strip the leading slash before
/// a Windows drive letter so it's a usable path. No-op for POSIX paths.
fn normalize_cwd(p: String) -> String {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        p[1..].to_string()
    } else {
        p
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (hex(bytes[i + 1]), hex(bytes[i + 2]));
            if let (Some(a), Some(b)) = h {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A finished command, as shown in the blocks panel.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandBlock {
    /// Command line reported via OSC 133;E, when the shell emitted one.
    pub command: Option<String>,
    /// Raw output bytes captured between OSC 133;C and OSC 133;D, decoded as
    /// lossy UTF-8 (escape sequences are left for the copy path to strip).
    pub output: String,
    /// Exit code from OSC 133;D. `None` when the block was closed without one
    /// (for example on shell exit).
    pub exit_code: Option<i32>,
    /// Working directory at the prompt, tracked from OSC 7.
    pub cwd: Option<String>,
    /// When the command started executing (OSC 133;C), or when the block was
    /// created if no output-start marker ever arrived.
    pub started_at: SystemTime,
}

struct PendingBlock {
    command: Option<String>,
    output: Vec<u8>,
    cwd: Option<String>,
    started_at: SystemTime,
}

impl PendingBlock {
    fn new(cwd: Option<String>) -> Self {
        Self {
            command: None,
            output: Vec::new(),
            cwd,
            started_at: SystemTime::now(),
        }
    }
}

/// Consumes [`OscEvent`]s and assembles [`CommandBlock`]s.
///
/// Feed it the same PTY bytes the parser gets; it forwards the [`FeedResult`]
/// back (so callers can write `passthrough` to the screen) while accumulating
/// output and completed blocks internally. Consecutive prompt-start markers
/// (shell prompt redraws) are deduplicated: a bare prompt that never received a
/// command or output does not produce extra blocks.
pub struct BlockTracker {
    parser: OscParser,
    current: Option<PendingBlock>,
    capturing: bool,
    cwd: Option<String>,
    completed: Vec<CommandBlock>,
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockTracker {
    pub fn new() -> Self {
        Self {
            parser: OscParser::new(),
            current: None,
            capturing: false,
            cwd: None,
            completed: Vec::new(),
        }
    }

    /// Parse a PTY chunk, updating block state. The returned result should be
    /// written to the terminal as usual.
    pub fn feed(&mut self, data: &[u8]) -> FeedResult {
        let result = self.parser.feed(data);
        let mut cursor = 0usize;
        for timed in &result.events {
            if self.capturing {
                self.current
                    .as_mut()
                    .expect("capturing implies a block")
                    .output
                    .extend_from_slice(&result.passthrough[cursor..timed.passthrough_idx]);
            }
            cursor = timed.passthrough_idx;
            self.handle(timed.event.clone());
        }
        if self.capturing {
            self.current
                .as_mut()
                .expect("capturing implies a block")
                .output
                .extend_from_slice(&result.passthrough[cursor..]);
        }
        result
    }

    /// Feed an already-parsed event (useful when a caller owns its own
    /// [`OscParser`]).
    pub fn push_event(&mut self, event: OscEvent) {
        self.handle(event);
    }

    /// Drain the blocks completed since the last call.
    pub fn take_blocks(&mut self) -> Vec<CommandBlock> {
        std::mem::take(&mut self.completed)
    }

    /// Last working directory reported by OSC 7.
    pub fn current_cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Close an in-flight block, e.g. when the shell exits without emitting
    /// OSC 133;D. Returns the flushed block, if any. Unlike a normal completion
    /// it is not added to [`take_blocks`](Self::take_blocks)'s queue.
    pub fn finish(&mut self) -> Option<CommandBlock> {
        self.build_block(None)
    }

    fn ensure_block(&mut self) -> &mut PendingBlock {
        if self.current.is_none() {
            self.current = Some(PendingBlock::new(self.cwd.clone()));
        }
        self.current.as_mut().expect("just created")
    }

    fn handle(&mut self, event: OscEvent) {
        match event {
            OscEvent::PromptStart => {
                let duplicate = self
                    .current
                    .as_ref()
                    .is_some_and(|p| p.command.is_none() && p.output.is_empty() && !self.capturing);
                if !duplicate {
                    // A prompt start without a preceding OutputEnd means the
                    // shell skipped the marker; close the stale block.
                    if self.current.is_some() {
                        self.complete(None);
                    }
                    self.current = Some(PendingBlock::new(self.cwd.clone()));
                }
            }
            OscEvent::PromptEnd => {}
            OscEvent::CommandLine(command) => {
                self.ensure_block().command = Some(command);
            }
            OscEvent::OutputStart => {
                let cwd = self.cwd.clone();
                let block = self.ensure_block();
                block.output.clear();
                block.started_at = SystemTime::now();
                if block.cwd.is_none() {
                    block.cwd = cwd;
                }
                self.capturing = true;
            }
            OscEvent::OutputEnd { exit_code } => {
                self.capturing = false;
                if self.current.is_some() {
                    self.complete(Some(exit_code));
                }
            }
            OscEvent::Cwd(path) => {
                self.cwd = Some(path.clone());
                if let Some(block) = self.current.as_mut() {
                    block.cwd = Some(path);
                }
            }
            OscEvent::Aliases(_) => {}
        }
    }

    fn complete(&mut self, exit_code: Option<i32>) {
        if let Some(block) = self.build_block(exit_code) {
            self.completed.push(block);
        }
    }

    fn build_block(&mut self, exit_code: Option<i32>) -> Option<CommandBlock> {
        self.capturing = false;
        let pending = self.current.take()?;
        Some(CommandBlock {
            command: pending.command,
            output: String::from_utf8_lossy(&pending.output).into_owned(),
            exit_code,
            cwd: pending.cwd.or_else(|| self.cwd.clone()),
            started_at: pending.started_at,
        })
    }
}
