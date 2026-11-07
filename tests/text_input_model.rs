//! Integration coverage for the public `TextInputStyle` surface.
//! UTF-16 <-> UTF-8 boundary regression coverage lives in the in-module
//! tests in `src/components/text_input.rs` (`cargo test --lib`), which can
//! call the private free helpers directly; integration tests cannot construct
//! a `Window`-gated `TextInput`.

use gpui::px;
use spritx_terminal::components::text_input::TextInputStyle;
use spritx_terminal::theme::{Theme, MONO_FONT};

#[test]
fn style_from_dark_theme_has_expected_geometry_and_palette() {
    let theme = Theme::dark();
    let style = TextInputStyle::from_theme(&theme);

    assert_eq!(style.text_color, theme.fg);
    assert_eq!(style.placeholder_color, theme.fg_dim);
    assert_eq!(style.background, theme.bg_soft);
    assert_eq!(style.border, theme.border);
    assert_eq!(style.focus_border, theme.accent);
    assert_eq!(style.cursor, theme.accent);
    assert_eq!(style.font_family.as_ref(), MONO_FONT);
    assert_eq!(style.font_size, px(12.5));
    assert_eq!(style.height, px(28.0));
    assert_eq!(style.padding_x, px(9.0));
    assert_eq!(style.radius, px(6.0));
    assert_eq!(style.width, None);
    assert_eq!(style.selection.a, theme.accent.a * 0.28);
}
