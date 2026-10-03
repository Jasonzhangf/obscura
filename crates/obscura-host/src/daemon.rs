use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use anyhow::{Context, Result};
use obscura_host_protocol::{Command, Control, ControlPhase, Device, EditorContext, FramePacket, InputBinding, InputEnvelope, InputEvent, InputLease, InputLeaseKind, Mode, Operation, Request, Response, ResultValue, SessionStatus, ViewportDeclaration, WebRtcCapability, WebRtcSessionBinding, WebRtcTransport, WebRtcVideoCodec, HOST_PROTOCOL_VERSION, WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION};
use crate::media;
use crate::worker::{self, Completion, Work, WorkKind};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{timeout, Duration};

const MAX_FRAME: usize = 64 * 1024;
enum Event {
    Request { connection: u64, request: Request, reply: oneshot::Sender<Response> },
    Gone(u64),
    MediaGone,
}

// Created exclusively. Never unlink a pre-existing socket or directory.
struct SocketDir(PathBuf);
impl Drop for SocketDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(self.0.join("host.sock")) {
            if error.kind() != std::io::ErrorKind::NotFound { eprintln!("socket cleanup: {error}"); }
        }
        if let Err(error) = std::fs::remove_file(self.0.join("frames.sock")) {
            if error.kind() != std::io::ErrorKind::NotFound { eprintln!("media socket cleanup: {error}"); }
        }
        if let Err(error) = std::fs::remove_dir(&self.0) { eprintln!("socket directory cleanup: {error}"); }
    }
}

fn error(id: u64, code: &str, message: impl Into<String>) -> Response {
    Response::Error { id, code: code.into(), message: message.into() }
}

fn bounded_response(response: Response) -> Result<Response> {
    if serde_json::to_vec(&response)?.len() > 1024 * 1024 {
        let id = match response { Response::Result { id, .. } | Response::Error { id, .. } => id, Response::Ready { .. } => 0 };
        return Ok(error(id, "RESPONSE_TOO_LARGE", "Operation completed but response exceeds local protocol limit; do not replay"));
    }
    Ok(response)
}
async fn write(stream: &mut tokio::net::unix::OwnedWriteHalf, response: Response) -> Result<()> {
    let mut bytes = serde_json::to_vec(&bounded_response(response)?)?;
    bytes.push(b'\n');
    timeout(Duration::from_secs(2), stream.write_all(&bytes)).await??;
    Ok(())
}

async fn connection(stream: UnixStream, id: u64, session: String, events: mpsc::Sender<Event>) -> Result<()> {
    let (read, mut out) = stream.into_split();
    let mut reader = BufReader::new(read);
    write(&mut out, Response::Ready { version: HOST_PROTOCOL_VERSION, session_id: session }).await?;
    loop {
        let mut frame = Vec::new();
        let size = (&mut reader).take((MAX_FRAME + 1) as u64).read_until(b'\n', &mut frame).await?;
        if size == 0 { return Ok(()); }
        if size > MAX_FRAME || frame.last() != Some(&b'\n') {
            write(&mut out, error(0, "INVALID_FRAME", "Bounded newline JSON required")).await?;
            return Ok(());
        }
        let request: Request = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(_) => { write(&mut out, error(0, "INVALID_REQUEST", "Invalid protocol request")).await?; continue; }
        };
        let (reply, result) = oneshot::channel();
        events.send(Event::Request { connection: id, request, reply }).await?;
        // Disconnect never cancels an accepted mutation. The session owns its outcome.
        write(&mut out, result.await?).await?;
    }
}

struct Session {
    id: String,
    viewport: Option<(f32, f32)>,
    attachments: HashMap<u64, Attachment>,
    agent: Option<u64>,
    viewport_revision: u64,
    document_revision: u64,
    fault: Option<String>,
    control: Control,
    records: HashMap<u64, Record>,
    retired: HashSet<u64>,
    viewport_owner: Option<u64>,
    viewport_update: Option<ViewportUpdate>,
    viewport_repair: Option<(u32, u32)>,
    editor_context: Option<EditorContext>,
    input_lease: Option<InputLease>,
    sequences: HashMap<u64, u64>,
}
struct Attachment { mode: Mode, viewport: Option<ViewportDeclaration>, webrtc: Option<WebRtcGrant> }
struct WebRtcGrant { capability: WebRtcCapability, binding: WebRtcSessionBinding, consumed: bool }
struct ViewportUpdate { owner: Option<u64>, width: u32, height: u32, revision: u64, cancelled: bool, cancel: Arc<AtomicBool> }
struct Record { operation: Operation, command: Command, response: Option<Response>, events: HashMap<u64, (InputEnvelope, Response)>, open: bool }
enum Admission {
    Replied(Response),
    Run(Dispatch),
}
struct Dispatch {
    connection: u64,
    command: Command,
    operation: Operation,
    input: Option<InputEnvelope>,
    fresh: bool,
}

fn replay_response(id: u64, response: &Response) -> Response {
    match response {
        Response::Result { value, .. } => Response::Result { id, value: value.clone() },
        Response::Error { code, message, .. } => error(id, code, message.clone()),
        Response::Ready { .. } => unreachable!("bootstrap responses are never cached"),
    }
}

