//! Local Host bootstrap protocol. Not the remote input/media ABI.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub command: Command,
    pub operation: Option<Operation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub session_id: String,
    pub attachment_id: u64,
    pub sequence: u64,
    pub control_epoch: u64,
    pub viewport_revision: u64,
    pub document_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Attach { mode: Mode, viewport: Option<ViewportDeclaration> },
    /// Attachment layout intent. Host elects one shared viewport; not input authority.
    DeclareViewport { viewport: ViewportDeclaration },
    Detach {},
    Status {},
    RequestTakeover { epoch: u64 },
    ReleaseControl { epoch: u64 },
    ResumeAgent { epoch: u64 },
    Navigate { url: String },
    /// Agent diagnostics only; not a human input operation or read-only API.
    Evaluate { expression: String },
    Resize { width: u32, height: u32 },
    Click { x: f64, y: f64 },
    InputText { text: String },
    Scroll { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    CloseSession {},
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode { Observe, Agent }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewportDeclaration {
    pub device: Device,
    /// Measured available page area in CSS pixels, excluding occupied native UI.
    pub css_width: u32,
    pub css_height: u32,
    /// Explicit device orientation; keyboard occlusion may invert the area ratio.
    pub orientation: Orientation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Device { Phone, Desktop }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation { Portrait, Landscape }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Control {
    pub epoch: u64,
    pub phase: ControlPhase,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlPhase {
    Agent,
    Waiting { attachment_id: u64 },
    Human { attachment_id: u64 },
    Paused,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatus {
    /// New on daemon restart; reconnect never recreates this identity.
    pub session_id: String,
    /// Connection-scoped identity, intentionally new after reconnect.
    pub attachment_id: Option<u64>,
    pub mode: Option<Mode>,
    pub attachments: usize,
    pub agent_attached: bool,
    /// Accepted browser work has not yet delivered its terminal completion.
    pub operation_running: bool,
    pub control: Control,
    pub fault: Option<String>,
    pub next_sequence: u64,
    pub viewport_revision: u64,
    pub document_revision: u64,
    pub viewport: Option<(f32, f32)>,
    /// Attachment whose declaration supplied the committed viewport; null when unmanaged.
    pub viewport_owner: Option<u64>,
    /// Declaration accepted or layout executing, but not yet committed. Fence new input.
    pub viewport_pending: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    pub value: Option<Value>,
    pub js_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResultValue {
    Status(SessionStatus),
    Evaluation { result: EvaluationResult },
    Closed { closed: bool },
    Input { input: InputReceipt },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputReceipt { pub state: InputState }
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState { Succeeded }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ready { version: u32, session_id: String },
    Result { id: u64, value: ResultValue },
    Error { id: u64, code: String, message: String },
}

/// Local diagnostic media stream: one bounded JSON line followed by exactly
/// byte_length bytes for Frame packets. Never sent on the control socket.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FramePacket {
    Waiting { session_id: String },
    Frame { info: FrameInfo },
    Unavailable { session_id: String, message: String },
    Closed { session_id: String },
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrameInfo {
    pub session_id: String,
    pub sequence: u64,
    pub document_revision: u64,
    pub viewport_revision: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub byte_length: u64,
    pub pixel_format: PixelFormat,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat { PremultipliedRgba8 }

/// Local encoded media adapter ABI. Header line, then byte_length Annex B bytes.
/// This descriptor projects Host revisions; it never grants input authority.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VideoPacket {
    Waiting { session_id: String },
    AccessUnit {
        source: FrameInfo,
        encoder_id: String,
        pts_us: u64,
        coded_width: u32,
        coded_height: u32,
        codec: VideoCodec,
        keyframe: bool,
        byte_length: u64,
    },
    Unavailable { session_id: String, message: String },
    Closed { session_id: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec { H264AnnexB }

pub const WEBRTC_PROTOCOL_VERSION: u32 = 1;
pub const WEBRTC_CONTROL_LABEL: &str = "obscura.control.v1";
pub const WEBRTC_H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// A transport capability exchanged on the WebRTC control plane.
///
/// This is a contract for the probe and the future product adapter. It is not
/// inferred from SDP, and it never travels in the browser business payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcCapability {
    pub protocol_version: u32,
    pub transport: WebRtcTransport,
    pub video_codec: WebRtcVideoCodec,
    pub data_channel_label: String,
    pub max_access_unit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRtcTransport { Udp, Tcp }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRtcVideoCodec { H264AnnexB, H265AnnexB }

/// Session identity and opaque authorization binding for a transport attempt.
///
/// The bytes are owned by the Host/session authority. The media probe compares
/// them exactly; it does not mint, derive, or silently replace them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcSessionBinding {
    pub session_id: String,
    pub attachment_id: u64,
    pub auth_binding: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebRtcControlMessage {
    Hello { capability: WebRtcCapability, binding: WebRtcSessionBinding },
    HelloAck { capability: WebRtcCapability, binding: WebRtcSessionBinding },
    Ping { request_id: u64 },
    Pong { request_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebRtcNetworkPath { Local }

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcCandidateEvidence {
    pub address: String,
    pub port: u16,
    pub candidate_type: String,
    pub protocol: WebRtcTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcFrameEvidence {
    pub coded_width: u32,
    pub coded_height: u32,
    pub rtp_packets_received: u64,
    pub access_unit_bytes: u64,
    pub decoded_rgba_bytes: u64,
    pub decoded_rgba_checksum: u64,
    pub decoded_nonzero: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcDataChannelEvidence {
    pub label: String,
    pub hello_ack: bool,
    pub ping_pong: bool,
}

/// Evidence returned by the isolated local two-peer WebRTC probe.
///
/// `product_enabled` is deliberately false: this report proves the transport
/// slice only and does not authorize endpoint or client rollout.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcProbeReport {
    pub product_enabled: bool,
    pub protocol_version: u32,
    pub session_id: String,
    pub attachment_id: u64,
    pub network_path: WebRtcNetworkPath,
    pub transport: WebRtcTransport,
    pub local_candidate: WebRtcCandidateEvidence,
    pub remote_candidate: WebRtcCandidateEvidence,
    pub video_codec: WebRtcVideoCodec,
    pub data_channel: WebRtcDataChannelEvidence,
    pub frame: WebRtcFrameEvidence,
}
