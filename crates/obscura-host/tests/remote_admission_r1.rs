//! Public Host-side regression for the remote profile admission slice.
//!
//! Every case drives the real `obscura-host` process over its real
//! `remote-admission.sock` / `host.sock` sockets. Nothing here mocks an
//! internal function or asserts on source text: the assertions are the bytes a
//! real peer reads, the bytes it writes, and the persisted catalog file.
//!
//! No case configures an authorization registry, membership or scope. The
//! admission frame is `{ token, request_id }`: the Host owns the one token
//! comparison and binds the catalog default profile. A peer reaches another
//! profile with an explicit `profile_select` on its own attachment.
//!
//! RP-42/43/44 (endpoint consumer rules) live in the `endpoint_*` tests at the
//! bottom of this file; they run the real `obscura-endpoint` binary against a
//! controlled Host admission socket.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

/// The one access token this Host is configured with.
const TOKEN: &str = "remote-admission-r1-token";

fn tmp_root(name: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("ora-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn profile_entry(profile_id: &str, display_name: &str, default: bool, access: Option<(&str, &[&str])>) -> Value {
    let mut value = json!({"profile_id": profile_id, "display_name": display_name, "default": default});
    if let Some((owner, members)) = access {
        value["access"] = json!({"owner_device_id": owner, "members": members});
    }
    value
}

fn catalog(default_profile_id: &str, next_id: u64, profiles: Vec<Value>) -> Value {
    json!({
        "catalog_revision": 1,
        "profiles": profiles,
        "default_profile_id": default_profile_id,
        "next_id": next_id,
    })
}

/// One remote peer. It carries the one access token and nothing else: no device
/// identity, scope, grant or initial profile selection takes part in admission.
#[derive(Clone)]
struct DeviceSpec {
    token: String,
}

impl DeviceSpec {
    fn new() -> Self {
        Self { token: TOKEN.into() }
    }

    fn wrong_token() -> Self {
        Self { token: "not-the-host-token".into() }
    }

    fn admission(&self, request_id: u64) -> Value {
        json!({"token": self.token, "request_id": request_id})
    }
}

struct HostFixture {
    root: PathBuf,
    socket_dir: PathBuf,
    profile_root: PathBuf,
    child: Child,
}

impl HostFixture {
    /// Start the real Host with a catalog and one access token.
    async fn start(name: &str, catalog_value: Value) -> Self {
        let root = tmp_root(name);
        let socket_dir = root.join("sock");
        let profile_root = root.join("profiles");
        std::fs::create_dir_all(&profile_root).unwrap();
        std::fs::write(profile_root.join("catalog.json"), serde_json::to_vec_pretty(&catalog_value).unwrap()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(&socket_dir)
            .arg("--profile-root").arg(&profile_root)
            .arg("--access-token").arg(TOKEN)
            .arg("--allow-private-network")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("start Host");
        let fixture = Self { root, socket_dir, profile_root, child };
        fixture.wait_socket("host.sock").await;
        fixture.wait_socket("remote-admission.sock").await;
        fixture
    }

    fn admission_path(&self) -> PathBuf { self.socket_dir.join("remote-admission.sock") }
    fn host_path(&self) -> PathBuf { self.socket_dir.join("host.sock") }
    fn profile_root(&self) -> &Path { &self.profile_root }

    /// Count the per-session sockets the Host bound. A refused admission must
    /// not add one.
    fn session_sockets(&self) -> usize {
        std::fs::read_dir(&self.socket_dir).unwrap().flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("session-") && name.ends_with(".sock")
            })
            .count()
    }

    async fn wait_socket(&self, name: &str) {
        let path = self.socket_dir.join(name);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !path.exists() {
            assert!(tokio::time::Instant::now() < deadline, "socket did not appear: {}", path.display());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn admission(&self) -> Conn {
        let stream = UnixStream::connect(self.admission_path()).await.expect("connect remote-admission.sock");
        Conn { reader: BufReader::new(stream) }
    }

    async fn local(&self) -> Conn {
        let stream = UnixStream::connect(self.host_path()).await.expect("connect host.sock");
        Conn { reader: BufReader::new(stream) }
    }

    fn terminate(&mut self) {
        let pid = self.child.id().expect("Host pid");
        let status = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status().expect("run kill");
        assert!(status.success(), "signal Host");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while self.child.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "Host did not exit after SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for HostFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Conn {
    reader: BufReader<UnixStream>,
}

impl Conn {
    async fn write_bytes(&mut self, bytes: &[u8]) {
        tokio::time::timeout(Duration::from_secs(30), self.reader.get_mut().write_all(bytes))
            .await.expect("write deadline").expect("write bytes");
    }

    async fn write_json(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).unwrap();
        line.push('\n');
        self.write_bytes(line.as_bytes()).await;
    }

    async fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
            .await.expect("response deadline").expect("read line");
        if read == 0 { None } else { Some(line) }
    }

    async fn read_json(&mut self) -> Value {
        let line = self.read_line().await.expect("expected a JSON line before EOF");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("invalid JSON line {line:?}: {error}"))
    }

    async fn expect_eof(&mut self) {
        assert!(self.read_line().await.is_none(), "expected the Host to close the admission stream");
    }
}

