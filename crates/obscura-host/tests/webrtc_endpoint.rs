#![cfg(unix)]

use std::{path::PathBuf, time::Duration};

use std::process::Stdio;

use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::TcpListener, process::Command};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}};

#[tokio::test]
async fn real_receiver_gets_continuous_host_media_and_rejects_stale_binding() {
    tokio::time::timeout(Duration::from_secs(180), exercise(false)).await.expect("WebRTC endpoint integration timed out").unwrap();
}

#[tokio::test]
async fn real_receiver_gets_typed_encoder_unavailable_error() {
    tokio::time::timeout(Duration::from_secs(180), exercise(true)).await.expect("WebRTC encoder failure integration timed out").unwrap();
}

#[tokio::test]
async fn same_content_new_sequence_publishes_each_frame() {
    tokio::time::timeout(Duration::from_secs(180), run_scripted_lifecycle("same-content"))
        .await.expect("same-content lifecycle timed out").unwrap();
}

#[tokio::test]
async fn foreign_session_then_matching_a_is_held_until_admitted() {
    tokio::time::timeout(Duration::from_secs(180), run_scripted_lifecycle("foreign-then-a"))
        .await.expect("foreign-session lifecycle timed out").unwrap();
}

#[tokio::test]
async fn initial_none_waits_for_first_admitted_frame() {
    tokio::time::timeout(Duration::from_secs(180), run_scripted_lifecycle("initial-none"))
        .await.expect("initial-None lifecycle timed out").unwrap();
}

#[tokio::test]
async fn active_receiver_observes_detach_terminal_and_reconnect() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("detach"))
        .await.expect("detach terminal lifecycle timed out").unwrap();
}

#[tokio::test]
async fn active_receiver_observes_rebind_terminal_without_leak() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("rebind"))
        .await.expect("rebind terminal lifecycle timed out").unwrap();
}

#[tokio::test]
async fn active_receiver_observes_saturated_request_queue_terminal_without_leak() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("backpressure-detach"))
        .await.expect("saturated request queue terminal lifecycle timed out").unwrap();
}

#[tokio::test]
async fn active_receiver_observes_source_close_terminal() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("none"))
        .await.expect("source-close terminal lifecycle timed out").unwrap();
}

// Separate coverage: the same fence issued on the mTLS control socket must
// still deliver its correlated receipt and fence the old stream.
#[tokio::test]
async fn active_receiver_observes_wss_detach_terminal() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("wss-detach"))
        .await.expect("WSS detach terminal lifecycle timed out").unwrap();
}

#[tokio::test]
async fn active_receiver_observes_wss_rebind_terminal_without_leak() {
    tokio::time::timeout(Duration::from_secs(240), run_terminal_lifecycle("wss-rebind"))
        .await.expect("WSS rebind terminal lifecycle timed out").unwrap();
}

/// A target session whose exact media binding could not be acquired must not
/// be reported as a successful rebind. Over the DataChannel browser path the
/// retained finalization failure has to become the correlated client error,
/// exactly as the WSS control path returns it.
#[tokio::test]
async fn data_channel_rebind_projects_retained_finalization_failure() {
    tokio::time::timeout(Duration::from_secs(240), run_retained_finalize_failure_data_channel())
        .await.expect("retained finalization failure DataChannel case timed out").unwrap();
}

/// F2: the fixture launches the p1 adapter as part of the prime attachment. A
/// post-launch receiver failure must still remove that exact adapter pid before
/// host and endpoint teardown, even though the case returns before the report
/// verification runs.
#[tokio::test]
async fn retained_finalization_failure_receiver_error_cleans_up_launched_adapter() {
    let sink = fixture_pid_sink();
    let outcome = run_fixture_fault("error", &sink, Duration::from_secs(240)).await;
    assert!(outcome.is_err(), "the injected post-launch receiver failure must fail the case");
    assert_recorded_adapter_gone(&sink).await;
    // R8-F5's "no second SIGKILL to the recorded pid" property is asserted by the
    // F5 black-box device (`r10_f5_cleanup_disarms_external_pid_sink`), which
    // observes the real outbound signal from outside this process through an
    // injected observer. No in-process counter is kept: it would be a second,
    // independently unverified source for the same fact.
}

/// F2: the outer case timeout must also reach cleanup. `hang` blocks after the
/// p1 adapter is launched, so the watchdog drops the case future; the fixture's
/// backstop must still remove the recorded adapter pid.
#[tokio::test]
async fn retained_finalization_failure_timeout_cleans_up_launched_adapter() {
    let sink = fixture_pid_sink();
    let outcome = run_fixture_fault("hang", &sink, Duration::from_secs(90)).await;
    assert!(outcome.is_err(), "the hanging case must be cancelled by the case timeout");
    assert_recorded_adapter_gone(&sink).await;
}

/// R8-F6: cleanup failure must be reported and must retain the records needed to
/// finish the job. A non-numeric pid record cannot be verified as terminated, so
/// the cleanup owner must return an error, keep the pid record, and stay armed
/// instead of silently removing the record and dropping signal authority.
#[tokio::test]
async fn final_failure_fixture_retains_pid_record_when_termination_unverified() {
    let sink = fixture_pid_sink();
    std::fs::write(&sink, b"not-a-pid").unwrap();
    let endpoint = Command::new("sleep").arg("300").kill_on_drop(true).spawn().unwrap();
    let host = Command::new("sleep").arg("300").kill_on_drop(true).spawn().unwrap();
    let mut fixture = FinalFailureFixture {
        label: "r8-f6 cleanup-failure fixture",
        pid_file: sink.clone(),
        adapter_termination_owed: true,
        endpoint,
        host,
        producer_task: None,
        root: PathBuf::new(),
    };
    let cleanup = fixture.cleanup().await;
    assert!(cleanup.is_err(), "cleanup must report the unverified adapter termination");
    let message = format!("{:#}", cleanup.unwrap_err());
    assert!(message.contains("cleanup incomplete"), "cleanup error must be explicit: {message}");
    assert!(sink.exists(), "cleanup must retain the pid record when termination is not verified");
    assert!(fixture.adapter_termination_owed, "cleanup must leave the termination owner armed");
    // Disarm before Drop so the test-owned signal arm is not left set, then
    // remove the retained evidence record.
    fixture.adapter_termination_owed = false;
    let _ = std::fs::remove_file(&sink);
}

/// Two authorized receivers bound to two distinct Host profiles run at the same
/// time. Each exact session owns its own adapter and encoded socket, so every
/// frame a receiver decodes must carry its own session identity.
#[tokio::test]
async fn two_concurrent_authorized_receivers_keep_distinct_sessions() {
    tokio::time::timeout(Duration::from_secs(240), run_two_session_isolation())
        .await.expect("two-session isolation lifecycle timed out").unwrap();
}

