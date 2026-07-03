//! DOM integration: build a taffy layout tree from a live [`DomTree`], run
//! layout, and return border-box geometry keyed by [`NodeId`].
//!
//! Phase 3. Text nodes do not yet contribute to size (no inline/text layout
//! until the text/paint phase), so a leaf element with only text may have zero
//! height. Block and flex structure, plus explicit sizes and box model, are
//! correct.

use std::collections::HashMap;

use obscura_dom::tree::{DomTree, NodeId};
use taffy::prelude::*;

use crate::{Rect, to_taffy_style};

/// Text width for layout. With the `paint` feature this is exact (real glyph
/// metrics from the embedded font, shared with rasterization). Without it
/// (layout-only builds, e.g. for `getBoundingClientRect`), fall back to a
/// standard average-character-width heuristic for proportional fonts so
/// `obscura-render` compiles and gives reasonable geometry with its default
/// (lightest-weight) feature set, matching RENDER.md's documented "layout
/// (default)" mode.
#[cfg(feature = "paint")]
fn text_width(text: &str, size: f32, is_bold: bool) -> f32 {
    crate::paint::measure_text(text, size, is_bold)
}

#[cfg(not(feature = "paint"))]
fn text_width(text: &str, size: f32, is_bold: bool) -> f32 {
    const AVG_CHAR_WIDTH_EM: f32 = 0.55;
    let chars = text.chars().filter(|c| !c.is_control()).count() as f32;
    let width = chars * size * AVG_CHAR_WIDTH_EM;
    if is_bold { width * 1.08 } else { width }
}

/// Per-element border boxes after layout, in viewport coordinates.
pub struct DomLayout {
    pub rects: HashMap<NodeId, Rect>,
    pub styles: HashMap<NodeId, crate::LayoutStyle>,
    /// The clip rect inherited from ancestor `overflow: hidden` boxes, keyed
    /// per node. `None` means unclipped. Does not include the node's own
    /// overflow (that only clips its children, not itself).
    pub clip_rects: HashMap<NodeId, Option<Rect>>,
}

/// Walk the tree top-down accumulating the clip rect imposed by ancestor
/// `overflow: hidden` boxes. Must run after layout, since it needs border
/// boxes. This is what makes `overflow: hidden` (used pervasively for the
/// "visually hidden but accessible" pattern: a 1x1 absolutely-positioned box
/// with clipped overflow) actually hide its contents instead of letting text
/// paint wherever the box's static position happens to land.
fn resolve_clip_rects(
    tree: &DomTree,
    id: NodeId,
    inherited: Option<Rect>,
    rects: &HashMap<NodeId, Rect>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    clip_rects: &mut HashMap<NodeId, Option<Rect>>,
) {
    clip_rects.insert(id, inherited);
    let next = match (styles.get(&id), rects.get(&id)) {
        (Some(style), Some(rect)) if style.overflow_hidden => {
            Some(match inherited {
                Some(clip) => clip.intersect(rect).unwrap_or(Rect::default()),
                None => *rect,
            })
        }
        _ => inherited,
    };
    for cid in tree.children(id) {
        resolve_clip_rects(tree, cid, next, rects, styles, clip_rects);
    }
}

