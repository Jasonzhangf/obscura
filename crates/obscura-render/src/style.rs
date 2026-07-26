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
        // Phrasing / inline-level content defaults to inline so a paragraph
        // that mixes these with text stays one inline formatting context that
        // cosmic-text can shape and wrap as a whole, instead of each element
        // becoming its own block box (which forces the flex word-promotion
        // fallback and its fragile one-word-per-line wrapping). Author CSS
        // (e.g. `code{display:block}`) still overrides this in the cascade.
        "span" | "a" | "b" | "i" | "strong" | "em" | "font" | "code" | "small"
        | "sub" | "sup" | "mark" | "abbr" | "cite" | "var" | "dfn" | "kbd"
        | "samp" | "q" | "time" | "s" | "u" | "del" | "ins" | "tt" | "big"
        | "bdi" | "bdo" | "wbr" | "data" | "output" | "label" | "ruby" | "rt"
        | "rp" => Display::Inline,
        "tr" => Display::Flex,
        _ => Display::Block,
    };
    if tag == "center" {
        // Browser UA sheets keep <center> block-level and give it a special
        // inherited text alignment which also centers fixed-width block
        // descendants. Keep that provenance separate from ordinary authored
        // text-align:center.
        style.text_align = Some(taffy::AlignItems::CENTER);
        style.legacy_center = true;
    } else if tag == "head" || tag == "script" || tag == "style" || tag == "title" || tag == "meta" || tag == "link" || tag == "noscript" || tag == "template"
        || tag == "desc" || tag == "metadata" || tag == "option" || tag == "optgroup"
        || tag == "source" || tag == "track" || tag == "param" || tag == "area" {
        // `noscript` content is only for scripting-disabled agents; with JS on
        // (as here) the parser keeps it as raw text and the browser hides it,
        // so a site's no-JS nav fallback must not paint as literal markup.
        // `template` content is inert and never rendered. svg `title`/`desc`/
        // `metadata` are AX/tooltip metadata, never rendered in flow (an inline
        // <svg> we cannot rasterize would otherwise leak its `<desc>` text).
        // `option`/`optgroup` render only inside the native select popup, so a
        // closed <select> must not paint every option label stacked.
        // `source`/`track`/`param`/`area` are metadata-only children of
        // picture/video/object/map; a `<picture><source width= height=>` must
        // not lay out as a real box (news CDNs put dimensions on `<source>`,
        // which otherwise paints an empty box the size of the image).
        style.display = crate::Display::None;
    } else if tag == "br" {
        // Keep forced breaks as full-width sentinels in the general
        // flex/block fallback. The builder replaces the zero height with the
        // inherited used line-height; pure and mixed inline runs fold the tag
        // into the shaped line stream instead.
        style.width = crate::Dimension::Percent(1.0);
        style.height = crate::Dimension::Px(0.0);
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
    } else if matches!(tag, "p" | "ul" | "ol" | "menu" | "dir") {
        style.margin = Edges { top: 16.0, bottom: 16.0, left: 0.0, right: 0.0 };
        if matches!(tag, "ul" | "menu" | "dir") {
            style.list_style = Some(crate::ListStyle::Disc);
            style.padding.left = 40.0;
        } else if tag == "ol" {
            style.list_style = Some(crate::ListStyle::Decimal);
            style.padding.left = 40.0;
        }
    } else if tag == "b" || tag == "strong" {
        style.font_weight = Some("bold".to_string());
    } else if tag == "i" || tag == "em" || tag == "cite" || tag == "var" || tag == "dfn" || tag == "address" {
        style.font_style_italic = Some(true);
    } else if tag == "a" {
        style.color = Some([0, 0, 238, 255]); // blue
        style.underline = Some(true); // UA default: links are underlined
    } else if tag == "input" {
        // Native text controls are atomic inline-level boxes with their own
        // platform font and intrinsic border-box dimensions; they do not
        // inherit the page's font shorthand by default. The declared CSS box
        // remains content-box in standards mode and switches in quirks mode.
        // Size-dependent geometry is resolved after cascading, once the input
        // type and `size` attribute are available (dom::layout_dom).
        style.display = Display::Inline;
        style.is_inline_block = true;
        style.font_size = Some(13.333_333);
        style.font_family = Some("arial".to_string());
        style.line_height = Some(crate::LineHeight::Normal);
        style.padding = Edges {
            top: 1.0,
            right: 2.0,
            bottom: 1.0,
            left: 2.0,
        };
        style.border = Edges {
            top: 2.0,
            right: 2.0,
            bottom: 2.0,
            left: 2.0,
        };
        style.border_color = Some([118, 118, 118, 255]);
        style.background_color = Some([255, 255, 255, 255]);
    } else if matches!(tag, "table" | "tbody" | "thead" | "tfoot") {
        style.display = Display::Flex;
        style.internal_flex_container = true;
        style.flex_direction = Some(taffy::FlexDirection::Column);
        style.align_items = Some(taffy::AlignItems::STRETCH); // stretch rows to fill table width
        // Rows fill the table width and may shrink below their content's
        // min-content size (the flexbox automatic-minimum-size gotcha), so a
        // width-constrained taxobox contains its content instead of blowing
        // out sideways. (Fully matching CSS auto table layout, where a table
        // grows to fit unshrinkable content, needs real table layout.)
        style.min_width = crate::Dimension::Px(0.0);
        if tag == "table" {
            // Chromium's HTML UA sheet makes the table grid border-box and
            // supplies the traditional two-pixel separate-border spacing.
            // Author declarations and the legacy `cellspacing` hint cascade
            // over these values.
            style.box_sizing = crate::BoxSizing::BorderBox;
            style.border_spacing = Some((2.0, 2.0));
            style.border_collapse = Some(false);
        } else {
            style.width = crate::Dimension::Percent(1.0);
            // The row-group UA rule is the source of the effective default
            // middle alignment; rows and cells inherit it below.
            style.vertical_align = Some(crate::VerticalAlign::Middle);
        }
    } else if tag == "tr" {
        style.internal_flex_container = true;
        // Rows fill the table width and can shrink below content min-content;
        // this is exactly why Wikipedia's own responsive CSS uses
        // `tr{min-width:100%}`. `align-items:stretch` alone did not pin them
        // once a cell's content (a 250px no-wrap widget) exceeded the box.
        style.min_width = crate::Dimension::Px(0.0);
        style.width = crate::Dimension::Percent(1.0);
    } else if tag == "td" || tag == "th" {
        style.display = Display::Flex;
        style.internal_flex_container = true;
        style.flex_direction = Some(taffy::FlexDirection::Column);
        style.align_items = Some(taffy::AlignItems::FLEX_START);
        style.padding = Edges {
            top: 1.0,
            right: 1.0,
            bottom: 1.0,
            left: 1.0,
        };
        style.min_width = crate::Dimension::Px(0.0);
        if tag == "th" {
            style.font_weight = Some("bold".to_string());
        }
    } else if tag == "img" || tag == "figure" {
        if tag == "img" {
            // Images are inline-level replaced elements by default. Model the
            // atomic inner box with the same inline-block marker used by
            // native controls, so an icon between text fragments participates
            // in their line instead of splitting the parent into block runs.
            // An authored display declaration clears this marker in the
            // cascade before applying its requested display.
            style.display = Display::Inline;
            style.is_inline_block = true;
        }
        // Near-universal reset: images fit their container instead of
        // overflowing. Wikipedia (and most sites) set `img{max-width:100%}`;
        // making it a UA default prevents a fixed-width thumbnail (e.g. a
        // 250px infobox image) from spilling out of a narrower box (a 200px
        // taxobox) when we have not applied the site's own rule.
        style.max_width = crate::Dimension::Percent(1.0);
        style.flex_shrink = Some(1.0);
    }
    style
}

pub fn apply_inline(style: &mut LayoutStyle, css: &str) {
    let (normal, important) = partition_declarations(css);
    apply_declarations(style, &normal);
    apply_declarations(style, &important);
}

/// Split a declaration block into normal and `!important` declarations while
/// preserving source order inside each priority. The returned declarations
/// have the priority marker removed, ready for [`apply_declarations`].
pub(crate) fn partition_declarations(css: &str) -> (String, String) {
    let mut normal = String::new();
    let mut important = String::new();
    for raw in split_declarations(css) {
        let decl = raw.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((name, value)) = decl.split_once(':') else { continue };
        let mut value = value.trim();
        let mut is_important = false;
        if let Some(bang) = value.rfind('!') {
            if value[bang + 1..].trim().eq_ignore_ascii_case("important") {
                value = value[..bang].trim_end();
                is_important = true;
            }
        }
        let out = if is_important { &mut important } else { &mut normal };
        out.push_str(name.trim());
        out.push(':');
        out.push_str(value);
        out.push(';');
    }
    (normal, important)
}

/// Apply declarations in the order provided. Priority ordering must already
/// have been resolved by the caller.
pub(crate) fn apply_declarations(style: &mut LayoutStyle, css: &str) {
    for raw in split_declarations(css) {
        let decl = raw.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((name, value)) = decl.split_once(':') else { continue };
        apply_value(style, &name.trim().to_ascii_lowercase(), value.trim());
    }
}

