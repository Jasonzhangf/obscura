use std::{collections::HashMap, net::SocketAddr, os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, process::Stdio, sync::{Arc, Mutex}, time::Duration};
use anyhow::{ensure, Context, Result};
use obscura_media::EncodedFrame;
use tokio::{net::{TcpListener, UnixListener}, process::Child, sync::{watch, oneshot, Mutex as AsyncMutex, Semaphore}};
use tokio_rustls::{TlsAcceptor, rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}}};
use tokio_tungstenite::{accept_hdr_async_with_config, tungstenite::{handshake::server::{Request, Response, ErrorResponse}, protocol::WebSocketConfig}};
use super::{Args, channels, encoder_control};
use crate::remote_authorization::{self, EndpointStatusReceipt};

/// One per-attachment endpoint grant. `media` is filled only after the Host
/// confirms an exact `(session_id, generation)` binding, so the `/media`
/// subscriber can only ever read that exact session's encoded stream.
struct Grant {
    peer: Vec<u8>,
    active: watch::Sender<channels::MediaState>,
    media_used: bool,
    media: Option<watch::Receiver<Option<Arc<EncodedFrame>>>>,
}
type Grants = Arc<Mutex<HashMap<String, Grant>>>;

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound { eprintln!("endpoint directory cleanup: {error}"); }
        }
    }
}
fn reject() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder().status(403).body(Some("Endpoint authorization rejected".into())).unwrap()
}

/// Owns exactly one adapter direct child. The child is never moved into the
/// ingest task, so an ingest panic cannot destroy it.
struct AdapterProcessOwner {
    child: Option<Child>,
}

impl AdapterProcessOwner {
    fn spawn(binary: &Path, frames_socket: &Path, encoded: &Path, ffmpeg: &Path) -> Result<Self> {
        let child = tokio::process::Command::new(binary)
            .arg("--frames-socket").arg(frames_socket)
            .arg("--video-socket").arg(encoded)
            .arg("--ffmpeg").arg(ffmpeg)
            .stdout(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true)
            .spawn().context("Start per-session media adapter")?;
        Ok(Self { child: Some(child) })
    }

    /// SIGTERM the adapter direct child, wait a bounded cooperative grace for
    /// its signal branch to finalize the retained encoder owner, then SIGKILL
    /// and asynchronously reap. It never signals the FFmpeg group itself.
    async fn finalize(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else { return Ok(()); };
        if child.try_wait()?.is_none() {
            let _ = signal_child(&child, libc::SIGTERM);
            if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                let _ = child.start_kill();
            }
        }
        let _ = child.wait().await;
        Ok(())
    }
}

