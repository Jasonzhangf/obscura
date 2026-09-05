use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use anyhow::{Context, Result};
use obscura_host_protocol::{Command, Control, ControlPhase, FramePacket, Mode, Operation, Request, Response, ResultValue, SessionStatus};
use crate::media;
use crate::worker::{self, Completion, Work};
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
    write(&mut out, Response::Ready { version: 3, session_id: session }).await?;
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
    attachments: HashMap<u64, Mode>,
    agent: Option<u64>,
    viewport_revision: u64,
    document_revision: u64,
    fault: Option<String>,
    control: Control,
    records: HashMap<u64, Record>,
    retired: HashSet<u64>,
}
struct Record { operation: Operation, command: Command, response: Option<Response> }
impl Session {
    fn detach(&mut self, connection: u64) {
        self.retired.insert(connection);
        self.attachments.remove(&connection);
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
        if self.fault.is_none() && self.viewport.is_some() {
            if let ControlPhase::Waiting { attachment_id } = self.control.phase {
                self.transition(ControlPhase::Human { attachment_id });
            }
        }
    }
    fn status(&self, connection: u64, busy: bool) -> ResultValue {
        ResultValue::Status(SessionStatus {
            session_id: self.id.clone(), attachments: self.attachments.len(),
            attachment_id: self.attachments.contains_key(&connection).then_some(connection),
            mode: self.attachments.get(&connection).copied(),
            agent_attached: self.agent.is_some(), viewport_revision: self.viewport_revision,
            document_revision: self.document_revision,
            operation_running: busy,
            control: self.control.clone(),
            fault: self.fault.clone(),
            next_sequence: self.records.get(&connection).map_or(1, |r| r.operation.sequence.saturating_add(1)),
            viewport: self.viewport,
        })
    }
    fn request(&mut self, connection: u64, request: Request, busy: bool) -> Result<Response, Command> {
        let id = request.id;
        let input = matches!(request.command, Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. });
        let mutation = input || matches!(request.command, Command::Navigate { .. } | Command::Evaluate { .. } | Command::Resize { .. } | Command::CloseSession {});
        if mutation {
            let Some(operation) = &request.operation else { return Ok(error(id, "OPERATION_REQUIRED", "Browser mutations require an operation identity")); };
            if operation.session_id != self.id || operation.attachment_id != connection {
                return Ok(error(id, "STALE_ATTACHMENT", "Operation identity belongs to another session or connection"));
            }
            if let Some(previous) = self.records.get(&connection) {
                if operation.sequence == previous.operation.sequence {
                    if *operation != previous.operation || request.command != previous.command {
                        return Ok(error(id, "OPERATION_CONFLICT", "Sequence already belongs to different operation content"));
                    }
                    return Ok(match &previous.response {
                        Some(Response::Result { value, .. }) => Response::Result { id, value: value.clone() },
                        Some(Response::Error { code, message, .. }) => error(id, code, message.clone()),
                        None => error(id, "OPERATION_RUNNING", "Operation already accepted; do not execute again"),
                        _ => unreachable!(),
                    });
                }
                if operation.sequence <= previous.operation.sequence {
                    return Ok(error(id, "OPERATION_EXPIRED", "Receipt expired; operation must not be replayed"));
                }
            }
            let expected = self.records.get(&connection).map_or(Some(1), |r| r.operation.sequence.checked_add(1));
            if Some(operation.sequence) != expected { return Ok(error(id, "INVALID_SEQUENCE", "Use the next sequence from Host status")); }
            if operation.control_epoch != self.control.epoch { return Ok(error(id, "STALE_CONTROL", "Control epoch changed")); }
            if operation.viewport_revision != self.viewport_revision { return Ok(error(id, "STALE_VIEWPORT", "Viewport revision changed")); }
            if operation.document_revision != self.document_revision { return Ok(error(id, "STALE_DOCUMENT", "Document revision changed")); }
        } else if request.operation.is_some() {
            return Ok(error(id, "INVALID_OPERATION", "Control requests cannot carry browser operation identity"));
        }
        if self.viewport.is_none() { return Ok(error(id, "SESSION_CLOSED", "Session was explicitly closed")); }
        if self.attachments.contains_key(&connection) && matches!(request.command, Command::Status {} | Command::Detach {}) {
            if matches!(request.command, Command::Detach {}) { self.detach(connection); }
            return Ok(Response::Result { id, value: self.status(connection, busy) });
        }
        if matches!(&request.command, Command::CloseSession {}) && self.agent == Some(connection) {
            if self.fault.is_none() && !matches!(self.control.phase, ControlPhase::Agent) { return Ok(error(id, "CONTROL_REQUIRED", "Agent control is suspended")); }
            return if busy { Ok(error(id, "OPERATION_BUSY", "Wait for the accepted operation to finish")) } else { Err(request.command) };
        }
        if let Some(fault) = &self.fault { return Ok(error(id, "SESSION_FAULTED", fault.clone())); }
        match request.command {
            Command::Attach { mode } => {
                if self.retired.contains(&connection) { return Ok(error(id, "RECONNECT_REQUIRED", "Detached identities cannot be reused; open a new connection")); }
                if self.attachments.contains_key(&connection) { return Ok(error(id, "ALREADY_ATTACHED", "Use a new connection to change attachment mode")); }
                if mode == Mode::Agent && self.agent.is_some() { return Ok(error(id, "AGENT_BUSY", "Agent attachment already exists")); }
                self.attachments.insert(connection, mode);
                if mode == Mode::Agent { self.agent = Some(connection); }
                Ok(Response::Result { id, value: self.status(connection, busy) })
            }
            command => {
                if !self.attachments.contains_key(&connection) { return Ok(error(id, "ATTACH_REQUIRED", "Attach first")); }
                match command {
                    Command::RequestTakeover { epoch } | Command::ReleaseControl { epoch } | Command::ResumeAgent { epoch } => {
                        if epoch != self.control.epoch { return Ok(error(id, "STALE_CONTROL", "Control epoch changed")); }
                        match command {
                            Command::RequestTakeover { .. } => {
                                if self.agent == Some(connection) { return Ok(error(id, "OBSERVER_REQUIRED", "Use an observer attachment for human takeover")); }
                                match self.control.phase {
                                    ControlPhase::Agent | ControlPhase::Paused => {
                                        // Acceptance fences Agent immediately, before the worker finishes.
                                        self.transition(ControlPhase::Waiting { attachment_id: connection });
                                        if !busy { self.finish_handover(); }
                                    }
                                    ControlPhase::Waiting { attachment_id } | ControlPhase::Human { attachment_id } if attachment_id == connection => {},
                                    _ => return Ok(error(id, "CONTROL_BUSY", "Another attachment requested human control")),
                                }
                            }
                            Command::ReleaseControl { .. } => {
                                if !matches!(self.control.phase, ControlPhase::Human { attachment_id } if attachment_id == connection) { return Ok(error(id, "CONTROL_REQUIRED", "Only the human holder may release control")); }
                                if busy { return Ok(error(id, "OPERATION_BUSY", "Finish the current operation before release")); }
                                self.transition(ControlPhase::Agent);
                            }
                            Command::ResumeAgent { .. } => {
                                if self.agent != Some(connection) || !matches!(self.control.phase, ControlPhase::Paused) { return Ok(error(id, "CONTROL_REQUIRED", "Explicit Agent resume requires paused control")); }
                                if busy { return Ok(error(id, "OPERATION_BUSY", "Finish the accepted operation before resume")); }
                                self.transition(ControlPhase::Agent);
                            }
                            _ => unreachable!(),
                        }
                        return Ok(Response::Result { id, value: self.status(connection, busy) });
                    }
                    _ => {},
                }
                if input {
                    let allowed = matches!(self.control.phase, ControlPhase::Agent) && self.agent == Some(connection)
                        || matches!(self.control.phase, ControlPhase::Human { attachment_id } if attachment_id == connection);
                    if !allowed { return Ok(error(id, "CONTROL_REQUIRED", "Input requires current control ownership")); }
                } else {
                    if self.agent != Some(connection) { return Ok(error(id, "AGENT_REQUIRED", "Observer cannot mutate or evaluate JavaScript")); }
                    if !matches!(self.control.phase, ControlPhase::Agent) { return Ok(error(id, "CONTROL_REQUIRED", "Agent control is suspended")); }
                }
                if busy { return Ok(error(id, "OPERATION_BUSY", "Wait for the accepted operation to finish")); }
                match &command {
                    Command::Resize { width, height } => {
                        if *width == 0 || *height == 0 || *width > 4096 || *height > 4096 { return Ok(error(id, "INVALID_VIEWPORT", "Dimensions must be 1..4096")); }
                    }
                    Command::Navigate { url } => {
                        // This local bootstrap surface never enables file navigation.
                        if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("data:text/html,") || url == "about:blank") {
                            return Ok(error(id, "INVALID_URL", "Unsupported navigation scheme"));
                        }
                    }
                    Command::Evaluate { .. } => {},
                    Command::Click { x, y } | Command::Scroll { x, y, .. } => {
                        if !cfg!(feature = "render") { return Ok(error(id, "INPUT_UNAVAILABLE", "Coordinate input requires a render-enabled Host")); }
                        let (width, height) = self.viewport.unwrap();
                        if !x.is_finite() || !y.is_finite() || *x < 0.0 || *y < 0.0 || *x >= width as f64 || *y >= height as f64 {
                            return Ok(error(id, "INVALID_POINT", "Coordinates must be inside the current CSS viewport"));
                        }
                        if let Command::Scroll { delta_x, delta_y, .. } = &command {
                            if !delta_x.is_finite() || !delta_y.is_finite() || delta_x.abs() > 4096.0 || delta_y.abs() > 4096.0 {
                                return Ok(error(id, "INVALID_SCROLL", "Scroll deltas must be finite and within 4096 CSS pixels"));
                            }
                        }
                    }
                    Command::InputText { text } => {
                        if text.chars().count() > 4096 { return Ok(error(id, "TEXT_TOO_LARGE", "At most 4096 characters per text operation")); }
                    }
                    _ => unreachable!("control commands handled above"),
                }
                Err(command)
            }
        }
    }
}

