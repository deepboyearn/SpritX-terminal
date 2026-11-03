//! Persistent configuration (load/save/export/import, 0600 on Unix).
//!
//! Mirrors the Tauri app's `config.toml`: camelCase keys, serde defaults for
//! every field (so an old or partial file still loads), an atomic writer, and
//! an export path that strips API keys. This module has no GPUI dependency.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level application configuration, stored in `$XDG_CONFIG_HOME/spritx/config.toml`
/// (or `~/.config/spritx/config.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppConfig {
    pub appearance: AppearanceConfig,
    pub behavior: BehaviorConfig,
    pub shell: ShellConfig,
    pub notifications: NotificationsConfig,
    pub file_tree: FileTreeConfig,
    pub remote: RemoteSettings,
    pub history: HistorySettings,
    pub keybindings: HashMap<String, String>,
}

impl AppConfig {
    /// A copy with every secret blanked, for backups / sharing.
    pub fn without_secrets(&self) -> Self {
        self.clone()
    }

    /// Load from [`config_path()`] (creating a default file if missing).
    pub fn load() -> Self {
        crate::backend::config::load()
    }

    /// Persist to [`config_path()`] atomically.
    pub fn save(&self) -> Result<(), String> {
        crate::backend::config::save(self)
    }

    /// Pretty JSON, secrets blanked.
    pub fn export_json(&self) -> Result<String, String> {
        crate::backend::config::export_json(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppearanceConfig {
    pub font_family: String,
    pub font_size: u16,
    /// Marks configs written after the terminal default changed to 8px.
    #[serde(default = "legacy_font_size_version")]
    pub font_size_version: u8,
    pub cursor_blink: bool,
    /// `"block" | "bar" | "underline"`.
    pub cursor_style: String,
    pub scrollback: u32,
    pub theme: ThemeConfig,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            // Lead with a family that is actually installed and monospace.
            font_family:
                r#""JetBrainsMonoNL Nerd Font Mono", "JetBrainsMonoNL NFM", "JetBrainsMono Nerd Font", "JetBrains Mono", Menlo, "DejaVu Sans Mono", "Liberation Mono", Consolas, monospace"#
                    .to_string(),
            font_size: 8,
            font_size_version: 1,
            cursor_blink: true,
            cursor_style: "block".to_string(),
            scrollback: 5000,
            theme: ThemeConfig::default(),
        }
    }
}

fn legacy_font_size_version() -> u8 {
    0
}

impl AppearanceConfig {
    /// Older releases persisted their 14px and 12px defaults. Upgrade those
    /// values once, then keep later choices made in Settings (including 14px).
    fn migrate_font_size(&mut self) -> bool {
        if self.font_size_version != 0 {
            return false;
        }
        if matches!(self.font_size, 12 | 14) {
            self.font_size = 8;
        }
        self.font_size_version = 1;
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThemeConfig {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection: String,
    pub accent: String,
    pub black: String,
    pub red: String,
    pub green: String,
    pub yellow: String,
    pub blue: String,
    pub magenta: String,
    pub cyan: String,
    pub white: String,
    pub bright_black: String,
    pub bright_red: String,
    pub bright_green: String,
    pub bright_yellow: String,
    pub bright_blue: String,
    pub bright_magenta: String,
    pub bright_cyan: String,
    pub bright_white: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            background: "#1e1e2e".into(),
            foreground: "#cdd6f4".into(),
            cursor: "#a6e3a1".into(),
            selection: "#45475a".into(),
            accent: "#b4befe".into(),
            black: "#45475a".into(),
            red: "#f38ba8".into(),
            green: "#a6e3a1".into(),
            yellow: "#f9e2af".into(),
            blue: "#89b4fa".into(),
            magenta: "#cba6f7".into(),
            cyan: "#94e2d5".into(),
            white: "#bac2de".into(),
            bright_black: "#585b70".into(),
            bright_red: "#f38ba8".into(),
            bright_green: "#a6e3a1".into(),
            bright_yellow: "#f9e2af".into(),
            bright_blue: "#89b4fa".into(),
            bright_magenta: "#f5c2e7".into(),
            bright_cyan: "#94e2d5".into(),
            bright_white: "#cdd6f4".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BehaviorConfig {
    pub focus_follows_mouse: bool,
    pub show_git_branch: bool,
    pub persist_blocks: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            focus_follows_mouse: false,
            show_git_branch: true,
            persist_blocks: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ShellConfig {
    pub program: Option<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NotificationsConfig {
    pub enabled: bool,
    /// Minimum command duration (seconds) before notifying.
    pub min_duration_sec: u32,
    pub sound: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration_sec: 10,
            sound: true,
        }
    }
}

/// Command templates run by the file-tree context menu. `{path}` is replaced
/// (POSIX-quoted) by the clicked entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FileTreeConfig {
    pub dir_list: String,
    pub dir_open: String,
    pub file_view: String,
    pub file_edit: String,
    pub file_open: String,
}

impl Default for FileTreeConfig {
    #[cfg(windows)]
    fn default() -> Self {
        Self {
            dir_list: "Get-ChildItem -Force -LiteralPath {path}".to_string(),
            dir_open: "Invoke-Item -LiteralPath {path}".to_string(),
            file_view: "Get-Content -LiteralPath {path}".to_string(),
            file_edit: "notepad {path}".to_string(),
            file_open: "notepad {path}".to_string(),
        }
    }

    #[cfg(not(windows))]
    fn default() -> Self {
        Self {
            dir_list: "ls -la {path}".to_string(),
            dir_open: "${EDITOR:-nano} {path}".to_string(),
            file_view: "cat {path}".to_string(),
            file_edit: "nano {path}".to_string(),
            file_open: "${EDITOR:-nano} {path}".to_string(),
        }
    }
}

/// Remote-control settings (this SpritX acting as a server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteSettings {
    pub port: u16,
    pub auto_tunnel: bool,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            port: 4530,
            auto_tunnel: true,
        }
    }
}

