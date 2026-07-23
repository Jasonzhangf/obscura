//! Real inline text layout via cosmic-text (HarfBuzz-class shaping through
//! rustybuzz + UAX#14 line breaking), replacing the earlier approximation
//! that split each text node into one taffy flex item per word.
//!
//! The model matches how real browsers separate formatting contexts: taffy
//! lays out the *block/flex/grid* boxes, and an *inline formatting context*
//! (a box whose children are all inline-level text/spans) collapses to a
//! single taffy leaf whose measure function line-breaks its shaped text at
//! whatever width taffy offers. Line wrapping, alignment, and intrinsic
//! sizing then come from a real text engine instead of flexbox tricks.
//!
//! Fonts are loaded from embedded bytes only, never the OS, so layout is
//! byte-for-byte deterministic across hosts (the whole engine's guarantee).

use std::sync::Arc;

use cosmic_text::{Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight, Wrap};

use obscura_dom::tree::{DomTree, NodeId};

use crate::{Dimension, Display, LayoutStyle, Rect, TextTransform};

// Bundled faces. Chrome on this class of host renders `sans-serif` and the
// ubiquitous Arial/Helvetica/system-ui stacks as Liberation Sans, `serif` as
// Liberation Serif, and `monospace` as Liberation Mono. Matching those keeps
// text metrics (advance widths, wrapping, line positions) aligned with Chromium
// instead of drifting ~12% wider on DejaVu's wider glyphs. DejaVu Sans is kept
// only as a last-resort fallback for glyphs the Liberation faces lack.
static SANS_R: &[u8] = include_bytes!("../assets/liberation-sans.ttf");
static SANS_B: &[u8] = include_bytes!("../assets/liberation-sans-bold.ttf");
static SANS_O: &[u8] = include_bytes!("../assets/liberation-sans-oblique.ttf");
static SANS_BO: &[u8] = include_bytes!("../assets/liberation-sans-boldoblique.ttf");
static SERIF_R: &[u8] = include_bytes!("../assets/liberation-serif.ttf");
static SERIF_B: &[u8] = include_bytes!("../assets/liberation-serif-bold.ttf");
static SERIF_O: &[u8] = include_bytes!("../assets/liberation-serif-oblique.ttf");
static SERIF_BO: &[u8] = include_bytes!("../assets/liberation-serif-boldoblique.ttf");
static MONO_R: &[u8] = include_bytes!("../assets/liberation-mono.ttf");
static MONO_B: &[u8] = include_bytes!("../assets/liberation-mono-bold.ttf");
static MONO_O: &[u8] = include_bytes!("../assets/liberation-mono-oblique.ttf");
static MONO_BO: &[u8] = include_bytes!("../assets/liberation-mono-boldoblique.ttf");
static FALLBACK: &[u8] = include_bytes!("../assets/dejavu-sans.ttf");

const FAMILY: &str = "Liberation Sans";
const SERIF_FAMILY: &str = "Liberation Serif";
const MONO_FAMILY: &str = "Liberation Mono";

/// Map a CSS `font-family` list to a bundled face the way Chromium resolves the
/// generic families on this host: a monospace/code stack -> Liberation Mono, a
/// serif stack -> Liberation Serif, everything else (sans-serif, Arial,
/// Helvetica, system-ui, named sans webfonts, ...) -> Liberation Sans. The first
/// family whose category is recognizable wins, matching CSS fallback order.
fn resolve_font_family(fam: Option<&str>) -> &'static str {
    let Some(f) = fam else { return FAMILY };
    for tok in f.split(',') {
        let t = tok.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        if t.is_empty() {
            continue;
        }
        if t == "monospace" || t.contains("mono") || t.contains("courier")
            || t.contains("consol") || t == "menlo" || t == "monaco" || t == "code"
        {
            return MONO_FAMILY;
        }
        if t == "serif" || t == "georgia" || t.contains("times") || t == "cambria"
            || t.contains("garamond") || t.contains("liberation serif") || t == "roman"
        {
            return SERIF_FAMILY;
        }
        if t == "sans-serif" || t.contains("sans") || t == "arial" || t == "helvetica"
            || t == "helvetica neue" || t == "system-ui" || t == "-apple-system"
            || t == "roboto" || t == "segoe ui" || t == "inter" || t == "verdana"
            || t == "tahoma" || t == "ui-sans-serif"
        {
            return FAMILY;
        }
        // Unrecognized named webfont: keep scanning for a generic fallback.
    }
    FAMILY
}
/// `line-height: normal` as a multiple of font-size. Browsers derive this from
/// font metrics; ~1.15 matches Liberation Sans (the default face) closely.
const NORMAL_LINE_HEIGHT: f32 = 1.15;
/// Underline is flagged per glyph through cosmic-text's `metadata` field.
const META_UNDERLINE: usize = 1;

