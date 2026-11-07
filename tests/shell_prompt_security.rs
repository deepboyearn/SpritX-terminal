#[test]
fn bash_treats_path_command_substitutions_as_literal_text() {
    let bash = if cfg!(windows) { "C:/Program Files/Git/bin/bash.exe" } else { "bash" };
    let script = r#"
source assets/shell/spritx-prompt.bash
PWD='/tmp/$(printf UNEXPECTED_EVALUATION)'
__spritx_node_dir=$PWD
__spritx_prompt >/dev/null
printf '%s' "${PS1@P}"
"#;
    let output = std::process::Command::new(bash)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["--noprofile", "--norc", "-c", script]).output().expect("Bash for shell regression tests");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("$(printf UNEXPECTED_EVALUATION)"), "{rendered}");
}

#[test]
fn interactive_bash_reports_the_executed_command_and_completion() {
    use spritx_terminal::backend::osc::{OscEvent, OscParser};
    use spritx_terminal::terminal::{SessionConfig, TerminalSession};
    use std::time::{Duration, Instant};
    let bash = if cfg!(windows) { "C:/Program Files/Git/bin/bash.exe" } else { "bash" };
    let prompt = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shell/spritx-prompt.bash");
    let mut config = SessionConfig::command(bash, ["--noprofile".into(), "--rcfile".into(), prompt.to_string_lossy().replace('\\', "/"), "-i".into()]);
    config.cwd = Some(env!("CARGO_MANIFEST_DIR").into());
    let mut terminal = TerminalSession::spawn(config).unwrap();
    assert!(terminal.wait_for_text("❯", Duration::from_secs(15)), "{}", terminal.screen_text());
    let command = "printf '%s%s\\n' BASH_ AUDIT_OK";
    terminal.write_str(&format!("{command}\r")).unwrap();
    let mut parser = OscParser::new();
    let mut recorded = None;
    let mut completed = false;
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if let Some(bytes) = terminal.pump_bytes() {
            for event in parser.feed(&bytes).events {
                match event.event {
                    OscEvent::CommandLine(text) => recorded = Some(text),
                    OscEvent::OutputEnd { exit_code: 0 } => completed = true,
                    _ => {}
                }
            }
        }
        if completed { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.kill();
    assert_eq!(recorded.as_deref(), Some(command));
    assert!(completed);
    assert!(terminal.screen_text().contains("BASH_AUDIT_OK"));
}
