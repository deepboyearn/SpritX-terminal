use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, Render, ScrollHandle, SharedString, Window, deferred, div, prelude::*, px,
};

use crate::components::buttons::{dot, icon_button};
use crate::components::icons::{Icon, icon};
use crate::state::{
    LayoutPreset, LAYOUT_PRESETS, LeafState, Modal, PaneNode, PaletteMode, SettingsData,
    SettingsField, SettingsMenu, SplitDirection, TabId, TabState, ThemeColorKey, Toast,
    MAX_PANES_PER_TAB,
};
use crate::backend::config::AppConfig;
use crate::backend::history::{HistoryEntry, HistoryStore};
use crate::backend::osc::{BlockTracker, CommandBlock};
use crate::backend::remote::TabInfo;
use crate::backend::remote_server::{
    Bind, RemoteInput, RemoteServer, RemoteStatus, ServerOptions, cloudflared_available,
    listing_json,
};
use crate::terminal::{TerminalSession, DEFAULT_COLS, DEFAULT_ROWS};
use crate::theme::{Theme, body_pad};

/// How long a toast stays up before dismissing itself.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// Palette row index for the synthetic "Connect to another SpritX" entry, which is
/// not part of the fixed command list.
const PALETTE_CONNECT_ROW: usize = usize::MAX;

/// Floating tab context menu (C47–C52). Coordinates are window points.
#[derive(Clone, Debug)]
pub struct TabMenuState {
    pub tab_id: TabId,
    pub x: f32,
    pub y: f32,
}

/// Floating pane context menu (C39–C46).
#[derive(Clone, Debug)]
pub struct PaneMenuState {
    pub leaf_id: u64,
    pub x: f32,
    pub y: f32,
}

/// Active divider drag; `path` addresses a split in the pane tree.
#[derive(Clone, Debug)]
pub struct SplitDragState {
    pub path: Vec<usize>,
    pub direction: SplitDirection,
}

/// Everything the UI mirrors from the remote server: running, connected
/// clients, a pairing code being open, and its remaining seconds. The countdown
/// is part of the stamp so it ticks without an external repaint nudge.
type RemoteStamp = (bool, usize, bool, Option<u64>, Vec<u64>);

pub struct SpritXApp {
    pub(super) desktop_quitting: bool,
    pub theme: Theme,
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub next_tab_id: u64,
    pub next_leaf_id: u64,
    pub focus_handle: FocusHandle,
    pub cursor_blink_on: bool,
    pub cursor_blink_epoch: std::time::Instant,
    pub file_tree_visible: bool,
    pub file_tree_width: f32,
    pub file_list_collapsed: bool,
    pub(super) file_list_scroll: gpui::UniformListScrollHandle,
    pub(super) file_scroll_thumb_visible: bool,
    pub(super) file_scroll_hide_task: Option<gpui::Task<()>>,
    pub(super) cli_launcher: super::cli_launcher::Launcher,
    pub modal: Option<Modal>,
    pub layouts_open: bool,
    pub remote_menu_open: bool,
    /// Where the remote menu was opened from. `None` anchors it to the toolbar
    /// icon; a pane's context menu passes the click point so the panel appears
    /// under the cursor instead of across the window.
    pub remote_menu_at: Option<(f32, f32)>,
    /// Live SpritX↔SpritX links, keyed by the local leaf rendering them. Dropping an
    /// entry (closing the pane) tears the link down.
    pub remote_links: HashMap<u64, Arc<crate::backend::remote_client::RemoteLink>>,
    /// Last reported status per link, so a transition can be announced once.
    remote_link_notes: HashMap<u64, Option<String>>,
    /// The live remote-control server, while sharing is on. Every read of the
    /// UI goes through `remote_status`, so there is no stale mirror to drift.
    pub remote_server: Option<RemoteServer>,
    /// Last status stamp seen by the pump, so a change can request a repaint.
    remote_status_stamp: RemoteStamp,
    /// Change detection for the pane list handed to remote clients.
    remote_tab_signature: u64,
    /// Device count at the last tick, so a new pairing can be announced.
    remote_device_count: Option<usize>,
    pub toast: Option<Toast>,
    pub toast_generation: u64,
    /// The pending toast dismissal; replaced per toast so bursts do not
    /// accumulate detached timers (dropping the old Task cancels it).
    pub(super) toast_task: Option<gpui::Task<()>>,
    pub settings: SettingsData,
    pub settings_section: crate::state::SettingsSection,
    pub palette_query: String,
    pub palette_selected: usize,
    pub theme_names: Vec<&'static str>,
    pub selected_theme: usize,
    /// Which Settings dropdown/inline list is expanded (Font, Cursor style,
    /// a color picker).
    pub settings_menu: Option<SettingsMenu>,
    /// Which numeric Settings field is being typed into.
    pub settings_field: Option<SettingsField>,
    /// Buffer for the field currently being edited.
    pub settings_buf: String,
    /// Installed font families, shared by terminal fallback and the font picker.
    pub settings_fonts: Vec<String>,
    /// Whether the 16 ANSI swatches are expanded in Appearance.
    pub ansi_open: bool,
    pub recording_action: Option<String>,
    pub file_tree_ui: Option<String>,
    pub tab_scroll: ScrollHandle,
    pub(super) command_completion: u64,
    pub(super) tab_highlight: std::rc::Rc<std::cell::RefCell<Option<super::tab_bar::TabHighlight>>>,
    pub tab_menu: Option<TabMenuState>,
    pub pane_menu: Option<PaneMenuState>,
    pub editing_tab: Option<TabId>,
    pub editing_title: String,
    pub split_drag: Option<SplitDragState>,
    pub sessions: HashMap<u64, TerminalSession>,
    pub session_errors: HashMap<u64, String>,
    pub block_trackers: HashMap<u64, BlockTracker>,
    pub history: Option<HistoryStore>,
    pub recent_projects: Vec<crate::backend::workspace::RecentProject>,
    pub(super) workspace_last_save: std::time::Instant,
    pub(super) restored_sessions: HashMap<u64, crate::backend::workspace::SavedSession>,
    pub(super) owner_seen: HashMap<u64, u64>,
    pub(super) owner_activation: HashMap<u64, u64>,
    pub(super) command_started: HashMap<u64, std::time::Instant>,
    pub(super) notification_tx: std::sync::mpsc::Sender<u64>,
    pub(super) notification_rx: std::sync::mpsc::Receiver<u64>,
}

