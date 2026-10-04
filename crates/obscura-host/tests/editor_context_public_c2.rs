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
        let dir = std::path::PathBuf::from("/tmp").join(format!("ohc2-{}", uuid::Uuid::new_v4()));
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
        let id = self.next_id;
        self.next_id += 1;
        let operation = identity(&self.ok(json!({"type":"status"})));
        self.raw(json!({
            "id": id,
            "operation": operation,
            "command": {"type":"click","x":x,"y":y}
        }))
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
    let id = client.next_id;
    client.next_id += 1;
    client.raw(json!({
        "id": id,
        "operation": identity(status),
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

#[test]
fn evaluate_mutation_publishes_new_context_and_keeps_stale_binding_rejected() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<input id=field style='position:absolute;left:10px;top:10px;width:300px;height:30px'>"}));
    agent.click(20.0, 20.0);

    let initial = agent.status();
    let initial_revision = initial["editor_context"]["editor_revision"].as_u64().unwrap();
    let stale = editor_binding(&initial["editor_context"], "stale-evaluate-mutation");
    agent.eval(
        "(function(){var f=document.getElementById('field');f.value='changed';f.setSelectionRange(0,7);return true;})()",
    );

    let after_mutation = agent.status();
    let mutated_revision = after_mutation["editor_context"]["editor_revision"].as_u64().unwrap();
    assert!(
        mutated_revision > initial_revision,
        "evaluate mutation must advance the editor revision: {after_mutation}"
    );
    assert_eq!(
        after_mutation["editor_context"]["selection"]["start"], 0,
        "{after_mutation}"
    );
    assert_eq!(
        after_mutation["editor_context"]["selection"]["end"], 7,
        "{after_mutation}"
    );
    let rejected = input_request(
        &mut agent,
        0,
        &after_mutation,
        stale,
        json!({"type":"replace_text","text":"blocked","input_type":"insertText"}),
    );
    assert_eq!(rejected["code"], "STALE_EDITOR_REVISION", "{rejected}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "changed");

    // The rejected stale attempt rolled its sequence back, and the value read
    // above advanced it again, so take a fresh status for the successful edit.
    let fresh_status = agent.status();
    let fresh = editor_binding(&fresh_status["editor_context"], "fresh-evaluate-mutation");
    let applied = input_request(
        &mut agent,
        0,
        &fresh_status,
        fresh,
        json!({"type":"replace_text","text":"fresh","input_type":"insertText"}),
    );
    assert_eq!(applied["value"]["editing"]["state"], "applied", "{applied}");
    assert_eq!(agent.eval("document.getElementById('field').value"), "fresh");

    agent.eval("document.activeElement.blur();document.activeElement===document.body");
    let blurred = agent.status();
    assert!(
        blurred["editor_context"].is_null(),
        "Evaluate must publish a real lost focus as null context: {blurred}"
    );
}
