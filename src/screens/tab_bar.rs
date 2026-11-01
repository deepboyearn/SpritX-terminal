use gpui::{
    AnyElement, Context, ElementId, FontWeight, IntoElement, MouseButton, SharedString, div,
    prelude::*, px,
};

use crate::components::buttons::{divider, dot, icon_button};
use crate::components::icons::{Icon, icon};
use crate::screens::app_shell::{SpritXApp, TabMenuState};
use crate::state::SplitDirection;
use crate::theme::{MONO_FONT, Theme, tab_bar_h};

pub fn render(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(tab_bar_h())
        .px(px(4.))
        .gap(px(2.))
        .bg(theme.bg_soft)
        .border_b_1()
        .border_color(theme.border)
        .child(render_lead(app, cx))
        .child(render_tabs(app, cx))
        .child(render_new_group(app, cx))
        .child(render_actions(app, cx))
        .into_any_element()
}

fn render_lead(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    div()
        .flex()
        .flex_none()
        .items_center()
        .pr(px(4.))
        .child(
            icon_button("tb-file-tree", Icon::Folder, &theme, app.file_tree_visible)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.file_tree_visible = !this.file_tree_visible;
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_tabs(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let offset = app.tab_scroll.offset().x;
    let max_offset = app.tab_scroll.max_offset().width;
    let can_scroll_left = offset > px(1.0);
    let can_scroll_right = offset + px(1.0) < max_offset;

    let mut strip = div()
        .id("tab-strip")
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(px(2.))
        .overflow_x_scroll()
        .scrollbar_width(px(0.))
        .track_scroll(&app.tab_scroll)
        .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
            let delta = event.delta.pixel_delta(px(20.));
            let max = this.tab_scroll.max_offset().width;
            let mut offset = this.tab_scroll.offset();
            offset.x = (offset.x - delta.y).clamp(px(0.0), max);
            this.tab_scroll.set_offset(offset);
            cx.stop_propagation();
            cx.notify();
        }));

    for (index, tab) in app.tabs.iter().enumerate() {
        let active = index == app.active_tab;
        let panes = tab.pane_count();
        let tab_id = tab.id;
        let title = tab.title.clone();
        let editing = app.editing_tab == Some(tab_id);
        let editing_title = app.editing_title.clone();
        let branch = tab
            .active_leaf_state()
            .map(|leaf| leaf.branch.clone())
            .unwrap_or_default();
        let remote = tab
            .active_leaf_state()
            .map(|leaf| leaf.remote)
            .unwrap_or(false);

        let mut tab_el = div()
            .id(("tab", tab_id as usize))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .pl(px(10.))
            .pr(px(8.))
            .mt(px(4.))
            .min_w(px(120.))
            .max_w(px(220.))
            .h_full()
            .rounded_t(px(6.))
            .border_1()
            .border_b_0()
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(if active { theme.fg } else { theme.fg_dim })
            .when(active, |d| d.bg(theme.bg).border_color(theme.border))
            .when(!active, |d| d.border_color(gpui::transparent_black()))
            .when(!active, |d| {
                d.hover(|s| s.bg(theme.white_04).text_color(theme.fg))
            })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.begin_rename(index, cx);
                } else {
                    this.activate_tab(index, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_tab_menu(
                        index,
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.close_tab(index, cx);
                }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(theme.fg_dim)
                    .bg(theme.white_06)
                    .rounded(px(3.))
                    .px(px(4.))
                    .py(px(1.))
                    .child(format!("{}", index + 1)),
            );

        if remote {
            tab_el = tab_el.child(icon(Icon::Remote, 11.0, theme.accent));
        }

        if editing {
            tab_el = tab_el.child(
                div()
                    .flex_1()
                    .min_w(px(40.))
                    .flex()
                    .items_center()
                    .h(px(20.))
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(theme.bg)
                    .border_1()
                    .border_color(theme.accent)
                    .text_size(px(12.5))
                    .text_color(theme.fg)
                    .child(editing_title)
                    .child(div().w(px(1.)).h(px(13.)).bg(theme.accent)),
            );
        } else {
            tab_el = tab_el.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(title),
            );
        }

        if !branch.is_empty() && !editing {
            let branch_color = if active { theme.accent } else { theme.fg_dim };
            tab_el = tab_el.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(3.))
                    .max_w(px(110.))
                    .overflow_hidden()
                    .font_family(MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(branch_color)
                    .opacity(0.85)
                    .child(icon(Icon::Branch, 10.0, branch_color))
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(branch),
                    ),
            );
        }

        if panes > 1 && !editing {
            tab_el = tab_el.child(
                div()
                    .flex_none()
                    .ml(px(4.))
                    .text_size(px(10.))
                    .text_color(theme.fg_dim)
                    .child(format!("⊞ {panes}")),
            );
        }

        if !editing {
            tab_el = tab_el.child(
                div()
                    .id(("tab-close", tab_id as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(2.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(theme.fg_dim)
                    .hover(|s| s.bg(theme.white_08).text_color(theme.fg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(index, cx);
                    }))
                    .child(icon(Icon::X, 11.0, theme.fg_dim)),
            );
        }

        if active {
            tab_el = tab_el.child(
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .right(px(0.))
                    .flex()
                    .justify_center()
                    .child(div().w(px(28.)).h(px(2.)).rounded(px(1.)).bg(theme.accent)),
            );
        }

        strip = strip.child(tab_el);
    }

    let mut wrap = div()
        .flex()
        .flex_1()
        .min_w_0()
        .relative()
        .child(strip);

    if can_scroll_left {
        wrap = wrap.child(scroll_button(app, true, cx));
    }
    if can_scroll_right {
        wrap = wrap.child(scroll_button(app, false, cx));
    }

    wrap.into_any_element()
}

