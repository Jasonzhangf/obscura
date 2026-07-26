//! Stylesheet cascade: parse `<style>` text into rules once, index each rule by
//! its subject key (id / class / tag), and resolve the matching declarations for
//! an element by testing only the handful of candidate rules that share a key.
//!
//! This replaces the naive "run every selector against the whole tree" approach,
//! which is O(rules x nodes) and dominated render time on large pages (thousands
//! of rules). The indexed cascade is closer to how real browsers match: bucket
//! rules, gather candidates per element, then match and sort by specificity.

use obscura_dom::selector::{CompiledSelector, Matcher, SelectorKey};
use obscura_dom::tree::{DomTree, NodeId};
use std::collections::HashMap;

use crate::LayoutStyle;

struct Rule {
    sel: CompiledSelector,
    normal_decls: String,
    important_decls: String,
    /// Source order, for breaking specificity ties (later wins).
    order: usize,
}

/// An indexed set of author rules ready for fast per-element matching.
pub struct Stylesheet {
    rules: Vec<Rule>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_local: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
    /// `sel::before{content:"..."}` / `sel::after{...}` rules with a plain
    /// string-literal `content`, matched separately from the main cascade:
    /// our selector engine has no pseudo-element matching machinery, but a
    /// `::before`/`::after` selector's *base* (everything before the
    /// pseudo-element) is an ordinary selector we can already compile and
    /// match. Extremely common on Wikipedia (`.hlist`/`.cslist` render as
    /// comma-separated lists purely via `li::before{content:", "}`); without
    /// this, adjacent list items with no real whitespace between them in the
    /// DOM run together with no separator at all.
    before_content: Vec<(CompiledSelector, String)>,
    after_content: Vec<(CompiledSelector, String)>,
}

impl Stylesheet {
    /// Parse and index a set of raw CSS sources (the text of each `<style>`
    /// block, in document order). Selectors that fail to parse are dropped.
    pub fn parse(tree: &DomTree, sources: &[String]) -> Self {
        Self::parse_for_viewport(tree, sources, (1280.0, 720.0))
    }

    /// Parse author CSS for the live CSS viewport. Media queries must use the
    /// same dimensions as layout and page JavaScript; filtering them against a
    /// fixed desktop width made responsive frameworks build one DOM while the
    /// renderer applied another breakpoint.
    pub fn parse_for_viewport(
        tree: &DomTree,
        sources: &[String],
        viewport: (f32, f32),
    ) -> Self {
        let mut sheet = Stylesheet {
            rules: Vec::new(),
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_local: HashMap::new(),
            universal: Vec::new(),
            before_content: Vec::new(),
            after_content: Vec::new(),
        };
        let mut order = 0usize;
        for src in sources {
            for (selector, decls) in parse_stylesheet_for_viewport(src, viewport) {
                let sel_trim = selector.trim();
                if let Some(base) = strip_pseudo_element(sel_trim, "before") {
                    if let (Some(content), Some(sel)) = (extract_string_content(&decls), tree.compile_rule_selector(base)) {
                        sheet.before_content.push((sel, content));
                    }
                    continue;
                }
                if let Some(base) = strip_pseudo_element(sel_trim, "after") {
                    if let (Some(content), Some(sel)) = (extract_string_content(&decls), tree.compile_rule_selector(base)) {
                        sheet.after_content.push((sel, content));
                    }
                    continue;
                }
                let Some(sel) = tree.compile_rule_selector(&selector) else { continue };
                let (normal_decls, important_decls) = crate::style::partition_declarations(&decls);
                let idx = sheet.rules.len();
                match sel.key() {
                    SelectorKey::Id(v) => sheet.by_id.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Class(v) => sheet.by_class.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Local(v) => sheet.by_local.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Universal => sheet.universal.push(idx),
                }
                sheet.rules.push(Rule { sel, normal_decls, important_decls, order });
                order += 1;
            }
        }
        sheet
    }

