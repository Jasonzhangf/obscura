# m1-frame-terminal-response-r5-20261005 / gcm-frame-r5 report

## Scope and source

- Worktree: `/Volumes/Intel/playground/obscura/m1-frame-terminal-response-r5-20261005`
- Branch: `codex/m1-frame-terminal-response-r5-20261005`
- HEAD: `c77c8d49c050f294f1fb782d936565f5aefff02f`
- Uncommitted delta: `crates/obscura-media/src/bin/webrtc_receiver.rs`

Not authored by this worker, preserved as found: the controller is concurrently
revising `docs/runs/m1-frame-terminal-response-r4-20261005/controller-dag-binding.md`
(mtime 2026-10-05T10:30:13Z) to cite `m1-connection@5` and `m1-video@2` for the
r5 candidate. That DAG binding is controller evidence, not an author artifact,
and this report does not claim it.

Changed source SHA256:

| File | SHA256 |
| --- | --- |
| `crates/obscura-media/src/bin/webrtc_receiver.rs` | `b585701900ff5d53429c044dc09da9b22707ae33b1aa93a01ffa295d82ba4859` |

## P2 convergence result

Review `m1-frame-terminal-response-impl-20261005-r4b` PASSed with no P0/P1 and one
P2 at `crates/obscura-media/src/bin/webrtc_receiver.rs:801`.

Direct comparison at HEAD confirmed the report: `request_dc` lines 772-788 and
`wait_for_correlated_response` lines 801-817 are byte-identical in the timeout
envelope, the `tokio::select!` arms, `VideoFrame` acceptance, the
`BrowserResponse` id check, the `Pong` no-op, the `Error` bail, the unexpected
message bail, and the deadline context text. The only difference is that
`request_dc` captured `let request_id = request.id;` before the loop.

Fix: `request_dc` now captures `request.id`, sends its `BrowserRequest`, and
delegates to `wait_for_correlated_response` with that id. The duplicated loop is
deleted. `wait_for_correlated_response` is the single response-wait owner. No
wrapper, no second branch, no rename, no call-site change, and no change to
message handling, error text, or deadline. The r4 backpressure fix in
`crates/obscura-media/src/webrtc_endpoint.rs` and its regression case in
`crates/obscura-host/tests/webrtc_endpoint.rs` are untouched.

Diff: 1 insertion, 17 deletions, one file.

Function line counts:

| Function | Before | After |
| --- | --- | --- |
| `request_dc` | 28 | 12 |
| `wait_for_correlated_response` | 25 | 25 |

## Commands and exits

Shared build target: `CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`,
granted to this task in the typed lease
`/Volumes/Intel/playground/agentbrowser/m1-repair-model-20261002/docs/runs/m1-20261002/ob-shared-target-lease.json`
(`state=granted`, `owner_task=m1-frame-terminal-response-r5-20261005`,
`owner_worker=gcm-frame-r5`).

| Command | Exit |
| --- | --- |
| `cargo build --locked --release -p obscura-media --features webrtc --bins` | 0 |
| `cargo nextest run --locked --release --features render -p obscura-host --test webrtc_endpoint --no-fail-fast` | 0 |
| `git diff --check -- crates/obscura-media/src/bin/webrtc_receiver.rs` | 0 |
| `cargo nextest run --locked --release --features render --no-fail-fast` | 0 |

Focused suite: 11 tests run, 11 passed, 0 skipped.
`active_receiver_observes_saturated_request_queue_terminal_without_leak` is GREEN.
Its raw consumer report
`.../gcm-frame-r5/raw/webrtc/terminal-backpressure-detach-d87992acebb142dfbb3a5824ed778cbe/terminal/stdout.txt`
contains `trigger_receipt` with `type=result`, `id=200`, `attachment_id=null`,
`frames_after_trigger=0`, and `terminal.kind=peer_state disconnected`.

Full suite new totals: 1784 tests run, 1784 passed, 0 leaky, 4 skipped, exit 0.

Disclosure: the r4 full run
`/Volumes/Intel/playground/obscura/.worker-runs/m1-frame-terminal-response-r4-20261005/gcm-frame-r4/raw/full-nextest/stderr.txt`
reported `1784 passed (4 leaky), 4 skipped` for the same 1784 tests; those four
`obscura-host::remote_admission_r1` cases are PASS in the r5 run with no `LEAK`
marker. Pass counts and skipped counts are unchanged; only the leak annotation
changed. Leak detection is per-run timing behavior and no source in that module
was touched. No prior finding, RED result, leaky result, or disclosure is
reclassified here; the r4 RED evidence remains valid for the defect it captured.

## Blast radius

`cargo tree -p obscura-cli -e normal --locked --features render` exit 0 reports
0 `obscura-media` and 0 `obscura-host` edges. The CLI does not link the changed
receiver binary path, so the r4 obstacle evidence at benchmark
`8bd11ade523fd7358c7b1fddc398b48e9040c3c9`, fixture
`386a94c048d436794f39c30f236dacd8f194f4cf3a9c98ab84abe93202892863`, manifest
`36273f50f6308f36dffca3abf93e30e57f24605712b961a9a60568a4466075ea`, and CLI
`23eee1832f83d53e78dfaa3cf5f81727763e2b17d6b7544ac59893b759712592` stays valid.
The obstacle course was not re-run, per scope.

## Retained resources

- Task worktree and source delta retained for controller commit/review.
- Controller DAG binding revision retained; its owner and validation remain the
  controller's.
- External record: `/Volumes/Intel/playground/obscura/.worker-runs/m1-frame-terminal-response-r5-20261005/gcm-frame-r5/`

Not performed, by scope: install, restart, device, emulator, OTA, Tailscale,
merge, push, Collab registration, and child agents.
