Rendering is available in official release archives and Docker images. Source
builds must enable the `render` feature:

```bash
cargo build --release --features render
```

Use `--features render,stealth` when the same binary also needs the optional
stealth transport.

## CLI screenshots

Capture the settled page as a PNG:

```bash
obscura fetch https://example.com --screenshot page.png --timeout 30
```

JavaScript supplied with `--eval` runs before capture, so it can prepare or
scroll the page:

```bash
obscura fetch https://example.com \
  --eval "window.scrollTo(0, document.documentElement.scrollHeight)" \
  --screenshot bottom.png
```

The CLI captures one URL and writes PNG. An omitted `--wait` uses adaptive
settling with a five-second cap; an explicit `--wait N` requests a fixed delay
of `N` seconds. Navigation and the capture path prepare discovered images and
fonts within bounded deadlines rather than waiting indefinitely for a broken
resource.

## Puppeteer

Start the render-enabled CDP server:

```bash
obscura serve --port 9222
```

Then use the standard Puppeteer APIs:

```js
import puppeteer from 'puppeteer-core';

const browser = await puppeteer.connect({
  browserWSEndpoint: 'ws://127.0.0.1:9222/devtools/browser',
});
const page = await browser.newPage();
await page.setViewport({ width: 1440, height: 1000, deviceScaleFactor: 1 });
await page.goto('https://example.com', { waitUntil: 'load' });

await page.screenshot({ path: 'viewport.png' });
await page.screenshot({ path: 'full-page.png', fullPage: true });
await page.pdf({
  path: 'page.pdf',
  format: 'A4',
  printBackground: true,
});

await browser.disconnect();
```

## Playwright

Use `connectOverCDP`, not Playwright's native `connect` protocol:

```js
import { chromium } from 'playwright-core';

const browser = await chromium.connectOverCDP('ws://127.0.0.1:9222');
const context = browser.contexts()[0] || await browser.newContext();
const page = await context.newPage();
await page.setViewportSize({ width: 1440, height: 1000 });
await page.goto('https://example.com', { waitUntil: 'load' });

await page.screenshot({ path: 'viewport.png' });
await page.screenshot({ path: 'full-page.png', fullPage: true });
await page.pdf({ path: 'page.pdf', format: 'A4', printBackground: true });

await browser.close();
```

## Scrolling and capture

Window and nested-element scrolling update renderer-owned geometry and paint.
Fixed and sticky elements are sampled at the live scroll position.

```js
await page.evaluate(() => window.scrollTo(0, 1200));
await page.screenshot({ path: 'scrolled.png' });

await page.evaluate(() => {
  document.querySelector('.overflow-panel')?.scrollTo(0, 600);
});
await page.screenshot({ path: 'nested-scroll.png' });
```

`fullPage: true` captures document space independently of the current viewport
scroll. A normal screenshot captures the currently visible viewport.

## Raw CDP screenshot

`Page.captureScreenshot` supports PNG, JPEG, and lossless WebP, viewport clips,
scaling, full-page capture through `captureBeyondViewport`, device metrics, and
transparent background overrides. `fromSurface: false` is not supported, and
the WebP encoder does not currently accept a quality setting.

```js
import fs from 'node:fs';

const client = await page.createCDPSession();
const { data } = await client.send('Page.captureScreenshot', {
  format: 'png',
  captureBeyondViewport: true,
});
await fs.promises.writeFile('capture.png', Buffer.from(data, 'base64'));
```

## Screencasting

Screencasting is exposed through CDP. Frames are emitted when navigation,
input, timers, animation frames, or visual mutations produce new content.
Clients must acknowledge frames; Obscura bounds unacknowledged work instead of
letting a slow consumer grow memory without limit.

```js
const client = await page.createCDPSession();

client.on('Page.screencastFrame', async ({ data, metadata, sessionId }) => {
  // Consume or forward Buffer.from(data, 'base64') here.
  console.log(metadata.timestamp, metadata.scrollOffsetY);
  await client.send('Page.screencastFrameAck', { sessionId });
});

await client.send('Page.startScreencast', {
  format: 'jpeg',
  quality: 80,
  maxWidth: 1280,
  maxHeight: 720,
  everyNthFrame: 1,
});

// ...navigate and interact...

await client.send('Page.stopScreencast');
```

This is an activity-driven page stream, not desktop/window capture or a
guarantee of a fixed frame rate.

## PDF export

Puppeteer and Playwright `page.pdf()` route to `Page.printToPDF`. Raw CDP can
return Base64 directly or a stream read with `IO.read` and closed with
`IO.close`.

The exporter uses print-media style and supports paper dimensions, margins,
landscape, scale, backgrounds, and page ranges. Output is raster-backed, so
text is not selectable/searchable and tagged PDFs, outlines, headers/footers,
`preferCSSPageSize`, and complete CSS `@page` sizing are not yet available.

## MCP visual output

Render-enabled MCP servers expose:

- `browser_screenshot`: returns the current page as an MCP PNG image content
  block.
- `browser_pdf`: returns the current page as an embedded `application/pdf`
  resource.

The screenshot tool accepts optional CSS-pixel `width` and `height`. The PDF
tool accepts `landscape`, `print_background`, `scale`, `paper_width`,
`paper_height`, and `margin_top` / `margin_bottom` / `margin_left` /
`margin_right`; paper and margin values are inches.

```bash
obscura mcp
# or
obscura mcp --http --host 127.0.0.1 --port 3000
```

Navigate first, then call the visual tool. MCP does not expose streaming
screencast frames; use the CDP API for that workflow.

## Current boundaries

Obscura implements broad modern layout and paint support, including block and
inline formatting, flexbox, grid, tables, floats, positioned/fixed/sticky
boxes, transforms, overflow, text and web fonts, images, SVG, canvas,
backgrounds, borders, clipping, generated content, and animation sampling.

It is still an evolving browser engine. Long-tail CSS, service workers, some
Web APIs, native media playback, GPU/compositor effects, and exact platform
font rasterization can differ from Chromium. Treat visual comparisons as
evidence: confirm both captures succeeded and are nonblank, compare the same
viewport/wait/identity/scroll/animation state, and inspect geometry and missing
resources in addition to pixel error.