async fn admit_ok(fixture: &HostFixture, device: &DeviceSpec, request_id: u64) -> (Conn, Value, Value) {
    let mut conn = fixture.admission().await;
    conn.write_json(&device.admission(request_id)).await;
    let result = conn.read_json().await;
    assert_eq!(result["type"], "ok", "admission result: {result}");
    assert_eq!(result["request_id"], request_id);
    let ready = conn.read_json().await;
    assert_eq!(ready["type"], "ready", "ready: {ready}");
    assert_eq!(ready["version"], 11);
    assert_eq!(ready["session_id"], result["session_id"], "Ready must match the admission session");
    (conn, result, ready)
}

async fn attach_observe(conn: &mut Conn) -> Value {
    conn.write_json(&json!({"id": 2, "command": {"type": "attach", "mode": "observe"}})).await;
    let response = conn.read_json().await;
    assert_eq!(response["type"], "result", "attach response: {response}");
    response["value"].clone()
}

/// Bind one peer to `profile_id` with an operation identity for its own
/// attachment, and return the `ProfileSelected` value.
async fn select_profile(conn: &mut Conn, status: &Value, profile_id: &str) -> Value {
    conn.write_json(&json!({
        "id": 3,
        "operation": operation(status),
        "command": {"type": "profile_select", "profile_id": profile_id},
    })).await;
    let response = conn.read_json().await;
    assert_eq!(response["type"], "result", "profile_select response: {response}");
    response["value"].clone()
}

fn operation(status: &Value) -> Value {
    json!({
        "session_id": status["session_id"],
        "attachment_id": status["attachment_id"],
        "sequence": status["next_sequence"],
        "control_epoch": status["control"]["epoch"],
        "viewport_revision": status["viewport_revision"],
        "document_revision": status["document_revision"],
    })
}

fn default_access<'a>(owner: &'a str, members: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    Some((owner, members))
}

// ---------------------------------------------------------------------------
// RP-01 .. RP-15 catalog/create/select, two profiles, registered A/B, denial,
// isolation, migration and local regression.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rp01_remote_catalog_is_unfiltered() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp01", catalog("default", 3, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Other", false, default_access("dev-b", &["dev-b"])),
        ])).await;
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut conn).await;
        conn.write_json(&json!({"id": 3, "command": {"type": "profile_catalog"}})).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "result", "catalog response: {response}");
        let catalog = &response["value"]["catalog"];
        let ids: Vec<&str> = catalog["profiles"].as_array().unwrap().iter()
            .map(|profile| profile["profile_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["default", "p1"], "a Host-side access record must not filter the remote catalog: {catalog}");
        assert_eq!(catalog["default_profile_id"], "default");
    }).await.expect("rp01 timed out");
}

