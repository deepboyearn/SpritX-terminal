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
        Ok(if dir.is_empty() || dir == "icons" {
            ICONS
                .iter()
                .map(|(name, _)| SharedString::from(format!("{name}.svg")))
                .collect()
        } else {
            Vec::new()
        })
    }
}
