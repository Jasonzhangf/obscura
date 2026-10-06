use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use anyhow::{Context, Result};
use obscura_host_protocol::{Command, Control, ControlPhase, Device, EditorContext, FramePacket, InputBinding, InputEnvelope, InputEvent, InputLease, InputLeaseKind, Mode, NavigationState, Operation, ProfileBinding, Request, Response, ResultValue, SessionStatus, TabSummary, ViewportDeclaration, WebRtcCapability, WebRtcSessionBinding, WebRtcTransport, WebRtcVideoCodec, HOST_PROTOCOL_VERSION, WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION};
use crate::media;
use crate::worker::{self, Completion, Work, WorkKind};
use crate::catalog::{self, ProfileCatalogStore};
use crate::remote_admission::{
    self, AdmissionRead, RemoteAdmissionErrorCode, RemoteAdmissionRequest, RemoteAdmissionResult,
    RemoteConnectionIo, RemotePrincipalContext, RemoteScope,
};
use crate::remote_authorization::{self, AuthFailure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{timeout, Duration};

const MAX_FRAME: usize = 64 * 1024;
enum Event {
    Request { connection: u64, request: Request, reply: oneshot::Sender<Response> },
    /// One typed remote admission request. The main loop owns registry
    /// verification, catalog membership and the route reservation; the
    /// accepted stream task only carries bytes.
    RemoteAdmission { connection: u64, request: RemoteAdmissionRequest, reply: oneshot::Sender<RemoteAdmissionResult> },
    Gone(u64),
    MediaGone(String),
    /// Detach one connection without retiring its identity, for a profile
    /// rebind. Rejected when the caller still owns an open input lease.
    Release { connection: u64, reply: oneshot::Sender<Result<(), &'static str>> },
    /// One typed local-admin request. The main loop owns the catalog store, the
    /// registry validation and the per-connection replay map.
    Admin { connection: u64, request: AdminRequest, reply: oneshot::Sender<AdminResponse> },
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
        if let Err(error) = std::fs::remove_file(self.0.join("remote-admission.sock")) {
            if error.kind() != std::io::ErrorKind::NotFound { eprintln!("remote admission socket cleanup: {error}"); }
        }
        if let Err(error) = std::fs::remove_file(self.0.join("host-admin.sock")) {
            if error.kind() != std::io::ErrorKind::NotFound { eprintln!("host admin socket cleanup: {error}"); }
        }
        // Per-session sources remove their own socket on drop; sweep any that
        // remain so a failed session cleanup does not leave a stale bound path.
        match std::fs::read_dir(&self.0) {
            Ok(entries) => for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("session-") && name.ends_with(".sock") {
                    if let Err(error) = std::fs::remove_file(entry.path()) {
                        if error.kind() != std::io::ErrorKind::NotFound { eprintln!("session media cleanup: {error}"); }
                    }
                }
            },
            Err(error) => eprintln!("socket directory listing failed: {error}"),
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

/// The single browser request/response loop. Both the local Ready-first path
/// and the remote admission path call this with the same retained reader and
/// writer; no path resplits, reconstructs or reads ahead.
async fn browser_loop(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    out: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let mut frame = Vec::new();
        let size = (&mut *reader).take((MAX_FRAME + 1) as u64).read_until(b'\n', &mut frame).await?;
        if size == 0 { return Ok(()); }
        if size > MAX_FRAME || frame.last() != Some(&b'\n') {
            write(out, error(0, "INVALID_FRAME", "Bounded newline JSON required")).await?;
            return Ok(());
        }
        let request: Request = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(_) => { write(out, error(0, "INVALID_REQUEST", "Invalid protocol request")).await?; continue; }
        };
        let (reply, result) = oneshot::channel();
        events.send(Event::Request { connection: id, request, reply }).await?;
        // Disconnect never cancels an accepted mutation. The session owns its outcome.
        write(out, result.await?).await?;
    }
}

/// Local `host.sock` bootstrap: Ready-first, then the shared browser loop.
async fn connection(stream: UnixStream, id: u64, session: String, events: mpsc::Sender<Event>) -> Result<()> {
    let mut io = RemoteConnectionIo::from_stream(stream);
    write(&mut io.writer, Response::Ready { version: HOST_PROTOCOL_VERSION, session_id: session }).await?;
    browser_loop(&mut io.reader, &mut io.writer, id, events).await
}

/// Remote `remote-admission.sock` bootstrap: one split, one retained reader,
/// admission -> typed result -> Ready (only on Ok) -> the same browser loop.
async fn remote_connection(stream: UnixStream, id: u64, events: mpsc::Sender<Event>) -> Result<()> {
    let mut io = RemoteConnectionIo::from_stream(stream);
    let result = match remote_admission::read_remote_admission(&mut io.reader).await? {
        AdmissionRead::FrameDecoded(request) => {
            let (reply, outcome) = oneshot::channel();
            events.send(Event::RemoteAdmission { connection: id, request, reply }).await?;
            match outcome.await {
                Ok(result) => result,
                Err(_) => return Ok(()),
            }
        }
        AdmissionRead::Failure(failure) => remote_admission::admission_error_result(failure),
        AdmissionRead::NoFrameEof => return Ok(()),
    };
    remote_admission::write_remote_admission_result(&mut io.writer, &result).await?;
    let session_id = match result {
        RemoteAdmissionResult::Ok { session_id, .. } => session_id,
        RemoteAdmissionResult::Err { .. } => return Ok(()),
    };
    write(&mut io.writer, Response::Ready { version: HOST_PROTOCOL_VERSION, session_id }).await?;
    browser_loop(&mut io.reader, &mut io.writer, id, events).await
}

struct Session {
    id: String,
    profile_id: String,
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
    terminal_cleanup: Option<TerminalCleanup>,
    sequences: HashMap<u64, u64>,
    presentation_revision: u64,
    active_tab_id: String,
    tabs: Vec<TabSummary>,
    navigation: NavigationState,
    profile: ProfileBinding,
}
struct Attachment { mode: Mode, viewport: Option<ViewportDeclaration>, webrtc: Option<WebRtcGrant> }
struct WebRtcGrant { capability: WebRtcCapability, binding: WebRtcSessionBinding, consumed: bool }
struct ViewportUpdate { owner: Option<u64>, width: u32, height: u32, revision: u64, cancelled: bool, cancel: Arc<AtomicBool> }
struct Record { operation: Operation, command: Command, response: Option<Response>, events: HashMap<u64, (InputEnvelope, Response)>, open: bool }
enum Admission {
    Replied(Response),
    Run(Dispatch),
    HostMutation(Dispatch),
}
struct Dispatch {
    connection: u64,
    command: Command,
    operation: Operation,
    input: Option<InputEnvelope>,
    fresh: bool,
}

/// A terminal input event queued because its attachment disconnected before
/// delivering it. It reuses the original operation identity, binding and frame
/// so the Host never clears the lease before the Engine confirms the terminal
/// outcome.
struct TerminalCleanup {
    connection: u64,
    operation: Operation,
    command: Command,
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

/// Synthesize the terminal event for an open lease without changing its
/// operation identity, binding or frame. A composition is cancelled (never
/// committed) and a gesture receives a pointer cancel. The result is only a
/// request; the Engine still confirms the terminal state before the lease is
/// released.
fn terminal_command(lease: &InputLease, command: &Command) -> Option<Command> {
    let Command::Input { envelope } = command else { return None; };
    let event = match lease.kind {
        InputLeaseKind::Composition => InputEvent::CompositionCancel {},
        InputLeaseKind::Gesture => {
            let pointer_id = match &envelope.event {
                InputEvent::PointerDown { pointer_id, .. }
                | InputEvent::PointerMove { pointer_id, .. }
                | InputEvent::PointerUp { pointer_id, .. } => *pointer_id,
                _ => 0,
            };
            InputEvent::PointerCancel { pointer_id }
        }
    };
    Some(Command::Input {
        envelope: InputEnvelope {
            event_sequence: 0,
            frame: envelope.frame.clone(),
            binding: envelope.binding.clone(),
            event,
        },
    })
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

    /// A disconnected attachment can no longer deliver the terminal event for
    /// its open gesture/composition. Build that terminal under the original
    /// operation identity and binding so the Engine, not the Host, decides the
    /// outcome. The lease is only cleared by the existing completion path once
    /// that terminal event has actually run.
    fn disconnect(&mut self, connection: u64) {
        if self.terminal_cleanup.is_none() {
            let lease = self.input_lease.clone().filter(|lease| lease.attachment_id == connection);
            if let Some(lease) = lease {
                let record = self.records.get(&connection)
                    .filter(|record| record.operation.operation_id == lease.operation_id);
                if let Some((operation, command)) = record.map(|record| (record.operation.clone(), record.command.clone())) {
                    if let Some(command) = terminal_command(&lease, &command) {
                        self.terminal_cleanup = Some(TerminalCleanup { connection, operation, command });
                    }
                }
            }
        }
        self.detach(connection);
    }

    /// Remove one connection's attachment without retiring its identity, for a
    /// profile rebind. Refuses while the caller still owns an open input lease
    /// so a composition is never aborted mid-flight. Other attachments, the
    /// Agent and the document revision are untouched.
    fn release_for_rebind(&mut self, connection: u64) -> Result<(), &'static str> {
        if self.input_lease.as_ref().is_some_and(|lease| lease.attachment_id == connection) {
            return Err("REBIND_BUSY");
        }
        self.attachments.remove(&connection);
        self.records.remove(&connection);
        self.sequences.remove(&connection);
        if self.viewport_owner == Some(connection) { self.viewport_owner = None; }
        if let Some(update) = self.viewport_update.as_mut() {
            if update.owner == Some(connection) {
                update.cancelled = true;
                update.cancel.store(true, Ordering::Release);
            }
        }
        if self.agent == Some(connection) { self.agent = None; }
        if matches!(self.control.phase, ControlPhase::Waiting { attachment_id } | ControlPhase::Human { attachment_id } if attachment_id == connection) {
            self.transition(ControlPhase::Paused);
        }
        Ok(())
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
            profile_id: self.profile_id.clone(),
            attachment_id: self.attachments.contains_key(&connection).then_some(connection),
            mode: self.attachments.get(&connection).map(|attachment| attachment.mode),
            agent_attached: self.agent.is_some(), viewport_revision: self.viewport_revision,
            document_revision: self.document_revision,
            presentation_revision: self.presentation_revision,
            active_tab_id: self.active_tab_id.clone(),
            tabs: self.tabs.clone(),
            navigation: self.navigation.clone(),
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
        let controlled = matches!(request.command, Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. } | Command::Navigate { .. } | Command::Input { .. }
            | Command::Back | Command::Forward | Command::Reload | Command::TabCreate {} | Command::TabSelect { .. } | Command::TabClose { .. });
        let host_mutation = matches!(request.command, Command::ProfileCreate { .. });
        let mutation = controlled || matches!(request.command, Command::Evaluate { .. } | Command::Resize { .. } | Command::CloseSession {}) || host_mutation;
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
                let open_lease = self.continues_open_lease(connection, &normalized);
                if envelope.frame.session_id != self.id {
                    return Admission::Replied(error(id, "STALE_FRAME", "Presented frame identity does not match the current Host revisions"));
                }
                // An open gesture/composition lease continues on the tab it was
                // admitted on, so its own terminal event is not fenced by the
                // active-tab presentation revision.
                if !open_lease && (envelope.frame.tab_id != self.active_tab_id || envelope.frame.presentation_revision != self.presentation_revision) {
                    return Admission::Replied(error(id, "STALE_PRESENTATION", "Presented frame belongs to a different active tab"));
                }
                if envelope.frame.document_revision != self.document_revision || envelope.frame.viewport_revision != self.viewport_revision {
                    return Admission::Replied(error(id, "STALE_FRAME", "Presented frame identity does not match the current Host revisions"));
                }
            }
            operation = Some(normalized);
        } else if request.operation.is_some() {
            return Admission::Replied(error(id, "INVALID_OPERATION", "Control requests cannot carry browser operation identity"));
        }
        if self.viewport.is_none() && !host_mutation { return Admission::Replied(error(id, "SESSION_CLOSED", "Session was explicitly closed")); }
        if self.attachments.contains_key(&connection) && matches!(request.command, Command::Status {} | Command::Detach {}) {
            // An explicit detach drops the authorizing attachment just like a
            // disconnect, so it must also queue the Engine terminal for any
            // open gesture/composition before the lease can be released.
            if matches!(request.command, Command::Detach {}) { self.disconnect(connection); }
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
                } else if !host_mutation {
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
                    Command::Back | Command::Forward | Command::Reload => {},
                    Command::TabCreate {} => {},
                    Command::TabSelect { tab_id } | Command::TabClose { tab_id } => {
                        if tab_id.trim().is_empty() {
                            return Admission::Replied(error(id, "INVALID_TAB", "Tab id must not be empty"));
                        }
                    }
                    Command::Evaluate { .. } => {},
                    // Host-level mutation; catalog persistence owns its effect.
                    Command::ProfileCreate { .. } => {},
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
                let dispatch = Dispatch {
                    connection,
                    command,
                    operation: operation.expect("mutation admission checked identity"),
                    input: None,
                    fresh: true,
                };
                if host_mutation { Admission::HostMutation(dispatch) } else { Admission::Run(dispatch) }
            }
        }
    }
}

