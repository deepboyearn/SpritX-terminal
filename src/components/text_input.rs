//! Single-line text input widget for the SpritX GPUI port.
//!
//! [`TextInput`] is a self-contained `Entity` that implements GPUI's
//! [`EntityInputHandler`], so it receives platform IME / compose events and
//! supports the usual selection, clipboard and navigation keys.
//!
//! # Usage
//!
//! ```ignore
//! use gpui::prelude::*;
//! use crate::components::text_input::{TextInput, TextInputStyle};
//! use crate::theme::Theme;
//!
//! let theme = Theme::dark();
//! let input = cx.new(|cx| {
//!     TextInput::new(
//!         "Search blocks…",
//!         TextInputStyle::from_theme(&theme).mono(),
//!         window,
//!         cx,
//!     )
//!     .on_change(|value, _window, _cx| println!("changed: {value}"))
//!     .on_submit(|value, _window, _cx| println!("submitted: {value}"))
//!     .on_escape(|_window, _cx| println!("escape"))
//! });
//!
//! // Render it like any other view entity:
//! // .child(input.clone())
//!
//! // Focus it from anywhere:
//! // input.update(cx, |input, cx| input.focus(window, cx));
//!
//! // Read / write the value:
//! // let text = input.read(cx).value(cx);  // getter
//! // // built with `.initial_value("seed")` if it should start populated
//! // input.update(cx, |input, cx| input.set_value("new", cx));
//! ```
//!
//! Notes:
//! - Always single-line: pasted / IME newlines are turned into spaces.
//! - Long content is clipped (no horizontal scrolling yet).
//! - Password masking uses `*` for every character.

use std::ops::Range;

use gpui::{
    div, fill, font, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    Focusable, FontWeight, GlobalElementId, Hsla, InspectorElementId, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};

use crate::theme::{Theme, MONO_FONT};

/// Visual configuration for a [`TextInput`].
///
/// Start from [`TextInputStyle::from_theme`] and override individual fields as
/// needed. Sized to match the web `.settings-input` (mono 12.5px, 6/9 padding,
/// 6px radius, `--bg-soft` fill).
#[derive(Clone)]
pub struct TextInputStyle {
    pub text_color: Hsla,
    pub placeholder_color: Hsla,
    pub background: Hsla,
    pub border: Hsla,
    pub focus_border: Hsla,
    pub selection: Hsla,
    pub cursor: Hsla,
    pub font_family: SharedString,
    pub font_size: Pixels,
    pub font_weight: FontWeight,
    pub height: Pixels,
    /// `None` means the field stretches to its container (`w_full`).
    pub width: Option<Pixels>,
    pub padding_x: Pixels,
    pub radius: Pixels,
}

impl TextInputStyle {
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            text_color: theme.fg,
            placeholder_color: theme.fg_dim,
            background: theme.bg_soft,
            border: theme.border,
            focus_border: theme.accent,
            selection: theme.accent.opacity(0.28),
            cursor: theme.accent,
            font_family: MONO_FONT.into(),
            font_size: px(12.5),
            font_weight: FontWeight::NORMAL,
            height: px(28.),
            width: None,
            padding_x: px(9.),
            radius: px(6.),
        }
    }

    /// Use the UI font instead of the default monospace family.
    pub fn ui(mut self) -> Self {
        self.font_family = crate::theme::UI_FONT.into();
        self
    }

    /// Use the monospace family (default).
    pub fn mono(mut self) -> Self {
        self.font_family = MONO_FONT.into();
        self
    }
}

type TextCallback = Box<dyn Fn(&str, &mut Window, &mut App) + 'static>;
type EscapeCallback = Box<dyn Fn(&mut Window, &mut App) + 'static>;

/// WHY: a single-line field is re-shaped every frame with no scroll window,
/// so a multi-MB paste would be shaped for megabytes per frame. Cap the
/// stored text in *chars* (char-based is safest with the existing
/// char-boundary helpers: every cut lands on a boundary by construction).
pub const MAX_LEN: usize = 4_096;

