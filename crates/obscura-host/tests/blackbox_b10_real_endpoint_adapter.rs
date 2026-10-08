#![cfg(unix)]
//! F5 black-box regression: the real `obscura-endpoint` exercises its own
//! `Armed` encoder authority against the real `obscura-media` adapter.
//!
//! The two cases that motivated this file (`registration_blackbox.rs`) used the
//! scripted control peers `serve_control_pending_register` and
//! `serve_control_delayed_revoke`, which replace `EncoderControlState` and the
//! endpoint finalizer with canned answers. They never exercise the real
//! endpoint's `Armed` authority. These cases do:
//!
//! * `real_endpoint_round_trip_produces_access_unit` proves the full real
//!   round trip. The real adapter registers with the real endpoint, the real
//!   endpoint commits `Armed`, and the real adapter produces a real H.264
//!   access unit on the endpoint's `/media` stream. The only test helper is the
//!   external FFmpeg binary the adapter spawns.
//!
//! * `real_endpoint_armed_authority_signals_registered_group` pauses the real
//!   adapter (SIGSTOP) after the endpoint has committed the registration but
//!   before the adapter has consumed any revoke acknowledgement, then releases
//!   the final control reference. The endpoint finalizer must settle the group
//!   through its own `Armed` authority. Because the adapter is paused, the
//!   `SIGTERM` the group records can only come from the endpoint. The assertion
//!   reads public protocol output: the group's signal log and the endpoint's
//!   session-directory cleanup. No canned control peer, no replaced
//!   `EncoderControlState` and no replaced finalizer are involved.
//!
//! R10 additions (F1/F2/F3/F4 of the m1-p3-r10 plan)
//! -------------------------------------------------
//! * `r10_unregistered_child_close_retains_failure` (F1): the close-edge
//!   retirement must not fabricate a no-child settlement for a permit that was
//!   granted and never registered while a real adapter direct child is alive.
//!   The same correct assertions run against the baseline build (no retirement
//!   guard) through `R10_PRODUCT_BINARY_DIR`, which is the F1 minimal
//!   counterfactual that attributes the deviation to the guard.
//! * `r11_reader_failure_and_unsettled_permit_both_surface` (F1, R11): the same
//!   unregistered-child session, but the encoder-control reader also fails on a
//!   real malformed frame. The single session terminal must carry the reader's
//!   original `ENCODER_CONTROL_MALFORMED` cause **and** the unsettled-permit
//!   terminal, on the published attachment surface and on the endpoint error
//!   exit. A supervisor that returns on the finalizer error first drops the
//!   reader's cause and fails this case.
//! * `r10_no_child_settlement_closes_normally` (F1 positive control): a real
//!   typed `Settle(RefusedNoChild)` over the real control socket must still
//!   close the session normally. A guard that turns every close into a retained
//!   failure fails here.
//! * `r10_revoking_close_waits_for_signal_completion` (F1): a close edge that
//!   lands while the endpoint's own group-signal work is in flight must not
//!   publish session completion before that work finishes.
//! * `r10_relay_preserves_bytes_and_release_boundary` (F2): the bounded
//!   control-socket relay obeys the seven O2 contracts, and the frozen baseline
//!   relay is shown to violate them (early EOF, early bytes, lost tail) in the
//!   same Unix receiver, the same input bytes and the same release driver.
//!   No FFmpeg and no endpoint are involved in that case. R11-F2 adds full byte
//!   identity: the withheld Register response and its order-distinguishable tail
//!   must arrive as the exact byte string the endpoint sent, so a relay that
//!   keeps length, timing and markers but changes those bytes fails. The
//!   `OBSCURA_R11_F2_RELAY=tail-reorder` red control drives a real relay that
//!   preserves all of those and reverses only the withheld tail.
//! * `r10_held_ack_requires_adapter_owned_completion` (F3): the held-ack
//!   completion arrangement keeps the unreaped leader a zombie owned by the
//!   still-live adapter until the acknowledgement is explicitly released inside
//!   the adapter's pending response-read window, and only then releases the
//!   endpoint reference. `R10_HELD_ACK_ARRANGEMENT=legacy` switches only that
//!   arrangement to the historical one and must fail the same assertions.
//! * `real_endpoint_adapter_startup_failure_cleans_up_relay` (F4) is extended
//!   with external resource checks for the accept stage and for a held-stage
//!   cancellation.
//! * `r10_accept_stage_cancellation_leaves_no_live_resource` (F4, R13): the
//!   accept stage reached by the case above, but cancelled instead of cleaned
//!   up normally. The cleanup owner is moved into the future that is dropped
//!   while the relay is still blocked in `accept()`, so the `Drop` backstop is
//!   the only code that can still run. The live-resource checks and the
//!   recovery-record retention checks run from outside that cancelled future.

use std::{cell::{Cell, RefCell}, path::{Path, PathBuf}, process::Stdio, sync::Arc, time::Duration};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use obscura_media::encoder_owner::{EncoderControlRequest, EncoderControlResponse, EncoderPermit, EncoderSettlement};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};

type Socket = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

#[tokio::test]
async fn real_endpoint_round_trip_produces_access_unit() {
    tokio::time::timeout(Duration::from_secs(180), round_trip())
        .await
        .expect("F5 round-trip case timed out")
        .expect("F5 round-trip case failed");
}

#[tokio::test]
async fn real_endpoint_armed_authority_signals_registered_group() {
    tokio::time::timeout(Duration::from_secs(180), armed_authority())
        .await
        .expect("F5 armed-authority case timed out")
        .expect("F5 armed-authority case failed");
}

struct Fixture {
    root: PathBuf,
    ca_der: CertificateDer<'static>,
    client_der: CertificateDer<'static>,
    client_key: rcgen::KeyPair,
    session_id: RefCell<Option<String>>,
    address: Cell<Option<std::net::SocketAddr>>,
}

impl Fixture {
    fn new(prefix: &str) -> anyhow::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let root = PathBuf::from("/tmp").join(format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        let ca_key = rcgen::KeyPair::generate()?;
        let mut ca_params = rcgen::CertificateParams::new(vec![])?;
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura b10 CA");
        let ca = ca_params.self_signed(&ca_key)?;
        let server_key = rcgen::KeyPair::generate()?;
        let server = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?
            .signed_by(&server_key, &ca, &ca_key)?;
        let client_key = rcgen::KeyPair::generate()?;
        let mut client_params = rcgen::CertificateParams::new(vec![])?;
        client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let client = client_params.signed_by(&client_key, &ca, &ca_key)?;
        std::fs::write(root.join("server.der"), server.der())?;
        std::fs::write(root.join("server.key"), server_key.serialize_der())?;
        std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(root.join("ca.der"), ca.der())?;
        std::fs::write(root.join("client.der"), client.der())?;
        std::fs::write(root.join("client.key"), client_key.serialize_der())?;
        std::fs::set_permissions(root.join("client.key"), std::fs::Permissions::from_mode(0o600))?;
        write_authorized(root.join("registry.json"), root.join("authorized-clients.json"), root.join("profiles"), &client.der().to_vec())?;
        Ok(Self { root, ca_der: ca.der().clone(), client_der: client.der().clone(), client_key, session_id: RefCell::new(None), address: Cell::new(None) })
    }

    fn connector(&self) -> anyhow::Result<tokio_rustls::TlsConnector> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca_der.clone())?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![self.client_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.client_key.serialize_der())),
            )?;
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
    }

    /// Spawn the real Host, attach an agent, resize, and navigate to a static
    /// red square so the exact session always has a frame to encode.
    async fn start_host(&self) -> anyhow::Result<tokio::process::Child> {
        let host = tokio::process::Command::new(host_binary())
            .arg("--socket-dir").arg(self.root.join("host"))
            .arg("--profile-root").arg(self.root.join("profiles"))
            .arg("--authorization-registry").arg(self.root.join("registry.json"))
            .stdout(Stdio::null()).stderr(std::fs::File::create(self.root.join("host.err"))?)
            .kill_on_drop(true).spawn()?;
        wait_socket(self.root.join("host/host.sock")).await;
        let mut local = BufReader::new(tokio::net::UnixStream::connect(self.root.join("host/host.sock")).await?);
        let mut line = String::new();
        local.read_line(&mut line).await?;
        let html = "data:text/html,<style>body{margin:0;background:white}div{width:120px;height:120px;background:red}</style><div></div>";
        let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
        let session_id = state["session_id"].as_str().expect("Host session identity missing").to_string();
        state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
        let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
        drop(local);
        self.session_id.replace(Some(session_id));
        Ok(host)
    }

    fn session_id(&self) -> String {
        self.session_id.borrow().clone().expect("session id set after start_host")
    }

    fn endpoint(&self, ffmpeg: Option<&PathBuf>) -> anyhow::Result<tokio::process::Child> {
        let media_bin = release_binary("obscura-media");
        anyhow::ensure!(media_bin.is_file(), "OBSCURA_TEST_BINARY_DIR must contain obscura-media: {}", media_bin.display());
        self.endpoint_with_media_bin(&media_bin, ffmpeg)
    }

    /// Spawn the real endpoint with an explicit `--media-bin`. The F4.1 harness
    /// passes a bounded pass-through relay here so it can hold the `Register`
    /// response at the actual control boundary.
    fn endpoint_with_media_bin(&self, media_bin: &std::path::Path, ffmpeg: Option<&PathBuf>) -> anyhow::Result<tokio::process::Child> {
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let address = reservation.local_addr()?;
        drop(reservation);
        self.address.replace(Some(address));
        anyhow::ensure!(media_bin.is_file(), "OBSCURA_TEST_BINARY_DIR must contain obscura-media: {}", media_bin.display());
        let mut command = tokio::process::Command::new(endpoint_binary());
        command.arg("--listen").arg(address.to_string())
            .arg("--host-dir").arg(self.root.join("host"))
            .arg("--socket-dir").arg(self.root.join("endpoint"))
            .arg("--server-cert").arg(self.root.join("server.der"))
            .arg("--server-key").arg(self.root.join("server.key"))
            .arg("--client-ca").arg(self.root.join("ca.der"))
            .arg("--authorized-clients").arg(self.root.join("authorized-clients.json"))
            .arg("--media-bin").arg(&media_bin);
        if let Some(ffmpeg) = ffmpeg { command.arg("--ffmpeg").arg(ffmpeg); }
        command.stdout(Stdio::null()).stderr(std::fs::File::create(self.root.join("endpoint.err"))?)
            .kill_on_drop(true).spawn().map_err(Into::into)
    }

    fn address(&self) -> std::net::SocketAddr {
        self.address.get().expect("endpoint address set after endpoint()")
    }
}


