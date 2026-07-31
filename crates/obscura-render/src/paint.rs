//! Paint: rasterize the laid-out DOM into a [`tiny_skia::Pixmap`].
//!
//! Phase 5a. Fills each element's border box with its background color over a
//! white page. Text rendering arrives with the text step; borders and images
//! are later enhancements. Pure Rust (tiny-skia, CPU), deterministic, no system
//! dependencies, so a screenshot is reproducible across hosts.

use obscura_dom::tree::DomTree;
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, PathBuilder, Pattern,
    Pixmap, Point, RadialGradient, Rect, SpreadMode, Transform,
};

static FONT_BYTES: &[u8] = include_bytes!("../assets/liberation-sans.ttf");
static SYSTEM_FONT_BYTES: &[u8] = include_bytes!("../assets/dejavu-sans.ttf");
static SERIF_FONT_BYTES: &[u8] = include_bytes!("../assets/liberation-serif.ttf");
static MONO_FONT_BYTES: &[u8] = include_bytes!("../assets/liberation-mono.ttf");
static FONT_BOLD_BYTES: &[u8] = include_bytes!("../assets/liberation-sans-bold.ttf");
static FONT_OBLIQUE_BYTES: &[u8] = include_bytes!("../assets/liberation-sans-oblique.ttf");
static FONT_BOLD_OBLIQUE_BYTES: &[u8] = include_bytes!("../assets/liberation-sans-boldoblique.ttf");

use crate::dom::layout_dom_with_web_fonts;

const DEFAULT_RESOURCE_CACHE_ENTRIES: usize = 512;
const DEFAULT_RESOURCE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MISSING_RESOURCE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
/// Exact formats the renderer can decode. Do not use `image/*` or `*/*` here:
/// either wildcard permits a content-negotiating server to choose AVIF,
/// JPEG-XL, or another format that this build cannot rasterize.
const IMAGE_ACCEPT: &str = "image/webp,image/apng,image/svg+xml,image/png,image/jpeg,image/gif,image/bmp,image/x-icon,image/vnd.microsoft.icon";

/// Synchronous byte loader used by [`RenderResourceCache`]. The default
/// implementation uses Obscura's pooled image agent; tests and embedding
/// callers can provide a local loader without changing preparation or paint.
pub trait RenderResourceLoader {
    fn load(&mut self, url: &str) -> Option<Vec<u8>>;
}

/// One script-created `FontFace` registered with the document's
/// `FontFaceSet`. The JS runtime owns the registry and supplies this snapshot
/// alongside the DOM when preparing a render; keeping it out of the DOM avoids
/// exposing an implementation-only `<style>` element to page selectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicFontFace {
    pub family: String,
    pub source: String,
    pub style: String,
    pub weight: String,
    pub unicode_range: String,
}

impl<F> RenderResourceLoader for F
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    fn load(&mut self, url: &str) -> Option<Vec<u8>> {
        self(url)
    }
}

struct HttpResourceLoader;

impl RenderResourceLoader for HttpResourceLoader {
    fn load(&mut self, url: &str) -> Option<Vec<u8>> {
        http_get_bytes(url)
    }
}

enum CachedResource {
    Bytes(Arc<[u8]>),
    Missing(std::time::Instant),
}

/// Page-scoped raw resource bytes shared by layout preparation and repeated
/// paints. Entries are FIFO-bounded by both count and retained byte size.
/// Successful bytes use `Arc` so consumers never clone an image/font body.
pub struct RenderResourceCache {
    entries: HashMap<String, CachedResource>,
    order: VecDeque<String>,
    retained_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    loader: Box<dyn RenderResourceLoader>,
}

impl Default for RenderResourceCache {
    fn default() -> Self {
        Self::with_loader_and_limits(
            HttpResourceLoader,
            DEFAULT_RESOURCE_CACHE_ENTRIES,
            DEFAULT_RESOURCE_CACHE_BYTES,
        )
    }
}

impl RenderResourceCache {
    pub fn with_loader(loader: impl RenderResourceLoader + 'static) -> Self {
        Self::with_loader_and_limits(
            loader,
            DEFAULT_RESOURCE_CACHE_ENTRIES,
            DEFAULT_RESOURCE_CACHE_BYTES,
        )
    }

    pub fn with_loader_and_limits(
        loader: impl RenderResourceLoader + 'static,
        max_entries: usize,
        max_bytes: usize,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            max_entries,
            max_bytes,
            loader: Box::new(loader),
        }
    }

    pub fn retained_entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn retained_byte_len(&self) -> usize {
        self.retained_bytes
    }

    /// Resolve, fetch, and inspect one image through the exact byte cache used
    /// by layout and paint. The JS image-element lifecycle calls this narrow
    /// bridge so `complete`/`naturalWidth` and the eventual screenshot are
    /// driven by one resource outcome instead of issuing an independent fetch.
    ///
    /// This intentionally accepts a plain `src`, not a DOM node: responsive
    /// `picture`/`srcset` selection remains owned by `prepare_dom`.
    pub fn image_metadata(
        &mut self,
        src: &str,
        base_url: Option<&str>,
    ) -> Option<(String, f32, f32)> {
        let resolved_url = resolve_resource_url(src, base_url)?;
        let bytes = fetch_bytes(&resolved_url, None, self)?;
        let (width, height) = image_metadata_from_bytes(&bytes)?;
        Some((resolved_url, width, height))
    }

    /// Inspect an image only when its renderer-cache outcome is already known.
    /// `None` means no live cache entry (the caller may queue a load);
    /// `Some(None)` means a retained load/decode failure; `Some(Some(...))`
    /// means success. `data:` sources have no network entry and are cheap to
    /// inspect directly.
    pub fn cached_image_metadata(
        &self,
        src: &str,
        base_url: Option<&str>,
    ) -> Option<Option<(String, f32, f32)>> {
        let resolved_url = resolve_resource_url(src, base_url)?;
        if resolved_url.starts_with("data:") {
            let mut scratch = RenderResourceCache::with_loader_and_limits(
                |_url: &str| None,
                0,
                0,
            );
            return Some(
                fetch_bytes(&resolved_url, None, &mut scratch)
                    .and_then(|bytes| image_metadata_from_bytes(&bytes))
                    .map(|(width, height)| (resolved_url, width, height)),
            );
        }
        match self.entries.get(&resolved_url) {
            Some(CachedResource::Bytes(bytes)) => Some(
                image_metadata_from_bytes(bytes)
                    .map(|(width, height)| (resolved_url, width, height)),
            ),
            Some(CachedResource::Missing(at))
                if at.elapsed() < MISSING_RESOURCE_RETRY_AFTER =>
            {
                Some(None)
            }
            _ => None,
        }
    }

    /// Select and inspect the resource for one live `<img>` using the same
    /// `picture`/`srcset`/`sizes` algorithm as `collect_image_intrinsics`.
    /// Dimensions are returned in CSS pixels after candidate-density scaling.
    /// A selected URL with `None` dimensions is an authoritative load/decode
    /// failure.
    pub fn image_element_metadata(
        &mut self,
        tree: &DomTree,
        id: obscura_dom::tree::NodeId,
        viewport: (f32, f32),
        base_url: Option<&str>,
    ) -> Option<(String, f32, Option<(f32, f32)>)> {
        let (src, density) = resolve_img_url(tree, id, viewport)?;
        let resolved_url = resolve_resource_url(&src, base_url).unwrap_or(src);
        let dimensions = fetch_bytes(&resolved_url, None, self)
            .and_then(|bytes| image_metadata_from_bytes(&bytes))
            .map(|(width, height)| (width / density, height / density));
        Some((resolved_url, density, dimensions))
    }

    /// Cache-only counterpart to [`Self::image_element_metadata`]. The boolean
    /// is false only when the selected candidate has no live cache outcome;
    /// callers may queue the loading form without ever blocking a getter.
    pub fn cached_image_element_metadata(
        &self,
        tree: &DomTree,
        id: obscura_dom::tree::NodeId,
        viewport: (f32, f32),
        base_url: Option<&str>,
    ) -> Option<(String, f32, bool, Option<(f32, f32)>)> {
        let (src, density) = resolve_img_url(tree, id, viewport)?;
        let resolved_url = resolve_resource_url(&src, base_url).unwrap_or(src);
        match self.cached_image_metadata(&resolved_url, None) {
            None => Some((resolved_url, density, false, None)),
            Some(dimensions) => Some((
                resolved_url,
                density,
                true,
                dimensions.map(|(_, width, height)| (width / density, height / density)),
            )),
        }
    }

    fn get_or_load(&mut self, url: &str) -> Option<Arc<[u8]>> {
        if let Some(entry) = self.entries.get(url) {
            match entry {
                CachedResource::Bytes(bytes) => return Some(Arc::clone(bytes)),
                CachedResource::Missing(at)
                    if at.elapsed() < MISSING_RESOURCE_RETRY_AFTER =>
                {
                    return None;
                }
                CachedResource::Missing(_) => {}
            }
        }
        self.remove(url);

        let loaded = self.loader.load(url).map(Arc::<[u8]>::from);
        match loaded {
            Some(bytes) => {
                self.insert_bytes(url.to_string(), Arc::clone(&bytes));
                Some(bytes)
            }
            None => {
                self.insert_missing(url.to_string());
                None
            }
        }
    }

    fn insert_bytes(&mut self, url: String, bytes: Arc<[u8]>) {
        if self.max_entries == 0 || bytes.len() > self.max_bytes {
            return;
        }
        while self.entries.len() >= self.max_entries
            || self.retained_bytes.saturating_add(bytes.len()) > self.max_bytes
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove_entry(&oldest);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes.len());
        self.order.push_back(url.clone());
        self.entries.insert(url, CachedResource::Bytes(bytes));
    }

    fn insert_missing(&mut self, url: String) {
        if self.max_entries == 0 {
            return;
        }
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove_entry(&oldest);
        }
        self.order.push_back(url.clone());
        self.entries
            .insert(url, CachedResource::Missing(std::time::Instant::now()));
    }

    fn remove(&mut self, url: &str) {
        if self.entries.contains_key(url) {
            self.order.retain(|key| key != url);
            self.remove_entry(url);
        }
    }

    fn remove_entry(&mut self, url: &str) {
        if let Some(CachedResource::Bytes(bytes)) = self.entries.remove(url) {
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes.len());
        }
    }
}

/// The exact responsive image candidate chosen during preparation.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedImage {
    pub resolved_url: String,
    pub density: f32,
}

/// A final image/font-aware document layout retained across viewport paints.
/// The DOM must not be mutated while this value is reused.
pub struct PreparedRender {
    viewport: (f32, f32),
    base_url: Option<String>,
    content_size: (f32, f32),
    viewport_fixed: std::collections::HashSet<obscura_dom::tree::NodeId>,
    sticky: crate::StickyLayout,
    selected_images: HashMap<obscura_dom::tree::NodeId, SelectedImage>,
    svg_fonts: Arc<usvg::fontdb::Database>,
    layout: crate::DomLayout,
}

impl PreparedRender {
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn layout(&self) -> &crate::DomLayout {
        &self.layout
    }

    pub fn content_size(&self) -> (f32, f32) {
        self.content_size
    }

    pub fn viewport_fixed_nodes(
        &self,
    ) -> &std::collections::HashSet<obscura_dom::tree::NodeId> {
        &self.viewport_fixed
    }

    pub fn sticky_layout(&self) -> &crate::StickyLayout {
        &self.sticky
    }

    pub fn clamp_scroll(&self, requested: (f32, f32)) -> (f32, f32) {
        let clamp_axis = |requested: f32, content: f32, viewport: f32| {
            if requested.is_finite() {
                requested.clamp(0.0, (content - viewport).max(0.0))
            } else {
                0.0
            }
        };
        (
            clamp_axis(requested.0, self.content_size.0, self.viewport.0),
            clamp_axis(requested.1, self.content_size.1, self.viewport.1),
        )
    }

    /// Border box in immutable document space, including authored translate
    /// transforms but excluding root-scroll and sticky movement.
    pub fn document_rect(
        &self,
        id: obscura_dom::tree::NodeId,
    ) -> Option<crate::Rect> {
        let rect = *self.layout.rects.get(&id)?;
        let offset = self
            .layout
            .translates
            .get(&id)
            .copied()
            .unwrap_or((0.0, 0.0));
        Some(crate::Rect {
            x: rect.x + offset.0,
            y: rect.y + offset.1,
            ..rect
        })
    }

    /// Unscaled padding-box size used by CSSOM View's `clientWidth` and
    /// `clientHeight`. Layout rects are border boxes, so remove the resolved
    /// borders but retain padding. This deliberately ignores visual
    /// transforms: client metrics describe layout geometry, unlike
    /// `getBoundingClientRect()`.
    pub fn client_size(
        &self,
        id: obscura_dom::tree::NodeId,
    ) -> Option<(f32, f32)> {
        let rect = self.layout.rects.get(&id)?;
        let style = self.layout.styles.get(&id)?;
        Some((
            (rect.width - style.border.left - style.border.right).max(0.0),
            (rect.height - style.border.top - style.border.bottom).max(0.0),
        ))
    }

    /// A compact CSSOM snapshot derived from the same final cascade and
    /// layout used by paint and geometry. Keeping this on `PreparedRender`
    /// lets script fetch all high-traffic computed properties in one op,
    /// without rebuilding layout once per property access.
    pub fn computed_style(
        &self,
        id: obscura_dom::tree::NodeId,
    ) -> Option<HashMap<&'static str, String>> {
        let style = self.layout.styles.get(&id)?;
        let rect = self.layout.rects.get(&id);
        let mut out = HashMap::new();

        let display = if style.display_contents {
            "contents"
        } else if style.display == crate::Display::None {
            "none"
        } else if style.internal_flex_container {
            "block"
        } else {
            match (style.display, style.is_inline_block) {
                (crate::Display::Flex, true) => "inline-flex",
                (crate::Display::Grid, true) => "inline-grid",
                (crate::Display::Block, true) => "inline-block",
                (crate::Display::Flex, false) => "flex",
                (crate::Display::Grid, false) => "grid",
                (crate::Display::Inline, false) => "inline",
                _ => "block",
            }
        };
        out.insert("display", display.to_string());
        out.insert(
            "position",
            if style.position_fixed {
                "fixed"
            } else if style.position_sticky {
                "sticky"
            } else {
                match style.position {
                    Some(taffy::Position::Absolute) => "absolute",
                    Some(taffy::Position::Relative) => "relative",
                    _ => "static",
                }
            }
            .to_string(),
        );
        out.insert(
            "z-index",
            style.z_index.map_or_else(|| "auto".to_string(), |v| v.to_string()),
        );
        out.insert(
            "visibility",
            if style.visibility_hidden.unwrap_or(false) {
                "hidden"
            } else {
                "visible"
            }
            .to_string(),
        );
        out.insert("opacity", css_number(style.opacity.unwrap_or(1.0)));
        out.insert(
            "background-color",
            css_color(style.background_color.unwrap_or([0, 0, 0, 0])),
        );
        out.insert("color", css_color(style.color.unwrap_or([0, 0, 0, 255])));

        if let Some(rect) = rect {
            let horizontal_non_content =
                style.border.left + style.border.right + style.padding.left + style.padding.right;
            let vertical_non_content =
                style.border.top + style.border.bottom + style.padding.top + style.padding.bottom;
            let (width, height) = match style.box_sizing {
                crate::BoxSizing::BorderBox => (rect.width, rect.height),
                _ => (
                    (rect.width - horizontal_non_content).max(0.0),
                    (rect.height - vertical_non_content).max(0.0),
                ),
            };
            out.insert("width", css_px(width));
            out.insert("height", css_px(height));
        } else {
            out.insert("width", dimension_css(style.width, "auto"));
            out.insert("height", dimension_css(style.height, "auto"));
        }
        out.insert("min-width", dimension_css(style.min_width, "auto"));
        out.insert("min-height", dimension_css(style.min_height, "auto"));
        out.insert("max-width", dimension_css(style.max_width, "none"));
        out.insert("max-height", dimension_css(style.max_height, "none"));
        out.insert(
            "box-sizing",
            if style.box_sizing == crate::BoxSizing::BorderBox {
                "border-box"
            } else {
                "content-box"
            }
            .to_string(),
        );

        let overflow_axis = |specified: u8, clipped: bool, scroll: bool| {
            if scroll {
                // The compact layout model intentionally merges
                // hidden/auto/scroll for clipping. `auto` is the least
                // surprising computed scroll-container value.
                "auto"
            } else if specified == 1 || clipped {
                "clip"
            } else {
                "visible"
            }
        };
        out.insert(
            "overflow-x",
            overflow_axis(
                style.overflow_specified_x,
                style.overflow_clip_x,
                style.overflow_scroll_x,
            )
            .to_string(),
        );
        out.insert(
            "overflow-y",
            overflow_axis(
                style.overflow_specified_y,
                style.overflow_clip_y,
                style.overflow_scroll_y,
            )
            .to_string(),
        );

        for (name, value, auto) in [
            ("margin-top", style.margin.top, style.margin_auto[0]),
            ("margin-right", style.margin.right, style.margin_auto[1]),
            ("margin-bottom", style.margin.bottom, style.margin_auto[2]),
            ("margin-left", style.margin.left, style.margin_auto[3]),
        ] {
            out.insert(name, if auto { "auto".to_string() } else { css_px(value) });
        }
        for (name, value) in [
            ("padding-top", style.padding.top),
            ("padding-right", style.padding.right),
            ("padding-bottom", style.padding.bottom),
            ("padding-left", style.padding.left),
            ("border-top-width", style.border.top),
            ("border-right-width", style.border.right),
            ("border-bottom-width", style.border.bottom),
            ("border-left-width", style.border.left),
        ] {
            out.insert(name, css_px(value));
        }
        let border_color = css_color(style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]));
        out.insert("border-top-color", border_color.clone());
        out.insert("border-right-color", border_color.clone());
        out.insert("border-bottom-color", border_color.clone());
        out.insert("border-left-color", border_color);
        for (name, width) in [
            ("border-top-style", style.border.top),
            ("border-right-style", style.border.right),
            ("border-bottom-style", style.border.bottom),
            ("border-left-style", style.border.left),
        ] {
            out.insert(name, if width > 0.0 { "solid" } else { "none" }.to_string());
        }

        out.insert(
            "flex-direction",
            match style.flex_direction.unwrap_or(taffy::FlexDirection::Row) {
                taffy::FlexDirection::Row => "row",
                taffy::FlexDirection::RowReverse => "row-reverse",
                taffy::FlexDirection::Column => "column",
                taffy::FlexDirection::ColumnReverse => "column-reverse",
            }
            .to_string(),
        );
        out.insert(
            "flex-wrap",
            match style.flex_wrap.unwrap_or(taffy::FlexWrap::NoWrap) {
                taffy::FlexWrap::NoWrap => "nowrap",
                taffy::FlexWrap::Wrap => "wrap",
                taffy::FlexWrap::WrapReverse => "wrap-reverse",
            }
            .to_string(),
        );
        out.insert(
            "align-items",
            style
                .align_items
                .map_or_else(|| "normal".to_string(), align_items_css),
        );
        out.insert(
            "justify-items",
            style
                .justify_items
                .map_or_else(|| "normal".to_string(), align_items_css),
        );
        out.insert(
            "justify-content",
            style
                .justify_content
                .map_or_else(|| "normal".to_string(), align_content_css),
        );
        out.insert(
            "align-content",
            style
                .align_content
                .map_or_else(|| "normal".to_string(), align_content_css),
        );
        out.insert("column-gap", style.column_gap.map_or_else(|| "normal".to_string(), css_px));
        out.insert("row-gap", style.row_gap.map_or_else(|| "normal".to_string(), css_px));
        out.insert(
            "grid-auto-flow",
            match style.grid_auto_flow.unwrap_or(taffy::GridAutoFlow::Row) {
                taffy::GridAutoFlow::Row => "row",
                taffy::GridAutoFlow::Column => "column",
                taffy::GridAutoFlow::RowDense => "row dense",
                taffy::GridAutoFlow::ColumnDense => "column dense",
            }
            .to_string(),
        );

        out.insert("transform", transform_css(style));
        out
            .insert("transform-origin", transform_origin_css(style, rect));
        Some(out)
    }

    /// Border box in the current root viewport. This is the read-only geometry
    /// path used by a later CSSOM integration and shares paint's clamped scroll,
    /// fixed-subtree, and sticky-positioning derivatives.
    pub fn viewport_rect(
        &self,
        id: obscura_dom::tree::NodeId,
        requested_scroll: (f32, f32),
    ) -> Option<crate::Rect> {
        let mut rect = self.document_rect(id)?;
        let scroll = self.clamp_scroll(requested_scroll);
        if !self.viewport_fixed.contains(&id) {
            let sticky = self.sticky.translation_for(id, self.viewport, scroll);
            rect.x += sticky.0 - scroll.0;
            rect.y += sticky.1 - scroll.1;
        }
        Some(rect)
    }

    pub fn selected_image(
        &self,
        id: obscura_dom::tree::NodeId,
    ) -> Option<&SelectedImage> {
        self.selected_images.get(&id)
    }
}

fn css_number(value: f32) -> String {
    if value == 0.0 {
        "0".to_string()
    } else {
        value.to_string()
    }
}

fn css_px(value: f32) -> String {
    format!("{}px", css_number(if value == 0.0 { 0.0 } else { value }))
}

fn dimension_css(value: crate::Dimension, auto: &str) -> String {
    match value {
        crate::Dimension::Auto => auto.to_string(),
        crate::Dimension::Px(v) => css_px(v),
        crate::Dimension::Percent(v) => format!("{}%", css_number(v * 100.0)),
        crate::Dimension::Em(v) => format!("{}em", css_number(v)),
        crate::Dimension::Ex(v) => format!("{}ex", css_number(v)),
        crate::Dimension::Rem(v) => format!("{}rem", css_number(v)),
        crate::Dimension::Vw(v) => format!("{}vw", css_number(v)),
        crate::Dimension::Vh(v) => format!("{}vh", css_number(v)),
        crate::Dimension::Vmin(v) => format!("{}vmin", css_number(v)),
        crate::Dimension::Vmax(v) => format!("{}vmax", css_number(v)),
    }
}

fn css_color([r, g, b, a]: [u8; 4]) -> String {
    if a == 255 {
        format!("rgb({r}, {g}, {b})")
    } else {
        format!(
            "rgba({r}, {g}, {b}, {})",
            css_number(a as f32 / 255.0)
        )
    }
}

fn align_items_css(value: taffy::AlignItems) -> String {
    let keyword = match value.keyword {
        taffy::AlignItemsKeyword::Start => "start",
        taffy::AlignItemsKeyword::End => "end",
        taffy::AlignItemsKeyword::FlexStart => "flex-start",
        taffy::AlignItemsKeyword::FlexEnd => "flex-end",
        taffy::AlignItemsKeyword::Center => "center",
        taffy::AlignItemsKeyword::Baseline => "baseline",
        taffy::AlignItemsKeyword::Stretch => "stretch",
    };
    if value.safety == taffy::AlignmentSafety::Safe {
        format!("safe {keyword}")
    } else {
        keyword.to_string()
    }
}

fn align_content_css(value: taffy::AlignContent) -> String {
    let keyword = match value.keyword {
        taffy::AlignContentKeyword::Start => "start",
        taffy::AlignContentKeyword::End => "end",
        taffy::AlignContentKeyword::FlexStart => "flex-start",
        taffy::AlignContentKeyword::FlexEnd => "flex-end",
        taffy::AlignContentKeyword::Center => "center",
        taffy::AlignContentKeyword::Stretch => "stretch",
        taffy::AlignContentKeyword::SpaceBetween => "space-between",
        taffy::AlignContentKeyword::SpaceEvenly => "space-evenly",
        taffy::AlignContentKeyword::SpaceAround => "space-around",
    };
    if value.safety == taffy::AlignmentSafety::Safe {
        format!("safe {keyword}")
    } else {
        keyword.to_string()
    }
}

fn transform_css(style: &crate::LayoutStyle) -> String {
    let mut parts = Vec::new();
    if let Some((x, y)) = style.transform_translate {
        parts.push(format!(
            "translate({}, {})",
            dimension_css(x, "0px"),
            dimension_css(y, "0px")
        ));
    }
    if let Some((x, y)) = style.individual_translate {
        parts.push(format!(
            "translate({}, {})",
            dimension_css(x, "0px"),
            dimension_css(y, "0px")
        ));
    }
    if let Some((x, y)) = style.transform_scale {
        parts.push(format!("scale({}, {})", css_number(x), css_number(y)));
    }
    if let Some([a, b, c, d]) = style.transform_projection {
        parts.push(format!(
            "matrix({}, {}, {}, {}, 0, 0)",
            css_number(a),
            css_number(b),
            css_number(c),
            css_number(d)
        ));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" ")
    }
}

fn transform_origin_css(style: &crate::LayoutStyle, rect: Option<&crate::Rect>) -> String {
    let (x, y) = style
        .transform_origin
        .unwrap_or((crate::Dimension::Percent(0.5), crate::Dimension::Percent(0.5)));
    let resolve = |value: crate::Dimension, axis: f32| match value {
        crate::Dimension::Percent(fraction) => css_px(fraction * axis),
        other => dimension_css(other, "0px"),
    };
    let (width, height) = rect.map_or((0.0, 0.0), |rect| (rect.width, rect.height));
    format!("{} {}", resolve(x, width), resolve(y, height))
}