/// A single-line text input entity.
pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    masked: bool,
    style: TextInputStyle,
    /// Char-based cap on `content` (see [`MAX_LEN`]); every edit path keeps
    /// the text at or below it and the caret in bounds, so an oversized
    /// paste can neither blow up per-frame shaping nor strand the caret.
    max_len: usize,
    on_change: Option<TextCallback>,
    on_submit: Option<TextCallback>,
    on_escape: Option<EscapeCallback>,
}

impl TextInput {
    /// Create a new input. Call from `cx.new(|cx| TextInput::new(..., window, cx))`.
    pub fn new(
        placeholder: impl Into<SharedString>,
        style: TextInputStyle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder: SharedString = placeholder.into();
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: sanitize(&placeholder).into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            masked: false,
            style,
            max_len: MAX_LEN,
            on_change: None,
            on_submit: None,
            on_escape: None,
        }
    }

    /// Override the char-based length cap (defaults to [`MAX_LEN`]).
    pub fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self.content = truncate_chars_with(&self.content, max_len).into();
        let end = self.content.len();
        self.selected_range.start = self.selected_range.start.min(end);
        self.selected_range.end = self.selected_range.end.min(end);
        if self.selected_range.end < self.selected_range.start {
            self.selected_range.end = self.selected_range.start;
        }
        self
    }

    /// Initial value, cursor moved to the end.
    pub fn initial_value(self, value: impl Into<SharedString>) -> Self {
        let value: SharedString = value.into();
        let value = truncate_chars_with(&sanitize(&value), MAX_LEN);
        let mut this = self;
        this.selected_range = value.len()..value.len();
        this.content = value.into();
        this
    }

    /// Mask the content with `*` (password field).
    pub fn password(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// Called after every user edit (typing, IME, paste, cut, delete).
    pub fn on_change(
        mut self,
        callback: impl Fn(&str, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Box::new(callback));
        self
    }

    /// Called when Enter / Return is pressed.
    pub fn on_submit(
        mut self,
        callback: impl Fn(&str, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_submit = Some(Box::new(callback));
        self
    }

    /// Called when Escape is pressed. When absent, Escape is left to propagate.
    pub fn on_escape(mut self, callback: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_escape = Some(Box::new(callback));
        self
    }

    /// Read the current value.
    pub fn value(&self, _cx: &App) -> SharedString {
        self.content.clone()
    }

    /// Replace the content programmatically (does not fire `on_change`).
    pub fn set_value(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        let value: SharedString = value.into();
        let value = truncate_chars_with(&sanitize(&value), self.max_len);
        self.content = value.clone().into();
        self.selected_range = value.len()..value.len();
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    /// Current selection in UTF-8 byte offsets.
    pub fn selection(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// Focus the input.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window);
        cx.notify();
    }

    /// Remove focus.
    pub fn blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            window.blur();
        }
        cx.notify();
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Clone of the focus handle (avoids clashing with `Focusable::focus_handle`).
    pub fn handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_style(&mut self, style: TextInputStyle, cx: &mut Context<Self>) {
        self.style = style;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Callbacks
    // ------------------------------------------------------------------

    fn notify_change(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(callback) = self.on_change.as_ref() {
            callback(&self.content, window, cx);
        }
    }

    // ------------------------------------------------------------------
    // Cursor / selection primitives
    // ------------------------------------------------------------------

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    // Cursor movement is intentionally char-granular (not grapheme-granular):
    // stepping by char always lands on a char boundary, at the cost of
    // splitting ZWJ emoji sequences and regional-indicator flags.
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        let mut offset = offset;
        // Advance to the end of the current word.
        while offset < self.content.len() {
            let next = self.next_boundary(offset);
            if self.content[offset..next]
                .chars()
                .all(|ch| ch.is_whitespace())
            {
                break;
            }
            offset = next;
        }
        // Skip the whitespace run that follows it.
        while offset < self.content.len() {
            let next = self.next_boundary(offset);
            if !self.content[offset..next]
                .chars()
                .all(|ch| ch.is_whitespace())
            {
                break;
            }
            offset = next;
        }
        offset
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        let mut offset = offset;
        while offset > 0 {
            let prev = self.previous_boundary(offset);
            if !self.content[prev..offset]
                .chars()
                .all(|ch| ch.is_whitespace())
            {
                break;
            }
            offset = prev;
        }
        while offset > 0 {
            let prev = self.previous_boundary(offset);
            if self.content[prev..offset]
                .chars()
                .all(|ch| ch.is_whitespace())
            {
                break;
            }
            offset = prev;
        }
        offset
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        // Single line: only the x coordinate matters (clicks in the field's
        // vertical padding still land on the nearest character).
        let display_index = line.closest_index_for_x(position.x - bounds.left());
        from_display_offset(&self.content, self.masked, display_index)
    }

    fn display_offset(&self, offset: usize) -> usize {
        to_display_offset(&self.content, self.masked, offset)
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn backspace(&mut self, word: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            let target = if word {
                self.previous_word_boundary(cursor)
            } else {
                self.previous_boundary(cursor)
            };
            self.select_to(target, cx);
        }
        self.delete_selection(window, cx);
    }

    fn delete(&mut self, word: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            let target = if word {
                self.next_word_boundary(cursor)
            } else {
                self.next_boundary(cursor)
            };
            self.select_to(target, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn copy(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &sanitize(&text), window, cx);
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection_reversed = false;
        self.selected_range = 0..self.content.len();
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        cx.stop_propagation();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    /// Keyboard handling. Printable characters arrive through the platform
    /// input handler (IME); this only deals with control / navigation keys.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let shortcut = keystroke.modifiers.platform || keystroke.modifiers.control;
        let shift = keystroke.modifiers.shift;
        let word = keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt;

        match keystroke.key.as_str() {
            "backspace" => {
                self.backspace(word, window, cx);
                cx.stop_propagation();
            }
            "delete" => {
                self.delete(word, window, cx);
                cx.stop_propagation();
            }
            "left" => {
                let cursor = self.cursor_offset();
                let offset = if shift {
                    if word {
                        self.previous_word_boundary(cursor)
                    } else {
                        self.previous_boundary(cursor)
                    }
                } else if self.selected_range.is_empty() {
                    if word {
                        self.previous_word_boundary(cursor)
                    } else {
                        self.previous_boundary(cursor)
                    }
                } else {
                    self.selected_range.start
                };
                if shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx);
                }
                cx.stop_propagation();
            }
            "right" => {
                let cursor = self.cursor_offset();
                let offset = if shift {
                    if word {
                        self.next_word_boundary(cursor)
                    } else {
                        self.next_boundary(cursor)
                    }
                } else if self.selected_range.is_empty() {
                    if word {
                        self.next_word_boundary(cursor)
                    } else {
                        self.next_boundary(cursor)
                    }
                } else {
                    self.selected_range.end
                };
                if shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx);
                }
                cx.stop_propagation();
            }
            "home" => {
                if shift {
                    self.select_to(0, cx);
                } else {
                    self.move_to(0, cx);
                }
                cx.stop_propagation();
            }
            "end" => {
                let end = self.content.len();
                if shift {
                    self.select_to(end, cx);
                } else {
                    self.move_to(end, cx);
                }
                cx.stop_propagation();
            }
            "enter" => {
                if let Some(callback) = self.on_submit.as_ref() {
                    callback(&self.content, window, cx);
                    cx.stop_propagation();
                }
            }
            "escape" => {
                if let Some(callback) = self.on_escape.as_ref() {
                    callback(window, cx);
                    cx.stop_propagation();
                } else if self.focus_handle.is_focused(window) {
                    window.blur();
                    cx.notify();
                    cx.stop_propagation();
                }
            }
            "a" if shortcut => {
                self.select_all(cx);
                cx.stop_propagation();
            }
            "c" if shortcut => {
                self.copy(window, cx);
                cx.stop_propagation();
            }
            "x" if shortcut => {
                self.cut(window, cx);
                cx.stop_propagation();
            }
            "v" if shortcut => {
                self.paste(window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // UTF-16 helpers (platform / IME offsets)
    // ------------------------------------------------------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        // Round DOWN: offset 0 and offsets past the end short-circuit, and
        // anything else runs the boundary-safe mapping so the result is
        // always a char boundary, never the middle of a surrogate pair.
        if offset == 0 {
            return 0;
        }
        if offset >= self.content.encode_utf16().count() {
            return self.content.len();
        }
        utf16_to_utf8_floor(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        // A byte offset that does not sit on a char boundary can only mean
        // a stale or platform-computed index; snap it down to the nearest
        // boundary instead of landing inside a multi-byte char.
        let byte = floor_char_boundary(&self.content, offset.min(self.content.len()));
        self.floor_byte_to_utf16(byte)
    }

    fn floor_byte_to_utf16(&self, byte_offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= byte_offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.offset_to_utf16(range.start);
        let end = self.offset_to_utf16(range.end).max(start);
        start..end
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        let start = self.offset_from_utf16(range_utf16.start);
        let end = self.offset_from_utf16(range_utf16.end).max(start);
        start..end
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        // WHY the cap lives here: this is the single chokepoint for typing,
        // IME commits and paste, so truncating the splice result to `max_len`
        // chars (at a char boundary, via `truncate_chars_with`) bounds both
        // the stored size and what the next frame shapes — a multi-MB paste
        // can no longer shape megabytes per frame. The caret is clamped to
        // the (possibly truncated) end so it can never strand past content.
        let new_text = sanitize(new_text);
        let removed_chars = self.content[range.clone()].chars().count();
        let room = self
            .max_len
            .saturating_sub(self.content.chars().count().saturating_sub(removed_chars));
        let new_text = if new_text.chars().count() > room {
            truncate_chars_with(&new_text, room)
        } else {
            new_text
        };
        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.notify_change(window, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        // WHY the same cap as `replace_text_in_range`: IME marked text flows
        // through here, so without it a long composition would grow the
        // content unbounded between commits. Truncate at a char boundary
        // and clamp the mapped caret/mark into the truncated replacement.
        let new_text = sanitize(new_text);
        let removed_chars = self.content[range.clone()].chars().count();
        let room = self
            .max_len
            .saturating_sub(self.content.chars().count().saturating_sub(removed_chars));
        let new_text = if new_text.chars().count() > room {
            truncate_chars_with(&new_text, room)
        } else {
            new_text
        };
        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // Coordinate space: `new_selected_range_utf16` is UTF-16 offsets into
        // the NEW (replacement) text alone, not into the whole document.
        // Clamp it to the replacement text, map it to byte offsets with the
        // same floor-boundary mapping, then rebase by `range.start` so it
        // lands in the post-splice content.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| {
                let len_utf16 = new_text.encode_utf16().count();
                let start = utf16_to_utf8_floor(&new_text, range_utf16.start.min(len_utf16));
                let end = utf16_to_utf8_floor(&new_text, range_utf16.end.min(len_utf16)).max(start);
                let local = start..end;
                range.start + local.start..range.start + local.end
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = self.display_offset(range.start);
        let end = self.display_offset(range.end);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(start),
                bounds.top(),
            ),
            point(bounds.left() + last_layout.x_for_index(end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        let content_index = from_display_offset(&self.content, self.masked, utf8_index);
        Some(self.offset_to_utf16(content_index))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let style = self.style.clone();

        div()
            .flex()
            .items_center()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .h(style.height)
            .px(style.padding_x)
            .rounded(style.radius)
            .bg(style.background)
            .border_1()
            .border_color(if focused { style.focus_border } else { style.border })
            .text_color(style.text_color)
            .text_size(style.font_size)
            .font_weight(style.font_weight)
            .when_some(style.width, |d, width| d.w(width))
            .when(style.width.is_none(), |d| d.w_full())
            .overflow_hidden()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

/// Custom element that shapes the (possibly masked) content, paints the
/// selection/caret and registers the IME input handler.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let font_size = self.input.read(cx).style.font_size;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (font_size * 1.45).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let style = input.style.clone();
        let masked = input.masked;
        let selected_range = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let marked_range = input.marked_range.clone();

        let empty = content.is_empty();
        let (display_text, text_color): (SharedString, Hsla) = if empty {
            (input.placeholder.clone(), style.placeholder_color)
        } else if masked {
            (masked_text(&content), style.text_color)
        } else {
            (content.clone(), style.text_color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: gpui::Font {
                weight: style.font_weight,
                ..font(style.font_family.clone())
            },
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let display_marked = marked_range
            .as_ref()
            .map(|range| {
                to_display_offset(&content, masked, range.start)
                    ..to_display_offset(&content, masked, range.end)
            })
            .filter(|range| range.start < range.end && range.end <= display_text.len());

        let runs: Vec<TextRun> = if let Some(marked) = display_marked {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let line = window
            .text_system()
            .shape_line(display_text, style.font_size, &runs, None);

        let line_height = bounds.bottom() - bounds.top();
        let cursor_display = to_display_offset(&content, masked, cursor_offset);
        let start_display = to_display_offset(&content, masked, selected_range.start);
        let end_display = to_display_offset(&content, masked, selected_range.end);

        let (selection, cursor) = if selected_range.is_empty() {
            let x = line.x_for_index(cursor_display);
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + x, bounds.top()),
                        size(px(2.), line_height),
                    ),
                    style.cursor,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(start_display), bounds.top()),
                        point(bounds.left() + line.x_for_index(end_display), bounds.bottom()),
                    ),
                    style.selection,
                )),
                None,
            )
        };

        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        if let Some(line) = prepaint.line.take() {
            let _ = line.paint(bounds.origin, bounds.bottom() - bounds.top(), window, cx);
            self.input.update(cx, |input, _cx| {
                input.last_layout = Some(line);
                input.last_bounds = Some(bounds);
            });
        }
    }
}