/// Rich command-history settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HistorySettings {
    pub enabled: bool,
    /// `*`/`?` patterns (case-insensitive, whole command line) never recorded.
    pub ignore: Vec<String>,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            ignore: default_history_ignore(),
        }
    }
}

pub fn default_history_ignore() -> Vec<String> {
    [
        "*password*",
        "*passwd*",
        "*secret*",
        "*token*",
        "*api_key*",
        "*apikey*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Linux: XDG config directory; Windows: `%APPDATA%/spritx/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    crate::platform::config_dir().map(|dir| dir.join("config.toml"))
}

/// Write via a same-directory temp file + rename. Permissions are forced to
/// 0600 on Unix — the file can hold API keys.
pub fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let tmp = {
        let mut t = path.as_os_str().to_owned();
        t.push(".tmp");
        PathBuf::from(t)
    };
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
}

/// Load the user config, creating a commented default file on first run.
pub fn load() -> AppConfig {
    let Some(path) = config_path() else {
        eprintln!("[spritx] user config directory unavailable — using default config");
        return AppConfig::default();
    };
    if !path.exists() {
        let config = AppConfig::default();
        if let Err(e) = save_to(&path, &config) {
            eprintln!(
                "[spritx] could not create default config at {}: {e}",
                path.display()
            );
        }
        return config;
    }
    let mut config = load_from(&path);
    if config.appearance.migrate_font_size() {
        if let Err(error) = save_to(&path, &config) {
            eprintln!(
                "[spritx] could not save font size migration at {}: {error}",
                path.display()
            );
        }
    }
    config
}

/// Load from an explicit path (no default-file creation).
pub fn load_from(path: &Path) -> AppConfig {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    match std::fs::read_to_string(path) {
        Ok(body) => match toml::from_str::<AppConfig>(&body) {
            Ok(config) => config,
            Err(e) => {
                eprintln!(
                    "[spritx] config parse error in {}: {e} — using defaults",
                    path.display()
                );
                // Keep the broken file around: a later save must not silently
                // destroy what the user could still hand-fix.
                let backup = path.with_extension("toml.broken");
                if std::fs::rename(path, &backup).is_ok() {
                    eprintln!("[spritx] broken config moved to {}", backup.display());
                }
                AppConfig::default()
            }
        },
        Err(e) => {
            eprintln!("[spritx] could not read {}: {e}", path.display());
            AppConfig::default()
        }
    }
}

/// Persist to `config_path()`.
pub fn save(config: &AppConfig) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "user config directory unavailable".to_string())?;
    save_to(&path, config)
}

/// Persist to an explicit path atomically, creating parent directories.
pub fn save_to(path: &Path, config: &AppConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    write_atomic(path, &body).map_err(|e| e.to_string())
}

/// Pretty JSON for backups/sharing, with every API key blanked.
pub fn export_json(config: &AppConfig) -> Result<String, String> {
    serde_json::to_string_pretty(&config.without_secrets()).map_err(|e| e.to_string())
}

