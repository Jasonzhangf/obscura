// C3 public runtime editor consumer (F3 revision invalidation, F4
// beforeinput/composition semantics, and the bootstrap storage mirror that
// pairs with C1's typed local-storage presence).
//
// Everything here drives the public `ObscuraJsRuntime` surface: `set_dom`,
// `set_url`, `run_page_init`, and `evaluate`. No private field, source
// assertion, or internal call count is used; page listeners observe the real
// events and the assertions read the page-visible value/selection/revision.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use obscura_dom::parse_html;
use obscura_js::ops::LocalStorageBackend;
use obscura_js::runtime::ObscuraJsRuntime;
use serde_json::{json, Value};

fn runtime(html: &str) -> ObscuraJsRuntime {
    let mut rt = ObscuraJsRuntime::new();
    rt.set_dom(parse_html(html));
    rt.set_url("http://example.test/editor");
    rt.run_page_init();
    rt
}

fn wire(rt: &mut ObscuraJsRuntime, id: &str) {
    let script = format!(
        r#"(function(){{
window.events = [];
window.cancelNext = false;
var el = document.getElementById('{id}');
['beforeinput','input','compositionstart','compositionupdate','compositionend'].forEach(function(name){{
  el.addEventListener(name, function(ev){{
    window.events.push({{type:ev.type,inputType:ev.inputType||'',target:ev.target&&ev.target.id||''}});
    if (name==='beforeinput' && window.cancelNext) {{ ev.preventDefault(); window.cancelNext = false; }}
  }});
}});
}})()"#
    );
    rt.evaluate(&script).expect("wire listeners");
}

fn focus_and_context(rt: &mut ObscuraJsRuntime, id: &str) -> Value {
    let expr = format!(
        "(function(){{document.getElementById('{id}').focus();return globalThis.__obscura_editor_context();}})()"
    );
    rt.evaluate(&expr).expect("focus evaluates")
}

fn apply(rt: &mut ObscuraJsRuntime, event: Value) -> Value {
    let expr = format!(
        "globalThis.__obscura_editor_apply({})",
        serde_json::to_string(&event).unwrap()
    );
    rt.evaluate(&expr).expect("editor apply evaluates")
}

fn event_types(rt: &mut ObscuraJsRuntime) -> Vec<String> {
    rt.evaluate("window.events.map(function(e){return e.type;})")
        .expect("read events")
        .as_array()
        .expect("events array")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect()
}

const FIELD_PAGE: &str = r#"<!doctype html><html><body>
<input id="field" style="position:absolute;left:10px;top:10px;width:300px;height:30px">
<textarea id="area" style="position:absolute;left:10px;top:60px;width:300px;height:60px"></textarea>
<div id="rich" contenteditable="true" style="position:absolute;left:10px;top:140px;width:300px;height:60px">seed</div>
</body></html>"#;

#[test]
fn composition_begin_dispatches_cancelable_beforeinput_before_input_and_composition() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    focus_and_context(&mut rt, "field");

    let receipt = apply(&mut rt, json!({"type": "composition_begin", "data": "中"}));
    assert_eq!(receipt["state"], json!("pending"), "{receipt}");
    assert_eq!(receipt["composition"], json!({"data": "中", "start": 0, "end": 1}), "{receipt}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中")
    );
    // Real order: the cancelable beforeinput is dispatched before the value
    // mutation, then the composition event, then input.
    assert_eq!(
        event_types(&mut rt),
        vec!["beforeinput", "compositionstart", "input"]
    );
    assert_eq!(
        rt.evaluate("window.events[0].inputType").unwrap(),
        json!("insertCompositionText")
    );
}

#[test]
fn prevented_beforeinput_does_not_mutate_or_commit() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    focus_and_context(&mut rt, "field");
    rt.evaluate("window.cancelNext = true").unwrap();

    let receipt = apply(&mut rt, json!({"type": "composition_begin", "data": "中"}));
    assert_eq!(receipt["state"], json!("cancelled"), "{receipt}");
    assert_eq!(receipt["composition"], json!(null), "{receipt}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("")
    );
    assert_eq!(event_types(&mut rt), vec!["beforeinput"]);

    // A rejected begin must not leave a lease behind: a later begin succeeds.
    let second = apply(&mut rt, json!({"type": "composition_begin", "data": "文"}));
    assert_eq!(second["state"], json!("pending"), "{second}");
}

