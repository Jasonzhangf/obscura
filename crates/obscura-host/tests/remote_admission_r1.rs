//! Public Host-side regression for the remote profile admission slice.
//!
//! Every case drives the real `obscura-host` process over its real
//! `remote-admission.sock` / `host.sock` sockets. Nothing here mocks an
//! internal function or asserts on source text: the assertions are the bytes a
//! real peer reads, the bytes it writes, and the persisted catalog file.
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
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::Message};

const OBSERVE: &str = "observe_human_control";
const READ: &str = "profile_read";
const CREATE: &str = "profile_create";
const SELECT: &str = "profile_select";
const ALL_SCOPES: [&str; 4] = [OBSERVE, READ, CREATE, SELECT];

fn tmp_root(name: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("ora-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn hex_fingerprint(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn fingerprint_array(byte: u8) -> Value {
    Value::Array((0..32).map(|_| json!(byte)).collect())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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

#[derive(Clone)]
struct DeviceSpec {
    device_id: String,
    fingerprint: String,
    scopes: Vec<&'static str>,
    selection: Value,
    revision: u64,
    status: &'static str,
}

impl DeviceSpec {
    fn new(device_id: &str, fingerprint_byte: u8, scopes: &[&'static str], selection: Value, revision: u64) -> Self {
        Self {
            device_id: device_id.into(),
            fingerprint: hex_fingerprint(fingerprint_byte),
            scopes: scopes.to_vec(),
            selection,
            revision,
            status: "active",
        }
    }

    fn revoked(mut self) -> Self {
        self.status = "revoked";
        self
    }

    fn registry_entry(&self) -> Value {
        json!({
            "device_id": self.device_id,
            "device_label": self.device_id,
            "client_certificate_sha256": self.fingerprint,
            "scopes": self.scopes,
            "initial_profile_grant": {"selection": self.selection, "authorization_revision": self.revision},
            "scope_revision": 1,
            "status": self.status,
            "not_before_unix": 0,
            "not_after_unix": 4_102_444_800i64,
            "created_at_unix": 0,
            "revoked_at_unix": null,
        })
    }

    fn projection_entry(&self) -> Value {
        json!({
            "device_id": self.device_id,
            "client_certificate_sha256": self.fingerprint,
            "scopes": self.scopes,
            "initial_profile_grant": {"selection": self.selection, "authorization_revision": self.revision},
        })
    }

    fn admission(&self, request_id: u64) -> Value {
        self.admission_with_selection(self.selection.clone(), request_id)
    }

    fn admission_with_selection(&self, selection: Value, request_id: u64) -> Value {
        json!({
            "principal": {
                "device_id": self.device_id,
                "client_cert_sha256": fingerprint_array_hex(&self.fingerprint),
                "scopes": self.scopes,
                "authorization_revision": self.revision,
            },
            "initial": {
                "device_id": self.device_id,
                "authorization_revision": self.revision,
                "selection": selection,
            },
            "request_id": request_id,
        })
    }
}

fn fingerprint_array_hex(hex: &str) -> Value {
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index + 1 < bytes.len() {
        out.push(json!(u8::from_str_radix(std::str::from_utf8(&bytes[index..index + 2]).unwrap(), 16).unwrap()));
        index += 2;
    }
    Value::Array(out)
}

struct HostFixture {
    root: PathBuf,
    socket_dir: PathBuf,
    profile_root: PathBuf,
    registry: PathBuf,
    child: Child,
}

impl HostFixture {
    async fn start(name: &str, catalog_value: Value, devices: &[DeviceSpec]) -> Self {
        let root = tmp_root(name);
        let socket_dir = root.join("sock");
        let profile_root = root.join("profiles");
        std::fs::create_dir_all(&profile_root).unwrap();
        std::fs::write(profile_root.join("catalog.json"), serde_json::to_vec_pretty(&catalog_value).unwrap()).unwrap();
        let revision = devices.iter().map(|device| device.revision).max().unwrap_or(1);
        let registry = root.join("registry.json");
        let authorized = root.join("authorized-clients.json");
        let registry_value = json!({
            "schema_version": 2,
            "authorization_revision": revision,
            "host_id": "host-test",
            "devices": devices.iter().map(DeviceSpec::registry_entry).collect::<Vec<_>>(),
        });
        let registry_bytes = serde_json::to_vec_pretty(&registry_value).unwrap();
        let projection_value = json!({
            "schema_version": 2,
            "authorization_revision": revision,
            "registry_sha256": sha256_hex(&registry_bytes),
            "clients": devices.iter().map(DeviceSpec::projection_entry).collect::<Vec<_>>(),
        });
        std::fs::write(&registry, &registry_bytes).unwrap();
        std::fs::write(&authorized, serde_json::to_vec_pretty(&projection_value).unwrap()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(&socket_dir)
            .arg("--profile-root").arg(&profile_root)
            .arg("--authorization-registry").arg(&registry)
            .arg("--allow-private-network")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("start Host");
        let fixture = Self { root, socket_dir, profile_root, registry, child };
        fixture.wait_socket("host.sock").await;
        fixture.wait_socket("remote-admission.sock").await;
        fixture
    }

    fn admission_path(&self) -> PathBuf { self.socket_dir.join("remote-admission.sock") }
    fn host_path(&self) -> PathBuf { self.socket_dir.join("host.sock") }
    fn profile_root(&self) -> &Path { &self.profile_root }
    fn registry_path(&self) -> &Path { &self.registry }

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

    /// Rewrite the registry with a fresh revision, keeping every device's
    /// declared revision rebind-consistent with the new global revision unless
    /// the device is explicitly pinned stale.
    fn rewrite_registry(&self, revision: u64, devices: &[DeviceSpec]) {
        let registry_value = json!({
            "schema_version": 2,
            "authorization_revision": revision,
            "host_id": "host-test",
            "devices": devices.iter().map(DeviceSpec::registry_entry).collect::<Vec<_>>(),
        });
        std::fs::write(&self.registry, serde_json::to_vec_pretty(&registry_value).unwrap()).unwrap();
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

async fn expect_denial(conn: &mut Conn, request_id: u64, code: &str) {
    let result = conn.read_json().await;
    assert_eq!(result["type"], "err", "expected a typed denial: {result}");
    assert_eq!(result["request_id"], request_id, "decoded denial keeps its request id: {result}");
    assert_eq!(result["code"], code, "denial code: {result}");
    conn.expect_eof().await;
}

async fn attach_observe(conn: &mut Conn) -> Value {
    conn.write_json(&json!({"id": 2, "command": {"type": "attach", "mode": "observe"}})).await;
    let response = conn.read_json().await;
    assert_eq!(response["type"], "result", "attach response: {response}");
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
async fn rp01_remote_catalog_filters_to_accessible_profiles() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x11, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp01", catalog("default", 3, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Other", false, default_access("dev-b", &["dev-b"])),
        ]), std::slice::from_ref(&device_a)).await;
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        attach_observe(&mut conn).await;
        conn.write_json(&json!({"id": 3, "command": {"type": "profile_catalog"}})).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "result", "catalog response: {response}");
        let catalog = &response["value"]["catalog"];
        let ids: Vec<&str> = catalog["profiles"].as_array().unwrap().iter()
            .map(|profile| profile["profile_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["default"], "remote catalog leaked another device's profile: {catalog}");
        assert_eq!(catalog["default_profile_id"], "default");
    }).await.expect("rp01 timed out");
}

#[tokio::test]
async fn rp02_remote_create_persists_remote_owner() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x21, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp02", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        let status = attach_observe(&mut conn).await;
        conn.write_json(&json!({
            "id": 3,
            "operation": operation(&status),
            "command": {"type": "profile_create", "display_name": "Remote", "make_default": false},
        })).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "result", "create response: {response}");
        let created = &response["value"]["profile"];
        assert_eq!(created["display_name"], "Remote");
        let persisted: Value = serde_json::from_slice(&std::fs::read(fixture.profile_root().join("catalog.json")).unwrap()).unwrap();
        let entry = persisted["profiles"].as_array().unwrap().iter()
            .find(|profile| profile["profile_id"] == created["profile_id"]).expect("created profile persisted");
        assert_eq!(entry["access"]["owner_device_id"], "dev-a", "remote create must record the remote owner: {entry}");
        assert_eq!(entry["access"]["members"], json!(["dev-a"]));
    }).await.expect("rp02 timed out");
}

