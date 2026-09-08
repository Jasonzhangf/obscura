#![cfg(unix)]

use std::{collections::BTreeMap, net::{IpAddr, SocketAddr}, path::PathBuf, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, ControlPhase, InputState, Mode, Operation, PixelFormat,
    Request, Response, ResultValue, SessionStatus, WebRtcCapability, WebRtcControlMessage,
    WebRtcSignal, WebRtcVideoCodec, WebRtcVideoFrame, WebRtcTransport,
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
const MAX_CONTROL_MESSAGE: usize = 64 * 1024;
const MAX_ASSOCIATIONS: usize = 64;
const INPUT_FIXTURE: &str = "data:text/html,<style>body{margin:0}button{position:absolute;left:10px;top:10px;width:120px;height:50px;background:red}input{position:absolute;left:10px;top:80px;width:130px;height:30px}.space{height:3000px;margin-top:150px}</style><button id='target' onclick=\"window.clicks=(window.clicks||0)+1;this.style.background='lime'\">target</button><input id='text'><div class='space'></div>";

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
    #[arg(long, default_value_t = false)] expect_encoder_error: bool,
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
    let selected_pair = obscura_media::webrtc::selected_udp_candidate_pair(&peer).await?;
    let mut media = MediaAssociations::new();
    let (control_tx, mut control_rx) = mpsc::channel(128);
    let control_reader = tokio::spawn(read_data_channel(Arc::clone(&data_channel), control_tx));
    let hello_binding = if args.bad_binding {
        let mut bad = binding.clone();
        bad.session_id.push_str("-stale");
        bad
    } else { binding.clone() };
    send_control(&data_channel, &WebRtcControlMessage::Hello { capability: capability.clone(), binding: hello_binding }).await?;
    if args.bad_binding {
        match next_control(&mut control_rx, "stale-binding rejection").await? {
            WebRtcControlMessage::Error { code, .. } => ensure!(code == "STALE_WEBRTC_BINDING", "unexpected stale-binding rejection code: {code}"),
            other => bail!("stale WebRTC binding was not rejected: {other:?}"),
        }
        println!("{}", serde_json::json!({"pass": true, "negative": "stale_binding_rejected", "session_id": session_id}));
        control_reader.abort();
        let _ = control_reader.await;
        let _ = peer.close().await;
        return Ok(());
    }

    wait_for_hello_ack(&mut control_rx, &mut media, &capability, &binding).await?;
    if args.expect_encoder_error {
        let WebRtcControlMessage::Error { code, message } = next_control(&mut control_rx, "encoder failure").await?
        else { bail!("Host did not return a typed encoder failure") };
        ensure!(code == "ENCODER_UNAVAILABLE", "unexpected encoder failure code: {code}");
        println!("{}", serde_json::json!({"pass": true, "error_code": code, "error_message": message, "session_id": session_id}));
        control_reader.abort();
        let _ = control_reader.await;
        let _ = peer.close().await;
        return Ok(());
    }
    send_control(&data_channel, &WebRtcControlMessage::Ping { request_id: 3 }).await?;
    wait_for_pong(&mut control_rx, &mut media, &session_id, 3).await?;
    let track = timeout(DEADLINE, track_rx.recv()).await.context("timed out waiting for Host H.264 track")?
        .context("Host WebRTC track ended before media")?;
    let mut evidence = Vec::new();
    let mut browser_path_done = args.skip_click;
    let mut status = attached;
    let mut last_sequence = None;
    let mut last_pts = None;
    let mut last_encoder_id = None;
    let mut last_dimensions = None;
    let mut last_checksum = None;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        while let Some((rtp_timestamp, descriptor, sample)) = media.take_matching() {
            validate_video_frame(&descriptor, &sample, rtp_timestamp, &session_id, &mut last_sequence, &mut last_pts,
                &mut last_encoder_id, &mut last_dimensions)?;
            let decoded = decode_sample(&sample.data, descriptor.coded_width, descriptor.coded_height).await?;
            let changed = last_checksum.is_some_and(|checksum| checksum != decoded.checksum);
            last_checksum = Some(decoded.checksum);
            evidence.push(FrameEvidence { descriptor, checksum: decoded.checksum, rtp_packets: sample.packet_count, changed });
        }
        if !browser_path_done && evidence.len() >= 1 && evidence.last().is_some_and(|frame|
            frame.descriptor.source.width == 160 && frame.descriptor.source.height == 120) {
            let initial_epoch = status.control.epoch;
            let forbidden = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 100, command: Command::Evaluate { expression: "window.location.href".into() }, operation: None }).await?;
            expect_error(forbidden, "REMOTE_COMMAND_FORBIDDEN")?;

            let takeover = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 101, command: Command::RequestTakeover { epoch: initial_epoch }, operation: None }).await?;
            status = status_from_response(takeover)?;
            ensure!(matches!(status.control.phase, ControlPhase::Human { .. }), "Host did not grant human control over WebRTC DataChannel");

            let stale_control = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 102, command: Command::RequestTakeover { epoch: initial_epoch }, operation: None }).await?;
            expect_error(stale_control, "STALE_CONTROL")?;

            let navigate_operation = operation_for(&status)?;
            let navigated = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 103, command: Command::Navigate { url: INPUT_FIXTURE.into() }, operation: Some(navigate_operation.clone()) }).await?;
            status = status_from_response(navigated)?;

            let mut stale_document = operation_for(&status)?;
            stale_document.document_revision = navigate_operation.document_revision;
            let stale_document_response = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 104, command: Command::Click { x: args.click_x, y: args.click_y }, operation: Some(stale_document) }).await?;
            expect_error(stale_document_response, "STALE_DOCUMENT")?;

            let mut stale_operation = operation_for(&status)?;
            stale_operation.control_epoch = initial_epoch;
            let stale_control_response = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 105, command: Command::Click { x: args.click_x, y: args.click_y }, operation: Some(stale_operation) }).await?;
            expect_error(stale_control_response, "STALE_CONTROL")?;

            let click_operation = operation_for(&status)?;
            let clicked = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 106, command: Command::Click { x: args.click_x, y: args.click_y }, operation: Some(click_operation.clone()) }).await?;
            expect_input(clicked)?;
            let duplicate = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 107, command: Command::Click { x: args.click_x, y: args.click_y }, operation: Some(click_operation) }).await?;
            expect_input(duplicate)?;
            status = status_from_response(request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 108, command: Command::Status {}, operation: None }).await?)?;

            let focus_click = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 109, command: Command::Click { x: 30.0, y: 95.0 }, operation: Some(operation_for(&status)?) }).await?;
            expect_input(focus_click)?;
            status = status_from_response(request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 110, command: Command::Status {}, operation: None }).await?)?;

            let input = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 111, command: Command::InputText { text: "移动端中文输入".into() }, operation: Some(operation_for(&status)?) }).await?;
            expect_input(input)?;
            status = status_from_response(request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 112, command: Command::Status {}, operation: None }).await?)?;

            let scroll = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 113, command: Command::Scroll { x: 30.0, y: 100.0, delta_x: 0.0, delta_y: 400.0 }, operation: Some(operation_for(&status)?) }).await?;
            expect_input(scroll)?;
            status = status_from_response(request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 114, command: Command::Status {}, operation: None }).await?)?;

            let released = request_dc(&data_channel, &mut control_rx, &track, &mut media, &session_id,
                Request { id: 115, command: Command::ReleaseControl { epoch: status.control.epoch }, operation: None }).await?;
            status = status_from_response(released)?;
            ensure!(matches!(status.control.phase, ControlPhase::Agent), "Host did not release WebRTC human control");
            browser_path_done = true;
        }
        if evidence.len() >= args.frames && (args.skip_click || (browser_path_done && evidence.iter().any(|frame| frame.changed))) { break; }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out collecting continuous WebRTC frames (rtp_packets={}, descriptors={}, samples={}, browser_path_done={browser_path_done})",
                media.rtp_packets, media.descriptors.len(), media.samples.len());
        }
        tokio::select! {
            message = control_rx.recv() => match message.context("WebRTC DataChannel reader ended")?? {
                WebRtcControlMessage::VideoFrame { descriptor } => media.accept_descriptor(descriptor, &session_id)?,
                WebRtcControlMessage::Error { code, message } => bail!("receiver WebRTC error {code}: {message}"),
                WebRtcControlMessage::Pong { .. } => {},
                other => bail!("unexpected Host WebRTC control message: {other:?}"),
            },
            event = track.poll() => match event {
                Some(event) => media.accept_track_event(event)?,
                None => bail!("receiver H.264 track ended"),
            },
        }
    }
    ensure!(evidence.len() >= 3, "continuous WebRTC evidence has fewer than three frames");
    println!("{}", serde_json::json!({
        "pass": true,
        "browser_path": "webrtc_data_channel",
        "ice": {
            "transport": "udp",
            "local_candidate": obscura_media::webrtc::candidate_evidence(selected_pair.local()),
            "remote_candidate": obscura_media::webrtc::candidate_evidence(selected_pair.remote()),
        },
        "session_id": session_id,
        "attachment_id": attachment_id,
        "frames": evidence.iter().map(|frame| serde_json::json!({
            "sequence": frame.descriptor.source.sequence,
            "document_revision": frame.descriptor.source.document_revision,
            "viewport_revision": frame.descriptor.source.viewport_revision,
            "session_id": frame.descriptor.source.session_id,
            "width": frame.descriptor.source.width,
            "height": frame.descriptor.source.height,
            "stride": frame.descriptor.source.stride,
            "source_byte_length": frame.descriptor.source.byte_length,
            "encoder_id": frame.descriptor.encoder_id,
            "rtp_timestamp": frame.descriptor.rtp_timestamp,
            "codec": frame.descriptor.codec,
            "keyframe": frame.descriptor.keyframe,
            "access_unit_bytes": frame.descriptor.access_unit_bytes,
            "coded_width": frame.descriptor.coded_width,
            "coded_height": frame.descriptor.coded_height,
            "pts_us": frame.descriptor.pts_us,
            "checksum": frame.checksum,
            "rtp_packets": frame.rtp_packets,
            "changed": frame.changed,
        })).collect::<Vec<_>>(),
    }));
    control_reader.abort();
    let _ = control_reader.await;
    let _ = peer.close().await;
    Ok(())
}

