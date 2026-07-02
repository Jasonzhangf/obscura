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

use crate::{compute_style, layout_dom};

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

        let bg = match bg_color_for(tree, nid) {
            Some(c) => c,
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
        let mut path = PathBuilder::new();
        path.push_rect(box_rect);
        let path = match path.finish() {
            Some(p) => p,
            None => continue,
        };
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
        paint.anti_alias = false;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    Some(pixmap)
}

fn bg_color_for(tree: &DomTree, nid: obscura_dom::tree::NodeId) -> Option<[u8; 4]> {
    let node = tree.get_node(nid)?;
    let name = node.as_element()?;
    let style = compute_style(name.local.as_ref(), node.get_attribute("style"));
    style.background_color
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
        obscura_dom::tree::NodeData::Text { contents } => contents,
        _ => return None,
    };
    
    // Find parent element to get geometry and color
    let parent_id = node.parent?;
    let parent = tree.get_node(parent_id)?;
    let parent_name = parent.as_element()?;
    
    let rect = laid.rects.get(&parent_id)?;
    let style = crate::compute_style(parent_name.local.as_ref(), parent.get_attribute("style"));
    // Default color to black if not specified.
    let color = style.color.unwrap_or([0, 0, 0, 255]);
    
    // Paint text at rect.x, rect.y
    draw_text(pixmap, text, rect.x, rect.y, color);
    Some(())
}

fn draw_text(pixmap: &mut Pixmap, text: &str, x: f32, y: f32, color: [u8; 4]) {
    let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
    let scale = PxScale::from(16.0); // Default font size
    let scaled_font = font.as_scaled(scale);
    let mut caret = ab_glyph::point(x, y + scaled_font.ascent());

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
                if px >= 0 && px < pixmap.width() as i32 && py >= 0 && py < pixmap.height() as i32 {
                    let alpha = (color[3] as f32 * c) as u8;
                    if alpha > 0 {
                        blend_pixel(pixmap, px as u32, py as u32, color[0], color[1], color[2], alpha);
                    }
                }
            });
            caret.x += scaled_font.h_advance(id);
        } else {
            caret.x += scaled_font.h_advance(id);
        }
    }
}

fn blend_pixel(pixmap: &mut Pixmap, x: u32, y: u32, r: u8, g: u8, b: u8, a: u8) {
    let width = pixmap.width();
    let pixels = pixmap.pixels_mut();
    let idx = (y * width + x) as usize;
    let dst = pixels[idx];
    
    // source pre-multiplied
    let src_a = a as u32;
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
