use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;

use anyhow::{Context as _, Result};
use gpui::{
    FontStyle, FontWeight, Hsla, SharedString, TextRun, UnderlineStyle, Window, font, px, rgb,
};
use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize,
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Scrollback used before a configured value is applied.
pub const DEFAULT_SCROLLBACK: usize = 10_000;
/// Line-height to font-size ratio for the grid (`19 / 13`, the previous fixed
/// values), so changing the font size keeps the old proportions.
const LINE_HEIGHT_RATIO: f32 = 19.0 / 13.0;
const MAX_PENDING: usize = 2 * 1024 * 1024;

pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 30;

/// Fallback cell advance width (px) for when measurement is unavailable.
const FALLBACK_CELL_WIDTH: f32 = 7.8;

/// Caret shape for the terminal grid, mirroring `cursorStyle` in the config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

/// A mouse selection in viewport cell coordinates `(row, col)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    /// Start and end ordered top-left to bottom-right.
    pub fn ordered(self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor.0, self.anchor.1) <= (self.head.0, self.head.1) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// The keyboard-modifier state of a mouse event, mapped to xterm modifier
/// bits (`4` shift, `8` alt, `16` ctrl) in a mouse report.
#[derive(Clone, Copy, Debug, Default)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl MouseMods {
    fn bits(self) -> i32 {
        i32::from(self.shift) * 4 + i32::from(self.alt) * 8 + i32::from(self.ctrl) * 16
    }
}

/// Base xterm button codes used in mouse reports.
pub mod mouse_button {
    pub const LEFT: i32 = 0;
    pub const MIDDLE: i32 = 1;
    pub const RIGHT: i32 = 2;
    /// Release / no button, in the legacy (non-SGR) encoding.
    pub const NONE: i32 = 3;
    pub const WHEEL_UP: i32 = 64;
    pub const WHEEL_DOWN: i32 = 65;
    pub const WHEEL_LEFT: i32 = 66;
    pub const WHEEL_RIGHT: i32 = 67;
}

/// The mouse-reporting protocol a child program selected: the mode it asked for
/// and the encoding it expects its reports in.
pub type MouseProtocol = (MouseProtocolMode, MouseProtocolEncoding);

/// A single mouse event to report to the child program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press { button: i32 },
    Release { button: i32 },
    /// Pointer moved; `button` is the held button, if any.
    Motion { button: Option<i32> },
    /// Wheel/trackpad notch; `button` is one of the `mouse_button::WHEEL_*`
    /// codes.
    Wheel { button: i32 },
}

/// Encode one mouse event for the protocol the running program selected, or
/// `None` when it disabled mouse reporting or the event is not reportable in
/// its mode. Pure so it can be unit-tested without a PTY.
pub fn encode_mouse_event(
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    col: u16,
    row: u16,
    action: MouseAction,
    mods: MouseMods,
) -> Option<Vec<u8>> {
    let motion = matches!(action, MouseAction::Motion { .. });
    let release = matches!(action, MouseAction::Release { .. });
    // Which event kinds the program asked for.
    match mode {
        MouseProtocolMode::None => return None,
        MouseProtocolMode::Press if release || motion => return None,
        MouseProtocolMode::PressRelease if motion => return None,
        MouseProtocolMode::ButtonMotion
            if motion && !matches!(action, MouseAction::Motion { button: Some(_) }) =>
        {
            return None;
        }
        _ => {}
    }

    let base = match action {
        MouseAction::Press { button } | MouseAction::Wheel { button } => button,
        MouseAction::Release { button } => button,
        MouseAction::Motion { button } => button.unwrap_or(mouse_button::NONE),
    };
    let x = i32::from(col) + 1;
    let y = i32::from(row) + 1;

    match encoding {
        MouseProtocolEncoding::Sgr => {
            let code = base + mods.bits() + if motion { 32 } else { 0 };
            let terminator = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{terminator}").into_bytes())
        }
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            // The legacy encoding has no button number for a release, so it is
            // reported as button 3.
            let code = (if release { mouse_button::NONE } else { base })
                + mods.bits()
                + if motion { 32 } else { 0 }
                + 32;
            let cx = x + 32;
            let cy = y + 32;
            let utf8 = matches!(encoding, MouseProtocolEncoding::Utf8);
            if !utf8 && (code > 255 || cx > 255 || cy > 255) {
                return None;
            }
            let mut out = vec![0x1b, b'[', b'M'];
            for value in [code, cx, cy] {
                if utf8 {
                    push_utf8(&mut out, value.max(0) as u32);
                } else {
                    out.push(value.clamp(0, 255) as u8);
                }
            }
            Some(out)
        }
    }
}

/// The modes a session is in that its *output tail* cannot describe: each one
/// is set once by the child (when it starts, or when it opens its fullscreen
/// screen), so a client attaching later is replayed a bounded tail that no
/// longer holds the sequence which set them.
///
/// Restoring the whole set is what makes a late client behave like the desktop’s
/// own terminal: without it a mouse-owning TUI gets no taps at all, a fullscreen
/// app is painted into the wrong screen buffer, and paste/arrow keys take the
/// wrong form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionModes {
    /// The mouse-reporting protocol the child armed, if any.
    pub mouse: Option<MouseProtocol>,
    /// The child is on the alternate screen (a fullscreen TUI).
    pub alternate: bool,
    /// Arrow keys are expected in application form (`\x1bOA`).
    pub application_cursor: bool,
    /// The keypad is expected in application form (`\x1bOp`).
    pub application_keypad: bool,
    /// The child asked for the cursor to stay hidden.
    pub hide_cursor: bool,
    /// The child asked for bracketed paste (`\x1b[200~`).
    pub bracketed_paste: bool,
}

