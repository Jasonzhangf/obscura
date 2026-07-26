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

pub mod css;
pub use css::Stylesheet;

pub mod style;
pub use style::compute_style;

pub mod dom;
pub use dom::{layout_dom, layout_dom_with_images, layout_dom_with_resources, DomLayout};

#[cfg(feature = "paint")]
mod paint;
#[cfg(feature = "paint")]
pub use paint::{paint_dom, screenshot_png};

// Real inline text layout (cosmic-text) lives behind the paint feature; the
// layout-only build keeps the lighter word-split geometry. The stub lets
// `dom.rs` name `inline::TextEngine` and call `try_build` unconditionally.
#[cfg(feature = "paint")]
pub mod inline;

#[cfg(not(feature = "paint"))]
pub mod inline {
    use obscura_dom::tree::{DomTree, NodeId};
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct TextEngine;

    impl TextEngine {
        pub fn new() -> Self {
            TextEngine
        }
        /// Layout-only builds have no shaper, so no container is ever treated
        /// as a cosmic-text inline formatting context: the word-split path
        /// handles text geometry for `getBoundingClientRect`.
        pub fn try_build(&mut self, _tree: &DomTree, _id: NodeId, _styles: &HashMap<NodeId, crate::LayoutStyle>) -> Option<usize> {
            None
        }
        /// See `try_build`: inline runs likewise fall back to word-split
        /// geometry in layout-only builds.
        pub fn try_build_run(
            &mut self,
            _tree: &DomTree,
            _parent: NodeId,
            _run: &[NodeId],
            _styles: &HashMap<NodeId, crate::LayoutStyle>,
        ) -> Option<usize> {
            None
        }
    }
}

/// An axis-aligned rectangle in CSS pixels, relative to the containing block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// The overlap of two rects, or `None` if they do not intersect (or the
    /// overlap is degenerate). Used to accumulate an ancestor clip chain for
    /// `overflow: hidden`.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        if x1 > x0 && y1 > y0 {
            Some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
        } else {
            None
        }
    }

    /// The smallest rect covering both. Used to derive a table row/section box
    /// from its cells, since `<tr>`/`<tbody>` are not laid out as taffy boxes.
    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.width).max(other.x + other.width);
        let y1 = (self.y + self.height).max(other.y + other.height);
        Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }
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
    Grid,
    Inline,
    #[allow(dead_code)]
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Dimension {
    #[default]
    Auto,
    Px(f32),
    /// 0.0-1.0 fraction of the containing block.
    Percent(f32),
    /// Font-relative and viewport-relative units, kept unresolved at parse
    /// time (the element font-size and viewport are not known then) and
    /// resolved to `Px` during `dom::layout_dom`'s top-down pass via
    /// [`Dimension::resolve`]. Resolving font/viewport units against a
    /// hardcoded 16px at
    /// parse time (the old behavior) silently corrupted every relative length.
    Em(f32),
    Ex(f32),
    Rem(f32),
    Vw(f32),
    Vh(f32),
    Vmin(f32),
    Vmax(f32),
}

impl Dimension {
    /// Resolve font/viewport-relative units to `Px`. `em_px` is the element's
    /// own font-size, `rem_px` the root's, and `vw`/`vh` are one hundredth of
    /// the viewport width/height. `Px`, `Percent`, and `Auto` pass through
    /// (`Percent` stays for taffy to resolve against the containing block).
    pub fn resolve(self, em_px: f32, rem_px: f32, vw: f32, vh: f32) -> Dimension {
        match self {
            Dimension::Em(v) => Dimension::Px(v * em_px),
            // Liberation Sans is the deterministic generic sans face used by
            // the renderer. This is its x-height as a fraction of the em,
            // matching Chromium's generic sans face on the capture host.
            Dimension::Ex(v) => Dimension::Px(v * em_px * 0.528_320_3),
            Dimension::Rem(v) => Dimension::Px(v * rem_px),
            Dimension::Vw(v) => Dimension::Px(v * vw),
            Dimension::Vh(v) => Dimension::Px(v * vh),
            Dimension::Vmin(v) => Dimension::Px(v * vw.min(vh)),
            Dimension::Vmax(v) => Dimension::Px(v * vw.max(vh)),
            other => other,
        }
    }
}

