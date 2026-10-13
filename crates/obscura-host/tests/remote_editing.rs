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
        let dir = std::path::PathBuf::from("/tmp").join(format!("owe-{}", uuid::Uuid::new_v4()));
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
            Some(
                "evaluate"
                    | "navigate"
                    | "resize"
                    | "close_session"
                    | "click"
                    | "input_text"
                    | "scroll"
                    | "input"
            )
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

fn input_request_with_frame(
    client: &mut Client,
    event_sequence: u64,
    status: &Value,
    frame_sequence: u64,
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
                "frame": frame(status, frame_sequence),
                "binding": binding,
                "event": event
            }
        }
    }))
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

fn editing_event(event: Value) -> Value {
    event
}

fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration, message: &str) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture() -> String {
    "data:text/html,<input id=field style='position:absolute;left:10px;top:10px;width:300px;height:30px'><textarea id=area style='position:absolute;left:10px;top:60px;width:300px;height:60px'></textarea><div id=rich contenteditable=true style='position:absolute;left:10px;top:140px;width:300px;height:60px'>seed</div><script>window.events=[];function log(e){window.events.push({type:e.type,data:e.data||'',inputType:e.inputType||'',target:e.target&&e.target.id||''})}['field','area','rich'].forEach(function(id){var e=document.getElementById(id);['beforeinput','input','compositionstart','compositionupdate','compositionend'].forEach(function(t){e.addEventListener(t,function(ev){log(ev);if(t==='beforeinput'&&window.cancelNext){ev.preventDefault();window.cancelNext=false}})})})</script>".to_string()
}

#[test]
fn public_editor_consumer_rejects_stale_epoch_target_and_keeps_frame_sequence_local() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);
    let status = agent.status();
    let context = &status["editor_context"];

    let event = editing_event(json!({
        "type":"replace_text",
        "text":"frame-local",
        "input_type":"insertText"
    }));
    let local_operation = identity(&status);
    let local = input_request_with_frame(
        &mut agent,
        0,
        &status,
        999,
        local_operation,
        editor_binding(context, "frame-local"),
        event,
    );
    assert_eq!(local["value"]["editing"]["state"], "applied", "{local}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "frame-local");

    let status = agent.status();
    let context = &status["editor_context"];
    let mut stale_epoch = identity(&status);
    stale_epoch["control_epoch"] = json!(stale_epoch["control_epoch"].as_u64().unwrap() - 1);
    let rejected = input_request_with_operation(
        &mut agent,
        0,
        &status,
        stale_epoch,
        editor_binding(context, "stale-epoch"),
        editing_event(json!({"type":"replace_text","text":"blocked","input_type":"insertText"})),
    );
    assert_eq!(rejected["code"], "STALE_CONTROL", "{rejected}");

    let status = agent.status();
    let wrong_target = editor_binding(
        &json!({
            "target_id":"node-does-not-exist",
            "focus_revision":status["editor_context"]["focus_revision"],
            "editor_revision":status["editor_context"]["editor_revision"]
        }),
        "stale-target",
    );
    let rejected = input_request(
        &mut agent,
        0,
        &status,
        wrong_target,
        editing_event(json!({"type":"replace_text","text":"blocked","input_type":"insertText"})),
    );
    assert_eq!(rejected["code"], "STALE_EDITOR_TARGET", "{rejected}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "frame-local");
}

#[test]
fn rejected_editor_event_does_not_consume_operation_sequence() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);

    let status = agent.status();
    let context = &status["editor_context"];
    let operation = identity(&status);
    let rejected = input_request_with_operation(
        &mut agent,
        0,
        &status,
        operation.clone(),
        editor_binding(context, "retry-edit"),
        editing_event(json!({"type":"delete_range","start":1,"end":2})),
    );
    assert_eq!(rejected["code"], "INVALID_UTF16_RANGE", "{rejected}");
    assert_eq!(
        agent.status()["next_sequence"],
        operation["sequence"],
        "a deterministic rejection must leave the operation sequence available"
    );

    let retry = input_request_with_operation(
        &mut agent,
        1,
        &status,
        operation,
        editor_binding(context, "retry-edit"),
        editing_event(json!({"type":"replace_text","text":"retry-ok","input_type":"insertText"})),
    );
    assert_eq!(retry["value"]["editing"]["state"], "applied", "{retry}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "retry-ok");
}

#[test]
fn public_editor_consumer_never_exposes_password_surrounding_text() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<input id=secret type=password value=secret style='position:absolute;left:10px;top:10px;width:300px;height:30px'>"}));
    agent.click(20.0, 20.0);
    let status = agent.status();
    let context = &status["editor_context"];
    assert_eq!(context["kind"], "input", "{status}");
    assert_eq!(context["password"], true, "{status}");
    assert!(context["selection"].is_object(), "{status}");
    assert!(context["surrounding"].is_null(), "{status}");
    assert!(!context.to_string().contains("secret"), "{status}");
}

