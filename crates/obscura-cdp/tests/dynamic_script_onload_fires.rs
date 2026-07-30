// Regression for issue #474: external scripts inserted after a timer must be
// fetched and execute before the post-navigation event-loop settle completes.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let read = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..read]);
                let (content_type, body) = if request.starts_with("GET /direct.js") {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    ("application/javascript", "window.__directExecuted = true;")
                } else if request.starts_with("GET /nested.js") {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    ("application/javascript", "window.__nestedExecuted = true;")
                } else {
                    (
                        "text/html",
                        r#"<html><body>
<div id="r">stage1</div>
<script>
setTimeout(function () {
  var direct = document.createElement("script");
  direct.src = "/direct.js";
  direct.onload = function () { window.__directLoaded = true; };
  document.body.appendChild(direct);

  var box = document.createElement("div");
  var nested = document.createElement("script");
  nested.src = "/nested.js";
  nested.onload = function () { window.__nestedLoaded = true; };
  box.appendChild(nested);
  document.body.appendChild(box);
}, 100);
</script>
</body></html>"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

async fn cdp(
    ctx: &mut CdpContext,
    id: u64,
    method: &str,
    params: Value,
    session_id: &str,
) -> Value {
    let response = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session_id.to_string()),
        },
        ctx,
    )
    .await;
    assert!(
        response.error.is_none(),
        "CDP {method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or_else(|| json!({}))
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_external_scripts_execute_and_fire_load() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": url, "waitUntil": "load"}),
        session_id,
    )
    .await;

    let result = cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify({directExecuted: !!window.__directExecuted, directLoaded: !!window.__directLoaded, nestedExecuted: !!window.__nestedExecuted, nestedLoaded: !!window.__nestedLoaded})",
            "returnByValue": true,
        }),
        session_id,
    )
    .await;
    assert_eq!(
        result["result"]["value"],
        r#"{"directExecuted":true,"directLoaded":true,"nestedExecuted":true,"nestedLoaded":true}"#,
        "dynamic scripts must execute and fire load before navigation settles"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_data_scripts_execute_before_chained_load_handlers() {
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id);

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({
            "url": "data:text/html,<html><head></head><body></body></html>",
            "waitUntil": "load"
        }),
        session_id,
    )
    .await;

    cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({
            "expression": r#"(function () {
                var state = {
                    aExec: false,
                    aLoad: false,
                    bExec: false,
                    bLoad: false,
                    cExec: false,
                    cLoad: false
                };
                window.__dataScriptState = state;

                var a = document.createElement('script');
                a.src = 'data:text/javascript,window.__dataScriptState.aExec=true';
                a.onload = function () {
                    state.aLoad = true;
                    var b = document.createElement('script');
                    b.src = 'data:application/javascript,' + encodeURIComponent('window.__dataScriptState.bExec=true');
                    b.onload = function () {
                        state.bLoad = true;
                        var c = document.createElement('script');
                        c.src = 'data:text/javascript;base64,' + btoa('window.__dataScriptState.cExec=true');
                        c.onload = function () { state.cLoad = true; };
                        document.head.appendChild(c);
                    };
                    document.head.appendChild(b);
                };
                document.head.appendChild(a);
                return 'kicked';
            })()"#,
            "returnByValue": true,
        }),
        session_id,
    )
    .await;

    ctx.pages[0].settle(500).await;

    let result = cdp(
        &mut ctx,
        3,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify(window.__dataScriptState)",
            "returnByValue": true,
        }),
        session_id,
    )
    .await;
    assert_eq!(
        result["result"]["value"],
        r#"{"aExec":true,"aLoad":true,"bExec":true,"bLoad":true,"cExec":true,"cLoad":true}"#,
        "data URL script bodies must execute before each chained load handler"
    );
}
