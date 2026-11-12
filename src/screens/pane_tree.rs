use gpui::{
    AnyElement, Context, CursorStyle, ElementId, Entity, IntoElement, MouseButton,
    Pixels, Render, SharedString, ScrollWheelEvent, Window, canvas, div, prelude::*, px, relative,
};

use crate::components::icons::{Icon, icon};
use crate::screens::app_shell::{SpritXApp, PaneMenuState, SplitDragState};
use crate::state::{
    LeafId, LeafState, PaneNode, SplitDirection, TabState, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO,
};
use crate::terminal;
use crate::theme::{Theme, term_pad_x, term_pad_y};

const PROMPT_STATUS_GAP_PX: f32 = 4.0;
/// Fixed width (px) of the split divider; the drag ratio must be computed
/// against the container size minus this, since the layout reserves it.
const DIVIDER_PX: f32 = 8.0;

fn terminal_right_padding() -> Pixels {
    term_pad_x() + px(8.)
}

fn paint_prompt_cap(
    window: &mut Window,
    bounds: gpui::Bounds<Pixels>,
    color: gpui::Hsla,
    background: gpui::Hsla,
    opening: bool,
) {
    let left = bounds.origin.x;
    let right = left + bounds.size.width;
    let top = bounds.origin.y;
    let bottom = top + bounds.size.height;
    let (join, tip) = if opening { (right, left) } else { (left, right) };
    let half_height = bounds.size.height / 2.;
    let mid = top + half_height;
    // Approximate a half ellipse that fills the shaped cap cell from top to bottom.
    let bend_x = join + (tip - join) * 0.552_284_8;
    let bend_y = half_height * 0.552_284_8;
    let mut path = gpui::PathBuilder::fill();
    path.move_to(gpui::point(join, top));
    path.cubic_bezier_to(gpui::point(tip, mid), gpui::point(bend_x, top), gpui::point(tip, mid - bend_y));
    path.cubic_bezier_to(gpui::point(join, bottom), gpui::point(tip, mid + bend_y), gpui::point(bend_x, bottom));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_quad(gpui::fill(bounds, background));
        window.paint_path(path, color);
    }
}

// ANSI applications address a uniform cell grid. Prompt decoration must never
// insert pixels between rows as the prompt enters/leaves the visible viewport.
fn visual_row_top(row: u16, line_h: f32, _prompt_row: Option<u16>) -> f32 {
    f32::from(row) * line_h
}

#[cfg(test)]
mod prompt_gap_tests {
    use super::*;
    #[test]
    fn prompt_scrolling_does_not_move_output_cursor_or_selection_rows() {
        for row in 0..100 {
            let plain = visual_row_top(row, 20., None);
            for header in [0, 1, 20, 99] {
                assert_eq!(visual_row_top(row, 20., Some(header)), plain);
            }
        }
    }
}

/// Payload used to resize a split while the 8px divider is dragged.
#[derive(Clone, Debug)]
pub struct SplitDrag {
    pub path: Vec<usize>,
    pub direction: SplitDirection,
}

/// Invisible view rendered under the cursor while dragging a divider.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(1.))
    }
}

/// Map a window-space point onto a terminal cell using the grid origin the
/// renderer recorded during the last paint.
fn select_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SPRITX_DEBUG_SELECT").is_some())
}