impl SpritXApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Err(error) = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(
            include_bytes!("../../assets/fonts/JetBrainsMonoNLNerdFontMono-Regular.ttf"),
        )]) {
            eprintln!("[spritx] could not register bundled terminal font: {error}");
        }
        let mut available_fonts = cx.text_system().all_font_names();
        available_fonts.retain(|name| !name.trim().is_empty() && !name.starts_with('.') && crate::terminal::font_supports_grid(cx.text_system(), name));
        available_fonts.sort();
        available_fonts.dedup();
        let app_config = AppConfig::load();
        crate::terminal::set_light_workspace(app_config.appearance.light_workspace);
        let theme = if app_config.appearance.light_workspace {
            Theme::light()
        } else {
            Theme::dark()
        };
        let settings = settings_from_config(&app_config);
        let history = HistoryStore::open_default().map(|store| {
            store.with_ignore(
                settings
                    .history_ignore
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        });
        let tabs = vec![TabState::single(1, "Terminal", 1)];
        let (notification_tx, notification_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            desktop_quitting: false,
            theme,
            tabs,
            active_tab: 0,
            next_tab_id: 2,
            next_leaf_id: 2,
            focus_handle: cx.focus_handle(),
            cursor_blink_on: true,
            cursor_blink_epoch: std::time::Instant::now(),
            file_tree_visible: true,
            file_tree_width: 230.0,
            // Exercise the expanded tree and its background watcher in UI smoke tests.
            file_list_collapsed: std::env::var("SPRITX_OPEN").as_deref() != Ok("file-tree"),
            file_list_scroll: Default::default(),
            file_scroll_thumb_visible: false,
            file_scroll_hide_task: None,
            cli_launcher: super::cli_launcher::Launcher::new(app_config.cli_launcher.clone()),
            modal: match std::env::var("SPRITX_OPEN").as_deref() {
                Ok("settings" | "settings-general" | "settings-notifications") => Some(Modal::Settings),
                Ok("palette") => Some(Modal::Palette(PaletteMode::Command)),
                Ok("remote") => Some(Modal::Remote),
                _ => None,
            },
            layouts_open: false,
            // The toolbar popups are not reachable by key, so the same
            // automation hook that opens modals opens them for smoke tests.
            remote_menu_open: std::env::var("SPRITX_OPEN")
                .as_deref()
                == Ok("remote-menu"),
            remote_menu_at: None,
            remote_links: HashMap::new(),
            remote_link_notes: HashMap::new(),
            remote_server: None,
            remote_status_stamp: RemoteStamp::default(),
            remote_tab_signature: 0,
            remote_device_count: None,
            toast: None,
            toast_generation: 0,
            toast_task: None,
            settings,
            settings_section: match std::env::var("SPRITX_OPEN").as_deref() {
                Ok("settings-general") => crate::state::SettingsSection::General,
                Ok("settings-notifications") => crate::state::SettingsSection::Notifications,
                _ => crate::state::SettingsSection::Appearance,
            },
            palette_query: String::new(),
            palette_selected: 0,
            theme_names: crate::state::sample_theme_names(),
            selected_theme: theme_index_for(&app_config.appearance.theme),
            settings_menu: None,
            settings_field: None,
            settings_buf: String::new(),
            settings_fonts: available_fonts,
            ansi_open: false,
            recording_action: None,
            file_tree_ui: None,
            tab_scroll: ScrollHandle::new(),
            command_completion: 0,
            tab_highlight: Default::default(),
            tab_menu: None,
            pane_menu: None,
            editing_tab: None,
            editing_title: String::new(),
            split_drag: None,
            sessions: HashMap::new(),
            session_errors: HashMap::new(),
            block_trackers: HashMap::new(),
            history,
            recent_projects: crate::backend::workspace::load().recent,
            workspace_last_save: std::time::Instant::now(),
            restored_sessions: HashMap::new(),
            owner_seen: HashMap::new(),
            owner_activation: HashMap::new(),
            command_started: HashMap::new(),
            notification_tx, notification_rx,
        };
        crate::terminal::set_appearance(terminal_appearance_from_config(&app_config, &app.settings_fonts));
        crate::terminal::set_term_theme(terminal_theme_from_config(&app_config));
        app.restore_workspace();
        // Register the wake signal before any reader thread can produce output.
        app.start_terminal_pump(cx);
        app.ensure_sessions();
        if let Some(tab) = app.active_tab_checked() { app.touch_recent(tab.active_leaf); }
        super::cli_launcher::start_scanning(&mut app, cx);
        cx.on_app_quit(|this, _| {
            this.checkpoint_workspace();
            async {}
        }).detach();
        cx.on_release(|this, _| this.checkpoint_workspace()).detach();
        app
    }

    fn start_terminal_pump(&self, cx: &mut Context<Self>) {
        // The PTY reader signals this the moment output arrives, so echo
        // appears on the very next frame instead of after a poll interval.
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        crate::terminal::set_wake_signal(wake.clone());
        // A phone keystroke wakes the same loop, so remote input is handled the
        // instant it lands instead of on the next 500 ms safety tick.
        let remote_wake = std::sync::Arc::new(tokio::sync::Notify::new());
        crate::backend::remote_server::set_remote_wake(remote_wake.clone());
        // Inbound output from another SpritX wakes the same loop.
        crate::backend::remote_client::set_client_wake(remote_wake.clone());

        let event_wake = wake.clone();
        // Stamped whenever a PTY reader delivers bytes; the gap to the
        // resumed task is time the UI task was not being polled at all.
        let woke = crate::terminal::wake_stamp();
        cx.spawn(async move |this, cx| loop {
            tokio::select! {
                _ = event_wake.notified() => {}
                _ = remote_wake.notified() => {}
            }
            let lag = {
                let stamp = woke.lock().ok().and_then(|slot| *slot);
                stamp.map(|at| crate::metrics::micros(at.elapsed()))
            };
            if let Some(lag) = lag {
                crate::metrics::probe("pump.wait", lag);
                crate::metrics::add(crate::metrics::Name::PumpWait, lag as u64);
            }
            let alive = this
                .update(cx, |this, cx| {
                    if this.pump_terminals() {
                        cx.notify();
                    }
                    if this.refresh_remote_links(cx) {
                        cx.notify();
                    }
                    this.remote_announce_pairing(cx);
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        // A slow safety pump that catches anything the wake signal could miss
        // (e.g. a session created before the signal was set).
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(500)).await;
                let alive = this
                    .update(cx, |this, cx| {
                        let visible = crate::terminal::cursor_blink_visible(
                            this.settings.cursor_blink, this.cursor_blink_epoch.elapsed(),
                        );
                        if visible != this.cursor_blink_on {
                            this.cursor_blink_on = visible;
                            if this.modal.is_none() { cx.notify(); }
                        }
                        if this.pump_terminals() {
                            cx.notify();
                        }
                        // Elapsed labels must advance even with reduced motion
                        // and a quiet CLI that produces no new output.
                        if this.tabs.iter().any(|tab| tab.leaves.values().any(|leaf| leaf.activity.running)) {
                            cx.notify();
                        }
                        this.poll_notification_activation(cx);
                        if this.workspace_last_save.elapsed() >= std::time::Duration::from_secs(2) {
                            this.checkpoint_workspace();
                        }
                        if this.refresh_remote_links(cx) {
                        cx.notify();
                    }
                    this.remote_announce_pairing(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn ensure_sessions(&mut self) {
        let leaves: Vec<u64> = self
            .tabs
            .iter()
            .flat_map(|tab| tab.leaf_ids())
            .collect();
        for leaf in leaves {
            self.spawn_session(leaf);
        }
    }

    pub fn spawn_session(&mut self, leaf: u64) {
        if self.sessions.contains_key(&leaf) {
            return;
        }
        if self.tabs.iter().any(|tab| tab.leaves.get(&leaf).is_some_and(|leaf| leaf.remote)) {
            self.session_errors.insert(leaf, "Reconnect this remote terminal using Remote control.".into());
            return;
        }
        if self.restore_pane_session(leaf) { return; }
        let args = match crate::platform::parse_shell_args(&self.settings.shell_args) {
            Ok(args) => args,
            Err(error) => {
                self.session_errors.insert(leaf, error);
                return;
            }
        };
        let config = crate::terminal::SessionConfig::for_shell(
            Some(&self.settings.shell_program),
            &args,
            self.tabs.iter().find_map(|tab| tab.leaves.get(&leaf).map(|leaf| leaf.cwd.clone())),
        );
        match self.spawn_workspace_session(config) {
            Ok(session) => {
                self.session_errors.remove(&leaf);
                self.sessions.insert(leaf, session);
                self.checkpoint_workspace();
            }
            Err(error) => {
                self.session_errors.insert(leaf, error.to_string());
            }
        }
    }

    pub fn pump_terminals(&mut self) -> bool {
        let pumped = self.pump_remote();
        let mut updates = Vec::new();
        let mut parse_us = 0u128;
        for (leaf, session) in self.sessions.iter_mut() {
            let started = std::time::Instant::now();
            let pumped_bytes = session.pump_bytes();
            parse_us += crate::metrics::micros(started.elapsed());
            // The session's modes live here, and a client cannot recover them
            // from the replay tail it is sent, so publish them every tick —
            // the server only encodes them again when they change.
            if let Some(server) = self.remote_server.as_ref() {
                server.set_modes(*leaf, session.session_modes());
            }
            if let Some(bytes) = pumped_bytes {
                updates.push((*leaf, bytes));
            }
        }
        if crate::metrics::enabled() && !updates.is_empty() {
            crate::metrics::probe("pump.vt", parse_us);
            crate::metrics::add(crate::metrics::Name::PumpVt, parse_us as u64);
        }
        let had_output = !updates.is_empty();
        for (leaf, bytes) in updates {
            // Fan the same bytes out to remote clients: the UI parser already
            // consumed them in `pump_bytes`, this only notifies bridges.
            if let Some(server) = self.remote_server.as_ref() {
                server.ingest_screen(leaf, &bytes, self.sessions.get(&leaf).map(|session| session.screen()));
            }
            let osc_started = std::time::Instant::now();
            let (events, blocks, cwd) = {
                let tracker = self.block_trackers.entry(leaf).or_default();
                let events = tracker.feed(&bytes).events;
                (events, tracker.take_blocks(), tracker.current_cwd().map(str::to_string))
            };
            for event in events {
                use crate::backend::osc::OscEvent;
                match event.event {
                    OscEvent::OutputStart => {
                        self.command_started.insert(leaf, std::time::Instant::now());
                        if let Some(state) = self.leaf_mut(leaf) {
                            state.activity.running = true;
                            state.activity.completed = 0;
                            state.activity.exit_code = None;
                        }
                        self.start_recent_work(leaf);
                    }
                    OscEvent::OutputEnd { exit_code } => {
                        if self.session(leaf).is_some_and(|session| session.owner_connection().is_none()) {
                            self.complete_terminal_command(leaf, exit_code);
                        }
                    }
                    _ => {}
                }
            }
            let osc_us = crate::metrics::micros(osc_started.elapsed());
            if crate::metrics::enabled() {
                crate::metrics::probe("pump.osc", osc_us);
                crate::metrics::add(crate::metrics::Name::PumpOsc, osc_us as u64);
            }
            // OSC 133 blocks are used only to record command history and track
            // the pane's cwd now that the Blocks panel has been removed.
            if let Some(cwd) = cwd {
                let mut moved = false;
                if let Some(leaf_state) = self.leaf_mut(leaf) {
                    moved = leaf_state.cwd != cwd;
                    leaf_state.cwd = cwd;
                }
                if moved { self.touch_recent(leaf); }
            }
            for block in blocks {
                self.append_history(&block);
            }
        }
        let exited: Vec<_> = self.tabs.iter().flat_map(|tab| tab.leaves.values())
            .filter(|leaf| leaf.activity.running)
            .filter_map(|leaf| Some((leaf.id, self.sessions.get(&leaf.id)?.exit_code()?)))
            .collect();
        let synced = self.sync_owned_status();
        let changed = had_output || !exited.is_empty() || synced;
        for (leaf, exit_code) in exited {
            self.complete_terminal_command(leaf, exit_code);
        }
        if changed {
            self.refresh_remote_tabs();
        }
        changed || pumped
    }

    fn complete_terminal_command(&mut self, leaf: u64, exit_code: i32) {
        if !self.tabs.iter().any(|tab| tab.leaves.get(&leaf).is_some_and(|state| state.activity.running)) {
            return;
        }
        self.command_completion += 1;
        let completed = self.command_completion;
        let Some(state) = self.leaf_mut(leaf) else { return };
        state.activity.running = false;
        state.activity.completed = completed;
        state.activity.exit_code = Some(exit_code);
        if self.session(leaf).is_some_and(|session| session.owner_connection().is_none()) {
            let elapsed = self.command_started.remove(&leaf).map(|at| at.elapsed()).unwrap_or_default();
            let cwd = self.tabs.iter().find_map(|tab| tab.leaves.get(&leaf).map(|leaf| leaf.cwd.clone())).unwrap_or_default();
            let sender = self.notification_tx.clone();
            let name = self.session(leaf).and_then(|session| session.launch_config()).and_then(|config| config.display_name).unwrap_or_else(|| "Command".into());
            crate::platform::command_completed(&cwd, &name, exit_code, elapsed, move || { let _ = sender.send(leaf); });
        }
    }

    fn leaf_mut(&mut self, leaf: u64) -> Option<&mut LeafState> {
        for tab in &mut self.tabs {
            if let Some(state) = tab.leaves.get_mut(&leaf) {
                return Some(state);
            }
        }
        None
    }

    fn append_history(&self, block: &CommandBlock) {
        if !self.settings.history_enabled {
            return;
        }
        let Some(command) = block.command.as_deref() else {
            return;
        };
        if command.starts_with(' ') || command.trim().is_empty() {
            return;
        }
        if let Some(history) = &self.history {
            let mut entry = HistoryEntry::new(command.to_string());
            entry.cwd = block.cwd.clone();
            entry.exit_code = block.exit_code.unwrap_or(0);
            entry.duration_ms = block
                .started_at
                .elapsed()
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or(0);
            entry.finished_at = now_ms();
            let _ = history.append(entry);
        }
    }

    pub fn save_settings(&self) {
        crate::metrics::event("action.save_settings");
        let mut config = AppConfig::load();
        apply_settings(&mut config, &self.settings);
        config.appearance.light_workspace = self.theme.bg.l >= 0.5;
        write_theme(&mut config, &crate::terminal::configured_term_theme());
        let _ = config.save();
        crate::terminal::set_appearance(terminal_appearance_from_config(&config, &self.settings_fonts));
    }

    /// Expand/collapse a Settings dropdown (toggling the same one closes it).
    pub fn toggle_settings_menu(&mut self, menu: SettingsMenu, cx: &mut Context<Self>) {
        self.settings_menu = if self.settings_menu == Some(menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }

    /// Apply a theme preset from Settings → Appearance.
    pub fn apply_theme_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(preset) = crate::theme_presets::PRESETS.get(index) else {
            return;
        };
        self.apply_palette_and_mode(preset.theme());
        self.settings.theme = preset.name;
        self.selected_theme = index;
        self.save_settings();
        cx.notify();
    }

    /// Replace one of the five primary terminal colors (color picker).
    pub fn set_theme_color(&mut self, key: ThemeColorKey, value: u32, cx: &mut Context<Self>) {
        let mut theme = crate::terminal::term_theme();
        let color: gpui::Hsla = gpui::rgb(value).into();
        match key {
            ThemeColorKey::Background => theme.background = color,
            ThemeColorKey::Foreground => theme.foreground = color,
            ThemeColorKey::Accent => theme.accent = color,
            ThemeColorKey::Cursor => theme.cursor = color,
            ThemeColorKey::Selection => theme.selection = color,
        }
        self.apply_palette_and_mode(theme);
        self.selected_theme = usize::MAX;
        self.settings.theme = "Custom";
        self.save_settings();
        cx.notify();
    }

    fn apply_palette_and_mode(&mut self, palette: crate::terminal::TerminalTheme) {
        let light = palette.background.l >= 0.5;
        crate::terminal::set_light_workspace(light);
        crate::terminal::set_term_theme(palette);
        self.theme = if light { Theme::light() } else { Theme::dark() };
    }

    /// Commit the inline field being edited (Enter / blur). Clamps numerics.
    pub fn commit_settings_field(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.settings_field.take() else {
            return;
        };
        if field == SettingsField::ShellArgs {
            if let Err(error) = crate::platform::parse_shell_args(&self.settings_buf) {
                self.settings_field = Some(field);
                self.show_toast(error, cx);
                return;
            }
        }
        field.commit(&mut self.settings, &self.settings_buf);
        if field == SettingsField::HistoryIgnore {
            self.refresh_history_ignore();
        }
        self.settings_buf.clear();
        self.save_settings();
        cx.notify();
    }

    /// Begin inline editing of a Settings field, seeding the buffer with the
    /// current value.
    pub fn begin_settings_edit(&mut self, field: SettingsField) {
        self.settings_field = Some(field);
        self.settings_buf = field.current(&self.settings);
    }

    /// Replace the settings snapshot from a freshly loaded config.
    fn reload_from_config(&mut self, config: &AppConfig, cx: &mut Context<Self>) {
        crate::terminal::set_light_workspace(config.appearance.light_workspace);
        self.theme = if config.appearance.light_workspace {
            Theme::light()
        } else {
            Theme::dark()
        };
        self.settings = settings_from_config(config);
        self.selected_theme = theme_index_for(&config.appearance.theme);
        crate::terminal::set_appearance(terminal_appearance_from_config(config, &self.settings_fonts));
        crate::terminal::set_term_theme(terminal_theme_from_config(config));
        self.refresh_history_ignore();
        cx.notify();
    }

    pub fn refresh_history_ignore(&mut self) {
        let patterns: Vec<String> = self
            .settings
            .history_ignore
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        self.history = HistoryStore::open_default().map(|store| store.with_ignore(patterns));
    }

    /// Restore the default file-tree context-menu commands.
    pub fn reset_file_tree_commands(&mut self, cx: &mut Context<Self>) {
        let defaults = crate::backend::config::FileTreeConfig::default();
        self.settings.file_tree_list = defaults.dir_list;
        self.settings.file_tree_open = defaults.dir_open;
        self.settings.file_tree_view = defaults.file_view;
        self.settings.file_tree_edit = defaults.file_edit;
        self.settings.file_tree_open_file = defaults.file_open;
        self.save_settings();
        self.show_toast("File tree commands reset", cx);
    }

    /// Delete the on-disk command history.
    pub fn clear_history(&mut self, cx: &mut Context<Self>) {
        match crate::backend::history::clear() {
            Ok(()) => self.show_toast("Command history cleared", cx),
            Err(error) => self.show_toast(format!("Could not clear history: {error}"), cx),
        }
    }

    /// Reveal the config directory in the platform file manager.
    pub fn reveal_config(&mut self, cx: &mut Context<Self>) {
        let directory = config_dir();
        cx.reveal_path(&directory);
        self.show_toast(format!("{}", directory.display()), cx);
    }

    /// Export the whole configuration to a JSON file the user picks.
    pub fn export_config(&mut self, cx: &mut Context<Self>) {
        let mut config = AppConfig::load();
        apply_settings(&mut config, &self.settings);
        write_theme(&mut config, &crate::terminal::configured_term_theme());
        let json = match config.export_json() {
            Ok(json) => json,
            Err(error) => {
                self.show_toast(format!("Could not export configuration: {error}"), cx);
                return;
            }
        };
        let receiver = cx.prompt_for_new_path(&config_dir(), Some("spritx-config.json"));
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |app, cx| match result {
                Ok(Ok(Some(path))) => match std::fs::write(&path, json) {
                    Ok(()) => app.show_toast(format!("Exported to {}", path.display()), cx),
                    Err(error) => app.show_toast(format!("Could not write file: {error}"), cx),
                },
                Ok(Ok(None)) => {}
                Ok(Err(error)) => app.show_toast(format!("Export failed: {error}"), cx),
                Err(_) => {}
            })
            .ok();
        })
        .detach();
    }

    /// Import a config JSON the user picks, then reload every setting.
    pub fn import_config(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select a SpritX config to import".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |app, cx| match result {
                Ok(Ok(Some(paths))) => {
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    match crate::backend::config::import_file(&path) {
                        Ok(config) => {
                            let _ = config.save();
                            app.reload_from_config(&config, cx);
                            app.show_toast("Configuration imported", cx);
                        }
                        Err(error) => {
                            app.show_toast(format!("Could not import configuration: {error}"), cx)
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => app.show_toast(format!("Import failed: {error}"), cx),
                Err(_) => {}
            })
            .ok();
        })
        .detach();
    }

    /// Pick a font family from the Appearance dropdown.
    pub fn select_font(&mut self, family: String, cx: &mut Context<Self>) {
        if !crate::terminal::font_supports_grid(cx.text_system(), &family) {
            self.settings_menu = None;
            self.show_toast("Choose a monospace font to keep terminal columns aligned", cx);
            return;
        }
        self.settings.font_family = format!("{family}, monospace");
        self.settings_menu = None;
        self.save_settings();
        self.show_toast(format!("Font set to {family}"), cx);
    }

    /// Pick a cursor shape from the Appearance dropdown.
    pub fn set_cursor_style(&mut self, style: &'static str, cx: &mut Context<Self>) {
        self.settings.cursor_style = style;
        self.settings_menu = None;
        self.save_settings();
        cx.notify();
    }

    /// Flip the caret blink toggle.
    pub fn toggle_cursor_blink(&mut self, cx: &mut Context<Self>) {
        self.settings.cursor_blink = !self.settings.cursor_blink;
        self.save_settings();
        cx.notify();
    }

    /// Load the system font families once, for the Appearance font picker.
    pub fn load_settings_fonts(&mut self, cx: &mut Context<Self>) {
        if !self.settings_fonts.is_empty() {
            return;
        }
        let mut names = cx.text_system().all_font_names();
        names.retain(|name| {
            let trimmed = name.trim();
            !trimmed.is_empty() && !trimmed.starts_with('.') && crate::terminal::font_supports_grid(cx.text_system(), name)
        });
        names.sort();
        names.dedup();
        self.settings_fonts = names;
    }

    /// Open a native file dialog, register the chosen font and select it.
    pub fn import_font(&mut self, cx: &mut Context<Self>) {
        let before = cx.text_system().all_font_names();
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select a font file".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |app, cx| match result {
                Ok(Ok(Some(paths))) => {
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            let added = cx
                                .text_system()
                                .add_fonts(vec![std::borrow::Cow::Owned(bytes)]);
                            match added {
                                Ok(()) => {
                                    let after = cx.text_system().all_font_names();
                                    app.settings_fonts = after.iter().filter(|name| crate::terminal::font_supports_grid(cx.text_system(), name)).cloned().collect();
                                    match after.iter().find(|name| !before.contains(*name)).cloned() {
                                        Some(family) => app.select_font(family, cx),
                                        None => app.show_toast(
                                            "Font registered but its family name could not be detected",
                                            cx,
                                        ),
                                    }
                                }
                                Err(error) => {
                                    app.show_toast(format!("Font import failed: {error}"), cx)
                                }
                            }
                        }
                        Err(error) => {
                            app.show_toast(format!("Could not read the font file: {error}"), cx)
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => app.show_toast(format!("Font dialog failed: {error}"), cx),
                Err(_) => app.show_toast("Font dialog was cancelled", cx),
            })
            .ok();
        })
        .detach();
    }

    pub fn write_active(&mut self, bytes: &[u8]) -> bool {
        crate::metrics::event(&format!("action.write_active bytes={}", bytes.len()));
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return false; };
        let Some(session) = self.sessions.get_mut(&leaf) else {
            return false;
        };
        session.clear_selection();
        // Typing always snaps back to the live prompt, like a normal terminal.
        session.scroll_to_bottom();
        let started = std::time::Instant::now();
        let ok = session.write(bytes).is_ok();
        if crate::metrics::enabled() {
            let us = crate::metrics::micros(started.elapsed());
            crate::metrics::probe("key-sent", us);
            crate::metrics::add(crate::metrics::Name::KeySent, us as u64);
        }
        ok
    }

    /// Send the terminal interrupt character to this pane's foreground job.
    pub fn interrupt_pane(&mut self, leaf: u64, cx: &mut Context<Self>) {
        let sent = self.sessions.get_mut(&leaf).is_some_and(|session| {
            session.clear_selection();
            session.scroll_to_bottom();
            session.write(&[0x03]).is_ok()
        });
        if !sent { self.show_toast("Could not interrupt this terminal", cx); }
        self.cursor_blink_on = true;
        self.cursor_blink_epoch = std::time::Instant::now();
        cx.notify();
    }

    /// Explicit fallback for an unresponsive local job. Kill only this pane's
    /// process tree, then reopen its configured shell in the same folder.
    pub fn force_stop_pane(&mut self, leaf: u64, cx: &mut Context<Self>) {
        if self.session(leaf).is_some_and(|session| session.is_remote()) {
            self.show_toast("Use force stop on the host terminal", cx);
            return;
        }
        if self.tab_index_for_leaf(leaf).is_none() { return; }
        self.teardown_leaf(leaf);
        if let Some(state) = self.leaf_mut(leaf) { state.activity = Default::default(); }
        self.spawn_session(leaf);
        self.checkpoint_workspace();
        self.show_toast("Process stopped; shell restarted", cx);
    }

    /// True when the active terminal is scrolled back into its history.
    pub fn active_scrolled(&self) -> bool {
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return false; };
        self.sessions
            .get(&leaf)
            .is_some_and(|session| session.screen().scrollback() > 0)
    }

    /// Send a keydown to the active terminal. Like a normal terminal, typing
    /// snaps the viewport back to the live prompt if it was scrolled up.
    pub fn type_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if let Some(bytes) = crate::keys::encode_key(event) {
            let was_scrolled = self.active_scrolled();
            let was_hidden = !self.cursor_blink_on;
            self.cursor_blink_epoch = std::time::Instant::now();
            self.cursor_blink_on = true;
            self.write_active(&bytes);
            if was_scrolled || was_hidden {
                cx.notify();
            }
        }
    }

    /// Type text into the active terminal. `execute` appends Enter.
    pub fn send_to_terminal(&mut self, text: &str, execute: bool) -> bool {
        crate::metrics::event(&format!("action.send_to_terminal execute={execute}"));
        let mut bytes = text.as_bytes().to_vec();
        if execute {
            bytes.push(b'\r');
        }
        self.write_active(&bytes)
    }

    /// Copy the active terminal's selection to the clipboard (Ctrl+Shift+C).
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return; };
        let text = self
            .sessions
            .get(&leaf)
            .and_then(|session| session.selection_text())
            .filter(|text| !text.trim().is_empty());
        match text {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.show_toast("Copied selection", cx);
            }
            None => self.show_toast("Nothing selected", cx),
        }
    }

    /// Select the whole visible terminal screen (Ctrl+Shift+A).
    pub fn select_all_active(&mut self, cx: &mut Context<Self>) {
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return; };
        if let Some(session) = self.sessions.get_mut(&leaf) {
            session.select_all();
            cx.notify();
        }
    }

    /// Paste the clipboard into the active terminal (Ctrl+Shift+V).
    pub fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()).filter(|text| !text.is_empty()) else {
            self.show_toast("Clipboard has no text to paste", cx);
            return;
        };
        if !self.write_active(text.as_bytes()) {
            self.show_toast("Could not paste into this terminal", cx);
        }
        cx.notify();
    }

    /// Zoom the terminal font by `delta` pixels (Ctrl + / Ctrl -).
    pub fn zoom_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.font_size = (self.settings.font_size + delta).clamp(8.0, 40.0);
        self.save_settings();
        cx.notify();
    }

    /// Scroll the active terminal: positive `lines` moves toward the live view.
    pub fn scroll_active(&mut self, lines: isize, cx: &mut Context<Self>) {
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return; };
        if let Some(session) = self.sessions.get_mut(&leaf) {
            if session.scroll_lines(lines) {
                cx.notify();
            }
        }
    }

    pub fn session(&self, leaf: u64) -> Option<&TerminalSession> {
        self.sessions.get(&leaf)
    }

    pub fn terminal_cols(&self) -> u16 {
        DEFAULT_COLS
    }

    pub fn terminal_rows(&self) -> u16 {
        DEFAULT_ROWS
    }

    fn prune_sessions(&mut self) {
        let live: std::collections::HashSet<u64> = self
            .tabs
            .iter()
            .flat_map(|tab| tab.leaf_ids())
            .collect();
        self.sessions.retain(|leaf, session| {
            if !live.contains(leaf) { session.kill(); return false; }
            true
        });
        self.session_errors.retain(|leaf, _| live.contains(leaf));
        self.block_trackers.retain(|leaf, _| live.contains(leaf));
        self.owner_seen.retain(|leaf, _| live.contains(leaf));
        self.owner_activation.retain(|leaf, _| live.contains(leaf));
        self.command_started.retain(|leaf, _| live.contains(leaf));
        self.restored_sessions.retain(|leaf, _| live.contains(leaf));
        // Dropping the last handle closes the link (see `RemoteLink::drop`), so
        // a removed pane cannot leave a reconnect loop behind.
        self.remote_links.retain(|leaf, _| live.contains(leaf));
        self.remote_link_notes.retain(|leaf, _| live.contains(leaf));
    }

    /// Drop every per-pane entry for a single leaf, killing its session and
    /// dropping its remote link (whose `Drop` tears the reconnect loop down).
    /// Called immediately on close so no stale entry outlives the pane.
    fn teardown_leaf(&mut self, leaf: u64) {
        if let Some(mut session) = self.sessions.remove(&leaf) {
            session.kill();
        }
        self.session_errors.remove(&leaf);
        self.block_trackers.remove(&leaf);
        self.owner_seen.remove(&leaf);
        self.owner_activation.remove(&leaf);
        self.command_started.remove(&leaf);
        self.restored_sessions.remove(&leaf);
        self.remote_links.remove(&leaf);
        self.remote_link_notes.remove(&leaf);
    }

    /// Keep a valid active index, using zero for an empty workspace.
    fn clamp_active_tab(&mut self) {
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    /// Empty workspaces have no active tab.
    pub fn active_tab_checked(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab).or_else(|| self.tabs.first())
    }

    pub(super) fn active_tab_checked_mut(&mut self) -> Option<&mut TabState> {
        self.clamp_active_tab();
        let idx = self.active_tab;
        if idx < self.tabs.len() {
            self.tabs.get_mut(idx)
        } else {
            self.tabs.first_mut()
        }
    }

    pub fn active_tab(&self) -> &TabState {
        // Read path is total even if `active_tab` somehow went stale:
        // fall back to the first tab rather than panicking the UI thread.
        // Only use this accessor after checking that a tab exists.
        debug_assert!(!self.tabs.is_empty(), "active_tab on an empty tab list");
        self.active_tab_checked().expect("tabs is non-empty (see debug_assert)")
    }

    pub fn active_tab_mut(&mut self) -> &mut TabState {
        self.clamp_active_tab();
        debug_assert!(!self.tabs.is_empty(), "active_tab_mut on an empty tab list");
        self.active_tab_checked_mut().expect("tabs is non-empty (see debug_assert)")
    }

    pub fn show_toast(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        crate::metrics::event(&format!("toast: {message}"));
        self.toast_generation = self.toast_generation.wrapping_add(1);
        let generation = self.toast_generation;
        self.toast = Some(Toast { message, generation });
        cx.notify();

        // Toasts are transient by nature: without a timer one stays on screen
        // until something else replaces it. The generation is checked so a
        // superseded toast's timer cannot clear the one now showing. The task
        // is stored (replacing any previous one) so bursts do not accumulate
        // detached timers — dropping the old Task cancels it.
        self.toast_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(TOAST_TTL).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|toast| toast.generation == generation) {
                    this.toast = None;
                    this.toast_task = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Return keyboard focus to the terminal once an overlay that captured
    /// it closes, instead of leaving focus on the dismissed popup. Only
    /// needed when the caller has a `Window`; the render-time force-focus
    /// below no longer steals focus while an overlay is open.
    pub(super) fn refocus_terminal(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }
    pub fn close_modal(&mut self) {
        crate::metrics::event("action.close_modal");
        self.modal = None;
        self.layouts_open = false;
        self.remote_menu_open = false;
        self.tab_menu = None;
        self.pane_menu = None;
        self.editing_tab = None;
        self.settings_menu = None;
        self.settings_field = None;
        self.settings_buf.clear();
        // Nothing above kept focus, but the caller re-notifies; the render
        // pass refocuses the terminal once no overlay remains open.
    }

    pub fn close_top_overlay(&mut self) -> bool {
        crate::metrics::event("action.close_top_overlay");
        if self.cli_launcher.dismiss_menu() {
            return true;
        }
        if self.tab_menu.is_some() {
            self.tab_menu = None;
            return true;
        }
        if self.pane_menu.is_some() {
            self.pane_menu = None;
            return true;
        }
        if self.editing_tab.is_some() {
            self.editing_tab = None;
            return true;
        }
        if self.modal.is_some() {
            self.modal = None;
            return true;
        }
        if self.layouts_open {
            self.layouts_open = false;
            return true;
        }
        if self.remote_menu_open {
            self.remote_menu_open = false;
            return true;
        }
        false
    }
    pub fn add_tab(&mut self, cx: &mut Context<Self>) {
        crate::metrics::event("action.add_tab");
        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        let leaf_id = self.next_leaf_id;
        self.next_leaf_id += 1;
        self.tabs.push(TabState::single(tab_id, "Terminal", leaf_id));
        self.active_tab = self.tabs.len() - 1;
        self.spawn_session(leaf_id);
        self.touch_recent(leaf_id);
        cx.notify();
    }

    pub fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.activate_tab index={index}"));
        if index < self.tabs.len() {
            self.active_tab = index;
            self.close_modal();
            self.touch_recent(self.tabs[index].active_leaf);
            cx.notify();
        }
    }

    pub fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.close_tab index={index}"));
        if index < self.tabs.len() {
            let removed = self.tabs.remove(index);
            for leaf in removed.leaf_ids() {
                self.teardown_leaf(leaf);
            }
            // Removing shifts later tabs left, so an active tab after the
            // removed slot would point at a neighbour without this.
            if index < self.active_tab {
                self.active_tab -= 1;
            }
            if !self.tabs.is_empty() && self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
            // A rename in progress for the tab just closed must not keep
            // editing (or committing to) a tab that no longer exists.
            if self.editing_tab == Some(removed.id) {
                self.editing_tab = None;
                self.editing_title.clear();
            }
            self.clamp_active_tab();
            self.tab_menu = None;
            self.pane_menu = None;
            self.refresh_remote_tabs();
            self.checkpoint_workspace();
            cx.notify();
        }
    }

    pub fn open_palette(&mut self, mode: PaletteMode, cx: &mut Context<Self>) {
        crate::metrics::event(&format!(
            "action.open_palette mode={}",
            match mode {
                PaletteMode::Command => "command",
                PaletteMode::History => "history",
            }
        ));
        self.close_modal();
        self.modal = Some(Modal::Palette(mode));
        self.palette_query.clear();
        self.palette_selected = 0;
        cx.notify();
    }

    pub fn run_palette_row(&mut self, index: usize, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.run_palette_row index={index}"));
        self.modal = None;
        if index == PALETTE_CONNECT_ROW {
            // Still the raw input, fragment and all.
            if let Some(raw) = self.palette_connect_target() {
                self.remote_connect(&raw, cx);
            }
            return;
        }
        match index {
            0 => self.add_tab(cx),
            1 => self.split_active_pane(SplitDirection::Row, cx),
            2 => self.split_active_pane(SplitDirection::Column, cx),
            3 => {
                self.layouts_open = true;
                cx.notify();
            }
            4 => {
                self.settings_section = crate::state::SettingsSection::Appearance;
                self.modal = Some(Modal::Settings);
                cx.notify();
            }
            5 => {
                self.modal = Some(Modal::Palette(PaletteMode::History));
                cx.notify();
            }
            6 => self.open_settings(cx),
            7 => {
                self.modal = Some(Modal::Remote);
                cx.notify();
            }
            _ => self.show_toast("Action applied", cx),
        }
    }

    /// The command-palette rows matching the current query, tagged with their
    /// original index so activation still dispatches to the right action.
    /// Rows matching the palette query, as `(command index, row)`.
    pub fn palette_rows_filtered(&self) -> Vec<(usize, (Icon, SharedString, &'static str))> {
        let query = self.palette_query.trim().to_lowercase();
        let mut rows: Vec<(usize, (Icon, SharedString, &'static str))> = self
            .palette_rows()
            .into_iter()
            .enumerate()
            .filter(|(_, (_, label, _))| query.is_empty() || label.to_lowercase().contains(&query))
            .collect();
        // A pasted pairing link is not an action name, so offer it directly
        // rather than filtering everything away.
        if let Some(raw) = self.palette_connect_target() {
            // Label without the fragment: the pairing secret must not end up in
            // a rendered row, but the connect call still needs the full input.
            let label = crate::backend::remote_client::display_target(&raw);
            if !label.is_empty() {
                rows.insert(
                    0,
                    (
                        PALETTE_CONNECT_ROW,
                        (Icon::Remote, format!("Connect to {label}").into(), "another SpritX"),
                    ),
                );
            }
        }
        rows
    }

    /// The palette query verbatim when it looks like an address to connect to, so
    /// the command palette doubles as the SpritX↔SpritX entry point.
    fn palette_connect_target(&self) -> Option<String> {
        let query = self.palette_query.trim();
        if query.is_empty() || query.contains(' ') {
            return None;
        }
        let addressable = query.contains("://")
            || query.contains("#p=")
            || (query.contains(':') && !query.starts_with(':'));
        addressable.then(|| query.to_string())
    }

    /// History entries matching the palette query (same order as the panel).
    pub fn palette_history(&self) -> Vec<HistoryEntry> {
        self.history
            .as_ref()
            .map(|store| store.search(&self.palette_query))
            .unwrap_or_default()
    }

    /// Number of selectable rows in the open palette, for ↑/↓ clamping.
    fn palette_result_count(&self) -> usize {
        match self.modal {
            Some(Modal::Palette(PaletteMode::Command)) => self.palette_rows_filtered().len(),
            Some(Modal::Palette(PaletteMode::History)) => self.palette_history().len(),
            _ => 1,
        }
    }

    /// Activate the current palette selection (Enter).
    pub fn activate_palette_selection(&mut self, cx: &mut Context<Self>) {
        crate::metrics::event("action.palette_enter");
        match self.modal {
            Some(Modal::Palette(PaletteMode::Command)) => {
                let rows = self.palette_rows_filtered();
                if rows.is_empty() {
                    self.show_toast("No matching action", cx);
                    return;
                }
                let position = self.palette_selected.min(rows.len() - 1);
                let index = rows[position].0;
                self.run_palette_row(index, cx);
            }
            Some(Modal::Palette(PaletteMode::History)) => {
                let entries = self.palette_history();
                let Some(entry) = entries.get(self.palette_selected) else {
                    return;
                };
                let command = entry.command.clone();
                let sent = self.send_to_terminal(&command, false);
                self.modal = None;
                if sent {
                    self.show_toast(format!("Inserted “{command}”"), cx);
                } else {
                    self.show_toast("No active terminal to insert into", cx);
                }
            }
            _ => {}
        }
    }

    /// Route a keystroke into the open palette. Returns true when consumed.
    fn handle_palette_key(
        &mut self,
        key: &str,
        ch: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        match key {
            "up" => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
                cx.notify();
                true
            }
            "down" => {
                let max = self.palette_result_count().saturating_sub(1);
                self.palette_selected = (self.palette_selected + 1).min(max);
                cx.notify();
                true
            }
            "backspace" => {
                self.palette_query.pop();
                self.palette_selected = 0;
                cx.notify();
                true
            }
            "enter" => {
                self.activate_palette_selection(cx);
                true
            }
            _ => match ch {
                Some(ch) if !ch.is_empty() && !ch.chars().any(char::is_control) => {
                    self.palette_query.push_str(ch);
                    self.palette_selected = 0;
                    cx.notify();
                    true
                }
                _ => false,
            },
        }
    }

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        crate::metrics::event("action.open_settings");
        self.close_modal();
        self.modal = Some(Modal::Settings);
        cx.notify();
    }

    pub fn switch_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.switch_tab delta={delta}"));
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        self.active_tab = ((self.active_tab as isize + delta).rem_euclid(len)) as usize;
        cx.notify();
    }

    pub fn apply_layout(&mut self, preset: &LayoutPreset, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.apply_layout preset={}", preset.id));
        self.layouts_open = false;
        if self.tabs.is_empty() { self.add_tab(cx); }
        let index = self.active_tab;
        let needed = match preset.id {
            "two-cols" | "two-rows" => 2,
            "grid" => 4,
            "main-side" | "triple" => 3,
            _ => 1,
        };

        let mut ids = self.tabs[index].leaf_ids();
        while ids.len() < needed {
            let leaf_id = self.next_leaf_id;
            self.next_leaf_id += 1;
            self.tabs[index]
                .leaves
                .insert(leaf_id, LeafState::sample(leaf_id));
            ids.push(leaf_id);
        }
        ids.truncate(needed);

        let leaf = |slot: usize| PaneNode::leaf(ids[slot]);
        let tree = match preset.id {
            "two-cols" => PaneNode::split(SplitDirection::Row, 0.5, leaf(0), leaf(1)),
            "two-rows" => PaneNode::split(SplitDirection::Column, 0.5, leaf(0), leaf(1)),
            "grid" => {
                let top = PaneNode::split(SplitDirection::Row, 0.5, leaf(0), leaf(1));
                let bottom = PaneNode::split(SplitDirection::Row, 0.5, leaf(2), leaf(3));
                PaneNode::split(SplitDirection::Column, 0.5, top, bottom)
            }
            "main-side" => {
                let side = PaneNode::split(SplitDirection::Column, 0.5, leaf(1), leaf(2));
                PaneNode::split(SplitDirection::Row, 0.66, leaf(0), side)
            }
            "triple" => {
                let right = PaneNode::split(SplitDirection::Row, 0.5, leaf(1), leaf(2));
                PaneNode::split(SplitDirection::Row, 1.0 / 3.0, leaf(0), right)
            }
            _ => leaf(0),
        };

        let tab = &mut self.tabs[index];
        tab.tree = tree;
        tab.leaves.retain(|id, _| ids.contains(id));
        tab.active_leaf = ids[0];
        let created = ids.clone();
        for leaf in created {
            self.spawn_session(leaf);
        }
        self.prune_sessions();
        self.show_toast(format!("Layout “{}” applied", preset.label), cx);
    }

    fn tab_index_for_leaf(&self, leaf: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.leaves.contains_key(&leaf))
    }

    pub fn focus_leaf(&mut self, leaf: u64, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.focus_leaf leaf={leaf}"));
        if let Some(index) = self.tab_index_for_leaf(leaf) {
            self.active_tab = index;
            self.tabs[index].active_leaf = leaf;
            self.touch_recent(leaf);
            cx.notify();
        }
    }

    /// Split one leaf (`row` = side by side, `column` = stacked).
    pub fn split_leaf_in_tab(
        &mut self,
        tab_index: usize,
        target: u64,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_index) else {
            return;
        };
        if tab.pane_count() >= MAX_PANES_PER_TAB {
            self.show_toast(format!("A tab holds at most {MAX_PANES_PER_TAB} panes"), cx);
            return;
        }
        let new_leaf = self.next_leaf_id;
        self.next_leaf_id += 1;
        if self.tabs[tab_index].split_leaf(target, new_leaf, direction) {
            self.active_tab = tab_index;
            self.spawn_session(new_leaf);
            cx.notify();
        }
    }

    pub fn split_active_pane(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.split_active direction={direction:?}"));
        let Some(target) = self.active_tab_checked().map(|tab| tab.active_leaf) else { return; };
        self.split_leaf_in_tab(self.active_tab, target, direction, cx);
    }

    pub fn split_tab(&mut self, index: usize, direction: SplitDirection, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.split_tab index={index} dir={direction:?}"));
        if index >= self.tabs.len() {
            return;
        }
        self.active_tab = index;
        self.tab_menu = None;
        let target = self.tabs[index].active_leaf;
        self.split_leaf_in_tab(index, target, direction, cx);
    }

    /// Close one pane; a tab's last pane degrades to closing the tab.
    pub fn close_pane(&mut self, leaf: u64, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.close_pane leaf={leaf}"));
        let Some(index) = self.tab_index_for_leaf(leaf) else {
            return;
        };
        self.pane_menu = None;
        if self.tabs[index].pane_count() <= 1 {
            self.close_tab(index, cx);
            return;
        }
        if self.tabs[index].close_leaf(leaf) {
            self.teardown_leaf(leaf);
            cx.notify();
        }
    }

    pub fn set_split_ratio(&mut self, path: &[usize], ratio: f32) -> bool {
        let index = self.active_tab;
        self.tabs.get_mut(index).is_some_and(|tab| tab.set_split_ratio(path, ratio))
    }

    pub fn cycle_pane(&mut self, forward: bool, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.cycle_pane forward={forward}"));
        let index = self.active_tab;
        let Some(current) = self.tabs.get(index).map(|tab| tab.active_leaf) else { return; };
        if let Some(next) = self.tabs[index].neighbour_leaf(current, forward) {
            self.tabs[index].active_leaf = next;
            cx.notify();
        }
    }

    pub fn open_tab_menu(&mut self, index: usize, x: f32, y: f32, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.open_tab_menu index={index} x={x:.0} y={y:.0}"));
        if let Some(tab) = self.tabs.get(index) {
            self.tab_menu = Some(TabMenuState {
                tab_id: tab.id,
                x,
                y,
            });
            self.pane_menu = None;
            self.editing_tab = None;
            cx.notify();
        }
    }

    /// Live position of a tab id. Closures capture the menu's `tab_id`, not a
    /// positional index, so a tab closed left of the target cannot retarget
    /// the action onto a neighbour.
    pub fn tab_index_for_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    pub fn begin_rename(&mut self, index: usize, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.begin_rename index={index}"));
        if let Some(tab) = self.tabs.get(index) {
            self.editing_tab = Some(tab.id);
            self.editing_title = tab.title.clone();
            self.tab_menu = None;
            self.pane_menu = None;
            cx.notify();
        }
    }

    pub fn commit_rename(&mut self, cx: &mut Context<Self>) {
        crate::metrics::event("action.commit_rename");
        if let Some(tab_id) = self.editing_tab.take() {
            let title = self.editing_title.trim().to_string();
            if !title.is_empty() {
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                    tab.title = title;
                }
            }
        }
        cx.notify();
    }

    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        crate::metrics::event("action.cancel_rename");
        self.editing_tab = None;
        cx.notify();
    }

    pub fn open_pane_menu(&mut self, leaf: u64, x: f32, y: f32, cx: &mut Context<Self>) {
        crate::metrics::event(&format!("action.open_pane_menu leaf={leaf} x={x:.0} y={y:.0}"));
        self.focus_leaf(leaf, cx);
        self.pane_menu = Some(PaneMenuState { leaf_id: leaf, x, y });
        self.tab_menu = None;
        cx.notify();
    }

    pub fn toggle_workspace_theme(&mut self, cx: &mut Context<Self>) {
        let mut config = AppConfig::load();
        config.appearance.light_workspace = !config.appearance.light_workspace;
        if let Err(error) = config.save() {
            self.show_toast(format!("Could not save workspace theme: {error}"), cx);
            return;
        }
        crate::terminal::set_light_workspace(config.appearance.light_workspace);
        self.theme = if config.appearance.light_workspace {
            Theme::light()
        } else {
            Theme::dark()
        };
        cx.notify();
    }

    pub(super) fn terminal_has_focus(&self) -> bool {
        self.modal.is_none()
            && self.editing_tab.is_none()
            && self.settings_menu.is_none()
            && self.tab_menu.is_none()
            && self.pane_menu.is_none()
            && !self.layouts_open
            && !self.remote_menu_open
    }

    fn render_body(&self, window_active: bool, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap(body_pad())
            .p(body_pad())
            .when(self.file_tree_visible, |d| {
                d.child(crate::metrics::file_tree(|| super::file_tree::render(self, cx)))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(if self.theme.bg.l >= 0.5 {
                        self.theme.outer
                    } else {
                        self.theme.border
                    })
                    .bg(self.theme.bg)
                    .overflow_hidden()
                    .child(crate::metrics::tab_bar(|| super::tab_bar::render(self, window_active, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .p(body_pad())
                            .child(super::pane_tree::render(self, cx)),
                    )
                    .child(self.render_status_hint()),
            )
            .into_any_element()
    }

    fn render_status_hint(&self) -> AnyElement {
        if self.tabs.is_empty() { return div().into_any_element(); }
        let theme = self.theme;
        let leaf = self.active_leaf();
        let (cwd, branch) = leaf
            .map(|l| (l.cwd.clone(), l.branch.clone()))
            .unwrap_or_default();
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(px(24.))
            .px(px(6.))
            .text_size(px(10.5))
            .text_color(theme.fg_dim)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon(Icon::Folder, 11.0, theme.fg_dim))
                    .child(cwd),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .when(self.settings.show_git_branch, |row| {
                        row.child(icon(Icon::Branch, 11.0, theme.accent))
                            .child(branch)
                    })
                    .child(dot(theme.success, 5.0))
                    .child("zsh"),
            )
            .into_any_element()
    }

    fn render_root_overlays(&self, viewport: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        let popup_open = self.layouts_open || self.remote_menu_open;
        if popup_open {
            overlays.push(super::overlays::render_popup_backdrop(cx));
        }
        if self.layouts_open {
            overlays.push(super::overlays::render_layouts_popup(self, cx));
        }
        if self.remote_menu_open {
            overlays.push(super::overlays::render_remote_menu(self, cx));
        }
        if let Some(modal) = self.modal {
            overlays.push(super::overlays::render_modal(self, modal, cx));
        }
        if let Some(menu) = &self.tab_menu {
            overlays.push(deferred(super::tab_bar::render_context_menu(self, menu, cx)).with_priority(super::overlays::CONTEXT_MENU_Z).into_any_element());
        }
        if let Some(menu) = &self.pane_menu {
            overlays.push(deferred(super::pane_tree::render_context_menu(self, menu, viewport, cx)).with_priority(super::overlays::CONTEXT_MENU_Z).into_any_element());
        }
        if let Some(toast) = &self.toast {
            overlays.push(super::overlays::render_toast(self, &toast.message));
        }
        overlays
    }
}