#[derive(Debug)]
struct FrameEvidence { descriptor: WebRtcVideoFrame, checksum: u64, rtp_packets: u64, changed: bool }
#[derive(Debug)]
struct DecodedSample { data: Vec<u8>, packet_count: u64 }
#[derive(Debug)]
struct Decoded { checksum: u64 }

struct MediaAssociations {
    descriptors: BTreeMap<u32, WebRtcVideoFrame>,
    samples: BTreeMap<u32, DecodedSample>,
    packet_counts: BTreeMap<u32, u64>,
    builder: SampleBuilder<H264Packet>,
    rtp_packets: u64,
}

impl MediaAssociations {
    fn new() -> Self {
        Self {
            descriptors: BTreeMap::new(),
            samples: BTreeMap::new(),
            packet_counts: BTreeMap::new(),
            builder: SampleBuilder::new(256, H264Packet::default(), H264_CLOCK_RATE)
                .with_max_time_delay(Duration::from_secs(2)),
            rtp_packets: 0,
        }
    }

    fn accept_descriptor(&mut self, descriptor: WebRtcVideoFrame, session_id: &str) -> Result<()> {
        validate_descriptor(&descriptor, session_id)?;
        ensure!(self.descriptors.insert(descriptor.rtp_timestamp, descriptor).is_none(),
            "duplicate WebRTC RTP timestamp descriptor");
        ensure!(self.descriptors.len() <= MAX_ASSOCIATIONS, "WebRTC descriptor association backlog exceeded 64 frames");
        Ok(())
    }

