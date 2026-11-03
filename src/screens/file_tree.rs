//! Left-hand file tree sidebar.
//!
//! Visual/interaction parity target is `src/FileTree.tsx` + App.css `.file-*`
//! (see docs/gpui-inventory/04-panels-overlays.md). The tree is backed by the
//! real filesystem through [`crate::backend::files`], rooted at the active
//! pane's cwd (falling back to the process cwd, then `$HOME`).
//!
//! Expansion, hidden-file and context-menu state use a dedicated app slot,
//! separate from shortcut recording.
//! The loaded directory tree is cached in a module-level slot and only rebuilt
//! when the root, hidden toggle, refresh counter or expansion set changes, so
//! rendering a frame never touches the filesystem.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{
    AnyElement, Context, DragMoveEvent, FontWeight, IntoElement, MouseButton, Render, Window, div,
    prelude::*, px, uniform_list,
};

use crate::backend::files::{FsEntry, expand_tilde, home_dir, list_dir};
use crate::components::icons::{Icon, icon};
use crate::screens::app_shell::SpritXApp;
use crate::state::TabState;
use crate::theme::{MONO_FONT, Theme};

const FT_PREFIX: &str = "filetree-ui";
const FT_SEP: char = '\u{1f}';
const FT_LIST_SEP: char = '\u{1e}';

/// Maximum number of nested directory levels loaded below the root. High
/// enough that normal projects never hit it; the expansion set is what
/// actually bounds the work, so this is just a safety net.
const MAX_DEPTH: usize = 32;
/// Total entry budget for one resolved tree (root + expanded descendants).
const ENTRY_CAP: usize = 2000;

#[derive(Default, Clone)]
struct FtUi {
    expanded: Vec<String>,
    ctx: Option<(String, bool)>,
    show_hidden: bool,
    refresh: u64,
}