async fn round_trip() -> anyhow::Result<()> {
    let fixture = Fixture::new("ob10a")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;
    let mut endpoint = fixture.endpoint(None)?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    let mut media = open_media(&connector, fixture.address(), &token).await?;
    let header = tokio::time::timeout(Duration::from_secs(60), next_access_unit(&mut media)).await
        .map_err(|_| anyhow::anyhow!("the real endpoint never delivered an encoded access unit"))??;
    anyhow::ensure!(header["type"] == "access_unit", "first media packet was not an access unit: {header}");
    eprintln!("F5 round trip: real endpoint + real adapter produced a real access unit");

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

async fn armed_authority() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("ob10b")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;

    // A real encoder group helper: it records its own process-group identity and
    // the adapter parent, records every signal it receives, and blocks without
    // producing an access unit. Blocking keeps the adapter's registration
    // `Armed` (mid encode) so the endpoint finalizer, not the adapter, settles
    // the group.
    let group_pgid = root.join("group.pgid");
    let adapter_pid_file = root.join("adapter.pid");
    let group_signals = root.join("group.signals");
    let helper = root.join("group.sh");
    let script = format!(
        "#!/bin/sh\nppid=$(ps -o ppid= -p $$ | tr -d ' ')\nprintf '%s\\n' \"$$\" > '{pgid}'\nprintf '%s\\n' \"$ppid\" > '{adapter}'\ntrap 'printf \"15\\n\" >> \"{signals}\"; exit 0' TERM\nwhile :; do sleep 0.2; done\n",
        pgid = group_pgid.display(),
        adapter = adapter_pid_file.display(),
        signals = group_signals.display(),
    );
    std::fs::write(&helper, script)?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;

    let mut endpoint = fixture.endpoint(Some(&helper))?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    // The real adapter spawns the group only after the endpoint admitted the
    // session and streamed the latest frame to it.
    wait_file(&group_pgid, Duration::from_secs(60)).await?;
    wait_file(&adapter_pid_file, Duration::from_secs(15)).await?;
    let adapter_pid: i32 = wait_pid_file(&adapter_pid_file, Duration::from_secs(15)).await?;
    anyhow::ensure!(process_alive(adapter_pid), "the real adapter parent was not alive");
    // Let the adapter send `Register` (the endpoint commits `Armed`) and enter
    // the blocked encode. It has not sent any revoke yet.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pause the real adapter so the only process that can signal the registered
    // group is the endpoint's own `Armed` authority.
    anyhow::ensure!(unsafe { libc::kill(adapter_pid, libc::SIGSTOP) } == 0, "SIGSTOP the real adapter");

    // Release the final control reference: the endpoint runs the exact-session
    // finalizer. It must settle the registered group through its own `Armed`
    // authority (no adapter cooperation, since the adapter is paused).
    control.send(Message::Close(None)).await?;
    drop(control);

    let signals = wait_signals(&group_signals, Duration::from_secs(20)).await?;
    anyhow::ensure!(signals.trim() == "15",
        "F5: the endpoint's Armed authority did not deliver exactly one SIGTERM to the registered group: {signals:?}");
    anyhow::ensure!(!process_group_alive(read_pgid(&group_pgid)?), "F5: the registered group survived the endpoint finalizer");

    let session_dir = root.join(format!("endpoint/session-{}", fixture.session_id()));
    wait_dir_gone(&session_dir, Duration::from_secs(20)).await?;
    eprintln!("F5 armed authority: endpoint settled the registered group through its own Armed state");

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn read_pgid(path: &std::path::Path) -> anyhow::Result<i32> {
    Ok(std::fs::read_to_string(path)?.trim().parse()?)
}

/// Wait until a pid record exists and parses as an integer. A pid file is
/// created by its writer before the integer is written, so existence alone can
/// race under load: read the record until it parses, then return it.
async fn wait_pid_file(path: &std::path::Path, timeout: Duration) -> anyhow::Result<i32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<i32>() { return Ok(pid); }
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "pid record never appeared: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// R8-F2/R10-F2: the bounded pass-through relay on the per-session
/// encoder-control socket. It forwards every byte unchanged in both directions
/// and withholds only the endpoint's `Register` response until `ack.release`
/// appears.
///
/// The seven O2 contracts this program implements, one implementation, one
/// endpoint-to-adapter output owner:
///
/// 1. `send_downstream` is the only writer of the adapter side; the reverse
///    thread only reads the adapter and writes the endpoint.
/// 2. The withheld `Register` response and every later tail byte stay in
///    `held`/`buffer` as raw bytes and are replayed in their original order.
/// 3. An upstream EOF only records `endpoint-closed` (and the `eof` marker). It
///    is never an implicit release.
/// 4. Only the explicit `ack.release` file releases the withheld bytes.
/// 5. `delivered` is recorded only after the send returned; the EOF is
///    propagated last, after delivery.
/// 6. A failed downstream send surfaces the original `OSError` and records no
///    `delivered`.
/// 7. The pre-release deadline produces only a failure: no delivery, no
///    success EOF.
///
/// Every event is appended under one lock with a monotonic sequence, so the
/// recorded order is the real wire order. Argument order:
/// `<listen> <upstream> <ready> <held> <release> <closed> <log> <eof>
/// <delivered> <release-deadline-epoch-seconds>`.
const RELAY_PY: &str = r#"import json, os, socket, sys, threading, time

listen_path, upstream_path = sys.argv[1], sys.argv[2]
ready_path, held_path, release_path, closed_path, log_path, eof_path, delivered_path = sys.argv[3:10]
release_deadline = float(sys.argv[10])

lock = threading.Lock()
seq = [0]

def note(message):
    with lock:
        seq[0] += 1
        with open(log_path, 'a') as handle:
            handle.write('%06d %s\n' % (seq[0], message))

def marker(path, text):
    tmp = path + '.tmp'
    with open(tmp, 'w') as handle:
        handle.write(text)
    os.replace(tmp, path)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(listen_path)
srv.listen(1)
marker(ready_path, 'ready')
conn, _ = srv.accept()
up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
connect_deadline = time.time() + 20
while True:
    try:
        up.connect(upstream_path)
        break
    except OSError:
        if time.time() > connect_deadline:
            raise
        time.sleep(0.02)
up.settimeout(0.05)

def send_downstream(payload):
    # Contract 1: the one endpoint-to-adapter output owner. Contract 5: the
    # caller records `delivered` only after this returned.
    conn.sendall(payload)

def adapter_to_endpoint():
    while True:
        try:
            data = conn.recv(4096)
        except OSError:
            data = b''
        if not data:
            note('adapter-closed')
            try:
                up.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            return
        note('a->e ' + repr(data))
        try:
            up.sendall(data)
        except OSError as exc:
            note('upstream-write-error %s errno=%s' % (type(exc).__name__, getattr(exc, 'errno', None)))
            return

def main():
    held = None
    buffer = bytearray()
    delivered = False
    eof = False
    release_seen = os.path.exists(release_path)
    if release_seen:
        note('release-observed-before-held')

    def deliver():
        # Contracts 2, 4 and 5: raw withheld bytes plus every later tail byte,
        # in their original order, sent only after the explicit release, with
        # `delivered` recorded after the send succeeded.
        nonlocal held, buffer, delivered
        if held is None or delivered:
            return
        send_downstream(held)
        if buffer:
            tail = bytes(buffer)
            send_downstream(tail)
            note('tail bytes=%d' % len(tail))
        delivered = True
        marker(delivered_path, 'delivered')
        note('delivered ' + repr(held))
        buffer = bytearray()

    while True:
        if not release_seen and os.path.exists(release_path):
            release_seen = True
            note('release-observed')
        if release_seen and held is not None and not delivered:
            deliver()
        if eof:
            if not delivered:
                if held is None:
                    note('eof-without-held')
                    return 2
                # Contract 3: EOF only records that the upstream closed. It is
                # never an implicit release. Contract 7: the pre-release
                # deadline produces only a failure, never a delivery or a
                # success EOF.
                while not release_seen:
                    if time.time() > release_deadline:
                        note('release-deadline-exceeded')
                        return 2
                    if os.path.exists(release_path):
                        release_seen = True
                        note('release-observed')
                        break
                    time.sleep(0.02)
                deliver()
            try:
                conn.shutdown(socket.SHUT_WR)
            except OSError as exc:
                # The downstream is already gone; delivery already succeeded, so
                # the EOF propagation is moot rather than an original error.
                note('eof-forward-error %s errno=%s' % (type(exc).__name__, getattr(exc, 'errno', None)))
            marker(closed_path, 'closed')
            note('eof-forwarded')
            return 0
        try:
            data = up.recv(4096)
        except socket.timeout:
            continue
        except OSError as exc:
            note('upstream-read-error %s errno=%s' % (type(exc).__name__, getattr(exc, 'errno', None)))
            return 3
        if not data:
            eof = True
            note('endpoint-closed')
            marker(eof_path, 'observed')
            continue
        buffer.extend(data)
        if held is None:
            # Contract 1: every complete endpoint frame is consumed in this
            # iteration. One recv may carry more than one frame, and a later
            # frame must not wait for a byte that may never arrive.
            while held is None:
                newline = buffer.find(b'\n')
                if newline == -1:
                    break
                line = bytes(buffer[:newline + 1])
                buffer = bytearray(buffer[newline + 1:])
                try:
                    frame = json.loads(line.decode('utf-8'))
                except Exception:
                    frame = {}
                if isinstance(frame, dict) and frame.get('op') == 'register':
                    held = line
                    note('hold ' + repr(line))
                    marker(held_path, 'held')
                else:
                    send_downstream(line)
                    note('e->a ' + repr(line))
        elif delivered:
            # Contract 4: after the release every later endpoint frame is
            # forwarded by the same single owner, in order, and recorded as an
            # endpoint->adapter frame so the consumer's answers stay observable.
            frame = bytes(buffer)
            buffer = bytearray()
            send_downstream(frame)
            note('e->a ' + repr(frame))

# Contract 1: the adapter->endpoint direction has exactly one reader and the
# endpoint->adapter direction has exactly one writer, owned by this thread.
# main() never writes to `conn` directly, so ordering is decided in one place.
threading.Thread(target=adapter_to_endpoint, daemon=True).start()

try:
    code = main()
except OSError as exc:
    # Contract 6: the original downstream error is surfaced and no `delivered`
    # record is written for the failed send.
    note('downstream-error %s errno=%s strerror=%s repr=%r' % (
        type(exc).__name__, getattr(exc, 'errno', None), getattr(exc, 'strerror', None), exc))
    code = 3
finally:
    for sock in (conn, up, srv):
        try:
            sock.close()
        except OSError:
            pass
    note('sockets-closed')
sys.exit(code)
"#;

/// R11-F2 red control: the one line of the candidate relay that delivers the
/// withheld tail bytes, and the same line with the withheld tail reversed. The
/// red control below replaces exactly this line, so the program under test in the
/// red run differs from the candidate by one line and nothing else.
const RELAY_TAIL_SEND_LINE: &str = "            send_downstream(tail)\n";
const RELAY_TAIL_REORDER_LINE: &str = "            send_downstream(tail[::-1])\n";

/// R11-F2 red control: the candidate relay with exactly one change in its single
/// endpoint-to-adapter output owner — the withheld tail is delivered in reversed
/// order. Every byte count, every marker, every recorded event and the
/// response-then-tail delivery order stay identical, so a relay that keeps length
/// and timing but changes the withheld bytes is invisible to a length-only
/// assertion. The derivation is exact and self-checked: replacing that one line
/// back reproduces `RELAY_PY` byte for byte.
fn relay_tail_reorder_source() -> anyhow::Result<String> {
    anyhow::ensure!(
        RELAY_PY.matches(RELAY_TAIL_SEND_LINE).count() == 1,
        "the candidate relay no longer has exactly one withheld-tail send line to derive the red control from"
    );
    let source = RELAY_PY.replacen(RELAY_TAIL_SEND_LINE, RELAY_TAIL_REORDER_LINE, 1);
    anyhow::ensure!(
        source.replacen(RELAY_TAIL_REORDER_LINE, RELAY_TAIL_SEND_LINE, 1) == RELAY_PY,
        "the red-control relay is not the candidate relay with exactly one changed line"
    );
    Ok(source)
}

/// R11-F2/R15-F2: which real relay program the F2 contract case drives. Unset —
/// every gate run — drives the candidate relay, the program the held-ack case
/// drives at the real control boundary. `tail-reorder` drives the red control
/// above: a real relay program that preserves length, timing, markers and event
/// order and changes only the withheld bytes. `baseline` drives the *historical*
/// relay baseline, the pre-fix program the plan's 「必需判别」 names: it is run
/// through the same harness and judged by the same `assert_candidate` contract,
/// so the case exits non-zero because the historical relay violates the
/// candidate contract (an early EOF, early bytes or lost bytes), never merely
/// because `deviated()` observed a deviation. Only the program under test
/// changes; the harness, the input bytes, the release driver and the assertions
/// are identical.
fn f2_relay_under_test() -> anyhow::Result<(String, &'static str)> {
    match std::env::var("OBSCURA_R11_F2_RELAY").as_deref() {
        Ok("tail-reorder") => Ok((relay_tail_reorder_source()?, "tail-reorder-red-control")),
        Ok("baseline") => Ok((RELAY_PY_BASELINE.to_owned(), "baseline-red-control")),
        _ => Ok((RELAY_PY.to_owned(), "candidate")),
    }
}

/// R10-F2 control: the relay exactly as the frozen `a6b1d966` baseline carries
/// it, copied verbatim from
/// `m1-p3-r10-before/crates/obscura-host/tests/blackbox_b10_real_endpoint_adapter.rs`
/// (sha256 `2b5c41cb9f09286c6c9ab2435cf61d8d0d80cc0b0f046b034ea3eca00a70fc3f`).
/// It is the pre-fix object under comparison, never the object under test: it
/// drops the withheld response and every tail byte when the endpoint closes
/// while the acknowledgement is held, and it propagates that close to the
/// adapter before any release. The F2 case runs it through the same Unix
/// receiver, the same input bytes and the same release driver, and requires the
/// harness to observe exactly those deviations.
const RELAY_PY_BASELINE: &str = r#"import json, os, socket, sys, threading, time

listen_path, upstream_path = sys.argv[1], sys.argv[2]
ready_path, held_path, release_path, closed_path, log_path = sys.argv[3:8]

def note(message):
    with open(log_path, 'a') as handle:
        handle.write(message + '\n')

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(listen_path)
srv.listen(1)
with open(ready_path, 'w') as handle:
    handle.write('ready')
conn, _ = srv.accept()
up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
deadline = time.time() + 20
while True:
    try:
        up.connect(upstream_path)
        break
    except OSError:
        if time.time() > deadline:
            raise
        time.sleep(0.02)

def adapter_to_endpoint():
    while True:
        try:
            data = conn.recv(4096)
        except OSError:
            data = b''
        if not data:
            note('adapter-closed')
            try:
                up.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            return
        note('a->e ' + repr(data))
        try:
            up.sendall(data)
        except OSError:
            return

held = False

def endpoint_to_adapter():
    global held
    buffer = b''
    while True:
        try:
            data = up.recv(4096)
        except OSError:
            data = b''
        if not data:
            note('endpoint-closed')
            with open(closed_path, 'w') as handle:
                handle.write('closed')
            try:
                conn.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            return
        buffer += data
        while b'\n' in buffer:
            line, buffer = buffer.split(b'\n', 1)
            payload = line + b'\n'
            hold = False
            try:
                frame = json.loads(line.decode('utf-8'))
                hold = frame.get('op') == 'register'
            except Exception:
                pass
            if hold and not held:
                held = True
                note('hold ' + repr(payload))
                with open(held_path, 'w') as handle:
                    handle.write('held')
                while not os.path.exists(release_path):
                    up.settimeout(0.02)
                    try:
                        more = up.recv(4096)
                    except socket.timeout:
                        more = None
                    except OSError:
                        more = b''
                    up.settimeout(None)
                    if more == b'':
                        note('endpoint-closed-while-held')
                        with open(closed_path, 'w') as handle:
                            handle.write('closed')
                        try:
                            conn.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
                        return
                    if more:
                        buffer += more
                note('release ' + repr(payload))
            else:
                note('e->a ' + repr(payload))
            try:
                conn.sendall(payload)
            except OSError:
                return

threading.Thread(target=adapter_to_endpoint, daemon=True).start()
endpoint_to_adapter()
"#;

/// The file names the relay harness binds inside one scratch root.
struct RelayPaths {
    ready: PathBuf,
    held: PathBuf,
    release: PathBuf,
    closed: PathBuf,
    eof: PathBuf,
    delivered: PathBuf,
    log: PathBuf,
    pid: PathBuf,
    wrapper: PathBuf,
    wrapper_ready: PathBuf,
}

/// The release deadline the harness relay is given. It is long enough that a
/// test which releases the acknowledgement normally never reaches it, and it is
/// the only bound on the relay's pre-release wait (contract 7).
const RELAY_RELEASE_DEADLINE_SECONDS: u64 = 120;

/// R8-F2/R8-F4/R10-F2: build the bounded control-socket relay harness the
/// endpoint spawns as `--media-bin`. The wrapper renames the endpoint's real
/// per-session control socket aside, starts the relay at the original path,
/// waits for the relay to bind, then runs `final_exec`. The endpoint's direct
/// child stays the process `final_exec` names: the real adapter for the
/// held-ack case, or an immediately failing binary for the adapter-startup-
/// failure case.
fn write_relay_harness(root: &std::path::Path, final_exec: &str) -> anyhow::Result<RelayPaths> {
    use std::os::unix::fs::PermissionsExt;
    let paths = RelayPaths {
        ready: root.join("relay.ready"),
        held: root.join("ack.held"),
        release: root.join("ack.release"),
        closed: root.join("conn.closed"),
        eof: root.join("conn.eof"),
        delivered: root.join("ack.delivered"),
        log: root.join("relay.log"),
        pid: root.join("relay.pid"),
        wrapper: root.join("adapter-wrapper.sh"),
        wrapper_ready: root.join("wrapper.ready"),
    };
    let relay = root.join("relay.py");
    std::fs::write(&relay, RELAY_PY)?;
    let deadline = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()
        + RELAY_RELEASE_DEADLINE_SECONDS;
    std::fs::write(&paths.wrapper, format!(
        "#!/bin/sh\n\
         video=''\n\
         prev=''\n\
         for arg in \"$@\"; do\n\
           if [ \"$prev\" = '--video-socket' ]; then video=\"$arg\"; fi\n\
           prev=\"$arg\"\n\
         done\n\
         session=$(dirname \"$video\")\n\
         control=\"$session/encoder-control.sock\"\n\
         realcontrol='{realcontrol}'\n\
         i=0\n\
         while [ ! -S \"$control\" ]; do\n\
           i=$((i+1)); if [ \"$i\" -gt 500 ]; then echo 'control socket never bounded' >&2; exit 9; fi\n\
           sleep 0.02\n\
         done\n\
         mv \"$control\" \"$realcontrol\"\n\
         python3 '{relay}' \"$control\" \"$realcontrol\" '{ready}' '{held}' '{release}' '{closed}' '{log}' '{eof}' '{delivered}' '{deadline}' &\n\
         echo $! > '{relay_pid}'\n\
         i=0\n\
         while [ ! -S \"$control\" ]; do\n\
           i=$((i+1)); if [ \"$i\" -gt 500 ]; then echo 'relay never bounded' >&2; exit 9; fi\n\
           sleep 0.02\n\
         done\n\
         : > '{wrapper_ready}'\n\
         {final_exec}\n",
        relay = relay.display(), ready = paths.ready.display(), held = paths.held.display(),
        release = paths.release.display(), closed = paths.closed.display(), log = paths.log.display(),
        eof = paths.eof.display(), delivered = paths.delivered.display(), deadline = deadline,
        realcontrol = root.join("control.real").display(),
        relay_pid = paths.pid.display(), wrapper_ready = paths.wrapper_ready.display(),
        final_exec = final_exec,
    ))?;
    std::fs::set_permissions(&paths.wrapper, std::fs::Permissions::from_mode(0o755))?;
    Ok(paths)
}

/// R8-F4: the single cleanup owner for the relay harness. It is created before
/// the endpoint launches the wrapper `--media-bin`, records every pid the
/// harness starts, and is the only authority that terminates them. It covers
/// the success path, every early error before the final assertion, and
/// outer-timeout cancellation through `Drop`. Pid records are retained until
/// termination is verified, so a failed cleanup leaves the owner armed for the
/// `Drop` backstop.
struct AdapterHarnessCleanup {
    label: &'static str,
    root: PathBuf,
    relay_pid_file: PathBuf,
    adapter_pid_file: PathBuf,
    endpoint: Option<tokio::process::Child>,
    host: Option<tokio::process::Child>,
    relay_pid: Option<i32>,
    adapter_pid: Option<i32>,
    group_pgid: Option<i32>,
    armed: bool,
}

impl AdapterHarnessCleanup {
    fn new(label: &'static str, root: PathBuf, relay_pid_file: PathBuf, adapter_pid_file: PathBuf) -> Self {
        Self {
            label, root, relay_pid_file, adapter_pid_file,
            endpoint: None, host: None, relay_pid: None, adapter_pid: None, group_pgid: None, armed: true,
        }
    }

    fn own_endpoint(&mut self, child: tokio::process::Child) { self.endpoint = Some(child); }
    fn own_host(&mut self, child: tokio::process::Child) { self.host = Some(child); }
    fn record_relay_pid(&mut self, pid: i32) { self.relay_pid = Some(pid); }
    fn record_adapter_pid(&mut self, pid: i32) { self.adapter_pid = Some(pid); }
    fn record_group_pgid(&mut self, pgid: i32) { self.group_pgid = Some(pgid); }

    fn endpoint_pid(&self) -> Option<i32> {
        self.endpoint.as_ref().and_then(|child| child.id()).and_then(|pid| i32::try_from(pid).ok())
    }

    fn host_pid(&self) -> Option<i32> {
        self.host.as_ref().and_then(|child| child.id()).and_then(|pid| i32::try_from(pid).ok())
    }

    /// Read a pid record written by a harness child. The record stays on disk
    /// until termination is verified, so a later retry can still use it.
    fn read_recorded_pid(path: &std::path::Path) -> Option<i32> {
        std::fs::read_to_string(path).ok()?.trim().parse::<i32>().ok()
    }

    fn recorded_targets(&self) -> Vec<(&'static str, i32)> {
        let mut targets = Vec::new();
        if let Some(pid) = self.relay_pid.or_else(|| Self::read_recorded_pid(&self.relay_pid_file)) {
            targets.push(("relay", pid));
        }
        if let Some(pid) = self.adapter_pid.or_else(|| Self::read_recorded_pid(&self.adapter_pid_file)) {
            targets.push(("adapter", pid));
        }
        targets
    }

    /// Terminate every recorded pid and remove the scratch root. It returns an
    /// error and leaves the owner armed when a recorded pid cannot be verified
    /// gone, so the `Drop` backstop can still finish the job.
    async fn cleanup(&mut self) -> anyhow::Result<()> {
        let mut failures: Vec<String> = Vec::new();
        for (name, pid) in self.recorded_targets() {
            if !process_alive(pid) { continue; }
            // Safety: signal the exact pid this harness recorded.
            unsafe { libc::kill(pid, libc::SIGTERM); }
            if !wait_process_gone(pid, Duration::from_secs(5)).await {
                // Safety: signal the exact pid this harness recorded.
                unsafe { libc::kill(pid, libc::SIGKILL); }
                if !wait_process_gone(pid, Duration::from_secs(5)).await {
                    failures.push(format!("{name} pid {pid} survived SIGKILL"));
                }
            }
        }
        if let Some(pgid) = self.group_pgid {
            if process_group_alive(pgid) {
                // Safety: signal the exact process group this harness recorded.
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
            }
        }
        if let Some(child) = self.endpoint.as_mut() { let _ = child.kill().await; let _ = child.wait().await; }
        if let Some(child) = self.host.as_mut() { let _ = child.kill().await; let _ = child.wait().await; }
        if !failures.is_empty() {
            return Err(anyhow::anyhow!("{} cleanup incomplete: {}", self.label, failures.join("; ")));
        }
        self.armed = false;
        let _ = std::fs::remove_dir_all(&self.root);
        Ok(())
    }
}

impl Drop for AdapterHarnessCleanup {
    fn drop(&mut self) {
        // Cancellation backstop. It fires only while this owner still owes a
        // termination, and it signals only the pids this harness recorded.
        if !self.armed { return; }
        for (_, pid) in self.recorded_targets() {
            if process_alive(pid) {
                // Safety: signal the exact pid this harness recorded.
                unsafe { libc::kill(pid, libc::SIGKILL); }
            }
        }
        if let Some(pgid) = self.group_pgid {
            if process_group_alive(pgid) {
                // Safety: signal the exact process group this harness recorded.
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
            }
        }
        if let Some(child) = self.endpoint.as_mut() { let _ = child.start_kill(); }
        if let Some(child) = self.host.as_mut() { let _ = child.start_kill(); }
    }
}

/// R10-F2: the relay's seven contracts, run without FFmpeg and without an
/// endpoint.
///
/// One Unix receiver plays the endpoint, one plays the adapter, and the same
/// input bytes and the same `ack.release` file drive every run. The candidate
/// relay — the program the held-ack case drives at the real control boundary —
/// must satisfy all seven. The frozen baseline relay is run through the same
/// harness and must be observed to violate them; that is the F2 red control.
/// Every candidate failure is a behaviour assertion: bytes before the release,
/// an EOF before the release, or withheld bytes lost.
#[tokio::test]
async fn r10_relay_preserves_bytes_and_release_boundary() {
    tokio::time::timeout(Duration::from_secs(180), relay_contract())
        .await
        .expect("r10 F2 relay case timed out")
        .expect("r10 F2 relay case failed");
}

/// The first offset where two byte strings differ, or `None` when they are equal.
/// It names the exact position the byte-identity assertion failed at.
fn first_difference(received: &[u8], expected: &[u8]) -> Option<usize> {
    received.iter().zip(expected.iter()).position(|(left, right)| left != right)
        .or_else(|| (received.len() != expected.len()).then_some(received.len().min(expected.len())))
}

#[derive(Debug)]
struct RelayRun {
    mode: &'static str,
    exit_code: Option<i32>,
    received: Vec<u8>,
    /// The complete byte string the endpoint sent through the relay, in order.
    /// Fidelity is asserted against these bytes, never against their length.
    expected: Vec<u8>,
    bytes_before_release: bool,
    eof_before_release: bool,
    eof_after_bytes: bool,
    delivered_marker: bool,
    closed_marker: bool,
    events: Vec<String>,
}

impl RelayRun {
    /// The candidate contract for one mode. Every failure names the observable
    /// behaviour that was violated.
    fn assert_candidate(&self) -> anyhow::Result<()> {
        match self.mode {
            "eof-before-release" | "release-then-eof" => {
                anyhow::ensure!(!self.bytes_before_release,
                    "the relay forwarded {} byte(s) to the adapter before the explicit release", self.received.len());
                anyhow::ensure!(!self.eof_before_release,
                    "the relay propagated the upstream EOF to the adapter before the explicit release");
                anyhow::ensure!(self.received.len() == self.expected.len(),
                    "the relay delivered {} byte(s) instead of the withheld {} byte(s): the withheld response or its tail bytes were lost",
                    self.received.len(), self.expected.len());
                // R11-F2: length is not fidelity. The complete withheld byte
                // string must arrive in its original order, so a relay that
                // substitutes the Register response or reorders the tail while
                // keeping length, timing and markers is caught here.
                anyhow::ensure!(self.received == self.expected,
                    "the relay delivered {} byte(s) with the withheld length but different content: first difference at offset {:?} (received[..16]={:?} expected[..16]={:?})",
                    self.received.len(), first_difference(&self.received, &self.expected),
                    &self.received[..self.received.len().min(16)], &self.expected[..self.expected.len().min(16)]);
                anyhow::ensure!(self.eof_after_bytes,
                    "the relay never propagated the upstream EOF after the release");
                anyhow::ensure!(self.exit_code == Some(0),
                    "the relay exited {:?} instead of 0 on the released boundary", self.exit_code);
                anyhow::ensure!(self.delivered_marker, "the relay never recorded `delivered` after a successful send");
                let delivered = event_index(&self.events, "delivered ").ok_or_else(|| anyhow::anyhow!("no delivered event"))?;
                let forwarded = event_index(&self.events, "eof-forwarded").ok_or_else(|| anyhow::anyhow!("no eof-forwarded event"))?;
                anyhow::ensure!(delivered < forwarded, "the relay propagated EOF before it recorded the delivery: {:?}", self.events);
            }
            "downstream-closed" => {
                anyhow::ensure!(self.exit_code == Some(3),
                    "the relay exited {:?} instead of surfacing the original downstream error", self.exit_code);
                anyhow::ensure!(!self.delivered_marker,
                    "the relay recorded `delivered` for a send that failed");
                anyhow::ensure!(event_line(&self.events, "downstream-error").is_some_and(|line| line.contains("errno=32")),
                    "the relay did not surface the original broken-pipe error: {:?}", self.events);
            }
            "deadline" => {
                anyhow::ensure!(self.exit_code == Some(2),
                    "the relay exited {:?} instead of failing on the pre-release deadline", self.exit_code);
                anyhow::ensure!(!self.delivered_marker, "the pre-release deadline produced a delivery");
                anyhow::ensure!(!self.closed_marker, "the pre-release deadline produced a success EOF");
                anyhow::ensure!(event_line(&self.events, "release-deadline-exceeded").is_some(),
                    "the relay did not record the pre-release deadline failure: {:?}", self.events);
            }
            other => anyhow::bail!("unknown relay contract mode {other}"),
        }
        Ok(())
    }

    /// The baseline deviation the F2 red control must observe: an EOF before the
    /// release, bytes before the release, or withheld bytes that are lost or
    /// changed.
    fn deviated(&self) -> bool {
        self.bytes_before_release || self.eof_before_release || self.received != self.expected
    }
}

async fn relay_contract() -> anyhow::Result<()> {
    // R11-F2: the program under test is selected here and nowhere else. The
    // default is the candidate relay; the red control swaps in the
    // length-preserving corruption fixture through the same harness.
    let (relay_source, relay_label) = f2_relay_under_test()?;
    let mut failures: Vec<String> = Vec::new();
    for mode in ["eof-before-release", "release-then-eof", "downstream-closed", "deadline"] {
        let run = relay_run(&relay_source, relay_label, mode).await?;
        eprintln!("R10-F2-RELAY label={relay_label} mode={mode} exit={:?} received={} expected={} first_difference={:?} bytes_before_release={} eof_before_release={} eof_after_bytes={} delivered={} closed={}",
            run.exit_code, run.received.len(), run.expected.len(),
            first_difference(&run.received, &run.expected),
            run.bytes_before_release, run.eof_before_release,
            run.eof_after_bytes, run.delivered_marker, run.closed_marker);
        if let Err(error) = run.assert_candidate() {
            failures.push(format!("{relay_label}/{mode}: {error}"));
        }
    }

    // F2 red control: the frozen baseline relay through the same harness.
    let baseline = relay_run(RELAY_PY_BASELINE, "baseline", "eof-before-release").await?;
    eprintln!("R10-F2-BASELINE mode=eof-before-release exit={:?} received={} expected={} first_difference={:?} bytes_before_release={} eof_before_release={} eof_after_bytes={} delivered={} closed={}",
        baseline.exit_code, baseline.received.len(), baseline.expected.len(),
        first_difference(&baseline.received, &baseline.expected), baseline.bytes_before_release,
        baseline.eof_before_release, baseline.eof_after_bytes, baseline.delivered_marker, baseline.closed_marker);
    anyhow::ensure!(baseline.deviated(),
        "F2 control: the harness did not observe the baseline relay's early EOF, early bytes or lost bytes: {baseline:?}");

    anyhow::ensure!(failures.is_empty(), "F2: the relay violated its contracts: {}", failures.join(" | "));
    Ok(())
}

async fn relay_run(relay_source: &str, label: &str, mode: &'static str) -> anyhow::Result<RelayRun> {
    let root = PathBuf::from("/tmp").join(format!("r10f2-{label}-{}-{}", mode, &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&root)?;
    let outcome = relay_run_inner(&root, relay_source, mode).await;
    let _ = std::fs::remove_dir_all(&root);
    outcome
}

async fn relay_run_inner(root: &Path, relay_source: &str, mode: &'static str) -> anyhow::Result<RelayRun> {
    let listen = root.join("control.sock");
    let upstream = root.join("upstream.sock");
    let ready = root.join("relay.ready");
    let held = root.join("ack.held");
    let release = root.join("ack.release");
    let closed = root.join("conn.closed");
    let eof = root.join("conn.eof");
    let delivered = root.join("ack.delivered");
    let log = root.join("relay.log");
    let program = root.join("relay.py");
    std::fs::write(&program, relay_source)?;

    let listener = tokio::net::UnixListener::bind(&upstream)?;
    let deadline = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()
        + if mode == "deadline" { 2 } else { 60 };
    let mut relay = tokio::process::Command::new("python3")
        .arg(&program).arg(&listen).arg(&upstream)
        .arg(&ready).arg(&held).arg(&release).arg(&closed).arg(&log).arg(&eof).arg(&delivered).arg(deadline.to_string())
        .stdout(Stdio::from(std::fs::File::create(root.join("relay.out"))?))
        .stderr(Stdio::from(std::fs::File::create(root.join("relay.err"))?))
        .kill_on_drop(true)
        .spawn()?;
    let relay_pid = i32::try_from(relay.id().context("relay pid")?)?;

    // The adapter side of the receiver.
    let mut adapter = wait_unix_connect(&listen, Duration::from_secs(30)).await?;
    // The endpoint side of the receiver.
    let (mut endpoint_side, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept()).await
        .map_err(|_| anyhow::anyhow!("the relay never dialled the endpoint side"))??;

    // The same input bytes for every run.
    let request = b"{\"op\":\"register\",\"registration_id\":1,\"pgid\":1}\n";
    adapter.write_all(request).await?;
    let mut scratch = vec![0u8; request.len()];
    endpoint_side.read_exact(&mut scratch).await?;
    let response = b"{\"op\":\"register\",\"refused\":false}\n";
    // R11-F2: the withheld tail must be order-distinguishable. A single repeated
    // byte makes even a tail reorder invisible to any byte comparison, so the
    // tail is a position-varying sequence of the same length.
    let tail: Vec<u8> = (0..64 * 1024).map(|index| b'A' + (index % 26) as u8).collect();
    let mut expected = response.to_vec();
    expected.extend_from_slice(&tail);
    endpoint_side.write_all(&expected).await?;
    endpoint_side.flush().await?;

    wait_file(&held, Duration::from_secs(30)).await?;
    let early = probe_downstream(&mut adapter, Duration::from_millis(300)).await?;
    let mut bytes_before_release = !early.0.is_empty();
    let mut eof_before_release = early.1;

    let mut received = early.0;
    let mut eof_after_bytes = false;

    let exit_code = match mode {
        "eof-before-release" => {
            endpoint_side.shutdown().await?;
            let observed = wait_endpoint_close_observed(&eof, &mut adapter, Duration::from_secs(20)).await;
            anyhow::ensure!(observed.upstream_recorded || observed.adapter_eof,
                "the harness never observed the endpoint's close through the relay");
            eof_before_release |= observed.adapter_eof;
            if !observed.bytes.is_empty() {
                bytes_before_release = true;
                received.extend_from_slice(&observed.bytes);
            }
            std::fs::write(&release, b"release")?;
            let read = read_downstream_to_eof(&mut adapter, Duration::from_secs(30)).await;
            received.extend_from_slice(&read.0);
            eof_after_bytes = read.1;
            wait_child_exit_code(&mut relay, Duration::from_secs(30)).await
        }
        "release-then-eof" => {
            std::fs::write(&release, b"release")?;
            let read = read_downstream_exactly(&mut adapter, expected.len(), Duration::from_secs(30)).await;
            received.extend_from_slice(&read.0);
            endpoint_side.shutdown().await?;
            let rest = read_downstream_to_eof(&mut adapter, Duration::from_secs(30)).await;
            received.extend_from_slice(&rest.0);
            eof_after_bytes = rest.1;
            wait_child_exit_code(&mut relay, Duration::from_secs(30)).await
        }
        "downstream-closed" => {
            drop(adapter);
            std::fs::write(&release, b"release")?;
            wait_child_exit_code(&mut relay, Duration::from_secs(30)).await
        }
        "deadline" => {
            endpoint_side.shutdown().await?;
            wait_child_exit_code(&mut relay, Duration::from_secs(30)).await
        }
        other => anyhow::bail!("unknown relay contract mode {other}"),
    };
    let _ = relay.kill().await;
    let _ = relay.wait().await;
    anyhow::ensure!(!process_alive(relay_pid), "the F2 relay {relay_pid} survived its own termination");

    Ok(RelayRun {
        mode,
        exit_code,
        received,
        expected,
        bytes_before_release,
        eof_before_release,
        eof_after_bytes,
        delivered_marker: delivered.exists(),
        closed_marker: closed.exists(),
        events: read_relay_events(&log),
    })
}

async fn wait_unix_connect(path: &Path, timeout: Duration) -> anyhow::Result<tokio::net::UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                anyhow::ensure!(tokio::time::Instant::now() < deadline, "the relay never bound {}: {error}", path.display());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Read what the adapter side already has without blocking: `(bytes, eof)`.
async fn probe_downstream(adapter: &mut tokio::net::UnixStream, window: Duration) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut buffer = vec![0u8; 64 * 1024];
    match tokio::time::timeout(window, adapter.read(&mut buffer)).await {
        Ok(Ok(0)) => Ok((Vec::new(), true)),
        Ok(Ok(count)) => Ok((buffer[..count].to_vec(), false)),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Ok((Vec::new(), false)),
    }
}

async fn read_downstream_exactly(adapter: &mut tokio::net::UnixStream, want: usize, timeout: Duration) -> (Vec<u8>, bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut received = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    while received.len() < want {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, adapter.read(&mut buffer)).await {
            Ok(Ok(0)) => return (received, true),
            Ok(Ok(count)) => received.extend_from_slice(&buffer[..count]),
            Ok(Err(_)) | Err(_) => return (received, false),
        }
    }
    (received, false)
}