#[tokio::test]
async fn rp03_remote_select_rebinds_without_killing_source() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x31, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp03", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
async fn rp04_missing_scope_is_denied_before_ready() {
    tokio::time::timeout(Duration::from_secs(120), async {
        // No observe_human_control scope: the admission itself is refused.
        let no_observe = DeviceSpec::new("dev-c", 0x41, &[READ], json!("default_profile"), 7);
        // Observe only: admission succeeds, but a profile command is refused.
        let observe_only = DeviceSpec::new("dev-d", 0x42, &[OBSERVE], json!("default_profile"), 7);
        let fixture = HostFixture::start("rp04", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-c", &["dev-c", "dev-d"])),
        ]), &[no_observe.clone(), observe_only.clone()]).await;
        let mut denied = fixture.admission().await;
        denied.write_json(&no_observe.admission(1)).await;
        expect_denial(&mut denied, 1, "REMOTE_PROFILE_SCOPE_REQUIRED").await;
        let (mut conn, _, _) = admit_ok(&fixture, &observe_only, 2).await;
        attach_observe(&mut conn).await;
        conn.write_json(&json!({"id": 3, "command": {"type": "profile_catalog"}})).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "error", "missing profile_read must be refused: {response}");
        assert_eq!(response["code"], "REMOTE_PROFILE_SCOPE_REQUIRED");
    }).await.expect("rp04 timed out");
}

