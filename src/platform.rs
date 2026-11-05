//! Platform defaults shared by persistence, shell sessions and file actions.
use std::path::PathBuf;

pub fn play_completion_sound() {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn MessageBeep(kind: u32) -> i32;
        }
        // Use the user's system information chime.
        unsafe { MessageBeep(0x40) };
    }
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| {
        let _ = notify_rust::Notification::new()
            .summary("SpritX Terminal")
            .body("Terminal command finished")
            .sound_name("complete")
            .show();
    });
}

pub const DEFAULT_SHELL_LABEL: &str = if cfg!(windows) {
    "Default (PowerShell)"
} else {
    "$SHELL (default)"
};

pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("APPDATA")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::backend::files::home_dir().map(|p| p.join("AppData/Roaming")))
            .map(|p| p.join("spritx"));
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::backend::files::home_dir().map(|p| p.join(".config")))
            .map(|p| p.join("spritx"))
    }
}

pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .map(|p| p.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "powershell.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/bash".into())
    }
}

pub fn is_default_shell(value: &str) -> bool {
    value.trim().is_empty() || value == "$SHELL (default)" || value == "Default (PowerShell)"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Posix,
    PowerShell,
    Cmd,
}

pub fn shell_kind(program: &str) -> ShellKind {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    match name.trim_end_matches(".exe") {
        "pwsh" | "powershell" => ShellKind::PowerShell,
        "cmd" => ShellKind::Cmd,
        _ => ShellKind::Posix,
    }
}

/// Quote one path for insertion into the selected shell (never for a host process).
pub fn quote_path(path: &str, kind: ShellKind) -> String {
    match kind {
        ShellKind::PowerShell => format!("'{}'", path.replace('\'', "''")),
        ShellKind::Cmd => {
            // Percent expansion and delayed expansion cannot be made literal by
            // double quotes. Refuse these paths rather than execute their content.
            if path.contains(['%', '!', '"', '\r', '\n']) {
                return String::new();
            }
            format!("\"{path}\"")
        }
        ShellKind::Posix => {
            if !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-+@:=%,~".contains(c))
            {
                path.to_string()
            } else {
                format!("'{}'", path.replace('\'', "'\\''"))
            }
        }
    }
}

/// A JSON array preserves spaces, backslashes and quotes. Accept legacy simple
/// whitespace-separated arguments for existing settings.
pub fn parse_shell_args(value: &str) -> Result<Vec<String>, String> {
    if value.trim_start().starts_with('[') {
        serde_json::from_str(value)
            .map_err(|e| format!("Shell arguments must be a JSON string array: {e}"))
    } else {
        Ok(value.split_whitespace().map(str::to_string).collect())
    }
}
