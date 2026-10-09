//! Cooperative encoder-child owner for one adapter session.
//!
//! The endpoint owns per-session encoder admission on a private Unix socket.
//! This module owns one retained adapter direct child: acquire the endpoint
//! permit, spawn FFmpeg in its own process group, register that group, revoke
//! before reap, and stop/kill/reap on timeout or cancellation. No process id is
//! ever recovered from stdout, stderr, a file or a log.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::{validate_pixels, MAX_ACCESS_UNIT};

/// Private adapter-to-endpoint encoder admission wire. It is a typed control
/// resource, never video payload, metadata or a log, and it carries no pid
/// recovered from external text: `Register.pgid` comes from the child this
/// adapter spawned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EncoderControlRequest {
    Acquire { registration_id: u64 },
    Register { registration_id: u64, pgid: i32 },
    Revoke { registration_id: u64 },
    Settle { registration_id: u64, outcome: EncoderSettlement },
}

/// Typed endpoint answer for one [`EncoderControlRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EncoderControlResponse {
    Acquire { permit: EncoderPermit },
    Register { refused: bool },
    Revoke { refused: bool },
    Settle,
}

/// One encoder acquisition permit. `AdmissionFinalized` is the typed refusal
/// once the session finalizer has closed the admission fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPermit {
    Granted,
    AdmissionFinalized,
}

/// The adapter's typed settlement of one in-flight admission permit. The
/// endpoint never signals or reaps a refused group; the adapter reports the
/// local outcome through this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderSettlement {
    Armed { pgid: i32 },
    RefusedNoChild,
    RefusedGroupStopped,
}

/// Whole-encode window. It covers input write, bounded stdout/stderr reads,
/// revoke acknowledgement and the leader reap. It is the only encode timeout.
pub const ENCODE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded exchange window for the short control messages that do not wait on
/// endpoint signal authority (`Acquire`, `Register`, `Settle`).
const CONTROL_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounded window the adapter waits for the endpoint's typed revoke
/// acknowledgement. It must cover the endpoint's own signal-completion path:
/// SIGTERM grace plus SIGKILL grace (`stop_armed_encoder_group` bounds each at
/// two seconds) plus scheduling slack. A response that does not arrive inside
/// this window is an explicit ownership failure, never an implicit
/// acknowledgement.
const REVOKE_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Cooperative group stop grace before escalation to SIGKILL.
const GROUP_GRACE: Duration = Duration::from_secs(1);
/// Bounded leader reap deadline after group stop/kill.
const REAP_DEADLINE: Duration = Duration::from_secs(3);
#[derive(Debug)]
pub enum EncoderStop {
    Exited(std::process::ExitStatus),
    KilledAndReaped(std::process::ExitStatus),
}

/// The endpoint registration state of the current child. A registration is
/// `Pending` from the moment `Register` is sent until its typed acknowledgement
/// is read, so a cancellation in that window never mistakes an unread
/// acknowledgement for an unregistered group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationState {
    /// No child is bound to the endpoint for the current registration.
    Unregistered,
    /// `Register` was sent; the acknowledgement has not been read yet.
    Pending,
    /// The endpoint acknowledged `Registered`; it holds signal authority and a
    /// `RevokeEncoder` is owed before the leader is reaped.
    Armed,
    /// The endpoint confirmed a refusal; the group is adapter-owned and is
    /// stopped locally without `RevokeEncoder`/`Revoked`.
    Refused,
}

/// One adapter's retained encoder child owner. It survives cancellation of an
/// in-flight `encode_frame` so the caller can always finalize the subtree.
pub struct EncoderChildOwner {
    binary: std::path::PathBuf,
    group: Option<i32>,
    child: Option<tokio::process::Child>,
    control: EncoderControlClient,
    registration_id: u64,
    registration: RegistrationState,
}

impl EncoderChildOwner {
    /// Open the session-scoped owner. It connects once and spawns no FFmpeg.
    pub async fn open(binary: &Path, control_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(control_path).await.context("Connect Host encoder control")?;
        let (read, write) = stream.into_split();
        Ok(Self {
            binary: binary.to_path_buf(),
            group: None,
            child: None,
            control: EncoderControlClient { reader: BufReader::new(read), writer: write },
            registration_id: 0,
            registration: RegistrationState::Unregistered,
        })
    }

