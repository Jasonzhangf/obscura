//! Isolated WebRTC UDP/H.264/DataChannel capability probe.
//!
//! This module is deliberately feature-gated and has no Host endpoint entry
//! point. It proves the transport slice with two real local PeerConnections;
//! it does not enable a product fallback or replace the existing WSS path.

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use obscura_host_protocol::{
    WEBRTC_CONTROL_LABEL, WEBRTC_H264_FMTP, WEBRTC_PROTOCOL_VERSION, WebRtcCandidateEvidence,
    WebRtcCapability, WebRtcControlMessage, WebRtcDataChannelEvidence, WebRtcFrameEvidence,
    WebRtcNetworkPath, WebRtcProbeReport, WebRtcSessionBinding, WebRtcTransport, WebRtcVideoCodec,
};
use rtc::{
    media::{Sample, io::sample_builder::SampleBuilder},
    media_stream::MediaStreamTrack,
    peer_connection::configuration::media_engine::MIME_TYPE_H264,
    peer_connection::transport::{RTCIceCandidatePair, RTCIceCandidateType, RTCIceProtocol},
    rtp::codec::h264::H264Packet,
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Notify, mpsc, oneshot},
    time::{sleep, timeout},
};
use webrtc_rs::{
    data_channel::{DataChannel, DataChannelEvent},
    media_stream::{
        track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfiguration,
        RTCIceGatheringState, RTCPeerConnectionState,
    },
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const H264_PAYLOAD_TYPE: u8 = 102;
pub(crate) const H264_CLOCK_RATE: u32 = 90_000;
pub(crate) const H264_SSRC: u32 = 0x4f42_5343;
pub(crate) const H264_RTP_MTU: usize = 1200;
pub(crate) const FRAME_DURATION: Duration = Duration::from_millis(333);
const PROBE_FRAME_COUNT: u32 = 2;
const CONTROL_PING_ID: u64 = 1;

/// Inputs for the feature-gated, local-only probe.
///
/// The two bindings model the two independent endpoints that a future
/// signaling owner must bind before enabling a product adapter. They must be
/// equal, but are kept as separate values so a mismatch is testable.
#[derive(Debug, Clone)]
pub struct WebRtcProbeInput {
    pub ffmpeg: PathBuf,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub capability: WebRtcCapability,
    pub offerer_binding: WebRtcSessionBinding,
    pub answerer_binding: WebRtcSessionBinding,
}

/// Run a real same-host two-peer WebRTC probe.
///
/// The probe uses in-memory SDP exchange only as signaling, binds both peers
/// to independent `127.0.0.1:0` UDP sockets, sends the existing Annex B output
/// through RTP, reassembles it on the remote track, decodes one frame with the
/// same FFmpeg toolchain, and completes a typed DataChannel round trip.
pub async fn run_local_webrtc_probe(input: WebRtcProbeInput) -> Result<WebRtcProbeReport> {
    validate_probe_input(&input)?;
    let access_unit = super::encode(&input.ffmpeg, input.width, input.height, input.rgba)
        .await
        .context("encode probe frame through the existing H.264 owner")?;
    ensure!(
        access_unit.len() <= input.capability.max_access_unit as usize,
        "encoded access unit exceeds negotiated WebRTC media budget"
    );

    let coded_width = even_dimension(input.width);
    let coded_height = even_dimension(input.height);

    let offerer_gather = Arc::new(Notify::new());
    let offerer_connected = Arc::new(Notify::new());
    let answerer_gather = Arc::new(Notify::new());
    let answerer_connected = Arc::new(Notify::new());
    let (offerer_track_tx, _offerer_track_rx) = mpsc::channel(1);
    let (offerer_dc_tx, _offerer_dc_rx) = mpsc::channel(1);
    let (answerer_track_tx, answerer_track_rx) = mpsc::channel(1);
    let (answerer_dc_tx, mut answerer_dc_rx) = mpsc::channel(1);

    let offerer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfiguration::default())
            .with_media_engine(h264_media_engine()?)
            .with_handler(Arc::new(ProbeHandler {
                gather_complete: Arc::clone(&offerer_gather),
                connected: Arc::clone(&offerer_connected),
                track_tx: offerer_track_tx,
                data_channel_tx: offerer_dc_tx,
            }))
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
            .build()
            .await
            .context("build offerer PeerConnection")?,
    );

    let answerer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfiguration::default())
            .with_media_engine(h264_media_engine()?)
            .with_handler(Arc::new(ProbeHandler {
                gather_complete: Arc::clone(&answerer_gather),
                connected: Arc::clone(&answerer_connected),
                track_tx: answerer_track_tx,
                data_channel_tx: answerer_dc_tx,
            }))
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
            .build()
            .await
            .context("build answerer PeerConnection")?,
    );

    let local_track = Arc::new(
        TrackLocalStaticSample::new(Instant::now(), h264_media_track())
            .context("create local H.264 sample track")?,
    );
    let sender = offerer
        .add_track(local_track.clone() as Arc<dyn TrackLocal>)
        .await
        .context("add H.264 track to offerer")?;
    let offerer_dc = offerer
        .create_data_channel(WEBRTC_CONTROL_LABEL, None)
        .await
        .context("create typed WebRTC control DataChannel")?;

    let offer = offerer
        .create_offer(None)
        .await
        .context("create WebRTC offer")?;
    offerer
        .set_local_description(offer)
        .await
        .context("set offerer local description")?;
    wait_for_notify(&offerer_gather, "offerer ICE gathering").await?;
    let offer = offerer
        .local_description()
        .await
        .context("offerer local description missing after ICE gathering")?;

    answerer
        .set_remote_description(offer)
        .await
        .context("set answerer remote description")?;
    let answer = answerer
        .create_answer(None)
        .await
        .context("create WebRTC answer")?;
    answerer
        .set_local_description(answer)
        .await
        .context("set answerer local description")?;
    wait_for_notify(&answerer_gather, "answerer ICE gathering").await?;
    let answer = answerer
        .local_description()
        .await
        .context("answerer local description missing after ICE gathering")?;
    offerer
        .set_remote_description(answer)
        .await
        .context("set offerer remote description")?;

    wait_for_notify(&offerer_connected, "offerer ICE/DTLS connection").await?;
    wait_for_notify(&answerer_connected, "answerer ICE/DTLS connection").await?;
    let payload_type = sender
        .get_parameters()
        .await
        .context("read negotiated H.264 sender parameters")?
        .rtp_parameters
        .codecs
        .into_iter()
        .find(|codec| {
            codec
                .rtp_codec
                .mime_type
                .eq_ignore_ascii_case(MIME_TYPE_H264)
        })
        .map(|codec| codec.payload_type)
        .context("negotiated sender has no H.264 payload type")?;

    let remote_dc = timeout(PROBE_TIMEOUT, answerer_dc_rx.recv())
        .await
        .context("timed out waiting for answerer DataChannel")?
        .context("answerer DataChannel event stream closed")?;
    let (control_tx, mut control_rx) = mpsc::channel(4);
    let expected_capability = input.capability.clone();
    let expected_binding = input.answerer_binding.clone();
    let control_task = tokio::spawn(service_answerer_control(
        remote_dc,
        expected_capability,
        expected_binding,
        control_tx,
    ));

    wait_for_data_channel_open(&offerer_dc).await?;
    send_control_message(
        &offerer_dc,
        &WebRtcControlMessage::Hello {
            capability: input.capability.clone(),
            binding: input.offerer_binding.clone(),
        },
    )
    .await?;
    let hello_ack = next_control_message(&mut control_rx, "WebRTC HelloAck").await?;
    match hello_ack {
        WebRtcControlMessage::HelloAck {
            capability,
            binding,
        } => {
            ensure!(
                capability == input.capability,
                "answerer acknowledged a different WebRTC capability"
            );
            ensure!(
                binding == input.offerer_binding,
                "answerer acknowledged a different session binding"
            );
        }
        message => bail!("unexpected WebRTC control message after Hello: {message:?}"),
    }
    send_control_message(
        &offerer_dc,
        &WebRtcControlMessage::Ping {
            request_id: CONTROL_PING_ID,
        },
    )
    .await?;
    match next_control_message(&mut control_rx, "WebRTC Pong").await? {
        WebRtcControlMessage::Pong { request_id } => ensure!(
            request_id == CONTROL_PING_ID,
            "WebRTC control pong request id does not match"
        ),
        message => bail!("unexpected WebRTC control message after Ping: {message:?}"),
    }

    let (track_ready_tx, track_ready_rx) = oneshot::channel();
    let receiver_task = tokio::spawn(receive_and_decode_h264(
        answerer_track_rx,
        input.ffmpeg.clone(),
        coded_width,
        coded_height,
        track_ready_tx,
    ));
    // The first RTP packet opens the remote track. Wait for that typed
    // lifecycle signal before sending the second probe frame; this is not a
    // product retry or fallback path.
    let first_timestamp = Instant::now();
    let send_probe_frame = |frame_index: u32| {
        let local_track = Arc::clone(&local_track);
        let access_unit = access_unit.clone();
        async move {
            let timestamp = first_timestamp + FRAME_DURATION * frame_index;
            let sample = Sample {
                data: access_unit.into(),
                timestamp,
                duration: FRAME_DURATION,
                ..Sample::new(timestamp)
            };
            local_track
                .write_sample(H264_SSRC, payload_type, &sample, &[])
                .await
                .context("write H.264 sample into local RTP packetizer")
        }
    };
    send_probe_frame(0).await?;
    timeout(PROBE_TIMEOUT, track_ready_rx)
        .await
        .context("timed out waiting for remote H.264 track to open")?
        .context("remote H.264 receiver ended before track open")?;
    for frame_index in 1..PROBE_FRAME_COUNT {
        send_probe_frame(frame_index).await?;
    }
    let frame = timeout(PROBE_TIMEOUT + Duration::from_secs(1), receiver_task)
        .await
        .context("timed out waiting for H.264 RTP receive/decode")?
        .context("H.264 RTP receiver task panicked")??;

    let pair = selected_udp_candidate_pair(&offerer).await?;
    let report = WebRtcProbeReport {
        product_enabled: false,
        protocol_version: input.capability.protocol_version,
        session_id: input.offerer_binding.session_id.clone(),
        attachment_id: input.offerer_binding.attachment_id,
        network_path: WebRtcNetworkPath::Local,
        transport: WebRtcTransport::Udp,
        local_candidate: candidate_evidence(pair.local()),
        remote_candidate: candidate_evidence(pair.remote()),
        video_codec: WebRtcVideoCodec::H264AnnexB,
        data_channel: WebRtcDataChannelEvidence {
            label: WEBRTC_CONTROL_LABEL.to_owned(),
            hello_ack: true,
            ping_pong: true,
        },
        frame,
    };

    offerer
        .close()
        .await
        .context("close offerer PeerConnection")?;
    answerer
        .close()
        .await
        .context("close answerer PeerConnection")?;
    let _ = timeout(Duration::from_secs(1), control_task).await;
    Ok(report)
}