#[tokio::test]
async fn rp03_remote_select_rebinds_without_killing_source() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp03", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (mut conn, result, _) = admit_ok(&fixture, &device_a, 1).await;
        let status = attach_observe(&mut conn).await;
        conn.write_json(&json!({
            "id": 3,
            "operation": operation(&status),
            "command": {"type": "profile_select", "profile_id": "p1"},
        })).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "result", "select response: {response}");
        assert_eq!(response["value"]["binding"]["profile_id"], "p1");
        let selected_session = response["value"]["binding"]["session_id"].as_str().unwrap().to_string();
        assert_ne!(selected_session, result["session_id"].as_str().unwrap(), "rebind must publish a distinct session");
        // The source session must survive the caller's rebind.
        let (mut second, second_result, _) = admit_ok(&fixture, &device_a, 4).await;
        attach_observe(&mut second).await;
        assert_eq!(second_result["session_id"], result["session_id"], "default session was destroyed by the rebind");
    }).await.expect("rp03 timed out");
}

#[tokio::test]
async fn rp05_two_profile_storage_is_isolated() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp05", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        local.write_json(&json!({"id": 1, "command": {"type": "attach", "mode": "agent"}})).await;
        let agent_status = local.read_json().await;
        assert_eq!(agent_status["type"], "result", "agent attach: {agent_status}");
        local.write_json(&json!({"id": 2, "operation": operation(&agent_status["value"]), "command": {"type": "navigate", "url": "data:text/html,<script>localStorage.setItem('k','default')</script><title>seeded</title>"}})).await;
        let seeded = local.read_json().await;
        assert_eq!(seeded["type"], "result", "seed navigation: {seeded}");
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        let status = attach_observe(&mut conn).await;
        conn.write_json(&json!({
            "id": 3,
            "operation": operation(&status),
            "command": {"type": "profile_select", "profile_id": "p1"},
        })).await;
        let selected = conn.read_json().await;
        assert_eq!(selected["type"], "result", "select p1: {selected}");
        let p1_status = selected["value"]["status"].clone();
        conn.write_json(&json!({
            "id": 4,
            "command": {"type": "request_takeover", "epoch": p1_status["control"]["epoch"]},
        })).await;
        let takeover = conn.read_json().await;
        assert_eq!(takeover["type"], "result", "remote takeover after rebind: {takeover}");
        let human_status = takeover["value"].clone();
        conn.write_json(&json!({
            "id": 5,
            "operation": operation(&human_status),
            "command": {"type": "navigate", "url": "data:text/html,<script>document.title=localStorage.getItem('k')||'empty'</script>"},
        })).await;
        let observed = conn.read_json().await;
        assert_eq!(observed["type"], "result", "p1 navigation: {observed}");
        assert_eq!(observed["value"]["navigation"]["title"], "empty", "p1 inherited default storage: {observed}");
    }).await.expect("rp05 timed out");
}

// RP-06/RP-07 categories: legacy access migration and independent observers.

#[tokio::test]
async fn rp_migration_legacy_catalog_without_access_admits_remote_binding() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        // A legacy entry has no Host-side access record. It must admit a remote
        // binding like any other entry.
        let fixture = HostFixture::start("rpmig", catalog("default", 2, vec![
            profile_entry("default", "Default", true, None),
        ])).await;
        let (mut conn, result, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut conn).await;
        assert_eq!(result["type"], "ok");
        // The legacy entry is equally readable over the local Ready-first socket.
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        local.write_json(&json!({"id": 1, "command": {"type": "profile_catalog"}})).await;
        let response = local.read_json().await;
        assert_eq!(response["type"], "result", "local catalog: {response}");
        assert_eq!(response["value"]["catalog"]["profiles"][0]["profile_id"], "default");
    }).await.expect("rp migration timed out");
}

