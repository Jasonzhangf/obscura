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
    Attach { mode: Mode },
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
