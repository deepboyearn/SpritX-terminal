//! Left-hand file tree sidebar.
//!
//! Visual/interaction parity target is `src/FileTree.tsx` + App.css `.file-*`
//! (see docs/gpui-inventory/04-panels-overlays.md). The tree is backed by the
//! real filesystem through [`crate::backend::files`], rooted at the active
//! pane's cwd (falling back to the process cwd, then `$HOME`).
//!
//! Expansion, hidden-file and context-menu state use a dedicated app slot,
//! separate from shortcut recording.
//! The loaded directory tree is cached in a module-level slot. A miss kicks
//! off a load on GPUI's background executor so rendering never blocks on
//! `read_dir`; the result is published back through the cache slot, where a
//! stale load (root, hidden, refresh or expansion changed under it) is
//! dropped instead of applied.
//!
//! Unlike the old manual-refresh-only tree, a bounded polling watcher samples
//! the rendered directories and debounces filesystem bursts into one `refresh`
//! bump. Its `read_dir` calls run on the same background executor as loads, so
//! render never blocks on filesystem I/O. No watcher thread, file descriptor,
//! or OS watch remains alive while the panel is hidden or collapsed.

use std::cell::Cell;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    canvas, div, prelude::*, px, uniform_list, AnyElement, Context, CursorStyle, DragMoveEvent,
    FontWeight, IntoElement, MouseButton, Pixels, Render, Window,
};

use crate::backend::files::{expand_tilde, home_dir, list_dir, FsEntry};
use crate::backend::git::{git_path_statuses, GitPathStatus};
use crate::screens::app_shell::SpritXApp;
use crate::state::{PaletteMode, TabState};
use crate::components::icons::{Icon, icon};
use crate::theme::Theme;

const FT_PREFIX: &str = "filetree-ui";
const FT_SEP: char = '\u{1f}';
const FT_LIST_SEP: char = '\u{1e}';

/// Maximum number of nested directory levels loaded below the root. High
/// enough that normal projects never hit it; the expansion set is what
/// actually bounds the work, so this is just a safety net.
const MAX_DEPTH: usize = 32;
/// Total entry budget for one resolved tree (root + expanded descendants).
const ENTRY_CAP: usize = 2000;
/// Poll cadence for the directory sampler. Short enough that a save from the
/// adjacent terminal appears near-instantly, long enough that a small tree
/// costs no perceptible background work.
const WATCH_POLL: Duration = Duration::from_millis(200);
/// Quiet interval required before a changed fingerprint is published: edit
/// bursts collapse into one refresh instead of a storm of reloads.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// Shortest time a load may spend scanning directories after the watcher asks
/// for a refresh, preventing an immediate retrigger while that scan is I/O
/// bound.
const WATCH_COOLDOWN: Duration = Duration::from_millis(300);
/// Bounded directory-entry budget for each fingerprint, mirroring the loader's
/// expansion bound and keeping polling work finite for very large trees.
const WATCH_ENTRY_CAP: usize = ENTRY_CAP;

#[derive(Default, Clone)]
struct FtUi {
    expanded: Vec<String>,
    ctx: Option<(String, bool)>,
    show_hidden: bool,
    refresh: u64,
}

/// Escapes a persistence field so the U+001E/U+001F separators can never
/// appear inside it (a POSIX filename may legally contain those bytes);
/// strings without them are returned unchanged, keeping old values valid.
fn ft_esc_field(field: &str) -> String {
    if !field.contains(['\\', FT_SEP, FT_LIST_SEP]) {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    for ch in field.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            sep if sep == FT_SEP => out.push_str("\\i"),
            sep if sep == FT_LIST_SEP => out.push_str("\\j"),
            _ => out.push(ch),
        }
    }
    out
}

fn ft_unesc_field(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('i') => out.push(FT_SEP),
            Some('j') => out.push(FT_LIST_SEP),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn ft_unesc_list(field: &str) -> Vec<String> {
    field
        .split(FT_LIST_SEP)
        .filter(|p| !p.is_empty())
        .map(ft_unesc_field)
        .collect()
}

fn ft_ui(app: &SpritXApp) -> FtUi {
    let Some(raw) = app.file_tree_ui.as_deref() else {
        return FtUi::default();
    };
    let mut parts = raw.split(FT_SEP);
    if parts.next() != Some(FT_PREFIX) {
        return FtUi::default();
    }
    let expanded = parts.next().map(ft_unesc_list).unwrap_or_default();
    let ctx_path = parts.next().map(ft_unesc_field).unwrap_or_default();
    let ctx_dir = parts.next() == Some("1");
    let show_hidden = parts.next() == Some("1");
    let refresh = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    FtUi {
        expanded,
        ctx: if ctx_path.is_empty() {
            None
        } else {
            Some((ctx_path, ctx_dir))
        },
        show_hidden,
        refresh,
    }
}

fn ft_encode(ui: &FtUi) -> Option<String> {
    if ui.expanded.is_empty() && ui.ctx.is_none() && !ui.show_hidden && ui.refresh == 0 {
        return None;
    }
    let expanded = ui
        .expanded
        .iter()
        .map(|p| ft_esc_field(p))
        .collect::<Vec<_>>()
        .join(&FT_LIST_SEP.to_string());
    let (ctx_path, ctx_dir) = ui
        .ctx
        .as_ref()
        .map(|(path, dir)| (ft_esc_field(path), *dir))
        .unwrap_or_default();
    Some(format!(
        "{FT_PREFIX}{FT_SEP}{expanded}{FT_SEP}{ctx_path}{FT_SEP}{}{FT_SEP}{}{FT_SEP}{}",
        u8::from(ctx_dir),
        u8::from(ui.show_hidden),
        ui.refresh
    ))
}

#[derive(Clone)]
struct FtNode {
    name: String,
    /// Expanded real path, used for actions and expansion keys.
    path: String,
    is_dir: bool,
    symlink: bool,
    /// Depth below the root; root children are level 0.
    level: usize,
    children: Loaded,
}

#[derive(Clone)]
enum Loaded {
    NotLoaded,
    Ok(DirLoad),
    Err(String),
}

#[derive(Clone)]
struct DirLoad {
    nodes: Vec<FtNode>,
    truncated: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FtCacheKey {
    root: String,
    show_hidden: bool,
    refresh: u64,
    expanded_key: String,
}

#[derive(Clone)]
struct FtCache {
    key: FtCacheKey,
    state: Result<Arc<DirLoad>, String>,
}

/// A cache key fully identifies one tree: root path, hidden toggle, refresh
/// counter and the expanded set. A load for a different key is stale and
/// must be dropped on arrival, never applied.
fn ft_cache_key(root: &Path, show_hidden: bool, refresh: u64, expanded: &[String]) -> FtCacheKey {
    FtCacheKey {
        root: root.to_string_lossy().into_owned(),
        show_hidden,
        refresh,
        // Unit separator so two entries can never blur into one key.
        expanded_key: expanded.join("\u{1d}"),
    }
}

/// Per-row git status shown in the tree. Collapsed to one letter for the
/// compact badge; colors map to existing theme tokens in `file_tree_row`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitBadge {
    Modified,
    Added,
    Deleted,
    Untracked,
}

impl GitBadge {
    fn from_status(status: GitPathStatus) -> Self {
        match status {
            GitPathStatus::Modified => Self::Modified,
            GitPathStatus::Added => Self::Added,
            GitPathStatus::Deleted => Self::Deleted,
            GitPathStatus::Untracked => Self::Untracked,
        }
    }
}

/// One refreshed git snapshot: absolute-path badges from the tree root's repo,
/// plus the absolute paths of nested repository roots found below it.
///
/// WHY nested roots matter as much as the badge map: `git status` in a parent
/// repository reports an embedded repository as a single summary entry, so a
/// file inside a nested repo would otherwise inherit the parent's view.
/// Entries under a nested root are excluded from the parent map; their own
/// statuses are merged in instead, so every badge comes from the repository
/// that actually owns the path.
struct GitStatuses {
    badges: std::collections::HashMap<PathBuf, GitBadge>,
    nested_roots: Vec<PathBuf>,
}

impl GitStatuses {
    /// The badge owning `path`, or `None` for a clean file. Untracked
    /// directories are the one place git reports a parent instead of children
    /// (porcelain-v2 omits `?` entries under a wholly-untracked directory), so
    /// an exact hit on an ancestor directory still counts — but never across
    /// a nested repository boundary.
    fn badge_for(&self, path: &Path) -> Option<GitBadge> {
        // Innermost first: a path under several nested roots belongs to the
        // deepest one. `nested_roots` is built deepest-first, so the first
        // containing root wins.
        for root in &self.nested_roots {
            if path.starts_with(root) {
                return self
                    .badges
                    .get(path)
                    .copied()
                    .or_else(|| self.ancestor_untracked(path, root));
            }
        }
        self.badges
            .get(path)
            .copied()
            .or_else(|| self.ancestor_untracked(path, Path::new("")))
    }

    /// Untracked-directory fallback: the nearest ancestor with a `?` entry.
    /// The walk stops at `boundary` (a nested root, or the empty path for the
    /// parent repository) so a parent's untracked entry can never leak into a
    /// nested repository.
    fn ancestor_untracked(&self, path: &Path, boundary: &Path) -> Option<GitBadge> {
        for ancestor in path.ancestors().skip(1) {
            if !boundary.as_os_str().is_empty() && !ancestor.starts_with(boundary) {
                break;
            }
            if let Some(&badge) = self.badges.get(ancestor) {
                if badge == GitBadge::Untracked {
                    return Some(badge);
                }
            }
        }
        None
    }

    /// Whether `path` is a nested repository root itself. The row for such a
    /// directory is badged by its own repository's dirtiness, handled by the
    /// caller folding `GitBadge` onto the row; this only marks the boundary.
    fn is_nested_root(&self, path: &Path) -> bool {
        self.nested_roots.iter().any(|root| root == path)
    }
}

fn git_statuses_slot() -> &'static Mutex<Option<(FtCacheKey, Arc<GitStatuses>)>> {
    static SLOT: LazyLock<Mutex<Option<(FtCacheKey, Arc<GitStatuses>)>>> =
        LazyLock::new(|| Mutex::new(None));
    &SLOT
}