/// One inline formatting context: a shaped cosmic-text buffer plus where to
/// paint it (filled in after layout).
pub struct InlineItem {
    buffer: Buffer,
    /// Content-box top-left in viewport coordinates, set by `finalize`.
    origin: (f32, f32),
    /// Ancestor `overflow: hidden` clip, set by `finalize`.
    clip: Option<Rect>,
    /// `-webkit-background-clip: text` fill: when set, the glyphs are painted
    /// with this background (a linear gradient sampled across the text box, or a
    /// solid color as a flat two-stop gradient) instead of the transparent text
    /// color that would make them invisible. See [`clip_text_fill`].
    clip_fill: Option<(f32, Vec<([u8; 4], Option<f32>)>)>,
}

/// Owns the font set and shaping caches for one render pass, plus every
/// inline formatting context discovered while building the tree. Lives in
/// [`crate::DomLayout`] so paint can rasterize the shaped glyphs.
pub struct TextEngine {
    font_system: FontSystem,
    swash: SwashCache,
    items: Vec<InlineItem>,
    replaced: Vec<ReplacedItem>,
}

const REPLACED_CONTEXT_BIT: usize = 1usize << (usize::BITS - 1);

#[derive(Clone, Copy)]
struct ReplacedItem {
    intrinsic_width: f32,
    intrinsic_height: f32,
    min_width: Option<f32>,
    min_height: Option<f32>,
    max_width: Option<f32>,
    max_height: Option<f32>,
}

impl ReplacedItem {
    fn from_style(width: f32, height: f32, style: &LayoutStyle) -> Self {
        let px = |dimension| match dimension {
            Dimension::Px(value) => Some(value.max(0.0)),
            _ => None,
        };
        ReplacedItem {
            intrinsic_width: width,
            intrinsic_height: height,
            min_width: px(style.min_width),
            min_height: px(style.min_height),
            max_width: px(style.max_width),
            max_height: px(style.max_height),
        }
    }

    fn clamp(value: f32, min: Option<f32>, max: Option<f32>) -> f32 {
        // CSS sizing gives the minimum precedence when min > max.
        let value = max.map_or(value, |max| value.min(max));
        min.map_or(value, |min| value.max(min))
    }

