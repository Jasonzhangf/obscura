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

use std::{collections::HashMap, sync::Arc};

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

#[derive(Clone)]
struct LoadedFamily {
    faces: Vec<LoadedFace>,
}

#[derive(Clone)]
struct LoadedFace {
    name: Arc<str>,
    min_weight: u16,
    max_weight: u16,
    shape_weight: u16,
    italic: bool,
    variable: bool,
}

#[derive(Clone)]
pub(crate) struct WebFont {
    pub data: Vec<u8>,
    pub family: Option<String>,
    pub weight: Option<(u16, u16)>,
    pub italic: Option<bool>,
}

fn resolve_loaded_font(
    fam: Option<&str>,
    requested_weight: u16,
    requested_italic: bool,
    loaded: &HashMap<String, LoadedFamily>,
) -> (Arc<str>, u16) {
    if let Some(stack) = fam {
        for token in stack.split(',') {
            let name = token
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .trim();
            if let Some(family) = loaded.get(&name.to_ascii_lowercase()) {
                let exact_style: Vec<_> = family
                    .faces
                    .iter()
                    .filter(|face| face.italic == requested_italic)
                    .collect();
                let candidates: Vec<_> = if exact_style.is_empty() {
                    family.faces.iter().collect()
                } else {
                    exact_style
                };
                if let Some(face) = candidates.iter().copied().find(|face| {
                    (face.min_weight..=face.max_weight).contains(&requested_weight)
                }) {
                    let weight = if face.variable {
                        requested_weight
                    } else {
                        face.shape_weight
                    };
                    return (Arc::clone(&face.name), weight);
                }
                let available: Vec<_> =
                    candidates.iter().map(|face| face.min_weight).collect();
                let matched = match_font_weight(requested_weight, &available);
                if let Some(face) = candidates
                    .into_iter()
                    .find(|face| face.min_weight == matched)
                {
                    return (Arc::clone(&face.name), face.shape_weight);
                }
            }
        }
    }
    let fallback = resolve_font_family(fam);
    let weights: &[u16] = &[400, 700];
    (Arc::from(fallback), match_font_weight(requested_weight, weights))
}

/// CSS Fonts' asymmetric missing-weight search. In particular, 600 selects
/// 700 (not 400) when a family only provides regular and bold faces.
fn match_font_weight(requested: u16, available: &[u16]) -> u16 {
    if available.contains(&requested) {
        return requested;
    }
    let mut weights = available.to_vec();
    weights.sort_unstable();
    weights.dedup();
    if weights.is_empty() {
        return requested;
    }
    if (400..=500).contains(&requested) {
        weights
            .iter()
            .copied()
            .filter(|weight| *weight >= requested && *weight <= 500)
            .min()
            .or_else(|| weights.iter().copied().filter(|weight| *weight < requested).max())
            .or_else(|| weights.iter().copied().filter(|weight| *weight > 500).min())
            .unwrap_or(requested)
    } else if requested < 400 {
        weights
            .iter()
            .copied()
            .filter(|weight| *weight <= requested)
            .max()
            .or_else(|| weights.iter().copied().filter(|weight| *weight > requested).min())
            .unwrap_or(requested)
    } else {
        weights
            .iter()
            .copied()
            .filter(|weight| *weight >= requested)
            .min()
            .or_else(|| weights.iter().copied().filter(|weight| *weight < requested).max())
            .unwrap_or(requested)
    }
}

fn face_is_variable(data: &[u8], face_index: u32) -> bool {
    let base = if data.get(..4) == Some(b"ttcf") {
        let offset = 12usize.saturating_add(face_index as usize * 4);
        data.get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0) as usize
    } else {
        0
    };
    let Some(count) = data
        .get(base + 4..base + 6)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
    else {
        return false;
    };
    (0..count as usize).any(|index| {
        let start = base + 12 + index * 16;
        data.get(start..start + 4) == Some(b"fvar")
    })
}
/// Resolve `line-height: normal` from the selected face's horizontal header.
///
/// Chromium's FreeType-backed Linux path grid-fits the ascent, descent, and
/// line gap independently before adding them. Multiplying their sum by the
/// font size (or rounding the final line height) is observably different at
/// fractional and small sizes: Liberation Sans at 9.333px is 10px in
/// Chromium, not 11px. Keep these metrics beside the embedded faces so normal
/// line boxes follow the same device-pixel rhythm without consulting host
/// fonts.
fn normal_line_height(font_size: f32, family: &str) -> f32 {
    let (ascent, descent, line_gap) = match family {
        SERIF_FAMILY => (1825.0, 443.0, 87.0),
        MONO_FAMILY => (1705.0, 615.0, 0.0),
        _ => (1854.0, 434.0, 67.0),
    };
    let scale = font_size / 2048.0;
    (ascent * scale).round() + (descent * scale).round() + (line_gap * scale).round()
}

