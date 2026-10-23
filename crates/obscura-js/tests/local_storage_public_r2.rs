//! Public-boundary localStorage tests for the C1 typed-presence repair.
//!
//! These drive the public `ObscuraJsRuntime` surface (`set_local_storage`,
//! `run_page_init`, `evaluate`) and assert the externally visible storage
//! behavior plus the backend side effects, never private runtime state.
//!
//! The absent-backend F7 case (`ObscuraJsRuntime::new()` with no backend keeps
//! a standalone in-memory store that is distinct from a valid empty backend)
//! also needs the C3-owned `bootstrap.js` load consumer, which treats a `null`
//! load result as "keep the mirror". That half is reported separately as
//! `COMBINED_E2E_WAIT` and is intentionally not asserted green here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use obscura_js::ops::LocalStorageBackend;
use obscura_js::runtime::ObscuraJsRuntime;

const ORIGIN: &str = "https://example.invalid";

/// Shared in-memory backend that records every origin -> key -> value mutation
/// so a test can confirm the public storage API wrote through to the owner.
#[derive(Default)]
struct MemBackend {
    origins: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl MemBackend {
    fn snapshot(&self, origin: &str) -> HashMap<String, String> {
        self.origins
            .lock()
            .expect("backend lock")
            .get(origin)
            .cloned()
            .unwrap_or_default()
    }
}

impl LocalStorageBackend for MemBackend {
    fn load(&self, origin: &str) -> String {
        let origins = self.origins.lock().expect("backend lock");
        serde_json::to_string(&origins.get(origin).unwrap_or(&HashMap::new()))
            .unwrap_or_else(|_| "{}".to_string())
    }

    fn set(&self, origin: &str, key: &str, value: &str) {
        self.origins
            .lock()
            .expect("backend lock")
            .entry(origin.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }

    fn remove(&self, origin: &str, key: &str) {
        if let Some(map) = self.origins.lock().expect("backend lock").get_mut(origin) {
            map.remove(key);
        }
    }

    fn clear(&self, origin: &str) {
        self.origins.lock().expect("backend lock").remove(origin);
    }
}

fn runtime_with_backend(backend: Arc<MemBackend>) -> ObscuraJsRuntime {
    let mut runtime = ObscuraJsRuntime::new();
    runtime.set_local_storage(ORIGIN, backend as Arc<dyn LocalStorageBackend>);
    runtime.run_page_init();
    runtime
}

#[test]
fn valid_empty_backend_round_trips_set_get_remove_clear() {
    let backend = Arc::new(MemBackend::default());
    let mut runtime = runtime_with_backend(backend.clone());

    let set = runtime
        .evaluate(
            "(() => { localStorage.clear(); localStorage.setItem('a','1'); return { get: localStorage.getItem('a'), len: localStorage.length }; })()",
        )
        .expect("set/get evaluate");
    assert_eq!(set, serde_json::json!({ "get": "1", "len": 1 }));
    assert_eq!(backend.snapshot(ORIGIN).get("a").map(String::as_str), Some("1"));

    let removed = runtime
        .evaluate(
            "(() => { localStorage.removeItem('a'); return { get: localStorage.getItem('a'), len: localStorage.length }; })()",
        )
        .expect("remove evaluate");
    assert_eq!(removed, serde_json::json!({ "get": null, "len": 0 }));
    assert!(backend.snapshot(ORIGIN).is_empty());

    let cleared = runtime
        .evaluate(
            "(() => { localStorage.setItem('a','1'); localStorage.setItem('b','2'); localStorage.clear(); return { get: localStorage.getItem('a'), len: localStorage.length }; })()",
        )
        .expect("clear evaluate");
    assert_eq!(cleared, serde_json::json!({ "get": null, "len": 0 }));
    assert!(backend.snapshot(ORIGIN).is_empty());
}

#[test]
fn populated_backend_hydrates_and_public_mutations_write_through() {
    let backend = Arc::new(MemBackend::default());
    backend.set(ORIGIN, "seed", "42");
    let mut runtime = runtime_with_backend(backend.clone());

    let hydrated = runtime
        .evaluate("(() => ({ seed: localStorage.getItem('seed'), len: localStorage.length }))()")
        .expect("hydrate evaluate");
    assert_eq!(hydrated, serde_json::json!({ "seed": "42", "len": 1 }));

    let removed = runtime
        .evaluate(
            "(() => { localStorage.removeItem('seed'); return localStorage.getItem('seed'); })()",
        )
        .expect("remove evaluate");
    assert_eq!(removed, serde_json::Value::Null);
    assert!(!backend.snapshot(ORIGIN).contains_key("seed"));
}
