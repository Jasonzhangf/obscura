//! Regression test for issue #600: a child iframe's document was fetched and
//! parsed, but the frame never got a scripting context, so a `<script>` inside
//! it stayed an inert node. This is the reporter's loopback repro, reduced to
//! the part that needs no network: two documents on one local server.

use std::io::{Read, Write};

use obscura::Browser;

const PARENT_HTML: &str = r#"<!doctype html><html><head><title>parent</title></head><body>
<script>
  var f = document.createElement('iframe');
  f.src = '/child.html';
  document.body.appendChild(f);
</script>
</body></html>"#;

const STATIC_PARENT_HTML: &str = r#"<!doctype html><html><head><title>parent</title></head><body>
<iframe src="/child.html"></iframe>
</body></html>"#;

const CHILD_HTML: &str = r#"<!doctype html><html><head><title>BEFORE</title></head><body>
<p>child</p>
<script>
  window.__ran = "YES";
  document.title = "RAN-IN-CHILD";
</script>
</body></html>"#;

/// Minimal HTTP/1.1 server serving the parent document at `/` and the child at
/// `/child.html`.
fn spawn_server(parent_html: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = match incoming {
                Ok(stream) => stream,
                Err(_) => continue,
            };
            let mut buf = [0u8; 2048];
            let read = stream.read(&mut buf).unwrap_or(0);
            let body = if buf[..read].starts_with(b"GET /child.html ") {
                CHILD_HTML
            } else {
                parent_html
            };
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_child_frame_runs_its_own_script() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server(PARENT_HTML);

    let browser = Browser::new().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&base).await.unwrap();
    page.settle(2000).await;

    assert_eq!(
        page.frame_urls(),
        vec![format!("{base}/child.html")],
        "the child document never became a frame"
    );
    assert_eq!(
        page.evaluate_in_frame(0, "window.__ran").unwrap(),
        serde_json::json!("YES"),
        "the child frame's script did not run"
    );
    // The child's own script wrote this over the static <title>, so it proves
    // the script ran against the frame's document rather than anywhere else.
    assert_eq!(
        page.evaluate_in_frame(0, "document.title").unwrap(),
        serde_json::json!("RAN-IN-CHILD"),
    );
    // The frame's writes must not reach the parent's document.
    assert_eq!(
        page.evaluate("document.title").as_str().unwrap_or(""),
        "parent",
    );
    assert_eq!(page.evaluate("window.__ran"), serde_json::Value::Null);
}

/// A parser-created `<iframe src>` never goes through the `src` setter, so
/// nothing used to start its load at all.
#[tokio::test]
async fn a_static_child_frame_runs_its_own_script() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = spawn_server(STATIC_PARENT_HTML);

    let browser = Browser::new().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&base).await.unwrap();
    page.settle(2000).await;

    assert_eq!(
        page.frame_urls(),
        vec![format!("{base}/child.html")],
        "a static iframe never started loading"
    );
    assert_eq!(
        page.evaluate_in_frame(0, "window.__ran").unwrap(),
        serde_json::json!("YES"),
    );
}
