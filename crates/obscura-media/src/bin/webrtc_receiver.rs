#![cfg(unix)]

use std::{collections::BTreeMap, net::{IpAddr, SocketAddr}, path::PathBuf, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, ControlPhase, InputState, Mode, Operation, Request, Response,
    ResultValue, SessionStatus, WebRtcCapability, WebRtcControlMessage, WebRtcSignal,
    WebRtcVideoCodec, WebRtcVideoFrame, WebRtcTransport,
    WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION};
use rtc::{media::io::sample_builder::SampleBuilder, rtp::codec::h264::H264Packet,
    rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit},
    rtp_transceiver::rtp_sender::RtpCodecKind};
use tokio::{net::TcpStream, process::Command as ProcessCommand,
    sync::{mpsc, Notify}, time::timeout};
use tokio_rustls::{TlsConnector, rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}, WebSocketStream};
use webrtc_rs::{data_channel::{DataChannel, DataChannelEvent}, media_stream::track_remote::{TrackRemote, TrackRemoteEvent},
    peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfiguration,
        RTCIceGatheringState, RTCSessionDescription}};

use obscura_media::webrtc::h264_media_engine;

const DEADLINE: Duration = Duration::from_secs(20);
const H264_CLOCK_RATE: u32 = 90_000;

#[derive(Parser)]
#[command(about = "Independent WebRTC receiver for an authenticated Obscura Host")]
struct Args {
    #[arg(long)] address: SocketAddr,
    #[arg(long)] ca: PathBuf,
    #[arg(long)] client_cert: PathBuf,
    #[arg(long)] client_key: PathBuf,
    #[arg(long, default_value = "localhost")] server_name: String,
    #[arg(long, default_value = "127.0.0.1")] bind_ip: IpAddr,
    #[arg(long, default_value_t = 3)] frames: usize,
    #[arg(long, default_value_t = 20.0)] click_x: f64,
    #[arg(long, default_value_t = 20.0)] click_y: f64,
    #[arg(long, default_value_t = false)] skip_click: bool,
    #[arg(long, default_value_t = false)] bad_binding: bool,
}

