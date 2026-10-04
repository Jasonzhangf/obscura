#![cfg(unix)]

//! Public F5/F6 consumers. Profile creation must ride the existing operation
//! admission/replay/conflict owner, and CloseSession must flush the shared
//! profile store before it publishes `{"closed":true}`. All assertions come
//! from the real Host socket, the real profile filesystem, and a cold restart.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Root {
    dir: PathBuf,
}

impl Root {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from("/tmp").join(format!("o2-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).expect("create private test root");
        Self { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Daemon {
    child: Child,
    dir: PathBuf,
    profile_root: PathBuf,
}

impl Daemon {
    fn start(root: &Root) -> Self {
        let dir = root.path().join("s");
        let profile_root = root.path().join("profiles");
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir")
            .arg(&dir)
            .arg("--profile-root")
            .arg(&profile_root)
            .arg("--allow-private-network")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start Host");
        let mut daemon = Self {
            child,
            dir,
            profile_root,
        };
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

    fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    fn terminate(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let status = Command::new("kill")
                .arg("-TERM")
                .arg(self.child.id().to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("run kill");
            assert!(status.success(), "signal Host");
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "Host did not exit after SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
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

    fn mutation(&mut self, operation: Value, command: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.raw(json!({"id": id, "operation": operation, "command": command}))
    }

    fn eval(&mut self, expression: &str) -> Value {
        let status = self.status();
        let response = self.mutation(identity(&status), json!({"type":"evaluate","expression":expression}));
        assert_eq!(response["type"], "result", "{response}");
        response["value"]["result"]["value"].clone()
    }

    fn navigate(&mut self, url: &str) -> Value {
        let status = self.status();
        let response = self.mutation(identity(&status), json!({"type":"navigate","url":url}));
        assert_eq!(response["type"], "result", "{response}");
        response["value"].clone()
    }
}

fn identity(status: &Value) -> Value {
    let sequence = status["next_sequence"].as_u64().unwrap();
    json!({
        "operation_id": format!("profile-op-{sequence}"),
        "session_id": status["session_id"],
        "attachment_id": status["attachment_id"],
        "sequence": sequence,
        "control_epoch": status["control"]["epoch"],
        "viewport_revision": status["viewport_revision"],
        "document_revision": status["document_revision"]
    })
}

fn start_page_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = b"<input id=f value=seed>";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    format!("http://127.0.0.1:{port}/page.html")
}

fn origin_file(profile_root: &Path, profile_id: &str) -> Option<PathBuf> {
    let dir = profile_root.join("profiles").join(profile_id).join("localStorage");
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            return Some(path);
        }
    }
    None
}

#[test]
fn profile_create_replays_identical_and_rejects_conflict() {
    let root = Root::new("replay");
    let daemon = Daemon::start(&root);
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));

    let before = agent.ok(json!({"type":"profile_catalog"}));
    let before_count = before["catalog"]["profiles"].as_array().unwrap().len();

    let status = agent.status();
    let operation = identity(&status);
    let command = json!({"type":"profile_create","display_name":"Work","make_default":false});
    let created = agent.mutation(operation.clone(), command.clone());
    assert_eq!(created["type"], "result", "{created}");
    let profile_id = created["value"]["profile"]["profile_id"].as_str().unwrap().to_string();
    assert_eq!(created["value"]["catalog"]["profiles"].as_array().unwrap().len(), before_count + 1);

    // Same operation identity and identical content: the cached receipt is
    // returned and no second profile is created.
    let replay = agent.mutation(operation.clone(), command.clone());
    assert_eq!(replay["type"], "result", "{replay}");
    assert_eq!(replay["value"], created["value"], "identical replay must return the same receipt");
    let after_replay = agent.ok(json!({"type":"profile_catalog"}));
    let profiles = after_replay["catalog"]["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), before_count + 1, "replay must not create a second profile");
    assert_eq!(
        profiles.iter().filter(|profile| profile["profile_id"] == json!(profile_id)).count(),
        1
    );

    // Same operation identity, different content: rejected before any effect.
    let conflict = agent.mutation(operation.clone(), json!({"type":"profile_create","display_name":"Other","make_default":false}));
    assert_eq!(conflict["code"], "OPERATION_CONFLICT", "{conflict}");
    let after_conflict = agent.ok(json!({"type":"profile_catalog"}));
    assert_eq!(
        after_conflict["catalog"]["profiles"].as_array().unwrap().len(),
        before_count + 1
    );

    // Old sequence with a fresh operation identity is expired, not replayed.
    let mut stale = identity(&agent.status());
    stale["operation_id"] = json!("profile-op-stale");
    stale["sequence"] = operation["sequence"].clone();
    let expired = agent.mutation(stale, command.clone());
    assert_eq!(expired["code"], "OPERATION_EXPIRED", "{expired}");

    // A binding for another attachment is refused.
    let mut other_binding = identity(&agent.status());
    other_binding["attachment_id"] = json!(999_999);
    let rejected = agent.mutation(other_binding, command);
    assert_eq!(rejected["code"], "STALE_ATTACHMENT", "{rejected}");

    let final_catalog = agent.ok(json!({"type":"profile_catalog"}));
    assert_eq!(
        final_catalog["catalog"]["profiles"].as_array().unwrap().len(),
        before_count + 1,
        "rejections must not change the catalog"
    );
}

#[test]
fn close_session_flushes_before_closed_and_survives_cold_restart() {
    let root = Root::new("close");
    let url = start_page_server();
    let mut daemon = Daemon::start(&root);
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.navigate(&url);
    assert_eq!(agent.eval("localStorage.setItem('close','v');localStorage.getItem('close')"), "v");

    let status = agent.status();
    let closed = agent.mutation(identity(&status), json!({"type":"close_session"}));
    assert_eq!(closed["type"], "result", "{closed}");
    assert_eq!(closed["value"]["closed"], true, "{closed}");

    // The value must already be on disk before the process restarts, proving
    // CloseSession flushed the store instead of relying on process teardown.
    let file = origin_file(daemon.profile_root(), "default").expect("origin snapshot after close");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("\"close\""), "close must flush the value: {text}");

    daemon.terminate();
    let restarted = Daemon::start(&root);
    let mut recovered = restarted.connect();
    recovered.ok(json!({"type":"attach","mode":"agent"}));
    recovered.navigate(&url);
    assert_eq!(
        recovered.eval("localStorage.getItem('close')"),
        "v",
        "cold restart must observe the close-flushed value"
    );
}

#[test]
fn close_session_storage_failure_is_typed_and_recoverable() {
    let root = Root::new("failure");
    let url = start_page_server();
    let daemon = Daemon::start(&root);
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.navigate(&url);
    assert_eq!(agent.eval("localStorage.setItem('fail','v');localStorage.getItem('fail')"), "v");
    agent.ok(json!({"type":"flush_profiles"}));

    let file = origin_file(daemon.profile_root(), "default").expect("origin snapshot after flush");
    let stem = file.file_stem().unwrap().to_str().unwrap().to_string();
    let blocked = file.with_file_name(format!("{stem}.json.tmp"));
    std::fs::create_dir(&blocked).expect("create blocking directory");

    let status = agent.status();
    let failed = agent.mutation(identity(&status), json!({"type":"close_session"}));
    assert_eq!(failed["type"], "error", "{failed}");
    assert_eq!(failed["code"], "PROFILE_STORAGE_FAILED", "{failed}");
    assert!(
        failed["value"]["closed"] != json!(true),
        "a failed flush must never publish closed:true: {failed}"
    );

    // The session must remain recoverable: status still reports a live
    // viewport, and a fresh close succeeds once the filesystem is unblocked.
    let alive = agent.status();
    assert!(!alive["viewport"].is_null(), "session must stay open after a failed close: {alive}");
    std::fs::remove_dir(&blocked).expect("remove blocking directory");
    let status = agent.status();
    let closed = agent.mutation(identity(&status), json!({"type":"close_session"}));
    assert_eq!(closed["type"], "result", "{closed}");
    assert_eq!(closed["value"]["closed"], true, "{closed}");
}
