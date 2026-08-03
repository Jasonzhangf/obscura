use serde_json::{json, Value};

use crate::dispatch::CdpContext;

const MAX_DEVICE_METRIC_DIMENSION: i64 = 10_000_000;
const DEFAULT_VIEWPORT: (f32, f32) = (1280.0, 720.0);

fn metric_dimension(params: &Value, name: &str) -> Result<u32, String> {
    let value = params
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Emulation.setDeviceMetricsOverride requires integer {name}"))?;
    if !(0..=MAX_DEVICE_METRIC_DIMENSION).contains(&value) {
        return Err(format!(
            "Emulation.setDeviceMetricsOverride {name} must be between 0 and {MAX_DEVICE_METRIC_DIMENSION}"
        ));
    }
    Ok(value as u32)
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "setDeviceMetricsOverride" => {
            let width = metric_dimension(params, "width")?;
            let height = metric_dimension(params, "height")?;
            let device_scale_factor = params
                .get("deviceScaleFactor")
                .and_then(Value::as_f64)
                .ok_or("Emulation.setDeviceMetricsOverride requires deviceScaleFactor")?;
            if !device_scale_factor.is_finite() || device_scale_factor < 0.0 {
                return Err(
                    "Emulation.setDeviceMetricsOverride requires a non-negative finite deviceScaleFactor"
                        .to_string(),
                );
            }
            let page = ctx
                .get_session_page_mut(session_id)
                .ok_or("No page for session")?;
            page.set_viewport((
                if width == 0 {
                    DEFAULT_VIEWPORT.0
                } else {
                    width as f32
                },
                if height == 0 {
                    DEFAULT_VIEWPORT.1
                } else {
                    height as f32
                },
            ));
            page.set_device_scale_factor(device_scale_factor as f32);
            Ok(json!({}))
        }
        "clearDeviceMetricsOverride" => {
            let page = ctx
                .get_session_page_mut(session_id)
                .ok_or("No page for session")?;
            page.set_viewport(DEFAULT_VIEWPORT);
            page.set_device_scale_factor(1.0);
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
        ctx.sessions.insert(session_id.clone().unwrap(), page_id);

        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 1024, "height": 768, "deviceScaleFactor": 2}),
            &mut ctx,
            &session_id,
        )
        .await
        .expect("metrics override");

        let page = ctx
            .get_session_page_mut(&session_id)
            .expect("page for session");
        assert_eq!(page.viewport, (1024.0, 768.0));
        assert_eq!(page.device_scale_factor, 2.0);
        assert_eq!(
            page.evaluate(
                "return [innerWidth, innerHeight, visualViewport.width,\
                         visualViewport.height, devicePixelRatio];"
            ),
            json!([1024, 768, 1024, 768, 2])
        );
    }

    #[tokio::test]
    async fn device_metrics_override_rejects_fractional_and_out_of_range_dimensions() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = Some("viewport-session".to_string());
        ctx.sessions.insert(session_id.clone().unwrap(), page_id);
        for params in [
            json!({"width": 0.5, "height": 768, "deviceScaleFactor": 1}),
            json!({"width": 10_000_001, "height": 768, "deviceScaleFactor": 1}),
            json!({"width": -1, "height": 768, "deviceScaleFactor": 1}),
        ] {
            assert!(
                handle("setDeviceMetricsOverride", &params, &mut ctx, &session_id)
                    .await
                    .is_err(),
                "must reject {params}"
            );
        }
    }

    #[tokio::test]
    async fn zero_dimensions_disable_size_override() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = Some("zero-size-session".to_string());
        ctx.sessions.insert(session_id.clone().unwrap(), page_id);
        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 0, "height": 0, "deviceScaleFactor": 0}),
            &mut ctx,
            &session_id,
        )
        .await
        .expect("zero disables the overrides");

        let page = ctx.get_session_page_mut(&session_id).unwrap();
        assert_eq!(page.viewport, DEFAULT_VIEWPORT);
        assert_eq!(page.device_scale_factor, 1.0);
    }

    #[tokio::test]
    async fn device_scale_factor_zero_disables_override_and_clear_restores_defaults() {
        let mut ctx = CdpContext::new();
        let page_id = ctx.create_page();
        let session_id = Some("scale-session".to_string());
        ctx.sessions.insert(session_id.clone().unwrap(), page_id);

        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 640, "height": 480, "deviceScaleFactor": 3}),
            &mut ctx,
            &session_id,
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.get_session_page(&session_id)
                .unwrap()
                .device_scale_factor,
            3.0
        );

        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 640, "height": 480, "deviceScaleFactor": 0}),
            &mut ctx,
            &session_id,
        )
        .await
        .expect("zero disables the scale override");
        assert_eq!(
            ctx.get_session_page(&session_id)
                .unwrap()
                .device_scale_factor,
            1.0
        );

        handle(
            "clearDeviceMetricsOverride",
            &json!({}),
            &mut ctx,
            &session_id,
        )
        .await
        .unwrap();
        let page = ctx.get_session_page(&session_id).unwrap();
        assert_eq!(page.viewport, (1280.0, 720.0));
        assert_eq!(page.device_scale_factor, 1.0);
    }
}
