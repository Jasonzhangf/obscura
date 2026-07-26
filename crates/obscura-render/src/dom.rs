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
    /// per node, in SCREEN space (the clip owner's box shifted by the owner's
    /// accumulated translate; see `resolve_clip_rects`). `None` means
    /// unclipped. Does not include the node's own overflow (that only clips
    /// its children, not itself).
    pub clip_rects: HashMap<NodeId, Option<Rect>>,
    /// Accumulated `transform: translate()` per node (own + ancestors), in CSS
    /// px. Only nodes with a non-zero accumulation are present; paint shifts
    /// each box by this to reach screen space.
    pub translates: HashMap<NodeId, (f32, f32)>,
    /// Per-word geometry for text nodes: a text DOM node lays out as one
    /// taffy leaf per word (see `build_text_words`), each wrapping
    /// independently within its container, so its rendered content is a list
    /// of (box, word text) pairs rather than a single box, unlike every other
    /// node kind. Keyed by the text node's `NodeId`, in layout order.
    pub text_runs: HashMap<NodeId, Vec<(Rect, String)>>,
    /// Inline formatting contexts that were shaped by cosmic-text (see
    /// `inline`): a single leaf per container, its glyphs held in
    /// `text_engine`. `ifc_items` maps the container's `NodeId` to its item
    /// index. Paint rasterizes these instead of the per-word `text_runs`.
    #[cfg(feature = "paint")]
    pub text_engine: crate::inline::TextEngine,
    #[cfg(feature = "paint")]
    pub ifc_items: HashMap<NodeId, usize>,
    /// Anonymous inline-run contexts: a mixed block's consecutive inline
    /// children folded to one shaped leaf each (see `build_mixed_block`).
    /// Keyed by the parent block's `NodeId`, in document order; the leaves
    /// have no DOM node of their own.
    #[cfg(feature = "paint")]
    pub run_ifc_items: HashMap<NodeId, Vec<usize>>,
}

/// Registry of cosmic-text inline formatting contexts created during the
/// taffy-tree build: `whole` holds single-leaf containers keyed by the
/// container's own DOM id, `runs` holds anonymous inline-run leaves keyed by
/// the parent block's DOM id (those leaves have no DOM node, so their final
/// rects are captured separately; see `compute_absolute_rects`).
#[derive(Default)]
struct IfcRegistry {
    whole: HashMap<NodeId, usize>,
    runs: HashMap<NodeId, Vec<usize>>,
    /// Specified column widths per table grid node, from `<col>` elements and
    /// colspan-1 cells: `(px, percent)` per column index. Consumed by the
    /// table column-balancing pass in `layout_dom_with_images`, which pins
    /// specified columns instead of sizing them purely from content.
    table_cols: HashMap<taffy::NodeId, (Vec<Option<f32>>, Vec<Option<f32>>)>,
    /// Minimum row heights per table grid node, one entry per source row.
    /// The post-width row-sizing pass combines these with each cell's final
    /// content height and pins the tracks before vertical alignment.
    table_rows: HashMap<taffy::NodeId, Vec<Option<f32>>>,
    /// Floats whose direct block parent does not establish a block formatting
    /// context and whose exclusion can therefore continue through later
    /// descendant blocks of the same BFC. The first layout pass gives these
    /// nodes real geometry; `apply_float_continuations` uses it to narrow the
    /// later intersecting bands before the final layout.
    float_continuations: Vec<FloatContinuation>,
}

#[derive(Clone, Copy)]
struct FloatContinuation {
    owner: NodeId,
    float: taffy::NodeId,
    flow: taffy::NodeId,
    side: crate::Float,
}

/// Walk the tree top-down accumulating the clip rect imposed by ancestor
/// `overflow: hidden` boxes. Must run after layout, since it needs border
/// boxes. This is what makes `overflow: hidden` (used pervasively for the
/// "visually hidden but accessible" pattern: a 1x1 absolutely-positioned box
/// with clipped overflow) actually hide its contents instead of letting text
/// paint wherever the box's static position happens to land.
/// Clips are stored in SCREEN space: the clip owner's border box offset by the
/// owner's own accumulated `transform: translate()`. A clip belongs to its
/// owner's coordinate space, not the painted descendant's; shifting an
/// inherited clip by the descendant's translate (the old behavior) let a
/// carousel track's `translateX` drag the viewport's clip along with every
/// slide, so all slides stayed visible inside their own shifted clip.
///
/// The same walk records each node's accumulated translate in `translates`
/// (one pass here instead of an ancestor walk per painted node).
fn resolve_clip_rects(
    tree: &DomTree,
    id: NodeId,
    inherited: Option<Rect>,
    tx: f32,
    ty: f32,
    rects: &HashMap<NodeId, Rect>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    clip_rects: &mut HashMap<NodeId, Option<Rect>>,
    translates: &mut HashMap<NodeId, (f32, f32)>,
) {
    clip_rects.insert(id, inherited);
    // This node's own translate joins the accumulation for its box and its
    // whole subtree (percentages resolve against its own border box).
    let (tx, ty) = match (styles.get(&id).and_then(|s| s.transform_translate), rects.get(&id)) {
        (Some((dx, dy)), Some(rect)) => (tx + resolve_translate(dx, rect.width), ty + resolve_translate(dy, rect.height)),
        (Some((dx, dy)), None) => (tx + resolve_translate(dx, 0.0), ty + resolve_translate(dy, 0.0)),
        _ => (tx, ty),
    };
    if tx != 0.0 || ty != 0.0 {
        translates.insert(id, (tx, ty));
    }
    let next = match (styles.get(&id), rects.get(&id)) {
        (Some(style), Some(rect)) if style.overflow_hidden => {
            let own = Rect { x: rect.x + tx, y: rect.y + ty, width: rect.width, height: rect.height };
            Some(match inherited {
                Some(clip) => clip.intersect(&own).unwrap_or(Rect::default()),
                None => own,
            })
        }
        _ => inherited,
    };
    for cid in tree.children(id) {
        resolve_clip_rects(tree, cid, next, tx, ty, rects, styles, clip_rects, translates);
    }
}

/// Resolve one `transform: translate()` component to px: a length passes
/// through, a percentage is taken against `basis` (the element's own
/// border-box extent on that axis). Font/viewport-relative leftovers fall
/// back to a coarse px value (translate rarely uses them).
pub(crate) fn resolve_translate(d: crate::Dimension, basis: f32) -> f32 {
    match d {
        crate::Dimension::Px(px) => px,
        crate::Dimension::Percent(p) => p * basis,
        crate::Dimension::Em(v) | crate::Dimension::Rem(v) => v * 16.0,
        crate::Dimension::Ex(v) => v * 16.0 * 0.528_320_3,
        crate::Dimension::Vw(v)
        | crate::Dimension::Vh(v)
        | crate::Dimension::Vmin(v)
        | crate::Dimension::Vmax(v) => v,
        crate::Dimension::Auto => 0.0,
    }
}

/// Apply HTML presentational attributes at their cascade origin: above the UA
/// defaults, but below every author stylesheet and style attribute.
fn apply_presentational_hints(node: &obscura_dom::tree::Node, style: &mut crate::LayoutStyle) {
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
    if let Some(valign) = node.get_attribute("valign") {
        crate::style::apply_inline(style, &format!("vertical-align: {}", valign));
    }
    if let Some(cellspacing) = node.get_attribute("cellspacing") {
        if cellspacing.chars().all(|c| c.is_ascii_digit()) {
            crate::style::apply_inline(style, &format!("border-spacing: {}px", cellspacing));
        }
    }
    if let Some(height) = node.get_attribute("height") {
        if height.chars().all(|c| c.is_ascii_digit()) {
            crate::style::apply_inline(style, &format!("height: {}px", height));
        } else {
            crate::style::apply_inline(style, &format!("height: {}", height));
        }
    }
    if style.aspect_ratio.is_none() {
        let aw = node.get_attribute("width").and_then(|w| w.parse::<f32>().ok());
        let ah = node.get_attribute("height").and_then(|h| h.parse::<f32>().ok());
        if let (Some(w), Some(h)) = (aw, ah) {
            if w > 0.0 && h > 0.0 {
                style.aspect_ratio = Some(w / h);
            }
        }
    }
    // An inline SVG with one CSS axis and a viewBox derives the other axis
    // from the viewBox's intrinsic aspect ratio. Framework logos commonly set
    // only a responsive utility width (`w-24 lg:w-28`) and rely on this rule;
    // without it the SVG border box had zero height and paint skipped it.
    if style.aspect_ratio.is_none()
        && node
            .as_element()
            .map_or(false, |element| element.local.as_ref() == "svg")
    {
        if let Some(view_box) = node
            .get_attribute("viewBox")
            .or_else(|| node.get_attribute("viewbox"))
        {
            let values: Vec<f32> = view_box
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse::<f32>().ok())
                .collect();
            if values.len() == 4 && values[2] > 0.0 && values[3] > 0.0 {
                style.aspect_ratio = Some(values[2] / values[3]);
            }
        }
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
    parent_props: &std::rc::Rc<HashMap<String, String>>,
    quirks_mode: bool,
    inherited_cell_padding: Option<f32>,
) {
    let Some(node) = tree.get_node(id) else { return };
    let is_element = node.is_element();
    // The custom-property map in force for this node's subtree: the parent's,
    // unless this element declares its own `--x` (then a richer map).
    let mut this_props = parent_props.clone();
    let mut descendant_cell_padding = inherited_cell_padding;
    if let Some(elem) = node.as_element() {
        if elem.local.as_ref() == "table" {
            // `cellpadding` is a table-scoped presentational hint applied to
            // its cells, below author CSS. Entering any nested table resets
            // the outer table's value before reading the nested attribute.
            descendant_cell_padding = node
                .get_attribute("cellpadding")
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0);
        }
        let mut style = crate::style::ua_style(elem.local.as_ref());
        if matches!(elem.local.as_ref(), "td" | "th") {
            if let Some(padding) = inherited_cell_padding {
                style.padding = crate::Edges {
                    top: padding,
                    right: padding,
                    bottom: padding,
                    left: padding,
                };
            }
        }
        if quirks_mode && elem.local.as_ref() == "form" {
            // Legacy HTML/quirks rendering keeps one em after forms. Standards
            // mode does not; Hacker News and many old document templates omit
            // a doctype and rely on this spacing.
            style.margin_relative[2] = Some(crate::Dimension::Em(1.0));
        }
        if elem.local.as_ref() == "input" {
            let input_type = node
                .get_attribute("type")
                .unwrap_or("text")
                .trim()
                .to_ascii_lowercase();
            if quirks_mode {
                style.box_sizing = crate::BoxSizing::BorderBox;
            }
            match input_type.as_str() {
                "checkbox" | "radio" => {
                    style.margin = crate::Edges {
                        top: 3.0,
                        right: 3.0,
                        bottom: 3.0,
                        left: 4.0,
                    };
                    style.padding = crate::Edges::default();
                    style.border = crate::Edges::default();
                }
                "range" | "color" => {
                    style.margin = crate::Edges {
                        top: 2.0,
                        right: 2.0,
                        bottom: 2.0,
                        left: 2.0,
                    };
                    style.padding = crate::Edges::default();
                    style.border = crate::Edges::default();
                }
                _ => {}
            }
        }
        // UA rule `[hidden]:not([hidden=until-found]) { display: none }`.
        // Applied before the author cascade so a matching author `display`
        // still wins (UA origin, per the HTML rendering spec). Sites ship
        // dialogs, tab panels, and menus with a plain `hidden` attribute and
        // reveal them by removing it (eBay's homepage interstitial uses
        // `.lightbox-dialog[role=dialog]:not([hidden]){display:flex}`), so
        // without this the hidden overlay paints its full-page dark backdrop.
        if let Some(h) = node.get_attribute("hidden") {
            if !h.eq_ignore_ascii_case("until-found") {
                style.display = crate::Display::None;
            }
        }
        apply_presentational_hints(&node, &mut style);
        let node_id = node.get_attribute("id");
        let classes: Vec<String> = node
            .get_attribute("class")
            .map(|c| c.split_whitespace().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        if let Some(m) = sheet.apply(
            tree,
            matcher,
            id,
            node_id,
            &classes,
            elem.local.as_ref(),
            &mut style,
            parent_props,
            node.get_attribute("style"),
        ) {
            this_props = std::rc::Rc::new(m);
        }
        let (before, after) = sheet.pseudo_content(tree, matcher, id);
        style.before_content = before;
        style.after_content = after;
        styles.insert(id, style);
    }

    if is_element {
        matcher.push_ancestor(tree, id);
    }
    for cid in tree.children(id) {
        cascade_walk(
            tree,
            cid,
            sheet,
            matcher,
            styles,
            &this_props,
            quirks_mode,
            descendant_cell_padding,
        );
    }
    if is_element {
        matcher.pop_ancestor();
    }
}

/// Lay out a DOM tree within `viewport` (width, height) in CSS pixels.
pub fn layout_dom(tree: &DomTree, viewport: (f32, f32)) -> DomLayout {
    layout_dom_with_images(tree, viewport, &HashMap::new())
}

/// Like [`layout_dom`], but `intrinsic` supplies fetched intrinsic pixel sizes
/// (width, height) for replaced elements keyed by `NodeId`. Paint collects
/// these before layout (it has the base URL and the image cache) so a
/// CSS-sized `<img>` with no width/height attribute gets a real box from its
/// intrinsic ratio instead of collapsing to zero area.
pub fn layout_dom_with_images(
    tree: &DomTree,
    viewport: (f32, f32),
    intrinsic: &HashMap<NodeId, (f32, f32)>,
) -> DomLayout {
    layout_dom_with_resources(tree, viewport, intrinsic, &[])
}

