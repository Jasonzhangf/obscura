use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use anyhow::{bail, ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, Request, Response, ResultValue, VideoPacket, WebRtcSignal, HOST_PROTOCOL_VERSION};
use obscura_media::EncodedFrame;
use obscura_media::webrtc_endpoint::{BrowserResponseDispatch, WebRtcBrowserResponseCompletion, SETTLEMENT_DEADLINE};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::UnixStream, sync::{mpsc, watch}};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::remote_admission::{self, InitialProfileGrant, RemoteAdmissionErrorCode, RemoteAdmissionRequest, RemoteAdmissionResult, RemotePrincipalContext};
use super::server::{SessionDurable, SessionGrant};

pub type Socket = WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

/// Exact admitted media subscription state for one mTLS control attachment.
/// Authority is only set after the Host confirms the final `Attach`/rebind; a
/// reservation or endpoint handshake alone never grants media rights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaState {
    pub attached: bool,
    pub session_id: Option<String>,
    pub generation: u64,
}

impl Default for MediaState {
    fn default() -> Self { Self { attached: false, session_id: None, generation: 0 } }
}

async fn reply(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Response> {
    let mut bytes = Vec::new();
    (&mut *reader).take(1024 * 1024 + 1).read_until(b'\n', &mut bytes).await?;
    ensure!(bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'), "Invalid bounded Host response");
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn control(
    mut remote: Socket,
    host_dir: &Path,
    active: watch::Sender<MediaState>,
    grant: SessionGrant,
    principal: RemotePrincipalContext,
    initial: InitialProfileGrant,
    enable_webrtc: bool,
    webrtc_bind: Option<SocketAddr>,
) -> Result<()> {
    // Same-stream pre-Ready admission: one split, one retained reader that
    // carries admission -> result -> Ready -> every browser response.
    let admission_path = host_dir.join("remote-admission.sock");
    let local = UnixStream::connect(&admission_path).await.context("Connect Host remote admission")?;
    let (read, mut write) = local.into_split(); let mut reader = BufReader::new(read);
    let request_id = 1u64;
    let request = RemoteAdmissionRequest { principal, initial, request_id };
    remote_admission::write_remote_admission_request(&mut write, &request).await?;
    let Some(result) = remote_admission::read_remote_admission_result(&mut reader).await? else {
        bail!("Host closed the remote admission stream before a result");
    };
    match &result {
        RemoteAdmissionResult::Ok { request_id: id, session_id, .. } => {
            ensure!(*id == request_id, "Remote admission result correlation mismatch");
            let ready = reply(&mut reader).await?;
            match &ready {
                Response::Ready { version, session_id: ready_session } => {
                    ensure!(*version == HOST_PROTOCOL_VERSION, "Host Ready protocol version mismatch");
                    ensure!(ready_session == session_id, "Host Ready session does not match the admission reservation");
                }
                _ => bail!("Host did not publish Ready after remote admission"),
            }
            send(&mut remote, Message::text(serde_json::to_string(&ready)?)).await?;
        }
        RemoteAdmissionResult::Err { request_id: id, code, message } => {
            let acceptable = match id {
                Some(id) => *id == request_id,
                None => *code == RemoteAdmissionErrorCode::RemoteAdmissionMalformed,
            };
            ensure!(acceptable, "Illegal remote admission result: {result:?}");
            let response = Response::Error { id: request_id, code: code.as_str().to_string(), message: message.clone() };
            send(&mut remote, Message::text(serde_json::to_string(&response)?)).await?;
            return Ok(());
        }
    }
    let mut webrtc_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut webrtc_filter: Option<tokio::task::JoinHandle<()>> = None;
    let mut browser_requests: Option<mpsc::Receiver<Request>> = None;
    let mut browser_responses: Option<mpsc::Sender<BrowserResponseDispatch>> = None;
    let mut terminal_pending: Option<watch::Sender<bool>> = None;
    // The exact-session encoded stream for the current binding. It is acquired
    // on a new binding and released on the previous one.
    let mut binding: Option<(String, Arc<SessionDurable>, watch::Receiver<Option<Arc<EncodedFrame>>>)> = None;
    let mut internal_id = u64::MAX;
    loop {
        tokio::select! {
            message = remote.next() => {
                let Some(message) = message else { break; };
                let text = match message? {
                    Message::Text(text) => text,
                    Message::Ping(bytes) => { send(&mut remote, Message::Pong(bytes)).await?; continue; }
                    Message::Close(_) => break,
                    _ => anyhow::bail!("Control accepts JSON text only"),
                };
                if let Ok(signal) = serde_json::from_str::<WebRtcSignal>(&text) {
                    match signal {
                        WebRtcSignal::Offer { id, capability, sdp } => {
                            let result = if !enable_webrtc {
                                Err(anyhow::anyhow!("WEBRTC_DISABLED: WebRTC requires explicit endpoint enablement"))
                            } else if !active.borrow().attached {
                                Err(anyhow::anyhow!("ATTACH_REQUIRED: Observe attachment must be active before WebRTC signaling"))
                            } else if webrtc_task.is_some() {
                                Err(anyhow::anyhow!("WEBRTC_CONNECTED: One WebRTC media connection per mTLS control attachment"))
                            } else {
                                let bind = webrtc_bind.context("WebRTC enablement requires an explicit bind address")?;
                                let authorization = host_command(&mut write, &mut reader, &mut internal_id,
                                    Command::AuthorizeWebRtc { capability: capability.clone() }).await?;
                                let (webrtc_binding, authorized_capability) = match authorization {
                                    Response::Result { value: ResultValue::WebRtcAuthorization { binding: webrtc_binding, capability }, .. } => (webrtc_binding, capability),
                                    other => bail!("Host rejected WebRTC authorization: {other:?}"),
                                };
                                ensure!(authorized_capability == capability, "Host returned a different WebRTC capability");
                                let (browser_request_tx, browser_request_rx) = mpsc::channel(16);
                                let (browser_response_tx, browser_response_rx) = mpsc::channel(16);
                                let (terminal_tx, terminal_rx) = watch::channel(false);
                                let (filtered_latest, webrtc_latest) = watch::channel(None);
                                let (answer_sdp, endpoint) = obscura_media::webrtc_endpoint::accept_offer(
                                    sdp, capability.clone(), webrtc_binding.clone(), webrtc_latest,
                                    browser_request_tx, browser_response_rx, terminal_rx, bind).await?;
                                let consumed = host_command(&mut write, &mut reader, &mut internal_id,
                                    Command::ConsumeWebRtc { binding: webrtc_binding.clone(), capability: capability.clone() }).await?;
                                ensure!(matches!(consumed, Response::Result { value: ResultValue::WebRtcConsumed { .. }, .. }),
                                    "Host did not consume WebRTC authorization");
                                let admitted = active.borrow().clone();
                                ensure!(admitted.attached && admitted.session_id.as_deref() == Some(webrtc_binding.session_id.as_str()),
                                    "WebRTC admitted session changed before binding commit");
                                let encoded = binding.as_ref()
                                    .filter(|(session_id, _, _)| session_id == &webrtc_binding.session_id)
                                    .map(|(_, _, encoded)| encoded.clone())
                                    .context("WebRTC offer arrived before the exact media binding was acquired")?;
                                let filter = exact_session_stream(filtered_latest, encoded, active.subscribe(), admitted);
                                let answer = WebRtcSignal::Answer { id, capability, binding: webrtc_binding, sdp: answer_sdp };
                                send(&mut remote, Message::text(serde_json::to_string(&answer)?)).await?;
                                browser_requests = Some(browser_request_rx);
                                browser_responses = Some(browser_response_tx);
                                terminal_pending = Some(terminal_tx);
                                webrtc_filter = Some(filter);
                                webrtc_task = Some(tokio::spawn(async move {
                                    let result = endpoint.run().await;
                                    if let Err(error) = &result { eprintln!("WebRTC endpoint task ended: {error}"); }
                                    result
                                }));
                                Ok(())
                            };
                            if let Err(error) = result {
                                let (code, message) = split_error(error);
                                let response = WebRtcSignal::Error { id, code, message };
                                send(&mut remote, Message::text(serde_json::to_string(&response)?)).await?;
                            }
                        }
                        WebRtcSignal::Answer { id, .. } | WebRtcSignal::Error { id, .. } => {
                            let response = WebRtcSignal::Error { id, code: "INVALID_SIGNALING_STATE".into(), message: "Endpoint accepts only a WebRTC offer on the mTLS control bootstrap".into() };
                            send(&mut remote, Message::text(serde_json::to_string(&response)?)).await?;
                        }
                    }
                    continue;
                }
                let request: Request = match serde_json::from_str(&text) {
                    Ok(request) => request,
                    Err(_) => {
                        let error = Response::Error { id: 0, code: "INVALID_REQUEST".into(), message: "Invalid browser protocol request".into() };
                        send(&mut remote, Message::text(serde_json::to_string(&error)?)).await?; continue;
                    }
                };
                let request_id = request.id;
                let result = forward_remote_request(&mut write, &mut reader, request).await?;
                let terminal_denial = is_terminal_authorization_denial(&result, request_id);
                let detached = response_ends_media(&active.borrow(), &result);
                // A correlated authorization denial is terminal for this
                // attachment even though it is not a Status/ProfileSelected
                // result. Commit the attachment-local media fence before any
                // cleanup await so the old /media grant cannot keep streaming
                // while release_binding waits on the session supervisor.
                if terminal_denial {
                    fence_media_state(&active);
                }
                // Publish the new exact media binding and forward the response
                // only after it. The media admission path never blocks on the
                // adapter connection or a first frame, so the consumer cannot
                // observe a binding that the endpoint has not yet acquired.
                update_media_state(&active, &result);
                reconcile_binding(&grant, &active, &mut binding).await;
                send(&mut remote, Message::text(serde_json::to_string(&result)?)).await?;
                if terminal_denial {
                    release_binding(&grant, &mut binding).await;
                    fence_without_terminal(&mut webrtc_task, &mut webrtc_filter, &mut browser_requests,
                        &mut browser_responses, &mut terminal_pending);
                    break;
                }
                if detached {
                    fence_without_terminal(&mut webrtc_task, &mut webrtc_filter, &mut browser_requests,
                        &mut browser_responses, &mut terminal_pending);
                }
            }
            request = async {
                match browser_requests.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending::<Option<Request>>().await,
                }
            } => {
                let Some(request) = request else {
                    browser_requests = None;
                    browser_responses = None;
                    webrtc_task = None;
                    continue;
                };
                let request_id = request.id;
                let result = forward_remote_request(&mut write, &mut reader, request).await?;
                let terminal_denial = is_terminal_authorization_denial(&result, request_id);
                let admitted = active.borrow().clone();
                let terminal = terminal_response(&admitted, &result, request_id);
                // Commit the terminal disposition and the reserved terminal
                // dispatch BEFORE the media fence, so closing the filtered frame
                // watch cannot end `run_inner` before the correlated response is
                // drained. `run_inner` keys its frame-watch-close handling off
                // this disposition.
                if terminal {
                    let _ = terminal_pending.as_ref().context("WebRTC terminal-pending sender unavailable")?
                        .send(true);
                }
                if terminal_denial {
                    fence_media_state(&active);
                }
                let response_tx = browser_responses.as_ref().context("WebRTC browser response pump unavailable")?;
                if terminal {
                    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
                    response_tx.send(BrowserResponseDispatch::Terminal {
                        response: result.clone(),
                        completion: completion_tx,
                    }).await.context("WebRTC terminal browser response receiver ended before terminal dispatch")?;
                    match tokio::time::timeout(SETTLEMENT_DEADLINE, completion_rx).await {
                        Ok(Ok(WebRtcBrowserResponseCompletion::BufferDrained)) => {}
                        Ok(Ok(outcome)) => bail!("WebRTC terminal browser response failed: {outcome:?}"),
                        Ok(Err(_)) => bail!("WebRTC terminal browser response receiver ended before completion"),
                        Err(_) => bail!("WebRTC terminal browser response settlement deadline exceeded"),
                    }
                } else {
                    response_tx.send(BrowserResponseDispatch::Ordinary(result.clone())).await
                        .context("WebRTC browser response receiver ended")?;
                }
                let detached = update_media_state(&active, &result);
                if terminal_denial {
                    release_binding(&grant, &mut binding).await;
                    fence_without_terminal(&mut webrtc_task, &mut webrtc_filter, &mut browser_requests,
                        &mut browser_responses, &mut terminal_pending);
                    continue;
                }
                reconcile_binding(&grant, &active, &mut binding).await;
                if terminal || detached {
                    fence_without_terminal(&mut webrtc_task, &mut webrtc_filter, &mut browser_requests,
                        &mut browser_responses, &mut terminal_pending);
                }
            }
        }
    }
    release_binding(&grant, &mut binding).await;
    if let Some(task) = webrtc_task { task.abort(); }
    if let Some(task) = webrtc_filter { task.abort(); }
    Ok(())
}

/// Typed identity used for publish-once filtering. It intentionally contains
/// no encoded bytes, Arc identity, or raw frame payload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PacketIdentity {
    Waiting { session_id: String },
    AccessUnit {
        session_id: String,
        tab_id: String,
        sequence: u64,
        presentation_revision: u64,
        document_revision: u64,
        viewport_revision: u64,
        width: u32,
        height: u32,
        stride: u32,
        byte_length: u64,
        pixel_format_premultiplied_rgba8: bool,
        encoder_id: String,
        pts_us: u64,
        coded_width: u32,
        coded_height: u32,
        codec_h264_annex_b: bool,
        keyframe: bool,
        descriptor_byte_length: u64,
    },
    Unavailable { session_id: String, message: String },
    EncoderUnavailable { session_id: String, message: String },
    Closed { session_id: String },
}

fn packet_identity(packet: &VideoPacket) -> PacketIdentity {
    match packet {
        VideoPacket::Waiting { session_id } => PacketIdentity::Waiting { session_id: session_id.clone() },
        VideoPacket::AccessUnit { source, encoder_id, pts_us, coded_width, coded_height, codec, keyframe, byte_length } => {
            PacketIdentity::AccessUnit {
                session_id: source.session_id.clone(),
                tab_id: source.tab_id.clone(),
                sequence: source.sequence,
                presentation_revision: source.presentation_revision,
                document_revision: source.document_revision,
                viewport_revision: source.viewport_revision,
                width: source.width,
                height: source.height,
                stride: source.stride,
                byte_length: source.byte_length,
                pixel_format_premultiplied_rgba8: matches!(source.pixel_format, obscura_host_protocol::PixelFormat::PremultipliedRgba8),
                encoder_id: encoder_id.clone(),
                pts_us: *pts_us,
                coded_width: *coded_width,
                coded_height: *coded_height,
                codec_h264_annex_b: matches!(codec, obscura_host_protocol::VideoCodec::H264AnnexB),
                keyframe: *keyframe,
                descriptor_byte_length: *byte_length,
            }
        }
        VideoPacket::Unavailable { session_id, message } => PacketIdentity::Unavailable {
            session_id: session_id.clone(),
            message: message.clone(),
        },
        VideoPacket::EncoderUnavailable { session_id, message } => PacketIdentity::EncoderUnavailable {
            session_id: session_id.clone(),
            message: message.clone(),
        },
        VideoPacket::Closed { session_id } => PacketIdentity::Closed { session_id: session_id.clone() },
    }
}

/// Derive an exact-session view of the endpoint encoded stream for one admitted
/// attachment. It never forwards a frame that does not belong to the
/// connection's current admitted session, so the shared encoded stream cannot
/// leak another profile's media to this peer. The task ends when the
/// attachment detaches or either watch closes.
fn exact_session_stream(
    filtered: watch::Sender<Option<Arc<EncodedFrame>>>,
    mut source: watch::Receiver<Option<Arc<EncodedFrame>>>,
    mut state: watch::Receiver<MediaState>,
    admitted: MediaState,
) -> tokio::task::JoinHandle<()> {
    let task = tokio::spawn(async move {
        let Some(admitted_session) = admitted.session_id.clone().filter(|_| admitted.attached) else {
            filtered.send_replace(None);
            return;
        };
        let mut last_published = None;
        loop {
            let current = state.borrow().clone();
            if current != admitted {
                if !current.attached {
                    filtered.send_replace(None);
                }
                return;
            }
            if let Some(frame) = source.borrow_and_update().clone() {
                if packet_session(&frame.packet).as_deref() == Some(admitted_session.as_str()) {
                    let identity = packet_identity(&frame.packet);
                    if last_published.as_ref() != Some(&identity) {
                        filtered.send_replace(Some(frame));
                        last_published = Some(identity);
                    }
                }
            }
            tokio::select! {
                changed = source.changed() => { if changed.is_err() { return; } }
                changed = state.changed() => { if changed.is_err() { return; } }
            }
        }
    });
    task
}

/// Forward a raw browser request over the already-admitted stream. The endpoint
/// no longer substitutes a bare command allowlist for Host scope/access
/// checks; only the local-only maintenance drain stays endpoint-refused.
async fn forward_remote_request(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    request: Request,
) -> Result<Response> {
    if matches!(request.command, Command::FlushProfiles {}) {
        return Ok(remote_command_error(request.id));
    }
    host_request(write, reader, request).await
}

fn remote_command_error(id: u64) -> Response {
    Response::Error {
        id,
        code: "REMOTE_COMMAND_FORBIDDEN".into(),
        message: "FlushProfiles is a Host-local maintenance command".into(),
    }
}

/// Acquire the exact-session media reference for a new binding and release the
/// previous one. The control attach path never blocks on an adapter connection
/// or a first frame; `acquire` only binds resources and returns the encoded
/// receiver.
async fn reconcile_binding(
    grant: &SessionGrant,
    active: &watch::Sender<MediaState>,
    binding: &mut Option<(String, Arc<SessionDurable>, watch::Receiver<Option<Arc<EncodedFrame>>>)>,
) {
    let current = active.borrow().clone();
    let wanted = if current.attached { current.session_id.clone() } else { None };
    let unchanged = binding.as_ref().is_some_and(|(session_id, _, _)| Some(session_id) == wanted.as_ref());
    if unchanged { return; }
    if let Some((_, durable, _)) = binding.take() {
        // A failed finalizer is retained and reported by the durable owner; it
        // must not abort the surviving control attachment or another session's
        // media.
        if let Err(error) = grant.release(&durable).await {
            eprintln!("media binding release failed: {error}");
        }
    }
    if let Some(session_id) = wanted {
        match grant.acquire(&session_id).await {
            Ok((encoded, durable)) => { *binding = Some((session_id, durable, encoded)); }
            Err(error) => eprintln!("media binding acquire failed for {session_id}: {error:?}"),
        }
    }
}

async fn release_binding(
    grant: &SessionGrant,
    binding: &mut Option<(String, Arc<SessionDurable>, watch::Receiver<Option<Arc<EncodedFrame>>>)>,
) {
    if let Some((_, durable, _)) = binding.take() {
        if let Err(error) = grant.release(&durable).await {
            eprintln!("media binding release failed: {error}");
        }
    }
}

/// Drop the endpoint media handles without aborting the WebRTC task, so the
/// peer closes with an observable terminal while the fence keeps the current
/// control attachment from reusing the old binding.
fn fence_without_terminal(
    webrtc_task: &mut Option<tokio::task::JoinHandle<Result<()>>>,
    webrtc_filter: &mut Option<tokio::task::JoinHandle<()>>,
    browser_requests: &mut Option<mpsc::Receiver<Request>>,
    browser_responses: &mut Option<mpsc::Sender<BrowserResponseDispatch>>,
    terminal_pending: &mut Option<watch::Sender<bool>>,
) {
    if let Some(task) = webrtc_filter.take() { task.abort(); }
    *webrtc_task = None;
    *browser_requests = None;
    *browser_responses = None;
    *terminal_pending = None;
}

/// True only for the frozen typed resident authorization denials. Other
/// errors, navigation results and log text never trigger the transition.
fn is_terminal_authorization_denial(response: &Response, request_id: u64) -> bool {
    matches!(
        response,
        Response::Error { id, code, .. }
            if *id == request_id
                && (code == RemoteAdmissionErrorCode::RemoteAuthRequired.as_str()
                    || code == RemoteAdmissionErrorCode::RemotePrincipalRevoked.as_str())
    )
}

fn update_media_state(
    active: &watch::Sender<MediaState>,
    result: &Response,
) -> bool {
    match result {
        Response::Result { value: ResultValue::Status(status), .. } => {
            let attached = status.attachment_id.is_some();
            let session_id = attached.then(|| status.session_id.clone());
            active.send_modify(|state| {
                if state.session_id != session_id { state.generation = state.generation.wrapping_add(1); }
                state.attached = attached;
                state.session_id = session_id;
            });
            !attached
        }
        Response::Result { value: ResultValue::ProfileSelected { binding, status }, .. } => {
            let attached = status.attachment_id.is_some();
            let session_id = attached.then(|| binding.session_id.clone());
            active.send_modify(|state| {
                if state.session_id != session_id { state.generation = state.generation.wrapping_add(1); }
                state.attached = attached;
                state.session_id = session_id;
            });
            !attached
        }
        _ => false,
    }
}

/// Clear the attachment-local media admission without touching any other
/// attachment's binding. The generation bump wakes every existing `/media`
/// stream and makes new handshakes fail the `attached` check.
fn fence_media_state(active: &watch::Sender<MediaState>) {
    active.send_modify(|state| {
        state.generation = state.generation.wrapping_add(1);
        state.attached = false;
        state.session_id = None;
    });
}

/// A response ends media for the current attachment when the Host reports no
/// active attachment, or when it moved the attachment to a different session.
/// A detach clears the attachment id; a rebind keeps an attachment id but
/// changes the session. Either case fences the admitted stream. The terminal
/// dispatch is committed before `update_media_state` publishes the fence, so
/// the correlated response remains reachable after the filter closes.
fn response_ends_media(admitted: &MediaState, result: &Response) -> bool {
    let (attachment_id, session_id) = match result {
        Response::Result { value: ResultValue::Status(status), .. } =>
            (status.attachment_id, Some(status.session_id.as_str())),
        Response::Result { value: ResultValue::ProfileSelected { status, .. }, .. } =>
            (status.attachment_id, Some(status.session_id.as_str())),
        _ => return false,
    };
    attachment_id.is_none() || admitted.session_id.as_deref() != session_id
}

/// The single terminal-response decision shared by the WSS and DataChannel
/// browser-request paths. A response is terminal for this attachment when the
/// Host either denied the resident principal (`REMOTE_AUTH_REQUIRED` /
/// `REMOTE_PRINCIPAL_REVOKED`) or reported that the attachment ended, detached
/// or moved to a different session.
fn terminal_response(admitted: &MediaState, result: &Response, request_id: u64) -> bool {
    is_terminal_authorization_denial(result, request_id) || response_ends_media(admitted, result)
}

async fn host_request(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    request: Request,
) -> Result<Response> {
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), write.write_all(&bytes)).await??;
    tokio::time::timeout(Duration::from_secs(15), reply(reader)).await.context("Host response deadline")?
}