/// The subset of CSS that influences box layout. Expanded in later phases.
#[derive(Debug, Clone, Default)]
pub struct LayoutStyle {
    pub display: Display,
    /// True when `display:flex` is only an internal stand-in for native HTML
    /// layout such as table cells, rather than the computed CSS display.
    /// Descendants are not CSS flex items in these containers.
    pub internal_flex_container: bool,
    /// The HTML UA sheet's vendor `text-align` behavior for `<center>`.
    /// Unlike ordinary `text-align:center`, it also centers fixed-width block
    /// descendants while leaving auto-width blocks fill-available.
    pub legacy_center: bool,
    pub width: Dimension,
    pub height: Dimension,
    /// Which box edge `width`/`height` and min/max sizes describe. CSS starts
    /// at `content-box`; many modern reset sheets opt into `border-box`.
    pub box_sizing: BoxSizing,
    /// Whether `width`/`height` was set by an author rule (including an explicit
    /// `auto`). Presentational `width`/`height` HTML attributes are a lower
    /// priority than author CSS, so they apply only when these are false; an
    /// explicit `width:auto` must still suppress a `width="408"` attribute so
    /// the element keeps its aspect-ratio size instead of the intrinsic one.
    pub width_set: bool,
    pub height_set: bool,
    pub min_width: Dimension,
    pub min_height: Dimension,
    pub max_width: Dimension,
    pub max_height: Dimension,
    /// Deferred CSS math expressions for width, height, min-width, min-height,
    /// max-width, and max-height. Functional lengths can depend on the actual
    /// viewport, containing block, or computed font size and cannot be safely
    /// collapsed to pixels during stylesheet parsing.
    pub size_expressions: [Option<String>; 6],
    /// `aspect-ratio` as width/height, or an image's intrinsic ratio resolved
    /// at layout. Lets a replaced element (or a padding-box card) derive the
    /// missing dimension from the given one, so a `width:100%` image gets a
    /// real height instead of collapsing to zero.
    pub aspect_ratio: Option<f32>,
    /// Fetched intrinsic CSS-pixel size for a replaced element. Kept alongside
    /// `aspect_ratio` so its taffy leaf can contribute a real min/max-content
    /// size when percentage dimensions are resolved through an auto-sized
    /// wrapper (`img { width:100%; height:auto }`).
    pub intrinsic_size: Option<(f32, f32)>,
    pub margin: Edges,
    /// Which margin sides are `auto` (top, right, bottom, left). `margin: 0
    /// auto` / `margin-inline: auto` centering needs a real Auto margin, which
    /// the f32 `margin` cannot express; this flag drives it at taffy mapping.
    pub margin_auto: [bool; 4],
    /// Percentage margin per side (top, right, bottom, left) as a 0..1 fraction,
    /// `None` when the side is a fixed length. Like padding, every side resolves
    /// against the containing block's WIDTH; the f32 `margin` cannot carry a
    /// percentage, so this is resolved to px during `dom::layout_dom`'s top-down
    /// pass once the containing-block width is known.
    pub margin_percent: [Option<f32>; 4],
    /// Font- and viewport-relative margin lengths (top, right, bottom, left).
    /// These retain their unit until the top-down pass knows the element font
    /// size, root font size, and viewport dimensions.
    pub margin_relative: [Option<Dimension>; 4],
    /// Deferred `calc()`/`min()`/`max()`/`clamp()` margin expressions.
    pub margin_expressions: [Option<String>; 4],
    pub padding: Edges,
    /// Percentage padding per side (top, right, bottom, left) as a 0..1
    /// fraction, `None` when the side is a fixed length. All four sides resolve
    /// against the containing block's WIDTH (per CSS, including top/bottom): this
    /// is the responsive aspect-ratio-box trick (`padding-top:56.25%` reserves a
    /// 16:9 area). The f32 `padding` cannot carry a percentage, so it is
    /// resolved to px in `dom::layout_dom`'s top-down pass and written back into
    /// `padding`, which then feeds taffy as a fixed length.
    pub padding_percent: [Option<f32>; 4],
    /// Font- and viewport-relative padding lengths (top, right, bottom, left),
    /// resolved alongside `margin_relative` during the top-down pass.
    pub padding_relative: [Option<Dimension>; 4],
    /// Deferred `calc()`/`min()`/`max()`/`clamp()` padding expressions.
    pub padding_expressions: [Option<String>; 4],
    pub border: Edges,
    /// `border-radius` (uniform; the first value of the shorthand). Rounds the
    /// background fill and border. In px after resolution.
    pub border_radius: f32,
    /// RGBA for the paint step. Parsed always (cheap), used only with `paint`.
    pub background_color: Option<[u8; 4]>,
    /// `linear-gradient(...)` background: (angle in degrees clockwise from 12
    /// o'clock per CSS, list of (rgba, optional 0..1 stop position)). Modern
    /// hero sections use gradients heavily; without this they paint white.
    pub background_gradient: Option<(f32, Vec<([u8; 4], Option<f32>)>)>,
    /// `conic-gradient(...)` background. The angle is the CSS `from` angle,
    /// the center is a fraction of the border box, and stops are normalized
    /// during paint. Conic gradients commonly provide the color source for a
    /// repeated SVG mask in modern hero artwork.
    pub background_conic_gradient:
        Option<(f32, (f32, f32), Vec<([u8; 4], Option<f32>)>)>,
    /// The first `url(...)` reference from `background`/`background-image`
    /// (gradients and repeat keywords in the same shorthand are ignored: we
    /// paint the referenced image, not the gradient layer).
    pub background_image: Option<String>,
    /// `background-size`, in px, when given as explicit length(s) (a bare
    /// `10px` applies to both axes, matching how small square icons are
    /// almost always sized).
    pub background_size: Option<(f32, f32)>,
    /// Keyword `background-size` behavior. `None` is CSS `auto`, which uses
    /// the image's intrinsic dimensions rather than stretching it to the box.
    pub background_size_fit: Option<ObjectFit>,
    /// `background-position` as a 0.0-1.0 fraction per axis (0,0 = default
    /// top-left; 1,0.5 = "right center"). The fraction applies to the leftover
    /// space after resolving explicit, intrinsic, cover, or contain size.
    pub background_position: (f32, f32),
    /// `background-clip: text` / `-webkit-background-clip: text`: the background
    /// paints only through the element's glyphs, not as a filled box. Combined
    /// with a transparent text color this is the common gradient-text technique
    /// (hero headings, buttons like astro.build's "Get Started"); without
    /// honoring it those labels paint invisible. Consumed in the text paint path
    /// (`inline::TextEngine` fills glyphs from the background; `paint` suppresses
    /// the box fill so the gradient does not paint as a rectangle).
    pub background_clip_text: bool,
    /// `mask-image`/`-webkit-mask-image: url(...)`: the ubiquitous "colored,
    /// scalable icon" pattern (an SVG shape used as a stencil, tinted by
    /// `background-color`/`color` instead of carrying its own colors). Without
    /// this, every such icon paints as a solid filled square.
    pub mask_image: Option<String>,
    /// Explicit `mask-size` / `-webkit-mask-size` in CSS px.
    pub mask_size: Option<(f32, f32)>,
    /// Explicit `(repeat-x, repeat-y)` choice. `None` retains the CSS default
    /// (`repeat` on both axes) when an explicit tile size exists, while
    /// preserving the legacy fill-box fallback for unsized icon masks.
    pub mask_repeat: Option<(bool, bool)>,
    /// Foreground (text) color for the paint step.
    pub color: Option<[u8; 4]>,
    pub border_color: Option<[u8; 4]>,
    pub font_size: Option<f32>,
    /// `font-size` given in a font/viewport-relative unit, resolved to
    /// `font_size` (px) during the inheritance pass against the parent and
    /// root font-sizes. `None` when font-size was absolute or unset.
    pub font_size_raw: Option<Dimension>,
    /// Deferred functional `font-size` (`clamp()`, `min()`, `max()`, `calc()`).
    /// These expressions must see the live viewport and parent font size;
    /// eagerly treating `9vw` as the number 9 made responsive headings pin to
    /// the minimum arm of their clamp.
    pub font_size_expression: Option<String>,
    pub font_weight: Option<String>,
    /// The computed `font-family` list, lowercased. Inherited. The text engine
    /// resolves it to a bundled face (Liberation Sans/Serif/Mono) the way
    /// Chromium picks a generic family on this host.
    pub font_family: Option<String>,
    /// Inherited `text-align`, represented with the matching horizontal
    /// alignment keywords. Kept separate from flex/grid `align-items`: using
    /// one field for both made `text-align:left` shrink-wrap flex children.
    pub text_align: Option<taffy::AlignItems>,
    pub align_items: Option<taffy::AlignItems>,
    pub justify_items: Option<taffy::JustifyItems>,
    pub align_self: Option<taffy::AlignSelf>,
    pub justify_self: Option<taffy::JustifySelf>,
    pub align_content: Option<taffy::AlignContent>,
    pub flex_direction: Option<taffy::FlexDirection>,
    pub flex_wrap: Option<taffy::FlexWrap>,
    pub justify_content: Option<taffy::JustifyContent>,
    pub flex_grow: Option<f32>,
    pub flex_shrink: Option<f32>,
    /// Flex/grid item order. The formatting algorithm consumes children in
    /// order-modified document order, with source order breaking ties.
    pub order: i32,
    /// `flex-basis` (longhand, or the length in a `flex:` shorthand). `Auto`
    /// is the default. Fixed-basis sidebars/columns (`flex: 0 0 260px`)
    /// collapse to content width without it.
    pub flex_basis: Dimension,