/// Compute the UA + author style for every element in preorder, maintaining
/// `matcher`'s ancestor filter as we descend so descendant-combinator rules
/// fast-reject correctly. Non-element nodes (text, comments) are skipped but
/// still walked through, since an element may be their descendant.
fn cascade_walk(
    tree: &DomTree,
    id: NodeId,
    sheet: &crate::css::Stylesheet,
    matcher: &mut obscura_dom::selector::Matcher,
    styles: &mut HashMap<NodeId, crate::LayoutStyle>,
) {
    let Some(node) = tree.get_node(id) else { return };
    let is_element = node.as_element().map(|elem| {
        let mut style = crate::style::ua_style(elem.local.as_ref());
        if !sheet.is_empty() {
            let node_id = node.get_attribute("id");
            let classes: Vec<String> = node
                .get_attribute("class")
                .map(|c| c.split_whitespace().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            sheet.apply(tree, matcher, id, node_id, &classes, elem.local.as_ref(), &mut style);
        }
        styles.insert(id, style);
    });

    if is_element.is_some() {
        matcher.push_ancestor(tree, id);
    }
    for cid in tree.children(id) {
        cascade_walk(tree, cid, sheet, matcher, styles);
    }
    if is_element.is_some() {
        matcher.pop_ancestor();
    }
}

/// Lay out a DOM tree within `viewport` (width, height) in CSS pixels.
pub fn layout_dom(tree: &DomTree, viewport: (f32, f32)) -> DomLayout {
    let timing = std::env::var("OBSCURA_RENDER_TIMING").is_ok();

    // Collect the text of every <style> block in document order.
    let mut css_sources = Vec::new();
    for nid in tree.descendants(tree.document()) {
        if let Some(node) = tree.get_node(nid) {
            if let Some(elem) = node.as_element() {
                if elem.local.as_ref() == "style" {
                    css_sources.push(tree.text_content(nid));
                }
            }
        }
    }

    let t0 = std::time::Instant::now();
    let sheet = crate::css::Stylesheet::parse(tree, &css_sources);
    let t_parse = t0.elapsed();

    let t1 = std::time::Instant::now();
    let mut matcher = tree.matcher();
    let mut styles: HashMap<NodeId, crate::LayoutStyle> = HashMap::new();
    // A real preorder walk (not a flat descendants() scan) so the matcher's
    // ancestor bloom filter tracks the current path: push before recursing
    // into children, pop on the way back out. This is what lets descendant
    // combinators (".mw-body .firstHeading") fast-reject via the filter
    // instead of falling back to the always-true "can't reject" case.
    cascade_walk(tree, tree.document(), &sheet, &mut matcher, &mut styles);
    if timing {
        let (r, i, c, l, u) = sheet.debug_stats();
        eprintln!("[timing] parse+index={:?} cascade={:?} rules={} id_keys={} class_keys={} local_keys={} universal={}", t_parse, t1.elapsed(), r, i, c, l, u);
    }
    for nid in tree.descendants(tree.document()) {
        if let Some(node) = tree.get_node(nid) {
            if node.is_element() {
                if let Some(style) = styles.get_mut(&nid) {
                    if let Some(inline) = node.get_attribute("style") {
                        crate::style::apply_inline(style, inline);
                    }
                    if let Some(color) = node.get_attribute("color") {
                        crate::style::apply_inline(style, &format!("color: {}", color));
                    }
                    if let Some(bgcolor) = node.get_attribute("bgcolor") {
                        crate::style::apply_inline(style, &format!("background-color: {}", bgcolor));
                    }
                    if let Some(width) = node.get_attribute("width") {
                        if width.chars().all(|c| c.is_ascii_digit()) {
                            crate::style::apply_inline(style, &format!("width: {}px", width));
                        } else {
                            crate::style::apply_inline(style, &format!("width: {}", width));
                        }
                    }
                    if let Some(align) = node.get_attribute("align") {
                        crate::style::apply_inline(style, &format!("text-align: {}", align));
                    }
                    if let Some(height) = node.get_attribute("height") {
                        if height.chars().all(|c| c.is_ascii_digit()) {
                            crate::style::apply_inline(style, &format!("height: {}px", height));
                        } else {
                            crate::style::apply_inline(style, &format!("height: {}", height));
                        }
                    }
                }
            }
        }
    }

    grow_trailing_auto_cells(tree, &mut styles);

    let mut taffy_tree: TaffyTree = TaffyTree::new();
    let mut id_map: HashMap<taffy::NodeId, NodeId> = HashMap::new();

    // The document node itself is not an element; lay out from the first
    // element descendant (the <html> root).
    let root = tree
        .descendants(tree.document())
        .into_iter()
        .find(|id| tree.get_node(*id).map(|n| n.is_element()).unwrap_or(false));

    let mut rects = HashMap::new();
    if let Some(root_id) = root {
        // Top-down inheritance of the properties CSS inherits by default.
        #[derive(Clone, Default)]
        struct Inherited {
            color: Option<[u8; 4]>,
            font_size: Option<f32>,
            font_weight: Option<String>,
        }
        let mut queue = vec![(root_id, Inherited::default())];
        while let Some((id, mut inh)) = queue.pop() {
            if let Some(style) = styles.get_mut(&id) {
                match style.color { Some(c) => inh.color = Some(c), None => style.color = inh.color }
                match style.font_size { Some(s) => inh.font_size = Some(s), None => style.font_size = inh.font_size }
                match &style.font_weight { Some(w) => inh.font_weight = Some(w.clone()), None => style.font_weight = inh.font_weight.clone() }
            }
            for cid in tree.children(id).into_iter().rev() {
                queue.push((cid, inh.clone()));
            }
        }

        resolve_grid_areas(tree, root_id, &mut styles);

        if let Some(taffy_root) = build(tree, root_id, &mut taffy_tree, &mut id_map, &styles) {
            let _ = taffy_tree.compute_layout(
                taffy_root,
                taffy::Size {
                    width: taffy::AvailableSpace::Definite(viewport.0),
                    height: taffy::AvailableSpace::Definite(viewport.1),
                },
            );
            compute_absolute_rects(&taffy_tree, taffy_root, 0.0, 0.0, &id_map, &mut rects);
        }
    }

    let mut clip_rects = HashMap::new();
    if let Some(root_id) = root {
        resolve_clip_rects(tree, root_id, None, &rects, &styles, &mut clip_rects);
    }

    DomLayout { rects, styles, clip_rects }
}

/// HTML table auto-layout approximation: for each `<tr>`, the last `<td>`/
/// `<th>` child with no explicit width absorbs the row's leftover space. Real
/// browsers run a genuine column-width-negotiation algorithm across every row
/// in a table; giving `flex_grow` to *every* auto-width cell approximates that
/// badly; multiple growing siblings compete and the split becomes sensitive to
/// each cell's own content size, which looks like near-random drift row to
/// row. Growing only the trailing auto cell matches the extremely common
/// layout intent (fixed-size leading cells, one expanding trailing cell) and
/// leaves the others shrink-to-fit, matching a shrink-to-fit table exactly
/// when there is no surplus width to distribute in the first place.
fn grow_trailing_auto_cells(tree: &DomTree, styles: &mut HashMap<NodeId, crate::LayoutStyle>) {
    let is_tag = |id: NodeId, tags: &[&str]| -> bool {
        match tree.get_node(id).and_then(|n| n.as_element().map(|e| e.local.to_string())) {
            Some(local) => tags.contains(&local.as_str()),
            None => false,
        }
    };
    for tr in tree.descendants(tree.document()) {
        if !is_tag(tr, &["tr"]) {
            continue;
        }
        let last_auto_cell = tree.children(tr).into_iter().rev().find(|&cid| {
            is_tag(cid, &["td", "th"])
                && styles.get(&cid).map(|s| s.width == crate::Dimension::Auto && s.flex_grow.is_none()).unwrap_or(false)
        });
        if let Some(cid) = last_auto_cell {
            if let Some(style) = styles.get_mut(&cid) {
                style.flex_grow = Some(1.0);
            }
        }
    }
}

/// Walk the tree; for each `display: grid` element that declares
/// `grid-template-areas`, resolve each direct child's `grid-area` name to a
/// taffy line placement. This is how Vector 2022 (and most grid layouts) place
/// their header/sidebar/content/footer regions.
fn resolve_grid_areas(
    tree: &DomTree,
    root: NodeId,
    styles: &mut HashMap<NodeId, crate::LayoutStyle>,
) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        for cid in tree.children(id) {
            stack.push(cid);
        }
        let areas = match styles.get(&id) {
            Some(s) if s.display == crate::Display::Grid => match &s.grid_areas {
                Some(a) if !a.is_empty() => a.clone(),
                _ => continue,
            },
            _ => continue,
        };

        // name -> (row_start, row_end, col_start, col_end) in 0-based track indices.
        let mut spans: HashMap<String, (usize, usize, usize, usize)> = HashMap::new();
        for (r, row) in areas.iter().enumerate() {
            for (c, name) in row.iter().enumerate() {
                if name == "." {
                    continue;
                }
                spans
                    .entry(name.clone())
                    .and_modify(|s| {
                        s.0 = s.0.min(r);
                        s.1 = s.1.max(r);
                        s.2 = s.2.min(c);
                        s.3 = s.3.max(c);
                    })
                    .or_insert((r, r, c, c));
            }
        }

        for cid in tree.children(id) {
            let Some(cstyle) = styles.get_mut(&cid) else { continue };
            let Some(name) = cstyle.grid_area_name.clone() else { continue };
            if let Some(&(r0, r1, c0, c1)) = spans.get(&name) {
                use taffy::style_helpers::line;
                cstyle.grid_row = Some(taffy::Line {
                    start: line((r0 + 1) as i16),
                    end: line((r1 + 2) as i16),
                });
                cstyle.grid_column = Some(taffy::Line {
                    start: line((c0 + 1) as i16),
                    end: line((c1 + 2) as i16),
                });
            }
        }
    }
}

