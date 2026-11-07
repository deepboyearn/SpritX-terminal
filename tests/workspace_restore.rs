//! An opt-in real-window round trip, isolated from the user's SpritX profile.
use spritx_terminal::backend::{
    config::AppConfig,
    workspace::{self, SavedSession, Workspace},
};
use spritx_terminal::state::{LeafState, PaneNode, SplitDirection, TabState};
use spritx_terminal::terminal::SessionConfig;

#[test]
fn closing_and_reopening_restores_tabs_splits_folders_and_saved_output() {
    if std::env::var("SPRITX_UI_TESTS").as_deref() != Ok("1") {
        eprintln!("Set SPRITX_UI_TESTS=1 to exercise the desktop app");
        return;
    }
    let root = std::env::temp_dir().join(format!("spritx-workspace-ui-{}", std::process::id()));
    let config_root = root.join("spritx");
    let state_path = config_root.join("sessions/workspace.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    let project = root.join("project with spaces");
    std::fs::create_dir_all(&project).unwrap();
    let mut tab = TabState::single(42, "Restored project", 31);
    assert!(tab.split_leaf(31, 32, SplitDirection::Column));
    tab.active_leaf = 32;
    for leaf in tab.leaves.values_mut() {
        leaf.cwd = project.to_string_lossy().into_owned();
    }
    let mut saved = Workspace {
        tabs: vec![tab],
        ..Default::default()
    };
    for pane in [31, 32] {
        saved.sessions.insert(
            pane,
            SavedSession {
                config: SessionConfig::for_shell(
                    None,
                    &[],
                    Some(project.to_string_lossy().into_owned()),
                ),
                screen: format!("SAVED_CONTEXT_{pane}\r\n"),
                owner: None,
                owner_completion: 0,
            },
        );
    }
    std::fs::write(&state_path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let config = AppConfig::default();
    spritx_terminal::backend::config::save_to(&config_root.join("config.toml"), &config).unwrap();
    for _ in 0..2 {
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
            .env(
                if cfg!(windows) {
                    "APPDATA"
                } else {
                    "XDG_CONFIG_HOME"
                },
                &root,
            )
            .env("SPRITX_SMOKE", "1")
            .env("SPRITX_QUIET", "1")
            .status()
            .unwrap();
        assert!(status.success());
        let actual: Workspace =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(actual.tabs.len(), 1);
        assert_eq!(actual.tabs[0].title, "Restored project");
        assert_eq!(actual.tabs[0].active_leaf, 32);
        assert_eq!(actual.tabs[0].leaf_ids(), [31, 32]);
        assert_eq!(
            std::path::Path::new(&actual.tabs[0].leaves[&31].cwd)
                .canonicalize()
                .unwrap(),
            project.canonicalize().unwrap()
        );
        assert!(
            actual.sessions[&31].screen.contains("SAVED_CONTEXT_31"),
            "{}",
            actual.sessions[&31].screen
        );
    }
    let _ = std::fs::remove_dir_all(root);
}
// pane_tree-2: a >12-deep split tree must be rejected by `valid()` without
// unbounded recursion. Pre-fix, `valid()` walked `leaf_ids()` (unbounded
// recursion, no depth check) before/independently of the depth guard, so a
// deep tree either crashed or was accepted instead of being rejected.
#[test]
fn deep_tree_rejected_and_shallow_workspace_valid() {
    // Positive control: an ordinary single-pane workspace is valid.
    let shallow = Workspace {
        tabs: vec![TabState::single(1, "ok", 10)],
        ..Default::default()
    };
    assert!(workspace::valid(&shallow), "shallow workspace must be valid");

    // Build a 60-deep Split chain in a loop: each level wraps the previous
    // tree on the right, with a fresh leaf on the left. Ratios are in range
    // and leaf ids are positive, so depth is the ONLY violation. Pre-fix,
    // `valid()` recursed via the unbounded `leaf_ids()` walk with no depth
    // guard, so this either overflowed the stack or returned true.
    // 60 levels of Box nesting drop fine (recursive drop is ~60 frames).
    let mut deep = Workspace {
        tabs: vec![TabState::single(1, "deep", 10)],
        ..Default::default()
    };
    let mut next_leaf = 100u64;
    let mut tree = PaneNode::Leaf(10);
    for _ in 0..60 {
        next_leaf += 1;
        tree = PaneNode::split(SplitDirection::Row, 0.5, PaneNode::Leaf(next_leaf), tree);
    }
    deep.tabs[0].tree = tree;
    for id in deep.tabs[0].leaf_ids() {
        deep.tabs[0]
            .leaves
            .entry(id)
            .or_insert_with(|| LeafState::sample(id));
    }
    deep.tabs[0].active_leaf = 10;
    assert!(
        !workspace::valid(&deep),
        "60-deep tree must be rejected (depth bound is 12)"
    );
}

#[test]
fn wrong_version_workspace_rejected() {
    let mut ws = Workspace {
        tabs: vec![TabState::single(1, "ok", 10)],
        ..Default::default()
    };
    assert!(workspace::valid(&ws));
    ws.version = workspace::VERSION + 99;
    assert!(
        !workspace::valid(&ws),
        "version != VERSION must be rejected"
    );
}

/// `load()` of a missing file yields a default workspace rather than
/// panicking. The real `load()` resolves its path from the platform config
/// dir, so bind APPDATA/XDG_CONFIG_HOME to an empty temp dir when the env
/// key is writable; if it cannot be bound, skip rather than touching the
/// user's real config.
#[test]
fn load_of_missing_file_yields_default_workspace() {
    let key = if cfg!(windows) {
        "APPDATA"
    } else {
        "XDG_CONFIG_HOME"
    };
    let previous = std::env::var_os(key);
    let root = std::env::temp_dir().join(format!(
        "spritx_ws_missing_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let old_user_profile = std::env::var_os("USERPROFILE");
    std::env::set_var(key, &root);
    if cfg!(windows) {
        std::env::set_var("USERPROFILE", &root);
    }
    let ws = workspace::load();
    match previous {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    if cfg!(windows) {
        match old_user_profile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(ws.version, workspace::VERSION);
    assert!(ws.tabs.is_empty());
}
#[test]
fn explicitly_closed_workspace_can_restore_with_zero_tabs() {
    use spritx_terminal::backend::workspace::{self, Workspace};
    let mut workspace = Workspace::default();
    assert!(!workspace::valid(&workspace), "missing snapshot keeps first-launch defaults");
    workspace.empty_workspace = true;
    assert!(workspace::valid(&workspace));
    workspace.active_tab = 1;
    assert!(!workspace::valid(&workspace));
    workspace.active_tab = 0;
    workspace.version += 1;
    assert!(!workspace::valid(&workspace));
}
