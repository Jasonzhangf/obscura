//! Bounded raster-backed PDF export over the retained document-space painter.
//!
//! This deliberately does not claim CSS paged-media support. It preserves the
//! screen layout, fits the full document width into the printable area, and
//! slices that immutable layout vertically across PDF pages.

use std::io;

use image::ImageEncoder as _;
use obscura_js::CaptureRegion;

use crate::Page;

const POINTS_PER_INCH: f32 = 72.0;
const MAX_PAPER_INCHES: f32 = 200.0;
const MAX_PDF_PAGES: usize = 250;
// Page ranges may select a small bounded subset from a much longer document.
// Keep the arithmetic/index space finite without charging unselected pages
// against the output-page limit.
const MAX_PDF_DOCUMENT_PAGES: usize = 1_000_000;
const MAX_PDF_PAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_PDF_TOTAL_RASTER_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_PDF_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterPdfPageRange {
    /// One-based inclusive first page. `None` means the first page.
    pub start: Option<usize>,
    /// One-based inclusive last page. `None` means the final page.
    pub end: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RasterPdfOptions {
    pub landscape: bool,
    pub print_background: bool,
    pub scale: f32,
    pub page_ranges: Vec<RasterPdfPageRange>,
    pub paper_width_in: f32,
    pub paper_height_in: f32,
    pub margin_top_in: f32,
    pub margin_bottom_in: f32,
    pub margin_left_in: f32,
    pub margin_right_in: f32,
}

impl Default for RasterPdfOptions {
    fn default() -> Self {
        Self {
            landscape: false,
            print_background: false,
            scale: 1.0,
            page_ranges: Vec::new(),
            paper_width_in: 8.5,
            paper_height_in: 11.0,
            // CDP's defaults are one centimetre.
            margin_top_in: 0.3937,
            margin_bottom_in: 0.3937,
            margin_left_in: 0.3937,
            margin_right_in: 0.3937,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RasterPdfError {
    #[error("PDF paper dimensions must be finite and between 0 and 200 inches")]
    InvalidPaperSize,
    #[error("PDF margins must be finite, non-negative, and leave a printable area")]
    InvalidMargins,
    #[error("PDF scale must be finite and between 0.1 and 2")]
    InvalidScale,
    #[error("PDF page ranges select no pages from this document")]
    EmptyPageRange,
    #[error("the page has no retained renderable document")]
    NoRenderableDocument,
    #[error("PDF pagination would exceed the {0}-page safety limit")]
    TooManyPages(usize),
    #[error("PDF raster work would exceed the bounded page or document pixel budget")]
    RasterWorkLimitExceeded,
    #[error("document-space PDF capture failed: {0}")]
    CaptureFailed(String),
    #[error("PDF raster image decoding failed: {0}")]
    ImageDecode(String),
    #[error("PDF JPEG encoding failed: {0}")]
    ImageEncode(String),
    #[error("encoded PDF would exceed the 64 MiB safety limit")]
    OutputLimitExceeded,
}

#[derive(Debug)]
struct RasterPage {
    rgb: image::RgbImage,
    draw_width_pt: f32,
    draw_height_pt: f32,
    #[cfg(test)]
    _lifetime_probe: Option<std::rc::Rc<()>>,
}

#[derive(Clone, Copy, Debug)]
struct PaginationPlan {
    points_per_css_pixel: f32,
    css_page_height: f32,
    page_count: usize,
}

impl RasterPdfOptions {
    fn page_geometry(&self) -> Result<(f32, f32, f32, f32, f32, f32), RasterPdfError> {
        let values = [self.paper_width_in, self.paper_height_in];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0 || *value > MAX_PAPER_INCHES)
        {
            return Err(RasterPdfError::InvalidPaperSize);
        }
        let margins = [
            self.margin_top_in,
            self.margin_bottom_in,
            self.margin_left_in,
            self.margin_right_in,
        ];
        if margins
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(RasterPdfError::InvalidMargins);
        }
        let (paper_width_in, paper_height_in) = if self.landscape {
            (self.paper_height_in, self.paper_width_in)
        } else {
            (self.paper_width_in, self.paper_height_in)
        };
        let page_width = paper_width_in * POINTS_PER_INCH;
        let page_height = paper_height_in * POINTS_PER_INCH;
        let left = self.margin_left_in * POINTS_PER_INCH;
        let bottom = self.margin_bottom_in * POINTS_PER_INCH;
        let printable_width =
            page_width - (self.margin_left_in + self.margin_right_in) * POINTS_PER_INCH;
        let printable_height =
            page_height - (self.margin_top_in + self.margin_bottom_in) * POINTS_PER_INCH;
        if printable_width <= 0.0 || printable_height <= 0.0 {
            return Err(RasterPdfError::InvalidMargins);
        }
        Ok((
            page_width,
            page_height,
            printable_width,
            printable_height,
            left,
            bottom,
        ))
    }
}

fn pagination_plan(
    content_width: f32,
    content_height: f32,
    printable_width: f32,
    printable_height: f32,
    scale: f32,
) -> Result<PaginationPlan, RasterPdfError> {
    if !scale.is_finite() || !(0.1..=2.0).contains(&scale) {
        return Err(RasterPdfError::InvalidScale);
    }
    let points_per_css_pixel = printable_width / content_width * scale;
    let css_page_height = printable_height / points_per_css_pixel;
    if !points_per_css_pixel.is_finite()
        || points_per_css_pixel <= 0.0
        || !css_page_height.is_finite()
        || css_page_height <= 0.0
    {
        return Err(RasterPdfError::RasterWorkLimitExceeded);
    }

    let page_count_value = (content_height / css_page_height).ceil().max(1.0);
    if !page_count_value.is_finite() || page_count_value > MAX_PDF_DOCUMENT_PAGES as f32 {
        return Err(RasterPdfError::TooManyPages(MAX_PDF_DOCUMENT_PAGES));
    }
    let page_count = page_count_value as usize;

    Ok(PaginationPlan {
        points_per_css_pixel,
        css_page_height,
        page_count,
    })
}

fn validate_selected_raster_work(
    content_width: f32,
    content_height: f32,
    plan: PaginationPlan,
    selected_pages: &[usize],
) -> Result<(), RasterPdfError> {
    let pixel_width = content_width.ceil();
    if !pixel_width.is_finite()
        || pixel_width <= 0.0
        || pixel_width > obscura_js::MAX_CAPTURE_DIMENSION as f32
    {
        return Err(RasterPdfError::RasterWorkLimitExceeded);
    }
    let pixel_width = pixel_width as u64;
    let mut total_pixels = 0u64;
    for &page_index in selected_pages {
        if page_index >= plan.page_count {
            return Err(RasterPdfError::EmptyPageRange);
        }
        let y = page_index as f32 * plan.css_page_height;
        let slice_height = (content_height - y).min(plan.css_page_height).ceil();
        if !slice_height.is_finite()
            || slice_height <= 0.0
            || slice_height > obscura_js::MAX_CAPTURE_DIMENSION as f32
        {
            return Err(RasterPdfError::RasterWorkLimitExceeded);
        }
        let page_pixels = pixel_width
            .checked_mul(slice_height as u64)
            .ok_or(RasterPdfError::RasterWorkLimitExceeded)?;
        if page_pixels > MAX_PDF_PAGE_PIXELS {
            return Err(RasterPdfError::RasterWorkLimitExceeded);
        }
        total_pixels = total_pixels
            .checked_add(page_pixels)
            .ok_or(RasterPdfError::RasterWorkLimitExceeded)?;
        if total_pixels > MAX_PDF_TOTAL_RASTER_PIXELS {
            return Err(RasterPdfError::RasterWorkLimitExceeded);
        }
    }

    Ok(())
}

fn selected_page_indices(
    page_count: usize,
    ranges: &[RasterPdfPageRange],
) -> Result<Vec<usize>, RasterPdfError> {
    if page_count == 0 {
        return Err(RasterPdfError::EmptyPageRange);
    }
    if ranges.is_empty() {
        if page_count > MAX_PDF_PAGES {
            return Err(RasterPdfError::TooManyPages(MAX_PDF_PAGES));
        }
        return Ok((0..page_count).collect());
    }
    let mut selected = std::collections::BTreeSet::new();
    for range in ranges {
        let start = range.start.unwrap_or(1);
        let end = range.end.unwrap_or(page_count);
        if start == 0 || end == 0 || start > end {
            return Err(RasterPdfError::EmptyPageRange);
        }
        if start > page_count {
            continue;
        }
        let end = end.min(page_count);
        let span = end - start + 1;
        if span > MAX_PDF_PAGES {
            return Err(RasterPdfError::TooManyPages(MAX_PDF_PAGES));
        }
        for page in start..=end {
            selected.insert(page - 1);
            if selected.len() > MAX_PDF_PAGES {
                return Err(RasterPdfError::TooManyPages(MAX_PDF_PAGES));
            }
        }
    }
    let selected = selected.into_iter().collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(RasterPdfError::EmptyPageRange);
    }
    Ok(selected)
}

impl Page {
    /// Export the current screen layout as a paginated raster PDF.
    ///
    /// The full document width is scaled uniformly into the printable width;
    /// vertical slices become pages. This does not reflow into `@media print`
    /// or implement CSS paged media, headers, or footers.
    pub fn raster_pdf(&self, options: RasterPdfOptions) -> Result<Vec<u8>, RasterPdfError> {
        self.raster_pdf_with_animation_sample(options, self.live_animation_sample())
    }

    pub fn raster_pdf_at_animation_time(
        &self,
        options: RasterPdfOptions,
        animation_sample_time: obscura_js::AnimationSampleTime,
    ) -> Result<Vec<u8>, RasterPdfError> {
        self.raster_pdf_with_animation_sample(
            options,
            obscura_js::AnimationSample {
                time: animation_sample_time,
                mode: obscura_js::AnimationSampleMode::LocalOverride,
            },
        )
    }

    pub fn raster_pdf_with_animation_sample(
        &self,
        options: RasterPdfOptions,
        animation_sample: obscura_js::AnimationSample,
    ) -> Result<Vec<u8>, RasterPdfError> {
        let (page_width, page_height, printable_width, printable_height, left, bottom) =
            options.page_geometry()?;
        let js = self
            .js
            .as_ref()
            .ok_or(RasterPdfError::NoRenderableDocument)?;
        if !js.set_animation_sample(animation_sample) {
            return Err(RasterPdfError::NoRenderableDocument);
        }
        let (content_width, content_height) = js
            .prepared_content_size()
            .ok_or(RasterPdfError::NoRenderableDocument)?;
        if !content_width.is_finite()
            || !content_height.is_finite()
            || content_width <= 0.0
            || content_height <= 0.0
        {
            return Err(RasterPdfError::NoRenderableDocument);
        }

        let plan = pagination_plan(
            content_width,
            content_height,
            printable_width,
            printable_height,
            options.scale,
        )?;
        let selected_pages = selected_page_indices(plan.page_count, &options.page_ranges)?;
        validate_selected_raster_work(content_width, content_height, plan, &selected_pages)?;

        encode_pdf_pages(
            selected_pages.len(),
            page_width,
            page_height,
            left,
            bottom,
            printable_height,
            |output_page_index| {
                let page_index = selected_pages[output_page_index];
                let y = page_index as f32 * plan.css_page_height;
                let slice_height = (content_height - y).min(plan.css_page_height);
                let png = js
                    .screenshot_prepared_region_with_backgrounds(
                        CaptureRegion::new(0.0, y, content_width, slice_height, 1.0),
                        options.print_background,
                    )
                    .map_err(|error| RasterPdfError::CaptureFailed(format!("{error:?}")))?;
                let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                    .map_err(|error| RasterPdfError::ImageDecode(error.to_string()))?;
                // The document capture is already a complete PNG allocation. Drop
                // it before converting the decoded pixels and, below, encoding the
                // JPEG directly into the final PDF buffer. At no point do we retain
                // PNGs or JPEGs for earlier pages.
                drop(png);
                let rgb = decoded.into_rgb8();
                Ok(RasterPage {
                    rgb,
                    draw_width_pt: content_width * plan.points_per_css_pixel,
                    draw_height_pt: slice_height * plan.points_per_css_pixel,
                    #[cfg(test)]
                    _lifetime_probe: None,
                })
            },
        )
    }
}

fn encode_pdf_pages(
    page_count: usize,
    page_width: f32,
    page_height: f32,
    left: f32,
    bottom: f32,
    printable_height: f32,
    mut page_source: impl FnMut(usize) -> Result<RasterPage, RasterPdfError>,
) -> Result<Vec<u8>, RasterPdfError> {
    let object_count = 2usize
        .checked_add(
            page_count
                .checked_mul(3)
                .ok_or(RasterPdfError::OutputLimitExceeded)?,
        )
        .ok_or(RasterPdfError::OutputLimitExceeded)?;
    let mut writer = PdfWriter::new(object_count, MAX_PDF_OUTPUT_BYTES)?;
    writer.write_object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;

    let kids = (0..page_count)
        .map(|index| format!("{} 0 R", 3 + index * 3))
        .collect::<Vec<_>>()
        .join(" ");
    let pages_dictionary = format!("<< /Type /Pages /Count {page_count} /Kids [{kids}] >>");
    writer.write_object(2, pages_dictionary.as_bytes())?;

    for index in 0..page_count {
        let page = page_source(index)?;
        let page_id = 3 + index * 3;
        let content_id = page_id + 1;
        let image_id = page_id + 2;
        let page_dictionary = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_width:.3} {page_height:.3}] /Resources << /XObject << /Im0 {image_id} 0 R >> >> /Contents {content_id} 0 R >>"
        );
        writer.write_object(page_id, page_dictionary.as_bytes())?;

        let draw_y = bottom + printable_height - page.draw_height_pt;
        let commands = format!(
            "q\n{:.3} 0 0 {:.3} {:.3} {:.3} cm\n/Im0 Do\nQ\n",
            page.draw_width_pt, page.draw_height_pt, left, draw_y,
        );
        let content = format!(
            "<< /Length {} >>\nstream\n{}endstream",
            commands.len(),
            commands
        );
        writer.write_object(content_id, content.as_bytes())?;
        writer.write_rgb_image(image_id, &page.rgb)?;
        // `page`, including its decoded RGB raster, is dropped here before
        // the next page is captured. Only the bounded final PDF survives.
    }

    writer.finish()
}

