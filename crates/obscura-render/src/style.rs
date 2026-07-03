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
    } else if tag == "head" || tag == "script" || tag == "style" || tag == "title" || tag == "meta" || tag == "link" {
        style.display = crate::Display::None;
    } else if tag == "body" {
        style.margin = Edges { top: 8.0, right: 8.0, bottom: 8.0, left: 8.0 };
    } else if tag == "h1" {
        style.font_size = Some(32.0);
        style.font_weight = Some("bold".to_string());
        style.margin = Edges { top: 21.0, bottom: 21.0, left: 0.0, right: 0.0 };
    } else if tag == "h2" {
        style.font_size = Some(24.0);
        style.font_weight = Some("bold".to_string());
        style.margin = Edges { top: 19.0, bottom: 19.0, left: 0.0, right: 0.0 };
    } else if tag == "h3" {
        style.font_size = Some(18.7);
        style.font_weight = Some("bold".to_string());
        style.margin = Edges { top: 18.0, bottom: 18.0, left: 0.0, right: 0.0 };
    } else if tag == "h4" || tag == "h5" || tag == "h6" {
        style.font_size = Some(16.0);
        style.font_weight = Some("bold".to_string());
        style.margin = Edges { top: 21.0, bottom: 21.0, left: 0.0, right: 0.0 };
    } else if tag == "p" || tag == "ul" || tag == "ol" {
        style.margin = Edges { top: 16.0, bottom: 16.0, left: 0.0, right: 0.0 };
    } else if tag == "li" {
        style.margin = Edges { top: 0.0, bottom: 0.0, left: 40.0, right: 0.0 };
    } else if tag == "b" || tag == "strong" || tag == "th" {
        style.font_weight = Some("bold".to_string());
    } else if tag == "a" {
        style.color = Some([0, 0, 238, 255]); // blue
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
    for raw in split_declarations(css) {
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

/// Split a declaration list on top-level semicolons, respecting `url(...)`
/// and quoted strings. A data: URI (`url(data:image/svg+xml;utf8,...)`, an
/// extremely common way to inline small icon SVGs) or a quoted string
/// (`content: "a; b"`) routinely contains a literal semicolon that is not a
/// declaration separator; splitting on every `;` blindly corrupts the
/// declaration into two malformed halves and silently drops it.
fn split_declarations(css: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in css.char_indices() {
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => in_quote = Some(c),
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth == 0 => {
                parts.push(&css[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&css[start..]);
    parts
}

fn apply_value(style: &mut LayoutStyle, name: &str, value: &str) {
    match name {
        "display" => match value {
            "none" => style.display = crate::Display::None,
            "flex" => style.display = crate::Display::Flex,
            "inline-flex" => style.display = crate::Display::Flex,
            "inline" => style.display = crate::Display::Inline,
            "inline-block" => style.display = crate::Display::Inline,
            "grid" => style.display = crate::Display::Grid,
            "inline-grid" => style.display = crate::Display::Grid,
            "block" => style.display = crate::Display::Block,
            _ => {}
        },
        "width" => style.width = dimension_value(value),
        "height" => style.height = dimension_value(value),
        "min-width" => style.min_width = dimension_value(value),
        "min-height" => style.min_height = dimension_value(value),
        "max-width" => style.max_width = dimension_value(value),
        "max-height" => style.max_height = dimension_value(value),
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
        "background-color" => style.background_color = parse_color(value),
        "background" => {
            style.background_color = parse_color(value);
            style.background_image = parse_url(value);
        }
        "background-image" => style.background_image = parse_url(value),
        "background-size" => style.background_size = parse_background_size(value),
        "background-position" => style.background_position = parse_background_position(value),
        "mask-image" | "-webkit-mask-image" => style.mask_image = parse_url(value),
        "color" => style.color = parse_color(value),
        "border-color" => style.border_color = parse_color(value),
        "font-size" => style.font_size = px(value),
        "font-weight" => style.font_weight = Some(value.to_string()),
        // Our engine has no real inline formatting context, so text-align is
        // approximated the same way as align-items: it positions a block's
        // (or column-flex's) children along the cross axis. See
        // `to_taffy_style`'s block-to-flex-column promotion, which is what
        // makes this actually take effect on plain block elements.
        "text-align" => match value {
            "right" => style.align_items = Some(taffy::AlignItems::FlexEnd),
            "center" => style.align_items = Some(taffy::AlignItems::Center),
            "left" | "start" | "justify" => style.align_items = Some(taffy::AlignItems::FlexStart),
            _ => {}
        },
        "align-items" => {
            match value {
                "center" => style.align_items = Some(taffy::AlignItems::Center),
                "flex-start" | "start" => style.align_items = Some(taffy::AlignItems::FlexStart),
                "flex-end" | "end" => style.align_items = Some(taffy::AlignItems::FlexEnd),
                "stretch" => style.align_items = Some(taffy::AlignItems::Stretch),
                "baseline" => style.align_items = Some(taffy::AlignItems::Baseline),
                _ => {}
            }
        },
        "justify-content" => {
            match value {
                "center" => style.justify_content = Some(taffy::JustifyContent::Center),
                "flex-start" | "start" | "left" => style.justify_content = Some(taffy::JustifyContent::FlexStart),
                "flex-end" | "end" | "right" => style.justify_content = Some(taffy::JustifyContent::FlexEnd),
                "space-between" => style.justify_content = Some(taffy::JustifyContent::SpaceBetween),
                "space-around" => style.justify_content = Some(taffy::JustifyContent::SpaceAround),
                "space-evenly" => style.justify_content = Some(taffy::JustifyContent::SpaceEvenly),
                _ => {}
            }
        },
        "flex-direction" => {
            match value {
                "row" => style.flex_direction = Some(taffy::FlexDirection::Row),
                "row-reverse" => style.flex_direction = Some(taffy::FlexDirection::RowReverse),
                "column" => style.flex_direction = Some(taffy::FlexDirection::Column),
                "column-reverse" => style.flex_direction = Some(taffy::FlexDirection::ColumnReverse),
                _ => {}
            }
        },
        "flex-wrap" => {
            match value {
                "wrap" => style.flex_wrap = Some(taffy::FlexWrap::Wrap),
                "nowrap" => style.flex_wrap = Some(taffy::FlexWrap::NoWrap),
                "wrap-reverse" => style.flex_wrap = Some(taffy::FlexWrap::WrapReverse),
                _ => {}
            }
        },
        "flex-grow" => { if let Some(v) = token(value).and_then(|t| t.parse::<f32>().ok()) { style.flex_grow = Some(v); } }
        "flex-shrink" => { if let Some(v) = token(value).and_then(|t| t.parse::<f32>().ok()) { style.flex_shrink = Some(v); } }
        "position" => {
            match value {
                "absolute" | "fixed" => style.position = Some(taffy::Position::Absolute),
                "relative" | "sticky" | "static" => style.position = Some(taffy::Position::Relative),
                _ => {}
            }
        },
        "float" => {
            match value {
                "left" => style.float = Some(crate::Float::Left),
                "right" => style.float = Some(crate::Float::Right),
                "none" => style.float = None,
                _ => {}
            }
        },
        "top" => style.inset[0] = px(value),
        "right" => style.inset[1] = px(value),
        "bottom" => style.inset[2] = px(value),
        "left" => style.inset[3] = px(value),
        "overflow" | "overflow-x" | "overflow-y" => {
            style.overflow_hidden = value != "visible";
        }
        "gap" | "grid-gap" => {
            let dims: Vec<f32> = value.split_whitespace().filter_map(px_value).collect();
            if let Some(&r) = dims.first() { style.row_gap = Some(r); style.column_gap = Some(*dims.get(1).unwrap_or(&r)); }
        }
        "row-gap" | "grid-row-gap" => style.row_gap = px(value),
        "column-gap" | "grid-column-gap" => style.column_gap = px(value),
        "grid-template-columns" => style.grid_template_columns = parse_track_list(value),
        "grid-template-rows" => style.grid_template_rows = parse_track_list(value),
        "grid-template-areas" => style.grid_areas = Some(parse_grid_areas(value)),
        "grid-template" => parse_grid_template(style, value),
        "grid-area" => {
            // Named area (single ident) or line form `r/c/r/c`. We only resolve
            // the named-area case here; line forms are handled by grid-row/column.
            let v = value.trim();
            if !v.contains('/') && !v.is_empty() {
                style.grid_area_name = Some(v.to_string());
            }
        }
        "grid-column" => style.grid_column = parse_grid_line(value),
        "grid-row" => style.grid_row = parse_grid_line(value),
        _ => {}
    }
}

/// Parse a CSS grid track list (`min-content 1fr min-content`, `12.25rem
/// minmax(0,1fr)`) into taffy sizing functions. Tokenizes respecting the
/// parentheses in `minmax(...)` / `fit-content(...)`.
pub(crate) fn parse_track_list(value: &str) -> Vec<taffy::TrackSizingFunction> {
    tokenize_tracks(value).iter().map(|t| track(t)).collect()
}

/// Split a track list on whitespace while keeping `func(a, b)` groups intact.
fn tokenize_tracks(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in value.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() { out.push(std::mem::take(&mut cur)); }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

fn track(tok: &str) -> taffy::TrackSizingFunction {
    use taffy::MinMax;
    let t = tok.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("minmax(").and_then(|s| s.strip_suffix(')')) {
        if let Some((a, b)) = inner.split_once(',') {
            return taffy::TrackSizingFunction::Single(MinMax {
                min: min_track(a.trim()),
                max: max_track(b.trim()),
            });
        }
    }
    if let Some(inner) = lower.strip_prefix("fit-content(").and_then(|s| s.strip_suffix(')')) {
        let lp = px_value(inner.trim())
            .map(taffy::style::LengthPercentage::Length)
            .unwrap_or(taffy::style::LengthPercentage::Length(0.0));
        return taffy::TrackSizingFunction::Single(MinMax {
            min: taffy::MinTrackSizingFunction::Auto,
            max: taffy::MaxTrackSizingFunction::FitContent(lp),
        });
    }
    taffy::TrackSizingFunction::Single(MinMax { min: min_track(t), max: max_track(t) })
}

fn min_track(tok: &str) -> taffy::MinTrackSizingFunction {
    use taffy::MinTrackSizingFunction as M;
    match tok.to_ascii_lowercase().as_str() {
        "min-content" => M::MinContent,
        "max-content" => M::MaxContent,
        "auto" => M::Auto,
        other => {
            if other.ends_with("fr") {
                // Flexible tracks have an automatic minimum.
                M::Auto
            } else if let Some(px) = dim_lp(other) {
                M::Fixed(px)
            } else {
                M::Auto
            }
        }
    }
}

fn max_track(tok: &str) -> taffy::MaxTrackSizingFunction {
    use taffy::MaxTrackSizingFunction as M;
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "min-content" => M::MinContent,
        "max-content" => M::MaxContent,
        "auto" => M::Auto,
        other => {
            if let Some(fr) = other.strip_suffix("fr").and_then(|n| n.trim().parse::<f32>().ok()) {
                M::Fraction(fr)
            } else if let Some(px) = dim_lp(other) {
                M::Fixed(px)
            } else {
                M::Auto
            }
        }
    }
}

/// A length or percentage as a taffy `LengthPercentage`.
fn dim_lp(tok: &str) -> Option<taffy::style::LengthPercentage> {
    let t = tok.trim();
    if let Some(p) = t.strip_suffix('%') {
        return p.parse::<f32>().ok().map(|v| taffy::style::LengthPercentage::Percent(v / 100.0));
    }
    px_value(t).map(taffy::style::LengthPercentage::Length)
}

/// Parse `grid-template-areas: 'a a' 'b c'` into a matrix of cell names.
fn parse_grid_areas(value: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_str = false;
    let mut cur = String::new();
    for c in value.chars() {
        match c {
            '\'' | '"' => {
                if in_str {
                    rows.push(cur.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>());
                    cur.clear();
                    in_str = false;
                } else {
                    in_str = true;
                }
            }
            _ if in_str => cur.push(c),
            _ => {}
        }
    }
    rows
}

/// Parse the `grid-template: <rows> / <cols>` shorthand. The rows side may embed
/// area strings; we support the common `tracks / tracks` form and, when area
/// strings are present, extract them too.
fn parse_grid_template(style: &mut LayoutStyle, value: &str) {
    let (rows_part, cols_part) = match value.split_once('/') {
        Some((r, c)) => (r.trim(), Some(c.trim())),
        None => (value.trim(), None),
    };
    if rows_part.contains('\'') || rows_part.contains('"') {
        // Rows side carries area strings interleaved with row track sizes.
        style.grid_areas = Some(parse_grid_areas(rows_part));
    } else if !rows_part.is_empty() {
        style.grid_template_rows = parse_track_list(rows_part);
    }
    if let Some(cols) = cols_part {
        style.grid_template_columns = parse_track_list(cols);
    }
}

/// Parse `grid-column`/`grid-row` values: `2`, `1 / 3`, `span 2`.
fn parse_grid_line(value: &str) -> Option<taffy::Line<taffy::GridPlacement>> {
    let place = |tok: &str| -> taffy::GridPlacement {
        let tok = tok.trim();
        if let Some(n) = tok.strip_prefix("span").map(|s| s.trim()) {
            if let Ok(s) = n.parse::<u16>() { return taffy::style_helpers::span(s); }
        }
        if let Ok(i) = tok.parse::<i16>() { return taffy::style_helpers::line(i); }
        taffy::GridPlacement::Auto
    };
    let (a, b) = match value.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (value, None),
    };
    let start = place(a);
    let end = b.map(place).unwrap_or(taffy::GridPlacement::Auto);
    Some(taffy::Line { start, end })
}

/// Parse a CSS color to RGBA. Handles #rgb, #rgba, #rrggbb, #rrggbbaa hex,
/// rgb()/rgba(), `var(--x, fallback)` (uses the fallback), and a set of named
/// colors. Returns None for anything else (transparent).
pub(crate) fn parse_color(value: &str) -> Option<[u8; 4]> {
    let raw = value.trim();
    // CSS custom property with a fallback: var(--name, <fallback>). We cannot
    // resolve the variable, but the fallback after the comma is a real color.
    if let Some(rest) = raw.strip_prefix("var(") {
        let inner = rest.strip_suffix(')').unwrap_or(rest);
        if let Some((_, fallback)) = inner.split_once(',') {
            return parse_color(fallback.trim());
        }
        return None;
    }
    // rgb()/rgba() functional notation.
    let lower_full = raw.to_ascii_lowercase();
    if let Some(rest) = lower_full.strip_prefix("rgb(").or_else(|| lower_full.strip_prefix("rgba(")) {
        let inner = rest.strip_suffix(')').unwrap_or(rest);
        let parts: Vec<&str> = inner.split([',', '/', ' ']).filter(|p| !p.trim().is_empty()).collect();
        if parts.len() >= 3 {
            let c = |s: &str| -> Option<u8> {
                let s = s.trim();
                if let Some(pct) = s.strip_suffix('%') {
                    pct.parse::<f32>().ok().map(|v| (v * 2.55).round().clamp(0.0, 255.0) as u8)
                } else {
                    s.parse::<f32>().ok().map(|v| v.round().clamp(0.0, 255.0) as u8)
                }
            };
            let r = c(parts[0])?;
            let g = c(parts[1])?;
            let b = c(parts[2])?;
            let a = parts.get(3)
                .and_then(|s| s.trim().parse::<f32>().ok())
                .map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
                .unwrap_or(255);
            return Some([r, g, b, a]);
        }
        return None;
    }
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
        "silver" => Some([192, 192, 192, 255]),
        "lightgray" | "lightgrey" => Some([211, 211, 211, 255]),
        "darkgray" | "darkgrey" => Some([169, 169, 169, 255]),
        "whitesmoke" => Some([245, 245, 245, 255]),
        "gainsboro" => Some([220, 220, 220, 255]),
        "red" => Some([255, 0, 0, 255]),
        "green" => Some([0, 128, 0, 255]),
        "lime" => Some([0, 255, 0, 255]),
        "blue" => Some([0, 0, 255, 255]),
        "navy" => Some([0, 0, 128, 255]),
        "yellow" => Some([255, 255, 0, 255]),
        "orange" => Some([255, 165, 0, 255]),
        "purple" => Some([128, 0, 128, 255]),
        "maroon" => Some([128, 0, 0, 255]),
        "teal" => Some([0, 128, 128, 255]),
        "aqua" | "cyan" => Some([0, 255, 255, 255]),
        "fuchsia" | "magenta" => Some([255, 0, 255, 255]),
        "olive" => Some([128, 128, 0, 255]),
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

/// Parse the first length in a value as CSS pixels. `auto` and non-numeric
/// values return None (treated as "no explicit size" in phase 1). Delegates
/// to `resolve_length` for anything beyond a bare token, since `calc()`,
/// `var()`, and `min()`/`max()` all contain spaces that would otherwise break
/// the single-token fast path.
fn px(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    if trimmed.contains('(') {
        return resolve_length(trimmed);
    }
    token(value).and_then(px_value)
}

/// Resolve a CSS length expression to px, recursively handling the small set
/// of functional forms real stylesheets actually nest in practice:
/// `var(--x, fallback)` (substitute the fallback; we track no custom
/// property values), `calc(...)`, and `min()`/`max()`. These commonly nest
/// inside each other (`calc(max(calc(var(--x,1rem) + 4px),10px))` is a real
/// example from Wikipedia's icon sizing), so each case recurses back into
/// this function rather than assuming a flat expression.
fn resolve_length(value: &str) -> Option<f32> {
    let v = value.trim();
    if let Some(rest) = v.strip_prefix("var(") {
        let end = find_matching_paren(rest)?;
        let inner = &rest[..end];
        let (_, fallback) = inner.split_once(',')?;
        return resolve_length(fallback.trim());
    }
    if let Some(rest) = v.strip_prefix("calc(") {
        let end = find_matching_paren(rest)?;
        return eval_calc(&rest[..end]);
    }
    if let Some(rest) = v.strip_prefix("max(").or_else(|| v.strip_prefix("min(")) {
        let is_max = v.starts_with("max(");
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        let mut values = args.iter().filter_map(|a| resolve_length(a));
        let mut best = values.next()?;
        for val in values {
            if (is_max && val > best) || (!is_max && val < best) {
                best = val;
            }
        }
        return Some(best);
    }
    if v.contains('(') {
        return None; // an unhandled function (clamp(), env(), ...): no safe fallback
    }
    px_value(v).or_else(|| v.parse::<f32>().ok())
}

/// Find the index (relative to `s`) of the `)` matching an already-consumed
/// opening `(`, accounting for nesting.
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split on `sep` at paren-depth 0 only, so `max(a,b)` inside an argument
/// list is not itself split on its internal comma.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Evaluate a `calc()` body (already stripped of `calc(` / the matching `)`):
/// a left-to-right sum of terms, each itself a left-to-right `*`/`/` chain.
/// Terms may themselves be nested `max()`/`min()`/`var()`/`calc()` calls.
fn eval_calc(expr: &str) -> Option<f32> {
    let mut terms: Vec<(f32, String)> = Vec::new();
    let mut sign = 1.0;
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in expr.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            '+' | '-' if depth == 0 => {
                if !cur.trim().is_empty() {
                    terms.push((sign, std::mem::take(&mut cur)));
                }
                sign = if c == '-' { -1.0 } else { 1.0 };
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        terms.push((sign, cur));
    }
    if terms.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for (term_sign, term) in terms {
        total += term_sign * eval_product(term.trim())?;
    }
    Some(total)
}

/// Evaluate a `*`/`/` chain within one additive term of a calc() expression,
/// e.g. `-1 * 22px / 2`, where a factor may itself be a nested function call.
fn eval_product(term: &str) -> Option<f32> {
    let mut result: Option<f32> = None;
    let mut op = '*';
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut factors = Vec::new();
    for c in term.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            ' ' if depth == 0 => {
                if !cur.is_empty() { factors.push(std::mem::take(&mut cur)); }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() { factors.push(cur); }

    for tok in &factors {
        if tok == "*" || tok == "/" {
            op = tok.chars().next()?;
            continue;
        }
        let v = resolve_length(tok)?;
        result = Some(match result {
            None => v,
            Some(r) if op == '/' => r / v,
            Some(r) => r * v,
        });
    }
    result
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

/// Extract the first `url(...)` reference from a `background`/`background-image`
/// value, unquoted. Ignores any other layers in the same shorthand (gradients,
/// `no-repeat`, etc.): we paint the referenced image, not the gradient.
fn parse_url(value: &str) -> Option<String> {
    let start = value.find("url(")? + 4;
    let end = value[start..].find(')')?;
    let inner = value[start..start + end].trim();
    let unquoted = inner.trim_matches(|c| c == '"' || c == '\'');
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// `background-size: 10px` / `0.857em` / `10px 20px` -> explicit px pair.
/// Keyword values (`cover`, `contain`, `auto`) are left unhandled (`None`, the
/// "stretch to fill the box" fallback) since evaluating them needs the
/// image's own intrinsic aspect ratio, which is not known until it is
/// fetched, well after style resolution.
fn parse_background_size(value: &str) -> Option<(f32, f32)> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    match tokens.as_slice() {
        [one] => px_value(one).map(|v| (v, v)),
        [w, h] => Some((px_value(w)?, px_value(h)?)),
        _ => None,
    }
}

/// `background-position` keywords/lengths -> a 0.0-1.0 fraction per axis (the
/// fraction of the box's leftover space, after the image's own size, to
/// offset by: 0 = start edge, 1 = end edge, 0.5 = centered).
///
/// `left`/`right` always set the x axis and `top`/`bottom` always set y,
/// regardless of order (CSS allows `center right` and `right center`
/// interchangeably for keyword-only values). Any axis left unset by an
/// explicit keyword defaults to centered, which correctly handles a bare
/// `center` (both axes), a single directional keyword (`right` -> the other
/// axis centers), and explicit percentages (first fills x, second fills y,
/// per CSS shorthand order).
fn parse_background_position(value: &str) -> (f32, f32) {
    let mut x = None;
    let mut y = None;
    for tok in value.split_whitespace() {
        match tok {
            "left" => x = Some(0.0),
            "right" => x = Some(1.0),
            "top" => y = Some(0.0),
            "bottom" => y = Some(1.0),
            t if t.ends_with('%') => {
                if let Ok(pct) = t.trim_end_matches('%').parse::<f32>() {
                    let v = pct / 100.0;
                    if x.is_none() { x = Some(v); } else { y = Some(v); }
                }
            }
            _ => {} // "center" (or an unhandled bare length): leaves this axis to default below
        }
    }
    (x.unwrap_or(0.5), y.unwrap_or(0.5))
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

    #[test]
    fn calc_with_multiply_and_divide() {
        // The exact shape MediaWiki uses to offset a TOC toggle button into
        // the left margin: a negative product divided by a constant.
        assert_eq!(resolve_length("calc(-1 * 22px / 2)"), Some(-11.0));
    }

    #[test]
    fn calc_add_and_subtract() {
        assert_eq!(resolve_length("calc(750px - 1px)"), Some(749.0));
        assert_eq!(resolve_length("calc(10px + 5px)"), Some(15.0));
    }

    #[test]
    fn var_with_fallback_resolves_to_fallback() {
        assert_eq!(resolve_length("var(--font-size-medium, 1rem)"), Some(16.0));
    }

    #[test]
    fn var_without_fallback_is_unresolvable() {
        assert_eq!(resolve_length("var(--unknown-token)"), None);
    }

    #[test]
    fn min_and_max_functions() {
        assert_eq!(resolve_length("max(5px, 10px)"), Some(10.0));
        assert_eq!(resolve_length("min(5px, 10px)"), Some(5.0));
    }

    #[test]
    fn nested_var_calc_max_like_wikipedia_icon_sizing() {
        // calc(max(calc(var(--font-size-medium,1rem) + 4px),10px))
        let expr = "calc(max(calc(var(--font-size-medium,1rem) + 4px),10px))";
        assert_eq!(resolve_length(expr), Some(20.0));
    }

    #[test]
    fn width_property_resolves_calc_with_var() {
        let s = compute_style("div", Some("width: calc(var(--x, 10px) + 5px)"));
        assert_eq!(s.width, crate::Dimension::Px(15.0));
    }
}
