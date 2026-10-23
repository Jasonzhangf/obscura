#![cfg(unix)]
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use futures_util::{SinkExt, StreamExt};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn token_endpoint_preserves_host_and_serves_remote_operations() {
    tokio::time::timeout(Duration::from_secs(90), exercise()).await.unwrap();
}

async fn exercise() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // Exact-session media dirs are `socket_dir/m-<12 hex>/encoder-control.sock`
    // and must stay strictly below the macOS SUN_LEN bound, so keep the root short.
    let root = PathBuf::from("/tmp").join(format!("oe-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let bind_ip = std::env::var("OBSCURA_ENDPOINT_BIND_IP").unwrap_or_else(|_| "127.0.0.1".into());
    let bind_ip: std::net::IpAddr = bind_ip.parse().unwrap();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(vec![]).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "Obscura endpoint test CA");
    let ca = ca.self_signed(&ca_key).unwrap();
    let host_key = rcgen::KeyPair::generate().unwrap();
    let host_cert = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.to_string()]).unwrap().signed_by(&host_key, &ca, &ca_key).unwrap();
    std::fs::write(root.join("server.der"), host_cert.der()).unwrap();
    std::fs::write(root.join("key.der"), host_key.serialize_der()).unwrap();
    std::fs::set_permissions(root.join("key.der"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(root.join("ca.der"), ca.der()).unwrap();
    write_catalog(&root);
    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--access-token").arg(ACCESS_TOKEN)
.stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await.unwrap();
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await.unwrap();
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await;
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"navigate","url":"data:text/html,<button id='target' style='width:200px;height:100px' onclick='window.clicked=1'>remote click</button>"}})).await;
    assert_eq!(state["document_revision"], 1);
    // Reserve a loopback port only for this test's explicit listener.
    let reservation = std::net::TcpListener::bind((bind_ip, 0)).unwrap();
    let address = reservation.local_addr().unwrap(); drop(reservation);
    let media = PathBuf::from(env!("CARGO_BIN_EXE_obscura-host")).with_file_name("obscura-media");
    assert!(media.is_file(), "Build obscura-media before endpoint acceptance");
    let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("key.der"))
        .arg("--media-bin").arg(media)
        .stdout(Stdio::null()).stderr(std::fs::File::create(root.join("endpoint.err")).unwrap()).kill_on_drop(true).spawn().unwrap();
    // Exact-session encoded sockets are bound lazily on first session admission,
    // so endpoint readiness is the socket directory created after TCP bind.
    wait_socket(root.join("endpoint")).await;
    let mut roots = rustls::RootCertStore::empty(); roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
    // The wrong token is the only refusal on the connection path: the endpoint
    // answers with the typed REMOTE_TOKEN_INVALID frame and never a Ready.
    let anonymous = rustls::ClientConfig::builder().with_root_certificates(roots.clone()).with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(anonymous)).connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
    let mut wrong = format!("wss://localhost:{}/control", address.port()).into_client_request().unwrap();
    wrong.headers_mut().insert("authorization", "Bearer wrong-token".parse().unwrap());
    let (mut refused, _) = client_async(wrong, tls).await.unwrap();
    let refusal: serde_json::Value = serde_json::from_str(refused.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(refusal["type"], "error", "a wrong token must be refused: {refusal}");
    assert_eq!(refusal["code"], "REMOTE_TOKEN_INVALID");
    let follow_up = refused.next().await;
    // The endpoint closes the refused connection; a Ready must never follow.
    assert!(!matches!(&follow_up, Some(Ok(Message::Text(text))) if text.contains("ready")), "a wrong token must not receive Ready: {follow_up:?}");
    drop(refused);
    let client = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut control_request = format!("wss://localhost:{}/control", address.port()).into_client_request().unwrap();
    control_request.headers_mut().insert("authorization", format!("Bearer {ACCESS_TOKEN}").parse().unwrap());
    let (mut control, response) = client_async(control_request, tls).await.unwrap();
    let token = response.headers()["x-obscura-media-token"].to_str().unwrap().to_string();
    let ready: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(ready["version"], 11);
    if let Ok(serial) = std::env::var("OBSCURA_ENDPOINT_ADB_SERIAL") {
        assert!(!bind_ip.is_loopback(), "Device probe requires an explicit reachable bind IP");
        // Ephemeral test-only credential, never installed into device/system trust.
        std::fs::write(root.join("token.txt"), ACCESS_TOKEN).unwrap();
        device_ready(&serial, &root, address, ready["session_id"].as_str().unwrap()).await;
    }
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"agent"}}"#)).await.unwrap();
    let rejected: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    // The removed remote fence is gone: this is now the session's own rule that
    // only one agent attachment may exist, and the local connection holds it.
    assert_eq!(rejected["code"], "AGENT_BUSY", "unexpected agent attach response: {rejected}");
    control.send(Message::text(r#"{"id":2,"command":{"type":"attach","mode":"observe","viewport":{"device":"phone","css_width":391,"css_height":701,"orientation":"portrait"}}}"#)).await.unwrap();
    let attached: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(attached["value"]["mode"], "observe");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        state = local_request(&mut local, serde_json::json!({"id":8,"command":{"type":"status"}})).await;
        if state["viewport_pending"] == false { break; }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(state["viewport"], serde_json::json!([391.0,701.0]));
    assert_eq!(state["viewport_owner"], attached["value"]["attachment_id"]);
    control.send(Message::text(r#"{"id":9,"command":{"type":"resize","width":100,"height":100}}"#)).await.unwrap();
    let resize: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    // The removed remote fence is gone: the surviving rule is the session's own
    // requirement that a browser mutation carries an operation identity.
    assert_eq!(resize["code"], "OPERATION_REQUIRED", "unexpected resize response: {resize}");
    control.send(Message::text(r#"{"id":20,"command":{"type":"profile_catalog"}}"#)).await.unwrap();
    let catalog: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(catalog["type"], "result", "profile catalog was not forwarded: {catalog}");
    assert_eq!(catalog["value"]["catalog"]["profiles"][0]["profile_id"], "default");
    control.send(Message::text(r#"{"id":21,"command":{"type":"profile_create","display_name":"Remote","make_default":false}}"#)).await.unwrap();
    let created: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(created["type"], "error", "remote profile_create requires an operation identity: {created}");
    assert_eq!(created["code"], "OPERATION_REQUIRED");
    control.send(Message::text(r#"{"id":22,"command":{"type":"profile_select","profile_id":"default"}}"#)).await.unwrap();
    let selected: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(selected["code"], "OPERATION_REQUIRED");
    control.send(Message::text(r#"{"id":23,"command":{"type":"flush_profiles"}}"#)).await.unwrap();
    let flush: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    // The removed remote fence is gone: a remote attachment may flush profiles.
    assert_eq!(flush["type"], "result", "remote flush_profiles must now be served: {flush}");
    assert_eq!(flush["value"]["flushed"], true);
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut invalid = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    invalid.headers_mut().insert("authorization", "Bearer invalid".parse().unwrap());
    assert!(client_async(invalid, tls).await.is_err());
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut video, _) = client_async(request, tls).await.unwrap();
    let packet = loop {
        let message = video.next().await.unwrap().unwrap();
        if let Message::Binary(bytes) = message {
            assert!(bytes.len() > 4);
            let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            let header: serde_json::Value = serde_json::from_slice(&bytes[4..4+length]).unwrap();
            if header["type"] == "access_unit" {
                if header["source"]["viewport_revision"] == state["viewport_revision"] { break bytes; }
                continue;
            }
            assert_eq!(header["type"], "waiting");
        }
    };
    let length = u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&packet[4..4+length]).unwrap();
    assert_eq!(header["type"], "access_unit");
    assert_eq!(header["source"]["session_id"], ready["session_id"]);
    assert_eq!(header["source"]["width"], 391);
    assert_eq!(header["source"]["height"], 701);
    assert_eq!(header["coded_width"], 392);
    assert_eq!(header["coded_height"], 702);
    assert_eq!(header["byte_length"].as_u64().unwrap() as usize, packet.len() - 4 - length);
    control.send(Message::text(r#"{"id":10,"command":{"type":"declare_viewport","viewport":{"device":"phone","css_width":701,"css_height":391,"orientation":"landscape"}}}"#)).await.unwrap();
    let declaration: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(declaration["type"], "result");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        state = local_request(&mut local, serde_json::json!({"id":11,"command":{"type":"status"}})).await;
        if state["viewport_pending"] == false { break; }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(state["viewport"], serde_json::json!([701.0,391.0]));
    assert_eq!(state["viewport_revision"], 2);
    control.send(Message::text(serde_json::json!({"id":3,"command":{"type":"request_takeover","epoch":attached["value"]["control"]["epoch"]}}).to_string())).await.unwrap();
    let takeover: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(takeover["value"]["control"]["phase"]["type"], "human");
    control.send(Message::text(serde_json::json!({"id":4,"operation":identity(&takeover["value"]),"command":{"type":"click","x":30,"y":30}}).to_string())).await.unwrap();
    let clicked: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(clicked["value"]["input"]["state"], "succeeded");
    control.send(Message::text(serde_json::json!({"id":5,"command":{"type":"release_control","epoch":takeover["value"]["control"]["epoch"]}}).to_string())).await.unwrap();
    let released: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(released["value"]["control"]["phase"]["type"], "agent");
    state = local_request(&mut local, serde_json::json!({"id":6,"command":{"type":"status"}})).await;
    let effect = local_request(&mut local, serde_json::json!({"id":7,"operation":identity(&state),"command":{"type":"evaluate","expression":"window.clicked"}})).await;
    assert_eq!(effect["result"]["value"].as_f64(), Some(1.0));
    state = local_request(&mut local, serde_json::json!({"id":8,"command":{"type":"status"}})).await;
    let closed = local_request(&mut local, serde_json::json!({"id":9,"operation":identity(&state),"command":{"type":"close_session"}})).await;
    assert_eq!(closed["closed"], true);
    let closed_header = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = video.next().await.expect("media channel ended before Closed").expect("media read failed");
            let Message::Binary(bytes) = message else { continue; };
            assert!(bytes.len() >= 4, "media frame header is truncated");
            let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert!(bytes.len() >= 4 + length, "media frame payload is truncated");
            let header: serde_json::Value = serde_json::from_slice(&bytes[4..4 + length]).unwrap();
            if header["type"] == "closed" { break header; }
        }
    }).await.expect("media channel did not deliver Closed");
    assert_eq!(closed_header["session_id"], ready["session_id"]);
    drop(control);
    // The r11 per-session endpoint is a persistent service: session close and
    // control EOF finalize only that session. Endpoint shutdown is the designed
    // cooperative SIGTERM path (implementation-contracts.md `finalize_all`;
    // blackbox-plan.md 6.14 terminates the endpoint with SIGTERM).
    std::process::Command::new("kill").args(["-TERM", &endpoint.id().unwrap().to_string()]).status().unwrap();
    endpoint.wait().await.unwrap();
    std::process::Command::new("kill").args(["-TERM", &host.id().unwrap().to_string()]).status().unwrap();
    host.wait().await.unwrap();
    // Only this test's newly created directory and fixtures.
    std::fs::remove_dir_all(root).unwrap();
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
}


/// Minimal endpoint fixtures for the applied-receipt cases: a self-signed CA
/// and one server certificate. The receipt cases never admit a peer, so no Host
/// is required.
struct ReceiptFixtures {
    root: PathBuf,
}

impl ReceiptFixtures {
    fn create() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = PathBuf::from("/tmp").join(format!("oer-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&root).unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura endpoint receipt test CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap()
            .signed_by(&server_key, &ca, &ca_key).unwrap();
        std::fs::write(root.join("server.der"), server.der()).unwrap();
        std::fs::write(root.join("server.key"), server_key.serialize_der()).unwrap();
        std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
        Self { root }
    }

    fn endpoint_command(&self, address: std::net::SocketAddr, receipt: &std::path::Path, generation: u64) -> tokio::process::Command {
        let media = PathBuf::from(env!("CARGO_BIN_EXE_obscura-host")).with_file_name("obscura-media");
        assert!(media.is_file(), "Build obscura-media before endpoint acceptance");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
        command.arg("--listen").arg(address.to_string())
            .arg("--host-dir").arg(self.root.join("host"))
            .arg("--socket-dir").arg(self.root.join("endpoint"))
            .arg("--server-cert").arg(self.root.join("server.der"))
            .arg("--server-key").arg(self.root.join("server.key"))
            .arg("--media-bin").arg(media)
            .arg("--applied-receipt-path").arg(receipt)
            .arg("--endpoint-generation").arg(generation.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }
}

impl Drop for ReceiptFixtures {
    fn drop(&mut self) {
        if self.root.exists() { let _ = std::fs::remove_dir_all(&self.root); }
    }
}

async fn wait_absent(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test]
async fn applied_receipt_is_written_after_bind_with_v1_shape_and_0600() {
    use std::os::unix::fs::PermissionsExt;
    tokio::time::timeout(Duration::from_secs(60), async {
        let fixtures = ReceiptFixtures::create();
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let receipt_path = fixtures.root.join("endpoint-applied").join("3.json");
        let mut endpoint = fixtures.endpoint_command(address, &receipt_path, 3).spawn().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !receipt_path.exists() {
            assert!(endpoint.try_wait().unwrap().is_none(), "endpoint exited before writing the receipt");
            assert!(tokio::time::Instant::now() < deadline, "applied receipt did not appear");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mode = std::fs::metadata(&receipt_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "applied receipt must be owner-only");
        let raw = std::fs::read_to_string(&receipt_path).unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(receipt["schema_version"], 1);
        assert_eq!(receipt["endpoint_generation"], 3);
        assert!(receipt.get("status").is_none(), "applied receipt must not carry a status field: {receipt}");
        // The receipt states only what the listener actually applied.
        let mut fields: Vec<&str> = receipt.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["bound_address", "endpoint_generation", "endpoint_pid", "schema_version"], "unexpected receipt fields: {receipt}");
        let bound: std::net::SocketAddr = receipt["bound_address"].as_str().unwrap().parse().unwrap();
        assert_eq!(bound, address);
        let pid = receipt["endpoint_pid"].as_u64().unwrap() as u32;
        assert_eq!(Some(pid), endpoint.id(), "receipt pid must be the real endpoint process");
        // Safety: signal 0 is an existence check on the explicit endpoint pid.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "receipt pid is not live");
        assert!(!receipt_path.with_extension("json.tmp").exists(), "temp file must be renamed away");
        // Terminate only the endpoint this test spawned.
        let _ = unsafe { libc::kill(endpoint.id().unwrap() as i32, libc::SIGTERM) };
        let _ = endpoint.wait().await;
    }).await.expect("applied receipt case timed out");
}

#[tokio::test]
async fn applied_receipt_is_absent_when_listener_bind_fails() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let fixtures = ReceiptFixtures::create();
        // Hold the port so the endpoint's bind fails before any receipt write.
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = occupied.local_addr().unwrap();
        let receipt_path = fixtures.root.join("endpoint-applied").join("4.json");
        let mut endpoint = fixtures.endpoint_command(address, &receipt_path, 4).spawn().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if endpoint.try_wait().unwrap().is_some() { break; }
            assert!(tokio::time::Instant::now() < deadline, "endpoint did not exit after a bind failure");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!receipt_path.exists(), "a failed bind must not publish an applied receipt");
        drop(occupied);
    }).await.expect("bind-failure receipt case timed out");
}

// ---------------------------------------------------------------------------
// BB-EX-MEDIA-35
//
// Healthy final-reference release reaches the finalizer before the reader is
// stopped and joined. Two exact sessions A and B run through the real Host and
// endpoint. The case releases A's final control reference while A's
// encoder-control reader is live, then observes only A's public media consumer,
// A's adapter process and exit status, the wrapper group state, and the
// per-session socket/directory listing.
// ---------------------------------------------------------------------------

fn media_packet_header(bytes: &[u8]) -> serde_json::Value {
    assert!(bytes.len() >= 4, "media frame header is truncated");
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert!(bytes.len() >= 4 + length, "media frame payload is truncated");
    serde_json::from_slice(&bytes[4..4 + length]).unwrap()
}

/// Read one media frame that belongs to the expected exact session, skipping
/// `waiting` placeholders for other sessions.
async fn media_frame(
    video: &mut (impl futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    expected_session: &str,
) -> serde_json::Value {
    loop {
        let message = video.next().await.expect("media channel ended before a frame").expect("media read failed");
        let Message::Binary(bytes) = message else { continue; };
        let header = media_packet_header(&bytes);
        if header["type"] == "waiting" { continue; }
        if let Some(session) = header["source"]["session_id"].as_str() {
            assert_eq!(session, expected_session, "media consumer observed a foreign session: {header}");
        }
        return header;
    }
}

async fn wait_dir(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

async fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Safety: signal 0 performs an existence/permission check only.
        if unsafe { libc::kill(pid, 0) } != 0 { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// PIDs whose full command line contains `needle`. Used to observe the real
/// per-session adapter process without a product test hook.
fn processes_matching(needle: &str) -> Vec<i32> {
    let output = std::process::Command::new("ps").args(["-axww", "-o", "pid=,command="]).output().expect("ps");
    String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| {
        let (pid, command) = line.trim().split_once(' ')?;
        command.contains(needle).then(|| pid.parse::<i32>().ok()).flatten()
    }).collect()
}

/// Read A's public media consumer to its explicit terminal. Returns the
/// terminal header (`closed` or `unavailable`).
async fn media_terminal(
    video: &mut (impl futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "A's media consumer did not reach a terminal");
        let message = match tokio::time::timeout(remaining, video.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) => return serde_json::json!({"type": "transport_end"}),
            Err(_) => panic!("A's media consumer did not reach a terminal within the bound"),
        };
        let Message::Binary(bytes) = message else { continue; };
        let header = media_packet_header(&bytes);
        if matches!(header["type"].as_str(), Some("closed") | Some("unavailable")) { return header; }
    }
}

async fn connect_endpoint_control(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> (tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String) {
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut request = format!("wss://localhost:{}/control", address.port()).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {ACCESS_TOKEN}").parse().unwrap());
    let (control, response) = client_async(request, tls).await.unwrap();
    let token = response.headers()["x-obscura-media-token"].to_str().unwrap().to_string();
    (control, token)
}

async fn endpoint_ready(control: &mut tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>) -> serde_json::Value {
    let message = control.next().await.unwrap().unwrap();
    let text = message.into_text().unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn open_media_consumer(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (video, _) = client_async(request, tls).await.unwrap();
    video
}

/// Catalog fixture. One profile: every remote peer is admitted to the catalog
/// default session, so two control connections hold independent references to
/// one exact-session media entry.
fn write_two_profile_catalog(root: &std::path::Path) {
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [
            {"profile_id": "default", "display_name": "Default", "default": true}
        ],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(root.join("profiles/catalog.json"), serde_json::to_string_pretty(&catalog).unwrap()).unwrap();
}

#[tokio::test]
async fn shared_session_release_finalizes_only_after_last_control() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = PathBuf::from("/tmp").join(format!("ob35-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura bb35 CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap()
            .signed_by(&server_key, &ca, &ca_key).unwrap();
        std::fs::write(root.join("server.der"), server.der()).unwrap();
        std::fs::write(root.join("server.key"), server_key.serialize_der()).unwrap();
        std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(root.join("ca.der"), ca.der()).unwrap();
        write_two_profile_catalog(&root);

        let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(root.join("host"))
            .arg("--profile-root").arg(root.join("profiles"))
            .arg("--access-token").arg(ACCESS_TOKEN)
            .stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
        wait_socket(root.join("host/host.sock")).await;
        let mut local = BufReader::new(tokio::net::UnixStream::connect(root.join("host/host.sock")).await.unwrap());
        let mut line = String::new(); local.read_line(&mut line).await.unwrap();
        let mut state_a = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await;
        let session_a = state_a["session_id"].as_str().unwrap().to_string();
        let html = "data:text/html,<style>body{margin:0;background:white}button{width:80px;height:80px;border:0;background:red}</style><button id='t'>t</button>";
        state_a = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state_a),"command":{"type":"resize","width":160,"height":120}})).await;
        let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state_a),"command":{"type":"navigate","url":html}})).await;
        // Every remote peer is admitted to the catalog default session, so the
        // two control connections below hold independent references to one
        // exact-session media entry.
        drop(local);

        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let media_bin = PathBuf::from(env!("CARGO_BIN_EXE_obscura-host")).with_file_name("obscura-media");
        assert!(media_bin.is_file(), "Build obscura-media before endpoint acceptance");
        let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
            .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
            .arg("--socket-dir").arg(root.join("endpoint"))
            .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("server.key"))
            .arg("--media-bin").arg(&media_bin)
            .stdout(Stdio::null()).stderr(std::fs::File::create(root.join("endpoint.err")).unwrap()).kill_on_drop(true).spawn().unwrap();
        wait_socket(root.join("endpoint")).await;

        let mut roots = rustls::RootCertStore::empty(); roots.add(ca.der().clone()).unwrap();
        let client_config = rustls::ClientConfig::builder().with_root_certificates(roots)
            .with_no_client_auth();
        let connector_a = tokio_rustls::TlsConnector::from(Arc::new(client_config.clone()));
        let connector_b = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let (mut control_a, token_a) = connect_endpoint_control(&connector_a, address).await;
        let ready_a = endpoint_ready(&mut control_a).await;
        assert_eq!(ready_a["version"], 11);
        assert_eq!(ready_a["session_id"].as_str(), Some(session_a.as_str()));
        let (mut control_b, token_b) = connect_endpoint_control(&connector_b, address).await;
        let ready_b = endpoint_ready(&mut control_b).await;
        assert_eq!(ready_b["version"], 11);
        assert_eq!(ready_b["session_id"].as_str(), Some(session_a.as_str()));
        assert_eq!(ready_a["session_id"], ready_b["session_id"], "both controls hold the same exact session");

        control_a.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await.unwrap();
        let attached_a: serde_json::Value = serde_json::from_str(control_a.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(attached_a["value"]["mode"], "observe");
        control_b.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await.unwrap();
        let attached_b: serde_json::Value = serde_json::from_str(control_b.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(attached_b["value"]["mode"], "observe");

        let mut video_a = open_media_consumer(&connector_a, address, &token_a).await;
        let mut video_b = open_media_consumer(&connector_b, address, &token_b).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let header = media_frame(&mut video_a, &session_a).await;
            if header["type"] == "access_unit" { break; }
            assert!(tokio::time::Instant::now() < deadline, "A never produced an access unit: {header}");
        }
        loop {
            let header = media_frame(&mut video_b, &session_a).await;
            if header["type"] == "access_unit" { break; }
            assert!(tokio::time::Instant::now() < deadline, "B never produced an access unit: {header}");
        }

        let dir = root.join(format!("endpoint/m-{}", &session_a.replace("-", "")[..12]));
        assert!(wait_dir(&dir, Duration::from_secs(10)).await, "the per-session directory was never bound");
        assert!(dir.join("encoded.sock").exists(), "encoded socket missing while healthy");
        assert!(dir.join("encoder-control.sock").exists(), "encoder-control socket missing while healthy");

        // One adapter serves the shared session. Its directory path and pid are
        // the identity of the exact session both controls hold.
        let adapters = processes_matching(&dir.to_string_lossy());
        assert_eq!(adapters.len(), 1, "exactly one adapter serves the shared session: {adapters:?}");
        let adapter_pid = adapters[0];

        // Release A's final control reference while its reader is live and B
        // still holds the same exact session. The endpoint reference count must
        // keep the shared session out of finalization.
        drop(control_a);
        let terminal = media_terminal(&mut video_a).await;
        assert!(
            matches!(terminal["type"].as_str(), Some("closed") | Some("unavailable") | Some("transport_end")),
            "A did not reach an explicit media terminal: {terminal}"
        );
        assert!(
            !wait_absent(&dir, Duration::from_secs(5)).await,
            "A's release finalized the session B still holds"
        );
        assert!(dir.join("encoded.sock").exists(), "A's release removed the shared encoded socket");
        assert!(dir.join("encoder-control.sock").exists(), "A's release removed the shared encoder-control socket");
        assert!(processes_matching(&dir.to_string_lossy()).contains(&adapter_pid), "A's release reaped B's adapter");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let header = media_frame(&mut video_b, &session_a).await;
            if header["type"] == "access_unit" { break; }
            assert!(tokio::time::Instant::now() < deadline, "B stopped decoding after A finalized: {header}");
        }
        assert!(dir.exists(), "A's release must not remove B's session directory");

        // Only the last reference releases the session: adapter reaped,
        // directory and sockets removed.
        drop(control_b);
        assert!(
            wait_absent(&dir, Duration::from_secs(20)).await,
            "the shared session was not finalized after the last release"
        );
        assert!(!dir.join("encoded.sock").exists(), "encoded socket survived finalization");
        assert!(wait_pid_gone(adapter_pid, Duration::from_secs(20)).await, "the adapter survived the last release");

        let _ = unsafe { libc::kill(endpoint.id().unwrap() as i32, libc::SIGTERM) };
        let _ = endpoint.wait().await;
        let _ = unsafe { libc::kill(host.id().unwrap() as i32, libc::SIGTERM) };
        let _ = host.wait().await;
        drop(video_a);
        drop(video_b);
        let _ = std::fs::remove_dir_all(&root);
    }).await.expect("shared session release case timed out");
}