fn input_event_terminal(event: &InputEvent) -> bool {
    matches!(event,
        InputEvent::PointerUp { .. }
            | InputEvent::PointerCancel { .. }
            | InputEvent::CompositionCommit { .. }
            | InputEvent::CompositionCancel { .. }
            | InputEvent::ReplaceText { .. }
            | InputEvent::DeleteRange { .. }
            | InputEvent::DeleteSurrounding { .. }
            | InputEvent::SetSelection { .. }
            | InputEvent::EditorAction { .. }
            | InputEvent::KeyDown { .. }
            | InputEvent::KeyUp { .. })
}

fn lease_projection(operation: &Operation, command: &Command) -> Option<InputLease> {
    let Command::Input { envelope } = command else { return None; };
    let base = |kind: InputLeaseKind, gesture: Option<String>, edit: Option<String>, target: Option<String>| InputLease {
        kind,
        operation_id: operation.operation_id.clone(),
        attachment_id: operation.attachment_id,
        gesture_session_id: gesture,
        edit_session_id: edit,
        target_id: target,
    };
    match &envelope.binding {
        InputBinding::Gesture { gesture_session_id } => Some(base(InputLeaseKind::Gesture, Some(gesture_session_id.clone()), None, None)),
        InputBinding::Editor { edit_session_id, target_id, .. } => Some(base(InputLeaseKind::Composition, None, Some(edit_session_id.clone()), Some(target_id.clone()))),
    }
}

/// Deterministic pre-execution editing refusals that did not consume the
/// operation sequence. Everything else (including OUTCOME_UNKNOWN) stays
/// recorded so a replay returns the known result instead of re-executing.
fn unconsumed_input_error(code: &str) -> bool {
    matches!(code,
        "STALE_EDITOR_TARGET" | "STALE_FOCUS_REVISION" | "STALE_EDITOR_REVISION"
            | "NO_EDITOR_CONTEXT" | "NOT_EDITABLE" | "INVALID_UTF16_RANGE"
            | "INVALID_EDITOR_EVENT" | "INVALID_EDITOR_ACTION" | "NO_ACTIVE_COMPOSITION"
            | "COMPOSITION_ALREADY_ACTIVE" | "INPUT_REJECTED" | "INPUT_UNAVAILABLE")
}

