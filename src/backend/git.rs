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
