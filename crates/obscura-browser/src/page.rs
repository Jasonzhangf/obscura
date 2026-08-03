use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use obscura_dom::{parse_html, DomTree, NodeData, NodeId};
use obscura_js::runtime::ObscuraJsRuntime;
use obscura_net::{CallbackRegistry, ObscuraHttpClient, ObscuraNetError, RequestCallback, Response, ResponseCallback};
use url::Url;

use crate::context::BrowserContext;
use crate::lifecycle::LifecycleState;

/// Parse `OBSCURA_GEOLOCATION="lat,lon"` for the navigator.geolocation shim.
/// Returns None when unset or malformed, leaving the built-in default in place.
/// Lets a deployment align the reported coordinates with the region its exit IP
/// resolves to, so timezone and location stay consistent (issue #228).
fn env_geolocation() -> Option<(f64, f64)> {
    let raw = std::env::var("OBSCURA_GEOLOCATION").ok()?;
    let (lat, lon) = raw.split_once(',')?;
    let lat: f64 = lat.trim().parse().ok()?;
    let lon: f64 = lon.trim().parse().ok()?;
    let valid = lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon);
    valid.then_some((lat, lon))
}

fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    if meta.split(';').any(|t| t.eq_ignore_ascii_case("base64")) {
        let cleaned: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        BASE64.decode(cleaned).ok()
    } else {
        Some(percent_decode(payload))
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = hex_val(b[i + 1]);
            let lo = hex_val(b[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character.
/// `&s[..max]` panics if `max` lands inside a multi-byte char; the evaluated
/// expression logged below is caller-controlled, so slice it safely.
/// (`str::floor_char_boundary` would do this but is still unstable.)
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(feature = "stealth")]
use obscura_net::StealthHttpClient;

/// Returns true when a JS-initiated navigation would step from a
/// non-file scheme into a file: URL. We treat that move as an SOP
/// violation because the existing realm survives the navigation and
/// can read the new document's body.
fn cross_scheme_to_file(from: &str, to: &str) -> bool {
    let to_is_file = Url::parse(to)
        .map(|u| u.scheme().eq_ignore_ascii_case("file"))
        .unwrap_or(false);
    if !to_is_file {
        return false;
    }
    Url::parse(from)
        .map(|u| !u.scheme().eq_ignore_ascii_case("file"))
        .unwrap_or(true)
}

/// Sub-resource fetch policy. http(s) is always fine; data: is allowed
/// because the bytes are inline in the URI (no network fetch, no SSRF);
/// file: is only allowed when the page itself was loaded from file:;
/// everything else (javascript:, chrome:, etc) is blocked.
/// Real Chrome allows data: subresources by default; Instagram and most
/// Meta properties depend on this for their inline bootstrap scripts.
fn subresource_allowed(page_url: Option<&Url>, resource: &str) -> bool {
    let Ok(target) = Url::parse(resource) else { return false };
    let scheme = target.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "data" => true,
        "file" => page_url.map(|u| u.scheme().eq_ignore_ascii_case("file")).unwrap_or(false),
        _ => false,
    }
}

/// Escape a value for safe inclusion inside a JavaScript template
/// literal. The previous implementation only escaped `\`, `` ` `` and
/// `${`; that left U+2028 / U+2029 (the JS-specific line terminators)
/// and other control characters as breakout vectors. Done at the
/// callsite means future tweaks come back to one function.
fn escape_for_js_template_literal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '$' => out.push_str("\\$"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '\u{0000}' => out.push_str("\\0"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct NetworkEvent {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub response_headers: Arc<std::collections::HashMap<String, String>>,
    pub body_size: usize,
    pub timestamp: f64,
}

#[derive(Debug, Clone)]
pub struct StoredResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

pub struct Page {
    pub id: String,
    pub frame_id: String,
    pub url: Option<Url>,
    pub dom: Option<DomTree>,
    pub js: Option<ObscuraJsRuntime>,
    pub lifecycle: LifecycleState,
    pub http_client: Arc<ObscuraHttpClient>,
    pub context: Arc<BrowserContext>,
    pub title: String,
    /// CSS viewport used by responsive page JavaScript and CDP screenshots.
    /// The physical `screen` fingerprint remains independent.
    pub viewport: (f32, f32),
    /// WHATWG canonical name of the current document's character encoding
    /// (e.g. "UTF-8", "EUC-JP"), detected when the response body is decoded.
    /// Exposed to JS as `document.characterSet` and used for the URL query
    /// encoding override on `<a>`/`<area>` hrefs in legacy-charset documents.
    pub encoding: String,
    /// Navigation history for Page.getNavigationHistory / navigateToHistoryEntry.
    /// Entries are URLs in visit order; `history_index` is the current position.
    /// Pushed on every successful navigation; truncated on goBack -> new nav.
    pub history: Vec<String>,
    pub history_index: usize,
    pub network_events: Vec<NetworkEvent>,
    response_bodies: std::collections::HashMap<String, StoredResponseBody>,
    response_body_order: std::collections::VecDeque<String>,
    network_event_counter: u32,
    pub intercept_enabled: bool,
    pub intercept_block_patterns: Vec<String>,
    pub blocked_url_patterns: Vec<String>,
    intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<obscura_js::ops::InterceptedRequest>>,
    // Scripts to execute in the page's JS context BEFORE any of the page's
    // own scripts run — the CDP `Page.addScriptToEvaluateOnNewDocument`
    // contract. Includes `Runtime.addBinding` shims so puppeteer's
    // `exposeFunction` bindings exist before inline `<script>` tags execute.
    preload_scripts: Vec<String>,
    /// Document-owned HTML script preparation flags saved while the V8 realm
    /// is suspended for CDP/MCP tab switching.  These are restored only when
    /// the same surviving DomTree is resumed; navigation clears them.
    suspended_started_script_ids: Vec<u32>,
    /// Passive on_request/on_response callbacks, scoped to this page (issue
    /// #408): they fire only for requests this page drives and die with it.
    /// Arc because the JS runtime state holds a second handle for fetch()/XHR.
    callbacks: Arc<CallbackRegistry>,
    #[cfg(feature = "stealth")]
    pub stealth_client: Option<Arc<StealthHttpClient>>,
}

/// Fetch an external stylesheet and inline any `@import`ed sheets it references,
/// recursively, so the imported CSS actually reaches the cascade. Sphinx docs
/// (and other older/theme-chained sites) ship the real layout in a base sheet
/// pulled in with `@import url("basic.css")` from the linked `classic.css`;
/// dropping it left the page effectively unstyled. Imports resolve against the
/// importing sheet's own URL and are inlined before its rules (CSS orders every
/// `@import` ahead of the sheet's other statements). A depth cap guards against
/// import cycles.
fn fetch_css_with_imports(
    client: Arc<ObscuraHttpClient>,
    url: String,
    depth: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> {
    Box::pin(async move {
        let parsed = url::Url::parse(&url).ok()?;
        let resp = tokio::time::timeout(tokio::time::Duration::from_secs(5), client.fetch(&parsed))
            .await
            .ok()?
            .ok()?;
        let css = obscura_net::decode_non_html(&resp.body, resp.content_type());
        // Stop recursing at the cap but keep this level's CSS as-is.
        if depth >= 4 {
            return Some(rebase_css_urls(&css, &parsed));
        }
        let (imports, stripped) = split_css_imports(&css);
        if imports.is_empty() {
            return Some(rebase_css_urls(&css, &parsed));
        }
        let mut out = String::new();
        for imp in imports {
            if let Ok(iu) = parsed.join(&imp) {
                if let Some(sub) = fetch_css_with_imports(client.clone(), iu.to_string(), depth + 1).await {
                    out.push_str(&sub);
                    out.push('\n');
                }
            }
        }
        out.push_str(&rebase_css_urls(&stripped, &parsed));
        Some(out)
    })
}

/// Preserve the URL base of a fetched stylesheet after it is materialized as
/// inline CSS. Relative `url(...)` values resolve against the stylesheet's
/// URL in browsers, not the document URL; failing to rebase them drops common
/// background, mask, cursor, and font assets from nested theme directories.
fn rebase_css_urls(css: &str, base: &url::Url) -> String {
    let mut out = String::with_capacity(css.len());
    let mut index = 0usize;
    while index < css.len() {
        let rest = &css[index..];
        if rest.starts_with("/*") {
            if let Some(end) = rest[2..].find("*/") {
                let length = end + 4;
                out.push_str(&rest[..length]);
                index += length;
            } else {
                out.push_str(rest);
                break;
            }
            continue;
        }
        let Some(first) = rest.chars().next() else { break };
        if first == '"' || first == '\'' {
            let quote = first;
            let mut escaped = false;
            let mut length = quote.len_utf8();
            for ch in rest[quote.len_utf8()..].chars() {
                length += ch.len_utf8();
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    break;
                }
            }
            out.push_str(&rest[..length]);
            index += length;
            continue;
        }
        let is_url = rest
            .get(..4)
            .map_or(false, |prefix| prefix.eq_ignore_ascii_case("url("));
        if !is_url {
            out.push(first);
            index += first.len_utf8();
            continue;
        }

        let mut quote = None;
        let mut escaped = false;
        let mut end = None;
        for (offset, ch) in rest[4..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            match quote {
                Some(open) if ch == open => quote = None,
                Some(_) => {}
                None if ch == '"' || ch == '\'' => quote = Some(ch),
                None if ch == ')' => {
                    end = Some(4 + offset);
                    break;
                }
                None => {}
            }
        }
        let Some(end) = end else {
            out.push_str(rest);
            break;
        };
        let raw = rest[4..end].trim();
        let value = if raw.len() >= 2
            && ((raw.starts_with('"') && raw.ends_with('"'))
                || (raw.starts_with('\'') && raw.ends_with('\'')))
        {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        let resolved = if value.is_empty()
            || value.starts_with('#')
            || value.contains("var(")
            || url::Url::parse(value).is_ok()
        {
            None
        } else {
            base.join(value).ok().map(|url| url.to_string())
        };
        if let Some(resolved) = resolved {
            out.push_str("url(\"");
            for ch in resolved.chars() {
                if ch == '\\' || ch == '"' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push_str("\")");
        } else {
            out.push_str(&rest[..=end]);
        }
        index += end + 1;
    }
    out
}

/// Pull leading `@import` rules out of a stylesheet. Returns each import target
/// URL (skipping print-only and `prefers-color-scheme: dark` conditional
/// imports, which must not apply in our light desktop context) plus the CSS
/// with those `@import` statements removed. Handles `@import "x.css";`,
/// `@import url("x.css");`, `@import url(x.css);` and an optional trailing
/// media query.
fn split_css_imports(css: &str) -> (Vec<String>, String) {
    let mut urls = Vec::new();
    let mut stripped = String::with_capacity(css.len());
    let mut rest = css;
    loop {
        let Some(pos) = rest.find("@import") else {
            stripped.push_str(rest);
            break;
        };
        // Real sheets place `@import` at the top (after an optional @charset), so
        // scanning for it anywhere is safe in practice and tolerates minified
        // whitespace. Text before this match carries through unchanged.
        stripped.push_str(&rest[..pos]);
        let after = &rest[pos + "@import".len()..];
        let Some(semi) = after.find(';') else {
            // Malformed; keep the remainder verbatim.
            stripped.push_str(&rest[pos..]);
            break;
        };
        let stmt = &after[..semi];
        if let Some(target) = parse_import_url(stmt) {
            urls.push(target);
        } else {
            // Could not parse a URL; preserve the statement so we don't lose it.
            stripped.push_str("@import");
            stripped.push_str(&after[..=semi]);
        }
        rest = &after[semi + 1..];
    }
    (urls, stripped)
}

/// Extract the URL from an `@import` statement body (the text between `@import`
/// and `;`), or `None` when the import is media-gated to print / dark and must
/// be skipped.
fn parse_import_url(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    let is_url_fn = s.len() >= 4 && s[..4].eq_ignore_ascii_case("url(");
    let (url, media) = if is_url_fn {
        let rest = &s[4..];
        let end = rest.find(')')?;
        let inner = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        (inner.to_string(), rest[end + 1..].trim())
    } else {
        let quote = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &s[1..];
        let end = rest.find(quote)?;
        (rest[..end].to_string(), rest[end + 1..].trim())
    };
    if !media.is_empty() {
        let compact: String = media.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect();
        let print_only = compact.contains("print") && !compact.contains("screen") && !compact.contains("all");
        if print_only || compact.contains("prefers-color-scheme:dark") {
            return None;
        }
    }
    if url.is_empty() {
        return None;
    }
    Some(url)
}

/// Materialize a fetched linked sheet immediately after its source `<link>`.
///
/// Keeping each sheet at its document position matters when linked and inline
/// author sheets are interleaved. Appending one aggregate `<style>` to `<head>`
/// makes every external rule later than every inline rule, which changes the
/// CSS cascade even when the external fetches themselves complete in order.
fn materialize_linked_stylesheet_script(link_index: usize, css: &str) -> String {
    let escaped_css = escape_for_js_template_literal(css);
    format!(
        r#"(function() {{
            var links = document.querySelectorAll('link[rel~="stylesheet"]');
            var link = links[{link_index}];
            if (!link || !link.parentNode) return;
            var style = null;
            function effectiveMedia() {{
                // Until the generic Element shim reflects HTMLLinkElement.media,
                // `this.media = "all"` creates an own property while the parsed
                // media="print" attribute remains unchanged.
                if (Object.prototype.hasOwnProperty.call(link, 'media')) {{
                    return String(link.media || '');
                }}
                return link.getAttribute('media') || '';
            }}
            function applies() {{
                var media = effectiveMedia().trim();
                if (!media) return true;
                try {{ return window.matchMedia(media).matches; }}
                catch (_) {{ return false; }}
            }}
            function syncSheet() {{
                var enabled = link.parentNode
                    && !link.disabled
                    && !link.hasAttribute('disabled')
                    && applies();
                if (!enabled) {{
                    if (style && style.parentNode) style.parentNode.removeChild(style);
                    return;
                }}
                if (!style) {{
                    style = document.createElement('style');
                    style.setAttribute('data-obscura-external-stylesheets', '');
                    style.textContent = `{escaped_css}`;
                }}
                if (!style.parentNode) {{
                    link.parentNode.insertBefore(style, link.nextSibling);
                }}
            }}

            // A non-matching sheet still loads and fires its event. Its handler
            // may then make the sheet applicable (the common
            // media=print/onload="this.media='all'" async-CSS pattern).
            syncSheet();
            try {{ link.dispatchEvent(new Event('load')); }}
            finally {{ syncSheet(); }}
        }})()"#
    )
}

/// Discover linked author sheets in document order.
///
/// Media queries control whether a loaded sheet participates in the cascade;
/// they do not suppress its fetch or `load` event. Keep the index among all
/// stylesheet links so the materialization script addresses the same node.
fn linked_stylesheet_requests(dom: &DomTree) -> Vec<(usize, String)> {
    let link_ids = dom
        .query_selector_all("link[rel~=\"stylesheet\"]")
        .unwrap_or_default();
    let mut links = Vec::new();
    for (link_index, lid) in link_ids.into_iter().enumerate() {
        if let Some(node) = dom.get_node(lid) {
            // Disabled alternate sheets remain dormant until script enables
            // them. Media-gated sheets are different: they still load.
            if node.get_attribute("disabled").is_some() {
                continue;
            }
            if let Some(href) = node.get_attribute("href") {
                links.push((link_index, href.to_string()));
            }
        }
    }
    links
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HydrationStats {
    elements: usize,
    text_chars: usize,
}

struct HydrationSnapshot {
    body_html: String,
    stats: HydrationStats,
}

fn capture_hydration_snapshot(js: &ObscuraJsRuntime) -> Option<HydrationSnapshot> {
    js.with_dom(|dom| {
        let body = dom.query_selector("body").ok().flatten()?;
        Some(HydrationSnapshot {
            body_html: dom.inner_html(body),
            stats: hydration_stats(dom, body),
        })
    })
    .flatten()
}

fn capture_hydration_stats(js: &ObscuraJsRuntime) -> Option<HydrationStats> {
    js.with_dom(|dom| {
        let body = dom.query_selector("body").ok().flatten()?;
        Some(hydration_stats(dom, body))
    })
    .flatten()
}

fn hydration_stats(dom: &DomTree, body: NodeId) -> HydrationStats {
    let mut elements = 0;
    let mut text_chars = 0;

    for node_id in dom.descendants(body) {
        let Some(node) = dom.get_node(node_id) else {
            continue;
        };
        match &node.data {
            NodeData::Element { name, .. } => {
                if !matches!(
                    name.local.as_ref(),
                    "script" | "style" | "link" | "meta" | "template" | "noscript"
                ) {
                    elements += 1;
                }
            }
            NodeData::Text { contents } => {
                if !has_non_content_ancestor(dom, node.parent, body) {
                    text_chars += contents.chars().filter(|c| !c.is_whitespace()).count();
                }
            }
            _ => {}
        }
    }

    HydrationStats {
        elements,
        text_chars,
    }
}

fn has_non_content_ancestor(
    dom: &DomTree,
    mut current: Option<NodeId>,
    body: NodeId,
) -> bool {
    while let Some(node_id) = current {
        if node_id == body {
            return false;
        }
        let Some(node) = dom.get_node(node_id) else {
            return false;
        };
        if node
            .as_element()
            .is_some_and(|name| matches!(name.local.as_ref(), "script" | "style" | "template" | "noscript"))
        {
            return true;
        }
        current = node.parent;
    }
    false
}

/// A server-rendered body should only be restored for an unmistakable wipe:
/// a substantial document lost at least seven eighths of its real elements
/// and visible text, leaving almost no content. This deliberately does not
/// second-guess ordinary framework rerenders or client-only application shells.
fn should_restore_hydration_snapshot(
    before: HydrationStats,
    after: HydrationStats,
) -> bool {
    before.elements >= 40
        && before.text_chars >= 200
        && after.elements <= 12
        && after.text_chars <= 128
        && after.elements.saturating_mul(8) < before.elements
        && after.text_chars.saturating_mul(8) < before.text_chars
}

fn restore_hydration_snapshot_if_needed(
    js: &ObscuraJsRuntime,
    before: HydrationSnapshot,
) -> bool {
    let Some(after) = capture_hydration_stats(js) else {
        return false;
    };
    if !should_restore_hydration_snapshot(before.stats, after) {
        return false;
    }
    tracing::warn!(
        before_elements = before.stats.elements,
        after_elements = after.elements,
        before_text_chars = before.stats.text_chars,
        after_text_chars = after.text_chars,
        "restoring server-rendered body after catastrophic hydration wipe"
    );
    js.replace_body_inner_html(&before.body_html)
}

impl Page {
    pub fn new(id: String, context: Arc<BrowserContext>) -> Self {
        let http_client = context.http_client.clone();
        // Chromium convention: the main frame's frameId == the targetId.
        // Playwright's frame manager looks up the main frame by targetId
        // (via target._targetInfo.targetId), so any divergence here makes
        // Page.getFrameTree return a frame the client cannot match,
        // triggering a Target.closeTarget and "Frame has been detached".
        let frame_id = id.clone();
        #[cfg(feature = "stealth")]
        let stealth_client = if context.stealth {
            // The wreq client backing StealthHttpClient does not speak SOCKS5.
            // Callers must validate the proxy scheme up front and fail loudly
            // (see obscura-cli) rather than silently rewriting socks5:// to
            // http://, which only works when the upstream happens to be a
            // Clash-style mixed-mode proxy and breaks plain SOCKS5 servers
            // like `ssh -ND` (#160).
            Some(Arc::new(StealthHttpClient::with_proxy(
                context.cookie_jar.clone(),
                context.proxy_url.as_deref(),
            )))
        } else {
            None
        };

        Page {
            id,
            frame_id,
            url: None,
            dom: None,
            js: None,
            lifecycle: LifecycleState::Idle,
            http_client,
            context,
            title: String::new(),
            viewport: (1280.0, 720.0),
            encoding: "UTF-8".to_string(),
            history: Vec::new(),
            history_index: 0,
            network_events: Vec::new(),
            response_bodies: std::collections::HashMap::new(),
            response_body_order: std::collections::VecDeque::new(),
            network_event_counter: 0,
            intercept_enabled: false,
            intercept_block_patterns: Vec::new(),
            blocked_url_patterns: Vec::new(),
            intercept_tx: None,
            preload_scripts: Vec::new(),
            suspended_started_script_ids: Vec::new(),
            callbacks: Arc::new(CallbackRegistry::new()),
            #[cfg(feature = "stealth")]
            stealth_client,
        }
    }

    fn should_block_url(&self, url: &str) -> bool {
        for pattern in &self.blocked_url_patterns {
            if url_matches_cdp_pattern(pattern, url) {
                return true;
            }
        }
        if self.intercept_enabled {
            for pattern in &self.intercept_block_patterns {
                if url_matches_cdp_pattern(pattern, url) {
                    return true;
                }
            }
        }
        false
    }

    /// Update the page's CSS viewport. Calling this before navigation makes
    /// responsive scripts observe it from their first instruction; calling it
    /// on a live page mirrors CDP's device-metrics override surfaces.
    pub fn set_viewport(&mut self, viewport: (f32, f32)) {
        if !viewport.0.is_finite()
            || !viewport.1.is_finite()
            || viewport.0 <= 0.0
            || viewport.1 <= 0.0
        {
            return;
        }
        self.viewport = viewport;
        if let Some(js) = &mut self.js {
            js.set_viewport(viewport.0 as f64, viewport.1 as f64);
        }
    }

    async fn do_fetch(&self, url: &Url) -> Result<Response, ObscuraNetError> {
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = self.stealth_client {
            return stealth.fetch(url).await;
        }
        self.http_client
            .fetch_with_callbacks(url, Some(&self.callbacks))
            .await
    }
    fn init_js(&mut self) {
        // init_js is also the new-document path.  Only resume_js explicitly
        // takes these IDs out before entering here and restores them after the
        // same DomTree is installed; a navigation must never inherit IDs from
        // a suspended prior document whose allocator may reuse them.
        self.suspended_started_script_ids.clear();
        // Drop any existing runtime so the JS realm starts clean on
        // every navigation. The old code reused the V8 isolate and
        // only re-bound `globalThis.document`, leaving window.onload,
        // custom window properties and event handlers from the prior
        // page in place. That made it possible for a page to set
        // attacker-controlled state, trigger a navigation, and then
        // run code in the next document's context.
        if self.js.is_some() {
            let _ = self.js.take();
        }

        // Thread the BrowserContext's proxy through to the ES-module loader
        // and op_fetch_url so dynamic imports and JS fetch() honour the
        // configured upstream proxy (#139). When proxy_url is None this is
        // equivalent to with_base_url() (direct connection).
        let mut rt = ObscuraJsRuntime::with_base_url_and_proxy(
            &self.url_string(),
            self.context.proxy_url.clone(),
        );
        rt.set_url(&self.url_string());
        rt.set_encoding(&self.encoding);
        rt.set_title(&self.title);

        #[cfg(feature = "stealth")]
        if self.stealth_client.is_some() {
            rt.set_stealth(true);
            rt.set_user_agent(obscura_net::STEALTH_USER_AGENT);
            rt.set_platform(
                obscura_net::STEALTH_NAVIGATOR_PLATFORM,
                obscura_net::STEALTH_UA_PLATFORM,
                obscura_net::STEALTH_UA_PLATFORM_VERSION,
            );
        } else {
            if let Ok(ua) = self.http_client.user_agent.try_read() {
                rt.set_user_agent(&ua);
            }
            rt.set_platform(
                &self.context.platform,
                &self.context.ua_platform,
                &self.context.ua_platform_version,
            );
        }
        #[cfg(not(feature = "stealth"))]
        {
            if let Ok(ua) = self.http_client.user_agent.try_read() {
                rt.set_user_agent(&ua);
            }
            rt.set_platform(
                &self.context.platform,
                &self.context.ua_platform,
                &self.context.ua_platform_version,
            );
        }
        if let Some((lat, lon)) = env_geolocation() {
            rt.set_geolocation(lat, lon);
        }
        rt.set_viewport(self.viewport.0 as f64, self.viewport.1 as f64);

        rt.set_cookie_jar(self.context.cookie_jar.clone());
        rt.set_http_client(self.http_client.clone());
        rt.set_callbacks(self.callbacks.clone());
        rt.set_blocked_urls(self.blocked_url_patterns.clone());
        #[cfg(feature = "stealth")]
        if let Some(ref stealth) = self.stealth_client {
            rt.set_stealth_client(stealth.clone());
        }

        if let Some(tx) = &self.intercept_tx {
            rt.set_intercept_tx(tx.clone());
        }
        // Re-apply intercept_enabled: enable_interception()/enable_intercept()
        // called before the first navigation sets this on the Page while the
        // runtime does not exist yet, so the new runtime would otherwise start
        // with interception disabled and op_fetch_url would never intercept.
        rt.set_intercept_enabled(self.intercept_enabled);

        if let Some(dom) = self.dom.take() {
            rt.set_dom(dom);
        }

        rt.run_page_init();

        self.js = Some(rt);
    }

    /// Resolve the document base URL per HTML spec:
    /// https://html.spec.whatwg.org/multipage/urls-and-fetching.html#document-base-url
    /// Falls back to self.url when no <base href> exists.
    fn resolve_base_url(&self) -> Option<url::Url> {
        let doc_url = self.url.as_ref()?;
        let base_href: Option<String> = self.js.as_ref().and_then(|js| {
            js.with_dom(|dom| {
                match dom.query_selector("base[href]") {
                    Ok(Some(nid)) => {
                        dom.get_node(nid).and_then(|n| n.get_attribute("href").map(|s| s.to_string()))
                    }
                    _ => None,
                }
            }).flatten()
        });
        match base_href {
            Some(href) => doc_url.join(&href).ok(),
            None => Some(doc_url.clone()),
        }
    }
    
    async fn fetch_stylesheets(&mut self) {
        let all_links = match &self.js {
            Some(js) => js.with_dom(linked_stylesheet_requests).unwrap_or_default(),
            None => {
                tracing::info!("fetch_stylesheets: no js runtime");
                return;
            }
        };

        tracing::info!("fetch_stylesheets: found {} stylesheet links", all_links.len());

        let document_base = self.resolve_base_url();

        let mut fetch_tasks = Vec::new();
        for (link_index, href) in all_links {
            let full_url = if href.starts_with("http://") || href.starts_with("https://") {
                href.clone()
            } else if let Some(base) = &document_base {
                base.join(&href).map(|u| u.to_string()).unwrap_or_else(|_| href.clone())
            } else {
                href.clone()
            };
            tracing::info!("fetch_stylesheets: fetching {}", full_url);

            let client = self.http_client.clone();
            fetch_tasks.push(tokio::spawn(async move {
                (
                    link_index,
                    fetch_css_with_imports(client, full_url, 0).await,
                )
            }));
        }

        for task in fetch_tasks {
            if let Ok((link_index, Some(css))) = task.await {
                tracing::info!("fetch_stylesheets: fetched {} bytes of CSS", css.len());
                tracing::info!(
                    "fetch_stylesheets: injecting sheet at link index {}",
                    link_index
                );
                if let Some(js) = &mut self.js {
                    let code = materialize_linked_stylesheet_script(link_index, &css);
                    let _ = js.execute_script("<fetch_stylesheets>", &code);
                }
            }
        }
    }

    async fn execute_scripts(&mut self) {
        tracing::info!("execute_scripts called, js runtime exists: {}", self.js.is_some());
        // Soft deadline on the entire script-execution phase. Heavy SPAs
        // (GitHub, Linear, CodeSandbox) ship 50+ scripts and our serial
        // fetch + execute loop can blow past a Puppeteer/Playwright goto
        // timeout. The old 10s default was too tight: a heavy React/Vue/Angular
        // SPA had its remaining scripts skipped before the app booted, so it
        // never fired its XHR/fetch calls and page.on('response') saw nothing
        // (issue #361). Only pages that actually run past the deadline are
        // affected; fast pages finish and return well before it, so a larger
        // budget costs them nothing. 30s gives an app room to initialize while
        // the per-phase watchdog (armed at this + 1s) still bounds a real
        // synchronous hang. Raise it further with OBSCURA_SCRIPT_DEADLINE_MS=<ms>
        // for very heavy SPAs on slow networks (pair it with a matching client
        // navigation timeout).
        let script_deadline_ms: u64 = std::env::var("OBSCURA_SCRIPT_DEADLINE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);
        let script_deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(script_deadline_ms);

        // Hard backstop over the WHOLE script-execution phase. Inline scripts
        // run back-to-back with no await between them, so neither the soft
        // deadline above (only checked between scripts) nor the per-script guard
        // can interrupt a page that burns the budget across many synchronous
        // scripts (the real-world SPA / anti-bot busy-loop hang). This watchdog
        // terminates the isolate if cumulative synchronous script work overruns.
        let exec_wd = self
            .js
            .as_mut()
            .map(|js| js.arm_watchdog(std::time::Duration::from_millis(script_deadline_ms + 1000)));

        #[derive(Debug, Clone, Copy)]
        enum ScriptKind {
            Classic,
            Module,
            ImportMap,
        }

        #[derive(Debug)]
        struct ScriptInfo {
            src: Option<String>,
            inline: String,
            is_defer: bool,
            is_async: bool,
            kind: ScriptKind,
            nid: u32,
            /// Document base URL at this element's parser encounter point.
            base_url: String,
        }

        let all_scripts = match &self.js {
            Some(js) => {
                let document_url = self.url_string();
                js.with_dom(|dom| {
                    let script_ids = dom.query_selector_all("script").unwrap_or_default();
                    let mut bases_at_script = std::collections::HashMap::new();
                    let mut active_base = url::Url::parse(&document_url).ok();
                    let mut found_base = false;
                    for nid in dom.descendants(dom.document()) {
                        let Some(node) = dom.get_node(nid) else {
                            continue;
                        };
                        let Some(name) = node.as_element() else {
                            continue;
                        };
                        if name.local.as_ref() == "base" && !found_base {
                            if let Some(href) = node.get_attribute("href") {
                                found_base = true;
                                if let Some(resolved) = active_base
                                    .as_ref()
                                    .and_then(|base| base.join(href).ok())
                                {
                                    active_base = Some(resolved);
                                }
                            }
                        } else if name.local.as_ref() == "script" {
                            bases_at_script.insert(
                                nid.raw(),
                                active_base
                                    .as_ref()
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| document_url.clone()),
                            );
                        }
                    }
                    let mut scripts = Vec::new();

                    for sid in script_ids {
                        if let Some(node) = dom.get_node(sid) {
                            let src = node.get_attribute("src").map(|s| s.to_string());
                            let script_type = node
                                .get_attribute("type")
                                .unwrap_or("")
                                .trim()
                                .to_ascii_lowercase();
                            let is_defer = node.get_attribute("defer").is_some();
                            let is_async = node.get_attribute("async").is_some();
                            let kind = match script_type.as_str() {
                                "module" => ScriptKind::Module,
                                "importmap" => ScriptKind::ImportMap,
                                "" | "text/javascript" | "application/javascript" => {
                                    ScriptKind::Classic
                                }
                                _ => continue,
                            };

                            let inline_code = if src.is_none() {
                                dom.text_content(sid)
                            } else {
                                String::new()
                            };

                            if matches!(kind, ScriptKind::ImportMap)
                                || src.is_some()
                                || !inline_code.trim().is_empty()
                            {
                                scripts.push(ScriptInfo {
                                    src,
                                    inline: inline_code,
                                    is_defer,
                                    is_async,
                                    kind,
                                    nid: sid.raw(),
                                    base_url: bases_at_script
                                        .get(&sid.raw())
                                        .cloned()
                                        .unwrap_or_else(|| document_url.clone()),
                                });
                            }
                        }
                    }
                    scripts
                }).unwrap_or_default()
            }
            None => return,
        };

        // HTML scripts have an "already started" flag. Mark every
        // parser-discovered script before running page code so React/Next
        // hydration can move or hoist those nodes without appendChild
        // executing them a second time.
        if let Some(js) = &mut self.js {
            let ids = all_scripts
                .iter()
                .map(|script| script.nid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = js.execute_script(
                "<parser-scripts>",
                &format!("globalThis.__markParserScripts([{}]);", ids),
            );
        }

        tracing::info!("Found {} parser-discovered scripts", all_scripts.len());
        let mut fetch_tasks: Vec<(usize, String)> = Vec::new();

        for (i, script) in all_scripts.iter().enumerate() {
            if !matches!(script.kind, ScriptKind::Classic) {
                continue;
            }
            if let Some(src_url) = &script.src {
                let full_url = if src_url.starts_with("http://") || src_url.starts_with("https://") {
                    src_url.clone()
                } else {
                    url::Url::parse(&script.base_url)
                        .ok()
                        .and_then(|base| base.join(src_url).ok())
                        .map(|url| url.to_string())
                        .unwrap_or_else(|| src_url.clone())
                };

                if !subresource_allowed(self.url.as_ref(), &full_url) {
                    // Block file://, data:, javascript:, and other
                    // off-origin schemes from being injected as a
                    // <script src>. Without this an http page can
                    // include <script src="file:///etc/passwd"> and
                    // see the body parsed as JS source.
                    tracing::warn!(
                        "blocking cross-scheme <script src>: page={} src={}",
                        self.url_string(),
                        full_url,
                    );
                    continue;
                }
                if self.should_block_url(&full_url) {
                    tracing::info!("Blocked script by interception: {}", full_url);
                    continue;
                }
                fetch_tasks.push((i, full_url));
            }
        }

        let client = self.http_client.clone();
        let page_callbacks = self.callbacks.clone();
        let fetch_futures: Vec<_> = fetch_tasks.iter().map(|(idx, url)| {
            let client = client.clone();
            let cbs = page_callbacks.clone();
            let url = url.clone();
            let idx = *idx;
            async move {
                let parsed = Url::parse(&url).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
                if parsed.scheme() == "data" {
                    // data: URIs are inline; decode locally, no network fetch.
                    // Instagram and other Meta properties serve their bootstrap
                    // as <script src="data:application/x-javascript;base64,...">.
                    let body = decode_data_uri(&url).unwrap_or_default();
                    let content_type = url
                        .strip_prefix("data:")
                        .and_then(|s| s.split(',').next())
                        .unwrap_or("application/javascript")
                        .split(';')
                        .next()
                        .unwrap_or("application/javascript")
                        .to_string();
                    let mut headers = std::collections::HashMap::new();
                    headers.insert("content-type".to_string(), content_type);
                    let resp = obscura_net::Response {
                        url: parsed,
                        status: 200,
                        headers,
                        body,
                        redirected_from: Vec::new(),
                    };
                    return Some((idx, url, resp));
                }
                match client.fetch_with_callbacks(&parsed, Some(&cbs)).await {
                    Ok(resp) => Some((idx, url, resp)),
                    Err(e) => {
                        tracing::warn!("Failed to fetch script {}: {}", url, e);
                        None
                    }
                }
            }
        }).collect();

        // Bound concurrency: a page with 100 external scripts would
        // otherwise open 100 sockets at once, exhausting the connection
        // pool / ephemeral ports and triggering OS-level backpressure.
        // 16 is well above the per-host pool ceiling most browsers use
        // and matches what real Chrome does for a given origin.
        use futures::StreamExt as _;
        let fetch_stream = futures::stream::iter(fetch_futures)
            .buffer_unordered(16);
        let fetch_results = match tokio::time::timeout_at(
            script_deadline,
            fetch_stream.collect::<Vec<_>>(),
        ).await {
            Ok(results) => results,
            Err(_) => {
                tracing::warn!(
                    "execute_scripts: fetch deadline reached, some scripts may not have loaded"
                );
                Vec::new()
            }
        };

        let mut fetched: std::collections::HashMap<usize, (String, String, obscura_net::Response)> = std::collections::HashMap::new();
        for result in fetch_results {
            if let Some((idx, url, resp)) = result {
                if !script_response_is_executable(resp.status) {
                    self.record_network_event_with_body(
                        &url,
                        "GET",
                        "Script",
                        resp.status,
                        &resp.headers,
                        &resp.body,
                        false,
                    );
                    tracing::warn!(
                        "Refusing to execute script {} after HTTP {}",
                        url,
                        resp.status
                    );
                    continue;
                }
                // Script bodies: only the HTTP Content-Type charset matters
                // (no in-band meta-charset for JS).
                let code = obscura_net::decode_non_html(&resp.body, resp.content_type());
                fetched.insert(idx, (url, code, resp));
            }
        }

        // Spec: readyState is "loading" while parser-discovered scripts execute.
        // Scripts that check readyState === 'loading' will register DOMContentLoaded
        // listeners instead of calling their callback immediately.
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<ready-state>", "globalThis.__documentReadyState__ = 'loading';");
        }

        // CDP `Page.addScriptToEvaluateOnNewDocument` contract: preload
        // sources must run BEFORE any of the page's own scripts. This is
        // also where puppeteer's `exposeFunction` wrapper installs itself —
        // if preload runs after page scripts, every early binding call
        // hits an undefined function and silently no-ops.
        let preload_sources = self.preload_scripts.clone();
        if let Some(js) = &mut self.js {
            for source in &preload_sources {
                if let Err(e) = js.execute_script_guarded("<preload>", source.as_str()) {
                    tracing::debug!("Preload script error: {}", e);
                }
            }
        }

        // Per-module budget. Modules on an already-rendered page are
        // enhancement, not the app: give them a short budget so one slow
        // non-essential module (e.g. YC's bookface, whose top-level eval
        // idle-waits ~10s) cannot block navigation completion. A page whose
        // body is still an empty shell IS the SPA (issue #205), so give it the
        // full script budget and the app module still mounts.
        let module_budget_ms: u64 = {
            let body_nodes = self
                .js
                .as_ref()
                .and_then(|js| {
                    js.with_dom(|dom| {
                        dom.query_selector("body")
                            .ok()
                            .flatten()
                            .map(|b| dom.descendants(b).len())
                            .unwrap_or(0)
                    })
                })
                .unwrap_or(0);
            let short_ms: u64 = std::env::var("OBSCURA_MODULE_BUDGET_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3_000);
            // A rendered body has hundreds of descendants; an unmounted Vite/Next
            // shell is <root> plus maybe a spinner.
            if body_nodes > 50 { short_ms } else { script_deadline_ms }
        };

        enum ScheduledScript {
            Classic(usize),
            Module {
                prepared: obscura_js::runtime::PreparedModule,
                url: Option<String>,
            },
        }

        let execute_classic = |
            page: &mut Self,
            script: &ScriptInfo,
            fetched_script: Option<(String, String, obscura_net::Response)>,
        | {
            if script.src.is_some() {
                if let Some((url, code, resp)) = fetched_script {
                    tracing::info!("Executing script ({} bytes): {}", code.len(), url);
                    let execution_url = resp.url.to_string();
                    page.record_network_event_with_body(
                        &url, "GET", "Script", resp.status, &resp.headers, &resp.body, false,
                    );
                    if let Some(js) = &mut page.js {
                        let _ = js.execute_script(
                            "<current-script>",
                            &format!("globalThis.__currentScriptNid={};", script.nid),
                        );
                        if let Err(error) = js.execute_script_guarded(&execution_url, &code) {
                            tracing::warn!("Script error ({}): {}", execution_url, error);
                        }
                        let _ = js.execute_script(
                            "<current-script>",
                            "globalThis.__currentScriptNid=0;",
                        );
                    }
                }
            } else if !script.inline.is_empty() {
                if let Some(js) = &mut page.js {
                    let _ = js.execute_script(
                        "<current-script>",
                        &format!("globalThis.__currentScriptNid={};", script.nid),
                    );
                    if let Err(error) = js.execute_script_guarded(&script.base_url, &script.inline) {
                        tracing::warn!("Inline script error: {}", error);
                    }
                    let _ = js.execute_script(
                        "<current-script>",
                        "globalThis.__currentScriptNid=0;",
                    );
                }
            }
        };

        let mut post_parse = Vec::new();

        // Process parser-discovered scripts in encounter order. Import maps
        // register at their exact position; module graphs start there too, but
        // evaluation of non-async modules remains post-parse.
        for (index, script) in all_scripts.iter().enumerate() {
            if tokio::time::Instant::now() >= script_deadline {
                tracing::warn!(
                    "execute_scripts: deadline reached, skipping {} remaining scripts",
                    all_scripts.len() - index,
                );
                break;
            }

            match script.kind {
                ScriptKind::ImportMap => {
                    if script.src.is_some() {
                        tracing::warn!("External import maps are not supported");
                        continue;
                    }
                    if let Some(js) = &self.js {
                        if let Err(error) = js.add_import_map(&script.inline, &script.base_url) {
                            tracing::warn!("Ignoring invalid import map: {}", error);
                        }
                    }
                }
                ScriptKind::Classic => {
                    if script.is_defer && !script.is_async && script.src.is_some() {
                        post_parse.push(ScheduledScript::Classic(index));
                    } else {
                        let fetched_script = fetched.remove(&index);
                        execute_classic(self, script, fetched_script);
                    }
                }
                ScriptKind::Module => {
                    let (prepared, module_url) = if let Some(src) = &script.src {
                        let full_url = if src.starts_with("http://")
                            || src.starts_with("https://")
                            || src.starts_with("data:")
                        {
                            src.clone()
                        } else {
                            url::Url::parse(&script.base_url)
                                .ok()
                                .and_then(|base| base.join(src).ok())
                                .map(|url| url.to_string())
                                .unwrap_or_else(|| src.clone())
                        };
                        tracing::info!("Preparing ES module graph: {}", full_url);
                        let result = match &mut self.js {
                            Some(js) => js.prepare_module(&full_url, module_budget_ms).await,
                            None => continue,
                        };
                        match result {
                            Ok(prepared) => (prepared, Some(full_url)),
                            Err(error) => {
                                tracing::warn!("ES module error ({}): {}", full_url, error);
                                continue;
                            }
                        }
                    } else {
                        let result = match &mut self.js {
                            Some(js) => js.prepare_inline_module(
                                &script.inline, &script.base_url, module_budget_ms,
                            ).await,
                            None => continue,
                        };
                        match result {
                            Ok(prepared) => (prepared, None),
                            Err(error) => {
                                tracing::warn!("Inline ES module error: {}", error);
                                continue;
                            }
                        }
                    };
                    let scheduled = ScheduledScript::Module {
                        prepared,
                        url: module_url,
                    };
                    if script.is_async {
                        let ScheduledScript::Module { prepared, url } = scheduled else {
                            unreachable!();
                        };
                        let result = match &mut self.js {
                            Some(js) => js.evaluate_prepared_module(prepared, module_budget_ms).await,
                            None => continue,
                        };
                        if let Err(error) = result {
                            tracing::warn!("ES module evaluation error: {}", error);
                        } else if let Some(url) = url {
                            tracing::info!("ES module loaded: {}", url);
                            self.record_network_event(
                                &url, "GET", "Script", 200,
                                &std::collections::HashMap::new(), 0,
                            );
                        }
                    } else {
                        post_parse.push(scheduled);
                    }
                }
            }
        }

        for scheduled in post_parse {
            if tokio::time::Instant::now() >= script_deadline {
                tracing::warn!("execute_scripts: deadline reached during post-parse scripts");
                break;
            }
            match scheduled {
                ScheduledScript::Classic(index) => {
                    let script = &all_scripts[index];
                    let fetched_script = fetched.remove(&index);
                    execute_classic(self, script, fetched_script);
                }
                ScheduledScript::Module { prepared, url } => {
                    let result = match &mut self.js {
                        Some(js) => js.evaluate_prepared_module(prepared, module_budget_ms).await,
                        None => continue,
                    };
                    if let Err(error) = result {
                        tracing::warn!("ES module evaluation error: {}", error);
                    } else if let Some(url) = url {
                        tracing::info!("ES module loaded: {}", url);
                        self.record_network_event(
                            &url, "GET", "Script", 200,
                            &std::collections::HashMap::new(), 0,
                        );
                    }
                }
            }
        }

        if let Some(js) = &mut self.js {
            // Spec order: readyState -> interactive, fire DOMContentLoaded on both
            // document and window, then readyState -> complete, fire load.
            let _ = js.execute_script("<load-events>",
                "globalThis.__documentReadyState__ = 'interactive';\n\
                 try { document.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}\n\
                 try { window.dispatchEvent(new Event('DOMContentLoaded', {bubbles:false,cancelable:false})); } catch(e) {}\n\
                 if (typeof window.onload === 'function') { try { window.onload(); } catch(e) {} }\n\
                 globalThis.__documentReadyState__ = 'complete';\n\
                 try { window.dispatchEvent(new Event('load', {bubbles:false,cancelable:false})); } catch(e) {}");
        }

        if let Some(js) = &mut self.js {
            let dynamic_settle_ms = std::env::var("OBSCURA_DYNAMIC_SCRIPT_SETTLE_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3_000)
                .max(500);
            // Bound the post-script settle loop by wall clock, not just by the
            // 10ms-tick branch. The old code only consulted `deadline` inside
            // the `Err(_)` arm (when the inner tick timed out), so a steady
            // stream of inflight XHR/fetch (active_requests() > 0) kept the
            // loop running indefinitely because it took the `Ok(Ok(()))` arm
            // and slept 1ms each iteration without ever checking the clock.
            // On busy sites this could keep the V8 lock held for tens of
            // seconds, wedging the entire CDP dispatcher (see triage for
            // issue series around the 40-site compat sweep).
            // A dynamic external script may still be in flight at 500ms. Keep
            // pumping only while that queue is pending, up to a separate bounded
            // budget, so normal pages and unrelated fetches retain the fast path.
            // A single run_event_loop poll that pins the thread inside V8 makes
            // the per-poll tokio timeouts below useless, so guard the whole loop
            // with a watchdog that fires 250ms past the longest deadline.
            let settle_wd = js.arm_watchdog(std::time::Duration::from_millis(dynamic_settle_ms + 250));
            let started = tokio::time::Instant::now();
            let deadline = started + tokio::time::Duration::from_millis(500);
            let dynamic_deadline = started + tokio::time::Duration::from_millis(dynamic_settle_ms);
            let mut idle_count = 0u32;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline
                    && (now >= dynamic_deadline || !js.has_pending_dynamic_scripts())
                {
                    break;
                }
                let result = tokio::time::timeout(
                    tokio::time::Duration::from_millis(10),
                    js.run_event_loop(),
                ).await;

                match result {
                    Ok(Ok(())) => {
                        if self.http_client.active_requests() == 0 {
                            idle_count += 1;
                            if idle_count >= 2 {
                                break;
                            }
                            tokio::task::yield_now().await;
                        } else {
                            idle_count = 0;
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {
                        idle_count = 0;
                    }
                }
            }
            js.disarm_watchdog(settle_wd);
        }
        if let Some(token) = exec_wd {
            if let Some(js) = self.js.as_mut() {
                js.disarm_watchdog(token);
            }
        }
    }

    pub async fn navigate(&mut self, url_str: &str) -> Result<(), PageError> {
        self.navigate_with_wait(url_str, crate::lifecycle::WaitUntil::Load).await
    }

    pub async fn navigate_with_wait(
        &mut self,
        url_str: &str,
        wait_until: crate::lifecycle::WaitUntil,
    ) -> Result<(), PageError> {
        self.navigate_with_wait_post(url_str, wait_until, "GET", "").await
    }

    pub async fn navigate_with_wait_post(
        &mut self,
        url_str: &str,
        wait_until: crate::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        // Hard ceiling on a single end-to-end navigation. Without this a slow
        // primary fetch or a runaway settle loop can hold the V8 lock for
        // arbitrarily long (we've measured 60+ seconds on JS-heavy news
        // sites), wedging every other in-flight CDP request because the
        // dispatcher holds the lock across the entire handler. 30 seconds
        // matches reqwest's default per-request timeout — the worst case is
        // one slow primary GET plus one slow JS-redirect chain step. Override
        // with `OBSCURA_NAV_TIMEOUT_MS=NN`.
        let nav_timeout_ms: u64 = std::env::var("OBSCURA_NAV_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);
        let nav_timeout = tokio::time::Duration::from_millis(nav_timeout_ms);

        let result = match tokio::time::timeout(
            nav_timeout,
            self.navigate_with_wait_post_inner(url_str, wait_until, method, body),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                self.lifecycle = crate::lifecycle::LifecycleState::Failed;
                Err(PageError::NetworkError(format!(
                    "navigation exceeded {nav_timeout_ms}ms deadline"
                )))
            }
        };
        if result.is_ok() {
            self.push_history(self.url_string());
        }
        result
    }

    /// Drive the JS event loop after navigation so deferred work can run:
    /// pending timers (setTimeout / setInterval), queued microtasks, in-flight
    /// fetches, and completion callbacks such as testharness's
    /// `add_completion_callback`. Returns as soon as the loop goes idle, or
    /// after `max_ms`. Without this the page is observed exactly as it stood at
    /// the load event, before any async work settles, which silently strands
    /// timer-driven tests and dynamic pages.
    pub async fn settle(&mut self, max_ms: u64) {
        if max_ms == 0 {
            return;
        }
        let hydration_snapshot = self.js.as_ref().and_then(capture_hydration_snapshot);
        if let Some(js) = &mut self.js {
            if std::env::var_os("OBSCURA_STRICT_SETTLE").is_some() {
                // Paired browser measurements need the same wall-clock interval
                // after load in both engines. The ordinary product path returns
                // as soon as the loop is idle for speed; strict mode pumps all
                // pending work, then retains the remaining wall-clock delay.
                let started = tokio::time::Instant::now();
                let _ = js.run_event_loop_bounded(max_ms).await;
                let requested = tokio::time::Duration::from_millis(max_ms);
                let elapsed = started.elapsed();
                if elapsed < requested {
                    tokio::time::sleep(requested - elapsed).await;
                }
            } else {
                // Bounded against both async idle and synchronous microtask
                // storms: a plain tokio timeout cannot preempt a page that pins
                // the thread inside V8, so settle uses the watchdog path.
                let _ = js.run_event_loop_bounded(max_ms).await;
            }

            if let Some(before) = hydration_snapshot {
                let _ = restore_hydration_snapshot_if_needed(js, before);
            }
        }
    }

    /// Append the current URL to the history stack, truncating any forward
    /// entries past the cursor (matches real Chrome: navigating after a
    /// goBack clobbers the forward history).
    pub fn push_history(&mut self, url: String) {
        if url.is_empty() { return; }
        // Don't dupe consecutive entries (Page.reload would otherwise pile up).
        if self.history.get(self.history_index) == Some(&url) {
            return;
        }
        if !self.history.is_empty() && self.history_index < self.history.len() - 1 {
            self.history.truncate(self.history_index + 1);
        }
        self.history.push(url);
        self.history_index = self.history.len() - 1;
    }

    /// Move the history cursor without re-navigating; used by
    /// Page.navigateToHistoryEntry which then drives the actual fetch.
    pub fn set_history_index(&mut self, idx: usize) {
        if idx < self.history.len() {
            self.history_index = idx;
        }
    }

    async fn navigate_with_wait_post_inner(
        &mut self,
        url_str: &str,
        wait_until: crate::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        let mut current_url = url_str.to_string();
        let mut current_method = method.to_string();
        let mut current_body = body.to_string();
        const REDIRECT_LIMIT: usize = 10;
        for chain in 0..REDIRECT_LIMIT {
            self.navigate_single(&current_url, wait_until, &current_method, &current_body).await?;
            if let Some((next_url, next_method, next_body)) = self.take_pending_navigation() {
                if cross_scheme_to_file(&current_url, &next_url) {
                    // SOP gate. A web page must not be able to drive
                    // a navigation to file:// and then read the loaded
                    // document. Without this an http(s) page sets
                    // window.onload, calls location.href = "file:..."
                    // and harvests document.body from a local file
                    // once the new document loads.
                    tracing::warn!(
                        "blocking JS-initiated cross-scheme navigation to file: {} -> {}",
                        current_url,
                        next_url,
                    );
                    break;
                }
                tracing::info!("JS-triggered navigation chain: {} {} -> {}", current_method, current_url, next_url);
                current_url = next_url;
                current_method = next_method;
                current_body = next_body;
                if chain + 1 == REDIRECT_LIMIT {
                    // Hit the cap and the page still wants to keep
                    // chaining. Surface that as an error instead of
                    // returning Ok(()) so callers can distinguish a
                    // successful load from a redirect storm.
                    return Err(PageError::TooManyRedirects(REDIRECT_LIMIT));
                }
                continue;
            }
            break;
        }
        Ok(())
    }

    async fn navigate_single(
        &mut self,
        url_str: &str,
        wait_until: crate::lifecycle::WaitUntil,
        method: &str,
        body: &str,
    ) -> Result<(), PageError> {
        let url = Url::parse(url_str).map_err(|e| PageError::InvalidUrl(e.to_string()))?;

        self.lifecycle = LifecycleState::Loading;
        self.url = Some(url.clone());
        self.network_events.clear();

        if self.context.obey_robots {
            if let Some(domain) = url.host_str() {
                if self.context.robots_cache.is_allowed(domain, "/robots.txt") {
                    let robots_url = format!("{}://{}/robots.txt", url.scheme(), domain);
                    if let Ok(robots_url) = Url::parse(&robots_url) {
                        if let Ok(resp) = self
                            .http_client
                            .fetch_with_callbacks(&robots_url, Some(&self.callbacks))
                            .await
                        {
                            if resp.status == 200 {
                                let body = String::from_utf8_lossy(&resp.body);
                                self.context.robots_cache.parse_and_store(
                                    domain,
                                    &body,
                                    &self.context.user_agent,
                                );
                            }
                        }
                    }
                }

                if !self.context.robots_cache.is_allowed(domain, url.path()) {
                    self.lifecycle = LifecycleState::Failed;
                    return Err(PageError::NetworkError(format!(
                        "Blocked by robots.txt: {}",
                        url
                    )));
                }
            }
        }

        if url.scheme() == "about" {
            self.navigate_blank();
            self.init_js();
            // Preloads (Page.addScriptToEvaluateOnNewDocument, the
            // Runtime.addBinding shim) must run on about:blank too —
            // puppeteer's `browser.newPage()` lands on about:blank and
            // a follow-up `exposeFunction` is unusable otherwise.
            let preload_sources = self.preload_scripts.clone();
            if let Some(js) = &mut self.js {
                for source in &preload_sources {
                    if let Err(e) = js.execute_script_guarded("<preload>", source.as_str()) {
                        tracing::debug!("Preload script error on about:blank: {}", e);
                    }
                }
            }
            return Ok(());
        }

        let response = if url.scheme() == "data" {
            let content_type = url_str.strip_prefix("data:")
                .and_then(|s| s.split(',').next())
                .unwrap_or("text/html")
                .split(';').next()
                .unwrap_or("text/html")
                .to_string();
            let body_bytes = decode_data_uri(url_str).unwrap_or_default();
            let mut headers = std::collections::HashMap::new();
            headers.insert("content-type".to_string(), content_type);
            Ok(obscura_net::Response { url: url.clone(), status: 200, headers, body: body_bytes, redirected_from: Vec::new() })
        } else if method == "POST" {
            self.http_client
                .post_form_with_callbacks(&url, body, Some(&self.callbacks))
                .await
        } else {
            self.do_fetch(&url).await
        }.map_err(|e| {
            self.lifecycle = LifecycleState::Failed;
            PageError::NetworkError(e.to_string())
        })?;

        // Store binary main resources (images, PDFs, octet-stream) base64 so
        // Network.getResponseBody returns intact bytes. A UTF-8-lossy text store
        // corrupts them (issue #340). Text-like types stay as text.
        let main_is_binary = !is_text_like_content_type(response.content_type());
        self.record_network_event_with_body(
            url.as_str(),
            "GET",
            "Document",
            response.status,
            &response.headers,
            &response.body,
            main_is_binary,
        );

        if !response.redirected_from.is_empty() {
            self.url = Some(response.url.clone());
        }

        // Honor the response charset: HTTP Content-Type → <meta charset> sniff
        // in the first 1KB → UTF-8 fallback. Without this, every non-UTF-8
        // page (GBK, Big5, Shift-JIS, Windows-125x, EUC-KR, ISO-8859-x)
        // came through as replacement characters.
        let (body_text, encoding_name) =
            obscura_net::decode_response_with_name(&response.body, response.content_type());
        self.encoding = encoding_name.to_string();
        let dom = parse_html(&body_text);

        self.title = dom
            .query_selector("title")
            .ok()
            .flatten()
            .map(|title_id| dom.text_content(title_id))
            .unwrap_or_default();

        let stylesheet_urls: Vec<String> = dom
            .query_selector_all("link")
            .unwrap_or_default()
            .iter()
            .filter_map(|&nid| {
                // Borrow the node instead of deep-cloning it; rel keywords are
                // ASCII so eq_ignore_ascii_case matches to_lowercase() exactly
                // without allocating a lowercased String.
                dom.with_node(nid, |node| {
                    let rel = node.get_attribute("rel")?;
                    if !rel.eq_ignore_ascii_case("stylesheet") {
                        return None;
                    }
                    node.get_attribute("href").map(|s| s.to_string())
                })
                .flatten()
            })
            .collect();

        let mut css_fetch_urls: Vec<String> = Vec::new();
        for href in &stylesheet_urls {
            let full_url = if href.starts_with("http://") || href.starts_with("https://") {
                href.clone()
            } else if let Some(base) = &self.url {
                base.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.clone())
            } else {
                href.clone()
            };
            if !subresource_allowed(self.url.as_ref(), &full_url) {
                tracing::warn!(
                    "blocking cross-scheme <link rel=stylesheet href>: page={} href={}",
                    self.url_string(),
                    full_url,
                );
                continue;
            }
            if self.should_block_url(&full_url) {
                tracing::info!("Blocked stylesheet by interception: {}", full_url);
                continue;
            }
            css_fetch_urls.push(full_url);
        }

        let client = self.http_client.clone();
        let page_callbacks = self.callbacks.clone();
        let css_futures: Vec<_> = css_fetch_urls.iter().map(|full_url| {
            let client = client.clone();
            let cbs = page_callbacks.clone();
            let url_str = full_url.clone();
            async move {
                let parsed = Url::parse(&url_str).unwrap_or_else(|_| Url::parse("about:blank").unwrap());
                match client.fetch_with_callbacks(&parsed, Some(&cbs)).await {
                    Ok(resp) => Some((url_str, resp)),
                    Err(e) => {
                        tracing::debug!("Failed to fetch stylesheet {}: {}", url_str, e);
                        None
                    }
                }
            }
        }).collect();

        // Same concurrency cap as script fetches.
        use futures::StreamExt as _;
        let css_results: Vec<_> = futures::stream::iter(css_futures)
            // Fetch concurrently but retain document order for the exposed
            // aggregate CSS. Completion order is not cascade order.
            .buffered(16)
            .collect()
            .await;
        let mut css_sources = Vec::new();
        for result in css_results {
            if let Some((url_str, resp)) = result {
                // CSS bodies: honor the Content-Type charset; CSS @charset is
                // out of scope for the current scrape-focused pipeline.
                let css = obscura_net::decode_non_html(&resp.body, resp.content_type());
                self.record_network_event_with_body(&url_str, "GET", "Stylesheet", resp.status, &resp.headers, &resp.body, false);
                css_sources.push(css);
            }
        }

        self.dom = Some(dom);
        self.init_js();

        // Inject CSS as a global so getComputedStyle and any CSS-aware shim
        // can read it. Has to happen before scripts run, regardless of
        // waitUntil, so handlers that read window.__obscura_css see it.
        if !css_sources.is_empty() {
            if let Some(js) = &mut self.js {
                let combined_css = css_sources.join("\n");
                // Use the thorough template-literal escape that
                // covers U+2028 / U+2029 and other control chars.
                // The previous escaper only handled `, \, and ${,
                // letting attacker-controlled CSS containing a raw
                // U+2028 break out of the template literal and run
                // arbitrary JS in the page's V8 realm.
                let escaped = escape_for_js_template_literal(&combined_css);
                let code = format!("globalThis.__obscura_css = `{}`;", escaped);
                let _ = js.execute_script("<css>", &code);
            }
        }
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<iframe-load>",
                "(function() { var iframes = document.querySelectorAll('iframe[src]'); for (var i = 0; i < iframes.length; i++) { var src = iframes[i].getAttribute('src'); if (src && src !== 'about:blank') iframes[i]._loadIframeSrc(src); } })()");
        }

        // Spec: DOMContentLoaded fires AFTER parser-blocking scripts run,
        // not before. Skipping execute_scripts() on the DCL path meant
        // every inline <script> in the page was silently dropped: form
        // listeners never registered, frameworks never bootstrapped,
        // page.click() handlers were no-ops. Now scripts run regardless
        // of waitUntil and DCL means "DOM parsed AND scripts executed".
        self.fetch_stylesheets().await;
        let hydration_snapshot = self.js.as_ref().and_then(capture_hydration_snapshot);
        self.execute_scripts().await;
        if let (Some(js), Some(before)) = (&self.js, hydration_snapshot) {
            let _ = restore_hydration_snapshot_if_needed(js, before);
        }

        self.lifecycle = LifecycleState::DomContentLoaded;

        if wait_until == crate::lifecycle::WaitUntil::DomContentLoaded {
            return Ok(());
        }

        if let Some(js) = &mut self.js {
            if let Ok(new_title) = js.evaluate("document.title") {
                if let Some(t) = new_title.as_str() {
                    self.title = t.to_string();
                }
            }
        }

        self.lifecycle = LifecycleState::Loaded;

        if matches!(
            wait_until,
            crate::lifecycle::WaitUntil::NetworkIdle0 | crate::lifecycle::WaitUntil::NetworkIdle2
        ) {
            let threshold = match wait_until {
                crate::lifecycle::WaitUntil::NetworkIdle0 => 0,
                crate::lifecycle::WaitUntil::NetworkIdle2 => 2,
                _ => 0,
            };

            // Same hazard as the post-script settle: a synchronous poll can pin
            // the thread past the 5s network-idle deadline, so arm a watchdog
            // that terminates the isolate ~500ms past it.
            let netidle_wd = self
                .js
                .as_mut()
                .map(|js| js.arm_watchdog(std::time::Duration::from_millis(5500)));
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            let mut idle_since: Option<tokio::time::Instant> = None;

            loop {
                let active = self.http_client.active_requests();
                let now = tokio::time::Instant::now();

                if active <= threshold {
                    if idle_since.is_none() {
                        idle_since = Some(now);
                    }
                    if now.duration_since(idle_since.unwrap()) >= tokio::time::Duration::from_millis(500) {
                        break;
                    }
                } else {
                    idle_since = None;
                }

                if now >= deadline {
                    tracing::debug!("Network idle timeout reached with {} active requests", active);
                    break;
                }

                if let Some(js) = &mut self.js {
                    let _ = tokio::time::timeout(
                        tokio::time::Duration::from_millis(50),
                        js.run_event_loop(),
                    ).await;
                } else {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }

            if let Some(token) = netidle_wd {
                if let Some(js) = self.js.as_mut() {
                    js.disarm_watchdog(token);
                }
            }
            self.lifecycle = LifecycleState::NetworkIdle;
        }

        Ok(())
    }

    pub fn navigate_blank(&mut self) {
        self.js = None;
        self.url = Some(Url::parse("about:blank").unwrap());
        self.dom = Some(parse_html("<!DOCTYPE html><html><head></head><body></body></html>"));
        self.title = String::new();
        self.lifecycle = LifecycleState::Loaded;
    }

    pub fn url_string(&self) -> String {
        self.url
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "about:blank".to_string())
    }

    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        if let Some(js) = &self.js {
            return js.with_dom(f);
        }
        self.dom.as_ref().map(f)
    }

    /// Rasterize the current DOM to PNG bytes at `viewport` (CSS pixels), when
    /// the render feature is compiled in. None if the page has no DOM or the
    /// viewport is zero-sized.
    #[cfg(feature = "render")]
    pub fn screenshot(&self, viewport: (f32, f32)) -> Option<Vec<u8>> {
        // Needed to resolve the relative image URLs ("logo.svg") that make up
        // the overwhelming majority of real markup.
        let base_url = self.resolve_base_url();
        let base_url = base_url.as_ref().map(|u| u.as_str());
        if let Some(js) = &self.js {
            if let Some(png) = js.screenshot_prepared(viewport, base_url) {
                return Some(png);
            }
        }
        // Compatibility path for a page without a JS runtime or an ad-hoc
        // viewport/base that does not match the runtime's CSSOM render key.
        let scroll = self
            .js
            .as_ref()
            .map(|js| js.scroll_offset())
            .unwrap_or((0.0, 0.0));
        self.with_dom(|dom| {
            obscura_js::screenshot_png_scrolled(dom, viewport, base_url, scroll)
        })
        .flatten()
    }

    /// Absolute URLs the page pulled in via fetch()/XHR (issue #301). Empty
    /// when the page has no live JS runtime.
    pub fn fetched_urls(&self) -> Vec<String> {
        self.js.as_ref().map(|js| js.fetched_urls()).unwrap_or_default()
    }

    /// Move network events recorded for script-initiated requests
    /// (fetch/XHR/dynamic resource) from the JS runtime into this page's
    /// network_events, so the CDP layer emits Network.requestWillBeSent /
    /// responseReceived for them (issue #406). Idempotent: the runtime's queue
    /// is drained, so calling this repeatedly does not duplicate events. The
    /// fetch-{N} request id is preserved so Network.getResponseBody resolves.
    pub fn sync_js_network_events(&mut self) {
        let events = match self.js.as_ref() {
            Some(js) => js.take_js_network_events(),
            None => return,
        };
        for ev in events {
            self.network_events.push(NetworkEvent {
                request_id: ev.request_id,
                url: ev.url,
                method: ev.method,
                resource_type: "Fetch".to_string(),
                status: ev.status,
                headers: std::collections::HashMap::new(),
                response_headers: Arc::new(ev.response_headers),
                body_size: ev.body_size,
                timestamp: ev.timestamp,
            });
        }
    }

    pub fn dom(&self) -> Option<&DomTree> {
        self.dom.as_ref()
    }

    /// V8 isolate handle for this page's runtime, if it has been initialized.
    /// Lets the CDP dispatcher arm a per-command watchdog (which bounds any one
    /// command so a hung page cannot hold this connection's V8 lock forever)
    /// without taking `&mut self`.
    pub fn isolate_handle(&self) -> Option<obscura_js::runtime::IsolateHandle> {
        self.js.as_ref().map(|js| js.isolate_handle())
    }

    /// Clear a V8 termination left by a per-command watchdog so the next command
    /// on this page can run. No-op if the runtime is absent or not terminating.
    pub fn cancel_v8_termination(&mut self) {
        if let Some(js) = self.js.as_mut() {
            js.cancel_termination();
        }
    }

    /// Like [`Self::evaluate`] but bounded by a V8 watchdog so a runaway
    /// expression cannot hang the process. A non-zero `timeout` of zero falls
    /// back to the unbounded path.
    pub fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate_with_timeout(expression, timeout) {
                Ok(val) => val,
                Err(e) => {
                    tracing::debug!("JS eval error/timeout for '{}': {}", truncate_on_char_boundary(expression, 80), e);
                    serde_json::Value::Null
                }
            }
        } else {
            self.evaluate(expression)
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate(expression) {
                Ok(val) => val,
                Err(e) => {
                    tracing::debug!("JS eval error for '{}': {}", truncate_on_char_boundary(expression, 80), e);
                    serde_json::Value::Null
                }
            }
        } else {
            match expression.trim() {
                "document.title" => serde_json::Value::String(self.title.clone()),
                "document.URL" | "document.location.href" | "window.location.href" => {
                    serde_json::Value::String(self.url_string())
                }
                _ => serde_json::Value::Null,
            }
        }
    }

    pub async fn evaluate_for_cdp(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> obscura_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.evaluate_for_cdp(expression, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    tracing::debug!("evaluate_for_cdp error: {}", e);
                    obscura_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            let val = self.evaluate(expression);
            obscura_js::runtime::RemoteObjectInfo {
                js_type: match &val {
                    serde_json::Value::String(_) => "string".into(),
                    serde_json::Value::Number(_) => "number".into(),
                    serde_json::Value::Bool(_) => "boolean".into(),
                    _ => "undefined".into(),
                },
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: Some(val),
            }
        }
    }

    pub async fn call_function_on_for_cdp(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        args: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> obscura_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.call_function_on_for_cdp(function_declaration, object_id, args, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    tracing::debug!("callFunctionOn error: {}", e);
                    obscura_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            obscura_js::runtime::RemoteObjectInfo {
                js_type: "undefined".into(),
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: None,
            }
        }
    }

    pub fn set_blocked_urls(&mut self, patterns: Vec<String>) {
        self.blocked_url_patterns = patterns.clone();
        if let Some(js) = &self.js {
            js.set_blocked_urls(patterns);
        }
    }

    pub fn release_object(&mut self, object_id: &str) {
        if let Some(js) = &mut self.js {
            js.release_object(object_id);
        }
    }

    fn record_network_event(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body_size: usize,
    ) {
        self.record_network_event_inner(url, method, resource_type, status, response_headers, body_size);
    }

    fn record_network_event_with_body(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body: &[u8],
        base64_encoded: bool,
    ) {
        let request_id = self.record_network_event_inner(
            url,
            method,
            resource_type,
            status,
            response_headers,
            body.len(),
        );
        self.store_response_body(request_id, body, base64_encoded);
    }

    fn record_network_event_inner(
        &mut self,
        url: &str,
        method: &str,
        resource_type: &str,
        status: u16,
        response_headers: &std::collections::HashMap<String, String>,
        body_size: usize,
    ) -> String {
        self.network_event_counter += 1;
        let request_id = format!("{}.{}", self.id, self.network_event_counter);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.network_events.push(NetworkEvent {
            request_id: request_id.clone(),
            url: url.to_string(),
            method: method.to_string(),
            resource_type: resource_type.to_string(),
            status,
            headers: std::collections::HashMap::new(),
            response_headers: Arc::new(response_headers.clone()),
            body_size,
            timestamp,
        });
        request_id
    }

    fn store_response_body(&mut self, request_id: String, body: &[u8], base64_encoded: bool) {
        let max_entries = response_body_entry_limit();
        let max_bytes = response_body_byte_limit();
        if max_entries == 0 || max_bytes == 0 || body.len() > max_bytes {
            return;
        }
        let body = if base64_encoded {
            BASE64.encode(body)
        } else {
            String::from_utf8_lossy(body).to_string()
        };
        self.response_bodies.insert(request_id.clone(), StoredResponseBody { body, base64_encoded });
        self.response_body_order.push_back(request_id);
        while self.response_body_order.len() > max_entries {
            if let Some(oldest) = self.response_body_order.pop_front() {
                self.response_bodies.remove(&oldest);
            }
        }
    }

    pub fn get_response_body(&self, request_id: &str) -> Option<StoredResponseBody> {
        self.response_bodies.get(request_id).cloned().or_else(|| {
            self.js.as_ref()?.get_network_response_body(request_id).map(|body| {
                StoredResponseBody {
                    body: body.body,
                    base64_encoded: body.base64_encoded,
                }
            })
        })
    }

    /// Take a stored response body as raw bytes for CDP streaming
    /// (Fetch.takeResponseBodyAsStream). Removes it from the in-memory cache and
    /// transfers ownership to the caller, so a large body is held once and freed
    /// when the stream is closed rather than lingering in this long-running
    /// process (issue #360). Binary bodies are stored base64 (byte-exact); text
    /// bodies return their UTF-8 bytes. Returns None if the body was never
    /// cached (e.g. it exceeded OBSCURA_NETWORK_BODY_BUFFER_BYTES and was
    /// dropped) or the id is unknown.
    pub fn take_response_body_raw(&mut self, request_id: &str) -> Option<Vec<u8>> {
        let stored = if let Some(body) = self.response_bodies.remove(request_id) {
            self.response_body_order.retain(|id| id != request_id);
            body
        } else {
            self.js.as_ref()?.get_network_response_body(request_id).map(|b| StoredResponseBody {
                body: b.body,
                base64_encoded: b.base64_encoded,
            })?
        };
        if stored.base64_encoded {
            BASE64.decode(stored.body.as_bytes()).ok()
        } else {
            Some(stored.body.into_bytes())
        }
    }

    /// Make the body stored under `from_id` also retrievable under `to_id`.
    /// The main navigation resource is stored under its internal request id, but
    /// the CDP layer reports it to clients with the navigation's loaderId as the
    /// requestId (Chrome's `requestId === loaderId` convention). Without this
    /// alias, `Network.getResponseBody(loaderId)` misses and a client navigating
    /// straight to an image or other resource cannot read the main-response body
    /// (issue #340).
    pub fn alias_response_body(&mut self, from_id: &str, to_id: &str) {
        if from_id == to_id || self.response_bodies.contains_key(to_id) {
            return;
        }
        if let Some(body) = self.response_bodies.get(from_id).cloned() {
            self.response_bodies.insert(to_id.to_string(), body);
            self.response_body_order.push_back(to_id.to_string());
        }
    }

    pub fn clear_response_bodies(&mut self) {
        self.response_bodies.clear();
        self.response_body_order.clear();
        if let Some(js) = &self.js {
            js.clear_network_response_bodies();
        }
    }

    pub fn execute_preload_script(&mut self, source: &str) -> Result<(), String> {
        if let Some(js) = &mut self.js {
            js.execute_script("<preload>", source)
        } else {
            Err("No JS runtime".to_string())
        }
    }

    pub fn suspend_js(&mut self) {
        let Some(js) = &self.js else {
            return;
        };
        let started_script_ids = js.started_script_ids();
        let dom = js.take_dom();
        if let Some(dom) = dom {
            self.dom = Some(dom);
            self.suspended_started_script_ids = started_script_ids;
        } else {
            self.suspended_started_script_ids.clear();
        }
        self.js = None;
    }

    pub fn resume_js(&mut self) {
        if self.js.is_some() {
            return;
        }
        let started_script_ids = std::mem::take(&mut self.suspended_started_script_ids);
        self.init_js();
        if let Some(js) = &self.js {
            js.restore_started_script_ids(&started_script_ids);
        }
    }

    pub fn has_js(&self) -> bool {
        self.js.is_some()
    }

    pub fn release_object_group(&mut self) {
        if let Some(js) = &mut self.js {
            js.release_object_group();
        }
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_navigation()
        } else {
            None
        }
    }

    pub fn take_pending_binding_calls(&self) -> Vec<(String, String)> {
        if let Some(js) = &self.js {
            js.take_pending_binding_calls()
        } else {
            Vec::new()
        }
    }

    pub fn set_preload_scripts(&mut self, scripts: Vec<String>) {
        self.preload_scripts = scripts;
    }

    /// Append a script that runs in the page before any of the page's own
    /// `<script>` tags, matching CDP `Page.addScriptToEvaluateOnNewDocument`.
    /// Takes effect on the next navigation (`goto` / `navigate*`).
    pub fn add_preload_script(&mut self, script: &str) {
        self.preload_scripts.push(script.to_string());
    }

    /// Enable CDP-Fetch-style interception of JS-initiated `fetch()`/XHR.
    /// Returns a receiver yielding every such request; resolve each through its
    /// `resolver` with `InterceptResolution::{Continue, Fulfill, Fail}` to pass,
    /// mock, or block it. Works in stealth and non-stealth. Mirrors how the CDP
    /// server wires the channel (`obscura-cdp/src/server.rs`).
    pub fn enable_interception(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<obscura_js::ops::InterceptedRequest> {
        let (tx, rx) =
            tokio::sync::mpsc::unbounded_channel::<obscura_js::ops::InterceptedRequest>();
        self.set_intercept_tx(tx);
        self.enable_intercept(true);
        rx
    }

    /// Register a passive callback fired for every JS `fetch()`/XHR (and
    /// navigation) request this page makes, once the method/headers/body are
    /// known and before it is sent. Non-blocking; use `enable_interception` to
    /// mutate or block. Returns a stable id; pass it to `off_request` to
    /// detach (issue #408). Scoped to this page: it never sees sibling pages'
    /// requests and dies with the page.
    pub fn on_request(&mut self, cb: RequestCallback) -> u64 {
        self.callbacks.add_request(cb)
    }

    /// Register a passive callback fired with every JS `fetch()`/XHR (and
    /// navigation) response this page receives, including its body.
    /// Non-blocking. The main path for crawlers that need to capture API
    /// response payloads. Returns a stable id for `off_response`. Page-scoped
    /// like `on_request`.
    pub fn on_response(&mut self, cb: ResponseCallback) -> u64 {
        self.callbacks.add_response(cb)
    }

    /// Detach a request observer registered with `on_request`. Returns true if
    /// one was removed.
    pub fn off_request(&mut self, id: u64) -> bool {
        self.callbacks.remove_request(id)
    }

    /// Detach a response observer registered with `on_response`. Returns true if
    /// one was removed.
    pub fn off_response(&mut self, id: u64) -> bool {
        self.callbacks.remove_response(id)
    }

    pub async fn process_pending_navigation(&mut self) -> Result<bool, PageError> {
        if let Some((url, method, body)) = self.take_pending_navigation() {
            self.navigate_with_wait_post(
                &url,
                crate::lifecycle::WaitUntil::Load,
                &method,
                &body,
            )
            .await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn set_intercept_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<obscura_js::ops::InterceptedRequest>) {
        self.intercept_tx = Some(tx.clone());
        if let Some(js) = &self.js {
            js.set_intercept_tx(tx);
        }
    }

    pub fn enable_intercept(&mut self, enabled: bool) {
        self.intercept_enabled = enabled;
        if let Some(js) = &self.js {
            js.set_intercept_enabled(enabled);
        }
    }
}

fn script_response_is_executable(status: u16) -> bool {
    (200..=299).contains(&status)
}

fn url_matches_cdp_pattern(pattern: &str, url: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    let mut remainder = url;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }

        let Some(index) = remainder.find(part) else {
            return false;
        };

        if first && !pattern.starts_with('*') && index != 0 {
            return false;
        }

        remainder = &remainder[index + part.len()..];
        first = false;
    }

    pattern.ends_with('*') || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        linked_stylesheet_requests, materialize_linked_stylesheet_script, parse_import_url,
        rebase_css_urls, should_restore_hydration_snapshot, split_css_imports,
        script_response_is_executable, truncate_on_char_boundary, url_matches_cdp_pattern,
        HydrationStats,
    };
    use obscura_dom::parse_html;

    fn spawn_parser_import_map_server(
        expected_requests: usize,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request
                    .lines().next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/").to_string();
                request_tx.send(path.clone()).unwrap();
                let (status, body) = match path.as_str() {
                    "/app/before.js" => ("200 OK", "export const value = 'before-first-module';"),
                    "/app/later.js" => ("200 OK", "export const value = 'later-map';"),
                    "/app/async.js" => (
                        "200 OK",
                        "import('too-late')\
                           .then(module => globalThis.__async_before_map = module.value)\
                           .catch(() => globalThis.__async_before_map = 'rejected');",
                    ),
                    _ => ("404 Not Found", "not found"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{}", address), request_rx)
    }

    #[test]
    fn external_scripts_require_a_successful_http_status() {
        assert!(script_response_is_executable(200));
        assert!(script_response_is_executable(204));
        assert!(script_response_is_executable(299));
        assert!(!script_response_is_executable(0));
        assert!(!script_response_is_executable(304));
        assert!(!script_response_is_executable(401));
        assert!(!script_response_is_executable(404));
        assert!(!script_response_is_executable(500));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn suspend_resume_preserves_document_script_start_state() {
        let context = std::sync::Arc::new(crate::BrowserContext::with_storage_and_network(
            "script-state-suspend".to_string(), None, false, None, None, true,
        ));
        let mut page = super::Page::new("script-state-suspend".to_string(), context);
        page.url = Some(url::Url::parse("http://example.com/suspend.html").unwrap());
        page.dom = Some(parse_html(
            r#"<html><head></head><body data-parser-runs="0" data-dynamic-runs="0" data-inert-runs="0">
            <script id="parser">
              document.body.setAttribute("data-parser-runs", String(Number(document.body.getAttribute("data-parser-runs")) + 1));
            </script>
            </body></html>"#,
        ));
        page.init_js();
        page.execute_scripts().await;

        let before = page
            .js
            .as_mut()
            .unwrap()
            .evaluate(
                r#"
                var scriptStateSetup = true;
                const dynamic = document.createElement("script");
                dynamic.id = "dynamic";
                dynamic.textContent =
                  'document.body.setAttribute("data-dynamic-runs", String(Number(document.body.getAttribute("data-dynamic-runs")) + 1))';
                document.body.appendChild(dynamic);

                const holder = document.createElement("div");
                holder.innerHTML =
                  '<script id="inert">document.body.setAttribute("data-inert-runs", String(Number(document.body.getAttribute("data-inert-runs")) + 1))<\/script>';
                document.body.appendChild(holder.firstChild);
                return [
                  document.body.getAttribute("data-parser-runs"),
                  document.body.getAttribute("data-dynamic-runs"),
                  document.body.getAttribute("data-inert-runs")
                ];
                "#,
            )
            .unwrap();
        assert_eq!(before, serde_json::json!(["1", "1", "0"]));

        page.suspend_js();
        page.suspend_js();
        page.resume_js();

        let after = page
            .js
            .as_mut()
            .unwrap()
            .evaluate(
                r#"
                var scriptStateCheck = true;
                for (const id of ["parser", "dynamic", "inert"]) {
                  const script = document.getElementById(id);
                  document.head.appendChild(script);
                  document.body.appendChild(script.cloneNode(true));
                }
                return [
                  document.body.getAttribute("data-parser-runs"),
                  document.body.getAttribute("data-dynamic-runs"),
                  document.body.getAttribute("data-inert-runs")
                ];
                "#,
            )
            .unwrap();
        assert_eq!(after, serde_json::json!(["1", "1", "0"]));
    }

    #[test]
    fn new_document_does_not_inherit_suspended_script_ids() {
        let context = std::sync::Arc::new(crate::BrowserContext::with_storage_and_network(
            "script-state-navigation".to_string(), None, false, None, None, true,
        ));
        let mut page = super::Page::new("script-state-navigation".to_string(), context);
        page.url = Some(url::Url::parse("http://example.com/old.html").unwrap());
        page.dom = Some(parse_html(
            "<html><head></head><body><script id=old></script></body></html>",
        ));
        page.init_js();
        page.js
            .as_mut()
            .unwrap()
            .evaluate(
                "var setup = true; const old = document.getElementById('old'); globalThis.__markParserScripts([old._nid]); return old._nid;",
            )
            .unwrap();
        page.suspend_js();

        page.url = Some(url::Url::parse("http://example.com/new.html").unwrap());
        page.dom = Some(parse_html(
            "<html><head></head><body data-fresh-runs=0><script id=fresh>document.body.setAttribute('data-fresh-runs', '1')</script></body></html>",
        ));
        page.init_js();
        let result = page
            .js
            .as_mut()
            .unwrap()
            .evaluate(
                "var check = true; document.head.appendChild(document.getElementById('fresh')); return document.body.getAttribute('data-fresh-runs');",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("1"));
    }

    fn import_map_test_page(name: &str, base: &str, html: &str) -> super::Page {
        let context = std::sync::Arc::new(crate::BrowserContext::with_storage_and_network(
            name.to_string(), None, false, None, None, true,
        ));
        let mut page = super::Page::new(name.to_string(), context);
        page.url = Some(url::Url::parse(&format!("{}/app/index.html", base)).unwrap());
        page.dom = Some(parse_html(html));
        page.init_js();
        page
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parser_import_map_before_first_module_controls_resolution() {
        let (base, requests) = spawn_parser_import_map_server(1);
        let mut page = import_map_test_page("import-map-order", &base, r#"<html><head>
            <script type="importmap">{"imports":{"ordered":"./before.js"}}</script>
            <script type="module">
                import { value } from "ordered";
                globalThis.__parser_import_map_value = value;
            </script>
            <script type="importmap">{"imports":{"ordered":"./after.js"}}</script>
        </head><body></body></html>"#);
        page.execute_scripts().await;

        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__parser_import_map_value").unwrap(),
            serde_json::json!("before-first-module"),
        );
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), "/app/before.js");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_import_map_adds_unrelated_rule_without_rebinding_resolved_rule() {
        let (base, requests) = spawn_parser_import_map_server(2);
        let mut page = import_map_test_page("multiple-import-map-order", &base, r#"<html><head>
            <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
            <script type="module">
                import { value } from "fixed";
                globalThis.__first_map_value = value;
            </script>
            <script type="importmap">{"imports":{"fixed":"./after.js","later":"./later.js"}}</script>
            <script type="module">
                import { value as fixed } from "fixed";
                import { value as later } from "later";
                globalThis.__later_map_values = [fixed, later];
            </script>
        </head><body></body></html>"#);
        page.execute_scripts().await;

        let js = page.js.as_mut().unwrap();
        assert_eq!(js.evaluate("globalThis.__first_map_value").unwrap(), serde_json::json!("before-first-module"));
        assert_eq!(js.evaluate("globalThis.__later_map_values").unwrap(), serde_json::json!(["before-first-module", "later-map"]));
        let paths = (0..2)
            .map(|_| requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"/app/before.js".to_string()), "{paths:?}");
        assert!(paths.contains(&"/app/later.js".to_string()), "{paths:?}");
        assert!(!paths.contains(&"/app/after.js".to_string()), "{paths:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn classic_dynamic_import_does_not_see_a_later_parser_import_map() {
        let (base, _requests) = spawn_parser_import_map_server(1);
        let mut page = import_map_test_page("classic-before-import-map", &base, r#"<html><head>
            <script>
                import("too-late")
                    .then(() => globalThis.__classic_before_map = "resolved")
                    .catch(() => globalThis.__classic_before_map = "rejected");
            </script>
            <script type="importmap">{"imports":{"too-late":"./later.js"}}</script>
        </head><body></body></html>"#);
        page.execute_scripts().await;
        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__classic_before_map").unwrap(),
            serde_json::json!("rejected"),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_async_classic_script_runs_before_a_later_parser_import_map() {
        let (base, requests) = spawn_parser_import_map_server(2);
        let mut page = import_map_test_page("async-classic-before-map", &base, r#"<html><head>
            <script async src="./async.js"></script>
            <script type="importmap">{"imports":{"too-late":"./later.js"}}</script>
        </head><body></body></html>"#);
        page.execute_scripts().await;
        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__async_before_map").unwrap(),
            serde_json::json!("rejected"),
        );
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), "/app/async.js");
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamically_inserted_import_map_controls_later_dynamic_import() {
        let (base, requests) = spawn_parser_import_map_server(1);
        let mut page = import_map_test_page("dynamic-import-map", &base, r#"<html><head></head><body>
            <script>
                const map = document.createElement("script");
                map.type = "importmap";
                map.textContent = JSON.stringify({imports:{dynamicName:"./later.js"}});
                document.head.appendChild(map);
                import("dynamicName")
                    .then(module => globalThis.__dynamic_map_value = module.value)
                    .catch(error => globalThis.__dynamic_map_value = error.message);
            </script>
        </body></html>"#);
        page.execute_scripts().await;
        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__dynamic_map_value").unwrap(),
            serde_json::json!("later-map"),
        );
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), "/app/later.js");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamic_import_map_uses_live_document_base_at_insertion() {
        let (base, requests) = spawn_parser_import_map_server(1);
        let mut page = import_map_test_page("dynamic-import-map-base", &base, r#"<html><head><base href="/old/"></head><body>
            <script>
                document.querySelector("base").setAttribute("href", "/app/");
                const map = document.createElement("script");
                map.type = "importmap";
                map.textContent = JSON.stringify({imports:{liveBase:"./later.js"}});
                document.head.appendChild(map);
                import("liveBase")
                    .then(module => globalThis.__dynamic_map_base = module.value)
                    .catch(error => globalThis.__dynamic_map_base = error.message);
            </script>
        </body></html>"#);
        page.execute_scripts().await;
        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__dynamic_map_base").unwrap(),
            serde_json::json!("later-map"),
        );
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), "/app/later.js");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_base_element_does_not_rebase_an_earlier_import_map() {
        let (base, requests) = spawn_parser_import_map_server(1);
        let mut page = import_map_test_page("temporal-import-map-base", &base, r#"<html><head>
            <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
            <base href="/assets/">
            <script type="module">
                import { value } from "fixed";
                globalThis.__temporal_base_value = value;
            </script>
        </head><body></body></html>"#);
        page.execute_scripts().await;
        assert_eq!(
            page.js.as_mut().unwrap().evaluate("globalThis.__temporal_base_value").unwrap(),
            serde_json::json!("before-first-module"),
        );
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), "/app/before.js");
    }

    #[cfg(feature = "render")]
    #[test]
    fn page_screenshot_uses_the_live_window_scroll_offset() {
        let context = std::sync::Arc::new(crate::BrowserContext::new("scroll-test".to_string()));
        let mut page = super::Page::new("scroll-page".to_string(), context);
        page.set_viewport((100.0, 80.0));

        let dom = parse_html(
            r#"<html style="margin:0"><body style="margin:0">
                <div style="height:80px;background:#ff0000"></div>
                <div id="second" style="height:80px;background:#0000ff"></div>
                <div style="position:fixed;left:0;top:0;width:20px;height:20px;background:#00ff00"></div>
            </body></html>"#,
        );
        let mut runtime = obscura_js::runtime::ObscuraJsRuntime::new();
        runtime.set_dom(dom);
        runtime.set_url("https://example.test/scroll");
        runtime.set_viewport(100.0, 80.0);
        runtime.run_page_init();
        page.js = Some(runtime);
        page.url = Some(url::Url::parse("https://example.test/scroll").unwrap());

        let before = page.screenshot(page.viewport).expect("top screenshot");
        assert_eq!(
            page.evaluate(
                "return (document.getElementById('second').scrollIntoView(), window.scrollY)"
            )
                .as_f64(),
            Some(80.0)
        );
        let after = page.screenshot(page.viewport).expect("scrolled screenshot");

        assert_ne!(before, after, "Page screenshot must paint the scrolled viewport");
        assert_eq!(
            page.js.as_ref().expect("runtime").scroll_offset(),
            (0.0, 80.0)
        );
    }

    #[test]
    fn truncate_never_splits_a_multibyte_char() {
        // A caller-supplied expression whose byte 80 lands inside a multi-byte
        // char would make `&expression[..80]` panic; the helper truncates safely.
        let s = format!("{}€tail", "a".repeat(79));
        assert!(!s.is_char_boundary(80), "setup: byte 80 splits the € char");
        let t = truncate_on_char_boundary(&s, 80);
        assert!(s.starts_with(t));
        assert_eq!(t.len(), 79, "should stop right before the € char");
        assert_eq!(truncate_on_char_boundary("short", 80), "short");
    }

    #[test]
    fn parse_import_url_extracts_url_forms() {
        assert_eq!(parse_import_url(" url(\"basic.css\")").as_deref(), Some("basic.css"));
        assert_eq!(parse_import_url(" url(basic.css)").as_deref(), Some("basic.css"));
        assert_eq!(parse_import_url(" \"basic.css\"").as_deref(), Some("basic.css"));
        assert_eq!(parse_import_url(" 'theme.css'").as_deref(), Some("theme.css"));
        assert_eq!(parse_import_url(" URL('x.css')").as_deref(), Some("x.css"));
    }

    #[test]
    fn parse_import_url_skips_print_and_dark_media() {
        assert_eq!(parse_import_url("url(\"p.css\") print"), None);
        assert_eq!(parse_import_url("url(\"d.css\") (prefers-color-scheme: dark)"), None);
        // screen / all and light preference still apply.
        assert_eq!(parse_import_url("url(\"s.css\") screen").as_deref(), Some("s.css"));
        assert_eq!(parse_import_url("url(\"a.css\") print, screen").as_deref(), Some("a.css"));
    }

    #[test]
    fn split_css_imports_pulls_imports_and_strips_them() {
        let css = "@import url(\"basic.css\");\nbody { color: red; }";
        let (imports, stripped) = split_css_imports(css);
        assert_eq!(imports, vec!["basic.css".to_string()]);
        assert!(!stripped.contains("@import"));
        assert!(stripped.contains("body { color: red; }"));
    }

    #[test]
    fn split_css_imports_leaves_import_free_css_untouched() {
        let css = "body { color: red; }";
        let (imports, stripped) = split_css_imports(css);
        assert!(imports.is_empty());
        assert_eq!(stripped, css);
    }

    #[test]
    fn stylesheet_asset_urls_keep_the_importing_sheets_base() {
        let base = url::Url::parse("https://example.com/css/theme/app.css").unwrap();
        let css = r#"
            .hero { background:url("../img/hero.png") }
            .icon { mask-image:URL('./icons/mark.svg') }
            .data { background:url("data:image/svg+xml,<svg></svg>") }
            .fragment { mask:url(#shape) }
            .copy::before { content:"url(../not-an-asset.png)" }
            /* url(../not-an-asset-either.png) */
        "#;
        let rebased = rebase_css_urls(css, &base);

        assert!(rebased.contains(
            r#"url("https://example.com/css/img/hero.png")"#
        ));
        assert!(rebased.contains(
            r#"url("https://example.com/css/theme/icons/mark.svg")"#
        ));
        assert!(rebased.contains(r#"url("data:image/svg+xml,<svg></svg>")"#));
        assert!(rebased.contains("url(#shape)"));
        assert!(rebased.contains(r#"content:"url(../not-an-asset.png)""#));
        assert!(rebased.contains("/* url(../not-an-asset-either.png) */"));
    }

    #[test]
    fn stylesheet_rel_token_selector_includes_preloaded_stylesheets() {
        let dom = parse_html(
            r#"<link rel="preload stylesheet" href="app.css">
               <link rel="preload" href="font.woff2">"#,
        );
        let links = dom
            .query_selector_all(r#"link[rel~="stylesheet"]"#)
            .expect("valid selector");
        assert_eq!(links.len(), 1);
        assert_eq!(
            dom.get_node(links[0])
                .and_then(|node| node.get_attribute("href").map(str::to_owned)),
            Some("app.css".to_string())
        );
    }

    #[test]
    fn media_gated_stylesheets_are_fetched_but_disabled_sheets_are_not() {
        let dom = parse_html(
            r#"<link rel="stylesheet" href="screen.css">
               <link rel="stylesheet" href="async.css" media="print"
                     onload="this.media='all'">
               <link rel="stylesheet" href="dark.css"
                     media="(prefers-color-scheme: dark)">
               <link rel="stylesheet" href="disabled.css" disabled>"#,
        );

        assert_eq!(
            linked_stylesheet_requests(&dom),
            vec![
                (0, "screen.css".to_string()),
                (1, "async.css".to_string()),
                (2, "dark.css".to_string()),
            ]
        );
    }

    #[test]
    fn print_media_onload_can_activate_a_fetched_stylesheet() {
        let dom = parse_html(
            r#"<html><head>
                <link id="async" rel="stylesheet" href="async.css" media="print"
                      onload="this.media='all';this.setAttribute('data-loaded','yes')">
            </head><body></body></html>"#,
        );
        let mut runtime = obscura_js::runtime::ObscuraJsRuntime::new();
        runtime.set_dom(dom);
        runtime.run_page_init();
        runtime
            .execute_script(
                "<async-sheet>",
                &materialize_linked_stylesheet_script(0, ".target{color:red}"),
            )
            .expect("load and materialize async linked sheet");

        let state = runtime
            .with_dom(|dom| {
                let link = dom
                    .query_selector("#async")
                    .expect("valid selector")
                    .expect("async link");
                let styles = dom
                    .query_selector_all("style[data-obscura-external-stylesheets]")
                    .expect("valid selector");
                (
                    dom.get_node(link)
                        .and_then(|node| {
                            node.get_attribute("data-loaded").map(str::to_owned)
                        }),
                    styles.first().map(|&nid| dom.text_content(nid)),
                )
            })
            .expect("live DOM");

        assert_eq!(
            state.0.as_deref(),
            Some("yes"),
            "link load handler must run"
        );
        assert_eq!(
            state.1.as_deref(),
            Some(".target{color:red}"),
            "the handler's `this.media = 'all'` must activate the sheet"
        );
    }

    #[test]
    fn true_print_stylesheet_loads_without_entering_screen_cascade() {
        let dom = parse_html(
            r#"<html><head>
                <link id="print" rel="stylesheet" href="print.css" media="print"
                      onload="this.setAttribute('data-loaded','yes')">
            </head><body></body></html>"#,
        );
        let mut runtime = obscura_js::runtime::ObscuraJsRuntime::new();
        runtime.set_dom(dom);
        runtime.run_page_init();
        runtime
            .execute_script(
                "<print-sheet>",
                &materialize_linked_stylesheet_script(0, "body{display:none}"),
            )
            .expect("finish print linked sheet load");

        let state = runtime
            .with_dom(|dom| {
                let link = dom
                    .query_selector("#print")
                    .expect("valid selector")
                    .expect("print link");
                (
                    dom.get_node(link)
                        .and_then(|node| {
                            node.get_attribute("data-loaded").map(str::to_owned)
                        }),
                    dom.query_selector_all("style[data-obscura-external-stylesheets]")
                        .expect("valid selector")
                        .len(),
                )
            })
            .expect("live DOM");

        assert_eq!(
            state.0.as_deref(),
            Some("yes"),
            "print link still fires load"
        );
        assert_eq!(
            state.1, 0,
            "print-only CSS must stay out of screen cascade"
        );
    }

    #[test]
    fn external_stylesheets_keep_their_positions_between_inline_sheets() {
        let dom = parse_html(
            r#"<html><head>
                <link rel="stylesheet" href="first.css">
                <style data-name="inline">.target{height:20px}</style>
                <link rel="preload stylesheet" href="second.css">
            </head><body></body></html>"#,
        );
        let mut runtime = obscura_js::runtime::ObscuraJsRuntime::new();
        runtime.set_dom(dom);
        runtime.run_page_init();
        runtime
            .execute_script(
                "<first-sheet>",
                &materialize_linked_stylesheet_script(0, ".target{height:10px}"),
            )
            .expect("materialize first linked sheet");
        runtime
            .execute_script(
                "<second-sheet>",
                &materialize_linked_stylesheet_script(1, ".target{height:30px}"),
            )
            .expect("materialize second linked sheet");

        let sheet_text = runtime
            .with_dom(|dom| {
                dom.query_selector_all("style")
                    .expect("valid selector")
                    .into_iter()
                    .map(|nid| dom.text_content(nid))
                    .collect::<Vec<_>>()
            })
            .expect("live DOM");
        assert_eq!(
            sheet_text,
            vec![
                ".target{height:10px}",
                ".target{height:20px}",
                ".target{height:30px}",
            ]
        );
    }

    #[test]
    fn hydration_recovery_requires_a_catastrophic_content_wipe() {
        assert!(should_restore_hydration_snapshot(
            HydrationStats {
                elements: 520,
                text_chars: 8_000,
            },
            HydrationStats {
                elements: 4,
                text_chars: 0,
            },
        ));
    }

    #[test]
    fn hydration_recovery_does_not_override_normal_rerenders() {
        assert!(!should_restore_hydration_snapshot(
            HydrationStats {
                elements: 520,
                text_chars: 8_000,
            },
            HydrationStats {
                elements: 180,
                text_chars: 3_000,
            },
        ));
        assert!(!should_restore_hydration_snapshot(
            HydrationStats {
                elements: 12,
                text_chars: 80,
            },
            HydrationStats {
                elements: 1,
                text_chars: 0,
            },
        ));
    }

    #[test]
    fn url_matches_cdp_pattern_handles_wildcards_across_url_parts() {
        assert!(url_matches_cdp_pattern(
            "*://*.gstatic.com/*.woff2",
            "https://fonts.gstatic.com/s/inter/v18/UcCO3FwrK3iLTcviYwYZ8UA3.woff2",
        ));
        assert!(url_matches_cdp_pattern(
            "*://*.google.com/maps/vt/*",
            "https://www.google.com/maps/vt/pb=!1m4!1m3",
        ));
        assert!(url_matches_cdp_pattern(
            "https://example.com/assets/*",
            "https://example.com/assets/app.js",
        ));
        assert!(!url_matches_cdp_pattern(
            "https://example.com/assets/*",
            "https://cdn.example.com/assets/app.js",
        ));
        assert!(!url_matches_cdp_pattern(
            "*://*.gstatic.com/*.woff2",
            "https://fonts.gstatic.com/s/inter/v18/font.woff",
        ));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Too many redirects (limit {0})")]
    TooManyRedirects(usize),
}

impl From<ObscuraNetError> for PageError {
    fn from(e: ObscuraNetError) -> Self {
        PageError::NetworkError(e.to_string())
    }
}

/// Whether a Content-Type is text-like and can be stored/returned as a UTF-8
/// string. Everything else (images, PDF, fonts, octet-stream) is binary and must
/// be base64-encoded so Network.getResponseBody returns intact bytes.
fn is_text_like_content_type(content_type: Option<&str>) -> bool {
    let ct = match content_type {
        Some(c) => c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase(),
        // No Content-Type: assume text (matches the HTML-parse default).
        None => return true,
    };
    if ct.is_empty() {
        return true;
    }
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/xhtml+xml"
        || ct == "application/javascript"
        || ct == "application/ecmascript"
        || ct == "image/svg+xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

fn response_body_entry_limit() -> usize {
    std::env::var("OBSCURA_NETWORK_BODY_BUFFER_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

fn response_body_byte_limit() -> usize {
    std::env::var("OBSCURA_NETWORK_BODY_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024)
}