#[tokio::test]
async fn rp07_other_observer_keeps_its_own_session_after_peer_select() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let device_b = DeviceSpec::new();
        let fixture = HostFixture::start("rp07", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a", "dev-b"])),
        ])).await;
        let (mut first, first_result, _) = admit_ok(&fixture, &device_a, 1).await;
        let (mut second, second_result, _) = admit_ok(&fixture, &device_b, 2).await;
        let first_status = attach_observe(&mut first).await;
        attach_observe(&mut second).await;
        // One peer rebinds to another profile.
        first.write_json(&json!({
            "id": 3,
            "operation": operation(&first_status),
            "command": {"type": "profile_select", "profile_id": "p1"},
        })).await;
        let selected = first.read_json().await;
        assert_eq!(selected["type"], "result", "select p1: {selected}");
        assert_eq!(selected["value"]["binding"]["profile_id"], "p1");
        // The other observer keeps its own default route, session and profile.
        second.write_json(&json!({"id": 4, "command": {"type": "status"}})).await;
        let status = second.read_json().await;
        assert_eq!(status["type"], "result", "observer status: {status}");
        assert_eq!(status["value"]["session_id"], second_result["session_id"], "peer select detached another observer");
        assert_eq!(status["value"]["profile_id"], "default");
        assert_ne!(status["value"]["session_id"], selected["value"]["binding"]["session_id"]);
        // Both peers admitted the same default profile before the rebind.
        assert_eq!(first_result["session_id"], second_result["session_id"]);
    }).await.expect("rp07 timed out");
}

#[tokio::test]
async fn rp13_two_peers_observe_the_same_session() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let device_b = DeviceSpec::new();
        let fixture = HostFixture::start("rp13", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
        ])).await;
        let (mut first, first_result, _) = admit_ok(&fixture, &device_a, 1).await;
        let (mut second, second_result, _) = admit_ok(&fixture, &device_b, 2).await;
        assert_eq!(first_result["session_id"], second_result["session_id"], "both peers must bind the same profile session");
        attach_observe(&mut first).await;
        attach_observe(&mut second).await;
    }).await.expect("rp13 timed out");
}

#[tokio::test]
async fn rp17_disconnect_releases_reservation_without_touching_other_routes() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp17", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (reserved, _, _) = admit_ok(&fixture, &device_a, 1).await;
        drop(reserved); // EOF after Ok, before Attach.
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready", "local Ready-first path must be unaffected");
        let (mut second, _, _) = admit_ok(&fixture, &device_a, 2).await;
        attach_observe(&mut second).await;
    }).await.expect("rp17 timed out");
}

#[tokio::test]
async fn rp18_rejected_attach_reclaims_only_its_own_route() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp18", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        conn.write_json(&json!({"id": 2, "command": {"type": "attach", "mode": "observe",
            "viewport": {"device": "phone", "css_width": 0, "css_height": 0, "orientation": "portrait"}}})).await;
        let rejected = conn.read_json().await;
        assert_eq!(rejected["type"], "error", "invalid viewport must be refused: {rejected}");
        assert_eq!(rejected["code"], "INVALID_VIEWPORT");
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 3).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp18 timed out");
}

#[tokio::test]
async fn rp19_local_host_socket_stays_ready_first() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp19", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut local = fixture.local().await;
        let ready = local.read_json().await;
        assert_eq!(ready["type"], "ready", "local host.sock must publish Ready before any request");
        local.write_json(&json!({"id": 1, "command": {"type": "profile_catalog"}})).await;
        let response = local.read_json().await;
        assert_eq!(response["type"], "result", "local catalog: {response}");
        // The local socket still owns the maintenance drain.
        local.write_json(&json!({"id": 2, "command": {"type": "flush_profiles"}})).await;
        let flushed = local.read_json().await;
        assert_eq!(flushed["type"], "result", "local flush: {flushed}");
        assert_eq!(flushed["value"]["flushed"], true);
    }).await.expect("rp19 timed out");
}