struct Pending {
    session_id: String,
    operation: Option<PendingOperation>,
    /// Host-initiated terminal delivery for a disconnected open input lease.
    cleanup: Option<TerminalCleanup>,
    /// True only for a Host-local maintenance flush of this session's store.
    /// Distinguishes a drain completion from the operation/viewport path.
    flush: bool,
    result: oneshot::Receiver<Completion>,
}
/// Host-local maintenance drain: flush each loaded profile's store in turn.
/// A failure stops the drain and is reported typed; `flushed` counts the
/// profiles that were written before the failure.
struct Maintenance {
    id: u64,
    reply: oneshot::Sender<Response>,
    remaining: Vec<String>,
    flushed: u32,
}
struct PendingOperation {
    id: u64,
    connection: u64,
    operation: Operation,
    fresh: bool,
    previous_sequence: Option<u64>,
    reply: oneshot::Sender<Response>,
}

/// One live profile session: its Host-side state, its dedicated worker and the
/// latest frame buffer. One Session == one BrowserContext == one V8 thread.
struct SessionRuntime {
    session: Session,
    work: mpsc::Sender<Work>,
    frames: tokio::sync::watch::Receiver<media::SharedFrame>,
    interest: tokio::sync::watch::Sender<usize>,
    /// Exact-session raw media listener bound at session start. It exists for
    /// every live session, not only the presented one.
    media_source: media::SessionMediaSource,
    viewers: usize,
    join: std::thread::JoinHandle<Result<()>>,
}