    // CSS Grid. Tracks are stored as taffy sizing functions; `grid_areas` is the
    // parsed `grid-template-areas` matrix (one Vec per row, `.` for a null cell),
    // resolved to line placements on children in a later pass.
    pub grid_template_columns: Vec<taffy::GridTemplateComponent<String>>,
    pub grid_template_rows: Vec<taffy::GridTemplateComponent<String>>,
    pub grid_auto_flow: Option<taffy::GridAutoFlow>,
    pub grid_areas: Option<Vec<Vec<String>>>,
    pub grid_area_name: Option<String>,
    pub grid_column: Option<taffy::Line<taffy::GridPlacement>>,
    pub grid_row: Option<taffy::Line<taffy::GridPlacement>>,
    /// `[line-name]` -> 1-based grid line number, parsed from
    /// `grid-template-columns`/`-rows`. taffy has no native named-line support,
    /// so children placed by name (`grid-column: content-start / content-end`,
    /// widely used by the Guardian and other editorial grids) are resolved to
    /// numeric lines against these maps in `dom::resolve_grid_areas`.
    pub grid_col_line_names: Option<std::collections::HashMap<String, i16>>,
    pub grid_row_line_names: Option<std::collections::HashMap<String, i16>>,
    /// Raw `grid-column`/`grid-row` value when it references a named line (so it
    /// cannot be resolved to a `taffy::Line` until the parent's line-name map is
    /// known). Resolved in the same later pass; numeric/`span` values still fill
    /// `grid_column`/`grid_row` directly at cascade time.
    pub grid_column_raw: Option<String>,
    pub grid_row_raw: Option<String>,
    pub column_gap: Option<f32>,
    pub row_gap: Option<f32>,
    /// Deferred gap values. Font- and viewport-relative units cannot be
    /// converted until the element's computed font-size and the live viewport
    /// are known; eagerly treating `rem` as 16px breaks pages that customize
    /// the root font-size.
    pub column_gap_expression: Option<String>,
    pub row_gap_expression: Option<String>,

