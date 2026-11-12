//! Integration tests for the filesystem, git, and OSC/block backends.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use spritx_terminal::backend::files::{
    expand_tilde, home_dir, is_probably_text, list_dir, read_text_preview, MAX_TEXT_FILE_BYTES,
    MAX_TEXT_PREVIEW_BYTES,
};
use spritx_terminal::backend::git::git_info;
use spritx_terminal::backend::osc::{
    BlockTracker, CommandBlock, FeedResult, OscEvent, OscParser, TimedEvent, MAX_OSC_LEN,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "spritx_terminal_{}_{}_{}",
            tag,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn names(result: &FeedResult) -> Vec<OscEvent> {
    result.events.iter().map(|e| e.event.clone()).collect()
}

fn only_events(events: &[TimedEvent]) -> Vec<OscEvent> {
    events.iter().map(|e| e.event.clone()).collect()
}

// ---------------------------------------------------------------------------
// files.rs
// ---------------------------------------------------------------------------

#[test]
fn list_dir_hides_dotfiles_and_sorts_dirs_first() {
    let tmp = TempDir::new("fs_sort");
    fs::create_dir(tmp.path().join("beta")).unwrap();
    fs::create_dir(tmp.path().join("AlphaDir")).unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join("b.txt"), b"b").unwrap();
    fs::write(tmp.path().join("A.txt"), b"a").unwrap();
    fs::write(tmp.path().join(".hidden"), b"h").unwrap();

    let entries = list_dir(tmp.path(), false).unwrap();
    let got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(got, vec!["AlphaDir", "beta", "A.txt", "b.txt"]);
    assert!(entries[0].is_dir);
    assert!(!entries[2].is_dir);
    assert_eq!(entries[0].path, tmp.path().join("AlphaDir"));
    assert_eq!(entries[2].size, 1);

    let all = list_dir(tmp.path(), true).unwrap();
    let all_names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
    assert!(all_names.contains(&".git"));
    assert!(all_names.contains(&".hidden"));
}

#[test]
fn list_dir_is_deterministic_with_mixed_names_and_hidden_filtering() {
    let tmp = TempDir::new("fs_parallel_determinism");
    // A mix that exercises every branch the parallel fan-out can take: real
    // directories, plain files, a dotfile (hidden filtering), and a name with
    // a space plus a non-ASCII character (lossy `to_string_lossy` round-trip).
    fs::create_dir(tmp.path().join("zdir")).unwrap();
    fs::create_dir(tmp.path().join(".hidden_dir")).unwrap();
    fs::write(tmp.path().join("zeta.txt"), b"zzz").unwrap();
    fs::write(tmp.path().join("Alpha.txt"), b"a").unwrap();
    fs::write(tmp.path().join(".dotfile"), b"dot").unwrap();
    fs::write(tmp.path().join("späce name ünicode.txt"), b"uni").unwrap();

    let visible = list_dir(tmp.path(), false).unwrap();
    let got: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
    // Directories first (case-insensitive), then files (case-insensitive),
    // with the byte-wise name as the tie-breaker — and no dot entries.
    // `späce …` sorts before `zeta.txt` because `s` < `z`.
    assert_eq!(got, vec!["zdir", "Alpha.txt", "späce name ünicode.txt", "zeta.txt"]);
    assert!(!got.iter().any(|n| n.starts_with('.')));
    // Directories report size 0; files report their real length.
    assert_eq!(visible[0].size, 0);
    assert_eq!(visible[1].size, 1);

    // The space + non-ASCII name must round-trip exactly, not lossily.
    let unicode = visible
        .iter()
        .find(|e| e.name == "späce name ünicode.txt")
        .expect("unicode name preserved");
    assert_eq!(unicode.path, tmp.path().join("späce name ünicode.txt"));

    // The same directory listed again must produce an identical vector even
    // though the stats now run across the rayon pool: the sort, not the
    // collect order, defines the output.
    let again = list_dir(tmp.path(), false).unwrap();
    assert_eq!(visible, again);

    // With hidden files shown, the dot entries appear — still dirs first, and
    // dot names sort before letters within each group (`.` is below `A`/`z`).
    let all = list_dir(tmp.path(), true).unwrap();
    let all_names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        all_names,
        vec![
            ".hidden_dir",
            "zdir",
            ".dotfile",
            "Alpha.txt",
            "späce name ünicode.txt",
            "zeta.txt"
        ]
    );
    let all_again = list_dir(tmp.path(), true).unwrap();
    assert_eq!(all, all_again);
}

