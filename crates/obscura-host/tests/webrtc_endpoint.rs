#![cfg(unix)]

use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, process::Command, net::TcpListener};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}};

#[tokio::test]
async fn real_receiver_gets_continuous_host_media_and_rejects_stale_binding() {
    tokio::time::timeout(Duration::from_secs(180), exercise(false)).await.expect("WebRTC endpoint integration timed out").unwrap();
}

#[tokio::test]
async fn real_receiver_gets_typed_encoder_unavailable_error() {
    tokio::time::timeout(Duration::from_secs(180), exercise(true)).await.expect("WebRTC encoder failure integration timed out").unwrap();
}

async fn exercise(encoder_failure: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("ow-{}", uuid::Uuid::new_v4().simple()));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
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

    let host = Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host"))
        .stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
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
        .arg("--enable-webrtc").arg("--webrtc-bind-ip").arg(bind_ip);
    if encoder_failure { endpoint_command.arg("--ffmpeg").arg("/nonexistent-obscura-encoder"); }
    let endpoint = endpoint_command.stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
    wait_socket(root.join("endpoint/encoded.sock")).await;

    let first = run_receiver(&receiver_bin, address, &root, false, false, encoder_failure).await?;
    if encoder_failure {
        anyhow::ensure!(first.status.success(), "typed encoder failure receiver failed: {}", String::from_utf8_lossy(&first.stderr));
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
    anyhow::ensure!(first.status.success(), "independent WebRTC receiver failed: {}", String::from_utf8_lossy(&first.stderr));
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

    let stale = run_receiver(&receiver_bin, address, &root, false, true, false).await?;
    anyhow::ensure!(stale.status.success(), "stale-binding receiver failed: {}", String::from_utf8_lossy(&stale.stderr));
    let stale_report: serde_json::Value = serde_json::from_slice(&stale.stdout)?;
    anyhow::ensure!(stale_report["negative"] == "stale_binding_rejected", "stale binding was not rejected: {stale_report}");

    let second = run_receiver(&receiver_bin, address, &root, true, false, false).await?;
    anyhow::ensure!(second.status.success(), "reconnected WebRTC receiver failed: {}", String::from_utf8_lossy(&second.stderr));
    let second_report: serde_json::Value = serde_json::from_slice(&second.stdout)?;
    anyhow::ensure!(second_report["pass"] == true, "reconnect evidence missing: {second_report}");
    anyhow::ensure!(second_report["session_id"] == first_report["session_id"], "reconnect changed Host Session identity");
    anyhow::ensure!(second_report["attachment_id"] != first_report["attachment_id"], "reconnect reused Host attachment identity");

    let mut endpoint = endpoint;
    let mut host = host;
    let _ = endpoint.kill().await;
    let _ = endpoint.wait().await;
    let _ = host.kill().await;
    let _ = host.wait().await;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

async fn run_receiver(binary: &PathBuf, address: std::net::SocketAddr, root: &PathBuf, skip_click: bool, bad_binding: bool, expect_encoder_error: bool) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new(binary);
    command.arg("--address").arg(address.to_string()).arg("--ca").arg(root.join("ca.der"))
        .arg("--client-cert").arg(root.join("client.der")).arg("--client-key").arg(root.join("client.key"))
        .arg("--frames").arg("3");
    if skip_click { command.arg("--skip-click"); }
    if bad_binding { command.arg("--bad-binding"); }
    if expect_encoder_error { command.arg("--expect-encoder-error"); }
    Ok(command.output().await?)
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
}

fn release_binary(name: &str) -> PathBuf {
    std::env::var_os("OBSCURA_TEST_BINARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release"))
        .join(name)
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

#[allow(dead_code)]
fn _tls_client_config(ca: &[u8], cert: &[u8], key: &[u8]) -> anyhow::Result<tokio_rustls::rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.to_vec()))?;
    Ok(rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(vec![CertificateDer::from(cert.to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())))?)
}
