use spritx_terminal::terminal::{SessionConfig, TerminalSession};
use gpui::{Keystroke, Modifiers};
use std::{net::TcpStream, time::{Duration, Instant}};

fn wait(session: &mut TerminalSession, timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        session.pump();
        if condition() { return true; }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn ctrl_c_reaches_node_npm_and_bun_servers_and_preserves_the_shell() {
    for persistent in [false, true] {
        for (name, command) in [("node", "node server.cjs"), ("npm", "npm run dev"), ("bun", "bun run --bun dev")] {
            // Optional external runtimes are not required to build SpritX.
            let program = if name == "npm" { "node" } else { name };
            let present = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir|
                dir.join(program).is_file() || (cfg!(windows) && dir.join(format!("{program}.exe")).is_file()));
            if !present { eprintln!("Skipping {name}: runtime is not installed"); continue; }
            let root = std::env::temp_dir().join(format!("spritx-interrupt-{}-{name}-{persistent}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("package.json"), r#"{"name":"interrupt-test","version":"1.0.0","scripts":{"dev":"node server.cjs"}}"#).unwrap();
            std::fs::write(root.join("server.cjs"), r#"
const fs = require('node:fs');
const server = require('node:http').createServer((req,res) => res.end('ok'));
process.on('SIGINT', () => {
  fs.writeFileSync('signal.txt', 'SIGINT');
  server.close(() => process.exit(0));
});
server.listen(0, '127.0.0.1', () => fs.writeFileSync('port.txt', String(server.address().port)));
"#).unwrap();
            let config = SessionConfig::for_shell(None, &[], Some(root.to_string_lossy().into_owned()));
            let mut session = if persistent {
                let connection = spritx_terminal::backend::session_owner::start_at(&config,
                    std::path::Path::new(env!("CARGO_BIN_EXE_spritx-terminal")), &root.join("owner")).unwrap();
                TerminalSession::attach_persistent(config, connection).unwrap()
            } else { TerminalSession::spawn(config).unwrap() };
            assert!(session.wait_for_text("❯", Duration::from_secs(15)), "shell not ready: {}", session.screen_text());
            let shell_pid = session.pid();
            session.write_str(&format!("{command}\r")).unwrap();
            let started = wait(&mut session, Duration::from_secs(15), || root.join("port.txt").exists());
            if !started { let output=session.screen_text(); session.kill(); panic!("{name}/{persistent} failed to start: {output}"); }
            let port: u16 = std::fs::read_to_string(root.join("port.txt")).unwrap().parse().unwrap();
            assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
            let ctrl_c = spritx_terminal::keys::encode_keystroke(&Keystroke {
                modifiers: Modifiers::control(), key: "c".into(), key_char: None,
            }).unwrap();
            session.write(&ctrl_c).unwrap();
            let stopped = wait(&mut session, Duration::from_secs(8), || root.join("signal.txt").exists()
                && TcpStream::connect(("127.0.0.1", port)).is_err());
            if !stopped { let output=session.screen_text(); session.kill(); panic!("Ctrl+C did not interrupt {name}, persistent={persistent}: {output}"); }
            #[cfg(windows)]
            session.write_str("[Console]::WriteLine(('SHELL_'+'STILL_READY'))\r").unwrap();
            #[cfg(not(windows))]
            session.write_str("printf 'SHELL_%s\\n' STILL_READY\r").unwrap();
            let reusable = session.wait_for_text("SHELL_STILL_READY", Duration::from_secs(8));
            let output = session.screen_text();
            assert_eq!(session.pid(), shell_pid);
            session.kill();
            assert!(reusable, "shell did not recover after {name}: {output}");
            println!("{name}: SIGINT delivered, server port released, shell reused (persistent={persistent})");
        }
    }
}

#[test]
#[cfg(debug_assertions)]
#[ignore = "requires a display and Node.js; run with --ignored"]
fn real_keyboard_interrupts_and_force_stop_is_scoped_to_one_pane() {
    use spritx_terminal::{backend::workspace::Workspace, state::TabState};
    let root = std::env::temp_dir().join(format!("spritx-process-ui-{}", std::process::id()));
    let project = root.join("project");
    let config = root.join("spritx");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(config.join("sessions")).unwrap();
    std::fs::write(config.join("config.toml"), "[notifications]\nenabled = false\nsound = false\n[behavior]\nkeepSessions = false\n").unwrap();
    let mut tab = TabState::single(1, "Terminal", 1);
    tab.leaves.get_mut(&1).unwrap().cwd = project.to_string_lossy().into_owned();
    let saved = Workspace { tabs: vec![tab], ..Default::default() };
    std::fs::write(config.join("sessions/workspace.json"), serde_json::to_vec(&saved).unwrap()).unwrap();
    std::fs::write(project.join("server.cjs"), r#"
const fs=require('node:fs'), http=require('node:http');
const server=http.createServer((q,r)=>r.end('ok'));
process.on('SIGINT',()=>{ fs.writeFileSync('signal.txt','SIGINT'); server.close(()=>process.exit(0)); });
server.listen(0,'127.0.0.1',()=>fs.writeFileSync('port.txt',String(server.address().port)));
"#).unwrap();
    std::fs::write(project.join("force-server.cjs"), r#"
const fs=require('node:fs'), http=require('node:http');
const server=http.createServer((q,r)=>r.end('ok'));
process.on('SIGINT',()=>fs.writeFileSync('ignored.txt','SIGINT intentionally ignored'));
server.listen(0,'127.0.0.1',()=>fs.writeFileSync('force-port.txt',String(server.address().port)));
"#).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
        .env(if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" }, &root)
        .env("SPRITX_PROCESS_ROOT", &project).env("SPRITX_PROCESS_TEST", "1")
        .env("SPRITX_QUIET", "1").env_remove("SPRITX_SMOKE").spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if started.elapsed() > Duration::from_secs(75) { let _=child.kill(); panic!("process-control UI test timed out"); }
        std::thread::sleep(Duration::from_millis(30));
    };
    assert!(status.success(), "process-control UI exited with {status}");
}
