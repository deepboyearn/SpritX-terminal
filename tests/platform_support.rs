use spritx_terminal::backend::{config, history::history_path};
use spritx_terminal::platform::{self, ShellKind};
use spritx_terminal::terminal::SessionConfig;

#[test]
fn config_history_and_device_store_share_the_platform_directory() {
    let dir = platform::config_dir().expect("user config directory");
    assert_eq!(config::config_path(), Some(dir.join("config.toml")));
    assert_eq!(history_path(), Some(dir.join("history.jsonl")));
    assert_eq!(
        spritx_terminal::backend::remote::DeviceStore::default_root(),
        Some(dir.join("remote"))
    );
    #[cfg(windows)]
    if let Some(roaming) = std::env::var_os("APPDATA") {
        assert_eq!(dir, std::path::PathBuf::from(roaming).join("spritx"));
    }
}

#[test]
fn configured_executable_and_arguments_are_preserved() {
    let args =
        platform::parse_shell_args(r#"["-NoProfile","-File","C:\\My Scripts\\init.ps1"]"#).unwrap();
    let config = SessionConfig::for_shell(
        Some("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
        &args,
        Some("C:\\My Projects".into()),
    );
    assert_eq!(config.program, "C:\\Program Files\\PowerShell\\7\\pwsh.exe");
    assert_eq!(config.args, args);
    assert_eq!(config.cwd.as_deref(), Some("C:\\My Projects"));
    assert!(config.env.is_empty());
    assert!(platform::parse_shell_args("[broken").is_err());
    assert_eq!(
        platform::parse_shell_args("-l -i").unwrap(),
        vec!["-l", "-i"]
    );
}

#[test]
fn paths_are_quoted_for_the_selected_shell() {
    assert_eq!(
        platform::shell_kind(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        ShellKind::PowerShell
    );
    assert_eq!(platform::shell_kind("CMD.EXE"), ShellKind::Cmd);
    assert_eq!(platform::shell_kind("/bin/bash"), ShellKind::Posix);
    assert_eq!(
        platform::quote_path(r"C:\O'Brien\a $b.txt", ShellKind::PowerShell),
        r"'C:\O''Brien\a $b.txt'"
    );
    assert_eq!(
        platform::quote_path("/tmp/O'Brien", ShellKind::Posix),
        "'/tmp/O'\\''Brien'"
    );
    assert_eq!(
        platform::quote_path(r"C:\My Projects\a&b.txt", ShellKind::Cmd),
        r#""C:\My Projects\a&b.txt""#
    );
    assert!(platform::quote_path("C:\\%PATH%", ShellKind::Cmd).is_empty());
    assert!(platform::quote_path("C:\\!name!", ShellKind::Cmd).is_empty());
}

#[test]
fn replacing_existing_config_preserves_the_new_value() {
    let dir = std::env::temp_dir().join(format!("spritx_replace_config_{}", std::process::id()));
    let path = dir.join("config.toml");
    let mut value = config::AppConfig::default();
    config::save_to(&path, &value).unwrap();
    value.appearance.font_size = 19;
    config::save_to(&path, &value).unwrap();
    assert_eq!(config::load_from(&path).appearance.font_size, 19);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(windows)]
mod windows {
    use super::*;
    use spritx_terminal::terminal::TerminalSession;
    use std::time::{Duration, Instant};

    #[test]
    fn prompt_header_fits_small_split_panes() {
        let script = format!(
            "$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false);\n{}\n{}",
            include_str!("../assets/shell/spritx-shell-init.ps1"),
            r#"$result = foreach ($width in @(8,12,24,40,80)) {
                $bar = Format-SpritXPromptBar 'LongUserNameForTesting' 'long-project-directory-name' 'v26.12.1' '23:10' $width
                $plain = $bar -replace '\x1b\[[0-9;]*m', ''
                @{ width=$width; cells=(Get-SpritXCells $plain) }
            }; $result | ConvertTo-Json -Compress"#,
        );
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let output = std::process::Command::new(platform::default_shell())
            .args(["-NoLogo", "-NoProfile", "-EncodedCommand", &spritx_terminal::backend::remote::b64(&utf16)])
            .output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows.len(), 5);
        for row in rows {
            assert!(row["cells"].as_u64().unwrap() < row["width"].as_u64().unwrap(), "{row}");
        }
    }

    fn powershell(script: &str) -> TerminalSession {
        let mut config = SessionConfig::command(
            platform::default_shell(),
            [
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                script.into(),
            ],
        );
        // Exercise a cwd with spaces and Unicode, independent of the user's shell.
        let dir = std::env::temp_dir().join(format!("spritx windows 测试 {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        config.cwd = Some(dir.to_string_lossy().into_owned());
        TerminalSession::spawn(config).expect("spawn PowerShell through ConPTY")
    }

    #[test]
    fn conpty_input_output_and_exit_code() {
        let mut session = powershell("[Console]::WriteLine('READY'); $line = [Console]::ReadLine(); [Console]::WriteLine('GOT:' + $line); exit 7");
        assert!(session.wait_for_text("READY", Duration::from_secs(15)));
        session.write_str("hello windows\r").unwrap();
        assert!(
            session.wait_for_text("GOT:hello windows", Duration::from_secs(10)),
            "{}",
            session.screen_text()
        );
        let until = Instant::now() + Duration::from_secs(10);
        while session.exit_code().is_none() && Instant::now() < until {
            session.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.exit_code(), Some(7));
    }

    #[test]
    fn conpty_resize_and_kill() {
        let mut session = powershell("[Console]::WriteLine('ALIVE'); Start-Sleep -Seconds 60");
        assert!(session.wait_for_text("ALIVE", Duration::from_secs(15)));
        session.resize(90, 24).unwrap();
        assert_eq!(session.size(), (90, 24));
        session.kill();
        let until = Instant::now() + Duration::from_secs(10);
        while session.exit_code().is_none() && Instant::now() < until {
            session.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(session.exit_code().is_some());
    }

    #[test]
    fn cmd_runs_without_a_posix_shell() {
        let mut session = TerminalSession::spawn(SessionConfig::command(
            "cmd.exe",
            [
                "/D".into(),
                "/C".into(),
                "echo CMD_READY & exit /b 9".into(),
            ],
        ))
        .unwrap();
        assert!(session.wait_for_text("CMD_READY", Duration::from_secs(15)));
        let until = Instant::now() + Duration::from_secs(10);
        while session.exit_code().is_none() && Instant::now() < until {
            session.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.exit_code(), Some(9));
    }

    #[test]
    fn interactive_powershell_accepts_commands_after_ctrl_c() {
        let config = SessionConfig::command(
            platform::default_shell(),
            [
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NoExit".into(),
                "-Command".into(),
                include_str!("../assets/shell/spritx-shell-init.ps1").into(),
            ],
        );
        let mut session = TerminalSession::spawn(config).unwrap();
        assert!(
            session.wait_for_text("❯", Duration::from_secs(15)),
            "{}",
            session.screen_text()
        );
        // No literal sentinel in the input: seeing it proves execution, not echo.
        session.write_str("Start-Sleep -Seconds 60\r").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        session.write(&[3]).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        session
            .write_str("[Console]::WriteLine(('AFTER_' + 'INTERRUPT'))\r")
            .unwrap();
        assert!(
            session.wait_for_text("AFTER_INTERRUPT", Duration::from_secs(15)),
            "{}",
            session.screen_text()
        );
    }

    #[test]
    fn prompt_reuses_trailing_blank_rows_without_overwriting_output() {
        let script = format!(
            "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false);\n{}\n[Console]::Write(\"KEEP_ONE`r`nKEEP_TWO`r`n`r`n`r`n\"); [Console]::Write((prompt)); Start-Sleep -Milliseconds 300",
            include_str!("../assets/shell/spritx-shell-init.ps1"),
        );
        let mut session = powershell(&script);
        assert!(session.wait_for_text("❯", Duration::from_secs(15)), "{}", session.screen_text());
        let lines = session.visible_lines();
        let last_output = lines.iter().position(|line| line.contains("KEEP_TWO")).unwrap();
        assert!(lines.iter().any(|line| line.contains("KEEP_ONE")));
        assert!(!lines[last_output + 1].trim().is_empty(), "extra blank rows before prompt: {lines:?}");
        assert!(lines[last_output + 2].contains("❯"), "typing row must follow header: {lines:?}");
        assert_eq!(session.powerline_prompt_row(), Some((last_output + 1) as u16));
    }

    #[test]
    fn powershell_prompt_reports_the_real_working_directory() {
        let script = format!(
            "{}\n$null = prompt; [Console]::WriteLine('DONE')",
            include_str!("../assets/shell/spritx-shell-init.ps1")
        );
        let mut session = powershell(&script);
        let bytes = {
            let until = Instant::now() + Duration::from_secs(15);
            let mut bytes = Vec::new();
            while Instant::now() < until {
                if let Some(chunk) = session.pump_bytes() {
                    bytes.extend(chunk);
                }
                if session.screen_text().contains("DONE") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            bytes
        };
        let mut tracker = spritx_terminal::backend::osc::BlockTracker::default();
        tracker.feed(&bytes);
        let cwd = tracker
            .current_cwd()
            .expect("OSC 7 cwd from PowerShell prompt");
        assert!(std::path::Path::new(cwd).is_dir(), "{cwd}");
        assert!(cwd.contains("测试"), "{cwd}");
    }
}
