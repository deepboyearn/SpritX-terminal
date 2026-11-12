//! Directory listing, hidden files, path helpers.
//!
//! Mirrors the Tauri `read_dir` command (`src-tauri/src/complete.rs`) for the
//! file-tree sidebar, plus a few helpers the GPUI screens need: home-directory
//! resolution, `~` expansion, and cheap "is this a text file?" checks for the
//! inline preview pane.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// The sidebar only ever renders a few thousand rows; matching the Tauri
/// backend's cap keeps a directory with a million entries from stalling the
/// UI thread.
pub const MAX_ENTRIES: usize = 5000;

/// Default cap for [`read_text_preview`].
pub const MAX_TEXT_PREVIEW_BYTES: usize = 64 * 1024;

/// Files larger than this are never treated as text, even if their extension
/// says otherwise (a 2 GiB `.log` would still hang the preview).
pub const MAX_TEXT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// One row of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Size in bytes (0 for directories or when metadata could not be read).
    pub size: u64,
    /// Whether the entry itself is a symbolic link.
    pub symlink: bool,
}

/// List `path` for the file tree.
///
/// Hidden entries (leading dot) are omitted unless `show_hidden` is set — this
/// is the only filtering the Tauri backend does, so `.git` is hidden by the
/// dot rule but no VCS-specific noise is skipped. Entries that vanish or whose
/// metadata cannot be read (per-entry permission errors) are skipped rather
/// than failing the whole listing. Directories sort first, then names
/// case-insensitively, with the byte-wise name as a stable tie-breaker.
pub fn list_dir(path: impl AsRef<Path>, show_hidden: bool) -> io::Result<Vec<FsEntry>> {
    let base = path.as_ref();
    let mut candidates = Vec::new();

    for entry in fs::read_dir(base)? {
        // A directory can be readable as a whole while individual entries are
        // not (ACLs, races with deletion). Skip those instead of failing.
        let Ok(entry) = entry else { continue };

        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        candidates.push((name, entry));
        if candidates.len() >= MAX_ENTRIES {
            break;
        }
    }

    // Per-entry work is one blocking `file_type`/`metadata` syscall each, and
    // on a network or mapped drive every stat is a round-trip: a 5000-entry
    // directory cost 5000 sequential round-trips before this. Stats are pure
    // and independent, so the fan-out is safe; the collected `DirEntry`s keep
    // read order, and the results are re-sorted by name below, so the output
    // stays byte-for-byte identical to the serial version. The cap is applied
    // to `candidates` first, so the parallel stage is bounded to
    // `MAX_ENTRIES` rows and can never emit more.
    let mut out: Vec<FsEntry> = candidates
        .into_par_iter()
        .filter_map(|(name, entry)| {
            let file_type = entry.file_type().ok()?;
            let symlink = file_type.is_symlink();
            let path = entry.path();
            // `file_type` does not follow symlinks, so resolve links to decide
            // whether the row should be expandable.
            let is_dir = file_type.is_dir() || (symlink && path.is_dir());
            // `DirEntry::metadata` reports the link itself on Unix, so follow
            // symlinks explicitly to show the target's size.
            let size = if is_dir {
                0
            } else {
                fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            };

            Some(FsEntry {
                name,
                path,
                is_dir,
                size,
                symlink,
            })
        })
        .collect();

    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// The current user's home directory, from `$HOME` (Unix) or
/// `$USERPROFILE` / `$HOMEDRIVE$HOMEPATH` (Windows).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            if !home.is_empty() {
                return Some(PathBuf::from(home));
            }
        }
        let drive = std::env::var_os("HOMEDRIVE")?;
        let path = std::env::var_os("HOMEPATH")?;
        let mut combined = drive;
        combined.push(path);
        return Some(PathBuf::from(combined));
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    }
}

/// Expand a leading `~` or `~/` (also `~\`) to the user's home directory.
/// Other paths — including `~user` — are returned unchanged.
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        return match home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(input),
        };
    }
    PathBuf::from(input)
}

const BINARY_EXTENSIONS: &[&str] = &[
    "7z", "a", "avi", "bin", "bmp", "bz2", "class", "dat", "db", "dll", "dylib", "eot", "exe",
    "flac", "gif", "gz", "ico", "jar", "jpeg", "jpg", "m4a", "mkv", "mov", "mp3", "mp4", "o",
    "obj", "ogg", "otf", "pdf", "png", "pyc", "rar", "rlib", "rmeta", "so", "sqlite", "sqlite3",
    "tar", "tgz", "ttf", "wasm", "wav", "webm", "webp", "woff", "woff2", "xz", "zip", "zst",
];

const TEXT_EXTENSIONS: &[&str] = &[
    "bash", "c", "cc", "cfg", "cjs", "conf", "cpp", "cs", "css", "csv", "dockerfile", "edn",
    "env", "fish", "go", "gradle", "h", "hpp", "htm", "html", "ini", "java", "js", "json",
    "jsonc", "jsx", "kt", "kts", "less", "lock", "log", "lua", "md", "mjs", "mk", "php", "properties",
    "py", "rb", "rs", "sass", "scss", "sh", "sql", "svg", "svelte", "swift", "toml", "ts", "tsv",
    "tsx", "txt", "vue", "xml", "yaml", "yml", "zsh",
];

const TEXT_NAMES: &[&str] = &[
    ".env",
    ".gitattributes",
    ".gitignore",
    ".gitmodules",
    "cargo.lock",
    "changelog",
    "cmakelists.txt",
    "dockerfile",
    "justfile",
    "license",
    "makefile",
    "procfile",
    "readme",
];

/// Cheap heuristic used before showing a file preview.
///
/// Rejects files over [`MAX_TEXT_FILE_BYTES`], known binary extensions, and
/// content whose first few KiB contain a NUL byte. Known text extensions and
/// well-known extension-less filenames (Makefile, Cargo.lock, …) short-circuit
/// without reading the file.
pub fn is_probably_text(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_TEXT_FILE_BYTES {
        return false;
    }
    if meta.len() == 0 {
        return true;
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());

    match extension {
        Some(ext) if BINARY_EXTENSIONS.contains(&ext.as_str()) => return false,
        Some(ext) if TEXT_EXTENSIONS.contains(&ext.as_str()) => return true,
        None if TEXT_NAMES.contains(&file_name.as_str()) => return true,
        _ => {}
    }

    sniff_text(path)
}

fn sniff_text(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 4096];
    let Ok(n) = file.take(head.len() as u64).read(&mut head) else {
        return false;
    };
    let head = &head[..n];
    if head.is_empty() {
        return true;
    }
    head.iter().all(|&b| {
        b == 0x1b
            || b == b'\n'
            || b == b'\r'
            || b == b'\t'
            || b == 0x0c
            || b >= 0x20
            || b == 0x08
    })
}

/// Read at most `max_bytes` from the start of a file as lossy UTF-8.
///
/// Use [`MAX_TEXT_PREVIEW_BYTES`] for the default cap. The returned string may
/// contain a replacement character where the cap splits a multi-byte sequence.
pub fn read_text_preview(
    path: impl AsRef<Path>,
    max_bytes: usize,
) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut buf = Vec::with_capacity(max_bytes.min(MAX_TEXT_PREVIEW_BYTES));
    file.take(max_bytes as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
