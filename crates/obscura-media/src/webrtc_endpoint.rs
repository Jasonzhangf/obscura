//! Authenticated Host media sender for the explicitly negotiated WebRTC path.
//!
//! The endpoint owns signaling and network admission. This module owns only
//! the H.264 RTP/DataChannel adapter and consumes the encoded-frame stream
//! produced by the existing raw-frame media owner.

use std::{net::SocketAddr, sync::Arc, time::{Duration, Instant}};

use anyhow::{Context, Result, anyhow, bail, ensure};
use obscura_host_protocol::{Request, Response, VideoCodec, VideoPacket, WebRtcCapability, WebRtcControlMessage,
    WebRtcSessionBinding, WebRtcTransport, WebRtcVideoCodec, WebRtcVideoFrame,
    WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION};
use rtc::rtp::{codec::h264::{H264Packet, H264Payloader}, packetizer::{new_packetizer, Depacketizer, Packetizer}, sequence::new_random_sequencer};
use tokio::{sync::{mpsc, Notify, watch}, time::{sleep, timeout}};
use webrtc_rs::{data_channel::{DataChannel, DataChannelEvent},
    media_stream::track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP},
    peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfiguration, RTCIceGatheringState, RTCSessionDescription},};

use crate::{EncodedFrame, MAX_ACCESS_UNIT};
use crate::webrtc::{h264_media_engine, h264_media_track, send_control_message, wait_for_notify,
    MAX_CONTROL_MESSAGE, H264_CLOCK_RATE, H264_PAYLOAD_TYPE, H264_RTP_MTU, H264_SSRC, FRAME_DURATION};

const WEBRTC_DEADLINE: Duration = Duration::from_secs(15);

/// A server-side PeerConnection after the mTLS WSS signaling exchange has
/// produced an SDP answer. It is not usable until the remote DataChannel
/// presents the Host-issued binding.
pub struct WebRtcEndpoint {
    peer: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticRTP>,
    packetizer: Box<dyn Packetizer>,
    data_channels: mpsc::Receiver<Arc<dyn DataChannel>>,
    capability: WebRtcCapability,
    binding: WebRtcSessionBinding,
    latest: watch::Receiver<Option<Arc<EncodedFrame>>>,
    browser_requests: mpsc::Sender<Request>,
    browser_responses: mpsc::Receiver<Response>,
}

/// Accept one complete SDP offer and prepare the answer. ICE candidates are
/// gathered in the SDP; trickle signaling is intentionally not implicit.
pub async fn accept_offer(
    offer_sdp: String,
    capability: WebRtcCapability,
    binding: WebRtcSessionBinding,
    latest: watch::Receiver<Option<Arc<EncodedFrame>>>,
    browser_requests: mpsc::Sender<Request>,
    browser_responses: mpsc::Receiver<Response>,
    bind: SocketAddr,
) -> Result<(String, WebRtcEndpoint)> {
    validate_capability(&capability)?;
    ensure!(!offer_sdp.is_empty(), "WebRTC offer SDP is empty");
    ensure!(binding.attachment_id != 0 && !binding.session_id.is_empty() && !binding.auth_binding.is_empty(),
        "Host WebRTC binding is incomplete");

    let gathered = Arc::new(Notify::new());
    let (data_tx, data_channels) = mpsc::channel(1);
    let peer: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfiguration::default())
            .with_media_engine(h264_media_engine()?)
            .with_handler(Arc::new(EndpointHandler { gathered: Arc::clone(&gathered), data_tx }))
            .with_udp_addrs(vec![bind])
            .build()
            .await
            .context("build Host WebRTC PeerConnection")?,
    );
    let track = Arc::new(TrackLocalStaticRTP::new(h264_media_track()));
    peer.add_track(track.clone() as Arc<dyn TrackLocal>)
        .await
        .context("add Host H.264 WebRTC track")?;
    let packetizer: Box<dyn Packetizer> = Box::new(new_packetizer(
        Instant::now(), H264_RTP_MTU, 0, H264_SSRC,
        Box::new(H264Payloader::default()), Box::new(new_random_sequencer()), H264_CLOCK_RATE,
    ));
    peer.set_remote_description(
        RTCSessionDescription::offer(offer_sdp).context("parse WebRTC offer SDP")?,
    )
    .await
    .context("apply WebRTC offer SDP")?;
    let answer = peer.create_answer(None).await.context("create WebRTC answer SDP")?;
    peer.set_local_description(answer).await.context("apply WebRTC answer SDP")?;
    wait_for_notify(&gathered, "Host WebRTC ICE gathering").await?;
    let answer = peer.local_description().await.context("Host WebRTC answer missing")?;

    Ok((answer.sdp, WebRtcEndpoint {
        peer,
        track,
        packetizer,
        data_channels,
        capability,
        binding,
        latest,
        browser_requests,
        browser_responses,
    }))
}

