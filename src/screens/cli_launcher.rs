use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, IntoElement, MouseButton,
};

use crate::backend::config::{AppConfig, CliLauncherConfig};
use crate::platform::{quote_path, ShellKind};
use crate::state::{SplitDirection, TabState, MAX_PANES_PER_TAB};
use crate::terminal::{SessionConfig, TerminalSession};
use crate::theme::Theme;

use super::app_shell::SpritXApp;

#[derive(Debug, PartialEq, Eq)]
struct Brand {
    command: &'static str,
    label: &'static str,
    image: &'static str,
    background: u32,
}

const BRANDS: &[Brand] = &[
    Brand {
        command: "codex",
        label: "Codex",
        image: "brands/Codex.png",
        background: 0xffffff,
    },
    Brand {
        command: "claude",
        label: "Claude Code",
        image: "brands/Claude.png",
        background: 0xffffff,
    },
    Brand {
        command: "opencode",
        label: "OpenCode",
        image: "brands/OpenCode.png",
        background: 0xffffff,
    },
    Brand {
        command: "pi",
        label: "Pi",
        image: "brands/Pi.png",
        background: 0xffffff,
    },
    Brand {
        command: "omp",
        label: "Oh My Pi",
        image: "brands/Omp.png",
        background: 0x0d0d0d,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tool {
    brand: &'static Brand,
    path: String,
}

pub(super) struct Launcher {
    config: CliLauncherConfig,
    tools: Vec<Tool>,
    menu: Option<&'static Brand>,
    scanning: bool,
    direction: SplitDirection,
    use_home: bool,
    scan_loop: Option<gpui::Task<()>>,
}

impl Launcher {
    pub(super) fn new(config: CliLauncherConfig) -> Self {
        Self {
            config,
            tools: Vec::new(),
            menu: None,
            scanning: false,
            direction: SplitDirection::Row,
            use_home: false,
            scan_loop: None,
        }
    }

    fn save(&self) -> Result<(), String> {
        let mut config = AppConfig::load();
        config.cli_launcher = self.config.clone();
        config.save()
    }

    pub(super) fn dismiss_menu(&mut self) -> bool {
        self.menu.take().is_some()
    }
}

fn executable(path: &Path) -> Option<Tool> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    #[cfg(windows)]
    if !path.extension()?.to_str().is_some_and(|ext| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "exe" | "com" | "cmd" | "bat" | "ps1"
        )
    }) {
        return None;
    }
    let path_text = path.to_str()?;
    if path_text.chars().any(char::is_control) {
        return None;
    }
    #[cfg(windows)]
    let name = path.file_stem()?.to_str()?;
    #[cfg(not(windows))]
    let name = path.file_name()?.to_str()?;
    let brand = BRANDS.iter().find(|brand| {
        if cfg!(windows) {
            brand.command.eq_ignore_ascii_case(name)
        } else {
            brand.command == name
        }
    })?;
    Some(Tool {
        brand,
        path: path_text.into(),
    })
}

fn discover(manual: Vec<String>) -> Vec<Tool> {
    #[cfg(windows)]
    const SUFFIXES: &[&str] = &[".exe", ".com", ".cmd", ".bat", ".ps1"];
    #[cfg(not(windows))]
    const SUFFIXES: &[&str] = &[""];

    let mut seen = HashSet::new();
    let dirs: Vec<_> = path_directories()
        .into_iter()
        .filter(|dir| dir.is_absolute() && seen.insert(dir.clone()))
        .collect();
    let manual: Vec<_> = manual
        .iter()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .filter_map(executable)
        .collect();

    BRANDS
        .iter()
        .filter_map(|brand| {
            manual
                .iter()
                .find(|tool| tool.brand == brand)
                .cloned()
                .or_else(|| {
                    dirs.iter().find_map(|dir| {
                        SUFFIXES.iter().find_map(|suffix| {
                            executable(&dir.join(format!("{}{suffix}", brand.command)))
                        })
                    })
                })
        })
        .collect()
}