    /// `border-spacing: <horizontal> <vertical>?` (or the `cellspacing`
    /// attribute). Only meaningful on a `<table>`; taffy has no native table
    /// display mode, so `dom::propagate_border_spacing` distributes this down
    /// as the table's own row gap and each descendant `<tr>`'s column gap.
    pub border_spacing: Option<(f32, f32)>,
    /// Computed `border-collapse`. This property is inherited; `None` means
    /// no value was specified on this node yet and is resolved top-down
    /// before table construction. The collapsed-border conflict/paint model
    /// is still approximate, but collapsed tables must at minimum contribute
    /// no border-spacing to their geometry.
    pub border_collapse: Option<bool>,

    // Positioning. `position: absolute|fixed` takes the box out of normal flow.
    pub position: Option<taffy::Position>,
    /// Distinguishes `fixed` from `absolute`; both map to taffy's absolute
    /// layout mode, but fixed boxes use the initial containing block.
    pub position_fixed: bool,
    pub inset: [Option<Dimension>; 4], // top, right, bottom, left
    /// Deferred functional inset expressions in top/right/bottom/left order.
    pub inset_expressions: [Option<String>; 4],

    /// `overflow`/-x/-y other than `visible`: clips this element's descendants
    /// to its border box during paint. This is what makes the ubiquitous
    /// "visually-hidden but accessible" pattern (a 1x1 absolutely-positioned,
    /// clipped box used for skip-links and screen-reader-only labels) actually
    /// invisible instead of painting its text wherever it lands.
    pub overflow_hidden: bool,

    /// `float: left|right`. True CSS float needs per-line reflow around the
    /// float's shape, which taffy's block/flex/grid modes do not do; see
    /// `dom::group_float_zone` for the bounded approximation this drives.
    pub float: Option<Float>,

