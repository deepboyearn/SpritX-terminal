//! Opt-in memory instrumentation: a counting global allocator that reports, in
//! megabytes, how much heap the process holds overall and which subsystem (and,
//! when enabled, which function) is holding it.
//!
//! It mirrors [`crate::metrics`]'s convention: lines go to stderr with the
//! `[spritx-mem]` prefix, debug builds are on by default, and a single relaxed
//! atomic load is the only cost when disabled.
//!
//! Enablement:
//!
//! * Debug builds (`cargo run`) are **on** from the first allocation so startup
//!   memory is captured too. `SPRITX_QUIET=1` turns everything off.
//! * Release builds are off unless `SPRITX_MEM=1` is set before startup; that
//!   run tracks from `init()` onward (RSS still shows the real total).
//! * `SPRITX_MEM_BACKTRACE=1` adds per-function attribution (a backtrace is
//!   captured on every allocation — much slower, deep-dive only).
//! * `SPRITX_MEM_INTERVAL=<secs>` sets the reporting cadence (default 5 s).
//!
//! What the numbers mean:
//!
//! * `rss` — resident set from `/proc/self/statm`, the true process footprint.
//! * `heap live` — bytes currently reachable through the allocator while
//!   tracking was on (`System` heap only; not GPU/Vulkan allocations).
//! * `peak` — high-water mark of `heap live`.
//! * Per-region lines are `heap live` split by [`Region`]; a region over its
//!   budget is flagged `⚠` so the optimization target is obvious.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Subsystems the allocator attributes live bytes to. `Other` is the catch-all
/// for allocations made outside any [`scope`] — GPUI, wgpu, the std runtime,
/// and any not-yet-tagged code path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Region {
    Scrollback = 0,
    Pending = 1,
    GridCache = 2,
    Osc = 3,
    History = 4,
    FileTree = 5,
    Remote = 6,
    Other = 7,
}

const REGION_COUNT: usize = 8;

const REGION_LABELS: [&str; REGION_COUNT] = [
    "terminal.scrollback",
    "terminal.pending",
    "terminal.grid_cache",
    "terminal.osc",
    "history.commands",
    "screens.file_tree",
    "remote.buffers",
    "other (gpui + deps)",
];

/// Per-region budgets in megabytes. A region whose live bytes exceed its budget
/// is flagged `⚠` — these are the conservative defaults for a single-window app
/// with a few panes; tune them against your own usage.
const REGION_BUDGET_MB: [u64; REGION_COUNT] = [40, 8, 8, 4, 20, 12, 16, u64::MAX];

const SHARDS: usize = 16;

static ENABLED: AtomicBool = AtomicBool::new(cfg!(debug_assertions));
static BACKTRACE: AtomicBool = AtomicBool::new(false);
static INITIALISED: AtomicBool = AtomicBool::new(false);

static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_ALLOCATED: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static REGION_LIVE: [AtomicU64; REGION_COUNT] = [const { AtomicU64::new(0) }; REGION_COUNT];