/// Collapse newlines so the field always stays single-line.
fn sanitize(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}
/// Keep at most `max_chars` chars of `text`. The cut is a byte offset
/// obtained from `char_indices`, so it always lands on a char boundary and
/// never splits a multi-byte char. Counted in chars (not bytes) to match
/// the caret helpers, which step char-by-char.
fn truncate_chars_with(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let byte_len = text
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    text[..byte_len].to_owned()
}

fn masked_text(content: &str) -> SharedString {
    content.chars().map(|_| '*').collect::<String>().into()
}

/// Map a content byte offset to its offset in the displayed (masked) text.
fn to_display_offset(content: &str, masked: bool, offset: usize) -> usize {
    if masked {
        content
            .char_indices()
            .take_while(|(idx, _)| *idx < offset)
            .count()
    } else {
        offset.min(content.len())
    }
}

/// Map an offset in the displayed (masked) text back to a content byte offset.
/// For masked text the display offset is a CHAR index (each original char
/// renders as one `*`), so resolve it through `char_indices` instead of
/// treating it as a byte offset.
fn from_display_offset(content: &str, masked: bool, offset: usize) -> usize {
    if masked {
        // `offset` is a char index: nth char boundary, or end-of-content.
        content
            .char_indices()
            .nth(offset)
            .map(|(idx, _)| idx)
            .unwrap_or(content.len())
    } else {
        // Shaped-line indices are byte offsets into the content; snap to a
        // char boundary so a mid-char point can never slice-panic later.
        floor_char_boundary(content, offset.min(content.len()))
    }
}

