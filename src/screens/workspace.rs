use super::app_shell::SpritXApp;
use crate::backend::workspace::{self, RecentProject, Workspace};
use crate::components::icons::{icon, Icon};
use gpui::{div, prelude::*, px, AnyElement, Context, IntoElement};
use std::path::Path;

impl SpritXApp {
    pub fn prepare_for_background(&mut self) {
        self.checkpoint_workspace();
    }

    pub fn quit_desktop(&mut self, cx: &mut Context<Self>) {
        self.desktop_quitting = true;
        self.checkpoint_workspace();
        for session in self.sessions.values_mut() {
            session.set_keep_on_drop(false);
            session.kill();
        }
        cx.quit();
    }
    pub(super) fn touch_recent(&mut self, pane: u64) {
        self.record_recent_activity(pane, false);
    }

    pub(super) fn start_recent_work(&mut self, pane: u64) {
        self.record_recent_activity(pane, true);
    }

    fn record_recent_activity(&mut self, pane: u64, work_started: bool) {
        if let Some(cwd) = self
            .tabs
            .iter()
            .find_map(|tab| tab.leaves.get(&pane).map(|leaf| leaf.cwd.clone()))
        {
            workspace::record_activity(
                &mut self.recent_projects,
                pane,
                &cwd,
                workspace::now_ms(),
                work_started,
            );
        }
    }

    pub(super) fn restore_workspace(&mut self) {
        let mut saved = workspace::load();
        for tab in &mut saved.tabs {
            tab.repair();
        }
        if !self.settings.restore_workspace || !workspace::valid(&saved) {
            for entry in &mut self.recent_projects {
                entry.pane = None;
            }
            return;
        }
        self.tabs = saved.tabs;
        self.active_tab = saved.active_tab.min(self.tabs.len().saturating_sub(1));
        self.next_tab_id = self.tabs.iter().map(|tab| tab.id).max().unwrap_or(0) + 1;
        self.next_leaf_id = self
            .tabs
            .iter()
            .flat_map(|tab| tab.leaf_ids())
            .chain(self.recent_projects.iter().filter_map(|entry| entry.pane))
            .max()
            .unwrap_or(0)
            + 1;
        self.restored_sessions = saved.sessions;
    }

    pub(super) fn spawn_workspace_session(
        &self,
        config: crate::terminal::SessionConfig,
    ) -> anyhow::Result<crate::terminal::TerminalSession> {
        if std::env::var_os("SPRITX_SMOKE").is_some() {
            return crate::terminal::TerminalSession::spawn(config);
        }
        let mut session = crate::terminal::TerminalSession::spawn_persistent(config)?;
        session.set_keep_on_drop(self.settings.keep_sessions && self.settings.restore_workspace);
        Ok(session)
    }

    pub(super) fn restore_pane_session(&mut self, pane: u64) -> bool {
        let Some(saved) = self.restored_sessions.remove(&pane) else {
            return false;
        };
        let retry = saved.clone();
        self.owner_seen.insert(pane, saved.owner_completion);
        let mut config = saved.config;
        if let Some(cwd) = self
            .tabs
            .iter()
            .find_map(|tab| tab.leaves.get(&pane).map(|leaf| leaf.cwd.clone()))
        {
            config.cwd = Some(cwd);
        }
        if let Some(connection) = saved.owner {
            if !connection.unavailable() {
                match crate::terminal::TerminalSession::attach_persistent(
                    config.clone(),
                    connection,
                ) {
                    Ok(mut session) => {
                        session.set_keep_on_drop(
                            self.settings.keep_sessions && self.settings.restore_workspace,
                        );
                        self.sessions.insert(pane, session);
                    }
                    Err(error) => {
                        self.session_errors.insert(pane, format!("Could not reconnect: {error:#}. Reopen the project to start a new session."));
                        self.restored_sessions.insert(pane, retry);
                    }
                }
                return true;
            }
        }
        if !config
            .cwd
            .as_ref()
            .is_some_and(|cwd| Path::new(cwd).is_dir())
        {
            config.cwd =
                crate::backend::files::home_dir().map(|path| path.to_string_lossy().into_owned());
        }
        // An ended CLI returns to a shell. A saved shell keeps its own configuration.
        if config.track_process {
            config = crate::terminal::SessionConfig::for_shell(
                Some(&self.settings.shell_program),
                &crate::platform::parse_shell_args(&self.settings.shell_args).unwrap_or_default(),
                config.cwd,
            );
        }
        if self.settings.save_terminal_output && !saved.screen.is_empty() {
            config.initial_output = Some(saved.screen);
        }
        match self.spawn_workspace_session(config) {
            Ok(session) => {
                self.sessions.insert(pane, session);
                for tab in &mut self.tabs {
                    if let Some(leaf) = tab.leaves.get_mut(&pane) {
                        leaf.activity.running = false;
                        leaf.remote = false;
                    }
                }
            }
            Err(error) => {
                self.session_errors.insert(pane, error.to_string());
            }
        }
        true
    }

