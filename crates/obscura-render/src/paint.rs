//! Paint: rasterize the laid-out DOM into a [`tiny_skia::Pixmap`].
//!
//! Phase 5a. Fills each element's border box with its background color over a
//! white page. Text rendering arrives with the text step; borders and images
//! are later enhancements. Pure Rust (tiny-skia, CPU), deterministic, no system
//! dependencies, so a screenshot is reproducible across hosts.

use obscura_dom::tree::DomTree;
use tiny_skia::{Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, SpreadMode, Transform};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

static FONT_BYTES: &[u8] = include_bytes!("../assets/liberation-sans.ttf");

use crate::layout_dom_with_images;

/// Render `tree` at `viewport` (width, height) in CSS pixels to a Pixmap, or
/// None if the viewport is zero-sized. `base_url`, when given, resolves the
/// relative image URLs (`<img src="logo.svg">`) that make up the overwhelming
/// majority of real-world markup; without it only absolute and `data:` URLs
/// can be fetched.
pub fn paint_dom(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Pixmap> {
    let (w, h) = (viewport.0 as u32, viewport.1 as u32);
    let mut pixmap = Pixmap::new(w, h)?;
    pixmap.fill(Color::WHITE);

    // The same URL (an icon sprite, a repeated background image) commonly
    // backs many elements on one page; fetch each distinct URL at most once
    // per screenshot. `None` caches a failed fetch too, so a broken image
    // reference does not retry on every element that references it.
    let mut image_cache: std::collections::HashMap<String, Option<Vec<u8>>> = std::collections::HashMap::new();
    // Fetch <img> bytes up front to learn intrinsic sizes for layout (a
    // CSS-sized image with no width/height attribute would otherwise be 0x0
    // and never paint). This seeds the same cache the paint pass reads, so
    // each URL is still fetched at most once.
    let intrinsic = collect_image_intrinsics(tree, base_url, &mut image_cache);
    let mut laid = layout_dom_with_images(tree, viewport, &intrinsic);
    // Nodes that live inside an inline `<svg>` we rasterized as one document;
    // their painting is owned by that raster, so they are skipped in both the
    // box/text loop below and the inline-formatting loop after it (an svg
    // `<text>` element must not also paint its glyphs on top of the raster).
    let mut svg_subtree_skip: std::collections::HashSet<obscura_dom::tree::NodeId> = std::collections::HashSet::new();
    // External sprite symbols, keyed by "url#id", extracted from a fetched
    // sprite file so a `<use href="url#id">` resolves. One sprite backs many
    // icons (a whole logo/icon band), so cache the parsed symbol across every
    // inline svg on the page rather than re-parsing the sprite per icon.
    let mut sprite_cache: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    // Whether any element carries a `transform: translate()`. When none does
    // (the overwhelmingly common case), every node's accumulated offset is
    // zero, so skip the per-node ancestor walk entirely and keep the paint
    // path free of any added cost.

    // Tree order so later elements paint over earlier ones (normal flow).
    for nid in tree.descendants(tree.document()) {
        if svg_subtree_skip.contains(&nid) {
            continue;
        }
        let node = match tree.get_node(nid) {
            Some(n) => n,
            None => continue,
        };

        if node.is_text() {
            paint_text_node(tree, nid, &laid, &mut pixmap);
            continue;
        }

        let name = match node.as_element() {
            Some(name) => name,
            None => continue,
        };
        let rect = match laid.rects.get(&nid) {
            Some(r) => *r,
            None => continue,
        };

        let style = match laid.styles.get(&nid) {
            Some(s) => s,
            None => continue,
        };

        if style.effectively_invisible {
            continue;
        }

        // A `transform: translate()` on this element or any ancestor offsets
        // this element's whole painted box (and, applied per node, its whole
        // subtree). The box shifts into screen space; the inherited clip is
        // ALREADY in screen space (shifted by its own owner's translate at
        // `resolve_clip_rects`), so it must not move with this descendant:
        // that is what lets a clip cull a slide the carousel track has
        // translated out of its viewport.
        let (ox, oy) = laid.translates.get(&nid).copied().unwrap_or((0.0, 0.0));
        let rect = crate::Rect { x: rect.x + ox, y: rect.y + oy, width: rect.width, height: rect.height };

        // Ancestor `overflow: hidden` clip, if any. Skip painting entirely
        // once the box has no visible overlap with it (this is what makes the
        // ubiquitous 1x1 clipped "visually hidden" accessibility pattern
        // actually invisible instead of painting text wherever it lands).
        let clip = laid.clip_rects.get(&nid).copied().flatten();
        let visible_rect = match clip {
            Some(c) => match rect.intersect(&c) {
                Some(r) => r,
                None => continue,
            },
            None => rect,
        };
        let box_rect = match Rect::from_xywh(visible_rect.x, visible_rect.y, visible_rect.width, visible_rect.height) {
            Some(r) => r,
            None => continue,
        };

        // Outset box-shadow paints behind this element's own background/border.
        // Geometry comes from the full (translate-adjusted) border box; the
        // ancestor overflow clip is reapplied inside so the shadow is clipped by
        // an ancestor exactly as the box itself is.
        if let Some(shadow) = style.box_shadow {
            paint_box_shadow(&mut pixmap, &shadow, &rect, style.border_radius, clip);
        }

        // Box path (rounded if border-radius), reused for gradient/color fill.
        let r = style.border_radius;
        let bg_path = || if r > 0.5 {
            rounded_rect_path(visible_rect.x, visible_rect.y, visible_rect.width, visible_rect.height, r)
        } else {
            let mut pb = PathBuilder::new();
            pb.push_rect(box_rect);
            pb.finish()
        };
        // A linear-gradient background (heavily used by modern hero sections);
        // without this it paints white. Takes precedence over a solid color.
        // `background-clip: text` clips the background to the glyphs, so it must
        // not paint as a box here; the text paint path fills the glyphs instead.
        if style.mask_image.is_none() && !style.background_clip_text {
            if let Some((angle, stops)) = &style.background_gradient {
                if let Some(path) = bg_path() {
                    paint_linear_gradient(&mut pixmap, &path, &visible_rect, *angle, stops);
                }
            } else if let Some(bg) = style.background_color {
                // A masked element's background-color is the mask's fill color,
                // not an ordinary box background (handled below), so this only
                // runs for unmasked boxes.
                if let Some(path) = bg_path() {
                    let mut paint = Paint::default();
                    paint.set_color(Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
                    paint.anti_alias = r > 0.5;
                    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
                }
            }
        }

        if let Some(mask_url) = &style.mask_image {
            let fill = style.background_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            paint_mask(mask_url, base_url, &visible_rect, fill, &mut pixmap, &mut image_cache);
        } else if let Some(bg_url) = &style.background_image {
            // With an explicit background-size, paint the image at that size
            // positioned within the box per background-position, instead of
            // stretching it to fill the whole element: a small icon
            // (background-size: 0.857em) on a wide text link must stay small
            // and corner-positioned, not balloon to the link's full width.
            let img_rect = match style.background_size {
                Some((iw, ih)) => {
                    let (px, py) = style.background_position;
                    crate::Rect {
                        x: rect.x + (rect.width - iw).max(0.0) * px,
                        y: rect.y + (rect.height - ih).max(0.0) * py,
                        width: iw,
                        height: ih,
                    }
                }
                None => rect,
            };
            let img_rect = match clip {
                Some(c) => img_rect.intersect(&c),
                None => Some(img_rect),
            };
            if let Some(img_rect) = img_rect {
                // object-fit does not apply to background images (that is
                // background-size, handled above); paint the layer as-is.
                paint_image(bg_url, base_url, &img_rect, crate::ObjectFit::Fill, &mut pixmap, &mut image_cache);
            }
        }

        // Rounded, uniform border: stroke the rounded-rect outline instead of
        // four sharp edge rects.
        let uniform_border = style.border.top == style.border.right
            && style.border.right == style.border.bottom
            && style.border.bottom == style.border.left
            && style.border.top > 0.0;
        if style.border_radius > 0.5 && uniform_border {
            let bc = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            let w = style.border.top;
            if let Some(path) = rounded_rect_path(rect.x + w / 2.0, rect.y + w / 2.0, rect.width - w, rect.height - w, (style.border_radius - w / 2.0).max(0.0)) {
                let mut paint = Paint::default();
                paint.set_color(Color::from_rgba8(bc[0], bc[1], bc[2], bc[3]));
                paint.anti_alias = true;
                let stroke = tiny_skia::Stroke { width: w, ..Default::default() };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        } else if style.border.top > 0.0 || style.border.right > 0.0 || style.border.bottom > 0.0 || style.border.left > 0.0 {
            let bc = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(bc[0], bc[1], bc[2], bc[3]));
            paint.anti_alias = false;

            let mut path = PathBuilder::new();
            let mut push_clipped = |x: f32, y: f32, w: f32, h: f32| {
                let edge = crate::Rect { x, y, width: w, height: h };
                let edge = match clip { Some(c) => edge.intersect(&c), None => Some(edge) };
                if let Some(e) = edge {
                    if let Some(r) = Rect::from_xywh(e.x, e.y, e.width, e.height) {
                        path.push_rect(r);
                    }
                }
            };
            if style.border.top > 0.0 {
                push_clipped(rect.x, rect.y, rect.width, style.border.top);
            }
            if style.border.right > 0.0 {
                push_clipped(rect.x + rect.width - style.border.right, rect.y, style.border.right, rect.height);
            }
            if style.border.bottom > 0.0 {
                push_clipped(rect.x, rect.y + rect.height - style.border.bottom, rect.width, style.border.bottom);
            }
            if style.border.left > 0.0 {
                push_clipped(rect.x, rect.y, style.border.left, rect.height);
            }
            if let Some(path) = path.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }

        if name.local.as_ref() == "img" {
            if let Some((src, _density)) = resolve_img_url(tree, nid) {
                let painted = paint_image(&src, base_url, &rect, style.object_fit, &mut pixmap, &mut image_cache);
                // Fall back to alt text only when the image itself did not paint.
                if !painted {
                    // A fetch/decode failure paints a neutral grey box (like a
                    // browser's lazy/broken-image placeholder) so a missing
                    // image reads as "not loaded", not as a broken render.
                    // box_rect/visible_rect are already clip-intersected, so
                    // this never paints outside an overflow:hidden clip.
                    if visible_rect.width >= 4.0 && visible_rect.height >= 4.0 {
                        let mut ph = Paint::default();
                        ph.set_color(Color::from_rgba8(0xE9, 0xEA, 0xEC, 0xFF));
                        pixmap.fill_rect(box_rect, &ph, Transform::identity(), None);
                    }
                    if let Some(alt) = node.get_attribute("alt") {
                        if !alt.is_empty() {
                            draw_text(&mut pixmap, alt, rect.x, rect.y, [0, 0, 0, 255], 12.0, false, clip);
                        }
                    }
                }
            }
        }

        // Inline `<svg>...</svg>`: serialize the whole subtree back to one
        // standalone SVG document and rasterize it as a unit, so a
        // `<use href="#id">` resolves against the `<symbol>`/`<defs>` in the
        // same svg. The raster owns the subtree, so its DOM children are not
        // painted individually (they are added to `svg_subtree_skip`). The svg
        // is drawn at its full border-box size (undistorted) and clipped to the
        // overflow-visible region.
        if name.local.as_ref() == "svg" {
            let mut markup = serialize_svg(tree, nid);
            // `<use href="url#id">` pointing at an EXTERNAL sprite file resolves
            // to nothing in resvg (the symbol lives in another document). Fetch
            // the sprite, splice the referenced `<symbol>` into a local `<defs>`,
            // and rewrite the href to a same-document `#id`. Same-document
            // `<use href="#id">` (empty url) is untouched.
            inject_external_sprites(tree, nid, base_url, &mut markup, &mut image_cache, &mut sprite_cache);
            if let Some(content) = render_svg(markup.as_bytes(), rect.width as u32, rect.height as u32) {
                let mask = clip.and_then(|_| rect_clip_mask(pixmap.width(), pixmap.height(), &visible_rect));
                pixmap.draw_pixmap(
                    rect.x as i32,
                    rect.y as i32,
                    content.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    mask.as_ref(),
                );
            }
            for child in tree.descendants(nid) {
                svg_subtree_skip.insert(child);
            }
        }

        // List-item marker (bullet or number), drawn in the indent to the left
        // of the item's content box. `list_style` is inherited and resolved,
        // so `None` (e.g. a nav `<ul style="list-style:none">`) suppresses it.
        if name.local.as_ref() == "li" {
            if let Some(marker) = list_marker_text(tree, nid, style.list_style) {
                let fsize = style.font_size.unwrap_or(16.0);
                let color = style.color.unwrap_or([0, 0, 0, 255]);
                let mw = measure_text(&marker, fsize, false);
                let mx = rect.x + style.padding.left - mw - 6.0;
                let my = rect.y + style.border.top + style.padding.top;
                draw_text(&mut pixmap, &marker, mx, my, color, fsize, false, clip);
            }
        }

        // `::before`/`::after` generated text (see `dom::build_pseudo_content`)
        // has no DOM text node of its own; its word runs are registered under
        // the host element's own id instead, so paint them here rather than
        // through `paint_text_node` (which only runs for real text nodes).
        if let Some(runs) = laid.text_runs.get(&nid) {
            let color = style.color.unwrap_or([0, 0, 0, 255]);
            let fsize = style.font_size.unwrap_or(16.0);
            let is_bold = style.font_weight.as_deref() == Some("bold");
            for (word_rect, word) in runs {
                draw_text(&mut pixmap, word, word_rect.x + ox, word_rect.y + oy, color, fsize, is_bold, clip);
            }
        }

        // An empty text `<input>`/`<textarea>` shows its `placeholder`
        // attribute as muted text; there is no DOM text node for it (it is
        // not real content), so paint it directly from the attribute instead
        // of going through `paint_text_node`.
        if name.local.as_ref() == "input" || name.local.as_ref() == "textarea" {
            let has_value = node.get_attribute("value").map(|v| !v.is_empty()).unwrap_or(false);
            if !has_value {
                if let Some(placeholder) = node.get_attribute("placeholder") {
                    if !placeholder.is_empty() {
                        let fsize = style.font_size.unwrap_or(16.0);
                        let text_x = rect.x + style.padding.left + style.border.left;
                        let text_y = rect.y + style.padding.top + style.border.top;
                        draw_text(&mut pixmap, placeholder, text_x, text_y, [117, 117, 117, 255], fsize, false, clip);
                    }
                }
            }
        }
    }

    // Inline formatting contexts shaped by cosmic-text (paragraphs, headings,
    // cells, labels) draw last, in tree order, so their glyphs sit above the
    // box backgrounds/borders painted in the loop above. Each item already
    // carries its final origin and clip from `TextEngine::finalize`.
    for nid in tree.descendants(tree.document()) {
        if svg_subtree_skip.contains(&nid) {
            continue;
        }
        let whole = laid.ifc_items.get(&nid).copied();
        let run_items = laid.run_ifc_items.get(&nid).cloned();
        if whole.is_none() && run_items.is_none() {
            continue;
        }
        if laid.styles.get(&nid).map(|s| s.effectively_invisible).unwrap_or(false) {
            continue;
        }
        // Shift the shaped glyphs by the same accumulated translate as the
        // container's box so text under a transformed ancestor moves with
        // it. Computed before the mutable `paint_item` borrow.
        let off = laid.translates.get(&nid).copied().unwrap_or((0.0, 0.0));
        if let Some(idx) = whole {
            laid.text_engine.paint_item(idx, &mut pixmap, off);
        }
        // Anonymous inline-run leaves of a mixed block (see
        // `build_mixed_block`), pinned to their own boxes at finalize.
        if let Some(items) = run_items {
            for idx in items {
                laid.text_engine.paint_item(idx, &mut pixmap, off);
            }
        }
    }

    Some(pixmap)
}

/// A closed rounded-rectangle path, corners approximated by quadratic curves
/// (visually indistinguishable from true arcs at typical UI radii). `r` is
/// clamped so it never exceeds half the shorter side.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Paint an outset `box-shadow` layer behind the element's own box. `rect` is
/// the element's (translate-adjusted) border box; the shadow is that box offset
/// by (offset_x, offset_y), expanded by `spread`, with a `blur`-wide soft edge.
/// tiny-skia has no gaussian blur, so the blur is approximated by nested
/// rounded rects from a solid core out to the blur radius, each at a fraction of
/// the shadow alpha so source-over accumulation ramps the coverage from full at
/// the core to near-zero at the outer edge. `inset` shadows are parsed but not
/// painted (an inner shadow needs a hole-punched fill this box model does not
/// build). `clip`, when set, is the ancestor `overflow: hidden` region and is
/// applied as a mask so the shadow is clipped like the element itself.
fn paint_box_shadow(
    pixmap: &mut Pixmap,
    shadow: &crate::BoxShadow,
    rect: &crate::Rect,
    border_radius: f32,
    clip: Option<crate::Rect>,
) {
    if shadow.inset || shadow.color[3] == 0 {
        return;
    }
    let spread = shadow.spread;
    let x0 = rect.x + shadow.offset_x - spread;
    let y0 = rect.y + shadow.offset_y - spread;
    let w0 = rect.width + 2.0 * spread;
    let h0 = rect.height + 2.0 * spread;
    if w0 <= 0.0 || h0 <= 0.0 {
        return;
    }
    let r0 = (border_radius + spread).max(0.0);
    let blur = shadow.blur.max(0.0);
    // Ancestor overflow clip: build a mask once and reuse it for every layer.
    let mask = match clip {
        Some(c) => {
            if c.width <= 0.0 || c.height <= 0.0 {
                return;
            }
            box_clip_mask(pixmap.width(), pixmap.height(), &c)
        }
        None => None,
    };
    let color = shadow.color;
    if blur < 0.5 {
        // No blur: a single crisp, offset (and spread) rounded rect.
        fill_shadow_rect(pixmap, x0, y0, w0, h0, r0, color, mask.as_ref());
        return;
    }
    let steps: u32 = (blur.ceil() as u32).clamp(2, 24);
    // Per-layer alpha chosen so `steps` source-over composites reach the target
    // alpha at the core: 1 - (1 - a)^steps == A  =>  a = 1 - (1 - A)^(1/steps).
    let a_frac = color[3] as f32 / 255.0;
    let per = 1.0 - (1.0 - a_frac).powf(1.0 / steps as f32);
    let layer_alpha = (per * 255.0).round().clamp(1.0, 255.0) as u8;
    let layer_color = [color[0], color[1], color[2], layer_alpha];
    for j in 0..steps {
        // j = 0 is the solid core (expansion 0); j = steps-1 reaches the blur
        // radius. Larger rects paint first, smaller (more-covered) ones on top.
        let e = blur * (j as f32) / ((steps - 1) as f32);
        fill_shadow_rect(pixmap, x0 - e, y0 - e, w0 + 2.0 * e, h0 + 2.0 * e, r0 + e, layer_color, mask.as_ref());
    }
}

/// Fill one (possibly rounded) shadow rectangle with a flat color, optionally
/// masked to an ancestor clip region. A helper for `paint_box_shadow`'s layers.
fn fill_shadow_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: [u8; 4],
    mask: Option<&tiny_skia::Mask>,
) {
    if w <= 0.0 || h <= 0.0 || color[3] == 0 {
        return;
    }
    let path = if radius > 0.5 {
        match rounded_rect_path(x, y, w, h, radius) {
            Some(p) => p,
            None => return,
        }
    } else {
        let r = match Rect::from_xywh(x, y, w, h) {
            Some(r) => r,
            None => return,
        };
        let mut pb = PathBuilder::new();
        pb.push_rect(r);
        match pb.finish() {
            Some(p) => p,
            None => return,
        }
    };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), mask);
}