fn signal_child(child: &Child, signal: libc::c_int) -> std::io::Result<()> {
    let Some(pid) = child.id() else { return Ok(()); };
    let pid = i32::try_from(pid).map_err(|_| std::io::Error::other("adapter pid exhausted"))?;
    // Safety: signaling one explicit direct child pid this owner spawned.
    let result = unsafe { libc::kill(pid, signal) };
    if result == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// One durable owner per exact session. The registry holds this record; it
/// survives the ingest task, every connection pump and every cancelled future.
pub struct SessionDurable {
    session_id: String,
    media_dir: PathBuf,
    encoded_path: PathBuf,
    encoded_control: encoder_control::EncoderControl,
    child: AsyncMutex<Option<AdapterProcessOwner>>,
    cancel: watch::Sender<bool>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
}

impl SessionDurable {
    /// Signal the known armed group directly and mark it `Revoked`. The pgid
    /// comes from the admission permit, never from a log or a guessed pid.
    async fn stop_armed_encoder_group(&self) -> Result<()> {
        let pgid = {
            let mut state = self.encoded_control.state.lock().await;
            state.begin_stop_armed_group()
        };
        if let Some(pgid) = pgid {
            obscura_media::encoder_owner::signal_group(pgid, libc::SIGTERM);
            if !wait_group_exit(pgid, Duration::from_secs(2)).await {
                obscura_media::encoder_owner::signal_group(pgid, libc::SIGKILL);
                let _ = wait_group_exit(pgid, Duration::from_secs(2)).await;
            }
            self.encoded_control.state.lock().await.mark_revoked();
        }
        Ok(())
    }

    /// Idempotent session finalizer. It closes the admission fence, settles
    /// every accepted or in-flight group, reaps the adapter while the healthy
    /// reader is still active, then stops the reader and removes resources.
    async fn finalize(&self) -> Result<()> {
        let settlement = {
            let mut state = self.encoded_control.state.lock().await;
            state.close_admission();
            state.take_settlement_receiver()
        };
        if let Some((_registration_id, receiver)) = settlement {
            match tokio::time::timeout(Duration::from_secs(5), receiver).await {
                Ok(Ok(obscura_media::encoder_owner::EncoderSettlement::Armed { pgid })) => {
                    obscura_media::encoder_owner::signal_group(pgid, libc::SIGTERM);
                    if !wait_group_exit(pgid, Duration::from_secs(2)).await {
                        obscura_media::encoder_owner::signal_group(pgid, libc::SIGKILL);
                        let _ = wait_group_exit(pgid, Duration::from_secs(2)).await;
                    }
                }
                Ok(Ok(
                    obscura_media::encoder_owner::EncoderSettlement::RefusedNoChild
                    | obscura_media::encoder_owner::EncoderSettlement::RefusedGroupStopped,
                )) => {}
                Ok(Err(_)) => {}
                Err(_) => {
                    // The settle deadline elapsed: adapter exit is the settle
                    // edge. The adapter's local refused-path cleanup owns the
                    // group; this finalizer does not SIGKILL an unbound group.
                }
            }
        }
        self.stop_armed_encoder_group().await?;
        if let Some(mut child) = self.child.lock().await.take() {
            child.finalize().await?;
        }
        // Only after adapter escalation does the reader stop. The supervisor
        // joins it after this finalizer returns.
        self.encoded_control.stop_reader();
        let _ = std::fs::remove_file(&self.encoded_path);
        let _ = std::fs::remove_file(&self.encoded_control.path);
        let _ = std::fs::remove_dir(&self.media_dir);
        Ok(())
    }
}

async fn wait_group_exit(pgid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !group_exists(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn group_exists(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

struct SessionMediaEntry {
    encoded_rx: watch::Receiver<Option<Arc<EncodedFrame>>>,
    durable: Arc<SessionDurable>,
    references: usize,
}

/// The endpoint's per-session registry. It owns every `SessionDurable` and the
/// adapters for the whole endpoint lifetime.
#[derive(Default)]
struct SessionMediaRegistry {
    entries: HashMap<String, SessionMediaEntry>,
}

impl SessionMediaRegistry {
    /// Create the exact-session resources if absent, then add one control
    /// reference and return the shared encoded receiver and durable handle.
    fn acquire(
        &mut self,
        socket_dir: &Path,
        host_dir: &Path,
        media_bin: &Path,
        ffmpeg: &Path,
        session_id: &str,
    ) -> Result<(watch::Receiver<Option<Arc<EncodedFrame>>>, Arc<SessionDurable>)> {
        if let Some(entry) = self.entries.get_mut(session_id) {
            entry.references = entry.references.saturating_add(1);
            return Ok((entry.encoded_rx.clone(), Arc::clone(&entry.durable)));
        }
        let media_dir = socket_dir.join(format!("session-{session_id}"));
        std::fs::DirBuilder::new().mode(0o700).create(&media_dir).context("Create per-session media directory")?;
        let encoded_path = media_dir.join("encoded.sock");
        let listener = UnixListener::bind(&encoded_path).context("Bind per-session encoded socket")?;
        std::fs::set_permissions(&encoded_path, std::fs::Permissions::from_mode(0o600))?;
        let mut control = encoder_control::EncoderControl::bind(socket_dir, session_id)?;
        let control_listener = control.take_listener().expect("encoder control listener is bound");
        let control_state = Arc::clone(&control.state);
        let frames_socket = host_dir.join(format!("session-{session_id}.sock"));
        let child = AdapterProcessOwner::spawn(media_bin, &frames_socket, &encoded_path, ffmpeg)?;
        let (encoded_tx, encoded_rx) = watch::channel(None);
        let (cancel, cancel_rx) = watch::channel(false);
        let (reader_stop_tx, reader_stop_rx) = oneshot::channel();
        control.set_reader_stop(reader_stop_tx);
        let durable = Arc::new(SessionDurable {
            session_id: session_id.to_owned(),
            media_dir,
            encoded_path,
            encoded_control: control,
            child: AsyncMutex::new(Some(child)),
            cancel,
            supervisor: Mutex::new(None),
        });
    let supervisor = spawn_supervisor(
        Arc::clone(&durable), listener, encoded_tx, encoded_rx.clone(),
        control_listener, control_state, cancel_rx, reader_stop_rx,
    );
    eprintln!("session media acquire: {session_id} child spawned, supervisor started");
        *durable.supervisor.lock().expect("supervisor owner") = Some(supervisor);
        self.entries.insert(session_id.to_owned(), SessionMediaEntry {
            encoded_rx: encoded_rx.clone(),
            durable,
            references: 1,
        });
        Ok((encoded_rx, Arc::clone(&self.entries[session_id].durable)))
    }

    fn release(&mut self, durable: &Arc<SessionDurable>) -> bool {
        let Some(entry) = self.entries.get_mut(&durable.session_id) else { return false; };
        entry.references = entry.references.saturating_sub(1);
        entry.references == 0
    }

    fn snapshots(&self) -> Vec<Arc<SessionDurable>> {
        self.entries.values().map(|entry| Arc::clone(&entry.durable)).collect()
    }
}

/// The per-session supervisor. Its only job is to own both JoinHandles, run the
/// durable finalizer while the healthy encoder-control reader is still active,
/// then stop and join that reader.
fn spawn_supervisor(
    durable: Arc<SessionDurable>,
    listener: UnixListener,
    encoded_tx: watch::Sender<Option<Arc<EncodedFrame>>>,
    _encoded_rx: watch::Receiver<Option<Arc<EncodedFrame>>>,
    control_listener: UnixListener,
    control_state: Arc<AsyncMutex<encoder_control::EncoderControlState>>,
    mut cancel_rx: watch::Receiver<bool>,
    reader_stop_rx: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        // Both JoinHandles have exactly this one joiner. The reader is spawned
        // first so a failure between spawn and the first poll is still observed
        // by the same select that owns the handle.
        let mut reader = tokio::spawn(encoder_control::run_reader(control_listener, control_state, reader_stop_rx));
        let mut ingest = tokio::spawn(channels::ingest(listener, encoded_tx, durable.cancel.subscribe()));
        let mut ingest_join: Option<Result<Result<()>, tokio::task::JoinError>> = None;
        let mut reader_join: Option<Result<Result<()>, tokio::task::JoinError>> = None;
        // Observe whichever finishes first: an external release/cancel, the
        // ingest terminal, or an unexpected encoder-control reader end. Each
        // handle is polled through `&mut` so a losing branch keeps its task.
        tokio::select! {
            _ = wait_cancel(&mut cancel_rx) => {}
            joined = &mut ingest => ingest_join = Some(joined),
            joined = &mut reader => reader_join = Some(joined),
        }
        // Total order: cancel and join ingest first.
        durable.cancel.send_replace(true);
        if ingest_join.is_none() {
            ingest_join = Some(ingest.await);
        }
        // Run the durable finalizer while the healthy encoder-control reader is
        // still active, so an adapter `RevokeEncoder` that arrives before exit
        // receives `Revoked` and a refused `RegisterEncoder` can settle.
        let finalize = durable.finalize().await;
        // Only after adapter escalation does the reader stop; then it is joined.
        durable.encoded_control.stop_reader();
        if reader_join.is_none() {
            reader_join = Some(reader.await);
        }
        finalize?;
        match ingest_join.expect("ingest join recorded") {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow::anyhow!("ingest task panicked")),
        }
        match reader_join.expect("reader join recorded") {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow::anyhow!("encoder control reader panicked")),
        }
        Ok(())
    })
}

