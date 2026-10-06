//! Local raw-frame consumer -> bounded H.264 socket stream. No network listener.
use std::{path::PathBuf, time::{Duration, Instant}};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use obscura_host_protocol::{FramePacket, VideoCodec, VideoPacket};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// One in-flight blocking operation result, or the lifetime signal that
/// cancelled it. Every potentially blocking await in the adapter loop competes
/// with SIGTERM/ctrl-c through this wrapper, so the cancellation endpoint stays
/// reachable even while the adapter waits on an idle raw socket, forwards a
/// non-frame packet, or blocks on a stalled output consumer.
enum Run<T> {
    Finished(T),
    Interrupted(&'static str),
}

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
    // Lifetime owner for the standalone control socket this invocation binds.
    // It removes only the path this invocation created, on every exit path.
    let mut _standalone_control: Option<ControlSocketGuard> = None;
    let mut owner = match obscura_media::encoder_owner::EncoderChildOwner::open(&args.ffmpeg, &control_path).await {
        Ok(owner) => owner,
        Err(endpoint_error) => {
            // Standalone consumers have no Endpoint to provide the typed
            // admission resource. Bind it locally and keep using the same
            // EncoderChildOwner for the actual process ownership. Endpoint
            // sessions already bound this path, so they take the branch above.
            let listener = tokio::net::UnixListener::bind(&control_path)
                .with_context(|| format!("Open Host encoder control ({endpoint_error})"))?;
            // The bind above succeeds only when this invocation created the
            // path. Own it for the whole adapter lifetime so a later invocation
            // can bind the same consumer directory again. A pre-existing path
            // fails the bind and is never removed.
            _standalone_control = Some(ControlSocketGuard::new(control_path.clone())
                .context("Record standalone encoder control socket identity")?);
            tokio::spawn(serve_standalone_control(listener));
            obscura_media::encoder_owner::EncoderChildOwner::open(&args.ffmpeg, &control_path).await
                .context("Open standalone encoder control")?
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // One receiver pair is shared for the whole lifetime: each raced operation
    // re-borrows it so a signal that arrives during any await is observed, not
    // queued until the next encode.
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let encoder_id = uuid::Uuid::new_v4().to_string();
    let start = Instant::now();
    let mut session = None;
    let mut previous: Option<(u64, u64, u64)> = None;
    loop {
        let mut header = Vec::new();
        match race(ctrl_c.as_mut(), &mut term, async {
            (&mut reader).take(4096).read_until(b'\n', &mut header).await
        }).await {
            Run::Finished(read) => { read?; }
            Run::Interrupted(signal) => return interrupt(&mut owner, signal).await,
        }
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
                match race(ctrl_c.as_mut(), &mut term, tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut rgba))).await {
                    Run::Finished(read) => { read.context("Incomplete raw frame deadline")??; }
                    Run::Interrupted(signal) => return interrupt(&mut owner, signal).await,
                }
                // The complete borrowed encode runs under one signal select.
                // If a signal wins, the borrow ends before the same retained
                // owner finalizes the group, so the group is never dropped
                // while an encode borrow is in flight.
                let outcome = {
                    let encode = owner.encode_frame(info.width, info.height, rgba);
                    tokio::pin!(encode);
                    tokio::select! {
                        result = &mut encode => EncodeRun::Finished(result),
                        _ = ctrl_c.as_mut() => EncodeRun::Interrupted("ctrl-c"),
                        _ = term.recv() => EncodeRun::Interrupted("SIGTERM"),
                    }
                };
                let encoded = match outcome {
                    EncodeRun::Finished(encoded) => encoded,
                    EncodeRun::Interrupted(signal) => {
                        return interrupt(&mut owner, signal).await;
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
        match race(ctrl_c.as_mut(), &mut term, send(&mut output, &packet, &bytes)).await {
            Run::Finished(sent) => sent?,
            Run::Interrupted(signal) => return interrupt(&mut owner, signal).await,
        }
        if matches!(packet, VideoPacket::Closed { .. }) { return Ok(()); }
    }
}

/// Race one potentially blocking adapter operation with the lifetime signal.
/// The operation future is dropped if a signal wins, so a pending header read,
/// payload read, or output send cannot hold the adapter past SIGTERM.
async fn race<T>(
    ctrl_c: impl std::future::Future<Output = std::io::Result<()>> + Unpin,
    term: &mut tokio::signal::unix::Signal,
    operation: impl std::future::Future<Output = T>,
) -> Run<T> {
    tokio::select! {
        result = operation => Run::Finished(result),
        _ = ctrl_c => Run::Interrupted("ctrl-c"),
        _ = term.recv() => Run::Interrupted("SIGTERM"),
    }
}

/// Finish a lifetime interruption: finalize the retained encoder owner only
/// when it still holds a live child, then return an error so the socket guard
/// drops and removes the control path. With no live child there is nothing to
/// settle and the adapter exits directly.
async fn interrupt(owner: &mut obscura_media::encoder_owner::EncoderChildOwner, signal: &'static str) -> Result<()> {
    if owner.has_live_child() {
        owner.finalize().await.context("Finalize encoder group on interrupt")?;
    }
    Err(anyhow::anyhow!("Media adapter received {signal} before completion"))
}

/// Owns the standalone `encoder-control.sock` path this invocation bound. It
/// removes the path on every exit so a later standalone adapter invocation can
/// bind the same consumer directory again. Dropping a `UnixListener` does not
/// unlink the filesystem entry, so the path needs an explicit owner.
#[cfg(unix)]
struct ControlSocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl ControlSocketGuard {
    fn new(path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self { path, dev: metadata.dev(), ino: metadata.ino() })
    }
}

#[cfg(unix)]
impl Drop for ControlSocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else { return; };
        if metadata.dev() == self.dev && metadata.ino() == self.ino {
            let _ = std::fs::remove_file(&self.path);
        }
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
