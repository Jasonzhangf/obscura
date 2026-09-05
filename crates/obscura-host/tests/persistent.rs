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
        let operation = if matches!(command["type"].as_str(), Some("evaluate" | "navigate" | "resize" | "close_session" | "click" | "input_text" | "scroll")) {
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
        let socket = UnixStream::connect(daemon.dir.join("frames.sock")).unwrap();
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
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop { let mut line = String::new(); assert!(reader.read_line(&mut line).unwrap() > 0); if line == "\r\n" { break; } }
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
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut input = BufReader::new(stream.try_clone().unwrap());
                    loop { let mut line = String::new(); input.read_line(&mut line).unwrap(); if line == "\r\n" { break; } }
                    std::thread::sleep(Duration::from_millis(250));
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
