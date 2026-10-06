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
/// Cooperative group stop grace before escalation to SIGKILL.
const GROUP_GRACE: Duration = Duration::from_secs(1);
/// Bounded leader reap deadline after group stop/kill.
const REAP_DEADLINE: Duration = Duration::from_secs(3);

pub enum EncoderStop {
    Exited(std::process::ExitStatus),
    KilledAndReaped(std::process::ExitStatus),
}

/// One adapter's retained encoder child owner. It survives cancellation of an
/// in-flight `encode_frame` so the caller can always finalize the subtree.
pub struct EncoderChildOwner {
    binary: std::path::PathBuf,
    group: Option<i32>,
    child: Option<tokio::process::Child>,
    control: EncoderControlClient,
    registration_id: u64,
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
        let registered = self.control.register(registration_id, pgid).await;
        match registered {
            Ok(EncoderControlResponse::Register { refused: false }) => {}
            Ok(EncoderControlResponse::Register { refused: true }) => {
                self.finalize_local_refused().await.context("Local refused encoder cleanup")?;
                self.control.settle(registration_id, EncoderSettlement::RefusedGroupStopped).await
                    .context("Report refused encoder settlement")?;
                bail!("ENCODER_ADMISSION_FINALIZED: Host session finalizer refused encoder registration");
            }
            Err(error) => {
                self.finalize_local_refused().await.context("Local refused encoder cleanup")?;
                return Err(error);
            }
            Ok(other) => {
                self.finalize_local_refused().await.context("Local refused encoder cleanup")?;
                bail!("Unexpected encoder control response: {other:?}");
            }
        }
        let outcome = tokio::time::timeout(ENCODE_TIMEOUT, self.run_encode(registration_id, rgba)).await
            .context("H.264 encoder exceeded five second deadline");
        match outcome {
            Ok(result) => result,
            Err(error) => {
                self.finalize().await.ok();
                Err(error)
            }
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
        let status = {
            let child = self.child.as_mut().context("Encoder child missing")?;
            child.wait().await.context("Wait for encoder leader")?
        };
        ensure!(status.success(), "H.264 encoder failed: {}", String::from_utf8_lossy(&errors));
        ensure!(!bytes.is_empty(), "H.264 encoder produced no access unit");
        self.group = None;
        self.child = None;
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
        let registration_id = self.registration_id;
        // Wait for the endpoint's typed acknowledgement before addressing the
        // group. The acknowledgement is ordered after endpoint signal
        // authority is complete (B4), so it is safe to stop the group here.
        let _ = self.control.revoke(registration_id).await;
        self.stop_kill_reap().await
    }

    /// Refused-permit local cleanup. The endpoint never stored this group, so
    /// it is stopped, killed and reaped without `RevokeEncoder`/`Revoked`.
    pub async fn finalize_local_refused(&mut self) -> Result<EncoderStop> {
        self.stop_kill_reap().await
    }

    async fn stop_kill_reap(&mut self) -> Result<EncoderStop> {
        let group = self.group.context("Encoder group identity missing")?;
        signal_group(group, libc::SIGTERM);
        let leader_exited = self.reap_within(GROUP_GRACE).await?;
        let group_gone = wait_group_gone(group, GROUP_GRACE).await;
        let escalated = !leader_exited || !group_gone;
        if escalated {
            signal_group(group, libc::SIGKILL);
        }
        if !wait_group_gone(group, REAP_DEADLINE).await {
            bail!("Encoder process group survived SIGKILL");
        }
        let status = self.reaped_status().await?;
        if escalated {
            Ok(EncoderStop::KilledAndReaped(status))
        } else {
            Ok(EncoderStop::Exited(status))
        }
    }

    /// Try to reap the leader within `grace`. Returns true when the leader
    /// exited and was reaped, false when the deadline expired.
    async fn reap_within(&mut self, grace: Duration) -> Result<bool> {
        let child = self.child.as_mut().context("Encoder child missing")?;
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(status) => status.map(|_| true).context("Wait for encoder leader"),
            Err(_) => Ok(false),
        }
    }

    /// Block for the already-signalled leader to exit and be reaped.
    async fn reaped_status(&mut self) -> Result<std::process::ExitStatus> {
        let status = tokio::time::timeout(REAP_DEADLINE, async {
            let child = self.child.as_mut().context("Encoder child missing")?;
            Ok::<_, anyhow::Error>(child.wait().await?)
        })
        .await
        .context("Encoder leader reap deadline exceeded")??;
        self.group = None;
        self.child = None;
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
    async fn exchange(&mut self, request: &EncoderControlRequest) -> Result<EncoderControlResponse> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(2), self.writer.write_all(&bytes)).await??;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), self.reader.read_line(&mut line)).await??;
        ensure!(line.ends_with('\n') && line.len() <= 4096, "Invalid bounded encoder control response");
        line.pop();
        Ok(serde_json::from_str(&line).context("Decode encoder control response")?)
    }

    async fn acquire(&mut self, registration_id: u64) -> Result<EncoderPermit> {
        match self.exchange(&EncoderControlRequest::Acquire { registration_id }).await? {
            EncoderControlResponse::Acquire { permit } => Ok(permit),
            other => bail!("Unexpected encoder control response: {other:?}"),
        }
    }

    async fn register(&mut self, registration_id: u64, pgid: i32) -> Result<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Register { registration_id, pgid }).await
    }

    async fn revoke(&mut self, registration_id: u64) -> Result<EncoderControlResponse> {
        self.exchange(&EncoderControlRequest::Revoke { registration_id }).await
    }

    async fn settle(&mut self, registration_id: u64, outcome: EncoderSettlement) -> Result<()> {
        match self.exchange(&EncoderControlRequest::Settle { registration_id, outcome }).await? {
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