type Socket = WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.frames >= 3, "Receiver evidence requires at least three frames");
    let (mut control, ready) = connect_control(&args).await?;
    let session_id = ready_session(&ready)?;
    send_json(&mut control, &Request { id: 1, command: Command::Attach {
        mode: Mode::Observe,
        viewport: Some(obscura_host_protocol::ViewportDeclaration {
            device: obscura_host_protocol::Device::Phone,
            css_width: 160,
            css_height: 120,
            orientation: obscura_host_protocol::Orientation::Landscape,
        }),
    }, operation: None }).await?;
    let attached = status_from_response(next_response(&mut control).await?)?;
    let attachment_id = attached.attachment_id.context("Host did not return attachment identity")?;
    ensure!(attached.session_id == session_id, "Attach response changed Host session identity");

    let capability = WebRtcCapability {
        protocol_version: WEBRTC_PROTOCOL_VERSION,
        transport: WebRtcTransport::Udp,
        video_codec: WebRtcVideoCodec::H264AnnexB,
        data_channel_label: WEBRTC_CONTROL_LABEL.to_owned(),
        max_access_unit: 4 * 1024 * 1024,
    };
    let gather = Arc::new(Notify::new());
    let (track_tx, mut track_rx) = mpsc::channel(1);
    let peer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfiguration::default())
            .with_media_engine(h264_media_engine()?)
            .with_handler(Arc::new(ReceiverHandler { gather: Arc::clone(&gather), track_tx }))
            .with_udp_addrs(vec![SocketAddr::new(args.bind_ip, 0)])
            .build()
            .await
            .context("build independent WebRTC receiver PeerConnection")?,
    );
    peer.add_transceiver_from_kind(
        RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            ..Default::default()
        }),
    )
    .await
    .context("declare Host H.264 receive transceiver")?;
    let data_channel = peer.create_data_channel(WEBRTC_CONTROL_LABEL, None).await
        .context("create receiver WebRTC control DataChannel")?;
    let offer = peer.create_offer(None).await.context("create receiver WebRTC offer")?;
    peer.set_local_description(offer).await.context("apply receiver WebRTC offer")?;
    wait_for_notify(&gather, "receiver ICE gathering").await?;
    let offer = peer.local_description().await.context("receiver WebRTC offer missing")?;
    send_json(&mut control, &WebRtcSignal::Offer { id: 2, capability: capability.clone(), sdp: offer.sdp }).await?;
    let answer = next_signal(&mut control).await?;
    let WebRtcSignal::Answer { id: 2, capability: answer_capability, binding, sdp } = answer else {
        bail!("Host did not return a WebRTC answer");
    };
    ensure!(answer_capability == capability, "Host answer capability differs from offer");
    ensure!(binding.session_id == session_id && binding.attachment_id == attachment_id && !binding.auth_binding.is_empty(),
        "Host answer binding does not match the authenticated attachment");
    peer.set_remote_description(RTCSessionDescription::answer(sdp).context("parse Host WebRTC answer")?)
        .await.context("apply Host WebRTC answer")?;
    wait_for_open(&data_channel).await?;
    let hello_binding = if args.bad_binding {
        let mut bad = binding.clone();
        bad.session_id.push_str("-stale");
        bad
    } else { binding.clone() };
    send_control(&data_channel, &WebRtcControlMessage::Hello { capability: capability.clone(), binding: hello_binding }).await?;
    if args.bad_binding {
        timeout(DEADLINE, async {
            loop {
                match data_channel.poll().await.context("WebRTC DataChannel ended without explicit stale-binding rejection")? {
                    DataChannelEvent::OnClose | DataChannelEvent::OnError => break,
                    DataChannelEvent::OnMessage(message) => {
                        let control: WebRtcControlMessage = serde_json::from_slice(&message.data)
                            .context("decode stale-binding WebRTC rejection")?;
                        match control {
                            WebRtcControlMessage::Error { code, .. } => {
                                ensure!(code == "STALE_WEBRTC_BINDING", "unexpected stale-binding rejection code: {code}");
                                break;
                            }
                            _ => bail!("stale WebRTC binding was not rejected"),
                        }
                    }
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await.context("timed out waiting for stale WebRTC binding rejection")??;
        println!("{}", serde_json::json!({"pass": true, "negative": "stale_binding_rejected", "session_id": session_id}));
        let _ = peer.close().await;
        return Ok(());
    }

    let track = timeout(DEADLINE, track_rx.recv()).await.context("timed out waiting for Host H.264 track")?
        .context("Host WebRTC track ended before media")?;
    let mut descriptors: BTreeMap<u32, WebRtcVideoFrame> = BTreeMap::new();
    let mut samples: BTreeMap<u32, DecodedSample> = BTreeMap::new();
    let mut evidence = Vec::new();
    let mut builder = SampleBuilder::new(256, H264Packet::default(), H264_CLOCK_RATE)
        .with_max_time_delay(Duration::from_secs(2));
    let mut hello_ack = false;
    let mut click_sent = false;
    let mut rtp_packets = 0u64;
    let mut status = attached;
    let mut last_sequence = None;
    let mut last_pts = None;
    let mut last_checksum = None;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if !args.skip_click && !click_sent && evidence.len() >= 1 {
            status = request_takeover_and_click(&mut control, &status, args.click_x, args.click_y).await?;
            click_sent = true;
        }
        while let Some(rtp_timestamp) = next_matching_timestamp(&samples, &descriptors) {
            let descriptor = descriptors.remove(&rtp_timestamp).expect("descriptor key was checked present");
            let sample = samples.remove(&rtp_timestamp).expect("sample key was checked present");
            let decoded = decode_sample(&sample.data, descriptor.coded_width, descriptor.coded_height).await?;
            ensure!(descriptor.session_id == session_id, "invalid frame descriptor session identity");
            ensure!(descriptor.sequence > last_sequence.unwrap_or(0), "WebRTC frame sequence did not increase");
            ensure!(descriptor.pts_us >= last_pts.unwrap_or(0), "WebRTC frame PTS did not increase");
            ensure!(descriptor.width > 0 && descriptor.height > 0, "WebRTC frame descriptor has invalid source size");
            ensure!(descriptor.coded_width >= descriptor.width && descriptor.coded_height >= descriptor.height,
                "WebRTC frame descriptor coded size is smaller than source size");
            last_sequence = Some(descriptor.sequence);
            last_pts = Some(descriptor.pts_us);
            let changed = last_checksum.is_some_and(|checksum| checksum != decoded.checksum);
            last_checksum = Some(decoded.checksum);
            evidence.push(FrameEvidence { descriptor, checksum: decoded.checksum, rtp_packets: sample.packet_count, changed });
            if evidence.len() >= args.frames && (args.skip_click || (click_sent && evidence.iter().any(|frame| frame.changed))) { break; }
        }
        if evidence.len() >= args.frames && (args.skip_click || (click_sent && evidence.iter().any(|frame| frame.changed))) { break; }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out collecting continuous WebRTC frames (rtp_packets={rtp_packets}, descriptors={}, samples={}, hello_ack={hello_ack})",
                descriptors.len(), samples.len());
        }
        tokio::select! {
            event = data_channel.poll() => match event {
                Some(DataChannelEvent::OnMessage(message)) => {
                    let control: WebRtcControlMessage = serde_json::from_slice(&message.data).context("decode Host WebRTC control message")?;
                    match control {
                        WebRtcControlMessage::HelloAck { capability: ack_capability, binding: ack_binding } => {
                            ensure!(ack_capability == capability && ack_binding == binding, "Host WebRTC HelloAck binding mismatch");
                            hello_ack = true;
                        }
                        WebRtcControlMessage::Error { code, message } => bail!("Host WebRTC error {code}: {message}"),
                        WebRtcControlMessage::VideoFrame { descriptor } => {
                        ensure!(descriptor.session_id == session_id, "WebRTC descriptor session identity differs from Host session");
                        ensure!(descriptors.insert(descriptor.rtp_timestamp, descriptor).is_none(), "duplicate WebRTC RTP timestamp descriptor");
                        ensure!(descriptors.len() <= 64, "WebRTC descriptor association backlog exceeded 64 frames");
                    }
                        WebRtcControlMessage::Pong { .. } => {},
                        other => bail!("unexpected Host WebRTC control message: {other:?}"),
                    }
                }
                Some(DataChannelEvent::OnError) => bail!("receiver WebRTC DataChannel error"),
                Some(DataChannelEvent::OnClose) | None => bail!("receiver WebRTC DataChannel closed"),
                _ => {},
            },
            event = track.poll() => match event {
                Some(TrackRemoteEvent::OnRtpPacket(packet)) => {
                    rtp_packets += 1;
                    builder.push(Instant::now(), packet);
                    if let Some(sample) = builder.pop(Instant::now()) {
                        let rtp_timestamp = sample.packet_timestamp;
                        ensure!(samples.insert(rtp_timestamp, DecodedSample { data: sample.data.to_vec(), packet_count: 1 }).is_none(),
                            "duplicate WebRTC RTP timestamp sample");
                        ensure!(samples.len() <= 64, "WebRTC RTP association backlog exceeded 64 samples");
                    }
                }
                Some(TrackRemoteEvent::OnError) => bail!("receiver H.264 track error"),
                Some(TrackRemoteEvent::OnEnded) | None => bail!("receiver H.264 track ended"),
                _ => {},
            },
        }
    }
    ensure!(hello_ack, "receiver did not receive Host WebRTC HelloAck");
    ensure!(evidence.len() >= 3, "continuous WebRTC evidence has fewer than three frames");
    println!("{}", serde_json::json!({
        "pass": true,
        "session_id": session_id,
        "attachment_id": attachment_id,
        "frames": evidence.iter().map(|frame| serde_json::json!({
            "sequence": frame.descriptor.sequence,
            "document_revision": frame.descriptor.document_revision,
            "viewport_revision": frame.descriptor.viewport_revision,
            "width": frame.descriptor.width,
            "height": frame.descriptor.height,
            "coded_width": frame.descriptor.coded_width,
            "coded_height": frame.descriptor.coded_height,
            "pts_us": frame.descriptor.pts_us,
            "checksum": frame.checksum,
            "rtp_packets": frame.rtp_packets,
            "changed": frame.changed,
        })).collect::<Vec<_>>(),
    }));
    let _ = peer.close().await;
    Ok(())
}