fn path_directories() -> Vec<PathBuf> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let paths: Vec<_> = std::env::split_paths(&inherited).collect();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ,
            RRF_RT_REG_SZ,
        };

        let mut paths = paths;
        // The process environment is a snapshot. Read the live system and user
        // PATH too, so installers can add launcher entries while SpritX is open.
        for (key, subkey) in [
            (
                HKEY_LOCAL_MACHINE,
                r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
            ),
            (HKEY_CURRENT_USER, "Environment"),
        ] {
            let subkey: Vec<u16> = subkey.encode_utf16().chain([0]).collect();
            let value = [b'P' as u16, b'a' as u16, b't' as u16, b'h' as u16, 0];
            let mut data = vec![0u16; 32768];
            let mut bytes = (data.len() * 2) as u32;
            // Both names are NUL-terminated and the output buffer has `bytes` bytes.
            let status = unsafe {
                RegGetValueW(
                    key,
                    subkey.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                    std::ptr::null_mut(),
                    data.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if status == 0 {
                let end = data
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(data.len());
                let value = std::ffi::OsString::from_wide(&data[..end]);
                paths.extend(std::env::split_paths(&value));
            }
        }
        paths
    }
    #[cfg(not(windows))]
    paths
}

pub(super) fn start_scanning(app: &mut SpritXApp, cx: &mut Context<SpritXApp>) {
    rescan(app, cx);
    app.cli_launcher.scan_loop = Some(cx.spawn(async move |this, cx| loop {
        gpui::Timer::after(Duration::from_secs(8)).await;
        if this.update(cx, rescan).is_err() {
            break;
        }
    }));
}

fn rescan(app: &mut SpritXApp, cx: &mut Context<SpritXApp>) {
    if app.cli_launcher.scanning {
        return;
    }
    app.cli_launcher.scanning = true;
    let manual = app.cli_launcher.config.manual.clone();
    let scan_manual = manual.clone();
    let scan = cx
        .background_executor()
        .spawn(async move { discover(scan_manual) });
    cx.spawn(async move |this, cx| {
        let tools = scan.await;
        let _ = this.update(cx, |app, cx| {
            app.cli_launcher.tools = tools;
            app.cli_launcher.scanning = false;
            if app.cli_launcher.config.manual != manual {
                rescan(app, cx);
            }
            cx.notify();
        });
    })
    .detach();
    cx.notify();
}

fn shell_command(tool: &Tool, kind: ShellKind) -> Option<String> {
    let path = quote_path(&tool.path, kind);
    if path.is_empty() {
        return None;
    }
    Some(if kind == ShellKind::PowerShell {
        format!("& {path}")
    } else {
        path
    })
}

fn session_config(tool: &Tool, cwd: Option<String>) -> SessionConfig {
    let mut config = SessionConfig::command(&tool.path, []);
    config.display_name = Some(tool.brand.label.into());
    if cwd.is_some() {
        config.cwd = cwd;
    }
    #[cfg(windows)]
    if Path::new(&tool.path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "bat" | "cmd" | "ps1"))
    {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let command = format!(
            "{}; exit $LASTEXITCODE",
            shell_command(tool, ShellKind::PowerShell).unwrap()
        );
        let bytes: Vec<_> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
        config.program = crate::platform::default_shell();
        config.args = vec![
            "-NoProfile".into(),
            "-EncodedCommand".into(),
            STANDARD.encode(bytes),
        ];
    }
    config
}

#[derive(Clone, Copy)]
enum Launch {
    Active,
    Tab(usize),
    Split(SplitDirection),
}

