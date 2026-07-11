//! Paint: rasterize the laid-out DOM into a [`tiny_skia::Pixmap`].
//!
//! Phase 5a. Fills each element's border box with its background color over a
//! white page. Text rendering arrives with the text step; borders and images
//! are later enhancements. Pure Rust (tiny-skia, CPU), deterministic, no system
//! dependencies, so a screenshot is reproducible across hosts.

use obscura_dom::tree::DomTree;
use tiny_skia::{Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, SpreadMode, Transform};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

static FONT_BYTES: &[u8] = include_bytes!("../assets/dejavu-sans.ttf");

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
    // Tree order so later elements paint over earlier ones (normal flow).
    for nid in tree.descendants(tree.document()) {
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
            Some(r) => r,
            None => continue,
        };

        let style = match laid.styles.get(&nid) {
            Some(s) => s,
            None => continue,
        };

        if style.effectively_invisible {
            continue;
        }

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
            None => *rect,
        };
        let box_rect = match Rect::from_xywh(visible_rect.x, visible_rect.y, visible_rect.width, visible_rect.height) {
            Some(r) => r,
            None => continue,
        };

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
        if style.mask_image.is_none() {
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
                None => *rect,
            };
            let img_rect = match clip {
                Some(c) => img_rect.intersect(&c),
                None => Some(img_rect),
            };
            if let Some(img_rect) = img_rect {
                paint_image(bg_url, base_url, &img_rect, &mut pixmap, &mut image_cache);
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
            if let Some(src) = resolve_img_url(tree, nid) {
                let painted = paint_image(&src, base_url, &rect, &mut pixmap, &mut image_cache);
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
                draw_text(&mut pixmap, word, word_rect.x, word_rect.y, color, fsize, is_bold, clip);
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
        if let Some(&idx) = laid.ifc_items.get(&nid) {
            if laid.styles.get(&nid).map(|s| s.effectively_invisible).unwrap_or(false) {
                continue;
            }
            laid.text_engine.paint_item(idx, &mut pixmap);
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
    let color = style.color.unwrap_or([0, 0, 0, 255]);
    let fsize = style.font_size.unwrap_or(16.0);
    let is_bold = style.font_weight.as_deref() == Some("bold");
    let clip = laid.clip_rects.get(&nid).copied().flatten();

    for (rect, word) in runs {
        draw_text(pixmap, word, rect.x, rect.y, color, fsize, is_bold, clip);
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
            // 429 (rate limit) and 5xx are transient: back off and retry.
            Err(ureq::Error::Status(code, _)) if matches!(code, 429 | 500 | 502 | 503 | 504) && attempt < 2 => {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            // A network/transport error is also worth one more try.
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
        let Some(url) = resolve_img_url(tree, nid) else { continue };
        let Some(bytes) = fetch_bytes(&url, base_url, cache) else { continue };
        if let Some((w, h)) = image_dimensions(&bytes) {
            if w > 0 && h > 0 {
                out.insert(nid, (w as f32, h as f32));
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
fn resolve_img_url(tree: &DomTree, nid: obscura_dom::tree::NodeId) -> Option<String> {
    let node = tree.get_node(nid)?;
    // A <picture>'s preceding, type/media-matching <source> wins over the
    // <img>'s own attributes (HTML "update the source set").
    if let Some(url) = picture_source_url(tree, nid) {
        return Some(url);
    }
    let sizes = node.get_attribute("sizes");
    for a in ["srcset", "data-srcset"] {
        if let Some(v) = node.get_attribute(a) {
            if let Some(u) = best_srcset_candidate(v, sizes) {
                return Some(u);
            }
        }
    }
    let url_attrs = ["src", "data-src", "data-lazy-src", "data-original", "data-fallback-src", "data-lazy"];
    // A non-inline URL first (a data: src is usually the lazy-load placeholder).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() && !v.starts_with("data:") {
                return Some(v.to_string());
            }
        }
    }
    // Otherwise fall back to whatever is there (an inlined data: image).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
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
fn picture_source_url(tree: &DomTree, img_nid: obscura_dom::tree::NodeId) -> Option<String> {
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
fn best_srcset_candidate(srcset: &str, sizes: Option<&str>) -> Option<String> {
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
        .map(|(_, u)| u.clone())
        .unwrap_or_else(|| cands.last().unwrap().1.clone());
    Some(pick)
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
    pixmap: &mut Pixmap,
    cache: &mut std::collections::HashMap<String, Option<Vec<u8>>>,
) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let Some(bytes) = fetch_bytes(src, base_url, cache) else { return false };

    let content = if is_svg(&bytes) {
        render_svg(&bytes, rect.width as u32, rect.height as u32)
    } else {
        raster_to_pixmap(&bytes, rect.width as u32, rect.height as u32)
    };
    if let Some(content) = content {
        pixmap.draw_pixmap(
            rect.x as i32,
            rect.y as i32,
            content.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        return true;
    }
    false
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
}