#[derive(Debug)]
struct FrameEvidence { descriptor: WebRtcVideoFrame, checksum: u64, rtp_packets: u64, changed: bool }
#[derive(Debug)]
struct DecodedSample { data: Vec<u8>, packet_count: u64 }
#[derive(Debug)]
struct Decoded { checksum: u64 }

fn next_matching_timestamp(
    samples: &BTreeMap<u32, DecodedSample>,
    descriptors: &BTreeMap<u32, WebRtcVideoFrame>,
) -> Option<u32> {
    samples.keys().copied().find(|timestamp| descriptors.contains_key(timestamp))
}

async fn connect_control(args: &Args) -> Result<(Socket, Response)> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(std::fs::read(&args.ca)?))?;
    let config = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(vec![CertificateDer::from(std::fs::read(&args.client_cert)?)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(&args.client_key)?)))?;
    let connector = TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(args.server_name.clone()).context("invalid TLS server name")?;
    let tls = connector.connect(name, TcpStream::connect(args.address).await?).await?;
    let request = format!("wss://{}/control", args.address).into_client_request()?;
    let (mut socket, _) = client_async(request, tls).await?;
    let ready = next_response(&mut socket).await?;
    Ok((socket, ready))
}

async fn next_response(socket: &mut Socket) -> Result<Response> {
    loop {
        match socket.next().await.context("control socket ended")?? {
            Message::Text(text) => {
                if let Ok(response) = serde_json::from_str::<Response>(&text) { return Ok(response); }
            }
            Message::Ping(bytes) => { socket.send(Message::Pong(bytes)).await?; }
            Message::Close(_) => bail!("control socket closed"),
            _ => bail!("control socket returned non-text message"),
        }
    }
}

