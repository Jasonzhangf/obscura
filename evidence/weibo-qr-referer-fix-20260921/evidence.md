# Weibo QR login: scripted-request Referer

## Scope

- Branch `codex/m1-referer-fetch-20260921`, base `7633e80`.
- Files: `crates/obscura-js/js/bootstrap.js`, `crates/obscura-js/src/ops.rs`.
- No merge, push, install, or restart performed here.

## Two independent first divergences

1. Entry module aborted. `crates/obscura-js/js/bootstrap.js` had no BarProp
   window surface, so the weibo desktop bundle died at top level with
   `ReferenceError: toolbar is not defined` and `#app` stayed empty. Already
   fixed upstream of this branch by `71bd62f`; this branch inherits it.
2. QR image 403. `op_fetch_url` never sent `Referer`. Sina's
   `sso/v2/qrcode/image` answers 403 without an in-site referer, so the login
   component's request failed and the QR box rendered as an empty placeholder.

## Fix

`fetch_referrer_header` mirrors `obscura_net::request_referrer`: same-origin
sends the full document URL minus fragment/credentials, cross-origin is
reduced to the serialized origin, an https->http downgrade or a non-http(s)
document URL sends nothing. `__documentReferrer()` reads `document_url` from
native state and is passed to all three `op_fetch_url` call sites.

## Evidence

Same profile, same document, base host vs fixed host:

| host | plain fetch of `/sso/v2/qrcode/image` |
| --- | --- |
| base `7633e80` (binary `b42a4e3a...`) | `403`, 0 bytes |
| fixed (binary `b45a075a...`) | `200`, 571 bytes |

Rendered frame from the fixed host, QR region decoded with `zbar`:

```
QR-Code:https://passport.weibo.cn/signin/qrcode/scan?qr=3ZGZqsRTDABb3UlQ1X0ywkZKP-r5Eml13BnFyY29kZQ..&sinainternalbrowser=topnav&showmenu=0
```

The QR `<img>` loads with `naturalWidth=180 naturalHeight=180 complete=true`
inside the previously empty `w-45 h-45 p-5` box.

Tests: `cargo test --release -p obscura-js --lib` -> 399 passed, 0 failed.

## Not proven

- No 15T device replay for this host.
- No AgentBrowser-side admission for this host.
- Stealth-mode scripted fetch is left unchanged; the deployed host is built
  without the `stealth` feature, so that path is not on the product route.
- Merge, push, remote receipt, baseline: none.
