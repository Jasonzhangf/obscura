//! Private per-session encoder-control resource.
//!
//! One Unix listener, one typed register/revoke protocol and one admission
//! fence per exact Host session. The endpoint never reads a pid or pgid from a
//! file, stdout, stderr or a log: the only group identity it can signal is the
//! one the adapter bound through `Register` while admission was open.

use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use obscura_media::encoder_owner::{
    EncoderControlRequest, EncoderControlResponse, EncoderPermit, EncoderSettlement,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{oneshot, Mutex},
};

/// One endpoint admission fence for encoder acquisition. Registration is
/// accepted only while `Open`; the session finalizer flips it to `Finalizing`
/// before it signals or stops any group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderAdmission {
    Open,
    Finalizing,
}

/// A permit acquired before FFmpeg is spawned. The spawned pgid is bound into
/// the same permit by `Register`; a finalizer either sees the pgid and settles
/// that exact group, or awaits the typed refusal outcome the adapter reports.
pub struct InFlightAcquisition {
    pub registration_id: u64,
    settlement_tx: Option<oneshot::Sender<EncoderSettlement>>,
    settlement_rx: Option<oneshot::Receiver<EncoderSettlement>>,
}

/// Registration lifecycle on the endpoint side. The endpoint never signals a
/// registered process group, so there is no intermediate "signal work in
/// flight" state: a registration is either `Armed` (bound and accepted while
/// admission was open) or `Revoked` (the endpoint holds no further claim to
/// it). Group signal and reap belong to the adapter, which is the only party
/// that can `wait` the FFmpeg leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderRegistrationState {
    Armed,
    Revoked,
}

/// The answer to one typed revoke request. Every non-refused answer is final,
/// because the endpoint has no group signal work to complete first.
pub enum EncoderRevoke {
    Acknowledged,
    Refused,
}

/// The branch one closing control connection's retirement barrier observed.
///
/// The barrier applies the permit rule independently of this value: an
/// outstanding permit that never reached `register` is retained in every
/// branch, and the barrier never synthesizes a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderRetirement {
    /// No outstanding permit and no valid registration: no authority to
    /// retire, and no settlement may be synthesized from this close edge.
    NoAuthority,
    /// An `Armed` registration was moved to `Revoked` under the same control
    /// lock, revoking every later claim to group-signal authority.
    RevokedArmed,
    /// The registration was already `Revoked`; no further signal is possible.
    AlreadyRevoked,
    /// No registration, but an outstanding permit never reached `register`.
    /// The permit keeps its settlement receiver and registration id, so the
    /// existing finalizer still observes the unknown settlement instead of a
    /// synthesized success.
    PermitRetained,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderRegistration {
    pub registration_id: u64,
    pub pgid: i32,
    pub state: EncoderRegistrationState,
}

/// The one typed encoder authority for an exact session. All transitions run
/// under the per-session control lock; the lock is never held across an await
/// or a process wait.
#[derive(Default)]
pub struct EncoderControlState {
    pub admission: EncoderAdmission,
    pub in_flight: Option<InFlightAcquisition>,
    pub registration: Option<EncoderRegistration>,
}

impl Default for EncoderAdmission {
    fn default() -> Self {
        Self::Open
    }
}

impl EncoderControlState {
    /// Grant one permit while admission is `Open`. `Register` resolves the
    /// stored settlement with the bound pgid, and `Settle` resolves it with the
    /// refusal outcome. The finalizer takes the receiver with
    /// `take_settlement_receiver`.
    pub fn acquire(&mut self, registration_id: u64) -> EncoderPermit {
        let (settlement_tx, settlement_rx) = oneshot::channel();
        if self.admission != EncoderAdmission::Open {
            return EncoderPermit::AdmissionFinalized;
        }
        self.in_flight = Some(InFlightAcquisition {
            registration_id,
            settlement_tx: Some(settlement_tx),
            settlement_rx: Some(settlement_rx),
        });
        EncoderPermit::Granted
    }

