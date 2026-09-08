//! Local raw-frame consumer -> bounded H.264 socket stream. No network listener.
use std::{path::PathBuf, time::{Duration, Instant}};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use obscura_host_protocol::{FramePacket, VideoCodec, VideoPacket};
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
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket = tokio::net::UnixStream::connect(&args.frames_socket).await.context("Connect Host frame socket")?;
    let mut reader = BufReader::new(socket);
    let mut output = tokio::net::UnixStream::connect(&args.video_socket).await.context("Connect encoded media consumer")?;
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
                match obscura_media::encode(&args.ffmpeg, info.width, info.height, rgba).await {
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

#[cfg(not(unix))]
fn main() { eprintln!("Local media adapter requires Unix sockets"); std::process::exit(1); }
