//! Exercise the actual background-owner executable across a client detach.
use spritx_terminal::terminal::{SessionConfig, TerminalSession};
use std::time::Duration;

fn detached_tick_number(text: &str) -> u32 {
    let mut best = 0;
    let mut rest = text;
    while let Some(at) = rest.find("DETACHED_TICK_") {
        let tail = &rest[at + "DETACHED_TICK_".len()..];
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u32>() { best = best.max(n); }
        rest = &tail[digits.len()..];
    }
    best
}

#[test]
fn identical_configurations_create_independent_panes() {
    let root = std::env::temp_dir().join(format!("spritx-independent-{}", std::process::id()));
    #[cfg(windows)]
    let config = SessionConfig::command("powershell.exe", ["-NoProfile".into(), "-Command".into(), "[Console]::WriteLine('INDEPENDENT_READY'); [Console]::ReadLine()".into()]);
    #[cfg(not(windows))]
    let config = SessionConfig::command("/bin/sh", ["-c".into(), "printf 'INDEPENDENT_READY\n'; read line".into()]);
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_spritx-terminal"));
    let first = spritx_terminal::backend::session_owner::start_at(&config, executable, &root).unwrap();
    let mut a = TerminalSession::attach_persistent(config.clone(), first.clone()).unwrap();
    let second = spritx_terminal::backend::session_owner::start_at(&config, executable, &root).unwrap();
    let mut b = TerminalSession::attach_persistent(config, second.clone()).unwrap();
    assert!(a.wait_for_text("INDEPENDENT_READY", Duration::from_secs(10)));
    assert!(b.wait_for_text("INDEPENDENT_READY", Duration::from_secs(10)));
    a.kill(); b.kill();
    assert_ne!(first.port, second.port);
    assert_ne!(a.pid(), b.pid());
}