#[tokio::test]
async fn rp21_concurrent_remote_admissions_keep_distinct_routes() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let device_b = DeviceSpec::new();
        let fixture = HostFixture::start("rp21", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-b", &["dev-b"])),
        ])).await;
        // Every peer is admitted to the catalog default session.
        let (mut first, first_result, _) = admit_ok(&fixture, &device_a, 1).await;
        let (mut second, second_result, _) = admit_ok(&fixture, &device_b, 2).await;
        assert_eq!(first_result["session_id"], second_result["session_id"], "both peers must be admitted to the catalog default");
        let first_status = attach_observe(&mut first).await;
        attach_observe(&mut second).await;
        // One peer selects the other profile. The two routes must stay distinct.
        let selected = select_profile(&mut first, &first_status, "p1").await;
        assert_eq!(selected["binding"]["profile_id"], "p1");
        assert_ne!(selected["binding"]["session_id"], second_result["session_id"], "an explicit selection must not share the peer's session");
        assert_eq!(selected["status"]["session_id"], selected["binding"]["session_id"]);
    }).await.expect("rp21 timed out");
}

#[tokio::test]
async fn rp22_interleaved_local_accept_keeps_routes_separate() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp22", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        let (mut remote, _, _) = admit_ok(&fixture, &device_a, 1).await;
        local.write_json(&json!({"id": 1, "command": {"type": "attach", "mode": "observe"}})).await;
        assert_eq!(local.read_json().await["type"], "result");
        attach_observe(&mut remote).await;
    }).await.expect("rp22 timed out");
}

#[tokio::test]
async fn rp23_repeated_admission_frame_is_invalid_request() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp23", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (mut conn, result, _) = admit_ok(&fixture, &device_a, 1).await;
        // A second admission-shaped frame is not a browser Request.
        conn.write_json(&device_a.admission(2)).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "error", "second admission frame must be rejected: {response}");
        assert_eq!(response["code"], "INVALID_REQUEST");
        // The original route is unchanged and still accepts the browser loop.
        attach_observe(&mut conn).await;
        assert_eq!(result["type"], "ok");
    }).await.expect("rp23 timed out");
}

#[tokio::test]
async fn rp25_eof_before_handoff_writes_no_result() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp25", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut empty = fixture.admission().await;
        empty.reader.get_mut().shutdown().await.unwrap();
        empty.expect_eof().await;
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp25 timed out");
}

#[tokio::test]
async fn rp26_eof_after_ok_before_attach_releases_reservation() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp26", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        drop(conn);
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 2).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp26 timed out");
}

#[tokio::test]
async fn rp27_eof_after_ready_releases_connection_route() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp27", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut conn).await;
        drop(conn);
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 2).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp27 timed out");
}

#[tokio::test]
async fn rp30_shutdown_removes_owned_sockets() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let mut fixture = HostFixture::start("rp30", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let (mut remote, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut remote).await;
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        fixture.terminate();
        assert!(!fixture.socket_dir.join("remote-admission.sock").exists(), "remote-admission.sock was not cleaned up");
        assert!(!fixture.socket_dir.join("host.sock").exists(), "host.sock was not cleaned up");
        assert!(!fixture.socket_dir.join("frames.sock").exists(), "frames.sock was not cleaned up");
    }).await.expect("rp30 timed out");
}

#[tokio::test]
async fn rp31_coalesced_admission_and_attach_share_one_reader() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp31", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        let mut coalesced = serde_json::to_string(&device_a.admission(1)).unwrap();
        coalesced.push('\n');
        coalesced.push_str(&serde_json::to_string(&json!({"id": 2, "command": {"type": "attach", "mode": "observe"}})).unwrap());
        coalesced.push('\n');
        conn.write_bytes(coalesced.as_bytes()).await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "ok", "coalesced admission result: {result}");
        let ready = conn.read_json().await;
        assert_eq!(ready["type"], "ready", "coalesced ready: {ready}");
        assert_eq!(ready["session_id"], result["session_id"]);
        let attach = conn.read_json().await;
        assert_eq!(attach["type"], "result", "buffered Attach must be parsed by the same reader: {attach}");
    }).await.expect("rp31 timed out");
}