/// Start one profile's worker and Host session state. Profile directories are
/// derived from the configured catalog root; the wire never supplies a path.
async fn spawn_session(
    profile_id: String,
    profile_dir: Option<PathBuf>,
    network: bool,
    faults_out: mpsc::Sender<(String, worker::PageUpdate)>,
    socket_dir: &std::path::Path,
) -> Result<SessionRuntime> {
    let id = uuid::Uuid::new_v4().to_string();
    let (work, incoming_work) = mpsc::channel(1);
    let (ready, initialized) = oneshot::channel();
    let (faults, mut fault_events) = mpsc::channel(1);
    let (frames, latest) = tokio::sync::watch::channel(media::state(FramePacket::Waiting { session_id: id.clone() }));
    let (interest, interested) = tokio::sync::watch::channel(0usize);
    let worker = worker::start(id.clone(), profile_dir, network, incoming_work, ready, faults, frames, interested)?;
    let viewport = initialized.await.context("browser worker stopped during startup")?.map_err(anyhow::Error::msg)?;
    // Bind the exact-session source only after the worker is live, so a failed
    // startup leaves no socket or accept task behind.
    let mut media_source = media::SessionMediaSource::bind(socket_dir, &id)
        .context("Bind exact-session media source")?;
    let listener = media_source.take_listener().expect("session media listener is bound");
    tokio::spawn(media::serve_session(listener, latest.clone(), interest.clone(), id.clone()));
    let forward_id = id.clone();
    tokio::spawn(async move {
        while let Some(update) = fault_events.recv().await {
            if faults_out.send((forward_id.clone(), update)).await.is_err() { break; }
        }
    });
    let session = Session {
        id: id.clone(),
        profile_id: profile_id.clone(),
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
        terminal_cleanup: None,
        sequences: HashMap::new(),
        presentation_revision: 0,
        active_tab_id: String::new(),
        tabs: Vec::new(),
        navigation: NavigationState { url: "about:blank".into(), title: String::new(), can_go_back: false, can_go_forward: false },
        profile: ProfileBinding { profile_id: profile_id.clone(), session_id: id.clone() },
    };
    Ok(SessionRuntime { session, work, frames: latest, interest, media_source, viewers: 0, join: worker })
}

/// Host-local reservation for one remote admission. It binds the accepted
/// `remote-admission.sock` stream to exactly one profile session before
/// `Ready`. It grants no attachment, control, frame or media right.
struct Route {
    profile_id: String,
    session_id: String,
    reservation_id: String,
}

/// Re-read the current committed registry and verify the presented principal.
/// This is the per-command authorization truth; a stale or revoked revision is
/// refused with no fallback to a cached projection.
fn verify_current_authorization(
    registry_path: Option<&std::path::Path>,
    principal: &RemotePrincipalContext,
) -> Result<(), AuthFailure> {
    let Some(path) = registry_path else {
        return Err(AuthFailure::new(
            RemoteAdmissionErrorCode::RemoteAuthRequired,
            "Host has no authorization registry configured",
        ));
    };
    let registry = remote_authorization::RegistryFile::load(path).map_err(|cause| {
        AuthFailure::new(RemoteAdmissionErrorCode::RemoteAuthRequired, format!("Authorization registry unavailable: {cause}"))
    })?;
    registry.authorize(principal).map(|_| ())
}

/// Per-command remote gate. Scope and registry revision are re-checked for
/// every request; a bare endpoint allowlist never substitutes for this.
fn remote_request_gate(
    id: u64,
    request: &Request,
    principal: &RemotePrincipalContext,
    registry_path: Option<&std::path::Path>,
) -> Option<Response> {
    let local_only = matches!(
        request.command,
        Command::FlushProfiles {}
            | Command::CloseSession {}
            | Command::Evaluate { .. }
            | Command::Resize { .. }
            | Command::ResumeAgent { .. }
    ) || matches!(request.command, Command::Attach { mode: Mode::Agent, .. });
    if local_only {
        return Some(error(id, "REMOTE_COMMAND_FORBIDDEN", "Remote access permits observation and human browser operations only"));
    }
    if let Err(failure) = verify_current_authorization(registry_path, principal) {
        return Some(error(id, failure.code.as_str(), failure.message));
    }
    let required = match &request.command {
        Command::ProfileCatalog {} => RemoteScope::ProfileRead,
        Command::ProfileCreate { .. } => RemoteScope::ProfileCreate,
        Command::ProfileSelect { .. } => RemoteScope::ProfileSelect,
        _ => RemoteScope::ObserveHumanControl,
    };
    if !principal.scopes.contains(&required) {
        return Some(error(id, "REMOTE_PROFILE_SCOPE_REQUIRED", format!("Remote principal lacks {} scope", required.wire_name())));
    }
    None
}

/// Resolve one completely decoded admission request against the current
/// registry and Host catalog, then reserve the route on the same accepted
/// stream. Every denial is returned as a correlated `RemoteAdmissionResult`.
async fn resolve_remote_admission(
    registry_path: Option<&std::path::Path>,
    catalog: &ProfileCatalogStore,
    sessions: &mut HashMap<String, SessionRuntime>,
    conn_route: &mut HashMap<u64, Route>,
    conn_principal: &mut HashMap<u64, RemotePrincipalContext>,
    socket_dir: &std::path::Path,
    connection: u64,
    request: RemoteAdmissionRequest,
    allow_private_network: bool,
    fault_tx: &mpsc::Sender<(String, worker::PageUpdate)>,
) -> RemoteAdmissionResult {
    let request_id = request.request_id;
    let deny = |code: RemoteAdmissionErrorCode, message: String| RemoteAdmissionResult::denial(request_id, code, message);
    let Some(path) = registry_path else {
        return deny(RemoteAdmissionErrorCode::RemoteAuthRequired, "Host has no authorization registry configured".into());
    };
    let registry = match remote_authorization::RegistryFile::load(path) {
        Ok(registry) => registry,
        Err(cause) => return deny(RemoteAdmissionErrorCode::RemoteAuthRequired, format!("Authorization registry unavailable: {cause}")),
    };
    let grant = match registry.authorize(&request.principal) {
        Ok(grant) => grant,
        Err(failure) => return deny(failure.code, failure.message),
    };
    if request.initial != grant {
        return deny(RemoteAdmissionErrorCode::RemotePrincipalRevoked, "Initial profile grant does not match the current registry".into());
    }
    if request.principal.device_id != request.initial.device_id {
        return deny(RemoteAdmissionErrorCode::RemoteAuthRequired, "Admission principal and initial grant belong to different devices".into());
    }
    if !request.principal.scopes.contains(&RemoteScope::ObserveHumanControl) {
        return deny(RemoteAdmissionErrorCode::RemoteProfileScopeRequired, "Remote principal lacks observe_human_control".into());
    }
    let profile_id = match grant.selection {
        remote_admission::InitialProfileSelection::DefaultProfile => match catalog.default_id() {
            Some(id) => id,
            None => return deny(RemoteAdmissionErrorCode::RemoteInitialProfileUnbound, "Catalog has no default profile".into()),
        },
        remote_admission::InitialProfileSelection::ProfileId(id) => id,
    };
    let Some(entry) = catalog.record(&profile_id) else {
        return deny(RemoteAdmissionErrorCode::RemoteInitialProfileUnbound, format!("Profile {profile_id} does not exist"));
    };
    let Some(access) = entry.access.as_ref() else {
        return deny(RemoteAdmissionErrorCode::ProfileAccessMigrationRequired, format!("Profile {profile_id} has no Host-side access record"));
    };
    if !access.grants(&request.principal.device_id) {
        return deny(RemoteAdmissionErrorCode::RemoteInitialProfileAccessDenied, format!("Profile {profile_id} does not grant this device"));
    }
    let profile_dir = catalog.profile_dir(&profile_id);
    if !sessions.contains_key(&profile_id) {
        match spawn_session(profile_id.clone(), profile_dir, allow_private_network, fault_tx.clone(), socket_dir).await {
            Ok(runtime) => { sessions.insert(profile_id.clone(), runtime); }
            Err(cause) => return deny(RemoteAdmissionErrorCode::RemoteInitialProfileUnbound, format!("Session unavailable: {cause}")),
        }
    }
    let Some(runtime) = sessions.get(&profile_id) else {
        return deny(RemoteAdmissionErrorCode::RemoteInitialProfileUnbound, format!("Session {profile_id} unavailable"));
    };
    let session_id = runtime.session.id.clone();
    let reservation_id = uuid::Uuid::new_v4().to_string();
    conn_route.insert(connection, Route { profile_id, session_id: session_id.clone(), reservation_id: reservation_id.clone() });
    conn_principal.insert(connection, request.principal.clone());
    RemoteAdmissionResult::Ok { request_id, reservation_id, session_id }
}

