//! Integration tests for the GPUI config and history backends.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use spritx_terminal::backend::config::{self, AppConfig};
use spritx_terminal::backend::history::{HistoryEntry, HistoryStore, MAX_READ_LINES};

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spritx_backend_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(command: &str, cwd: &str, exit_code: i32, branch: &str, at: i64) -> HistoryEntry {
    HistoryEntry {
        command: command.into(),
        cwd: Some(cwd.into()),
        exit_code,
        duration_ms: 12,
        branch: Some(branch.into()),
        finished_at: at,
    }
}

fn store_with_sample(tag: &str) -> (PathBuf, HistoryStore) {
    let dir = temp_dir(tag);
    let store = HistoryStore::new(dir.join("history.jsonl"));
    for e in [
        entry("php artisan migrate", "/p/palr", 0, "main", 1),
        entry("composer install", "/p/palr", 1, "feat/x", 2),
        entry("git status", "/p/spritx", 0, "main", 3),
        entry("php artisan migrate", "/p/palr", 0, "main", 4),
        entry("composer update", "/p/other", 2, "dev", 5),
    ] {
        store.append(e).unwrap();
    }
    (dir, store)
}

#[test]
fn history_search_newest_first_with_term() {
    let (dir, store) = store_with_sample("search_term");
    let hits = store.search("ARTISAN");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].finished_at, 4, "newest run first");
    assert_eq!(hits[1].finished_at, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_search_filters_status_cwd_branch_here() {
    let (dir, store) = store_with_sample("search_filters");

    let failed = store.search("failed composer");
    assert_eq!(
        failed.iter().map(|e| e.command.as_str()).collect::<Vec<_>>(),
        vec!["composer update", "composer install"]
    );

    let ok = store.search("ok branch:main");
    assert_eq!(
        ok.iter().map(|e| e.command.as_str()).collect::<Vec<_>>(),
        vec!["php artisan migrate", "git status", "php artisan migrate"]
    );

    let project = store.search("project:palr");
    assert_eq!(project.len(), 3);
    assert!(project.iter().all(|e| e.cwd.as_deref() == Some("/p/palr")));

    let cwd = store.search("cwd:/p/spritx");
    assert_eq!(cwd.len(), 1);
    assert_eq!(cwd[0].command, "git status");

    let here = store.search_with_cwd("here", Some("/p/other/"));
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].command, "composer update");
    assert!(store.search_with_cwd("here", None).is_empty());

    assert_eq!(store.search("failed").len(), 2);
    assert_eq!(store.search("ok").len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_append_skips_space_prefix_blank_and_ignored() {
    let dir = temp_dir("append_skip");
    let path = dir.join("history.jsonl");
    let store = HistoryStore::new(&path)
        .with_ignore(config::default_history_ignore());

    store.append(HistoryEntry::new(" secret --token abc")).unwrap();
    store.append(HistoryEntry::new("   ")).unwrap();
    store.append(HistoryEntry::new("export GITHUB_TOKEN=abc")).unwrap();
    store.append(HistoryEntry::new("ls -la")).unwrap();

    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(body.lines().count(), 1);
    assert!(body.contains("ls -la"));
    assert!(!body.contains("TOKEN"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_ignores_corrupt_lines_and_clear_removes_file() {
    let dir = temp_dir("corrupt");
    let path = dir.join("history.jsonl");
    let store = HistoryStore::new(&path);
    let mut body = String::from("not json\n{\"command\":\"half");
    body.push_str("\n");
    body.push_str(&serde_json::to_string(&HistoryEntry::new("git status")).unwrap());
    body.push('\n');
    std::fs::write(&path, body).unwrap();

    let hits = store.search("git");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].command, "git status");

    store.clear().unwrap();
    assert!(!path.exists());
    store.clear().unwrap(); // idempotent
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_read_is_bounded_to_newest_window() {
    let dir = temp_dir("bounded");
    let path = dir.join("history.jsonl");
    let mut body = String::new();
    let total = MAX_READ_LINES + 25;
    for i in 0..total {
        let e = HistoryEntry::new(format!("cmd-{i}"));
        body.push_str(&serde_json::to_string(&e).unwrap());
        body.push('\n');
    }
    std::fs::write(&path, body).unwrap();

    let store = HistoryStore::new(&path);
    let all = store.all();
    assert_eq!(all.len(), MAX_READ_LINES);
    // The newest line survives; the oldest ones fell out of the window.
    assert_eq!(all[0].command, format!("cmd-{}", total - 1));
    assert!(all.iter().any(|e| e.command == "cmd-25"));
    assert!(!all.iter().any(|e| e.command == "cmd-24"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_round_trip_and_secret_stripping() {
    let dir = temp_dir("config");
    let path = dir.join("config.toml");

    let mut cfg = AppConfig::default();
    cfg.appearance.font_size = 17;
    cfg.remote.port = 6000;
    cfg.ai.openai_api_key = "sk-secret".into();
    config::save_to(&path, &cfg).unwrap();

    let loaded = config::load_from(&path);
    assert_eq!(loaded.appearance.font_size, 17);
    assert_eq!(loaded.remote.port, 6000);
    assert_eq!(loaded.ai.openai_api_key, "sk-secret");

    let exported = config::export_json(&loaded).unwrap();
    assert!(!exported.contains("sk-secret"));
    let imported = config::import_json(&exported).unwrap();
    assert_eq!(imported.appearance.font_size, 17);
    assert!(imported.ai.openai_api_key.is_empty());

    let from_toml = config::import_file(&path).unwrap();
    assert_eq!(from_toml.remote.port, 6000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_broken_file_is_backed_up_and_defaults_load() {
    let dir = temp_dir("config_broken");
    let path = dir.join("config.toml");
    std::fs::write(&path, "[[[ not toml").unwrap();
    let loaded = config::load_from(&path);
    assert_eq!(loaded.appearance.font_size, 14);
    assert!(dir.join("config.toml.broken").exists());
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn config_save_is_atomic_and_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("config_perms");
    let path = dir.join("sub").join("config.toml");
    config::save_to(&path, &AppConfig::default()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(!Path::new(&format!("{}.tmp", path.display())).exists());
    let _ = std::fs::remove_dir_all(&dir);
}
