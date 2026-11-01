use gpui::{
    Div, ElementId, FontWeight, Hsla, Stateful, div, prelude::*, px,
};

use crate::components::icons::{Icon, icon};
use crate::theme::Theme;

pub fn icon_button(
    id: impl Into<ElementId>,
    ic: Icon,
    theme: &Theme,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(30.))
        .h(px(26.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(if active { theme.accent } else { theme.fg_dim })
        .when(active, |d| d.bg(theme.accent.opacity(0.12)))
        .hover(|s| s.bg(theme.white_a(0.07)).text_color(theme.fg))
        .child(icon(ic, 16.0, if active { theme.accent } else { theme.fg_dim }))
}

pub fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<String>,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px(px(10.))
        .rounded(px(5.))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.))
        .text_color(theme.fg_dim)
        .cursor_pointer()
        .hover(|s| s.text_color(theme.fg).bg(theme.white_04))
        .child(label.into())
}

pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<String>,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(28.))
        .px(px(12.))
        .rounded(px(6.))
        .bg(theme.accent)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.on_accent)
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.88)))
        .child(label.into())
}

pub fn danger_button(
    id: impl Into<ElementId>,
    label: impl Into<String>,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(28.))
        .px(px(12.))
        .rounded(px(6.))
        .bg(theme.danger)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.on_accent)
        .cursor_pointer()
        .hover(|s| s.bg(theme.danger.opacity(0.88)))
        .child(label.into())
}

pub fn divider(theme: &Theme) -> Div {
    div().w(px(1.)).h(px(16.)).mx(px(4.)).bg(theme.border)
}

pub fn dot(color: Hsla, size: f32) -> Div {
    div().size(px(size)).rounded_full().bg(color)
}
