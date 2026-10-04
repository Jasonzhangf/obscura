use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use obscura_js::ops::LocalStorageBackend;
use obscura_js::runtime::ObscuraJsRuntime;

#[derive(Default)]
struct MemBackend {
    origins: Mutex<HashMap<String, HashMap<String, String>>>,
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
        if let Some(origin_map) = self.origins.lock().expect("backend lock").get_mut(origin) {
            origin_map.remove(key);
        }
    }

    fn clear(&self, origin: &str) {
        self.origins.lock().expect("backend lock").remove(origin);
    }
}

fn assert_eq_json(
    label: &str,
    actual: serde_json::Value,
    expected: serde_json::Value,
) -> Result<(), String> {
    if actual != expected {
        return Err(format!("{label}: expected {expected}, got {actual}"));
    }
    println!("{label}: ok {actual}");
    Ok(())
}

fn require_json_result(
    label: &str,
    actual: &serde_json::Value,
    expected: serde_json::Value,
    failures: &mut Vec<String>,
) {
    let Some(encoded) = actual.as_str() else {
        failures.push(format!("{label}: expected JSON string, got {actual}"));
        return;
    };
    let value = match serde_json::from_str(encoded) {
        Ok(value) => value,
        Err(error) => {
            failures.push(format!(
                "{label}: malformed JSON string {encoded:?}: {error}"
            ));
            return;
        }
    };
    if let Err(error) = assert_eq_json(label, value, expected) {
        failures.push(error);
    }
}

fn run_absent(mut runtime: ObscuraJsRuntime) -> Vec<String> {
    let mut failures = Vec::new();
    runtime.run_page_init();
    let step1 = match runtime
        .evaluate(
            "(() => { localStorage.clear(); localStorage.setItem('k','v'); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("absent set/get failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "absent set/get",
        &step1,
        serde_json::json!({"get":"v","len":1}),
        &mut failures,
    );

    let step2 = match runtime
        .evaluate(
            "(() => { localStorage.removeItem('k'); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("absent remove failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "absent remove",
        &step2,
        serde_json::json!({"get":null,"len":0}),
        &mut failures,
    );

    let step3 = match runtime
        .evaluate(
            "(() => { localStorage.setItem('k','v'); localStorage.clear(); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("absent clear failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "absent clear",
        &step3,
        serde_json::json!({"get":null,"len":0}),
        &mut failures,
    );
    failures
}

fn run_valid_empty_backend(mut runtime: ObscuraJsRuntime) -> Vec<String> {
    let mut failures = Vec::new();
    let backend = Arc::new(MemBackend::default());
    runtime.set_local_storage(
        "https://example.invalid",
        backend as Arc<dyn LocalStorageBackend>,
    );
    runtime.run_page_init();
    let set_get = match runtime
        .evaluate(
            "(() => { localStorage.clear(); localStorage.setItem('k','v'); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("valid-empty set/get failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "valid-empty set/get",
        &set_get,
        serde_json::json!({"get":"v","len":1}),
        &mut failures,
    );

    let remove = match runtime
        .evaluate(
            "(() => { localStorage.removeItem('k'); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("valid-empty remove failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "valid-empty remove",
        &remove,
        serde_json::json!({"get":null,"len":0}),
        &mut failures,
    );

    let clear = match runtime
        .evaluate(
            "(() => { localStorage.setItem('k','v'); localStorage.clear(); return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length}); })()",
        )
    {
        Ok(value) => value,
        Err(e) => {
            failures.push(format!("valid-empty clear failed: {e}"));
            serde_json::Value::Null
        }
    };
    require_json_result(
        "valid-empty clear",
        &clear,
        serde_json::json!({"get":null,"len":0}),
        &mut failures,
    );
    failures
}

fn main() {
    let absent = run_absent(ObscuraJsRuntime::new());
    if absent.is_empty() {
        println!("ABSENT_PHASE=PASS");
    } else {
        println!("ABSENT_PHASE=FAIL");
        for failure in &absent {
            println!("  {failure}");
        }
    }
    let valid = run_valid_empty_backend(ObscuraJsRuntime::new());
    if valid.is_empty() {
        println!("VALID_EMPTY_PHASE=PASS");
    } else {
        println!("VALID_EMPTY_PHASE=FAIL");
        for failure in &valid {
            println!("  {failure}");
        }
    }
    if absent.is_empty() && valid.is_empty() {
        println!("SUMMARY=PASS");
        std::process::exit(0);
    }
    println!("SUMMARY=FAIL");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::require_json_result;
    use serde_json::json;

    fn decode_failures(actual: serde_json::Value) -> Vec<String> {
        let mut failures = Vec::new();
        require_json_result(
            "decoder-negative",
            &actual,
            json!({"get":"v","len":1}),
            &mut failures,
        );
        failures
    }

    #[test]
    fn null_result_is_a_labelled_failure() {
        let failures = decode_failures(serde_json::Value::Null);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("decoder-negative"));
        assert!(failures[0].contains("expected JSON string"));
    }

    #[test]
    fn non_string_result_is_a_labelled_failure() {
        let failures = decode_failures(json!(["not", "a", "string"]));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("decoder-negative"));
        assert!(failures[0].contains("expected JSON string"));
    }

    #[test]
    fn malformed_json_result_is_a_labelled_failure() {
        let failures = decode_failures(json!("{\"get\":"));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("decoder-negative"));
        assert!(failures[0].contains("malformed JSON string"));
    }
}
