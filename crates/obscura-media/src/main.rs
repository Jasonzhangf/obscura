//! Local raw-frame consumer -> bounded H.264 socket stream. No network listener.
use std::{path::PathBuf, time::{Duration, Instant}};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use obscura_host_protocol::{FramePacket, VideoCodec, VideoPacket};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// One in-flight encode result or the signal that cancelled its borrow.
enum EncodeRun {
    Finished(Result<Vec<u8>>),
    Interrupted(&'static str),
}

#[derive(Parser)]
#[command(about = "Encode Host raw frames as H.264 access units to a local consumer")]
struct Args {
    #[arg(long)]
    frames_socket: PathBuf,
    /// Consumer-owned private Unix socket; video and control remain separate.
    #[arg(long)]
    video_socket: PathBuf,
    /// External FFmpeg with libx264 enabled. Failure is explicit, no codec fallback.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket = tokio::net::UnixStream::connect(&args.frames_socket).await.context("Connect Host frame socket")?;
    let mut reader = BufReader::new(socket);
    let mut output = tokio::net::UnixStream::connect(&args.video_socket).await.context("Connect encoded media consumer")?;
    // The private encoder-control socket lives beside the consumer-owned video
    // socket, so no new CLI flag or ABI is introduced.
    let control_dir = args.video_socket.parent().context("Video socket has no parent directory")?;
    let control_path = control_dir.join("encoder-control.sock");
    let mut owner = match obscura_media::encoder_owner::EncoderChildOwner::open(&args.ffmpeg, &control_path).await {
        Ok(owner) => owner,
        Err(endpoint_error) => {
            // Standalone consumers have no Endpoint to provide the typed
            // admission resource. Bind it locally and keep using the same
            // EncoderChildOwner for the actual process ownership. Endpoint
            // sessions already bound this path, so they take the branch above.
            let listener = tokio::net::UnixListener::bind(&control_path)
                .with_context(|| format!("Open Host encoder control ({endpoint_error})"))?;
            tokio::spawn(serve_standalone_control(listener));
            obscura_media::encoder_owner::EncoderChildOwner::open(&args.ffmpeg, &control_path).await
                .context("Open standalone encoder control")?
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let encoder_id = uuid::Uuid::new_v4().to_string();
    let start = Instant::now();
    let mut session = None;
    let mut previous: Option<(u64, u64, u64)> = None;
    loop {
        let mut header = Vec::new();
        (&mut reader).take(4096).read_until(b'\n', &mut header).await?;
        ensure!(!header.is_empty(), "Host media ended without Closed");
        ensure!(header.len() < 4096 && header.last() == Some(&b'\n'), "Invalid bounded media header");
        let packet: FramePacket = serde_json::from_slice(&header).context("Decode Host media header")?;
        let current = match &packet {
            FramePacket::Frame { info } => &info.session_id,
            FramePacket::Waiting { session_id } | FramePacket::Unavailable { session_id, .. } | FramePacket::Closed { session_id } => session_id,
        };
        if let Some(session) = &session { ensure!(session == current, "Host media session changed on live connection"); }
        else { session = Some(current.clone()); }
        let mut bytes = Vec::new();
        let packet = match packet {
            FramePacket::Waiting { session_id } => VideoPacket::Waiting { session_id },
            FramePacket::Unavailable { session_id, message } => VideoPacket::Unavailable { session_id, message },
            FramePacket::Closed { session_id } => VideoPacket::Closed { session_id },
            FramePacket::Frame { info } => {
                obscura_media::validate_pixels(info.width, info.height, info.byte_length)?;
                ensure!(info.stride == info.width * 4, "Unsupported raw frame stride");
                if let Some((sequence, document, viewport)) = previous {
                    ensure!(info.sequence > sequence && info.document_revision >= document && info.viewport_revision >= viewport,
                        "Regressive Host frame identity");
                }
                previous = Some((info.sequence, info.document_revision, info.viewport_revision));
                let pts_us = u64::try_from(start.elapsed().as_micros()).context("Media timestamp exhausted")?;
                let mut rgba = vec![0; info.byte_length as usize];
                tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut rgba)).await.context("Incomplete raw frame deadline")??;
                // The complete borrowed encode runs under one signal select.
                // If a signal wins, the borrow ends before the same retained
                // owner finalizes the group, so the group is never dropped
                // while an encode borrow is in flight.
                let outcome = {
                    let encode = owner.encode_frame(info.width, info.height, rgba);
                    tokio::pin!(encode);
                    tokio::select! {
                        result = &mut encode => EncodeRun::Finished(result),
                        _ = tokio::signal::ctrl_c() => EncodeRun::Interrupted("ctrl-c"),
                        _ = term.recv() => EncodeRun::Interrupted("SIGTERM"),
                    }
                };
                let encoded = match outcome {
                    EncodeRun::Finished(encoded) => encoded,
                    EncodeRun::Interrupted(signal) => {
                        owner.finalize().await.context("Finalize encoder group on interrupt")?;
                        return Err(anyhow::anyhow!("Media adapter received {signal} before frame completion"));
                    }
                };
                match encoded {
                    Ok(encoded) => {
                        bytes = encoded;
                        VideoPacket::AccessUnit { coded_width: (info.width + 1) & !1, coded_height: (info.height + 1) & !1,
                            source: info, encoder_id: encoder_id.clone(), pts_us, codec: VideoCodec::H264AnnexB,
                            keyframe: true, byte_length: bytes.len() as u64 }
                    }
                    Err(error) => {
                        let failure = VideoPacket::EncoderUnavailable { session_id: info.session_id, message: error.to_string() };
                        send(&mut output, &failure, &[]).await?;
                        return Err(error);
                    }
                }
            }
        };
        send(&mut output, &packet, &bytes).await?;
        if matches!(packet, VideoPacket::Closed { .. }) { return Ok(()); }
    }
}

async fn send(output: &mut (impl tokio::io::AsyncWrite + Unpin), packet: &VideoPacket, bytes: &[u8]) -> Result<()> {
    let mut header = serde_json::to_vec(packet)?;
    ensure!(header.len() < 4096, "Encoded media header exceeds limit");
    header.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), async {
        output.write_all(&header).await?;
        output.write_all(bytes).await?;
        output.flush().await
    }).await.context("Encoded media consumer stalled")??;
    Ok(())
}

async fn serve_standalone_control(listener: tokio::net::UnixListener) {
    let Ok((stream, _)) = listener.accept().await else { return; };
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        let count = match reader.read_line(&mut line).await { Ok(count) => count, Err(_) => return };
        if count == 0 { return; }
        let request: obscura_media::encoder_owner::EncoderControlRequest =
            match serde_json::from_str(line.trim_end()) { Ok(request) => request, Err(_) => return };
        use obscura_media::encoder_owner::{EncoderControlRequest as Request, EncoderControlResponse as Response};
        let response = match request {
            Request::Acquire { .. } => Response::Acquire { permit: obscura_media::encoder_owner::EncoderPermit::Granted },
            Request::Register { .. } => Response::Register { refused: false },
            Request::Revoke { .. } => Response::Revoke { refused: false },
            Request::Settle { .. } => Response::Settle,
        };
        let mut bytes = match serde_json::to_vec(&response) { Ok(bytes) => bytes, Err(_) => return };
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() { return; }
    }
}

#[cfg(not(unix))]
fn main() { eprintln!("Local media adapter requires Unix sockets"); std::process::exit(1); }
