use serde_json::{json, Value};
use crate::dispatch::CdpContext;

/// CDP owns event projection; browser input semantics live with Page.
pub async fn handle(method: &str, params: &Value, ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    // Existing protocol compatibility commands; not Host input capabilities.
    if matches!(method, "dispatchTouchEvent" | "setIgnoreInputEvents") { return Ok(json!({})); }
    let page = ctx.get_session_page_mut(session_id).ok_or("Input page unavailable")?;
    obscura_browser::input::dispatch(method, params, page).await?;
    // Preserve CDP's navigation wait and frame event projection. Host instead
    // acknowledges input dispatch before its autonomous navigation work.
    if method == "dispatchMouseEvent" && params["type"] == "mouseReleased"
        && page.process_pending_navigation().await.map_err(|e| e.to_string())? {
        let page_id = page.id.clone(); let frame_id = page.frame_id.clone(); let url = page.url_string();
        let loader_id = ctx.current_loader_ids.get(&page_id).cloned()
            .unwrap_or_else(|| format!("loader-blank-{page_id}"));
        ctx.pending_events.push(crate::types::CdpEvent {
            method: "Page.frameNavigated".into(),
            params: json!({
                "frame": crate::domains::page::frame_value(&frame_id, None, &loader_id, &url, "text/html"),
                "type": "Navigation",
            }),
            session_id: Some(session_id.clone().unwrap_or_default()),
        });
    }
    Ok(json!({}))
}