async fn read_downstream_to_eof(adapter: &mut tokio::net::UnixStream, timeout: Duration) -> (Vec<u8>, bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut received = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, adapter.read(&mut buffer)).await {
            Ok(Ok(0)) => return (received, true),
            Ok(Ok(count)) => received.extend_from_slice(&buffer[..count]),
            Ok(Err(_)) | Err(_) => return (received, false),
        }
    }
}

/// What ended the wait for the endpoint's close edge.
struct CloseObservation {
    /// The relay recorded that the upstream (endpoint) side closed. This is a
    /// record of the close, never a propagation of it to the adapter.
    upstream_recorded: bool,
    /// The adapter side observed the close itself.
    adapter_eof: bool,
    /// Bytes that arrived during the wait, never dropped.
    bytes: Vec<u8>,
}

/// Wait for the relay to have observed the upstream close. The fixed relay
/// records it in its `eof` marker; the frozen baseline only exposes it by
/// propagating the close to the adapter. The two observables are reported
/// separately, because only the second one is an EOF that reached the adapter.
async fn wait_endpoint_close_observed(marker: &Path, adapter: &mut tokio::net::UnixStream, timeout: Duration) -> CloseObservation {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut byte = [0u8; 1];
    let mut bytes = Vec::new();
    loop {
        if marker.exists() {
            return CloseObservation { upstream_recorded: true, adapter_eof: false, bytes };
        }
        match adapter.try_read(&mut byte) {
            Ok(0) => return CloseObservation { upstream_recorded: false, adapter_eof: true, bytes },
            Ok(_) => bytes.push(byte[0]),
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return CloseObservation { upstream_recorded: false, adapter_eof: true, bytes },
        }
        if tokio::time::Instant::now() >= deadline {
            return CloseObservation { upstream_recorded: false, adapter_eof: false, bytes };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_child_exit_code(child: &mut tokio::process::Child, timeout: Duration) -> Option<i32> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => None,
    }
}

/// R10-F3: the held-ack completion arrangement, observed against the real
/// endpoint and the real adapter.
///
/// The endpoint binds `session-<id>/encoder-control.sock` and then spawns
/// `--media-bin`. This case passes a real-adapter wrapper as `--media-bin`.
/// Before it `exec`s the real adapter, the wrapper renames the endpoint's real
/// control socket aside and binds the bounded pass-through relay at the
/// original path. The real adapter connects to the relay; the relay connects to
/// the real endpoint listener and withholds only the endpoint's `Register`
/// response, which is the accepted-but-unconsumed precondition.
///
/// The group helper is the adapter's FFmpeg child: it records its own pgid and
/// its real parent (the adapter), records every signal, and then exits by
/// itself. Because the adapter is blocked in the withheld `Register` exchange,
/// it cannot reap that child, so the leader is an unreaped zombie owned by the
/// still-live adapter — an identity that only the adapter can retire.
///
/// The completion arrangement is the object under test here:
///
/// * the leader must still be that unreaped zombie, and the endpoint must not
///   have published a close edge, while the acknowledgement is unconsumed;
/// * the acknowledgement is released explicitly inside the adapter's existing
///   bounded response-read window (no long sleep is a required step and no
///   product timeout is extended);
/// * the adapter's typed `Revoke` request proves it consumed the endpoint's own
///   `Register` response, and the relay must forward the endpoint's complete
///   `Revoke` response, not just see the request;
/// * the adapter reaps its own leader and exits by itself before the endpoint
///   reference is released, so the endpoint cannot be the one that tore the
///   group down;
/// * the endpoint teardown is last and must not publish the unsettled-permit
///   terminal for a registration the adapter settled on its own.
///
/// `R10_HELD_ACK_ARRANGEMENT=legacy` switches only that arrangement to the
/// historical one (release the endpoint reference first, wait for the
/// endpoint's own retirement edge, then release the acknowledgement). The
/// observer and the assertions do not change, so the legacy run must fail on
/// the ordering assertion.
#[tokio::test]
async fn r10_held_ack_requires_adapter_owned_completion() {
    let legacy = std::env::var("R10_HELD_ACK_ARRANGEMENT").as_deref() == Ok("legacy");
    tokio::time::timeout(Duration::from_secs(180), held_ack_adapter_owned_completion(legacy))
        .await
        .expect("r10 F3 held-ack case timed out")
        .expect("r10 F3 held-ack case failed");
}

async fn held_ack_adapter_owned_completion(legacy: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("r10f3")?;
    let root = fixture.root.clone();

    // The group helper writes its own pgid and the real adapter parent, then
    // exits by itself: the adapter's blocked `Register` exchange keeps the
    // leader an unreaped zombie.
    let group_dir = root.join("group");
    std::fs::create_dir_all(&group_dir)?;
    let group_pgid = group_dir.join("group.pgid");
    let adapter_pid_file = group_dir.join("adapter.pid");
    let group_signals = group_dir.join("group.signals");
    let helper = group_dir.join("group.sh");
    std::fs::write(&helper, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{pgid}'\nprintf '%s\\n' \"$PPID\" > '{adapter}'\ntrap 'printf \"15\\n\" >> \"{signals}\"' TERM\nsleep 0.2\nexit 0\n",
        pgid = group_pgid.display(), adapter = adapter_pid_file.display(), signals = group_signals.display(),
    ))?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;

    let real_adapter = release_binary("obscura-media");
    anyhow::ensure!(real_adapter.is_file(), "OBSCURA_TEST_BINARY_DIR must contain obscura-media: {}", real_adapter.display());
    let paths = write_relay_harness(&root, &format!("exec '{}' \"$@\"", real_adapter.display()))?;

    // The cleanup owner exists before the endpoint launches the wrapper, so
    // every pid this harness starts is owned from the first fallible line,
    // including outer-timeout cancellation.
    let mut cleanup = AdapterHarnessCleanup::new(
        "r10 F3 held-ack", root.clone(), paths.pid.clone(), adapter_pid_file.clone());
    cleanup.own_host(fixture.start_host().await?);
    let session_id = fixture.session_id();
    cleanup.own_endpoint(fixture.endpoint_with_media_bin(&paths.wrapper, Some(&helper))?);
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");
    // Open the endpoint media grant and keep both real references for the whole
    // case: without the media grant the adapter never receives a frame, so it
    // never spawns the group or sends `Register`.
    let _media = open_media(&connector, fixture.address(), &token).await?;

    wait_file(&paths.wrapper_ready, Duration::from_secs(30)).await?;
    let relay_pid: i32 = wait_pid_file(&paths.pid, Duration::from_secs(30)).await?;
    cleanup.record_relay_pid(relay_pid);
    wait_file(&group_pgid, Duration::from_secs(60)).await?;
    wait_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    let pgid = read_pgid(&group_pgid)?;
    let adapter_pid: i32 = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    cleanup.record_group_pgid(pgid);
    cleanup.record_adapter_pid(adapter_pid);

    // Precondition: the relay holds the endpoint's real `Register` response.
    wait_file(&paths.held, Duration::from_secs(30)).await?;
    let held_events = read_relay_events(&paths.log);
    let held_line = event_line(&held_events, "hold ")
        .ok_or_else(|| anyhow::anyhow!("the relay never recorded holding the Register response: {held_events:?}"))?;
    anyhow::ensure!(held_line.contains("register"), "the relay did not hold the endpoint's Register response: {held_line}");
    anyhow::ensure!(event_line(&held_events, "delivered ").is_none(), "the relay delivered the held response before the test released it");

    // Precondition: the leader is an unreaped zombie whose parent is the real,
    // still-live adapter direct child.
    anyhow::ensure!(process_alive(adapter_pid), "the real adapter direct child was not alive");
    anyhow::ensure!(wait_unreaped_zombie(pgid, adapter_pid, Duration::from_secs(20)).await,
        "the group leader never became an unreaped zombie owned by the adapter: {:?}", process_state(pgid));
    let (state, parent) = process_state(pgid).expect("leader state after the zombie precondition");
    eprintln!("R10-F3-PRECONDITION: relay holds the real Register response; leader pid {pgid} state={state} ppid={parent}; adapter {adapter_pid} alive; arrangement={}",
        if legacy { "legacy" } else { "adapter-owned" });

    // Interrupt the real adapter inside the accepted-but-unconsumed window.
    anyhow::ensure!(unsafe { libc::kill(adapter_pid, libc::SIGTERM) } == 0, "SIGTERM the live adapter");

    let mut control = Some(control);
    if legacy {
        // Historical completion arrangement: release the endpoint reference
        // first and wait for the endpoint's own retirement edge. The observer
        // and the assertions below are unchanged.
        if let Some(mut socket) = control.take() { let _ = socket.send(Message::Close(None)).await; }
        wait_relay_event(&paths.log, "endpoint-closed", Duration::from_secs(40)).await?;
    }

    // The correct assertions, with the observer unchanged.
    let events = read_relay_events(&paths.log);
    anyhow::ensure!(event_line(&events, "delivered ").is_none(),
        "F3: the relay delivered the Register acknowledgement before the test released it");
    anyhow::ensure!(event_line(&events, "endpoint-closed").is_none(),
        "F3: the endpoint published its close edge while the Register acknowledgement was still unconsumed");
    anyhow::ensure!(process_alive(adapter_pid),
        "F3: the real adapter disappeared before the Register acknowledgement was released");
    anyhow::ensure!(unreaped_zombie_parented_by(pgid, adapter_pid),
        "F3: the unreaped leader stopped being an adapter-owned zombie before the acknowledgement was released: {:?}", process_state(pgid));

    // The pin must hold for the whole held-ack interval, inside the adapter's
    // existing bounded response-read window.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::ensure!(process_alive(adapter_pid), "F3: the adapter exited while the acknowledgement was still unconsumed");
        anyhow::ensure!(unreaped_zombie_parented_by(pgid, adapter_pid),
            "F3: the leader stopped being an adapter-owned zombie while the acknowledgement was unconsumed: {:?}", process_state(pgid));
        anyhow::ensure!(!paths.closed.exists(), "F3: the endpoint propagated its close edge while the acknowledgement was unconsumed");
        let signals = std::fs::read_to_string(&group_signals).unwrap_or_default();
        anyhow::ensure!(signals.trim().is_empty(), "F3: the registered group was signalled while the acknowledgement was unconsumed: {signals:?}");
    }

    // Explicit release inside the adapter's pending response-read window.
    std::fs::write(&paths.release, b"release")?;
    let delivered = wait_relay_event(&paths.log, "delivered ", Duration::from_secs(10)).await?;
    anyhow::ensure!(delivered.contains("register"), "the relay did not deliver the endpoint's own Register response: {delivered}");

    // The adapter consumed the endpoint's own response: its typed `Revoke`
    // request follows the delivery.
    let revoked = wait_relay_event_with(&paths.log, "a->e ", "revoke", Duration::from_secs(10)).await?;
    anyhow::ensure!(revoked.contains("revoke"), "the adapter did not consume the released response: {revoked}");
    // The endpoint's complete real `Revoke` response must be forwarded, not just
    // the adapter's request.
    let confirmed = wait_relay_event_with(&paths.log, "e->a ", "revoke", Duration::from_secs(10)).await?;
    anyhow::ensure!(confirmed.contains("refused") && confirmed.contains("false"),
        "the endpoint's complete Revoke response was not forwarded to the adapter: {confirmed}");

    // The adapter reaps its own leader and exits by itself. Only then may the
    // endpoint reference be released.
    anyhow::ensure!(wait_process_gone(pgid, Duration::from_secs(20)).await,
        "F3: the real adapter did not reap its retained leader");
    anyhow::ensure!(wait_process_gone(adapter_pid, Duration::from_secs(20)).await,
        "F3: the real adapter did not exit after its own reap");

    // The consumption chain, as content evidence rather than as log order: the
    // relay held the endpoint's own `Register` response and never delivered it
    // before the release; the adapter then produced a legal `Revoke` for the
    // registration it asked the endpoint to arm; and the endpoint's complete
    // `Revoke` response was forwarded back to that same adapter. A `Revoke` is
    // only reachable after a consumed `Armed` acknowledgement, so this chain
    // proves consumption, not merely delivery.
    let ordered = read_relay_events(&paths.log);
    let registered = event_line_with(&ordered, "a->e ", "register")
        .ok_or_else(|| anyhow::anyhow!("the adapter never requested a registration"))?;
    let registration = registered
        .split("registration_id\":")
        .nth(1)
        .map(|rest| rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .unwrap_or_default();
    anyhow::ensure!(!registration.is_empty(), "the adapter's registration request carried no registration id: {registered}");
    anyhow::ensure!(revoked.contains(&format!("registration_id\":{registration}")),
        "F3: the adapter's revoke was not for the registration the endpoint armed: {revoked} vs {registered}");
    anyhow::ensure!(event_line_with(&ordered, "e->a ", "revoke").is_some(),
        "F3: the endpoint's revoke response was never forwarded to the adapter: {ordered:?}");

    // Release the endpoint reference only after the adapter's own exit.
    if let Some(mut socket) = control.take() { let _ = socket.send(Message::Close(None)).await; }
    let session_dir = root.join(format!("endpoint/session-{session_id}"));
    wait_dir_gone(&session_dir, Duration::from_secs(30)).await?;
    let closing = read_relay_events(&paths.log);
    let adapter_closed_at = event_index(&closing, "adapter-closed").ok_or_else(|| anyhow::anyhow!("the adapter never closed its control connection"))?;
    let endpoint_closed_at = event_index(&closing, "endpoint-closed").ok_or_else(|| anyhow::anyhow!("the relay never recorded the endpoint close edge"))?;
    anyhow::ensure!(adapter_closed_at < endpoint_closed_at,
        "F3: the endpoint closed before the adapter's own exit: {closing:?}");
    let stderr = std::fs::read_to_string(root.join("endpoint.err")).unwrap_or_default();
    anyhow::ensure!(!stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F3: the endpoint published the unsettled-permit terminal for a registration the adapter settled on its own:\n{stderr}");
    eprintln!("R10-F3-RESULT: arrangement={} delivered<revoke-request<revoke-response<adapter-exit<endpoint-close; adapter-owned reap confirmed",
        if legacy { "legacy" } else { "adapter-owned" });

    cleanup.cleanup().await?;
    Ok(())
}

/// R10-F1: one real observation session. It reuses the O1 real-boundary setup —
/// the real Host, the real endpoint over real mTLS, the real per-session
/// `encoder-control.sock` and a real endpoint-spawned adapter direct child —
/// without copying the long-term regression bodies.
struct ObservedSession {
    host: tokio::process::Child,
    endpoint: tokio::process::Child,
    control: Socket,
    control_path: PathBuf,
    endpoint_stderr: PathBuf,
    session_dir: PathBuf,
}

impl ObservedSession {
    async fn open(fixture: &Fixture, adapter: &Path) -> anyhow::Result<Self> {
        let root = fixture.root.clone();
        let host = fixture.start_host().await?;
        let session_id = fixture.session_id();
        let endpoint = fixture.endpoint_with_media_bin(adapter, None)?;
        wait_socket(root.join("endpoint")).await;
        let connector = fixture.connector()?;
        let (mut control, _token) = connect_control(&connector, fixture.address()).await?;
        let ready = next_json(&mut control).await?;
        anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
        control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
        let attached = next_json(&mut control).await?;
        anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");
        Ok(Self {
            host,
            endpoint,
            control,
            control_path: root.join(format!("endpoint/session-{session_id}/encoder-control.sock")),
            endpoint_stderr: root.join("endpoint.err"),
            session_dir: root.join(format!("endpoint/session-{session_id}")),
        })
    }

    fn session_dir(&self) -> PathBuf { self.session_dir.clone() }

    fn stderr(&self) -> String { std::fs::read_to_string(&self.endpoint_stderr).unwrap_or_default() }

    /// Release the last control reference of this observation session.
    async fn release_control_reference(&mut self) {
        let _ = self.control.send(Message::Close(None)).await;
    }

    /// Terminate the endpoint this session started, by explicit child handle
    /// only, and report the exit code.
    async fn terminate_endpoint(&mut self) -> Option<i32> {
        if let Some(pid) = self.endpoint.id() {
            // Safety: signal the exact endpoint process this session started.
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
        }
        match tokio::time::timeout(Duration::from_secs(20), self.endpoint.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => { let _ = self.endpoint.kill().await; None }
        }
    }

    /// Terminate both product processes this session started.
    async fn shutdown(&mut self) {
        let _ = self.endpoint.kill().await;
        let _ = self.endpoint.wait().await;
        let _ = self.host.kill().await;
        let _ = self.host.wait().await;
    }
}

/// R10-F1: the endpoint-spawned adapter direct child. It records its own pid and
/// every signal it receives. `stubborn` selects whether it survives SIGTERM, so
/// the endpoint's cooperative grace can expire and the retained terminal stays
/// observable.
/// R12-F1: the adapter direct child's wait loop must not spawn a child of its
/// own. `while :; do sleep 0.2; done` forks a `sleep` that is orphaned for up to
/// its own interval whenever the fixture reclaims the shell with SIGKILL, and
/// nextest's leak detector then records a leak for the case that caused it. A
/// blocking `read` on a FIFO this fixture owns waits inside the shell itself:
/// no child process, no busy loop, the TERM trap still runs, and a stubborn
/// child still survives SIGTERM.
fn write_adapter_child(root: &Path, stubborn: bool) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("adapter-child.sh");
    let pid_file = root.join("adapter-child.pid");
    let signals = root.join("adapter-child.signals");
    let fifo = root.join("adapter-child.fifo");
    let created = std::process::Command::new("mkfifo").arg(&fifo).status()?;
    anyhow::ensure!(created.success(), "mkfifo failed for {}", fifo.display());
    let trap = if stubborn {
        format!("trap 'printf \"15\\n\" >> \"{}\"' TERM\n", signals.display())
    } else {
        format!("trap 'printf \"15\\n\" >> \"{}\"; exit 0' TERM\n", signals.display())
    };
    std::fs::write(&script, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{pid}'\nexec 3<> '{fifo}'\n{trap}while :; do read -r _ <&3; done\n",
        pid = pid_file.display(), fifo = fifo.display(), trap = trap,
    ))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok((script, pid_file, signals))
}

