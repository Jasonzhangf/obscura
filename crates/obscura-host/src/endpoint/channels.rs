use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use anyhow::{bail, ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, Mode, Request, Response, ResultValue, VideoPacket, WebRtcSignal};
use obscura_media::EncodedFrame;
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::UnixStream, sync::watch};
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
    let mut webrtc_task = None;
    let mut internal_id = u64::MAX;
    while let Some(message) = remote.next().await {
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
                        let authorization = host_request(&mut write, &mut reader, &mut internal_id,
                            Command::AuthorizeWebRtc { capability: capability.clone() }).await?;
                        let (binding, authorized_capability) = match authorization {
                            Response::Result { value: ResultValue::WebRtcAuthorization { binding, capability }, .. } => (binding, capability),
                            other => bail!("Host rejected WebRTC authorization: {other:?}"),
                        };
                        ensure!(authorized_capability == capability, "Host returned a different WebRTC capability");
                        let (answer_sdp, endpoint) = obscura_media::webrtc_endpoint::accept_offer(
                            sdp, capability.clone(), binding.clone(), latest.clone(), bind).await?;
                        let consumed = host_request(&mut write, &mut reader, &mut internal_id,
                            Command::ConsumeWebRtc { binding: binding.clone(), capability: capability.clone() }).await?;
                        ensure!(matches!(consumed, Response::Result { value: ResultValue::WebRtcConsumed { .. }, .. }),
                            "Host did not consume WebRTC authorization");
                        let answer = WebRtcSignal::Answer { id, capability, binding, sdp: answer_sdp };
                        send(&mut remote, Message::text(serde_json::to_string(&answer)?)).await?;
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
        let permitted = matches!(request.command, Command::Attach { mode: Mode::Observe, .. } | Command::DeclareViewport { .. } | Command::Status {} | Command::Detach {}
            | Command::RequestTakeover { .. } | Command::ReleaseControl { .. } | Command::Navigate { .. } | Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. });
        let result = if permitted {
            let mut bytes = serde_json::to_vec(&request)?; bytes.push(b'\n');
            tokio::time::timeout(Duration::from_secs(2), write.write_all(&bytes)).await??;
            // Accepted input completes at its Host boundary even after remote EOF.
            tokio::time::timeout(Duration::from_secs(15), reply(&mut reader)).await.context("Host response deadline")??
        } else {
            Response::Error { id: request.id, code: "REMOTE_COMMAND_FORBIDDEN".into(), message: "Prepaired remote access permits observation and human browser operations only".into() }
        };
        if let Response::Result { value: ResultValue::Status(status), .. } = &result {
            active.send_replace(status.attachment_id.is_some());
            if status.attachment_id.is_none() {
                if let Some(task) = webrtc_task.take() { task.abort(); }
            }
        }
        send(&mut remote, Message::text(serde_json::to_string(&result)?)).await?;
    }
    if let Some(task) = webrtc_task { task.abort(); }
    Ok(())
}

async fn host_request(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    internal_id: &mut u64,
    command: Command,
) -> Result<Response> {
    *internal_id = internal_id.checked_sub(1).context("Host internal request id exhausted")?;
    let request = Request { id: *internal_id, command, operation: None };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), write.write_all(&bytes)).await??;
    tokio::time::timeout(Duration::from_secs(15), reply(reader)).await.context("Host WebRTC grant deadline")?
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
        if closed { return Ok(()); }
    }
}