/// The marker text for a list item, or `None` when markers are suppressed
/// (`list-style: none`). `Decimal` numbers the item by its position among
/// sibling list items so `<ol>`s count 1, 2, 3.
fn list_marker_text(tree: &DomTree, nid: obscura_dom::tree::NodeId, style: Option<crate::ListStyle>) -> Option<String> {
    match style {
        Some(crate::ListStyle::Disc) => Some("\u{2022}".to_string()),
        Some(crate::ListStyle::Circle) => Some("\u{25E6}".to_string()),
        Some(crate::ListStyle::Square) => Some("\u{25AA}".to_string()),
        Some(crate::ListStyle::Decimal) => {
            let mut n = 1usize;
            let mut cur = tree.get_node(nid).and_then(|node| node.prev_sibling);
            while let Some(sib) = cur {
                if tree.get_node(sib).and_then(|s| s.as_element().map(|e| e.local.to_string())).as_deref() == Some("li") {
                    n += 1;
                }
                cur = tree.get_node(sib).and_then(|s| s.prev_sibling);
            }
            Some(format!("{}.", n))
        }
        Some(crate::ListStyle::None) | None => None,
    }
}

/// Render `tree` at `viewport` to PNG bytes (RGBA 8-bit). Returns None if the
/// viewport is zero-sized. Convenience over `paint_dom` + `encode_png`.
pub fn screenshot_png(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Vec<u8>> {
    paint_dom(tree, viewport, base_url)?.encode_png().ok()
}

/// A representative visible color for `background-clip: text` text whose own
/// color is transparent, used on the word-split paint path (the cosmic-text IFC
/// path samples the gradient per glyph in `inline`). Returns the gradient's mid
/// stop or the background color so a transparent-colored label still paints;
/// `None` when the element is not a transparent-text clip-to-text box.
fn clip_text_fill_color(style: &crate::LayoutStyle) -> Option<[u8; 4]> {
    if !style.background_clip_text {
        return None;
    }
    if style.color.map(|c| c[3] != 0).unwrap_or(true) {
        return None;
    }
    if let Some((_, stops)) = &style.background_gradient {
        if !stops.is_empty() {
            let mid = stops[stops.len() / 2].0;
            return Some([mid[0], mid[1], mid[2], 255]);
        }
    }
    style.background_color.filter(|c| c[3] != 0).map(|c| [c[0], c[1], c[2], 255])
}

/// Paint every word of a text node at its own laid-out position. A text node
/// lays out as one taffy leaf per word (see `dom::build_text_words`), each
/// wrapping independently, so its content is a list of (box, word) pairs
/// rather than one box for the whole node; color/font/clip come from the
/// parent element and are the same for every word.
fn paint_text_node(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    laid: &crate::DomLayout,
    pixmap: &mut Pixmap,

) -> Option<()> {
    let runs = laid.text_runs.get(&nid)?;
    let node = tree.get_node(nid)?;
    let parent = node.parent?;
    let style = laid.styles.get(&parent)?;
    if style.effectively_invisible {
        return Some(());
    }
    let color = clip_text_fill_color(style).unwrap_or_else(|| style.color.unwrap_or([0, 0, 0, 255]));
    let fsize = style.font_size.unwrap_or(16.0);
    let is_bold = style.font_weight.as_deref() == Some("bold");
    // A text node has no transform of its own, but any transformed element
    // ancestor offsets it (the accumulation covers text nodes too). The clip
    // is already in screen space and stays put.
    let (ox, oy) = laid.translates.get(&nid).copied().unwrap_or((0.0, 0.0));
    let clip = laid.clip_rects.get(&nid).copied().flatten();

    for (rect, word) in runs {
        draw_text(pixmap, word, rect.x + ox, rect.y + oy, color, fsize, is_bold, clip);
    }
    Some(())
}

pub fn measure_text(text: &str, size: f32, is_bold: bool) -> f32 {
    let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
    let scale = PxScale::from(size);
    let scaled_font = font.as_scaled(scale);
    let mut width = 0.0;
    for c in text.chars() {
        if c.is_control() { continue; }
        width += scaled_font.h_advance(font.glyph_id(c));
    }
    if is_bold { width += text.chars().filter(|c| !c.is_control()).count() as f32; }
    width
}

fn draw_text(pixmap: &mut Pixmap, text: &str, x: f32, y: f32, color: [u8; 4], size: f32, is_bold: bool, clip: Option<crate::Rect>) {
    // A fully clipped-away run (the common "visually hidden" accessibility
    // pattern: a 1x1 box with overflow: hidden) paints nothing at all.
    if let Some(c) = clip {
        if c.width <= 0.0 || c.height <= 0.0 {
            return;
        }
    }
    let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
    let scale = PxScale::from(size);
    let scaled_font = font.as_scaled(scale);
    let mut caret = ab_glyph::point(x, y + scaled_font.ascent());

    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    let clip_bounds = clip.map(|c| (c.x, c.y, c.x + c.width, c.y + c.height));
    let pixels = pixmap.pixels_mut();
    let (r, g, b, a_full) = (color[0], color[1], color[2], color[3]);

    for c in text.chars() {
        if c.is_control() {
            continue;
        }
        let glyph_id = font.glyph_id(c);
        let id = glyph_id;
        let glyph = glyph_id.with_scale_and_position(scale, caret);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, c| {
                let px = (bounds.min.x + gx as f32) as i32;
                let py = (bounds.min.y + gy as f32) as i32;
                if let Some((cx0, cy0, cx1, cy1)) = clip_bounds {
                    if (px as f32) < cx0 || (px as f32) >= cx1 || (py as f32) < cy0 || (py as f32) >= cy1 {
                        return;
                    }
                }
                if px >= 0 && px < width && py >= 0 && py < height {
                    let alpha = (a_full as f32 * c) as u8;
                    if alpha > 0 {
                        let mut px_indices = vec![(py * width + px) as usize];
                        if is_bold && px + 1 < width {
                            px_indices.push((py * width + px + 1) as usize);
                        }
                        for idx in px_indices {
                            let dst = pixels[idx];
                            
                            let src_a = alpha as u32;
                            let src_r = (r as u32 * src_a) / 255;
                            let src_g = (g as u32 * src_a) / 255;
                            let src_b = (b as u32 * src_a) / 255;
                            
                            let dst_a = dst.alpha() as u32;
                            let out_a = src_a + (dst_a * (255 - src_a) / 255);
                            
                            if out_a > 0 {
                                let out_r = src_r + (dst.red() as u32 * (255 - src_a) / 255);
                                let out_g = src_g + (dst.green() as u32 * (255 - src_a) / 255);
                                let out_b = src_b + (dst.blue() as u32 * (255 - src_a) / 255);
                                
                                pixels[idx] = tiny_skia::PremultipliedColorU8::from_rgba(
                                    out_r as u8, out_g as u8, out_b as u8, out_a as u8
                                ).unwrap_or_else(|| tiny_skia::PremultipliedColorU8::from_rgba(0,0,0,0).unwrap());
                            }
                        }
                    }
                }
            });
            // Matches measure_text's +1px-per-character bold compensation:
            // without it, a word's reserved layout width (from measure_text)
            // is wider than what draw_text actually advances through, and
            // the difference shows up as a visible gap after every word once
            // each word is its own independently-positioned box.
            caret.x += scaled_font.h_advance(id) + if is_bold { 1.0 } else { 0.0 };
        } else {
            caret.x += scaled_font.h_advance(id) + if is_bold { 1.0 } else { 0.0 };
        }
    }
}