impl SessionModes {
    /// The DECSET bytes that put a terminal into exactly these modes, or empty
    /// when the child is in none of them — an empty result means "leave the
    /// terminal alone", since a fresh one already starts in that state.
    pub fn restore_sequence(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Alternate screen first: entering it clears the buffer the tail is
        // about to paint.
        if self.alternate {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        if self.application_cursor {
            out.extend_from_slice(b"\x1b[?1h");
        }
        if self.application_keypad {
            out.extend_from_slice(b"\x1b[?66h");
        }
        if self.bracketed_paste {
            out.extend_from_slice(b"\x1b[?2004h");
        }
        if self.hide_cursor {
            out.extend_from_slice(b"\x1b[?25l");
        }
        if let Some((protocol, encoding)) = self.mouse {
            mouse_restore_sequence(protocol, encoding, &mut out);
        }
        out
    }
}

/// Append the DECSET bytes for the mouse protocol a child program armed.
fn mouse_restore_sequence(
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    out: &mut Vec<u8>,
) {
    match mode {
        MouseProtocolMode::None => return,
        // X10: press only.
        MouseProtocolMode::Press => out.extend_from_slice(b"\x1b[?9h"),
        // VT200: press and release.
        MouseProtocolMode::PressRelease => out.extend_from_slice(b"\x1b[?1000h"),
        // Button-event: VT200 plus motion while a button is held (drag).
        MouseProtocolMode::ButtonMotion => out.extend_from_slice(b"\x1b[?1002h"),
        // Any-event: every motion, held button or not.
        MouseProtocolMode::AnyMotion => out.extend_from_slice(b"\x1b[?1003h"),
    }
    match encoding {
        // The default single-byte encoding has no DECSET of its own.
        MouseProtocolEncoding::Default => {}
        MouseProtocolEncoding::Utf8 => out.extend_from_slice(b"\x1b[?1005h"),
        MouseProtocolEncoding::Sgr => out.extend_from_slice(b"\x1b[?1006h"),
    }
}

fn push_utf8(out: &mut Vec<u8>, code: u32) {
    let mut buf = [0u8; 4];
    match char::from_u32(code) {
        Some(ch) => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
        None => out.push(b'?'),
    }
}

/// Runtime terminal appearance, driven by `AppConfig::appearance` so the
/// Settings panel actually affects the grid (font, size, scrollback, caret).
#[derive(Clone)]
pub struct TerminalAppearance {
    pub font_family: SharedString,
    pub font_size: f32,
    pub scrollback: usize,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
}

impl Default for TerminalAppearance {
    fn default() -> Self {
        Self {
            font_family: SharedString::from(crate::theme::MONO_FONT),
            font_size: 13.0,
            scrollback: DEFAULT_SCROLLBACK,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
        }
    }
}

static APPEARANCE: OnceLock<RwLock<TerminalAppearance>> = OnceLock::new();

fn appearance_slot() -> &'static RwLock<TerminalAppearance> {
    APPEARANCE.get_or_init(|| RwLock::new(TerminalAppearance::default()))
}

/// Snapshot the current appearance. Cheap enough for per-row rendering.
pub fn appearance() -> TerminalAppearance {
    appearance_slot().read().expect("appearance lock").clone()
}

/// Replace the appearance. Call at startup and whenever Settings are saved.
pub fn set_appearance(appearance: TerminalAppearance) {
    *appearance_slot().write().expect("appearance lock") = appearance;
}

/// Terminal color scheme. The chrome stays dark; only the grid, its caret and
/// the ANSI palette follow this, mirroring the web app where themes never
/// repaint the chrome.
#[derive(Clone, Copy)]
pub struct TerminalTheme {
    pub background: Hsla,
    pub foreground: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub accent: Hsla,
    pub ansi: [Hsla; 16],
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            background: rgb(0x0e1014).into(),
            foreground: rgb(0xe6e6e6).into(),
            cursor: rgb(0xe6e6e6).into(),
            selection: rgb(0x3a4150).into(),
            accent: rgb(0x4ea1ff).into(),
            ansi: std::array::from_fn::<Hsla, 16, _>(|index| rgb(ANSI_PALETTE[index]).into()),
        }
    }
}

static TERM_THEME: OnceLock<RwLock<TerminalTheme>> = OnceLock::new();

fn term_theme_slot() -> &'static RwLock<TerminalTheme> {
    TERM_THEME.get_or_init(|| RwLock::new(TerminalTheme::default()))
}

/// Snapshot the active terminal theme.
pub fn term_theme() -> TerminalTheme {
    *term_theme_slot().read().expect("theme lock")
}

/// Replace the terminal theme. Call at startup and whenever Settings are saved.
pub fn set_term_theme(theme: TerminalTheme) {
    *term_theme_slot().write().expect("theme lock") = theme;
}

/// Wake signal the PTY reader fires the instant new output lands, so the UI
/// repaints without polling. Registered once by the app at startup.
static WAKE: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// Register the signal the reader threads wake. Idempotent.
pub fn set_wake_signal(signal: Arc<tokio::sync::Notify>) {
    let _ = WAKE.set(signal);
}

/// Last time a reader thread handed bytes over. The UI pump compares it with
/// its own resume time to measure how late the wake-up was serviced.
type WakeStamp = std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>;

pub fn wake_stamp() -> WakeStamp {
    static STAMP: OnceLock<WakeStamp> = OnceLock::new();
    STAMP
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(None)))
        .clone()
}

fn signal_wake() {
    if let Ok(mut stamp) = wake_stamp().lock() {
        *stamp = Some(std::time::Instant::now());
    }
    if let Some(signal) = WAKE.get() {
        signal.notify_one();
    }
}

