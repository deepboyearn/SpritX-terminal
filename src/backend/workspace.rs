use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::state::TabState;

pub const VERSION: u32 = 1;

// Bound for one saved session's replayed output — mirrors the live PTY tail
// cap (session_owner::TAIL_LIMIT, 512 KiB). Kept as a local const because
// session_owner's copy is private; the values must stay in sync.
const MAX_SAVED_SCREEN_BYTES: usize = 512 * 1024;
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentProject {
    pub project: String,
    pub cwd: String,
    pub pane: Option<u64>,
    pub last_used: i64,
}

impl RecentProject {
    pub fn name(&self) -> String {
        Path::new(&self.project)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.project.clone())
    }
}

pub fn project_root(cwd: &str) -> String {
    let resolved = Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd));
    let path = resolved.as_path();
    path.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

pub fn is_profile_directory(cwd: &str) -> bool {
    let Some(home) = crate::backend::files::home_dir() else {
        return false;
    };
    let home = home.canonicalize().unwrap_or(home);
    let cwd = Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd));
    cwd == home
}

pub fn record_activity(
    recent: &mut Vec<RecentProject>,
    pane: u64,
    cwd: &str,
    now: i64,
    work_started: bool,
) {
    if cwd.is_empty() {
        return;
    }
    let project = project_root(cwd);
    if is_profile_directory(cwd) || is_profile_directory(&project) {
        for entry in recent.iter_mut().filter(|entry| entry.pane == Some(pane)) {
            entry.pane = None;
        }
        return;
    }
    for entry in recent
        .iter_mut()
        .filter(|entry| entry.pane == Some(pane) && entry.project != project)
    {
        entry.pane = None;
    }
    if let Some(entry) = recent
        .iter_mut()
        .find(|entry| entry.pane == Some(pane) && entry.project == project)
    {
        entry.cwd = cwd.into();
        entry.last_used = now;
    } else if work_started {
        recent.push(RecentProject {
            project,
            cwd: cwd.into(),
            pane: Some(pane),
            last_used: now,
        });
    }
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
    recent.truncate(100);
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    #[serde(default)]
    pub empty_workspace: bool,
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub recent: Vec<RecentProject>,
    #[serde(default)]
    pub sessions: std::collections::HashMap<u64, SavedSession>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub config: crate::terminal::SessionConfig,
    pub screen: String,
    #[serde(default)]
    pub owner: Option<crate::backend::session_owner::Connection>,
    #[serde(default)]
    pub owner_completion: u64,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            version: VERSION,
            empty_workspace: false,
            tabs: Vec::new(),
            active_tab: 0,
            recent: Vec::new(),
            sessions: Default::default(),
        }
    }
}

// Pure path computation: must not create directories or touch the filesystem.
// Securing the directory is a write-side concern — only save() does that, so a
// read-only load() never has side effects and a missing config dir surfaces as
// a diagnostic rather than a silently swallowed None.
pub fn path() -> Option<PathBuf> {
    crate::platform::config_dir().map(|dir| dir.join("sessions").join("workspace.json"))
}

pub fn load() -> Workspace {
    path()
        .and_then(|path| std::fs::read(path).ok())
        .filter(|bytes| bytes.len() <= 16 * 1024 * 1024)
        .and_then(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok())
        .filter(|workspace| workspace.version == VERSION)
        .map(|mut workspace| {
            workspace.recent.retain(|entry| {
                !is_profile_directory(&entry.cwd) && !is_profile_directory(&entry.project)
            });
            workspace
        })
        .unwrap_or_default()
}

