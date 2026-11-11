use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Bounds, Context, ElementId, FontWeight, IntoElement, MouseButton, Pixels,
    SharedString, canvas, div, prelude::*, px,
};

use crate::components::buttons::{divider, dot, icon_button};
use crate::components::icons::{Icon, icon};
use crate::components::thinking_orb::thinking_orb;
use crate::screens::app_shell::{SpritXApp, TabMenuState};
use crate::state::SplitDirection;
use crate::theme::{Theme, tab_bar_h};

const TAB_SLIDE_DURATION: Duration = Duration::from_millis(210);

pub(super) struct TabHighlight {
    tab_id: u64,
    from: Bounds<Pixels>,
    target: Bounds<Pixels>,
    started: Instant,
}

impl TabHighlight {
    fn position(&self, now: Instant) -> Bounds<Pixels> {
        let progress = (now.duration_since(self.started).as_secs_f32()
            / TAB_SLIDE_DURATION.as_secs_f32()).min(1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        let mut bounds = self.target;
        bounds.origin.x = self.from.origin.x + (self.target.origin.x - self.from.origin.x) * eased;
        bounds.size.width = self.from.size.width + (self.target.size.width - self.from.size.width) * eased;
        bounds
    }
}

pub fn render(app: &SpritXApp, window_active: bool, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    div()
        .flex()
        .flex_none()
        .items_end()
        .h(tab_bar_h())
        .px(px(8.))
        .gap(px(6.))
        .bg(theme.outer)
        .when(!app.file_tree_visible, |bar| bar.child(render_lead(app, cx)))
        .child(render_tabs(app, window_active, cx))
        .child(div().pb(px(4.)).child(render_new_group(app, cx)))
        .child(div().flex_1())
        .child(div().pb(px(5.)).child(render_actions(app, cx)))
        .into_any_element()
}

fn render_lead(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    div()
        .flex()
        .flex_none()
        .items_center()
        .pr(px(4.))
        .pb(px(5.))
        .child(
            icon_button("tb-file-tree", Icon::Sidebar, &theme, false)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.file_tree_visible = true;
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_tabs(app: &SpritXApp, _window_active: bool, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let selected_fill = theme.bg;
    let offset = app.tab_scroll.offset().x;
    let max_offset = app.tab_scroll.max_offset().width;
    let can_scroll_left = offset > px(1.0);
    // An empty strip renders nothing: `active_tab()` below would otherwise
    // be read unconditionally off an index that cannot exist here.
    if app.tabs.is_empty() {
        return div()
            .id("tab-strip")
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(10.))
            .p(px(3.))
            .into_any_element();
    }
    let can_scroll_right = offset + px(1.0) < max_offset;
    // Defensive clamp: a stale `active_tab` must not desync which tab is
    // drawn as selected from the one `active_tab()` resolves to.
    let active_index = app.active_tab.min(app.tabs.len().saturating_sub(1));
    let mut strip = div()
        .id("tab-strip")
        .flex()
        .min_w_0()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .h(px(36.))
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
        let active = index == active_index;
        let panes = tab.pane_count();
        let tab_id = tab.id;
        let title = if tab.title.is_empty() {
            "Terminal".to_string()
        } else {
            tab.display_title().to_string()
        };
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
        let mobile = app.mobile_client_panes().iter().any(|leaf| tab.leaves.contains_key(leaf));
        let process_running = tab.leaves.values().any(|leaf| leaf.activity.running);
        let now = Instant::now();
        let running = tab.leaves.values().any(|leaf| leaf.activity.running
            && app.cli_activity.get(&leaf.id).is_some_and(|activity| activity.active(now)));
        let completed = tab.leaves.values()
            .filter(|leaf| leaf.activity.exit_code.is_some())
            .max_by_key(|leaf| leaf.activity.completed)
            .and_then(|leaf| leaf.activity.exit_code);
        // A leaf with no live session and not running is dead: surface it
        // distinctly instead of looking like a healthy idle tab.
        let dead = !process_running
            && tab
                .leaves
                .values()
                .any(|leaf| !app.sessions.contains_key(&leaf.id));

        let mut tab_el = div()
            .id(("tab", tab_id as usize))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .pl(px(12.))
            .pr(px(8.))
            .min_w(px(132.))
            .max_w(px(204.))
            .h(px(36.))
            .rounded(px(10.))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(if active { theme.fg } else { theme.fg_dim })
            .when(active, |d| d.font_weight(FontWeight::MEDIUM))
            .when(!active, |d| {
                // The inset rounded surface is painted by the strip canvas.
                // A background here would paint a second, taller hover box.
                d.hover(|s| s.text_color(theme.fg))
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
            );

        let mut leading_icon = div().flex_none().w(px(20.)).h(px(20.))
            .flex().items_center().justify_center();
        leading_icon = if running {
            leading_icon.child(thinking_orb(theme.bg.l < 0.5, app.settings.reduced_motion))
        } else {
            leading_icon.child(icon(
                if remote { Icon::Remote } else { Icon::Ssh },
                14.0,
                if active { theme.fg } else { theme.fg_dim },
            ))
        };
        tab_el = tab_el.child(leading_icon);

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
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(title),
                    ),
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
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(3.))
                    .text_size(px(10.))
                    .text_color(theme.fg_dim)
                    .child(icon(Icon::Layouts, 11.0, theme.fg_dim))
                    .child(panes.to_string()),
            );
        }

        if !editing {
            if mobile {
                tab_el = tab_el.child(div().flex_none().child(icon(Icon::Smartphone, 12.0, theme.accent)));
            }
            if let Some(exit_code) = completed.filter(|_| !process_running) {
                let (label, color) = if exit_code == 0 { ("Done", theme.success) } else { ("Failed", theme.danger) };
                tab_el = tab_el.child(div().flex_none().text_size(px(10.)).text_color(color).child(label));
            } else if dead {
                tab_el = tab_el.child(div().flex_none().text_size(px(10.)).text_color(theme.danger).child("Dead"));
            }
            tab_el = tab_el.child(
                div()
                    .id(("tab-close", tab_id as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .w(px(20.))
                    .h(px(20.))
                    .rounded(px(6.))
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

        strip = strip.child(tab_el);
    }

    let scroll = app.tab_scroll.clone();
    let highlight = app.tab_highlight.clone();
    let selected_index = active_index;
    let tab_count = app.tabs.len();
    // `active_tab_checked` is the total read path; the empty case returned
    // above, so this resolves to a real tab.
    let selected_id = app.active_tab_checked().map_or(0, |tab| tab.id);
    if highlight.borrow().as_ref().is_none_or(|pill| pill.tab_id != selected_id) {
        scroll.scroll_to_item(selected_index);
    }
    let sliding_pill = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let offset = scroll.offset();
            // All labels share one baseline. Inactive surfaces are inset
            // inside the same slots rather than moving the tab contents up.
            for index in 0..tab_count.min(50) {
                if index == selected_index { continue; }
                if let Some(mut surface) = scroll.bounds_for_item(index) {
                    surface.origin += offset;
                    surface.origin.y += px(4.);
                    surface.size.height -= px(8.);
                    window.paint_quad(gpui::quad(
                        surface,
                        gpui::Corners::all(px(8.)),
                        theme.white_04,
                        gpui::Edges::all(px(0.)),
                        gpui::transparent_black(),
                        gpui::BorderStyle::Solid,
                    ));
                }
            }
            let Some(mut target) = scroll.bounds_for_item(selected_index) else {
                return;
            };
            target.origin -= bounds.origin;
            let now = Instant::now();
            let mut state = highlight.borrow_mut();
            let pill = state.get_or_insert_with(|| TabHighlight {
                tab_id: selected_id,
                from: target,
                target,
                started: now - TAB_SLIDE_DURATION,
            });
            if pill.tab_id != selected_id {
                pill.from = pill.position(now);
                pill.started = now;
                pill.tab_id = selected_id;
            }
            pill.target = target;
            let mut position = pill.position(now);
            position.origin += bounds.origin + offset;
            window.paint_quad(gpui::quad(
                position,
                gpui::Corners::all(px(10.)),
                selected_fill,
                gpui::Edges::all(px(0.)),
                gpui::transparent_black(),
                gpui::BorderStyle::Solid,
            ));
            // Continue the rounded tab into its content surface. Paint this
            // with the moving highlight so the connection follows selection.
            let mut bridge = position;
            bridge.origin.y += bridge.size.height - px(12.);
            bridge.size.height = px(12.);
            window.paint_quad(gpui::quad(
                bridge,
                gpui::Corners::all(px(0.)),
                selected_fill,
                gpui::Edges::all(px(0.)),
                gpui::transparent_black(),
                gpui::BorderStyle::Solid,
            ));
            // Concave shoulders soften the join without separating the tab
            // from its terminal. Keep them inside the scroll viewport.
            let radius = px(8.);
            let bottom = position.origin.y + position.size.height;
            for (edge, direction) in [
                (position.origin.x, -1.0),
                (position.origin.x + position.size.width, 1.0),
            ] {
                let mut shoulder = gpui::PathBuilder::fill();
                shoulder.move_to(gpui::point(edge + radius * direction, bottom));
                shoulder.cubic_bezier_to(
                    gpui::point(edge, bottom - radius),
                    gpui::point(edge + radius * direction * 0.448, bottom),
                    gpui::point(edge, bottom - radius * 0.448),
                );
                shoulder.line_to(gpui::point(edge, bottom));
                shoulder.close();
                if let Ok(path) = shoulder.build() {
                    window.paint_path(path, selected_fill);
                }
            }
            if now.duration_since(pill.started) < TAB_SLIDE_DURATION {
                window.request_animation_frame();
            }
        },
    ).absolute().size_full().top_0().left_0();

    let mut wrap = div()
        .flex()
        .min_w_0()
        .relative()
        .overflow_hidden()
        .child(sliding_pill)
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
                .w(px(28.))
                .h(px(28.))
                .rounded(px(8.))
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
            icon_button("tb-workspace-theme", Icon::Theme, &theme, false)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_workspace_theme(cx))),
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
    // Resolve by stable id, not a stored position: closing an earlier tab
    // shifts positions, and a stale index would act on the wrong tab.
    let index = app.tab_index_for_id(menu.tab_id);
    let panes = index.and_then(|i| app.tabs.get(i)).map(|tab| tab.pane_count()).unwrap_or(1);
    let close_label = if panes > 1 {
        format!("Close tab · {panes} panes")
    } else {
        "Close tab".to_string()
    };
    // Tab is gone: menu already rendered with default label; actions no-op.
    let tab_id = menu.tab_id;

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
            move |this, cx| {
                if let Some(i) = this.tab_index_for_id(tab_id) { this.begin_rename(i, cx); }
            },
        ))
        .child(ctx_row(
            "tab-ctx-split-h",
            "Split right",
            Icon::SplitH,
            false,
            theme,
            cx,
            move |this, cx| {
                if let Some(i) = this.tab_index_for_id(tab_id) { this.split_tab(i, SplitDirection::Row, cx); }
            },
        ))
        .child(ctx_row(
            "tab-ctx-split-v",
            "Split down",
            Icon::SplitV,
            false,
            theme,
            cx,
            move |this, cx| {
                if let Some(i) = this.tab_index_for_id(tab_id) { this.split_tab(i, SplitDirection::Column, cx); }
            },
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
            move |this, cx| {
                if let Some(i) = this.tab_index_for_id(tab_id) { this.close_tab(i, cx); }
            },
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
