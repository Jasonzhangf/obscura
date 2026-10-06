#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use serde_json::{json, Value};

struct Daemon { child: Child, dir: std::path::PathBuf }
impl Daemon {
    fn start() -> Self {
        Self::start_with_network(false)
    }
    fn start_with_network(network: bool) -> Self {
        let dir = std::path::PathBuf::from("/tmp").join(format!("oh-{}", uuid::Uuid::new_v4()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_obscura-host"));
        command.arg("--socket-dir").arg(&dir).stdout(Stdio::null());
        if network { command.arg("--allow-private-network"); }
        let child = command.spawn().unwrap();
        let mut daemon = Self { child, dir };
        // Cold executable admission under concurrent full-suite discovery can
        // delay macOS process startup; protocol operations retain short deadlines.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            // The socket inode appears between bind and listen. File existence
            // alone is not listener readiness under concurrent process load.
            match UnixStream::connect(daemon.dir.join("host.sock")) {
                Ok(_) => break,
                Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {},
                Err(error) => panic!("daemon listener startup: {error}"),
            }
            assert!(daemon.child.try_wait().unwrap().is_none(), "daemon exited before listener ready");
            assert!(Instant::now() < deadline, "daemon startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
    fn connect(&self) -> Client {
        let stream = UnixStream::connect(self.dir.join("host.sock")).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut client = Client(BufReader::new(stream));
        assert_eq!(client.read()["type"], "ready"); client
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() { self.child.kill().unwrap(); }
        self.child.wait().unwrap();
        if self.dir.exists() { std::fs::remove_dir_all(&self.dir).unwrap(); }
    }
}
struct Client(BufReader<UnixStream>);
impl Client {
    fn read(&mut self) -> Value {
        let mut line = String::new(); assert!(self.0.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    }
    fn send(&mut self, command: Value) -> Value {
        let mutation_types = [
            "evaluate", "navigate", "back", "forward", "reload",
            "tab_create", "tab_select", "tab_close",
            "profile_create", "profile_select",
            "resize", "close_session", "click", "input_text", "scroll",
        ];
        let operation = if mutation_types.iter().any(|name| command["type"].as_str() == Some(name)) {
            let status = self.ok(json!({"type":"status"}));
            Some(identity(&status))
        } else { None };
        self.raw(json!({"id":1,"command":command,"operation":operation}))
    }
    fn raw(&mut self, request: Value) -> Value {
        writeln!(self.0.get_mut(), "{request}").unwrap(); self.read()
    }
    fn ok(&mut self, command: Value) -> Value {
        let value = self.send(command); assert_eq!(value["type"], "result", "{value}"); value["value"].clone()
    }
    fn eval(&mut self, expression: &str) -> Value {
        self.ok(json!({"type":"evaluate","expression":expression}))["result"]["value"].clone()
    }
}
fn identity(status: &Value) -> Value {
    json!({"session_id":status["session_id"],"attachment_id":status["attachment_id"],"sequence":status["next_sequence"],"control_epoch":status["control"]["epoch"],"viewport_revision":status["viewport_revision"],"document_revision":status["document_revision"]})
}

fn read_complete_http_request(mut stream: &std::net::TcpStream, max_header_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buffered.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(buffered);
        }
        if buffered.len() >= max_header_bytes {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "HTTP request headers exceeded fixture bound"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "HTTP request closed before header end")),
            Ok(read) => buffered.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

struct Media(BufReader<UnixStream>);

fn declaration(device: &str, width: u32, height: u32, orientation: &str) -> Value {
    json!({"device":device,"css_width":width,"css_height":height,"orientation":orientation})
}

fn settled(client: &mut Client) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = client.ok(json!({"type":"status"}));
        if status["viewport_pending"] == false { return status; }
        assert!(Instant::now() < deadline, "viewport did not settle: {status}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn declared_phone_viewport_preserves_page_and_rejects_stale_input() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    let initial = agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<input id='retained'><script>window.marker={value:17};document.getElementById('retained').value='kept';</script>"}));
    let old = agent.ok(json!({"type":"status"}));
    let mut phone = daemon.connect();
    let attached = phone.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    let status = settled(&mut phone);
    assert_eq!(status["viewport"], json!([390.0,701.0]));
    assert_eq!(status["viewport_owner"], attached["attachment_id"]);
    assert_eq!(status["viewport_revision"], 1);
    assert_eq!(status["document_revision"], old["document_revision"]);
    assert_eq!(status["control"]["epoch"], initial["control"]["epoch"]);
    assert_eq!(agent.raw(json!({"id":88,"operation":identity(&old),"command":{"type":"click","x":1,"y":1}}))["code"], "STALE_VIEWPORT");
    assert_eq!(agent.eval("window.marker.value===17 && document.getElementById('retained').value==='kept'"), true);
    assert_eq!(agent.send(json!({"type":"resize","width":800,"height":600}))["code"], "VIEWPORT_MANAGED");
    phone.ok(json!({"type":"declare_viewport","viewport":declaration("phone",701,390,"landscape")}));
    let rotated = settled(&mut phone);
    assert_eq!(rotated["viewport"], json!([701.0,390.0]));
    assert_eq!(rotated["viewport_revision"], 2);
    phone.ok(json!({"type":"declare_viewport","viewport":declaration("phone",701,390,"portrait")}));
    assert_eq!(settled(&mut phone)["viewport_revision"], 2, "orientation is independent; equal dimensions do not relayout");
}

#[test]
fn viewport_election_uses_whole_smallest_phone_and_reselects_on_disconnect() {
    let daemon = Daemon::start();
    let mut desktop = daemon.connect();
    let desk = desktop.ok(json!({"type":"attach","mode":"observe","viewport":declaration("desktop",200,100,"landscape")}));
    settled(&mut desktop);
    let mut first = daemon.connect();
    let one = first.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",300,800,"portrait")}));
    assert_eq!(settled(&mut desktop)["viewport_owner"], one["attachment_id"]);
    let mut second = daemon.connect();
    let two = second.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",400,500,"portrait")}));
    let small = settled(&mut desktop);
    assert_eq!(small["viewport_owner"], two["attachment_id"]);
    assert_eq!(small["viewport"], json!([400.0,500.0]), "never combine widths/heights from different phones");
    first.ok(json!({"type":"declare_viewport","viewport":declaration("phone",250,800,"portrait")}));
    assert_eq!(settled(&mut desktop)["viewport_owner"], one["attachment_id"], "equal area prefers smaller width");
    second.ok(json!({"type":"declare_viewport","viewport":declaration("phone",250,800,"portrait")}));
    let tied = settled(&mut desktop);
    assert_eq!(tied["viewport_owner"], one["attachment_id"], "equal size prefers older attachment");
    first.ok(json!({"type":"detach"}));
    let after_detach = settled(&mut desktop);
    assert_eq!(after_detach["viewport_owner"], two["attachment_id"]);
    assert_eq!(after_detach["viewport_revision"], tied["viewport_revision"], "same-size ownership transfer is not a resize");
    drop(second);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = settled(&mut desktop);
        if state["viewport_owner"] == desk["attachment_id"] { break; }
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(settled(&mut desktop)["viewport"], json!([200.0,100.0]));
    let last = desktop.ok(json!({"type":"detach"}));
    drop(desktop);
    let mut reconnect = daemon.connect();
    reconnect.ok(json!({"type":"attach","mode":"observe"}));
    let retained = settled(&mut reconnect);
    assert_eq!(retained["viewport"], last["viewport"]);
    assert_eq!(retained["viewport_revision"], last["viewport_revision"]);
    assert!(retained["viewport_owner"].is_null());
}

#[test]
fn viewport_declarations_validate_identity_and_capture_budget() {
    let daemon = Daemon::start();
    let mut viewer = daemon.connect();
    assert_eq!(viewer.send(json!({"type":"declare_viewport","viewport":declaration("phone",300,600,"portrait")}))["code"], "ATTACH_REQUIRED");
    assert_eq!(viewer.send(json!({"type":"attach","mode":"agent","viewport":declaration("phone",300,600,"portrait")}))["code"], "OBSERVER_REQUIRED");
    viewer.ok(json!({"type":"attach","mode":"observe"}));
    let initial = settled(&mut viewer);
    for (width, height) in [(0,600),(4097,100),(4096,4096)] {
        assert_eq!(viewer.send(json!({"type":"declare_viewport","viewport":declaration("phone",width,height,"portrait")}))["code"], "INVALID_VIEWPORT");
    }
    assert_eq!(viewer.raw(json!({"id":55,"operation":identity(&initial),"command":{"type":"declare_viewport","viewport":declaration("phone",300,600,"portrait")}}))["code"], "INVALID_OPERATION");
    let invalid = json!({"device":"phone","css_width":300,"css_height":600,"orientation":"guessed"});
    assert_eq!(viewer.send(json!({"type":"declare_viewport","viewport":invalid}))["code"], "INVALID_REQUEST");
    assert_eq!(settled(&mut viewer)["viewport_revision"], initial["viewport_revision"]);
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(agent.send(json!({"type":"declare_viewport","viewport":declaration("phone",300,600,"portrait")}))["code"], "OBSERVER_REQUIRED");
    let before = agent.ok(json!({"type":"resize","width":400,"height":300}));
    let after = agent.ok(json!({"type":"resize","width":400,"height":300}));
    assert_eq!(before["viewport_revision"], after["viewport_revision"]);
}

#[test]
fn viewport_waits_for_atomic_click_and_coalesces_before_human_input() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<button style='position:absolute;left:10px;top:10px;width:200px;height:60px' onmousedown='const end=Date.now()+1500;while(Date.now()<end){}' onclick='window.clickWidth=innerWidth;window.clicked=true'>hold</button>"}));
    let mut observer = daemon.connect(); let original = observer.ok(json!({"type":"attach","mode":"observe"}));
    let operation = identity(&agent.ok(json!({"type":"status"})));
    writeln!(agent.0.get_mut(), "{}", json!({"id":90,"operation":operation,"command":{"type":"click","x":30,"y":30}})).unwrap();
    let deadline = Instant::now() + Duration::from_millis(600);
    while observer.ok(json!({"type":"status"}))["operation_running"] != true {
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    }
    let pending = observer.ok(json!({"type":"declare_viewport","viewport":declaration("phone",390,701,"portrait")}));
    assert_eq!(pending["viewport_pending"], true);
    assert_eq!(pending["viewport"], original["viewport"]);
    assert_eq!(pending["viewport_revision"], original["viewport_revision"]);
    observer.ok(json!({"type":"declare_viewport","viewport":declaration("phone",701,390,"landscape")}));
    let waiting = observer.ok(json!({"type":"request_takeover","epoch":original["control"]["epoch"]}));
    assert_eq!(waiting["control"]["phase"]["type"], "waiting");
    assert_eq!(observer.send(json!({"type":"click","x":30,"y":30}))["type"], "error");
    assert_eq!(agent.read()["value"]["input"]["state"], "succeeded");
    let applied = settled(&mut observer);
    assert_eq!(applied["viewport"], json!([701.0,390.0]));
    assert_eq!(applied["viewport_revision"], 1, "intermediate pending declaration must not resize");
    assert_eq!(applied["control"]["phase"]["type"], "human");
    observer.ok(json!({"type":"release_control","epoch":applied["control"]["epoch"]}));
    assert_eq!(agent.eval("window.clicked===true && window.clickWidth===1280 && globalThis.__obscura_mouse_down===null"), true);
}

