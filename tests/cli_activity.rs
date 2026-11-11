use spritx_terminal::{
    backend::cli_activity::{screen_fingerprint, CliActivity, QUIET_AFTER},
    terminal::{SessionConfig, TerminalSession},
};
use std::time::{Duration, Instant};

#[test]
fn unknown_interactive_process_can_be_idle_then_busy_then_confirm_done_without_exiting() {
    // An arbitrary executable, not any branded launcher CLI. It stays alive
    // after reporting a task boundary, like an interactive CLI with hooks.
    #[cfg(windows)]
    let config = SessionConfig::command("powershell.exe", ["-NoProfile".into(), "-Command".into(),
        "[Console]::WriteLine('ARBITRARY_READY'); $null=[Console]::ReadLine(); [Console]::WriteLine('TASK_OUTPUT'); $null=[Console]::ReadLine(); [Console]::Write([char]27+']133;D;0'+[char]7); [Console]::WriteLine('TASK_FINISHED'); $null=[Console]::ReadLine()".into()]);
    #[cfg(not(windows))]
    let config = SessionConfig::command("/bin/sh", ["-c".into(),
        "printf 'ARBITRARY_READY\\n'; read task; printf 'TASK_OUTPUT\\n'; read finish; printf '\\033]133;D;0\\007TASK_FINISHED\\n'; read exit".into()]);
    let temp = std::env::temp_dir().canonicalize().unwrap();
    let root = temp.join(format!("spritx-unknown-cli-{}", std::process::id()));
    let connection = spritx_terminal::backend::session_owner::start_at(
        &config,
        std::path::Path::new(env!("CARGO_BIN_EXE_spritx-terminal")),
        &root,
    )
    .unwrap();
    let mut terminal = TerminalSession::attach_persistent(config, connection).unwrap();
    assert!(terminal.wait_for_text("ARBITRARY_READY", Duration::from_secs(10)));
    let now = Instant::now();
    let mut activity = CliActivity::default();
    activity.observe_screen(screen_fingerprint(&terminal.live_screen_text()), true, now);
    assert!(
        !activity.active(now),
        "open process alone is not active work"
    );

    activity.input(b"task\r", true, now);
    terminal.write(b"task\r").unwrap();
    assert!(terminal.wait_for_text("TASK_OUTPUT", Duration::from_secs(10)));
    assert!(activity.active(now));
    let text = screen_fingerprint(&terminal.live_screen_text());
    activity.observe_screen(text, true, now + Duration::from_millis(300));
    let quiet = now + QUIET_AFTER + Duration::from_secs(1);
    activity.observe_screen(text, true, quiet);
    assert!(
        !activity.active(quiet),
        "quiet interactive CLI must stop animating"
    );
    let status = terminal.owner_status().unwrap();
    assert!(status.running, "quiet is not confirmed completion");
    assert_eq!(status.completion, 0);
    assert_eq!(status.exit_code, None);

    terminal.write(b"finish\r").unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        terminal.pump();
        if terminal.owner_status().unwrap().completion > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = terminal.owner_status().unwrap();
    terminal.kill();
    assert_eq!(root.parent(), Some(temp.as_path()));
    let _ = std::fs::remove_dir_all(&root);
    assert!(!status.running);
    assert_eq!(status.exit_code, Some(0));
    assert!(status.completion > 0, "confirmed completion enables Done");
    assert_eq!(
        status.process_exit, None,
        "task completion need not close the CLI"
    );
}

#[test]
fn inspecting_live_output_does_not_move_the_users_scrollback_view() {
    #[cfg(windows)]
    let mut config = SessionConfig::command("powershell.exe", ["-NoProfile".into(), "-Command".into(),
        "1..30 | ForEach-Object { [Console]::WriteLine('history-'+$_) }; [Console]::Write('LIVE_READY'); $null=[Console]::ReadLine()".into()]);
    #[cfg(not(windows))]
    let mut config = SessionConfig::command("/bin/sh", ["-c".into(),
        "i=0; while [ $i -lt 30 ]; do printf 'history-%s\\n' $i; i=$((i+1)); done; printf LIVE_READY; read exit".into()]);
    config.rows = 8;
    let mut terminal = TerminalSession::spawn(config).unwrap();
    assert!(terminal.wait_for_text("LIVE_READY", Duration::from_secs(10)));
    terminal.set_scrollback(15);
    let before = terminal.screen_text();
    let offset = terminal.screen().scrollback();
    let live = terminal.live_screen_text();
    terminal.kill();
    assert!(offset > 0);
    assert!(!before.contains("LIVE_READY"));
    assert!(live.contains("LIVE_READY"));
    assert_eq!(terminal.screen().scrollback(), offset);
    assert_eq!(terminal.screen_text(), before);
}
