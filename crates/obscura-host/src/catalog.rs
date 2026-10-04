//! Typed persistent profile catalog owned by the Host.
//!
//! The catalog persists profile id, display name, the default identity and the
//! Host-owned `ProfileAccess { owner_device_id, members }` under an explicit
//! profile root. It is typed configuration, never reconstructed by directory
//! scanning or log parsing. When no root is configured the Host runs one
//! in-memory profile and the catalog is not persistent.
//!
//! `ProfileAccess` is the single membership source. The device registry only
//! answers "what identity/scope does this device hold"; it never grants profile
//! membership. The browser wire `ProfileRecord` keeps its current fields; the
//! access structure is a Host-side persisted field only.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use obscura_host_protocol::{ProfileCatalog, ProfileRecord};
use serde::{Deserialize, Serialize};

/// Owner recorded for a profile created over the local authenticated
/// `host.sock`. Remote profiles always record the remote device id.
pub const LOCAL_DEVICE_ID: &str = "local";

#[derive(Debug)]
pub struct CatalogError {
    pub code: &'static str,
    pub message: String,
}

impl CatalogError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    fn io(context: &str, error: std::io::Error) -> Self {
        Self::new("PROFILE_CATALOG_FAILED", format!("{context}: {error}"))
    }
}

/// Host-side profile membership. `owner_device_id` is non-empty and is always
/// a member. A private profile is exactly `members == {owner_device_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileAccess {
    pub owner_device_id: String,
    pub members: BTreeSet<String>,
}

impl ProfileAccess {
    pub fn owner_only(owner_device_id: &str) -> Result<Self, CatalogError> {
        if owner_device_id.trim().is_empty() {
            return Err(CatalogError::new("PROFILE_ACCESS_FAILED", "profile owner must not be empty"));
        }
        let mut members = BTreeSet::new();
        members.insert(owner_device_id.to_string());
        Ok(Self { owner_device_id: owner_device_id.to_string(), members })
    }

    pub fn grants(&self, device_id: &str) -> bool {
        self.members.contains(device_id)
    }
}

/// One persisted catalog entry. `access` is `None` only for a legacy catalog
/// written before Host-side membership existed; such an entry refuses remote
/// resolution with `PROFILE_ACCESS_MIGRATION_REQUIRED` until a local owner runs
/// the explicit typed migration action.
#[derive(Debug, Clone)]
pub struct ProfileEntry {
    pub profile_id: String,
    pub display_name: String,
    pub default: bool,
    pub access: Option<ProfileAccess>,
}

impl ProfileEntry {
    fn wire(&self) -> ProfileRecord {
        ProfileRecord {
            profile_id: self.profile_id.clone(),
            display_name: self.display_name.clone(),
            default: self.default,
        }
    }
}

/// Typed receipt for one explicit local membership mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipReceipt {
    pub catalog_revision: u64,
    pub profile_id: String,
    pub device_id: String,
}

pub struct ProfileCatalogStore {
    root: Option<PathBuf>,
    profiles: Vec<ProfileEntry>,
    default_profile_id: Option<String>,
    next_id: u64,
    catalog_revision: u64,
}

impl ProfileCatalogStore {
    /// Load the catalog for the configured root. A fresh root creates one
    /// private local default profile; an existing root must contain a readable,
    /// well-formed catalog. A malformed catalog is a typed failure, never an
    /// empty success.
    pub fn load(root: Option<PathBuf>) -> Result<Self, CatalogError> {
        match root {
            None => Ok(Self {
                root: None,
                profiles: vec![fresh_default_entry()],
                default_profile_id: Some("default".to_string()),
                next_id: 1,
                catalog_revision: 1,
            }),
            Some(root) => {
                let path = root.join("catalog.json");
                if path.exists() {
                    let data = std::fs::read_to_string(&path)
                        .map_err(|error| CatalogError::io("read profile catalog", error))?;
                    let parsed = parse_catalog_file(&data)?;
                    if parsed.profiles.is_empty() {
                        return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog contains no profiles"));
                    }
                    Ok(Self {
                        root: Some(root),
                        profiles: parsed.profiles,
                        default_profile_id: parsed.default_profile_id,
                        next_id: parsed.next_id,
                        catalog_revision: parsed.catalog_revision,
                    })
                } else {
                    let mut store = Self {
                        root: Some(root),
                        profiles: vec![fresh_default_entry()],
                        default_profile_id: Some("default".to_string()),
                        next_id: 1,
                        catalog_revision: 1,
                    };
                    store.persist()?;
                    Ok(store)
                }
            }
        }
    }

    pub fn root_configured(&self) -> bool { self.root.is_some() }

    pub fn catalog_revision(&self) -> u64 { self.catalog_revision }