#[test]
fn detaching_viewport_owner_during_pending_layout_does_not_publish_stale_owner_or_layout() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut owner = daemon.connect();
    let attached = owner.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    let committed = settled(&mut owner);
    assert_eq!(committed["viewport_owner"], attached["attachment_id"]);

    agent.eval("globalThis.__obscura_recompute_resizes=function(){const end=Date.now()+750;while(Date.now()<end){}};true");
    owner.ok(json!({"type":"declare_viewport","viewport":declaration("phone",400,700,"portrait")}));
    let pending = owner.ok(json!({"type":"status"}));
    assert_eq!(pending["viewport_pending"], true);
    assert_eq!(pending["viewport"], committed["viewport"]);
    assert_eq!(pending["viewport_revision"], committed["viewport_revision"]);

    let detached = owner.ok(json!({"type":"detach"}));
    assert!(detached["attachment_id"].is_null());
    assert!(detached["viewport_owner"].is_null(), "detached attachment remains elected: {detached}");
    assert_eq!(detached["viewport"], committed["viewport"]);
    assert_eq!(detached["viewport_revision"], committed["viewport_revision"]);

    let deadline = Instant::now() + Duration::from_secs(3);
    let after = loop {
        let state = agent.ok(json!({"type":"status"}));
        if state["viewport_pending"] == false { break state; }
        assert!(Instant::now() < deadline, "detached layout did not settle: {state}");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(after["viewport_owner"].is_null(), "detached layout elected stale owner: {after}");
    assert_eq!(after["viewport"], committed["viewport"], "detached layout committed stale dimensions");
    assert_eq!(after["viewport_revision"], committed["viewport_revision"], "detached layout advanced revision");
}

#[test]
fn detaching_pending_viewport_restores_committed_dimensions_before_next_frame() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut owner = daemon.connect();
    owner.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    let committed = settled(&mut owner);
    let mut media = Media::connect(&daemon);
    let (initial_frame, _) = media.frame();
    assert_eq!(initial_frame["width"], 390);
    assert_eq!(initial_frame["height"], 701);
    assert_eq!(initial_frame["viewport_revision"], committed["viewport_revision"]);

    agent.eval("globalThis.__obscura_recompute_resizes=function(){const end=Date.now()+750;while(Date.now()<end){}};true");
    owner.ok(json!({"type":"declare_viewport","viewport":declaration("phone",400,700,"portrait")}));
    owner.ok(json!({"type":"detach"}));
    let after = settled(&mut agent);
    assert!(after["viewport_owner"].is_null());
    assert_eq!(after["viewport"], committed["viewport"]);
    assert_eq!(after["viewport_revision"], committed["viewport_revision"]);

    let (restored_frame, _) = media.frame();
    assert_eq!(restored_frame["width"], 390, "detached layout leaked stale frame width");
    assert_eq!(restored_frame["height"], 701, "detached layout leaked stale frame height");
    assert_eq!(restored_frame["viewport_revision"], committed["viewport_revision"], "detached layout leaked a new frame revision");
}