async fn wait_cancel(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() { return; }
        if cancel.changed().await.is_err() { return; }
    }
}

/// Survives every cancellable connection pump. Owned by `serve` for the whole
/// endpoint lifetime, outside `clients`.
struct EndpointOwner {
    registry: Arc<AsyncMutex<SessionMediaRegistry>>,
    shutdown: watch::Sender<bool>,
    directory: Directory,
}

impl EndpointOwner {
    /// Session-local finalization entry. It marks that one entry finalizing,
    /// signals its cancel handle and awaits only that session's supervisor.
    async fn finalize_session(&self, durable: &Arc<SessionDurable>) -> Result<()> {
        let supervisor = { durable.supervisor.lock().expect("supervisor owner").take() };
        durable.cancel.send_replace(true);
        if let Some(supervisor) = supervisor {
            supervisor.await.context("Session supervisor panicked")??;
        }
        self.registry.lock().await.entries.remove(&durable.session_id);
        Ok(())
    }

    /// Actual endpoint shutdown: sets the shutdown watch, then finalizes every
    /// session through the same session-local owner.
    async fn finalize_all(&mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        let snapshots = self.registry.lock().await.snapshots();
        for durable in snapshots {
            if let Err(error) = self.finalize_session(&durable).await {
                eprintln!("endpoint session finalize: {error}");
            }
        }
        Ok(())
    }
}

