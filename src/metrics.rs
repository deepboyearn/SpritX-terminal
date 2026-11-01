//! Opt-in latency instrumentation.
//!
//! Enable with `SPRITX_PROFILE=1` (off by default, zero cost besides one
//! atomic load per probe). Add `SPRITX_ROWS=1` for the very chatty per-row
//! terminal probes. Every line goes to stderr as the event happens, so a laggy
//! click or a slow keystroke echo can be traced to the exact stage that ate the
//! time.
//!
//! Two line prefixes are used:
//!
//! * `[spritx-trace] t=<ms>` — discrete events in order (input arrived, which
//!   action/button ran, blocks panel contents). These give the *sequence*.
//! * `[spritx-prof] <label> <us>` — durations. These give the *cost*.
//!
//! For a click/keystroke the console reads, in order:
//!
//! ```text
//! [spritx-trace] t=...ms mouse.down x=.. y=..   // press arrived
//! [spritx-prof] mouse.handler            <us>   // time spent in the press handler
//! [spritx-trace] t=...ms action.<name>          // which action the button ran
//! [spritx-prof] clickup->paint           <us>   // queue + dispatch delay (waiting
//!                                             //   for the next display frame)
//! [spritx-prof] render ... build <us> = term + tree + blocks + tabs + other
//! [spritx-prof] elements panes=.. term.rows=.. tree.rows=.. blocks.drawn=..
//! [spritx-prof] clickup->frame.end       <us>   // TOTAL user-visible latency
//! ```
//!
//! Interpretation: a large `->paint` is event-loop/display pacing (nothing the
//! app can cache away); a large `render` is the element build, split by section
//! so you can see whether it is the terminal grid, the file tree or the blocks
//! list scaling up. A large `pump.wait` means PTY output sat waiting for the UI
//! task to be polled, which shows up as terminal echo lag.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static CHECKED: AtomicBool = AtomicBool::new(false);
static ROWS_CHECKED: AtomicBool = AtomicBool::new(false);
static ROWS_ON: AtomicBool = AtomicBool::new(false);

/// A frame build slower than this is flagged; 8 ms is half a 60 Hz frame, so
/// anything above it is already visible as dropped frames / input lag.
const SLOW_FRAME_US: u128 = 8_000;

/// Per-row terminal probes (`row.parse` / `row.build`). Off by default because
/// they print once per grid row (≈ 30–50 lines per frame); set
/// `SPRITX_ROWS=1` together with the profiler when you need to inspect the
/// grid build itself.
pub fn rows_enabled() -> bool {
    if !ROWS_CHECKED.swap(true, Relaxed) {
        ROWS_ON.store(std::env::var_os("SPRITX_ROWS").is_some(), Relaxed);
    }
    ROWS_ON.load(Relaxed)
}

static FRAMES_CHECKED: AtomicBool = AtomicBool::new(false);
static FRAMES_ON: AtomicBool = AtomicBool::new(false);

/// Per-frame summary lines. Off by default because the app renders every vsync
/// now (GPUI's Wayland backend re-requests a frame each time); printing on every
/// frame floods the terminal and can itself add latency. Frames are still
/// reported when an input is being answered or when a frame is slow. Set
/// `SPRITX_FRAMES=1` to log every frame anyway.
pub fn per_frame_enabled() -> bool {
    if !FRAMES_CHECKED.swap(true, Relaxed) {
        FRAMES_ON.store(
            std::env::var_os("SPRITX_FRAMES").is_some()
                || std::env::var_os("SPRITX_VERBOSE").is_some(),
            Relaxed,
        );
    }
    FRAMES_ON.load(Relaxed)
}

/// Force instrumentation on/off (tests).
pub fn set_enabled(value: bool) {
    CHECKED.store(true, Relaxed);
    ENABLED.store(value, Relaxed);
}