/// Render `tree` at `viewport` (width, height) in CSS pixels to a Pixmap, or
/// None if the viewport is zero-sized. `base_url`, when given, resolves the
/// relative image URLs (`<img src="logo.svg">`) that make up the overwhelming
/// majority of real-world markup; without it only absolute and `data:` URLs
/// can be fetched.
pub fn paint_dom(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Pixmap> {
    paint_dom_scrolled(tree, viewport, base_url, (0.0, 0.0))
}

/// Render the visible viewport after root scrolling. Normal document content
/// is translated by the clamped scroll offset while viewport-fixed subtrees
/// remain anchored to the initial containing block.
pub fn paint_dom_scrolled(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    scroll: (f32, f32),
) -> Option<Pixmap> {
    let mut resources = RenderResourceCache::default();
    let mut prepared = prepare_dom(tree, viewport, base_url, &mut resources)?;
    paint_prepared(tree, &mut prepared, &mut resources, scroll)
}

/// Resolve image candidates and web fonts, then create the single final layout
/// shared by CSS geometry consumers and repeated paint.
pub fn prepare_dom(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    resources: &mut RenderResourceCache,
) -> Option<PreparedRender> {
    prepare_dom_with_dynamic_fonts(tree, viewport, base_url, resources, &[])
}

/// Prepare a DOM with the document's script-created `FontFace` registrations.
///
/// Authored rules and dynamic faces deliberately share the same bounded
/// resource cache, decoder, descriptor matching, and shaping database.
pub fn prepare_dom_with_dynamic_fonts(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    resources: &mut RenderResourceCache,
    dynamic_fonts: &[DynamicFontFace],
) -> Option<PreparedRender> {
    if !viewport.0.is_finite()
        || !viewport.1.is_finite()
        || viewport.0 <= 0.0
        || viewport.1 <= 0.0
    {
        return None;
    }
    // Fetch <img> bytes up front to learn intrinsic sizes for layout (a
    // CSS-sized image with no width/height attribute would otherwise be 0x0
    // and never paint). This seeds the same cache the paint pass reads, so
    // each URL is still fetched at most once.
    let (mut intrinsic, mut selected_images) =
        collect_image_intrinsics(tree, viewport, base_url, resources);
    let fonts = collect_web_fonts(tree, base_url, resources, dynamic_fonts);
    // Most framework pages use web fonts and many decorative SVG icons, but
    // only SVG text needs the page font faces. Avoid cloning/loading the page
    // font database for ordinary icons and HTML-only text.
    let svg_fonts = if has_inline_svg_text(tree) {
        svg_font_database_with_web_fonts(&fonts)
    } else {
        svg_font_database()
    };
    let mut laid = layout_dom_with_web_fonts(tree, viewport, &intrinsic, &fonts);
    // `content:url(...)` is computed by the author cascade, whereas ordinary
    // HTML image sources are available before layout. Pay for a second layout
    // only on the uncommon pages that actually use a CSS image as replaced
    // content: its metadata then enters the same intrinsic-size map as `src`.
    if collect_content_image_intrinsics(
        tree,
        &laid.styles,
        base_url,
        resources,
        &mut intrinsic,
        &mut selected_images,
    ) {
        laid = layout_dom_with_web_fonts(tree, viewport, &intrinsic, &fonts);
    }
    let content_size = laid.scrolling_content_size(tree, viewport);
    let viewport_fixed = laid.viewport_fixed_nodes(tree);
    let sticky = laid.root_sticky_layout(tree, viewport);
    Some(PreparedRender {
        viewport,
        base_url: base_url.map(str::to_string),
        content_size,
        viewport_fixed,
        sticky,
        selected_images,
        svg_fonts,
        layout: laid,
    })
}

/// Paint one root-scroll position from an already prepared resource-aware
/// layout. Resource bytes and glyph caches are reused across calls.
pub fn paint_prepared(
    tree: &DomTree,
    prepared: &mut PreparedRender,
    resources: &mut RenderResourceCache,
    scroll: (f32, f32),
) -> Option<Pixmap> {
    let (w, h) = (prepared.viewport.0 as u32, prepared.viewport.1 as u32);
    let mut pixmap = Pixmap::new(w, h)?;
    pixmap.fill(Color::WHITE);
    paint_laid_dom_scrolled(
        tree,
        prepared.viewport,
        prepared.base_url.as_deref(),
        scroll,
        pixmap,
        resources,
        &prepared.selected_images,
        &prepared.svg_fonts,
        prepared.content_size,
        &prepared.viewport_fixed,
        &prepared.sticky,
        &mut prepared.layout,
        None,
        None,
        None,
    )
}

/// The used `z-index` for an element which participates in stacking order.
///
/// A non-auto z-index applies not only to positioned boxes, but also to
/// otherwise-static flex and grid items. `display:contents` boxes are skipped
/// while finding the item's formatting-context parent because they generate no
/// box of their own.
fn stacking_z_index(
    tree: &DomTree,
    laid: &crate::DomLayout,
    id: obscura_dom::tree::NodeId,
) -> Option<i32> {
    let style = laid.styles.get(&id)?;
    if style.display == crate::Display::None || style.display_contents {
        return None;
    }
    let z = style.z_index?;
    if style.position.is_some() {
        return Some(z);
    }

    let mut parent = tree.get_node(id).and_then(|node| node.parent);
    while let Some(parent_id) = parent {
        let Some(parent_style) = laid.styles.get(&parent_id) else {
            parent = tree.get_node(parent_id).and_then(|node| node.parent);
            continue;
        };
        if parent_style.display_contents {
            parent = tree.get_node(parent_id).and_then(|node| node.parent);
            continue;
        }
        let is_flex_or_grid = parent_style.display == crate::Display::Grid
            || (parent_style.display == crate::Display::Flex
                && !parent_style.internal_flex_container);
        return is_flex_or_grid.then_some(z);
    }
    None
}

/// Paint an already prepared layout without changing its document-space
/// geometry. Root scrolling and sticky positioning are per-shot visual state,
/// so alternating captures can safely reuse the same layout.
fn paint_laid_dom_scrolled(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    scroll: (f32, f32),
    mut pixmap: Pixmap,
    image_cache: &mut RenderResourceCache,
    selected_images: &HashMap<obscura_dom::tree::NodeId, SelectedImage>,
    svg_fonts: &Arc<usvg::fontdb::Database>,
    content_size: (f32, f32),
    viewport_fixed: &std::collections::HashSet<obscura_dom::tree::NodeId>,
    sticky: &crate::StickyLayout,
    laid: &mut crate::DomLayout,
    paint_root: Option<obscura_dom::tree::NodeId>,
    suppress_opacity_for: Option<obscura_dom::tree::NodeId>,
    suppress_stacking_for: Option<obscura_dom::tree::NodeId>,
) -> Option<Pixmap> {
    let scroll_state =
        ScrollPaintState::new(viewport, scroll, content_size, viewport_fixed, sticky);
    // Only raster images inside an actually rotated/scaled subtree receive an
    // affine entry. Ordinary pages pay one fast style scan and allocate no
    // matrix map; transformed pages do matrix work only along those subtrees.
    let projected_images = collect_projected_image_transforms(tree, laid, &scroll_state);
    let root_font_size = tree
        .query_selector("html")
        .ok()
        .flatten()
        .and_then(|root| laid.styles.get(&root))
        .and_then(|style| style.font_size)
        .unwrap_or(16.0);
    // Nodes that live inside an inline `<svg>` we rasterized as one document;
    // their painting is owned by that raster, so they are skipped in both the
    // box/text loop below and the inline-formatting loop after it (an svg
    // `<text>` element must not also paint its glyphs on top of the raster).
    let mut svg_subtree_skip: std::collections::HashSet<obscura_dom::tree::NodeId> = std::collections::HashSet::new();
    // Text is painted in a second pass, after the box-order loop. Remember
    // every subtree already painted into an opacity layer so its shaped text
    // is not drawn a second time at full opacity in the outer pass.
    let mut opacity_subtree_skip: std::collections::HashSet<
        obscura_dom::tree::NodeId,
    > = std::collections::HashSet::new();
    // External sprite symbols, keyed by "url#id", extracted from a fetched
    // sprite file so a `<use href="url#id">` resolves. One sprite backs many
    // icons (a whole logo/icon band), so cache the parsed symbol across every
    // inline svg on the page rather than re-parsing the sprite per icon.
    let mut sprite_cache: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    // Whether any element carries a `transform: translate()`. When none does
    // (the overwhelmingly common case), every node's accumulated offset is
    // zero, so skip the per-node ancestor walk entirely and keep the paint
    // path free of any added cost.

    // Paint order: tree order for the normal flow (later elements paint over
    // earlier ones), except that a positioned element or a flex/grid item
    // with a non-auto z-index lifts its whole subtree into an atomic stacking
    // unit: negative layers paint under the normal flow, non-negative ones
    // above it, each sorted by z-index ascending (stable, so equal z keeps
    // tree order). The unit is recursively painted at its sorted position,
    // preventing its backgrounds, replaced content, and shaped text from
    // leaking into different global paint phases.
    let mut neg_layers: Vec<(i32, Vec<obscura_dom::tree::NodeId>)> = Vec::new();
    let mut pos_layers: Vec<(i32, Vec<obscura_dom::tree::NodeId>)> = Vec::new();
    let mut normal: Vec<obscura_dom::tree::NodeId> = Vec::new();
    let mut consumed: std::collections::HashSet<obscura_dom::tree::NodeId> = std::collections::HashSet::new();
    let mut paint_nodes = paint_root.into_iter().collect::<Vec<_>>();
    paint_nodes.extend(tree.descendants(paint_root.unwrap_or_else(|| tree.document())));
    for nid in paint_nodes.iter().copied() {
        if consumed.contains(&nid) {
            continue;
        }
        let is_opacity_root = suppress_opacity_for != Some(nid)
            && laid
                .styles
                .get(&nid)
                .and_then(|style| style.opacity)
                .is_some_and(|opacity| opacity.clamp(0.0, 1.0) < 1.0);
        let z = (suppress_stacking_for != Some(nid))
            .then(|| stacking_z_index(tree, laid, nid))
            .flatten();
        if is_opacity_root {
            let mut sub = vec![nid];
            sub.extend(tree.descendants(nid));
            for &member in &sub {
                consumed.insert(member);
            }
            // An opacity effect is one atomic paint-order unit. Its internal
            // z-order is resolved while painting its isolated surface.
            match z {
                Some(z) if z < 0 => neg_layers.push((z, vec![nid])),
                Some(z) => pos_layers.push((z, vec![nid])),
                None => normal.push(nid),
            }
        } else if let Some(z) = z {
            let mut sub = vec![nid];
            sub.extend(tree.descendants(nid));
            for &m in &sub {
                consumed.insert(m);
            }
            if z < 0 {
                neg_layers.push((z, vec![nid]));
            } else {
                pos_layers.push((z, vec![nid]));
            }
        } else {
            normal.push(nid);
        }
    }
    neg_layers.sort_by_key(|(z, _)| *z);
    pos_layers.sort_by_key(|(z, _)| *z);
    let paint_order: Vec<obscura_dom::tree::NodeId> = neg_layers
        .into_iter()
        .flat_map(|(_, sub)| sub)
        .chain(normal)
        .chain(pos_layers.into_iter().flat_map(|(_, sub)| sub))
        .collect();

    // Generated boxes are anonymous layout children. ::before paints directly
    // after its host's own box; ::after paints after the host's last DOM
    // descendant in this paint order. Build the latter schedule only on pages
    // that actually materialized a generated box. A reverse DOM-preorder pass
    // propagates each node's paint index to its parent once, deriving subtree
    // endpoints in O(nodes + generated boxes), including reordered z layers.
    let mut generated_before: std::collections::HashMap<
        obscura_dom::tree::NodeId,
        Vec<crate::dom::GeneratedBox>,
    > = std::collections::HashMap::new();
    let mut generated_after_at: Vec<Vec<crate::dom::GeneratedBox>> =
        vec![Vec::new(); paint_order.len()];
    if !laid.generated_boxes.is_empty() {
        let paint_indices: std::collections::HashMap<obscura_dom::tree::NodeId, usize> =
            paint_order
                .iter()
                .enumerate()
                .map(|(index, &nid)| (nid, index))
                .collect();
        let mut last_index: std::collections::HashMap<obscura_dom::tree::NodeId, usize> =
            paint_indices.clone();
        let dom_preorder = paint_nodes.clone();
        for nid in dom_preorder.into_iter().rev() {
            let Some(index) = last_index.get(&nid).copied() else {
                continue;
            };
            if paint_root != Some(nid) {
                if let Some(parent) = tree.get_node(nid).and_then(|node| node.parent) {
                    last_index
                        .entry(parent)
                        .and_modify(|last| *last = (*last).max(index))
                        .or_insert(index);
                }
            }
        }
        for generated in laid.generated_boxes.iter().copied() {
            match generated.kind {
                crate::dom::GeneratedBoxKind::Before => {
                    if paint_indices.contains_key(&generated.host) {
                        generated_before
                            .entry(generated.host)
                            .or_default()
                            .push(generated);
                    }
                }
                crate::dom::GeneratedBoxKind::After => {
                    if let Some(index) = last_index.get(&generated.host) {
                        generated_after_at[*index].push(generated);
                    }
                }
            }
        }
    }

    for (paint_index, nid) in paint_order.into_iter().enumerate() {
        if svg_subtree_skip.contains(&nid) {
            continue;
        }
        if suppress_stacking_for != Some(nid)
            && stacking_z_index(tree, laid, nid).is_some()
        {
            opacity_subtree_skip.insert(nid);
            opacity_subtree_skip.extend(tree.descendants(nid));
            // A stacking context is one structural paint item in its parent.
            // Re-enter the same display-list builder with this root suppressed
            // so all of its decorations, descendants, and shaped inline text
            // finish before the next sibling stacking unit starts. Passing the
            // existing pixmap avoids a viewport-sized intermediate surface;
            // opacity still takes the isolated-surface path below when needed.
            pixmap = paint_laid_dom_scrolled(
                tree,
                viewport,
                base_url,
                scroll,
                pixmap,
                image_cache,
                selected_images,
                svg_fonts,
                content_size,
                viewport_fixed,
                sticky,
                laid,
                Some(nid),
                suppress_opacity_for,
                Some(nid),
            )?;
            continue;
        }
        let node = match tree.get_node(nid) {
            Some(n) => n,
            None => continue,
        };

        if node.is_text() {
            paint_text_node(tree, nid, laid, &scroll_state, &mut pixmap);
            for generated in &generated_after_at[paint_index] {
                paint_in_flow_generated_box(
                    &mut pixmap,
                    generated,
                    laid,
                    &scroll_state,
                    viewport,
                    root_font_size,
                    base_url,
                    image_cache,
                );
            }
            continue;
        }

        let name = match node.as_element() {
            Some(name) => name,
            None => continue,
        };
        let rect = match laid.rects.get(&nid) {
            Some(r) => *r,
            None => continue,
        };

        let own_opacity = laid
            .styles
            .get(&nid)
            .and_then(|style| style.opacity)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
        if suppress_opacity_for != Some(nid) && own_opacity < 1.0 {
            opacity_subtree_skip.insert(nid);
            opacity_subtree_skip.extend(tree.descendants(nid));
            if own_opacity <= 0.0 {
                continue;
            }
            // Opacity is applied to the finished stacking context, never to
            // each primitive. Otherwise two opaque overlapping children at
            // opacity:.5 incorrectly become .75 alpha in their overlap.
            let layer = Pixmap::new(pixmap.width(), pixmap.height())?;
            let layer = paint_laid_dom_scrolled(
                tree,
                viewport,
                base_url,
                scroll,
                layer,
                image_cache,
                selected_images,
                svg_fonts,
                content_size,
                viewport_fixed,
                sticky,
                laid,
                Some(nid),
                Some(nid),
                suppress_stacking_for,
            )?;
            let group_paint = tiny_skia::PixmapPaint {
                opacity: own_opacity,
                ..tiny_skia::PixmapPaint::default()
            };
            pixmap.draw_pixmap(
                0,
                0,
                layer.as_ref(),
                &group_paint,
                Transform::identity(),
                None,
            );
            continue;
        }

        let style = match laid.styles.get(&nid) {
            Some(s) => s,
            None => continue,
        };

        if style.effectively_invisible {
            continue;
        }

        // A `transform: translate()` on this element or any ancestor offsets
        // this element's whole painted box (and, applied per node, its whole
        // subtree). The box shifts into screen space. The inherited clip is
        // owner-shifted by layout and root-scroll/sticky-adjusted by the visual
        // state, but it must not move with this descendant: that is what lets a
        // clip cull a slide the carousel track translated out of its viewport.
        let (ox, oy) = scroll_state.translation_for(laid, nid);
        let rect = crate::Rect { x: rect.x + ox, y: rect.y + oy, width: rect.width, height: rect.height };

        // Ancestor `overflow: hidden` clip, if any. Skip painting entirely
        // once the box has no visible overlap with it (this is what makes the
        // ubiquitous 1x1 clipped "visually hidden" accessibility pattern
        // actually invisible instead of painting text wherever it lands).
        let clip = scroll_state.clip_for(laid, nid);
        let projected_image = projected_images.get(&nid).copied();
        let cull_rect = projected_image
            .map(|transform| transform.map_rect(rect))
            .unwrap_or(rect);
        let visible_rect = match clip {
            Some(c) => match cull_rect.intersect(&c) {
                Some(r) => r,
                None => continue,
            },
            None => cull_rect,
        };
        let box_rect = match Rect::from_xywh(visible_rect.x, visible_rect.y, visible_rect.width, visible_rect.height) {
            Some(r) => r,
            None => continue,
        };
        let clip_path_mask = style.clip_path.as_ref().and_then(|polygon| {
            polygon_clip_mask(
                pixmap.width(),
                pixmap.height(),
                polygon,
                &rect,
                style.font_size.unwrap_or(16.0),
                root_font_size,
                viewport,
            )
        });

        // Outset box-shadow paints behind this element's own background/border.
        // Geometry comes from the full (translate-adjusted) border box; the
        // ancestor overflow clip is reapplied inside so the shadow is clipped by
        // an ancestor exactly as the box itself is.
        if let Some(shadow) = style.box_shadow {
            paint_box_shadow(&mut pixmap, &shadow, &rect, style.border_radius, clip);
        }

        // Box path (rounded if border-radius), reused for gradient/color fill.
        let radius = style.border_radius.resolve(rect.width, rect.height);
        let has_radius = radius.0 > 0.5 && radius.1 > 0.5;
        let bg_path = || {
            if has_radius {
            rounded_rect_path(
                visible_rect.x,
                visible_rect.y,
                visible_rect.width,
                visible_rect.height,
                radius.0,
                radius.1,
            )
        } else {
            let mut pb = PathBuilder::new();
            pb.push_rect(box_rect);
            pb.finish()
            }
        };
        // A linear-gradient background (heavily used by modern hero sections);
        // without this it paints white. Takes precedence over a solid color.
        // `background-clip: text` clips the background to the glyphs, so it must
        // not paint as a box here; the text paint path fills the glyphs instead.
        if style.mask_image.is_none() && !style.background_clip_text {
            if let Some(bg) = style.background_color {
                if let Some(path) = bg_path() {
                    let mut paint = Paint::default();
                    paint.set_color(Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
                    paint.anti_alias = has_radius;
                    pixmap.fill_path(
                        &path,
                        &paint,
                        FillRule::Winding,
                        Transform::identity(),
                        clip_path_mask.as_ref(),
                    );
                }
            }
            if !style.background_gradient_layers.is_empty() {
                if let Some(path) = bg_path() {
                    paint_background_gradient_layers(
                        &mut pixmap,
                        &path,
                        &visible_rect,
                        radius,
                        style,
                        root_font_size,
                        viewport,
                        clip_path_mask.as_ref(),
                    );
                }
            } else {
            if let Some((center, stops)) = &style.background_radial_gradient {
                if let Some(path) = bg_path() {
                    paint_radial_gradient(
                        &mut pixmap,
                        &path,
                        &visible_rect,
                        *center,
                        stops,
                        clip_path_mask.as_ref(),
                    );
                }
            }
            if let Some((angle, center, stops)) = &style.background_conic_gradient {
                paint_conic_gradient(
                    &mut pixmap,
                    &visible_rect,
                    radius,
                    *angle,
                    *center,
                    stops,
                    clip_path_mask.as_ref(),
                );
            }
            if let Some((angle, stops)) = &style.background_gradient {
                if let Some(path) = bg_path() {
                    paint_linear_gradient(
                        &mut pixmap,
                        &path,
                        &visible_rect,
                        *angle,
                        stops,
                        clip_path_mask.as_ref(),
                    );
                }
            }
        }
        }

        if let Some(mask_url) = &style.mask_image {
            let fill = style.background_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            paint_mask(
                mask_url,
                base_url,
                &visible_rect,
                radius,
                fill,
                style.background_radial_gradient.as_ref(),
                style.background_gradient.as_ref(),
                style.background_conic_gradient.as_ref(),
                style.mask_size,
                style.mask_repeat,
                clip_path_mask.as_ref(),
                &mut pixmap,
                image_cache,
            );
        } else if let Some(bg_url) = &style.background_image {
            if let Some(img_rect) = background_image_rect(
                bg_url,
                base_url,
                &rect,
                style.background_size,
                style.background_size_expression.as_deref(),
                style.background_size_fit,
                style.background_position,
                style.font_size.unwrap_or(16.0),
                root_font_size,
                viewport,
                image_cache,
            ) {
                // A background layer is always clipped to its owner's border
                // box and then to inherited overflow. Keep its full destination
                // rect separate from that clip: intersecting first and then
                // scaling would resize a partially clipped image.
                let visible = match clip {
                    Some(c) => rect.intersect(&c),
                    None => Some(rect),
                };
                if let Some(visible) = visible {
                    paint_image(
                        bg_url,
                        base_url,
                        &img_rect,
                        &visible,
                        crate::ObjectFit::Fill,
                        &mut pixmap,
                        image_cache,
                        None,
                        radius,
                        clip_path_mask.as_ref(),
                    );
                }
            }
        }
        let positioned_pseudo_containing_block = {
            let mut ancestor = Some(nid);
            let mut found = None;
            while let Some(candidate) = ancestor {
                let candidate_style = laid.styles.get(&candidate);
                let establishes = candidate_style.is_some_and(|candidate_style| {
                    candidate_style.position.is_some()
                        || candidate_style.establishes_positioning_containing_block()
                });
                if establishes {
                    if let Some(candidate_rect) = laid.rects.get(&candidate).copied() {
                        let (candidate_x, candidate_y) =
                            scroll_state.translation_for(laid, candidate);
                        found = Some(crate::Rect {
                            x: candidate_rect.x + candidate_x,
                            y: candidate_rect.y + candidate_y,
                            width: candidate_rect.width,
                            height: candidate_rect.height,
                        });
                        break;
                    }
                    // A positioned inline can be flattened out of the taffy
                    // tree when its block children form the real layout
                    // boxes. It is still the CSS containing block, but has no
                    // usable rectangle in this representation. Keep walking
                    // to the nearest positioned ancestor that does have one
                    // instead of falling back to the host line itself. The
                    // latter places `left:0` generated gutters directly over
                    // their margin-shifted text.
                }
                ancestor = tree.get_node(candidate).and_then(|node| node.parent);
            }
            found.unwrap_or(rect)
        };
        let positioned_pseudo_clip = scroll_state.descendant_clip_for(laid, nid);
        for pseudo in [
            style.before_pseudo.as_deref(),
            style.after_pseudo.as_deref(),
        ]
            .into_iter()
            .flatten()
        {
            paint_positioned_pseudo(
                &mut laid.text_engine,
                &mut pixmap,
                pseudo,
                &positioned_pseudo_containing_block,
                &rect,
                viewport,
                root_font_size,
                positioned_pseudo_clip,
                base_url,
                image_cache,
            );
        }

        // Rounded, uniform border: stroke the rounded-rect outline instead of
        // four sharp edge rects.
        let uniform_border = style.border.top == style.border.right
            && style.border.right == style.border.bottom
            && style.border.bottom == style.border.left
            && style.border.top > 0.0;
        if has_radius && uniform_border {
            let bc = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            let w = style.border.top;
            if let Some(path) = rounded_rect_path(
                rect.x + w / 2.0,
                rect.y + w / 2.0,
                rect.width - w,
                rect.height - w,
                (radius.0 - w / 2.0).max(0.0),
                (radius.1 - w / 2.0).max(0.0),
            ) {
                let mut paint = Paint::default();
                paint.set_color(Color::from_rgba8(bc[0], bc[1], bc[2], bc[3]));
                paint.anti_alias = true;
                let stroke = tiny_skia::Stroke { width: w, ..Default::default() };
                pixmap.stroke_path(
                    &path,
                    &paint,
                    &stroke,
                    Transform::identity(),
                    clip_path_mask.as_ref(),
                );
            }
        } else if style.border.top > 0.0 || style.border.right > 0.0 || style.border.bottom > 0.0 || style.border.left > 0.0 {
            let bc = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(bc[0], bc[1], bc[2], bc[3]));
            paint.anti_alias = false;

            let mut path = PathBuilder::new();
            let mut push_clipped = |x: f32, y: f32, w: f32, h: f32| {
                let edge = crate::Rect { x, y, width: w, height: h };
                let edge = match clip { Some(c) => edge.intersect(&c), None => Some(edge) };
                if let Some(e) = edge {
                    if let Some(r) = Rect::from_xywh(e.x, e.y, e.width, e.height) {
                        path.push_rect(r);
                    }
                }
            };
            if style.border.top > 0.0 {
                push_clipped(rect.x, rect.y, rect.width, style.border.top);
            }
            if style.border.right > 0.0 {
                push_clipped(rect.x + rect.width - style.border.right, rect.y, style.border.right, rect.height);
            }
            if style.border.bottom > 0.0 {
                push_clipped(rect.x, rect.y + rect.height - style.border.bottom, rect.width, style.border.bottom);
            }
            if style.border.left > 0.0 {
                push_clipped(rect.x, rect.y, style.border.left, rect.height);
            }
            if let Some(path) = path.finish() {
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    clip_path_mask.as_ref(),
                );
            }
        }

        if name.local.as_ref() == "img" {
            if let Some(source) = selected_images.get(&nid) {
                // `visible_rect` is the border box already intersected with the
                // ancestor overflow clip: the raster must not paint past it (a
                // half-scrolled carousel slide's image otherwise bleeds over
                // the viewport edge).
                let painted = paint_image(
                        &source.resolved_url,
                        None,
                        &rect,
                        &visible_rect,
                        style.object_fit,
                        &mut pixmap,
                        image_cache,
                        projected_image,
                        radius,
                        clip_path_mask.as_ref(),
                    );
                // Fall back when the image itself did not paint, following
                // what browsers show for a broken image: a non-empty alt
                // renders as text in place of the image (no placeholder box),
                // alt="" renders nothing at all (the author declared the
                // image decorative), and only a MISSING alt keeps the neutral
                // grey placeholder. box_rect/visible_rect are already
                // clip-intersected, so none of this paints outside an
                // overflow:hidden clip.
                if !painted {
                    match node.get_attribute("alt") {
                        Some(alt) if !alt.trim().is_empty() => {
                            draw_text(
                                &mut pixmap,
                                &alt,
                                rect.x,
                                rect.y,
                                [0, 0, 0, 255],
                                12.0,
                                false,
                                None,
                                0.0,
                                clip,
                            );
                        }
                        Some(_) => {}
                        None => {
                            if visible_rect.width >= 4.0 && visible_rect.height >= 4.0 {
                                let mut ph = Paint::default();
                                ph.set_color(Color::from_rgba8(0xE9, 0xEA, 0xEC, 0xFF));
                                pixmap.fill_rect(box_rect, &ph, Transform::identity(), None);
                            }
                        }
                    }
                }
            }
        }

        // Inline `<svg>...</svg>`: serialize the whole subtree back to one
        // standalone SVG document and rasterize it as a unit, so a
        // `<use href="#id">` resolves against the `<symbol>`/`<defs>` in the
        // same svg. The raster owns the subtree, so its DOM children are not
        // painted individually (they are added to `svg_subtree_skip`). The svg
        // is drawn at its full border-box size (undistorted) and clipped to the
        // overflow-visible region.
        if name.local.as_ref() == "svg" {
            let mut markup = serialize_svg_styled(
                tree,
                nid,
                &laid.styles,
                (suppress_opacity_for == Some(nid)).then_some(nid),
            );
            // Resolve referenced symbols before carrying the host color into
            // the standalone document. A document-level/external symbol may
            // itself contain `currentColor`, and therefore has to be present
            // when the root color is established.
            inject_external_sprites(tree, nid, base_url, &mut markup, image_cache, &mut sprite_cache);
            // resvg parses the serialized subtree as a standalone SVG
            // document, outside the page's author stylesheet. Preserve the
            // host element's computed `color` so paths using `currentColor`
            // (the standard framework-logo/icon pattern) do not fall back to
            // black.
            if let Some(color) = style.color {
                inject_svg_current_color(&mut markup, color);
            }
            // `<use href="url#id">` pointing at an EXTERNAL sprite file resolves
            // to nothing in resvg (the symbol lives in another document). Fetch
            // the sprite, splice the referenced `<symbol>` into a local `<defs>`,
            // and rewrite the href to a same-document `#id`. Same-document
            // `<use href="#id">` (empty url) is untouched.
            if let Some(content) = render_svg_with_font_database(
                markup.as_bytes(),
                rect.width as u32,
                rect.height as u32,
                &svg_fonts,
            ) {
                let mask = if clip.is_some() || has_radius {
                    rounded_box_clip_mask(
                        pixmap.width(),
                        pixmap.height(),
                        &visible_rect,
                        radius,
                    )
                } else {
                    None
                };
                pixmap.draw_pixmap(
                    rect.x as i32,
                    rect.y as i32,
                    content.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    mask.as_ref(),
                );
            }
            for child in tree.descendants(nid) {
                svg_subtree_skip.insert(child);
            }
        }

        if let Some(generated) = generated_before.get(&nid) {
            for generated in generated {
                paint_in_flow_generated_box(
                    &mut pixmap,
                    generated,
                    laid,
                    &scroll_state,
                    viewport,
                    root_font_size,
                    base_url,
                    image_cache,
                );
            }
        }

        // List-item marker (bullet or number), drawn in the indent to the left
        // of the item's content box. `list_style` is inherited and resolved,
        // so `None` (e.g. a nav `<ul style="list-style:none">`) suppresses it.
        if name.local.as_ref() == "li" {
            if let Some(marker) = list_marker_text(tree, nid, style.list_style) {
                let fsize = style.font_size.unwrap_or(16.0);
                let color = style.color.unwrap_or([0, 0, 0, 255]);
                let mw = measure_text(&marker, fsize, false, style.font_family.as_deref());
                let mx = rect.x + style.padding.left - mw - 6.0;
                let my = rect.y + style.border.top + style.padding.top;
                draw_text(
                    &mut pixmap,
                    &marker,
                    mx,
                    my,
                    color,
                    fsize,
                    false,
                    style.font_family.as_deref(),
                    style.letter_spacing.unwrap_or(0.0),
                    clip,
                );
            }
        }

        // `::before`/`::after` generated text (see `dom::build_pseudo_content`)
        // has no DOM text node of its own; its word runs are registered under
        // the host element's own id instead, so paint them here rather than
        // through `paint_text_node` (which only runs for real text nodes).
        if let Some(runs) = laid.text_runs.get(&nid) {
            let color = style.color.unwrap_or([0, 0, 0, 255]);
            let fsize = style.font_size.unwrap_or(16.0);
            let is_bold = crate::style::used_font_weight(style) >= 600;
            for (word_rect, word) in runs {
                draw_text(
                    &mut pixmap,
                    word,
                    word_rect.x + ox,
                    word_rect.y + oy,
                    color,
                    fsize,
                    is_bold,
                    style.font_family.as_deref(),
                    style.letter_spacing.unwrap_or(0.0),
                    clip,
                );
            }
        }

        // A closed native `<select>` paints only its selected option. Options
        // themselves are popup content (`display:none` in the layout tree),
        // so the label and disclosure arrow belong to the atomic control.
        if name.local.as_ref() == "select" {
            if let Some(label) = selected_option_label(tree, nid) {
                let fsize = style.font_size.unwrap_or(13.333_333);
                let line_height = crate::inline::used_line_height(style);
                let text_x = rect.x + style.border.left + style.padding.left;
                let text_y = rect.y + (rect.height - line_height) / 2.0;
                draw_text(
                    &mut pixmap,
                    &label,
                    text_x,
                    text_y,
                    style.color.unwrap_or([0, 0, 0, 255]),
                    fsize,
                    crate::style::used_font_weight(style) >= 600,
                    style.font_family.as_deref(),
                    style.letter_spacing.unwrap_or(0.0),
                    Some(visible_rect),
                );
            }
            if rect.width >= 12.0 && rect.height >= 8.0 {
                let center_x = rect.x + rect.width - style.border.right - 8.0;
                let center_y = rect.y + rect.height / 2.0;
                let mut arrow = PathBuilder::new();
                arrow.move_to(center_x - 3.5, center_y - 2.0);
                arrow.line_to(center_x + 3.5, center_y - 2.0);
                arrow.line_to(center_x, center_y + 2.5);
                arrow.close();
                if let Some(arrow) = arrow.finish() {
                    let mut arrow_paint = Paint::default();
                    let color = style.color.unwrap_or([0, 0, 0, 255]);
                    arrow_paint.set_color(Color::from_rgba8(
                        color[0], color[1], color[2], color[3],
                    ));
                    pixmap.fill_path(
                        &arrow,
                        &arrow_paint,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        // An empty text `<input>`/`<textarea>` shows its `placeholder`
        // attribute as muted text; there is no DOM text node for it (it is
        // not real content), so paint it directly from the attribute instead
        // of going through `paint_text_node`.
        if name.local.as_ref() == "input" || name.local.as_ref() == "textarea" {
            let has_value = node.get_attribute("value").map(|v| !v.is_empty()).unwrap_or(false);
            if !has_value {
                if let Some(placeholder) = node.get_attribute("placeholder") {
                    if !placeholder.is_empty() {
                        let fsize = style.font_size.unwrap_or(16.0);
                        let text_x = rect.x + style.padding.left + style.border.left;
                        let text_y = rect.y + style.padding.top + style.border.top;
                        draw_text(
                            &mut pixmap,
                            placeholder,
                            text_x,
                            text_y,
                            [117, 117, 117, 255],
                            fsize,
                            false,
                            style.font_family.as_deref(),
                            style.letter_spacing.unwrap_or(0.0),
                            clip,
                        );
                    }
                }
            }
        }
        for generated in &generated_after_at[paint_index] {
            paint_in_flow_generated_box(
                &mut pixmap,
                generated,
                laid,
                &scroll_state,
                viewport,
                root_font_size,
                base_url,
                image_cache,
            );
        }
    }

    // Inline formatting contexts shaped by cosmic-text (paragraphs, headings,
    // cells, labels) draw last, in tree order, so their glyphs sit above the
    // box backgrounds/borders painted in the loop above. Each item already
    // carries its final origin and clip from `TextEngine::finalize`.
    for nid in paint_nodes {
        if svg_subtree_skip.contains(&nid) || opacity_subtree_skip.contains(&nid) {
            continue;
        }
        let whole = laid.ifc_items.get(&nid).copied();
        let run_items = laid.run_ifc_items.get(&nid).cloned();
        if whole.is_none() && run_items.is_none() {
            continue;
        }
        if laid.styles.get(&nid).map(|s| s.effectively_invisible).unwrap_or(false) {
            continue;
        }
        // Shift the shaped glyphs by the same accumulated translate as the
        // container's box so text under a transformed ancestor moves with
        // it. Computed before the mutable `paint_item` borrow.
        let off = scroll_state.translation_for(laid, nid);
        let clip = scroll_state.shaped_text_clip_for(laid, nid);
        if let Some(idx) = whole {
            laid.text_engine
                .paint_item_with_clip(idx, &mut pixmap, off, clip);
        }
        // Anonymous inline-run leaves of a mixed block (see
        // `build_mixed_block`), pinned to their own boxes at finalize.
        if let Some(items) = run_items {
            for idx in items {
                laid.text_engine
                    .paint_item_with_clip(idx, &mut pixmap, off, clip);
            }
        }
    }

    Some(pixmap)
}

/// Per-capture root-scroll and sticky offsets layered over an immutable
/// document-space [`DomLayout`]. Keeping these deltas out of the layout avoids
/// accumulating movement when the same prepared document paints more than one
/// frame.
#[derive(Debug)]
struct ScrollPaintState<'a> {
    viewport: (f32, f32),
    scroll: (f32, f32),
    viewport_fixed: &'a std::collections::HashSet<obscura_dom::tree::NodeId>,
    sticky: std::collections::HashMap<obscura_dom::tree::NodeId, (f32, f32)>,
    sticky_clips: std::collections::HashMap<obscura_dom::tree::NodeId, (f32, f32)>,
    active: bool,
}

impl<'a> ScrollPaintState<'a> {
    fn new(
        viewport: (f32, f32),
        requested: (f32, f32),
        content: (f32, f32),
        viewport_fixed: &'a std::collections::HashSet<obscura_dom::tree::NodeId>,
        sticky_layout: &crate::StickyLayout,
    ) -> Self {
        let scroll_x = if requested.0.is_finite() {
            requested
                .0
                .clamp(0.0, (content.0 - viewport.0).max(0.0))
        } else {
            0.0
        };
        let scroll_y = if requested.1.is_finite() {
            requested
                .1
                .clamp(0.0, (content.1 - viewport.1).max(0.0))
        } else {
            0.0
        };
        let scroll = (scroll_x, scroll_y);
        let active = scroll != (0.0, 0.0) || !sticky_layout.is_empty();
        if !active {
            return Self {
                viewport,
                scroll,
                viewport_fixed,
                sticky: std::collections::HashMap::new(),
                sticky_clips: std::collections::HashMap::new(),
                active,
            };
        }

        let sticky = sticky_layout.translations(viewport, scroll);
        let sticky_clips = sticky_layout.clip_translations_from(&sticky);
        Self {
            viewport,
            scroll,
            viewport_fixed,
            sticky,
            sticky_clips,
            active,
        }
    }

    fn translation_for(
        &self,
        laid: &crate::DomLayout,
        id: obscura_dom::tree::NodeId,
    ) -> (f32, f32) {
        let base = laid.translates.get(&id).copied().unwrap_or((0.0, 0.0));
        if !self.active || self.viewport_fixed.contains(&id) {
            return base;
        }
        let sticky = self.sticky.get(&id).copied().unwrap_or((0.0, 0.0));
        (
            base.0 + sticky.0 - self.scroll.0,
            base.1 + sticky.1 - self.scroll.1,
        )
    }

    fn clip_for(
        &self,
        laid: &crate::DomLayout,
        id: obscura_dom::tree::NodeId,
    ) -> Option<crate::Rect> {
        self.overflow_clip_for(laid, id)
            .map(|clip| clip.viewport_rect(self.viewport))
    }

    fn overflow_clip_for(
        &self,
        laid: &crate::DomLayout,
        id: obscura_dom::tree::NodeId,
    ) -> Option<crate::dom::OverflowClip> {
        let mut clip = laid.clip_rects.get(&id).copied().flatten()?;
        if !self.active || self.viewport_fixed.contains(&id) {
            return Some(clip);
        }
        let sticky = self
            .sticky_clips
            .get(&id)
            .copied()
            .unwrap_or((0.0, 0.0));
        clip.translate(
            sticky.0 - self.scroll.0,
            sticky.1 - self.scroll.1,
        );
        Some(clip)
    }

    fn descendant_clip_for(
        &self,
        laid: &crate::DomLayout,
        id: obscura_dom::tree::NodeId,
    ) -> Option<crate::Rect> {
        let inherited = self.overflow_clip_for(laid, id);
        let style = laid.styles.get(&id)?;
        if !style.overflow_hidden || style.overflow_propagated_to_viewport {
            return inherited.map(|clip| clip.viewport_rect(self.viewport));
        }
        let rect = laid.rects.get(&id)?;
        let (ox, oy) = self.translation_for(laid, id);
        let own = crate::dom::OverflowClip::for_box(rect, style, ox, oy);
        Some(
            match inherited {
                Some(clip) => clip.intersect(own),
                None => own,
            }
            .viewport_rect(self.viewport),
        )
    }

    /// Capture-space clip for a shaped inline context. `clip_for` supplies the
    /// adjusted ancestor chain; text owned by an `overflow:hidden` element
    /// also needs that element's own padding-box clip in the same viewport
    /// coordinates.
    fn shaped_text_clip_for(
        &self,
        laid: &crate::DomLayout,
        id: obscura_dom::tree::NodeId,
    ) -> Option<crate::Rect> {
        self.descendant_clip_for(laid, id)
    }
}

#[derive(Clone, Copy, Debug)]
struct ImageAffine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl ImageAffine {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Compose `self(other(point))`.
    fn then(self, other: Self) -> Self {
        Self {
            a: self.a * other.a + self.c * other.b,
            b: self.b * other.a + self.d * other.b,
            c: self.a * other.c + self.c * other.d,
            d: self.b * other.c + self.d * other.d,
            e: self.a * other.e + self.c * other.f + self.e,
            f: self.b * other.e + self.d * other.f + self.f,
        }
    }

    fn around(origin: (f32, f32), linear: [f32; 4]) -> Self {
        let [a, b, c, d] = linear;
        Self {
            a,
            b,
            c,
            d,
            e: origin.0 - a * origin.0 - c * origin.1,
            f: origin.1 - b * origin.0 - d * origin.1,
        }
    }

    fn map(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    fn map_rect(self, rect: crate::Rect) -> crate::Rect {
        let points = [
            self.map(rect.x, rect.y),
            self.map(rect.x + rect.width, rect.y),
            self.map(rect.x, rect.y + rect.height),
            self.map(rect.x + rect.width, rect.y + rect.height),
        ];
        let left = points.iter().map(|point| point.0).fold(f32::INFINITY, f32::min);
        let top = points.iter().map(|point| point.1).fold(f32::INFINITY, f32::min);
        let right = points
            .iter()
            .map(|point| point.0)
            .fold(f32::NEG_INFINITY, f32::max);
        let bottom = points
            .iter()
            .map(|point| point.1)
            .fold(f32::NEG_INFINITY, f32::max);
        crate::Rect {
            x: left,
            y: top,
            width: (right - left).max(0.0),
            height: (bottom - top).max(0.0),
        }
    }

    fn tiny_skia(self) -> Transform {
        Transform::from_row(self.a, self.b, self.c, self.d, self.e, self.f)
    }

    fn is_identity(self) -> bool {
        (self.a - 1.0).abs() < f32::EPSILON
            && self.b.abs() < f32::EPSILON
            && self.c.abs() < f32::EPSILON
            && (self.d - 1.0).abs() < f32::EPSILON
            && self.e.abs() < f32::EPSILON
            && self.f.abs() < f32::EPSILON
    }
}

/// Accumulate rotation/scale matrices through transformed subtrees, storing
/// entries only for raster `<img>` descendants. Translation comes from the
/// per-shot visual state so the document-space layout remains reusable.
fn collect_projected_image_transforms(
    tree: &DomTree,
    layout: &crate::DomLayout,
    scroll_state: &ScrollPaintState,
) -> std::collections::HashMap<obscura_dom::tree::NodeId, ImageAffine> {
    if !layout.styles.values().any(|style| {
        style.transform_projection.is_some() || style.transform_scale.is_some()
    }) {
        return std::collections::HashMap::new();
    }

    fn walk(
        tree: &DomTree,
        layout: &crate::DomLayout,
        scroll_state: &ScrollPaintState,
        id: obscura_dom::tree::NodeId,
        parent: ImageAffine,
        active: bool,
        out: &mut std::collections::HashMap<obscura_dom::tree::NodeId, ImageAffine>,
    ) {
        let style = layout.styles.get(&id);
        let own_active = style.is_some_and(|style| {
            style.transform_projection.is_some() || style.transform_scale.is_some()
        });
        let mut combined = parent;
        if own_active {
            let style = style.unwrap();
            let projection = style
                .transform_projection
                .unwrap_or([1.0, 0.0, 0.0, 1.0]);
            let (scale_x, scale_y) = style.transform_scale.unwrap_or((1.0, 1.0));
            let linear = [
                scale_x * projection[0],
                scale_y * projection[1],
                scale_x * projection[2],
                scale_y * projection[3],
            ];
            if let Some(rect) = layout.rects.get(&id) {
                let offset = scroll_state.translation_for(layout, id);
                let (origin_x, origin_y) = style.transform_origin.unwrap_or((
                    crate::Dimension::Percent(0.5),
                    crate::Dimension::Percent(0.5),
                ));
                let origin = (
                    rect.x + offset.0 + crate::dom::resolve_translate(origin_x, rect.width),
                    rect.y + offset.1 + crate::dom::resolve_translate(origin_y, rect.height),
                );
                combined = parent.then(ImageAffine::around(origin, linear));
            }
        }

        let transformed = active || own_active;
        if transformed
            && !combined.is_identity()
            && tree.get_node(id).is_some_and(|node| {
                node.as_element()
                    .is_some_and(|element| element.local.as_ref() == "img")
            })
        {
            out.insert(id, combined);
        }
        for child in tree.children(id) {
            walk(
                tree,
                layout,
                scroll_state,
                child,
                combined,
                transformed,
                out,
            );
        }
    }

    let mut out = std::collections::HashMap::new();
    walk(
        tree,
        layout,
        scroll_state,
        tree.document(),
        ImageAffine::IDENTITY,
        false,
        &mut out,
    );
    out
}

/// A closed rounded-rectangle path, corners approximated by quadratic curves
/// (visually indistinguishable from true arcs at typical UI radii). The
/// horizontal and vertical radii are scaled together when necessary, matching
/// CSS's overlap rule while preserving percentage ellipses.
fn rounded_rect_path(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    rx: f32,
    ry: f32,
) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let mut rx = rx.max(0.0);
    let mut ry = ry.max(0.0);
    if rx <= f32::EPSILON || ry <= f32::EPSILON {
        let mut pb = PathBuilder::new();
        pb.push_rect(Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }
    let scale = 1.0f32.min(w / (2.0 * rx)).min(h / (2.0 * ry));
    rx *= scale;
    ry *= scale;
    let mut pb = PathBuilder::new();
    pb.move_to(x + rx, y);
    pb.line_to(x + w - rx, y);
    pb.quad_to(x + w, y, x + w, y + ry);
    pb.line_to(x + w, y + h - ry);
    pb.quad_to(x + w, y + h, x + w - rx, y + h);
    pb.line_to(x + rx, y + h);
    pb.quad_to(x, y + h, x, y + h - ry);
    pb.line_to(x, y + ry);
    pb.quad_to(x, y, x + rx, y);
    pb.close();
    pb.finish()
}

/// Paint an outset `box-shadow` layer behind the element's own box. `rect` is
/// the element's (translate-adjusted) border box; the shadow is that box offset
/// by (offset_x, offset_y), expanded by `spread`, with a `blur`-wide soft edge.
/// tiny-skia has no gaussian blur, so the blur is approximated by nested
/// rounded rects from a solid core out to the blur radius, each at a fraction of
/// the shadow alpha so source-over accumulation ramps the coverage from full at
/// the core to near-zero at the outer edge. `inset` shadows are parsed but not
/// painted (an inner shadow needs a hole-punched fill this box model does not
/// build). `clip`, when set, is the ancestor `overflow: hidden` region and is
/// applied as a mask so the shadow is clipped like the element itself.
fn paint_box_shadow(
    pixmap: &mut Pixmap,
    shadow: &crate::BoxShadow,
    rect: &crate::Rect,
    border_radius: crate::BorderRadius,
    clip: Option<crate::Rect>,
) {
    if shadow.inset || shadow.color[3] == 0 {
        return;
    }
    let spread = shadow.spread;
    let x0 = rect.x + shadow.offset_x - spread;
    let y0 = rect.y + shadow.offset_y - spread;
    let w0 = rect.width + 2.0 * spread;
    let h0 = rect.height + 2.0 * spread;
    if w0 <= 0.0 || h0 <= 0.0 {
        return;
    }
    let radius = border_radius.resolve(rect.width, rect.height);
    let rx0 = (radius.0 + spread).max(0.0);
    let ry0 = (radius.1 + spread).max(0.0);
    let blur = shadow.blur.max(0.0);
    // Ancestor overflow clip: build a mask once and reuse it for every layer.
    let mask = match clip {
        Some(c) => {
            if c.width <= 0.0 || c.height <= 0.0 {
                return;
            }
            box_clip_mask(pixmap.width(), pixmap.height(), &c)
        }
        None => None,
    };
    let color = shadow.color;
    if blur < 0.5 {
        // No blur: a single crisp, offset (and spread) rounded rect.
        fill_shadow_rect(pixmap, x0, y0, w0, h0, rx0, ry0, color, mask.as_ref());
        return;
    }
    let steps: u32 = (blur.ceil() as u32).clamp(2, 24);
    // Per-layer alpha chosen so `steps` source-over composites reach the target
    // alpha at the core: 1 - (1 - a)^steps == A  =>  a = 1 - (1 - A)^(1/steps).
    let a_frac = color[3] as f32 / 255.0;
    let per = 1.0 - (1.0 - a_frac).powf(1.0 / steps as f32);
    let layer_alpha = (per * 255.0).round().clamp(1.0, 255.0) as u8;
    let layer_color = [color[0], color[1], color[2], layer_alpha];
    for j in 0..steps {
        // j = 0 is the solid core (expansion 0); j = steps-1 reaches the blur
        // radius. Larger rects paint first, smaller (more-covered) ones on top.
        let e = blur * (j as f32) / ((steps - 1) as f32);
        fill_shadow_rect(
            pixmap,
            x0 - e,
            y0 - e,
            w0 + 2.0 * e,
            h0 + 2.0 * e,
            rx0 + e,
            ry0 + e,
            layer_color,
            mask.as_ref(),
        );
    }
}

/// Fill one (possibly rounded) shadow rectangle with a flat color, optionally
/// masked to an ancestor clip region. A helper for `paint_box_shadow`'s layers.
fn fill_shadow_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius_x: f32,
    radius_y: f32,
    color: [u8; 4],
    mask: Option<&tiny_skia::Mask>,
) {
    if w <= 0.0 || h <= 0.0 || color[3] == 0 {
        return;
    }
    let path = if radius_x > 0.5 && radius_y > 0.5 {
        match rounded_rect_path(x, y, w, h, radius_x, radius_y) {
            Some(p) => p,
            None => return,
        }
    } else {
        let r = match Rect::from_xywh(x, y, w, h) {
            Some(r) => r,
            None => return,
        };
        let mut pb = PathBuilder::new();
        pb.push_rect(r);
        match pb.finish() {
            Some(p) => p,
            None => return,
        }
    };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), mask);
}

/// The marker text for a list item, or `None` when markers are suppressed
/// (`list-style: none`). `Decimal` numbers the item by its position among
/// sibling list items so `<ol>`s count 1, 2, 3.
fn list_marker_text(tree: &DomTree, nid: obscura_dom::tree::NodeId, style: Option<crate::ListStyle>) -> Option<String> {
    match style {
        Some(crate::ListStyle::Disc) => Some("\u{2022}".to_string()),
        Some(crate::ListStyle::Circle) => Some("\u{25E6}".to_string()),
        Some(crate::ListStyle::Square) => Some("\u{25AA}".to_string()),
        Some(crate::ListStyle::Decimal) => {
            let mut n = 1usize;
            let mut cur = tree.get_node(nid).and_then(|node| node.prev_sibling);
            while let Some(sib) = cur {
                if tree.get_node(sib).and_then(|s| s.as_element().map(|e| e.local.to_string())).as_deref() == Some("li") {
                    n += 1;
                }
                cur = tree.get_node(sib).and_then(|s| s.prev_sibling);
            }
            Some(format!("{}.", n))
        }
        Some(crate::ListStyle::None) | None => None,
    }
}

fn selected_option_label(
    tree: &DomTree,
    select: obscura_dom::tree::NodeId,
) -> Option<String> {
    let mut first = None;
    for option_id in tree.descendants(select) {
        let Some(option) = tree.get_node(option_id) else { continue };
        if option
            .as_element()
            .map_or(true, |name| name.local.as_ref() != "option")
        {
            continue;
        }
        let label = option
            .get_attribute("label")
            .map(str::to_owned)
            .unwrap_or_else(|| tree.text_content(option_id).trim().to_string());
        if first.is_none() {
            first = Some(label.clone());
        }
        if option.get_attribute("selected").is_some() {
            return Some(label);
        }
    }
    first
}

/// Render `tree` at `viewport` to PNG bytes (RGBA 8-bit). Returns None if the
/// viewport is zero-sized. Convenience over `paint_dom` + `encode_png`.
pub fn screenshot_png(tree: &DomTree, viewport: (f32, f32), base_url: Option<&str>) -> Option<Vec<u8>> {
    paint_dom(tree, viewport, base_url)?.encode_png().ok()
}

/// PNG convenience wrapper for a scrolled root viewport.
pub fn screenshot_png_scrolled(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    scroll: (f32, f32),
) -> Option<Vec<u8>> {
    paint_dom_scrolled(tree, viewport, base_url, scroll)
        .and_then(|pixmap| pixmap.encode_png().ok())
}

/// PNG convenience wrapper for a retained resource-aware layout.
pub fn screenshot_prepared(
    tree: &DomTree,
    prepared: &mut PreparedRender,
    resources: &mut RenderResourceCache,
    scroll: (f32, f32),
) -> Option<Vec<u8>> {
    paint_prepared(tree, prepared, resources, scroll)?
        .encode_png()
        .ok()
}

/// A representative visible color for `background-clip: text` text whose own
/// color is transparent, used on the word-split paint path (the cosmic-text IFC
/// path samples the gradient per glyph in `inline`). Returns the gradient's mid
/// stop or the background color so a transparent-colored label still paints;
/// `None` when the element is not a transparent-text clip-to-text box.
fn clip_text_fill_color(style: &crate::LayoutStyle) -> Option<[u8; 4]> {
    if !style.background_clip_text {
        return None;
    }
    if style.color.map(|c| c[3] != 0).unwrap_or(true) {
        return None;
    }
    if let Some((_, stops)) = &style.background_gradient {
        if !stops.is_empty() {
            let mid = stops[stops.len() / 2].0;
            return Some([mid[0], mid[1], mid[2], 255]);
        }
    }
    style.background_color.filter(|c| c[3] != 0).map(|c| [c[0], c[1], c[2], 255])
}

/// Paint every word of a text node at its own laid-out position. A text node
/// lays out as one taffy leaf per word (see `dom::build_text_words`), each
/// wrapping independently, so its content is a list of (box, word) pairs
/// rather than one box for the whole node; color/font/clip come from the
/// parent element and are the same for every word.
fn paint_text_node(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    laid: &crate::DomLayout,
    scroll_state: &ScrollPaintState,
    pixmap: &mut Pixmap,
) -> Option<()> {
    let runs = laid.text_runs.get(&nid)?;
    let node = tree.get_node(nid)?;
    let parent = node.parent?;
    let style = laid.styles.get(&parent)?;
    if style.effectively_invisible {
        return Some(());
    }
    let color = clip_text_fill_color(style).unwrap_or_else(|| style.color.unwrap_or([0, 0, 0, 255]));
    let fsize = style.font_size.unwrap_or(16.0);
    let is_bold = crate::style::used_font_weight(style) >= 600;
    // A text node has no transform of its own, but any transformed element
    // ancestor offsets it (the accumulation covers text nodes too). The clip
    // receives root-scroll/sticky movement without following the descendant's
    // own transform.
    let (ox, oy) = scroll_state.translation_for(laid, nid);
    let clip = scroll_state.clip_for(laid, nid);

    for (rect, word) in runs {
        draw_text(
            pixmap,
            word,
            rect.x + ox,
            rect.y + oy,
            color,
            fsize,
            is_bold,
            style.font_family.as_deref(),
            style.letter_spacing.unwrap_or(0.0),
            clip,
        );
    }
    Some(())
}

fn fallback_font_bytes(family: Option<&str>) -> &'static [u8] {
    let Some(family) = family else { return FONT_BYTES };
    for token in family.split(',') {
        let token = token
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_ascii_lowercase();
        if token == "system-ui" || token == "ui-sans-serif" {
            return SYSTEM_FONT_BYTES;
        }
        if token == "monospace"
            || token.contains("mono")
            || token.contains("courier")
            || token.contains("consol")
            || token == "menlo"
            || token == "monaco"
            || token == "code"
        {
            return MONO_FONT_BYTES;
        }
        if token == "serif"
            || token == "georgia"
            || token.contains("times")
            || token == "cambria"
            || token.contains("garamond")
            || token.contains("liberation serif")
            || token == "roman"
        {
            return SERIF_FONT_BYTES;
        }
        if token == "sans-serif"
            || token.contains("sans")
            || token == "arial"
            || token == "helvetica"
            || token == "helvetica neue"
            || token == "-apple-system"
            || token == "roboto"
            || token == "segoe ui"
            || token == "inter"
            || token == "verdana"
            || token == "tahoma"
        {
            return FONT_BYTES;
        }
    }
    FONT_BYTES
}

