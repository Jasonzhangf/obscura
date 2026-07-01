# obscura-render

An optional, scoped render layer for Obscura. It adds real CSS box geometry to
the JS surface and rasterizes PNG screenshots, pure Rust, behind an opt-in
feature. The default scraping build is unchanged.

## Why

Obscura's speed and low memory come from not carrying a full browser rendering
pipeline. This layer adds only what scraping and automation need, real box
geometry (so `getBoundingClientRect`, `elementFromPoint`, and
`IntersectionObserver` return true values) and screenshots, without the weight
of a Chromium-class engine.

## Enable

```bash
cargo build --release --features render
```

The feature propagates from `obscura-cli` through `obscura-browser`,
`obscura-cdp`, and `obscura-js` to `obscura-render`. The default build compiles
it out entirely.

## Use

CLI screenshot of the settled page:

```bash
obscura fetch --screenshot out.png https://example.com/
```

CDP (Puppeteer / Playwright `page.screenshot()`), driving `obscura serve`:

```js
await page.goto('https://example.com/');
await page.screenshot({ path: 'out.png' });   // Page.captureScreenshot
```

Real geometry in JS (no screenshot needed):

```js
const r = await page.evaluate(() => document.querySelector('div').getBoundingClientRect());
// r.x, r.y, r.width, r.height are laid-out values, not synthetic
```

## What it renders

- Block, flexbox, and CSS grid box layout via [taffy](https://github.com/dioxusLabs/taffy).
- Inline styles plus a small built-in UA default sheet, parsed for the
  layout-relevant properties (display, width/height, margin, padding, border)
  and background-color.
- Background colors painted to the pixmap (backgrounds, borders, and text are
  staged below).

## Benchmark

Same page, same 1280x720 viewport, warmed, on the same host. Headless Chrome is
`google-chrome --headless=new --screenshot`.

| page          | engine   | wall     | peak RSS | PNG    |
| ------------- | -------- | -------- | -------- | ------ |
| local fixture | obscura  | ~0.06s   | ~33 MB   | 5.3 KB |
| local fixture | chrome   | ~0.90s   | ~195 MB  | 4.4 KB |
| example.com   | obscura  | ~0.10s   | ~34 MB   | 5.2 KB |
| example.com   | chrome   | ~0.98s   | ~199 MB  | 17 KB  |

Obscura is roughly 10-15x faster and uses about 6x less memory than headless
Chrome for a screenshot. Chrome's larger PNG on example.com is because it
renders text, which obscura does not yet paint.

## Limitations and next steps

These are tracked enhancements, not bugs:

- **Text** is not rendered yet (the biggest fidelity gap).
- **Borders and images** are not painted; only background colors.
- **CSS coverage** is the layout-relevant subset. Full cascade (selector
  matching, media queries, inheritance of all properties, relative units) is
  not implemented.
- The layout cache is computed lazily and cleared on navigation. It is not
  invalidated on every DOM mutation, so geometry read mid-script may lag a
  frame; reading after settle is reliable.

## Architecture

`obscura-render` is its own workspace crate with two optional capabilities:

- **Layout (default):** `layout_dom(dom, viewport)` walks a `DomTree`, computes
  each element's style, builds a taffy tree, and returns border-box geometry
  keyed by `NodeId`. Exposed to JS by `op_layout_geometry` (feature-gated).
- **Paint (`paint` feature):** `paint_dom` / `screenshot_png` rasterize the
  laid-out tree with [tiny-skia](https://crates.io/crates/tiny-skia) (CPU, pure
  Rust, deterministic) to a pixmap or PNG bytes.

The JS surface stays synthetic when the feature is off: `bootstrap.js` probes
`op_layout_geometry` with `typeof` and falls through to the existing synthetic
rect, so the default build is byte-for-byte equivalent.