/// True when probes should record anything.
pub fn enabled() -> bool {
    if !CHECKED.swap(true, Relaxed) {
        // Debug builds (`cargo run dev`) log by default so the console always
        // shows what a click/keystroke cost. `SPRITX_QUIET=1` silences them;
        // release builds stay quiet unless one of the env vars is set.
        let quiet = std::env::var_os("SPRITX_QUIET").is_some();
        let on = !quiet
            && (cfg!(debug_assertions)
                || std::env::var_os("SPRITX_PROFILE").is_some()
                || std::env::var_os("SPRITX_LOG").is_some()
                || std::env::var_os("SPRITX_TRACE").is_some());
        if on {
            eprintln!(
                "[spritx-prof] profiling ON (debug build)\n            [spritx-prof] clicks/keys print a full latency breakdown; idle frames stay quiet\n            [spritx-prof] SPRITX_FRAMES=1 every frame · SPRITX_ROWS=1 per row · SPRITX_QUIET=1 off"
            );
        }
        ENABLED.store(on, Relaxed);
    }
    ENABLED.load(Relaxed)
}

/// Microseconds of a duration, saturating.
pub fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

/// Immediate one-line probe.
pub fn probe(label: &str, micros: u128) {
    if enabled() {
        print_probe(label, micros);
    }
}

fn print_probe(label: &str, micros: u128) {
    eprintln!("[spritx-prof] {label:<14} {micros:>9} us");
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Milliseconds since the first traced event, for cross-thread ordering.
fn since_start_ms() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// Timestamped discrete trace line. Use for input arrival, dispatch, focus
/// changes and anything that is not a duration, so a click/key can be followed
/// across the UI thread and the worker tasks.
pub fn event(label: &str) {
    if enabled() {
        eprintln!("[spritx-trace] t={:>10.3}ms {label}", since_start_ms());
    }
}

/// RAII scope timer: prints `label` in microseconds when dropped.
pub struct Guard {
    label: &'static str,
    start: Instant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        probe(self.label, micros(self.start.elapsed()));
    }
}

/// Start timing a scope; the probe is emitted on drop when profiling is on.
pub fn guard(label: &'static str) -> Guard {
    Guard {
        label,
        start: Instant::now(),
    }
}

/// Bytes still queued on the tty when the key event was delivered. A big
/// value means the event loop was busy before GPUI could read the key.
#[cfg(target_os = "linux")]
pub fn tty_backlog() -> Option<u128> {
    const TIOCINQ: libc::c_ulong = 0x541B;
    let mut pending: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(0, TIOCINQ, &mut pending as *mut libc::c_int) };
    if rc != 0 || pending <= 0 {
        None
    } else {
        Some(pending as u128)
    }
}

#[cfg(not(target_os = "linux"))]
pub fn tty_backlog() -> Option<u128> {
    None
}

/// Named counters aggregated for the frame being built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    Rows = 0,
    Chars = 1,
    Runs = 2,
    Cells = 3,
    KeyRx = 4,
    KeySent = 5,
    PtyRead = 6,
    PtyWake = 7,
    PumpWait = 8,
    PumpVt = 9,
    PumpOsc = 10,
    PumpBlocks = 11,
    PumpHistory = 12,
    Clicks = 13,
    Panes = 14,
    TreeRows = 15,
    BlocksDrawn = 16,
    RenderCalls = 17,
}

const COUNTERS: [&str; 18] = [
    "line.rows",
    "line.chars",
    "line.runs",
    "line.cells",
    "key.rx",
    "key.sent",
    "pty.read",
    "pty.wake",
    "pump.wait",
    "pump.vt",
    "pump.osc",
    "pump.blocks",
    "pump.history",
    "clicks",
    "panes",
    "tree.rows",
    "blocks.drawn",
    "renders",
];

static COUNTERS_V: [AtomicU64; 18] = [const { AtomicU64::new(0) }; 18];

/// Bump a frame counter.
pub fn add(name: Name, value: u64) {
    if enabled() {
        COUNTERS_V[name as usize].fetch_add(value, Relaxed);
    }
}

/// Record one built terminal line.
pub fn line(cells: u64, chars: u64, runs: u64) {
    add(Name::Rows, 1);
    add(Name::Cells, cells);
    add(Name::Chars, chars);
    add(Name::Runs, runs);
}