/// Split a declaration list on top-level semicolons, respecting `url(...)`
/// and quoted strings. A data: URI (`url(data:image/svg+xml;utf8,...)`, an
/// extremely common way to inline small icon SVGs) or a quoted string
/// (`content: "a; b"`) routinely contains a literal semicolon that is not a
/// declaration separator; splitting on every `;` blindly corrupts the
/// declaration into two malformed halves and silently drops it.
pub(crate) fn split_declarations(css: &str) -> Vec<&str> {
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
            // Track `{...}` too: once @layer bodies are admitted, CSS-nested
            // rules (`&:hover{a:b}`, nested @media) appear inside declaration
            // lists. Keeping a nested block as one chunk makes it a single
            // unparseable declaration that is dropped, rather than leaking its
            // inner declarations into the parent rule at the first `;`.
            '(' | '{' => depth += 1,
            ')' | '}' => depth = (depth - 1).max(0),
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
        "display" => {
            if value != "contents" {
                style.display_contents = false;
            }
            if matches!(
                value,
                "none"
                    | "flex"
                    | "inline-flex"
                    | "inline"
                    | "inline-block"
                    | "grid"
                    | "inline-grid"
                    | "block"
                    | "flow-root"
                    | "contents"
            ) {
                style.internal_flex_container = false;
                style.is_inline_block = false;
                style.flow_root = false;
            }
            match value {
                "none" => style.display = crate::Display::None,
                "flex" => style.display = crate::Display::Flex,
                "inline-flex" => style.display = crate::Display::Flex,
                "inline" => style.display = crate::Display::Inline,
                "inline-block" => {
                    style.display = crate::Display::Inline;
                    style.is_inline_block = true;
                }
                "grid" => style.display = crate::Display::Grid,
                "inline-grid" => style.display = crate::Display::Grid,
                "block" => style.display = crate::Display::Block,
                "flow-root" => {
                    style.display = crate::Display::Block;
                    style.flow_root = true;
                }
                "contents" => {
                    // `display:contents` can override an earlier `display:none`
                    // in the cascade (responsive desktop/mobile wrappers do
                    // this constantly). It suppresses only this element's box;
                    // its children remain generated and are flattened into the
                    // parent formatting context.
                    style.display = crate::Display::Block;
                    style.display_contents = true;
                }
                _ => {}
            }
        }
        "width" => {
            style.width = dimension_value(value);
            style.size_expressions[0] = deferred_length_expression(value);
            style.width_set = true;
        }
        "height" => {
            style.height = dimension_value(value);
            style.size_expressions[1] = deferred_length_expression(value);
            style.height_set = true;
        }
        "box-sizing" => {
            style.box_sizing = if value.eq_ignore_ascii_case("border-box") {
                crate::BoxSizing::BorderBox
            } else if value.eq_ignore_ascii_case("content-box") {
                crate::BoxSizing::ContentBox
            } else if value.eq_ignore_ascii_case("inherit") {
                crate::BoxSizing::Inherit
            } else {
                style.box_sizing
            };
        }
        "min-width" => {
            style.min_width = dimension_value(value);
            style.size_expressions[2] = deferred_length_expression(value);
        }
        "min-height" => {
            style.min_height = dimension_value(value);
            style.size_expressions[3] = deferred_length_expression(value);
        }
        "max-width" => {
            style.max_width = dimension_value(value);
            style.size_expressions[4] = deferred_length_expression(value);
        }
        "max-height" => {
            style.max_height = dimension_value(value);
            style.size_expressions[5] = deferred_length_expression(value);
        }
        "aspect-ratio" => style.aspect_ratio = parse_aspect_ratio(value),
        "margin" => apply_margin_shorthand(style, value),
        "margin-top" => set_margin_side(style, 0, value),
        "margin-right" => set_margin_side(style, 1, value),
        "margin-bottom" => set_margin_side(style, 2, value),
        "margin-left" => set_margin_side(style, 3, value),
        // Logical margins (LTR: inline = left/right, block = top/bottom).
        "margin-inline" => { let (s, e) = two(value); set_margin_side(style, 3, s); set_margin_side(style, 1, e); }
        "margin-inline-start" => set_margin_side(style, 3, value),
        "margin-inline-end" => set_margin_side(style, 1, value),
        "margin-block" => { let (s, e) = two(value); set_margin_side(style, 0, s); set_margin_side(style, 2, e); }
        "margin-block-start" => set_margin_side(style, 0, value),
        "margin-block-end" => set_margin_side(style, 2, value),
        "padding" => apply_padding_shorthand(style, value),
        "padding-top" => set_padding_side(style, 0, value),
        "padding-right" => set_padding_side(style, 1, value),
        "padding-bottom" => set_padding_side(style, 2, value),
        "padding-left" => set_padding_side(style, 3, value),
        "padding-inline" => { let (s, e) = two(value); set_padding_side(style, 3, s); set_padding_side(style, 1, e); }
        "padding-inline-start" => set_padding_side(style, 3, value),
        "padding-inline-end" => set_padding_side(style, 1, value),
        "padding-block" => { let (s, e) = two(value); set_padding_side(style, 0, s); set_padding_side(style, 2, e); }
        "padding-block-start" => set_padding_side(style, 0, value),
        "padding-block-end" => set_padding_side(style, 2, value),
        "border-radius" => {
            // Uniform radius from the first value (ignore per-corner / the
            // `/` vertical-radius form; the common case is one length).
            if let Some(r) = value.split(['/', ' ']).next().and_then(|t| px(t)) {
                style.border_radius = r;
            }
        }
        "border" => {
            if value.split_whitespace().any(|token| {
                token.eq_ignore_ascii_case("none") || token.eq_ignore_ascii_case("hidden")
            }) {
                style.border = Edges::default();
                style.border_color = None;
                return;
            }
            for p in value.split_whitespace() {
                if let Some(c) = parse_color(p) {
                    style.border_color = Some(c);
                } else if p.ends_with("px") || p.chars().all(|c| c.is_ascii_digit()) {
                    if let Some(e) = edges(p) { style.border = e; }
                }
            }
        }
        "border-width" => { if let Some(e) = edges(value) { style.border = e; } }
        "border-top-width" | "border-top" => {
            set_edge(&mut style.border, Side::Top, border_side_width(value))
        }
        "border-right-width" | "border-right" => {
            set_edge(&mut style.border, Side::Right, border_side_width(value))
        }
        "border-bottom-width" | "border-bottom" => {
            set_edge(&mut style.border, Side::Bottom, border_side_width(value))
        }
        "border-left-width" | "border-left" => {
            set_edge(&mut style.border, Side::Left, border_side_width(value))
        }
        "background-color" => style.background_color = parse_color(value),
        "background" => {
            // A shorthand resets every omitted background longhand to its
            // initial value before applying the layers it does name.
            // `background:0` is a valid position-only shorthand commonly used
            // to clear a component background; merely assigning the fields we
            // can parse leaves an earlier color/image painting underneath it.
            // An empty value is invalid (for example an unresolved `var()`),
            // so leave the prior cascade winner untouched in that case.
            if !value.trim().is_empty() {
                style.background_color = None;
                style.background_gradient = parse_linear_gradient(value);
                style.background_radial_gradient = parse_radial_gradient(value);
                style.background_conic_gradient = parse_conic_gradient(value);
                if style.background_gradient.is_none()
                    && style.background_radial_gradient.is_none()
                    && style.background_conic_gradient.is_none()
                {
                    style.background_color = parse_color(value);
                }
                style.background_image = parse_url(value);
                style.background_size = None;
                style.background_size_expression = background_size_expression(value);
                style.background_size_fit = parse_background_size_fit(value);
                style.background_position = (0.0, 0.0);
                style.background_clip_text = false;
            }
        }
        "background-image" => {
            style.background_gradient = parse_linear_gradient(value);
            style.background_radial_gradient = parse_radial_gradient(value);
            style.background_conic_gradient = parse_conic_gradient(value);
            style.background_image = parse_url(value);
        }
        "background-size" => {
            style.background_size = parse_background_size(value);
            style.background_size_expression =
                (!value.trim().is_empty()).then(|| value.trim().to_string());
            style.background_size_fit = parse_background_size_fit(value);
        }
        "background-position" => style.background_position = parse_background_position(value),
        "mask-image" | "-webkit-mask-image" => style.mask_image = parse_url(value),
        "mask-size" | "-webkit-mask-size" => {
            style.mask_size = parse_background_size(value)
        }
        "mask-repeat" | "-webkit-mask-repeat" => {
            style.mask_repeat = match value.trim() {
                "repeat" | "space" | "round" => Some((true, true)),
                "repeat-x" => Some((true, false)),
                "repeat-y" => Some((false, true)),
                "no-repeat" => Some((false, false)),
                _ => None,
            }
        }
        "background-clip" | "-webkit-background-clip" => {
            // `text` clips the background to the element's glyphs (the gradient/
            // solid-color text technique). Any box value (border-box/padding-box/
            // content-box) is an ordinary background, so clear the flag.
            style.background_clip_text = value.trim().eq_ignore_ascii_case("text");
        }
        // Blink/WebKit gradient text commonly makes the glyph fill
        // transparent through this inherited property while clipping a
        // background gradient to the text. We model one effective text color,
        // so the vendor fill color is the paint-time color winner.
        "color" | "-webkit-text-fill-color" => style.color = parse_color(value),
        "border-color" => style.border_color = parse_color(value),
        "font-size" => {
            // Absolute lengths resolve now; font/viewport-relative ones defer
            // to the inheritance pass (they need parent/root font-size).
            apply_font_size(style, value);
        }
        "font" => apply_font_shorthand(style, value),
        "font-weight" => {
            if let Some(weight) = specified_font_weight(value) {
                style.font_weight = Some(weight);
            }
        }
        "font-family" => {
            let v = value.trim().to_ascii_lowercase();
            if !v.is_empty() && v != "inherit" {
                style.font_family = Some(v);
            }
        }
        // Text alignment is inherited and applies to inline line boxes. It is
        // deliberately separate from flex/grid `align-items`, which positions
        // child boxes rather than text inside them.
        "text-align" => match value {
            "right" | "end" => {
                style.text_align = Some(taffy::AlignItems::FLEX_END);
                style.legacy_center = false;
            }
            "center" => {
                style.text_align = Some(taffy::AlignItems::CENTER);
                style.legacy_center = false;
            }
            "left" | "start" | "justify" => {
                style.text_align = Some(taffy::AlignItems::FLEX_START);
                style.legacy_center = false;
            }
            _ => {}
        },
        "align-items" => {
            if let Some(Some(value)) = self_alignment_value(value) {
                style.align_items = Some(value);
            }
        },
        "justify-items" => {
            if let Some(Some(value)) = self_alignment_value(value) {
                style.justify_items = Some(value);
            }
        },
        "place-items" => {
            if let Some((Some(align), Some(justify))) = self_alignment_pair(value) {
                style.align_items = Some(align);
                style.justify_items = Some(justify);
            }
        },
        "align-self" => {
            if let Some(value) = self_alignment_value(value) {
                style.align_self = value;
            }
        },
        "justify-self" => {
            if let Some(value) = self_alignment_value(value) {
                style.justify_self = value;
            }
        },
        "place-self" => {
            if let Some((align, justify)) = self_alignment_pair(value) {
                style.align_self = align;
                style.justify_self = justify;
            }
        },
        "align-content" => {
            if let Some(value) = content_alignment_value(value) {
                style.align_content = Some(value);
            }
        },
        "justify-content" => {
            let value = match value.trim().to_ascii_lowercase().as_str() {
                "left" => Some(taffy::JustifyContent::START),
                "right" => Some(taffy::JustifyContent::END),
                _ => content_alignment_value(value),
            };
            if let Some(value) = value {
                style.justify_content = Some(value);
            }
        },
        "place-content" => {
            if let Some((align, justify)) = content_alignment_pair(value) {
                style.align_content = Some(align);
                style.justify_content = Some(justify);
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
        "order" => {
            if let Ok(order) = value.trim().parse::<i32>() {
                style.order = order;
            }
        }
        "flex-basis" => { style.flex_basis = dimension_value(value.trim()); }
        "flex" => parse_flex_shorthand(style, value),
        "position" => {
            match value {
                "absolute" => {
                    style.position = Some(taffy::Position::Absolute);
                    style.position_fixed = false;
                }
                "fixed" => {
                    style.position = Some(taffy::Position::Absolute);
                    style.position_fixed = true;
                }
                "relative" | "sticky" => {
                    style.position = Some(taffy::Position::Relative);
                    style.position_fixed = false;
                }
                "static" => {
                    style.position = None;
                    style.position_fixed = false;
                }
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
        "object-fit" => {
            match value.trim().to_ascii_lowercase().as_str() {
                "fill" => style.object_fit = crate::ObjectFit::Fill,
                "contain" => style.object_fit = crate::ObjectFit::Contain,
                "cover" => style.object_fit = crate::ObjectFit::Cover,
                "scale-down" => style.object_fit = crate::ObjectFit::ScaleDown,
                "none" => style.object_fit = crate::ObjectFit::None,
                _ => {}
            }
        },
        "top" => set_inset_side(style, 0, value),
        "right" => set_inset_side(style, 1, value),
        "bottom" => set_inset_side(style, 2, value),
        "left" => set_inset_side(style, 3, value),
        "inset" => {
            // 1-4 values, CSS shorthand order: all / v h / t h b / t r b l.
            let parts = split_ws_paren(value);
            let (t, r, b, l) = match parts.as_slice() {
                [a] => (*a, *a, *a, *a),
                [v, h] => (*v, *h, *v, *h),
                [t, h, b] => (*t, *h, *b, *h),
                [t, r, b, l, ..] => (*t, *r, *b, *l),
                [] => return,
            };
            set_inset_side(style, 0, t);
            set_inset_side(style, 1, r);
            set_inset_side(style, 2, b);
            set_inset_side(style, 3, l);
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            style.overflow_hidden = value != "visible";
        }
        "visibility" => style.visibility_hidden = Some(value.eq_ignore_ascii_case("hidden")),
        "opacity" => style.opacity = value.trim().parse::<f32>().ok(),
        "animation" => apply_animation_shorthand(style, value),
        "animation-name" => {
            let first = split_top_level(value, ',').into_iter().next().unwrap_or("").trim();
            style.animation_name = if first.is_empty() || first.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(first.to_string())
            };
        }
        "animation-fill-mode" => {
            let first = split_top_level(value, ',').into_iter().next().unwrap_or("").trim();
            style.animation_fill_forwards =
                first.eq_ignore_ascii_case("forwards") || first.eq_ignore_ascii_case("both");
        }
        "animation-iteration-count" => {
            let first = split_top_level(value, ',').into_iter().next().unwrap_or("").trim();
            style.animation_iteration_infinite = first.eq_ignore_ascii_case("infinite");
        }
        "z-index" => {
            style.z_index = match value.trim() {
                "auto" | "inherit" | "initial" => None,
                v => v.parse::<i32>().ok(),
            };
        }
        "clear" => {
            style.clear = match value.trim().to_ascii_lowercase().as_str() {
                "left" | "inline-start" => Some(crate::Clear::Left),
                "right" | "inline-end" => Some(crate::Clear::Right),
                "both" => Some(crate::Clear::Both),
                _ => None,
            };
        }
        "vertical-align" => {
            style.vertical_align = match value.trim().to_ascii_lowercase().as_str() {
                "top" | "baseline" | "text-top" => Some(crate::VerticalAlign::Top),
                "middle" => Some(crate::VerticalAlign::Middle),
                "bottom" | "text-bottom" => Some(crate::VerticalAlign::Bottom),
                // sub/super/lengths are text-level; leave the cell default.
                _ => style.vertical_align,
            };
        }
        "list-style-type" => {
            style.list_style = Some(list_style_keyword(value.trim()).unwrap_or(crate::ListStyle::Disc));
        }
        "list-style" => {
            // Shorthand: type | position | image in any order. We only track
            // the type keyword (and `none`, which suppresses the marker, the
            // common way nav `<ul>`s drop their bullets).
            for tok in value.split_whitespace() {
                if let Some(ls) = list_style_keyword(tok) {
                    style.list_style = Some(ls);
                }
            }
        }
        "line-height" => {
            let v = value.trim();
            if v.contains('(') {
                style.line_height = None;
                style.line_height_expression = Some(v.to_string());
                return;
            }
            style.line_height_expression = None;
            style.line_height = if v.eq_ignore_ascii_case("normal") {
                Some(crate::LineHeight::Normal)
            } else if let Some(pct) = v.strip_suffix('%') {
                pct.trim()
                    .parse::<f32>()
                    .ok()
                    .map(|number| {
                        crate::LineHeight::Relative(crate::Dimension::Percent(
                            number / 100.0,
                        ))
                    })
            } else if v.ends_with("px") || v.ends_with("pt") {
                px_value(v).map(crate::LineHeight::Px)
            } else if ["rem", "em", "ex", "vw", "vh", "vmin", "vmax"]
                .iter()
                .any(|unit| v.ends_with(unit))
            {
                Some(crate::LineHeight::Relative(dimension_value(v)))
            } else {
                // Unitless number: a multiple of font-size (the common case).
                v.parse::<f32>().ok().map(crate::LineHeight::Ratio)
            };
        }
        "font-style" => {
            let v = value.trim().to_ascii_lowercase();
            style.font_style_italic = Some(v.starts_with("italic") || v.starts_with("oblique"));
        }
        "text-transform" => {
            style.text_transform = Some(match value.trim().to_ascii_lowercase().as_str() {
                "uppercase" => crate::TextTransform::Uppercase,
                "lowercase" => crate::TextTransform::Lowercase,
                "capitalize" => crate::TextTransform::Capitalize,
                _ => crate::TextTransform::None,
            });
        }
        "text-decoration" | "text-decoration-line" => {
            // Shorthand can carry color/style/thickness; we only model the
            // underline line (the dominant case, and the UA default for links).
            let toks: Vec<String> = value.split_whitespace().map(|t| t.to_ascii_lowercase()).collect();
            let underline = toks.iter().any(|t| t == "underline");
            let none = toks.iter().any(|t| t == "none");
            style.underline = Some(underline && !none);
        }
        "gap" | "grid-gap" => {
            let values = split_ws_paren(value);
            if let Some(row) = values.first() {
                apply_gap_value(style, true, row);
                apply_gap_value(style, false, values.get(1).copied().unwrap_or(row));
            }
        }
        "row-gap" | "grid-row-gap" => apply_gap_value(style, true, value),
        "column-gap" | "grid-column-gap" => apply_gap_value(style, false, value),
        "border-spacing" => {
            let dims: Vec<f32> = value.split_whitespace().filter_map(px_value).collect();
            if let Some(&h) = dims.first() {
                style.border_spacing = Some((h, *dims.get(1).unwrap_or(&h)));
            }
        }
        "border-collapse" => {
            style.border_collapse = match value.trim().to_ascii_lowercase().as_str() {
                "collapse" => Some(true),
                "separate" | "initial" | "revert" | "revert-layer" => Some(false),
                // This is an inherited property, so both an omitted value and
                // an explicit inherit/unset are resolved in the top-down pass.
                "inherit" | "unset" => None,
                _ => style.border_collapse,
            };
        }
        "grid-template-columns" => {
            let (tracks, names) = parse_track_list_named(value);
            style.grid_template_columns = tracks;
            style.grid_col_line_names = (!names.is_empty()).then(|| build_line_map(names));
        }
        "grid-template-rows" => {
            let (tracks, names) = parse_track_list_named(value);
            style.grid_template_rows = tracks;
            style.grid_row_line_names = (!names.is_empty()).then(|| build_line_map(names));
        }
        "grid-template-areas" => style.grid_areas = Some(parse_grid_areas(value)),
        "grid-template" => parse_grid_template(style, value),
        "grid" => parse_grid_shorthand(style, value),
        "grid-auto-flow" => style.grid_auto_flow = parse_grid_auto_flow(value),
        "grid-area" => {
            // Named area (single ident) or line form `r/c/r/c`. We only resolve
            // the named-area case here; line forms are handled by grid-row/column.
            let v = value.trim();
            if !v.contains('/') && !v.is_empty() {
                style.grid_area_name = Some(v.to_string());
            }
        }
        "grid-column" => set_grid_placement(style, value, true),
        "grid-row" => set_grid_placement(style, value, false),
        "grid-column-start" => set_grid_placement_side(style, value, true, true),
        "grid-column-end" => set_grid_placement_side(style, value, true, false),
        "grid-row-start" => set_grid_placement_side(style, value, false, true),
        "grid-row-end" => set_grid_placement_side(style, value, false, false),
        "transform" => parse_transform(style, value),
        "filter" => set_containing_block_trigger(
            style,
            crate::CB_TRIGGER_FILTER,
            non_none_value(value),
        ),
        "backdrop-filter" | "-webkit-backdrop-filter" => set_containing_block_trigger(
            style,
            crate::CB_TRIGGER_BACKDROP_FILTER,
            non_none_value(value),
        ),
        "perspective" => set_containing_block_trigger(
            style,
            crate::CB_TRIGGER_PERSPECTIVE,
            non_none_value(value),
        ),
        "contain" => {
            let establishes = value
                .split_whitespace()
                .any(|v| matches!(v.to_ascii_lowercase().as_str(), "layout" | "paint" | "strict" | "content"));
            set_containing_block_trigger(style, crate::CB_TRIGGER_CONTAIN, establishes);
        }
        "will-change" => {
            let establishes = value
                .split([',', ' '])
                .map(str::trim)
                .any(|v| matches!(v.to_ascii_lowercase().as_str(), "transform" | "filter" | "backdrop-filter" | "perspective" | "contain"));
            set_containing_block_trigger(style, crate::CB_TRIGGER_WILL_CHANGE, establishes);
        }
        "content-visibility" => set_containing_block_trigger(
            style,
            crate::CB_TRIGGER_CONTENT_VISIBILITY,
            value.trim().eq_ignore_ascii_case("auto"),
        ),
        "box-shadow" | "-webkit-box-shadow" => {
            style.box_shadow = parse_box_shadow(value, style.color);
        }
        _ => {}
    }
}

fn apply_animation_shorthand(style: &mut LayoutStyle, value: &str) {
    style.animation_name = None;
    style.animation_fill_forwards = false;
    style.animation_iteration_infinite = false;

    let first = split_top_level(value, ',').into_iter().next().unwrap_or("").trim();
    if first.is_empty() || first.eq_ignore_ascii_case("none") {
        return;
    }

    for token in split_ws_paren(first) {
        let lower = token.to_ascii_lowercase();
        let timing_keyword = matches!(
            lower.as_str(),
            "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out"
        ) || lower.starts_with("cubic-bezier(")
            || lower.starts_with("steps(")
            || lower.starts_with("linear(");
        let time = lower
            .strip_suffix("ms")
            .or_else(|| lower.strip_suffix('s'))
            .and_then(|number| number.parse::<f32>().ok())
            .is_some();
        let control_keyword = matches!(
            lower.as_str(),
            "normal"
                | "reverse"
                | "alternate"
                | "alternate-reverse"
                | "backwards"
                | "running"
                | "paused"
        );
        if lower == "forwards" || lower == "both" {
            style.animation_fill_forwards = true;
        } else if lower == "infinite" {
            style.animation_iteration_infinite = true;
        } else if time
            || timing_keyword
            || control_keyword
            || lower.parse::<f32>().is_ok()
            || lower == "none"
        {
            continue;
        } else if style.animation_name.is_none() {
            style.animation_name = Some(token.to_string());
        }
    }
}

/// Parse the common CSS Box Alignment values that taffy can represent.
///
/// The outer option distinguishes an invalid declaration from `auto`, which
/// resets a preceding declaration to the inherited item-alignment behavior.
fn self_alignment_value(value: &str) -> Option<Option<taffy::AlignSelf>> {
    let normalized = value.trim().to_ascii_lowercase();
    let alignment = match normalized.as_str() {
        "auto" => return Some(None),
        "normal" => taffy::AlignSelf::STRETCH,
        "start" | "self-start" => taffy::AlignSelf::START,
        "end" | "self-end" => taffy::AlignSelf::END,
        "flex-start" => taffy::AlignSelf::FLEX_START,
        "flex-end" => taffy::AlignSelf::FLEX_END,
        "center" => taffy::AlignSelf::CENTER,
        "baseline" | "first baseline" => taffy::AlignSelf::BASELINE,
        "stretch" => taffy::AlignSelf::STRETCH,
        "safe start" | "safe self-start" => taffy::AlignSelf::SAFE_START,
        "safe end" | "safe self-end" => taffy::AlignSelf::SAFE_END,
        "safe flex-start" => taffy::AlignSelf::SAFE_FLEX_START,
        "safe flex-end" => taffy::AlignSelf::SAFE_FLEX_END,
        "safe center" => taffy::AlignSelf::SAFE_CENTER,
        "unsafe start" | "unsafe self-start" => taffy::AlignSelf::START,
        "unsafe end" | "unsafe self-end" => taffy::AlignSelf::END,
        "unsafe flex-start" => taffy::AlignSelf::FLEX_START,
        "unsafe flex-end" => taffy::AlignSelf::FLEX_END,
        "unsafe center" => taffy::AlignSelf::CENTER,
        _ => return None,
    };
    Some(Some(alignment))
}

fn self_alignment_pair(
    value: &str,
) -> Option<(Option<taffy::AlignSelf>, Option<taffy::JustifySelf>)> {
    if let Some(alignment) = self_alignment_value(value) {
        return Some((alignment, alignment));
    }
    let tokens: Vec<&str> = value.split_whitespace().collect();
    for split in 1..tokens.len() {
        let align = tokens[..split].join(" ");
        let justify = tokens[split..].join(" ");
        if let (Some(align), Some(justify)) = (
            self_alignment_value(&align),
            self_alignment_value(&justify),
        ) {
            return Some((align, justify));
        }
    }
    None
}

fn content_alignment_value(value: &str) -> Option<taffy::AlignContent> {
    let normalized = value.trim().to_ascii_lowercase();
    Some(match normalized.as_str() {
        "normal" | "stretch" => taffy::AlignContent::STRETCH,
        "start" => taffy::AlignContent::START,
        "end" => taffy::AlignContent::END,
        "flex-start" => taffy::AlignContent::FLEX_START,
        "flex-end" => taffy::AlignContent::FLEX_END,
        "center" => taffy::AlignContent::CENTER,
        "space-between" => taffy::AlignContent::SPACE_BETWEEN,
        "space-around" => taffy::AlignContent::SPACE_AROUND,
        "space-evenly" => taffy::AlignContent::SPACE_EVENLY,
        "safe start" => taffy::AlignContent::SAFE_START,
        "safe end" => taffy::AlignContent::SAFE_END,
        "safe flex-start" => taffy::AlignContent::SAFE_FLEX_START,
        "safe flex-end" => taffy::AlignContent::SAFE_FLEX_END,
        "safe center" => taffy::AlignContent::SAFE_CENTER,
        "unsafe start" => taffy::AlignContent::START,
        "unsafe end" => taffy::AlignContent::END,
        "unsafe flex-start" => taffy::AlignContent::FLEX_START,
        "unsafe flex-end" => taffy::AlignContent::FLEX_END,
        "unsafe center" => taffy::AlignContent::CENTER,
        _ => return None,
    })
}

fn content_alignment_pair(
    value: &str,
) -> Option<(taffy::AlignContent, taffy::JustifyContent)> {
    if let Some(alignment) = content_alignment_value(value) {
        return Some((alignment, alignment));
    }
    let tokens: Vec<&str> = value.split_whitespace().collect();
    for split in 1..tokens.len() {
        let align = tokens[..split].join(" ");
        let justify = tokens[split..].join(" ");
        if let (Some(align), Some(justify)) = (
            content_alignment_value(&align),
            content_alignment_value(&justify),
        ) {
            return Some((align, justify));
        }
    }
    None
}

/// Parse the subset of `transform` obscura applies at paint time: `translate`,
/// `translateX`, `translateY` (px and %, stored unresolved so % can resolve
/// against the element's own border box later) and `scale`/`scaleX`/`scaleY`.
/// `rotate`, `skew`, `matrix`, and `perspective` are ignored (left unhandled)
/// rather than erroring, so a value that mixes them still contributes its
/// translate and scale parts.
fn parse_transform(style: &mut LayoutStyle, value: &str) {
    let v = value.trim();
    if v.is_empty() {
        return;
    }
    // `transform: none` resets any transform from a lower-priority rule (the
    // carousel idiom: a class translates the track, an inline style clears it
    // for the untransformed state).
    if v.eq_ignore_ascii_case("none") {
        style.transform_translate = None;
        style.transform_scale = None;
        set_containing_block_trigger(style, crate::CB_TRIGGER_TRANSFORM, false);
        return;
    }
    let functions = transform_functions(v);
    if functions.is_empty() {
        return;
    }
    set_containing_block_trigger(style, crate::CB_TRIGGER_TRANSFORM, true);
    let zero = crate::Dimension::Px(0.0);
    let (mut tx, mut ty): (Option<crate::Dimension>, Option<crate::Dimension>) = (None, None);
    let (mut sx, mut sy): (Option<f32>, Option<f32>) = (None, None);
    for (func, args) in functions {
        let parts: Vec<&str> = args.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        match func.to_ascii_lowercase().as_str() {
            // translate3d's z component is ignored (no perspective model);
            // Swiper and similar carousels position their track exclusively
            // through translate3d, so dropping it left every slide stacked.
            "translate" | "translate3d" => {
                if let Some(a) = parts.first() {
                    tx = Some(dimension_value(a));
                }
                // A missing second component is 0 (translateY), not inherited.
                ty = Some(parts.get(1).map(|a| dimension_value(a)).unwrap_or(zero));
            }
            "translatex" => {
                if let Some(a) = parts.first() {
                    tx = Some(dimension_value(a));
                }
            }
            "translatey" => {
                if let Some(a) = parts.first() {
                    ty = Some(dimension_value(a));
                }
            }
            "scale" => {
                if let Some(a) = parts.first().and_then(|s| scale_number(s)) {
                    // scale(s) is uniform; scale(sx, sy) sets each axis.
                    sy = Some(parts.get(1).and_then(|s| scale_number(s)).unwrap_or(a));
                    sx = Some(a);
                }
            }
            "scalex" => {
                if let Some(a) = parts.first().and_then(|s| scale_number(s)) {
                    sx = Some(a);
                }
            }
            "scaley" => {
                if let Some(a) = parts.first().and_then(|s| scale_number(s)) {
                    sy = Some(a);
                }
            }
            // rotate / skew / matrix / perspective: not modeled, skip.
            _ => {}
        }
    }
    if tx.is_some() || ty.is_some() {
        style.transform_translate = Some((tx.unwrap_or(zero), ty.unwrap_or(zero)));
    }
    if sx.is_some() || sy.is_some() {
        style.transform_scale = Some((sx.unwrap_or(1.0), sy.unwrap_or(1.0)));
    }
}

fn non_none_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.eq_ignore_ascii_case("none")
}

fn set_containing_block_trigger(style: &mut LayoutStyle, trigger: u16, enabled: bool) {
    if enabled {
        style.containing_block_triggers |= trigger;
    } else {
        style.containing_block_triggers &= !trigger;
    }
}

/// Split a `transform` value into its `name(args)` functions, in source order.
/// Tokenizes on parentheses (tracking depth so a nested `calc(...)` inside an
/// argument stays intact); the function name is the trailing identifier run
/// just before each `(`.
fn transform_functions(value: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('(') {
        let name: String = rest[..open]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let after = &rest[open + 1..];
        let mut depth = 1i32;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(e) = end else { break };
        if !name.is_empty() {
            out.push((name, after[..e].to_string()));
        }
        rest = &after[e + 1..];
    }
    out
}

/// Parse a unitless scale factor. A trailing `%` (`scale(50%)`) is accepted and
/// divided by 100, matching the individual `scale` property's percentage form.
fn scale_number(s: &str) -> Option<f32> {
    let t = s.trim();
    if let Some(p) = t.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|v| v / 100.0);
    }
    t.parse::<f32>().ok()
}

