//! Real HTTP requests through the same PTY and shell initialization as the app.
use spritx_terminal::terminal::{SessionConfig, TerminalSession};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(port: u16) -> std::path::PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = Instant::now().elapsed().as_nanos();
    std::env::temp_dir().join(format!(
        "spritx-curl-{}-{}-{}-{}",
        std::process::id(),
        port,
        nonce,
        nanos
    ))
}

#[test]
fn curl_get_headers_json_queries_and_failures_work_through_the_terminal() {
    // Port: bind 0 = OS-assigned unique port per invocation; read back the
    // actual port for every URL. No fixed port, so parallel test binaries
    // cannot collide. The bound listener already accepts before requests.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    // Server reports each completed request over a channel so the test can
    // pace itself deterministically on the server-side acknowledgement.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let until = Instant::now() + Duration::from_secs(60);
        while requests.len() < 5 && Instant::now() < until {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut bytes = Vec::new();
            let header_end;
            loop {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") {
                    header_end = end + 4;
                    break;
                }
            }
            let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() - header_end < length {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let failed = header.starts_with("GET /fail ");
            let status = if failed { "418 Teapot" } else { "200 OK" };
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
            stream.write_all(response.as_bytes()).unwrap();
            requests.push(String::from_utf8(bytes).unwrap());
            let _ = done_tx.send(());
        }
        requests
    });

    // Unique temp dir per invocation (pid + ephemeral port + counter + time)
    // so parallel invocations never share body.json or the shell cwd.
    let root = unique_temp_dir(port);
    std::fs::create_dir_all(&root).unwrap();
    let data = root.join("body.json");
    std::fs::write(&data, r#"{"message":"hello world"}"#).unwrap();
    let config = SessionConfig::for_shell(None, &[], Some(root.to_string_lossy().into_owned()));
    let mut terminal = TerminalSession::spawn(config).unwrap();
    #[cfg(windows)]
    let command = format!(
        r#"
curl -sS --max-time 5 http://127.0.0.1:{port}/get
curl.exe -sS --max-time 5 -H 'X-SpritX: custom value' http://127.0.0.1:{port}/header
curl -sS --max-time 5 -H 'Content-Type: application/json' --data-binary '@body.json' http://127.0.0.1:{port}/json
curl -sS --max-time 5 'http://127.0.0.1:{port}/query?a=1&b=two'
curl -sS --max-time 5 --fail http://127.0.0.1:{port}/fail
[Console]::WriteLine(('CURL_RESULT_' + $LASTEXITCODE))
"#
    );
    #[cfg(not(windows))]
    let command = format!(
        r#"
curl -sS --max-time 5 http://127.0.0.1:{port}/get
curl -sS --max-time 5 -H 'X-SpritX: custom value' http://127.0.0.1:{port}/header
curl -sS --max-time 5 -H 'Content-Type: application/json' --data-binary '@body.json' http://127.0.0.1:{port}/json
curl -sS --max-time 5 'http://127.0.0.1:{port}/query?a=1&b=two'
curl -sS --max-time 5 --fail http://127.0.0.1:{port}/fail
printf 'CURL_RESULT_%s\n' "$?"
"#
    );
    // Wait for the shell prompt (bounded poll) instead of a fixed sleep:
    // typing before the prompt sends keys into shell init instead of the
    // line editor.
    assert!(
        terminal.wait_for_text("❯", Duration::from_secs(20)),
        "{}",
        terminal.screen_text()
    );
    let mut lifecycle = spritx_terminal::backend::osc::OscParser::new();
    let commands: Vec<&str> = command
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    for line in commands {
        terminal.write(line.as_bytes()).unwrap();
        terminal.write(b"\r").unwrap();
        // Pace input deterministically: after each curl command, wait until
        // the server confirms it handled that request (channel ack) before
        // typing the next line, so Enter keys can never bunch into a
        // not-yet-ready readline / PTY buffer under parallel contention.
        // The final marker line is handled by the CURL_RESULT gate below.
        if line.starts_with("curl") {
            assert!(
                done_rx.recv_timeout(Duration::from_secs(15)).is_ok(),
                "server did not handle command: {}\n{}",
                line,
                terminal.screen_text()
            );
            // An old prompt remains on-screen: wait for this command's actual
            // completion marker instead of mistaking it for a fresh prompt.
            let until = Instant::now() + Duration::from_secs(10);
            let mut completed = false;
            while Instant::now() < until {
                if let Some(bytes) = terminal.pump_bytes() {
                    completed |= lifecycle.feed(&bytes).events.iter().any(|event|
                        matches!(event.event, spritx_terminal::backend::osc::OscEvent::OutputEnd { .. }));
                }
                if completed { break; }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(completed, "curl never returned to the shell: {line}");
        }
    }
    assert!(
        terminal.wait_for_text("CURL_RESULT_22", Duration::from_secs(60)),
        "{}",
        terminal.screen_text()
    );
    terminal.kill();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[0].starts_with("GET /get "));
    assert!(requests[1].contains("X-SpritX: custom value"));
    assert!(requests[2].ends_with(r#"{"message":"hello world"}"#));
    assert!(requests[3].starts_with("GET /query?a=1&b=two "));
    drop(terminal);
    let started = Instant::now();
    while let Err(error) = std::fs::remove_dir_all(&root) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Cleanup failed: {error}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}
