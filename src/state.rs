use std::collections::{BTreeMap, HashMap};

pub type TabId = u64;
pub type LeafId = u64;

/// Mirrors `MAX_PANES_PER_TAB` in the web app (`Tabs.tsx:164`).
pub const MAX_PANES_PER_TAB: usize = 4;

/// Split ratios are clamped to 0.1 … 0.9 (`panes.ts:202`).
pub const MIN_SPLIT_RATIO: f32 = 0.1;
pub const MAX_SPLIT_RATIO: f32 = 0.9;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDirection {
    Row,
    Column,
}

#[derive(Clone)]
pub enum PaneNode {
    Leaf(LeafId),
    Split {
        direction: SplitDirection,
        ratio: f32,
        children: [Box<PaneNode>; 2],
    },
}

impl PaneNode {
    pub fn leaf(id: LeafId) -> Self {
        Self::Leaf(id)
    }

    pub fn split(direction: SplitDirection, ratio: f32, a: PaneNode, b: PaneNode) -> Self {
        Self::Split {
            direction,
            ratio,
            children: [Box::new(a), Box::new(b)],
        }
    }

    pub fn leaves(&self, out: &mut Vec<LeafId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { children, .. } => {
                children[0].leaves(out);
                children[1].leaves(out);
            }
        }
    }

    pub fn first_leaf(&self) -> LeafId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { children, .. } => children[0].first_leaf(),
        }
    }

    pub fn contains(&self, leaf: LeafId) -> bool {
        match self {
            Self::Leaf(id) => *id == leaf,
            Self::Split { children, .. } => {
                children[0].contains(leaf) || children[1].contains(leaf)
            }
        }
    }

    /// Replace `target` with a split holding `target` plus `new_leaf`.
    /// Returns false when the target leaf is not in this tree.
    pub fn split_leaf(
        &mut self,
        target: LeafId,
        new_leaf: LeafId,
        direction: SplitDirection,
        ratio: f32,
    ) -> bool {
        let owned = std::mem::replace(self, Self::Leaf(target));
        let (tree, inserted) = owned.split_leaf_owned(target, new_leaf, direction, ratio);
        *self = tree;
        inserted
    }

    fn split_leaf_owned(
        self,
        target: LeafId,
        new_leaf: LeafId,
        direction: SplitDirection,
        ratio: f32,
    ) -> (Self, bool) {
        match self {
            Self::Leaf(id) if id == target => (
                Self::split(
                    direction,
                    ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO),
                    Self::Leaf(id),
                    Self::Leaf(new_leaf),
                ),
                true,
            ),
            Self::Leaf(id) => (Self::Leaf(id), false),
            Self::Split {
                direction: split_direction,
                ratio: split_ratio,
                children,
            } => {
                let [a, b] = children;
                let (a, inserted) = a.split_leaf_owned(target, new_leaf, direction, ratio);
                if inserted {
                    return (
                        Self::Split {
                            direction: split_direction,
                            ratio: split_ratio,
                            children: [Box::new(a), b],
                        },
                        true,
                    );
                }
                let (b, inserted) = b.split_leaf_owned(target, new_leaf, direction, ratio);
                (
                    Self::Split {
                        direction: split_direction,
                        ratio: split_ratio,
                        children: [Box::new(a), Box::new(b)],
                    },
                    inserted,
                )
            }
        }
    }

    /// Remove a leaf, promoting its sibling into the parent's slot.
    /// Returns false when the leaf is not in this tree (or it is the root).
    pub fn remove_leaf(&mut self, target: LeafId) -> bool {
        if matches!(self, Self::Leaf(_)) {
            return false;
        }
        let owned = std::mem::replace(self, Self::Leaf(target));
        let (tree, removed) = owned.remove_leaf_owned(target);
        *self = tree;
        removed
    }

    fn remove_leaf_owned(self, target: LeafId) -> (Self, bool) {
        match self {
            Self::Leaf(id) => (Self::Leaf(id), false),
            Self::Split {
                direction,
                ratio,
                children,
            } => {
                let [a, b] = children;
                let a = *a;
                let b = *b;
                if matches!(a, Self::Leaf(id) if id == target) {
                    return (b, true);
                }
                if matches!(b, Self::Leaf(id) if id == target) {
                    return (a, true);
                }
                let (new_a, removed) = a.remove_leaf_owned(target);
                if removed {
                    return (
                        Self::Split {
                            direction,
                            ratio,
                            children: [Box::new(new_a), Box::new(b)],
                        },
                        true,
                    );
                }
                let (new_b, removed) = b.remove_leaf_owned(target);
                (
                    Self::Split {
                        direction,
                        ratio,
                        children: [Box::new(new_a), Box::new(new_b)],
                    },
                    removed,
                )
            }
        }
    }

    /// Set the ratio of the split addressed by `path` (0 = first child, 1 = second).
    /// An empty path addresses `self`.
    pub fn set_ratio(&mut self, path: &[usize], ratio: f32) -> bool {
        match self {
            Self::Split {
                ratio: split_ratio,
                children,
                ..
            } => {
                if path.is_empty() {
                    *split_ratio = ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    true
                } else {
                    children[path[0]].set_ratio(&path[1..], ratio)
                }
            }
            Self::Leaf(_) => false,
        }
    }

    /// Next/previous leaf in tree order, wrapping around.
    pub fn neighbour_leaf(&self, current: LeafId, forward: bool) -> Option<LeafId> {
        let mut ids = Vec::new();
        self.leaves(&mut ids);
        if ids.is_empty() {
            return None;
        }
        let index = ids.iter().position(|id| *id == current)?;
        let next = if forward {
            (index + 1) % ids.len()
        } else {
            (index + ids.len() - 1) % ids.len()
        };
        ids.get(next).copied()
    }
}

