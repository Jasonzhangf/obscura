use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use obscura_net::{CookieJar, ObscuraHttpClient, RobotsCache};
use sha2::{Digest, Sha256};

/// Typed profile storage failure. The `code` is a stable Host error code so a
/// load/save/flush failure can travel worker -> Host without being converted
/// into an empty jar or a successful receipt.
#[derive(Debug, Clone)]
pub struct ProfileStorageError {
    pub code: &'static str,
    pub message: String,
}

impl ProfileStorageError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    fn io(context: &str, error: std::io::Error) -> Self {
        Self::new("PROFILE_STORAGE_FAILED", format!("{context}: {error}"))
    }
}

impl std::fmt::Display for ProfileStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProfileStorageError {}

/// One profile's shared, origin-partitioned localStorage owner.
///
/// All Pages in a `BrowserContext` share this instance. A single mutex
/// serializes snapshots so two tabs never race or overwrite each other's file,
/// and the on-disk layout is one `localStorage/<origin-key>.json` per origin.
pub struct SharedLocalStorage {
    dir: Option<PathBuf>,
    state: std::sync::Mutex<BTreeMap<String, BTreeMap<String, String>>>,
}

impl SharedLocalStorage {
    pub fn in_memory() -> Self {
        Self { dir: None, state: std::sync::Mutex::new(BTreeMap::new()) }
    }

    /// Load every persisted origin snapshot under `dir`. Any unreadable or
    /// malformed file is a typed initialization failure, never a silent empty
    /// store.
    pub fn load(dir: Option<PathBuf>) -> Result<Self, ProfileStorageError> {
        let mut state = BTreeMap::new();
        if let Some(dir) = &dir {
            if dir.exists() {
                let entries = std::fs::read_dir(dir)
                    .map_err(|error| ProfileStorageError::io("read local storage directory", error))?;
                for entry in entries {
                    let entry = entry
                        .map_err(|error| ProfileStorageError::io("read local storage directory entry", error))?;
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        return Err(ProfileStorageError::new(
                            "PROFILE_STORAGE_FAILED",
                            format!("local storage file name is not a valid origin encoding: {}", path.display()),
                        ));
                    };
                    if stem.is_empty() || !is_hex(stem) {
                        return Err(ProfileStorageError::new(
                            "PROFILE_STORAGE_FAILED",
                            format!("local storage file name is not a valid origin encoding: {}", path.display()),
                        ));
                    }
                    let data = std::fs::read_to_string(&path)
                        .map_err(|error| ProfileStorageError::io("read local storage file", error))?;
                    let parsed: BTreeMap<String, String> = serde_json::from_str(&data).map_err(|error| {
                        ProfileStorageError::new(
                            "PROFILE_STORAGE_FAILED",
                            format!("local storage {} is corrupt: {error}", path.display()),
                        )
                    })?;
                    state.insert(stem.to_string(), parsed);
                }
            }
        }
        Ok(Self { dir, state: std::sync::Mutex::new(state) })
    }

    pub fn configured(&self) -> bool { self.dir.is_some() }

    pub fn load_origin(&self, origin: &str) -> String {
        let guard = self.state.lock().expect("local storage owner");
        match guard.get(origin) {
            Some(map) => serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string()),
            None => "{}".to_string(),
        }
    }

    pub fn set(&self, origin: &str, key: &str, value: &str) {
        let mut guard = self.state.lock().expect("local storage owner");
        guard.entry(origin.to_string()).or_default().insert(key.to_string(), value.to_string());
    }

    pub fn remove(&self, origin: &str, key: &str) {
        let mut guard = self.state.lock().expect("local storage owner");
        if let Some(map) = guard.get_mut(origin) { map.remove(key); }
    }

    pub fn clear(&self, origin: &str) {
        let mut guard = self.state.lock().expect("local storage owner");
        guard.remove(origin);
    }

    /// Persist every authoritative origin snapshot atomically and remove owned
    /// origin snapshot files that are no longer authoritative, so a cleared or
    /// removed origin cannot be resurrected by a cold reload. A write or
    /// removal failure is returned so a close/flush never reports a false
    /// `saved`. Only owned snapshot files (`<hex-origin>.json`) are removed;
    /// unrelated files and the storage root are left untouched.
    pub fn flush(&self) -> Result<(), ProfileStorageError> {
        let Some(dir) = &self.dir else { return Ok(()); };
        std::fs::create_dir_all(dir).map_err(|error| ProfileStorageError::io("create local storage directory", error))?;
        // Hold the owner lock across the write/remove pass so a concurrent
        // set/clear cannot interleave with the authoritative snapshot.
        let guard = self.state.lock().expect("local storage owner");
        for (origin, map) in guard.iter() {
            let path = dir.join(format!("{origin}.json"));
            let json = serde_json::to_string_pretty(map)
                .map_err(|error| ProfileStorageError::new("PROFILE_STORAGE_FAILED", format!("serialize local storage for {origin}: {error}")))?;
            write_atomic(&path, json.as_bytes()).map_err(|error| ProfileStorageError::io("write local storage file", error))?;
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|error| ProfileStorageError::io("read local storage directory", error))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| ProfileStorageError::io("read local storage directory entry", error))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.is_empty() || !is_hex(stem) || guard.contains_key(stem) {
                continue;
            }
            std::fs::remove_file(&path)
                .map_err(|error| ProfileStorageError::io("remove obsolete local storage file", error))?;
        }
        Ok(())
    }
}