impl WebRtcEndpoint {
    /// Send the continuous Host stream after DataChannel binding succeeds.
    /// DataChannel carries typed transport control, frame descriptors and the
    /// browser request/response envelope. WSS remains signaling/bootstrap only.
    pub async fn run(self) -> Result<()> {
        let peer = Arc::clone(&self.peer);
        let result = self.run_inner().await;
        let _ = peer.close().await;
        result
    }

    pub async fn close(self) -> Result<()> {
        self.peer.close().await.context("close Host WebRTC PeerConnection")
    }

    async fn run_inner(mut self) -> Result<()> {
        let data_channel = timeout(WEBRTC_DEADLINE, self.data_channels.recv())
            .await
            .context("timed out waiting for remote WebRTC DataChannel")?
            .context("remote WebRTC DataChannel ended before authentication")?;
        let label = data_channel.label().await.context("read WebRTC DataChannel label")?;
        ensure!(label == WEBRTC_CONTROL_LABEL, "WebRTC DataChannel label does not match Host ABI");
        wait_for_open(&data_channel).await?;
        if let Err(error) = authenticate(&data_channel, &self.capability, &self.binding).await {
            let detail = error.to_string();
            let (code, message) = detail.split_once(": ")
                .map_or(("WEBRTC_AUTH_REJECTED", detail.as_str()), |(code, message)| (code, message));
            let rejection = send_control_message(&data_channel, &WebRtcControlMessage::Error {
                code: code.to_owned(), message: message.to_owned(),
            }).await;
            if rejection.is_ok() {
                // DataChannel send queues into the SCTP driver; the bounded grace lets the
                // typed rejection leave before the failed PeerConnection is closed.
                sleep(Duration::from_millis(250)).await;
            }
            return Err(error);
        }

        let mut last_sequence = None;
        let mut last_document_revision = 0;
        let mut last_viewport_revision = 0;
        let mut last_pts = None;
        let mut emit_current = true;
        loop {
            if emit_current {
                let frame = self.latest.borrow_and_update().clone();
                emit_frame(&self.track, &data_channel, frame, &self.binding, &mut last_sequence,
                    &mut last_document_revision, &mut last_viewport_revision, &mut last_pts,
                    self.packetizer.as_mut()).await?;
                emit_current = false;
            }
        tokio::select! {
            changed = self.latest.changed() => {
                if let Err(error) = changed {
                    let terminal_error = self.latest
                        .borrow()
                        .as_deref()
                        .and_then(|frame| terminal_encoder_error(&frame.packet));
                    if let Some((code, message)) = terminal_error {
                        let typed = send_control_message(&data_channel, &WebRtcControlMessage::Error {
                            code: code.clone(), message: message.clone(),
                        }).await;
                        if let Err(send_error) = typed {
                            return Err(anyhow!(
                                "{code}: {message}; typed DataChannel delivery failed: {send_error}"
                            ));
                        }
                        // DataChannel send queues into the SCTP driver; the bounded grace lets
                        // the typed terminal error leave before the failed PeerConnection closes.
                        sleep(Duration::from_millis(250)).await;
                        return Err(anyhow!("{code}: {message}"));
                    }
                    return Err(error).context("Host encoded media stream ended");
                }
                emit_current = true;
            }
            response = self.browser_responses.recv() => {
                let response = response.context("Host browser request pump ended")?;
                send_control_message(&data_channel, &WebRtcControlMessage::BrowserResponse { response }).await?;
            }
            event = data_channel.poll() => match event {
                Some(DataChannelEvent::OnMessage(message)) => {
                    ensure!(message.data.len() <= MAX_CONTROL_MESSAGE, "WebRTC control message exceeds 64 KiB");
                    let control: WebRtcControlMessage = serde_json::from_slice(&message.data)
                            .context("decode WebRTC control message")?;
                        match control {
                            WebRtcControlMessage::Ping { request_id } => {
                                send_control_message(&data_channel, &WebRtcControlMessage::Pong { request_id }).await?;
                            }
                            WebRtcControlMessage::BrowserRequest { request } => {
                                self.browser_requests
                                    .send(request)
                                    .await
                                    .context("send WebRTC browser request to Host pump")?;
                            }
                            WebRtcControlMessage::BrowserResponse { .. } => {
                                bail!("remote sent Host-owned WebRTC browser response")
                            }
                            WebRtcControlMessage::Hello { .. } => bail!("duplicate WebRTC Hello"),
                            WebRtcControlMessage::HelloAck { .. } => bail!("remote sent WebRTC HelloAck"),
                            WebRtcControlMessage::Error { code, message } => bail!("remote sent WebRTC error {code}: {message}"),
                            WebRtcControlMessage::VideoFrame { .. } => bail!("remote sent Host-owned video descriptor"),
                            WebRtcControlMessage::Pong { .. } => bail!("remote sent unsolicited WebRTC Pong"),
                        }
                    }
                    Some(DataChannelEvent::OnError) => bail!("WebRTC DataChannel reported an error"),
                    Some(DataChannelEvent::OnClose) | None => bail!("WebRTC DataChannel closed"),
                    _ => {}
                }
            }
        }
    }
}

