//! Modal and overlay layer.
//!
//! Every expanded surface is emitted as its own full-window, deferred layer:
//! the dim backdrop and the card are separate siblings with explicit absolute
//! geometry, and both are painted after the shell. The previous implementation
//! nested the card inside the backdrop's absolute box; in GPUI 0.2.2 that inner
//! box can be laid out at zero height when the surrounding flex column has
//! already consumed the window height, so the backdrop painted but the card
//! never received a paintable rect. Sibling layers plus `deferred` remove that
//! coupling and guarantee the card paints above the dim layer.

use std::path::Path;

use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, IntoElement, MouseButton, SharedString,
    Stateful, deferred, div, prelude::*, px, relative, rgb,
};

use crate::backend::history::HistoryEntry;
use crate::components::buttons::{danger_button, dot, primary_button, text_button};
use crate::components::icons::{Icon, icon};
use crate::components::toggle::switch;
use crate::screens::app_shell::{SpritXApp, hsla_to_hex};
use crate::state::{
    LAYOUT_PRESETS, Modal, PaletteMode, SettingsField, SettingsMenu, SettingsSection,
    ThemeColorKey,
};
use crate::theme::Theme;

// Deferred-overlay priorities (higher paints later / receives clicks first):
// context menus 110, popup backdrop 119, popups 120, modal/side modal 121,
// toast 130. Kept distinct so click-outside ordering is not vector-order
// dependent when a modal and a popup are stacked.
pub(crate) const CONTEXT_MENU_Z: usize = 110;
const POPUP_BACKDROP_Z: usize = 119;
const POPUP_Z: usize = 120;
const MODAL_Z: usize = 121;
const TOAST_Z: usize = 130;
const TOP_OFFSET: f32 = 108.0;

thread_local! {
    // Device-revoke confirm state lives only in this view layer. It must
    // never be stored in `SpritXApp::recording_action`: the root key
    // handler treats any non-None value there as "capture next chord as a
    // keybinding" and would persist a bogus `remote-revoke:<id>` binding.
    // This cell is cleared whenever the settings/remote modal is dismissed.
    static ARMED_REVOKE: std::cell::RefCell<Option<String>> =
        std::cell::RefCell::new(None);
}

fn armed_revoke() -> Option<String> {
    ARMED_REVOKE.with(|state| state.borrow().clone())
}

fn set_armed_revoke(device_id: Option<String>) {
    ARMED_REVOKE.with(|state| *state.borrow_mut() = device_id);
}

/// Full modal dismissal: delegate to the app's shared helper (which clears
/// modal, popups, menus, tab/pane edit state and the inline settings
/// field), then clear view-owned transient state the helper does not own.
/// Taking this path from every dim-layer/X/close handler prevents a closed
/// modal from leaving buffers that the next keydown would edit.
fn dismiss_modal(app: &mut SpritXApp, cx: &mut Context<SpritXApp>) {
    app.close_modal();
    // close_modal does not cover recording/palette transients; clear them
    // here so they cannot survive into the next modal or be captured as a
    // shortcut chord after the dialog is gone.
    app.recording_action = None;
    app.palette_query.clear();
    app.palette_selected = 0;
    set_armed_revoke(None);
    cx.notify();
}

// ---------------------------------------------------------------------------
// Shared primitives
// ---------------------------------------------------------------------------

/// Full-window dim layer. It is the only element that closes a modal on click
/// and it is emitted before the card, so the card always paints on top.
fn dim_layer(id: impl Into<ElementId>, theme: Theme, cx: &mut Context<SpritXApp>) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(theme.black_40)
        .occlude()
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
            dismiss_modal(this, cx);
        }))
}

fn modal_layer(
    id: &'static str,
    theme: Theme,
    card: AnyElement,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let stage = div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(TOP_OFFSET));

    let layer = div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(dim_layer(
            SharedString::from(format!("{id}-dim")),
            theme,
            cx,
        ))
        .child(stage.child(card));

    deferred(layer).with_priority(MODAL_Z).into_any_element()
}

fn side_layer(
    id: &'static str,
    theme: Theme,
    width: f32,
    inset: f32,
    panel: AnyElement,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let layer = div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(dim_layer(
            SharedString::from(format!("{id}-dim")),
            theme,
            cx,
        ))
        .child(
            div()
                .id(SharedString::from(format!("{id}-panel")))
                .absolute()
                .top(px(inset))
                .right(px(inset))
                .bottom(px(inset))
                .w(px(width))
                .max_w(relative(0.97))
                .flex()
                .flex_col()
                .when(inset == 0., |d| {
                    d.bg(theme.bg)
                        .border_l_1()
                        .border_color(theme.border)
                        .shadow_lg()
                })
                .occlude()
                .child(panel),
        );

    deferred(layer).with_priority(MODAL_Z).into_any_element()
}

fn popup_layer(
    id: &'static str,
    top: f32,
    right: Option<f32>,
    left: Option<f32>,
    width: f32,
    content: AnyElement,
) -> AnyElement {
    let mut popup = div().id(id).absolute().top(px(top)).w(px(width));
    if let Some(right) = right {
        popup = popup.right(px(right));
    }
    if let Some(left) = left {
        popup = popup.left(px(left));
    }
    deferred(popup.child(content)).with_priority(POPUP_Z).into_any_element()
}

/// Transparent click-catcher under a toolbar popup. Without it the menu only
/// closes on Escape or a second click on its own icon, which reads as stuck.
/// Sits below `POPUP_Z` so the popup itself still takes clicks.
pub fn render_popup_backdrop(cx: &mut Context<SpritXApp>) -> AnyElement {
    deferred(
        div()
            .id("popup-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.layouts_open = false;
                    this.remote_menu_open = false;
                    cx.notify();
                }),
            ),
    )
    .with_priority(POPUP_BACKDROP_Z)
    .into_any_element()
}

fn card(theme: Theme, width: f32, content: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w(px(width))
        .max_h(relative(0.78))
        .rounded(px(10.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .overflow_hidden()
        .occlude()
        .child(content)
        .into_any_element()
}

fn footer_row(theme: Theme, hints: AnyElement, actions: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .px(px(12.))
        .py(px(8.))
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.bg_soft)
        .child(hints)
        .child(actions)
        .into_any_element()
}

fn kbd(theme: Theme, keys: impl Into<String>) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(19.))
        .px(px(6.))
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.white_06)
        .text_size(px(10.))
        .text_color(theme.fg_dim)
        .child(keys.into())
}

fn hint(theme: Theme, keys: &'static str, label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .child(kbd(theme, keys))
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.fg_dim)
                .child(label),
        )
}

fn hints(items: Vec<Div>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .children(items)
        .into_any_element()
}

fn section_header(theme: Theme, label: impl Into<String>) -> AnyElement {
    div()
        .px(px(10.))
        .pt(px(8.))
        .pb(px(3.))
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.fg_dim)
        .child(label.into().to_uppercase())
        .into_any_element()
}

fn esc_chip(theme: Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(20.))
        .px(px(7.))
        .rounded(px(4.))
        .bg(theme.white_06)
        .text_size(px(10.))
        .text_color(theme.fg_dim)
        .child("Esc")
        .into_any_element()
}

