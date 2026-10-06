#![cfg(unix)]
//! B1 black-box regression on the real WebRTC DataChannel browser-operation
//! path.
//!
//! Two authorized observers share one exact Host session. Each opens the real
//! endpoint `/media` WSS stream through its own mTLS control attachment. Observer
//! A then drives a browser request over a real WebRTC DataChannel; the Host
//! denies it with a correlated `REMOTE_PRINCIPAL_REVOKED` after A's principal is
//! revoked in the committed registry.
//!
//! Externally observable fence: the denied attachment's already-open `/media`
//! stream must end on that correlated denial while observer B keeps receiving
//! frames on the shared session. The pre-fix DataChannel branch classified the
//! denial with `response_ends_media`, which is false for a denial, so it never
//! fenced the attachment-local media admission and A's `/media` stream kept
//! delivering until the control connection itself ended. This case asserts only
//! wire bytes, a correlated response and stream liveness; it reads no private
//! state, counter or source structure.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, Request, Response, WebRtcCapability, WebRtcControlMessage,
    WebRtcSignal, WebRtcTransport, WebRtcVideoCodec, WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION};
use rtc::rtp_transceiver::{rtp_sender::RtpCodecKind, RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use sha2::{Digest, Sha256};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::TcpListener, sync::Notify};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}, WebSocketStream};
use webrtc_rs::{data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfiguration, RTCIceGatheringState, RTCSessionDescription}};

type Socket = WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

const DEADLINE: Duration = Duration::from_secs(20);
const TRIGGER_REQUEST_ID: u64 = 200;

#[tokio::test]
async fn datachannel_denial_fences_only_the_denied_attachment_media() {
    tokio::time::timeout(Duration::from_secs(180), exercise())
        .await
        .expect("B1 DataChannel denial-fence case timed out")
        .expect("B1 DataChannel denial-fence case failed");
}

async fn exercise() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ob1dc-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let bind_ip = "127.0.0.1";

    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b1 DataChannel CA");
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
    write_shared_session_fixtures(&root, &client_a.der().to_vec(), &client_b.der().to_vec())?;

    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .kill_on_drop(true).spawn()?;
    wait_socket(root.join("host/host.sock")).await;
    let mut local = BufReader::new(tokio::net::UnixStream::connect(root.join("host/host.sock")).await?);
    let mut line = String::new();
    local.read_line(&mut line).await?;
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let session_id = state["session_id"].as_str().expect("Host session identity missing").to_string();
    let html = "data:text/html,<style>body{margin:0;background:white}div{width:120px;height:120px;background:red}</style><div></div>";
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let media_bin = release_binary("obscura-media");
    anyhow::ensure!(media_bin.is_file(), "Build target/release/obscura-media with --features webrtc first");
    let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string())
        .arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der"))
        .arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--media-bin").arg(&media_bin)
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(root.join("endpoint.err"))?)
        .kill_on_drop(true).spawn()?;
    wait_socket(root.join("endpoint")).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone())?;
    let connector_a = tls_connector(&roots, client_a.der().clone(), &client_a_key)?;
    let connector_b = tls_connector(&roots, client_b.der().clone(), &client_b_key)?;

    let (mut control_a, token_a) = connect_control(&connector_a, address).await?;
    anyhow::ensure!(ready_session(&next_json(&mut control_a).await?)? == session_id, "A Ready session changed");
    attach_observe(&mut control_a).await?;
    let mut video_a = open_media(&connector_a, address, &token_a).await?;

    let (mut control_b, token_b) = connect_control(&connector_b, address).await?;
    anyhow::ensure!(ready_session(&next_json(&mut control_b).await?)? == session_id, "B Ready session changed");
    attach_observe(&mut control_b).await?;
    let mut video_b = open_media(&connector_b, address, &token_b).await?;

    // Both streams must be live before the denial, otherwise a premature close
    // would make the fence assertion meaningless.
    next_access_unit(&mut video_a, Duration::from_secs(30)).await?;
    next_access_unit(&mut video_b, Duration::from_secs(30)).await?;

    // Observer A brings up the real WebRTC DataChannel browser-operation path.
    let datachannel = open_datachannel(&mut control_a, bind_ip).await?;

    // Revoke observer A's principal in the committed registry. The Host re-reads
    // this file for every remote command, so A's next browser request is denied.
    revoke_device(&root, "device-a");
    send_control(&datachannel, &WebRtcControlMessage::BrowserRequest {
        request: Request { id: TRIGGER_REQUEST_ID, command: Command::Status {}, operation: None },
    }).await?;
    match wait_for_browser_response(&datachannel, TRIGGER_REQUEST_ID).await? {
        Response::Error { code, .. } => anyhow::ensure!(code == "REMOTE_PRINCIPAL_REVOKED",
            "unexpected DataChannel denial code: {code}"),
        other => anyhow::bail!("DataChannel request was not denied: {other:?}"),
    }

    // Externally observable fence: the denied attachment's already-open `/media`
    // stream ends on the correlated denial. Pre-fix this stream kept delivering
    // access units because the DataChannel branch never fenced the media state.
    let fenced = media_terminates_within(&mut video_a, Duration::from_secs(8)).await;
    anyhow::ensure!(fenced, "the denied observer's /media stream survived a correlated DataChannel authorization denial");

    // Observer B is untouched and keeps receiving frames for the shared session.
    next_access_unit(&mut video_b, Duration::from_secs(20)).await
        .map_err(|error| anyhow::anyhow!("observer B stopped after A was denied: {error}"))?;

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn tls_connector(
    roots: &rustls::RootCertStore,
    cert: CertificateDer<'static>,
    key: &rcgen::KeyPair,
) -> anyhow::Result<tokio_rustls::TlsConnector> {
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots.clone())
        .with_client_auth_cert(vec![cert], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))?;
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