fn validate_probe_input(input: &WebRtcProbeInput) -> Result<()> {
    let capability = &input.capability;
    ensure!(
        capability.protocol_version == WEBRTC_PROTOCOL_VERSION,
        "unsupported WebRTC capability protocol version {}",
        capability.protocol_version
    );
    ensure!(
        capability.transport == WebRtcTransport::Udp,
        "WebRTC probe requires the explicitly negotiated UDP transport"
    );
    ensure!(
        capability.video_codec == WebRtcVideoCodec::H264AnnexB,
        "WebRTC probe requires the explicitly negotiated H.264 Annex B codec"
    );
    ensure!(
        capability.data_channel_label == WEBRTC_CONTROL_LABEL,
        "WebRTC control channel label does not match the typed contract"
    );
    ensure!(
        capability.max_access_unit > 0,
        "WebRTC access-unit budget is zero"
    );
    ensure!(
        capability.max_access_unit <= super::MAX_ACCESS_UNIT as u64,
        "WebRTC access-unit budget exceeds the media owner limit"
    );
    ensure!(
        !input.offerer_binding.session_id.is_empty()
            && !input.answerer_binding.session_id.is_empty(),
        "WebRTC session binding requires a session id"
    );
    ensure!(
        input.offerer_binding.attachment_id != 0 && input.answerer_binding.attachment_id != 0,
        "WebRTC session binding requires a non-zero attachment id"
    );
    ensure!(
        !input.offerer_binding.auth_binding.is_empty()
            && !input.answerer_binding.auth_binding.is_empty(),
        "WebRTC session binding requires an opaque auth binding"
    );
    ensure!(
        input.offerer_binding == input.answerer_binding,
        "WebRTC auth/session binding mismatch; reject before transport setup"
    );
    Ok(())
}

