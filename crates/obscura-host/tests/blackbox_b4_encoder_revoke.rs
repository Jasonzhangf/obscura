#![cfg(unix)]
//! B4 black-box regression: the endpoint is NOT an owner of the registered
//! process group.
//!
//! The test acts as a real adapter over the per-session Unix socket. It
//! registers a real process group whose SIGTERM handler writes a marker. The
//! adapter is the sole owner of group signal and reap, so the endpoint must
//! acknowledge a revoke without ever signalling that group: the marker must
//! never appear, and the group must still be alive when the acknowledgement
//! arrives. A later revoke is idempotent.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use obscura_media::encoder_owner::{EncoderControlRequest, EncoderControlResponse, EncoderPermit};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

#[tokio::test]
async fn revoke_ack_does_not_depend_on_endpoint_group_signal() {
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
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    write_catalog(root.join("profiles"))?;

    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--access-token").arg(TOKEN)
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
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    let (mut control, _token) = connect_control(&connector, address, TOKEN).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");

    let control_socket = root.join(format!("endpoint/m-{}/encoder-control.sock", &session_id.replace("-", "")[..12]));
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

    // The adapter owns the group while the session is live. A revoke must be
    // acknowledged without any endpoint-side signal work: the endpoint has no
    // authority to signal, so it has nothing to wait for. The group is
    // deliberately still alive, so an endpoint that waited for its own signal
    // could never answer.
    let response = exchange(&mut read, &mut write, EncoderControlRequest::Revoke { registration_id: 7 }).await?;
    anyhow::ensure!(matches!(response, EncoderControlResponse::Revoke { refused: false }),
        "revoke was not acknowledged: {response:?}");
    anyhow::ensure!(
        !marker.exists(),
        "the endpoint signalled the registered group while acknowledging the revoke"
    );
    anyhow::ensure!(
        unsafe { libc::kill(-pgid, 0) } == 0,
        "the registered group was gone after revoke: the endpoint reaped a group it does not own"
    );

    // Idempotent second revoke, still without any group signal.
    let second = exchange(&mut read, &mut write, EncoderControlRequest::Revoke { registration_id: 7 }).await?;
    anyhow::ensure!(matches!(second, EncoderControlResponse::Revoke { refused: false }),
        "idempotent revoke was refused: {second:?}");
    anyhow::ensure!(
        !marker.exists(),
        "a repeated revoke caused the endpoint to signal the registered group"
    );

    // Now close the session. The finalizer retires its registration authority
    // and must still never signal the group, which remains adapter-owned.
    control.send(Message::Close(None)).await?;
    drop(control);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    anyhow::ensure!(
        !marker.exists(),
        "the endpoint signalled the registered group during session finalize: {} appeared",
        marker.display()
    );
    anyhow::ensure!(
        unsafe { libc::kill(-pgid, 0) } == 0,
        "the endpoint reaped the registered group during finalize; only the adapter may"
    );

    // The adapter now owns cleanup: it signals and reaps the group itself.
    // This is the exact sequence the base adapter runs, and it must still work
    // after the endpoint has retired its authority.
    anyhow::ensure!(
        unsafe { libc::kill(-pgid, libc::SIGTERM) } == 0,
        "adapter-owned SIGTERM did not reach the registered group"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    anyhow::ensure!(marker.exists(), "the group's own SIGTERM handler did not record the adapter signal");
    // SIGKILL the group now that delivery is proven, then reap it.
    anyhow::ensure!(
        unsafe { libc::kill(-pgid, libc::SIGKILL) } == 0,
        "adapter-owned SIGKILL did not reach the registered group"
    );
    let status = tokio::time::timeout(Duration::from_secs(20), group_child.wait())
        .await
        .context("adapter-owned group reap deadline")??;
    anyhow::ensure!(!status.success(), "registered group exited successfully instead of being signalled: {status}");
    // The group identity is only gone once the reaper has collected it.
    let mut identity_gone = false;
    for _ in 0..40 {
        identity_gone = unsafe { libc::kill(-pgid, 0) } != 0;
        if identity_gone {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(identity_gone, "the registered group identity was still addressable after reap");

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
    access_token: &str,
) -> anyhow::Result<(tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/control", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {access_token}").parse()?);
    let (control, response) = client_async(request, tls).await?;
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

/// The one access token this test configures on both binaries.
const TOKEN: &str = "b4-access-token";

/// A minimal persistent catalog. It carries no membership and no access record.
fn write_catalog(profile_root: PathBuf) -> anyhow::Result<()> {
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
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}
