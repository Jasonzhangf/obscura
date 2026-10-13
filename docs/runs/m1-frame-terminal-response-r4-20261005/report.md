# m1-frame-terminal-response-r4-20261005 / gcm-frame-r4 report

## Scope and source

- Worktree: `/Volumes/Intel/playground/obscura/m1-frame-terminal-response-r4-20261005`
- Branch: `codex/m1-frame-terminal-response-r4-20261005`
- HEAD: `7c487c9b4b3fb3b4e422d940822b97c19e6d0212`
- Uncommitted delta: `crates/obscura-host/tests/webrtc_endpoint.rs`, `crates/obscura-media/src/bin/webrtc_receiver.rs`, `crates/obscura-media/src/webrtc_endpoint.rs`

Changed source SHA256:

| File | SHA256 |
| --- | --- |
| `crates/obscura-host/src/endpoint/channels.rs` | `9e089dd2aad232ff7e565ec52a9e185a1d9cf906889bc15a9086d55df4279066` |
| `crates/obscura-media/src/webrtc_endpoint.rs` | `1b2568c7abcd814b93b7af43d3c1a1ab317e26b6a645866adf2545c1e56df304` |
| `crates/obscura-media/src/bin/webrtc_receiver.rs` | `a29dba024b10993461b6584cb6c3d8c37cccaa54e3e7bec98b02545d7bde621f` |
| `crates/obscura-host/tests/webrtc_endpoint.rs` | `36d6e349b7d5deaab4c2d2e29fd2de5c8f24ae6ab2273a38bb4e825030f8821f` |

## P1 #1 result

RED first: the new authenticated DataChannel regression pipelined a detach fence
and 17 follow-up requests. The receiver failed with `timed out waiting for WebRTC
browser response`; the endpoint logged `send WebRTC browser request to Host
pump` and `WebRTC terminal browser response settlement deadline exceeded`. This
is the reviewer-described circular wait.

Fix: `WebRtcEndpoint::run_inner` now waits on `terminal_pending.changed()` while
forwarding a `BrowserRequest`. Once the attachment is fenced it stops forwarding
follow-up requests, so a full request queue cannot prevent the terminal response
dispatch from being selected.

Green: the same case passes. The receiver report contains `trigger_receipt` with
`type=result`, `id=200`, `attachment_id=null`, `frames_after_trigger=0`, and
`terminal.kind=peer_state disconnected`.

## P1 #2 result

Accepted fixture root: `/Volumes/Intel/playground/obscura-benchmark/m1-io-serial-repair-20261002`

- Fixture HEAD: `8bd11ade523fd7358c7b1fddc398b48e9040c3c9`
- `observer-intersection.html` SHA256: `386a94c048d436794c39c30f236dacd8f194f4cf3a9c98ab84abe93202892863`
- `manifest.json` SHA256: `36273f50f6308f36dffca3abf93e30e57f24605712b961a9a60568a4466075ea`
- Candidate CLI SHA256: `23eee1832f83d53e78dfaa3cf5f81727763e2b17d6b7544ac59893b759712592`

Runner exits:

| Run | Exit |
| --- | --- |
| `obstacle-course/run.py --filter observer-intersection --runs 1 --warmup 0 --json` | 0 |
| `obstacle-course/run.py --runs 1 --warmup 0 --json` | 0 |

Results: focused `1/1` passed; full `33/33` passed.

## Commands and exits

1. `cargo build --locked --release -p obscura-media --features webrtc --bins` exit 0
2. `cargo nextest run --locked --release --features render -p obscura-host --test webrtc_endpoint --no-fail-fast` exit 0, 11/11 passed
3. `git diff --check -- allowed paths` exit 0
4. `cargo nextest run --locked --release --features render --no-fail-fast` exit 0, 1784 passed (4 leaky), 4 skipped
5. `cargo build --locked --release -p obscura-cli --bins --features render` exit 0
6. Two obstacle-course invocations above exit 0

## Blast radius

`cargo tree -p obscura-cli -e normal --locked` reports 0 `obscura-host` edges.
The CLI binary does not link the WebRTC host endpoint changed by this task. The
obstacle gate was still run and is green on the accepted fixture.

## Retained resources

- Task worktree and source delta retained for controller commit/review.
- External record: `/Volumes/Intel/playground/obscura/.worker-runs/m1-frame-terminal-response-r4-20261005/gcm-frame-r4/`