fn empty_state(theme: Theme, text: &'static str) -> AnyElement {
    div()
        .px(px(14.))
        .py(px(24.))
        .text_size(px(12.))
        .text_color(theme.fg_dim)
        .child(text)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Layouts popup
// ---------------------------------------------------------------------------

pub fn render_layouts_popup(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let current = app.active_tab_checked().and_then(|tab| current_layout(&tab.tree));
    let mut presets = div().flex().flex_wrap().gap(px(8.));
    for (index, preset) in LAYOUT_PRESETS.iter().enumerate() {
        let id = preset.id;
        let label = preset.label;
        let selected = current == Some(id);
        presets = presets.child(
            div()
                .id(("layout", index))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .w(px(104.))
                .p(px(8.))
                .rounded(px(8.))
                .bg(if selected { theme.bg_soft } else { theme.bg })
                .cursor_pointer()
                .hover(|s| s.bg(theme.white_06))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(p) = LAYOUT_PRESETS.iter().find(|p| p.id == id) {
                        this.apply_layout(p, cx);
                    }
                }))
                .child(layout_preview(theme, preset.hint, selected))
                .child(
                    div()
                        .text_size(px(11.))
                        .when(selected, |d| d.font_weight(FontWeight::MEDIUM))
                        .text_color(theme.fg)
                        .child(label),
                ),
        );
    }

    let body = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(16.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.fg)
                        .child("Pane layouts"),
                )
                .child(esc_chip(theme)),
        )
        .child(presets)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(theme.border)
                .pt(px(8.))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.fg_dim)
                        .child(format!("Up to {} panes per tab", crate::state::MAX_PANES_PER_TAB)),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.fg_dim)
                        .child(format!("{} active", app.active_tab_checked().map(|tab| tab.pane_count()).unwrap_or(0))),
                ),
        )
        .into_any_element();

    let shell = div()
        .flex()
        .flex_col()
        .w(px(372.))
        .rounded(px(12.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .occlude()
        .child(body)
        .into_any_element();

    let header_bottom = f32::from(crate::theme::body_pad() + crate::theme::tab_bar_h());
    popup_layer("layouts-popup", header_bottom, Some(84.0), None, 372.0, shell)
}

fn current_layout(tree: &crate::state::PaneNode) -> Option<&'static str> {
    use crate::state::PaneNode::{Leaf, Split};
    use crate::state::SplitDirection::{Column, Row};

    let Split { direction, children, .. } = tree else { return Some("single") };
    // A malformed Split with fewer than two children must not panic the UI
    // thread on every frame; treat it as an unknown layout instead.
    let (Some(first), Some(second)) = (children.get(0), children.get(1)) else {
        return None;
    };
    match (direction, first.as_ref(), second.as_ref()) {
        (Row, Leaf(_), Leaf(_)) => Some("two-cols"),
        (Column, Leaf(_), Leaf(_)) => Some("two-rows"),
        (Row, Leaf(_), right) if current_layout(right) == Some("two-rows") => Some("main-side"),
        (Row, Leaf(_), right) if current_layout(right) == Some("two-cols") => Some("triple"),
        (Column, top, bottom)
            if current_layout(top) == Some("two-cols") && current_layout(bottom) == Some("two-cols") => Some("grid"),
        _ => None,
    }
}
fn layout_preview(theme: Theme, hint: &'static str, selected: bool) -> AnyElement {
    let fill = if selected { theme.accent.opacity(0.24) } else { theme.fg_dim.opacity(0.13) };
    let mut preview = div()
        .w(px(80.))
        .h(px(50.))
        .rounded(px(5.))
        .bg(theme.white_04)
        .relative()
        .overflow_hidden();

    let bar = |w: f32, h: f32, x: f32, y: f32| {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .rounded(px(3.))
            .bg(fill)
    };

    match hint {
        "single" => {
            preview = preview.child(
                div()
                    .absolute()
                    .inset(px(2.))
                    .rounded(px(2.))
                    .bg(fill),
            );
        }
        "side-by-side" => {
            preview = preview.child(bar(36.0, 46.0, 2.0, 2.0)).child(bar(36.0, 46.0, 42.0, 2.0));
        }
        "stacked" => {
            preview = preview.child(bar(76.0, 21.0, 2.0, 2.0)).child(bar(76.0, 21.0, 2.0, 27.0));
        }
        "grid-2x2" => {
            preview = preview
                .child(bar(36.0, 21.0, 2.0, 2.0))
                .child(bar(36.0, 21.0, 42.0, 2.0))
                .child(bar(36.0, 21.0, 2.0, 27.0))
                .child(bar(36.0, 21.0, 42.0, 27.0));
        }
        "main-side" => {
            preview = preview
                .child(bar(50.0, 46.0, 2.0, 2.0))
                .child(bar(24.0, 21.0, 54.0, 2.0))
                .child(bar(24.0, 21.0, 54.0, 27.0));
        }
        _ => {
            preview = preview
                .child(bar(23.0, 46.0, 2.0, 2.0))
                .child(bar(23.0, 46.0, 29.0, 2.0))
                .child(bar(23.0, 46.0, 56.0, 2.0));
        }
    }

    preview.into_any_element()
}

// ---------------------------------------------------------------------------
// Remote menu popup
// ---------------------------------------------------------------------------

pub fn render_remote_menu(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(16.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(icon(Icon::Smartphone, 16.0, theme.fg_dim))
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg)
                                .child("Remote control"),
                        ),
                )
                .child(esc_chip(theme)),
        );

    let status = app.remote_status();
    if status.running {
        let connected = status.clients > 0;
        let link = status
            .pair_url
            .clone()
            .or_else(|| status.public_url.clone())
            .unwrap_or_else(|| status.url.clone());
        let waiting = status.pair_url.is_none() && !connected;
        body = body
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .child(dot(
                                if connected {
                                    theme.success
                                } else {
                                    theme.warning
                                },
                                7.0,
                            ))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(theme.fg)
                                    .child(if connected {
                                        format!(
                                            "Sharing · {} device(s) connected",
                                            status.clients
                                        )
                                    } else {
                                        "Sharing · no devices connected".to_string()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(if connected { theme.success } else { theme.warning })
                            .child(if connected { "Encrypted" } else { "Waiting" }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .p(px(9.))
                    .rounded(px(6.))
                    .bg(theme.bg_soft)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.fg_dim)
                                    .child(if status.public_url.is_some() {
                                        "Public link"
                                    } else {
                                        "Share link"
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(if status.pair_url.is_some() {
                                        theme.warning
                                    } else {
                                        theme.success
                                    })
                                    .child(match status.pairing_expires_in {
                                        Some(seconds) => format!("pairing {}s", seconds),
                                        None if waiting => "no pairing code".to_string(),
                                        None => "ready".to_string(),
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.accent)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(link),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(
                        primary_button("remote-new-link", "New link", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.remote_new_pairing(cx);
                            }),
                        ),
                    )
                    .child(text_button("remote-copy", "Copy", &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.remote_copy_pairing(cx);
                        }),
                    ))
                    .child(danger_button("remote-stop", "Stop", &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.remote_stop(cx);
                            this.remote_menu_open = false;
                        }),
                    )),
            );
    } else {
        body = body
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.fg_dim)
                    .child("Share your terminals with a phone, browser or another SpritX."),
            )
            .child(
                primary_button("remote-start", "Start sharing", &theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.remote_start(cx);
                    },
                )),
            );
    }

    let footer = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(menu_row("Sharing and devices", Icon::Info, theme, cx, |this, cx| {
            this.remote_menu_open = false;
            this.modal = Some(Modal::Remote);
            cx.notify();
        }))
        .child(menu_row(
            "Connect from clipboard",
            Icon::Clipboard,
            theme,
            cx,
            |this, cx| {
                this.remote_menu_open = false;
                this.remote_connect_from_clipboard(cx);
            },
        ));
    body = body.child(div().h(px(1.)).bg(theme.border)).child(footer);

    let shell = div()
        .flex()
        .flex_col()
        .w(px(340.))
        .rounded(px(12.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .occlude()
        .child(body.into_any_element())
        .into_any_element();

    match app.remote_menu_at {
        Some((x, y)) => popup_layer("remote-menu", y, None, Some(x), 340.0, shell),
        None => {
            let header_bottom = f32::from(crate::theme::body_pad() + crate::theme::tab_bar_h());
            popup_layer("remote-menu", header_bottom, Some(120.0), None, 340.0, shell)
        }
    }
}

fn menu_row(
    label: &'static str,
    ic: Icon,
    theme: Theme,
    cx: &mut Context<SpritXApp>,
    action: impl Fn(&mut SpritXApp, &mut Context<SpritXApp>) + 'static,
) -> AnyElement {
    div()
        .id(label)
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .h(px(32.))
        .rounded(px(8.))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(theme.fg)
        .hover(|s| s.bg(theme.white_06))
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(icon(ic, 16.0, theme.fg_dim))
        .child(label)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------

pub fn render_toast(app: &SpritXApp, message: &str) -> AnyElement {
    let theme = app.theme;
    let toast = div()
        .absolute()
        .bottom(px(30.))
        .left(px(0.))
        .right(px(0.))
        .flex()
        .justify_center()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(14.))
                .py(px(8.))
                .rounded(px(7.))
                .bg(theme.bg_soft)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .child(icon(Icon::Check, 13.0, theme.success))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.fg)
                        .child(message.to_string()),
                ),
        )
        .into_any_element();

    deferred(toast).with_priority(TOAST_Z).into_any_element()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn render_modal(app: &SpritXApp, modal: Modal, cx: &mut Context<SpritXApp>) -> AnyElement {
    match modal {
        Modal::Palette(mode) => render_palette(app, mode, cx),
        Modal::Settings => render_settings(app, cx),
        Modal::Remote => render_remote_dialog(app, cx),
        Modal::CloseConfirm => render_close_confirm(app, cx),
    }
}

// ---------------------------------------------------------------------------
// Command palette / history
// ---------------------------------------------------------------------------

/// Shorten a cwd under `$HOME` to a `~/…` form for display.
fn display_cwd(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return "—".to_string();
    };
    if let Some(home) = crate::backend::files::home_dir() {
        if let Ok(rest) = Path::new(cwd).strip_prefix(&home) {
            let rest = rest.display().to_string();
            return if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rest}")
            };
        }
    }
    cwd.to_string()
}