async fn next_signal(socket: &mut Socket) -> Result<WebRtcSignal> {
    loop {
        match socket.next().await.context("signaling socket ended")?? {
            Message::Text(text) => {
                let signal: WebRtcSignal = serde_json::from_str(&text)?;
                if let WebRtcSignal::Error { code, message, .. } = &signal { bail!("WebRTC signaling rejected: {code}: {message}"); }
                return Ok(signal);
            }
            Message::Ping(bytes) => { socket.send(Message::Pong(bytes)).await?; }
            Message::Close(_) => bail!("signaling socket closed"),
            _ => bail!("signaling socket returned non-text message"),
        }
    }
}

async fn send_json<T: serde::Serialize>(socket: &mut Socket, value: &T) -> Result<()> {
    socket.send(Message::text(serde_json::to_string(value)?)).await?;
    Ok(())
}

fn ready_session(response: &Response) -> Result<String> {
    match response { Response::Ready { session_id, .. } => Ok(session_id.clone()), other => bail!("expected Host Ready, got {other:?}") }
}

fn status_from_response(response: Response) -> Result<SessionStatus> {
    match response {
        Response::Result { value: ResultValue::Status(status), .. } => Ok(status),
        Response::Error { code, message, .. } => bail!("Host rejected control request: {code}: {message}"),
        other => bail!("expected Host Status, got {other:?}"),
    }
}

async fn wait_for_open(channel: &Arc<dyn DataChannel>) -> Result<()> {
    loop {
        match timeout(DEADLINE, channel.poll()).await?.context("DataChannel closed before open")? {
            DataChannelEvent::OnOpen => return Ok(()),
            DataChannelEvent::OnError => bail!("DataChannel error before open"),
            DataChannelEvent::OnClose => bail!("DataChannel closed before open"),
            _ => {},
        }
    }
}

async fn send_control(channel: &Arc<dyn DataChannel>, value: &WebRtcControlMessage) -> Result<()> {
    channel.send_text(&serde_json::to_string(value)?).await?;
    Ok(())
}

async fn wait_for_notify(notify: &Notify, description: &str) -> Result<()> {
    timeout(DEADLINE, notify.notified()).await.with_context(|| format!("timed out waiting for {description}"))?;
    Ok(())
}

