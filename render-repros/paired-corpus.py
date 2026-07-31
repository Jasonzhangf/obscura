#!/usr/bin/env python3
"""Capture Obscura and Chromium concurrently with the same real settle delay.

Every run uses a new output directory, checks process status and non-empty
screenshots, records browser versions and timings, and reports raw full-canvas
pixel diagnostics plus background-tolerant structural-edge diagnostics from
check.py. Browser identity is pinned and both engines record DOM/text
fingerprints, viewport geometry, and JS-visible resource-readiness state at
capture time. Repeatable `--geometry-selector` probes can additionally retain
bounded, viewport-relative element rects from that same pre-screenshot sample.
It deliberately emits no aggregate parity verdict. An optional pre-change
Obscura binary can be captured concurrently so regressions are compared
against the same live-page moment.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.parse
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
from PIL import Image
from playwright.sync_api import sync_playwright

from check import pair_metrics


CANONICAL_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/143.0.0.0 Safari/537.36"
)
CANONICAL_PLATFORM = "Win32"
CANONICAL_UA_PLATFORM = "Windows"
CANONICAL_UA_PLATFORM_VERSION = "10.0.0"
CANONICAL_OBSCURA_PROFILE = 0
CANONICAL_COLOR_SCHEME = "light"
# Obscura currently models the default motion preference, not `reduce`.
CANONICAL_REDUCED_MOTION = "no-preference"
EXPECTED_MEDIA_MATCHES = {
    "prefers_color_scheme_light": True,
    "prefers_color_scheme_dark": False,
    "prefers_reduced_motion_no_preference": True,
    "prefers_reduced_motion_reduce": False,
}
GREASE_CHARS = [" ", "(", ":", "-", ".", "/", ")", ";", "=", "?", "_"]
GREASE_VERSIONS = ["8", "99", "24"]
BRAND_PERMUTATIONS = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
]
GEOMETRY_PROBE_RECT_LIMIT = 200


def geometry_probe_javascript(selectors_expression):
    """Return bounded, per-selector geometry sampling JavaScript."""
    return (
        "const sampleGeometrySelector=selector=>{try{"
        "const elements=Array.from(document.querySelectorAll(selector));"
        "const rects=elements.slice(0,"
        f"{GEOMETRY_PROBE_RECT_LIMIT}"
        ").map((element,index)=>{"
        "const rect=element.getBoundingClientRect();"
        "const style=getComputedStyle(element);"
        "const opacity=Number.parseFloat(style.opacity);"
        "const clientRectCount=element.getClientRects().length;"
        "return {index:index,x:rect.left,y:rect.top,"
        "width:rect.width,height:rect.height,"
        "visible:clientRectCount>0&&rect.width>0&&rect.height>0"
        "&&style.display!=='none'&&style.visibility!=='hidden'"
        "&&style.visibility!=='collapse'"
        "&&(!Number.isFinite(opacity)||opacity>0),"
        "client_rect_count:clientRectCount};"
        "});"
        "return {selector:selector,valid:true,count:elements.length,"
        "coordinate_space:'viewport-css-px',"
        f"rect_limit:{GEOMETRY_PROBE_RECT_LIMIT},"
        f"rects_truncated:elements.length>{GEOMETRY_PROBE_RECT_LIMIT},"
        "rects:rects,error:null};"
        "}catch(error){return {selector:selector,valid:false,count:null,"
        "coordinate_space:'viewport-css-px',"
        f"rect_limit:{GEOMETRY_PROBE_RECT_LIMIT},"
        "rects_truncated:false,rects:[],"
        "error:{name:error&&error.name?String(error.name):'Error',"
        "message:error&&error.message?String(error.message):String(error)}};}};"
        f"const geometryProbes={selectors_expression}.map(sampleGeometrySelector);"
    )


def slug(url):
    value = re.sub(r"[^a-z0-9]+", "-", url.lower()).strip("-")
    return value[:80] or "page"


def binary_version(binary):
    result = subprocess.run(
        [binary, "--version"], capture_output=True, text=True, timeout=10
    )
    text = (result.stdout or result.stderr).strip()
    return {"status": result.returncode, "text": text}


def diagnostic_text(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return value


def media_matches_configured(media):
    return all(media.get(key) is expected for key, expected in EXPECTED_MEDIA_MATCHES.items())


def parse_scroll_y(value):
    if value == "bottom":
        return value
    try:
        return int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "--scroll-y must be an integer CSS-pixel offset or 'bottom'"
        ) from error


def scroll_eval_expression(scroll):
    """Compatibility wrapper used by older callers and focused tests."""
    return obscura_state_eval_expression(scroll)


def obscura_state_eval_expression(scroll, geometry_selectors=None):
    """Sample the live page after an optional scroll and immediately before paint."""
    scroll_script = ""
    requested = "null"
    if scroll is not None:
        scroll_x, scroll_y = scroll
        requested_y = (
            "document.documentElement.scrollHeight"
            if scroll_y == "bottom"
            else str(scroll_y)
        )
        scroll_script = (
            f"const requestedX={scroll_x},requestedY={requested_y};"
            "window.scrollTo(requestedX,requestedY);"
        )
        requested = "{x:requestedX,y:requestedY}"
    geometry_setup = ""
    geometry_result = ""
    if geometry_selectors:
        selectors_json = json.dumps(
            list(geometry_selectors), ensure_ascii=True, separators=(",", ":")
        )
        geometry_setup = geometry_probe_javascript(selectors_json)
        geometry_result = "geometry_probes:geometryProbes,"
    return (
        "(()=>{"
        + scroll_script
        + "const root=document.documentElement,body=document.body;"
        "const dom=root?root.outerHTML:'';"
        "const injectedStyles=root?Array.from(root.querySelectorAll("
        "'style[data-obscura-external-stylesheets],style[data-obscura-linked]'"
        ")):[];"
        "const normalizedDom=injectedStyles.reduce((html,node)=>"
        "typeof node.outerHTML==='string'?html.replace(node.outerHTML,''):html,dom);"
        "const text=body?body.innerText.replace(/\\s+/g,' ').trim():'';"
        "const images=Array.from(document.images||[]),fonts=document.fonts;"
        "const hash=value=>{let h=2166136261;"
        "for(let i=0;i<value.length;i++){h^=value.charCodeAt(i);"
        "h=Math.imul(h,16777619)}"
        "return ('00000000'+(h>>>0).toString(16)).slice(-8)};"
        + geometry_setup
        + "return JSON.stringify({"
        "sampled_phase:'immediately-before-screenshot',"
        + geometry_result
        + f"requested:{requested},"
        "url:location.href,"
        "document:{ready_state:document.readyState,"
        "element_count:document.getElementsByTagName('*').length,"
        "outer_html_utf16:dom.length,outer_html_fnv1a32:hash(dom),"
        "normalized_outer_html_utf16:normalizedDom.length,"
        "normalized_outer_html_fnv1a32:hash(normalizedDom),"
        "visible_text_utf16:text.length,visible_text_fnv1a32:hash(text)},"
        "geometry:{inner_width:innerWidth,inner_height:innerHeight,"
        "scroll_x:scrollX,scroll_y:scrollY,"
        "document_client_width:root?root.clientWidth:null,"
        "document_client_height:root?root.clientHeight:null,"
        "document_scroll_width:root?root.scrollWidth:null,"
        "document_scroll_height:root?root.scrollHeight:null,"
        "body_client_width:body?body.clientWidth:null,"
        "body_client_height:body?body.clientHeight:null,"
        "body_scroll_width:body?body.scrollWidth:null,"
        "body_scroll_height:body?body.scrollHeight:null},"
        "fonts:{supported:!!fonts,status:fonts?fonts.status:null,"
        "face_count:fonts?Array.from(fonts).length:null,"
        "ready_at_sample:fonts?fonts.status==='loaded':null},"
        "images:{total:images.length,"
        "complete:images.filter(image=>image.complete).length,"
        "complete_with_pixels:images.filter(image=>image.complete&&image.naturalWidth>0).length,"
        "complete_without_pixels:images.filter(image=>image.complete&&image.naturalWidth===0).length,"
        "pending:images.filter(image=>!image.complete).length,"
        "lazy:images.filter(image=>image.loading==='lazy').length},"
        "media:{"
        "prefers_color_scheme_light:matchMedia('(prefers-color-scheme: light)').matches,"
        "prefers_color_scheme_dark:matchMedia('(prefers-color-scheme: dark)').matches,"
        "prefers_reduced_motion_no_preference:matchMedia('(prefers-reduced-motion: no-preference)').matches,"
        "prefers_reduced_motion_reduce:matchMedia('(prefers-reduced-motion: reduce)').matches}"
        "})"
        "})()"
    )


def parse_obscura_capture_report(stdout):
    """Parse the CLI's evaluation plus authoritative screenshot capture state."""
    for line in reversed(stdout.splitlines()):
        try:
            report = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(report, dict) or not isinstance(
            report.get("captureState"), dict
        ):
            continue
        evaluated = report.get("evaluation")
        if isinstance(evaluated, str):
            try:
                evaluated = json.loads(evaluated)
            except json.JSONDecodeError:
                evaluated = None
        if not isinstance(evaluated, dict):
            continue
        state = dict(evaluated)
        geometry = dict(state.get("geometry") or {})
        capture = report["captureState"]
        # These values come from the exact prepared render used by screenshot,
        # so they take precedence over the JS sample if the two ever diverge.
        geometry.update(
            {
                "inner_width": capture.get("innerWidth"),
                "inner_height": capture.get("innerHeight"),
                "scroll_x": capture.get("scrollX"),
                "scroll_y": capture.get("scrollY"),
                "document_scroll_width": capture.get("scrollWidth"),
                "document_scroll_height": capture.get("scrollHeight"),
            }
        )
        state["geometry"] = geometry
        state["sampled_phase"] = "immediately-before-screenshot"
        return state
    return None