#[test]
fn list_dir_missing_directory_errors() {
    let tmp = TempDir::new("fs_missing");
    assert!(list_dir(tmp.path().join("nope"), false).is_err());
}

#[cfg(unix)]
#[test]
fn list_dir_reports_symlinks_and_dangling_links() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new("fs_symlink");
    fs::write(tmp.path().join("real.txt"), b"hello").unwrap();
    fs::create_dir(tmp.path().join("real_dir")).unwrap();
    symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt")).unwrap();
    symlink(tmp.path().join("real_dir"), tmp.path().join("link_dir")).unwrap();
    symlink(tmp.path().join("gone"), tmp.path().join("dangling")).unwrap();

    // A dangling symlink must not fail the whole listing.
    let entries = list_dir(tmp.path(), false).unwrap();
    let link = entries.iter().find(|e| e.name == "link.txt").unwrap();
    assert!(link.symlink);
    assert!(!link.is_dir);
    assert_eq!(link.size, 5);

    let dir_link = entries.iter().find(|e| e.name == "link_dir").unwrap();
    assert!(dir_link.symlink);
    assert!(dir_link.is_dir);

    let dangling = entries.iter().find(|e| e.name == "dangling").unwrap();
    assert!(dangling.symlink);
    assert!(!dangling.is_dir);
    assert_eq!(dangling.size, 0);
}

#[test]
fn expand_tilde_handles_home_forms() {
    if let Some(home) = home_dir() {
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/proj"), home.join("proj"));
        assert_eq!(expand_tilde("~\\proj"), home.join("proj"));
    }
    assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
    assert_eq!(expand_tilde("relative"), PathBuf::from("relative"));
}

#[test]
fn is_probably_text_uses_size_extension_and_content() {
    let tmp = TempDir::new("fs_text");
    fs::write(tmp.path().join("notes.txt"), b"hello").unwrap();
    fs::write(tmp.path().join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(tmp.path().join("Makefile"), b"all:\n\ttrue\n").unwrap();
    fs::write(tmp.path().join("Cargo.lock"), b"[[package]]\n").unwrap();
    fs::write(tmp.path().join("blob"), b"abc\0def").unwrap();
    fs::write(tmp.path().join("plain"), b"just words\n").unwrap();

    assert!(is_probably_text(tmp.path().join("notes.txt")));
    assert!(!is_probably_text(tmp.path().join("image.png")));
    assert!(is_probably_text(tmp.path().join("Makefile")));
    assert!(is_probably_text(tmp.path().join("Cargo.lock")));
    assert!(!is_probably_text(tmp.path().join("blob")));
    assert!(is_probably_text(tmp.path().join("plain")));
    assert!(!is_probably_text(tmp.path().join("does-not-exist.txt")));

    let huge = tmp.path().join("huge.log");
    let file = fs::File::create(&huge).unwrap();
    file.set_len(MAX_TEXT_FILE_BYTES + 1).unwrap();
    assert!(!is_probably_text(&huge));
}

#[test]
fn read_text_preview_respects_byte_cap() {
    let tmp = TempDir::new("fs_preview");
    let path = tmp.path().join("data.txt");
    fs::write(&path, b"0123456789abcdefghij").unwrap();

    assert_eq!(read_text_preview(&path, 10).unwrap(), "0123456789");
    assert_eq!(read_text_preview(&path, 0).unwrap(), "");
    assert_eq!(
        read_text_preview(&path, MAX_TEXT_PREVIEW_BYTES).unwrap(),
        "0123456789abcdefghij"
    );
    assert!(read_text_preview(tmp.path().join("missing"), 10).is_err());
}

// ---------------------------------------------------------------------------
// git.rs
// ---------------------------------------------------------------------------

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "SpritX Test")
        .env("GIT_AUTHOR_EMAIL", "test@spritx.invalid")
        .env("GIT_COMMITTER_NAME", "SpritX Test")
        .env("GIT_COMMITTER_EMAIL", "test@spritx.invalid")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn init_repo(dir: &Path) -> bool {
    if !git(dir, &["init", "-q"]) || !git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]) {
        return false;
    }
    fs::write(dir.join("tracked.txt"), b"one\n").unwrap();
    git(dir, &["add", "tracked.txt"]) && git(dir, &["commit", "-q", "-m", "init"])
}

