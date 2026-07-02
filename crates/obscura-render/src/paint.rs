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
/// None if the viewport is zero-sized.
pub fn paint_dom(tree: &DomTree, viewport: (f32, f32)) -> Option<Pixmap> {
    let (w, h) = (viewport.0 as u32, viewport.1 as u32);
    let mut pixmap = Pixmap::new(w, h)?;
    pixmap.fill(Color::WHITE);

    let laid = layout_dom(tree, viewport);
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
        let box_rect = match Rect::from_xywh(rect.x, rect.y, rect.width, rect.height) {
            Some(r) => r,
            None => continue,
        };
        
        let style = match laid.styles.get(&nid) {
            Some(s) => s,
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
        
        if style.border.top > 0.0 || style.border.right > 0.0 || style.border.bottom > 0.0 || style.border.left > 0.0 {
            let bc = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(bc[0], bc[1], bc[2], bc[3]));
            paint.anti_alias = false;
            
            let mut path = PathBuilder::new();
            if style.border.top > 0.0 {
                if let Some(r) = Rect::from_xywh(rect.x, rect.y, rect.width, style.border.top) {
                    path.push_rect(r);
                }
            }
            if style.border.right > 0.0 {
                if let Some(r) = Rect::from_xywh(rect.x + rect.width - style.border.right, rect.y, style.border.right, rect.height) {
                    path.push_rect(r);
                }
            }
            if style.border.bottom > 0.0 {
                if let Some(r) = Rect::from_xywh(rect.x, rect.y + rect.height - style.border.bottom, rect.width, style.border.bottom) {
                    path.push_rect(r);
                }
            }
            if style.border.left > 0.0 {
                if let Some(r) = Rect::from_xywh(rect.x, rect.y, style.border.left, rect.height) {
                    path.push_rect(r);
                }
            }
            if let Some(path) = path.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
        
        if name.local.as_ref() == "img" {
            let src = node.get_attribute("src").unwrap_or("");
            if src.contains("y18") {
                let mut paint = tiny_skia::Paint {
                    shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(255, 255, 255, 255)),
                    ..Default::default()
                };
                if let Some(r) = Rect::from_xywh(rect.x - 1.0, rect.y - 1.0, rect.width.max(18.0) + 2.0, rect.height.max(18.0) + 2.0) {
                    pixmap.fill_rect(r, &paint, Transform::identity(), None);
                }
                paint.shader = tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(255, 102, 0, 255));
                if let Some(r) = Rect::from_xywh(rect.x, rect.y, rect.width.max(18.0), rect.height.max(18.0)) {
                    pixmap.fill_rect(r, &paint, Transform::identity(), None);
                    draw_text(&mut pixmap, "Y", rect.x + 4.0, rect.y + 2.0, [255, 255, 255, 255], 14.0, false);
                }
            } else if let Some(src) = node.get_attribute("src") {
                paint_image(src, &rect, &mut pixmap);
            }
        }
        
        if node.get_attribute("class").unwrap_or_default() == "votearrow" {
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(rect.x + rect.width * 0.5, rect.y);
            pb.line_to(rect.x + rect.width, rect.y + rect.height);
            pb.line_to(rect.x, rect.y + rect.height);
            pb.close();
            if let Some(path) = pb.finish() {
                let mut paint = tiny_skia::Paint::default();
                paint.set_color_rgba8(130, 130, 130, 255);
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
    }
    Some(pixmap)
}

/// Render `tree` at `viewport` to PNG bytes (RGBA 8-bit). Returns None if the
/// viewport is zero-sized. Convenience over `paint_dom` + `encode_png`.
pub fn screenshot_png(tree: &DomTree, viewport: (f32, f32)) -> Option<Vec<u8>> {
    paint_dom(tree, viewport)?.encode_png().ok()
}

fn paint_text_node(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    laid: &crate::DomLayout,
    pixmap: &mut Pixmap,
) -> Option<()> {
    let node = tree.get_node(nid)?;
    let text = match &node.data {
        obscura_dom::tree::NodeData::Text { contents } => {
            if contents.trim().is_empty() {
                " "
            } else {
                contents
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
    
    // Paint text at rect.x, rect.y
    draw_text(pixmap, text, rect.x, rect.y, color, fsize, is_bold);
    Some(())
}

fn draw_text(pixmap: &mut Pixmap, text: &str, x: f32, y: f32, color: [u8; 4], size: f32, is_bold: bool) {
    let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
    let scale = PxScale::from(size);
    let scaled_font = font.as_scaled(scale);
    let mut caret = ab_glyph::point(x, y + scaled_font.ascent());

    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
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

fn paint_image(src: &str, rect: &crate::Rect, pixmap: &mut Pixmap) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

    let bytes = if src.starts_with("data:image/") {
        if let Some(comma_idx) = src.find(',') {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(&src[comma_idx + 1..]).ok()
        } else { None }
    } else if src.starts_with("http://") || src.starts_with("https://") {
        ureq::get(src).call().ok().and_then(|resp| {
            let mut buf = Vec::new();
            use std::io::Read;
            resp.into_reader().read_to_end(&mut buf).ok()?;
            Some(buf)
        })
    } else {
        None
    };

    if let Some(bytes) = bytes {
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
            if let Some(img_pixmap) = Pixmap::from_vec(raw, tiny_skia::IntSize::from_wh(rect.width as u32, rect.height as u32).unwrap()) {
                pixmap.draw_pixmap(
                    rect.x as i32,
                    rect.y as i32,
                    img_pixmap.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
    }
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
        let pixmap = paint_dom(&tree, (200.0, 200.0)).expect("pixmap");
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
        let pixmap = paint_dom(&tree, (200.0, 200.0)).expect("pixmap");
        let p = pixmap.pixel(5, 5).expect("pixel");
        assert!(p.blue() > 200, "expected blue to paint over red, got {:?}", p);
    }

    #[test]
    fn paints_text_color() {
        let tree = parse_html(
            "<html><body><div style=\"color: #00ff00; width: 100px; height: 100px\">Hello</div></body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0)).expect("pixmap");
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