impl obscura_js::ops::LocalStorageBackend for SharedLocalStorage {
    fn load(&self, origin: &str) -> String { self.load_origin(origin) }
    fn set(&self, origin: &str, key: &str, value: &str) { SharedLocalStorage::set(self, origin, key, value); }
    fn remove(&self, origin: &str, key: &str) { SharedLocalStorage::remove(self, origin, key); }
    fn clear(&self, origin: &str) { SharedLocalStorage::clear(self, origin); }
}

/// localStorage partition key for a document URL.
///
/// Tuple origins persist their exact origin serialization, so real HTTP(S)
/// buckets keep the documented, human-checkable name. Opaque origins (for
/// example `data:`) serialize to `"null"`, and the full URL is unbounded, so
/// they persist under a digest: two different data URLs stay separate buckets
/// and the file name cannot exceed the path length limit.
pub fn storage_key(url: &url::Url) -> String {
    let origin = url.origin();
    if origin.is_tuple() {
        to_hex(origin.ascii_serialization().as_bytes())
    } else {
        let mut hasher = Sha256::new();
        hasher.update(url.as_str().as_bytes());
        to_hex(&hasher.finalize())
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_hex(text: &str) -> bool {
    text.len() % 2 == 0 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub struct BrowserContext {
    pub id: String,
    pub cookie_jar: Arc<CookieJar>,
    pub http_client: Arc<ObscuraHttpClient>,
    pub user_agent: String,
    pub platform: String,
    pub ua_platform: String,
    pub ua_platform_version: String,
    pub proxy_url: Option<String>,
    pub robots_cache: Arc<RobotsCache>,
    pub obey_robots: bool,
    pub stealth: bool,
    /// When true, CDP-driven navigation to file:// URLs is permitted.
    /// Default is false: a remote CDP client cannot point the browser
    /// at /etc/shadow even if Obscura is running as a privileged user.
    /// Flip on via `obscura serve --allow-file-access` for legitimate
    /// local-HTML testing workflows. The CLI's own `obscura fetch
    /// file://...` path is unaffected because it does not go through
    /// the CDP server.
    pub allow_file_access: bool,
    pub storage_dir: Option<PathBuf>,
    /// When true, the http client allows fetching localhost / RFC1918 /
    /// link-local addresses. Set via `--allow-private-network` (issue #33).
    /// Independent of `allow_file_access` because they cover different threat
    /// models: file:// is a local file-system read, while private-network is
    /// the broader SSRF gate from issue #4.
    pub allow_private_network: bool,
    /// Shared origin-partitioned localStorage owner for this profile. All Pages
    /// in the context share one instance.
    pub local_storage: Arc<SharedLocalStorage>,
}

impl BrowserContext {
    pub fn new(id: String) -> Self {
        Self::_new_inner(id, None, false, None, None, false)
    }

    /// Create a BrowserContext with an optional storage directory.
    /// When `storage_dir` is set, cookies are automatically loaded from
    /// `{storage_dir}/cookies.json` on creation.
    pub fn with_storage(
        id: String,
        storage_dir: Option<PathBuf>,
    ) -> Self {
        Self::_new_inner(id, None, false, None, storage_dir, false)
    }

    /// Create a BrowserContext with full options including storage_dir.
    pub fn with_storage_full(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, storage_dir, false)
    }

    /// Variant that also accepts the `allow_private_network` opt-in. All
    /// pre-existing constructors default it to `false`; callers that want the
    /// CLI's `--allow-private-network` (issue #33) behaviour go through here.
    pub fn with_storage_and_network(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, storage_dir, allow_private_network)
    }

    fn _new_inner(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        let cookie_jar = Arc::new(CookieJar::new());

        // Restore cookies from disk if storage_dir is configured
        if let Some(ref dir) = storage_dir {
            let cookie_path = dir.join("cookies.json");
            if cookie_path.exists() {
                match cookie_jar.load_from_file(&cookie_path) {
                    Ok(n) if n > 0 => {
                        tracing::info!("Loaded {} cookies from {}", n, cookie_path.display());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("Failed to load cookies from {}: {}", cookie_path.display(), e);
                    }
                }
            }
        }

        let mut client = ObscuraHttpClient::with_full_options(
            cookie_jar.clone(),
            proxy_url.as_deref(),
            allow_private_network,
        );
        if stealth {
            client.block_trackers = true;
        }
        let profile = crate::profiles::select_profile();
        let resolved_ua = user_agent.unwrap_or_else(|| profile.user_agent.to_string());
        let platform = profile.platform.to_string();
        let ua_platform = profile.ua_platform.to_string();
        let ua_platform_version = profile.ua_platform_version.to_string();
        // Sync the http client's UA at construction so navigation requests pick it
        // up before any async setup runs. The lock has no other holders here, so
        // try_write always succeeds; we fall back silently if it ever fails.
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = resolved_ua.clone();
        }
        let http_client = Arc::new(client);
        BrowserContext {
            id,
            cookie_jar,
            http_client,
            user_agent: resolved_ua,
            platform,
            ua_platform,
            ua_platform_version,
            proxy_url,
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: false,
            stealth,
            allow_file_access: false,
            storage_dir,
            allow_private_network,
            local_storage: Arc::new(SharedLocalStorage::in_memory()),
        }
    }

    /// Construct a context for one persistent profile directory.
    ///
    /// Cookie and localStorage load failures are typed and returned, so a
    /// corrupt or unreadable store is an explicit initialization failure
    /// instead of an empty jar. When `storage_dir` is `None` the context runs
    /// in memory only, matching the historical single-session behavior.
    pub fn with_profile_storage(
        id: String,
        storage_dir: Option<PathBuf>,
        stealth: bool,
        user_agent: Option<String>,
        allow_private_network: bool,
    ) -> Result<Self, ProfileStorageError> {
        if let Some(dir) = &storage_dir {
            std::fs::create_dir_all(dir).map_err(|error| ProfileStorageError::io("create profile directory", error))?;
        }
        let cookie_jar = Arc::new(CookieJar::new());
        if let Some(dir) = &storage_dir {
            let cookie_path = dir.join("cookies.json");
            if cookie_path.exists() {
                cookie_jar.load_from_file(&cookie_path).map_err(|error| {
                    ProfileStorageError::new("PROFILE_STORAGE_FAILED", format!("load cookies {}: {error}", cookie_path.display()))
                })?;
            }
        }
        let local_storage = SharedLocalStorage::load(storage_dir.as_ref().map(|dir| dir.join("localStorage")))?;

        let mut client = ObscuraHttpClient::with_full_options(
            cookie_jar.clone(),
            None,
            allow_private_network,
        );
        if stealth {
            client.block_trackers = true;
        }
        let profile = crate::profiles::select_profile();
        let resolved_ua = user_agent.unwrap_or_else(|| profile.user_agent.to_string());
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = resolved_ua.clone();
        }
        Ok(BrowserContext {
            id,
            cookie_jar,
            http_client: Arc::new(client),
            user_agent: resolved_ua,
            platform: profile.platform.to_string(),
            ua_platform: profile.ua_platform.to_string(),
            ua_platform_version: profile.ua_platform_version.to_string(),
            proxy_url: None,
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: false,
            stealth,
            allow_file_access: false,
            storage_dir,
            allow_private_network,
            local_storage: Arc::new(local_storage),
        })
    }

    pub fn with_options(id: String, proxy_url: Option<String>, stealth: bool) -> Self {
        Self::with_full_options(id, proxy_url, stealth, None)
    }

    pub fn with_full_options(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, None, false)
    }

    pub fn with_proxy(id: String, proxy_url: Option<String>) -> Self {
        Self::with_options(id, proxy_url, false)
    }

    /// Create a context with the same browser configuration but independent
    /// mutable network state. Persistent copies start with the template's
    /// current cookies; incognito copies start empty and never write to the
    /// template's storage directory.
    pub fn isolated_copy(&self, id: String, persistent: bool) -> Self {
        let cookie_jar = Arc::new(CookieJar::new());
        if persistent {
            cookie_jar.set_cookies_from_cdp(self.cookie_jar.get_all_cookies());
        }

        let mut client = ObscuraHttpClient::with_full_options(
            cookie_jar.clone(),
            self.proxy_url.as_deref(),
            self.allow_private_network,
        );
        if self.stealth {
            client.block_trackers = true;
        }
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = self.user_agent.clone();
        }

        BrowserContext {
            id,
            cookie_jar,
            http_client: Arc::new(client),
            user_agent: self.user_agent.clone(),
            platform: self.platform.clone(),
            ua_platform: self.ua_platform.clone(),
            ua_platform_version: self.ua_platform_version.clone(),
            proxy_url: self.proxy_url.clone(),
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: self.obey_robots,
            stealth: self.stealth,
            allow_file_access: self.allow_file_access,
            storage_dir: persistent.then(|| self.storage_dir.clone()).flatten(),
            allow_private_network: self.allow_private_network,
            local_storage: Arc::new(SharedLocalStorage::in_memory()),
        }
    }

    /// Persist cookies to disk if storage_dir is configured.
    /// Called during graceful shutdown.
    pub fn save_cookies(&self) {
        if let Some(ref dir) = self.storage_dir {
            let _ = std::fs::create_dir_all(dir);
            let cookie_path = dir.join("cookies.json");
            if let Err(e) = self.cookie_jar.save_to_file(&cookie_path) {
                tracing::warn!("Failed to save cookies to {}: {}", cookie_path.display(), e);
            } else {
                tracing::info!("Saved cookies to {}", cookie_path.display());
            }
        }
    }

    /// Persist cookies and the shared localStorage owner for this profile.
    /// Any failure is returned so a close/flush never reports a false `saved`.
    pub fn save_profile_storage(&self) -> Result<(), ProfileStorageError> {
        if let Some(dir) = &self.storage_dir {
            std::fs::create_dir_all(dir).map_err(|error| ProfileStorageError::io("create profile directory", error))?;
            let cookie_path = dir.join("cookies.json");
            self.cookie_jar.save_to_file(&cookie_path).map_err(|error| {
                ProfileStorageError::new("PROFILE_STORAGE_FAILED", format!("save cookies {}: {error}", cookie_path.display()))
            })?;
        }
        self.local_storage.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_propagates_user_agent_to_http_client() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            Some("Custom-UA/1.0".to_string()),
        );
        assert_eq!(ctx.user_agent, "Custom-UA/1.0");
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert_eq!(client_ua, "Custom-UA/1.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_falls_back_to_chrome_default() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            None,
        );
        assert!(ctx.user_agent.contains("Chrome"));
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert!(client_ua.contains("Chrome"));
        assert_eq!(ctx.user_agent, client_ua);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_options_keeps_default_user_agent() {
        let ctx = BrowserContext::with_options("test".to_string(), None, false);
        assert!(ctx.user_agent.contains("Chrome"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn isolated_copy_does_not_share_mutable_network_state() {
        let source = BrowserContext::with_full_options(
            "source".to_string(),
            None,
            false,
            Some("Template-UA/1.0".to_string()),
        );
        source.cookie_jar.set_cookie("sid=source", &url::Url::parse("https://example.com").unwrap());

        let persistent = source.isolated_copy("persistent".to_string(), true);
        let incognito = source.isolated_copy("incognito".to_string(), false);

        assert_eq!(persistent.cookie_jar.get_all_cookies().len(), 1);
        assert!(incognito.cookie_jar.get_all_cookies().is_empty());
        persistent.cookie_jar.clear();
        persistent.http_client.set_user_agent("Changed-UA/2.0").await;

        assert_eq!(source.cookie_jar.get_all_cookies().len(), 1);
        assert_eq!(source.http_client.user_agent.read().await.as_str(), "Template-UA/1.0");
    }
}