async fn wait_for_open(channel: &Arc<dyn DataChannel>) -> Result<()> {
    loop {
        match timeout(WEBRTC_DEADLINE, channel.poll()).await
            .context("timed out waiting for WebRTC DataChannel open")?
            .context("WebRTC DataChannel closed before open")? {
            DataChannelEvent::OnOpen => return Ok(()),
            DataChannelEvent::OnError => bail!("WebRTC DataChannel reported an error before open"),
            DataChannelEvent::OnClose => bail!("WebRTC DataChannel closed before open"),
            _ => {}
        }
    }
}

async fn authenticate(
    channel: &Arc<dyn DataChannel>,
    expected_capability: &WebRtcCapability,
    expected_binding: &WebRtcSessionBinding,
) -> Result<()> {
    let message = timeout(WEBRTC_DEADLINE, channel.poll())
        .await
        .context("timed out waiting for WebRTC Hello")?
        .context("WebRTC DataChannel ended before Hello")?;
    let DataChannelEvent::OnMessage(message) = message else {
        bail!("WebRTC DataChannel did not send Hello after opening");
    };
    ensure!(message.data.len() <= MAX_CONTROL_MESSAGE, "WebRTC control message exceeds 64 KiB");
    let WebRtcControlMessage::Hello { capability, binding } = serde_json::from_slice(&message.data)
        .context("decode WebRTC Hello")? else {
        bail!("WebRTC DataChannel first message must be Hello");
    };
    ensure!(capability == *expected_capability, "WEBRTC_CAPABILITY_MISMATCH: WebRTC capability differs from Host authorization");
    ensure!(binding == *expected_binding, "STALE_WEBRTC_BINDING: WebRTC binding differs from Host authorization");
    send_control_message(channel, &WebRtcControlMessage::HelloAck {
        capability: expected_capability.clone(), binding: expected_binding.clone(),
    }).await
}