pub fn h264_media_engine() -> Result<webrtc_rs::peer_connection::MediaEngine> {
    let mut media_engine = webrtc_rs::peer_connection::MediaEngine::default();
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: H264_CLOCK_RATE,
                    channels: 0,
                    sdp_fmtp_line: WEBRTC_H264_FMTP.to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: H264_PAYLOAD_TYPE,
            },
            RtpCodecKind::Video,
        )
        .context("register H.264 WebRTC codec")?;
    Ok(media_engine)
}

pub(crate) fn h264_media_track() -> MediaStreamTrack {
    MediaStreamTrack::new(
        "obscura-probe-stream".to_owned(),
        "obscura-probe-video".to_owned(),
        "obscura-probe-h264".to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(H264_SSRC),
                ..Default::default()
            },
            codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: H264_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: WEBRTC_H264_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            ..Default::default()
        }],
    )
}

fn even_dimension(value: u32) -> u32 {
    value.saturating_add(1) & !1
}

pub(crate) async fn wait_for_notify(notify: &Notify, description: &str) -> Result<()> {
    timeout(PROBE_TIMEOUT, notify.notified())
        .await
        .with_context(|| format!("timed out waiting for {description}"))?;
    Ok(())
}

async fn wait_for_data_channel_open(dc: &Arc<dyn DataChannel>) -> Result<()> {
    let label = dc.label().await.context("read offerer DataChannel label")?;
    ensure!(
        label == WEBRTC_CONTROL_LABEL,
        "offerer DataChannel label does not match the typed contract"
    );
    loop {
        let event = timeout(PROBE_TIMEOUT, dc.poll())
            .await
            .context("timed out waiting for offerer DataChannel open")?
            .context("offerer DataChannel event stream closed")?;
        match event {
            DataChannelEvent::OnOpen => return Ok(()),
            DataChannelEvent::OnError => bail!("offerer DataChannel reported an error"),
            DataChannelEvent::OnClose => bail!("offerer DataChannel closed before open"),
            _ => {}
        }
    }
}