struct Pending {
    id: u64,
    connection: u64,
    resize: bool,
    reply: oneshot::Sender<Response>,
    result: oneshot::Receiver<Completion>,
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
    let mut session = Session { id: id.clone(), viewport: Some(viewport), attachments: HashMap::new(), agent: None, viewport_revision: 0, document_revision: 0, fault: None, control: Control { epoch: 1, phase: ControlPhase::Agent }, records: HashMap::new(), retired: HashSet::new() };
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
                    session.detach(connection);
                    session.retired.remove(&connection);
                }
                Event::Request { connection, request, reply } => {
                    let id = request.id;
                    let operation = request.operation.clone();
                    match session.request(connection, request, pending.is_some()) {
                        Ok(response) => { let _ = reply.send(response); }
                        Err(command) => {
                            let resize = matches!(command, Command::Resize { .. });
                            let record = Record { operation: operation.expect("mutation admission checked identity"), command: command.clone(), response: None };
                            let (finished, result) = oneshot::channel();
                            if work.try_send(Work { command, document_revision: record.operation.document_revision, reply: finished }).is_err() {
                                session.fault = Some("Browser worker unavailable; recreate session explicitly".into());
                                let _ = reply.send(error(id, "SESSION_FAULTED", session.fault.clone().unwrap()));
                            } else {
                                session.records.insert(connection, record);
                                pending = Some(Pending { id, connection, resize, reply, result });
                            }
                        }
                    }
                }
            },
            completion = async { (&mut pending.as_mut().unwrap().result).await }, if pending.is_some() => {
                let operation = pending.take().unwrap();
                let response = match completion {
                    Ok(completion) => {
                        session.viewport = completion.viewport;
                        session.document_revision = session.document_revision.max(completion.document_revision);
                        if completion.fault.is_some() { session.fault = completion.fault; }
                        session.finish_handover();
                        match completion.result {
                            Ok(value) => {
                                if operation.resize { session.viewport_revision += 1; }
                                Response::Result { id: operation.id, value: value.unwrap_or_else(|| session.status(operation.connection, false)) }
                            }
                            Err((code, cause)) => error(operation.id, code, cause),
                        }
                    }
                    Err(_) => {
                        session.fault = Some("Browser worker stopped; operation outcome unknown".into());
                        error(operation.id, "OUTCOME_UNKNOWN", session.fault.clone().unwrap())
                    }
                };
                // Cache only the latest bounded receipt per live attachment. Earlier
                // sequences are rejected, never silently evicted into re-execution.
                let response = bounded_response(response)?;
                if let Some(record) = session.records.get_mut(&operation.connection) { record.response = Some(response.clone()); }
                let _ = operation.reply.send(response);
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