struct PdfWriter {
    output: Vec<u8>,
    offsets: Vec<usize>,
    limit: usize,
    limit_exceeded: bool,
}

impl PdfWriter {
    fn new(object_count: usize, limit: usize) -> Result<Self, RasterPdfError> {
        let mut writer = Self {
            output: Vec::new(),
            offsets: vec![0usize; object_count + 1],
            limit,
            limit_exceeded: false,
        };
        writer.append(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")?;
        Ok(writer)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), RasterPdfError> {
        let new_len = self
            .output
            .len()
            .checked_add(bytes.len())
            .ok_or(RasterPdfError::OutputLimitExceeded)?;
        if new_len > self.limit {
            self.limit_exceeded = true;
            return Err(RasterPdfError::OutputLimitExceeded);
        }
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn write_object(&mut self, id: usize, body: &[u8]) -> Result<(), RasterPdfError> {
        self.offsets[id] = self.output.len();
        self.append(format!("{id} 0 obj\n").as_bytes())?;
        self.append(body)?;
        self.append(b"\nendobj\n")
    }

    fn write_rgb_image(&mut self, id: usize, rgb: &image::RgbImage) -> Result<(), RasterPdfError> {
        self.offsets[id] = self.output.len();
        self.append(format!(
            "{id} 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length ",
            rgb.width(), rgb.height(),
        ).as_bytes())?;
        // Encode into the final PDF rather than building a second JPEG Vec.
        // A fixed-width decimal token lets us patch /Length after encoding.
        const LENGTH_DIGITS: usize = 20;
        let length_offset = self.output.len();
        self.append(b"00000000000000000000 >>\nstream\n")?;
        let stream_offset = self.output.len();
        let encode_result = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut *self, 90)
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            );
        if let Err(error) = encode_result {
            return if self.limit_exceeded {
                Err(RasterPdfError::OutputLimitExceeded)
            } else {
                Err(RasterPdfError::ImageEncode(error.to_string()))
            };
        }
        let stream_len = self.output.len() - stream_offset;
        let length = format!("{stream_len:0LENGTH_DIGITS$}");
        if length.len() != LENGTH_DIGITS {
            return Err(RasterPdfError::OutputLimitExceeded);
        }
        self.output[length_offset..length_offset + LENGTH_DIGITS]
            .copy_from_slice(length.as_bytes());
        self.append(b"\nendstream\nendobj\n")
    }