fn input_envelope(command: &Command) -> Option<&InputEnvelope> {
    match command { Command::Input { envelope } => Some(envelope), _ => None }
}
fn valid_viewport(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width <= 4096 && height <= 4096 && u64::from(width) * u64::from(height) <= 4_194_304
}
fn validate_webrtc_capability(capability: &WebRtcCapability) -> Result<(), String> {
    if capability.protocol_version != WEBRTC_PROTOCOL_VERSION {
        return Err(format!("Unsupported WebRTC protocol version {}", capability.protocol_version));
    }
    if capability.transport != WebRtcTransport::Udp {
        return Err("Only explicitly negotiated UDP WebRTC transport is enabled".into());
    }
    if capability.video_codec != WebRtcVideoCodec::H264AnnexB {
        return Err("Only H.264 Annex B WebRTC media is enabled".into());
    }
    if capability.data_channel_label != WEBRTC_CONTROL_LABEL {
        return Err("WebRTC control DataChannel label does not match the Host ABI".into());
    }
    if capability.max_access_unit == 0 || capability.max_access_unit > obscura_media::MAX_ACCESS_UNIT as u64 {
        return Err("WebRTC access-unit budget exceeds the Host media limit".into());
    }
    Ok(())
}
impl Session {
    fn selected_viewport(&self) -> Option<(u64, ViewportDeclaration)> {
        self.attachments.iter().filter_map(|(&id, attachment)| attachment.viewport.map(|viewport| (id, viewport)))
            .min_by_key(|(id, viewport)| (match viewport.device { Device::Phone => 0, Device::Desktop => 1 },
                u64::from(viewport.css_width) * u64::from(viewport.css_height), viewport.css_width, *id))
    }
    fn viewport_pending(&self) -> bool {
        self.viewport_update.is_some() || self.viewport_repair.is_some() || self.selected_viewport().is_some_and(|(_, viewport)|
            self.viewport != Some((viewport.css_width as f32, viewport.css_height as f32)))
    }
    fn viewport_work(&mut self, owner: Option<u64>, width: u32, height: u32, document_revision: Option<u64>) -> Result<WorkKind, String> {
        let revision = if self.viewport == Some((width as f32, height as f32)) { self.viewport_revision }
            else { self.viewport_revision.checked_add(1).ok_or("Viewport revision exhausted; recreate session")? };
        let cancel = Arc::new(AtomicBool::new(false));
        self.viewport_update = Some(ViewportUpdate { owner, width, height, revision, cancelled: false, cancel: Arc::clone(&cancel) });
        Ok(WorkKind::Viewport { width, height, revision, document_revision, cancel })
    }
    fn detach(&mut self, connection: u64) {
        // A disconnected attachment can no longer deliver the composition's
        // terminal event. End the local lease here so a later field never
        // inherits it and so takeover does not wait forever.
        if self.input_lease.as_ref().is_some_and(|lease| lease.attachment_id == connection) {
            self.input_lease = None;
        }
        self.retired.insert(connection);
        self.attachments.remove(&connection);
        self.sequences.remove(&connection);
        if self.viewport_owner == Some(connection) { self.viewport_owner = None; }
        if let Some(update) = self.viewport_update.as_mut() {
            if update.owner == Some(connection) {
                update.cancelled = true;
                update.cancel.store(true, Ordering::Release);
            }
        }
        if self.agent == Some(connection) { self.agent = None; }
        self.records.remove(&connection);
        if matches!(self.control.phase, ControlPhase::Waiting { attachment_id } | ControlPhase::Human { attachment_id } if attachment_id == connection) {
            self.transition(ControlPhase::Paused);
        }
    }
    fn transition(&mut self, phase: ControlPhase) {
        if let Some(epoch) = self.control.epoch.checked_add(1) {
            self.control = Control { epoch, phase };
        } else { self.fault = Some("Control epoch exhausted; recreate session".into()); }
    }
    fn finish_handover(&mut self) {
        if self.fault.is_none() && self.viewport.is_some() && self.input_lease.is_none() {
            if let ControlPhase::Waiting { attachment_id } = self.control.phase {
                self.transition(ControlPhase::Human { attachment_id });
            }
        }
    }
    fn response_editor_context(&self) -> Option<EditorContext> {
        self.editor_context.clone()
    }
    fn next_sequence(&self, connection: u64) -> u64 {
        self.sequences.get(&connection).copied().unwrap_or(0).saturating_add(1)
    }
    fn binding_fence(&self, id: u64, binding: &InputBinding) -> Option<Response> {
        let InputBinding::Editor { target_id, focus_revision, editor_revision, .. } = binding else { return None; };
        let Some(context) = &self.editor_context else {
            return Some(error(id, "NO_EDITOR_CONTEXT", "No editable target is focused"));
        };
        if context.target_id != *target_id { return Some(error(id, "STALE_EDITOR_TARGET", "Editor target changed")); }
        if context.focus_revision != *focus_revision { return Some(error(id, "STALE_FOCUS_REVISION", "Focus revision changed")); }
        if context.editor_revision != *editor_revision { return Some(error(id, "STALE_EDITOR_REVISION", "Editor revision changed")); }
        if !context.editable { return Some(error(id, "NOT_EDITABLE", "Editor target is not writable")); }
        None
    }
    fn continues_open_lease(&self, connection: u64, operation: &Operation) -> bool {
        self.input_lease.as_ref().is_some_and(|lease|
            lease.attachment_id == connection && lease.operation_id == operation.operation_id)
    }
    fn begin_dispatch(&mut self, dispatch: &Dispatch) -> Option<u64> {
        let previous_sequence = self
            .sequences
            .insert(dispatch.connection, dispatch.operation.sequence);
        let open = dispatch.input.as_ref().is_some_and(|envelope| !input_event_terminal(&envelope.event));
        if dispatch.fresh {
            if open { self.input_lease = lease_projection(&dispatch.operation, &dispatch.command); }
            self.records.insert(dispatch.connection, Record {
                operation: dispatch.operation.clone(),
                command: dispatch.command.clone(),
                response: None,
                events: HashMap::new(),
                open,
            });
        } else if let Some(record) = self.records.get_mut(&dispatch.connection) {
            record.command = dispatch.command.clone();
            record.open = open;
        }
        previous_sequence
    }
    fn status(&self, connection: u64, busy: bool) -> ResultValue {
        ResultValue::Status(SessionStatus {
            session_id: self.id.clone(), attachments: self.attachments.len(),
            attachment_id: self.attachments.contains_key(&connection).then_some(connection),
            mode: self.attachments.get(&connection).map(|attachment| attachment.mode),
            agent_attached: self.agent.is_some(), viewport_revision: self.viewport_revision,
            document_revision: self.document_revision,
            operation_running: busy || self.input_lease.is_some(),
            control: self.control.clone(),
            fault: self.fault.clone(),
            next_sequence: self.next_sequence(connection),
            viewport: self.viewport,
            viewport_owner: self.viewport_owner,
            viewport_pending: self.viewport_pending(),
            editor_context: self.response_editor_context(),
            input_lease: self.input_lease.clone(),
        })
    }
    fn request(&mut self, connection: u64, request: Request, busy: bool) -> Admission {
        let id = request.id;
        let controlled = matches!(request.command, Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. } | Command::Navigate { .. } | Command::Input { .. });
        let mutation = controlled || matches!(request.command, Command::Evaluate { .. } | Command::Resize { .. } | Command::CloseSession {});
        let mut operation: Option<Operation> = None;
        let mut fresh = true;
        if mutation {
            let Some(raw) = request.operation.as_ref() else {
                return Admission::Replied(error(id, "OPERATION_REQUIRED", "Browser mutations require an operation identity"));
            };
            let mut normalized = raw.clone();
            if normalized.operation_id.is_empty() { normalized.operation_id = format!("legacy-{}", normalized.sequence); }
            if normalized.session_id != self.id || normalized.attachment_id != connection {
                return Admission::Replied(error(id, "STALE_ATTACHMENT", "Operation identity belongs to another session or connection"));
            }
            if let Some(previous) = self.records.get(&connection) {
                if previous.operation.operation_id == normalized.operation_id {
                    if previous.operation != normalized {
                        return Admission::Replied(error(id, "OPERATION_CONFLICT", "Operation identity was reused with different fencing"));
                    }
                    match &request.command {
                        Command::Input { envelope } => {
                            if let Some((prior, response)) = previous.events.get(&envelope.event_sequence) {
                                if prior == envelope { return Admission::Replied(replay_response(id, response)); }
                                if let Some(response) = self.binding_fence(id, &envelope.binding) { return Admission::Replied(response); }
                                return Admission::Replied(error(id, "OPERATION_CONFLICT", "Event sequence already belongs to different event content"));
                            }
                            if previous.open {
                                fresh = false;
                            } else {
                                return Admission::Replied(error(id, "OPERATION_EXPIRED", "Operation lease already terminated; use the next operation identity"));
                            }
                        }
                        _ => {
                            if let Some(response) = &previous.response {
                                if previous.command == request.command {
                                    return Admission::Replied(replay_response(id, response));
                                }
                                return Admission::Replied(error(id, "OPERATION_CONFLICT", "Sequence already belongs to different operation content"));
                            }
                            if previous.command != request.command {
                                return Admission::Replied(error(id, "OPERATION_CONFLICT", "Sequence already belongs to different operation content"));
                            }
                            return Admission::Replied(error(id, "OPERATION_RUNNING", "Operation already accepted; do not execute again"));
                        }
                    }
                } else if previous.open {
                    return Admission::Replied(error(id, "OPERATION_BUSY", "Wait for the active gesture/composition lease to terminate"));
                }
            }
            if fresh && normalized.sequence <= self.sequences.get(&connection).copied().unwrap_or(0) {
                return Admission::Replied(error(id, "OPERATION_EXPIRED", "Receipt expired; operation must not be replayed"));
            }
            // An accepted gesture/composition lease outlives the control-epoch
            // fence that admitted it: takeover waits for its terminal event, so
            // the original attachment must still be able to deliver that event
            // under the epoch that granted the lease.
            if !self.continues_open_lease(connection, &normalized) && normalized.control_epoch != self.control.epoch {
                return Admission::Replied(error(id, "STALE_CONTROL", "Control epoch changed"));
            }
            if normalized.viewport_revision != self.viewport_revision { return Admission::Replied(error(id, "STALE_VIEWPORT", "Viewport revision changed")); }
            if normalized.document_revision != self.document_revision { return Admission::Replied(error(id, "STALE_DOCUMENT", "Document revision changed")); }
            if fresh {
                if normalized.sequence != self.next_sequence(connection) {
                    return Admission::Replied(error(id, "INVALID_SEQUENCE", "Use the next sequence from Host status"));
                }
            } else if self.sequences.get(&connection).copied() != Some(normalized.sequence) {
                return Admission::Replied(error(id, "INVALID_SEQUENCE", "Lease events keep the operation sequence"));
            }
            if let Command::Input { envelope } = &request.command {
                if envelope.frame.session_id != self.id
                    || envelope.frame.document_revision != self.document_revision
                    || envelope.frame.viewport_revision != self.viewport_revision {
                    return Admission::Replied(error(id, "STALE_FRAME", "Presented frame identity does not match the current Host revisions"));
                }
            }
            operation = Some(normalized);
        } else if request.operation.is_some() {
            return Admission::Replied(error(id, "INVALID_OPERATION", "Control requests cannot carry browser operation identity"));
        }
        if self.viewport.is_none() { return Admission::Replied(error(id, "SESSION_CLOSED", "Session was explicitly closed")); }
        if self.attachments.contains_key(&connection) && matches!(request.command, Command::Status {} | Command::Detach {}) {
            if matches!(request.command, Command::Detach {}) { self.detach(connection); }
            return Admission::Replied(Response::Result { id, value: self.status(connection, busy) });
        }
        if matches!(&request.command, Command::CloseSession {}) && self.agent == Some(connection) {
            if self.fault.is_none() && !matches!(self.control.phase, ControlPhase::Agent) { return Admission::Replied(error(id, "CONTROL_REQUIRED", "Agent control is suspended")); }
            return if busy { Admission::Replied(error(id, "OPERATION_BUSY", "Wait for the accepted operation to finish")) } else {
                Admission::Run(Dispatch { connection, command: request.command, operation: operation.expect("mutation admission checked identity"), input: None, fresh: true })
            };
        }
        if let Some(fault) = &self.fault { return Admission::Replied(error(id, "SESSION_FAULTED", fault.clone())); }
        match request.command {
            Command::Attach { mode, viewport } => {
                if self.retired.contains(&connection) { return Admission::Replied(error(id, "RECONNECT_REQUIRED", "Detached identities cannot be reused; open a new connection")); }
                if self.attachments.contains_key(&connection) { return Admission::Replied(error(id, "ALREADY_ATTACHED", "Use a new connection to change attachment mode")); }
                if mode == Mode::Agent && self.agent.is_some() { return Admission::Replied(error(id, "AGENT_BUSY", "Agent attachment already exists")); }
                if let Some(viewport) = viewport {
                    if mode != Mode::Observe { return Admission::Replied(error(id, "OBSERVER_REQUIRED", "Only observer attachments declare a viewport")); }
                    if !valid_viewport(viewport.css_width, viewport.css_height) { return Admission::Replied(error(id, "INVALID_VIEWPORT", "Dimensions must be 1..4096 and at most 4194304 CSS pixels")); }
                }
                self.attachments.insert(connection, Attachment { mode, viewport, webrtc: None });
                if mode == Mode::Agent { self.agent = Some(connection); }
                Admission::Replied(Response::Result { id, value: self.status(connection, busy) })
            }
            command => {
                if !self.attachments.contains_key(&connection) { return Admission::Replied(error(id, "ATTACH_REQUIRED", "Attach first")); }
                match &command {
                    Command::AuthorizeWebRtc { capability } => {
                        let attachment = self.attachments.get_mut(&connection).unwrap();
                        if attachment.mode != Mode::Observe {
                            return Admission::Replied(error(id, "OBSERVER_REQUIRED", "WebRTC media requires an observer attachment"));
                        }
                        if let Err(message) = validate_webrtc_capability(capability) {
                            return Admission::Replied(error(id, "WEBRTC_CAPABILITY_REJECTED", message));
                        }
                        if attachment.webrtc.is_some() {
                            return Admission::Replied(error(id, "WEBRTC_GRANT_EXISTS", "WebRTC authorization already exists; detach and reconnect"));
                        }
                        let binding = WebRtcSessionBinding {
                            session_id: self.id.clone(),
                            attachment_id: connection,
                            auth_binding: uuid::Uuid::new_v4().as_bytes().to_vec(),
                        };
                        attachment.webrtc = Some(WebRtcGrant { capability: capability.clone(), binding: binding.clone(), consumed: false });
                        return Admission::Replied(Response::Result { id, value: ResultValue::WebRtcAuthorization { binding, capability: capability.clone() } });
                    }
                    Command::ConsumeWebRtc { binding, capability } => {
                        let Some(attachment) = self.attachments.get_mut(&connection) else {
                            return Admission::Replied(error(id, "ATTACH_REQUIRED", "Attach before consuming a WebRTC authorization"));
                        };
                        let Some(grant) = attachment.webrtc.as_mut() else {
                            return Admission::Replied(error(id, "WEBRTC_GRANT_MISSING", "No Host-issued WebRTC authorization exists"));
                        };
                        if grant.consumed {
                            return Admission::Replied(error(id, "WEBRTC_GRANT_USED", "WebRTC authorization is single-use"));
                        }
                        if &grant.binding != binding {
                            return Admission::Replied(error(id, "STALE_WEBRTC_BINDING", "WebRTC authorization does not match this Session attachment"));
                        }
                        if &grant.capability != capability {
                            return Admission::Replied(error(id, "WEBRTC_CAPABILITY_MISMATCH", "WebRTC capability differs from Host authorization"));
                        }
                        grant.consumed = true;
                        return Admission::Replied(Response::Result { id, value: ResultValue::WebRtcConsumed { session_id: self.id.clone(), attachment_id: connection } });
                    }
                    Command::DeclareViewport { viewport } => {
                        let attachment = self.attachments.get_mut(&connection).unwrap();
                        if attachment.mode != Mode::Observe { return Admission::Replied(error(id, "OBSERVER_REQUIRED", "Only observer attachments declare a viewport")); }
                        if !valid_viewport(viewport.css_width, viewport.css_height) { return Admission::Replied(error(id, "INVALID_VIEWPORT", "Dimensions must be 1..4096 and at most 4194304 CSS pixels")); }
                        attachment.viewport = Some(*viewport);
                        return Admission::Replied(Response::Result { id, value: self.status(connection, busy) });
                    }
                    Command::RequestTakeover { epoch } | Command::ReleaseControl { epoch } | Command::ResumeAgent { epoch } => {
                        if *epoch != self.control.epoch { return Admission::Replied(error(id, "STALE_CONTROL", "Control epoch changed")); }
                        match &command {
                            Command::RequestTakeover { .. } => {
                                if self.agent == Some(connection) { return Admission::Replied(error(id, "OBSERVER_REQUIRED", "Use an observer attachment for human takeover")); }
                                match self.control.phase {
                                    ControlPhase::Agent | ControlPhase::Paused => {
                                        // Acceptance fences Agent immediately, before the worker finishes.
                                        self.transition(ControlPhase::Waiting { attachment_id: connection });
                                        if !busy { self.finish_handover(); }
                                    }
                                    ControlPhase::Waiting { attachment_id } | ControlPhase::Human { attachment_id } if attachment_id == connection => {},
                                    _ => return Admission::Replied(error(id, "CONTROL_BUSY", "Another attachment requested human control")),
                                }
                            }
                            Command::ReleaseControl { .. } => {
                                if !matches!(self.control.phase, ControlPhase::Human { attachment_id } if attachment_id == connection) { return Admission::Replied(error(id, "CONTROL_REQUIRED", "Only the human holder may release control")); }
                                if busy { return Admission::Replied(error(id, "OPERATION_BUSY", "Finish the current operation before release")); }
                                self.transition(ControlPhase::Agent);
                            }
                            Command::ResumeAgent { .. } => {
                                if self.agent != Some(connection) || !matches!(self.control.phase, ControlPhase::Paused) { return Admission::Replied(error(id, "CONTROL_REQUIRED", "Explicit Agent resume requires paused control")); }
                                if busy { return Admission::Replied(error(id, "OPERATION_BUSY", "Finish the accepted operation before resume")); }
                                self.transition(ControlPhase::Agent);
                            }
                            _ => unreachable!(),
                        }
                        return Admission::Replied(Response::Result { id, value: self.status(connection, busy) });
                    }
                    _ => {},
                }
                if controlled {
                    let allowed = matches!(self.control.phase, ControlPhase::Agent) && self.agent == Some(connection)
                        || matches!(self.control.phase, ControlPhase::Human { attachment_id } if attachment_id == connection);
                    let lease_owner = operation.as_ref().is_some_and(|operation| self.continues_open_lease(connection, operation));
                    if !allowed && !lease_owner { return Admission::Replied(error(id, "CONTROL_REQUIRED", "Input requires current control ownership")); }
                } else {
                    if self.agent != Some(connection) { return Admission::Replied(error(id, "AGENT_REQUIRED", "Observer cannot mutate or evaluate JavaScript")); }
                    if !matches!(self.control.phase, ControlPhase::Agent) { return Admission::Replied(error(id, "CONTROL_REQUIRED", "Agent control is suspended")); }
                }
                if self.viewport_pending() { return Admission::Replied(error(id, "VIEWPORT_PENDING", "Wait for the shared viewport to commit")); }
                if busy { return Admission::Replied(error(id, "OPERATION_BUSY", "Wait for the accepted operation to finish")); }
                match &command {
                    Command::Resize { width, height } => {
                        if self.selected_viewport().is_some() { return Admission::Replied(error(id, "VIEWPORT_MANAGED", "Attached viewers select the shared viewport")); }
                        if !valid_viewport(*width, *height) { return Admission::Replied(error(id, "INVALID_VIEWPORT", "Dimensions must be 1..4096 and at most 4194304 CSS pixels")); }
                    }
                    Command::Navigate { url } => {
                        // This local bootstrap surface never enables file navigation.
                        if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("data:text/html,") || url == "about:blank") {
                            return Admission::Replied(error(id, "INVALID_URL", "Unsupported navigation scheme"));
                        }
                    }
                    Command::Evaluate { .. } => {},
                    Command::Click { x, y } | Command::Scroll { x, y, .. } => {
                        if !cfg!(feature = "render") { return Admission::Replied(error(id, "INPUT_UNAVAILABLE", "Coordinate input requires a render-enabled Host")); }
                        let (width, height) = self.viewport.unwrap();
                        if !x.is_finite() || !y.is_finite() || *x < 0.0 || *y < 0.0 || *x >= width as f64 || *y >= height as f64 {
                            return Admission::Replied(error(id, "INVALID_POINT", "Coordinates must be inside the current CSS viewport"));
                        }
                        if let Command::Scroll { delta_x, delta_y, .. } = &command {
                            if !delta_x.is_finite() || !delta_y.is_finite() || delta_x.abs() > 4096.0 || delta_y.abs() > 4096.0 {
                                return Admission::Replied(error(id, "INVALID_SCROLL", "Scroll deltas must be finite and within 4096 CSS pixels"));
                            }
                        }
                    }
                    Command::InputText { text } => {
                        if text.chars().count() > 4096 { return Admission::Replied(error(id, "TEXT_TOO_LARGE", "At most 4096 characters per text operation")); }
                    }
                    Command::Input { envelope } => {
                        let Some(dispatched) = &operation else { unreachable!("typed input is a mutation") };
                        if let Some(response) = self.binding_fence(id, &envelope.binding) {
                            return Admission::Replied(response);
                        }
                        if let Some(existing) = &self.input_lease {
                            let matches_lease = match (&existing.kind, &envelope.binding) {
                                (InputLeaseKind::Gesture, InputBinding::Gesture { gesture_session_id }) => existing.gesture_session_id.as_deref() == Some(gesture_session_id.as_str()),
                                (InputLeaseKind::Composition, InputBinding::Editor { edit_session_id, target_id, .. }) => {
                                    existing.edit_session_id.as_deref() == Some(edit_session_id.as_str())
                                        && existing.target_id.as_deref() == Some(target_id.as_str())
                                        && existing.attachment_id == dispatched.attachment_id
                                        && existing.operation_id == dispatched.operation_id
                                }
                                _ => false,
                            };
                            if !matches_lease {
                                return Admission::Replied(error(id, "INPUT_LEASE_BUSY", "Another gesture/composition lease is active"));
                            }
                        }
                        if matches!(envelope.binding, InputBinding::Gesture { .. }) && !cfg!(feature = "render") {
                            return Admission::Replied(error(id, "INPUT_UNAVAILABLE", "Gesture input requires a render-enabled Host"));
                        }
                        if let InputEvent::ReplaceText { text, .. } = &envelope.event {
                            if text.chars().count() > 4096 { return Admission::Replied(error(id, "TEXT_TOO_LARGE", "At most 4096 characters per editing event")); }
                        }
                        return Admission::Run(Dispatch {
                            connection,
                            command: command.clone(),
                            operation: dispatched.clone(),
                            input: Some(envelope.clone()),
                            fresh,
                        });
                    }
                    _ => unreachable!("control commands handled above"),
                }
                Admission::Run(Dispatch {
                    connection,
                    command,
                    operation: operation.expect("mutation admission checked identity"),
                    input: None,
                    fresh: true,
                })
            }
        }
    }
}