    pub(super) fn checkpoint_workspace(&mut self) {
        let live: Vec<_> = self.tabs.iter().flat_map(|tab| tab.leaf_ids()).collect();
        for entry in &mut self.recent_projects {
            if entry.pane.is_some_and(|pane| !live.contains(&pane)) {
                entry.pane = None;
            }
        }
        let mut sessions = if self.settings.restore_workspace {
            self.restored_sessions
                .iter()
                .filter(|(pane, _)| live.contains(pane))
                .map(|(pane, session)| (*pane, session.clone()))
                .collect()
        } else {
            std::collections::HashMap::new()
        };
        for (&pane, session) in &mut self.sessions {
            let keep = self.settings.keep_sessions
                && self.settings.restore_workspace
                && !self.desktop_quitting;
            session.set_keep_on_drop(keep);
            if let Some(config) = session
                .launch_config()
                .filter(|_| self.settings.restore_workspace)
            {
                sessions.insert(
                    pane,
                    workspace::SavedSession {
                        config,
                        screen: if self.settings.save_terminal_output {
                            session.saved_screen()
                        } else {
                            String::new()
                        },
                        owner: session
                            .owner_connection()
                            .filter(|_| !self.desktop_quitting),
                        owner_completion: *self.owner_seen.get(&pane).unwrap_or(&0),
                    },
                );
            }
        }
        let snapshot = Workspace {
            empty_workspace: self.settings.restore_workspace && self.tabs.is_empty(),
            tabs: if self.settings.restore_workspace {
                self.tabs.clone()
            } else {
                Vec::new()
            },
            active_tab: self.active_tab,
            recent: self.recent_projects.clone(),
            sessions,
            ..Default::default()
        };
        if let Err(error) = workspace::save(&snapshot) {
            eprintln!("[spritx] workspace save: {error}");
        }
        self.workspace_last_save = std::time::Instant::now();
    }

    pub(super) fn sync_owned_status(&mut self) -> bool {
        let statuses: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(&pane, session)| Some((pane, session.owner_status()?)))
            .collect();
        let mut changed = false;
        for (pane, status) in statuses {
            // Older owners have no timestamp: retain a local observation timer
            // rather than leaving a reattached running command stuck at 0s.
            if status.running && !status.disconnected {
                self.command_started.entry(pane).or_insert_with(std::time::Instant::now);
            } else if !status.running {
                self.command_started.remove(&pane);
            }
            let previous = self.owner_seen.entry(pane).or_default();
            let completed = status.completion > *previous;
            *previous = status.completion;
            let mut moved = false;
            let mut started = false;
            for tab in &mut self.tabs {
                if let Some(leaf) = tab.leaves.get_mut(&pane) {
                    let running = status.running && !status.disconnected;
                    started |= running && !leaf.activity.running;
                    changed |= leaf.activity.running != running;
                    leaf.activity.running = running;
                    if running {
                        leaf.activity.completed = 0;
                        leaf.activity.exit_code = None;
                    }
                    if completed {
                        self.command_completion += 1;
                        leaf.activity.completed = self.command_completion;
                        leaf.activity.exit_code = status.exit_code;
                        changed = true;
                    }
                    if let Some(cwd) = &status.cwd {
                        if leaf.cwd != *cwd {
                            leaf.cwd.clone_from(cwd);
                            moved = true;
                        }
                    }
                }
            }
            if started {
                self.start_recent_work(pane);
            }
            if moved {
                self.touch_recent(pane);
                changed = true;
            }
        }
        changed
    }

    pub(super) fn poll_notification_activation(&mut self, cx: &mut Context<Self>) {
        let mut targets: Vec<_> = self.notification_rx.try_iter().collect();
        for (&pane, session) in &self.sessions {
            if let Some(status) = session.owner_status() {
                let seen = self.owner_activation.entry(pane).or_default();
                if status.activation > *seen {
                    *seen = status.activation;
                    targets.push(pane);
                }
            }
        }
        for pane in targets {
            self.focus_leaf(pane, cx);
            cx.activate(true);
            cx.defer(|cx| {
                for window in cx.windows() {
                    let _ = window.update(cx, |_, window, _| crate::desktop::show_window(window));
                }
            });
        }
    }

    fn open_recent(&mut self, entry: &RecentProject, cx: &mut Context<Self>) {
        if let Some(pane) = entry.pane.filter(|pane| {
            self.tabs.iter().any(|tab| tab.leaves.contains_key(pane))
                && self
                    .session(*pane)
                    .is_some_and(|session| session.exit_code().is_none())
        }) {
            self.focus_leaf(pane, cx);
            return;
        }
        if !Path::new(&entry.cwd).is_dir() {
            self.show_toast(
                "Project folder is unavailable. Use × beside the entry to dismiss it.",
                cx,
            );
            return;
        }
        let pane = self.next_leaf_id;
        let tab = crate::state::TabState::single(self.next_tab_id, entry.name(), pane);
        self.next_leaf_id += 1;
        self.next_tab_id += 1;
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.active_tab_mut().leaves.get_mut(&pane).unwrap().cwd = entry.cwd.clone();
        self.spawn_session(pane);
        self.touch_recent(pane);
        cx.notify();
    }
}