/// Reclaim one fixture-owned adapter direct child by explicit pid. A retained
/// failure means the endpoint never tore that child down, so the fixture that
/// caused it to be spawned must not leave it behind.
fn reclaim_adapter(pid: i32) {
    if process_alive(pid) {
        // Safety: signal the exact adapter pid this fixture's endpoint spawned.
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }
}

/// R12-F1: the single cleanup owner for one fixture's adapter direct child.
///
/// The endpoint spawns that child, so it is not a child of this test and no
/// `kill_on_drop` covers it. The child writes its own pid and, in the stubborn
/// arrangement, survives SIGTERM by design, so only an explicit reclaim of that
/// exact pid can retire it.
///
/// Every case in this file that starts an adapter direct child creates one of
/// these before the child can exist. It reclaims the child on all three termini
/// of the case: the success path (`reclaim`, which then disarms the backstop),
/// every early `?`/`ensure!` return before the final reclaim, and outer
/// `tokio::time::timeout` cancellation, where `Drop` is the only code that still
/// runs. Until the pid is recorded, `Drop` falls back to the pid the child wrote
/// for itself, so a case that fails before `wait_pid_file` returns still cannot
/// leave the child behind.
///
/// The shape is the one `AdapterHarnessCleanup` uses: one owner, exact recorded
/// pids, a `Drop` backstop. That owner is not reused here because it owns the
/// endpoint, host and group children and the scratch root for the R8-F4 relay
/// harness, while these cases already own their endpoint and host through
/// `ObservedSession`; reusing it would need a placeholder relay pid record.
/// `reclaim_adapter` stays the one reclaim authority.
struct AdapterChildCleanup {
    root: PathBuf,
    pid_file: PathBuf,
    pid: Option<i32>,
    armed: bool,
}