#[test]
fn prevented_update_does_not_mutate_or_commit_and_commit_can_still_apply() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    focus_and_context(&mut rt, "field");

    let begin = apply(&mut rt, json!({"type": "composition_begin", "data": "中"}));
    assert_eq!(begin["state"], json!("pending"), "{begin}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中")
    );

    rt.evaluate("window.cancelNext = true").unwrap();
    let blocked = apply(&mut rt, json!({"type": "composition_update", "data": "中文"}));
    assert_eq!(blocked["state"], json!("cancelled"), "{blocked}");
    assert_eq!(
        blocked["composition"],
        json!({"data": "中", "start": 0, "end": 1}),
        "{blocked}"
    );
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中")
    );
    assert_eq!(
        event_types(&mut rt),
        vec![
            "beforeinput",
            "compositionstart",
            "input",
            "beforeinput",
        ]
    );

    let committed = apply(&mut rt, json!({"type": "composition_commit", "data": "中文"}));
    assert_eq!(committed["state"], json!("applied"), "{committed}");
    assert_eq!(committed["composition"], json!(null), "{committed}");
    assert_eq!(committed["editor_revision"], json!(1), "{committed}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中文")
    );
}

#[test]
fn prevented_commit_ends_without_mutation_or_revision_bump() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    focus_and_context(&mut rt, "field");

    let begin = apply(&mut rt, json!({"type": "composition_begin", "data": "中"}));
    assert_eq!(begin["state"], json!("pending"), "{begin}");
    rt.evaluate("window.cancelNext = true").unwrap();
    let blocked = apply(&mut rt, json!({"type": "composition_commit", "data": "中文"}));
    assert_eq!(blocked["state"], json!("cancelled"), "{blocked}");
    assert_eq!(blocked["composition"], json!(null), "{blocked}");
    assert_eq!(blocked["editor_revision"], json!(0), "{blocked}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中")
    );
    assert_eq!(event_types(&mut rt).last().map(String::as_str), Some("compositionend"));
}

#[test]
fn composition_update_commit_order_and_single_revision_bump() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    let focused = focus_and_context(&mut rt, "field");
    assert_eq!(focused["editor_revision"], json!(0), "{focused}");

    apply(&mut rt, json!({"type": "composition_begin", "data": "中"}));
    let updated = apply(&mut rt, json!({"type": "composition_update", "data": "中文"}));
    assert_eq!(updated["state"], json!("pending"), "{updated}");
    assert_eq!(
        updated["context"]["surrounding"]["text"],
        json!("中文"),
        "{updated}"
    );

    let committed = apply(&mut rt, json!({"type": "composition_commit", "data": "中文"}));
    assert_eq!(committed["state"], json!("applied"), "{committed}");
    assert_eq!(committed["composition"], json!(null), "{committed}");
    assert_eq!(committed["editor_revision"], json!(1), "{committed}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("中文")
    );
    assert_eq!(
        event_types(&mut rt),
        vec![
            "beforeinput",
            "compositionstart",
            "input",
            "beforeinput",
            "compositionupdate",
            "input",
            "beforeinput",
            "compositionend",
            "input",
        ]
    );
    // Commit bumps exactly once.
    let after = focus_and_context(&mut rt, "field");
    assert_eq!(after["editor_revision"], json!(1), "{after}");
}

#[test]
fn composition_cancel_restores_snapshot_and_bumps_once() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    rt.evaluate(
        "(function(){var f=document.getElementById('field');f.value='seed';f.setSelectionRange(4,4);f.focus();return true;})()",
    )
    .unwrap();
    let base = focus_and_context(&mut rt, "field");
    let begin = apply(&mut rt, json!({"type": "composition_begin", "data": "x"}));
    assert_eq!(begin["state"], json!("pending"), "{begin}");
    apply(&mut rt, json!({"type": "composition_update", "data": "xy"}));
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("seedxy")
    );
    let cancelled = apply(&mut rt, json!({"type": "composition_cancel"}));
    assert_eq!(cancelled["state"], json!("cancelled"), "{cancelled}");
    assert_eq!(cancelled["composition"], json!(null), "{cancelled}");
    assert_eq!(
        rt.evaluate("document.getElementById('field').value").unwrap(),
        json!("seed")
    );
    assert_eq!(
        cancelled["editor_revision"].as_u64().unwrap(),
        base["editor_revision"].as_u64().unwrap() + 1,
        "{cancelled}"
    );
    assert_eq!(
        event_types(&mut rt),
        vec![
            "beforeinput",
            "compositionstart",
            "input",
            "beforeinput",
            "compositionupdate",
            "input",
            "beforeinput",
            "compositionend",
        ]
    );
}

#[test]
fn page_value_mutation_advances_revision_and_stales_an_existing_binding() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "field");
    let before = focus_and_context(&mut rt, "field");
    assert_eq!(before["editor_revision"], json!(0), "{before}");

    // Ordinary page-script mutation, the F3 red case.
    rt.evaluate(
        "(function(){document.getElementById('field').value='changed';return true;})()",
    )
    .unwrap();
    let after = focus_and_context(&mut rt, "field");
    assert_ne!(before["editor_revision"], after["editor_revision"], "{after}");
    assert_eq!(after["editor_revision"], json!(1), "{after}");

    // A binding captured before the mutation is stale: the Engine no longer
    // publishes the revision that binding was admitted with.
    assert_eq!(before["editor_revision"], json!(0));
    assert_eq!(after["editor_revision"], json!(1));
}