async fn exercise(encoder_failure: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // The r11 exact-session sockets live at `socket_dir/session-<36>/...`, so
    // keep the root short enough to stay below the macOS SUN_LEN bound.
    let root = PathBuf::from("/tmp").join(format!("ow-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let raw_root = raw_webrtc_root("ow");
    std::fs::create_dir_all(&raw_root)?;
    std::fs::write(raw_root.join("command.env"), format!(
        "encoder_failure={encoder_failure}\nUTC={}\n",
        std::process::Command::new("/bin/date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    ))?;
    let bind_ip = "127.0.0.1";
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura WebRTC integration CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
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

    let host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(std::fs::File::create(raw_root.join("host.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("host.stderr"))?)
        .kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("host.pid"), host.id().expect("Host child pid").to_string())?;
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await?;
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await?;
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let html = "data:text/html,<style>body{margin:0;background:white}button{width:80px;height:80px;border:0;background:red}</style><button id='target' onclick=\"this.style.background='lime'\">target</button>";
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?; drop(reservation);
    let media_bin = release_binary("obscura-media");
    let receiver_bin = release_binary("obscura-webrtc-receiver");
    anyhow::ensure!(media_bin.is_file(), "Build target/release/obscura-media with --features webrtc first");
    anyhow::ensure!(receiver_bin.is_file(), "Build target/release/obscura-webrtc-receiver with --features webrtc first");
    let mut endpoint_command = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
    endpoint_command
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der")).arg("--media-bin").arg(&media_bin)
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip);
    endpoint_command
        .stdout(std::fs::File::create(raw_root.join("endpoint.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("endpoint.stderr"))?);
    if encoder_failure { endpoint_command.arg("--ffmpeg").arg("/nonexistent-obscura-encoder"); }
    let endpoint = endpoint_command.kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("endpoint.pid"), endpoint.id().expect("endpoint child pid").to_string())?;
    wait_socket(root.join("endpoint")).await;

    let first = run_receiver(&receiver_bin, address, &root, &raw_root, "first", false, false, encoder_failure).await?;
    if encoder_failure {
        anyhow::ensure!(first.status.success(), "typed encoder failure receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&first.stderr));
        let report: serde_json::Value = serde_json::from_slice(&first.stdout)?;
        anyhow::ensure!(report["pass"] == true && report["error_code"] == "ENCODER_UNAVAILABLE",
            "remote encoder failure was not typed: {report}");
        let mut endpoint = endpoint;
        let mut host = host;
        let _ = endpoint.kill().await;
        let _ = endpoint.wait().await;
        let _ = host.kill().await;
        let _ = host.wait().await;
        std::fs::remove_dir_all(root)?;
        return Ok(());
    }
    anyhow::ensure!(first.status.success(), "independent WebRTC receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&first.stderr));
    let first_report: serde_json::Value = serde_json::from_slice(&first.stdout)?;
    anyhow::ensure!(first_report["pass"] == true && first_report["browser_path"] == "webrtc_data_channel", "WebRTC DataChannel browser path missing: {first_report}");
    let ice = &first_report["ice"];
    anyhow::ensure!(ice["transport"] == "udp" && ice["local_candidate"]["protocol"] == "udp" && ice["remote_candidate"]["protocol"] == "udp",
        "selected ICE pair was not UDP: {first_report}");
    anyhow::ensure!(ice["local_candidate"]["candidate_type"] == "host" && ice["remote_candidate"]["candidate_type"] == "host",
        "selected ICE pair was not host/host: {first_report}");
    let frames = first_report["frames"].as_array().ok_or_else(|| anyhow::anyhow!("continuous WebRTC frame evidence missing: {first_report}"))?;
    anyhow::ensure!(frames.len() >= 3, "continuous WebRTC evidence has fewer than three frames: {first_report}");
    for frame in frames {
        anyhow::ensure!(frame["session_id"] == first_report["session_id"]
            && frame["width"] == 160 && frame["height"] == 120 && frame["stride"] == 640
            && frame["coded_width"] == 160 && frame["coded_height"] == 120
            && frame["codec"] == "h264_annex_b" && frame["keyframe"] == true,
            "incomplete WebRTC frame descriptor: {frame}");
        anyhow::ensure!(frame["encoder_id"].as_str().is_some_and(|value| !value.is_empty())
            && frame["rtp_timestamp"].as_u64().is_some()
            && frame["pts_us"].as_u64().is_some()
            && frame["source_byte_length"].as_u64().is_some_and(|value| value > 0)
            && frame["access_unit_bytes"].as_u64().is_some_and(|value| value > 0)
            && frame["rtp_packets"].as_u64().is_some_and(|value| value > 0),
            "incomplete WebRTC frame identity or RTP evidence: {frame}");
    }
    anyhow::ensure!(frames.iter().any(|frame| frame["changed"] == true), "Host click did not produce a different WebRTC frame: {first_report}");

    let stale = run_receiver(&receiver_bin, address, &root, &raw_root, "stale", false, true, false).await?;
    anyhow::ensure!(stale.status.success(), "stale-binding receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&stale.stderr));
    let stale_report: serde_json::Value = serde_json::from_slice(&stale.stdout)?;
    anyhow::ensure!(stale_report["negative"] == "stale_binding_rejected", "stale binding was not rejected: {stale_report}");

    let second = run_receiver(&receiver_bin, address, &root, &raw_root, "reconnect", true, false, false).await?;
    anyhow::ensure!(second.status.success(), "reconnected WebRTC receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&second.stderr));
    let second_report: serde_json::Value = serde_json::from_slice(&second.stdout)?;
    anyhow::ensure!(second_report["pass"] == true, "reconnect evidence missing: {second_report}");
    anyhow::ensure!(second_report["session_id"] == first_report["session_id"], "reconnect changed Host Session identity");
    anyhow::ensure!(second_report["attachment_id"] != first_report["attachment_id"], "reconnect reused Host attachment identity");

    let mut endpoint = endpoint;
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let mut host = host;
    let _ = host.kill().await;
    let _ = host.wait().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

/// Typed `AccessUnit` for the endpoint's declared shared-encoded-frame ABI
/// (`--media-bin` writes `VideoPacket` headers plus payload to `encoded.sock`).
fn scripted_access_unit(session_id: &str, sequence: u64, access_unit_bytes: u64) -> obscura_host_protocol::VideoPacket {
    let source_byte_length = 160u64 * 120 * 4;
    obscura_host_protocol::VideoPacket::AccessUnit {
        source: obscura_host_protocol::FrameInfo {
            session_id: session_id.to_owned(),
            tab_id: "scripted-tab".into(),
            sequence,
            presentation_revision: 1,
            document_revision: 1,
            viewport_revision: 1,
            width: 160,
            height: 120,
            stride: 640,
            byte_length: source_byte_length,
            pixel_format: obscura_host_protocol::PixelFormat::PremultipliedRgba8,
        },
        encoder_id: "scripted-encoder".into(),
        pts_us: sequence,
        coded_width: 160,
        coded_height: 120,
        codec: obscura_host_protocol::VideoCodec::H264AnnexB,
        keyframe: true,
        byte_length: access_unit_bytes,
    }
}

/// Opaque premultiplied RGBA scene used once to produce a real H.264 IDR.
fn scripted_scene(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let x = index as u32 % width;
        let y = index as u32 / width;
        let band = ((x / 20) + (y / 20)) % 2 == 0;
        pixel[0] = if band { 200 } else { 40 };
        pixel[1] = if band { 40 } else { 160 };
        pixel[2] = 80;
        pixel[3] = 255;
    }
    rgba
}

async fn send_video_packet(stream: &mut tokio::net::UnixStream, packet: &obscura_host_protocol::VideoPacket, bytes: &[u8]) -> anyhow::Result<()> {
    let mut header = serde_json::to_vec(packet)?;
    anyhow::ensure!(header.len() < 4096, "scripted shared-encoded-frame header exceeds endpoint budget");
    header.push(b'\n');
    stream.write_all(&header).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Drive the endpoint's declared `--media-bin` / `encoded.sock` shared-encoded
/// frame ABI with a scripted producer while a real independent receiver decodes
/// the Host H.264 over real WebRTC. This injects the exact typed source
/// sequences (foreign session, identical bytes with a new sequence, initially
/// empty source) that the real single-session adapter cannot produce on its own.
async fn run_scripted_lifecycle(scenario: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::process::Stdio;
    let root = PathBuf::from("/tmp").join(format!("ow-scripted-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let raw_root = raw_webrtc_root(&format!("scripted-{scenario}"));
    std::fs::create_dir_all(&raw_root)?;
    std::fs::write(raw_root.join("command.env"), format!(
        "scenario={scenario}\nUTC={}\n",
        std::process::Command::new("/bin/date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    ))?;
    let bind_ip = "127.0.0.1";
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura WebRTC scripted CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
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

    let mut host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(std::fs::File::create(raw_root.join("host.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("host.stderr"))?)
        .kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("host.pid"), host.id().expect("Host child pid").to_string())?;
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await?;
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await?;
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let session_id = state["session_id"].as_str().ok_or_else(|| anyhow::anyhow!("Host session identity missing"))?.to_owned();
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:160px;height:120px;background:white'></div>";
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?; drop(reservation);
    let receiver_bin = release_binary("obscura-webrtc-receiver");
    anyhow::ensure!(receiver_bin.is_file(), "Build target/release/obscura-webrtc-receiver with --features webrtc first");
    let mut endpoint_command = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
    endpoint_command
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        // The test itself owns the shared-encoded-frame producer on `encoded.sock`;
        // a no-op `--media-bin` leaves the socket to the scripted producer.
        .arg("--media-bin").arg("/usr/bin/true")
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip);
    endpoint_command
        .stdout(std::fs::File::create(raw_root.join("endpoint.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("endpoint.stderr"))?);
    let endpoint = endpoint_command.kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("endpoint.pid"), endpoint.id().expect("endpoint child pid").to_string())?;
    wait_socket(root.join("endpoint")).await;

    let mut receiver_command = Command::new(&receiver_bin);
    receiver_command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
        .arg("--client-cert").arg(root.join("client.der")).arg("--client-key").arg(root.join("client.key"))
        .arg("--frames").arg("3").arg("--skip-click")
        .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let receiver = receiver_command.spawn()?;

    // The r11 endpoint binds the exact-session encoded socket only on
    // admission, so the receiver must attach before the producer connects.
    let session_socket = root.join(format!("endpoint/session-{session_id}/encoded.sock"));
    wait_socket(session_socket.clone()).await;
    let mut producer = tokio::net::UnixStream::connect(&session_socket).await?;

    let bytes = obscura_media::encode(std::path::Path::new("ffmpeg"), 160, 120, scripted_scene(160, 120)).await?;
    anyhow::ensure!(!bytes.is_empty(), "scripted producer got no H.264 access unit");
    let foreign_session = format!("{session_id}-foreign");
    if scenario == "initial-none" {
        // Hold the shared source empty until the attachment filter is admitted
        // and running with no matching frame; a `None`-as-fence bug fails here.
        tokio::time::sleep(Duration::from_secs(5)).await;
    } else {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    let mut released = false;
    for sequence in 1..=12u64 {
        if scenario == "foreign-then-a" {
            let foreign = scripted_access_unit(&foreign_session, sequence, bytes.len() as u64);
            if send_video_packet(&mut producer, &foreign, &bytes).await.is_err() { released = true; break; }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let packet = scripted_access_unit(&session_id, sequence, bytes.len() as u64);
        if send_video_packet(&mut producer, &packet, &bytes).await.is_err() { released = true; break; }
        tokio::time::sleep(Duration::from_millis(350)).await;
    }

    let output = receiver.wait_with_output().await?;
    let phase_dir = raw_root.join("scripted");
    std::fs::create_dir_all(&phase_dir)?;
    std::fs::write(phase_dir.join("producer-release.txt"), format!(
        "observed={released}\nsession_socket_exists={}\n",
        session_socket.exists()
    ))?;
    anyhow::ensure!(released, "scripted producer never observed the designed session release");
    std::fs::write(phase_dir.join("stdout.txt"), &output.stdout)?;
    std::fs::write(phase_dir.join("stderr.txt"), &output.stderr)?;
    std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", output.status.code()))?;
    anyhow::ensure!(output.status.success(), "scripted receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(report["pass"] == true && report["browser_path"] == "webrtc_data_channel", "scripted WebRTC path missing: {report}");
    anyhow::ensure!(report["session_id"].as_str() == Some(session_id.as_str()), "scripted receiver session changed: {report}");
    let frames = report["frames"].as_array().ok_or_else(|| anyhow::anyhow!("scripted frame evidence missing: {report}"))?;
    anyhow::ensure!(frames.len() >= 3, "scripted WebRTC evidence has fewer than three frames: {report}");
    for frame in frames {
        anyhow::ensure!(frame["session_id"].as_str() == Some(session_id.as_str())
            && frame["width"] == 160 && frame["height"] == 120 && frame["stride"] == 640
            && frame["codec"] == "h264_annex_b" && frame["keyframe"] == true,
            "incomplete scripted frame descriptor: {frame}");
    }
    let sequences: Vec<u64> = frames.iter().map(|frame| frame["sequence"].as_u64().unwrap_or(0)).collect();
    anyhow::ensure!(sequences.windows(2).all(|pair| pair[1] > pair[0]), "scripted sequence did not increase: {report}");

    drop(producer);
    let mut endpoint = endpoint;
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let mut host = host;
    let _ = host.kill().await;
    let _ = host.wait().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

/// Keep a real receiver authenticated and active across the terminal the old
/// harness never reached. `mode` selects the fence the receiver itself triggers
/// over the authenticated DataChannel after the first decoded frame
/// (`detach`/`rebind`) or a wait for the external producer to close (`none`).
/// The receiver reports the correlated BrowserResponse it received at the
/// trigger request id before it reports the terminal, so the Host cannot close
/// the peer first and leave the initiating operation without its result. Every
/// decoded frame must belong to the admitted session, and after a fence a
/// fresh authorized attachment must still receive media from the still-running
/// producer.
async fn run_terminal_lifecycle(mode: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::process::Stdio;
    anyhow::ensure!(matches!(mode, "detach" | "rebind" | "none" | "wss-detach" | "wss-rebind" | "backpressure-detach"),
        "unsupported terminal mode {mode}");
    let (trigger, transport) = match mode {
        "wss-detach" | "wss-rebind" => (&mode[4..], "wss"),
        "backpressure-detach" => ("detach-backpressure", "data-channel"),
        other => (other, "data-channel"),
    };
    let root = PathBuf::from("/tmp").join(format!("ow-terminal-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let raw_root = raw_webrtc_root(&format!("terminal-{mode}"));
    std::fs::create_dir_all(&raw_root)?;
    std::fs::write(raw_root.join("command.env"), format!(
        "scenario=terminal-{mode}\nUTC={}\n",
        std::process::Command::new("/bin/date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    ))?;
    let bind_ip = "127.0.0.1";
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura WebRTC terminal CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
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

    let mut host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(std::fs::File::create(raw_root.join("host.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("host.stderr"))?)
        .kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("host.pid"), host.id().expect("Host child pid").to_string())?;
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await?;
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await?;
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let session_id = state["session_id"].as_str().ok_or_else(|| anyhow::anyhow!("Host session identity missing"))?.to_owned();
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:160px;height:120px;background:white'></div>";
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?; drop(reservation);
    let receiver_bin = release_binary("obscura-webrtc-receiver");
    anyhow::ensure!(receiver_bin.is_file(), "Build target/release/obscura-webrtc-receiver with --features webrtc first");
    let mut endpoint_command = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
    endpoint_command
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        // The test owns the shared-encoded-frame producer on `encoded.sock`.
        .arg("--media-bin").arg("/usr/bin/true")
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip);
    endpoint_command
        .stdout(std::fs::File::create(raw_root.join("endpoint.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("endpoint.stderr"))?);
    let endpoint = endpoint_command.kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("endpoint.pid"), endpoint.id().expect("endpoint child pid").to_string())?;
    wait_socket(root.join("endpoint")).await;

    let bytes = obscura_media::encode(std::path::Path::new("ffmpeg"), 160, 120, scripted_scene(160, 120)).await?;
    anyhow::ensure!(!bytes.is_empty(), "terminal producer got no H.264 access unit");
    // The producer stays live across the fence so a stream that fails to
    // terminate keeps delivering admitted-session frames and trips the
    // receiver's finite deadline instead of silently passing.
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

    let ready_file = root.join("ready.json");
    let mut receiver_command = Command::new(&receiver_bin);
    receiver_command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
        .arg("--client-cert").arg(root.join("client.der")).arg("--client-key").arg(root.join("client.key"))
        .arg("--frames").arg("3").arg("--skip-click")
        .arg("--terminal-trigger").arg(trigger).arg("--terminal-transport").arg(transport)
        .arg("--ready-file").arg(&ready_file)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let receiver = receiver_command.spawn()?;

    // The r11 endpoint binds the exact-session encoded socket only on
    // admission, so the receiver must attach before the producer connects.
    let session_socket = root.join(format!("endpoint/session-{session_id}/encoded.sock"));
    wait_socket(session_socket.clone()).await;
    let producer = tokio::net::UnixStream::connect(&session_socket).await?;
    let producer_task = {
        let session_id = session_id.clone();
        let bytes = bytes.clone();
        tokio::spawn(async move {
            let mut producer = producer;
            let mut sequence = 0u64;
            loop {
                sequence += 1;
                let packet = scripted_access_unit(&session_id, sequence, bytes.len() as u64);
                if send_video_packet(&mut producer, &packet, &bytes).await.is_err() { break; }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                    changed = stop_rx.changed() => { if changed.is_err() || *stop_rx.borrow() { break; } }
                }
                if *stop_rx.borrow() { break; }
            }
            drop(producer);
        })
    };

    if trigger == "none" {
        // The receiver writes the ready marker only after decoding its first
        // frame; only then does the external producer close.
        wait_file(&ready_file, Duration::from_secs(60)).await?;
        let _ = stop_tx.send(true);
    }

    let output = receiver.wait_with_output().await?;
    let phase_dir = raw_root.join("terminal");
    std::fs::create_dir_all(&phase_dir)?;
    std::fs::write(phase_dir.join("stdout.txt"), &output.stdout)?;
    std::fs::write(phase_dir.join("stderr.txt"), &output.stderr)?;
    std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", output.status.code()))?;
    anyhow::ensure!(output.status.success(), "terminal receiver failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(report["pass"] == true, "terminal report not passing: {report}");
    anyhow::ensure!(report["session_id"].as_str() == Some(session_id.as_str()), "terminal session changed: {report}");
    let terminal = report["terminal"]["kind"].as_str().ok_or_else(|| anyhow::anyhow!("terminal kind missing: {report}"))?;
    anyhow::ensure!(!terminal.is_empty(), "terminal kind empty: {report}");
    let frames = report["frames"].as_array().ok_or_else(|| anyhow::anyhow!("terminal frame evidence missing: {report}"))?;
    anyhow::ensure!(!frames.is_empty(), "terminal evidence decoded no frames: {report}");
    for frame in frames {
        anyhow::ensure!(frame["session_id"].as_str() == Some(session_id.as_str()),
            "terminal stream carried a non-admitted frame: {frame}");
    }
    if trigger == "detach" || trigger == "detach-backpressure" {
        let receipt = &report["trigger_receipt"];
        anyhow::ensure!(receipt["type"] == "result" && receipt["id"] == 200, "detach produced no correlated result receipt: {report}");
        anyhow::ensure!(receipt["value"]["attachment_id"].is_null(),
            "detach receipt still reports an attachment: {receipt}");
    }
    if trigger == "rebind" {
        let receipt = &report["trigger_receipt"];
        anyhow::ensure!(receipt["type"] == "result" && receipt["id"] == 200, "rebind produced no correlated result receipt: {report}");
        let rebound = receipt["value"]["binding"]["session_id"].as_str().unwrap_or("");
        anyhow::ensure!(!rebound.is_empty() && rebound != session_id,
            "rebind receipt did not move the attachment to a new session: {receipt}");
    }

    let _ = stop_tx.send(true);
    let _ = producer_task.await;
    if trigger != "none" {
        // Zero media references finalize the old adapter. A fresh authorized
        // attachment must re-acquire the session and receive a new source.
        wait_absent(&session_socket, Duration::from_secs(15)).await?;
        let mut fresh_command = Command::new(&receiver_bin);
        fresh_command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
            .arg("--client-cert").arg(root.join("client.der")).arg("--client-key").arg(root.join("client.key"))
            .arg("--frames").arg("3").arg("--skip-click")
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let fresh = fresh_command.spawn()?;
        wait_socket(session_socket.clone()).await;
        let fresh_producer = tokio::spawn({
            let session_socket = session_socket.clone();
            let session_id = session_id.clone();
            let bytes = bytes.clone();
            async move {
                let mut producer = tokio::net::UnixStream::connect(&session_socket).await?;
                for sequence in 1..=12u64 {
                    let packet = scripted_access_unit(&session_id, sequence, bytes.len() as u64);
                    if send_video_packet(&mut producer, &packet, &bytes).await.is_err() { break; }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                Ok::<(), anyhow::Error>(())
            }
        });
        let fresh = fresh.wait_with_output().await?;
        let _ = fresh_producer.await;
        anyhow::ensure!(fresh.status.success(), "fresh authorized attachment failed after {mode}: {}", String::from_utf8_lossy(&fresh.stderr));
        let phase_dir = raw_root.join(format!("after-{mode}"));
        std::fs::create_dir_all(&phase_dir)?;
        std::fs::write(phase_dir.join("stdout.txt"), &fresh.stdout)?;
        std::fs::write(phase_dir.join("stderr.txt"), &fresh.stderr)?;
        std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", fresh.status.code()))?;
        let fresh_report: serde_json::Value = serde_json::from_slice(&fresh.stdout)?;
        anyhow::ensure!(fresh_report["pass"] == true && fresh_report["session_id"].as_str() == Some(session_id.as_str()),
            "fresh authorized attachment evidence missing after {mode}: {fresh_report}");
    }

    let mut endpoint = endpoint;
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let mut host = host;
    let _ = host.kill().await;
    let _ = host.wait().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

/// Drive two concurrent authorized receivers, each paired with a distinct Host
/// profile. Both exact sessions own their own adapter and encoded socket, so
/// every frame a receiver decodes must carry its own session identity.
async fn run_two_session_isolation() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ow-two-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let raw_root = raw_webrtc_root("two-session");
    std::fs::create_dir_all(&raw_root)?;
    std::fs::write(raw_root.join("command.env"), format!(
        "scenario=two-session\nUTC={}\n",
        std::process::Command::new("/bin/date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    ))?;
    let bind_ip = "127.0.0.1";
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura two-session CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
        .signed_by(&server_key, &ca, &ca_key)?;
    let client_a_key = rcgen::KeyPair::generate()?;
    let client_b_key = rcgen::KeyPair::generate()?;
    let mut client_params = rcgen::CertificateParams::new(vec![])?;
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client_a = client_params.clone().signed_by(&client_a_key, &ca, &ca_key)?;
    let client_b = client_params.clone().signed_by(&client_b_key, &ca, &ca_key)?;
    std::fs::write(root.join("server.der"), server.der())?;
    std::fs::write(root.join("server.key"), server_key.serialize_der())?;
    std::fs::set_permissions(root.join("server.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("ca.der"), ca.der())?;
    std::fs::write(root.join("client-a.der"), client_a.der())?;
    std::fs::write(root.join("client-a.key"), client_a_key.serialize_der())?;
    std::fs::set_permissions(root.join("client-a.key"), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(root.join("client-b.der"), client_b.der())?;
    std::fs::write(root.join("client-b.key"), client_b_key.serialize_der())?;
    std::fs::set_permissions(root.join("client-b.key"), std::fs::Permissions::from_mode(0o600))?;
    write_authorized_two(root.join("registry.json"), root.join("authorized-clients.json"),
        root.join("profiles"), &client_a.der().to_vec(), &client_b.der().to_vec())?;

    let mut host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(std::fs::File::create(raw_root.join("host.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("host.stderr"))?)
        .kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("host.pid"), host.id().expect("Host child pid").to_string())?;
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await?;
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await?;
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let default_session = state["session_id"].as_str().ok_or_else(|| anyhow::anyhow!("Host default session missing"))?.to_owned();
    let html = "data:text/html,<style>body{margin:0;background:white}button{width:80px;height:80px;border:0;background:red}</style><button id='target'>target</button>";
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    // Materialize the secondary profile/session and give it its own page so the
    // second adapter has a real frame source when its receiver attaches.
    let bound = local_request(&mut local, serde_json::json!({"id":4,"operation":identity(&state),"command":{"type":"profile_select","profile_id":"p1"}})).await?;
    let secondary_session = bound["status"]["session_id"].as_str().ok_or_else(|| anyhow::anyhow!("Host secondary session missing"))?.to_owned();
    anyhow::ensure!(default_session != secondary_session, "two profiles resolved to one session");
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?; drop(reservation);
    let media_bin = release_binary("obscura-media");
    let receiver_bin = release_binary("obscura-webrtc-receiver");
    anyhow::ensure!(media_bin.is_file(), "Build target/release/obscura-media with --features webrtc first");
    anyhow::ensure!(receiver_bin.is_file(), "Build target/release/obscura-webrtc-receiver with --features webrtc first");
    let mut endpoint_command = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"));
    endpoint_command
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        .arg("--media-bin").arg(&media_bin)
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip);
    endpoint_command
        .stdout(std::fs::File::create(raw_root.join("endpoint.stdout"))?)
        .stderr(std::fs::File::create(raw_root.join("endpoint.stderr"))?);
    let endpoint = endpoint_command.kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("endpoint.pid"), endpoint.id().expect("endpoint child pid").to_string())?;
    wait_socket(root.join("endpoint")).await;

    let (report_a, report_b) = tokio::try_join!(
        run_receiver_cert(&receiver_bin, address, &root, &raw_root, "profile-a", "client-a"),
        run_receiver_cert(&receiver_bin, address, &root, &raw_root, "profile-b", "client-b"),
    )?;
    anyhow::ensure!(report_a["pass"] == true && report_b["pass"] == true,
        "concurrent receivers did not both pass: a={report_a} b={report_b}");
    let session_a = report_a["session_id"].as_str().unwrap_or("");
    let session_b = report_b["session_id"].as_str().unwrap_or("");
    anyhow::ensure!(!session_a.is_empty() && !session_b.is_empty() && session_a != session_b,
        "concurrent receivers did not bind distinct sessions: a={session_a} b={session_b}");
    for (label, report, expected) in [("a", &report_a, session_a), ("b", &report_b, session_b)] {
        let frames = report["frames"].as_array().ok_or_else(|| anyhow::anyhow!("receiver {label} frame evidence missing: {report}"))?;
        anyhow::ensure!(frames.len() >= 3, "receiver {label} decoded fewer than three frames: {report}");
        for frame in frames {
            anyhow::ensure!(frame["session_id"].as_str() == Some(expected),
                "receiver {label} decoded a frame from another session: {frame}");
        }
    }

    let mut endpoint = endpoint;
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let mut host = host;
    let _ = host.kill().await;
    let _ = host.wait().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

/// Real two-profile WebRTC regression with a retained failed target session.
///
/// The `p1` adapter acquires an encoder permit, never binds a pgid, and keeps
/// the adapter alive through the endpoint's cooperative SIGTERM. The endpoint's
/// `p1` media entry therefore runs its finalizer, cannot settle the permit, and
/// is retained in `CompletionState::Failed`. A receiver on the default profile
/// is then asked to rebind to `p1` over the authenticated DataChannel. The Host
/// answers `ProfileSelected`; only the pre-dispatch acquisition result may turn
/// that into the correlated `SESSION_MEDIA_FINALIZE_FAILED` error. Pre-fix the
/// DataChannel path published the success and kept the failure on stderr.
async fn run_retained_finalize_failure_data_channel() -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ow-finalfail-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let raw_root = raw_webrtc_root("finalfail-dc");
    std::fs::create_dir_all(&raw_root)?;
    std::fs::write(raw_root.join("command.env"), format!(
        "scenario=retained-finalization-failure-data-channel\nUTC={}\n",
        std::process::Command::new("/bin/date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    ))?;
    let bind_ip = "127.0.0.1";
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(vec![])?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Obscura final-failure CA");
    let ca = ca_params.self_signed(&ca_key)?;
    let server_key = rcgen::KeyPair::generate()?;
    let server = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.into()])?
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
    write_authorized(root.join("registry.json"), root.join("authorized-clients.json"),
        root.join("profiles"), &client.der().to_vec())?;

    let mut host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .arg("--profile-root").arg(root.join("profiles"))
        .arg("--authorization-registry").arg(root.join("registry.json"))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(raw_root.join("host.stderr"))?)
        .kill_on_drop(true).spawn()?;
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await?;
    let mut local = BufReader::new(local);
    let mut line = String::new();
    local.read_line(&mut line).await?;
    let html = "data:text/html,<style>body{margin:0;background:white}</style><div style='width:160px;height:120px;background:red'></div>";
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await?;
    let session_id = state["session_id"].as_str().expect("Host session identity missing").to_owned();
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"resize","width":160,"height":120}})).await?;
    let _ = local_request(&mut local, serde_json::json!({"id":3,"operation":identity(&state),"command":{"type":"navigate","url":html}})).await?;
    // Materialize the secondary profile and let its adapter fail so the
    // endpoint retains a `CompletionState::Failed` entry for it.
    let bound = local_request(&mut local, serde_json::json!({"id":4,"operation":identity(&state),"command":{"type":"profile_select","profile_id":"p1"}})).await?;
    let secondary_session = bound["status"]["session_id"].as_str().expect("Host secondary session missing").to_owned();
    anyhow::ensure!(session_id != secondary_session, "two profiles resolved to one session");
    drop(local);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reservation = TcpListener::bind((bind_ip, 0)).await?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let media_bin = release_binary("obscura-media");
    let receiver_bin = release_binary("obscura-webrtc-receiver");
    anyhow::ensure!(media_bin.is_file(), "Build release obscura-media with --features webrtc first");
    anyhow::ensure!(receiver_bin.is_file(), "Build release obscura-webrtc-receiver with --features webrtc first");
    // The wrapper is the real endpoint adapter entrypoint. It leaves the
    // default profile to the test-owned encoded.sock producer and runs the
    // failing adapter only for the already-materialized p1 session. The p1
    // adapter acquires a permit, never binds a pgid, and ignores the
    // endpoint's cooperative SIGTERM, so the real finalizer retains
    // `CompletionState::Failed` with the exact unsettled permit.
    let p1_marker = root.join("p1-session");
    std::fs::write(&p1_marker, &secondary_session)?;
    let p1_adapter = root.join("p1-adapter.py");
    // The fixture's own scratch root is torn down by the cleanup owner, so the
    // fault cases redirect the adapter pid record to an external sink. The
    // recorded pid then outlives the root and can be checked after every exit.
    let p1_pid_file = match std::env::var_os("OBSCURA_FIXTURE_PID_SINK") {
        Some(sink) => PathBuf::from(sink),
        None => root.join("p1-adapter.pid"),
    };
    let p1_script = [
        "#!/usr/bin/env python3",
        "import json, os, signal, socket, sys, time",
        "args = sys.argv[1:]",
        "video = args[args.index('--video-socket') + 1]",
        "control = os.path.dirname(video.rstrip('/')) + '/encoder-control.sock'",
        &format!("pid_file = {pid:?}", pid = p1_pid_file.display().to_string()),
        "def connect(path, timeout):",
        "    handle = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)",
        "    deadline = time.time() + timeout",
        "    while True:",
        "        try:",
        "            handle.connect(path)",
        "            return handle",
        "        except OSError:",
        "            if time.time() > deadline:",
        "                raise",
        "            time.sleep(0.05)",
        "with open(pid_file, 'w') as handle:",
        "    handle.write(str(os.getpid()))",
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)",
        "video_sock = connect(video, 20)",
        "ctl = connect(control, 20)",
        "ctl.sendall((json.dumps({'op': 'acquire', 'registration_id': 1}) + '\\n').encode())",
        "buf = b''",
        "while b'\\n' not in buf:",
        "    chunk = ctl.recv(4096)",
        "    if not chunk:",
        "        raise SystemExit(3)",
        "    buf += chunk",
        "response = json.loads(buf.split(b'\\n', 1)[0])",
        "if response.get('op') != 'acquire' or response.get('permit') != 'granted':",
        "    raise SystemExit(4)",
        "while True:",
        "    time.sleep(0.2)",
        "",
    ].join("\n");
    std::fs::write(&p1_adapter, p1_script)?;
    std::fs::set_permissions(&p1_adapter, std::fs::Permissions::from_mode(0o755))?;
    let adapter = root.join("adapter-wrapper.py");
    let marker_literal = format!("{:?}", p1_marker.display().to_string());
    let p1_literal = format!("{:?}", p1_adapter.display().to_string());
    let wrapper = format!(
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nvideo = args[args.index('--video-socket') + 1] if '--video-socket' in args else ''\nmarker = open({marker}).read().strip()\nif marker and marker in video:\n    os.execv(sys.executable, [sys.executable, {p1}] + args)\nos.execv('/usr/bin/true', ['true'] + args)\n",
        marker = marker_literal,
        p1 = p1_literal
    );
    std::fs::write(&adapter, wrapper)?;
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755))?;
    let mut endpoint = Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string())
        .arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der"))
        .arg("--server-key").arg(root.join("server.key"))
        .arg("--client-ca").arg(root.join("ca.der"))
        .arg("--media-bin").arg(&adapter)
        .arg("--authorized-clients").arg(root.join("authorized-clients.json"))
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(raw_root.join("endpoint.stderr"))?)
        .kill_on_drop(true).spawn()?;
    std::fs::write(raw_root.join("endpoint.pid"), endpoint.id().expect("endpoint pid").to_string())?;
    wait_socket(root.join("endpoint")).await;

    // Keep a real default-session source available while the prime attachment
    // selects p1 and closes. The endpoint creates the exact encoded socket only
    // after that attachment is admitted.
    let bytes = obscura_media::encode(std::path::Path::new("ffmpeg"), 160, 120, scripted_scene(160, 120)).await?;
    anyhow::ensure!(!bytes.is_empty(), "finalization-failure producer got no access unit");
    let session_socket = root.join(format!("endpoint/session-{session_id}/encoded.sock"));
    let producer_task = tokio::spawn({
        let session_socket = session_socket.clone();
        let session_id = session_id.clone();
        let bytes = bytes.clone();
        async move {
            let mut sequence = 1u64;
            loop {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                while !session_socket.exists() {
                    if tokio::time::Instant::now() >= deadline {
                        eprintln!("default encoded socket did not reappear for the retained-failure rebind");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let Ok(mut producer) = tokio::net::UnixStream::connect(&session_socket).await else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                loop {
                    let packet = scripted_access_unit(&session_id, sequence, bytes.len() as u64);
                    if send_video_packet(&mut producer, &packet, &bytes).await.is_err() { break; }
                    sequence = sequence.wrapping_add(1);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    });

    // F6 observation device: the fault hook must touch the same record path the
    // fixture cleanup owner will read. `p1_pid_file` itself moves into the
    // fixture, so keep a clone for the hook.
    let p1_pid_file_for_fault = p1_pid_file.clone();
    // From here the p1 adapter is launched and every fallible operation runs
    // inside the fixture's single cleanup owner, so no early return or outer
    // timeout can orphan the exact p1 pid or the endpoint/host teardown.
    let fixture = FinalFailureFixture {
        label: "p1 fixture adapter",
        pid_file: p1_pid_file,
        adapter_termination_owed: true,
        endpoint,
        host,
        producer_task: Some(producer_task),
        root: root.clone(),
    };
    let receiver_bin = receiver_bin.clone();
    let root = root.clone();
    let raw_root = raw_root.clone();
    let bind_ip = bind_ip.to_string();
    fixture.run(async move {
        let prime_ready = root.join("ready-prime.json");
        let mut prime_command = Command::new(&receiver_bin);
        prime_command.arg("--address").arg(address.to_string())
            .arg("--ca").arg(root.join("ca.der"))
            .arg("--client-cert").arg(root.join("client.der"))
            .arg("--client-key").arg(root.join("client.key"))
            .arg("--bind-ip").arg(&bind_ip)
            .arg("--frames").arg("3").arg("--skip-click")
            .arg("--terminal-trigger").arg("prime-finalize-failed")
            .arg("--terminal-transport").arg("data-channel")
            .arg("--ready-file").arg(&prime_ready)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let prime_output = prime_command.output().await?;
        let prime_dir = raw_root.join("prime");
        std::fs::create_dir_all(&prime_dir)?;
        std::fs::write(prime_dir.join("stdout.txt"), &prime_output.stdout)?;
        std::fs::write(prime_dir.join("stderr.txt"), &prime_output.stderr)?;
        std::fs::write(prime_dir.join("exit.txt"), format!("EXIT={:?}\n", prime_output.status.code()))?;
        anyhow::ensure!(prime_output.status.success(),
            "p1 prime attachment failed: {}", String::from_utf8_lossy(&prime_output.stderr));
        // F2 fault injection. The prime attachment has now launched the p1
        // adapter, so these faults fall inside the exact lifetime the fixture
        // cleanup owner must cover. `error` fails the case immediately; `hang`
        // blocks so the outer `tokio::time::timeout` cancels this future.
        //
        // F6 (reused from O4) adds a real I/O-boundary fault:
        // `pid-record-unparseable` replaces the adapter's own pid record with
        // unparseable bytes after the adapter recorded it and before the cleanup
        // owner reads it. `record-pid-only` captures the same evidence without
        // corrupting the record (normal-path control).
        let f6_fault = std::env::var("OBSCURA_FIXTURE_FAULT").unwrap_or_default();
        if f6_fault.contains("pid-record-unparseable") || f6_fault == "record-pid-only" {
            f6_record_pid_evidence(
                f6_case_from_fault(&f6_fault),
                &p1_pid_file_for_fault,
                &root,
                f6_fault.contains("pid-record-unparseable"),
            )
            .await?;
        }
        match f6_fault.as_str() {
            "error" | "error+pid-record-unparseable" => { return Err(anyhow::anyhow!("injected post-launch receiver failure")); }
            "hang" => { tokio::time::sleep(Duration::from_secs(600)).await; }
            _ => {}
        }

        // The prime attachment has closed. Wait for the endpoint's real finalizer
        // to publish the retained failure before the DataChannel rebind.
        let secondary_dir = root.join(format!("endpoint/session-{secondary_session}"));
        wait_socket_deadline(secondary_dir.join("encoded.sock"), Duration::from_secs(30)).await?;
        let endpoint_stderr_path = raw_root.join("endpoint.stderr");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let stderr = std::fs::read_to_string(&endpoint_stderr_path).unwrap_or_default();
            if stderr.contains("SESSION_ENCODER_PERMIT_UNSETTLED") { break; }
            anyhow::ensure!(tokio::time::Instant::now() < deadline,
                "p1 finalizer did not publish a retained failure: {stderr}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let ready_file = root.join("ready-finalfail.json");
        let mut command = Command::new(&receiver_bin);
        command.arg("--address").arg(address.to_string())
            .arg("--ca").arg(root.join("ca.der"))
            .arg("--client-cert").arg(root.join("client.der"))
            .arg("--client-key").arg(root.join("client.key"))
            .arg("--bind-ip").arg(&bind_ip)
            .arg("--frames").arg("3").arg("--skip-click")
            .arg("--terminal-trigger").arg("rebind-finalize-failed")
            .arg("--terminal-transport").arg("data-channel")
            .arg("--ready-file").arg(&ready_file)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let output = command.output().await?;
        let phase_dir = raw_root.join("receiver");
        std::fs::create_dir_all(&phase_dir)?;
        std::fs::write(phase_dir.join("stdout.txt"), &output.stdout)?;
        std::fs::write(phase_dir.join("stderr.txt"), &output.stderr)?;
        std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", output.status.code()))?;
        anyhow::ensure!(output.status.success(),
            "retained-finalization-failure receiver failed: {}", String::from_utf8_lossy(&output.stderr));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        anyhow::ensure!(report["pass"] == true && report["browser_path"] == "webrtc_data_channel",
            "retained-finalization-failure report not passing: {report}");
        let receipt = &report["trigger_receipt"];
        anyhow::ensure!(receipt["type"] == "error" && receipt["id"] == 200,
            "the DataChannel rebind did not return a correlated error receipt: {receipt}");
        anyhow::ensure!(receipt["code"] == "SESSION_MEDIA_FINALIZE_FAILED",
            "the retained finalization failure was not projected to the DataChannel client: {receipt}");
        anyhow::ensure!(receipt["message"].as_str().is_some_and(|message| message.contains("SESSION_ENCODER_PERMIT_UNSETTLED")),
            "the retained finalization failure lost its cause: {receipt}");
        anyhow::ensure!(report["frames_at_trigger"].as_u64().is_some_and(|count| count >= 1),
            "the receiver had no frame before it triggered the rebind: {report}");
        Ok(())
    }).await
}

/// Owns the full post-adapter-launch lifetime of the retained-failure fixture.
/// Its async `cleanup` removes each exact resource this fixture created: the
/// exact p1 adapter pid (SIGTERM then SIGKILL), the endpoint and host direct
/// children, the default-frame producer task, and the scratch root. The `Drop`
/// impl is a synchronous backstop, so the outer `tokio::time::timeout`
/// cancelling the case future still reaches cleanup and cannot orphan the p1
/// fixture.
struct FinalFailureFixture {
    label: &'static str,
    pid_file: PathBuf,
    // Whether this owner still owes the adapter a termination. It is armed when
    // the fixture is created, disarmed the moment exact-pid termination is
    // verified, and left armed when termination fails so the `Drop` backstop can
    // retry with the exact recorded pid.
    adapter_termination_owed: bool,
    endpoint: tokio::process::Child,
    host: tokio::process::Child,
    producer_task: Option<tokio::task::JoinHandle<()>>,
    root: PathBuf,
}

impl FinalFailureFixture {
    /// Always run `work`, then remove every owned resource. The work result and
    /// any cleanup failure are both propagated, so a swallowed cleanup failure
    /// can never hide a live retained adapter.
    async fn run<F, T>(mut self, work: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        let outcome = work.await;
        let cleanup = self.cleanup().await;
        match (outcome, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(cleanup_error)) => Err(cleanup_error.context("fixture cleanup failed after work succeeded")),
            (Err(work_error), Ok(())) => Err(work_error),
            (Err(work_error), Err(cleanup_error)) => Err(work_error.context(format!("fixture cleanup also failed: {cleanup_error:#}"))),
        }
    }

    /// Remove every owned resource and report failure. Adapter termination is
    /// verified before the signal authority is disarmed. On any failure the pid
    /// record and the scratch root are retained and the owner stays armed, so a
    /// later `Drop` retry still has the exact pid to finish the job.
    async fn cleanup(&mut self) -> anyhow::Result<()> {
        let mut failures: Vec<String> = Vec::new();
        if self.adapter_termination_owed {
            match wait_pid(&self.pid_file, Duration::from_secs(30)).await {
                Ok(adapter_pid) => match terminate_exact_pid(self.label, adapter_pid, Duration::from_secs(30)).await {
                    Ok(()) => self.adapter_termination_owed = false,
                    Err(error) => failures.push(format!("exact-pid termination failed: {error:#}")),
                },
                Err(error) => failures.push(format!("adapter pid unavailable: {error:#}")),
            }
        }
        if !failures.is_empty() {
            return Err(anyhow::anyhow!("{} cleanup incomplete: {}", self.label, failures.join("; ")));
        }
        let _ = self.endpoint.kill().await;
        let _ = self.endpoint.wait().await;
        let _ = self.host.kill().await;
        let _ = self.host.wait().await;
        if let Some(task) = self.producer_task.take() {
            task.abort();
            let _ = task.await;
        }
        let root = std::mem::take(&mut self.root);
        if !root.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(root);
        }
        Ok(())
    }
}

impl Drop for FinalFailureFixture {
    fn drop(&mut self) {
        // Cancellation backstop. It fires only while this owner still owes the
        // adapter a termination: on the normal path `cleanup` has verified
        // termination and disarmed the owner, so a reused pid is never signalled
        // just because the external pid file still exists as evidence.
        if let Some(task) = self.producer_task.take() {
            task.abort();
        }
        if self.adapter_termination_owed {
            if let Ok(text) = std::fs::read_to_string(&self.pid_file) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    // Safety: this signal targets the exact pid this fixture wrote.
                    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

/// A unique scratch path for the recorded p1 adapter pid. It lives outside the
/// fixture root, so the recorded pid survives the root teardown and can be
/// checked after the case future has been dropped.
fn fixture_pid_sink() -> PathBuf {
    std::env::temp_dir().join(format!("oc-fixpid-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]))
}

/// Run one retained-failure case with a post-launch fault and an external pid
/// sink. The case timeout is applied here so a hanging case is cancelled while
/// the fixture's `Drop` backstop still removes the recorded adapter pid.
async fn run_fixture_fault(fault: &str, sink: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    std::env::set_var("OBSCURA_FIXTURE_FAULT", fault);
    std::env::set_var("OBSCURA_FIXTURE_PID_SINK", sink);
    let outcome = tokio::time::timeout(timeout, run_retained_finalize_failure_data_channel()).await;
    std::env::remove_var("OBSCURA_FIXTURE_FAULT");
    std::env::remove_var("OBSCURA_FIXTURE_PID_SINK");
    match outcome {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("fixture case timed out after {timeout:?}")),
    }
}

/// Assert the pid the fixture recorded is no longer a live process. The pid
/// file is external, so it is read directly. Zombies count as not live: the
/// adapter may briefly remain an unreaped zombie of the endpoint child.
async fn assert_recorded_adapter_gone(sink: &PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(sink) {
            if let Ok(pid) = text.trim().parse::<i32>() { break pid; }
        }
        assert!(tokio::time::Instant::now() < deadline,
            "F2: the fixture never recorded the launched adapter pid at {}", sink.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let live_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !pid_exited_or_zombie(pid) {
        assert!(tokio::time::Instant::now() < live_deadline,
            "F2: the recorded adapter pid {pid} was still live after fixture cleanup");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(sink);
}


async fn run_receiver_cert(
    binary: &PathBuf,
    address: std::net::SocketAddr,
    root: &PathBuf,
    raw_root: &PathBuf,
    phase: &str,
    client: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut command = Command::new(binary);
    command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
        .arg("--client-cert").arg(root.join(format!("{client}.der")))
        .arg("--client-key").arg(root.join(format!("{client}.key")))
        .arg("--frames").arg("3").arg("--skip-click");
    let output = command.output().await?;
    let phase_dir = raw_root.join(phase);
    std::fs::create_dir_all(&phase_dir)?;
    std::fs::write(phase_dir.join("stdout.txt"), &output.stdout)?;
    std::fs::write(phase_dir.join("stderr.txt"), &output.stderr)?;
    std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", output.status.code()))?;
    anyhow::ensure!(output.status.success(), "receiver {phase} failed at {}: {}", raw_root.display(), String::from_utf8_lossy(&output.stderr));
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn wait_file(path: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "ready marker was not written: {}", path.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn wait_absent(path: &PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "path did not disappear: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

async fn wait_pid(path: &PathBuf, timeout: Duration) -> anyhow::Result<i32> {
    wait_file(path, timeout).await?;
    let pid = std::fs::read_to_string(path)?.trim().parse::<i32>()?;
    anyhow::ensure!(pid > 0, "fixture pid must be positive: {pid}");
    Ok(pid)
}

async fn terminate_exact_pid(label: &str, pid: i32, timeout: Duration) -> anyhow::Result<()> {
    // Safety: this signal targets the exact pid this fixture wrote.
    anyhow::ensure!(unsafe { libc::kill(pid, libc::SIGTERM) } == 0, "SIGTERM {label} pid {pid}");
    let deadline = tokio::time::Instant::now() + timeout;
    while !pid_exited_or_zombie(pid) {
        if tokio::time::Instant::now() >= deadline {
            anyhow::ensure!(unsafe { libc::kill(pid, libc::SIGKILL) } == 0, "SIGKILL {label} pid {pid}");
            let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !pid_exited_or_zombie(pid) {
                anyhow::ensure!(tokio::time::Instant::now() < kill_deadline, "{label} pid {pid} survived SIGKILL");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn pid_exited_or_zombie(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(output) => {
            let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
            stat.is_empty() || stat.starts_with("Z")
        }
        Err(_) => unsafe { libc::kill(pid, 0) != 0 },
    }
}

async fn run_receiver(binary: &PathBuf, address: std::net::SocketAddr, root: &PathBuf, raw_root: &PathBuf, phase: &str, skip_click: bool, bad_binding: bool, expect_encoder_error: bool) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new(binary);
    command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
        .arg("--client-cert").arg(root.join("client.der")).arg("--client-key").arg(root.join("client.key"))
        .arg("--frames").arg("3");
    if skip_click { command.arg("--skip-click"); }
    if bad_binding { command.arg("--bad-binding"); }
    if expect_encoder_error { command.arg("--expect-encoder-error"); }
    let output = command.output().await?;
    let phase_dir = raw_root.join(phase);
    std::fs::create_dir_all(&phase_dir)?;
    std::fs::write(phase_dir.join("stdout.txt"), &output.stdout)?;
    std::fs::write(phase_dir.join("stderr.txt"), &output.stderr)?;
    std::fs::write(phase_dir.join("exit.txt"), format!("EXIT={:?}\n", output.status.code()))?;
    Ok(output)
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
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

fn write_authorized(registry_path: PathBuf, projection_path: PathBuf, profile_root: PathBuf, client_der: &[u8]) -> anyhow::Result<()> {
    let fingerprint = hex(&Sha256::digest(client_der));
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-test",
        "devices": [{
            "device_id": "test-device",
            "device_label": "test client",
            "client_certificate_sha256": fingerprint,
            "scopes": ["observe_human_control", "profile_read", "profile_create", "profile_select"],
            "initial_profile_grant": {
                "selection": "default_profile",
                "authorization_revision": 1
            },
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
            "scopes": ["observe_human_control", "profile_read", "profile_create", "profile_select"],
            "initial_profile_grant": {
                "selection": "default_profile",
                "authorization_revision": 1
            }
        }]
    });
    std::fs::write(&projection_path, serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "test-device"]);
    let catalog = serde_json::json!({
        "catalog_revision": 2,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true,
            "access": {"owner_device_id": "local", "members": members}
        }, {
            "profile_id": "p1",
            "display_name": "Rebind Target",
            "default": false,
            "access": {"owner_device_id": "local", "members": members}
        }],
        "default_profile_id": "default",
        "next_id": 2
    });
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}

/// Registry/projection/catalog for two paired devices, each granted a distinct
/// initial profile so two remote attachments resolve to two exact sessions.
fn write_authorized_two(
    registry_path: PathBuf,
    projection_path: PathBuf,
    profile_root: PathBuf,
    client_a: &[u8],
    client_b: &[u8],
) -> anyhow::Result<()> {
    let scopes = ["observe_human_control", "profile_read", "profile_create", "profile_select"];
    let fingerprint_a = hex(&Sha256::digest(client_a));
    let fingerprint_b = hex(&Sha256::digest(client_b));
    let device = |device_id: &str, label: &str, fingerprint: &str, selection: serde_json::Value| serde_json::json!({
        "device_id": device_id,
        "device_label": label,
        "client_certificate_sha256": fingerprint,
        "scopes": scopes,
        "initial_profile_grant": {"selection": selection, "authorization_revision": 1},
        "scope_revision": 1,
        "status": "active",
        "not_before_unix": 0,
        "not_after_unix": 4102444800i64,
        "created_at_unix": 0,
        "revoked_at_unix": null
    });
    let registry = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "host_id": "host-test",
        "devices": [
            device("device-a", "profile a", &fingerprint_a, serde_json::json!("default_profile")),
            device("device-b", "profile b", &fingerprint_b, serde_json::json!({"profile_id": "p1"})),
        ]
    });
    std::fs::write(&registry_path, serde_json::to_vec(&registry)?)?;
    let client = |device_id: &str, fingerprint: &str, selection: serde_json::Value| serde_json::json!({
        "device_id": device_id,
        "client_certificate_sha256": fingerprint,
        "scopes": scopes,
        "initial_profile_grant": {"selection": selection, "authorization_revision": 1}
    });
    let projection = serde_json::json!({
        "schema_version": 2,
        "authorization_revision": 1,
        "registry_sha256": "00".repeat(32),
        "clients": [
            client("device-a", &fingerprint_a, serde_json::json!("default_profile")),
            client("device-b", &fingerprint_b, serde_json::json!({"profile_id": "p1"})),
        ]
    });
    std::fs::write(&projection_path, serde_json::to_vec(&projection)?)?;
    let members = serde_json::json!(["local", "device-a", "device-b"]);
    let catalog = serde_json::json!({
        "catalog_revision": 2,
        "profiles": [{
            "profile_id": "default",
            "display_name": "Default",
            "default": true,
            "access": {"owner_device_id": "local", "members": members}
        }, {
            "profile_id": "p1",
            "display_name": "Secondary",
            "default": false,
            "access": {"owner_device_id": "local", "members": members}
        }],
        "default_profile_id": "default",
        "next_id": 2
    });
    std::fs::create_dir_all(&profile_root)?;
    std::fs::write(profile_root.join("catalog.json"), serde_json::to_string_pretty(&catalog)?)?;
    Ok(())
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
}

/// Root for per-case raw WebRTC evidence. `OBSCURA_WEBRTC_RAW_DIR` redirects
/// generated fixtures to an external record directory; the default keeps the
/// historical public evidence path for the original harness.
fn raw_webrtc_root(prefix: &str) -> PathBuf {
    std::env::var_os("OBSCURA_WEBRTC_RAW_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/runs/m1-frame-forwarding-impl-r1-20261005/raw/webrtc")
        })
        .join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
}

async fn local_request(socket: &mut BufReader<tokio::net::UnixStream>, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    socket.get_mut().write_all(format!("{request}\n").as_bytes()).await?;
    let mut line = String::new(); socket.read_line(&mut line).await?;
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    anyhow::ensure!(reply["type"] == "result", "Host request failed: {reply}");
    Ok(reply["value"].clone())
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize: {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn wait_socket_deadline(path: PathBuf, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !path.exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "listener did not initialize: {} within {timeout:?}", path.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

#[allow(dead_code)]
fn _tls_client_config(ca: &[u8], cert: &[u8], key: &[u8]) -> anyhow::Result<tokio_rustls::rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.to_vec()))?;
    Ok(rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(vec![CertificateDer::from(cert.to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())))?)
}

// ===========================================================================
// F5 black-box external signal device, and the F6 real-fault regression device.
//
// F5: `f5-blackbox.py` injects `kill-observer.dylib` into THIS test executable
// only. The device snapshots the observer log path, clears the inherited
// injection variables from its own environment before any child process is
// spawned, runs the real retained-failure fixture entry, and then -- after the
// entry returned and before it removes the external pid sink -- reads the raw
// signal log and filters it by this process as caller and by the exact positive
// pid the fixture itself recorded. The injected observer is the only source of
// the "no extra signal to the old pid" fact; no in-process counter is kept.
//
// F6: the same real fixture entry plus one real post-prime I/O-boundary fault
// reused from O4: the adapter's own pid record is replaced with unparseable
// bytes after the adapter recorded it and before the cleanup owner reads it.
// ===========================================================================

/// Snapshot and clear the F5 injection environment, and return the observer log
/// path. It must run before the case spawns any child process, so the injected
/// observer can never reach the Host, the endpoint, media or the receiver.
fn f5_take_injection_scope() -> Option<PathBuf> {
    let observer_log = std::env::var_os("KILL_OBSERVER_LOG").map(PathBuf::from);
    let inherited_dylib = std::env::var_os("DYLD_INSERT_LIBRARIES");
    for name in ["KILL_OBSERVER_LOG", "DYLD_INSERT_LIBRARIES", "DYLD_FORCE_FLAT_NAMESPACE"] {
        std::env::remove_var(name);
    }
    println!(
        "F5-SCOPE test_pid={} observer_log={:?} inherited_dyld_insert={:?} after_clear_log={:?} after_clear_dyld={:?}",
        std::process::id(),
        observer_log,
        inherited_dylib,
        std::env::var_os("KILL_OBSERVER_LOG"),
        std::env::var_os("DYLD_INSERT_LIBRARIES"),
    );
    observer_log
}

/// One raw line written by kill-observer.c.
#[derive(Debug, Clone)]
struct F5Signal {
    seq: i64,
    caller_pid: i32,
    target_pid: i32,
    signal: i32,
    signal_name: String,
    ret: i32,
    errno: i32,
}

impl F5Signal {
    /// The full recorded tuple, so every field the observer writes is also
    /// reported and asserted on: seq, caller, target, signal, ret, errno.
    fn f5_line(&self) -> String {
        format!(
            "seq={} caller_pid={} target_pid={} signal={}({}) ret={} errno={}",
            self.seq, self.caller_pid, self.target_pid, self.signal, self.signal_name, self.ret, self.errno,
        )
    }
}

/// Read the raw observer log exactly as written; no filtering happens here.
fn f5_read_signals(path: &PathBuf) -> anyhow::Result<Vec<F5Signal>> {
    let text = std::fs::read_to_string(path)?;
    let mut signals = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let value: serde_json::Value = serde_json::from_str(line)?;
        signals.push(F5Signal {
            seq: value["seq"].as_i64().unwrap_or(-1),
            caller_pid: value["caller_pid"].as_i64().unwrap_or(-1) as i32,
            target_pid: value["target_pid"].as_i64().unwrap_or(-1) as i32,
            signal: value["signal"].as_i64().unwrap_or(-1) as i32,
            signal_name: value["signal_name"].as_str().unwrap_or("<missing>").to_owned(),
            ret: value["ret"].as_i64().unwrap_or(i64::MIN) as i32,
            errno: value["errno"].as_i64().unwrap_or(-1) as i32,
        });
    }
    Ok(signals)
}

/// Filter by the exact caller (this test process) and the exact positive target
/// pid the fixture recorded. A signal to any other pid is not evidence here.
fn f5_signals_to(signals: &[F5Signal], caller_pid: i32, target_pid: i32) -> Vec<F5Signal> {
    signals.iter()
        .filter(|event| event.caller_pid == caller_pid && event.target_pid == target_pid)
        .cloned()
        .collect()
}

/// The positive pid the fixture wrote into the external sink.
fn f5_read_recorded_pid(sink: &PathBuf) -> Option<i32> {
    std::fs::read_to_string(sink).ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
}

/// F5 normal path. The real retained-failure fixture entry runs with an external
/// pid sink, so the recorded adapter pid survives the scratch-root teardown. The
/// injected observer must show the fixture's own real termination call to that
/// exact pid (observer activity positive control), and after the entry returned
/// it must show no extra SIGKILL to the old pid.
///
/// Ignored by default: this case is only valid when `f5-blackbox.py` injects the
/// external observer into this exact test process. Without that injection
/// `KILL_OBSERVER_LOG` is unset, no outbound signal can be observed, and the
/// device fails as invalid. Run it through the runner, which passes `--ignored`.
#[ignore = "needs f5-blackbox.py to inject the external observer; without injection this device is invalid"]
#[tokio::test]
async fn r10_f5_cleanup_disarms_external_pid_sink() {
    let observer_log = f5_take_injection_scope().expect(
        "F5 DEVICE INVALID: no observer was injected (KILL_OBSERVER_LOG is unset); run this case through f5-blackbox.py",
    );
    let sink = fixture_pid_sink();
    std::env::set_var("OBSCURA_FIXTURE_PID_SINK", &sink);
    let outcome = run_retained_finalize_failure_data_channel().await;
    std::env::remove_var("OBSCURA_FIXTURE_PID_SINK");
    let outcome_text = match &outcome {
        Ok(()) => "Ok(())".to_owned(),
        Err(error) => format!("{error:#}"),
    };
    println!("F5-NORMAL case_outcome={outcome_text}");

    // Read the signal log after the fixture entry returned and before the
    // external sink is removed.
    assert!(sink.exists(), "F5 DEVICE INVALID: the external pid sink was gone before the signal log was read: {}", sink.display());
    let recorded_pid = f5_read_recorded_pid(&sink).expect(
        "F5 DEVICE INVALID: the fixture never recorded a positive adapter pid in the external sink",
    );
    let caller_pid = std::process::id() as i32;
    let signals = f5_read_signals(&observer_log).expect("F5 DEVICE INVALID: observer log unreadable");
    let to_target = f5_signals_to(&signals, caller_pid, recorded_pid);
    println!("F5-NORMAL recorded_pid={recorded_pid} caller_pid={caller_pid} filtered_events={}", to_target.len());
    for event in &to_target {
        println!("F5-NORMAL observed {}", event.f5_line());
    }

    // Observer activity positive control: a real termination call, issued by this
    // process, to the exact pid the fixture itself recorded.
    let real_terminations = to_target.iter()
        .filter(|event| (event.signal == libc::SIGTERM || event.signal == libc::SIGKILL) && event.ret == 0)
        .count();
    assert!(
        real_terminations >= 1,
        "F5 DEVICE INVALID: the observer recorded no real termination call from caller {caller_pid} to the exact recorded pid {recorded_pid}: {to_target:?}",
    );

    // The dangerous call: after cleanup verified adapter termination and disarmed
    // the owner, no further signal may target the old pid.
    let sigkills: Vec<F5Signal> = to_target.iter().filter(|event| event.signal == libc::SIGKILL).cloned().collect();
    assert_eq!(
        sigkills.len(), 1,
        "F5: cleanup verified adapter termination and disarmed the owner, so only cleanup's own SIGKILL may target recorded pid {recorded_pid}; the observer recorded {} SIGKILL(s) from caller {caller_pid}: {}",
        sigkills.len(),
        sigkills.iter().map(F5Signal::f5_line).collect::<Vec<_>>().join(" | "),
    );
    assert_eq!(
        sigkills[0].ret, 0,
        "F5: the single SIGKILL to recorded pid {recorded_pid} must be cleanup's own real termination, not a stale-pid drop kill: {:?}",
        sigkills[0],
    );

    // No further signal to the old pid after the fixture entry returned.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = f5_read_signals(&observer_log).expect("F5 DEVICE INVALID: observer log unreadable");
    let after_target = f5_signals_to(&after, caller_pid, recorded_pid);
    assert_eq!(
        after_target.len(), to_target.len(),
        "F5: a signal to the old pid {recorded_pid} appeared after the fixture entry returned: {after_target:?}",
    );

    let adapter_gone = pid_exited_or_zombie(recorded_pid);
    let _ = std::fs::remove_file(&sink);
    assert!(adapter_gone, "F5: the adapter pid {recorded_pid} must not be alive after verified termination");
    assert!(outcome.is_ok(), "F5: the normal retained-failure path must complete: {outcome_text}");
}

/// F5 cancel path. The post-launch `hang` fault makes the outer case timeout drop
/// the fixture future, so cleanup never runs and the `Drop` backstop must still
/// deliver a real SIGKILL to the exact recorded adapter pid. The observer proves
/// the call happened; the process state independently proves the owned adapter
/// is gone.
///
/// Ignored by default: this case is only valid when `f5-blackbox.py` injects the
/// external observer into this exact test process. Without that injection
/// `KILL_OBSERVER_LOG` is unset, no outbound signal can be observed, and the
/// device fails as invalid. Run it through the runner, which passes `--ignored`.
#[ignore = "needs f5-blackbox.py to inject the external observer; without injection this device is invalid"]
#[tokio::test]
async fn r10_f5_cancel_keeps_backstop_effective() {
    let observer_log = f5_take_injection_scope().expect(
        "F5 DEVICE INVALID: no observer was injected (KILL_OBSERVER_LOG is unset); run this case through f5-blackbox.py",
    );
    let sink = fixture_pid_sink();
    let outcome = run_fixture_fault("hang", &sink, Duration::from_secs(90)).await;
    let outcome_text = match &outcome {
        Ok(()) => "Ok(())".to_owned(),
        Err(error) => format!("{error:#}"),
    };
    println!("F5-CANCEL case_outcome={outcome_text}");
    assert!(outcome.is_err(),
        "F5 DEVICE INVALID: the hanging case was not cancelled by the case timeout, so the backstop path was never exercised: {outcome_text}");

    assert!(sink.exists(), "F5 DEVICE INVALID: the external pid sink was gone before the signal log was read: {}", sink.display());
    let recorded_pid = f5_read_recorded_pid(&sink).expect(
        "F5 DEVICE INVALID: the fixture never recorded a positive adapter pid in the external sink",
    );
    let caller_pid = std::process::id() as i32;
    let signals = f5_read_signals(&observer_log).expect("F5 DEVICE INVALID: observer log unreadable");
    let to_target = f5_signals_to(&signals, caller_pid, recorded_pid);
    println!("F5-CANCEL recorded_pid={recorded_pid} caller_pid={caller_pid} filtered_events={}", to_target.len());
    for event in &to_target {
        println!("F5-CANCEL observed {}", event.f5_line());
    }

    // Observer activity positive control: the backstop's real SIGKILL to the exact
    // recorded pid, delivered while the adapter was still alive (ret == 0).
    let backstop_kills = to_target.iter()
        .filter(|event| event.signal == libc::SIGKILL && event.ret == 0)
        .count();
    assert!(
        backstop_kills >= 1,
        "F5 DEVICE INVALID: the observer recorded no real backstop SIGKILL from caller {caller_pid} to the exact recorded pid {recorded_pid}: {to_target:?}",
    );

    // The call record is not by itself a delivery proof: process state must
    // independently show the owned adapter is no longer alive.
    let adapter_gone = pid_exited_or_zombie(recorded_pid);
    let _ = std::fs::remove_file(&sink);
    assert!(adapter_gone,
        "F5: the owned adapter pid {recorded_pid} must not be alive after the cancellation backstop");
}

// ---------------------------------------------------------------------------
// F6 real-fault regression device (reused from O4).
// ---------------------------------------------------------------------------

/// Where the F6 device writes its raw evidence. The runner sets it per run.
fn f6_evidence_dir() -> PathBuf {
    std::env::var_os("OBSCURA_F6_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("r10-f6-evidence"))
}

/// The evidence case name for one injected fault value.
fn f6_case_from_fault(fault: &str) -> &'static str {
    if fault.contains("pid-record-unparseable") && fault.contains("error") {
        "work-and-cleanup-errors"
    } else if fault.contains("pid-record-unparseable") {
        "cleanup-error-after-success"
    } else {
        "normal-cleanup"
    }
}

fn f6_case_dir(case: &str) -> PathBuf {
    f6_evidence_dir().join(case)
}

fn f6_write_evidence(case: &str, name: &str, text: &str) -> anyhow::Result<()> {
    let dir = f6_case_dir(case);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(name), text)?;
    Ok(())
}

fn f6_read_injection(case: &str) -> serde_json::Value {
    std::fs::read_to_string(f6_case_dir(case).join("fault-injection.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Raw external process observation for one exact pid.
fn f6_ps_stat(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=,etime=,command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Terminate exactly one pid this device captured. Never a computed or matched
/// pid: the caller passes the pid read from the adapter's own record.
fn f6_release_exact_pid(label: &str, pid: i32) -> bool {
    if pid <= 0 || pid_exited_or_zombie(pid) {
        return true;
    }
    // Safety: this signal targets the exact pid the device read from the record.
    unsafe { libc::kill(pid, libc::SIGKILL); }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !pid_exited_or_zombie(pid) {
        if std::time::Instant::now() >= deadline {
            eprintln!("F6: {label} pid {pid} survived SIGKILL");
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Read the real pid record the adapter wrote, capture it as evidence, and (when
/// `corrupt`) replace it with unparseable bytes at the real I/O boundary. This
/// runs inside the case after the prime attachment launched the real adapter and
/// before the fixture cleanup owner reads the same file.
async fn f6_record_pid_evidence(case: &str, pid_file: &PathBuf, root: &PathBuf, corrupt: bool) -> anyhow::Result<()> {
    let pid = wait_pid(pid_file, Duration::from_secs(60))
        .await
        .map_err(|error| error.context(format!(
            "F6 DEVICE INVALID: the real adapter pid record never became readable at {}",
            pid_file.display()
        )))?;
    let before = std::fs::read(pid_file)?;
    let live_before = !pid_exited_or_zombie(pid);
    anyhow::ensure!(live_before, "F6 DEVICE INVALID: recorded adapter pid {pid} is not a live process");
    if corrupt {
        std::fs::write(pid_file, b"not-a-pid\n")?;
    }
    let after = std::fs::read(pid_file)?;
    let evidence = serde_json::json!({
        "stage": "after-prime-attachment-before-fixture-cleanup",
        "pid_file": pid_file.display().to_string(),
        "pid_file_before_bytes": String::from_utf8_lossy(&before).to_string(),
        "pid_file_after_bytes": String::from_utf8_lossy(&after).to_string(),
        "pid_recorded": pid,
        "adapter_live_at_injection": live_before,
        "adapter_ps_at_injection": f6_ps_stat(pid),
        "corrupted": corrupt,
        "scratch_root": root.display().to_string(),
        "scratch_root_exists_at_injection": root.exists(),
        "record_lives_outside_scratch_root": !pid_file.starts_with(root),
    });
    f6_write_evidence(case, "fault-injection.json", &serde_json::to_string_pretty(&evidence)?)?;
    println!("F6-INJECT case={case} stage=post-prime pid={pid} live={live_before} corrupt={corrupt} record={} root={}",
        pid_file.display(), root.display());
    Ok(())
}

/// Shared tail for the two F6 fault cases: capture the observable state after the
/// case returned, write the raw record, reclaim only the exact captured pid and
/// the records it retained, and assert the failure surface.
async fn f6_finish_fault_case(
    case: &str,
    outcome: &Result<Result<(), anyhow::Error>, tokio::time::error::Elapsed>,
    record_path: &PathBuf,
    expected_work_error: Option<&str>,
) -> anyhow::Result<()> {
    let injection = f6_read_injection(case);
    let pid = injection["pid_recorded"].as_i64().unwrap_or(-1) as i32;
    let root = PathBuf::from(injection["scratch_root"].as_str().unwrap_or_default());
    let case_outcome = match outcome {
        Err(_) => "OUTER_OBSERVATION_TIMEOUT".to_string(),
        Ok(Ok(())) => "Ok(()): the case reported success".to_string(),
        Ok(Err(error)) => format!("{error:#}"),
    };
    let work_reported_success = matches!(outcome, Ok(Ok(())));
    let record_exists = record_path.exists();
    let record_text = std::fs::read_to_string(record_path).ok();
    let root_exists = !root.as_os_str().is_empty() && root.exists();
    let adapter_live = pid > 0 && !pid_exited_or_zombie(pid);
    let adapter_stat = f6_ps_stat(pid);
    let evidence = serde_json::json!({
        "case": case,
        "injection": injection,
        "case_outcome": case_outcome,
        "work_reported_success": work_reported_success,
        "pid_record_path": record_path.display().to_string(),
        "pid_record_exists_after_case": record_exists,
        "pid_record_text_after_case": record_text,
        "scratch_root_path": root.display().to_string(),
        "scratch_root_exists_after_case": root_exists,
        "adapter_pid": pid,
        "adapter_live_after_case": adapter_live,
        "adapter_ps_after_case": adapter_stat,
    });
    f6_write_evidence(case, "observation.json", &serde_json::to_string_pretty(&evidence)?)?;
    println!("F6-OBSERVED case={case}");
    println!("F6-OBSERVED case_outcome={case_outcome}");
    println!("F6-OBSERVED pid_record_exists={record_exists} text={record_text:?}");
    println!("F6-OBSERVED scratch_root_exists={root_exists} path={}", root.display());
    println!("F6-OBSERVED adapter_pid={pid} live={adapter_live} ps={adapter_stat:?}");

    // Reclamation: only the exact pid this device captured, then the records it
    // retained. The retained state is captured above first.
    let released = f6_release_exact_pid("f6 fault adapter", pid);
    if record_exists {
        let _ = std::fs::remove_file(record_path);
    }
    if root_exists {
        let _ = std::fs::remove_dir_all(&root);
    }
    println!("F6-RECLAIMED observer_released_exact_pid={released} pid={pid}");

    assert!(!work_reported_success,
        "F6: the cleanup failure must not be swallowed into case success: {case_outcome}");
    if let Some(expected) = expected_work_error {
        assert!(case_outcome.contains(expected),
            "F6: the work error must stay in the propagated chain (expected {expected:?}): {case_outcome}");
    }
    assert!(case_outcome.contains("cleanup incomplete"),
        "F6: the cleanup failure must be explicit: {case_outcome}");
    assert!(case_outcome.contains("adapter pid unavailable"),
        "F6: the unverified termination must name the unreadable pid record: {case_outcome}");
    assert!(record_exists, "F6: the pid record must be retained when termination is not verified");
    assert!(root_exists, "F6: the scratch root must be retained when termination is not verified");
    assert!(adapter_live, "F6: the real adapter child must still be observable after unverified cleanup");
    assert!(released, "F6: the device must release the exact captured pid");
    Ok(())
}

/// F6 case A: work succeeds, cleanup cannot parse the pid record. The real
/// retained-failure entry runs unchanged; the only fault is the real
/// I/O-boundary replacement of the adapter's own pid record with unparseable
/// bytes.
#[tokio::test]
async fn r10_f6_cleanup_error_after_success_retains_records() {
    let sink = fixture_pid_sink();
    std::env::set_var("OBSCURA_FIXTURE_PID_SINK", &sink);
    std::env::set_var("OBSCURA_FIXTURE_FAULT", "pid-record-unparseable");
    let outcome = tokio::time::timeout(Duration::from_secs(300), run_retained_finalize_failure_data_channel()).await;
    std::env::remove_var("OBSCURA_FIXTURE_FAULT");
    std::env::remove_var("OBSCURA_FIXTURE_PID_SINK");
    f6_finish_fault_case("cleanup-error-after-success", &outcome, &sink, None).await.unwrap();
}

/// F6 case B: the same real fault plus the pre-existing post-launch work
/// failure, so the work error and the cleanup error must both appear in one
/// propagated chain.
#[tokio::test]
async fn r10_f6_work_and_cleanup_errors_both_surface() {
    let sink = fixture_pid_sink();
    std::env::set_var("OBSCURA_FIXTURE_PID_SINK", &sink);
    std::env::set_var("OBSCURA_FIXTURE_FAULT", "error+pid-record-unparseable");
    let outcome = tokio::time::timeout(Duration::from_secs(300), run_retained_finalize_failure_data_channel()).await;
    std::env::remove_var("OBSCURA_FIXTURE_FAULT");
    std::env::remove_var("OBSCURA_FIXTURE_PID_SINK");
    f6_finish_fault_case(
        "work-and-cleanup-errors",
        &outcome,
        &sink,
        Some("injected post-launch receiver failure"),
    ).await.unwrap();
}

/// F6 control: the same device, the same real entry, the same observation
/// points, and no fault. The adapter is terminated and verified, the owner
/// disarms, the scratch root (which holds the pid record on this path) is
/// removed, and the adapter is gone.
#[tokio::test]
async fn r10_f6_normal_cleanup_retires_records() {
    std::env::set_var("OBSCURA_FIXTURE_FAULT", "record-pid-only");
    let outcome = tokio::time::timeout(Duration::from_secs(300), run_retained_finalize_failure_data_channel()).await;
    std::env::remove_var("OBSCURA_FIXTURE_FAULT");
    let case = "normal-cleanup";
    let injection = f6_read_injection(case);
    let pid = injection["pid_recorded"].as_i64().unwrap_or(-1) as i32;
    let pid_file = PathBuf::from(injection["pid_file"].as_str().unwrap_or_default());
    let root = PathBuf::from(injection["scratch_root"].as_str().unwrap_or_default());
    let case_outcome = match &outcome {
        Err(_) => "OUTER_OBSERVATION_TIMEOUT".to_string(),
        Ok(Ok(())) => "Ok(())".to_string(),
        Ok(Err(error)) => format!("{error:#}"),
    };
    let case_ok = matches!(&outcome, Ok(Ok(())));
    let record_exists = pid_file.exists();
    let root_exists = root.exists();
    let adapter_live = pid > 0 && !pid_exited_or_zombie(pid);
    let adapter_stat = f6_ps_stat(pid);
    let evidence = serde_json::json!({
        "case": case,
        "injection": injection,
        "case_outcome": case_outcome,
        "pid_record_path": pid_file.display().to_string(),
        "pid_record_exists_after_case": record_exists,
        "scratch_root_path": root.display().to_string(),
        "scratch_root_exists_after_case": root_exists,
        "adapter_pid": pid,
        "adapter_live_after_case": adapter_live,
        "adapter_ps_after_case": adapter_stat,
    });
    f6_write_evidence(case, "observation.json", &serde_json::to_string_pretty(&evidence).unwrap()).unwrap();
    println!("F6-CONTROL case_outcome={case_outcome}");
    println!("F6-CONTROL pid_record_path={} exists={record_exists}", pid_file.display());
    println!("F6-CONTROL scratch_root={} exists={root_exists}", root.display());
    println!("F6-CONTROL adapter_pid={pid} live={adapter_live} ps={adapter_stat:?}");
    let released = f6_release_exact_pid("f6 control adapter", pid);
    println!("F6-CONTROL observer_released_exact_pid={released} pid={pid}");

    assert!(case_ok, "F6 control: the normal path must complete: {case_outcome}");
    assert!(!record_exists, "F6 control: the pid record must be retired after verified termination");
    assert!(!root_exists, "F6 control: the scratch root must be removed after verified termination");
    assert!(!adapter_live, "F6 control: the adapter must be gone after verified termination");
}
