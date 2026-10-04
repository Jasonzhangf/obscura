#![cfg(unix)]

//! Public F2 lease consumers. A disconnect must terminate the Engine's open
//! composition before the Host releases the authorizing lease, and a direct
//! same-session cancel must stay green. Both cases are asserted from real Host
//! socket responses and Page values, never private Host fields.

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
        let dir = std::path::PathBuf::from("/tmp").join(format!("olc2-{}", uuid::Uuid::new_v4().simple()));
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir")
            .arg(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
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
            .set_read_timeout(Some(Duration::from_secs(15)))
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
        assert!(self.reader.read_line(&mut line).unwrap() > 0, "Host closed the socket");
        serde_json::from_str(&line).unwrap()
    }

    fn raw(&mut self, request: Value) -> Value {
        writeln!(self.reader.get_mut(), "{request}").unwrap();
        self.read()
    }

    fn ok(&mut self, command: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let response = self.raw(json!({"id": id, "operation": null, "command": command}));
        assert_eq!(response["type"], "result", "command {command} -> {response}");
        response["value"].clone()
    }

    fn status(&mut self) -> Value {
        self.ok(json!({"type":"status"}))
    }

    fn click(&mut self, x: f64, y: f64) -> Value {
        let status = self.status();
        let id = self.next_id;
        self.next_id += 1;
        let response = self.raw(json!({
            "id": id,
            "operation": identity(&status),
            "command": {"type":"click","x":x,"y":y}
        }));
        assert_eq!(response["type"], "result", "{response}");
        response["value"].clone()
    }

    fn input(&mut self, status: &Value, operation: Value, event_sequence: u64, binding: Value, event: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.raw(json!({
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
}

fn identity(status: &Value) -> Value {
    let sequence = status["next_sequence"].as_u64().unwrap();
    json!({
        "operation_id": format!("lease-op-{sequence}"),
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

const COMPOSE_PAGE: &str = "data:text/html,<input id=f value=seed style='position:absolute;left:10px;top:10px;width:300px;height:30px'>";

fn focus_editor(client: &mut Client) {
    client.ok(json!({"type":"attach","mode":"agent"}));
    let status = client.status();
    let id = client.next_id;
    client.next_id += 1;
    let navigate = client.raw(json!({
        "id": id,
        "operation": identity(&status),
        "command": {"type":"navigate","url":COMPOSE_PAGE}
    }));
    assert_eq!(navigate["type"], "result", "{navigate}");
    client.click(20.0, 20.0);
    let status = client.status();
    assert!(
        status["editor_context"]["target_id"].is_string(),
        "click must focus a real editable target: {status}"
    );
}

fn wait_for_clean_lease(daemon: &Daemon) -> Client {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut client = daemon.connect();
        let attach = client.raw(json!({"id":1,"operation":null,"command":{"type":"attach","mode":"agent"}}));
        if attach["type"] == "result" {
            let mut id = client.next_id;
            loop {
                let response = client.raw(json!({"id":id,"operation":null,"command":{"type":"status"}}));
                id += 1;
                client.next_id = id;
                assert_eq!(response["type"], "result", "{response}");
                let status = &response["value"];
                if status["input_lease"].is_null() {
                    return client;
                }
                assert!(Instant::now() < deadline, "disconnect terminal never released the lease: {status}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(Instant::now() < deadline, "could not reattach after disconnect: {attach}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn disconnect_terminates_engine_composition_before_lease_release() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    focus_editor(&mut agent);

    let status = agent.status();
    let context = status["editor_context"].clone();
    let operation = identity(&status);
    let binding = editor_binding(&context, "disconnect-1");
    let begin = agent.input(&status, operation.clone(), 0, binding.clone(), json!({"type":"composition_begin","data":"中"}));
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let update = agent.input(&status, operation.clone(), 1, binding, json!({"type":"composition_update","data":"中文"}));
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    let provisional = agent.status()["editor_context"]["surrounding"]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(provisional, "seed", "composition update must be visible before disconnect");

    // Drop the attachment without sending a terminal. The Host must deliver a
    // real Engine cancel and only then release the lease.
    drop(agent);

    let mut reattached = wait_for_clean_lease(&daemon);
    let clean = reattached.status();
    assert!(clean["input_lease"].is_null(), "{clean}");
    assert!(
        clean["editor_context"]["composition"].is_null(),
        "disconnect must clear the Engine composition: {clean}"
    );
    assert_eq!(
        clean["editor_context"]["surrounding"]["text"],
        "seed",
        "terminal cancel must restore the pre-composition snapshot: {clean}"
    );

    // A new composition on the same target must be admitted, not refused as
    // COMPOSITION_ALREADY_ACTIVE, and it must not inherit the old provisional
    // text or held pointer.
    let next_context = clean["editor_context"].clone();
    let next_operation = identity(&clean);
    let next_binding = editor_binding(&next_context, "disconnect-2");
    let next_begin = reattached.input(
        &clean,
        next_operation.clone(),
        0,
        next_binding.clone(),
        json!({"type":"composition_begin","data":"文"}),
    );
    assert_eq!(next_begin["value"]["editing"]["state"], "pending", "{next_begin}");
    let next_provisional = reattached.status()["editor_context"]["surrounding"]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        next_provisional.contains('文') && next_provisional != "seed",
        "new composition must start cleanly on the same target: {next_provisional}"
    );
    let next_cancel = reattached.input(
        &clean,
        next_operation,
        1,
        next_binding,
        json!({"type":"composition_cancel"}),
    );
    assert_eq!(next_cancel["value"]["editing"]["state"], "cancelled", "{next_cancel}");
    let final_status = reattached.status();
    assert!(final_status["input_lease"].is_null(), "{final_status}");
    assert_eq!(
        final_status["editor_context"]["surrounding"]["text"],
        "seed"
    );
}

#[test]
fn direct_same_session_composition_cancel_stays_green() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    focus_editor(&mut agent);

    let status = agent.status();
    let context = status["editor_context"].clone();
    let operation = identity(&status);
    let binding = editor_binding(&context, "direct-cancel");
    let begin = agent.input(&status, operation.clone(), 0, binding.clone(), json!({"type":"composition_begin","data":"中"}));
    assert_eq!(begin["value"]["editing"]["state"], "pending", "{begin}");
    let update = agent.input(&status, operation.clone(), 1, binding.clone(), json!({"type":"composition_update","data":"中文"}));
    assert_eq!(update["value"]["editing"]["state"], "pending", "{update}");
    let cancel = agent.input(&status, operation, 2, binding, json!({"type":"composition_cancel"}));
    assert_eq!(cancel["value"]["editing"]["state"], "cancelled", "{cancel}");
    let after = agent.status();
    assert!(after["input_lease"].is_null(), "{after}");
    assert!(after["editor_context"]["composition"].is_null(), "{after}");
    assert_eq!(after["editor_context"]["surrounding"]["text"], "seed");
}
