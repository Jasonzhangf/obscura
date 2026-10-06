use obscura_media::encode;

use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use obscura_media::encoder_owner::EncoderChildOwner;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[tokio::test]
async fn h264_is_independently_decodable_with_odd_viewport() {
    let pixels = [255, 0, 0, 255].repeat(65 * 47);
    let encoded = encode(std::path::Path::new("ffmpeg"), 65, 47, pixels).await.unwrap();
    let mut decoder = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    use tokio::io::AsyncWriteExt;
    let mut input = decoder.stdin.take().unwrap();
    input.write_all(&encoded).await.unwrap(); drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), decoder.wait_with_output()).await.unwrap().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout.len(), 66 * 48 * 3);
    let center = &output.stdout[(20 * 66 + 20) * 3..][..3];
    assert!(center[0] > 220 && center[1] < 30 && center[2] < 30, "{center:?}");
}

#[tokio::test]
async fn invalid_pixels_and_missing_encoder_fail_explicitly() {
    assert!(encode(std::path::Path::new("ffmpeg"), 0, 1, vec![]).await.is_err());
    assert!(encode(std::path::Path::new("ffmpeg"), 2, 2, vec![0; 12]).await.is_err());
    assert!(encode(std::path::Path::new("/nonexistent-obscura-encoder"), 2, 2, vec![0; 16]).await.is_err());
}

/// One private test root that owns the encoder-control socket path below the
/// macOS `SUN_LEN` bound and removes it on drop.
struct OwnerRoot(PathBuf);