/// Terminal font size in px (from the configured appearance).
pub fn term_font_size() -> f32 {
    appearance().font_size
}

/// Terminal line height in px, derived from the font size.
pub fn term_line_height() -> f32 {
    (appearance().font_size * LINE_HEIGHT_RATIO).round()
}

/// The standard 16-ANSI palette, matching the defaults in
/// `backend::config::ThemeConfig` / `src/themes.ts`. Index 0..8 are the dark
/// colors, 8..16 the bright ones.
pub static ANSI_PALETTE: [u32; 16] = [
    0x2c303a, // black
    0xff5c8d, // red
    0x56d364, // green
    0xe3b341, // yellow
    0x4ea1ff, // blue
    0xc678dd, // magenta
    0x56b6c2, // cyan
    0xc0c4ce, // white
    0x525866, // bright black
    0xff7ba0, // bright red
    0x7ce081, // bright green
    0xf0c674, // bright yellow
    0x73b5ff, // bright blue
    0xd49eea, // bright magenta
    0x7fc8d3, // bright cyan
    0xe6e9f0, // bright white
];

/// Resolved visual attributes of a single terminal cell. `inverse` has already
/// been applied to `fg`/`bg` when produced by [`term_row`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellColors {
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// One styled span of a terminal row.
#[derive(Clone, Debug)]
pub struct TermSpan {
    pub text: String,
    pub colors: CellColors,
    pub underline: bool,
}

/// One visible terminal row as merged styled runs.
#[derive(Clone, Debug)]
pub struct TermRow {
    pub spans: Vec<TermSpan>,
}

/// Cached cell advance width, keyed by `(font family, font-size bits)` so a
/// font or size change invalidates the measurement.
type CellWidthCache = Mutex<Option<((SharedString, u32), f32)>>;
static CELL_WIDTH: OnceLock<CellWidthCache> = OnceLock::new();

fn cell_width_cache() -> &'static CellWidthCache {
    CELL_WIDTH.get_or_init(|| Mutex::new(None))
}

fn rgb_hsla(value: u32) -> Hsla {
    rgb(value).into()
}

fn brighten(color: Hsla) -> Hsla {
    Hsla {
        l: (color.l + 0.14).min(1.0),
        ..color
    }
}

fn dimmed(color: Hsla) -> Hsla {
    Hsla {
        l: color.l * 0.7,
        ..color
    }
}

