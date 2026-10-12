#![cfg(unix)]
//! B9 black-box regression (F3): an acquired encoder permit that never binds a
//! pgid must remain an explicit failure, not become successful resource
//! cleanup.
//!
//! The test drives real mTLS control against a real `obscura-endpoint` and a
//! real `--media-bin` adapter. It acts as the adapter on the private
//! `encoder-control.sock`: it acquires a permit (granted) and then never sends
//! `Register`, so the endpoint has no signal authority over any group. It then
//! closes the control attachment, which is the real final-reference release.
//! Pre-fix the finalizer silently ignored the unsettled permit, dropped the
//! durable (SIGKILLing the retained adapter) and reported successful cleanup, so
//! the endpoint then shut down with status zero. Post-fix the endpoint keeps the
//! durable resource owner and the live adapter, surfaces
//! `SessionTerminal::EncoderPermitUnsettled` through the release, retains the
//! entry, and reports the per-session failure again through shutdown. The
//! external observables are the retained adapter process, the retained session
//! directory and the endpoint's own nonzero exit status; no endpoint-private
//! state is asserted.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use obscura_media::encoder_owner::{EncoderControlRequest, EncoderControlResponse, EncoderPermit};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::ServerName};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

#[tokio::test]
async fn unsettled_encoder_permit_is_an_explicit_terminal() {
    tokio::time::timeout(Duration::from_secs(120), exercise())
        .await
        .expect("B9 public unsettled-permit case timed out")
        .expect("B9 public unsettled-permit case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob9-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b9 CA");
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
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:80px;height:80px;background:red'></div>";
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);

    let reservation = std::net::TcpListener::bind((bind_ip, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);

    // A real adapter that connects to the exact-session video socket, records
    // its own pid and a same-owner descendant, and traps SIGTERM without
    // exiting, so the endpoint's cooperative adapter grace expires while the
    // permit is unsettled. The retained owner must keep this adapter (and its
    // descendant) alive until the endpoint itself is torn down.
    let marker = root.join("adapter-signalled");
    let pid_file = root.join("adapter.pid");
    let child_file = root.join("adapter-child.pid");
    let media_bin = root.join("adapter.py");
    let script = [
        "#!/usr/bin/env python3",
        "import os, signal, socket, subprocess, sys, time",
        "args = sys.argv[1:]",
        "video = args[args.index('--video-socket') + 1]",
        &format!("marker = {mark:?}", mark = marker.display().to_string()),
        &format!("pid_file = {pid:?}", pid = pid_file.display().to_string()),
        &format!("child_file = {child:?}", child = child_file.display().to_string()),
        "with open(pid_file, 'w') as handle:",
        "    handle.write(str(os.getpid()))",
        "descendant = subprocess.Popen(['sleep', '300'])",
        "with open(child_file, 'w') as handle:",
        "    handle.write(str(descendant.pid))",
        "def on_term(signum, frame):",
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

    let stderr_path = root.join("endpoint.err");
    let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string())
        .arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der"))
        .arg("--server-key").arg(root.join("server.key"))
        .arg("--media-bin").arg(&media_bin)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path)?)
        .kill_on_drop(true)
        .spawn()?;
    wait_socket(root.join("endpoint")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone())?;
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    let (mut control, _token) = connect_control(&connector, address).await?;
    let ready = next_json(&mut control).await?;
    let session_id = ready["session_id"].as_str().expect("Endpoint Ready session identity missing").to_string();
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "attach failed: {attached}");

    let control_socket = root.join(format!("endpoint/m-{}/encoder-control.sock", &session_id.replace("-", "")[..12]));
    wait_socket(control_socket.clone()).await;

    // Act as the adapter: acquire a permit and never bind a pgid through
    // `Register`. The endpoint therefore has no signal authority over any group.
    let stream = tokio::net::UnixStream::connect(&control_socket).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let acquired = exchange(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 41 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "encoder acquire was not granted: {acquired:?}");

    // Wait for the adapter's real identity and descendant before triggering the
    // release, so the retained-ownership observation is grounded.
    wait_file(&pid_file, Duration::from_secs(15)).await?;
    wait_file(&child_file, Duration::from_secs(15)).await?;
    let adapter_pid: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    let descendant_pid: i32 = std::fs::read_to_string(&child_file)?.trim().parse()?;
    anyhow::ensure!(process_alive(adapter_pid), "fixture adapter was not alive before the release");

    // Close the control attachment. This is the final reference, so the endpoint
    // runs the exact-session finalizer. The permit stays unsettled, so the
    // release must remain an explicit failure that retains the durable owner.
    drop(control);
    wait_stderr(&stderr_path, "SESSION_ENCODER_PERMIT_UNSETTLED", Duration::from_secs(40)).await?;
    // Give the failed release a moment to finish dropping nothing: on the fixed
    // path the adapter must still be alive, on the reverted path kill_on_drop
    // has already SIGKILLed it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let adapter_retained = process_alive(adapter_pid);
    let descendant_retained = process_alive(descendant_pid);
    let session_dir = root.join(format!("endpoint/m-{}", &session_id.replace("-", "")[..12]));
    // The failed release must retain the durable resource owner: the session
    // directory and its control path stay on disk while the endpoint is still
    // alive. This observation is taken before endpoint shutdown, because the
    // endpoint's own socket-directory guard removes the whole tree on exit.
    let session_dir_retained = session_dir.exists();
    let control_path_retained = session_dir.join("encoder-control.sock").exists();

    // F2: a new attachment to the same session must learn the retained
    // finalization failure instead of waiting for a finalization that will
    // never complete. Drive a fresh real mTLS control attachment and attach in
    // observe mode; the endpoint host-admits and Ready advertises the same
    // session, but the exact media binding acquire must return the retained
    // failure terminal.
    let (mut reattach, _reattach_token) = connect_control(&connector, address).await?;
    let reattach_ready = next_json(&mut reattach).await?;
    anyhow::ensure!(reattach_ready["session_id"].as_str() == Some(session_id.as_str()),
        "F2: reattach Ready session changed: {reattach_ready}");
    reattach.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let reattach_result = tokio::time::timeout(Duration::from_secs(20), next_json(&mut reattach)).await
        .map_err(|_| anyhow::anyhow!("F2: reattachment waited for the retained finalization failure instead of returning it"))??;
    eprintln!("B9 F2 reattach terminal: {reattach_result}");
    anyhow::ensure!(reattach_result["type"] == "error"
            && reattach_result["code"] == "SESSION_MEDIA_FINALIZE_FAILED",
        "F2: a new attachment to a retained finalization failure did not return the explicit failure terminal: {reattach_result}");
    anyhow::ensure!(reattach_result["message"].as_str().is_some_and(|message| message.contains("SESSION_ENCODER_PERMIT_UNSETTLED")),
        "F2: the retained failure terminal did not carry the exact encoder-permit terminal: {reattach_result}");
    // The preserved durable owner must remain unchanged by the refused
    // reattachment.
    anyhow::ensure!(process_alive(adapter_pid), "F2: the retained adapter was dropped while answering the reattachment");
    anyhow::ensure!(session_dir.exists(), "F2: the retained session directory was removed while answering the reattachment");

    // SIGTERM the endpoint. Shutdown must surface the retained per-session
    // failure and exit nonzero. Both the release terminal and the shutdown
    // terminal carry the registration id.
    if let Some(pid) = endpoint.id() {
        // Safety: signaling the one endpoint process this test spawned.
        let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    let status = tokio::time::timeout(Duration::from_secs(40), endpoint.wait()).await
        .map_err(|_| anyhow::anyhow!("endpoint did not exit after SIGTERM"))??;
    drop(write);

    let stderr = std::fs::read_to_string(&stderr_path)?;
    anyhow::ensure!(stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "endpoint did not surface an unsettled-permit terminal:\n{stderr}");
    anyhow::ensure!(stderr.contains("41"),
        "unsettled-permit terminal did not carry the registration id:\n{stderr}");
    // The core G1 resource endpoint: a failed release retains the durable
    // owner, so the adapter and its separately spawned group stay alive and
    // the session directory and control path stay on disk. Pre-fix the release
    // removed the entry, dropped the owner, SIGKILLed the adapter and published
    // successful cleanup.
    anyhow::ensure!(adapter_retained,
        "G1: the failed release dropped the durable owner and SIGKILLed the retained adapter");
    anyhow::ensure!(descendant_retained,
        "G1: the retained adapter's separately spawned group was not retained with its owner");
    anyhow::ensure!(session_dir_retained,
        "G1: the failed release removed the retained session directory instead of preserving it");
    anyhow::ensure!(control_path_retained,
        "G1: the failed release removed the retained encoder-control path");
    anyhow::ensure!(!status.success(),
        "endpoint shutdown returned success while an encoder permit was unsettled: {status}");
    anyhow::ensure!(stderr.contains("ENDPOINT_SESSION_FINALIZE_FAILED"),
        "shutdown did not surface the retained per-session finalize failure:\n{stderr}");

    // The fixture adapter and its descendant are test-owned. Endpoint teardown
    // drops the retained owner and SIGKILLs the adapter direct child; the
    // descendant is not the endpoint's resource, so reap it by explicit pid.
    unsafe { libc::kill(descendant_pid, libc::SIGKILL); }
    let _ = wait_pid_gone(adapter_pid, Duration::from_secs(5)).await;
    let _ = wait_pid_gone(descendant_pid, Duration::from_secs(5)).await;

    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

async fn exchange(
    read: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    request: EncoderControlRequest,
) -> anyhow::Result<EncoderControlResponse> {
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line)).await
        .map_err(|_| anyhow::anyhow!("encoder control response deadline"))??;
    Ok(serde_json::from_str(line.trim_end())?)
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

async fn wait_stderr(path: &std::path::Path, needle: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.contains(needle) { return Ok(()); }
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "endpoint never reported {needle}:\n{}", std::fs::read_to_string(path).unwrap_or_default());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn process_alive(pid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while process_alive(pid) {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

/// The one access token this test configures on the Host.
const TOKEN: &str = "b9-access-token";

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