    /// One access unit. Borrows `self`; the child stays with the owner so a
    /// timeout or cancellation can still finalize it.
    pub async fn encode_frame(&mut self, width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>> {
        self.registration_id = self.registration_id.checked_add(1).context("Encoder registration exhausted")?;
        let registration_id = self.registration_id;
        match self.control.acquire(registration_id).await? {
            EncoderPermit::AdmissionFinalized => {
                self.control.settle(registration_id, EncoderSettlement::RefusedNoChild).await
                    .context("Report refused encoder acquisition")?;
                bail!("ENCODER_ADMISSION_FINALIZED: Host session finalizer refused the encoder permit");
            }
            EncoderPermit::Granted => {}
        }
        let mut rgba = rgba;
        validate_pixels(width, height, rgba.len() as u64)?;
        for pixel in rgba.chunks_exact_mut(4) {
            let background = 255 - pixel[3];
            for channel in &mut pixel[..3] {
                *channel = channel.saturating_add(background);
            }
            pixel[3] = 255;
        }
        let child = tokio::process::Command::new(&self.binary)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "rawvideo", "-pixel_format", "rgba", "-video_size"])
            .arg(format!("{width}x{height}"))
            .args(["-framerate", "3", "-i", "pipe:0", "-frames:v", "1", "-an", "-vf",
                "pad=ceil(iw/2)*2:ceil(ih/2)*2:color=white,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
                "-c:v", "libx264", "-threads", "1", "-preset", "ultrafast", "-tune", "zerolatency",
                "-profile:v", "baseline", "-crf", "23", "-g", "1", "-bf", "0",
                "-x264-params", "repeat-headers=1:aud=1", "-colorspace", "bt709",
                "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv", "-f", "h264", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .context("Start configured FFmpeg H.264 encoder")?;
        let pid = child.id().context("Encoder child identity unavailable")?;
        let pgid = i32::try_from(pid).context("Encoder process group identity exhausted")?;
        ensure!(pgid > 0, "Encoder process group identity must be positive");
        self.group = Some(pgid);
        self.child = Some(child);
        // From here the `Register` request may reach the endpoint before the
        // acknowledgement is read. Record that pending state first so a
        // cancellation cannot mistake the unread acknowledgement for an
        // unregistered group.
        self.registration = RegistrationState::Pending;
        match self.control.register(registration_id, pgid).await {
            ControlExchange::Answered(EncoderControlResponse::Register { refused: false }) => {
                self.registration = RegistrationState::Armed;
            }
            ControlExchange::Answered(EncoderControlResponse::Register { refused: true }) => {
                self.registration = RegistrationState::Refused;
                self.finalize_local_refused().await.context("Local refused encoder cleanup")?;
                self.control.settle(registration_id, EncoderSettlement::RefusedGroupStopped).await
                    .context("Report refused encoder settlement")?;
                bail!("ENCODER_ADMISSION_FINALIZED: Host session finalizer refused encoder registration");
            }
            ControlExchange::Answered(other) => {
                // A response that is neither `Register` outcome is a protocol
                // violation. The endpoint state is unknown, so never reap
                // blind; surface the ownership failure.
                bail!("ENCODER_REGISTRATION_UNRESOLVED: unexpected encoder control response: {other:?}");
            }
            ControlExchange::Retired => {
                // The endpoint closed the control connection before it answered,
                // so it cannot hold signal authority over this group. The group
                // is adapter-owned: settle it locally before surfacing the
                // failure.
                self.registration = RegistrationState::Unregistered;
                self.stop_kill_reap().await.context("Local encoder cleanup after control retirement")?;
                bail!("ENCODER_CONTROL_RETIRED: endpoint closed the encoder control connection before the registration acknowledgement");
            }
            ControlExchange::Unresolved(error) => {
                // The typed acknowledgement was not received and the connection
                // was not observed closed. Endpoint signal authority may still
                // exist, so the group is never reaped blind and the child stays
                // pinned for the caller's ownership-preserving boundary.
                bail!("ENCODER_REGISTRATION_UNRESOLVED: registration {registration_id} unresolved: {error:#}");
            }
        }
        let outcome = tokio::time::timeout(ENCODE_TIMEOUT, self.run_encode(registration_id, rgba)).await
            .context("H.264 encoder exceeded five second deadline");
        match outcome {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => {
                self.finalize_after_terminal_error(error, registration_id).await
            }
            Err(error) => {
                self.finalize_after_terminal_error(error, registration_id).await
            }
        }
    }

    /// A terminal encode error still owns the retained subtree. `run_encode`
    /// settles the group through the same owner on every path that reaches its
    /// reap, so this only finalizes when a live child is still retained (the
    /// encode future was cancelled or dropped before that settle).
    async fn finalize_after_terminal_error(&mut self, error: anyhow::Error, registration_id: u64) -> Result<Vec<u8>> {
        if !self.has_live_child() {
            return Err(error);
        }
        match self.finalize().await {
            Ok(_) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "encoder group cleanup failed for registration {registration_id}: {cleanup}"
            ))),
        }
    }

    async fn run_encode(&mut self, registration_id: u64, rgba: Vec<u8>) -> Result<Vec<u8>> {
        // Input write and the bounded pipe reads complete before any reap. The
        // FFmpeg leader must not be reaped before the Revoke acknowledgement.
        let (_written, bytes, errors) = {
            let child = self.child.as_mut().context("Encoder child missing")?;
            let mut input = child.stdin.take().context("Encoder input missing")?;
            let output = child.stdout.take().context("Encoder output missing")?;
            let diagnostic = child.stderr.take().context("Encoder diagnostics missing")?;
            tokio::try_join!(
                async { input.write_all(&rgba).await?; drop(input); Ok::<_, anyhow::Error>(()) },
                bounded_read(output, MAX_ACCESS_UNIT),
                bounded_read(diagnostic, 64 * 1024),
            )?
        };
        match self.control.revoke(registration_id).await {
            ControlExchange::Answered(EncoderControlResponse::Revoke { refused: false }) => {}
            ControlExchange::Answered(EncoderControlResponse::Revoke { refused: true }) => {
                bail!("Encoder group was refused before revoke acknowledgement");
            }
            ControlExchange::Answered(other) => bail!("Unexpected encoder control response: {other:?}"),
            ControlExchange::Retired => bail!(
                "ENCODER_CONTROL_RETIRED: endpoint closed the encoder control connection before the revoke acknowledgement"
            ),
            ControlExchange::Unresolved(error) => {
                return Err(error.context("Encoder revoke exchange failed"));
            }
        }
        // Settle the group through the same retained owner while the leader
        // identity is still pinned. `stop_kill_reap` signals the group before
        // it reaps the leader and retires signal authority when the leader is
        // reaped, so no group signal can address a reused numeric pgid after
        // `child.wait()` has completed. It returns the leader status.
        let status = match self.stop_kill_reap().await? {
            EncoderStop::Exited(status) | EncoderStop::KilledAndReaped(status) => status,
        };
        ensure!(status.success(), "H.264 encoder failed: {}", String::from_utf8_lossy(&errors));
        ensure!(!bytes.is_empty(), "H.264 encoder produced no access unit");
        Ok(bytes)
    }

    /// True while a spawned, not-yet-reaped encoder child is retained. The
    /// adapter signal branch uses it to skip finalization between frames.
    pub fn has_live_child(&self) -> bool {
        self.child.is_some()
    }
    /// Cooperative group stop/kill/reap for a registered group. It revokes
    /// before reaping the leader so the endpoint signal cannot address a
    /// reused process id.
    ///
    /// This is one typed finalize attempt. `Ok` means the group was settled and
    /// the leader reaped. `Err` means endpoint authority may still be held; the
    /// direct child stays pinned and the caller must wait for the
    /// authority-retirement edge before retrying. A closed or broken control
    /// connection is that edge, so it is classified as retired and the group is
    /// settled locally instead of being reported as unresolved.
    pub async fn finalize(&mut self) -> Result<EncoderStop> {
        match self.registration {
            // An endpoint-acknowledged registration owes a `RevokeEncoder` and
            // a successful typed acknowledgement before the leader is reaped.
            RegistrationState::Armed => self.finalize_armed().await,
            // The `Register` request was sent but its acknowledgement was not
            // read. Resolve it before deciding: an acknowledged registration
            // still owes a revoke; a confirmed refusal runs the adapter-owned
            // local cleanup; a closed connection retires endpoint authority.
            // An unresolved acknowledgement is an explicit ownership failure
            // and the group is never reaped blind.
            RegistrationState::Pending => {
                match self.control.read_response(REVOKE_ACK_TIMEOUT).await {
                    ControlExchange::Answered(EncoderControlResponse::Register { refused: false }) => {
                        self.registration = RegistrationState::Armed;
                        self.finalize_armed().await
                    }
                    ControlExchange::Answered(EncoderControlResponse::Register { refused: true }) => {
                        self.registration = RegistrationState::Refused;
                        self.stop_kill_reap().await
                    }
                    ControlExchange::Answered(other) => {
                        bail!("Unexpected encoder control response while resolving registration: {other:?}")
                    }
                    // The endpoint closed the control connection, so it can no
                    // longer hold signal authority over this group. The group is
                    // adapter-owned and settled locally.
                    ControlExchange::Retired => {
                        self.registration = RegistrationState::Unregistered;
                        self.stop_kill_reap().await
                    }
                    ControlExchange::Unresolved(error) => {
                        bail!("ENCODER_REGISTRATION_UNRESOLVED: {error:#}")
                    }
                }
            }
            // A confirmed refusal (or a group with no endpoint registration)
            // is adapter-owned and is stopped locally.
            RegistrationState::Refused | RegistrationState::Unregistered => self.stop_kill_reap().await,
        }
    }

    /// The `Armed` half of one typed finalize attempt. It sends `RevokeEncoder`
    /// and requires the typed non-refused acknowledgement before the group is
    /// addressed. A closed control connection is the authority-retirement edge,
    /// so the group is settled locally; every other failure leaves endpoint
    /// authority unresolved and keeps the child pinned.
    async fn finalize_armed(&mut self) -> Result<EncoderStop> {
        let registration_id = self.registration_id;
        match self.control.revoke(registration_id).await {
            ControlExchange::Answered(EncoderControlResponse::Revoke { refused: false }) => self.stop_kill_reap().await,
            ControlExchange::Answered(EncoderControlResponse::Revoke { refused: true }) => {
                bail!("ENCODER_REVOKE_REFUSED: endpoint refused revoke for registration {registration_id}")
            }
            ControlExchange::Answered(other) => bail!("Unexpected encoder control response: {other:?}"),
            // The endpoint closed the control connection: it can no longer hold
            // signal authority over this registration, so the group is
            // adapter-owned and settled locally.
            ControlExchange::Retired => {
                self.registration = RegistrationState::Unregistered;
                self.stop_kill_reap().await
            }
            ControlExchange::Unresolved(error) => Err(error.context("Revoke encoder registration")),
        }
    }

    /// Refused-permit local cleanup. The endpoint never stored this group, so
    /// it is stopped, killed and reaped without `RevokeEncoder`/`Revoked`.
    pub async fn finalize_local_refused(&mut self) -> Result<EncoderStop> {
        self.stop_kill_reap().await
    }

    async fn stop_kill_reap(&mut self) -> Result<EncoderStop> {
        let group = self.group.context("Encoder group identity missing")?;
        let mut escalated = false;
        // Signal only a group that still holds a live member. A group whose only
        // remaining member is this unreaped zombie leader cannot be signalled at
        // all: `kill(-pgid, SIGTERM)` fails with EPERM. Skipping the signal there
        // is not suppression; there is nothing left that could receive it.
        if group_has_live_member(group) {
            signal_group(group, libc::SIGTERM);
            // Decide on escalation from the group's own existence, never from a
            // reaped leader. The leader stays unreaped here, so its numeric pgid
            // still names exactly this group when a later SIGKILL is chosen.
            // Reaping first would release that identity for reuse and let a
            // stale pgid address an unrelated group.
            if !wait_group_gone(group, GROUP_GRACE).await {
                escalated = true;
                // The leader is still unreaped, so the pgid is still pinned to
                // this process group.
                signal_group(group, libc::SIGKILL);
                if !wait_group_gone(group, REAP_DEADLINE).await {
                    bail!("Encoder process group survived SIGKILL");
                }
            }
        }
        // Every signal decision is complete. Retire child/group authority at
        // the one actual reap boundary, so no further caller can signal a
        // reaped, reusable identity.
        let status = self.reaped_status().await?;
        if escalated {
            Ok(EncoderStop::KilledAndReaped(status))
        } else {
            Ok(EncoderStop::Exited(status))
        }
    }

    /// The one ownership-preserving terminal boundary every adapter exit path
    /// uses. It makes exactly one typed `finalize` attempt. If that settles the
    /// group, ownership is released and the leader is reaped. Otherwise endpoint
    /// authority may still be held, so the direct child stays pinned and this
    /// waits, with no elapsed-retention deadline, for the authority-retirement
    /// edge: the control connection closing (`EOF` or closed-connection
    /// `EPIPE`). Only after that edge is observed is the group settled locally
    /// and the leader reaped.
    pub async fn settle_until_retired(&mut self) -> Result<EncoderStop> {
        ensure!(self.has_live_child(), "No encoder child is retained");
        if let Ok(stop) = self.finalize().await {
            return Ok(stop);
        }
        self.await_authority_retirement().await?;
        self.registration = RegistrationState::Unregistered;
        self.stop_kill_reap().await
    }

    /// Wait, without an elapsed-retention deadline, for the endpoint's control
    /// connection to close. `EOF` or a closed-connection `EPIPE` is the typed
    /// authority-retirement edge: the endpoint closes this connection only after
    /// it has retired signal authority for the session, and an endpoint process
    /// exit closes it too. Late bytes are discarded; only the close is the edge.
    /// The retained child is untouched for the whole wait.
    pub async fn await_authority_retirement(&mut self) -> Result<()> {
        ensure!(self.has_live_child(), "No encoder child is retained");
        let mut scratch = [0u8; 256];
        loop {
            match self.control.reader.read(&mut scratch).await {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(error) => {
                    if is_io_peer_close(&error) { return Ok(()); }
                    // No peer-close evidence yet: authority may still be held,
                    // so keep the child pinned and retry instead of returning
                    // an error that would drop the retained owner.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Reap the already-signalled leader and retire signal authority. This is
    /// the only place the direct child is reaped, so it is the only place the
    /// retained group/child identity is cleared.
    async fn reaped_status(&mut self) -> Result<std::process::ExitStatus> {
        let status = tokio::time::timeout(REAP_DEADLINE, async {
            let child = self.child.as_mut().context("Encoder child missing")?;
            Ok::<_, anyhow::Error>(child.wait().await?)
        })
        .await
        .context("Encoder leader reap deadline exceeded")??;
        self.group = None;
        self.child = None;
        self.registration = RegistrationState::Unregistered;
        Ok(status)
    }
}

/// Signal one encoder process group by its exact bound pgid. The pgid comes
/// from the child this owner spawned, never from external text.
pub fn signal_group(pgid: i32, signal: libc::c_int) {
    // Safety: `kill` with a negative pid targets the process group. The pgid
    // comes from the child this owner spawned, never from external text.
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("encoder group signal failed: {error}");
        }
    }
}

async fn wait_group_gone(pgid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !group_has_live_member(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// True while the bound group still holds at least one live (non-zombie)
/// member. Not interchangeable with an existence probe: a group whose only
/// remaining member is an unreaped zombie leader makes `kill(-pgid, 0)` fail
/// with `EPERM`, and a zombie can never be signalled.
fn group_has_live_member(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

struct EncoderControlClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

/// Classification of one typed control exchange. It separates the two
/// authority outcomes the adapter must tell apart:
///
/// * [`ControlExchange::Retired`] means the endpoint closed the control
///   connection. It cannot hold signal authority over any group after that
///   edge, so the adapter may settle the group locally.
/// * [`ControlExchange::Unresolved`] means the exchange failed while the
///   connection may still be live (for example a timeout, a write error on a
///   half-open stream, or a malformed frame). Endpoint signal authority may
///   still exist, so the retained child must stay pinned.
#[derive(Debug)]
enum ControlExchange<T> {
    Answered(T),
    Retired,
    Unresolved(anyhow::Error),
}

/// True when an I/O error proves the peer closed its end of the control
/// connection. A write that fails with a broken pipe, or a read that returns
/// end-of-file, are both terminal peer-close evidence. Any other error keeps
/// authority unresolved.
fn is_peer_close(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset)
        })
    })
}