def parse_obscura_scroll_report(stdout):
    state = parse_obscura_capture_report(stdout)
    if state is None:
        return None
    geometry = state.get("geometry") or {}
    return {
        "requested": state.get("requested"),
        "actual": {"x": geometry.get("scroll_x"), "y": geometry.get("scroll_y")},
        "viewport": {
            "width": geometry.get("inner_width"),
            "height": geometry.get("inner_height"),
        },
        "content": {
            "width": geometry.get("document_scroll_width"),
            "height": geometry.get("document_scroll_height"),
        },
        "sampled_phase": state["sampled_phase"],
    }


def obscura_environment(width, height):
    env = dict(
        os.environ,
        OBSCURA_SHOT_W=str(width),
        OBSCURA_SHOT_H=str(height),
        OBSCURA_ALLOW_PRIVATE_NETWORK="1",
        # Product fetches return early when the event loop becomes idle. Paired
        # captures instead need the same complete post-load wall interval that
        # Playwright's wait_for_timeout below uses.
        OBSCURA_STRICT_SETTLE="1",
        # Match Playwright's 50-second goto allowance. This is the browser
        # engine's millisecond ceiling, distinct from the CLI's seconds unit.
        OBSCURA_NAV_TIMEOUT_MS="50000",
        # Pin the navigator platform/profile as well as the explicit UA. A
        # randomized platform changes responsive content and font selection,
        # making a renderer comparison answer the wrong question.
        OBSCURA_PROFILE=str(CANONICAL_OBSCURA_PROFILE),
    )
    return env