impl Render for SpritXApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::metrics::input_painted();
        crate::metrics::frame_tick(false);
        crate::metrics::frame_start();
        // Only claim the terminal when nothing else owns the keyboard:
        // forcing focus every frame steals it from modal/menu/editing
        // fields. Once they close, this refocuses the terminal again.
        if self.terminal_has_focus() && !self.focus_handle.contains_focused(window, cx) {
            window.focus(&self.focus_handle);
        }
        let theme = self.theme;
        // Everything below runs while the element tree is built; closing the
        // frame once the top-level children are constructed captures the bulk
        // of the per-frame work (the window turns it into paint afterwards).
        let root = div()
            .id("spritx-root")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.outer)
            .font_family(crate::terminal::appearance().font_family)
            .text_color(theme.fg)
            .text_size(px(12.5))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    // Root handler sees every click before the child that
                    // owns it, so this is the click's arrival time.
                    let _guard = crate::metrics::guard("mouse.handler");
                    crate::metrics::event(&format!(
                        "mouse.down x={:.0} y={:.0}",
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                    ));
                    crate::metrics::input_arrived("click");
                    crate::metrics::add(crate::metrics::Name::Clicks, 1);
                    // Only repaint when this click actually changed something:
                    // a floating menu closed, or focus moved to the root. A
                    // plain click on a normal button must not, because the
                    // button's own on_click will notify on mouse-up anyway.
                    let menu_closed =
                        this.tab_menu.take().is_some() | this.pane_menu.take().is_some();
                    let refocused = !this.focus_handle.is_focused(window);
                    if refocused {
                        window.focus(&this.focus_handle);
                    }
                    if menu_closed || refocused {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    if this.tab_menu.take().is_some() | this.pane_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                    // `on_click` fires on mouse-up, so this is when the button's
                    // own handler is about to run. The render it schedules is
                    // the first frame that shows the button's effect.
                    let _guard = crate::metrics::guard("mouseup.handler");
                    crate::metrics::event(&format!(
                        "mouse.up x={:.0} y={:.0}",
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                    ));
                    crate::metrics::input_arrived("clickup");
                    if this.split_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Latency probe: bytes still queued on the tty when this ran
                // (big value = the event loop was blocked before dispatch).
                if crate::metrics::enabled() {
                    let backlog = crate::metrics::tty_backlog().unwrap_or(0);
                    if backlog > 0 {
                        crate::metrics::event(&format!("key.rx backlog={backlog}B"));
                    }
                    crate::metrics::add(crate::metrics::Name::KeyRx, backlog as u64);
                    crate::metrics::input_arrived("key");
                }
                let _guard = crate::metrics::guard("key.handler");
                let key = event.keystroke.key.as_str();
                if std::env::var_os("SPRITX_DEBUG_KEYS").is_some() {
                    eprintln!("[spritx-key] {} {:?}", key, event.keystroke.modifiers);
                }
                let mods = event.keystroke.modifiers;
                // Shortcut recording captures the very next chord.
                if let Some(action) = this.recording_action.clone() {
                    if key == "escape" {
                        this.recording_action = None;
                    } else {
                        this.settings
                            .keybindings
                            .insert(action, format_keystroke(event));
                        this.recording_action = None;
                        this.save_settings();
                        this.show_toast("Shortcut updated", cx);
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                // Inline Settings field captures the keyboard while editing.
                if let Some(field) = this.settings_field {
                    match key {
                        "escape" => {
                            this.settings_field = None;
                            this.settings_buf.clear();
                        }
                        "enter" | "kp_enter" => this.commit_settings_field(cx),
                        "backspace" => {
                            this.settings_buf.pop();
                            cx.notify();
                        }
                        _ => {
                            if !mods.control && !mods.platform && !mods.alt {
                                if let Some(ch) = event.keystroke.key_char.as_ref() {
                                    if !ch.is_empty()
                                        && (!field.is_numeric()
                                            || ch.chars().all(|c| c.is_ascii_digit()))
                                    {
                                        this.settings_buf.push_str(ch);
                                        cx.notify();
                                    }
                                }
                            }
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.editing_tab.is_some() {
                    match key {
                        "escape" => this.cancel_rename(cx),
                        "enter" => this.commit_rename(cx),
                        "backspace" => {
                            this.editing_title.pop();
                            cx.notify();
                        }
                        _ => {
                            if !mods.control && !mods.platform && !mods.alt {
                                if let Some(ch) = event.keystroke.key_char.as_ref() {
                                    if ch.chars().count() == 1 {
                                        this.editing_title.push_str(ch);
                                        cx.notify();
                                    }
                                }
                            }
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                if key == "escape" && this.close_top_overlay() {
                    if this.terminal_has_focus() {
                        this.refocus_terminal(window);
                    }
                    cx.notify();
                    return;
                }
                // Interrupts are terminal controls, never clipboard actions.
                // Keep them ahead of global shortcuts while a terminal owns focus.
                if this.terminal_has_focus() && mods.control && !mods.alt && !mods.platform {
                    let leaf = this.active_tab_checked().map(|tab| tab.active_leaf);
                    if key.eq_ignore_ascii_case("c") && !mods.shift {
                        if let Some(leaf) = leaf { this.interrupt_pane(leaf, cx); }
                        cx.stop_propagation();
                        return;
                    }
                    if key.eq_ignore_ascii_case("k") && mods.shift {
                        if let Some(leaf) = leaf { this.force_stop_pane(leaf, cx); }
                        cx.stop_propagation();
                        return;
                    }
                }
                let cmd = mods.control || mods.platform;
                // Terminal clipboard / zoom / scrollback shortcuts, matching
                // common terminals: Ctrl+Shift+C/V, Ctrl +/-/0, Shift+PgUp/PgDn.
                if this.modal.is_none() && cmd && mods.shift && key == "c" {
                    this.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && mods.shift && key == "v" {
                    this.paste_clipboard(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && mods.shift && key == "a" {
                    this.select_all_active(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && key == "insert" {
                    this.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && mods.shift && key == "insert" {
                    this.paste_clipboard(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && (key == "=" || key == "plus" || key == "+") {
                    this.zoom_font(1.0, cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && (key == "-" || key == "minus") {
                    this.zoom_font(-1.0, cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && cmd && key == "0" {
                    this.settings.font_size = f32::from(crate::backend::config::DEFAULT_TERMINAL_FONT_SIZE);
                    this.save_settings();
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && mods.shift && key == "pageup" {
                    this.scroll_active(-10, cx);
                    cx.stop_propagation();
                    return;
                }
                if this.modal.is_none() && mods.shift && key == "pagedown" {
                    this.scroll_active(10, cx);
                    cx.stop_propagation();
                    return;
                }
                // Palette text entry + navigation: the search box is a plain
                // field, so route printable keys, backspace and arrows here.
                if matches!(this.modal, Some(Modal::Palette(_)))
                    && !cmd
                    && !mods.alt
                    && this.handle_palette_key(
                        key,
                        event.keystroke.key_char.as_deref(),
                        cx,
                    )
                {
                    cx.stop_propagation();
                    return;
                }
                // Global shortcuts are inert while any overlay owns the
                // keyboard: otherwise a Settings/CloseConfirm modal could
                // be bypassed (Ctrl+W/T, Alt+arrows, Ctrl+digits acting
                // underneath it). The palette path above already returned.
                let overlays_clear = this.modal.is_none()
                    && !this.layouts_open
                    && !this.remote_menu_open
                    && this.tab_menu.is_none()
                    && this.pane_menu.is_none();
                if overlays_clear {
                    if mods.alt && !cmd && matches!(key, "left" | "right" | "up" | "down") {
                        match key {
                            "left" | "up" => this.cycle_pane(false, cx),
                            "right" | "down" => this.cycle_pane(true, cx),
                            _ => {}
                        }
                    } else if cmd && mods.shift && key == "p" {
                        this.open_palette(PaletteMode::Command, cx);
                    } else if cmd && mods.shift && key == "h" {
                        this.open_palette(PaletteMode::History, cx);
                    } else if cmd && mods.shift && key == "d" {
                        this.split_active_pane(SplitDirection::Row, cx);
                    } else if cmd && mods.shift && key == "e" {
                        this.split_active_pane(SplitDirection::Column, cx);
                    } else if cmd && key == "t" {
                        this.add_tab(cx);
                    } else if cmd && key == "w" {
                        if let Some(leaf) = this.active_tab_checked().map(|tab| tab.active_leaf) { this.close_pane(leaf, cx); }
                    } else if cmd && (key == "," || key == "comma") {
                        this.open_settings(cx);
                    } else if cmd && key == "tab" {
                        this.switch_tab(if mods.shift { -1 } else { 1 }, cx);
                    } else if cmd && mods.shift && key == "]" {
                        this.switch_tab(1, cx);
                    } else if cmd && mods.shift && key == "[" {
                        this.switch_tab(-1, cx);
                    } else if cmd && !mods.shift {
                        if let Some(digit) = key.chars().next().and_then(|c| c.to_digit(10)) {
                            if digit >= 1 && digit <= 9 {
                                this.activate_tab(digit as usize - 1, cx);
                            }
                        } else if this.modal.is_none() {
                            // No local echo: the PTY reader wakes the view when the
                            // shell echoes, so we don't force a repaint per key.
                            this.type_key(event, cx);
                        }
                    } else if this.modal.is_none() && this.editing_tab.is_none() {
                        this.type_key(event, cx);
                    }
                }
            }));
        let window_active = window.is_window_active();
        let viewport = window.bounds().size;
        let mut root = root.child(self.render_body(window_active, cx));
        for overlay in self.render_root_overlays(viewport, cx) {
            root = root.child(overlay);
        }
        crate::metrics::frame_end();
        root
    }
}

impl SpritXApp {
    pub fn render_toolbar_action(
        &self,
        id: &'static str,
        icon_kind: Icon,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        icon_button(id, icon_kind, &self.theme, active)
            .on_click(cx.listener(move |this, _, _, cx| {
                crate::metrics::event(&format!("toolbar.click id={id}"));
                handler(this, cx)
            }))
            .into_any_element()
    }

    pub fn toolbar_action_active(&self) -> bool {
        self.layouts_open || self.remote_menu_open
    }
}

impl SpritXApp {
    pub fn theme_preset_names(&self) -> &[&'static str] {
        &self.theme_names
    }

    pub fn layout_presets(&self) -> &[LayoutPreset] {
        &LAYOUT_PRESETS
    }

    pub fn active_leaf(&self) -> Option<&LeafState> {
        self.active_tab_checked().and_then(TabState::active_leaf_state)
    }

    pub fn palette_rows(&self) -> Vec<(Icon, SharedString, &'static str)> {
        vec![
            (Icon::Plus, "New tab".into(), "Ctrl+T"),
            (Icon::SplitH, "Split right".into(), "Ctrl+Shift+D"),
            (Icon::SplitV, "Split down".into(), "Ctrl+Shift+E"),
            (Icon::Layouts, "Layout presets".into(), "›"),
            (Icon::Theme, "Change theme".into(), "›"),
            (Icon::History, "Search history".into(), "Ctrl+Shift+H"),
            (Icon::Settings, "Settings".into(), "Ctrl+,"),
            (Icon::Remote, "Remote control".into(), ""),
        ]
    }
}

/// Map the persisted appearance settings onto the terminal renderer. GPUI
/// wants one installed family; resolve the configured stack before measuring cells.
fn terminal_appearance_from_config(config: &AppConfig, available_fonts: &[String]) -> crate::terminal::TerminalAppearance {
    let appearance = &config.appearance;
    crate::terminal::TerminalAppearance {
        font_family: resolve_terminal_font_family(&appearance.font_family, available_fonts).into(),
        font_size: f32::from(appearance.font_size).clamp(8.0, 40.0),
        scrollback: appearance.scrollback.max(1) as usize,
        cursor_shape: match appearance.cursor_style.as_str() {
            "bar" => crate::terminal::CursorShape::Bar,
            "underline" => crate::terminal::CursorShape::Underline,
            _ => crate::terminal::CursorShape::Block,
        },
        cursor_blink: appearance.cursor_blink,
    }
}

/// Resolve the CSS-style stack ourselves: GPUI otherwise substitutes a
/// proportional system font when the first family is absent. Measuring that
/// font's zero width does not match its prompt text and pushes the cursor away.
pub fn resolve_terminal_font_family(stack: &str, available_fonts: &[String]) -> String {
    let configured = stack.split(',').map(|part| part.trim().trim_matches(['"', '\'']).trim());
    let fallbacks = [crate::theme::MONO_FONT, "Cascadia Mono", "Consolas", "DejaVu Sans Mono",
        "Liberation Mono", "Menlo", "Courier New"];
    for candidate in configured.chain(fallbacks) {
        if let Some(installed) = available_fonts.iter().find(|name| name.eq_ignore_ascii_case(candidate)) {
            return installed.clone();
        }
    }
    crate::theme::MONO_FONT.to_string()
}

/// `"JetBrainsMono Nerd Font", Menlo, monospace` → `JetBrainsMono Nerd Font`.
pub fn primary_font_family(stack: &str) -> String {
    let first = stack
        .split(',')
        .map(str::trim)
        .find(|part| !part.is_empty())
        .unwrap_or(crate::theme::MONO_FONT);
    let trimmed = first.trim_matches(|c| c == '"' || c == '\'').trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("monospace") {
        crate::theme::MONO_FONT.to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_hex(value: &str) -> u32 {
    u32::from_str_radix(value.trim().trim_start_matches('#'), 16).unwrap_or(0)
}

/// Parse a `#rrggbb` string into a GPUI color.
pub fn hsla_from_hex(value: &str) -> gpui::Hsla {
    gpui::rgb(parse_hex(value)).into()
}

/// Format a GPUI color as `#rrggbb` (used by the color pickers and config).
pub fn hsla_to_hex(color: gpui::Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn terminal_theme_from_config(config: &AppConfig) -> crate::terminal::TerminalTheme {
    let theme = &config.appearance.theme;
    crate::terminal::TerminalTheme {
        background: hsla_from_hex(&theme.background),
        foreground: hsla_from_hex(&theme.foreground),
        cursor: hsla_from_hex(&theme.cursor),
        selection: hsla_from_hex(&theme.selection),
        accent: hsla_from_hex(&theme.accent),
        ansi: [
            hsla_from_hex(&theme.black),
            hsla_from_hex(&theme.red),
            hsla_from_hex(&theme.green),
            hsla_from_hex(&theme.yellow),
            hsla_from_hex(&theme.blue),
            hsla_from_hex(&theme.magenta),
            hsla_from_hex(&theme.cyan),
            hsla_from_hex(&theme.white),
            hsla_from_hex(&theme.bright_black),
            hsla_from_hex(&theme.bright_red),
            hsla_from_hex(&theme.bright_green),
            hsla_from_hex(&theme.bright_yellow),
            hsla_from_hex(&theme.bright_blue),
            hsla_from_hex(&theme.bright_magenta),
            hsla_from_hex(&theme.bright_cyan),
            hsla_from_hex(&theme.bright_white),
        ],
    }
}

fn write_theme(config: &mut AppConfig, theme: &crate::terminal::TerminalTheme) {
    let target = &mut config.appearance.theme;
    target.background = hsla_to_hex(theme.background);
    target.foreground = hsla_to_hex(theme.foreground);
    target.cursor = hsla_to_hex(theme.cursor);
    target.selection = hsla_to_hex(theme.selection);
    target.accent = hsla_to_hex(theme.accent);
    target.black = hsla_to_hex(theme.ansi[0]);
    target.red = hsla_to_hex(theme.ansi[1]);
    target.green = hsla_to_hex(theme.ansi[2]);
    target.yellow = hsla_to_hex(theme.ansi[3]);
    target.blue = hsla_to_hex(theme.ansi[4]);
    target.magenta = hsla_to_hex(theme.ansi[5]);
    target.cyan = hsla_to_hex(theme.ansi[6]);
    target.white = hsla_to_hex(theme.ansi[7]);
    target.bright_black = hsla_to_hex(theme.ansi[8]);
    target.bright_red = hsla_to_hex(theme.ansi[9]);
    target.bright_green = hsla_to_hex(theme.ansi[10]);
    target.bright_yellow = hsla_to_hex(theme.ansi[11]);
    target.bright_blue = hsla_to_hex(theme.ansi[12]);
    target.bright_magenta = hsla_to_hex(theme.ansi[13]);
    target.bright_cyan = hsla_to_hex(theme.ansi[14]);
    target.bright_white = hsla_to_hex(theme.ansi[15]);
}

fn theme_hexes(theme: &crate::backend::config::ThemeConfig) -> [&str; 21] {
    [
        theme.background.as_str(),
        theme.foreground.as_str(),
        theme.cursor.as_str(),
        theme.selection.as_str(),
        theme.accent.as_str(),
        theme.black.as_str(),
        theme.red.as_str(),
        theme.green.as_str(),
        theme.yellow.as_str(),
        theme.blue.as_str(),
        theme.magenta.as_str(),
        theme.cyan.as_str(),
        theme.white.as_str(),
        theme.bright_black.as_str(),
        theme.bright_red.as_str(),
        theme.bright_green.as_str(),
        theme.bright_yellow.as_str(),
        theme.bright_blue.as_str(),
        theme.bright_magenta.as_str(),
        theme.bright_cyan.as_str(),
        theme.bright_white.as_str(),
    ]
}

fn theme_index_for(theme: &crate::backend::config::ThemeConfig) -> usize {
    let current = theme_hexes(theme);
    crate::theme_presets::PRESETS
        .iter()
        .position(|preset| preset.matches(&current))
        .unwrap_or(usize::MAX)
}

fn settings_from_config(config: &AppConfig) -> SettingsData {
    let mut settings = SettingsData::default();
    settings.close_to_tray = config.behavior.close_to_tray;
    settings.restore_workspace = config.behavior.restore_workspace;
    settings.keep_sessions = config.behavior.keep_sessions;
    settings.save_terminal_output = config.behavior.save_terminal_output;
    settings.reduced_motion = config.behavior.reduced_motion;
    settings.font_family = config.appearance.font_family.clone();
    settings.font_size = config.appearance.font_size as f32;
    settings.cursor_blink = config.appearance.cursor_blink;
    settings.cursor_style = match config.appearance.cursor_style.as_str() {
        "bar" => "Bar",
        "underline" => "Underline",
        _ => "Block",
    };
    settings.scrollback = config.appearance.scrollback;
    settings.shell_program = config
        .shell
        .program
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::platform::DEFAULT_SHELL_LABEL.to_string());
    settings.shell_args = serde_json::to_string(&config.shell.args).unwrap_or_default();
    settings.notify_enabled = config.notifications.enabled;
    settings.notify_min_duration = config.notifications.min_duration_sec;
    settings.notify_sound = config.notifications.sound;
    settings.remote_port = config.remote.port;
    settings.remote_auto_tunnel = config.remote.auto_tunnel;
    settings.history_enabled = config.history.enabled;
    settings.history_ignore = config.history.ignore.join(", ");
    settings.focus_follows_mouse = config.behavior.focus_follows_mouse;
    settings.show_git_branch = config.behavior.show_git_branch;
    settings.persist_blocks = config.behavior.persist_blocks;
    settings.file_tree_list = config.file_tree.dir_list.clone();
    settings.file_tree_open = config.file_tree.dir_open.clone();
    settings.file_tree_view = config.file_tree.file_view.clone();
    settings.file_tree_edit = config.file_tree.file_edit.clone();
    settings.file_tree_open_file = config.file_tree.file_open.clone();
    settings.keybindings = config.keybindings.clone();
    settings.theme = crate::theme_presets::PRESETS
        .get(theme_index_for(&config.appearance.theme))
        .map(|preset| preset.name)
        .unwrap_or("Custom");
    settings
}

fn apply_settings(config: &mut AppConfig, settings: &SettingsData) {
    config.behavior.close_to_tray = settings.close_to_tray;
    config.behavior.restore_workspace = settings.restore_workspace;
    config.behavior.keep_sessions = settings.keep_sessions;
    config.behavior.save_terminal_output = settings.save_terminal_output;
    config.behavior.reduced_motion = settings.reduced_motion;
    config.appearance.font_family = settings.font_family.clone();
    config.appearance.font_size = settings.font_size.round().clamp(8.0, 40.0) as u16;
    config.appearance.cursor_blink = settings.cursor_blink;
    config.appearance.cursor_style = settings.cursor_style.to_ascii_lowercase();
    config.appearance.scrollback = settings.scrollback;
    config.shell.program = if crate::platform::is_default_shell(&settings.shell_program) {
        None
    } else {
        Some(settings.shell_program.clone())
    };
    if let Ok(args) = crate::platform::parse_shell_args(&settings.shell_args) {
        config.shell.args = args;
    }
    config.notifications.enabled = settings.notify_enabled;
    config.notifications.min_duration_sec = settings.notify_min_duration;
    config.notifications.sound = settings.notify_sound;
    config.remote.port = settings.remote_port;
    config.remote.auto_tunnel = settings.remote_auto_tunnel;
    config.history.enabled = settings.history_enabled;
    config.history.ignore = settings
        .history_ignore
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    config.behavior.focus_follows_mouse = settings.focus_follows_mouse;
    config.behavior.show_git_branch = settings.show_git_branch;
    config.behavior.persist_blocks = settings.persist_blocks;
    config.file_tree.dir_list = settings.file_tree_list.clone();
    config.file_tree.dir_open = settings.file_tree_open.clone();
    config.file_tree.file_view = settings.file_tree_view.clone();
    config.file_tree.file_edit = settings.file_tree_edit.clone();
    config.file_tree.file_open = settings.file_tree_open_file.clone();
    config.keybindings = settings.keybindings.clone();
}

fn config_dir() -> std::path::PathBuf {
    crate::platform::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Human-readable form of a keystroke, e.g. `Ctrl + Shift + T`.
fn format_keystroke(event: &KeyDownEvent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if event.keystroke.modifiers.platform {
        parts.push("Cmd".to_string());
    }
    if event.keystroke.modifiers.control {
        parts.push("Ctrl".to_string());
    }
    if event.keystroke.modifiers.alt {
        parts.push("Alt".to_string());
    }
    if event.keystroke.modifiers.shift {
        parts.push("Shift".to_string());
    }
    let key = event.keystroke.key.as_str();
    let label = match key {
        "space" => "Space".to_string(),
        "escape" => "Esc".to_string(),
        "enter" => "Enter".to_string(),
        "tab" => "Tab".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    };
    parts.push(label);
    parts.join(" + ")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Remote control
// ---------------------------------------------------------------------------

impl SpritXApp {
    /// A snapshot of the server's state for the UI. Falls back to "not sharing"
    /// when no server is running.
    pub fn remote_status(&self) -> RemoteStatus {
        match &self.remote_server {
            Some(server) => server.status(),
            None => RemoteStatus::offline(),
        }
    }

    /// Attached remote clients right now. While one is attached, that client's
    /// terminal size *is* the pane's PTY size (its `Resize` inputs land in
    /// [`SpritXApp::pump_terminals`]), so the pane's own pixel box must not
    /// resize the session back: the client would then paint a crop and a phone
    /// would lose the bottom rows — where a full-screen TUI keeps its prompt.
    pub fn remote_clients(&self) -> usize {
        self.remote_server
            .as_ref()
            .map(RemoteServer::client_count)
            .unwrap_or(0)
    }

    /// Start the HTTP/WebSocket server on the configured port and open a
    /// single-use pairing code.
    pub fn remote_start(&mut self, cx: &mut Context<Self>) {
        if let Some(server) = self.remote_server.as_ref() {
            if server.is_running() {
                cx.notify();
                return;
            }
        }
        let port = self.settings.remote_port;
        // The "Internet tunnel automatically" setting only takes effect when
        // cloudflared is actually installed — otherwise the share would bind
        // loopback and silently lock the LAN out.
        let tunnel = self.settings.remote_auto_tunnel && cloudflared_available();
        let options = ServerOptions {
            port,
            bind: if tunnel {
                // With a tunnel the LAN socket has no reason to be exposed.
                Bind::Loopback
            } else {
                Bind::Lan
            },
            store: None,
            tunnel,
            ..ServerOptions::default()
        };
        match RemoteServer::start(options) {
            Ok(server) => {
                // A taken port falls back to an ephemeral one; say so rather
                // than leaving the configured value looking like a lie. Only
                // one toast, since showing a second would bury the first.
                let bound = server.port();
                let moved = bound != port;
                self.remote_server = Some(server);
                self.remote_tab_signature = 0;
                self.refresh_remote_tabs();
                let url = self.remote_status().pair_url.unwrap_or_default();
                let prefix = if moved {
                    format!("Remote control started on {bound} (port {port} was busy) · pair with ")
                } else {
                    "Remote control started · pair with ".to_string()
                };
                self.show_toast(format!("{prefix}{}", truncate_url(&url)), cx);
            }
            Err(error) => {
                self.show_toast(format!("Could not start remote control: {error}"), cx);
            }
        }
        cx.notify();
    }

    /// Stop sharing: closes the listener, every live bridge and the tunnel.
    pub fn remote_stop(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.remote_server.take() else {
            // Nothing was sharing; don't claim a stop that never happened.
            self.remote_menu_open = false;
            cx.notify();
            return;
        };
        server.stop();
        self.remote_menu_open = false;
        self.show_toast("Remote control stopped", cx);
        cx.notify();
    }

    /// Replace the pairing code with a fresh single-use one and return its URL.
    pub fn remote_new_pairing(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let url = self.remote_server.as_ref().and_then(RemoteServer::new_pairing);
        match url {
            Some(url) => {
                self.show_toast(format!("New pairing link · {}", truncate_url(&url)), cx);
                cx.notify();
                Some(url)
            }
            None => {
                self.show_toast("Start sharing before pairing a device", cx);
                cx.notify();
                None
            }
        }
    }

    /// Forget a device and cut its live connections.
    pub fn remote_revoke(&mut self, device_id: &str, cx: &mut Context<Self>) {
        let outcome = self
            .remote_server
            .as_ref()
            .map(|server| server.revoke(device_id));
        self.recording_action = None;
        match outcome {
            Some(Ok(true)) => self.show_toast("Device revoked", cx),
            Some(Ok(false)) => self.show_toast("That device is already forgotten", cx),
            Some(Err(error)) => self.show_toast(format!("Could not revoke the device: {error}"), cx),
            None => self.show_toast("Remote control is not running", cx),
        }
        cx.notify();
    }

    /// Start the cloudflared quick tunnel (the binary must already be installed).
    pub fn remote_tunnel(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.remote_server.as_ref() else {
            self.show_toast("Start sharing before opening a tunnel", cx);
            cx.notify();
            return;
        };
        if server.start_tunnel() {
            self.show_toast("Opening the Internet tunnel…", cx);
        } else {
            let reason = server
                .status()
                .last_error
                .unwrap_or_else(|| "cloudflared is not installed".to_string());
            self.show_toast(format!("Tunnel unavailable: {reason}"), cx);
        }
        cx.notify();
    }

    /// Connect using a pairing link the other SpritX already copied to the
    /// clipboard — the usual way this gets used, since typing a URL with a
    /// one-time secret on a phone-then-desktop round trip is miserable.
    pub fn remote_connect_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let candidate = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(|text| text.trim().lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        if candidate.is_empty() {
            self.show_toast("Nothing on the clipboard to connect with", cx);
            cx.notify();
            return;
        }
        self.remote_connect(&candidate, cx);
    }

    /// Open a tab driven by *another* SpritX. `link` is what its pairing panel
    /// shows: a `#p=` link, an `http(s)` base, or a bare `host:port`.
    ///
    /// Returns as soon as the address is accepted; the handshake runs on the
    /// link's own thread and reports through `refresh_remote_links`.
    pub fn remote_connect(&mut self, link: &str, cx: &mut Context<Self>) {
        let remote = match crate::backend::remote_client::connect(link) {
            Ok(remote) => Arc::new(remote),
            Err(error) => {
                self.show_toast(format!("Could not connect: {error}"), cx);
                cx.notify();
                return;
            }
        };
        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        let leaf_id = self.next_leaf_id;
        self.next_leaf_id += 1;
        self.tabs
            .push(TabState::single(tab_id, "Remote · connecting".to_string(), leaf_id));
        self.active_tab = self.tabs.len() - 1;
        self.sessions.insert(leaf_id, TerminalSession::attach_remote(remote.clone()));
        self.remote_links.insert(leaf_id, remote);
        self.show_toast("Connecting to the other SpritX…", cx);
        cx.notify();
    }

    /// Close a SpritX↔SpritX pane and tear its link down.
    pub fn remote_disconnect(&mut self, leaf: u64, cx: &mut Context<Self>) {
        let removed = self.remote_links.remove(&leaf);
        self.remote_link_notes.remove(&leaf);
        match removed {
            Some(link) => {
                link.close();
                self.show_toast("Disconnected from the other SpritX", cx);
            }
            None => self.show_toast("That pane is not a remote link", cx),
        }
        cx.notify();
    }

    /// Name panes once their peer identifies itself, and announce link
    /// transitions. Returns true when the UI has to repaint.
    fn refresh_remote_links(&mut self, cx: &mut Context<Self>) -> bool {
        let mut repaint = false;
        let leaves: Vec<u64> = self.remote_links.keys().copied().collect();
        for leaf in leaves {
            let Some(link) = self.remote_links.get(&leaf).cloned() else {
                continue;
            };
            let online = link.is_online();
            let note = link.error();
            if self.remote_link_notes.insert(leaf, note.clone()) != Some(note.clone()) {
                repaint = true;
                let peer = link.server_name().unwrap_or_else(|| "Remote".to_string());
                // "connecting…" is the initial state, not news worth a toast.
                match note {
                    Some(message) if !online && message != "connecting…" => {
                        self.show_toast(format!("{peer}: {message}"), cx);
                    }
                    None if online => {
                        self.show_toast(format!("Connected to {peer}"), cx);
                    }
                    _ => {}
                }
            }
            if online {
                let Some(peer) = link.server_name() else { continue };
                let title = format!("{peer} · remote");
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.leaves.contains_key(&leaf))
                {
                    if tab.title != title {
                        tab.title = title;
                        repaint = true;
                    }
                }
            }
        }
        repaint
    }

    /// Copy the pairing link into the clipboard (the secret stays in the URL
    /// fragment, which no server ever receives). GPUI 0.2.2 exposes the
    /// clipboard on `App` (`cx`), not on `Window`.
    pub fn remote_copy_pairing(&mut self, cx: &mut Context<Self>) {
        let status = self.remote_status();
        let link = status
            .public_pair_url
            .clone()
            .or(status.pair_url.clone())
            .unwrap_or_else(|| status.url.clone());
        if link.is_empty() {
            self.show_toast("Nothing to copy — remote control is off", cx);
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
        self.show_toast("Pairing link copied · it carries a one-time secret", cx);
    }

    /// Copy the plain share URL (no secret) for a device that is already paired.
    pub fn remote_copy_share(&mut self, cx: &mut Context<Self>) {
        let status = self.remote_status();
        let link = status
            .public_url
            .clone()
            .unwrap_or_else(|| status.url.clone());
        if link.is_empty() {
            self.show_toast("Nothing to copy — remote control is off", cx);
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
        self.show_toast("Share link copied", cx);
    }

    /// The server has no event channel back to the UI, so a freshly paired
    /// device is noticed by watching the device count.
    fn remote_announce_pairing(&mut self, cx: &mut Context<Self>) {
        if self.remote_server.is_none() {
            self.remote_device_count = None;
            return;
        }
        let devices = self.remote_status().devices;
        let count = devices.len();
        let previous = self.remote_device_count.replace(count);
        if previous.is_some_and(|before| count > before) {
            let name = devices
                .first()
                .map(|device| device.name.clone())
                .unwrap_or_else(|| "device".to_string());
            self.show_toast(format!("{name} is now paired"), cx);
        }
    }

    /// Publish the panes a remote client may drive, if the set changed.
    fn refresh_remote_tabs(&mut self) {
        let Some(server) = self.remote_server.clone() else {
            return;
        };
        let signature = self.remote_tab_signature_now();
        if signature == self.remote_tab_signature {
            return;
        }
        self.remote_tab_signature = signature;
        let tabs = self.remote_tab_infos();
        server.set_active_leaf(self.active_tab_checked().map(|tab| tab.active_leaf).unwrap_or(0));
        server.set_tabs(tabs);
    }

    fn remote_tab_signature_now(&self) -> u64 {
        let mut hash: u64 = 14695981039346656037;
        let mut mix = |value: &str| {
            for byte in value.bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(1099511628211);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(1099511628211);
        };
        mix(&self.active_tab_checked().map(|tab| tab.active_leaf).unwrap_or(0).to_string());
        for tab in &self.tabs {
            mix(&tab.title);
            for leaf in tab.leaf_ids() {
                mix(&leaf.to_string());
                if let Some(state) = tab.leaves.get(&leaf) {
                    mix(&state.cwd);
                    mix(&state.activity.running.to_string());
                    mix(&state.activity.completed.to_string());
                }
            }
        }
        hash
    }

    /// Every pane of every tab, addressed by its leaf id (what the bridge
    /// writes to), grouped by tab for the client's picker.
    fn remote_tab_infos(&self) -> Vec<TabInfo> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            for leaf in tab.leaf_ids() {
                let cwd = tab
                    .leaves
                    .get(&leaf)
                    .map(|state| state.cwd.clone())
                    .unwrap_or_default();
                let title = pane_title(&cwd, tab.display_title());
                out.push(TabInfo {
                    id: leaf,
                    title,
                    tab: tab.display_title().to_string(),
                    tab_id: tab.id,
                    activity: tab.leaves.get(&leaf).map_or_else(Default::default, |state| state.activity),
                });
            }
        }
        out
    }

    /// Move PTY output/geometry between the remote bridges and the sessions.
    /// Returns true when the UI has to repaint.
    fn pump_remote(&mut self) -> bool {
        let Some(server) = self.remote_server.clone() else {
            return false;
        };
        self.refresh_remote_tabs();
        let inputs = server.take_inputs();
        let mut repaint = false;
        for input in inputs {
            match input {
                RemoteInput::Write { leaf, bytes, .. } => {
                    if let Some(session) = self.sessions.get_mut(&leaf) {
                        session.clear_selection();
                        if session.scroll_to_bottom() {
                            repaint = true;
                        }
                        let _ = session.write(&bytes);
                    }
                }
                RemoteInput::Resize { leaf, cols, rows } => {
                    if let Some(session) = self.sessions.get_mut(&leaf) {
                        let _ = session.resize(cols, rows);
                        repaint = true;
                    }
                }
                RemoteInput::Completion { conn, leaf, token } => {
                    let cwd = self
                        .tabs
                        .iter()
                        .flat_map(|tab| tab.leaves.iter())
                        .find(|(id, _)| **id == leaf)
                        .map(|(_, state)| state.cwd.clone())
                        .unwrap_or_default();
                    server.reply_control(conn, listing_json(Some(cwd.as_str()), &token));
                }
                RemoteInput::NewTab { .. } => {
                    let tab_id = self.next_tab_id;
                    self.next_tab_id += 1;
                    let leaf_id = self.next_leaf_id;
                    self.next_leaf_id += 1;
                    self.tabs
                        .push(TabState::single(tab_id, "Terminal", leaf_id));
                    // A remote-requested tab must not steal the local user's
                    // focus: leave `active_tab` alone so they keep working.
                    self.spawn_session(leaf_id);
                    self.touch_recent(leaf_id);
                    self.checkpoint_workspace();
                    // The new pane becomes this client's target, and every
                    // client still without a pane is repointed by `set_tabs`.
                    server.set_active_leaf(leaf_id);
                    self.remote_tab_signature = 0;
                    self.refresh_remote_tabs();
                    repaint = true;
                }
            }
        }
        // Repaint whenever anything the UI mirrors from the server moved,
        // including the pairing countdown, which has no other trigger.
        let stamp = self.remote_stamp();
        let changed = self.remote_status_stamp != stamp;
        self.remote_status_stamp = stamp;
        repaint || changed
    }

    /// Snapshot of every server-derived value the UI displays.
    fn remote_stamp(&self) -> RemoteStamp {
        let status = self.remote_status();
        (
            status.running,
            status.clients,
            status.pair_url.is_some(),
            status.pairing_expires_in,
            self.remote_server.as_ref().map_or_else(Vec::new, RemoteServer::mobile_client_panes),
        )
    }

    pub(super) fn mobile_client_panes(&self) -> &[u64] {
        &self.remote_status_stamp.4
    }
}

/// A short, recognisable label for a pane: its directory name (`~` for home).
fn pane_title(cwd: &str, fallback: &str) -> String {
    if cwd.is_empty() {
        return fallback.to_string();
    }
    let home = crate::backend::files::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let shown = cwd
        .strip_prefix(home.as_str())
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| cwd.to_string());
    if shown == "~" {
        return "~".to_string();
    }
    shown
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .map(str::to_string)
        .unwrap_or(shown)
}

fn truncate_url(url: &str) -> String {
    if url.len() <= 48 {
        return url.to_string();
    }
    let prefix = url
        .char_indices()
        .nth(47)
        .map(|(index, _)| &url[..index])
        .unwrap_or(url);
    format!("{prefix}…")
}



#[cfg(test)]
mod font_fallback_tests {
    use super::resolve_terminal_font_family;
#[test]
fn missing_fonts_use_an_installed_monospace_family() {
    let available = vec!["Arial".into(), "Consolas".into(), "Courier New".into()];
    let stack = crate::backend::config::AppearanceConfig::default().font_family;
    assert_eq!(resolve_terminal_font_family(&stack, &available), "Consolas");
    assert_eq!(resolve_terminal_font_family("Missing Nerd Font, monospace", &available), "Consolas");
    assert_eq!(resolve_terminal_font_family("'courier new', Consolas", &available), "Courier New");
    let available = vec!["JetBrains Mono".into(), "DejaVu Sans Mono".into()];
    assert_eq!(resolve_terminal_font_family("Missing, 'DejaVu Sans Mono', monospace", &available), "DejaVu Sans Mono");
    assert_eq!(resolve_terminal_font_family("JetBrains Mono, monospace", &available), "JetBrains Mono");
}

}
/// Drive close-last/empty/recreate through the actual window and render loop.
#[cfg(debug_assertions)]
pub fn tab_lifecycle_test(window: gpui::WindowHandle<SpritXApp>, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        window.update(cx, |app, _, cx| {
            app.next_tab_id = 13;
            app.add_tab(cx);
            assert_eq!(app.active_tab().title, "Terminal");
            app.close_tab(1, cx);
            app.close_tab(0, cx);
            assert!(app.tabs.is_empty());
            assert!(app.sessions.is_empty());
            assert!(app.active_leaf().is_none());
            assert!(!app.write_active(b"ignored"));
            assert!(!app.active_scrolled());
            assert!(!app.set_split_ratio(&[], 0.5));
            app.cycle_pane(true, cx);
            app.split_active_pane(SplitDirection::Row, cx);
            assert!(app.tabs.is_empty());
            let saved = crate::backend::workspace::load();
            assert!(saved.empty_workspace);
            assert!(crate::backend::workspace::valid(&saved));
            app.restore_workspace();
            assert!(app.tabs.is_empty());
            app.layouts_open = true;
            cx.notify();
        }).unwrap();
        gpui::Timer::after(std::time::Duration::from_millis(300)).await;
        window.update(cx, |app, _, cx| { app.open_settings(cx); }).unwrap();
        gpui::Timer::after(std::time::Duration::from_millis(300)).await;
        window.update(cx, |app, _, cx| {
            app.close_modal();
            app.add_tab(cx); // Same handler as the + button.
            assert_eq!(app.tabs.len(), 1);
            assert_eq!(app.active_tab().display_title(), "Terminal");
            assert_eq!(app.sessions.len(), 1);
        }).unwrap();
        gpui::Timer::after(std::time::Duration::from_millis(300)).await;
        window.update(cx, |app, _, cx| {
            app.close_tab(0, cx); // Same handler as the tab's × button.
            app.close_tab(0, cx); // A stale/double close is harmless.
            assert!(app.tabs.is_empty());
            assert!(app.sessions.is_empty());
        }).unwrap();
        gpui::Timer::after(std::time::Duration::from_millis(300)).await;
        cx.update(|cx| cx.quit()).unwrap();
    }).detach();
}
#[cfg(debug_assertions)]
pub fn process_control_test(window: gpui::WindowHandle<SpritXApp>, cx: &mut gpui::App) {
    use gpui::AppContext;
    cx.spawn(async move |cx| {
        let root = std::path::PathBuf::from(std::env::var("SPRITX_PROCESS_ROOT").unwrap());
        macro_rules! wait_until {
            ($condition:expr, $message:expr) => {{
                let mut ready = false;
                for _ in 0..100 {
                    gpui::Timer::after(std::time::Duration::from_millis(100)).await;
                    if $condition { ready = true; break; }
                }
                assert!(ready, "{}", $message);
            }};
        }
        wait_until!(window.update(cx, |app, _, _| {
            app.pump_terminals();
            app.session(app.active_tab().active_leaf).is_some_and(|session| session.powerline_prompt_end().is_some())
        }).unwrap(), "initial shell did not become ready");
        let (leaf, original_pid) = window.update(cx, |app, _, _| {
            let leaf = app.active_tab().active_leaf;
            assert!(app.session(leaf).unwrap().owner_connection().is_some());
            assert!(app.send_to_terminal("node server.cjs", true));
            (leaf, app.session(leaf).unwrap().pid())
        }).unwrap();
        wait_until!(std::fs::read_to_string(root.join("port.txt")).ok().and_then(|s| s.parse::<u16>().ok()).is_some(), "Node server did not start");
        let port: u16 = std::fs::read_to_string(root.join("port.txt")).unwrap().parse().unwrap();
        // Dispatch through the window's real key listeners, not directly to the PTY.
        cx.update_window(window.into(), |_, window, cx| {
            assert!(window.dispatch_keystroke(gpui::Keystroke {
                modifiers: gpui::Modifiers::control(), key: "c".into(), key_char: None,
            }, cx));
        }).unwrap();
        wait_until!(root.join("signal.txt").exists() && std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "keyboard Ctrl+C did not deliver SIGINT/release the port");
        wait_until!(window.update(cx, |app, _, _| {
            app.pump_terminals();
            app.session(leaf).is_some_and(|session| session.powerline_prompt_end().is_some() && session.cursor().1 == 2)
        }).unwrap(), "shell did not return after Ctrl+C");
        let (other_leaf, other_pid) = window.update(cx, |app, _, cx| {
            assert_eq!(app.session(leaf).unwrap().pid(), original_pid);
            app.add_tab(cx);
            let other = app.active_tab().active_leaf;
            let other_pid = app.session(other).unwrap().pid();
            app.focus_leaf(leaf, cx);
            assert!(app.send_to_terminal("node force-server.cjs", true));
            (other, other_pid)
        }).unwrap();
        wait_until!(std::fs::read_to_string(root.join("force-port.txt")).ok().and_then(|s| s.parse::<u16>().ok()).is_some(), "unresponsive test server did not start");
        let force_port: u16 = std::fs::read_to_string(root.join("force-port.txt")).unwrap().parse().unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_keystroke(gpui::Keystroke {
                modifiers: gpui::Modifiers::control(), key: "c".into(), key_char: None,
            }, cx);
        }).unwrap();
        wait_until!(root.join("ignored.txt").exists(), "test server did not observe Ctrl+C");
        assert!(std::net::TcpStream::connect(("127.0.0.1", force_port)).is_ok());
        cx.update_window(window.into(), |_, window, cx| {
            assert!(window.dispatch_keystroke(gpui::Keystroke {
                modifiers: gpui::Modifiers { control: true, shift: true, ..Default::default() },
                key: "k".into(), key_char: None,
            }, cx));
        }).unwrap();
        wait_until!(std::net::TcpStream::connect(("127.0.0.1", force_port)).is_err(), "force stop left the server running");
        window.update(cx, |app, _, cx| {
            assert_ne!(app.session(leaf).unwrap().pid(), original_pid);
            assert_eq!(app.session(other_leaf).unwrap().pid(), other_pid, "force stop affected another pane");
            assert!(app.session(other_leaf).unwrap().exit_code().is_none());
            assert_eq!(app.tabs.len(), 2);
            app.quit_desktop(cx);
        }).unwrap();
    }).detach();
}
