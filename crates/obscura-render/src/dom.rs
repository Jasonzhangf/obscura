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
    parent_props: &std::rc::Rc<HashMap<String, String>>,
    node_props: &mut HashMap<NodeId, std::rc::Rc<HashMap<String, String>>>,
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
        if !sheet.is_empty() {
            let node_id = node.get_attribute("id");
            let classes: Vec<String> = node
                .get_attribute("class")
                .map(|c| c.split_whitespace().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            if let Some(m) = sheet.apply(tree, matcher, id, node_id, &classes, elem.local.as_ref(), &mut style, parent_props) {
                this_props = std::rc::Rc::new(m);
            }
        }
        let (before, after) = sheet.pseudo_content(tree, matcher, id);
        style.before_content = before;
        style.after_content = after;
        styles.insert(id, style);
        node_props.insert(id, this_props.clone());
    }

    if is_element {
        matcher.push_ancestor(tree, id);
    }
    for cid in tree.children(id) {
        cascade_walk(tree, cid, sheet, matcher, styles, &this_props, node_props);
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
    // Custom-property (`--x`) map per element, inherited down the tree and
    // shared via Rc so only elements that declare their own tokens allocate a
    // new map. Used to resolve `var()` here and in the inline-style pass.
    let mut node_props: HashMap<NodeId, std::rc::Rc<HashMap<String, String>>> = HashMap::new();
    let root_props = std::rc::Rc::new(HashMap::new());
    // A real preorder walk (not a flat descendants() scan) so the matcher's
    // ancestor bloom filter tracks the current path: push before recursing
    // into children, pop on the way back out. This is what lets descendant
    // combinators (".mw-body .firstHeading") fast-reject via the filter
    // instead of falling back to the always-true "can't reject" case.
    cascade_walk(tree, tree.document(), &sheet, &mut matcher, &mut styles, &root_props, &mut node_props);
    if timing {
        let (r, i, c, l, u) = sheet.debug_stats();
        eprintln!("[timing] parse+index={:?} cascade={:?} rules={} id_keys={} class_keys={} local_keys={} universal={}", t_parse, t1.elapsed(), r, i, c, l, u);
    }
    for nid in tree.descendants(tree.document()) {
        if let Some(node) = tree.get_node(nid) {
            if node.is_element() {
                if let Some(style) = styles.get_mut(&nid) {
                    if let Some(inline) = node.get_attribute("style") {
                        // Resolve var() in inline styles against this element's
                        // custom-property map (design systems set tokens then
                        // reference them inline). Fold in custom properties
                        // declared in this same inline style: they are applied
                        // after the cascade built node_props, so a `--x` set and
                        // referenced inline (e.g. `--bg:url(a.png);
                        // background-image:var(--bg)`) is otherwise unresolved.
                        let inline_vars: Vec<(String, String)> = crate::style::split_declarations(inline)
                            .into_iter()
                            .filter_map(|d| d.split_once(':').map(|(n, v)| (n.trim().to_string(), v.trim().to_string())))
                            .filter(|(n, _)| n.starts_with("--") && n.len() > 2)
                            .collect();
                        let base = node_props.get(&nid);
                        let expanded = if base.is_some() || !inline_vars.is_empty() {
                            let mut map = base.map(|p| (**p).clone()).unwrap_or_default();
                            for (k, v) in inline_vars {
                                map.insert(k, v);
                            }
                            crate::css::substitute_vars(inline, &map, 0)
                        } else {
                            inline.to_string()
                        };
                        crate::style::apply_inline(style, &expanded);
                    }
                    if let Some(color) = node.get_attribute("color") {
                        crate::style::apply_inline(style, &format!("color: {}", color));
                    }
                    if let Some(bgcolor) = node.get_attribute("bgcolor") {
                        crate::style::apply_inline(style, &format!("background-color: {}", bgcolor));
                    }
                    // `width`/`height` HTML attributes are presentational hints:
                    // they rank BELOW author CSS, so a class that sizes the
                    // element (`.logo{height:2rem}` over `<img ... height="250">`)
                    // must win. Apply the attribute only when the cascade left
                    // the dimension unset, else the intrinsic attribute size
                    // wrongly overrides the CSS and the element renders oversized.
                    // Regardless, both attributes together establish the intrinsic
                    // aspect ratio, so a CSS-sized-on-one-axis image (`.logo{
                    // width:auto;height:100%}`) still derives the other axis from
                    // the ratio instead of collapsing to zero.
                    if style.aspect_ratio.is_none() {
                        let aw = node.get_attribute("width").and_then(|w| w.parse::<f32>().ok());
                        let ah = node.get_attribute("height").and_then(|h| h.parse::<f32>().ok());
                        if let (Some(w), Some(h)) = (aw, ah) {
                            if w > 0.0 && h > 0.0 {
                                style.aspect_ratio = Some(w / h);
                            }
                        }
                    }
                    if !style.width_set {
                        if let Some(width) = node.get_attribute("width") {
                            if width.chars().all(|c| c.is_ascii_digit()) {
                                crate::style::apply_inline(style, &format!("width: {}px", width));
                            } else {
                                crate::style::apply_inline(style, &format!("width: {}", width));
                            }
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
                    if !style.height_set {
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
    }

    grow_trailing_auto_cells(tree, &mut styles);
    propagate_border_spacing(tree, &mut styles);

    // The leaf context is the index of a cosmic-text inline formatting
    // context in `engine`; leaves without text carry no context.
    let mut taffy_tree: TaffyTree<usize> = TaffyTree::new();
    let mut id_map: HashMap<taffy::NodeId, NodeId> = HashMap::new();
    let mut words: HashMap<taffy::NodeId, (NodeId, String)> = HashMap::new();
    let mut engine = crate::inline::TextEngine::new();
    let mut ifc_items: HashMap<NodeId, usize> = HashMap::new();

    // The document node itself is not an element; lay out from the first
    // element descendant (the <html> root).
    let root = tree
        .descendants(tree.document())
        .into_iter()
        .find(|id| tree.get_node(*id).map(|n| n.is_element()).unwrap_or(false));

    let mut rects = HashMap::new();
    let mut text_runs = HashMap::new();
    if let Some(root_id) = root {
        // Top-down inheritance of the properties CSS inherits by default.
        #[derive(Clone)]
        struct Inherited {
            color: Option<[u8; 4]>,
            font_size: Option<f32>,
            font_weight: Option<String>,
            visibility_hidden: bool,
            opacity_product: f32,
            list_style: crate::ListStyle,
            line_height: crate::LineHeight,
            text_transform: crate::TextTransform,
            italic: bool,
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
                    visibility_hidden: false,
                    opacity_product: 1.0,
                    // CSS initial value of list-style-type.
                    list_style: crate::ListStyle::Disc,
                    line_height: crate::LineHeight::Normal,
                    text_transform: crate::TextTransform::None,
                    italic: false,
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
                inh.visibility_hidden = style.visibility_hidden.unwrap_or(inh.visibility_hidden);
                inh.opacity_product *= style.opacity.unwrap_or(1.0);
                style.effectively_invisible = inh.visibility_hidden || inh.opacity_product < 0.02;
                match style.list_style { Some(v) => inh.list_style = v, None => style.list_style = Some(inh.list_style) }
                match style.line_height { Some(v) => inh.line_height = v, None => style.line_height = Some(inh.line_height) }
                match style.text_transform { Some(v) => inh.text_transform = v, None => style.text_transform = Some(inh.text_transform) }
                match style.font_style_italic { Some(v) => inh.italic = v, None => style.font_style_italic = Some(inh.italic) }

                // Percentage padding/margin resolve against the containing
                // block WIDTH on every side (per CSS: `padding-top:56.25%` in a
                // 1000px block is 562.5px, not a fraction of the height). The
                // f32 Edges cannot carry a percentage, so bake the resolved px
                // back into `padding`/`margin`, which then feed taffy.
                let cb_w = inh.cb_width;
                for i in 0..4 {
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
                // its own content-box width. taffy sizes width as border-box
                // (its default box_sizing), so subtract this element's resolved
                // padding and border; an auto width fills the containing block.
                let used_w = match style.width {
                    crate::Dimension::Px(w) => w,
                    crate::Dimension::Percent(p) => p * cb_w,
                    _ => (cb_w - style.margin.left - style.margin.right).max(0.0),
                };
                child_cb_width = (used_w
                    - style.padding.left
                    - style.padding.right
                    - style.border.left
                    - style.border.right)
                    .max(0.0);
            }
            inh.cb_width = child_cb_width;
            for cid in tree.children(id).into_iter().rev() {
                queue.push((cid, inh.clone()));
            }
        }

        resolve_grid_areas(tree, root_id, &mut styles);

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
                                // Width to line-break at: a known definite width,
                                // else the available width; MinContent -> narrowest
                                // (longest word), MaxContent -> unbounded (one line).
                                let width = known.width.or(match avail.width {
                                    taffy::AvailableSpace::Definite(w) => Some(w),
                                    taffy::AvailableSpace::MinContent => Some(0.0),
                                    taffy::AvailableSpace::MaxContent => None,
                                });
                                let (w, h) = engine.measure(idx, width);
                                taffy::Size { width: w, height: h }
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
                        let sum_min: f32 = col_min.iter().sum();
                        let sum_max: f32 = col_max.iter().sum();
                        // overhead = table border + padding + inter-column spacing,
                        // recovered from the whole-table min-content width.
                        let overhead = (min_c - sum_min).max(0.0);
                        let target = (used - overhead).max(0.0);
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
            compute_absolute_rects(&taffy_tree, taffy_root, 0.0, 0.0, &id_map, &words, &mut rects, &mut text_runs);
            synthesize_row_rects(tree, &mut rects);
        }
    }

    let mut clip_rects = HashMap::new();
    if let Some(root_id) = root {
        resolve_clip_rects(tree, root_id, None, &rects, &styles, &mut clip_rects);
    }

    // Pin each shaped inline context to its final content-box origin/width now
    // that layout is done, so paint draws the same line breaks it was sized for.
    #[cfg(feature = "paint")]
    for (nid, &idx) in &ifc_items {
        if let (Some(rect), Some(style)) = (rects.get(nid), styles.get(nid)) {
            let origin = crate::inline::content_origin(rect, style);
            let cw = crate::inline::content_width(rect, style);
            // The shaped text is this container's own content, so an
            // `overflow: hidden` on the container clips it (this is what keeps
            // the 1x1 "visually hidden" skip-link box from painting its text,
            // now that the text is one leaf rather than clipped word boxes).
            let inherited = clip_rects.get(nid).copied().flatten();
            let clip = if style.overflow_hidden {
                Some(match inherited {
                    Some(c) => c.intersect(rect).unwrap_or(crate::Rect::default()),
                    None => *rect,
                })
            } else {
                inherited
            };
            engine.finalize(idx, origin, cw, clip);
        }
    }

    DomLayout {
        rects,
        styles,
        clip_rects,
        text_runs,
        #[cfg(feature = "paint")]
        text_engine: engine,
        #[cfg(feature = "paint")]
        ifc_items,
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
) {
    if let Ok(layout) = taffy_tree.layout(taffy_id) {
        let x = abs_x + layout.location.x;
        let y = abs_y + layout.location.y;
        let rect = Rect { x, y, width: layout.size.width, height: layout.size.height };

        if let Some(dom_id) = id_map.get(&taffy_id) {
            rects.insert(*dom_id, rect);
        }
        // A word leaf's dom_id is its owning text node, shared by every other
        // word from the same node, so this appends rather than overwrites.
        if let Some((text_dom_id, word)) = words.get(&taffy_id) {
            text_runs.entry(*text_dom_id).or_default().push((rect, word.clone()));
        }

        if let Ok(children) = taffy_tree.children(taffy_id) {
            for child_id in children {
                compute_absolute_rects(taffy_tree, child_id, x, y, id_map, words, rects, text_runs);
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
            _ => styles
                .get(&cid)
                .map(|s| {
                    // A display:contents wrapper is transparent: whether it
                    // reads as inline content depends on what it splices in.
                    if s.display_contents && s.display != crate::Display::None {
                        has_inline_content(tree, cid, styles)
                    } else {
                        s.display == crate::Display::Inline
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
    ifc_items: &mut HashMap<NodeId, usize>,
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
    ifc_items: &mut HashMap<NodeId, usize>,
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
            let _ = taffy_tree.set_style(cell_node, cstyle);
        }
        children.push(cell_node);
    }
    if children.is_empty() {
        return None;
    }

    // The grid container inherits the table's own box (border, padding,
    // background, margin) but is forced to width:auto so the intrinsic-sizing
    // pass in layout_dom can measure content before choosing the used width.
    let col = || {
        taffy::GridTemplateComponent::Single(taffy::MinMax {
            min: taffy::MinTrackSizingFunction::min_content(),
            max: taffy::MaxTrackSizingFunction::max_content(),
        })
    };
    let row_track = || {
        taffy::GridTemplateComponent::Single(taffy::MinMax {
            min: taffy::MinTrackSizingFunction::auto(),
            max: taffy::MaxTrackSizingFunction::auto(),
        })
    };
    let mut tstyle = to_taffy_style(style);
    tstyle.display = Display::Grid;
    // A percentage width resolves against the container, so keep it and let the
    // used-width pass leave it to taffy. Any other width (px or auto) is forced
    // to auto here so that pass can measure content before choosing the width.
    if !matches!(style.width, crate::Dimension::Percent(_)) {
        tstyle.size.width = Dimension::auto();
    }
    tstyle.grid_template_columns = (0..ncols).map(|_| col()).collect();
    tstyle.grid_template_rows = (0..nrows).map(|_| row_track()).collect();
    if let Some((h, v)) = style.border_spacing {
        tstyle.gap = taffy::Size { width: length(h), height: length(v) };
    }
    let table_node = taffy_tree.new_with_children(tstyle, &children).ok()?;
    id_map.insert(table_node, id);
    Some(table_node)
}

fn build(
    tree: &DomTree,
    id: NodeId,
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut HashMap<NodeId, usize>,
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

    // If this container is a pure-text inline formatting context, collapse its
    // whole subtree to one leaf shaped and line-broken by cosmic-text (real
    // text layout), sized on demand by the measure function. This is the fast,
    // correct path for paragraphs/headings/labels/cells of text; the flex-wrap
    // approximation below only handles the leftovers (mixed inline + atomic
    // boxes, and layout-only builds where `try_build` always declines).
    if let Some(item) = engine.try_build(tree, id, styles) {
        let leaf = taffy_tree.new_leaf_with_context(taffy_style, item).ok()?;
        id_map.insert(leaf, id);
        ifc_items.insert(id, item);
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

    let mut dom_children = tree.children(id);
    // In a grid formatting context, whitespace-only text between items does not
    // generate an anonymous grid item (CSS Grid). taffy 0.12 places each stray
    // whitespace text node in its own cell, which offsets every real item, so a
    // Tailwind `grid-cols-3` block with newlines between its children lays out
    // diagonally instead of in one row (this collapsed the whole modern
    // framework cluster after the taffy upgrade). Drop them before building.
    if style.display == crate::Display::Grid {
        dom_children.retain(|&cid| {
            tree.get_node(cid).map_or(false, |n| n.is_element()) || !tree.text_content(cid).trim().is_empty()
        });
    }
    let has_float_child = dom_children.iter().any(|&cid| styles.get(&cid).map(|s| s.float.is_some()).unwrap_or(false));
    let mut child_ids: Vec<taffy::NodeId> = if has_float_child {
        build_children_with_float_zone(tree, &dom_children, taffy_tree, id_map, words, engine, ifc_items, styles)
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

/// Approximate `float: left|right` without real per-line reflow (which
/// taffy's block/flex/grid modes do not provide): place the float alongside
/// the flow siblings that follow it, up to whichever comes first of the next
/// heading or another floated element, then let everything from there on
/// revert to normal full-width flow.
///
/// This is not a general CSS float implementation (a float taller than its
/// flow zone, or one that should keep affecting content past a heading,
/// won't reflow correctly), but it directly targets the overwhelmingly
/// common real-world shape: a floated image or infobox near the top of an
/// article, sitting beside the intro text, with the rest of the content
/// (starting at the next section heading) running full width beneath it.
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
    (image_height + text_height).max(DEFAULT_FLOAT_HEIGHT_ESTIMATE)
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
    dom_children: &[NodeId],
    taffy_tree: &mut TaffyTree<usize>,
    id_map: &mut HashMap<taffy::NodeId, NodeId>,
    words: &mut HashMap<taffy::NodeId, (NodeId, String)>,
    engine: &mut crate::inline::TextEngine,
    ifc_items: &mut HashMap<NodeId, usize>,
    styles: &HashMap<NodeId, crate::LayoutStyle>,
) -> Vec<taffy::NodeId> {
    let is_float = |cid: NodeId| styles.get(&cid).map(|s| s.float.is_some()).unwrap_or(false);
    let is_heading = |cid: NodeId| {
        tree.get_node(cid)
            .and_then(|n| n.as_element().map(|e| e.local.to_string()))
            .map(|local| matches!(local.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
            .unwrap_or(false)
    };

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
            tree, &dom_children[run_end..], taffy_tree, id_map, words, engine, ifc_items, styles,
        ));
        return result;
    }

    // Stop growing the zone once the flow siblings collected so far would
    // already fill (an estimate of) the float's own height: real float
    // reflow ends when normal-flow content passes the float's bottom edge,
    // not at the next heading regardless of how tall the float actually is.
    // Without this, a short floated thumbnail (a few hundred px) dragged an
    // entire multi-paragraph section into a narrow flow column alongside it
    // — visibly wrong wrapping plus a large empty gap once the (much
    // shorter) float ran out, both from treating "next heading" as the only
    // bound. The estimate is necessarily rough (actual available width is a
    // taffy layout result we don't have yet at tree-build time), but even an
    // approximate bound beats an unbounded one.
    let float_height_budget = estimate_float_height(tree, dom_children[float_idx], styles);
    const ASSUMED_FLOW_WIDTH: f32 = 500.0;
    let mut zone_end = float_idx + 1;
    let mut flow_height_estimate = 0.0f32;
    while zone_end < dom_children.len() && !is_heading(dom_children[zone_end]) && !is_float(dom_children[zone_end]) {
        flow_height_estimate += estimate_text_height(tree, dom_children[zone_end], styles, ASSUMED_FLOW_WIDTH);
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
    let flow_taffy: Vec<taffy::NodeId> = dom_children[float_idx + 1..zone_end]
        .iter()
        .flat_map(|&cid| build_any(tree, cid, taffy_tree, id_map, words, engine, ifc_items, styles))
        .collect();

    match float_taffy {
        Some(float_id) => {
            let flow_column_style = taffy::Style {
                display: taffy::style::Display::Flex,
                flex_direction: taffy::FlexDirection::Column,
                flex_grow: 1.0,
                flex_shrink: 1.0,
                flex_basis: taffy::Dimension::length(0.0),
                min_size: taffy::Size { width: taffy::Dimension::length(0.0), height: taffy::Dimension::auto() },
                ..Default::default()
            };
            let flow_column = if flow_taffy.is_empty() {
                taffy_tree.new_leaf(flow_column_style).ok()
            } else {
                taffy_tree.new_with_children(flow_column_style, &flow_taffy).ok()
            };

            let row_style = taffy::Style {
                display: taffy::style::Display::Flex,
                flex_direction: taffy::FlexDirection::Row,
                align_items: Some(taffy::AlignItems::FLEX_START),
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
        None => result.extend(flow_taffy),
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
}