pub fn measure_text(text: &str, size: f32, is_bold: bool, family: Option<&str>) -> f32 {
    let font = FontRef::try_from_slice(fallback_font_bytes(family)).unwrap();
    let scale = PxScale::from(size);
    let scaled_font = font.as_scaled(scale);
    let mut width = 0.0;
    for c in text.chars() {
        if c.is_control() { continue; }
        width += scaled_font.h_advance(font.glyph_id(c));
    }
    if is_bold { width += text.chars().filter(|c| !c.is_control()).count() as f32; }
    width
}

fn draw_text(
    pixmap: &mut Pixmap,
    text: &str,
    x: f32,
    y: f32,
    color: [u8; 4],
    size: f32,
    is_bold: bool,
    family: Option<&str>,
    letter_spacing: f32,
    clip: Option<crate::Rect>,
) {
    // A fully clipped-away run (the common "visually hidden" accessibility
    // pattern: a 1x1 box with overflow: hidden) paints nothing at all.
    if let Some(c) = clip {
        if c.width <= 0.0 || c.height <= 0.0 {
            return;
        }
    }
    let font = FontRef::try_from_slice(fallback_font_bytes(family)).unwrap();
    let scale = PxScale::from(size);
    let scaled_font = font.as_scaled(scale);
    let mut caret = ab_glyph::point(x, y + scaled_font.ascent());

    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    let clip_bounds = clip.map(|c| (c.x, c.y, c.x + c.width, c.y + c.height));
    let pixels = pixmap.pixels_mut();
    let (r, g, b, a_full) = (color[0], color[1], color[2], color[3]);

    for c in text.chars() {
        if c.is_control() {
            continue;
        }
        let glyph_id = font.glyph_id(c);
        let id = glyph_id;
        let glyph = glyph_id.with_scale_and_position(scale, caret);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, c| {
                let px = (bounds.min.x + gx as f32) as i32;
                let py = (bounds.min.y + gy as f32) as i32;
                if let Some((cx0, cy0, cx1, cy1)) = clip_bounds {
                    if (px as f32) < cx0 || (px as f32) >= cx1 || (py as f32) < cy0 || (py as f32) >= cy1 {
                        return;
                    }
                }
                if px >= 0 && px < width && py >= 0 && py < height {
                    let alpha = (a_full as f32 * c) as u8;
                    if alpha > 0 {
                        let mut px_indices = vec![(py * width + px) as usize];
                        if is_bold && px + 1 < width {
                            px_indices.push((py * width + px + 1) as usize);
                        }
                        for idx in px_indices {
                            let dst = pixels[idx];
                            
                            let src_a = alpha as u32;
                            let src_r = (r as u32 * src_a) / 255;
                            let src_g = (g as u32 * src_a) / 255;
                            let src_b = (b as u32 * src_a) / 255;
                            
                            let dst_a = dst.alpha() as u32;
                            let out_a = src_a + (dst_a * (255 - src_a) / 255);
                            
                            if out_a > 0 {
                                let out_r = src_r + (dst.red() as u32 * (255 - src_a) / 255);
                                let out_g = src_g + (dst.green() as u32 * (255 - src_a) / 255);
                                let out_b = src_b + (dst.blue() as u32 * (255 - src_a) / 255);
                                
                                pixels[idx] = tiny_skia::PremultipliedColorU8::from_rgba(
                                    out_r as u8, out_g as u8, out_b as u8, out_a as u8
                                ).unwrap_or_else(|| tiny_skia::PremultipliedColorU8::from_rgba(0,0,0,0).unwrap());
                            }
                        }
                    }
                }
            });
            // Matches measure_text's +1px-per-character bold compensation:
            // without it, a word's reserved layout width (from measure_text)
            // is wider than what draw_text actually advances through, and
            // the difference shows up as a visible gap after every word once
            // each word is its own independently-positioned box.
            caret.x += scaled_font.h_advance(id)
                + if is_bold { 1.0 } else { 0.0 }
                + letter_spacing;
        } else {
            caret.x += scaled_font.h_advance(id)
                + if is_bold { 1.0 } else { 0.0 }
                + letter_spacing;
        }
    }
}

/// Resolve `src` (a `data:` URI, or an absolute/relative URL against
/// `base_url`) to raw bytes, fetching over the network at most once per
/// distinct URL through the retained resource cache.
fn fetch_bytes(
    src: &str,
    base_url: Option<&str>,
    cache: &mut RenderResourceCache,
) -> Option<Arc<[u8]>> {
    if let Some(rest) = src.strip_prefix("data:") {
        let comma_idx = rest.find(',')?;
        let (meta, data) = (&rest[..comma_idx], &rest[comma_idx + 1..]);
        // Data-backed SVGs and web fonts may be base64 or percent-escaped.
        // Decode from the encoding label rather than assuming every data URI
        // is base64.
        let bytes = if meta.contains("base64") {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(data).ok()
        } else {
            Some(percent_decode(data))
        }?;
        return Some(Arc::from(bytes));
    }
    let resolved = resolve_resource_url(src, base_url)?;
    cache.get_or_load(&resolved)
}

fn resolve_resource_url(src: &str, base_url: Option<&str>) -> Option<String> {
    if src.starts_with("data:") {
        return Some(src.to_string());
    }
    // Resolve relative to the document's base URL: the overwhelming majority
    // of real markup uses relative image paths ("logo.svg", not
    // "https://example.com/logo.svg"), so without this every relative <img>
    // or mask/background reference silently fails to fetch.
    if src.starts_with("http://") || src.starts_with("https://") {
        Some(src.to_string())
    } else if let Some(rest) = src.strip_prefix("//") {
        // Protocol-relative URL (`//upload.wikimedia.org/...`, ubiquitous on
        // Wikipedia and CDN-hosted media): inherit the document scheme, but
        // never `file:`/other non-network schemes (a `file://` base would give
        // `file://host/...` and fail), so default to https for those.
        let scheme = base_url
            .and_then(|b| url::Url::parse(b).ok())
            .map(|u| u.scheme().to_string())
            .filter(|s| s == "http" || s == "https")
            .unwrap_or_else(|| "https".to_string());
        Some(format!("{scheme}://{rest}"))
    } else {
        base_url
            .and_then(|b| url::Url::parse(b).ok())
            .and_then(|base| base.join(src).ok())
            .map(|u| u.to_string())
    }
}

/// Fetch the Latin/ASCII face from each authored `@font-face` rule and decode
/// WOFF/WOFF2 into the sfnt bytes consumed by fontdb/cosmic-text. Unicode-range
/// filtering is load-bearing for performance: generated font packages commonly
/// emit six or seven script subsets per face, while an English page needs only
/// the subset containing ASCII.
fn collect_web_fonts(
    tree: &DomTree,
    base_url: Option<&str>,
    cache: &mut RenderResourceCache,
    dynamic_fonts: &[DynamicFontFace],
) -> Vec<crate::inline::WebFont> {
    let mut seen = std::collections::HashSet::new();
    let mut fonts = Vec::new();
    let mut rules = Vec::new();

    for nid in tree.descendants(tree.document()) {
        let Some(node) = tree.get_node(nid) else { continue };
        if node
            .as_element()
            .map(|element| element.local.as_ref() != "style")
            .unwrap_or(true)
        {
            continue;
        }
        let css = tree.text_content(nid);
        for face in font_face_blocks(&css) {
            if !font_face_covers_ascii(face) {
                continue;
            }
            let Some(src) = font_face_urls(face).into_iter().next() else {
                continue;
            };
            rules.push((
                font_resource_key(&src, base_url),
                src,
                font_face_family(face),
                font_face_weight(face),
                font_face_italic(face),
            ));
        }
    }
    for face in dynamic_fonts {
        let descriptor_block = format!(
            "src:{};font-weight:{};font-style:{};unicode-range:{}",
            face.source, face.weight, face.style, face.unicode_range
        );
        if !font_face_covers_ascii(&descriptor_block) {
            continue;
        }
        let Some(src) = font_face_urls(&descriptor_block).into_iter().next() else {
            continue;
        };
        rules.push((
            font_resource_key(&src, base_url),
            src,
            (!face.family.is_empty()).then(|| face.family.clone()),
            font_face_weight(&descriptor_block),
            font_face_italic(&descriptor_block),
        ));
    }

    // Critical web fonts are normally preloaded from the document with a URL
    // already resolved relative to the HTML. Fetch those first, while retaining
    // the matching @font-face descriptors needed for CSS family/weight lookup.
    let mut preloads = Vec::new();
    for nid in tree.descendants(tree.document()) {
        let Some(node) = tree.get_node(nid) else { continue };
        if node
            .as_element()
            .map(|element| element.local.as_ref() != "link")
            .unwrap_or(true)
        {
            continue;
        }
        let rel = node.get_attribute("rel").unwrap_or("");
        let as_value = node.get_attribute("as").unwrap_or("");
        if rel
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("preload"))
            && as_value.eq_ignore_ascii_case("font")
        {
            if let Some(href) = node.get_attribute("href") {
                preloads.push(href.to_string());
            }
        }
    }
    for src in preloads.iter().take(16) {
        let key = font_resource_key(src, base_url);
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(decoded) = fetch_and_decode_font(src, base_url, cache) {
            let metadata = rules.iter().find(|rule| rule.0 == key);
            fonts.push(crate::inline::WebFont {
                data: decoded,
                family: metadata.and_then(|rule| rule.2.clone()),
                weight: metadata.and_then(|rule| rule.3),
                italic: metadata.and_then(|rule| rule.4),
            });
        }
    }

    for (key, src, family, weight, italic) in rules {
        if fonts.len() >= 16 {
            break;
        }
        if !seen.insert(key) {
            continue;
        }
        if let Some(decoded) = fetch_and_decode_font(&src, base_url, cache) {
            fonts.push(crate::inline::WebFont {
                data: decoded,
                family,
                weight,
                italic,
            });
        }
    }
    fonts
}

fn font_resource_key(src: &str, base_url: Option<&str>) -> String {
    url::Url::parse(src)
        .ok()
        .or_else(|| {
            base_url
                .and_then(|base| url::Url::parse(base).ok())
                .and_then(|base| base.join(src).ok())
        })
        .map(|url| url.to_string())
        .unwrap_or_else(|| src.to_string())
}

fn fetch_and_decode_font(
    src: &str,
    base_url: Option<&str>,
    cache: &mut RenderResourceCache,
) -> Option<Vec<u8>> {
    let compressed = fetch_bytes(src, base_url, cache)?;
    if compressed.len() > 8 * 1024 * 1024 {
        return None;
    }
    let decoded = match compressed.get(..4) {
        Some(b"wOF2") => wuff::decompress_woff2(&compressed).ok(),
        Some(b"wOFF") => wuff::decompress_woff1(&compressed).ok(),
        // TrueType/OpenType collections and raw sfnt fonts already have the
        // representation fontdb expects.
        Some(b"\0\x01\0\0" | b"OTTO" | b"ttcf") => Some(compressed.as_ref().to_vec()),
        _ => None,
    }?;
    (decoded.len() <= 32 * 1024 * 1024).then_some(decoded)
}

fn font_face_blocks(css: &str) -> Vec<&str> {
    let lower = css.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("@font-face") {
        let at = cursor + relative;
        let Some(open_relative) = lower[at..].find('{') else {
            break;
        };
        let open = at + open_relative;
        let mut depth = 1i32;
        let mut quote = None;
        let mut escaped = false;
        let mut close = None;
        for (offset, ch) in css[open + 1..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if let Some(active) = quote {
                if ch == active {
                    quote = None;
                }
                continue;
            }
            if matches!(ch, '"' | '\'') {
                quote = Some(ch);
                continue;
            }
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + 1 + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { break };
        out.push(&css[open + 1..close]);
        cursor = close + 1;
    }
    out
}

fn font_face_declaration<'a>(face: &'a str, name: &str) -> Option<&'a str> {
    split_css_top_level(face, ';')
        .into_iter()
        .find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
            property
                .trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
    })
}

fn font_face_family(face: &str) -> Option<String> {
    font_face_declaration(face, "font-family")
        .map(|family| family.trim().trim_matches(|ch| matches!(ch, '"' | '\'')).to_string())
        .filter(|family| !family.is_empty())
}

fn font_face_weight(face: &str) -> Option<(u16, u16)> {
    fn parse(value: &str) -> Option<u16> {
        match value.to_ascii_lowercase().as_str() {
            "normal" => Some(400),
            "bold" => Some(700),
            value => value
                .parse::<f32>()
                .ok()
                .filter(|weight| weight.is_finite() && (1.0..=1000.0).contains(weight))
                .map(|weight| weight.round() as u16),
        }
    }
    let mut values = font_face_declaration(face, "font-weight")?
        .split_ascii_whitespace()
        .filter_map(parse);
    let first = values.next()?;
    let second = values.next().unwrap_or(first);
    Some((first.min(second), first.max(second)))
}

fn font_face_italic(face: &str) -> Option<bool> {
    font_face_declaration(face, "font-style").and_then(|style| {
        let style = style.trim().to_ascii_lowercase();
        if style == "normal" {
            Some(false)
        } else if style == "italic" || style.starts_with("oblique") {
            Some(true)
        } else {
            None
        }
    })
}

fn font_face_covers_ascii(face: &str) -> bool {
    let Some(range) = font_face_declaration(face, "unicode-range") else {
        return true;
    };
    range.split(',').any(|part| {
        let token = part.trim().to_ascii_lowercase();
        let Some(value) = token.strip_prefix("u+") else {
            return false;
        };
        let (start, end) = if value.contains('?') {
            (
                u32::from_str_radix(&value.replace('?', "0"), 16).ok(),
                u32::from_str_radix(&value.replace('?', "f"), 16).ok(),
            )
        } else if let Some((start, end)) = value.split_once('-') {
            (
                u32::from_str_radix(start, 16).ok(),
                u32::from_str_radix(end, 16).ok(),
            )
        } else {
            let point = u32::from_str_radix(value, 16).ok();
            (point, point)
        };
        matches!((start, end), (Some(start), Some(end)) if start <= 0x7e && end >= 0x20)
    })
}

fn font_face_urls(face: &str) -> Vec<String> {
    let Some(src) = font_face_declaration(face, "src") else {
        return Vec::new();
    };
    let lower = src.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("url(") {
        let start = cursor + relative + 4;
        let Some(end_relative) = src[start..].find(')') else {
            break;
        };
        let end = start + end_relative;
        let value = src[start..end]
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'')
            .trim();
        if !value.is_empty() {
            out.push(value.to_string());
        }
        cursor = end + 1;
    }
    out
}

