//! Terminal color presets, ported 1:1 from `src/themes.ts`.
//!
//! Each preset carries 21 colors in a fixed order:
//! `background, foreground, cursor, selection, accent,` then the eight normal
//! ANSI colors (`black … white`) and the eight bright ones
//! (`brightBlack … brightWhite`). [`ThemePreset::theme`] turns that into a
//! [`TerminalTheme`] the renderer can use directly.

use gpui::{Hsla, rgb};

use crate::terminal::TerminalTheme;

/// One named terminal theme.
pub struct ThemePreset {
    pub name: &'static str,
    /// 21 `#rrggbb` strings, see the module docs for the order.
    pub colors: [&'static str; 21],
}

impl ThemePreset {
    /// Build the renderer-facing theme.
    pub fn theme(&self) -> TerminalTheme {
        let hex = |index: usize| hsla(self.colors[index]);
        let mut ansi: [Hsla; 16] = [rgb(0).into(); 16];
        for (slot, source) in (5usize..21).enumerate() {
            ansi[slot] = hex(source);
        }
        TerminalTheme {
            background: hex(0),
            foreground: hex(1),
            cursor: hex(2),
            selection: hex(3),
            accent: hex(4),
            ansi,
        }
    }

    /// True when every color matches `other` (case-insensitively), so a loaded
    /// config can be mapped back to the preset it came from.
    pub fn matches(&self, other: &[&str; 21]) -> bool {
        self.colors
            .iter()
            .zip(other.iter())
            .all(|(a, b)| (*a).eq_ignore_ascii_case(b))
    }
}

fn hsla(value: &str) -> Hsla {
    let raw = u32::from_str_radix(value.trim().trim_start_matches('#'), 16).unwrap_or(0);
    rgb(raw).into()
}

/// The presets shown in Settings → Appearance, in display order.
pub const PRESETS: &[ThemePreset] = &[
    ThemePreset {
        name: "SpritX Pastel",
        colors: [
            "#1e1e2e", "#cdd6f4", "#a6e3a1", "#45475a", "#b4befe", "#45475a", "#f38ba8",
            "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#bac2de", "#585b70",
            "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
        ],
    },
    ThemePreset {
        name: "SpritX Dark",
        colors: [
            "#0e1014", "#e6e6e6", "#e6e6e6", "#3a4150", "#4ea1ff", // background, foreground, cursor, selection, accent
            "#2c303a", "#ff5c8d", "#56d364", "#e3b341", "#4ea1ff", "#c678dd", "#56b6c2",
            "#c0c4ce", // black..white
            "#525866", "#ff7ba0", "#7ce081", "#f0c674", "#73b5ff", "#d49eea", "#7fc8d3",
            "#e6e9f0", // bright black..bright white
        ],
    },
    ThemePreset {
        name: "Catppuccin Mocha",
        colors: [
            "#1e1e2e", "#cdd6f4", "#f5e0dc", "#585b70", "#89b4fa", "#45475a", "#f38ba8",
            "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de", "#585b70",
            "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
        ],
    },
    ThemePreset {
        name: "Catppuccin Latte",
        colors: [
            "#eff1f5", "#4c4f69", "#4c4f69", "#ccced9", "#1e66f5", "#5c5f77", "#d20f39",
            "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be", "#6c6f85",
            "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#bcc0cc",
        ],
    },
    ThemePreset {
        name: "Dracula",
        colors: [
            "#282a36", "#f8f8f2", "#f8f8f2", "#44475a", "#bd93f9", "#21222c", "#ff5555",
            "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2", "#6272a4",
            "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
    ThemePreset {
        name: "Nord",
        colors: [
            "#2e3440", "#d8dee9", "#d8dee9", "#434c5e", "#88c0d0", "#3b4252", "#bf616a",
            "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0", "#4c566a",
            "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ],
    },
    ThemePreset {
        name: "Tokyo Night",
        colors: [
            "#1a1b26", "#c0caf5", "#c0caf5", "#283457", "#7aa2f7", "#15161e", "#f7768e",
            "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6", "#414868",
            "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    },
    ThemePreset {
        name: "Gruvbox Dark",
        colors: [
            "#282828", "#ebdbb2", "#ebdbb2", "#504945", "#fabd2f", "#282828", "#cc241d",
            "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984", "#928374",
            "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ],
    },
    ThemePreset {
        name: "Solarized Dark",
        colors: [
            "#002b36", "#93a1a1", "#93a1a1", "#073642", "#268bd2", "#073642", "#dc322f",
            "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5", "#586e75",
            "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    ThemePreset {
        name: "One Dark",
        colors: [
            "#282c34", "#abb2bf", "#528bff", "#3e4451", "#61afef", "#282c34", "#e06c75",
            "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf", "#5c6370",
            "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff",
        ],
    },
    ThemePreset {
        name: "Rosé Pine",
        colors: [
            "#191724", "#e0def4", "#e0def4", "#403d52", "#c4a7e7", "#26233a", "#eb6f92",
            "#3e8fb0", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4", "#6e6a86",
            "#eb6f92", "#3e8fb0", "#f6c177", "#9ccfd8", "#c4a7e7", "#ebbcba", "#e0def4",
        ],
    },
];

#[cfg(test)]
mod tests {
    #[test]
    fn pastel_defaults_match_the_preset() {
        let config = crate::backend::config::AppConfig::default();
        let value = serde_json::to_value(&config.appearance.theme).unwrap();
        let expected = &super::PRESETS[0];
        assert_eq!(expected.name, "SpritX Pastel");
        assert_eq!(value["background"], expected.colors[0]);
        assert_eq!(value["foreground"], expected.colors[1]);
        assert_eq!(value["cursor"], expected.colors[2]);
    }
}
