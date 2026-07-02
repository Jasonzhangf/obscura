//! obscura-render: the optional scoped render layer for Obscura.
//!
//! Obscura's default build has no layout or paint engine, which is the source
//! of its speed and low memory. This crate adds a render layer behind a feature
//! flag: real CSS box geometry (so getBoundingClientRect, elementFromPoint, and
//! IntersectionObserver return true values) and, with the paint feature,
//! rasterized PNG screenshots.
//!
//! Phase 1 (this file): the layout core. A `LayoutNode` tree plus a viewport is
//! laid out with taffy and the resulting border-box geometry is returned per
//! node. Later phases build the `LayoutNode` tree from the live DOM + computed
//! styles, feed geometry back to JS, and add text + paint.

use taffy::prelude::*;

mod style;
pub use style::compute_style;

mod dom;
pub use dom::{layout_dom, DomLayout};

#[cfg(feature = "paint")]
mod paint;
#[cfg(feature = "paint")]
pub use paint::{paint_dom, screenshot_png};

/// An axis-aligned rectangle in CSS pixels, relative to the containing block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Per-edge box values (margin / padding / border) in CSS pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

/// The display modes obscura-render cares about for phase 1. Inline text layout
/// arrives with the text/paint phase and is folded in then.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Display {
    #[default]
    Block,
    Flex,
    Inline,
    #[allow(dead_code)]
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Dimension {
    #[default]
    Auto,
    Px(f32),
    Percent(f32),
}

/// The subset of CSS that influences box layout. Expanded in later phases.
#[derive(Debug, Clone, Default)]
pub struct LayoutStyle {
    pub display: Display,
    pub width: Dimension,
    pub height: Dimension,
    pub margin: Edges,
    pub padding: Edges,
    pub border: Edges,
    /// RGBA for the paint step. Parsed always (cheap), used only with `paint`.
    pub background_color: Option<[u8; 4]>,
    /// Foreground (text) color for the paint step.
    pub color: Option<[u8; 4]>,
    pub border_color: Option<[u8; 4]>,
    pub font_size: Option<f32>,
    pub font_weight: Option<String>,
    pub align_items: Option<taffy::AlignItems>,
    pub flex_direction: Option<taffy::FlexDirection>,
    pub justify_content: Option<taffy::JustifyContent>,
    pub flex_grow: Option<f32>,
}

/// A node in the input layout tree. `text` is carried for the paint phase; it
/// does not affect layout in phase 1 (inline/text layout comes later).
#[derive(Debug, Clone)]
pub struct LayoutNode {
    pub style: LayoutStyle,
    #[allow(dead_code)]
    pub text: Option<String>,
    pub children: Vec<LayoutNode>,
}

impl LayoutNode {
    pub fn leaf(style: LayoutStyle) -> Self {
        LayoutNode { style, text: None, children: Vec::new() }
    }
}

/// Computed geometry for one node and its subtree.
#[derive(Debug, Clone, Default)]
pub struct NodeRect {
    /// Border box, in viewport coordinates.
    pub border_box: Rect,
    pub children: Vec<NodeRect>,
}

/// Lay out `root` within a `viewport` (width, height) in CSS pixels and return
/// the border-box geometry per node, mirroring the input tree.
pub fn layout(root: &LayoutNode, viewport: (f32, f32)) -> NodeRect {
    let mut tree: TaffyTree = TaffyTree::new();
    let root_id = build_node(&mut tree, root);
    let _ = tree.compute_layout(
        root_id,
        taffy::Size {
            width: taffy::AvailableSpace::Definite(viewport.0),
            height: taffy::AvailableSpace::Definite(viewport.1),
        },
    );
    read_node(&tree, root_id)
}

fn build_node(tree: &mut TaffyTree, node: &LayoutNode) -> NodeId {
    let style = to_taffy_style(&node.style);
    if node.children.is_empty() {
        tree.new_leaf(style).expect("taffy new_leaf")
    } else {
        let child_ids: Vec<NodeId> =
            node.children.iter().map(|c| build_node(tree, c)).collect();
        tree.new_with_children(style, &child_ids).expect("taffy new_with_children")
    }
}

fn read_node(tree: &TaffyTree, id: NodeId) -> NodeRect {
    let layout = tree.layout(id).expect("taffy layout");
    NodeRect {
        border_box: Rect {
            x: layout.location.x,
            y: layout.location.y,
            width: layout.size.width,
            height: layout.size.height,
        },
        children: tree
            .children(id)
            .unwrap_or_default()
            .iter()
            .map(|&cid| read_node(tree, cid))
            .collect(),
    }
}