async fn connect_control(connector: &tokio_rustls::TlsConnector, address: std::net::SocketAddr) -> anyhow::Result<(Socket, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let (control, response) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await?;
    let token = response.headers()["x-obscura-media-token"].to_str()?.to_string();
    Ok((control, token))
}

async fn open_media(connector: &tokio_rustls::TlsConnector, address: std::net::SocketAddr, token: &str) -> anyhow::Result<Socket> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
    let (media, _) = client_async(request, tls).await?;
    Ok(media)
}

async fn attach_observe(control: &mut Socket) -> anyhow::Result<()> {
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let reply = next_json(control).await?;
    anyhow::ensure!(reply["type"] == "result", "observe attach failed: {reply}");
    Ok(())
}

async fn next_json(control: &mut Socket) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    Ok(serde_json::from_str(&message.into_text()?)?)
}

fn ready_session(value: &serde_json::Value) -> anyhow::Result<String> {
    anyhow::ensure!(value["type"] == "ready", "expected Ready, got {value}");
    value["session_id"].as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("Ready session missing: {value}"))
}

/// Read one `/media` access unit. The stream must be live before the denial.
async fn next_access_unit(video: &mut Socket, timeout: Duration) -> anyhow::Result<serde_json::Value> {
    tokio::time::timeout(timeout, async {
        loop {
            let message = video.next().await.ok_or_else(|| anyhow::anyhow!("media stream ended before an access unit"))??;
            let Message::Binary(bytes) = message else { continue; };
            let header = media_packet_header(&bytes)?;
            if header["type"] == "access_unit" { return Ok(header); }
        }
    }).await.map_err(|_| anyhow::anyhow!("timed out waiting for a /media access unit"))?
}

/// True when the `/media` stream reaches a terminal (explicit end, close or read
/// error) within the bound. Access units received while waiting are ignored.
async fn media_terminates_within(video: &mut Socket, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() { return false; }
        match tokio::time::timeout(remaining, video.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return true,
            Ok(Some(Ok(_))) => continue,
            Err(_) => return false,
        }
    }
}

fn media_packet_header(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(bytes.len() >= 4, "media frame header is truncated");
    let length = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
    anyhow::ensure!(bytes.len() >= 4 + length, "media frame payload is truncated");
    Ok(serde_json::from_slice(&bytes[4..4 + length])?)
}