/// Write [`export_json`] output to `path`.
pub fn export_json_to(path: &Path, config: &AppConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = export_json(config)?;
    std::fs::write(path, body).map_err(|e| e.to_string())
}

/// Parse a config from a JSON export (falls back to TOML for raw config files).
/// Missing fields are filled from the defaults.
pub fn import_json(body: &str) -> Result<AppConfig, String> {
    serde_json::from_str::<AppConfig>(body)
        .or_else(|_| toml::from_str::<AppConfig>(body))
        .map_err(|e| format!("invalid config file: {e}"))
}

/// Read + parse a config file (JSON export or raw `config.toml`).
pub fn import_file(path: &Path) -> Result<AppConfig, String> {
    let body = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    import_json(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spritx_config_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_match_original() {
        let c = AppConfig::default();
        assert_eq!(c.appearance.font_size, 8);
        assert_eq!(c.appearance.font_size_version, 1);
        assert_eq!(c.appearance.scrollback, 5000);
        assert_eq!(c.appearance.theme.background, "#1e1e2e");
        assert!(!c.behavior.focus_follows_mouse);
        assert!(c.behavior.show_git_branch);
        assert!(c.behavior.persist_blocks);
        assert!(c.shell.program.is_none());
        assert!(c.shell.args.is_empty());
        assert_eq!(c.notifications.min_duration_sec, 10);
        assert_eq!(c.file_tree.dir_list, FileTreeConfig::default().dir_list);
        assert_eq!(c.remote.port, 4530);
        assert!(c.remote.auto_tunnel);
        assert!(c.history.enabled);
        assert!(c.history.ignore.contains(&"*token*".to_string()));
        assert!(c.keybindings.is_empty());
    }

    #[test]
    fn legacy_default_font_sizes_migrate_once() {
        for old_size in [12, 14] {
            let body = format!("[appearance]\nfontSize = {old_size}\n");
            let mut config: AppConfig = toml::from_str(&body).unwrap();
            assert_eq!(config.appearance.font_size_version, 0);
            assert!(config.appearance.migrate_font_size());
            assert_eq!(config.appearance.font_size, 8);
            assert!(!config.appearance.migrate_font_size());

            // A later user choice of 14px must survive a save and reload.
            config.appearance.font_size = 14;
            let saved = toml::to_string(&config).unwrap();
            let mut reloaded: AppConfig = toml::from_str(&saved).unwrap();
            assert!(!reloaded.appearance.migrate_font_size());
            assert_eq!(reloaded.appearance.font_size, 14);
        }

        let mut custom: AppConfig = toml::from_str("[appearance]\nfontSize = 18\n").unwrap();
        assert!(custom.appearance.migrate_font_size());
        assert_eq!(custom.appearance.font_size, 18);
    }

    #[test]
    fn toml_round_trip_uses_camel_case() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.toml");
        let mut config = AppConfig::default();
        config.appearance.font_size = 18;
        config.appearance.cursor_style = "bar".into();
        config.remote.port = 5050;
        config.shell.program = Some("/bin/zsh".into());
        config.shell.args = vec!["-l".into()];
        config.keybindings.insert("palette".into(), "cmd+k".into());

        save_to(&path, &config).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("fontSize = 18"));
        assert!(body.contains("minDurationSec"));
        assert!(body.contains("fileTree"));
        assert!(body.contains("program = \"/bin/zsh\""));

        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.font_size, 18);
        assert_eq!(loaded.appearance.cursor_style, "bar");
        assert_eq!(loaded.remote.port, 5050);
        assert_eq!(loaded.shell.program.as_deref(), Some("/bin/zsh"));
        assert_eq!(
            loaded.keybindings.get("palette").map(String::as_str),
            Some("cmd+k")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_accepts_json_and_toml() {
        let from_json = import_json(
            r#"{"appearance":{"fontSize":20}}"#,
        )
        .unwrap();
        assert_eq!(from_json.appearance.font_size, 20);
        // Unspecified fields fall back to defaults.
        assert_eq!(from_json.remote.port, 4530);

        let from_toml = import_json("[appearance]\nfontSize = 16\n").unwrap();
        assert_eq!(from_toml.appearance.font_size, 16);
    }

    #[test]
    fn broken_file_is_moved_aside() {
        let dir = temp_dir("broken");
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not = valid toml [[[").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.font_size, 8);
        assert!(!path.exists());
        assert!(dir.join("config.toml.broken").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let path = dir.join("nested").join("config.toml");
        save_to(&path, &AppConfig::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.join("nested").join("config.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