fn cell_from_position(
    session: &terminal::TerminalSession,
    x: f32,
    y: f32,
    clamp: bool,
) -> Option<(u16, u16)> {
    let slot = session.grid_origin_slot();
    let (origin_x, origin_y, width, height) = (*slot.lock().ok()?)?;
    let cell_w = terminal::cell_width();
    let line_h = terminal::term_line_height();
    if cell_w <= 0.0 || line_h <= 0.0 {
        return None;
    }
    let (cols, rows) = session.size();
    let cols = f32::from(cols.max(1));
    let rows = f32::from(rows.max(1));
    let local_x = x - origin_x;
    let local_y = y - origin_y;
    // Clickable content area: the canvas rect when available, never smaller
    // than the laid-out text grid.
    let content_w = width.max(cols * cell_w);
    let content_h = height.max(rows * line_h);
    // A new selection must start inside the content area; a drag is clamped
    // to the nearest cell so it keeps working past the edges.
    if !clamp && (local_x < 0.0 || local_y < 0.0 || local_x >= content_w || local_y >= content_h) {
        return None;
    }
    let col = (local_x / cell_w).floor().clamp(0.0, cols - 1.0) as u16;
    let row = (local_y / line_h).floor().clamp(0.0, rows - 1.0) as u16;
    if select_debug() {
        eprintln!(
            "[sel] pos=({x:.0},{y:.0}) origin=({origin_x:.0},{origin_y:.0}) size=({width:.0}x{height:.0}) cols={cols} rows={rows} cell_w={cell_w:.2} line_h={line_h:.1} -> row={row} col={col}"
        );
    }
    Some((row, col))
}

/// Map a GPUI mouse button to its xterm base button code.
fn mouse_base_button(button: MouseButton) -> i32 {
    match button {
        MouseButton::Left => terminal::mouse_button::LEFT,
        MouseButton::Middle => terminal::mouse_button::MIDDLE,
        MouseButton::Right => terminal::mouse_button::RIGHT,
        MouseButton::Navigate(_) => terminal::mouse_button::NONE,
    }
}

/// Map GPUI keyboard modifiers to the bits xterm mouse reports carry.
fn mouse_mods(modifiers: &gpui::Modifiers) -> terminal::MouseMods {
    terminal::MouseMods {
        shift: modifiers.shift,
        alt: modifiers.alt,
        ctrl: modifiers.control,
    }
}

/// Translate a scroll delta into `(wheel button, notch count)`. In this UI a
/// positive `y` is wheel-up (the same sign `scroll_lines` consumes), so wheel
/// up sends button 64 and wheel down button 65. The notch count is capped so a
/// fast trackpad fling cannot flood the child with hundreds of reports.
fn wheel_notches(delta: gpui::ScrollDelta) -> (i32, i32) {
    const MAX_NOTCHES: i32 = 10;
    let (y, per_notch) = match delta {
        gpui::ScrollDelta::Lines(point) => (point.y, 1.0),
        gpui::ScrollDelta::Pixels(point) => (f32::from(point.y), 10.0),
    };
    let button = if y >= 0.0 {
        terminal::mouse_button::WHEEL_UP
    } else {
        terminal::mouse_button::WHEEL_DOWN
    };
    let count = (y.abs() / per_notch).round().max(1.0).min(MAX_NOTCHES as f32) as i32;
    (button, count)
}

pub fn render(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let Some(tab) = app.active_tab_checked() else {
        return div().flex().flex_1().items_center().justify_center()
            .text_size(px(12.)).text_color(app.theme.fg_dim)
            .child("Use + to open a terminal").into_any_element();
    };
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .relative()
        .child(render_node(app, tab, &tab.tree, &[], cx))
        .into_any_element()
}