def probe_obscura_identity(binary):
    expression = (
        "JSON.stringify({userAgent:navigator.userAgent,"
        "platform:navigator.platform,"
        "uaPlatform:navigator.userAgentData&&navigator.userAgentData.platform,"
        "uaBrands:navigator.userAgentData&&navigator.userAgentData.brands,"
        "media:{"
        "prefers_color_scheme_light:matchMedia('(prefers-color-scheme: light)').matches,"
        "prefers_color_scheme_dark:matchMedia('(prefers-color-scheme: dark)').matches,"
        "prefers_reduced_motion_no_preference:matchMedia('(prefers-reduced-motion: no-preference)').matches,"
        "prefers_reduced_motion_reduce:matchMedia('(prefers-reduced-motion: reduce)').matches"
        "}})"
    )
    command = [
        binary,
        "fetch",
        "data:text/html,<title>identity-probe</title>",
        "--user-agent",
        CANONICAL_USER_AGENT,
        "--eval",
        expression,
        "--timeout",
        "5",
        "--wait",
        "0",
        "--quiet",
    ]
    env = obscura_environment(1, 1)
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=15, env=env
        )
        raw = result.stdout.strip()
        effective = json.loads(raw) if result.returncode == 0 else None
        return {
            "ok": result.returncode == 0,
            "status": result.returncode,
            "effective": effective,
            "media_matches_configured": (
                media_matches_configured(effective.get("media", {}))
                if effective
                else False
            ),
            "diagnostic": result.stderr.strip() or None,
        }
    except (subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        return {"ok": False, "status": "probe-error", "diagnostic": str(error)}


def probe_obscura_css_media(binary):
    """Verify the renderer's CSS media evaluator, not only JS matchMedia."""
    html = """<!doctype html><style>
      html,body{margin:0}
      #scheme,#motion{position:fixed;top:0;width:4px;height:4px;background:#ff00ff}
      #scheme{left:0} #motion{left:4px}
      @media (prefers-color-scheme:light){#scheme{background:#00ff00}}
      @media (prefers-color-scheme:dark){#scheme{background:#ff0000}}
      @media (prefers-reduced-motion:no-preference){#motion{background:#0000ff}}
      @media (prefers-reduced-motion:reduce){#motion{background:#ffff00}}
      </style><div id=scheme></div><div id=motion></div>"""
    url = "data:text/html," + urllib.parse.quote(html, safe="")
    with tempfile.TemporaryDirectory(prefix="obscura-media-probe-") as directory:
        screenshot = Path(directory) / "probe.png"
        command = [
            binary,
            "fetch",
            url,
            "--user-agent",
            CANONICAL_USER_AGENT,
            "--screenshot",
            str(screenshot),
            "--timeout",
            "5",
            "--wait",
            "0",
            "--quiet",
        ]
        try:
            result = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=15,
                env=obscura_environment(8, 4),
            )
            if result.returncode != 0 or not screenshot.is_file():
                return {
                    "ok": False,
                    "status": result.returncode,
                    "diagnostic": result.stderr.strip() or "missing probe screenshot",
                }
            image = Image.open(screenshot).convert("RGB")
            scheme = list(image.getpixel((1, 1)))
            motion = list(image.getpixel((5, 1)))
            expected = {"color_scheme": [0, 255, 0], "reduced_motion": [0, 0, 255]}
            actual = {"color_scheme": scheme, "reduced_motion": motion}
            return {
                "ok": actual == expected,
                "status": result.returncode,
                "configured": {
                    "color_scheme": CANONICAL_COLOR_SCHEME,
                    "reduced_motion": CANONICAL_REDUCED_MOTION,
                },
                "expected_rgb": expected,
                "actual_rgb": actual,
                "diagnostic": None if actual == expected else "CSS media probe colors differ",
            }
        except (subprocess.TimeoutExpired, OSError) as error:
            return {"ok": False, "status": "probe-error", "diagnostic": str(error)}


def capture_obscura(
    binary,
    url,
    screenshot,
    log,
    width,
    height,
    settle_ms,
    scroll=None,
    geometry_selectors=None,
):
    env = obscura_environment(width, height)
    command = [
        binary,
        "fetch",
        url,
        "--user-agent",
        CANONICAL_USER_AGENT,
        "--screenshot",
        str(screenshot),
        "--timeout",
        "50",
        "--wait",
        f"{settle_ms / 1000:g}",
    ]
    state_expression = obscura_state_eval_expression(scroll, geometry_selectors)
    command.extend(["--eval", state_expression])
    started = time.time()
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=75, env=env
        )
        log.write_text(result.stdout + result.stderr)
        state = (
            parse_obscura_capture_report(result.stdout)
            if result.returncode == 0
            else None
        )
        if state is not None:
            media = state.get("media") or {}
            media["matches_configured"] = media_matches_configured(media)
            state["media"] = media
        scroll_state = (
            parse_obscura_scroll_report(result.stdout)
            if scroll is not None and state is not None
            else None
        )
        ok = (
            result.returncode == 0
            and screenshot.is_file()
            and screenshot.stat().st_size > 0
            and state is not None
            and state["media"]["matches_configured"]
            and (scroll is None or scroll_state is not None)
        )
        return {
            "ok": ok,
            "status": result.returncode,
            "elapsed_s": round(time.time() - started, 3),
            "state": state,
            "scroll_state": scroll_state,
        }
    except subprocess.TimeoutExpired as error:
        log.write_text(
            diagnostic_text(error.stdout) + diagnostic_text(error.stderr)
        )
        return {"ok": False, "status": "timeout", "elapsed_s": round(time.time() - started, 3)}


