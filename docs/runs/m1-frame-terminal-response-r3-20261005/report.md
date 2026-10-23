# P2 frame terminal response r3

Task m1-frame-terminal-response-r3-20261005 (worker gcm-frame-r3). Fresh
standalone GCM execution author. This run fixes the r2 P1 ordering defect: the
initiating browser operation's correlated `BrowserResponse` must reach the
authenticated DataChannel client before the media fence closes the peer.

## Candidate

- Worktree `/Volumes/Intel/playground/obscura/m1-frame-terminal-response-r3-20261005`,
  branch `codex/m1-frame-terminal-response-r3-20261005`.
- HEAD `ac42a403d64f428b35105fb50a05415e19d9fa4d`. Delta is uncommitted; the
  controller owns add/commit/merge/push and review.
- Target grant `ob-shared-target-lease.json` permits
  `CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`
  for this task. All commands used it with `CARGO_INCREMENTAL=0`,
  `CARGO_BUILD_JOBS=2`.

Changed product paths and worktree sha256:

```
9e089dd2aad232ff7e565ec52a9e185a1d9cf906889bc15a9086d55df4279066  crates/obscura-host/src/endpoint/channels.rs
945c0f5b38f9f10c6934658f51045cfdfe3b8418e3ab9d74f6beae44c42a05b2  crates/obscura-media/src/webrtc_endpoint.rs
c765c634a38670b54562971bde61e55400994d45d4f7e6ef58cdc3f899c9acdc  crates/obscura-media/src/bin/webrtc_receiver.rs
a73d3deb9cb51c0ebd49f9012ca624e41f76b0daf028076985fe3523b6c15ccb  crates/obscura-host/tests/webrtc_endpoint.rs
```

## Root cause and fix

`channels.rs::control` forwarded a fence request, then awaited
`update_media_state` and delivered the response on the ordinary browser-response
pump. `exact_session_stream` closes the filtered frame watch the moment the
attachment is fenced. `webrtc_endpoint.rs::run_inner` observed that close first
and returned, closing the PeerConnection before the correlated
`BrowserResponse` reached the DataChannel.

The fix keeps the response reachable across the fence:

1. `webrtc_endpoint.rs` adds `BrowserResponseDispatch::{Ordinary, Terminal}` and
   `WebRtcBrowserResponseCompletion::{BufferDrained, SendFailed, SendTimedOut}`.
   `run_inner` handles `Terminal` by calling `send_browser_response`, which
   enqueues the correlated envelope once and then observes the transport release
   its whole send buffer (`outstanding_bytes() == 0`) with one owned, rearmed
   `poll()` receive and a biased close/error branch. It returns the completion
   before exiting, so the peer cannot close first.
2. `run_inner` also gates frame emission on a terminal-pending watch, so the
   fence still stops old-stream publication immediately even though the peer
   stays open long enough to deliver the receipt.
3. `channels.rs::control` classifies a fence with `response_ends_media`, which
   is true for a detach (no attachment id) and for a rebind (the Host moved the
   attachment to a different session). It commits the terminal dispatch and
   waits for `BufferDrained` before it releases the endpoint handle, and it
   aborts the exact-session filter on `terminal || detached`.

The drained counter is a transport release signal, not a remote-application
receipt. No fake ACK, no sleep, and no fallback path were added.

## Public-boundary evidence

The regression consumer `obscura-webrtc-receiver` now issues the fence on the
authenticated DataChannel by default and requires the matching correlated
`BrowserResponse` at the trigger request id before it reports the terminal.
`--terminal-transport wss` retains the r2 control-socket path as separate
coverage.

RED (reverse intervention, before the fix): `raw/red-focused/exit` `EXIT=100`.
Both the DataChannel detach and rebind cases failed with
`timed out waiting for WebRTC browser response` at HEAD `ac42a403` with only the
receiver-side DC fence wiring present.

GREEN (after the fix): `raw/focused4/exit` `EXIT=0`, 10/10 focused cases pass.
For the DataChannel detach and rebind cases the decoded report shows:

- `trigger_receipt.type = "result"`, `id = 200` (non-null, id-matched);
- `terminal = {"kind": "peer_state", "state": "disconnected"}`;
- `frames_after_trigger = 0` and every decoded frame belongs to the admitted
  session, so no replacement binding frame leaked into the old stream.

The WSS detach/rebind cases pass as separate coverage, and the source-close,
stale-binding, same-content, foreign-session, initial-None and encoder-failure
cases stay green.

## Commands and exits

```
1 cargo build --locked --release -p obscura-media --features webrtc --bins   -> raw/build-media5/exit EXIT=0
2 cargo nextest run --locked --release --features render -p obscura-host --test webrtc_endpoint --no-fail-fast
                                                                            -> raw/focused4/exit EXIT=0 (10/10)
3 git diff --check -- <four allowed paths>                                  -> raw/diffcheck/exit EXIT=0
4 cargo nextest run --locked --release --features render --no-fail-fast     -> raw/full-nextest3/exit EXIT=0 (1783/1783, 4 skipped)
5 cargo build --locked --release -p obscura-cli --bins --features render     -> raw/build-cli/exit EXIT=0
```

`raw/full-nextest2` recorded one transient `obscura-host::persistent` broken-pipe
failure under full parallel load; `raw/retry-persistent` reran that case alone
and passed (`EXIT=0`), and `raw/full-nextest3` ran the whole suite green. This
change touches only the WebRTC endpoint path, not that Host persistent socket
test.

## Blast radius and reuse

The change is confined to the WebRTC endpoint/pump ordering and its harness. It
does not alter CLI code paths, so the r2 release-build and obstacle evidence
remain the applicable CLI evidence; the CLI release build above re-confirms the
tree still builds. The r2 obstacle 32/33 and every earlier FAIL remain disclosed
and are not erased.

## Not claimed

This run does not declare review PASS, installed user blackbox, M1, or the
original combined FAIL root cause closed. The controller owns commit, merge,
push, installation, review and final acceptance.