    fn accept_track_event(&mut self, event: TrackRemoteEvent) -> Result<()> {
        match event {
            TrackRemoteEvent::OnRtpPacket(packet) => {
                self.rtp_packets += 1;
                let timestamp = packet.header.timestamp;
                *self.packet_counts.entry(timestamp).or_insert(0) += 1;
                ensure!(self.packet_counts.len() <= MAX_ASSOCIATIONS,
                    "WebRTC RTP packet association backlog exceeded 64 timestamps");
                self.builder.push(Instant::now(), packet);
                while let Some(sample) = self.builder.pop(Instant::now()) {
                    ensure!(!sample.data.is_empty() && sample.data.len() <= obscura_media::MAX_ACCESS_UNIT,
                        "invalid reassembled WebRTC H.264 access unit");
                    let packet_count = self.packet_counts.remove(&sample.packet_timestamp)
                        .context("missing RTP packet count for reassembled access unit")?;
                    ensure!(self.samples.insert(sample.packet_timestamp, DecodedSample {
                        data: sample.data.to_vec(), packet_count,
                    }).is_none(), "duplicate WebRTC RTP timestamp sample");
                    ensure!(self.samples.len() <= MAX_ASSOCIATIONS,
                        "WebRTC RTP association backlog exceeded 64 samples");
                }
                Ok(())
            }
            TrackRemoteEvent::OnError => bail!("receiver H.264 track error"),
            TrackRemoteEvent::OnEnded => bail!("receiver H.264 track ended"),
            _ => Ok(()),
        }
    }

