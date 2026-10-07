//! Cooperative encoder-child owner for one adapter session.
//!
//! The endpoint owns per-session encoder admission on a private Unix socket.
//! This module owns one retained adapter direct child: acquire the endpoint
//! permit, spawn FFmpeg in its own process group, register that group, revoke
//! before reap, and stop/kill/reap on timeout or cancellation. No process id is
//! ever recovered from stdout, stderr, a file or a log.

use std::{path::Path, process::Stdio, time::{Duration, Instant}};

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
/// Bounded window that an unresolved endpoint authority may remain pinned
/// before the adapter reaches its explicit terminal outcome.
pub const RETENTION_TERMINAL_TIMEOUT: Duration = Duration::from_secs(5);

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
    retention_deadline: Option<Instant>,
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
            retention_deadline: None,
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
        let registered = self.control.register(registration_id, pgid).await;
        match registered {
            Ok(EncoderControlResponse::Register { refused: false }) => self.registration = RegistrationState::Armed,
            Ok(EncoderControlResponse::Register { refused: true }) => {
                self.registration = RegistrationState::Refused;
                self.finalize_local_refused().await.context("Local refused encoder cleanup")?;
                self.control.settle(registration_id, EncoderSettlement::RefusedGroupStopped).await
                    .context("Report refused encoder settlement")?;
                bail!("ENCODER_ADMISSION_FINALIZED: Host session finalizer refused encoder registration");
            }
            Err(error) => {
                // The typed acknowledgement was not received. Endpoint signal
                // authority may or may not exist, so the group is never reaped
                // blind: resolve the outstanding acknowledgement through the
                // same typed control resource, then revoke or refuse before any
                // local reap. An unresolved acknowledgement is surfaced as an
                // explicit ownership failure.
                return match self.finalize().await {
                    Ok(_) => Err(error.context("Encoder registration acknowledgement not received")),
                    Err(cleanup) => Err(error.context(format!(
                        "ENCODER_REGISTRATION_UNRESOLVED: registration {registration_id} unresolved: {cleanup}"
                    ))),
                };
            }
            Ok(other) => {
                // A response that is neither `Register` outcome is a protocol
                // violation. The endpoint state is unknown, so never reap
                // blind; surface the ownership failure.
                bail!("ENCODER_REGISTRATION_UNRESOLVED: unexpected encoder control response: {other:?}");
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
        match self.control.revoke(registration_id).await? {
            EncoderControlResponse::Revoke { refused: false } => {}
            EncoderControlResponse::Revoke { refused: true } => {
                bail!("Encoder group was refused before revoke acknowledgement");
            }
            other => bail!("Unexpected encoder control response: {other:?}"),
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
    pub async fn finalize(&mut self) -> Result<EncoderStop> {
        match self.registration {
            // An endpoint-acknowledged registration owes a `RevokeEncoder` and
            // a successful typed acknowledgement before the leader is reaped.
            RegistrationState::Armed => {
                self.revoke_registered().await?;
                self.stop_kill_reap().await
            }
            // The `Register` request was sent but its acknowledgement was not
            // read. Resolve it before deciding: an acknowledged registration
            // still owes a revoke; a confirmed refusal runs the adapter-owned
            // local cleanup. An unresolved acknowledgement is an explicit
            // ownership failure and the group is never reaped blind.
            RegistrationState::Pending => {
                match self.control.read_response(REVOKE_ACK_TIMEOUT).await {
                    Ok(EncoderControlResponse::Register { refused: false }) => {
                        self.registration = RegistrationState::Armed;
                        self.revoke_registered().await?;
                        self.stop_kill_reap().await
                    }
                    Ok(EncoderControlResponse::Register { refused: true }) => {
                        self.registration = RegistrationState::Refused;
                        self.stop_kill_reap().await
                    }
                    Ok(other) => bail!("Unexpected encoder control response while resolving registration: {other:?}"),
                    Err(error) => bail!("ENCODER_REGISTRATION_UNRESOLVED: {error:#}"),
                }
            }
            // A confirmed refusal (or a group with no endpoint registration)
            // is adapter-owned and is stopped locally.
            RegistrationState::Refused | RegistrationState::Unregistered => self.stop_kill_reap().await,
        }
    }

    /// Send `RevokeEncoder` and require the typed non-refused acknowledgement
    /// before the group is addressed. A refused or unresolved revoke is an
    /// explicit ownership failure; the caller must not treat it as
    /// acknowledgement or proceed to reap.
    async fn revoke_registered(&mut self) -> Result<()> {
        let registration_id = self.registration_id;
        match self.control.revoke(registration_id).await? {
            EncoderControlResponse::Revoke { refused: false } => Ok(()),
            EncoderControlResponse::Revoke { refused: true } => {
                bail!("ENCODER_REVOKE_REFUSED: endpoint refused revoke for registration {registration_id}")
            }
            other => bail!("Unexpected encoder control response: {other:?}"),
        }
    }

    /// Refused-permit local cleanup. The endpoint never stored this group, so
    /// it is stopped, killed and reaped without `RevokeEncoder`/`Revoked`.
    pub async fn finalize_local_refused(&mut self) -> Result<EncoderStop> {
        self.stop_kill_reap().await
    }

    async fn stop_kill_reap(&mut self) -> Result<EncoderStop> {
        let group = self.group.context("Encoder group identity missing")?;
        signal_group(group, libc::SIGTERM);
        // Decide on escalation from the group's own existence, never from a
        // reaped leader. The leader stays unreaped here, so its numeric pgid
        // still names exactly this group when a later SIGKILL is chosen. Reaping
        // first would release that identity for reuse and let a stale pgid
        // address an unrelated group.
        let group_gone = wait_group_gone(group, GROUP_GRACE).await;
        let escalated = !group_gone;
        if escalated {
            // The leader is still unreaped, so the pgid is still pinned to this
            // process group.
            signal_group(group, libc::SIGKILL);
        }
        if !wait_group_gone(group, REAP_DEADLINE).await {
            bail!("Encoder process group survived SIGKILL");
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

    /// Enter the unresolved endpoint-authority state and keep the exact child
    /// retained for one bounded window. If the control resource is permanently
    /// gone, this returns an explicit terminal instead of polling forever. The
    /// child is deliberately not reaped here: only a typed `finalize` outcome
    /// may clear the direct-child identity.
    pub async fn retain_unresolved(&mut self, error: anyhow::Error) -> Result<EncoderStop> {
        ensure!(self.has_live_child(), "No encoder child is retained");
        ensure!(self.registration != RegistrationState::Unregistered,
            "No endpoint registration is retained");
        let deadline = match self.retention_deadline {
            Some(deadline) => deadline,
            None => {
                let deadline = Instant::now() + RETENTION_TERMINAL_TIMEOUT;
                self.retention_deadline = Some(deadline);
                deadline
            }
        };
        if Instant::now() >= deadline {
            bail!("ENCODER_FINALIZATION_UNRESOLVED: endpoint authority was not retired within the bounded retention window: {error:#}");
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        if !self.has_live_child() {
            bail!("ENCODER_REGISTRATION_UNRESOLVED: encoder child was dropped before authority retirement");
        }
        ensure!(Instant::now() >= deadline, "Encoder retention window ended early");
        bail!("ENCODER_FINALIZATION_UNRESOLVED: endpoint authority was not retired within the bounded retention window: {error:#}");
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
        if !group_exists(pgid) { return true; }
        if tokio::time::Instant::now() >= deadline { return false; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn group_exists(pgid: i32) -> bool {
    // Safety: signal 0 performs an existence/permission check only.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

struct EncoderControlClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl EncoderControlClient {
    async fn exchange(&mut self, request: &EncoderControlRequest, timeout: Duration) -> Result<EncoderControlResponse> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        tokio::time::timeout(timeout, self.writer.write_all(&bytes)).await??;
        self.read_response(timeout).await
    }

    /// Read one bounded typed response. It is used both for a normal exchange
    /// and to resolve an outstanding `Register` acknowledgement whose read was
    /// interrupted by cancellation.
    async fn read_response(&mut self, timeout: Duration) -> Result<EncoderControlResponse> {
        let mut line = String::new();
        tokio::time::timeout(timeout, self.reader.read_line(&mut line)).await??;
        ensure!(line.ends_with('\n') && line.len() <= 4096, "Invalid bounded encoder control response");
        line.pop();
        Ok(serde_json::from_str(&line).context("Decode encoder control response")?)
    }

    async fn acquire(&mut self, registration_id: u64) -> Result<EncoderPermit> {
        match self.exchange(&EncoderControlRequest::Acquire { registration_id }, CONTROL_EXCHANGE_TIMEOUT).await? {
            EncoderControlResponse::Acquire { permit } => Ok(permit),
            other => bail!("Unexpected encoder control response: {other:?}"),
        }
    }

    async fn register(&mut self, registration_id: u64, pgid: i32) -> Result<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Register { registration_id, pgid }, CONTROL_EXCHANGE_TIMEOUT).await
    }

    async fn revoke(&mut self, registration_id: u64) -> Result<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Revoke { registration_id }, REVOKE_ACK_TIMEOUT).await
    }

    async fn settle(&mut self, registration_id: u64, outcome: EncoderSettlement) -> Result<()> {
        match self.exchange(&EncoderControlRequest::Settle { registration_id, outcome }, CONTROL_EXCHANGE_TIMEOUT).await? {
            EncoderControlResponse::Settle => Ok(()),
            other => bail!("Unexpected encoder control response: {other:?}"),
        }
    }
}

async fn bounded_read(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= limit, "Encoder output exceeds limit");
    Ok(bytes)
}