fn git_statuses_for(key: &FtCacheKey) -> Option<Arc<GitStatuses>> {
    let guard = git_statuses_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .filter(|(cached_key, _)| cached_key == key)
        .map(|(_, statuses)| Arc::clone(statuses))
}

enum TreeView {
    Ready(Result<Arc<DirLoad>, String>),
    Pending(Option<Result<Arc<DirLoad>, String>>),
}

enum LoadDecision {
    Reuse(Result<Arc<DirLoad>, String>),
    Pending(Option<Result<Arc<DirLoad>, String>>),
    Spawn,
}

fn cache_slot() -> &'static Mutex<Option<FtCache>> {
    static SLOT: LazyLock<Mutex<Option<FtCache>>> = LazyLock::new(|| Mutex::new(None));
    &SLOT
}

/// Keys with a load already in flight, one entry per distinct cache key, so
/// consecutive renders never double-spawn the same directory scan.
fn in_flight_slot() -> &'static Mutex<std::collections::HashSet<FtCacheKey>> {
    static SLOT: LazyLock<Mutex<std::collections::HashSet<FtCacheKey>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
    &SLOT
}

/// Exact-key hit, same-root previous tree for painting while a load runs,
/// or a signal to spawn the load. Render never reads the filesystem.
fn load_decision(
    root: &Path,
    show_hidden: bool,
    refresh: u64,
    expanded: &[String],
) -> LoadDecision {
    let key = ft_cache_key(root, show_hidden, refresh, expanded);
    let guard = cache_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cache) = guard.as_ref() {
        if cache.key == key {
            return LoadDecision::Reuse(cache.state.clone());
        }
    }
    // Same root but no exact key yet (expansion/hidden changed): keep
    // showing the previous tree instead of flashing a skeleton.
    let previous = guard
        .as_ref()
        .filter(|cache| cache.key.root == key.root)
        .map(|cache| cache.state.clone());
    drop(guard);
    let in_flight = in_flight_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if in_flight.contains(&key) {
        LoadDecision::Pending(previous)
    } else {
        LoadDecision::Spawn
    }
}

/// Publishes `state` under `key` only while that key is still the one the
/// app wants (root/hidden/refresh/expansion all unchanged). A stale
/// result is discarded; the newer key's own load will arrive separately.
fn publish_load(key: FtCacheKey, state: Result<Arc<DirLoad>, String>, want: FtCacheKey) -> bool {
    if key != want {
        return false;
    }
    let mut guard = cache_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(FtCache { key, state });
    true
}

fn io_note(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        io::ErrorKind::NotFound => "Folder no longer exists".to_string(),
        _ => format!("Unable to read folder: {err}"),
    }
}

fn read_dir(path: &Path, show_hidden: bool) -> Result<Vec<FsEntry>, String> {
    list_dir(path, show_hidden).map_err(|err| io_note(&err))
}

fn load_children(
    dir: &Path,
    show_hidden: bool,
    expanded: &[String],
    level: usize,
    budget: &mut usize,
) -> Result<DirLoad, String> {
    let _mem = crate::memprof::scope(crate::memprof::Region::FileTree);
    let entries = read_dir(dir, show_hidden)?;
    let mut nodes = Vec::new();
    let mut truncated = false;

    for entry in entries {
        if *budget == 0 {
            truncated = true;
            break;
        }
        *budget -= 1;

        let path = entry.path.to_string_lossy().into_owned();
        let is_open = entry.is_dir
            && !entry.symlink
            && level < MAX_DEPTH
            && expanded.iter().any(|p| p == &path);

        let mut node = FtNode {
            name: entry.name,
            path: path.clone(),
            is_dir: entry.is_dir,
            symlink: entry.symlink,
            level,
            children: Loaded::NotLoaded,
        };

        if is_open {
            node.children =
                match load_children(Path::new(&path), show_hidden, expanded, level + 1, budget) {
                    Ok(children) => Loaded::Ok(children),
                    Err(note) => Loaded::Err(note),
                };
        }

        nodes.push(node);
    }

    Ok(DirLoad { nodes, truncated })
}

fn load_tree(root: &Path, show_hidden: bool, expanded: &[String]) -> Result<DirLoad, String> {
    let mut budget = ENTRY_CAP;
    load_children(root, show_hidden, expanded, 0, &mut budget)
}

/// Identity of the directory set the sampler is guarding. The refresh counter
/// is intentionally excluded: a manual or watcher-driven refresh must not
/// cancel the watcher that caused it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FtWatchKey {
    root: String,
    show_hidden: bool,
    expanded_key: String,
}

/// Shared, bounded sampler state. It is the only filesystem data the watcher
/// task owns.
#[derive(Default)]
struct FtWatchState {
    baseline: Option<String>,
    last_refresh: Option<Instant>,
}

/// The single active watcher slot. A new key takes over the slot and bumps the
/// generation, so an old debounce/poller task notices and exits instead of
/// refreshing the new tree.
#[derive(Default)]
struct FtWatcher {
    key: Option<FtWatchKey>,
    generation: u64,
    state: Option<FtWatchState>,
}

fn ft_watcher_slot() -> &'static Mutex<FtWatcher> {
    static SLOT: LazyLock<Mutex<FtWatcher>> = LazyLock::new(|| Mutex::new(FtWatcher::default()));
    &SLOT
}

fn ft_watch_key(root: &Path, show_hidden: bool, expanded: &[String]) -> FtWatchKey {
    FtWatchKey {
        root: root.to_string_lossy().into_owned(),
        show_hidden,
        expanded_key: expanded.join("\u{1d}"),
    }
}

/// Stop the sampler without starting work. Used when the whole panel is not
/// rendered (sidebar hidden, file list collapsed, or no live leaf remains).
fn stop_ft_watcher() {
    let mut guard = ft_watcher_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.key.is_some() {
        guard.key = None;
        guard.state = None;
        guard.generation = guard.generation.wrapping_add(1);
    }
}

