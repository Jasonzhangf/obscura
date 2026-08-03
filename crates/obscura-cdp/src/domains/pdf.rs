#[cfg(feature = "render")]
use serde_json::json;
use serde_json::Value;

use crate::dispatch::CdpContext;

#[cfg(feature = "render")]
const MAX_BASE64_PDF_BYTES: usize = 64 * 1024 * 1024;

#[cfg(feature = "render")]
fn base64_encoded_len(raw_len: usize) -> Option<usize> {
    raw_len.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

#[cfg(feature = "render")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PdfTransferMode {
    ReturnAsBase64,
    ReturnAsStream,
}

#[cfg(feature = "render")]
#[derive(Clone, Copy, Debug)]
struct ParsedPdfOptions {
    raster: obscura_browser::RasterPdfOptions,
    transfer_mode: PdfTransferMode,
    requested_print_background: bool,
    requested_tagged_pdf: bool,
}

#[cfg(feature = "render")]
fn number(params: &Value, name: &str, default: f32) -> Result<f32, String> {
    match params.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| value as f32)
            .ok_or_else(|| format!("Invalid parameters: {name} must be a finite number")),
    }
}

#[cfg(feature = "render")]
fn boolean(params: &Value, name: &str, default: bool) -> Result<bool, String> {
    match params.get(name) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("Invalid parameters: {name} must be a boolean")),
    }
}

#[cfg(feature = "render")]
fn parse_options(params: &Value) -> Result<ParsedPdfOptions, String> {
    if !params.is_object() {
        return Err("Invalid parameters: expected an object".to_string());
    }
    let unsupported_true = [
        ("displayHeaderFooter", "headers and footers"),
        ("preferCSSPageSize", "CSS @page sizing"),
        ("generateDocumentOutline", "document outlines"),
    ];
    for (name, capability) in unsupported_true {
        if boolean(params, name, false)? {
            return Err(format!(
                "Page.printToPDF does not yet support {capability} ({name}=true)"
            ));
        }
    }
    // Current Puppeteer and Playwright always send their false default. The
    // raster paginator cannot suppress backgrounds without a second paint
    // mode, so accept the protocol shape and disclose the limitation in the
    // extension capability fields returned with the result.
    let requested_print_background = boolean(params, "printBackground", false)?;
    // Puppeteer currently sends true by default. Raster PDFs have no semantic
    // structure to tag, but rejecting the request makes the standard client
    // unusable. Accept and truthfully report `taggedPdf:false` in the response.
    let requested_tagged_pdf = boolean(params, "generateTaggedPDF", false)?;
    let scale = number(params, "scale", 1.0)?;
    if (scale - 1.0).abs() > f32::EPSILON {
        return Err(
            "Page.printToPDF scale is not supported by the raster paginator; only scale=1 is accepted"
                .to_string(),
        );
    }
    if let Some(value) = params.get("pageRanges") {
        let ranges = value
            .as_str()
            .ok_or("Invalid parameters: pageRanges must be a string")?;
        if !ranges.trim().is_empty() {
            return Err("Page.printToPDF pageRanges are not yet supported".to_string());
        }
    }
    for name in ["headerTemplate", "footerTemplate"] {
        if let Some(value) = params.get(name) {
            let template = value
                .as_str()
                .ok_or_else(|| format!("Invalid parameters: {name} must be a string"))?;
            if !template.is_empty() {
                return Err(format!("Page.printToPDF {name} is not yet supported"));
            }
        }
    }
    let transfer_mode = match params.get("transferMode") {
        None => PdfTransferMode::ReturnAsBase64,
        Some(Value::String(value)) => match value.as_str() {
            "ReturnAsBase64" => PdfTransferMode::ReturnAsBase64,
            "ReturnAsStream" => PdfTransferMode::ReturnAsStream,
            _ => return Err("Invalid parameters: unknown transferMode".into()),
        },
        Some(_) => return Err("Invalid parameters: transferMode must be a string".into()),
    };

    let defaults = obscura_browser::RasterPdfOptions::default();
    Ok(ParsedPdfOptions {
        raster: obscura_browser::RasterPdfOptions {
            landscape: boolean(params, "landscape", defaults.landscape)?,
            paper_width_in: number(params, "paperWidth", defaults.paper_width_in)?,
            paper_height_in: number(params, "paperHeight", defaults.paper_height_in)?,
            margin_top_in: number(params, "marginTop", defaults.margin_top_in)?,
            margin_bottom_in: number(params, "marginBottom", defaults.margin_bottom_in)?,
            margin_left_in: number(params, "marginLeft", defaults.margin_left_in)?,
            margin_right_in: number(params, "marginRight", defaults.margin_right_in)?,
        },
        transfer_mode,
        requested_print_background,
        requested_tagged_pdf,
    })
}