/// Drive the real `/control` signaling exchange and return the authenticated
/// DataChannel. The endpoint accepts one WebRTC offer per control attachment.
async fn open_datachannel(control: &mut Socket, bind_ip: &str) -> anyhow::Result<Arc<dyn DataChannel>> {
    let capability = WebRtcCapability {
        protocol_version: WEBRTC_PROTOCOL_VERSION,
        transport: WebRtcTransport::Udp,
        video_codec: WebRtcVideoCodec::H264AnnexB,
        data_channel_label: WEBRTC_CONTROL_LABEL.to_owned(),
        max_access_unit: 4 * 1024 * 1024,
    };
    let gather = Arc::new(Notify::new());
    let peer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfiguration::default())
            .with_media_engine(obscura_media::webrtc::h264_media_engine()?)
            .with_handler(Arc::new(GatherHandler { gather: Arc::clone(&gather) }))
            .with_udp_addrs(vec![std::net::SocketAddr::new(bind_ip.parse()?, 0)])
            .build()
            .await?,
    );
    peer.add_transceiver_from_kind(RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default() })).await?;
    let datachannel = peer.create_data_channel(WEBRTC_CONTROL_LABEL, None).await?;
    let offer = peer.create_offer(None).await?;
    peer.set_local_description(offer).await?;
    tokio::time::timeout(DEADLINE, gather.notified()).await
        .map_err(|_| anyhow::anyhow!("timed out waiting for receiver ICE gathering"))?;
    let offer = peer.local_description().await.ok_or_else(|| anyhow::anyhow!("receiver WebRTC offer missing"))?;
    send_control_signal(control, &WebRtcSignal::Offer { id: 2, capability: capability.clone(), sdp: offer.sdp }).await?;

    let answer = loop {
        let message = control.next().await.ok_or_else(|| anyhow::anyhow!("signaling socket closed"))??;
        let signal: WebRtcSignal = serde_json::from_str(&message.into_text()?)?;
        match signal {
            WebRtcSignal::Answer { id, .. } => { anyhow::ensure!(id == 2, "signaling answer id mismatch"); break signal; }
            WebRtcSignal::Error { code, message, .. } => anyhow::bail!("WebRTC signaling rejected: {code}: {message}"),
            WebRtcSignal::Offer { .. } => anyhow::bail!("endpoint echoed an offer"),
        }
    };
    let WebRtcSignal::Answer { capability: answer_capability, binding, sdp, .. } = answer else {
        anyhow::bail!("Host did not return a WebRTC answer");
    };
    anyhow::ensure!(answer_capability == capability, "Host answer capability differs from offer");
    anyhow::ensure!(!binding.session_id.is_empty() && !binding.auth_binding.is_empty(), "Host answer binding is incomplete");
    peer.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
    wait_for_open(&datachannel).await?;
    send_control(&datachannel, &WebRtcControlMessage::Hello { capability: capability.clone(), binding }).await?;
    expect_hello_ack(&datachannel, &capability).await?;
    send_control(&datachannel, &WebRtcControlMessage::Ping { request_id: 3 }).await?;
    expect_pong(&datachannel, 3).await?;
    Ok(datachannel)
}

async fn wait_for_open(channel: &Arc<dyn DataChannel>) -> anyhow::Result<()> {
    loop {
        let event = tokio::time::timeout(DEADLINE, channel.poll()).await
            .map_err(|_| anyhow::anyhow!("timed out waiting for DataChannel open"))?
            .ok_or_else(|| anyhow::anyhow!("DataChannel ended before open"))?;
        match event {
            DataChannelEvent::OnOpen => return Ok(()),
            DataChannelEvent::OnError => anyhow::bail!("DataChannel error before open"),
            DataChannelEvent::OnClose => anyhow::bail!("DataChannel closed before open"),
            _ => {}
        }
    }
}

async fn expect_hello_ack(channel: &Arc<dyn DataChannel>, capability: &WebRtcCapability) -> anyhow::Result<()> {
    loop {
        match next_control(channel, "HelloAck").await? {
            WebRtcControlMessage::HelloAck { capability: ack, .. } => {
                anyhow::ensure!(ack == *capability, "Host HelloAck capability mismatch");
                return Ok(());
            }
            WebRtcControlMessage::VideoFrame { .. } => continue,
            WebRtcControlMessage::Error { code, message } => anyhow::bail!("Host WebRTC error {code}: {message}"),
            other => anyhow::bail!("unexpected DataChannel message while waiting for HelloAck: {other:?}"),
        }
    }
}

async fn expect_pong(channel: &Arc<dyn DataChannel>, request_id: u64) -> anyhow::Result<()> {
    loop {
        match next_control(channel, "Pong").await? {
            WebRtcControlMessage::Pong { request_id: received } => {
                anyhow::ensure!(received == request_id, "WebRTC Pong id mismatch");
                return Ok(());
            }
            WebRtcControlMessage::VideoFrame { .. } => continue,
            WebRtcControlMessage::Error { code, message } => anyhow::bail!("Host WebRTC error {code}: {message}"),
            other => anyhow::bail!("unexpected DataChannel message while waiting for Pong: {other:?}"),
        }
    }
}