    /// Bind the spawned pgid into the outstanding permit. Returns `true` when
    /// the registration is refused (admission already finalized) and the
    /// endpoint must not store or signal the pgid.
    pub fn register(&mut self, registration_id: u64, pgid: i32) -> bool {
        let in_flight = self.in_flight.as_ref().is_some_and(|permit| permit.registration_id == registration_id);
        if self.admission != EncoderAdmission::Open || !in_flight {
            // Refused: leave no `Armed` state and no signal authority.
            return true;
        }
        self.registration = Some(EncoderRegistration { registration_id, pgid, state: EncoderRegistrationState::Armed });
        if let Some(mut in_flight) = self.in_flight.take() {
            if let Some(sender) = in_flight.settlement_tx.take() {
                let _ = sender.send(EncoderSettlement::Armed { pgid });
            }
        }
        false
    }

    /// Adapter-side settle for a refused permit. Only the matching in-flight
    /// registration is accepted; the endpoint signals nothing.
    pub fn settle(&mut self, registration_id: u64, outcome: EncoderSettlement) {
        if self.in_flight.as_ref().is_some_and(|permit| permit.registration_id == registration_id) {
            if let Some(mut in_flight) = self.in_flight.take() {
                if let Some(sender) = in_flight.settlement_tx.take() {
                    let _ = sender.send(outcome);
                }
            }
        }
    }

    /// Take the settlement receiver for the finalizer to await. It returns the
    /// in-flight `registration_id` alongside the receiver so the finalizer can
    /// report the exact unsettled permit.
    pub fn take_settlement_receiver(&mut self) -> Option<(u64, oneshot::Receiver<EncoderSettlement>)> {
        let in_flight = self.in_flight.as_mut()?;
        let receiver = in_flight.settlement_rx.take()?;
        Some((in_flight.registration_id, receiver))
    }

    /// Revoke one registration. An `Armed` group is acknowledged immediately
    /// and the registration becomes `Revoked`, so no later caller can claim
    /// signal authority over it. The endpoint never signals the group itself.
    /// Unknown and mismatched ids are refused.
    pub fn revoke(&mut self, registration_id: u64) -> EncoderRevoke {
        let Some(registration) = self.registration.as_mut() else { return EncoderRevoke::Refused; };
        if registration.registration_id != registration_id {
            return EncoderRevoke::Refused;
        }
        registration.state = EncoderRegistrationState::Revoked;
        EncoderRevoke::Acknowledged
    }

    /// Retire the registration's signal authority without addressing the group.
    /// This is the endpoint's only close-edge authority transition: the adapter
    /// owns group signal and reap, so the endpoint has no signal work to finish
    /// first and no bounded group wait to complete.
    pub fn revoke_registration_authority(&mut self) {
        if let Some(registration) = self.registration.as_mut() {
            registration.state = EncoderRegistrationState::Revoked;
        }
    }

    /// First step of the retirement barrier for one control connection that is
    /// closing: read the real state under the control lock and take the only
    /// action that state authorizes. It never signals a group, never clears the
    /// registration, and never resolves a permit: a closed transport is not
    /// evidence that no child exists.
    pub fn begin_retirement(&mut self) -> EncoderRetirement {
        if let Some(registration) = self.registration.as_mut() {
            return match registration.state {
                // The endpoint holds no group signal authority, so revoking is
                // the whole transition: the group is adapter-owned from this
                // close edge on. The registration record keeps its true state.
                EncoderRegistrationState::Armed => {
                    registration.state = EncoderRegistrationState::Revoked;
                    EncoderRetirement::RevokedArmed
                }
                EncoderRegistrationState::Revoked => EncoderRetirement::AlreadyRevoked,
            };
        }
        // `registration == None` is not proof of no child. An outstanding
        // permit that never reached `register` keeps its settlement receiver
        // and registration id, so the existing finalizer still reaches its
        // unknown-settlement path instead of consuming a fabricated
        // `RefusedNoChild` success.
        if self.in_flight.is_some() {
            EncoderRetirement::PermitRetained
        } else {
            EncoderRetirement::NoAuthority
        }
    }

