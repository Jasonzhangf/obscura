#![cfg(unix)]
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use futures_util::{SinkExt, StreamExt};
use tokio_rustls::rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName}};
use tokio_tungstenite::{client_async, tungstenite::{client::IntoClientRequest, Message}};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn mutual_tls_endpoint_preserves_host_and_fences_remote_agent_access() {
    tokio::time::timeout(Duration::from_secs(20), exercise()).await.unwrap();
}

async fn exercise() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = PathBuf::from("/tmp").join(format!("oe-{}", uuid::Uuid::new_v4().simple()));
    std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let bind_ip = std::env::var("OBSCURA_ENDPOINT_BIND_IP").unwrap_or_else(|_| "127.0.0.1".into());
    let bind_ip: std::net::IpAddr = bind_ip.parse().unwrap();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(vec![]).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "Obscura endpoint test CA");
    let ca = ca.self_signed(&ca_key).unwrap();
    let host_key = rcgen::KeyPair::generate().unwrap();
    let host_cert = rcgen::CertificateParams::new(vec!["localhost".into(), bind_ip.to_string()]).unwrap().signed_by(&host_key, &ca, &ca_key).unwrap();
    let client_key = rcgen::KeyPair::generate().unwrap();
    let mut client_params = rcgen::CertificateParams::new(vec![]).unwrap();
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();
    std::fs::write(root.join("server.der"), host_cert.der()).unwrap();
    std::fs::write(root.join("key.der"), host_key.serialize_der()).unwrap();
    std::fs::set_permissions(root.join("key.der"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(root.join("ca.der"), ca.der()).unwrap();
    let mut host = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-host"))
        .arg("--socket-dir").arg(root.join("host")).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    wait_socket(root.join("host/host.sock")).await;
    let local = tokio::net::UnixStream::connect(root.join("host/host.sock")).await.unwrap();
    let mut local = BufReader::new(local);
    let mut line = String::new(); local.read_line(&mut line).await.unwrap();
    let mut state = local_request(&mut local, serde_json::json!({"id":1,"command":{"type":"attach","mode":"agent"}})).await;
    state = local_request(&mut local, serde_json::json!({"id":2,"operation":identity(&state),"command":{"type":"navigate","url":"data:text/html,<button id='target' style='width:200px;height:100px' onclick='window.clicked=1'>remote click</button>"}})).await;
    assert_eq!(state["document_revision"], 1);
    // Reserve a loopback port only for this test's explicit listener.
    let reservation = std::net::TcpListener::bind((bind_ip, 0)).unwrap();
    let address = reservation.local_addr().unwrap(); drop(reservation);
    let media = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/release/obscura-media");
    assert!(media.is_file(), "Build obscura-media before endpoint acceptance");
    let mut endpoint = tokio::process::Command::new(env!("CARGO_BIN_EXE_obscura-endpoint"))
        .arg("--listen").arg(address.to_string()).arg("--host-dir").arg(root.join("host"))
        .arg("--socket-dir").arg(root.join("endpoint"))
        .arg("--server-cert").arg(root.join("server.der")).arg("--server-key").arg(root.join("key.der"))
        .arg("--client-ca").arg(root.join("ca.der")).arg("--media-bin").arg(media)
        .stdout(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true).spawn().unwrap();
    wait_socket(root.join("endpoint/encoded.sock")).await;
    let mut roots = rustls::RootCertStore::empty(); roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
    let anonymous = rustls::ClientConfig::builder().with_root_certificates(roots.clone()).with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(anonymous)).connect(ServerName::try_from("localhost").unwrap(), tcp).await;
    if let Ok(tls) = tls { assert!(client_async(format!("wss://localhost:{}/control", address.port()), tls).await.is_err()); }
    let client = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(vec![client_cert.der().clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key.serialize_der()))).unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let (mut control, response) = client_async(format!("wss://localhost:{}/control", address.port()), tls).await.unwrap();
    let token = response.headers()["x-obscura-media-token"].to_str().unwrap().to_string();
    let ready: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(ready["version"], 3);
    if let Ok(serial) = std::env::var("OBSCURA_ENDPOINT_ADB_SERIAL") {
        assert!(!bind_ip.is_loopback(), "Device probe requires an explicit reachable bind IP");
        // Ephemeral test-only pairing, never installed into device/system trust.
        std::fs::write(root.join("ca.pem"), ca.pem()).unwrap();
        std::fs::write(root.join("client.pem"), client_cert.pem()).unwrap();
        std::fs::write(root.join("client.key"), client_key.serialize_pem()).unwrap();
        std::fs::set_permissions(root.join("client.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
        device_ready(&serial, &root, address, ready["session_id"].as_str().unwrap()).await;
    }
    control.send(Message::text(r#"{"id":1,"command":{"type":"attach","mode":"agent"}}"#)).await.unwrap();
    let rejected: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(rejected["code"], "REMOTE_COMMAND_FORBIDDEN");
    control.send(Message::text(r#"{"id":2,"command":{"type":"attach","mode":"observe"}}"#)).await.unwrap();
    let attached: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(attached["value"]["mode"], "observe");
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut invalid = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    invalid.headers_mut().insert("authorization", "Bearer invalid".parse().unwrap());
    assert!(client_async(invalid, tls).await.is_err());
    let other_key = rcgen::KeyPair::generate().unwrap();
    let mut other_params = rcgen::CertificateParams::new(vec![]).unwrap();
    other_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let other_cert = other_params.signed_by(&other_key, &ca, &ca_key).unwrap();
    let mut roots = rustls::RootCertStore::empty(); roots.add(ca.der().clone()).unwrap();
    let other = rustls::ClientConfig::builder().with_root_certificates(roots)
        .with_client_auth_cert(vec![other_cert.der().clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(other_key.serialize_der()))).unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(other)).connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut crossed = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    crossed.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    assert!(client_async(crossed, tls).await.is_err(), "media token must bind the paired certificate");
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tokio::net::TcpStream::connect(address).await.unwrap()).await.unwrap();
    let mut request = format!("wss://localhost:{}/media", address.port()).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut video, _) = client_async(request, tls).await.unwrap();
    let packet = loop {
        let message = video.next().await.unwrap().unwrap();
        if let Message::Binary(bytes) = message {
            assert!(bytes.len() > 4);
            let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            let header: serde_json::Value = serde_json::from_slice(&bytes[4..4+length]).unwrap();
            if header["type"] == "access_unit" { break bytes; }
            assert_eq!(header["type"], "waiting");
        }
    };
    let length = u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&packet[4..4+length]).unwrap();
    assert_eq!(header["type"], "access_unit");
    assert_eq!(header["source"]["session_id"], ready["session_id"]);
    assert_eq!(header["byte_length"].as_u64().unwrap() as usize, packet.len() - 4 - length);
    control.send(Message::text(serde_json::json!({"id":3,"command":{"type":"request_takeover","epoch":attached["value"]["control"]["epoch"]}}).to_string())).await.unwrap();
    let takeover: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(takeover["value"]["control"]["phase"]["type"], "human");
    control.send(Message::text(serde_json::json!({"id":4,"operation":identity(&takeover["value"]),"command":{"type":"click","x":30,"y":30}}).to_string())).await.unwrap();
    let clicked: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(clicked["value"]["input"]["state"], "succeeded");
    control.send(Message::text(serde_json::json!({"id":5,"command":{"type":"release_control","epoch":takeover["value"]["control"]["epoch"]}}).to_string())).await.unwrap();
    let released: serde_json::Value = serde_json::from_str(control.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(released["value"]["control"]["phase"]["type"], "agent");
    state = local_request(&mut local, serde_json::json!({"id":6,"command":{"type":"status"}})).await;
    let effect = local_request(&mut local, serde_json::json!({"id":7,"operation":identity(&state),"command":{"type":"evaluate","expression":"window.clicked"}})).await;
    assert_eq!(effect["result"]["value"].as_f64(), Some(1.0));
    control.close(None).await.unwrap();
    while let Some(Ok(message)) = video.next().await { if message.is_close() { break; } }
    std::process::Command::new("kill").args(["-TERM", &endpoint.id().unwrap().to_string()]).status().unwrap();
    endpoint.wait().await.unwrap();
    std::process::Command::new("kill").args(["-TERM", &host.id().unwrap().to_string()]).status().unwrap();
    host.wait().await.unwrap();
    // Only this test's newly created directory and fixtures.
    std::fs::remove_dir_all(root).unwrap();
}

fn identity(state: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"session_id":state["session_id"],"attachment_id":state["attachment_id"],
        "sequence":state["next_sequence"],"control_epoch":state["control"]["epoch"],
        "viewport_revision":state["viewport_revision"],"document_revision":state["document_revision"]})
}

