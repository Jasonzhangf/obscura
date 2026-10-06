//! Strict typed readers for the Host authorization registry and the endpoint
//! authorized-client projection.
//!
//! The registry is the single device identity/scope/initial-grant source. The
//! projection is a read-only derivation consumed by the endpoint. Neither is a
//! browser wire. This module is compiled into both binaries and depends only on
//! the frozen admission types, `serde` and a SHA-256 digest.
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::remote_admission::{
    InitialProfileGrant, InitialProfileSelection, RemoteAdmissionErrorCode, RemotePrincipalContext,
    RemoteScope,
};

pub const REGISTRY_SCHEMA_VERSION: u32 = 2;

/// A typed authorization failure. It always carries the frozen admission code
/// so the Host admission owner writes one correlated refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailure {
    pub code: RemoteAdmissionErrorCode,
    pub message: String,
}

impl AuthFailure {
    pub fn new(code: RemoteAdmissionErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// The registry `initial_profile_grant` object only carries the selection and
/// the grant freshness revision; `device_id` comes from the owning entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireGrant {
    pub selection: InitialProfileSelection,
    pub authorization_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Active,
    Revoked,
}

/// One `registry.json` device entry. Unknown fields are rejected so a schema
/// drift cannot silently change authorization semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryDevice {
    pub device_id: String,
    pub device_label: String,
    pub client_certificate_sha256: String,
    pub scopes: Vec<RemoteScope>,
    pub initial_profile_grant: WireGrant,
    pub scope_revision: u64,
    pub status: DeviceStatus,
    pub not_before_unix: i64,
    pub not_after_unix: i64,
    pub created_at_unix: i64,
    pub revoked_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryFile {
    pub schema_version: u32,
    pub authorization_revision: u64,
    pub host_id: String,
    pub devices: Vec<RegistryDevice>,
}

impl RegistryFile {
    /// Parse the registry source bytes strictly. Duplicate device ids or
    /// duplicate certificate fingerprints are rejected, never merged.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let registry: RegistryFile = serde_json::from_slice(bytes).context("Decode authorization registry")?;
        if registry.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!("Authorization registry schema_version must be {REGISTRY_SCHEMA_VERSION}");
        }
        let mut device_ids = BTreeSet::new();
        let mut fingerprints = BTreeSet::new();
        for device in &registry.devices {
            if device.device_id.trim().is_empty() {
                bail!("Authorization registry device_id must not be empty");
            }
            if !device_ids.insert(device.device_id.clone()) {
                bail!("Authorization registry contains a duplicate device_id");
            }
            let fingerprint = decode_hex(&device.client_certificate_sha256)
                .filter(|bytes| bytes.len() == 32)
                .context("Authorization registry client_certificate_sha256 must be 32 hex bytes")?;
            if !fingerprints.insert(fingerprint) {
                bail!("Authorization registry contains a duplicate certificate fingerprint");
            }
            if device.scopes.iter().collect::<BTreeSet<_>>().len() != device.scopes.len() {
                bail!("Authorization registry device scopes must be unique");
            }
        }
        Ok(registry)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("Read authorization registry {path:?}"))?;
        Self::parse(&bytes)
    }

    /// Independently verify a presented principal against the current committed
    /// registry material and return the registry-owned initial grant.
    pub fn authorize(&self, principal: &RemotePrincipalContext) -> Result<InitialProfileGrant, AuthFailure> {
        if principal.device_id.trim().is_empty() {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemoteAuthRequired,
                "Remote principal has no device identity",
            ));
        }
        if principal.authorization_revision != self.authorization_revision {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemotePrincipalRevoked,
                "Remote principal was derived from a stale authorization revision",
            ));
        }
        let device = self
            .devices
            .iter()
            .find(|device| device.device_id == principal.device_id)
            .ok_or_else(|| AuthFailure::new(
                RemoteAdmissionErrorCode::RemoteAuthRequired,
                "Remote device is not registered",
            ))?;
        if device.status != DeviceStatus::Active {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemotePrincipalRevoked,
                "Remote device is revoked",
            ));
        }
        let now = unix_now();
        if now < device.not_before_unix || now > device.not_after_unix {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemotePrincipalRevoked,
                "Remote device certificate is outside its authorization window",
            ));
        }
        let fingerprint = decode_hex(&device.client_certificate_sha256).ok_or_else(|| AuthFailure::new(
            RemoteAdmissionErrorCode::RemoteAuthRequired,
            "Registry certificate fingerprint is malformed",
        ))?;
        if fingerprint.as_slice() != principal.client_cert_sha256.as_slice() {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemoteAuthRequired,
                "Remote principal certificate does not match the registry",
            ));
        }
        let scopes: BTreeSet<RemoteScope> = device.scopes.iter().copied().collect();
        if scopes != principal.scopes {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemoteAuthRequired,
                "Remote principal scope set does not match the registry",
            ));
        }
        if device.initial_profile_grant.authorization_revision != self.authorization_revision {
            return Err(AuthFailure::new(
                RemoteAdmissionErrorCode::RemotePrincipalRevoked,
                "Registry initial profile grant is stale",
            ));
        }
        Ok(InitialProfileGrant {
            device_id: device.device_id.clone(),
            authorization_revision: device.initial_profile_grant.authorization_revision,
            selection: device.initial_profile_grant.selection.clone(),
        })
    }
}

