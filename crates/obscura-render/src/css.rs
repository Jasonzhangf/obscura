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

struct PseudoRule {
    sel: CompiledSelector,
    normal_decls: String,
    important_decls: String,
    order: usize,
}

/// An indexed set of author rules ready for fast per-element matching.
pub struct Stylesheet {
    rules: Vec<Rule>,
    /// Final declarations from `@keyframes name { ... 100%/to { ... } }`.
    /// Static screenshots are taken after the requested settle interval, so a
    /// finite forwards-filled animation contributes this declaration block.
    keyframe_ends: HashMap<String, String>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_local: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
    /// `sel::before` / `sel::after` rules matched against their ordinary base
    /// selector. Keeping their full declaration cascade supports both literal
    /// generated text and positioned decorative boxes.
    before_rules: Vec<PseudoRule>,
    after_rules: Vec<PseudoRule>,
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
            keyframe_ends: HashMap::new(),
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_local: HashMap::new(),
            universal: Vec::new(),
            before_rules: Vec::new(),
            after_rules: Vec::new(),
        };
        let mut order = 0usize;
        for src in sources {
            for (name, decls) in extract_keyframe_end_states(src) {
                sheet.keyframe_ends.insert(name, decls);
            }
            for (selector, decls) in parse_stylesheet_for_viewport(src, viewport) {
                let sel_trim = selector.trim();
                if let Some(base) = strip_pseudo_element(sel_trim, "before") {
                    if let Some(sel) = tree.compile_rule_selector(base) {
                        let (normal_decls, important_decls) =
                            crate::style::partition_declarations(&decls);
                        sheet.before_rules.push(PseudoRule {
                            sel,
                            normal_decls,
                            important_decls,
                            order,
                        });
                    }
                    order += 1;
                    continue;
                }
                if let Some(base) = strip_pseudo_element(sel_trim, "after") {
                    if let Some(sel) = tree.compile_rule_selector(base) {
                        let (normal_decls, important_decls) =
                            crate::style::partition_declarations(&decls);
                        sheet.after_rules.push(PseudoRule {
                            sel,
                            normal_decls,
                            important_decls,
                            order,
                        });
                    }
                    order += 1;
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

    pub fn pseudo_styles(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        props: &HashMap<String, String>,
        host_style: &LayoutStyle,
    ) -> (Option<LayoutStyle>, Option<LayoutStyle>) {
        let build = |rules: &[PseudoRule], matcher: &mut Matcher| {
            let mut matched: Vec<(u32, usize, &PseudoRule)> = rules
                .iter()
                .filter(|rule| matcher.matches(tree, nid, &rule.sel))
                .map(|rule| (rule.sel.specificity(), rule.order, rule))
                .collect();
            if matched.is_empty() {
                return None;
            }
            matched.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            // Generated ::before/::after boxes have an inline outer display
            // by default. LayoutStyle's general default is block because it
            // primarily represents ordinary DOM boxes, so set the pseudo
            // initial value explicitly before applying author declarations.
            let mut style = LayoutStyle {
                display: crate::Display::Inline,
                ..Default::default()
            };
            style.color_scheme_dark = host_style.color_scheme_dark;
            let inherited_color_scheme_dark = host_style.color_scheme_dark;
            let mut content = None;
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.normal_decls, props);
                crate::style::apply_color_scheme_declarations_from(
                    &mut style,
                    &expanded,
                    inherited_color_scheme_dark,
                );
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.important_decls, props);
                crate::style::apply_color_scheme_declarations_from(
                    &mut style,
                    &expanded,
                    inherited_color_scheme_dark,
                );
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.normal_decls, props);
                crate::style::apply_declarations_with_locked_color_scheme(
                    &mut style,
                    &expanded,
                );
                if let Some(value) = extract_content(&expanded, tree, nid) {
                    content = value;
                }
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.important_decls, props);
                crate::style::apply_declarations_with_locked_color_scheme(
                    &mut style,
                    &expanded,
                );
                if let Some(value) = extract_content(&expanded, tree, nid) {
                    content = value;
                }
            }
            style.before_content = content;
            if style.before_content.is_some() || style.content_image.is_some() {
                Some(style)
            } else {
                None
            }
        };
        (
            build(&self.before_rules, matcher),
            build(&self.after_rules, matcher),
        )
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
                match v.trim().to_ascii_lowercase().as_str() {
                    // CSS-wide keywords on a custom property are cascade
                    // instructions, not literal token streams. `initial`
                    // produces the guaranteed-invalid value so var() must use
                    // its fallback even when an ancestor defined the token.
                    "initial" => {
                        m.remove(&k);
                    }
                    // Custom properties inherit by default. `unset` therefore
                    // has the same effect as `inherit`; approximate both
                    // revert forms with the inherited author value as well.
                    "inherit" | "unset" | "revert" | "revert-layer" => {
                        if let Some(inherited) = parent_props.get(&k) {
                            m.insert(k, inherited.clone());
                        } else {
                            m.remove(&k);
                        }
                    }
                    _ => {
                        m.insert(k, v);
                    }
                }
            }
            Some(m)
        };
        let props = effective.as_ref().unwrap_or(parent_props);

        let inherited_color_scheme_dark = style.color_scheme_dark;
        // `light-dark()` resolves against the element's final used color
        // scheme, not the declaration order. Determine the scheme winner
        // across the complete author cascade before applying any color-valued
        // property. The style starts with its inherited scheme.
        for &(_, _, i) in &matched {
            let expanded =
                substitute_declarations(&self.rules[i].normal_decls, props);
            crate::style::apply_color_scheme_declarations_from(
                style,
                &expanded,
                inherited_color_scheme_dark,
            );
        }
        let expanded = substitute_declarations(&inline_normal, props);
        crate::style::apply_color_scheme_declarations_from(
            style,
            &expanded,
            inherited_color_scheme_dark,
        );
        for &(_, _, i) in &matched {
            let expanded =
                substitute_declarations(&self.rules[i].important_decls, props);
            crate::style::apply_color_scheme_declarations_from(
                style,
                &expanded,
                inherited_color_scheme_dark,
            );
        }
        let expanded = substitute_declarations(&inline_important, props);
        crate::style::apply_color_scheme_declarations_from(
            style,
            &expanded,
            inherited_color_scheme_dark,
        );

        // Pass 2: apply normal declarations with `var()` substituted against
        // the resolved custom-property map.
        for &(_, _, i) in &matched {
            let expanded = substitute_declarations(&self.rules[i].normal_decls, props);
            crate::style::apply_declarations_with_locked_color_scheme(
                style,
                &expanded,
            );
        }
        let expanded = substitute_declarations(&inline_normal, props);
        crate::style::apply_declarations_with_locked_color_scheme(style, &expanded);

        // CSS animations form a cascade origin above normal author rules and
        // below author !important. A static renderer has no compositor clock;
        // after page settling, the useful deterministic state for a finite
        // forwards/both animation is its final keyframe. Infinite animations
        // intentionally keep their ordinary computed values.
        if style.animation_fill_forwards && !style.animation_iteration_infinite {
            if let Some(name) = style.animation_name.as_deref() {
                if let Some(decls) = self.keyframe_ends.get(name) {
                    let expanded = substitute_declarations(decls, props);
                    crate::style::apply_declarations_with_locked_color_scheme(
                        style,
                        &expanded,
                    );
                }
            }
        }

        for &(_, _, i) in &matched {
            let expanded = substitute_declarations(&self.rules[i].important_decls, props);
            crate::style::apply_declarations_with_locked_color_scheme(
                style,
                &expanded,
            );
        }
        let expanded = substitute_declarations(&inline_important, props);
        crate::style::apply_declarations_with_locked_color_scheme(style, &expanded);
        effective
    }
}