/// Resolve `src` (a `data:` URI, or an absolute/relative URL against
/// `base_url`) to raw bytes, fetching over the network at most once per
/// distinct URL per screenshot via `cache`.
fn fetch_bytes(
    src: &str,
    base_url: Option<&str>,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
) -> Option<Vec<u8>> {
    if let Some(rest) = src.strip_prefix("data:image/") {
        let comma_idx = rest.find(',')?;
        let (meta, data) = (&rest[..comma_idx], &rest[comma_idx + 1..]);
        // Inline SVGs are very commonly authored as data:image/svg+xml;utf8,
        // (or with no encoding label at all, which is equivalent): plain,
        // percent-escaped text, not base64. Only decode as base64 when the
        // URI actually says so.
        return if meta.contains("base64") {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(data).ok()
        } else {
            Some(percent_decode(data))
        };
    }
    // Resolve relative to the document's base URL: the overwhelming majority
    // of real markup uses relative image paths ("logo.svg", not
    // "https://example.com/logo.svg"), so without this every relative <img>
    // or mask/background reference silently fails to fetch.
    let resolved = if src.starts_with("http://") || src.starts_with("https://") {
        Some(src.to_string())
    } else if let Some(rest) = src.strip_prefix("//") {
        // Protocol-relative URL (`//upload.wikimedia.org/...`, ubiquitous on
        // Wikipedia and CDN-hosted media): inherit the document scheme, but
        // never `file:`/other non-network schemes (a `file://` base would give
        // `file://host/...` and fail), so default to https for those.
        let scheme = base_url
            .and_then(|b| url::Url::parse(b).ok())
            .map(|u| u.scheme().to_string())
            .filter(|s| s == "http" || s == "https")
            .unwrap_or_else(|| "https".to_string());
        Some(format!("{scheme}://{rest}"))
    } else {
        base_url
            .and_then(|b| url::Url::parse(b).ok())
            .and_then(|base| base.join(src).ok())
            .map(|u| u.to_string())
    };
    // The same icon/sprite/background is routinely referenced by dozens of
    // elements on one page (every story's vote arrow, every repeated logo);
    // fetch each distinct URL over the network once per screenshot rather
    // than once per element.
    let url = resolved?;
    cache
        .entry(url.clone())
        .or_insert_with(|| http_get_bytes(&url))
        .clone()
}