fn history_age(finished_at: i64) -> String {
    if finished_at <= 0 {
        return "—".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let secs = ((now - finished_at).max(0)) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn history_meta(entry: &HistoryEntry) -> String {
    let branch = entry.branch.as_deref().unwrap_or("—");
    let duration = if entry.duration_ms >= 1000 {
        format!("{:.2}s", entry.duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", entry.duration_ms.max(0))
    };
    format!(
        "{branch} · {} · {duration}",
        history_age(entry.finished_at)
    )
}

fn render_palette(app: &SpritXApp, mode: PaletteMode, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let (sigil, placeholder, title) = match mode {
        PaletteMode::Command => (
            None,
            "Search actions, tabs, themes…",
            "Command palette",
        ),
        PaletteMode::History => (Some("!"), "Search command history…", "Command history"),
    };

    let query_empty = app.palette_query.trim().is_empty();
    let query_display = if query_empty {
        placeholder.to_string()
    } else {
        app.palette_query.clone()
    };

    let mut header = div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(12.))
        .border_b_1()
        .border_color(theme.border);

    header = match sigil {
        Some("!") => header.child(
            div()
                .w(px(16.))
                .flex()
                .justify_center()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(15.))
                .text_color(theme.warning)
                .child("!"),
        ),
        _ => header.child(
            div()
                .w(px(16.))
                .flex()
                .justify_center()
                .child(icon(
                    match mode {
                        PaletteMode::History => Icon::History,
                        PaletteMode::Command => Icon::Search,
                    },
                    16.0,
                    theme.accent,
                )),
        ),
    };

    header = header
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(if query_empty { theme.fg_dim } else { theme.fg })
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(query_display),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.fg_dim)
                        .child(match mode {
                            PaletteMode::Command => "Actions, tabs, hosts and recent commands",
                            PaletteMode::History => "Insert with Enter · run with Shift+Enter",
                        }),
                ),
        )
        .child(esc_chip(theme));

    let mut sections: Vec<AnyElement> = Vec::new();
    let mut history_count = 0usize;

    match mode {
        PaletteMode::Command => {
            sections.push(section_header(theme, "Actions"));
            let rows = app.palette_rows_filtered();
            let selected_position = app.palette_selected.min(rows.len().saturating_sub(1));
            if rows.is_empty() {
                sections.push(empty_state(theme, "No action matches."));
            }
            for (position, (index, (ic, label, shortcut))) in rows.into_iter().enumerate() {
                let selected = position == selected_position;
                let sub = if shortcut == "›" {
                    "Open sub-menu"
                } else if shortcut.is_empty() {
                    "Open remote control"
                } else {
                    "Run action"
                };
                sections.push(palette_action_row(
                    theme,
                    ("palette-row", index),
                    ic,
                    label,
                    sub,
                    shortcut,
                    selected,
                    cx,
                    move |this, cx| this.run_palette_row(index, cx),
                ));
            }
        }
        PaletteMode::History => {
            sections.push(section_header(theme, "Recent commands"));
            let entries: Vec<HistoryEntry> = app
                .history
                .as_ref()
                .map(|store| store.search(&app.palette_query))
                .unwrap_or_default();
            history_count = entries.len();
            let selected_position = app.palette_selected.min(entries.len().saturating_sub(1));
            if entries.is_empty() {
                sections.push(empty_state(
                    theme,
                    if query_empty {
                        "No command history recorded yet."
                    } else {
                        "No command in the history matches."
                    },
                ));
            }
            for (index, entry) in entries.iter().take(200).enumerate() {
                let selected = index == selected_position;
                let command = entry.command.clone();
                let cwd = display_cwd(entry.cwd.as_deref());
                let meta = history_meta(entry);
                let code = entry.exit_code;
                let row = div()
                    .id(("hist", index))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if selected {
                        theme.accent.opacity(0.35)
                    } else {
                        theme.white_04.opacity(0.0)
                    })
                    .when(selected, |d| d.bg(theme.accent.opacity(0.12)))
                    .hover(|s| s.bg(theme.white_06))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let command = command.clone();
                        let sent = this.send_to_terminal(&command, false);
                        dismiss_modal(this, cx);
                        if sent {
                            this.show_toast(format!("Inserted “{command}”"), cx);
                        } else {
                            this.show_toast("No active terminal to insert into", cx);
                        }
                    }))
                    .child(
                        div()
                            .w(px(22.))
                            .flex()
                            .justify_center()
                            .text_size(px(10.5))
                            .text_color(if code == 0 { theme.success } else { theme.danger })
                            .child(if code == 0 {
                                "✓".to_string()
                            } else {
                                code.to_string()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.fg)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(entry.command.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.fg_dim)
                                    .child(format!("{cwd}  ·  {meta}")),
                            ),
                    )
                    .when(selected, |d| d.child(icon(Icon::Enter, 13.0, theme.accent)));
                sections.push(row.into_any_element());
            }
        }
    }

    let list = div()
        .id("palette-list")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .p(px(6.))
        .gap(px(1.))
        .overflow_y_scroll()
        .children(sections);

    let body = match mode {
        PaletteMode::History => footer_row(
            theme,
            hints(vec![
                hint(theme, "↵", "insert"),
                hint(theme, "⇧↵", "run"),
                hint(theme, "Esc", "close"),
            ]),
            div()
                .text_size(px(10.5))
                .text_color(theme.fg_dim)
                .child(format!("{} results", history_count))
                .into_any_element(),
        ),
        PaletteMode::Command => footer_row(
            theme,
            hints(vec![
                hint(theme, "↑↓", "navigate"),
                hint(theme, "↵", "run"),
                hint(theme, "Esc", "close"),
            ]),
            div()
                .text_size(px(10.5))
                .text_color(theme.fg_dim)
                .child(title)
                .into_any_element(),
        ),
    };

    let panel = card(
        theme,
        640.0,
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(list)
            .child(body)
            .into_any_element(),
    );

    modal_layer("palette-modal", theme, panel, cx)
}

#[allow(clippy::too_many_arguments)]
fn palette_action_row(
    theme: Theme,
    id: impl Into<ElementId>,
    ic: Icon,
    label: impl Into<String>,
    sub: &'static str,
    shortcut: &'static str,
    selected: bool,
    cx: &mut Context<SpritXApp>,
    action: impl Fn(&mut SpritXApp, &mut Context<SpritXApp>) + 'static,
) -> AnyElement {
    let mut row = div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.))
        .mx(px(6.))
        .px(px(10.))
        .h(px(36.))
        .rounded(px(6.))
        .border_1()
        .border_color(if selected {
            theme.accent.opacity(0.35)
        } else {
            theme.white_04.opacity(0.0)
        })
        .when(selected, |d| d.bg(theme.accent.opacity(0.12)))
        .hover(|s| s.bg(theme.white_06))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        .child(
            div()
                .w(px(18.))
                .flex()
                .justify_center()
                .child(icon(
                    ic,
                    15.0,
                    if selected { theme.accent } else { theme.fg_dim },
                )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.fg)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.fg_dim)
                        .child(sub),
                ),
        );

    if !shortcut.is_empty() {
        row = row.child(
            div()
                .text_size(px(10.))
                .text_color(theme.fg_dim)
                .child(shortcut),
        );
    }

    row.into_any_element()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

const SETTINGS_SECTIONS: [(SettingsSection, Icon); 9] = [
    (SettingsSection::Appearance, Icon::Appearance),
    (SettingsSection::Shell, Icon::Ssh),
    (SettingsSection::Notifications, Icon::Bell),
    (SettingsSection::FileTree, Icon::Folder),
    (SettingsSection::Remote, Icon::Smartphone),
    (SettingsSection::History, Icon::History),
    (SettingsSection::Keys, Icon::Keyboard),
    (SettingsSection::General, Icon::Settings),
    (SettingsSection::About, Icon::Info),
];

fn render_settings(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let panel_radius = px(12.);

    let mut sidebar = div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(176.))
        .h_full()
        .gap(px(4.))
        .p(px(12.))
        .child(
            div()
                .px(px(8.))
                .pt(px(4.))
                .pb(px(8.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.fg_dim)
                .child("Settings"),
        );

    for (index, (section, ic)) in SETTINGS_SECTIONS.into_iter().enumerate() {
        let selected = app.settings_section == section;
        sidebar = sidebar.child(
            div()
                .id(("settings-nav", index))
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .px(px(8.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if selected { theme.fg } else { theme.fg_dim })
                .when(selected, |d| d.bg(theme.accent.opacity(0.14)).font_weight(FontWeight::MEDIUM))
                .hover(|s| s.bg(theme.white_06).text_color(theme.fg))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.settings_section != section {
                        this.commit_settings_field(cx);
                        if this.settings_field.is_some() {
                            return;
                        }
                        this.settings_section = section;
                        this.settings_menu = None;
                        this.recording_action = None;
                        set_armed_revoke(None);
                    }
                    cx.notify();
                }))
                .child(icon(
                    ic,
                    16.0,
                    if selected { theme.accent } else { theme.fg_dim },
                ))
                .child(section.label()),
        );
    }

    let content = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .bg(theme.bg)
        .rounded(panel_radius)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .px(px(24.))
                .py(px(16.))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(app.settings_section.label()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.fg_dim)
                                .child(settings_summary(app.settings_section)),
                        ),
                )
                .child(
                    div()
                        .id("settings-close")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(32.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(theme.fg_dim)
                        .hover(|s| s.bg(theme.white_06).text_color(theme.fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            dismiss_modal(this, cx);
                        }))
                        .child(icon(Icon::X, 16.0, theme.fg_dim)),
                ),
        )
        .child(
            div()
                .id(("settings-scroll", app.settings_section as usize))
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(24.))
                .child(settings_content(app, cx)),
        );

    let panel = div()
        .flex()
        .flex_row()
        .size_full()
        .p(crate::theme::body_pad())
        .bg(theme.outer)
        .rounded(panel_radius)
        .overflow_hidden()
        .font_family(crate::terminal::appearance().font_family)
        .child(sidebar)
        .child(content)
        .into_any_element();

    side_layer("settings-layer", theme, 860.0, 8.0, panel, cx)
}

fn settings_summary(section: SettingsSection) -> &'static str {
    match section {
        SettingsSection::Appearance => "Workspace font, terminal colors and cursor",
        SettingsSection::Shell => "Executable and arguments for new terminals",
        SettingsSection::Notifications => "Command alerts and completion sound",
        SettingsSection::FileTree => "Commands for files and folders",
        SettingsSection::Remote => "Sharing, pairing and connected devices",
        SettingsSection::History => "Local command history and excluded patterns",
        SettingsSection::Keys => "Keyboard bindings and fixed shortcuts",
        SettingsSection::General => "Workspace behavior and configuration",
        SettingsSection::About => "Version, configuration folder and licenses",
    }
}