/// Collect the declaration block for each keyframes rule's `to`/`100%` stop.
/// This is deliberately independent of selector parsing: keyframe selectors
/// are percentages, not DOM selectors, and must never enter the rule index.
fn extract_keyframe_end_states(css: &str) -> Vec<(String, String)> {
    let lower = css.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut cursor = 0usize;

    while cursor < css.len() {
        let standard = lower[cursor..].find("@keyframes").map(|offset| {
            (cursor + offset, "@keyframes".len())
        });
        let webkit = lower[cursor..].find("@-webkit-keyframes").map(|offset| {
            (cursor + offset, "@-webkit-keyframes".len())
        });
        let Some((start, keyword_len)) = (match (standard, webkit) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }) else {
            break;
        };

        let after_keyword = start + keyword_len;
        let Some(open_rel) = css[after_keyword..].find('{') else { break };
        let open = after_keyword + open_rel;
        let name = css[after_keyword..open].trim();
        if name.is_empty() {
            cursor = open + 1;
            continue;
        }

        let mut depth = 1i32;
        let mut in_quote: Option<char> = None;
        let mut escaped = false;
        let mut close = None;
        for (offset, ch) in css[open + 1..].char_indices() {
            if let Some(quote) = in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    in_quote = None;
                }
                continue;
            }
            if ch == '"' || ch == '\'' {
                in_quote = Some(ch);
            } else if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + 1 + offset);
                    break;
                }
            }
        }
        let Some(close) = close else { break };
        let inner = &css[open + 1..close];
        let mut final_decls = None;
        for (selector, decls) in parse_stylesheet_for_viewport(inner, (1280.0, 720.0)) {
            if selector
                .split(',')
                .any(|part| matches!(part.trim().to_ascii_lowercase().as_str(), "to" | "100%"))
            {
                final_decls = Some(decls);
            }
        }
        if let Some(decls) = final_decls {
            found.push((name.to_string(), decls));
        }
        cursor = close + 1;
    }
    found
}

