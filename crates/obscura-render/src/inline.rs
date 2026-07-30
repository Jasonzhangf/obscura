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

use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, CacheKeyFlags, Color, Family, FontSystem, Metrics, Shaping,
    Style, SwashCache, SwashImage, Weight, Wrap,
};
use swash::scale::{image::Content as SwashContent, Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform, Vector};

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
}

#[derive(Clone)]
struct ResolvedFont {
    family: Arc<str>,
    shape_weight: u16,
    /// Authored `wght` coordinate for a ranged @font-face. Cosmic Text 0.12
    /// cannot carry this into shaping, so it is kept separately for raster.
    variation_weight: Option<u16>,
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
) -> ResolvedFont {
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
                if let Some(face) = candidates
                    .iter()
                    .copied()
                    .find(|face| (face.min_weight..=face.max_weight).contains(&requested_weight))
                {
                    // cosmic-text 0.12 only accepts the named family when the
                    // shaping weight exactly matches fontdb's weight for that
                    // file. A variable face commonly advertises `100 900` in
                    // CSS while fontdb records its default instance as 400.
                    // Passing the authored weight makes cosmic-text reject the
                    // right family and fall through to an unrelated exact-
                    // weight face. Select the file at its database weight;
                    // variable-axis shaping is not exposed by this version.
                    return ResolvedFont {
                        family: Arc::clone(&face.name),
                        shape_weight: face.shape_weight,
                        variation_weight: (face.min_weight != face.max_weight)
                            .then_some(requested_weight),
                    };
                }
                let available: Vec<_> =
                    candidates.iter().map(|face| face.min_weight).collect();
                let matched = match_font_weight(requested_weight, &available);
                if let Some(face) = candidates
                    .into_iter()
                    .find(|face| face.min_weight == matched)
                {
                    return ResolvedFont {
                        family: Arc::clone(&face.name),
                        shape_weight: face.shape_weight,
                        variation_weight: None,
                    };
                }
            }
        }
    }
    let fallback = resolve_font_family(fam);
    let weights: &[u16] = &[400, 700];
    ResolvedFont {
        family: Arc::from(fallback),
        shape_weight: match_font_weight(requested_weight, weights),
        variation_weight: None,
    }
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
const META_VARIATION_BITS: usize = 10;
const META_VARIATION_SHIFT: usize = usize::BITS as usize - META_VARIATION_BITS;
const META_VARIATION_MASK: usize =
    ((1usize << META_VARIATION_BITS) - 1) << META_VARIATION_SHIFT;
const META_FILL_MASK: usize = ((1usize << META_VARIATION_SHIFT) - 1) & !META_UNDERLINE;

fn metadata_fill(metadata: usize) -> Option<usize> {
    ((metadata & META_FILL_MASK) >> META_FILL_SHIFT).checked_sub(1)
}

fn metadata_variation_weight(metadata: usize) -> Option<u16> {
    let weight = ((metadata & META_VARIATION_MASK) >> META_VARIATION_SHIFT) as u16;
    (weight != 0).then_some(weight)
}

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
    variable_swash: VariableSwashCache,
    items: Vec<InlineItem>,
    replaced: Vec<ReplacedItem>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct VariableCacheKey {
    glyph: CacheKey,
    weight: u16,
}

/// Swash's ordinary cache key intentionally has no variation coordinates.
/// Keep variable instances in a separate cache so a 400 glyph can never be
/// reused for 700 and repeated paint remains O(1) after the first raster.
struct VariableSwashCache {
    context: ScaleContext,
    images: HashMap<VariableCacheKey, Option<SwashImage>>,
}

impl VariableSwashCache {
    fn new() -> Self {
        Self {
            context: ScaleContext::new(),
            images: HashMap::new(),
        }
    }