async fn emit_frame(
    track: &TrackLocalStaticRTP,
    channel: &Arc<dyn DataChannel>,
    frame: Option<Arc<EncodedFrame>>,
    binding: &WebRtcSessionBinding,
    last_sequence: &mut Option<u64>,
    last_document_revision: &mut u64,
    last_viewport_revision: &mut u64,
    last_pts: &mut Option<u64>,
    packetizer: &mut dyn Packetizer,
) -> Result<()> {
    let Some(frame) = frame else { return Ok(()); };
    if !frame_available(&frame.packet)? { return Ok(()); }
    let VideoPacket::AccessUnit { source, encoder_id, pts_us, coded_width, coded_height, codec, keyframe, byte_length } = &frame.packet else {
        unreachable!("non-access-unit media packet passed the availability guard");
    };
    ensure!(source.session_id == binding.session_id, "Host media session changed during WebRTC stream");
    ensure!(matches!(codec, VideoCodec::H264AnnexB), "Host produced a non-H.264 WebRTC access unit");
    ensure!(*byte_length as usize == frame.bytes.len(), "Host media descriptor does not match access-unit length");
    ensure!(*byte_length <= MAX_ACCESS_UNIT as u64, "Host media access unit exceeds WebRTC budget");
    ensure!(*coded_width == even_dimension(source.width) && *coded_height == even_dimension(source.height),
        "Host coded dimensions do not match source viewport");
    if let Some(sequence) = last_sequence {
        ensure!(source.sequence > *sequence, "Host media frame sequence regressed or repeated");
        ensure!(source.document_revision >= *last_document_revision && source.viewport_revision >= *last_viewport_revision,
            "Host media revision regressed");
    }
    if let Some(previous_pts) = last_pts { ensure!(*pts_us >= *previous_pts, "Host media PTS regressed"); }
    let payload = frame.bytes.as_ref().to_vec().into();
    let packets = packetizer.packetize(
        Instant::now(), &payload,
        (FRAME_DURATION.as_secs_f64() * H264_CLOCK_RATE as f64) as u32,
    ).context("packetize Host H.264 access unit")?;
    ensure!(!packets.is_empty(), "Host H.264 packetizer produced no RTP packets");
    let rtp_timestamp = packets[0].header.timestamp;
    ensure!(packets.iter().all(|packet| packet.header.timestamp == rtp_timestamp),
        "Host H.264 access unit was split across RTP timestamps");
    let access_unit_bytes = canonical_access_unit_len(&packets)?;
    let descriptor = WebRtcVideoFrame {
        source: source.clone(),
        encoder_id: encoder_id.clone(),
        rtp_timestamp,
        coded_width: *coded_width,
        coded_height: *coded_height,
        codec: WebRtcVideoCodec::H264AnnexB,
        pts_us: *pts_us,
        keyframe: *keyframe,
        access_unit_bytes: access_unit_bytes as u64,
    };
    send_control_message(channel, &WebRtcControlMessage::VideoFrame { descriptor }).await?;
    for mut packet in packets {
        packet.header.payload_type = H264_PAYLOAD_TYPE;
        track.write_rtp(packet).await.context("send Host H.264 RTP packet")?;
    }
    *last_sequence = Some(source.sequence);
    *last_document_revision = source.document_revision;
    *last_viewport_revision = source.viewport_revision;
    *last_pts = Some(*pts_us);
    Ok(())
}