    /// `visibility: hidden|visible`, own value. `None` means "inherit the
    /// ancestor's computed value" (visibility, unlike most box properties, is
    /// a real inherited CSS property). Resolved into `effectively_invisible`
    /// during `dom::layout_dom`'s inheritance pass.
    pub visibility_hidden: Option<bool>,
    /// `opacity`, own (non-inherited) value in 0.0-1.0. `None` means the
    /// default of 1.0.
    pub opacity: Option<f32>,
    /// First CSS animation name and the parts of its timing contract needed by
    /// a settled, static screenshot. A finite animation with a forwards/both
    /// fill mode contributes its 100% keyframe after normal declarations and
    /// before author `!important`, matching the animation cascade origin.
    pub animation_name: Option<String>,
    pub animation_fill_forwards: bool,
    pub animation_iteration_infinite: bool,
    /// `vertical-align` for a table cell's content. Cells effectively default
    /// to `middle` in browsers (the HTML UA sheet sets it on row groups and
    /// cells inherit it); obscura applies it as main-axis alignment of the
    /// cell's flex-column stand-in. `None` on non-cell elements.
    pub vertical_align: Option<VerticalAlign>,
    /// `z-index` on a positioned element. `None` is `auto` (tree order). A
    /// non-zero value lifts the element's whole subtree into a separate paint
    /// layer: negatives under the normal flow, positives above it, sorted.
    pub z_index: Option<i32>,
    /// `clear`, when set: this element moves below preceding floats on the
    /// given side(s), ending their float zone.
    pub clear: Option<Clear>,
    /// Resolved during the inheritance pass: true when this element should
    /// not be painted at all, either from its own or an inherited
    /// `visibility: hidden`, or because the product of its own and every
    /// ancestor's `opacity` has collapsed to (near) zero. Real `opacity`
    /// composites as translucent groups, which our paint step does not do;
    /// collapsing it to a binary paint/don't-paint decision is enough to
    /// make the extremely common "opacity:0 + visibility:hidden" collapsed
    /// dropdown/panel pattern actually invisible, without needing full alpha
    /// compositing.
    pub effectively_invisible: bool,

    /// Literal text injected by a `::before`/`::after` rule with a plain
    /// string-literal `content` (see `css::Stylesheet::pseudo_content`).
    /// Rendered as an extra word-run at the start/end of this element's
    /// children, same as if it were real text content.
    pub before_content: Option<String>,
    pub after_content: Option<String>,

    /// True for `inline-block`/`inline-flex`/`inline-grid`: participates in
    /// the surrounding inline flow from the outside, like plain `inline`
    /// (both currently collapse to `Display::Inline` — this engine has no
    /// separate inline-block layout mode), but unlike plain `inline` it must
    /// stay a single atomic box rather than have its own content merge into
    /// the parent's line-breaking. `dom::is_flattenable_inline` uses this to
    /// avoid flattening these away: doing so would lose the element as its
    /// own box (including any `::before`/`::after` content attached to it).
    pub is_inline_block: bool,

    /// `display: flow-root`: generates a normal block box but establishes a
    /// new block formatting context, containing descendant floats and stopping
    /// their exclusion bands from propagating into outside siblings.
    pub flow_root: bool,

    /// `display: contents`: the element generates no box of its own; its
    /// children participate in the parent's formatting context directly
    /// (`dom::build_any` splices them into the parent's child list). Kept as a
    /// flag beside `display` because the element still carries inherited styles
    /// for its subtree and `display:none` must still win.
    pub display_contents: bool,

    /// `list-style-type` (or the `list-style` shorthand). Inherited, like in
    /// real CSS; `None` means "not set on this element, inherit". Resolved to
    /// a concrete value during the inheritance pass. Only `<li>` elements draw
    /// a marker from it, but it is carried on every element because it
    /// inherits (a `list-style: none` on a `<ul>` must reach its `<li>`
    /// children, which is how nav menus suppress bullets).
    pub list_style: Option<ListStyle>,

    /// `line-height`. Inherited. `None` means "not set, inherit"; resolved to
    /// a concrete value in the inheritance pass. Drives the vertical rhythm of
    /// shaped text (a fixed ratio made real-site prose noticeably tighter than
    /// Chromium).
    pub line_height: Option<LineHeight>,
    /// Deferred functional line-height (`calc()`, `min()`, `clamp()`) resolved
    /// after the element font and live viewport are known.
    pub line_height_expression: Option<String>,

    /// `text-transform`. Inherited. Applied to span text before shaping.
    pub text_transform: Option<TextTransform>,

    /// `text-decoration-line: underline` (or the `text-decoration` shorthand).
    /// Not inherited in CSS, but a decoration visually covers descendant inline
    /// text, so it is propagated into the shaped spans of the element's subtree
    /// (this is what underlines links, which are underlined by UA default).
    pub underline: Option<bool>,

    /// `font-style: italic|oblique`. Inherited. Selects the oblique face when
    /// shaping (we embed the DejaVu Sans oblique/bold-oblique faces). `None`
    /// means inherit.
    pub font_style_italic: Option<bool>,