    fn with_pixels<F: FnMut(i32, i32, Color)>(
        &mut self,
        font_system: &mut FontSystem,
        cache_key: CacheKey,
        weight: u16,
        base: Color,
        mut f: F,
    ) {
        let key = VariableCacheKey {
            glyph: cache_key,
            weight,
        };
        let image = self.images.entry(key).or_insert_with(|| {
            let font = font_system.get_font(cache_key.font_id)?;
            let mut scaler = self
                .context
                .builder(font.as_swash())
                .size(f32::from_bits(cache_key.font_size_bits))
                .hint(true)
                .variations([("wght", weight as f32)])
                .build();
            let offset = Vector::new(cache_key.x_bin.as_float(), cache_key.y_bin.as_float());
            Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ])
            .format(Format::Alpha)
            .offset(offset)
            .transform(
                cache_key
                    .flags
                    .contains(CacheKeyFlags::FAKE_ITALIC)
                    .then(|| {
                        Transform::skew(
                            Angle::from_degrees(14.0),
                            Angle::from_degrees(0.0),
                        )
                    }),
            )
            .render(&mut scaler, cache_key.glyph_id)
        });
        let Some(image) = image else { return };
        let left = image.placement.left;
        let top = -image.placement.top;
        match image.content {
            SwashContent::Mask => {
                for (index, alpha) in image.data.iter().copied().enumerate() {
                    let x = index as i32 % image.placement.width as i32;
                    let y = index as i32 / image.placement.width as i32;
                    f(
                        left + x,
                        top + y,
                        Color(((alpha as u32) << 24) | base.0 & 0x00FF_FFFF),
                    );
                }
            }
            SwashContent::Color => {
                for (index, rgba) in image.data.chunks_exact(4).enumerate() {
                    let x = index as i32 % image.placement.width as i32;
                    let y = index as i32 / image.placement.width as i32;
                    f(
                        left + x,
                        top + y,
                        Color::rgba(rgba[0], rgba[1], rgba[2], rgba[3]),
                    );
                }
            }
            SwashContent::SubpixelMask => {}
        }
    }
}

const REPLACED_CONTEXT_BIT: usize = 1usize << (usize::BITS - 1);

#[derive(Clone, Copy)]
struct ReplacedItem {
    intrinsic_width: f32,
    preferred_width: Option<f32>,
    preferred_height: Option<f32>,
    preferred_ratio: f32,
    min_width: Option<f32>,
    min_height: Option<f32>,
    max_width: Option<f32>,
    max_height: Option<f32>,
    /// CSS Sizing's cyclic-percentage rule makes a proper replaced element's
    /// inline min-content contribution zero when its preferred or maximum
    /// inline size contains a percentage. The natural size still participates
    /// in max-content sizing and in the final definite layout.
    zero_inline_min_content: bool,
}

impl ReplacedItem {
    fn from_style(width: f32, height: f32, style: &LayoutStyle) -> Self {
        let px = |dimension| match dimension {
            Dimension::Px(value) => Some(value.max(0.0)),
            _ => None,
        };
        let expression_has_percentage = |index: usize| {
            style.size_expressions[index]
                .as_deref()
                .map_or(false, |expression| expression.contains('%'))
        };
        let intrinsic_ratio = if width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0
        {
            width / height
        } else {
            1.0
        };
        ReplacedItem {
            intrinsic_width: width,
            preferred_width: px(style.width),
            preferred_height: px(style.height),
            preferred_ratio: style
                .aspect_ratio
                .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
                .unwrap_or(intrinsic_ratio),
            min_width: px(style.min_width),
            min_height: px(style.min_height),
            max_width: px(style.max_width),
            max_height: px(style.max_height),
            zero_inline_min_content: matches!(style.width, Dimension::Percent(_))
                || matches!(style.max_width, Dimension::Percent(_))
                || expression_has_percentage(0)
                || expression_has_percentage(4),
        }
    }

    fn clamp(value: f32, min: Option<f32>, max: Option<f32>) -> f32 {
        // CSS sizing gives the minimum precedence when min > max.
        let value = max.map_or(value, |max| value.min(max));
        min.map_or(value, |min| value.max(min))
    }

