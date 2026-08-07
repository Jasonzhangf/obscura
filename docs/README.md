Obscura is an open-source headless browser engine written in Rust. It runs JavaScript through V8, owns its DOM, layout, and paint pipeline, and speaks the Chrome DevTools Protocol for common Puppeteer and Playwright workflows.

The render-enabled release supports screenshots, scroll-aware layout,
activity-driven CDP screencasting, and raster PDF export. Obscura is an
independent engine rather than a bundled Chromium build, so compatibility and
performance should be measured against the pages and automation flows that
matter to you.

## Quickstart

- [Installation](Installation.md)
- [Your first fetch](Your-first-fetch.md)
- [Extract data](Extract-data.md)
- [Connect Puppeteer or Playwright](Connect-Puppeteer-or-Playwright.md)

## Guides

- [Build from source](Build-from-source.md)
- [Rendering, screenshots, screencasting, and PDF](Rendering-screenshots-screencasting-and-PDF.md)
- [Configure stealth and proxies](Configure-stealth-and-proxies.md)
- [Markdown extraction](Markdown-extraction.md)
- [Use with Puppeteer](Use-with-Puppeteer.md)
- [Use with Playwright](Use-with-Playwright.md)
- [Use the MCP server](Use-the-MCP-server.md)
- [Use as a Rust library](Use-as-a-Rust-library.md)
- [Persist cookies and storage](Persist-cookies-and-storage.md)
- [Intercept and modify requests](Intercept-and-modify-requests.md)
- [Run in production at scale](Run-in-production-at-scale.md)

## Reference

- [CLI reference](CLI-reference.md)
- [Environment variables](Environment-variables.md)

## Contributing

- [Architecture overview](Architecture-overview.md)
- [Adding a CDP method or Web API](Adding-a-CDP-method-or-Web-API.md)
- [Testing and debugging](Testing-and-debugging.md)

## Links

- Source: https://github.com/h4ckf0r0day/obscura
- Releases: https://github.com/h4ckf0r0day/obscura/releases
- Issues: https://github.com/h4ckf0r0day/obscura/issues

License: Apache-2.0.