#[test]
fn git_info_reports_branch_root_and_dirty() {
    if !git_available() {
        eprintln!("git not available; skipping");
        return;
    }
    let tmp = TempDir::new("git_repo");
    if !init_repo(tmp.path()) {
        eprintln!("git init failed; skipping");
        return;
    }

    let info = git_info(tmp.path()).expect("repo info");
    assert_eq!(info.branch, "main");
    assert!(!info.detached);
    assert!(!info.dirty);
    assert_eq!(info.ahead, 0);
    assert_eq!(info.behind, 0);
    assert_eq!(
        info.root.canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap()
    );

    fs::write(tmp.path().join("untracked.txt"), b"new\n").unwrap();
    assert!(git_info(tmp.path()).unwrap().dirty);

    // From a subdirectory the root is still reported and the dirty flag seen.
    fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
    let nested = git_info(tmp.path().join("src/deep")).unwrap();
    assert_eq!(nested.branch, "main");
    assert_eq!(
        nested.root.canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap()
    );
}

#[test]
fn git_info_reports_detached_head() {
    if !git_available() {
        return;
    }
    let tmp = TempDir::new("git_detached");
    if !init_repo(tmp.path()) || !git(tmp.path(), &["checkout", "-q", "--detach", "HEAD"]) {
        return;
    }
    let info = git_info(tmp.path()).unwrap();
    assert!(info.detached);
    assert_eq!(info.branch.len(), 7);
    assert!(info.branch.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn git_info_reports_ahead_count() {
    if !git_available() {
        return;
    }
    let tmp = TempDir::new("git_ahead");
    let repo = tmp.path().join("repo");
    let remote = tmp.path().join("remote.git");
    fs::create_dir(&repo).unwrap();
    if !init_repo(&repo)
        || !std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return;
    }
    let remote_s = remote.to_string_lossy().to_string();
    if !git(&repo, &["remote", "add", "origin", &remote_s])
        || !git(&repo, &["push", "-q", "-u", "origin", "main"])
    {
        return;
    }

    fs::write(repo.join("tracked.txt"), b"two\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "second"]);

    let info = git_info(&repo).unwrap();
    assert_eq!(info.ahead, 1);
    assert_eq!(info.behind, 0);
    assert!(!info.dirty);
}

#[test]
fn git_info_falls_back_to_head_files() {
    let tmp = TempDir::new("git_head");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("src/deep")).unwrap();
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/login\n").unwrap();

    let info = git_info(repo.join("src/deep")).expect("fallback info");
    assert_eq!(info.branch, "feature/login");
    assert!(!info.detached);
    assert!(!info.dirty);
    assert_eq!(info.root, repo);

    // Detached HEAD written as a raw hash.
    fs::write(
        repo.join(".git/HEAD"),
        "3758353a1b2c3d4e5f60718293a4b5c6d7e8f901\n",
    )
    .unwrap();
    let detached = git_info(&repo).unwrap();
    assert!(detached.detached);
    assert_eq!(detached.branch, "3758353");

    // `.git` file (worktree/submodule) pointing at another git dir.
    let wt = tmp.path().join("wt");
    fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
    fs::write(
        repo.join(".git/worktrees/wt/HEAD"),
        "ref: refs/heads/hotfix\n",
    )
    .unwrap();
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
    let worktree = git_info(&wt).unwrap();
    assert_eq!(worktree.branch, "hotfix");
    assert_eq!(worktree.root, wt);

    assert!(git_info(tmp.path().join("not-a-repo")).is_none());
}

// ---------------------------------------------------------------------------
// osc.rs — parser
// ---------------------------------------------------------------------------

#[test]
fn prompt_start_bel_is_forwarded() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;A\x07");
    assert_eq!(names(&result), vec![OscEvent::PromptStart]);
    assert_eq!(result.passthrough, b"\x1b]133;A\x07");
}