fn compute_absolute_rects(
    taffy_tree: &TaffyTree,
    taffy_id: taffy::NodeId,
    abs_x: f32,
    abs_y: f32,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    rects: &mut HashMap<NodeId, Rect>,
) {
    if let Ok(layout) = taffy_tree.layout(taffy_id) {
        let x = abs_x + layout.location.x;
        let y = abs_y + layout.location.y;
        
        if let Some(dom_id) = id_map.get(&taffy_id) {
            rects.insert(
                *dom_id,
                Rect {
                    x,
                    y,
                    width: layout.size.width,
                    height: layout.size.height,
                },
            );
        }
        
        if let Ok(children) = taffy_tree.children(taffy_id) {
            for child_id in children {
                compute_absolute_rects(taffy_tree, child_id, x, y, id_map, rects);
            }
        }
    }
}

/// Does `id` have any direct child that is inline-level (a non-whitespace
/// text node, or an element whose resolved display is `Inline`)? Used to
/// decide whether a block container needs the flex-row-wrap approximation of
/// an inline formatting context.
fn has_inline_content(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, crate::LayoutStyle>) -> bool {
    tree.children(id).into_iter().any(|cid| {
        let Some(node) = tree.get_node(cid) else { return false };
        match &node.data {
            obscura_dom::tree::NodeData::Text { contents } => !contents.trim().is_empty(),
            _ => styles.get(&cid).map(|s| s.display == crate::Display::Inline).unwrap_or(false),
        }
    })
}

