//! Local Host bootstrap protocol. Not the remote input/media ABI.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub command: Command,
    pub operation: Option<Operation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    /// Host10 operation identity. Distinct from `sequence`: one user-semantic
    /// operation (a click, a whole drag/pinch, one composition) keeps a single
    /// id while its typed events carry their own `event_sequence`. Legacy
    /// callers that predate Host10 omit it; the Host normalizes to `legacy-N`.
    #[serde(default)]
    pub operation_id: String,
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
    /// History back on the active tab. Rejected with `NO_HISTORY` when the
    /// active tab has no earlier entry; a later navigation truncates forward.
    Back,
    /// History forward on the active tab. Rejected with `NO_HISTORY` when the
    /// active tab has no later entry.
    Forward,
    /// Reload the active tab's current document. Does not create a new history
    /// entry for the same URL.
    Reload,
    /// Create and activate a new blank tab inside the bound profile session.
    /// The new Page shares the profile's one BrowserContext.
    TabCreate {},
    /// Activate an existing tab by Host-assigned tab id. Does not reload it.
    TabSelect { tab_id: String },
    /// Close a tab by Host-assigned tab id. Rejected with `LAST_TAB` when it is
    /// the only tab in the session.
    TabClose { tab_id: String },
    /// Read-only profile catalog query. Grants no control authority.
    ProfileCatalog {},
    /// Create an empty profile in the Host-configured profile root. Does not
    /// clone any existing login state. `make_default` moves the default
    /// identity when true.
    ProfileCreate { display_name: String, make_default: bool },
    /// Rebind this attachment to another profile's live session. The old
    /// session keeps running for its other attachments; this caller is moved
    /// to an observer binding on the target session.
    ProfileSelect { profile_id: String },
    /// Host-local maintenance drain. Stop accepting new browser work, let the
    /// already-accepted local operation terminate, then flush every loaded
    /// profile's cookie/localStorage store. A service may terminate the
    /// process only after this reports `flushed: true`; a failure is a typed
    /// error and never a false `saved`. Never carried across the remote
    /// endpoint.
    FlushProfiles {},
    /// Agent diagnostics only; not a human input operation or read-only API.
    Evaluate { expression: String },
    Resize { width: u32, height: u32 },
    Click { x: f64, y: f64 },
    InputText { text: String },
    Scroll { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    /// Host10 typed remote input. One lease (gesture or composition) is one
    /// operation; the envelope's `event_sequence` orders the events inside it.
    Input { envelope: InputEnvelope },
    CloseSession {},
    /// Host-local WebRTC grant lifecycle. Endpoint calls these on the same
    /// attachment that was created through the mTLS control bootstrap.
    AuthorizeWebRtc { capability: WebRtcCapability },
    ConsumeWebRtc { binding: WebRtcSessionBinding, capability: WebRtcCapability },
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

/// Host-owned projection of one tab. The UI mirrors this; it never becomes a
/// second tab registry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TabSummary {
    pub tab_id: String,
    /// Actual Page URL after the last committed document change.
    pub url: String,
    /// Actual Page title after the last committed document change.
    pub title: String,
    pub active: bool,
}

/// Authoritative navigation state of the active tab, read from the Page after
/// the operation, never from the request or an ACK.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NavigationState {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

/// One persisted catalog record. Never carries cookie contents or a filesystem
/// path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRecord {
    pub profile_id: String,
    pub display_name: String,
    pub default: bool,
}

/// Persisted profile catalog projection. `root_configured` is false when the
/// Host runs without an explicit profile root.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCatalog {
    pub profiles: Vec<ProfileRecord>,
    pub default_profile_id: Option<String>,
    pub root_configured: bool,
}