fn split_css_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '"' | '\'') {
            quote = Some(ch);
            continue;
        }
        match ch {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ch if ch == separator && depth == 0 => {
                out.push(&value[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

/// Fetch `url` with a descriptive User-Agent and a bounded timeout, retrying on
/// rate-limit / transient errors with backoff. Real pages pull dozens of images
/// from one CDN in a burst (a Wikipedia article references ~60); hosts like
/// Wikimedia answer a rapid burst with HTTP 429 after ~10 requests. Without a
/// retry the rate-limited images (e.g. an infobox photo montage fetched late in
/// the burst) came back blank, and the failure was cached permanently. The
/// backoff both recovers them and paces the burst back under the limit.
fn http_get_bytes(url: &str) -> Option<Vec<u8>> {
    let mut backoff = std::time::Duration::from_millis(200);
    for attempt in 0..3 {
        // Advertise only formats that this build can decode. Content-negotiating
        // CDNs otherwise commonly choose AVIF and leave the image blank.
        let res = image_agent()
            .get(url)
            .set("Accept", IMAGE_ACCEPT)
            .call();
        match res {
            Ok(resp) => {
                let mut buf = Vec::new();
                use std::io::Read;
                return resp.into_reader().read_to_end(&mut buf).ok().map(|_| buf);
            }
            // 429 (rate limit) and 5xx are transient: a short backoff clears a
            // brief blip. A sustained limit (Wikimedia 429s a 60-image burst
            // from a datacenter IP hard, with `Retry-After: 1`) is NOT worth
            // waiting out here: honoring the hint stalls the whole render for
            // minutes, so fast-fail to the grey placeholder instead. Real
            // fidelity for that case needs an HTTP/2 image client (multiplexing
            // like Chrome), not blocking retries.
            Err(ureq::Error::Status(code, _)) if matches!(code, 429 | 500 | 502 | 503 | 504) && attempt < 2 => {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(ureq::Error::Transport(_)) if attempt < 2 => {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(_) => return None,
        }
    }
    None
}

/// One shared HTTP agent for all image fetches in the process, with a browser
/// User-Agent and keep-alive connection pooling. A CDN's bot rate-limiter keys
/// on connection churn as much as on rate: a fresh TLS handshake per image (the
/// old per-call `ureq::get`) reads as a burst and gets 429'd, whereas reusing
/// one pooled connection to the same host (as a browser does) both avoids most
/// throttling and is much faster on an image-heavy page.
fn image_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            // Present the same normal browser identity the engine uses for the
            // document. A bot-identifying UA got image requests filtered by CDNs
            // that gate on User-Agent (Akamai/Cloudflare image endpoints on
            // cnbc, techcrunch, arstechnica), so the images Chrome loads came
            // back blank; a real browser UA loads the same bytes Chrome does.
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
            .build()
    })
}

/// Decode a percent-escaped data: URI payload (`%23` -> `#`, etc). Bytes that
/// are not part of a `%XX` escape pass through unchanged, which is exactly
/// right for the inline-SVG case: only the characters that would otherwise be
/// ambiguous in a URI (`#`, `"`, ...) get escaped, everything else is literal
/// UTF-8 text.
fn percent_decode(s: &str) -> Vec<u8> {
    // Operates on raw bytes throughout (never slices `s` as a string): a
    // stray '%' followed by non-hex bytes could otherwise land a string
    // slice in the middle of a multi-byte UTF-8 character and panic.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Decode raster image bytes (GIF/JPEG/PNG/WebP) to a premultiplied-alpha pixmap
/// resized to `w`x`h`.
fn raster_to_pixmap(bytes: &[u8], w: u32, h: u32) -> Option<Pixmap> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let resized = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    let mut raw = resized.into_raw();
    for pixel in raw.chunks_exact_mut(4) {
        let a = pixel[3] as u32;
        pixel[0] = ((pixel[0] as u32 * a) / 255) as u8;
        pixel[1] = ((pixel[1] as u32 * a) / 255) as u8;
        pixel[2] = ((pixel[2] as u32 * a) / 255) as u8;
    }
    let size = tiny_skia::IntSize::from_wh(w, h)?;
    Pixmap::from_vec(raw, size)
}

/// Read an image's intrinsic pixel dimensions from its header only, without
/// decoding the whole thing. Returns None for formats the raster decoder does
/// not recognize (e.g. SVG, which is sized elsewhere).
/// Fill `path` with a CSS `linear-gradient`. `angle` is degrees clockwise from
/// 12 o'clock (0 = to top). The gradient line length uses the CSS formula so
/// the stops land where a browser puts them. Positionless stops are spread
/// evenly; positions are clamped monotonic (tiny-skia requires ascending).
fn paint_linear_gradient(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    rect: &crate::Rect,
    angle: f32,
    stops: &[([u8; 4], Option<f32>)],
    clip: Option<&tiny_skia::Mask>,
) {
    if stops.len() < 2 {
        return;
    }
    let rad = angle.to_radians();
    let dx = rad.sin();
    let dy = -rad.cos();
    let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let half = (dx.abs() * rect.width + dy.abs() * rect.height) / 2.0;
    let start = Point::from_xy(cx - dx * half, cy - dy * half);
    let end = Point::from_xy(cx + dx * half, cy + dy * half);
    let n = stops.len();
    let mut gs: Vec<GradientStop> = Vec::with_capacity(n);
    let mut last = 0.0f32;
    for (i, (_, pos)) in stops.iter().enumerate() {
        let c = gradient_stop_color(stops, i);
        let p = pos.unwrap_or(i as f32 / (n - 1) as f32).clamp(0.0, 1.0).max(last);
        last = p;
        gs.push(GradientStop::new(p, Color::from_rgba8(c[0], c[1], c[2], c[3])));
    }
    if let Some(shader) = LinearGradient::new(start, end, gs, SpreadMode::Pad, Transform::identity()) {
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), clip);
    }
}

fn paint_linear_gradient_layer(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    rect: &crate::Rect,
    angle: f32,
    stops: &[([u8; 4], Option<f32>)],
    stop_positions: &[Option<String>],
    repeating: bool,
    em: f32,
    rem: f32,
    viewport: (f32, f32),
    clip: Option<&tiny_skia::Mask>,
) {
    if stops.len() < 2 {
        return;
    }
    let rad = angle.to_radians();
    let dx = rad.sin();
    let dy = -rad.cos();
    let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
    let half = (dx.abs() * rect.width + dy.abs() * rect.height) / 2.0;
    let line_length = half * 2.0;
    if line_length <= f32::EPSILON {
        return;
    }
    let base_start = Point::from_xy(cx - dx * half, cy - dy * half);
    let base_end = Point::from_xy(cx + dx * half, cy + dy * half);
    let mut positions: Vec<Option<f32>> = stops
        .iter()
        .enumerate()
        .map(|(index, (_, legacy))| {
            stop_positions
                .get(index)
                .and_then(Option::as_deref)
                .and_then(|value| {
                    crate::style::resolve_contextual_length(
                        value,
                        em,
                        rem,
                        viewport.0 / 100.0,
                        viewport.1 / 100.0,
                        line_length,
                    )
                    .map(|pixels| pixels / line_length)
                })
                .or(*legacy)
        })
        .collect();
    if positions.first().is_some_and(Option::is_none) {
        positions[0] = Some(0.0);
    }
    let last_index = positions.len() - 1;
    if positions[last_index].is_none() {
        positions[last_index] = Some(1.0);
    }
    let mut previous = 0usize;
    for index in 1..positions.len() {
        let Some(mut position) = positions[index] else {
            continue;
        };
        let previous_position = positions[previous].unwrap_or(0.0);
        position = position.max(previous_position);
        positions[index] = Some(position);
        let gap = index - previous;
        for offset in 1..gap {
            positions[previous + offset] = Some(
                previous_position + (position - previous_position) * offset as f32 / gap as f32,
            );
        }
        previous = index;
    }
    let first = positions[0].unwrap_or(0.0);
    let last = positions[last_index].unwrap_or(first);
    if repeating && last - first <= 1e-6 {
        let color = gradient_stop_color(stops, last_index);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), clip);
        return;
    }
    let (start, end, spread, normalize) = if repeating {
        (
            Point::from_xy(
                base_start.x + (base_end.x - base_start.x) * first,
                base_start.y + (base_end.y - base_start.y) * first,
            ),
            Point::from_xy(
                base_start.x + (base_end.x - base_start.x) * last,
                base_start.y + (base_end.y - base_start.y) * last,
            ),
            SpreadMode::Repeat,
            Some((first, last - first)),
        )
    } else {
        (base_start, base_end, SpreadMode::Pad, None)
    };
    let mut gradient_stops = Vec::with_capacity(stops.len());
    let mut monotonic = 0.0f32;
    for (index, _) in stops.iter().enumerate() {
        let color = gradient_stop_color(stops, index);
        let position = match normalize {
            Some((origin, span)) => {
                ((positions[index].unwrap_or(origin) - origin) / span).clamp(0.0, 1.0)
            }
            None => positions[index].unwrap_or(0.0).clamp(0.0, 1.0),
    }
        .max(monotonic);
        monotonic = position;
        gradient_stops.push(GradientStop::new(
            position,
            Color::from_rgba8(color[0], color[1], color[2], color[3]),
        ));
    }
    if let Some(shader) =
        LinearGradient::new(start, end, gradient_stops, spread, Transform::identity())
    {
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), clip);
    }
}

fn paint_radial_gradient(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    rect: &crate::Rect,
    center: (f32, f32),
    stops: &[([u8; 4], Option<f32>)],
    clip: Option<&tiny_skia::Mask>,
) {
    if stops.len() < 2 {
        return;
    }
    let center = Point::from_xy(
        rect.x + rect.width * center.0,
        rect.y + rect.height * center.1,
    );
    let radius = [
        (rect.x - center.x).hypot(rect.y - center.y),
        (rect.x + rect.width - center.x).hypot(rect.y - center.y),
        (rect.x - center.x).hypot(rect.y + rect.height - center.y),
        (rect.x + rect.width - center.x).hypot(rect.y + rect.height - center.y),
    ]
    .into_iter()
    .fold(0.0, f32::max);
    let normalized = normalized_stops(stops);
    let gradient_stops = normalized
        .into_iter()
        .map(|(position, color)| {
            GradientStop::new(
                position,
                Color::from_rgba8(color[0], color[1], color[2], color[3]),
            )
        })
        .collect();
    if let Some(shader) = RadialGradient::new(
        center,
        0.0,
        center,
        radius,
        gradient_stops,
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let mut paint = Paint::default();
        paint.shader = shader;
        paint.anti_alias = true;
        pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), clip);
    }
}

fn paint_background_gradient_layers(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    rect: &crate::Rect,
    border_radius: (f32, f32),
    style: &crate::LayoutStyle,
    root_font_size: f32,
    viewport: (f32, f32),
    clip: Option<&tiny_skia::Mask>,
) {
    let layers = &style.background_gradient_layers;
    let em = style.font_size.unwrap_or(16.0);
    let tile_size = background_gradient_tile_size(style, rect, em, root_font_size, viewport);
    let needs_tile =
        (tile_size.0 - rect.width).abs() > 0.01 || (tile_size.1 - rect.height).abs() > 0.01;
    if needs_tile && tile_size.0 > 0.0 && tile_size.1 > 0.0 {
        let width = tile_size.0.ceil().clamp(1.0, 4096.0) as u32;
        let height = tile_size.1.ceil().clamp(1.0, 4096.0) as u32;
        if let Some(mut tile) = Pixmap::new(width, height) {
            let tile_rect = crate::Rect {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
            };
            if let Some(tile_path) = Rect::from_xywh(0.0, 0.0, tile_rect.width, tile_rect.height)
                .and_then(|rect| {
                    let mut builder = PathBuilder::new();
                    builder.push_rect(rect);
                    builder.finish()
                })
            {
                paint_gradient_layer_stack(
                    &mut tile,
                    &tile_path,
                    &tile_rect,
                    (0.0, 0.0),
                    layers,
                    em,
                    root_font_size,
                    viewport,
                    None,
                );
                let tile_x = rect.x
                    + style
                        .background_position
                        .x
                        .resolve(rect.width - tile_size.0);
                let tile_y = rect.y
                    + style
                        .background_position
                        .y
                        .resolve(rect.height - tile_size.1);
                let repeats = style.background_repeat.unwrap_or((true, true));
                if repeats == (true, true) {
                    let mut paint = Paint::default();
                    paint.shader = Pattern::new(
                        tile.as_ref(),
                        SpreadMode::Repeat,
                        FilterQuality::Nearest,
                        1.0,
                        Transform::from_translate(tile_x, tile_y),
                    );
                    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), clip);
                    return;
                }

                let mut owner_clip = clip.cloned().or_else(|| {
                    let mut mask = tiny_skia::Mask::new(pixmap.width(), pixmap.height())?;
                    mask.fill_path(path, FillRule::Winding, true, Transform::identity());
                    Some(mask)
                });
                if clip.is_some() {
                    if let Some(mask) = owner_clip.as_mut() {
                        mask.intersect_path(path, FillRule::Winding, true, Transform::identity());
                    }
                }
                let start_x = if repeats.0 {
                    tile_x - ((tile_x - rect.x) / width as f32).ceil() * width as f32
                } else {
                    tile_x
                };
                let start_y = if repeats.1 {
                    tile_y - ((tile_y - rect.y) / height as f32).ceil() * height as f32
                } else {
                    tile_y
                };
                let end_x = if repeats.0 {
                    rect.x + rect.width
                } else {
                    tile_x + 0.5
                };
                let end_y = if repeats.1 {
                    rect.y + rect.height
                } else {
                    tile_y + 0.5
                };
                let mut y = start_y;
                while y < end_y {
                    let mut x = start_x;
                    while x < end_x {
                        pixmap.draw_pixmap(
                            x.floor() as i32,
                            y.floor() as i32,
                            tile.as_ref(),
                            &tiny_skia::PixmapPaint::default(),
                            Transform::identity(),
                            owner_clip.as_ref(),
                        );
                        if !repeats.0 {
                            break;
                        }
                        x += width as f32;
                    }
                    if !repeats.1 {
                        break;
                    }
                    y += height as f32;
                }
                return;
            }
        }
    }

    paint_gradient_layer_stack(
        pixmap,
        path,
        rect,
        border_radius,
        layers,
        em,
        root_font_size,
        viewport,
        clip,
    );
}

fn paint_gradient_layer_stack(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    rect: &crate::Rect,
    border_radius: (f32, f32),
    layers: &[crate::BackgroundGradientLayer],
    em: f32,
    root_font_size: f32,
    viewport: (f32, f32),
    clip: Option<&tiny_skia::Mask>,
) {
    // CSS lists the topmost background first. Paint back-to-front so every
    // translucent layer composites over the layers authored after it.
    for layer in layers.iter().rev() {
        match layer {
            crate::BackgroundGradientLayer::Linear {
                angle,
                stops,
                stop_positions,
                repeating,
            } => {
                paint_linear_gradient_layer(
                    pixmap,
                    path,
                    rect,
                    *angle,
                    stops,
                    stop_positions,
                    *repeating,
                    em,
                    root_font_size,
                    viewport,
                    clip,
                );
            }
            crate::BackgroundGradientLayer::Radial { center, stops } => {
                paint_radial_gradient(pixmap, path, rect, *center, stops, clip);
            }
            crate::BackgroundGradientLayer::Conic {
                angle,
                center,
                stops,
            } => {
                paint_conic_gradient(pixmap, rect, border_radius, *angle, *center, stops, clip);
            }
        }
    }
}

fn background_gradient_tile_size(
    style: &crate::LayoutStyle,
    rect: &crate::Rect,
    em: f32,
    rem: f32,
    viewport: (f32, f32),
) -> (f32, f32) {
    if matches!(
        style.background_size_fit,
        Some(crate::ObjectFit::Cover | crate::ObjectFit::Contain)
    ) {
        return (rect.width, rect.height);
    }
    if let Some(expression) = style.background_size_expression.as_deref() {
        let components = split_background_size_components(expression);
        let resolve = |value: &&str, basis: f32| {
            (!value.eq_ignore_ascii_case("auto"))
                .then(|| {
                    crate::style::resolve_contextual_length(
                        value,
                        em,
                        rem,
                        viewport.0 / 100.0,
                        viewport.1 / 100.0,
                        basis,
                    )
                })
                .flatten()
        };
        let width = components
            .first()
            .and_then(|value| resolve(value, rect.width))
            .unwrap_or(rect.width);
        let height = components
            .get(1)
            .and_then(|value| resolve(value, rect.height))
            .unwrap_or(rect.height);
        return (width, height);
    }
    style.background_size.unwrap_or((rect.width, rect.height))
}

fn paint_conic_gradient(
    pixmap: &mut Pixmap,
    rect: &crate::Rect,
    border_radius: (f32, f32),
    angle: f32,
    center: (f32, f32),
    stops: &[([u8; 4], Option<f32>)],
    extra_clip: Option<&tiny_skia::Mask>,
) {
    if rect.width <= 0.0 || rect.height <= 0.0 || stops.len() < 2 {
        return;
    }
    let width = rect.width.ceil() as u32;
    let height = rect.height.ceil() as u32;
    let Some(mut layer) = Pixmap::new(width, height) else {
        return;
    };
    let normalized = normalized_stops(stops);
    for y in 0..height {
        for x in 0..width {
            let color = conic_color_at(
                rect,
                angle,
                center,
                &normalized,
                rect.x + x as f32 + 0.5,
                rect.y + y as f32 + 0.5,
            );
            layer.pixels_mut()[(y * width + x) as usize] = premultiplied(color);
        }
    }
    let mut clip = extra_clip.cloned();
    if border_radius.0 > 0.5 && border_radius.1 > 0.5 {
        let path = rounded_rect_path(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            border_radius.0,
            border_radius.1,
        );
        match (clip.as_mut(), path) {
            (Some(mask), Some(path)) => {
                mask.intersect_path(
                    &path,
                    FillRule::Winding,
                    true,
                    Transform::identity(),
                );
            }
            (None, _) => {
                clip = rounded_box_clip_mask(
                    pixmap.width(),
                    pixmap.height(),
                    rect,
                    border_radius,
                );
            }
            _ => {}
        }
    }
    pixmap.draw_pixmap(
        rect.x.floor() as i32,
        rect.y.floor() as i32,
        layer.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::identity(),
        clip.as_ref(),
    );
}

fn normalized_stops(
    stops: &[([u8; 4], Option<f32>)],
) -> Vec<(f32, [u8; 4])> {
    let count = stops.len();
    let mut normalized = Vec::with_capacity(count);
    let mut last = 0.0f32;
    for (index, (_, position)) in stops.iter().enumerate() {
        let color = gradient_stop_color(stops, index);
        let position = position
            .unwrap_or_else(|| {
                if count <= 1 {
                    0.0
                } else {
                    index as f32 / (count - 1) as f32
                }
            })
            .clamp(0.0, 1.0)
            .max(last);
        last = position;
        normalized.push((position, color));
    }
    normalized
}

fn gradient_stop_color(
    stops: &[([u8; 4], Option<f32>)],
    index: usize,
) -> [u8; 4] {
    let color = stops[index].0;
    if color[3] != 0 {
        return color;
    }
    let neighbor = stops[index + 1..]
        .iter()
        .find(|(candidate, _)| candidate[3] != 0)
        .or_else(|| stops[..index].iter().rev().find(|(candidate, _)| candidate[3] != 0));
    neighbor
        .map(|(neighbor, _)| [neighbor[0], neighbor[1], neighbor[2], 0])
        .unwrap_or(color)
}

fn sample_normalized_stops(stops: &[(f32, [u8; 4])], t: f32) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    let Some(&(first_position, first_color)) = stops.first() else {
        return [0, 0, 0, 0];
    };
    if t <= first_position {
        return first_color;
    }
    for pair in stops.windows(2) {
        let (start_position, start_color) = pair[0];
        let (end_position, end_color) = pair[1];
        if t <= end_position {
            let span = end_position - start_position;
            let fraction = if span <= f32::EPSILON {
                1.0
            } else {
                ((t - start_position) / span).clamp(0.0, 1.0)
            };
            let interpolate = |start: u8, end: u8| {
                (start as f32 + (end as f32 - start as f32) * fraction)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            return [
                interpolate(start_color[0], end_color[0]),
                interpolate(start_color[1], end_color[1]),
                interpolate(start_color[2], end_color[2]),
                interpolate(start_color[3], end_color[3]),
            ];
        }
    }
    stops.last().map(|(_, color)| *color).unwrap_or(first_color)
}

fn conic_color_at(
    rect: &crate::Rect,
    angle: f32,
    center: (f32, f32),
    stops: &[(f32, [u8; 4])],
    x: f32,
    y: f32,
) -> [u8; 4] {
    let center_x = rect.x + rect.width * center.0;
    let center_y = rect.y + rect.height * center.1;
    let point_angle = (x - center_x)
        .atan2(-(y - center_y))
        .to_degrees()
        .rem_euclid(360.0);
    let position = (point_angle - angle).rem_euclid(360.0) / 360.0;
    sample_normalized_stops(stops, position)
}

fn linear_color_at(
    rect: &crate::Rect,
    angle: f32,
    stops: &[(f32, [u8; 4])],
    x: f32,
    y: f32,
) -> [u8; 4] {
    let radians = angle.to_radians();
    let dx = radians.sin();
    let dy = -radians.cos();
    let center_x = rect.x + rect.width / 2.0;
    let center_y = rect.y + rect.height / 2.0;
    let half = (dx.abs() * rect.width + dy.abs() * rect.height) / 2.0;
    if half <= f32::EPSILON {
        return sample_normalized_stops(stops, 0.5);
    }
    let start_x = center_x - dx * half;
    let start_y = center_y - dy * half;
    let position = ((x - start_x) * dx + (y - start_y) * dy) / (2.0 * half);
    sample_normalized_stops(stops, position)
}

fn radial_color_at(
    rect: &crate::Rect,
    center: (f32, f32),
    stops: &[(f32, [u8; 4])],
    x: f32,
    y: f32,
) -> [u8; 4] {
    let center_x = rect.x + rect.width * center.0;
    let center_y = rect.y + rect.height * center.1;
    let radius = [
        (rect.x - center_x).hypot(rect.y - center_y),
        (rect.x + rect.width - center_x).hypot(rect.y - center_y),
        (rect.x - center_x).hypot(rect.y + rect.height - center_y),
        (rect.x + rect.width - center_x).hypot(rect.y + rect.height - center_y),
    ]
    .into_iter()
    .fold(0.0, f32::max);
    let position = if radius <= f32::EPSILON {
        0.0
    } else {
        (x - center_x).hypot(y - center_y) / radius
    };
    sample_normalized_stops(stops, position)
}

fn premultiplied(color: [u8; 4]) -> tiny_skia::PremultipliedColorU8 {
    let alpha = color[3] as u32;
    tiny_skia::PremultipliedColorU8::from_rgba(
        ((color[0] as u32 * alpha) / 255) as u8,
        ((color[1] as u32 * alpha) / 255) as u8,
        ((color[2] as u32 * alpha) / 255) as u8,
        color[3],
    )
    .unwrap_or_else(|| {
        tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 0)
            .expect("transparent premultiplied color")
    })
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn image_metadata_from_bytes(bytes: &[u8]) -> Option<(f32, f32)> {
    let (width, height) = image_dimensions(bytes)
        .map(|(width, height)| (width as f32, height as f32))
        .or_else(|| svg_intrinsic(bytes))?;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((width, height))
}

fn background_image_rect(
    src: &str,
    base_url: Option<&str>,
    box_rect: &crate::Rect,
    explicit_size: Option<(f32, f32)>,
    size_expression: Option<&str>,
    fit: Option<crate::ObjectFit>,
    position: crate::BackgroundPosition,
    em: f32,
    rem: f32,
    viewport: (f32, f32),
    cache: &mut RenderResourceCache,
) -> Option<crate::Rect> {
    let bytes = fetch_bytes(src, base_url, cache)?;
    let intrinsic = if is_svg(&bytes) {
        svg_intrinsic(&bytes)
    } else {
        image_dimensions(&bytes).map(|(width, height)| (width as f32, height as f32))
    };
    let expression_size = size_expression.and_then(|expression| {
        let components = split_background_size_components(expression);
        let width = components.first().and_then(|value| {
            (!value.eq_ignore_ascii_case("auto")).then(|| {
                crate::style::resolve_contextual_length(
                    value,
                    em,
                    rem,
                    viewport.0 / 100.0,
                    viewport.1 / 100.0,
                    box_rect.width,
                )
            })?
        });
        let height = components.get(1).and_then(|value| {
            (!value.eq_ignore_ascii_case("auto")).then(|| {
                crate::style::resolve_contextual_length(
                    value,
                    em,
                    rem,
                    viewport.0 / 100.0,
                    viewport.1 / 100.0,
                    box_rect.height,
                )
            })?
        });
        match (width, height, intrinsic) {
            (Some(width), Some(height), _) => Some((width, height)),
            (Some(width), None, Some((iw, ih))) => Some((width, width * ih / iw)),
            (None, Some(height), Some((iw, ih))) => Some((height * iw / ih, height)),
            (None, None, Some(intrinsic)) => Some(intrinsic),
            _ => None,
        }
    });
    let (width, height) = if let Some(size) = expression_size {
        size
    } else if let Some(size) = explicit_size {
        size
    } else if let Some(fit) = fit {
        let (iw, ih) = intrinsic?;
        let scale = match fit {
            crate::ObjectFit::Cover => (box_rect.width / iw).max(box_rect.height / ih),
            crate::ObjectFit::Contain => (box_rect.width / iw).min(box_rect.height / ih),
            _ => 1.0,
        };
        (iw * scale, ih * scale)
    } else {
        intrinsic.unwrap_or((box_rect.width, box_rect.height))
    };
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(crate::Rect {
        x: box_rect.x + position.x.resolve(box_rect.width - width),
        y: box_rect.y + position.y.resolve(box_rect.height - height),
        width,
        height,
    })
}

fn split_background_size_components(value: &str) -> Vec<&str> {
    let mut components = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => {
                depth += 1;
                start.get_or_insert(index);
            }
            ')' => depth = (depth - 1).max(0),
            ch if ch.is_whitespace() && depth == 0 => {
                if let Some(start) = start.take() {
                    components.push(value[start..index].trim());
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
    }
    if let Some(start) = start {
        components.push(value[start..].trim());
    }
    components
}

fn paint_in_flow_generated_box(
    pixmap: &mut Pixmap,
    generated: &crate::dom::GeneratedBox,
    laid: &crate::dom::DomLayout,
    scroll_state: &ScrollPaintState,
    viewport: (f32, f32),
    root_font_size: f32,
    base_url: Option<&str>,
    image_cache: &mut RenderResourceCache,
) {
    let Some(host_style) = laid.styles.get(&generated.host) else {
        return;
    };
    let style = match generated.kind {
        crate::dom::GeneratedBoxKind::Before => host_style.before_pseudo.as_deref(),
        crate::dom::GeneratedBoxKind::After => host_style.after_pseudo.as_deref(),
    };
    let Some(style) = style else { return };
    if style.effectively_invisible {
        return;
    }

    let (ox, oy) = scroll_state.translation_for(laid, generated.host);
    let rect = crate::Rect {
        x: generated.rect.x + ox,
        y: generated.rect.y + oy,
        width: generated.rect.width,
        height: generated.rect.height,
    };
    let clip = scroll_state.descendant_clip_for(laid, generated.host);
    let visible = match clip {
        Some(clip) => rect.intersect(&clip),
        None => Some(rect),
    };
    let Some(visible) = visible else { return };
    if visible.width <= 0.0 || visible.height <= 0.0 {
        return;
    }

    if let Some(shadow) = style.box_shadow {
        paint_box_shadow(pixmap, &shadow, &rect, style.border_radius, clip);
    }
    let radius = style.border_radius.resolve(rect.width, rect.height);
    let has_radius = radius.0 > 0.5 && radius.1 > 0.5;
    let clip_path_mask = style.clip_path.as_ref().and_then(|polygon| {
        polygon_clip_mask(
            pixmap.width(),
            pixmap.height(),
            polygon,
            &rect,
            style.font_size.unwrap_or(16.0),
            root_font_size,
            viewport,
        )
    });
    let path = if has_radius {
        rounded_rect_path(
            visible.x,
            visible.y,
            visible.width,
            visible.height,
            radius.0,
            radius.1,
        )
    } else {
        Rect::from_xywh(visible.x, visible.y, visible.width, visible.height).and_then(|rect| {
            let mut builder = PathBuilder::new();
            builder.push_rect(rect);
            builder.finish()
        })
    };
    let Some(path) = path else { return };
    if style.mask_image.is_none() && !style.background_clip_text {
        if let Some(color) = style.background_color {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
            paint.anti_alias = has_radius;
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip_path_mask.as_ref(),
            );
        }
        if !style.background_gradient_layers.is_empty() {
            paint_background_gradient_layers(
                pixmap,
                &path,
                &rect,
                radius,
                style,
                root_font_size,
                viewport,
                clip_path_mask.as_ref(),
            );
        } else {
        if let Some((center, stops)) = &style.background_radial_gradient {
            paint_radial_gradient(
                pixmap,
                &path,
                &rect,
                *center,
                stops,
                clip_path_mask.as_ref(),
            );
        }
        if let Some((angle, center, stops)) = &style.background_conic_gradient {
            paint_conic_gradient(
                pixmap,
                &rect,
                radius,
                *angle,
                *center,
                stops,
                clip_path_mask.as_ref(),
            );
        }
        if let Some((angle, stops)) = &style.background_gradient {
                paint_linear_gradient(pixmap, &path, &rect, *angle, stops, clip_path_mask.as_ref());
            }
        }
    }
    if let Some(mask_url) = &style.mask_image {
        let fill = style
            .background_color
            .or(style.color)
            .unwrap_or([0, 0, 0, 255]);
        paint_mask(
            mask_url,
            base_url,
            &visible,
            radius,
            fill,
            style.background_radial_gradient.as_ref(),
            style.background_gradient.as_ref(),
            style.background_conic_gradient.as_ref(),
            style.mask_size,
            style.mask_repeat,
            clip_path_mask.as_ref(),
            pixmap,
            image_cache,
        );
    } else if let Some(background_url) = &style.background_image {
        if let Some(image_rect) = background_image_rect(
            background_url,
            base_url,
            &rect,
            style.background_size,
            style.background_size_expression.as_deref(),
            style.background_size_fit,
            style.background_position,
            style.font_size.unwrap_or(16.0),
            root_font_size,
            viewport,
            image_cache,
        ) {
            paint_image(
                background_url,
                base_url,
                &image_rect,
                &visible,
                crate::ObjectFit::Fill,
                pixmap,
                image_cache,
                None,
                radius,
                clip_path_mask.as_ref(),
            );
        }
    }

    let border_color = style.border_color.or(style.color).unwrap_or([0, 0, 0, 255]);
    let mut border_paint = Paint::default();
    border_paint.set_color(Color::from_rgba8(
        border_color[0],
        border_color[1],
        border_color[2],
        border_color[3],
    ));
    let mut borders = PathBuilder::new();
    let mut push_edge = |edge: crate::Rect| {
        let edge = match clip {
            Some(clip) => edge.intersect(&clip),
            None => Some(edge),
        };
        if let Some(edge) = edge {
            if let Some(rect) = Rect::from_xywh(edge.x, edge.y, edge.width, edge.height) {
                borders.push_rect(rect);
            }
        }
    };
    if style.border.top > 0.0 {
        push_edge(crate::Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: style.border.top,
        });
    }
    if style.border.right > 0.0 {
        push_edge(crate::Rect {
            x: rect.x + rect.width - style.border.right,
            y: rect.y,
            width: style.border.right,
            height: rect.height,
        });
    }
    if style.border.bottom > 0.0 {
        push_edge(crate::Rect {
            x: rect.x,
            y: rect.y + rect.height - style.border.bottom,
            width: rect.width,
            height: style.border.bottom,
        });
    }
    if style.border.left > 0.0 {
        push_edge(crate::Rect {
            x: rect.x,
            y: rect.y,
            width: style.border.left,
            height: rect.height,
        });
    }
    if let Some(path) = borders.finish() {
        pixmap.fill_path(
            &path,
            &border_paint,
            FillRule::Winding,
            Transform::identity(),
            clip_path_mask.as_ref(),
        );
    }
}

fn paint_positioned_pseudo(
    text_engine: &mut crate::inline::TextEngine,
    pixmap: &mut Pixmap,
    style: &crate::LayoutStyle,
    containing_block: &crate::Rect,
    static_position_rect: &crate::Rect,
    viewport: (f32, f32),
    root_font_size: f32,
    ancestor_clip: Option<crate::Rect>,
    base_url: Option<&str>,
    image_cache: &mut RenderResourceCache,
) {
    if style.position != Some(taffy::Position::Absolute) {
        return;
    }
    let em = style.font_size.unwrap_or(16.0);
    let resolve = |dimension: crate::Dimension, basis: f32| match dimension.resolve(
        em,
        root_font_size,
        viewport.0 / 100.0,
        viewport.1 / 100.0,
    ) {
            crate::Dimension::Px(value) => Some(value),
            crate::Dimension::Percent(value) => Some(value * basis),
            _ => None,
    };
    let top = style.inset[0].and_then(|value| resolve(value, containing_block.height));
    let right = style.inset[1].and_then(|value| resolve(value, containing_block.width));
    let bottom = style.inset[2].and_then(|value| resolve(value, containing_block.height));
    let left = style.inset[3].and_then(|value| resolve(value, containing_block.width));
    // Generated text supplies the shrink-to-fit dimensions of an absolutely
    // positioned pseudo whose width and/or height is auto. Tailwind's code
    // gutters use exactly this shape (`width` plus auto height); requiring two
    // opposing insets previously discarded the pseudo before text painting.
    let generated_item = style
        .before_content
        .as_deref()
        .filter(|content| !content.is_empty())
        .and_then(|content| text_engine.push_generated_text(content, style));
    let generated_intrinsic = generated_item.map(|item| text_engine.measure(item, None));
    let width = resolve(style.width, containing_block.width)
        .or_else(|| Some(containing_block.width - left? - right?))
        .or_else(|| generated_intrinsic.map(|size| size.0));
    let height = resolve(style.height, containing_block.height)
        .or_else(|| Some(containing_block.height - top? - bottom?))
        .or_else(|| generated_intrinsic.map(|size| size.1));
    let (Some(width), Some(height)) = (width, height) else {
        return;
    };
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let x = left
        .map(|value| containing_block.x + value)
        .or_else(|| right.map(|value| containing_block.x + containing_block.width - value - width))
        .unwrap_or(static_position_rect.x);
    let y = top
        .map(|value| containing_block.y + value)
        .or_else(|| {
            bottom.map(|value| containing_block.y + containing_block.height - value - height)
        })
        .unwrap_or(static_position_rect.y);
    let rect = crate::Rect {
        x,
        y,
        width,
        height,
    };
    let visible = match ancestor_clip {
        Some(clip) => rect.intersect(&clip),
        None => Some(rect),
    };
    let Some(visible) = visible else { return };
    let radius = style.border_radius.resolve(rect.width, rect.height);
    let has_radius = radius.0 > 0.5 && radius.1 > 0.5;
    let clip_path_mask = style.clip_path.as_ref().and_then(|polygon| {
        polygon_clip_mask(
            pixmap.width(),
            pixmap.height(),
            polygon,
            &rect,
            em,
            root_font_size,
            viewport,
        )
    });
    let path = if has_radius {
        rounded_rect_path(
            visible.x,
            visible.y,
            visible.width,
            visible.height,
            radius.0,
            radius.1,
        )
    } else {
        Rect::from_xywh(visible.x, visible.y, visible.width, visible.height).and_then(|rect| {
            let mut builder = PathBuilder::new();
            builder.push_rect(rect);
            builder.finish()
        })
    };
    let Some(path) = path else { return };
    if style.mask_image.is_none() {
        if let Some(color) = style.background_color {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip_path_mask.as_ref(),
            );
        }
        if !style.background_gradient_layers.is_empty() {
            paint_background_gradient_layers(
                pixmap,
                &path,
                &rect,
                radius,
                style,
                root_font_size,
                viewport,
                clip_path_mask.as_ref(),
            );
        } else {
        if let Some((center, stops)) = &style.background_radial_gradient {
            paint_radial_gradient(
                pixmap,
                &path,
                &rect,
                *center,
                stops,
                clip_path_mask.as_ref(),
            );
        }
        if let Some((angle, center, stops)) = &style.background_conic_gradient {
            paint_conic_gradient(
                pixmap,
                &rect,
                radius,
                *angle,
                *center,
                stops,
                clip_path_mask.as_ref(),
            );
        }
        if let Some((angle, stops)) = &style.background_gradient {
                paint_linear_gradient(pixmap, &path, &rect, *angle, stops, clip_path_mask.as_ref());
            }
        }
    }
    if let Some(mask_url) = &style.mask_image {
        let fill = style
            .background_color
            .or(style.color)
            .unwrap_or([0, 0, 0, 255]);
        paint_mask(
            mask_url,
            base_url,
            &visible,
            radius,
            fill,
            style.background_radial_gradient.as_ref(),
            style.background_gradient.as_ref(),
            style.background_conic_gradient.as_ref(),
            style.mask_size,
            style.mask_repeat,
            clip_path_mask.as_ref(),
            pixmap,
            image_cache,
        );
    } else if let Some(bg_url) = &style.background_image {
        if let Some(image_rect) = background_image_rect(
            bg_url,
            base_url,
            &rect,
            style.background_size,
            style.background_size_expression.as_deref(),
            style.background_size_fit,
            style.background_position,
            em,
            root_font_size,
            viewport,
            image_cache,
        ) {
            paint_image(
                bg_url,
                base_url,
                &image_rect,
                &visible,
                crate::ObjectFit::Fill,
                pixmap,
                image_cache,
                None,
                radius,
                clip_path_mask.as_ref(),
            );
        }
    }
    if let Some(item) = generated_item {
        let (text_width, text_height) = generated_intrinsic.unwrap_or_default();
        // `text-align` positions inline content within the pseudo's content
        // box. It is independent of flex/grid `justify-content`: Tailwind's
        // absolutely positioned line-number gutters are inline-block pseudos,
        // so consulting only `justify-content` left every counter at the
        // gutter's start edge even though its inherited computed alignment
        // was `right`.
        let x = if style.display == crate::Display::Flex {
            match style.justify_content {
                Some(taffy::JustifyContent::CENTER) => {
                    rect.x + (rect.width - text_width) / 2.0
                }
                Some(taffy::JustifyContent::FLEX_END | taffy::JustifyContent::END) => {
                    rect.x + rect.width - style.padding.right - text_width
                }
                _ => rect.x + style.padding.left,
            }
        } else {
            match style.text_align {
                Some(taffy::AlignItems::CENTER) => rect.x + (rect.width - text_width) / 2.0,
                Some(taffy::AlignItems::FLEX_END | taffy::AlignItems::END) => {
                    rect.x + rect.width - style.padding.right - text_width
                }
                _ => rect.x + style.padding.left,
            }
        };
        let y = match style.align_items {
            Some(taffy::AlignItems::CENTER) => rect.y + (rect.height - text_height) / 2.0,
            Some(taffy::AlignItems::FLEX_END | taffy::AlignItems::END) => {
                rect.y + rect.height - style.padding.bottom - text_height
            }
            _ => rect.y + style.padding.top,
        };
        text_engine.finalize(item, (x, y), text_width, Some(visible));
        text_engine.paint_item(item, pixmap, (0.0, 0.0));
    }
}