fn settings_content(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let section = app.settings_section;
    let mut body = div().flex().flex_col().flex_none().gap(px(8.));

    match section {
        SettingsSection::Appearance => {
            let term_theme = crate::terminal::term_theme();
            let font_label = crate::terminal::appearance().font_family.to_string();
            let size_editing = matches!(app.settings_field, Some(SettingsField::FontSize));
            let size_value = if size_editing {
                format!("{}|", app.settings_buf)
            } else {
                format!("{}", app.settings.font_size as u32)
            };
            let scrollback_editing = matches!(app.settings_field, Some(SettingsField::Scrollback));
            let scrollback_value = if scrollback_editing {
                format!("{}|", app.settings_buf)
            } else {
                app.settings.scrollback.to_string()
            };
            body = body
                .child(appearance_preview(theme, &font_label, app.settings.font_size))
                .child(settings_group(theme, "Typography"))
                .child(setting_row(
                    theme,
                    "Font family",
                    "Applies to the workspace and terminal text",
                    div()
                        .flex()
                        .gap(px(8.))
                        .child({
                            let mut control = div()
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .w(px(260.))
                                .child(select_trigger(
                                    "set-font",
                                    theme,
                                    font_label.clone(),
                                    matches!(app.settings_menu, Some(SettingsMenu::Font)),
                                    cx.listener(|this, _, _, cx| {
                                        this.load_settings_fonts(cx);
                                        this.toggle_settings_menu(SettingsMenu::Font, cx);
                                    }),
                                ));
                            if matches!(app.settings_menu, Some(SettingsMenu::Font)) {
                                control = control.child(font_options(app, theme, cx));
                            }
                            control.into_any_element()
                        })
                        .child(text_button("set-font-import", "Import", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.import_font(cx);
                            }),
                        ))
                        .into_any_element(),
                ))
                .child(setting_row(
                    theme,
                    "Terminal font size",
                    "Terminal text size in pixels (8–40)",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(editable_number(
                            "set-size",
                            theme,
                            size_value,
                            size_editing,
                            72.0,
                            cx.listener(|this, _, _, cx| {
                                this.settings_field = Some(SettingsField::FontSize);
                                this.settings_buf = (this.settings.font_size as u32).to_string();
                                cx.notify();
                            }),
                        ))
                        .child(text_button("set-size-dec", "−", &theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.settings.font_size = (this.settings.font_size - 1.0).max(8.0);
                                // Keep an active inline edit in sync so a later
                                // Enter commits the stepper result, not the
                                // stale buffer captured before the click.
                                if matches!(this.settings_field, Some(SettingsField::FontSize)) {
                                    this.settings_buf = (this.settings.font_size as u32).to_string();
                                }
                                this.save_settings();
                                cx.notify();
                            },
                        )))
                        .child(text_button("set-size-inc", "+", &theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.settings.font_size = (this.settings.font_size + 1.0).min(40.0);
                                if matches!(this.settings_field, Some(SettingsField::FontSize)) {
                                    this.settings_buf = (this.settings.font_size as u32).to_string();
                                }
                                this.save_settings();
                                cx.notify();
                            },
                        )))
                        .into_any_element(),
                ))
                .child(settings_group(theme, "Cursor and scrollback"))
                .child(setting_row(
                    theme,
                    "Cursor style",
                    "Block, bar or underline",
                    {
                        let mut control = div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .w(px(180.))
                            .child(select_trigger(
                                "set-cursor",
                                theme,
                                app.settings.cursor_style,
                                matches!(app.settings_menu, Some(SettingsMenu::CursorStyle)),
                                cx.listener(|this, _, _, cx| {
                                    this.toggle_settings_menu(SettingsMenu::CursorStyle, cx);
                                }),
                            ));
                        if matches!(app.settings_menu, Some(SettingsMenu::CursorStyle)) {
                            control = control.child(cursor_options(app, theme, cx));
                        }
                        control.into_any_element()
                    }
                ))
                .child(setting_row(
                    theme,
                    "Blinking cursor",
                    "Blink the terminal caret",
                    switch(
                        "set-cursor-blink",
                        &theme,
                        app.settings.cursor_blink,
                        cx.listener(|this, _, _, cx| this.toggle_cursor_blink(cx)),
                    )
                    .into_any_element(),
                ))
                .child(setting_row(
                    theme,
                    "Scrollback (lines)",
                    "Kept per terminal (100–100000)",
                    editable_number(
                        "set-scrollback",
                        theme,
                        scrollback_value,
                        scrollback_editing,
                        120.0,
                        cx.listener(|this, _, _, cx| {
                            this.settings_field = Some(SettingsField::Scrollback);
                            this.settings_buf = this.settings.scrollback.to_string();
                            cx.notify();
                        }),
                    ),
                ))
                .child(settings_group(theme, "Terminal theme"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.fg_dim)
                                .child(format!("Selected · {}", app.settings.theme)),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.fg_dim)
                                .child("Terminal palette"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .children(app.theme_names.iter().enumerate().filter_map(|(index, name)| {
                            let name = name.to_string();
                            let name_for_click = name.clone();
                            let selected = index == app.selected_theme;
                            // Skip a name whose preset is missing instead of
                            // panicking if theme_names and PRESETS drift apart.
                            let palette = crate::theme_presets::PRESETS.get(index)?.theme();
                            Some(
                                div()
                                    .id(("theme", index))
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .w(px(136.))
                                    .p(px(8.))
                                    .rounded(px(10.))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(if selected { theme.bg_soft } else { theme.bg })
                                    .hover(|style| style.bg(theme.bg_soft))
                                    .child(div().flex().flex_col().justify_center().gap(px(6.)).h(px(48.)).px(px(8.)).rounded(px(6.)).bg(palette.background).child(div().text_size(px(11.)).text_color(palette.foreground).child("❯ SpritX")).child(div().flex().gap(px(4.)).children(palette.ansi.get(1..7).unwrap_or(&[]).iter().map(|color| div().w(px(10.)).h(px(4.)).rounded(px(2.)).bg(*color)))))
                                    .child(div().flex().items_center().justify_between().gap(px(4.)).text_size(px(11.)).text_color(if selected { theme.fg } else { theme.fg_dim }).child(name).when(selected, |row| row.child(icon(Icon::Check, 12.0, theme.accent))))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.apply_theme_index(index, cx);
                                        this.show_toast(format!("Theme changed to {name_for_click}"), cx);
                                    })),
                            )
                        }))
                )
                .child(settings_group(theme, "Palette"))
                .child(
                    div().flex().flex_wrap().gap(px(12.)).children(
                        [
                            ("Background", ThemeColorKey::Background, term_theme.background),
                            ("Foreground", ThemeColorKey::Foreground, term_theme.foreground),
                            ("Accent", ThemeColorKey::Accent, term_theme.accent),
                            ("Cursor", ThemeColorKey::Cursor, term_theme.cursor),
                            ("Selection", ThemeColorKey::Selection, term_theme.selection),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, key, color))| {
                            let open = app.settings_menu == Some(SettingsMenu::Color(key));
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .w(px(64.))
                                .child(
                                    div()
                                        .id(("color-chip", index))
                                        .h(px(32.))
                                        .rounded(px(5.))
                                        .bg(color)
                                        .border_1()
                                        .border_color(if open { theme.accent } else { theme.border })
                                        .cursor_pointer()
                                        .hover(|style| style.border_color(theme.accent))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_settings_menu(
                                                SettingsMenu::Color(key),
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(theme.fg_dim)
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_size(px(9.))
                                        .text_color(theme.fg_dim)
                                        .child(hsla_to_hex(color)),
                                )
                        }),
                    ),
                )
                .children(color_picker(app, theme, cx))
                .child(
                    div()
                        .id("ansi-toggle")
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .h(px(32.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.bg_soft))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ansi_open = !this.ansi_open;
                            cx.notify();
                        }))
                        .child(icon(
                            if app.ansi_open {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronRight
                            },
                            12.0,
                            theme.fg_dim,
                        ))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg_dim)
                                .child("ANSI colors (16)"),
                        ),
                )
                .children(app.ansi_open.then(|| ansi_grid(theme, &term_theme.ansi)))
                .child(
                    text_button("appearance-reset", "Reset appearance", &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.settings.font_family =
                                crate::backend::config::AppearanceConfig::default().font_family;
                            this.settings.font_size = f32::from(crate::backend::config::DEFAULT_TERMINAL_FONT_SIZE);
                            this.settings.cursor_blink = true;
                            this.settings.cursor_style = "Block";
                            this.settings.scrollback = 5_000;
                            this.settings_menu = None;
                            this.settings_field = None;
                            this.apply_theme_index(0, cx);
                            this.save_settings();
                            this.show_toast("Appearance reset to defaults", cx);
                        }),
                    ),
                );
        }
        SettingsSection::Shell => {
            let (program, program_active) = field_display(app, SettingsField::ShellProgram);
            let (args, args_active) = field_display(app, SettingsField::ShellArgs);
            body = body
                .child(settings_group(theme, "Shell startup"))
                .child(setting_row(
                    theme,
                    "Program",
                    "Shell executable; empty uses the platform default",
                    editable_input(
                        "set-shell-program",
                        theme,
                        program,
                        program_active,
                        320.0,
                        SettingsField::ShellProgram,
                        cx,
                    ),
                ))
                .child(setting_row(
                    theme,
                    "Arguments",
                    r#"JSON array, e.g. ["-NoLogo"]"#,
                    editable_input(
                        "set-shell-args",
                        theme,
                        args,
                        args_active,
                        200.0,
                        SettingsField::ShellArgs,
                        cx,
                    ),
                ))
                .child(note(
                    theme,
                    "Changes apply to new terminals. [] uses shell integration defaults.",
                ));
        }
        SettingsSection::Notifications => {
            body = body
                .child(settings_group(theme, "Command alerts"))
                .child(setting_row(
                    theme,
                    "Desktop completion notifications",
                    "Announce completed commands in the desktop notification center",
                    toggle_control(
                        "set-notify",
                        theme,
                        app.settings.notify_enabled,
                        cx,
                        |this| this.settings.notify_enabled = !this.settings.notify_enabled,
                    ),
                ))
                .child(setting_row(
                    theme,
                    "Minimum duration (s)",
                    "Only notify after this many seconds",
                    {
                        let (value, active) =
                            field_display(app, SettingsField::NotifyMinDuration);
                        editable_input(
                            "set-notify-min",
                            theme,
                            value,
                            active,
                            110.0,
                            SettingsField::NotifyMinDuration,
                            cx,
                        )
                    },
                ))
                .child(setting_row(
                    theme,
                    "Command completion sound",
                    "Play a sound when a terminal command finishes",
                    toggle_control(
                        "set-notify-sound",
                        theme,
                        app.settings.notify_sound,
                        cx,
                        |this| this.settings.notify_sound = !this.settings.notify_sound,
                    ),
                ))
                .child(note(
                    theme,
                    "The completion sound can be turned off independently.",
                ));
        }
        SettingsSection::FileTree => {
            let commands = [
                ("List folder", SettingsField::FileTreeList),
                ("Open folder in editor", SettingsField::FileTreeOpen),
                ("View file", SettingsField::FileTreeView),
                ("Edit file", SettingsField::FileTreeEdit),
                ("Open file in editor", SettingsField::FileTreeOpenFile),
            ];
            body = body.child(settings_group(theme, "File actions")).child(note(
                theme,
                "These commands run from the file context menu. SpritX replaces {path} with the selected path.",
            ));
            for (index, (label, field)) in commands.into_iter().enumerate() {
                let (value, active) = field_display(app, field);
                body = body.child(setting_row(
                    theme,
                    label,
                    "Command to run for this action",
                    editable_input(
                        ("set-ft", index),
                        theme,
                        value,
                        active,
                        320.0,
                        field,
                        cx,
                    ),
                ));
            }
            body = body.child(
                text_button("ft-reset", "Reset commands", &theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.reset_file_tree_commands(cx);
                    },
                )),
            );
        }
        SettingsSection::Remote => {
            let status = app.remote_status();
            let link = status
                .public_url
                .clone()
                .unwrap_or_else(|| status.url.clone());
            body = body
                .child(settings_group(theme, "Connection"))
                .child(setting_row(
                    theme,
                    "Port",
                    "A free port is used if taken",
                    {
                        let (value, active) = field_display(app, SettingsField::RemotePort);
                        editable_input(
                            "set-remote-port",
                            theme,
                            value,
                            active,
                            110.0,
                            SettingsField::RemotePort,
                            cx,
                        )
                    },
                ))
                .child(setting_row(
                    theme,
                    "Internet tunnel automatically",
                    "Open cloudflared when installed",
                    toggle_control(
                        "set-auto-tunnel",
                        theme,
                        app.settings.remote_auto_tunnel,
                        cx,
                        |this| {
                            this.settings.remote_auto_tunnel = !this.settings.remote_auto_tunnel;
                        },
                    ),
                ))
                .child(settings_group(theme, "Sharing status"))
                .child(div().text_size(px(12.)).text_color(theme.fg_dim).child(
                    if status.running {
                        format!(
                            "Sharing on port {} · {} connected",
                            status.port, status.clients
                        )
                    } else {
                        "Not sharing".to_string()
                    },
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(px(8.))
                                .py(px(6.))
                                .rounded(px(5.))
                                .bg(theme.bg_soft)
                                .border_1()
                                .border_color(theme.border)
                                .text_size(px(10.5))
                                .text_color(if status.running { theme.accent } else { theme.fg_dim })
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(if status.running {
                                    link
                                } else {
                                    "stopped".to_string()
                                }),
                        )
                        .child(
                            text_button("remote-copy-settings", "Copy", &theme).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.remote_copy_share(cx);
                                }),
                            ),
                        ),
                )
                .child(settings_group(theme, "Paired devices"))
                .child(remote_devices(theme, app, cx))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            text_button("remote-new-pair", "New pairing", &theme).on_click(
                                cx.listener(|this, _, _, cx| {
                                    // Starting already opens a fresh code, so
                                    // only rotate when a share is live; doing
                                    // both would bury the start error.
                                    if this.remote_status().running {
                                        this.remote_new_pairing(cx);
                                    } else {
                                        this.remote_start(cx);
                                    }
                                }),
                            ),
                        )
                        .children(status.running.then(|| {
                            danger_button(
                                "remote-stop-settings",
                                "Stop remote",
                                &theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remote_stop(cx);
                            }))
                        })),
                )
                .child(note(
                    theme,
                    "The slide-over manages QR pairing and the tunnel.",
                ));
        }
        SettingsSection::History => {
            body = body
                .child(settings_group(theme, "Recording and privacy"))
                .child(setting_row(
                    theme,
                    "Record command history",
                    "Stored locally in history.jsonl",
                    toggle_control(
                        "set-history",
                        theme,
                        app.settings.history_enabled,
                        cx,
                        |this| this.settings.history_enabled = !this.settings.history_enabled,
                    ),
                ))
                .child(setting_row_top(
                    theme,
                    "Never record",
                    "Comma-separated glob patterns, case-insensitive",
                    {
                        let (value, active) =
                            field_display(app, SettingsField::HistoryIgnore);
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(editable_input(
                                "set-history-ignore",
                                theme,
                                value,
                                active,
                                360.0,
                                SettingsField::HistoryIgnore,
                                cx,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(
                                        text_button(
                                            "history-reset-ignore",
                                            "Restore default patterns",
                                            &theme,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.settings.history_ignore =
                                                "*password*, *token*, *secret*, *api_key*".into();
                                            this.refresh_history_ignore();
                                            this.save_settings();
                                            cx.notify();
                                        })),
                                    )
                                    .child(
                                        danger_button("history-clear", "Clear history", &theme)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.clear_history(cx);
                                            })),
                                    ),
                            )
                            .into_any_element()
                    },
                ));
        }
        SettingsSection::Keys => {
            body = body.child(settings_group(theme, "Custom shortcuts"))
                .child(note(theme, "Click a key binding to record a new shortcut."));
            for (index, (action, keys)) in SHORTCUTS.iter().enumerate() {
                let recording = app.recording_action.as_deref() == Some(*action);
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .min_h(px(40.))
                        .gap(px(12.))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().text_size(px(13.)).child(*action))
                        .child(
                            div()
                                .id(("keybind", index))
                                .flex()
                                .items_center()
                                .justify_center()
                                .min_w(px(132.))
                                .h(px(28.))
                                .px(px(8.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(if recording { theme.accent } else { theme.border })
                                .bg(if recording {
                                    theme.accent.opacity(0.14)
                                } else {
                                    theme.bg_soft
                                })
                                .text_size(px(11.))
                                .text_color(if recording { theme.accent } else { theme.fg })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.recording_action =
                                        if this.recording_action.as_deref() == Some(*action) {
                                            None
                                        } else {
                                            Some((*action).to_string())
                                        };
                                    cx.notify();
                                }))
                                .child(if recording {
                                    "Press a key…".to_string()
                                } else {
                                    app.settings
                                        .keybindings
                                        .get(*action)
                                        .cloned()
                                        .unwrap_or_else(|| (*keys).to_string())
                                }),
                        ),
                );
            }
            body = body
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .pt(px(6.))
                        .child(settings_group(theme, "Fixed shortcuts"))
                        .child(
                            text_button("keys-reset", "Reset shortcuts", &theme).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.recording_action = None;
                                    this.settings.keybindings.clear();
                                    this.save_settings();
                                    this.show_toast("Shortcuts reset", cx);
                                }),
                            ),
                        ),
                )
                .child(note(
                    theme,
                    "Fixed: Ctrl+1…9 tabs · Ctrl+/− zoom · Alt+←↑↓→ panes",
                ))
                .child(settings_group(theme, "Process control"))
                .child(note(theme, "Ctrl+C interrupts the foreground process. Ctrl+Shift+C copies selected text."))
                .child(note(theme, "Ctrl+Shift+K force stops this terminal's processes and restarts its shell."))
                .child(note(theme, if cfg!(windows) {
                    "Ctrl+D sends EOF when supported by the app. Windows console readers may use Ctrl+Z followed by Enter."
                } else {
                    "Ctrl+D sends EOF. Ctrl+Z suspends the foreground job; fg resumes it. Ctrl+\\ sends SIGQUIT."
                }));
        }
        SettingsSection::General => {
            body = body
                .child(settings_group(theme, "Workspace recovery"))
                .child(setting_row(
                    theme, "Close window to background",
                    "Keep SpritX available in the system tray. Open it again from its icon or launcher.",
                    toggle_control("set-close-to-tray", theme, app.settings.close_to_tray, cx, |this| {
                        this.settings.close_to_tray = !this.settings.close_to_tray;
                    }),
                ))
                .child(setting_row(
                    theme, "Restore workspace on startup",
                    "Reopen tabs, split layouts, project folders and the active pane",
                    toggle_control("set-restore-workspace", theme, app.settings.restore_workspace, cx, |this| {
                        this.settings.restore_workspace = !this.settings.restore_workspace;
                    }),
                ))
                .child(setting_row(
                    theme, "Recover running sessions after an app restart",
                    "Requires workspace restore. Explicit Quit stops current sessions.",
                    toggle_control("set-keep-sessions", theme, app.settings.keep_sessions, cx, |this| {
                        this.settings.keep_sessions = !this.settings.keep_sessions;
                    }),
                ))
                .child(setting_row(
                    theme, "Save terminal output locally",
                    "Store a bounded output history on this device. Terminal output can contain sensitive data.",
                    toggle_control("set-save-output", theme, app.settings.save_terminal_output, cx, |this| {
                        this.settings.save_terminal_output = !this.settings.save_terminal_output;
                    }),
                ))
                .child(setting_row(
                    theme, "Reduced motion",
                    "Use a static running indicator instead of the animated dots",
                    toggle_control("set-reduced-motion", theme, app.settings.reduced_motion, cx, |this| {
                        this.settings.reduced_motion = !this.settings.reduced_motion;
                    }),
                ))
                .child(
                    text_button("clear-saved-workspace", "Clear saved workspace", &theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.recent_projects.clear();
                            this.settings.restore_workspace = false;
                            this.settings.save_terminal_output = false;
                            this.save_settings();
                            this.checkpoint_workspace();
                            this.show_toast("Saved workspace cleared. Restore and output saving are now off. Current sessions remain open.", cx);
                        })),
                )
                .child(settings_group(theme, "Workspace behavior"))
                .child(setting_row(
                    theme,
                    "Focus pane on hover",
                    "Follow the pointer without clicking",
                    toggle_control("set-focus-hover", theme, app.settings.focus_follows_mouse, cx, |this| {
                        this.settings.focus_follows_mouse = !this.settings.focus_follows_mouse;
                    }),
                ))
                .child(setting_row(
                    theme,
                    "Show git branch",
                    "In each tab header",
                    toggle_control("set-git-branch", theme, app.settings.show_git_branch, cx, |this| {
                        this.settings.show_git_branch = !this.settings.show_git_branch;
                    }),
                ))
                .child(setting_row(
                    theme,
                    "Persist command blocks",
                    "Keep blocks across restarts",
                    toggle_control("set-persist-blocks", theme, app.settings.persist_blocks, cx, |this| {
                        this.settings.persist_blocks = !this.settings.persist_blocks;
                    }),
                ))
                .child(settings_group(theme, "Configuration"))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(text_button("general-export", "Export", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.export_config(cx);
                            }),
                        ))
                        .child(text_button("general-import", "Import", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.import_config(cx);
                            }),
                        )),
                )
                .child(note(
                    theme,
                    "Export a backup of your preferences or import an existing configuration.",
                ))
                .child(text_button("general-quit", "Quit SpritX", &theme).on_click(
                    cx.listener(|this, _, _, cx| this.quit_desktop(cx)),
                ));
        }
        SettingsSection::About => {
            body = body
                .child(settings_group(theme, "Application"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(46.))
                                .rounded(px(10.))
                                .bg(theme.accent.opacity(0.14))
                                .child(icon(Icon::Appearance, 24.0, theme.accent)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .text_size(px(18.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("SpritX"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme.fg_dim)
                                        .child(concat!("Version ", env!("CARGO_PKG_VERSION"))),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme.fg_dim)
                                        .child("MIT license"),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(text_button("about-config", "Open config folder", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.reveal_config(cx);
                            }),
                        )),
                )
                .child(note(
                    theme,
                    "Built with Rust, GPUI 0.2.2 and the SpritX shell integration.",
                ))
                .child(settings_group(theme, "Third-party licenses"))
                .child(note(
                    theme,
                    include_str!("../../assets/icons/LICENSE-Hugeicons.txt"),
                ));
        }
    }

    body.into_any_element()
}

