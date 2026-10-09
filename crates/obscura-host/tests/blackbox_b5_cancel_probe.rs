#![cfg(unix)]
//! B5 impossibility probe (non-discriminating by design).
//!
//! The B5 defect only leaks the exact-session reference when the one-hour
//! connection-lifetime future is cancelled while the endpoint process keeps
//! running. This probe exercises the only other public cancellation trigger:
//! endpoint shutdown (`SIGTERM`), which aborts every connection task and then
//! runs `finalize_all` over the whole registry. That shutdown finalizer removes
//! the session regardless of the cancelled connection's leaked reference, so
//! this arrangement cannot discriminate the fix. It is retained only as the
//! recorded impossibility arrangement; see `docs/runs/.../notes.md` A3.
//!
//! All observations are external: real mTLS/WSS control, the real per-session
//! `--media-bin` process under `--socket-dir`, real process liveness and real
//! filesystem effects.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::Message};

#[tokio::test]
async fn endpoint_shutdown_reaps_bound_session_but_does_not_discriminate_b5() {
    tokio::time::timeout(Duration::from_secs(120), exercise())
        .await
        .expect("B5 shutdown probe timed out")
        .expect("B5 shutdown probe failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob5-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b5 CA");
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
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let session_id = state["session_id"].as_str().expect("Host session identity missing").to_string();
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:80px;height:80px;background:red'></div>";
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);

    let reservation = std::net::TcpListener::bind((bind_ip, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);

    // A real adapter process that outlives the control connection until the
    // endpoint's shutdown finalizer signals it.
    let media_bin = root.join("adapter.sh");
    std::fs::write(&media_bin, "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n")?;
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
    let endpoint_pid = endpoint.id().expect("endpoint pid") as i32;
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
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result" && attached["value"]["mode"] == "observe",
        "attach failed: {attached}");

    let session_dir = root.join(format!("endpoint/m-{}", &session_id.replace("-", "")[..12]));
    anyhow::ensure!(wait_dir(&session_dir, Duration::from_secs(10)).await,
        "the bound session directory was never created");

    // The connection and its bound slot are live. Endpoint shutdown aborts the
    // connection task, then runs the endpoint-wide finalizer.
    anyhow::ensure!(unsafe { libc::kill(endpoint_pid, libc::SIGTERM) } == 0, "endpoint SIGTERM failed");
    let status = endpoint.wait().await?;
    anyhow::ensure!(status.success(), "endpoint did not exit cleanly on SIGTERM: {status}");
    anyhow::ensure!(wait_absent(&session_dir, Duration::from_secs(20)).await,
        "endpoint shutdown left the bound session directory behind");
    anyhow::ensure!(!session_dir.join("encoded.sock").exists(),
        "endpoint shutdown left encoded.sock behind");
    anyhow::ensure!(!session_dir.join("encoder-control.sock").exists(),
        "endpoint shutdown left encoder-control.sock behind");

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

async fn wait_absent(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
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
        "host_id": "host-b5",
        "devices": [{
            "device_id": "test-device",
            "device_label": "b5 client",
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
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true,
            "access": {"owner_device_id": "local", "members": ["local", "test-device"]}
        }],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}