#[derive(Clone)]
pub struct LeafState {
    pub id: LeafId,
    pub pty_id: Option<u64>,
    pub cwd: String,
    pub branch: String,
    pub remote: bool,
    pub scroll_offset: f32,
}

impl LeafState {
    pub fn sample(id: LeafId) -> Self {
        Self {
            id,
            pty_id: Some(id),
            cwd: crate::backend::files::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            branch: String::new(),
            remote: false,
            scroll_offset: 0.0,
        }
    }
}

#[derive(Clone)]
pub struct TabState {
    pub id: TabId,
    pub title: String,
    pub tree: PaneNode,
    pub leaves: BTreeMap<LeafId, LeafState>,
    pub active_leaf: LeafId,
}

impl TabState {
    pub fn single(id: TabId, title: impl Into<String>, leaf_id: LeafId) -> Self {
        let leaf = LeafState::sample(leaf_id);
        let mut leaves = BTreeMap::new();
        leaves.insert(leaf_id, leaf);
        Self {
            id,
            title: title.into(),
            tree: PaneNode::Leaf(leaf_id),
            leaves,
            active_leaf: leaf_id,
        }
    }

    pub fn leaf_ids(&self) -> Vec<LeafId> {
        let mut out = Vec::new();
        self.tree.leaves(&mut out);
        out
    }

    pub fn active_leaf_state(&self) -> Option<&LeafState> {
        self.leaves.get(&self.active_leaf)
    }

    pub fn pane_count(&self) -> usize {
        self.leaf_ids().len()
    }

    /// Split `target` and register a fresh sample leaf. Capped at four panes.
    pub fn split_leaf(
        &mut self,
        target: LeafId,
        new_leaf: LeafId,
        direction: SplitDirection,
    ) -> bool {
        if self.pane_count() >= MAX_PANES_PER_TAB {
            return false;
        }
        if !self.leaves.contains_key(&target) {
            return false;
        }
        if self.tree.split_leaf(target, new_leaf, direction, 0.5) {
            self.leaves.insert(new_leaf, LeafState::sample(new_leaf));
            self.active_leaf = new_leaf;
            true
        } else {
            false
        }
    }