async fn request_takeover_and_click(socket: &mut Socket, status: &SessionStatus, x: f64, y: f64) -> Result<SessionStatus> {
    send_json(socket, &Request { id: 10, command: Command::RequestTakeover { epoch: status.control.epoch }, operation: None }).await?;
    let takeover = status_from_response(next_response(socket).await?)?;
    ensure!(matches!(takeover.control.phase, ControlPhase::Human { .. }), "Host did not grant human control for receiver click");
    let operation = Operation { session_id: takeover.session_id.clone(), attachment_id: takeover.attachment_id.context("takeover attachment missing")?,
        sequence: takeover.next_sequence, control_epoch: takeover.control.epoch, viewport_revision: takeover.viewport_revision, document_revision: takeover.document_revision };
    send_json(socket, &Request { id: 11, command: Command::Click { x, y }, operation: Some(operation) }).await?;
    match next_response(socket).await? {
        Response::Result { value: ResultValue::Input { input }, .. } => ensure!(matches!(input.state, InputState::Succeeded), "Host click did not succeed"),
        Response::Error { code, message, .. } => bail!("Host click rejected: {code}: {message}"),
        other => bail!("unexpected Host click response: {other:?}"),
    }
    send_json(socket, &Request { id: 12, command: Command::Status {}, operation: None }).await?;
    let after_click = status_from_response(next_response(socket).await?)?;
    send_json(socket, &Request { id: 13, command: Command::ReleaseControl { epoch: after_click.control.epoch }, operation: None }).await?;
    let released = status_from_response(next_response(socket).await?)?;
    ensure!(matches!(released.control.phase, ControlPhase::Agent), "Host did not release human control");
    Ok(released)
}

async fn decode_sample(data: &[u8], width: u32, height: u32) -> Result<Decoded> {
    let expected = usize::try_from(width).ok().and_then(|width| usize::try_from(height).ok().and_then(|height| width.checked_mul(height)?.checked_mul(4))).context("decoded frame size overflow")?;
    let mut child = ProcessCommand::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "h264", "-i", "pipe:0", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn()?;
    let mut stdin = child.stdin.take().context("decoder stdin missing")?;
    tokio::io::AsyncWriteExt::write_all(&mut stdin, data).await?;
    drop(stdin);
    let output = timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    ensure!(output.status.success(), "receiver H.264 decode failed: {}", String::from_utf8_lossy(&output.stderr));
    ensure!(output.stdout.len() == expected, "receiver decoded {} bytes, expected {expected}", output.stdout.len());
    let checksum = output.stdout.iter().fold(0u64, |sum, byte| sum.wrapping_mul(131).wrapping_add(u64::from(*byte)));
    Ok(Decoded { checksum })
}

struct ReceiverHandler { gather: Arc<Notify>, track_tx: mpsc::Sender<Arc<dyn TrackRemote>> }

#[async_trait::async_trait]
impl PeerConnectionEventHandler for ReceiverHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete { self.gather.notify_one(); }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.track_tx.send(track).await;
    }
}

#[cfg(test)]
mod association_tests {
    use super::*;

    fn descriptor(rtp_timestamp: u32, sequence: u64) -> WebRtcVideoFrame {
        WebRtcVideoFrame {
            session_id: "session".into(), sequence, rtp_timestamp,
            document_revision: 0, viewport_revision: 0,
            width: 160, height: 120, coded_width: 160, coded_height: 120,
            pts_us: sequence, keyframe: true,
        }
    }

    fn sample() -> DecodedSample { DecodedSample { data: vec![1], packet_count: 1 } }

    #[test]
    fn lost_rtp_frame_cannot_pair_descriptor_with_a_later_sample() {
        let mut descriptors = BTreeMap::new();
        descriptors.insert(100, descriptor(100, 1));
        descriptors.insert(300, descriptor(300, 3));
        let mut samples = BTreeMap::new();
        samples.insert(300, sample());

        let timestamp = next_matching_timestamp(&samples, &descriptors).expect("later exact timestamp should match");
        assert_eq!(timestamp, 300);
        assert_eq!(descriptors.get(&timestamp).expect("matching descriptor").sequence, 3);
        assert!(descriptors.contains_key(&100), "missing RTP frame descriptor must remain explicitly unmatched");
    }

    #[test]
    fn descriptor_and_rtp_sample_may_arrive_in_different_orders() {
        let mut descriptors = BTreeMap::new();
        let mut samples = BTreeMap::new();
        samples.insert(200, sample());
        assert!(next_matching_timestamp(&samples, &descriptors).is_none());
        descriptors.insert(200, descriptor(200, 2));

        let timestamp = next_matching_timestamp(&samples, &descriptors).expect("late descriptor should match exact RTP timestamp");
        assert_eq!(descriptors.get(&timestamp).expect("matching descriptor").sequence, 2);
    }
}
