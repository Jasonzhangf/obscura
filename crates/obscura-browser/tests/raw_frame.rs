#![cfg(feature = "render")]
use std::sync::Arc;
use obscura_browser::{BrowserContext, Page};

#[tokio::test(flavor = "current_thread")]
async fn raw_frame_matches_png_pixels_and_preserves_document_on_resize() {
    let id = "raw-frame-fixture".to_string();
    let context = Arc::new(BrowserContext::with_storage_and_network(id.clone(), None, false, None, None, false));
    let mut page = Page::new(id, context);
    page.set_viewport((240.0, 180.0));
    page.navigate("data:text/html,<style>html,body{margin:0;background:white}input{display:block}div{height:800px;background:rgb(10,80,150)}</style><input id='field' value='retained'><canvas id='canvas' width='50' height='50'></canvas><div></div><script>var c=document.getElementById('canvas').getContext('2d');c.fillStyle='rgba(255,0,0,0.5)';c.fillRect(0,0,50,50);</script>").await.unwrap();
    let (width, height, rgba) = page.render_frame().unwrap();
    let png = image::load_from_memory(&page.screenshot(page.viewport).unwrap()).unwrap().to_rgba8();
    assert_eq!((width, height), png.dimensions());
    assert_eq!(rgba, png.into_raw(), "opaque canvas, DOM and PNG paths must paint identical pixels");
    page.set_viewport((390.0, 844.0));
    assert_eq!(page.evaluate("document.getElementById('field').value"), "retained");
    page.evaluate("window.scrollTo(0,100)");
    let (width, height, rgba) = page.render_frame().unwrap();
    assert_eq!((width, height), (390, 844));
    let png = image::load_from_memory(&page.screenshot(page.viewport).unwrap()).unwrap().to_rgba8();
    assert_eq!(rgba, png.into_raw());
}