pub(crate) async fn send_control_message(
    dc: &Arc<dyn DataChannel>,
    message: &WebRtcControlMessage,
) -> Result<()> {
    let text = serde_json::to_string(message).context("serialize typed WebRTC control message")?;
    dc.send_text(&text)
        .await
        .context("send typed WebRTC control message")?;
    Ok(())
}

async fn next_control_message(
    rx: &mut mpsc::Receiver<Result<WebRtcControlMessage>>,
    description: &str,
) -> Result<WebRtcControlMessage> {
    timeout(PROBE_TIMEOUT, rx.recv())
        .await
        .with_context(|| format!("timed out waiting for {description}"))?
        .context("answerer control task ended without a result")?
}

async fn service_answerer_control(
    dc: Arc<dyn DataChannel>,
    expected_capability: WebRtcCapability,
    expected_binding: WebRtcSessionBinding,
    tx: mpsc::Sender<Result<WebRtcControlMessage>>,
) {
    let result: Result<()> = async {
        let label = dc
            .label()
            .await
            .context("read answerer DataChannel label")?;
        ensure!(
            label == WEBRTC_CONTROL_LABEL,
            "answerer DataChannel label does not match the typed contract"
        );
        while let Some(event) = dc.poll().await {
            match event {
                DataChannelEvent::OnMessage(message) => {
                    let text = std::str::from_utf8(&message.data)
                        .context("decode UTF-8 WebRTC control message")?;
                    let control: WebRtcControlMessage =
                        serde_json::from_str(text).context("parse typed WebRTC control message")?;
                    match control {
                        WebRtcControlMessage::Hello {
                            capability,
                            binding,
                        } => {
                            ensure!(
                                capability == expected_capability,
                                "answerer rejected a WebRTC capability mismatch"
                            );
                            ensure!(
                                binding == expected_binding,
                                "answerer rejected a WebRTC auth/session binding mismatch"
                            );
                            let ack = WebRtcControlMessage::HelloAck {
                                capability: expected_capability.clone(),
                                binding: expected_binding.clone(),
                            };
                            send_control_message(&dc, &ack).await?;
                            tx.send(Ok(ack))
                                .await
                                .map_err(|_| anyhow::anyhow!("control result receiver closed"))?;
                        }
                        WebRtcControlMessage::Ping { request_id } => {
                            let pong = WebRtcControlMessage::Pong { request_id };
                            send_control_message(&dc, &pong).await?;
                            tx.send(Ok(pong))
                                .await
                                .map_err(|_| anyhow::anyhow!("control result receiver closed"))?;
                        }
                        message => {
                            bail!("answerer received an out-of-order WebRTC message: {message:?}");
                        }
                    }
                }
                DataChannelEvent::OnError => bail!("answerer DataChannel reported an error"),
                DataChannelEvent::OnClose => bail!("answerer DataChannel closed during probe"),
                _ => {}
            }
        }
        bail!("answerer DataChannel event stream ended")
    }
    .await;

    if let Err(error) = result {
        let _ = tx.send(Err(error)).await;
    }
}