/// Map a non-default vt100 color to an GPUI color. `Color::Idx` below 16 uses
/// the theme palette; 16..255 approximates the xterm cube/grayscale ramp.
fn color_hsla(color: vt100::Color, default: Hsla, ansi: &[Hsla; 16]) -> Hsla {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => {
            rgb_hsla((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
        }
        vt100::Color::Idx(i) => {
            let idx = usize::from(i);
            if idx < 16 {
                ansi[idx]
            } else if idx < 232 {
                let n = idx - 16;
                let scale = |v: usize| if v == 0 { 0 } else { 55 + v * 40 };
                let r = scale(n / 36);
                let g = scale((n / 6) % 6);
                let b = scale(n % 6);
                rgb_hsla(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
            } else {
                let v = (8 + (idx - 232) * 10).min(255) as u32;
                rgb_hsla((v << 16) | (v << 8) | v)
            }
        }
    }
}

fn cell_colors(
    cell: &vt100::Cell,
    theme_fg: Hsla,
    theme_bg: Hsla,
    ansi: &[Hsla; 16],
) -> CellColors {
    let mut fg = color_hsla(cell.fgcolor(), theme_fg, ansi);
    if cell.bold() {
        fg = brighten(fg);
    } else if cell.dim() {
        fg = dimmed(fg);
    }
    let mut bg = if matches!(cell.bgcolor(), vt100::Color::Default) {
        None
    } else {
        Some(color_hsla(cell.bgcolor(), theme_bg, ansi))
    };
    if cell.inverse() {
        // Swap: what was the background becomes the foreground and vice versa.
        let new_fg = bg.unwrap_or(theme_bg);
        bg = Some(fg);
        fg = new_fg;
    }
    CellColors {
        fg,
        bg,
        bold: cell.bold(),
        italic: cell.italic(),
        underline: cell.underline(),
    }
}

/// Build one visible terminal row (scrollback-offset aware) into styled
/// spans. `screen.cell(row, ..)` returns the row currently in view for the
/// active scroll offset, so `row` is simply the on-screen row index.
pub fn term_row(screen: &vt100::Screen, row: u16, theme_fg: Hsla, theme_bg: Hsla) -> TermRow {
    let ansi = term_theme().ansi;
    let (_, cols) = screen.size();
    let cols = cols.get();
    let default = CellColors {
        fg: theme_fg,
        bg: None,
        bold: false,
        italic: false,
        underline: false,
    };

    // Single pass over the row: read each cell once, carry the previous
    // cell's colors for wide continuations, and flush a run when the colors
    // change. The previous version read every cell twice and allocated a
    // `Vec<CellColors>` as long as the row on every frame.
    let mut spans: Vec<TermSpan> = Vec::new();
    let mut run_colors = default;
    let mut has_run = false;
    let mut text = String::new();
    let mut prev_colors = default;

    for col in 0..cols {
        let cell = screen.cell(row, col);
        let colors = match cell {
            Some(cell) if cell.is_wide_continuation() => prev_colors,
            Some(cell) => {
                let colors = cell_colors(cell, theme_fg, theme_bg, &ansi);
                prev_colors = colors;
                colors
            }
            None => default,
        };

        if !has_run || colors != run_colors {
            if has_run {
                spans.push(TermSpan {
                    text: std::mem::take(&mut text),
                    colors: run_colors,
                    underline: run_colors.underline,
                });
            }
            run_colors = colors;
            has_run = true;
        }

        match cell {
            Some(cell) if cell.is_wide_continuation() => {}
            Some(cell) if cell.has_contents() => text.push_str(cell.contents()),
            _ => text.push(' '),
        }
    }

    if has_run {
        spans.push(TermSpan {
            text,
            colors: run_colors,
            underline: run_colors.underline,
        });
    }

    // Trim a trailing run that is pure default-attribute whitespace; it only
    // costs layout. Backgrounded/underlined tails must stay.
    let trim_tail = spans.last().is_some_and(|last| {
        last.colors.bg.is_none()
            && !last.colors.underline
            && !last.text.is_empty()
            && last.text.chars().all(|c| c == ' ')
    });
    if trim_tail {
        spans.pop();
    }

    TermRow { spans }
}

/// Convert a [`TermSpan`] into a GPUI [`TextRun`].
pub fn span_run(span: &TermSpan) -> TextRun {
    let mut font = font(appearance().font_family);
    if span.colors.bold {
        font.weight = FontWeight::BOLD;
    }
    if span.colors.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len: span.text.len(),
        font,
        color: span.colors.fg,
        background_color: span.colors.bg,
        underline: span.underline.then(UnderlineStyle::default),
        strikethrough: None,
    }
}

/// A one-cell run for the `\n` between two grid lines, so the newline carries
/// no background/underline from the line above it.
fn newline_run(appearance: &TerminalAppearance, fg: Hsla) -> TextRun {
    TextRun {
        len: 1,
        font: font(appearance.font_family.clone()),
        color: fg,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// The cursor position mapped into the *visible* viewport, accounting for the
/// current scrollback offset. `None` when the cursor is scrolled off-screen.
pub fn visible_cursor(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let (row, col) = screen.cursor_position();
    let scrolled = screen.scrollback();
    if usize::from(row) < scrolled {
        return None;
    }
    let visible = row - scrolled as u16;
    let (rows, _) = screen.size();
    if visible >= rows.get() {
        return None;
    }
    Some((visible, col))
}

/// Clamp a requested scroll offset into `[0, scrollback_len]`.
pub fn clamp_scrollback(screen: &vt100::Screen, requested: isize) -> usize {
    let max = screen.scrollback_len() as isize;
    requested.clamp(0, max) as usize
}

/// Terminal grid metrics: pixel advance per column and per row.
#[derive(Clone, Copy, Debug)]
pub struct TermMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
}

/// Measure the mono cell width against the live display (cached per
/// font/size); falls back to [`FALLBACK_CELL_WIDTH`] if measurement fails.
pub fn term_metrics(window: &mut Window) -> TermMetrics {
    let appearance = appearance();
    let key = (appearance.font_family.clone(), appearance.font_size.to_bits());
    let cache = cell_width_cache();
    let mut guard = cache.lock().expect("cell width lock");
    let width = match guard.as_ref() {
        Some((cached, value)) if *cached == key => *value,
        _ => {
            let sample = "0".repeat(80);
            let run = TextRun {
                len: sample.len(),
                font: font(appearance.font_family.clone()),
                color: rgb_hsla(0),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let layout = window.text_system().layout_line(
                &sample,
                px(appearance.font_size),
                &[run],
                None,
            );
            let measured = f32::from(layout.width) / 80.0;
            let value = if measured.is_finite() && measured > 1.0 && measured < 100.0 {
                measured
            } else {
                FALLBACK_CELL_WIDTH
            };
            *guard = Some((key, value));
            value
        }
    };
    TermMetrics {
        cell_width: width,
        cell_height: term_line_height(),
    }
}

/// Cached (or fallback) cell advance width in px; safe to call outside a
/// render pass. [`term_metrics`] warms this cache from the live display.
pub fn cell_width() -> f32 {
    let appearance = appearance();
    let key = (appearance.font_family, appearance.font_size.to_bits());
    let cache = cell_width_cache();
    let guard = cache.lock().expect("cell width lock");
    match guard.as_ref() {
        Some((cached, value)) if *cached == key => *value,
        _ => FALLBACK_CELL_WIDTH,
    }
}

/// Compute a grid size fitting the given pixel area; never below `(1, 1)` and
/// tolerant of zero-sized panes.
pub fn grid_size(area_width: f32, area_height: f32, metrics: TermMetrics) -> (u16, u16) {
    let cols = if area_width.is_finite() && area_width > 0.0 {
        (area_width / metrics.cell_width).floor().max(1.0) as u16
    } else {
        1
    };
    let rows = if area_height.is_finite() && area_height > 0.0 {
        (area_height / metrics.cell_height).floor().max(1.0) as u16
    } else {
        1
    };
    (cols, rows)
}

impl TerminalSession {
    /// Number of scrollback lines currently available above the viewport.
    pub fn scrollback_len(&self) -> usize {
        self.parser.screen().scrollback_len()
    }

    /// Whether the child program is using the alternate screen (e.g. `less`,
    /// `vim`). Scrollback is meaningless there.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// The mouse-reporting protocol the child program enabled, or `None` when
    /// it did not ask for mouse events. A fullscreen TUI (e.g. Pi) enables
    /// this and expects the terminal to forward clicks, drags and wheel
    /// notches as escape sequences.
    pub fn mouse_mode(&self) -> Option<MouseProtocol> {
        let screen = self.parser.screen();
        match screen.mouse_protocol_mode() {
            MouseProtocolMode::None => None,
            mode => Some((mode, screen.mouse_protocol_encoding())),
        }
    }

    /// True when the child program asked the terminal to report mouse events.
    pub fn wants_mouse(&self) -> bool {
        self.mouse_mode().is_some()
    }

    /// The modes a client must be put into before it is fed this session's
    /// output: see [`SessionModes`]. Read from the same parsed screen the
    /// desktop renders, so it is exactly the state this terminal is in.
    pub fn session_modes(&self) -> SessionModes {
        let screen = self.parser.screen();
        SessionModes {
            mouse: self.mouse_mode(),
            alternate: screen.alternate_screen(),
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
            hide_cursor: screen.hide_cursor(),
            bracketed_paste: screen.bracketed_paste(),
        }
    }

    /// Forward a mouse event to the child when it enabled mouse reporting.
    /// Returns true only when an escape sequence was actually written, so the
    /// caller knows whether a repaint is needed (e.g. motion is dropped in
    /// `Press`/`PressRelease` modes and produces no report).
    pub fn report_mouse(
        &self,
        col: u16,
        row: u16,
        action: MouseAction,
        mods: MouseMods,
    ) -> bool {
        let Some((mode, encoding)) = self.mouse_mode() else {
            return false;
        };
        match encode_mouse_event(mode, encoding, col, row, action, mods) {
            Some(bytes) => self.write(&bytes).is_ok(),
            None => false,
        }
    }

    /// Mouse-wheel scrolling: shift the view by `lines` rows (negative scrolls
    /// up into history). Ignored on the alternate screen. Returns true when
    /// the offset actually changed.
    pub fn scroll_lines(&mut self, lines: isize) -> bool {
        if self.parser.screen().alternate_screen() {
            return false;
        }
        let current = self.parser.screen().scrollback() as isize;
        let target = clamp_scrollback(self.parser.screen(), current - lines);
        let changed = target != self.parser.screen().scrollback();
        if changed {
            self.parser.screen_mut().set_scrollback(target);
        }
        changed
    }

    /// The cursor position as visible in the current viewport, or `None` when
    /// hidden / scrolled off-screen.
    pub fn visible_cursor(&self) -> Option<(u16, u16)> {
        if self.parser.screen().hide_cursor() {
            return None;
        }
        visible_cursor(self.parser.screen())
    }

    /// Render one visible row (including scrollback rows) into styled spans.
    pub fn render_row(&self, row: u16, theme_fg: Hsla, theme_bg: Hsla) -> TermRow {
        term_row(self.parser.screen(), row, theme_fg, theme_bg)
    }

    /// The whole visible grid as one shaped string + text runs, cached between
    /// changes. Idle repaints (a click, the caret blink) hit the cache and do
    /// no per-cell work at all.
    pub fn grid_text(&self, theme_fg: Hsla, theme_bg: Hsla) -> (String, Vec<TextRun>) {
        let appearance = appearance();
        let screen = self.parser.screen();
        let (rows_nz, cols_nz) = screen.size();
        let rows = rows_nz.get();
        let cols = cols_nz.get();
        let scrollback = screen.scrollback();
        let generation = self.generation.load(Ordering::Relaxed);
        let font_size_bits = appearance.font_size.to_bits();

        if let Ok(guard) = self.grid_cache.lock() {
            if let Some(cache) = guard.as_ref() {
                if cache.generation == generation
                    && cache.font_family == appearance.font_family
                    && cache.font_size_bits == font_size_bits
                    && cache.theme_fg == theme_fg
                    && cache.theme_bg == theme_bg
                    && cache.rows == rows
                    && cache.cols == cols
                    && cache.scrollback == scrollback
                {
                    return (cache.text.clone(), cache.runs.clone());
                }
            }
        }

        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        for row in 0..rows {
            let parse_started = std::time::Instant::now();
            let term_row = term_row(screen, row, theme_fg, theme_bg);
            let parse_us = crate::metrics::micros(parse_started.elapsed());
            for span in &term_row.spans {
                if span.text.is_empty() {
                    continue;
                }
                runs.push(span_run(span));
                text.push_str(&span.text);
            }
            if crate::metrics::enabled() {
                crate::metrics::line(
                    u64::from(cols),
                    term_row.spans.iter().map(|s| s.text.chars().count()).sum::<usize>() as u64,
                    term_row.spans.len() as u64,
                );
            }
            if crate::metrics::rows_enabled() {
                crate::metrics::probe("row.parse", parse_us);
            }
            if row + 1 < rows {
                text.push('\n');
                runs.push(newline_run(&appearance, theme_fg));
            }
        }

        if let Ok(mut guard) = self.grid_cache.lock() {
            *guard = Some(GridCache {
                generation,
                font_family: appearance.font_family.clone(),
                font_size_bits,
                theme_fg,
                theme_bg,
                rows,
                cols,
                scrollback,
                text: text.clone(),
                runs: runs.clone(),
            });
        }
        (text, runs)
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let (args, env) = shell_setup(&shell);
        Self {
            program: shell,
            args,
            cwd: std::env::var("HOME").ok(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            env,
        }
    }
}

fn shell_setup(shell: &str) -> (Vec<String>, Vec<(String, String)>) {
    let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|root| root.join("scripts"))
        .unwrap_or_default();
    let config_root = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config")))
        .map(|base| base.join("spritx"));

    if shell.ends_with("bash") {
        let init = scripts.join("spritx-shell-init.bash");
        if let (Some(root), true) = (config_root, init.exists()) {
            let rc = root.join("gpui-bashrc");
            let body = format!(
                "[ -f \"$HOME/.bashrc\" ] && source \"$HOME/.bashrc\"\nsource \"{}\"\n",
                init.display()
            );
            if std::fs::create_dir_all(&root).is_ok() && std::fs::write(&rc, body).is_ok() {
                return (
                    vec!["--rcfile".to_string(), rc.display().to_string(), "-i".to_string()],
                    Vec::new(),
                );
            }
        }
        return (vec!["-l".to_string()], Vec::new());
    }

    if shell.ends_with("zsh") {
        let init = scripts.join("spritx-shell-init.zsh");
        if let (Some(root), true) = (config_root, init.exists()) {
            let zdot = root.join("gpui-zdotdir");
            let rc = zdot.join(".zshrc");
            let body = format!(
                "[ -f \"$HOME/.zshrc\" ] && source \"$HOME/.zshrc\"\nsource \"{}\"\n",
                init.display()
            );
            if std::fs::create_dir_all(&zdot).is_ok() && std::fs::write(&rc, body).is_ok() {
                return (
                    vec!["-i".to_string()],
                    vec![("ZDOTDIR".to_string(), zdot.display().to_string())],
                );
            }
        }
        return (vec!["-l".to_string()], Vec::new());
    }

    if shell.ends_with("fish") {
        let init = scripts.join("spritx-shell-init.fish");
        if init.exists() {
            return (
                vec!["-C".to_string(), format!("source {}", init.display())],
                Vec::new(),
            );
        }
        return (vec![], Vec::new());
    }

    (Vec::new(), Vec::new())
}

impl SessionConfig {
    pub fn command(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            ..Self::default()
        }
    }
}

/// A live shell process attached to a PTY, plus its parsed screen.
///
/// Bytes are read on a background thread into a small mailbox; the UI thread
/// calls [`Self::pump`] to feed them into the terminal parser. This keeps all
/// `vt100` state on one thread and makes the session trivially testable without
/// GPUI.
/// What is on the other end of a pane: a local shell in a PTY, or a terminal of
/// another SpritX over the encrypted remote channel. Both feed the same `vt100`
/// parser, so rendering, selection and scrollback are shared.
enum SessionBackend {
    Local(LocalPty),
    Remote(RemotePane),
}

struct LocalPty {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    exit_code: Arc<Mutex<Option<i32>>>,
    pid: Option<u32>,
}

struct RemotePane {
    link: Arc<crate::backend::remote_client::RemoteLink>,
}

pub struct TerminalSession {
    backend: SessionBackend,
    parser: vt100::Parser,
    /// Output waiting to be parsed. Shared with the PTY reader thread, or with
    /// the remote link task for a pane driven by another SpritX.
    pending: Arc<Mutex<Vec<u8>>>,
    dirty: Arc<AtomicBool>,
    cols: u16,
    rows: u16,
    /// Bumped whenever the parsed screen changes (new output, resize,
    /// scroll). The grid-text cache below is only valid for one generation.
    generation: AtomicU64,
    grid_cache: Mutex<Option<GridCache>>,
    /// Current mouse selection, if any.
    selection: Option<Selection>,
    /// True between mouse-down and mouse-up while the user is dragging out a
    /// selection, even before the head has moved to another cell.
    selecting: bool,
    /// Absolute grid rectangle `(x, y, width, height)` in window
    /// coordinates, captured during paint so mouse events can be mapped back
    /// to cells. The size is the full content area, so clicks in the empty
    /// space past the last column still land on the nearest cell.
    grid_origin: Arc<Mutex<Option<(f32, f32, f32, f32)>>>,
}

/// Cached shaped representation of the whole visible grid. Rebuilt only when
/// the screen generation or the visual parameters change, so idle repaints
/// (clicks, caret blink) reuse it instead of re-reading every vt100 cell.
struct GridCache {
    generation: u64,
    font_family: SharedString,
    font_size_bits: u32,
    theme_fg: Hsla,
    theme_bg: Hsla,
    rows: u16,
    cols: u16,
    scrollback: usize,
    text: String,
    runs: Vec<TextRun>,
}

impl TerminalSession {
    pub fn spawn(config: SessionConfig) -> Result<Self> {
        let cols = config.cols.max(1);
        let rows = config.rows.max(1);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&config.program);
        cmd.args(&config.args);
        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("SPRITX_TERM", "1");
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;
        // The slave handle must be closed in the parent or the reader never
        // sees EOF when the child exits.
        drop(pair.slave);

        let pending = Arc::new(Mutex::new(Vec::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(Mutex::new(None));

        spawn_reader(reader, pending.clone(), dirty.clone());
        spawn_reaper(child, exit_code.clone());

        Ok(Self {
            backend: SessionBackend::Local(LocalPty {
                writer: Arc::new(Mutex::new(writer)),
                master: pair.master,
                killer,
                exit_code: exit_code.clone(),
                pid,
            }),
            parser: vt100::Parser::new(
                NonZeroU16::new(rows).expect("rows > 0"),
                NonZeroU16::new(cols).expect("cols > 0"),
                appearance().scrollback.max(1),
            ),
            pending,
            dirty,
            cols,
            rows,
            generation: AtomicU64::new(0),
            grid_cache: Mutex::new(None),
            selection: None,
            selecting: false,
            grid_origin: Arc::new(Mutex::new(None)),
        })
    }

    /// A pane that shows a terminal of another SpritX. Output arrives from the
    /// link task into the same buffers a PTY reader would use.
    pub fn attach_remote(link: Arc<crate::backend::remote_client::RemoteLink>) -> Self {
        let (pending, dirty) = link.output_buffers();
        let (cols, rows) = (80, 24);
        Self {
            backend: SessionBackend::Remote(RemotePane { link }),
            parser: vt100::Parser::new(
                NonZeroU16::new(rows).expect("rows > 0"),
                NonZeroU16::new(cols).expect("cols > 0"),
                appearance().scrollback.max(1),
            ),
            pending,
            dirty,
            cols,
            rows,
            generation: AtomicU64::new(0),
            grid_cache: Mutex::new(None),
            selection: None,
            selecting: false,
            grid_origin: Arc::new(Mutex::new(None)),
        }
    }

    /// True when this pane is driven by another SpritX rather than a local shell.
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, SessionBackend::Remote(_))
    }

    /// The peer's name, for a remote pane's title.
    pub fn remote_label(&self) -> Option<String> {
        match &self.backend {
            SessionBackend::Remote(pane) => pane.link.server_name(),
            SessionBackend::Local(_) => None,
        }
    }

    pub fn spawn_default() -> Result<Self> {
        Self::spawn(SessionConfig::default())
    }

    pub fn spawn_command(program: &str, args: &[&str]) -> Result<Self> {
        Self::spawn(SessionConfig::command(
            program,
            args.iter().map(|arg| arg.to_string()),
        ))
    }

    /// Feed buffered PTY output into the parser. Returns true if the screen
    /// changed and the UI should repaint.
    pub fn pump_bytes(&mut self) -> Option<Vec<u8>> {
        let bytes = {
            let mut pending = self.pending.lock().expect("pending lock");
            if pending.is_empty() {
                return None;
            }
            std::mem::take(&mut *pending)
        };
        self.parser.process(&bytes);
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.dirty.store(false, Ordering::Relaxed);
        Some(bytes)
    }

    pub fn pump(&mut self) -> bool {
        self.pump_bytes().is_some()
    }

    pub fn has_pending(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    // ------------------------------------------------------------------
    // Mouse selection
    // ------------------------------------------------------------------

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|selection| selection.anchor != selection.head)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
    }

    /// Snap the viewport back to the live output (bottom). Returns true when
    /// the scroll offset actually changed. On the alternate screen the offset
    /// is already 0, so this is a no-op there.
    pub fn scroll_to_bottom(&mut self) -> bool {
        if self.parser.screen().scrollback() == 0 {
            return false;
        }
        self.parser.screen_mut().set_scrollback(0);
        true
    }

    /// Begin a drag selection at `(row, col)` in viewport cells.
    pub fn begin_selection(&mut self, row: u16, col: u16) {
        self.selection = Some(Selection {
            anchor: (row, col),
            head: (row, col),
        });
        self.selecting = true;
    }

    /// True while the mouse button is held down for a selection drag.
    pub fn is_selecting(&self) -> bool {
        self.selecting
    }

    /// The drag ended (mouse up, including outside the pane).
    pub fn end_selection(&mut self) {
        self.selecting = false;
    }

    /// Select the whole visible screen.
    pub fn select_all(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        self.selection = Some(Selection {
            anchor: (0, 0),
            head: (rows.get().saturating_sub(1), cols.get().saturating_sub(1)),
        });
    }

    /// Move the selection head while dragging.
    pub fn extend_selection(&mut self, row: u16, col: u16) {
        if let Some(selection) = self.selection.as_mut() {
            selection.head = (row, col);
        }
    }

    /// Shared slot the renderer writes the grid rectangle into during paint.
    pub fn grid_origin_slot(&self) -> Arc<Mutex<Option<(f32, f32, f32, f32)>>> {
        self.grid_origin.clone()
    }

    /// The selected text, with trailing whitespace trimmed per line.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let last_row = rows.get().saturating_sub(1);
        let last_col = cols.get().saturating_sub(1);
        let start_row = start.0.min(last_row);
        let end_row = end.0.min(last_row);
        let mut out = String::new();
        for row in start_row..=end_row {
            let first = if row == start_row { start.1 } else { 0 };
            let last = if row == end_row { end.1.min(last_col) } else { last_col };
            let mut line = String::new();
            let mut col = first;
            while col <= last {
                if let Some(cell) = screen.cell(row, col) {
                    if !cell.is_wide_continuation() {
                        line.push_str(cell.contents());
                    }
                }
                col += 1;
            }
            out.push_str(line.trim_end());
            if row != end_row {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        match &self.backend {
            SessionBackend::Local(pty) => {
                let mut writer = pty.writer.lock().expect("writer lock");
                writer.write_all(bytes)?;
                writer.flush()?;
                Ok(())
            }
            SessionBackend::Remote(pane) => pane
                .link
                .send(crate::backend::remote_client::RemoteCommand::Input(
                    bytes.to_vec(),
                )),
        }
    }

    pub fn write_str(&self, text: &str) -> Result<()> {
        self.write(text.as_bytes())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        match &mut self.backend {
            SessionBackend::Local(pty) => pty.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?,
            // The peer resizes its own PTY to match this pane.
            SessionBackend::Remote(pane) => pane
                .link
                .send(crate::backend::remote_client::RemoteCommand::Resize {
                    cols,
                    rows,
                })?,
        }
        self.parser.set_size(
            NonZeroU16::new(rows).expect("rows > 0"),
            NonZeroU16::new(cols).expect("cols > 0"),
        );
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn screen_text(&self) -> String {
        self.parser.screen().contents()
    }

    pub fn visible_lines(&self) -> Vec<String> {
        let (rows, cols) = self.parser.screen().size();
        let rows = rows.get();
        let cols = cols.get();
        (0..rows)
            .map(|row| {
                let mut line = String::with_capacity(cols as usize);
                for col in 0..cols {
                    if let Some(cell) = self.parser.screen().cell(row, col) {
                        if cell.is_wide_continuation() {
                            continue;
                        }
                        line.push_str(cell.contents());
                    }
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn exit_code(&self) -> Option<i32> {
        match &self.backend {
            SessionBackend::Local(pty) => *pty.exit_code.lock().expect("exit lock"),
            // A remote pane hosts no local process; the link itself reports down.
            SessionBackend::Remote(_) => None,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        match &self.backend {
            SessionBackend::Local(pty) => pty.pid,
            SessionBackend::Remote(_) => None,
        }
    }

    /// Scroll the viewport into scrollback (`0` is the live screen).
    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn set_scrollback(&mut self, offset: usize) {
        self.parser.screen_mut().set_scrollback(offset);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn kill(&mut self) {
        match &mut self.backend {
            SessionBackend::Local(pty) => {
                // Portable-pty only kills the shell; the shell's foreground job may
                // ignore SIGHUP. Take the process-group id before the shell dies.
                #[cfg(unix)]
                let pgid = pty.master.process_group_leader();
                let _ = pty.killer.kill();
                #[cfg(unix)]
                if let Some(pgid) = pgid {
                    terminate_process_group(pgid);
                }
            }
            SessionBackend::Remote(pane) => pane.link.close(),
        }
    }

    pub fn wait_for_text(&mut self, needle: &str, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            self.pump();
            if self.screen_text().contains(needle) {
                return true;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        self.pump();
        self.screen_text().contains(needle)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    pending: Arc<Mutex<Vec<u8>>>,
    dirty: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("spritx-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let got = std::time::Instant::now();
                        let mut pending = pending.lock().expect("pending lock");
                        pending.extend_from_slice(&buf[..n]);
                        if pending.len() > MAX_PENDING {
                            let drop = pending.len() - MAX_PENDING;
                            pending.drain(..drop);
                        }
                        dirty.store(true, Ordering::Relaxed);
                        drop(pending);
                        let handoff = crate::metrics::micros(got.elapsed());
                        crate::metrics::add(crate::metrics::Name::PtyRead, handoff as u64);
                        signal_wake();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            dirty.store(true, Ordering::Relaxed);
            signal_wake();
        })
        .expect("spawn pty reader");
}

fn spawn_reaper(mut child: Box<dyn Child + Send + Sync>, exit_code: Arc<Mutex<Option<i32>>>) {
    thread::Builder::new()
        .name("spritx-pty-reaper".into())
        .spawn(move || {
            if let Ok(status) = child.wait() {
                *exit_code.lock().expect("exit lock") = Some(status.exit_code() as i32);
            }
        })
        .expect("spawn pty reaper");
}

#[cfg(unix)]
fn terminate_process_group(pgid: libc::pid_t) {
    if pgid <= 1 {
        return;
    }
    let alive = move || unsafe { libc::killpg(pgid, 0) } == 0;
    unsafe {
        libc::killpg(pgid, libc::SIGHUP);
    }
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(1000));
        if !alive() {
            return;
        }
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    });
}

#[cfg(test)]
mod mouse_tests {
    use super::*;

    fn sgr(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("utf8")
    }

    #[test]
    fn sgr_press_release_and_wheel() {
        let mode = MouseProtocolMode::PressRelease;
        assert_eq!(
            encode_mouse_event(mode, MouseProtocolEncoding::Sgr, 9, 4, MouseAction::Press { button: mouse_button::LEFT }, MouseMods::default())
                .map(sgr),
            Some("\x1b[<0;10;5M".to_string())
        );
        assert_eq!(
            encode_mouse_event(mode, MouseProtocolEncoding::Sgr, 9, 4, MouseAction::Release { button: mouse_button::LEFT }, MouseMods::default())
                .map(sgr),
            Some("\x1b[<0;10;5m".to_string())
        );
        assert_eq!(
            encode_mouse_event(mode, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Wheel { button: mouse_button::WHEEL_UP }, MouseMods::default())
                .map(sgr),
            Some("\x1b[<64;1;1M".to_string())
        );
        assert_eq!(
            encode_mouse_event(mode, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Wheel { button: mouse_button::WHEEL_DOWN }, MouseMods::default())
                .map(sgr),
            Some("\x1b[<65;1;1M".to_string())
        );
    }

    #[test]
    fn modifiers_are_encoded() {
        let mods = MouseMods { shift: true, alt: true, ctrl: true };
        assert_eq!(
            encode_mouse_event(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr, 3, 2, MouseAction::Press { button: mouse_button::RIGHT }, mods)
                .map(sgr),
            Some("\x1b[<30;4;3M".to_string())
        );
    }

    #[test]
    fn mode_filters_events() {
        // X10 (Press) reports presses and wheel, but not releases or motion.
        assert!(encode_mouse_event(MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Release { button: mouse_button::LEFT }, MouseMods::default()).is_none());
        assert!(encode_mouse_event(MouseProtocolMode::Press, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Motion { button: Some(mouse_button::LEFT) }, MouseMods::default()).is_none());
        assert!(encode_mouse_event(MouseProtocolMode::None, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Press { button: mouse_button::LEFT }, MouseMods::default()).is_none());
        // AnyMotion reports bare motion (button code 35).
        assert_eq!(
            encode_mouse_event(MouseProtocolMode::AnyMotion, MouseProtocolEncoding::Sgr, 0, 0, MouseAction::Motion { button: None }, MouseMods::default())
                .map(sgr),
            Some("\x1b[<35;1;1M".to_string())
        );
    }

    #[test]
    fn legacy_encoding_offsets_cells() {
        // ESC [ M, then button+32, col+1+32, row+1+32.
        let bytes = encode_mouse_event(
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
            0,
            0,
            MouseAction::Press { button: mouse_button::LEFT },
            MouseMods::default(),
        )
        .expect("encoded");
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 33, 33]);
        // A release in the legacy encoding is button 3.
        let bytes = encode_mouse_event(
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
            4,
            2,
            MouseAction::Release { button: mouse_button::LEFT },
            MouseMods::default(),
        )
        .expect("encoded");
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 3 + 32, 5 + 32, 3 + 32]);
    }
}