#[test]
fn public_editor_consumer_covers_context_composition_utf16_selection_and_contenteditable() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));

    let clicked = agent.click(20.0, 20.0);
    assert_eq!(clicked["value"]["input"]["state"], "succeeded", "{clicked}");
    let status = agent.status();
    let context = &status["editor_context"];
    assert_eq!(context["kind"], "input", "{status}");
    assert_eq!(context["editable"], true, "{status}");
    assert_eq!(context["selection"]["start"], 0, "{status}");
    assert!(context["target_id"].as_str().is_some_and(|value| !value.is_empty()));

    let replace = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "edit-field"),
        editing_event(json!({
            "type":"replace_text",
            "text":"A😀中",
            "input_type":"insertText"
        })),
    );
    assert_eq!(replace["value"]["editing"]["state"], "applied", "{replace}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "A😀中");
    let edited = agent.status();
    let context = &edited["editor_context"];
    assert_eq!(context["editor_revision"], 1, "{edited}");

    let split = input_request(
        &mut agent,
        0,
        &edited,
        editor_binding(context, "edit-field"),
        editing_event(json!({"type":"delete_range","start":1,"end":2})),
    );
    assert_eq!(split["code"], "INVALID_UTF16_RANGE", "{split}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "A😀中");

    let edited = agent.status();
    let context = &edited["editor_context"];
    let delete = input_request(
        &mut agent,
        0,
        &edited,
        editor_binding(context, "edit-field"),
        editing_event(json!({"type":"delete_range","start":1,"end":3})),
    );
    assert_eq!(delete["value"]["editing"]["state"], "applied", "{delete}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "A中");

    let selected = agent.status();
    let context = &selected["editor_context"];
    let selection = input_request(
        &mut agent,
        0,
        &selected,
        editor_binding(context, "edit-field"),
        editing_event(json!({"type":"set_selection","start":0,"end":2,"direction":"forward"})),
    );
    assert_eq!(selection["value"]["editing"]["state"], "applied", "{selection}");
    let selected = agent.status();
    let context = &selected["editor_context"];
    let replace_selection = input_request(
        &mut agent,
        0,
        &selected,
        editor_binding(context, "edit-field"),
        editing_event(json!({"type":"replace_text","text":"X","input_type":"insertText"})),
    );
    assert_eq!(
        replace_selection["value"]["editing"]["state"],
        "applied",
        "{replace_selection}"
    );
    assert_eq!(agent.eval("document.getElementById('field').value"), "X");

    agent.click(20.0, 70.0);
    let status = agent.status();
    let context = &status["editor_context"];
    assert_eq!(context["kind"], "textarea", "{status}");
    let begin = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "compose-1"),
        editing_event(json!({"type":"composition_begin","data":"中"})),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    assert_eq!(agent.status()["input_lease"]["kind"], "composition");
    let update = input_request(
        &mut agent,
        1,
        &status,
        editor_binding(context, "compose-1"),
        editing_event(json!({"type":"composition_update","data":"中文"})),
    );
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    assert_eq!(
        agent.status()["editor_context"]["surrounding"]["text"],
        "中文"
    );
    let commit = input_request(
        &mut agent,
        2,
        &status,
        editor_binding(context, "compose-1"),
        editing_event(json!({"type":"composition_commit","data":"中文"})),
    );
    assert_eq!(commit["value"]["editing"]["state"], "applied", "{commit}");
    assert_eq!(
        agent.status()["editor_context"]["surrounding"]["text"],
        "中文"
    );
    let replay = input_request(
        &mut agent,
        2,
        &status,
        editor_binding(context, "compose-1"),
        editing_event(json!({"type":"composition_commit","data":"中文"})),
    );
    assert_eq!(replay["value"], commit["value"], "replayed commit must not execute twice");
    assert_eq!(
        agent
            .eval("window.events.filter(e=>e.type==='input'&&e.target==='area').length")
            .as_f64(),
        Some(3.0)
    );

    agent.eval("document.getElementById('area').value='base';document.getElementById('area').setSelectionRange(4,4);true");
    let cancel_status = agent.status();
    let context = &cancel_status["editor_context"];
    let begin = input_request(
        &mut agent,
        0,
        &cancel_status,
        editor_binding(context, "compose-cancel"),
        editing_event(json!({"type":"composition_begin","data":"x"})),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let update = input_request(
        &mut agent,
        1,
        &cancel_status,
        editor_binding(context, "compose-cancel"),
        editing_event(json!({"type":"composition_update","data":"xy"})),
    );
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    assert_eq!(
        agent.status()["editor_context"]["surrounding"]["text"],
        "basexy"
    );
    let cancel = input_request(
        &mut agent,
        2,
        &cancel_status,
        editor_binding(context, "compose-cancel"),
        editing_event(json!({"type":"composition_cancel"})),
    );
    assert_eq!(cancel["value"]["editing"]["state"], "cancelled", "{cancel}");
    assert_eq!(agent.eval("document.getElementById('area').value"), "base");

    agent.eval("window.cancelNext=true;true");
    let cancelled_status = agent.status();
    let context = &cancelled_status["editor_context"];
    let _ = context;
    let cancelled = input_request(
        &mut agent,
        0,
        &cancelled_status,
        editor_binding(&cancelled_status["editor_context"], "cancel-beforeinput"),
        editing_event(json!({"type":"replace_text","text":"blocked","input_type":"insertText"})),
    );
    assert_eq!(cancelled["value"]["editing"]["state"], "cancelled", "{cancelled}");
    assert_eq!(agent.eval("document.getElementById('area').value"), "base");

    agent.click(20.0, 150.0);
    let status = agent.status();
    let context = &status["editor_context"];
    assert_eq!(context["kind"], "content_editable", "{status}");
    let rich = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "rich-1"),
        editing_event(json!({"type":"replace_text","text":"富文本","input_type":"insertText"})),
    );
    assert_eq!(rich["value"]["editing"]["state"], "applied", "{rich}");
    assert_eq!(
        agent.eval("document.getElementById('rich').textContent"),
        "seed富文本"
    );
    assert!(agent.eval(
        "window.events.some(e=>e.target==='rich'&&e.type==='beforeinput') && window.events.some(e=>e.target==='rich'&&e.type==='input')"
    ).as_bool().unwrap());

    let stale_status = agent.status();
    let stale = input_request(
        &mut agent,
        0,
        &stale_status,
        editor_binding(context, "stale-rich"),
        editing_event(json!({"type":"replace_text","text":"stale","input_type":"insertText"})),
    );
    assert_eq!(stale["code"], "STALE_EDITOR_REVISION", "{stale}");
    assert_eq!(
        agent.eval("document.getElementById('rich').textContent"),
        "seed富文本"
    );
}