#[test]
fn detaching_pending_owner_reselects_remaining_observer_after_cancellation() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut first = daemon.connect();
    let first_attachment = first.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    let committed = settled(&mut first);
    assert_eq!(committed["viewport_owner"], first_attachment["attachment_id"]);

    let mut second = daemon.connect();
    let second_attachment = second.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",600,600,"portrait")}));
    assert_eq!(settled(&mut second)["viewport_owner"], first_attachment["attachment_id"]);
    agent.eval("globalThis.__obscura_recompute_resizes=function(){const end=Date.now()+750;while(Date.now()<end){}};true");
    first.ok(json!({"type":"declare_viewport","viewport":declaration("phone",400,700,"portrait")}));
    let detached = first.ok(json!({"type":"detach"}));
    assert!(detached["viewport_owner"].is_null());

    let after = settled(&mut second);
    assert_eq!(after["viewport_owner"], second_attachment["attachment_id"]);
    assert_eq!(after["viewport"], json!([600.0,600.0]));
    assert_eq!(after["viewport_revision"], json!(committed["viewport_revision"].as_u64().unwrap() + 1));
}

#[test]
fn detaching_pending_owner_with_remaining_committed_observer_keeps_frames_live() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut first = daemon.connect();
    let first_attachment = first.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    let committed = settled(&mut first);
    assert_eq!(committed["viewport_owner"], first_attachment["attachment_id"]);

    let mut media = Media::connect(&daemon);
    let (initial_frame, _) = media.frame();
    assert_eq!(initial_frame["width"], 390);
    assert_eq!(initial_frame["height"], 701);
    assert_eq!(initial_frame["viewport_revision"], committed["viewport_revision"]);

    agent.eval("globalThis.__obscura_recompute_resizes=function(){const end=Date.now()+750;while(Date.now()<end){}};true");
    first.ok(json!({"type":"declare_viewport","viewport":declaration("phone",400,700,"portrait")}));
    assert_eq!(first.ok(json!({"type":"status"}))["viewport_pending"], true);

    let mut remaining = daemon.connect();
    let remaining_attachment = remaining.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",390,701,"portrait")}));
    assert_eq!(remaining_attachment["viewport_pending"], true);
    first.ok(json!({"type":"detach"}));

    let after = settled(&mut remaining);
    assert_eq!(after["viewport_pending"], false);
    assert_eq!(after["viewport"], committed["viewport"]);
    assert_eq!(after["viewport_revision"], committed["viewport_revision"]);
    assert_eq!(after["viewport_owner"], remaining_attachment["attachment_id"]);

    let (next_frame, _) = media.frame();
    assert_eq!(next_frame["width"], 390, "cancelled layout left stale frame width");
    assert_eq!(next_frame["height"], 701, "cancelled layout left stale frame height");
    assert_eq!(next_frame["viewport_revision"], committed["viewport_revision"], "cancelled layout left stale frame revision");
}

#[test]
fn declared_viewports_publish_one_shared_frame_after_rotation() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<style>html,body{margin:0;background:rgb(12,34,56)}</style>"}));
    let mut phone = daemon.connect();
    phone.ok(json!({"type":"attach","mode":"observe","viewport":declaration("phone",391,701,"portrait")}));
    let mut first = Media::connect(&daemon);
    let mut second = Media::connect(&daemon);
    for (width, height, orientation) in [(391,701,"portrait"),(701,391,"landscape")] {
        phone.ok(json!({"type":"declare_viewport","viewport":declaration("phone",width,height,orientation)}));
        let state = settled(&mut phone);
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut a, mut pixels_a) = first.frame();
        let (mut b, mut pixels_b) = second.frame();
        while a["sequence"] != b["sequence"] || a["viewport_revision"] != state["viewport_revision"] {
            assert!(Instant::now() < deadline, "shared viewport frames did not converge");
            if a["sequence"].as_u64() <= b["sequence"].as_u64() { (a, pixels_a) = first.frame(); }
            else { (b, pixels_b) = second.frame(); }
        }
        assert_eq!(a, b);
        assert_eq!(pixels_a, pixels_b);
        assert_eq!(a["width"], width);
        assert_eq!(a["height"], height);
        assert_eq!(a["document_revision"], state["document_revision"]);
    }
}

#[test]
fn viewport_callback_deadline_faults_without_committing_or_handover() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.eval("globalThis.__obscura_recompute_resizes=function(){while(true){}};true");
    let mut observer = daemon.connect();
    let original = observer.ok(json!({"type":"attach","mode":"observe"}));
    observer.ok(json!({"type":"declare_viewport","viewport":declaration("phone",390,701,"portrait")}));
    observer.ok(json!({"type":"request_takeover","epoch":original["control"]["epoch"]}));
    let deadline = Instant::now() + Duration::from_secs(7);
    let faulted = loop {
        let state = observer.ok(json!({"type":"status"}));
        if state["fault"].is_string() { break state; }
        assert!(Instant::now() < deadline, "viewport callback left layout permanently pending");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(faulted["viewport_revision"], original["viewport_revision"]);
    assert_eq!(faulted["viewport"], original["viewport"], "partial resize is not a committed layout");
    assert_eq!(faulted["control"]["phase"]["type"], "waiting");
    agent.ok(json!({"type":"close_session"}));
}
impl Media {
    fn connect(daemon: &Daemon) -> Self {
        Self::connect_path(&daemon.dir.join("frames.sock"))
    }
    fn connect_path(path: &std::path::Path) -> Self {
        let socket = UnixStream::connect(path).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        Self(BufReader::new(socket))
    }
    fn frame(&mut self) -> (Value, Vec<u8>) {
        loop {
            let mut header = String::new();
            assert!(self.0.read_line(&mut header).unwrap() > 0);
            assert!(header.len() < 4096);
            let packet: Value = serde_json::from_str(&header).unwrap();
            if packet["type"] == "waiting" { continue; }
            assert_eq!(packet["type"], "frame", "{packet}");
            let info = packet["info"].clone();
            let size = info["byte_length"].as_u64().unwrap() as usize;
            assert!(size <= 4 * 4_194_304);
            let mut pixels = vec![0; size]; self.0.read_exact(&mut pixels).unwrap();
            return (info, pixels);
        }
    }
}

