The `obscura` crate embeds the engine in a Rust program with a `Browser` / `Page` / `Element` API plus a cookie store, no CDP round-trips. It builds V8 from source, so it is a git dependency rather than a crates.io release.

## Add the dependency

```toml
[dependencies]
obscura = { git = "https://github.com/h4ckf0r0day/obscura" }
tokio = { version = "1", features = ["rt", "macros"] }
anyhow = "1"
```

The first build compiles V8 from source, so it is slow and needs the same build tools as [Build from source](Build-from-source.md). Pin a tag for reproducible builds:

```toml
obscura = { git = "https://github.com/h4ckf0r0day/obscura", tag = "v0.1.7" }
```

## Quickstart

```rust
use obscura::Browser;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let browser = Browser::builder()
        .stealth(true)
        .build()?;

    let mut page = browser.new_page().await?;
    page.goto("https://example.com").await?;

    println!("URL: {}", page.url());
    println!("HTML bytes: {}", page.content().len());

    let el = page.wait_for_selector("h1", Duration::from_secs(5)).await?;
    println!("Heading: {}", el.text());

    let title = page.evaluate("document.title");
    println!("Title: {}", title);

    Ok(())
}
```

## API surface

`Browser::builder()` configures the engine: `.stealth(bool)`, `.proxy(url)`, `.user_agent(ua)`, `.storage_dir(dir)`, then `.build()`. `Browser::new()` uses defaults.

`Page`:
- `goto(url).await` navigate and wait for load
- `content()` rendered HTML
- `url()` current URL
- `evaluate(js)` run JavaScript, returns a `serde_json::Value`
- `query_selector(css)` first match as an `Element`, or `None`
- `wait_for_selector(css, Duration).await` poll until present
- `settle(max_ms).await` drive the event loop so async work (`fetch`, timers) completes

`Element`: `text()`, `attribute(name)`, `click()`.

`CookieStore`: `set`, `get_all`, `get_for_url`, `save_to_file`, `load_from_file`.

## When to use which interface

- Embedding the engine in a Rust service: this crate.
- Driving from Node/Python with existing Puppeteer/Playwright code: the [CDP server](Connect-Puppeteer-or-Playwright.md).
- Giving an AI agent browser tools: the [MCP server](Use-the-MCP-server.md).
- One-off fetches and scraping from the shell: the [CLI](CLI-reference.md).
