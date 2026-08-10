//! Regression test for the protocol half of issue #600: `Page.getFrameTree`
//! reported `childFrames: []` however many frames a page had built, and no
//! `Page.frameAttached` was ever emitted, so a Playwright or Puppeteer client
//! saw a single-frame page and could never address the child.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// `/` embeds `/child.html`, which itself embeds `/grandchild.html`, so the
/// tree is deep enough to show nesting rather than a flat list.
async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let body = if request.starts_with("GET /child.html ") {
                    "<html><body><iframe src=\"/grandchild.html\"></iframe></body></html>"
                } else if request.starts_with("GET /grandchild.html ") {
                    "<html><body><p>deep</p></body></html>"
                } else {
                    "<html><body><iframe src=\"/child.html\"></iframe></body></html>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session: &str) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session.to_string()),
        },
        ctx,
    )
    .await;
    assert!(resp.error.is_none(), "CDP {method} failed: {:?}", resp.error);
    resp.result.unwrap_or_else(|| json!({}))
}

#[tokio::test(flavor = "current_thread")]
async fn get_frame_tree_reports_nested_child_frames() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session = "session-1";
    ctx.sessions.insert(session.to_string(), page_id);

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": url, "waitUntil": "load"}),
        session,
    )
    .await;
    // Frames are built when the page settles, which is not part of the
    // navigation itself.
    cdp(&mut ctx, 2, "Runtime.evaluate", json!({"expression": "1"}), session).await;

    let tree = cdp(&mut ctx, 3, "Page.getFrameTree", json!({}), session).await;
    let root = &tree["frameTree"];
    let child = &root["childFrames"][0];
    assert!(
        child["frame"]["url"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/child.html"),
        "no child frame in the tree: {tree}"
    );
    assert_eq!(
        child["frame"]["parentId"], root["frame"]["id"],
        "the child does not point back at the main frame"
    );

    let grandchild = &child["childFrames"][0];
    assert!(
        grandchild["frame"]["url"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/grandchild.html"),
        "a frame inside a frame is missing: {tree}"
    );
    assert_eq!(grandchild["frame"]["parentId"], child["frame"]["id"]);

    // A client builds its frame list from the events, so the tree alone is not
    // enough. Attach must precede the navigation of the same frame.
    let child_id = child["frame"]["id"].as_str().unwrap().to_string();
    let attached = ctx
        .pending_events
        .iter()
        .position(|e| e.method == "Page.frameAttached" && e.params["frameId"] == child_id)
        .expect("no Page.frameAttached for the child frame");
    let navigated = ctx
        .pending_events
        .iter()
        .position(|e| e.method == "Page.frameNavigated" && e.params["frame"]["id"] == child_id)
        .expect("no Page.frameNavigated for the child frame");
    assert!(
        attached < navigated,
        "frameAttached must come before frameNavigated"
    );

    // Each frame is announced once, however many commands the client sends.
    let before = ctx.pending_events.len();
    cdp(&mut ctx, 4, "Page.getFrameTree", json!({}), session).await;
    let repeats = ctx.pending_events[before..]
        .iter()
        .filter(|e| e.method == "Page.frameAttached")
        .count();
    assert_eq!(repeats, 0, "the same frame was announced twice");
}