/// One `authorized-clients.json` entry. The projection is endpoint input only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionClient {
    pub device_id: String,
    pub client_certificate_sha256: String,
    pub scopes: Vec<RemoteScope>,
    pub initial_profile_grant: WireGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionFile {
    pub schema_version: u32,
    pub authorization_revision: u64,
    pub registry_sha256: String,
    pub clients: Vec<ProjectionClient>,
}

/// A loaded projection plus the external digest of the exact bytes read.
#[derive(Debug, Clone)]
pub struct LoadedProjection {
    pub projection: ProjectionFile,
    pub projection_sha256: [u8; 32],
}

impl LoadedProjection {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("Read authorized-clients projection {path:?}"))?;
        let projection: ProjectionFile = serde_json::from_slice(&bytes).context("Decode authorized-clients projection")?;
        if projection.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!("authorized-clients schema_version must be {REGISTRY_SCHEMA_VERSION}");
        }
        if decode_hex(&projection.registry_sha256).filter(|bytes| bytes.len() == 32).is_none() {
            bail!("authorized-clients registry_sha256 must be 32 hex bytes");
        }
        let mut device_ids = BTreeSet::new();
        let mut fingerprints = BTreeSet::new();
        for client in &projection.clients {
            if client.device_id.trim().is_empty() {
                bail!("authorized-clients device_id must not be empty");
            }
            if !device_ids.insert(client.device_id.clone()) {
                bail!("authorized-clients contains a duplicate device_id");
            }
            let fingerprint = decode_hex(&client.client_certificate_sha256)
                .filter(|bytes| bytes.len() == 32)
                .context("authorized-clients client_certificate_sha256 must be 32 hex bytes")?;
            if !fingerprints.insert(fingerprint) {
                bail!("authorized-clients contains a duplicate certificate fingerprint");
            }
            if client.scopes.iter().collect::<BTreeSet<_>>().len() != client.scopes.len() {
                bail!("authorized-clients client scopes must be unique");
            }
        }
        let projection_sha256 = sha256(&bytes);
        Ok(Self { projection, projection_sha256 })
    }

    /// Resolve the mTLS peer certificate fingerprint to the typed Host-local
    /// principal and initial grant. An empty client set denies every peer.
    pub fn principal_for_cert(&self, cert_sha256: &[u8; 32]) -> Option<(RemotePrincipalContext, InitialProfileGrant)> {
        let client = self.projection.clients.iter().find(|client| {
            decode_hex(&client.client_certificate_sha256)
                .is_some_and(|fingerprint| fingerprint.as_slice() == cert_sha256.as_slice())
        })?;
        let scopes: BTreeSet<RemoteScope> = client.scopes.iter().copied().collect();
        let principal = RemotePrincipalContext {
            device_id: client.device_id.clone(),
            client_cert_sha256: *cert_sha256,
            scopes,
            authorization_revision: self.projection.authorization_revision,
        };
        let initial = InitialProfileGrant {
            device_id: client.device_id.clone(),
            authorization_revision: client.initial_profile_grant.authorization_revision,
            selection: client.initial_profile_grant.selection.clone(),
        };
        Some((principal, initial))
    }

    pub fn registry_sha256(&self) -> &str {
        &self.projection.registry_sha256
    }

    pub fn authorization_revision(&self) -> u64 {
        self.projection.authorization_revision
    }
}