/// Like [`layout_dom_with_images`], with decoded OpenType web-font data loaded
/// into the shaping database for this render pass.
pub fn layout_dom_with_resources(
    tree: &DomTree,
    viewport: (f32, f32),
    intrinsic: &HashMap<NodeId, (f32, f32)>,
    fonts: &[Vec<u8>],
) -> DomLayout {
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
    let sheet = crate::css::Stylesheet::parse_for_viewport(tree, &css_sources, viewport);
    let t_parse = t0.elapsed();

    let t1 = std::time::Instant::now();
    let mut matcher = tree.matcher();
    let mut styles: HashMap<NodeId, crate::LayoutStyle> = HashMap::new();
    let root_props = std::rc::Rc::new(HashMap::new());
    // A real preorder walk (not a flat descendants() scan) so the matcher's
    // ancestor bloom filter tracks the current path: push before recursing
    // into children, pop on the way back out. This is what lets descendant
    // combinators (".mw-body .firstHeading") fast-reject via the filter
    // instead of falling back to the always-true "can't reject" case.
    let quirks_mode = !tree
        .descendants(tree.document())
        .into_iter()
        .any(|id| {
            tree.get_node(id).map_or(false, |node| {
                matches!(node.data, obscura_dom::tree::NodeData::Doctype { .. })
            })
        });
    cascade_walk(
        tree,
        tree.document(),
        &sheet,
        &mut matcher,
        &mut styles,
        &root_props,
        quirks_mode,
        None,
    );
    if timing {
        let (r, i, c, l, u) = sheet.debug_stats();
        eprintln!("[timing] parse+index={:?} cascade={:?} rules={} id_keys={} class_keys={} local_keys={} universal={}", t_parse, t1.elapsed(), r, i, c, l, u);
    }
    grow_trailing_auto_cells(tree, &mut styles);

    // The leaf context is the index of a cosmic-text inline formatting
    // context in `engine`; leaves without text carry no context.
    let mut taffy_tree: TaffyTree<usize> = TaffyTree::new();
    let mut id_map: HashMap<taffy::NodeId, NodeId> = HashMap::new();
    let mut words: HashMap<taffy::NodeId, (NodeId, String)> = HashMap::new();
    let mut engine = crate::inline::TextEngine::new_with_fonts(fonts);
    let mut ifc_items = IfcRegistry::default();

    // The document node itself is not an element; lay out from the first
    // element descendant (the <html> root).
    let root = tree
        .descendants(tree.document())
        .into_iter()
        .find(|id| tree.get_node(*id).map(|n| n.is_element()).unwrap_or(false));

    let mut rects = HashMap::new();
    let mut text_runs = HashMap::new();
    // Final absolute rects of anonymous inline-run leaves, keyed by the
    // engine item index (they have no DOM id to key `rects` by).
    let mut anon_rects: HashMap<usize, Rect> = HashMap::new();
    if let Some(root_id) = root {
        // Top-down inheritance of the properties CSS inherits by default.
        #[derive(Clone)]
        struct Inherited {
            color: Option<[u8; 4]>,
            font_size: Option<f32>,
            font_weight: Option<String>,
            font_family: Option<String>,
            text_align: Option<taffy::AlignItems>,
            legacy_center: bool,
            visibility_hidden: bool,
            opacity_product: f32,
            list_style: crate::ListStyle,
            line_height: crate::LineHeight,
            text_transform: crate::TextTransform,
            italic: bool,
            box_sizing: crate::BoxSizing,
            border_collapse: bool,
            table_vertical_align: Option<crate::VerticalAlign>,
            /// Containing-block width in px for the current element, carried
            /// down so percentage padding/margin (which resolve against the
            /// containing block WIDTH, all sides) can be turned into px before
            /// taffy layout. Not a CSS-inherited property; it is recomputed to
            /// the element's own content width for its children.
            cb_width: f32,
            /// Whether the containing block has a definite height. Percentage
            /// heights in ordinary flow compute to auto when this is false;
            /// resolving them against a synthetic zero height collapses
            /// content-heavy modern UI wrappers such as code editors.
            cb_height_definite: bool,
        }
        impl Default for Inherited {
            fn default() -> Self {
                Inherited {
                    color: None,
                    font_size: None,
                    font_weight: None,
                    font_family: None,
                    text_align: None,
                    legacy_center: false,
                    visibility_hidden: false,
                    opacity_product: 1.0,
                    // CSS initial value of list-style-type.
                    list_style: crate::ListStyle::Disc,
                    line_height: crate::LineHeight::Normal,
                    text_transform: crate::TextTransform::None,
                    italic: false,
                    box_sizing: crate::BoxSizing::ContentBox,
                    border_collapse: false,
                    table_vertical_align: None,
                    cb_width: 0.0,
                    cb_height_definite: false,
                }
            }
        }
        // Viewport-per-unit for vw/vh, and the root font-size for rem. em/rem/
        // vw/vh lengths were parsed unresolved (the element font-size and
        // viewport are not known at parse time); resolve them here, top-down,
        // where the computed font-size is available.
        let vw = viewport.0 / 100.0;
        let vh = viewport.1 / 100.0;
        let root_fs = {
            let s = styles.get(&root_id);
            match (
                s.and_then(|s| s.font_size),
                s.and_then(|s| s.font_size_raw),
                s.and_then(|s| s.font_size_expression.as_deref()),
            ) {
                (Some(px), _, _) => px,
                (None, _, Some(expression)) => {
                    crate::style::resolve_contextual_length(
                        expression,
                        16.0,
                        16.0,
                        vw,
                        vh,
                        16.0,
                    )
                    .unwrap_or(16.0)
                }
                (None, Some(d), _) => match d.resolve(16.0, 16.0, vw, vh) {
                    crate::Dimension::Px(p) => p,
                    crate::Dimension::Percent(p) => 16.0 * p,
                    _ => 16.0,
                },
                _ => 16.0,
            }
        };

        // The root element's containing block is the initial containing block,
        // i.e. the viewport width.
        let mut root_inh = Inherited::default();
        root_inh.cb_width = viewport.0;
        root_inh.cb_height_definite = true;
        let mut queue = vec![(root_id, root_inh)];
        while let Some((id, mut inh)) = queue.pop() {
            // Default the child containing-block width to this element's own
            // (updated to its content width inside the block below).
            let mut child_cb_width = inh.cb_width;
            let mut child_cb_height_definite = false;
            if let Some(style) = styles.get_mut(&id) {
                match style.color { Some(c) => inh.color = Some(c), None => style.color = inh.color }
                // Resolve a relative font-size against the PARENT (em/%) or
                // ROOT (rem) font-size before inheriting it downward.
                let parent_fs = inh.font_size.unwrap_or(16.0);
                if let Some(expression) = style.font_size_expression.as_deref() {
                    style.font_size = crate::style::resolve_contextual_length(
                        expression,
                        parent_fs,
                        root_fs,
                        vw,
                        vh,
                        parent_fs,
                    );
                } else if let Some(raw) = style.font_size_raw {
                    let resolved = match raw {
                        crate::Dimension::Percent(p) => parent_fs * p,
                        crate::Dimension::Em(v) => parent_fs * v,
                        d => match d.resolve(parent_fs, root_fs, vw, vh) {
                            crate::Dimension::Px(p) => p,
                            _ => parent_fs,
                        },
                    };
                    style.font_size = Some(resolved);
                }
                match style.font_size { Some(s) => inh.font_size = Some(s), None => style.font_size = inh.font_size }
                // em in non-font-size properties is relative to this element's
                // OWN computed font-size; resolve every relative length now.
                let em_px = style.font_size.unwrap_or(parent_fs);
                if let Some(expression) = style.row_gap_expression.as_deref() {
                    style.row_gap = crate::style::resolve_contextual_length(
                        expression,
                        em_px,
                        root_fs,
                        vw,
                        vh,
                        inh.cb_width,
                    );
                }
                if let Some(expression) = style.column_gap_expression.as_deref() {
                    style.column_gap = crate::style::resolve_contextual_length(
                        expression,
                        em_px,
                        root_fs,
                        vw,
                        vh,
                        inh.cb_width,
                    );
                }
                if let Some(expression) = style.line_height_expression.as_deref() {
                    if let Some(resolved) = crate::style::resolve_contextual_length(
                        expression,
                        em_px,
                        root_fs,
                        vw,
                        vh,
                        em_px,
                    ) {
                        style.line_height = Some(
                            if crate::style::line_height_expression_is_length(
                                expression,
                            ) {
                                crate::LineHeight::Px(resolved)
                            } else {
                                crate::LineHeight::Ratio(resolved)
                            },
                        );
                    }
                } else if let Some(crate::LineHeight::Relative(relative)) =
                    style.line_height
                {
                    let pixels = match relative {
                        crate::Dimension::Percent(percent) => em_px * percent,
                        dimension => match dimension.resolve(
                            em_px,
                            root_fs,
                            vw,
                            vh,
                        ) {
                            crate::Dimension::Px(pixels) => pixels,
                            _ => em_px,
                        },
                    };
                    style.line_height = Some(crate::LineHeight::Px(pixels));
                }
                let cb_w = inh.cb_width;
                for index in 0..6 {
                    let Some(expression) = style.size_expressions[index].as_deref() else {
                        continue;
                    };
                    let percent_base = if matches!(index, 1 | 3 | 5) {
                        viewport.1
                    } else {
                        cb_w
                    };
                    let Some(px) = crate::style::resolve_contextual_length(
                        expression,
                        em_px,
                        root_fs,
                        vw,
                        vh,
                        percent_base,
                    ) else {
                        continue;
                    };
                    let resolved = crate::Dimension::Px(px);
                    match index {
                        0 => style.width = resolved,
                        1 => style.height = resolved,
                        2 => style.min_width = resolved,
                        3 => style.min_height = resolved,
                        4 => style.max_width = resolved,
                        _ => style.max_height = resolved,
                    }
                }
                style.width = style.width.resolve(em_px, root_fs, vw, vh);
                style.height = style.height.resolve(em_px, root_fs, vw, vh);
                style.min_width = style.min_width.resolve(em_px, root_fs, vw, vh);
                style.min_height = style.min_height.resolve(em_px, root_fs, vw, vh);
                style.max_width = style.max_width.resolve(em_px, root_fs, vw, vh);
                style.max_height = style.max_height.resolve(em_px, root_fs, vw, vh);
                style.flex_basis = style.flex_basis.resolve(em_px, root_fs, vw, vh);
                if matches!(style.height, crate::Dimension::Percent(_))
                    && !inh.cb_height_definite
                    && !matches!(style.position, Some(taffy::Position::Absolute))
                {
                    style.height = crate::Dimension::Auto;
                }
                child_cb_height_definite = matches!(
                    style.height,
                    crate::Dimension::Px(_) | crate::Dimension::Percent(_)
                );
                for index in 0..4 {
                    let Some(expression) =
                        style.inset_expressions[index].as_deref()
                    else {
                        continue;
                    };
                    let percent_base = if matches!(index, 1 | 3) {
                        cb_w
                    } else {
                        viewport.1
                    };
                    style.inset[index] =
                        crate::style::resolve_contextual_length(
                            expression,
                            em_px,
                            root_fs,
                            vw,
                            vh,
                            percent_base,
                        )
                        .map(crate::Dimension::Px);
                }
                for i in style.inset.iter_mut() {
                    if let Some(d) = i {
                        *i = Some(d.resolve(em_px, root_fs, vw, vh));
                    }
                }
                // A fixed box is positioned against the initial containing
                // block, whose dimensions are the viewport, not the full
                // scrollable root element. Taffy attaches out-of-flow boxes to
                // a layout node, and the root node can grow to the document's
                // full content height. Make the CSS 2.1 stretch equation
                // explicit for the ubiquitous `position:fixed; inset:0` case
                // so overlays/canvases remain exactly viewport-sized.
                if style.position_fixed {
                    if style.width == crate::Dimension::Auto {
                        if let (
                            Some(crate::Dimension::Px(right)),
                            Some(crate::Dimension::Px(left)),
                        ) = (style.inset[1], style.inset[3])
                        {
                            style.width = crate::Dimension::Px(
                                (viewport.0 - left - right).max(0.0),
                            );
                        }
                    }
                    if style.height == crate::Dimension::Auto {
                        if let (
                            Some(crate::Dimension::Px(top)),
                            Some(crate::Dimension::Px(bottom)),
                        ) = (style.inset[0], style.inset[2])
                        {
                            style.height = crate::Dimension::Px(
                                (viewport.1 - top - bottom).max(0.0),
                            );
                        }
                    }
                    child_cb_height_definite = matches!(
                        style.height,
                        crate::Dimension::Px(_) | crate::Dimension::Percent(_)
                    );
                }
                match &style.font_weight { Some(w) => inh.font_weight = Some(w.clone()), None => style.font_weight = inh.font_weight.clone() }
                match &style.font_family { Some(f) => inh.font_family = Some(f.clone()), None => style.font_family = inh.font_family.clone() }
                let is_table = tree
                    .get_node(id)
                    .map_or(false, |node| {
                        node.as_element()
                            .map_or(false, |name| name.local.as_ref() == "table")
                    });
                if is_table && inh.legacy_center && style.text_align.is_none() {
                    // The vendor alignment used by <center> centers the table
                    // outer box but does not leak into its internal formatting
                    // context. Browser UA table layout resets it to start.
                    style.text_align = Some(taffy::AlignItems::FLEX_START);
                    style.legacy_center = false;
                    inh.text_align = style.text_align;
                    inh.legacy_center = false;
                } else {
                    match style.text_align {
                        Some(a) => {
                            inh.text_align = Some(a);
                            inh.legacy_center = style.legacy_center;
                        }
                        None => {
                            style.text_align = inh.text_align;
                            style.legacy_center = inh.legacy_center;
                        }
                    }
                }
                inh.visibility_hidden = style.visibility_hidden.unwrap_or(inh.visibility_hidden);
                inh.opacity_product *= style.opacity.unwrap_or(1.0);
                style.effectively_invisible = inh.visibility_hidden || inh.opacity_product < 0.02;
                match style.list_style { Some(v) => inh.list_style = v, None => style.list_style = Some(inh.list_style) }
                match style.line_height { Some(v) => inh.line_height = v, None => style.line_height = Some(inh.line_height) }
                match style.text_transform { Some(v) => inh.text_transform = v, None => style.text_transform = Some(inh.text_transform) }
                match style.font_style_italic { Some(v) => inh.italic = v, None => style.font_style_italic = Some(inh.italic) }
                if style.box_sizing == crate::BoxSizing::Inherit {
                    style.box_sizing = inh.box_sizing;
                }
                inh.box_sizing = style.box_sizing;
                match style.border_collapse {
                    Some(value) => inh.border_collapse = value,
                    None => style.border_collapse = Some(inh.border_collapse),
                }
                let is_table_part = tree.get_node(id).map_or(false, |node| {
                    node.as_element().map_or(false, |name| {
                        matches!(
                            name.local.as_ref(),
                            "tbody" | "thead" | "tfoot" | "tr" | "td" | "th"
                        )
                    })
                });
                if is_table_part {
                    match style.vertical_align {
                        Some(value) => inh.table_vertical_align = Some(value),
                        None => style.vertical_align = inh.table_vertical_align,
                    }
                }

                // Resolve font/viewport-relative box edges now that their
                // reference sizes are known. Percentage padding/margin then
                // resolve against the containing block WIDTH on every side
                // (per CSS: `padding-top:56.25%` in a 1000px block is 562.5px,
                // not a fraction of the height). Bake the resolved px back into
                // `padding`/`margin`, which then feed taffy.
                for i in 0..4 {
                    if let Some(expression) = style.padding_expressions[i].as_deref() {
                        if let Some(px) = crate::style::resolve_contextual_length(
                            expression,
                            em_px,
                            root_fs,
                            vw,
                            vh,
                            cb_w,
                        ) {
                            match i {
                                0 => style.padding.top = px.max(0.0),
                                1 => style.padding.right = px.max(0.0),
                                2 => style.padding.bottom = px.max(0.0),
                                _ => style.padding.left = px.max(0.0),
                            }
                        }
                    }
                    if let Some(expression) = style.margin_expressions[i].as_deref() {
                        if let Some(px) = crate::style::resolve_contextual_length(
                            expression,
                            em_px,
                            root_fs,
                            vw,
                            vh,
                            cb_w,
                        ) {
                            match i {
                                0 => style.margin.top = px,
                                1 => style.margin.right = px,
                                2 => style.margin.bottom = px,
                                _ => style.margin.left = px,
                            }
                        }
                    }
                    if let Some(relative) = style.padding_relative[i] {
                        if let crate::Dimension::Px(px) =
                            relative.resolve(em_px, root_fs, vw, vh)
                        {
                            match i {
                                0 => style.padding.top = px.max(0.0),
                                1 => style.padding.right = px.max(0.0),
                                2 => style.padding.bottom = px.max(0.0),
                                _ => style.padding.left = px.max(0.0),
                            }
                        }
                    }
                    if let Some(relative) = style.margin_relative[i] {
                        if let crate::Dimension::Px(px) =
                            relative.resolve(em_px, root_fs, vw, vh)
                        {
                            match i {
                                0 => style.margin.top = px,
                                1 => style.margin.right = px,
                                2 => style.margin.bottom = px,
                                _ => style.margin.left = px,
                            }
                        }
                    }
                    if let Some(frac) = style.padding_percent[i] {
                        let px = (frac * cb_w).max(0.0);
                        match i {
                            0 => style.padding.top = px,
                            1 => style.padding.right = px,
                            2 => style.padding.bottom = px,
                            _ => style.padding.left = px,
                        }
                    }
                    if let Some(frac) = style.margin_percent[i] {
                        let px = frac * cb_w;
                        match i {
                            0 => style.margin.top = px,
                            1 => style.margin.right = px,
                            2 => style.margin.bottom = px,
                            _ => style.margin.left = px,
                        }
                    }
                }

                // Containing-block width handed to this element's children is
                // its own content-box width. A definite content-box width is
                // already that value; a border-box or auto width includes
                // padding and border, which must be removed.
                let used_w = match style.width {
                    crate::Dimension::Px(w) => w,
                    crate::Dimension::Percent(p) => p * cb_w,
                    _ => (cb_w - style.margin.left - style.margin.right).max(0.0),
                };
                let definite_content_box = matches!(
                    style.width,
                    crate::Dimension::Px(_) | crate::Dimension::Percent(_)
                ) && style.box_sizing == crate::BoxSizing::ContentBox;
                child_cb_width = if definite_content_box {
                    used_w.max(0.0)
                } else {
                    (used_w
                        - style.padding.left
                        - style.padding.right
                        - style.border.left
                        - style.border.right)
                        .max(0.0)
                };
            }
            inh.cb_width = child_cb_width;
            inh.cb_height_definite = child_cb_height_definite;
            for cid in tree.children(id).into_iter().rev() {
                queue.push((cid, inh.clone()));
            }
        }

        // Border-collapse is inherited, so only distribute a table's
        // effective spacing to the legacy flex fallback after the computed
        // top-down values are known.
        propagate_border_spacing(tree, &mut styles);

        // Resolve native input-control intrinsic border-box geometry after
        // inheritance and author cascading. Text-like inputs use the HTML
        // `size` attribute (20 by default) and the control's own computed
        // font; author CSS widths/heights remain authoritative. Without this
        // replaced-control sizing, every input is an empty auto-sized leaf
        // (0px tall and often stretched to its container).
        for (&id, style) in styles.iter_mut() {
            let Some(node) = tree.get_node(id) else { continue };
            let Some(element) = node.as_element() else { continue };
            if element.local.as_ref() != "input" {
                continue;
            }
            let input_type = node
                .get_attribute("type")
                .unwrap_or("text")
                .trim()
                .to_ascii_lowercase();
            if input_type == "hidden" {
                style.display = crate::Display::None;
                continue;
            }

            let font_size = style.font_size.unwrap_or(13.333_333).max(1.0);
            let horizontal_edges = style.padding.left
                + style.padding.right
                + style.border.left
                + style.border.right;
            let vertical_edges = style.padding.top
                + style.padding.bottom
                + style.border.top
                + style.border.bottom;
            let default_height =
                crate::inline::used_line_height(style).max(1.0) + vertical_edges;

            let (intrinsic_width, intrinsic_height) = match input_type.as_str() {
                "checkbox" | "radio" => (13.0, 13.0),
                "range" => (129.0, 16.0),
                "color" => (50.0, 27.0),
                "file" => (253.0, default_height.max(22.0)),
                "submit" | "reset" | "button" => {
                    let fallback = match input_type.as_str() {
                        "reset" => "Reset",
                        "button" => "",
                        _ => "Submit Query",
                    };
                    let label = node.get_attribute("value").unwrap_or(fallback);
                    (
                        (label.chars().count() as f32 * font_size * 0.55
                            + font_size * 1.5
                            + horizontal_edges)
                            .max(20.0),
                        default_height,
                    )
                }
                "image" => (horizontal_edges, vertical_edges),
                _ => {
                    let size = node
                        .get_attribute("size")
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|&value| value > 0)
                        .unwrap_or(20) as f32;
                    (
                        size * font_size * 0.6
                            + font_size * 0.675
                            + horizontal_edges,
                        default_height,
                    )
                }
            };
            if style.width == crate::Dimension::Auto {
                let declared_width = if style.box_sizing == crate::BoxSizing::ContentBox {
                    (intrinsic_width - horizontal_edges).max(0.0)
                } else {
                    intrinsic_width
                };
                style.width = crate::Dimension::Px(declared_width);
            }
            if style.height == crate::Dimension::Auto {
                let declared_height = if style.box_sizing == crate::BoxSizing::ContentBox {
                    (intrinsic_height - vertical_edges).max(0.0)
                } else {
                    intrinsic_height
                };
                style.height = crate::Dimension::Px(declared_height);
            }
        }

        resolve_grid_areas(tree, root_id, &mut styles);

        // Blockify flex/grid items (CSS Display 3): a direct inline child of a
        // genuine flex or grid container gets a blockified outer display while
        // retaining its own inner formatting context. Without the flex half,
        // `<pre class=flex><code>...</code></pre>` collapses the code item and
        // navigation bars treat direct `<a>` children as wrapping inline
        // containers instead of flex items.
        //
        // `internal_flex_container` is deliberately excluded: table cells use
        // a flex column only as our block-layout stand-in, and their ordinary
        // inline text must continue to form an inline formatting context.
        let item_parents: Vec<NodeId> = styles
            .iter()
            .filter(|(_, s)| {
                s.display == crate::Display::Grid
                    || (s.display == crate::Display::Flex
                        && !s.internal_flex_container)
            })
            .map(|(&id, _)| id)
            .collect();
        for pid in item_parents {
            for cid in tree.children(pid) {
                if let Some(cs) = styles.get_mut(&cid) {
                    if cs.display == crate::Display::Inline {
                        cs.display = crate::Display::Block;
                    }
                }
            }
        }

        // In an auto-height column flex container, a zero flex basis still
        // participates in intrinsic main-size calculation through the item's
        // automatic minimum size. Taffy otherwise treats `flex: 1 1` plus
        // overflow:auto as an unconditional zero contribution, collapsing the
        // entire ancestor chain even when the item contains many lines. Use
        // an auto basis for this indefinite-size phase; definite-height flex
        // containers keep the authored zero basis and normal free-space math.
        let intrinsic_column_flex_parents: Vec<NodeId> = styles
            .iter()
            .filter(|(_, style)| {
                style.display == crate::Display::Flex
                    && style.flex_direction == Some(taffy::FlexDirection::Column)
                    && style.height == crate::Dimension::Auto
            })
            .map(|(&id, _)| id)
            .collect();
        for parent in intrinsic_column_flex_parents {
            for child in tree.children(parent) {
                let Some(style) = styles.get_mut(&child) else {
                    continue;
                };
                let zero_basis = matches!(
                    style.flex_basis,
                    crate::Dimension::Px(value) | crate::Dimension::Percent(value)
                        if value == 0.0
                );
                if zero_basis
                    && style.height == crate::Dimension::Auto
                    && style.min_height == crate::Dimension::Auto
                {
                    style.flex_basis = crate::Dimension::Auto;
                }
            }
        }

        // CSS Display blockification: absolute/fixed boxes and floats compute
        // an inline outside display to block. This affects how their own mixed
        // children form lines and blocks; they remain shrink-to-fit where the
        // positioning/float algorithm requires it.
        for style in styles.values_mut() {
            if (matches!(style.position, Some(taffy::Position::Absolute)) || style.float.is_some())
                && style.display == crate::Display::Inline
            {
                style.display = crate::Display::Block;
            }
        }

        // The initial containing block is modelled by the root taffy node's
        // definite viewport height (set at build), which is what `html,body{
        // height:100%}` chains and sticky-footer app shells resolve against.
        // taffy 0.7 needed an extra `min-height: viewport` push on the root
        // style to stop those chains collapsing to content height; taffy 0.12
        // resolves them from the node's definite height directly, and that push
        // instead over-propagated the viewport height down every `height:100%`
        // subtree (nextjs.org's nav stretched to a full 688px, burying the
        // hero), so it is gone.

        // Apply fetched intrinsic image sizes. A replaced element with no
        // explicit dimensions must size from its intrinsic pixels (else it is
        // 0x0 and never paints); with one dimension given, the aspect ratio
        // fills the other. `max-width:100%` (a UA default for img) still caps
        // it to the container, and the aspect ratio keeps it proportional.
        for (&nid, &(iw, ih)) in intrinsic {
            if iw <= 0.0 || ih <= 0.0 {
                continue;
            }
            if let Some(s) = styles.get_mut(&nid) {
                s.intrinsic_size = Some((iw, ih));
                if s.aspect_ratio.is_none() {
                    s.aspect_ratio = Some(iw / ih);
                }
                let w_auto = matches!(s.width, crate::Dimension::Auto);
                let h_auto = matches!(s.height, crate::Dimension::Auto);
                if w_auto && h_auto {
                    s.width = crate::Dimension::Px(iw);
                    s.height = crate::Dimension::Px(ih);
                }
            }
        }

        if let Some(taffy_root) = build(tree, root_id, &mut taffy_tree, &mut id_map, &mut words, &mut engine, &mut ifc_items, &styles) {
            let static_position_candidates =
                reparent_inset_positioned_nodes(tree, &mut taffy_tree, taffy_root, &id_map, &styles);
            let available = taffy::Size {
                width: taffy::AvailableSpace::Definite(viewport.0),
                height: taffy::AvailableSpace::Definite(viewport.1),
            };
            #[cfg(feature = "paint")]
            {
                let engine = &mut engine;
                let mut measure =
                    |known: taffy::Size<Option<f32>>, avail: taffy::Size<taffy::AvailableSpace>, _node, ctx: Option<&mut usize>, _style: &taffy::Style| {
                        match ctx {
                            Some(&mut idx) => {
                                engine.measure_taffy(idx, known, avail)
                            }
                            None => taffy::Size::ZERO,
                        }
                    };

                // Table used-width pass. A grid table is built at width:auto, so
                // its final width has to be chosen the way CSS chooses a table's
                // used width: at least the content min-content (so an unbreakable
                // wide cell, e.g. a fixed-width image row in an infobox, makes the
                // table grow rather than overflow), at most the available width,
                // preferring the author's specified width when there is one.
                let mut tables: Vec<(taffy::NodeId, NodeId, usize)> = id_map
                    .iter()
                    .filter(|(_, dom)| {
                        tree.get_node(**dom)
                            .and_then(|n| n.as_element().map(|e| e.local.as_ref() == "table"))
                            .unwrap_or(false)
                    })
                    .map(|(t, d)| (*t, *d, dom_depth(tree, *d)))
                    .collect();
                // Deepest table first: an inner table gets its final width set
                // before an outer table measures the subtree that contains it.
                // Two tables at the same depth cannot be nested in one another
                // (nesting increases depth), so their relative order is
                // irrelevant and the final layout is deterministic even though
                // id_map iteration order is not.
                tables.sort_by(|a, b| b.2.cmp(&a.2));
                for (tnode, dom, _depth) in tables {
                    let Some(table_style) = styles.get(&dom) else {
                        continue;
                    };
                    // A percentage-width table resolves against its container, so
                    // leave taffy's percentage handling in place.
                    let width_style = Some(table_style.width);
                    if matches!(width_style, Some(crate::Dimension::Percent(_))) {
                        continue;
                    }
                    let min_c = {
                        let _ = taffy_tree.compute_layout_with_measure(
                            tnode,
                            taffy::Size { width: taffy::AvailableSpace::MinContent, height: taffy::AvailableSpace::MaxContent },
                            &mut measure,
                        );
                        taffy_tree.layout(tnode).map(|l| l.size.width).unwrap_or(0.0)
                    };
                    // A table can never be narrower than an unshrinkable
                    // fixed-width descendant. A Wikipedia infobox holds a
                    // ~267px image montage and a ~250px geologic-timeline
                    // widget, each with an explicit px width; taffy's grid
                    // min-content does not surface such a deep descendant's
                    // definite width up through the intervening flex/absolute
                    // boxes, leaving the table too narrow so those widgets
                    // overflow it. Floor min-content by the widest fixed child,
                    // matching CSS (a table's min-content is at least any
                    // fixed-width content it contains).
                    let min_c = min_c.max(max_definite_descendant_width(tree, dom, &styles).unwrap_or(0.0));
                    let max_c = {
                        let _ = taffy_tree.compute_layout_with_measure(
                            tnode,
                            taffy::Size { width: taffy::AvailableSpace::MaxContent, height: taffy::AvailableSpace::MaxContent },
                            &mut measure,
                        );
                        taffy_tree.layout(tnode).map(|l| l.size.width).unwrap_or(0.0)
                    };
                    let inline_outer_edges = table_inline_outer_edges(table_style);
                    let preferred_outer = match width_style {
                        Some(crate::Dimension::Px(w))
                            if table_style.box_sizing == crate::BoxSizing::ContentBox =>
                        {
                            w + inline_outer_edges
                        }
                        Some(crate::Dimension::Px(w)) => w,
                        _ => max_c,
                    };
                    let used_outer = preferred_outer
                        .max(min_c)
                        .min(viewport.0.max(min_c));
                    let used_declaration =
                        if table_style.box_sizing == crate::BoxSizing::ContentBox {
                            (used_outer - inline_outer_edges).max(0.0)
                        } else {
                            used_outer
                        };
                    // Distribute the used track space proportionally between
                    // each column's own min-content and max-content width, the
                    // way CSS tables do, instead of letting the grid hand every
                    // auto track an equal share of the surplus (which over-widens
                    // narrow label columns and starves wide prose columns).
                    let cells: Vec<(taffy::NodeId, usize, usize)> = taffy_tree
                        .children(tnode)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|cell| {
                            let gc = taffy_tree.style(cell).ok()?.grid_column.clone();
                            let col = match gc.start {
                                GridPlacement::Line(l) => (l.as_i16().max(1) as usize) - 1,
                                _ => return None,
                            };
                            let span = match gc.end {
                                GridPlacement::Span(s) => (s as usize).max(1),
                                _ => 1,
                            };
                            Some((cell, col, span))
                        })
                        .collect();
                    let ncols = taffy_tree
                        .style(tnode)
                        .map(|s| s.grid_template_columns.len())
                        .unwrap_or(0);
                    // Bound the extra per-cell measurement work; a pathologically
                    // large table keeps the grid's equal-share sizing (still renders).
                    if ncols > 0 && cells.len() <= 4096 {
                        // Measure every cell's own min-content and max-content width
                        // in isolation (the cell node is a valid subtree root; its
                        // grid placement only matters to its parent, so it lays out
                        // as a plain box here).
                        let mut measured: Vec<(usize, usize, f32, f32)> =
                            Vec::with_capacity(cells.len());
                        for (cell, col, span) in &cells {
                            let _ = taffy_tree.compute_layout_with_measure(
                                *cell,
                                taffy::Size { width: taffy::AvailableSpace::MinContent, height: taffy::AvailableSpace::MaxContent },
                                &mut measure,
                            );
                            let cmin = taffy_tree.layout(*cell).map(|l| l.size.width).unwrap_or(0.0);
                            let _ = taffy_tree.compute_layout_with_measure(
                                *cell,
                                taffy::Size { width: taffy::AvailableSpace::MaxContent, height: taffy::AvailableSpace::MaxContent },
                                &mut measure,
                            );
                            let cmax = taffy_tree.layout(*cell).map(|l| l.size.width).unwrap_or(0.0);
                            measured.push((*col, *span, cmin, cmax.max(cmin)));
                        }
                        let mut col_min = vec![0.0f32; ncols];
                        let mut col_max = vec![0.0f32; ncols];
                        // Pass 1: single-column cells set each column's floor.
                        for &(col, span, cmin, cmax) in &measured {
                            if span == 1 && col < ncols {
                                col_min[col] = col_min[col].max(cmin);
                                col_max[col] = col_max[col].max(cmax);
                            }
                        }
                        // Pass 2: a spanning cell that needs more than its columns
                        // currently give grows them, splitting the shortfall evenly
                        // across the spanned columns (keeps colspan lining up).
                        for &(col, span, cmin, cmax) in &measured {
                            if span <= 1 || col >= ncols {
                                continue;
                            }
                            let end = (col + span).min(ncols);
                            let n = (end - col) as f32;
                            let cur_min: f32 = col_min[col..end].iter().sum();
                            if cmin > cur_min {
                                let add = (cmin - cur_min) / n;
                                for w in &mut col_min[col..end] {
                                    *w += add;
                                }
                            }
                            let cur_max: f32 = col_max[col..end].iter().sum();
                            if cmax > cur_max {
                                let add = (cmax - cur_max) / n;
                                for w in &mut col_max[col..end] {
                                    *w += add;
                                }
                            }
                        }
                        for j in 0..ncols {
                            if col_max[j] < col_min[j] {
                                col_max[j] = col_min[j];
                            }
                        }
                        // Track space is the final table border box minus its
                        // actual border/padding, the two outer spacing bands,
                        // and the spacing between columns. Inferring this from
                        // min-content is wrong when a specified cell/column
                        // width already inflated that measurement: the fixed
                        // width gets counted as "overhead", starving later
                        // auto columns and forcing avoidable text wrapping.
                        let (horizontal_spacing, _) = table_spacing(table_style);
                        let interior_spacing =
                            horizontal_spacing * ncols.saturating_sub(1) as f32;
                        let target =
                            (used_outer - inline_outer_edges - interior_spacing)
                                .max(0.0);
                        // Specified column widths (a `<col>` or colspan-1 cell
                        // width, recorded at build time) pin their columns:
                        // px directly, percent against the table's content
                        // width. Content min-content still floors them, so a
                        // too-narrow spec never crushes its content. The
                        // remaining space interpolates across the auto
                        // columns exactly as before.
                        let specified_columns = ifc_items.table_cols.get(&tnode);
                        if let Some((spec_px, spec_pct)) = specified_columns {
                            for j in 0..ncols {
                                let spec = spec_px
                                    .get(j)
                                    .copied()
                                    .flatten()
                                    .or_else(|| spec_pct.get(j).copied().flatten().map(|p| p * target));
                                if let Some(w) = spec {
                                    let w = w.max(col_min[j]);
                                    col_min[j] = w;
                                    col_max[j] = w;
                                }
                            }
                        }
                        let sum_min: f32 = col_min.iter().sum();
                        let sum_max: f32 = col_max.iter().sum();
                        let widths: Vec<f32> = if target <= sum_min {
                            col_min.clone()
                        } else if target >= sum_max {
                            // Surplus follows the table-layout priority: auto
                            // columns absorb it before fixed and percentage
                            // columns. This is important even when every
                            // column has min==max (a one-word auto cell still
                            // fills the remainder of a definite-width table).
                            let mut result = col_max.clone();
                            let extra = target - sum_max;
                            let is_px = |j: usize| {
                                specified_columns
                                    .and_then(|(px, _)| px.get(j))
                                    .copied()
                                    .flatten()
                                    .is_some()
                            };
                            let is_pct = |j: usize| {
                                specified_columns
                                    .and_then(|(_, pct)| pct.get(j))
                                    .copied()
                                    .flatten()
                                    .is_some()
                            };
                            let mut candidates: Vec<usize> = (0..ncols)
                                .filter(|&j| !is_px(j) && !is_pct(j) && col_max[j] > 0.0)
                                .collect();
                            if candidates.is_empty() {
                                candidates = (0..ncols)
                                    .filter(|&j| !is_px(j) && !is_pct(j))
                                    .collect();
                            }
                            if candidates.is_empty() {
                                candidates = (0..ncols).filter(|&j| is_px(j)).collect();
                            }
                            if candidates.is_empty() {
                                candidates = (0..ncols).filter(|&j| is_pct(j)).collect();
                            }
                            if candidates.is_empty() {
                                candidates = (0..ncols).collect();
                            }
                            let weight_sum: f32 =
                                candidates.iter().map(|&j| col_max[j]).sum();
                            for &j in &candidates {
                                let share = if weight_sum > 0.0 {
                                    extra * col_max[j] / weight_sum
                                } else {
                                    extra / candidates.len() as f32
                                };
                                result[j] += share;
                            }
                            result
                        } else {
                            let scale = (target - sum_min) / (sum_max - sum_min);
                            col_min
                                .iter()
                                .zip(&col_max)
                                .map(|(mn, mx)| mn + (mx - mn) * scale)
                                .collect()
                        };
                        if let Ok(cur) = taffy_tree.style(tnode) {
                            let mut s = cur.clone();
                            s.size.width = length(used_declaration);
                            s.grid_template_columns = widths
                                .iter()
                                .map(|w| {
                                    taffy::GridTemplateComponent::Single(taffy::MinMax {
                                        min: taffy::MinTrackSizingFunction::length(*w),
                                        max: taffy::MaxTrackSizingFunction::length(*w),
                                    })
                                })
                                .collect();
                            let _ = taffy_tree.set_style(tnode, s);
                        }
                    } else if let Ok(cur) = taffy_tree.style(tnode) {
                        let mut s = cur.clone();
                        s.size.width = length(used_declaration);
                        let _ = taffy_tree.set_style(tnode, s);
                    }
                }

                if !static_position_candidates.is_empty() {
                    let _ = taffy_tree.compute_layout_with_measure(taffy_root, available, &mut measure);
                    resolve_static_positions_and_reparent(
                        &mut taffy_tree,
                        &static_position_candidates,
                    );
                }
                let _ = taffy_tree.compute_layout_with_measure(taffy_root, available, &mut measure);
                if apply_float_continuations(
                    tree,
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                    &ifc_items,
                ) {
                    let _ = taffy_tree.compute_layout_with_measure(
                        taffy_root,
                        available,
                        &mut measure,
                    );
                }
                if apply_table_row_geometry(
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                    &ifc_items,
                ) {
                    let _ = taffy_tree.compute_layout_with_measure(
                        taffy_root,
                        available,
                        &mut measure,
                    );
                }
                if apply_table_cell_block_alignment(
                    tree,
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                ) {
                    let _ = taffy_tree.compute_layout_with_measure(
                        taffy_root,
                        available,
                        &mut measure,
                    );
                }
            }
            #[cfg(not(feature = "paint"))]
            {
                if !static_position_candidates.is_empty() {
                    let _ = taffy_tree.compute_layout(taffy_root, available);
                    resolve_static_positions_and_reparent(
                        &mut taffy_tree,
                        &static_position_candidates,
                    );
                }
                let _ = taffy_tree.compute_layout(taffy_root, available);
                if apply_float_continuations(
                    tree,
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                    &ifc_items,
                ) {
                    let _ = taffy_tree.compute_layout(taffy_root, available);
                }
                if apply_table_row_geometry(
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                    &ifc_items,
                ) {
                    let _ = taffy_tree.compute_layout(taffy_root, available);
                }
                if apply_table_cell_block_alignment(
                    tree,
                    &mut taffy_tree,
                    &id_map,
                    &styles,
                ) {
                    let _ = taffy_tree.compute_layout(taffy_root, available);
                }
            }
            compute_absolute_rects(&taffy_tree, taffy_root, 0.0, 0.0, &id_map, &words, &mut rects, &mut text_runs, &mut anon_rects);
            synthesize_row_rects(tree, &mut rects);
        }
    }

    let mut clip_rects = HashMap::new();
    let mut translates = HashMap::new();
    if let Some(root_id) = root {
        resolve_clip_rects(tree, root_id, None, 0.0, 0.0, &rects, &styles, &mut clip_rects, &mut translates);
    }

    // Pin each shaped inline context to its final content-box origin/width now
    // that layout is done, so paint draws the same line breaks it was sized for.
    #[cfg(feature = "paint")]
    for (nid, &idx) in &ifc_items.whole {
        if let (Some(rect), Some(style)) = (rects.get(nid), styles.get(nid)) {
            let mut origin = crate::inline::content_origin(rect, style);
            let cw = crate::inline::content_width(rect, style);
            // A table cell stretched taller than its text (row height from a
            // neighbor) aligns its content per vertical-align; the pure-text
            // leaf path has no inner box to align, so shift the pinned origin
            // by the leftover space instead.
            if let Some(va @ (crate::VerticalAlign::Middle | crate::VerticalAlign::Bottom)) = style.vertical_align {
                let (_, th) = engine.measure(idx, Some(cw));
                let content_h = rect.height
                    - style.padding.top
                    - style.padding.bottom
                    - style.border.top
                    - style.border.bottom;
                let free = (content_h - th).max(0.0);
                origin.1 += if va == crate::VerticalAlign::Middle { free / 2.0 } else { free };
            }
            // The shaped text is this container's own content, so an
            // `overflow: hidden` on the container clips it (this is what keeps
            // the 1x1 "visually hidden" skip-link box from painting its text,
            // now that the text is one leaf rather than clipped word boxes).
            // Clips live in screen space; the container's own box joins the
            // chain shifted by its accumulated translate.
            let inherited = clip_rects.get(nid).copied().flatten();
            let clip = if style.overflow_hidden {
                let (tx, ty) = translates.get(nid).copied().unwrap_or((0.0, 0.0));
                let own = crate::Rect { x: rect.x + tx, y: rect.y + ty, width: rect.width, height: rect.height };
                Some(match inherited {
                    Some(c) => c.intersect(&own).unwrap_or(crate::Rect::default()),
                    None => own,
                })
            } else {
                inherited
            };
            engine.finalize(idx, origin, cw, clip);
        }
    }
    // Anonymous run leaves have no DOM node and no border/padding of their
    // own: the leaf rect IS the content box. Clipping comes from the parent
    // block (its inherited chain, plus its own overflow like any child).
    #[cfg(feature = "paint")]
    for (parent, items) in &ifc_items.runs {
        let inherited = clip_rects.get(parent).copied().flatten();
        let clip = match (styles.get(parent), rects.get(parent)) {
            (Some(style), Some(prect)) if style.overflow_hidden => {
                let (tx, ty) = translates.get(parent).copied().unwrap_or((0.0, 0.0));
                let own = crate::Rect { x: prect.x + tx, y: prect.y + ty, width: prect.width, height: prect.height };
                Some(match inherited {
                    Some(c) => c.intersect(&own).unwrap_or(crate::Rect::default()),
                    None => own,
                })
            }
            _ => inherited,
        };
        for &idx in items {
            if let Some(rect) = anon_rects.get(&idx) {
                engine.finalize(idx, (rect.x, rect.y), rect.width, clip);
            }
        }
    }

    DomLayout {
        rects,
        styles,
        clip_rects,
        translates,
        text_runs,
        #[cfg(feature = "paint")]
        text_engine: engine,
        #[cfg(feature = "paint")]
        ifc_items: ifc_items.whole,
        #[cfg(feature = "paint")]
        run_ifc_items: ifc_items.runs,
    }
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