#[tokio::test]
async fn rp32_fragmented_admission_frame_is_accumulated() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp32", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        let mut frame = serde_json::to_string(&device_a.admission(1)).unwrap();
        frame.push('\n');
        let (head, tail) = frame.split_at(frame.len() / 2);
        conn.write_bytes(head.as_bytes()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        conn.write_bytes(tail.as_bytes()).await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "ok", "fragmented admission result: {result}");
        assert_eq!(conn.read_json().await["type"], "ready");
        attach_observe(&mut conn).await;
    }).await.expect("rp32 timed out");
}

#[tokio::test]
async fn rp33_malformed_newline_json_is_uncorrelated_refusal() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp33", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        conn.write_bytes(b"{not-json}\n").await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "err", "malformed frame result: {result}");
        assert_eq!(result["request_id"], Value::Null, "malformed frames carry no request id: {result}");
        assert_eq!(result["code"], "REMOTE_ADMISSION_MALFORMED");
        conn.expect_eof().await;
    }).await.expect("rp33 timed out");
}

#[tokio::test]
async fn rp34_structurally_invalid_request_is_refused_before_binding() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp34", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        conn.write_json(&json!({"principal": {}, "initial": {}, "request_id": 1})).await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "err", "structurally invalid request: {result}");
        assert_eq!(result["request_id"], Value::Null);
        assert_eq!(result["code"], "REMOTE_ADMISSION_MALFORMED");
        conn.expect_eof().await;
    }).await.expect("rp34 timed out");
}

#[tokio::test]
async fn rp35_oversized_frame_without_delimiter_is_refused_at_the_bound() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp35", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        let oversized = vec![b'x'; 64 * 1024 + 1];
        conn.write_bytes(&oversized).await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "err", "oversized frame result: {result}");
        assert_eq!(result["request_id"], Value::Null);
        assert_eq!(result["code"], "REMOTE_ADMISSION_MALFORMED");
        conn.expect_eof().await;
    }).await.expect("rp35 timed out");
}

#[tokio::test]
async fn rp36_partial_frame_at_eof_is_not_empty_eof() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp36", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        conn.write_bytes(b"{\"principal\":").await;
        conn.reader.get_mut().shutdown().await.unwrap();
        let result = conn.read_json().await;
        assert_eq!(result["type"], "err", "partial frame must produce a typed refusal: {result}");
        assert_eq!(result["request_id"], Value::Null);
        assert_eq!(result["code"], "REMOTE_ADMISSION_MALFORMED");
    }).await.expect("rp36 timed out");
}

#[tokio::test]
async fn rp37_empty_eof_writes_no_result() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp37", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        conn.reader.get_mut().shutdown().await.unwrap();
        conn.expect_eof().await;
    }).await.expect("rp37 timed out");
}

#[tokio::test]
async fn rp40_failing_stream_does_not_touch_healthy_routes() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new();
        let fixture = HostFixture::start("rp40", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        let mut malformed = fixture.admission().await;
        malformed.write_bytes(b"{not-json}\n").await;
        assert_eq!(malformed.read_json().await["code"], "REMOTE_ADMISSION_MALFORMED");
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut healthy).await;
        local.write_json(&json!({"id": 1, "command": {"type": "attach", "mode": "observe"}})).await;
        assert_eq!(local.read_json().await["type"], "result");
    }).await.expect("rp40 timed out");
}