async fn host_command(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    internal_id: &mut u64,
    command: Command,
) -> Result<Response> {
    *internal_id = internal_id.checked_sub(1).context("Host internal request id exhausted")?;
    host_request(write, reader, Request { id: *internal_id, command, operation: None }).await
}

fn split_error(error: anyhow::Error) -> (String, String) {
    let message = error.to_string();
    if let Some((code, message)) = message.split_once(": ") { (code.to_owned(), message.to_owned()) }
    else { ("WEBRTC_NEGOTIATION_FAILED".into(), message) }
}

pub async fn send(remote: &mut Socket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), remote.send(message)).await.context("WSS consumer stalled")??; Ok(())
}

fn packet_session(packet: &VideoPacket) -> Option<String> {
    match packet {
        VideoPacket::Waiting { session_id }
        | VideoPacket::Unavailable { session_id, .. }
        | VideoPacket::EncoderUnavailable { session_id, .. }
        | VideoPacket::Closed { session_id } => Some(session_id.clone()),
        VideoPacket::AccessUnit { source, .. } => Some(source.session_id.clone()),
    }
}

pub async fn media(mut remote: Socket, mut latest: watch::Receiver<Option<Arc<EncodedFrame>>>, mut active: watch::Receiver<MediaState>) -> Result<()> {
    let mut emit = true;
    loop {
        // A client may end its control attachment before this endpoint receives
        // or forwards the final Closed frame (for example MainActivity.onStop
        // calls network.disconnect after moveTaskToBack). That is a normal
        // client teardown, not a server/media delivery failure: there is no
        // longer a consumer that must receive Closed.
        let expected = (*active.borrow()).clone();
        if !expected.attached { return Ok(()); }
        let expected_session = expected.session_id.clone().context("Attached media state has no session identity")?;
        let expected_generation = expected.generation;
        let frame = if emit { latest.borrow_and_update().clone() } else { None };
        if let Some(frame) = frame {
            // Exact admitted session/generation only. A frame from any other
            // profile session is never forwarded to this peer.
            if packet_session(&frame.packet).as_deref() != Some(expected_session.as_str()) {
                tokio::select! {
                    changed = latest.changed() => { changed.context("Encoder stream ended")?; emit = true; }
                    changed = active.changed() => {
                        match changed {
                            Ok(_) => {
                                let current = (*active.borrow()).clone();
                                if !current.attached || current.generation != expected_generation { return Ok(()); }
                                emit = true;
                            }
                            Err(_) => return Ok(()),
                        }
                    }
                }
                continue;
            }
            send(&mut remote, Message::Binary(frame.wire_bytes()?.into())).await?;
            if matches!(frame.packet, VideoPacket::Closed { .. }) { return Ok(()); }
        }
        emit = false;
        tokio::select! {
            changed = latest.changed() => { changed.context("Encoder stream ended")?; emit = true; }
            changed = active.changed() => {
                match changed {
                    Ok(_) => {
                        let current = (*active.borrow()).clone();
                        if !current.attached || current.generation != expected_generation { return Ok(()); }
                    }
                    Err(_) => return Ok(()),
                }
            }
            message = remote.next() => match message {
                Some(Ok(Message::Ping(bytes))) => send(&mut remote, Message::Pong(bytes)).await?,
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                _ => anyhow::bail!("Media channel is read-only"),
            }
        }
    }
}

