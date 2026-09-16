//! Fork-only: adopt a URL the page routed to itself.
//!
//! A single page app answers a click by calling `history.pushState` and
//! rendering the next view in place. `bootstrap.js` tracks that in
//! `__virtualUrl` so `location.href` reads correctly, but nothing on the Rust
//! side ever looked at it, so the page had moved on while `page.url()` still
//! reported the old document. To a CDP client that is a click that did nothing,
//! and `tools/ab/clicklocal.mjs` reports it as `spa route: FAILED`.
//!
//! Ported from fork commit `d7dca7a`. Kept out of `page.rs` so an upstream
//! rewrite of that file does not touch it; Rust allows an inherent impl in any
//! module of the defining crate, so the call site still reads
//! `self.sync_virtual_url()`.

use url::Url;

use crate::page::Page;

impl Page {
    /// Adopt a URL the page routed to itself, without fetching anything.
    ///
    /// Returns whether the URL changed.
    pub fn sync_virtual_url(&mut self) -> bool {
        let Some(js) = self.js.as_mut() else {
            return false;
        };
        let Ok(virtual_url) = js.evaluate("globalThis.__virtualUrl || ''") else {
            return false;
        };
        let Some(virtual_url) = virtual_url.as_str().filter(|url| !url.is_empty()) else {
            return false;
        };
        let Ok(parsed) = Url::parse(virtual_url) else {
            return false;
        };
        if self.url.as_ref() == Some(&parsed) {
            return false;
        }
        js.set_url(virtual_url);
        self.url = Some(parsed);
        self.push_history(self.url_string());
        true
    }
}

#[cfg(all(test, feature = "render"))]
mod tests {
    use std::io::{Read, Write};

    use super::Page;

    #[tokio::test(flavor = "current_thread")]
    async fn virtual_url_change_keeps_prepared_frame_available() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let body = r#"<!doctype html><html><body>
                <div style="width:100px;height:100px;background:red"></div>
            </body></html>"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let context = std::sync::Arc::new(crate::BrowserContext::with_storage_and_network(
            "virtual-url-frame".to_string(),
            None,
            false,
            None,
            None,
            true,
        ));
        let mut page = Page::new("virtual-url-frame".to_string(), context);
        page.set_viewport((100.0, 80.0));
        page.navigate(&format!("http://{address}/start"))
            .await
            .unwrap();

        page.render_frame().expect("initial frame");
        page.evaluate("history.pushState({}, '', '/route?tabtype=weibo')");
        assert!(
            page.sync_virtual_url(),
            "history.pushState must be adopted by the Page URL"
        );
        page.render_frame()
            .expect("frame must stay available after a same-document URL change");
    }
}