#[test]
fn viewers_share_raw_frames_and_follow_input_and_viewport_changes() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"resize","width":160,"height":120}));
    let state = agent.ok(json!({"type":"navigate","url":"data:text/html,<style>html,body{margin:0;background:rgb(12,34,56)}button{position:absolute;left:0;top:0;width:40px;height:40px;border:0;background:rgb(255,0,0)}</style><button id='tile' onclick=\"this.style.background='rgb(0,255,0)'\"></button><input id='retained' value='same document' style='position:absolute;left:50px;top:50px;width:100px;height:30px'>"}));
    assert_eq!(std::fs::metadata(daemon.dir.join("frames.sock")).unwrap().permissions().mode() & 0o777, 0o600);
    let mut first = Media::connect(&daemon); let mut second = Media::connect(&daemon);
    let (mut a, mut pixels_a) = first.frame(); let (mut b, mut pixels_b) = second.frame();
    let deadline = Instant::now() + Duration::from_secs(3);
    while a["sequence"] != b["sequence"] {
        assert!(Instant::now() < deadline, "viewers did not converge on a shared frame");
        if a["sequence"].as_u64() < b["sequence"].as_u64() { (a, pixels_a) = first.frame(); }
        else { (b, pixels_b) = second.frame(); }
    }
    assert_eq!(a, b); assert_eq!(pixels_a, pixels_b);
    assert_eq!(a["session_id"], state["session_id"]);
    assert_eq!(a["document_revision"], state["document_revision"]);
    assert_eq!(a["pixel_format"], "premultiplied_rgba8");
    assert_eq!(a["width"], 160); assert_eq!(a["height"], 120);
    let offset = 4 * (20 * 160 + 20);
    assert_eq!(&pixels_a[offset..offset+4], &[255,0,0,255]);
    assert_eq!(&pixels_a[4*(110*160+110)..4*(110*160+110)+4], &[12,34,56,255]);
    if let Ok(directory) = std::env::var("OBSCURA_FRAME_EVIDENCE_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(std::path::Path::new(&directory).join("shared.rgba"), &pixels_a).unwrap();
        std::fs::write(std::path::Path::new(&directory).join("shared.json"), serde_json::to_vec_pretty(&a).unwrap()).unwrap();
    }
    agent.ok(json!({"type":"click","x":20,"y":20}));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, pixels) = first.frame();
        if pixels[offset..offset+4] == [0,255,0,255] { break; }
        assert!(Instant::now() < deadline, "input was not reflected in frame pixels");
    }
    let resized = agent.ok(json!({"type":"resize","width":390,"height":844}));
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (info, _) = first.frame();
        if info["viewport_revision"] == resized["viewport_revision"] {
            assert_eq!(info["width"], 390); assert_eq!(info["height"], 844);
            assert_eq!(info["document_revision"], state["document_revision"]); break;
        }
        assert!(Instant::now() < deadline);
    }
    drop(first); drop(second);
    assert_eq!(agent.eval("document.getElementById('retained').value"), "same document");
    let mut reconnect = Media::connect(&daemon);
    let (info, _) = reconnect.frame(); assert_eq!(info["session_id"], state["session_id"]);
    assert_eq!(info["viewport_revision"], resized["viewport_revision"]);
}

#[test]
fn human_click_text_and_scroll_use_the_live_page() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<style>body{margin:0}input{position:absolute;left:10px;top:10px;width:200px;height:40px}button{position:absolute;left:10px;top:70px;width:200px;height:40px}.space{height:3000px;margin-top:150px}</style><input id='field'><button id='button' onclick='window.clicks=(window.clicks||0)+1'>click</button><div class='space'></div>"}));
    agent.eval("window.events=[];['mousedown','mouseup','click','input','wheel'].forEach(t=>document.addEventListener(t,e=>window.events.push([e.type,e.isTrusted])))");
    let mut human = daemon.connect();
    let attached = human.ok(json!({"type":"attach","mode":"observe"}));
    let operation = identity(&attached);
    assert_eq!(human.raw(json!({"id":10,"operation":operation,"command":{"type":"click","x":30,"y":30}}))["code"], "CONTROL_REQUIRED");
    human.ok(json!({"type":"request_takeover","epoch":attached["control"]["epoch"]}));
    let status = human.ok(json!({"type":"status"}));
    let clicked = human.raw(json!({"id":11,"operation":identity(&status),"command":{"type":"click","x":30,"y":30}}));
    assert_eq!(clicked["type"], "result", "{clicked}");
    assert_eq!(clicked["value"]["input"]["state"], "succeeded");
    let text = "中文'\\\"🙂\u{2028}";
    let input = json!({"id":12,"operation":identity(&human.ok(json!({"type":"status"}))),"command":{"type":"input_text","text":text}});
    let inserted = human.raw(input.clone());
    assert_eq!(inserted["type"], "result", "{inserted}");
    assert_eq!(human.raw(input)["type"], "result");
    human.ok(json!({"type":"click","x":30,"y":90}));
    assert_eq!(human.send(json!({"type":"input_text","text":"must not edit button"}))["code"], "INPUT_REJECTED");
    human.ok(json!({"type":"scroll","x":300,"y":300,"delta_x":0,"delta_y":400}));
    assert_eq!(human.send(json!({"type":"click","x":-1,"y":30}))["code"], "INVALID_POINT");
    assert_eq!(human.send(json!({"type":"input_text","text":"x".repeat(4097)}))["code"], "TEXT_TOO_LARGE");
    let status = human.ok(json!({"type":"status"}));
    human.ok(json!({"type":"release_control","epoch":status["control"]["epoch"]}));
    assert_eq!(agent.eval("document.getElementById('field').value"), text);
    assert_eq!(agent.eval("window.clicks").as_f64(), Some(1.0));
    assert!(agent.eval("document.scrollingElement.scrollTop").as_f64().unwrap() > 0.0);
    assert_eq!(agent.eval("window.events.every(e=>e[1]===true)"), true);
    assert_eq!(agent.eval("window.events.filter(e=>e[0]==='input').length").as_f64(), Some(1.0));
    assert_eq!(agent.eval("globalThis.__obscura_mouse_down===null"), true);
}

#[test]
fn human_navigation_respects_control_identity_and_document_revision() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<title>before</title>"}));

    let mut observer = daemon.connect();
    let attached = observer.ok(json!({"type":"attach","mode":"observe"}));
    let operation = identity(&attached);
    assert_eq!(observer.raw(json!({"id":201,"operation":operation,"command":{"type":"navigate","url":"about:blank"}}))["code"], "CONTROL_REQUIRED");

    let human = observer.ok(json!({"type":"request_takeover","epoch":attached["control"]["epoch"]}));
    assert_eq!(human["control"]["phase"]["type"], "human");
    let stale = identity(&attached);
    assert_eq!(observer.raw(json!({"id":202,"operation":stale,"command":{"type":"navigate","url":"about:blank"}}))["code"], "STALE_CONTROL");
    assert_eq!(observer.send(json!({"type":"navigate","url":"file:///blocked"}))["code"], "INVALID_URL");

    let before = observer.ok(json!({"type":"status"}));
    let after = observer.ok(json!({"type":"navigate","url":"data:text/html,<title>after</title>"}));
    assert_eq!(after["document_revision"].as_u64(), Some(before["document_revision"].as_u64().unwrap() + 1));
    observer.ok(json!({"type":"release_control","epoch":after["control"]["epoch"]}));
    assert_eq!(agent.eval("document.title"), "after");
}