    /// Local projection: every loaded profile, unfiltered.
    pub fn catalog(&self) -> ProfileCatalog {
        ProfileCatalog {
            profiles: self.profiles.iter().map(ProfileEntry::wire).collect(),
            default_profile_id: self.default_profile_id.clone(),
            root_configured: self.root.is_some(),
        }
    }

    /// Remote projection: only profiles whose Host-owned access grants this
    /// device. A legacy entry without access is excluded, not silently granted.
    pub fn remote_catalog(&self, device_id: &str) -> ProfileCatalog {
        let profiles: Vec<ProfileRecord> = self.profiles.iter()
            .filter(|entry| entry.access.as_ref().is_some_and(|access| access.grants(device_id)))
            .map(ProfileEntry::wire)
            .collect();
        let default_profile_id = self.default_profile_id.clone()
            .filter(|id| profiles.iter().any(|profile| profile.profile_id == *id));
        ProfileCatalog {
            profiles,
            default_profile_id,
            root_configured: self.root.is_some(),
        }
    }

    pub fn record(&self, profile_id: &str) -> Option<&ProfileEntry> {
        self.profiles.iter().find(|profile| profile.profile_id == profile_id)
    }

    pub fn access_for(&self, profile_id: &str) -> Option<&ProfileAccess> {
        self.record(profile_id).and_then(|entry| entry.access.as_ref())
    }

    pub fn default_id(&self) -> Option<String> {
        self.default_profile_id.clone().or_else(|| self.profiles.first().map(|profile| profile.profile_id.clone()))
    }

    /// Directory allocated to a profile. Only the Host derives this; it never
    /// comes from the wire.
    pub fn profile_dir(&self, profile_id: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|root| root.join("profiles").join(profile_id))
    }

    /// Create a new empty profile owned by `owner_device_id` and persist the
    /// catalog before returning. A persistence failure means the profile was
    /// not created. The owner is always a member; no implicit default grant.
    pub fn create(&mut self, display_name: &str, make_default: bool, owner_device_id: &str) -> Result<ProfileRecord, CatalogError> {
        let name = display_name.trim();
        if name.is_empty() {
            return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "display name must not be empty"));
        }
        if name.chars().count() > 64 {
            return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "display name must be at most 64 characters"));
        }
        let access = ProfileAccess::owner_only(owner_device_id)?;
        let profile_id = format!("p{}", self.next_id);
        let entry = ProfileEntry {
            profile_id: profile_id.clone(),
            display_name: name.to_string(),
            default: make_default,
            access: Some(access),
        };
        let previous_profiles = self.profiles.clone();
        let previous_default = self.default_profile_id.clone();
        let previous_next = self.next_id;
        let previous_revision = self.catalog_revision;
        self.next_id = self.next_id.saturating_add(1);
        self.catalog_revision = self.catalog_revision.saturating_add(1);
        if make_default {
            for profile in &mut self.profiles { profile.default = false; }
            self.default_profile_id = Some(profile_id);
        }
        self.profiles.push(entry);
        if let Err(error) = self.persist() {
            self.profiles = previous_profiles;
            self.default_profile_id = previous_default;
            self.next_id = previous_next;
            self.catalog_revision = previous_revision;
            return Err(error);
        }
        Ok(self.profiles.last().expect("created profile exists").wire())
    }

    /// Explicit local membership authority. This is the single typed write path
    /// for `ProfileAccess`; the Host service consumer owns calling it with an
    /// owner-authorized action. It also completes a legacy migration because it
    /// writes the full owner/member set under a new catalog revision.
    pub fn assign_access(
        &mut self,
        profile_id: &str,
        owner_device_id: &str,
        members: BTreeSet<String>,
    ) -> Result<MembershipReceipt, CatalogError> {
        if owner_device_id.trim().is_empty() {
            return Err(CatalogError::new("PROFILE_ACCESS_FAILED", "profile owner must not be empty"));
        }
        if !members.contains(owner_device_id) {
            return Err(CatalogError::new("PROFILE_ACCESS_FAILED", "profile owner must be a member"));
        }
        let previous_profiles = self.profiles.clone();
        let previous_revision = self.catalog_revision;
        let Some(entry) = self.profiles.iter_mut().find(|entry| entry.profile_id == profile_id) else {
            return Err(CatalogError::new("PROFILE_NOT_FOUND", "Unknown profile id"));
        };
        entry.access = Some(ProfileAccess { owner_device_id: owner_device_id.to_string(), members });
        self.catalog_revision = self.catalog_revision.saturating_add(1);
        if let Err(error) = self.persist() {
            self.profiles = previous_profiles;
            self.catalog_revision = previous_revision;
            return Err(error);
        }
        Ok(MembershipReceipt {
            catalog_revision: self.catalog_revision,
            profile_id: profile_id.to_string(),
            device_id: owner_device_id.to_string(),
        })
    }

    fn persist(&self) -> Result<(), CatalogError> {
        let Some(root) = &self.root else { return Ok(()); };
        std::fs::create_dir_all(root).map_err(|error| CatalogError::io("create profile root", error))?;
        let profiles: Vec<serde_json::Value> = self.profiles.iter().map(|profile| {
            let mut value = serde_json::json!({
                "profile_id": profile.profile_id,
                "display_name": profile.display_name,
                "default": profile.default,
            });
            if let Some(access) = &profile.access {
                value["access"] = serde_json::to_value(access).unwrap_or(serde_json::Value::Null);
            }
            value
        }).collect();
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "catalog_revision": self.catalog_revision,
            "profiles": profiles,
            "default_profile_id": self.default_profile_id,
            "next_id": self.next_id,
        })).map_err(|error| CatalogError::new("PROFILE_CATALOG_FAILED", format!("serialize profile catalog: {error}")))?;
        let path = root.join("catalog.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|error| CatalogError::io("write profile catalog", error))?;
        std::fs::rename(&tmp, &path).map_err(|error| CatalogError::io("commit profile catalog", error))?;
        Ok(())
    }
}