// Live allocation table, keyed by address (usize, not a raw pointer, so the
// statics are Sync). Region id per pointer is always tracked while enabled;
// the function-key map is only populated in backtrace mode. `Mutex<Option<...>>`
// because `HashMap::new` is not a `const fn`; each shard materialises lazily.
static REG_SHARDS: [Mutex<Option<HashMap<usize, u8>>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
static FUNC_SHARDS: [Mutex<Option<HashMap<usize, u32>>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];

// Interning of function names to small ids, and live bytes/count per id.
static FUNC_TABLE: Mutex<Option<(HashMap<String, u32>, Vec<String>)>> = Mutex::new(None);
static FUNC_LIVE: Mutex<Option<HashMap<u32, (u64, u64)>>> = Mutex::new(None);

thread_local! {
    /// Region the current thread is attributing allocations to. Defaults to
    /// `Other`; [`scope`] pushes a different value for the duration of a guard.
    static CURRENT_REGION: Cell<u8> = const { Cell::new(Region::Other as u8) };
    /// Re-entrancy guard: our own bookkeeping allocates (map inserts, name
    /// interning), and those allocations must not re-enter the tracker (which
    /// would both deadlock on the shard lock and pollute the counts).
    static IN_TRACKER: Cell<bool> = const { Cell::new(false) };
}

fn shard_for(addr: usize) -> usize {
    (addr >> 4) % SHARDS
}

/// True when the allocator should book-keep. A single relaxed load: the wrapper
/// is otherwise a plain `System` forward.
#[inline]
fn tracking() -> bool {
    ENABLED.load(Relaxed)
}

/// RAII guard that attributes allocations on this thread to a [`Region`].
pub struct MemScope {
    saved: u8,
}

impl Drop for MemScope {
    fn drop(&mut self) {
        // `try_with` because a `MemScope` can be dropped during TLS teardown,
        // where `with` would panic; a destroyed cell needs no restore.
        let _ = CURRENT_REGION.try_with(|cell| cell.set(self.saved));
    }
}

/// Attribute every allocation made until the returned guard is dropped to
/// `region`. Nesting is fine: the previous region is restored on drop.
#[must_use]
pub fn scope(region: Region) -> MemScope {
    // `try_with` because this can run during TLS teardown; treat a destroyed
    // cell as `Other` rather than panicking in the allocator path.
    let saved = CURRENT_REGION
        .try_with(|cell| {
            let prev = cell.get();
            cell.set(region as u8);
            prev
        })
        .unwrap_or(Region::Other as u8);
    MemScope { saved }
}

struct NoReenter;
impl NoReenter {
    #[inline]
    fn new() -> Option<NoReenter> {
        // `try_with` because the allocator runs during TLS destruction, where
        // `with` panics; a destroyed cell means "already in tracker" (skip).
        if IN_TRACKER.try_with(|flag| flag.get()).unwrap_or(true) {
            None
        } else {
            let _ = IN_TRACKER.try_with(|flag| flag.set(true));
            Some(NoReenter)
        }
    }
}
impl Drop for NoReenter {
    #[inline]
    fn drop(&mut self) {
        // `try_with` because a `NoReenter` dropped during TLS teardown must not panic.
        let _ = IN_TRACKER.try_with(|flag| flag.set(false));
    }
}

fn record_alloc(ptr: usize, size: usize) {
    let Some(_guard) = NoReenter::new() else {
        return;
    };
    ALLOC_COUNT.fetch_add(1, Relaxed);
    TOTAL_ALLOCATED.fetch_add(size as u64, Relaxed);
    let now = LIVE_BYTES.fetch_add(size as u64, Relaxed) + size as u64;
    PEAK_BYTES.fetch_max(now, Relaxed);

    // `try_with` because this runs on the allocator path during TLS teardown.
    let region = CURRENT_REGION
        .try_with(|cell| cell.get())
        .unwrap_or(Region::Other as u8);
    REGION_LIVE[region as usize % REGION_COUNT].fetch_add(size as u64, Relaxed);
    if let Ok(mut shard) = REG_SHARDS[shard_for(ptr)].lock() {
        shard.get_or_insert_with(HashMap::new).insert(ptr, region);
    }

    if BACKTRACE.load(Relaxed) {
        let name = capture_function();
        record_alloc_func(ptr, size, &name);
    }
}

fn record_alloc_func(ptr: usize, size: usize, name: &str) {
    let id = intern(name);
    adjust_func_live(id, size as u64, true);
    if let Ok(mut shard) = FUNC_SHARDS[shard_for(ptr)].lock() {
        shard.get_or_insert_with(HashMap::new).insert(ptr, id);
    }
}

fn intern(name: &str) -> u32 {
    if let Ok(mut slot) = FUNC_TABLE.lock() {
        let (index, names) = slot.get_or_insert_with(|| (HashMap::new(), Vec::new()));
        if let Some(id) = index.get(name) {
            return *id;
        }
        let id = names.len() as u32;
        names.push(name.to_string());
        index.insert(name.to_string(), id);
        return id;
    }
    u32::MAX
}

fn adjust_func_live(id: u32, bytes: u64, add: bool) {
    if id == u32::MAX {
        return;
    }
    if let Ok(mut slot) = FUNC_LIVE.lock() {
        let map = slot.get_or_insert_with(HashMap::new);
        let entry = map.entry(id).or_insert((0, 0));
        if add {
            entry.0 = entry.0.saturating_add(bytes);
            entry.1 = entry.1.saturating_add(1);
        } else {
            entry.0 = entry.0.saturating_sub(bytes);
            entry.1 = entry.1.saturating_sub(1);
        }
    }
}

fn record_dealloc(ptr: usize, size: usize) {
    let Some(_guard) = NoReenter::new() else {
        return;
    };
    // Only adjust for blocks we actually counted at allocation time; an
    // untracked pointer (allocated before tracking began) must not subtract.
    let region = REG_SHARDS[shard_for(ptr)]
        .lock()
        .ok()
        .and_then(|mut shard| shard.as_mut().and_then(|map| map.remove(&ptr)));
    let Some(region) = region else {
        return;
    };
    clamp_sub(&LIVE_BYTES, size as u64);
    clamp_sub(&REGION_LIVE[region as usize % REGION_COUNT], size as u64);

    if BACKTRACE.load(Relaxed) {
        if let Some(id) = FUNC_SHARDS[shard_for(ptr)]
            .lock()
            .ok()
            .and_then(|mut shard| shard.as_mut().and_then(|map| map.remove(&ptr)))
        {
            adjust_func_live(id, size as u64, false);
        }
    }
}

/// Re-key a tracked block whose address changed (or whose size grew/shrank) via
/// `realloc`. If the original block was never tracked, nothing is done so the
/// counts stay consistent with what we observed at allocation time.
fn record_realloc(old: usize, old_size: usize, new: usize, new_size: usize) {
    let Some(_guard) = NoReenter::new() else {
        return;
    };
    let region = match REG_SHARDS[shard_for(old)].lock() {
        Ok(mut shard) => shard.as_mut().and_then(|map| map.remove(&old)),
        Err(_) => None,
    };
    let Some(region) = region else {
        return;
    };
    let delta = new_size as i64 - old_size as i64;
    if delta >= 0 {
        let now = LIVE_BYTES.fetch_add(delta as u64, Relaxed) + delta as u64;
        PEAK_BYTES.fetch_max(now, Relaxed);
        REGION_LIVE[region as usize % REGION_COUNT].fetch_add(delta as u64, Relaxed);
    } else {
        clamp_sub(&LIVE_BYTES, (-delta) as u64);
        clamp_sub(&REGION_LIVE[region as usize % REGION_COUNT], (-delta) as u64);
    }
    if let Ok(mut shard) = REG_SHARDS[shard_for(new)].lock() {
        shard.get_or_insert_with(HashMap::new).insert(new, region);
    }

    if BACKTRACE.load(Relaxed) {
        if let Some(id) = FUNC_SHARDS[shard_for(old)]
            .lock()
            .ok()
            .and_then(|mut shard| shard.as_mut().and_then(|map| map.remove(&old)))
        {
            adjust_func_live(id, delta.unsigned_abs(), delta >= 0);
            if let Ok(mut shard) = FUNC_SHARDS[shard_for(new)].lock() {
                shard.get_or_insert_with(HashMap::new).insert(new, id);
            }
        }
    }
}

fn clamp_sub(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Relaxed, Relaxed, |current| {
        Some(current.saturating_sub(value))
    });
}