const SHORTCUTS: [(&str, &str); 14] = [
    ("New tab", "Ctrl + Shift + T"),
    ("Close tab", "Ctrl + Shift + W"),
    ("Next tab", "Ctrl + Tab"),
    ("Previous tab", "Ctrl + Shift + Tab"),
    ("Split right", "Ctrl + Shift + D"),
    ("Split down", "Ctrl + Shift + E"),
    ("Toggle panel", "Ctrl + B"),
    ("Search", "Ctrl + F"),
    ("Terminal search", "Ctrl + Shift + F"),
    ("Command palette", "Ctrl + Shift + P"),
    ("Command history", "Ctrl + Shift + H"),
    ("Copy", "Ctrl + Shift + C"),
    ("Paste", "Ctrl + Shift + V"),
    ("Settings", "Ctrl + ,"),
];

fn setting_row(
    theme: Theme,
    label: &'static str,
    description: &'static str,
    control: AnyElement,
) -> AnyElement {
    settings_row_layout(theme, label, description, control)
        .items_center()
        .into_any_element()
}

fn setting_row_top(
    theme: Theme,
    label: &'static str,
    description: &'static str,
    control: AnyElement,
) -> AnyElement {
    settings_row_layout(theme, label, description, control)
        .items_start()
        .into_any_element()
}