/// Result of an attachment profile rebind. `session_id` is the target session
/// the caller is now bound to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBinding {
    pub profile_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatus {
    /// New on daemon restart; reconnect never recreates this identity.
    pub session_id: String,
    /// Persisted profile this session belongs to.
    pub profile_id: String,
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
    /// Bumped whenever the active tab changes. A -> B -> A produces a distinct
    /// value so a stale frame/editor binding for tab A is rejected after return.
    pub presentation_revision: u64,
    /// Host-assigned identity of the active tab.
    pub active_tab_id: String,
    /// Bounded active-tab projection. Host is the only tab registry.
    pub tabs: Vec<TabSummary>,
    /// Authoritative navigation state of the active tab.
    pub navigation: NavigationState,
    pub viewport: Option<(f32, f32)>,
    /// Attachment whose declaration supplied the committed viewport; null when unmanaged.
    pub viewport_owner: Option<u64>,
    /// Declaration accepted or layout executing, but not yet committed. Fence new input.
    pub viewport_pending: bool,
    /// Engine-confirmed editing target. `null` when focus is not editable.
    pub editor_context: Option<EditorContext>,
    /// Held gesture/composition lease. New Agent operations are fenced while
    /// this is present; takeover waits for its terminal event.
    pub input_lease: Option<InputLease>,
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
    ProfileCatalog { catalog: ProfileCatalog },
    ProfileCreated { profile: ProfileRecord, catalog: ProfileCatalog },
    ProfileSelected { binding: ProfileBinding, status: SessionStatus },
    /// Host-local maintenance receipt. `flushed` is true only when every
    /// loaded profile store was written successfully; `profiles` is the count
    /// of flushed profile sessions.
    ProfileFlush { flushed: bool, profiles: u32 },
    Evaluation { result: EvaluationResult },
    Closed { closed: bool },
    Input { input: InputReceipt },
    Editing { editing: EditingReceipt },
    WebRtcAuthorization { binding: WebRtcSessionBinding, capability: WebRtcCapability },
    WebRtcConsumed { session_id: String, attachment_id: u64 },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputReceipt { pub state: InputState }
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState { Pending, Succeeded, Cancelled }

/// Identity of the frame the client had presented when it produced an event.
///
/// The Host validates session/document/viewport fencing only. A new frame
/// sequence alone never invalidates input; display rectangle/transform stays a
/// client-local concern and is deliberately absent from the wire.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresentedFrameIdentity {
    pub session_id: String,
    /// Host-assigned tab the presented frame belongs to.
    pub tab_id: String,
    /// Active-tab presentation revision the client had presented.
    pub presentation_revision: u64,
    pub frame_sequence: u64,
    pub document_revision: u64,
    pub viewport_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputBinding {
    Gesture { gesture_session_id: String },
    Editor {
        edit_session_id: String,
        target_id: String,
        focus_revision: u64,
        editor_revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputEvent {
    PointerDown { pointer_id: u64, x: f64, y: f64, button: String, buttons: u64, modifiers: u64 },
    PointerMove { pointer_id: u64, x: f64, y: f64, buttons: u64, modifiers: u64 },
    PointerUp { pointer_id: u64, x: f64, y: f64, button: String, buttons: u64, modifiers: u64 },
    PointerCancel { pointer_id: u64 },
    CompositionBegin { data: String },
    CompositionUpdate { data: String },
    CompositionCommit { data: String },
    CompositionCancel {},
    ReplaceText { text: String, input_type: String },
    DeleteRange { start: u64, end: u64 },
    DeleteSurrounding { before: u64, after: u64 },
    SetSelection { start: u64, end: u64, direction: SelectionDirection },
    EditorAction { action: String },
    KeyDown { key: String, code: String, text: Option<String>, modifiers: u64 },
    KeyUp { key: String, code: String, modifiers: u64 },
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputEnvelope {
    pub event_sequence: u64,
    pub frame: PresentedFrameIdentity,
    pub binding: InputBinding,
    pub event: InputEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorKind { Input, Textarea, ContentEditable }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionDirection { Forward, Backward, None }

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRange {
    /// UTF-16 code-unit offsets into the target's text value.
    pub start: u64,
    pub end: u64,
    pub direction: SelectionDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionState {
    pub data: String,
    /// UTF-16 range currently occupied by the uncommitted composition text.
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaretRect { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }

/// Bounded text around the caret, in UTF-16 code units. Password targets never
/// carry plaintext; only the selection/length metadata remains.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SurroundingText {
    pub text: String,
    /// Offset of `text` inside the full value, in UTF-16 code units.
    pub offset: u64,
    pub before_length: u64,
    pub after_length: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditorContext {
    /// Engine-assigned, document-lifetime-stable node identity.
    pub target_id: String,
    pub kind: EditorKind,
    pub editable: bool,
    pub read_only: bool,
    pub input_type: Option<String>,
    pub input_mode: Option<String>,
    pub enter_key_hint: Option<String>,
    pub multiline: bool,
    pub password: bool,
    pub focus_revision: u64,
    pub editor_revision: u64,
    pub selection: Option<SelectionRange>,
    pub composition: Option<CompositionState>,
    pub caret: Option<CaretRect>,
    pub surrounding: Option<SurroundingText>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLeaseKind { Gesture, Composition }

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputLease {
    pub kind: InputLeaseKind,
    pub operation_id: String,
    pub attachment_id: u64,
    pub gesture_session_id: Option<String>,
    pub edit_session_id: Option<String>,
    pub target_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditingState { Pending, Applied, Cancelled, Rejected, Failed, Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditingReceipt {
    pub operation_id: String,
    pub edit_session_id: String,
    pub target_id: String,
    pub focus_revision: u64,
    pub editor_revision: u64,
    pub event_sequence: u64,
    pub state: EditingState,
    pub selection: Option<SelectionRange>,
    pub composition: Option<CompositionState>,
    pub message: Option<String>,
}

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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrameInfo {
    pub session_id: String,
    /// Host-assigned tab this frame was captured from.
    pub tab_id: String,
    pub sequence: u64,
    /// Active-tab presentation revision at capture time.
    pub presentation_revision: u64,
    pub document_revision: u64,
    pub viewport_revision: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub byte_length: u64,
    pub pixel_format: PixelFormat,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// Terminal failure owned by the media encoder. This is distinct from
    /// Host capture/page availability, which remains recoverable above.
    EncoderUnavailable { session_id: String, message: String },
    Closed { session_id: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec { H264AnnexB }

/// Version 10 pairs with Host11: adds Host-owned tab identity and the active-tab
/// presentation revision to the frame/editor fences, plus the profile catalog
/// and attachment rebind results. Older peers are rejected during Hello.
pub const WEBRTC_PROTOCOL_VERSION: u32 = 10;
/// Host bootstrap wire version. Host11 adds navigation history, the tab
/// registry, the profile catalog/rebind results and profile-root identity to
/// the Host10 operation identity and typed remote editing fences.
pub const HOST_PROTOCOL_VERSION: u32 = 11;
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebRtcControlMessage {
    Hello { capability: WebRtcCapability, binding: WebRtcSessionBinding },
    HelloAck { capability: WebRtcCapability, binding: WebRtcSessionBinding },
    /// A browser request forwarded through the already-authenticated control
    /// attachment. Its operation identity is checked by the Host owner.
    BrowserRequest { request: Request },
    /// The terminal Host response for one `BrowserRequest`, correlated by the
    /// embedded response id. No browser state is reconstructed from metadata.
    BrowserResponse { response: Response },
    Error { code: String, message: String },
    VideoFrame { descriptor: WebRtcVideoFrame },
    Ping { request_id: u64 },
    Pong { request_id: u64 },
}

/// Signaling envelope carried only on the already-authenticated WSS control
/// bootstrap. It is disjoint from `Request`, so SDP cannot be interpreted as a
/// browser operation or status payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebRtcSignal {
    Offer { id: u64, capability: WebRtcCapability, sdp: String },
    Answer { id: u64, capability: WebRtcCapability, binding: WebRtcSessionBinding, sdp: String },
    Error { id: u64, code: String, message: String },
}

/// Source identity sent on the typed DataChannel immediately before the
/// corresponding RTP access unit. The receiver must validate this descriptor
/// against the decoded media order; it may not invent revision metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcVideoFrame {
    /// Exact source descriptor from the Host `VideoPacket::AccessUnit`.
    pub source: FrameInfo,
    /// Exact encoder incarnation from the Host media owner.
    pub encoder_id: String,
    pub rtp_timestamp: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub codec: WebRtcVideoCodec,
    pub pts_us: u64,
    pub keyframe: bool,
    /// Canonical Annex-B bytes reconstructed from the corresponding RTP access unit.
    pub access_unit_bytes: u64,
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
