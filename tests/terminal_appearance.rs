//! The terminal grid must honour the configured appearance (Settings →
//! Appearance). These run in one process against the process-global
//! `terminal::appearance`, so keep them in a single test to avoid races.

use spritx_terminal::terminal::{self, CursorShape, TerminalAppearance};

#[test]
fn appearance_is_configurable_and_scales_line_height() {
    // Defaults are the previous fixed values (13px / 19px line, block caret).
    let default = TerminalAppearance::default();
    assert_eq!(default.font_size, 13.0);
    assert_eq!(default.cursor_shape, CursorShape::Block);
    assert!(default.cursor_blink);
    assert_eq!(default.scrollback, terminal::DEFAULT_SCROLLBACK);

    terminal::set_appearance(TerminalAppearance {
        font_family: "Test Mono".into(),
        font_size: 20.0,
        scrollback: 1234,
        cursor_shape: CursorShape::Bar,
        cursor_blink: false,
    });

    let appearance = terminal::appearance();
    assert_eq!(appearance.font_family.to_string(), "Test Mono");
    assert_eq!(appearance.font_size, 20.0);
    assert_eq!(appearance.scrollback, 1234);
    assert_eq!(appearance.cursor_shape, CursorShape::Bar);
    assert!(!appearance.cursor_blink);

    assert_eq!(terminal::term_font_size(), 20.0);
    // Line height keeps the 19 / 13 ratio, so it must grow with the font size.
    assert!(terminal::term_line_height() > 19.0);
    assert!((terminal::term_line_height() - 20.0 * 19.0 / 13.0).abs() < 1.0);
}
