#![cfg(unix)]
//! B8 black-box regression (F1): endpoint shutdown must join an in-progress
//! final-reference release before it drops the socket directory and returns.
//!
//! The test drives real mTLS control against a real `obscura-endpoint` and a
//! real `--media-bin` adapter. It closes the control attachment so the
//! final-reference release begins (the endpoint SIGTERMs the adapter), waits
//! until that release is observably in progress, then SIGTERMs the endpoint
//! during the release. Pre-fix the endpoint skips the finalizing entry and
//! exits almost immediately, abandoning the still-running cleanup. Post-fix it
//! awaits that finalizer, so shutdown is bounded by the adapter's cooperative
//! grace. The measured shutdown duration and the adapter's observed death are
//! external observations; no endpoint-private state is asserted.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::Message};

#[tokio::test]
async fn endpoint_shutdown_joins_in_progress_final_release() {
    tokio::time::timeout(Duration::from_secs(120), exercise())
        .await
        .expect("B8 public shutdown-join case timed out")
        .expect("B8 public shutdown-join case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob8-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b8 CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
        .signed_by(&server_key, &ca, &ca_key)?;
    let client_key = rcgen::KeyPair::generate()?;
    let mut client_params = rcgen::CertificateParams::new(vec![])?;
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &ca, &ca_key)?;
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("ca.der"), ca.der())?;
    std::fs::write(root.join("client.der"), client.der())?;
    std::fs::write(root.join("client.key"), client_key.serialize_der())?;
    std::fs::set_permissions(root.join("client.key"), std::fs::Permissions::from_mode(0o600))?;
    write_authorized(root.join("registry.json"), root.join("authorized-clients.json"), root.join("profiles"), &client.der().to_vec())?;

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
    drop(local);

    let reservation = std::net::TcpListener::bind((bind_ip, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);

    // A real adapter that connects to the exact-session video socket, records
    // its pid, and installs a SIGTERM handler that records the signal but keeps
    // the process alive. The endpoint's cooperative grace therefore expires and
    // only the adapter's own reap (post-fix, joined by shutdown) ends it.
    let pid_file = root.join("adapter.pid");
    let marker = root.join("adapter-signalled");
    let media_bin = root.join("adapter.py");
    let script = [
        "#!/usr/bin/env python3",
        "import os, signal, socket, sys, time",
        "args = sys.argv[1:]",
        "video = args[args.index('--video-socket') + 1]",
        &format!("pid_file = {pid:?}", pid = pid_file.display().to_string()),
        &format!("marker = {mark:?}", mark = marker.display().to_string()),
        "def on_term(signum, frame):",
        "    with open(pid_file, 'w') as handle:",
        "        handle.write(str(os.getpid()))",
        "    open(marker, 'w').close()",
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
        "open(video + '.ready', 'w').close()",
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
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![client.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key.serialize_der())),
        )?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    let (mut control, _token) = connect_control(&connector, address).await?;
    let ready = next_json(&mut control).await?;
    let session_id = ready["session_id"].as_str().expect("Endpoint Ready session identity missing").to_string();
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");

    let session_dir = root.join(format!("endpoint/session-{session_id}"));
    anyhow::ensure!(wait_dir(&session_dir, Duration::from_secs(15)).await, "session directory was never bound");
    wait_file(&session_dir.join("encoded.sock.ready"), Duration::from_secs(15)).await?;

    // Close the control attachment. This is the final reference, so the endpoint
    // begins the exact-session finalizer and SIGTERMs the adapter.
    drop(control);
    wait_file(&marker, Duration::from_secs(15)).await?;
    let pid: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    anyhow::ensure!(process_alive(pid), "adapter was not alive when the release began (pid {pid})");

    // SIGTERM the endpoint while the final-reference release is in progress.
    let started = tokio::time::Instant::now();
    if let Some(pid) = endpoint.id() {
        // Safety: signaling the one endpoint process this test spawned.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    let status = tokio::time::timeout(Duration::from_secs(30), endpoint.wait()).await
        .map_err(|_| anyhow::anyhow!("endpoint did not exit after SIGTERM"))??;
    let elapsed = started.elapsed();
    eprintln!("B8: endpoint shutdown took {elapsed:?} (status {status})");
    anyhow::ensure!(elapsed >= Duration::from_millis(2000),
        "endpoint shutdown returned before the in-progress final-reference release finished ({elapsed:?}); it did not join the finalizer");

    // By the time shutdown returns, the joined finalizer has reaped the adapter.
    let reaped = wait_pid_gone(pid, Duration::from_secs(5)).await;
    anyhow::ensure!(reaped, "endpoint shutdown did not reap its adapter before returning (pid {pid})");

    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
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

async fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while process_alive(pid) {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

fn process_alive(pid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
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

fn write_authorized(
    registry_path: PathBuf,
    projection_path: PathBuf,
    profile_root: PathBuf,
    client_der: &[u8],
) -> anyhow::Result<()> {
    let fingerprint = hex(&Sha256::digest(client_der));
    let scopes = ["observe_human_control", "profile_read", "profile_create", "profile_select"];
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-b8",
        "devices": [{
            "device_id": "test-device",
            "device_label": "b8 client",
            "client_certificate_sha256": fingerprint,
            "scopes": scopes,
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1},
            "scope_revision": 1,
            "status": "active",
            "not_before_unix": 0,
            "not_after_unix": 4102444800i64,
            "created_at_unix": 0,
            "revoked_at_unix": null
        }]
    });
    std::fs::write(&registry_path, serde_json::to_vec(&registry)?)?;
    let projection = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "registry_sha256": "00".repeat(32),
        "clients": [{
            "device_id": "test-device",
            "client_certificate_sha256": fingerprint,
            "scopes": scopes,
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1}
        }]
    });
    std::fs::write(&projection_path, serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "test-device"]);
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true,
            "access": {"owner_device_id": "local", "members": members}
        }],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}
