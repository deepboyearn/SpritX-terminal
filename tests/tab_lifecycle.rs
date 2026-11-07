#[test]
#[ignore = "requires a display; run with --ignored"]
fn last_tab_closes_empty_workspace_renders_and_plus_recreates_terminal() {
    let root = std::env::temp_dir().join(format!("spritx-empty-tabs-{}", std::process::id()));
    let mut process = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
        .env(if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" }, &root)
        .env("SPRITX_SMOKE", "1").env("SPRITX_TAB_TEST", "1").env("SPRITX_QUIET", "1")
        .spawn().unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() { break status; }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = process.kill();
            panic!("Empty workspace lifecycle did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    assert!(status.success(), "tab lifecycle exited with {status}");
    let path = root.join("spritx/sessions/workspace.json");
    let saved: spritx_terminal::backend::workspace::Workspace = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.empty_workspace);
    assert!(saved.tabs.is_empty());
    // A normal restart must retain zero tabs instead of creating a fresh shell.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
        .env(if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" }, &root)
        .env("SPRITX_SMOKE", "1").env("SPRITX_QUIET", "1").env_remove("SPRITX_TAB_TEST")
        .status().unwrap();
    assert!(status.success());
    let saved: spritx_terminal::backend::workspace::Workspace = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.empty_workspace);
    assert!(saved.tabs.is_empty());
    let _ = std::fs::remove_dir_all(root);
}