def chromium_identity_override(session):
    """Keep request headers and navigator identity aligned with Obscura."""
    match = re.search(r"Chrome/(\d+)", CANONICAL_USER_AGENT)
    major = int(match.group(1)) if match else 143
    grease = {
        "brand": (
            "Not"
            + GREASE_CHARS[major % len(GREASE_CHARS)]
            + "A"
            + GREASE_CHARS[(major + 1) % len(GREASE_CHARS)]
            + "Brand"
        ),
        "version": GREASE_VERSIONS[major % len(GREASE_VERSIONS)],
    }
    unordered = [
        grease,
        {"brand": "Chromium", "version": str(major)},
        {"brand": "Google Chrome", "version": str(major)},
    ]
    permutation = BRAND_PERMUTATIONS[major % len(BRAND_PERMUTATIONS)]
    brands = [unordered[index] for index in permutation]
    session.send(
        "Emulation.setUserAgentOverride",
        {
            "userAgent": CANONICAL_USER_AGENT,
            "acceptLanguage": "en-US,en",
            "platform": CANONICAL_PLATFORM,
            "userAgentMetadata": {
                "brands": brands,
                "fullVersionList": [
                    {
                        "brand": brand["brand"],
                        "version": brand["version"] + ".0.0.0",
                    }
                    for brand in brands
                ],
                "platform": CANONICAL_UA_PLATFORM,
                "platformVersion": CANONICAL_UA_PLATFORM_VERSION,
                "architecture": "x86",
                "model": "",
                "mobile": False,
                "bitness": "64",
                "wow64": False,
            },
        },
    )


def capture_chromium_state(page, geometry_selectors=None):
    """Sample page provenance immediately before the screenshot."""
    expression = """async () => {
          async function sha256(value) {
            if (!globalThis.crypto || !crypto.subtle) return null;
            const bytes = new TextEncoder().encode(value);
            const digest = await crypto.subtle.digest("SHA-256", bytes);
            return Array.from(new Uint8Array(digest), byte =>
              byte.toString(16).padStart(2, "0")).join("");
          }
          function fnv1a32(value) {
            let hash = 2166136261;
            for (let index = 0; index < value.length; index++) {
              hash ^= value.charCodeAt(index);
              hash = Math.imul(hash, 16777619);
            }
            return (hash >>> 0).toString(16).padStart(8, "0");
          }

          const root = document.documentElement;
          const body = document.body;
          const dom = root ? root.outerHTML : "";
          const injectedStyles = root ? Array.from(root.querySelectorAll(
            "style[data-obscura-external-stylesheets],style[data-obscura-linked]"
          )) : [];
          const normalizedDom = injectedStyles.reduce(
            (html, node) => typeof node.outerHTML === "string"
              ? html.replace(node.outerHTML, "")
              : html,
            dom
          );
          const visibleText = body ? body.innerText : "";
          const normalizedText = visibleText.replace(/\\s+/g, " ").trim();
          const images = Array.from(document.images || []);
          const fonts = document.fonts;
          const media = {
            prefers_color_scheme_light:
              matchMedia("(prefers-color-scheme: light)").matches,
            prefers_color_scheme_dark:
              matchMedia("(prefers-color-scheme: dark)").matches,
            prefers_reduced_motion_no_preference:
              matchMedia("(prefers-reduced-motion: no-preference)").matches,
            prefers_reduced_motion_reduce:
              matchMedia("(prefers-reduced-motion: reduce)").matches
          };
          return {
            sampled_phase: "immediately-before-screenshot",
            url: location.href,
            identity: {
              user_agent: navigator.userAgent,
              platform: navigator.platform,
              ua_platform: navigator.userAgentData
                ? navigator.userAgentData.platform
                : null,
              ua_brands: navigator.userAgentData
                ? Array.from(navigator.userAgentData.brands)
                : null,
              language: navigator.language,
              languages: Array.from(navigator.languages || []),
            },
            document: {
              ready_state: document.readyState,
              element_count: document.getElementsByTagName("*").length,
              outer_html_utf16: dom.length,
              outer_html_fnv1a32: fnv1a32(dom),
              outer_html_bytes: new TextEncoder().encode(dom).length,
              outer_html_sha256: await sha256(dom),
              normalized_outer_html_utf16: normalizedDom.length,
              normalized_outer_html_fnv1a32: fnv1a32(normalizedDom),
              normalized_outer_html_bytes:
                new TextEncoder().encode(normalizedDom).length,
              normalized_outer_html_sha256: await sha256(normalizedDom),
              visible_text_utf16: normalizedText.length,
              visible_text_fnv1a32: fnv1a32(normalizedText),
              visible_text_bytes: new TextEncoder().encode(normalizedText).length,
              visible_text_sha256: await sha256(normalizedText),
            },
            geometry: {
              inner_width: innerWidth,
              inner_height: innerHeight,
              scroll_x: scrollX,
              scroll_y: scrollY,
              device_pixel_ratio: devicePixelRatio,
              document_client_width: root ? root.clientWidth : null,
              document_client_height: root ? root.clientHeight : null,
              document_scroll_width: root ? root.scrollWidth : null,
              document_scroll_height: root ? root.scrollHeight : null,
              body_client_width: body ? body.clientWidth : null,
              body_client_height: body ? body.clientHeight : null,
              body_scroll_width: body ? body.scrollWidth : null,
              body_scroll_height: body ? body.scrollHeight : null,
              visual_viewport: visualViewport ? {
                width: visualViewport.width,
                height: visualViewport.height,
                scale: visualViewport.scale,
                offset_left: visualViewport.offsetLeft,
                offset_top: visualViewport.offsetTop
              } : null
            },
            fonts: {
              supported: !!fonts,
              status: fonts ? fonts.status : null,
              face_count: fonts ? Array.from(fonts).length : null,
              ready_at_sample: fonts ? fonts.status === "loaded" : null
            },
            images: {
              total: images.length,
              complete: images.filter(image => image.complete).length,
              complete_with_pixels: images.filter(image =>
                image.complete && image.naturalWidth > 0).length,
              complete_without_pixels: images.filter(image =>
                image.complete && image.naturalWidth === 0).length,
              pending: images.filter(image => !image.complete).length,
              lazy: images.filter(image => image.loading === "lazy").length
            },
            media: {
              ...media,
              root_computed_color_scheme: root
                ? getComputedStyle(root).colorScheme
                : null,
              root_class: root ? root.className : null,
              root_data_theme: root ? root.getAttribute("data-theme") : null,
              body_class: body ? body.className : null,
              body_data_theme: body ? body.getAttribute("data-theme") : null
            }
          };
        }"""
    if geometry_selectors:
        expression = expression.replace(
            "async () => {", "async geometrySelectors => {", 1
        )
        expression = expression.replace(
            "          const root = document.documentElement;",
            geometry_probe_javascript("geometrySelectors")
            + "          const root = document.documentElement;",
            1,
        )
        expression = expression.replace(
            '          return {\n            sampled_phase: "immediately-before-screenshot",',
            '          return {\n            geometry_probes: geometryProbes,\n'
            '            sampled_phase: "immediately-before-screenshot",',
            1,
        )
        state = page.evaluate(expression, list(geometry_selectors))
    else:
        state = page.evaluate(expression)
    document_state = state["document"]
    if (
        document_state["outer_html_sha256"] is None
        or document_state["visible_text_sha256"] is None
    ):
        # crypto.subtle is unavailable on some non-secure/file origins. Only
        # transfer the potentially large strings on that fallback path.
        fallback = page.evaluate(
            """() => {
              const dom = document.documentElement
                ? document.documentElement.outerHTML
                : "";
              const text = document.body
                ? document.body.innerText.replace(/\\s+/g, " ").trim()
                : "";
              return {dom, text};
            }"""
        )
        document_state["outer_html_sha256"] = hashlib.sha256(
            fallback["dom"].encode()
        ).hexdigest()
        document_state["visible_text_sha256"] = hashlib.sha256(
            fallback["text"].encode()
        ).hexdigest()
    return state