    /// `object-fit` for a replaced element (`<img>`). Controls how the decoded
    /// image is scaled into the element's box when their aspect ratios differ;
    /// `Fill` (default) stretches to the box, the rest preserve aspect ratio.
    /// Only consulted in the image paint path.
    pub object_fit: ObjectFit,
    /// `transform: translate(x[, y])` / `translateX` / `translateY`, stored
    /// unresolved as (dx, dy). Applied at paint time as an offset to the
    /// element's own box AND its whole descendant subtree; percentages resolve
    /// against the element's own border-box size then. This is what makes the
    /// canonical `translate(-50%,-50%)` centering of an absolutely-positioned
    /// box land in the right place, and moves a `translate(-9999px,0)`
    /// off-screen skip-link out of view instead of painting it on-screen. Not
    /// inherited (transform is a non-inherited property).
    pub transform_translate: Option<(Dimension, Dimension)>,
    /// Independent CSS-property triggers that establish containing blocks for
    /// absolute and fixed descendants. Kept as a bitset so `filter:none`
    /// cannot clear a transform/containment trigger from another property.
    pub containing_block_triggers: u16,
    /// `transform: scale(sx[, sy])` / `scaleX` / `scaleY`, captured as (sx, sy).
    /// Parsed and stored so a value that mixes scale with translate still yields
    /// its translate part; scale is not yet folded into paint geometry (doing so
    /// correctly would require scaling every descendant's size and text, outside
    /// the paint-time translate-offset model used here).
    pub transform_scale: Option<(f32, f32)>,
    /// `box-shadow` (first layer only). Painted behind the element's own
    /// background/border box: cards, buttons, menus, and modals across the
    /// modern web rely on it for depth, and without it those elements paint
    /// flat. See [`BoxShadow`] and `paint::paint_box_shadow`.
    pub box_shadow: Option<BoxShadow>,
}

pub(crate) const CB_TRIGGER_TRANSFORM: u16 = 1 << 0;
pub(crate) const CB_TRIGGER_FILTER: u16 = 1 << 1;
pub(crate) const CB_TRIGGER_BACKDROP_FILTER: u16 = 1 << 2;
pub(crate) const CB_TRIGGER_PERSPECTIVE: u16 = 1 << 3;
pub(crate) const CB_TRIGGER_CONTAIN: u16 = 1 << 4;
pub(crate) const CB_TRIGGER_WILL_CHANGE: u16 = 1 << 5;
pub(crate) const CB_TRIGGER_CONTENT_VISIBILITY: u16 = 1 << 6;

impl LayoutStyle {
    pub(crate) fn establishes_positioning_containing_block(&self) -> bool {
        self.containing_block_triggers != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Float {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
    /// A specified CSS-wide `inherit` value. The DOM's top-down computed-style
    /// pass resolves this to the parent's computed value before layout.
    Inherit,
}

/// One `box-shadow` layer. Offsets, blur, and spread are in CSS px; `color` is
/// the resolved RGBA (falling back to the element's text color, per CSS
/// `currentColor`, when the value omits a color); `inset` distinguishes an
/// inner shadow from the default outer (drop) shadow. Only the first layer of a
/// comma-separated list is modeled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: [u8; 4],
    pub inset: bool,
}

/// `object-fit` for replaced elements (`<img>`): how the image's intrinsic
/// content is scaled into its box when their aspect ratios differ. `Fill` (the
/// default) stretches to the whole box; the others preserve the image's aspect
/// ratio, either letterboxing inside the box (`Contain`), cropping to cover it
/// (`Cover`), or using the intrinsic size (`None`, or `ScaleDown` which is
/// `Contain` capped at the intrinsic size so it never upscales).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObjectFit {
    #[default]
    Fill,
    Contain,
    Cover,
    ScaleDown,
    None,
}

/// `vertical-align` positions for table-cell content. `baseline` (and the
/// text-level values like sub/super, which do not apply to cells) map to
/// `Top` as an approximation: real per-row baseline alignment needs shared
/// ascent metrics across the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
}

/// `clear`: which floated side(s) an element moves below. Ends the float
/// zone in `dom::build_children_with_float_zone` (the clearfix idiom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    Left,
    Right,
    Both,
}

/// `line-height`: `normal` (a font-relative default), a unitless multiple of
/// font-size, or an absolute pixel length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    Normal,
    /// Unitless number. Unlike length/percentage values, this remains a ratio
    /// when inherited and therefore scales with each descendant's font size.
    Ratio(f32),
    Px(f32),
    /// A specified length or percentage awaiting computed-value resolution.
    /// It becomes `Px` on the declaring element before inheritance.
    Relative(Dimension),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// `list-style-type` values we render a marker for. `Decimal` numbers the