fn settings_row_layout(
    theme: Theme,
    label: &'static str,
    description: &'static str,
    control: AnyElement,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .justify_between()
        .gap(px(12.))
        .min_h(px(48.))
        .py(px(8.))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(136.))
                .gap(px(4.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(theme.fg).child(label))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.fg_dim)
                        .child(description),
                ),
        )
        .child(div().flex().flex_none().max_w_full().child(control))
}

fn settings_group(theme: Theme, label: &'static str) -> AnyElement {
    div()
        .pt(px(12.))
        .pb(px(4.))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.fg)
        .child(label)
        .into_any_element()
}

fn appearance_preview(theme: Theme, family: &str, font_size: f32) -> AnyElement {
    let palette = crate::terminal::term_theme();
    let line_height = crate::terminal::term_line_height();
    let cell_width = crate::terminal::cell_width();
    let (cursor_width, cursor_height) = match crate::terminal::appearance().cursor_shape {
        crate::terminal::CursorShape::Bar => (2., line_height),
        crate::terminal::CursorShape::Underline => (cell_width, 2.),
        crate::terminal::CursorShape::Block => (cell_width, line_height),
    };
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_soft)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(12.))
                .py(px(8.))
                .child(div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).child("Terminal preview"))
                .child(
                    div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis()
                        .text_size(px(11.)).text_color(theme.fg_dim)
                        .child(format!("{family} · {font_size:.0}px")),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .mx(px(4.))
                .mb(px(4.))
                .px(px(12.))
                .py(px(12.))
                .rounded(px(6.))
                .bg(palette.background)
                .text_size(px(font_size))
                .line_height(px(line_height))
                .text_color(palette.foreground)
                .child(
                    div().flex().items_center().gap(px(8.))
                        .child(div().text_color(palette.accent).child("❯"))
                        .child("echo \"SpritX ready\""),
                )
                .child("SpritX ready")
                .child(
                    div().flex().items_end().gap(px(8.)).h(px(line_height))
                        .child(div().text_color(palette.accent).child("❯"))
                        .child(div().w(px(cursor_width)).h(px(cursor_height)).bg(palette.cursor)),
                ),
        )
        .into_any_element()
}

/// A select trigger (no popover): the caller expands the option list in flow
/// beneath it, so nothing is drawn with `deferred` inside the deferred modal
/// layer (GPUI 0.2.2 panics on nested deferred draws).
fn select_trigger(
    id: impl Into<ElementId>,
    theme: Theme,
    value: impl Into<String>,
    open: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .w_full()
        .h(px(28.))
        .px(px(9.))
        .rounded(px(6.))
        .bg(theme.bg_soft)
        .border_1()
        .border_color(if open { theme.accent } else { theme.border })
        .text_size(px(11.5))
        .text_color(theme.fg)
        .cursor_pointer()
        .hover(|s| s.border_color(theme.accent))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(value.into()),
        )
        .child(icon(
            if open { Icon::ChevronUp } else { Icon::ChevronDown },
            12.0,
            theme.fg_dim,
        ))
        .into_any_element()
}

/// In-flow option list for the Font picker.
fn font_options(app: &SpritXApp, theme: Theme, cx: &mut Context<SpritXApp>) -> AnyElement {
    let current = crate::terminal::appearance().font_family.to_string();
    let mut list = div()
        .id("font-list")
        .flex()
        .flex_col()
        .gap(px(2.))
        .max_h(px(240.))
        .overflow_y_scroll()
        .p(px(6.))
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border);
    if app.settings_fonts.is_empty() {
        list = list.child(
            div()
                .px(px(8.))
                .py(px(6.))
                .text_size(px(11.))
                .text_color(theme.fg_dim)
                .child("Loading fonts…"),
        );
    }
    for (index, name) in app.settings_fonts.iter().enumerate() {
        let name = name.clone();
        let selected = name == current;
        let picked = name.clone();
        list = list.child(
            div()
                .id(("font-opt", index))
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .h(px(24.))
                .px(px(8.))
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if selected { theme.accent } else { theme.fg })
                .when(selected, |style| style.bg(theme.accent.opacity(0.12)))
                .cursor_pointer()
                .hover(|style| style.bg(theme.white_06))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_font(picked.clone(), cx);
                }))
                .child(name),
        );
    }
    // Let the list process the wheel first, then stop it before the
    // surrounding Appearance page gets the same event.
    div()
        .id("font-scroll-boundary")
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(list)
        .into_any_element()
}

