//! Host-local remote admission control resource.
//!
//! This module is compiled into both the `obscura-host` and `obscura-endpoint`
//! binaries. It owns the frozen typed admission messages and the exact
//! newline-framed reader/writer bounds. It carries no browser wire, no session
//! state and no profile store. The principal and initial grant are Host-local
//! control facts: they never travel in a browser `Request`, `Response` or
//! metadata field.
use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Frozen admission request bound: one newline-terminated typed JSON frame.
pub const REMOTE_ADMISSION_REQUEST_MAX: usize = 64 * 1024;
/// Frozen admission result bound. Mirrors the browser response bound.
pub const REMOTE_ADMISSION_RESULT_MAX: usize = 1024 * 1024;
/// Frozen result-write deadline. There is no admission read deadline.
pub const REMOTE_ADMISSION_WRITE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// One explicitly paired remote device. The values are derived from the Host
/// registry and endpoint projection, never from a browser payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePrincipalContext {
    pub device_id: String,
    pub client_cert_sha256: [u8; 32],
    pub scopes: BTreeSet<RemoteScope>,
    pub authorization_revision: u64,
}

/// The only remote capability values admitted by the first slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteScope {
    ObserveHumanControl,
    ProfileRead,
    ProfileCreate,
    ProfileSelect,
}

impl RemoteScope {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::ObserveHumanControl => "observe_human_control",
            Self::ProfileRead => "profile_read",
            Self::ProfileCreate => "profile_create",
            Self::ProfileSelect => "profile_select",
        }
    }
}

/// Explicit initial profile selection. `DefaultProfile` is a string on the
/// wire and `ProfileId` is a single-key object, exactly as frozen by G-1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialProfileSelection {
    DefaultProfile,
    ProfileId(String),
}

/// Host-local initial binding. The wire `initial_profile_grant` object only
/// carries `selection` and `authorization_revision`; `device_id` is supplied by
/// the owning registry/projection entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialProfileGrant {
    pub device_id: String,
    pub authorization_revision: u64,
    pub selection: InitialProfileSelection,
}

/// One typed admission request on the Host-owned `remote-admission.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAdmissionRequest {
    pub principal: RemotePrincipalContext,
    pub initial: InitialProfileGrant,
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

/// Frozen Host-local admission error surface. `RemoteAdmissionEof` is retired:
/// an empty read-side EOF is a transport-owner closure with no result frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteAdmissionErrorCode {
    #[serde(rename = "REMOTE_AUTH_REQUIRED")]
    RemoteAuthRequired,
    #[serde(rename = "REMOTE_PRINCIPAL_REVOKED")]
    RemotePrincipalRevoked,
    #[serde(rename = "REMOTE_PROFILE_SCOPE_REQUIRED")]
    RemoteProfileScopeRequired,
    #[serde(rename = "REMOTE_INITIAL_PROFILE_UNBOUND")]
    RemoteInitialProfileUnbound,
    #[serde(rename = "REMOTE_INITIAL_PROFILE_ACCESS_DENIED")]
    RemoteInitialProfileAccessDenied,
    #[serde(rename = "PROFILE_ACCESS_MIGRATION_REQUIRED")]
    ProfileAccessMigrationRequired,
    #[serde(rename = "REMOTE_ADMISSION_MALFORMED")]
    RemoteAdmissionMalformed,
    #[serde(rename = "REMOTE_ADMISSION_CANCELED")]
    RemoteAdmissionCanceled,
}

impl RemoteAdmissionErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RemoteAuthRequired => "REMOTE_AUTH_REQUIRED",
            Self::RemotePrincipalRevoked => "REMOTE_PRINCIPAL_REVOKED",
            Self::RemoteProfileScopeRequired => "REMOTE_PROFILE_SCOPE_REQUIRED",
            Self::RemoteInitialProfileUnbound => "REMOTE_INITIAL_PROFILE_UNBOUND",
            Self::RemoteInitialProfileAccessDenied => "REMOTE_INITIAL_PROFILE_ACCESS_DENIED",
            Self::ProfileAccessMigrationRequired => "PROFILE_ACCESS_MIGRATION_REQUIRED",
            Self::RemoteAdmissionMalformed => "REMOTE_ADMISSION_MALFORMED",
            Self::RemoteAdmissionCanceled => "REMOTE_ADMISSION_CANCELED",
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
