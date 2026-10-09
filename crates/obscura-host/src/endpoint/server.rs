use std::{collections::HashMap, net::SocketAddr, os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, process::Stdio, sync::{Arc, Mutex}, time::Duration};
use anyhow::{bail, ensure, Context, Result};
use obscura_media::EncodedFrame;
use tokio::{net::{TcpListener, UnixListener}, process::Child, sync::{watch, oneshot, Mutex as AsyncMutex, Semaphore}};
use tokio_rustls::{TlsAcceptor, rustls::{self, pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}}};
use tokio_tungstenite::{accept_hdr_async_with_config, tungstenite::{handshake::server::{Request, Response, ErrorResponse}, protocol::WebSocketConfig}};
use super::{Args, channels, encoder_control};
use crate::remote_authorization::{self, EndpointStatusReceipt};

/// Bounded window the session finalizer waits for an acquired encoder permit to
/// settle through the control stream before it treats adapter exit as the
/// remaining settle edge.
const ENCODER_ADMISSION_SETTLE_DEADLINE: Duration = Duration::from_secs(5);
/// Bounded cooperative grace the session finalizer gives the adapter direct
/// child before it stops claiming that an unsettled encoder subtree was
/// settled. The endpoint never SIGKILLs the adapter while the permit is
/// unsettled.
const ADAPTER_COOPERATIVE_GRACE: Duration = Duration::from_secs(3);

/// A typed session terminal that is an explicit failure rather than a cleanup
/// success. `EncoderPermitUnsettled` is returned when an acquired encoder
/// permit was not settled within the bounded settle deadline and adapter
/// cooperative grace, so the endpoint has no signal authority for the
/// separately spawned group and must not report successful cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTerminal {
    EncoderPermitUnsettled { registration_id: u64 },
}

/// macOS binds Unix sockets below `SUN_LEN`. The endpoint owns this layout, so
/// it validates both of its own socket names before creating anything.
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
/// Endpoint-owned per-session media directory prefix.
const MEDIA_DIR_PREFIX: &str = "m-";
/// Hex chars of the canonical session UUID used as the bounded on-disk media id.
/// The full 36-char canonical UUID stays the session identity for the registry,
/// the admission decision and the Host frame socket; only this endpoint-owned
/// directory name is truncated so both socket paths fit `SUN_LEN` at any
/// deployment prefix. A truncated id must never silently share a directory, so
/// acquisition creates it exclusively and rejects a collision.
const MEDIA_ID_HEX_CHARS: usize = 12;
const ENCODED_SOCKET_NAME: &str = "encoded.sock";
const CONTROL_SOCKET_NAME: &str = "encoder-control.sock";

/// The session identity is a canonical UUID. The Host generates it, and it is
/// what the registry keys, the admission projection carries and the raw frame
/// socket is named with.
fn validate_canonical_session_id(session_id: &str) -> Result<()> {
    ensure!(session_id.len() == 36, "Session media socket requires a canonical UUID");
    for (position, byte) in session_id.bytes().enumerate() {
        if byte == b'-' {
            ensure!(matches!(position, 8 | 13 | 18 | 23),
                "Session media socket requires a canonical UUID");
            continue;
        }
        ensure!(byte.is_ascii_hexdigit(), "Session media socket requires a canonical UUID");
    }
    Ok(())
}

/// Endpoint-owned per-session media directory name: prefix plus the bounded
/// fingerprint of the validated session identity.
pub(crate) fn media_dir_name(session_id: &str) -> Result<String> {
    validate_canonical_session_id(session_id)?;
    let hex = session_id.replace('-', "");
    Ok(format!("{prefix}{id}", prefix = MEDIA_DIR_PREFIX, id = &hex[..MEDIA_ID_HEX_CHARS]))
}

/// Create the per-session media directory exclusively. A fingerprint collision
/// in this endpoint-owned space is an explicit failure, never a shared socket.
fn create_media_dir(media_dir: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(media_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "Session media directory fingerprint collision for {}",
            media_dir.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
        ),
        Err(error) => Err(error).context("Create per-session media directory"),
    }
}

/// Reject an endpoint-owned socket layout before the first bind, so an overlong
/// deployment prefix fails as a typed acquisition error instead of a partial
/// bind that leaves media resources behind.
fn ensure_media_socket_paths_fit(socket_dir: &Path, media_dir: &str) -> Result<()> {
    let encoded = socket_dir.join(media_dir).join(ENCODED_SOCKET_NAME);
    ensure!(
        encoded.to_string_lossy().len() < UNIX_SOCKET_PATH_LIMIT,
        "Per-session encoded socket path exceeds macOS bound"
    );
    let control = socket_dir.join(media_dir).join(CONTROL_SOCKET_NAME);
    ensure!(
        control.to_string_lossy().len() < UNIX_SOCKET_PATH_LIMIT,
        "Per-session encoder-control socket path exceeds macOS bound"
    );
    Ok(())
}