impl AdapterChildCleanup {
    fn new(root: PathBuf, pid_file: PathBuf) -> Self {
        Self { root, pid_file, pid: None, armed: true }
    }

    /// Record the exact pid the adapter child wrote for itself.
    fn record(&mut self, pid: i32) { self.pid = Some(pid); }

    /// The exact pid this owner must retire: the recorded one, or the pid the
    /// child wrote for itself when the case ended before the record was read.
    fn target_pid(&self) -> Option<i32> {
        self.pid.or_else(|| std::fs::read_to_string(&self.pid_file).ok()?.trim().parse::<i32>().ok())
    }

    /// Reclaim the child on the success path and disarm the backstop.
    fn reclaim(&mut self) {
        if let Some(pid) = self.target_pid() { reclaim_adapter(pid); }
        self.armed = false;
    }
}

impl Drop for AdapterChildCleanup {
    fn drop(&mut self) {
        // Failure and cancellation backstop. It fires only while this owner
        // still owes a reclaim, and it signals only this fixture's adapter pid.
        if !self.armed { return; }
        if let Some(pid) = self.target_pid() { reclaim_adapter(pid); }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// R10-F1: a real test-owned process group that records every SIGTERM and
/// survives it, so the endpoint's own signal work stays observable past its
/// two-second grace.
async fn spawn_stubborn_group(root: &Path) -> anyhow::Result<(tokio::process::Child, i32, PathBuf)> {
    let signals = root.join("group.signals");
    let ready = root.join("group.ready");
    let script = format!(
        "import signal, time\nsignals = {signals:?}\nready = {ready:?}\ndef on_term(signum, frame):\n    open(signals, 'a').write('15\\n')\nsignal.signal(signal.SIGTERM, on_term)\nopen(ready, 'w').close()\nwhile True:\n    time.sleep(0.2)\n",
        signals = signals.display().to_string(),
        ready = ready.display().to_string(),
    );
    let group = tokio::process::Command::new("python3")
        .arg("-c").arg(script)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pgid = i32::try_from(group.id().context("group child pid")?)?;
    wait_file(&ready, Duration::from_secs(10)).await?;
    anyhow::ensure!(unsafe { libc::getpgid(pgid) } == pgid, "the test group child was not its own process group");
    Ok((group, pgid, signals))
}

struct CloseTerminalObservation {
    terminal_present: bool,
    session_dir_present: bool,
    adapter_alive: bool,
    adapter_signals: String,
}

/// R10-F1: watch the endpoint's published terminal after a close edge. The wait
/// ends as soon as the failure terminal appears or the endpoint publishes a
/// cleanup success, so the caller asserts on a settled observation.
async fn observe_close_terminal(
    session: &ObservedSession,
    adapter_pid: i32,
    adapter_signals: &Path,
    timeout: Duration,
) -> CloseTerminalObservation {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let terminal_present = session.stderr().contains("SESSION_ENCODER_PERMIT_UNSETTLED");
        let session_dir_present = session.session_dir().exists();
        let adapter_alive = process_alive(adapter_pid);
        if terminal_present || !session_dir_present || !adapter_alive || tokio::time::Instant::now() >= deadline {
            return CloseTerminalObservation {
                terminal_present,
                session_dir_present,
                adapter_alive,
                adapter_signals: std::fs::read_to_string(adapter_signals).unwrap_or_default(),
            };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn read_to_eof(read: &mut BufReader<tokio::net::unix::OwnedReadHalf>, timeout: Duration) -> anyhow::Result<()> {
    let mut scratch = [0u8; 256];
    tokio::time::timeout(timeout, async {
        loop {
            match read.read(&mut scratch).await {
                Ok(0) => return Ok::<(), anyhow::Error>(()),
                Ok(_) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("the endpoint never closed the control transport"))??;
    Ok(())
}

async fn wait_child_status(child: &mut tokio::process::Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    }
}

/// R10-F1: the close terminal for a session that owns an unregistered adapter
/// child.
///
/// The endpoint spawns a real adapter direct child, and the session is driven
/// through the real `encoder-control.sock` by the test, which is the adapter
/// here. The test takes a real permit and then asks the endpoint to revoke it.
/// The endpoint refuses, because this session has no registered group. The
/// test's child is alive and was never registered, so the session can never be
/// settled by an adapter-owned completion: the adapter has nothing to retire.
///
/// The close edge is a real transport close of that control socket. The
/// endpoint must then retain the session and publish the unsettled-permit
/// terminal; it must not synthesize a no-child settlement, remove the session
/// directory, or force-kill the live direct child.
#[tokio::test]
async fn r10_unregistered_child_close_retains_failure() {
    tokio::time::timeout(Duration::from_secs(180), unregistered_child_close_retains_failure())
        .await
        .expect("r10 F1 unregistered-child case timed out")
        .expect("r10 F1 unregistered-child case failed");
}

async fn unregistered_child_close_retains_failure() -> anyhow::Result<()> {
    let fixture = Fixture::new("r10f1a")?;
    let root = fixture.root.clone();
    let (adapter, adapter_pid_file, adapter_signals) = write_adapter_child(&root, true)?;
    // R12-F1: the fixture's single cleanup owner, armed before the child exists.
    let mut adapter_cleanup = AdapterChildCleanup::new(root.clone(), adapter_pid_file.clone());
    let mut session = ObservedSession::open(&fixture, &adapter).await?;
    let adapter_pid = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    adapter_cleanup.record(adapter_pid);
    anyhow::ensure!(process_alive(adapter_pid), "the endpoint's adapter direct child was not alive");

    wait_socket(session.control_path.clone()).await;
    let stream = tokio::net::UnixStream::connect(&session.control_path).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let acquired = exchange_frame(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 77 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "the endpoint did not grant the encoder permit this case needs: {acquired:?}");
    let probe = exchange_frame(&mut read, &mut write, EncoderControlRequest::Revoke { registration_id: 77 }).await?;
    let refused = matches!(probe, EncoderControlResponse::Revoke { refused: true });
    anyhow::ensure!(refused, "the endpoint accepted a revoke for a registration it never made: {probe:?}");
    eprintln!("R10-F1A-PRECONDITION: acquire=Granted revoke=refused:true; adapter direct child {adapter_pid} alive and never registered");

    // The close edge: the real control transport ends while the child is alive
    // and the registration is not adapter-owned.
    drop(write);
    drop(read);
    // The close edge must leave the session retained. The wait ends early only
    // on a published cleanup success: the session directory disappearing or the
    // live direct child dying. The terminal itself is only printed when the
    // endpoint finalizes the entry, so the retained-failure terminal is read
    // after the shutdown below, never before it.
    let observed = observe_close_terminal(&session, adapter_pid, &adapter_signals, Duration::from_secs(15)).await;
    eprintln!("R10-F1A-OBSERVED: terminal_present={} session_dir_present={} adapter_alive={} adapter_signals={:?}",
        observed.terminal_present, observed.session_dir_present, observed.adapter_alive, observed.adapter_signals);
    anyhow::ensure!(observed.session_dir_present,
        "F1: the endpoint removed the session directory after a close edge with a live, never-registered adapter child, so it published a no-child cleanup success that no adapter settlement supports");
    anyhow::ensure!(observed.adapter_alive,
        "F1: the endpoint force-killed the live adapter direct child of an unsettled session");

    // Endpoint shutdown drives the same session owner: the retained entry must
    // be published as the unsettled-permit terminal, and the shutdown must fail
    // instead of reporting a successful cleanup of the retained session.
    let code = session.terminate_endpoint().await;
    let stderr = session.stderr();
    eprintln!("R10-F1A-RESULT: endpoint_exit={code:?} terminal_present={} finalize_failed={}",
        stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"), stderr.contains("ENDPOINT_SESSION_FINALIZE_FAILED"));
    anyhow::ensure!(stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1: the endpoint did not publish SESSION_ENCODER_PERMIT_UNSETTLED for a session that ended with an unsettled encoder permit:\n{stderr}");
    anyhow::ensure!(!matches!(code, Some(0)),
        "F1: the endpoint reported a successful shutdown after publishing an unsettled encoder permit: exit={code:?}");
    anyhow::ensure!(stderr.contains("ENDPOINT_SESSION_FINALIZE_FAILED"),
        "F1: the endpoint did not report the failed session finalization at shutdown:\n{stderr}");
    adapter_cleanup.reclaim();
    session.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// R11-F1: the close terminal when the encoder-control reader fails and the
/// session's encoder permit is unsettled at the same time.
///
/// The session owns a real adapter direct child that refuses cooperative exit.
/// The test takes a real permit over the real per-session control socket and
/// never registers or settles it. The reader then receives a real, bounded,
/// malformed typed frame, so it ends with its original
/// `ENCODER_CONTROL_MALFORMED` cause while the finalizer independently ends with
/// the unsettled-permit terminal.
///
/// The supervisor is the single exit for both owners, so the session terminal it
/// publishes must carry both causes. The endpoint publishes that terminal as the
/// message of `SESSION_MEDIA_FINALIZE_FAILED` on a new attachment to the retained
/// session, and on its own error exit. A supervisor that returns on the
/// finalizer error first silently drops the reader's original cause; the same
/// assertion fails there.
#[tokio::test]
async fn r11_reader_failure_and_unsettled_permit_both_surface() {
    tokio::time::timeout(Duration::from_secs(240), reader_failure_and_unsettled_permit_both_surface())
        .await
        .expect("r11 F1 reader-failure case timed out")
        .expect("r11 F1 reader-failure case failed");
}

/// R11-F1: wait for the endpoint to publish this session's finalization terminal
/// on its own error exit. The release path prints the exact terminal string the
/// published `CompletionState::Failed` carries.
async fn wait_session_finalize_terminal(session: &ObservedSession, timeout: Duration) -> anyhow::Result<String> {
    const MARKER: &str = "session media finalize terminal: ";
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let stderr = session.stderr();
        if let Some(line) = stderr.lines().find(|line| line.starts_with(MARKER)) {
            return Ok(line.trim_start_matches(MARKER).to_owned());
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "the endpoint never published this session's finalization terminal:\n{stderr}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn reader_failure_and_unsettled_permit_both_surface() -> anyhow::Result<()> {
    let fixture = Fixture::new("r11f1")?;
    let root = fixture.root.clone();
    // The endpoint's real adapter direct child records SIGTERM and survives it,
    // so the finalizer's cooperative grace expires and the permit stays
    // unsettled while the session is retained.
    let (adapter, adapter_pid_file, adapter_signals) = write_adapter_child(&root, true)?;
    // R12-F1: the fixture's single cleanup owner, armed before the child exists.
    let mut adapter_cleanup = AdapterChildCleanup::new(root.clone(), adapter_pid_file.clone());
    let mut session = ObservedSession::open(&fixture, &adapter).await?;
    let adapter_pid = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    adapter_cleanup.record(adapter_pid);
    anyhow::ensure!(process_alive(adapter_pid), "the endpoint's adapter direct child was not alive");

    wait_socket(session.control_path.clone()).await;
    let stream = tokio::net::UnixStream::connect(&session.control_path).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let acquired = exchange_frame(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 77 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "the endpoint did not grant the encoder permit this case needs: {acquired:?}");

    // The reader's own failure: one real bounded malformed typed frame. The
    // reader ends with its original `ENCODER_CONTROL_MALFORMED` cause.
    write.write_all(b"{\"op\":\"__r11_f1_reader_failure__\"}\n").await?;
    write.flush().await?;
    read_to_eof(&mut read, Duration::from_secs(20)).await?;
    eprintln!("R11-F1-PRECONDITION: acquire=Granted registration_id=77 never registered and never settled; malformed frame ended the reader; adapter direct child {adapter_pid} alive and stubborn");

    // The close edge: the last control reference of this session is released, so
    // the endpoint finalizes the entry and publishes its terminal.
    session.release_control_reference().await;
    let published = wait_session_finalize_terminal(&session, Duration::from_secs(60)).await?;
    eprintln!("R11-F1-PUBLISHED-TERMINAL: {published}");

    // R15-F1: the shared F1 behaviour is observed and printed before any of its
    // assertions runs, so one run's log carries both the published terminal and
    // the retained resources. The wrong retirement guard deviates on the
    // resources this input keeps alive, and that deviation is the first
    // assertion it fails; the candidate keeps both.
    //
    // The unresolved terminal must not become a cleanup success: the session
    // directory and the live direct child that never registered stay retained.
    let retained_dir = session.session_dir().exists();
    let retained_adapter = process_alive(adapter_pid);
    eprintln!("R11-F1-RETAINED: session_dir_present={retained_dir} adapter_alive={retained_adapter} adapter_signals={:?}",
        std::fs::read_to_string(&adapter_signals).unwrap_or_default());
    anyhow::ensure!(retained_dir,
        "F1: the endpoint removed the session directory for a session with an unresolved terminal, so it published a cleanup success that no adapter settlement supports");
    anyhow::ensure!(retained_adapter,
        "F1: the endpoint force-killed the live adapter direct child of a session with an unresolved terminal");
    anyhow::ensure!(published.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1: the published session terminal lost the unsettled-permit terminal:\n{published}");
    anyhow::ensure!(published.contains("ENCODER_CONTROL_MALFORMED"),
        "F1: the published session terminal dropped the reader's original error, so a finalizer failure replaced the reader's cause:\n{published}");

    // The same terminal is the public attachment surface: a new attachment to the
    // retained session must receive it as `SESSION_MEDIA_FINALIZE_FAILED`.
    let connector = fixture.connector()?;
    let (mut control, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":9,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attachment = next_json(&mut control).await?;
    eprintln!("R11-F1-ATTACH-TERMINAL: {attachment}");
    anyhow::ensure!(attachment["type"] == "error" && attachment["code"] == "SESSION_MEDIA_FINALIZE_FAILED",
        "F1: a new attachment to the retained session did not receive the retained finalization terminal: {attachment}");
    let message = attachment["message"].as_str().unwrap_or_default().to_owned();
    anyhow::ensure!(message.contains("ENCODER_CONTROL_MALFORMED"),
        "F1: the endpoint attachment terminal dropped the reader's original error:\n{message}");
    anyhow::ensure!(message.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1: the endpoint attachment terminal lost the unsettled-permit terminal:\n{message}");

    // R15-F1: the retained-resource assertions for this input ran above, before
    // the terminal-content assertions; nothing about them is weakened or dropped.

    // The endpoint's own error exit must report the same terminal and must not
    // report a successful cleanup of the retained session.
    let code = session.terminate_endpoint().await;
    let stderr = session.stderr();
    eprintln!("R11-F1-RESULT: endpoint_exit={code:?} stderr_has_reader_cause={} stderr_has_unsettled={}",
        stderr.contains("ENCODER_CONTROL_MALFORMED"), stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"));
    anyhow::ensure!(stderr.contains("ENCODER_CONTROL_MALFORMED"),
        "F1: the endpoint error exit did not carry the reader's original error:\n{stderr}");
    anyhow::ensure!(stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1: the endpoint error exit did not carry the unsettled-permit terminal:\n{stderr}");
    anyhow::ensure!(!matches!(code, Some(0)),
        "F1: the endpoint reported a successful shutdown after publishing an unresolved session terminal: exit={code:?}");
    adapter_cleanup.reclaim();
    session.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// R10-F1 positive control: the same close path, but with a real typed
/// no-child settlement from the adapter side.
///
/// This is the boundary the correct behaviour must not turn into a blanket
/// failure. The adapter takes a real permit, answers the endpoint's settlement
/// question with the typed `RefusedNoChild` outcome, and only then closes. The
/// endpoint must complete the session normally: remove the session directory,
/// publish no unsettled-permit terminal, and shut down with exit code 0.
#[tokio::test]
async fn r10_no_child_settlement_closes_normally() {
    tokio::time::timeout(Duration::from_secs(180), no_child_settlement_closes_normally())
        .await
        .expect("r10 F1 no-child positive control timed out")
        .expect("r10 F1 no-child positive control failed");
}

async fn no_child_settlement_closes_normally() -> anyhow::Result<()> {
    let fixture = Fixture::new("r10f1b")?;
    let root = fixture.root.clone();
    let (adapter, adapter_pid_file, _adapter_signals) = write_adapter_child(&root, false)?;
    // R12-F1: the fixture's single cleanup owner, armed before the child exists.
    let mut adapter_cleanup = AdapterChildCleanup::new(root.clone(), adapter_pid_file.clone());
    let mut session = ObservedSession::open(&fixture, &adapter).await?;
    let adapter_pid = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    adapter_cleanup.record(adapter_pid);
    anyhow::ensure!(process_alive(adapter_pid), "the endpoint's adapter direct child was not alive");

    wait_socket(session.control_path.clone()).await;
    let stream = tokio::net::UnixStream::connect(&session.control_path).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let acquired = exchange_frame(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 11 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "the endpoint did not grant the encoder permit this control needs: {acquired:?}");
    let settled = exchange_frame(&mut read, &mut write, EncoderControlRequest::Settle {
        registration_id: 11, outcome: EncoderSettlement::RefusedNoChild,
    }).await?;
    anyhow::ensure!(matches!(settled, EncoderControlResponse::Settle),
        "the endpoint did not answer the typed no-child settlement: {settled:?}");
    eprintln!("R10-F1B-PRECONDITION: acquire=Granted settle=RefusedNoChild answered by the endpoint; no Register frame was written");

    drop(write);
    drop(read);
    let removed = wait_dir_gone_bool(&session.session_dir(), Duration::from_secs(40)).await;
    let stderr = session.stderr();
    eprintln!("R10-F1B-OBSERVED: session_dir_removed={removed} terminal_present={}", stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"));
    anyhow::ensure!(removed,
        "F1 positive control: the endpoint retained the session directory after a real typed no-child settlement, so it turned every close into a retained failure");
    anyhow::ensure!(!stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1 positive control: the endpoint published the unsettled-permit terminal after a real typed no-child settlement:\n{stderr}");

    session.release_control_reference().await;
    let code = session.terminate_endpoint().await;
    eprintln!("R10-F1B-RESULT: endpoint_exit={code:?}");
    anyhow::ensure!(code == Some(0),
        "F1 positive control: the endpoint did not shut down cleanly after a real no-child settlement: exit={code:?}\n{}", session.stderr());
    adapter_cleanup.reclaim();
    session.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// R10-F1: the close terminal for a session the endpoint has already put into
/// signal work.
///
/// The endpoint's reader takes authority from the real registration and starts
/// its own signal work: it delivers SIGTERM to the registered group, and that
/// group is still alive afterwards. The transport then fails while that work is
/// still in flight. The endpoint must not publish a session completion at that
/// moment; it must finish the work it started, and only then close.
#[tokio::test]
async fn r10_revoking_close_waits_for_signal_completion() {
    tokio::time::timeout(Duration::from_secs(180), revoking_close_waits_for_signal_completion())
        .await
        .expect("r10 F1 revoking-close case timed out")
        .expect("r10 F1 revoking-close case failed");
}

async fn revoking_close_waits_for_signal_completion() -> anyhow::Result<()> {
    let fixture = Fixture::new("r10f1c")?;
    let root = fixture.root.clone();
    let (adapter, adapter_pid_file, _adapter_signals) = write_adapter_child(&root, false)?;
    // R12-F1: the fixture's single cleanup owner, armed before the child exists.
    let mut adapter_cleanup = AdapterChildCleanup::new(root.clone(), adapter_pid_file.clone());
    let mut session = ObservedSession::open(&fixture, &adapter).await?;
    let adapter_pid = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    adapter_cleanup.record(adapter_pid);
    anyhow::ensure!(process_alive(adapter_pid), "the endpoint's adapter direct child was not alive");

    wait_socket(session.control_path.clone()).await;
    let stream = tokio::net::UnixStream::connect(&session.control_path).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let acquired = exchange_frame(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 21 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "the endpoint did not grant the encoder permit this case needs: {acquired:?}");
    let (mut group, pgid, group_signals) = spawn_stubborn_group(&root).await?;
    let registered = exchange_frame(&mut read, &mut write, EncoderControlRequest::Register { registration_id: 21, pgid }).await?;
    anyhow::ensure!(matches!(registered, EncoderControlResponse::Register { refused: false }),
        "the endpoint refused the real group registration this case needs: {registered:?}");
    anyhow::ensure!(process_group_alive(pgid), "the registered group died before the endpoint took authority");

    // Release the last control reference: the endpoint's finalizer moves the
    // permit into signal work and delivers its own SIGTERM.
    session.release_control_reference().await;
    let signal_started = tokio::time::Instant::now();
    wait_signals(&group_signals, Duration::from_secs(20)).await?;
    let alive_at_trigger = process_group_alive(pgid);
    let dir_at_trigger = session.session_dir().exists();
    eprintln!("R10-F1C-PRECONDITION: endpoint SIGTERM recorded; group_alive={alive_at_trigger} session_dir_present={dir_at_trigger} group={pgid} adapter={adapter_pid}");
    anyhow::ensure!(alive_at_trigger, "the registered group did not survive the endpoint's SIGTERM, so no signal work was in flight");
    anyhow::ensure!(dir_at_trigger, "the endpoint published session completion before it delivered its own SIGTERM");

    // Trigger: the control transport fails while the endpoint's signal work is
    // still in flight.
    write.write_all(b"{\"op\":\"__r10_f1c_reader_failure__\"}\n").await?;
    let elapsed = signal_started.elapsed();
    // The close barrier must hold the transport open for as long as the
    // endpoint's own signal work is unfinished. A close inside the two-second
    // grace is the premature close edge: it retires authority the endpoint is
    // still exercising, so the adapter and the endpoint would own the group at
    // the same time.
    let closed_within_grace =
        tokio::time::timeout(Duration::from_millis(300), read_to_eof(&mut read, Duration::from_secs(20))).await;
    let alive_after_trigger = process_group_alive(pgid);
    let dir_after_trigger = session.session_dir().exists();
    eprintln!("R10-F1C-TRIGGER: reader failure {elapsed:?} after the endpoint SIGTERM; transport_closed_within_grace={} group_alive={alive_after_trigger} session_dir_present={dir_after_trigger}",
        closed_within_grace.is_ok());
    anyhow::ensure!(elapsed < Duration::from_secs(2),
        "the reader failure landed {elapsed:?} after the SIGTERM, outside the endpoint's two-second SIGTERM grace, so the close did not overlap the signal work");
    anyhow::ensure!(closed_within_grace.is_err(),
        "F1 Revoking: the endpoint closed the encoder-control transport while the group-signal work it had started was still in flight, so it retired authority it was still exercising");
    anyhow::ensure!(alive_after_trigger, "F1 Revoking: the registered group was already settled when the reader failed");
    anyhow::ensure!(dir_after_trigger,
        "F1 Revoking: the endpoint published its session completion while the signal work it had started was still unfinished");
    read_to_eof(&mut read, Duration::from_secs(20)).await?;

    // The endpoint must finish the work it started, and only then close.
    let status = wait_child_status(&mut group, Duration::from_secs(20)).await;
    let exit_signal = status.as_ref().and_then(|status| {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    });
    let reaped = wait_group_reaped(pgid, Duration::from_secs(10)).await;
    let dir_removed = wait_dir_gone_bool(&session.session_dir(), Duration::from_secs(30)).await;
    let stderr = session.stderr();
    let recorded = std::fs::read_to_string(&group_signals).unwrap_or_default();
    eprintln!("R10-F1C-OBSERVED: group_exit_signal={exit_signal:?} group_reaped={reaped} session_dir_removed={dir_removed} signals={recorded:?} terminal_present={}",
        stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"));
    anyhow::ensure!(exit_signal == Some(libc::SIGKILL),
        "F1 Revoking: the endpoint abandoned the signal work it had started after the close edge; group_exit_signal={exit_signal:?}");
    anyhow::ensure!(reaped, "F1 Revoking: the registered group was not reaped after the endpoint's own signal work");
    anyhow::ensure!(dir_removed, "F1 Revoking: the endpoint did not complete the close after the signal work finished");
    anyhow::ensure!(recorded.trim() == "15", "F1 Revoking: the group recorded signals other than the endpoint's own SIGTERM: {recorded:?}");
    anyhow::ensure!(!stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED"),
        "F1 Revoking: an unsettled-permit terminal appeared for a registered group:\n{stderr}");

    let code = session.terminate_endpoint().await;
    eprintln!("R10-F1C-RESULT: endpoint_exit={code:?}");
    adapter_cleanup.reclaim();
    session.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// R8-F4/R10-F4: an adapter that fails before it opens the control connection
/// leaves the bounded relay blocked in `accept()`. The cleanup owner created
/// before the wrapper launch must terminate that relay on the early exit, not
/// only after a fully successful assertion run.
///
/// R10-F4 adds two things to this same test: an external resource check after
/// the accept-stage cleanup, and a second probe that cancels a run while it sits
/// in the held stage, where the owner's `Drop` backstop is the only remaining
/// authority.
#[tokio::test]
async fn real_endpoint_adapter_startup_failure_cleans_up_relay() {
    tokio::time::timeout(Duration::from_secs(240), adapter_startup_failure_cleanup())
        .await
        .expect("R8-F4 startup-failure case timed out")
        .expect("R8-F4 startup-failure case failed");
}

async fn adapter_startup_failure_cleanup() -> anyhow::Result<()> {
    let fixture = Fixture::new("ob10f")?;
    let root = fixture.root.clone();

    // The wrapper is the endpoint's `--media-bin`. It binds the relay at the
    // real control path and then execs a media binary that exits immediately,
    // before it ever dials the relay: the relay is left blocked in `accept()`.
    let paths = write_relay_harness(&root, "exec /bin/false")?;
    let adapter_pid_file = root.join("group/adapter.pid");

    // R8-F4: the cleanup owner is created before the wrapper is launched.
    let mut cleanup = AdapterHarnessCleanup::new(
        "F4 startup failure", root.clone(), paths.pid.clone(), adapter_pid_file.clone());
    cleanup.own_host(fixture.start_host().await?);
    let session_id = fixture.session_id();
    cleanup.own_endpoint(fixture.endpoint_with_media_bin(&paths.wrapper, None)?);
    let endpoint_pid = cleanup.endpoint_pid();
    let host_pid = cleanup.host_pid();
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control_socket, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control_socket).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control_socket.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control_socket).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    // The adapter exits before it opens the control connection, so the relay
    // stays blocked in `accept()` and no adapter pid record is ever written.
    wait_file(&paths.wrapper_ready, Duration::from_secs(30)).await?;
    wait_file(&paths.pid, Duration::from_secs(30)).await?;
    let relay_pid: i32 = wait_pid_file(&paths.pid, Duration::from_secs(30)).await?;
    cleanup.record_relay_pid(relay_pid);
    anyhow::ensure!(process_alive(relay_pid), "the relay exited before the early adapter failure was observed");
    anyhow::ensure!(!adapter_pid_file.exists(), "the adapter opened the control connection despite the startup failure");

    // The owner terminates the recorded relay on this early exit path, before
    // it removes the scratch root.
    cleanup.cleanup().await?;
    anyhow::ensure!(!process_alive(relay_pid), "R8-F4: the relay leaked after the adapter startup failure");
    // R10-F4: the accept-stage external resource check. After the owner ran, no
    // harness resource may remain.
    anyhow::ensure!(!root.exists(), "R10-F4: the accept-stage scratch root survived cleanup: {}", root.display());
    if let Some(pid) = endpoint_pid {
        anyhow::ensure!(!process_alive(pid), "R10-F4: the accept-stage endpoint pid {pid} is still live after cleanup");
    }
    if let Some(pid) = host_pid {
        anyhow::ensure!(!process_alive(pid), "R10-F4: the accept-stage host pid {pid} is still live after cleanup");
    }
    eprintln!("R8-F4/R10-F4: accept-stage failure left no relay, endpoint, host or scratch root behind");

    drop(control_socket);

    // R10-F4: the held stage. The run reaches a held acknowledgement and is then
    // cancelled from the outside; only the owner's `Drop` backstop can still
    // terminate the harness.
    held_stage_cancellation_probe().await?;
    Ok(())
}

/// R10-F4: cancellation in the held stage. A real endpoint, driven through the
/// real bounded relay, holds a real `Register` response for an adapter
/// stand-in that stays connected. The waiting future is then dropped while the
/// run is in that held stage, which is exactly the path where no assertion ever
/// runs and the cleanup owner's `Drop` is the only remaining authority. Every
/// harness resource must be gone afterwards.
async fn held_stage_cancellation_probe() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = Fixture::new("r10f4h")?;
    let root = fixture.root.clone();
    let adapter = root.join("held-adapter.py");
    let adapter_pid_file = root.join("held-adapter.pid");
    let source = r#"import json, os, socket, sys, time
video = ''
prev = ''
for arg in sys.argv[1:]:
    if prev == '--video-socket':
        video = arg
    prev = arg
control = os.path.join(os.path.dirname(video), 'encoder-control.sock')
deadline = time.time() + 20
while not os.path.exists(control):
    if time.time() > deadline:
        sys.exit(9)
    time.sleep(0.02)
conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
conn.connect(control)
open(PID_PATH, 'w').write(str(os.getpid()))
conn.sendall(json.dumps({'op': 'acquire', 'registration_id': 1}).encode() + b'\n')
conn.sendall(json.dumps({'op': 'register', 'registration_id': 1, 'pgid': os.getpid()}).encode() + b'\n')
while True:
    time.sleep(0.2)
"#.replace("PID_PATH", &format!("{:?}", adapter_pid_file.display().to_string()));
    std::fs::write(&adapter, source)?;
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755))?;

    let paths = write_relay_harness(&root, &format!("exec python3 '{}' \"$@\"", adapter.display()))?;
    let mut cleanup = AdapterHarnessCleanup::new(
        "r10 F4 held stage", root.clone(), paths.pid.clone(), adapter_pid_file.clone());
    cleanup.own_host(fixture.start_host().await?);
    let session_id = fixture.session_id();
    cleanup.own_endpoint(fixture.endpoint_with_media_bin(&paths.wrapper, None)?);
    let endpoint_pid = cleanup.endpoint_pid();
    let host_pid = cleanup.host_pid();
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control_socket, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control_socket).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control_socket.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control_socket).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    let held_stage = Arc::new(AtomicBool::new(false));
    let reached = held_stage.clone();
    let held_path = paths.held.clone();
    let cancelled = tokio::time::timeout(Duration::from_secs(60), async move {
        // The owner is moved into this future, so dropping the future is the
        // cancellation and drops the owner with it.
        let _owner = cleanup;
        wait_file(&held_path, Duration::from_secs(40)).await?;
        reached.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    anyhow::ensure!(cancelled.is_err() && held_stage.load(Ordering::SeqCst),
        "R10-F4: the held-stage probe never reached the held acknowledgement before its cancellation edge");

    let relay_pid = read_pid_record(&paths.pid);
    let adapter_pid = read_pid_record(&adapter_pid_file);
    anyhow::ensure!(relay_pid.is_some() && adapter_pid.is_some(),
        "R10-F4: the held stage did not record its relay and adapter pids");
    if let Some(pid) = relay_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await, "R10-F4: the held-stage relay {pid} survived cancellation");
    }
    if let Some(pid) = adapter_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await, "R10-F4: the held-stage adapter {pid} survived cancellation");
    }
    if let Some(pid) = endpoint_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await, "R10-F4: the held-stage endpoint {pid} survived cancellation");
    }
    if let Some(pid) = host_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await, "R10-F4: the held-stage host {pid} survived cancellation");
    }
    drop(control_socket);
    let _ = std::fs::remove_dir_all(&root);
    eprintln!("R10-F4: held-stage cancellation left no relay, adapter, endpoint or host behind");
    Ok(())
}

/// R13-F4: cancellation while the bounded relay is still blocked in `accept()`.
///
/// The startup-failure case reaches this same stage and then calls the owner's
/// normal asynchronous `cleanup()`. That is the failure path, not the
/// cancellation path. Here the owner is moved into the future that is dropped at
/// the accept stage, so the owner's `Drop` backstop is the only code that can
/// still run: the inner future's own assertions never execute, and `cleanup()`
/// is never called. The live-resource and recovery-record checks then run from
/// outside the cancelled future.
#[tokio::test]
async fn r10_accept_stage_cancellation_leaves_no_live_resource() {
    tokio::time::timeout(Duration::from_secs(240), accept_stage_cancellation_probe())
        .await
        .expect("r10 F4 accept-stage cancellation probe timed out")
        .expect("r10 F4 accept-stage cancellation probe failed");
}

async fn accept_stage_cancellation_probe() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = Fixture::new("r10f4a")?;
    let root = fixture.root.clone();
    // The same real harness as the startup-failure case: the relay binds the real
    // per-session control path, and the endpoint's direct child execs a binary
    // that exits before it dials, so the relay stays blocked in `accept()`.
    let paths = write_relay_harness(&root, "exec /bin/false")?;
    let adapter_pid_file = root.join("group/adapter.pid");

    // R13-F4: the cleanup owner exists before the wrapper is launched. It is
    // moved into the future below, so dropping that future is the cancellation.
    let mut cleanup = AdapterHarnessCleanup::new(
        "r10 F4 accept-stage cancellation", root.clone(), paths.pid.clone(), adapter_pid_file.clone());
    cleanup.own_host(fixture.start_host().await?);
    let session_id = fixture.session_id();
    cleanup.own_endpoint(fixture.endpoint_with_media_bin(&paths.wrapper, None)?);
    let endpoint_pid = cleanup.endpoint_pid();
    let host_pid = cleanup.host_pid();
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control_socket, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control_socket).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control_socket.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control_socket).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    // Positive control for the negative assertions below: at the cancellation
    // edge the relay, the endpoint and the host were all live. The relay is
    // blocked in `accept()` because the adapter exited before it dialed.
    wait_file(&paths.wrapper_ready, Duration::from_secs(40)).await?;
    let relay_pid = wait_pid_file(&paths.pid, Duration::from_secs(40)).await?;
    anyhow::ensure!(process_alive(relay_pid),
        "R13-F4: the accept-stage relay {relay_pid} was not live before the cancellation edge");
    anyhow::ensure!(endpoint_pid.is_some_and(process_alive) && host_pid.is_some_and(process_alive),
        "R13-F4: the accept-stage endpoint and host children were not both live before the cancellation edge");

    // The accept-stage precondition is proved from inside the future that owns
    // the cleanup owner, and the flag is read only after that future was
    // dropped. A cancellation that happened anywhere else fails this case.
    let reached_accept = Arc::new(AtomicBool::new(false));
    let reached = reached_accept.clone();
    let accept_ready = paths.ready.clone();
    let accept_wrapper_ready = paths.wrapper_ready.clone();
    let accept_held = paths.held.clone();
    let accept_release = paths.release.clone();
    let accept_log = paths.log.clone();
    let accept_adapter_pid_file = adapter_pid_file.clone();
    let cancelled = tokio::time::timeout(Duration::from_secs(60), async move {
        // The owner is moved into this future, so dropping the future is the
        // cancellation and drops the owner with it.
        let _owner = cleanup;
        anyhow::ensure!(accept_ready.exists(),
            "R13-F4: the relay never bound the real control path, so this is not the accept stage");
        anyhow::ensure!(accept_wrapper_ready.exists(),
            "R13-F4: the wrapper never reached the adapter exec, so this is not the accept stage");
        anyhow::ensure!(!accept_held.exists(),
            "R13-F4: the run had already passed the accept stage: the held acknowledgement exists");
        anyhow::ensure!(!accept_release.exists(),
            "R13-F4: the run had already passed the accept stage: the release marker exists");
        anyhow::ensure!(!accept_adapter_pid_file.exists(),
            "R13-F4: an adapter dialed the relay before the cancellation edge, so this is not the accept stage");
        anyhow::ensure!(
            !read_relay_events(&accept_log).iter().any(|line| line.starts_with("a->e") || line.starts_with("e->a")),
            "R13-F4: the relay already carried control frames, so this is not the accept stage");
        reached.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    anyhow::ensure!(cancelled.is_err() && reached_accept.load(Ordering::SeqCst),
        "R13-F4: the accept-stage probe never proved the accept-stage precondition before its cancellation edge: {cancelled:?}");

    // Everything below runs outside the cancelled future: only the owner's `Drop`
    // backstop has run.
    //
    // The recovery records the owner would still need to finish the job are not
    // deleted before termination is verified: the relay pid record still names
    // the exact pid, and the scratch root with the relay's ready marker is still
    // on disk.
    let recorded_relay = read_pid_record(&paths.pid);
    anyhow::ensure!(recorded_relay == Some(relay_pid),
        "R13-F4: the accept-stage relay pid record was deleted or changed before termination was verified: {recorded_relay:?} vs {relay_pid}");
    anyhow::ensure!(root.exists() && paths.ready.exists(),
        "R13-F4: the cancellation backstop deleted the recovery records (scratch root, relay ready marker) before termination was verified");
    anyhow::ensure!(!paths.held.exists(),
        "R13-F4: the cancelled accept-stage run reached the held acknowledgement");
    anyhow::ensure!(wait_process_gone(relay_pid, Duration::from_secs(10)).await,
        "R13-F4: the accept-stage relay {relay_pid} survived the cancellation backstop");
    if let Some(pid) = endpoint_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await,
            "R13-F4: the accept-stage endpoint {pid} survived the cancellation backstop");
    }
    if let Some(pid) = host_pid {
        anyhow::ensure!(wait_process_gone(pid, Duration::from_secs(10)).await,
            "R13-F4: the accept-stage host {pid} survived the cancellation backstop");
    }
    // At the accept stage no adapter ever dialed, so there is no adapter pid and
    // no adapter process group: those two sub-conditions of the F4 row are
    // vacuous here and are asserted as absent rather than claimed as observed.
    anyhow::ensure!(!adapter_pid_file.exists(),
        "R13-F4: an adapter pid record appeared after the accept-stage cancellation");
    eprintln!(
        "R13-F4-ACCEPT-CANCELLED: relay_pid={relay_pid} endpoint_pid={endpoint_pid:?} host_pid={host_pid:?} \
         relay_pid_record_present={} relay_ready_marker_present={} held_marker_present={} \
         adapter_pid_record_present={} scratch_root_present={}",
        paths.pid.exists(), paths.ready.exists(), paths.held.exists(),
        adapter_pid_file.exists(), root.exists());

    // The test's own scratch teardown runs only after the live-resource and
    // record-retention checks above.
    drop(control_socket);
    let _ = std::fs::remove_dir_all(&root);
    anyhow::ensure!(!root.exists(),
        "R13-F4: the accept-stage scratch root survived the test's own teardown");
    Ok(())
}

/// Parse the relay's ordered event log. The relay serializes every event under
/// one lock with a monotonic sequence, so the recorded order is the wire order.
fn read_relay_events(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_owned).collect()
}

fn event_line(events: &[String], needle: &str) -> Option<String> {
    events.iter().find(|line| line.contains(needle)).cloned()
}

fn event_index(events: &[String], needle: &str) -> Option<usize> {
    events.iter().position(|line| line.contains(needle))
}

async fn wait_relay_event(path: &std::path::Path, needle: &str, timeout: Duration) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(line) = event_line(&read_relay_events(path), needle) { return Ok(line); }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "the relay never recorded {needle:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn event_line_with(events: &[String], first: &str, second: &str) -> Option<String> {
    events.iter().find(|line| line.contains(first) && line.contains(second)).cloned()
}

async fn wait_relay_event_with(path: &std::path::Path, first: &str, second: &str, timeout: Duration) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(line) = event_line_with(&read_relay_events(path), first, second) { return Ok(line); }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "the relay never recorded a {first:?} event carrying {second:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read_pid_record(path: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<i32>().ok()
}

/// The external process-state observer. `kill(pid, 0)` cannot distinguish a
/// live process from an unreaped zombie, and on macOS a process group whose only
/// member is a zombie answers `kill(-pgid, 0)` with `EPERM`, so neither helper
/// above can observe the F3 pin. `ps` reports the exact state character and the
/// parent pid instead.
fn process_state(pid: i32) -> Option<(String, i32)> {
    let output = std::process::Command::new("ps").args(["-o", "stat=,ppid=", "-p", &pid.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let state = fields.next()?.to_string();
    let parent = fields.next()?.parse::<i32>().ok()?;
    Some((state, parent))
}

fn unreaped_zombie_parented_by(pid: i32, parent: i32) -> bool {
    process_state(pid).is_some_and(|(state, ppid)| state.starts_with('Z') && ppid == parent)
}

async fn wait_unreaped_zombie(pid: i32, parent: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if unreaped_zombie_parented_by(pid, parent) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for a session directory to disappear without turning the wait itself
/// into an assertion. Callers assert on the returned observation.
async fn wait_dir_gone_bool(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

/// R8-F1: a control transport that dies abnormally must retire the endpoint's
/// encoder authority before the stream is dropped, so a closed transport is a
/// real retirement edge. The case is the adapter: it registers a live process
/// group over the real `encoder-control.sock`, then breaks the transport with a
/// malformed frame. The real endpoint reader bails on one of its non-retiring
/// exit paths; the retirement guard must clear the `Armed` registration and
/// settle or deny the in-flight permit before it drops the connection. The
/// observation is public: once the session finalizer has run, the endpoint must
/// not signal or kill the registered group, because the group is adapter-owned
/// after the close edge. Before the guard the endpoint finalizer still held
/// `Armed` authority and signalled the group, so this case is red.
#[tokio::test]
async fn real_endpoint_abnormal_control_close_retires_registration() {
    tokio::time::timeout(Duration::from_secs(120), abnormal_control_close_retirement())
        .await
        .expect("R8-F1 abnormal-close case timed out")
        .expect("R8-F1 abnormal-close case failed");
}

async fn abnormal_control_close_retirement() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("ob10g")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;
    let session_id = fixture.session_id();

    // A sleeper is the endpoint's direct adapter child. The case is the adapter
    // itself, so this binary never touches the control socket.
    let media_bin = root.join("sleeper.sh");
    std::fs::write(&media_bin, "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n")?;
    std::fs::set_permissions(&media_bin, std::fs::Permissions::from_mode(0o755))?;

    let mut endpoint = fixture.endpoint_with_media_bin(&media_bin, None)?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(session_id.as_str()), "Ready session changed: {ready}");
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    let control_socket = root.join(format!("endpoint/session-{session_id}/encoder-control.sock"));
    wait_socket(control_socket.clone()).await;
    let stream = tokio::net::UnixStream::connect(&control_socket).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);

    let acquired = exchange_frame(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 5 }).await?;
    anyhow::ensure!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }),
        "encoder acquire was not granted: {acquired:?}");

    // A real process group that records every SIGTERM and survives it, so a
    // single endpoint signal is observable and the group outlives the grace.
    let signals = root.join("group.signals");
    let group_ready = root.join("group.ready");
    let group_script = format!(
        "import signal, time\nsignals = {signals:?}\nready = {ready:?}\ndef on_term(signum, frame):\n    open(signals, 'a').write('15\\n')\nsignal.signal(signal.SIGTERM, on_term)\nopen(ready, 'w').close()\nwhile True:\n    time.sleep(0.2)\n",
        signals = signals.display().to_string(),
        ready = group_ready.display().to_string(),
    );
    let mut group = tokio::process::Command::new("python3")
        .arg("-c").arg(group_script)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pgid = i32::try_from(group.id().expect("group child pid"))?;
    wait_file(&group_ready, Duration::from_secs(10)).await?;
    anyhow::ensure!(unsafe { libc::getpgid(pgid) } == pgid, "test group child was not its own process group");

    let registered = exchange_frame(&mut read, &mut write, EncoderControlRequest::Register { registration_id: 5, pgid }).await?;
    anyhow::ensure!(matches!(registered, EncoderControlResponse::Register { refused: false }),
        "encoder registration was refused: {registered:?}");
    anyhow::ensure!(process_group_alive(pgid), "the registered group died before the transport broke");

    // Break the control transport with a malformed frame. The endpoint reader
    // bails on the malformed-frame exit path and must retire before dropping the
    // stream. Read until the endpoint closes its side: that close is the
    // retirement edge the adapter relies on.
    write.write_all(b"{\"op\":\"__r8_f1_break__\"}\n").await?;
    let mut scratch = [0u8; 64];
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match read.read(&mut scratch).await {
                Ok(0) => return Ok::<(), anyhow::Error>(()),
                Ok(_) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }).await.map_err(|_| anyhow::anyhow!("the endpoint never closed the control transport after the malformed frame"))??;

    // The endpoint supervisor runs the session finalizer when the reader exits.
    // Once the session directory is gone the finalizer has completed, so any
    // signal to the registered group would already have been delivered.
    let session_dir = root.join(format!("endpoint/session-{session_id}"));
    wait_dir_gone(&session_dir, Duration::from_secs(30)).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let recorded = std::fs::read_to_string(&signals).unwrap_or_default();
    anyhow::ensure!(recorded.trim().is_empty(),
        "R8-F1: the endpoint signalled the registered group after the control transport closed: {recorded:?}");
    anyhow::ensure!(process_group_alive(pgid),
        "R8-F1: the endpoint killed the registered group after the control transport closed");
    eprintln!("R8-F1: abnormal control close retired authority; the registered group is adapter-owned and untouched");

    // The adapter owns the group after the close edge; reap it here.
    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let _ = group.wait().await;

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