/// Parse a CSS grid track list (`min-content 1fr min-content`, `12.25rem
/// minmax(0,1fr)`) into taffy sizing functions. Tokenizes respecting the
/// parentheses in `minmax(...)` / `fit-content(...)`.
/// Parse a track list into taffy sizing functions plus the `[line-name]` map
/// (name -> 1-based grid line number). `repeat(n, ...)` is expanded to n copies;
/// auto-fill/auto-fit remain typed repetitions so layout can derive their count
/// from the used container width. `[name]` annotations are captured rather than
/// turned into tracks. First occurrence of a name wins.
pub(crate) fn parse_track_list_named(
    value: &str,
) -> (
    Vec<taffy::GridTemplateComponent<String>>,
    Vec<(String, i16)>,
) {
    let mut tracks = Vec::new();
    let mut names = Vec::new();
    let mut line: i16 = 1;
    for tok in tokenize_tracks(value) {
        expand_track_token(&tok, &mut tracks, &mut names, &mut line);
    }
    (tracks, names)
}

fn expand_track_token(
    tok: &str,
    tracks: &mut Vec<taffy::GridTemplateComponent<String>>,
    names: &mut Vec<(String, i16)>,
    line: &mut i16,
) {
    let t = tok.trim();
    if t.starts_with('[') {
        let inner = t.trim_start_matches('[').trim_end_matches(']');
        for name in inner.split_whitespace() {
            names.push((name.to_string(), *line));
        }
        return;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("repeat(") && t.ends_with(')') {
        let inner = &t["repeat(".len()..t.len() - 1];
        if let Some((cnt, sub)) = inner.split_once(',') {
            let subtoks = tokenize_tracks(sub.trim());
            let repetition = match cnt.trim().to_ascii_lowercase().as_str() {
                "auto-fill" => Some(taffy::RepetitionCount::AutoFill),
                "auto-fit" => Some(taffy::RepetitionCount::AutoFit),
                _ => None,
            };
            if let Some(count) = repetition {
                let repeated_tracks = subtoks
                    .iter()
                    .filter(|st| !st.trim_start().starts_with('['))
                    .map(|st| track(st))
                    .collect();
                tracks.push(taffy::GridTemplateComponent::Repeat(
                    taffy::GridTemplateRepetition {
                        count,
                        tracks: repeated_tracks,
                        line_names: Vec::new(),
                    },
                ));
                *line += 1;
                return;
            }
            let count = cnt.trim().parse::<usize>().unwrap_or(1).min(1000);
            for _ in 0..count {
                for st in &subtoks {
                    expand_track_token(st, tracks, names, line);
                }
            }
        }
        return;
    }
    tracks.push(taffy::GridTemplateComponent::Single(track(t)));
    *line += 1;
}

pub(crate) fn build_line_map(pairs: Vec<(String, i16)>) -> std::collections::HashMap<String, i16> {
    let mut m = std::collections::HashMap::new();
    for (name, line) in pairs {
        m.entry(name).or_insert(line);
    }
    m
}

/// Split a track list on whitespace while keeping `func(a, b)` groups and
/// `[line-name lists]` intact.
fn tokenize_tracks(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_bracket = false;
    for c in value.chars() {
        match c {
            '[' => { in_bracket = true; cur.push(c); }
            ']' => { in_bracket = false; cur.push(c); }
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            c if c.is_whitespace() && depth == 0 && !in_bracket => {
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
            return MinMax { min: min_track(a.trim()), max: max_track(b.trim()) };
        }
    }
    if let Some(inner) = lower.strip_prefix("fit-content(").and_then(|s| s.strip_suffix(')')) {
        let px = px_value(inner.trim()).unwrap_or(0.0);
        return MinMax {
            min: taffy::MinTrackSizingFunction::auto(),
            max: taffy::MaxTrackSizingFunction::fit_content_px(px),
        };
    }
    MinMax { min: min_track(t), max: max_track(t) }
}

fn min_track(tok: &str) -> taffy::MinTrackSizingFunction {
    use taffy::MinTrackSizingFunction as M;
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "min-content" => M::min_content(),
        "max-content" => M::max_content(),
        "auto" => M::auto(),
        other => {
            if other.ends_with("fr") {
                // Flexible tracks have an automatic minimum.
                M::auto()
            } else if let Some(p) = other.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()) {
                M::percent(p / 100.0)
            } else if let Some(px) = px_value(other) {
                M::length(px)
            } else {
                M::auto()
            }
        }
    }
}

fn max_track(tok: &str) -> taffy::MaxTrackSizingFunction {
    use taffy::MaxTrackSizingFunction as M;
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "min-content" => M::min_content(),
        "max-content" => M::max_content(),
        "auto" => M::auto(),
        other => {
            if let Some(fr) = other.strip_suffix("fr").and_then(|n| n.trim().parse::<f32>().ok()) {
                M::fr(fr)
            } else if let Some(p) = other.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()) {
                M::percent(p / 100.0)
            } else if let Some(px) = px_value(other) {
                M::length(px)
            } else {
                M::auto()
            }
        }
    }
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
        let (tracks, names) = parse_track_list_named(rows_part);
        style.grid_template_rows = tracks;
        style.grid_row_line_names = (!names.is_empty()).then(|| build_line_map(names));
    }
    if let Some(cols) = cols_part {
        let (tracks, names) = parse_track_list_named(cols);
        style.grid_template_columns = tracks;
        style.grid_col_line_names = (!names.is_empty()).then(|| build_line_map(names));
    }
}