    fn finish(mut self) -> Result<Vec<u8>, RasterPdfError> {
        let object_count = self.offsets.len() - 1;
        let xref_offset = self.output.len();
        self.append(format!("xref\n0 {}\n0000000000 65535 f \n", object_count + 1).as_bytes())?;
        for index in 1..self.offsets.len() {
            let offset = self.offsets[index];
            self.append(format!("{offset:010} 00000 n \n").as_bytes())?;
        }
        self.append(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                object_count + 1,
            )
            .as_bytes(),
        )?;
        Ok(self.output)
    }
}

impl io::Write for PdfWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)
            .map(|()| bytes.len())
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_impossible_media_boxes() {
        let mut options = RasterPdfOptions::default();
        options.paper_width_in = 0.0;
        assert_eq!(
            options.page_geometry(),
            Err(RasterPdfError::InvalidPaperSize)
        );
        let mut options = RasterPdfOptions::default();
        options.margin_left_in = 5.0;
        options.margin_right_in = 5.0;
        assert_eq!(options.page_geometry(), Err(RasterPdfError::InvalidMargins));
    }

    #[test]
    fn pagination_preflight_bounds_pages_and_raster_work() {
        let (_, _, printable_width, printable_height, _, _) =
            RasterPdfOptions::default().page_geometry().unwrap();
        let ordinary = pagination_plan(1280.0, 10_000.0, printable_width, printable_height, 1.0)
            .expect("an ordinary multi-page document stays inside the budget");
        assert!(ordinary.page_count > 1);
        let ordinary_pages = selected_page_indices(ordinary.page_count, &[]).unwrap();
        validate_selected_raster_work(1280.0, 10_000.0, ordinary, &ordinary_pages).unwrap();

        let oversized_page =
            pagination_plan(5_000.0, 5_000.0, printable_width, printable_height, 1.0).unwrap();
        let oversized_page_selection =
            selected_page_indices(oversized_page.page_count, &[]).unwrap();
        assert_eq!(
            validate_selected_raster_work(
                5_000.0,
                5_000.0,
                oversized_page,
                &oversized_page_selection,
            )
            .unwrap_err(),
            RasterPdfError::RasterWorkLimitExceeded,
            "one excessively large raster page must fail before capture"
        );

        let too_much_total =
            pagination_plan(1_000.0, 70_000.0, printable_width, printable_height, 1.0).unwrap();
        let too_much_total_selection =
            selected_page_indices(too_much_total.page_count, &[]).unwrap();
        assert_eq!(
            validate_selected_raster_work(
                1_000.0,
                70_000.0,
                too_much_total,
                &too_much_total_selection,
            )
            .unwrap_err(),
            RasterPdfError::RasterWorkLimitExceeded,
            "many individually valid pages must still respect a total work budget"
        );

        let too_many =
            pagination_plan(1_000.0, 400_000.0, printable_width, printable_height, 1.0).unwrap();
        assert_eq!(
            selected_page_indices(too_many.page_count, &[]).unwrap_err(),
            RasterPdfError::TooManyPages(MAX_PDF_PAGES),
        );
    }

    #[test]
    fn selected_ranges_alone_determine_output_and_raster_budgets() {
        let (_, _, printable_width, printable_height, _, _) =
            RasterPdfOptions::default().page_geometry().unwrap();
        let long =
            pagination_plan(1_000.0, 400_000.0, printable_width, printable_height, 1.0).unwrap();
        assert!(long.page_count > MAX_PDF_PAGES);
        let selected = selected_page_indices(
            long.page_count,
            &[RasterPdfPageRange {
                start: Some(1),
                end: Some(1),
            }],
        )
        .unwrap();
        assert_eq!(selected, vec![0]);
        validate_selected_raster_work(1_000.0, 400_000.0, long, &selected).unwrap();

        assert_eq!(
            selected_page_indices(
                long.page_count,
                &[RasterPdfPageRange {
                    start: Some(1),
                    end: Some(MAX_PDF_PAGES + 1),
                }],
            ),
            Err(RasterPdfError::TooManyPages(MAX_PDF_PAGES))
        );

        let base = pagination_plan(800.0, 2_000.0, printable_width, printable_height, 1.0)
            .expect("base geometry");
        let impossible_height =
            base.css_page_height * (MAX_PDF_DOCUMENT_PAGES as f32 + 16.0);
        assert_eq!(
            pagination_plan(
                800.0,
                impossible_height,
                printable_width,
                printable_height,
                1.0,
            )
            .unwrap_err(),
            RasterPdfError::TooManyPages(MAX_PDF_DOCUMENT_PAGES)
        );
    }

    #[test]
    fn scale_changes_css_page_span_and_rejects_invalid_values() {
        let (_, _, printable_width, printable_height, _, _) =
            RasterPdfOptions::default().page_geometry().unwrap();
        let normal =
            pagination_plan(800.0, 2_000.0, printable_width, printable_height, 1.0).unwrap();
        let enlarged =
            pagination_plan(800.0, 2_000.0, printable_width, printable_height, 2.0).unwrap();
        assert_eq!(
            enlarged.points_per_css_pixel,
            normal.points_per_css_pixel * 2.0
        );
        assert_eq!(enlarged.css_page_height, normal.css_page_height / 2.0);
        assert!(enlarged.page_count >= normal.page_count);
        assert_eq!(
            pagination_plan(800.0, 2_000.0, printable_width, printable_height, 0.09,)
                .unwrap_err(),
            RasterPdfError::InvalidScale
        );
    }

    #[test]
    fn page_ranges_clip_deduplicate_and_preserve_document_order() {
        assert_eq!(selected_page_indices(4, &[]).unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(
            selected_page_indices(
                6,
                &[
                    RasterPdfPageRange {
                        start: Some(3),
                        end: Some(5),
                    },
                    RasterPdfPageRange {
                        start: Some(1),
                        end: Some(3),
                    },
                    RasterPdfPageRange {
                        start: Some(5),
                        end: None,
                    },
                ],
            )
            .unwrap(),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            selected_page_indices(
                6,
                &[RasterPdfPageRange {
                    start: None,
                    end: Some(2),
                }],
            )
            .unwrap(),
            vec![0, 1]
        );
        assert_eq!(
            selected_page_indices(
                3,
                &[RasterPdfPageRange {
                    start: Some(9),
                    end: Some(12),
                }],
            ),
            Err(RasterPdfError::EmptyPageRange)
        );
    }

    #[test]
    fn writer_emits_xref_and_one_image_per_page() {
        let pdf = encode_pdf_pages(1, 612.0, 792.0, 36.0, 36.0, 720.0, |_| {
            Ok(RasterPage {
                rgb: image::RgbImage::from_pixel(2, 3, image::Rgb([10, 20, 30])),
                draw_width_pt: 100.0,
                draw_height_pt: 150.0,
                _lifetime_probe: None,
            })
        })
        .unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Count 1"));
        assert!(text.contains("/Subtype /Image"));
        assert!(text.contains("xref\n0 6"));
        let startxref = text
            .rsplit_once("startxref\n")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(pdf[startxref..].starts_with(b"xref\n"));
        let object_one_offset = text
            .split("xref\n0 6\n")
            .nth(1)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()[..10]
            .parse::<usize>()
            .unwrap();
        assert!(pdf[object_one_offset..].starts_with(b"1 0 obj\n"));
    }

    #[test]
    fn page_rasters_are_released_before_capturing_the_next_page() {
        let previous = std::cell::RefCell::new(None::<std::rc::Weak<()>>);
        let pdf = encode_pdf_pages(4, 612.0, 792.0, 36.0, 36.0, 720.0, |index| {
            if let Some(previous) = previous.borrow().as_ref() {
                assert!(
                    previous.upgrade().is_none(),
                    "page {index} was requested while the prior raster was still retained"
                );
            }
            let probe = std::rc::Rc::new(());
            *previous.borrow_mut() = Some(std::rc::Rc::downgrade(&probe));
            Ok(RasterPage {
                rgb: image::RgbImage::from_pixel(8, 8, image::Rgb([index as u8, 0, 0])),
                draw_width_pt: 100.0,
                draw_height_pt: 100.0,
                _lifetime_probe: Some(probe),
            })
        })
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&pdf)
                .matches("/Subtype /Image")
                .count(),
            4
        );
    }

    #[test]
    fn writer_enforces_the_output_limit_while_encoding_the_image_stream() {
        let mut writer = PdfWriter::new(5, 600).unwrap();
        writer
            .write_object(1, b"<< /Type /Catalog /Pages 2 0 R >>")
            .unwrap();
        writer
            .write_object(2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>")
            .unwrap();
        let noisy = image::RgbImage::from_fn(128, 128, |x, y| {
            image::Rgb([
                x.wrapping_mul(37) as u8,
                y.wrapping_mul(53) as u8,
                x.wrapping_add(y).wrapping_mul(71) as u8,
            ])
        });
        assert_eq!(
            writer.write_rgb_image(5, &noisy),
            Err(RasterPdfError::OutputLimitExceeded)
        );
        assert!(writer.output.len() <= 600);
    }
}
