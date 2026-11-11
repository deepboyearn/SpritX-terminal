use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

include!(concat!(env!("OUT_DIR"), "/icons.rs"));

/// The UI's icons, baked into the executable by `build.rs`.
///
/// Nothing is read from disk: every release is a bare binary, so the assets
/// have to be inside it.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = BRAND_IMAGES.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        // `Icon::path` asks for "icons/IconName.svg".
        let name = path.strip_prefix("icons/").unwrap_or(path);
        let name = name.strip_suffix(".svg").unwrap_or(name);
        Ok(ICONS
            .iter()
            .find(|(icon, _)| *icon == name)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let dir = path.trim_end_matches('/');
        Ok(if dir == "brands" {
            BRAND_IMAGES.iter().map(|(name, _)| SharedString::from(
                name.strip_prefix("brands/").unwrap_or(name).to_string()
            )).collect()
        } else if dir.is_empty() || dir == "icons" {
            ICONS
                .iter()
                .map(|(name, _)| SharedString::from(format!("{name}.svg")))
                .collect()
        } else {
            Vec::new()
        })
    }
}

#[cfg(test)]
mod brand_tests {
    use super::*;

    fn pixels(name: &str) -> image::RgbaImage {
        let data = Assets.load(&format!("brands/{name}.png")).unwrap().unwrap();
        image::load_from_memory(&data).unwrap().into_rgba8()
    }

    #[test]
    fn all_launcher_logos_are_embedded_full_color_images() {
        for name in ["Codex", "Claude", "OpenCode", "Pi", "Omp"] {
            let image = pixels(name);
            assert_eq!(image.dimensions(), (128, 128));
            assert!(image.pixels().any(|pixel| pixel[3] > 0));
        }
    }

    #[test]
    fn source_brand_colors_and_gradients_survive_embedding() {
        for (name, colors) in [
            ("Claude", vec![[217, 119, 87]]),
            ("Pi", vec![[240, 144, 130], [77, 154, 191], [241, 190, 88]]),
            ("Omp", vec![[250, 250, 250], [249, 115, 22]]),
        ] {
            let image = pixels(name);
            for color in colors {
                assert!(image.pixels().any(|p| p[3] == 255 && p.0[..3] == color),
                    "{name} lost original color {color:?}");
            }
        }
        let codex = pixels("Codex");
        let colors: std::collections::HashSet<_> = codex.pixels()
            .filter(|p| p[3] == 255).map(|p| p.0).collect();
        assert!(colors.len() > 100, "Codex gradient was reduced to a single tint");
    }
}