fn ft_watcher_is_current(generation: u64) -> bool {
    ft_watcher_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .generation
        == generation
}

/// The single active watcher baseline. One short lock replaces a second
/// generation read on the polling path.
fn ft_watcher_baseline() -> Option<String> {
    ft_watcher_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .state
        .as_ref()
        .and_then(|state| state.baseline.clone())
}

/// Elapsed time since the last watcher-driven refresh, shared across the poll
/// task and the load task so a burst cannot retrigger the loader while a
/// previous refresh is still scanning.
fn ft_watcher_last_refresh_elapsed() -> Option<Duration> {
    ft_watcher_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .state
        .as_ref()
        .and_then(|state| state.last_refresh)
        .map(|instant| instant.elapsed())
}

/// Record the fingerprint that produced the published tree. It will be sampled
/// on the background executor, alongside the load itself, so the first poll
/// after the load has a baseline from the same point in time.
fn note_ft_tree_loaded(key: &FtWatchKey, fingerprint: String) {
    let mut guard = ft_watcher_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.key.as_ref() == Some(key) {
        if let Some(state) = guard.state.as_mut() {
            state.baseline = Some(fingerprint);
        }
    }
}

/// Hash one path/value pair without allocations in the fingerprint path.
fn hash_ft_value(hash: &mut u64, scheme: u64, value: u64) {
    let mut x = *hash ^ scheme ^ value.wrapping_mul(0x9e3779b97f4a7c15);
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 29;
    *hash = x;
}

fn ft_last_modified(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

/// Fold one directory's visible children into the fingerprint. It mirrors
/// `load_children`'s hidden/depth/expansion rules (symlink directories are not
/// followed), but never descends into a directory the loader would not load.
fn fingerprint_children(
    dir: &Path,
    show_hidden: bool,
    expanded: &[String],
    level: usize,
    budget: &mut usize,
    hash: &mut u64,
) {
    let _mem = crate::memprof::scope(crate::memprof::Region::FileTree);
    let mut entries = match list_dir(dir, show_hidden) {
        Ok(entries) => entries,
        Err(_) => {
            // A directory that now errors (deleted parent, revoked
            // permissions) must change the fingerprint, or the tree would
            // stay stale.
            hash_ft_value(hash, 0x01, 1);
            return;
        }
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    hash_ft_value(hash, 0x02, entries.len() as u64);
    for entry in &entries {
        if *budget == 0 {
            // Past the budget the fingerprint degrades to a truncated marker
            // rather than unbounded work.
            hash_ft_value(hash, 0x03, 1);
            return;
        }
        *budget -= 1;
        for chunk in entry
            .path
            .as_os_str()
            .to_string_lossy()
            .as_bytes()
            // One u64 holds eight bytes. A larger chunk shifts by >= 64
            // on normal paths, panicking in debug and aliasing bytes in release.
            .chunks(std::mem::size_of::<u64>())
        {
            let mut value = 0u64;
            for (i, &byte) in chunk.iter().enumerate() {
                value |= (byte as u64) << (8 * i);
            }
            hash_ft_value(hash, 0x05, value);
        }
        if entry.is_dir {
            hash_ft_value(hash, 0x06, 1);
        }
        if let Ok(metadata) = std::fs::metadata(&entry.path) {
            if let Some(modified) = ft_last_modified(&metadata) {
                if let Ok(since) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                    hash_ft_value(hash, 0x07, since.as_nanos() as u64);
                    hash_ft_value(hash, 0x08, (since.as_nanos() >> 64) as u64);
                }
            }
            hash_ft_value(hash, 0x09, metadata.len());
        } else {
            hash_ft_value(hash, 0x0a, 1);
        }
        let is_open = entry.is_dir
            && !entry.symlink
            && level < MAX_DEPTH
            && expanded.iter().any(|p| p == &entry.path.to_string_lossy());
        if is_open {
            fingerprint_children(&entry.path, show_hidden, expanded, level + 1, budget, hash);
        }
    }
}

/// A 64-bit, order-sensitive fingerprint of every directory the tree currently
/// renders. Bounded by WATCH_ENTRY_CAP, exactly like the loader is bounded by
/// ENTRY_CAP. Sampled only on the background executor.
fn fingerprint_tree(root: &Path, show_hidden: bool, expanded: &[String]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64 ^ 0x243f6a8885a308d3;
    let mut budget = WATCH_ENTRY_CAP;
    // The root's own metadata catches root replacement even when its children
    // changed underneath it.
    match std::fs::metadata(root) {
        Ok(metadata) => {
            hash_ft_value(&mut hash, 0x10, metadata.len());
            if let Some(modified) = ft_last_modified(&metadata) {
                if let Ok(since) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                    hash_ft_value(&mut hash, 0x11, since.as_nanos() as u64);
                    hash_ft_value(&mut hash, 0x12, (since.as_nanos() >> 64) as u64);
                }
            }
        }
        Err(_) => hash_ft_value(&mut hash, 0x13, 1),
    }
    fingerprint_children(root, show_hidden, expanded, 0, &mut budget, &mut hash);
    hash
}

/// Start exactly one debounce/poller task for the current watch key. Repeated
/// renders are no-ops. A key change bumps the generation and detaches the old
/// task, which is also how a collapse stops the loop.
fn maybe_start_watcher(root: &Path, ui: &FtUi, cx: &mut Context<SpritXApp>) {
    let key = ft_watch_key(root, ui.show_hidden, &ui.expanded);
    let generation = {
        let mut guard = ft_watcher_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.key.as_ref() == Some(&key) {
            return;
        }
        guard.key = Some(key.clone());
        guard.state = Some(FtWatchState::default());
        guard.generation = guard.generation.wrapping_add(1);
        guard.generation
    };
    let watch_root = root.to_path_buf();
    let show_hidden = ui.show_hidden;
    let expanded = ui.expanded.clone();
    cx.spawn(async move |this, cx| {
        let executor = cx.background_executor().clone();
            // Wake only on the poll/debounce cadence; generation checks decide
            // whether the wake is still current.
            let mut next_poll = Instant::now() + WATCH_POLL;
            loop {
                // Wait until the next poll; sleep in short slices so a key change
                // stops us promptly.
                let now = Instant::now();
                if next_poll > now {
                    gpui::Timer::after(next_poll - now).await;
                }
                if !ft_watcher_is_current(generation) {
                    return;
                }
                // Raw filesystem sampling runs on the background executor, so the
                // render thread never blocks on read_dir. Baseline is the
                // fingerprint of the tree exactly as the last completed load
                // published it; no baseline yet means the first sample is only
                // establishing stability, not firing.
                if ft_watcher_baseline().is_none() {
                    next_poll = Instant::now() + WATCH_POLL;
                    continue;
                }
                let sample_root = watch_root.clone();
                let sample_expanded = expanded.clone();
                let sample = executor
                    .spawn(async move {
                        fingerprint_tree(&sample_root, show_hidden, &sample_expanded).to_string()
                    })
                    .await;
                // Compare against the published baseline, not the last local
                // sample: a change since that load is what must refresh, and
                // consecutive equal polls stay quiet.
                let baseline = ft_watcher_baseline();
                if baseline.as_deref() != Some(sample.as_str()) {
                    // A burst is in flight; the newest sample is the only one that
                    // matters. A previous refresh's cooldown shields a new scan.
                    if ft_watcher_last_refresh_elapsed() < Some(WATCH_COOLDOWN) {
                        next_poll = Instant::now() + WATCH_POLL;
                        continue;
                    }
                    // Debounce: wait for a quiet interval, re-sample, and confirm
                    // that no further writes landed during it.
                    gpui::Timer::after(WATCH_DEBOUNCE).await;
                    if !ft_watcher_is_current(generation) {
                        return;
                    }
                    let fresh_root = watch_root.clone();
                    let fresh_expanded = expanded.clone();
                    let fresh = executor
                        .spawn(async move {
                            fingerprint_tree(&fresh_root, show_hidden, &fresh_expanded).to_string()
                        })
                        .await;
                    if fresh != sample || ft_watcher_last_refresh_elapsed() < Some(WATCH_COOLDOWN) {
                        // Burst still moving, or the cooldown from the previous
                        // refresh still shields a new scan: keep debouncing.
                        next_poll = Instant::now() + WATCH_POLL;
                        continue;
                    }
                    let apply_root = watch_root.clone();
                    let apply_expanded = expanded.clone();
                    // Apply a stable change on the UI thread.
                    let applied = this.update(cx, |app, cx| {
                        if !ft_watcher_is_current(generation) {
                            return;
                        }
                        {
                            let mut guard = ft_watcher_slot()
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if let Some(watch_state) = guard.state.as_mut() {
                                watch_state.baseline = Some(sample.clone());
                                watch_state.last_refresh = Some(Instant::now());
                            }
                        }
                        let leaf = app.active_leaf();
                        let cwd = leaf.map(|l| l.cwd.clone()).unwrap_or_default();
                        let Some(current_root) = resolve_root(&cwd) else {
                            return;
                        };
                        let ui_now = ft_ui(app);
                        if ft_watch_key(&current_root, ui_now.show_hidden, &ui_now.expanded)
                            != ft_watch_key(&apply_root, show_hidden, &apply_expanded)
                        {
                            // The root, hidden toggle, or expansion changed while we
                            // were debouncing; the render loop's own key change starts
                            // the right watcher.
                            return;
                        }
                        let mut ui_now = ui_now;
                        ui_now.refresh = ui_now.refresh.wrapping_add(1);
                        app.file_tree_ui = ft_encode(&ui_now);
                        cx.notify();
                    });
                    if applied.is_err() {
                        return;
                    }
                }
                next_poll = next_poll + WATCH_POLL;
                if next_poll <= Instant::now() {
                    // Never sleep longer than one poll: a long task must not let
                    // the loop keep an old wake-up.
                    next_poll = Instant::now() + WATCH_POLL;
                }
            }
        })
        .detach();
}

