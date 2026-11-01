//! Stateless helpers for SpritX's shared context menus
//! (`.ctx-item` / `.ctx-sep`): a floating menu container plus row and
//! separator builders. Position the container yourself (absolute/deferred)
//! at the pointer or anchor.

use gpui::{
    div, prelude::*, px, ClickEvent, Div, ElementId, SharedString, Stateful, Window, App,
};

use crate::components::icons::{icon, Icon};
use crate::theme::Theme;

/// Menu container: 8px radius, `--bg` surface, 1px border, large shadow.
pub fn context_menu(id: impl Into<ElementId>, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .min_w(px(180.))
        .p(px(4.))
        .rounded(px(6.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .occlude()
}

/// One menu row: optional 14px icon, label, optional right-aligned shortcut,
/// and a danger color variant.
pub fn context_menu_item(
    id: impl Into<ElementId>,
    icon_kind: Option<Icon>,
    label: impl Into<SharedString>,
    shortcut: Option<SharedString>,
    danger: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let text_color = if danger { theme.danger_soft } else { theme.fg };
    let icon_color = if danger { theme.danger_soft } else { theme.fg_dim };

    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(8.))
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(text_color)
        .cursor_pointer()
        .hover(move |style| {
            style.bg(if danger {
                theme.danger.opacity(0.12)
            } else {
                theme.white_06
            })
        })
        .on_click(on_click)
        .children(icon_kind.map(|kind| icon(kind, 14.0, icon_color)))
        .child(div().flex_1().overflow_hidden().text_ellipsis().child(label.into()))
        .children(
            shortcut.map(|hint| {
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(theme.fg_dim)
                    .child(hint)
            }),
        )
}

/// 1px separator between menu groups.
pub fn context_menu_separator(theme: &Theme) -> Div {
    div().h(px(1.)).my(px(4.)).bg(theme.border)
}