/// Largest char boundary at or before `index` (0 for empty / past-end).
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let index = index.min(s.len());
    if s.is_char_boundary(index) {
        return index;
    }
    s.char_indices()
        .rev()
        .find_map(|(idx, _)| (idx < index).then_some(idx))
        .unwrap_or(0)
}

/// Map a UTF-16 code-unit offset to the largest UTF-8 byte offset whose
/// UTF-16 prefix length does not exceed `offset` (round DOWN). Snapping
/// down means a lone surrogate half maps to the char before it, never
/// into the middle of the char it splits.
fn utf16_to_utf8_floor(s: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    if offset >= s.encode_utf16().count() {
        return s.len();
    }
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in s.chars() {
        // Next char would overshoot `offset`: it is the char being split.
        if utf16_count + ch.len_utf16() > offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_valid_slice(s: &str, start: usize, end: usize) {
        assert!(
            s.get(start..end).is_some(),
            "slice {start}..{end} of {s:?} is not on char boundaries"
        );
        assert!(s.is_char_boundary(start));
        assert!(s.is_char_boundary(end));
    }

    #[test]
    fn floor_char_boundary_snaps_down_and_clamps() {
        let s = "🔐abc";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 0);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 5), 5);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 7), 7);
        assert_eq!(floor_char_boundary(s, 8), 7);
        assert_eq!(floor_char_boundary(s, usize::MAX), 7);
        assert_eq!(floor_char_boundary("", 0), 0);
        assert_eq!(floor_char_boundary("", 3), 0);
        assert_eq!(floor_char_boundary("", usize::MAX), 0);
        for i in [0, 1, 2, 3, 4, usize::MAX] {
            let snapped = floor_char_boundary(s, i);
            assert!(s.is_char_boundary(snapped), "snapped {snapped} is not a boundary");
        }
        assert_valid_slice(s, floor_char_boundary(s, 1), floor_char_boundary(s, 2));
    }

    #[test]
    fn utf16_to_utf8_floor_rounds_down_mid_surrogate() {
        let s = "🔐ab";
        assert_eq!(utf16_to_utf8_floor(s, 0), 0);
        assert_eq!(utf16_to_utf8_floor(s, 1), 0);
        assert_eq!(utf16_to_utf8_floor(s, 2), 4);
        assert_eq!(utf16_to_utf8_floor(s, 3), 5);
        assert_eq!(utf16_to_utf8_floor(s, 4), 6);
        assert_eq!(utf16_to_utf8_floor(s, 99), 6);
        assert_eq!(utf16_to_utf8_floor(s, usize::MAX), 6);
        let empty = "";
        assert_eq!(utf16_to_utf8_floor(empty, 0), 0);
        assert_eq!(utf16_to_utf8_floor(empty, 1), 0);
        assert_eq!(utf16_to_utf8_floor(empty, usize::MAX), 0);
        for u in [1, usize::MAX] {
            let b = utf16_to_utf8_floor(s, u);
            assert!(s.is_char_boundary(b));
            assert_valid_slice(s, 0, b);
        }
    }

    #[test]
    fn utf16_mapping_matches_offset_from_utf16_contract() {
        // Mirror offset_from_utf16: 0 -> 0, >= utf16 len -> len, else floor map.
        let s = "🔐abc";
        for (u16_off, want) in [(0,0),(1,0),(2,4),(3,5),(4,6),(5,7),(6,7)] {
            let got = if u16_off==0 {0} else if u16_off >= s.encode_utf16().count() { s.len() } else { utf16_to_utf8_floor(s, u16_off) };
            assert_eq!(got, want, "u16 {u16_off}");
            assert_valid_slice(s, 0, got);
        }
        assert_eq!(utf16_to_utf8_floor(s, usize::MAX), 7);
        assert_eq!(utf16_to_utf8_floor(s, 6), 7);
    }

    #[test]
    fn range_mapping_is_boundary_safe_and_ordered() {
        let s = "🔐abc";
        let map = |r: Range<usize>| -> Range<usize> {
            let off = |u: usize| if u==0 {0} else if u >= s.encode_utf16().count() { s.len() } else { utf16_to_utf8_floor(s, u) };
            let start = off(r.start);
            let end = off(r.end).max(start);
            start..end
        };
        for (inp, want) in [((0..1),(0..0)), ((1..3),(0..5)), ((0..usize::MAX),(0..7)), ((4..1),(6..6))] {
            let got = map(inp);
            assert_eq!(got, want);
            assert_valid_slice(s, got.start, got.end);
        }
    }

    #[test]
    fn from_display_offset_masked_uses_char_index() {
        let s = "🔐ab";
        assert_eq!(from_display_offset(s, true, 0), 0);
        assert_eq!(from_display_offset(s, true, 1), 4);
        assert_eq!(from_display_offset(s, true, 2), 5);
        assert_eq!(from_display_offset(s, true, 3), 6);
        assert_eq!(from_display_offset(s, true, 99), 6);
        assert_eq!(from_display_offset(s, false, 1), 0);
        assert_eq!(from_display_offset(s, false, 2), 0);
        assert_eq!(from_display_offset(s, false, 4), 4);
        assert_eq!(from_display_offset(s, false, usize::MAX), 6);
        assert_eq!(from_display_offset("", true, 1), 0);
        assert_eq!(from_display_offset("", false, 3), 0);
        for masked in [true, false] {
            for off in [0,1,2,99] {
                let b = from_display_offset(s, masked, off);
                assert!(s.is_char_boundary(b));
                assert_valid_slice(s, 0, b);
            }
        }
    }

    #[test]
    fn to_display_offset_roundtrips_masked() {
        let s = "🔐ab";
        assert_eq!(to_display_offset(s, true, 0), 0);
        assert_eq!(to_display_offset(s, true, 4), 1);
        assert_eq!(to_display_offset(s, true, 5), 2);
        assert_eq!(to_display_offset(s, true, 6), 3);
        for char_idx in 0..=s.chars().count() {
            let byte = from_display_offset(s, true, char_idx);
            assert_eq!(to_display_offset(s, true, byte), char_idx);
        }
    }

    #[test]
    fn utf16_roundtrip_is_monotone() {
        let s = "🔐aé€z";
        let utf16_len = s.encode_utf16().count();
        let mut prev = 0usize;
        for u in 0..=utf16_len + 2 {
            let byte = if u == 0 { 0 } else if u >= utf16_len { s.len() } else { utf16_to_utf8_floor(s, u) };
            assert!(s.is_char_boundary(byte));
            assert_valid_slice(s, 0, byte);
            // inline floor_byte_to_utf16 equivalent
            let byte_snapped = floor_char_boundary(s, byte.min(s.len()));
            let mut back = 0usize;
            let mut acc = 0usize;
            for ch in s.chars() {
                if acc >= byte_snapped { break; }
                acc += ch.len_utf8();
                back += ch.len_utf16();
            }
            assert!(back >= prev, "non-monotone at u={u}: {back} < {prev}");
            assert!(back <= utf16_len);
            prev = back;
        }
    }

    #[test]
    fn truncate_chars_with_caps_on_char_boundaries() {
        assert_eq!(truncate_chars_with("abcdef", 3), "abc");
        assert_eq!(truncate_chars_with("abc", 3), "abc");
        assert_eq!(truncate_chars_with("", 0), "");
        // Multi-byte chars: the cut must stay on a boundary, never split.
        let out = truncate_chars_with("🔐ab", 1);
        assert_eq!(out, "🔐");
        assert!(out.is_char_boundary(out.len()));
        assert_eq!(truncate_chars_with("🔐ab", 2), "🔐a");
        let big: String = std::iter::repeat_n('x', MAX_LEN + 100).collect();
        let capped = truncate_chars_with(&big, MAX_LEN);
        assert_eq!(capped.chars().count(), MAX_LEN);
        assert!(capped.is_char_boundary(capped.len()));
    }

    // NOTE: TextInput methods offset_from_utf16 / offset_to_utf16 / range_from_utf16 /
    // range_to_utf16 require a TextInput instance whose FocusHandle field has no
    // public / cfg(test) constructor (FocusHandle::new is pub(crate) in gpui), so
    // they cannot be built in-module without a Window/App. The contract tests above
    // pin the underlying free functions those methods delegate to, including the
    // slice-bounds panic each fix prevents.
}