async fn wait_for_browser_response(channel: &Arc<dyn DataChannel>, request_id: u64) -> anyhow::Result<Response> {
    loop {
        match next_control(channel, "BrowserResponse").await? {
            WebRtcControlMessage::BrowserResponse { response } => {
                let id = match &response {
                    Response::Result { id, .. } | Response::Error { id, .. } => Some(*id),
                    Response::Ready { .. } => None,
                };
                anyhow::ensure!(id == Some(request_id), "DataChannel response id mismatch: {response:?}");
                return Ok(response);
            }
            WebRtcControlMessage::VideoFrame { .. } => continue,
            WebRtcControlMessage::Error { code, message } => anyhow::bail!("Host WebRTC error {code}: {message}"),
            other => anyhow::bail!("unexpected DataChannel message while waiting for browser response: {other:?}"),
        }
    }
}

async fn next_control(channel: &Arc<dyn DataChannel>, description: &str) -> anyhow::Result<WebRtcControlMessage> {
    loop {
        let event = tokio::time::timeout(DEADLINE, channel.poll()).await
            .map_err(|_| anyhow::anyhow!("timed out waiting for WebRTC {description}"))?
            .ok_or_else(|| anyhow::anyhow!("WebRTC DataChannel ended while waiting for {description}"))?;
        match event {
            DataChannelEvent::OnMessage(message) => return Ok(serde_json::from_slice(&message.data)?),
            DataChannelEvent::OnError => anyhow::bail!("WebRTC DataChannel error while waiting for {description}"),
            DataChannelEvent::OnClose => anyhow::bail!("WebRTC DataChannel closed while waiting for {description}"),
            _ => {
                // The WebRTC implementation may deliver non-message lifecycle
                // events (for example OnOpen again or buffered-amount updates)
                // between control messages. They do not change the typed
                // control stream.
            }
        }
    }
}

async fn send_control(channel: &Arc<dyn DataChannel>, value: &WebRtcControlMessage) -> anyhow::Result<()> {
    channel.send_text(&serde_json::to_string(value)?).await?;
    Ok(())
}

async fn send_control_signal(control: &mut Socket, value: &WebRtcSignal) -> anyhow::Result<()> {
    control.send(Message::text(serde_json::to_string(value)?)).await?;
    Ok(())
}

struct GatherHandler {
    gather: Arc<Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for GatherHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather.notify_one();
        }
    }
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
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

/// Registry, projection and catalog for two paired devices, each granted the
/// `default_profile` so both remote attachments resolve to one exact session.
fn write_shared_session_fixtures(root: &std::path::Path, client_a: &[u8], client_b: &[u8]) -> anyhow::Result<()> {
    let fingerprint_a = hex(&Sha256::digest(client_a));
    let fingerprint_b = hex(&Sha256::digest(client_b));
    let scopes = ["observe_human_control", "profile_read", "profile_create", "profile_select"];
    let grant = serde_json::json!({"selection": "default_profile", "authorization_revision": 1});
    let device = |id: &str, label: &str, fingerprint: &str| serde_json::json!({
        "device_id": id, "device_label": label,
        "client_certificate_sha256": fingerprint, "scopes": scopes,
        "initial_profile_grant": grant.clone(),
        "scope_revision": 1, "status": "active",
        "not_before_unix": 0, "not_after_unix": 4102444800i64,
        "created_at_unix": 0, "revoked_at_unix": null
    });
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-b1-dc",
        "devices": [device("device-a", "observer a", &fingerprint_a), device("device-b", "observer b", &fingerprint_b)]
    });
    std::fs::write(root.join("registry.json"), serde_json::to_vec(&registry)?)?;
    let client = |id: &str, fingerprint: &str| serde_json::json!({
        "device_id": id, "client_certificate_sha256": fingerprint, "scopes": scopes,
        "initial_profile_grant": grant.clone()
    });
    let projection = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "registry_sha256": "00".repeat(32),
        "clients": [client("device-a", &fingerprint_a), client("device-b", &fingerprint_b)]
    });
    std::fs::write(root.join("authorized-clients.json"), serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "device-a", "device-b"]);
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [{
            "profile_id": "default", "display_name": "Default", "default": true,
            "access": {"owner_device_id": "local", "members": members}
        }],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(root.join("profiles"))?;
    std::fs::write(root.join("profiles/catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}

/// Rewrite the committed registry to revoke one device. The Host re-reads this
/// file for every remote command, so that device's next request is denied.
fn revoke_device(root: &std::path::Path, device_id: &str) {
    let path = root.join("registry.json");
    let mut registry: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("registry read")).expect("registry json");
    for device in registry["devices"].as_array_mut().expect("devices array") {
        if device["device_id"] == device_id {
            device["status"] = serde_json::json!("revoked");
            device["revoked_at_unix"] = serde_json::json!(4102444800i64);
        }
    }
    std::fs::write(&path, serde_json::to_vec(&registry).expect("registry encode")).expect("registry write");
}
