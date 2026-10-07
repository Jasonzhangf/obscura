#![cfg(unix)]
//! F5 black-box regression: the real `obscura-endpoint` exercises its own
//! `Armed` encoder authority against the real `obscura-media` adapter.
//!
//! The two cases that motivated this file (`registration_blackbox.rs`) used the
//! scripted control peers `serve_control_pending_register` and
//! `serve_control_delayed_revoke`, which replace `EncoderControlState` and the
//! endpoint finalizer with canned answers. They never exercise the real
//! endpoint's `Armed` authority. These cases do:
//!
//! * `real_endpoint_round_trip_produces_access_unit` proves the full real
//!   round trip. The real adapter registers with the real endpoint, the real
//!   endpoint commits `Armed`, and the real adapter produces a real H.264
//!   access unit on the endpoint's `/media` stream. The only test helper is the
//!   external FFmpeg binary the adapter spawns.
//!
//! * `real_endpoint_armed_authority_signals_registered_group` pauses the real
//!   adapter (SIGSTOP) after the endpoint has committed the registration but
//!   before the adapter has consumed any revoke acknowledgement, then releases
//!   the final control reference. The endpoint finalizer must settle the group
//!   through its own `Armed` authority. Because the adapter is paused, the
//!   `SIGTERM` the group records can only come from the endpoint. The assertion
//!   reads public protocol output: the group's signal log and the endpoint's
//!   session-directory cleanup. No canned control peer, no replaced
//!   `EncoderControlState` and no replaced finalizer are involved.

use std::{cell::{Cell, RefCell}, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

type Socket = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

#[tokio::test]
async fn real_endpoint_round_trip_produces_access_unit() {
    tokio::time::timeout(Duration::from_secs(180), round_trip())
        .await
        .expect("F5 round-trip case timed out")
        .expect("F5 round-trip case failed");
}

#[tokio::test]
async fn real_endpoint_armed_authority_signals_registered_group() {
    tokio::time::timeout(Duration::from_secs(180), armed_authority())
        .await
        .expect("F5 armed-authority case timed out")
        .expect("F5 armed-authority case failed");
}

struct Fixture {
    root: PathBuf,
    ca_der: CertificateDer<'static>,
    client_der: CertificateDer<'static>,
    client_key: rcgen::KeyPair,
    session_id: RefCell<Option<String>>,
    address: Cell<Option<std::net::SocketAddr>>,
}

impl Fixture {
    fn new(prefix: &str) -> anyhow::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let root = PathBuf::from("/tmp").join(format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        let ca_key = rcgen::KeyPair::generate()?;
        let mut ca_params = rcgen::CertificateParams::new(vec![])?;
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b10 CA");
        let ca = ca_params.self_signed(&ca_key)?;
        let server_key = rcgen::KeyPair::generate()?;
        let server = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?
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
        Ok(Self { root, ca_der: ca.der().clone(), client_der: client.der().clone(), client_key, session_id: RefCell::new(None), address: Cell::new(None) })
    }

    fn connector(&self) -> anyhow::Result<tokio_rustls::TlsConnector> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca_der.clone())?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![self.client_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.client_key.serialize_der())),
            )?;
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
    }

    /// Spawn the real Host, attach an agent, resize, and navigate to a static
    /// red square so the exact session always has a frame to encode.
    async fn start_host(&self) -> anyhow::Result<tokio::process::Child> {
        let host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(self.root.join("host"))
            .arg("--profile-root").arg(self.root.join("profiles"))
            .arg("--authorization-registry").arg(self.root.join("registry.json"))
            .stdout(Stdio::null()).stderr(std::fs::File::create(self.root.join("host.err"))?)
            .kill_on_drop(true).spawn()?;
        wait_socket(self.root.join("host/host.sock")).await;
        let mut local = BufReader::new(tokio::net::UnixStream::connect(self.root.join("host/host.sock")).await?);
        let mut line = String::new();
        local.read_line(&mut line).await?;
        let html = "data:text/html,<style>body{margin:0;background:white}div{width:120px;height:120px;background:red}</style><div></div>";
        let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
        let session_id = state["session_id"].as_str().expect("Host session identity missing").to_string();
        state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
        let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
        drop(local);
        self.session_id.replace(Some(session_id));
        Ok(host)
    }

    fn session_id(&self) -> String {
        self.session_id.borrow().clone().expect("session id set after start_host")
    }

    fn endpoint(&self, ffmpeg: Option<&PathBuf>) -> anyhow::Result<tokio::process::Child> {
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let address = reservation.local_addr()?;
        drop(reservation);
        self.address.replace(Some(address));
        let media_bin = release_binary("obscura-media");
        anyhow::ensure!(media_bin.is_file(), "OBSCURA_TEST_BINARY_DIR must contain obscura-media: {}", media_bin.display());
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
        command.arg("--listen").arg(address.to_string())
            .arg("--host-dir").arg(self.root.join("host"))
            .arg("--socket-dir").arg(self.root.join("endpoint"))
            .arg("--server-cert").arg(self.root.join("server.der"))
            .arg("--server-key").arg(self.root.join("server.key"))
            .arg("--client-ca").arg(self.root.join("ca.der"))
            .arg("--authorized-clients").arg(self.root.join("authorized-clients.json"))
            .arg("--media-bin").arg(&media_bin);
        if let Some(ffmpeg) = ffmpeg { command.arg("--ffmpeg").arg(ffmpeg); }
        command.stdout(Stdio::null()).stderr(std::fs::File::create(self.root.join("endpoint.err"))?)
            .kill_on_drop(true).spawn().map_err(Into::into)
    }

    fn address(&self) -> std::net::SocketAddr {
        self.address.get().expect("endpoint address set after endpoint()")
    }
}