/// Fetch `url` with a descriptive User-Agent and a bounded timeout, retrying on
/// rate-limit / transient errors with backoff. Real pages pull dozens of images
/// from one CDN in a burst (a Wikipedia article references ~60); hosts like
/// Wikimedia answer a rapid burst with HTTP 429 after ~10 requests. Without a
/// retry the rate-limited images (e.g. an infobox photo montage fetched late in
/// the burst) came back blank, and the failure was cached permanently. The
/// backoff both recovers them and paces the burst back under the limit.
fn http_get_bytes(url: &str) -> Option<Vec<u8>> {
    let mut backoff = std::time::Duration::from_millis(200);
    for attempt in 0..3 {
        // A browser-like Accept advertises the modern image formats and is what
        // content-negotiating CDNs expect; some UA-gated hosts also reject a
        // request with no Accept header outright.
        let res = image_agent()
            .get(url)
            .set("Accept", "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8")
            .call();
        match res {
            Ok(resp) => {
                let mut buf = Vec::new();
                use std::io::Read;
                return resp.into_reader().read_to_end(&mut buf).ok().map(|_| buf);
            }
            // 429 (rate limit) and 5xx are transient: a short backoff clears a
            // brief blip. A sustained limit (Wikimedia 429s a 60-image burst
            // from a datacenter IP hard, with `Retry-After: 1`) is NOT worth
            // waiting out here: honoring the hint stalls the whole render for
            // minutes, so fast-fail to the grey placeholder instead. Real
            // fidelity for that case needs an HTTP/2 image client (multiplexing
            // like Chrome), not blocking retries.
            Err(ureq::Error::Status(code, _)) if matches!(code, 429 | 500 | 502 | 503 | 504) && attempt < 2 => {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(ureq::Error::Transport(_)) if attempt < 2 => {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(_) => return None,
        }
    }
    None
}

/// One shared HTTP agent for all image fetches in the process, with a browser
/// User-Agent and keep-alive connection pooling. A CDN's bot rate-limiter keys
/// on connection churn as much as on rate: a fresh TLS handshake per image (the
/// old per-call `ureq::get`) reads as a burst and gets 429'd, whereas reusing
/// one pooled connection to the same host (as a browser does) both avoids most
/// throttling and is much faster on an image-heavy page.
fn image_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            // Present the same normal browser identity the engine uses for the
            // document. A bot-identifying UA got image requests filtered by CDNs
            // that gate on User-Agent (Akamai/Cloudflare image endpoints on
            // cnbc, techcrunch, arstechnica), so the images Chrome loads came
            // back blank; a real browser UA loads the same bytes Chrome does.
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
            .build()
    })
}

/// Decode a percent-escaped data: URI payload (`%23` -> `#`, etc). Bytes that
/// are not part of a `%XX` escape pass through unchanged, which is exactly
/// right for the inline-SVG case: only the characters that would otherwise be
/// ambiguous in a URI (`#`, `"`, ...) get escaped, everything else is literal
/// UTF-8 text.
fn percent_decode(s: &str) -> Vec<u8> {
    // Operates on raw bytes throughout (never slices `s` as a string): a
    // stray '%' followed by non-hex bytes could otherwise land a string
    // slice in the middle of a multi-byte UTF-8 character and panic.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Decode raster image bytes (jpeg/png/webp) to a premultiplied-alpha pixmap
/// resized to `w`x`h`.
fn raster_to_pixmap(bytes: &[u8], w: u32, h: u32) -> Option<Pixmap> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let resized = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    let mut raw = resized.into_raw();
    for pixel in raw.chunks_exact_mut(4) {
        let a = pixel[3] as u32;
        pixel[0] = ((pixel[0] as u32 * a) / 255) as u8;
        pixel[1] = ((pixel[1] as u32 * a) / 255) as u8;
        pixel[2] = ((pixel[2] as u32 * a) / 255) as u8;
    }
    let size = tiny_skia::IntSize::from_wh(w, h)?;
    Pixmap::from_vec(raw, size)
}

/// Read an image's intrinsic pixel dimensions from its header only, without
/// decoding the whole thing. Returns None for formats the raster decoder does
/// not recognize (e.g. SVG, which is sized elsewhere).
/// Fill `path` with a CSS `linear-gradient`. `angle` is degrees clockwise from
/// 12 o'clock (0 = to top). The gradient line length uses the CSS formula so
/// the stops land where a browser puts them. Positionless stops are spread
/// evenly; positions are clamped monotonic (tiny-skia requires ascending).
fn paint_linear_gradient(pixmap: &mut Pixmap, path: &tiny_skia::Path, rect: &crate::Rect, angle: f32, stops: &[([u8; 4], Option<f32>)]) {
    if stops.len() < 2 {
        return;
    }
    let rad = angle.to_radians();
    let dx = rad.sin();
    let dy = -rad.cos();
    let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let half = (dx.abs() * rect.width + dy.abs() * rect.height) / 2.0;
    let start = Point::from_xy(cx - dx * half, cy - dy * half);
    let end = Point::from_xy(cx + dx * half, cy + dy * half);
    let n = stops.len();
    let mut gs: Vec<GradientStop> = Vec::with_capacity(n);
    let mut last = 0.0f32;
    for (i, (c, pos)) in stops.iter().enumerate() {
        let p = pos.unwrap_or(i as f32 / (n - 1) as f32).clamp(0.0, 1.0).max(last);
        last = p;
        gs.push(GradientStop::new(p, Color::from_rgba8(c[0], c[1], c[2], c[3])));
    }
    if let Some(shader) = LinearGradient::new(start, end, gs, SpreadMode::Pad, Transform::identity()) {
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Fetch every `<img>` once (seeding `cache` for the paint pass) and record its
/// intrinsic (width, height) so layout can size replaced elements that have no
/// explicit dimensions. Keyed by the `<img>`'s NodeId.
fn collect_image_intrinsics(
    tree: &DomTree,
    base_url: Option<&str>,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
) -> std::collections::HashMap<obscura_dom::tree::NodeId, (f32, f32)> {
    let mut out = std::collections::HashMap::new();
    for nid in tree.descendants(tree.document()) {
        let Some(node) = tree.get_node(nid) else { continue };
        if node.as_element().map(|e| e.local.as_ref() != "img").unwrap_or(true) {
            continue;
        }
        let Some((url, density)) = resolve_img_url(tree, nid) else { continue };
        let Some(bytes) = fetch_bytes(&url, base_url, cache) else { continue };
        if let Some((w, h)) = image_dimensions(&bytes) {
            if w > 0 && h > 0 {
                // A 2x (or w-descriptor) candidate's raw pixels are density
                // times its CSS size; divide so layout sees CSS px, or every
                // responsive image occupies twice its design size.
                out.insert(nid, (w as f32 / density, h as f32 / density));
            }
        }
    }
    out
}

/// Choose the URL to paint for an `<img>`. Browsers do not use `src` alone:
/// a wrapping `<picture>`'s `<source>`s, `srcset`, and `sizes` select by
/// type/media/viewport/density, and lazy-loaded images keep the real URL in
/// `data-src`/`data-srcset` with `src` holding a 1x1 placeholder until script
/// swaps it in. Since obscura may not have run the site's lazy-load script,
/// resolve the same URL the browser would end up with: a matching `<picture>`
/// source first, then a real candidate from `srcset`/`data-srcset`, then a
/// non-inline `src`/`data-*` URL, then any `src` (an inlined data: image).
fn resolve_img_url(tree: &DomTree, nid: obscura_dom::tree::NodeId) -> Option<(String, f32)> {
    let node = tree.get_node(nid)?;
    // A <picture>'s preceding, type/media-matching <source> wins over the
    // <img>'s own attributes (HTML "update the source set").
    if let Some(pick) = picture_source_url(tree, nid) {
        return Some(pick);
    }
    let sizes = node.get_attribute("sizes");
    for a in ["srcset", "data-srcset"] {
        if let Some(v) = node.get_attribute(a) {
            if let Some(pick) = best_srcset_candidate(v, sizes) {
                return Some(pick);
            }
        }
    }
    let url_attrs = ["src", "data-src", "data-lazy-src", "data-original", "data-fallback-src", "data-lazy"];
    // A non-inline URL first (a data: src is usually the lazy-load placeholder).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() && !v.starts_with("data:") {
                return Some((v.to_string(), 1.0));
            }
        }
    }
    // Otherwise fall back to whatever is there (an inlined data: image).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() {
                return Some((v.to_string(), 1.0));
            }
        }
    }
    None
}

/// When `img_nid` is an `<img>` inside a `<picture>`, walk its preceding
/// `<source>` siblings in document order and return the selected URL of the
/// first supported one (matching `type` and `media`), per WebKit's
/// `HTMLImageElement::bestFitSourceFromPictureElement`. `None` means no source
/// applied and the caller should fall back to the `<img>`'s own attributes.
fn picture_source_url(tree: &DomTree, img_nid: obscura_dom::tree::NodeId) -> Option<(String, f32)> {
    let img = tree.get_node(img_nid)?;
    let parent = img.parent?;
    let is_picture = tree
        .get_node(parent)
        .and_then(|p| p.as_element().map(|e| e.local.as_ref() == "picture"))
        .unwrap_or(false);
    if !is_picture {
        return None;
    }
    for cid in tree.children(parent) {
        // Only sources that precede the <img> contribute.
        if cid == img_nid {
            break;
        }
        let Some(child) = tree.get_node(cid) else { continue };
        if child.as_element().map(|e| e.local.as_ref() != "source").unwrap_or(true) {
            continue;
        }
        let Some(srcset) = child.get_attribute("srcset") else { continue };
        if srcset.trim().is_empty() {
            continue;
        }
        if let Some(t) = child.get_attribute("type") {
            if !source_type_supported(t) {
                continue;
            }
        }
        if let Some(m) = child.get_attribute("media") {
            if !m.trim().is_empty() && !crate::css::media_query_applies(m) {
                continue;
            }
        }
        let sizes = child.get_attribute("sizes");
        if let Some(u) = best_srcset_candidate(srcset, sizes) {
            return Some(u);
        }
    }
    None
}

