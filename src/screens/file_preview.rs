//! Read-only file viewer, owned by a stable terminal leaf rather than a cwd.
use crate::{
    backend::file_preview::Content,
    components::{
        buttons::icon_button,
        icons::{icon, Icon},
    },
    screens::app_shell::SpritXApp,
};
use gpui::{
    div, prelude::*, px, relative, uniform_list, AnimationExt, AnyElement, Context, IntoElement,
    MouseButton, ScrollHandle, SharedString, UniformListScrollHandle,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

const MOTION: Duration = Duration::from_millis(210);

pub(super) struct Preview {
    path: PathBuf,
    request: u64,
    closing: bool,
    content: ViewContent,
    scroll: UniformListScrollHandle,
    horizontal: ScrollHandle,
    #[cfg(debug_assertions)]
    bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
}

enum ViewContent {
    Loading,
    Text {
        original: Arc<String>,
        lines: Arc<Vec<String>>,
        truncated: bool,
    },
    Image {
        image: Arc<gpui::Image>,
        width: u32,
        height: u32,
    },
    Message(String),
}

impl SpritXApp {
    pub(super) fn file_preview_has_focus(&self) -> bool {
        self.file_preview_focused.is_some_and(|leaf| {
            self.active_tab_checked()
                .is_some_and(|tab| tab.active_leaf == leaf)
                && self
                    .file_previews
                    .get(&leaf)
                    .is_some_and(|preview| !preview.closing)
        })
    }

    pub(super) fn open_file_preview(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(leaf) = self.active_tab_checked().map(|tab| tab.active_leaf) else {
            self.show_toast("Open a terminal to preview this file", cx);
            return;
        };
        self.next_file_preview_request = self.next_file_preview_request.wrapping_add(1);
        let request = self.next_file_preview_request;
        self.file_previews.insert(
            leaf,
            Preview {
                path: path.clone(),
                request,
                closing: false,
                content: ViewContent::Loading,
                scroll: UniformListScrollHandle::new(),
                horizontal: ScrollHandle::new(),
                #[cfg(debug_assertions)]
                bounds: Default::default(),
            },
        );
        self.file_preview_focused = Some(leaf);
        let loading = cx
            .background_executor()
            .spawn(async move { crate::backend::file_preview::load(&path) });
        cx.spawn(async move |this, cx| {
            let result = loading.await;
            let _ = this.update(cx, |app, cx| {
                let Some(preview) = app.file_previews.get_mut(&leaf) else {
                    return;
                };
                if preview.request != request || preview.closing {
                    return;
                }
                preview.content = match result {
                    Ok(Content::Text { text, truncated }) => ViewContent::Text {
                        lines: Arc::new(if text.is_empty() {
                            vec![String::new()]
                        } else {
                            text.split('\n')
                                .map(|line| line.trim_end_matches('\r').replace('\t', "    "))
                                .collect()
                        }),
                        original: Arc::new(text),
                        truncated,
                    },
                    Ok(Content::Image { png, width, height }) => ViewContent::Image {
                        image: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)),
                        width,
                        height,
                    },
                    Ok(Content::Unsupported) => ViewContent::Message(
                        "This file is binary; a text preview is unavailable".into(),
                    ),
                    Err(error) => {
                        ViewContent::Message(format!("Could not preview this file: {error}"))
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn close_file_preview(&mut self, leaf: u64, cx: &mut Context<Self>) {
        let Some(preview) = self.file_previews.get_mut(&leaf) else {
            return;
        };
        if preview.closing {
            return;
        }
        if self.settings.reduced_motion {
            self.file_previews.remove(&leaf);
        } else {
            preview.closing = true;
            let request = preview.request;
            cx.spawn(async move |this, cx| {
                gpui::Timer::after(MOTION).await;
                let _ = this.update(cx, |app, cx| {
                    if app
                        .file_previews
                        .get(&leaf)
                        .is_some_and(|p| p.request == request && p.closing)
                    {
                        app.file_previews.remove(&leaf);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        if self.file_preview_focused == Some(leaf) {
            self.file_preview_focused = None;
        }
        cx.notify();
    }

    pub(super) fn copy_file_preview(&mut self, leaf: u64, cx: &mut Context<Self>) {
        if let Some(Preview {
            content: ViewContent::Text { original, .. },
            ..
        }) = self.file_previews.get(&leaf)
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                original.as_str().to_string(),
            ));
            self.show_toast("Preview copied", cx);
        }
    }
}

pub(super) fn render(app: &SpritXApp, leaf: u64, cx: &mut Context<SpritXApp>) -> AnyElement {
    let Some(preview) = app.file_previews.get(&leaf) else {
        return div().into_any_element();
    };
    let theme = app.theme;
    let request = preview.request;
    let closing = preview.closing;
    let filename = preview
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let header = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .h(px(40.))
        .px(px(12.))
        .border_b_1()
        .border_color(theme.border)
        .child(icon(Icon::File, 16., theme.fg_dim))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(12.))
                .text_color(theme.fg)
                .child(filename),
        )
        .when(
            matches!(preview.content, ViewContent::Text { .. }),
            |header| {
                header.child(
                    icon_button(("preview-copy", leaf as usize), Icon::Copy, &theme, false)
                        .on_click(
                            cx.listener(move |app, _, _, cx| app.copy_file_preview(leaf, cx)),
                        ),
                )
            },
        )
        .child(
            icon_button(("preview-close", leaf as usize), Icon::X, &theme, false)
                .on_click(cx.listener(move |app, _, _, cx| app.close_file_preview(leaf, cx))),
        );
    let (body, status): (AnyElement, String) = match &preview.content {
        ViewContent::Loading => (message(theme, "Loading file…"), "Read only".into()),
        ViewContent::Message(text) => (message(theme, text), "Read only".into()),
        ViewContent::Image {
            image,
            width,
            height,
        } => (
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .p(px(12.))
                .child(
                    gpui::img(image.clone())
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .into_any_element(),
            format!("{width} × {height} · Read only"),
        ),
        ViewContent::Text {
            lines, truncated, ..
        } => {
            let lines = lines.clone();
            let count = lines.len();
            let width = lines
                .iter()
                .map(|line| line.chars().count().min(4096))
                .max()
                .unwrap_or(0) as f32
                * 8.0
                + 64.0;
            let list = uniform_list(
                ("preview-lines", leaf as usize),
                count,
                move |range, _, _| {
                    range
                        .map(|index| {
                            let text: String = lines[index].chars().take(4096).collect();
                            div()
                                .flex()
                                .items_center()
                                .h(px(20.))
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(44.))
                                        .pr(px(12.))
                                        .text_align(gpui::TextAlign::Right)
                                        .text_color(theme.fg_dim)
                                        .child((index + 1).to_string()),
                                )
                                .child(div().whitespace_nowrap().text_color(theme.fg).child(text))
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(preview.scroll.clone())
            .h_full()
            .min_w(px(width));
            let body = div()
                .id(("preview-code-scroll", leaf as usize))
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_x_scroll()
                .track_scroll(&preview.horizontal)
                .py(px(8.))
                .child(list)
                .into_any_element();
            (
                body,
                if *truncated {
                    format!("{count} lines · First 64 KB · Read only")
                } else {
                    format!("{count} lines · Read only")
                },
            )
        }
    };
    let panel = div()
        .id(("file-preview", leaf as usize))
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w_full()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .bg(theme.bg)
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |app, _, _, cx| {
                app.focus_leaf(leaf, cx);
                app.file_preview_focused = Some(leaf);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(header)
        .child(
            div()
                .flex_none()
                .px(px(12.))
                .py(px(6.))
                .text_size(px(10.))
                .text_color(theme.fg_dim)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(preview.path.to_string_lossy().into_owned()),
        )
        .child(body)
        .child(
            div()
                .flex_none()
                .border_t_1()
                .border_color(theme.border)
                .px(px(12.))
                .py(px(8.))
                .text_size(px(10.))
                .text_color(theme.fg_dim)
                .child(status),
        );
    #[cfg(debug_assertions)]
    let panel = {
        let bounds = preview.bounds.clone();
        panel.child(
            gpui::canvas(move |rect, _, _| bounds.set(Some(rect)), |_, _, _, _| {})
                .absolute()
                .inset_0(),
        )
    };
    let panel = if app.settings.reduced_motion {
        panel.into_any_element()
    } else {
        panel
            .with_animation(
                SharedString::from(format!("preview-motion-{leaf}-{request}-{closing}")),
                gpui::Animation::new(MOTION).with_easing(gpui::ease_out_quint()),
                move |panel, progress| {
                    panel
                        .right(relative(if closing { -progress } else { progress - 1. }))
                        .opacity(if closing { 1. - progress } else { progress })
                },
            )
            .into_any_element()
    };
    // The terminal resizes once when the slot is inserted/removed; the sliding
    // content never changes its PTY geometry on every animation frame.
    div()
        .relative()
        .flex_none()
        .w(relative(0.5))
        .max_w(px(560.))
        .min_w_0()
        .h_full()
        .ml(px(8.))
        .overflow_hidden()
        .child(panel)
        .into_any_element()
}

fn message(theme: crate::theme::Theme, text: &str) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .p(px(16.))
        .text_size(px(12.))
        .text_color(theme.fg_dim)
        .child(text.to_string())
        .into_any_element()
}

#[cfg(debug_assertions)]
pub fn run_test(window: gpui::WindowHandle<SpritXApp>, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        let root = PathBuf::from(std::env::var("SPRITX_PREVIEW_FIXTURE").unwrap());
        let (first, second) = window.update(cx, |app, _, cx| {
            app.settings.reduced_motion = false;
            let first = app.active_tab().active_leaf;
            let cwd = app.active_tab().active_leaf_state().unwrap().cwd.clone();
            app.add_tab(cx);
            let second = app.active_tab().active_leaf;
            app.active_tab_mut().leaves.get_mut(&second).unwrap().cwd = cwd;
            app.open_file_preview(root.join("first.rs"), cx);
            assert!(!app.file_previews.contains_key(&first));
            assert!(app.file_previews.contains_key(&second));
            app.close_file_preview(second, cx);
            // Reopening while a close/load is pending must defeat both stale completions.
            app.open_file_preview(root.join("final file.rs"), cx);
            assert!(app.file_preview_has_focus());
            assert!(!app.terminal_has_focus());
            (first, second)
        }).unwrap();
        let mut positions = Vec::new();
        for _ in 0..10 {
            gpui::Timer::after(Duration::from_millis(30)).await;
            if let Some(bounds) = window.update(cx, |app, _, _| app.file_previews[&second].bounds.get()).unwrap() {
                assert!(f32::from(bounds.size.width) > 20. && f32::from(bounds.size.height) > 20.);
                positions.push(f32::from(bounds.origin.x));
            }
        }
        eprintln!("[file-preview] slide_x={positions:?}");
        assert!(positions.windows(2).any(|p| p[0] > p[1] + 1.), "preview must slide through actual layout positions");
        window.update(cx, |app, _, cx| {
            assert!(matches!(&app.file_previews[&second].content,
                ViewContent::Text { original, .. } if original.as_str() == "fn final_preview() {}\n"));
            app.activate_tab(0, cx);
            assert_eq!(app.active_tab().active_leaf, first);
            assert!(!app.file_previews.contains_key(&first));
            assert!(!app.file_preview_has_focus());
            app.open_file_preview(root.join("first.rs"), cx);
            assert_eq!(app.file_previews.len(), 2);
        }).unwrap();
        gpui::Timer::after(Duration::from_millis(260)).await;
        window.update(cx, |app, _, cx| {
            app.settings.reduced_motion = true;
            app.close_file_preview(first, cx);
            assert!(!app.file_previews.contains_key(&first));
            assert!(app.file_previews.contains_key(&second));
            app.activate_tab(1, cx);
            app.open_file_preview(root.join("picture.png"), cx);
        }).unwrap();
        gpui::Timer::after(Duration::from_millis(260)).await;
        window.update(cx, |app, _, cx| {
            assert!(matches!(app.file_previews[&second].content, ViewContent::Image { .. }));
            app.settings.reduced_motion = false;
            cx.notify();
        }).unwrap();
        let mut ready = false;
        for _ in 0..200 {
            gpui::Timer::after(Duration::from_millis(50)).await;
            ready = window.update(cx, |app, _, _| {
                app.pump_terminals();
                app.session(second).is_some_and(|s| s.powerline_prompt_end().is_some())
            }).unwrap();
            if ready { break; }
        }
        assert!(ready, "shell must be ready for right-click path insertion");
        let completion = window.update(cx, |app, _, cx| {
            let completion = app.active_leaf().unwrap().activity.completed;
            assert!(super::file_tree::insert_tree_path(app, root.join("final file.rs").to_str().unwrap(), cx));
            assert!(!app.file_preview_has_focus());
            completion
        }).unwrap();
        gpui::Timer::after(Duration::from_millis(250)).await;
        window.update(cx, |app, _, cx| {
            app.pump_terminals();
            assert!(app.session(second).unwrap().screen_text().replace(['\n', '\r'], "").contains("final file.rs"));
            assert_eq!(app.active_leaf().unwrap().activity.completed, completion, "path paste must not execute a command");
            assert!(!app.active_leaf().unwrap().activity.running);
            app.close_file_preview(second, cx);
        }).unwrap();
        gpui::Timer::after(Duration::from_millis(300)).await;
        window.update(cx, |app, _, _| {
            assert!(app.file_previews.is_empty());
            assert!(app.session(first).is_some());
            assert!(app.session(second).is_some());
        }).unwrap();
        cx.update(|cx| cx.quit()).unwrap();
    }).detach();
}
