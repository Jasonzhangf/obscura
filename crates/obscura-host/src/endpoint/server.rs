use std::{collections::HashMap, net::SocketAddr, os::unix::fs::{DirBuilderExt, PermissionsExt}, path::PathBuf, process::Stdio, sync::{Arc, Mutex}, time::Duration};
use anyhow::{ensure, Context, Result};
use tokio::{net::{TcpListener, UnixListener}, sync::{watch, Semaphore}};
use tokio_rustls::{TlsAcceptor, rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}}};
use tokio_tungstenite::{accept_hdr_async_with_config, tungstenite::{handshake::server::{Request, Response, ErrorResponse}, protocol::WebSocketConfig}};
use super::{Args, channels};

struct Grant { peer: Vec<u8>, active: watch::Sender<bool>, media_used: bool }
type Grants = Arc<Mutex<HashMap<String, Grant>>>;
struct Lease { token: String, grants: Grants }
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(grant) = self.grants.lock().expect("grant owner").remove(&self.token) { grant.active.send_replace(false); }
    }
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(self.0.join("encoded.sock")) { eprintln!("encoded socket cleanup: {error}"); }
        if let Err(error) = std::fs::remove_dir(&self.0) { eprintln!("endpoint directory cleanup: {error}"); }
    }
}
fn reject() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder().status(403).body(Some("Endpoint authorization rejected".into())).unwrap()
}

pub async fn serve(args: Args) -> Result<()> {
    if args.enable_webrtc { ensure!(args.webrtc_bind_ip.is_some(), "--enable-webrtc requires explicit --webrtc-bind-ip"); }
    let key_permissions = std::fs::metadata(&args.server_key)?.permissions().mode();
    ensure!(key_permissions & 0o077 == 0, "Endpoint private key must not be group/world accessible");
    let mut roots = rustls::RootCertStore::empty(); roots.add(CertificateDer::from(std::fs::read(&args.client_ca)?))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let tls = rustls::ServerConfig::builder().with_client_cert_verifier(verifier)
        .with_single_cert(vec![CertificateDer::from(std::fs::read(&args.server_cert)?)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(&args.server_key)?)))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let tcp = TcpListener::bind(args.listen).await.context("Bind explicit endpoint address")?;
    std::fs::DirBuilder::new().mode(0o700).create(&args.socket_dir).context("Endpoint socket directory must be new")?;
    let directory = Directory(args.socket_dir);
    let encoded = directory.0.join("encoded.sock");
    let listener = UnixListener::bind(&encoded)?;
    std::fs::set_permissions(&encoded, std::fs::Permissions::from_mode(0o600))?;
    let mut encoder = tokio::process::Command::new(&args.media_bin)
        .arg("--frames-socket").arg(args.host_dir.join("frames.sock")).arg("--video-socket").arg(&encoded)
        .arg("--ffmpeg").arg(args.ffmpeg).stdout(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true).spawn().context("Start single media adapter")?;
    let (latest, frames) = watch::channel(None);
    let (encoded_latest, encoded_frames) = watch::channel(None);
    let media = async {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await.context("Media adapter startup deadline")??;
        channels::ingest(socket, latest, encoded_latest).await
    };
    tokio::pin!(media);
    let grants: Grants = Arc::new(Mutex::new(HashMap::new()));
    let permits = Arc::new(Semaphore::new(32));
    let mut clients = tokio::task::JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let outcome = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = term.recv() => break Ok(()),
            result = &mut media => break result,
            accepted = tcp.accept() => {
                let (socket, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let acceptor = acceptor.clone(); let grants = grants.clone(); let frames = frames.clone();
                let encoded_frames = encoded_frames.clone(); let enable_webrtc = args.enable_webrtc;
                let webrtc_bind = args.webrtc_bind_ip.map(|ip| SocketAddr::new(ip, 0));
                let host = args.host_dir.join("host.sock");
                clients.spawn(async move {
                    let _permit = permit;
                    tokio::time::timeout(Duration::from_secs(3600), async {
                        let tls = tokio::time::timeout(Duration::from_secs(5), acceptor.accept(socket)).await??;
                        let peer = tls.get_ref().1.peer_certificates().context("Paired client certificate required")?[0].as_ref().to_vec();
                        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
                        let mut media_active = None;
                        let mut is_control = false;
                        let mut config = WebSocketConfig::default();
                        config.max_message_size = Some(64 * 1024); config.max_frame_size = Some(64 * 1024);
                        config.max_write_buffer_size = 5 * 1024 * 1024;
                        let remote = tokio::time::timeout(Duration::from_secs(5), accept_hdr_async_with_config(tls, |request: &Request, mut response: Response| {
                            // Native paired clients own TLS credentials. Do not let a
                            // website initiate a connection with ambient client certs.
                            if request.uri().query().is_some() || request.headers().contains_key("origin") { return Err(reject()); }
                            match request.uri().path() {
                                "/control" => {
                                    is_control = true;
                                    response.headers_mut().insert("x-obscura-media-token", token.parse().unwrap());
                                }
                                "/media" => {
                                    let supplied = request.headers().get("authorization").and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).ok_or_else(reject)?;
                                    let mut entries = grants.lock().expect("grant owner");
                                    let grant = entries.get_mut(supplied).ok_or_else(reject)?;
                                    if grant.peer != peer || !*grant.active.borrow() || grant.media_used { return Err(reject()); }
                                    grant.media_used = true; media_active = Some(grant.active.subscribe());
                                }
                                _ => return Err(reject()),
                            }
                            Ok(response)
                        }, Some(config))).await??;
                        if is_control {
                            let (active, _) = watch::channel(false);
                            grants.lock().expect("grant owner").insert(token.clone(), Grant { peer, active: active.clone(), media_used: false });
                            let _lease = Lease { token, grants };
                            channels::control(remote, &host, active, encoded_frames, enable_webrtc, webrtc_bind).await
                        } else { channels::media(remote, frames, media_active.context("Missing media grant")?).await }
                    }).await.context("Paired connection lifetime expired")?
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) => eprintln!("endpoint connection ended: {error}"),
                Err(error) => break Err(error.into()),
            },
        }
    };
    clients.abort_all(); while clients.join_next().await.is_some() {}
    if encoder.try_wait()?.is_none() { encoder.start_kill()?; }
    encoder.wait().await?;
    outcome
}