/// Whether a `<source type=...>` names an image format this build can decode.
/// AVIF/JPEG-XL are intentionally excluded: the `image` crate cannot decode
/// them here, so a decodable `<img>` fallback must win over such a source.
fn source_type_supported(t: &str) -> bool {
    matches!(
        t.trim().to_ascii_lowercase().as_str(),
        "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp"
            | "image/bmp" | "image/svg+xml" | "image/x-icon" | "image/vnd.microsoft.icon"
    )
}

/// Assumed layout viewport width (matches the desktop width the `@media`
/// cascade evaluates against in `css.rs`). Used to turn `w` descriptors and
/// `vw`/`%` source sizes into effective pixel densities.
const SRCSET_VIEWPORT_W: f32 = 1280.0;

/// Pick one URL from a `srcset` list, matching the WebKit/Blink selection:
/// normalize each `w` descriptor to an effective density (`w / source-size`,
/// with the source-size taken from `sizes` or falling back to the viewport
/// width), treat `x` descriptors as-is and a bare candidate as `1x`, then pick
/// the smallest density at least the device pixel ratio (1 at DPR 1), else the
/// largest available.
/// Returns the picked candidate URL and its pixel density. The density is the
/// x-descriptor (or, for w-descriptors, width / source-size): the factor the
/// file's raw pixels must be divided by to get CSS px. Laying out with raw
/// pixels made every 2x responsive image occupy twice its design size.
fn best_srcset_candidate(srcset: &str, sizes: Option<&str>) -> Option<(String, f32)> {
    const DPR: f32 = 1.0;
    let source_size = source_size_px(sizes);
    let mut cands: Vec<(f32, String)> = Vec::new();
    // Parse candidates WHATWG-style: a URL is a run of non-whitespace (so a
    // data: URI's internal commas stay part of it, unlike a naive split on
    // ','), optionally followed by a descriptor up to the next comma.
    let is_ws = |c: char| c.is_whitespace();
    let mut rest = srcset.trim_start_matches(|c: char| is_ws(c) || c == ',');
    while !rest.is_empty() {
        let url_end = rest.find(is_ws).unwrap_or(rest.len());
        let raw_url = &rest[..url_end];
        rest = &rest[url_end..];
        // Trailing commas on the URL mean the candidate had no descriptor.
        let url = raw_url.trim_end_matches(',');
        let no_desc = url.len() != raw_url.len();
        rest = rest.trim_start_matches(is_ws);
        let desc = if no_desc {
            ""
        } else {
            let d_end = rest.find(',').unwrap_or(rest.len());
            let d = rest[..d_end].trim();
            rest = &rest[d_end..];
            d
        };
        rest = rest.trim_start_matches(|c: char| c == ',' || is_ws(c));
        if url.is_empty() {
            continue;
        }
        let density = if desc.is_empty() {
            1.0
        } else if let Some(w) = desc.strip_suffix('w').and_then(|s| s.parse::<f32>().ok()) {
            if source_size > 0.0 { w / source_size } else { continue }
        } else if let Some(x) = desc.strip_suffix('x').and_then(|s| s.parse::<f32>().ok()) {
            x
        } else {
            // An `h` (height) descriptor or malformed token: skip the candidate.
            continue;
        };
        cands.push((density, url.to_string()));
    }
    if cands.is_empty() {
        return None;
    }
    cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let pick = cands
        .iter()
        .find(|(d, _)| *d >= DPR)
        .map(|(d, u)| (u.clone(), *d))
        .unwrap_or_else(|| {
            let (d, u) = cands.last().unwrap();
            (u.clone(), *d)
        });
    Some((pick.0, pick.1.max(0.01)))
}

/// Approximate the CSS px size an image will be displayed at, from its `sizes`
/// attribute: the first entry whose media condition holds at our assumed
/// desktop viewport (a bare entry always holds), else the viewport width. Used
/// only to convert `w` descriptors to densities, so a coarse value is fine.
fn source_size_px(sizes: Option<&str>) -> f32 {
    let Some(sizes) = sizes else { return SRCSET_VIEWPORT_W };
    for entry in sizes.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (cond, len) = split_size_entry(entry);
        if let Some(cond) = cond {
            if !crate::css::media_query_applies(&cond) {
                continue;
            }
        }
        if let Some(px) = length_to_px(&len) {
            return px;
        }
    }
    SRCSET_VIEWPORT_W
}

/// Split one `sizes` entry into its optional leading media condition and its
/// trailing `<length>`. Tokenizes on whitespace at paren depth 0 so a
/// `calc(...)` length or a parenthesized condition stays intact; the last
/// token is the length, anything before it is the condition.
fn split_size_entry(entry: &str) -> (Option<String>, String) {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in entry.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() { tokens.push(std::mem::take(&mut cur)); }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let len = tokens.pop().unwrap_or_default();
    let cond = if tokens.is_empty() { None } else { Some(tokens.join(" ")) };
    (cond, len)
}

/// Resolve a `sizes` length to px against the assumed viewport. `vw`/`%` scale
/// by the viewport width; `px` is literal; `em`/`rem` use the 16px root.
/// `calc()` and other forms return `None` (the caller tries the next entry).
fn length_to_px(len: &str) -> Option<f32> {
    let t = len.trim().to_ascii_lowercase();
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(v) = t.strip_suffix("vw").and_then(num) { return Some(v / 100.0 * SRCSET_VIEWPORT_W); }
    if let Some(v) = t.strip_suffix('%').and_then(num) { return Some(v / 100.0 * SRCSET_VIEWPORT_W); }
    if let Some(v) = t.strip_suffix("px").and_then(num) { return Some(v); }
    if let Some(v) = t.strip_suffix("rem").and_then(num) { return Some(v * 16.0); }
    if let Some(v) = t.strip_suffix("em").and_then(num) { return Some(v * 16.0); }
    num(&t)
}

fn paint_image(
    src: &str,
    base_url: Option<&str>,
    rect: &crate::Rect,
    object_fit: crate::ObjectFit,
    pixmap: &mut Pixmap,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let Some(bytes) = fetch_bytes(src, base_url, cache) else { return false };
    let svg = is_svg(&bytes);

    // Destination sub-rect within the element box. `Fill` keeps the historical
    // behavior (stretch the image to the whole box); the other modes need the
    // image's intrinsic size to preserve its aspect ratio, and fall back to
    // fill when it cannot be read.
    let dest = if object_fit == crate::ObjectFit::Fill {
        *rect
    } else {
        let intrinsic = if svg {
            svg_intrinsic(&bytes)
        } else {
            image_dimensions(&bytes).map(|(w, h)| (w as f32, h as f32))
        };
        match intrinsic {
            Some((iw, ih)) => object_fit_dest(rect, iw, ih, object_fit),
            None => *rect,
        }
    };

    let (dw, dh) = (dest.width.round().max(1.0) as u32, dest.height.round().max(1.0) as u32);
    let content = if svg {
        render_svg(&bytes, dw, dh)
    } else {
        raster_to_pixmap(&bytes, dw, dh)
    };
    let Some(content) = content else { return false };

    // `Cover`/`None` can size the image past the box; clip it to the box so it
    // does not paint over neighboring content (CSS clips object-fit to the
    // content box). `Fill`/`Contain`/`scale-down` always fit inside the box, so
    // they take the faster unclipped path.
    let clip = if dest.width > rect.width + 0.5 || dest.height > rect.height + 0.5 {
        box_clip_mask(pixmap.width(), pixmap.height(), rect)
    } else {
        None
    };
    pixmap.draw_pixmap(
        dest.x as i32,
        dest.y as i32,
        content.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::identity(),
        clip.as_ref(),
    );
    true
}

/// The destination sub-rect for a replaced element's image within its box,
/// given the image's intrinsic `(iw, ih)` size and `object-fit`. Centered in
/// the box; for `Cover`/`None` it can extend past the box edges (the caller
/// clips it). Aspect ratio is preserved for every mode except `Fill`.
fn object_fit_dest(box_rect: &crate::Rect, iw: f32, ih: f32, fit: crate::ObjectFit) -> crate::Rect {
    let (bw, bh) = (box_rect.width, box_rect.height);
    if iw <= 0.0 || ih <= 0.0 {
        return *box_rect;
    }
    let (dw, dh) = match fit {
        crate::ObjectFit::Fill => (bw, bh),
        crate::ObjectFit::Contain => {
            let s = (bw / iw).min(bh / ih);
            (iw * s, ih * s)
        }
        crate::ObjectFit::Cover => {
            let s = (bw / iw).max(bh / ih);
            (iw * s, ih * s)
        }
        crate::ObjectFit::None => (iw, ih),
        crate::ObjectFit::ScaleDown => {
            // min(Contain-size, intrinsic-size): the Contain fit, but never
            // scaled up past the image's own pixels.
            let s = (bw / iw).min(bh / ih).min(1.0);
            (iw * s, ih * s)
        }
    };
    crate::Rect {
        x: box_rect.x + (bw - dw) / 2.0,
        y: box_rect.y + (bh - dh) / 2.0,
        width: dw,
        height: dh,
    }
}