/// Computed used line-height shared by shaped inline runs and forced-break
/// sentinels that cannot join a run.
pub(crate) fn used_line_height(style: &LayoutStyle) -> f32 {
    let font_size = style.font_size.unwrap_or(16.0);
    match style.line_height {
        Some(crate::LineHeight::Px(px)) => px,
        Some(crate::LineHeight::Ratio(ratio)) => font_size * ratio,
        Some(crate::LineHeight::Relative(relative)) => match relative {
            crate::Dimension::Percent(percent) => font_size * percent,
            dimension => match dimension.resolve(font_size, 16.0, 0.0, 0.0) {
                crate::Dimension::Px(px) => px,
                _ => font_size,
            },
        },
        None | Some(crate::LineHeight::Normal) => normal_line_height(
            font_size,
            resolve_font_family(style.font_family.as_deref()),
        ),
    }
}

/// Per-glyph flags carried through cosmic-text's `metadata` field. Bit zero is
/// the underline flag; the remaining bits encode an optional one-based index
/// into an [`InlineItem`]'s clip-text fills.
const META_UNDERLINE: usize = 1;
const META_FILL_SHIFT: usize = 1;

type ClipTextFill = (f32, Vec<([u8; 4], Option<f32>)>);

/// One inline formatting context: a shaped cosmic-text buffer plus where to
/// paint it (filled in after layout).
pub struct InlineItem {
    buffer: Buffer,
    /// Minimum block-size contributed by explicit `<br>` breaks. cosmic-text
    /// omits the final empty run for a trailing newline, while CSS still gives
    /// a break-only or consecutive-break line the parent's used line-height.
    forced_min_height: f32,
    /// Content-box top-left in viewport coordinates, set by `finalize`.
    origin: (f32, f32),
    /// Ancestor `overflow: hidden` clip, set by `finalize`.
    clip: Option<Rect>,
    /// Per-span `-webkit-background-clip: text` fills. Glyph metadata selects
    /// one entry, allowing an inline accent span to own a gradient without
    /// recoloring the rest of its heading.
    clip_fills: Vec<ClipTextFill>,
}