fn launch(app: &mut SpritXApp, tool: &Tool, target: Launch, cx: &mut Context<SpritXApp>) {
    app.cli_launcher.menu = None;
    let active = app.active_tab_checked().map(|tab| tab.active_leaf).unwrap_or(0);
    let target = match target {
        Launch::Active | Launch::Split(_) if app.tabs.is_empty() => Launch::Tab(1),
        other => other,
    };
    let cwd = if app.cli_launcher.use_home {
        None
    } else {
        app.active_leaf()
            .map(|leaf| leaf.cwd.clone())
            .filter(|cwd| Path::new(cwd).is_dir())
    };
    if matches!(target, Launch::Active)
        && !app.cli_launcher.use_home
        && app.active_leaf().is_some_and(|leaf| !leaf.activity.running)
    {
        let command = app.session(active).and_then(|session| {
            let (header, _) = session.powerline_prompt_end()?;
            let (row, col) = session.visible_cursor()?;
            if session.exit_code().is_some() || row != header + 1 || col != 2 {
                return None;
            }
            let screen = session.screen();
            if (col..screen.size().1.get()).any(|col| {
                screen
                    .cell(row, col)
                    .is_some_and(|cell| cell.has_contents())
            }) {
                return None;
            }
            shell_command(tool, session.shell_kind()?)
        });
        if let Some(command) = command {
            if app.send_to_terminal(&command, true) {
                if let Some(leaf) = app.active_tab_mut().leaves.get_mut(&active) {
                    leaf.activity.running = true;
                }
            } else {
                app.show_toast("Could not write to the active terminal", cx);
            }
            cx.notify();
            return;
        }
    }
    if matches!(target, Launch::Split(_)) && app.active_tab().pane_count() >= MAX_PANES_PER_TAB {
        app.show_toast(format!("A tab holds at most {MAX_PANES_PER_TAB} panes"), cx);
        return;
    }
    let count = match target {
        Launch::Tab(count) => count,
        _ => 1,
    };
    let config = session_config(tool, cwd);
    let cwd = config.cwd.clone().unwrap_or_default();
    let mut sessions = Vec::with_capacity(count);
    for _ in 0..count {
        match app.spawn_workspace_session(config.clone()) {
            Ok(session) => sessions.push(session),
            Err(error) => {
                for session in &mut sessions { session.kill(); }
                app.show_toast(
                    format!("Could not launch {}: {error:#}", tool.brand.label),
                    cx,
                );
                return;
            }
        }
    }

    let start = app.next_leaf_id;
    let direction = app.cli_launcher.direction;
    if let Launch::Split(direction) = target {
        if !app.active_tab_mut().split_leaf(active, start, direction) {
            for session in &mut sessions { session.kill(); }
            app.show_toast("Could not split the active pane", cx);
            return;
        }
    } else {
        let mut tab = TabState::single(app.next_tab_id, tool.brand.label, start);
        let mut targets = VecDeque::from([start]);
        for id in start + 1..start + count as u64 {
            let parent = targets.pop_front().expect("a leaf is available to split");
            let split = if id - start >= 3 {
                if direction == SplitDirection::Row {
                    SplitDirection::Column
                } else {
                    SplitDirection::Row
                }
            } else {
                direction
            };
            tab.split_leaf(parent, id, split);
            targets.extend([parent, id]);
        }
        tab.active_leaf = start;
        app.next_tab_id += 1;
        app.tabs.push(tab);
        app.active_tab = app.tabs.len() - 1;
    }
    app.next_leaf_id += count as u64;
    for (index, session) in sessions.into_iter().enumerate() {
        let id = start + index as u64;
        if let Some(leaf) = app.active_tab_mut().leaves.get_mut(&id) {
            leaf.cwd.clone_from(&cwd);
            leaf.activity.running = true;
        }
        app.sessions.insert(id, session);
        app.command_started.insert(id, std::time::Instant::now());
        app.start_recent_work(id);
    }
    app.checkpoint_workspace();
    cx.notify();
}

fn button(id: &'static str, label: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(7.))
        .py(px(4.))
        .rounded(px(5.))
        .cursor_pointer()
        .hover(|style| style.bg(theme.white_06))
        .child(label)
}

fn update_config(
    app: &mut SpritXApp,
    cx: &mut Context<SpritXApp>,
    edit: impl FnOnce(&mut CliLauncherConfig),
) {
    let old = app.cli_launcher.config.clone();
    edit(&mut app.cli_launcher.config);
    if let Err(error) = app.cli_launcher.save() {
        app.cli_launcher.config = old;
        app.show_toast(format!("Could not save launcher settings: {error}"), cx);
    }
    cx.notify();
}