async fn round_trip() -> anyhow::Result<()> {
    let fixture = Fixture::new("ob10a")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;
    let mut endpoint = fixture.endpoint(None)?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    let mut media = open_media(&connector, fixture.address(), &token).await?;
    let header = tokio::time::timeout(Duration::from_secs(60), next_access_unit(&mut media)).await
        .map_err(|_| anyhow::anyhow!("the real endpoint never delivered an encoded access unit"))??;
    anyhow::ensure!(header["type"] == "access_unit", "first media packet was not an access unit: {header}");
    eprintln!("F5 round trip: real endpoint + real adapter produced a real access unit");

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

async fn armed_authority() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("ob10b")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;

    // A real encoder group helper: it records its own process-group identity and
    // the adapter parent, records every signal it receives, and blocks without
    // producing an access unit. Blocking keeps the adapter's registration
    // `Armed` (mid encode) so the endpoint finalizer, not the adapter, settles
    // the group.
    let group_pgid = root.join("group.pgid");
    let adapter_pid_file = root.join("adapter.pid");
    let group_signals = root.join("group.signals");
    let helper = root.join("group.sh");
    let script = format!(
        "#!/bin/sh\nppid=$(ps -o ppid= -p $$ | tr -d ' ')\nprintf '%s\\n' \"$$\" > '{pgid}'\nprintf '%s\\n' \"$ppid\" > '{adapter}'\ntrap 'printf \"15\\n\" >> \"{signals}\"; exit 0' TERM\nwhile :; do sleep 0.2; done\n",
        pgid = group_pgid.display(),
        adapter = adapter_pid_file.display(),
        signals = group_signals.display(),
    );
    std::fs::write(&helper, script)?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;

    let mut endpoint = fixture.endpoint(Some(&helper))?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    // The real adapter spawns the group only after the endpoint admitted the
    // session and streamed the latest frame to it.
    wait_file(&group_pgid, Duration::from_secs(60)).await?;
    wait_file(&adapter_pid_file, Duration::from_secs(15)).await?;
    let adapter_pid: i32 = std::fs::read_to_string(&adapter_pid_file)?.trim().parse()?;
    anyhow::ensure!(process_alive(adapter_pid), "the real adapter parent was not alive");
    // Let the adapter send `Register` (the endpoint commits `Armed`) and enter
    // the blocked encode. It has not sent any revoke yet.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pause the real adapter so the only process that can signal the registered
    // group is the endpoint's own `Armed` authority.
    anyhow::ensure!(unsafe { libc::kill(adapter_pid, libc::SIGSTOP) } == 0, "SIGSTOP the real adapter");

    // Release the final control reference: the endpoint runs the exact-session
    // finalizer. It must settle the registered group through its own `Armed`
    // authority (no adapter cooperation, since the adapter is paused).
    control.send(Message::Close(None)).await?;
    drop(control);

    let signals = wait_signals(&group_signals, Duration::from_secs(20)).await?;
    anyhow::ensure!(signals.trim() == "15",
        "F5: the endpoint's Armed authority did not deliver exactly one SIGTERM to the registered group: {signals:?}");
    anyhow::ensure!(!process_group_alive(read_pgid(&group_pgid)?), "F5: the registered group survived the endpoint finalizer");

    let session_dir = root.join(format!("endpoint/session-{}", fixture.session_id()));
    wait_dir_gone(&session_dir, Duration::from_secs(20)).await?;
    eprintln!("F5 armed authority: endpoint settled the registered group through its own Armed state");

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn read_pgid(path: &std::path::Path) -> anyhow::Result<i32> {
    Ok(std::fs::read_to_string(path)?.trim().parse()?)
}

fn process_alive(pid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn process_group_alive(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

async fn wait_signals(path: &std::path::Path, timeout: Duration) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if !text.trim().is_empty() { return Ok(text); }
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "the registered group never recorded a signal: {text:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_dir_gone(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "the endpoint finalizer returned before removing the session directory: {}", path.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
}

async fn connect_control(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> anyhow::Result<(Socket, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let (control, response) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await?;
    let token = response.headers()["x-obscura-media-token"].to_str()?.to_string();
    Ok((control, token))
}

async fn open_media(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
    token: &str,
) -> anyhow::Result<Socket> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
    let (media, _) = client_async(request, tls).await?;
    Ok(media)
}

async fn next_json(control: &mut Socket) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    Ok(serde_json::from_str(&message.into_text()?)?)
}

/// Read one `/media` frame header (4-byte big-endian length, then JSON).
async fn next_access_unit(media: &mut Socket) -> anyhow::Result<serde_json::Value> {
    loop {
        let message = media.next().await.ok_or_else(|| anyhow::anyhow!("media stream ended before an access unit"))??;
        let Message::Binary(bytes) = message else { continue; };
        anyhow::ensure!(bytes.len() >= 4, "media frame header is truncated");
        let length = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
        anyhow::ensure!(bytes.len() >= 4 + length, "media frame payload is truncated");
        let header: serde_json::Value = serde_json::from_slice(&bytes[4..4 + length])?;
        if header["type"] == "access_unit" { return Ok(header); }
    }
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
        "host_id": "host-b10",
        "devices": [{
            "device_id": "test-device",
            "device_label": "b10 client",
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