#[test]
fn click_receipt_precedes_navigation_and_old_document_input_is_rejected() {
    let daemon = Daemon::start_with_network(true);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived, started) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("fixture accept: {e}"),
            }
        };
        // The listener is nonblocking to poll accept, but macOS accepts inherit
        // O_NONBLOCK, which makes set_read_timeout inert and read() return EAGAIN
        // before the request arrives. Restore blocking mode so the read waits for
        // the request (bounded by the timeout) instead of racing the client.
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let request = read_complete_http_request(&stream, 8 * 1024)
            .unwrap_or_else(|error| panic!("fixture request read: {error}"));
        assert!(request.starts_with(b"GET / HTTP/"), "fixture did not observe a complete HTTP GET request: {request:?}");
        arrived.send(()).unwrap();
        gate.recv_timeout(Duration::from_secs(4)).unwrap();
        let body = "<title>destination</title><button style='position:absolute;left:10px;top:10px;width:200px;height:60px' onclick='window.unwanted=true'>new document</button>";
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":format!("data:text/html,<a style='position:absolute;left:10px;top:10px;width:200px;height:60px' href='http://{address}/'>next</a>")}));
    let mut human = daemon.connect(); let status = human.ok(json!({"type":"attach","mode":"observe"}));
    human.ok(json!({"type":"request_takeover","epoch":status["control"]["epoch"]}));
    let start = Instant::now();
    human.ok(json!({"type":"click","x":30,"y":30}));
    assert!(start.elapsed() < Duration::from_secs(1), "input receipt waited for navigation response");
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let status = human.ok(json!({"type":"status"}));
    human.ok(json!({"type":"release_control","epoch":status["control"]["epoch"]}));
    let stale = identity(&agent.ok(json!({"type":"status"})));
    writeln!(agent.0.get_mut(), "{}", json!({"id":81,"operation":stale,"command":{"type":"click","x":30,"y":30}})).unwrap();
    release.send(()).unwrap(); server.join().unwrap();
    assert_eq!(agent.read()["code"], "STALE_DOCUMENT");
    assert_eq!(agent.eval("document.title"), "destination");
    assert_eq!(agent.eval("window.unwanted===undefined"), true);
}

#[test]
fn takeover_during_real_click_waits_for_release_and_click_dispatch() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<button style='position:absolute;left:10px;top:10px;width:200px;height:60px' onmousedown='const end=Date.now()+1500;while(Date.now()<end){}' onclick='window.clicked=true'>hold</button>"}));
    let mut observer = daemon.connect(); let status = observer.ok(json!({"type":"attach","mode":"observe"}));
    let operation = identity(&agent.ok(json!({"type":"status"})));
    writeln!(agent.0.get_mut(), "{}", json!({"id":90,"operation":operation,"command":{"type":"click","x":30,"y":30}})).unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while observer.ok(json!({"type":"status"}))["operation_running"] != true {
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    }
    let waiting = observer.ok(json!({"type":"request_takeover","epoch":status["control"]["epoch"]}));
    assert_eq!(waiting["control"]["phase"]["type"], "waiting");
    assert_eq!(observer.send(json!({"type":"click","x":30,"y":30}))["code"], "CONTROL_REQUIRED");
    assert_eq!(agent.read()["value"]["input"]["state"], "succeeded");
    let granted = observer.ok(json!({"type":"status"}));
    assert_eq!(granted["control"]["phase"]["type"], "human");
    observer.ok(json!({"type":"release_control","epoch":granted["control"]["epoch"]}));
    assert_eq!(agent.eval("window.clicked===true && globalThis.__obscura_mouse_down===null"), true);
}

#[test]
fn all_clients_detach_without_destroying_document_or_timers() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    let first = agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"navigate","url":"data:text/html,<input id='form'>"}));
    assert_eq!(agent.eval("(function(){window.marker='original';document.getElementById('form').value='retained';window.ticks=0;setInterval(()=>window.ticks++,20);return true})()"), true);
    agent.ok(json!({"type":"detach"})); drop(agent);
    std::thread::sleep(Duration::from_millis(200));
    let mut reconnect = daemon.connect();
    let second = reconnect.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(first["session_id"], second["session_id"]);
    assert_eq!(reconnect.eval("window.marker"), "original");
    assert_eq!(reconnect.eval("document.getElementById('form').value"), "retained");
    assert!(reconnect.eval("window.ticks").as_f64().unwrap() > 0.0);
    reconnect.ok(json!({"type":"resize","width":390,"height":844}));
    assert_eq!(reconnect.eval("window.innerWidth").as_f64().unwrap(), 390.0);
    assert_eq!(reconnect.eval("document.getElementById('form').value"), "retained");
}

#[test]
fn observers_cannot_mutate_and_close_is_explicit() {
    let daemon = Daemon::start();
    let mut observer = daemon.connect();
    observer.ok(json!({"type":"attach","mode":"observe"}));
    assert_eq!(observer.send(json!({"type":"evaluate","expression":"1"}))["code"], "AGENT_REQUIRED");
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut rival = daemon.connect();
    assert_eq!(rival.send(json!({"type":"attach","mode":"agent"}))["code"], "AGENT_BUSY");
    assert_eq!(agent.send(json!({"type":"resize","width":0,"height":10}))["code"], "INVALID_VIEWPORT");
    agent.ok(json!({"type":"close_session"}));
    assert_eq!(observer.send(json!({"type":"status"}))["code"], "SESSION_CLOSED");
}

#[test]
fn sleeping_page_remains_responsive_and_fetch_finishes_after_disconnect() {
    let daemon = Daemon::start_with_network(true);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // The listener is nonblocking so the accept loop can poll,
                    // but on macOS the accepted socket inherits O_NONBLOCK, which
                    // makes set_read_timeout inert: read() would return EAGAIN
                    // before the request arrives. Return the stream to blocking
                    // mode so the read waits for the request (bounded by the
                    // timeout) instead of racing the client.
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let request = read_complete_http_request(&stream, 8 * 1024)
                        .unwrap_or_else(|error| panic!("fixture request read: {error}"));
                    assert!(request.starts_with(b"GET / HTTP/"), "fixture did not observe a complete HTTP GET request: {request:?}");
                    write!(stream, "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone").unwrap();
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { assert!(Instant::now() < until, "fetch never arrived"); std::thread::sleep(Duration::from_millis(10)); }
                Err(e) => panic!("fixture: {e}"),
            }
        }
    });
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let expression = format!("(function(){{window.completed='pending';setTimeout(()=>window.distant=true,60000);fetch('http://{address}/').then(r=>r.text()).then(t=>window.completed=t);return true}})()");
    assert_eq!(agent.eval(&expression), true);
    drop(agent); // No detach command: EOF must remove only the attachment.
    server.join().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    let mut other = daemon.connect(); other.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(other.eval("window.completed"), "done");
    assert!(start.elapsed() < Duration::from_secs(2), "distant timer blocked incoming commands");
}

#[test]
fn timeout_fences_session_and_exception_is_not_success_null() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"throw new Error('fixture')"}))["code"], "JAVASCRIPT_EXCEPTION");
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"'x'.repeat(1048577)"}))["code"], "RESPONSE_TOO_LARGE");
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"while(true){}"}))["code"], "OUTCOME_UNKNOWN");
    let mut observer = daemon.connect();
    assert_eq!(observer.send(json!({"type":"attach","mode":"agent"}))["code"], "SESSION_FAULTED");
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"1"}))["code"], "SESSION_FAULTED");
    agent.ok(json!({"type":"close_session"}));
}

#[test]
fn status_stays_responsive_during_synchronous_javascript() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut observer = daemon.connect();
    observer.ok(json!({"type":"attach","mode":"observe"}));
    let operation = identity(&agent.ok(json!({"type":"status"})));
    writeln!(agent.0.get_mut(), "{}", json!({"id":2,"operation":operation,"command":{"type":"evaluate","expression":"(function(){const until=Date.now()+1500;while(Date.now()<until){};return 42})()"}})).unwrap();
    let admission_deadline = Instant::now() + Duration::from_millis(500);
    while observer.ok(json!({"type":"status"}))["operation_running"] != true {
        assert!(Instant::now() < admission_deadline, "operation was not admitted promptly");
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    observer.ok(json!({"type":"status"}));
    assert!(started.elapsed() < Duration::from_millis(500), "browser execution blocked control plane");
    assert_eq!(agent.read()["value"]["result"]["value"].as_f64(), Some(42.0));
    assert_eq!(observer.ok(json!({"type":"status"}))["operation_running"], false);
}