fn render_node(
    app: &SpritXApp,
    tab: &TabState,
    node: &PaneNode,
    path: &[usize],
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    match node {
        PaneNode::Leaf(id) => render_leaf(app, tab, *id, cx),
        PaneNode::Split {
            direction,
            ratio,
            children,
        } => {
            let theme = app.theme;
            let is_row = *direction == SplitDirection::Row;
            let ratio = ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
            let mut container = div()
                .id(split_name("split", tab.id, path))
                .flex()
                .flex_1()
                .min_w_0()
                .min_h_0();
            if is_row {
                container = container.flex_row();
            } else {
                container = container.flex_col();
            }

            let drag_path = path.to_vec();
            let drag_direction = *direction;
            container = container.on_drag_move::<SplitDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<SplitDrag>, _, cx| {
                    let Some(drag) = event.dragged_item().downcast_ref::<SplitDrag>() else {
                        return;
                    };
                    if drag.path != drag_path || drag.direction != drag_direction {
                        return;
                    }
                    let bounds = event.bounds;
                    let position = event.event.position;
                    let ratio = match drag_direction {
                        SplitDirection::Row => {
                            (f32::from(position.x) - f32::from(bounds.origin.x))
                                / (f32::from(bounds.size.width) - DIVIDER_PX).max(1.0)
                        }
                        SplitDirection::Column => {
                            (f32::from(position.y) - f32::from(bounds.origin.y))
                                / (f32::from(bounds.size.height) - DIVIDER_PX).max(1.0)
                        }
                    };
                    if this.set_split_ratio(&drag.path, ratio) {
                        this.split_drag = Some(SplitDragState {
                            path: drag.path.clone(),
                            direction: drag.direction,
                        });
                        cx.notify();
                    }
                },
            ));

            let mut path_a = path.to_vec();
            path_a.push(0);
            let mut path_b = path.to_vec();
            path_b.push(1);
            let a = render_node(app, tab, &children[0], &path_a, cx);
            let b = render_node(app, tab, &children[1], &path_b, cx);
            let child_a = div()
                .flex()
                .min_w_0()
                .min_h_0()
                .flex_basis(relative(ratio))
                .flex_grow()
                .flex_shrink()
                .child(a);
            let child_b = div()
                .flex()
                .min_w_0()
                .min_h_0()
                .flex_basis(relative(1.0 - ratio))
                .flex_grow()
                .flex_shrink()
                .child(b);

            container
                .child(child_a)
                .child(divider(theme, is_row, path, tab.id))
                .child(child_b)
                .into_any_element()
        }
    }
}

/// Element name for a split at `path` in tab `tab_id`, built from the path
/// itself (not a wrapping hash) so distinct splits can never collide.
fn split_name(prefix: &str, tab_id: u64, path: &[usize]) -> SharedString {
    let mut name = format!("{prefix}-{tab_id}");
    for slot in path {
        name.push_str(&format!("-{slot}"));
    }
    name.into()
}

fn divider(theme: Theme, is_row: bool, path: &[usize], tab_id: u64) -> AnyElement {
    let drag_path = path.to_vec();
    let direction = if is_row {
        SplitDirection::Row
    } else {
        SplitDirection::Column
    };
    let mut base = div()
        .id(split_name("pane-divider", tab_id, path))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor(if is_row {
            CursorStyle::ResizeLeftRight
        } else {
            CursorStyle::ResizeUpDown
        })
        .hover(|s| s.bg(theme.accent.opacity(0.22)))
        .on_drag(
            SplitDrag {
                path: drag_path,
                direction,
            },
            |_, _, _, cx| cx.new(|_| DragGhost),
        );
    let line = if is_row {
        div().w(px(1.)).h_full().rounded(px(1.)).bg(theme.border)
    } else {
        div().w_full().h(px(1.)).rounded(px(1.)).bg(theme.border)
    };
    base = if is_row {
        base.w(px(DIVIDER_PX)).h_full()
    } else {
        base.h(px(DIVIDER_PX)).w_full()
    };
    base.child(line).into_any_element()
}