    fn size(self, known: taffy::Size<Option<f32>>) -> taffy::Size<f32> {
        let (width, height) = match (known.width, known.height) {
            (Some(width), Some(height)) => (width, height),
            (Some(width), None) => (
                width,
                width * self.intrinsic_height / self.intrinsic_width,
            ),
            (None, Some(height)) => (
                height * self.intrinsic_width / self.intrinsic_height,
                height,
            ),
            (None, None) => (self.intrinsic_width, self.intrinsic_height),
        };
        taffy::Size {
            width: Self::clamp(width, self.min_width, self.max_width),
            height: Self::clamp(height, self.min_height, self.max_height),
        }
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    pub fn new() -> Self {
        // Build a database from our four embedded faces only. Never call
        // load_system_fonts: a host's font set would make layout differ
        // machine to machine and add a multi-millisecond startup scan.
        let mut db = cosmic_text::fontdb::Database::new();
        for bytes in [
            SANS_R, SANS_B, SANS_O, SANS_BO,
            SERIF_R, SERIF_B, SERIF_O, SERIF_BO,
            MONO_R, MONO_B, MONO_O, MONO_BO,
            FALLBACK,
        ] {
            db.load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(bytes)));
        }
        db.set_sans_serif_family(FAMILY);
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        TextEngine {
            font_system,
            swash: SwashCache::new(),
            items: Vec::new(),
            replaced: Vec::new(),
        }
    }

    /// Number of inline formatting contexts collected (for debug/stats).
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Build an inline formatting context for `id`'s subtree if it is one we
    /// can collapse to shaped text (see [`is_pure_text_ifc`]); returns the
    /// item index to store as the leaf's taffy context. `None` means the
    /// container is not a pure-text IFC and should build through the normal
    /// (block / flex / word-split) path.
    pub fn try_build(&mut self, tree: &DomTree, id: NodeId, styles: &std::collections::HashMap<NodeId, LayoutStyle>) -> Option<usize> {
        if !is_pure_text_ifc(tree, id, styles) {
            return None;
        }
        let base = styles.get(&id)?;
        let (ctx, clip_fill) = base_span_ctx(base);
        let mut spans: Vec<(String, SpanAttrs)> = Vec::new();
        let mut collector = Collector { last_was_space: true };
        collect_spans(tree, id, styles, ctx, &mut spans, &mut collector);
        self.push_shaped_item(base, spans, clip_fill)
    }

    /// Build an inline formatting context from a *run* of consecutive
    /// inline-level siblings inside `parent` (a mixed-content block whose
    /// other children are block-level). The run folds to one shaped buffer
    /// exactly like a whole-container IFC, using the parent's style as the
    /// base. Returns `None` when any node in the run cannot fold (atomic
    /// inline, replaced element, ...) or the run has no visible text; the
    /// caller then falls back to the flex-wrap wrapper for that run.
    pub fn try_build_run(
        &mut self,
        tree: &DomTree,
        parent: NodeId,
        run: &[NodeId],
        styles: &std::collections::HashMap<NodeId, LayoutStyle>,
    ) -> Option<usize> {
        let mut has_text = false;
        for &cid in run {
            if !inline_child_ok(tree, cid, styles, &mut has_text) {
                return None;
            }
        }
        if !has_text {
            return None;
        }
        let base = styles.get(&parent)?;
        let (ctx, clip_fill) = base_span_ctx(base);
        let mut spans: Vec<(String, SpanAttrs)> = Vec::new();
        let mut collector = Collector { last_was_space: true };
        for &cid in run {
            collect_node_spans(tree, cid, styles, ctx, &mut spans, &mut collector);
        }
        self.push_shaped_item(base, spans, clip_fill)
    }

    /// Shared tail of [`try_build`] / [`try_build_run`]: shape the collected
    /// spans into a cosmic-text buffer under `base`'s font metrics and
    /// alignment, and store it as a new inline item.
    fn push_shaped_item(
        &mut self,
        base: &LayoutStyle,
        mut spans: Vec<(String, SpanAttrs)>,
        clip_fill: Option<(f32, Vec<([u8; 4], Option<f32>)>)>,
    ) -> Option<usize> {
        // Trim a single trailing space so it does not widen the last line.
        if let Some((text, _)) = spans.last_mut() {
            if text.ends_with(' ') {
                text.pop();
            }
        }
        if spans.iter().all(|(t, _)| t.trim().is_empty()) {
            return None;
        }

        let base_size = base.font_size.unwrap_or(16.0);
        // Real `line-height` drives vertical rhythm; a fixed ratio made
        // real-site prose (e.g. Wikipedia's 1.6) noticeably tighter than
        // Chromium. `normal` is font-relative; ~1.2 matches DejaVu closely.
        let line_h = match base.line_height {
            Some(crate::LineHeight::Px(px)) => px,
            Some(crate::LineHeight::Ratio(r)) => base_size * r,
            _ => base_size * NORMAL_LINE_HEIGHT,
        };
        // cosmic-text asserts (an uncatchable process abort) if font size OR
        // line height is 0. `font-size:0` is a common whitespace-collapse trick
        // and drives both to 0, so floor both at 1px here. The glyphs stay
        // ~invisible, matching the intent, and one page can never abort a worker.
        let cosmic_size = base_size.max(1.0);
        let metrics = Metrics::new(cosmic_size, line_h.max(cosmic_size).ceil());
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        // Break only at word boundaries, never mid-word. This keeps the
        // min-content width (measured at width 0) equal to the longest word,
        // so a table/flex column never squeezes narrower than a whole word
        // and forces an ugly mid-word break; a single word wider than its
        // column overflows instead, matching normal `overflow-wrap: normal`.
        buffer.set_wrap(&mut self.font_system, Wrap::Word);
        // Always Advanced shaping: Basic mis-maps per-span attribute
        // boundaries in cosmic-text 0.12 (a multi-color run like body text
        // with links ends up coloring the wrong glyphs), and shaping is a
        // small fraction of a page's CSS-cascade-dominated render time.
        let rich = spans.iter().map(|(t, a)| (t.as_str(), a.to_attrs()));
        buffer.set_rich_text(&mut self.font_system, rich, Attrs::new().family(Family::Name(FAMILY)), Shaping::Advanced);

        let align = match base.text_align {
            Some(taffy::AlignItems::CENTER) => Some(Align::Center),
            Some(taffy::AlignItems::FLEX_END) => Some(Align::End),
            _ => None,
        };
        if let Some(a) = align {
            for line in buffer.lines.iter_mut() {
                line.set_align(Some(a));
            }
        }

        let idx = self.items.len();
        self.items.push(InlineItem { buffer, origin: (0.0, 0.0), clip: None, clip_fill });
        Some(idx)
    }

    /// Measure the inline context `idx` at `width` (content-box width, or
    /// None for max-content), returning its shaped (width, height). Called by
    /// taffy's measure function during layout.
    pub fn measure(&mut self, idx: usize, width: Option<f32>) -> (f32, f32) {
        if idx & REPLACED_CONTEXT_BIT != 0 {
            let size = self.replaced[idx & !REPLACED_CONTEXT_BIT]
                .size(taffy::Size { width, height: None });
            return (size.width, size.height);
        }
        let TextEngine { font_system, items, .. } = self;
        let item = &mut items[idx];
        item.buffer.set_size(font_system, width.map(|w| w.max(0.0)), None);
        item.buffer.shape_until_scroll(font_system, false);
        buffer_size(&item.buffer)
    }

    /// Register a replaced element's intrinsic size as a taffy measure
    /// context. Percentage-sized image leaves still need their intrinsic
    /// max-content contribution while an auto-sized ancestor is measured.
    pub fn register_replaced(
        &mut self,
        width: f32,
        height: f32,
        style: &LayoutStyle,
    ) -> usize {
        let index = self.replaced.len();
        self.replaced
            .push(ReplacedItem::from_style(width, height, style));
        REPLACED_CONTEXT_BIT | index
    }

    /// Measure either a shaped text context or an intrinsic replaced element.
    /// Replaced boxes transfer a definite axis through their intrinsic ratio;
    /// with neither axis definite they contribute their natural size.
    pub fn measure_taffy(
        &mut self,
        idx: usize,
        known: taffy::Size<Option<f32>>,
        available: taffy::Size<taffy::AvailableSpace>,
    ) -> taffy::Size<f32> {
        if idx & REPLACED_CONTEXT_BIT != 0 {
            return self.replaced[idx & !REPLACED_CONTEXT_BIT].size(known);
        }
        let width = known.width.or(match available.width {
            taffy::AvailableSpace::Definite(width) => Some(width),
            taffy::AvailableSpace::MinContent => Some(0.0),
            taffy::AvailableSpace::MaxContent => None,
        });
        let (width, height) = self.measure(idx, width);
        taffy::Size { width, height }
    }

    /// After layout, pin each context to its final content-box origin and
    /// clip, reshaping once at the resolved width so paint draws the same
    /// line breaks the box was sized for.
    pub fn finalize(&mut self, idx: usize, content_origin: (f32, f32), content_width: f32, clip: Option<Rect>) {
        let TextEngine { font_system, items, .. } = self;
        let item = &mut items[idx];
        item.buffer.set_size(font_system, Some(content_width.max(0.0)), None);
        item.buffer.shape_until_scroll(font_system, false);
        item.origin = content_origin;
        item.clip = clip;
    }
}