/// item by its position among sibling list items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStyle {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
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
    s.box_sizing = match style.box_sizing {
        BoxSizing::ContentBox => taffy::BoxSizing::ContentBox,
        BoxSizing::BorderBox => taffy::BoxSizing::BorderBox,
        // Programmatic LayoutStyle users do not have a DOM inheritance pass;
        // fall back to the property's CSS initial value in that case.
        BoxSizing::Inherit => taffy::BoxSizing::ContentBox,
    };

    // A block box with centered/right inline content needs a flex-column
    // stand-in because taffy's native block algorithm has no line alignment.
    // `text_align` is separate from real flex/grid `align-items`, so a
    // text-align declaration never changes how flex children are sized.
    let promote_for_alignment = style.display == Display::Block
        && matches!(style.text_align, Some(taffy::AlignItems::CENTER) | Some(taffy::AlignItems::FLEX_END));

    s.display = match style.display {
        Display::Block if promote_for_alignment => taffy::style::Display::Flex,
        Display::Block => taffy::style::Display::Block,
        Display::Flex => taffy::style::Display::Flex,
        Display::Grid => taffy::style::Display::Grid,
        Display::Inline => taffy::style::Display::Flex,
        Display::None => taffy::style::Display::None,
    };
    if promote_for_alignment {
        s.flex_direction = taffy::FlexDirection::Column;
        s.align_items = style.text_align;
    }
    if let Some(fd) = style.flex_direction {
        s.flex_direction = fd;
    }
    if let Some(fw) = style.flex_wrap {
        s.flex_wrap = fw;
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
    // Tell taffy's own layout algorithm about `overflow: hidden`, not just our
    // paint-time clip rects: per spec, a flex/grid item's automatic minimum
    // size is content-based only when its overflow is `visible`, and `0`
    // otherwise. Without this, an explicit `height: 0; overflow: hidden`
    // element (the common collapsed-dropdown-panel pattern: hidden until a
    // sibling checkbox is checked) still grows to fit its content, since
    // taffy has no way to know it should not.
    if style.overflow_hidden {
        s.overflow = taffy::Point { x: taffy::style::Overflow::Hidden, y: taffy::style::Overflow::Hidden };
    }
    s.min_size = taffy::Size {
        width: dimension(style.min_width),
        height: dimension(style.min_height),
    };
    s.max_size = taffy::Size {
        width: dimension(style.max_width),
        height: dimension(style.max_height),
    };
    if let Some(ar) = style.aspect_ratio {
        if ar.is_finite() && ar > 0.0 {
            s.aspect_ratio = Some(ar);
        }
    }
    if style.display != Display::Block {
        if let Some(ai) = style.align_items {
            s.align_items = Some(ai);
        }
    } else if !promote_for_alignment {
        // `align-items` has no effect on a block formatting context.
        s.align_items = None;
    }
    s.justify_items = style.justify_items;
    s.align_self = style.align_self;
    s.justify_self = style.justify_self;
    s.align_content = style.align_content;
    if let Some(jc) = style.justify_content {
        s.justify_content = Some(jc);
    }
    if let Some(fg) = style.flex_grow {
        s.flex_grow = fg;
    }
    if let Some(fs) = style.flex_shrink {
        s.flex_shrink = fs;
    }
    if style.flex_basis != Dimension::Auto {
        s.flex_basis = dimension(style.flex_basis);
    }

    // Grid container tracks and gaps. Numeric repeat() values are expanded
    // during parsing, while auto-fill/auto-fit remain native taffy repetition
    // components so their count can use the final container size. The 0.7-era
    // fr->Auto row workaround (which stopped `minmax(0,1fr)` image rows from
    // collapsing to a sliver) is gone: taffy 0.12 treats an in-flow child's
    // vertical available space as indefinite, so fr rows of an auto-height grid
    // size to their content the way real CSS does.
    if style.display == Display::Grid {
        if !style.grid_template_columns.is_empty() {
            s.grid_template_columns = style.grid_template_columns.clone();
        }
        if !style.grid_template_rows.is_empty() {
            s.grid_template_rows = style.grid_template_rows.clone();
        }
        if let Some(flow) = style.grid_auto_flow {
            s.grid_auto_flow = flow;
        }
    }
    let cg = style.column_gap.unwrap_or(0.0);
    let rg = style.row_gap.unwrap_or(0.0);
    s.gap = taffy::Size {
        width: taffy::style::LengthPercentage::length(cg),
        height: taffy::style::LengthPercentage::length(rg),
    };

    // Grid item placement (resolved from grid-area names or explicit lines).
    // `GridPlacement` is no longer `Copy` in taffy 0.12 (it can carry a named
    // line), so clone out of the borrowed style.
    if let Some(gc) = &style.grid_column {
        s.grid_column = gc.clone();
    }
    if let Some(gr) = &style.grid_row {
        s.grid_row = gr.clone();
    }

    // Positioning. Absolute/fixed take the box out of flow.
    if let Some(pos) = style.position {
        s.position = pos;
        s.inset = taffy::Rect {
            top: inset_lpa(style.inset[0]),
            right: inset_lpa(style.inset[1]),
            bottom: inset_lpa(style.inset[2]),
            left: inset_lpa(style.inset[3]),
        };
    }

    s.margin = rect_auto(style.margin, style.margin_auto);
    s.padding = rect_lp(style.padding);
    s.border = rect_lp(style.border);
    s
}