/// Parse the common `grid` shorthand forms. A side containing `auto-flow`
/// defines the implicit placement axis; the opposite side is the explicit
/// track list. Without `auto-flow`, this is the `grid-template` shorthand.
fn parse_grid_shorthand(style: &mut LayoutStyle, value: &str) {
    let Some((rows, columns)) = value.split_once('/') else {
        parse_grid_template(style, value);
        return;
    };
    let rows = rows.trim();
    let columns = columns.trim();
    if rows.to_ascii_lowercase().contains("auto-flow") {
        style.grid_template_rows.clear();
        let (tracks, names) = parse_track_list_named(columns);
        style.grid_template_columns = tracks;
        style.grid_col_line_names = (!names.is_empty()).then(|| build_line_map(names));
        style.grid_auto_flow = Some(if rows.to_ascii_lowercase().contains("dense") {
            taffy::GridAutoFlow::RowDense
        } else {
            taffy::GridAutoFlow::Row
        });
    } else if columns.to_ascii_lowercase().contains("auto-flow") {
        style.grid_template_columns.clear();
        let (tracks, names) = parse_track_list_named(rows);
        style.grid_template_rows = tracks;
        style.grid_row_line_names = (!names.is_empty()).then(|| build_line_map(names));
        style.grid_auto_flow = Some(if columns.to_ascii_lowercase().contains("dense") {
            taffy::GridAutoFlow::ColumnDense
        } else {
            taffy::GridAutoFlow::Column
        });
    } else {
        parse_grid_template(style, value);
    }
}

fn parse_grid_auto_flow(value: &str) -> Option<taffy::GridAutoFlow> {
    let lower = value.trim().to_ascii_lowercase();
    let dense = lower.split_whitespace().any(|token| token == "dense");
    let column = lower.split_whitespace().any(|token| token == "column");
    Some(match (column, dense) {
        (false, false) => taffy::GridAutoFlow::Row,
        (false, true) => taffy::GridAutoFlow::RowDense,
        (true, false) => taffy::GridAutoFlow::Column,
        (true, true) => taffy::GridAutoFlow::ColumnDense,
    })
}

/// Store a `grid-column`/`grid-row` value. Numeric/`span` forms resolve to a
/// `taffy::Line` now; a value that names a grid line (`content-start /
/// content-end`, or the `grid-column: content` area shorthand) is kept raw and
/// resolved against the parent's line-name map in `dom::resolve_grid_areas`.
/// Whichever representation is set, the other is cleared so a later cascade
/// rule of the opposite kind fully overrides it.
fn set_grid_placement(style: &mut LayoutStyle, value: &str, is_col: bool) {
    if grid_line_has_name(value) {
        let raw = Some(value.trim().to_string());
        if is_col {
            style.grid_column_raw = raw;
            style.grid_column = None;
        } else {
            style.grid_row_raw = raw;
            style.grid_row = None;
        }
    } else {
        let line = parse_grid_line(value);
        if is_col {
            style.grid_column = line;
            style.grid_column_raw = None;
        } else {
            style.grid_row = line;
            style.grid_row_raw = None;
        }
    }
}

/// Apply one grid-placement longhand without clearing the opposite side.
/// Responsive grid systems commonly establish a default span with
/// `.layout > * { grid-column-end: span 4 }` and override only the start/end
/// on selected children; dropping these longhands traps every item in one
/// auto-placed track.
fn set_grid_placement_side(
    style: &mut LayoutStyle,
    value: &str,
    is_col: bool,
    is_start: bool,
) {
    if grid_line_has_name(value) {
        let raw_slot = if is_col {
            &mut style.grid_column_raw
        } else {
            &mut style.grid_row_raw
        };
        let (mut start, mut end) = raw_slot
            .as_deref()
            .and_then(|raw| raw.split_once('/'))
            .map(|(start, end)| {
                (start.trim().to_string(), end.trim().to_string())
            })
            .unwrap_or_else(|| ("auto".to_string(), "auto".to_string()));
        if is_start {
            start = value.trim().to_string();
        } else {
            end = value.trim().to_string();
        }
        *raw_slot = Some(format!("{start} / {end}"));
        if is_col {
            style.grid_column = None;
        } else {
            style.grid_row = None;
        }
        return;
    }

    let placement = parse_grid_placement(value);
    let line_slot = if is_col {
        style.grid_column_raw = None;
        &mut style.grid_column
    } else {
        style.grid_row_raw = None;
        &mut style.grid_row
    };
    let mut line = line_slot.clone().unwrap_or(taffy::Line {
        start: taffy::GridPlacement::Auto,
        end: taffy::GridPlacement::Auto,
    });
    if is_start {
        line.start = placement;
    } else {
        line.end = placement;
    }
    *line_slot = Some(line);
}

/// True when a `grid-column`/`grid-row` value references a named line (any
/// alphabetic token that is not a bare `span <n>` count), so it must defer to
/// the parent's line-name map.
fn grid_line_has_name(value: &str) -> bool {
    value.split('/').any(|part| {
        let p = part.trim();
        let rest = p.strip_prefix("span").map(str::trim).unwrap_or(p);
        rest.chars().any(|c| c.is_ascii_alphabetic())
    })
}

/// Parse `grid-column`/`grid-row` values: `2`, `1 / 3`, `span 2`.
fn parse_grid_line(value: &str) -> Option<taffy::Line<taffy::GridPlacement>> {
    let (a, b) = match value.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (value, None),
    };
    let start = parse_grid_placement(a);
    let end = b
        .map(parse_grid_placement)
        .unwrap_or(taffy::GridPlacement::Auto);
    Some(taffy::Line { start, end })
}

fn parse_grid_placement(value: &str) -> taffy::GridPlacement {
    let value = value.trim();
    if let Some(span) = value.strip_prefix("span").map(str::trim) {
        if let Ok(span) = span.parse::<u16>() {
            return taffy::style_helpers::span(span);
        }
    }
    if let Ok(line) = value.parse::<i16>() {
        return taffy::style_helpers::line(line);
    }
    taffy::GridPlacement::Auto
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
    // hsl()/hsla() functional notation.
    if let Some(rest) = lower_full.strip_prefix("hsl(").or_else(|| lower_full.strip_prefix("hsla(")) {
        let inner = rest.strip_suffix(')').unwrap_or(rest);
        let parts: Vec<&str> = inner.split([',', '/', ' ']).filter(|p| !p.trim().is_empty()).collect();
        if parts.len() >= 3 {
            let h = parts[0].trim().trim_end_matches("deg").parse::<f32>().ok()?;
            let s = parts[1].trim().trim_end_matches('%').parse::<f32>().ok()? / 100.0;
            let l = parts[2].trim().trim_end_matches('%').parse::<f32>().ok()? / 100.0;
            let a = parts.get(3)
                .and_then(|s| s.trim().trim_end_matches('%').parse::<f32>().ok())
                .map(|v| if parts[3].contains('%') { (v * 2.55).round() } else { (v * 255.0).round() }.clamp(0.0, 255.0) as u8)
                .unwrap_or(255);
            return Some(hsl_to_rgba(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0), a));
        }
        return None;
    }
    // oklch()/oklab() - Tailwind v4's entire palette. Convert through OKLab to
    // sRGB; without this every modern-framework color resolved to nothing.
    if lower_full.starts_with("oklch(") || lower_full.starts_with("oklab(") {
        let is_lch = lower_full.starts_with("oklch(");
        let inner = &lower_full[if is_lch { 6 } else { 6 }..];
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        let (main, alpha) = match inner.split_once('/') {
            Some((m, a)) => (m, Some(a)),
            None => (inner, None),
        };
        let comps: Vec<&str> = main.split([',', ' ']).filter(|p| !p.trim().is_empty()).collect();
        if comps.len() >= 3 {
            let num = |s: &str| -> Option<f32> {
                let s = s.trim();
                s.strip_suffix('%').map(|p| p.parse::<f32>().map(|v| v / 100.0)).unwrap_or_else(|| s.parse::<f32>())
                    .ok()
            };
            let l = num(comps[0])?;
            let c = num(comps[1])?;
            let a = alpha
                .and_then(|s| {
                    let s = s.trim();
                    s.strip_suffix('%').map(|p| p.parse::<f32>().map(|v| v / 100.0)).unwrap_or_else(|| s.parse::<f32>()).ok()
                })
                .unwrap_or(1.0);
            let (oa, ob) = if is_lch {
                let h = comps[2].trim().trim_end_matches("deg").parse::<f32>().ok()?;
                let hr = h.to_radians();
                (c * hr.cos(), c * hr.sin())
            } else {
                (c, comps[2].trim().parse::<f32>().ok()?)
            };
            return Some(oklab_to_rgba(l, oa, ob, (a * 255.0).round().clamp(0.0, 255.0) as u8));
        }
        return None;
    }
    // color-mix(in <space>, c1 p1%, c2 p2%) - Tailwind v4 uses this pervasively,
    // usually `color-mix(in oklab, <color> N%, transparent)` to apply opacity.
    if lower_full.starts_with("color-mix(") {
        let inner = raw[raw.to_ascii_lowercase().find("color-mix(").unwrap() + "color-mix(".len()..].trim_end();
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        let args = split_top_commas(inner);
        if args.len() >= 3 {
            let parse_arg = |s: &str| -> Option<([u8; 4], Option<f32>)> {
                let s = s.trim();
                if let Some(idx) = s.rfind(char::is_whitespace) {
                    let tail = s[idx + 1..].trim();
                    if let Some(p) = tail.strip_suffix('%').and_then(|x| x.parse::<f32>().ok()) {
                        return parse_color(s[..idx].trim()).map(|c| (c, Some(p / 100.0)));
                    }
                }
                parse_color(s).map(|c| (c, None))
            };
            if let (Some((c1, p1)), Some((c2, p2))) = (parse_arg(args[1]), parse_arg(args[2])) {
                let (w1, w2) = match (p1, p2) {
                    (Some(a), Some(b)) => (a, b),
                    (Some(a), None) => (a, 1.0 - a),
                    (None, Some(b)) => (1.0 - b, b),
                    (None, None) => (0.5, 0.5),
                };
                let tot = (w1 + w2).max(1e-6);
                let (w1, w2) = (w1 / tot, w2 / tot);
                // Mixing with a fully transparent color is the opacity idiom:
                // keep the visible color, scale its alpha (not toward black).
                if c2[3] == 0 {
                    return Some([c1[0], c1[1], c1[2], (c1[3] as f32 * w1).round().clamp(0.0, 255.0) as u8]);
                }
                if c1[3] == 0 {
                    return Some([c2[0], c2[1], c2[2], (c2[3] as f32 * w2).round().clamp(0.0, 255.0) as u8]);
                }
                let m = |i: usize| (c1[i] as f32 * w1 + c2[i] as f32 * w2).round().clamp(0.0, 255.0) as u8;
                return Some([m(0), m(1), m(2), m(3)]);
            }
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
        _ => named_color(&v),
    }
}

/// The remaining common CSS named colors (the hot ones from real sites) beyond
/// the handful spelled out above.
fn named_color(v: &str) -> Option<[u8; 4]> {
    let rgb = match v {
        "darkblue" => [0, 0, 139],
        "mediumblue" => [0, 0, 205],
        "royalblue" => [65, 105, 225],
        "dodgerblue" => [30, 144, 255],
        "cornflowerblue" => [100, 149, 237],
        "steelblue" => [70, 130, 180],
        "deepskyblue" => [0, 191, 255],
        "skyblue" => [135, 206, 235],
        "lightskyblue" => [135, 206, 250],
        "lightblue" => [173, 216, 230],
        "powderblue" => [176, 224, 230],
        "cadetblue" => [95, 158, 160],
        "slateblue" => [106, 90, 205],
        "darkslateblue" => [72, 61, 139],
        "midnightblue" => [25, 25, 112],
        "indigo" => [75, 0, 130],
        "darkgreen" => [0, 100, 0],
        "forestgreen" => [34, 139, 34],
        "seagreen" => [46, 139, 87],
        "mediumseagreen" => [60, 179, 113],
        "limegreen" => [50, 205, 50],
        "yellowgreen" => [154, 205, 50],
        "olivedrab" => [107, 142, 35],
        "darkolivegreen" => [85, 107, 47],
        "greenyellow" => [173, 255, 47],
        "lightgreen" => [144, 238, 144],
        "palegreen" => [152, 251, 152],
        "springgreen" => [0, 255, 127],
        "mediumaquamarine" => [102, 205, 170],
        "aquamarine" => [127, 255, 212],
        "turquoise" => [64, 224, 208],
        "mediumturquoise" => [72, 209, 204],
        "darkcyan" => [0, 139, 139],
        "crimson" => [220, 20, 60],
        "firebrick" => [178, 34, 34],
        "darkred" => [139, 0, 0],
        "indianred" => [205, 92, 92],
        "tomato" => [255, 99, 71],
        "orangered" => [255, 69, 0],
        "coral" => [255, 127, 80],
        "salmon" => [250, 128, 114],
        "lightsalmon" => [255, 160, 122],
        "darksalmon" => [233, 150, 122],
        "hotpink" => [255, 105, 180],
        "deeppink" => [255, 20, 147],
        "pink" => [255, 192, 203],
        "lightpink" => [255, 182, 193],
        "palevioletred" => [219, 112, 147],
        "mediumvioletred" => [199, 21, 133],
        "violet" => [238, 130, 238],
        "orchid" => [218, 112, 214],
        "plum" => [221, 160, 221],
        "mediumpurple" => [147, 112, 219],
        "blueviolet" => [138, 43, 226],
        "darkviolet" => [148, 0, 211],
        "darkorchid" => [153, 50, 204],
        "darkmagenta" => [139, 0, 139],
        "lavender" => [230, 230, 250],
        "thistle" => [216, 191, 216],
        "gold" => [255, 215, 0],
        "goldenrod" => [218, 165, 32],
        "darkgoldenrod" => [184, 134, 11],
        "khaki" => [240, 230, 140],
        "darkkhaki" => [189, 183, 107],
        "peachpuff" => [255, 218, 185],
        "moccasin" => [255, 228, 181],
        "papayawhip" => [255, 239, 213],
        "wheat" => [245, 222, 179],
        "tan" => [210, 180, 140],
        "burlywood" => [222, 184, 135],
        "sandybrown" => [244, 164, 96],
        "peru" => [205, 133, 63],
        "chocolate" => [210, 105, 30],
        "sienna" => [160, 82, 45],
        "saddlebrown" => [139, 69, 19],
        "brown" => [165, 42, 42],
        "rosybrown" => [188, 143, 143],
        "darkorange" => [255, 140, 0],
        "lightyellow" => [255, 255, 224],
        "lightgoldenrodyellow" => [250, 250, 210],
        "lemonchiffon" => [255, 250, 205],
        "beige" => [245, 245, 220],
        "ivory" => [255, 255, 240],
        "azure" => [240, 255, 255],
        "mintcream" => [245, 255, 250],
        "honeydew" => [240, 255, 240],
        "snow" => [255, 250, 250],
        "seashell" => [255, 245, 238],
        "linen" => [250, 240, 230],
        "oldlace" => [253, 245, 230],
        "floralwhite" => [255, 250, 240],
        "ghostwhite" => [248, 248, 255],
        "aliceblue" => [240, 248, 255],
        "lavenderblush" => [255, 240, 245],
        "mistyrose" => [255, 228, 225],
        "cornsilk" => [255, 248, 220],
        "antiquewhite" => [250, 235, 215],
        "bisque" => [255, 228, 196],
        "blanchedalmond" => [255, 235, 205],
        "navajowhite" => [255, 222, 173],
        "dimgray" | "dimgrey" => [105, 105, 105],
        "slategray" | "slategrey" => [112, 128, 144],
        "lightslategray" | "lightslategrey" => [119, 136, 153],
        "darkslategray" | "darkslategrey" => [47, 79, 79],
        _ => return None,
    };
    Some([rgb[0], rgb[1], rgb[2], 255])
}

