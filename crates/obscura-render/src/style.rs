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
pub fn ua_style(tag: &str) -> LayoutStyle {
    let mut style = LayoutStyle::default();
    if tag == "b" || tag == "strong" {
        style.font_weight = Some("bold".into());
    }

    style.display = match tag {
        "span" | "a" | "b" | "i" | "strong" | "em" | "font" => Display::Inline,
        "tr" => Display::Flex,
        _ => Display::Block,
    };
    if tag == "center" {
        style.display = Display::Flex;
        style.flex_direction = Some(taffy::FlexDirection::Column);
        style.align_items = Some(taffy::AlignItems::Center);
    } else if tag == "body" {
        style.margin = Edges { top: 8.0, right: 8.0, bottom: 8.0, left: 8.0 };
    } else if tag == "table" || tag == "tbody" {
        style.display = Display::Flex;
        style.flex_direction = Some(taffy::FlexDirection::Column);
        style.align_items = Some(taffy::AlignItems::Stretch); // stretch rows to fill table width
    } else if tag == "td" || tag == "th" {
        style.display = Display::Flex;
        style.flex_direction = Some(taffy::FlexDirection::Column);
        style.align_items = Some(taffy::AlignItems::FlexStart);
        style.padding = Edges { top: 0.0, right: 5.0, bottom: 0.0, left: 0.0 };
    }
    style
}

pub fn apply_inline(style: &mut LayoutStyle, css: &str) {
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
            "inline" => style.display = Display::Inline,
            "none" => style.display = Display::None,
            _ => {}
        },
        "width" => style.width = dimension_value(value),
        "height" => style.height = dimension_value(value),
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
        "border" => {
            for p in value.split_whitespace() {
                if let Some(c) = parse_color(p) {
                    style.border_color = Some(c);
                } else if p.ends_with("px") || p.chars().all(|c| c.is_ascii_digit()) {
                    if let Some(e) = edges(p) { style.border = e; }
                }
            }
        }
        "border-width" => { if let Some(e) = edges(value) { style.border = e; } }
        "border-top-width" | "border-top" => set_edge(&mut style.border, Side::Top, px(value)),
        "border-right-width" | "border-right" => set_edge(&mut style.border, Side::Right, px(value)),
        "border-bottom-width" | "border-bottom" => set_edge(&mut style.border, Side::Bottom, px(value)),
        "border-left-width" | "border-left" => set_edge(&mut style.border, Side::Left, px(value)),
        "background-color" | "background" => style.background_color = parse_color(value),
        "color" => style.color = parse_color(value),
        "border-color" => style.border_color = parse_color(value),
        "font-size" => style.font_size = px(value),
        "font-weight" => style.font_weight = Some(value.to_string()),
        "text-align" => {
            if value == "right" {
                style.justify_content = Some(taffy::JustifyContent::FlexEnd);
                style.align_items = Some(taffy::AlignItems::FlexEnd);
            } else if value == "center" {
                style.justify_content = Some(taffy::JustifyContent::Center);
                style.align_items = Some(taffy::AlignItems::Center);
            }
        },
        "align-items" => {
            if value == "center" {
                style.align_items = Some(taffy::AlignItems::Center);
            } else if value == "flex-start" || value == "start" {
                style.align_items = Some(taffy::AlignItems::FlexStart);
            } else if value == "flex-end" || value == "end" {
                style.align_items = Some(taffy::AlignItems::FlexEnd);
            }
        },
        _ => {}
    }
}

/// Parse a CSS color to RGBA. Handles #rgb, #rgba, #rrggbb, #rrggbbaa hex and a
/// small set of named colors. Returns None for anything else (transparent).
fn parse_color(value: &str) -> Option<[u8; 4]> {
    let v = value.split_whitespace().next()?.to_ascii_lowercase();
    if let Some(h) = v.strip_prefix('#') {
        let (r, g, b, a) = match h.len() {
            3 => (
                u8::from_str_radix(&h[0..1].repeat(2), 16).ok()?,
                u8::from_str_radix(&h[1..2].repeat(2), 16).ok()?,
                u8::from_str_radix(&h[2..3].repeat(2), 16).ok()?,
                255u8,
            ),
            4 => (
                u8::from_str_radix(&h[0..1].repeat(2), 16).ok()?,
                u8::from_str_radix(&h[1..2].repeat(2), 16).ok()?,
                u8::from_str_radix(&h[2..3].repeat(2), 16).ok()?,
                u8::from_str_radix(&h[3..4].repeat(2), 16).ok()?,
            ),
            6 => (
                u8::from_str_radix(&h[0..2], 16).ok()?,
                u8::from_str_radix(&h[2..4], 16).ok()?,
                u8::from_str_radix(&h[4..6], 16).ok()?,
                255u8,
            ),
            8 => (
                u8::from_str_radix(&h[0..2], 16).ok()?,
                u8::from_str_radix(&h[2..4], 16).ok()?,
                u8::from_str_radix(&h[4..6], 16).ok()?,
                u8::from_str_radix(&h[6..8], 16).ok()?,
            ),
            _ => return None,
        };
        return Some([r, g, b, a]);
    }
    match v.as_str() {
        "white" => Some([255, 255, 255, 255]),
        "black" => Some([0, 0, 0, 255]),
        "gray" | "grey" => Some([128, 128, 128, 255]),
        "red" => Some([255, 0, 0, 255]),
        "green" => Some([0, 128, 0, 255]),
        "blue" => Some([0, 0, 255, 255]),
        "orange" => Some([255, 165, 0, 255]),
        "transparent" => Some([0, 0, 0, 0]),
        _ => None,
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

fn dimension_value(tok: &str) -> crate::Dimension {
    let n = tok.trim();
    if n.ends_with("%") {
        if let Ok(v) = n[..n.len()-1].parse::<f32>() {
            return crate::Dimension::Percent(v / 100.0);
        }
    } else if let Some(px) = px(tok) {
        return crate::Dimension::Px(px);
    }
    crate::Dimension::Auto
}

/// Parse every length token in a value as px (for box shorthands).
fn edges(value: &str) -> Option<Edges> {
    let dims: Vec<f32> = value.split_whitespace().filter_map(px_value).collect();
    edges_from(dims)
}

fn px_value(tok: &str) -> Option<f32> {
    let mut n = tok;
    let mut scale = 1.0;
    
    if n.ends_with("px") {
        n = &n[..n.len() - 2];
    } else if n.ends_with("pt") {
        n = &n[..n.len() - 2];
        scale = 1.333; // 1pt ≈ 1.333px
    } else if n.ends_with("em") || n.ends_with("rem") {
        n = n.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        scale = 16.0; // 1em = 16px
    } else if n.ends_with('%') {
        n = &n[..n.len() - 1];
        scale = 16.0 / 100.0;
    } else {
        n = n.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    }
    
    if n.chars().any(|c| !(c.is_ascii_digit() || c == '.' || c == '-')) {
        return None;
    }
    n.parse::<f32>().ok().map(|v| v * scale)
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
        assert_eq!(s.width, crate::Dimension::Px(200.0));
        assert_eq!(s.height, crate::Dimension::Px(50.0));
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
        assert_eq!(s.width, crate::Dimension::Px(100.0));
        assert_eq!(s.height, crate::Dimension::Auto);
    }
}