async fn exchange_frame(
    read: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    request: EncoderControlRequest,
) -> anyhow::Result<EncoderControlResponse> {
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line)).await
        .map_err(|_| anyhow::anyhow!("encoder control response deadline"))??;
    Ok(serde_json::from_str(line.trim_end())?)
}

fn process_alive(pid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn process_group_alive(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

async fn wait_signals(path: &std::path::Path, timeout: Duration) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if !text.trim().is_empty() { return Ok(text); }
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "the registered group never recorded a signal: {text:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_dir_gone(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline,
            "the endpoint finalizer returned before removing the session directory: {}", path.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
}

/// The real product binaries this device drives. The default is the binary cargo
/// built for this candidate. `R10_PRODUCT_BINARY_DIR` selects a different
/// directory of real binaries: the F1 minimal counterfactual runs the same test
/// against the baseline build from `m1-p3-r10-before`. It never selects a
/// scripted peer; the selected programs are still the real `obscura-host` and
/// `obscura-endpoint` binaries, built from real source.
fn product_binary(name: &str, built_for_candidate: &str) -> PathBuf {
    match std::env::var_os("R10_PRODUCT_BINARY_DIR") {
        Some(directory) => PathBuf::from(directory).join(name),
        None => PathBuf::from(built_for_candidate),
    }
}

fn host_binary() -> PathBuf {
    product_binary("obscura-host", env!("CARGO_BIN_EXE_obscura-host"))
}

fn endpoint_binary() -> PathBuf {
    product_binary("obscura-endpoint", env!("CARGO_BIN_EXE_obscura-endpoint"))
}

async fn connect_control(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
) -> anyhow::Result<(Socket, String)> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let (control, response) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await?;
    let token = response.headers()["x-obscura-media-token"].to_str()?.to_string();
    Ok((control, token))
}