fn frame_available(packet: &VideoPacket) -> Result<bool> {
    match packet {
        VideoPacket::AccessUnit { .. } => Ok(true),
        VideoPacket::Waiting { .. } => Ok(false),
        VideoPacket::Closed { .. } => bail!("Host session closed during WebRTC stream"),
        // Host capture/page failures are recoverable states on the raw media
        // stream. Encoder failure is classified only after this state is
        // followed by encoded-stream EOF (see `terminal_encoder_error`).
        VideoPacket::Unavailable { .. } => Ok(false),
    }
}

/// Classify an unavailable packet only after the encoded watch sender has
/// closed. The packet is shared with Host capture/page failures, so its
/// arrival alone is not evidence of an encoder failure.
fn terminal_encoder_error(packet: &VideoPacket) -> Option<(String, String)> {
    let VideoPacket::Unavailable { session_id, message } = packet else { return None; };
    Some((
        "ENCODER_UNAVAILABLE".to_owned(),
        format!("Host media for session {session_id} is unavailable: {message}"),
    ))
}

/// Return the exact Annex-B length produced by the RTP H.264 adapter. The
/// payloader may omit non-media NALUs and the depacketizer emits canonical
/// four-byte start codes, so the encoder's source length is not the wire
/// access-unit length advertised to a receiver.
fn canonical_access_unit_len(packets: &[rtc::rtp::Packet]) -> Result<usize> {
    let mut depacketizer = H264Packet::default();
    let mut length = 0usize;
    for packet in packets {
        let data = depacketizer
            .depacketize(&packet.payload)
            .context("canonicalize Host H.264 RTP access unit")?;
        length = length
            .checked_add(data.len())
            .context("Host H.264 canonical access-unit length overflow")?;
    }
    ensure!(length > 0 && length <= MAX_ACCESS_UNIT, "Host H.264 canonical access unit is empty or exceeds budget");
    Ok(length)
}

fn validate_capability(capability: &WebRtcCapability) -> Result<()> {
    ensure!(capability.protocol_version == WEBRTC_PROTOCOL_VERSION, "unsupported WebRTC protocol version");
    ensure!(capability.transport == WebRtcTransport::Udp, "WebRTC endpoint requires negotiated UDP transport");
    ensure!(capability.video_codec == WebRtcVideoCodec::H264AnnexB, "WebRTC endpoint requires H.264 Annex B");
    ensure!(capability.data_channel_label == WEBRTC_CONTROL_LABEL, "WebRTC DataChannel label mismatch");
    ensure!(capability.max_access_unit > 0 && capability.max_access_unit <= MAX_ACCESS_UNIT as u64,
        "WebRTC access-unit budget exceeds media owner limit");
    Ok(())
}

fn even_dimension(value: u32) -> u32 { value.saturating_add(1) & !1 }

struct EndpointHandler {
    gathered: Arc<Notify>,
    data_tx: mpsc::Sender<Arc<dyn DataChannel>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_media_is_a_recoverable_host_state_until_stream_ends() {
        let packet = VideoPacket::Unavailable {
            session_id: "boundary-session".into(),
            message: "capture temporarily unavailable".into(),
        };
        assert!(!frame_available(&packet).expect("unavailable host state must be accepted"));
    }

    #[test]
    fn terminal_unavailable_media_has_a_typed_encoder_error() {
        let packet = VideoPacket::Unavailable {
            session_id: "boundary-session".into(),
            message: "configured encoder exited".into(),
        };
        let (code, message) = terminal_encoder_error(&packet).expect("closed unavailable stream must classify encoder failure");
        assert_eq!(code, "ENCODER_UNAVAILABLE");
        assert_eq!(message, "Host media for session boundary-session is unavailable: configured encoder exited");
    }

    #[test]
    fn access_unit_and_waiting_states_are_not_terminal_encoder_errors() {
        let waiting = VideoPacket::Waiting { session_id: "boundary-session".into() };
        assert!(terminal_encoder_error(&waiting).is_none());
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for EndpointHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete { self.gathered.notify_one(); }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.data_tx.send(data_channel).await;
    }
}