// ---------------------------------------------------------------------------
// Local admin authority (`host-admin.sock`)
//
// One UTF-8 JSON object plus LF per frame, request/response, same-uid local
// peer only. The catalog store remains the single membership source: this
// layer validates intent, registered devices and revision, then calls the
// existing `ProfileCatalogStore::create`/`assign_access`/`select`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct AdminRequest {
    schema_version: u32,
    id: u64,
    op: String,
    #[serde(default)] intent: Option<String>,
    #[serde(default)] attempt_id: Option<String>,
    #[serde(default)] expected_catalog_revision: Option<u64>,
    #[serde(default)] profile_id: Option<String>,
    #[serde(default)] owner_device_id: Option<String>,
    #[serde(default)] device_id: Option<String>,
    #[serde(default)] members: Option<Vec<String>>,
    #[serde(default)] display_name: Option<String>,
    #[serde(default)] make_default: Option<bool>,
}

impl AdminRequest {
    /// Compare the typed mutating request without the per-frame `id`, so an
    /// exact retry may carry a new request id but any changed field is refused.
    fn same_attempt(&self, other: &AdminRequest) -> bool {
        self.schema_version == other.schema_version
            && self.op == other.op
            && self.intent == other.intent
            && self.attempt_id == other.attempt_id
            && self.expected_catalog_revision == other.expected_catalog_revision
            && self.profile_id == other.profile_id
            && self.owner_device_id == other.owner_device_id
            && self.device_id == other.device_id
            && self.members == other.members
            && self.display_name == other.display_name
            && self.make_default == other.make_default
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AdminProfile {
    profile_id: String,
    display_name: String,
    default: bool,
    owner_device_id: Option<String>,
    members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AdminMembershipReceipt {
    receipt_type: String,
    catalog_revision: u64,
    profile_id: String,
    device_id: String,
    owner_device_id: String,
    members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AdminResponse {
    schema_version: u32,
    id: u64,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")] code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] catalog_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] default_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] profiles: Option<Vec<AdminProfile>>,
    #[serde(skip_serializing_if = "Option::is_none")] profile: Option<AdminProfile>,
    #[serde(skip_serializing_if = "Option::is_none")] membership_receipt: Option<AdminMembershipReceipt>,
}

fn admin_ok(id: u64) -> AdminResponse {
    AdminResponse {
        schema_version: 1, id, state: "ok".into(), code: None, message: None,
        catalog_revision: None, default_profile_id: None, profiles: None, profile: None,
        membership_receipt: None,
    }
}

fn admin_error(id: u64, code: &str, message: impl Into<String>) -> AdminResponse {
    let mut response = admin_ok(id);
    response.state = "error".into();
    response.code = Some(code.into());
    response.message = Some(message.into());
    response
}

fn admin_profile(entry: &catalog::ProfileEntry) -> AdminProfile {
    AdminProfile {
        profile_id: entry.profile_id.clone(),
        display_name: entry.display_name.clone(),
        default: entry.default,
        owner_device_id: entry.access.as_ref().map(|access| access.owner_device_id.clone()),
        members: entry.access.as_ref().map(|access| access.members.iter().cloned().collect()).unwrap_or_default(),
    }
}

#[derive(Default)]
struct AdminConnState {
    replay: HashMap<String, (AdminRequest, AdminResponse)>,
    order: std::collections::VecDeque<String>,
}

impl AdminConnState {
    fn remember(&mut self, attempt_id: String, request: AdminRequest, response: AdminResponse) {
        const MAX_REPLAY: usize = 64;
        if self.replay.insert(attempt_id.clone(), (request, response)).is_none() {
            self.order.push_back(attempt_id);
        }
        while self.order.len() > MAX_REPLAY {
            if let Some(oldest) = self.order.pop_front() { self.replay.remove(&oldest); }
        }
    }
}

fn admin_revision_conflict(catalog: &ProfileCatalogStore, expected: Option<u64>) -> bool {
    expected.is_some_and(|expected| expected != catalog.catalog_revision())
}

fn handle_admin(
    catalog: &mut ProfileCatalogStore,
    registry_path: Option<&std::path::Path>,
    state: &mut AdminConnState,
    request: AdminRequest,
) -> AdminResponse {
    let id = request.id;
    if request.schema_version != 1 {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "schema_version must be 1");
    }
    match request.op.as_str() {
        "admin.catalog.info" => {
            let mut response = admin_ok(id);
            response.catalog_revision = Some(catalog.catalog_revision());
            response.default_profile_id = catalog.default_id();
            response.profiles = Some(catalog.entries().iter().map(admin_profile).collect());
            response
        }
        "admin.catalog.create" => {
            if admin_revision_conflict(catalog, request.expected_catalog_revision) {
                return admin_error(id, "CATALOG_REVISION_CONFLICT", "expected_catalog_revision does not match the committed catalog");
            }
            let Some(display_name) = request.display_name.as_deref() else {
                return admin_error(id, "ADMIN_FRAME_MALFORMED", "display_name is required");
            };
            let make_default = request.make_default.unwrap_or(false);
            match catalog.create(display_name, make_default, catalog::LOCAL_DEVICE_ID) {
                Ok(record) => {
                    let mut response = admin_ok(id);
                    response.catalog_revision = Some(catalog.catalog_revision());
                    response.profile = catalog.record(&record.profile_id).map(admin_profile);
                    response
                }
                Err(error) => admin_error(id, error.code, error.message),
            }
        }
        "admin.catalog.select" => {
            if admin_revision_conflict(catalog, request.expected_catalog_revision) {
                return admin_error(id, "CATALOG_REVISION_CONFLICT", "expected_catalog_revision does not match the committed catalog");
            }
            let Some(profile_id) = request.profile_id.as_deref() else {
                return admin_error(id, "ADMIN_FRAME_MALFORMED", "profile_id is required");
            };
            match catalog.select(profile_id) {
                Ok(record) => {
                    let mut response = admin_ok(id);
                    response.catalog_revision = Some(catalog.catalog_revision());
                    response.default_profile_id = Some(record.profile_id.clone());
                    response.profile = catalog.record(&record.profile_id).map(admin_profile);
                    response
                }
                Err(error) => admin_error(id, error.code, error.message),
            }
        }
        "admin.membership.set" => {
            if let Some(attempt_id) = request.attempt_id.as_deref() {
                if let Some((stored, response)) = state.replay.get(attempt_id) {
                    if stored.same_attempt(&request) {
                        let mut replayed = response.clone();
                        replayed.id = id;
                        return replayed;
                    }
                    return admin_error(id, "ADMIN_FRAME_MALFORMED", "attempt_id replay with a changed field");
                }
            }
            let response = admin_membership_set(catalog, registry_path, &request);
            if response.state == "ok" {
                if let Some(attempt_id) = request.attempt_id.clone() {
                    state.remember(attempt_id, request.clone(), response.clone());
                }
            }
            response
        }
        _ => admin_error(id, "ADMIN_FRAME_MALFORMED", "unknown admin op"),
    }
}

fn admin_membership_set(
    catalog: &mut ProfileCatalogStore,
    registry_path: Option<&std::path::Path>,
    request: &AdminRequest,
) -> AdminResponse {
    let id = request.id;
    let Some(intent) = request.intent.as_deref() else {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "intent is required and must be grant or remove");
    };
    if intent != "grant" && intent != "remove" {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "intent must be exactly grant or remove");
    }
    let Some(attempt_id) = request.attempt_id.as_deref() else {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "attempt_id is required");
    };
    if attempt_id.trim().is_empty() {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "attempt_id must not be empty");
    }
    let (Some(profile_id), Some(owner), Some(device_id), Some(members)) =
        (request.profile_id.as_deref(), request.owner_device_id.as_deref(), request.device_id.as_deref(), request.members.as_ref())
    else {
        return admin_error(id, "ADMIN_FRAME_MALFORMED", "profile_id, owner_device_id, device_id and members are required");
    };
    if admin_revision_conflict(catalog, request.expected_catalog_revision) {
        return admin_error(id, "CATALOG_REVISION_CONFLICT", "expected_catalog_revision does not match the committed catalog");
    }
    if catalog.record(profile_id).is_none() {
        return admin_error(id, "PROFILE_NOT_FOUND", "Unknown profile id");
    }
    let Some(registry_path) = registry_path else {
        return admin_error(id, "PROFILE_ACCESS_FAILED", "authorization registry is not configured");
    };
    let registry = match remote_authorization::RegistryFile::load(registry_path) {
        Ok(registry) => registry,
        Err(error) => return admin_error(id, "PROFILE_ACCESS_FAILED", format!("authorization registry unavailable: {error}")),
    };
    if let Err((code, message)) = validate_membership_devices(&registry, intent, owner, device_id, members) {
        return admin_error(id, code, message);
    }
    let requested: BTreeSet<String> = members.iter().cloned().collect();
    match catalog.assign_access(profile_id, owner, requested) {
        Ok(_) => {}
        Err(error) => return admin_error(id, error.code, error.message),
    }
    let committed = catalog.access_for(profile_id);
    let committed_members: Vec<String> = committed.map(|access| access.members.iter().cloned().collect()).unwrap_or_default();
    let receipt_type = match intent {
        "grant" if committed_members.iter().any(|member| member == device_id) => "export_eligible",
        "remove" if !committed_members.iter().any(|member| member == device_id) => "removed",
        "grant" => return admin_error(id, "MEMBERSHIP_REQUIRED", "granted members do not contain the target device"),
        _ => return admin_error(id, "PROFILE_ACCESS_FAILED", "removed members still contain the target device"),
    };
    let mut response = admin_ok(id);
    response.catalog_revision = Some(catalog.catalog_revision());
    response.membership_receipt = Some(AdminMembershipReceipt {
        receipt_type: receipt_type.into(),
        catalog_revision: catalog.catalog_revision(),
        profile_id: profile_id.to_string(),
        device_id: device_id.to_string(),
        owner_device_id: owner.to_string(),
        members: committed_members,
    });
    response
}

