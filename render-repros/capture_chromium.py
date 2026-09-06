#!/usr/bin/env python3
"""Capture one deterministic Chromium reference with Playwright."""

import argparse
import base64
import hashlib
import json
from pathlib import Path

from playwright.sync_api import sync_playwright


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("url")
    parser.add_argument("output", type=Path)
    parser.add_argument("--width", type=int, default=900)
    parser.add_argument("--height", type=int, default=1000)
    parser.add_argument("--settle-ms", type=int, default=2000)
    parser.add_argument("--executable")
    parser.add_argument(
        "--bundled-fonts", action="store_true",
        help="Load Obscura's Liberation fonts for static fixture comparison; does not normalize native controls",
    )
    args = parser.parse_args()

    launch = {"headless": True}
    if args.executable:
        launch["executable_path"] = args.executable

    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(**launch)
        context = browser.new_context(
            viewport={"width": args.width, "height": args.height},
            device_scale_factor=1,
            locale="en-US",
            timezone_id="UTC",
            color_scheme="light",
        )
        page = context.new_page()
        fonts = []
        if args.bundled_fonts:
            assets = Path(__file__).resolve().parents[1] / "crates/obscura-render/assets"
            for family in ("sans", "serif", "mono"):
                for suffix, weight, style in (
                    ("", "400", "normal"), ("-bold", "700", "normal"),
                    ("-oblique", "400", "italic"), ("-boldoblique", "700", "italic"),
                ):
                    path = assets / f"liberation-{family}{suffix}.ttf"
                    data = path.read_bytes()
                    fonts.append({
                        "family": f"Liberation {family.title()}",
                        "weight": weight, "style": style,
                        "file": path.name, "sha256": hashlib.sha256(data).hexdigest(),
                        "url": "data:font/ttf;base64," + base64.b64encode(data).decode("ascii"),
                    })
            context.new_cdp_session(page).send("Page.setFontFamilies", {
                "fontFamilies": {
                    "standard": "Liberation Serif", "serif": "Liberation Serif",
                    "sansSerif": "Liberation Sans", "fixed": "Liberation Mono",
                },
            })
        page.goto(args.url, wait_until="load", timeout=30_000)
        if fonts:
            # Explicit post-load static comparison, not evidence for script-time geometry.
            page.evaluate("""async fonts => {
                for (const font of fonts) {
                    const face = new FontFace(font.family, `url(${font.url})`, {
                        weight: font.weight, style: font.style,
                    });
                    document.fonts.add(await face.load());
                }
                await document.fonts.ready;
            }""", fonts)
        page.wait_for_timeout(args.settle_ms)
        page.screenshot(path=str(args.output))
        print(
            json.dumps(
                {
                    "browser": "chromium",
                    "version": browser.version,
                    "viewport": [args.width, args.height],
                    "device_scale_factor": 1,
                    "settle_ms": args.settle_ms,
                    "reference_fonts": [
                        {key: value for key, value in font.items() if key != "url"}
                        for font in fonts
                    ],
                    "font_load_boundary": "post-load-static" if fonts else "system",
                }
            )
        )
        context.close()
        browser.close()


if __name__ == "__main__":
    main()