pub fn valid(workspace: &Workspace) -> bool {
    use std::collections::HashSet;
    if workspace.tabs.is_empty() {
        return workspace.version == VERSION && workspace.empty_workspace && workspace.sessions.is_empty() && workspace.active_tab == 0;
    }
    if workspace.version != VERSION || workspace.tabs.len() > 64 {
        return false;
    }
    fn valid_tree(tree: &crate::state::PaneNode, depth: usize) -> bool {
        use crate::state::{PaneNode, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO};
        if depth > 12 {
            return false;
        }
        match tree {
            PaneNode::Leaf(id) => *id > 0 && *id < u64::MAX - 1,
            PaneNode::Split {
                ratio, children, ..
            } => {
                ratio.is_finite()
                    && (MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(ratio)
                    && children.iter().all(|child| valid_tree(child, depth + 1))
            }
        }
    }
    let mut tabs = HashSet::new();
    let mut panes = HashSet::new();
    let tabs_ok = workspace.tabs.iter().all(|tab| {
        // The depth-checked walk must run before leaf_ids(): leaf_ids()/leaves()
        // recurse the PaneNode tree with no depth bound, so validating depth first
        // keeps a hand-edited workspace.json from overflowing the stack. valid_tree
        // itself is bounded (depth > 12 returns false) and cannot recurse deeper.
        if !valid_tree(&tab.tree, 0) {
            return false;
        }
        if !(tabs.insert(tab.id) && tab.id < u64::MAX - 1) {
            return false;
        }
        let ids = tab.leaf_ids();
        ids.len() <= crate::state::MAX_PANES_PER_TAB
            && ids.contains(&tab.active_leaf)
            && ids.len() == tab.leaves.len()
            && ids
                .into_iter()
                .all(|id| panes.insert(id) && tab.leaves.get(&id).is_some_and(|leaf| leaf.id == id))
    });
    if !tabs_ok {
        return false;
    }
    // Reject snapshots pointing at a tab index past the end of the list: the
    // restore path indexes tabs[active_tab] and would panic on a hand-edited file.
    if workspace.active_tab >= workspace.tabs.len() {
        return false;
    }
    // Every saved-session key must name a leaf that actually exists in some
    // tab, else a crafted workspace wastes memory (or worse) spawning sessions
    // that are never displayed. `panes` holds the full set of validated leaf ids.
    if !workspace.sessions.keys().all(|id| panes.contains(id)) {
        return false;
    }
    // Cap each saved screen at the live tail size, so a truncated-but-parseable
    // snapshot cannot smuggle an unbounded blob into initial_output (which is
    // replayed into the terminal on restore).
    if !workspace
        .sessions
        .values()
        .all(|session| session.screen.len() <= MAX_SAVED_SCREEN_BYTES)
    {
        return false;
    }
    true
}

pub fn save(workspace: &Workspace) -> Result<(), String> {
    let path = path().ok_or("Config directory unavailable")?;
    // Directory creation/secure_directory is a write-side concern (path() is pure).
    // Previously a secure_directory failure hid inside path()'s `None`, so save
    // reported a generic "Config directory unavailable" instead of the real cause.
    if let Some(parent) = path.parent() {
        crate::backend::session_owner::secure_directory(parent)
            .map_err(|error| format!("could not secure session directory {}: {error}", parent.display()))?;
    }
    let body = serde_json::to_string(workspace).map_err(|error| error.to_string())?;
    crate::backend::config::write_atomic(&path, &body).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_entries_keep_panes_distinct_and_follow_directory_changes() {
        let root = std::env::temp_dir().join(format!("spritx-recent-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut recent = Vec::new();
        record_activity(&mut recent, 1, &root.to_string_lossy(), 1, true);
        record_activity(
            &mut recent,
            1,
            &root.join("src").to_string_lossy(),
            2,
            false,
        );
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].project,
            root.canonicalize().unwrap().to_string_lossy()
        );
        record_activity(&mut recent, 2, &root.to_string_lossy(), 3, true);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].pane, Some(2));
        record_activity(
            &mut recent,
            1,
            &std::env::temp_dir().to_string_lossy(),
            4,
            true,
        );
        assert!(recent.iter().any(|entry| entry.project
            == root.canonicalize().unwrap().to_string_lossy()
            && entry.pane.is_none()));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_split_snapshots_are_rejected() {
        let mut snapshot = Workspace {
            tabs: vec![crate::state::TabState::single(1, "project", 1)],
            ..Default::default()
        };
        assert!(valid(&snapshot));
        snapshot.tabs[0].active_leaf = 500;
        assert!(!valid(&snapshot));
        snapshot.tabs[0].active_leaf = 1;
        snapshot.tabs.push(snapshot.tabs[0].clone());
        assert!(!valid(&snapshot));
        snapshot.tabs.pop();
        snapshot.tabs[0].tree = crate::state::PaneNode::split(
            crate::state::SplitDirection::Row,
            f32::NAN,
            crate::state::PaneNode::leaf(1),
            crate::state::PaneNode::leaf(2),
        );
        assert!(!valid(&snapshot));
    }

    #[test]
    fn recent_excludes_profile_and_requires_new_work_after_removal() {
        let mut recent = Vec::new();
        if let Some(home) = crate::backend::files::home_dir() {
            record_activity(&mut recent, 1, &home.to_string_lossy(), 1, true);
            assert!(recent.is_empty());
        }
        let project =
            std::env::temp_dir().join(format!("spritx-recent-filter-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        let cwd = project.to_string_lossy();
        record_activity(&mut recent, 2, &cwd, 2, false);
        assert!(
            recent.is_empty(),
            "Idle navigation must not create a Recent"
        );
        record_activity(&mut recent, 2, &cwd, 3, true);
        assert_eq!(recent.len(), 1);
        recent.clear();
        record_activity(&mut recent, 2, &cwd, 4, false);
        assert!(
            recent.is_empty(),
            "A deleted Recent must stay removed during idle updates"
        );
        record_activity(&mut recent, 2, &cwd, 5, true);
        assert_eq!(recent.len(), 1);
        std::fs::remove_dir_all(project).unwrap();
    }
}