    /// Close one pane, promoting the sibling subtree. Refuses to remove the last pane.
    pub fn close_leaf(&mut self, leaf: LeafId) -> bool {
        if self.pane_count() <= 1 || !self.leaves.contains_key(&leaf) {
            return false;
        }
        if self.tree.remove_leaf(leaf) {
            self.leaves.remove(&leaf);
            if self.active_leaf == leaf {
                self.active_leaf = self.tree.first_leaf();
            }
            true
        } else {
            false
        }
    }

    pub fn set_split_ratio(&mut self, path: &[usize], ratio: f32) -> bool {
        self.tree.set_ratio(path, ratio)
    }

    pub fn neighbour_leaf(&self, current: LeafId, forward: bool) -> Option<LeafId> {
        self.tree.neighbour_leaf(current, forward)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Shell,
    Notifications,
    FileTree,
    Remote,
    History,
    Keys,
    General,
    About,
}

impl SettingsSection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Shell => "Shell",
            Self::Notifications => "Notifications",
            Self::FileTree => "File tree",
            Self::Remote => "Remote",
            Self::History => "History",
            Self::Keys => "Shortcuts",
            Self::General => "General",
            Self::About => "About",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Command,
    History,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Palette(PaletteMode),
    Settings,
    Remote,
    CloseConfirm,
}

/// Which Settings popover/inline list is currently expanded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsMenu {
    Font,
    CursorStyle,
    Color(ThemeColorKey),
}

/// A Settings field edited inline from the keyboard (numeric or text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsField {
    FontSize,
    Scrollback,
    NotifyMinDuration,
    RemotePort,
    ShellProgram,
    ShellArgs,
    HistoryIgnore,
    FileTreeList,
    FileTreeOpen,
    FileTreeView,
    FileTreeEdit,
    FileTreeOpenFile,
}

impl SettingsField {
    /// Numeric fields accept digits only; everything else accepts text.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::FontSize | Self::Scrollback | Self::NotifyMinDuration | Self::RemotePort
        )
    }

    /// The current value as a string, for the inline editor's initial buffer.
    pub fn current(self, settings: &SettingsData) -> String {
        match self {
            Self::FontSize => format!("{}", settings.font_size as u32),
            Self::Scrollback => settings.scrollback.to_string(),
            Self::NotifyMinDuration => settings.notify_min_duration.to_string(),
            Self::RemotePort => settings.remote_port.to_string(),
            Self::ShellProgram => settings.shell_program.clone(),
            Self::ShellArgs => settings.shell_args.clone(),
            Self::HistoryIgnore => settings.history_ignore.clone(),
            Self::FileTreeList => settings.file_tree_list.clone(),
            Self::FileTreeOpen => settings.file_tree_open.clone(),
            Self::FileTreeView => settings.file_tree_view.clone(),
            Self::FileTreeEdit => settings.file_tree_edit.clone(),
            Self::FileTreeOpenFile => settings.file_tree_open_file.clone(),
        }
    }

    /// Commit an edited buffer back into the settings, clamping numerics.
    pub fn commit(self, settings: &mut SettingsData, buffer: &str) {
        let trimmed = buffer.trim();
        match self {
            Self::FontSize => {
                if let Ok(value) = trimmed.parse::<f32>() {
                    settings.font_size = value.clamp(8.0, 40.0);
                }
            }
            Self::Scrollback => {
                if let Ok(value) = trimmed.parse::<u32>() {
                    settings.scrollback = value.clamp(100, 100_000);
                }
            }
            Self::NotifyMinDuration => {
                if let Ok(value) = trimmed.parse::<u32>() {
                    settings.notify_min_duration = value.min(86_400);
                }
            }
            Self::RemotePort => {
                if let Ok(value) = trimmed.parse::<u16>() {
                    if value >= 1 {
                        settings.remote_port = value;
                    }
                }
            }
            Self::ShellProgram => {
                settings.shell_program = if trimmed.is_empty() {
                    crate::platform::DEFAULT_SHELL_LABEL.to_string()
                } else {
                    trimmed.to_string()
                };
            }
            Self::ShellArgs => settings.shell_args = trimmed.to_string(),
            Self::HistoryIgnore => settings.history_ignore = buffer.to_string(),
            Self::FileTreeList => settings.file_tree_list = trimmed.to_string(),
            Self::FileTreeOpen => settings.file_tree_open = trimmed.to_string(),
            Self::FileTreeView => settings.file_tree_view = trimmed.to_string(),
            Self::FileTreeEdit => settings.file_tree_edit = trimmed.to_string(),
            Self::FileTreeOpenFile => settings.file_tree_open_file = trimmed.to_string(),
        }
    }
}