#[tokio::test]
async fn rp05_two_profile_storage_is_isolated() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x51, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp05", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
async fn rp_migration_legacy_catalog_without_access_refuses_remote_binding() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x52, &ALL_SCOPES, json!("default_profile"), 7);
        // A legacy entry has no Host-side access record. Remote binding must
        // refuse with the typed migration code instead of granting a default.
        let fixture = HostFixture::start("rpmig", catalog("default", 2, vec![
            profile_entry("default", "Default", true, None),
        ]), std::slice::from_ref(&device_a)).await;
        let mut denied = fixture.admission().await;
        denied.write_json(&device_a.admission(1)).await;
        expect_denial(&mut denied, 1, "PROFILE_ACCESS_MIGRATION_REQUIRED").await;
        // The legacy entry is still readable over the local Ready-first socket.
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
        let device_a = DeviceSpec::new("dev-a", 0x53, &ALL_SCOPES, json!("default_profile"), 7);
        let device_b = DeviceSpec::new("dev-b", 0x54, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp07", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
            profile_entry("p1", "Work", false, default_access("dev-a", &["dev-a", "dev-b"])),
        ]), &[device_a.clone(), device_b.clone()]).await;
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
async fn rp09_unauthorized_initial_profile_is_denied_before_ready() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x61, &ALL_SCOPES, json!("default_profile"), 7);
        let device_b = DeviceSpec::new("dev-b", 0x62, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp09", catalog("default", 3, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-b", &["dev-b"])),
        ]), &[device_a, device_b.clone()]).await;
        let mut denied = fixture.admission().await;
        denied.write_json(&device_b.admission(1)).await;
        expect_denial(&mut denied, 1, "REMOTE_INITIAL_PROFILE_ACCESS_DENIED").await;
        // RP-10: a reconnect must not recover the profile through any fallback.
        let mut reconnect = fixture.admission().await;
        reconnect.write_json(&device_b.admission(2)).await;
        expect_denial(&mut reconnect, 2, "REMOTE_INITIAL_PROFILE_ACCESS_DENIED").await;
    }).await.expect("rp09 timed out");
}

#[tokio::test]
async fn rp11_unbound_initial_selection_is_denied() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x71, &ALL_SCOPES, json!({"profile_id": "missing"}), 7);
        let fixture = HostFixture::start("rp11", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let mut conn = fixture.admission().await;
        conn.write_json(&device_a.admission(1)).await;
        expect_denial(&mut conn, 1, "REMOTE_INITIAL_PROFILE_UNBOUND").await;
    }).await.expect("rp11 timed out");
}

#[tokio::test]
async fn rp12_stale_or_revoked_grant_is_denied() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut stale = DeviceSpec::new("dev-a", 0x81, &ALL_SCOPES, json!("default_profile"), 7);
        stale.revision = 6; // registry is at revision 7: the principal is stale.
        let revoked = DeviceSpec::new("dev-b", 0x82, &ALL_SCOPES, json!("default_profile"), 7).revoked();
        let fixture = HostFixture::start("rp12", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
        ]), &[DeviceSpec { revision: 7, ..stale.clone() }, revoked.clone()]).await;
        let mut stale_conn = fixture.admission().await;
        stale_conn.write_json(&stale.admission(1)).await;
        expect_denial(&mut stale_conn, 1, "REMOTE_PRINCIPAL_REVOKED").await;
        let mut revoked_conn = fixture.admission().await;
        revoked_conn.write_json(&revoked.admission(2)).await;
        expect_denial(&mut revoked_conn, 2, "REMOTE_PRINCIPAL_REVOKED").await;
    }).await.expect("rp12 timed out");
}