impl std::fmt::Display for SessionTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionTerminal::EncoderPermitUnsettled { registration_id } => write!(
                formatter,
                "SESSION_ENCODER_PERMIT_UNSETTLED: encoder permit {registration_id} was not settled before adapter cooperative grace"
            ),
        }
    }
}

impl std::error::Error for SessionTerminal {}

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

/// Owns the per-session files created before `SessionDurable` exists. If any
/// startup step fails, it removes only this acquisition's paths and directory.
struct SessionAcquisition {
    media_dir: PathBuf,
    encoded_path: PathBuf,
    control_path: PathBuf,
    committed: bool,
}

impl SessionAcquisition {
    fn new(media_dir: PathBuf, encoded_path: PathBuf, control_path: PathBuf) -> Self {
        Self { media_dir, encoded_path, control_path, committed: false }
    }

    fn commit(&mut self) { self.committed = true; }
}

impl Drop for SessionAcquisition {
    fn drop(&mut self) {
        if self.committed { return; }
        let _ = std::fs::remove_file(&self.encoded_path);
        let _ = std::fs::remove_file(&self.control_path);
        let _ = std::fs::remove_dir(&self.media_dir);
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
            if tokio::time::timeout(ADAPTER_COOPERATIVE_GRACE, child.wait()).await.is_err() {
                let _ = child.start_kill();
            }
        }
        let _ = child.wait().await;
        Ok(())
    }

    /// SIGTERM the adapter direct child and await its bounded cooperative grace
    /// without escalating to SIGKILL. Returns whether the child exited. Used
    /// when an in-flight encoder permit is unsettled: adapter exit is the only
    /// remaining settle edge, and the endpoint must not force-kill the adapter
    /// before that settle is observed.
    async fn cooperative_exit(&mut self, grace: Duration) -> Result<bool> {
        let Some(child) = self.child.as_mut() else { return Ok(true); };
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        let _ = signal_child(child, libc::SIGTERM);
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(status) => {
                status?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
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
        let mut unsettled = None;
        if let Some((registration_id, receiver)) = settlement {
            match tokio::time::timeout(ENCODER_ADMISSION_SETTLE_DEADLINE, receiver).await {
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
                // A closed settlement channel or an elapsed deadline leaves the
                // in-flight permit unsettled. The endpoint never bound this
                // pgid, so it has no signal authority and must not report
                // successful cleanup. Retain the registration identity.
                Ok(Err(_)) | Err(_) => unsettled = Some(registration_id),
            }
        }
        self.stop_armed_encoder_group().await?;
        if let Some(registration_id) = unsettled {
            // Adapter exit is the remaining settle edge. SIGTERM the adapter
            // direct child and wait its bounded cooperative grace without
            // SIGKILL: only cooperative exit proves its signal branch ran the
            // local refused-group cleanup for the unregistered group.
            let exited = self.cooperative_adapter_exit().await?;
            if !exited {
                return Err(anyhow::Error::new(SessionTerminal::EncoderPermitUnsettled { registration_id }));
            }
            // Adapter exit settles the permit; drop the already-reaped direct
            // child handle.
            let _ = self.child.lock().await.take();
        } else if let Some(mut child) = self.child.lock().await.take() {
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

    /// SIGTERM the adapter direct child and await its bounded cooperative grace
    /// without SIGKILL escalation. Returns whether the child exited.
    async fn cooperative_adapter_exit(&self) -> Result<bool> {
        let mut guard = self.child.lock().await;
        match guard.as_mut() {
            Some(child) => child.cooperative_exit(ADAPTER_COOPERATIVE_GRACE).await,
            None => Ok(true),
        }
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
    finalizing: bool,
    /// Present from the moment `finalizing` is set. The finalizer task owns the
    /// sender and publishes the terminal result when it finishes. It publishes
    /// `Succeeded` only after the session's resources are actually removed; a
    /// failed finalization publishes `Failed` and leaves the entry (and its
    /// durable resource owner, live adapter child and paths) retained, so
    /// release and shutdown can never convert an unsettled encoder permit into
    /// successful cleanup. Awaiters subscribe to this sender.
    completion: Option<watch::Sender<CompletionState>>,
}

/// The terminal result of one session finalization. `Running` is the initial
/// value; `Succeeded` means every resource was removed; `Failed` retains the
/// durable owner and carries the exact terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionState {
    Running,
    Succeeded,
    Failed(String),
}

enum Release {
    Absent,
    Referenced,
    Ready(Arc<SessionDurable>, watch::Sender<CompletionState>),
    Finalizing(watch::Receiver<CompletionState>),
}

/// Outcome of asking the registry to finalize one entry. A fresh entry yields
/// the supervisor handle and a completion sender to publish on; an entry that
/// is already finalizing yields its in-progress completion receiver to await.
enum FinalizeAction {
    Absent,
    InProgress(watch::Receiver<CompletionState>),
    Start(Option<tokio::task::JoinHandle<Result<()>>>, watch::Sender<CompletionState>),
}

/// The endpoint's per-session registry. It owns every `SessionDurable` and the
/// adapters for the whole endpoint lifetime.
struct SessionMediaRegistry {
    entries: HashMap<String, SessionMediaEntry>,
}

impl Default for SessionMediaRegistry {
    fn default() -> Self {
        Self { entries: HashMap::new() }
    }
}

/// Readiness of one session entry for a new [`SessionGrant::acquire`]. `Open`
/// means the entry is absent or not finalizing. `Finalizing` carries the
/// in-progress terminal to await outside the registry lock. `Failed` carries
/// the retained finalization failure so a new attachment returns it instead of
/// waiting on a generation that never advances.
enum SessionAvailability {
    Open,
    Finalizing(watch::Receiver<CompletionState>),
    Failed(String),
}

/// Why [`SessionGrant::acquire`] returned without a binding. Only
/// `FinalizationFailed` is an explicit attachment terminal; an ordinary
/// `Acquire` failure keeps the previous behavior of logging the error and
/// leaving the control attachment alive with an unbound media grant.
pub enum AcquireError {
    /// The session entry is retained with a failed finalization. The new
    /// attachment learns the retained terminal instead of waiting on a
    /// finalization that will never complete.
    FinalizationFailed(String),
    /// An ordinary acquisition failure (adapter spawn, socket bind, ...).
    Acquire(anyhow::Error),
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
            ensure!(!entry.finalizing, "SESSION_FINALIZING: session media entry is shutting down");
            entry.references = entry.references.saturating_add(1);
            return Ok((entry.encoded_rx.clone(), Arc::clone(&entry.durable)));
        }
        let media_name = media_dir_name(session_id)?;
        ensure_media_socket_paths_fit(socket_dir, &media_name)?;
        let media_dir = socket_dir.join(&media_name);
        create_media_dir(&media_dir)?;
        let encoded_path = media_dir.join("encoded.sock");
        let control_path = media_dir.join("encoder-control.sock");
        let mut acquisition = SessionAcquisition::new(media_dir.clone(), encoded_path.clone(), control_path);
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
        acquisition.commit();
        self.entries.insert(session_id.to_owned(), SessionMediaEntry {
            encoded_rx: encoded_rx.clone(),
            durable,
            references: 1,
            finalizing: false,
            completion: None,
        });
        Ok((encoded_rx, Arc::clone(&self.entries[session_id].durable)))
    }

    fn release(&mut self, durable: &Arc<SessionDurable>) -> Release {
        let Some(entry) = self.entries.get_mut(&durable.session_id) else { return Release::Absent; };
        if entry.finalizing {
            let receiver = entry.completion.as_ref().map(|completion| completion.subscribe());
            return match receiver {
                Some(receiver) => Release::Finalizing(receiver),
                None => Release::Absent,
            };
        }
        entry.references = entry.references.saturating_sub(1);
        if entry.references == 0 {
            entry.finalizing = true;
            let (completion, _completion_rx) = watch::channel(CompletionState::Running);
            entry.completion = Some(completion.clone());
            Release::Ready(Arc::clone(&entry.durable), completion)
        } else {
            Release::Referenced
        }
    }

    fn begin_finalize(&mut self, durable: &Arc<SessionDurable>) -> FinalizeAction {
        let Some(entry) = self.entries.get_mut(&durable.session_id) else { return FinalizeAction::Absent; };
        if entry.finalizing {
            return match entry.completion.as_ref() {
                Some(completion) => FinalizeAction::InProgress(completion.subscribe()),
                None => FinalizeAction::Absent,
            };
        }
        entry.finalizing = true;
        let (completion, _completion_rx) = watch::channel(CompletionState::Running);
        entry.completion = Some(completion.clone());
        let supervisor = entry.durable.supervisor.lock().expect("supervisor owner").take();
        FinalizeAction::Start(supervisor, completion)
    }

    fn remove_finalized(&mut self, durable: &Arc<SessionDurable>) {
        let same = self.entries.get(&durable.session_id)
            .is_some_and(|entry| entry.finalizing && Arc::ptr_eq(&entry.durable, durable));
        if same {
            self.entries.remove(&durable.session_id);
        }
    }

    /// Classify a session entry for acquisition. A retained failure is an
    /// explicit terminal: the caller returns it instead of waiting on a
    /// finalization that will never complete.
    fn availability(&self, session_id: &str) -> SessionAvailability {
        let Some(entry) = self.entries.get(session_id) else { return SessionAvailability::Open; };
        if !entry.finalizing { return SessionAvailability::Open; }
        match entry.completion.as_ref().map(|completion| completion.borrow().clone()) {
            Some(CompletionState::Failed(failure)) => SessionAvailability::Failed(failure),
            _ => match entry.completion.as_ref().map(|completion| completion.subscribe()) {
                Some(receiver) => SessionAvailability::Finalizing(receiver),
                None => SessionAvailability::Failed(
                    "SESSION_FINALIZE_ABANDONED: session media entry is finalizing without a completion owner".into(),
                ),
            },
        }
    }

    fn snapshots(&self) -> Vec<Arc<SessionDurable>> {
        self.entries.values().map(|entry| Arc::clone(&entry.durable)).collect()
    }
}

