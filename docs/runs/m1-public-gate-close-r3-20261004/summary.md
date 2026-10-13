# public gate r3 - worker intake summary

Status: AUTHOR_PARTIAL (one confirmed product defect outside write scope)

Candidate: HEAD aaff68f0ae5e163c163286e6c52a60ede08b60ef
tree 20e50e9e1b50cb70b7f675d6846d98af54e35946
Base: 67d936eb036d3063c4bedf981bb98975a10750ec / tree 3ee3d23809d82283cd6210c96323b72b0cac6901
Branch: codex/m1-public-gate-close-r3-20261004 (not pushed; parent owns review/integration)
Scope: only the three owned host test files plus docs/runs/m1-public-gate-close-r3-20261004/**
No product source was modified.

## Landed changes

1. `crates/obscura-host/tests/remote_admission_r1.rs` - rp24 now distinguishes the two
   denial classes and keeps the no-leak assertion:
   - mismatched request selection vs registry-owned grant -> `REMOTE_PRINCIPAL_REVOKED`
   - registry-owned selection pointing at a missing profile -> `REMOTE_INITIAL_PROFILE_UNBOUND`
   - both assert typed denial and denied EOF; a healthy owner admission still attaches,
     proving no reservation leak. Product authorization order unchanged.

2. `crates/obscura-host/tests/persistent.rs` - both HTTP fixtures now use one bounded real
   header reader (read until `\r\n\r\n`, 8 KiB bound, explicit EOF/error). Removed the
   click-loop retry-on-`Ok(0)` and the ignored `peek`. Click receipt, old-document
   rejection, background fetch, and after-detach responsiveness assertions kept.

3. `crates/obscura-host/tests/webrtc_endpoint.rs` - diagnosis only: captures per-attempt
   Host/endpoint stdout/stderr and PIDs plus each receiver phase stdout/stderr/exit. All
   original assertions kept (UDP ICE host/host, >=3 frames, click mutation, command
   denial, stale control/document, takeover/release, same-session reconnect, typed
   encoder control). No timer changed.

## Verified

| Gate | Result | Evidence |
|---|---|---|
| rp24 focused | exit 0, 1 passed 36 skipped | raw/rp24-red/stderr.txt |
| persistent focused | exit 0, 25 passed 1 leaky 0 failed | raw/persistent-green/stderr.txt |
| obscura-media webrtc rebuild | exit 0 | raw/rebuild-media/stderr.txt |
| webrtc suite | exit 100, encoder control PASS, continuous test FAIL | raw/webrtc-rebuilt/stderr.txt |
| affected combined | exit 100, 185/186 pass, only the WebRTC product failure | raw/combined/stderr.txt |
| full workspace | exit 100, 1774/1775 pass (4 leaky, 4 skip), only the WebRTC product failure | raw/full/stderr.txt |
| release build host+cli | exit 0 | raw/build/stderr.txt |
| obstacle course | exit 0, 33/33 | raw/obstacle/stdout.txt |

## First remaining owner

`crates/obscura-host/src/endpoint/channels.rs`, `exact_session_stream` (lines 201-226),
introduced by ffccee81. The filter republishes the same `Arc<EncodedFrame>` whenever
`state.changed()` fires, because a no-op `ResultValue::Status` still bumps the watch
version. The endpoint's strictly-increasing sequence guard then fails and the endpoint
dies before forwarding the pending browser response.

Reproduced twice on this candidate; removed by a `--skip-click` differential where no
no-op state update occurs (raw/webrtc-skipclick). Required minimal diff and admission gap
are recorded in full in notes.md.

## Parent next action

Admit the stateful publish-once invariant for the per-attachment encoded-stream filter,
assign a product owner, and use `webrtc_endpoint`'s continuous test as the red-green gate.
This worker's three in-scope deliverables are complete and committed. Installed
Android/15T/Mac user acceptance is still parent-owned and not proven by these consumers.