def freeze_chromium_animations(page, sample_ms):
    """Pause currently exposed Web Animations at one explicit local time.

    This is opt-in: live wall-clock captures remain useful for runtime
    failures, while deterministic CSS-animation comparisons need Chromium
    sampled at the same T=0 used by the static renderer.
    """
    return page.evaluate(
        """sampleMs => {
          if (typeof document.getAnimations !== "function") {
            return {
              supported: false,
              requested_ms: sampleMs,
              discovered: 0,
              frozen: 0,
              failures: []
            };
          }
          const animations = document.getAnimations();
          const failures = [];
          let frozen = 0;
          for (let index = 0; index < animations.length; index++) {
            const animation = animations[index];
            try {
              animation.pause();
              animation.currentTime = sampleMs;
              frozen++;
            } catch (error) {
              failures.push({
                index,
                name: error && error.name ? String(error.name) : "Error",
                message: error && error.message
                  ? String(error.message)
                  : String(error)
              });
            }
          }
          if (document.documentElement) {
            void document.documentElement.getBoundingClientRect().width;
          }
          return {
            supported: true,
            requested_ms: sampleMs,
            discovered: animations.length,
            frozen,
            failures
          };
        }""",
        sample_ms,
    )


def load_rgb(path):
    return np.asarray(Image.open(path).convert("RGB"))


def write_results(path, manifest):
    path.write_text(json.dumps(manifest, indent=2) + "\n")


def compare_page_states(obscura, chromium):
    """Return explicit same-page and geometry deltas; never infer a parity verdict."""
    obscura_document = (obscura or {}).get("document") or {}
    chromium_document = (chromium or {}).get("document") or {}
    obscura_geometry = (obscura or {}).get("geometry") or {}
    chromium_geometry = (chromium or {}).get("geometry") or {}

    def delta(left, right):
        if isinstance(left, (int, float)) and isinstance(right, (int, float)):
            return left - right
        return None

    geometry_fields = (
        "inner_width",
        "inner_height",
        "scroll_x",
        "scroll_y",
        "document_client_width",
        "document_client_height",
        "document_scroll_width",
        "document_scroll_height",
        "body_client_width",
        "body_client_height",
        "body_scroll_width",
        "body_scroll_height",
    )
    return {
        "url_equal": (obscura or {}).get("url") == (chromium or {}).get("url"),
        "ready_state_equal": (
            obscura_document.get("ready_state")
            == chromium_document.get("ready_state")
        ),
        "element_count_delta": delta(
            obscura_document.get("element_count"),
            chromium_document.get("element_count"),
        ),
        "outer_html_utf16_delta": delta(
            obscura_document.get("outer_html_utf16"),
            chromium_document.get("outer_html_utf16"),
        ),
        "visible_text_utf16_delta": delta(
            obscura_document.get("visible_text_utf16"),
            chromium_document.get("visible_text_utf16"),
        ),
        "outer_html_fingerprint_equal": (
            obscura_document.get("outer_html_fnv1a32") is not None
            and obscura_document.get("outer_html_fnv1a32")
            == chromium_document.get("outer_html_fnv1a32")
        ),
        "normalized_outer_html_utf16_delta": delta(
            obscura_document.get("normalized_outer_html_utf16"),
            chromium_document.get("normalized_outer_html_utf16"),
        ),
        "normalized_outer_html_fingerprint_equal": (
            obscura_document.get("normalized_outer_html_fnv1a32") is not None
            and obscura_document.get("normalized_outer_html_fnv1a32")
            == chromium_document.get("normalized_outer_html_fnv1a32")
        ),
        "visible_text_fingerprint_equal": (
            obscura_document.get("visible_text_fnv1a32") is not None
            and obscura_document.get("visible_text_fnv1a32")
            == chromium_document.get("visible_text_fnv1a32")
        ),
        "geometry_delta": {
            field: delta(
                obscura_geometry.get(field),
                chromium_geometry.get(field),
            )
            for field in geometry_fields
        },
    }


