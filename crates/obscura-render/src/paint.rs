//! Paint: rasterize the laid-out DOM into a [`tiny_skia::Pixmap`].
//!
//! Phase 5a. Fills each element's border box with its background color over a
//! white page. Text rendering arrives with the text step; borders and images
//! are later enhancements. Pure Rust (tiny-skia, CPU), deterministic, no system
//! dependencies, so a screenshot is reproducible across hosts.

use obscura_dom::tree::DomTree;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

static FONT_BYTES: &[u8] = include_bytes!("../assets/dejavu-sans.ttf");

use crate::layout_dom;

/// Render `tree` at `viewport` (width, height) in CSS pixels to a Pixmap, or
/// None if the viewport is zero-sized. `base_url`, when given, resolves the
/// relative image URLs (`<img src="logo.svg">`) that make up the overwhelming
/// majority of real-world markup; without it only absolute and `data:` URLs
/// can be fetched.
pub fn paint_dom(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Pixmap> {
    let (w, h) = (viewport.0 as u32, viewport.1 as u32);
    let mut pixmap = Pixmap::new(w, h)?;
    pixmap.fill(Color::WHITE);

    let laid = layout_dom(tree, viewport);
    // The same URL (an icon sprite, a repeated background image) commonly
    // backs many elements on one page; fetch each distinct URL at most once
    // per screenshot. `None` caches a failed fetch too, so a broken image
    // reference does not retry on every element that references it.
    let mut image_cache: std::collections::HashMap<String, Option<Vec<u8>>> = std::collections::HashMap::new();
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

        if let Some(bg) = style.background_color {
            let mut path = PathBuilder::new();
            path.push_rect(box_rect);
            if let Some(path) = path.finish() {
                let mut paint = Paint::default();
                paint.set_color(Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
                paint.anti_alias = false;
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }

        if let Some(bg_url) = &style.background_image {
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

        if style.border.top > 0.0 || style.border.right > 0.0 || style.border.bottom > 0.0 || style.border.left > 0.0 {
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
            if let Some(src) = node.get_attribute("src") {
                let painted = paint_image(src, base_url, &rect, &mut pixmap, &mut image_cache);
                // Fall back to alt text only when the image itself did not paint.
                if !painted {
                    if let Some(alt) = node.get_attribute("alt") {
                        if !alt.is_empty() {
                            draw_text(&mut pixmap, alt, rect.x, rect.y, [0, 0, 0, 255], 12.0, false, clip);
                        }
                    }
                }
            }
        }
    }
    Some(pixmap)
}

/// Render `tree` at `viewport` to PNG bytes (RGBA 8-bit). Returns None if the
/// viewport is zero-sized. Convenience over `paint_dom` + `encode_png`.
pub fn screenshot_png(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Vec<u8>> {
    paint_dom(tree, viewport, base_url)?.encode_png().ok()
}

fn paint_text_node(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    laid: &crate::DomLayout,
    pixmap: &mut Pixmap,
) -> Option<()> {
    let node = tree.get_node(nid)?;
    let mut text_buf = String::new();
    let text = match &node.data {
        obscura_dom::tree::NodeData::Text { contents } => {
            let mut in_space = false;
            for c in contents.chars() {
                if c.is_whitespace() {
                    if !in_space {
                        text_buf.push(' ');
                        in_space = true;
                    }
                } else {
                    text_buf.push(c);
                    in_space = false;
                }
            }
            if text_buf.is_empty() {
                " "
            } else {
                &text_buf
            }
        },
        _ => return None,
    };
    
    let parent = node.parent?;
    let style = laid.styles.get(&parent)?;
    let color = style.color.unwrap_or([0, 0, 0, 255]);
    let fsize = style.font_size.unwrap_or(16.0);
    
    let is_bold = style.font_weight.as_deref() == Some("bold");
    
    // Get geometry of the text node itself!
    let rect = match laid.rects.get(&nid) {
        Some(r) => r,
        None => return None,
    };

    let clip = laid.clip_rects.get(&nid).copied().flatten();
    draw_text(pixmap, text, rect.x, rect.y, color, fsize, is_bold, clip);
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
            caret.x += scaled_font.h_advance(id);
        } else {
            caret.x += scaled_font.h_advance(id);
        }
    }
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

    let bytes = if src.starts_with("data:image/") {
        if let Some(comma_idx) = src.find(',') {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(&src[comma_idx + 1..]).ok()
        } else { None }
    } else {
        // Resolve relative to the document's base URL: the overwhelming
        // majority of real markup uses relative image paths ("logo.svg", not
        // "https://example.com/logo.svg"), so without this every relative
        // <img> silently fails to fetch, not just SVGs.
        let resolved = if src.starts_with("http://") || src.starts_with("https://") {
            Some(src.to_string())
        } else {
            base_url
                .and_then(|b| url::Url::parse(b).ok())
                .and_then(|base| base.join(src).ok())
                .map(|u| u.to_string())
        };
        match resolved {
            // The same icon/sprite/background is routinely referenced by
            // dozens of elements on one page (every story's vote arrow, every
            // repeated logo); fetch each distinct URL over the network once
            // per screenshot rather than once per element.
            Some(url) => cache
                .entry(url.clone())
                .or_insert_with(|| {
                    ureq::get(&url).call().ok().and_then(|resp| {
                        let mut buf = Vec::new();
                        use std::io::Read;
                        resp.into_reader().read_to_end(&mut buf).ok()?;
                        Some(buf)
                    })
                })
                .clone(),
            None => None,
        }
    };

    let Some(bytes) = bytes else { return false };

    if is_svg(&bytes) {
        if let Some(svg_pixmap) = render_svg(&bytes, rect.width as u32, rect.height as u32) {
            pixmap.draw_pixmap(
                rect.x as i32,
                rect.y as i32,
                svg_pixmap.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            return true;
        }
        return false;
    }

    if let Ok(img) = image::load_from_memory(&bytes) {
        let img = img.to_rgba8();
        let resized = image::imageops::resize(&img, rect.width as u32, rect.height as u32, image::imageops::FilterType::Triangle);

        let mut raw = resized.into_raw();
        for pixel in raw.chunks_exact_mut(4) {
            let a = pixel[3] as u32;
            pixel[0] = ((pixel[0] as u32 * a) / 255) as u8;
            pixel[1] = ((pixel[1] as u32 * a) / 255) as u8;
            pixel[2] = ((pixel[2] as u32 * a) / 255) as u8;
        }
        if let Some(size) = tiny_skia::IntSize::from_wh(rect.width as u32, rect.height as u32) {
            if let Some(img_pixmap) = Pixmap::from_vec(raw, size) {
                pixmap.draw_pixmap(
                    rect.x as i32,
                    rect.y as i32,
                    img_pixmap.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                return true;
            }
        }
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