    fn take_matching(&mut self) -> Option<(u32, WebRtcVideoFrame, DecodedSample)> {
        let timestamp = next_matching_timestamp(&self.samples, &self.descriptors)?;
        Some((timestamp, self.descriptors.remove(&timestamp)?, self.samples.remove(&timestamp)?))
    }
}

fn even_dimension(value: u32) -> u32 { value.saturating_add(1) & !1 }

fn validate_descriptor(descriptor: &WebRtcVideoFrame, session_id: &str) -> Result<()> {
    ensure!(descriptor.source.session_id == session_id, "WebRTC frame session identity differs from Host session");
    ensure!(matches!(descriptor.source.pixel_format, PixelFormat::PremultipliedRgba8),
        "WebRTC frame source pixel format is unsupported");
    obscura_media::validate_pixels(descriptor.source.width, descriptor.source.height, descriptor.source.byte_length)?;
    ensure!(descriptor.source.stride == descriptor.source.width.checked_mul(4).context("WebRTC source stride overflow")?,
        "WebRTC frame source stride does not match source width");
    ensure!(!descriptor.encoder_id.is_empty() && descriptor.encoder_id.len() <= 128,
        "WebRTC frame encoder identity is invalid");
    ensure!(matches!(descriptor.codec, WebRtcVideoCodec::H264AnnexB),
        "WebRTC frame codec is not H.264 Annex B");
    ensure!(descriptor.keyframe, "WebRTC frame is not independently decodable");
    ensure!(descriptor.access_unit_bytes > 0 && descriptor.access_unit_bytes <= obscura_media::MAX_ACCESS_UNIT as u64,
        "WebRTC frame access-unit length is invalid");
    ensure!(descriptor.coded_width == even_dimension(descriptor.source.width)
        && descriptor.coded_height == even_dimension(descriptor.source.height),
        "WebRTC frame coded dimensions do not match source dimensions");
    Ok(())
}

