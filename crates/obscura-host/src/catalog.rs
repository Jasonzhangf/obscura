//! Typed persistent profile catalog owned by the Host.
//!
//! The catalog persists profile id, display name and the default identity under
//! an explicit profile root. It is typed configuration, never reconstructed by
//! directory scanning or log parsing. When no root is configured the Host runs
//! one in-memory profile and the catalog is not persistent.
use std::path::{Path, PathBuf};

use obscura_host_protocol::{ProfileCatalog, ProfileRecord};

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

pub struct ProfileCatalogStore {
    root: Option<PathBuf>,
    profiles: Vec<ProfileRecord>,
    default_profile_id: Option<String>,
    next_id: u64,
}

impl ProfileCatalogStore {
    /// Load the catalog for the configured root. A fresh root creates an empty
    /// default profile; an existing root must contain a readable, well-formed
    /// catalog. A malformed catalog is a typed failure, never an empty success.
    pub fn load(root: Option<PathBuf>) -> Result<Self, CatalogError> {
        match root {
            None => Ok(Self {
                root: None,
                profiles: vec![ProfileRecord { profile_id: "default".to_string(), display_name: "Default".to_string(), default: true }],
                default_profile_id: Some("default".to_string()),
                next_id: 1,
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
                    Ok(Self { root: Some(root), profiles: parsed.profiles, default_profile_id: parsed.default_profile_id, next_id: parsed.next_id })
                } else {
                    let mut store = Self {
                        root: Some(root),
                        profiles: vec![ProfileRecord { profile_id: "default".to_string(), display_name: "Default".to_string(), default: true }],
                        default_profile_id: Some("default".to_string()),
                        next_id: 1,
                    };
                    store.persist()?;
                    Ok(store)
                }
            }
        }
    }

    pub fn root_configured(&self) -> bool { self.root.is_some() }

    pub fn catalog(&self) -> ProfileCatalog {
        ProfileCatalog {
            profiles: self.profiles.clone(),
            default_profile_id: self.default_profile_id.clone(),
            root_configured: self.root.is_some(),
        }
    }

    pub fn record(&self, profile_id: &str) -> Option<&ProfileRecord> {
        self.profiles.iter().find(|profile| profile.profile_id == profile_id)
    }

    pub fn default_id(&self) -> Option<String> {
        self.default_profile_id.clone().or_else(|| self.profiles.first().map(|profile| profile.profile_id.clone()))
    }

    /// Directory allocated to a profile. Only the Host derives this; it never
    /// comes from the wire.
    pub fn profile_dir(&self, profile_id: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|root| root.join("profiles").join(profile_id))
    }

    /// Create a new empty profile and persist the catalog before returning. A
    /// persistence failure means the profile was not created.
    pub fn create(&mut self, display_name: &str, make_default: bool) -> Result<ProfileRecord, CatalogError> {
        let name = display_name.trim();
        if name.is_empty() {
            return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "display name must not be empty"));
        }
        if name.chars().count() > 64 {
            return Err(CatalogError::new("PROFILE_CATALOG_FAILED", "display name must be at most 64 characters"));
        }
        let profile_id = format!("p{}", self.next_id);
        let record = ProfileRecord { profile_id: profile_id.clone(), display_name: name.to_string(), default: make_default };
        let previous_profiles = self.profiles.clone();
        let previous_default = self.default_profile_id.clone();
        let previous_next = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        if make_default {
            for profile in &mut self.profiles { profile.default = false; }
            self.default_profile_id = Some(profile_id);
        }
        self.profiles.push(record.clone());
        if let Err(error) = self.persist() {
            self.profiles = previous_profiles;
            self.default_profile_id = previous_default;
            self.next_id = previous_next;
            return Err(error);
        }
        Ok(record)
    }

    fn persist(&self) -> Result<(), CatalogError> {
        let Some(root) = &self.root else { return Ok(()); };
        std::fs::create_dir_all(root).map_err(|error| CatalogError::io("create profile root", error))?;
        let profiles: Vec<serde_json::Value> = self.profiles.iter().map(|profile| {
            serde_json::json!({
                "profile_id": profile.profile_id,
                "display_name": profile.display_name,
                "default": profile.default,
            })
        }).collect();
        let json = serde_json::to_string_pretty(&serde_json::json!({
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

struct CatalogFile {
    profiles: Vec<ProfileRecord>,
    default_profile_id: Option<String>,
    next_id: u64,
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
        profiles.push(ProfileRecord {
            profile_id: profile_id.to_string(),
            display_name: display_name.to_string(),
            default: is_default,
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
    Ok(CatalogFile { profiles, default_profile_id, next_id })
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