/// Fetch every `<img>` once (seeding `cache` for the paint pass) and record its
/// intrinsic (width, height) so layout can size replaced elements that have no
/// explicit dimensions. Keyed by the `<img>`'s NodeId.
fn collect_image_intrinsics(
    tree: &DomTree,
    viewport: (f32, f32),
    base_url: Option<&str>,
    cache: &mut RenderResourceCache,
) -> (
    HashMap<obscura_dom::tree::NodeId, (f32, f32)>,
    HashMap<obscura_dom::tree::NodeId, SelectedImage>,
) {
    let mut out = std::collections::HashMap::new();
    let mut selected = HashMap::new();
    for nid in tree.descendants(tree.document()) {
        let Some(node) = tree.get_node(nid) else { continue };
        if node.as_element().map(|e| e.local.as_ref() != "img").unwrap_or(true) {
            continue;
        }
        let Some((url, density)) = resolve_img_url(tree, nid, viewport) else { continue };
        let resolved_url = resolve_resource_url(&url, base_url).unwrap_or(url);
        selected.insert(
            nid,
            SelectedImage {
                resolved_url: resolved_url.clone(),
                density,
            },
        );
        let Some(bytes) = fetch_bytes(&resolved_url, None, cache) else { continue };
        let dimensions = image_dimensions(&bytes).map(|(width, height)| (width as f32, height as f32))
            .or_else(|| svg_intrinsic(&bytes));
        if let Some((w, h)) = dimensions {
            if w > 0.0 && h > 0.0 {
                // A 2x (or w-descriptor) candidate's raw pixels are density
                // times its CSS size; divide so layout sees CSS px, or every
                // responsive image occupies twice its design size.
                out.insert(nid, (w / density, h / density));
            }
        }
    }
    (out, selected)
}

/// Add intrinsic metadata for CSS `content:url(...)` images after the first
/// cascade has exposed their computed URL. Returns true when a new intrinsic
/// entry was added and layout therefore needs one resource-aware retry.
fn collect_content_image_intrinsics(
    tree: &DomTree,
    styles: &std::collections::HashMap<obscura_dom::tree::NodeId, crate::LayoutStyle>,
    base_url: Option<&str>,
    cache: &mut RenderResourceCache,
    out: &mut std::collections::HashMap<obscura_dom::tree::NodeId, (f32, f32)>,
    selected: &mut HashMap<obscura_dom::tree::NodeId, SelectedImage>,
) -> bool {
    let mut changed = false;
    for (&nid, style) in styles {
        let Some(node) = tree.get_node(nid) else {
            continue;
        };
        if node
            .as_element()
            .map_or(true, |name| name.local.as_ref() != "img")
        {
            continue;
        }
        let Some(url) = style.content_image.as_deref() else {
            continue;
        };
        let resolved_url =
            resolve_resource_url(url, base_url).unwrap_or_else(|| url.to_string());
        selected.insert(
            nid,
            SelectedImage {
                resolved_url: resolved_url.clone(),
                density: 1.0,
            },
        );
        let Some(bytes) = fetch_bytes(&resolved_url, None, cache) else {
            continue;
        };
        let dimensions = image_dimensions(&bytes)
            .map(|(width, height)| (width as f32, height as f32))
            .or_else(|| svg_intrinsic(&bytes));
        let Some((width, height)) = dimensions else {
            continue;
        };
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        changed |= out.insert(nid, (width, height)) != Some((width, height));
    }
    changed
}

/// Choose the URL to paint for an `<img>`. Browsers do not use `src` alone:
/// a wrapping `<picture>`'s `<source>`s, `srcset`, and `sizes` select by
/// type/media/viewport/density, and lazy-loaded images keep the real URL in
/// `data-src`/`data-srcset` with `src` holding a 1x1 placeholder until script
/// swaps it in. Since obscura may not have run the site's lazy-load script,
/// resolve the same URL the browser would end up with: a matching `<picture>`
/// source first, then a real candidate from `srcset`/`data-srcset`, then a
/// non-inline `src`/`data-*` URL, then any `src` (an inlined data: image).
fn resolve_img_url(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    viewport: (f32, f32),
) -> Option<(String, f32)> {
    let node = tree.get_node(nid)?;
    // A <picture>'s preceding, type/media-matching <source> wins over the
    // <img>'s own attributes (HTML "update the source set").
    if let Some(pick) = picture_source_url(tree, nid, viewport) {
        return Some(pick);
    }
    let sizes = node.get_attribute("sizes");
    for a in ["srcset", "data-srcset"] {
        if let Some(v) = node.get_attribute(a) {
            if let Some(pick) = best_srcset_candidate(v, sizes, viewport) {
                return Some(pick);
            }
        }
    }
    let url_attrs = ["src", "data-src", "data-lazy-src", "data-original", "data-fallback-src", "data-lazy"];
    // A non-inline URL first (a data: src is usually the lazy-load placeholder).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() && !v.starts_with("data:") {
                return Some((v.to_string(), 1.0));
            }
        }
    }
    // Otherwise fall back to whatever is there (an inlined data: image).
    for a in url_attrs {
        if let Some(v) = node.get_attribute(a) {
            let v = v.trim();
            if !v.is_empty() {
                return Some((v.to_string(), 1.0));
            }
        }
    }
    None
}

/// When `img_nid` is an `<img>` inside a `<picture>`, walk its preceding
/// `<source>` siblings in document order and return the selected URL of the
/// first supported one (matching `type` and `media`), per WebKit's
/// `HTMLImageElement::bestFitSourceFromPictureElement`. `None` means no source
/// applied and the caller should fall back to the `<img>`'s own attributes.
fn picture_source_url(
    tree: &DomTree,
    img_nid: obscura_dom::tree::NodeId,
    viewport: (f32, f32),
) -> Option<(String, f32)> {
    let img = tree.get_node(img_nid)?;
    let parent = img.parent?;
    let is_picture = tree
        .get_node(parent)
        .and_then(|p| p.as_element().map(|e| e.local.as_ref() == "picture"))
        .unwrap_or(false);
    if !is_picture {
        return None;
    }
    for cid in tree.children(parent) {
        // Only sources that precede the <img> contribute.
        if cid == img_nid {
            break;
        }
        let Some(child) = tree.get_node(cid) else { continue };
        if child.as_element().map(|e| e.local.as_ref() != "source").unwrap_or(true) {
            continue;
        }
        let Some(srcset) = child.get_attribute("srcset") else { continue };
        if srcset.trim().is_empty() {
            continue;
        }
        if let Some(t) = child.get_attribute("type") {
            if !crate::source_type_supported(t) {
                continue;
            }
        }
        if let Some(m) = child.get_attribute("media") {
            if !m.trim().is_empty()
                && !crate::css::media_query_applies_for_viewport(m, viewport)
            {
                continue;
            }
        }
        let sizes = child.get_attribute("sizes");
        if let Some(u) = best_srcset_candidate(srcset, sizes, viewport) {
            return Some(u);
        }
    }
    None
}

/// Pick one URL from a `srcset` list, matching the WebKit/Blink selection:
/// normalize each `w` descriptor to an effective density (`w / source-size`,
/// with the source-size taken from `sizes` or falling back to the viewport
/// width), treat `x` descriptors as-is and a bare candidate as `1x`, then pick
/// the smallest density at least the device pixel ratio (1 at DPR 1), else the
/// largest available.
/// Returns the picked candidate URL and its pixel density. The density is the
/// x-descriptor (or, for w-descriptors, width / source-size): the factor the
/// file's raw pixels must be divided by to get CSS px. Laying out with raw
/// pixels made every 2x responsive image occupy twice its design size.
fn best_srcset_candidate(
    srcset: &str,
    sizes: Option<&str>,
    viewport: (f32, f32),
) -> Option<(String, f32)> {
    const DPR: f32 = 1.0;
    let source_size = source_size_px(sizes, viewport);
    let mut cands: Vec<(f32, String)> = Vec::new();
    // Parse candidates WHATWG-style: a URL is a run of non-whitespace (so a
    // data: URI's internal commas stay part of it, unlike a naive split on
    // ','), optionally followed by a descriptor up to the next comma.
    let is_ws = |c: char| c.is_whitespace();
    let mut rest = srcset.trim_start_matches(|c: char| is_ws(c) || c == ',');
    while !rest.is_empty() {
        let url_end = rest.find(is_ws).unwrap_or(rest.len());
        let raw_url = &rest[..url_end];
        rest = &rest[url_end..];
        // Trailing commas on the URL mean the candidate had no descriptor.
        let url = raw_url.trim_end_matches(',');
        let no_desc = url.len() != raw_url.len();
        rest = rest.trim_start_matches(is_ws);
        let desc = if no_desc {
            ""
        } else {
            let d_end = rest.find(',').unwrap_or(rest.len());
            let d = rest[..d_end].trim();
            rest = &rest[d_end..];
            d
        };
        rest = rest.trim_start_matches(|c: char| c == ',' || is_ws(c));
        if url.is_empty() {
            continue;
        }
        let density = if desc.is_empty() {
            1.0
        } else if let Some(w) = desc.strip_suffix('w').and_then(|s| s.parse::<f32>().ok()) {
            if source_size > 0.0 { w / source_size } else { continue }
        } else if let Some(x) = desc.strip_suffix('x').and_then(|s| s.parse::<f32>().ok()) {
            x
        } else {
            // An `h` (height) descriptor or malformed token: skip the candidate.
            continue;
        };
        cands.push((density, url.to_string()));
    }
    if cands.is_empty() {
        return None;
    }
    cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let pick = cands
        .iter()
        .find(|(d, _)| *d >= DPR)
        .map(|(d, u)| (u.clone(), *d))
        .unwrap_or_else(|| {
            let (d, u) = cands.last().unwrap();
            (u.clone(), *d)
        });
    Some((pick.0, pick.1.max(0.01)))
}

/// Approximate the CSS px size an image will be displayed at, from its `sizes`
/// attribute: the first entry whose media condition holds at our assumed
/// desktop viewport (a bare entry always holds), else the viewport width. Used
/// only to convert `w` descriptors to densities, so a coarse value is fine.
fn source_size_px(sizes: Option<&str>, viewport: (f32, f32)) -> f32 {
    let Some(sizes) = sizes else { return viewport.0 };
    for entry in sizes.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (cond, len) = split_size_entry(entry);
        if let Some(cond) = cond {
            if !crate::css::media_query_applies_for_viewport(&cond, viewport) {
                continue;
            }
        }
        if let Some(px) = length_to_px(&len, viewport.0) {
            return px;
        }
    }
    viewport.0
}

/// Split one `sizes` entry into its optional leading media condition and its
/// trailing `<length>`. Tokenizes on whitespace at paren depth 0 so a
/// `calc(...)` length or a parenthesized condition stays intact; the last
/// token is the length, anything before it is the condition.
fn split_size_entry(entry: &str) -> (Option<String>, String) {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in entry.chars() {
        match c {
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() { tokens.push(std::mem::take(&mut cur)); }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let len = tokens.pop().unwrap_or_default();
    let cond = if tokens.is_empty() { None } else { Some(tokens.join(" ")) };
    (cond, len)
}

/// Resolve a `sizes` length to px against the assumed viewport. `vw`/`%` scale
/// by the viewport width; `px` is literal; `em`/`rem` use the 16px root.
/// `calc()` and other forms return `None` (the caller tries the next entry).
fn length_to_px(len: &str, viewport_width: f32) -> Option<f32> {
    let t = len.trim().to_ascii_lowercase();
    let num = |s: &str| s.trim().parse::<f32>().ok();
    if let Some(v) = t.strip_suffix("vw").and_then(num) { return Some(v / 100.0 * viewport_width); }
    if let Some(v) = t.strip_suffix('%').and_then(num) { return Some(v / 100.0 * viewport_width); }
    if let Some(v) = t.strip_suffix("px").and_then(num) { return Some(v); }
    if let Some(v) = t.strip_suffix("rem").and_then(num) { return Some(v * 16.0); }
    if let Some(v) = t.strip_suffix("em").and_then(num) { return Some(v * 16.0); }
    num(&t)
}

fn paint_image(
    src: &str,
    base_url: Option<&str>,
    rect: &crate::Rect,
    visible_rect: &crate::Rect,
    object_fit: crate::ObjectFit,
    pixmap: &mut Pixmap,
    cache: &mut RenderResourceCache,
    transform: Option<ImageAffine>,
    clip_radius: (f32, f32),
    extra_clip: Option<&tiny_skia::Mask>,
) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let Some(bytes) = fetch_bytes(src, base_url, cache) else { return false };
    let svg = is_svg(&bytes);

    // Destination sub-rect within the element box. `Fill` keeps the historical
    // behavior (stretch the image to the whole box); the other modes need the
    // image's intrinsic size to preserve its aspect ratio, and fall back to
    // fill when it cannot be read.
    let dest = if object_fit == crate::ObjectFit::Fill {
        *rect
    } else {
        let intrinsic = if svg {
            svg_intrinsic(&bytes)
        } else {
            image_dimensions(&bytes).map(|(w, h)| (w as f32, h as f32))
        };
        match intrinsic {
            Some((iw, ih)) => object_fit_dest(rect, iw, ih, object_fit),
            None => *rect,
        }
    };

    let (dw, dh) = (dest.width.round().max(1.0) as u32, dest.height.round().max(1.0) as u32);
    let content = if svg {
        render_svg(&bytes, dw, dh)
    } else {
        raster_to_pixmap(&bytes, dw, dh)
    };
    let Some(content) = content else { return false };

    // The raster may not paint past `visible_rect` (the border box already
    // intersected with the ancestor overflow clip): `Cover`/`None` can size
    // the image past the box, and an ancestor clip can cut into the box
    // itself. Only the fully-inside case takes the unmasked fast path.
    let has_radius = clip_radius.0 > 0.5 && clip_radius.1 > 0.5;
    let needs_box_clip = has_radius
        || dest.width > visible_rect.width + 0.5
        || dest.height > visible_rect.height + 0.5
        || dest.x < visible_rect.x - 0.5
        || dest.y < visible_rect.y - 0.5;
    let mut clip = extra_clip.cloned();
    if needs_box_clip {
        let path = if has_radius {
            rounded_rect_path(
                visible_rect.x,
                visible_rect.y,
                visible_rect.width,
                visible_rect.height,
                clip_radius.0,
                clip_radius.1,
            )
        } else {
            Rect::from_xywh(
                visible_rect.x,
                visible_rect.y,
                visible_rect.width,
                visible_rect.height,
            )
            .and_then(|rect| {
                let mut builder = PathBuilder::new();
                builder.push_rect(rect);
                builder.finish()
            })
        };
        match (clip.as_mut(), path) {
            (Some(mask), Some(path)) => mask.intersect_path(
                &path,
                FillRule::Winding,
                true,
                Transform::identity(),
            ),
            (None, _) => {
                clip = rounded_box_clip_mask(
                    pixmap.width(),
                    pixmap.height(),
                    visible_rect,
                    clip_radius,
                );
            }
            _ => {}
        }
    }
    pixmap.draw_pixmap(
        dest.x as i32,
        dest.y as i32,
        content.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        transform
            .map(ImageAffine::tiny_skia)
            .unwrap_or_else(Transform::identity),
        clip.as_ref(),
    );
    true
}

/// The destination sub-rect for a replaced element's image within its box,
/// given the image's intrinsic `(iw, ih)` size and `object-fit`. Centered in
/// the box; for `Cover`/`None` it can extend past the box edges (the caller
/// clips it). Aspect ratio is preserved for every mode except `Fill`.
fn object_fit_dest(box_rect: &crate::Rect, iw: f32, ih: f32, fit: crate::ObjectFit) -> crate::Rect {
    let (bw, bh) = (box_rect.width, box_rect.height);
    if iw <= 0.0 || ih <= 0.0 {
        return *box_rect;
    }
    let (dw, dh) = match fit {
        crate::ObjectFit::Fill => (bw, bh),
        crate::ObjectFit::Contain => {
            let s = (bw / iw).min(bh / ih);
            (iw * s, ih * s)
        }
        crate::ObjectFit::Cover => {
            let s = (bw / iw).max(bh / ih);
            (iw * s, ih * s)
        }
        crate::ObjectFit::None => (iw, ih),
        crate::ObjectFit::ScaleDown => {
            // min(Contain-size, intrinsic-size): the Contain fit, but never
            // scaled up past the image's own pixels.
            let s = (bw / iw).min(bh / ih).min(1.0);
            (iw * s, ih * s)
        }
    };
    crate::Rect {
        x: box_rect.x + (bw - dw) / 2.0,
        y: box_rect.y + (bh - dh) / 2.0,
        width: dw,
        height: dh,
    }
}

/// The intrinsic `(width, height)` of an SVG image from its size/`viewBox`,
/// used to preserve aspect ratio under `object-fit`. Parses the SVG once; the
/// eventual raster re-parses in `render_svg` (only reached for a non-`fill`
/// object-fit on an SVG image, which is rare).
fn svg_intrinsic(bytes: &[u8]) -> Option<(f32, f32)> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let size = tree.size();
    if size.width() > 0.0 && size.height() > 0.0 {
        Some((size.width(), size.height()))
    } else {
        None
    }
}

/// A full-pixmap clip mask admitting only the pixels inside `rect`, used to
/// crop an `object-fit: cover|none` image to its element box.
fn box_clip_mask(pw: u32, ph: u32, rect: &crate::Rect) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(pw, ph)?;
    let r = Rect::from_xywh(rect.x, rect.y, rect.width, rect.height)?;
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    let path = pb.finish()?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

/// A full-pixmap clip mask for a border box with a uniform elliptical radius.
/// Used by replaced and background images, whose raster content otherwise
/// ignores the owner's rounded corners.
fn rounded_box_clip_mask(
    pw: u32,
    ph: u32,
    rect: &crate::Rect,
    radius: (f32, f32),
) -> Option<tiny_skia::Mask> {
    if radius.0 <= 0.5 || radius.1 <= 0.5 {
        return box_clip_mask(pw, ph, rect);
    }
    let mut mask = tiny_skia::Mask::new(pw, ph)?;
    let path = rounded_rect_path(
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        radius.0,
        radius.1,
    )?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

/// Resolve a CSS polygon against its border-box reference and rasterize it as
/// a full-surface alpha clip. Gecko's basic-shape path builder likewise
/// resolves each x percentage against the reference width and each y
/// percentage against its height before closing the path.
fn polygon_clip_mask(
    pw: u32,
    ph: u32,
    polygon: &crate::ClipPathPolygon,
    rect: &crate::Rect,
    em: f32,
    rem: f32,
    viewport: (f32, f32),
) -> Option<tiny_skia::Mask> {
    let resolve = |coordinate: crate::Dimension, basis: f32| match coordinate.resolve(
            em,
            rem,
            viewport.0 / 100.0,
            viewport.1 / 100.0,
        ) {
            crate::Dimension::Px(value) => Some(value),
            crate::Dimension::Percent(value) => Some(value * basis),
            _ => None,
    };
    let mut builder = PathBuilder::new();
    for (index, &(x, y)) in polygon.points.iter().enumerate() {
        let x = rect.x + resolve(x, rect.width)?;
        let y = rect.y + resolve(y, rect.height)?;
        if index == 0 {
            builder.move_to(x, y);
        } else {
            builder.line_to(x, y);
        }
    }
    builder.close();
    let path = builder.finish();
    let fill_rule = match polygon.fill_rule {
        crate::ClipPathFillRule::Nonzero => FillRule::Winding,
        crate::ClipPathFillRule::Evenodd => FillRule::EvenOdd,
    };
    let mut mask = tiny_skia::Mask::new(pw, ph)?;
    // Degenerate polygons are valid CSS basic shapes but have no enclosed
    // area. Keep the newly zeroed mask in that case: treating it as "no mask"
    // would incorrectly reveal the whole element.
    if let Some(path) = path {
        mask.fill_path(&path, fill_rule, true, Transform::identity());
    }
    Some(mask)
}

/// Sniff SVG content: either an XML/SVG prolog, or a bare `<svg` root tag
/// (both are valid, and image responses commonly omit the XML declaration).
fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(256)];
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<?xml") || trimmed.starts_with("<svg")
}

/// Rasterize SVG bytes to a `width` x `height` pixmap, scaled to fit (matching
/// how a replaced element like `<img>` sizes its intrinsic content).
fn render_svg(bytes: &[u8], width: u32, height: u32) -> Option<Pixmap> {
    let fonts = svg_font_database();
    render_svg_with_font_database(bytes, width, height, &fonts)
}

fn render_svg_with_font_database(
    bytes: &[u8],
    width: u32,
    height: u32,
    fonts: &std::sync::Arc<usvg::fontdb::Database>,
) -> Option<Pixmap> {
    if width == 0 || height == 0 {
        return None;
    }
    let mut opts = usvg::Options::default();
    // The outer replaced element supplies the SVG document viewport. Force
    // that used CSS size onto the root before usvg resolves `viewBox`:
    // a missing height is represented as 100%, which usvg otherwise resolves
    // against the viewBox height itself. `<svg width=32 viewBox="0 0 223
    // 236">` would therefore become a 32x236 viewport; its artwork is fitted
    // into a thin centered strip and then the whole strip is scaled to 32x34.
    // Author `preserveAspectRatio` still controls fitting inside this viewport.
    // usvg resolves root dimensions before an injected stylesheet can
    // override them, so provide the used viewport as actual root attributes.
    let viewport_svg = svg_with_root_viewport(bytes, width, height)?;
    opts.default_size = usvg::Size::from_wh(width as f32, height as f32)?;
    opts.font_family = "Liberation Serif".to_string();
    opts.fontdb = std::sync::Arc::clone(fonts);
    let tree = usvg::Tree::from_data(&viewport_svg, &opts).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }
    let mut svg_pixmap = Pixmap::new(width, height)?;
    let transform = Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, transform, &mut svg_pixmap.as_mut());
    Some(svg_pixmap)
}

/// A deterministic font database shared by every SVG raster in the process.
/// Constructing/scanning a database per icon is far too expensive for pages
/// with SVG-heavy navigation and would be prohibitive for future repeated
/// frame capture. The embedded faces are the same stable browser-generic
/// families used by the HTML text engine.
fn svg_font_database() -> std::sync::Arc<usvg::fontdb::Database> {
    static DATABASE: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> =
        std::sync::OnceLock::new();
    std::sync::Arc::clone(DATABASE.get_or_init(|| {
        let mut database = usvg::fontdb::Database::new();
        for bytes in [
            FONT_BYTES,
            FONT_BOLD_BYTES,
            FONT_OBLIQUE_BYTES,
            FONT_BOLD_OBLIQUE_BYTES,
            SERIF_FONT_BYTES,
            MONO_FONT_BYTES,
        ] {
            database.load_font_data(bytes.to_vec());
        }
        database.set_sans_serif_family("Liberation Sans");
        database.set_serif_family("Liberation Serif");
        database.set_monospace_family("Liberation Mono");
        std::sync::Arc::new(database)
    }))
}

fn svg_font_database_with_web_fonts(
    web_fonts: &[crate::inline::WebFont],
) -> std::sync::Arc<usvg::fontdb::Database> {
    let base = svg_font_database();
    if web_fonts.is_empty() {
        return base;
    }
    // `fontdb::Database::clone` shares each binary source; only the page's
    // already-decoded user fonts allocate here, once per page rather than once
    // per SVG. HTML shaping needs the same bytes, so retaining both databases
    // is the cost of keeping the rasterizer and layout engine deterministic.
    let mut database = (*base).clone();
    for font in web_fonts {
        database.load_font_data(font.data.clone());
    }
    std::sync::Arc::new(database)
}

fn has_inline_svg_text(tree: &DomTree) -> bool {
    tree.descendants(tree.document()).into_iter().any(|nid| {
        tree.get_node(nid).is_some_and(|node| {
            node.as_element()
                .is_some_and(|name| matches!(name.local.as_ref(), "text" | "tspan" | "textPath"))
        })
    })
}

/// Return SVG XML whose root `width`/`height` are the resolved CSS viewport.
///
/// This is deliberately a narrow XML start-tag rewrite rather than a DOM
/// reserialization: all namespaces, styles, definitions, and source order
/// remain byte-for-byte intact. Existing attribute values are replaced;
/// missing ones are appended. Quoted `>` characters are respected while
/// finding the end of the root tag.
fn svg_with_root_viewport(bytes: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let source = std::str::from_utf8(bytes).ok()?;
    let start = source.find("<svg")?;
    let tail = &source[start..];
    let mut quote = None;
    let mut tag_end = None;
    for (offset, ch) in tail.char_indices() {
        match (quote, ch) {
            (Some(open), close) if close == open => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => {
                tag_end = Some(start + offset);
                break;
            }
            _ => {}
        }
    }
    let tag_end = tag_end?;
    let mut root = source[start..=tag_end].to_string();
    for (name, value) in [("width", width), ("height", height)] {
        if let Some((value_start, value_end)) = svg_root_attr_value_range(&root, name) {
            root.replace_range(value_start..value_end, &value.to_string());
        } else {
            root.insert_str(root.len() - 1, &format!(" {name}=\"{value}\""));
        }
    }

    let mut output = String::with_capacity(source.len() + 32);
    output.push_str(&source[..start]);
    output.push_str(&root);
    output.push_str(&source[tag_end + 1..]);
    Some(output.into_bytes())
}

/// Value byte range for one attribute in an `<svg ...>` start tag.
fn svg_root_attr_value_range(tag: &str, wanted: &str) -> Option<(usize, usize)> {
    let bytes = tag.as_bytes();
    let mut index = "<svg".len();
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] == b'>' || bytes[index] == b'/' {
            return None;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'>'
            && bytes[index] != b'/'
        {
            index += 1;
        }
        let name_end = index;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let (value_start, value_end) =
            if index < bytes.len() && matches!(bytes[index], b'"' | b'\'') {
            let delimiter = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != delimiter {
                index += 1;
            }
            let end = index;
            index = (index + 1).min(bytes.len());
            (start, end)
        } else {
            let start = index;
            while index < bytes.len()
                && !bytes[index].is_ascii_whitespace()
                && bytes[index] != b'>'
                && bytes[index] != b'/'
            {
                index += 1;
            }
            (start, index)
        };
        if &tag[name_start..name_end] == wanted {
            return Some((value_start, value_end));
        }
    }
    None
}

/// Serialize an inline `<svg>` subtree (rooted at `root`) back to a standalone
/// SVG document string. Emits `<tag attr="v">children</tag>` for the element
/// and every descendant, preserving the root's `viewBox`/`width`/`height` and
/// all `<defs>`/`<symbol>`/`<use>`/`<path>` structure so resvg can rasterize it
/// as a self-contained document. SVG is XML-clean, so there are no HTML
/// void-element or optional-close rules to apply; every element gets an
/// explicit closing tag. The root gains an `xmlns` declaration when it lacks
/// one (common for inline svg, whose namespace is implied by the HTML parser
/// but required for usvg to parse the string on its own).
#[cfg(test)]
fn serialize_svg(tree: &DomTree, root: obscura_dom::tree::NodeId) -> String {
    let mut buf = String::new();
    serialize_svg_node(tree, root, true, None, None, &mut buf);
    buf
}

/// Serialize an inline SVG while carrying the page's computed author styling
/// into the standalone document consumed by resvg. External page stylesheets
/// are otherwise outside that document, so class-driven SVG text and icons
/// silently fall back to presentation attributes or SVG defaults.
fn serialize_svg_styled(
    tree: &DomTree,
    root: obscura_dom::tree::NodeId,
    styles: &std::collections::HashMap<obscura_dom::tree::NodeId, crate::LayoutStyle>,
    suppress_opacity_for: Option<obscura_dom::tree::NodeId>,
) -> String {
    let mut buf = String::new();
    serialize_svg_node(
        tree,
        root,
        true,
        Some(styles),
        suppress_opacity_for,
        &mut buf,
    );
    buf
}

fn inject_svg_current_color(markup: &mut String, color: [u8; 4]) {
    let Some(start) = markup.find("<svg") else { return };
    let Some(end) = markup[start..].find('>').map(|offset| start + offset) else { return };
    let root = &markup[start..end];
    // An explicit presentation attribute already survives serialization and
    // is the correct local currentColor source.
    if root.contains(" color=") {
        return;
    }
    let attribute = format!(
        " color=\"#{:02x}{:02x}{:02x}\"",
        color[0], color[1], color[2]
    );
    markup.insert_str(start + "<svg".len(), &attribute);
}

fn serialize_svg_node(
    tree: &DomTree,
    nid: obscura_dom::tree::NodeId,
    is_root: bool,
    styles: Option<&std::collections::HashMap<obscura_dom::tree::NodeId, crate::LayoutStyle>>,
    suppress_opacity_for: Option<obscura_dom::tree::NodeId>,
    buf: &mut String,
) {
    let node = match tree.get_node(nid) {
        Some(n) => n,
        None => return,
    };
    if let Some(text) = node.text_content_of_text_node() {
        svg_escape_text(text, buf);
        return;
    }
    let name = match node.as_element() {
        Some(n) => n,
        // Document/comment/PI: no tag of its own, emit only element children.
        None => {
            for child in tree.children(nid) {
                serialize_svg_node(
                    tree,
                    child,
                    false,
                    styles,
                    suppress_opacity_for,
                    buf,
                );
            }
            return;
        }
    };
    let tag = name.local.as_ref();
    buf.push('<');
    buf.push_str(tag);
    let mut has_xmlns = false;
    let mut source_style: Option<String> = None;
    if let Some(attrs) = node.attrs() {
        for attr in attrs {
            // Emit the local name only, dropping any prefix (`xlink:href` ->
            // `href`): resvg reads both, and a bare local avoids needing an
            // `xmlns:xlink` declaration in the standalone document.
            let aname = attr.name.local.as_ref();
            // HTML frameworks commonly stamp hydration attributes such as
            // `q:id` onto inline SVG. In an HTML document that name is fine,
            // but our standalone XML serialization has no matching `xmlns:q`,
            // so one irrelevant attribute makes usvg reject the entire logo.
            // Namespace-aware attributes arrive with a clean local name;
            // discard only literal, unbound colon names from the HTML parser.
            if aname.contains(':') {
                continue;
            }
            if aname == "xmlns" {
                has_xmlns = true;
            }
            if styles.is_some() && aname == "style" {
                source_style = Some(attr.value.to_string());
                continue;
            }
            buf.push(' ');
            buf.push_str(aname);
            buf.push_str("=\"");
            svg_escape_attr(&attr.value, buf);
            buf.push('"');
        }
    }
    if styles.is_some() {
        let mut declarations = String::new();
        if let Some(source) = source_style.as_deref() {
            declarations.push_str(source.trim());
            if !declarations.is_empty() && !declarations.ends_with(';') {
                declarations.push(';');
            }
        }
        let mut append = |name: &str, value: &str| {
            if value.trim().is_empty() {
                return;
            }
            declarations.push_str(name);
            declarations.push(':');
            declarations.push_str(value);
            declarations.push_str("!important;");
        };
        if let Some(computed) = styles.and_then(|all| all.get(&nid)) {
            if let Some(value) = computed.svg_fill.as_deref() {
                append(
                    "fill",
                    if value.eq_ignore_ascii_case("currentcolor") {
                        "currentColor"
                    } else {
                        value
                    },
                );
            }
            if let Some(value) = computed.svg_stroke.as_deref() {
                append(
                    "stroke",
                    if value.eq_ignore_ascii_case("currentcolor") {
                        "currentColor"
                    } else {
                        value
                    },
                );
            }
            if let Some(value) = computed.svg_stroke_width.as_deref() {
                append("stroke-width", value);
            }
            if matches!(tag, "svg" | "text" | "textPath" | "textpath" | "tspan") {
                if let Some(value) = computed.font_size {
                    append("font-size", &format!("{value}px"));
                }
                if let Some(value) = computed.font_weight.as_deref() {
                    append("font-weight", value);
                }
                if let Some(value) = computed.font_family.as_deref() {
                    append("font-family", value);
                }
                if computed.font_style_italic == Some(true) {
                    append("font-style", "italic");
                }
            }
            if let Some(color) = computed.color {
                append(
                    "color",
                    &format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]),
                );
            }
            if suppress_opacity_for == Some(nid) {
                // The HTML paint layer applies this SVG root's opacity after
                // resvg returns. Override both a source style declaration and
                // the computed value here so the group is not faded twice.
                append("opacity", "1");
            } else {
                if let Some(opacity) = computed.opacity {
                    append("opacity", &opacity.to_string());
                }
            }
        }
        if !declarations.is_empty() {
            buf.push_str(" style=\"");
            svg_escape_attr(&declarations, buf);
            buf.push('"');
        }
    }
    if is_root && !has_xmlns {
        buf.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }
    buf.push('>');
    for child in tree.children(nid) {
        serialize_svg_node(
            tree,
            child,
            false,
            styles,
            suppress_opacity_for,
            buf,
        );
    }
    buf.push_str("</");
    buf.push_str(tag);
    buf.push('>');
}