    /// Apply the CSS 2.1 10.4/10.7 constraint table for a replaced element
    /// whose preferred width and height are both auto. Unlike independently
    /// clamping the two axes, this transfers a one-axis min/max constraint
    /// through the preferred aspect ratio whenever the constraints allow it.
    fn constrain_auto_size(self, tentative: taffy::Size<f32>) -> taffy::Size<f32> {
        let min_width = self.min_width.unwrap_or(0.0);
        let min_height = self.min_height.unwrap_or(0.0);
        let max_width = self.max_width.unwrap_or(f32::INFINITY).max(min_width);
        let max_height = self.max_height.unwrap_or(f32::INFINITY).max(min_height);
        let width = tentative.width;
        let height = tentative.height;

        let height_at_max_width = (max_width / self.preferred_ratio).max(min_height);
        let height_at_min_width = (min_width / self.preferred_ratio).min(max_height);
        let width_at_max_height = (max_height * self.preferred_ratio).max(min_width);
        let width_at_min_height = (min_height * self.preferred_ratio).min(max_width);

        let (width, height) = if width > max_width {
            if height > max_height {
                if max_width * height <= max_height * width {
                    (max_width, height_at_max_width)
                } else {
                    (width_at_max_height, max_height)
                }
            } else {
                (max_width, height_at_max_width)
            }
        } else if width < min_width {
            if height < min_height {
                if min_width * height <= min_height * width {
                    (width_at_min_height, min_height)
                } else {
                    (min_width, height_at_min_width)
                }
            } else {
                (min_width, height_at_min_width)
            }
        } else if height > max_height {
            (width_at_max_height, max_height)
        } else if height < min_height {
            (width_at_min_height, min_height)
        } else {
            (width, height)
        };

        taffy::Size { width, height }
    }

    fn size(self, known: taffy::Size<Option<f32>>) -> taffy::Size<f32> {
        let (width, height) = match (known.width, known.height) {
            (Some(width), Some(height)) => (width, height),
            (Some(width), None) => (
                width,
                width / self.preferred_ratio,
            ),
            (None, Some(height)) => (
                height * self.preferred_ratio,
                height,
            ),
            (None, None) => match (self.preferred_width, self.preferred_height) {
                (Some(width), Some(height)) => (width, height),
                (Some(width), None) => (width, width / self.preferred_ratio),
                (None, Some(height)) => (height * self.preferred_ratio, height),
                (None, None) => (
                    self.intrinsic_width,
                    self.intrinsic_width / self.preferred_ratio,
                ),
            },
        };
        let tentative = taffy::Size { width, height };
        if self.preferred_width.is_none()
            && self.preferred_height.is_none()
            && (known.width.is_none() || known.height.is_none())
        {
            self.constrain_auto_size(tentative)
        } else {
            taffy::Size {
                width: Self::clamp(width, self.min_width, self.max_width),
                height: Self::clamp(height, self.min_height, self.max_height),
            }
        }
    }
}