/// Read a frame counter.
pub fn count(name: Name) -> u64 {
    COUNTERS_V[name as usize].load(Relaxed)
}

/// Reset every frame counter.
pub fn reset() {
    for counter in &COUNTERS_V {
        counter.store(0, Relaxed);
    }
}

/// Print every counter on one line.
pub fn dump() {
    if !enabled() {
        return;
    }
    let mut line = String::from("[spritx-prof] counters       ");
    for (index, name) in COUNTERS.iter().enumerate() {
        line.push_str(&format!("{name}={} ", COUNTERS_V[index].load(Relaxed)));
    }
    eprintln!("{}", line.trim_end());
}

// --- input -> paint round trip --------------------------------------------

/// When the current input (key or click) was received. Set by the input
/// handlers, consumed by the first frame built afterwards, so the probe shows
/// the *user-visible* delay: "I pressed this, the screen changed this much
/// later".
static PENDING_INPUT: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, &'static str)>>> =
    std::sync::OnceLock::new();

fn pending() -> &'static std::sync::Mutex<Option<(Instant, &'static str)>> {
    PENDING_INPUT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Mark that an input arrived now and will be answered by the next frame.
pub fn input_arrived(label: &'static str) {
    if !enabled() {
        return;
    }
    if let Ok(mut slot) = pending().lock() {
        *slot = Some((Instant::now(), label));
    }
}

/// Close the round trip if an input is waiting; prints `<label>->paint`.
pub fn input_painted() {
    if !enabled() {
        return;
    }
    let taken = pending().lock().ok().and_then(|mut slot| slot.take());
    if let Some((at, label)) = taken {
        print_probe(&format!("{label}->paint"), micros(at.elapsed()));
        if let Ok(mut slot) = frame_input().lock() {
            *slot = Some((at, label));
        }
    }
}

static FRAME_INPUT: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, &'static str)>>> =
    std::sync::OnceLock::new();

fn frame_input() -> &'static std::sync::Mutex<Option<(Instant, &'static str)>> {
    FRAME_INPUT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Frames built since startup, and how many milliseconds the gap between two
/// consecutive frames was — a big gap means the loop was blocked, not that the
/// frame itself was slow.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME: std::sync::OnceLock<std::sync::Mutex<Option<Instant>>> =
    std::sync::OnceLock::new();

fn last_frame() -> &'static std::sync::Mutex<Option<Instant>> {
    LAST_FRAME.get_or_init(|| std::sync::Mutex::new(None))
}

/// Called once per built frame. `first` separates "the app woke up to paint"
/// from "a new input or an animation asked for this frame": only the latter
/// measures a user-visible delay, so quiet gaps are not counted as lag.
pub fn frame_tick(first: bool) -> u128 {
    let now = Instant::now();
    FRAMES.fetch_add(1, Relaxed);
    let previous = last_frame()
        .lock()
        .ok()
        .and_then(|mut slot| slot.replace(now));
    match previous {
        Some(previous) => {
            let gap = micros(now - previous);
            if enabled() && !first && (per_frame_enabled() || gap > 100_000) {
                print_probe("frame.gap", gap);
            }
            gap
        }
        None => 0,
    }
}


/// Total frames built since startup (tests).
pub fn frames_built() -> u64 {
    FRAMES.load(Relaxed)
}

/// Reset frame bookkeeping (tests).
pub fn reset_frames() {
    FRAMES.store(0, Relaxed);
    if let Ok(mut slot) = last_frame().lock() {
        *slot = None;
    }
}

// --- per-frame section timings -------------------------------------------

/// A frame's section timings, in microseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameReport {
    pub build_us: u128,
    pub terminal_us: u128,
    pub file_tree_us: u128,
    pub blocks_us: u128,
    pub tab_bar_us: u128,
    pub panes: u64,
    pub tree_rows: u64,
    pub blocks_drawn: u64,
    pub term_rows: u64,
    pub term_runs: u64,
    pub term_cells: u64,
}

struct Scope {
    start: Option<Instant>,
    terminal: Duration,
    file_tree: Duration,
    blocks: Duration,
    tab_bar: Duration,
}