fn svg_escape_text(s: &str, buf: &mut String) {
    for c in s.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '>' => buf.push_str("&gt;"),
            _ => buf.push(c),
        }
    }
}

fn svg_escape_attr(s: &str, buf: &mut String) {
    for c in s.chars() {
        match c {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '"' => buf.push_str("&quot;"),
            _ => buf.push(c),
        }
    }
}

/// Resolve `<use>` elements in an inline `<svg>` subtree against either a
/// document-level symbol sprite (`href="#id"`) or an external sprite file
/// (`href="url#id"`), splicing the referenced symbol into the standalone SVG
/// handed to resvg. Symbols already inside `root` need no injection.
fn inject_external_sprites(
    tree: &DomTree,
    root: obscura_dom::tree::NodeId,
    base_url: Option<&str>,
    markup: &mut String,
    cache: &mut RenderResourceCache,
    sprite_cache: &mut std::collections::HashMap<String, Option<String>>,
) {
    // Distinct external references (full href, url, fragment id), in first-seen
    // order. Dedupe so one symbol referenced by several `<use>` is fetched and
    // injected once (the rewrite below still fixes every occurrence).
    let root_descendants = tree.descendants(root);
    let mut refs: Vec<(String, String, String)> = Vec::new();
    let mut local_fragments = Vec::new();
    for nid in tree.descendants(root) {
        let Some(node) = tree.get_node(nid) else { continue };
        let Some(el) = node.as_element() else { continue };
        if el.local.as_ref() != "use" {
            continue;
        }
        // `get_attribute` matches by local name, so a single "href" lookup
        // already covers both `href` and `xlink:href`; check the prefixed form
        // too for completeness.
        let Some(href) = node
            .get_attribute("href")
            .or_else(|| node.get_attribute("xlink:href"))
        else {
            continue;
        };
        let Some(hash) = href.find('#') else { continue };
        let (url, frag) = (&href[..hash], &href[hash + 1..]);
        if frag.is_empty() {
            continue;
        }
        if url.is_empty() {
            if !local_fragments.iter().any(|existing| existing == frag) {
                local_fragments.push(frag.to_string());
            }
            continue;
        }
        let entry = (href.to_string(), url.to_string(), frag.to_string());
        if !refs.contains(&entry) {
            refs.push(entry);
        }
    }
    let mut defs = String::new();
    let mut rewrites: Vec<(String, String)> = Vec::new();
    let wanted_local: std::collections::HashSet<&str> =
        local_fragments.iter().map(String::as_str).collect();
    let mut local_nodes = std::collections::HashMap::new();
    if !wanted_local.is_empty() {
        for nid in tree.descendants(tree.document()) {
            let Some(node) = tree.get_node(nid) else { continue };
            let Some(id) = node.get_attribute("id") else { continue };
            if wanted_local.contains(id) {
                local_nodes.entry(id.to_string()).or_insert(nid);
            }
        }
    }
    for frag in local_fragments {
        let Some(&symbol_id) = local_nodes.get(&frag) else { continue };
        if symbol_id == root || root_descendants.contains(&symbol_id) {
            continue;
        }
        serialize_svg_node(tree, symbol_id, false, None, None, &mut defs);
    }
    for (href, url, frag) in &refs {
        let key = format!("{url}#{frag}");
        let symbol = sprite_cache
            .entry(key)
            .or_insert_with(|| {
                let bytes = fetch_bytes(url, base_url, cache)?;
                let text = String::from_utf8_lossy(&bytes);
                // Drop `xlink:` prefixes in the fetched fragment (resvg reads a
                // bare `href`), matching how the local subtree is serialized and
                // avoiding an undeclared-namespace parse error in the standalone
                // document.
                extract_svg_element_by_id(&text, frag).map(|s| s.replace("xlink:href", "href"))
            })
            .clone();
        let Some(symbol) = symbol else { continue };
        defs.push_str(&symbol);
        rewrites.push((href.clone(), format!("#{frag}")));
    }
    if defs.is_empty() {
        return;
    }

    // Splice the fetched symbols into a `<defs>` immediately after the opening
    // `<svg ...>` tag (the first `>` in the serialized document).
    if let Some(gt) = markup.find('>') {
        markup.insert_str(gt + 1, &format!("<defs>{defs}</defs>"));
    }
    // Point each external `<use>` at the injected local symbol. The serialized
    // href is attribute-escaped, so match against the escaped form.
    for (href, local) in rewrites {
        let from = format!("href=\"{}\"", svg_escape_attr_str(&href));
        let to = format!("href=\"{}\"", svg_escape_attr_str(&local));
        *markup = markup.replace(&from, &to);
    }
}

/// Escape a string for use as an SVG attribute value (`&`, `<`, `"`), returning
/// it as an owned `String` (the buffer-writing `svg_escape_attr` in one call).
fn svg_escape_attr_str(s: &str) -> String {
    let mut buf = String::new();
    svg_escape_attr(s, &mut buf);
    buf
}

/// Pull the element carrying `id="id"` (a `<symbol>`, `<g>`, `<path>`, ...) out
/// of an external sprite document, returned as a verbatim serialized substring
/// (its start tag through the matching end tag, or the self-closing tag alone).
/// A lightweight namespace-agnostic XML scan, not a full parse: usvg would
/// flatten `<symbol>`/`<use>` structure, and we want to re-inject the element
/// unchanged. Returns None when no element has that id.
fn extract_svg_element_by_id(sprite: &str, id: &str) -> Option<String> {
    let mut i = 0usize;
    while i < sprite.len() {
        let rest = &sprite[i..];
        if !rest.starts_with('<') {
            // Advance to the next tag (skips text/whitespace between elements).
            i += rest.find('<')?;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") || rest.starts_with("</") {
            i += rest.find('>').map(|p| p + 1)?;
            continue;
        }
        // A start tag: inner spans between '<' and '>'.
        let gt = i + rest.find('>')?;
        let inner = &sprite[i + 1..gt];
        if tag_attr(inner, "id") == Some(id) {
            if inner.trim_end().ends_with('/') {
                return Some(sprite[i..=gt].to_string());
            }
            let name = tag_name(inner);
            let end = element_end(sprite, gt + 1, name)?;
            return Some(sprite[i..end].to_string());
        }
        i = gt + 1;
    }
    None
}

/// The tag name from a tag's inner text (the bytes between `<` and `>`),
/// dropping any leading `/` of an end tag and stopping at the first whitespace
/// or self-close slash.
fn tag_name(inner: &str) -> &str {
    let inner = inner.trim_start().trim_start_matches('/');
    let end = inner
        .find(|c: char| c.is_ascii_whitespace() || c == '/')
        .unwrap_or(inner.len());
    &inner[..end]
}

/// The value of attribute `want` in a tag's inner text, or None if absent.
/// Matches attribute names whole (so `id` does not match `data-id`/`xml:id`)
/// and handles single/double quoted and bare values.
fn tag_attr<'a>(inner: &'a str, want: &str) -> Option<&'a str> {
    let b = inner.as_bytes();
    let mut i = 0usize;
    // Skip the tag name.
    while i < b.len() && !b[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'/' {
            break;
        }
        let name_start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() && b[i] != b'/' {
            i += 1;
        }
        let name = &inner[name_start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                let vstart = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                let v = &inner[vstart..i.min(b.len())];
                if i < b.len() {
                    i += 1;
                }
                v
            } else {
                let vstart = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'/' {
                    i += 1;
                }
                &inner[vstart..i]
            };
            if name == want {
                return Some(value);
            }
        } else if name == want {
            // Valueless (boolean) attribute.
            return Some("");
        }
    }
    None
}

/// The byte offset just past the `</name>` that closes an element whose content
/// starts at `start`, tracking nesting of same-named tags (e.g. `<g>` inside
/// `<g>`). None if the document ends without a matching close.
fn element_end(sprite: &str, start: usize, name: &str) -> Option<usize> {
    let mut i = start;
    let mut depth = 1usize;
    while i < sprite.len() {
        let rest = &sprite[i..];
        if !rest.starts_with('<') {
            i += rest.find('<')?;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i += rest.find('>').map(|p| p + 1)?;
            continue;
        }
        let gt = i + rest.find('>')?;
        let inner = &sprite[i + 1..gt];
        if rest.starts_with("</") {
            if tag_name(inner) == name {
                depth -= 1;
                if depth == 0 {
                    return Some(gt + 1);
                }
            }
        } else if tag_name(inner) == name && !inner.trim_end().ends_with('/') {
            depth += 1;
        }
        i = gt + 1;
    }
    None
}

