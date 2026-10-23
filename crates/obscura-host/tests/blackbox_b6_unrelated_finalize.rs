#![cfg(unix)]
//! B6 black-box regression: a fresh remote admission must not be serialized
//! behind another session's slow finalize. Two real sessions run against one
//! `obscura-endpoint`; peer H keeps the catalog default session bound, and peer
//! A moves to the second profile so A's adapter is the one that finalizes
//! slowly. While A's adapter is still alive, a fresh admission to the still
//! bound default session must be served. The observable is external: the
//! admission and attach responses arrive while A's adapter process is alive,
//! instead of only after A's slow supervisor wait completes.
//!
//! Everything here is external to the endpoint: real TLS/WSS control with the
//! one access token, the real per-session `--media-bin` adapter process under
//! `--socket-dir`, and real process liveness. No private state, counters or
//! source structure are asserted.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

#[tokio::test]
async fn admission_is_not_serialized_behind_an_unrelated_finalize() {
    tokio::time::timeout(Duration::from_secs(180), exercise())
        .await
        .expect("B6 unrelated-finalize case timed out")
        .expect("B6 unrelated-finalize case failed");
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
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("ca.der"), ca.der())?;
    write_two_profile_catalog(&root)?;

    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--access-token").arg(TOKEN)
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
    let session_p1 = bound["status"]["session_id"].as_str().expect("second session identity missing").to_string();
    drop(local);

    let reservation = std::net::TcpListener::bind((bind_ip, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);

    // A real adapter that keeps the endpoint's ingest reader open (it connects
    // to the `--video-socket` it is handed) and stays alive for five seconds
    // after SIGTERM, so session A's supervisor wait is observable from the
    // outside. The handler records the direct adapter pid and the observation
    // marker only when the endpoint signals it; the default session's adapter
    // never overwrites the file.
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
        .arg("--media-bin").arg(&media_bin)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(root.join("endpoint.err"))?)
        .kill_on_drop(true)
        .spawn()?;
    wait_socket(root.join("endpoint")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone())?;
    let connector_a = tls_connector(&roots)?;
    let connector_h = tls_connector(&roots)?;
    let connector_b = tls_connector(&roots)?;

    // The admission frame carries no profile selection, so every peer is
    // admitted to the catalog default session. Peer H keeps that session bound
    // for the whole case. Peer A moves to the second profile, so A's adapter is
    // the one that finalizes slowly below.
    let (mut control_h, _token_h, session_default) = connect_observe(&connector_h, address).await?;
    let (control_a, _token_a, session_a) = connect_bound(&connector_a, address, "p1").await?;
    anyhow::ensure!(session_a == session_p1, "A bound the wrong session: {session_a} != {session_p1}");
    anyhow::ensure!(session_a != session_default, "the two profiles resolved to one session");

    let dir_a = root.join(format!("endpoint/m-{}", &session_a.replace("-", "")[..12]));
    anyhow::ensure!(wait_dir(&dir_a, Duration::from_secs(10)).await, "A session directory was never bound");
    // A's adapter records readiness after installing the SIGTERM handler and
    // connecting to the exact-session socket, so the abrupt close below cannot
    // race Python startup and miss the trapped signal.
    wait_file(&dir_a.join("encoded.sock.ready"), Duration::from_secs(15)).await?;

    // Abruptly close A's control connection. The endpoint releases A's exact
    // session reference and finalizes A: it SIGTERMs the adapter, which traps
    // TERM, records its pid, touches the marker and then stays alive for five
    // seconds. The marker proves A's slow supervisor wait is in progress.
    drop(control_a);
    wait_file(&marker, Duration::from_secs(15)).await?;
    let pid_a: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    anyhow::ensure!(process_alive(pid_a), "A's adapter was not alive when its supervisor wait began (pid {pid_a})");

    // A fresh admission to the *other*, still bound session must be served while
    // A is finalizing: the default session is already bound by peer H, so this
    // admission must not wait for A's supervisor wait to finish.
    let started = tokio::time::Instant::now();
    let (mut probe, _probe_token, probe_session) = connect_observe(&connector_b, address).await?;
    let elapsed = started.elapsed();
    anyhow::ensure!(probe_session == session_default, "the probe bound the wrong session: {probe_session}");

    let a_still_alive = process_alive(pid_a);
    eprintln!("B6: unrelated-session admission completed in {elapsed:?}; A adapter alive={a_still_alive} pid={pid_a}");
    anyhow::ensure!(elapsed < Duration::from_secs(2),
        "the unrelated session was served only after session A's slow supervisor wait finished ({elapsed:?})");
    anyhow::ensure!(a_still_alive,
        "the unrelated session was served only after session A's adapter was reaped");

    let _ = &mut control_h;
    let _ = &mut probe;
    graceful_stop(&mut endpoint).await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn tls_connector(roots: &rustls::RootCertStore) -> anyhow::Result<tokio_rustls::TlsConnector> {
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots.clone())
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
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

async fn next_json(
    control: &mut tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    Ok(serde_json::from_str(&message.into_text()?)?)
}

/// Admit one peer and attach it as an observer of its admitted session.
async fn connect_observe(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> anyhow::Result<(tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String, String)> {
    let (mut control, token) = connect_control(connector, address).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["type"] == "ready", "admission did not publish Ready: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");
    let session_id = attached["value"]["session_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("attach returned no session identity: {attached}"))?.to_string();
    Ok((control, token, session_id))
}

/// Admit one peer, attach it as an observer, then bind it to `profile_id` with
/// an operation identity for its own attachment. Returns the bound session.
async fn connect_bound(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
    profile_id: &str,
) -> anyhow::Result<(tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, String, String)> {
    let (mut control, token) = connect_control(connector, address).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["type"] == "ready", "admission did not publish Ready: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");
    let operation = identity(&attached["value"]);
    let request = serde_json::json!({"id":2,"operation":operation,
        "command":{"type":"profile_select","profile_id":profile_id}});
    control.send(Message::text(request.to_string())).await?;
    let selected = next_json(&mut control).await?;
    anyhow::ensure!(selected["type"] == "result", "profile_select failed: {selected}");
    let session_id = selected["value"]["binding"]["session_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("profile_select returned no session binding: {selected}"))?.to_string();
    Ok((control, token, session_id))
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

/// The one access token this test configures on the Host.
const TOKEN: &str = "b6-access-token";

/// Two profiles, so each peer can resolve to its own exact session. The
/// catalog carries no membership and no access record.
fn write_two_profile_catalog(root: &std::path::Path) -> anyhow::Result<()> {
    let catalog = serde_json::json!({
        "catalog_revision": 2,
        "profiles": [
            {"profile_id": "default", "display_name": "Default", "default": true},
            {"profile_id": "p1", "display_name": "Secondary", "default": false}
        ],
        "default_profile_id": "default",
        "next_id": 2
    });
    std::fs::create_dir_all(root.join("profiles"))?;
    std::fs::write(root.join("profiles/catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}
