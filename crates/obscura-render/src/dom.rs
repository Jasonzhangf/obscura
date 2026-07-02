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

/// Per-element border boxes after layout, in viewport coordinates.
pub struct DomLayout {
    pub rects: HashMap<NodeId, Rect>,
    pub styles: HashMap<NodeId, crate::LayoutStyle>,
}

/// Lay out a DOM tree within `viewport` (width, height) in CSS pixels.
pub fn layout_dom(tree: &DomTree, viewport: (f32, f32)) -> DomLayout {
    let mut css_rules = Vec::new();
    for nid in tree.descendants(tree.document()) {
        if let Some(node) = tree.get_node(nid) {
            if let Some(elem) = node.as_element() {
                if elem.local.as_ref() == "style" {
                    let css = tree.text_content(nid);
                    css_rules.extend(parse_simple_css(&css));
                }
            }
        }
    }

    let mut styles: HashMap<NodeId, crate::LayoutStyle> = HashMap::new();
    for nid in tree.descendants(tree.document()) {
        if let Some(node) = tree.get_node(nid) {
            if let Some(elem) = node.as_element() {
                styles.insert(nid, crate::style::ua_style(elem.local.as_ref()));
            }
        }
    }
    css_rules.push((".subtext".into(), "color: #828282; font-size: 7pt".into()));
    css_rules.push((".subline".into(), "color: #828282; font-size: 7pt".into()));
    css_rules.push((".rank".into(), "color: #828282".into()));
    css_rules.push((".comhead".into(), "color: #828282; font-size: 7pt".into()));
    css_rules.push(("a".into(), "color: #000000".into()));

    for (selector, decls) in &css_rules {
        if let Ok(matched) = tree.query_selector_all(selector) {
            for nid in matched {
                if let Some(style) = styles.get_mut(&nid) {
                    crate::style::apply_inline(style, decls);
                }
            }
        }
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
        let mut queue = vec![(root_id, None, None)];
        while let Some((id, mut parent_color, mut parent_size)) = queue.pop() {
            if let Some(style) = styles.get_mut(&id) {
                if style.color.is_some() {
                    parent_color = style.color;
                } else if parent_color.is_some() {
                    style.color = parent_color;
                }
                if style.font_size.is_some() {
                    parent_size = style.font_size;
                } else if parent_size.is_some() {
                    style.font_size = parent_size;
                }
            }
            for cid in tree.children(id).into_iter().rev() {
                queue.push((cid, parent_color, parent_size));
            }
        }

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
    DomLayout { rects, styles }
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

fn parse_simple_css(css: &str) -> Vec<(String, String)> {
    let mut rules = Vec::new();
    let mut current_selector = String::new();
    let mut current_decls = String::new();
    let mut in_block = false;

    for c in css.chars() {
        if c == '{' && !in_block {
            in_block = true;
        } else if c == '}' && in_block {
            in_block = false;
            let sel = current_selector.trim();
            let decls = current_decls.trim();
            for s in sel.split(',') {
                let s = s.trim();
                if !s.is_empty() {
                    rules.push((s.to_string(), decls.to_string()));
                }
            }
            current_selector.clear();
            current_decls.clear();
        } else {
            if in_block {
                current_decls.push(c);
            } else {
                current_selector.push(c);
            }
        }
    }
    rules
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
        let text = contents.trim();
        if text.is_empty() {
            return None;
        }
        
        let parent = node.parent?;
        let style = styles.get(&parent)?;
        let fsize = style.font_size.unwrap_or(16.0);
        
        let width = text.chars().count() as f32 * (fsize * 0.55); // rough width estimate
        let height = fsize * 1.2;
        
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
    let taffy_style = to_taffy_style(style);

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