pub async fn serve(args: Args) -> Result<()> {
    if args.enable_webrtc { ensure!(args.webrtc_bind_ip.is_some(), "--enable-webrtc requires explicit --webrtc-bind-ip"); }
    let projection = remote_authorization::LoadedProjection::load(&args.authorized_clients)?;
    let key_permissions = std::fs::metadata(&args.server_key)?.permissions().mode();
    ensure!(key_permissions & 0o077 == 0, "Endpoint private key must not be group/world accessible");
    let mut roots = rustls::RootCertStore::empty(); roots.add(CertificateDer::from(std::fs::read(&args.client_ca)?))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let tls = rustls::ServerConfig::builder().with_client_cert_verifier(verifier)
        .with_single_cert(vec![CertificateDer::from(std::fs::read(&args.server_cert)?)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(&args.server_key)?)))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let tcp = TcpListener::bind(args.listen).await.context("Bind explicit endpoint address")?;
    let bound = tcp.local_addr().context("Read bound endpoint address")?;
    std::fs::DirBuilder::new().mode(0o700).create(&args.socket_dir).context("Endpoint socket directory must be new")?;
    let directory = Directory(args.socket_dir.clone());
    // Applied receipt is written only after the TCP listener and the local
    // socket directory exist.
    write_applied_receipt(&args, &projection, bound)?;
    let projection = Arc::new(projection);
    let registry: Arc<AsyncMutex<SessionMediaRegistry>> = Arc::new(AsyncMutex::new(SessionMediaRegistry::default()));
    let grants: Grants = Arc::new(Mutex::new(HashMap::new()));
    let permits = Arc::new(Semaphore::new(32));
    let (shutdown, _shutdown_rx) = watch::channel(false);
    let mut owner = EndpointOwner { registry: Arc::clone(&registry), shutdown: shutdown.clone(), directory };
    let mut clients = tokio::task::JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let outcome = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = term.recv() => break Ok(()),
            accepted = tcp.accept() => {
                let (socket, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let acceptor = acceptor.clone();
                let grants = grants.clone();
                let enable_webrtc = args.enable_webrtc;
                let webrtc_bind = args.webrtc_bind_ip.map(|ip| SocketAddr::new(ip, 0));
                let host_dir = args.host_dir.clone();
                let socket_dir = args.socket_dir.clone();
                let media_bin = args.media_bin.clone();
                let ffmpeg = args.ffmpeg.clone();
                let registry = Arc::clone(&registry);
                let projection = Arc::clone(&projection);
                clients.spawn(async move {
                    let _permit = permit;
                    let result = tokio::time::timeout(Duration::from_secs(3600), async {
                        let tls = tokio::time::timeout(Duration::from_secs(5), acceptor.accept(socket)).await??;
                        let peer_cert = tls.get_ref().1.peer_certificates().context("Paired client certificate required")?[0].as_ref();
                        let peer = peer_cert.to_vec();
                        let peer_sha256 = remote_authorization::sha256(peer_cert);
                        let (principal, initial) = projection.principal_for_cert(&peer_sha256)
                            .context("ENDPOINT_CLIENT_NOT_AUTHORIZED: peer certificate is not in the authorized-client projection")?;
                        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
                        let mut media_active = None;
                        let mut is_control = false;
                        let grants_for_handshake = grants.clone();
                        let mut config = WebSocketConfig::default();
                        config.max_message_size = Some(64 * 1024); config.max_frame_size = Some(64 * 1024);
                        config.max_write_buffer_size = 5 * 1024 * 1024;
                        let remote = tokio::time::timeout(Duration::from_secs(5), accept_hdr_async_with_config(tls, |request: &Request, mut response: Response| {
                            if request.uri().query().is_some() || request.headers().contains_key("origin") { return Err(reject()); }
                            match request.uri().path() {
                                "/control" => {
                                    is_control = true;
                                    response.headers_mut().insert("x-obscura-media-token", token.parse().unwrap());
                                }
                                "/media" => {
                                    let supplied = request.headers().get("authorization").and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).ok_or_else(reject)?;
                                    let mut entries = grants_for_handshake.lock().expect("grant owner");
                                    let grant = entries.get_mut(supplied).ok_or_else(reject)?;
                                    if grant.peer != peer || !grant.active.borrow().attached || grant.media_used {
                                        eprintln!("media reject: peer={} attached={} used={}", grant.peer == peer, grant.active.borrow().attached, grant.media_used);
                                        return Err(reject());
                                    }
                                    let media = grant.media.clone().ok_or_else(|| { eprintln!("media reject: no session binding"); reject() })?;
                                    grant.media_used = true; media_active = Some((grant.active.subscribe(), media));
                                }
                                _ => return Err(reject()),
                            }
                            Ok(response)
                        }, Some(config))).await??;
                        if is_control {
                            let (active, _) = watch::channel(channels::MediaState::default());
                            grants.lock().expect("grant owner").insert(token.clone(), Grant {
                                peer,
                                active: active.clone(),
                                media_used: false,
                                media: None,
                            });
                            let grant = SessionGrant {
                                token: token.clone(), grants: grants.clone(), registry: Arc::clone(&registry),
                                socket_dir: socket_dir.clone(), host_dir: host_dir.clone(),
                                media_bin: media_bin.clone(), ffmpeg: ffmpeg.clone(),
                                bound: Arc::new(Mutex::new(None)),
                            };
                            let retained = Arc::clone(&grant.bound);
                            let result = channels::control(remote, &host_dir, active, grant, principal, initial, enable_webrtc, webrtc_bind).await;
                            let retained = { retained.lock().expect("grant binding owner").take() };
                            if let Some(durable) = retained {
                                release_exact_session(&registry, &durable).await;
                            }
                            result
                        } else {
                            let (active, media) = media_active.context("Missing media grant")?;
                            channels::media(remote, media, active).await
                        }
                    }).await.context("Paired connection lifetime expired")?;
                    result
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) => eprintln!("endpoint connection ended: {error}"),
                Err(error) => eprintln!("endpoint connection task failed: {error}"),
            },
        }
    };
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    owner.finalize_all().await?;
    drop(owner);
    outcome
}