    /// Flip admission to `Finalizing` and return the exact armed pgid if one is
    /// bound. The finalizer calls this before it settles or stops anything. The
    /// pgid is used only to identify the permit; the endpoint never signals it.
    pub fn close_admission(&mut self) -> Option<i32> {
        self.admission = EncoderAdmission::Finalizing;
        self.registration.as_ref().map(|registration| registration.pgid)
    }
}

/// State-sensitive retirement barrier for one encoder-control connection that
/// is closing. It runs after the request loop for every ordinary await return
/// (owner stop, EOF, malformed frame, read failure, stalled consumer) and
/// before the stream is dropped.
///
/// It retires exactly the authority the real state authorizes: an `Armed`
/// registration loses its future group-signal authority, and an outstanding
/// permit that never registered stays untouched for the finalizer's
/// unknown-settlement path. It never signals a group, never clears the
/// registration, and never fabricates a settlement. There is no wait: the
/// endpoint has no group signal work whose completion a close edge must await.
pub async fn retire_connection_authority(state: &Arc<Mutex<EncoderControlState>>) -> EncoderRetirement {
    state.lock().await.begin_retirement()
}

pub struct EncoderControl {
    pub path: PathBuf,
    listener: Option<UnixListener>,
    pub state: Arc<Mutex<EncoderControlState>>,
    stop: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl EncoderControl {
    pub fn bind(dir: &Path, session_id: &str) -> Result<Self> {
        let dir = dir.join(super::server::media_dir_name(session_id)?);
        // The session media registry owns creation of the 0700 per-session
        // directory before the encoded socket bind; tolerate it being present.
        if !dir.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&dir).context("Create session media directory")?;
        }
        let path = dir.join("encoder-control.sock");
        anyhow::ensure!(path.to_string_lossy().len() < 104, "Encoder-control socket path exceeds macOS bound");
        let listener = UnixListener::bind(&path).context("Bind per-session encoder control")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path,
            listener: Some(listener),
            state: Arc::new(Mutex::new(EncoderControlState::default())),
            stop: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Install the reader stop handle. The finalizer owns this handle and
    /// signals the healthy reader only after adapter reap.
    pub fn set_reader_stop(&self, stop: tokio::sync::oneshot::Sender<()>) {
        if let Ok(mut slot) = self.stop.lock() { *slot = Some(stop); }
    }

    /// Stop the reader. Idempotent: a second call is a no-op.
    pub fn stop_reader(&self) {
        if let Ok(mut slot) = self.stop.lock() {
            if let Some(stop) = slot.take() { let _ = stop.send(()); }
        }
    }
}

/// The per-session encoder-control reader. It owns one accepted adapter
/// connection and answers typed requests until the owner stops it. Its
/// `JoinHandle` has exactly one owner: the session supervisor.
pub async fn run_reader(listener: UnixListener, state: Arc<Mutex<EncoderControlState>>, mut stop: oneshot::Receiver<()>) -> Result<()> {
    let accepted = tokio::select! {
        biased;
        _ = &mut stop => return Ok(()),
        stream = listener.accept() => stream,
    };
    let (stream, _) = accepted.context("Accept encoder control")?;
    serve_connection(stream, state, &mut stop).await
}

async fn serve_connection(mut stream: UnixStream, state: Arc<Mutex<EncoderControlState>>, stop: &mut oneshot::Receiver<()>) -> Result<()> {
    let (read, mut write) = stream.split();
    let result = serve_requests(&mut BufReader::new(read), &mut write, &state, stop).await;
    // Retirement barrier: runs after the request loop ends for every ordinary
    // await return, while the stream is still open. It retires this
    // connection's authority according to the real state it finds, and the
    // original `serve_requests` result is returned unchanged below.
    retire_connection_authority(&state).await;
    result
}