fn elapsed_seconds(started_at_ms: i64, now_ms: i64) -> u64 {
    now_ms.saturating_sub(started_at_ms).max(0) as u64 / 1000
}

fn format_elapsed(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

#[cfg(test)]
mod recent_time_tests {
    use super::*;
    #[test]
    fn elapsed_labels_switch_units_at_the_exact_boundary() {
        for (seconds, expected) in [(0,"0s"), (30,"30s"), (59,"59s"), (60,"1m"),
            (3599,"59m"), (3600,"1h"), (86399,"23h"), (86400,"1d"), (172800,"2d")] {
            assert_eq!(format_elapsed(seconds), expected);
        }
        assert_eq!(elapsed_seconds(10_000, 9_000), 0);
        assert_eq!(elapsed_seconds(10_000, 70_999), 60);
    }
}

struct RecentTooltip {
    text: String,
    theme: crate::theme::Theme,
}

impl gpui::Render for RecentTooltip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p(px(7.))
            .max_w(px(450.))
            .rounded(px(6.))
            .bg(self.theme.bg)
            .border_1()
            .border_color(self.theme.border)
            .text_color(self.theme.fg)
            .text_size(px(11.))
            .child(self.text.clone())
    }
}

pub(super) fn render(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let mut list = div()
        .id("recent-projects-list")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .px(px(8.))
        .py(px(4.))
        .gap(px(4.))
        .overflow_y_scroll();
    if app.recent_projects.is_empty() {
        list = list.child(
            div()
                .p(px(10.))
                .text_color(theme.fg_dim)
                .child("Your projects will appear here"),
        );
    }
    for (index, entry) in app.recent_projects.iter().enumerate() {
        let open = entry.clone();
        let dismiss = entry.clone();
        let live = entry.pane.and_then(|pane| {
            app.tabs
                .iter()
                .find_map(|tab| tab.leaves.get(&pane).map(|leaf| (tab, leaf)))
        });
        let activity = live.map(|(_, leaf)| leaf.activity);
        let session = entry.pane.and_then(|pane| app.session(pane));
        let owner = session.and_then(|session| session.owner_status());
        let launch = session.and_then(|session| session.launch_config());
        let ended = session.is_some_and(|session| session.exit_code().is_some());
        let tracked = launch.as_ref().is_some_and(|config| config.track_process
            || config.env.iter().any(|(name, value)| name == "SPRITX_COMMAND_TRACKING" && value == "1"));
        let disconnected = owner.as_ref().is_some_and(|status| status.disconnected);
        let status = match activity {
            _ if disconnected => "Disconnected".into(),
            Some(activity) if activity.running && !ended => "Running".to_string(),
            Some(activity) if activity.completed > 0 => match activity.exit_code {
                Some(0) => "Completed".into(),
                Some(code) => format!("Exit {code}"),
                None => "Completed".into(),
            },
            _ if ended || live.is_none() => "Closed".into(),
            _ if !tracked => "Status unavailable".into(),
            _ => "Idle".into(),
        };
        let running = activity.is_some_and(|activity| activity.running) && !ended && !disconnected;
        let completed = activity.is_some_and(|activity| activity.completed > 0) && !running && !disconnected;
        let failed = completed && activity.and_then(|activity| activity.exit_code).is_some_and(|code| code != 0);
        let status_color = if disconnected || failed { theme.danger }
            else if running || completed { theme.accent } else { theme.fg_dim };
        let command = launch.as_ref().and_then(|config| config.display_name.clone())
            .or_else(|| owner.as_ref().and_then(|status| status.command.clone()))
            .or_else(|| entry.pane.and_then(|pane| app.block_trackers.get(&pane))
                .and_then(|tracker| tracker.current_command()).map(str::to_owned));
        let activity_label = if running {
            command.as_deref().filter(|text| !text.trim().is_empty()).unwrap_or("Session")
        } else { match status.as_str() {
            "Completed" => "Done", "Disconnected" => "Offline",
            "Status unavailable" => "Unknown", other => other,
        }};
        let elapsed = if let Some(started) = owner.as_ref().and_then(|status| status.started_at_ms) {
            elapsed_seconds(started, workspace::now_ms())
        } else {
            entry.pane.and_then(|pane| app.command_started.get(&pane)).map(|at| at.elapsed().as_secs()).unwrap_or(0)
        };
        let elapsed_label = format_elapsed(elapsed);
        let tip = format!("{} · {}\n{}\n{}{}", entry.name(), activity_label, entry.cwd, status,
            if running { format!(" · {elapsed_label}") } else { String::new() });
        let selected = entry.pane.is_some_and(|pane|
            app.active_tab_checked().is_some_and(|tab| tab.active_leaf == pane));
        let state = div().flex_1().min_w_0().max_w(px(70.))
            .overflow_hidden().text_ellipsis().whitespace_nowrap()
            .text_size(px(10.)).text_color(theme.fg_dim).child(activity_label.to_string());
        let row = div()
            .id(("recent-project", index))
            .flex().flex_none().items_center()
            .h(px(32.)).min_w_0().gap(px(5.))
            .px(px(8.)).rounded(px(7.))
            .bg(if selected { theme.white_08 } else { theme.white_04 })
            .cursor_pointer()
            .hover(|style| style.bg(theme.white_08))
            .tooltip(move |_, cx| {
                cx.new(|_| RecentTooltip { text: tip.clone(), theme }).into()
            })
            .child(icon(Icon::ChatBot, 14., if selected { theme.fg } else { theme.fg_dim }))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis()
                .whitespace_nowrap().text_size(px(11.5))
                .font_weight(gpui::FontWeight::MEDIUM).text_color(theme.fg)
                .child(entry.name()))
            .child(div().flex_none().w(px(1.)).h(px(10.)).bg(theme.border))
            .child(state)
            .when(running, |row| row
                .child(div().flex_none().text_size(px(10.)).text_color(theme.fg_dim).child(elapsed_label))
                .child(super::tab_bar::activity_wave(status_color, app.settings.reduced_motion)))
            .when(!running && completed, |row| row.child(crate::components::buttons::dot(status_color, 4.)));
        let row = row.child(
            div()
                .id(("dismiss-recent", index))
                .flex().flex_none().items_center().justify_center()
                .size(px(16.))
                .rounded(px(4.))
                .text_color(theme.fg_dim)
                .hover(|style| {
                    style
                        .text_color(theme.danger)
                        .bg(theme.danger.opacity(0.08))
                })
                .tooltip(move |_, cx| {
                    cx.new(|_| RecentTooltip {
                        text: "Remove from Recent".into(),
                        theme,
                    })
                    .into()
                })
                .child(icon(Icon::X, 10., theme.fg_dim))
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.recent_projects.retain(|entry| {
                        entry.project != dismiss.project || entry.pane != dismiss.pane
                    });
                    app.checkpoint_workspace();
                    cx.stop_propagation();
                    cx.notify();
                })),
        );
        list = list.child(row.on_click(cx.listener(move |app, _, window, cx| {
            window.focus(&app.focus_handle);
            app.open_recent(&open, cx);
            if let Some(pane) = open.pane {
                if let Some(leaf) = app.active_tab_checked_mut().and_then(|tab| tab.leaves.get_mut(&pane)) {
                    leaf.activity.completed = 0;
                }
            }
        })));
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .px(px(10.))
                .py(px(8.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("Recent"),
        )
        .child(list)
        .into_any_element()
}