fn validate_membership_devices(
    registry: &remote_authorization::RegistryFile,
    intent: &str,
    owner: &str,
    device_id: &str,
    members: &[String],
) -> Result<(), (&'static str, String)> {
    let active: HashSet<&str> = registry.devices.iter()
        .filter(|device| device.status == remote_authorization::DeviceStatus::Active)
        .map(|device| device.device_id.as_str())
        .collect();
    let device = registry.devices.iter().find(|device| device.device_id == device_id)
        .ok_or(("DEVICE_NOT_REGISTERED", "target device is not registered".to_string()))?;
    if device.status != remote_authorization::DeviceStatus::Active {
        return Err(("DEVICE_REVOKED", "target device is revoked".to_string()));
    }
    if device.scopes.is_empty() {
        return Err(("DEVICE_NOT_REGISTERED", "target device has no scopes".to_string()));
    }
    if device.initial_profile_grant.authorization_revision != registry.authorization_revision {
        return Err(("DEVICE_GRANT_STALE", "target device initial grant is stale".to_string()));
    }
    if owner != catalog::LOCAL_DEVICE_ID && !active.contains(owner) {
        return Err(("DEVICE_NOT_REGISTERED", "profile owner is not an active registered device".to_string()));
    }
    for member in members {
        if member != catalog::LOCAL_DEVICE_ID && !active.contains(member.as_str()) {
            return Err(("DEVICE_NOT_REGISTERED", "member is not an active registered device".to_string()));
        }
    }
    match intent {
        "grant" if !members.iter().any(|member| member == device_id) => {
            return Err(("MEMBERSHIP_REQUIRED", "granted members must contain the target device".to_string()));
        }
        "remove" if members.iter().any(|member| member == device_id) => {
            return Err(("PROFILE_ACCESS_FAILED", "removed members must not contain the target device".to_string()));
        }
        _ => {}
    }
    Ok(())
}

async fn admin_write(write: &mut tokio::net::unix::OwnedWriteHalf, response: AdminResponse) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    timeout(Duration::from_secs(2), write.write_all(&bytes)).await??;
    Ok(())
}

async fn admin_connection(stream: UnixStream, connection: u64, events: mpsc::Sender<Event>) -> Result<()> {
    let peer = stream.peer_cred().context("Read admin peer credentials")?;
    // Safety: geteuid is always safe and never fails.
    let euid = unsafe { libc::geteuid() };
    if peer.uid() != euid {
        anyhow::bail!("ADMIN_PEER_REJECTED: admin socket peer uid does not match the daemon");
    }
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut line = Vec::new();
        let read = {
            let mut bounded = (&mut reader).take(MAX_FRAME as u64 + 1);
            timeout(Duration::from_secs(30), bounded.read_until(b'\n', &mut line)).await
        };
        let count = match read { Ok(result) => result?, Err(_) => return Ok(()) };
        if count == 0 {
            if line.is_empty() { return Ok(()); }
            admin_write(&mut write, admin_error(0, "ADMIN_FRAME_MALFORMED", "partial admin frame at EOF")).await?;
            return Ok(());
        }
        if line.len() > MAX_FRAME || line.last() != Some(&b'\n') {
            admin_write(&mut write, admin_error(0, "ADMIN_FRAME_MALFORMED", "admin frame must be one JSON object plus newline")).await?;
            continue;
        }
        line.pop();
        let request: AdminRequest = match serde_json::from_slice(&line) {
            Ok(request) => request,
            Err(_) => {
                admin_write(&mut write, admin_error(0, "ADMIN_FRAME_MALFORMED", "malformed admin frame")).await?;
                continue;
            }
        };
        let (reply, result) = oneshot::channel();
        if events.send(Event::Admin { connection, request, reply }).await.is_err() { return Ok(()); }
        let response = match result.await { Ok(response) => response, Err(_) => return Ok(()) };
        admin_write(&mut write, response).await?;
    }
}