async fn serve_requests<R, W>(reader: &mut BufReader<R>, write: &mut W, state: &Arc<Mutex<EncoderControlState>>, stop: &mut oneshot::Receiver<()>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let mut line = String::new();
        let read = tokio::select! {
            biased;
            _ = &mut *stop => return Ok(()),
            read = reader.read_line(&mut line) => read,
        };
        let count = read.context("Read encoder control")?;
        if count == 0 {
            return Ok(());
        }
        anyhow::ensure!(line.ends_with('\n') && line.len() <= 4096, "Invalid bounded encoder control request");
        let request: EncoderControlRequest = match serde_json::from_str(line.trim_end()) {
            Ok(request) => request,
            Err(_) => bail!("ENCODER_CONTROL_MALFORMED: invalid typed encoder-control frame"),
        };
        let response = match request {
            EncoderControlRequest::Acquire { registration_id } => {
                let permit = {
                    let mut guard = state.lock().await;
                    guard.acquire(registration_id)
                };
                EncoderControlResponse::Acquire { permit }
            }
            EncoderControlRequest::Register { registration_id, pgid } => {
                let refused = {
                    let mut guard = state.lock().await;
                    guard.register(registration_id, pgid)
                };
                EncoderControlResponse::Register { refused }
            }
            EncoderControlRequest::Revoke { registration_id } => {
                EncoderControlResponse::Revoke { refused: await_revoke(&state, registration_id).await }
            }
            EncoderControlRequest::Settle { registration_id, outcome } => {
                state.lock().await.settle(registration_id, outcome);
                EncoderControlResponse::Settle
            }
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(2), write.write_all(&bytes)).await.context("Encoder control consumer stalled")??;
    }
}

