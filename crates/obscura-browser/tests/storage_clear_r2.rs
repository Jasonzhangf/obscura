//! Public-boundary tests for `SharedLocalStorage::flush`.
//!
//! These assert the externally visible persistence side effects: an
//! authoritative snapshot is written, a cleared origin's snapshot file is
//! removed so a cold reload cannot resurrect it, unrelated files and the root
//! are preserved, and a failed write/removal surfaces the typed
//! `PROFILE_STORAGE_FAILED` error without a false success.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use obscura_browser::context::SharedLocalStorage;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Two valid hex origin keys, matching the on-disk snapshot naming rule.
const ORIGIN_A: &str = "aa";
const ORIGIN_B: &str = "bb";

fn unique_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "obscura-storage-clear-r2-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn origin_json(store: &SharedLocalStorage, origin: &str) -> serde_json::Value {
    serde_json::from_str(&store.load_origin(origin)).expect("origin snapshot is JSON")
}

#[test]
fn clear_then_flush_removes_snapshot_and_cold_reload_has_no_resurrection() {
    let dir = unique_dir("clear");
    let file = dir.join(format!("{ORIGIN_A}.json"));

    let store = SharedLocalStorage::load(Some(dir.clone())).expect("load store");
    store.set(ORIGIN_A, "k", "v");
    store.flush().expect("flush set");
    assert!(file.exists(), "authoritative snapshot written");
    assert_eq!(origin_json(&store, ORIGIN_A), serde_json::json!({ "k": "v" }));

    store.clear(ORIGIN_A);
    store.flush().expect("flush clear");
    assert!(!file.exists(), "obsolete origin snapshot removed after clear");

    // Cold reload of the same directory must not resurrect the cleared value.
    let reloaded = SharedLocalStorage::load(Some(dir.clone())).expect("cold reload");
    assert_eq!(origin_json(&reloaded, ORIGIN_A), serde_json::json!({}));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flush_preserves_unrelated_files_and_the_root() {
    let dir = unique_dir("preserve");
    let store = SharedLocalStorage::load(Some(dir.clone())).expect("load store");

    let unrelated = dir.join("notes.txt");
    let non_origin_json = dir.join("cookies.json");
    fs::write(&unrelated, b"keep me").expect("write unrelated file");
    fs::write(&non_origin_json, b"{}").expect("write non-origin json");

    store.set(ORIGIN_A, "k", "v");
    store.flush().expect("flush");

    assert!(dir.exists(), "storage root preserved");
    assert_eq!(fs::read(&unrelated).expect("unrelated readable"), b"keep me");
    assert_eq!(fs::read(&non_origin_json).expect("json readable"), b"{}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn failed_write_returns_profile_storage_failed_and_keeps_state_recoverable() {
    let dir = unique_dir("write-fail");
    let store = SharedLocalStorage::load(Some(dir.clone())).expect("load store");

    // A directory squatting on the authoritative path makes the atomic rename
    // fail; flush must surface the typed error instead of a false success.
    fs::create_dir(dir.join(format!("{ORIGIN_A}.json"))).expect("squat snapshot path");
    store.set(ORIGIN_A, "k", "v");

    let error = store.flush().expect_err("flush must fail");
    assert_eq!(error.code, "PROFILE_STORAGE_FAILED");
    // The in-memory owner still holds the value, so the failure is recoverable.
    assert_eq!(origin_json(&store, ORIGIN_A), serde_json::json!({ "k": "v" }));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn failed_removal_returns_profile_storage_failed_and_keeps_obstacle() {
    let dir = unique_dir("remove-fail");
    let store = SharedLocalStorage::load(Some(dir.clone())).expect("load store");

    // An owned-name snapshot that is actually a directory cannot be removed;
    // flush must stop and return the typed error, not a successful flush.
    let obstacle = dir.join(format!("{ORIGIN_B}.json"));
    fs::create_dir(&obstacle).expect("squat obsolete snapshot");

    let error = store.flush().expect_err("flush must fail");
    assert_eq!(error.code, "PROFILE_STORAGE_FAILED");
    assert!(obstacle.exists(), "failed removal left the obstacle in place");

    let _ = fs::remove_dir_all(&dir);
}