#[tokio::test]
async fn rp41_newline_at_the_consumed_bound_is_oversized() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("rp41", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let mut conn = fixture.admission().await;
        let mut frame = vec![b'x'; 64 * 1024];
        frame.push(b'\n');
        conn.write_bytes(&frame).await;
        let result = conn.read_json().await;
        assert_eq!(result["type"], "err", "newline at MAX+1 must be oversized: {result}");
        assert_eq!(result["request_id"], Value::Null);
        assert_eq!(result["code"], "REMOTE_ADMISSION_MALFORMED");
        conn.expect_eof().await;
    }).await.expect("rp41 timed out");
}

/// BB13: the Host owns the one token comparison. A wrong token is refused with
/// `REMOTE_TOKEN_INVALID`, receives no session and creates no session, while the
/// configured token still admits the catalog default session.
#[tokio::test]
async fn bb13_wrong_token_is_refused_without_creating_a_session() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let fixture = HostFixture::start("bb13", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ])).await;
        let sessions_before = fixture.session_sockets();
        assert_eq!(sessions_before, 1, "the catalog default session is the only session at startup");

        let mut denied = fixture.admission().await;
        denied.write_json(&DeviceSpec::wrong_token().admission(1)).await;
        let refusal = denied.read_json().await;
        assert_eq!(refusal["type"], "err", "a wrong token must be refused: {refusal}");
        assert_eq!(refusal["code"], "REMOTE_TOKEN_INVALID");
        assert_eq!(refusal["request_id"], 1);
        assert!(refusal.get("session_id").is_none(), "a refusal must not carry a session: {refusal}");
        assert!(refusal.get("reservation_id").is_none(), "a refusal must not carry a reservation: {refusal}");
        denied.expect_eof().await;
        assert_eq!(fixture.session_sockets(), sessions_before, "a refused admission must not create a session");

        // The configured token still admits the catalog default session.
        let (_, admitted, ready) = admit_ok(&fixture, &DeviceSpec::new(), 2).await;
        assert_eq!(admitted["session_id"], ready["session_id"]);
        assert_eq!(fixture.session_sockets(), sessions_before, "admitting the default session must not create a second one");
    }).await.expect("bb13 timed out");
}

// ---------------------------------------------------------------------------
// RP-38 / RP-42 / RP-43 / RP-44 endpoint consumer rules. The real endpoint
// binary is driven against a controlled Host admission socket so the crafted
// result is the only input the endpoint can consume.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ControlOutcome {
    Message(Value),
    Closed,
    Failed,
    TimedOut,
}

fn forwarded_ready(outcome: &ControlOutcome) -> bool {
    matches!(outcome, ControlOutcome::Message(value) if value["type"] == "ready")
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
}