/// A run of same-styled inline text.
#[derive(Clone, PartialEq)]
struct SpanAttrs {
    bold: bool,
    italic: bool,
    underline: bool,
    color: [u8; 4],
    family: &'static str,
}

impl SpanAttrs {
    fn to_attrs(&self) -> Attrs<'_> {
        let mut a = Attrs::new().family(Family::Name(self.family));
        a = a.weight(if self.bold { Weight::BOLD } else { Weight::NORMAL });
        a = a.style(if self.italic { Style::Italic } else { Style::Normal });
        a = a.color(Color::rgba(self.color[0], self.color[1], self.color[2], self.color[3]));
        // Underline has no native cosmic-text attribute; carry it through the
        // per-glyph metadata so paint can stroke it (see `paint_item`).
        a = a.metadata(if self.underline { META_UNDERLINE } else { 0 });
        a
    }
}

/// Inherited inline context threaded down the subtree while collecting spans.
#[derive(Clone, Copy)]
struct SpanCtx {
    color: [u8; 4],
    bold: bool,
    italic: bool,
    underline: bool,
    transform: TextTransform,
    family: &'static str,
}

struct Collector {
    last_was_space: bool,
}

/// DFS the inline subtree, appending whitespace-collapsed text runs. Adjacent
/// runs with identical attributes are merged so cosmic-text sees the fewest
/// spans. Collapsing spans HTML's insignificant whitespace (runs of spaces,
/// tabs, and newlines fold to one space; leading space at the start of the
/// context is dropped) exactly as `white-space: normal` requires.
/// Root span context (and background-clip-text fill, when active) for an IFC
/// whose base style is `base`.
///
/// `-webkit-background-clip: text` on a transparent-colored element paints
/// its background *through* the glyphs (gradient/solid text). When active,
/// shape the glyphs in opaque white so their coverage renders, then recolor
/// them from the background at paint time; otherwise transparent text stays
/// transparent (and invisible), unchanged.
fn base_span_ctx(base: &LayoutStyle) -> (SpanCtx, Option<(f32, Vec<([u8; 4], Option<f32>)>)>) {
    let clip_fill = clip_text_fill(base);
    let default_color = if clip_fill.is_some() {
        [255, 255, 255, 255]
    } else {
        base.color.unwrap_or([0, 0, 0, 255])
    };
    let ctx = SpanCtx {
        color: default_color,
        bold: base.font_weight.as_deref() == Some("bold"),
        italic: base.font_style_italic.unwrap_or(false),
        underline: base.underline.unwrap_or(false),
        transform: base.text_transform.unwrap_or(TextTransform::None),
        family: resolve_font_family(base.font_family.as_deref()),
    };
    (ctx, clip_fill)
}

fn collect_spans(
    tree: &DomTree,
    id: NodeId,
    styles: &std::collections::HashMap<NodeId, LayoutStyle>,
    ctx: SpanCtx,
    out: &mut Vec<(String, SpanAttrs)>,
    c: &mut Collector,
) {
    for cid in tree.children(id) {
        collect_node_spans(tree, cid, styles, ctx, out, c);
    }
}