/// The control connection's handle into the exact-session registry. It owns
/// the acquire/release reference for its current binding.
pub struct SessionGrant {
    token: String,
    grants: Grants,
    registry: Arc<AsyncMutex<SessionMediaRegistry>>,
    socket_dir: PathBuf,
    host_dir: PathBuf,
    media_bin: PathBuf,
    ffmpeg: PathBuf,
    // The exact-session reference this control connection currently holds. The
    // control boundary drains this slot after `channels::control` returns, so
    // an early transport error cannot leave the exact session unfinalized.
    bound: Arc<Mutex<Option<Arc<SessionDurable>>>>,
}

impl SessionGrant {
    /// Acquire the exact-session media reference for `session_id`. The control
    /// attach path never blocks on an adapter connection or first frame.
    pub async fn acquire(&self, session_id: &str) -> Result<(watch::Receiver<Option<Arc<EncodedFrame>>>, Arc<SessionDurable>)> {
        let (encoded, durable) = self.registry.lock().await
            .acquire(&self.socket_dir, &self.host_dir, &self.media_bin, &self.ffmpeg, session_id)?;
        if let Some(grant) = self.grants.lock().expect("grant owner").get_mut(&self.token) {
            grant.media = Some(encoded.clone());
        }
        *self.bound.lock().expect("grant binding owner") = Some(Arc::clone(&durable));
        Ok((encoded, durable))
    }

    /// Lose one control reference for the exact binding. At zero references it
    /// finalizes only that session.
    pub async fn release(&self, durable: &Arc<SessionDurable>) {
        {
            let mut bound = self.bound.lock().expect("grant binding owner");
            if bound.as_ref().is_some_and(|held| Arc::ptr_eq(held, durable)) {
                *bound = None;
            }
        }
        release_exact_session(&self.registry, durable).await;
    }
}

impl Drop for SessionGrant {
    fn drop(&mut self) {
        if let Some(grant) = self.grants.lock().expect("grant owner").remove(&self.token) {
            grant.active.send_replace(channels::MediaState::default());
        }
    }
}

/// Drop one exact-session reference and, at zero references, join that
/// session's supervisor. Only the exact session is finalized; other sessions
/// and the endpoint lifetime are untouched.
async fn release_exact_session(registry: &AsyncMutex<SessionMediaRegistry>, durable: &Arc<SessionDurable>) {
    let mut registry = registry.lock().await;
    if registry.release(durable) {
        let supervisor = { durable.supervisor.lock().expect("supervisor owner").take() };
        durable.cancel.send_replace(true);
        if let Some(supervisor) = supervisor {
            let _ = supervisor.await;
        }
        registry.entries.remove(&durable.session_id);
    }
}

fn write_applied_receipt(args: &Args, projection: &remote_authorization::LoadedProjection, bound: SocketAddr) -> Result<()> {
    let Some(path) = &args.applied_receipt_path else { return Ok(()); };
    let Some(parent) = path.parent() else { return Ok(()); };
    std::fs::create_dir_all(parent)?;
    let receipt = EndpointStatusReceipt::applied_v1(projection, bound.to_string(), args.endpoint_generation, std::process::id());
    let bytes = serde_json::to_vec(&receipt)?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
