//! Local raw-frame consumer -> bounded H.264 socket stream. No network listener.
use std::{path::PathBuf, time::{Duration, Instant}};
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use obscura_host_protocol::{FramePacket, SourceDimensions, VideoCodec, VideoPacket, VideoQualityProfile, VideoQualitySettings};
use obscura_media::QualityControl;
use tokio::sync::mpsc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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
    /// Optional endpoint-owned typed control socket. When present, the adapter
    /// rejects a broken or invalid quality control connection instead of
    /// continuing with guessed encoder state.
    #[arg(long)]
    quality_socket: Option<PathBuf>,
}

enum QualityEvent {
    Update { session_id: String, generation: u64, effective: VideoQualitySettings },
    Error(String),
}

async fn quality_reader(socket: tokio::net::UnixStream, tx: mpsc::Sender<QualityEvent>) {
    let mut reader = BufReader::new(socket);
    loop {
        let mut header = Vec::new();
        match (&mut reader).take(4096).read_until(b'\n', &mut header).await {
            Ok(0) => {
                let _ = tx.send(QualityEvent::Error("Quality control socket closed unexpectedly".into())).await;
                return;
            }
            Ok(_) => {}
            Err(error) => {
                let _ = tx.send(QualityEvent::Error(format!("Quality control socket read failed: {error}"))).await;
                return;
            }
        }
        if header.len() >= 4096 || header.last() != Some(&b'\n') {
            let _ = tx.send(QualityEvent::Error("Quality control socket ended or invalid header".into())).await;
            return;
        }
        header.pop();
        match serde_json::from_slice::<QualityControl>(&header) {
            Ok(QualityControl::Update { session_id, generation, requested: _, effective }) => {
                if tx.send(QualityEvent::Update { session_id, generation, effective }).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(QualityEvent::Error(format!("Invalid quality control command: {error}"))).await;
                return;
            }
        }
    }
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket = tokio::net::UnixStream::connect(&args.frames_socket).await.context("Connect Host frame socket")?;
    let mut reader = BufReader::new(socket);
    let mut output = tokio::net::UnixStream::connect(&args.video_socket).await.context("Connect encoded media consumer")?;
    let mut quality_rx = None;
    if let Some(quality_socket) = args.quality_socket {
        let quality = tokio::net::UnixStream::connect(&quality_socket).await.context("Connect endpoint quality control socket")?;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(quality_reader(quality, tx));
        quality_rx = Some(rx);
    }
    let encoder_id = uuid::Uuid::new_v4().to_string();
    let start = Instant::now();
    let mut session = None;
    let mut previous: Option<(u64, u64, u64)> = None;
    let mut quality_session = None;
    let mut quality_generation = None;
    let mut quality_settings = VideoQualitySettings { profile: VideoQualityProfile::Balanced, low_latency: true };
    loop {
        let mut header = Vec::new();
        if let Some(rx) = &mut quality_rx {
            let mut frame_reader = (&mut reader).take(4096);
            let read_header = frame_reader.read_until(b'\n', &mut header);
            tokio::select! {
                event = rx.recv() => match event {
                    Some(QualityEvent::Update { session_id, generation, effective }) => {
                        if let Some(session) = &session {
                            ensure!(session == &session_id, "Quality control session changed on live connection");
                        }
                        ensure!(generation > 0, "Quality control generation must be positive");
                        if let Some(previous) = quality_generation {
                            ensure!(generation > previous, "Quality control generation regressed");
                        }
                        quality_session = Some(session_id);
                        quality_generation = Some(generation);
                        quality_settings = effective;
                        continue;
                    }
                    Some(QualityEvent::Error(error)) => bail!("{error}"),
                    None => bail!("Quality control channel ended unexpectedly"),
                },
                result = read_header => {
                    result?;
                }
            }
        } else {
            (&mut reader).take(4096).read_until(b'\n', &mut header).await?;
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
        if let Some(expected) = &quality_session {
            ensure!(expected == current, "Quality control session does not match Host media session");
        }
        let mut bytes = Vec::new();
        let packet = match packet {
            FramePacket::Waiting { session_id } => VideoPacket::Waiting { session_id },
            FramePacket::Unavailable { session_id, message } => VideoPacket::Unavailable { session_id, message },
            FramePacket::Closed { session_id } => VideoPacket::Closed { session_id },
            FramePacket::Frame { info } => {
                obscura_media::validate_pixels(info.width, info.height, info.byte_length)?;
                ensure!(info.stride == info.width * 4, "Unsupported raw frame stride");
                ensure!(info.source_dimensions == SourceDimensions::from_raw(info.width, info.height),
                    "Host source dimensions do not match raw frame dimensions");
                if let Some((sequence, document, viewport)) = previous {
                    ensure!(info.sequence > sequence && info.document_revision >= document && info.viewport_revision >= viewport,
                        "Regressive Host frame identity");
                }
                previous = Some((info.sequence, info.document_revision, info.viewport_revision));
                let pts_us = u64::try_from(start.elapsed().as_micros()).context("Media timestamp exhausted")?;
                let mut rgba = vec![0; info.byte_length as usize];
                tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut rgba)).await.context("Incomplete raw frame deadline")??;
                match obscura_media::encode_with_settings(&args.ffmpeg, info.width, info.height, rgba, quality_settings).await {
                    Ok(encoded) => {
                        bytes = encoded;
                        VideoPacket::AccessUnit { source: info, encoder_id: encoder_id.clone(), pts_us, codec: VideoCodec::H264AnnexB,
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

#[cfg(not(unix))]
fn main() { eprintln!("Local media adapter requires Unix sockets"); std::process::exit(1); }