/// The one access token this test configures on the Host and the endpoint.
const ACCESS_TOKEN: &str = "endpoint-test-access-token";

/// A minimal persistent catalog. It carries no membership and no access record.
fn write_catalog(root: &std::path::Path) {
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true
        }],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(root.join("profiles/catalog.json"), serde_json::to_string_pretty(&catalog).unwrap()).unwrap();
}

async fn local_request(socket: &mut BufReader<tokio::net::UnixStream>, request: serde_json::Value) -> serde_json::Value {
    socket.get_mut().write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let mut line = String::new(); socket.read_line(&mut line).await.unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["type"], "result", "{reply}"); reply["value"].clone()
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn adb(serial: &str, arguments: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("adb");
    command.args(["-s", serial]).args(arguments).kill_on_drop(true);
    Ok(tokio::time::timeout(Duration::from_secs(8), command.output()).await??)
}

async fn device_ready(serial: &str, root: &std::path::Path, address: std::net::SocketAddr, session: &str) {
    let remote = format!("/data/local/tmp/{}", root.file_name().unwrap().to_str().unwrap());
    assert!(adb(serial, &["shell", "mkdir", "-m", "700", &remote]).await.unwrap().status.success());
    let result: anyhow::Result<_> = async {
        let token = root.join("token.txt");
        anyhow::ensure!(adb(serial, &["push", token.to_str().unwrap(), &format!("{remote}/token.txt")]).await?.status.success(), "Device fixture upload failed");
        anyhow::ensure!(adb(serial, &["shell", "chmod", "600", &format!("{remote}/token.txt")]).await?.status.success(), "Device token permissions failed");
        let command = format!("curl -i -sS -k --http1.1 --max-time 2 -H 'Authorization: Bearer {ACCESS_TOKEN}' -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' https://{address}/control");
        adb(serial, &["shell", &command]).await
    }.await;
    // Remove only the ephemeral test token, including on upload/curl errors.
    let removed = adb(serial, &["shell", "rm", "-f", &format!("{remote}/token.txt")]).await;
    let directory = adb(serial, &["shell", "rmdir", &remote]).await;
    assert!(removed.unwrap().status.success() && directory.unwrap().status.success(), "Temporary device token cleanup failed");
    let result = result.unwrap();
    let response = String::from_utf8_lossy(&result.stdout);
    assert!(response.contains("101 Switching Protocols") && response.contains(session), "Device TLS/WSS ready failed: {:?}; {}", result.status, String::from_utf8_lossy(&result.stderr));
    println!("DEVICE_TLS_WSS_READY_PASS: paired Android curl received the actual Host session; native media/UI not tested");
}