#[test]
fn takeover_waits_for_gesture_and_composition_lease_terminal_events() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);

    let status = agent.status();
    let gesture_operation = identity(&status);
    let down = input_request_with_operation(
        &mut agent,
        0,
        &status,
        gesture_operation.clone(),
        json!({"type":"gesture","gesture_session_id":"gesture-1"}),
        json!({
            "type":"pointer_down",
            "pointer_id":1,
            "x":20.0,
            "y":20.0,
            "button":"left",
            "buttons":1,
            "modifiers":0
        }),
    );
    assert_eq!(down["value"]["input"]["state"], "pending", "{down}");
    assert_eq!(agent.status()["input_lease"]["kind"], "gesture");

    let mut human = daemon.connect();
    human.ok(json!({"type":"attach","mode":"observe"}));
    let epoch = human.status()["control"]["epoch"].clone();
    let waiting = human.ok(json!({"type":"request_takeover","epoch":epoch}));
    assert_eq!(waiting["control"]["phase"]["type"], "waiting", "{waiting}");
    assert_eq!(waiting["operation_running"], true);

    let up = input_request_with_operation(
        &mut agent,
        1,
        &status,
        gesture_operation,
        json!({"type":"gesture","gesture_session_id":"gesture-1"}),
        json!({
            "type":"pointer_up",
            "pointer_id":1,
            "x":20.0,
            "y":20.0,
            "button":"left",
            "buttons":0,
            "modifiers":0
        }),
    );
    assert_eq!(up["value"]["input"]["state"], "succeeded", "{up}");
    wait_until(
        || human.status()["control"]["phase"]["type"] == "human",
        Duration::from_secs(2),
        "takeover did not finish after gesture terminal event",
    );

    let human_status = human.status();
    human.ok(json!({"type":"release_control","epoch":human_status["control"]["epoch"]}));
    let status = agent.status();
    let context = &status["editor_context"];
    let begin = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "takeover-compose"),
        editing_event(json!({"type":"composition_begin","data":"中"})),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let epoch = human.status()["control"]["epoch"].clone();
    let waiting = human.ok(json!({"type":"request_takeover","epoch":epoch}));
    assert_eq!(waiting["control"]["phase"]["type"], "waiting", "{waiting}");
    let commit = input_request(
        &mut agent,
        1,
        &status,
        editor_binding(context, "takeover-compose"),
        editing_event(json!({"type":"composition_commit","data":"中"})),
    );
    assert_eq!(commit["value"]["editing"]["state"], "applied", "{commit}");
    wait_until(
        || human.status()["control"]["phase"]["type"] == "human",
        Duration::from_secs(2),
        "takeover did not finish after composition terminal event",
    );
}

#[test]
fn disconnect_cancels_open_composition_without_leaking_to_next_target() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":fixture()}));
    agent.click(20.0, 20.0);
    let status = agent.status();
    let context = &status["editor_context"];
    let begin = input_request(
        &mut agent,
        0,
        &status,
        editor_binding(context, "disconnect-compose"),
        editing_event(json!({"type":"composition_begin","data":"中"})),
    );
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");

    drop(agent);
    let mut reconnect = daemon.connect();
    reconnect.ok(json!({"type":"attach","mode":"agent"}));
    let status = reconnect.status();
    assert!(status["input_lease"].is_null(), "{status}");
    reconnect.click(20.0, 70.0);
    let status = reconnect.status();
    assert_eq!(status["editor_context"]["kind"], "textarea", "{status}");
    assert!(status["editor_context"]["composition"].is_null(), "{status}");
    assert_eq!(reconnect.eval("document.getElementById('area').value"), "");
}