#[test]
fn prompt_end_is_forwarded() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;B\x07");
    assert_eq!(names(&result), vec![OscEvent::PromptEnd]);
    assert_eq!(result.passthrough, b"\x1b]133;B\x07");
}

#[test]
fn output_end_carries_exit_code_and_offset() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"hello\x1b]133;D;42\x07world");
    assert_eq!(names(&result), vec![OscEvent::OutputEnd { exit_code: 42 }]);
    assert_eq!(result.passthrough, b"helloworld");
    assert_eq!(result.events[0].passthrough_idx, 5);
}

#[test]
fn output_end_without_code_defaults_to_zero() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;D\x07");
    assert_eq!(names(&result), vec![OscEvent::OutputEnd { exit_code: 0 }]);
    assert!(result.passthrough.is_empty());
}

#[test]
fn st_terminator_is_accepted() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;C\x1b\\");
    assert_eq!(names(&result), vec![OscEvent::OutputStart]);
    assert!(result.passthrough.is_empty());
}

#[test]
fn sequence_split_across_reads() {
    let mut parser = OscParser::new();
    let first = parser.feed(b"\x1b]13");
    assert!(first.events.is_empty());
    assert!(first.passthrough.is_empty());
    let second = parser.feed(b"3;A\x07");
    assert_eq!(names(&second), vec![OscEvent::PromptStart]);
}

#[test]
fn byte_at_a_time_command_line() {
    let mut parser = OscParser::new();
    let input = b"\x1b]133;E;echo hi\x07";
    let mut events = Vec::new();
    let mut passthrough = Vec::new();
    for &byte in input {
        let result = parser.feed(&[byte]);
        events.extend(only_events(&result.events));
        passthrough.extend_from_slice(&result.passthrough);
    }
    assert_eq!(
        events,
        vec![OscEvent::CommandLine("echo hi".to_string())]
    );
    assert!(passthrough.is_empty());
}

#[test]
fn non_osc_osc_is_preserved() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]0;window title\x07after");
    assert!(result.events.is_empty());
    assert_eq!(result.passthrough, b"\x1b]0;window title\x07after");
}

#[test]
fn csi_and_plain_bytes_pass_through_unchanged() {
    let mut parser = OscParser::new();
    let input = b"\x1b[31mred\x1b[0m plain\r\n";
    let result = parser.feed(input);
    assert!(result.events.is_empty());
    assert_eq!(result.passthrough, input);
}

#[test]
fn osc7_file_url_is_decoded_and_forwarded() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]7;file://machine/home/user/project\x07");
    assert_eq!(
        names(&result),
        vec![OscEvent::Cwd("/home/user/project".to_string())]
    );
    assert!(result.passthrough.starts_with(b"\x1b]7;"));
}

#[test]
fn osc7_normalizes_windows_drive_and_percent_escapes() {
    let mut parser = OscParser::new();
    let win = parser.feed(b"\x1b]7;file://HOST/C:/Users/me\x07");
    assert_eq!(
        names(&win),
        vec![OscEvent::Cwd("C:/Users/me".to_string())]
    );

    let spaced = parser.feed(b"\x1b]7;file:///tmp/with%20space\x07");
    assert_eq!(
        names(&spaced),
        vec![OscEvent::Cwd("/tmp/with space".to_string())]
    );
}

#[test]
fn osc7_accepts_plain_paths_and_st_terminator() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]7;/tmp/plain\x1b\\");
    assert_eq!(names(&result), vec![OscEvent::Cwd("/tmp/plain".to_string())]);
    // Non-133 sequences are re-emitted with BEL, which every terminal accepts.
    assert_eq!(result.passthrough, b"\x1b]7;/tmp/plain\x07");
}

