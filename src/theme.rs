use gpui::{Hsla, Pixels, rgb, rgba, px};

/// Workspace chrome colors. The terminal transcript has its own palette.
#[derive(Clone, Copy)]
pub struct Theme {
    pub outer: Hsla,
    pub sidebar: Hsla,
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
            outer: rgb(0x0a0a0c).into(),
            sidebar: rgb(0x0f0f10).into(),
            bg: rgb(0x141415).into(),
            bg_soft: rgb(0x232325).into(),
            border: rgba(0xffffff14).into(),
            fg: rgb(0xededed).into(),
            fg_dim: rgb(0xa1a1a6).into(),
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

    pub fn light() -> Self {
        Self {
            outer: rgb(0xf0f3f0).into(),
            sidebar: rgb(0xf8fdf9).into(),
            bg: rgb(0xffffff).into(),
            bg_soft: rgb(0xf1f5f2).into(),
            border: rgb(0xe5eae6).into(),
            fg: rgb(0x18181b).into(),
            fg_dim: rgb(0x52525b).into(),
            accent: rgb(0x405eb0).into(),
            danger: rgb(0xb91c1c).into(),
            danger_soft: rgb(0x9f1239).into(),
            success: rgb(0x16a34a).into(),
            warning: rgb(0xb45309).into(),
            white_04: rgba(0x0000000a).into(),
            white_06: rgba(0x0000000f).into(),
            white_08: rgba(0x00000014).into(),
            white_16: rgba(0x00000029).into(),
            ..Self::dark()
        }
    }

    pub fn accent_a(&self, alpha: f32) -> Hsla {
        self.accent.opacity(alpha)
    }

    pub fn white_a(&self, alpha: f32) -> Hsla {
        self.fg.opacity(alpha)
    }

    pub fn danger_a(&self, alpha: f32) -> Hsla {
        self.danger.opacity(alpha)
    }
}

pub fn tab_bar_h() -> Pixels {
    px(44.0)
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
