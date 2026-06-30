//! Computed-style lite: parse inline CSS declarations plus a small UA default
//! sheet into the layout-relevant subset of [`crate::LayoutStyle`].
//!
//! This is deliberately not a full CSS cascade. It handles the properties that
//! influence phase-1 box layout (display, width/height, margin, padding, border
//! width) from inline `style="..."` attributes, layered on a tiny built-in UA
//! sheet. A real cascade with selector matching and `cssparser` can harden this
//! later; for inline-style attributes a compact tokenizer is enough and keeps
//! the crate dependency-light.

use crate::{Display, Edges, LayoutStyle};

/// Compute the layout-relevant style for an element: UA defaults for its tag,
/// overridden by its inline `style="..."` declarations.
pub fn compute_style(tag: &str, inline_css: Option<&str>) -> LayoutStyle {
    let mut style = ua_style(tag);
    if let Some(css) = inline_css {
        apply_inline(&mut style, css);
    }
    style
}

/// Built-in UA defaults. Inline elements currently map to block layout; real
/// inline/text layout arrives with the text/paint phase.
fn ua_style(tag: &str) -> LayoutStyle {
    let _ = tag;
    LayoutStyle { display: Display::Block, ..Default::default() }
}

fn apply_inline(style: &mut LayoutStyle, css: &str) {
    for raw in css.split(';') {
        let decl = raw.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((name, value)) = decl.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        // Drop `!important`: it does not change the computed value here.
        let value: String = value
            .trim()
            .split_whitespace()
            .take_while(|t| !t.eq_ignore_ascii_case("!important") && *t != "!")
            .collect::<Vec<_>>()
            .join(" ");
        apply_value(style, &name, &value);
    }
}

fn apply_value(style: &mut LayoutStyle, name: &str, value: &str) {
    match name {
        "display" => match value {
            "block" => style.display = Display::Block,
            "flex" => style.display = Display::Flex,
            "none" => style.display = Display::None,
            _ => {}
        },
        "width" => style.width = px(value),
        "height" => style.height = px(value),
        "margin" => { if let Some(e) = edges(value) { style.margin = e; } }
        "margin-top" => set_edge(&mut style.margin, Side::Top, px(value)),
        "margin-right" => set_edge(&mut style.margin, Side::Right, px(value)),
        "margin-bottom" => set_edge(&mut style.margin, Side::Bottom, px(value)),
        "margin-left" => set_edge(&mut style.margin, Side::Left, px(value)),
        "padding" => { if let Some(e) = edges(value) { style.padding = e; } }
        "padding-top" => set_edge(&mut style.padding, Side::Top, px(value)),
        "padding-right" => set_edge(&mut style.padding, Side::Right, px(value)),
        "padding-bottom" => set_edge(&mut style.padding, Side::Bottom, px(value)),
        "padding-left" => set_edge(&mut style.padding, Side::Left, px(value)),
        "border-width" | "border" => { if let Some(e) = edges(value) { style.border = e; } }
        "border-top-width" | "border-top" => set_edge(&mut style.border, Side::Top, px(value)),
        "border-right-width" | "border-right" => set_edge(&mut style.border, Side::Right, px(value)),
        "border-bottom-width" | "border-bottom" => set_edge(&mut style.border, Side::Bottom, px(value)),
        "border-left-width" | "border-left" => set_edge(&mut style.border, Side::Left, px(value)),
        _ => {}
    }
}

enum Side { Top, Right, Bottom, Left }

fn set_edge(edges: &mut Edges, side: Side, v: Option<f32>) {
    let v = match v { Some(v) => v, None => return };
    match side {
        Side::Top => edges.top = v,
        Side::Right => edges.right = v,
        Side::Bottom => edges.bottom = v,
        Side::Left => edges.left = v,
    }
}

/// Parse the first length in a value as CSS pixels. `auto`, percentages, and
/// non-numeric values return None (treated as "no explicit size" in phase 1).
fn px(value: &str) -> Option<f32> {
    token(value).and_then(px_value)
}

/// Parse every length token in a value as px (for box shorthands).
fn edges(value: &str) -> Option<Edges> {
    let dims: Vec<f32> = value.split_whitespace().filter_map(px_value).collect();
    edges_from(dims)
}

fn px_value(tok: &str) -> Option<f32> {
    let n = tok.strip_suffix("px").unwrap_or(tok);
    // Reject percentages and other units for phase 1.
    if n.chars().any(|c| !(c.is_ascii_digit() || c == '.' || c == '-')) {
        return None;
    }
    n.parse::<f32>().ok()
}

fn token(value: &str) -> Option<&str> {
    value.split_whitespace().next()
}

/// CSS box shorthand: 1 value applies to all sides; 2 values are (v, h);
/// 3 values are (top, h, bottom); 4 values are (top, right, bottom, left).
fn edges_from(dims: Vec<f32>) -> Option<Edges> {
    match dims.len() {
        0 => None,
        1 => Some(Edges { top: dims[0], right: dims[0], bottom: dims[0], left: dims[0] }),
        2 => Some(Edges { top: dims[0], right: dims[1], bottom: dims[0], left: dims[1] }),
        3 => Some(Edges { top: dims[0], right: dims[1], bottom: dims[2], left: dims[1] }),
        _ => Some(Edges { top: dims[0], right: dims[1], bottom: dims[2], left: dims[3] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_display_and_size() {
        let s = compute_style("div", Some("display: flex; width: 200px; height: 50px"));
        assert_eq!(s.display, Display::Flex);
        assert_eq!(s.width, Some(200.0));
        assert_eq!(s.height, Some(50.0));
    }

    #[test]
    fn margin_shorthand_expands() {
        let s = compute_style("div", Some("margin: 10px 20px"));
        assert_eq!(s.margin, Edges { top: 10.0, right: 20.0, bottom: 10.0, left: 20.0 });
    }

    #[test]
    fn longhand_overrides_shorthand() {
        let s = compute_style("div", Some("padding: 5px; padding-left: 30px"));
        assert_eq!(s.padding.top, 5.0);
        assert_eq!(s.padding.left, 30.0);
    }

    #[test]
    fn ua_defaults_and_ignore_unknown() {
        let s = compute_style("span", Some("color: red; ; bogus: ; display: none"));
        assert_eq!(s.display, Display::None);
    }

    #[test]
    fn important_and_auto() {
        let s = compute_style("div", Some("width: 100px !important; height: auto"));
        assert_eq!(s.width, Some(100.0));
        assert_eq!(s.height, None);
    }
}