struct Pending {
    operation: Option<PendingOperation>,
    result: oneshot::Receiver<Completion>,
}
struct PendingOperation {
    id: u64,
    connection: u64,
    operation: Operation,
    fresh: bool,
    previous_sequence: Option<u64>,
    reply: oneshot::Sender<Response>,
}

pub async fn serve(socket_dir: PathBuf, allow_private_network: bool) -> Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::DirBuilder::new().mode(0o700).create(&socket_dir).context("socket directory must be new")?;
    let directory = SocketDir(socket_dir);
    let (work, incoming_work) = mpsc::channel(1);
    let (ready, initialized) = oneshot::channel();
    let (faults, mut fault_events) = mpsc::channel(1);
    let (frames, latest) = tokio::sync::watch::channel(media::state(FramePacket::Waiting { session_id: id.clone() }));
    let (interest, interested) = tokio::sync::watch::channel(0usize);
    let worker = worker::start(id.clone(), allow_private_network, incoming_work, ready, faults, frames, interested)?;
    let viewport = initialized.await.context("browser worker stopped during startup")?.map_err(anyhow::Error::msg)?;
    let mut session = Session {
        id: id.clone(),
        viewport: Some(viewport),
        attachments: HashMap::new(),
        agent: None,
        viewport_revision: 0,
        document_revision: 0,
        fault: None,
        control: Control { epoch: 1, phase: ControlPhase::Agent },
        records: HashMap::new(),
        retired: HashSet::new(),
        viewport_owner: None,
        viewport_update: None,
        viewport_repair: None,
        editor_context: None,
        input_lease: None,
        sequences: HashMap::new(),
    };
    let listener = UnixListener::bind(directory.0.join("host.sock"))?;
    std::fs::set_permissions(directory.0.join("host.sock"), std::fs::Permissions::from_mode(0o600))?;
    let media_listener = UnixListener::bind(directory.0.join("frames.sock"))?;
    std::fs::set_permissions(directory.0.join("frames.sock"), std::fs::Permissions::from_mode(0o600))?;
    let media_permits = Arc::new(Semaphore::new(16));
    let mut viewers = 0usize;
    let permits = Arc::new(Semaphore::new(16));
    let (events, mut incoming) = mpsc::channel(16);
    let mut tasks = tokio::task::JoinSet::new();
    let mut counter = 0u64;
    let mut pending: Option<Pending> = None;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        // Declarations remain responsive while atomic work completes. Schedule only
        // the latest elected pair, before admitting any subsequent browser mutation.
        if pending.is_none() && session.fault.is_none() && session.viewport.is_some() {
            let target = session.viewport_repair.take().map(|(width, height)| (None, width, height, true))
                .or_else(|| session.selected_viewport().map(|(owner, viewport)| (Some(owner), viewport.css_width, viewport.css_height, false)));
            match target {
                Some((owner, width, height, force)) if force || session.viewport != Some((width as f32, height as f32)) => {
                    match session.viewport_work(owner, width, height, None) {
                        Ok(kind) => {
                            let (reply, result) = oneshot::channel();
                            if work.try_send(Work { kind, reply }).is_err() {
                                session.viewport_update = None;
                                session.fault = Some("Browser worker unavailable; recreate session explicitly".into());
                            } else { pending = Some(Pending { operation: None, result }); }
                        }
                        Err(cause) => session.fault = Some(cause),
                    }
                }
                Some((owner, _, _, _)) => session.viewport_owner = owner,
                None => session.viewport_owner = None,
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
            accepted = media_listener.accept() => {
                let (socket, _) = accepted?;
                if let Ok(permit) = media_permits.clone().try_acquire_owned() {
                    viewers += 1; interest.send_replace(viewers);
                    let latest = latest.clone(); let events = events.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let result = media::stream(socket, latest).await;
                        let _ = events.send(Event::MediaGone).await;
                        result
                    });
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    counter = counter.checked_add(1).context("connection identity exhausted")?;
                    let connection_id = counter;
                    let sender = events.clone(); let session_id = id.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let result = connection(stream, connection_id, session_id, sender.clone()).await;
                        sender.send(Event::Gone(connection_id)).await.ok();
                        result
                    });
                }
                // At capacity, close the unaccepted socket; no queued unbounded tasks.
            }
            Some(event) = incoming.recv() => match event {
                Event::MediaGone => {
                    viewers = viewers.checked_sub(1).expect("media viewer ownership");
                    interest.send_replace(viewers);
                }
                Event::Gone(connection) => {
                    if session.input_lease.as_ref().is_some_and(|lease| lease.attachment_id == connection) {
                        session.input_lease = None;
                    }
                    if session.records.contains_key(&connection) {
                        session.records.remove(&connection);
                    }
                    session.detach(connection);
                    session.retired.remove(&connection);
                }
                Event::Request { connection, request, reply } => {
                    let id = request.id;
                    match session.request(connection, request, pending.is_some()) {
                        Admission::Replied(response) => { let _ = reply.send(response); }
                        Admission::Run(dispatch) => {
                            let previous_sequence = session.begin_dispatch(&dispatch);
                            let record_operation = dispatch.operation.clone();
                            let kind = if let Command::Resize { width, height } = &dispatch.command {
                                match session.viewport_work(None, *width, *height, Some(record_operation.document_revision)) {
                                    Ok(kind) => kind,
                                    Err(cause) => {
                                        session.fault = Some(cause.clone());
                                        let _ = reply.send(error(id, "SESSION_FAULTED", cause));
                                        session.records.remove(&dispatch.connection);
                                        session.input_lease = None;
                                        continue;
                                    }
                                }
                            } else {
                                WorkKind::Browser {
                                    command: dispatch.command.clone(),
                                    document_revision: record_operation.document_revision,
                                    operation_id: record_operation.operation_id.clone(),
                                }
                            };
                            let (finished, result) = oneshot::channel();
                            if work.try_send(Work { kind, reply: finished }).is_err() {
                                session.viewport_update = None;
                                session.fault = Some("Browser worker unavailable; recreate session explicitly".into());
                                let _ = reply.send(error(id, "SESSION_FAULTED", session.fault.clone().unwrap()));
                                session.records.remove(&dispatch.connection);
                                session.input_lease = None;
                            } else {
                                pending = Some(Pending { operation: Some(PendingOperation {
                                    id,
                                    connection,
                                    operation: record_operation,
                                    fresh: dispatch.fresh,
                                    previous_sequence,
                                    reply,
                                }), result });
                            }
                        }
                    }
                }
            },
            completion = async { (&mut pending.as_mut().unwrap().result).await }, if pending.is_some() => {
                let finished = pending.take().unwrap();
                let viewport_update = session.viewport_update.take();
                let outcome = match completion {
                    Ok(completion) => {
                        // A failed resize may have changed Page partially. Keep the
                        // last committed geometry/revision and fence the faulted Page.
                        if viewport_update.is_none() { session.viewport = completion.viewport; }
                        session.document_revision = session.document_revision.max(completion.document_revision);
                        if completion.fault.is_some() { session.fault = completion.fault; }
                        if let Some(update) = viewport_update {
                            if update.cancelled {
                                if completion.result.is_ok() {
                                    session.viewport_repair = session.viewport.map(|(width, height)| (width as u32, height as u32));
                                }
                            } else if completion.result.is_ok() {
                                if completion.viewport_revision != update.revision || completion.viewport != Some((update.width as f32, update.height as f32)) {
                                    session.fault = Some("Viewport completion mismatched Host assignment; recreate session".into());
                                } else {
                                    session.viewport = completion.viewport;
                                    session.viewport_revision = completion.viewport_revision;
                                    session.viewport_owner = update.owner;
                                }
                            }
                        }
                        if finished.operation.is_none() {
                            if let Err((code, cause)) = &completion.result { session.fault = Some(format!("Viewport application failed: {code}: {cause}")); }
                        }
                        session.editor_context = completion.editor_context.clone();
                        session.finish_handover();
                        match completion.result {
                            Ok(_) if session.fault.is_some() && session.viewport.is_some() => Err(("SESSION_FAULTED", session.fault.clone().unwrap())),
                            result => result,
                        }
                    }
                    Err(_) => {
                        session.fault = Some("Browser worker stopped; operation outcome unknown".into());
                        Err(("OUTCOME_UNKNOWN", session.fault.clone().unwrap()))
                    }
                };
                // Cache only the latest bounded receipt per live attachment. Earlier
                // sequences are rejected, never silently evicted into re-execution.
                if let Some(operation) = finished.operation {
                    let error_code = match &outcome { Err((code, _)) => Some(*code), Ok(_) => None };
                    let unconsumed = operation.fresh && error_code.is_some_and(unconsumed_input_error);
                    let response = match outcome {
                        Ok(value) => Response::Result { id: operation.id, value: value.unwrap_or_else(|| session.status(operation.connection, false)) },
                        Err((code, cause)) => error(operation.id, code, cause),
                    };
                    let response = bounded_response(response)?;
                    let envelope = session.records.get(&operation.connection)
                        .filter(|record| record.operation.operation_id == operation.operation.operation_id)
                        .and_then(|record| input_envelope(&record.command).cloned());
                    if let Some(envelope) = envelope {
                        if unconsumed {
                            session.records.remove(&operation.connection);
                            session.input_lease = None;
                            if let Some(previous_sequence) = operation.previous_sequence {
                                session.sequences.insert(operation.connection, previous_sequence);
                            } else {
                                session.sequences.remove(&operation.connection);
                            }
                        } else if let Some(record) = session.records.get_mut(&operation.connection) {
                            let terminal = input_event_terminal(&envelope.event);
                            record.events.insert(envelope.event_sequence, (envelope, response.clone()));
                            if terminal {
                                record.open = false;
                                if session.input_lease.as_ref().is_some_and(|lease| lease.operation_id == operation.operation.operation_id) {
                                    session.input_lease = None;
                                }
                            }
                        }
                    } else if let Some(record) = session.records.get_mut(&operation.connection) {
                        record.response = Some(response.clone());
                    }
                    session.finish_handover();
                    let _ = operation.reply.send(response);
                }
            },
            update = fault_events.recv(), if session.fault.is_none() => {
                match update {
                    Some(update) => {
                        session.document_revision = session.document_revision.max(update.document_revision);
                        session.fault = update.fault;
                    }
                    None => session.fault = Some("Browser worker stopped".into()),
                }
            },
            Some(completed) = tasks.join_next(), if !tasks.is_empty() => match completed {
                Ok(Ok(())) => {},
                Ok(Err(cause)) => eprintln!("attachment ended: {cause}"),
                Err(cause) => return Err(cause.into()),
            },
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    drop(work);
    // Keep the bounded page-update channel draining while accepted work exits.
    // Otherwise a navigation update can block the worker that we are joining.
    while fault_events.recv().await.is_some() {}
    // The worker finishes accepted work before exiting; never move/drop V8 here.
    tokio::task::spawn_blocking(move || worker.join().map_err(|_| anyhow::anyhow!("browser worker panicked"))?).await??;
    drop(session); drop(listener); drop(media_listener); drop(directory);
    Ok(())
}
