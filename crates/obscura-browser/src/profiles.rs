pub struct BrowserProfile {
    pub user_agent: &'static str,
    pub platform: &'static str,
    pub ua_platform: &'static str,
    pub ua_platform_version: &'static str,
}

pub static PROFILES: &[BrowserProfile] = &[
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "13.6.7",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.4.1",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.5.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.6.0",
    },
];

/// Phone browser identity.
///
/// The pool above is desktop-only, so a phone client attached to the Host was
/// still served a Windows desktop identity. Responsive sites then rendered a
/// desktop layout inside a phone-sized viewport, which is exactly the
/// misalignment users reported on weibo. The identity has to follow the
/// attaching device, not the Host's build-time default.
pub static PHONE_PROFILE: BrowserProfile = BrowserProfile {
    user_agent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Mobile Safari/537.36",
    platform: "Linux armv8l",
    ua_platform: "Android",
    ua_platform_version: "14.0.0",
};

pub fn phone_profile() -> &'static BrowserProfile {
    &PHONE_PROFILE
}

/// Derive the platform surfaces for a caller-supplied UA string.
///
/// The profile pool only covers desktop identities, so a caller that pins a UA
/// (the Host, for one) would otherwise send an Android UA while
/// `navigator.platform` and the UA-CH platform fields still said Windows.
/// Keeping them consistent is the point of an identity, not a nicety: sites
/// branch on the whole bundle.
pub fn identity_for_user_agent(user_agent: &str) -> crate::context::BrowserIdentity {
    if user_agent.contains("Android") {
        let version = user_agent
            .split("Android ")
            .nth(1)
            .and_then(|rest| rest.split(|ch: char| !ch.is_ascii_digit() && ch != '.').next())
            .map(|version| format!("{}.0.0", version.split('.').next().unwrap_or("14")))
            .unwrap_or_else(|| "14.0.0".to_string());
        return crate::context::BrowserIdentity {
            user_agent: user_agent.to_string(),
            platform: "Linux armv8l".into(),
            ua_platform: "Android".into(),
            ua_platform_version: version,
        };
    }
    if user_agent.contains("Windows NT") {
        return crate::context::BrowserIdentity {
            user_agent: user_agent.to_string(),
            platform: "Win32".into(),
            ua_platform: "Windows".into(),
            ua_platform_version: "10.0.0".into(),
        };
    }
    if user_agent.contains("Macintosh") {
        return crate::context::BrowserIdentity {
            user_agent: user_agent.to_string(),
            platform: "MacIntel".into(),
            ua_platform: "macOS".into(),
            ua_platform_version: "14.6.0".into(),
        };
    }
    crate::context::BrowserIdentity {
        user_agent: user_agent.to_string(),
        platform: "Linux x86_64".into(),
        ua_platform: "Linux".into(),
        ua_platform_version: "6.5.0".into(),
    }
}

pub fn random_profile() -> &'static BrowserProfile {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize)
        % PROFILES.len();
    &PROFILES[idx]
}

/// Pick the profile for a new browser context.
///
/// The default is a single stable profile. Cycling through different browser
/// identities from one address is itself a bot signal (a real address maps to a
/// stable device), and the rotated profile does not yet carry a matching TLS or
/// timezone fingerprint, so rotation is opt-in:
///   OBSCURA_PROFILE=<index>   pin a specific profile from PROFILES
///   OBSCURA_ROTATE_PROFILE=1  pick a random profile per context
pub fn select_profile() -> &'static BrowserProfile {
    if let Some(idx) = std::env::var("OBSCURA_PROFILE")
        .ok()
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<usize>().ok())
    {
        if idx < PROFILES.len() {
            return &PROFILES[idx];
        }
    }
    if env_enabled("OBSCURA_ROTATE_PROFILE") {
        return random_profile();
    }
    &PROFILES[0]
}

fn env_enabled(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