#[test]
fn takeover_waits_for_current_operation_and_requires_explicit_release() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut human = daemon.connect(); human.ok(json!({"type":"attach","mode":"observe"}));
    let status = human.ok(json!({"type":"status"}));
    let epoch = status["control"]["epoch"].clone();
    let operation = identity(&agent.ok(json!({"type":"status"})));
    writeln!(agent.0.get_mut(), "{}", json!({"id":7,"operation":operation,"command":{"type":"evaluate","expression":"(function(){const until=Date.now()+1500;while(Date.now()<until){};window.finished=true;return 7})()"}})).unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while human.ok(json!({"type":"status"}))["operation_running"] != true {
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    }
    let requested = human.ok(json!({"type":"request_takeover","epoch":epoch}));
    assert_eq!(requested["control"]["phase"]["type"], "waiting");
    assert_eq!(requested["operation_running"], true);
    assert!(Instant::now() < deadline, "takeover acceptance blocked behind JavaScript");
    // Pipelined Agent input was sent before receiving the terminal reply. Its
    // old generation must never start once takeover has been accepted.
    let mut next = operation.clone(); next["sequence"] = json!(2);
    writeln!(agent.0.get_mut(), "{}", json!({"id":8,"operation":next,"command":{"type":"evaluate","expression":"window.unwanted=true"}})).unwrap();
    assert_eq!(agent.read()["value"]["result"]["value"].as_f64(), Some(7.0));
    assert_eq!(agent.read()["code"], "STALE_CONTROL");
    let granted = human.ok(json!({"type":"status"}));
    assert_eq!(granted["control"]["phase"]["type"], "human");
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"1"}))["code"], "CONTROL_REQUIRED");
    assert_eq!(human.send(json!({"type":"evaluate","expression":"1"}))["code"], "AGENT_REQUIRED");
    assert_eq!(human.send(json!({"type":"release_control","epoch":epoch}))["code"], "STALE_CONTROL");
    human.ok(json!({"type":"release_control","epoch":granted["control"]["epoch"]}));
    assert_eq!(agent.eval("window.finished === true && window.unwanted === undefined"), true);
}

#[test]
fn operation_receipts_prevent_duplicate_effects_and_reject_stale_context() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    let status = agent.ok(json!({"type":"attach","mode":"agent"}));
    let operation = identity(&status);
    let request = json!({"id":10,"operation":operation,"command":{"type":"evaluate","expression":"window.count=(window.count||0)+1"}});
    let first = agent.raw(request.clone());
    let mut replay = request.clone(); replay["id"] = json!(11);
    let second = agent.raw(replay);
    assert_eq!(second["id"], 11); assert_eq!(first["value"], second["value"]);
    let mut conflict = request.clone(); conflict["command"]["expression"] = json!("window.count=99");
    assert_eq!(agent.raw(conflict)["code"], "OPERATION_CONFLICT");
    assert_eq!(agent.eval("window.count").as_f64(), Some(1.0));
    assert_eq!(agent.raw(request.clone())["code"], "OPERATION_EXPIRED");
    let stale = identity(&agent.ok(json!({"type":"status"})));
    agent.ok(json!({"type":"resize","width":390,"height":844}));
    let mut wrong_viewport = identity(&agent.ok(json!({"type":"status"})));
    wrong_viewport["viewport_revision"] = stale["viewport_revision"].clone();
    assert_eq!(agent.raw(json!({"id":12,"operation":wrong_viewport,"command":{"type":"evaluate","expression":"window.count=99"}}))["code"], "STALE_VIEWPORT");
    assert_eq!(agent.raw(json!({"id":13,"command":{"type":"evaluate","expression":"window.count=99"}}))["code"], "OPERATION_REQUIRED");
    agent.ok(json!({"type":"detach"}));
    assert_eq!(agent.send(json!({"type":"attach","mode":"agent"}))["code"], "RECONNECT_REQUIRED");
    let mut reconnect = daemon.connect(); reconnect.ok(json!({"type":"attach","mode":"agent"}));
    assert_eq!(reconnect.raw(request)["code"], "STALE_ATTACHMENT");
    assert_eq!(reconnect.eval("window.count").as_f64(), Some(1.0));
    let operation = identity(&reconnect.ok(json!({"type":"status"})));
    let partial = json!({"id":20,"operation":operation,"command":{"type":"evaluate","expression":"window.count++;throw new Error('partial')"}});
    assert_eq!(reconnect.raw(partial.clone())["code"], "JAVASCRIPT_EXCEPTION");
    assert_eq!(reconnect.raw(partial)["code"], "JAVASCRIPT_EXCEPTION");
    assert_eq!(reconnect.eval("window.count").as_f64(), Some(2.0));
}

#[test]
fn human_disconnect_pauses_until_explicit_agent_resume() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    let mut human = daemon.connect();
    let status = human.ok(json!({"type":"attach","mode":"observe"}));
    human.ok(json!({"type":"request_takeover","epoch":status["control"]["epoch"]}));
    drop(human);
    let deadline = Instant::now() + Duration::from_secs(2);
    let paused = loop {
        let status = agent.ok(json!({"type":"status"}));
        if status["control"]["phase"]["type"] == "paused" { break status; }
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(agent.send(json!({"type":"evaluate","expression":"1"}))["code"], "CONTROL_REQUIRED");
    agent.ok(json!({"type":"resume_agent","epoch":paused["control"]["epoch"]}));
    assert_eq!(agent.eval("1").as_f64(), Some(1.0));
}

#[test]
fn unknown_outcome_never_grants_pending_takeover() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect();
    agent.ok(json!({"type":"attach","mode":"agent"}));
    let status = agent.ok(json!({"type":"navigate","url":"data:text/html,<button style='position:absolute;left:10px;top:10px;width:200px;height:60px' onmousedown='while(true){}'>stuck press</button>"}));
    let mut human = daemon.connect(); human.ok(json!({"type":"attach","mode":"observe"}));
    writeln!(agent.0.get_mut(), "{}", json!({"id":7,"operation":identity(&status),"command":{"type":"click","x":30,"y":30}})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while human.ok(json!({"type":"status"}))["operation_running"] != true {
        assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5));
    }
    human.ok(json!({"type":"declare_viewport","viewport":declaration("phone",390,701,"portrait")}));
    human.ok(json!({"type":"request_takeover","epoch":status["control"]["epoch"]}));
    assert_eq!(agent.read()["code"], "OUTCOME_UNKNOWN");
    let faulted = human.ok(json!({"type":"status"}));
    assert_eq!(faulted["control"]["phase"]["type"], "waiting");
    assert!(faulted["fault"].is_string());
    assert_eq!(faulted["viewport_pending"], true);
    assert_eq!(faulted["viewport_revision"], status["viewport_revision"]);
    assert_eq!(faulted["viewport"], status["viewport"]);
    assert_eq!(human.send(json!({"type":"release_control","epoch":faulted["control"]["epoch"]}))["code"], "SESSION_FAULTED");
    agent.ok(json!({"type":"close_session"}));
}