/// Resolve the panel root: visible pane cwd, then process cwd, then `$HOME`.
fn resolve_root(cwd: &str) -> Option<PathBuf> {
    let trimmed = cwd.trim();
    let from_leaf = if trimmed.is_empty() {
        None
    } else {
        Some(expand_tilde(trimmed))
    };
    from_leaf
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok().filter(|path| path.is_dir()))
        .or_else(|| home_dir().filter(|path| path.is_dir()))
}

fn root_title(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

fn active_shell_kind(app: &SpritXApp) -> crate::platform::ShellKind {
    if let Some(kind) = app
        .active_tab_checked()
        .and_then(|tab| app.sessions.get(&tab.active_leaf))
        .and_then(|s| s.shell_kind())
    {
        return kind;
    }
    let program = if crate::platform::is_default_shell(&app.settings.shell_program) {
        crate::platform::default_shell()
    } else {
        app.settings.shell_program.clone()
    };
    crate::platform::shell_kind(&program)
}

fn shell_quote(app: &SpritXApp, path: &str) -> String {
    crate::platform::quote_path(path, active_shell_kind(app))
}

fn with_path(template: &str, quoted: &str) -> String {
    template.replace("{path}", quoted)
}

struct Row {
    name: String,
    path: String,
    depth: usize,
    is_dir: bool,
    open: bool,
    /// Symlinked entry; kept off the expand chevron path (see `flatten`).
    symlink: bool,
    note: Option<String>,
}
/// A row with its git badge resolved. `Row` stays a plain layout row; this
/// pairs it with the cached git snapshot for the render pass.
struct BadgedRow {
    row: Row,
    git: Option<GitBadge>,
}

/// Applies git badges to flattened rows: exact file hits, folder rollups
/// (a directory shows `M` when anything beneath it changed, so dirty folders
/// read at a glance), and nested-repo boundaries. A nested repository root is
/// badged by whether its *own* repository is dirty; nothing above it rolls up
/// across the boundary, and nothing below it inherits the parent's statuses.
fn badge_rows(rows: Vec<Row>, statuses: &GitStatuses) -> Vec<BadgedRow> {
    // Directory rollup: walk the flattened rows deepest-first so a parent
    // absorbs its children's badges. The flat list is a pre-order traversal,
    // so reversing it visits children before their parent.
    let mut dir_badges: std::collections::HashMap<String, GitBadge> =
        std::collections::HashMap::new();
    for row in rows.iter().rev() {
        if row.note.is_some() {
            continue;
        }
        let path = Path::new(&row.path);
        if statuses.is_nested_root(path) {
            // Boundary: the nested repo's own dirtiness badges the folder, and
            // no badge propagates further up into the parent repository.
            if statuses
                .badges
                .keys()
                .any(|p| p != path && p.starts_with(path))
            {
                dir_badges.insert(row.path.clone(), GitBadge::Modified);
            }
            continue;
        }
        if let Some(badge) = statuses.badge_for(path) {
            dir_badges.insert(row.path.clone(), badge);
        }
        if row.is_dir {
            for (child_path, badge) in &dir_badges {
                if child_path == &row.path {
                    continue;
                }
                let child = Path::new(child_path);
                if !child.starts_with(path) {
                    continue;
                }
                // Never roll a nested repository's badge into the parent.
                if statuses
                    .nested_roots
                    .iter()
                    .any(|root| root != path && child.starts_with(root))
                {
                    continue;
                }
                dir_badges.insert(row.path.clone(), *badge);
                break;
            }
        }
    }
    rows.into_iter()
        .map(|row| {
            let own = if row.note.is_some() {
                None
            } else {
                dir_badges.get(&row.path).copied()
            };
            BadgedRow { row, git: own }
        })
        .collect()
}

fn note_row(depth: usize, note: impl Into<String>) -> Row {
    Row {
        name: String::new(),
        path: String::new(),
        depth,
        is_dir: false,
        open: false,
        symlink: false,
        note: Some(note.into()),
    }
}

fn flatten(dir: &DirLoad, depth: usize, expanded: &[String], rows: &mut Vec<Row>) {
    for node in &dir.nodes {
        // Symlink dirs are deliberately not expandable (no visited-inode
        // guard to follow them safely), so they get no chevron either.
        let open = node.is_dir && !node.symlink && expanded.iter().any(|p| p == &node.path);
        rows.push(Row {
            name: node.name.clone(),
            path: node.path.clone(),
            depth,
            is_dir: node.is_dir,
            open,
            symlink: node.symlink,
            note: None,
        });

        if !open {
            continue;
        }

        if node.symlink {
            rows.push(note_row(depth + 1, "Symlinked folder (not followed)"));
            continue;
        }
        if node.level >= MAX_DEPTH {
            rows.push(note_row(depth + 1, "Depth limit reached"));
            continue;
        }

        match &node.children {
            Loaded::Err(note) => rows.push(note_row(depth + 1, note.clone())),
            Loaded::NotLoaded => rows.push(note_row(depth + 1, "Not loaded")),
            Loaded::Ok(children) => {
                if children.nodes.is_empty() {
                    rows.push(note_row(depth + 1, "Empty folder"));
                } else {
                    flatten(children, depth + 1, expanded, rows);
                }
                if children.truncated {
                    rows.push(note_row(depth + 1, "Entry limit reached"));
                }
            }
        }
    }

    if dir.truncated {
        rows.push(note_row(
            depth,
            format!("Entry limit reached ({ENTRY_CAP})"),
        ));
    }
}

struct FileResize {
    start_width: f32,
    anchor_x: Rc<Cell<f32>>,
}

const RESIZE_HANDLE_WIDTH: f32 = 6.0;

fn resized_file_tree_width(start: f32, anchor: f32, pointer: f32) -> f32 {
    (start + pointer - anchor).clamp(180.0, 600.0)
}

fn resize_handle_left(width: f32) -> Pixels {
    crate::theme::body_pad() + px(width - RESIZE_HANDLE_WIDTH / 2.0)
}

#[derive(Default)]
struct FileScrollDrag {
    anchor: Cell<(Pixels, Pixels)>,
}

struct FileTreeDragGhost;

impl Render for FileTreeDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size(px(0.))
    }
}

