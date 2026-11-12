use std::process::Command;

fn run_smoke(modal: Option<&str>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spritx-terminal"));
    let config_root = std::env::temp_dir().join(format!(
        "spritx-ui-{}-{}-{}",
        std::process::id(),
        modal.unwrap_or("main"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // UI verification must not write settings or pairing data into a real profile.
    cmd.env(if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" }, &config_root);
    cmd.env("SPRITX_SMOKE", "1");
    if modal == Some("palette-light") {
        std::fs::create_dir_all(config_root.join("spritx")).unwrap();
        std::fs::write(config_root.join("spritx/config.toml"),
            "[appearance]\nlightWorkspace = true\n").unwrap();
    }
    if modal == Some("file-tree") {
        let fixture = config_root.join("file-tree-project-测试");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("long-file-name-λ.txt"), "before").unwrap();
        cmd.env("HOME", &fixture).env("USERPROFILE", &fixture);
    }
    if let Some(modal) = modal {
        cmd.env("SPRITX_OPEN", if modal == "palette-light" { "palette" } else { modal });
    }
    let status = cmd.status().expect("launch spritx-terminal");
    let _ = std::fs::remove_dir_all(config_root);
    assert!(status.success(), "GPUI app exited with {status}");
}
#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn app_boots_and_quits_cleanly() {
    run_smoke(None);
}

#[test]
#[ignore = "requires a display; run with --ignored"]
fn expanded_file_tree_loads_and_watches_without_crashing() {
    run_smoke(Some("file-tree"));
}

#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn settings_modal_exits_cleanly() {
    run_smoke(Some("settings"));
}

#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn workspace_recovery_settings_exits_cleanly() {
    run_smoke(Some("settings-general"));
}

#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn completion_notification_settings_exits_cleanly() {
    run_smoke(Some("settings-notifications"));
}

#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn palette_modal_exits_cleanly() {
    run_smoke(Some("palette"));
}

#[test]
#[ignore = "requires a display; run with --ignored"]
fn palette_light_modal_exits_cleanly() {
    run_smoke(Some("palette-light"));
}

/// Covers the pairing panel with a real QR encoder and the "SpritX you drive"
/// list wired in: both render even with no link and no share running.
#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn remote_modal_exits_cleanly() {
    run_smoke(Some("remote"));
}

/// The slide-over menu the toolbar icon opens. Its off state once showed a
/// "Stop remote control" row that had nothing to stop.
#[test]
#[ignore = "requires SPRITX_UI_TESTS=1 and a display; run with --ignored"]
fn remote_menu_exits_cleanly() {
    run_smoke(Some("remote-menu"));
}