#[test]
fn private_socket_rejects_replacement_and_invalid_protocol_frames() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start();
    assert_eq!(std::fs::metadata(&daemon.dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(daemon.dir.join("host.sock")).unwrap().permissions().mode() & 0o777, 0o600);
    let duplicate = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(&daemon.dir).output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("socket directory must be new"));
    let mut client = daemon.connect();
    assert_eq!(client.send(json!({"type":"status","unexpected":1}))["code"], "INVALID_REQUEST");
    assert_eq!(client.send(json!({"type":"status"}))["code"], "ATTACH_REQUIRED");
    client.ok(json!({"type":"attach","mode":"observe"}));
    client.0.get_mut().write_all(&vec![b'x';65537]).unwrap();
    assert_eq!(client.read()["code"], "INVALID_FRAME");
    let mut line = String::new(); assert_eq!(client.0.read_line(&mut line).unwrap(), 0);
}

/// B2: frame interest has exactly one accounting owner. The exact-session raw
/// socket and the legacy `frames.sock` share one count. A legacy viewer that
/// connects and then leaves must not drop that count to zero while an
/// exact-session consumer is still attached, so the worker must keep painting.
#[test]
fn exact_session_interest_survives_legacy_viewer_disconnect() {
    let daemon = Daemon::start();
    let mut agent = daemon.connect(); agent.ok(json!({"type":"attach","mode":"agent"}));
    agent.ok(json!({"type":"resize","width":160,"height":120}));
    let state = agent.ok(json!({"type":"navigate","url":"data:text/html,<style>html,body{margin:0;background:rgb(12,34,56)}</style>"}));
    let session_id = state["session_id"].as_str().unwrap().to_string();
    let exact_path = daemon.dir.join(format!("session-{session_id}.sock"));
    let mut exact = Media::connect_path(&exact_path);
    let (mut previous, _) = exact.frame();
    assert_eq!(previous["session_id"], state["session_id"]);
    // A legacy viewer joins and is fully accepted (one frame), then leaves.
    let mut legacy = Media::connect(&daemon);
    let _ = legacy.frame();
    drop(legacy);
    // The exact-session consumer must keep receiving new frames. Before the
    // one-owner fix the legacy disconnect drives the shared interest to zero and
    // the worker stops painting, so this advance cannot be reached.
    let target = previous["sequence"].as_u64().unwrap() + 6;
    let deadline = Instant::now() + Duration::from_secs(6);
    while previous["sequence"].as_u64().unwrap() < target {
        let (next, _) = exact.frame();
        if next["sequence"].as_u64().unwrap() > previous["sequence"].as_u64().unwrap() { previous = next; }
        assert!(Instant::now() < deadline, "exact-session stream stopped after the legacy viewer disconnected: {previous}");
    }
    agent.ok(json!({"type":"close_session"}));
}

// ---------------------------------------------------------------------------
// Local admin authority (`host-admin.sock`)
//
// These cases drive the real daemon process over the real Unix socket. They
// assert only the public frame/receipt surface: typed success receipts, typed
// refusals, and the persisted catalog revision observed through
// `admin.catalog.info`.
// ---------------------------------------------------------------------------

struct AdminDaemon {
    child: Child,
    root: std::path::PathBuf,
    socket_dir: std::path::PathBuf,
}