#[test]
fn owner_preserves_process_output_input_and_resize_across_detach() {
    let root = std::env::temp_dir().join(format!("spritx-owner-{}.{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    std::fs::create_dir_all(&root).unwrap();
    let config = {
        #[cfg(windows)]
        {
            SessionConfig::command("powershell.exe", vec![
                "-NoProfile".into(), "-Command".into(),
                "[Console]::Write([char]27+'[?1049h'); [Console]::Write([char]27+']133;C'+[char]7); [Console]::WriteLine('OWNER_READY'); $buf=''; $tick=0; while($true){ if([Console]::KeyAvailable){ $k=[Console]::ReadKey($true); if($k.Key -eq [ConsoleKey]::Enter){ break }; $buf += $k.KeyChar }; $tick++; if($tick % 10 -eq 0){ [Console]::WriteLine(\"DETACHED_TICK_$tick\") }; Start-Sleep -Milliseconds 20 }; [Console]::WriteLine('RECONNECTED_'+$buf); [Console]::Write([char]27+']133;D;0'+[char]7)".into(),
            ])
        }
        #[cfg(not(windows))]
        {
            SessionConfig::command("/bin/sh", vec!["-c".into(), r#"printf '\033[?1049h\033]133;C\007OWNER_READY\n'; ( i=1; while [ $i -le 40 ]; do printf 'DETACHED_TICK_%s\n' "$i"; i=$((i+1)); sleep 0.2; done ) & read line; printf 'RECONNECTED_%s\n' "$line"; printf '\033]133;D;0\007'"#.into()])
        }
    };
    let connection = spritx_terminal::backend::session_owner::start_at(
        &config,
        std::path::Path::new(env!("CARGO_BIN_EXE_spritx-terminal")),
        &root,
    )
    .unwrap();
    let mut first = TerminalSession::attach_persistent(config.clone(), connection.clone()).unwrap();
    assert!(first.wait_for_text("OWNER_READY", Duration::from_secs(10)));
    assert!(first.owner_status().unwrap().running);
    let started_at = first.owner_status().unwrap().started_at_ms;
    assert!(started_at.is_some(), "running CLI must publish its real start timestamp");
    assert!(first.owner_status().unwrap().command.is_some());
    assert!(first.screen().alternate_screen());
    assert!(
        TerminalSession::attach_persistent(config.clone(), connection.clone()).is_err(),
        "A second UI must not take over a live lease"
    );
    let mut wrong = connection.clone();
    wrong.token = "wrong-token".into();
    assert!(TerminalSession::attach_persistent(config.clone(), wrong).is_err());
    let pid = first.pid();
    let first_tick = detached_tick_number(&first.screen_text());
    drop(first);
    let detached_started = std::time::Instant::now();
    let mut second = loop {
        match TerminalSession::attach_persistent(config.clone(), connection.clone()) {
            Ok(session) => break session,
            Err(error) => {
                assert!(
                    detached_started.elapsed() < Duration::from_secs(5),
                    "re-attach failed after detach: {error}"
                );
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    };
    let deadline = std::time::Instant::now();
    while detached_tick_number(&second.screen_text()) <= first_tick
        && deadline.elapsed() < Duration::from_secs(10)
    {
        second.pump();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        detached_tick_number(&second.screen_text()) > first_tick,
        "output produced while detached was dropped (first_tick={first_tick}): {}",
        second.screen_text()
    );
    assert_eq!(second.pid(), pid);
    assert_eq!(second.owner_status().unwrap().started_at_ms, started_at, "reattach must not restart the elapsed timer");
    assert!(second.screen().alternate_screen());
    assert!(second.screen_text().contains("OWNER_READY"));
    second.resize(97, 31).unwrap();
    assert_eq!(second.size(), (97, 31));
    second.write(b"ok\r").unwrap();
    assert!(
        second.wait_for_text("RECONNECTED_ok", Duration::from_secs(10)),
        "{}",
        second.screen_text()
    );
    let started = std::time::Instant::now();
    while second.owner_status().unwrap().completion == 0
        && started.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(30));
    }
    let status = second.owner_status().unwrap();
    assert_eq!(status.completion, 1, "completion must be observable on every platform: {status:?}");
    assert_eq!(status.exit_code, Some(0), "{status:?}");
    assert!(!status.running);
    second.kill();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn saved_output_is_readable_in_scrollback_without_replaying_a_command() {
    let mut config = {
        #[cfg(windows)]
        {
            SessionConfig::command(
                "powershell.exe",
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "[Console]::WriteLine('LIVE_READY'); [Console]::ReadLine()".into(),
                ],
            )
        }
        #[cfg(not(windows))]
        {
            SessionConfig::command(
                "/bin/sh",
                vec!["-c".into(), "printf 'LIVE_READY\n'; read line".into()],
            )
        }
    };
    config.initial_output = Some("ARCHIVED_OUTPUT_ONLY\r\n".into());
    let mut terminal = TerminalSession::spawn(config).unwrap();
    assert!(terminal.wait_for_text("LIVE_READY", Duration::from_secs(10)));
    let before = terminal.screen_text();
    assert!(!before.contains("ARCHIVED_OUTPUT_ONLY"));
    assert!(terminal.saved_screen().contains("ARCHIVED_OUTPUT_ONLY"));
    assert!(terminal.scroll_lines(-1000));
    assert!(terminal.screen_text().contains("ARCHIVED_OUTPUT_ONLY"));
    terminal.set_scrollback(0);
    assert_eq!(terminal.screen_text(), before);
    terminal.kill();
}

#[test]
fn busy_cli_output_keeps_the_owner_connection_stable() {
    let root = std::env::temp_dir().join(format!("spritx-owner-output-{}", std::process::id()));
    let config = {
        #[cfg(windows)]
        {
            SessionConfig::command("powershell.exe", vec![
                "-NoProfile".into(), "-Command".into(),
                "[Console]::WriteLine('BURST_READY'); $null=[Console]::ReadLine(); [Console]::Write([char]27+']133;C'+[char]7); $block='x'*4096; for($i=0;$i -lt 256;$i++){[Console]::Write($block)}; [Console]::WriteLine('BURST_DONE'); [Console]::Write([char]27+']133;D;0'+[char]7); $null=[Console]::ReadLine()".into()
            ])
        }
        #[cfg(not(windows))]
        {
            SessionConfig::command("/bin/sh", vec!["-c".into(), r#"printf 'BURST_READY\n'; read start; printf '\033]133;C\007'; head -c 1048576 /dev/zero | tr '\000' x; printf '\nBURST_DONE\n\033]133;D;0\007'; read finish"#.into()])
        }
    };
    let connection = spritx_terminal::backend::session_owner::start_at(
        &config,
        std::path::Path::new(env!("CARGO_BIN_EXE_spritx-terminal")),
        &root,
    )
    .unwrap();
    let mut terminal = TerminalSession::attach_persistent(config, connection).unwrap();
    assert!(terminal.wait_for_text("BURST_READY", Duration::from_secs(10)));
    terminal.write(b"go\r").unwrap();
    let start = std::time::Instant::now();
    let mut disconnected = false;
    let mut finished = false;
    while start.elapsed() < Duration::from_secs(20) {
        terminal.pump();
        disconnected |= terminal.owner_status().unwrap().disconnected;
        if terminal.screen_text().contains("BURST_DONE")
            && terminal.owner_status().unwrap().completion > 0
        {
            finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.kill();
    let _ = std::fs::remove_dir_all(root);
    assert!(finished, "CLI burst did not finish streaming");
    assert!(!disconnected, "CLI output caused a false disconnect");
}