pub async fn serve(
    socket_dir: PathBuf,
    profile_root: Option<PathBuf>,
    authorization_registry: Option<PathBuf>,
    allow_private_network: bool,
) -> Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(&socket_dir).context("socket directory must be new")?;
    let directory = SocketDir(socket_dir.clone());
    if let Some(root) = &profile_root { catalog::prepare_root(root).map_err(|cause| anyhow::anyhow!("{}: {}", cause.code, cause.message))?; }
    let mut catalog = ProfileCatalogStore::load(profile_root).map_err(|cause| anyhow::anyhow!("{}: {}", cause.code, cause.message))?;
    let default_profile = catalog.default_id().unwrap_or_else(|| "default".to_string());

    let (fault_tx, mut fault_rx) = mpsc::channel::<(String, worker::PageUpdate)>(16);
    let mut sessions: HashMap<String, SessionRuntime> = HashMap::new();
    let default_runtime = spawn_session(default_profile.clone(), catalog.profile_dir(&default_profile), allow_private_network, fault_tx.clone(), &socket_dir).await?;
    let default_session_id = default_runtime.session.id.clone();
    sessions.insert(default_profile.clone(), default_runtime);
    let mut presented = default_profile.clone();
    let mut conn_session: HashMap<u64, String> = HashMap::new();
    let mut conn_route: HashMap<u64, Route> = HashMap::new();
    let mut conn_principal: HashMap<u64, RemotePrincipalContext> = HashMap::new();
    let mut conn_admin: HashMap<u64, AdminConnState> = HashMap::new();

    let listener = UnixListener::bind(socket_dir.join("host.sock"))?;
    std::fs::set_permissions(socket_dir.join("host.sock"), std::fs::Permissions::from_mode(0o600))?;
    let media_listener = UnixListener::bind(socket_dir.join("frames.sock"))?;
    std::fs::set_permissions(socket_dir.join("frames.sock"), std::fs::Permissions::from_mode(0o600))?;
    let remote_listener = UnixListener::bind(socket_dir.join("remote-admission.sock"))?;
    std::fs::set_permissions(socket_dir.join("remote-admission.sock"), std::fs::Permissions::from_mode(0o600))?;
    let admin_listener = UnixListener::bind(socket_dir.join("host-admin.sock"))?;
    std::fs::set_permissions(socket_dir.join("host-admin.sock"), std::fs::Permissions::from_mode(0o600))?;
    let media_permits = Arc::new(Semaphore::new(16));
    let permits = Arc::new(Semaphore::new(16));
    let (events, mut incoming) = mpsc::channel(64);
    let mut tasks = tokio::task::JoinSet::new();
    // Requests that arrived for a session whose disconnected lease is still
    // delivering its Engine terminal. They are re-injected once cleanup
    // completes so no caller observes a lease released before the Engine.
    let mut deferred: Vec<(String, u64, Request, oneshot::Sender<Response>)> = Vec::new();
    let mut counter = 0u64;
    let mut pending: Option<Pending> = None;
    let mut maintenance: Option<Maintenance> = None;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        // Declarations remain responsive while atomic work completes. Schedule only
        // the latest elected pair, before admitting any subsequent browser mutation.
        if pending.is_none() {
            // A disconnected attachment's open lease still owes the Engine a
            // terminal event. Deliver it before any new input can be admitted.
            let mut cleanup: Option<(String, TerminalCleanup)> = None;
            for (sid, runtime) in sessions.iter_mut() {
                if let Some(terminal) = runtime.session.terminal_cleanup.take() {
                    cleanup = Some((sid.clone(), terminal));
                    break;
                }
            }
            if let Some((sid, terminal)) = cleanup {
                let runtime = sessions.get_mut(&sid).expect("cleanup session exists");
                let kind = WorkKind::Browser {
                    command: terminal.command.clone(),
                    document_revision: terminal.operation.document_revision,
                    operation_id: terminal.operation.operation_id.clone(),
                };
                let (reply, result) = oneshot::channel();
                if runtime.work.try_send(Work { kind, reply }).is_err() {
                    runtime.session.fault = Some("Browser worker unavailable; input terminal outcome unknown".into());
                } else {
                    pending = Some(Pending { session_id: sid, operation: None, cleanup: Some(terminal), flush: false, result });
                }
            }
            // Re-inject callers that were deferred while a disconnected lease
            // delivered its Engine terminal. Only sessions with no remaining
            // cleanup work are released back into the request path.
            if !deferred.is_empty() {
                let waiting = std::mem::take(&mut deferred);
                for (waiting_session, waiting_connection, waiting_request, waiting_reply) in waiting {
                    let still_inflight = pending.as_ref().is_some_and(|pending| pending.cleanup.is_some() && pending.session_id == waiting_session)
                        || sessions.get(&waiting_session).is_some_and(|runtime| runtime.session.terminal_cleanup.is_some());
                    if still_inflight {
                        deferred.push((waiting_session, waiting_connection, waiting_request, waiting_reply));
                        continue;
                    }
                    let event = Event::Request {
                        connection: waiting_connection,
                        request: waiting_request,
                        reply: waiting_reply,
                    };
                    if let Err(tokio::sync::mpsc::error::TrySendError::Full(event)) = events.try_send(event) {
                        if let Event::Request { connection, request, reply } = event {
                            deferred.push((waiting_session, connection, request, reply));
                        }
                    }
                }
            }
            // Flush one loaded profile per loop turn so a failure is bound to a
            // single profile and never masks the others.
            let next = if pending.is_none() { maintenance.as_mut().and_then(|drain| drain.remaining.pop()) } else { None };
            if let Some(next) = next {
                match sessions.get_mut(&next) {
                    Some(runtime) => {
                        let (reply, result) = oneshot::channel();
                        match runtime.work.try_send(Work { kind: WorkKind::Flush, reply }) {
                            Ok(()) => pending = Some(Pending { session_id: next, operation: None, cleanup: None, flush: true, result }),
                            Err(_) => {
                                let drain = maintenance.take().expect("maintenance owner");
                                let _ = drain.reply.send(error(drain.id, "SESSION_FAULTED", "Browser worker unavailable for maintenance flush"));
                            }
                        }
                    }
                    None => {}
                }
            } else if pending.is_none() {
                if let Some(drain) = maintenance.take() {
                let _ = drain.reply.send(Response::Result { id: drain.id, value: ResultValue::ProfileFlush { flushed: true, profiles: drain.flushed } });
                }
            }
            let mut scheduled: Option<(String, WorkKind)> = None;
            let scheduling = maintenance.is_none() && pending.is_none();
            for (sid, runtime) in sessions.iter_mut().filter(|_| scheduling) {
                let session = &mut runtime.session;
                if session.fault.is_some() || session.viewport.is_none() { continue; }
                let target = session.viewport_repair.take().map(|(width, height)| (None, width, height, true))
                    .or_else(|| session.selected_viewport().map(|(owner, viewport)| (Some(owner), viewport.css_width, viewport.css_height, false)));
                match target {
                    Some((owner, width, height, force)) if force || session.viewport != Some((width as f32, height as f32)) => {
                        match session.viewport_work(owner, width, height, None) {
                            Ok(kind) => { scheduled = Some((sid.clone(), kind)); break; }
                            Err(cause) => session.fault = Some(cause),
                        }
                    }
                    Some((owner, _, _, _)) => session.viewport_owner = owner,
                    None => session.viewport_owner = None,
                }
            }
            if let Some((sid, kind)) = scheduled {
                let runtime = sessions.get_mut(&sid).expect("scheduled session exists");
                let (reply, result) = oneshot::channel();
                if runtime.work.try_send(Work { kind, reply }).is_err() {
                    runtime.session.viewport_update = None;
                    runtime.session.fault = Some("Browser worker unavailable; recreate session explicitly".into());
                } else { pending = Some(Pending { session_id: sid, operation: None, cleanup: None, flush: false, result }); }
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
            accepted = media_listener.accept() => {
                let (socket, _) = accepted?;
                if let Ok(permit) = media_permits.clone().try_acquire_owned() {
                    if let Some(runtime) = sessions.get_mut(&presented) {
                        runtime.viewers += 1;
                        runtime.interest.send_replace(runtime.viewers);
                        let latest = runtime.frames.clone();
                        let events = events.clone();
                        let session = presented.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            let result = media::stream(socket, latest).await;
                            let _ = events.send(Event::MediaGone(session)).await;
                            result
                        });
                    }
                }
            }
            accepted = remote_listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    counter = counter.checked_add(1).context("connection identity exhausted")?;
                    let connection_id = counter;
                    let sender = events.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let result = remote_connection(stream, connection_id, sender.clone()).await;
                        sender.send(Event::Gone(connection_id)).await.ok();
                        result
                    });
                }
            }
            accepted = admin_listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    counter = counter.checked_add(1).context("connection identity exhausted")?;
                    let connection_id = counter;
                    let sender = events.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let result = admin_connection(stream, connection_id, sender.clone()).await;
                        sender.send(Event::Gone(connection_id)).await.ok();
                        result
                    });
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    counter = counter.checked_add(1).context("connection identity exhausted")?;
                    let connection_id = counter;
                    let sender = events.clone();
                    let session_id = sessions.get(&presented).map(|runtime| runtime.session.id.clone()).unwrap_or_else(|| default_session_id.clone());
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
                Event::MediaGone(session) => {
                    if let Some(runtime) = sessions.get_mut(&session) {
                        runtime.viewers = runtime.viewers.saturating_sub(1);
                        runtime.interest.send_replace(runtime.viewers);
                    }
                }
                Event::Gone(connection) => {
                    if let Some(sid) = conn_session.remove(&connection) {
                        if let Some(runtime) = sessions.get_mut(&sid) {
                            let session = &mut runtime.session;
                            session.disconnect(connection);
                            session.retired.remove(&connection);
                        }
                    }
                    conn_route.remove(&connection);
                    conn_principal.remove(&connection);
                    conn_admin.remove(&connection);
                }
                Event::Admin { connection, request, reply } => {
                    let state = conn_admin.entry(connection).or_default();
                    let response = handle_admin(&mut catalog, authorization_registry.as_deref(), state, request);
                    let _ = reply.send(response);
                }
                Event::RemoteAdmission { connection, request, reply } => {
                    let outcome = resolve_remote_admission(
                        authorization_registry.as_deref(),
                        &catalog,
                        &mut sessions,
                        &mut conn_route,
                        &mut conn_principal,
                        &socket_dir,
                        connection,
                        request,
                        allow_private_network,
                        &fault_tx,
                    ).await;
                    let _ = reply.send(outcome);
                }
                Event::Release { connection, reply } => {
                    let result = match conn_session.get(&connection).and_then(|sid| sessions.get_mut(sid)) {
                        Some(runtime) => runtime.session.release_for_rebind(connection),
                        None => Ok(()),
                    };
                    let _ = reply.send(result);
                }
                Event::Request { connection, request, reply } => {
                    let id = request.id;
                    let remote_principal = conn_principal.get(&connection).cloned();
                    if let Some(principal) = &remote_principal {
                        if let Some(response) = remote_request_gate(id, &request, principal, authorization_registry.as_deref()) {
                            let _ = reply.send(response);
                            continue;
                        }
                    }
                    let request_target = if remote_principal.is_some() {
                        conn_session.get(&connection).cloned().or_else(|| conn_route.get(&connection).map(|route| route.profile_id.clone()))
                    } else {
                        conn_session.get(&connection).cloned().or_else(|| Some(presented.clone()))
                    };
                    if let Some(target) = &request_target {
                        let cleanup_inflight = pending.as_ref()
                            .is_some_and(|pending| pending.cleanup.is_some() && pending.session_id == *target)
                            || sessions.get(target).is_some_and(|runtime| runtime.session.terminal_cleanup.is_some());
                        if cleanup_inflight {
                            deferred.push((target.clone(), connection, request, reply));
                            continue;
                        }
                    }
                    match &request.command {
                        Command::ProfileCatalog {} => {
                            let catalog_value = match &remote_principal {
                                Some(principal) => catalog.remote_catalog(&principal.device_id),
                                None => catalog.catalog(),
                            };
                            let _ = reply.send(Response::Result { id, value: ResultValue::ProfileCatalog { catalog: catalog_value } });
                            continue;
                        }
                        Command::FlushProfiles {} => {
                            if maintenance.is_some() || pending.is_some() {
                                let _ = reply.send(error(id, "MAINTENANCE_BUSY", "Wait for the accepted operation and any drain to finish"));
                                continue;
                            }
                            let mut remaining: Vec<String> = sessions.keys().cloned().collect();
                            remaining.sort();
                            maintenance = Some(Maintenance { id, reply, remaining, flushed: 0 });
                            continue;
                        }
                        Command::ProfileSelect { profile_id } => {
                            let profile_id = profile_id.clone();
                            let Some(source) = conn_session.get(&connection).cloned() else {
                                let _ = reply.send(error(id, "ATTACH_REQUIRED", "Attach before selecting a profile"));
                                continue;
                            };
                            let session_id = sessions.get(&source).map(|runtime| runtime.session.id.clone()).unwrap_or_default();
                            let valid_operation = request.operation.as_ref().is_some_and(|operation| operation.session_id == session_id && operation.attachment_id == connection);
                            if !valid_operation {
                                let _ = reply.send(error(id, "OPERATION_REQUIRED", "Profile selection requires an operation identity for the bound session"));
                                continue;
                            }
                            let Some(target_entry) = catalog.record(&profile_id).cloned() else {
                                let _ = reply.send(error(id, "PROFILE_NOT_FOUND", "Unknown profile id"));
                                continue;
                            };
                            if let Some(principal) = &remote_principal {
                                match &target_entry.access {
                                    Some(access) if access.grants(&principal.device_id) => {}
                                    Some(_) => {
                                        let _ = reply.send(error(id, "REMOTE_INITIAL_PROFILE_ACCESS_DENIED", "Profile does not grant this remote device"));
                                        continue;
                                    }
                                    None => {
                                        let _ = reply.send(error(id, "PROFILE_ACCESS_MIGRATION_REQUIRED", "Profile has no Host-side access record"));
                                        continue;
                                    }
                                }
                            }
                            let source_busy = pending.as_ref().is_some_and(|pending| pending.session_id == source && pending.operation.as_ref().is_some_and(|operation| operation.connection == connection))
                                || sessions.get(&source).is_some_and(|runtime| runtime.session.input_lease.as_ref().is_some_and(|lease| lease.attachment_id == connection));
                            if source_busy {
                                let _ = reply.send(error(id, "REBIND_BUSY", "Finish the current operation or composition before selecting a profile"));
                                continue;
                            }
                            if profile_id == source {
                                let runtime = sessions.get_mut(&source).expect("source session exists");
                                let status = match runtime.session.status(connection, false) { ResultValue::Status(status) => status, _ => unreachable!() };
                                let session_id = runtime.session.id.clone();
                                let _ = reply.send(Response::Result { id, value: ResultValue::ProfileSelected { binding: ProfileBinding { profile_id, session_id }, status } });
                                continue;
                            }
                            if !sessions.contains_key(&profile_id) {
                                match spawn_session(profile_id.clone(), catalog.profile_dir(&profile_id), allow_private_network, fault_tx.clone(), &socket_dir).await {
                                    Ok(runtime) => { sessions.insert(profile_id.clone(), runtime); }
                                    Err(cause) => { let _ = reply.send(error(id, "SESSION_UNAVAILABLE", cause.to_string())); continue; }
                                }
                            }
                            let released = sessions.get_mut(&source).expect("source session exists").session.release_for_rebind(connection);
                            if let Err(code) = released {
                                let _ = reply.send(error(id, code, "Finish the current operation before selecting a profile"));
                                continue;
                            }
                            let attach = Request { id, command: Command::Attach { mode: Mode::Observe, viewport: None }, operation: None };
                            let runtime = sessions.get_mut(&profile_id).expect("target session exists");
                            let response = match runtime.session.request(connection, attach, pending.is_some()) {
                                Admission::Replied(response) => response,
                                Admission::Run(_) | Admission::HostMutation(_) => error(id, "REBIND_FAILED", "Observer rebind was unexpectedly deferred"),
                            };
                            let status = match response {
                                Response::Result { value: ResultValue::Status(status), .. } => status,
                                other => { let _ = reply.send(other); continue; }
                            };
                            conn_session.insert(connection, profile_id.clone());
                            let session_id = runtime.session.id.clone();
                            if remote_principal.is_some() {
                                conn_route.insert(connection, Route { profile_id: profile_id.clone(), session_id: session_id.clone(), reservation_id: String::new() });
                            } else {
                                presented = profile_id.clone();
                            }
                            let _ = reply.send(Response::Result { id, value: ResultValue::ProfileSelected { binding: ProfileBinding { profile_id, session_id }, status } });
                            continue;
                        }
                        _ => {}
                    }
                    let Some(target) = request_target.clone() else {
                        let _ = reply.send(error(id, "SESSION_UNAVAILABLE", "No authorized session route for this connection"));
                        continue;
                    };
                    if let Some(principal) = &remote_principal {
                        match catalog.record(&target).and_then(|entry| entry.access.as_ref()) {
                            Some(access) if access.grants(&principal.device_id) => {}
                            Some(_) => {
                                let _ = reply.send(error(id, "REMOTE_INITIAL_PROFILE_ACCESS_DENIED", "Profile does not grant this remote device"));
                                continue;
                            }
                            None if catalog.record(&target).is_some() => {
                                let _ = reply.send(error(id, "PROFILE_ACCESS_MIGRATION_REQUIRED", "Profile has no Host-side access record"));
                                continue;
                            }
                            None => {
                                let _ = reply.send(error(id, "SESSION_UNAVAILABLE", "Authorized profile is unavailable"));
                                continue;
                            }
                        }
                    }
                    let Some(runtime) = sessions.get_mut(&target) else {
                        let _ = reply.send(error(id, "SESSION_UNAVAILABLE", "Session is unavailable"));
                        continue;
                    };
                    let work = runtime.work.clone();
                    let busy = pending.is_some();
                    let is_attach = matches!(request.command, Command::Attach { .. });
                    let is_detach = matches!(request.command, Command::Detach {});
                    let session = &mut runtime.session;
                    match session.request(connection, request, busy) {
                        Admission::Replied(response) => {
                            if is_attach && matches!(response, Response::Result { .. }) {
                                conn_session.insert(connection, target.clone());
                                if remote_principal.is_some() {
                                    let session_id = session.id.clone();
                                    conn_route.insert(connection, Route { profile_id: target.clone(), session_id, reservation_id: String::new() });
                                } else {
                                    presented = target.clone();
                                }
                            } else if is_detach { conn_session.remove(&connection); }
                            let _ = reply.send(response);
                        }
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
                                pending = Some(Pending { session_id: target.clone(), operation: Some(PendingOperation {
                                    id,
                                    connection,
                                    operation: record_operation,
                                    fresh: dispatch.fresh,
                                    previous_sequence,
                                    reply,
                                }), cleanup: None, flush: false, result });
                            }
                        }
                        Admission::HostMutation(dispatch) => {
                            let previous_sequence = session.begin_dispatch(&dispatch);
                            let result = match &dispatch.command {
                                Command::ProfileCreate { display_name, make_default } => {
                                    let owner = remote_principal.as_ref()
                                        .map(|principal| principal.device_id.clone())
                                        .unwrap_or_else(|| catalog::LOCAL_DEVICE_ID.to_string());
                                    match catalog.create(display_name, *make_default, &owner) {
                                        Ok(profile) => {
                                            let catalog_value = match &remote_principal {
                                                Some(principal) => catalog.remote_catalog(&principal.device_id),
                                                None => catalog.catalog(),
                                            };
                                            Ok(Some(ResultValue::ProfileCreated { profile, catalog: catalog_value }))
                                        }
                                        Err(cause) => Err((cause.code, cause.message)),
                                    }
                                }
                                _ => Err(("INVALID_OPERATION", "Unsupported Host mutation".to_string())),
                            };
                            let error_code = match &result { Err((code, _)) => Some(*code), Ok(_) => None };
                            let unconsumed = dispatch.fresh && error_code.is_some_and(unconsumed_input_error);
                            let response = match result {
                                Ok(value) => Response::Result { id, value: value.expect("Host mutation returns a receipt") },
                                Err((code, cause)) => error(id, code, cause),
                            };
                            let response = bounded_response(response)?;
                            if unconsumed {
                                if let Some(previous_sequence) = previous_sequence {
                                    session.sequences.insert(dispatch.connection, previous_sequence);
                                } else {
                                    session.sequences.remove(&dispatch.connection);
                                }
                                session.records.remove(&dispatch.connection);
                            } else if let Some(record) = session.records.get_mut(&dispatch.connection) {
                                record.response = Some(response.clone());
                            }
                            let _ = reply.send(response);
                        }
                    }
                }
            },
            completion = async { (&mut pending.as_mut().unwrap().result).await }, if pending.is_some() => {
                let finished = pending.take().unwrap();
                let sid = finished.session_id.clone();
                let runtime = sessions.get_mut(&sid).expect("pending session exists");
                let session = &mut runtime.session;
                if let Some(terminal) = finished.cleanup.as_ref() {
                    // Host-initiated terminal delivery for a disconnected
                    // attachment. There is no reply to send; the only Host
                    // state change is releasing the lease once the Engine has
                    // confirmed the terminal (or exposing the unknown outcome).
                    match completion {
                        Ok(completion) => {
                            session.document_revision = session.document_revision.max(completion.document_revision);
                            session.presentation_revision = session.presentation_revision.max(completion.presentation_revision);
                            session.active_tab_id = completion.active_tab_id.clone();
                            session.tabs = completion.tabs.clone();
                            session.navigation = completion.navigation.clone();
                            session.editor_context = completion.editor_context.clone();
                            if completion.fault.is_some() { session.fault = completion.fault; }
                            if let Err((code, cause)) = &completion.result {
                                session.fault = Some(format!("Input terminal {code}: {cause}"));
                            }
                        }
                        Err(_) => {
                            session.fault = Some("Browser worker stopped; input terminal outcome unknown".into());
                        }
                    }
                    session.records.remove(&terminal.connection);
                    if session.input_lease.as_ref().is_some_and(|lease| lease.operation_id == terminal.operation.operation_id) {
                        session.input_lease = None;
                    }
                    session.finish_handover();
                    continue;
                }
                let viewport_update = session.viewport_update.take();
                let outcome = match completion {
                    Ok(completion) => {
                        // Host-local maintenance flush: no viewport/operation
                        // state changes, only the typed store receipt.
                        if finished.flush {
                            if let Some(drain) = maintenance.as_mut() {
                                match completion.result {
                                    Ok(Some(ResultValue::ProfileFlush { flushed, .. })) if flushed => {
                                        drain.flushed = drain.flushed.saturating_add(1);
                                        continue;
                                    }
                                    Err((code, cause)) => {
                                        let maintenance = maintenance.take().expect("maintenance owner");
                                        let _ = maintenance.reply.send(error(maintenance.id, code, cause));
                                        continue;
                                    }
                                    _ => {
                                        let maintenance = maintenance.take().expect("maintenance owner");
                                        let _ = maintenance.reply.send(error(maintenance.id, "PROFILE_STORAGE_FAILED", "maintenance flush did not report a saved store"));
                                        continue;
                                    }
                                }
                            }
                            continue;
                        }
                        // A failed resize may have changed Page partially. Keep the
                        // last committed geometry/revision and fence the faulted Page.
                        if viewport_update.is_none() { session.viewport = completion.viewport; }
                        session.document_revision = session.document_revision.max(completion.document_revision);
                        session.presentation_revision = session.presentation_revision.max(completion.presentation_revision);
                        session.active_tab_id = completion.active_tab_id.clone();
                        session.tabs = completion.tabs.clone();
                        session.navigation = completion.navigation.clone();
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
            update = fault_rx.recv() => {
                match update {
                    Some((sid, update)) => {
                        if let Some(runtime) = sessions.get_mut(&sid) {
                            if runtime.session.fault.is_none() {
                                runtime.session.document_revision = runtime.session.document_revision.max(update.document_revision);
                                runtime.session.fault = update.fault;
                            }
                        }
                    }
                    None => {},
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
    // Drop work senders so workers finish accepted work and exit, then join the
    // worker threads without moving V8 onto this thread.
    let mut joins = Vec::new();
    for (_, runtime) in sessions.drain() { joins.push(runtime.join); }
    drop(fault_tx);
    drop(incoming);
    for join in joins {
        tokio::task::spawn_blocking(move || join.join().map_err(|_| anyhow::anyhow!("browser worker panicked"))?).await??;
    }
    drop(listener); drop(media_listener); drop(directory);
    Ok(())
}