impl OwnerRoot {
    fn new(name: &str) -> Self {
        let dir = std::path::Path::new("/tmp").join(format!("oe-{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&dir).expect("create owner test root");
        Self(dir)
    }

    fn control_path(&self) -> PathBuf { self.0.join("encoder-control.sock") }
}

impl Drop for OwnerRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How the real control peer answers the typed encoder-control protocol. It is
/// a real Unix-domain peer for the declared control surface, never a mock of
/// the adapter, FFmpeg or the endpoint.
#[derive(Clone, Copy)]
enum ControlScript {
    AcceptAll,
    RefuseRegistration,
    RefuseAcquisition,
}

/// Accept one real `EncoderChildOwner` connection and answer every typed
/// request while recording it for the test to inspect.
async fn serve_control(
    listener: UnixListener,
    script: ControlScript,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    bound_pgid: Arc<Mutex<Option<i32>>>,
) {
    let Ok((stream, _)) = listener.accept().await else { return; };
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut line = String::new();
        let count = match reader.read_line(&mut line).await { Ok(count) => count, Err(_) => return };
        if count == 0 { return; }
        let request: serde_json::Value = match serde_json::from_str(line.trim_end()) { Ok(value) => value, Err(_) => return };
        requests.lock().expect("requests owner").push(request.clone());
        let response = match request["op"].as_str().unwrap_or("") {
            "acquire" => match script {
                ControlScript::RefuseAcquisition => serde_json::json!({"op":"acquire","permit":"admission_finalized"}),
                _ => serde_json::json!({"op":"acquire","permit":"granted"}),
            },
            "register" => {
                if let Some(pgid) = request["pgid"].as_i64() { *bound_pgid.lock().expect("pgid owner") = Some(pgid as i32); }
                let refused = matches!(script, ControlScript::RefuseRegistration);
                serde_json::json!({"op":"register","refused":refused})
            }
            "revoke" => serde_json::json!({"op":"revoke","refused":false}),
            "settle" => serde_json::json!({"op":"settle"}),
            _ => return,
        };
        let mut bytes = serde_json::to_vec(&response).expect("serialize control response");
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() { return; }
    }
}

fn write_wrapper(root: &OwnerRoot, name: &str, body: &str) -> PathBuf {
    let path = root.0.join(name);
    std::fs::write(&path, body).expect("write wrapper encoder");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("wrapper mode");
    path
}

/// A real wrapper encoder that drains stdin, emits one access unit and exits.
const WRAPPER_NORMAL: &str = "#!/bin/sh\ncat >/dev/null\nprintf 'AU'\n";
/// A real wrapper encoder that spawns a long-lived grandchild in its own
/// process group and then blocks without reading stdin.
const WRAPPER_BLOCKING: &str = "#!/bin/sh\nsleep 300 &\nsleep 300\n";
/// A real wrapper encoder that exits while a descendant inherits stdin/stdout/
/// stderr, so the leader is gone but the process group still holds the pipes.
const WRAPPER_WRAPPER_EXIT: &str = "#!/bin/sh\nsleep 300 &\nexit 0\n";

fn owner_ops(requests: &Arc<Mutex<Vec<serde_json::Value>>>) -> Vec<String> {
    requests.lock().expect("requests owner")
        .iter().map(|value| value["op"].as_str().unwrap_or("").to_string()).collect()
}

fn owner_settlement(requests: &Arc<Mutex<Vec<serde_json::Value>>>) -> serde_json::Value {
    requests.lock().expect("requests owner").iter()
        .find(|value| value["op"] == "settle")
        .map(|value| value["outcome"].clone())
        .expect("adapter must report a typed PermitSettled")
}

fn group_alive(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

async fn wait_group_gone(pgid: i32) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !group_alive(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn solid_rgba(width: u32, height: u32) -> Vec<u8> {
    vec![255u8; (width as usize) * (height as usize) * 4]
}

/// A healthy encode revokes the registration, receives `Revoked`, reaps the
/// FFmpeg leader with the same retained owner, and leaves no live group.
#[tokio::test]
async fn encoder_owner_normal_exit_is_reaped() {
    let root = OwnerRoot::new("normal");
    let control = root.control_path();
    let listener = UnixListener::bind(&control).expect("bind control peer");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bound = Arc::new(Mutex::new(None));
    tokio::spawn(serve_control(listener, ControlScript::AcceptAll, Arc::clone(&requests), Arc::clone(&bound)));
    let wrapper = write_wrapper(&root, "normal.sh", WRAPPER_NORMAL);
    let mut owner = EncoderChildOwner::open(&wrapper, &control).await.expect("open owner");
    let bytes = owner.encode_frame(160, 120, solid_rgba(160, 120)).await.expect("healthy encode");
    assert_eq!(bytes, b"AU");
    assert!(!owner.has_live_child(), "normal completion must reap the leader");
    let pgid = bound.lock().expect("pgid owner").expect("register bound a pgid");
    assert!(wait_group_gone(pgid).await, "no encoder group may survive normal completion");
    assert_eq!(owner_ops(&requests), vec!["acquire", "register", "revoke"], "normal completion must revoke before reap");
}

/// The whole-encode window covers a wrapper that never reads stdin: the
/// five-second deadline fires, the borrow drops, and the same retained owner
/// finalizes the wrapper group and its grandchild.
#[tokio::test]
async fn encoder_owner_timeout_sigkills_process_group() {
    let root = OwnerRoot::new("timeout");
    let control = root.control_path();
    let listener = UnixListener::bind(&control).expect("bind control peer");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bound = Arc::new(Mutex::new(None));
    tokio::spawn(serve_control(listener, ControlScript::AcceptAll, Arc::clone(&requests), Arc::clone(&bound)));
    let wrapper = write_wrapper(&root, "blocking.sh", WRAPPER_BLOCKING);
    let mut owner = EncoderChildOwner::open(&wrapper, &control).await.expect("open owner");
    let started = std::time::Instant::now();
    let error = owner.encode_frame(160, 120, solid_rgba(160, 120)).await.expect_err("non-reading wrapper must time out");
    assert!(error.to_string().contains("five second deadline"), "unexpected timeout error: {error}");
    assert!(started.elapsed() >= Duration::from_secs(4), "whole-encode timeout fired too early");
    let pgid = bound.lock().expect("pgid owner").expect("register bound a pgid");
    assert!(wait_group_gone(pgid).await, "whole-encode timeout must finalize the retained subtree");
    assert!(!owner.has_live_child(), "no live child may remain after timeout finalization");
}

/// B3: `finalize` must settle the whole process group, not just the leader. A
/// wrapper that exits while a descendant still holds the encoder stdout makes
/// `run_encode` block on pipe EOF until its deadline. The leader exit must not
/// short-circuit finalization: the descendant group is stopped, and then the
/// leader is reaped and ownership released.
#[tokio::test]
async fn encoder_owner_finalize_settles_descendant_of_exited_wrapper() {
    let root = OwnerRoot::new("wrapper-exit");
    let control = root.control_path();
    let listener = UnixListener::bind(&control).expect("bind control peer");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bound = Arc::new(Mutex::new(None));
    tokio::spawn(serve_control(listener, ControlScript::AcceptAll, Arc::clone(&requests), Arc::clone(&bound)));
    let wrapper = write_wrapper(&root, "wrapper-exit.sh", WRAPPER_WRAPPER_EXIT);
    let mut owner = EncoderChildOwner::open(&wrapper, &control).await.expect("open owner");
    owner.encode_frame(16, 16, solid_rgba(16, 16)).await
        .expect_err("an exited wrapper that leaks stdout must hit the deadline, not succeed");
    let pgid = bound.lock().expect("pgid owner").expect("register bound a pgid");
    let leader_reaped = !owner.has_live_child();
    let group_gone = wait_group_gone(pgid).await;
    if !group_gone {
        // Keep a failing pre-fix run from leaking the test wrapper descendant.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    assert!(leader_reaped, "finalize must reap the leader after settling the group");
    assert!(group_gone, "finalize must settle the descendant group of an exited wrapper");
}

/// A refused acquisition settles the permit with `RefusedNoChild` and spawns no
/// FFmpeg child at all.
#[tokio::test]
async fn encoder_owner_refused_acquisition_settles_permit() {
    let root = OwnerRoot::new("refused-acq");
    let control = root.control_path();
    let listener = UnixListener::bind(&control).expect("bind control peer");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bound = Arc::new(Mutex::new(None));
    tokio::spawn(serve_control(listener, ControlScript::RefuseAcquisition, Arc::clone(&requests), Arc::clone(&bound)));
    let wrapper = write_wrapper(&root, "blocking.sh", WRAPPER_BLOCKING);
    let mut owner = EncoderChildOwner::open(&wrapper, &control).await.expect("open owner");
    let error = owner.encode_frame(160, 120, solid_rgba(160, 120)).await.expect_err("finalized admission must be refused");
    assert!(error.to_string().contains("ENCODER_ADMISSION_FINALIZED"), "unexpected refusal error: {error}");
    assert_eq!(owner_ops(&requests), vec!["acquire", "settle"], "refused acquisition must settle the permit and nothing else");
    assert_eq!(owner_settlement(&requests), serde_json::json!("refused_no_child"), "refused acquisition must report no child");
    assert!(bound.lock().expect("pgid owner").is_none(), "refused acquisition must spawn no child");
}

/// BB-EX-MEDIA-36 media side: a registration refused before any `Armed` state
/// runs the adapter's local cleanup, reaps its own wrapper group without any
/// `RevokeEncoder`/`Revoked`, and reports `PermitSettled { GroupStopped }`.
#[tokio::test]
async fn encoder_owner_refused_registration_reaps_without_revoked() {
    let root = OwnerRoot::new("refused-reg");
    let control = root.control_path();
    let listener = UnixListener::bind(&control).expect("bind control peer");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bound = Arc::new(Mutex::new(None));
    tokio::spawn(serve_control(listener, ControlScript::RefuseRegistration, Arc::clone(&requests), Arc::clone(&bound)));
    let wrapper = write_wrapper(&root, "blocking.sh", WRAPPER_BLOCKING);
    let mut owner = EncoderChildOwner::open(&wrapper, &control).await.expect("open owner");
    let error = owner.encode_frame(160, 120, solid_rgba(160, 120)).await.expect_err("refused registration must fail");
    assert!(error.to_string().contains("ENCODER_ADMISSION_FINALIZED"), "unexpected refusal error: {error}");
    let pgid = bound.lock().expect("pgid owner").expect("register bound a pgid");
    assert!(wait_group_gone(pgid).await, "refused registration must reap its own wrapper group");
    assert!(!owner.has_live_child(), "refused registration must leave no live child");
    let ops = owner_ops(&requests);
    assert!(!ops.contains(&"revoke".to_string()), "refused registration must not send RevokeEncoder: {ops:?}");
    assert_eq!(owner_settlement(&requests), serde_json::json!("refused_group_stopped"), "refusal must settle with GroupStopped");
}
