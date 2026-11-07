//! Git branch / status helper for the active pane.
//!
//! Fast path: read `.git/HEAD` directly (microseconds, no process spawn) for
//! the branch and repository root. When the `git` binary is available we also
//! ask it for the worktree root and a porcelain-v2 status blob, which gives
//! the dirty flag and ahead/behind counts in a single extra process. Git is
//! always invoked with an argument array (`git -C cwd ...`), never a shell, so
//! paths and branch names cannot be interpreted as shell syntax.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// Branch name, or the short commit hash when HEAD is detached.
    pub branch: String,
    pub detached: bool,
    /// Worktree root (the directory containing `.git`).
    pub root: PathBuf,
    /// True when `git status` reports staged, unstaged, or untracked changes.
    pub dirty: bool,
    /// Commits ahead of the upstream branch (0 when there is no upstream).
    pub ahead: u32,
    /// Commits behind the upstream branch.
    pub behind: u32,
}

/// Inspect `cwd`'s git context.
///
/// Returns `None` when `cwd` is not inside a repository. When the `git` binary
/// is missing or fails (e.g. "dubious ownership"), the `.git/HEAD` fallback
/// still provides branch + root, with `dirty = false` and no ahead/behind.
pub fn git_info(cwd: impl AsRef<Path>) -> Option<GitInfo> {
    let cwd = cwd.as_ref();
    if !cwd.is_dir() {
        return None;
    }

    if let Some(info) = git_cli_info(cwd) {
        return Some(info);
    }
    head_fallback(cwd)
}

fn git_cli_info(cwd: &Path) -> Option<GitInfo> {
    let root_line = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root_text = root_line.lines().next()?.trim();
    if root_text.is_empty() {
        return None;
    }
    let mut info = GitInfo {
        branch: String::new(),
        detached: false,
        root: PathBuf::from(root_text),
        dirty: false,
        ahead: 0,
        behind: 0,
    };

    let mut head_seen = false;
    if let Some(status) = git(cwd, &["status", "--porcelain=v2", "--branch"]) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("# branch.head ") {
                let name = rest.trim();
                if name == "(detached)" {
                    info.detached = true;
                } else if !name.is_empty() && name != "(unknown)" {
                    info.branch = name.to_string();
                    head_seen = true;
                }
            } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
                let oid = rest.trim();
                if info.detached && info.branch.is_empty() && oid.len() >= 7 {
                    info.branch = oid.chars().take(7).collect();
                }
            } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
                for part in rest.split_whitespace() {
                    if let Some(ahead) = part.strip_prefix('+') {
                        info.ahead = ahead.parse().unwrap_or(0);
                    } else if let Some(behind) = part.strip_prefix('-') {
                        info.behind = behind.parse().unwrap_or(0);
                    }
                }
            } else if !line.starts_with('#') && !line.trim().is_empty() {
                info.dirty = true;
            }
        }
    }

    if !head_seen && !info.detached {
        // `git status` can fail while `rev-parse` worked (odd index state);
        // fall back to HEAD for the branch.
        if let Some((git_dir, _)) = find_git_dir(cwd) {
            if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
                if let Some((branch, detached)) = parse_head(&head) {
                    info.branch = branch;
                    info.detached = detached;
                }
            }
        }
    }

    if info.branch.is_empty() {
        return None;
    }
    Some(info)
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
/// Per-path worktree status for `cwd`'s repository, keyed by absolute path.
///
/// WHY: the file tree needs a badge per row, so the whole porcelain-v2 blob is
/// parsed once per tree refresh (off the render thread) instead of spawning
/// `git` per row. Paths come back from git relative to the worktree root; they
/// are re-anchored onto that root so callers can look up expanded filesystem
/// paths directly. Git is invoked with an argument array only, never a shell.
/// Entries that are not inside a repository, and any failure to run `git`
/// (missing binary, dubious ownership, …), yield an empty map — the caller
/// renders no badges, matching the "git not available" pattern elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPathStatus {
    /// Staged, unstaged, or type-change modification.
    Modified,
    /// New tracked file (staged addition).
    Added,
    /// Tracked file missing from the worktree.
    Deleted,
    /// Untracked file (git collapses wholly-untracked directories to one `?`
    /// entry for the directory itself).
    Untracked,
}