async fn receive_and_decode_h264(
    mut track_rx: mpsc::Receiver<Arc<dyn TrackRemote>>,
    ffmpeg: PathBuf,
    coded_width: u32,
    coded_height: u32,
    track_ready_tx: oneshot::Sender<()>,
) -> Result<WebRtcFrameEvidence> {
    let track = timeout(PROBE_TIMEOUT, track_rx.recv())
        .await
        .context("timed out waiting for remote H.264 RTP track")?
        .context("remote H.264 track event stream closed")?;
    let _ = track_ready_tx.send(());
    let mut builder = SampleBuilder::new(256, H264Packet::default(), H264_CLOCK_RATE)
        .with_max_time_delay(Duration::from_secs(2));
    let mut packet_count = 0u64;

    loop {
        let event = timeout(PROBE_TIMEOUT, track.poll())
            .await
            .context("timed out waiting for remote H.264 RTP packet")?
            .context("remote H.264 track event stream closed")?;
        match event {
            TrackRemoteEvent::OnRtpPacket(packet) => {
                packet_count += 1;
                builder.push(Instant::now(), packet);
                if let Some(sample) = builder.pop(Instant::now()) {
                    ensure!(
                        !sample.data.is_empty(),
                        "H.264 RTP reassembly produced no bytes"
                    );
                    ensure!(
                        sample.data.len() <= super::MAX_ACCESS_UNIT,
                        "reassembled H.264 access unit exceeds media budget"
                    );
                    let (decoded_rgba_bytes, decoded_rgba_checksum, decoded_nonzero) =
                        decode_access_unit(&ffmpeg, coded_width, coded_height, &sample.data)
                            .await?;
                    return Ok(WebRtcFrameEvidence {
                        coded_width,
                        coded_height,
                        rtp_packets_received: packet_count,
                        access_unit_bytes: sample.data.len() as u64,
                        decoded_rgba_bytes: decoded_rgba_bytes as u64,
                        decoded_rgba_checksum,
                        decoded_nonzero,
                    });
                }
            }
            TrackRemoteEvent::OnError => bail!("remote H.264 track reported an error"),
            TrackRemoteEvent::OnEnded => bail!("remote H.264 track ended before a frame decoded"),
            _ => {}
        }
    }
}

