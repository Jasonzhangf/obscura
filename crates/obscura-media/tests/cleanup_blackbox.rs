#![cfg(unix)]
//! Black-box regressions for the standalone adapter's public entrypoint.
//!
//! F2: every terminal encode failure must settle the retained encoder process
//! group before the adapter exits, so a configured `--ffmpeg` wrapper that
//! spawns a descendant cannot leak that subtree.
//! F4: a standalone adapter must release the control socket it creates, so its
//! public entrypoint can be used again against the same consumer directory.

use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use obscura_media::encoder_owner::{EncoderControlRequest, EncoderControlResponse, EncoderPermit};
use obscura_host_protocol::{FrameInfo, FramePacket, PixelFormat, VideoPacket};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::path::Path::new("/tmp").join(format!("oc-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A real Unix control peer that answers the typed register/revoke protocol
/// exactly as the endpoint control surface does.
async fn serve_control(listener: UnixListener) {
    let Ok((stream, _)) = listener.accept().await else { return; };
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        let count = match reader.read_line(&mut line).await { Ok(count) => count, Err(_) => return };
        if count == 0 { return; }
        let request: EncoderControlRequest = match serde_json::from_str(line.trim_end()) { Ok(request) => request, Err(_) => return };
        let response = match request {
            EncoderControlRequest::Acquire { .. } => EncoderControlResponse::Acquire { permit: EncoderPermit::Granted },
            EncoderControlRequest::Register { .. } => EncoderControlResponse::Register { refused: false },
            EncoderControlRequest::Revoke { .. } => EncoderControlResponse::Revoke { refused: false },
            EncoderControlRequest::Settle { .. } => EncoderControlResponse::Settle,
        };
        let mut bytes = serde_json::to_vec(&response).unwrap();
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() { return; }
    }
}

async fn write_frame(socket: &mut UnixStream, packet: &FramePacket, pixels: &[u8]) {
    let mut header = serde_json::to_vec(packet).unwrap();
    header.push(b'\n');
    socket.write_all(&header).await.unwrap();
    socket.write_all(pixels).await.unwrap();
}

async fn read_video(socket: &mut BufReader<UnixStream>) -> (VideoPacket, Vec<u8>) {
    let mut header = Vec::new();
    socket.read_until(b'\n', &mut header).await.unwrap();
    let packet: VideoPacket = serde_json::from_slice(&header).unwrap();
    let size = match &packet { VideoPacket::AccessUnit { byte_length, .. } => *byte_length, _ => 0 };
    let mut pixels = vec![0; size as usize];
    socket.read_exact(&mut pixels).await.unwrap();
    (packet, pixels)
}

const FRAME_SESSION: &str = "cleanup-session";

fn frame(width: u32, height: u32) -> (FramePacket, Vec<u8>) {
    let pixels = [0, 128, 0, 255].repeat((width * height) as usize);
    let info = FrameInfo {
        session_id: FRAME_SESSION.into(),
        tab_id: "cleanup-tab".into(),
        sequence: 1,
        presentation_revision: 1,
        document_revision: 1,
        viewport_revision: 1,
        width,
        height,
        stride: width * 4,
        byte_length: pixels.len() as u64,
        pixel_format: PixelFormat::PremultipliedRgba8,
    };
    (FramePacket::Frame { info }, pixels)
}

fn group_alive(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

async fn wait_group_gone(pgid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !group_alive(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// F2: a real standalone adapter whose configured `--ffmpeg` wrapper spawns a
/// descendant and then floods stdout past the bounded access-unit limit must
/// report the failure and leave no encoder subtree behind. The descendant is in
/// the wrapper's process group, so only a group settle reaches it.
#[tokio::test]
async fn standalone_adapter_settles_encoder_group_on_bounded_output_failure() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let control = UnixListener::bind(dir.0.join("encoder-control.sock")).unwrap();
    std::fs::set_permissions(dir.0.join("encoder-control.sock"), std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(tokio::spawn(serve_control(control)));

    let pgid_file = dir.0.join("wrapper.pgid");
    let wrapper = dir.0.join("wrapper.sh");
    write_executable(&wrapper, &format!(
        "#!/bin/sh\necho \"$$\" > {pgid}\nsleep 300 &\ndd if=/dev/zero bs=1024 count=6000 2>/dev/null\nexit 0\n",
        pgid = pgid_file.display()
    ));

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw"))
        .arg("--video-socket").arg(dir.0.join("video"))
        .arg("--ffmpeg").arg(&wrapper);
    let child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);

    let (packet, pixels) = frame(64, 48);
    write_frame(&mut source, &packet, &pixels).await;
    let (reply, _) = tokio::time::timeout(Duration::from_secs(15), read_video(&mut receiver)).await
        .expect("the adapter never emitted a packet for the failing frame");
    assert!(matches!(reply, VideoPacket::EncoderUnavailable { .. }),
        "bounded-output failure must emit EncoderUnavailable, got {reply:?}");

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await
        .expect("adapter did not exit after the encode failure").unwrap();
    assert!(!output.status.success(), "a terminal encode failure must fail the adapter process");

    let pgid: i32 = std::fs::read_to_string(&pgid_file).unwrap().trim().parse().unwrap();
    let group_gone = wait_group_gone(pgid, Duration::from_secs(5)).await;
    if !group_gone {
        // Keep a failing pre-fix run from leaking the wrapper descendant.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    assert!(group_gone, "the adapter left the configured encoder process group alive after a terminal failure");
}

/// F4: two consecutive real standalone adapter invocations against the same
/// consumer directory must both succeed. The adapter owns and removes the
/// control socket it binds, so the second invocation is not blocked by a stale
/// path. No control listener is pre-bound here.
#[tokio::test]
async fn standalone_adapter_releases_its_control_socket_for_reuse() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();

    for attempt in ["first", "second"] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
        command.arg("--frames-socket").arg(dir.0.join("raw"))
            .arg("--video-socket").arg(dir.0.join("video"));
        let child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let (mut source, _) = raw.accept().await.unwrap();
        let mut receiver = BufReader::new(video.accept().await.unwrap().0);
        write_frame(&mut source, &FramePacket::Waiting { session_id: "reuse-session".into() }, &[]).await;
        assert!(matches!(read_video(&mut receiver).await.0, VideoPacket::Waiting { .. }),
            "the {attempt} standalone invocation did not forward Waiting");
        write_frame(&mut source, &FramePacket::Closed { session_id: "reuse-session".into() }, &[]).await;
        assert!(matches!(read_video(&mut receiver).await.0, VideoPacket::Closed { .. }),
            "the {attempt} standalone invocation did not forward Closed");
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await
            .unwrap_or_else(|_| panic!("the {attempt} standalone invocation did not exit")).unwrap();
        assert!(output.status.success(),
            "the {attempt} standalone invocation failed: {}",
            String::from_utf8_lossy(&output.stderr));
        // The invocation must release the control socket it bound. A leftover
        // path is exactly what blocks the next invocation in the same consumer
        // directory.
        assert!(!dir.0.join("encoder-control.sock").exists(),
            "the {attempt} standalone invocation left its control socket path behind");
    }
}
