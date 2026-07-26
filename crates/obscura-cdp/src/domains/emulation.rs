use serde_json::{json, Value};

use crate::dispatch::CdpContext;

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "setDeviceMetricsOverride" => {
            let width = params
                .get("width")
                .and_then(Value::as_f64)
                .ok_or("Emulation.setDeviceMetricsOverride requires width")?;
            let height = params
                .get("height")
                .and_then(Value::as_f64)
                .ok_or("Emulation.setDeviceMetricsOverride requires height")?;
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err(
                    "Emulation.setDeviceMetricsOverride requires positive finite dimensions"
                        .to_string(),
                );
            }
            let page = ctx
                .get_session_page_mut(session_id)
                .ok_or("No page for session")?;
            page.set_viewport((width as f32, height as f32));
            Ok(json!({}))
        }
        "clearDeviceMetricsOverride" => {
            let page = ctx
                .get_session_page_mut(session_id)
                .ok_or("No page for session")?;
            page.set_viewport((1280.0, 720.0));
            Ok(json!({}))
        }
        // Touch emulation does not affect layout yet, but acknowledging it is
        // compatible with clients that pair it with a metrics override.
        "setTouchEmulationEnabled" => Ok(json!({})),
        _ => Ok(json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn device_metrics_override_updates_page_and_window_viewport() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = Some("viewport-session".to_string());
        ctx.sessions
            .insert(session_id.clone().unwrap(), page_id);

        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 1024, "height": 768, "deviceScaleFactor": 1}),
            &mut ctx,
            &session_id,
        )
        .await
        .expect("metrics override");

        let page = ctx
            .get_session_page_mut(&session_id)
            .expect("page for session");
        assert_eq!(page.viewport, (1024.0, 768.0));
        assert_eq!(
            page.evaluate(
                "return [innerWidth, innerHeight, visualViewport.width,\
                         visualViewport.height];"
            ),
            json!([1024, 768, 1024, 768])
        );
    }

    #[tokio::test]
    async fn device_metrics_override_rejects_zero_dimensions() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = Some("viewport-session".to_string());
        ctx.sessions
            .insert(session_id.clone().unwrap(), page_id);
        let error = handle(
            "setDeviceMetricsOverride",
            &json!({"width": 0, "height": 768}),
            &mut ctx,
            &session_id,
        )
        .await
        .expect_err("zero width must fail");
        assert!(error.contains("positive finite dimensions"));
    }
}