async fn local_request(socket: &mut BufReader<tokio::net::UnixStream>, request: serde_json::Value) -> serde_json::Value {
    socket.get_mut().write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let mut line = String::new(); socket.read_line(&mut line).await.unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["type"], "result", "{reply}"); reply["value"].clone()
}

async fn wait_socket(path: PathBuf) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(tokio::time::Instant::now() < deadline, "listener did not initialize");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn adb(serial: &str, arguments: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("adb");
    command.args(["-s", serial]).args(arguments).kill_on_drop(true);
    Ok(tokio::time::timeout(Duration::from_secs(8), command.output()).await??)
}

async fn device_ready(serial: &str, root: &std::path::Path, address: std::net::SocketAddr, session: &str) {
    let remote = format!("/data/local/tmp/{}", root.file_name().unwrap().to_str().unwrap());
    assert!(adb(serial, &["shell", "mkdir", "-m", "700", &remote]).await.unwrap().status.success());
    let result: anyhow::Result<_> = async {
        for name in ["ca.pem", "client.pem", "client.key"] {
            anyhow::ensure!(adb(serial, &["push", root.join(name).to_str().unwrap(), &format!("{remote}/{name}")]).await?.status.success(), "Device fixture upload failed");
        }
        anyhow::ensure!(adb(serial, &["shell", "chmod", "600", &format!("{remote}/client.key")]).await?.status.success(), "Device private key permissions failed");
        let command = format!("curl -i -sS --http1.1 --max-time 2 --cacert {remote}/ca.pem --cert {remote}/client.pem --key {remote}/client.key -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' https://{address}/control");
        adb(serial, &["shell", &command]).await
    }.await;
    // Remove only the three test credentials, including on upload/curl errors.
    let removed = adb(serial, &["shell", "rm", "-f", &format!("{remote}/ca.pem"), &format!("{remote}/client.pem"), &format!("{remote}/client.key")]).await;
    let directory = adb(serial, &["shell", "rmdir", &remote]).await;
    assert!(removed.unwrap().status.success() && directory.unwrap().status.success(), "Temporary device credential cleanup failed");
    let result = result.unwrap();
    let response = String::from_utf8_lossy(&result.stdout);
    assert!(response.contains("101 Switching Protocols") && response.contains(session), "Device TLS/WSS ready failed: {:?}; {}", result.status, String::from_utf8_lossy(&result.stderr));
    println!("DEVICE_TLS_WSS_READY_PASS: paired Android curl received the actual Host session; native media/UI not tested");
}