static SCOPE: std::sync::Mutex<Option<Scope>> = std::sync::Mutex::new(None);

fn scope<R>(f: impl FnOnce(&mut Option<Scope>) -> R) -> R {
    let mut guard = SCOPE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// Begin a frame.
pub fn frame_start() {
    scope(|slot| {
        *slot = Some(Scope {
            start: Some(Instant::now()),
            terminal: Duration::ZERO,
            file_tree: Duration::ZERO,
            blocks: Duration::ZERO,
            tab_bar: Duration::ZERO,
        })
    });
}

/// End a frame, printing the section split and per-frame element counts.
pub fn frame_end() -> FrameReport {
    let taken = scope(std::mem::take);
    let Some(scope) = taken else {
        return FrameReport::default();
    };
    let build_us = scope.start.map(|s| micros(s.elapsed())).unwrap_or(0);
    let report = FrameReport {
        build_us,
        terminal_us: micros(scope.terminal),
        file_tree_us: micros(scope.file_tree),
        blocks_us: micros(scope.blocks),
        tab_bar_us: micros(scope.tab_bar),
        panes: count(Name::Panes),
        tree_rows: count(Name::TreeRows),
        blocks_drawn: count(Name::BlocksDrawn),
        term_rows: count(Name::Rows),
        term_runs: count(Name::Runs),
        term_cells: count(Name::Cells),
    };
    if enabled() {
        // Only print the expensive per-frame summary when it carries
        // information: an input is waiting to be shown, the frame is slow, or
        // per-frame logging was explicitly requested.
        let input = frame_input().lock().ok().and_then(|mut slot| slot.take());
        if let Some((at, label)) = input {
            print_probe(&format!("{label}->frame.end"), micros(at.elapsed()));
        }
        let interesting =
            input.is_some() || report.build_us >= SLOW_FRAME_US || per_frame_enabled();
        if interesting {
            let named = report
                .terminal_us
                .saturating_add(report.file_tree_us)
                .saturating_add(report.blocks_us)
                .saturating_add(report.tab_bar_us);
            let other = report.build_us.saturating_sub(named);
            eprintln!(
                "[spritx-prof] render         build {:>7} us = term {:>6} + tree {:>6} + blocks {:>6} + tabs {:>6} + other {:>6}",
                report.build_us,
                report.terminal_us,
                report.file_tree_us,
                report.blocks_us,
                report.tab_bar_us,
                other,
            );
            eprintln!(
                "[spritx-prof] elements      panes={} term.rows={} term.runs={} term.cells={} tree.rows={} blocks.drawn={} clicks={} key.rx={} key.sent={}",
                report.panes,
                report.term_rows,
                report.term_runs,
                report.term_cells,
                report.tree_rows,
                report.blocks_drawn,
                count(Name::Clicks),
                count(Name::KeyRx),
                count(Name::KeySent),
            );
        }
        if report.build_us >= SLOW_FRAME_US {
            eprintln!(
                "[spritx-prof] SLOW FRAME  {} us (> {} us) — biggest section above",
                report.build_us, SLOW_FRAME_US
            );
        }
    }
    // Per-frame counters: clear after printing so the next frame starts clean.
    reset();
    report
}

fn section<R>(pick: impl FnOnce(&mut Scope) -> &mut Duration, f: impl FnOnce() -> R) -> R {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    scope(|slot| {
        if let Some(scope) = slot.as_mut() {
            *pick(scope) += elapsed;
        }
    });
    out
}

/// Time the terminal grid build.
pub fn terminal<R>(f: impl FnOnce() -> R) -> R {
    section(|scope| &mut scope.terminal, f)
}

/// Time the file-tree panel build.
pub fn file_tree<R>(f: impl FnOnce() -> R) -> R {
    section(|scope| &mut scope.file_tree, f)
}

/// Time the blocks panel build.
pub fn blocks<R>(f: impl FnOnce() -> R) -> R {
    section(|scope| &mut scope.blocks, f)
}

/// Time the tab bar build.
pub fn tab_bar<R>(f: impl FnOnce() -> R) -> R {
    section(|scope| &mut scope.tab_bar, f)
}