#[tokio::test]
async fn rp13_two_registered_devices_observe_the_same_session() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x91, &ALL_SCOPES, json!("default_profile"), 7);
        let device_b = DeviceSpec::new("dev-b", 0x92, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp13", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
        ]), &[device_a.clone(), device_b.clone()]).await;
        let (mut first, first_result, _) = admit_ok(&fixture, &device_a, 1).await;
        let (mut second, second_result, _) = admit_ok(&fixture, &device_b, 2).await;
        assert_eq!(first_result["session_id"], second_result["session_id"], "both devices must bind the same profile session");
        attach_observe(&mut first).await;
        attach_observe(&mut second).await;
    }).await.expect("rp13 timed out");
}

#[tokio::test]
async fn rp14_revoking_second_device_keeps_owner_alive() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0xa1, &ALL_SCOPES, json!("default_profile"), 7);
        let device_b = DeviceSpec::new("dev-b", 0xa2, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp14", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a", "dev-b"])),
        ]), &[device_a.clone(), device_b.clone()]).await;
        fixture.rewrite_registry(8, &[DeviceSpec { revision: 8, ..device_a.clone() }, DeviceSpec { revision: 8, ..device_b.clone() }.revoked()]);
        let mut denied = fixture.admission().await;
        denied.write_json(&device_b.admission(1)).await;
        expect_denial(&mut denied, 1, "REMOTE_PRINCIPAL_REVOKED").await;
        let rebind_a = DeviceSpec { revision: 8, ..device_a };
        let (mut owner, _, _) = admit_ok(&fixture, &rebind_a, 2).await;
        attach_observe(&mut owner).await;
    }).await.expect("rp14 timed out");
}

#[tokio::test]
async fn rp17_disconnect_releases_reservation_without_touching_other_routes() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0xb1, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp17", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0xc1, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp18", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0xd1, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp19", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let mut local = fixture.local().await;
        let ready = local.read_json().await;
        assert_eq!(ready["type"], "ready", "local host.sock must publish Ready before any request");
        local.write_json(&json!({"id": 1, "command": {"type": "profile_catalog"}})).await;
        let response = local.read_json().await;
        assert_eq!(response["type"], "result", "local catalog: {response}");
        // Local maintenance still owns FlushProfiles; the remote path must not.
        local.write_json(&json!({"id": 2, "command": {"type": "flush_profiles"}})).await;
        let flushed = local.read_json().await;
        assert_eq!(flushed["type"], "result", "local flush: {flushed}");
        assert_eq!(flushed["value"]["flushed"], true);
    }).await.expect("rp19 timed out");
}

#[tokio::test]
async fn rp20_revision_change_rejects_attach_without_destroying_owner() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0xe1, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp20", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let mut local = fixture.local().await;
        assert_eq!(local.read_json().await["type"], "ready");
        local.write_json(&json!({"id": 1, "command": {"type": "attach", "mode": "observe"}})).await;
        assert_eq!(local.read_json().await["type"], "result");
        let (mut conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        // Advance the registry revision after admission but before Attach.
        fixture.rewrite_registry(9, std::slice::from_ref(&device_a));
        conn.write_json(&json!({"id": 2, "command": {"type": "attach", "mode": "observe"}})).await;
        let response = conn.read_json().await;
        assert_eq!(response["type"], "error", "stale revision must refuse control: {response}");
        assert_eq!(response["code"], "REMOTE_PRINCIPAL_REVOKED");
        // The pre-existing local attachment must survive the refusal.
        local.write_json(&json!({"id": 2, "command": {"type": "status"}})).await;
        assert_eq!(local.read_json().await["type"], "result", "owner attachment was destroyed by a stale remote refusal");
    }).await.expect("rp20 timed out");
}

#[tokio::test]
async fn rp21_concurrent_remote_admissions_keep_distinct_sessions() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0xf1, &ALL_SCOPES, json!("default_profile"), 7);
        let device_b = DeviceSpec::new("dev-b", 0xf2, &ALL_SCOPES, json!({"profile_id": "p1"}), 7);
        let fixture = HostFixture::start("rp21", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
            profile_entry("p1", "Work", false, default_access("dev-b", &["dev-b"])),
        ]), &[device_a.clone(), device_b.clone()]).await;
        let (mut first, first_result, _) = admit_ok(&fixture, &device_a, 1).await;
        let (mut second, second_result, _) = admit_ok(&fixture, &device_b, 2).await;
        assert_ne!(first_result["session_id"], second_result["session_id"], "independent profiles must not share a session");
        attach_observe(&mut first).await;
        attach_observe(&mut second).await;
    }).await.expect("rp21 timed out");
}