/// Convert `hsl()`/`hsla()` to RGBA. `h` in degrees, `s`/`l` as 0-1.
/// Convert an OKLab color (L in 0..1, a/b unbounded) to sRGB rgba bytes.
/// (oklch is converted to oklab by the caller.) Standard Björn Ottosson matrix.
fn oklab_to_rgba(l: f32, a: f32, b: f32, alpha: u8) -> [u8; 4] {
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;
    let (lc, mc, sc) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let lr = 4.0767416621 * lc - 3.3077115913 * mc + 0.2309699292 * sc;
    let lg = -1.2684380046 * lc + 2.6097574011 * mc - 0.3413193965 * sc;
    let lb = -0.0041960863 * lc - 0.7034186147 * mc + 1.7076147010 * sc;
    let enc = |x: f32| {
        let x = x.clamp(0.0, 1.0);
        let s = if x <= 0.0031308 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [enc(lr), enc(lg), enc(lb), alpha]
}

/// Split on top-level commas, respecting nested `()` (so a `color-mix` argument
/// like `oklch(0.7 0.1 20)` or `var(--x, y)` is not shattered).
fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn hsl_to_rgba(h: f32, s: f32, l: f32, a: u8) -> [u8; 4] {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        (((r + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
        (((g + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
        (((b + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
        a,
    ]
}

enum Side { Top, Right, Bottom, Left }

fn border_side_width(value: &str) -> Option<f32> {
    if value.split_whitespace().any(|token| {
        token.eq_ignore_ascii_case("none") || token.eq_ignore_ascii_case("hidden")
    }) {
        Some(0.0)
    } else {
        px(value)
    }
}

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

fn deferred_length_expression(value: &str) -> Option<String> {
    value.trim().contains('(').then(|| value.trim().to_string())
}

/// Resolve functional CSS lengths once the actual layout context is known.
/// `vw`/`vh` are one-percent viewport units and `percent_base` is the relevant
/// containing-block dimension. Custom properties have already been substituted
/// by the cascade before this function sees the expression.
pub(crate) fn resolve_contextual_length(
    value: &str,
    em_px: f32,
    rem_px: f32,
    vw: f32,
    vh: f32,
    percent_base: f32,
) -> Option<f32> {
    let context = LengthContext {
        em_px,
        rem_px,
        vw,
        vh,
        percent_base,
    };
    resolve_contextual(value, &context)
}

#[derive(Clone, Copy)]
struct LengthContext {
    em_px: f32,
    rem_px: f32,
    vw: f32,
    vh: f32,
    percent_base: f32,
}

fn resolve_contextual(value: &str, context: &LengthContext) -> Option<f32> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('(') {
        let end = find_matching_paren(rest)?;
        if end + 2 == value.len() {
            return eval_contextual_calc(&rest[..end], context);
        }
    }
    if let Some(rest) = value.strip_prefix("var(") {
        let end = find_matching_paren(rest)?;
        let inner = &rest[..end];
        let (_, fallback) = inner.split_once(',')?;
        return resolve_contextual(fallback.trim(), context);
    }
    if let Some(rest) = value.strip_prefix("calc(") {
        let end = find_matching_paren(rest)?;
        return eval_contextual_calc(&rest[..end], context);
    }
    if let Some(rest) = value
        .strip_prefix("max(")
        .or_else(|| value.strip_prefix("min("))
    {
        let is_max = value.starts_with("max(");
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        let mut values = args
            .iter()
            .filter_map(|arg| eval_contextual_calc(arg, context));
        let mut best = values.next()?;
        for candidate in values {
            if (is_max && candidate > best) || (!is_max && candidate < best) {
                best = candidate;
            }
        }
        return Some(best);
    }
    if let Some(rest) = value.strip_prefix("clamp(") {
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        if args.len() == 3 {
            let low = eval_contextual_calc(args[0], context)?;
            let preferred = eval_contextual_calc(args[1], context)?;
            let high = eval_contextual_calc(args[2], context)?;
            return Some(preferred.min(high).max(low));
        }
    }
    if let Some(rest) = value.strip_prefix("round(") {
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        let (strategy, value_index) = match args.first()?.trim() {
            "nearest" | "up" | "down" | "to-zero" => (args[0].trim(), 1),
            _ => ("nearest", 0),
        };
        let resolved = eval_contextual_calc(args.get(value_index)?.trim(), context)?;
        let step = match args.get(value_index + 1) {
            Some(step) => eval_contextual_calc(step.trim(), context)?,
            None => 1.0,
        };
        return round_css_value(resolved, step, strategy);
    }
    contextual_atom(value, context)
}

fn round_css_value(value: f32, step: f32, strategy: &str) -> Option<f32> {
    if !value.is_finite() || !step.is_finite() || step == 0.0 {
        return None;
    }
    let quotient = value / step.abs();
    let rounded = match strategy {
        "up" => quotient.ceil(),
        "down" => quotient.floor(),
        "to-zero" => quotient.trunc(),
        _ => quotient.round(),
    };
    Some(rounded * step.abs())
}

fn contextual_atom(value: &str, context: &LengthContext) -> Option<f32> {
    let lower = value.trim().to_ascii_lowercase();
    let parse = |number: &str| number.trim().parse::<f32>().ok();
    if let Some(value) = lower.strip_suffix("rem").and_then(parse) {
        return Some(value * context.rem_px);
    }
    if let Some(value) = lower.strip_suffix("em").and_then(parse) {
        return Some(value * context.em_px);
    }
    if let Some(value) = lower.strip_suffix("ex").and_then(parse) {
        return Some(value * context.em_px * 0.528_320_3);
    }
    if let Some(value) = lower.strip_suffix("vmin").and_then(parse) {
        return Some(value * context.vw.min(context.vh));
    }
    if let Some(value) = lower.strip_suffix("vmax").and_then(parse) {
        return Some(value * context.vw.max(context.vh));
    }
    if let Some(value) = lower
        .strip_suffix("dvw")
        .or_else(|| lower.strip_suffix("svw"))
        .or_else(|| lower.strip_suffix("lvw"))
        .and_then(parse)
    {
        return Some(value * context.vw);
    }
    if let Some(value) = lower
        .strip_suffix("dvh")
        .or_else(|| lower.strip_suffix("svh"))
        .or_else(|| lower.strip_suffix("lvh"))
        .and_then(parse)
    {
        return Some(value * context.vh);
    }
    if let Some(value) = lower.strip_suffix("vw").and_then(parse) {
        return Some(value * context.vw);
    }
    if let Some(value) = lower.strip_suffix("vh").and_then(parse) {
        return Some(value * context.vh);
    }
    if let Some(value) = lower.strip_suffix('%').and_then(parse) {
        return Some(value * context.percent_base / 100.0);
    }
    if let Some(value) = lower.strip_suffix("px").and_then(parse) {
        return Some(value);
    }
    if let Some(value) = lower.strip_suffix("pt").and_then(parse) {
        return Some(value * 1.333);
    }
    parse(&lower)
}

fn eval_contextual_calc(expr: &str, context: &LengthContext) -> Option<f32> {
    let mut terms: Vec<(f32, String)> = Vec::new();
    let mut sign = 1.0;
    let mut current = String::new();
    let mut depth = 0i32;
    for character in expr.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth -= 1;
                current.push(character);
            }
            '+' | '-' if depth == 0 => {
                if !current.trim().is_empty() {
                    terms.push((sign, std::mem::take(&mut current)));
                }
                sign = if character == '-' { -1.0 } else { 1.0 };
            }
            _ => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        terms.push((sign, current));
    }
    if terms.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for (term_sign, term) in terms {
        total += term_sign * eval_contextual_product(term.trim(), context)?;
    }
    Some(total)
}

fn eval_contextual_product(term: &str, context: &LengthContext) -> Option<f32> {
    let mut result: Option<f32> = None;
    let mut operator = '*';
    let mut depth = 0i32;
    let mut current = String::new();
    let mut factors: Vec<(char, String)> = Vec::new();
    for character in term.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth -= 1;
                current.push(character);
            }
            '*' | '/' if depth == 0 => {
                if current.trim().is_empty() {
                    return None;
                }
                factors.push((operator, std::mem::take(&mut current)));
                operator = character;
            }
            _ => current.push(character),
        }
    }
    if current.trim().is_empty() {
        return None;
    }
    factors.push((operator, current));
    for (operator, factor) in &factors {
        let value = resolve_contextual(factor, context)?;
        result = Some(match result {
            None => value,
            Some(previous) if *operator == '/' => previous / value,
            Some(previous) => previous * value,
        });
    }
    result
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
    if let Some(rest) = v.strip_prefix('(') {
        let end = find_matching_paren(rest)?;
        if end + 2 == v.len() {
            return eval_calc(&rest[..end]);
        }
    }
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
        let mut values = args.iter().filter_map(|a| eval_calc(a));
        let mut best = values.next()?;
        for val in values {
            if (is_max && val > best) || (!is_max && val < best) {
                best = val;
            }
        }
        return Some(best);
    }
    if let Some(rest) = v.strip_prefix("clamp(") {
        // clamp(min, preferred, max) == max(min, min(preferred, max)). Widely
        // used for responsive widths/font-sizes/gaps; returning None here made
        // any `width: clamp(...)` element collapse (svelte.dev's hero grid).
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        if args.len() == 3 {
            let lo = eval_calc(args[0].trim())?;
            let mid = eval_calc(args[1].trim())?;
            let hi = eval_calc(args[2].trim())?;
            return Some(mid.min(hi).max(lo));
        }
    }
    if let Some(rest) = v.strip_prefix("round(") {
        let end = find_matching_paren(rest)?;
        let args = split_top_level(&rest[..end], ',');
        let (strategy, value_index) = match args.first()?.trim() {
            "nearest" | "up" | "down" | "to-zero" => (args[0].trim(), 1),
            _ => ("nearest", 0),
        };
        let resolved = eval_calc(args.get(value_index)?.trim())?;
        let step = match args.get(value_index + 1) {
            Some(step) => eval_calc(step.trim())?,
            None => 1.0,
        };
        return round_css_value(resolved, step, strategy);
    }
    if v.contains('(') {
        return None; // an unhandled function (env(), ...): no safe fallback
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
    let mut factors: Vec<(char, String)> = Vec::new();
    for c in term.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            '*' | '/' if depth == 0 => {
                if cur.trim().is_empty() {
                    return None;
                }
                factors.push((op, std::mem::take(&mut cur)));
                op = c;
            }
            _ => cur.push(c),
        }
    }
    if cur.trim().is_empty() {
        return None;
    }
    factors.push((op, cur));

    for (op, tok) in &factors {
        let v = resolve_length(tok)?;
        result = Some(match result {
            None => v,
            Some(r) if *op == '/' => r / v,
            Some(r) => r * v,
        });
    }
    result
}

/// Recognize a `list-style-type` / `list-style` keyword. Returns `None` for
/// tokens that are not list-style types (positions like `inside`, `url(...)`,
/// or unknown type names), so a shorthand scan can skip them.
fn list_style_keyword(tok: &str) -> Option<crate::ListStyle> {
    match tok.trim() {
        "none" => Some(crate::ListStyle::None),
        "disc" => Some(crate::ListStyle::Disc),
        "circle" => Some(crate::ListStyle::Circle),
        "square" => Some(crate::ListStyle::Square),
        "decimal" | "decimal-leading-zero" => Some(crate::ListStyle::Decimal),
        _ => None,
    }
}

/// An inset component (top/right/bottom/left). `auto` and absent both become
/// `None`; everything else keeps its (possibly relative) dimension for the
/// resolution pass.
fn inset_dim(value: &str) -> Option<crate::Dimension> {
    match dimension_value(value) {
        crate::Dimension::Auto => None,
        d => Some(d),
    }
}

fn set_inset_side(style: &mut LayoutStyle, index: usize, value: &str) {
    let value = value.trim();
    if let Some(expression) = deferred_length_expression(value) {
        style.inset[index] = None;
        style.inset_expressions[index] = Some(expression);
    } else {
        style.inset[index] = inset_dim(value);
        style.inset_expressions[index] = None;
    }
}

/// Absolute keyword font-sizes (the `medium`-anchored scale), for the handful
/// of pages that still use them.
fn font_size_keyword(v: &str) -> Option<f32> {
    Some(match v.to_ascii_lowercase().as_str() {
        "xx-small" => 9.6,
        "x-small" => 12.0,
        "small" => 13.3,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        _ => return None,
    })
}

fn apply_font_size(style: &mut LayoutStyle, value: &str) {
    if value.trim().contains('(') {
        style.font_size = None;
        style.font_size_raw = None;
        style.font_size_expression = Some(value.trim().to_string());
        return;
    }
    style.font_size_expression = None;
    match dimension_value(value) {
        crate::Dimension::Px(p) => {
            style.font_size = Some(p);
            style.font_size_raw = None;
        }
        crate::Dimension::Auto => {
            // Keyword sizes (medium/small/large/...) or unknown; map the
            // common ones, else leave to inherit.
            if let Some(px) = font_size_keyword(value.trim()) {
                style.font_size = Some(px);
                style.font_size_raw = None;
            }
        }
        rel => {
            style.font_size = None;
            style.font_size_raw = Some(rel);
        }
    }
}

fn apply_gap_value(style: &mut LayoutStyle, row: bool, value: &str) {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let contextual = lower.contains('(')
        || lower.ends_with('%')
        || ["rem", "em", "ex", "vw", "vh", "vmin", "vmax"]
            .iter()
            .any(|unit| lower.ends_with(unit));
    let expression = if value.eq_ignore_ascii_case("normal")
        || value.is_empty()
        || !contextual
    {
        None
    } else {
        Some(value.to_string())
    };
    let immediate = if value.eq_ignore_ascii_case("normal") || value.is_empty() {
        None
    } else {
        px(value)
    };
    if row {
        style.row_gap = immediate;
        style.row_gap_expression = expression;
    } else {
        style.column_gap = immediate;
        style.column_gap_expression = expression;
    }
}

pub(crate) fn line_height_expression_is_length(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains('%')
        || ["px", "pt", "pc", "in", "cm", "mm", "rem", "em", "ex", "vw", "vh", "vmin", "vmax"]
            .iter()
            .any(|unit| lower.contains(unit))
}

/// Parse the layout-relevant portion of the CSS `font` shorthand:
/// `[style || variant || weight || stretch]? size [/ line-height]? family`.
///
/// As in Gecko's shorthand expansion, omitted longhands reset to their
/// initial values instead of inheriting a previously cascaded declaration.
/// We do not model variant/stretch, but still accept their keywords before
/// the required size so modern design-system declarations reach the size,
/// line-height, weight, style, and family fields that affect our layout.
fn apply_font_shorthand(style: &mut LayoutStyle, value: &str) {
    let tokens = split_ws_paren(value);
    let Some((size_index, size, attached_line_height)) =
        tokens.iter().enumerate().find_map(|(index, token)| {
            let (candidate, line_height) = token.split_once('/').unwrap_or((token, ""));
            is_font_size_token(candidate).then_some((index, candidate, line_height))
        })
    else {
        return;
    };

    let mut family_index = size_index + 1;
    let mut line_height = (!attached_line_height.is_empty()).then_some(attached_line_height);
    if line_height.is_none() && family_index < tokens.len() {
        if tokens[family_index] == "/" {
            family_index += 1;
            if family_index < tokens.len() {
                line_height = Some(tokens[family_index]);
                family_index += 1;
            }
        } else if let Some(after_slash) = tokens[family_index].strip_prefix('/') {
            if !after_slash.is_empty() {
                line_height = Some(after_slash);
            }
            family_index += 1;
        }
    }
    if family_index >= tokens.len() {
        return;
    }

    // The shorthand resets every constituent before applying supplied values.
    style.font_style_italic = Some(false);
    style.font_weight = Some("400".to_string());
    style.line_height = Some(crate::LineHeight::Normal);
    style.line_height_expression = None;
    for token in &tokens[..size_index] {
        let lower = token.to_ascii_lowercase();
        if lower == "italic" || lower.starts_with("oblique") {
            style.font_style_italic = Some(true);
        } else if let Some(weight) = specified_font_weight(&lower) {
            style.font_weight = Some(weight);
        }
    }
    apply_font_size(style, size);
    if let Some(line_height) = line_height {
        apply_value(style, "line-height", line_height);
    }
    style.font_family = Some(tokens[family_index..].join(" ").to_ascii_lowercase());
}

/// Normalize a specified CSS font weight while preserving relative keywords
/// until the top-down inheritance pass can see the parent's computed weight.
fn specified_font_weight(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "normal" => Some("400".to_string()),
        "bold" => Some("700".to_string()),
        "bolder" | "lighter" => Some(lower),
        _ => lower
            .parse::<f32>()
            .ok()
            .filter(|weight| weight.is_finite() && (1.0..=1000.0).contains(weight))
            .map(|weight| weight.round().to_string()),
    }
}

