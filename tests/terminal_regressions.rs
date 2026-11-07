use spritx_terminal::terminal::{SessionConfig, TerminalSession};
use std::time::Duration;

fn output(windows: &str, posix: &str, cols: u16) -> TerminalSession {
    #[cfg(windows)]
    let mut config = SessionConfig::command("powershell.exe", ["-NoProfile".into(), "-Command".into(), windows.into()]);
    #[cfg(not(windows))]
    let mut config = SessionConfig::command("/bin/sh", ["-c".into(), posix.into()]);
    let _ = (windows, posix);
    config.cols = cols;
    config.rows = 8;
    TerminalSession::spawn(config).unwrap()
}

#[test]
fn copying_soft_wraps_preserves_commands_but_keeps_real_newlines() {
    let mut session = output(
        "[Console]::WriteLine('ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789'); [Console]::Write('NEXT_AUDIT_DONE'); [Console]::ReadLine()",
        "printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\nNEXT_AUDIT_DONE'; read line", 20);
    assert!(session.wait_for_text("NEXT_AUDIT_DONE", Duration::from_secs(10)));
    session.begin_selection(0, 0);
    session.extend_selection(2, 14);
    let copied = session.selection_text().unwrap();
    session.kill();
    assert_eq!(copied, "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\nNEXT_AUDIT_DONE");
}

#[test]
fn prompt_at_first_row_has_the_same_spacing_boundary_as_other_rows() {
    let mut session = output(
        "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write(\" x `r`n❯ AUDIT_DONE\"); [Console]::ReadLine()",
        "printf ' x \r\n❯ AUDIT_DONE'; read line", 60);
    assert!(session.wait_for_text("AUDIT_DONE", Duration::from_secs(10)));
    assert_eq!(session.powerline_prompt_row(), Some(0), "{}", session.screen_text());
    session.kill();
}

#[test]
fn repeated_ls_and_alias_keep_each_listing_once_and_record_command_text() {
    use spritx_terminal::backend::osc::{OscEvent, OscParser};
    use std::time::Instant;
    let root = std::env::temp_dir().join(format!("spritx-ls-regression-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for name in ["audit_file_alpha.txt", "audit_file_beta.txt"] { std::fs::write(root.join(name), "test").unwrap(); }
    let mut config = SessionConfig::for_shell(None, &[], Some(root.to_string_lossy().into_owned()));
    config.rows = 300; config.cols = 90;
    let mut session = TerminalSession::spawn(config).unwrap();
    assert!(session.wait_for_text("❯", Duration::from_secs(15)), "{}", session.screen_text());
    #[cfg(windows)]
    let commands = ["ls", "Set-Alias auditls Get-ChildItem; auditls", "ls"];
    #[cfg(not(windows))]
    let commands = ["ls", "alias auditls='ls'; eval auditls", "ls"];
    let mut osc = OscParser::new();
    for command in commands.into_iter().cycle().take(12) {
        session.write_str(&format!("{command}\r")).unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        let mut completed = false;
        let mut recorded = None;
        while Instant::now() < until {
            if let Some(bytes) = session.pump_bytes() {
                for event in osc.feed(&bytes).events {
                    match event.event {
                        OscEvent::CommandLine(text) => recorded = Some(text),
                        OscEvent::OutputEnd { exit_code } => { assert_eq!(exit_code, 0); completed = true; }
                        _ => {}
                    }
                }
            }
            if completed { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(completed, "no completion for {command}: {}", session.screen_text());
        assert_eq!(recorded.as_deref(), Some(command));
        // The completion marker precedes the prompt; allow its trailing bytes
        // to settle before submitting the next command to the line editor.
        std::thread::sleep(Duration::from_millis(60)); session.pump();
    }
    let text = session.screen_text();
    session.kill();
    assert_eq!(text.matches("audit_file_alpha.txt").count(), 12, "{text}");
    assert_eq!(text.matches("audit_file_beta.txt").count(), 12, "{text}");
}