/// The intrinsic `(width, height)` of an SVG image from its size/`viewBox`,
/// used to preserve aspect ratio under `object-fit`. Parses the SVG once; the
/// eventual raster re-parses in `render_svg` (only reached for a non-`fill`
/// object-fit on an SVG image, which is rare).
fn svg_intrinsic(bytes: &[u8]) -> Option<(f32, f32)> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size();
    if size.width() > 0.0 && size.height() > 0.0 {
        Some((size.width(), size.height()))
    } else {
        None
    }
}

/// A full-pixmap clip mask admitting only the pixels inside `rect`, used to
/// crop an `object-fit: cover|none` image to its element box.
fn box_clip_mask(pw: u32, ph: u32, rect: &crate::Rect) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(pw, ph)?;
    let r = Rect::from_xywh(rect.x, rect.y, rect.width, rect.height)?;
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    let path = pb.finish()?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

/// Sniff SVG content: either an XML/SVG prolog, or a bare `<svg` root tag
/// (both are valid, and image responses commonly omit the XML declaration).
fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(256)];
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<?xml") || trimmed.starts_with("<svg")
}

/// Rasterize SVG bytes to a `width` x `height` pixmap, scaled to fit (matching
/// how a replaced element like `<img>` sizes its intrinsic content).
fn render_svg(bytes: &[u8], width: u32, height: u32) -> Option<Pixmap> {
    if width == 0 || height == 0 {
        return None;
    }
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &opts).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }
    let mut svg_pixmap = Pixmap::new(width, height)?;
    let transform = Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, transform, &mut svg_pixmap.as_mut());
    Some(svg_pixmap)
}

/// Serialize an inline `<svg>` subtree (rooted at `root`) back to a standalone
/// SVG document string. Emits `<tag attr="v">children</tag>` for the element
/// and every descendant, preserving the root's `viewBox`/`width`/`height` and
/// all `<defs>`/`<symbol>`/`<use>`/`<path>` structure so resvg can rasterize it
/// as a self-contained document. SVG is XML-clean, so there are no HTML
/// void-element or optional-close rules to apply; every element gets an
/// explicit closing tag. The root gains an `xmlns` declaration when it lacks
/// one (common for inline svg, whose namespace is implied by the HTML parser
/// but required for usvg to parse the string on its own).
fn serialize_svg(tree: &DomTree, root: obscura_dom::tree::NodeId) -> String {
    let mut buf = String::new();
    serialize_svg_node(tree, root, true, &mut buf);
    buf
}

fn serialize_svg_node(tree: &DomTree, nid: obscura_dom::tree::NodeId, is_root: bool, buf: &mut String) {
    let node = match tree.get_node(nid) {
        Some(n) => n,
        None => return,
    };
    if let Some(text) = node.text_content_of_text_node() {
        svg_escape_text(text, buf);
        return;
    }
    let name = match node.as_element() {
        Some(n) => n,
        // Document/comment/PI: no tag of its own, emit only element children.
        None => {
            for child in tree.children(nid) {
                serialize_svg_node(tree, child, false, buf);
            }
            return;
        }
    };
    let tag = name.local.as_ref();
    buf.push('<');
    buf.push_str(tag);
    let mut has_xmlns = false;
    if let Some(attrs) = node.attrs() {
        for attr in attrs {
            // Emit the local name only, dropping any prefix (`xlink:href` ->
            // `href`): resvg reads both, and a bare local avoids needing an
            // `xmlns:xlink` declaration in the standalone document.
            let aname = attr.name.local.as_ref();
            if aname == "xmlns" {
                has_xmlns = true;
            }
            buf.push(' ');
            buf.push_str(aname);
            buf.push_str("=\"");
            svg_escape_attr(&attr.value, buf);
            buf.push('"');
        }
    }
    if is_root && !has_xmlns {
        buf.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }
    buf.push('>');
    for child in tree.children(nid) {
        serialize_svg_node(tree, child, false, buf);
    }
    buf.push_str("</");
    buf.push_str(tag);
    buf.push('>');
}

fn svg_escape_text(s: &str, buf: &mut String) {
    for c in s.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '>' => buf.push_str("&gt;"),
            _ => buf.push(c),
        }
    }
}

fn svg_escape_attr(s: &str, buf: &mut String) {
    for c in s.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '"' => buf.push_str("&quot;"),
            _ => buf.push(c),
        }
    }
}

/// Resolve `<use>` elements in an inline `<svg>` subtree that point at an
/// EXTERNAL sprite file (`href`/`xlink:href` = `url#id` with a non-empty url),
/// splicing the referenced symbol into the already-serialized `markup` so resvg
/// can rasterize it. For each distinct external reference, fetch the sprite
/// (once, via the shared image cache and resolving relative urls against
/// `base_url`), pull out the element carrying `id="id"`, inject it inside a
/// `<defs>` right after the opening `<svg>` tag, and rewrite the `<use>` href to
/// a now-local `#id`. Same-document `<use href="#id">` (empty url) is ignored
/// and left to resvg's own resolution. A fetch or parse failure is a silent
/// no-op: the affected `<use>` simply renders nothing, exactly as before.
fn inject_external_sprites(
    tree: &DomTree,
    root: obscura_dom::tree::NodeId,
    base_url: Option<&str>,
    markup: &mut String,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
    sprite_cache: &mut std::collections::HashMap<String, Option<String>>,
) {
    // Distinct external references (full href, url, fragment id), in first-seen
    // order. Dedupe so one symbol referenced by several `<use>` is fetched and
    // injected once (the rewrite below still fixes every occurrence).
    let mut refs: Vec<(String, String, String)> = Vec::new();
    for nid in tree.descendants(root) {
        let Some(node) = tree.get_node(nid) else { continue };
        let Some(el) = node.as_element() else { continue };
        if el.local.as_ref() != "use" {
            continue;
        }
        // `get_attribute` matches by local name, so a single "href" lookup
        // already covers both `href` and `xlink:href`; check the prefixed form
        // too for completeness.
        let Some(href) = node
            .get_attribute("href")
            .or_else(|| node.get_attribute("xlink:href"))
        else {
            continue;
        };
        let Some(hash) = href.find('#') else { continue };
        let (url, frag) = (&href[..hash], &href[hash + 1..]);
        if url.is_empty() || frag.is_empty() {
            // Same-document `#id` (or a malformed ref): nothing to fetch.
            continue;
        }
        let entry = (href.to_string(), url.to_string(), frag.to_string());
        if !refs.contains(&entry) {
            refs.push(entry);
        }
    }
    if refs.is_empty() {
        return;
    }

    let mut defs = String::new();
    let mut rewrites: Vec<(String, String)> = Vec::new();
    for (href, url, frag) in &refs {
        let key = format!("{url}#{frag}");
        let symbol = sprite_cache
            .entry(key)
            .or_insert_with(|| {
                let bytes = fetch_bytes(url, base_url, cache)?;
                let text = String::from_utf8_lossy(&bytes);
                // Drop `xlink:` prefixes in the fetched fragment (resvg reads a
                // bare `href`), matching how the local subtree is serialized and
                // avoiding an undeclared-namespace parse error in the standalone
                // document.
                extract_svg_element_by_id(&text, frag).map(|s| s.replace("xlink:href", "href"))
            })
            .clone();
        let Some(symbol) = symbol else { continue };
        defs.push_str(&symbol);
        rewrites.push((href.clone(), format!("#{frag}")));
    }
    if defs.is_empty() {
        return;
    }

    // Splice the fetched symbols into a `<defs>` immediately after the opening
    // `<svg ...>` tag (the first `>` in the serialized document).
    if let Some(gt) = markup.find('>') {
        markup.insert_str(gt + 1, &format!("<defs>{defs}</defs>"));
    }
    // Point each external `<use>` at the injected local symbol. The serialized
    // href is attribute-escaped, so match against the escaped form.
    for (href, local) in rewrites {
        let from = format!("href=\"{}\"", svg_escape_attr_str(&href));
        let to = format!("href=\"{}\"", svg_escape_attr_str(&local));
        *markup = markup.replace(&from, &to);
    }
}

/// Escape a string for use as an SVG attribute value (`&`, `<`, `"`), returning
/// it as an owned `String` (the buffer-writing `svg_escape_attr` in one call).
fn svg_escape_attr_str(s: &str) -> String {
    let mut buf = String::new();
    svg_escape_attr(s, &mut buf);
    buf
}

/// Pull the element carrying `id="id"` (a `<symbol>`, `<g>`, `<path>`, ...) out
/// of an external sprite document, returned as a verbatim serialized substring
/// (its start tag through the matching end tag, or the self-closing tag alone).
/// A lightweight namespace-agnostic XML scan, not a full parse: usvg would
/// flatten `<symbol>`/`<use>` structure, and we want to re-inject the element
/// unchanged. Returns None when no element has that id.
fn extract_svg_element_by_id(sprite: &str, id: &str) -> Option<String> {
    let mut i = 0usize;
    while i < sprite.len() {
        let rest = &sprite[i..];
        if !rest.starts_with('<') {
            // Advance to the next tag (skips text/whitespace between elements).
            i += rest.find('<')?;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") || rest.starts_with("</") {
            i += rest.find('>').map(|p| p + 1)?;
            continue;
        }
        // A start tag: inner spans between '<' and '>'.
        let gt = i + rest.find('>')?;
        let inner = &sprite[i + 1..gt];
        if tag_attr(inner, "id") == Some(id) {
            if inner.trim_end().ends_with('/') {
                return Some(sprite[i..=gt].to_string());
            }
            let name = tag_name(inner);
            let end = element_end(sprite, gt + 1, name)?;
            return Some(sprite[i..end].to_string());
        }
        i = gt + 1;
    }
    None
}