/// [`is_peer_close`] for a bare [`std::io::Error`].
fn is_io_peer_close(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset)
}

impl EncoderControlClient {
    async fn exchange(&mut self, request: &EncoderControlRequest, timeout: Duration) -> ControlExchange<EncoderControlResponse> {
        let mut bytes = match serde_json::to_vec(request) {
            Ok(bytes) => bytes,
            Err(error) => return ControlExchange::Unresolved(error.into()),
        };
        bytes.push(b'\n');
        match tokio::time::timeout(timeout, self.writer.write_all(&bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let error = anyhow::Error::new(error).context("Write encoder control request");
                return if is_peer_close(&error) { ControlExchange::Retired } else { ControlExchange::Unresolved(error) };
            }
            Err(_) => {
                return ControlExchange::Unresolved(
                    anyhow::anyhow!("Encoder control write deadline exceeded"),
                )
            }
        }
        self.read_response(timeout).await
    }

    /// Read one bounded typed response. It is used both for a normal exchange
    /// and to resolve an outstanding `Register` acknowledgement whose read was
    /// interrupted by cancellation.
    async fn read_response(&mut self, timeout: Duration) -> ControlExchange<EncoderControlResponse> {
        let mut line = String::new();
        let read = match tokio::time::timeout(timeout, self.reader.read_line(&mut line)).await {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                let error = anyhow::Error::new(error).context("Read encoder control response");
                return if is_peer_close(&error) { ControlExchange::Retired } else { ControlExchange::Unresolved(error) };
            }
            Err(_) => {
                return ControlExchange::Unresolved(
                    anyhow::anyhow!("Encoder control read deadline exceeded"),
                )
            }
        };
        if read == 0 {
            // EOF with no bytes is the peer closing the connection.
            return ControlExchange::Retired;
        }
        if !line.ends_with('\n') || line.len() > 4096 {
            return ControlExchange::Unresolved(anyhow::anyhow!("Invalid bounded encoder control response"));
        }
        line.pop();
        match serde_json::from_str(&line) {
            Ok(response) => ControlExchange::Answered(response),
            Err(error) => ControlExchange::Unresolved(anyhow::Error::new(error).context("Decode encoder control response")),
        }
    }

    async fn acquire(&mut self, registration_id: u64) -> Result<EncoderPermit> {
        match self.exchange(&EncoderControlRequest::Acquire { registration_id }, CONTROL_EXCHANGE_TIMEOUT).await {
            ControlExchange::Answered(EncoderControlResponse::Acquire { permit }) => Ok(permit),
            ControlExchange::Answered(other) => bail!("Unexpected encoder control response: {other:?}"),
            ControlExchange::Retired => bail!("ENCODER_CONTROL_RETIRED: endpoint closed the encoder control connection before the acquisition response"),
            ControlExchange::Unresolved(error) => Err(error.context("Encoder acquisition exchange failed")),
        }
    }

    async fn register(&mut self, registration_id: u64, pgid: i32) -> ControlExchange<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Register { registration_id, pgid }, CONTROL_EXCHANGE_TIMEOUT).await
    }

    async fn revoke(&mut self, registration_id: u64) -> ControlExchange<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Revoke { registration_id }, REVOKE_ACK_TIMEOUT).await
    }

    async fn settle(&mut self, registration_id: u64, outcome: EncoderSettlement) -> Result<()> {
        match self.exchange(&EncoderControlRequest::Settle { registration_id, outcome }, CONTROL_EXCHANGE_TIMEOUT).await {
            ControlExchange::Answered(EncoderControlResponse::Settle) => Ok(()),
            ControlExchange::Answered(other) => bail!("Unexpected encoder control response: {other:?}"),
            ControlExchange::Retired => bail!("ENCODER_CONTROL_RETIRED: endpoint closed the encoder control connection before the settlement response"),
            ControlExchange::Unresolved(error) => Err(error.context("Encoder settlement exchange failed")),
        }
    }
}

async fn bounded_read(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= limit, "Encoder output exceeds limit");
    Ok(bytes)
}