fn validate_video_frame(
    descriptor: &WebRtcVideoFrame,
    sample: &DecodedSample,
    rtp_timestamp: u32,
    session_id: &str,
    last_sequence: &mut Option<u64>,
    last_pts: &mut Option<u64>,
    last_encoder_id: &mut Option<String>,
    last_dimensions: &mut Option<(u64, u64, u32, u32)>,
) -> Result<()> {
    validate_descriptor(descriptor, session_id)?;
    ensure!(descriptor.rtp_timestamp == rtp_timestamp, "WebRTC RTP timestamp association changed");
    ensure!(descriptor.access_unit_bytes == sample.data.len() as u64,
        "WebRTC descriptor access-unit length differs from reassembled RTP bytes: descriptor={} rtp={}",
        descriptor.access_unit_bytes, sample.data.len());
    ensure!(sample.packet_count > 0, "WebRTC frame has no RTP packets");
    if let Some(previous) = *last_sequence {
        ensure!(descriptor.source.sequence > previous, "WebRTC frame sequence did not increase");
    }
    if let Some(previous) = *last_pts {
        ensure!(descriptor.pts_us >= previous, "WebRTC frame PTS did not increase");
    }
    if let Some(previous) = last_encoder_id.as_ref() {
        ensure!(previous == &descriptor.encoder_id, "WebRTC encoder identity changed during one connection");
    } else {
        *last_encoder_id = Some(descriptor.encoder_id.clone());
    }
    if let Some((viewport_revision, document_revision, width, height)) = *last_dimensions {
        ensure!(descriptor.source.viewport_revision >= viewport_revision, "WebRTC viewport revision regressed");
        ensure!(descriptor.source.document_revision >= document_revision, "WebRTC document revision regressed");
        if descriptor.source.viewport_revision == viewport_revision {
            ensure!((descriptor.source.width, descriptor.source.height) == (width, height),
                "WebRTC source dimensions changed without a viewport revision");
        }
    }
    *last_sequence = Some(descriptor.source.sequence);
    *last_pts = Some(descriptor.pts_us);
    *last_dimensions = Some((descriptor.source.viewport_revision, descriptor.source.document_revision,
        descriptor.source.width, descriptor.source.height));
    Ok(())
}

async fn read_data_channel(channel: Arc<dyn DataChannel>, tx: mpsc::Sender<Result<WebRtcControlMessage>>) {
    let result: Result<()> = async {
        loop {
            let event = channel.poll().await.context("WebRTC DataChannel ended")?;
            match event {
                DataChannelEvent::OnMessage(message) => {
                    ensure!(message.data.len() <= MAX_CONTROL_MESSAGE, "WebRTC control message exceeds 64 KiB");
                    let control = serde_json::from_slice(&message.data).context("decode WebRTC control message")?;
                    if tx.send(Ok(control)).await.is_err() { return Ok(()); }
                }
                DataChannelEvent::OnError => bail!("receiver WebRTC DataChannel error"),
                DataChannelEvent::OnClose => bail!("receiver WebRTC DataChannel closed"),
                _ => {}
            }
        }
    }.await;
    if let Err(error) = result { let _ = tx.send(Err(error)).await; }
}

async fn next_control(rx: &mut mpsc::Receiver<Result<WebRtcControlMessage>>, description: &str) -> Result<WebRtcControlMessage> {
    Ok(timeout(DEADLINE, rx.recv()).await
        .with_context(|| format!("timed out waiting for WebRTC {description}"))?
        .context("WebRTC DataChannel reader ended")??)
}

async fn wait_for_hello_ack(
    rx: &mut mpsc::Receiver<Result<WebRtcControlMessage>>,
    media: &mut MediaAssociations,
    capability: &WebRtcCapability,
    binding: &obscura_host_protocol::WebRtcSessionBinding,
) -> Result<()> {
    loop {
        match next_control(rx, "HelloAck").await? {
            WebRtcControlMessage::HelloAck { capability: ack_capability, binding: ack_binding } => {
                ensure!(ack_capability == *capability && ack_binding == *binding, "Host WebRTC HelloAck binding mismatch");
                return Ok(());
            }
            WebRtcControlMessage::VideoFrame { descriptor } => media.accept_descriptor(descriptor, &binding.session_id)?,
            WebRtcControlMessage::Error { code, message } => bail!("Host WebRTC error {code}: {message}"),
            other => bail!("unexpected WebRTC control message while waiting for HelloAck: {other:?}"),
        }
    }
}

async fn wait_for_pong(
    rx: &mut mpsc::Receiver<Result<WebRtcControlMessage>>,
    media: &mut MediaAssociations,
    session_id: &str,
    request_id: u64,
) -> Result<()> {
    loop {
        match next_control(rx, "Pong").await? {
            WebRtcControlMessage::Pong { request_id: received } => {
                ensure!(received == request_id, "WebRTC Pong request id mismatch");
                return Ok(());
            }
            WebRtcControlMessage::VideoFrame { descriptor } => media.accept_descriptor(descriptor, session_id)?,
            WebRtcControlMessage::Error { code, message } => bail!("Host WebRTC error {code}: {message}"),
            other => bail!("unexpected WebRTC control message while waiting for Pong: {other:?}"),
        }
    }
}

