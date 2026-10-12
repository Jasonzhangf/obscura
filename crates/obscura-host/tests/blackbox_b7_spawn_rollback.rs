#![cfg(unix)]
//! B7 black-box regression: a real `obscura-endpoint` whose configured
//! `--media-bin` cannot be spawned must not leave the per-session directory or
//! either bound socket behind, and a later acquisition on the same endpoint
//! must still be able to serve media.
//!
//! Everything here is external to the endpoint: real mTLS/WSS control, real
//! filesystem effects under `--socket-dir`, and the real `/media` upgrade
//! handshake. No private state, counters or source structure are asserted.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

#[tokio::test]
async fn failed_adapter_spawn_rolls_back_and_retry_serves_media() {
    tokio::time::timeout(Duration::from_secs(120), exercise())
        .await
        .expect("B7 public rollback case timed out")
        .expect("B7 public rollback case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // Exact-session media dirs are `socket_dir/session-<36>/encoded.sock` and
    // must stay below the macOS SUN_LEN bound, so keep the root short.
    let root = PathBuf::from("/tmp").join(format!("ob7-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b7 CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
        .signed_by(&server_key, &ca, &ca_key)?;
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("ca.der"), ca.der())?;
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

    // The configured media binary intentionally does not exist yet. This is the
    // documented public way to force a real adapter spawn failure.
    let media_bin = root.join("adapter.sh");
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

    let session_dir = root.join(format!("endpoint/m-{}", &session_id.replace("-", "")[..12]));
    let encoded_socket = session_dir.join("encoded.sock");
    let control_socket = session_dir.join("encoder-control.sock");

    // First attachment: the Host attach succeeds, but the endpoint cannot spawn
    // the adapter. The failed acquisition must roll its own resources back.
    let (mut control, token) = connect_control(&connector, address).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result" && attached["value"]["mode"] == "observe",
        "attach did not succeed before the media acquisition failure: {attached}");

    // The externally observable rollback: no session directory and no bound
    // sockets survive the failed adapter spawn.
    wait_absent(&session_dir, Duration::from_secs(10)).await?;
    anyhow::ensure!(!encoded_socket.exists(), "failed spawn left encoded.sock behind: {}", encoded_socket.display());
    anyhow::ensure!(!control_socket.exists(), "failed spawn left encoder-control.sock behind: {}", control_socket.display());
    eprintln!("B7 first attach: rollback observed (no session directory or sockets)");

    // Provide a real, spawnable adapter and retry on a fresh authorized
    // attachment. The retry must not be poisoned by the failed attempt.
    std::fs::write(&media_bin, "#!/bin/sh\nsleep 30\n")?;
    std::fs::set_permissions(&media_bin, std::fs::Permissions::from_mode(0o755))?;
    let (mut retry, retry_token) = connect_control(&connector, address).await?;
    let retry_ready = next_json(&mut retry).await?;
    anyhow::ensure!(retry_ready["session_id"].as_str() == Some(session_id.as_str()), "retry Ready session changed: {retry_ready}");
    retry.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let retry_attached = next_json(&mut retry).await?;
    anyhow::ensure!(retry_attached["type"] == "result" && retry_attached["value"]["mode"] == "observe",
        "retry attach failed: {retry_attached}");

    // A successful acquisition binds the exact session and populates the media
    // grant, so the real `/media` upgrade is accepted. A poisoned retry leaves
    // the grant unbound and the handshake is refused.
    let media = open_media(&connector, address, &retry_token).await?;
    eprintln!("B7 retry: /media upgrade accepted after the failed acquisition");
    drop(media);

    // Keep both control attachments alive long enough for the media grant to be
    // established before teardown.
    let _ = token;
    let _ = &mut control;

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

async fn connect_control(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> anyhow::Result<(tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/control", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {TOKEN}").parse()?);
    let (control, response) = client_async(request, tls).await?;
    let token = response.headers()["x-obscura-media-token"].to_str()?.to_string();
    Ok((control, token))
}

async fn open_media(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
    token: &str,
) -> anyhow::Result<tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
    let (media, _) = client_async(request, tls).await?;
    Ok(media)
}

async fn next_json(
    control: &mut tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    let text = message.into_text()?;
    Ok(serde_json::from_str(&text)?)
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn wait_absent(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "path did not disappear: {}", path.display());
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
const TOKEN: &str = "b7-access-token";

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