fn file_scroll_thumb_height(height: Pixels, max: Pixels) -> Pixels {
    (height * (height / (height + max)))
        .max(px(16.))
        .min(height)
}

fn show_file_scroll_thumb(app: &mut SpritXApp, cx: &mut Context<SpritXApp>) {
    app.file_scroll_thumb_visible = true;
    app.file_scroll_hide_task = Some(cx.spawn(async move |this, cx| {
        gpui::Timer::after(std::time::Duration::from_secs(3)).await;
        let mut waited_for_drag = false;
        while this
            .update(cx, |_, cx| cx.has_active_drag())
            .unwrap_or(false)
        {
            waited_for_drag = true;
            gpui::Timer::after(std::time::Duration::from_millis(100)).await;
        }
        if waited_for_drag {
            gpui::Timer::after(std::time::Duration::from_secs(3)).await;
        }
        let _ = this.update(cx, |this, cx| {
            this.file_scroll_thumb_visible = false;
            cx.notify();
        });
    }));
    cx.notify();
}
/// Builds the per-row git snapshot for one tree refresh. Runs on the
/// background executor (see `maybe_start_load`), never on the render thread.
///
/// The parent repository's statuses come from `git_path_statuses`. Every
/// nested repository below the root gets its own query and its badges replace
/// the parent's summary entries for the same paths — this is the boundary
/// rule: a nested repo's files are badged from the nested repo, never the
/// parent. When git is unavailable or the root is not a repository the map is
/// empty and the tree renders without badges.
fn load_git_statuses(root: &Path) -> Arc<GitStatuses> {
    let raw = git_path_statuses(root);
    if raw.is_empty() {
        return Arc::new(GitStatuses {
            badges: std::collections::HashMap::new(),
            nested_roots: Vec::new(),
        });
    }
    // Find nested repository roots below the tree root by looking for `.git`
    // entries. This is a plain filesystem walk, not a git invocation, and it
    // runs off the render thread with the rest of the scan.
    let mut nested_roots = Vec::new();
    collect_nested_repo_roots(root, &mut nested_roots);
    // Deepest first so badge lookups hit the innermost owning repository.
    nested_roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));

    let mut badges: std::collections::HashMap<PathBuf, GitBadge> = std::collections::HashMap::new();
    for (path, status) in &raw {
        // Drop parent-repo entries that live inside a nested repository: the
        // nested repo's own query below supplies the authoritative badges.
        if nested_roots
            .iter()
            .any(|nested| path != nested && path.starts_with(nested))
        {
            continue;
        }
        badges.insert(path.clone(), GitBadge::from_status(*status));
    }
    for nested in &nested_roots {
        for (path, status) in git_path_statuses(nested) {
            badges.insert(path, GitBadge::from_status(status));
        }
    }
    Arc::new(GitStatuses {
        badges,
        nested_roots,
    })
}

/// Depth-bounded walk collecting directories that contain a `.git` entry
/// (directory or worktree/submodule file). Hidden entries are included:
/// `.git` is itself dot-prefixed.
fn collect_nested_repo_roots(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            if let Some(parent) = path.parent() {
                if parent != dir {
                    out.push(parent.to_path_buf());
                }
            }
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir && path.components().count() - dir.components().count() < MAX_DEPTH {
            collect_nested_repo_roots(&path, out);
        }
    }
}

/// Starts the background directory load when the current render has no
/// exact-key result yet. The in-flight set guarantees one load per key, so
/// any number of renders may call this and only the first one dispatches.
/// Follows the `cli_launcher::rescan` convention: the blocking scan runs on
/// `background_executor`, completion lands via `cx.spawn` + `cx.notify`.
fn maybe_start_load(root: &Path, ui: &FtUi, cx: &mut Context<SpritXApp>) {
    let key = ft_cache_key(root, ui.show_hidden, ui.refresh, &ui.expanded);
    {
        let mut in_flight = in_flight_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !in_flight.insert(key.clone()) {
            return;
        }
    }
    let load_key = key.clone();
    let load_root = root.to_path_buf();
    let show_hidden = ui.show_hidden;
    let expanded_for_load = ui.expanded.clone();
    let scan = cx.background_executor().spawn(async move {
        let state: Result<Arc<DirLoad>, String> =
            load_tree(&load_root, show_hidden, &expanded_for_load).map(Arc::new);
        // Git status rides the same background task as the directory scan, so
        // `git` never runs on the render thread and only once per tree
        // refresh (one spawn per cache key via the in-flight set above).
        let git_statuses = load_git_statuses(&load_root);
        // The fingerprint for the watcher is sampled on this same background
        // executor, while the tree is being scanned, so the poller's first
        // comparison baseline reflects the just-loaded directory set.
        let fingerprint = fingerprint_tree(&load_root, show_hidden, &expanded_for_load).to_string();
        (load_key, state, git_statuses, fingerprint)
    });
    cx.spawn(async move |this, cx| {
        let (done_key, state, git_statuses, fingerprint) = scan.await;
        {
            let mut in_flight = in_flight_slot()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight.remove(&done_key);
        }
        let _ = this.update(cx, |app, cx| {
            // Re-resolve the key the app wants *now*. A result whose root,
            // hidden toggle, refresh or expansion changed under it is stale
            // and dropped instead of applied; the newer key's own load
            // (kicked off by the next render, or already in flight) lands
            // separately. Caching the error too is safe: it stays pinned to
            // this refresh key, so the next refresh bump retries.
            let leaf = app.active_leaf();
            let cwd = leaf.map(|l| l.cwd.clone()).unwrap_or_default();
            let current_root = resolve_root(&cwd);
            let ui_now = ft_ui(app);
            if let Some(want) = current_root
                .as_deref()
                .map(|r| ft_cache_key(r, ui_now.show_hidden, ui_now.refresh, &ui_now.expanded))
            {
                let published = publish_load(done_key.clone(), state, want);
                if published {
                    // Git badges stay pinned to the same key as the tree they
                    // were scanned with; a stale key never shows its badges.
                    let mut guard = git_statuses_slot()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *guard = Some((done_key.clone(), git_statuses));
                    // Publish the fingerprint sampled with this load, so the
                    // sampler's first comparison is against the exact state
                    // just painted.
                    let watch_key = FtWatchKey {
                        root: done_key.root.clone(),
                        show_hidden: done_key.show_hidden,
                        expanded_key: done_key.expanded_key.clone(),
                    };
                    note_ft_tree_loaded(&watch_key, fingerprint);
                }
            }
            cx.notify();
        });
    })
    .detach();
}