pub fn git_path_statuses(cwd: impl AsRef<Path>) -> std::collections::HashMap<PathBuf, GitPathStatus> {
    let mut out = std::collections::HashMap::new();
    let cwd = cwd.as_ref();
    if !cwd.is_dir() {
        return out;
    }
    let root_line = match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Some(line) => line,
        None => return out,
    };
    let root_text = root_line.lines().next().unwrap_or("").trim();
    if root_text.is_empty() {
        return out;
    }
    let root = PathBuf::from(root_text);
    let status = match git(cwd, &["status", "--porcelain=v2", "--untracked-files=normal"]) {
        Some(status) => status,
        None => return out,
    };
    for line in status.lines() {
        let (rel, status) = match parse_porcelain_v2_line(line) {
            Some(parsed) => parsed,
            None => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        out.insert(root.join(rel), status);
    }
    out
}

/// Narrow one porcelain-v2 entry to its worktree-relative path and the status
/// class the file tree badges. Ordinary (`1`), renamed (`2`), untracked (`?`),
/// and ignored (`!`) records are understood; ignored paths never surface
/// because the caller omits `--ignored`. A rename/copied entry names the new
/// path after the tab, which is the path the tree holds.
fn parse_porcelain_v2_line(line: &str) -> Option<(PathBuf, GitPathStatus)> {
    if let Some(rest) = line.strip_prefix("? ") {
        return Some((PathBuf::from(rest.trim()), GitPathStatus::Untracked));
    }
    if line.starts_with('!') || line.starts_with('#') {
        return None;
    }
    // `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` where XY is the
    // index/worktree two-letter status. Renames/copies (`2`) insert a score
    // field before `<orig>\t<path>`; the tree holds the new path after the tab.
    let fields: Vec<&str> = line.splitn(10, ' ').collect();
    if fields.len() < 9 {
        return None;
    }
    let xy = fields[1];
    let status = xy_status(xy)?;
    let path_field = if fields[0] == "2" {
        *fields.get(9)?
    } else {
        fields[8]
    };
    let path = path_field.rsplit('\t').next().unwrap_or(path_field);
    Some((PathBuf::from(path), status))
}

/// Fold porcelain-v2's two status letters into the badge class. Conflicts
/// (`UU`, `AA`, `U*`, …) are badges too — the file definitely needs attention.
fn xy_status(xy: &str) -> Option<GitPathStatus> {
    let bytes = xy.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let (x, y) = (bytes[0], bytes[1]);
    if x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D') {
        return Some(GitPathStatus::Modified);
    }
    match (x, y) {
        (b'A', _) => Some(GitPathStatus::Added),
        (b'D', _) | (_, b'D') => Some(GitPathStatus::Deleted),
        (b'M', _) | (_, b'M') | (b'T', _) | (_, b'T') | (b'R', _) | (_, b'R') | (b'C', _) | (_, b'C') => {
            Some(GitPathStatus::Modified)
        }
        _ => None,
    }
}

fn head_fallback(cwd: &Path) -> Option<GitInfo> {
    let (git_dir, root) = find_git_dir(cwd)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let (branch, detached) = parse_head(&head)?;
    Some(GitInfo {
        branch,
        detached,
        root,
        dirty: false,
        ahead: 0,
        behind: 0,
    })
}

/// Walk up from `start` to the first directory holding a `.git` entry,
/// returning the git directory and the worktree root. A `.git` *file*
/// (worktrees, submodules) points at the real git dir with a
/// `gitdir: <path>` line, relative to the file's directory.
fn find_git_dir(start: &Path) -> Option<(PathBuf, PathBuf)> {
    for dir in start.ancestors() {
        let dot = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&dot) else {
            continue;
        };
        if meta.is_dir() {
            return Some((dot, dir.to_path_buf()));
        }
        if meta.is_file() {
            let content = std::fs::read_to_string(&dot).ok()?;
            let target = content.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            let path = Path::new(target);
            let git_dir = if path.is_absolute() {
                path.to_path_buf()
            } else {
                dir.join(path)
            };
            return Some((git_dir, dir.to_path_buf()));
        }
    }
    None
}

fn parse_head(head: &str) -> Option<(String, bool)> {
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        let branch = r.strip_prefix("refs/heads/").unwrap_or(r);
        if branch.is_empty() {
            return None;
        }
        return Some((branch.to_string(), false));
    }
    if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some((head[..7].to_string(), true));
    }
    None
}