    /// The literal text (if any) that `::before`/`::after` rules inject for
    /// `nid`, as `(before, after)`. Last matching rule wins, approximating
    /// cascade order without tracking specificity for this small side table.
    pub fn pseudo_content(&self, tree: &DomTree, matcher: &mut Matcher, nid: NodeId) -> (Option<String>, Option<String>) {
        if self.before_content.is_empty() && self.after_content.is_empty() {
            return (None, None);
        }
        let mut before = None;
        for (sel, content) in &self.before_content {
            if matcher.matches(tree, nid, sel) {
                before = Some(content.clone());
            }
        }
        let mut after = None;
        for (sel, content) in &self.after_content {
            if matcher.matches(tree, nid, sel) {
                after = Some(content.clone());
            }
        }
        (before, after)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[doc(hidden)]
    pub fn debug_stats(&self) -> (usize, usize, usize, usize, usize) {
        (self.rules.len(), self.by_id.len(), self.by_class.len(), self.by_local.len(), self.universal.len())
    }

    /// Apply every author rule that matches `nid` to `style`, in cascade order
    /// (ascending specificity, then source order, so the winner is applied last).
    /// `id`, `classes`, and `local` are the element's precomputed keys.
    /// `parent_props` is the element's inherited custom-property map. Returns
    /// `Some(map)` (parent + this element's own `--x` declarations) when this
    /// element declares any custom properties, so the caller can thread the
    /// richer map to descendants; `None` means "reuse the parent's map".
    pub fn apply(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        id: Option<&str>,
        classes: &[String],
        local: &str,
        style: &mut LayoutStyle,
        parent_props: &HashMap<String, String>,
        inline_css: Option<&str>,
    ) -> Option<HashMap<String, String>> {
        // (specificity, order, rule index) for each matching rule.
        let mut matched: Vec<(u32, usize, usize)> = Vec::new();
        let mut consider = |bucket: Option<&Vec<usize>>, matched: &mut Vec<(u32, usize, usize)>| {
            if let Some(idxs) = bucket {
                for &i in idxs {
                    let rule = &self.rules[i];
                    if matcher.matches(tree, nid, &rule.sel) {
                        matched.push((rule.sel.specificity(), rule.order, i));
                    }
                }
            }
        };

        consider(self.by_local.get(local), &mut matched);
        if let Some(id) = id {
            consider(self.by_id.get(id), &mut matched);
        }
        for c in classes {
            consider(self.by_class.get(c), &mut matched);
        }
        if !self.universal.is_empty() {
            consider(Some(&self.universal), &mut matched);
        }

        matched.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let (inline_normal, inline_important) = inline_css
            .map(crate::style::partition_declarations)
            .unwrap_or_default();

        // Pass 1: collect this element's own custom properties (`--x: value`),
        // in cascade order (last wins), layered over the inherited map. Custom
        // properties cascade fully before any `var()` is substituted.
        let mut own: Vec<(String, String)> = Vec::new();
        let mut collect_custom = |css: &str| {
            for decl in crate::style::split_declarations(css) {
                if let Some((name, val)) = decl.split_once(':') {
                    let name = name.trim();
                    if name.starts_with("--") && name.len() > 2 {
                        own.push((name.to_string(), val.trim().to_string()));
                    }
                }
            }
        };
        for &(_, _, i) in &matched {
            collect_custom(&self.rules[i].normal_decls);
        }
        collect_custom(&inline_normal);
        for &(_, _, i) in &matched {
            collect_custom(&self.rules[i].important_decls);
        }
        collect_custom(&inline_important);
        let effective = if own.is_empty() {
            None
        } else {
            let mut m = parent_props.clone();
            for (k, v) in own {
                m.insert(k, v);
            }
            Some(m)
        };
        let props = effective.as_ref().unwrap_or(parent_props);

        // Pass 2: apply normal declarations with `var()` substituted against
        // the resolved custom-property map.
        for &(_, _, i) in &matched {
            let expanded = substitute_vars(&self.rules[i].normal_decls, props, 0);
            crate::style::apply_declarations(style, &expanded);
        }
        let expanded = substitute_vars(&inline_normal, props, 0);
        crate::style::apply_declarations(style, &expanded);
        for &(_, _, i) in &matched {
            let expanded = substitute_vars(&self.rules[i].important_decls, props, 0);
            crate::style::apply_declarations(style, &expanded);
        }
        let expanded = substitute_vars(&inline_important, props, 0);
        crate::style::apply_declarations(style, &expanded);
        effective
    }
}

/// Substitute every `var(--name, fallback?)` in `input` with its resolved
/// value from `props` (recursively, so a token that itself references another
/// token resolves; a depth cap guards against reference cycles). A missing
/// variable with no fallback expands to empty, which makes the declaration
/// invalid and dropped, exactly as CSS specifies.
pub(crate) fn substitute_vars(input: &str, props: &HashMap<String, String>, depth: u8) -> String {
    if depth > 16 || !input.contains("var(") {
        return input.to_string();
    }
    let mut out = String::new();
    let mut rest = input;
    while let Some(pos) = rest.find("var(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        // Matching close paren, respecting nesting.
        let mut d = 1i32;
        let mut end = None;
        for (i, ch) in after.char_indices() {
            match ch {
                '(' => d += 1,
                ')' => {
                    d -= 1;
                    if d == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let inner = &after[..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        let replacement = match props.get(name) {
            Some(v) => substitute_vars(v, props, depth + 1),
            None => fallback.map(|f| substitute_vars(f, props, depth + 1)).unwrap_or_default(),
        };
        out.push_str(&replacement);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// If `selector`'s rightmost compound is the pseudo-element `which`
/// (`"before"` or `"after"`, matching either the modern `::` or legacy `:`
/// form), return everything before it, trimmed. Only a trailing
/// pseudo-element is handled: `li::before` strips to `li`, but a selector
/// that uses `::before` as anything other than the final component (not
/// valid CSS in the first place) is left alone.
fn strip_pseudo_element<'a>(selector: &'a str, which: &str) -> Option<&'a str> {
    for prefix in ["::", ":"] {
        let suffix = format!("{prefix}{which}");
        if let Some(base) = selector.strip_suffix(&suffix) {
            if !base.is_empty() {
                return Some(base.trim());
            }
        }
    }
    None
}

/// Pull a plain quoted-string `content` value out of a declaration list
/// (`content: ", "` -> `Some(", ")`). Deliberately narrow: real `content`
/// also accepts `counter()`, `attr()`, `url()` and concatenations of these,
/// none of which apply here (no counters or `attr()`-driven content on the
/// pages this targets), so anything other than a single quoted string is
/// left unhandled rather than guessed at. When `content` is declared more
/// than once (the `content: X; content: X / Y` accessible-alt-text pattern),
/// the last one wins, matching normal CSS declaration order.
fn extract_string_content(decls: &str) -> Option<String> {
    let mut result = None;
    let mut search_from = 0;
    while let Some(rel_idx) = decls[search_from..].find("content") {
        let idx = search_from + rel_idx;
        search_from = idx + "content".len();
        let after = decls[search_from..].trim_start();
        let Some(after_colon) = after.strip_prefix(':') else { continue };
        let after_colon = after_colon.trim_start();
        let Some(quote) = after_colon.chars().next() else { continue };
        if quote != '"' && quote != '\'' {
            continue;
        }
        let rest = &after_colon[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else { continue };
        result = Some(unescape_css_string(&rest[..end]));
    }
    result
}

/// Decode CSS string escapes: `\` followed by 1-6 hex digits is a Unicode
/// code point (`\200B` -> U+200B ZERO WIDTH SPACE, ubiquitous in generated
/// `content` for accessible section-edit-link brackets and similar), with a
/// single trailing whitespace character consumed as the escape's own
/// terminator per the CSS spec rather than treated as literal content;
/// anything else after a backslash (`\"`, `\\`) is a literal escaped
/// character. Without this, a hex escape prints as its own literal digits.
fn unescape_css_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 {
            match chars.peek() {
                Some(h) if h.is_ascii_hexdigit() => {
                    hex.push(*h);
                    chars.next();
                }
                _ => break,
            }
        }
        if !hex.is_empty() {
            if matches!(chars.peek(), Some(next) if next.is_whitespace()) {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
                continue;
            }
        }
        if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}

/// Split a stylesheet into `(selector, declarations)` rules. Handles nested
/// braces, `/* comments */`, and the at-rules that carry ordinary rules inside
/// (`@media`, `@supports`, `@layer`); other at-rules (`@font-face`, `@keyframes`, ...) are
/// skipped since they do not contribute layout-relevant declarations here.
pub fn parse_stylesheet(css: &str) -> Vec<(String, String)> {
    parse_stylesheet_for_viewport(css, (1280.0, 720.0))
}

fn parse_stylesheet_for_viewport(
    css: &str,
    viewport: (f32, f32),
) -> Vec<(String, String)> {
    let mut rules = Vec::new();
    let mut current_selector = String::new();
    let mut current_decls = String::new();
    let mut block_depth = 0;
    let mut in_comment = false;
    let mut chars = css.chars().peekable();

    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
            continue;
        }

        if c == '{' {
            if block_depth != 0 {
                current_decls.push(c);
            }
            block_depth += 1;
        } else if c == '}' && block_depth == 0 {
            // Stray top-level close brace (unbalanced author CSS; remoteok.com
            // ships one mid-sheet). Browsers error-recover and keep parsing;
            // without this block_depth goes negative and the state machine
            // inverts, scrambling and losing every rule in the rest of the sheet.
            current_selector.clear();
        } else if c == '}' {
            block_depth -= 1;
            if block_depth == 0 {
                let sel = current_selector.trim();
                let decls = current_decls.trim();
                if let Some(at) = sel.strip_prefix('@') {
                    flush_at_rule(at, sel, decls, &mut rules, viewport);
                } else {
                    // The body may contain nested rules (CSS Nesting, ubiquitous
                    // in Tailwind v4 / modern frameworks: `.a{ &:hover{} .b{} }`).
                    // Flatten them against this selector; denest also handles the
                    // no-nesting case (just emits the rule's own declarations).
                    denest(sel, decls, &mut rules, viewport);
                }
                current_selector.clear();
                current_decls.clear();
            } else {
                current_decls.push(c);
            }
        } else if c == ';' && block_depth == 0 {
            // A `;` at the top level terminates an at-statement that carries no
            // rules (`@layer a, b;`, `@import ...;`, `@charset ...;`). Discard
            // the buffered text so it cannot bleed into the next rule's selector
            // and take a real rule down with it.
            current_selector.clear();
        } else if block_depth > 0 {
            current_decls.push(c);
        } else {
            current_selector.push(c);
        }
    }
    rules
}

/// Handle the at-rules whose bodies contain ordinary rules. For `@media`, apply
/// the inner rules only when the query holds for a desktop 1280px viewport.
fn flush_at_rule(
    at: &str,
    sel: &str,
    inner: &str,
    rules: &mut Vec<(String, String)>,
    viewport: (f32, f32),
) {
    if at.starts_with("media") {
        if media_query_applies_for_viewport(sel, viewport) {
            rules.extend(parse_stylesheet_for_viewport(inner, viewport));
        }
    } else if at.starts_with("supports") {
        rules.extend(parse_stylesheet_for_viewport(inner, viewport));
    } else if at.starts_with("property") {
        // `@property --x { syntax:..; initial-value: V }` declares a custom
        // property and its default. Register the initial-value as a `:root`
        // declaration so `var(--x)` resolves when nothing else sets it. Tailwind
        // v4 and modern frameworks define theme tokens this way; dropping them
        // left those `var()`s (page/section backgrounds, colors) unresolved.
        let name = at["property".len()..].trim();
        if name.starts_with("--") {
            for decl in inner.split(';') {
                if let Some((k, v)) = decl.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("initial-value") && !v.trim().is_empty() {
                        rules.push((":root".to_string(), format!("{}: {}", name, v.trim())));
                    }
                }
            }
        }
    } else if at.starts_with("layer") {
        // Cascade layers: `@layer name { ... }` wraps ordinary rules. We do not
        // model layer priority (real CSS ranks unlayered above layered and
        // later layers above earlier); just flatten the body in source order so
        // the (specificity, source-order) cascade applies it. Tailwind/UnoCSS,
        // Nuxt UI and similar wrap nearly all their CSS, including the `:root`
        // design tokens and background/color utilities, in `@layer`; dropping it
        // left whole pages unstyled (white backgrounds, collapsed layout). The
        // `@layer a, b;` ordering-statement form has no block and is discarded
        // by parse_stylesheet's top-level `;` handling.
        rules.extend(parse_stylesheet_for_viewport(inner, viewport));
    }
    // Other at-rules (@font-face, @keyframes, @import, ...) carry no
    // layout-relevant rules for us, so drop them.
}

/// Coarse `@media` evaluation against an assumed 1280px-wide desktop viewport.
///
/// Real stylesheets format media features inconsistently
/// (`max-width:750px`, `max-width: 750px`, even `max-width : 750px`), so this
/// strips whitespace before scanning: CSS gives no semantic meaning to spaces
/// inside `(feature: value)`, so it's safe to discard them wholesale rather
/// than special-case every formatting variant a site might use.
pub(crate) fn media_query_applies(query: &str) -> bool {
    media_query_applies_for_viewport(query, (1280.0, 720.0))
}

pub(crate) fn media_query_applies_for_viewport(
    query: &str,
    viewport: (f32, f32),
) -> bool {
    // A media-query list is an OR, not an AND. Evaluate each top-level comma
    // arm independently (commas inside functions such as rgb() / calc() are
    // not list separators). This also keeps an inapplicable `print` arm from
    // suppressing a later screen/feature arm.
    split_media_query_list(query)
        .into_iter()
        .any(|query| single_media_query_applies_for_viewport(query, viewport))
}

fn single_media_query_applies_for_viewport(
    query: &str,
    viewport: (f32, f32),
) -> bool {
    let viewport_w = viewport.0;
    let viewport_h = viewport.1;
    if query.contains("print") {
        return false;
    }
    let compact: String = query.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect();

    // A `not all` / `not all and (...)` query is a browser-targeting hack (most
    // commonly the Safari-only flex-gap fallback `@media not all and
    // (min-resolution:.001dpcm)`, which flips containers from flex to grid); it
    // does not apply in a Chromium-like context.
    if compact.starts_with("notall") {
        return false;
    }

    // Color-scheme: we render the light (default) context. A site's
    // `@media (prefers-color-scheme: dark)` block must NOT apply on top of its
    // light defaults (that is what was leaking dark backgrounds, e.g. near
    // black inline <code>); a `:light` block should apply.
    if compact.contains("prefers-color-scheme:dark") {
        return false;
    }
    // Reduced-motion / high-contrast / inverted: default (no preference).
    if compact.contains("prefers-reduced-motion:reduce")
        || compact.contains("prefers-contrast:more")
        || compact.contains("prefers-contrast:less")
        || compact.contains("inverted-colors:inverted")
        || compact.contains("forced-colors:active")
    {
        return false;
    }

    // Width constraints, both `min-width:`/`max-width:` and the modern range
    // forms `width>=Npx` / `(Npx<=width)`.
    if let Some(px) = extract_length(&compact, "max-width:", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "min-width:", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width<=", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width>=", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width>", viewport, LengthAxis::Width) {
        if viewport_w <= px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width<", viewport, LengthAxis::Width) {
        if viewport_w >= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<=width", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<width", viewport, LengthAxis::Width) {
        if viewport_w <= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">=width", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">width", viewport, LengthAxis::Width) {
        if viewport_w >= px {
            return false;
        }
    }

    if let Some(px) = extract_length(&compact, "max-height:", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "min-height:", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height<=", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height>=", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height>", viewport, LengthAxis::Height) {
        if viewport_h <= px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height<", viewport, LengthAxis::Height) {
        if viewport_h >= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<=height", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<height", viewport, LengthAxis::Height) {
        if viewport_h <= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">=height", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">height", viewport, LengthAxis::Height) {
        if viewport_h >= px {
            return false;
        }
    }
    if compact.contains("orientation:portrait") && viewport_w > viewport_h {
        return false;
    }
    if compact.contains("orientation:landscape") && viewport_h > viewport_w {
        return false;
    }
    true
}

/// Combine a nested selector `child` with its enclosing `parent` (CSS Nesting).
/// `&` in the child is replaced by the parent; a child with no `&` becomes a
/// descendant (`parent child`). Both may be comma lists, so the result is the
/// cartesian product. `parent` is None only at the stylesheet top level, where
/// the child is returned unchanged.
fn combine_selectors(parent: &str, child: &str) -> String {
    let pparts = split_selector_list(parent);
    let cparts = split_selector_list(child);
    let mut out: Vec<String> = Vec::new();
    for c in &cparts {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        for p in &pparts {
            let p = p.trim();
            if c.contains('&') {
                out.push(c.replace('&', p));
            } else {
                out.push(format!("{} {}", p, c));
            }
        }
    }
    out.join(", ")
}

/// Flatten a rule body that may contain nested rules (CSS Nesting) into flat
/// `(selector, declarations)` pairs. The parser hands us the whole body of a
/// rule; here we separate its own declarations from nested `sel { ... }` blocks
/// (and nested `@media`/`@supports`/`@layer` at-rules, which keep the parent's
/// selector), emit `(sel, own-declarations)`, and recurse into each nested rule
/// with the combined selector. Without this, Tailwind v4 / modern-framework CSS
/// (which nests almost everything) loses the nested utility rules entirely.
fn denest(
    sel: &str,
    body: &str,
    rules: &mut Vec<(String, String)>,
    viewport: (f32, f32),
) {
    let chars: Vec<char> = body.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut seg = 0; // start of the current declaration / nested prelude
    let mut own = String::new();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut paren = 0i32;
    while i < n {
        let c = chars[i];
        if comment {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            comment = true;
            i += 2;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => paren += 1,
            ')' => paren = (paren - 1).max(0),
            '{' if paren == 0 => {
                let prelude: String = chars[seg..i].iter().collect();
                // Find the matching close brace (quote/comment aware).
                let mut depth = 1;
                let mut j = i + 1;
                let (mut q2, mut cm2) = (None::<char>, false);
                while j < n && depth > 0 {
                    let cj = chars[j];
                    if cm2 {
                        if cj == '*' && chars.get(j + 1) == Some(&'/') {
                            cm2 = false;
                            j += 2;
                            continue;
                        }
                    } else if let Some(qq) = q2 {
                        if cj == qq {
                            q2 = None;
                        }
                    } else if cj == '/' && chars.get(j + 1) == Some(&'*') {
                        cm2 = true;
                        j += 2;
                        continue;
                    } else if cj == '\'' || cj == '"' {
                        q2 = Some(cj);
                    } else if cj == '{' {
                        depth += 1;
                    } else if cj == '}' {
                        depth -= 1;
                    }
                    j += 1;
                }
                let inner: String = chars[i + 1..j.saturating_sub(1).max(i + 1)].iter().collect();
                let pre = prelude.trim();
                if let Some(at) = pre.strip_prefix('@') {
                    // A nested at-rule keeps the enclosing selector for its body.
                    if at.starts_with("media") {
                        if media_query_applies_for_viewport(pre, viewport) {
                            denest(sel, &inner, rules, viewport);
                        }
                    } else if at.starts_with("supports") || at.starts_with("layer") {
                        denest(sel, &inner, rules, viewport);
                    }
                } else if !pre.is_empty() {
                    let full = combine_selectors(sel, pre);
                    denest(&full, &inner, rules, viewport);
                }
                i = j;
                seg = i;
                continue;
            }
            ';' if paren == 0 => {
                let d: String = chars[seg..i].iter().collect();
                let d = d.trim();
                if !d.is_empty() {
                    own.push_str(d);
                    own.push(';');
                }
                i += 1;
                seg = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let tail: String = chars[seg..].iter().collect();
    let tail = tail.trim();
    if !tail.is_empty() && !tail.contains('{') {
        own.push_str(tail);
        own.push(';');
    }
    if !own.trim().is_empty() {
        for s in split_selector_list(sel) {
            let s = s.trim();
            if !s.is_empty() {
                rules.push((s.to_string(), own.clone()));
            }
        }
    }
}

/// Split a CSS selector list at top-level commas only, leaving commas inside
/// `()` (e.g. `:is(a, b)`, `:not(.x, .y)`) and `[]` (`[attr="a,b"]`) and quoted
/// strings intact. A naive `split(',')` shatters those grouped selectors into
/// fragments that fail to compile, dropping the whole rule.
fn split_selector_list(sel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let mut chars = sel.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            _ if quote == Some(c) => {
                quote = None;
                cur.push(c);
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            '(' if quote.is_none() => {
                depth_paren += 1;
                cur.push(c);
            }
            ')' if quote.is_none() => {
                depth_paren -= 1;
                cur.push(c);
            }
            '[' if quote.is_none() => {
                depth_brack += 1;
                cur.push(c);
            }
            ']' if quote.is_none() => {
                depth_brack -= 1;
                cur.push(c);
            }
            ',' if quote.is_none() && depth_paren == 0 && depth_brack == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[derive(Clone, Copy)]
enum LengthAxis {
    Width,
    Height,
}

/// Read a CSS length immediately following `prop`. Media-query `em` and `rem`
/// units resolve against the initial font size (16 CSS px), not an element's
/// computed font. Modern utility frameworks deliberately use those units for
/// breakpoints, so treating only `px` as typed made every `min-width:64rem`
/// desktop rule unconditional.
fn extract_length(
    s: &str,
    prop: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let start = s.find(prop)? + prop.len();
    let rest = &s[start..];
    if let Some(inner) = rest.strip_prefix("calc(") {
        let end = inner.find(')')?;
        return eval_length_sum(&inner[..end], viewport, axis);
    }
    parse_length_prefix(rest, viewport, axis)
}

/// Read the length immediately before a range marker (`64rem<=width`).
fn extract_length_before(
    s: &str,
    marker: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let end = s.find(marker)?;
    let prefix = &s[..end];
    let start = prefix
        .rfind(|c: char| matches!(c, '(' | ')' | ':' | ','))
        .map_or(0, |idx| idx + 1);
    let value = &prefix[start..];
    if let Some(inner) = value.strip_prefix("calc(") {
        return eval_length_sum(inner, viewport, axis);
    }
    parse_length_prefix(value, viewport, axis)
}

fn parse_length_prefix(
    input: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let numeric_len = input
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit() || matches!(c, '.' | '+' | '-'))
        .last()
        .map_or(0, |(idx, c)| idx + c.len_utf8());
    if numeric_len == 0 {
        return None;
    }
    let value = input[..numeric_len].parse::<f32>().ok()?;
    let unit: String = input[numeric_len..]
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '%')
        .collect();
    let px = match unit.as_str() {
        "" | "px" => value,
        "em" | "rem" => value * 16.0,
        "vw" => value * viewport.0 / 100.0,
        "vh" => value * viewport.1 / 100.0,
        "vmin" => value * viewport.0.min(viewport.1) / 100.0,
        "vmax" => value * viewport.0.max(viewport.1) / 100.0,
        "in" => value * 96.0,
        "cm" => value * 96.0 / 2.54,
        "mm" => value * 96.0 / 25.4,
        "q" => value * 96.0 / 101.6,
        "pt" => value * 96.0 / 72.0,
        "pc" => value * 16.0,
        "%" => match axis {
            LengthAxis::Width => value * viewport.0 / 100.0,
            LengthAxis::Height => value * viewport.1 / 100.0,
        },
        _ => return None,
    };
    px.is_finite().then_some(px)
}

/// Sum the common media-query `calc()` form (`64rem - 1px`) left to right.
fn eval_length_sum(
    expr: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let mut total = 0.0;
    let mut sign = 1.0;
    let mut term = String::new();
    let flush = |term: &mut String, sign: f32, total: &mut f32| -> Option<()> {
        let value = parse_length_prefix(term, viewport, axis)?;
        *total += sign * value;
        term.clear();
        Some(())
    };
    for (idx, c) in expr.char_indices() {
        match c {
            '+' if idx > 0 => {
                flush(&mut term, sign, &mut total)?;
                sign = 1.0;
            }
            '-' if idx > 0 => {
                flush(&mut term, sign, &mut total)?;
                sign = -1.0;
            }
            _ => term.push(c),
        }
    }
    flush(&mut term, sign, &mut total)?;
    Some(total)
}

fn split_media_query_list(query: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    for (idx, c) in query.char_indices() {
        match c {
            _ if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '(' if quote.is_none() => depth += 1,
            ')' if quote.is_none() => depth = (depth - 1).max(0),
            ',' if quote.is_none() && depth == 0 => {
                parts.push(query[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(query[start..].trim());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_rules_use_the_live_width_and_height() {
        let css = r#"
            .base { color: black; }
            @media (max-width: 950px) { .narrow { color: green; } }
            @media (min-height: 900px) { .tall { color: blue; } }
            @media (orientation: portrait) { .portrait { color: red; } }
        "#;
        let selectors = |viewport| {
            parse_stylesheet_for_viewport(css, viewport)
                .into_iter()
                .map(|(selector, _)| selector)
                .collect::<Vec<_>>()
        };

        let narrow_tall = selectors((900.0, 1000.0));
        assert!(narrow_tall.iter().any(|s| s == ".base"));
        assert!(narrow_tall.iter().any(|s| s == ".narrow"));
        assert!(narrow_tall.iter().any(|s| s == ".tall"));
        assert!(narrow_tall.iter().any(|s| s == ".portrait"));

        let wide_short = selectors((1280.0, 720.0));
        assert!(wide_short.iter().any(|s| s == ".base"));
        assert!(!wide_short.iter().any(|s| s == ".narrow"));
        assert!(!wide_short.iter().any(|s| s == ".tall"));
        assert!(!wide_short.iter().any(|s| s == ".portrait"));
    }

    #[test]
    fn nested_media_rules_use_the_live_viewport() {
        let css = ".card { display:block; @media (max-width: 950px) { width:100%; } }";
        let narrow = parse_stylesheet_for_viewport(css, (900.0, 1000.0));
        let wide = parse_stylesheet_for_viewport(css, (1280.0, 720.0));
        assert!(
            narrow
                .iter()
                .any(|(selector, declarations)| selector == ".card"
                    && declarations.contains("width:100%"))
        );
        assert!(
            !wide
                .iter()
                .any(|(_, declarations)| declarations.contains("width:100%"))
        );
    }

    #[test]
    fn media_breakpoints_support_font_relative_lengths_and_ranges() {
        assert!(!media_query_applies_for_viewport(
            "@media (min-width: 64rem)",
            (900.0, 1000.0)
        ));
        assert!(media_query_applies_for_viewport(
            "@media (min-width: 64rem)",
            (1024.0, 768.0)
        ));
        assert!(media_query_applies_for_viewport(
            "@media (56.25rem <= width)",
            (900.0, 1000.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media (width > calc(60em - 1px))",
            (900.0, 1000.0)
        ));
    }

    #[test]
    fn media_query_lists_are_or_conditions() {
        assert!(media_query_applies_for_viewport(
            "@media print, (min-width: 64rem)",
            (1280.0, 720.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media print, (min-width: 64rem)",
            (900.0, 1000.0)
        ));
    }

    #[test]
    fn rem_breakpoint_does_not_reveal_desktop_menu_on_narrow_viewport() {
        let css = r#"
            header .menu-toolkit { display: none }
            @media (min-width: 64rem) {
                header .menu-toolkit { display: flex }
            }
        "#;
        let narrow = parse_stylesheet_for_viewport(css, (900.0, 1000.0));
        assert!(narrow.iter().any(|(selector, declarations)| {
            selector == "header .menu-toolkit" && declarations.contains("display: none")
        }));
        assert!(!narrow.iter().any(|(_, declarations)| declarations.contains("display: flex")));

        let wide = parse_stylesheet_for_viewport(css, (1024.0, 768.0));
        assert!(wide.iter().any(|(_, declarations)| declarations.contains("display: flex")));
    }
}