pub(crate) fn to_taffy_style(style: &LayoutStyle) -> Style {
    let mut s = Style::DEFAULT;
    s.display = match style.display {
        Display::Block => taffy::style::Display::Block,
        Display::Flex => taffy::style::Display::Flex,
        Display::Inline => taffy::style::Display::Flex,
        Display::None => taffy::style::Display::None,
    };
    if let Some(fd) = style.flex_direction {
        s.flex_direction = fd;
        s.flex_wrap = taffy::FlexWrap::NoWrap;
    } else if style.display == Display::Inline {
        s.flex_direction = taffy::FlexDirection::Row;
        s.flex_wrap = taffy::FlexWrap::Wrap;
    } else {
        s.flex_wrap = taffy::FlexWrap::NoWrap;
    }
    s.size = taffy::Size {
        width: dimension(style.width),
        height: dimension(style.height),
    };
    if let Some(ai) = style.align_items {
        s.align_items = Some(ai);
    }
    if let Some(jc) = style.justify_content {
        s.justify_content = Some(jc);
    }
    if let Some(fg) = style.flex_grow {
        s.flex_grow = fg;
    }
    s.margin = rect_auto(style.margin);
    s.padding = rect_lp(style.padding);
    s.border = rect_lp(style.border);
    s
}

fn dimension(v: Dimension) -> taffy::style::Dimension {
    match v {
        Dimension::Px(px) => taffy::style::Dimension::Length(px),
        Dimension::Percent(p) => taffy::style::Dimension::Percent(p),
        Dimension::Auto => taffy::style::Dimension::Auto,
    }
}

fn rect_lp(e: Edges) -> taffy::Rect<taffy::style::LengthPercentage> {
    taffy::Rect {
        top: taffy::style::LengthPercentage::Length(e.top),
        right: taffy::style::LengthPercentage::Length(e.right),
        bottom: taffy::style::LengthPercentage::Length(e.bottom),
        left: taffy::style::LengthPercentage::Length(e.left),
    }
}

fn rect_auto(e: Edges) -> taffy::Rect<taffy::style::LengthPercentageAuto> {
    taffy::Rect {
        top: taffy::style::LengthPercentageAuto::Length(e.top),
        right: taffy::style::LengthPercentageAuto::Length(e.right),
        bottom: taffy::style::LengthPercentageAuto::Length(e.bottom),
        left: taffy::style::LengthPercentageAuto::Length(e.left),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_box(display: Display, w: f32, h: f32) -> LayoutStyle {
        LayoutStyle { display, width: Dimension::Px(w), height: Dimension::Px(h), ..Default::default() }
    }

    #[test]
    fn block_children_stack_vertically() {
        // A 1000px-wide viewport, two fixed-size block children: they should
        // stack top-to-bottom at the expected y offsets.
        let root = LayoutNode {
            style: make_box(Display::Block, 1000.0, 800.0),
            text: None,
            children: vec![
                LayoutNode::leaf(make_box(Display::Block, 1000.0, 50.0)),
                LayoutNode::leaf(make_box(Display::Block, 1000.0, 30.0)),
            ],
        };
        let out = layout(&root, (1000.0, 800.0));
        assert_eq!(out.border_box.width, 1000.0);
        assert_eq!(out.children.len(), 2);
        assert_eq!(out.children[0].border_box.height, 50.0);
        assert_eq!(out.children[1].border_box.height, 30.0);
        // Second block begins where the first ended.
        assert!(
            (out.children[1].border_box.y - out.children[0].border_box.y).abs()
                >= out.children[0].border_box.height - 0.01,
            "blocks should stack: c0.y={} c1.y={}",
            out.children[0].border_box.y,
            out.children[1].border_box.y
        );
    }

    #[test]
    fn flex_row_lays_out_horizontally() {
        let root = LayoutNode {
            style: LayoutStyle { display: Display::Flex, width: Dimension::Px(600.0), height: Dimension::Px(100.0), ..Default::default() },
            text: None,
            children: vec![
                LayoutNode::leaf(make_box(Display::Block, 200.0, 100.0)),
                LayoutNode::leaf(make_box(Display::Block, 200.0, 100.0)),
            ],
        };
        let out = layout(&root, (600.0, 400.0));
        assert_eq!(out.border_box.width, 600.0);
        assert_eq!(out.children.len(), 2);
        // In a row the second child is to the right of the first.
        assert!(
            out.children[1].border_box.x > out.children[0].border_box.x,
            "flex row should place children horizontally: c0.x={} c1.x={}",
            out.children[0].border_box.x,
            out.children[1].border_box.x
        );
    }

    #[test]
    fn padding_insets_the_box() {
        let root = LayoutNode {
            style: LayoutStyle {
                display: Display::Block,
                width: Dimension::Px(100.0),
                height: Dimension::Px(100.0),
                padding: Edges { top: 10.0, right: 10.0, bottom: 10.0, left: 10.0 },
                ..Default::default()
            },
            text: None,
            children: vec![],
        };
        let out = layout(&root, (1000.0, 800.0));
        // Border box still matches the requested size; padding affects content,
        // not the outer border box here.
        assert_eq!(out.border_box.width, 100.0);
    }
}