/// Collect the spans contributed by one node (a text node's runs, or an
/// element's whole subtree with its style threaded through). Split out of
/// [`collect_spans`] so an inline *run* (a slice of siblings, not a whole
/// container) can also be folded into one shaped buffer.
fn collect_node_spans(
    tree: &DomTree,
    cid: NodeId,
    styles: &std::collections::HashMap<NodeId, LayoutStyle>,
    ctx: SpanCtx,
    out: &mut Vec<(String, SpanAttrs)>,
    c: &mut Collector,
) {
    let Some(node) = tree.get_node(cid) else { return };
    match &node.data {
        obscura_dom::tree::NodeData::Text { contents } => {
            let attrs = SpanAttrs { bold: ctx.bold, italic: ctx.italic, underline: ctx.underline, color: ctx.color, family: ctx.family };
            push_text(contents, ctx.transform, &attrs, out, c);
        }
        _ => {
            let Some(elem) = node.as_element() else { return };
            let style = styles.get(&cid);
            if style.map(|s| s.display == Display::None).unwrap_or(false) {
                return;
            }
            if elem.local.as_ref() == "br" {
                out.push(("\n".to_string(), SpanAttrs { bold: ctx.bold, italic: ctx.italic, underline: ctx.underline, color: ctx.color, family: ctx.family }));
                c.last_was_space = true;
                return;
            }
            let child = SpanCtx {
                color: style.and_then(|s| s.color).unwrap_or(ctx.color),
                bold: ctx.bold || style.map(|s| s.font_weight.as_deref() == Some("bold")).unwrap_or(false),
                italic: ctx.italic || style.and_then(|s| s.font_style_italic).unwrap_or(false),
                // Underline propagates in: an ancestor's underline covers
                // descendant text; an element only sets its own via CSS.
                underline: ctx.underline || style.and_then(|s| s.underline).unwrap_or(false),
                transform: style.and_then(|s| s.text_transform).unwrap_or(ctx.transform),
                family: style
                    .and_then(|s| s.font_family.as_deref())
                    .map(|f| resolve_font_family(Some(f)))
                    .unwrap_or(ctx.family),
            };
            collect_spans(tree, cid, styles, child, out, c);
        }
    }
}

fn push_text(raw: &str, transform: TextTransform, attrs: &SpanAttrs, out: &mut Vec<(String, SpanAttrs)>, c: &mut Collector) {
    let mut buf = String::new();
    let mut at_word_start = c.last_was_space;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !c.last_was_space {
                buf.push(' ');
                c.last_was_space = true;
            }
            at_word_start = true;
        } else {
            match transform {
                TextTransform::Uppercase => buf.extend(ch.to_uppercase()),
                TextTransform::Lowercase => buf.extend(ch.to_lowercase()),
                TextTransform::Capitalize if at_word_start => buf.extend(ch.to_uppercase()),
                _ => buf.push(ch),
            }
            c.last_was_space = false;
            at_word_start = false;
        }
    }
    if buf.is_empty() {
        return;
    }
    if let Some((last_text, last_attrs)) = out.last_mut() {
        if last_attrs == attrs {
            last_text.push_str(&buf);
            return;
        }
    }
    out.push((buf, attrs.clone()));
}

/// Total shaped size of a buffer: widest line, and the bottom of the last line.
fn buffer_size(buffer: &Buffer) -> (f32, f32) {
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
        h = h.max(run.line_top + run.line_height);
    }
    (w.ceil(), h.ceil())
}

/// Does `id` establish an inline formatting context made purely of text and
/// plain inline formatting (no atomic inline boxes, no block children, no
/// inline element carrying its own box)? Such a container collapses cleanly
/// to one shaped buffer; anything else keeps the general build path.
fn is_pure_text_ifc(tree: &DomTree, id: NodeId, styles: &std::collections::HashMap<NodeId, LayoutStyle>) -> bool {
    let Some(style) = styles.get(&id) else { return false };
    // Only containers that lay their children out in normal flow (block, or
    // the flex-column stand-ins our UA sheet uses for td/th/center). A real
    // flex/grid row with inline children is rare and better left to taffy.
    let flow = style.display == Display::Block
        || (style.display == Display::Flex && style.flex_direction == Some(taffy::FlexDirection::Column));
    if !flow {
        return false;
    }
    let mut has_text = false;
    let children = tree.children(id);
    if children.is_empty() {
        return false;
    }
    for cid in &children {
        if !inline_child_ok(tree, *cid, styles, &mut has_text) {
            return false;
        }
    }
    has_text
}

/// Replaced / atomic-inline tags: their box does not contain text, so folding
/// one into a shaped buffer would drop its content entirely. A subtree that
/// contains one is never a pure-text IFC (it keeps the general build path,
/// where the element gets a real taffy box). This is by tag, not display, so a
/// stylesheet setting `img{display:inline}` cannot trick us into folding it.
fn is_replaced(local: &str) -> bool {
    matches!(
        local,
        "img" | "svg" | "canvas" | "video" | "audio" | "iframe" | "embed"
            | "object" | "input" | "textarea" | "select" | "button" | "progress"
            | "meter"
    )
}

