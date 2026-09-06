use std::{path::Path, sync::Arc, time::Duration};
use anyhow::{ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use obscura_host_protocol::{Command, Mode, Request, Response, ResultValue, VideoPacket};
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

pub async fn control(mut remote: Socket, path: &Path, active: watch::Sender<bool>) -> Result<()> {
    let local = UnixStream::connect(path).await.context("Connect local Host control")?;
    let (read, mut write) = local.into_split(); let mut reader = BufReader::new(read);
    let ready = reply(&mut reader).await?;
    send(&mut remote, Message::text(serde_json::to_string(&ready)?)).await?;
    while let Some(message) = remote.next().await {
        let text = match message? {
            Message::Text(text) => text,
            Message::Ping(bytes) => { send(&mut remote, Message::Pong(bytes)).await?; continue; }
            Message::Close(_) => return Ok(()),
            _ => anyhow::bail!("Control accepts JSON text only"),
        };
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
        }
        send(&mut remote, Message::text(serde_json::to_string(&result)?)).await?;
    }
    Ok(())
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

pub async fn ingest(socket: UnixStream, latest: watch::Sender<Option<Frame>>) -> Result<()> {
    let mut reader = BufReader::new(socket);
    loop {
        let mut header = Vec::new();
        (&mut reader).take(4096).read_until(b'\n', &mut header).await?;
        ensure!(header.len() < 4096 && header.last() == Some(&b'\n'), "Encoder stream ended or invalid header");
        header.pop();
        let packet: VideoPacket = serde_json::from_slice(&header)?;
        let size = match &packet { VideoPacket::AccessUnit { byte_length, .. } => *byte_length, _ => 0 };
        ensure!(size <= 4 * 1024 * 1024, "Encoded frame exceeds endpoint budget");
        let mut bytes = Vec::with_capacity(4 + header.len() + size as usize);
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes()); bytes.extend_from_slice(&header);
        let offset = bytes.len(); bytes.resize(offset + size as usize, 0);
        tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut bytes[offset..])).await??;
        let closed = matches!(packet, VideoPacket::Closed { .. });
        latest.send_replace(Some(Arc::new(Video { bytes, closed })));
        if closed { return Ok(()); }
    }
}