fn menu(app: &SpritXApp, tool: Tool, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let pinned = app.cli_launcher.config.pinned.contains(&tool.path);
    let hidden = app.cli_launcher.config.hidden.contains(&tool.path);
    let launch_button = |id, label, target, cx: &mut Context<SpritXApp>| {
        let tool = tool.clone();
        button(id, label, theme)
            .on_click(cx.listener(move |app, _, _, cx| launch(app, &tool, target, cx)))
    };
    let mut counts = div().flex().gap(px(4.)).px(px(4.));
    for count in 2..=MAX_PANES_PER_TAB {
        let tool = tool.clone();
        counts = counts.child(
            div()
                .id(("cli-count", count))
                .flex_1()
                .text_center()
                .py(px(4.))
                .rounded(px(5.))
                .bg(theme.bg_soft)
                .cursor_pointer()
                .hover(|style| style.bg(theme.white_08))
                .child(count.to_string())
                .on_click(
                    cx.listener(move |app, _, _, cx| launch(app, &tool, Launch::Tab(count), cx)),
                ),
        );
    }
    let copy = tool.clone();
    let pin = tool.clone();
    let hide = tool.clone();
    div()
        .id("cli-launch-menu")
        .absolute()
        .bottom_full()
        .left(px(7.))
        .right(px(10.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(6.))
        .rounded(px(9.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .occlude()
        .text_size(px(11.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .pl(px(7.))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(tool.brand.label),
                )
                .child(button("cli-menu-close", "×", theme).on_click(cx.listener(
                    |app, _, _, cx| {
                        app.cli_launcher.menu = None;
                        cx.notify();
                    },
                ))),
        )
        .child(launch_button(
            "cli-open-tab",
            "Open in new tab",
            Launch::Tab(1),
            cx,
        ))
        .child(launch_button(
            "cli-split-right",
            "Split right",
            Launch::Split(SplitDirection::Row),
            cx,
        ))
        .child(launch_button(
            "cli-split-down",
            "Split down",
            Launch::Split(SplitDirection::Column),
            cx,
        ))
        .child(
            button("cli-close-pane", "Close active pane", theme).on_click(cx.listener(
                |app, _, _, cx| {
                    app.cli_launcher.dismiss_menu();
                    if let Some(leaf) = app.active_tab_checked().map(|tab| tab.active_leaf) { app.close_pane(leaf, cx); }
                },
            )),
        )
        .child(
            div()
                .px(px(7.))
                .pt(px(4.))
                .text_color(theme.fg_dim)
                .child("Open in splits"),
        )
        .child(counts)
        .child(
            button(
                "cli-direction",
                if app.cli_launcher.direction == SplitDirection::Row {
                    "Layout · Columns first"
                } else {
                    "Layout · Rows first"
                },
                theme,
            )
            .on_click(cx.listener(|app, _, _, cx| {
                app.cli_launcher.direction = if app.cli_launcher.direction == SplitDirection::Row {
                    SplitDirection::Column
                } else {
                    SplitDirection::Row
                };
                cx.notify();
            })),
        )
        .child(
            button(
                "cli-folder",
                if app.cli_launcher.use_home {
                    "Folder · Home"
                } else {
                    "Folder · Active pane"
                },
                theme,
            )
            .on_click(cx.listener(|app, _, _, cx| {
                app.cli_launcher.use_home = !app.cli_launcher.use_home;
                cx.notify();
            })),
        )
        .child(
            button("cli-copy", "Copy launch command", theme).on_click(cx.listener(
                move |app, _, _, cx| {
                    let kind = app
                        .active_tab_checked().and_then(|tab| app.session(tab.active_leaf))
                        .and_then(TerminalSession::shell_kind)
                        .unwrap_or_else(|| {
                            crate::platform::shell_kind(&crate::platform::default_shell())
                        });
                    if let Some(command) = shell_command(&copy, kind) {
                        cx.write_to_clipboard(ClipboardItem::new_string(command));
                        app.cli_launcher.menu = None;
                        app.show_toast("Launch command copied", cx);
                    } else {
                        app.show_toast("This path cannot be quoted for the current shell", cx);
                    }
                },
            )),
        )
        .child(
            button(
                "cli-pin",
                if pinned { "Unpin" } else { "Pin to front" },
                theme,
            )
            .on_click(cx.listener(move |app, _, _, cx| {
                app.cli_launcher.menu = None;
                update_config(app, cx, |config| {
                    if pinned {
                        config.pinned.retain(|path| path != &pin.path);
                    } else {
                        config.pinned.push(pin.path.clone());
                    }
                });
            })),
        )
        .child(
            button(
                "cli-hide",
                if hidden {
                    "Enable launch"
                } else {
                    "Disable launch"
                },
                theme,
            )
            .on_click(cx.listener(move |app, _, _, cx| {
                app.cli_launcher.menu = None;
                update_config(app, cx, |config| {
                    if hidden {
                        config.hidden.retain(|path| path != &hide.path);
                    } else {
                        config.hidden.push(hide.path.clone());
                    }
                });
            })),
        )
        .into_any_element()
}

struct LogoTooltip {
    text: String,
    theme: Theme,
}

impl gpui::Render for LogoTooltip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(7.))
            .py(px(4.))
            .rounded(px(5.))
            .bg(self.theme.bg)
            .border_1()
            .border_color(self.theme.border)
            .text_color(self.theme.fg)
            .text_size(px(11.))
            .child(self.text.clone())
    }
}