/// Resolve variables one declaration at a time. An invalid variable poisons
/// its entire declaration at computed-value time, but must not erase unrelated
/// declarations in the same rule.
fn substitute_declarations(css: &str, props: &HashMap<String, String>) -> String {
    let mut expanded = String::new();
    for declaration in crate::style::split_declarations(css) {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let Some(value) = substitute_var_value(value.trim(), props, 0) else {
            continue;
        };
        expanded.push_str(name.trim());
        expanded.push(':');
        expanded.push_str(&value);
        expanded.push(';');
    }
    expanded
}

/// Substitute every `var(--name, fallback?)` in one property value. `None`
/// represents CSS's guaranteed-invalid value. Crucially, invalidity propagates
/// through an intermediate custom property so an outer var() can use its own
/// fallback (`--toggle:var(--missing) dark; color:var(--toggle,light)`).
fn substitute_var_value(input: &str, props: &HashMap<String, String>, depth: u8) -> Option<String> {
    if depth > 16 {
        return None;
    }
    if !input.contains("var(") {
        return Some(input.to_string());
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
            return None;
        };
        let inner = &after[..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        let resolved = props
            .get(name)
            .and_then(|value| substitute_var_value(value, props, depth + 1));
        let replacement = match resolved {
            Some(value) => value,
            None => {
                let fallback = fallback?;
                substitute_var_value(fallback, props, depth + 1)?
            }
        };
        out.push_str(&replacement);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
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

/// Return the final valid `content` declaration in a declaration list.
///
/// The outer option says whether a declaration was found; the inner option is
/// the generated text (`none`/`normal` suppress the pseudo). Along with quoted
/// strings, support the common `attr(name)` form used by component-library
/// buttons and badges. The attribute is resolved against the originating
/// element, as CSS generated content requires.
fn extract_content(decls: &str, tree: &DomTree, nid: NodeId) -> Option<Option<String>> {
    let mut result = None;
    for raw in crate::style::split_declarations(decls) {
        let Some((name, value)) = raw.split_once(':') else { continue };
        if !name.trim().eq_ignore_ascii_case("content") {
            continue;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("normal") {
            result = Some(None);
            continue;
        }
        let parsed = if let Some(quote) =
            value.chars().next().filter(|ch| matches!(ch, '"' | '\''))
        {
            let rest = &value[quote.len_utf8()..];
            rest.find(quote).map(|end| unescape_css_string(&rest[..end]))
        } else if let Some(rest) = value.strip_prefix("attr(") {
            rest.find(')').map(|end| {
                tree.get_node(nid)
                    .and_then(|node| {
                        node.get_attribute(rest[..end].trim()).map(str::to_owned)
                    })
                    .unwrap_or_default()
            })
        } else {
            None
        };
        if let Some(parsed) = parsed {
            result = Some(Some(parsed));
        } else if value
            .trim_start()
            .get(..4)
            .map_or(false, |prefix| prefix.eq_ignore_ascii_case("url("))
        {
            // An image-valued content declaration supersedes any earlier
            // string declaration. The image itself is retained on
            // LayoutStyle::content_image by apply_declarations; clearing the
            // text here keeps the two views of the winning declaration in
            // sync and, importantly, keeps the pseudo alive.
            result = Some(None);
        }
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
        if supports_condition_applies(sel) {
            rules.extend(parse_stylesheet_for_viewport(inner, viewport));
        }
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

/// Evaluate the boolean subset of CSS Conditional Rules used by modern
/// framework stylesheets. This follows the same shape as Gecko's
/// SupportsCondition tree: declaration/selector leaves, arbitrary grouping,
/// unary `not`, and top-level `and`/`or` chains. Unknown or malformed future
/// syntax is false instead of optimistically enabling a fallback branch.
pub(crate) fn supports_condition_applies(condition: &str) -> bool {
    let condition = condition.trim();
    let condition = condition
        .strip_prefix("@supports")
        .or_else(|| condition.strip_prefix("supports"))
        .unwrap_or(condition)
        .trim();
    eval_supports_condition(condition)
}

fn eval_supports_condition(condition: &str) -> bool {
    let condition = condition.trim();
    if condition.is_empty() {
        return false;
    }
    if let Some(inner) = enclosing_parenthesized(condition) {
        return eval_supports_condition(inner);
    }
    if condition
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("not"))
        && condition
            .as_bytes()
            .get(3)
            .is_some_and(u8::is_ascii_whitespace)
    {
        return !eval_supports_condition(condition[3..].trim());
    }
    if let Some(parts) = split_supports_operator(condition, "or") {
        return parts.into_iter().any(eval_supports_condition);
    }
    if let Some(parts) = split_supports_operator(condition, "and") {
        return parts.into_iter().all(eval_supports_condition);
    }
    let lower = condition.to_ascii_lowercase();
    if lower.starts_with("selector(") && condition.ends_with(')') {
        let inner = &condition["selector(".len()..condition.len() - 1];
        return obscura_dom::selector::parse_selector(inner.trim()).is_ok();
    }
    let Some((name, value)) = condition.split_once(':') else {
        return false;
    };
    crate::style::supports_declaration(name, value)
}

/// Return the contents when one outer parenthesis pair encloses the complete
/// expression. A declaration leaf such as `(display:grid)` intentionally
/// becomes `display:grid` and is handled after boolean operators.
fn enclosing_parenthesized(condition: &str) -> Option<&str> {
    if !condition.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote = None;
    for (index, character) in condition.char_indices() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                if depth == 0 {
                    return (index + character.len_utf8() == condition.len())
                        .then_some(&condition[1..index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_supports_operator<'a>(condition: &'a str, operator: &str) -> Option<Vec<&'a str>> {
    let bytes = condition.as_bytes();
    let operator = operator.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut depth = 0i32;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            } else if byte == b'\\' {
                index += 1;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ if depth == 0
                && index + operator.len() <= bytes.len()
                && bytes[index..index + operator.len()].eq_ignore_ascii_case(operator)
                && index > 0
                && bytes[index - 1].is_ascii_whitespace()
                && bytes
                    .get(index + operator.len())
                    .is_some_and(u8::is_ascii_whitespace) =>
            {
                let part = condition[start..index].trim();
                if part.is_empty() {
                    return None;
                }
                parts.push(part);
                index += operator.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    if depth != 0 || quote.is_some() || parts.is_empty() {
        return None;
    }
    let tail = condition[start..].trim();
    if tail.is_empty() {
        return None;
    }
    parts.push(tail);
    Some(parts)
}

/// Coarse `@media` evaluation against the live layout viewport.
///
/// Real stylesheets format media features inconsistently
/// (`max-width:750px`, `max-width: 750px`, even `max-width : 750px`), so this
/// strips whitespace before scanning: CSS gives no semantic meaning to spaces
/// inside `(feature: value)`, so it's safe to discard them wholesale rather
/// than special-case every formatting variant a site might use.
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
    let query = query.trim().strip_prefix("@media").unwrap_or(query).trim();
    let compact: String = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();

    // Tailwind expresses every `max-*` breakpoint as the negation of a
    // min-width query (`not all and (min-width:40rem)`). Evaluate the inner
    // condition and invert it; treating all `not all` queries as permanently
    // false makes mutually exclusive responsive utilities apply together at
    // desktop widths. Unknown browser-targeting features still conservatively
    // evaluate true inside and therefore false after negation.
    if let Some(inner) = compact.strip_prefix("notalland") {
        return !single_media_query_applies_for_viewport(inner, viewport);
    }
    if compact == "notall" {
        return false;
    }
    if compact.contains("print") {
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
                    } else if at.starts_with("supports") {
                        if supports_condition_applies(pre) {
                            denest(sel, &inner, rules, viewport);
                        }
                    } else if at.starts_with("layer") {
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
    fn generated_content_resolves_host_attributes_and_resets() {
        let tree = obscura_dom::parse_html(
            r#"<button id="cta" data-label="Get Started"></button>"#,
        );
        let cta = tree.query_selector("#cta").unwrap().unwrap();
        assert_eq!(
            extract_content("content:attr(data-label)", &tree, cta),
            Some(Some("Get Started".to_string()))
        );
        assert_eq!(
            extract_content(r#"content:"fallback";content:none"#, &tree, cta),
            Some(None)
        );
    }

    #[test]
    fn keyframes_extract_only_the_final_animation_state() {
        let css = r#"
            @keyframes dismiss {
                from { opacity: 1; visibility: visible; }
                50% { opacity: .5; }
                to { opacity: 0; visibility: hidden; }
            }
            @-webkit-keyframes slide {
                0% { transform: translateX(0); }
                100% { transform: translateX(20px); }
            }
        "#;
        let ends: HashMap<_, _> = extract_keyframe_end_states(css).into_iter().collect();
        assert!(ends["dismiss"].contains("opacity: 0"));
        assert!(ends["dismiss"].contains("visibility: hidden"));
        assert!(!ends["dismiss"].contains("opacity: 1"));
        assert!(ends["slide"].contains("translateX(20px)"));
    }

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
    fn supports_conditions_gate_legacy_framework_fallbacks() {
        let legacy_probe = "(((-webkit-hyphens:none)) and \
            (not (margin-trim:inline))) or \
            ((-moz-orient:inline) and \
            (not (color:rgb(from red r g b))))";
        assert!(!supports_condition_applies(legacy_probe));
        assert!(supports_condition_applies(
            "(display:grid) and (selector(.card > *))"
        ));
        assert!(supports_condition_applies(
            "not (unknown-engine-prop:value)"
        ));

        let css = format!(
            "@supports {legacy_probe} {{ .legacy {{ line-height:1.5 }} }}\
             @supports (display:grid) {{ .modern {{ display:grid }} }}\
             .host {{ @supports {legacy_probe} {{ width:999px }} }}"
        );
        let rules = parse_stylesheet_for_viewport(&css, (1280.0, 720.0));
        assert!(
            !rules.iter().any(|(selector, declarations)| {
                selector == ".legacy" || declarations.contains("999px")
            }),
            "false supports branches must be skipped: {rules:?}"
        );
        assert!(
            rules.iter().any(|(selector, declarations)| {
                selector == ".modern" && declarations.contains("display:grid")
            }),
            "true supports branch should remain: {rules:?}"
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
    fn negated_min_width_queries_form_max_breakpoints() {
        assert!(media_query_applies_for_viewport(
            "@media not all and (min-width: 40rem)",
            (639.0, 900.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media not all and (min-width: 40rem)",
            (1280.0, 900.0)
        ));

        let css = r#"
            .hidden { display: none }
            @media not all and (min-width: 40rem) {
                .max-sm\:inline { display: inline }
            }
            @media (min-width: 80rem) {
                .xl\:inline { display: inline }
            }
        "#;
        let desktop = parse_stylesheet_for_viewport(css, (1280.0, 900.0));
        assert!(!desktop
            .iter()
            .any(|(selector, _)| selector == r".max-sm\:inline"));
        assert!(desktop
            .iter()
            .any(|(selector, _)| selector == r".xl\:inline"));
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