/// Is `cid` (and its whole subtree) inline-level, in-flow content safe to fold
/// into a shaped buffer? Sets `has_text` if it contributes any non-whitespace
/// text. Inline wrappers (`<a>`, `<span>`, `<b>`, `<code>`, `<sup>`, ...) are
/// accepted and recursed into even when they carry a background or border of
/// their own: keeping the whole paragraph as one shaped run (correct wrapping
/// plus per-span color/weight/style/underline that `collect_spans` threads) is
/// worth losing an inline decoration, and it avoids the taffy-discouraged
/// flex word-promotion path that breaks real prose wrapping. Only boxes that
/// genuinely cannot fold are rejected: replaced/atomic elements, block-level
/// children, floats, out-of-flow positioned boxes, and elements with generated
/// content (which would be lost).
fn inline_child_ok(tree: &DomTree, cid: NodeId, styles: &std::collections::HashMap<NodeId, LayoutStyle>, has_text: &mut bool) -> bool {
    let Some(node) = tree.get_node(cid) else { return true };
    match &node.data {
        obscura_dom::tree::NodeData::Text { contents } => {
            if !contents.trim().is_empty() {
                *has_text = true;
            }
            true
        }
        obscura_dom::tree::NodeData::Element { .. } => {
            let Some(elem) = node.as_element() else { return true };
            let Some(style) = styles.get(&cid) else { return false };
            if style.display == Display::None {
                return true; // removed from flow; ignore its subtree
            }
            if elem.local.as_ref() == "br" {
                return true;
            }
            // A replaced element or an atomic inline-block has its own box with
            // non-text content; it must stay a real taffy box (keep flex path).
            if is_replaced(elem.local.as_ref()) || style.is_inline_block {
                return false;
            }
            // Only genuinely inline-level, in-flow boxes fold. A block-level
            // child, a float, or an out-of-flow positioned box each needs the
            // general path; so does an element with generated ::before/::after
            // content (lost if folded) or an overflow clip of its own.
            let foldable_inline = style.display == Display::Inline
                && style.position.is_none()
                && style.float.is_none()
                && !style.overflow_hidden
                && style.before_content.is_none()
                && style.after_content.is_none();
            if !foldable_inline {
                return false;
            }
            for gc in tree.children(cid) {
                if !inline_child_ok(tree, gc, styles, has_text) {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

/// Content-box origin for a container whose border box is `rect`, i.e. inside
/// its border and padding: where inline text actually starts.
pub fn content_origin(rect: &Rect, style: &LayoutStyle) -> (f32, f32) {
    (rect.x + style.border.left + style.padding.left, rect.y + style.border.top + style.padding.top)
}

pub fn content_width(rect: &Rect, style: &LayoutStyle) -> f32 {
    (rect.width - style.border.left - style.border.right - style.padding.left - style.padding.right).max(0.0)
}

/// The background to paint through the glyphs for `-webkit-background-clip: text`
/// when the element's own text color is transparent (the common gradient-text
/// technique on hero headings and buttons). Returns the background gradient as
/// is, or a solid background color as a flat two-stop gradient. `None` when the
/// element is not a transparent-text clip-to-text box, so ordinary transparent
/// text still renders invisibly.
fn clip_text_fill(style: &LayoutStyle) -> Option<(f32, Vec<([u8; 4], Option<f32>)>)> {
    if !style.background_clip_text {
        return None;
    }
    // Only when the text itself is transparent: an opaque color paints normally
    // and the clip is a no-op we would otherwise recolor incorrectly.
    if style.color.map(|c| c[3] != 0).unwrap_or(true) {
        return None;
    }
    if let Some(g) = &style.background_gradient {
        if g.1.len() >= 2 {
            return Some(g.clone());
        }
    }
    let bg = style.background_color.filter(|c| c[3] != 0)?;
    Some((180.0, vec![(bg, Some(0.0)), (bg, Some(1.0))]))
}

/// Sample a CSS linear gradient at point `(x, y)` inside a `w` x `h` text box,
/// returning an rgba color. `angle` is CSS degrees clockwise from 12 o'clock
/// (0 = to top, 90 = to right, 180 = to bottom), matching `parse_linear_gradient`
/// and `paint::paint_linear_gradient`. Positionless stops are spread evenly.
fn sample_gradient(fill: &(f32, Vec<([u8; 4], Option<f32>)>), x: f32, y: f32, w: f32, h: f32) -> [u8; 4] {
    let (angle, stops) = fill;
    match stops.len() {
        0 => return [0, 0, 0, 255],
        1 => return stops[0].0,
        _ => {}
    }
    let rad = angle.to_radians();
    let (dx, dy) = (rad.sin(), -rad.cos());
    let (w, h) = (w.max(1.0), h.max(1.0));
    // Full extent of the box along the gradient direction (the CSS gradient-line
    // length), so the endpoints land at the box's projected corners.
    let len = (w * dx).abs() + (h * dy).abs();
    let t = if len <= 0.0 {
        0.5
    } else {
        (((x - w / 2.0) * dx + (y - h / 2.0) * dy) / len + 0.5).clamp(0.0, 1.0)
    };
    let n = stops.len();
    let pos = |i: usize| stops[i].1.unwrap_or(i as f32 / (n as f32 - 1.0)).clamp(0.0, 1.0);
    // Walk to the pair of stops surrounding t, then interpolate between them.
    let mut lo = 0usize;
    while lo + 1 < n && pos(lo + 1) < t {
        lo += 1;
    }
    let hi = (lo + 1).min(n - 1);
    let (p0, p1) = (pos(lo), pos(hi));
    let f = if (p1 - p0).abs() < 1e-6 { 0.0 } else { ((t - p0) / (p1 - p0)).clamp(0.0, 1.0) };
    let (c0, c1) = (stops[lo].0, stops[hi].0);
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round().clamp(0.0, 255.0) as u8;
    [lerp(c0[0], c1[0]), lerp(c0[1], c1[1]), lerp(c0[2], c1[2]), lerp(c0[3], c1[3])]
}

impl TextEngine {
    /// Rasterize inline context `idx` into `pixmap`, honoring its clip. Uses
    /// cosmic-text's swash-backed rasterizer (anti-aliased, per-glyph color
    /// from the span attributes). `offset` is the accumulated `transform:
    /// translate()` of the container's ancestors, shifting both the glyph
    /// origin and the clip so shaped text moves with a transformed box.
    pub fn paint_item(&mut self, idx: usize, pixmap: &mut tiny_skia::Pixmap, offset: (f32, f32)) {
        let TextEngine {
            font_system,
            swash,
            items,
            ..
        } = self;
        let Some(item) = items.get_mut(idx) else { return };
        let (ox, oy) = (item.origin.0 + offset.0, item.origin.1 + offset.1);
        // The glyph origin shifts by the container's accumulated translate,
        // but the clip is already in screen space (owner-shifted at
        // `resolve_clip_rects`) and must not move with the container, or a
        // translated slide would drag its viewport's clip along with it.
        let clip = item.clip;
        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;
        let clip_bounds = clip.map(|c| (c.x, c.y, c.x + c.width, c.y + c.height));

        // Collect underline segments before drawing glyphs (both borrow the
        // buffer). Underline is carried per glyph via metadata; group runs of
        // consecutive underlined glyphs on a line into one stroke below the
        // baseline. Done first so the draw() mutable borrow does not overlap.
        let mut underlines: Vec<(f32, f32, f32, f32, [u8; 4])> = Vec::new(); // x0, x1, y, thickness, color
        for run in item.buffer.layout_runs() {
            let base_y = run.line_y;
            let mut seg: Option<(f32, f32, f32, [u8; 4])> = None; // x0, x1, font_size, color
            for g in run.glyphs {
                let underlined = g.metadata == META_UNDERLINE as usize;
                let col = g.color_opt.map(|c| [c.r(), c.g(), c.b(), c.a()]).unwrap_or([0, 0, 0, 255]);
                if underlined {
                    match &mut seg {
                        Some((_, x1, fs, c)) if *c == col => {
                            *x1 = g.x + g.w;
                            *fs = fs.max(g.font_size);
                        }
                        _ => {
                            if let Some((x0, x1, fs, c)) = seg.take() {
                                underlines.push((x0, x1, base_y + (fs * 0.12).max(1.0), (fs / 14.0).max(1.0), c));
                            }
                            seg = Some((g.x, g.x + g.w, g.font_size, col));
                        }
                    }
                } else if let Some((x0, x1, fs, c)) = seg.take() {
                    underlines.push((x0, x1, base_y + (fs * 0.12).max(1.0), (fs / 14.0).max(1.0), c));
                }
            }
            if let Some((x0, x1, fs, c)) = seg.take() {
                underlines.push((x0, x1, base_y + (fs * 0.12).max(1.0), (fs / 14.0).max(1.0), c));
            }
        }

        // Fallback color if a glyph carries none (shouldn't happen: every span
        // sets one), black.
        let default = Color::rgba(0, 0, 0, 255);
        // `-webkit-background-clip: text`: recolor each glyph pixel from the
        // background gradient sampled across the shaped text box, keeping the
        // glyph's coverage as alpha. The glyphs were shaped opaque (see
        // `try_build`) so this coverage exists; without a clip fill the per-span
        // colors pass through unchanged.
        let clip_fill = item.clip_fill.clone();
        let fill_extent = clip_fill.as_ref().map(|_| buffer_size(&item.buffer));
        let pixels = pixmap.pixels_mut();
        item.buffer.draw(font_system, swash, default, |gx, gy, gw, gh, color| {
            let a = color.a() as u32;
            if a == 0 {
                return;
            }
            let (r, g, b) = match (&clip_fill, fill_extent) {
                (Some(fill), Some((tw, th))) => {
                    let c = sample_gradient(fill, gx as f32 + gw as f32 / 2.0, gy as f32 + gh as f32 / 2.0, tw, th);
                    (c[0], c[1], c[2])
                }
                _ => (color.r(), color.g(), color.b()),
            };
            for dy in 0..gh as i32 {
                for dx in 0..gw as i32 {
                    let px = ox as i32 + gx + dx;
                    let py = oy as i32 + gy + dy;
                    if let Some((cx0, cy0, cx1, cy1)) = clip_bounds {
                        if (px as f32) < cx0 || (px as f32) >= cx1 || (py as f32) < cy0 || (py as f32) >= cy1 {
                            continue;
                        }
                    }
                    if px < 0 || px >= pw || py < 0 || py >= ph {
                        continue;
                    }
                    let idx = (py * pw + px) as usize;
                    let dst = pixels[idx];
                    let sa = a;
                    let sr = (r as u32 * sa) / 255;
                    let sg = (g as u32 * sa) / 255;
                    let sb = (b as u32 * sa) / 255;
                    let inv = 255 - sa;
                    let out_a = sa + (dst.alpha() as u32 * inv / 255);
                    if out_a == 0 {
                        continue;
                    }
                    let out_r = sr + (dst.red() as u32 * inv / 255);
                    let out_g = sg + (dst.green() as u32 * inv / 255);
                    let out_b = sb + (dst.blue() as u32 * inv / 255);
                    pixels[idx] = tiny_skia::PremultipliedColorU8::from_rgba(out_r as u8, out_g as u8, out_b as u8, out_a as u8)
                        .unwrap_or(dst);
                }
            }
        });

        // Stroke the underline segments (opaque; text is already drawn above).
        for (x0, x1, y, thick, col) in underlines {
            let t = thick.max(1.0).round() as i32;
            for dt in 0..t {
                let py = oy as i32 + y as i32 + dt;
                if py < 0 || py >= ph {
                    continue;
                }
                if let Some((_, cy0, _, cy1)) = clip_bounds {
                    if (py as f32) < cy0 || (py as f32) >= cy1 {
                        continue;
                    }
                }
                for px in (ox + x0) as i32..(ox + x1) as i32 {
                    if px < 0 || px >= pw {
                        continue;
                    }
                    if let Some((cx0, _, cx1, _)) = clip_bounds {
                        if (px as f32) < cx0 || (px as f32) >= cx1 {
                            continue;
                        }
                    }
                    let i = (py * pw + px) as usize;
                    pixels[i] = tiny_skia::PremultipliedColorU8::from_rgba(col[0], col[1], col[2], 255).unwrap_or(pixels[i]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    #[test]
    fn clip_fill_only_for_transparent_clip_text() {
        let mut s = LayoutStyle::default();
        // Gradient + clip-to-text + transparent color: fills through the glyphs.
        s.background_clip_text = true;
        s.color = Some([0, 0, 0, 0]);
        s.background_gradient = Some((90.0, vec![(RED, None), (BLUE, None)]));
        assert!(clip_text_fill(&s).is_some());

        // Same, but opaque text: paints normally, no clip fill.
        s.color = Some([10, 20, 30, 255]);
        assert!(clip_text_fill(&s).is_none());

        // Clip-to-text off: ordinary transparent text stays invisible.
        s.color = Some([0, 0, 0, 0]);
        s.background_clip_text = false;
        assert!(clip_text_fill(&s).is_none());

        // Solid background color becomes a flat two-stop gradient.
        s.background_clip_text = true;
        s.background_gradient = None;
        s.background_color = Some([12, 34, 56, 255]);
        let fill = clip_text_fill(&s).expect("solid bg clip fill");
        assert_eq!(fill.1.len(), 2);
        assert_eq!(fill.1[0].0, [12, 34, 56, 255]);
    }

    #[test]
    fn sample_gradient_tints_left_to_right() {
        // 90deg (to right): left edge is the first stop, right edge the last.
        let fill = (90.0f32, vec![(RED, None), (BLUE, None)]);
        let left = sample_gradient(&fill, 0.0, 5.0, 100.0, 10.0);
        let right = sample_gradient(&fill, 100.0, 5.0, 100.0, 10.0);
        assert!(left[0] > left[2], "left end should be reddish: {left:?}");
        assert!(right[2] > right[0], "right end should be bluish: {right:?}");
        // A single-color list samples to that color everywhere.
        let flat = (0.0f32, vec![([7, 8, 9, 255], None)]);
        assert_eq!(sample_gradient(&flat, 3.0, 3.0, 20.0, 20.0), [7, 8, 9, 255]);
    }
}