async fn open_media(
    connector: &tokio_rustls::TlsConnector,
    address: std::net::SocketAddr,
    token: &str,
) -> anyhow::Result<Socket> {
    let tls = connector.connect(ServerName::try_from("localhost")?, tokio::net::TcpStream::connect(address).await?).await?;
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
    let (media, _) = client_async(request, tls).await?;
    Ok(media)
}

async fn next_json(control: &mut Socket) -> anyhow::Result<serde_json::Value> {
    let message = control.next().await.ok_or_else(|| anyhow::anyhow!("control socket closed"))??;
    Ok(serde_json::from_str(&message.into_text()?)?)
}

/// Read one `/media` frame header (4-byte big-endian length, then JSON).
async fn next_access_unit(media: &mut Socket) -> anyhow::Result<serde_json::Value> {
    loop {
        let message = media.next().await.ok_or_else(|| anyhow::anyhow!("media stream ended before an access unit"))??;
        let Message::Binary(bytes) = message else { continue; };
        anyhow::ensure!(bytes.len() >= 4, "media frame header is truncated");
        let length = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
        anyhow::ensure!(bytes.len() >= 4 + length, "media frame payload is truncated");
        let header: serde_json::Value = serde_json::from_slice(&bytes[4..4 + length])?;
        if header["type"] == "access_unit" { return Ok(header); }
    }
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn wait_file(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "file never appeared: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
}

async fn local_request(socket: &mut BufReader<tokio::net::UnixStream>, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    socket.get_mut().write_all(format!("{request}\n").as_bytes()).await?;
    let mut line = String::new();
    socket.read_line(&mut line).await?;
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    anyhow::ensure!(reply["type"] == "result", "Host request failed: {reply}");
    Ok(reply["value"].clone())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn write_authorized(
    registry_path: PathBuf,
    projection_path: PathBuf,
    profile_root: PathBuf,
    client_der: &[u8],
) -> anyhow::Result<()> {
    let fingerprint = hex(&Sha256::digest(client_der));
    let scopes = ["observe_human_control", "profile_read", "profile_create", "profile_select"];
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-b10",
        "devices": [{
            "device_id": "test-device",
            "device_label": "b10 client",
            "client_certificate_sha256": fingerprint,
            "scopes": scopes,
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1},
            "scope_revision": 1,
            "status": "active",
            "not_before_unix": 0,
            "not_after_unix": 4102444800i64,
            "created_at_unix": 0,
            "revoked_at_unix": null
        }]
    });
    std::fs::write(&registry_path, serde_json::to_vec(&registry)?)?;
    let projection = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "registry_sha256": "00".repeat(32),
        "clients": [{
            "device_id": "test-device",
            "client_certificate_sha256": fingerprint,
            "scopes": scopes,
            "initial_profile_grant": {"selection": "default_profile", "authorization_revision": 1}
        }]
    });
    std::fs::write(&projection_path, serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "test-device"]);
    let catalog = serde_json::json!({
        "catalog_revision": 1,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true,
            "access": {"owner_device_id": "local", "members": members}
        }],
        "default_profile_id": "default",
        "next_id": 1
    });
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}

