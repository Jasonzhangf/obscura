use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use anyhow::{bail, ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, Mode, Request, Response, ResultValue, VideoPacket, WebRtcSignal};
use obscura_media::EncodedFrame;
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::UnixStream, sync::{mpsc, watch}};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub type Socket = WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;
pub struct Video { pub bytes: Vec<u8>, pub closed: bool }
pub type Frame = Arc<Video>;

async fn reply(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Response> {
    let mut bytes = Vec::new();
    (&mut *reader).take(1024 * 1024 + 1).read_until(b'\n', &mut bytes).await?;
    ensure!(bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'), "Invalid bounded Host response");
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn control(
    mut remote: Socket,
    path: &Path,
    active: watch::Sender<bool>,
    latest: watch::Receiver<Option<Arc<EncodedFrame>>>,
    enable_webrtc: bool,
    webrtc_bind: Option<SocketAddr>,
) -> Result<()> {
    let local = UnixStream::connect(path).await.context("Connect local Host control")?;
    let (read, mut write) = local.into_split(); let mut reader = BufReader::new(read);
    let ready = reply(&mut reader).await?;
    send(&mut remote, Message::text(serde_json::to_string(&ready)?)).await?;
    let mut webrtc_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut browser_requests: Option<mpsc::Receiver<Request>> = None;
    let mut browser_responses: Option<mpsc::Sender<Response>> = None;
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
                            } else if !*active.borrow() {
                                Err(anyhow::anyhow!("ATTACH_REQUIRED: Observe attachment must be active before WebRTC signaling"))
                            } else if webrtc_task.is_some() {
                                Err(anyhow::anyhow!("WEBRTC_CONNECTED: One WebRTC media connection per mTLS control attachment"))
                            } else {
                                let bind = webrtc_bind.context("WebRTC enablement requires an explicit bind address")?;
                                let authorization = host_command(&mut write, &mut reader, &mut internal_id,
                                    Command::AuthorizeWebRtc { capability: capability.clone() }).await?;
                                let (binding, authorized_capability) = match authorization {
                                    Response::Result { value: ResultValue::WebRtcAuthorization { binding, capability }, .. } => (binding, capability),
                                    other => bail!("Host rejected WebRTC authorization: {other:?}"),
                                };
                                ensure!(authorized_capability == capability, "Host returned a different WebRTC capability");
                                let (browser_request_tx, browser_request_rx) = mpsc::channel(16);
                                let (browser_response_tx, browser_response_rx) = mpsc::channel(16);
                                let (answer_sdp, endpoint) = obscura_media::webrtc_endpoint::accept_offer(
                                    sdp, capability.clone(), binding.clone(), latest.clone(),
                                    browser_request_tx, browser_response_rx, bind).await?;
                                let consumed = host_command(&mut write, &mut reader, &mut internal_id,
                                    Command::ConsumeWebRtc { binding: binding.clone(), capability: capability.clone() }).await?;
                                ensure!(matches!(consumed, Response::Result { value: ResultValue::WebRtcConsumed { .. }, .. }),
                                    "Host did not consume WebRTC authorization");
                                let answer = WebRtcSignal::Answer { id, capability, binding, sdp: answer_sdp };
                                send(&mut remote, Message::text(serde_json::to_string(&answer)?)).await?;
                                browser_requests = Some(browser_request_rx);
                                browser_responses = Some(browser_response_tx);
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
                let result = if permitted_remote_command(&request.command) {
                    host_request(&mut write, &mut reader, request).await?
                } else {
                    remote_command_error(request.id)
                };
                let detached = update_active(&active, &result);
                send(&mut remote, Message::text(serde_json::to_string(&result)?)).await?;
                if detached {
                    if let Some(task) = webrtc_task.take() { task.abort(); }
                    browser_requests = None;
                    browser_responses = None;
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
                let result = if permitted_remote_command(&request.command) {
                    host_request(&mut write, &mut reader, request).await?
                } else {
                    remote_command_error(request.id)
                };
                let detached = update_active(&active, &result);
                let response_tx = browser_responses.as_ref().context("WebRTC browser response pump unavailable")?;
                response_tx.send(result).await.context("WebRTC browser response receiver ended")?;
                if detached {
                    if let Some(task) = webrtc_task.take() { task.abort(); }
                    browser_requests = None;
                    browser_responses = None;
                }
            }
        }
    }
    if let Some(task) = webrtc_task { task.abort(); }
    Ok(())
}

fn permitted_remote_command(command: &Command) -> bool {
    matches!(command,
        Command::Attach { mode: Mode::Observe, .. }
            | Command::DeclareViewport { .. }
            | Command::Status {}
            | Command::Detach {}
            | Command::RequestTakeover { .. }
            | Command::ReleaseControl { .. }
            | Command::Navigate { .. }
            | Command::Click { .. }
            | Command::InputText { .. }
            | Command::Scroll { .. })
}

fn remote_command_error(id: u64) -> Response {
    Response::Error {
        id,
        code: "REMOTE_COMMAND_FORBIDDEN".into(),
        message: "Prepaired remote access permits observation and human browser operations only".into(),
    }
}

fn update_active(
    active: &watch::Sender<bool>,
    result: &Response,
) -> bool {
    let Response::Result { value: ResultValue::Status(status), .. } = result else { return false; };
    let attached = status.attachment_id.is_some();
    active.send_replace(attached);
    !attached
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

pub async fn media(mut remote: Socket, mut latest: watch::Receiver<Option<Frame>>, mut active: watch::Receiver<bool>) -> Result<()> {
    let mut emit = true;
    loop {
        ensure!(*active.borrow(), "Control attachment ended");
        let frame = if emit { latest.borrow_and_update().clone() } else { None };
        if let Some(frame) = frame {
            send(&mut remote, Message::Binary(frame.bytes.clone().into())).await?;
            if frame.closed { return Ok(()); }
        }
        emit = false;
        tokio::select! {
            changed = latest.changed() => { changed.context("Encoder stream ended")?; emit = true; }
            changed = active.changed() => { changed.context("Control attachment ended")?; }
            message = remote.next() => match message {
                Some(Ok(Message::Ping(bytes))) => send(&mut remote, Message::Pong(bytes)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                _ => anyhow::bail!("Media channel is read-only"),
            }
        }
    }
}

pub async fn ingest(
    socket: UnixStream,
    latest: watch::Sender<Option<Frame>>,
    encoded_latest: watch::Sender<Option<Arc<EncodedFrame>>>,
) -> Result<()> {
    let mut reader = BufReader::new(socket);
    loop {
        let mut header = Vec::new();
        (&mut reader).take(4096).read_until(b'\n', &mut header).await?;
        ensure!(header.len() < 4096 && header.last() == Some(&b'\n'), "Encoder stream ended or invalid header");
        header.pop();
        let packet: VideoPacket = serde_json::from_slice(&header)?;
        let size = match &packet { VideoPacket::AccessUnit { byte_length, .. } => *byte_length, _ => 0 };
        ensure!(size <= 4 * 1024 * 1024, "Encoded frame exceeds endpoint budget");
        let mut payload = vec![0; size as usize];
        tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut payload)).await??;
        encoded_latest.send_replace(Some(Arc::new(EncodedFrame { packet: packet.clone(), bytes: payload.clone().into() })));
        let mut bytes = Vec::with_capacity(4 + header.len() + payload.len());
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes()); bytes.extend_from_slice(&header); bytes.extend_from_slice(&payload);
        let closed = matches!(packet, VideoPacket::Closed { .. });
        latest.send_replace(Some(Arc::new(Video { bytes, closed })));
        if matches!(packet, VideoPacket::EncoderUnavailable { .. }) {
            // EncoderUnavailable is terminal at this local ABI boundary. Close
            // both fan-out watches immediately so every endpoint can deliver
            // its typed DataChannel error even if the adapter delays or fails
            // to close its socket after sending the marker. Keep this media
            // future alive: `server::serve` must not select a completed ingest
            // future and abort the authenticated WebRTC task before delivery.
            drop(latest);
            drop(encoded_latest);
            // The server owns cancellation of this media future: it drops the
            // pinned future after its shutdown branch wins. Release the
            // adapter socket before waiting so the pending future retains no
            // file descriptor while the endpoint remains alive.
            drop(reader);
            return std::future::pending::<Result<()>>().await;
        }
        if closed { return Ok(()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn terminal_encoder_unavailable_closes_fanout_watches() {
        let (mut writer, socket) = UnixStream::pair().expect("create encoded stream pair");
        let (latest_tx, mut latest_rx) = watch::channel(None);
        let (encoded_tx, mut encoded_rx) = watch::channel(None);
        let mut task = tokio::spawn(ingest(socket, latest_tx, encoded_tx));
        let packet = VideoPacket::EncoderUnavailable {
            session_id: "terminal-session".into(),
            message: "configured encoder exited".into(),
        };
        let mut header = serde_json::to_vec(&packet).expect("serialize unavailable packet");
        header.push(b'\n');
        writer.write_all(&header).await.expect("write unavailable packet");

        latest_rx.changed().await.expect("receive unavailable media state");
        assert!(matches!(latest_rx.borrow().as_ref().map(|frame| frame.closed), Some(false)));
        assert!(tokio::time::timeout(Duration::from_secs(1), latest_rx.changed()).await
            .expect("latest watch closure was not observed").is_err());
        encoded_rx.changed().await.expect("receive unavailable encoded state");
        assert!(matches!(encoded_rx.borrow().as_ref().map(|frame| &frame.packet), Some(VideoPacket::EncoderUnavailable { .. })));
        assert!(tokio::time::timeout(Duration::from_secs(1), encoded_rx.changed()).await
            .expect("encoded watch closure was not observed").is_err());
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut task).await.is_err(),
            "terminal ingest must remain alive until endpoint shutdown");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn host_unavailable_eof_keeps_generic_stream_error() {
        let (mut writer, socket) = UnixStream::pair().expect("create encoded stream pair");
        let (latest_tx, mut latest_rx) = watch::channel(None);
        let (encoded_tx, mut encoded_rx) = watch::channel(None);
        let task = tokio::spawn(ingest(socket, latest_tx, encoded_tx));
        let packet = VideoPacket::Unavailable {
            session_id: "host-session".into(),
            message: "capture temporarily unavailable".into(),
        };
        let mut header = serde_json::to_vec(&packet).expect("serialize host unavailable packet");
        header.push(b'\n');
        writer.write_all(&header).await.expect("write host unavailable packet");
        writer.shutdown().await.expect("close encoded stream");

        latest_rx.changed().await.expect("receive host unavailable media state");
        encoded_rx.changed().await.expect("receive host unavailable encoded state");
        assert!(matches!(encoded_rx.borrow().as_ref().map(|frame| &frame.packet), Some(VideoPacket::Unavailable { .. })));
        assert!(tokio::time::timeout(Duration::from_secs(1), encoded_rx.changed()).await
            .expect("encoded watch closure was not observed").is_err());
        assert!(task.await.expect("ingest task panicked").is_err(), "Host unavailable EOF must remain a generic ingest error");
    }
}
