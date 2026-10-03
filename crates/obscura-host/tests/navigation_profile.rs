#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
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
        let dir = std::path::PathBuf::from("/tmp").join(format!("onp-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).expect("create private test root");
        Self { dir }
    }
    fn path(&self) -> &Path { &self.dir }
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
        let dir = root.path().join(format!("sock-{}", uuid::Uuid::new_v4().simple()));
        let profile_root = root.path().join("profiles");
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(&dir)
            .arg("--profile-root").arg(&profile_root)
            .arg("--allow-private-network")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start Host");
        let mut daemon = Self { child, dir, profile_root };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match UnixStream::connect(daemon.dir.join("host.sock")) {
                Ok(_) => break,
                Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {}
                Err(error) => panic!("Host listener startup: {error}"),
            }
            assert!(daemon.child.try_wait().unwrap().is_none(), "Host exited before listener ready");
            assert!(Instant::now() < deadline, "Host startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }

    fn connect(&self) -> Client {
        let stream = UnixStream::connect(self.dir.join("host.sock")).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut client = Client { reader: BufReader::new(stream) };
        assert_eq!(client.read()["type"], "ready");
        client
    }

    fn terminate(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let pid = self.child.id();
            let status = Command::new("kill").arg("-TERM").arg(pid.to_string())
                .stdout(Stdio::null()).stderr(Stdio::null()).status().expect("run kill");
            assert!(status.success(), "signal Host");
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.child.try_wait().unwrap().is_some() { break; }
            assert!(Instant::now() < deadline, "Host did not exit after SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn profile_root(&self) -> &Path { &self.profile_root }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
        if self.dir.exists() { let _ = std::fs::remove_dir_all(&self.dir); }
    }
}

struct Client {
    reader: BufReader<UnixStream>,
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
        let response = self.send(command.clone());
        assert_eq!(response["type"], "result", "command {command} -> {response}");
        response["value"].clone()
    }
    fn error(&mut self, command: Value) -> String {
        let response = self.send(command.clone());
        assert_eq!(response["type"], "error", "command {command} -> {response}");
        response["code"].as_str().unwrap().to_string()
    }
    fn send(&mut self, command: Value) -> Value {
        let command_type = command["type"].as_str().unwrap_or("");
        let mutation = matches!(command_type,
            "evaluate" | "navigate" | "back" | "forward" | "reload"
                | "tab_create" | "tab_select" | "tab_close"
                | "profile_create" | "profile_select"
                | "resize" | "close_session" | "click" | "input_text" | "scroll" | "input");
        let operation = if mutation {
            let status = self.status();
            Some(identity(&status))
        } else {
            None
        };
        self.raw(json!({"id":1,"operation":operation,"command":command}))
    }
    fn status(&mut self) -> Value { self.ok_readonly(json!({"type":"status"})) }
    fn ok_readonly(&mut self, command: Value) -> Value {
        let response = self.raw(json!({"id":1,"operation":null,"command":command}));
        assert_eq!(response["type"], "result", "{response}");
        response["value"].clone()
    }
    fn eval(&mut self, expression: &str) -> Value {
        self.ok(json!({"type":"evaluate","expression":expression}))["result"]["value"].clone()
    }
}

fn identity(status: &Value) -> Value {
    json!({
        "session_id": status["session_id"],
        "attachment_id": status["attachment_id"],
        "sequence": status["next_sequence"],
        "control_epoch": status["control"]["epoch"],
        "viewport_revision": status["viewport_revision"],
        "document_revision": status["document_revision"],
    })
}

#[test]
fn navigation_tabs_and_profiles_keep_actual_page_state_across_restart() {
    let root = Root::new("main");
    let mut daemon = Daemon::start(&root);
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));

    assert_eq!(agent.error(json!({"type":"back"})), "NO_HISTORY");
    assert_eq!(agent.error(json!({"type":"forward"})), "NO_HISTORY");
    let shared_data_url = "data:text/html,<title>first</title><input id=field value=saved><script>localStorage.setItem('tab0','yes');window.tabMarker='zero';</script>";
    let first = agent.ok(json!({"type":"navigate","url":shared_data_url}));
    assert_eq!(first["navigation"]["url"], shared_data_url);
    assert_eq!(first["navigation"]["title"], "first");
    let first_revision = first["presentation_revision"].as_u64().unwrap();
    let second = agent.ok(json!({"type":"navigate","url":"data:text/html,<title>second</title>ok"}));
    assert_eq!(second["navigation"]["title"], "second");
    let back = agent.ok(json!({"type":"back"}));
    assert_eq!(back["navigation"]["title"], "first");
    assert_eq!(agent.eval("window.tabMarker"), "zero");
    assert_eq!(agent.eval("localStorage.getItem('tab0')"), "yes");
    let forward = agent.ok(json!({"type":"forward"}));
    assert_eq!(forward["navigation"]["title"], "second");
    agent.ok(json!({"type":"back"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<title>truncated</title>done"}));
    assert_eq!(agent.error(json!({"type":"forward"})), "NO_HISTORY");
    let single = agent.status();
    assert_eq!(agent.error(json!({"type":"tab_close","tab_id":single["active_tab_id"]})), "LAST_TAB");

    let tab_b = agent.ok(json!({"type":"tab_create"}));
    assert_eq!(tab_b["tabs"].as_array().unwrap().len(), 2);
    let tab_b_id = tab_b["active_tab_id"].as_str().unwrap().to_string();
    assert_eq!(tab_b["navigation"]["title"], "");
    agent.ok(json!({"type":"navigate","url":shared_data_url}));
    assert_eq!(agent.eval("localStorage.getItem('tab0')"), "yes");
    agent.ok(json!({"type":"evaluate","expression":"window.tabBMarker={value:7};localStorage.setItem('tab1','yes');true"}));
    let return_a = agent.ok(json!({"type":"tab_select","tab_id":first["active_tab_id"].as_str().unwrap()}));
    assert_eq!(return_a["navigation"]["title"], "truncated");
    assert_eq!(return_a["presentation_revision"], first_revision + 2);
    let back_to_b = agent.ok(json!({"type":"tab_select","tab_id":tab_b_id}));
    assert_eq!(back_to_b["presentation_revision"], first_revision + 3);
    assert_eq!(agent.eval("window.tabBMarker.value"), 7.0);
    let catalog = agent.ok(json!({"type":"profile_catalog"}));
    let catalog_value = &catalog["catalog"];
    assert_eq!(catalog_value["root_configured"], true, "{catalog}");
    assert_eq!(catalog_value["profiles"].as_array().unwrap().len(), 1);
    let created = agent.ok(json!({"type":"profile_create","display_name":"Work","make_default":true}));
    let work_id = created["profile"]["profile_id"].as_str().unwrap().to_string();
    assert_eq!(created["profile"]["display_name"], "Work");
    let selected = agent.ok(json!({"type":"profile_select","profile_id":work_id}));
    assert_eq!(selected["binding"]["profile_id"], work_id);
    let work_session_id = selected["status"]["session_id"].as_str().unwrap().to_string();
    assert_eq!(selected["status"]["mode"], "observe");
    assert_eq!(selected["status"]["agent_attached"], false);
    // Profile select rebinds the caller as an observer. It never grants page
    // write authority, so the old Agent identity must be rejected here.
    assert_eq!(agent.error(json!({"type":"evaluate","expression":"localStorage.getItem('tab0')"})), "AGENT_REQUIRED");

    // A fresh Agent attached to the selected profile sees an isolated store:
    // nothing leaks from the default profile.
    let mut work_agent = daemon.connect();
    let work_status = work_agent.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(work_status["profile_id"], work_id);
    assert_eq!(work_agent.eval("localStorage.getItem('tab0')"), Value::Null);

    // The default session was not killed by the rebind. Select it back and a
    // fresh Agent sees the original document and its localStorage intact.
    let back_to_default = agent.ok(json!({"type":"profile_select","profile_id":"default"}));
    assert_eq!(back_to_default["binding"]["profile_id"], "default");
    let mut other = daemon.connect();
    let old_session = other.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(old_session["profile_id"], "default");
    assert!(old_session["agent_attached"].as_bool().unwrap_or(false));
    assert_eq!(other.eval("localStorage.getItem('tab0')"), "yes");

    // Host-local maintenance drain flushes every loaded profile and reports a
    // typed saved receipt before a service may terminate the process.
    let flushed = agent.ok(json!({"type":"flush_profiles"}));
    assert_eq!(flushed["flushed"], true, "{flushed}");
    assert_eq!(flushed["profiles"], 2, "{flushed}");

    daemon.terminate();
    let mut restarted = Daemon::start(&root);
    let mut recovered = restarted.connect();
    let restored = recovered.ok(json!({"type":"attach","mode":"agent"}));
    // `p1` was created with make_default, so a restarted Host opens it first.
    // session/epoch/attachment are all new even though the profile persists.
    assert_eq!(restored["profile_id"], work_id);
    assert_ne!(restored["session_id"], work_session_id);
    let work = recovered.ok(json!({"type":"profile_select","profile_id":work_id}));
    assert_eq!(work["binding"]["profile_id"], work_id);
    let back = recovered.ok(json!({"type":"profile_select","profile_id":"default"}));
    assert_eq!(back["binding"]["profile_id"], "default");
    let takeover = recovered.ok(json!({"type":"request_takeover","epoch":back["status"]["control"]["epoch"]}));
    assert_eq!(takeover["control"]["phase"]["type"], "human");
    recovered.ok(json!({"type":"release_control","epoch":takeover["control"]["epoch"]}));
    let mut inspect = restarted.connect();
    let agent_status = inspect.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(agent_status["profile_id"], "default");
    assert_eq!(agent_status["mode"], "agent", "{agent_status}");
    assert_eq!(agent_status["agent_attached"], true, "{agent_status}");
    assert_eq!(agent_status["control"]["phase"]["type"], "agent", "{agent_status}");
    inspect.ok(json!({"type":"navigate","url":shared_data_url}));
    assert_eq!(inspect.eval("localStorage.getItem('tab0')"), "yes");
    assert_eq!(inspect.eval("localStorage.getItem('tab1')"), "yes");
    let recovered_catalog = recovered.ok(json!({"type":"profile_catalog"}));
    assert_eq!(recovered_catalog["catalog"]["profiles"].as_array().unwrap().len(), 2);
    assert_eq!(recovered_catalog["catalog"]["default_profile_id"], work_id);
}

#[test]
fn corrupt_profile_catalog_fails_startup_with_a_typed_error() {
    let root = Root::new("badcatalog");
    let profile_root = root.path().join("profiles");
    std::fs::create_dir_all(&profile_root).unwrap();
    // An existing but malformed catalog must fail explicitly instead of being
    // silently replaced by an empty success.
    std::fs::write(profile_root.join("catalog.json"), b"{ not valid json").unwrap();
    let sock = root.path().join("sock");
    let output = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(&sock)
        .arg("--profile-root").arg(&profile_root)
        .output()
        .expect("run Host");
    assert!(!output.status.success(), "corrupt catalog must fail startup: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("PROFILE_CATALOG_FAILED"), "stderr: {stderr}");
    assert!(!sock.exists(), "failed startup must not leave a bound socket");
}
