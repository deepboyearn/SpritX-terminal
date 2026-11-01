//! Stateless dropdown: a trigger row plus a deferred popover list.
//!
//! The caller owns the `open` flag and the selected index; the helper renders
//! the trigger, and (when `open`) an absolutely positioned option list that is
//! painted above sibling elements via [`gpui::deferred`]. This mirrors the
//! custom `<select>` replacement required by the settings screen
//! (`.settings-input` + chevron, radius 6, popover radius 8).

use std::rc::Rc;

use gpui::{
    deferred, div, prelude::*, px, ClickEvent, ElementId, IntoElement, SharedString, Window, App,
};

use crate::components::icons::{icon, Icon};
use crate::theme::Theme;

const TRIGGER_HEIGHT: f32 = 28.0;

/// Render a select trigger with an optional open popover.
///
/// `label` is the trigger text (usually the currently selected value);
/// `items` are the option labels; `selected` highlights one row; `open`
/// controls whether the popover is rendered. `on_toggle` fires on the trigger
/// and `on_select` receives the clicked option index.
pub fn dropdown<I>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    items: &[I],
    selected: usize,
    open: bool,
    theme: &Theme,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_select: impl Fn(usize, &ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement
where
    I: Into<SharedString> + Clone,
{
    let on_select = Rc::new(on_select);
    let mut list = div()
        .id("dropdown-list")
        .flex()
        .flex_col()
        .p(px(4.))
        .w_full()
        .max_h(px(240.))
        .overflow_y_scroll()
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .occlude();

    for (index, item) in items.iter().enumerate() {
        let is_selected = index == selected;
        let select = on_select.clone();
        list = list.child(
            div()
                .id(("dropdown-item", index))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if is_selected { theme.accent } else { theme.fg })
                .when(is_selected, |row| row.bg(theme.accent.opacity(0.12)))
                .cursor_pointer()
                .hover(|style| style.bg(theme.white_06))
                .on_click(move |event, window, cx| {
                    select(index, event, window, cx);
                })
                .child(item.clone().into())
                .when(is_selected, |row| {
                    row.child(icon(Icon::Check, 12.0, theme.accent))
                }),
        );
    }

    let list: Option<gpui::AnyElement> = open.then(|| {
        deferred(
            div()
                .absolute()
                .top(px(TRIGGER_HEIGHT + 4.))
                .left_0()
                .right_0()
                .child(list),
        )
        .into_any_element()
    });

    div()
        .relative()
        .w_full()
        .child(
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(TRIGGER_HEIGHT))
                .pl(px(9.))
                .pr(px(8.))
                .rounded(px(6.))
                .bg(theme.bg_soft)
                .border_1()
                .border_color(if open { theme.accent } else { theme.border })
                .text_size(px(12.5))
                .text_color(theme.fg)
                .cursor_pointer()
                .hover(|style| style.bg(theme.white_04))
                .on_click(on_toggle)
                .child(div().flex_1().overflow_hidden().text_ellipsis().child(label.into()))
                .child(icon(
                    if open {
                        Icon::ChevronUp
                    } else {
                        Icon::ChevronDown
                    },
                    12.0,
                    theme.fg_dim,
                )),
        )
        .children(list)
}