#[test]
fn page_selection_mutation_advances_revision() {
    let mut rt = runtime(FIELD_PAGE);
    rt.evaluate(
        "(function(){var f=document.getElementById('field');f.value='abcdef';f.focus();f.setSelectionRange(2,2);return true;})()",
    )
    .unwrap();
    let base = focus_and_context(&mut rt, "field");
    let current = base["editor_revision"].as_u64().unwrap();
    rt.evaluate("document.getElementById('field').setSelectionRange(4,4)").unwrap();
    let after = focus_and_context(&mut rt, "field");
    assert_eq!(
        after["editor_revision"].as_u64().unwrap(),
        current + 1,
        "{after}"
    );
}

#[test]
fn content_editable_page_mutation_advances_revision() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "rich");
    let before = focus_and_context(&mut rt, "rich");
    assert_eq!(before["editor_revision"], json!(0), "{before}");

    rt.evaluate("document.getElementById('rich').textContent = 'changed'")
        .unwrap();
    let after = focus_and_context(&mut rt, "rich");
    assert_eq!(after["editor_revision"], json!(1), "{after}");
    assert_eq!(
        rt.evaluate("document.getElementById('rich').textContent").unwrap(),
        json!("changed")
    );
}

#[test]
fn content_editable_text_node_mutation_advances_revision() {
    let mut rt = runtime(FIELD_PAGE);
    wire(&mut rt, "rich");
    let before = focus_and_context(&mut rt, "rich");
    assert_eq!(before["editor_revision"], json!(0), "{before}");

    rt.evaluate("document.getElementById('rich').firstChild.data = 'changed'")
        .unwrap();
    let after = focus_and_context(&mut rt, "rich");
    assert_eq!(after["editor_revision"], json!(1), "{after}");
    assert_eq!(
        rt.evaluate("document.getElementById('rich').textContent").unwrap(),
        json!("changed")
    );
}

// --- storage mirror consumer (pairs with C1's typed local-storage presence) --

#[derive(Default)]
struct EmptyBackend;

impl LocalStorageBackend for EmptyBackend {
    fn load(&self, _origin: &str) -> String {
        "{}".to_string()
    }
    fn set(&self, _origin: &str, _key: &str, _value: &str) {}
    fn remove(&self, _origin: &str, _key: &str) {}
    fn clear(&self, _origin: &str) {}
}

#[derive(Default)]
struct PresenceBackend {
    origins: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl LocalStorageBackend for PresenceBackend {
    fn load(&self, origin: &str) -> String {
        let guard = self.origins.lock().unwrap();
        serde_json::to_string(guard.get(origin).unwrap_or(&HashMap::new())).unwrap()
    }
    fn set(&self, origin: &str, key: &str, value: &str) {
        self.origins
            .lock()
            .unwrap()
            .entry(origin.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }
    fn remove(&self, origin: &str, key: &str) {
        if let Some(map) = self.origins.lock().unwrap().get_mut(origin) {
            map.remove(key);
        }
    }
    fn clear(&self, origin: &str) {
        self.origins.lock().unwrap().remove(origin);
    }
}

#[test]
fn absent_backend_keeps_a_standalone_in_memory_mirror() {
    let mut rt = runtime("<html><body></body></html>");
    let result = rt
        .evaluate(
            "(function(){localStorage.clear();localStorage.setItem('k','v');\
             return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length});})()",
        )
        .unwrap();
    assert_eq!(result, json!(r#"{"get":"v","len":1}"#), "{result}");
}

#[test]
fn valid_empty_backend_keeps_a_live_store_and_does_not_accumulate_standalone_values() {
    let mut rt = ObscuraJsRuntime::new();
    rt.set_dom(parse_html("<html><body></body></html>"));
    rt.set_url("http://example.test/editor");
    rt.set_local_storage(
        "http://example.test",
        Arc::new(EmptyBackend) as Arc<dyn LocalStorageBackend>,
    );
    rt.run_page_init();
    let result = rt
        .evaluate(
            "(function(){localStorage.clear();localStorage.setItem('k','v');\
             return JSON.stringify({get:localStorage.getItem('k'),len:localStorage.length});})()",
        )
        .unwrap();
    assert_eq!(result, json!(r#"{"get":null,"len":0}"#), "{result}");
}

#[test]
fn configured_backend_refreshes_the_mirror_from_the_snapshot() {
    let mut rt = ObscuraJsRuntime::new();
    rt.set_dom(parse_html("<html><body></body></html>"));
    rt.set_url("http://example.test/editor");
    let backend = Arc::new(PresenceBackend::default());
    backend.set("http://example.test", "seed", "s");
    rt.set_local_storage("http://example.test", backend as Arc<dyn LocalStorageBackend>);
    rt.run_page_init();
    let result = rt
        .evaluate("JSON.stringify({get:localStorage.getItem('seed'),len:localStorage.length})")
        .unwrap();
    assert_eq!(result, json!(r#"{"get":"s","len":1}"#), "{result}");
}