/// The tag name from a tag's inner text (the bytes between `<` and `>`),
/// dropping any leading `/` of an end tag and stopping at the first whitespace
/// or self-close slash.
fn tag_name(inner: &str) -> &str {
    let inner = inner.trim_start().trim_start_matches('/');
    let end = inner
        .find(|c: char| c.is_ascii_whitespace() || c == '/')
        .unwrap_or(inner.len());
    &inner[..end]
}

/// The value of attribute `want` in a tag's inner text, or None if absent.
/// Matches attribute names whole (so `id` does not match `data-id`/`xml:id`)
/// and handles single/double quoted and bare values.
fn tag_attr<'a>(inner: &'a str, want: &str) -> Option<&'a str> {
    let b = inner.as_bytes();
    let mut i = 0usize;
    // Skip the tag name.
    while i < b.len() && !b[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'/' {
            break;
        }
        let name_start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() && b[i] != b'/' {
            i += 1;
        }
        let name = &inner[name_start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                let vstart = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                let v = &inner[vstart..i.min(b.len())];
                if i < b.len() {
                    i += 1;
                }
                v
            } else {
                let vstart = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'/' {
                    i += 1;
                }
                &inner[vstart..i]
            };
            if name == want {
                return Some(value);
            }
        } else if name == want {
            // Valueless (boolean) attribute.
            return Some("");
        }
    }
    None
}

/// The byte offset just past the `</name>` that closes an element whose content
/// starts at `start`, tracking nesting of same-named tags (e.g. `<g>` inside
/// `<g>`). None if the document ends without a matching close.
fn element_end(sprite: &str, start: usize, name: &str) -> Option<usize> {
    let mut i = start;
    let mut depth = 1usize;
    while i < sprite.len() {
        let rest = &sprite[i..];
        if !rest.starts_with('<') {
            i += rest.find('<')?;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i += rest.find('>').map(|p| p + 1)?;
            continue;
        }
        let gt = i + rest.find('>')?;
        let inner = &sprite[i + 1..gt];
        if rest.starts_with("</") {
            if tag_name(inner) == name {
                depth -= 1;
                if depth == 0 {
                    return Some(gt + 1);
                }
            }
        } else if tag_name(inner) == name && !inner.trim_end().ends_with('/') {
            depth += 1;
        }
        i = gt + 1;
    }
    None
}

/// A full-pixmap alpha mask that is opaque only inside `rect`, used to clip a
/// blit (e.g. an inline svg raster) to an ancestor's overflow-hidden region.
fn rect_clip_mask(width: u32, height: u32, rect: &crate::Rect) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(width, height)?;
    let r = Rect::from_xywh(rect.x, rect.y, rect.width, rect.height)?;
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    let path = pb.finish()?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