/// Await one entry's finalization completion and report its terminal result.
/// `Succeeded` means resources were removed; `Failed` carries the exact
/// terminal and the entry is retained. A closed channel means the completion
/// sender was dropped without publishing, which is itself a failure.
async fn await_completion(mut receiver: watch::Receiver<CompletionState>) -> Result<()> {
    loop {
        match receiver.borrow().clone() {
            CompletionState::Running => {}
            CompletionState::Succeeded => return Ok(()),
            CompletionState::Failed(failure) => return Err(anyhow::anyhow!(failure)),
        }
        if receiver.changed().await.is_err() {
            bail!("SESSION_FINALIZE_ABANDONED: session finalizer ended without a terminal result");
        }
    }
}

/// The per-session supervisor. Its only job is to own both JoinHandles, run the
/// durable finalizer while the healthy encoder-control reader is still active,
/// then stop and join that reader. It is also the single exit for the three
/// owners, so it reports every terminal they produced instead of the first one.
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
        // The supervisor is the single exit for three owners, so it must keep
        // every terminal. A failing finalizer must not replace the reader's
        // original error, and the reader's original error must not replace an
        // unsettled terminal. Both the published `CompletionState::Failed` and
        // the endpoint error exit render this one error's own message, so when
        // more than one owner fails that message names every cause.
        let mut failures: Vec<anyhow::Error> = Vec::new();
        if let Err(error) = finalize { failures.push(error); }
        match ingest_join.expect("ingest join recorded") {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(error),
            Err(_) => failures.push(anyhow::anyhow!("ingest task panicked")),
        }
        match reader_join.expect("reader join recorded") {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(error),
            Err(_) => failures.push(anyhow::anyhow!("encoder control reader panicked")),
        }
        match failures.len() {
            0 => Ok(()),
            // One owner failed: its own error is returned unchanged, so every
            // established single-terminal message stays exact.
            1 => Err(failures.pop().expect("one recorded failure")),
            // More than one owner failed: name every cause in the returned
            // error's own message and keep the finalizer's terminal as the cause
            // chain tail.
            _ => {
                let causes: Vec<String> = failures.iter().map(|error| format!("{error:#}")).collect();
                let primary = failures.remove(0);
                Err(primary.context(format!("SESSION_MULTIPLE_TERMINALS: {}", causes.join(" | "))))
            }
        }
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
    /// signals its cancel handle and awaits that session's supervisor. If the
    /// entry is already finalizing (a release is in flight), it awaits the
    /// in-progress completion instead of starting a second finalizer. The
    /// completion await is never taken while the registry mutex is held: a
    /// finalizing entry is only published `Succeeded` after this mutex is
    /// re-acquired, so a waiter that held it would deadlock against its own
    /// finalizer.
    async fn finalize_session(&self, durable: &Arc<SessionDurable>) -> Result<()> {
        let action = { self.registry.lock().await.begin_finalize(durable) };
        match action {
            FinalizeAction::Absent => Ok(()),
            FinalizeAction::InProgress(completion) => return await_completion(completion).await,
            FinalizeAction::Start(supervisor, completion) => {
                durable.cancel.send_replace(true);
                let joined = match supervisor {
                    Some(supervisor) => supervisor.await,
                    None => Ok(Ok(())),
                };
                let result = match joined {
                    Ok(result) => result,
                    Err(error) => Err(anyhow::Error::new(error).context("Session supervisor panicked")),
                };
                match result {
                    Ok(()) => {
                        self.registry.lock().await.remove_finalized(durable);
                        completion.send_replace(CompletionState::Succeeded);
                        Ok(())
                    }
                    Err(error) => {
                        // A failed finalizer retains the durable resource
                        // owner, live adapter child and session paths. The
                        // entry is not removed; the terminal is published so
                        // every awaiter observes the failure, not success.
                        completion.send_replace(CompletionState::Failed(error.to_string()));
                        Err(error)
                    }
                }
            }
        }
    }

    /// Actual endpoint shutdown: sets the shutdown watch, then finalizes every
    /// session through the same session-local owner. A per-session failure is
    /// kept scoped to that session and surfaced, not converted into a
    /// successful endpoint shutdown.
    async fn finalize_all(&mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        let snapshots = self.registry.lock().await.snapshots();
        let mut failures = Vec::new();
        for durable in snapshots {
            if let Err(error) = self.finalize_session(&durable).await {
                eprintln!("endpoint session finalize: {error}");
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("ENDPOINT_SESSION_FINALIZE_FAILED: {}", failures.join("; "))
        }
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
                    let binding_guard = SessionBindingGuard::new(
                        Arc::clone(&registry),
                        Arc::new(Mutex::new(None)),
                    );
                    let bound = Arc::clone(&binding_guard.bound);
                    let timed = tokio::time::timeout(Duration::from_secs(3600), async move {
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
                                bound,
                            };
                            channels::control(remote, &host_dir, active, grant, principal, initial, enable_webrtc, webrtc_bind).await
                        } else {
                            let (active, media) = media_active.context("Missing media grant")?;
                            channels::media(remote, media, active).await
                        }
                    }).await;
                    let result = match timed {
                        Ok(result) => result,
                        Err(error) => {
                            binding_guard.drain().await;
                            return Err(anyhow::Error::new(error).context("Paired connection lifetime expired"));
                        }
                    };
                    binding_guard.drain().await;
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
struct SessionBindingGuard {
    registry: Arc<AsyncMutex<SessionMediaRegistry>>,
    bound: Arc<Mutex<Option<Arc<SessionDurable>>>>,
}