async fn wait_path(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "path did not appear: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn endpoint_control_outcome(name: &str, reply: Option<Value>) -> ControlOutcome {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp_root(name);
    let host_dir = root.join("host");
    std::fs::create_dir_all(&host_dir).unwrap();

    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura admission endpoint test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap()
        .signed_by(&server_key, &ca, &ca_key).unwrap();
    std::fs::write(root.join("server.der"), server.der()).unwrap();
    std::fs::write(root.join("server.key"), server_key.serialize_der()).unwrap();
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(root.join("ca.der"), ca.der()).unwrap();

    let admission_listener = tokio::net::UnixListener::bind(host_dir.join("remote-admission.sock")).unwrap();
    let fake = tokio::spawn(async move {
        let Ok((socket, _)) = admission_listener.accept().await else { return; };
        let mut reader = BufReader::new(socket);
        let mut request = Vec::new();
        let _ = reader.read_until(b'\n', &mut request).await;
        if let Some(reply) = reply {
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            let _ = reader.get_mut().write_all(&bytes).await;
            let _ = reader.get_mut().flush().await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let frames_listener = tokio::net::UnixListener::bind(host_dir.join("frames.sock")).unwrap();
    let frames = tokio::spawn(async move {
        if let Ok((socket, _)) = frames_listener.accept().await {
            let _held = socket;
            std::future::pending::<()>().await;
        }
    });

    let media_bin = release_binary("obscura-media");
    assert!(media_bin.is_file(), "Build target/release/obscura-media before endpoint acceptance");
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut endpoint = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string())
        .arg("--host-dir").arg(&host_dir)
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der"))
        .arg("--server-key").arg(root.join("server.key"))
        .arg("--media-bin").arg(&media_bin)
        .stdout(Stdio::null()).stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_path(&root.join("endpoint")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(ca.der().to_vec())).unwrap();
    let client_config = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut request = format!("wss://localhost:{}/control", address.port()).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = client_async(request, tls).await.unwrap();
    let outcome = match tokio::time::timeout(Duration::from_secs(30), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => ControlOutcome::Message(serde_json::from_str(&text).unwrap()),
        Ok(Some(Ok(_))) => ControlOutcome::Failed,
        Ok(Some(Err(_))) => ControlOutcome::Failed,
        Ok(None) => ControlOutcome::Closed,
        Err(_) => ControlOutcome::TimedOut,
    };
    drop(ws);
    fake.abort();
    frames.abort();
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    outcome
}

#[tokio::test]
async fn rp38_endpoint_surfaces_missing_result_without_forwarding_ready() {
    tokio::time::timeout(Duration::from_secs(120), async {
        // The controlled Host reads the admission request and closes without a
        // result. The endpoint must observe the transport closure and forward
        // no Ready.
        let outcome = endpoint_control_outcome("rp38", None).await;
        assert!(!forwarded_ready(&outcome), "endpoint fabricated Ready after a missing result: {outcome:?}");
    }).await.expect("rp38 timed out");
}

#[tokio::test]
async fn rp42_endpoint_rejects_mismatched_request_id() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let outcome = endpoint_control_outcome("rp42", Some(json!({
            "type": "err",
            "request_id": 999,
            "code": "REMOTE_SESSION_UNAVAILABLE",
            "message": "mismatched correlation",
        }))).await;
        assert!(!forwarded_ready(&outcome), "endpoint forwarded Ready for a mismatched request id: {outcome:?}");
        assert!(
            !matches!(&outcome, ControlOutcome::Message(value) if value["code"] == "REMOTE_SESSION_UNAVAILABLE"),
            "endpoint must not relay a mismatched Some(id) refusal: {outcome:?}"
        );
    }).await.expect("rp42 timed out");
}

#[tokio::test]
async fn rp43_endpoint_rejects_illegal_none_code() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let outcome = endpoint_control_outcome("rp43", Some(json!({
            "type": "err",
            "request_id": null,
            "code": "REMOTE_SESSION_UNAVAILABLE",
            "message": "illegal uncorrelated refusal",
        }))).await;
        assert!(!forwarded_ready(&outcome), "endpoint forwarded Ready for an illegal None code: {outcome:?}");
        assert!(
            !matches!(&outcome, ControlOutcome::Message(value) if value["code"] == "REMOTE_SESSION_UNAVAILABLE"),
            "None must never become an EOF or another wire terminal: {outcome:?}"
        );
    }).await.expect("rp43 timed out");
}

#[tokio::test]
async fn rp44_endpoint_accepts_sole_outstanding_none_malformed() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let outcome = endpoint_control_outcome("rp44", Some(json!({
            "type": "err",
            "request_id": null,
            "code": "REMOTE_ADMISSION_MALFORMED",
            "message": "partial admission frame",
        }))).await;
        assert!(!forwarded_ready(&outcome), "endpoint forwarded Ready for a malformed admission: {outcome:?}");
        match &outcome {
            ControlOutcome::Message(value) => {
                assert_eq!(value["type"], "error", "legitimate None/MALFORMED must surface as a typed error: {value}");
                assert_eq!(value["code"], "REMOTE_ADMISSION_MALFORMED");
            }
            other => panic!("endpoint did not forward the sole-outstanding refusal: {other:?}"),
        }
    }).await.expect("rp44 timed out");
}