#[test]
fn command_line_keeps_semicolons_and_allows_empty() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;E;ls -la; echo hi\x07");
    assert_eq!(
        names(&result),
        vec![OscEvent::CommandLine("ls -la; echo hi".to_string())]
    );

    let empty = parser.feed(b"\x1b]133;E;\x07");
    assert_eq!(names(&empty), vec![OscEvent::CommandLine(String::new())]);
    assert!(empty.passthrough.is_empty());
}

#[test]
fn aliases_sequence_is_parsed_and_stripped() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]7733;bHM9IGxvbGFsaQ==\x07");
    assert_eq!(
        names(&result),
        vec![OscEvent::Aliases("bHM9IGxvbGFsaQ==".to_string())]
    );
    assert!(result.passthrough.is_empty());
}

#[test]
fn aliases_split_across_reads() {
    let mut parser = OscParser::new();
    assert!(parser.feed(b"\x1b]7733;abc").events.is_empty());
    let result = parser.feed(b"def\x07");
    assert_eq!(
        names(&result),
        vec![OscEvent::Aliases("abcdef".to_string())]
    );
}

#[test]
fn escape_at_end_of_chunk_is_held() {
    let mut parser = OscParser::new();
    let first = parser.feed(b"abc\x1b");
    assert_eq!(first.passthrough, b"abc");
    assert!(first.events.is_empty());
    let second = parser.feed(b"]0;title\x07");
    assert_eq!(second.passthrough, b"\x1b]0;title\x07");
    assert!(second.events.is_empty());
}

#[test]
fn output_capture_offsets_between_c_and_d() {
    let mut parser = OscParser::new();
    let result = parser.feed(b"\x1b]133;C\x07hello world\x1b]133;D;0\x07");
    assert_eq!(
        names(&result),
        vec![OscEvent::OutputStart, OscEvent::OutputEnd { exit_code: 0 }]
    );
    assert_eq!(result.passthrough, b"hello world");
    assert_eq!(result.events[0].passthrough_idx, 0);
    assert_eq!(result.events[1].passthrough_idx, 11);
}

#[test]
fn runaway_osc_is_capped_then_parser_recovers() {
    let mut parser = OscParser::new();
    let mut bytes = vec![0x1b, b']'];
    bytes.extend(std::iter::repeat(b'a').take(MAX_OSC_LEN + 100));
    let result = parser.feed(&bytes);
    assert!(result.events.is_empty());
    assert!(result.passthrough.len() > MAX_OSC_LEN);

    let after = parser.feed(b"\x1b]133;A\x07");
    assert_eq!(names(&after), vec![OscEvent::PromptStart]);
}

#[test]
fn runaway_esc_overflow_keeps_current_byte() {
    let mut parser = OscParser::new();
    let mut bytes = vec![0x1b, b']'];
    bytes.extend(std::iter::repeat(b'a').take(MAX_OSC_LEN));
    bytes.push(0x1b);
    bytes.push(b'x');
    let result = parser.feed(&bytes);
    assert!(result.events.is_empty());
    assert!(result.passthrough.ends_with(b"\x1bx"));
}

// ---------------------------------------------------------------------------
// osc.rs — block tracking
// ---------------------------------------------------------------------------

fn complete_block(tracker: &mut BlockTracker) -> Vec<CommandBlock> {
    tracker.take_blocks()
}

#[test]
fn tracker_builds_one_block_with_command_output_and_exit() {
    let mut tracker = BlockTracker::new();
    let before = SystemTime::now();
    tracker.feed(
        b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;E;ls -la\x07\x1b]133;C\x07total 0\r\n\x1b]133;D;0\x07",
    );
    let blocks = complete_block(&mut tracker);
    assert_eq!(blocks.len(), 1);
    let block = &blocks[0];
    assert_eq!(block.command.as_deref(), Some("ls -la"));
    assert_eq!(block.output, "total 0\r\n");
    assert_eq!(block.exit_code, Some(0));
    assert_eq!(block.cwd, None);
    assert!(block.started_at >= before && block.started_at <= SystemTime::now());
}

