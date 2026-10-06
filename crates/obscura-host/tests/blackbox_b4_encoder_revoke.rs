#![cfg(unix)]
//! B4 black-box regression: a revoke acknowledgement from the real endpoint
//! encoder-control socket must not outrun the endpoint's signal authority over
//! the registered process group.
//!
//! The test acts as a real adapter over the per-session Unix socket. It
//! registers a real process group that writes a marker when it receives the
//! endpoint's SIGTERM. While that group is still alive, a revoke request must
//! remain unanswered. The acknowledgement is accepted only after the group has
//! been reaped, and a later revoke is idempotent.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use obscura_media::encoder_owner::{EncoderControlRequest, EncoderControlResponse, EncoderPermit};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::Message};

#[tokio::test]
async fn revoke_ack_waits_for_endpoint_group_signal_authority() {
    tokio::time::timeout(Duration::from_secs(120), exercise())
        .await
        .expect("B4 public revoke-ordering case timed out")
        .expect("B4 public revoke-ordering case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob4-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b4 CA");
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

    let media_bin = root.join("adapter.sh");
    let adapter_term = root.join("adapter-term");
    std::fs::write(&media_bin, format!(
        "#!/bin/sh\ntrap 'echo term > {marker}' TERM\nwhile :; do sleep 1; done\n",
        marker = adapter_term.display()
    ))?;
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
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");

    let control_socket = root.join(format!("endpoint/session-{session_id}/encoder-control.sock"));
    wait_socket(control_socket.clone()).await;
    let stream = tokio::net::UnixStream::connect(&control_socket).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);

    let acquired = exchange(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 7 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "encoder acquire was not granted: {acquired:?}");

    let marker = root.join("group-signalled");
    let ready = root.join("group-ready");
    let group_script = format!(
        "import signal, sys, time\nmarker = {marker:?}\nready = {ready:?}\ndef on_term(signum, frame):\n    open(marker, 'w').close()\n    time.sleep(30)\nsignal.signal(signal.SIGTERM, on_term)\nopen(ready, 'w').close()\nwhile True:\n    time.sleep(0.2)\n",
        marker = marker.display().to_string(),
        ready = ready.display().to_string()
    );
    let mut group_child = tokio::process::Command::new("python3")
        .arg("-c").arg(group_script)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pgid = i32::try_from(group_child.id().expect("group child pid"))?;
    // The trap must be installed before the endpoint can signal the group, or
    // the default SIGTERM action kills the helper without writing the marker.
    wait_file(&ready, Duration::from_secs(10)).await?;
    anyhow::ensure!(unsafe { libc::getpgid(pgid) } == pgid, "test group child was not its own process group");
    anyhow::ensure!(unsafe { libc::kill(-pgid, 0) } == 0, "test process group does not exist before registration");
    let registered = exchange(&mut read, &mut write, EncoderControlRequest::Register { registration_id: 7, pgid }).await?;
    anyhow::ensure!(matches!(registered, EncoderControlResponse::Register { refused: false }),
        "encoder registration was refused: {registered:?}");

    control.send(Message::Close(None)).await?;
    drop(control);
    // The endpoint has now begun its finalizer: it takes signal authority over
    // the registered group and SIGTERMs it. The marker proves the endpoint, not
    // the test, signalled the group; only then do we issue the adapter revoke.
    wait_file(&marker, Duration::from_secs(10)).await?;

    let mut group_wait = tokio::spawn(async move { group_child.wait().await });
    send(&mut write, &EncoderControlRequest::Revoke { registration_id: 7 }).await?;
    let mut response_line = String::new();
    let mut group_status = None;
    let ack_before_reap = {
        let response_fut = read.read_line(&mut response_line);
        tokio::pin!(response_fut);
        tokio::select! {
            joined = &mut group_wait => {
                group_status = Some(joined.expect("group wait task panicked")?);
                false
            }
            read = &mut response_fut => {
                read?;
                true
            }
        }
    };
    anyhow::ensure!(!ack_before_reap,
        "revoke acknowledgement arrived while the endpoint still held signal authority over the registered group");
    // The group was reaped first; the acknowledgement must follow.
    if response_line.is_empty() {
        tokio::time::timeout(Duration::from_secs(15), read.read_line(&mut response_line)).await
            .context("revoke was not acknowledged after the group was reaped")??;
    }
    let response: EncoderControlResponse = serde_json::from_str(response_line.trim_end())?;
    anyhow::ensure!(matches!(response, EncoderControlResponse::Revoke { refused: false }),
        "revoke was not acknowledged: {response:?}");
    let status = match group_status {
        Some(status) => status,
        None => group_wait.await.expect("group wait task panicked")?,
    };
    anyhow::ensure!(!status.success(), "registered group exited successfully instead of being signalled: {status}");

    let mut second_line = String::new();
    send(&mut write, &EncoderControlRequest::Revoke { registration_id: 7 }).await?;
    tokio::time::timeout(Duration::from_secs(2), read.read_line(&mut second_line)).await
        .context("idempotent revoke was not acknowledged")??;
    let second: EncoderControlResponse = serde_json::from_str(second_line.trim_end())?;
    anyhow::ensure!(matches!(second, EncoderControlResponse::Revoke { refused: false }),
        "idempotent revoke was refused: {second:?}");

    if let Some(pid) = endpoint.id() {
        // Safety: signaling the one endpoint process this test spawned so its
        // finalizer reaps the per-session media adapter instead of orphaning it.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    let _ = tokio::time::timeout(Duration::from_secs(20), endpoint.wait()).await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, request: &EncoderControlRequest) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    Ok(())
}

async fn exchange(
    read: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    request: EncoderControlRequest,
) -> anyhow::Result<EncoderControlResponse> {
    send(write, &request).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line)).await
        .context("encoder control response deadline")??;
    Ok(serde_json::from_str(line.trim_end())?)
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

async fn wait_file(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "file never appeared: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
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
        "host_id": "host-b4",
        "devices": [{
            "device_id": "test-device",
            "device_label": "b4 client",
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