#[tokio::test]
async fn real_endpoint_live_adapter_revoke_waits_for_endpoint_authority() {
    tokio::time::timeout(Duration::from_secs(180), revoke_during_endpoint_cleanup())
        .await
        .expect("F4.2 case timed out")
        .expect("F4.2 case failed");
}

/// F4.2: a live adapter that requests `Revoke` while the real endpoint owns the
/// group cleanup must not reap its leader before the endpoint retires signal
/// authority. The endpoint's typed revoke acknowledgement is the public signal
/// that authority is retired: `stop_armed_encoder_group` moves the registration
/// to `Revoking`, signals the group, and only calls `mark_revoked` after the
/// group settles. The group here ignores SIGTERM, so the endpoint spends its
/// full two second SIGTERM grace in `Revoking`; an adapter that reaped without
/// waiting would drop the leader inside its own one second grace.
async fn revoke_during_endpoint_cleanup() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("ob10d")?;
    let root = fixture.root.clone();
    let mut host = fixture.start_host().await?;

    let group_pgid = root.join("group.pgid");
    let adapter_pid_file = root.join("adapter.pid");
    let group_signals = root.join("group.signals");
    let helper = root.join("group.sh");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{pgid}'\nprintf '%s\\n' \"$PPID\" > '{adapter}'\ntrap 'printf \"15\\n\" >> \"{signals}\"' TERM\nwhile :; do sleep 0.05; done\n",
        pgid = group_pgid.display(),
        adapter = adapter_pid_file.display(),
        signals = group_signals.display(),
    );
    std::fs::write(&helper, script)?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;

    let mut endpoint = fixture.endpoint(Some(&helper))?;
    wait_socket(root.join("endpoint")).await;

    let connector = fixture.connector()?;
    let (mut control_socket, _token) = connect_control(&connector, fixture.address()).await?;
    let ready = next_json(&mut control_socket).await?;
    anyhow::ensure!(ready["session_id"].as_str() == Some(fixture.session_id().as_str()), "Ready session changed: {ready}");
    control_socket.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"observe"}}"#)).await?;
    let attached = next_json(&mut control_socket).await?;
    anyhow::ensure!(attached["type"] == "result", "observe attach failed: {attached}");

    wait_file(&group_pgid, Duration::from_secs(60)).await?;
    wait_file(&adapter_pid_file, Duration::from_secs(15)).await?;
    let adapter_pid: i32 = wait_pid_file(&adapter_pid_file, Duration::from_secs(30)).await?;
    let pgid = read_pgid(&group_pgid)?;
    // Let the real adapter commit `Armed` and block inside the encode.
    tokio::time::sleep(Duration::from_millis(800)).await;
    anyhow::ensure!(process_group_alive(pgid), "the registered group died before the endpoint took authority");

    // Release the final control reference. The real endpoint finalizer moves the
    // registration to `Revoking`, takes signal authority and signals the group.
    control_socket.send(Message::Close(None)).await?;
    drop(control_socket);
    let signals = wait_signals(&group_signals, Duration::from_secs(20)).await?;
    anyhow::ensure!(signals.contains("15"),
        "F4.2: the endpoint did not take signal authority over the registered group: {signals:?}");
    anyhow::ensure!(process_group_alive(pgid), "the group settled before the adapter requested revoke");

    // Interrupt the live adapter. Its `finalize()` must request revoke and wait
    // for the endpoint's typed acknowledgement before it reaps the leader.
    anyhow::ensure!(unsafe { libc::kill(adapter_pid, libc::SIGTERM) } == 0, "SIGTERM the live adapter");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    anyhow::ensure!(process_group_alive(pgid),
        "F4.2: the adapter reaped the registered leader before the endpoint retired signal authority");
    anyhow::ensure!(process_alive(adapter_pid),
        "F4.2: the adapter exited before it consumed the endpoint's revoke acknowledgement");

    // The endpoint's SIGTERM grace ends, `mark_revoked` retires authority, the
    // adapter consumes the acknowledgement and reaps the retained leader.
    anyhow::ensure!(wait_group_reaped(pgid, Duration::from_secs(15)).await,
        "F4.2: the adapter never reaped the retained leader");
    anyhow::ensure!(wait_process_gone(adapter_pid, Duration::from_secs(15)).await,
        "F4.2: the adapter did not exit after reaping its leader");

    let session_dir = root.join(format!("endpoint/session-{}", fixture.session_id()));
    wait_dir_gone(&session_dir, Duration::from_secs(20)).await?;
    eprintln!("F4.2: live adapter revoke waited for endpoint authority before leader reap");

    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn group_reaped(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    let result = unsafe { libc::kill(-pgid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

async fn wait_group_reaped(pgid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if group_reaped(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_process_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_alive(pid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
