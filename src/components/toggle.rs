//! Toggle / switch widgets matching SpritX's `.settings-toggle` (38×22 track,
//! 16px white knob, 140ms slide in the web app).

use gpui::{div, prelude::*, px, ClickEvent, Div, ElementId, Stateful, Window, App};

use crate::theme::Theme;

/// Purely visual switch: 38×22 track with a 16px knob.
///
/// Use this when the surrounding row already handles the click (e.g. a whole
/// settings row), otherwise prefer [`switch`].
pub fn toggle(theme: &Theme, on: bool) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .w(px(38.))
        .h(px(22.))
        .p(px(2.))
        .rounded(px(11.))
        .bg(if on { theme.accent } else { theme.bg_soft })
        .border_1()
        .border_color(if on { theme.accent } else { theme.border })
        .child(
            div()
                .flex_none()
                .size(px(16.))
                .rounded_full()
                .bg(theme.on_accent)
                .shadow_sm()
                .when(on, |knob| knob.ml(px(16.))),
        )
}

/// Interactive switch. Stateless: `on` is supplied by the caller and the
/// returned element merely reports clicks through `on_click`.
pub fn switch(
    id: impl Into<ElementId>,
    theme: &Theme,
    on: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    toggle(theme, on)
        .id(id)
        .cursor_pointer()
        .hover(|style| {
            style.border_color(if on {
                theme.accent.opacity(0.85)
            } else {
                theme.white_16
            })
        })
        .on_click(on_click)
}