pub fn render(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let ui = ft_ui(app);
    let leaf = app.active_leaf();
    let loading = leaf.is_none();
    let cwd = leaf.map(|l| l.cwd.clone()).unwrap_or_default();
    let root = resolve_root(&cwd);

    let title = root
        .as_deref()
        .map(root_title)
        .unwrap_or_else(|| "—".to_string());
    let root_display = root
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown directory".to_string());
    let _mem = crate::memprof::scope(crate::memprof::Region::FileTree);
    // The filesystem watcher lives only for the life of this panel: start it
    // when a visible root is available, otherwise stop the previous task.
    if !app.file_tree_visible || app.file_list_collapsed || loading {
        stop_ft_watcher();
    } else if let Some(root) = root.as_deref() {
        maybe_start_watcher(root, &ui, cx);
    } else {
        stop_ft_watcher();
    }
    let tree: Option<TreeView> = match root.as_deref() {
        Some(root) if !loading && !app.file_list_collapsed => {
            Some(
                match load_decision(root, ui.show_hidden, ui.refresh, &ui.expanded) {
                    LoadDecision::Reuse(state) => TreeView::Ready(state),
                    LoadDecision::Pending(previous) => TreeView::Pending(previous),
                    LoadDecision::Spawn => {
                        // The blocking scan runs on the background executor, so
                        // a large or network directory paints the previous tree
                        // (or the loading skeleton) instead of blocking render.
                        maybe_start_load(root, &ui, cx);
                        TreeView::Pending(None)
                    }
                },
            )
        }
        _ => None,
    };
    let mut plain_rows = Vec::new();
    match &tree {
        Some(TreeView::Ready(Ok(dir))) => flatten(dir, 0, &ui.expanded, &mut plain_rows),
        // While the load for the current key is in flight, keep painting
        // the previous tree for this root rather than a blank body.
        Some(TreeView::Pending(Some(Ok(dir)))) => flatten(dir, 0, &ui.expanded, &mut plain_rows),
        _ => {}
    }
    // Badges come from the cached snapshot for this exact key; when git is
    // unavailable the snapshot is empty and rows render exactly as before.
    let rows: Vec<BadgedRow> = match (root.as_deref(), &tree) {
        (Some(root), Some(TreeView::Ready(_)))
        | (Some(root), Some(TreeView::Pending(Some(Ok(_))))) => {
            let key = ft_cache_key(root, ui.show_hidden, ui.refresh, &ui.expanded);
            match git_statuses_for(&key) {
                Some(statuses) => badge_rows(plain_rows, &statuses),
                None => plain_rows
                    .into_iter()
                    .map(|row| BadgedRow { row, git: None })
                    .collect(),
            }
        }
        _ => plain_rows
            .into_iter()
            .map(|row| BadgedRow { row, git: None })
            .collect(),
    };
    let pending = matches!(tree, Some(TreeView::Pending(_)));

    let mut panel = div()
        .id("file-tree-panel")
        .relative()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(app.file_tree_width))
        .min_w(px(180.))
        .min_h_0()
        .h_full()
        .bg(theme.outer)
        .overflow_hidden()
        .on_drag_move::<FileResize>(cx.listener(
            |this, event: &DragMoveEvent<FileResize>, _, cx| {
                let drag = event.drag(cx);
                let width = resized_file_tree_width(drag.start_width,
                    drag.anchor_x.get(), f32::from(event.event.position.x));
                if (this.file_tree_width - width).abs() > 0.5 {
                    this.file_tree_width = width;
                    cx.notify();
                }
            },
        ));

    panel = panel.child(
        div()
            .flex()
            .flex_none()
            .h(crate::theme::tab_bar_h())
            .items_center()
            .justify_between()
            .px(px(12.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon(Icon::Blocks, 16.0, theme.fg))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("SpritX"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        ft_button("workspace-search", theme, "Search files")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_palette(PaletteMode::Command, cx)
                            }))
                            .child(icon(Icon::Search, 14.0, theme.fg_dim)),
                    )
                    .child(
                        ft_button("workspace-hide-sidebar", theme, "Hide sidebar")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.file_tree_visible = false;
                                cx.notify();
                            }))
                            .child(icon(Icon::Sidebar, 14.0, theme.fg_dim)),
                    ),
            ),
    );

    // --- directory controls -------------------------------------------------
    let directory_controls = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .px(px(10.))
        .py(px(8.))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .child(icon(Icon::Folder, 12.0, theme.accent))
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg)
                                .child(title),
                        ),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(9.5))
                        .text_color(theme.fg_dim)
                        .child(root_display),
                ),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(2.))
                .child(
                    ft_button("ft-hidden", theme, if ui.show_hidden { "Hide hidden files" } else { "Show hidden files" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut ui = ft_ui(this);
                            ui.show_hidden = !ui.show_hidden;
                            let shown = ui.show_hidden;
                            this.file_tree_ui = ft_encode(&ui);
                            this.show_toast(
                                if shown {
                                    "Hidden files shown"
                                } else {
                                    "Hidden files hidden"
                                },
                                cx,
                            );
                        }))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(if ui.show_hidden {
                                    theme.accent
                                } else {
                                    theme.fg_dim
                                })
                                .child(".*"),
                        ),
                )
                .child(
                    ft_button("ft-refresh", theme, "Reload files")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut ui = ft_ui(this);
                            ui.refresh = ui.refresh.wrapping_add(1);
                            this.file_tree_ui = ft_encode(&ui);
                            this.show_toast("File tree refreshed", cx);
                        }))
                        .child(icon(Icon::Refresh, 12.0, theme.fg_dim)),
                )
                .child(
                    ft_button("ft-toggle-file-list", theme, if app.file_list_collapsed { "Show files" } else { "Hide files" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.file_list_collapsed = !this.file_list_collapsed;
                            this.file_scroll_thumb_visible = false;
                            this.file_scroll_hide_task = None;
                            cx.notify();
                        }))
                        .child(icon(
                            if app.file_list_collapsed {
                                Icon::EyeOff
                            } else {
                                Icon::Eye
                            },
                            14.0,
                            theme.fg_dim,
                        )),
                )
                .child(
                    ft_button("ft-close", theme, "Close sidebar")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.file_tree_visible = false;
                            this.file_tree_ui = None;
                            cx.notify();
                        }))
                        .child(icon(Icon::X, 12.0, theme.fg_dim)),
                ),
        );

    // --- body ---------------------------------------------------------------
    let mut body = div()
        .id("file-tree-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .relative()
        .py(px(4.))
        .text_size(px(12.5));

    if app.file_list_collapsed {
        // The directory header remains visible while the list is collapsed.
    } else if loading {
        body = body.child(ft_note(theme, 0, "Loading…"));
    } else if root.is_none() {
        body = body.child(ft_note(theme, 0, "Unknown directory"));
    } else if let Some(TreeView::Ready(Err(note))) = tree.as_ref() {
        body = body.child(ft_note(theme, 0, note));
    } else if pending {
        body = body.child(ft_note(theme, 0, "Loading…"));
    } else if rows.is_empty() {
        body = body.child(ft_note(theme, 0, "Empty folder"));
    } else {
        let ctx_path = ui.ctx.as_ref().map(|(path, _)| path.clone());
        let scroll = app.file_list_scroll.clone();
        body = body
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                    let max = this
                        .file_list_scroll
                        .0
                        .borrow()
                        .base_handle
                        .max_offset()
                        .height;
                    if max <= px(0.) || event.delta.pixel_delta(window.line_height()).y == px(0.) {
                        return;
                    }
                    show_file_scroll_thumb(this, cx);
                }),
            )
            .child(
                uniform_list(
                    "file-tree-list",
                    rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let mut items = Vec::with_capacity(range.len());
                        for ix in range {
                            let badged = &rows[ix];
                            let row = &badged.row;
                            let is_ctx = ctx_path.as_deref() == Some(row.path.as_str());
                            items.push(file_tree_row(this.theme, row, badged.git, is_ctx, cx));
                        }
                        items
                    }),
                )
                .track_scroll(scroll.clone())
                .flex_1()
                .min_h_0(),
            );
        if app.file_scroll_thumb_visible {
            let drag_scroll = scroll.clone();
            body = body.child(
                div()
                    .id("file-tree-scrollbar")
                    .absolute()
                    .top(px(4.))
                    .bottom(px(4.))
                    // Leave the outer 6px available for sidebar resizing.
                    .right(px(6.))
                    .w(px(10.))
                    .cursor(CursorStyle::PointingHand)
                    .block_mouse_except_scroll()
                    .on_drag(FileScrollDrag::default(), move |drag, _, window, cx| {
                        let offset = drag_scroll.0.borrow().base_handle.offset().y;
                        drag.anchor.set((window.mouse_position().y, offset));
                        cx.new(|_| FileTreeDragGhost)
                    })
                    .on_drag_move::<FileScrollDrag>(cx.listener(
                        |this, event: &DragMoveEvent<FileScrollDrag>, _, cx| {
                            let (anchor_y, start_offset) = event.drag(cx).anchor.get();
                            let state = this.file_list_scroll.0.borrow();
                            let handle = &state.base_handle;
                            let max = handle.max_offset().height;
                            let height = handle.bounds().size.height;
                            let travel = height - file_scroll_thumb_height(height, max);
                            if max <= px(0.) || travel <= px(0.) {
                                return;
                            }
                            let delta = (event.event.position.y - anchor_y) / travel;
                            let offset = (start_offset - max * delta).clamp(-max, px(0.));
                            handle.set_offset(gpui::point(handle.offset().x, offset));
                            drop(state);
                            show_file_scroll_thumb(this, cx);
                        },
                    ))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let state = scroll.0.borrow();
                                let handle = &state.base_handle;
                                let max = handle.max_offset().height;
                                let height = bounds.size.height;
                                if max <= px(0.) || height <= px(0.) {
                                    return;
                                }
                                let thumb_height = file_scroll_thumb_height(height, max);
                                let progress = (-handle.offset().y / max).clamp(0.0, 1.0);
                                let thumb = gpui::Bounds {
                                    origin: gpui::point(
                                        bounds.left() + px(4.),
                                        bounds.top() + (height - thumb_height) * progress,
                                    ),
                                    size: gpui::size(px(2.), thumb_height),
                                };
                                window.paint_quad(gpui::quad(
                                    thumb,
                                    gpui::Corners::all(px(1.)),
                                    theme.fg_dim.opacity(0.45),
                                    gpui::Edges::all(px(0.)),
                                    gpui::transparent_black(),
                                    gpui::BorderStyle::Solid,
                                ));
                            },
                        )
                        .size_full(),
                    ),
            );
        }
    }

    panel = panel
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(directory_controls)
                .child(body),
        )
        .child(super::workspace::render(app, cx))
        .child(super::cli_launcher::render(app, cx));

    // --- context menu -------------------------------------------------------
    if let Some((path, is_dir)) = ui.ctx.clone().filter(|_| !app.file_list_collapsed) {
        let name = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        panel = panel
            .child(ft_ctx_backdrop(cx))
            .child(render_ft_menu(app, &name, &path, is_dir, theme, cx));
    }

    panel.into_any_element()
}

