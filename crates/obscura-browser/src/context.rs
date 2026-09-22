use std::path::{Path, PathBuf};
use std::sync::Arc;

use obscura_net::{CookieJar, ObscuraHttpClient, RobotsCache};

/// One browser identity: the UA string plus the platform surfaces that must
/// agree with it. `navigator.platform` and the UA-CH platform fields are part
/// of the same statement as the UA, so they travel together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserIdentity {
    pub user_agent: String,
    pub platform: String,
    pub ua_platform: String,
    pub ua_platform_version: String,
}

impl BrowserIdentity {
    fn from_profile(profile: &crate::profiles::BrowserProfile) -> Self {
        Self {
            user_agent: profile.user_agent.to_string(),
            platform: profile.platform.to_string(),
            ua_platform: profile.ua_platform.to_string(),
            ua_platform_version: profile.ua_platform_version.to_string(),
        }
    }

    /// Desktop identity for a client that attaches as `Device::Desktop`.
    pub fn desktop() -> Self {
        Self::from_profile(
            crate::profiles::PROFILES
                .iter()
                .find(|profile| profile.platform == "MacIntel")
                .expect("macOS profile is part of the browser identity catalog"),
        )
    }

    /// Identity for a client that attaches as `Device::Phone`.
    pub fn phone() -> Self {
        Self::from_profile(crate::profiles::phone_profile())
    }
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
}

fn storage_origin_key(url: &url::Url) -> Option<String> {
    url.origin().is_tuple().then(|| url.origin().ascii_serialization())
}

fn local_storage_path(storage_dir: &Path, url: &url::Url) -> Option<PathBuf> {
    let origin = storage_origin_key(url)?;
    let mut key = String::with_capacity(origin.len() * 2);
    for byte in origin.bytes() {
        use std::fmt::Write as _;
        write!(&mut key, "{byte:02x}").expect("writing to String cannot fail");
    }
    Some(storage_dir.join("local-storage").join(format!("{key}.json")))
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

    /// Create a context under an explicit identity. The Host uses this to open
    /// a Session behind the identity of whichever device holds it.
    pub fn with_identity(
        id: String,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
        identity: BrowserIdentity,
    ) -> Self {
        Self::_new_inner_with_identity(id, None, false, storage_dir, allow_private_network, identity)
    }

    /// Rebind UA and platform surfaces to another identity. Only the Page owner
    /// may call this: it decides when the live document's JS surfaces are
    /// updated and when the new identity first applies to a navigation.
    pub(crate) async fn replace_identity(&mut self, identity: BrowserIdentity) {
        self.user_agent = identity.user_agent;
        self.platform = identity.platform;
        self.ua_platform = identity.ua_platform;
        self.ua_platform_version = identity.ua_platform_version;
        self.http_client.set_user_agent(&self.user_agent).await;
    }

    fn _new_inner(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        let profile = crate::profiles::select_profile();
        let identity = match user_agent {
            // A caller-supplied UA is authoritative for that whole identity:
            // an Android UA must not keep a desktop `navigator.platform`.
            Some(user_agent) => crate::profiles::identity_for_user_agent(&user_agent),
            None => BrowserIdentity::from_profile(profile),
        };
        Self::_new_inner_with_identity(id, proxy_url, stealth, storage_dir, allow_private_network, identity)
    }

    fn _new_inner_with_identity(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
        identity: BrowserIdentity,
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
        // Sync the http client's UA at construction so navigation requests pick it
        // up before any async setup runs. The lock has no other holders here, so
        // try_write always succeeds; we fall back silently if it ever fails.
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = identity.user_agent.clone();
        }
        let http_client = Arc::new(client);
        BrowserContext {
            id,
            cookie_jar,
            http_client,
            user_agent: identity.user_agent,
            platform: identity.platform,
            ua_platform: identity.ua_platform,
            ua_platform_version: identity.ua_platform_version,
            proxy_url,
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: false,
            stealth,
            allow_file_access: false,
            storage_dir,
            allow_private_network,
        }
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

    pub fn load_local_storage(
        &self,
        url: &url::Url,
    ) -> Result<std::collections::BTreeMap<String, String>, String> {
        let Some(path) = self
            .storage_dir
            .as_deref()
            .and_then(|dir| local_storage_path(dir, url))
        else {
            return Ok(std::collections::BTreeMap::new());
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(std::collections::BTreeMap::new());
            }
            Err(error) => {
                return Err(format!(
                    "read localStorage {}: {error}",
                    path.display()
                ));
            }
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse localStorage {}: {error}", path.display()))
    }

    pub fn save_local_storage(
        &self,
        url: &url::Url,
        values: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        let Some(path) = self
            .storage_dir
            .as_deref()
            .and_then(|dir| local_storage_path(dir, url))
        else {
            return Ok(());
        };
        let parent = path.parent().ok_or_else(|| format!("invalid localStorage path: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec(values)
            .map_err(|error| format!("encode {}: {error}", path.display()))?;
        std::fs::write(&temporary, bytes)
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("rename {} -> {}: {error}", temporary.display(), path.display()))
    }
}

#[cfg(test)]
mod local_storage_tests {
    use super::*;

    #[test]
    fn origin_keys_do_not_collide_after_encoding() {
        let first = url::Url::parse("https://a-b.example/").unwrap();
        let second = url::Url::parse("https://a_b.example/").unwrap();
        let first = local_storage_path(Path::new("/profile"), &first).unwrap();
        let second = local_storage_path(Path::new("/profile"), &second).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn a_corrupt_local_storage_file_fails_instead_of_loading_empty() {
        let dir = std::path::PathBuf::from("/tmp").join(format!(
            "obscura-ls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let url = url::Url::parse("https://example.test/").unwrap();
        let path = local_storage_path(&dir, &url).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let context = BrowserContext::with_storage("corrupt".to_string(), Some(dir.clone()));
        let error = context
            .load_local_storage(&url)
            .expect_err("corrupt profile must not load as empty");
        assert!(error.contains("parse localStorage"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
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
    async fn phone_identity_matches_its_user_agent_on_every_surface() {
        let identity = BrowserIdentity::phone();
        let ctx = BrowserContext::with_identity("phone".to_string(), None, false, identity.clone());
        // A site sees one statement, not a UA that contradicts navigator.platform.
        assert!(ctx.user_agent.contains("Android"));
        assert_eq!(ctx.platform, "Linux armv8l");
        assert_eq!(ctx.ua_platform, "Android");
        assert_eq!(ctx.http_client.user_agent.read().await.as_str(), identity.user_agent);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replace_identity_rebinds_user_agent_and_platform_together() {
        let mut ctx = BrowserContext::with_identity("switch".to_string(), None, false, BrowserIdentity::desktop());
        assert!(ctx.user_agent.contains("Macintosh"));
        ctx.replace_identity(BrowserIdentity::phone()).await;
        assert!(ctx.user_agent.contains("Android"), "{}", ctx.user_agent);
        assert_eq!(ctx.platform, "Linux armv8l");
        assert_eq!(ctx.ua_platform, "Android");
        assert!(ctx.http_client.user_agent.read().await.contains("Android"));
    }

    #[test]
    fn caller_supplied_user_agent_does_not_keep_a_desktop_platform() {
        let ctx = BrowserContext::with_full_options(
            "derived".to_string(),
            None,
            false,
            Some("Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Mobile Safari/537.36".to_string()),
        );
        assert_eq!(ctx.platform, "Linux armv8l");
        assert_eq!(ctx.ua_platform, "Android");
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
