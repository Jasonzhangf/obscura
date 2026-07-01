//! Paint: rasterize the laid-out DOM into a [`tiny_skia::Pixmap`].
//!
//! Phase 5a. Fills each element's border box with its background color over a
//! white page. Text rendering arrives with the text step; borders and images
//! are later enhancements. Pure Rust (tiny-skia, CPU), deterministic, no system
//! dependencies, so a screenshot is reproducible across hosts.

use obscura_dom::tree::DomTree;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};

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
}