impl SessionBindingGuard {
    fn new(registry: Arc<AsyncMutex<SessionMediaRegistry>>, bound: Arc<Mutex<Option<Arc<SessionDurable>>>>) -> Self {
        Self { registry, bound }
    }

    /// Drain the surviving bound slot after success, error or timeout.
    async fn drain(&self) {
        let durable = self.bound.lock().expect("grant binding owner").take();
        if let Some(durable) = durable {
            let registry = Arc::clone(&self.registry);
            let completion = tokio::spawn(async move { release_exact_session(&registry, &durable).await });
            if let Ok(Err(error)) = completion.await {
                eprintln!("session media finalize terminal: {error}");
            }
        }
    }
}

impl Drop for SessionBindingGuard {
    fn drop(&mut self) {
        let Some(durable) = self.bound.lock().expect("grant binding owner").take() else { return; };
        let registry = Arc::clone(&self.registry);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = release_exact_session(&registry, &durable).await {
                    eprintln!("session media finalize terminal: {error}");
                }
            });
        }
    }
}

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
    /// attach path never blocks on an adapter connection or first frame. When
    /// the same session's entry is running its finalization, the acquire waits
    /// for that entry to reach its terminal and then creates a fresh one, so a
    /// new attachment never reuses a durable that is already shutting down. A
    /// retained finalization failure is returned explicitly instead of waiting
    /// on an entry that is deliberately kept and will never be removed. The
    /// completion wait is never taken while the registry mutex is held.
    pub async fn acquire(&self, session_id: &str) -> std::result::Result<(watch::Receiver<Option<Arc<EncodedFrame>>>, Arc<SessionDurable>), AcquireError> {
        let (encoded, durable) = loop {
            let availability = {
                let mut registry = self.registry.lock().await;
                match registry.availability(session_id) {
                    SessionAvailability::Open => break registry.acquire(
                        &self.socket_dir, &self.host_dir, &self.media_bin, &self.ffmpeg, session_id,
                    ).map_err(AcquireError::Acquire)?,
                    other => other,
                }
            };
            match availability {
                SessionAvailability::Finalizing(completion) => {
                    // Await the running finalization outside the registry lock.
                    // A failure terminal is returned explicitly; success means
                    // the entry was removed, so the loop acquires a fresh one.
                    await_completion(completion).await
                        .map_err(|error| AcquireError::FinalizationFailed(format!("{error:#}")))?;
                }
                SessionAvailability::Failed(failure) => return Err(AcquireError::FinalizationFailed(failure)),
                SessionAvailability::Open => unreachable!("open entries acquire inside the lock"),
            }
        };
        if let Some(grant) = self.grants.lock().expect("grant owner").get_mut(&self.token) {
            grant.media = Some(encoded.clone());
        }
        *self.bound.lock().expect("grant binding owner") = Some(Arc::clone(&durable));
        Ok((encoded, durable))
    }

    /// Lose one control reference for the exact binding. At zero references it
    /// finalizes only that session and returns its terminal result.
    pub async fn release(&self, durable: &Arc<SessionDurable>) -> Result<()> {
        {
            let mut bound = self.bound.lock().expect("grant binding owner");
            if bound.as_ref().is_some_and(|held| Arc::ptr_eq(held, durable)) {
                *bound = None;
            }
        }
        let registry = Arc::clone(&self.registry);
        let durable = Arc::clone(durable);
        let completion = tokio::spawn(async move { release_exact_session(&registry, &durable).await });
        match completion.await {
            Ok(result) => result,
            Err(error) => Err(anyhow::Error::new(error).context("session finalizer task panicked")),
        }
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
/// and the endpoint lifetime are untouched. A failed finalizer retains the
/// durable resource owner and its exact terminal instead of publishing
/// successful cleanup.
async fn release_exact_session(registry: &Arc<AsyncMutex<SessionMediaRegistry>>, durable: &Arc<SessionDurable>) -> Result<()> {
    // Take the release action under the registry lock, but never await the
    // finalization completion while holding it. The finalizer must re-acquire
    // this same mutex in `remove_finalized` before it publishes `Succeeded`; a
    // waiter that held the guard across the completion await would deadlock
    // against its own finalizer and block unrelated sessions.
    enum Action {
        Nothing,
        Await(watch::Receiver<CompletionState>),
        Run(Arc<SessionDurable>, watch::Sender<CompletionState>),
    }
    let action = {
        let mut registry = registry.lock().await;
        match registry.release(durable) {
            Release::Ready(durable, completion) => Action::Run(durable, completion),
            Release::Finalizing(completion) => Action::Await(completion),
            Release::Absent | Release::Referenced => Action::Nothing,
        }
    };
    // The guard above is already dropped: the completion await and the
    // supervisor join below both run without holding the registry mutex.
    let (durable, completion) = match action {
        Action::Nothing => return Ok(()),
        Action::Await(completion) => return await_completion(completion).await,
        Action::Run(durable, completion) => (durable, completion),
    };
    let supervisor = durable.supervisor.lock().expect("supervisor owner").take();
    let registry = Arc::clone(registry);
    // The completion runs in a detached task so aborting the caller after the
    // reference reaches zero cannot strand the entry in `finalizing`.
    let task = tokio::spawn(async move {
        durable.cancel.send_replace(true);
        let joined = match supervisor {
            Some(supervisor) => match supervisor.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::Error::new(error).context("Session supervisor panicked")),
            },
            None => Ok(()),
        };
        match joined {
            Ok(()) => {
                registry.lock().await.remove_finalized(&durable);
                completion.send_replace(CompletionState::Succeeded);
                Ok(())
            }
            Err(error) => {
                // Retain the durable, the live child and the paths on failure;
                // publish the exact terminal so a later waiter observes it.
                eprintln!("session media finalize terminal: {error}");
                completion.send_replace(CompletionState::Failed(error.to_string()));
                Err(error)
            }
        }
    });
    match task.await {
        Ok(result) => result,
        Err(error) => Err(anyhow::Error::new(error).context("session finalizer task panicked")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let root = PathBuf::from("/tmp").join(format!("sv-{name}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn durable(root: &Path, id: &str, supervisor: tokio::task::JoinHandle<Result<()>>) -> Arc<SessionDurable> {
        let control = encoder_control::EncoderControl::bind(root, id).unwrap();
        let media_dir = root.join(media_dir_name(id).unwrap());
        let encoded_path = media_dir.join("encoded.sock");
        let (cancel, _) = watch::channel(false);
        Arc::new(SessionDurable {
            session_id: id.to_owned(),
            media_dir,
            encoded_path,
            encoded_control: control,
            child: AsyncMutex::new(None),
            cancel,
            supervisor: Mutex::new(Some(supervisor)),
        })
    }

    #[test]
    fn endpoint_media_dir_uses_a_bounded_fingerprint_of_the_canonical_identity() {
        let session_id = "603d917e-965a-4a60-bb4a-834466dc5df8";
        assert_eq!(media_dir_name(session_id).unwrap(), "m-603d917e965a");
        assert!(media_dir_name("603d917e-965a-4a60-bb4a-834466dc5df8Z").is_err());
        assert!(media_dir_name("603d917e965a-4a60-bb4a-834466dc5df8").is_err());
        assert!(media_dir_name("cancel-session").is_err());
    }

    /// Both endpoint-owned socket budgets are checked at their own threshold:
    /// the encoded path fails at the limit, and the eight bytes longer
    /// encoder-control path fails at the limit while the encoded path still
    /// fits. Nothing is created before the check.
    #[test]
    fn endpoint_media_socket_paths_are_checked_before_any_bind() {
        fn socket_dir_for(path_len: usize, name: &str, media_dir: &str) -> PathBuf {
            let pad = path_len - "/tmp/".len() - 1 - media_dir.len() - 1 - name.len();
            PathBuf::from("/tmp").join("b".repeat(pad))
        }
        let session_id = "603d917e-965a-4a60-bb4a-834466dc5df8";
        let media_name = media_dir_name(session_id).unwrap();
        let encoded = |socket_dir: &Path| socket_dir.join(&media_name).join(ENCODED_SOCKET_NAME);
        let control = |socket_dir: &Path| socket_dir.join(&media_name).join(CONTROL_SOCKET_NAME);

        let encoded_over = socket_dir_for(UNIX_SOCKET_PATH_LIMIT, ENCODED_SOCKET_NAME, &media_name);
        std::fs::create_dir_all(&encoded_over).unwrap();
        assert_eq!(encoded(&encoded_over).to_string_lossy().len(), UNIX_SOCKET_PATH_LIMIT);
        let error = ensure_media_socket_paths_fit(&encoded_over, &media_name).unwrap_err().to_string();
        assert!(error.contains("encoded socket path exceeds macOS bound"), "{error}");

        let control_over = socket_dir_for(UNIX_SOCKET_PATH_LIMIT, CONTROL_SOCKET_NAME, &media_name);
        std::fs::create_dir_all(&control_over).unwrap();
        assert!(encoded(&control_over).to_string_lossy().len() < UNIX_SOCKET_PATH_LIMIT);
        assert_eq!(control(&control_over).to_string_lossy().len(), UNIX_SOCKET_PATH_LIMIT);
        let error = ensure_media_socket_paths_fit(&control_over, &media_name).unwrap_err().to_string();
        assert!(error.contains("encoder-control socket path exceeds macOS bound"), "{error}");

        let at_budget = socket_dir_for(UNIX_SOCKET_PATH_LIMIT - 1, CONTROL_SOCKET_NAME, &media_name);
        std::fs::create_dir_all(&at_budget).unwrap();
        ensure_media_socket_paths_fit(&at_budget, &media_name).unwrap();

        for socket_dir in [&encoded_over, &control_over, &at_budget] {
            assert!(!socket_dir.join(&media_name).exists(), "the layout check created the session directory");
            let _ = std::fs::remove_dir_all(socket_dir);
        }
    }

    /// The accepted internal-endpoint deployment used a 51-byte socket directory
    /// with the 36-char session layout: encoded 109, encoder-control 117. The
    /// bounded media id must keep both under `SUN_LEN` at that exact prefix.
    #[test]
    fn endpoint_media_socket_paths_fit_the_internal_deployment_prefix() {
        let socket_dir = PathBuf::from("/private/tmp/abnocode-f13310de/rt/endpoint-17fcd5cd");
        let session_id = "603d917e-965a-4a60-bb4a-834466dc5df8";
        assert_eq!(socket_dir.to_string_lossy().len(), 51);
        assert_eq!(
            format!("{}/session-{session_id}/{ENCODED_SOCKET_NAME}", socket_dir.display()).len(),
            109,
            "fixture prefix changed; refresh the accepted evidence numbers"
        );
        assert_eq!(
            format!("{}/session-{session_id}/{CONTROL_SOCKET_NAME}", socket_dir.display()).len(),
            117,
            "fixture prefix changed; refresh the accepted evidence numbers"
        );

        let media_name = media_dir_name(session_id).unwrap();
        ensure_media_socket_paths_fit(&socket_dir, &media_name).unwrap();
        assert!(media_name.len() <= 14, "media dir name must stay bounded: {media_name}");
    }

    /// A truncated fingerprint must not silently share a directory, and the
    /// layout budget check must run before any path is created.
    #[test]
    fn endpoint_media_dir_requires_a_unique_fingerprint() {
        let root = PathBuf::from("/tmp").join(format!("ob-media-{uuid}", uuid = uuid::Uuid::new_v4()));
        let media_dir = root.join(media_dir_name("603d917e-965a-4a60-bb4a-834466dc5df8").unwrap());
        std::fs::create_dir_all(&media_dir).unwrap();
        let error = create_media_dir(&media_dir).unwrap_err().to_string();
        assert!(error.contains("fingerprint collision"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fixed canonical identity for the registry-lock tests. The endpoint now
    /// validates this shape before binding its owned media layout.
    fn canonical_id(tag: u8) -> String {
        let mut bytes = [0x60, 0x3d, 0x91, 0x7e, 0x96, 0x5a, 0x4a, 0x60,
                         0xbb, 0x4a, 0x83, 0x44, 0x66, 0xdc, 0x5d, 0x00];
        bytes[15] = tag;
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    fn insert_entry(registry: &mut SessionMediaRegistry, durable: Arc<SessionDurable>) {
        let (_encoded_tx, encoded_rx) = watch::channel(None);
        registry.entries.insert(durable.session_id.clone(), SessionMediaEntry {
            encoded_rx,
            durable,
            references: 1,
            finalizing: false,
            completion: None,
        });
    }

    /// B5: the bound slot survives the cancellable connection future, so a
    /// cancelled connection (the one-hour timeout or task cancellation) still
    /// drains its exact-session reference through the surviving guard owner.
    #[tokio::test]
    async fn cancelled_connection_drains_surviving_bound_slot() {
        let root = test_root("cancel");
        let supervisor = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        });
        let durable = durable(&root, &canonical_id(0x10), supervisor);
        let registry = Arc::new(AsyncMutex::new(SessionMediaRegistry::default()));
        insert_entry(&mut *registry.lock().await, Arc::clone(&durable));
        let guard = SessionBindingGuard::new(Arc::clone(&registry), Arc::new(Mutex::new(Some(Arc::clone(&durable)))));
        // The connection future owns the guard and pends while the exact media
        // binding is live. Cancelling the task drops the future without running
        // any explicit drain, so only the surviving guard owner can release the
        // reference. The old inline drain was dropped with the future.
        let connection = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        connection.abort();
        let _ = connection.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if registry.lock().await.entries.get(&canonical_id(0x10)).is_none() { break; }
            assert!(tokio::time::Instant::now() < deadline, "cancelled connection left its exact-session reference");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// F1: a second release of one session while the first release is still
    /// finalizing must not hold the registry mutex across the completion await.
    /// The first release owns the completion sender and its supervisor is gated,
    /// so the entry is neither removed nor published. The overlapped release
    /// therefore parks on that completion, and the registry mutex must be free
    /// for the finalizer to publish. A release that keeps the guard live inside
    /// the await waits forever while still holding the mutex, so the probe lock
    /// below is never acquired.
    #[tokio::test]
    async fn release_overlapping_in_progress_finalization_awaits_outside_the_registry_lock() {
        let root = test_root("overlap");
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let supervisor = tokio::spawn(async move {
            let _ = gate_rx.await;
            Ok(())
        });
        let durable = durable(&root, &canonical_id(0x11), supervisor);
        let registry = Arc::new(AsyncMutex::new(SessionMediaRegistry::default()));
        insert_entry(&mut *registry.lock().await, Arc::clone(&durable));

        let finalizer_registry = Arc::clone(&registry);
        let finalizer_durable = Arc::clone(&durable);
        let finalizer = tokio::spawn(async move {
            release_exact_session(&finalizer_registry, &finalizer_durable).await
        });
        // Wait until the first release has marked the entry finalizing. From
        // here the second release necessarily takes the already-finalizing
        // branch and awaits this same completion receiver.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let finalizing = registry.lock().await.entries.get(&canonical_id(0x11)).is_some_and(|entry| entry.finalizing);
            if finalizing { break; }
            assert!(tokio::time::Instant::now() < deadline, "first release never marked the entry finalizing");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let waiter_registry = Arc::clone(&registry);
        let waiter_durable = Arc::clone(&durable);
        let waiter = tokio::spawn(async move {
            release_exact_session(&waiter_registry, &waiter_durable).await
        });
        // Let the overlapped release reach the completion await. Its supervisor
        // is gated, so the finalizer has neither removed the entry nor published
        // a terminal. The registry mutex must be free for that finalizer.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let guard = tokio::time::timeout(Duration::from_millis(250), registry.lock()).await
            .expect("F1: the release completion was awaited while the registry lock was held");
        assert!(guard.entries.get(&canonical_id(0x11)).is_some_and(|entry| entry.finalizing),
            "F1: the overlapped entry was no longer finalizing");
        drop(guard);

        // Release the supervisor: the finalizer removes the entry and publishes
        // the terminal, and the overlapped release must observe it.
        let _ = gate_tx.send(());
        let terminal = tokio::time::timeout(Duration::from_secs(5), waiter).await
            .expect("F1: the overlapping release hung on the completion await")
            .expect("overlapping release task panicked");
        assert!(terminal.is_ok(), "overlapping release reported an error");
        let terminal = finalizer.await.expect("first release task panicked");
        assert!(terminal.is_ok(), "first release reported an error");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while registry.lock().await.entries.get(&canonical_id(0x11)).is_some() {
            assert!(tokio::time::Instant::now() < deadline, "overlapped session was not removed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// B6: the shared registry lock is not held while a session supervisor is
    /// awaited. A different registry operation must remain available.
    #[tokio::test]
    async fn session_release_does_not_hold_registry_lock_across_supervisor_wait() {
        let root = test_root("lock");
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let supervisor = tokio::spawn(async move {
            let _ = release_rx.await;
            Ok(())
        });
        let durable = durable(&root, &canonical_id(0x12), supervisor);
        let registry = Arc::new(AsyncMutex::new(SessionMediaRegistry::default()));
        insert_entry(&mut *registry.lock().await, Arc::clone(&durable));
        let release_registry = Arc::clone(&registry);
        let release_durable = Arc::clone(&durable);
        let release = tokio::spawn(async move { release_exact_session(&release_registry, &release_durable).await; });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let guard = tokio::time::timeout(Duration::from_millis(200), registry.lock()).await
                .expect("registry lock was held across the supervisor await");
            if guard.entries.get(&canonical_id(0x12)).is_some_and(|entry| entry.finalizing) { break; }
            drop(guard);
            assert!(tokio::time::Instant::now() < deadline, "session release never marked the entry finalizing");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = release_tx.send(());
        let _ = release.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while registry.lock().await.entries.get(&canonical_id(0x12)).is_some() {
            assert!(tokio::time::Instant::now() < deadline, "finalized session was not removed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// B7: a failed adapter spawn removes the directory and socket paths it
    /// created, and a later acquisition is not poisoned by that attempt.
    #[tokio::test]
    async fn failed_adapter_spawn_rolls_back_session_resources() {
        let root = test_root("rollback");
        let socket_dir = root.join("endpoint");
        let host_dir = root.join("host");
        std::fs::create_dir_all(&socket_dir).unwrap();
        std::fs::create_dir_all(&host_dir).unwrap();
        let registry = Arc::new(AsyncMutex::new(SessionMediaRegistry::default()));
        let missing = root.join("missing-media");
        let failed = registry.lock().await.acquire(&socket_dir, &host_dir, &missing, Path::new("/usr/bin/true"), &canonical_id(0x13));
        assert!(failed.is_err(), "missing media binary must fail acquisition");
        let session_dir = socket_dir.join(media_dir_name(&canonical_id(0x13)).unwrap());
        assert!(!session_dir.exists(), "failed acquisition left its session directory");

        let media = root.join("media.sh");
        std::fs::write(&media, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&media, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, durable) = registry.lock().await
            .acquire(&socket_dir, &host_dir, &media, Path::new("/usr/bin/true"), &canonical_id(0x13))
            .expect("later acquisition must not be poisoned by the failed attempt");
        release_exact_session(&registry, &durable).await;
        assert!(!session_dir.exists(), "successful release did not remove the retried session directory");
        let _ = std::fs::remove_dir_all(&root);
    }
}