#[test]
fn tracker_dedups_repeated_prompt_markers() {
    let mut tracker = BlockTracker::new();
    for _ in 0..5 {
        tracker.feed(b"\x1b]133;A\x07");
        assert!(tracker.take_blocks().is_empty());
    }
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;true\x07\x1b]133;C\x07\x1b]133;D;0\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command.as_deref(), Some("true"));
}

#[test]
fn tracker_assembles_output_across_split_reads() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;make\x07");
    tracker.feed(b"\x1b]133;C\x07build");
    tracker.feed(b" output");
    tracker.feed(b"\x1b]133;D;2\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].output, "build output");
    assert_eq!(blocks[0].exit_code, Some(2));
    assert_eq!(blocks[0].command.as_deref(), Some("make"));
}

#[test]
fn tracker_strips_markers_from_captured_output() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;echo hi\x07\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks[0].output, "hi\r\n");
    assert!(!blocks[0].output.contains("133;"));
}

#[test]
fn tracker_closes_block_when_output_end_is_missing() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;sleep 1\x07\x1b]133;C\x07partial");
    assert!(tracker.take_blocks().is_empty());
    // A fresh prompt without outputEnd flushes the stale block.
    tracker.feed(b"\x1b]133;A\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command.as_deref(), Some("sleep 1"));
    assert_eq!(blocks[0].output, "partial");
    assert_eq!(blocks[0].exit_code, None);
}

#[test]
fn tracker_tracks_cwd_and_attaches_it_to_blocks() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]7;file://host/home/me/proj\x07\x1b]133;A\x07\x1b]133;E;pwd\x07\x1b]133;C\x07/home/me/proj\r\n\x1b]133;D;0\x07");
    assert_eq!(tracker.current_cwd(), Some("/home/me/proj"));
    let blocks = tracker.take_blocks();
    assert_eq!(blocks[0].cwd.as_deref(), Some("/home/me/proj"));
    assert_eq!(blocks[0].output, "/home/me/proj\r\n");
}

#[test]
fn tracker_collects_multiple_blocks_and_drains() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;one\x07\x1b]133;C\x071\x07\x1b]133;D;0\x07");
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;two\x07\x1b]133;C\x072\x1b]133;D;10\x07");
    tracker.feed(b"\x1b]133;A\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].command.as_deref(), Some("one"));
    assert_eq!(blocks[1].output, "2");
    assert_eq!(blocks[1].exit_code, Some(10));
    assert!(tracker.take_blocks().is_empty());
}

#[test]
fn tracker_ignores_aliases_and_prompt_end() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]7733;YWxpYXM=\x07");
    assert!(tracker.take_blocks().is_empty());
    tracker.feed(b"\x1b]133;E;echo\x07\x1b]133;C\x07ok\x1b]133;D;0\x07");
    assert_eq!(tracker.take_blocks().len(), 1);
}

#[test]
fn tracker_negative_exit_code_is_kept() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;kill -TERM %%1\x07\x1b]133;C\x07\x1b]133;D;143\x07");
    let blocks = tracker.take_blocks();
    assert_eq!(blocks[0].exit_code, Some(143));
}

#[test]
fn tracker_finish_flushes_in_flight_block() {
    let mut tracker = BlockTracker::new();
    tracker.feed(b"\x1b]133;A\x07\x1b]133;E;tail -f log\x07\x1b]133;C\x07line 1\n");
    let block = tracker.finish().expect("flushed block");
    assert_eq!(block.command.as_deref(), Some("tail -f log"));
    assert_eq!(block.output, "line 1\n");
    assert_eq!(block.exit_code, None);
    assert!(tracker.finish().is_none());

    // Output after finish must not panic or leak into a phantom block.
    tracker.feed(b"more");
    assert!(tracker.take_blocks().is_empty());
    assert!(tracker.finish().is_none());
}