/// Paint a `mask-image`: the ubiquitous "colored, scalable icon" pattern,
/// where an SVG shape is used purely as a stencil and tinted by
/// `background-color`/`color` rather than carrying its own colors. Fetches
/// and rasterizes the mask the same way as an ordinary image, then repaints
/// every pixel it covers as `fill`, weighted by the mask's own alpha there
/// (its "coverage"), instead of drawing the mask's own pixel colors.
fn paint_mask(
    src: &str,
    base_url: Option<&str>,
    rect: &crate::Rect,
    fill: [u8; 4],
    pixmap: &mut Pixmap,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let Some(bytes) = fetch_bytes(src, base_url, cache) else { return false };
    let (w, h) = (rect.width as u32, rect.height as u32);
    let mask = if is_svg(&bytes) { render_svg(&bytes, w, h) } else { raster_to_pixmap(&bytes, w, h) };
    let Some(mask) = mask else { return false };

    let recolored = recolor_by_alpha(&mask, fill);
    pixmap.draw_pixmap(
        rect.x as i32,
        rect.y as i32,
        recolored.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    true
}

/// Replace every pixel's color with `fill`, scaling `fill`'s alpha by the
/// source pixel's own alpha (its mask coverage at that point).
fn recolor_by_alpha(src: &Pixmap, fill: [u8; 4]) -> Pixmap {
    let (w, h) = (src.width(), src.height());
    let mut out = Pixmap::new(w, h).expect("non-zero size, already validated by caller");
    let dst = out.pixels_mut();
    for (i, p) in src.pixels().iter().enumerate() {
        let coverage = p.alpha() as u32;
        if coverage == 0 {
            continue;
        }
        let a = (fill[3] as u32 * coverage) / 255;
        let r = (fill[0] as u32 * a) / 255;
        let g = (fill[1] as u32 * a) / 255;
        let b = (fill[2] as u32 * a) / 255;
        dst[i] = tiny_skia::PremultipliedColorU8::from_rgba(r as u8, g as u8, b as u8, a as u8)
            .unwrap_or_else(|| tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_dom::tree_sink::parse_html;

    #[test]
    fn object_fit_contain_and_cover_center_and_preserve_aspect() {
        // A 200x100 box (2:1) with a square 100x100 image, offset so centering
        // is checked against the box origin, not (0,0).
        let box_rect = crate::Rect { x: 10.0, y: 20.0, width: 200.0, height: 100.0 };
        let (iw, ih) = (100.0f32, 100.0f32);

        // Contain: the largest square fitting inside 200x100 is 100x100,
        // letterboxed horizontally and centered.
        let c = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Contain);
        assert!((c.width - 100.0).abs() < 0.01 && (c.height - 100.0).abs() < 0.01, "contain size {:?}", c);
        assert!((c.width / c.height - iw / ih).abs() < 1e-3, "contain preserves aspect: {:?}", c);
        assert!((c.x - 60.0).abs() < 0.01, "contain centered x (10 + (200-100)/2): {}", c.x);
        assert!((c.y - 20.0).abs() < 0.01, "contain centered y (20 + (100-100)/2): {}", c.y);
        // Contain always fits inside the box.
        assert!(c.x >= box_rect.x - 0.01 && c.x + c.width <= box_rect.x + box_rect.width + 0.01);
        assert!(c.y >= box_rect.y - 0.01 && c.y + c.height <= box_rect.y + box_rect.height + 0.01);

        // Cover: the smallest square covering 200x100 is 200x200, centered so
        // it overflows the box vertically (the paint path clips it).
        let v = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Cover);
        assert!((v.width - 200.0).abs() < 0.01 && (v.height - 200.0).abs() < 0.01, "cover size {:?}", v);
        assert!((v.width / v.height - iw / ih).abs() < 1e-3, "cover preserves aspect: {:?}", v);
        assert!((v.x - 10.0).abs() < 0.01, "cover centered x (10 + (200-200)/2): {}", v.x);
        assert!((v.y + 30.0).abs() < 0.01, "cover centered y (20 + (100-200)/2 = -30): {}", v.y);
        // Cover fully covers the box on both axes.
        assert!(v.x <= box_rect.x + 0.01 && v.x + v.width >= box_rect.x + box_rect.width - 0.01);
        assert!(v.y <= box_rect.y + 0.01 && v.y + v.height >= box_rect.y + box_rect.height - 0.01);

        // scale-down never upscales: a 100x100 image in a 200x200 box stays
        // 100x100 (Contain would grow it to 200x200), centered.
        let box2 = crate::Rect { x: 0.0, y: 0.0, width: 200.0, height: 200.0 };
        let sd = object_fit_dest(&box2, iw, ih, crate::ObjectFit::ScaleDown);
        assert!((sd.width - 100.0).abs() < 0.01 && (sd.height - 100.0).abs() < 0.01, "scale-down no upscale: {:?}", sd);
        assert!((sd.x - 50.0).abs() < 0.01 && (sd.y - 50.0).abs() < 0.01, "scale-down centered: {:?}", sd);
        let cn = object_fit_dest(&box2, iw, ih, crate::ObjectFit::Contain);
        assert!((cn.width - 200.0).abs() < 0.01, "contain upscales into the box: {:?}", cn);

        // None uses the intrinsic size regardless of box, centered.
        let n = object_fit_dest(&box2, iw, ih, crate::ObjectFit::None);
        assert!((n.width - 100.0).abs() < 0.01 && (n.height - 100.0).abs() < 0.01, "none intrinsic size: {:?}", n);
        assert!((n.x - 50.0).abs() < 0.01 && (n.y - 50.0).abs() < 0.01, "none centered: {:?}", n);

        // Fill stretches to exactly the box.
        let f = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Fill);
        assert!((f.width - box_rect.width).abs() < 0.01 && (f.height - box_rect.height).abs() < 0.01, "fill: {:?}", f);
        assert!((f.x - box_rect.x).abs() < 0.01 && (f.y - box_rect.y).abs() < 0.01, "fill origin: {:?}", f);
    }

    #[test]
    fn paints_background_color() {
        let tree = parse_html(
            "<html><body><div style=\"background-color: #ff0000; width: 100px; height: 80px\"></div></body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        assert_eq!(pixmap.width(), 200);
        // The red div is laid out at the origin; sample inside it.
        let inside = pixmap.pixel(10, 10).expect("pixel");
        assert!(inside.red() > 200, "expected red bg, got {:?}", inside);
        assert!(inside.green() < 60);
        assert!(inside.blue() < 60);
        // Outside the 100x80 div the page background is white.
        let outside = pixmap.pixel(150, 150).expect("pixel");
        assert_eq!(outside.red(), 255);
        assert_eq!(outside.green(), 255);
        assert_eq!(outside.blue(), 255);
    }

    #[test]
    fn later_element_paints_over_earlier() {
        // A blue div nested inside a red one: both cover the origin, and blue
        // (a descendant, later in tree order) paints over red.
        let tree = parse_html(
            "<html><body>\
             <div style=\"background-color:red; width:100px; height:100px\">\
               <div style=\"background-color:blue; width:50px; height:50px\"></div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let p = pixmap.pixel(5, 5).expect("pixel");
        assert!(p.blue() > 200, "expected blue to paint over red, got {:?}", p);
    }

    #[test]
    fn nested_translate_accumulates_through_subtree() {
        // Parent red box (position:absolute at 0,0, 20x20) translated by
        // (50,60). Child blue box (10x10, in-flow at the red box's origin)
        // translated by an additional (30,0). The child's painted position must
        // be the SUM of both translates, (50+30, 60+0) = (80,60), proving an
        // ancestor's translate offsets the whole subtree on top of the node's
        // own translate.
        let tree = parse_html(
            "<html><body style=\"margin:0\">\
             <div style=\"position:relative; width:200px; height:200px\">\
               <div style=\"position:absolute; top:0; left:0; width:20px; height:20px; \
                            background:#ff0000; transform:translate(50px,60px)\">\
                 <div style=\"width:10px; height:10px; background:#0000ff; \
                              transform:translate(30px,0)\"></div>\
               </div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        // Child blue lands at (80..90, 60..70).
        let blue = pixmap.pixel(85, 65).expect("pixel");
        assert!(
            blue.blue() > 200 && blue.red() < 60,
            "expected blue child at accumulated offset (80,60), got {:?}",
            blue
        );
        // Parent red lands at (50..70, 60..80); sample where the blue child does
        // not cover.
        let red = pixmap.pixel(55, 75).expect("pixel");
        assert!(
            red.red() > 200 && red.blue() < 60,
            "expected red parent at its own translate (50,60), got {:?}",
            red
        );
        // Nothing painted at the pre-transform origin: both boxes moved away.
        let origin = pixmap.pixel(5, 5).expect("pixel");
        assert_eq!((origin.red(), origin.green(), origin.blue()), (255, 255, 255));
    }

    #[test]
    fn translate_offscreen_box_is_not_painted() {
        // translate(-10000px,0) shoves the box far off the left edge (the old
        // hidden skip-link idiom); it must not paint anywhere on the canvas.
        let tree = parse_html(
            "<html><body>\
             <div style=\"position:absolute; top:0; left:0; width:50px; height:50px; \
                          background:#ff0000; transform:translate(-10000px,0)\"></div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut any_red = false;
        'scan: for y in 0..200 {
            for x in 0..200 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.red() > 200 && p.green() < 60 && p.blue() < 60 {
                    any_red = true;
                    break 'scan;
                }
            }
        }
        assert!(!any_red, "translate(-10000px,0) box should be off-screen and unpainted");
    }

    #[test]
    fn translate_percent_centers_absolute_box() {
        // The canonical centering idiom: an absolutely-positioned box at
        // top:50%/left:50% of its containing block pulled back by
        // translate(-50%,-50%) of its own size centers within it. In a 200x200
        // container a 40x40 box centers at (100,100), so its border box (with
        // top-left at 100,100 before the transform) becomes (80..120, 80..120).
        let tree = parse_html(
            "<html><body style=\"margin:0\">\
             <div style=\"position:relative; width:200px; height:200px\">\
               <div style=\"position:absolute; top:50%; left:50%; width:40px; height:40px; \
                            background:#ff0000; transform:translate(-50%,-50%)\"></div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let center = pixmap.pixel(100, 100).expect("pixel");
        assert!(center.red() > 200 && center.blue() < 60, "expected centered red box, got {:?}", center);
        // Just outside the centered box stays white.
        let outside = pixmap.pixel(70, 70).expect("pixel");
        assert_eq!((outside.red(), outside.green(), outside.blue()), (255, 255, 255));
    }

    #[test]
    fn paints_text_color() {
        let tree = parse_html(
            "<html><body><div style=\"color: #00ff00; width: 100px; height: 100px\">Hello</div></body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_green = false;
        for y in 0..200 {
            for x in 0..200 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.green() > 200 && p.red() < 50 && p.blue() < 50 {
                    found_green = true;
                    break;
                }
            }
            if found_green { break; }
        }
        assert!(found_green, "expected green text to be painted");
    }

    #[test]
    fn serializes_inline_svg_subtree() {
        // A sprite-style svg: a <use> that references a <symbol> in the same
        // document must survive serialization so resvg can resolve it.
        let tree = parse_html(
            r##"<html><body><svg viewBox="0 0 10 10"><use href="#a"/><symbol id="a"><path d="M0 0h10v10z"/></symbol></svg></body></html>"##,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let out = serialize_svg(&tree, svg);
        assert!(out.starts_with("<svg"), "root svg tag: {out}");
        assert!(out.contains(r#"viewBox="0 0 10 10""#), "viewBox preserved: {out}");
        assert!(out.contains(r#"xmlns="http://www.w3.org/2000/svg""#), "xmlns injected: {out}");
        assert!(out.contains("<use") && out.contains(r##"href="#a""##), "use + href: {out}");
        assert!(out.contains("<symbol") && out.contains(r#"id="a""#), "symbol id: {out}");
        assert!(out.contains("<path") && out.contains("</path>"), "path opened + closed: {out}");
        assert!(out.trim_end().ends_with("</svg>"), "root closed: {out}");
        // The serialized string parses as a standalone SVG document.
        let opts = usvg::Options::default();
        assert!(
            usvg::Tree::from_data(out.as_bytes(), &opts).is_ok(),
            "usvg should parse serialized svg: {out}",
        );
    }

    #[test]
    fn injects_xmlns_only_when_absent() {
        let tree = parse_html(
            r#"<html><body><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="4" height="4"/></svg></body></html>"#,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let out = serialize_svg(&tree, svg);
        assert_eq!(out.matches("xmlns=").count(), 1, "no duplicate xmlns: {out}");
    }

    #[test]
    fn paints_inline_svg() {
        // The <rect> inside an inline svg must rasterize (it is not an <img>).
        let tree = parse_html(
            r##"<html><body><svg width="40" height="40" viewBox="0 0 40 40"><rect x="0" y="0" width="40" height="40" fill="#ff0000"/></svg></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_red = false;
        'outer: for y in 0..80 {
            for x in 0..80 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.red() > 200 && p.green() < 60 && p.blue() < 60 {
                    found_red = true;
                    break 'outer;
                }
            }
        }
        assert!(found_red, "expected inline svg <rect> to paint red");
    }

    #[test]
    fn paints_inline_svg_use_reference() {
        // The icon-sprite pattern: <use href="#id"> resolves against a <defs>
        // element in the same svg only because the whole subtree is serialized
        // and handed to resvg as one document.
        let tree = parse_html(
            r##"<html><body><svg width="40" height="40" viewBox="0 0 40 40"><defs><rect id="a" width="40" height="40" fill="#0000ff"/></defs><use href="#a"/></svg></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_blue = false;
        'outer: for y in 0..80 {
            for x in 0..80 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.blue() > 200 && p.red() < 60 && p.green() < 60 {
                    found_blue = true;
                    break 'outer;
                }
            }
        }
        assert!(found_blue, "expected <use> to instantiate the referenced <rect>");
    }

    #[test]
    fn extracts_symbol_by_id_from_sprite() {
        // The external-sprite core: given a fetched sprite, pull out just the
        // referenced <symbol> verbatim so it can be spliced into the local svg.
        let sprite = r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><symbol id="a" viewBox="0 0 10 10"><path d="M0 0h10v10z"/></symbol><symbol id="b"><rect width="4" height="4"/></symbol></defs></svg>"##;
        let out = extract_svg_element_by_id(sprite, "a").expect("symbol a found");
        assert!(out.starts_with("<symbol"), "starts at the symbol tag: {out}");
        assert!(out.contains(r#"id="a""#), "keeps the id: {out}");
        assert!(out.contains("<path") && out.contains("h10v10z"), "keeps children: {out}");
        assert!(out.trim_end().ends_with("</symbol>"), "closed at matching end: {out}");
        assert!(!out.contains(r#"id="b""#), "stops before the sibling symbol: {out}");
        assert!(!out.contains("<rect"), "no sibling content leaks in: {out}");
    }

    #[test]
    fn extract_handles_self_closing_nesting_and_absent() {
        // A self-closing element carrying the id returns just that tag.
        let s1 = r#"<svg><rect id="x" width="4" height="4"/></svg>"#;
        assert_eq!(
            extract_svg_element_by_id(s1, "x").as_deref(),
            Some(r#"<rect id="x" width="4" height="4"/>"#),
        );
        // Same-name nesting: the matching close is the outer one, not the inner.
        let s2 = r#"<svg><g id="grp"><g><path/></g></g></svg>"#;
        assert_eq!(
            extract_svg_element_by_id(s2, "grp").as_deref(),
            Some(r#"<g id="grp"><g><path/></g></g>"#),
        );
        // `data-id` / a missing id must not be mistaken for `id`.
        let s3 = r#"<svg><symbol data-id="a"><path/></symbol></svg>"#;
        assert!(extract_svg_element_by_id(s3, "a").is_none(), "data-id is not id");
        assert!(extract_svg_element_by_id(s2, "nope").is_none(), "absent id");
    }

    #[test]
    fn same_document_use_left_unchanged_by_inject() {
        // A same-document `<use href="#a">` has an empty url, so inject does no
        // network work and leaves the serialized markup byte-for-byte unchanged.
        let tree = parse_html(
            r##"<html><body><svg viewBox="0 0 10 10"><use href="#a"/><symbol id="a"><path d="M0 0h10v10z"/></symbol></svg></body></html>"##,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let mut markup = serialize_svg(&tree, svg);
        let before = markup.clone();
        let mut cache = std::collections::HashMap::new();
        let mut sprite_cache = std::collections::HashMap::new();
        inject_external_sprites(&tree, svg, None, &mut markup, &mut cache, &mut sprite_cache);
        assert_eq!(markup, before, "same-document use must be untouched");
    }
}