fn build(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Option<taffy::NodeId> {
    let node = tree.get_node(id)?;

    if let obscura_dom::tree::NodeData::Text { contents } = &node.data {
        let mut display_text = String::new();
        let mut in_space = false;
        for c in contents.chars() {
            if c.is_whitespace() {
                if !in_space {
                    display_text.push(' ');
                    in_space = true;
                }
            } else {
                display_text.push(c);
                in_space = false;
            }
        }


        let mut fsize = 16.0;
        let mut is_bold = false;
        if let Some(parent_id) = node.parent {
            if let Some(p_style) = styles.get(&parent_id) {
                fsize = p_style.font_size.unwrap_or(16.0);
                is_bold = p_style.font_weight.as_deref() == Some("bold");
            }
        }
        
        let width = text_width(&display_text, fsize, is_bold);
        // A text node that is pure whitespace is almost always HTML source
        // formatting (a newline and indentation between sibling tags), not
        // meaningful content: real browsers collapse it away entirely. We
        // still give it its natural (small) width, since a bare space
        // between two inline elements ("<a>Foo</a> <a>Bar</a>") does need to
        // keep them visually separated, but zero height, so it never adds a
        // spurious blank row when it lands between block-level siblings
        // (e.g. formatting whitespace around a run of now-collapsed,
        // display:none list items).
        let height = if contents.trim().is_empty() { 0.0 } else { fsize * 1.2 };

        let taffy_style = taffy::Style {
            size: taffy::Size {
                width: taffy::Dimension::Length(width),
                height: taffy::Dimension::Length(height),
            },
            ..Default::default()
        };
        let taffy_id = taffy_tree.new_leaf(taffy_style).ok()?;
        id_map.insert(taffy_id, id);
        return Some(taffy_id);
    }

    let _name = node.as_element()?;
    let style = styles.get(&id)?;
    if style.display == crate::Display::None {
        return None;
    }
    let mut taffy_style = to_taffy_style(style);

    // Taffy has no real inline formatting context: its native Block layout
    // treats every direct child as its own full-width block box, stacked
    // vertically. That is correct when a block's children are other block
    // elements (a div full of divs), but wrong the moment a block holds
    // inline-level content (running text with <a>/<span>/<b> mixed in, i.e.
    // any ordinary paragraph or list item), where real CSS instead lays
    // consecutive inline boxes out left-to-right, wrapping at the container's
    // width. Approximate that inline formatting context by promoting such
    // blocks to a wrapping flex row, so text and inline elements actually
    // flow together on shared lines instead of each getting its own row.
    if style.display == crate::Display::Block && has_inline_content(tree, id, styles) {
        taffy_style.display = taffy::style::Display::Flex;
        taffy_style.flex_direction = taffy::FlexDirection::Row;
        taffy_style.flex_wrap = taffy::FlexWrap::Wrap;
    }

    let child_ids: Vec<taffy::NodeId> = tree
        .children(id)
        .into_iter()
        .filter_map(|cid| build(tree, cid, taffy_tree, id_map, styles))
        .collect();

    let taffy_id = if child_ids.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &child_ids).ok()?
    };
    id_map.insert(taffy_id, id);
    Some(taffy_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_dom::tree_sink::parse_html;

    #[test]
    fn lays_out_real_dom() {
        let tree = parse_html(
            "<html><body><div style=\"width: 300px\"><div style=\"width: 100px; height: 40px\"></div><div style=\"width: 100px; height: 60px\"></div></div></body></html>",
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        // Every element gets a rect.
        assert!(laid.rects.len() >= 4, "expected >=4 element rects, got {}", laid.rects.len());

        // The two inner divs stack vertically inside the 300px container.
        let stacks = laid
            .rects
            .values()
            .filter(|r| (r.width - 100.0).abs() < 0.1)
            .map(|r| (r.y, r.height))
            .collect::<Vec<_>>();
        assert_eq!(stacks.len(), 2, "expected two 100px-wide children");
        let mut sorted = stacks.clone();
        sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert!(
            (sorted[0].1 - 40.0).abs() < 0.1 && (sorted[1].1 - 60.0).abs() < 0.1,
            "children heights should be 40 and 60, got {:?}",
            sorted
        );
    }

    #[test]
    fn empty_document_is_safe() {
        // html5ever always synthesizes html/head/body, so an empty document
        // still has a few element rects. The point is that it does not panic.
        let tree = parse_html("");
        let laid = layout_dom(&tree, (1280.0, 720.0));
        assert!(laid.rects.len() <= 4, "got {}", laid.rects.len());
    }
}
