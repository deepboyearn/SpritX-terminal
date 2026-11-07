use std::collections::{BTreeMap, HashMap};

pub type TabId = u64;
pub type LeafId = u64;

pub const MAX_PANES_PER_TAB: usize = 5;

/// Split ratios are clamped to 0.1 … 0.9 (`panes.ts:202`).
pub const MIN_SPLIT_RATIO: f32 = 0.1;
pub const MAX_SPLIT_RATIO: f32 = 0.9;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SplitDirection {
    Row,
    Column,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    /// An empty path addresses `self`. An out-of-range path segment returns false
    /// instead of panicking on the fixed-size children array.
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
                } else if path[0] >= children.len() {
                    false
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LeafState {
    pub activity: crate::backend::remote::CommandActivity,
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
            activity: Default::default(),
            pty_id: Some(id),
            cwd: crate::backend::files::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            branch: String::new(),
            remote: false,
            scroll_offset: 0.0,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TabState {
    pub id: TabId,
    pub title: String,
    pub tree: PaneNode,
    pub leaves: BTreeMap<LeafId, LeafState>,
    pub active_leaf: LeafId,
}

impl TabState {
    /// Hide the historical generated suffix; the tab strip already numbers tabs.
    pub fn display_title(&self) -> &str {
        if self.title.strip_prefix("Terminal ").and_then(|suffix| suffix.parse::<u64>().ok()) == Some(self.id) {
            "Terminal"
        } else { &self.title }
    }

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

    /// Split `target` and register a fresh sample leaf.
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
        // Focus should move to an adjacent survivor, not jump to the first
        // leaf in tree order: compute the neighbour on the pre-removal tree.
        let focus_fallback = if self.active_leaf == leaf {
            self.tree.neighbour_leaf(leaf, true)
        } else {
            None
        };
        if self.tree.remove_leaf(leaf) {
            self.leaves.remove(&leaf);
            if self.active_leaf == leaf {
                self.active_leaf = focus_fallback
                    .filter(|id| self.tree.contains(*id))
                    .unwrap_or_else(|| self.tree.first_leaf());
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

    /// Repair a `TabState` loaded from disk so it satisfies the invariants
    /// `workspace::valid` checks: prune `leaves` entries not present in
    /// `tree`, drop duplicate/out-of-range (> MAX_PANES_PER_TAB) leaf ids
    /// from the tree, re-seat `active_leaf` to `tree.first_leaf()` when it
    /// is missing, and fresh-seat any tree leaf with no `leaves` entry.
    /// WHY repair instead of privatising `tree`/`leaves`/`active_leaf`:
    /// those fields are read/written directly from 10+ call sites across
    /// app_shell/pane_tree/workspace/tab_bar/cli_launcher, so privatising
    /// would churn every caller for no behavioural gain; a `repair()`
    /// invoked on the restore path fixes hand-edited/corrupt files while
    /// keeping the public API. Callers MUST invoke this after load, before
    /// `valid()` — see the exact call site in the report.
    ///
    /// Produces a tab satisfying the `valid()` invariants when the tree
    /// itself passes `valid_tree` (bounded depth, ids in range, finite
    /// ratios); a too-deep tree is rebuilt as a single leaf.
    pub fn repair(&mut self) {
        // Bound the walk first (mirroring `workspace::valid`): an unbounded
        // `leaf_ids()` recursion on a hand-edited, deeply-nested tree could
        // overflow the stack. A too-deep tree is replaced wholesale with a
        // single leaf, reusing the active id when it has a surviving entry.
        let Some(mut ids) = bounded_leaf_ids(&self.tree, 0) else {
            let seeded = if self.leaves.contains_key(&self.active_leaf) {
                self.active_leaf
            } else {
                self.leaves.keys().next().copied().unwrap_or(1)
            };
            self.tree = PaneNode::Leaf(seeded);
            self.leaves = BTreeMap::from([(seeded, LeafState::sample(seeded))]);
            self.active_leaf = seeded;
            return;
        };
        // Ratios ride along with the tree: a NaN / out-of-range ratio fails
        // `valid()` too, so clamp them while the depth is known-bounded.
        clamp_split_ratios(&mut self.tree);
        // Drop duplicate ids (keep the first occurrence in tree order) and
        // ids past the pane cap. `remove_leaf` removes one node per call,
        // so repeated calls collapse each duplicate to a single survivor.
        for id in ids.clone() {
            while ids.iter().filter(|x| **x == id).count() > 1 {
                if !self.tree.remove_leaf(id) {
                    break;
                }
                ids = self.leaf_ids();
            }
        }
        while ids.len() > MAX_PANES_PER_TAB {
            let last = *ids.last().expect("over-cap tree has leaves");
            if !self.tree.remove_leaf(last) {
                break;
            }
            ids = self.leaf_ids();
        }
        // Prune orphan leaves (not in the repaired tree) plus entries whose
        // embedded `id` disagrees with the map key (`valid()` checks both).
        self.leaves
            .retain(|id, leaf| ids.contains(id) && leaf.id == *id);
        // Fresh-seat tree leaves with no entry so every tree id resolves.
        for id in &ids {
            self.leaves.entry(*id).or_insert_with(|| LeafState::sample(*id));
        }
        if !ids.contains(&self.active_leaf) {
            self.active_leaf = self.tree.first_leaf();
        }
    }
}

/// Bounded mirror of `PaneNode::leaves`; `None` when the tree exceeds the
/// depth `workspace::valid` accepts (12), so repair never recurses into a
/// stack overflow on a hand-edited file.
fn bounded_leaf_ids(tree: &PaneNode, depth: usize) -> Option<Vec<LeafId>> {
    if depth > 12 {
        return None;
    }
    match tree {
        PaneNode::Leaf(id) => Some(vec![*id]),
        PaneNode::Split { children, .. } => {
            let mut ids = bounded_leaf_ids(children[0].as_ref(), depth + 1)?;
            ids.extend(bounded_leaf_ids(children[1].as_ref(), depth + 1)?);
            Some(ids)
        }
    }
}

/// Clamp every split ratio into [MIN_SPLIT_RATIO, MAX_SPLIT_RATIO],
/// replacing non-finite ratios with a neutral 0.5. Only called after the
/// depth is verified bounded, so the recursion is safe.
fn clamp_split_ratios(tree: &mut PaneNode) {
    match tree {
        PaneNode::Split {
            ratio, children, ..
        } => {
            *ratio = if ratio.is_finite() {
                ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO)
            } else {
                0.5
            };
            clamp_split_ratios(children[0].as_mut());
            clamp_split_ratios(children[1].as_mut());
        }
        PaneNode::Leaf(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror of `workspace::valid`'s per-tab invariants, inlined so the
    /// repair contract is pinned without pulling in the backend module.
    fn tab_invariants_ok(tab: &TabState) -> bool {
        let ids = bounded_leaf_ids(&tab.tree, 0).unwrap_or_default();
        ids.len() <= MAX_PANES_PER_TAB
            && ids.contains(&tab.active_leaf)
            && ids.len() == tab.leaves.len()
            && ids
                .iter()
                .all(|id| tab.leaves.get(id).is_some_and(|leaf| leaf.id == *id))
    }

    #[test]
    fn repair_prunes_orphans_reseats_active_and_fresh_seats_missing() {
        let mut leaves = BTreeMap::new();
        leaves.insert(1, LeafState::sample(1));
        leaves.insert(999, LeafState::sample(999)); // orphan
        let mut tab = TabState {
            id: 1,
            title: "t".into(),
            tree: PaneNode::split(
                SplitDirection::Row,
                f32::NAN,
                PaneNode::leaf(1),
                PaneNode::leaf(2), // tree leaf with no leaves entry
            ),
            leaves,
            active_leaf: 999, // points at the orphan
        };
        tab.repair();
        assert!(tab_invariants_ok(&tab));
        assert_eq!(tab.active_leaf, tab.tree.first_leaf());
        assert!(!tab.leaves.contains_key(&999));
        assert!(tab.leaves.contains_key(&2));
    }

    #[test]
    fn repair_deduplicates_and_caps_pane_count() {
        let mut tree = PaneNode::leaf(1);
        for id in 2..8 {
            tree = PaneNode::split(SplitDirection::Row, 0.5, tree, PaneNode::leaf(id));
        }
        // Duplicate the first leaf.
        tree = PaneNode::split(SplitDirection::Row, 0.5, tree, PaneNode::leaf(1));
        let mut leaves = BTreeMap::new();
        for id in [1u64, 2, 3, 4, 5, 6, 7] {
            leaves.insert(id, LeafState::sample(id));
        }
        let mut tab = TabState {
            id: 1,
            title: "t".into(),
            tree,
            leaves,
            active_leaf: 7,
        };
        tab.repair();
        let ids = tab.leaf_ids();
        assert!(ids.len() <= MAX_PANES_PER_TAB);
        assert_eq!(ids.iter().filter(|x| **x == 1).count(), 1);
        assert!(tab_invariants_ok(&tab));
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
            Self::FontSize => format!("{}", settings.font_size.round() as u32),
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
    pub close_to_tray: bool,
    pub restore_workspace: bool,
    pub keep_sessions: bool,
    pub save_terminal_output: bool,
    pub reduced_motion: bool,
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
            close_to_tray: true,
            restore_workspace: true,
            keep_sessions: true,
            save_terminal_output: true,
            reduced_motion: false,
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: f32::from(crate::backend::config::DEFAULT_TERMINAL_FONT_SIZE),
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
