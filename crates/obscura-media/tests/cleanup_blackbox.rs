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

/// Real control peer that grants acquisition, commits registration, then closes
/// the stream without answering `Revoke`. The adapter therefore reaches an
/// explicit unresolved revoke terminal.
async fn serve_control_register_then_close(listener: UnixListener) {
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
            EncoderControlRequest::Revoke { .. } => return,
            EncoderControlRequest::Settle { .. } => EncoderControlResponse::Settle,
        };
        let mut bytes = serde_json::to_vec(&response).unwrap();
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() { return; }
        if matches!(request, EncoderControlRequest::Register { .. }) { return; }
    }
}

/// Write a wrapper encoder whose leader exits on its own while a same-group
/// descendant records whether the leader is still present. When the leader is
/// reaped before acknowledgement, `leader_gone` is written while the
/// descendant is still alive.
fn write_leader_retention_probe(root: &Path, name: &str, marker: &Path) -> PathBuf {
    let observer = root.join(format!("{name}-observer.sh"));
    let ready = root.join(format!("{name}.ready"));
    write_executable(&observer, &format!(
        "#!/bin/sh\nleader=\"$1\"\nmarker=\"$2\"\nready=\"$3\"\n: > \"$ready\"\necho leader_alive > \"$marker\"\nwhile kill -0 \"$leader\" 2>/dev/null; do sleep 0.01; done\necho leader_gone > \"$marker\"\nexit 0\n"
    ));
    let wrapper = root.join(format!("{name}.sh"));
    write_executable(&wrapper, &format!(
        "#!/bin/sh\nleader=$$\nsh '{observer}' \"$leader\" '{marker}' '{ready}' </dev/null >/dev/null 2>&1 &\nwhile [ ! -f '{ready}' ]; do sleep 0.02; done\nexec 1>&- 2>&-\nwhile :; do sleep 0.1; done\n",
        observer = observer.display(),
        marker = marker.display(),
        ready = ready.display(),
    ));
    wrapper
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

/// Write a wrapper encoder that exits with `exit_code` and emits no access unit,
/// plus a same-process-group descendant that records whether the wrapper leader
/// is still present when the group is signalled. The leader closes its
/// stdout/stderr so the adapter's bounded reads complete while the leader is
/// still alive, then exits on its own after a bounded delay. A finalizer that
/// reaps the leader before it signals the group leaves `gone` in the marker; a
/// finalizer that signals while the leader identity is still pinned leaves
/// `present`.
fn write_post_reap_probe(root: &Path, name: &str, exit_code: u32, marker: &Path) -> PathBuf {
    let observer = root.join(format!("{name}-observer.sh"));
    let ready = root.join(format!("{name}.ready"));
    write_executable(&observer, &format!(
        "#!/bin/sh\nleader=\"$1\"\nmarker=\"$2\"\nready=\"$3\"\ntrap 'if kill -0 \"$leader\" 2>/dev/null; then echo present > \"$marker\"; else echo gone > \"$marker\"; fi; exit 0' TERM\n: > \"$ready\"\nwhile :; do sleep 0.1; done\n"
    ));
    let wrapper = root.join(format!("{name}.sh"));
    write_executable(&wrapper, &format!(
        "#!/bin/sh\nleader=$$\nsh '{observer}' \"$leader\" '{marker}' '{ready}' </dev/null >/dev/null 2>&1 &\nwhile [ ! -f '{ready}' ]; do sleep 0.02; done\nexec 1>&- 2>&-\ntrap 'sleep 0.3; exit {exit_code}' TERM\nsleep 0.5\nexit {exit_code}\n",
        observer = observer.display(),
        marker = marker.display(),
        ready = ready.display(),
    ));
    wrapper
}

/// Drive one real standalone adapter invocation with `wrapper` as `--ffmpeg`,
/// send a single raw frame, and return the emitted video packet plus the
/// adapter's exit status.
async fn run_standalone_failure(dir: &Directory, wrapper: &Path) -> (VideoPacket, std::process::ExitStatus) {
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let control = UnixListener::bind(dir.0.join("encoder-control.sock")).unwrap();
    std::fs::set_permissions(dir.0.join("encoder-control.sock"), std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(tokio::spawn(serve_control(control)));

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw"))
        .arg("--video-socket").arg(dir.0.join("video"))
        .arg("--ffmpeg").arg(wrapper);
    let child = command.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let (mut source, _) = raw.accept().await.unwrap();
    let mut receiver = BufReader::new(video.accept().await.unwrap().0);

    let (packet, pixels) = frame(64, 48);
    write_frame(&mut source, &packet, &pixels).await;
    let (reply, _) = tokio::time::timeout(Duration::from_secs(15), read_video(&mut receiver)).await
        .expect("the adapter never emitted a packet for the failing frame");
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await
        .expect("adapter did not exit after the encode failure").unwrap();
    (reply, output.status)
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

/// G4: a configured encoder that exits nonzero is already reaped by the time
/// `run_encode` inspects its status. The retained owner must signal and settle
/// the group while the leader identity is still pinned, so no group signal is
/// ever addressed to a reaped (and reusable) pgid. The same-group descendant
/// records whether the leader was still present when the group was signalled.
#[tokio::test]
async fn standalone_adapter_retires_group_authority_before_reaping_nonzero_encoder() {
    let dir = Directory::new();
    let marker = dir.0.join("nonzero.marker");
    let wrapper = write_post_reap_probe(&dir.0, "nonzero", 7, &marker);
    let (reply, status) = run_standalone_failure(&dir, &wrapper).await;
    assert!(matches!(reply, VideoPacket::EncoderUnavailable { .. }),
        "a nonzero encoder exit must emit EncoderUnavailable, got {reply:?}");
    assert!(!status.success(), "a nonzero encoder exit must fail the adapter process");
    let observed = std::fs::read_to_string(&marker)
        .expect("the encoder descendant must record the group-signal ordering");
    assert_eq!(observed.trim(), "present",
        "G4: the encoder leader identity must still be pinned when the group is signalled, not reaped first");
}

/// G4: the empty-output branch has the same requirement. A successful encoder
/// that produces no access unit is already reaped, so its retained pgid must
/// not be signalled after the reap.
#[tokio::test]
async fn standalone_adapter_retires_group_authority_before_reaping_empty_encoder_output() {
    let dir = Directory::new();
    let marker = dir.0.join("empty.marker");
    let wrapper = write_post_reap_probe(&dir.0, "empty", 0, &marker);
    let (reply, status) = run_standalone_failure(&dir, &wrapper).await;
    assert!(matches!(reply, VideoPacket::EncoderUnavailable { .. }),
        "empty encoder output must emit EncoderUnavailable, got {reply:?}");
    assert!(!status.success(), "empty encoder output must fail the adapter process");
    let observed = std::fs::read_to_string(&marker)
        .expect("the encoder descendant must record the group-signal ordering");
    assert_eq!(observed.trim(), "present",
        "G4: the encoder leader identity must still be pinned when the group is signalled, not reaped first");
}

/// G5: the standalone adapter must honour SIGTERM while it is idle, waiting on a
/// raw frame header. Before the fix SIGTERM was queued until the next encode, so
/// the process ignored it indefinitely and retained its control socket. The
/// adapter must exit within a bounded window and remove the socket path.
#[tokio::test]
async fn standalone_adapter_exits_on_sigterm_while_waiting_for_frames() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-media"));
    command.arg("--frames-socket").arg(dir.0.join("raw"))
        .arg("--video-socket").arg(dir.0.join("video"));
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let (_source, _) = raw.accept().await.unwrap();
    let (_receiver, _) = video.accept().await.unwrap();
    // The standalone adapter binds its own control socket in the consumer
    // directory; wait until it exists before signalling.
    let control = dir.0.join("encoder-control.sock");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !control.exists() {
        assert!(tokio::time::Instant::now() < deadline, "standalone control socket was never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = child.id().expect("standalone adapter pid") as i32;
    // Safety: signaling the one adapter process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "SIGTERM the standalone adapter");
    // The raw source stays idle: no frame is ever sent. The adapter must still
    // observe the lifetime signal and exit within its bounded window.
    let exited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let socket_removed = !control.exists();
    if exited.is_err() {
        // Keep a pre-fix run from leaking the still-running fixture adapter.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    assert!(exited.is_ok(),
        "G5: the idle standalone adapter ignored SIGTERM and did not exit within its bounded window");
    assert!(socket_removed,
        "G5: the interrupted standalone adapter must remove its encoder-control socket path");
}

/// Write a wrapper encoder whose leader exits on its own while a same-group
/// descendant ignores SIGTERM and polls the leader's liveness. The descendant
/// writes `reaped` as soon as `kill -0 <leader>` fails while the descendant is
/// still alive, i.e. exactly when the leader was reaped before the group was
/// fully settled.
fn write_reap_ordering_probe(root: &Path, name: &str, marker: &Path) -> PathBuf {
    let observer = root.join(format!("{name}-observer.sh"));
    let ready = root.join(format!("{name}.ready"));
    write_executable(&observer, &format!(
        "#!/bin/sh\n\
         trap '' TERM\n\
         leader=\"$1\"\n\
         marker=\"$2\"\n\
         ready=\"$3\"\n\
         : > \"$ready\"\n\
         while :; do\n\
           if ! kill -0 \"$leader\" 2>/dev/null; then echo reaped > \"$marker\"; exit 0; fi\n\
           sleep 0.01\n\
         done\n"
    ));
    let wrapper = root.join(format!("{name}.sh"));
    write_executable(&wrapper, &format!(
        "#!/bin/sh\n\
         leader=$$\n\
         sh '{observer}' \"$leader\" '{marker}' '{ready}' </dev/null >/dev/null 2>&1 &\n\
         while [ ! -f '{ready}' ]; do sleep 0.02; done\n\
         exec 1>&- 2>&-\n\
         while :; do sleep 0.1; done\n",
        observer = observer.display(),
        marker = marker.display(),
        ready = ready.display(),
    ));
    wrapper
}

/// F3: an encoder leader that exits while a same-group descendant resists
/// cleanup must stay unreaped until every group signal is complete. Pre-fix
/// `stop_kill_reap` reaped the leader within the cooperative grace and then
/// escalated with a SIGKILL addressed to a reaped pgid. The resistant descendant
/// observes that early reap (`kill -0 <leader>` starts failing while the
/// descendant is still alive) and records it; the fixed ordering reaps the
/// leader only after the group is gone, so the descendant is already dead and
/// records nothing.
#[tokio::test]
async fn standalone_adapter_pins_leader_identity_until_group_settled() {
    let dir = Directory::new();
    let marker = dir.0.join("reap-ordering.marker");
    let wrapper = write_reap_ordering_probe(&dir.0, "reaporder", &marker);
    let (reply, status) = run_standalone_failure(&dir, &wrapper).await;
    assert!(matches!(reply, VideoPacket::EncoderUnavailable { .. }),
        "a leader that exits without output must emit EncoderUnavailable, got {reply:?}");
    assert!(!status.success(), "a leader that exits without output must fail the adapter process");
    let early_reap = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(early_reap.trim().is_empty(),
        "F3: the encoder leader was reaped while a same-group descendant was still alive, so the escalation signalled a reaped identity");
}

/// F4: when the endpoint commits a registration but the revoke acknowledgement
/// is never delivered, the standalone adapter must return without dropping its
/// retained `Child`. Dropping that owner would SIGKILL and reap the leader
/// without the required acknowledgement. The same-group observer records
/// whether the leader was reaped while it was still present.
#[tokio::test]
async fn standalone_adapter_preserves_leader_when_revoke_acknowledgment_is_unavailable() {
    let dir = Directory::new();
    let raw = UnixListener::bind(dir.0.join("raw")).unwrap();
    let video = UnixListener::bind(dir.0.join("video")).unwrap();
    let control = UnixListener::bind(dir.0.join("encoder-control.sock")).unwrap();
    std::fs::set_permissions(dir.0.join("encoder-control.sock"), std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(tokio::spawn(serve_control_register_then_close(control)));

    let marker = dir.0.join("leader-retention.marker");
    let pgid_file = dir.0.join("leader.pgid");
    let wrapper = write_leader_retention_probe(&dir.0, "leaderretention", &marker);
    let wrapper = {
        let wrapper = wrapper.display().to_string();
        let pgid = pgid_file.display().to_string();
        let wrapper_text = format!(
            "#!/bin/sh\necho \"$$\" > {pgid}\nexec {wrapper}\n",
        );
        let wrapper_path = dir.0.join("leaderretention-pgid.sh");
        write_executable(&wrapper_path, &wrapper_text);
        wrapper_path
    };

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
        .expect("the adapter never emitted a packet for the unresolved revoke");
    assert!(matches!(reply, VideoPacket::EncoderUnavailable { .. }),
        "unresolved revoke must emit EncoderUnavailable, got {reply:?}");
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await
        .expect("adapter did not exit after the unresolved revoke").unwrap();
    assert!(!output.status.success(),
        "unresolved revoke must fail the adapter process: {}", String::from_utf8_lossy(&output.stderr));

    let pgid: i32 = std::fs::read_to_string(&pgid_file).unwrap().trim().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let observed = std::fs::read_to_string(&marker).expect("the encoder descendant must record leader retention");
    assert_eq!(observed.trim(), "leader_alive",
        "F4: the adapter silently killed and reaped the leader without the revoke acknowledgement");
    assert!(group_alive(pgid),
        "F4: the unresolved revoke exit left no encoder group for endpoint authority");
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    assert!(wait_group_gone(pgid, Duration::from_secs(5)).await,
        "F4: the test fixture encoder group survived explicit cleanup");
}
