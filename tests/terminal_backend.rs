#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use spritx_terminal::terminal::{SessionConfig, TerminalSession};

#[cfg(unix)]
fn wait_for(session: &mut TerminalSession, needle: &str, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        session.pump();
        if session.screen_text().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    session.pump();
    session.screen_text().contains(needle)
}

#[cfg(unix)]
fn shell(script: &str) -> TerminalSession {
    TerminalSession::spawn(SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), script.to_string()],
    ))
    .expect("spawn test shell")
}

#[cfg(unix)]
#[test]
fn shell_echo_round_trip() {
    let mut session = shell(
        "printf 'READY\\n'; IFS= read -r line; printf 'GOT:%s\\n' \"$line\"; exit 7",
    );
    assert!(wait_for(&mut session, "READY", Duration::from_secs(5)));
    session.write_str("hello world\n").expect("write");
    assert!(
        wait_for(&mut session, "GOT:hello world", Duration::from_secs(5)),
        "screen was: {}",
        session.screen_text()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while session.exit_code().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(session.exit_code(), Some(7));
}

#[cfg(unix)]
#[test]
fn ansi_colors_are_parsed() {
    let mut session = shell("printf '\\033[31mRED\\033[0m normal\\n'; sleep 0.1");
    assert!(wait_for(&mut session, "RED normal", Duration::from_secs(5)));
    let cell = session.screen().cell(0, 0).expect("cell 0,0");
    assert_eq!(cell.contents(), "R");
    assert_ne!(format!("{:?}", cell.fgcolor()), "Default");
}

#[cfg(unix)]
#[test]
fn ctrl_c_interrupts_foreground_job() {
    let mut session = shell(
        "trap 'printf \"INTERRUPTED\\n\"; exit 130' INT; printf 'READY\\n'; while :; do sleep 1; done",
    );
    assert!(wait_for(&mut session, "READY", Duration::from_secs(5)));
    session.write(&[0x03]).expect("send ctrl-c");
    assert!(
        wait_for(&mut session, "INTERRUPTED", Duration::from_secs(5)),
        "screen was: {}",
        session.screen_text()
    );
}

#[cfg(unix)]
#[test]
fn resize_and_scrollback_api() {
    let mut session = shell("seq 1 60; sleep 0.2");
    assert!(wait_for(&mut session, "60", Duration::from_secs(5)));
    assert_eq!(session.size(), (100, 30));
    session.resize(40, 12).expect("resize");
    assert_eq!(session.size(), (40, 12));
    assert_eq!(session.scrollback(), 0);
    session.set_scrollback(1);
    assert_eq!(session.scrollback(), 1);
    session.set_scrollback(0);
}

#[cfg(unix)]
#[test]
fn kill_stops_a_long_running_job() {
    let mut session = shell("sleep 60");
    std::thread::sleep(Duration::from_millis(150));
    session.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.exit_code().is_none() && Instant::now() < deadline {
        session.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.exit_code().is_some(), "process should have exited");
}

#[cfg(unix)]
#[test]
fn ansi_color_sequences_survive_pump() {
    let mut session = shell(
        "printf '\\033[31mR\\033[0m\\033[48;5;22mG\\033[0m\\033[38;2;10;20;30mX\\033[0m\\n'; sleep 0.1",
    );
    assert!(wait_for(&mut session, "RGX", Duration::from_secs(5)));
    let screen = session.screen();
    let red = screen.cell(0, 0).expect("red cell");
    assert_eq!(red.contents(), "R");
    assert_eq!(red.fgcolor(), vt100::Color::Idx(1));
    let green_bg = screen.cell(0, 1).expect("green-bg cell");
    assert_eq!(green_bg.bgcolor(), vt100::Color::Idx(22));
    let truecolor = screen.cell(0, 2).expect("rgb cell");
    assert_eq!(truecolor.fgcolor(), vt100::Color::Rgb(10, 20, 30));
    assert_eq!(screen.cell(0, 3).expect("reset cell").fgcolor(), vt100::Color::Default);
}

#[test]
fn term_row_maps_palette_and_attributes() {
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(4).unwrap(),
        std::num::NonZeroU16::new(40).unwrap(),
        100,
    );
    parser.process(b"\x1b[31mR\x1b[1m\x1b[4mB\x1b[0m plain\n");
    let theme_fg: gpui::Hsla = gpui::rgb(0xe6e6e6).into();
    let theme_bg: gpui::Hsla = gpui::rgb(0x0e1014).into();
    let row = spritx_terminal::terminal::term_row(parser.screen(), 0, theme_fg, theme_bg);
    let palette_red: gpui::Hsla =
        gpui::rgb(spritx_terminal::terminal::ANSI_PALETTE[1]).into();
    assert_eq!(row.spans[0].colors.fg, palette_red);
    assert!(!row.spans[0].colors.bold);
    let bold = row
        .spans
        .iter()
        .find(|s| s.text.contains('B'))
        .expect("bold span");
    assert!(bold.colors.bold);
    assert!(bold.colors.underline);
    let run = spritx_terminal::terminal::span_run(bold);
    assert_eq!(run.font.weight, gpui::FontWeight::BOLD);
    assert!(run.underline.is_some());
    // Text of the row reproduces the visible line.
    let text: String = row.spans.iter().map(|s| s.text.as_str()).collect();
    assert!(text.starts_with("RB"), "row text was: {text:?}");
}