def compare_geometry_probes(obscura, chromium):
    """Report raw per-selector deltas without reducing them to a verdict."""
    obscura_probes = (obscura or {}).get("geometry_probes") or []
    chromium_probes = (chromium or {}).get("geometry_probes") or []
    comparisons = []
    for index in range(max(len(obscura_probes), len(chromium_probes))):
        obscura_probe = (
            obscura_probes[index] if index < len(obscura_probes) else None
        )
        chromium_probe = (
            chromium_probes[index] if index < len(chromium_probes) else None
        )
        obscura_rects = (obscura_probe or {}).get("rects") or []
        chromium_rects = (chromium_probe or {}).get("rects") or []
        rect_deltas = []
        for rect_index in range(min(len(obscura_rects), len(chromium_rects))):
            obscura_rect = obscura_rects[rect_index]
            chromium_rect = chromium_rects[rect_index]
            deltas = {}
            for field in ("x", "y", "width", "height"):
                left = obscura_rect.get(field)
                right = chromium_rect.get(field)
                deltas[field] = (
                    left - right
                    if isinstance(left, (int, float))
                    and isinstance(right, (int, float))
                    else None
                )
            rect_deltas.append(
                {
                    "index": rect_index,
                    "delta": deltas,
                    "visibility": {
                        "obscura": obscura_rect.get("visible"),
                        "chromium": chromium_rect.get("visible"),
                    },
                }
            )
        obscura_count = (obscura_probe or {}).get("count")
        chromium_count = (chromium_probe or {}).get("count")
        comparisons.append(
            {
                "index": index,
                "selector": (
                    (obscura_probe or {}).get("selector")
                    if obscura_probe is not None
                    else (chromium_probe or {}).get("selector")
                ),
                "valid": {
                    "obscura": (obscura_probe or {}).get("valid"),
                    "chromium": (chromium_probe or {}).get("valid"),
                },
                "errors": {
                    "obscura": (obscura_probe or {}).get("error"),
                    "chromium": (chromium_probe or {}).get("error"),
                },
                "counts": {
                    "obscura": obscura_count,
                    "chromium": chromium_count,
                    "delta": (
                        obscura_count - chromium_count
                        if isinstance(obscura_count, int)
                        and isinstance(chromium_count, int)
                        else None
                    ),
                },
                "rects_compared": len(rect_deltas),
                "rect_deltas": rect_deltas,
            }
        )
    return comparisons


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
    parser.add_argument(
        "--animation-time-ms",
        type=int,
        choices=[0],
        help=(
            "pause Chromium animations currently exposed through Web Animations "
            "at T=0 immediately before state sampling and screenshot paint; "
            "matches Obscura's deterministic static animation sample"
        ),
    )
    parser.add_argument(
        "--geometry-selector",
        action="append",
        default=[],
        metavar="CSS_SELECTOR",
        help=(
            "repeatable selector whose match count and viewport-relative "
            "bounding rects are sampled in both engines immediately before capture"
        ),
    )
    parser.add_argument(
        "--scroll-x",
        type=int,
        help="scroll both engines to this CSS-pixel x offset before capture",
    )
    parser.add_argument(
        "--scroll-y",
        type=parse_scroll_y,
        help="scroll both engines to this CSS-pixel y offset or 'bottom' before capture",
    )
    args = parser.parse_args()
    if args.settle_ms % 1000:
        parser.error(
            "--settle-ms must be a whole number of seconds because Obscura's "
            "fetch --wait interface accepts integer seconds"
        )

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    controlled_scroll = None
    if args.scroll_x is not None or args.scroll_y is not None:
        controlled_scroll = (
            args.scroll_x if args.scroll_x is not None else 0,
            args.scroll_y if args.scroll_y is not None else 0,
        )
    urls = [
        line.strip()
        for line in Path(args.urls).read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    manifest = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "viewport": {"width": args.width, "height": args.height, "dpr": 1},
        "settle_ms_after_load": args.settle_ms,
        "settle_ms_after_controlled_scroll": (
            args.settle_ms if controlled_scroll is not None else 0
        ),
        "settle_semantics": (
            "full wall-clock interval while pumping each engine; when a "
            "controlled scroll is requested, the same interval runs once "
            "after load and once after scrolling"
        ),
        "animation_sampling": (
            {
                "mode": "deterministic-active-web-animations",
                "sample_ms": args.animation_time_ms,
                "chromium": (
                    "document.getAnimations() results are paused and assigned "
                    "currentTime immediately before state and screenshot capture"
                ),
                "obscura": "static renderer animation sample time T=0",
            }
            if args.animation_time_ms is not None
            else {"mode": "live-wall-clock", "sample_ms": None}
        ),
        "controlled_scroll": (
            {"x": controlled_scroll[0], "y": controlled_scroll[1]}
            if controlled_scroll is not None
            else None
        ),
        "navigation_timeout_ms": 50000,
        "obscura": binary_version(args.obscura_bin),
        "baseline": binary_version(args.baseline_bin) if args.baseline_bin else None,
        "capture_identity": {
            "normalized": True,
            "configured_user_agent": CANONICAL_USER_AGENT,
            "configured_platform": CANONICAL_PLATFORM,
            "configured_ua_platform": CANONICAL_UA_PLATFORM,
            "configured_ua_platform_version": CANONICAL_UA_PLATFORM_VERSION,
            "obscura_profile": CANONICAL_OBSCURA_PROFILE,
        },
        "capture_media": {
            "normalized": True,
            "color_scheme": CANONICAL_COLOR_SCHEME,
            "reduced_motion": CANONICAL_REDUCED_MOTION,
            "expected_match_media": EXPECTED_MEDIA_MATCHES,
            "reduced_motion_reason": (
                "Obscura currently models the browser default "
                "(no-preference), so Chromium is pinned to the same state"
            ),
        },
        "state_observability": {
            "chromium": "same page, sampled immediately before screenshot",
            "obscura": (
                "same live page sampled immediately before screenshot; the CLI "
                "captureState records the exact shared PreparedRender viewport, "
                "scroll offset, and content size used by paint"
            ),
        },
        "methodology_limits": {
            "pixel_metrics": (
                "raw full-canvas diagnostics only; they are a tripwire, not a "
                "fidelity verdict"
            ),
            "controlled_scroll": (
                "CSSOM and screenshot paint share one resource-aware "
                "PreparedRender. Content-size and scroll deltas are exact for "
                "each captured engine, but different DOM/resource states can "
                "still make semantic bottom landmarks differ."
            ),
            "page_state": (
                "DOM/text fingerprints and length deltas expose different live "
                "page states. They are provenance tripwires, not proof that "
                "equal states contain equal layout or that unequal serialized "
                "DOM necessarily represents a rendering failure. Normalized DOM "
                "fingerprints exclude only Obscura's explicitly marked external-"
                "stylesheet mirror nodes, because Chromium's CSSOM does not "
                "serialize fetched stylesheet text into outerHTML."
            ),
            "resource_readiness": (
                "Obscura's DOM image/font readiness is sampled before paint; "
                "paint may then fetch retained renderer resources that are not "
                "reflected back into HTMLImageElement or FontFaceSet state."
            ),
            "animation_sampling": (
                "The deterministic option controls only animations still "
                "exposed by document.getAnimations() at capture time. Finished "
                "fill-none animations and script-driven visual state that has "
                "already been discarded cannot be rewound by this harness."
            ),
        },
        "pages": [],
    }
    if args.geometry_selector:
        manifest["geometry_probes"] = {
            "selectors": args.geometry_selector,
            "coordinate_space": "viewport-css-px",
            "rect_limit_per_selector": GEOMETRY_PROBE_RECT_LIMIT,
            "visibility": (
                "practical rendered-box heuristic: client rect exists, positive "
                "bounding size, display/visibility permit paint, and opacity > 0"
            ),
            "comparison_semantics": (
                "raw per-selector counts, rect deltas in document order, and "
                "visibility observations; no aggregate parity verdict"
            ),
        }
    manifest["obscura_identity_probe"] = probe_obscura_identity(args.obscura_bin)
    manifest["obscura_css_media_probe"] = probe_obscura_css_media(args.obscura_bin)
    if args.baseline_bin:
        manifest["baseline_identity_probe"] = probe_obscura_identity(args.baseline_bin)
        manifest["baseline_css_media_probe"] = probe_obscura_css_media(
            args.baseline_bin
        )
    results_path = out / "results.json"
    write_results(results_path, manifest)
    probes = [
        ("obscura JS media", manifest["obscura_identity_probe"].get("media_matches_configured")),
        ("obscura CSS media", manifest["obscura_css_media_probe"].get("ok")),
    ]
    if args.baseline_bin:
        probes.extend(
            [
                (
                    "baseline JS media",
                    manifest["baseline_identity_probe"].get("media_matches_configured"),
                ),
                (
                    "baseline CSS media",
                    manifest["baseline_css_media_probe"].get("ok"),
                ),
            ]
        )
    failed_probes = [name for name, passed in probes if not passed]
    if failed_probes:
        print(
            "capture environment mismatch: " + ", ".join(failed_probes),
            file=sys.stderr,
        )
        print(f"probe evidence: {results_path}", file=sys.stderr)
        raise SystemExit(2)

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
                    user_agent=CANONICAL_USER_AGENT,
                    color_scheme=CANONICAL_COLOR_SCHEME,
                    reduced_motion=CANONICAL_REDUCED_MOTION,
                    locale="en-US",
                    timezone_id="UTC",
                )
                page = context.new_page()
                # Repeat the context-level settings at page scope so a future
                # context refactor cannot silently fall back to host media.
                page.emulate_media(
                    color_scheme=CANONICAL_COLOR_SCHEME,
                    reduced_motion=CANONICAL_REDUCED_MOTION,
                )
                chromium_identity_override(context.new_cdp_session(page))
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
                    controlled_scroll,
                    args.geometry_selector,
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
                        controlled_scroll,
                        args.geometry_selector,
                    )

                chrome_started = time.time()
                try:
                    page.goto(url, wait_until="load", timeout=50000)
                    page.wait_for_timeout(args.settle_ms)
                    if controlled_scroll is not None:
                        scroll_x, scroll_y = controlled_scroll
                        page.evaluate(
                            """([x, y]) => {
                              const requestedY = y === "bottom"
                                ? document.documentElement.scrollHeight
                                : y;
                              window.scrollTo(x, requestedY);
                            }""",
                            [scroll_x, scroll_y],
                        )
                        page.wait_for_timeout(args.settle_ms)
                    animation_sampling = None
                    if args.animation_time_ms is not None:
                        animation_sampling = freeze_chromium_animations(
                            page, args.animation_time_ms
                        )
                    chromium_state_ok = False
                    try:
                        chromium_state = capture_chromium_state(
                            page, args.geometry_selector
                        )
                        if animation_sampling is not None:
                            chromium_state["animation_sampling"] = (
                                animation_sampling
                            )
                        chromium_state["media"]["matches_configured"] = (
                            media_matches_configured(chromium_state["media"])
                        )
                        if not chromium_state["media"]["matches_configured"]:
                            raise RuntimeError(
                                "Chromium matchMedia differs from configured capture media"
                            )
                        chromium_state_ok = True
                    except Exception as error:
                        chromium_state = None
                        chrome_messages.append(f"state capture error: {error}")
                    page.screenshot(
                        path=str(chrome_path), full_page=False, timeout=50000
                    )
                    chrome_ok = (
                        chromium_state_ok
                        and chrome_path.is_file()
                        and chrome_path.stat().st_size > 0
                    )
                    chrome_status = 0 if chrome_ok else "state-error"
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
                    "state": chromium_state if chrome_ok else None,
                }
                context.close()
                page_result["obscura"] = ours_future.result()
                if baseline_future:
                    page_result["baseline"] = baseline_future.result()

                if chrome_ok and page_result["obscura"]["ok"]:
                    page_result["page_state_comparison"] = compare_page_states(
                        page_result["obscura"].get("state"),
                        chromium_state,
                    )
                    if args.geometry_selector:
                        page_result["geometry_probe_comparison"] = (
                            compare_geometry_probes(
                                page_result["obscura"].get("state"),
                                chromium_state,
                            )
                        )
                if (
                    chrome_ok
                    and baseline_future
                    and page_result["baseline"]["ok"]
                ):
                    page_result["baseline_page_state_comparison"] = (
                        compare_page_states(
                            page_result["baseline"].get("state"),
                            chromium_state,
                        )
                    )
                    if args.geometry_selector:
                        page_result["baseline_geometry_probe_comparison"] = (
                            compare_geometry_probes(
                                page_result["baseline"].get("state"),
                                chromium_state,
                            )
                        )

                if (
                    controlled_scroll is not None
                    and chrome_ok
                    and page_result["obscura"]["ok"]
                ):
                    ours_scroll = page_result["obscura"].get("scroll_state") or {}
                    ours_actual = ours_scroll.get("actual") or {}
                    ours_content = ours_scroll.get("content") or {}
                    chrome_geometry = chromium_state.get("geometry") or {}
                    comparable = all(
                        isinstance(value, (int, float))
                        for value in (
                            ours_actual.get("x"),
                            ours_actual.get("y"),
                            chrome_geometry.get("scroll_x"),
                            chrome_geometry.get("scroll_y"),
                        )
                    )
                    page_result["controlled_scroll_comparison"] = {
                        "comparable": comparable,
                        "obscura_actual": ours_actual,
                        "chromium_actual": {
                            "x": chrome_geometry.get("scroll_x"),
                            "y": chrome_geometry.get("scroll_y"),
                        },
                        "actual_delta": (
                            {
                                "x": ours_actual["x"] - chrome_geometry["scroll_x"],
                                "y": ours_actual["y"] - chrome_geometry["scroll_y"],
                            }
                            if comparable
                            else None
                        ),
                        "content_size_delta": {
                            "width": (
                                ours_content.get("width")
                                - chrome_geometry.get("document_scroll_width")
                                if isinstance(ours_content.get("width"), (int, float))
                                and isinstance(
                                    chrome_geometry.get("document_scroll_width"),
                                    (int, float),
                                )
                                else None
                            ),
                            "height": (
                                ours_content.get("height")
                                - chrome_geometry.get("document_scroll_height")
                                if isinstance(ours_content.get("height"), (int, float))
                                and isinstance(
                                    chrome_geometry.get("document_scroll_height"),
                                    (int, float),
                                )
                                else None
                            ),
                        },
                    }

                if chrome_ok and page_result["obscura"]["ok"]:
                    chrome_rgb = load_rgb(chrome_path)
                    current_metrics = pair_metrics(load_rgb(ours_path), chrome_rgb)
                    page_result["metrics"] = current_metrics
                    if baseline_future and page_result["baseline"]["ok"]:
                        baseline_metrics = pair_metrics(load_rgb(baseline_path), chrome_rgb)
                        page_result["baseline_metrics"] = baseline_metrics
                        for key in (
                            "rgb_mae",
                            "pixels_gt_10",
                            "pixels_gt_50",
                            "edge_bbox_max_delta",
                            "edge_row_projection_delta",
                            "edge_column_projection_delta",
                            "edge_bidirectional_mean_distance_px",
                            "edge_bidirectional_p95_distance_px",
                        ):
                            if key in current_metrics and key in baseline_metrics:
                                if current_metrics[key] is None or baseline_metrics[key] is None:
                                    continue
                                page_result.setdefault("delta_vs_baseline", {})[key] = round(
                                    current_metrics[key] - baseline_metrics[key], 6
                                )
                manifest["pages"].append(page_result)
                write_results(results_path, manifest)
                metric = page_result.get("metrics", {}).get("pixels_gt_50")
                edge_bbox = page_result.get("metrics", {}).get("edge_bbox_max_delta")
                edge_row = page_result.get("metrics", {}).get("edge_row_projection_delta")
                edge_col = page_result.get("metrics", {}).get("edge_column_projection_delta")
                edge_delta = page_result.get("delta_vs_baseline", {}).get(
                    "edge_column_projection_delta"
                )
                edge_2d = page_result.get("metrics", {}).get(
                    "edge_bidirectional_mean_distance_px"
                )
                edge_2d_delta = page_result.get("delta_vs_baseline", {}).get(
                    "edge_bidirectional_mean_distance_px"
                )
                print(
                    f"{name:84} "
                    f"p>50={metric if metric is not None else 'capture-fail'} "
                    f"edge_bbox={edge_bbox if edge_bbox is not None else '-'} "
                    f"edge_row={edge_row if edge_row is not None else '-'} "
                    f"edge_col={edge_col if edge_col is not None else '-'} "
                    f"edge_2d={edge_2d if edge_2d is not None else '-'} "
                    f"edge_col_delta={edge_delta if edge_delta is not None else '-'} "
                    f"edge_2d_delta={edge_2d_delta if edge_2d_delta is not None else '-'}",
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