/// In-flow option list for the cursor-shape picker.
fn cursor_options(app: &SpritXApp, theme: Theme, cx: &mut Context<SpritXApp>) -> AnyElement {
    let mut list = div()
        .id("cursor-list")
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(6.))
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border);
    for (index, label) in ["Block", "Bar", "Underline"].into_iter().enumerate() {
        let selected = app.settings.cursor_style == label;
        list = list.child(
            div()
                .id(("cursor-opt", index))
                .flex()
                .items_center()
                .h(px(26.))
                .px(px(8.))
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(if selected { theme.accent } else { theme.fg })
                .when(selected, |style| style.bg(theme.accent.opacity(0.12)))
                .cursor_pointer()
                .hover(|style| style.bg(theme.white_06))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_cursor_style(label, cx);
                }))
                .child(label),
        );
    }
    list.into_any_element()
}

/// The display value + active flag for an inline Settings field.
fn field_display(app: &SpritXApp, field: SettingsField) -> (String, bool) {
    let active = app.settings_field == Some(field);
    let value = if active {
        format!("{}|", app.settings_buf)
    } else {
        field.current(&app.settings)
    };
    (value, active)
}

/// A left-aligned, inline-editable single-line text field.
fn editable_input(
    id: impl Into<ElementId>,
    theme: Theme,
    value: impl Into<String>,
    active: bool,
    width: f32,
    field: SettingsField,
    cx: &mut Context<SpritXApp>,
) -> AnyElement {
    let on_click = cx.listener(move |this, _, _, cx| {
        this.begin_settings_edit(field);
        cx.notify();
    });
    div()
        .id(id)
        .flex()
        .items_center()
        .w(px(width))
        .h(px(28.))
        .px(px(9.))
        .rounded(px(6.))
        .bg(theme.bg_soft)
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .text_size(px(11.5))
        .text_color(theme.fg)
        .cursor_text()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .on_click(on_click)
        .child(value.into())
        .into_any_element()
}

/// A numeric field that becomes editable on click (buffer lives on `SpritXApp`).
fn editable_number(
    id: impl Into<ElementId>,
    theme: Theme,
    value: impl Into<String>,
    active: bool,
    width: f32,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_end()
        .w(px(width))
        .h(px(28.))
        .px(px(9.))
        .rounded(px(6.))
        .bg(theme.bg_soft)
        .border_1()
        .border_color(if active { theme.accent } else { theme.border })
        .text_size(px(11.5))
        .text_color(theme.fg)
        .cursor_text()
        .overflow_hidden()
        .whitespace_nowrap()
        .on_click(on_click)
        .child(value.into())
        .into_any_element()
}

/// Color-picker popover for the five primary terminal colors.
fn color_picker(
    app: &SpritXApp,
    theme: Theme,
    cx: &mut Context<SpritXApp>,
) -> Option<AnyElement> {
    let menu = app.settings_menu?;
    let SettingsMenu::Color(key) = menu else {
        return None;
    };
    const PALETTE: [u32; 24] = [
        0x0e1014, 0x161922, 0x20242e, 0x2c303a, 0x525866, 0x8a8f9b, 0xe6e6e6, 0xffffff,
        0xff5c8d, 0xc678dd, 0x4ea1ff, 0x56b6c2, 0x56d364, 0xe3b341, 0xff7ba0, 0xd49eea,
        0x73b5ff, 0x7fc8d3, 0x7ce081, 0xf0c674, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9,
    ];
    let label = match key {
        ThemeColorKey::Background => "Background",
        ThemeColorKey::Foreground => "Foreground",
        ThemeColorKey::Accent => "Accent",
        ThemeColorKey::Cursor => "Cursor",
        ThemeColorKey::Selection => "Selection",
    };
    let mut grid = div()
        .flex()
        .flex_wrap()
        .gap(px(6.))
        .p(px(8.))
        .rounded(px(8.))
        .bg(theme.bg)
        .border_1()
        .border_color(theme.border);
    for (index, value) in PALETTE.into_iter().enumerate() {
        grid = grid.child(
            div()
                .id(("color-swatch", index))
                .size(px(22.))
                .rounded(px(4.))
                .bg(gpui::rgb(value))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|style| style.border_color(theme.accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_theme_color(key, value, cx);
                })),
        );
    }
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.fg_dim)
                    .child(format!("{label} — choose a color")),
            )
            .child(grid)
            .into_any_element(),
    )
}

fn toggle_control(
    id: impl Into<ElementId>,
    theme: Theme,
    on: bool,
    cx: &mut Context<SpritXApp>,
    toggle: fn(&mut SpritXApp),
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .w(px(38.))
        .h(px(22.))
        .p(px(2.))
        .rounded_full()
        .bg(if on { theme.accent } else { theme.white_16 })
        .border_1()
        .border_color(if on { theme.accent } else { theme.border })
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            toggle(this);
            this.save_settings();
            cx.notify();
        }))
        .child(
            div()
                .ml(if on { px(16.) } else { px(0.) })
                .size(px(16.))
                .rounded_full()
                .bg(theme.on_accent)
                .shadow_sm(),
        )
        .into_any_element()
}

fn note(theme: Theme, text: &'static str) -> AnyElement {
    div()
        .text_size(px(11.5))
        .text_color(theme.fg_dim)
        .child(text)
        .into_any_element()
}

fn ansi_grid(theme: Theme, ansi: &[gpui::Hsla; 16]) -> AnyElement {
    const LABELS: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "br black",
        "br red", "br green", "br yellow", "br blue", "br magenta", "br cyan", "br white",
    ];

    let mut grid = div().flex().flex_wrap().gap(px(8.));
    for (index, label) in LABELS.iter().enumerate() {
        // Skip a swatch whose color is missing rather than panicking if the
        // ANSI array and labels drift apart.
        let Some(color) = ansi.get(index) else { continue };
        grid = grid.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .w(px(118.))
                .child(
                    div()
                        .size(px(18.))
                        .rounded(px(4.))
                        .bg(*color)
                        .border_1()
                        .border_color(theme.border),
                )
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(theme.fg_dim)
                        .child(*label),
                ),
        );
    }
    grid.into_any_element()
}

fn remote_devices(theme: Theme, app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let status = app.remote_status();
    if status.devices.is_empty() {
        return div()
            .px(px(10.))
            .py(px(14.))
            .rounded(px(6.))
            .bg(theme.bg_soft)
            .border_1()
            .border_color(theme.border)
            .text_size(px(11.5))
            .text_color(theme.fg_dim)
            .child(if status.running {
                "No paired device yet."
            } else {
                "No paired device yet — pairing happens while sharing is on."
            })
            .into_any_element();
    }

    let mut list = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(4.))
        .rounded(px(8.))
        .bg(theme.bg_soft)
        .border_1()
        .border_color(theme.border);

    for device in &status.devices {
        let live = device.connections > 0;
        let armed = armed_revoke().as_deref() == Some(device.id.as_str());
        let subtitle = if live {
            format!("connected · {} session(s)", device.connections)
        } else {
            match device.last_seen {
                Some(seen) => format!("last seen {}", ago_label(seen)),
                None => "paired, never connected".to_string(),
            }
        };
        let id = device.id.clone();
        list = list.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(8.))
                .py(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(9.))
                        .child(dot(if live { theme.success } else { theme.fg_dim }, 7.0))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(1.))
                                .child(div().text_size(px(12.0)).child(device.name.clone()))
                                .child(
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(theme.fg_dim)
                                        .child(subtitle),
                                ),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "remote-revoke-{}",
                            device.id
                        )))
                        .flex()
                        .items_center()
                        .h(px(22.))
                        .px(px(8.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if armed { theme.danger } else { theme.border })
                        .bg(if armed {
                            theme.danger.opacity(0.16)
                        } else {
                            theme.white_04
                        })
                        .text_size(px(10.5))
                        .text_color(if armed { theme.danger } else { theme.fg_dim })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if armed_revoke().as_deref() == Some(id.as_str()) {
                                set_armed_revoke(None);
                                this.remote_revoke(&id, cx);
                            } else {
                                // Arm only the view-local confirm state; never
                                // touch recording_action, which the root key
                                // handler would treat as keybinding capture.
                                set_armed_revoke(Some(id.clone()));
                                cx.notify();
                            }
                        }))
                        .child(if armed { "Confirm" } else { "Revoke" }),
                ),
        );
    }
    list.into_any_element()
}

/// "just now / 4 min ago / 3 h ago / 12 d ago" for a millisecond timestamp.
fn ago_label(milliseconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    let delta = (now - milliseconds).max(0);
    let seconds = delta / 1000;
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h ago", seconds / 3600)
    } else {
        format!("{} d ago", seconds / 86_400)
    }
}

// ---------------------------------------------------------------------------
// Remote slide-over
// ---------------------------------------------------------------------------

/// The module matrix behind a pairing QR: side length plus row-major dark
/// flags. Split from the renderer so it can be tested without a window.
///
/// `None` when the URL is empty or cannot be encoded, so callers degrade to the
/// link alone rather than showing something a camera cannot read.
pub fn qr_modules(url: &str) -> Option<(usize, Vec<bool>)> {
    if url.is_empty() {
        return None;
    }
    // Default error correction (level M) — enough for a code shown on screen.
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let width = code.width();
    Some((
        width,
        code.to_colors()
            .iter()
            .map(|color| *color == qrcode::Color::Dark)
            .collect(),
    ))
}