/// Paint a `mask-image`: the ubiquitous "colored, scalable icon" pattern,
/// where an SVG shape is used purely as a stencil and tinted by
/// `background-color`/`color` rather than carrying its own colors. Fetches
/// and rasterizes the mask the same way as an ordinary image, then repaints
/// every pixel it covers as `fill`, weighted by the mask's own alpha there
/// (its "coverage"), instead of drawing the mask's own pixel colors.
fn paint_mask(
    src: &str,
    base_url: Option<&str>,
    rect: &crate::Rect,
    border_radius: (f32, f32),
    fill: [u8; 4],
    radial_gradient: Option<&((f32, f32), Vec<([u8; 4], Option<f32>)>)>,
    linear_gradient: Option<&(f32, Vec<([u8; 4], Option<f32>)>)>,
    conic_gradient: Option<&(f32, (f32, f32), Vec<([u8; 4], Option<f32>)>)>,
    mask_size: Option<(f32, f32)>,
    mask_repeat: Option<(bool, bool)>,
    extra_clip: Option<&tiny_skia::Mask>,
    pixmap: &mut Pixmap,
    cache: &mut RenderResourceCache,
) -> bool {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return false;
    }
    let Some(bytes) = fetch_bytes(src, base_url, cache) else { return false };
    let (box_width, box_height) = (rect.width.ceil() as u32, rect.height.ceil() as u32);
    let (tile_width, tile_height) = mask_size
        .map(|(width, height)| {
            (
                width.max(1.0).ceil() as u32,
                height.max(1.0).ceil() as u32,
            )
        })
        .unwrap_or((box_width, box_height));
    let mask = if is_svg(&bytes) {
        render_svg(&bytes, tile_width, tile_height)
    } else {
        raster_to_pixmap(&bytes, tile_width, tile_height)
    };
    let Some(mask) = mask else { return false };

    let repeat = if mask_size.is_some() {
        mask_repeat.unwrap_or((true, true))
    } else {
        mask_repeat.unwrap_or((false, false))
    };
    let normalized_linear =
        linear_gradient.map(|(_, stops)| normalized_stops(stops));
    let normalized_conic =
        conic_gradient.map(|(_, _, stops)| normalized_stops(stops));
    let normalized_radial =
        radial_gradient.map(|(_, stops)| normalized_stops(stops));
    let Some(mut recolored) = Pixmap::new(box_width, box_height) else {
        return false;
    };
    for y in 0..box_height {
        if !repeat.1 && y >= tile_height {
            continue;
        }
        let tile_y = if repeat.1 { y % tile_height } else { y };
        for x in 0..box_width {
            if !repeat.0 && x >= tile_width {
                continue;
            }
            let tile_x = if repeat.0 { x % tile_width } else { x };
            let coverage =
                mask.pixels()[(tile_y * tile_width + tile_x) as usize].alpha() as u32;
            if coverage == 0 {
                continue;
            }
            let sample_x = rect.x + x as f32 + 0.5;
            let sample_y = rect.y + y as f32 + 0.5;
            let mut color = if let (Some((angle, center, _)), Some(stops)) =
                (conic_gradient, normalized_conic.as_deref())
            {
                conic_color_at(rect, *angle, *center, stops, sample_x, sample_y)
            } else if let (Some((angle, _)), Some(stops)) =
                (linear_gradient, normalized_linear.as_deref())
            {
                linear_color_at(rect, *angle, stops, sample_x, sample_y)
            } else if let (Some((center, _)), Some(stops)) =
                (radial_gradient, normalized_radial.as_deref())
            {
                radial_color_at(rect, *center, stops, sample_x, sample_y)
            } else {
                fill
            };
            color[3] = ((color[3] as u32 * coverage) / 255) as u8;
            recolored.pixels_mut()[(y * box_width + x) as usize] =
                premultiplied(color);
        }
    }
    let mut clip = extra_clip.cloned();
    if border_radius.0 > 0.5 && border_radius.1 > 0.5 {
        let path = rounded_rect_path(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            border_radius.0,
            border_radius.1,
        );
        match (clip.as_mut(), path) {
            (Some(mask), Some(path)) => mask.intersect_path(
                &path,
                FillRule::Winding,
                true,
                Transform::identity(),
            ),
            (None, _) => {
                clip = rounded_box_clip_mask(
                    pixmap.width(),
                    pixmap.height(),
                    rect,
                    border_radius,
                );
            }
            _ => {}
        }
    }
    pixmap.draw_pixmap(
        rect.x.floor() as i32,
        rect.y.floor() as i32,
        recolored.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::identity(),
        clip.as_ref(),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_dom::tree_sink::parse_html;

    #[test]
    fn image_accept_advertises_exactly_decodable_mime_types() {
        assert!(!IMAGE_ACCEPT.contains('*'));
        assert!(!IMAGE_ACCEPT.to_ascii_lowercase().contains("avif"));
        for mime in IMAGE_ACCEPT.split(',') {
            assert!(
                crate::source_type_supported(mime),
                "advertised MIME type must be decodable: {mime}"
            );
        }
        for required in [
            "image/webp",
            "image/apng",
            "image/svg+xml",
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/bmp",
            "image/x-icon",
            "image/vnd.microsoft.icon",
        ] {
            assert!(
                IMAGE_ACCEPT.split(',').any(|mime| mime == required),
                "missing supported MIME type {required}"
            );
        }
    }

    #[test]
    fn picture_type_filter_skips_avif_and_accepts_parameterized_mime_essence() {
        let tree = parse_html(
            r#"<picture>
                 <source type="IMAGE/AVIF; codecs=av01" srcset="unsupported.avif">
                 <source type=" Image/WebP ; codecs=lossless " srcset="supported.webp">
                 <img id="hero" src="fallback.png">
               </picture>"#,
        );
        let hero = tree.get_element_by_id("hero").expect("hero");
        assert_eq!(
            picture_source_url(&tree, hero, (800.0, 600.0)),
            Some(("supported.webp".to_string(), 1.0))
        );
    }

    #[test]
    fn enabled_bmp_and_ico_formats_have_metadata_and_full_raster_decode() {
        use std::io::Cursor;

        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            3,
            image::Rgba([20, 40, 60, 255]),
        ));
        for format in [image::ImageFormat::Bmp, image::ImageFormat::Ico] {
            let mut encoded = Cursor::new(Vec::new());
            source
                .write_to(&mut encoded, format)
                .expect("encode fixture");
            let encoded = encoded.into_inner();
            assert_eq!(image_dimensions(&encoded), Some((2, 3)), "{format:?}");
            let raster = raster_to_pixmap(&encoded, 2, 3).expect("decode raster");
            assert_eq!((raster.width(), raster.height()), (2, 3));
        }
    }

    #[test]
    fn gif_placeholders_have_intrinsic_pixels_and_valid_gifs_decode() {
        // Apple's lazy-picture system (and many generic lazy loaders) uses a
        // transparent 1x1 GIF as the selected source until the real candidate
        // enters its preload range. It is still a successfully decoded image:
        // Chromium reports complete=true and naturalWidth=1.
        const TRANSPARENT_GIF: &[u8] = &[
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x70,
            0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02,
            0x44, 0x01, 0x00, 0x3b,
        ];

        // Apple's deliberately palette-less placeholder is accepted by
        // browsers for source selection. Header metadata is sufficient for
        // complete/naturalWidth; visually it contributes no pixels.
        assert_eq!(image_dimensions(TRANSPARENT_GIF), Some((1, 1)));

        const VISIBLE_GIF: &[u8] = &[
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80,
            0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x2c, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x4c, 0x00,
            0x3b,
        ];
        let decoded = raster_to_pixmap(VISIBLE_GIF, 1, 1).expect("decode GIF");
        assert_eq!(decoded.width(), 1);
        assert_eq!(decoded.height(), 1);
        assert_eq!(decoded.pixel(0, 0).expect("pixel").alpha(), 255);
    }

    #[test]
    fn scrolled_viewport_moves_document_content_but_not_fixed_subtrees() {
        let tree = parse_html(
            r#"<html style="margin:0"><body style="margin:0">
                <div style="height:80px;background:#ff0000"></div>
                <div style="height:80px;background:#0000ff"></div>
                <div style="position:fixed;z-index:10;left:0;top:0;width:20px;height:20px;background:#00ff00">
                    <span style="color:#00ff00">x</span>
                </div>
            </body></html>"#,
        );
        let top = paint_dom_scrolled(&tree, (100.0, 80.0), None, (0.0, 0.0))
            .expect("top viewport");
        let scrolled = paint_dom_scrolled(&tree, (100.0, 80.0), None, (0.0, 80.0))
            .expect("scrolled viewport");

        let top_content = top.pixel(50, 10).expect("top content");
        assert!(
            top_content.red() > 240 && top_content.blue() < 15,
            "top viewport should show first red block: {top_content:?}"
        );
        let scrolled_content = scrolled.pixel(50, 10).expect("scrolled content");
        assert!(
            scrolled_content.blue() > 240 && scrolled_content.red() < 15,
            "scrolled viewport should show second blue block: {scrolled_content:?}"
        );
        for (name, pixmap) in [("top", &top), ("scrolled", &scrolled)] {
            let fixed = pixmap.pixel(5, 5).expect("fixed content");
            assert!(
                fixed.green() > 240 && fixed.red() < 15 && fixed.blue() < 15,
                "{name} viewport should keep fixed subtree at the viewport origin: {fixed:?}"
            );
        }
    }

    #[test]
    fn repeated_scroll_capture_moves_shaped_text_and_its_overflow_clip_together() {
        let tree = parse_html(
            r#"<html style="margin:0;overflow:auto"><body style="margin:0">
               <div style="height:1000px;background:#ff0000"></div>
               <section style="height:160px;overflow:hidden;background:#000000;color:#ffffff">
                 <h2 style="margin:0;font-size:32px;line-height:40px">VISIBLE SCROLLED TEXT</h2>
                 <p style="margin:0;font-size:20px;line-height:28px">SECOND SHAPED LINE</p>
                 <div style="position:absolute;left:260px;top:1100px;width:20px;height:20px;background:#00ff00"></div>
                 <svg style="position:absolute;left:260px;top:1040px;width:20px;height:20px"
                      viewBox="0 0 20 20"><rect width="20" height="20" fill="cyan"/></svg>
                 <div style="position:absolute;left:20px;top:1150px;width:20px;height:30px;background:#ff00ff"></div>
               </section>
               <div style="height:200px"></div>
               </body></html>"#,
        );
        let viewport = (300.0, 180.0);
        let mut resources = RenderResourceCache::default();
        let mut prepared =
            prepare_dom(&tree, viewport, None, &mut resources).expect("prepared render");
        let top =
            paint_prepared(&tree, &mut prepared, &mut resources, (0.0, 0.0)).expect("top capture");
        let scrolled = paint_prepared(&tree, &mut prepared, &mut resources, (0.0, 1000.0))
        .expect("scrolled capture");
        let top_repeat = paint_prepared(&tree, &mut prepared, &mut resources, (0.0, 0.0))
        .expect("repeated top capture");
        let scrolled_repeat = paint_prepared(&tree, &mut prepared, &mut resources, (0.0, 1000.0))
        .expect("repeated scrolled capture");

        assert_eq!(top, top_repeat, "returning to the top must not accumulate scroll");
        assert_eq!(
            scrolled, scrolled_repeat,
            "repeated bottom paint must reuse immutable document geometry"
        );
        let white_ink = (0..90)
            .flat_map(|y| (0..250).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let pixel = scrolled.pixel(x, y).expect("text pixel");
                pixel.red() > 220 && pixel.green() > 220 && pixel.blue() > 220
            })
            .count();
        assert!(
            white_ink > 100,
            "visible shaped text must share the viewport-space overflow clip, found {white_ink} white pixels"
        );
        let marker = scrolled.pixel(270, 110).expect("marker pixel");
        assert!(
            marker.green() > 220 && marker.red() < 40 && marker.blue() < 40,
            "a non-text box in the same clipped section must remain visible: {marker:?}"
        );
        let svg_marker = scrolled.pixel(270, 50).expect("svg marker pixel");
        assert!(
            svg_marker.green() > 220 && svg_marker.blue() > 220 && svg_marker.red() < 40,
            "an inline svg in the same clipped section must remain visible: {svg_marker:?}"
        );
        let clipped_marker = scrolled.pixel(30, 165).expect("clipped marker pixel");
        assert!(
            clipped_marker.red() > 240
                && clipped_marker.green() > 240
                && clipped_marker.blue() > 240,
            "nested overflow must still clip content below its document-space padding box: {clipped_marker:?}"
        );
    }

    #[test]
    fn body_overflow_stays_a_content_clip_when_html_owns_root_overflow() {
        let tree = parse_html(
            r#"<html style="margin:0;overflow:auto">
               <body style="margin:0;width:100px;height:50px;overflow:hidden">
                 <div style="position:absolute;left:10px;top:60px;width:20px;height:20px;background:red"></div>
               </body>
               </html>"#,
        );
        let output = paint_dom(&tree, (100.0, 100.0), None).expect("paint");
        let below_body = output.pixel(15, 65).expect("pixel");
        assert!(
            below_body.red() > 240
                && below_body.green() > 240
                && below_body.blue() > 240,
            "body overflow must not be mistaken for a viewport clip when html already owns overflow: {below_body:?}"
        );
    }

    #[test]
    fn repeated_scroll_captures_reuse_immutable_layout_geometry() {
        let tree = parse_html(
            r#"<html style="margin:0"><head><style>
                @font-face { font-family: Fixture; src: url("https://assets.test/font.ttf"); }
                body { font-family: Fixture; }
            </style></head><body style="margin:0">
                <img id="hero" src="https://assets.test/fallback.svg"
                     srcset="https://assets.test/hero.svg 2x"
                     style="display:block;width:100px;height:auto">
                <div style="position:sticky;top:0;height:10px;background:#00ff00"></div>
                <div style="height:60px;background:#ff0000"></div>
                <div style="height:180px;overflow:hidden;background:#0000ff;
                            background-image:url('https://assets.test/background.svg')">
                    <div style="position:sticky;top:5px;height:20px;background:#00ff00"></div>
                    <div style="transform:translate(3px,4px);height:80px;color:#ffffff">stable text</div>
                </div>
                <div id="fixed" style="position:fixed;top:2px;left:2px;width:8px;height:8px"></div>
            </body></html>"#,
        );
        let viewport = (100.0, 80.0);
        let counts = Arc::new(std::sync::Mutex::new(HashMap::<String, usize>::new()));
        let loader_counts = Arc::clone(&counts);
        let mut resources = RenderResourceCache::with_loader(move |url: &str| {
            *loader_counts
                .lock()
                .expect("loader counts")
                .entry(url.to_string())
                .or_default() += 1;
            match url {
                "https://assets.test/font.ttf" => Some(FONT_BYTES.to_vec()),
                "https://assets.test/hero.svg" => Some(
                    br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
                        <rect width="200" height="100" fill="#ffff00"/>
                    </svg>"##
                        .to_vec(),
                ),
                "https://assets.test/background.svg" => Some(
                    br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
                        <rect width="20" height="20" fill="#0000ff"/>
                    </svg>"##
                        .to_vec(),
                ),
                _ => None,
            }
        });
        let mut prepared =
            prepare_dom(&tree, viewport, None, &mut resources).expect("prepared render");
        let hero = tree
            .query_selector("#hero")
            .expect("valid selector")
            .expect("hero");
        assert_eq!(
            prepared.selected_image(hero),
            Some(&SelectedImage {
                resolved_url: "https://assets.test/hero.svg".to_string(),
                density: 2.0,
            })
        );
        let hero_rect = prepared.layout().rects.get(&hero).expect("hero rect");
        assert!((hero_rect.width - 100.0).abs() < 0.1);
        assert!((hero_rect.height - 50.0).abs() < 0.1);
        assert!(prepared.content_size().1 > viewport.1);
        assert!(!prepared.sticky_layout().is_empty());
        assert_eq!(
            prepared.viewport_rect(hero, (0.0, 20.0)).unwrap().y,
            prepared.document_rect(hero).unwrap().y - 20.0
        );
        let fixed = tree
            .query_selector("#fixed")
            .expect("valid selector")
            .expect("fixed");
        assert!(prepared.viewport_fixed_nodes().contains(&fixed));
        assert_eq!(
            prepared.viewport_rect(fixed, (0.0, 100.0)),
            prepared.document_rect(fixed)
        );
        let base_rects = prepared.layout().rects.clone();
        let base_translates = prepared.layout().translates.clone();
        let base_clips = prepared.layout().clip_rects.clone();

        let near = screenshot_prepared(&tree, &mut prepared, &mut resources, (0.0, 20.0))
        .expect("near capture");
        let far = screenshot_prepared(&tree, &mut prepared, &mut resources, (0.0, 100.0))
        .expect("far capture");
        let far_repeat = screenshot_prepared(&tree, &mut prepared, &mut resources, (0.0, 100.0))
        .expect("repeated far capture");
        let near_after_far = screenshot_prepared(&tree, &mut prepared, &mut resources, (0.0, 20.0))
        .expect("repeated near capture");

        assert_ne!(
            near, far,
            "distinct scroll positions must paint distinct frames"
        );
        assert_eq!(far, far_repeat, "the same scroll position must be stable");
        assert_eq!(
            near, near_after_far,
            "an intervening capture must not accumulate scroll movement"
        );
        assert_eq!(prepared.layout().rects, base_rects);
        assert_eq!(prepared.layout().translates, base_translates);
        assert_eq!(prepared.layout().clip_rects, base_clips);
        let counts = counts.lock().expect("final loader counts");
        for url in [
            "https://assets.test/font.ttf",
            "https://assets.test/hero.svg",
            "https://assets.test/background.svg",
        ] {
            assert_eq!(counts.get(url), Some(&1), "{url} must load exactly once");
        }
        assert!(!counts.contains_key("https://assets.test/fallback.svg"));
        assert_eq!(resources.retained_entry_count(), 3);
        assert!(resources.retained_byte_len() > FONT_BYTES.len());
    }

    /// Portable pixel counterpart to the Chromium root-sticky geometry probe
    /// in obscura-js. It checks that sticky backgrounds and descendants move
    /// as one painted subtree, bottom sticking is visible, fixed remains
    /// viewport-anchored, and the top sticky eventually exits at its
    /// containing-block boundary.
    #[test]
    fn root_scroll_sticky_paints_subtrees_and_respects_bottom_boundary() {
        let tree = parse_html(
            r#"<html style="margin:0"><body style="margin:0;background:#220022">
                <div style="height:40px;background:#ffffff"></div>
                <div style="box-sizing:border-box;height:900px;padding:10px 12px;border:4px solid #333;background:#dddddd">
                    <div style="box-sizing:border-box;position:sticky;top:20px;height:60px;margin:6px;background:#ff0000">
                        <div style="height:12px;background:#0000ff"></div>
                    </div>
                    <div style="height:500px"></div>
                    <div style="box-sizing:border-box;position:sticky;bottom:15px;height:50px;margin:5px;background:#ff8800"></div>
                </div>
                <div style="height:700px;background:#220022"></div>
                <div style="position:fixed;z-index:10;left:600px;top:20px;width:60px;height:60px;background:#00ff00"></div>
            </body></html>"#,
        );
        let viewport = (800.0, 513.0);
        let top = paint_dom_scrolled(&tree, viewport, None, (0.0, 0.0)).unwrap();
        let stuck = paint_dom_scrolled(&tree, viewport, None, (0.0, 100.0)).unwrap();
        let bottom_normal =
            paint_dom_scrolled(&tree, viewport, None, (0.0, 400.0)).unwrap();
        let boundary =
            paint_dom_scrolled(&tree, viewport, None, (0.0, 9999.0)).unwrap();

        let is_color = |pixel: tiny_skia::PremultipliedColorU8, rgb: [u8; 3]| {
            (pixel.red() as i16 - rgb[0] as i16).abs() < 8
                && (pixel.green() as i16 - rgb[1] as i16).abs() < 8
                && (pixel.blue() as i16 - rgb[2] as i16).abs() < 8
        };
        assert!(is_color(top.pixel(100, 80).unwrap(), [255, 0, 0]));
        assert!(is_color(top.pixel(100, 460).unwrap(), [255, 136, 0]));
        assert!(is_color(stuck.pixel(100, 25).unwrap(), [0, 0, 255]));
        assert!(is_color(stuck.pixel(100, 50).unwrap(), [255, 0, 0]));
        assert!(is_color(stuck.pixel(100, 460).unwrap(), [255, 136, 0]));
        assert!(is_color(
            bottom_normal.pixel(100, 240).unwrap(),
            [255, 136, 0]
        ));
        assert!(is_color(boundary.pixel(100, 20).unwrap(), [34, 0, 34]));
        for pixmap in [&top, &stuck, &bottom_normal, &boundary] {
            assert!(is_color(pixmap.pixel(620, 30).unwrap(), [0, 255, 0]));
        }
    }

    #[test]
    fn object_fit_contain_and_cover_center_and_preserve_aspect() {
        // A 200x100 box (2:1) with a square 100x100 image, offset so centering
        // is checked against the box origin, not (0,0).
        let box_rect = crate::Rect { x: 10.0, y: 20.0, width: 200.0, height: 100.0 };
        let (iw, ih) = (100.0f32, 100.0f32);

        // Contain: the largest square fitting inside 200x100 is 100x100,
        // letterboxed horizontally and centered.
        let c = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Contain);
        assert!((c.width - 100.0).abs() < 0.01 && (c.height - 100.0).abs() < 0.01, "contain size {:?}", c);
        assert!((c.width / c.height - iw / ih).abs() < 1e-3, "contain preserves aspect: {:?}", c);
        assert!((c.x - 60.0).abs() < 0.01, "contain centered x (10 + (200-100)/2): {}", c.x);
        assert!((c.y - 20.0).abs() < 0.01, "contain centered y (20 + (100-100)/2): {}", c.y);
        // Contain always fits inside the box.
        assert!(c.x >= box_rect.x - 0.01 && c.x + c.width <= box_rect.x + box_rect.width + 0.01);
        assert!(c.y >= box_rect.y - 0.01 && c.y + c.height <= box_rect.y + box_rect.height + 0.01);

        // Cover: the smallest square covering 200x100 is 200x200, centered so
        // it overflows the box vertically (the paint path clips it).
        let v = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Cover);
        assert!((v.width - 200.0).abs() < 0.01 && (v.height - 200.0).abs() < 0.01, "cover size {:?}", v);
        assert!((v.width / v.height - iw / ih).abs() < 1e-3, "cover preserves aspect: {:?}", v);
        assert!((v.x - 10.0).abs() < 0.01, "cover centered x (10 + (200-200)/2): {}", v.x);
        assert!((v.y + 30.0).abs() < 0.01, "cover centered y (20 + (100-200)/2 = -30): {}", v.y);
        // Cover fully covers the box on both axes.
        assert!(v.x <= box_rect.x + 0.01 && v.x + v.width >= box_rect.x + box_rect.width - 0.01);
        assert!(v.y <= box_rect.y + 0.01 && v.y + v.height >= box_rect.y + box_rect.height - 0.01);

        // scale-down never upscales: a 100x100 image in a 200x200 box stays
        // 100x100 (Contain would grow it to 200x200), centered.
        let box2 = crate::Rect { x: 0.0, y: 0.0, width: 200.0, height: 200.0 };
        let sd = object_fit_dest(&box2, iw, ih, crate::ObjectFit::ScaleDown);
        assert!((sd.width - 100.0).abs() < 0.01 && (sd.height - 100.0).abs() < 0.01, "scale-down no upscale: {:?}", sd);
        assert!((sd.x - 50.0).abs() < 0.01 && (sd.y - 50.0).abs() < 0.01, "scale-down centered: {:?}", sd);
        let cn = object_fit_dest(&box2, iw, ih, crate::ObjectFit::Contain);
        assert!((cn.width - 200.0).abs() < 0.01, "contain upscales into the box: {:?}", cn);

        // None uses the intrinsic size regardless of box, centered.
        let n = object_fit_dest(&box2, iw, ih, crate::ObjectFit::None);
        assert!((n.width - 100.0).abs() < 0.01 && (n.height - 100.0).abs() < 0.01, "none intrinsic size: {:?}", n);
        assert!((n.x - 50.0).abs() < 0.01 && (n.y - 50.0).abs() < 0.01, "none centered: {:?}", n);

        // Fill stretches to exactly the box.
        let f = object_fit_dest(&box_rect, iw, ih, crate::ObjectFit::Fill);
        assert!((f.width - box_rect.width).abs() < 0.01 && (f.height - box_rect.height).abs() < 0.01, "fill: {:?}", f);
        assert!((f.x - box_rect.x).abs() < 0.01 && (f.y - box_rect.y).abs() < 0.01, "fill origin: {:?}", f);
    }

    #[test]
    fn paints_background_color() {
        let tree = parse_html(
            "<html><body><div style=\"background-color: #ff0000; width: 100px; height: 80px\"></div></body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        assert_eq!(pixmap.width(), 200);
        // The red div is laid out at the origin; sample inside it.
        let inside = pixmap.pixel(10, 10).expect("pixel");
        assert!(inside.red() > 200, "expected red bg, got {:?}", inside);
        assert!(inside.green() < 60);
        assert!(inside.blue() < 60);
        // Outside the 100x80 div the page background is white.
        let outside = pixmap.pixel(150, 150).expect("pixel");
        assert_eq!(outside.red(), 255);
        assert_eq!(outside.green(), 255);
        assert_eq!(outside.blue(), 255);
    }

    #[test]
    fn paints_border_when_var_is_adjacent_to_border_style_token() {
        let tree = parse_html(
            r##"<html><head><style>
                body { margin:0 }
                #target {
                    --stroke:2px;
                    --ink:#e11d48;
                    width:30px;
                    height:30px;
                    border:var(--stroke)solid var(--ink);
                    background:#fff;
                }
            </style></head><body><div id="target"></div></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (60.0, 60.0), None).expect("pixmap");
        let border = pixmap.pixel(1, 1).expect("border pixel");
        assert!(
            border.red() > 200 && border.green() < 70 && border.blue() < 100,
            "adjacent var() substitution must retain a painted red border: {border:?}"
        );
        let interior = pixmap.pixel(5, 5).expect("interior pixel");
        assert!(
            interior.red() > 245 && interior.green() > 245 && interior.blue() > 245,
            "the border must not consume the content box: {interior:?}"
        );
    }

    #[test]
    fn percentage_border_radius_paints_circles_ellipses_and_replaced_clips() {
        let tree = parse_html(
            r##"<html><body style="margin:0">
               <div id="circle" style="position:absolute;left:0;top:0;width:40px;height:40px;
                    border-radius:50%;background:#ff0000"></div>
               <div id="ellipse" style="position:absolute;left:50px;top:0;width:80px;height:40px;
                    border-radius:50%;background:#0000ff"></div>
               <div id="pill" style="position:absolute;left:140px;top:0;width:80px;height:40px;
                    border-radius:20px;background:#00aa00"></div>
               <img alt="" src="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='40'%20height='40'%3E%3Crect%20width='40'%20height='40'%20fill='%23800080'/%3E%3C/svg%3E"
                    style="position:absolute;left:230px;top:0;width:40px;height:40px;border-radius:50%">
               </body></html>"##,
        );
        let pixmap = paint_dom(&tree, (280.0, 50.0), None).expect("pixmap");
        let is_white = |x, y| {
            let pixel = pixmap.pixel(x, y).expect("pixel");
            pixel.red() > 245 && pixel.green() > 245 && pixel.blue() > 245
        };
        assert!(is_white(1, 1), "a square 50% radius must clear its corner");
        let circle_top = pixmap.pixel(20, 1).expect("circle top");
        assert!(circle_top.red() > 200 && circle_top.green() < 40 && circle_top.blue() < 40);

        assert!(is_white(60, 3), "a rectangular 50% radius must use a 40x20 elliptical corner");
        let ellipse_top = pixmap.pixel(90, 1).expect("ellipse top");
        let ellipse_left = pixmap.pixel(51, 20).expect("ellipse left");
        for pixel in [ellipse_top, ellipse_left] {
            assert!(pixel.blue() > 200 && pixel.red() < 40 && pixel.green() < 40);
        }

        let pill_corner = pixmap.pixel(150, 3).expect("pixel radius pill corner");
        assert!(
            pill_corner.green() > 100 && pill_corner.red() < 40 && pill_corner.blue() < 40,
            "a 20px radius must remain circular rather than resolving like 50%: {pill_corner:?}"
        );

        assert!(is_white(231, 1), "a circular replaced image must clip its raster corner");
        let image_center = pixmap.pixel(250, 20).expect("image center");
        assert!(image_center.red() > 80 && image_center.blue() > 80 && image_center.green() < 40);
    }

    #[test]
    fn auto_background_size_uses_intrinsic_dimensions_and_position() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:100px;height:100px;background-color:red;
                 background-image:url(&quot;data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='20'%20height='10'%3E%3Crect%20width='20'%20height='10'%20fill='blue'/%3E%3C/svg%3E&quot;);
                 background-position:right bottom;background-repeat:no-repeat"></div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (120.0, 120.0), None).expect("pixmap");
        let background = pixmap.pixel(10, 10).expect("pixel");
        assert!(
            background.red() > 200 && background.blue() < 60,
            "the intrinsic image must not stretch across the owner"
        );
        let image = pixmap.pixel(90, 95).expect("pixel");
        assert!(
            image.blue() > 200 && image.red() < 60,
            "the 20x10 intrinsic image must anchor at bottom right"
        );
    }

    #[test]
    fn ordered_background_gradients_paint_every_translucent_layer() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:100px;height:100px;background-color:white;
                 background-image:
                   linear-gradient(180deg,transparent,white 85%),
                   radial-gradient(circle at top left,rgba(255,0,0,.9),transparent 50%),
                   radial-gradient(circle at top right,rgba(0,0,255,.9),transparent 50%)">
               </div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (100.0, 100.0), None).expect("pixmap");
        let left = pixmap.pixel(4, 4).expect("top-left gradient");
        let right = pixmap.pixel(95, 4).expect("top-right gradient");
        assert!(
            left.red() > left.blue() + 80,
            "the top-left radial layer must tint its own corner: {left:?}"
        );
        assert!(
            right.blue() > right.red() + 80,
            "the later top-right radial layer must remain beneath the authored top layer: {right:?}"
        );
    }

    #[test]
    fn repeating_linear_gradient_tiles_at_background_size_over_ordered_layer() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:40px;height:40px;background-color:white;
                 background-image:
                   repeating-linear-gradient(315deg,
                     rgba(0,0,0,.65) 0,rgba(0,0,0,.65) 1px,
                     transparent 0,transparent 50%),
                   linear-gradient(red,red);
                 background-size:10px 10px">
               </div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (40.0, 40.0), None).expect("pixmap");
        let mut dark = 0usize;
        let mut red = 0usize;
        for y in 0..10 {
            for x in 0..10 {
                let first = pixmap.pixel(x, y).expect("first tile");
                let repeated = pixmap.pixel(x + 20, y + 20).expect("repeated tile");
                assert_eq!(
                    first, repeated,
                    "background-size must establish a stable 10px tile at ({x},{y})"
                );
                if first.red() < 150 {
                    dark += 1;
                } else if first.red() > 220 && first.green() < 40 && first.blue() < 40 {
                    red += 1;
                }
            }
        }
        assert!(dark > 4, "the repeating hatch must paint dark stripes");
        assert!(
            red > 20,
            "transparent hatch gaps must reveal the ordered red layer"
        );
    }

    #[test]
    fn length_background_positions_select_sprite_frames() {
        let sprite = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='48'%20height='24'%3E%3Crect%20width='24'%20height='24'%20fill='%23ff0000'/%3E%3Crect%20x='24'%20width='24'%20height='24'%20fill='%230000ff'/%3E%3C/svg%3E";
        let tree = parse_html(&format!(
            r#"<html><body style="margin:0;background:white">
               <div style="position:absolute;left:0;top:0;width:24px;height:42px;
                 background-color:black;background-image:url(&quot;{sprite}&quot;);
                 background-size:48px 24px;background-position:0;background-repeat:no-repeat"></div>
               <div style="position:absolute;left:30px;top:0;width:24px;height:42px;
                 background-color:black;background-image:url(&quot;{sprite}&quot;);
                 background-size:48px 24px;background-position:-24px;background-repeat:no-repeat"></div>
               </body></html>"#
        ));
        let pixmap = paint_dom(&tree, (60.0, 45.0), None).expect("pixmap");

        let first = pixmap.pixel(12, 21).expect("first sprite frame");
        assert!(
            first.red() > 200 && first.green() < 40 && first.blue() < 40,
            "`background-position:0` must anchor the first frame at the start edge: {first:?}"
        );
        let second = pixmap.pixel(42, 21).expect("second sprite frame");
        assert!(
            second.blue() > 200 && second.red() < 40 && second.green() < 40,
            "a negative pixel position must select the next sprite frame: {second:?}"
        );
        let above = pixmap.pixel(12, 2).expect("vertical centering");
        assert!(
            above.red() < 30 && above.green() < 30 && above.blue() < 30,
            "a one-value horizontal position must default the vertical axis to center: {above:?}"
        );
    }

    #[test]
    fn negative_text_indent_clips_label_without_hiding_background_icon() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <a style="display:block;width:24px;height:24px;overflow:hidden;
                 white-space:nowrap;text-indent:-9999px;color:black;background-color:blue;
                 background-image:url(&quot;data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='16'%20height='16'%3E%3Crect%20width='16'%20height='16'%20fill='red'/%3E%3C/svg%3E&quot;);
                 background-position:center;background-size:16px 16px;background-repeat:no-repeat">Bluesky (@mozilla.org)</a>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (32.0, 32.0), None).expect("pixmap");
        let corner = pixmap.pixel(1, 1).expect("anchor background");
        assert!(
            corner.blue() > 200 && corner.red() < 40 && corner.green() < 40,
            "the anchor background must remain visible: {corner:?}"
        );
        let icon = pixmap.pixel(12, 12).expect("centered SVG icon");
        assert!(
            icon.red() > 200 && icon.green() < 40 && icon.blue() < 40,
            "the background SVG must paint independently of indented text: {icon:?}"
        );
        for y in 0..24 {
            for x in 0..24 {
                let pixel = pixmap.pixel(x, y).expect("anchor pixel");
                assert!(
                    pixel.red() > 40 || pixel.green() > 40 || pixel.blue() > 40,
                    "black label glyph leaked through the 24px overflow clip at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn contextual_background_size_preserves_auto_axis_ratio() {
        let source = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='200'%20height='50'%3E%3C/svg%3E";
        let owner = crate::Rect {
            x: 0.0,
            y: 0.0,
            width: 132.0,
            height: 60.0,
        };
        let mut cache = RenderResourceCache::default();
        let image = background_image_rect(
            source,
            None,
            &owner,
            None,
            Some("calc(100% - 2rem) auto"),
            None,
            crate::BackgroundPosition::new(
                crate::BackgroundPositionAxis::percentage(0.0),
                crate::BackgroundPositionAxis::percentage(0.5),
            ),
            10.0,
            10.0,
            (1280.0, 720.0),
            &mut cache,
        )
        .unwrap();
        assert_eq!(image.width, 112.0);
        assert_eq!(image.height, 28.0);
        assert_eq!(image.y, 16.0);
    }

    #[test]
    fn paints_positioned_empty_pseudo_background_box() {
        let tree = parse_html(
            r#"<html><head><style>
               body { margin:0 }
               #host { position:relative; width:100px; height:50px }
               #host::before {
                 content:"";
                 position:absolute;
                 top:10px;
                 left:20px;
                 width:40px;
                 height:30px;
                 background:
                   linear-gradient(to bottom, transparent, #ffffff),
                   radial-gradient(circle at 50% 50%, #ebf3f9, #d6dee4);
               }
               </style></head><body><div id="host"></div></body></html>"#,
        );
        let pixmap = paint_dom(&tree, (120.0, 80.0), None).expect("pixmap");
        let center = pixmap.pixel(40, 25).expect("pixel");
        assert!(
            center.red() >= 214 && center.green() >= 222 && center.blue() >= 228,
            "transparent-to-white over a light radial layer must not darken it: {center:?}"
        );
        let outside = pixmap.pixel(5, 5).expect("pixel");
        assert_eq!(
            (outside.red(), outside.green(), outside.blue()),
            (255, 255, 255)
        );
    }

    #[test]
    fn polygon_clip_path_paints_responsive_geometry_on_elements_and_pseudos() {
        let tree = parse_html(
            r#"<html><head><style>
               html, body { margin:0 }
               @supports (clip-path:polygon(0 0,100% 0,50% 100%)) {
                 .triangle, #in-flow::before, #positioned::after {
                   clip-path:polygon(0 0,100% 0,50% 100%);
                 }
               }
               .triangle { position:absolute; left:0; top:0;
                 width:80px; height:80px; background:#f00 }
               #in-flow { position:absolute; left:100px; top:0 }
               #in-flow::before { content:""; display:block;
                 width:80px; height:80px; background:#0a0 }
               #positioned { position:absolute; left:200px; top:0;
                 width:80px; height:80px }
               #positioned::after { content:""; position:absolute; inset:0;
                 background:#00f }
               </style></head><body>
                 <div class="triangle"></div>
                 <div id="in-flow"></div>
                 <div id="positioned"></div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (300.0, 100.0), None).expect("pixmap");
        for (name, left, channel) in [
            ("ordinary element", 0, 0),
            ("in-flow pseudo", 100, 1),
            ("positioned pseudo", 200, 2),
        ] {
            let inside = pixmap.pixel(left + 40, 65).expect("inside pixel");
            let colored = match channel {
                0 => inside.red() > 180 && inside.green() < 80 && inside.blue() < 80,
                1 => inside.green() > 100 && inside.red() < 80 && inside.blue() < 80,
                _ => inside.blue() > 180 && inside.red() < 80 && inside.green() < 80,
            };
            assert!(colored, "{name} must paint inside its percentage polygon: {inside:?}");
            let outside = pixmap.pixel(left + 4, 65).expect("outside pixel");
            assert_eq!(
                (outside.red(), outside.green(), outside.blue()),
                (255, 255, 255),
                "{name} must be clipped outside its triangle: {outside:?}"
            );
        }
    }

    #[test]
    fn polygon_clip_path_honors_evenodd_fill_rule() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:100px;height:100px;background:#e00;
                 clip-path:polygon(evenodd,
                   0 0,100% 0,100% 100%,0 100%,0 25%,
                   75% 25%,75% 75%,25% 75%,25% 25%,0 25%)"></div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (120.0, 120.0), None).expect("pixmap");
        let shell = pixmap.pixel(10, 50).expect("outer shell");
        assert!(
            shell.red() > 180 && shell.green() < 80,
            "the outer contour must remain painted: {shell:?}"
        );
        let hole = pixmap.pixel(50, 50).expect("inner hole");
        assert_eq!(
            (hole.red(), hole.green(), hole.blue()),
            (255, 255, 255),
            "evenodd must cut the inner winding out of the clip: {hole:?}"
        );
    }

    #[test]
    fn degenerate_polygon_clip_path_clips_the_element_away() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:80px;height:80px;background:red;
                 clip-path:polygon(20px 20px)"></div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (100.0, 100.0), None).expect("pixmap");
        for (x, y) in [(10, 10), (20, 20), (40, 40)] {
            let pixel = pixmap.pixel(x, y).expect("pixel");
            assert_eq!(
                (pixel.red(), pixel.green(), pixel.blue()),
                (255, 255, 255),
                "a zero-area polygon must not fall back to an unclipped box: {pixel:?}"
            );
        }
    }

    #[test]
    fn paints_generated_style_images_and_sizes_content_url_as_replaced_content() {
        let tree = parse_html(
            r#"<html><head><style>
               body { margin:0 }
               #host { position:relative; width:100px; height:40px }
               #host::before {
                 content:""; position:absolute; left:0; top:0;
                 width:40px; height:40px;
                 background-image:url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='40'%20height='40'%3E%3Crect%20width='40'%20height='40'%20fill='blue'/%3E%3C/svg%3E");
                 background-size:100% 100%;
               }
               #host::after {
                 content:""; position:absolute; left:50px; top:0;
                 width:40px; height:40px; background-color:red;
                 mask-image:url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='40'%20height='40'%3E%3Ccircle%20cx='20'%20cy='20'%20r='16'%20fill='white'/%3E%3C/svg%3E");
                 mask-size:40px 40px; mask-repeat:no-repeat;
               }
               #content-image {
                 display:block;
                 content:url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='30'%20height='20'%3E%3Crect%20width='30'%20height='20'%20fill='lime'/%3E%3C/svg%3E");
               }
               </style></head><body>
                 <div id="host"></div><img id="content-image" alt="">
               </body></html>"#,
        );

        // The first cascade exposes the style image. Feeding its decoded
        // dimensions back through the ordinary intrinsic map must give the
        // source-less replaced element its 30x20 CSS box.
        let mut intrinsic = std::collections::HashMap::new();
        let first = layout_dom_with_web_fonts(&tree, (120.0, 80.0), &intrinsic, &[]);
        let host_id = tree
            .query_selector("#host")
            .expect("selector")
            .expect("host");
        let host_style = &first.styles[&host_id];
        assert!(
            host_style
                .before_pseudo
                .as_deref()
                .and_then(|style| style.background_image.as_deref())
                .is_some(),
            "the positioned pseudo must retain its parsed URL background"
        );
        assert!(
            host_style
                .after_pseudo
                .as_deref()
                .and_then(|style| style.mask_image.as_deref())
                .is_some(),
            "the positioned pseudo must retain its parsed URL mask"
        );
        let host_rect = first.rects[&host_id];
        assert_eq!(
            (host_rect.x, host_rect.y, host_rect.width, host_rect.height),
            (0.0, 0.0, 100.0, 40.0)
        );
        let mut cache = RenderResourceCache::default();
        let mut selected = HashMap::new();
        assert!(collect_content_image_intrinsics(
            &tree,
            &first.styles,
            None,
            &mut cache,
            &mut intrinsic,
            &mut selected,
        ));
        let laid = layout_dom_with_web_fonts(&tree, (120.0, 80.0), &intrinsic, &[]);
        let image_id = tree
            .query_selector("#content-image")
            .expect("selector")
            .expect("content image");
        let image_rect = laid.rects[&image_id];
        assert_eq!(
            (
                image_rect.x,
                image_rect.y,
                image_rect.width,
                image_rect.height
            ),
            (0.0, 40.0, 30.0, 20.0),
            "content:url must use ordinary replaced-element geometry"
        );

        let pixmap = paint_dom(&tree, (120.0, 80.0), None).expect("pixmap");
        let blue = pixmap.pixel(20, 20).expect("blue pseudo");
        assert!(
            blue.blue() > 220 && blue.red() < 40 && blue.green() < 80,
            "positioned pseudo background-image must paint: {blue:?}"
        );
        let red = pixmap.pixel(70, 20).expect("masked pseudo center");
        assert!(
            red.red() > 220 && red.green() < 40 && red.blue() < 40,
            "positioned pseudo mask center must use the authored fill: {red:?}"
        );
        let transparent_corner = pixmap.pixel(51, 1).expect("mask corner");
        assert_eq!(
            (
                transparent_corner.red(),
                transparent_corner.green(),
                transparent_corner.blue(),
            ),
            (255, 255, 255),
            "transparent mask corners must not paint the pseudo's solid box"
        );
        let green = pixmap.pixel(15, 50).expect("content image");
        assert!(
            green.green() > 220 && green.red() < 40 && green.blue() < 40,
            "content:url image must paint through the replaced-image path: {green:?}"
        );
    }

    #[test]
    fn repeated_data_svg_masks_sample_radial_sources_on_every_box_path() {
        let tree = parse_html(
            r##"<html><head><style>
               html, body { margin:0 }
               .mask {
                 width:88px; height:66px;
               }
               .mask-source, #in-flow::before, #positioned::after {
                 mask-image:url("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='72' height='72' viewBox='0 0 72 72'><defs><pattern id='p' patternUnits='userSpaceOnUse' width='72' height='72'><g transform='translate(36 36) rotate(-60)'><line x1='-10' y1='0' x2='10' y2='0' stroke='white' stroke-width='3' stroke-linecap='round'/></g></pattern></defs><rect width='100%' height='100%' fill='url(%23p)'/></svg>");
                 mask-size:22px 22px; mask-repeat:repeat;
               }
               #ordinary { position:absolute; left:0; top:0 }
               #in-flow { position:absolute; left:100px; top:0 }
               #in-flow::before {
                 content:""; display:block;
                 width:88px; height:66px;
                 background:radial-gradient(circle at 50% 125%,transparent 20%,#f627e3 35%,#6911d2 55%,transparent 75%);
               }
               #positioned { position:absolute; left:200px; top:0 }
               #positioned::after {
                 content:""; position:absolute; inset:0;
                 background:radial-gradient(circle at 50% 125%,transparent 20%,#f627e3 35%,#6911d2 55%,transparent 75%);
               }
               #ordinary {
                 background:radial-gradient(circle at 50% 125%,transparent 20%,#f627e3 35%,#6911d2 55%,transparent 75%);
               }
               #solid-source { position:absolute; left:0; top:80px; background-color:#00aa00 }
               #linear-source { position:absolute; left:100px; top:80px; background:linear-gradient(90deg,#ff0000,#0000ff) }
               #conic-source { position:absolute; left:200px; top:80px; background:conic-gradient(from 0deg at 50% 50%,#ff0000,#0000ff,#ff0000) }
               </style></head><body>
                 <div id="ordinary" class="mask mask-source"></div>
                 <div id="in-flow" class="mask"></div>
                 <div id="positioned" class="mask"></div>
                 <div id="solid-source" class="mask mask-source"></div>
                 <div id="linear-source" class="mask mask-source"></div>
                 <div id="conic-source" class="mask mask-source"></div>
               </body></html>"##,
        );
        let pixmap = paint_dom(&tree, (300.0, 160.0), None).expect("pixmap");
        let count_pixels = |left: u32, top: u32, predicate: fn(u8, u8, u8) -> bool| {
            (top..top + 66)
                .flat_map(|y| (left..left + 88).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let pixel = pixmap.pixel(x, y).expect("pixel");
                    predicate(pixel.red(), pixel.green(), pixel.blue())
                })
                .count()
        };
        let is_radial_color = |red, green, blue| {
            red > 70 && blue > 100 && blue as u16 > green as u16 * 2
        };
        for (name, left) in [("ordinary element", 0), ("in-flow pseudo", 100), ("positioned pseudo", 200)] {
            let colored = count_pixels(left, 0, is_radial_color);
            assert!(
                colored > 20,
                "{name} must sample the radial source through the repeated SVG mask, found {colored} colored pixels"
            );
            let black = count_pixels(left, 0, |red, green, blue| red < 20 && green < 20 && blue < 20);
            assert_eq!(
                black, 0,
                "{name} must not fall back to the default black mask fill"
            );
        }
        assert!(
            count_pixels(0, 80, |red, green, blue| green > 100 && red < 40 && blue < 40) > 20,
            "solid mask sources must keep painting"
        );
        assert!(
            count_pixels(100, 80, |red, green, blue| (red > 100 || blue > 100) && green < 80) > 20,
            "linear-gradient mask sources must keep painting"
        );
        assert!(
            count_pixels(200, 80, |red, green, blue| (red > 100 || blue > 100) && green < 80) > 20,
            "conic-gradient mask sources must keep painting"
        );
    }

    #[test]
    fn paints_empty_in_flow_generated_block_at_its_layout_rect() {
        let tree = parse_html(
            r#"<html><head><style>
               html, body { margin:0 }
               body { font-size:20px; line-height:20px }
               #host { width:200px }
               #host::before {
                 content:""; display:block; width:80px; height:40px;
                 margin-bottom:10px; background:#0066cc;
               }
               #next { width:20px; height:10px; background:#00aa00 }
               </style></head><body>
                 <div id="host">TEXT</div><div id="next"></div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (240.0, 100.0), None).expect("pixmap");
        let generated = pixmap.pixel(40, 20).expect("generated block pixel");
        assert!(
            generated.blue() > 180 && generated.green() > 70 && generated.red() < 30,
            "the anonymous generated box must paint its own background: {generated:?}"
        );
        let following = pixmap.pixel(10, 75).expect("following block pixel");
        assert!(
            following.green() > 120 && following.red() < 30 && following.blue() < 30,
            "the following block must paint below the generated geometry: {following:?}"
        );
    }

    #[test]
    fn paints_positioned_attr_content_over_the_host_background() {
        let tree = parse_html(
            r#"<html><head><style>
               body { margin:0 }
               #cta {
                 position:relative; width:120px; height:40px; border:0;
                 padding:0; color:transparent; background:red;
               }
               #cta::before {
                 content:attr(data-label);
                 position:absolute; inset:1px;
                 display:flex; align-items:center; justify-content:center;
                 border-radius:4px; color:black; background:white;
               }
               </style></head><body>
               <button id="cta" data-label="Get Started">Get Started</button>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (140.0, 60.0), None).expect("pixmap");
        let inner = pixmap.pixel(5, 5).expect("inner pixel");
        assert_eq!(
            (inner.red(), inner.green(), inner.blue()),
            (255, 255, 255),
            "the generated box must cover the red host background"
        );
        let dark_pixels = (35..85)
            .flat_map(|x| (8..32).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                let pixel = pixmap.pixel(x, y).unwrap();
                pixel.red() < 100 && pixel.green() < 100 && pixel.blue() < 100
            })
            .count();
        assert!(dark_pixels > 10, "generated attr() text must be painted");
    }

    #[test]
    fn later_positioned_pseudo_opaquely_covers_the_earlier_one() {
        let tree = parse_html(
            r#"<html><head><style>
               body { margin:0 }
               #cta {
                 position:relative; width:120px; height:40px; padding:0;
                 color:transparent; background:black;
               }
               #cta::before {
                 content:"before";
                 position:absolute; inset:1px;
                 display:flex; align-items:center; justify-content:center;
                 color:red; background:red;
               }
               #cta::after {
                 content:"after";
                 position:absolute; inset:1px;
                 display:flex; align-items:center; justify-content:center;
                 color:blue; background:white;
               }
               </style></head><body><button id="cta">host</button></body></html>"#,
        );
        let pixmap = paint_dom(&tree, (140.0, 60.0), None).expect("pixmap");
        let inner = pixmap.pixel(5, 5).expect("inner pixel");
        assert_eq!(
            (inner.red(), inner.green(), inner.blue()),
            (255, 255, 255),
            "::after's opaque background must cover ::before"
        );
        let red_pixels = (1..119)
            .flat_map(|x| (1..39).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                let pixel = pixmap.pixel(x, y).unwrap();
                pixel.red() > 180 && pixel.green() < 80 && pixel.blue() < 80
            })
            .count();
        assert_eq!(red_pixels, 0, "::before must not bleed through ::after");
    }

    #[test]
    fn angular_absolute_primary_button_paints_one_generated_label() {
        let tree = parse_html(
            r#"<html><head><style>
               :root {
                 --orange-red:#f00; --vivid-pink:#f0f; --electric-violet:#70f;
                 --page-bg-radial-gradient:radial-gradient(circle,#fff 0%,#fff 100%);
                 --page-background:#fff; --primary-contrast:#111;
               }
               html, body { margin:0 }
               .section { position:relative; width:500px; height:300px }
               .content button { position:absolute; bottom:48px }
               .docs-primary-btn {
                 cursor:pointer; border:none; outline:none; position:relative;
                 border-radius:4px; padding:12px 24px; width:max-content;
                 color:transparent; font-size:14px; font-weight:600;
                 background:linear-gradient(90deg,var(--orange-red) 0%,
                   var(--vivid-pink) 50%,var(--electric-violet) 100%);
               }
               .docs-primary-btn::before {
                 content:attr(text); position:absolute; inset:1px;
                 background:var(--page-bg-radial-gradient); border-radius:3px;
                 display:flex; align-items:center; justify-content:center;
                 color:var(--primary-contrast);
               }
               .docs-primary-btn::after {
                 content:attr(text); position:absolute; inset:1px;
                 background:var(--page-background); border-radius:3px;
                 display:flex; align-items:center; justify-content:center;
                 color:var(--primary-contrast);
               }
               </style></head><body>
                 <section class="section"><div class="content">
                   <button id="cta" class="docs-primary-btn" text="Learn more">Learn more</button>
                 </div></section>
               </body></html>"#,
        );
        let laid = crate::layout_dom(&tree, (500.0, 300.0));
        let cta = tree.get_element_by_id("cta").unwrap();
        let rect = laid.rects[&cta];
        let style = &laid.styles[&cta];
        assert_eq!(style.color, Some([0, 0, 0, 0]));
        let pixmap = paint_dom(&tree, (500.0, 300.0), None).expect("pixmap");
        let rows = (rect.y.floor() as u32..(rect.y + rect.height).ceil() as u32)
            .map(|y| {
                (rect.x.floor() as u32..(rect.x + rect.width).ceil() as u32)
                    .filter(|&x| {
                        let pixel = pixmap.pixel(x, y).unwrap();
                        pixel.red() < 100 && pixel.green() < 100 && pixel.blue() < 100
                    })
                    .count()
            })
            .collect::<Vec<_>>();
        let ink_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(row, &ink)| (ink > 0).then_some(row))
            .collect::<Vec<_>>();
        assert!(
            ink_rows.last().unwrap() - ink_rows.first().unwrap() <= 14,
            "only the generated label may paint; transparent host text leaked across rows {rows:?}"
        );
    }

    #[test]
    fn native_select_paints_only_the_selected_label_and_arrow() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <select id="theme">
                    <option>Light</option>
                    <option selected>Dark</option>
                </select>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (160.0, 60.0), None).expect("pixmap");
        let dark_pixels = (0..120)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                let pixel = pixmap.pixel(x, y).unwrap();
                pixel.red() < 100 && pixel.green() < 100 && pixel.blue() < 100
            })
            .count();
        assert!(
            dark_pixels > 20,
            "selected label, border, and disclosure arrow should paint"
        );
        let select = tree.get_element_by_id("theme").unwrap();
        assert_eq!(selected_option_label(&tree, select).as_deref(), Some("Dark"));
    }

    #[test]
    fn authored_combobox_button_paints_css_math_border() {
        let tree = parse_html(
            r#"<html style="margin:0"><head><style>
                :root {
                    --stroke-standard: calc(1 * 1px);
                    --control-height: calc(4px * 10);
                    --control-radius: calc(4px * 2);
                }
                #host { width:260px }
                #language {
                    display:flex;
                    align-items:center;
                    justify-content:space-between;
                    width:100%;
                    height:var(--control-height);
                    box-sizing:border-box;
                    border-style:solid;
                    border-width:var(--stroke-standard);
                    border-color:rgba(208,217,251,.4);
                    border-radius:var(--control-radius);
                    padding:1px 12px;
                }
            </style></head><body style="margin:0">
                <div id="host">
                    <button id="language" role="combobox">
                        <span>English (United States)</span><span>▼</span>
                    </button>
                </div>
            </body></html>"#,
        );
        let laid = crate::layout_dom(&tree, (320.0, 80.0));
        let language = tree.get_element_by_id("language").unwrap();
        let rect = laid.rects[&language];
        let style = &laid.styles[&language];
        assert_eq!((rect.width, rect.height), (260.0, 40.0));
        assert_eq!(
            style.border,
            crate::Edges {
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
                left: 1.0,
            }
        );

        let pixmap = paint_dom(&tree, (320.0, 80.0), None).expect("pixmap");
        let painted_top_edge = (20..240)
            .filter(|&x| pixmap.pixel(x, 0).is_some_and(|pixel| pixel.alpha() > 0))
            .count();
        assert!(
            painted_top_edge > 200,
            "the rounded authored border should paint across the control: {painted_top_edge}"
        );
    }

    #[test]
    fn fractional_opacity_composites_boxes_and_inline_svg_once() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div style="width:20px;height:20px;background:black;opacity:.05"></div>
                <svg style="position:absolute;left:30px;top:0;opacity:.05"
                     width="20" height="20" viewBox="0 0 20 20">
                    <rect width="20" height="20" fill="black"/>
                </svg>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (60.0, 30.0), None).expect("pixmap");
        for (label, x) in [("box", 10), ("svg", 40)] {
            let pixel = pixmap.pixel(x, 10).expect("pixel");
            assert!(
                (240..=244).contains(&pixel.red())
                    && pixel.red() == pixel.green()
                    && pixel.green() == pixel.blue(),
                "{label} opacity:.05 should composite black once over white: {pixel:?}"
            );
        }
    }

    #[test]
    fn opacity_is_applied_to_overlapping_children_as_one_group() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div style="position:relative;width:30px;height:20px;opacity:.5">
                    <div style="position:absolute;left:0;width:20px;height:20px;background:black"></div>
                    <div style="position:absolute;left:10px;width:20px;height:20px;background:black"></div>
                </div>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (40.0, 30.0), None).expect("pixmap");
        let samples = [5, 15, 25].map(|x| pixmap.pixel(x, 10).unwrap().red());
        assert!(
            samples.iter().all(|channel| (126..=129).contains(channel)),
            "single-child and overlap regions must receive the same group alpha: {samples:?}"
        );
    }

    #[test]
    fn nested_opacity_groups_multiply_at_composite_boundaries() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div style="width:20px;height:20px;opacity:.5">
                    <div style="width:20px;height:20px;background:black;opacity:.5"></div>
                </div>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (30.0, 30.0), None).expect("pixmap");
        let pixel = pixmap.pixel(10, 10).expect("pixel");
        assert!(
            (190..=193).contains(&pixel.red())
                && pixel.red() == pixel.green()
                && pixel.green() == pixel.blue(),
            "nested .5 groups should produce .25 black over white: {pixel:?}"
        );
    }

    #[test]
    fn opacity_group_contains_z_order_and_preserves_clip_transforms() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div style="position:relative;width:20px;height:20px;
                            overflow:hidden;opacity:.5">
                    <div style="position:absolute;z-index:999;left:0;top:0;
                                transform:translate(10px,0);width:20px;height:20px;
                                background:red"></div>
                </div>
                <div style="position:absolute;left:30px;top:0;width:20px;height:20px;
                            opacity:.5">
                    <div style="position:absolute;z-index:999;width:20px;height:20px;
                                background:red"></div>
                </div>
                <div style="position:absolute;left:30px;top:0;width:20px;height:20px;
                            background:blue"></div>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (60.0, 30.0), None).expect("pixmap");
        let clipped_out = pixmap.pixel(5, 10).unwrap();
        let clipped_in = pixmap.pixel(15, 10).unwrap();
        let outside = pixmap.pixel(25, 10).unwrap();
        assert_eq!(
            (clipped_out.red(), clipped_out.green(), clipped_out.blue()),
            (255, 255, 255)
        );
        assert!(
            clipped_in.red() > 240 && (126..=129).contains(&clipped_in.green()),
            "translated child should be clipped then group-composited: {clipped_in:?}"
        );
        assert_eq!(
            (outside.red(), outside.green(), outside.blue()),
            (255, 255, 255)
        );
        let covered = pixmap.pixel(40, 10).unwrap();
        assert!(
            covered.blue() > 240 && covered.red() < 20,
            "high-z descendant must stay inside the earlier opacity group: {covered:?}"
        );
    }

    #[test]
    fn static_flex_and_grid_item_z_index_paints_each_subtree_atomically() {
        let blue_image = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='100'%20height='60'%3E%3Crect%20width='100'%20height='60'%20fill='blue'/%3E%3C/svg%3E";
        let tree = parse_html(&format!(
            r#"<html><head><style>
                 body {{ margin:0 }}
                 .row {{
                   position:relative; width:120px; height:70px;
                   display:flex; align-items:flex-start;
                 }}
                 .grid {{
                   width:120px; height:70px; display:grid;
                   grid-template-columns:100px;
                   grid-template-rows:60px;
                 }}
                 .front {{
                   box-sizing:border-box; width:100px; height:60px;
                   z-index:2; background:#ff0000; border:5px solid #00ff00;
                   color:#000000; font-size:24px; line-height:30px;
                 }}
                 .cover {{
                   position:absolute; z-index:auto; left:0; top:0;
                   width:100px; height:60px; background:#0000ff;
                 }}
                 .cell {{ grid-area:1 / 1; }}
                 img.cell {{ width:100px; height:60px; }}
               </style></head><body>
                 <div class="row">
                   <div class="front">FLEX</div>
                   <div class="cover"></div>
                 </div>
                 <div class="grid">
                   <div class="front cell">GRID</div>
                   <img class="cell" alt="" src="{blue_image}">
                 </div>
                 <div class="row" style="height:60px">
                   <div class="front" style="z-index:1">LOW</div>
                   <div class="cover" style="z-index:3"></div>
                 </div>
               </body></html>"#
        ));
        let pixmap = paint_dom(&tree, (140.0, 210.0), None).expect("pixmap");

        let is_green = |x, y| {
            let pixel = pixmap.pixel(x, y).expect("border pixel");
            pixel.green() > 230 && pixel.red() < 30 && pixel.blue() < 30
        };
        let is_red = |x, y| {
            let pixel = pixmap.pixel(x, y).expect("background pixel");
            pixel.red() > 230 && pixel.green() < 30 && pixel.blue() < 30
        };
        assert!(
            is_green(2, 30) && is_red(80, 50),
            "the static flex item's border/background must paint above its later absolute sibling"
        );
        assert!(
            is_green(2, 100) && is_red(80, 120),
            "the static grid item's border/background must paint above its later image sibling"
        );

        for (label, y0) in [("flex", 0), ("grid", 70)] {
            let dark_ink = (8..92)
                .flat_map(|x| (y0 + 8..y0 + 42).map(move |y| (x, y)))
                .filter(|&(x, y)| {
                    let pixel = pixmap.pixel(x, y).expect("text pixel");
                    pixel.red() < 40 && pixel.green() < 40 && pixel.blue() < 40
                })
                .count();
            assert!(
                dark_ink > 20,
                "{label} item text must remain inside the raised atomic subtree, found {dark_ink} dark pixels"
            );
        }

        assert!(
            (0..60).all(|dy| (0..100).all(|x| {
                let pixel = pixmap.pixel(x, 140 + dy).expect("covered pixel");
                pixel.blue() > 230 && pixel.red() < 30 && pixel.green() < 30
            })),
            "a higher-z sibling must cover the lower item's background, border, and shaped text as one unit"
        );
    }

    #[test]
    fn overflow_clipping_keeps_physical_axes_independent() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div style="position:relative;width:20px;height:20px;
                            overflow-x:clip">
                    <div style="position:absolute;left:10px;top:30px;
                                width:20px;height:10px;background:red"></div>
                </div>
                <div style="position:absolute;left:40px;top:0;width:20px;height:20px;
                            overflow-y:clip">
                    <div style="position:absolute;left:30px;top:10px;
                                width:10px;height:20px;background:blue"></div>
                </div>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (90.0, 50.0), None).expect("pixmap");

        let x_inside_y_outside = pixmap.pixel(15, 35).unwrap();
        assert!(
            x_inside_y_outside.red() > 240 && x_inside_y_outside.blue() < 20,
            "overflow-x:clip must not clip a Brave-shaped absolute child on Y: \
             {x_inside_y_outside:?}"
        );
        assert_eq!(
            pixmap.pixel(25, 35).map(|p| (p.red(), p.green(), p.blue())),
            Some((255, 255, 255)),
            "overflow-x:clip must still clip X"
        );

        let y_inside_x_outside = pixmap.pixel(75, 15).unwrap();
        assert!(
            y_inside_x_outside.blue() > 240 && y_inside_x_outside.red() < 20,
            "overflow-y:clip must not clip the reciprocal X overflow: \
             {y_inside_x_outside:?}"
        );
        assert_eq!(
            pixmap.pixel(75, 25).map(|p| (p.red(), p.green(), p.blue())),
            Some((255, 255, 255)),
            "overflow-y:clip must still clip Y"
        );
    }

    #[test]
    fn generated_boxes_use_the_hosts_overflow_on_only_the_authored_axis() {
        let tree = parse_html(
            r#"<html><body style="margin:0">
                <div id="x"></div><div id="y"></div>
                <style>
                  #x { position:relative;width:20px;height:20px;overflow-x:clip }
                  #x::before { content:"";position:absolute;left:10px;top:30px;
                               width:20px;height:10px;background:red }
                  #y { position:absolute;left:40px;top:0;width:20px;height:20px;
                       overflow-y:clip }
                  #y::after { content:"";display:block;width:10px;height:30px;
                              margin-left:30px;background:blue }
                </style>
            </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (90.0, 50.0), None).expect("pixmap");

        let positioned_y_overflow = pixmap.pixel(15, 35).unwrap();
        assert!(
            positioned_y_overflow.red() > 240 && positioned_y_overflow.blue() < 20,
            "positioned pseudo must remain visible outside Y for overflow-x:clip: \
             {positioned_y_overflow:?}"
        );
        assert_eq!(
            pixmap.pixel(25, 35).map(|p| (p.red(), p.green(), p.blue())),
            Some((255, 255, 255)),
            "positioned pseudo must be clipped on X"
        );

        let in_flow_x_overflow = pixmap.pixel(75, 10).unwrap();
        assert!(
            in_flow_x_overflow.blue() > 240 && in_flow_x_overflow.red() < 20,
            "in-flow pseudo must remain visible outside X for overflow-y:clip: \
             {in_flow_x_overflow:?}"
        );
        assert_eq!(
            pixmap.pixel(75, 25).map(|p| (p.red(), p.green(), p.blue())),
            Some((255, 255, 255)),
            "in-flow pseudo must be clipped on Y"
        );
    }

    #[test]
    fn later_element_paints_over_earlier() {
        // A blue div nested inside a red one: both cover the origin, and blue
        // (a descendant, later in tree order) paints over red.
        let tree = parse_html(
            "<html><body>\
             <div style=\"background-color:red; width:100px; height:100px\">\
               <div style=\"background-color:blue; width:50px; height:50px\"></div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let p = pixmap.pixel(5, 5).expect("pixel");
        assert!(p.blue() > 200, "expected blue to paint over red, got {:?}", p);
    }

    #[test]
    fn nested_translate_accumulates_through_subtree() {
        // Parent red box (position:absolute at 0,0, 20x20) translated by
        // (50,60). Child blue box (10x10, in-flow at the red box's origin)
        // translated by an additional (30,0). The child's painted position must
        // be the SUM of both translates, (50+30, 60+0) = (80,60), proving an
        // ancestor's translate offsets the whole subtree on top of the node's
        // own translate.
        let tree = parse_html(
            "<html><body style=\"margin:0\">\
             <div style=\"position:relative; width:200px; height:200px\">\
               <div style=\"position:absolute; top:0; left:0; width:20px; height:20px; \
                            background:#ff0000; transform:translate(50px,60px)\">\
                 <div style=\"width:10px; height:10px; background:#0000ff; \
                              transform:translate(30px,0)\"></div>\
               </div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        // Child blue lands at (80..90, 60..70).
        let blue = pixmap.pixel(85, 65).expect("pixel");
        assert!(
            blue.blue() > 200 && blue.red() < 60,
            "expected blue child at accumulated offset (80,60), got {:?}",
            blue
        );
        // Parent red lands at (50..70, 60..80); sample where the blue child does
        // not cover.
        let red = pixmap.pixel(55, 75).expect("pixel");
        assert!(
            red.red() > 200 && red.blue() < 60,
            "expected red parent at its own translate (50,60), got {:?}",
            red
        );
        // Nothing painted at the pre-transform origin: both boxes moved away.
        let origin = pixmap.pixel(5, 5).expect("pixel");
        assert_eq!((origin.red(), origin.green(), origin.blue()), (255, 255, 255));
    }

    #[test]
    fn transformed_image_is_clipped_inside_overflow_border() {
        // CSS overflow clipping belongs to the owner's padding box. The
        // translated images paint after the viewport's border in tree order,
        // so a border-box clip would let them overwrite the border.
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="width:100px;height:60px;overflow:hidden;
                           border:4px solid red">
                 <div style="display:flex;transform:translate(-50px,0)">
                   <img alt="" src="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='100'%20height='60'%3E%3Crect%20width='100'%20height='60'%20fill='blue'/%3E%3C/svg%3E"
                        style="width:100px;height:60px;object-fit:cover;flex-shrink:0">
                   <img alt="" src="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='100'%20height='60'%3E%3Crect%20width='100'%20height='60'%20fill='blue'/%3E%3C/svg%3E"
                        style="width:100px;height:60px;object-fit:cover;flex-shrink:0">
                 </div>
               </div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (140.0, 90.0), None).expect("pixmap");

        for &(x, y) in &[(0, 30), (3, 30), (104, 30), (107, 30), (50, 0), (50, 67)] {
            let pixel = pixmap.pixel(x, y).expect("border pixel");
            assert!(
                pixel.red() > 220 && pixel.green() < 40 && pixel.blue() < 40,
                "translated image must not overwrite border pixel ({x},{y}): {pixel:?}"
            );
        }
        let content = pixmap.pixel(50, 30).expect("content pixel");
        assert!(
            content.blue() > 220 && content.red() < 40,
            "translated cover image must remain visible inside padding box: {content:?}"
        );
    }

    #[test]
    fn projected_image_transform_enters_overflow_clip() {
        // Chromium reduction: without the projected rotate/scale, this image is
        // wholly left of the clip. Its transformed right edge reaches x=53.14.
        let tree = parse_html(
            r#"<html><body style="margin:0">
               <div style="position:relative;width:120px;height:100px;overflow:hidden">
                 <div style="position:absolute;left:-60px;top:50px;transform-origin:0 0;
                             transform:rotateX(60deg) rotateZ(-45deg);scale:200%">
                   <img alt="" src="data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='40'%20height='40'%3E%3Crect%20width='40'%20height='40'%20fill='red'/%3E%3C/svg%3E"
                        style="display:block;width:40px;height:40px">
                 </div>
               </div>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (160.0, 120.0), None).expect("pixmap");

        assert!(
            (0..100).any(|y| (0..120).any(|x| {
                let pixel = pixmap.pixel(x, y).expect("inside viewport");
                pixel.red() > 220 && pixel.green() < 40 && pixel.blue() < 40
            })),
            "projected image should enter the overflow clip"
        );
        assert!(
            (0..120).all(|y| (120..160).all(|x| {
                let pixel = pixmap.pixel(x, y).expect("outside clip");
                pixel.red() > 240 && pixel.green() > 240 && pixel.blue() > 240
            })),
            "projected image must remain clipped to its overflow ancestor"
        );
    }

    #[test]
    fn translate_offscreen_box_is_not_painted() {
        // translate(-10000px,0) shoves the box far off the left edge (the old
        // hidden skip-link idiom); it must not paint anywhere on the canvas.
        let tree = parse_html(
            "<html><body>\
             <div style=\"position:absolute; top:0; left:0; width:50px; height:50px; \
                          background:#ff0000; transform:translate(-10000px,0)\"></div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut any_red = false;
        'scan: for y in 0..200 {
            for x in 0..200 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.red() > 200 && p.green() < 60 && p.blue() < 60 {
                    any_red = true;
                    break 'scan;
                }
            }
        }
        assert!(!any_red, "translate(-10000px,0) box should be off-screen and unpainted");
    }

    #[test]
    fn translate_percent_centers_absolute_box() {
        // The canonical centering idiom: an absolutely-positioned box at
        // top:50%/left:50% of its containing block pulled back by
        // translate(-50%,-50%) of its own size centers within it. In a 200x200
        // container a 40x40 box centers at (100,100), so its border box (with
        // top-left at 100,100 before the transform) becomes (80..120, 80..120).
        let tree = parse_html(
            "<html><body style=\"margin:0\">\
             <div style=\"position:relative; width:200px; height:200px\">\
               <div style=\"position:absolute; top:50%; left:50%; width:40px; height:40px; \
                            background:#ff0000; transform:translate(-50%,-50%)\"></div>\
             </div>\
             </body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let center = pixmap.pixel(100, 100).expect("pixel");
        assert!(center.red() > 200 && center.blue() < 60, "expected centered red box, got {:?}", center);
        // Just outside the centered box stays white.
        let outside = pixmap.pixel(70, 70).expect("pixel");
        assert_eq!((outside.red(), outside.green(), outside.blue()), (255, 255, 255));
    }

    #[test]
    fn paints_text_color() {
        let tree = parse_html(
            "<html><body><div style=\"color: #00ff00; width: 100px; height: 100px\">Hello</div></body></html>",
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_green = false;
        for y in 0..200 {
            for x in 0..200 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.green() > 200 && p.red() < 50 && p.blue() < 50 {
                    found_green = true;
                    break;
                }
            }
            if found_green { break; }
        }
        assert!(found_green, "expected green text to be painted");
    }

    #[test]
    fn word_measurement_honors_generic_font_family() {
        let sans = measure_text("iiiiiiii", 16.0, false, Some("sans-serif"));
        let mono = measure_text("iiiiiiii", 16.0, false, Some("monospace"));
        assert!(
            mono > sans * 1.5,
            "monospace advances must be used for code text: sans={sans}, mono={mono}"
        );

        let sample = "Build fast, responsive sites with Bootstrap";
        let system = measure_text(sample, 64.0, false, Some("system-ui, sans-serif"));
        let arial = measure_text(sample, 64.0, false, Some("Arial, sans-serif"));
        assert!(
            system > arial * 1.08,
            "Chromium's Linux system-ui face is DejaVu Sans, not the narrower \
             Liberation Sans used for Arial: system={system}, arial={arial}"
        );
    }

    #[test]
    fn paints_vendor_gradient_on_inline_text_span() {
        // Vue and many other framework sites put the gradient on an inline
        // accent span, not on the whole heading. The surrounding text must
        // keep its normal color while this span samples both gradient ends.
        let tree = parse_html(
            r#"<html><head><style>
               h1 { color:#17233c; font-size:50px; margin:0 }
               html:not(.dark) .accent[data-v-x] {
                 -webkit-text-fill-color:transparent;
                 background:-webkit-linear-gradient(315deg,#42d392 25%,#647eff);
                 -webkit-background-clip:text;
                 background-clip:text
               }
               </style></head><body style="margin:0">
               <h1>The <span class="accent" data-v-x>Progressive</span></h1>
               </body></html>"#,
        );
        let pixmap = paint_dom(&tree, (500.0, 100.0), None).expect("pixmap");
        let mut green = false;
        let mut blue = false;
        let mut normal = false;
        for pixel in pixmap.pixels() {
            let (r, g, b) = (pixel.red(), pixel.green(), pixel.blue());
            green |= g > r.saturating_add(20) && g > b.saturating_add(10);
            blue |= b > r.saturating_add(20) && b > g.saturating_add(5);
            normal |= b > g.saturating_add(10) && r < 80 && g < 100;
        }
        assert!(normal, "surrounding heading text should retain its normal color");
        assert!(green && blue, "inline accent should contain both gradient colors");
    }

    #[test]
    fn serializes_inline_svg_subtree() {
        // A sprite-style svg: a <use> that references a <symbol> in the same
        // document must survive serialization so resvg can resolve it.
        let tree = parse_html(
            r##"<html><body><svg viewBox="0 0 10 10"><use href="#a"/><symbol id="a"><path d="M0 0h10v10z"/></symbol></svg></body></html>"##,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let out = serialize_svg(&tree, svg);
        assert!(out.starts_with("<svg"), "root svg tag: {out}");
        assert!(out.contains(r#"viewBox="0 0 10 10""#), "viewBox preserved: {out}");
        assert!(out.contains(r#"xmlns="http://www.w3.org/2000/svg""#), "xmlns injected: {out}");
        assert!(out.contains("<use") && out.contains(r##"href="#a""##), "use + href: {out}");
        assert!(out.contains("<symbol") && out.contains(r#"id="a""#), "symbol id: {out}");
        assert!(out.contains("<path") && out.contains("</path>"), "path opened + closed: {out}");
        assert!(out.trim_end().ends_with("</svg>"), "root closed: {out}");
        // The serialized string parses as a standalone SVG document.
        let opts = usvg::Options::default();
        assert!(
            usvg::Tree::from_data(out.as_bytes(), &opts).is_ok(),
            "usvg should parse serialized svg: {out}",
        );
    }

    #[test]
    fn inline_svg_keeps_author_css_and_embedded_text_fonts() {
        // Inline SVG is parsed in the HTML document and styled by the page's
        // author sheet, then serialized into a standalone document for resvg.
        // The standalone boundary must not erase a CSS fill/font and inherit
        // the root's `fill:none`, which made text-only illustrations blank.
        let tree = parse_html(
            r##"<html><head><style>
                .art > text {
                    fill:#00cc55;
                    font-family:sans-serif;
                    font-size:24px;
                    font-weight:400
                }
                </style></head><body style="margin:0">
                <svg class="art" width="120" height="50"
                     viewBox="0 0 120 50" fill="none">
                    <text x="4" y="32">SVG text</text>
                </svg>
                </body></html>"##,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let layout = crate::dom::layout_dom(&tree, (160.0, 80.0));
        let markup = serialize_svg_styled(&tree, svg, &layout.styles, None);
        assert!(
            markup.contains("fill:#00cc55!important"),
            "computed author fill must cross the standalone boundary: {markup}"
        );
        assert!(
            markup.contains("font-size:24px!important"),
            "computed SVG font size must cross the standalone boundary: {markup}"
        );

        let pixmap = paint_dom(&tree, (160.0, 80.0), None).expect("pixmap");
        let painted_green = pixmap
            .pixels()
            .iter()
            .any(|pixel| pixel.green() > 150 && pixel.red() < 80 && pixel.blue() < 120);
        assert!(
            painted_green,
            "author-styled SVG text should rasterize with embedded fonts"
        );
    }

    #[test]
    fn injects_xmlns_only_when_absent() {
        let tree = parse_html(
            r#"<html><body><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="4" height="4"/></svg></body></html>"#,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let out = serialize_svg(&tree, svg);
        assert_eq!(out.matches("xmlns=").count(), 1, "no duplicate xmlns: {out}");
    }

    #[test]
    fn paints_inline_svg() {
        // The <rect> inside an inline svg must rasterize (it is not an <img>).
        let tree = parse_html(
            r##"<html><body><svg width="40" height="40" viewBox="0 0 40 40"><rect x="0" y="0" width="40" height="40" fill="#ff0000"/></svg></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_red = false;
        'outer: for y in 0..80 {
            for x in 0..80 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.red() > 200 && p.green() < 60 && p.blue() < 60 {
                    found_red = true;
                    break 'outer;
                }
            }
        }
        assert!(found_red, "expected inline svg <rect> to paint red");
    }

    #[test]
    fn svg_missing_root_height_uses_the_final_css_viewport() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"
            width="32" viewBox="0 0 223 236">
            <rect width="223" height="236" fill="#ed174c"/>
        </svg>"##;
        let pixmap = render_svg(svg, 32, 34).expect("svg raster");
        let mut min_y = 34u32;
        let mut max_y = 0u32;
        for y in 0..34 {
            for x in 0..32 {
                let pixel = pixmap.pixel(x, y).unwrap();
                if pixel.alpha() > 0 {
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        assert!(
            max_y.saturating_sub(min_y) >= 30,
            "viewBox artwork should fill the resolved 32x34 viewport, got rows {min_y}..{max_y}"
        );
    }

    #[test]
    fn paints_inline_svg_current_color_from_computed_style() {
        let tree = parse_html(
            r##"<html><body><svg style="color:#0784aa" width="40" height="40" viewBox="0 0 40 40"><circle cx="20" cy="20" r="18" fill="currentColor"/></svg></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (80.0, 80.0), None).expect("pixmap");
        let mut found = false;
        for pixel in pixmap.pixels() {
            found |= pixel.blue() > 120 && pixel.green() > 80 && pixel.red() < 40;
        }
        assert!(found, "computed color should resolve currentColor in inline svg");
    }

    #[test]
    fn paints_inline_svg_with_framework_colon_attribute() {
        let tree = obscura_dom::parse_html(
            r##"<html><body><svg q:id="f" width="40" height="40" viewBox="0 0 40 40"><rect width="40" height="40" fill="#18b6f6"/></svg></body></html>"##,
        );
        let output = paint_dom(&tree, (80.0, 80.0), None).expect("pixmap");
        let found_blue = (0..80).any(|y| {
            (0..80).any(|x| {
                let pixel = output.pixel(x, y).expect("pixel");
                pixel.blue() > 200 && pixel.green() > 120 && pixel.red() < 80
            })
        });
        assert!(
            found_blue,
            "framework hydration attributes must not invalidate inline SVG XML"
        );
    }

    #[test]
    fn paints_inline_svg_use_reference() {
        // The icon-sprite pattern: <use href="#id"> resolves against a <defs>
        // element in the same svg only because the whole subtree is serialized
        // and handed to resvg as one document.
        let tree = parse_html(
            r##"<html><body><svg width="40" height="40" viewBox="0 0 40 40"><defs><rect id="a" width="40" height="40" fill="#0000ff"/></defs><use href="#a"/></svg></body></html>"##,
        );
        let pixmap = paint_dom(&tree, (200.0, 200.0), None).expect("pixmap");
        let mut found_blue = false;
        'outer: for y in 0..80 {
            for x in 0..80 {
                let p = pixmap.pixel(x, y).expect("pixel");
                if p.blue() > 200 && p.red() < 60 && p.green() < 60 {
                    found_blue = true;
                    break 'outer;
                }
            }
        }
        assert!(found_blue, "expected <use> to instantiate the referenced <rect>");
    }

    #[test]
    fn extracts_symbol_by_id_from_sprite() {
        // The external-sprite core: given a fetched sprite, pull out just the
        // referenced <symbol> verbatim so it can be spliced into the local svg.
        let sprite = r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><symbol id="a" viewBox="0 0 10 10"><path d="M0 0h10v10z"/></symbol><symbol id="b"><rect width="4" height="4"/></symbol></defs></svg>"##;
        let out = extract_svg_element_by_id(sprite, "a").expect("symbol a found");
        assert!(out.starts_with("<symbol"), "starts at the symbol tag: {out}");
        assert!(out.contains(r#"id="a""#), "keeps the id: {out}");
        assert!(out.contains("<path") && out.contains("h10v10z"), "keeps children: {out}");
        assert!(out.trim_end().ends_with("</symbol>"), "closed at matching end: {out}");
        assert!(!out.contains(r#"id="b""#), "stops before the sibling symbol: {out}");
        assert!(!out.contains("<rect"), "no sibling content leaks in: {out}");
    }

    #[test]
    fn extract_handles_self_closing_nesting_and_absent() {
        // A self-closing element carrying the id returns just that tag.
        let s1 = r#"<svg><rect id="x" width="4" height="4"/></svg>"#;
        assert_eq!(
            extract_svg_element_by_id(s1, "x").as_deref(),
            Some(r#"<rect id="x" width="4" height="4"/>"#),
        );
        // Same-name nesting: the matching close is the outer one, not the inner.
        let s2 = r#"<svg><g id="grp"><g><path/></g></g></svg>"#;
        assert_eq!(
            extract_svg_element_by_id(s2, "grp").as_deref(),
            Some(r#"<g id="grp"><g><path/></g></g>"#),
        );
        // `data-id` / a missing id must not be mistaken for `id`.
        let s3 = r#"<svg><symbol data-id="a"><path/></symbol></svg>"#;
        assert!(extract_svg_element_by_id(s3, "a").is_none(), "data-id is not id");
        assert!(extract_svg_element_by_id(s2, "nope").is_none(), "absent id");
    }

    #[test]
    fn same_document_use_left_unchanged_by_inject() {
        // A same-document symbol already inside the target SVG needs no
        // injection and leaves the serialized markup byte-for-byte unchanged.
        let tree = parse_html(
            r##"<html><body><svg viewBox="0 0 10 10"><use href="#a"/><symbol id="a"><path d="M0 0h10v10z"/></symbol></svg></body></html>"##,
        );
        let svg = tree.query_selector("svg").unwrap().unwrap();
        let mut markup = serialize_svg(&tree, svg);
        let before = markup.clone();
        let mut cache = RenderResourceCache::default();
        let mut sprite_cache = std::collections::HashMap::new();
        inject_external_sprites(&tree, svg, None, &mut markup, &mut cache, &mut sprite_cache);
        assert_eq!(markup, before, "same-document use must be untouched");
    }

    #[test]
    fn injects_document_level_symbol_into_target_svg() {
        // Frameworks commonly keep one hidden sprite beside the application
        // root and reference it from otherwise independent inline SVGs.
        let tree = parse_html(
            r##"<html><body>
                <svg style="display:none"><symbol id="arrow" viewBox="0 0 10 10"><path d="M0 0h10v10z"/></symbol></svg>
                <svg id="icon" viewBox="0 0 10 10"><use href="#arrow"/></svg>
            </body></html>"##,
        );
        let svg = tree.query_selector("#icon").unwrap().unwrap();
        let mut markup = serialize_svg(&tree, svg);
        let mut cache = RenderResourceCache::default();
        let mut sprite_cache = std::collections::HashMap::new();
        inject_external_sprites(&tree, svg, None, &mut markup, &mut cache, &mut sprite_cache);
        assert!(
            markup.contains(r#"<defs><symbol id="arrow""#),
            "document-level symbol must be copied into target SVG: {markup}"
        );
        assert!(
            markup.contains(r##"<use href="#arrow""##),
            "local use reference must remain intact: {markup}"
        );
    }

    #[test]
    fn injected_document_symbol_inherits_target_current_color() {
        let tree = parse_html(
            r##"<html><body>
                <svg style="display:none"><symbol id="arrow" viewBox="0 0 10 10"><rect width="10" height="10" fill="currentColor"/></symbol></svg>
                <svg id="icon" viewBox="0 0 10 10"><use href="#arrow"/></svg>
            </body></html>"##,
        );
        let svg = tree.query_selector("#icon").unwrap().unwrap();
        let mut markup = serialize_svg(&tree, svg);
        let mut cache = RenderResourceCache::default();
        let mut sprite_cache = std::collections::HashMap::new();
        inject_external_sprites(&tree, svg, None, &mut markup, &mut cache, &mut sprite_cache);
        inject_svg_current_color(&mut markup, [220, 20, 60, 255]);
        let pixmap = render_svg(markup.as_bytes(), 20, 20).expect("injected svg renders");
        assert!(
            pixmap
                .pixels()
                .iter()
                .any(|pixel| pixel.red() > 180 && pixel.green() < 60 && pixel.blue() < 100),
            "injected currentColor symbol should inherit target SVG color: {markup}",
        );
    }

    #[test]
    fn svg_light_dark_presentation_is_resolved_before_usvg() {
        let tree = parse_html(
            r#"<style>
               #dark { color-scheme:dark }
               #dark rect {
                 fill:light-dark(#c3c7cb,#51565d);
                 stroke:light-dark(#ffffff,#000000);
               }
               </style>
               <div id="dark">
                 <svg id="icon" width="10" height="10" viewBox="0 0 10 10">
                   <rect width="10" height="10"/>
                 </svg>
               </div>"#,
        );
        let laid = crate::dom::layout_dom(&tree, (100.0, 100.0));
        let svg = tree.query_selector("#icon").unwrap().unwrap();
        let markup = serialize_svg_styled(&tree, svg, &laid.styles, None);
        assert!(
            !markup.to_ascii_lowercase().contains("light-dark("),
            "unsupported CSS Color 5 syntax must not reach usvg: {markup}"
        );
        assert!(
            markup.contains("fill:#51565dff!important")
                && markup.contains("stroke:#000000ff!important"),
            "serialized presentation colors must use the dark subtree scheme: {markup}"
        );
        let pixmap =
            render_svg(markup.as_bytes(), 10, 10).expect("resolved SVG renders");
        let center = pixmap.pixel(5, 5).expect("center pixel");
        assert!(
            center.red() > 60
                && center.red() < 110
                && center.green() > 60
                && center.green() < 120
                && center.blue() > 70
                && center.blue() < 130,
            "resolved dark fill must survive usvg rasterization: {center:?}"
        );
    }

    #[test]
    fn font_face_parser_selects_ascii_subset_and_preserves_functional_src() {
        let css = r#"
            @font-face {
                font-family: "Example";
                src: local("Example"), url("./example-cyrillic.woff2") format("woff2");
                unicode-range: U+0400-04FF;
            }
            @font-face {
                font-family: "Example";
                font-style: italic;
                font-weight: 350 650;
                src: url(data:font/woff2;base64,d09GMg==) format("woff2"),
                     url("./example-latin.woff") format("woff");
                unicode-range: U+??, U+2000-206F;
            }
        "#;
        let faces = font_face_blocks(css);
        assert_eq!(faces.len(), 2);
        assert!(!font_face_covers_ascii(faces[0]));
        assert!(font_face_covers_ascii(faces[1]));
        assert_eq!(font_face_family(faces[1]).as_deref(), Some("Example"));
        assert_eq!(font_face_weight(faces[1]), Some((350, 650)));
        assert_eq!(font_face_italic(faces[1]), Some(true));
        assert_eq!(
            font_face_urls(faces[1]),
            vec![
                "data:font/woff2;base64,d09GMg==".to_string(),
                "./example-latin.woff".to_string(),
            ]
        );
    }

    #[test]
    fn font_face_without_unicode_range_is_general_purpose() {
        let css = r#"@font-face{font-family:Example;src:url(example.otf)}"#;
        let face = font_face_blocks(css)[0];
        assert!(font_face_covers_ascii(face));
        assert_eq!(font_face_urls(face), vec!["example.otf"]);
    }

    #[test]
    fn dynamic_font_face_uses_shared_fetch_decode_and_layout_path() {
        let tree = parse_html(
            r#"<html><body><span id="sample" style="display:inline-block;width:max-content;
                font:40px DynamicFixture;white-space:nowrap">WWWWiiii</span></body></html>"#,
        );
        let sample = tree
            .query_selector("#sample")
            .expect("valid selector")
            .expect("sample");
        let fallback = prepare_dom(
            &tree,
            (400.0, 100.0),
            Some("https://example.test/page/index.html"),
            &mut RenderResourceCache::default(),
        )
        .expect("fallback render")
        .document_rect(sample)
        .expect("fallback geometry")
        .width;
        let loads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let loader_loads = Arc::clone(&loads);
        let mut resources = RenderResourceCache::with_loader(move |url: &str| {
            loader_loads.lock().expect("font loads").push(url.to_string());
            (url == "https://example.test/fonts/fixture.ttf")
                .then(|| SERIF_FONT_BYTES.to_vec())
        });
        let prepared = prepare_dom_with_dynamic_fonts(
            &tree,
            (400.0, 100.0),
            Some("https://example.test/page/index.html"),
            &mut resources,
            &[DynamicFontFace {
                family: "DynamicFixture".to_string(),
                source: "url('../../fonts/fixture.ttf') format('truetype')".to_string(),
                style: "normal".to_string(),
                weight: "400".to_string(),
                unicode_range: "U+20-7E".to_string(),
            }],
        )
        .expect("dynamic font render");
        let dynamic = prepared
            .document_rect(sample)
            .expect("dynamic geometry")
            .width;
        assert_ne!(fallback, dynamic, "dynamic face must participate in shaping");
        assert_eq!(
            *loads.lock().expect("dynamic font loads"),
            vec!["https://example.test/fonts/fixture.ttf".to_string()]
        );
    }
}