pub async fn print_to_pdf(
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    #[cfg(feature = "render")]
    {
        let options = parse_options(params)?;
        let page = ctx
            .get_session_page_mut(session_id)
            .ok_or("No page for session")?;
        crate::domains::page::prepare_capture_resources_if_requested(page).await;
        let pdf = page
            .raster_pdf(options.raster)
            .map_err(|error| error.to_string())?;
        let mut response = json!({
            "obscuraPrintMode": "screen-raster",
            "obscuraPrintBackground": true,
            "obscuraRequestedPrintBackground": options.requested_print_background,
            "obscuraTaggedPDF": false,
            "obscuraRequestedTaggedPDF": options.requested_tagged_pdf,
            "obscuraCapabilities": {
                "cssPagedMedia": false,
                "honorsPrintBackground": false,
                "taggedPdf": false,
            },
        });
        let mut ignored_options = Vec::new();
        if !options.requested_print_background {
            ignored_options.push("printBackground");
        }
        if options.requested_tagged_pdf {
            ignored_options.push("generateTaggedPDF");
        }
        response["obscuraIgnoredOptions"] = json!(ignored_options);
        match options.transfer_mode {
            PdfTransferMode::ReturnAsBase64 => {
                use base64::Engine as _;
                let encoded_len = base64_encoded_len(pdf.len())
                    .ok_or("Page.printToPDF base64 response size overflow")?;
                if encoded_len > MAX_BASE64_PDF_BYTES {
                    return Err(format!(
                        "Page.printToPDF base64 response would be {encoded_len} bytes, exceeding the {MAX_BASE64_PDF_BYTES}-byte response limit; use transferMode=ReturnAsStream"
                    ));
                }
                response["data"] =
                    Value::String(base64::engine::general_purpose::STANDARD.encode(pdf));
            }
            PdfTransferMode::ReturnAsStream => {
                let handle = ctx
                    .io_streams
                    .insert(pdf)
                    .map_err(|error| format!("Page.printToPDF could not open stream: {error}"))?;
                response["data"] = Value::String(String::new());
                response["stream"] = Value::String(handle);
            }
        }
        Ok(response)
    }
    #[cfg(not(feature = "render"))]
    {
        let _ = (params, ctx, session_id);
        Err("Page.printToPDF requires a build with the render feature".to_string())
    }
}

#[cfg(all(test, feature = "render"))]
mod tests {
    use super::*;
    use crate::domains::page;

    #[test]
    fn parser_accepts_standard_client_defaults_and_rejects_unrepresented_features() {
        let standard = parse_options(&json!({
            "transferMode": "ReturnAsStream",
            "displayHeaderFooter": false,
            "headerTemplate": "",
            "footerTemplate": "",
            "printBackground": false,
            "scale": 1,
            "pageRanges": "",
            "preferCSSPageSize": false,
            "generateTaggedPDF": true,
            "generateDocumentOutline": false,
        }))
        .expect("current Puppeteer defaults must be accepted");
        assert_eq!(standard.transfer_mode, PdfTransferMode::ReturnAsStream);
        assert!(!standard.requested_print_background);
        assert!(standard.requested_tagged_pdf);

        for params in [
            json!({"displayHeaderFooter": true}),
            json!({"preferCSSPageSize": true}),
            json!({"scale": 0.5}),
            json!({"pageRanges": "2-3"}),
            json!({"headerTemplate": "<span>title</span>"}),
            json!({"generateDocumentOutline": true}),
        ] {
            assert!(parse_options(&params).is_err(), "must reject {params}");
        }
    }

