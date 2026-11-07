#[cfg(windows)]
#[test]
fn cross_hides_to_tray_and_normal_launch_reopens_the_same_terminal() {
    if std::env::var("SPRITX_UI_TESTS").as_deref() != Ok("1") {
        eprintln!("Set SPRITX_UI_TESTS=1 for the real tray lifecycle test");
        return;
    }
    let root = std::env::temp_dir().join(format!("spritx-tray-ui-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut process = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
        .env("APPDATA", &root)
        .env("SPRITX_DESKTOP_TEST", "1")
        .env("SPRITX_QUIET", "1")
        .env_remove("SPRITX_SMOKE")
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = process.kill();
            panic!("The tray close/reopen test did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success());
    let sessions = root.join("spritx/sessions");
    assert!(sessions.join("desktop-test-passed").exists());
    assert!(
        !sessions.join("desktop.json").exists(),
        "Quit must remove the running-instance endpoint"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(not(windows))]
#[test]
fn instance_lock_reopens_primary_and_releases_on_drop() {
    // Linux headless counterpart: the full tray lifecycle (Win32 hide/show,
    // native tray icon, HWND visibility) needs a display and is only
    // exercised on Windows above. What IS testable headlessly everywhere is
    // the single-instance handoff the tray lifecycle depends on — the
    // file-lock claim, second-launch reopen via the endpoint, and lock
    // release on drop — which this test mirrors from `src/desktop.rs`'s
    // unit test without requiring GPUI or a tray icon.
    use spritx_terminal::desktop::{Claim, DesktopAction, Instance};

    let root = std::env::temp_dir().join(format!(
        "spritx-desktop-lifecycle-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let Claim::Primary(first) = Instance::claim(&root).unwrap() else {
        panic!("Primary instance expected");
    };
    assert!(matches!(Instance::claim(&root).unwrap(), Claim::Reopened));
    assert_eq!(
        first.events.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
        DesktopAction::Show
    );
    drop(first);
    let Claim::Primary(second) = Instance::claim(&root).unwrap() else {
        panic!("The lock must be released after the primary drops");
    };
    Instance::send_at(&root.join("desktop.json"), DesktopAction::Quit).unwrap();
    assert_eq!(
        second.events.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
        DesktopAction::Quit
    );
    drop(second);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(not(windows))]
#[test]
#[ignore = "requires a Linux display and SPRITX_UI_TESTS=1; the StatusNotifier tray, retained-window minimize, and Wayland/X11 show/hide paths cannot be exercised headlessly"]
fn status_notifier_tray_lifecycle_requires_display() {
    // Explicit gap marker: on Linux, closing the window minimizes it
    // (`background_window`) rather than hiding it, and the StatusNotifier
    // tray icon needs a notification host. Neither is reachable headless,
    // so this stays ignored instead of leaving the Linux tray gap silent.
    unimplemented!()
}