/// `border-spacing` (from CSS or the `cellspacing` attribute) is set on a
/// `<table>`, but taffy has no native table display mode: our table is a
/// column-flex stack of row-flex `<tr>`s, so the gap that separates cells has
/// to live on each row individually. Distribute the table's border-spacing
/// down as the table's own row gap (space between stacked `<tr>`s) and every
/// descendant `<tr>`'s column gap (space between cells within a row), without
/// crossing into a nested `<table>`'s own scope.
fn propagate_border_spacing(tree: &DomTree, styles: &mut HashMap<NodeId, crate::LayoutStyle>) {
    fn local_name(tree: &DomTree, id: NodeId) -> Option<String> {
        tree.get_node(id).and_then(|n| n.as_element().map(|e| e.local.to_string()))
    }

    fn apply_to_rows(tree: &DomTree, id: NodeId, h: f32, v: f32, styles: &mut HashMap<NodeId, crate::LayoutStyle>) {
        for cid in tree.children(id) {
            if local_name(tree, cid).as_deref() == Some("table") {
                continue;
            }
            if local_name(tree, cid).as_deref() == Some("tr") {
                if let Some(s) = styles.get_mut(&cid) {
                    s.column_gap = Some(h);
                    s.row_gap = Some(v);
                    s.column_gap_expression = None;
                    s.row_gap_expression = None;
                }
            }
            apply_to_rows(tree, cid, h, v, styles);
        }
    }

    for id in tree.descendants(tree.document()) {
        if local_name(tree, id).as_deref() != Some("table") {
            continue;
        }
        let Some(table_style) = styles.get(&id) else {
            continue;
        };
        let (h, v) = table_spacing(table_style);
        if let Some(s) = styles.get_mut(&id) {
            s.row_gap = Some(v);
            s.row_gap_expression = None;
        }
        apply_to_rows(tree, id, h, v, styles);
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
        let (areas, col_lines, row_lines) = match styles.get(&id) {
            Some(s) if s.display == crate::Display::Grid => (
                s.grid_areas.clone().filter(|a| !a.is_empty()),
                s.grid_col_line_names.clone(),
                s.grid_row_line_names.clone(),
            ),
            _ => continue,
        };
        if areas.is_none() && col_lines.is_none() && row_lines.is_none() {
            continue;
        }

        if let Some(areas) = &areas {
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

        // Named grid lines: resolve children placed with `grid-column`/`grid-row`
        // values that reference a line name against this container's maps.
        if col_lines.is_some() || row_lines.is_some() {
            for cid in tree.children(id) {
                let Some(cstyle) = styles.get_mut(&cid) else { continue };
                if let (Some(raw), Some(map)) = (cstyle.grid_column_raw.clone(), &col_lines) {
                    if let Some(l) = resolve_named_placement(&raw, map) {
                        cstyle.grid_column = Some(l);
                    }
                }
                if let (Some(raw), Some(map)) = (cstyle.grid_row_raw.clone(), &row_lines) {
                    if let Some(l) = resolve_named_placement(&raw, map) {
                        cstyle.grid_row = Some(l);
                    }
                }
            }
        }
    }
}

/// Resolve a raw `grid-column`/`grid-row` value that names grid lines into a
/// numeric `taffy::Line`, using `map` (line-name -> 1-based line number). Handles
/// `a / b` (each side a name, integer, or `span n`) and the single-ident
/// `grid-column: foo` area shorthand (`foo-start / foo-end`). Returns `None` when
/// a referenced name is absent, leaving the item to auto-place.
fn resolve_named_placement(
    raw: &str,
    map: &HashMap<String, i16>,
) -> Option<taffy::Line<taffy::GridPlacement>> {
    use taffy::style_helpers::{line, span};
    let side = |tok: &str| -> Option<taffy::GridPlacement> {
        let t = tok.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("auto") {
            return Some(taffy::GridPlacement::Auto);
        }
        if let Some(n) = t.strip_prefix("span") {
            if let Ok(s) = n.trim().parse::<u16>() {
                return Some(span(s));
            }
        }
        if let Ok(i) = t.parse::<i16>() {
            return Some(line(i));
        }
        map.get(t).map(|&l| line(l))
    };
    if let Some((a, b)) = raw.split_once('/') {
        Some(taffy::Line { start: side(a)?, end: side(b)? })
    } else {
        let name = raw.trim();
        if let Some(&s) = map.get(&format!("{name}-start")) {
            let end = map
                .get(&format!("{name}-end"))
                .map(|&e| line(e))
                .unwrap_or(taffy::GridPlacement::Auto);
            return Some(taffy::Line { start: line(s), end });
        }
        map.get(name).map(|&s| taffy::Line { start: line(s), end: taffy::GridPlacement::Auto })
    }
}

fn compute_absolute_rects(
    taffy_tree: &TaffyTree<usize>,
    taffy_id: taffy::NodeId,
    abs_x: f32,
    abs_y: f32,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    words: &HashMap<taffy::NodeId, (NodeId, String)>,
    rects: &mut HashMap<NodeId, Rect>,
    text_runs: &mut HashMap<NodeId, Vec<(Rect, String)>>,
    anon_rects: &mut HashMap<usize, Rect>,
) {
    if let Ok(layout) = taffy_tree.layout(taffy_id) {
        let x = abs_x + layout.location.x;
        let y = abs_y + layout.location.y;
        let rect = Rect { x, y, width: layout.size.width, height: layout.size.height };

        if let Some(dom_id) = id_map.get(&taffy_id) {
            rects.insert(*dom_id, rect);
        } else if let Some(&item) = taffy_tree.get_node_context(taffy_id) {
            // A taffy leaf with an engine-item context but no DOM id is an
            // anonymous inline-run leaf (see `build_mixed_block`); record its
            // final rect by item index so the finalize pass can pin it.
            anon_rects.insert(item, rect);
        }
        // A word leaf's dom_id is its owning text node, shared by every other
        // word from the same node, so this appends rather than overwrites.
        if let Some((text_dom_id, word)) = words.get(&taffy_id) {
            text_runs.entry(*text_dom_id).or_default().push((rect, word.clone()));
        }

        if let Ok(children) = taffy_tree.children(taffy_id) {
            for child_id in children {
                compute_absolute_rects(taffy_tree, child_id, x, y, id_map, words, rects, text_runs, anon_rects);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct StaticPositionCandidate {
    child: taffy::NodeId,
    target: taffy::NodeId,
    inline_axis: bool,
    block_axis: bool,
}

/// Attach positioned boxes to their CSS containing block rather than their
/// immediate DOM parent.
///
/// Taffy resolves an absolute child's insets against its direct layout-tree
/// parent. CSS instead uses the nearest positioned or transformed ancestor;
/// fixed boxes use the nearest transformed ancestor or the initial containing
/// block. A box with a fully-auto axis first remains in its original formatting
/// context so taffy can produce the placeholder-like static coordinate. The
/// caller harvests that coordinate and reparents it in a bounded second pass.
fn reparent_inset_positioned_nodes(
    tree: &DomTree,
    taffy_tree: &mut TaffyTree<usize>,
    taffy_root: taffy::NodeId,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Vec<StaticPositionCandidate> {
    let reverse: HashMap<NodeId, taffy::NodeId> =
        id_map.iter().map(|(&taffy_id, &dom_id)| (dom_id, taffy_id)).collect();
    let mut nearest_abs_cb_for_children: HashMap<NodeId, taffy::NodeId> = HashMap::new();
    let mut nearest_fixed_cb_for_children: HashMap<NodeId, taffy::NodeId> = HashMap::new();
    let mut static_candidates = Vec::new();

    for dom_id in tree.descendants(tree.document()) {
        let Some(style) = styles.get(&dom_id) else { continue };
        let parent = tree.get_node(dom_id).and_then(|node| node.parent);
        let inherited_abs_cb = parent
            .and_then(|id| nearest_abs_cb_for_children.get(&id).copied())
            .unwrap_or(taffy_root);
        let inherited_fixed_cb = parent
            .and_then(|id| nearest_fixed_cb_for_children.get(&id).copied())
            .unwrap_or(taffy_root);

        // Record this before any candidate early-exit so all descendants get
        // O(1) nearest-containing-block lookups. Positioned and transformed
        // boxes capture absolute descendants; only transformed boxes capture
        // fixed descendants. The full walk stays O(n).
        let own_box = reverse.get(&dom_id).copied();
        let establishes_cb = style.establishes_positioning_containing_block();
        let abs_child_cb = if style.position.is_some() || establishes_cb {
            own_box.unwrap_or(inherited_abs_cb)
        } else {
            inherited_abs_cb
        };
        let fixed_child_cb = if establishes_cb {
            own_box.unwrap_or(inherited_fixed_cb)
        } else {
            inherited_fixed_cb
        };
        nearest_abs_cb_for_children.insert(dom_id, abs_child_cb);
        nearest_fixed_cb_for_children.insert(dom_id, fixed_child_cb);

        if !matches!(style.position, Some(taffy::Position::Absolute)) {
            continue;
        }
        let has_block_inset = style.inset[0].is_some() || style.inset[2].is_some();
        let has_inline_inset = style.inset[1].is_some() || style.inset[3].is_some();
        let Some(&child) = reverse.get(&dom_id) else { continue };
        let target = if style.position_fixed {
            inherited_fixed_cb
        } else {
            inherited_abs_cb
        };
        let Some(current) = taffy_tree.parent(child) else { continue };
        if current == target {
            continue;
        }
        if !has_block_inset || !has_inline_inset {
            static_candidates.push(StaticPositionCandidate {
                child,
                target,
                inline_axis: !has_inline_inset,
                block_axis: !has_block_inset,
            });
            continue;
        }
        if taffy_tree.remove_child(current, child).is_ok() {
            let _ = taffy_tree.add_child(target, child);
        }
    }
    static_candidates
}

fn taffy_global_origin(
    taffy_tree: &TaffyTree<usize>,
    node: taffy::NodeId,
) -> Option<(f32, f32)> {
    let mut current = Some(node);
    let mut x = 0.0;
    let mut y = 0.0;
    while let Some(id) = current {
        let layout = taffy_tree.layout(id).ok()?;
        x += layout.location.x;
        y += layout.location.y;
        current = taffy_tree.parent(id);
    }
    Some((x, y))
}

fn collect_taffy_global_rects(
    taffy_tree: &TaffyTree<usize>,
    node: taffy::NodeId,
    parent_x: f32,
    parent_y: f32,
    rects: &mut HashMap<taffy::NodeId, Rect>,
) {
    let Ok(layout) = taffy_tree.layout(node) else {
        return;
    };
    let x = parent_x + layout.location.x;
    let y = parent_y + layout.location.y;
    rects.insert(
        node,
        Rect {
            x,
            y,
            width: layout.size.width,
            height: layout.size.height,
        },
    );
    for child in taffy_tree.children(node).unwrap_or_default() {
        collect_taffy_global_rects(taffy_tree, child, x, y, rects);
    }
}

#[derive(Clone, Copy)]
struct FloatBand {
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
    side: crate::Float,
}

fn narrow_node_to_float_band(
    taffy_tree: &mut TaffyTree<usize>,
    node: taffy::NodeId,
    preliminary_rects: &HashMap<taffy::NodeId, Rect>,
    band: FloatBand,
) -> bool {
    let Some(&rect) = preliminary_rects.get(&node) else {
        return false;
    };
    if rect.y >= band.bottom
        || rect.y + rect.height <= band.top
        || rect.width <= 0.0
    {
        return false;
    }
    let Ok(current) = taffy_tree.style(node) else {
        return false;
    };
    let Ok(layout) = taffy_tree.layout(node) else {
        return false;
    };
    let mut narrowed = current.clone();
    let (available, left_shift) = match band.side {
        crate::Float::Right => {
            if band.left >= rect.x + rect.width {
                return false;
            }
            ((band.left - rect.x).max(0.0), 0.0)
        }
        crate::Float::Left => {
            if band.right <= rect.x {
                return false;
            }
            let shift = (band.right - rect.x).max(0.0);
            ((rect.width - shift).max(0.0), shift)
        }
    };
    if available >= rect.width - 0.01 {
        return false;
    }
    let specified = if current.box_sizing == taffy::BoxSizing::ContentBox {
        (available
            - layout.padding.left
            - layout.padding.right
            - layout.border.left
            - layout.border.right)
            .max(0.0)
    } else {
        available
    };
    narrowed.size.width = taffy::Dimension::length(specified);
    narrowed.max_size.width = taffy::Dimension::length(specified);
    if left_shift > 0.0 {
        narrowed.margin.left =
            taffy::LengthPercentageAuto::length(layout.margin.left + left_shift);
    }
    taffy_tree.set_style(node, narrowed).is_ok()
}

fn grow_bfc_to_float_bottom(
    taffy_tree: &mut TaffyTree<usize>,
    node: taffy::NodeId,
    preliminary_rects: &HashMap<taffy::NodeId, Rect>,
    float_bottom: f32,
) -> bool {
    let Some(&rect) = preliminary_rects.get(&node) else {
        return false;
    };
    let desired_border_height = (float_bottom - rect.y).max(0.0);
    if desired_border_height <= rect.height + 0.01 {
        return false;
    }
    let Ok(current) = taffy_tree.style(node) else {
        return false;
    };
    let Ok(layout) = taffy_tree.layout(node) else {
        return false;
    };
    let specified = if current.box_sizing == taffy::BoxSizing::ContentBox {
        (desired_border_height
            - layout.padding.top
            - layout.padding.bottom
            - layout.border.top
            - layout.border.bottom)
            .max(0.0)
    } else {
        desired_border_height
    };
    let mut grown = current.clone();
    grown.min_size.height = taffy::Dimension::length(specified);
    taffy_tree.set_style(node, grown).is_ok()
}

fn narrow_intersecting_descendants(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    reverse: &HashMap<NodeId, taffy::NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    ifc_items: &IfcRegistry,
    preliminary_rects: &HashMap<taffy::NodeId, Rect>,
    band: FloatBand,
) -> bool {
    let Some(&node) = reverse.get(&id) else {
        return false;
    };
    let Some(&rect) = preliminary_rects.get(&node) else {
        return false;
    };
    if rect.y >= band.bottom || rect.y + rect.height <= band.top {
        return false;
    }
    let Some(style) = styles.get(&id) else {
        return false;
    };
    if style.display == crate::Display::None
        || style.float.is_some()
        || matches!(style.position, Some(taffy::Position::Absolute))
    {
        return false;
    }

    // Float-avoiding formatting contexts move as one box. A shaped IFC or a
    // leaf has no deeper block boundary at which its width can change, so it
    // also consumes the current band as a unit.
    let in_flow_element_children: Vec<NodeId> = tree
        .children(id)
        .into_iter()
        .filter(|child| {
            styles.get(child).map_or(false, |child_style| {
                child_style.display != crate::Display::None
                    && child_style.float.is_none()
                    && !matches!(
                        child_style.position,
                        Some(taffy::Position::Absolute)
                    )
            })
        })
        .collect();
    if establishes_block_formatting_context(style)
        || ifc_items.whole.contains_key(&id)
        || in_flow_element_children.is_empty()
    {
        return narrow_node_to_float_band(
            taffy_tree,
            node,
            preliminary_rects,
            band,
        );
    }

    let mut changed = false;
    for child in in_flow_element_children {
        changed |= narrow_intersecting_descendants(
            tree,
            child,
            taffy_tree,
            reverse,
            styles,
            ifc_items,
            preliminary_rects,
            band,
        );
    }
    changed
}

fn apply_float_continuations(
    tree: &DomTree,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    ifc_items: &IfcRegistry,
) -> bool {
    let reverse: HashMap<NodeId, taffy::NodeId> =
        id_map.iter().map(|(&taffy, &dom)| (dom, taffy)).collect();
    let Some(root) = id_map
        .keys()
        .copied()
        .find(|node| taffy_tree.parent(*node).is_none())
    else {
        return false;
    };
    let mut preliminary_rects = HashMap::with_capacity(id_map.len());
    collect_taffy_global_rects(
        taffy_tree,
        root,
        0.0,
        0.0,
        &mut preliminary_rects,
    );
    let mut changed = false;
    for continuation in &ifc_items.float_continuations {
        let Some(&float_rect) = preliminary_rects.get(&continuation.float) else {
            continue;
        };
        let Ok(float_layout) = taffy_tree.layout(continuation.float) else {
            continue;
        };
        let Some(&flow_rect) = preliminary_rects.get(&continuation.flow) else {
            continue;
        };
        let band = FloatBand {
            top: float_rect.y - float_layout.margin.top,
            bottom: float_rect.y + float_rect.height + float_layout.margin.bottom,
            left: float_rect.x - float_layout.margin.left,
            right: float_rect.x + float_rect.width + float_layout.margin.right,
            side: continuation.side,
        };
        if band.bottom <= flow_rect.y + flow_rect.height + 0.01 {
            continue;
        }

        // A non-BFC wrapper is transparent to the BFC's float manager. Visit
        // the wrapper's following siblings, then repeat at each ancestor until
        // (and including) the nearest ancestor BFC. Descend through ordinary
        // blocks so only the leaf/block bands that actually intersect the
        // float are narrowed; later siblings below the float stay full width.
        let mut current = continuation.owner;
        while let Some(parent) = tree.get_node(current).and_then(|node| node.parent)
        {
            let siblings = tree.children(parent);
            let Some(index) = siblings.iter().position(|candidate| *candidate == current)
            else {
                break;
            };
            for sibling in &siblings[index + 1..] {
                changed |= narrow_intersecting_descendants(
                    tree,
                    *sibling,
                    taffy_tree,
                    &reverse,
                    styles,
                    ifc_items,
                    &preliminary_rects,
                    band,
                );
            }
            let reached_bfc = styles
                .get(&parent)
                .map(establishes_block_formatting_context)
                .unwrap_or(false);
            if reached_bfc {
                if styles
                    .get(&parent)
                    .map(|style| matches!(style.height, crate::Dimension::Auto))
                    .unwrap_or(false)
                {
                    if let Some(&bfc_node) = reverse.get(&parent) {
                        changed |=
                            grow_bfc_to_float_bottom(
                                taffy_tree,
                                bfc_node,
                                &preliminary_rects,
                                band.bottom,
                            );
                    }
                }
                break;
            }
            current = parent;
        }
    }
    changed
}

/// Recompute table row tracks from the cells' content at their final column
/// widths. Generic grid intrinsic sizing can retain a taller contribution
/// from an earlier, narrower nested-table measurement even after that child
/// resolves to a shorter final box. CSS tables instead finish columns first,
/// measure cells at those widths, then derive rows bottom-up.
fn apply_table_row_geometry(
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    ifc_items: &IfcRegistry,
) -> bool {
    let mut changed = false;
    for (&table_node, row_minimums) in &ifc_items.table_rows {
        let nrows = row_minimums.len();
        if nrows == 0 {
            continue;
        }
        let mut row_heights: Vec<f32> = row_minimums
            .iter()
            .map(|height| height.unwrap_or(0.0).max(0.0))
            .collect();
        let mut cells = Vec::new();
        for cell in taffy_tree.children(table_node).unwrap_or_default() {
            let (Ok(cell_style), Ok(layout)) =
                (taffy_tree.style(cell), taffy_tree.layout(cell))
            else {
                continue;
            };
            let row = match cell_style.grid_row.start {
                GridPlacement::Line(line) => (line.as_i16().max(1) as usize) - 1,
                _ => continue,
            };
            let span = match cell_style.grid_row.end {
                GridPlacement::Span(span) => (span as usize).max(1),
                _ => 1,
            };
            if row >= nrows {
                continue;
            }
            let edges = layout.padding.top
                + layout.padding.bottom
                + layout.border.top
                + layout.border.bottom;
            // Taffy's content_size is the furthest content/child overflow in
            // border-box coordinates (an empty padded cell reports its padding
            // extent), so it is already the right natural border-box floor.
            let mut natural = layout.content_size.height.max(edges);
            if let Some(dom_id) = id_map.get(&cell) {
                if let Some(style) = styles.get(dom_id) {
                    if let crate::Dimension::Px(height) = style.height {
                        let specified = if style.box_sizing == crate::BoxSizing::ContentBox {
                            height + style.padding.top
                                + style.padding.bottom
                                + style.border.top
                                + style.border.bottom
                        } else {
                            height
                        };
                        natural = natural.max(specified);
                    }
                }
            }
            cells.push((cell, row, span.min(nrows - row), natural.max(0.0)));
        }

        // Non-spanning cells define their row directly.
        for &(_, row, span, natural) in &cells {
            if span == 1 {
                row_heights[row] = row_heights[row].max(natural);
            }
        }

        let table_dom = id_map.get(&table_node).copied();
        let (row_gap, table_has_height) = table_dom
            .and_then(|dom_id| styles.get(&dom_id))
            .map(|style| {
                (
                    table_spacing(style).1,
                    matches!(style.height, crate::Dimension::Px(_)),
                )
            })
            .unwrap_or((0.0, false));

        // A rowspan contributes only the shortfall beyond the rows and gaps
        // it spans. Prefer unstyled rows; when they already have content,
        // distribute proportionally, matching the table row-group algorithm.
        for &(_, row, span, natural) in &cells {
            if span <= 1 {
                continue;
            }
            let end = row + span;
            let current: f32 = row_heights[row..end].iter().sum::<f32>()
                + row_gap * span.saturating_sub(1) as f32;
            let extra = natural - current;
            if extra <= 0.0 {
                continue;
            }
            let mut targets: Vec<usize> = (row..end)
                .filter(|&index| row_minimums[index].is_none())
                .collect();
            if targets.is_empty() {
                targets = (row..end).collect();
            }
            let weight: f32 = targets.iter().map(|&index| row_heights[index]).sum();
            for &index in &targets {
                let share = if weight > 0.0 {
                    extra * row_heights[index] / weight
                } else {
                    extra / targets.len() as f32
                };
                row_heights[index] += share;
            }
        }

        // A definite table height distributes surplus into rows; an auto
        // table must not preserve stale free space from the generic grid pass.
        if table_has_height {
            if let Ok(table_layout) = taffy_tree.layout(table_node) {
                let track_target = (table_layout.size.height
                    - table_layout.padding.top
                    - table_layout.padding.bottom
                    - table_layout.border.top
                    - table_layout.border.bottom
                    - row_gap * nrows.saturating_sub(1) as f32)
                    .max(0.0);
                let current: f32 = row_heights.iter().sum();
                let extra = track_target - current;
                if extra > 0.0 {
                    let mut targets: Vec<usize> = (0..nrows)
                        .filter(|&index| row_minimums[index].is_none())
                        .collect();
                    if targets.is_empty() {
                        targets = (0..nrows).collect();
                    }
                    let weight: f32 =
                        targets.iter().map(|&index| row_heights[index]).sum();
                    for &index in &targets {
                        let share = if weight > 0.0 {
                            extra * row_heights[index] / weight
                        } else {
                            extra / targets.len() as f32
                        };
                        row_heights[index] += share;
                    }
                }
            }
        }

        if let Ok(current) = taffy_tree.style(table_node) {
            let mut fixed = current.clone();
            fixed.grid_template_rows = row_heights
                .iter()
                .map(|height| {
                    taffy::GridTemplateComponent::Single(taffy::MinMax {
                        min: taffy::MinTrackSizingFunction::length(*height),
                        max: taffy::MaxTrackSizingFunction::length(*height),
                    })
                })
                .collect();
            if taffy_tree.set_style(table_node, fixed).is_ok() {
                changed = true;
            }
        }
        // A CSS height on a cell is a row minimum, not a smaller final cell
        // box. Once the tracks are pinned, restore auto block-size so every
        // cell stretches to its complete (possibly spanned) grid area.
        for (cell, _, _, _) in cells {
            if let Ok(current) = taffy_tree.style(cell) {
                let mut stretched = current.clone();
                stretched.size.height = taffy::Dimension::auto();
                let _ = taffy_tree.set_style(cell, stretched);
            }
        }
    }
    changed
}

/// Table-cell block alignment is a post-row-sizing operation. Feeding
/// `justify-content:center/end` into a cell while grid is still computing its
/// auto row tracks lets alignment offsets contaminate the cell's intrinsic
/// block-size (a nested 24px table can incorrectly demand a 29px row). Measure
/// cells from block-start first, freeze each final cell border-box height, then
/// enable middle/bottom alignment for the final layout pass.
fn apply_table_cell_block_alignment(
    tree: &DomTree,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> bool {
    let mut changed = false;
    for (&node, &dom_id) in id_map {
        let Some(style) = styles.get(&dom_id) else {
            continue;
        };
        if !style.internal_flex_container
            || !matches!(
                style.vertical_align,
                Some(crate::VerticalAlign::Middle | crate::VerticalAlign::Bottom)
            )
        {
            continue;
        }
        let is_cell = tree.get_node(dom_id).map_or(false, |dom_node| {
            dom_node.as_element().map_or(false, |element| {
                matches!(element.local.as_ref(), "td" | "th")
            })
        });
        if !is_cell || taffy_tree.children(node).map_or(true, |children| children.is_empty()) {
            // Pure-text cells are aligned after shaping in the text finalize
            // path, where their actual line box height is available.
            continue;
        }
        let (Ok(current), Ok(layout)) = (taffy_tree.style(node), taffy_tree.layout(node)) else {
            continue;
        };
        if current.display != taffy::style::Display::Flex {
            continue;
        }
        let mut aligned = current.clone();
        let border_padding = layout.border.top
            + layout.border.bottom
            + layout.padding.top
            + layout.padding.bottom;
        let declared_height = if aligned.box_sizing == taffy::BoxSizing::ContentBox {
            (layout.size.height - border_padding).max(0.0)
        } else {
            layout.size.height
        };
        aligned.size.height = length(declared_height);
        let is_middle = style.vertical_align == Some(crate::VerticalAlign::Middle);
        if aligned.flex_direction == taffy::FlexDirection::Column {
            aligned.justify_content = Some(if is_middle {
                taffy::JustifyContent::CENTER
            } else {
                taffy::JustifyContent::FLEX_END
            });
        } else {
            aligned.align_content = Some(if is_middle {
                taffy::AlignContent::CENTER
            } else {
                taffy::AlignContent::FLEX_END
            });
        }
        if taffy_tree.set_style(node, aligned).is_ok() {
            changed = true;
        }
    }
    changed
}

fn resolve_static_positions_and_reparent(
    taffy_tree: &mut TaffyTree<usize>,
    candidates: &[StaticPositionCandidate],
) {
    // Harvest every placeholder coordinate before mutating parent links.
    // Otherwise reparenting an outer candidate changes the global-origin walk
    // for a nested candidate while both still contain preliminary-pass layout
    // coordinates.
    let mut resolved = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let Some(child_origin) = taffy_global_origin(taffy_tree, candidate.child) else { continue };
        let Some(target_origin) = taffy_global_origin(taffy_tree, candidate.target) else { continue };
        let Ok(child_layout) = taffy_tree.layout(candidate.child) else { continue };
        let child_margin = child_layout.margin;
        let Ok(target_layout) = taffy_tree.layout(candidate.target) else { continue };
        let target_border = target_layout.border;
        let Ok(current_style) = taffy_tree.style(candidate.child) else { continue };
        let mut style = current_style.clone();

        if candidate.inline_axis {
            style.inset.left =
                length(child_origin.0 - target_origin.0 - target_border.left - child_margin.left);
        }
        if candidate.block_axis {
            style.inset.top =
                length(child_origin.1 - target_origin.1 - target_border.top - child_margin.top);
        }
        resolved.push((*candidate, style));
    }

    for (candidate, style) in resolved {
        let Some(current) = taffy_tree.parent(candidate.child) else { continue };
        if taffy_tree.remove_child(current, candidate.child).is_err() {
            continue;
        }
        if taffy_tree.add_child(candidate.target, candidate.child).is_err() {
            let _ = taffy_tree.add_child(current, candidate.child);
            continue;
        }
        if taffy_tree.set_style(candidate.child, style).is_err() {
            let _ = taffy_tree.remove_child(candidate.target, candidate.child);
            let _ = taffy_tree.add_child(current, candidate.child);
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
            _ => styles
                .get(&cid)
                .map(|s| {
                    // A display:contents wrapper is transparent: whether it
                    // reads as inline content depends on what it splices in.
                    if s.display_contents && s.display != crate::Display::None {
                        has_inline_content(tree, cid, styles)
                    } else {
                        // Out-of-flow boxes (absolutely positioned, floated)
                        // are not inline content: an inline that is the sole
                        // child of a hero wrapper must not drag the wrapper
                        // into the inline-formatting path.
                        s.display == crate::Display::Inline
                            && !matches!(s.position, Some(taffy::Position::Absolute))
                            && s.float.is_none()
                    }
                })
                .unwrap_or(false),
        }
    })
}

/// Build whichever of an element or a text node `id` is, returning every
/// taffy node it produced (usually one, but a text node fans out to one leaf
/// per word; see `build_text_words`). Callers collecting a container's
/// children should use this instead of calling `build` directly, so text
/// nodes flatten into the same child list as their sibling elements rather
/// than being skipped or nested a level deeper (either of which would break
/// wrapping: the whole point is for words from different text nodes and
/// inline elements to sit as flat, interleaved siblings that a flex-wrap row
/// can break between anywhere).
fn build_any(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut IfcRegistry,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Vec<taffy::NodeId> {
    let is_text = tree
        .get_node(id)
        .map(|n| matches!(n.data, obscura_dom::tree::NodeData::Text { .. }))
        .unwrap_or(false);
    if is_text {
        return build_text_words(tree, id, taffy_tree, styles, words);
    }
    // `display: contents` removes the element's own box; its children lay out as
    // if they were direct children of its parent (CSS Display 3). Splice them
    // into the caller's child list (same trick as inline wrappers below).
    // Without this the wrapper became a normal auto-sized box (news-site card
    // anchors are `<a style="display:contents">` around the image), which
    // collapsed to zero area and blanked the image inside (bbc.com/news hero,
    // theguardian.com cards). `display:none` still wins.
    let splices_children = styles
        .get(&id)
        .map(|s| s.display_contents && s.display != crate::Display::None)
        .unwrap_or(false);
    if splices_children {
        return tree
            .children(id)
            .into_iter()
            .flat_map(|cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
            .collect();
    }
    if is_flattenable_inline(tree, id, styles) {
        // A plain inline wrapper (`<span>`, `<a>`, `<b>`, ...) around text
        // with no box appearance of its own: giving it its own flex-wrap
        // container, like every other element gets, would make *it* the
        // inline-formatting context instead of the enclosing block, which is
        // backwards. Real inline boxes do not wrap independently; the words
        // they contain wrap as part of the single line-breaking process run
        // by their nearest block ancestor. Concretely, a taffy flex-wrap
        // container with an auto width sizes itself from a "measure" pass
        // that does not yet know the final available width, so it collapses
        // toward a min-content (one word wide) guess — visible on Wikipedia's
        // "53 languages" label, which wrapped to "53" / "languages" on two
        // lines purely because the wrapping `<span>` around that text was
        // itself an independent auto-width wrap container. Flattening the
        // wrapper's children into the caller's list (same trick already used
        // for text nodes in `build_text_words`) fixes this at the root: the
        // words become flat siblings that wrap only at the real block level.
        return tree
            .children(id)
            .into_iter()
            .flat_map(|cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
            .collect();
    }
    build(tree, id, taffy_tree, id_map, words, engine, ifc_items, styles).into_iter().collect()
}

/// Is `id` a `display: inline` element with no box appearance or sizing of
/// its own — safe to flatten into its parent's child list instead of giving
/// it an independent (and, for wrapping auto-width containers, buggy) flex
/// context? Covers the dominant real-world case: `<a>`/`<span>`/`<b>`/etc.
/// wrapping plain text with no inline styling.
fn is_flattenable_inline(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, crate::LayoutStyle>) -> bool {
    let Some(node) = tree.get_node(id) else { return false };
    if node.as_element().is_none() {
        return false;
    }
    let Some(style) = styles.get(&id) else { return false };
    style.display == crate::Display::Inline
        && !style.is_inline_block
        && style.before_content.is_none()
        && style.after_content.is_none()
        && style.background_color.is_none()
        && style.background_image.is_none()
        && style.mask_image.is_none()
        && style.border == crate::Edges::default()
        && style.width == crate::Dimension::Auto
        && style.height == crate::Dimension::Auto
        && style.position.is_none()
        && !style.overflow_hidden
        && style.float.is_none()
}

/// Split a text node into one taffy leaf per word (a whitespace-delimited
/// token, each keeping a single trailing space so adjacent words stay
/// visually separated), so it can wrap across several lines within its
/// container instead of being one indivisible box. A single leaf per whole
/// text node cannot wrap internally: flex-wrap only ever breaks *between*
/// items, so a long run of plain text with no inline elements breaking it up
/// (a very common shape: prose without a link for several sentences) would
/// either fit as one giant item or overflow straight past the container's
/// edge, never wrapping onto a new line the way real text does.
fn build_text_words(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
) -> Vec<taffy::NodeId> {
    let Some(node) = tree.get_node(id) else { return Vec::new() };
    let obscura_dom::tree::NodeData::Text { contents } = &node.data else { return Vec::new() };

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

    build_word_leaves(id, &display_text, fsize, is_bold, taffy_tree, words)
}

/// Split `text` into one taffy leaf per word and register each against
/// `source_id` in `words`. Shared by `build_text_words` (a real DOM text
/// node) and `build_pseudo_content` (a `::before`/`::after` literal, which
/// has no text node of its own — `source_id` is the host element instead).
fn build_word_leaves(
    source_id: NodeId,
    text: &str,
    fsize: f32,
    is_bold: bool,
    taffy_tree: &mut TaffyTree<usize>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
) -> Vec<taffy::NodeId> {
    tokenize_with_spaces(text)
        .into_iter()
        .filter_map(|token| {
            let width = text_width(&token, fsize, is_bold);
            // A pure-whitespace token is HTML source formatting or a bare
            // inter-element space; it keeps its (small) width so adjacent
            // inline content stays visually separated, but contributes no
            // height, so it never adds a spurious blank row when it lands
            // between block-level siblings (e.g. formatting whitespace
            // around a run of now-collapsed, display:none list items).
            let height = if token.trim().is_empty() { 0.0 } else { fsize * 1.2 };
            let taffy_style = taffy::Style {
                size: taffy::Size { width: taffy::Dimension::length(width), height: taffy::Dimension::length(height) },
                ..Default::default()
            };
            let taffy_id = taffy_tree.new_leaf(taffy_style).ok()?;
            words.insert(taffy_id, (source_id, token));
            Some(taffy_id)
        })
        .collect()
}

/// Build the word leaves for a `::before`/`::after` literal `content`,
/// registered against the host element's own id (there is no real DOM text
/// node backing generated content, so its geometry is reported under the
/// element itself in `DomLayout::text_runs`, and painted the same way a real
/// text node's words are).
fn build_pseudo_content(
    id: NodeId,
    content: &str,
    style: &crate::LayoutStyle,
    taffy_tree: &mut TaffyTree<usize>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
) -> Vec<taffy::NodeId> {
    let fsize = style.font_size.unwrap_or(16.0);
    let is_bold = style.font_weight.as_deref() == Some("bold");
    build_word_leaves(id, content, fsize, is_bold, taffy_tree, words)
}

/// Split already whitespace-collapsed text into tokens, each carrying at most
/// one trailing space (`"Hello World "` -> `["Hello ", "World "]`), so a
/// word's own width naturally includes the gap to the next one.
fn tokenize_with_spaces(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if c == ' ' {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Give each `<tr>`/`<tbody>`/`<thead>`/`<tfoot>` its CSS table-grid band. In
/// the grid table model these wrappers are not taffy boxes, so without this
/// their backgrounds, borders, and DOM geometry would disappear.
///
/// A row's inline extent includes all cells that start in it, but its block
/// extent must ignore the portion of a `rowspan` that continues through later
/// rows. Nested-table cells must not participate at all. Sections are then the
/// union of their already-synthesized direct rows.
fn synthesize_row_rects(tree: &DomTree, rects: &mut HashMap<NodeId, Rect>) {
    let mut rows = Vec::new();
    let mut sections = Vec::new();
    let mut table_inline: HashMap<NodeId, Rect> = HashMap::new();
    for id in tree.descendants(tree.document()) {
        let local = match tree.get_node(id).and_then(|n| n.as_element().map(|e| e.local.to_string())) {
            Some(l) => l,
            None => continue,
        };
        match local.as_str() {
            "tr" => rows.push(id),
            "tbody" | "thead" | "tfoot" => sections.push(id),
            "td" | "th" => {
                let mut ancestor = tree.get_node(id).and_then(|node| node.parent);
                while let Some(parent) = ancestor {
                    let is_table = tree.get_node(parent).map_or(false, |node| {
                        node.as_element()
                            .map_or(false, |element| element.local.as_ref() == "table")
                    });
                    if is_table {
                        if let Some(rect) = rects.get(&id) {
                            table_inline
                                .entry(parent)
                                .and_modify(|current| *current = current.union(rect))
                                .or_insert(*rect);
                        }
                        break;
                    }
                    ancestor = tree.get_node(parent).and_then(|node| node.parent);
                }
            }
            _ => {}
        }
    }

    for id in rows {
        if rects.contains_key(&id) {
            continue;
        }
        let mut inline: Option<Rect> = None;
        let mut block: Option<Rect> = None;
        for cell in tree.children(id) {
            let is_cell = tree
                .get_node(cell)
                .and_then(|n| n.as_element().map(|e| matches!(e.local.as_ref(), "td" | "th")))
                .unwrap_or(false);
            if !is_cell {
                continue;
            }
            let Some(r) = rects.get(&cell) else {
                continue;
            };
            inline = Some(match inline {
                Some(a) => a.union(r),
                None => *r,
            });
            let spans_one_row = tree
                .get_node(cell)
                .and_then(|node| {
                    node.get_attribute("rowspan")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .map_or(true, |span| span == 1);
            if spans_one_row {
                block = Some(match block {
                    Some(a) => a.union(r),
                    None => *r,
                });
            }
        }
        if let Some(mut inline) = inline {
            let mut ancestor = tree.get_node(id).and_then(|node| node.parent);
            while let Some(parent) = ancestor {
                if let Some(table_band) = table_inline.get(&parent) {
                    inline.x = table_band.x;
                    inline.width = table_band.width;
                    break;
                }
                ancestor = tree.get_node(parent).and_then(|node| node.parent);
            }
            let block = block.unwrap_or(inline);
            rects.insert(
                id,
                Rect {
                    x: inline.x,
                    y: block.y,
                    width: inline.width,
                    height: block.height,
                },
            );
        }
    }

    for id in sections {
        if rects.contains_key(&id) {
            continue;
        }
        let mut section: Option<Rect> = None;
        for row in tree.children(id) {
            let is_row = tree
                .get_node(row)
                .map_or(false, |node| {
                    node.as_element()
                        .map_or(false, |element| element.local.as_ref() == "tr")
                });
            if !is_row {
                continue;
            }
            if let Some(rect) = rects.get(&row) {
                section = Some(match section {
                    Some(current) => current.union(rect),
                    None => *rect,
                });
            }
        }
        if let Some(rect) = section {
            rects.insert(id, rect);
        }
    }
}

/// Collect `<tr>` elements of a table in document order, descending through
/// `<thead>`/`<tbody>`/`<tfoot>` section wrappers but never into a cell or a
/// nested `<table>` (whose rows belong to that inner table).
/// Number of ancestors of `id` up to the document root. Used to order nested
/// tables deepest-first in the table used-width pass.
fn dom_depth(tree: &DomTree, id: NodeId) -> usize {
    let mut d = 0usize;
    let mut cur = id;
    while let Some(p) = tree.get_node(cur).and_then(|n| n.parent) {
        d += 1;
        cur = p;
        if d > 4096 {
            break;
        }
    }
    d
}

fn collect_table_rows(tree: &DomTree, id: NodeId, rows: &mut Vec<NodeId>) {
    for cid in tree.children(id) {
        let local = tree.get_node(cid).and_then(|n| n.as_element().map(|e| e.local.to_string()));
        match local.as_deref() {
            Some("tr") => rows.push(cid),
            Some("thead") | Some("tbody") | Some("tfoot") => collect_table_rows(tree, cid, rows),
            _ => {}
        }
    }
}

/// Effective separate-border spacing. Collapsed tables contribute no spacing
/// at either the outer table edges or between tracks.
fn table_spacing(style: &crate::LayoutStyle) -> (f32, f32) {
    if style.border_collapse.unwrap_or(false) {
        (0.0, 0.0)
    } else {
        style.border_spacing.unwrap_or((0.0, 0.0))
    }
}

/// Horizontal non-track area in the table's border box, excluding the gaps
/// *between* columns: authored border/padding plus one border-spacing unit at
/// each outer edge.
fn table_inline_outer_edges(style: &crate::LayoutStyle) -> f32 {
    let (spacing, _) = table_spacing(style);
    style.border.left
        + style.border.right
        + style.padding.left
        + style.padding.right
        + spacing * 2.0
}

/// Build a `<table>` as a CSS grid. Modeling the table as a grid is what makes
/// columns negotiate a shared width across every row (min-content/max-content
/// track sizing), which the old flex-row-per-`<tr>` stack could not do: each
/// row sized its cells independently, so columns drifted and never lined up.
/// Cells (`<td>`/`<th>`) become grid items placed by (row, column) with
/// colspan/rowspan mapped to grid spans; `<tr>`/`<tbody>`/`<thead>`/`<tfoot>`
/// do not get their own layout boxes (their backgrounds are not modeled yet).
/// The grid node is created at width:auto here; its final width is set later in
/// `layout_dom` by a two-pass intrinsic measurement (see the table-sizing pass)
/// so the table can grow to fit an unbreakable wide cell the way real tables do.
/// Returns `None` (falling back to the generic path) if the table has no cells.
fn build_table(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut IfcRegistry,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Option<taffy::NodeId> {
    let style = styles.get(&id)?;
    let mut rows: Vec<NodeId> = Vec::new();
    collect_table_rows(tree, id, &mut rows);
    if rows.is_empty() {
        return None;
    }
    // Bounds so a crafted table cannot exhaust memory or time: `colspan`/
    // `rowspan` and the column count are page-controlled, and the occupancy
    // fill is O(rows x cols) native code that neither the V8 watchdog nor a
    // tokio timeout can interrupt. Real tables never approach these. Rows are
    // capped too, both to bound work and to keep grid line/span indices within
    // taffy's i16/u16 range.
    const MAX_SPAN: usize = 1000;
    const MAX_COLS: usize = 1024;
    const MAX_ROWS: usize = 10000;
    if rows.len() > MAX_ROWS {
        rows.truncate(MAX_ROWS);
    }
    let nrows = rows.len();

    // Assign every cell a (row, column) with a rowspan-occupancy grid so a cell
    // that spans down pushes later rows' cells past the columns it still covers.
    let span_attr = |cid: NodeId, name: &str| -> usize {
        tree.get_node(cid)
            .and_then(|n| n.get_attribute(name).and_then(|v| v.trim().parse::<usize>().ok()))
            .unwrap_or(1)
    };
    let mut occupied: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut placed: Vec<(NodeId, usize, usize, usize, usize)> = Vec::new();
    let mut ncols = 0usize;
    for (r, &tr) in rows.iter().enumerate() {
        let mut c = 0usize;
        for cid in tree.children(tr) {
            let local = tree.get_node(cid).and_then(|n| n.as_element().map(|e| e.local.to_string()));
            if !matches!(local.as_deref(), Some("td") | Some("th")) {
                continue;
            }
            // A hidden cell is removed from the table model entirely (it must
            // not reserve a column slot, or the surviving cells shift right).
            if styles.get(&cid).map(|s| s.display == crate::Display::None).unwrap_or(false) {
                continue;
            }
            while occupied.contains(&(r, c)) {
                c += 1;
            }
            if c >= MAX_COLS {
                break;
            }
            let cs = span_attr(cid, "colspan").clamp(1, MAX_SPAN);
            // rowspan=0 means "span to the end"; a cell can never span more rows
            // than remain, so clamp both to the rows left (which also bounds the
            // occupancy fill and keeps spans within taffy's range).
            let rs_raw = span_attr(cid, "rowspan");
            let rs = if rs_raw == 0 { nrows - r } else { rs_raw }.clamp(1, nrows - r);
            for dr in 0..rs {
                for dc in 0..cs {
                    occupied.insert((r + dr, c + dc));
                }
            }
            placed.push((cid, r, c, rs, cs));
            c += cs;
            ncols = ncols.max(c).min(MAX_COLS);
        }
    }
    if placed.is_empty() || ncols == 0 {
        return None;
    }

    // Build each cell and pin it to its grid area.
    let mut children: Vec<taffy::NodeId> = Vec::new();
    for (cid, r, c, rs, cs) in &placed {
        let Some(cell_node) = build(tree, *cid, taffy_tree, id_map, words, engine, ifc_items, styles) else {
            continue;
        };
        if let Ok(cur) = taffy_tree.style(cell_node) {
            let mut cstyle = cur.clone();
            cstyle.grid_row = taffy::Line { start: line((*r as i16) + 1), end: span(*rs as u16) };
            cstyle.grid_column = taffy::Line { start: line((*c as i16) + 1), end: span(*cs as u16) };
            // Grid does the sizing; a leftover flex_grow from the flex-table
            // heuristic would be ignored anyway, but clear it to be explicit.
            cstyle.flex_grow = 0.0;
            // A cell's specified width sizes its COLUMN (fed into the track
            // by the pre-pass above); the cell box itself always fills its
            // grid area. Left in place, a `width:50%` cell would shrink to
            // half of its own already-halved track.
            cstyle.size.width = Dimension::auto();
            let _ = taffy_tree.set_style(cell_node, cstyle);
        }
        children.push(cell_node);
    }
    if children.is_empty() {
        return None;
    }

    // Column sizing pre-pass: specified widths on `<col>` elements and on
    // colspan-1 cells feed the tracks, so author column sizing actually
    // applies (a `td{width:200px}` must size the COLUMN, across every row).
    // A percent width becomes a percent track (resolving against the table),
    // a px width caps the track at that length (min-content still protects
    // the content), and unspecified columns keep content sizing with an
    // `auto` max so they stretch to fill a definite table width instead of
    // leaving a dead strip of bare table background.
    let mut col_px: Vec<Option<f32>> = vec![None; ncols];
    let mut col_pct: Vec<Option<f32>> = vec![None; ncols];
    let attr_width = |cid: NodeId| -> (Option<f32>, Option<f32>) {
        let Some(v) = tree.get_node(cid).and_then(|n| n.get_attribute("width").map(|s| s.trim().to_string())) else {
            return (None, None);
        };
        if let Some(p) = v.strip_suffix('%').and_then(|s| s.trim().parse::<f32>().ok()) {
            (None, Some(p / 100.0))
        } else {
            (v.trim_end_matches("px").trim().parse::<f32>().ok(), None)
        }
    };
    let style_width = |cid: NodeId| -> (Option<f32>, Option<f32>) {
        match styles.get(&cid).map(|s| s.width) {
            Some(crate::Dimension::Px(w)) if w > 0.0 => (Some(w), None),
            Some(crate::Dimension::Percent(p)) if p > 0.0 => (None, Some(p)),
            _ => attr_width(cid),
        }
    };
    // <col> elements (direct or under <colgroup>), each spanning `span` columns.
    let mut next_col = 0usize;
    let mut col_elems: Vec<NodeId> = Vec::new();
    for cid in tree.children(id) {
        match tree.get_node(cid).and_then(|n| n.as_element().map(|e| e.local.to_string())).as_deref() {
            Some("col") => col_elems.push(cid),
            Some("colgroup") => {
                for gc in tree.children(cid) {
                    if tree.get_node(gc).and_then(|n| n.as_element().map(|e| e.local.as_ref() == "col")).unwrap_or(false) {
                        col_elems.push(gc);
                    }
                }
            }
            _ => {}
        }
    }
    for col_el in col_elems {
        let span = tree
            .get_node(col_el)
            .and_then(|n| n.get_attribute("span").and_then(|v| v.trim().parse::<usize>().ok()))
            .unwrap_or(1)
            .clamp(1, MAX_SPAN);
        let (px, pct) = style_width(col_el);
        for _ in 0..span {
            if next_col >= ncols {
                break;
            }
            col_px[next_col] = px;
            col_pct[next_col] = pct;
            next_col += 1;
        }
    }
    // colspan-1 cells override <col> (they are closer to the content).
    for (cid, _r, c, _rs, cs) in &placed {
        if *cs != 1 || *c >= ncols {
            continue;
        }
        let (mut px, pct) = style_width(*cid);
        // A fixed width declared on a cell describes its content box unless
        // the author opted into border-box. Grid tracks describe the cell's
        // outer border box, so carry padding and border into the pinned track.
        // `<col>` widths above already describe the column track and must not
        // receive a particular cell's box edges.
        if let (Some(w), Some(s)) = (px, styles.get(cid)) {
            if s.box_sizing == crate::BoxSizing::ContentBox {
                px = Some(
                    w + s.padding.left
                        + s.padding.right
                        + s.border.left
                        + s.border.right,
                );
            }
        }
        if let Some(w) = px {
            col_px[*c] = Some(col_px[*c].map_or(w, |cur| cur.max(w)));
        }
        if let Some(p) = pct {
            col_pct[*c] = Some(col_pct[*c].map_or(p, |cur| cur.max(p)));
        }
    }

    // Row sizing: a `height` on the row or a rowspan-1 cell is a MINIMUM
    // (content can always grow a row), matching how tables treat heights.
    let mut row_min: Vec<Option<f32>> = vec![None; nrows];
    for (r, &tr) in rows.iter().enumerate() {
        if let Some(crate::Dimension::Px(h)) = styles.get(&tr).map(|s| s.height) {
            if h > 0.0 {
                row_min[r] = Some(h);
            }
        }
    }
    for (cid, r, _c, rs, _cs) in &placed {
        if *rs != 1 {
            continue;
        }
        if let Some(crate::Dimension::Px(h)) = styles.get(cid).map(|s| s.height) {
            if h > 0.0 {
                row_min[*r] = Some(row_min[*r].map_or(h, |cur| cur.max(h)));
            }
        }
    }

    let col = |i: usize| {
        let max = if let Some(p) = col_pct[i] {
            taffy::MaxTrackSizingFunction::percent(p)
        } else if let Some(px) = col_px[i] {
            taffy::MaxTrackSizingFunction::length(px)
        } else {
            taffy::MaxTrackSizingFunction::auto()
        };
        taffy::GridTemplateComponent::Single(taffy::MinMax { min: taffy::MinTrackSizingFunction::min_content(), max })
    };
    let row_track = |r: usize| {
        let min = match row_min[r] {
            Some(h) => taffy::MinTrackSizingFunction::length(h),
            None => taffy::MinTrackSizingFunction::auto(),
        };
        taffy::GridTemplateComponent::Single(taffy::MinMax { min, max: taffy::MaxTrackSizingFunction::auto() })
    };
    // In the separate-border model, border-spacing also exists between the
    // table edge and the first/last row and column. Grid `gap` only covers
    // interior tracks, so model the two outer spacing bands as internal
    // layout padding while leaving the computed CSS padding unchanged.
    let (horizontal_spacing, vertical_spacing) = table_spacing(style);
    let mut grid_style = style.clone();
    grid_style.padding.left += horizontal_spacing;
    grid_style.padding.right += horizontal_spacing;
    grid_style.padding.top += vertical_spacing;
    grid_style.padding.bottom += vertical_spacing;
    let mut tstyle = to_taffy_style(&grid_style);
    tstyle.display = Display::Grid;
    // A percentage width resolves against the container, so keep it and let the
    // used-width pass leave it to taffy. Any other width (px or auto) is forced
    // to auto here so that pass can measure content before choosing the width.
    if !matches!(style.width, crate::Dimension::Percent(_)) {
        tstyle.size.width = Dimension::auto();
    }
    tstyle.grid_template_columns = (0..ncols).map(col).collect();
    tstyle.grid_template_rows = (0..nrows).map(row_track).collect();
    tstyle.gap = taffy::Size {
        width: length(horizontal_spacing),
        height: length(vertical_spacing),
    };
    let table_node = taffy_tree.new_with_children(tstyle, &children).ok()?;
    id_map.insert(table_node, id);
    ifc_items.table_rows.insert(table_node, row_min);
    if col_px.iter().any(Option::is_some) || col_pct.iter().any(Option::is_some) {
        ifc_items.table_cols.insert(table_node, (col_px, col_pct));
    }
    Some(table_node)
}

fn build(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut IfcRegistry,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Option<taffy::NodeId> {
    let node = tree.get_node(id)?;
    let _name = node.as_element()?;
    let style = styles.get(&id)?;
    if style.display == crate::Display::None {
        return None;
    }

    // Real table layout: a <table> becomes a CSS grid so columns negotiate
    // across rows (the flex approximation could not) and colspan/rowspan map to
    // grid spans. Falls through to the generic path only if the table has no
    // usable rows/cells.
    if _name.local.as_ref() == "table" {
        if let Some(node) = build_table(tree, id, taffy_tree, id_map, words, engine, ifc_items, styles) {
            return Some(node);
        }
    }

    let mut taffy_style = to_taffy_style(style);

    // A forced break that reaches the general box builder sits between block
    // or atomic siblings, so it cannot be folded into a shaped inline run.
    // Give that sentinel the inherited used line-height instead of the UA
    // style's zero placeholder. This is the block/flex analogue of BRFrame's
    // empty-line contribution; break tags inside text and mixed block runs
    // are handled by the inline collector below.
    if _name.local.as_ref() == "br" {
        let height = crate::inline::used_line_height(style).max(0.0);
        taffy_style.size.height = taffy::style::Dimension::length(height);
        let leaf = taffy_tree.new_leaf(taffy_style).ok()?;
        id_map.insert(leaf, id);
        return Some(leaf);
    }

    // CSS has separate outer and inner display types, while taffy exposes one
    // display value. An inline-block normally needs our wrapping inline
    // approximation so it remains atomic in an inline formatting context.
    // As a direct flex/grid item, however, its outer display is blockified and
    // its flow-root inner display must stack block children. Apply the block
    // inner mode only in that formatting context; doing it globally turns
    // inline-block lists into one full-width item per line.
    if style.is_inline_block {
        let mut parent = node.parent;
        let flex_or_grid_item = loop {
            let Some(parent_id) = parent else { break false };
            let Some(parent_style) = styles.get(&parent_id) else { break false };
            if parent_style.display_contents {
                parent = tree.get_node(parent_id).and_then(|node| node.parent);
                continue;
            }
            break matches!(
                parent_style.display,
                crate::Display::Flex | crate::Display::Grid
            ) && !parent_style.internal_flex_container;
        };
        if flex_or_grid_item {
            taffy_style.display = taffy::style::Display::Block;
            taffy_style.flex_wrap = taffy::FlexWrap::NoWrap;
        } else if style.width == crate::Dimension::Auto {
            // An auto-width inline-block shrink-fits to its max-content width
            // when that fits the available line. Taffy's wrapping flex
            // approximation otherwise chooses a min-content width first,
            // turning ordinary two-word buttons into two-line controls. Keep
            // wrapping for explicitly constrained inline-blocks and for ones
            // with real in-flow block children; those need their inner block
            // formatting rather than a single max-content line.
            let has_in_flow_block_child = tree.children(id).iter().any(|child| {
                styles.get(child).map_or(false, |child_style| {
                    child_style.display == crate::Display::Block
                        && !matches!(
                            child_style.position,
                            Some(taffy::Position::Absolute)
                        )
                })
            });
            if !has_in_flow_block_child {
                taffy_style.flex_wrap = taffy::FlexWrap::NoWrap;
            }
        }
    }

    // A replaced image is a measured leaf, even when CSS gives it a percentage
    // width. Its intrinsic dimensions participate in an auto-sized ancestor's
    // max-content measurement; once the percentage axis becomes definite, the
    // measure callback derives the other axis through the intrinsic ratio.
    if _name.local.as_ref() == "img" {
        if let Some((width, height)) = style.intrinsic_size {
            // When an auto axis has a definite min/max constraint, the
            // measured replaced leaf must own intrinsic-ratio transfer so it
            // can clamp the derived size. Leaving the same ratio on taffy's
            // style lets taffy synthesize that axis before measurement and
            // bypass the constraint (width:100%; height:auto;
            // max-height:200px became the uncapped intrinsic height).
            //
            // Keep taffy's ratio in the unconstrained case: percentage-sized
            // images in auto wrappers can be measured at intrinsic size before
            // their percentage width becomes definite, and taffy then needs
            // the ratio to transfer that final width to height.
            let measured_axis_constraint =
                (!matches!(style.height, crate::Dimension::Auto)
                    && matches!(style.width, crate::Dimension::Auto)
                    && matches!(
                        (style.min_width, style.max_width),
                        (crate::Dimension::Px(_), _) | (_, crate::Dimension::Px(_))
                    ))
                    || (!matches!(style.width, crate::Dimension::Auto)
                        && matches!(style.height, crate::Dimension::Auto)
                        && matches!(
                            (style.min_height, style.max_height),
                            (crate::Dimension::Px(_), _) | (_, crate::Dimension::Px(_))
                        ));
            if measured_axis_constraint {
                taffy_style.aspect_ratio = None;
            }
            let context = engine.register_replaced(width, height, style);
            let leaf = taffy_tree.new_leaf_with_context(taffy_style, context).ok()?;
            id_map.insert(leaf, id);
            return Some(leaf);
        }
    }

    // If this container is a pure-text inline formatting context, collapse its
    // whole subtree to one leaf shaped and line-broken by cosmic-text (real
    // text layout), sized on demand by the measure function. This is the fast,
    // correct path for paragraphs/headings/labels/cells of text; the flex-wrap
    // approximation below only handles the leftovers (mixed inline + atomic
    // boxes, and layout-only builds where `try_build` always declines).
    if let Some(item) = engine.try_build(tree, id, styles) {
        if style.display == crate::Display::Block && style.width == crate::Dimension::Auto {
            // A pure-text block is still a fill-available block. Its shaped
            // inline context performs text alignment internally; retaining
            // the flex alignment stand-in here shrink-wraps the leaf to its
            // text, leaving no free width in which center/end can move it.
            taffy_style.display = taffy::style::Display::Block;
        }
        let leaf = taffy_tree.new_leaf_with_context(taffy_style, item).ok()?;
        id_map.insert(leaf, id);
        ifc_items.whole.insert(id, item);
        return Some(leaf);
    }

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
    //
    // The same fix applies to `<td>`/`<th>` (and any other UA default that
    // stacks its children in a flex column rather than taffy's Block mode):
    // without it, a text node's per-word leaves (see `build_text_words`)
    // become direct children of a column-direction flex container and each
    // word lands on its own line, one word per row, instead of wrapping
    // several words per line the way inline text actually flows.
    let stacks_children_vertically = style.display == crate::Display::Block
        || (style.display == crate::Display::Flex && style.flex_direction == Some(taffy::FlexDirection::Column));
    let has_inline_ish_content =
        has_inline_content(tree, id, styles) || style.before_content.is_some() || style.after_content.is_some();

    let mut dom_children = tree.children(id);
    let internal_mixed_block_flow = style.internal_flex_container
        && dom_children.iter().any(|cid| {
            styles.get(cid).map_or(false, |child| {
                child.display == crate::Display::Block
                    && child.float.is_none()
                    && !matches!(
                        child.position,
                        Some(taffy::Position::Absolute)
                    )
            })
        });
    // In flex and grid formatting contexts, collapsible whitespace-only text
    // between items does not generate an anonymous item. Taffy places each
    // stray whitespace leaf in the item sequence, shifting every real child.
    // This is especially visible in pretty-printed HTML: indentation before a
    // flex child becomes several pixels of unexplained leading space.
    //
    // Flatten display:contents first because its children participate as direct
    // flex/grid items. Otherwise formatting whitespace inside the transparent
    // wrapper would survive this filter and become an item in `build_any`.
    if matches!(style.display, crate::Display::Flex | crate::Display::Grid)
        && !internal_mixed_block_flow
    {
        let mut flat = Vec::new();
        flatten_contents_children(tree, &dom_children, styles, &mut flat);
        dom_children = flat;
        dom_children.retain(|&cid| {
            tree.get_node(cid).map_or(false, |n| n.is_element()) || !tree.text_content(cid).trim().is_empty()
        });
        // Flex and grid placement consume the order-modified document order.
        // Rust's stable sort preserves source order for equal values, exactly
        // the CSS tie-break. Taffy has no CSS `order` style field, so feeding
        // it the correctly ordered item sequence is the missing translation.
        dom_children.sort_by_key(|cid| styles.get(cid).map(|style| style.order).unwrap_or(0));
    } else if style.display == crate::Display::Block {
        // Formatting whitespace between block lines does not generate an
        // inline line box. Keeping a zero-height taffy leaf here still breaks
        // sibling margin adjacency, turning `margin-bottom:30px` followed by
        // `margin-top:40px` into 70px instead of the collapsed 40px. Preserve
        // whitespace only when this parent actually has inline content, where
        // the inter-element space belongs to the inline formatting context.
        if !has_inline_ish_content {
            dom_children.retain(|&cid| {
                tree.get_node(cid).map_or(false, |node| node.is_element())
                    || !tree.text_content(cid).trim().is_empty()
            });
        }
        // A float nested directly in a transparent inline wrapper (the common
        // `<a><img style="float:left"></a>` logo pattern) belongs to the
        // ancestor block formatting context. Gecko reparents that out-of-flow
        // frame to the BFC and leaves only a placeholder in the inline. Expose
        // the same item to our float-band builder instead of charging the
        // wrapper a full normal-flow row.
        for child in &mut dom_children {
            if let Some(float) = inline_wrapper_float(tree, *child, styles) {
                *child = float;
            }
        }
    }
    // `float` has no effect on a flex or grid item. Legacy stylesheets often
    // leave floats on children after a newer rule turns their parent into a
    // flex container; routing those children through the block float-zone
    // approximation corrupts flex sizing and percentage-margin placement.
    let has_float_child = style.display == crate::Display::Block
        && dom_children.iter().any(|&cid| styles.get(&cid).map(|s| s.float.is_some()).unwrap_or(false));
    let has_in_flow_block_child = dom_children.iter().any(|cid| {
            styles.get(cid).map_or(false, |child| {
                child.display == crate::Display::Block
                    && child.float.is_none()
                    && !matches!(child.position, Some(taffy::Position::Absolute))
            })
        });
    let is_native_table_cell = node.as_element().map_or(false, |element| {
        matches!(element.local.as_ref(), "td" | "th")
            && style.internal_flex_container
    });

    // `text-align` affects inline content, never the used width or placement
    // of an in-flow block child. `to_taffy_style` promotes a centered/right
    // block to a flex column as an inline-alignment stand-in; retaining that
    // promotion when the container has real block children makes every auto
    // width child shrink to max-content. That turns full-width paragraphs,
    // responsive picture wrappers, and rows of inline-block buttons into
    // narrow columns that wrap even though their containing block has room.
    //
    // Keep the legacy `<center>` behavior: unlike CSS text-align, that element
    // historically centers block descendants as well.
    if style.display == crate::Display::Block
        && has_in_flow_block_child
        && !style.legacy_center
    {
        taffy_style.display = taffy::style::Display::Block;
    }

    // A block with mixed inline + block children keeps real block layout:
    // block-level children become direct block children (full available
    // width, exactly what CSS block layout gives them), while each maximal
    // run of consecutive inline-level siblings is wrapped in one anonymous
    // block-level box that carries the inline formatting context. This is
    // how real engines structure mixed content, and it replaces the old
    // whole-container flex-row-wrap promotion, which collapsed contentless
    // block children (e.g. a `position:relative` hero wrapper whose only
    // child is out-of-flow) to width 0 and let text and blocks share lines.
    // Floats still take the legacy zone path below.
    if (style.display == crate::Display::Block
        || (is_native_table_cell && has_in_flow_block_child))
        && has_inline_ish_content
        && !has_float_child
    {
        return build_mixed_block(tree, id, style, taffy_style, &dom_children, taffy_tree, id_map, words, engine, ifc_items, styles);
    }

    if stacks_children_vertically
        && has_inline_ish_content
        && !(has_float_child && has_in_flow_block_child)
    {
        taffy_style.display = taffy::style::Display::Flex;
        taffy_style.flex_direction = taffy::FlexDirection::Row;
        taffy_style.flex_wrap = taffy::FlexWrap::Wrap;

        // The promoted container's main axis is horizontal, so text alignment
        // belongs on `justify_content`. Real `justify-content` from actual CSS
        // wins if present.
        if style.justify_content.is_none() {
            taffy_style.justify_content = match style.text_align {
                Some(taffy::AlignItems::FLEX_END) => Some(taffy::JustifyContent::FLEX_END),
                Some(taffy::AlignItems::CENTER) => Some(taffy::JustifyContent::CENTER),
                _ => taffy_style.justify_content,
            };
        }
        taffy_style.align_items = Some(taffy::AlignItems::FLEX_START);
    }

    let mut child_ids: Vec<taffy::NodeId> = if has_float_child {
        build_children_with_float_zone(tree, id, &dom_children, taffy_tree, id_map, words, engine, ifc_items, styles)
    } else {
        dom_children.into_iter().flat_map(|cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)).collect()
    };
    if let Some(content) = &style.before_content {
        let mut leaves = build_pseudo_content(id, content, style, taffy_tree, words);
        leaves.append(&mut child_ids);
        child_ids = leaves;
    }
    if let Some(content) = &style.after_content {
        child_ids.append(&mut build_pseudo_content(id, content, style, taffy_tree, words));
    }

    if style.internal_flex_container
        && taffy_style.display == taffy::style::Display::Flex
        && taffy_style.flex_direction == taffy::FlexDirection::Column
    {
        // Table cells and the other native block-layout stand-ins are not
        // genuine CSS flex containers. Their block children neither flex-shrink
        // to an authored container height nor shrink-wrap along the inline
        // axis. Preserving the children's block sizes is particularly
        // important for table cells, where `height` is only a row minimum and
        // taller content must grow the row.
        for child in &child_ids {
            let fills_inline_axis = id_map
                .get(child)
                .and_then(|dom_id| styles.get(dom_id))
                .map_or(false, |child_style| {
                    child_style.display == crate::Display::Block
                        && child_style.width == crate::Dimension::Auto
                        && child_style.float.is_none()
                        && !matches!(
                            child_style.position,
                            Some(taffy::Position::Absolute)
                        )
                });
            if let Ok(current) = taffy_tree.style(*child) {
                let mut block_item = current.clone();
                block_item.flex_shrink = 0.0;
                if fills_inline_axis {
                    block_item.size.width = taffy::style::Dimension::percent(1.0);
                }
                let _ = taffy_tree.set_style(*child, block_item);
            }
        }
    }

    let taffy_id = if child_ids.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &child_ids).ok()?
    };
    id_map.insert(taffy_id, id);
    Some(taffy_id)
}

fn inline_wrapper_float(
    tree: &DomTree,
    wrapper: NodeId,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Option<NodeId> {
    let wrapper_style = styles.get(&wrapper)?;
    if wrapper_style.display != crate::Display::Inline
        || wrapper_style.margin != crate::Edges::default()
        || wrapper_style.padding != crate::Edges::default()
        || wrapper_style.border != crate::Edges::default()
        || wrapper_style.before_content.is_some()
        || wrapper_style.after_content.is_some()
    {
        return None;
    }
    let visible: Vec<NodeId> = tree
        .children(wrapper)
        .into_iter()
        .filter(|&child| {
            let Some(node) = tree.get_node(child) else { return false };
            if !node.is_element() {
                return !tree.text_content(child).trim().is_empty();
            }
            styles
                .get(&child)
                .map(|style| style.display != crate::Display::None)
                .unwrap_or(false)
        })
        .collect();
    let [child] = visible.as_slice() else { return None };
    styles.get(child).and_then(|style| style.float).map(|_| *child)
}

/// Build a block container whose children mix inline-level and block-level
/// content, preserving real block layout for the block children.
///
/// Structure produced (mirroring how CSS block containers actually work):
/// - block-level in-flow children stay direct children of the (taffy Block)
///   parent, so an auto width fills the containing block even when the child
///   has no in-flow content of its own;
/// - each maximal run of consecutive inline-level siblings becomes ONE
///   anonymous block-level box carrying the inline formatting context. A run
///   of pure text and foldable inline wrappers collapses to a single shaped
///   cosmic-text leaf (one taffy node for the whole run instead of one per
///   word: faster and lighter than the old whole-container promotion). Runs
///   holding atomic inline boxes (img, inline-block, ...) fall back to an
///   anonymous flex-wrap wrapper around the run's boxes;
/// - out-of-flow (absolutely positioned) children neither join nor break a
///   run; they are appended after the flow children so their containing
///   block is this parent, whose used width they resolve percentages against.
#[allow(clippy::too_many_arguments)]
fn build_mixed_block(
    tree: &DomTree,
    id: NodeId,
    style: &crate::LayoutStyle,
    mut taffy_style: taffy::Style,
    dom_children: &[NodeId],
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut IfcRegistry,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Option<taffy::NodeId> {
    // The parent is a genuine block container. `to_taffy_style` may have
    // promoted it to a flex column for `text-align` (taffy block layout has
    // no align-items); undo that: text-align moves line content, never
    // block children, so it is applied to the runs below instead.
    taffy_style.display = taffy::style::Display::Block;

    // Splice display:contents children and drop nothing else, so runs
    // partition over the child list exactly as CSS sees it.
    let mut flat: Vec<NodeId> = Vec::new();
    flatten_contents_children(tree, dom_children, styles, &mut flat);

    enum Seg {
        Run(Vec<NodeId>),
        Block(NodeId),
    }
    let mut segs: Vec<Seg> = Vec::new();
    let mut out_of_flow: Vec<NodeId> = Vec::new();
    for &cid in &flat {
        let Some(node) = tree.get_node(cid) else { continue };
        let is_text = matches!(node.data, obscura_dom::tree::NodeData::Text { .. });
        // Comments, doctypes, and other non-rendered DOM nodes generate no CSS
        // box and cannot interrupt an inline formatting context. Hydrating
        // frameworks place marker comments between adjacent inline controls;
        // treating each marker as a block segment split one button row into
        // multiple anonymous block wrappers.
        if !is_text && !node.is_element() {
            continue;
        }
        let inline_level = if is_text {
            true
        } else if let Some(s) = styles.get(&cid) {
            if s.display == crate::Display::None {
                continue;
            }
            if matches!(s.position, Some(taffy::Position::Absolute)) {
                out_of_flow.push(cid);
                continue;
            }
            let is_forced_break = node
                .as_element()
                .map_or(false, |element| element.local.as_ref() == "br");
            is_forced_break || s.display == crate::Display::Inline
        } else {
            false
        };
        if inline_level {
            match segs.last_mut() {
                Some(Seg::Run(run)) => run.push(cid),
                _ => segs.push(Seg::Run(vec![cid])),
            }
        } else {
            segs.push(Seg::Block(cid));
        }
    }

    // ::before joins the first inline run and ::after the last (a list
    // marker must share its item text's lines); when the adjacent segment
    // is a block, the pseudo content gets its own anonymous run instead.
    let before_leaves = style.before_content.as_ref().map(|c| build_pseudo_content(id, c, style, taffy_tree, words)).unwrap_or_default();
    let after_leaves = style.after_content.as_ref().map(|c| build_pseudo_content(id, c, style, taffy_tree, words)).unwrap_or_default();
    let mut before_pending = !before_leaves.is_empty();
    let mut after_pending = !after_leaves.is_empty();

    let n_segs = segs.len();
    let mut child_ids: Vec<taffy::NodeId> = Vec::new();
    for (i, seg) in segs.into_iter().enumerate() {
        match seg {
            Seg::Block(cid) => {
                let built =
                    build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles);
                if style.legacy_center {
                    let has_default_horizontal_margins = styles.get(&cid).map_or(false, |child| {
                        child.margin.left == 0.0
                            && child.margin.right == 0.0
                            && !child.margin_auto[1]
                            && !child.margin_auto[3]
                    });
                    if has_default_horizontal_margins {
                        for child in &built {
                            if let Ok(current) = taffy_tree.style(*child) {
                                let mut centered = current.clone();
                                centered.margin.left =
                                    taffy::style::LengthPercentageAuto::auto();
                                centered.margin.right =
                                    taffy::style::LengthPercentageAuto::auto();
                                let _ = taffy_tree.set_style(*child, centered);
                            }
                        }
                    }
                }
                child_ids.extend(built);
            }
            Seg::Run(run) => {
                let has_text_strut = run.iter().any(|&cid| {
                    tree.get_node(cid).map_or(false, |node| {
                        matches!(
                            node.data,
                            obscura_dom::tree::NodeData::Text { .. }
                        )
                    })
                });
                // Collapsible source formatting at the start/end of an inline
                // run does not create line width. Preserve whitespace between
                // inline siblings, but trim indentation adjacent to block
                // boundaries so pretty-printed markup starts at the line edge.
                let is_whitespace_text = |cid: NodeId| {
                    tree.get_node(cid).map_or(false, |node| {
                        matches!(node.data, obscura_dom::tree::NodeData::Text { .. })
                            && tree.text_content(cid).trim().is_empty()
                    })
                };
                let start = run.iter().position(|&cid| !is_whitespace_text(cid)).unwrap_or(run.len());
                let end = run
                    .iter()
                    .rposition(|&cid| !is_whitespace_text(cid))
                    .map(|index| index + 1)
                    .unwrap_or(start);
                let run = &run[start..end];
                let join_before = before_pending && i == 0;
                let join_after = after_pending && i + 1 == n_segs;
                // Fast path: the whole run folds to one shaped leaf, unless
                // pseudo-content word leaves must share its lines.
                if !join_before && !join_after {
                    if let Some(item) = engine.try_build_run(tree, id, run, styles) {
                        let leaf = taffy_tree.new_leaf_with_context(run_leaf_style(), item).ok()?;
                        ifc_items.runs.entry(id).or_default().push(item);
                        child_ids.push(leaf);
                        continue;
                    }
                }
                let mut atoms: Vec<taffy::NodeId> = Vec::new();
                if join_before {
                    atoms.extend(before_leaves.iter().copied());
                    before_pending = false;
                }
                for &rc in run {
                    atoms.extend(build_any(tree, rc, taffy_tree, id_map, words, engine, ifc_items, styles));
                }
                if join_after {
                    atoms.extend(after_leaves.iter().copied());
                    after_pending = false;
                }
                if atoms.is_empty() {
                    // Whitespace-only run between blocks: no anonymous box.
                    continue;
                }
                let wrapper = taffy_tree
                    .new_with_children(
                        run_wrapper_style(style, has_text_strut),
                        &atoms,
                    )
                    .ok()?;
                child_ids.push(wrapper);
            }
        }
    }
    // Pseudo content that found no adjacent run to join.
    if before_pending {
        let wrapper = taffy_tree
            .new_with_children(run_wrapper_style(style, true), &before_leaves)
            .ok()?;
        child_ids.insert(0, wrapper);
    }
    if after_pending {
        let wrapper = taffy_tree
            .new_with_children(run_wrapper_style(style, true), &after_leaves)
            .ok()?;
        child_ids.push(wrapper);
    }
    for cid in out_of_flow {
        child_ids.extend(build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles));
    }

    let taffy_id = if child_ids.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &child_ids).ok()?
    };
    id_map.insert(taffy_id, id);
    Some(taffy_id)
}

/// Expand `display: contents` wrappers so their children partition into the
/// caller's segment list as if they were direct children (CSS Display 3).
fn flatten_contents_children(
    tree: &DomTree,
    children: &[NodeId],
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    out: &mut Vec<NodeId>,
) {
    for &cid in children {
        let splices = styles.get(&cid).map(|s| s.display_contents && s.display != crate::Display::None).unwrap_or(false);
        if splices {
            let kids = tree.children(cid);
            flatten_contents_children(tree, &kids, styles, out);
        } else {
            out.push(cid);
        }
    }
}

/// Style for a single-leaf inline run: a block-level box that fills the
/// containing block's width, with height from the shaped text (measure fn).
fn run_leaf_style() -> taffy::Style {
    taffy::Style {
        size: taffy::Size { width: taffy::style::Dimension::percent(1.0), height: taffy::style::Dimension::auto() },
        ..Default::default()
    }
}

/// Style for an anonymous inline-run wrapper: a full-width block-level box
/// whose interior is the flex-wrap approximation of an inline formatting
/// context. The parent's `text-align` moves the run's line content via
/// justify-content, exactly as the old whole-container
/// promotion did, but scoped to the run so sibling blocks stay full width.
fn run_wrapper_style(
    parent: &crate::LayoutStyle,
    has_text_strut: bool,
) -> taffy::Style {
    let justify = match parent.text_align {
        Some(taffy::AlignItems::FLEX_END) => Some(taffy::JustifyContent::FLEX_END),
        Some(taffy::AlignItems::CENTER) => Some(taffy::JustifyContent::CENTER),
        _ => None,
    };
    let line_height = if has_text_strut {
        crate::inline::used_line_height(parent).max(0.0)
    } else {
        0.0
    };
    taffy::Style {
        display: taffy::style::Display::Flex,
        flex_direction: taffy::FlexDirection::Row,
        flex_wrap: taffy::FlexWrap::Wrap,
        align_items: Some(taffy::AlignItems::FLEX_START),
        justify_content: justify,
        size: taffy::Size { width: taffy::style::Dimension::percent(1.0), height: taffy::style::Dimension::auto() },
        // Every CSS line box starts with the parent's font/line-height strut,
        // even when its only atomic inline is shorter (or zero-sized).
        min_size: taffy::Size {
            width: taffy::style::Dimension::auto(),
            height: taffy::style::Dimension::length(line_height),
        },
        ..Default::default()
    }
}

/// Approximate `float: left|right` without real per-line reflow (which
/// taffy's block/flex/grid modes do not provide): place the float alongside
/// the flow siblings that follow it until their estimated height reaches the
/// float's estimated bottom, a matching `clear` is encountered, or another
/// float begins, then let everything from there on revert to normal full-width
/// flow.
///
/// This is not a general CSS float implementation (a float taller than its
/// estimated flow zone won't reflow correctly), but it directly targets the overwhelmingly
/// common real-world shape: a floated image or infobox near the top of an
/// article, sitting beside the intro text, with the rest of the content
/// running full width once normal flow passes the float.
/// Rough height budget for a float with no explicit size and no images
/// (an icon-only or empty float, rare in practice): enough for a couple of
/// lines of caption-sized text without being so generous it drags in a
/// whole section the way an unbounded zone did.
const DEFAULT_FLOAT_HEIGHT_ESTIMATE: f32 = 200.0;

/// Estimate a float's rendered height in CSS px, for bounding how many flow
/// siblings should wrap alongside it (see `build_children_with_float_zone`).
/// Real layout hasn't run yet at this point (we're still building the taffy
/// tree), so this can only approximate: prefer an explicit height on the
/// float itself, else sum the explicit heights of descendant `<img>`s (the
/// common `<figure><img height=".."><figcaption>` thumbnail shape) plus a
/// text-based estimate of the float's own content (the common tall-infobox
/// shape, where the height comes from many rows of text rather than a
/// single image).
fn estimate_float_height(tree: &DomTree, float_id: NodeId, styles: &HashMap<NodeId, crate::LayoutStyle>) -> f32 {
    if let Some(crate::Dimension::Px(h)) = styles.get(&float_id).map(|s| s.height) {
        return h;
    }
    let image_height: f32 = tree
        .descendants(float_id)
        .into_iter()
        .filter(|&id| {
            tree.get_node(id).and_then(|n| n.as_element().map(|e| e.local.to_string())).as_deref() == Some("img")
        })
        .filter_map(|id| match styles.get(&id).map(|s| s.height) {
            Some(crate::Dimension::Px(h)) => Some(h),
            _ => None,
        })
        .sum();
    const ASSUMED_FLOAT_WIDTH: f32 = 280.0;
    let text_height = estimate_text_height(tree, float_id, styles, ASSUMED_FLOAT_WIDTH);
    // Flattened character count misses forced rows. A sidebar list with twenty
    // short `<li>`s or an infobox table with many `<tr>`s is much taller than
    // the same text treated as one wrapping paragraph. Add one line for each
    // structural row; the continuous-text estimate still accounts for extra
    // wrapping within those rows.
    let structural_height: f32 = tree
        .descendants(float_id)
        .into_iter()
        .filter(|&id| {
            tree.get_node(id)
                .and_then(|node| node.as_element().map(|element| element.local.to_string()))
                .map(|local| {
                    matches!(
                        local.as_str(),
                        "li" | "tr" | "dt" | "dd" | "p" | "figcaption" | "h1" | "h2" | "h3" | "h4"
                            | "h5" | "h6"
                    )
                })
                .unwrap_or(false)
        })
        .map(|id| styles.get(&id).and_then(|style| style.font_size).unwrap_or(16.0) * 1.2)
        .sum();
    (image_height + text_height + structural_height).max(DEFAULT_FLOAT_HEIGHT_ESTIMATE)
}

/// Estimate how tall `id`'s text content would render at `assumed_width`,
/// using the same average-character-width heuristic as the layout-only
/// (non-`paint`) text sizing fallback. Used only to bound the float-wrapping
/// zone (see `build_children_with_float_zone`), where the real available
/// width is not yet known, so this is deliberately approximate.
fn estimate_text_height(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, crate::LayoutStyle>, assumed_width: f32) -> f32 {
    let char_count = tree.text_content(id).chars().filter(|c| !c.is_whitespace()).count() as f32;
    if char_count == 0.0 {
        return 0.0;
    }
    let fsize = styles.get(&id).and_then(|s| s.font_size).unwrap_or(16.0);
    const AVG_CHAR_WIDTH_EM: f32 = 0.55;
    let chars_per_line = (assumed_width / (fsize * AVG_CHAR_WIDTH_EM)).max(1.0);
    let lines = (char_count / chars_per_line).ceil().max(1.0);
    lines * fsize * 1.2 + 16.0
}

/// Estimate the normal-flow height consumed by one sibling alongside a float.
/// Text alone is insufficient for image grids and fixed-height boxes, which
/// otherwise cost zero budget and remain squeezed beside a float long after
/// they should have passed its bottom.
fn estimate_flow_sibling_height(
    tree: &DomTree,
    id: NodeId,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
    assumed_width: f32,
) -> f32 {
    let style = styles.get(&id);
    if style.map(|style| style.display == crate::Display::None).unwrap_or(false)
        || style
            .and_then(|style| style.position)
            .map(|position| position == taffy::Position::Absolute)
            .unwrap_or(false)
    {
        return 0.0;
    }
    let explicit_height = match style.map(|style| style.height) {
        Some(crate::Dimension::Px(height)) => height.max(0.0),
        _ => 0.0,
    };
    let descendant_image_height: f32 = tree
        .descendants(id)
        .into_iter()
        .filter(|&descendant| {
            tree.get_node(descendant)
                .and_then(|node| node.as_element().map(|element| element.local.as_ref() == "img"))
                .unwrap_or(false)
        })
        .filter_map(|descendant| match styles.get(&descendant).map(|style| style.height) {
            Some(crate::Dimension::Px(height)) => Some(height.max(0.0)),
            _ => None,
        })
        .sum();
    let own_image_height = if tree
        .get_node(id)
        .and_then(|node| node.as_element().map(|element| element.local.as_ref() == "img"))
        .unwrap_or(false)
    {
        explicit_height
    } else {
        0.0
    };
    let content_height = estimate_text_height(tree, id, styles, assumed_width)
        .max(explicit_height)
        .max(descendant_image_height + own_image_height);
    let margins = style.map(|style| (style.margin.top + style.margin.bottom).max(0.0)).unwrap_or(0.0);
    content_height + margins
}

/// Largest definite (px) width among `id` and its descendants. Used to cap an
/// auto-width floated figure: a Wikipedia thumbnail is `<figure ...><img
/// width=250><figcaption>long text</figcaption></figure>`, and without a cap
/// the caption's unwrapped one-line max-content width (~700px) sizes the float
/// and starves the adjacent flow column to nothing. Real browsers size the
/// figure to the image (display:table) and wrap the caption; the image's
/// definite width is the bound that reproduces that.
fn max_definite_descendant_width(tree: &DomTree, id: NodeId, styles: &HashMap<NodeId, crate::LayoutStyle>) -> Option<f32> {
    let mut best: Option<f32> = None;
    for d in tree.descendants(id) {
        if let Some(crate::Dimension::Px(w)) = styles.get(&d).map(|s| s.width.clone()) {
            if w > 0.0 {
                best = Some(best.map_or(w, |b: f32| b.max(w)));
            }
        }
    }
    best
}

fn establishes_block_formatting_context(style: &crate::LayoutStyle) -> bool {
    matches!(style.display, crate::Display::Flex | crate::Display::Grid)
        || style.flow_root
        || style.overflow_hidden
        || style.is_inline_block
        || style.float.is_some()
        || matches!(style.position, Some(taffy::Position::Absolute))
}

fn build_children_with_float_zone(
    tree: &DomTree,
    parent_id: NodeId,
    dom_children: &[NodeId],
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut IfcRegistry,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Vec<taffy::NodeId> {
    let is_float = |cid: NodeId| styles.get(&cid).map(|s| s.float.is_some()).unwrap_or(false);

    // A block made entirely from inline-ish flow content and two or more
    // right floats is the classic utility/navigation bar. Right floats are
    // placed from the inline end inward, so their visual order is the reverse
    // of source order, while ordinary inline content keeps filling from the
    // start of the same band. Serializing each encountered float into its own
    // synthetic row reverses those two groups and can leave the entire bar
    // shrink-wrapped at the start.
    //
    // Model this bounded one-band case as [flow | reversed right-float group].
    // A nested group preserves every float's authored margins; only the
    // anonymous group receives the auto margin used to represent the free
    // space between the two sides.
    let right_floats: Vec<NodeId> = dom_children
        .iter()
        .copied()
        .filter(|cid| {
            styles.get(cid).map_or(false, |style| {
                style.float == Some(crate::Float::Right)
                    && style.display != crate::Display::None
            })
        })
        .collect();
    let has_left_float = dom_children.iter().any(|cid| {
        styles.get(cid).map_or(false, |style| {
            style.float == Some(crate::Float::Left)
                && style.display != crate::Display::None
        })
    });
    let flow_is_inline = dom_children.iter().all(|cid| {
        let Some(node) = tree.get_node(*cid) else {
            return true;
        };
        if !node.is_element() || is_float(*cid) {
            return true;
        }
        styles
            .get(cid)
            .map_or(true, |style| style.display != crate::Display::Block)
    });
    if right_floats.len() >= 2 && !has_left_float && flow_is_inline {
        // Removing out-of-flow items must not remove the one collapsible
        // space between the inline items on either side of them. Collapse
        // any run of formatting whitespace to one representative node, while
        // dropping leading/trailing whitespace at the band edges.
        let mut flow_dom = Vec::new();
        let mut pending_whitespace = None;
        let mut has_flow_content = false;
        for &cid in dom_children {
            if is_float(cid) {
                continue;
            }
            let is_whitespace = tree.get_node(cid).map_or(false, |node| {
                !node.is_element() && tree.text_content(cid).trim().is_empty()
            });
            if is_whitespace {
                if has_flow_content && pending_whitespace.is_none() {
                    pending_whitespace = Some(cid);
                }
                continue;
            }
            if has_flow_content {
                if let Some(whitespace) = pending_whitespace.take() {
                    flow_dom.push(whitespace);
                }
            }
            flow_dom.push(cid);
            has_flow_content = true;
        }
        let mut row_children: Vec<taffy::NodeId> = flow_dom
            .into_iter()
            .flat_map(|cid| {
                build_any(
                    tree,
                    cid,
                    taffy_tree,
                    id_map,
                    words,
                    engine,
                    ifc_items,
                    styles,
                )
            })
            .collect();
        let right_children: Vec<taffy::NodeId> = right_floats
            .iter()
            .rev()
            .filter_map(|cid| {
                build(
                    tree,
                    *cid,
                    taffy_tree,
                    id_map,
                    words,
                    engine,
                    ifc_items,
                    styles,
                )
            })
            .collect();
        if !row_children.is_empty() && !right_children.is_empty() {
            let right_group_style = taffy::Style {
                display: taffy::style::Display::Flex,
                flex_direction: taffy::FlexDirection::Row,
                flex_wrap: taffy::FlexWrap::Wrap,
                margin: taffy::Rect {
                    top: taffy::style::LengthPercentageAuto::length(0.0),
                    right: taffy::style::LengthPercentageAuto::length(0.0),
                    bottom: taffy::style::LengthPercentageAuto::length(0.0),
                    left: taffy::style::LengthPercentageAuto::auto(),
                },
                ..Default::default()
            };
            if let Ok(right_group) =
                taffy_tree.new_with_children(right_group_style, &right_children)
            {
                row_children.push(right_group);
                let row_style = taffy::Style {
                    display: taffy::style::Display::Flex,
                    flex_direction: taffy::FlexDirection::Row,
                    flex_wrap: taffy::FlexWrap::Wrap,
                    align_items: Some(taffy::AlignItems::FLEX_START),
                    size: taffy::Size {
                        width: taffy::Dimension::percent(1.0),
                        height: taffy::Dimension::auto(),
                    },
                    ..Default::default()
                };
                if let Ok(row) =
                    taffy_tree.new_with_children(row_style, &row_children)
                {
                    return vec![row];
                }
            }
        }
    }

    let Some(float_idx) = dom_children.iter().position(|&cid| is_float(cid)) else {
        return dom_children.iter().flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)).collect();
    };

    let mut result: Vec<taffy::NodeId> = dom_children[..float_idx]
        .iter()
        .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
        .collect();

    let float_side = styles.get(&dom_children[float_idx]).and_then(|s| s.float);

    // Opposing header floats share one float band even when an empty legacy
    // compatibility box sits between them. This is the classic left-logo /
    // right-tagline header: serializing the two synthetic rows doubles the
    // header height and pushes every later box down. Real float placement
    // scans the same BFC band and puts the second float against the opposite
    // edge when both margin boxes fit.
    let is_empty_bridge = |cid: NodeId| {
        let Some(node) = tree.get_node(cid) else { return true };
        if !node.is_element() {
            return tree.text_content(cid).trim().is_empty();
        }
        let style = styles.get(&cid);
        let no_size = style
            .map(|style| {
                matches!(style.width, crate::Dimension::Auto)
                    && matches!(style.height, crate::Dimension::Auto)
                    && matches!(style.min_width, crate::Dimension::Auto)
                    && matches!(style.min_height, crate::Dimension::Auto)
                    && matches!(style.max_width, crate::Dimension::Auto)
                    && matches!(style.max_height, crate::Dimension::Auto)
                    && style.margin == crate::Edges::default()
                    && style.padding == crate::Edges::default()
                    && style.border == crate::Edges::default()
                    && style.before_content.is_none()
                    && style.after_content.is_none()
            })
            .unwrap_or(true);
        no_size && tree.text_content(cid).trim().is_empty()
    };
    let mut opposite_idx = float_idx + 1;
    while opposite_idx < dom_children.len() && is_empty_bridge(dom_children[opposite_idx]) {
        opposite_idx += 1;
    }
    let opposite_side = dom_children
        .get(opposite_idx)
        .and_then(|cid| styles.get(cid))
        .and_then(|style| style.float);
    if opposite_side.is_some() && opposite_side != float_side {
        let first = build(
            tree,
            dom_children[float_idx],
            taffy_tree,
            id_map,
            words,
            engine,
            ifc_items,
            styles,
        );
        let second = build(
            tree,
            dom_children[opposite_idx],
            taffy_tree,
            id_map,
            words,
            engine,
            ifc_items,
            styles,
        );
        let row_children: Vec<taffy::NodeId> = match float_side {
            Some(crate::Float::Left) => [first, second].into_iter().flatten().collect(),
            _ => [second, first].into_iter().flatten().collect(),
        };
        let row_style = taffy::Style {
            display: taffy::style::Display::Flex,
            flex_direction: taffy::FlexDirection::Row,
            justify_content: Some(taffy::JustifyContent::SPACE_BETWEEN),
            align_items: Some(taffy::AlignItems::FLEX_START),
            size: taffy::Size {
                width: taffy::Dimension::percent(1.0),
                height: taffy::Dimension::auto(),
            },
            ..Default::default()
        };
        if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
            result.push(row);
        }
        result.extend(
            dom_children[opposite_idx + 1..]
                .iter()
                .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)),
        );
        return result;
    }

    // A run of two or more consecutively floated siblings (the classic
    // float-grid idiom: several `float:left; width:N%` boxes forming columns,
    // e.g. craigslist's `.sites .box{float:left;width:23%}` site directory) is
    // not the single-float-beside-flow shape handled below: real float layout
    // places the run side by side, wrapping to a new line when the row fills.
    // Model the run as a wrapping flex row. Whitespace-only text between the
    // floats does not break the run.
    let mut run_end = float_idx + 1;
    let mut float_count = 1usize;
    while run_end < dom_children.len() {
        let cid = dom_children[run_end];
        if styles.get(&cid).and_then(|s| s.float) == float_side {
            float_count += 1;
            run_end += 1;
        } else if tree.get_node(cid).map_or(false, |n| !n.is_element())
            && tree.text_content(cid).trim().is_empty()
        {
            run_end += 1;
        } else {
            break;
        }
    }
    if float_count >= 2 {
        let mut run_children: Vec<taffy::NodeId> = dom_children[float_idx..run_end]
            .iter()
            // Formatting whitespace between floats does not generate an
            // in-flow flex item or consume horizontal space.
            .filter(|&&cid| styles.get(&cid).and_then(|s| s.float) == float_side)
            .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
            .collect();
        // A common navigation-bar shape is a run of left floats followed by
        // one right float. The right float still scans the current float band:
        // it does not start a new row merely because multiple left floats
        // precede it. Keep the run in one wrapping row and use an auto inline
        // margin to reserve all remaining space before the opposing float.
        //
        // This stays deliberately narrower than a general float manager. In
        // particular, multiple right floats have reverse source-order
        // placement semantics and need their own representation.
        let trailing_right = (float_side == Some(crate::Float::Left)
            && dom_children
                .get(run_end)
                .and_then(|cid| styles.get(cid))
                .and_then(|style| style.float)
                == Some(crate::Float::Right))
        .then(|| dom_children[run_end]);
        if let Some(right_dom) = trailing_right {
            let right = build(
                tree,
                right_dom,
                taffy_tree,
                id_map,
                words,
                engine,
                ifc_items,
                styles,
            );
            if let Some(right) = right {
                if let Ok(current) = taffy_tree.style(right) {
                    let mut pushed_right = current.clone();
                    pushed_right.margin.left =
                        taffy::style::LengthPercentageAuto::auto();
                    let _ = taffy_tree.set_style(right, pushed_right);
                }
                run_children.push(right);
                let row_style = taffy::Style {
                    display: taffy::style::Display::Flex,
                    flex_direction: taffy::FlexDirection::Row,
                    flex_wrap: taffy::FlexWrap::Wrap,
                    align_items: Some(taffy::AlignItems::FLEX_START),
                    size: taffy::Size {
                        width: taffy::Dimension::percent(1.0),
                        height: taffy::Dimension::auto(),
                    },
                    ..Default::default()
                };
                if let Ok(row) = taffy_tree.new_with_children(row_style, &run_children) {
                    result.push(row);
                }
                result.extend(build_children_with_float_zone(
                    tree,
                    parent_id,
                    &dom_children[run_end + 1..],
                    taffy_tree,
                    id_map,
                    words,
                    engine,
                    ifc_items,
                    styles,
                ));
                return result;
            }
        }
        let row_style = taffy::Style {
            display: taffy::style::Display::Flex,
            flex_direction: taffy::FlexDirection::Row,
            flex_wrap: taffy::FlexWrap::Wrap,
            align_items: Some(taffy::AlignItems::FLEX_START),
            ..Default::default()
        };
        if let Ok(row) = taffy_tree.new_with_children(row_style, &run_children) {
            result.push(row);
        }
        result.extend(build_children_with_float_zone(
            tree, parent_id, &dom_children[run_end..], taffy_tree, id_map, words, engine, ifc_items,
            styles,
        ));
        return result;
    }

    // Stop growing the zone once the flow siblings collected so far would
    // already fill (an estimate of) the float's own height: real float
    // reflow ends when normal-flow content passes the float's bottom edge,
    // Headings do not terminate a CSS float's influence by themselves.
    // Without this, a short floated thumbnail (a few hundred px) dragged an
    // entire multi-paragraph section into a narrow flow column alongside it
    // — visibly wrong wrapping plus a large empty gap once the (much
    // shorter) float ran out, both from treating "next heading" as the only
    // bound. The estimate is necessarily rough (actual available width is a
    // taffy layout result we don't have yet at tree-build time), but even an
    // approximate bound beats an unbounded one.
    let float_height_budget = estimate_float_height(tree, dom_children[float_idx], styles);
    const ASSUMED_FLOW_WIDTH: f32 = 500.0;
    // `clear` on a sibling ends the zone: the cleared element moves below the
    // float (the clearfix idiom), so it must not join the flow column beside it.
    let clears_this_float = |cid: NodeId| {
        let Some(c) = styles.get(&cid).and_then(|s| s.clear) else { return false };
        match (float_side, c) {
            (_, crate::Clear::Both) => true,
            (Some(crate::Float::Left), crate::Clear::Left) => true,
            (Some(crate::Float::Right), crate::Clear::Right) => true,
            _ => false,
        }
    };
    let mut zone_end = float_idx + 1;
    let mut flow_height_estimate = 0.0f32;
    while zone_end < dom_children.len()
        && !is_float(dom_children[zone_end])
        && !clears_this_float(dom_children[zone_end])
    {
        flow_height_estimate +=
            estimate_flow_sibling_height(tree, dom_children[zone_end], styles, ASSUMED_FLOW_WIDTH);
        zone_end += 1;
        if flow_height_estimate >= float_height_budget {
            break;
        }
    }
    // The float itself is always an element (only elements get style
    // entries, and `is_float` above required one), so a direct `build` call
    // is correct here; only its flow siblings need the word-splitting `build_any`.
    let float_taffy = build(tree, dom_children[float_idx], taffy_tree, id_map, words, engine, ifc_items, styles);
    // Cap an auto-width float at its widest definite-width descendant so a long
    // wrappable caption cannot inflate it to the caption's one-line max-content
    // width and starve the flow column beside it (the Wikipedia-thumbnail /
    // article-body-collapses-to-one-word-per-line bug). Only when the float is
    // itself auto-width and actually contains such a box; text-only floats
    // (pull quotes, sized infoboxes) are left to normal shrink-to-fit.
    if let Some(float_id) = float_taffy {
        let float_dom = dom_children[float_idx];
        let float_auto = styles.get(&float_dom).map(|s| matches!(s.width, crate::Dimension::Auto)).unwrap_or(true);
        if float_auto {
            if let Some(w) = max_definite_descendant_width(tree, float_dom, styles) {
                if let Ok(cur) = taffy_tree.style(float_id) {
                    let mut st = cur.clone();
                    // A little slack for the figure's own border/padding so the
                    // image is not clipped; the caption still wraps to ~image width.
                    st.max_size.width = taffy::Dimension::length(w + 12.0);
                    let _ = taffy_tree.set_style(float_id, st);
                }
            }
        }
    }
    match float_taffy {
        Some(float_id) => {
            let flow_column_style = taffy::Style {
                display: taffy::style::Display::Block,
                flex_grow: 1.0,
                flex_shrink: 1.0,
                flex_basis: taffy::Dimension::length(0.0),
                min_size: taffy::Size { width: taffy::Dimension::length(0.0), height: taffy::Dimension::auto() },
                ..Default::default()
            };
            let flow_dom = &dom_children[float_idx + 1..zone_end];
            let flow_column = if flow_dom.is_empty() {
                taffy_tree.new_leaf(flow_column_style).ok()
            } else {
                // The zone is still an ordinary block formatting context:
                // consecutive text/inline siblings must share inline runs,
                // while block siblings stack. Building each sibling directly
                // into a flex column makes every link a separate stretched
                // row. Reuse the mixed-block builder, but leave this anonymous
                // wrapper out of the DOM id map so it cannot overwrite the
                // real parent's rectangle.
                let mut flow_style = styles.get(&parent_id).cloned().unwrap_or_default();
                flow_style.before_content = None;
                flow_style.after_content = None;
                let column = build_mixed_block(
                    tree,
                    parent_id,
                    &flow_style,
                    flow_column_style,
                    flow_dom,
                    taffy_tree,
                    id_map,
                    words,
                    engine,
                    ifc_items,
                    styles,
                );
                if let Some(column_id) = column {
                    id_map.remove(&column_id);
                }
                column
            };

            let row_style = taffy::Style {
                display: taffy::style::Display::Flex,
                flex_direction: taffy::FlexDirection::Row,
                align_items: Some(taffy::AlignItems::FLEX_START),
                size: taffy::Size {
                    width: taffy::Dimension::percent(1.0),
                    height: taffy::Dimension::auto(),
                },
                ..Default::default()
            };
            // A float does not contribute to the height of a non-BFC block
            // that contains it. Put an escaping float inside a zero-height,
            // overflow-visible wrapper: its real width still reserves the
            // current band, but its height can protrude into later descendant
            // blocks of the ancestor BFC. A matching `clear` or any remaining
            // direct full-width sibling keeps the old containing row, since
            // that content explicitly terminates the local float zone.
            let can_escape = zone_end == dom_children.len()
                && styles
                    .get(&parent_id)
                    .map(|style| !establishes_block_formatting_context(style))
                    .unwrap_or(false);
            let row_float = if can_escape {
                let wrapper_style = taffy::Style {
                    display: taffy::style::Display::Block,
                    flex_grow: 0.0,
                    flex_shrink: 0.0,
                    size: taffy::Size {
                        width: taffy::Dimension::auto(),
                        height: taffy::Dimension::length(0.0),
                    },
                    ..Default::default()
                };
                taffy_tree
                    .new_with_children(wrapper_style, &[float_id])
                    .ok()
                    .unwrap_or(float_id)
            } else {
                float_id
            };
            let row_children: Vec<taffy::NodeId> = match float_side {
                Some(crate::Float::Left) => [Some(row_float), flow_column].into_iter().flatten().collect(),
                _ => [flow_column, Some(row_float)].into_iter().flatten().collect(),
            };
            if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
                result.push(row);
                if can_escape {
                    if let (Some(flow), Some(side)) = (flow_column, float_side) {
                        ifc_items.float_continuations.push(FloatContinuation {
                            owner: parent_id,
                            float: float_id,
                            flow,
                            side,
                        });
                    }
                }
            }
        }
        // The float itself failed to build (e.g. display:none resolved for
        // it specifically); still build its flow siblings so their content
        // is not silently lost.
        None => result.extend(
            dom_children[float_idx + 1..zone_end]
                .iter()
                .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)),
        ),
    }

    result.extend(
        dom_children[zone_end..]
            .iter()
            .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)),
    );
    result
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

    #[test]
    fn author_cascade_honors_important_and_presentational_hint_order() {
        let tree = parse_html(
            r#"<style>
                #sheet { width: 320px !important; background: green !important }
                .case#sheet { width: 80px; background: red }
                .inline-normal { width: 320px !important; background: green !important }
                #inline-important { width: 80px !important; background: red !important }
                .custom { --w: 320px !important; --w: 80px; width: var(--w) }
                .hint { background: green }
            </style>
            <div id="sheet" class="case"></div>
            <div id="inline-normal" class="inline-normal" style="width:80px;background:red"></div>
            <div id="inline-important" style="width:320px!important;background:green!important"></div>
            <div id="inline-order" style="width:320px!important;width:80px;background:green!important;background:red"></div>
            <div id="custom" class="custom"></div>
            <div id="hint" class="hint" bgcolor="red"></div>"#,
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        for id in ["sheet", "inline-normal", "inline-important", "inline-order", "custom"] {
            let nid = tree.query_selector(&format!("#{id}")).unwrap().unwrap();
            let style = laid.styles.get(&nid).unwrap();
            assert_eq!(style.width, crate::Dimension::Px(320.0), "wrong cascade width for {id}");
        }
        for id in ["sheet", "inline-normal", "inline-important", "inline-order", "hint"] {
            let nid = tree.query_selector(&format!("#{id}")).unwrap().unwrap();
            let style = laid.styles.get(&nid).unwrap();
            assert_eq!(style.background_color, Some([0, 128, 0, 255]), "wrong cascade color for {id}");
        }
    }

    #[test]
    fn box_sizing_controls_the_declared_size_edge() {
        let tree = parse_html(
            r#"<style>
                body { margin: 0 }
                .box { width:100px; height:50px; padding:10px; border:2px solid black }
                .border { box-sizing:border-box }
                .parent { width:400px }
                .half { width:50%; padding:10px; border:2px solid black }
                .limited { width:200px; max-width:100px; padding:10px; border:2px solid black }
                .inherit-parent { box-sizing:border-box }
                .inherit-child { box-sizing:inherit; width:100px; padding:10px; border:2px solid black }
            </style>
            <div id="content" class="box"></div>
            <div id="border" class="box border"></div>
            <div class="parent"><div id="half-content" class="half"></div><div id="half-border" class="half border"></div></div>
            <div id="max-content" class="limited"></div>
            <div id="max-border" class="limited border"></div>
            <div class="inherit-parent"><div id="inherited-border" class="inherit-child"></div></div>"#,
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        let size = |id: &str| {
            let nid = tree.query_selector(&format!("#{id}")).unwrap().unwrap();
            let rect = laid.rects.get(&nid).unwrap();
            (rect.width, rect.height)
        };
        assert_eq!(size("content"), (124.0, 74.0));
        assert_eq!(size("border"), (100.0, 50.0));
        assert_eq!(size("half-content").0, 224.0);
        assert_eq!(size("half-border").0, 200.0);
        assert_eq!(size("max-content").0, 124.0);
        assert_eq!(size("max-border").0, 100.0);
        assert_eq!(size("inherited-border").0, 100.0);
    }

    #[test]
    fn root_percentage_font_size_controls_rem_lengths() {
        let tree = parse_html(
            r#"<style>
                html { font-size: 62.5% }
                body { margin: 0 }
                #box {
                    display: grid;
                    width: 4rem;
                    margin-top: 3rem;
                    row-gap: 4rem;
                    column-gap: calc(1rem + 2vw);
                }
                #box > div { height: 2rem }
            </style>
            <div id="box"><div></div><div></div></div>"#,
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        let html = tree.query_selector("html").unwrap().unwrap();
        let box_id = tree.query_selector("#box").unwrap().unwrap();
        assert_eq!(laid.styles.get(&html).unwrap().font_size, Some(10.0));
        assert_eq!(laid.styles.get(&box_id).unwrap().row_gap, Some(40.0));
        assert_eq!(laid.styles.get(&box_id).unwrap().column_gap, Some(35.6));
        assert_eq!(laid.rects.get(&box_id).unwrap().width, 40.0);
        assert_eq!(laid.rects.get(&box_id).unwrap().height, 80.0);
        assert_eq!(laid.rects.get(&box_id).unwrap().y, 30.0);
    }

    #[test]
    fn block_and_grid_sibling_margins_collapse() {
        let tree = parse_html(
            r#"<style>
                body { margin: 0 }
                .a { position: relative; height: 10px; margin-bottom: 30px }
                .b { position: relative; display: grid; height: 10px; margin-top: 40px }
            </style>
            <div class="a"></div>
            <div id="b" class="b"></div>"#,
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        let b = tree.query_selector("#b").unwrap().unwrap();
        assert_eq!(laid.rects.get(&b).unwrap().y, 50.0);
    }

    #[test]
    fn table_cell_content_box_width_includes_padding_and_border_in_track() {
        let tree = parse_html(
            r#"<style>
                table { border-spacing:0 }
                td { width:100px; padding:10px; border:2px solid black }
                .border { box-sizing:border-box }
            </style>
            <table><tr><td id="content-cell">content</td></tr></table>
            <table><tr><td id="border-cell" class="border">border</td></tr></table>"#,
        );
        let laid = layout_dom(&tree, (1280.0, 720.0));
        let width = |id: &str| {
            let nid = tree.query_selector(&format!("#{id}")).unwrap().unwrap();
            laid.rects.get(&nid).unwrap().width
        };
        assert_eq!(width("content-cell"), 124.0);
        assert_eq!(width("border-cell"), 100.0);
    }

}