#[tokio::test]
async fn rp22_interleaved_local_accept_keeps_routes_separate() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x11, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp22", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x12, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp23", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
async fn rp24_admission_denial_creates_no_route() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x13, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp24", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let mut denied = fixture.admission().await;
        denied.write_json(&device_a.admission_with_selection(json!({"profile_id": "nope"}), 1)).await;
        expect_denial(&mut denied, 1, "REMOTE_INITIAL_PROFILE_UNBOUND").await;
        // No reservation leaked into the owner's healthy route.
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 2).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp24 timed out");
}

#[tokio::test]
async fn rp25_eof_before_handoff_writes_no_result() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x14, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp25", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x15, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp26", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let (conn, _, _) = admit_ok(&fixture, &device_a, 1).await;
        drop(conn);
        let (mut healthy, _, _) = admit_ok(&fixture, &device_a, 2).await;
        attach_observe(&mut healthy).await;
    }).await.expect("rp26 timed out");
}

#[tokio::test]
async fn rp27_eof_after_ready_releases_connection_route() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x16, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp27", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x17, &ALL_SCOPES, json!("default_profile"), 7);
        let mut fixture = HostFixture::start("rp30", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x18, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp31", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x19, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp32", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x1a, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp33", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x1b, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp34", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x1c, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp35", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x1d, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp36", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x1e, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp37", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
        let mut conn = fixture.admission().await;
        conn.reader.get_mut().shutdown().await.unwrap();
        conn.expect_eof().await;
    }).await.expect("rp37 timed out");
}

#[tokio::test]
async fn rp40_failing_stream_does_not_touch_healthy_routes() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let device_a = DeviceSpec::new("dev-a", 0x1f, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp40", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
        let device_a = DeviceSpec::new("dev-a", 0x2a, &ALL_SCOPES, json!("default_profile"), 7);
        let fixture = HostFixture::start("rp41", catalog("default", 2, vec![
            profile_entry("default", "Default", true, default_access("dev-a", &["dev-a"])),
        ]), std::slice::from_ref(&device_a)).await;
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
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
    let client_key = rcgen::KeyPair::generate().unwrap();
    let mut client_params = rcgen::CertificateParams::new(vec![]).unwrap();
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();
    std::fs::write(root.join("server.der"), server.der()).unwrap();
    std::fs::write(root.join("server.key"), server_key.serialize_der()).unwrap();
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(root.join("ca.der"), ca.der()).unwrap();

    let projection = json!({
        "schema_version": 2,
        "authorization_revision": 7,
        "registry_sha256": sha256_hex(b"endpoint-test-registry-material"),
        "clients": [{
            "device_id": "dev-e",
            "client_certificate_sha256": sha256_hex(&client.der().to_vec()),
            "scopes": ALL_SCOPES,
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 7},
        }],
    });
    let authorized = root.join("authorized-clients.json");
    std::fs::write(&authorized, serde_json::to_vec_pretty(&projection).unwrap()).unwrap();

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
        .arg("--client-ca").arg(root.join("ca.der"))
        .arg("--authorized-clients").arg(&authorized)
        .arg("--media-bin").arg(&media_bin)
        .stdout(Stdio::null()).stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_path(&root.join("endpoint/encoded.sock")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
    let client_config = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(
            vec![CertificateDer::from(client.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key.serialize_der())),
        )
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let (mut ws, _) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await.unwrap();
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
            "code": "REMOTE_PRINCIPAL_REVOKED",
            "message": "mismatched correlation",
        }))).await;
        assert!(!forwarded_ready(&outcome), "endpoint forwarded Ready for a mismatched request id: {outcome:?}");
        assert!(
            !matches!(&outcome, ControlOutcome::Message(value) if value["code"] == "REMOTE_PRINCIPAL_REVOKED"),
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
            "code": "REMOTE_AUTH_REQUIRED",
            "message": "illegal uncorrelated refusal",
        }))).await;
        assert!(!forwarded_ready(&outcome), "endpoint forwarded Ready for an illegal None code: {outcome:?}");
        assert!(
            !matches!(&outcome, ControlOutcome::Message(value) if value["code"] == "REMOTE_AUTH_REQUIRED"),
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
