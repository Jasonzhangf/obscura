//! One latest immutable frame, shared by all local viewers. No input admission.
use std::sync::Arc;
use anyhow::Result;
use obscura_host_protocol::FramePacket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::{timeout, Duration};

pub struct Frame { pub packet: FramePacket, pub pixels: Vec<u8> }
pub type SharedFrame = Arc<Frame>;

pub fn state(packet: FramePacket) -> SharedFrame {
    Arc::new(Frame { packet, pixels: Vec::new() })
}

pub async fn stream(socket: UnixStream, mut frames: watch::Receiver<SharedFrame>) -> Result<()> {
    let (mut input, mut output) = socket.into_split();
    loop {
        let frame = frames.borrow_and_update().clone();
        let mut header = serde_json::to_vec(&frame.packet)?;
        anyhow::ensure!(header.len() < 4096, "Media header exceeds limit");
        header.push(b'\n');
        timeout(Duration::from_secs(2), async {
            output.write_all(&header).await?;
            output.write_all(&frame.pixels).await
        }).await??;
        if matches!(frame.packet, FramePacket::Closed { .. }) { return Ok(()); }
        drop(frame);
        let mut byte = [0];
        tokio::select! {
            changed = frames.changed() => { if changed.is_err() { return Ok(()); } }
            received = input.read(&mut byte) => {
                anyhow::ensure!(received? == 0, "Media socket is read-only");
                return Ok(());
            }
        }
    }
}