async fn request_dc(
    channel: &Arc<dyn DataChannel>,
    rx: &mut mpsc::Receiver<Result<WebRtcControlMessage>>,
    track: &Arc<dyn TrackRemote>,
    media: &mut MediaAssociations,
    session_id: &str,
    request: Request,
) -> Result<Response> {
    let request_id = request.id;
    send_control(channel, &WebRtcControlMessage::BrowserRequest { request }).await?;
    timeout(DEADLINE, async {
        loop {
            tokio::select! {
                message = rx.recv() => match message.context("WebRTC DataChannel reader ended")?? {
                    WebRtcControlMessage::VideoFrame { descriptor } => media.accept_descriptor(descriptor, session_id)?,
                    WebRtcControlMessage::BrowserResponse { response } => {
                        ensure!(response_id(&response) == Some(request_id), "WebRTC browser response id mismatch");
                        return Ok(response);
                    }
                    WebRtcControlMessage::Pong { .. } => {},
                    WebRtcControlMessage::Error { code, message } => bail!("Host WebRTC error {code}: {message}"),
                    other => bail!("unexpected WebRTC control message while waiting for browser response: {other:?}"),
                },
                event = track.poll() => media.accept_track_event(event.context("receiver H.264 track ended")?)?,
            }
        }
    }).await.context("timed out waiting for WebRTC browser response")?
}

fn operation_for(status: &SessionStatus) -> Result<Operation> {
    Ok(Operation {
        session_id: status.session_id.clone(),
        attachment_id: status.attachment_id.context("Host status has no WebRTC attachment")?,
        sequence: status.next_sequence,
        control_epoch: status.control.epoch,
        viewport_revision: status.viewport_revision,
        document_revision: status.document_revision,
    })
}

fn response_id(response: &Response) -> Option<u64> {
    match response {
        Response::Result { id, .. } | Response::Error { id, .. } => Some(*id),
        Response::Ready { .. } => None,
    }
}

fn expect_error(response: Response, expected: &str) -> Result<()> {
    match response {
        Response::Error { code, .. } => ensure!(code == expected, "expected {expected}, got {code}"),
        other => bail!("expected Host error {expected}, got {other:?}"),
    }
    Ok(())
}

fn expect_input(response: Response) -> Result<()> {
    match response {
        Response::Result { value: ResultValue::Input { input }, .. } => ensure!(matches!(input.state, InputState::Succeeded), "Host input receipt was not successful"),
        other => bail!("expected successful Host input receipt, got {other:?}"),
    }
    Ok(())
}

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
    let text = serde_json::to_string(value)?;
    ensure!(text.len() <= MAX_CONTROL_MESSAGE, "WebRTC control message exceeds 64 KiB");
    channel.send_text(&text).await?;
    Ok(())
}

async fn wait_for_notify(notify: &Notify, description: &str) -> Result<()> {
    timeout(DEADLINE, notify.notified()).await.with_context(|| format!("timed out waiting for {description}"))?;
    Ok(())
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
            source: obscura_host_protocol::FrameInfo {
                session_id: "session".into(), sequence, document_revision: 0, viewport_revision: 0,
                width: 160, height: 120, stride: 640, byte_length: 160 * 120 * 4,
                pixel_format: PixelFormat::PremultipliedRgba8,
            },
            encoder_id: "encoder".into(), rtp_timestamp, coded_width: 160, coded_height: 120,
            codec: WebRtcVideoCodec::H264AnnexB, pts_us: sequence, keyframe: true,
            access_unit_bytes: 1,
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
        assert_eq!(descriptors.get(&timestamp).expect("matching descriptor").source.sequence, 3);
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
        assert_eq!(descriptors.get(&timestamp).expect("matching descriptor").source.sequence, 2);
    }
}