fn scroll_button(app: &SpritXApp, left: bool, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let mut button = div()
        .id(if left {
            "tab-scroll-left"
        } else {
            "tab-scroll-right"
        })
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.bg_soft)
        .cursor_pointer()
        .text_color(theme.fg_dim)
        .hover(|s| s.text_color(theme.accent).bg(theme.white_06));
    button = if left {
        button.left_0().border_r_1()
    } else {
        button.right_0().border_l_1()
    };
    button
        .border_color(theme.border)
        .on_click(cx.listener(move |this, _, _, cx| {
            let max = this.tab_scroll.max_offset().width;
            let mut offset = this.tab_scroll.offset();
            offset.x = if left {
                (offset.x - px(220.0)).max(px(0.0))
            } else {
                (offset.x + px(220.0)).min(max)
            };
            this.tab_scroll.set_offset(offset);
            cx.notify();
        }))
        .child(icon(
            if left {
                Icon::ChevronLeft
            } else {
                Icon::ChevronRight
            },
            14.0,
            theme.fg_dim,
        ))
        .into_any_element()
}

fn render_new_group(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    div()
        .flex()
        .flex_none()
        .items_center()
        .pr(px(6.))
        .child(
            div()
                .id("tb-new-tab")
                .flex()
                .items_center()
                .justify_center()
                .mt(px(4.))
                .ml(px(4.))
                .w(px(24.))
                .h(px(24.))
                .rounded_tl(px(4.))
                .rounded_bl(px(4.))
                .text_color(theme.fg_dim)
                .cursor_pointer()
                .hover(|s| s.bg(theme.white_08).text_color(theme.fg))
                .on_click(cx.listener(|this, _, _, cx| this.add_tab(cx)))
                .child(icon(Icon::Plus, 14.0, theme.fg_dim)),
        )
        .into_any_element()
}

