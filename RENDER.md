# Obscura rendering engine

Obscura has an optional pure-Rust layout and paint pipeline for browser
automation. The `render` feature powers live DOM geometry, viewport and
full-page screenshots, CDP screencasting, and raster PDF export without
launching Chromium.

Official release archives and the Docker image are built with rendering. When
building from source, enable it explicitly:

```bash
cargo build --release --features render

# Rendering plus the optional stealth transport
cargo build --release --features render,stealth
```

## Capture surfaces

- CLI PNG: `obscura fetch https://example.com --screenshot page.png`
- Puppeteer/Playwright: `page.screenshot()` and `page.pdf()` over CDP
- Raw CDP: `Page.captureScreenshot`, `Page.startScreencast`, and
  `Page.printToPDF`
- MCP: `browser_screenshot` and `browser_pdf` in render-enabled builds

Screencasting is an activity-driven CDP stream. Consumers must acknowledge
each `Page.screencastFrame` with `Page.screencastFrameAck`; it is not a desktop
video-recorder API.

## Implemented rendering coverage

The engine includes selector matching and cascade, media queries, inherited
and relative values, block and inline formatting, flexbox, grid, tables,
floats, positioned and sticky elements, overflow and scrolling, transforms,
clipping, backgrounds, borders, shadows, shaped text and web fonts, images,
SVG, canvas, generated content, and animation sampling. Layout is retained and
invalidated by relevant DOM/style/resource mutations so geometry and capture
observe the same page state.

This is broad browser-rendering support, not a claim that every long-tail CSS,
DOM, compositor, font, or graphics behavior is already identical to Chromium.
Test the sites and workflows that matter to you and report reduced fixtures
for reproducible differences.

## Architecture

`obscura-render` consumes the shared DOM and computed style state. Taffy
provides the flex/grid foundation; Obscura layers browser formatting behavior,
text shaping, intrinsic replaced-element sizing, retained layout, and a
CPU-backed paint pipeline on top. `obscura-js` exposes renderer-owned geometry
to browser APIs, `obscura-browser` owns resource preparation and capture, and
`obscura-cdp` maps the result to Chrome DevTools Protocol methods.

PDF output is intentionally raster-backed. It honors print media, backgrounds,
paper size, margins, landscape, scale, and page ranges, but does not yet provide
selectable PDF text, tagged structure, headers/footers, outlines, or complete
CSS paged-media support.

## Verification

Rendering changes are checked with deterministic fixtures first, then a broad
real-site suite against Chromium. Captures must use the same viewport, identity,
settle policy, animation time, scroll position, and output boundary. Pixel
error is a regression tripwire, not a standalone correctness verdict; geometry,
structural edges, missing resources, nonblank output, and reduced repros decide
whether a difference is real.

See [Rendering, screenshots, screencasting, and PDF](docs/Rendering-screenshots-screencasting-and-PDF.md)
for user-facing examples and current limits.