/// Resolve `font-weight` to the numeric computed value defined by CSS Fonts.
/// Relative keywords use the inherited weight table rather than a binary
/// normal/bold threshold.
pub(crate) fn computed_font_weight(specified: Option<&str>, inherited: u16) -> u16 {
    match specified {
        None => inherited,
        Some("normal") => 400,
        Some("bold") => 700,
        Some("bolder") if inherited < 100 => 400,
        Some("bolder") if inherited < 350 => 400,
        Some("bolder") if inherited < 550 => 700,
        Some("bolder") if inherited < 900 => 900,
        Some("bolder") => inherited,
        Some("lighter") if inherited < 100 => inherited,
        Some("lighter") if inherited < 350 => 100,
        Some("lighter") if inherited < 550 => 100,
        Some("lighter") if inherited < 750 => 400,
        Some("lighter") if inherited < 900 => 700,
        Some("lighter") => 700,
        Some(weight) => weight
            .parse::<f32>()
            .ok()
            .filter(|weight| weight.is_finite())
            .map(|weight| weight.round().clamp(1.0, 1000.0) as u16)
            .unwrap_or(inherited),
    }
}

pub(crate) fn used_font_weight(style: &LayoutStyle) -> u16 {
    computed_font_weight(style.font_weight.as_deref(), 400)
}

fn is_font_size_token(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "0" || font_size_keyword(&lower).is_some() {
        return true;
    }
    if lower.starts_with("calc(")
        || lower.starts_with("min(")
        || lower.starts_with("max(")
        || lower.starts_with("clamp(")
    {
        return true;
    }
    [
        "px", "pt", "em", "ex", "rem", "vw", "vh", "dvw", "dvh", "svw",
        "svh", "lvw", "lvh", "vmin", "vmax", "%",
    ]
    .iter()
    .any(|unit| lower.strip_suffix(unit).and_then(|number| number.parse::<f32>().ok()).is_some())
}

fn dimension_value(tok: &str) -> crate::Dimension {
    use crate::Dimension;
    let n = tok.trim();
    if n.eq_ignore_ascii_case("auto") || n.is_empty() {
        return Dimension::Auto;
    }
    // calc()/min()/max()/var(): resolve context-free to px where possible
    // (relative units inside are approximated; rare for these properties).
    if n.contains('(') {
        return px(n).map(Dimension::Px).unwrap_or(Dimension::Auto);
    }
    let lower = n.to_ascii_lowercase();
    let parse = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(v) = lower.strip_suffix('%').and_then(parse) {
        return Dimension::Percent(v / 100.0);
    }
    // Order matters: check `rem` before `em`, `vmin`/`vmax` before `vw`/`vh`.
    if let Some(v) = lower.strip_suffix("rem").and_then(parse) { return Dimension::Rem(v); }
    if let Some(v) = lower.strip_suffix("em").and_then(parse) { return Dimension::Em(v); }
    if let Some(v) = lower.strip_suffix("ex").and_then(parse) { return Dimension::Ex(v); }
    if let Some(v) = lower.strip_suffix("vmin").and_then(parse) { return Dimension::Vmin(v); }
    if let Some(v) = lower.strip_suffix("vmax").and_then(parse) { return Dimension::Vmax(v); }
    if let Some(v) = lower
        .strip_suffix("dvw")
        .or_else(|| lower.strip_suffix("svw"))
        .or_else(|| lower.strip_suffix("lvw"))
        .and_then(parse)
    {
        return Dimension::Vw(v);
    }
    if let Some(v) = lower
        .strip_suffix("dvh")
        .or_else(|| lower.strip_suffix("svh"))
        .or_else(|| lower.strip_suffix("lvh"))
        .and_then(parse)
    {
        return Dimension::Vh(v);
    }
    if let Some(v) = lower.strip_suffix("vw").and_then(parse) { return Dimension::Vw(v); }
    if let Some(v) = lower.strip_suffix("vh").and_then(parse) { return Dimension::Vh(v); }
    if let Some(v) = lower.strip_suffix("px").and_then(parse) { return Dimension::Px(v); }
    if let Some(v) = lower.strip_suffix("pt").and_then(parse) { return Dimension::Px(v * 1.333); }
    // CSS lengths accept a unitless number only when it is zero. Treating
    // arbitrary numbers as pixels changes invalid declarations into tiny
    // geometry (for example `font-size:.813` must be ignored and inherited,
    // not rendered at 0.813px).
    if let Some(v) = parse(&lower).filter(|v| *v == 0.0) {
        return Dimension::Px(v);
    }
    Dimension::Auto
}

/// Parse every length token in a value as px (for box shorthands).
fn edges(value: &str) -> Option<Edges> {
    let dims: Vec<f32> = value.split_whitespace().filter_map(px_value).collect();
    edges_from(dims)
}

/// Split a 1-or-2 value shorthand into (start, end); a single value applies to
/// both. Used by the logical-property axes (`margin-inline`, `padding-block`).
fn two(value: &str) -> (&str, &str) {
    let mut it = value.split_whitespace();
    let a = it.next().unwrap_or("0");
    let b = it.next().unwrap_or(a);
    (a, b)
}

/// Set one margin side (0=top,1=right,2=bottom,3=left), tracking `auto` and, for
/// a percentage, deferring resolution to the containing-block width in the
/// top-down pass (recorded in `margin_percent`).
fn set_margin_side(style: &mut LayoutStyle, idx: usize, value: &str) {
    let v = value.trim();
    let is_auto = v.eq_ignore_ascii_case("auto");
    if let Some(expression) = deferred_length_expression(v) {
        style.margin_expressions[idx] = Some(expression);
        style.margin_percent[idx] = None;
        style.margin_relative[idx] = None;
        style.margin_auto[idx] = false;
        set_margin_px(&mut style.margin, idx, 0.0);
        return;
    }
    style.margin_expressions[idx] = None;
    if let Some(frac) = percent_fraction(v) {
        style.margin_percent[idx] = Some(frac);
        style.margin_relative[idx] = None;
        set_margin_px(&mut style.margin, idx, 0.0);
        style.margin_auto[idx] = false;
        return;
    }
    let dimension = dimension_value(v);
    match dimension {
        crate::Dimension::Px(px) => {
            set_margin_px(&mut style.margin, idx, px);
            style.margin_relative[idx] = None;
        }
        crate::Dimension::Em(_)
        | crate::Dimension::Ex(_)
        | crate::Dimension::Rem(_)
        | crate::Dimension::Vw(_)
        | crate::Dimension::Vh(_)
        | crate::Dimension::Vmin(_)
        | crate::Dimension::Vmax(_) => {
            set_margin_px(&mut style.margin, idx, 0.0);
            style.margin_relative[idx] = Some(dimension);
        }
        _ => {
            set_margin_px(&mut style.margin, idx, 0.0);
            style.margin_relative[idx] = None;
        }
    }
    style.margin_auto[idx] = is_auto;
    style.margin_percent[idx] = None;
}

fn set_margin_px(margin: &mut Edges, idx: usize, px: f32) {
    match idx {
        0 => margin.top = px,
        1 => margin.right = px,
        2 => margin.bottom = px,
        3 => margin.left = px,
        _ => {}
    }
}

/// Set one padding side (0=top,1=right,2=bottom,3=left). A percentage is
/// recorded in `padding_percent` and resolved against the containing-block
/// width during the top-down pass; a length is stored directly.
fn set_padding_side(style: &mut LayoutStyle, idx: usize, value: &str) {
    let value = value.trim();
    if let Some(expression) = deferred_length_expression(value) {
        style.padding_expressions[idx] = Some(expression);
        style.padding_percent[idx] = None;
        style.padding_relative[idx] = None;
        set_padding_px(&mut style.padding, idx, 0.0);
        return;
    }
    style.padding_expressions[idx] = None;
    if let Some(frac) = percent_fraction(value) {
        style.padding_percent[idx] = Some(frac);
        style.padding_relative[idx] = None;
        set_padding_px(&mut style.padding, idx, 0.0);
        return;
    }
    let dimension = dimension_value(value);
    match dimension {
        crate::Dimension::Px(px) => {
            set_padding_px(&mut style.padding, idx, px);
            style.padding_relative[idx] = None;
            style.padding_percent[idx] = None;
        }
        crate::Dimension::Em(_)
        | crate::Dimension::Ex(_)
        | crate::Dimension::Rem(_)
        | crate::Dimension::Vw(_)
        | crate::Dimension::Vh(_)
        | crate::Dimension::Vmin(_)
        | crate::Dimension::Vmax(_) => {
            set_padding_px(&mut style.padding, idx, 0.0);
            style.padding_relative[idx] = Some(dimension);
            style.padding_percent[idx] = None;
        }
        _ => {}
    }
}

fn set_padding_px(padding: &mut Edges, idx: usize, px: f32) {
    match idx {
        0 => padding.top = px,
        1 => padding.right = px,
        2 => padding.bottom = px,
        3 => padding.left = px,
        _ => {}
    }
}

/// `padding: <t> <r>? <b>? <l>?`, percentage-aware per side.
fn apply_padding_shorthand(style: &mut LayoutStyle, value: &str) {
    let toks: Vec<&str> = value.split_whitespace().collect();
    let (t, r, b, l) = match toks.as_slice() {
        [a] => (*a, *a, *a, *a),
        [v, h] => (*v, *h, *v, *h),
        [t, h, b] => (*t, *h, *b, *h),
        [t, r, b, l, ..] => (*t, *r, *b, *l),
        [] => return,
    };
    set_padding_side(style, 0, t);
    set_padding_side(style, 1, r);
    set_padding_side(style, 2, b);
    set_padding_side(style, 3, l);
}

/// A bare `<number>%` token as a 0..1 fraction (`56.25%` -> `0.5625`). Returns
/// `None` for anything that is not a plain percentage (lengths, `calc(...%)`,
/// keywords), so those keep their existing length handling.
fn percent_fraction(tok: &str) -> Option<f32> {
    let num = tok.trim().strip_suffix('%')?;
    let v: f32 = num.trim().parse().ok()?;
    if v.is_finite() {
        Some(v / 100.0)
    } else {
        None
    }
}