/// Walk the captured backtrace to the first frame belonging to this crate and
/// return that symbol, so deep-dive attribution names SpritX functions rather
/// than allocator/iterator internals.
fn capture_function() -> String {
    let bt = std::backtrace::Backtrace::force_capture();
    let text = bt.to_string();
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_start();
        // Skip the `<Backtrace captured...>` preamble and index noise.
        if line.starts_with("0x") || line.starts_with('<') || line.is_empty() {
            continue;
        }
        // Frame lines look like: `   0: crate::path::function` — take the symbol.
        if let Some(sym) = line.split_once(": ") {
            let sym = sym.1.trim();
            if fallback.is_none() {
                fallback = Some(sym.to_string());
            }
            if sym.contains("spritx_terminal::") {
                return sym.to_string();
            }
        }
    }
    fallback.unwrap_or_else(|| "unknown".to_string())
}

/// The counting global allocator. Every path forwards to `System`; book-keeping
/// happens only while [`tracking`] is true and not already inside the tracker.
pub struct TrackingAlloc;

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() && tracking() {
            record_alloc(ptr as usize, layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() && tracking() {
            record_alloc(ptr as usize, layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() && tracking() {
            record_realloc(ptr as usize, layout.size(), new_ptr as usize, new_size);
        }
        new_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if tracking() {
            record_dealloc(ptr as usize, layout.size());
        }
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

static START: OnceLock<Instant> = OnceLock::new();

fn started() -> Instant {
    *START.get_or_init(Instant::now)
}

/// Resident-set bytes from `/proc/self/statm` (field 2 = resident pages).
#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    };
    Some(resident_pages * page)
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
    None
}

const MB: f64 = 1024.0 * 1024.0;

fn mb(bytes: u64) -> f64 {
    bytes as f64 / MB
}

/// Build one memory report into `out`. `alloc_rate` is allocations since the
/// previous report divided by the elapsed seconds, shown on the summary line.
fn write_report(out: &mut String) {
    let elapsed = started().elapsed().as_secs_f64().max(0.001);
    let allocs = ALLOC_COUNT.load(Relaxed);
    let rate = allocs as f64 / elapsed;

    out.push_str(&format!(
        "[spritx-mem] t={:>6.1}s  rss {:>7.1} MB | heap live {:>7.1} MB (peak {:>7.1} MB) | allocs {:.1}M ({:.0}/s)\n",
        elapsed,
        mb(rss_bytes().unwrap_or(0)),
        mb(LIVE_BYTES.load(Relaxed)),
        mb(PEAK_BYTES.load(Relaxed)),
        allocs as f64 / 1_000_000.0,
        rate,
    ));

    // Regions sorted by live bytes, largest first.
    let mut rows: Vec<(u64, usize)> = (0..REGION_COUNT)
        .map(|i| (REGION_LIVE[i].load(Relaxed), i))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    for (bytes, id) in rows {
        if bytes == 0 {
            continue;
        }
        let budget = REGION_BUDGET_MB[id];
        // The `!= u64::MAX` check must short-circuit first; the multiply would
        // otherwise overflow for the sentinel `Other` budget.
        let flag = if budget != u64::MAX && bytes > budget * (MB as u64) {
            format!("   ⚠ over {} MB budget", budget)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "[spritx-mem]   {:<22} {:>7.2} MB{}\n",
            REGION_LABELS[id],
            mb(bytes),
            flag,
        ));
    }

    if BACKTRACE.load(Relaxed) {
        write_function_report(out);
    }
}

fn write_function_report(out: &mut String) {
    // Take each lock in turn rather than holding both, so the report path never
    // nests a lock the allocation path also reaches.
    let names: Vec<String> = FUNC_TABLE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|(_, names)| names.clone()))
        .unwrap_or_default();
    let entries: Vec<(u32, u64, u64)> = FUNC_LIVE
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|map| map.iter().map(|(id, (bytes, count))| (*id, *bytes, *count)).collect())
        })
        .unwrap_or_default();

    let mut funcs = entries;
    funcs.retain(|(_, bytes, _)| *bytes > 0);
    if funcs.is_empty() {
        return;
    }
    funcs.sort_by(|a, b| b.1.cmp(&a.1));

    out.push_str("[spritx-mem] top allocating functions (live):\n");
    for (id, bytes, count) in funcs.iter().take(20) {
        let name = names.get(*id as usize).map(String::as_str).unwrap_or("?");
        out.push_str(&format!(
            "[spritx-mem]   {:>8.2} MB  {:>7} allocs  {}\n",
            mb(*bytes),
            count,
            name,
        ));
    }
}

