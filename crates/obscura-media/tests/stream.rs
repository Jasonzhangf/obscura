#![cfg(unix)]
use std::{os::unix::fs::DirBuilderExt, path::PathBuf, process::Stdio, time::Duration};
use obscura_host_protocol::{FrameInfo, FramePacket, PixelFormat, VideoPacket};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::{UnixListener, UnixStream}};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("om-{}", uuid::Uuid::new_v4().simple()));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap(); Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        for name in ["raw", "video"] { std::fs::remove_file(self.0.join(name)).unwrap(); }
        std::fs::remove_dir(&self.0).unwrap();
    }
}
async fn write(socket: &mut UnixStream, packet: &FramePacket, pixels: &[u8]) {
    let mut header = serde_json::to_vec(packet).unwrap(); header.push(b'\n');
    socket.write_all(&header).await.unwrap(); socket.write_all(pixels).await.unwrap();
}
async fn read(socket: &mut BufReader<UnixStream>) -> (VideoPacket, Vec<u8>) {
    let mut header = Vec::new(); socket.read_until(b'\n', &mut header).await.unwrap();
    let packet: VideoPacket = serde_json::from_slice(&header).unwrap();
    let size = match &packet { VideoPacket::AccessUnit { byte_length, .. } => *byte_length, _ => 0 };
    assert!(size <= obscura_media::MAX_ACCESS_UNIT as u64);
    let mut pixels = vec![0; size as usize]; socket.read_exact(&mut pixels).await.unwrap();
    (packet, pixels)
}

async fn exercise(missing_encoder: bool) {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw")).arg("--video-socket").arg(dir.0.join("video"));
    if missing_encoder { command.args(["--ffmpeg", "/nonexistent-obscura-encoder"]); }
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);
    write(&mut source, &FramePacket::Waiting { session_id: "live-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Waiting { .. }));
    let mut encoder = None;
    let mut timestamp = None;
    for (sequence, width, height) in [(1, 64, 48), (3, 65, 47)] {
        let pixels = [0, 128, 0, 255].repeat(width * height);
        let info = FrameInfo { session_id: "live-session".into(), sequence, document_revision: 7,
            viewport_revision: sequence, width: width as u32, height: height as u32, stride: width as u32 * 4,
            byte_length: pixels.len() as u64, pixel_format: PixelFormat::PremultipliedRgba8 };
        write(&mut source, &FramePacket::Frame { info }, &pixels).await;
        let (packet, bytes) = read(&mut receiver).await;
        if missing_encoder {
            assert!(matches!(packet, VideoPacket::EncoderUnavailable { .. }));
            assert!(!child.wait().await.unwrap().success()); return;
        }
        let VideoPacket::AccessUnit { source, encoder_id, pts_us, coded_width, coded_height, keyframe, .. } = packet else { panic!("expected access unit"); };
        assert_eq!(source.sequence, sequence); assert_eq!(source.document_revision, 7);
        assert_eq!(source.viewport_revision, sequence);
        assert_eq!((source.width, source.height), (width as u32, height as u32));
        assert_eq!((coded_width, coded_height), (((width + 1) & !1) as u32, ((height + 1) & !1) as u32));
        assert!(keyframe && !bytes.is_empty());
        if let Some(previous) = encoder { assert_eq!(previous, encoder_id); }
        if let Some(previous) = timestamp { assert!(pts_us > previous); }
        encoder = Some(encoder_id); timestamp = Some(pts_us);
    }
    write(&mut source, &FramePacket::Closed { session_id: "live-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Closed { .. }));
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn public_adapter_rejects_session_switch_and_exits() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw")).arg("--video-socket").arg(dir.0.join("video"));
    let child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);
    write(&mut source, &FramePacket::Waiting { session_id: "first-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Waiting { session_id } if session_id == "first-session"));
    write(&mut source, &FramePacket::Unavailable {
        session_id: "second-session".into(), message: "unexpected session switch".into(),
    }, &[]).await;
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.unwrap().unwrap();
    assert!(!output.status.success(), "session switch must fail the adapter process");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Host media session changed on live connection"));
}

#[tokio::test]
async fn public_adapter_stops_after_first_closed_packet() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw")).arg("--video-socket").arg(dir.0.join("video"));
    let child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);
    write(&mut source, &FramePacket::Waiting { session_id: "terminal-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Waiting { session_id } if session_id == "terminal-session"));
    write(&mut source, &FramePacket::Closed { session_id: "terminal-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Closed { session_id } if session_id == "terminal-session"));
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.unwrap().unwrap();
    assert!(output.status.success(), "a valid Closed packet must end the adapter successfully");
    let mut trailing = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(1), receiver.read_until(b'\n', &mut trailing)).await.unwrap().unwrap();
    assert_eq!(read, 0, "the adapter must not emit a second packet after Closed");
}

#[tokio::test]
async fn public_adapter_preserves_host_identity_through_resize_and_close() {
    tokio::time::timeout(Duration::from_secs(10), exercise(false)).await.unwrap();
}
#[tokio::test]
async fn public_adapter_reports_encoder_failure_and_exits() {
    tokio::time::timeout(Duration::from_secs(10), exercise(true)).await.unwrap();
}

#[tokio::test]
async fn public_adapter_recovers_after_host_unavailable() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw")).arg("--video-socket").arg(dir.0.join("video"));
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);
    write(&mut source, &FramePacket::Waiting { session_id: "recoverable-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Waiting { .. }));
    write(&mut source, &FramePacket::Unavailable {
        session_id: "recoverable-session".into(), message: "capture temporarily unavailable".into(),
    }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Unavailable { .. }));
    let pixels = [0, 128, 0, 255].repeat(64 * 48);
    let info = FrameInfo { session_id: "recoverable-session".into(), sequence: 1, document_revision: 1,
        viewport_revision: 1, width: 64, height: 48, stride: 64 * 4,
        byte_length: pixels.len() as u64, pixel_format: PixelFormat::PremultipliedRgba8 };
    write(&mut source, &FramePacket::Frame { info }, &pixels).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::AccessUnit { .. }));
    write(&mut source, &FramePacket::Closed { session_id: "recoverable-session".into() }, &[]).await;
    assert!(matches!(read(&mut receiver).await.0, VideoPacket::Closed { .. }));
    assert!(child.wait().await.unwrap().success());
}
