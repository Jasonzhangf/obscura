#!/usr/bin/env python3
"""Capture Obscura and Chromium concurrently with the same real settle delay.

Every run uses a new output directory, checks process status and non-empty
screenshots, records browser versions and timings, and reports the raw
full-canvas metrics from check.py. It deliberately emits no aggregate parity
verdict. An optional pre-change Obscura binary can be captured concurrently so
regressions are compared against the same live-page moment.
"""

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
from PIL import Image
from playwright.sync_api import sync_playwright

from check import pair_metrics


def slug(url):
    value = re.sub(r"[^a-z0-9]+", "-", url.lower()).strip("-")
    return value[:80] or "page"


def binary_version(binary):
    result = subprocess.run(
        [binary, "--version"], capture_output=True, text=True, timeout=10
    )
    text = (result.stdout or result.stderr).strip()
    return {"status": result.returncode, "text": text}


def capture_obscura(binary, url, screenshot, log, width, height, settle_ms):
    env = dict(
        os.environ,
        OBSCURA_SHOT_W=str(width),
        OBSCURA_SHOT_H=str(height),
        OBSCURA_ALLOW_PRIVATE_NETWORK="1",
    )
    command = [
        binary,
        "fetch",
        url,
        "--screenshot",
        str(screenshot),
        "--timeout",
        "50000",
        "--wait",
        f"{settle_ms / 1000:g}",
    ]
    started = time.time()
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=75, env=env
        )
        log.write_text(result.stdout + result.stderr)
        ok = result.returncode == 0 and screenshot.is_file() and screenshot.stat().st_size > 0
        return {
            "ok": ok,
            "status": result.returncode,
            "elapsed_s": round(time.time() - started, 3),
        }
    except subprocess.TimeoutExpired as error:
        text = (error.stdout or "") + (error.stderr or "")
        log.write_text(text if isinstance(text, str) else text.decode(errors="replace"))
        return {"ok": False, "status": "timeout", "elapsed_s": round(time.time() - started, 3)}


def load_rgb(path):
    return np.asarray(Image.open(path).convert("RGB"))


def write_results(path, manifest):
    path.write_text(json.dumps(manifest, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("urls", help="one URL per line; # comments allowed")
    parser.add_argument("--obscura-bin", required=True)
    parser.add_argument("--baseline-bin")
    parser.add_argument(
        "--chromium-bin",
        help="Chromium executable (default: Playwright's pinned Chromium)",
    )
    parser.add_argument("--out", required=True, help="must not already exist")
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=1400)
    parser.add_argument("--settle-ms", type=int, default=3000)
    args = parser.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    urls = [
        line.strip()
        for line in Path(args.urls).read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    manifest = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "viewport": {"width": args.width, "height": args.height, "dpr": 1},
        "settle_ms_after_load": args.settle_ms,
        "obscura": binary_version(args.obscura_bin),
        "baseline": binary_version(args.baseline_bin) if args.baseline_bin else None,
        "pages": [],
    }
    results_path = out / "results.json"
    write_results(results_path, manifest)

    with sync_playwright() as playwright:
        chromium_executable = args.chromium_bin or playwright.chromium.executable_path
        manifest["chromium_executable"] = chromium_executable
        browser = playwright.chromium.launch(
            executable_path=chromium_executable,
            headless=True,
            args=[
                "--no-sandbox",
                "--hide-scrollbars",
                "--disable-background-networking",
                "--force-device-scale-factor=1",
            ],
        )
        manifest["chromium_version"] = browser.version
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            for index, url in enumerate(urls):
                name = f"{index:03d}-{slug(url)}"
                ours_path = out / f"{name}.obscura.png"
                chrome_path = out / f"{name}.chrome.png"
                baseline_path = out / f"{name}.baseline.png"
                page_result = {"url": url, "name": name}
                context = browser.new_context(
                    viewport={"width": args.width, "height": args.height},
                    device_scale_factor=1,
                    color_scheme="light",
                    reduced_motion="no-preference",
                    locale="en-US",
                    timezone_id="UTC",
                )
                page = context.new_page()
                chrome_messages = []
                page.on("console", lambda message: chrome_messages.append(f"console {message.type}: {message.text}"))
                page.on("pageerror", lambda error: chrome_messages.append(f"pageerror: {error}"))

                ours_future = executor.submit(
                    capture_obscura,
                    args.obscura_bin,
                    url,
                    ours_path,
                    out / f"{name}.obscura.log",
                    args.width,
                    args.height,
                    args.settle_ms,
                )
                baseline_future = None
                if args.baseline_bin:
                    baseline_future = executor.submit(
                        capture_obscura,
                        args.baseline_bin,
                        url,
                        baseline_path,
                        out / f"{name}.baseline.log",
                        args.width,
                        args.height,
                        args.settle_ms,
                    )

                chrome_started = time.time()
                try:
                    page.goto(url, wait_until="load", timeout=50000)
                    page.wait_for_timeout(args.settle_ms)
                    page.screenshot(
                        path=str(chrome_path), full_page=False, timeout=50000
                    )
                    chrome_ok = chrome_path.is_file() and chrome_path.stat().st_size > 0
                    chrome_status = 0
                except Exception as error:
                    chrome_messages.append(f"capture error: {error}")
                    chrome_ok = False
                    chrome_status = "error"
                (out / f"{name}.chrome.log").write_text("\n".join(chrome_messages) + "\n")
                page_result["chromium"] = {
                    "ok": chrome_ok,
                    "status": chrome_status,
                    "elapsed_s": round(time.time() - chrome_started, 3),
                    "title": page.title() if chrome_ok else None,
                }
                context.close()
                page_result["obscura"] = ours_future.result()
                if baseline_future:
                    page_result["baseline"] = baseline_future.result()

                if chrome_ok and page_result["obscura"]["ok"]:
                    chrome_rgb = load_rgb(chrome_path)
                    current_metrics = pair_metrics(load_rgb(ours_path), chrome_rgb)
                    page_result["metrics"] = current_metrics
                    if baseline_future and page_result["baseline"]["ok"]:
                        baseline_metrics = pair_metrics(load_rgb(baseline_path), chrome_rgb)
                        page_result["baseline_metrics"] = baseline_metrics
                        for key in ("rgb_mae", "pixels_gt_10", "pixels_gt_50"):
                            if key in current_metrics and key in baseline_metrics:
                                page_result.setdefault("delta_vs_baseline", {})[key] = round(
                                    current_metrics[key] - baseline_metrics[key], 6
                                )
                manifest["pages"].append(page_result)
                write_results(results_path, manifest)
                metric = page_result.get("metrics", {}).get("pixels_gt_50")
                delta = page_result.get("delta_vs_baseline", {}).get("pixels_gt_50")
                print(
                    f"{name:84} p>50={metric if metric is not None else 'capture-fail'} "
                    f"delta={delta if delta is not None else '-'}",
                    flush=True,
                )
        browser.close()

    manifest["finished_utc"] = datetime.now(timezone.utc).isoformat()
    write_results(results_path, manifest)
    failed = [
        page["name"]
        for page in manifest["pages"]
        if not page.get("chromium", {}).get("ok") or not page.get("obscura", {}).get("ok")
    ]
    if failed:
        print(f"capture failures: {', '.join(failed)}", file=sys.stderr)
        raise SystemExit(1)
    print(f"paired captures and raw diagnostics: {results_path}")


if __name__ == "__main__":
    main()