async fn await_revoke(state: &Arc<Mutex<EncoderControlState>>, registration_id: u64) -> bool {
    // The endpoint owns no group signal, so a revoke is decided in one lock
    // acquisition: acknowledged for the exact bound registration, refused
    // otherwise. There is no signal work to wait for.
    match state.lock().await.revoke(registration_id) {
        EncoderRevoke::Acknowledged => false,
        EncoderRevoke::Refused => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exchange(
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        request: EncoderControlRequest,
    ) -> EncoderControlResponse {
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    /// B4: the endpoint never signals the registered group, so a revoke is
    /// acknowledged from the bound registration alone and must not depend on
    /// any endpoint-side group signal completing.
    #[tokio::test]
    async fn revoke_is_acknowledged_without_endpoint_group_signal() {
        let root = std::path::Path::new("/tmp").join(format!("ec-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        let (stop_tx, stop_rx) = oneshot::channel();
        let reader_task = tokio::spawn(run_reader(listener, Arc::clone(&state), stop_rx));
        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);

        let acquired = exchange(&mut read, &mut write, EncoderControlRequest::Acquire { registration_id: 1 }).await;
        assert!(matches!(acquired, EncoderControlResponse::Acquire { permit: EncoderPermit::Granted }));
        let registered = exchange(&mut read, &mut write, EncoderControlRequest::Register { registration_id: 1, pgid: 42 }).await;
        assert!(matches!(registered, EncoderControlResponse::Register { refused: false }));

        // The revoke must be answered on the bound registration alone: the
        // adapter owns the group, so the endpoint has no signal work to do and
        // must not leave the caller waiting for any.
        let revoked = tokio::time::timeout(
            Duration::from_secs(1),
            exchange(&mut read, &mut write, EncoderControlRequest::Revoke { registration_id: 1 }),
        )
        .await
        .expect("revoke was not acknowledged: the endpoint held group signal authority it does not own");
        assert!(matches!(revoked, EncoderControlResponse::Revoke { refused: false }));

        // A second revoke of the same registration is still acknowledged, and
        // the registration records no lingering signal authority.
        let again = exchange(&mut read, &mut write, EncoderControlRequest::Revoke { registration_id: 1 }).await;
        assert!(matches!(again, EncoderControlResponse::Revoke { refused: false }));
        assert_eq!(
            state.lock().await.registration.map(|registration| registration.state),
            Some(EncoderRegistrationState::Revoked),
        );

        let _ = stop_tx.send(());
        let _ = reader_task.await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// F1 row 1: with no outstanding permit and no valid registration there is
    /// no authority to retire, so the barrier closes without inventing a
    /// settlement.
    #[tokio::test]
    async fn retirement_barrier_without_authority_invents_no_settlement() {
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        assert_eq!(retire_connection_authority(&state).await, EncoderRetirement::NoAuthority);
        let guard = state.lock().await;
        assert!(guard.registration.is_none(), "the barrier invented a registration");
        assert!(guard.in_flight.is_none(), "the barrier invented an in-flight permit");
    }

    /// F1 row 2: an `Armed` registration is revoked inside the same control
    /// lock, so no later claim to group-signal authority can be made and the
    /// barrier needs no wait.
    #[tokio::test]
    async fn retirement_barrier_revokes_armed_without_signal_authority() {
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        assert!(matches!(state.lock().await.acquire(1), EncoderPermit::Granted));
        assert!(!state.lock().await.register(1, 42));
        assert_eq!(retire_connection_authority(&state).await, EncoderRetirement::RevokedArmed);
        let mut guard = state.lock().await;
        assert_eq!(guard.registration.map(|registration| registration.state), Some(EncoderRegistrationState::Revoked));
        // A later revoke is a no-op acknowledgement rather than a signal claim.
        assert!(matches!(guard.revoke(1), EncoderRevoke::Acknowledged));
    }

    /// F1 row 3: an already `Revoked` registration is closed directly and
    /// without any further signal.
    #[tokio::test]
    async fn retirement_barrier_completes_without_signal_when_already_revoked() {
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        assert!(matches!(state.lock().await.acquire(1), EncoderPermit::Granted));
        assert!(!state.lock().await.register(1, 42));
        assert!(matches!(state.lock().await.revoke(1), EncoderRevoke::Acknowledged));
        assert_eq!(retire_connection_authority(&state).await, EncoderRetirement::AlreadyRevoked);
    }

    /// F1 row 4: the retirement barrier is total for the close edge. Every
    /// reachable registration state maps to a branch without waiting, because
    /// the endpoint owns no group signal whose completion it must await.
    #[tokio::test]
    async fn retirement_barrier_never_waits_for_endpoint_signal_work() {
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        assert!(matches!(state.lock().await.acquire(1), EncoderPermit::Granted));
        assert!(!state.lock().await.register(1, 42));
        let branch = tokio::time::timeout(Duration::from_millis(150), retire_connection_authority(&state))
            .await
            .expect("the barrier waited for endpoint signal work the endpoint does not own");
        assert_eq!(branch, EncoderRetirement::RevokedArmed);
        assert_eq!(
            state.lock().await.registration.map(|registration| registration.state),
            Some(EncoderRegistrationState::Revoked),
        );
    }

    /// F1 row 5: an outstanding permit that never registered is retained, and
    /// the barrier fabricates no settlement for it. The adapter's own `Settle`
    /// remains the only settle edge for that permit.
    #[tokio::test]
    async fn retirement_barrier_retains_unregistered_permit_for_unknown_settlement() {
        let state = Arc::new(Mutex::new(EncoderControlState::default()));
        assert!(matches!(state.lock().await.acquire(7), EncoderPermit::Granted));
        assert_eq!(retire_connection_authority(&state).await, EncoderRetirement::PermitRetained);
        let (registration_id, mut settlement) = state
            .lock()
            .await
            .take_settlement_receiver()
            .expect("the barrier dropped the unregistered permit's settlement receiver");
        assert_eq!(registration_id, 7);
        assert!(
            matches!(settlement.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "the barrier synthesized a settlement for an unregistered permit"
        );
        state.lock().await.settle(7, EncoderSettlement::RefusedGroupStopped);
        assert!(matches!(settlement.await, Ok(EncoderSettlement::RefusedGroupStopped)));
    }
}