/// Owns the font set and shaping caches for one render pass, plus every
/// inline formatting context discovered while building the tree. Lives in
/// [`crate::DomLayout`] so paint can rasterize the shaped glyphs.
pub struct TextEngine {
    font_system: FontSystem,
    loaded_families: HashMap<String, LoadedFamily>,
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
        Self::new_with_fonts(&[])
    }

    pub fn new_with_fonts(fonts: &[Vec<u8>]) -> Self {
        let fonts: Vec<_> = fonts
            .iter()
            .map(|data| WebFont {
                data: data.clone(),
                family: None,
                weight: None,
                italic: None,
            })
            .collect();
        Self::new_with_web_fonts(&fonts)
    }

    pub(crate) fn new_with_web_fonts(fonts: &[WebFont]) -> Self {
        // Build a database from embedded and page-provided faces. Never call
        // load_system_fonts: a host's font set would make layout differ
        // machine to machine and add a multi-millisecond startup scan.
        let mut db = cosmic_text::fontdb::Database::new();
        let mut declarations = Vec::new();
        for bytes in [
            SANS_R, SANS_B, SANS_O, SANS_BO,
            SERIF_R, SERIF_B, SERIF_O, SERIF_BO,
            MONO_R, MONO_B, MONO_O, MONO_BO,
            FALLBACK,
        ] {
            for id in db.load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(bytes))) {
                declarations.push((id, None, None, None));
            }
        }
        for font in fonts {
            for id in db.load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(
                font.data.clone(),
            ))) {
                declarations.push((
                    id,
                    font.family.clone(),
                    font.weight,
                    font.italic,
                ));
            }
        }
        let mut loaded_families = HashMap::new();
        for (id, declared_family, declared_weight, declared_italic) in declarations {
            let Some(face) = db.face(id) else { continue };
            let names = face.families.clone();
            let internal_name = names
                .first()
                .map(|(name, _)| Arc::<str>::from(name.as_str()))
                .unwrap_or_else(|| Arc::from(FAMILY));
            let shape_weight = face.weight.0;
            let italic = declared_italic
                .unwrap_or(!matches!(face.style, cosmic_text::fontdb::Style::Normal));
            let variable = db
                .with_face_data(id, face_is_variable)
                .unwrap_or(false);
            let weight = declared_weight.unwrap_or((shape_weight, shape_weight));
            let declared_names: Vec<String> = declared_family
                .map(|name| vec![name])
                .unwrap_or_else(|| names.into_iter().map(|(name, _)| name).collect());
            for name in declared_names {
                let family = loaded_families
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| LoadedFamily { faces: Vec::new() });
                family.faces.push(LoadedFace {
                    name: Arc::clone(&internal_name),
                    min_weight: weight.0,
                    max_weight: weight.1,
                    shape_weight,
                    italic,
                    variable,
                });
            }
        }
        db.set_sans_serif_family(FAMILY);
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        TextEngine {
            font_system,
            loaded_families,
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
        let mut collector = Collector { last_was_space: true, clip_fills: Vec::new() };
        let (family, weight) = resolve_loaded_font(
            base.font_family.as_deref(),
            crate::style::used_font_weight(base),
            base.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let ctx = base_span_ctx(
            base,
            family,
            weight,
            &mut collector,
        );
        let mut spans: Vec<(String, SpanAttrs)> = Vec::new();
        collect_spans(
            tree,
            id,
            styles,
            ctx,
            &mut spans,
            &mut collector,
            &self.loaded_families,
        );
        self.push_shaped_item(base, spans, collector.clip_fills)
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
        let mut collector = Collector { last_was_space: true, clip_fills: Vec::new() };
        let (family, weight) = resolve_loaded_font(
            base.font_family.as_deref(),
            crate::style::used_font_weight(base),
            base.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let ctx = base_span_ctx(
            base,
            family,
            weight,
            &mut collector,
        );
        let mut spans: Vec<(String, SpanAttrs)> = Vec::new();
        for &cid in run {
            collect_node_spans(
                tree,
                cid,
                styles,
                ctx.clone(),
                &mut spans,
                &mut collector,
                &self.loaded_families,
            );
        }
        self.push_shaped_item(base, spans, collector.clip_fills)
    }

    /// Shape generated text that owns a positioned pseudo box.
    ///
    /// Positioned `::before`/`::after` boxes do not participate in the taffy
    /// tree, but their text still uses the same authored webfonts, variable
    /// weight selection, transformations, and glyph rasterizer as an ordinary
    /// inline formatting context. The caller measures/finalizes/paints the
    /// returned item immediately against the pseudo's resolved content box.
    pub(crate) fn push_generated_text(
        &mut self,
        text: &str,
        style: &LayoutStyle,
    ) -> Option<usize> {
        let mut collector = Collector {
            last_was_space: true,
            clip_fills: Vec::new(),
        };
        let (family, weight) = resolve_loaded_font(
            style.font_family.as_deref(),
            crate::style::used_font_weight(style),
            style.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let context = base_span_ctx(style, family, weight, &mut collector);
        let attrs = SpanAttrs {
            weight: context.weight,
            italic: context.italic,
            underline: context.underline,
            color: context.color,
            family: context.family,
            clip_fill: context.clip_fill,
        };
        let mut spans = Vec::new();
        push_text(
            text,
            context.transform,
            &attrs,
            &mut spans,
            &mut collector,
        );
        self.push_shaped_item(style, spans, collector.clip_fills)
    }

    /// Shared tail of [`try_build`] / [`try_build_run`]: shape the collected
    /// spans into a cosmic-text buffer under `base`'s font metrics and
    /// alignment, and store it as a new inline item.
    fn push_shaped_item(
        &mut self,
        base: &LayoutStyle,
        mut spans: Vec<(String, SpanAttrs)>,
        clip_fills: Vec<ClipTextFill>,
    ) -> Option<usize> {
        // Trim a single trailing space so it does not widen the last line.
        if let Some((text, _)) = spans.last_mut() {
            if text.ends_with(' ') {
                text.pop();
            }
        }
        if spans
            .iter()
            .all(|(text, _)| text.trim().is_empty() && !text.contains('\n'))
        {
            return None;
        }

        let base_size = base.font_size.unwrap_or(16.0);
        // Explicit line-height stays fractional. `normal` is derived from the
        // embedded face metrics with the same per-component pixel fitting as
        // Chromium's Linux font path.
        let line_h = used_line_height(base);
        let mut forced_breaks = 0usize;
        let mut visible_after_last_break = false;
        for (text, _) in &spans {
            for ch in text.chars() {
                if ch == '\n' {
                    forced_breaks += 1;
                    visible_after_last_break = false;
                } else if !ch.is_whitespace() {
                    visible_after_last_break = true;
                }
            }
        }
        let forced_lines =
            forced_breaks + usize::from(forced_breaks > 0 && visible_after_last_break);
        let forced_min_height = forced_lines as f32 * line_h.max(1.0);
        // cosmic-text asserts (an uncatchable process abort) if font size OR
        // line height is 0. `font-size:0` is a common whitespace-collapse trick
        // and drives both to 0, so floor both at 1px here. The glyphs stay
        // ~invisible, matching the intent, and one page can never abort a worker.
        let cosmic_size = base_size.max(1.0);
        let metrics = Metrics::new(cosmic_size, line_h.max(1.0));
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
        self.items.push(InlineItem {
            buffer,
            forced_min_height,
            origin: (0.0, 0.0),
            clip: None,
            clip_fills,
        });
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
        let (width, height) = buffer_size(&item.buffer);
        (width, height.max(item.forced_min_height))
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
    weight: u16,
    italic: bool,
    underline: bool,
    color: [u8; 4],
    family: Arc<str>,
    clip_fill: Option<usize>,
}

impl SpanAttrs {
    fn to_attrs(&self) -> Attrs<'_> {
        let mut a = Attrs::new().family(Family::Name(self.family.as_ref()));
        a = a.weight(Weight(self.weight));
        a = a.style(if self.italic { Style::Italic } else { Style::Normal });
        // Clip-text glyphs must be shaped with an opaque fill so their coverage
        // reaches paint; the real gradient is selected through metadata.
        let color = if self.clip_fill.is_some() { [255, 255, 255, 255] } else { self.color };
        a = a.color(Color::rgba(color[0], color[1], color[2], color[3]));
        // Underline and the optional fill index share the per-glyph metadata.
        let fill = self.clip_fill.map_or(0, |index| (index + 1) << META_FILL_SHIFT);
        a = a.metadata(fill | usize::from(self.underline));
        a
    }
}

/// Inherited inline context threaded down the subtree while collecting spans.
#[derive(Clone)]
struct SpanCtx {
    color: [u8; 4],
    weight: u16,
    italic: bool,
    underline: bool,
    transform: TextTransform,
    family: Arc<str>,
    clip_fill: Option<usize>,
}

struct Collector {
    last_was_space: bool,
    clip_fills: Vec<ClipTextFill>,
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
fn base_span_ctx(
    base: &LayoutStyle,
    family: Arc<str>,
    weight: u16,
    collector: &mut Collector,
) -> SpanCtx {
    let clip_fill = clip_text_fill(base).map(|fill| {
        let index = collector.clip_fills.len();
        collector.clip_fills.push(fill);
        index
    });
    SpanCtx {
        color: base.color.unwrap_or([0, 0, 0, 255]),
        weight,
        italic: base.font_style_italic.unwrap_or(false),
        underline: base.underline.unwrap_or(false),
        transform: base.text_transform.unwrap_or(TextTransform::None),
        family,
        clip_fill,
    }
}

fn collect_spans(
    tree: &DomTree,
    id: NodeId,
    styles: &std::collections::HashMap<NodeId, LayoutStyle>,
    ctx: SpanCtx,
    out: &mut Vec<(String, SpanAttrs)>,
    c: &mut Collector,
    loaded_families: &HashMap<String, LoadedFamily>,
) {
    for cid in tree.children(id) {
        collect_node_spans(
            tree,
            cid,
            styles,
            ctx.clone(),
            out,
            c,
            loaded_families,
        );
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
    loaded_families: &HashMap<String, LoadedFamily>,
) {
    let Some(node) = tree.get_node(cid) else { return };
    match &node.data {
        obscura_dom::tree::NodeData::Text { contents } => {
            let attrs = SpanAttrs {
                weight: ctx.weight,
                italic: ctx.italic,
                underline: ctx.underline,
                color: ctx.color,
                family: Arc::clone(&ctx.family),
                clip_fill: ctx.clip_fill,
            };
            push_text(contents, ctx.transform, &attrs, out, c);
        }
        _ => {
            let Some(elem) = node.as_element() else { return };
            let style = styles.get(&cid);
            if style.map(|s| s.display == Display::None).unwrap_or(false) {
                return;
            }
            if elem.local.as_ref() == "br" {
                out.push(("\n".to_string(), SpanAttrs {
                    weight: ctx.weight,
                    italic: ctx.italic,
                    underline: ctx.underline,
                    color: ctx.color,
                    family: Arc::clone(&ctx.family),
                    clip_fill: ctx.clip_fill,
                }));
                c.last_was_space = true;
                return;
            }
            let own_clip_fill = style.and_then(clip_text_fill).map(|fill| {
                let index = c.clip_fills.len();
                c.clip_fills.push(fill);
                index
            });
            let color = style.and_then(|s| s.color).unwrap_or(ctx.color);
            // A descendant with its own clip-text background replaces the
            // inherited fill. Transparent descendants otherwise continue an
            // ancestor's fill; an opaque text color paints normally.
            let clip_fill = own_clip_fill.or_else(|| {
                if color[3] == 0 { ctx.clip_fill } else { None }
            });
            let requested_weight = style
                .map(crate::style::used_font_weight)
                .unwrap_or(ctx.weight);
            let (family, weight) = style
                .and_then(|style| style.font_family.as_deref())
                .map(|family| {
                    resolve_loaded_font(
                        Some(family),
                        requested_weight,
                        style
                            .and_then(|style| style.font_style_italic)
                            .unwrap_or(ctx.italic),
                        loaded_families,
                    )
                })
                .unwrap_or_else(|| (Arc::clone(&ctx.family), requested_weight));
            let child = SpanCtx {
                color,
                weight,
                italic: ctx.italic || style.and_then(|s| s.font_style_italic).unwrap_or(false),
                // Underline propagates in: an ancestor's underline covers
                // descendant text; an element only sets its own via CSS.
                underline: ctx.underline || style.and_then(|s| s.underline).unwrap_or(false),
                transform: style.and_then(|s| s.text_transform).unwrap_or(ctx.transform),
                family,
                clip_fill,
            };
            collect_spans(
                tree,
                cid,
                styles,
                child,
                out,
                c,
                loaded_families,
            );
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
    (w.ceil(), h)
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
        || style.is_inline_block
        || (style.display == Display::Flex
            && style.flex_direction == Some(taffy::FlexDirection::Column));
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
                *has_text = true;
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
        let mut fill_bounds: Vec<Option<(f32, f32, f32, f32)>> =
            vec![None; item.clip_fills.len()];
        for run in item.buffer.layout_runs() {
            let base_y = run.line_y;
            let mut seg: Option<(f32, f32, f32, [u8; 4])> = None; // x0, x1, font_size, color
            for g in run.glyphs {
                let underlined = g.metadata & META_UNDERLINE != 0;
                if let Some(fill_index) = (g.metadata >> META_FILL_SHIFT).checked_sub(1) {
                    if let Some(bounds) = fill_bounds.get_mut(fill_index) {
                        let glyph_bounds = (
                            g.x,
                            run.line_top,
                            g.x + g.w,
                            run.line_top + run.line_height,
                        );
                        *bounds = Some(match *bounds {
                            Some((x0, y0, x1, y1)) => (
                                x0.min(glyph_bounds.0),
                                y0.min(glyph_bounds.1),
                                x1.max(glyph_bounds.2),
                                y1.max(glyph_bounds.3),
                            ),
                            None => glyph_bounds,
                        });
                    }
                }
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
        // `-webkit-background-clip: text`: rasterize each glyph while its
        // metadata is still available, then sample that span's background
        // gradient across the span's own shaped bounds. Buffer::draw omits
        // metadata from its callback, so using it here would force one fill
        // over the entire heading and lose inline accent gradients.
        let clip_fills = item.clip_fills.clone();
        let pixels = pixmap.pixels_mut();
        for run in item.buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let glyph_color = glyph.color_opt.unwrap_or(default);
                let fill_index = (glyph.metadata >> META_FILL_SHIFT).checked_sub(1);
                swash.with_pixels(
                    font_system,
                    physical.cache_key,
                    glyph_color,
                    |x, y, color| {
                    let a = color.a() as u32;
                    if a == 0 {
                        return;
                    }
                    let gx = physical.x + x;
                    let gy = run.line_y as i32 + physical.y + y;
                    let (r, g, b) = fill_index
                        .and_then(|index| {
                            let fill = clip_fills.get(index)?;
                            let (x0, y0, x1, y1) = fill_bounds.get(index).copied().flatten()?;
                            let sampled = sample_gradient(
                                fill,
                                gx as f32 + 0.5 - x0,
                                gy as f32 + 0.5 - y0,
                                x1 - x0,
                                y1 - y0,
                            );
                            Some((sampled[0], sampled[1], sampled[2]))
                        })
                        .unwrap_or_else(|| (color.r(), color.g(), color.b()));
                    let px = ox as i32 + gx;
                    let py = oy as i32 + gy;
                    if let Some((cx0, cy0, cx1, cy1)) = clip_bounds {
                        if (px as f32) < cx0 || (px as f32) >= cx1 || (py as f32) < cy0 || (py as f32) >= cy1 {
                            return;
                        }
                    }
                    if px < 0 || px >= pw || py < 0 || py >= ph {
                        return;
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
                        return;
                    }
                    let out_r = sr + (dst.red() as u32 * inv / 255);
                    let out_g = sg + (dst.green() as u32 * inv / 255);
                    let out_b = sb + (dst.blue() as u32 * inv / 255);
                    pixels[idx] = tiny_skia::PremultipliedColorU8::from_rgba(out_r as u8, out_g as u8, out_b as u8, out_a as u8)
                        .unwrap_or(dst);
                    },
                );
            }
        }

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
    fn normal_line_height_grid_fits_each_face_metric() {
        // Values measured from Chromium 145 using the same bundled Linux
        // platform faces. Small fractional sizes expose the difference from
        // rounding a single 1.15 multiplier.
        assert_eq!(normal_line_height(9.3333, FAMILY), 10.0);
        assert_eq!(normal_line_height(12.0, FAMILY), 14.0);
        assert_eq!(normal_line_height(13.0, SERIF_FAMILY), 16.0);
        assert_eq!(normal_line_height(13.0, MONO_FAMILY), 15.0);
    }

    #[test]
    fn missing_font_weights_follow_css_search_order() {
        assert_eq!(match_font_weight(500, &[400, 700]), 400);
        assert_eq!(match_font_weight(600, &[400, 500, 700]), 700);
        assert_eq!(match_font_weight(300, &[100, 400, 700]), 100);
        assert_eq!(match_font_weight(800, &[400, 700]), 700);
        assert_eq!(match_font_weight(500, &[400, 500, 700]), 500);
    }

    #[test]
    fn declared_family_selects_a_face_with_a_different_internal_name() {
        let family = LoadedFamily {
            faces: vec![
                LoadedFace {
                    name: Arc::from("Poppins"),
                    min_weight: 400,
                    max_weight: 400,
                    shape_weight: 400,
                    italic: false,
                    variable: false,
                },
                LoadedFace {
                    name: Arc::from("Poppins Medium"),
                    min_weight: 500,
                    max_weight: 500,
                    shape_weight: 500,
                    italic: false,
                    variable: false,
                },
                LoadedFace {
                    name: Arc::from("Poppins"),
                    min_weight: 700,
                    max_weight: 700,
                    shape_weight: 700,
                    italic: false,
                    variable: false,
                },
            ],
        };
        let loaded = HashMap::from([("poppins".to_string(), family)]);
        let medium = resolve_loaded_font(Some("Poppins, sans-serif"), 500, false, &loaded);
        assert_eq!(medium.0.as_ref(), "Poppins Medium");
        assert_eq!(medium.1, 500);
        let semibold = resolve_loaded_font(Some("Poppins"), 600, false, &loaded);
        assert_eq!(semibold.0.as_ref(), "Poppins");
        assert_eq!(semibold.1, 700);
    }

    #[test]
    fn text_only_inline_block_keeps_an_internal_shaping_context() {
        let tree = obscura_dom::parse_html(
            "<span id='icon'>ligature_name</span>",
        );
        let icon = tree.get_element_by_id("icon").unwrap();
        let mut style = LayoutStyle::default();
        style.display = Display::Inline;
        style.is_inline_block = true;
        let styles = HashMap::from([(icon, style)]);

        assert!(
            is_pure_text_ifc(&tree, icon, &styles),
            "atomic inline participation must not disable shaping inside the box"
        );
    }

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
