//! Host-local remote admission control resource.
//!
//! This module is compiled into both the `obscura-host` and `obscura-endpoint`
//! binaries. It owns the frozen typed admission messages and the exact
//! newline-framed reader/writer bounds. It carries no browser wire, no session
//! state and no profile store. The token is the only credential: it travels
//! from the endpoint's `/control` upgrade to the Host, which compares it. No
//! principal, scope or grant travels here.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Frozen admission request bound: one newline-terminated typed JSON frame.
pub const REMOTE_ADMISSION_REQUEST_MAX: usize = 64 * 1024;
/// Frozen admission result bound. Mirrors the browser response bound.
pub const REMOTE_ADMISSION_RESULT_MAX: usize = 1024 * 1024;
/// Frozen result-write deadline. There is no admission read deadline.
pub const REMOTE_ADMISSION_WRITE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// One typed admission request on the Host-owned `remote-admission.sock`. The
/// endpoint copies the client's `Authorization: Bearer <token>` value verbatim;
/// the Host owns the single comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAdmissionRequest {
    pub token: String,
    pub request_id: u64,
}

/// Typed admission result. `Err.request_id` is `None` only with
/// `REMOTE_ADMISSION_MALFORMED`; a completely decoded denial keeps `Some(id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAdmissionResult {
    Ok { request_id: u64, reservation_id: String, session_id: String },
    Err { request_id: Option<u64>, code: RemoteAdmissionErrorCode, message: String },
}

impl RemoteAdmissionResult {
    pub fn denial(request_id: u64, code: RemoteAdmissionErrorCode, message: impl Into<String>) -> Self {
        Self::Err { request_id: Some(request_id), code, message: message.into() }
    }
}

/// Frozen Host-local admission error surface. A wrong token and a session that
/// cannot be opened are the only typed refusals; a malformed frame stays
/// uncorrelated. `RemoteAdmissionEof` is retired: an empty read-side EOF is a
/// transport-owner closure with no result frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteAdmissionErrorCode {
    #[serde(rename = "REMOTE_ADMISSION_MALFORMED")]
    RemoteAdmissionMalformed,
    #[serde(rename = "REMOTE_TOKEN_INVALID")]
    RemoteTokenInvalid,
    #[serde(rename = "REMOTE_SESSION_UNAVAILABLE")]
    RemoteSessionUnavailable,
}

impl RemoteAdmissionErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RemoteAdmissionMalformed => "REMOTE_ADMISSION_MALFORMED",
            Self::RemoteTokenInvalid => "REMOTE_TOKEN_INVALID",
            Self::RemoteSessionUnavailable => "REMOTE_SESSION_UNAVAILABLE",
        }
    }
}

/// Classified admission framing/decoding failure. It is never used for a clean
/// empty EOF or a transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionReadError {
    Malformed(&'static str),
    TooLarge(&'static str),
    PartialFrameEof(&'static str),
}

impl AdmissionReadError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Malformed(message) | Self::TooLarge(message) | Self::PartialFrameEof(message) => message,
        }
    }
}

/// One admission read outcome. `NoFrameEof` is a transport-owner closure, not a
/// typed refusal: the caller returns before any result write.
#[derive(Debug)]
pub enum AdmissionRead {
    FrameDecoded(RemoteAdmissionRequest),
    Failure(AdmissionReadError),
    NoFrameEof,
}

/// Split one accepted stream exactly once and retain the same reader/writer for
/// admission, result, `Ready` and the single browser loop.
pub struct RemoteConnectionIo {
    pub reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    pub writer: tokio::net::unix::OwnedWriteHalf,
}

impl RemoteConnectionIo {
    pub fn from_stream(stream: tokio::net::UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self { reader: BufReader::new(read), writer: write }
    }
}

/// Read one bounded admission frame. `take(MAX + 1)` is applied before
/// `read_until`, and `n > MAX` is classified oversized before the delimiter is
/// considered, so `MAX + 1` bytes ending in a newline are still rejected.
pub async fn read_remote_admission(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<AdmissionRead> {
    let mut frame = Vec::new();
    let consumed = (&mut *reader)
        .take((REMOTE_ADMISSION_REQUEST_MAX + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await?;
    if consumed == 0 {
        return Ok(AdmissionRead::NoFrameEof);
    }
    if consumed > REMOTE_ADMISSION_REQUEST_MAX {
        return Ok(AdmissionRead::Failure(AdmissionReadError::TooLarge(
            "Remote admission request exceeds the 64 KiB bound",
        )));
    }
    if frame.last() != Some(&b'\n') {
        return Ok(AdmissionRead::Failure(AdmissionReadError::PartialFrameEof(
            "Remote admission request ended before a newline delimiter",
        )));
    }
    match serde_json::from_slice::<RemoteAdmissionRequest>(&frame) {
        Ok(request) => Ok(AdmissionRead::FrameDecoded(request)),
        Err(_) => Ok(AdmissionRead::Failure(AdmissionReadError::Malformed(
            "Remote admission request is not a complete typed JSON frame",
        ))),
    }
}

/// Convert only classified admission failures into the uncorrelated typed
/// refusal. Never called for `FrameDecoded`, `NoFrameEof` or a transport error.
pub fn admission_error_result(failure: AdmissionReadError) -> RemoteAdmissionResult {
    RemoteAdmissionResult::Err {
        request_id: None,
        code: RemoteAdmissionErrorCode::RemoteAdmissionMalformed,
        message: failure.message().to_string(),
    }
}

/// Write exactly one bounded admission result under the frozen 2 s deadline.
pub async fn write_remote_admission_result(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    result: &RemoteAdmissionResult,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(result).context("Serialize remote admission result")?;
    if bytes.len() + 1 > REMOTE_ADMISSION_RESULT_MAX {
        bail!("Remote admission result exceeds the 1 MiB bound");
    }
    bytes.push(b'\n');
    tokio::time::timeout(REMOTE_ADMISSION_WRITE_DEADLINE, writer.write_all(&bytes))
        .await.context("Remote admission result deadline")??;
    Ok(())
}

/// Endpoint side: submit one admission request under the frozen write deadline.
pub async fn write_remote_admission_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &RemoteAdmissionRequest,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(request).context("Serialize remote admission request")?;
    if bytes.len() + 1 > REMOTE_ADMISSION_REQUEST_MAX {
        bail!("Remote admission request exceeds the 64 KiB bound");
    }
    bytes.push(b'\n');
    tokio::time::timeout(REMOTE_ADMISSION_WRITE_DEADLINE, writer.write_all(&bytes))
        .await.context("Remote admission request deadline")??;
    Ok(())
}

/// Endpoint side: read one bounded admission result from the retained reader.
/// A clean EOF before any byte returns `None`; the caller reports a transport
/// closure and never fabricates a typed result.
pub async fn read_remote_admission_result(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Option<RemoteAdmissionResult>> {
    let mut frame = Vec::new();
    let consumed = (&mut *reader)
        .take((REMOTE_ADMISSION_RESULT_MAX + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await?;
    if consumed == 0 {
        return Ok(None);
    }
    if consumed > REMOTE_ADMISSION_RESULT_MAX || frame.last() != Some(&b'\n') {
        bail!("Invalid bounded remote admission result frame");
    }
    let result = serde_json::from_slice::<RemoteAdmissionResult>(&frame)
        .context("Decode remote admission result")?;
    Ok(Some(result))
}