impl AdminDaemon {
    /// Start a daemon with a persistent profile root and one registry that
    /// contains an active grantable device, a revoked device, and a device
    /// whose initial grant revision is stale.
    fn start() -> Self {
        let root = std::path::PathBuf::from("/tmp").join(format!("oha-{}", uuid::Uuid::new_v4()));
        let socket_dir = root.join("host");
        let profile_root = root.join("profiles");
        std::fs::create_dir_all(&root).unwrap();
        write_admin_registry(&root.join("registry.json"));
        let child = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
            .arg("--socket-dir").arg(&socket_dir)
            .arg("--profile-root").arg(&profile_root)
            .arg("--authorization-registry").arg(root.join("registry.json"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut daemon = Self { child, root, socket_dir };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match UnixStream::connect(daemon.socket_dir.join("host-admin.sock")) {
                Ok(_) => break,
                Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {}
                Err(error) => panic!("admin listener startup: {error}"),
            }
            assert!(daemon.child.try_wait().unwrap().is_none(), "daemon exited before admin listener ready");
            assert!(Instant::now() < deadline, "admin listener startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
}

impl Drop for AdminDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() { self.child.kill().unwrap(); }
        self.child.wait().unwrap();
        if self.root.exists() { std::fs::remove_dir_all(&self.root).unwrap(); }
    }
}

struct AdminClient {
    stream: UnixStream,
    buffered: BufReader<UnixStream>,
}

impl AdminClient {
    fn connect(socket_dir: &std::path::Path) -> Self {
        let stream = UnixStream::connect(socket_dir.join("host-admin.sock")).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let buffered = BufReader::new(stream.try_clone().unwrap());
        Self { stream, buffered }
    }

    fn raw(&mut self, frame: &[u8]) -> Value {
        self.stream.write_all(frame).unwrap();
        let mut line = String::new();
        assert!(self.buffered.read_line(&mut line).unwrap() > 0, "admin connection closed without a response");
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, request: Value) -> Value {
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        self.raw(&frame)
    }

    /// Send a request and require the typed success shape, returning the full
    /// response so the caller can inspect `catalog_revision` and receipts.
    fn ok(&mut self, request: Value) -> Value {
        let response = self.request(request);
        assert_eq!(response["state"], "ok", "expected admin success: {response}");
        response
    }

    fn error(&mut self, request: Value, code: &str) -> Value {
        let response = self.request(request);
        assert_eq!(response["state"], "error", "expected admin refusal {code}: {response}");
        assert_eq!(response["code"], code, "unexpected admin refusal: {response}");
        response
    }
}

fn write_admin_registry(path: &std::path::Path) {
    let device = |device_id: &str, status: &str, grant_revision: u64, fingerprint: &str| {
        json!({
            "device_id": device_id,
            "device_label": device_id,
            "client_certificate_sha256": fingerprint,
            "scopes": ["observe_human_control", "profile_read", "profile_create", "profile_select"],
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": grant_revision},
            "scope_revision": 1,
            "status": status,
            "not_before_unix": 0,
            "not_after_unix": 4102444800i64,
            "created_at_unix": 0,
            "revoked_at_unix": null
        })
    };
    let registry = json!({
        "schema_version": 2,
        "authorization_revision": 3,
        "host_id": "host-admin-test",
        "devices": [
            device("device-active", "active", 3, &"22".repeat(32)),
            device("device-revoked", "revoked", 3, &"33".repeat(32)),
            device("device-stale", "active", 2, &"44".repeat(32)),
        ]
    });
    std::fs::write(path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
}

fn admin_create(id: u64, display_name: &str, make_default: bool, expected: Option<u64>) -> Value {
    let mut request = json!({
        "schema_version": 1,
        "id": id,
        "op": "admin.catalog.create",
        "display_name": display_name,
        "make_default": make_default,
    });
    if let Some(expected) = expected { request["expected_catalog_revision"] = json!(expected); }
    request
}

fn admin_select(id: u64, profile_id: &str, expected: Option<u64>) -> Value {
    let mut request = json!({
        "schema_version": 1,
        "id": id,
        "op": "admin.catalog.select",
        "profile_id": profile_id,
    });
    if let Some(expected) = expected { request["expected_catalog_revision"] = json!(expected); }
    request
}

fn admin_membership(
    id: u64,
    intent: &str,
    attempt_id: &str,
    profile_id: &str,
    device_id: &str,
    members: &[&str],
    expected: Option<u64>,
) -> Value {
    let mut request = json!({
        "schema_version": 1,
        "id": id,
        "op": "admin.membership.set",
        "intent": intent,
        "attempt_id": attempt_id,
        "profile_id": profile_id,
        "owner_device_id": "local",
        "device_id": device_id,
        "members": members,
    });
    if let Some(expected) = expected { request["expected_catalog_revision"] = json!(expected); }
    request
}

#[test]
fn admin_authority_commits_catalog_and_refuses_invalid_membership() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = AdminDaemon::start();
    let mode = std::fs::metadata(daemon.socket_dir.join("host-admin.sock")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "host-admin.sock must be owner-only");

    let mut admin = AdminClient::connect(&daemon.socket_dir);
    let info = admin.ok(json!({"schema_version":1,"id":1,"op":"admin.catalog.info"}));
    assert_eq!(info["catalog_revision"], 1);
    assert_eq!(info["default_profile_id"], "default");

    // Catalog create commits a new revision and returns the created profile.
    let created = admin.ok(admin_create(2, "Work", false, Some(1)));
    assert_eq!(created["catalog_revision"], 2);
    assert_eq!(created["profile"]["profile_id"], "p1");
    assert_eq!(created["profile"]["owner_device_id"], "local");

    // Select commits another revision, never creating a profile.
    let selected = admin.ok(admin_select(3, "p1", Some(2)));
    assert_eq!(selected["catalog_revision"], 3);
    assert_eq!(selected["default_profile_id"], "p1");
    admin.error(admin_select(4, "p404", None), "PROFILE_NOT_FOUND");

    // Grant to an active registered device returns an export-eligible receipt
    // bound to the committed revision and the exact applied members.
    let granted = admin.ok(admin_membership(5, "grant", "att-grant", "p1", "device-active", &["local", "device-active"], Some(3)));
    assert_eq!(granted["catalog_revision"], 4);
    let receipt = &granted["membership_receipt"];
    assert_eq!(receipt["receipt_type"], "export_eligible");
    assert_eq!(receipt["catalog_revision"], 4);
    assert_eq!(receipt["profile_id"], "p1");
    assert_eq!(receipt["device_id"], "device-active");
    assert_eq!(receipt["owner_device_id"], "local");
    assert_eq!(receipt["members"], json!(["device-active", "local"]));

    // Exact replay with a new request id returns the same committed receipt.
    let replay = admin.ok(admin_membership(6, "grant", "att-grant", "p1", "device-active", &["local", "device-active"], Some(3)));
    assert_eq!(replay["membership_receipt"], *receipt);

    // Same attempt id with any changed field is not a replay.
    admin.error(admin_membership(7, "remove", "att-grant", "p1", "device-active", &["local"], Some(3)), "ADMIN_FRAME_MALFORMED");

    // Same attempt id after catalog movement is a revision conflict.
    admin.ok(admin_create(8, "Later", false, Some(4)));
    admin.error(admin_membership(9, "grant", "att-grant", "p1", "device-active", &["local", "device-active"], Some(3)), "CATALOG_REVISION_CONFLICT");

    // Revision conflict is refused before any membership write.
    admin.error(admin_membership(10, "grant", "att-conflict", "p1", "device-active", &["local", "device-active"], Some(2)), "CATALOG_REVISION_CONFLICT");

    // Revoked, absent and stale-grant devices are typed refusals.
    admin.error(admin_membership(11, "grant", "att-revoked", "p1", "device-revoked", &["local", "device-revoked"], None), "DEVICE_REVOKED");
    admin.error(admin_membership(12, "grant", "att-absent", "p1", "device-absent", &["local", "device-absent"], None), "DEVICE_NOT_REGISTERED");
    admin.error(admin_membership(13, "grant", "att-stale", "p1", "device-stale", &["local", "device-stale"], None), "DEVICE_GRANT_STALE");

    // Grant that omits the target is refused before mutation.
    admin.error(admin_membership(14, "grant", "att-omitted", "p1", "device-active", &["local"], None), "MEMBERSHIP_REQUIRED");

    // Explicit removal refuses while the target is still present.
    admin.error(admin_membership(15, "remove", "att-remove-bad", "p1", "device-active", &["local", "device-active"], None), "PROFILE_ACCESS_FAILED");

    // Valid removal commits and returns a `removed` receipt that cannot be
    // consumed as a grant.
    let removed = admin.ok(admin_membership(16, "remove", "att-remove", "p1", "device-active", &["local"], None));
    let removed_receipt = &removed["membership_receipt"];
    assert_eq!(removed_receipt["receipt_type"], "removed");
    assert_eq!(removed_receipt["device_id"], "device-active");
    assert_eq!(removed_receipt["members"], json!(["local"]));
    assert!(
        !removed_receipt["members"].as_array().unwrap().iter().any(|member| member == "device-active"),
        "removed receipt must not contain the removed target: {removed_receipt}"
    );

    // Persisted truth matches the committed receipt.
    let after = admin.ok(json!({"schema_version":1,"id":17,"op":"admin.catalog.info"}));
    assert_eq!(after["catalog_revision"], removed["catalog_revision"]);
    let p1 = after["profiles"].as_array().unwrap().iter().find(|profile| profile["profile_id"] == "p1").unwrap();
    assert_eq!(p1["members"], json!(["local"]));
}

#[test]
fn admin_authority_refuses_malformed_partial_unknown_and_wrong_intent() {
    let daemon = AdminDaemon::start();
    let mut admin = AdminClient::connect(&daemon.socket_dir);

    // Unknown op and wrong intent are typed malformed refusals.
    admin.error(json!({"schema_version":1,"id":1,"op":"admin.catalog.delete"}), "ADMIN_FRAME_MALFORMED");
    admin.error(admin_membership(2, "toggle", "att-intent", "p1", "device-active", &["local", "device-active"], None), "ADMIN_FRAME_MALFORMED");

    // Malformed JSON on one frame is refused and the connection stays usable.
    let malformed = admin.raw(b"{not json}\n");
    assert_eq!(malformed["state"], "error");
    assert_eq!(malformed["code"], "ADMIN_FRAME_MALFORMED");
    admin.ok(json!({"schema_version":1,"id":3,"op":"admin.catalog.info"}));

    // A partial frame at EOF is refused while the response side stays writable.
    let mut partial = AdminClient::connect(&daemon.socket_dir);
    partial.stream.write_all(b"{\"schema_version\":1,\"id\":4,").unwrap();
    partial.stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut line = String::new();
    assert!(partial.buffered.read_line(&mut line).unwrap() > 0, "partial frame must receive a typed refusal");
    let refusal: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(refusal["state"], "error");
    assert_eq!(refusal["code"], "ADMIN_FRAME_MALFORMED");
}