fn ft_ui(app: &SpritXApp) -> FtUi {
    let Some(raw) = app.file_tree_ui.as_deref() else {
        return FtUi::default();
    };
    let mut parts = raw.split(FT_SEP);
    if parts.next() != Some(FT_PREFIX) {
        return FtUi::default();
    }
    let expanded = parts
        .next()
        .map(|s| {
            s.split(FT_LIST_SEP)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let ctx_path = parts.next().unwrap_or("").to_string();
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
    let expanded = ui.expanded.join(&FT_LIST_SEP.to_string());
    let (ctx_path, ctx_dir) = ui
        .ctx
        .as_ref()
        .map(|(path, dir)| (path.as_str(), *dir))
        .unwrap_or(("", false));
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

#[derive(Clone)]
struct FtCache {
    root: String,
    show_hidden: bool,
    refresh: u64,
    expanded_key: String,
    state: Result<Arc<DirLoad>, String>,
}

fn cache_slot() -> &'static Mutex<Option<FtCache>> {
    static SLOT: OnceLock<Mutex<Option<FtCache>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
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

fn cached_tree(
    root: &Path,
    show_hidden: bool,
    refresh: u64,
    expanded: &[String],
) -> Result<Arc<DirLoad>, String> {
    let root_key = root.to_string_lossy().into_owned();
    let expanded_key = expanded.join("\u{1d}");

    {
        let guard = cache_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cache) = guard.as_ref() {
            if cache.root == root_key
                && cache.show_hidden == show_hidden
                && cache.refresh == refresh
                && cache.expanded_key == expanded_key
            {
                return cache.state.clone();
            }
        }
    }

    let state: Result<Arc<DirLoad>, String> =
        load_tree(root, show_hidden, expanded).map(Arc::new);
    let mut guard = cache_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(FtCache {
        root: root_key,
        show_hidden,
        refresh,
        expanded_key,
        state: state.clone(),
    });
    state
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
    if let Some(kind) = app.sessions.get(&app.active_tab().active_leaf).and_then(|s| s.shell_kind()) {
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
    note: Option<String>,
}

fn note_row(depth: usize, note: impl Into<String>) -> Row {
    Row {
        name: String::new(),
        path: String::new(),
        depth,
        is_dir: false,
        open: false,
        note: Some(note.into()),
    }
}

fn flatten(dir: &DirLoad, depth: usize, expanded: &[String], rows: &mut Vec<Row>) {
    for node in &dir.nodes {
        let open = node.is_dir && expanded.iter().any(|p| p == &node.path);
        rows.push(Row {
            name: node.name.clone(),
            path: node.path.clone(),
            depth,
            is_dir: node.is_dir,
            open,
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
        rows.push(note_row(depth, format!("Entry limit reached ({ENTRY_CAP})")));
    }
}

struct FileResize {
    start_width: f32,
    anchor_x: Cell<f32>,
}

struct ResizeGhost;

impl Render for ResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size(px(0.))
    }
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
    let tree = match root.as_deref() {
        Some(root) if !loading => Some(cached_tree(root, ui.show_hidden, ui.refresh, &ui.expanded)),
        _ => None,
    };
    let mut rows = Vec::new();
    if let Some(Ok(dir)) = tree.as_ref() {
        flatten(dir, 0, &ui.expanded, &mut rows);
    }

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
        .rounded(px(5.))
        .bg(theme.bg_soft)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .on_drag_move::<FileResize>(cx.listener(
            |this, event: &DragMoveEvent<FileResize>, _, cx| {
                let drag = event.drag(cx);
                let width = (drag.start_width
                    + (f32::from(event.event.position.x) - drag.anchor_x.get()))
                    .clamp(180.0, 600.0);
                if (this.file_tree_width - width).abs() > 0.5 {
                    this.file_tree_width = width;
                    cx.notify();
                }
            },
        ));

    // --- header -------------------------------------------------------------
    panel = panel.child(
        div()
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
                        ft_button("ft-hidden", theme)
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
                                    .font_family(MONO_FONT)
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
                        ft_button("ft-refresh", theme)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let mut ui = ft_ui(this);
                                ui.refresh = ui.refresh.wrapping_add(1);
                                this.file_tree_ui = ft_encode(&ui);
                                this.show_toast("File tree refreshed", cx);
                            }))
                            .child(icon(Icon::Refresh, 12.0, theme.fg_dim)),
                    )
                    .child(
                        ft_button("ft-close", theme)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.file_tree_visible = false;
                                this.file_tree_ui = None;
                                cx.notify();
                            }))
                            .child(icon(Icon::X, 12.0, theme.fg_dim)),
                    ),
            ),
    );

    // --- body ---------------------------------------------------------------
    let mut body = div()
        .id("file-tree-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .py(px(4.))
        .text_size(px(12.5));

    if loading {
        body = body.child(ft_note(theme, 0, "Loading…"));
    } else if root.is_none() {
        body = body.child(ft_note(theme, 0, "Unknown directory"));
    } else if let Some(Err(note)) = tree.as_ref() {
        body = body.child(ft_note(theme, 0, note));
    } else if rows.is_empty() {
        body = body.child(ft_note(theme, 0, "Empty folder"));
    } else {
        let ctx_path = ui.ctx.as_ref().map(|(path, _)| path.clone());
        body = body.child(
            uniform_list(
                "file-tree-list",
                rows.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        let row = &rows[ix];
                        let is_ctx = ctx_path.as_deref() == Some(row.path.as_str());
                        items.push(file_tree_row(this.theme, row, is_ctx, cx));
                    }
                    items
                }),
            )
            .flex_1()
            .min_h_0(),
        );
    }

    panel = panel.child(body);

    // --- context menu -------------------------------------------------------
    if let Some((path, is_dir)) = ui.ctx.clone() {
        let name = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        panel = panel
            .child(ft_ctx_backdrop(cx))
            .child(render_ft_menu(app, &name, &path, is_dir, theme, cx));
    }

    // --- resize handle ------------------------------------------------------
    let start_width = app.file_tree_width;
    panel = panel.child(
        div()
            .id("file-tree-resize")
            .absolute()
            .right(px(0.))
            .top(px(0.))
            .bottom(px(0.))
            .w(px(5.))
            .cursor_col_resize()
            .hover(|s| s.bg(theme.accent.opacity(0.35)))
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _: &mut gpui::Window, cx| {
                if event.click_count() >= 2 {
                    this.file_tree_width = 240.0;
                    cx.notify();
                }
            }))
            .on_drag(
                FileResize {
                    start_width,
                    anchor_x: Cell::new(0.0),
                },
                |drag, _, window, cx| {
                    drag.anchor_x.set(f32::from(window.mouse_position().x));
                    cx.new(|_| ResizeGhost)
                },
            ),
    );

    panel.into_any_element()
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
                    this.show_toast("This path cannot be safely inserted into cmd.exe; use PowerShell", cx);
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
            .child(icon(
                if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                11.0,
                theme.fg_dim,
            ))
            .child(icon(Icon::Folder, 12.0, theme.accent));
    } else {
        row_el = row_el
            .child(div().w(px(11.)))
            .child(icon(Icon::File, 12.0, theme.fg_dim.opacity(0.85)));
    }

    row_el
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name),
        )
        .into_any_element()
}

fn ft_button(id: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
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
    if chip.is_empty() { None } else { Some(chip) }
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
    let list = if cmd && settings.file_tree_list == defaults.dir_list { "dir {path}" } else { &settings.file_tree_list };
    let view = if cmd && settings.file_tree_view == defaults.file_view { "type {path}" } else { &settings.file_tree_view };
    let open_dir = if cmd && settings.file_tree_open == defaults.dir_open { "explorer {path}" } else { &settings.file_tree_open };
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
                chip_for(if is_dir { open_dir } else { &settings.file_tree_open_file }),
                FtMenuAction::Run(with_path(if is_dir { open_dir } else { &settings.file_tree_open_file }, &quoted)),
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
                chip_for(if is_dir { open_dir } else { &settings.file_tree_open_file }),
                FtMenuAction::Run(with_path(if is_dir { open_dir } else { &settings.file_tree_open_file }, &quoted)),
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
                    this.show_toast("This path cannot be safely inserted into cmd.exe; use PowerShell", cx);
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
                    .font_family(MONO_FONT)
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