/// Print a report to stderr. Cheap mode (regions only) and backtrace mode share
/// this path; nothing is printed when tracking is off.
fn report_line() {
    if !tracking() {
        return;
    }
    let mut buf = String::new();
    write_report(&mut buf);
    eprint!("{buf}");
}

/// Resolve the `SPRITX_MEM*` environment and start the periodic reporter. Call
/// once, as the very first thing in `main`, so the reporter thread sees the
/// allocator from the start. Idempotent: later calls do nothing.
pub fn init() {
    if INITIALISED.swap(true, Relaxed) {
        return;
    }

    if std::env::var_os("SPRITX_QUIET").is_some() {
        ENABLED.store(false, Relaxed);
        return;
    }
    if std::env::var_os("SPRITX_MEM").is_some() {
        ENABLED.store(true, Relaxed);
    }
    if std::env::var_os("SPRITX_MEM_BACKTRACE").is_some() {
        BACKTRACE.store(true, Relaxed);
    }

    if !tracking() {
        return;
    }

    let interval = std::env::var("SPRITX_MEM_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(5);

    eprintln!(
        "[spritx-mem] memory profiling ON ({}) · interval {}s · SPRITX_MEM_BACKTRACE=1 for per-function · SPRITX_QUIET=1 off",
        if cfg!(debug_assertions) { "debug build" } else { "SPRITX_MEM=1" },
        interval
    );

    thread::Builder::new()
        .name("spritx-mem".to_string())
        .spawn(move || loop {
            thread::sleep(Duration::from_secs(interval));
            report_line();
        })
        .ok();
}

/// Print a final full report. Called after the app exits.
pub fn report_now() {
    report_line();
}