async fn decode_access_unit(
    binary: &Path,
    width: u32,
    height: u32,
    access_unit: &[u8],
) -> Result<(usize, u64, bool)> {
    let expected_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("decoded frame size overflow")?;
    let mut child = tokio::process::Command::new(binary)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start FFmpeg H.264 decode verifier")?;
    let mut input = child.stdin.take().context("decoder input missing")?;
    let output = child.stdout.take().context("decoder output missing")?;
    let diagnostic = child.stderr.take().context("decoder diagnostics missing")?;
    let (_, decoded, errors, status) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            async {
                input.write_all(access_unit).await?;
                drop(input);
                Ok::<_, anyhow::Error>(())
            },
            super::bounded_read(output, expected_bytes),
            super::bounded_read(diagnostic, 64 * 1024),
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
        )
    })
    .await
    .context("H.264 decode verifier exceeded five second deadline")??;
    ensure!(
        status.success(),
        "H.264 decode verifier failed: {}",
        String::from_utf8_lossy(&errors)
    );
    ensure!(
        decoded.len() == expected_bytes,
        "H.264 decode verifier returned {} bytes, expected {expected_bytes}",
        decoded.len()
    );
    let checksum = decoded.iter().fold(0u64, |sum, byte| {
        sum.wrapping_mul(131).wrapping_add(u64::from(*byte))
    });
    let nonzero = decoded.iter().any(|byte| *byte != 0);
    ensure!(nonzero, "H.264 decode verifier returned an all-zero frame");
    Ok((decoded.len(), checksum, nonzero))
}

async fn selected_udp_candidate_pair(pc: &Arc<dyn PeerConnection>) -> Result<RTCIceCandidatePair> {
    let sctp = pc
        .sctp()
        .await
        .context("SCTP transport was not negotiated")?;
    let ice = sctp.transport().ice_transport();
    let pair = timeout(PROBE_TIMEOUT, async {
        loop {
            if let Some(pair) = ice
                .get_selected_candidate_pair()
                .await
                .context("read selected ICE candidate pair")?
            {
                return Ok::<_, anyhow::Error>(pair);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("timed out waiting for selected ICE candidate pair")??;
    ensure!(
        pair.local().protocol == RTCIceProtocol::Udp
            && pair.remote().protocol == RTCIceProtocol::Udp,
        "selected ICE candidate pair is not UDP"
    );
    ensure!(
        matches!(pair.local().typ, RTCIceCandidateType::Host)
            && matches!(pair.remote().typ, RTCIceCandidateType::Host),
        "local probe selected a non-host ICE candidate"
    );
    ensure!(
        pair.local()
            .address
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
            && pair
                .remote()
                .address
                .parse::<IpAddr>()
                .map(|address| address.is_loopback())
                .unwrap_or(false),
        "local probe selected a non-loopback candidate"
    );
    Ok(pair)
}

fn candidate_evidence(
    candidate: &rtc::peer_connection::transport::RTCIceCandidate,
) -> WebRtcCandidateEvidence {
    WebRtcCandidateEvidence {
        address: candidate.address.clone(),
        port: candidate.port,
        candidate_type: candidate.typ.to_string(),
        protocol: match candidate.protocol {
            RTCIceProtocol::Udp => WebRtcTransport::Udp,
            RTCIceProtocol::Tcp => WebRtcTransport::Tcp,
            RTCIceProtocol::Unspecified => WebRtcTransport::Tcp,
            _ => WebRtcTransport::Tcp,
        },
    }
}

struct ProbeHandler {
    gather_complete: Arc<Notify>,
    connected: Arc<Notify>,
    track_tx: mpsc::Sender<Arc<dyn TrackRemote>>,
    data_channel_tx: mpsc::Sender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for ProbeHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather_complete.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.connected.notify_one();
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.data_channel_tx.send(data_channel).await;
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.track_tx.send(track).await;
    }
}