/// `margin: <t> <r>? <b>? <l>?` with per-side `auto` (so `margin: 0 auto`
/// centers).
fn apply_margin_shorthand(style: &mut LayoutStyle, value: &str) {
    let toks: Vec<&str> = value.split_whitespace().collect();
    let (t, r, b, l) = match toks.as_slice() {
        [a] => (*a, *a, *a, *a),
        [v, h] => (*v, *h, *v, *h),
        [t, h, b] => (*t, *h, *b, *h),
        [t, r, b, l, ..] => (*t, *r, *b, *l),
        [] => return,
    };
    set_margin_side(style, 0, t);
    set_margin_side(style, 1, r);
    set_margin_side(style, 2, b);
    set_margin_side(style, 3, l);
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
    } else if n.ends_with("ex") {
        n = n.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        scale = 16.0 * 0.528_320_3;
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

/// Parse a `linear-gradient(...)` (also `repeating-`/`-webkit-`/`-moz-`) into
/// (angle-degrees, color stops). Angle is CSS convention (0deg = to top, grows
/// clockwise); `to <side>` keywords map to their angle. Color stops keep their
/// optional 0..1 position. Returns None if it is not a linear-gradient or has
/// no parseable colors. Radial/conic gradients are not handled (None).
fn parse_linear_gradient(value: &str) -> Option<(f32, Vec<([u8; 4], Option<f32>)>)> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let start = lower.find("linear-gradient(")?;
    // The original prefixed WebKit gradient syntax predates the standardized
    // angle system: 0deg points right and positive angles turn
    // counter-clockwise. Blink still accepts that syntax for compatibility.
    // Convert it to our standard CSS angle (0deg up, clockwise) before paint.
    let prefix = &lower[..start];
    let legacy_webkit_angle =
        prefix.ends_with("-webkit-") || prefix.ends_with("-webkit-repeating-");
    let open = start + "linear-gradient(".len();
    // Match the closing paren for this function.
    let bytes = v.as_bytes();
    let mut depth = 1;
    let mut end = open;
    while end < bytes.len() && depth > 0 {
        match bytes[end] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        end += 1;
    }
    let inner = &v[open..end.saturating_sub(1)];
    // Split on top-level commas (respect rgb()/rgba()/hsl() parens).
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut d = 0i32;
    for c in inner.chars() {
        match c {
            '(' => { d += 1; cur.push(c); }
            ')' => { d -= 1; cur.push(c); }
            ',' if d == 0 => { parts.push(std::mem::take(&mut cur)); }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    if parts.is_empty() {
        return None;
    }
    // Leading angle / direction, if present.
    let mut angle = 180.0f32; // default: to bottom
    let first = parts[0].trim().to_ascii_lowercase();
    let mut stop_start = 0;
    if first.ends_with("deg") {
        if let Ok(a) = first.trim_end_matches("deg").trim().parse::<f32>() {
            angle = if legacy_webkit_angle {
                (90.0 - a).rem_euclid(360.0)
            } else {
                a.rem_euclid(360.0)
            };
        }
        stop_start = 1;
    } else if first.starts_with("to ") {
        angle = match first.as_str() {
            "to top" => 0.0, "to right" => 90.0, "to bottom" => 180.0, "to left" => 270.0,
            "to top right" | "to right top" => 45.0, "to bottom right" | "to right bottom" => 135.0,
            "to bottom left" | "to left bottom" => 225.0, "to top left" | "to left top" => 315.0,
            _ => 180.0,
        };
        stop_start = 1;
    } else if first.starts_with("turn") || first.ends_with("turn") {
        stop_start = 1;
    }
    let mut stops: Vec<([u8; 4], Option<f32>)> = Vec::new();
    for p in &parts[stop_start..] {
        let t = p.trim();
        if t.is_empty() {
            continue;
        }
        // "color [pos%]" - the color may itself contain spaces (rgb( ... )) so
        // split off a trailing percentage token if present.
        let (color_str, pos) = if let Some(idx) = t.rfind(char::is_whitespace) {
            let tail = t[idx + 1..].trim();
            if let Some(pct) = tail.strip_suffix('%').and_then(|s| s.parse::<f32>().ok()) {
                (t[..idx].trim(), Some((pct / 100.0).clamp(0.0, 1.0)))
            } else {
                (t, None)
            }
        } else {
            (t, None)
        };
        if let Some(c) = parse_color(color_str) {
            stops.push((c, pos));
        }
    }
    if stops.len() < 2 {
        // A single-color "gradient" is just that color; let the caller fall back
        // to background_color instead (return None so parse_color runs).
        return None;
    }
    Some((angle, stops))
}

fn parse_radial_gradient(
    value: &str,
) -> Option<((f32, f32), Vec<([u8; 4], Option<f32>)>)> {
    let lower = value.to_ascii_lowercase();
    let start = lower.find("radial-gradient(")?;
    let open = start + "radial-gradient(".len();
    let end = find_matching_paren(&value[open..])? + open;
    let parts = split_top_level(&value[open..end], ',');
    if parts.is_empty() {
        return None;
    }
    let mut center = (0.5, 0.5);
    let mut stop_start = 0;
    let prelude = parts[0].trim().to_ascii_lowercase();
    if prelude.contains(" at ") || prelude.starts_with("at ") {
        let coords = prelude
            .split_once(" at ")
            .map(|(_, coords)| coords)
            .or_else(|| prelude.strip_prefix("at "))
            .unwrap_or_default();
        let mut coords = coords.split_whitespace();
        if let Some(x) = coords.next().and_then(percent_fraction) {
            center.0 = x;
        }
        if let Some(y) = coords.next().and_then(percent_fraction) {
            center.1 = y;
        }
        stop_start = 1;
    } else if parse_color(parts[0].trim()).is_none() {
        stop_start = 1;
    }
    let mut stops = Vec::new();
    for part in &parts[stop_start..] {
        let (color, position) = split_color_stop(part.trim());
        if let Some(color) = parse_color(color) {
            stops.push((color, position));
        }
    }
    (stops.len() >= 2).then_some((center, stops))
}

/// Parse the common `conic-gradient(from A at X Y, color P%, ...)` form.
/// Angles follow CSS convention (0deg at 12 o'clock, clockwise); the center
/// is retained as box-relative fractions for paint-time resolution.
fn parse_conic_gradient(
    value: &str,
) -> Option<(f32, (f32, f32), Vec<([u8; 4], Option<f32>)>)> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let start = lower.find("conic-gradient(")?;
    let open = start + "conic-gradient(".len();
    let end = find_matching_paren(&v[open..])? + open;
    let inner = &v[open..end];
    let parts = split_top_level(inner, ',');
    if parts.is_empty() {
        return None;
    }

    let mut angle = 0.0f32;
    let mut center = (0.5f32, 0.5f32);
    let mut stop_start = 0usize;
    let prelude = parts[0].trim().to_ascii_lowercase();
    if prelude.starts_with("from ") || prelude.starts_with("at ") {
        if let Some(from) = prelude.find("from ") {
            let token = prelude[from + 5..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            angle = parse_css_angle(token).unwrap_or(0.0).rem_euclid(360.0);
        }
        if let Some(at) = prelude.find(" at ") {
            let coords: Vec<&str> = prelude[at + 4..].split_whitespace().collect();
            if let Some(x) = coords.first().and_then(|value| percent_fraction(value)) {
                center.0 = x;
            }
            if let Some(y) = coords.get(1).and_then(|value| percent_fraction(value)) {
                center.1 = y;
            }
        } else if let Some(at) = prelude.strip_prefix("at ") {
            let coords: Vec<&str> = at.split_whitespace().collect();
            if let Some(x) = coords.first().and_then(|value| percent_fraction(value)) {
                center.0 = x;
            }
            if let Some(y) = coords.get(1).and_then(|value| percent_fraction(value)) {
                center.1 = y;
            }
        }
        stop_start = 1;
    }

    let mut stops = Vec::new();
    for part in &parts[stop_start..] {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (color, position) = split_color_stop(part);
        if let Some(color) = parse_color(color) {
            stops.push((color, position));
        }
    }
    (stops.len() >= 2).then_some((angle, center, stops))
}

fn parse_css_angle(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(degrees) = value.strip_suffix("deg") {
        return degrees.trim().parse::<f32>().ok();
    }
    if let Some(turns) = value.strip_suffix("turn") {
        return turns.trim().parse::<f32>().ok().map(|turns| turns * 360.0);
    }
    if let Some(gradians) = value.strip_suffix("grad") {
        return gradians.trim().parse::<f32>().ok().map(|grad| grad * 0.9);
    }
    if let Some(radians) = value.strip_suffix("rad") {
        return radians
            .trim()
            .parse::<f32>()
            .ok()
            .map(f32::to_degrees);
    }
    None
}

fn split_color_stop(value: &str) -> (&str, Option<f32>) {
    if let Some(idx) = value.rfind(char::is_whitespace) {
        let tail = value[idx + 1..].trim();
        if let Some(percent) = tail
            .strip_suffix('%')
            .and_then(|number| number.parse::<f32>().ok())
        {
            return (
                value[..idx].trim(),
                Some((percent / 100.0).clamp(0.0, 1.0)),
            );
        }
        if let Some(degrees) = tail
            .strip_suffix("deg")
            .and_then(|number| number.parse::<f32>().ok())
        {
            return (
                value[..idx].trim(),
                Some((degrees / 360.0).clamp(0.0, 1.0)),
            );
        }
    }
    (value, None)
}

/// Parse `aspect-ratio` to a width/height ratio. Accepts `16 / 9`, `1.5`, and
/// the `auto <ratio>` form (the `auto` keyword alone yields `None`, meaning the
/// intrinsic ratio, which for images is filled in at layout).
fn parse_aspect_ratio(value: &str) -> Option<f32> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("auto") {
        return None;
    }
    // Drop a leading/trailing `auto` keyword from the `auto <ratio>` form.
    let ratio_part: String = v
        .split_whitespace()
        .filter(|t| !t.eq_ignore_ascii_case("auto"))
        .collect::<Vec<_>>()
        .join(" ");
    if let Some((w, h)) = ratio_part.split_once('/') {
        let w: f32 = w.trim().parse().ok()?;
        let h: f32 = h.trim().parse().ok()?;
        if h > 0.0 && w > 0.0 {
            return Some(w / h);
        }
        return None;
    }
    let r: f32 = ratio_part.trim().parse().ok()?;
    (r.is_finite() && r > 0.0).then_some(r)
}

/// Extract the first `url(...)` reference from a `background`/`background-image`
/// value, unquoted. Ignores any other layers in the same shorthand (gradients,
/// `no-repeat`, etc.): we paint the referenced image, not the gradient.
fn parse_url(value: &str) -> Option<String> {
    let start = value.find("url(")? + 4;
    let mut depth = 1i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut end = None;
    for (offset, character) in value[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
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
                if depth == 0 {
                    end = Some(start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = value[start..end?].trim();
    let unquoted = inner.trim_matches(|c| c == '"' || c == '\'');
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// `flex: none|auto|<grow>|<grow> <shrink>|<grow> <shrink> <basis>`. This is
/// the shorthand form almost all real-world flexbox CSS actually uses (far
/// more often than the flex-grow/flex-shrink longhands); leaving it
/// unhandled silently drops grow/shrink from every rule written this way.
/// flex-basis is not modeled as a distinct field from width, so a basis
/// length in the shorthand is parsed (to keep the number-only forms working)
/// but otherwise not separately applied; auto is a reasonable approximation
/// for the common case where basis is 0 or unspecified.
fn parse_flex_shorthand(style: &mut LayoutStyle, value: &str) {
    match value.trim() {
        "none" => {
            style.flex_grow = Some(0.0);
            style.flex_shrink = Some(0.0);
            style.flex_basis = crate::Dimension::Auto;
            return;
        }
        "auto" => {
            style.flex_grow = Some(1.0);
            style.flex_shrink = Some(1.0);
            style.flex_basis = crate::Dimension::Auto;
            return;
        }
        "initial" => {
            style.flex_grow = Some(0.0);
            style.flex_shrink = Some(1.0);
            style.flex_basis = crate::Dimension::Auto;
            return;
        }
        _ => {}
    }
    // Grammar: `flex: <grow> <shrink>? || <basis>`. Bare numbers are grow then
    // shrink; a token with a unit / `auto` / a third numeric is the basis
    // (e.g. `flex: 0 0 260px`, the fixed-width sidebar idiom).
    let mut numbers: Vec<f32> = Vec::new();
    let mut basis: Option<crate::Dimension> = None;
    for tok in value.split_whitespace() {
        if let Ok(n) = tok.parse::<f32>() {
            if numbers.len() < 2 {
                numbers.push(n);
            } else {
                basis = Some(dimension_value(tok));
            }
        } else {
            basis = Some(dimension_value(tok));
        }
    }
    match numbers.as_slice() {
        [grow] => {
            style.flex_grow = Some(*grow);
            style.flex_shrink = Some(1.0);
        }
        [grow, shrink, ..] => {
            style.flex_grow = Some(*grow);
            style.flex_shrink = Some(*shrink);
        }
        [] => {}
    }
    // Explicit basis wins; otherwise numbers-only shorthand implies basis 0
    // (per spec `flex: 1` == `1 1 0%`), while a bare basis keeps grow/shrink 1.
    style.flex_basis = match basis {
        Some(b) => b,
        None if !numbers.is_empty() => crate::Dimension::Px(0.0),
        None => {
            style.flex_grow = Some(1.0);
            style.flex_shrink = Some(1.0);
            crate::Dimension::Auto
        }
    };
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

fn parse_background_size_fit(value: &str) -> Option<crate::ObjectFit> {
    let size = value.rsplit_once('/').map_or(value, |(_, size)| size);
    if size.split_whitespace().any(|token| token == "cover") {
        Some(crate::ObjectFit::Cover)
    } else if size.split_whitespace().any(|token| token == "contain") {
        Some(crate::ObjectFit::Contain)
    } else {
        None
    }
}

fn background_size_expression(value: &str) -> Option<String> {
    let (_, size) = value.rsplit_once('/')?;
    let mut depth = 0i32;
    let mut end = size.len();
    for (index, ch) in size.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            _ if depth == 0 && ch == ',' => {
                end = index;
                break;
            }
            _ => {}
        }
    }
    let size = size[..end].trim();
    (!size.is_empty()).then(|| size.to_string())
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

/// Parse a `box-shadow` value into its first layer:
/// `[inset]? <offset-x> <offset-y> <blur>? <spread>? <color>?`. The `inset`
/// keyword and the color may each lead or trail the lengths; comma-separated
/// multiples are accepted but only the first layer is stored. `current_color`
/// supplies the default when the color is omitted (CSS `currentColor`).
fn parse_box_shadow(value: &str, current_color: Option<[u8; 4]>) -> Option<crate::BoxShadow> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return None;
    }
    // Only the first comma-separated layer is modeled; split at paren depth 0 so
    // the commas inside an rgba()/hsl() color are not treated as separators.
    let layer = split_top_level(v, ',').into_iter().next()?;
    let mut inset = false;
    let mut color: Option<[u8; 4]> = None;
    let mut lengths: Vec<f32> = Vec::new();
    for tok in split_ws_paren(layer.trim()) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if t.eq_ignore_ascii_case("inset") {
            inset = true;
            continue;
        }
        // Try a length first so a bare `0` is an offset, not a failed color;
        // px_value rejects color tokens (`#ccc`, `red`, `rgba(...)`) so they
        // fall through to parse_color.
        if lengths.len() < 4 {
            if let Some(px) = px_value(t) {
                lengths.push(px);
                continue;
            }
        }
        if let Some(c) = parse_color(t) {
            color = Some(c);
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    Some(crate::BoxShadow {
        offset_x: lengths[0],
        offset_y: lengths[1],
        blur: lengths.get(2).copied().unwrap_or(0.0),
        spread: lengths.get(3).copied().unwrap_or(0.0),
        color: color.or(current_color).unwrap_or([0, 0, 0, 255]),
        inset,
    })
}

/// Split on ASCII whitespace at paren depth 0, so a functional color like
/// `rgba(0, 0, 0, .15)` (whose internal spaces are not token separators) stays
/// one token.
fn split_ws_paren(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() && depth == 0 {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
            continue;
        }
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            _ => {}
        }
        if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
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

        let flow_root = compute_style("div", Some("display: flow-root"));
        assert_eq!(flow_root.display, Display::Block);
        assert!(flow_root.flow_root);
    }

    #[test]
    fn animation_shorthand_preserves_settled_forward_fill_contract() {
        let finite = compute_style(
            "div",
            Some("animation: dismiss-overlay .6s ease-out forwards"),
        );
        assert_eq!(finite.animation_name.as_deref(), Some("dismiss-overlay"));
        assert!(finite.animation_fill_forwards);
        assert!(!finite.animation_iteration_infinite);

        let infinite = compute_style(
            "div",
            Some("animation: pulse 1s linear infinite"),
        );
        assert_eq!(infinite.animation_name.as_deref(), Some("pulse"));
        assert!(!infinite.animation_fill_forwards);
        assert!(infinite.animation_iteration_infinite);
    }

    #[test]
    fn display_contents_overrides_an_earlier_display_none() {
        let style = compute_style("div", Some("display:none; display:contents"));
        assert_eq!(style.display, Display::Block);
        assert!(style.display_contents);
    }

    #[test]
    fn authored_display_replaces_internal_flex_provenance() {
        let native_cell = compute_style("td", None);
        assert_eq!(native_cell.display, Display::Flex);
        assert!(native_cell.internal_flex_container);

        let authored_cell = compute_style("td", Some("display:flex"));
        assert_eq!(authored_cell.display, Display::Flex);
        assert!(!authored_cell.internal_flex_container);

        let invalid = compute_style("td", Some("display:bogus"));
        assert!(invalid.internal_flex_container);

        let native_image = compute_style("img", None);
        assert_eq!(native_image.display, Display::Inline);
        assert!(native_image.is_inline_block);

        let block_image = compute_style("img", Some("display:block"));
        assert_eq!(block_image.display, Display::Block);
        assert!(!block_image.is_inline_block);
    }

    #[test]
    fn table_ua_geometry_and_border_collapse_parse() {
        let table = compute_style("table", None);
        assert_eq!(table.box_sizing, crate::BoxSizing::BorderBox);
        assert_eq!(table.border_spacing, Some((2.0, 2.0)));
        assert_eq!(table.border_collapse, Some(false));

        let cell = compute_style("td", None);
        assert_eq!(
            cell.padding,
            Edges {
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
                left: 1.0,
            }
        );
        assert_eq!(cell.vertical_align, None);

        let collapsed = compute_style(
            "table",
            Some("border-spacing:8px; border-collapse:collapse"),
        );
        assert_eq!(collapsed.border_spacing, Some((8.0, 8.0)));
        assert_eq!(collapsed.border_collapse, Some(true));
    }

    #[test]
    fn border_none_clears_native_and_per_side_widths() {
        let input = compute_style("input", Some("border:none"));
        assert_eq!(input.border, Edges::default());

        let side = compute_style(
            "div",
            Some("border:3px solid red;border-left:none"),
        );
        assert_eq!(side.border.top, 3.0);
        assert_eq!(side.border.right, 3.0);
        assert_eq!(side.border.bottom, 3.0);
        assert_eq!(side.border.left, 0.0);
    }

    #[test]
    fn item_self_alignment_parses_and_resets() {
        let aligned = compute_style(
            "div",
            Some("align-self:safe center;justify-self:flex-end"),
        );
        assert_eq!(aligned.align_self, Some(taffy::AlignSelf::SAFE_CENTER));
        assert_eq!(aligned.justify_self, Some(taffy::JustifySelf::FLEX_END));

        let reset = compute_style(
            "div",
            Some("align-self:center;align-self:auto;justify-self:end;justify-self:auto"),
        );
        assert_eq!(reset.align_self, None);
        assert_eq!(reset.justify_self, None);

        let normal = compute_style("div", Some("align-self:normal;justify-self:normal"));
        assert_eq!(normal.align_self, Some(taffy::AlignSelf::STRETCH));
        assert_eq!(normal.justify_self, Some(taffy::JustifySelf::STRETCH));

        let shorthand = compute_style("div", Some("place-self:safe center flex-end"));
        assert_eq!(shorthand.align_self, Some(taffy::AlignSelf::SAFE_CENTER));
        assert_eq!(shorthand.justify_self, Some(taffy::JustifySelf::FLEX_END));

        let parent = compute_style(
            "div",
            Some("align-items:start;justify-items:safe end;place-items:end center"),
        );
        assert_eq!(parent.align_items, Some(taffy::AlignItems::END));
        assert_eq!(parent.justify_items, Some(taffy::JustifyItems::CENTER));

        let content = compute_style(
            "div",
            Some("align-content:space-between;place-content:safe center end"),
        );
        assert_eq!(content.align_content, Some(taffy::AlignContent::SAFE_CENTER));
        assert_eq!(content.justify_content, Some(taffy::JustifyContent::END));
    }

    #[test]
    fn font_shorthand_expands_layout_fields_and_resets_omissions() {
        let s = compute_style(
            "div",
            Some(
                "font-style:italic;font-weight:bold;line-height:2;\
                 font:normal small-caps 500 64px/60px \"Google Sans\", sans-serif",
            ),
        );
        assert_eq!(s.font_size, Some(64.0));
        assert_eq!(s.line_height, Some(crate::LineHeight::Px(60.0)));
        assert_eq!(s.font_weight.as_deref(), Some("500"));
        assert_eq!(s.font_style_italic, Some(false));
        assert_eq!(s.font_family.as_deref(), Some("\"google sans\", sans-serif"));

        let reset = compute_style(
            "div",
            Some("font-style:italic;font-weight:bold;line-height:2;font:20px Arial"),
        );
        assert_eq!(reset.font_size, Some(20.0));
        assert_eq!(reset.line_height, Some(crate::LineHeight::Normal));
        assert_eq!(reset.font_weight.as_deref(), Some("400"));
        assert_eq!(reset.font_style_italic, Some(false));
    }

    #[test]
    fn font_weight_preserves_numeric_values_and_resolves_relative_keywords() {
        let medium = compute_style("div", Some("font-weight:500"));
        assert_eq!(medium.font_weight.as_deref(), Some("500"));
        let semibold = compute_style("div", Some("font-weight:600"));
        assert_eq!(semibold.font_weight.as_deref(), Some("600"));
        let normal = compute_style("strong", Some("font-weight:normal"));
        assert_eq!(normal.font_weight.as_deref(), Some("400"));

        assert_eq!(computed_font_weight(Some("bolder"), 99), 400);
        assert_eq!(computed_font_weight(Some("bolder"), 349), 400);
        assert_eq!(computed_font_weight(Some("bolder"), 350), 700);
        assert_eq!(computed_font_weight(Some("bolder"), 550), 900);
        assert_eq!(computed_font_weight(Some("bolder"), 900), 900);
        assert_eq!(computed_font_weight(Some("lighter"), 99), 99);
        assert_eq!(computed_font_weight(Some("lighter"), 100), 100);
        assert_eq!(computed_font_weight(Some("lighter"), 350), 100);
        assert_eq!(computed_font_weight(Some("lighter"), 550), 400);
        assert_eq!(computed_font_weight(Some("lighter"), 750), 700);
        assert_eq!(computed_font_weight(Some("lighter"), 900), 700);
    }

    #[test]
    fn nonzero_unitless_font_size_is_invalid() {
        let inherited = compute_style("div", Some("font-size:14px;font-size:.813"));
        assert_eq!(inherited.font_size, Some(14.0));

        let zero = compute_style("div", Some("font-size:0"));
        assert_eq!(zero.font_size, Some(0.0));
    }

    #[test]
    fn containing_block_property_triggers_are_independent() {
        let s = compute_style("div", Some("transform:rotate(0deg);filter:none"));
        assert!(s.establishes_positioning_containing_block());

        let s = compute_style("div", Some("filter:blur(0);transform:none"));
        assert!(s.establishes_positioning_containing_block());

        let s = compute_style(
            "div",
            Some("contain:layout;content-visibility:visible;filter:none"),
        );
        assert!(s.establishes_positioning_containing_block());

        let s = compute_style(
            "div",
            Some("contain:none;content-visibility:visible;filter:none;perspective:none"),
        );
        assert!(!s.establishes_positioning_containing_block());
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
    fn percentage_padding_recorded_not_pixelized() {
        // A percentage padding must be deferred (recorded as a 0..1 fraction),
        // not eagerly converted to a bogus px value: it resolves against the
        // containing block width later, in the top-down pass.
        let s = compute_style("div", Some("padding-top: 56.25%"));
        assert_eq!(s.padding_percent[0], Some(0.5625));
        assert_eq!(s.padding.top, 0.0);

        // The shorthand splits per side, so a mix of length and percent lands
        // in the right buckets.
        let s = compute_style("div", Some("padding: 10px 25%"));
        assert_eq!(s.padding.top, 10.0);
        assert_eq!(s.padding_percent[0], None);
        assert_eq!(s.padding_percent[1], Some(0.25));
    }

    #[test]
    fn percentage_margin_recorded() {
        let s = compute_style("div", Some("margin-left: 10%"));
        assert_eq!(s.margin_percent[3], Some(0.1));
        assert!(!s.margin_auto[3]);
    }

    #[test]
    fn relative_box_edges_remain_unresolved() {
        let s = compute_style(
            "div",
            Some("font-size:20px;margin:15vh auto 2em 10vw;padding:1rem 2vmin"),
        );
        assert_eq!(s.margin_relative[0], Some(crate::Dimension::Vh(15.0)));
        assert!(s.margin_auto[1]);
        assert_eq!(s.margin_relative[2], Some(crate::Dimension::Em(2.0)));
        assert_eq!(s.margin_relative[3], Some(crate::Dimension::Vw(10.0)));
        assert_eq!(s.padding_relative[0], Some(crate::Dimension::Rem(1.0)));
        assert_eq!(s.padding_relative[1], Some(crate::Dimension::Vmin(2.0)));
    }

    #[test]
    fn responsive_grid_repetitions_and_shorthand_remain_typed() {
        let auto = compute_style(
            "div",
            Some("display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr))"),
        );
        assert!(matches!(
            auto.grid_template_columns.as_slice(),
            [taffy::GridTemplateComponent::Repeat(
                taffy::GridTemplateRepetition {
                    count: taffy::RepetitionCount::AutoFit,
                    ..
                }
            )]
        ));

        let shorthand = compute_style(
            "div",
            Some("display:grid;grid:auto-flow/repeat(3,1fr)"),
        );
        assert_eq!(shorthand.grid_auto_flow, Some(taffy::GridAutoFlow::Row));
        assert_eq!(shorthand.grid_template_columns.len(), 3);
    }

    #[test]
    fn grid_placement_longhands_preserve_the_opposite_side() {
        let style = compute_style(
            "div",
            Some(
                "grid-column-start:2;grid-column-end:span 4;\
                 grid-row-start:3;grid-row-end:5",
            ),
        );
        assert_eq!(
            style.grid_column,
            Some(taffy::Line {
                start: taffy::style_helpers::line(2),
                end: taffy::style_helpers::span(4),
            })
        );
        assert_eq!(
            style.grid_row,
            Some(taffy::Line {
                start: taffy::style_helpers::line(3),
                end: taffy::style_helpers::line(5),
            })
        );
    }

    #[test]
    fn contextual_css_math_uses_runtime_geometry() {
        let context = (20.0, 16.0, 9.0, 10.0, 900.0);
        assert_eq!(
            resolve_contextual_length(
                "min(25vw,350px)",
                context.0,
                context.1,
                context.2,
                context.3,
                context.4,
            ),
            Some(225.0)
        );
        assert_eq!(
            resolve_contextual_length(
                "clamp(200px,30vw,320px)",
                context.0,
                context.1,
                context.2,
                context.3,
                context.4,
            ),
            Some(270.0)
        );
        assert_eq!(
            resolve_contextual_length(
                "calc(10vw + 2rem)",
                context.0,
                context.1,
                context.2,
                context.3,
                context.4,
            ),
            Some(122.0)
        );
        assert_eq!(
            resolve_contextual_length(
                "calc(round(247px * 1, 10px))",
                context.0,
                context.1,
                context.2,
                context.3,
                context.4,
            ),
            Some(250.0)
        );
        let grouped = resolve_contextual_length(
            "calc(clamp(128px,92px + 7vw,188px) + (100vw - 48px)*43/440)",
            context.0,
            context.1,
            context.2,
            context.3,
            context.4,
        )
        .unwrap();
        assert!(
            (grouped - 238.263_64).abs() < 0.001,
            "grouped calc should preserve the nested subtraction: {grouped}"
        );
    }

    #[test]
    fn ua_defaults_and_ignore_unknown() {
        let s = compute_style("span", Some("color: red; ; bogus: ; display: none"));
        assert_eq!(s.display, Display::None);
    }

    #[test]
    fn background_clip_text_flag() {
        let s = compute_style("h1", Some("color: transparent; -webkit-background-clip: text"));
        assert!(s.background_clip_text);
        let vendor_fill = compute_style(
            "h1",
            Some(
                "color: black; -webkit-text-fill-color: transparent;\
                 background: linear-gradient(90deg, red, blue);\
                 -webkit-background-clip: text",
            ),
        );
        assert_eq!(vendor_fill.color, Some([0, 0, 0, 0]));
        assert!(vendor_fill.background_clip_text);
        assert!(vendor_fill.background_gradient.is_some());
        let legacy_angle = compute_style(
            "span",
            Some("background:-webkit-linear-gradient(315deg,#42d392 25%,#647eff)"),
        )
        .background_gradient
        .expect("prefixed gradient")
        .0;
        assert_eq!(legacy_angle, 135.0);
        let n = compute_style("h1", Some("background-clip: border-box"));
        assert!(!n.background_clip_text);
        let l = compute_style("h1", Some("background-clip: text"));
        assert!(l.background_clip_text);
    }

    #[test]
    fn background_shorthand_resets_omitted_layers() {
        let s = compute_style(
            "div",
            Some(
                "background:#1971c2 url(icon.png);background-size:20px 20px;\
                 background-position:center;background-clip:text;background:0",
            ),
        );
        assert_eq!(s.background_color, None);
        assert_eq!(s.background_gradient, None);
        assert_eq!(s.background_conic_gradient, None);
        assert_eq!(s.background_image, None);
        assert_eq!(s.background_size, None);
        assert_eq!(s.background_size_expression, None);
        assert_eq!(s.background_size_fit, None);
        assert_eq!(s.background_position, (0.0, 0.0));
        assert!(!s.background_clip_text);

        let cover = compute_style(
            "div",
            Some("background:url(hero.svg) center/cover no-repeat"),
        );
        assert_eq!(cover.background_size_fit, Some(crate::ObjectFit::Cover));
        let contain = compute_style("div", Some("background-size:contain"));
        assert_eq!(
            contain.background_size_fit,
            Some(crate::ObjectFit::Contain)
        );
        let contextual = compute_style(
            "a",
            Some(
                "background:url(icon.svg) no-repeat 0 50% / calc(100% - 2rem) auto",
            ),
        );
        assert_eq!(
            contextual.background_size_expression.as_deref(),
            Some("calc(100% - 2rem) auto")
        );
    }

    #[test]
    fn conic_background_and_repeated_data_svg_mask_are_preserved() {
        let style = compute_style(
            "div",
            Some(
                "background:conic-gradient(from 122deg at 50% 50%,\
                 transparent 17%,#f627e3 25%,#6911d2 32%,transparent 91%);\
                 mask-image:url(\"data:image/svg+xml,<svg viewBox='0 0 72 72'>\
                 <g transform='translate(36 36) rotate(-60)'></g></svg>\");\
                 mask-size:22px 22px;mask-repeat:repeat",
            ),
        );
        let (angle, center, stops) = style
            .background_conic_gradient
            .expect("conic gradient should parse");
        assert_eq!(angle, 122.0);
        assert_eq!(center, (0.5, 0.5));
        assert_eq!(stops.len(), 4);
        assert_eq!(style.mask_size, Some((22.0, 22.0)));
        assert_eq!(style.mask_repeat, Some((true, true)));
        let mask = style.mask_image.expect("data SVG mask should parse");
        assert!(mask.ends_with("</svg>"));
        assert!(mask.contains("rotate(-60)"));
    }

    #[test]
    fn important_and_auto() {
        let s = compute_style("div", Some("width: 100px !important; height: auto"));
        assert_eq!(s.width, crate::Dimension::Px(100.0));
        assert_eq!(s.height, crate::Dimension::Auto);
    }

    #[test]
    fn box_sizing_defaults_to_content_box_and_parses_both_keywords() {
        assert_eq!(
            compute_style("div", None).box_sizing,
            crate::BoxSizing::ContentBox
        );
        assert_eq!(
            compute_style("div", Some("box-sizing:border-box")).box_sizing,
            crate::BoxSizing::BorderBox
        );
        assert_eq!(
            compute_style(
                "div",
                Some("box-sizing:border-box;box-sizing:content-box")
            )
            .box_sizing,
            crate::BoxSizing::ContentBox
        );
        assert_eq!(
            compute_style("div", Some("box-sizing:inherit")).box_sizing,
            crate::BoxSizing::Inherit
        );
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

    #[test]
    fn flex_shorthand_two_numbers() {
        // The exact form Wikipedia's infobox uses for its label/value cells
        // (`.infobox tbody > tr > th/td{flex:1 0}`): without shorthand
        // support this was silently dropped, leaving both columns
        // shrink-to-fit instead of sharing the row's width.
        let s = compute_style("div", Some("flex: 1 0"));
        assert_eq!(s.flex_grow, Some(1.0));
        assert_eq!(s.flex_shrink, Some(0.0));
    }

    #[test]
    fn flex_shorthand_keywords() {
        let none = compute_style("div", Some("flex: none"));
        assert_eq!(none.flex_grow, Some(0.0));
        assert_eq!(none.flex_shrink, Some(0.0));

        let auto = compute_style("div", Some("flex: auto"));
        assert_eq!(auto.flex_grow, Some(1.0));
        assert_eq!(auto.flex_shrink, Some(1.0));
    }

    #[test]
    fn flex_shorthand_single_number_defaults_shrink_to_one() {
        let s = compute_style("div", Some("flex: 2"));
        assert_eq!(s.flex_grow, Some(2.0));
        assert_eq!(s.flex_shrink, Some(1.0));
    }

    #[test]
    fn box_shadow_outset_parses() {
        let s = compute_style("div", Some("box-shadow: 0 2px 8px rgba(0,0,0,.15)"));
        let sh = s.box_shadow.expect("box-shadow parsed");
        assert!(!sh.inset);
        assert_eq!(sh.offset_x, 0.0);
        assert_eq!(sh.offset_y, 2.0);
        assert_eq!(sh.blur, 8.0);
        assert_eq!(sh.spread, 0.0);
        assert_eq!(sh.color, [0, 0, 0, 38]);
    }

    #[test]
    fn box_shadow_inset_parses() {
        let s = compute_style("div", Some("box-shadow: inset 0 0 0 1px #ccc"));
        let sh = s.box_shadow.expect("box-shadow parsed");
        assert!(sh.inset);
        assert_eq!(sh.offset_x, 0.0);
        assert_eq!(sh.offset_y, 0.0);
        assert_eq!(sh.blur, 0.0);
        assert_eq!(sh.spread, 1.0);
        assert_eq!(sh.color, [204, 204, 204, 255]);
    }

    #[test]
    fn box_shadow_color_defaults_to_current_color() {
        // No explicit color: falls back to the element's text color.
        let s = compute_style("div", Some("color: red; box-shadow: 1px 1px 2px"));
        let sh = s.box_shadow.expect("box-shadow parsed");
        assert_eq!(sh.color, [255, 0, 0, 255]);
    }

    #[test]
    fn box_shadow_none_clears() {
        let s = compute_style("div", Some("box-shadow: none"));
        assert!(s.box_shadow.is_none());
    }
}