fn fresh_default_entry() -> ProfileEntry {
    ProfileEntry {
        profile_id: "default".to_string(),
        display_name: "Default".to_string(),
        default: true,
        access: ProfileAccess::owner_only(LOCAL_DEVICE_ID).ok(),
    }
}

struct CatalogFile {
    profiles: Vec<ProfileEntry>,
    default_profile_id: Option<String>,
    next_id: u64,
    catalog_revision: u64,
}

fn parse_catalog_file(data: &str) -> Result<CatalogFile, CatalogError> {
    let value: serde_json::Value = serde_json::from_str(data).map_err(|error| {
        CatalogError::new("PROFILE_CATALOG_FAILED", format!("profile catalog is corrupt: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog must be a JSON object")
    })?;
    let entries = object.get("profiles").and_then(serde_json::Value::as_array).ok_or_else(|| {
        CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog profiles must be an array")
    })?;
    let mut profiles = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.as_object().ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog entry must be an object")
        })?;
        let profile_id = entry.get("profile_id").and_then(serde_json::Value::as_str).ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog profile_id must be a string")
        })?;
        let display_name = entry.get("display_name").and_then(serde_json::Value::as_str).ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog display_name must be a string")
        })?;
        let is_default = entry.get("default").and_then(serde_json::Value::as_bool).ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog default must be a boolean")
        })?;
        let access = match entry.get("access") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let access: ProfileAccess = serde_json::from_value(value.clone()).map_err(|error| {
                    CatalogError::new("PROFILE_CATALOG_FAILED", format!("profile catalog access is invalid: {error}"))
                })?;
                if access.owner_device_id.trim().is_empty() {
                    return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog access owner must not be empty"));
                }
                if !access.members.contains(&access.owner_device_id) {
                    return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog access owner must be a member"));
                }
                Some(access)
            }
        };
        profiles.push(ProfileEntry {
            profile_id: profile_id.to_string(),
            display_name: display_name.to_string(),
            default: is_default,
            access,
        });
    }
    let default_profile_id = match object.get("default_profile_id") {
        Some(serde_json::Value::Null) | None => None,
        Some(value) => Some(value.as_str().ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog default_profile_id must be a string or null")
        })?.to_string()),
    };
    let next_id = match object.get("next_id") {
        Some(value) => value.as_u64().ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog next_id must be a non-negative integer")
        })?,
        None => 1,
    };
    let catalog_revision = match object.get("catalog_revision") {
        Some(value) => value.as_u64().ok_or_else(|| {
            CatalogError::new("PROFILE_CATALOG_FAILED", "profile catalog catalog_revision must be a non-negative integer")
        })?,
        None => 1,
    };
    Ok(CatalogFile { profiles, default_profile_id, next_id, catalog_revision })
}

/// Validate that a configured root can be created and written before the Host
/// reports readiness, so an unwritable root fails startup instead of later
/// reporting a false saved profile.
pub fn prepare_root(root: &Path) -> Result<(), CatalogError> {
    std::fs::create_dir_all(root).map_err(|error| CatalogError::io("create profile root", error))?;
    let probe = root.join(".write-probe");
    std::fs::write(&probe, b"ok").map_err(|error| CatalogError::io("profile root is not writable", error))?;
    std::fs::remove_file(&probe).map_err(|error| CatalogError::io("clean profile root probe", error))?;
    Ok(())
}