fn render_leaf(
    app: &SpritXApp,
    tab: &TabState,
    id: LeafId,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let theme = app.theme;
    let Some(leaf) = tab.leaves.get(&id) else {
        return div().into_any_element();
    };
    // Capture the stable tab id, not the render-time index: an earlier tab
    // closing between render and click would shift the index onto a
    // different tab.
    let tab_id = tab.id;

    let mut pane = div()
        .id(("pane", id as usize))
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .relative()
        .overflow_hidden()
        .rounded(px(10.))
        .bg(terminal::term_theme().background)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                this.file_preview_focused = None;
                // Look the tab up by id (captured above), not by a
                // render-time index that a tab close could have shifted.
                if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if tab.leaves.contains_key(&id) {
                        tab.active_leaf = id;
                    }
                }
                // Any press in the pane maps to the nearest cell, so a
                // selection can start anywhere (left, right or past the last
                // column).
                let cell = this.sessions.get(&id).and_then(|session| {
                    cell_from_position(
                        session,
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        false,
                    )
                });
                if let Some((row, col)) = cell {
                    // A fullscreen TUI that enabled mouse reporting (e.g. Pi)
                    // owns the pointer; Shift forces a local text selection
                    // instead, like a normal terminal.
                    let child_owns = !event.modifiers.shift
                        && this.sessions.get(&id).is_some_and(|s| s.wants_mouse());
                    if child_owns {
                        if let Some(session) = this.sessions.get(&id) {
                            session.report_mouse(
                                col,
                                row,
                                terminal::MouseAction::Press {
                                    button: mouse_base_button(event.button),
                                },
                                mouse_mods(&event.modifiers),
                            );
                        }
                    } else if let Some(session) = this.sessions.get_mut(&id) {
                        session.begin_selection(row, col);
                    }
                }
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
            let mut repaint = false;
            if this.settings.focus_follows_mouse {
                // Same id-based lookup as mouse-down above: a render-time
                // index could point at a different tab after a close.
                if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if tab.active_leaf != id && tab.leaves.contains_key(&id) {
                        tab.active_leaf = id;
                        repaint = true;
                    }
                }
            }
            let cell = this.sessions.get(&id).and_then(|session| {
                cell_from_position(
                    session,
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                    true,
                )
            });
            if let Some((row, col)) = cell {
                let child_owns = !event.modifiers.shift
                    && this.sessions.get(&id).is_some_and(|s| s.wants_mouse());
                if child_owns {
                    if let Some(session) = this.sessions.get(&id) {
                        repaint |= session.report_mouse(
                            col,
                            row,
                            terminal::MouseAction::Motion {
                                button: event.pressed_button.map(mouse_base_button),
                            },
                            mouse_mods(&event.modifiers),
                        );
                    }
                } else if let Some(session) = this.sessions.get_mut(&id) {
                    if session.is_selecting() {
                        session.extend_selection(row, col);
                        repaint = true;
                    }
                }
            }
            if repaint {
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                let child_owns = !event.modifiers.shift
                    && this.sessions.get(&id).is_some_and(|s| s.wants_mouse());
                if child_owns {
                    let cell = this.sessions.get(&id).and_then(|session| {
                        cell_from_position(
                            session,
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            true,
                        )
                    });
                    if let Some((row, col)) = cell {
                        if let Some(session) = this.sessions.get(&id) {
                            session.report_mouse(
                                col,
                                row,
                                terminal::MouseAction::Release {
                                    button: mouse_base_button(event.button),
                                },
                                mouse_mods(&event.modifiers),
                            );
                        }
                    }
                } else {
                    let mut repaint = false;
                    if let Some(session) = this.sessions.get_mut(&id) {
                        session.end_selection();
                        if !session.has_selection() {
                            session.clear_selection();
                            repaint = true;
                        }
                    }
                    if repaint {
                        cx.notify();
                    }
                }
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                let child_owns = !event.modifiers.shift
                    && this.sessions.get(&id).is_some_and(|s| s.wants_mouse());
                if child_owns {
                    // The pointer was released outside the pane: still tell the
                    // child, or it would believe a button is stuck down.
                    let cell = this.sessions.get(&id).and_then(|session| {
                        cell_from_position(
                            session,
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            true,
                        )
                    });
                    if let (Some((row, col)), Some(session)) =
                        (cell, this.sessions.get(&id))
                    {
                        session.report_mouse(
                            col,
                            row,
                            terminal::MouseAction::Release {
                                button: mouse_base_button(event.button),
                            },
                            mouse_mods(&event.modifiers),
                        );
                    }
                } else if let Some(session) = this.sessions.get_mut(&id) {
                    session.end_selection();
                }
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.open_pane_menu(
                    id,
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                    cx,
                );
            }),
        )
        .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
            // A fullscreen TUI that enabled mouse reporting receives wheel
            // notches as mouse buttons; Shift forces local scrollback instead.
            let child_owns = !event.modifiers.shift
                && this.sessions.get(&id).is_some_and(|s| s.wants_mouse());
            if child_owns {
                let cell = this.sessions.get(&id).and_then(|session| {
                    cell_from_position(
                        session,
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        true,
                    )
                });
                let (button, count) = wheel_notches(event.delta);
                if let (Some((row, col)), Some(session)) = (cell, this.sessions.get(&id)) {
                    let mods = mouse_mods(&event.modifiers);
                    for _ in 0..count {
                        session.report_mouse(
                            col,
                            row,
                            terminal::MouseAction::Wheel { button },
                            mods,
                        );
                    }
                }
                return;
            }
            let Some(session) = this.sessions.get_mut(&id) else {
                return;
            };
            // One row per ~10px (or one line per notch) of wheel travel;
            // ignored on the alternate screen (handled by
            // `scroll_lines`). Negated so wheel-up reveals older lines, the
            // same direction as a normal terminal.
            let lines = match event.delta {
                gpui::ScrollDelta::Lines(point) => -point.y.round() as isize,
                gpui::ScrollDelta::Pixels(point) => {
                    -(f32::from(point.y) / 10.0).round() as isize
                }
            };
            if lines != 0 && session.scroll_lines(lines) {
                cx.notify();
            }
        }));

    pane = pane.child(render_live_terminal(
        app,
        leaf,
        &theme,
        cx.entity(),
        id,
    ));

    div().id(("pane-with-preview", id as usize)).flex().flex_1().min_w_0().min_h_0()
        .overflow_hidden().child(pane)
        .when(app.file_previews.contains_key(&id), |row| row.child(super::file_preview::render(app, id, cx)))
        .into_any_element()
}