#[cfg(unix)]
#[test]
fn resize_updates_size_and_scrollback_rendering() {
    let mut session = shell("seq 1 60; sleep 0.2");
    assert!(wait_for(&mut session, "60", Duration::from_secs(5)));
    session.resize(40, 12).expect("resize");
    assert_eq!(session.size(), (40, 12));
    let (rows, cols) = session.screen().size();
    assert_eq!((rows.get(), cols.get()), (12, 40));

    assert!(session.scrollback_len() > 0, "scrollback should hold the pushed-off lines");
    let theme_fg: gpui::Hsla = gpui::rgb(0xe6e6e6).into();
    let theme_bg: gpui::Hsla = gpui::rgb(0x0e1014).into();

    // The last non-empty row of the live screen shows the tail of `seq`.
    let mut bottom_text = String::new();
    for row in (0..rows.get()).rev() {
        let term_row =
            spritx_terminal::terminal::term_row(session.screen(), row, theme_fg, theme_bg);
        bottom_text = term_row.spans.iter().map(|s| s.text.as_str()).collect();
        if !bottom_text.trim().is_empty() {
            break;
        }
    }
    assert!(bottom_text.contains('6'), "bottom row: {bottom_text:?}");

    // Scroll up 10 lines: the viewport must show earlier history, and the
    // cursor must stay pinned to the live position relative to the view.
    let cursor_live = session.cursor();
    assert!(session.scroll_lines(-10), "scroll up should change the offset");
    assert_eq!(session.scrollback(), 10);
    let top = spritx_terminal::terminal::term_row(session.screen(), 0, theme_fg, theme_bg);
    let top_text: String = top.spans.iter().map(|s| s.text.as_str()).collect();
    let top_num: i32 = top_text
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("999")
        .parse()
        .unwrap_or(999);
    assert!(
        top_num >= 1 && top_num < 47,
        "scrolled top row should be earlier history, was {top_text:?}"
    );
    assert!(session.visible_cursor().is_none(), "the live cursor is below the historical viewport");

    // Scrolling back to the bottom restores the live view; clamping must
    // never exceed the available history.
    assert!(session.scroll_lines(10));
    assert_eq!(session.scrollback(), 0);
    assert_eq!(session.visible_cursor().unwrap().0, cursor_live.0);
    session.scroll_lines(100_000);
    assert_eq!(session.scrollback(), 0);
}

#[cfg(unix)]
#[test]
fn alternate_screen_suppresses_scrolling() {
    let mut session = shell("printf '\\033[?1049h'; sleep 0.1");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.alternate_screen() && Instant::now() < deadline {
        session.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.alternate_screen());
    assert!(!session.scroll_lines(-5), "wheel scrolling is ignored in alternate screen");
    assert_eq!(session.scrollback(), 0);
}

#[cfg(unix)]
#[test]
fn mouse_reporting_is_detected_and_forwarded_to_the_child() {
    use spritx_terminal::terminal::{MouseAction, MouseMods, mouse_button};

    // Pi's fullscreen setup: VT200 + button-motion + SGR encoding. A real TUI
    // also puts the terminal in raw mode, which the test mirrors so the child
    // reads the report instead of the line discipline echoing it.
    let mut session = shell(
        "stty raw -echo; printf '\\033[?1000h\\033[?1002h\\033[?1006h'; head -c 10 | od -An -tx1",
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.wants_mouse() && Instant::now() < deadline {
        session.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(session.wants_mouse(), "child never enabled mouse reporting");

    // Wheel and press are consumed by the child, not the local viewport.
    let sent = session.report_mouse(
        9,
        4,
        MouseAction::Press {
            button: mouse_button::LEFT,
        },
        MouseMods::default(),
    );
    assert!(sent, "press should be written to the child");
    // ESC [ < 0 ; 1 0 ; 5 M
    assert!(
        wait_for(&mut session, "1b 5b 3c 30 3b 31 30 3b 35 4d", Duration::from_secs(5)),
        "screen was: {}",
        session.screen_text()
    );
}