// Painted after the terminal at the shared edge, outside the sidebar's clip.
// The hit target straddles both surfaces without reserving a visual gutter.
pub(super) fn render_resize_handle(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let start_width = app.file_tree_width;
    // Keep the press coordinate across repaints before GPUI starts the drag.
    let anchor = app.file_tree_resize_anchor.clone();
    let mouse_anchor = anchor.clone();
    div()
        .id("file-tree-resize")
        .absolute()
        .left(resize_handle_left(start_width))
        .top(crate::theme::body_pad())
        .bottom(crate::theme::body_pad())
        .w(px(RESIZE_HANDLE_WIDTH))
        .cursor_col_resize()
        .occlude()
        .on_mouse_down(MouseButton::Left, move |event, _, _| {
            mouse_anchor.set(f32::from(event.position.x));
        })
        .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
            if event.click_count() >= 2 {
                this.file_tree_width = 240.0;
                cx.notify();
            }
        }))
        .on_drag(
            FileResize { start_width, anchor_x: anchor },
            |_, _, _, cx| cx.new(|_| FileTreeDragGhost),
        )
        .into_any_element()
}

fn ft_note(theme: Theme, depth: usize, text: &str) -> AnyElement {
    div()
        .pl(px(depth as f32 * 12.0 + 28.0))
        .pr(px(8.))
        .py(px(2.))
        .text_size(px(11.5))
        .text_color(theme.fg_dim)
        .opacity(0.7)
        .child(text.to_string())
        .into_any_element()
}

/// One virtualized file-tree row. Kept at a fixed 22px height (notes
/// included) so [`uniform_list`] can lay the list out without full taffy
/// passes — only the rows actually on screen are ever built.
fn file_tree_row(
    theme: Theme,
    row: &Row,
    git_badge: Option<GitBadge>,
    is_ctx: bool,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    crate::metrics::add(crate::metrics::Name::TreeRows, 1);
    if let Some(note) = &row.note {
        return div()
            .h(px(22.))
            .flex()
            .items_center()
            .pl(px(row.depth as f32 * 12.0 + 28.0))
            .pr(px(8.))
            .text_size(px(11.5))
            .text_color(theme.fg_dim)
            .opacity(0.7)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis()
            .child(note.clone())
            .into_any_element();
    }

    let name = row.name.clone();
    let path = row.path.clone();
    let path_for_ctx = path.clone();
    let is_dir = row.is_dir;
    let open = row.open;
    let is_symlink = row.symlink;
    let depth = row.depth;

    let mut row_el = div()
        .id(gpui::SharedString::from(format!("ft-row-{path}")))
        .relative()
        .flex()
        .items_center()
        .gap(px(4.))
        .h(px(22.))
        .pl(px(depth as f32 * 12.0 + 6.0))
        .pr(px(8.))
        .cursor_pointer()
        .text_color(if is_dir { theme.fg } else { theme.fg_dim })
        .when(is_ctx, |d| d.bg(theme.accent.opacity(0.12)))
        .hover(|s| s.bg(theme.white_06).text_color(theme.fg))
        .on_click(cx.listener(move |this, _, _, cx| {
            if is_dir {
                let mut ui = ft_ui(this);
                if let Some(pos) = ui.expanded.iter().position(|p| p == &path) {
                    ui.expanded.remove(pos);
                } else {
                    ui.expanded.push(path.clone());
                }
                this.file_tree_ui = ft_encode(&ui);
            } else {
                let quoted = shell_quote(this, &path);
                if quoted.is_empty() {
                    this.show_toast(
                        "This path cannot be safely inserted into cmd.exe; use PowerShell",
                        cx,
                    );
                    return;
                }
                if this.send_to_terminal(&quoted, false) {
                    this.show_toast(format!("Inserted {quoted}"), cx);
                } else {
                    this.show_toast("No active terminal", cx);
                }
            }
            cx.notify();
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, _, cx| {
                let mut ui = ft_ui(this);
                ui.ctx = Some((path_for_ctx.clone(), is_dir));
                this.file_tree_ui = ft_encode(&ui);
                cx.notify();
            }),
        );

    if is_ctx {
        row_el = row_el.child(
            div()
                .absolute()
                .left(px(0.))
                .top(px(2.))
                .bottom(px(2.))
                .w(px(2.))
                .bg(theme.accent),
        );
    }

    if is_dir {
        row_el = row_el
            .child(if is_symlink {
                // No expand chevron: symlink dirs are never expanded, so the
                // folder icon alone would imply expansion that cannot happen.
                div().w(px(11.)).into_any_element()
            } else {
                icon(
                    if open {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    },
                    11.0,
                    theme.fg_dim,
                )
                .into_any_element()
            })
            .child(icon(Icon::Folder, 12.0, theme.accent));
    } else {
        row_el = row_el.child(div().w(px(11.))).child(icon(
            Icon::File,
            12.0,
            theme.fg_dim.opacity(0.85),
        ));
    }

    // Compact git badge: one letter, trailing the file name like the
    // extension badges in VS Code's explorer. Colors reuse existing theme
    // tokens only — warning for modified, success for added, danger for
    // deleted, fg_dim for untracked — so the badge follows the palette.
    let badge_el = git_badge.map(|badge| {
        let (letter, color) = match badge {
            GitBadge::Modified => ("M", theme.warning),
            GitBadge::Added => ("A", theme.success),
            GitBadge::Deleted => ("D", theme.danger),
            GitBadge::Untracked => ("?", theme.fg_dim),
        };
        div()
            .flex_none()
            .w(px(14.))
            .h(px(14.))
            .rounded(px(3.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(9.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color)
            .bg(color.opacity(0.14))
            .child(letter)
            .into_any_element()
    });

    let mut tail = row_el.child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(name),
    );
    if let Some(badge_el) = badge_el {
        tail = tail.child(badge_el);
    }
    tail.into_any_element()
}

struct FileTreeTooltip {
    label: &'static str,
    theme: Theme,
}

impl Render for FileTreeTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().px(px(8.)).py(px(6.)).rounded(px(7.))
            .bg(self.theme.bg).border_1().border_color(self.theme.border)
            .text_size(px(11.)).text_color(self.theme.fg).child(self.label)
    }
}