/// The five primary terminal colors in Settings → Appearance → Colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeColorKey {
    Background,
    Foreground,
    Accent,
    Cursor,
    Selection,
}

#[derive(Clone)]
pub struct SettingsData {
    pub font_family: String,
    pub font_size: f32,
    pub cursor_blink: bool,
    pub cursor_style: &'static str,
    pub scrollback: u32,
    pub theme: &'static str,
    pub shell_program: String,
    pub shell_args: String,
    pub notify_enabled: bool,
    pub notify_min_duration: u32,
    pub notify_sound: bool,
    pub file_tree_width: f32,
    pub remote_port: u16,
    pub remote_auto_tunnel: bool,
    pub history_enabled: bool,
    pub history_ignore: String,
    pub focus_follows_mouse: bool,
    pub show_git_branch: bool,
    pub persist_blocks: bool,
    pub file_tree_list: String,
    pub file_tree_open: String,
    pub file_tree_view: String,
    pub file_tree_edit: String,
    pub file_tree_open_file: String,
    /// User-recorded overrides keyed by action name.
    pub keybindings: HashMap<String, String>,
}

impl Default for SettingsData {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: 11.0,
            cursor_blink: true,
            cursor_style: "Block",
            scrollback: 5_000,
            theme: "SpritX Dark",
            shell_program: crate::platform::DEFAULT_SHELL_LABEL.into(),
            shell_args: "[]".into(),
            notify_enabled: true,
            notify_min_duration: 10,
            notify_sound: false,
            file_tree_width: 240.0,
            remote_port: 4530,
            remote_auto_tunnel: true,
            history_enabled: true,
            history_ignore: "*password*, *token*, *secret*, *api_key*".into(),
            focus_follows_mouse: false,
            show_git_branch: true,
            persist_blocks: true,
            file_tree_list: "ls -la {path}".into(),
            file_tree_open: "${EDITOR:-nano} {path}".into(),
            file_tree_view: "cat {path}".into(),
            file_tree_edit: "nano {path}".into(),
            file_tree_open_file: "${EDITOR:-nano} {path}".into(),
            keybindings: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct Toast {
    pub message: String,
    pub generation: u64,
}

#[derive(Clone, Copy)]
pub struct LayoutPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const LAYOUT_PRESETS: [LayoutPreset; 6] = [
    LayoutPreset { id: "single", label: "Single", hint: "single" },
    LayoutPreset { id: "two-cols", label: "Two columns", hint: "side-by-side" },
    LayoutPreset { id: "two-rows", label: "Two rows", hint: "stacked" },
    LayoutPreset { id: "grid", label: "2 × 2 grid", hint: "grid-2x2" },
    LayoutPreset { id: "main-side", label: "Main + side", hint: "main-side" },
    LayoutPreset { id: "triple", label: "Three columns", hint: "triple-col" },
];

pub fn sample_theme_names() -> Vec<&'static str> {
    crate::theme_presets::PRESETS
        .iter()
        .map(|preset| preset.name)
        .collect()
}

pub type KeybindingMap = HashMap<&'static str, &'static str>;