fn render_actions(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let remote = app.remote_status();
    let remote_color = if remote.running {
        theme.accent
    } else {
        theme.fg_dim
    };

    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .pl(px(6.))
        .child(
            div()
                .id("tb-remote")
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(remote_color)
                .when(app.remote_menu_open, |d| {
                    d.bg(theme.white_a(0.07)).text_color(theme.fg)
                })
                .hover(|s| s.bg(theme.white_a(0.07)).text_color(theme.fg))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.remote_menu_open = !this.remote_menu_open;
                    this.remote_menu_at = None;
                    this.layouts_open = false;
                    this.tab_menu = None;
                    this.pane_menu = None;
                    cx.notify();
                }))
                .child(icon(Icon::Smartphone, 16.0, remote_color))
                .when(remote.running, |d| {
                    d.child(dot(
                        if remote.clients > 0 {
                            theme.success
                        } else {
                            theme.fg_dim
                        },
                        7.0,
                    ))
                })
                .when(remote.clients > 0, |d| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{}", remote.clients)),
                    )
                }),
        )
        .child(divider(&theme))
        .child(
            icon_button("tb-layouts", Icon::Layouts, &theme, app.layouts_open).on_click(
                cx.listener(|this, _, _, cx| {
                    this.layouts_open = !this.layouts_open;
                    this.remote_menu_open = false;
                    this.tab_menu = None;
                    this.pane_menu = None;
                    cx.notify();
                }),
            ),
        )
        .child(divider(&theme))
        .child(
            icon_button("tb-settings", Icon::Settings, &theme, false).on_click(cx.listener(
                |this, _, _, cx| this.open_settings(cx),
            )),
        )
        .into_any_element()
}

/// Tab context menu (C47–C52), rendered on the root overlay layer.
pub fn render_context_menu(
    app: &SpritXApp,
    menu: &TabMenuState,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let theme = app.theme;
    let index = menu.index;
    let panes = app.tabs.get(index).map(|tab| tab.pane_count()).unwrap_or(1);
    let close_label = if panes > 1 {
        format!("Close tab · {panes} panes")
    } else {
        "Close tab".to_string()
    };

    div()
        .id("tab-context-menu")
        .absolute()
        .left(px(menu.x.min(880.0)))
        .top(px(menu.y))
        .w(px(220.))
        .flex()
        .flex_col()
        .p(px(4.))
        .rounded(px(6.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
            cx.stop_propagation();
        }))
        .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
            cx.stop_propagation();
        }))
        .child(ctx_row(
            "tab-ctx-rename",
            "Rename",
            Icon::Pencil,
            false,
            theme,
            cx,
            move |this, cx| this.begin_rename(index, cx),
        ))
        .child(ctx_row(
            "tab-ctx-split-h",
            "Split right",
            Icon::SplitH,
            false,
            theme,
            cx,
            move |this, cx| this.split_tab(index, SplitDirection::Row, cx),
        ))
        .child(ctx_row(
            "tab-ctx-split-v",
            "Split down",
            Icon::SplitV,
            false,
            theme,
            cx,
            move |this, cx| this.split_tab(index, SplitDirection::Column, cx),
        ))
        .child(div().h(px(1.)).my(px(4.)).bg(theme.border))
        .child(ctx_row(
            "tab-ctx-new",
            "New tab",
            Icon::Plus,
            false,
            theme,
            cx,
            |this, cx| this.add_tab(cx),
        ))
        .child(ctx_row(
            "tab-ctx-close",
            close_label,
            Icon::X,
            true,
            theme,
            cx,
            move |this, cx| this.close_tab(index, cx),
        ))
        .into_any_element()
}

fn ctx_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    ic: Icon,
    danger: bool,
    theme: Theme,
    cx: &mut Context<SpritXApp>,
    action: impl Fn(&mut SpritXApp, &mut Context<SpritXApp>) + 'static,
) -> AnyElement {
    let label: SharedString = label.into();
    let color = if danger { theme.danger_soft } else { theme.fg };
    let icon_color = if danger {
        theme.danger_soft
    } else {
        theme.fg_dim
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .py(px(6.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(color)
        .hover(move |s| {
            if danger {
                s.bg(theme.danger_a(0.12)).text_color(theme.danger)
            } else {
                s.bg(theme.white_06)
            }
        })
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(icon(ic, 14.0, icon_color))
        .child(label)
        .into_any_element()
}

pub fn tab_close_button(theme: &Theme) -> AnyElement {
    crate::components::buttons::text_button("close", "Close", theme).into_any_element()
}
