//! The terminal grid must honour the configured appearance (Settings →
//! Appearance). These run in one process against the process-global
//! `terminal::appearance`, so keep them in a single test to avoid races.

use spritx_terminal::terminal::{self, CursorShape, TerminalAppearance};

#[test]
fn appearance_is_configurable_and_scales_line_height() {
    // The default font size is 14px with a blinking block caret.
    let default = TerminalAppearance::default();
    assert_eq!(default.font_size, 14.0);
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

    let pastel = spritx_terminal::theme_presets::PRESETS[0].theme();
    terminal::set_term_theme(pastel);
    terminal::set_light_workspace(true);
    assert!(terminal::term_theme().background.l > 0.9);
    assert!(terminal::term_theme().foreground.l < 0.2);
    assert_eq!(terminal::configured_term_theme().background, pastel.background);

    terminal::set_light_workspace(false);
    assert_eq!(terminal::term_theme().background, pastel.background);

    let mut custom_light = pastel;
    custom_light.background = gpui::rgb(0xf8fdf9).into();
    terminal::set_term_theme(custom_light);
    assert!(terminal::term_theme().background.l < 0.5);
    terminal::set_light_workspace(true);
    assert_eq!(terminal::term_theme().background, custom_light.background);

    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(4).unwrap(),
        std::num::NonZeroU16::new(40).unwrap(),
        0,
    );
    parser.process(
        "\x1b[38;2;250;179;135m\x1b[48;2;250;179;135;38;2;30;30;46m folder \x1b[0m\r\n\x1b[48;2;250;179;135mordinary output\x1b[0m"
            .as_bytes(),
    );
    terminal::set_light_workspace(false);
    terminal::set_term_theme(pastel);
    let original = terminal::term_row(parser.screen(), 0, pastel.foreground, pastel.background);
    let light = spritx_terminal::theme_presets::SPRITX_LIGHT.theme();
    terminal::set_light_workspace(true);
    terminal::set_term_theme(light);
    let recolored = terminal::term_row(parser.screen(), 0, light.foreground, light.background);
    assert_ne!(original.spans[1].colors.bg, recolored.spans[1].colors.bg);
    assert_eq!(recolored.spans[0].colors.fg, light.ansi[3]);
    assert_eq!(recolored.spans[1].colors.bg, Some(light.ansi[3]));
    let output = terminal::term_row(parser.screen(), 1, light.foreground, light.background);
    assert_eq!(output.spans[0].colors.bg, original.spans[1].colors.bg);
}