/// The one per-session ingest task. It borrows the durable's listener and
/// cancellation handle and owns only its reader and encoded fan-out sender.
/// Every potentially blocking transport operation competes with cancellation.
pub async fn ingest(
    listener: tokio::net::UnixListener,
    encoded_latest: watch::Sender<Option<Arc<EncodedFrame>>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    // Startup is the existing concrete 10-second adapter deadline, racing
    // cancellation. There is no separate pending-adapter future.
    let accepted = tokio::select! {
        biased;
        _ = wait_cancelled(&mut cancel) => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(10), listener.accept()) => result,
    }.context("Media adapter startup deadline")??;
    let (socket, _) = accepted;
    let mut reader = BufReader::new(socket);
    loop {
        let mut header = Vec::new();
        // Scope the bounded header reader so its mutable borrow of `reader`
        // ends before the payload read below.
        let read = {
            let mut bounded = (&mut reader).take(4096);
            tokio::select! {
                biased;
                read = bounded.read_until(b'\n', &mut header) => read,
                _ = wait_cancelled(&mut cancel) => return Ok(()),
            }
        };
        read?;
        ensure!(header.len() < 4096 && header.last() == Some(&b'\n'), "Encoder stream ended or invalid header");
        header.pop();
        let packet: VideoPacket = serde_json::from_slice(&header)?;
        let size = match &packet { VideoPacket::AccessUnit { byte_length, .. } => *byte_length, _ => 0 };
        ensure!(size <= 4 * 1024 * 1024, "Encoded frame exceeds endpoint budget");
        let mut payload = vec![0; size as usize];
        let payload_read = tokio::select! {
            biased;
            read = tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut payload)) => read,
            _ = wait_cancelled(&mut cancel) => return Ok(()),
        };
        payload_read??;
        encoded_latest.send_replace(Some(Arc::new(EncodedFrame { packet: packet.clone(), bytes: payload.into() })));
        if matches!(packet, VideoPacket::EncoderUnavailable { .. }) {
            // Terminal marker is retained, then the sole sender drops so every
            // subscriber watch closes. The supervisor joins this task.
            drop(encoded_latest);
            return Ok(());
        }
        if matches!(packet, VideoPacket::Closed { .. }) {
            drop(encoded_latest);
            return Ok(());
        }
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) -> () {
    loop {
        if *cancel.borrow() { return; }
        if cancel.changed().await.is_err() { return; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// B1: a correlated `REMOTE_PRINCIPAL_REVOKED` / `REMOTE_AUTH_REQUIRED`
    /// denial is terminal for the attachment's media. `terminal_response`,
    /// the one decision both request paths use, must classify the denial as
    /// terminal even though it is not a `Status`/`ProfileSelected` result, and
    /// the fence must invalidate existing and new `/media` grants.
    #[test]
    fn correlated_authorization_denial_fences_attachment_media() {
        let (active, receiver) = watch::channel(MediaState {
            attached: true,
            session_id: Some("shared-session".into()),
            generation: 4,
        });
        let admitted = active.borrow().clone();
        let denied = Response::Error {
            id: 200,
            code: RemoteAdmissionErrorCode::RemotePrincipalRevoked.as_str().to_string(),
            message: "Remote device is revoked".into(),
        };
        assert!(!response_ends_media(&active.borrow(), &denied),
            "an authorization denial is not a Status/ProfileSelected result and must be fenced explicitly");
        assert!(terminal_response(&admitted, &denied, 200),
            "the shared terminal decision must treat a correlated denial as terminal");
        assert!(!terminal_response(&admitted, &denied, 201),
            "a denial for a different request id must not be treated as terminal");
        fence_media_state(&active);
        let fenced = receiver.borrow().clone();
        assert!(!fenced.attached, "a terminal denial must clear the attachment-local media admission");
        assert_eq!(fenced.session_id, None, "a terminal denial must drop the admitted session identity");
        assert_eq!(fenced.generation, 5, "a terminal denial must bump the generation so existing /media streams wake");
        let auth_required = Response::Error {
            id: 5,
            code: RemoteAdmissionErrorCode::RemoteAuthRequired.as_str().to_string(),
            message: "Remote device is not registered".into(),
        };
        assert!(terminal_response(&admitted, &auth_required, 5),
            "REMOTE_AUTH_REQUIRED must also be media-terminal for the denied attachment");
    }

    #[tokio::test]
    async fn terminal_encoder_unavailable_closes_fanout_watch() {
        let path = std::env::temp_dir().join(format!("ch-ing-{}.sock", uuid::Uuid::new_v4().simple()));
        let listener = tokio::net::UnixListener::bind(&path).expect("bind ingest socket");
        let mut writer = tokio::net::UnixStream::connect(&path).await.expect("connect ingest socket");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (encoded_tx, mut encoded_rx) = watch::channel(None);
        let task = tokio::spawn(ingest(listener, encoded_tx, cancel_rx));
        let packet = VideoPacket::EncoderUnavailable {
            session_id: "terminal-session".into(),
            message: "configured encoder exited".into(),
        };
        let mut header = serde_json::to_vec(&packet).expect("serialize unavailable packet");
        header.push(b'\n');
        writer.write_all(&header).await.expect("write unavailable packet");

        encoded_rx.changed().await.expect("receive unavailable encoded state");
        assert!(matches!(encoded_rx.borrow().as_ref().map(|frame| &frame.packet), Some(VideoPacket::EncoderUnavailable { .. })));
        assert!(tokio::time::timeout(Duration::from_millis(200), task).await
            .expect("terminal ingest must finish").is_ok());
        assert!(tokio::time::timeout(Duration::from_secs(1), encoded_rx.changed()).await
            .expect("encoded watch closure was not observed").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn closed_packet_is_terminal_and_preserves_matching_session_state() {
        let path = std::env::temp_dir().join(format!("ch-closed-{}.sock", uuid::Uuid::new_v4().simple()));
        let listener = tokio::net::UnixListener::bind(&path).expect("bind ingest socket");
        let mut writer = tokio::net::UnixStream::connect(&path).await.expect("connect ingest socket");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (encoded_tx, mut encoded_rx) = watch::channel(None);
        let task = tokio::spawn(ingest(listener, encoded_tx, cancel_rx));
        let packet = VideoPacket::Closed { session_id: "closed-session".into() };
        let mut header = serde_json::to_vec(&packet).expect("serialize closed packet");
        header.push(b'\n');
        writer.write_all(&header).await.expect("write closed packet");

        encoded_rx.changed().await.expect("receive closed encoded state");
        assert!(matches!(
            encoded_rx.borrow().as_ref().map(|frame| &frame.packet),
            Some(VideoPacket::Closed { session_id }) if session_id == "closed-session"
        ));
        assert!(tokio::time::timeout(Duration::from_secs(1), task).await
            .expect("closed ingest must finish").is_ok());
        assert!(tokio::time::timeout(Duration::from_millis(100), encoded_rx.changed()).await
            .expect("encoded watch closure was not observed").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn host_unavailable_eof_keeps_generic_stream_error() {
        let path = std::env::temp_dir().join(format!("ch-eof-{}.sock", uuid::Uuid::new_v4().simple()));
        let listener = tokio::net::UnixListener::bind(&path).expect("bind ingest socket");
        let mut writer = tokio::net::UnixStream::connect(&path).await.expect("connect ingest socket");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (encoded_tx, mut encoded_rx) = watch::channel(None);
        let task = tokio::spawn(ingest(listener, encoded_tx, cancel_rx));
        let packet = VideoPacket::Unavailable {
            session_id: "host-session".into(),
            message: "capture temporarily unavailable".into(),
        };
        let mut header = serde_json::to_vec(&packet).expect("serialize host unavailable packet");
        header.push(b'\n');
        writer.write_all(&header).await.expect("write host unavailable packet");
        writer.shutdown().await.expect("close encoded stream");

        encoded_rx.changed().await.expect("receive host unavailable encoded state");
        assert!(matches!(encoded_rx.borrow().as_ref().map(|frame| &frame.packet), Some(VideoPacket::Unavailable { .. })));
        assert!(task.await.expect("ingest task panicked").is_err(), "Host unavailable EOF must remain a generic ingest error");
        let _ = std::fs::remove_file(&path);
    }
}
