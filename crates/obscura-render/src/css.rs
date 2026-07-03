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
    decls: String,
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
}

impl Stylesheet {
    /// Parse and index a set of raw CSS sources (the text of each `<style>`
    /// block, in document order). Selectors that fail to parse are dropped.
    pub fn parse(tree: &DomTree, sources: &[String]) -> Self {
        let mut sheet = Stylesheet {
            rules: Vec::new(),
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_local: HashMap::new(),
            universal: Vec::new(),
        };
        let mut order = 0usize;
        for src in sources {
            for (selector, decls) in parse_stylesheet(src) {
                let Some(sel) = tree.compile_rule_selector(&selector) else { continue };
                let idx = sheet.rules.len();
                match sel.key() {
                    SelectorKey::Id(v) => sheet.by_id.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Class(v) => sheet.by_class.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Local(v) => sheet.by_local.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Universal => sheet.universal.push(idx),
                }
                sheet.rules.push(Rule { sel, decls, order });
                order += 1;
            }
        }
        sheet
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
    pub fn apply(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        id: Option<&str>,
        classes: &[String],
        local: &str,
        style: &mut LayoutStyle,
    ) {
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

        if matched.is_empty() {
            return;
        }
        matched.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for (_, _, i) in matched {
            crate::style::apply_inline(style, &self.rules[i].decls);
        }
    }
}

/// Split a stylesheet into `(selector, declarations)` rules. Handles nested
/// braces, `/* comments */`, and the at-rules that carry ordinary rules inside
/// (`@media`, `@supports`); other at-rules (`@font-face`, `@keyframes`, ...) are
/// skipped since they do not contribute layout-relevant declarations here.
pub fn parse_stylesheet(css: &str) -> Vec<(String, String)> {
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
        } else if c == '}' {
            block_depth -= 1;
            if block_depth == 0 {
                let sel = current_selector.trim();
                let decls = current_decls.trim();
                if let Some(at) = sel.strip_prefix('@') {
                    flush_at_rule(at, sel, decls, &mut rules);
                } else {
                    for s in sel.split(',') {
                        let s = s.trim();
                        if !s.is_empty() {
                            rules.push((s.to_string(), decls.to_string()));
                        }
                    }
                }
                current_selector.clear();
                current_decls.clear();
            } else {
                current_decls.push(c);
            }
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
fn flush_at_rule(at: &str, sel: &str, inner: &str, rules: &mut Vec<(String, String)>) {
    if at.starts_with("media") {
        if media_query_applies(sel) {
            rules.extend(parse_stylesheet(inner));
        }
    } else if at.starts_with("supports") {
        rules.extend(parse_stylesheet(inner));
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
fn media_query_applies(query: &str) -> bool {
    const VIEWPORT_W: f32 = 1280.0;
    if query.contains("print") {
        return false;
    }
    let compact: String = query.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(px) = extract_px(&compact, "max-width:") {
        if VIEWPORT_W > px {
            return false;
        }
    }
    if let Some(px) = extract_px(&compact, "min-width:") {
        if VIEWPORT_W < px {
            return false;
        }
    }
    true
}

/// Read the number immediately following `prop` in `s`. Callers pass a
/// whitespace-stripped `s`, so the first non-digit character always ends the
/// number. Handles a bare px value (`750px`) and the extremely common
/// responsive-breakpoint idiom `calc(750px - 1px)` (real stylesheets use this
/// constantly to express "one pixel narrower than the next breakpoint"):
/// evaluated as a simple left-to-right sum of every px term, since css media
/// feature calc() expressions in practice are always plain +/- of px values,
/// never nested, multiplied, or mixed-unit.
fn extract_px(s: &str, prop: &str) -> Option<f32> {
    let start = s.find(prop)? + prop.len();
    let rest = &s[start..];
    if let Some(inner) = rest.strip_prefix("calc(") {
        let end = inner.find(')')?;
        return Some(eval_px_sum(&inner[..end]));
    }
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    num.parse::<f32>().ok()
}

/// Sum a sequence of signed px terms like `1120px-1px` (whitespace already
/// stripped by the caller) left to right.
fn eval_px_sum(expr: &str) -> f32 {
    let mut total = 0.0;
    let mut sign = 1.0;
    let mut num = String::new();
    let flush = |num: &mut String, sign: f32, total: &mut f32| {
        if let Ok(v) = num.parse::<f32>() {
            *total += sign * v;
        }
        num.clear();
    };
    for c in expr.chars() {
        match c {
            '+' => { flush(&mut num, sign, &mut total); sign = 1.0; }
            '-' => { flush(&mut num, sign, &mut total); sign = -1.0; }
            c if c.is_ascii_digit() || c == '.' => num.push(c),
            _ => {} // unit suffix (px) or anything else: not part of the number
        }
    }
    flush(&mut num, sign, &mut total);
    total
}
