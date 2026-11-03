use gpui::{Hsla, Pixels, rgb, rgba, px};

/// Chrome colors for the dark SpritX UI. Terminal themes only affect the
/// transcript, matching the web app where themes never repaint the chrome.
#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Hsla,
    pub bg_soft: Hsla,
    pub border: Hsla,
    pub fg: Hsla,
    pub fg_dim: Hsla,
    pub accent: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub on_accent: Hsla,
    pub black_40: Hsla,
    pub black_50: Hsla,
    pub white_04: Hsla,
    pub white_06: Hsla,
    pub white_08: Hsla,
    pub white_16: Hsla,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x1e1e2e).into(),
            bg_soft: rgb(0x181825).into(),
            border: rgb(0x313244).into(),
            fg: rgb(0xcdd6f4).into(),
            fg_dim: rgb(0x9399b2).into(),
            accent: rgb(0xb4befe).into(),
            danger: rgb(0xf38ba8).into(),
            danger_soft: rgb(0xeba0ac).into(),
            success: rgb(0xa6e3a1).into(),
            warning: rgb(0xf9e2af).into(),
            on_accent: rgb(0xffffff).into(),
            black_40: rgba(0x00000066).into(),
            black_50: rgba(0x00000080).into(),
            white_04: rgba(0xffffff0a).into(),
            white_06: rgba(0xffffff0f).into(),
            white_08: rgba(0xffffff14).into(),
            white_16: rgba(0xffffff29).into(),
        }
    }

    pub fn accent_a(&self, alpha: f32) -> Hsla {
        self.accent.opacity(alpha)
    }

    pub fn white_a(&self, alpha: f32) -> Hsla {
        Hsla::from(rgb(0xffffff)).opacity(alpha)
    }

    pub fn danger_a(&self, alpha: f32) -> Hsla {
        self.danger.opacity(alpha)
    }
}

pub fn tab_bar_h() -> Pixels {
    px(34.0)
}

pub fn body_pad() -> Pixels {
    px(8.0)
}

pub fn term_pad_x() -> Pixels {
    px(10.0)
}

pub fn term_pad_y() -> Pixels {
    px(8.0)
}

pub const UI_FONT: &str = "Inter";
#[cfg(windows)]
pub const MONO_FONT: &str = "Consolas";
#[cfg(not(windows))]
pub const MONO_FONT: &str = "JetBrains Mono";