fn ft_button(id: &'static str, theme: Theme, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(22.))
        .min_w(px(22.))
        .px(px(3.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(theme.fg_dim)
        .hover(|s| s.bg(theme.white_08).text_color(theme.fg))
        .tooltip(move |_, cx| cx.new(|_| FileTreeTooltip { label, theme }).into())
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    #[test]
    fn handle_is_centered_on_the_shared_container_edge() {
        for width in [180.0, 240.0, 420.0, 600.0] {
            assert_eq!(resize_handle_left(width) + px(RESIZE_HANDLE_WIDTH / 2.0),
                crate::theme::body_pad() + px(width));
        }
    }

    #[test]
    fn dragging_tracks_pointer_distance_without_a_gutter_offset() {
        assert_eq!(resized_file_tree_width(240., 248., 248.), 240.);
        assert_eq!(resized_file_tree_width(240., 248., 268.), 260.);
        assert_eq!(resized_file_tree_width(240., 248., 228.), 220.);
        assert_eq!(resized_file_tree_width(240., 248., 100.), 180.);
        assert_eq!(resized_file_tree_width(240., 248., 900.), 600.);
    }
}

fn ft_ctx_backdrop(cx: &mut Context<SpritXApp>) -> AnyElement {
    div()
        .id("ft-ctx-backdrop")
        .absolute()
        .inset_0()
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                let mut ui = ft_ui(this);
                ui.ctx = None;
                this.file_tree_ui = ft_encode(&ui);
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, _, _, cx| {
                let mut ui = ft_ui(this);
                ui.ctx = None;
                this.file_tree_ui = ft_encode(&ui);
                cx.notify();
            }),
        )
        .into_any_element()
}

fn chip_for(template: &str) -> Option<String> {
    let chip = template.replace("{path}", "");
    let chip = chip.trim().to_string();
    if chip.is_empty() {
        None
    } else {
        Some(chip)
    }
}

#[derive(Clone)]
enum FtMenuAction {
    Run(String),
    CopyPath,
    InsertPath,
}

fn render_ft_menu(
    app: &SpritXApp,
    name: &str,
    path: &str,
    is_dir: bool,
    theme: Theme,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let quoted = shell_quote(app, path);
    let settings = &app.settings;
    let kind = active_shell_kind(app);
    let defaults = crate::backend::config::FileTreeConfig::default();
    let cmd = cfg!(windows) && kind == crate::platform::ShellKind::Cmd;
    let list = if cmd && settings.file_tree_list == defaults.dir_list {
        "dir {path}"
    } else {
        &settings.file_tree_list
    };
    let view = if cmd && settings.file_tree_view == defaults.file_view {
        "type {path}"
    } else {
        &settings.file_tree_view
    };
    let open_dir = if cmd && settings.file_tree_open == defaults.dir_open {
        "explorer {path}"
    } else {
        &settings.file_tree_open
    };
    let cd = match kind {
        crate::platform::ShellKind::PowerShell => "Set-Location -LiteralPath {path}",
        crate::platform::ShellKind::Cmd => "cd /d {path}",
        crate::platform::ShellKind::Posix => "cd {path}",
    };
    let actions: Vec<(&'static str, Option<String>, FtMenuAction)> = if is_dir {
        vec![
            (
                "Open folder",
                chip_for(cd),
                FtMenuAction::Run(with_path(cd, &quoted)),
            ),
            (
                "List",
                chip_for(list),
                FtMenuAction::Run(with_path(list, &quoted)),
            ),
            (
                "Open in editor",
                chip_for(if is_dir {
                    open_dir
                } else {
                    &settings.file_tree_open_file
                }),
                FtMenuAction::Run(with_path(
                    if is_dir {
                        open_dir
                    } else {
                        &settings.file_tree_open_file
                    },
                    &quoted,
                )),
            ),
            ("Copy path", None, FtMenuAction::CopyPath),
            ("Insert path", None, FtMenuAction::InsertPath),
        ]
    } else {
        vec![
            (
                "View",
                chip_for(view),
                FtMenuAction::Run(with_path(view, &quoted)),
            ),
            (
                "Edit",
                chip_for(&settings.file_tree_edit),
                FtMenuAction::Run(with_path(&settings.file_tree_edit, &quoted)),
            ),
            (
                "Open in editor",
                chip_for(if is_dir {
                    open_dir
                } else {
                    &settings.file_tree_open_file
                }),
                FtMenuAction::Run(with_path(
                    if is_dir {
                        open_dir
                    } else {
                        &settings.file_tree_open_file
                    },
                    &quoted,
                )),
            ),
            ("Copy path", None, FtMenuAction::CopyPath),
            ("Insert path", None, FtMenuAction::InsertPath),
        ]
    };

    let mut menu = div()
        .id("ft-ctx-menu")
        .absolute()
        .top(px(52.))
        .right(px(6.))
        .w(px(208.))
        .flex()
        .flex_col()
        .gap(px(1.))
        .p(px(4.))
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .child(
            div()
                .px(px(10.))
                .pt(px(4.))
                .pb(px(6.))
                .mb(px(4.))
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(11.))
                .text_color(theme.fg_dim)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name.to_string()),
        );

    for (index, (label, chip, action)) in actions.into_iter().enumerate() {
        if index == 3 {
            menu = menu.child(div().h(px(1.)).my(px(2.)).bg(theme.border));
        }

        let path = path.to_string();
        let quoted = quoted.clone();
        let mut item = div()
            .id(("ft-ctx-item", index))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(5.))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(theme.fg)
            .hover(|s| s.bg(theme.accent.opacity(0.16)))
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut ui = ft_ui(this);
                ui.ctx = None;
                this.file_tree_ui = ft_encode(&ui);

                if quoted.is_empty() && !matches!(&action, FtMenuAction::CopyPath) {
                    this.show_toast(
                        "This path cannot be safely inserted into cmd.exe; use PowerShell",
                        cx,
                    );
                    return;
                }
                match &action {
                    FtMenuAction::CopyPath => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
                        this.show_toast("Path copied", cx);
                    }
                    FtMenuAction::InsertPath => {
                        if this.send_to_terminal(&quoted, false) {
                            this.show_toast(format!("Inserted {quoted}"), cx);
                        } else {
                            this.show_toast("No active terminal", cx);
                        }
                    }
                    FtMenuAction::Run(command) => {
                        let command = command.clone();
                        if this.send_to_terminal(&command, true) {
                            this.show_toast(format!("Ran `{command}`"), cx);
                        } else {
                            this.show_toast("No active terminal", cx);
                        }
                    }
                }
                cx.notify();
            }));

        item = item.child(label.to_string());
        if let Some(chip) = chip {
            item = item.child(
                div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .bg(theme.bg_soft)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(10.5))
                    .text_color(theme.fg_dim)
                    .child(chip),
            );
        }
        menu = menu.child(item);
    }

    menu.into_any_element()
}

pub fn render_for_tab(tab: &TabState, theme: Theme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .px(px(10.))
        .py(px(4.))
        .text_size(px(12.))
        .text_color(theme.fg_dim)
        .child(tab.title.clone())
        .into_any_element()
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    #[test]
    fn long_unicode_paths_and_expanded_folders_refresh_without_panicking() {
        let root = std::env::temp_dir().join(format!(
            "spritx-fingerprint-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let folder = root.join("long-project-directory-测试");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("long-file-name-λ.txt");
        std::fs::write(&file, "first").unwrap();
        let expanded = vec![folder.to_string_lossy().into_owned()];

        let before = fingerprint_tree(&root, true, &expanded);
        for _ in 0..3 {
            assert_eq!(fingerprint_tree(&root, true, &expanded), before);
        }
        std::fs::write(&file, "updated contents with a different size").unwrap();
        let after = fingerprint_tree(&root, true, &expanded);
        assert_ne!(after, before, "expanded child changes must refresh the tree");
        std::fs::remove_file(&file).unwrap();
        assert_ne!(fingerprint_tree(&root, true, &expanded), after);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
