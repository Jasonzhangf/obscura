#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
    dir: std::path::PathBuf,
}

impl Daemon {
    fn start() -> Self {
        let dir = std::path::PathBuf::from("/tmp").join(format!("owe-r2-{}", uuid::Uuid::new_v4()));
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir")
            .arg(&dir)
            .stdout(Stdio::null())
            .spawn()
            .expect("start Host");
        let mut daemon = Self { child, dir };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match UnixStream::connect(daemon.dir.join("host.sock")) {
                Ok(_) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => panic!("Host listener startup: {error}"),
            }
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "Host exited before listener ready"
            );
            assert!(Instant::now() < deadline, "Host startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }

    fn connect(&self) -> Client {
        let stream = UnixStream::connect(self.dir.join("host.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut client = Client {
            reader: BufReader::new(stream),
            next_id: 1,
        };
        assert_eq!(client.read()["type"], "ready");
        client
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    fn read(&mut self) -> Value {
        let mut line = String::new();
        assert!(self.reader.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    }

    fn raw(&mut self, request: Value) -> Value {
        writeln!(self.reader.get_mut(), "{request}").unwrap();
        self.read()
    }

    fn request(&mut self, command: Value) -> Value {
        let status = self.ok(json!({"type":"status"}));
        let id = self.next_id;
        self.next_id += 1;
        self.raw(json!({
            "id": id,
            "operation": identity(&status),
            "command": command
        }))
    }

    fn ok(&mut self, command: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mutation = matches!(
            command["type"].as_str(),
            Some("evaluate" | "navigate" | "click" | "input")
        );
        let operation = mutation.then(|| {
            let status = self.ok(json!({"type":"status"}));
            identity(&status)
        });
        let value = self.raw(json!({"id": id, "operation": operation, "command": command}));
        assert_eq!(value["type"], "result", "{value}");
        value["value"].clone()
    }

    fn eval(&mut self, expression: &str) -> Value {
        self.ok(json!({"type":"evaluate","expression":expression}))["result"]["value"].clone()
    }

    fn click(&mut self, x: f64, y: f64) -> Value {
        self.request(json!({"type":"click","x":x,"y":y}))
    }

    fn status(&mut self) -> Value {
        self.ok(json!({"type":"status"}))
    }
}

fn identity(status: &Value) -> Value {
    let sequence = status["next_sequence"].as_u64().unwrap();
    json!({
        "operation_id": format!("operation-{sequence}"),
        "session_id": status["session_id"],
        "attachment_id": status["attachment_id"],
        "sequence": sequence,
        "control_epoch": status["control"]["epoch"],
        "viewport_revision": status["viewport_revision"],
        "document_revision": status["document_revision"]
    })
}

fn frame(status: &Value, sequence: u64) -> Value {
    json!({
        "session_id": status["session_id"],
        "tab_id": status["active_tab_id"],
        "presentation_revision": status["presentation_revision"],
        "frame_sequence": sequence,
        "document_revision": status["document_revision"],
        "viewport_revision": status["viewport_revision"]
    })
}

fn editor_binding(context: &Value, edit_session_id: &str) -> Value {
    json!({
        "type": "editor",
        "edit_session_id": edit_session_id,
        "target_id": context["target_id"],
        "focus_revision": context["focus_revision"],
        "editor_revision": context["editor_revision"]
    })
}

fn input_request(
    client: &mut Client,
    event_sequence: u64,
    status: &Value,
    binding: Value,
    event: Value,
) -> Value {
    input_request_with_operation(
        client,
        event_sequence,
        status,
        identity(status),
        binding,
        event,
    )
}

fn input_request_with_operation(
    client: &mut Client,
    event_sequence: u64,
    status: &Value,
    operation: Value,
    binding: Value,
    event: Value,
) -> Value {
    let id = client.next_id;
    client.next_id += 1;
    client.raw(json!({
        "id": id,
        "operation": operation,
        "command": {
            "type": "input",
            "envelope": {
                "event_sequence": event_sequence,
                "frame": frame(status, event_sequence),
                "binding": binding,
                "event": event
            }
        }
    }))
}

fn fixture() -> String {
    "data:text/html,<input id=field style='position:absolute;left:10px;top:10px;width:300px;height:30px'><textarea id=area style='position:absolute;left:10px;top:60px;width:300px;height:60px'></textarea><div id=rich contenteditable=true style='position:absolute;left:10px;top:140px;width:300px;height:60px'>seed</div><script>window.events=[];function log(e){window.events.push({type:e.type,data:e.data||'',inputType:e.inputType||'',target:e.target&&e.target.id||''})}['field','area','rich'].forEach(function(id){var e=document.getElementById(id);['beforeinput','input','compositionstart','compositionupdate','compositionend'].forEach(function(t){e.addEventListener(t,function(ev){log(ev);if(t==='beforeinput'&&window.cancelNext){ev.preventDefault();window.cancelNext=false}})})})</script>".to_string()
}

fn target_event_types(client: &mut Client, target: &str) -> Vec<String> {
    client
        .eval(&format!(
            "window.events.filter(function(e){{return e.target==='{target}';}}).map(function(e){{return e.type;}})"
        ))
        .as_array()
        .expect("event types")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn public_page_mutation_rejects_stale_revision_and_new_context_recovers() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);

    let initial = agent.status();
    assert_eq!(initial["editor_context"]["editor_revision"], 0, "{initial}");
    let stale = editor_binding(&initial["editor_context"], "stale-page-mutation");

    agent.eval(
        "(function(){var f=document.getElementById('field');f.value='changed';f.setSelectionRange(0,7);return true;})()",
    );
    let after_mutation = agent.status();
    let rejected = input_request(
        &mut agent,
        0,
        &after_mutation,
        stale,
        json!({"type":"replace_text","text":"blocked","input_type":"insertText"}),
    );
    assert_eq!(rejected["code"], "STALE_EDITOR_REVISION", "{rejected}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "changed");

    agent.click(20.0, 20.0);
    let recovered = agent.status();
    assert_ne!(
        recovered["editor_context"]["editor_revision"],
        initial["editor_context"]["editor_revision"],
        "{recovered}"
    );
    let applied = input_request(
        &mut agent,
        0,
        &recovered,
        editor_binding(&recovered["editor_context"], "recovered-page-mutation"),
        json!({"type":"replace_text","text":"fresh","input_type":"insertText"}),
    );
    assert_eq!(applied["value"]["editing"]["state"], "applied", "{applied}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "fresh");
}

#[test]
fn public_composition_events_order_commit_cancel_and_prevented_input() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);

    let status = agent.status();
    let context = &status["editor_context"];
    assert_eq!(context["editor_revision"], 0, "{status}");
    let begin = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "compose-field"),
        json!({"type":"composition_begin","data":"中"}),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let update = input_request(
        &mut agent,
        1,
        &status,
        editor_binding(context, "compose-field"),
        json!({"type":"composition_update","data":"中文"}),
    );
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    let commit = input_request(
        &mut agent,
        2,
        &status,
        editor_binding(context, "compose-field"),
        json!({"type":"composition_commit","data":"中文"}),
    );
    assert_eq!(commit["value"]["editing"]["state"], "applied", "{commit}");
    assert_eq!(commit["value"]["editing"]["editor_revision"], 1, "{commit}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "中文");
    assert_eq!(
        target_event_types(&mut agent, "field"),
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

    let committed = agent.status();
    assert_eq!(
        committed["editor_context"]["editor_revision"], 1,
        "{committed}"
    );
    agent.eval("window.cancelNext=true;true");
    let cancel_ready = agent.status();
    let prevented = input_request(
        &mut agent,
        0,
        &cancel_ready,
        editor_binding(&cancel_ready["editor_context"], "prevent-field-edit"),
        json!({"type":"replace_text","text":"blocked","input_type":"insertText"}),
    );
    assert_eq!(prevented["value"]["editing"]["state"], "cancelled", "{prevented}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "中文");
    assert_eq!(
        agent.status()["editor_context"]["editor_revision"],
        1,
        "a prevented edit must not bump the revision"
    );

    agent.eval(
        "document.getElementById('area').value='base';document.getElementById('area').setSelectionRange(4,4);true",
    );
    agent.click(20.0, 70.0);
    let area_status = agent.status();
    let area_context = &area_status["editor_context"];
    assert_eq!(area_context["kind"], "textarea", "{area_status}");
    let begin = input_request(
        &mut agent,
        0,
        &area_status,
        editor_binding(area_context, "cancel-area"),
        json!({"type":"composition_begin","data":"x"}),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let update = input_request(
        &mut agent,
        1,
        &area_status,
        editor_binding(area_context, "cancel-area"),
        json!({"type":"composition_update","data":"xy"}),
    );
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    let cancel = input_request(
        &mut agent,
        2,
        &area_status,
        editor_binding(area_context, "cancel-area"),
        json!({"type":"composition_cancel"}),
    );
    assert_eq!(cancel["value"]["editing"]["state"], "cancelled", "{cancel}");
    assert_eq!(agent.eval("document.getElementById('area').value"), "base");
    let cancelled = agent.status();
    assert_eq!(
        cancelled["editor_context"]["editor_revision"], 1,
        "{cancelled}"
    );
    assert!(cancelled["editor_context"]["composition"].is_null(), "{cancelled}");
    assert!(cancelled["input_lease"].is_null(), "{cancelled}");
    assert_eq!(
        target_event_types(&mut agent, "area"),
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
fn public_editor_rejects_stale_target_and_epoch() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);
    let field_status = agent.status();
    let field_binding = editor_binding(&field_status["editor_context"], "field-target");

    agent.click(20.0, 70.0);
    let area_status = agent.status();
    assert_eq!(area_status["editor_context"]["kind"], "textarea", "{area_status}");
    let stale_target = input_request(
        &mut agent,
        0,
        &area_status,
        field_binding,
        json!({"type":"replace_text","text":"blocked","input_type":"insertText"}),
    );
    assert_eq!(stale_target["code"], "STALE_EDITOR_TARGET", "{stale_target}");
    assert_eq!(agent.eval("document.getElementById('area').value"), "");

    // The rejected stale-target attempt consumed and rolled back its operation
    // sequence, so a fresh status yields the next valid operation identity.
    // The epoch case must use its own operation id: reusing the prior id with a
    // different epoch fence is an OPERATION_CONFLICT, not a control-epoch test.
    let epoch_status = agent.status();
    let mut stale_epoch = identity(&epoch_status);
    stale_epoch["operation_id"] = json!("stale-epoch-op");
    stale_epoch["control_epoch"] =
        json!(stale_epoch["control_epoch"].as_u64().unwrap().saturating_sub(1));
    let stale_epoch_result = input_request_with_operation(
        &mut agent,
        0,
        &epoch_status,
        stale_epoch,
        editor_binding(&epoch_status["editor_context"], "stale-epoch"),
        json!({"type":"replace_text","text":"blocked","input_type":"insertText"}),
    );
    assert_eq!(stale_epoch_result["code"], "STALE_CONTROL", "{stale_epoch_result}");
    assert_eq!(agent.eval("document.getElementById('area').value"), "");
}