pub(crate) fn constrained_auto_replaced_size(
    width: f32,
    height: f32,
    style: &LayoutStyle,
) -> taffy::Size<f32> {
    ReplacedItem::from_style(width, height, style).size(taffy::Size {
        width: None,
        height: None,
    })
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
                });
            }
        }
        db.set_sans_serif_family(FAMILY);
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        TextEngine {
            font_system,
            loaded_families,
            swash: SwashCache::new(),
            variable_swash: VariableSwashCache::new(),
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

    #[cfg(test)]
    pub(crate) fn item_text(&self, idx: usize) -> String {
        self.items[idx]
            .buffer
            .lines
            .iter()
            .map(|line| line.text())
            .collect()
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
        let font = resolve_loaded_font(
            base.font_family.as_deref(),
            crate::style::used_font_weight(base),
            base.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let ctx = base_span_ctx(base, font, &mut collector);
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
        let font = resolve_loaded_font(
            base.font_family.as_deref(),
            crate::style::used_font_weight(base),
            base.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let ctx = base_span_ctx(base, font, &mut collector);
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
        let font = resolve_loaded_font(
            style.font_family.as_deref(),
            crate::style::used_font_weight(style),
            style.font_style_italic.unwrap_or(false),
            &self.loaded_families,
        );
        let context = base_span_ctx(style, font, &mut collector);
        let attrs = SpanAttrs {
            font_size: context.font_size,
            line_height: context.line_height,
            weight: context.weight,
            variation_weight: context.variation_weight,
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
            let replaced = self.replaced[idx & !REPLACED_CONTEXT_BIT];
            let mut size = replaced.size(known);
            if replaced.zero_inline_min_content
                && known.width.is_none()
                && matches!(available.width, taffy::AvailableSpace::MinContent)
            {
                size.width = 0.0;
            }
            return size;
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
    font_size: f32,
    line_height: f32,
    weight: u16,
    variation_weight: Option<u16>,
    italic: bool,
    underline: bool,
    color: [u8; 4],
    family: Arc<str>,
    clip_fill: Option<usize>,
}

impl SpanAttrs {
    fn to_attrs(&self) -> Attrs<'_> {
        let mut a = Attrs::new().family(Family::Name(self.family.as_ref()));
        // Inline descendants keep their own computed font metrics inside the
        // enclosing line box. Without per-span metrics, an `<a>`/`<span>`
        // with a relative font-size was shaped at the block container's size
        // even though cascade had resolved the descendant correctly.
        a = a.metrics(Metrics::new(
            self.font_size.max(1.0),
            self.line_height.max(1.0),
        ));
        a = a.weight(Weight(self.weight));
        a = a.style(if self.italic { Style::Italic } else { Style::Normal });
        // Clip-text glyphs must be shaped with an opaque fill so their coverage
        // reaches paint; the real gradient is selected through metadata.
        let color = if self.clip_fill.is_some() { [255, 255, 255, 255] } else { self.color };
        a = a.color(Color::rgba(color[0], color[1], color[2], color[3]));
        // Underline, the optional fill index, and a variable `wght` coordinate
        // share the per-glyph metadata.
        let fill = self.clip_fill.map_or(0, |index| (index + 1) << META_FILL_SHIFT);
        debug_assert_eq!(fill & !META_FILL_MASK, 0);
        let variation = self
            .variation_weight
            .map_or(0, |weight| usize::from(weight.clamp(1, 1000)) << META_VARIATION_SHIFT);
        a = a.metadata(fill | variation | usize::from(self.underline));
        a
    }
}

/// Inherited inline context threaded down the subtree while collecting spans.
#[derive(Clone)]
struct SpanCtx {
    font_size: f32,
    line_height: f32,
    color: [u8; 4],
    weight: u16,
    variation_weight: Option<u16>,
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
    font: ResolvedFont,
    collector: &mut Collector,
) -> SpanCtx {
    let clip_fill = clip_text_fill(base).map(|fill| {
        let index = collector.clip_fills.len();
        collector.clip_fills.push(fill);
        index
    });
    SpanCtx {
        font_size: base.font_size.unwrap_or(16.0),
        line_height: used_line_height(base),
        color: base.color.unwrap_or([0, 0, 0, 255]),
        weight: font.shape_weight,
        variation_weight: font.variation_weight,
        italic: base.font_style_italic.unwrap_or(false),
        underline: base.underline.unwrap_or(false),
        transform: base.text_transform.unwrap_or(TextTransform::None),
        family: font.family,
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
                font_size: ctx.font_size,
                line_height: ctx.line_height,
                weight: ctx.weight,
                variation_weight: ctx.variation_weight,
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
                    font_size: ctx.font_size,
                    line_height: ctx.line_height,
                    weight: ctx.weight,
                    variation_weight: ctx.variation_weight,
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
            let font = style
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
                .unwrap_or_else(|| ResolvedFont {
                    family: Arc::clone(&ctx.family),
                    shape_weight: if ctx.variation_weight.is_some() {
                        ctx.weight
                    } else {
                        requested_weight
                    },
                    variation_weight: ctx
                        .variation_weight
                        .map(|_| requested_weight.clamp(1, 1000)),
                });
            let child = SpanCtx {
                font_size: style
                    .and_then(|style| style.font_size)
                    .unwrap_or(ctx.font_size),
                line_height: style
                    .map(used_line_height)
                    .unwrap_or(ctx.line_height),
                color,
                weight: font.shape_weight,
                variation_weight: font.variation_weight,
                italic: ctx.italic || style.and_then(|s| s.font_style_italic).unwrap_or(false),
                // Underline propagates in: an ancestor's underline covers
                // descendant text; an element only sets its own via CSS.
                underline: ctx.underline || style.and_then(|s| s.underline).unwrap_or(false),
                transform: style.and_then(|s| s.text_transform).unwrap_or(ctx.transform),
                family: font.family,
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
fn is_pure_text_ifc(
    tree: &DomTree,
    id: NodeId,
    styles: &std::collections::HashMap<NodeId, LayoutStyle>,
) -> bool {
    let Some(style) = styles.get(&id) else {
        return false;
    };
    if style.before_pseudo.is_some() || style.after_pseudo.is_some() {
        return false;
    }
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
                && style.before_pseudo.is_none()
                && style.after_pseudo.is_none();
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
            variable_swash,
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
                if let Some(fill_index) = metadata_fill(g.metadata) {
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
                let fill_index = metadata_fill(glyph.metadata);
                let mut draw_pixel = |x, y, color: Color| {
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
                };
                if let Some(weight) = metadata_variation_weight(glyph.metadata) {
                    variable_swash.with_pixels(
                        font_system,
                        physical.cache_key,
                        weight,
                        glyph_color,
                        &mut draw_pixel,
                    );
                } else {
                    swash.with_pixels(
                        font_system,
                        physical.cache_key,
                        glyph_color,
                        &mut draw_pixel,
                    );
                }
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
    use base64::Engine as _;

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
    fn replaced_percentage_math_only_zeroes_inline_min_content() {
        for expression_index in [0, 4] {
            let mut engine = TextEngine::new();
            let mut style = LayoutStyle::default();
            style.size_expressions[expression_index] =
                Some("calc(100% - 1px)".to_string());
            let item = engine.register_replaced(800.0, 400.0, &style);
            let unknown = taffy::Size {
                width: None,
                height: None,
            };
            let min_content = engine.measure_taffy(
                item,
                unknown,
                taffy::Size {
                    width: taffy::AvailableSpace::MinContent,
                    height: taffy::AvailableSpace::MaxContent,
                },
            );
            let max_content = engine.measure_taffy(
                item,
                unknown,
                taffy::Size {
                    width: taffy::AvailableSpace::MaxContent,
                    height: taffy::AvailableSpace::MaxContent,
                },
            );
            let final_size = engine.measure_taffy(
                item,
                taffy::Size {
                    width: Some(300.0),
                    height: None,
                },
                taffy::Size {
                    width: taffy::AvailableSpace::Definite(300.0),
                    height: taffy::AvailableSpace::MaxContent,
                },
            );

            assert_eq!(min_content.width, 0.0);
            assert_eq!(max_content.width, 800.0);
            assert_eq!(max_content.height, 400.0);
            assert_eq!(final_size.width, 300.0);
            assert_eq!(final_size.height, 150.0);
        }
    }

    #[test]
    fn both_auto_replaced_constraints_transfer_through_preferred_ratio() {
        let unknown = taffy::Size {
            width: None,
            height: None,
        };
        let measure = |style: &LayoutStyle| {
            ReplacedItem::from_style(512.0, 323.0, style).size(unknown)
        };

        let max_height = LayoutStyle {
            max_height: Dimension::Px(128.0),
            ..Default::default()
        };
        let size = measure(&max_height);
        assert!((size.width - 202.89783).abs() < 0.001, "{size:?}");
        assert!((size.height - 128.0).abs() < 0.001, "{size:?}");

        let max_width = LayoutStyle {
            max_width: Dimension::Px(256.0),
            ..Default::default()
        };
        let size = measure(&max_width);
        assert!((size.width - 256.0).abs() < 0.001, "{size:?}");
        assert!((size.height - 161.5).abs() < 0.001, "{size:?}");

        let min_width = LayoutStyle {
            min_width: Dimension::Px(1024.0),
            ..Default::default()
        };
        let size = measure(&min_width);
        assert!((size.width - 1024.0).abs() < 0.001, "{size:?}");
        assert!((size.height - 646.0).abs() < 0.001, "{size:?}");

        let min_height = LayoutStyle {
            min_height: Dimension::Px(646.0),
            ..Default::default()
        };
        let size = measure(&min_height);
        assert!((size.width - 1024.0).abs() < 0.001, "{size:?}");
        assert!((size.height - 646.0).abs() < 0.001, "{size:?}");

        // A non-intrinsic authored ratio is the preferred ratio used for
        // transfer, rather than the decoded resource's natural ratio.
        let authored_ratio = LayoutStyle {
            max_height: Dimension::Px(128.0),
            aspect_ratio: Some(4.0),
            ..Default::default()
        };
        let size = measure(&authored_ratio);
        assert!((size.width - 512.0).abs() < 0.001, "{size:?}");
        assert!((size.height - 128.0).abs() < 0.001, "{size:?}");
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
                },
                LoadedFace {
                    name: Arc::from("Poppins Medium"),
                    min_weight: 500,
                    max_weight: 500,
                    shape_weight: 500,
                    italic: false,
                },
                LoadedFace {
                    name: Arc::from("Poppins"),
                    min_weight: 700,
                    max_weight: 700,
                    shape_weight: 700,
                    italic: false,
                },
            ],
        };
        let loaded = HashMap::from([("poppins".to_string(), family)]);
        let medium = resolve_loaded_font(Some("Poppins, sans-serif"), 500, false, &loaded);
        assert_eq!(medium.family.as_ref(), "Poppins Medium");
        assert_eq!(medium.shape_weight, 500);
        assert_eq!(medium.variation_weight, None);
        let semibold = resolve_loaded_font(Some("Poppins"), 600, false, &loaded);
        assert_eq!(semibold.family.as_ref(), "Poppins");
        assert_eq!(semibold.shape_weight, 700);
        assert_eq!(semibold.variation_weight, None);
    }

    #[test]
    fn ranged_face_shapes_at_its_font_database_weight() {
        let tree = obscura_dom::parse_html("<p id='copy'>Variable family</p>");
        let copy = tree.get_element_by_id("copy").unwrap();
        let mut style = LayoutStyle::default();
        style.display = Display::Block;
        style.font_family = Some("test variable".to_string());
        style.font_weight = Some("700".to_string());
        style.font_size = Some(32.0);
        let styles = HashMap::from([(copy, style)]);

        let mut engine = TextEngine::new();
        engine.loaded_families.insert(
            "test variable".to_string(),
            LoadedFamily {
                faces: vec![LoadedFace {
                    name: Arc::from(FAMILY),
                    min_weight: 100,
                    max_weight: 900,
                    shape_weight: 400,
                    italic: false,
                }],
            },
        );

        let item = engine.try_build(&tree, copy, &styles).unwrap();
        engine.measure(item, None);
        let font_id = engine.items[item]
            .buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|glyph| glyph.font_id)
            .unwrap();
        let face = engine.font_system.db().face(font_id).unwrap();
        assert!(
            face.families.iter().any(|(name, _)| name == FAMILY),
            "authored family must not fall through to an unrelated exact-weight face"
        );
        assert_eq!(face.weight.0, 400);
    }

    #[test]
    fn ranged_face_preserves_requested_weight_for_variable_raster() {
        let loaded = HashMap::from([(
            "inter".to_string(),
            LoadedFamily {
                faces: vec![LoadedFace {
                    name: Arc::from("Inter"),
                    min_weight: 100,
                    max_weight: 900,
                    shape_weight: 400,
                    italic: false,
                }],
            },
        )]);
        let resolved = resolve_loaded_font(Some("Inter, sans-serif"), 725, false, &loaded);
        assert_eq!(resolved.family.as_ref(), "Inter");
        assert_eq!(resolved.shape_weight, 400);
        assert_eq!(resolved.variation_weight, Some(725));
    }

    #[test]
    fn glyph_metadata_keeps_fill_and_variable_weight_independent() {
        let attrs = SpanAttrs {
            font_size: 16.0,
            line_height: 18.0,
            weight: 400,
            variation_weight: Some(725),
            italic: false,
            underline: true,
            color: [1, 2, 3, 255],
            family: Arc::from(FAMILY),
            clip_fill: Some(37),
        };
        let metadata = attrs.to_attrs().metadata;
        assert_ne!(metadata & META_UNDERLINE, 0);
        assert_eq!(metadata_fill(metadata), Some(37));
        assert_eq!(metadata_variation_weight(metadata), Some(725));
    }

    fn variable_font_fixture() -> Vec<u8> {
        let encoded: String = include_str!("../tests/fonts/obscura-vf-test.woff2.b64")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode variable-font fixture");
        wuff::decompress_woff2(&compressed).expect("decompress variable-font fixture")
    }

    fn render_variable_weight(weight: u16) -> ((f32, f32), u64, Vec<u16>) {
        let mut engine = TextEngine::new_with_web_fonts(&[WebFont {
            data: variable_font_fixture(),
            family: Some("Obscura VF Test".to_string()),
            weight: Some((100, 900)),
            italic: Some(false),
        }]);
        let tree = obscura_dom::parse_html("<p id='copy'>MMMMMMMM</p>");
        let copy = tree.get_element_by_id("copy").unwrap();
        let mut style = LayoutStyle::default();
        style.display = Display::Block;
        style.font_family = Some("Obscura VF Test".to_string());
        style.font_weight = Some(weight.to_string());
        style.font_size = Some(64.0);
        style.line_height = Some(crate::LineHeight::Px(80.0));
        let styles = HashMap::from([(copy, style)]);
        let item = engine.try_build(&tree, copy, &styles).unwrap();
        let geometry = engine.measure(item, Some(600.0));
        engine.finalize(item, (0.0, 0.0), 600.0, None);
        let mut pixmap = tiny_skia::Pixmap::new(600, 100).unwrap();
        engine.paint_item(item, &mut pixmap, (0.0, 0.0));
        let ink = pixmap
            .pixels()
            .iter()
            .map(|pixel| u64::from(pixel.alpha()))
            .sum();
        let mut cached_weights: Vec<_> = engine
            .variable_swash
            .images
            .keys()
            .map(|key| key.weight)
            .collect();
        cached_weights.sort_unstable();
        cached_weights.dedup();
        (geometry, ink, cached_weights)
    }

    #[test]
    fn variable_wght_changes_true_outline_without_changing_layout_geometry() {
        let (regular_geometry, regular_ink, regular_cache) = render_variable_weight(400);
        let (black_geometry, black_ink, black_cache) = render_variable_weight(900);
        assert_eq!(regular_geometry, black_geometry);
        assert!(
            black_ink > regular_ink * 13 / 10,
            "wght=900 should carry substantially more raster ink: {regular_ink} vs {black_ink}"
        );
        assert_eq!(regular_cache, vec![400]);
        assert_eq!(black_cache, vec![900]);
    }

    #[test]
    fn variable_glyph_cache_keys_include_weight_axis() {
        let mut engine = TextEngine::new_with_web_fonts(&[WebFont {
            data: variable_font_fixture(),
            family: Some("Obscura VF Test".to_string()),
            weight: Some((100, 900)),
            italic: Some(false),
        }]);
        let tree = obscura_dom::parse_html(
            "<p id='copy'><span id='regular'>M</span><span id='black'>M</span></p>",
        );
        let copy = tree.get_element_by_id("copy").unwrap();
        let regular = tree.get_element_by_id("regular").unwrap();
        let black = tree.get_element_by_id("black").unwrap();
        let mut base = LayoutStyle::default();
        base.display = Display::Block;
        base.font_family = Some("Obscura VF Test".to_string());
        base.font_size = Some(64.0);
        base.line_height = Some(crate::LineHeight::Px(80.0));
        let mut regular_style = base.clone();
        regular_style.display = Display::Inline;
        regular_style.font_weight = Some("400".to_string());
        let mut black_style = regular_style.clone();
        black_style.font_weight = Some("900".to_string());
        let styles = HashMap::from([
            (copy, base),
            (regular, regular_style),
            (black, black_style),
        ]);
        let item = engine.try_build(&tree, copy, &styles).unwrap();
        engine.measure(item, Some(200.0));
        engine.finalize(item, (0.0, 0.0), 200.0, None);
        let mut pixmap = tiny_skia::Pixmap::new(200, 100).unwrap();
        engine.paint_item(item, &mut pixmap, (0.0, 0.0));
        let weights: std::collections::HashSet<_> = engine
            .variable_swash
            .images
            .keys()
            .map(|key| key.weight)
            .collect();
        assert_eq!(weights, std::collections::HashSet::from([400, 900]));
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
    fn inline_descendant_keeps_its_computed_font_metrics() {
        let tree = obscura_dom::parse_html(
            r#"<style>
                #copy { font-size:16px; line-height:20px }
                #big { font-size:2em; line-height:1.5 }
            </style>
            <p id="copy">small <a id="big">large</a></p>"#,
        );
        let copy = tree.get_element_by_id("copy").unwrap();
        let big = tree.get_element_by_id("big").unwrap();
        let laid = crate::dom::layout_dom(&tree, (500.0, 200.0));

        assert_eq!(laid.styles[&big].font_size, Some(32.0));
        let item = laid.ifc_items[&copy];
        let glyph_sizes = laid.text_engine.items[item]
            .buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.font_size))
            .collect::<Vec<_>>();
        assert!(
            glyph_sizes.iter().any(|size| (*size - 16.0).abs() < 0.01),
            "base text should shape at 16px: {glyph_sizes:?}"
        );
        assert!(
            glyph_sizes.iter().any(|size| (*size - 32.0).abs() < 0.01),
            "inline descendant should shape at 32px: {glyph_sizes:?}"
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
