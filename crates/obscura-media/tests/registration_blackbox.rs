#![cfg(unix)]
//! Black-box regressions for the standalone adapter's registration and revoke
//! acknowledgement handling.
//!
//! G2: a registration whose `Register` was sent but whose acknowledgement has
//! not been consumed yet is not an unregistered group. When SIGTERM wins the
//! adapter's lifetime select in that window, the retained owner must resolve the
//! outstanding registration through the typed control resource and revoke before
//! it reaps.
//!
//! G3: a registered leader must not be reaped until the typed non-refused revoke
//! acknowledgement arrives. A healthy endpoint holds signal authority for up to
//! two seconds of SIGTERM grace plus two seconds after SIGKILL, so an
//! acknowledgement that lags the previous two-second exchange deadline is not
//! permission to address the group.

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
        let path = std::path::Path::new("/tmp").join(format!("oreg-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The endpoint commits `Armed` before it writes the `Registered` answer. This
/// peer stores the registration, signals the test, then holds the typed answer
/// until the test releases it.
async fn serve_control_pending_register(
    listener: UnixListener,
    register_seen: tokio::sync::oneshot::Sender<i32>,
    mut ack_gate: tokio::sync::oneshot::Receiver<()>,
    revoke_seen: tokio::sync::oneshot::Sender<()>,
) {
    let Ok((stream, _)) = listener.accept().await else { return; };
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut register_seen = Some(register_seen);
    let mut revoke_seen = Some(revoke_seen);
    loop {
        let mut line = String::new();
        let count = match reader.read_line(&mut line).await { Ok(count) => count, Err(_) => return };
        if count == 0 { return; }
        let request: EncoderControlRequest = match serde_json::from_str(line.trim_end()) { Ok(request) => request, Err(_) => return };
        let response = match request {
            EncoderControlRequest::Acquire { .. } => EncoderControlResponse::Acquire { permit: EncoderPermit::Granted },
            EncoderControlRequest::Register { pgid, .. } => {
                if let Some(signal) = register_seen.take() { let _ = signal.send(pgid); }
                let _ = (&mut ack_gate).await;
                EncoderControlResponse::Register { refused: false }
            }
            EncoderControlRequest::Revoke { .. } => {
                if let Some(signal) = revoke_seen.take() { let _ = signal.send(()); }
                EncoderControlResponse::Revoke { refused: false }
            }
            EncoderControlRequest::Settle { .. } => EncoderControlResponse::Settle,
        };
        let mut bytes = serde_json::to_vec(&response).unwrap();
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() { return; }
    }
}

/// A healthy endpoint still owns signal authority over the registered group
/// while it completes its SIGTERM/SIGKILL path, so its `Revoked` answer can lag
/// the adapter's request. This peer answers `Register` immediately, records the
/// `Revoke`, then holds the answer until the test releases it.
async fn serve_control_delayed_revoke(
    listener: UnixListener,
    revoke_seen: tokio::sync::oneshot::Sender<i32>,
    mut ack_gate: tokio::sync::oneshot::Receiver<()>,
) {
    let Ok((stream, _)) = listener.accept().await else { return; };
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut revoke_seen = Some(revoke_seen);
    let mut pgid = None;
    loop {
        let mut line = String::new();
        let count = match reader.read_line(&mut line).await { Ok(count) => count, Err(_) => return };
        if count == 0 { return; }
        let request: EncoderControlRequest = match serde_json::from_str(line.trim_end()) { Ok(request) => request, Err(_) => return };
        let response = match request {
            EncoderControlRequest::Acquire { .. } => EncoderControlResponse::Acquire { permit: EncoderPermit::Granted },
            EncoderControlRequest::Register { pgid: bound, .. } => {
                pgid = Some(bound);
                EncoderControlResponse::Register { refused: false }
            }
            EncoderControlRequest::Revoke { .. } => {
                if let Some(signal) = revoke_seen.take() { let _ = signal.send(pgid.expect("revoke follows register")); }
                let _ = (&mut ack_gate).await;
                EncoderControlResponse::Revoke { refused: false }
            }
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

fn frame(width: u32, height: u32) -> (FramePacket, Vec<u8>) {
    let pixels = [0, 128, 0, 255].repeat((width * height) as usize);
    let info = FrameInfo {
        session_id: "registration-session".into(),
        tab_id: "registration-tab".into(),
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

fn bind_control(dir: &Directory) -> UnixListener {
    let control = UnixListener::bind(dir.0.join("encoder-control.sock")).unwrap();
    std::fs::set_permissions(dir.0.join("encoder-control.sock"), std::fs::Permissions::from_mode(0o600)).unwrap();
    control
}

#[tokio::test]
async fn standalone_adapter_resolves_pending_registration_before_reaping() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let control = bind_control(&dir);
    let (register_tx, register_rx) = tokio::sync::oneshot::channel();
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    let (revoke_tx, revoke_rx) = tokio::sync::oneshot::channel();
    drop(tokio::spawn(serve_control_pending_register(control, register_tx, ack_rx, revoke_tx)));

    let wrapper = dir.0.join("group.sh");
    write_executable(&wrapper, "#!/bin/sh\nsleep 300 &\nsleep 300\n");

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw"))
        .arg("--video-socket").arg(dir.0.join("video"))
        .arg("--ffmpeg").arg(&wrapper);
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let _receiver = video.accept().await.unwrap();

    let (packet, pixels) = frame(64, 48);
    write_frame(&mut source, &packet, &pixels).await;

    let pgid = tokio::time::timeout(Duration::from_secs(15), register_rx).await
        .expect("the adapter never sent RegisterEncoder").expect("register signal");
    assert!(pgid > 0, "RegisterEncoder must carry the spawned group identity");

    let pid = child.id().expect("adapter pid") as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "SIGTERM the standalone adapter");
    // Hold the Register acknowledgement until the adapter has observed the
    // lifetime signal and dropped the in-flight registration. The endpoint
    // committed `Armed` before it answered, so an adapter that treats the unread
    // acknowledgement as unregistered reaps the leader without resolving it.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let _ = ack_tx.send(());

    let saw_revoke = tokio::time::timeout(Duration::from_secs(10), revoke_rx)
        .await
        .ok()
        .and_then(|signal| signal.ok())
        .is_some();
    let exited = tokio::time::timeout(Duration::from_secs(15), child.wait()).await.is_ok();
    if !exited { let _ = child.start_kill(); let _ = child.wait().await; }
    if group_alive(pgid) { unsafe { libc::kill(-pgid, libc::SIGKILL); } }
    let group_gone = wait_group_gone(pgid, Duration::from_secs(5)).await;

    assert!(saw_revoke,
        "G2: the adapter must resolve a pending registration through the typed revoke before reaping, not treat the unread acknowledgement as unregistered");
    assert!(exited, "the interrupted adapter must exit after resolving the pending registration");
    assert!(group_gone, "G2: the retained owner must settle the wrapper group");
}

#[tokio::test]
async fn standalone_adapter_requires_revoke_ack_before_reaping_registered_leader() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let control = bind_control(&dir);
    let (revoke_tx, revoke_rx) = tokio::sync::oneshot::channel();
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    drop(tokio::spawn(serve_control_delayed_revoke(control, revoke_tx, ack_rx)));

    let wrapper = dir.0.join("alive.sh");
    write_executable(&wrapper, "#!/bin/sh\nexec 1>&- 2>&-\ntrap 'exit 0' TERM\nwhile :; do sleep 0.1; done\n");

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw"))
        .arg("--video-socket").arg(dir.0.join("video"))
        .arg("--ffmpeg").arg(&wrapper);
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);

    let (packet, pixels) = frame(64, 48);
    write_frame(&mut source, &packet, &pixels).await;

    let pgid = tokio::time::timeout(Duration::from_secs(15), revoke_rx).await
        .expect("the adapter never sent RevokeEncoder").expect("revoke signal");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let leader_alive = unsafe { libc::kill(pgid, 0) == 0 };
    let _ = ack_tx.send(());

    let terminal = tokio::time::timeout(Duration::from_secs(15), read_video(&mut receiver)).await.ok();
    if let Some((packet, _)) = terminal {
        assert!(matches!(packet, VideoPacket::EncoderUnavailable { .. }), "unexpected terminal {packet:?}");
    }
    let exited = tokio::time::timeout(Duration::from_secs(15), child.wait()).await.is_ok();
    if !exited { let _ = child.start_kill(); let _ = child.wait().await; }
    if group_alive(pgid) { unsafe { libc::kill(-pgid, libc::SIGKILL); } }

    assert!(leader_alive,
        "G3: the adapter must not reap a registered leader before its typed revoke acknowledgement");
    assert!(exited, "the adapter must exit after the revoke acknowledgement");
    assert!(wait_group_gone(pgid, Duration::from_secs(5)).await, "G3: the group must be settled after the acknowledgement");
}