    #[test]
    fn base64_size_preflight_is_exact_and_overflow_safe() {
        assert_eq!(base64_encoded_len(0), Some(0));
        assert_eq!(base64_encoded_len(1), Some(4));
        assert_eq!(base64_encoded_len(2), Some(4));
        assert_eq!(base64_encoded_len(3), Some(4));
        assert_eq!(base64_encoded_len(4), Some(8));
        assert_eq!(base64_encoded_len(usize::MAX), None);
        let largest_raw = (MAX_BASE64_PDF_BYTES / 4) * 3;
        assert_eq!(base64_encoded_len(largest_raw), Some(MAX_BASE64_PDF_BYTES));
        assert!(base64_encoded_len(largest_raw + 1).unwrap() > MAX_BASE64_PDF_BYTES);
    }

    #[tokio::test]
    async fn print_to_pdf_returns_paginated_pdf_with_requested_media_box() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = format!("{page_id}-session");
        ctx.sessions.insert(session_id.clone(), page_id);
        let session = Some(session_id);
        ctx.get_session_page_mut(&session)
            .unwrap()
            .set_viewport((100.0, 80.0));
        page::handle(
            "navigate",
            &json!({
                "url": "data:text/html,<html style='margin:0'><body style='margin:0;height:400px;background:linear-gradient(red,blue)'></body></html>",
                "waitUntil": "load",
            }),
            &mut ctx,
            &session,
        ).await.expect("navigate fixture");
        let response = print_to_pdf(
            &json!({
                "paperWidth": 4.0, "paperHeight": 6.0,
                "marginTop": 0.5, "marginBottom": 0.5,
                "marginLeft": 0.5, "marginRight": 0.5,
                "printBackground": true,
            }),
            &mut ctx,
            &session,
        )
        .await
        .expect("raster PDF");
        assert_eq!(response["obscuraPrintMode"], "screen-raster");
        assert_eq!(response["obscuraPrintBackground"], true);
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response["data"].as_str().unwrap())
            .unwrap();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/MediaBox [0 0 288.000 432.000]"));
        assert!(
            text.matches("/Subtype /Image").count() >= 2,
            "400px tall fixture should paginate at this printable aspect ratio"
        );

        let streamed = page::handle(
            "printToPDF",
            &json!({
                "transferMode": "ReturnAsStream",
                "landscape": false,
                "displayHeaderFooter": false,
                "headerTemplate": "",
                "footerTemplate": "",
                "printBackground": false,
                "scale": 1,
                "paperWidth": 8.5,
                "paperHeight": 11,
                "marginTop": 0,
                "marginBottom": 0,
                "marginLeft": 0,
                "marginRight": 0,
                "pageRanges": "",
                "preferCSSPageSize": false,
                "generateTaggedPDF": true,
                "generateDocumentOutline": false,
            }),
            &mut ctx,
            &session,
        )
        .await
        .expect("Puppeteer/Playwright-shaped stream request");
        assert_eq!(streamed["data"], "");
        assert_eq!(streamed["obscuraPrintBackground"], true);
        assert_eq!(streamed["obscuraRequestedPrintBackground"], false);
        assert_eq!(streamed["obscuraTaggedPDF"], false);
        assert_eq!(streamed["obscuraRequestedTaggedPDF"], true);
        assert_eq!(
            streamed["obscuraIgnoredOptions"],
            json!(["printBackground", "generateTaggedPDF"])
        );
        let handle = streamed["stream"].as_str().expect("protocol stream handle");
        let mut streamed_bytes = Vec::new();
        loop {
            let chunk = crate::domains::io::handle(
                "read",
                &json!({"handle": handle, "size": 1024}),
                &mut ctx,
            )
            .await
            .expect("read PDF stream");
            streamed_bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(chunk["data"].as_str().unwrap())
                    .unwrap(),
            );
            if chunk["eof"] == true {
                break;
            }
        }
        assert!(streamed_bytes.starts_with(b"%PDF-1.4"));
        assert!(streamed_bytes.ends_with(b"%%EOF\n"));
        crate::domains::io::handle("close", &json!({"handle": handle}), &mut ctx)
            .await
            .expect("close PDF stream");
        assert!(
            crate::domains::io::handle("read", &json!({"handle": handle}), &mut ctx,)
                .await
                .is_err(),
            "closed PDF handles must release their buffer"
        );
    }
}