/// Typed endpoint status receipt. `projection_sha256` is the digest of the exact
/// read buffer; it is never a projection content field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointStatusReceipt {
    pub schema_version: u32,
    pub endpoint_generation: u64,
    pub endpoint_pid: u32,
    pub bound_address: String,
    pub authorization_revision: u64,
    pub registry_sha256: String,
    pub projection_sha256: String,
}

impl EndpointStatusReceipt {
    /// The v1 applied receipt. It carries no `status` field and is written by
    /// the endpoint only after the TCP listener and local socket directory are
    /// ready.
    pub fn applied_v1(
        projection: &LoadedProjection,
        bound_address: String,
        endpoint_generation: u64,
        endpoint_pid: u32,
    ) -> Self {
        Self {
            schema_version: 1,
            endpoint_generation,
            endpoint_pid,
            bound_address,
            authorization_revision: projection.authorization_revision(),
            registry_sha256: projection.registry_sha256().to_string(),
            projection_sha256: encode_hex(&projection.projection_sha256),
        }
    }

    /// Strict typed consumer for the endpoint-applied receipt. Every field is
    /// required, unknown fields are refused, and `schema_version` must be 1.
    /// A partial, malformed, missing-field or unknown-field receipt is refused
    /// here and never mapped to an applied result.
    pub fn load_applied(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("Read endpoint applied receipt {path:?}"))?;
        let receipt: Self = serde_json::from_slice(&bytes)
            .context("Decode endpoint applied receipt")?;
        if receipt.schema_version != 1 {
            bail!("endpoint applied receipt schema_version must be 1");
        }
        Ok(receipt)
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    bytes
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::EndpointStatusReceipt;

    fn valid_receipt() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "endpoint_generation": 3,
            "endpoint_pid": 4242,
            "bound_address": "127.0.0.1:8443",
            "authorization_revision": 7,
            "registry_sha256": "aa".repeat(32),
            "projection_sha256": "bb".repeat(32)
        })
    }

    fn load(value: &serde_json::Value) -> anyhow::Result<EndpointStatusReceipt> {
        let dir = std::env::temp_dir().join(format!("ear-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("applied.json");
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        let result = EndpointStatusReceipt::load_applied(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        result
    }

    #[test]
    fn applied_receipt_accepts_exact_v1_shape() {
        let receipt = load(&valid_receipt()).expect("exact v1 shape must parse");
        assert_eq!(receipt.schema_version, 1);
        assert_eq!(receipt.endpoint_generation, 3);
        assert_eq!(receipt.endpoint_pid, 4242);
        assert_eq!(receipt.bound_address, "127.0.0.1:8443");
        assert_eq!(receipt.authorization_revision, 7);
        assert_eq!(receipt.registry_sha256, "aa".repeat(32));
        assert_eq!(receipt.projection_sha256, "bb".repeat(32));
    }

    #[test]
    fn applied_receipt_refuses_missing_unknown_and_malformed_fields() {
        let mut missing = valid_receipt();
        missing.as_object_mut().unwrap().remove("projection_sha256");
        assert!(load(&missing).is_err(), "missing field must be refused");

        let mut unknown = valid_receipt();
        unknown["status"] = serde_json::json!("ready");
        assert!(load(&unknown).is_err(), "unknown status field must be refused");

        let mut wrong_schema = valid_receipt();
        wrong_schema["schema_version"] = serde_json::json!(2);
        assert!(load(&wrong_schema).is_err(), "non-v1 schema_version must be refused");

        let dir = std::env::temp_dir().join(format!("ear-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("applied.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(EndpointStatusReceipt::load_applied(&path).is_err(), "malformed JSON must be refused");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
