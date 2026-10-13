//! One latest immutable frame, shared by all local viewers. No input admission.
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use anyhow::Result;
use obscura_host_protocol::FramePacket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::{timeout, Duration};

pub struct Frame { pub packet: FramePacket, pub pixels: Vec<u8> }
pub type SharedFrame = Arc<Frame>;

/// The single frame-interest accounting owner for one session. Every raw-media
/// consumer, exact-session or legacy, acquires and releases this one count. No
/// path replaces the count from an independent local total, so the last
/// consumer to leave always drives interest back to zero.
#[derive(Clone)]
pub struct FrameInterest(watch::Sender<usize>);

impl FrameInterest {
    pub fn new(sender: watch::Sender<usize>) -> Self { Self(sender) }
    pub fn acquire(&self) { self.0.send_modify(|count| *count = count.saturating_add(1)); }
    pub fn release(&self) { self.0.send_modify(|count| *count = count.saturating_sub(1)); }
}

/// One exact-session raw media source owned by the Host. It validates the
/// session identity before binding so a long or malformed session id cannot
/// truncate a Unix socket path.
pub struct SessionMediaSource {
    pub session_id: String,
    pub socket_path: std::path::PathBuf,
    listener: Option<UnixListener>,
}

impl Drop for SessionMediaSource {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.socket_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("session media socket cleanup: {error}");
            }
        }
    }
}

impl SessionMediaSource {
    pub fn socket_name(session_id: &str) -> Result<String> {
        let valid = session_id.len() == 36
            && session_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        anyhow::ensure!(valid, "Session media socket requires a 36-char lowercase UUID");
        Ok(format!("session-{session_id}.sock"))
    }

    pub fn bind(socket_dir: &Path, session_id: &str) -> Result<Self> {
        let name = Self::socket_name(session_id)?;
        let socket_path = socket_dir.join(&name);
        anyhow::ensure!(socket_path.to_string_lossy().len() < 104, "Session media socket path exceeds macOS bound");
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { session_id: session_id.to_owned(), socket_path, listener: Some(listener) })
    }

    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }
}

/// Accept exactly this session's local raw-media viewers, raise and lower that
/// session's frame interest, and stream its frames. Only `media::stream`
/// writes to the socket; the stream is read-only for the consumer.
pub async fn serve_session(
    mut listener: UnixListener,
    frames: watch::Receiver<SharedFrame>,
    interest: FrameInterest,
    session_id: String,
) {
    loop {
        let Ok((socket, _)) = listener.accept().await else { return; };
        interest.acquire();
        let frames = frames.clone();
        let interest = interest.clone();
        tokio::spawn(async move {
            let _ = stream(socket, frames).await;
            interest.release();
        });
        let _ = session_id;
    }
}

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
