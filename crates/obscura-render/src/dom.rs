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
) {
    let Some(node) = tree.get_node(id) else { return };
    let is_element = node.is_element();
    // The custom-property map in force for this node's subtree: the parent's,
    // unless this element declares its own `--x` (then a richer map).
    let mut this_props = parent_props.clone();
    if let Some(elem) = node.as_element() {
        let mut style = crate::style::ua_style(elem.local.as_ref());
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
        cascade_walk(tree, cid, sheet, matcher, styles, &this_props);
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
    let root_props = std::rc::Rc::new(HashMap::new());
    // A real preorder walk (not a flat descendants() scan) so the matcher's
    // ancestor bloom filter tracks the current path: push before recursing
    // into children, pop on the way back out. This is what lets descendant
    // combinators (".mw-body .firstHeading") fast-reject via the filter
    // instead of falling back to the always-true "can't reject" case.
    cascade_walk(tree, tree.document(), &sheet, &mut matcher, &mut styles, &root_props);
    if timing {
        let (r, i, c, l, u) = sheet.debug_stats();
        eprintln!("[timing] parse+index={:?} cascade={:?} rules={} id_keys={} class_keys={} local_keys={} universal={}", t_parse, t1.elapsed(), r, i, c, l, u);
    }
    grow_trailing_auto_cells(tree, &mut styles);
    propagate_border_spacing(tree, &mut styles);

    // The leaf context is the index of a cosmic-text inline formatting
    // context in `engine`; leaves without text carry no context.
    let mut taffy_tree: TaffyTree<usize> = TaffyTree::new();
    let mut id_map: HashMap<taffy::NodeId, NodeId> = HashMap::new();
    let mut words: HashMap<taffy::NodeId, (NodeId, String)> = HashMap::new();
    let mut engine = crate::inline::TextEngine::new();
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
            visibility_hidden: bool,
            opacity_product: f32,
            list_style: crate::ListStyle,
            line_height: crate::LineHeight,
            text_transform: crate::TextTransform,
            italic: bool,
            box_sizing: crate::BoxSizing,
            /// Containing-block width in px for the current element, carried
            /// down so percentage padding/margin (which resolve against the
            /// containing block WIDTH, all sides) can be turned into px before
            /// taffy layout. Not a CSS-inherited property; it is recomputed to
            /// the element's own content width for its children.
            cb_width: f32,
        }
        impl Default for Inherited {
            fn default() -> Self {
                Inherited {
                    color: None,
                    font_size: None,
                    font_weight: None,
                    font_family: None,
                    visibility_hidden: false,
                    opacity_product: 1.0,
                    // CSS initial value of list-style-type.
                    list_style: crate::ListStyle::Disc,
                    line_height: crate::LineHeight::Normal,
                    text_transform: crate::TextTransform::None,
                    italic: false,
                    box_sizing: crate::BoxSizing::ContentBox,
                    cb_width: 0.0,
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
            match (s.and_then(|s| s.font_size), s.and_then(|s| s.font_size_raw)) {
                (Some(px), _) => px,
                (None, Some(d)) => match d.resolve(16.0, 16.0, vw, vh) {
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
        let mut queue = vec![(root_id, root_inh)];
        while let Some((id, mut inh)) = queue.pop() {
            // Default the child containing-block width to this element's own
            // (updated to its content width inside the block below).
            let mut child_cb_width = inh.cb_width;
            if let Some(style) = styles.get_mut(&id) {
                match style.color { Some(c) => inh.color = Some(c), None => style.color = inh.color }
                // Resolve a relative font-size against the PARENT (em/%) or
                // ROOT (rem) font-size before inheriting it downward.
                let parent_fs = inh.font_size.unwrap_or(16.0);
                if let Some(raw) = style.font_size_raw {
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
                for i in style.inset.iter_mut() {
                    if let Some(d) = i {
                        *i = Some(d.resolve(em_px, root_fs, vw, vh));
                    }
                }
                match &style.font_weight { Some(w) => inh.font_weight = Some(w.clone()), None => style.font_weight = inh.font_weight.clone() }
                match &style.font_family { Some(f) => inh.font_family = Some(f.clone()), None => style.font_family = inh.font_family.clone() }
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
            for cid in tree.children(id).into_iter().rev() {
                queue.push((cid, inh.clone()));
            }
        }

        resolve_grid_areas(tree, root_id, &mut styles);

        // Blockify grid items (CSS Display 3): a direct child of a grid
        // container is blockified, so an inline `<a>`/`<time>`/`<span>` becomes
        // a block-level grid item and lays its text out as its own box instead
        // of being flattened into loose words in the container's cell (which
        // shattered grid lists like jvns.ca's `.article-list{display:grid}` of
        // alternating `<time>`/`<a>` into one word per cell). Only grid is done
        // here, not flex: obscura also uses flex-column as the UA stand-in for
        // block containers like `<td>`, whose inline text must stay inline.
        let grid_parents: Vec<NodeId> = styles
            .iter()
            .filter(|(_, s)| s.display == crate::Display::Grid)
            .map(|(&id, _)| id)
            .collect();
        for pid in grid_parents {
            for cid in tree.children(pid) {
                if let Some(cs) = styles.get_mut(&cid) {
                    if cs.display == crate::Display::Inline && !cs.is_inline_block {
                        cs.display = crate::Display::Block;
                    }
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
                    // A percentage-width table resolves against its container, so
                    // leave taffy's percentage handling in place.
                    let width_style = styles.get(&dom).map(|s| s.width.clone());
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
                    let preferred = match width_style {
                        Some(crate::Dimension::Px(w)) => w,
                        _ => max_c,
                    };
                    let used = preferred.max(min_c).min(viewport.0.max(min_c));
                    // Distribute `used` across the columns proportionally between
                    // each column's own min-content and max-content width, the way
                    // CSS tables do, instead of letting the grid hand every auto
                    // track an equal share of the surplus (which over-widens narrow
                    // label columns and starves wide prose columns). NetSurf
                    // layout.c layout_table: col.width = col.min + (col.max - col.min)
                    // * (used - min) / (max - min). The table's border, padding, and
                    // inter-column border-spacing are layout-invariant, so they are
                    // recovered once from the whole-table min-content width and kept
                    // out of the interpolation.
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
                        // overhead = table border + padding + inter-column spacing,
                        // recovered from the whole-table min-content width
                        // (from the content mins, before specified widths pin
                        // any column).
                        let sum_min_content: f32 = col_min.iter().sum();
                        let overhead = (min_c - sum_min_content).max(0.0);
                        let target = (used - overhead).max(0.0);
                        // Specified column widths (a `<col>` or colspan-1 cell
                        // width, recorded at build time) pin their columns:
                        // px directly, percent against the table's content
                        // width. Content min-content still floors them, so a
                        // too-narrow spec never crushes its content. The
                        // remaining space interpolates across the auto
                        // columns exactly as before.
                        if let Some((spec_px, spec_pct)) = ifc_items.table_cols.get(&tnode) {
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
                        let widths: Vec<f32> = if sum_max <= sum_min || target <= sum_min {
                            col_min.clone()
                        } else if target >= sum_max {
                            // Table wider than max-content (explicit/percentage-ish
                            // width): hand the extra out equally, as real tables do.
                            let extra = (target - sum_max) / ncols as f32;
                            col_max.iter().map(|m| m + extra).collect()
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
                            s.size.width = length(used);
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
                        s.size.width = length(used);
                        let _ = taffy_tree.set_style(tnode, s);
                    }
                }

                let _ = taffy_tree.compute_layout_with_measure(taffy_root, available, &mut measure);
            }
            #[cfg(not(feature = "paint"))]
            {
                let _ = taffy_tree.compute_layout(taffy_root, available);
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
                }
            }
            apply_to_rows(tree, cid, h, v, styles);
        }
    }

    for id in tree.descendants(tree.document()) {
        if local_name(tree, id).as_deref() != Some("table") {
            continue;
        }
        let Some((h, v)) = styles.get(&id).and_then(|s| s.border_spacing) else { continue };
        if let Some(s) = styles.get_mut(&id) {
            s.row_gap = Some(v);
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

/// Attach inset-positioned boxes to their CSS containing block rather than
/// their immediate DOM parent.
///
/// Taffy resolves an absolute child's insets against its direct layout-tree
/// parent. CSS instead uses the nearest positioned or transformed ancestor;
/// fixed boxes use the nearest transformed ancestor or the initial containing
/// block. Gecko represents that distinction by reparenting the positioned
/// frame and leaving a placeholder in normal flow. We can safely do the same
/// without a placeholder when each axis has at least one specified inset: no
/// static-position coordinate is needed. Boxes with an entirely auto axis
/// stay under their DOM parent until the placeholder path is implemented.
fn reparent_inset_positioned_nodes(
    tree: &DomTree,
    taffy_tree: &mut TaffyTree<usize>,
    taffy_root: taffy::NodeId,
    id_map: &HashMap<taffy::NodeId, NodeId>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) {
    let reverse: HashMap<NodeId, taffy::NodeId> =
        id_map.iter().map(|(&taffy_id, &dom_id)| (dom_id, taffy_id)).collect();
    let mut nearest_abs_cb_for_children: HashMap<NodeId, taffy::NodeId> = HashMap::new();
    let mut nearest_fixed_cb_for_children: HashMap<NodeId, taffy::NodeId> = HashMap::new();

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
        if !has_block_inset || !has_inline_inset {
            continue;
        }
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
        if taffy_tree.remove_child(current, child).is_ok() {
            let _ = taffy_tree.add_child(target, child);
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

/// Give each `<tr>`/`<tbody>`/`<thead>`/`<tfoot>` a rect equal to the union of
/// its descendant cell rects. In the grid table model these wrappers are not
/// taffy boxes, so without this their backgrounds and borders (zebra striping,
/// a `thead` background, a per-row border) would never paint. Skips a wrapper
/// that already has a rect (a table that fell back to the flex path keeps its
/// real box).
fn synthesize_row_rects(tree: &DomTree, rects: &mut HashMap<NodeId, Rect>) {
    for id in tree.descendants(tree.document()) {
        let local = match tree.get_node(id).and_then(|n| n.as_element().map(|e| e.local.to_string())) {
            Some(l) => l,
            None => continue,
        };
        if !matches!(local.as_str(), "tr" | "tbody" | "thead" | "tfoot") {
            continue;
        }
        if rects.contains_key(&id) {
            continue;
        }
        let mut acc: Option<Rect> = None;
        for d in tree.descendants(id) {
            let is_cell = tree
                .get_node(d)
                .and_then(|n| n.as_element().map(|e| matches!(e.local.as_ref(), "td" | "th")))
                .unwrap_or(false);
            if !is_cell {
                continue;
            }
            if let Some(r) = rects.get(&d) {
                acc = Some(match acc {
                    Some(a) => a.union(r),
                    None => *r,
                });
            }
        }
        if let Some(r) = acc {
            rects.insert(id, r);
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
    let mut tstyle = to_taffy_style(style);
    tstyle.display = Display::Grid;
    // A percentage width resolves against the container, so keep it and let the
    // used-width pass leave it to taffy. Any other width (px or auto) is forced
    // to auto here so that pass can measure content before choosing the width.
    if !matches!(style.width, crate::Dimension::Percent(_)) {
        tstyle.size.width = Dimension::auto();
    }
    tstyle.grid_template_columns = (0..ncols).map(col).collect();
    tstyle.grid_template_rows = (0..nrows).map(row_track).collect();
    if let Some((h, v)) = style.border_spacing {
        tstyle.gap = taffy::Size { width: length(h), height: length(v) };
    }
    let table_node = taffy_tree.new_with_children(tstyle, &children).ok()?;
    id_map.insert(table_node, id);
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

    // A replaced image is a measured leaf, even when CSS gives it a percentage
    // width. Its intrinsic dimensions participate in an auto-sized ancestor's
    // max-content measurement; once the percentage axis becomes definite, the
    // measure callback derives the other axis through the intrinsic ratio.
    if _name.local.as_ref() == "img" {
        if let Some((width, height)) = style.intrinsic_size {
            let context = engine.register_replaced(width, height);
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
    // In flex and grid formatting contexts, collapsible whitespace-only text
    // between items does not generate an anonymous item. Taffy places each
    // stray whitespace leaf in the item sequence, shifting every real child.
    // This is especially visible in pretty-printed HTML: indentation before a
    // flex child becomes several pixels of unexplained leading space.
    //
    // Flatten display:contents first because its children participate as direct
    // flex/grid items. Otherwise formatting whitespace inside the transparent
    // wrapper would survive this filter and become an item in `build_any`.
    if matches!(style.display, crate::Display::Flex | crate::Display::Grid) {
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
    }
    // `float` has no effect on a flex or grid item. Legacy stylesheets often
    // leave floats on children after a newer rule turns their parent into a
    // flex container; routing those children through the block float-zone
    // approximation corrupts flex sizing and percentage-margin placement.
    let has_float_child = style.display == crate::Display::Block
        && dom_children.iter().any(|&cid| styles.get(&cid).map(|s| s.float.is_some()).unwrap_or(false));

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
    if style.display == crate::Display::Block && has_inline_ish_content && !has_float_child {
        return build_mixed_block(tree, id, style, taffy_style, &dom_children, taffy_tree, id_map, words, engine, ifc_items, styles);
    }

    if stacks_children_vertically && has_inline_ish_content {
        taffy_style.display = taffy::style::Display::Flex;
        taffy_style.flex_direction = taffy::FlexDirection::Row;
        taffy_style.flex_wrap = taffy::FlexWrap::Wrap;

        // Before promotion, `align_items` was the horizontal-position stand-in
        // for a column container (from `text-align` or `<center>`; see
        // `style::apply_value`'s "text-align" arm): correct there, since a
        // column's cross axis is horizontal. The promoted container's *main*
        // axis is horizontal instead, so that same "where should content sit
        // horizontally" intent now belongs on `justify_content`, not
        // `align_items` — otherwise e.g. `text-align: right` shoves wrapped
        // text to the bottom of its line (the new cross axis) rather than to
        // the right. Real `justify-content` from actual CSS wins if present.
        if style.justify_content.is_none() {
            taffy_style.justify_content = match style.align_items {
                Some(taffy::AlignItems::FLEX_END) => Some(taffy::JustifyContent::FLEX_END),
                Some(taffy::AlignItems::CENTER) => Some(taffy::JustifyContent::CENTER),
                _ => taffy_style.justify_content,
            };
        }
        taffy_style.align_items = Some(taffy::AlignItems::FLEX_START);
    }

    // Table-cell vertical alignment: place the cell's content along the
    // vertical axis of whichever shape the cell took, the flex-column
    // stand-in's main axis or the wrapped lines of a promoted inline
    // context. (The pure-text leaf path is aligned at finalize instead,
    // where the shaped text height is known.)
    if let Some(va) = style.vertical_align {
        if taffy_style.display == taffy::style::Display::Flex {
            if taffy_style.flex_direction == taffy::FlexDirection::Column {
                if style.justify_content.is_none() {
                    taffy_style.justify_content = Some(match va {
                        crate::VerticalAlign::Top => taffy::JustifyContent::FLEX_START,
                        crate::VerticalAlign::Middle => taffy::JustifyContent::CENTER,
                        crate::VerticalAlign::Bottom => taffy::JustifyContent::FLEX_END,
                    });
                }
            } else {
                taffy_style.align_content = Some(match va {
                    crate::VerticalAlign::Top => taffy::AlignContent::FLEX_START,
                    crate::VerticalAlign::Middle => taffy::AlignContent::CENTER,
                    crate::VerticalAlign::Bottom => taffy::AlignContent::FLEX_END,
                });
            }
        }
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

    let taffy_id = if child_ids.is_empty() {
        taffy_tree.new_leaf(taffy_style).ok()?
    } else {
        taffy_tree.new_with_children(taffy_style, &child_ids).ok()?
    };
    id_map.insert(taffy_id, id);
    Some(taffy_id)
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
            s.display == crate::Display::Inline
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
                child_ids.extend(build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles));
            }
            Seg::Run(run) => {
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
                let wrapper = taffy_tree.new_with_children(run_wrapper_style(style), &atoms).ok()?;
                child_ids.push(wrapper);
            }
        }
    }
    // Pseudo content that found no adjacent run to join.
    if before_pending {
        let wrapper = taffy_tree.new_with_children(run_wrapper_style(style), &before_leaves).ok()?;
        child_ids.insert(0, wrapper);
    }
    if after_pending {
        let wrapper = taffy_tree.new_with_children(run_wrapper_style(style), &after_leaves).ok()?;
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
/// context. The parent's `text-align` stand-in (align_items) moves the run's
/// line content via justify-content, exactly as the old whole-container
/// promotion did, but scoped to the run so sibling blocks stay full width.
fn run_wrapper_style(parent: &crate::LayoutStyle) -> taffy::Style {
    let justify = match parent.align_items {
        Some(taffy::AlignItems::FLEX_END) => Some(taffy::JustifyContent::FLEX_END),
        Some(taffy::AlignItems::CENTER) => Some(taffy::JustifyContent::CENTER),
        _ => None,
    };
    taffy::Style {
        display: taffy::style::Display::Flex,
        flex_direction: taffy::FlexDirection::Row,
        flex_wrap: taffy::FlexWrap::Wrap,
        align_items: Some(taffy::AlignItems::FLEX_START),
        justify_content: justify,
        size: taffy::Size { width: taffy::style::Dimension::percent(1.0), height: taffy::style::Dimension::auto() },
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

    let Some(float_idx) = dom_children.iter().position(|&cid| is_float(cid)) else {
        return dom_children.iter().flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles)).collect();
    };

    let mut result: Vec<taffy::NodeId> = dom_children[..float_idx]
        .iter()
        .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
        .collect();

    let float_side = styles.get(&dom_children[float_idx]).and_then(|s| s.float);

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
        let run_children: Vec<taffy::NodeId> = dom_children[float_idx..run_end]
            .iter()
            .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
            .collect();
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
            let row_children: Vec<taffy::NodeId> = match float_side {
                Some(crate::Float::Left) => [Some(float_id), flow_column].into_iter().flatten().collect(),
                _ => [flow_column, Some(float_id)].into_iter().flatten().collect(),
            };
            if let Ok(row) = taffy_tree.new_with_children(row_style, &row_children) {
                result.push(row);
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
