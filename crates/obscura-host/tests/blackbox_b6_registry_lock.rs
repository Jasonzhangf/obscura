#![cfg(unix)]
//! B6 black-box regression: releasing one real exact session must not hold the
//! endpoint's shared registry across that session's slow supervisor wait. Two
//! real sessions run against one `obscura-endpoint`; while session A's adapter
//! is slow to exit, a fresh authorized attachment to session B must still be
//! served. The observable is external: the second attach response arrives while
//! session A's adapter process is still alive, instead of only after A's
//! supervisor wait releases the registry.
//!
//! Everything here is external to the endpoint: real mTLS/WSS control, the real
//! per-session `--media-bin` adapter process under `--socket-dir`, and real
//! process liveness. No private state, counters or source structure are
//! asserted.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::Message};

#[tokio::test]
async fn slow_session_finalize_does_not_block_a_second_session() {
    tokio::time::timeout(Duration::from_secs(180), exercise())
        .await
        .expect("B6 public registry-lock case timed out")
        .expect("B6 public registry-lock case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob6-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b6 CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
        .signed_by(&server_key, &ca, &ca_key)?;
    let client_a_key = rcgen::KeyPair::generate()?;
    let client_b_key = rcgen::KeyPair::generate()?;
    let mut client_params = rcgen::CertificateParams::new(vec![])?;
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client_a = client_params.clone().signed_by(&client_a_key, &ca, &ca_key)?;
    let client_b = client_params.clone().signed_by(&client_b_key, &ca, &ca_key)?;
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("ca.der"), ca.der())?;
    std::fs::write(root.join("client-a.der"), client_a.der())?;
    std::fs::write(root.join("client-a.key"), client_a_key.serialize_der())?;
    std::fs::set_permissions(root.join("client-a.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("client-b.der"), client_b.der())?;
    std::fs::write(root.join("client-b.key"), client_b_key.serialize_der())?;
    std::fs::set_permissions(root.join("client-b.key"), std::fs::Permissions::from_mode(0o600))?;
    write_two_session_fixtures(&root, &client_a.der().to_vec(), &client_b.der().to_vec())?;

    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(Stdio::null()).stderr(std::fs::File::create(root.join("host.err"))?).kill_on_drop(true).spawn()?;
    wait_socket(root.join("host/host.sock")).await;
    let mut local = BufReader::new(tokio::net::UnixStream::connect(root.join("host/host.sock")).await?);
    let mut line = String::new();
    local.read_line(&mut line).await?;
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:80px;height:80px;background:red'></div>";
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    // Materialize the second profile as its own exact session.
    let bound = local_request(&mut local, serde_json::json!({"id":4,"operation":identity(&state),"command":{"type":"profile_select","profile_id":"p1"}})).await?;
    let session_b = bound["status"]["session_id"].as_str().expect("second session identity missing").to_string();
    drop(local);

    let reservation = std::net::TcpListener::bind((bind_ip, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);

    // A real adapter that keeps the endpoint's ingest reader open (it connects
    // to the `--video-socket` it is handed) and stays alive for five seconds
    // after SIGTERM, so session A's supervisor wait is observable from the
    // outside. The handler records the direct adapter pid and the observation
    // marker only when the endpoint signals it; B's adapter never overwrites
    // the file.
    let pid_file = root.join("a.pid");
    let marker = root.join("a-signalled");
    let media_bin = root.join("adapter.sh");
    let script = [
        "#!/usr/bin/env python3",
        "import os, signal, socket, sys, time",
        "args = sys.argv[1:]",
        "video = args[args.index('--video-socket') + 1]",
        "ready = video + '.ready'",
        &format!("pid_file = {pid:?}", pid = pid_file.display().to_string()),
        &format!("marker = {marker:?}", marker = marker.display().to_string()),
        "def on_term(signum, frame):",
        "    with open(pid_file, 'w') as handle:",
        "        handle.write(str(os.getpid()))",
        "    open(marker, 'w').close()",
        "    time.sleep(5)",
        "    sys.exit(0)",
        "signal.signal(signal.SIGTERM, on_term)",
        "sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)",
        "deadline = time.time() + 10",
        "while True:",
        "    try:",
        "        sock.connect(video)",
        "        break",
        "    except OSError:",
        "        if time.time() > deadline:",
        "            raise",
        "        time.sleep(0.05)",
        "open(ready, 'w').close()",
        "while True:",
        "    time.sleep(0.2)",
        "",
    ].join("\n");
    std::fs::write(&media_bin, script)?;
    std::fs::set_permissions(&media_bin, std::fs::Permissions::from_mode(0o755))?;

    let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string())
        .arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der"))
        .arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--media-bin").arg(&media_bin)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(root.join("endpoint.err"))?)
        .kill_on_drop(true)
        .spawn()?;
    wait_socket(root.join("endpoint")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone())?;
    let connector_a = tls_connector(&roots, &client_a, &client_a_key)?;
    let connector_b = tls_connector(&roots, &client_b, &client_b_key)?;

    // Attach device A (default session) and device B (secondary session).
    let (mut control_a, _token_a) = connect_control(&connector_a, address).await?;
    let ready_a = next_json(&mut control_a).await?;
    let session_a = ready_a["session_id"].as_str().expect("A session identity missing").to_string();
    anyhow::ensure!(session_a != session_b, "the two profiles resolved to one session");
    control_a.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached_a = next_json(&mut control_a).await?;
    anyhow::ensure!(attached_a["type"] == "result", "A attach failed: {attached_a}");

    let (mut control_b, _token_b) = connect_control(&connector_b, address).await?;
    let ready_b = next_json(&mut control_b).await?;
    anyhow::ensure!(ready_b["session_id"].as_str() == Some(session_b.as_str()), "B Ready session changed: {ready_b}");
    control_b.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached_b = next_json(&mut control_b).await?;
    anyhow::ensure!(attached_b["type"] == "result", "B attach failed: {attached_b}");

    let dir_a = root.join(format!("endpoint/m-{}", &session_a.replace("-", "")[..12]));
    anyhow::ensure!(wait_dir(&dir_a, Duration::from_secs(10)).await, "A session directory was never bound");
    // A's adapter records readiness after installing the SIGTERM handler and
    // connecting to the exact-session socket, so the abrupt close below cannot
    // race Python startup and miss the trapped signal.
    wait_file(&dir_a.join("encoded.sock.ready"), Duration::from_secs(15)).await?;

    // Abruptly close A's control connection. The endpoint releases A's exact
    // session reference and finalizes A: it SIGTERMs the adapter, which traps
    // TERM, records its pid, touches the marker and then stays alive for five
    // seconds. The marker proves A's slow supervisor wait is in progress,
    // which pre-fix happens while the shared registry lock is held.
    drop(control_a);
    wait_file(&marker, Duration::from_secs(15)).await?;
    let pid_a: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    anyhow::ensure!(process_alive(pid_a), "A's adapter was not alive when its supervisor wait began (pid {pid_a})");

    // A fresh authorized attachment to the *other* session must be served while
    // A is still finalizing. Pre-fix, the registry mutex is held across A's
    // supervisor wait, so this only completes after A's adapter is reaped.
    let started = tokio::time::Instant::now();
    let (mut retry_b, _retry_token) = connect_control(&connector_b, address).await?;
    let retry_ready = next_json(&mut retry_b).await?;
    anyhow::ensure!(retry_ready["session_id"].as_str() == Some(session_b.as_str()), "retry Ready session changed: {retry_ready}");
    retry_b.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let retry_attached = next_json(&mut retry_b).await?;
    let elapsed = started.elapsed();
    anyhow::ensure!(retry_attached["type"] == "result", "B retry attach failed: {retry_attached}");

    let a_still_alive = process_alive(pid_a);
    eprintln!("B6: second-session attach completed in {elapsed:?}; A adapter alive={a_still_alive} pid={pid_a}");
    anyhow::ensure!(elapsed < Duration::from_secs(2),
        "the second session was served only after session A's slow supervisor wait released the registry ({elapsed:?})");
    anyhow::ensure!(a_still_alive,
        "the second session was only served after session A's slow supervisor wait released the registry");

    let _ = &mut control_b;
    let _ = &mut retry_b;
    graceful_stop(&mut endpoint).await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn tls_connector(
    roots: &rustls::RootCertStore,
    cert: &rcgen::Certificate,
    key: &rcgen::KeyPair,
) -> anyhow::Result<tokio_rustls::TlsConnector> {
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots.clone())
        .with_client_auth_cert(
            vec![rustls::pki_types::CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )?;
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

async fn connect_control(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> anyhow::Result<(tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let (control, response) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await?;
    let token = response.headers()["x-obscura-media-token"].to_str()?.to_string();
    Ok((control, token))
}

async fn next_json(
    control: &mut tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    Ok(serde_json::from_str(&message.into_text()?)?)
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn wait_dir(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.is_dir() {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

async fn wait_file(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "file never appeared: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn process_alive(pid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
}

/// SIGTERM the endpoint so its real shutdown path reaps the slow adapters
/// instead of leaving them orphaned.
async fn graceful_stop(endpoint: &mut tokio::process::Child) {
    if let Some(pid) = endpoint.id() {
        // Safety: signaling the one endpoint process this test spawned.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    let _ = tokio::time::timeout(Duration::from_secs(20), endpoint.wait()).await;
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
}

async fn local_request(socket: &mut BufReader<tokio::net::UnixStream>, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    socket.get_mut().write_all(format!("{request}\n").as_bytes()).await?;
    let mut line = String::new();
    socket.read_line(&mut line).await?;
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    anyhow::ensure!(reply["type"] == "result", "Host request failed: {reply}");
    Ok(reply["value"].clone())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn write_two_session_fixtures(root: &std::path::Path, client_a: &[u8], client_b: &[u8]) -> anyhow::Result<()> {
    let fingerprint_a = hex(&Sha256::digest(client_a));
    let fingerprint_b = hex(&Sha256::digest(client_b));
    let scopes = serde_json::json!(["observe_human_control", "profile_read", "profile_create", "profile_select"]);
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-b6",
        "devices": [
            {
                "device_id": "device-a", "device_label": "session a",
                "client_certificate_sha256": fingerprint_a, "scopes": scopes,
                "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1},
                "scope_revision": 1, "status": "active",
                "not_before_unix": 0, "not_after_unix": 4102444800i64,
                "created_at_unix": 0, "revoked_at_unix": null
            },
            {
                "device_id": "device-b", "device_label": "session b",
                "client_certificate_sha256": fingerprint_b, "scopes": scopes,
                "initial_profile_grant": {"selection": {"profile_id": "p1"}, "authorization_revision": 1},
                "scope_revision": 1, "status": "active",
                "not_before_unix": 0, "not_after_unix": 4102444800i64,
                "created_at_unix": 0, "revoked_at_unix": null
            }
        ]
    });
    std::fs::write(root.join("registry.json"), serde_json::to_vec(&registry)?)?;
    let projection = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "registry_sha256": "dd".repeat(32),
        "clients": [
            {
                "device_id": "device-a", "client_certificate_sha256": fingerprint_a, "scopes": scopes,
                "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1}
            },
            {
                "device_id": "device-b", "client_certificate_sha256": fingerprint_b, "scopes": scopes,
                "initial_profile_grant": {"selection": {"profile_id": "p1"}, "authorization_revision": 1}
            }
        ]
    });
    std::fs::write(root.join("authorized-clients.json"), serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "device-a", "device-b"]);
    let catalog = serde_json::json!({
        "catalog_revision": 2,
        "profiles": [
            {"profile_id": "default", "display_name": "Default", "default": true,
             "access": {"owner_device_id": "local", "members": members}},
            {"profile_id": "p1", "display_name": "Secondary", "default": false,
             "access": {"owner_device_id": "local", "members": members}}
        ],
        "default_profile_id": "default",
        "next_id": 2
    });
    std::fs::create_dir_all(root.join("profiles"))?;
    std::fs::write(root.join("profiles/catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}
