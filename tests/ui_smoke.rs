use std::process::Command;

fn run_smoke(modal: Option<&str>) {
    if std::env::var("SPRITX_UI_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping UI smoke test: set SPRITX_UI_TESTS=1");
        return;
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spritx-terminal"));
    cmd.env("SPRITX_SMOKE", "1");
    if let Some(modal) = modal {
        cmd.env("SPRITX_OPEN", modal);
    }
    let status = cmd.status().expect("launch spritx-terminal");
    assert!(status.success(), "GPUI app exited with {status}");
}

#[test]
fn app_boots_and_quits_cleanly() {
    run_smoke(None);
}

#[test]
fn settings_modal_renders_without_panicking() {
    run_smoke(Some("settings"));
}

#[test]
fn palette_modal_renders_without_panicking() {
    run_smoke(Some("palette"));
}

/// Covers the pairing panel with a real QR encoder and the "SpritX you drive"
/// list wired in: both render even with no link and no share running.
#[test]
fn remote_modal_renders_without_panicking() {
    run_smoke(Some("remote"));
}

/// The slide-over menu the toolbar icon opens. Its off state once showed a
/// "Stop remote control" row that had nothing to stop.
#[test]
fn remote_menu_renders_without_panicking() {
    run_smoke(Some("remote-menu"));
}