fn inset_lpa(v: Option<Dimension>) -> taffy::style::LengthPercentageAuto {
    match v {
        Some(Dimension::Px(px)) => taffy::style::LengthPercentageAuto::length(px),
        Some(Dimension::Percent(p)) => taffy::style::LengthPercentageAuto::percent(p),
        // Relative units are resolved to Px before layout; unresolved leftovers
        // and `auto`/absent both map to Auto.
        _ => taffy::style::LengthPercentageAuto::auto(),
    }
}

fn dimension(v: Dimension) -> taffy::style::Dimension {
    match v {
        Dimension::Px(px) => taffy::style::Dimension::length(px),
        Dimension::Percent(p) => taffy::style::Dimension::percent(p),
        Dimension::Auto => taffy::style::Dimension::auto(),
        // Relative units are resolved to Px before layout; if one slips
        // through unresolved, fall back to its raw magnitude (em/rem ~16px)
        // rather than panicking.
        Dimension::Em(v) | Dimension::Rem(v) => taffy::style::Dimension::length(v * 16.0),
        Dimension::Ex(v) => taffy::style::Dimension::length(v * 16.0 * 0.528_320_3),
        Dimension::Vw(v) | Dimension::Vh(v) | Dimension::Vmin(v) | Dimension::Vmax(v) => taffy::style::Dimension::length(v),
    }
}

fn rect_lp(e: Edges) -> taffy::Rect<taffy::style::LengthPercentage> {
    taffy::Rect {
        top: taffy::style::LengthPercentage::length(e.top),
        right: taffy::style::LengthPercentage::length(e.right),
        bottom: taffy::style::LengthPercentage::length(e.bottom),
        left: taffy::style::LengthPercentage::length(e.left),
    }
}

fn rect_auto(e: Edges, auto: [bool; 4]) -> taffy::Rect<taffy::style::LengthPercentageAuto> {
    let side = |value, is_auto| {
        if is_auto {
            taffy::style::LengthPercentageAuto::auto()
        } else {
            taffy::style::LengthPercentageAuto::length(value)
        }
    };
    taffy::Rect {
        top: side(e.top, auto[0]),
        right: side(e.right, auto[1]),
        bottom: side(e.bottom, auto[2]),
        left: side(e.left, auto[3]),
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
    fn block_auto_margins_absorb_horizontal_free_space() {
        let centered = LayoutStyle {
            display: Display::Block,
            width: Dimension::Px(300.0),
            height: Dimension::Px(40.0),
            margin_auto: [false, true, false, true],
            ..Default::default()
        };
        let pushed_end = LayoutStyle {
            display: Display::Block,
            width: Dimension::Px(200.0),
            height: Dimension::Px(40.0),
            margin: Edges { right: 50.0, ..Default::default() },
            margin_auto: [false, false, false, true],
            ..Default::default()
        };
        let root = LayoutNode {
            style: make_box(Display::Block, 900.0, 200.0),
            text: None,
            children: vec![LayoutNode::leaf(centered), LayoutNode::leaf(pushed_end)],
        };
        let out = layout(&root, (900.0, 200.0));
        assert!((out.children[0].border_box.x - 300.0).abs() < 0.01);
        assert!((out.children[1].border_box.x - 650.0).abs() < 0.01);
    }

    #[test]
    fn negative_flex_margin_overlays_without_shifting_items() {
        let main = make_box(Display::Block, 900.0, 200.0);
        let sidebar = LayoutStyle {
            display: Display::Flex,
            width: Dimension::Px(225.0),
            height: Dimension::Px(180.0),
            margin: Edges {
                left: -900.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let root = LayoutNode {
            style: make_box(Display::Flex, 900.0, 220.0),
            text: None,
            children: vec![LayoutNode::leaf(main), LayoutNode::leaf(sidebar)],
        };
        let out = layout(&root, (900.0, 220.0));
        assert!(
            out.children[0].border_box.x.abs() < 0.01,
            "main shifted to {:?}",
            out.children[0].border_box
        );
        assert!(
            out.children[1].border_box.x.abs() < 0.01,
            "overlay shifted to {:?}",
            out.children[1].border_box
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
    fn padding_expands_content_box_but_not_border_box() {
        let content_box = LayoutNode {
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
        let content_out = layout(&content_box, (1000.0, 800.0));
        assert_eq!(content_out.border_box.width, 120.0);

        let mut border_box = content_box;
        border_box.style.box_sizing = BoxSizing::BorderBox;
        let border_out = layout(&border_box, (1000.0, 800.0));
        assert_eq!(border_out.border_box.width, 100.0);
    }
}