/// A real, scannable QR of the pairing URL.
///
/// Same-colour modules in a row are merged into one node: a version-4 symbol is
/// 33x33 cells, and emitting a div per cell would rebuild ~1100 nodes on every
/// repaint of the panel.
fn qr_code(url: &str, _theme: Theme) -> Option<AnyElement> {
    let (width, dark) = qr_modules(url)?;
    let module = 5.0f32;
    let dark_colour = rgb(0x0e1014);
    let light = rgb(0xffffff);

    let mut grid = div().flex().flex_col();
    for row in 0..width {
        let mut line = div().flex();
        let mut col = 0usize;
        while col < width {
            let is_dark = dark[row * width + col];
            let mut run = 1usize;
            while col + run < width && dark[row * width + col + run] == is_dark {
                run += 1;
            }
            line = line.child(
                div()
                    .w(px(run as f32 * module))
                    .h(px(module))
                    .bg(if is_dark { dark_colour } else { light }),
            );
            col += run;
        }
        grid = grid.child(line);
    }

    Some(
        div()
            .flex()
            .p(px(module * 2.0))
            .bg(light)
            .child(grid)
            .into_any_element(),
    )
}

fn render_remote_dialog(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;
    let status = app.remote_status();
    let running = status.running;
    let connected = status.clients > 0;
    let pairing = running && status.pair_url.is_some();
    let tunnel_up = status.public_url.is_some();
    let tunnel_pending = running && status.tunnel_requested && !tunnel_up;
    let pair_link = status
        .public_pair_url
        .clone()
        .or_else(|| status.pair_url.clone())
        .unwrap_or_default();
    let share_link = status
        .public_url
        .clone()
        .unwrap_or_else(|| status.url.clone());

    let head = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .px(px(16.))
        .py(px(12.))
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(icon(Icon::Smartphone, 16.0, theme.fg_dim))
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Remote control"),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(esc_chip(theme))
                .child(
                    div()
                        .id("remote-dialog-close")
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(theme.fg_dim)
                        .hover(|s| s.bg(theme.white_06).text_color(theme.fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            dismiss_modal(this, cx);
                        }))
                        .child(icon(Icon::X, 16.0, theme.fg_dim)),
                ),
        );

    let badge_text = if !running {
        "Not sharing".to_string()
    } else if connected {
        format!("{} connected", status.clients)
    } else {
        format!(
            "Sharing on port {}",
            status.port
        )
    };

    let mut body = div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(16.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .p(px(12.))
                .rounded(px(8.))
                .bg(theme.bg_soft)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(dot(
                            if connected {
                                theme.success
                            } else if running {
                                theme.warning
                            } else {
                                theme.fg_dim
                            },
                            7.0,
                        ))
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(theme.fg)
                                .child(badge_text),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.fg_dim)
                        .child("End-to-end encryption"),
                ),
        );

    if !running {
        body = body
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.fg_dim)
                    .child("Start sharing to pair a device and control this workspace remotely."),
            )
            .child(
                primary_button("remote-dialog-start", "Start sharing", &theme).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.remote_start(cx);
                    }),
                ),
            );
    } else if tunnel_pending {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .p(px(16.))
                .rounded(px(8.))
                .bg(theme.bg_soft)
                .border_1()
                .border_color(theme.border)
                .child(dot(theme.warning, 10.0))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.fg)
                        .child("Creating public tunnel…"),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.fg_dim)
                        .child("This usually takes a few seconds"),
                ),
        );
    } else {
        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.fg_dim)
                        .child("PAIR A DEVICE"),
                )
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(theme.fg_dim)
                        .child(match status.pairing_expires_in {
                            Some(seconds) => format!("single use · {}s left", seconds),
                            None => "single use · 10 min".to_string(),
                        }),
                ),
        );

        if pairing {
            body = body
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(10.))
                        .p(px(14.))
                        .rounded(px(8.))
                        .bg(theme.bg_soft)
                        .border_1()
                        .border_color(theme.border)
                        .children(qr_code(&pair_link, theme))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.fg_dim)
                                .child("Scan this code, or open the link below, on the other device"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(px(8.))
                                .py(px(6.))
                                .rounded(px(5.))
                                .bg(theme.bg_soft)
                                .border_1()
                                .border_color(theme.border)
                                .text_size(px(10.))
                                .text_color(theme.accent)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(pair_link.clone()),
                        )
                        .child(
                            text_button("remote-pair-copy", "Copy", &theme).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.remote_copy_pairing(cx);
                                }),
                            ),
                        ),
                );
        } else {
            body = body
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.fg_dim)
                        .child("The previous pairing code expired or was used. Generate a fresh one."),
                )
                .child(
                    text_button("remote-new-qr", "New QR code", &theme).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.remote_new_pairing(cx);
                        },
                    )),
                );
        }

        body = body.child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.fg_dim)
                .child("PAIRED DEVICES"),
        );

        body = body
            .child(settings_group(theme, "Paired devices"))
            .child(remote_devices(theme, app, cx));

        if connected || pairing {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(5.))
                            .bg(theme.bg_soft)
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(10.))
                            .text_color(theme.accent)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(share_link.clone()),
                    )
                    .child(
                        text_button("remote-share-copy", "Copy", &theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.remote_copy_share(cx);
                            },
                        )),
                    ),
            );
        }

        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::Check, 11.0, theme.success))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.success)
                        .child(if tunnel_up {
                            "End-to-end encrypted · public tunnel active"
                        } else {
                            "End-to-end encrypted · single-use pairing"
                        }),
                ),
        );

        if let Some(error) = &status.last_error {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon(Icon::Warning, 11.0, theme.danger))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.danger)
                            .child(error.clone()),
                    ),
            );
        }

        if !status.tunnel_requested {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(10.))
                    .rounded(px(7.))
                    .bg(theme.bg_soft)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.fg)
                            .child("Enable the Internet tunnel"),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.fg_dim)
                            .child(if status.tunnel_available {
                                "Share outside your LAN through a cloudflared quick tunnel."
                            } else {
                                "cloudflared is not installed — share on your LAN, or install it and restart SpritX."
                            }),
                    )
                    .child(
                        text_button("remote-enable-tunnel", "Start the tunnel", &theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.remote_tunnel(cx);
                            }),
                        ),
                    ),
            );
        }
    }

    // Lumas this machine is driving, so the links are visible and stoppable
    // rather than only inferable from a tab title.
    if !app.remote_links.is_empty() {
        let mut links = div().flex().flex_col().gap(px(4.));
        for (leaf, link) in app.remote_links.iter() {
            let leaf = *leaf;
            let peer = link
                .server_name()
                .unwrap_or_else(|| "another SpritX".to_string());
            let note = link.error().unwrap_or_else(|| "connected".to_string());
            links = links.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(dot(
                        if link.is_online() {
                            theme.success
                        } else {
                            theme.warning
                        },
                        6.0,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.5))
                            .text_color(theme.fg)
                            .child(peer),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.fg_dim)
                            .child(note),
                    )
                    .child(
                        text_button(
                            SharedString::from(format!("remote-link-close-{leaf}")),
                            "Disconnect",
                            &theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remote_disconnect(leaf, cx);
                        })),
                    ),
            );
        }
        body = body
            .child(section_header(theme, "SpritX you drive"))
            .child(links);
    }

    if running {
        body = body.child(
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(icon(Icon::Warning, 12.0, theme.warning))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.fg_dim)
                        .child("Share pairing links only with people you trust. Revoke access when a device is no longer needed."),
                ),
        );
    }

    let panel = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(head)
        .child(
            div()
                .id("remote-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(16.))
                .child(body),
        )
        .when(running, |panel| {
            panel.child(
                div()
                    .flex_none()
                    .p(px(16.))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        danger_button("remote-dialog-stop", "Stop sharing", &theme)
                            .w_full()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remote_stop(cx);
                                dismiss_modal(this, cx);
                            })),
                    ),
            )
        })
        .into_any_element();

    side_layer("remote-layer", theme, 364.0, 0.0, panel, cx)
}

// ---------------------------------------------------------------------------
// Close confirmation
// ---------------------------------------------------------------------------

fn render_close_confirm(app: &SpritXApp, cx: &mut Context<SpritXApp>) -> AnyElement {
    let theme = app.theme;

    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(9.))
                .px(px(16.))
                .py(px(13.))
                .border_b_1()
                .border_color(theme.border)
                .child(icon(Icon::Warning, 18.0, theme.warning))
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Close this tab?"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(16.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.fg_dim)
                        .child("A command may still be running in this tab. Closing it will terminate its process group."),
                ),
        )
        .child(footer_row(
            theme,
            hints(vec![hint(theme, "Esc", "cancel"), hint(theme, "↵", "close")]),
            div()
                .flex()
                .gap(px(8.))
                .child(text_button("close-cancel", "Cancel", &theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        dismiss_modal(this, cx);
                    },
                )))
                .child(
                    danger_button("close-confirm", "Close anyway", &theme).on_click(cx.listener(
                        |this, _, _, cx| {
                            dismiss_modal(this, cx);
                            let index = this.active_tab;
                            this.close_tab(index, cx);
                        },
                    )),
                )
                .into_any_element(),
        ))
        .into_any_element();

    modal_layer(
        "close-confirm-modal",
        theme,
        card(theme, 470.0, content),
        cx,
    )
}