fn logo(
    brand: &'static Brand,
    tool: Option<Tool>,
    app: &SpritXApp,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let theme = app.theme;
    let enabled = tool
        .as_ref()
        .is_some_and(|tool| !app.cli_launcher.config.hidden.contains(&tool.path));
    let tooltip = if enabled {
        brand.label.to_string()
    } else {
        format!(
            "{} · {}",
            brand.label,
            if tool.is_some() {
                "Disabled"
            } else {
                "Executable not found"
            }
        )
    };
    div()
        .id(brand.command)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .size(px(30.))
        .rounded(px(7.))
        .cursor_pointer()
        // Brand art and its contrast plate stay identical in both themes.
        // Unlike GPUI's monochrome SVG mask, an image retains original fills.
        .bg(gpui::rgb(brand.background))
        .border_1().border_color(theme.white_08)
        .hover(|style| style.border_color(theme.fg_dim))
        .child(gpui::img(brand.image).size(px(20.)).flex_none())
        .tooltip(move |_, cx| {
            cx.new(|_| LogoTooltip {
                text: tooltip.clone(),
                theme,
            })
            .into()
        })
        .on_click(cx.listener(move |app, _, window, cx| {
            if let Some(tool) = tool.as_ref().filter(|_| enabled) {
                window.focus(&app.focus_handle);
                launch(app, tool, Launch::Active, cx);
            } else {
                app.show_toast(format!("Right-click {} for options", brand.label), cx);
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |app, _, _, cx| {
                app.cli_launcher.menu = Some(brand);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .into_any_element()
}

fn missing_menu(brand: &'static Brand, theme: Theme, cx: &mut Context<SpritXApp>) -> AnyElement {
    div()
        .id("cli-missing-menu")
        .absolute()
        .bottom_full()
        .left(px(7.))
        .right(px(10.))
        .flex()
        .flex_col()
        .p(px(5.))
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .occlude()
        .child(div().px(px(7.)).py(px(4.)).child(brand.label))
        .child(
            button("cli-rescan", "Rescan", theme).on_click(cx.listener(|app, _, _, cx| {
                app.cli_launcher.dismiss_menu();
                rescan(app, cx);
                cx.notify();
            })),
        )
        .child(
            button("cli-add", "Add path from clipboard", theme).on_click(cx.listener(
                move |app, _, _, cx| {
                    let text = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    let path = Path::new(text.trim().trim_matches('"'));
                    let tool = path
                        .is_absolute()
                        .then(|| executable(path))
                        .flatten()
                        .filter(|tool| tool.brand == brand);
                    let Some(tool) = tool else {
                        app.show_toast(
                            format!("Copy the full executable path for {}", brand.label),
                            cx,
                        );
                        return;
                    };
                    update_config(app, cx, |config| {
                        if !config.manual.contains(&tool.path) {
                            config.manual.push(tool.path);
                        }
                    });
                    app.cli_launcher.dismiss_menu();
                    rescan(app, cx);
                },
            )),
        )
        .into_any_element()
}

pub(super) fn render(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let launcher = &app.cli_launcher;
    let find_tool = |brand: &Brand| {
        launcher
            .tools
            .iter()
            .find(|tool| tool.brand == brand)
            .cloned()
    };
    let mut entries: Vec<_> = BRANDS
        .iter()
        .map(|brand| (brand, find_tool(brand)))
        .collect();
    entries.sort_by_key(|(_, tool)| {
        !tool
            .as_ref()
            .is_some_and(|tool| launcher.config.pinned.contains(&tool.path))
    });
    let mut dock = div()
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .py(px(6.))
        .pr(px(6.))
        .children(
            entries
                .into_iter()
                .map(|(brand, tool)| logo(brand, tool, app, cx)),
        );
    if let Some(brand) = launcher.menu {
        dock = dock.child(match find_tool(brand) {
            Some(tool) => menu(app, tool, cx),
            None => missing_menu(brand, app.theme, cx),
        });
    }
    dock.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_quotes_the_entire_executable_path() {
        let tool = Tool {
            brand: &BRANDS[0],
            path: "/path with 'quotes'/tool".into(),
        };
        assert_eq!(
            shell_command(&tool, ShellKind::Posix).as_deref(),
            Some("'/path with '\\''quotes'\\''/tool'")
        );
        assert_eq!(
            shell_command(&tool, ShellKind::PowerShell).as_deref(),
            Some("& '/path with ''quotes''/tool'")
        );
    }
}