fn render_live_terminal(
    app: &SpritXApp,
    leaf: &LeafState,
    theme: &Theme,
    pane_entity: Entity<SpritXApp>,
    leaf_id: u64,
) -> AnyElement {
    if let Some(error) = app.session_errors.get(&leaf.id) {
        return div()
            .flex()
            .flex_col()
            .flex_1()
            .p(px(16.))
            .gap(px(8.))
            .text_size(px(12.))
            .child(
                div()
                    .text_color(theme.danger)
                    .child("Could not start the shell"),
            )
            .child(div().text_color(theme.fg_dim).child(error.clone()))
            .into_any_element();
    }
    let Some(session) = app.session(leaf.id) else {
        return div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(theme.fg_dim)
            .child("No terminal session")
            .into_any_element();
    };

    let screen = session.screen();
    let cursor = session
        .visible_cursor()
        .filter(|_| session.exit_code().is_none());
    let cell_w = terminal::cell_width();
    let line_h = terminal::term_line_height();
    let appearance = terminal::appearance();
    let term_theme = terminal::term_theme();
    let alternate = screen.alternate_screen();
    let prompt_row = session.powerline_prompt_row();
    let prompt_layout = session.powerline_prompt_end().and_then(|(row, _)| {
        let header = session.render_row(row, term_theme.foreground, term_theme.background);
        let first = header.spans.first()?;
        let last = header.spans.last()?;
        let caps = (first.text == "" && last.text == ""
            && first.colors.bg.is_none() && last.colors.bg.is_none())
            .then_some((first.colors.fg, last.colors.fg));
        let mut text = String::new();
        let mut runs = Vec::new();
        for span in &header.spans {
            terminal::append_span_runs(span, &mut runs);
            text.push_str(&span.text);
        }
        let top = px(visual_row_top(row, line_h, prompt_row));
        Some((top, text, runs, caps))
    });
    let font_size = px(appearance.font_size);
    let status_size = px(appearance.font_size * (5. / 13.));
    let show_status = app.active_tab().active_leaf == leaf_id && app.terminal_has_focus();
    let status_color = theme.success;

    crate::metrics::add(crate::metrics::Name::Panes, 1);
    // One shaped text grid with fixed line height: shell redraws, wrapping,
    // and scrollback cannot change the geometry of previously painted rows.
    let grid = crate::metrics::terminal(|| {
        let (text, runs) = session.grid_text(term_theme.foreground, term_theme.background);
        div().flex().flex_col().flex_1().whitespace_nowrap().overflow_hidden()
            .child(gpui::StyledText::new(text).with_runs(runs))
    });

    // Absolute canvas that records where the grid content starts, so mouse
    // events (window coordinates) can be mapped back to terminal cells.
    let origin_slot = session.grid_origin_slot();
    let origin_canvas = canvas(
        move |bounds: gpui::Bounds<Pixels>, _window: &mut Window, _cx: &mut gpui::App| {
            if let Ok(mut slot) = origin_slot.lock() {
                *slot = Some((
                    f32::from(bounds.origin.x),
                    f32::from(bounds.origin.y),
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                ));
            }
        },
        move |bounds: gpui::Bounds<Pixels>, _: (), window: &mut Window, _cx: &mut gpui::App| {
            let mut status_origin = bounds.origin
                + gpui::point(bounds.size.width - status_size, px(0.));
            if let Some((top, text, runs, caps)) = prompt_layout {
                let layout = window.text_system().layout_line(&text, font_size, &runs, None);
                let origin = bounds.origin + gpui::point(px(0.), top);
                status_origin = origin + gpui::point(layout.width + px(PROMPT_STATUS_GAP_PX), px(0.));
                if let Some((opening, closing)) = caps {
                    let start_width = layout.x_for_index(''.len_utf8());
                    let end_x = layout.x_for_index(text.len() - ''.len_utf8());
                    for (x, width, color, opening) in [
                        (px(0.), start_width, opening, true),
                        (end_x, layout.width - end_x, closing, false),
                    ] {
                        let cap_bounds = gpui::Bounds::new(
                            origin + gpui::point(x, px(0.)),
                            gpui::size(width, px(line_h)),
                        );
                        paint_prompt_cap(window, cap_bounds, color, term_theme.background, opening);
                    }
                }
            }
            if show_status && window.is_window_active() {
                let dot_bounds = gpui::Bounds::new(
                    status_origin + gpui::point(px(0.), (px(line_h) - status_size) / 2.),
                    gpui::size(status_size, status_size),
                );
                window.paint_quad(gpui::quad(
                    dot_bounds,
                    gpui::Corners::all(status_size / 2.),
                    status_color,
                    gpui::Edges::all(px(0.)),
                    gpui::transparent_black(),
                    gpui::BorderStyle::Solid,
                ));
            }
        },
    )
    .absolute()
    .top(term_pad_y())
    .left(term_pad_x())
    .right(terminal_right_padding())
    .bottom(term_pad_y());

    // Translucent overlay also highlights cells with explicit ANSI backgrounds.
    let mut selection_rects: Vec<AnyElement> = Vec::new();
    if let Some(selection) = session.selection() {
        let (start, end) = selection.ordered();
        let (row_count, col_count) = {
            let (rows, cols) = screen.size();
            (rows.get(), cols.get())
        };
        for row in start.0..=end.0.min(row_count.saturating_sub(1)) {
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1.min(col_count.saturating_sub(1))
            } else {
                col_count.saturating_sub(1)
            };
            if last < first {
                continue;
            }
            selection_rects.push(
                div()
                    .absolute()
                    .left(term_pad_x() + px(f32::from(first) * cell_w))
                    .top(term_pad_y() + px(visual_row_top(row, line_h, prompt_row)))
                    .w(px(f32::from(last - first + 1) * cell_w))
                    .h(px(line_h))
                    .bg(term_theme.selection.opacity(0.45))
                    .into_any_element(),
            );
        }
    }

    let mut body = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .relative()
        .cursor(CursorStyle::IBeam)
        .overflow_hidden()
        .pl(term_pad_x())
        .pr(terminal_right_padding())
        .py(term_pad_y())
        .font_family(appearance.font_family.clone())
        .text_size(px(appearance.font_size))
        .line_height(px(line_h))
        .text_color(term_theme.foreground)
        .child(grid)
        .child(origin_canvas)
        .children(selection_rects);

    // Only the active pane blinks; inactive panes retain a stable caret.
    let active = app.active_tab().active_leaf == leaf_id && app.modal.is_none();
    if let Some((cursor_row, cursor_col)) = cursor.filter(|_| !active || app.cursor_blink_on || !appearance.cursor_blink) {
        let wide = screen
            .cell(cursor_row, cursor_col)
            .is_some_and(|cell| cell.is_wide());
        let cell_width = cell_w * if wide { 2.0 } else { 1.0 };
        let (caret_w, caret_h, dx, dy) = match appearance.cursor_shape {
            terminal::CursorShape::Block => (px(cell_width), px(line_h), px(0.), px(0.)),
            terminal::CursorShape::Bar => (px(2.), px(line_h), px(0.), px(0.)),
            terminal::CursorShape::Underline => {
                (px(cell_width), px(2.), px(0.), px(line_h - 2.0))
            }
        };
        body = body.child(
            div()
                .absolute()
                .left(term_pad_x() + px(cursor_col as f32 * cell_w) + dx)
                .top(term_pad_y() + px(visual_row_top(cursor_row, line_h, prompt_row)) + dy)
                .w(caret_w)
                .h(caret_h)
                .bg(term_theme.cursor.opacity(0.85)),
        );
    }

    // Scrollback hint (e.g. "3 / 512 lines") only while scrolled in the
    // normal screen; the alternate screen never shows it.
    let scrollback = screen.scrollback();
    if scrollback > 0 && !alternate {
        body = body.child(
            div()
                .absolute()
                .right(terminal_right_padding())
                .top(px(4.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(3.))
                .bg(theme.bg_soft)
                .border_1()
                .border_color(theme.border)
                .text_size(px(10.))
                .text_color(theme.fg_dim)
                .child(format!("{scrollback} lines up")),
        );
    }

    if let Some(code) = session.exit_code() {
        body = body.child(
            div()
                .absolute()
                .right(px(8.))
                .bottom(px(6.))
                .px(px(6.))
                .py(px(2.))
                .rounded(px(3.))
                .bg(if code == 0 {
                    theme.success.opacity(0.14)
                } else {
                    theme.danger.opacity(0.14)
                })
                .text_size(px(10.))
                .text_color(if code == 0 { theme.success } else { theme.danger })
                .child(format!("process exited · {code}")),
        );
    }

    // Pane-aware PTY sizing: measure the body's pixel box during prepaint
    // and resize the session on the next deferred frame. Skipped while a remote
    // client is attached — its Resize input already sized the PTY for its own
    // screen, and re-asserting the box here would hand the client a grid taller
    // than the one it can draw (its bottom rows, where a TUI keeps its prompt,
    // are then unreachable). The last client leaving changes the status stamp,
    // which repaints this pane and lets it take its size back.
    let remote_driven = app.remote_server.as_ref().is_some_and(|server| server.has_clients_for_leaf(leaf_id));
    let (cur_cols, cur_rows) = session.size();
    body = body.child(
        canvas(
            move |bounds, window, cx| {
                if remote_driven {
                    return;
                }
                let width = f32::from(bounds.size.width - term_pad_x() - terminal_right_padding());
                let height = f32::from(bounds.size.height) - 2.0 * f32::from(term_pad_y());
                if width <= 0.0 || height <= 0.0 {
                    return; // collapsed pane: keep the current size
                }
                let metrics = terminal::term_metrics(window);
                let (new_cols, new_rows) = terminal::grid_size(width, height, metrics);
                if new_cols == cur_cols && new_rows == cur_rows {
                    return;
                }
                cx.defer(move |cx| {
                    let _ = pane_entity.update(cx, |this, app_cx| {
                        if let Some(session) = this.sessions.get_mut(&leaf_id) {
                            let (c, r) = session.size();
                            if (c, r) != (new_cols, new_rows) {
                                let _ = session.resize(new_cols, new_rows);
                                app_cx.notify();
                            }
                        }
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0(),
    );

    body.into_any_element()
}

/// Pane context menu (C39–C46), rendered on the root overlay layer.
pub fn render_context_menu(
    app: &SpritXApp,
    menu: &PaneMenuState,
    viewport: gpui::Size<Pixels>,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let theme = app.theme;
    let leaf = menu.leaf_id;
    let at = (menu.x, menu.y);
    let last_pane = app
        .tabs
        .iter()
        .find(|tab| tab.leaves.contains_key(&leaf))
        .map(|tab| tab.pane_count() <= 1)
        .unwrap_or(true);
    let close_label = if last_pane {
        "Close tab".to_string()
    } else {
        "Close pane".to_string()
    };

    div()
        .id("pane-context-menu")
        .absolute()
        .left(px(menu.x).clamp(px(0.), (viewport.width - px(220.)).max(px(0.))))
        .top(px(menu.y).clamp(px(0.), (viewport.height - px(330.)).max(px(0.))))
        .w(px(220.))
        .flex()
        .flex_col()
        .p(px(6.))
        .rounded(px(12.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .occlude()
        .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| {
            cx.stop_propagation();
        }))
        .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| {
            cx.stop_propagation();
        }))
        .child(menu_row(
            "pane-ctx-copy",
            "Copy",
            Icon::Copy,
            false,
            theme,
            cx,
            move |this, cx| {
                this.focus_leaf(leaf, cx);
                this.copy_selection(cx);
            },
        ))
        .child(menu_row(
            "pane-ctx-paste",
            "Paste",
            Icon::Clipboard,
            false,
            theme,
            cx,
            move |this, cx| {
                this.focus_leaf(leaf, cx);
                this.paste_clipboard(cx);
            },
        ))
        .child(div().h(px(1.)).my(px(4.)).bg(theme.border))
        .child(menu_row("pane-ctx-interrupt", "Interrupt process", Icon::Stop, false, theme, cx,
            move |this, cx| { this.focus_leaf(leaf, cx); this.interrupt_pane(leaf, cx); }))
        .when(!app.session(leaf).is_some_and(|session| session.is_remote()), |menu| menu.child(
            menu_row("pane-ctx-force-stop", "Force stop & restart", Icon::Refresh, true, theme, cx,
                move |this, cx| { this.focus_leaf(leaf, cx); this.force_stop_pane(leaf, cx); })))
        .child(div().h(px(1.)).my(px(4.)).bg(theme.border))
        .child(menu_row(
            "pane-ctx-split-h",
            "Split right",
            Icon::SplitH,
            false,
            theme,
            cx,
            move |this, cx| {
                // Resolve the owning tab from the leaf at click time: after a
                // tab switch with this menu open, `active_tab` is a different
                // tab and the split would silently no-op against it.
                if let Some(index) = this.tabs.iter().position(|t| t.leaves.contains_key(&leaf)) {
                    this.split_leaf_in_tab(index, leaf, SplitDirection::Row, cx)
                }
            },
        ))
        .child(menu_row(
            "pane-ctx-split-v",
            "Split down",
            Icon::SplitV,
            false,
            theme,
            cx,
            move |this, cx| {
                // Same leaf-based resolution as split-right above.
                if let Some(index) = this.tabs.iter().position(|t| t.leaves.contains_key(&leaf)) {
                    this.split_leaf_in_tab(index, leaf, SplitDirection::Column, cx)
                }
            },
        ))
        .child(menu_row(
            "pane-ctx-remote",
            "Remote control",
            Icon::Remote,
            false,
            theme,
            cx,
            move |this, cx| {
                // Share *this* pane, not whatever the desktop is focused on:
                // `refresh_remote_tabs` publishes the active leaf, so focusing
                // here is what actually points remote clients at it.
                this.focus_leaf(leaf, cx);
                this.remote_menu_at = Some(at);
                this.remote_menu_open = true;
                cx.notify();
            },
        ))
        .child(div().h(px(1.)).my(px(4.)).bg(theme.border))
        .child(menu_row(
            "pane-ctx-new",
            "New tab",
            Icon::Plus,
            false,
            theme,
            cx,
            |this, cx| this.add_tab(cx),
        ))
        .child(menu_row(
            "pane-ctx-close",
            close_label,
            Icon::X,
            true,
            theme,
            cx,
            move |this, cx| this.close_pane(leaf, cx),
        ))
        .into_any_element()
}

fn menu_row(
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
        .h(px(32.))
        .rounded(px(8.))
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
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.pane_menu = None;
            action(this, cx);
            cx.notify();
        }))
        .child(icon(ic, 14.0, icon_color))
        .child(label)
        .into_any_element()
}
