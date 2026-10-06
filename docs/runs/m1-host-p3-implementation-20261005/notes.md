# P3 host suite closure node log (whole task)

Status: COMPLETE for the r6 node. The two open blockers are settled, the full
nextest gate is green with zero failures on four consecutive runs, the three
release builds and the focused sets are green, and the docs are written. No
commit, merge, install, restart, OTA, device, or runtime operation was
performed. The r4 section below is retained as history.

Final candidate: checkpoint HEAD `b2584ee01b702fcff59f69c5b5f45cf6d60d307d`
(tree `33dc4afb4fc277e97132e059e61b247596f42ef2`) plus two r6 working-tree
changes: `crates/obscura-host/tests/persistent.rs` and
`crates/obscura-media/tests/stream.rs`.

r4 Source HEAD: `6d64a027d0f888b8c2a16a51281bc9649b3baacb`

r4 Tree: `a027202f771a5e235f479491e8c0d24d8ea82a32`

Branch: `codex/m1-host-p3-implementation-20261005`

Task base: `9d02931f24631e3680b3038601f4e3efe288c0ab`

## P3-r6 terminal rows (r6)

| Time / node | Conclusion or state | Evidence | Input version / environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T10:4x / A1 `admission` (r6) | PASS. HEAD `b2584ee`, branch `codex/m1-host-p3-implementation-20261005`, clean tracked tree, sole target lease owned by `gcm-host-p3-r6`. | record `notes.md` A1 row; `ob-shared-target-lease.json` | checkpoint `b2584ee` | Settle A13, tighten A12. |
| 2026-10-06T11:0x / A13 `persistent-stability` (r6) | PASS. Root cause = the fixture, not the product: macOS accepted sockets inherit the nonblocking listener flag, so `set_read_timeout` is inert and the read at `persistent.rs:657` returns `EAGAIN` (os error 35) under load. Fixed by restoring blocking mode on the accepted socket. The sibling `click_receipt_precedes_navigation...` (`persistent.rs:562`) had the identical bug and got the same fix. Assertions untouched. Isolated 1/1 PASS; passes in 4/4 consecutive 32-way full-suite runs. | `raw/a13-persistent-isolated.log`/`.exit`; `raw/a7-full-suite-env2..5.log`/`.exit` | checkpoint `b2584ee` + r6 delta; default 32 test threads | Confirm full suite. |
| 2026-10-06T11:1x / A12 `typed-fixture` (r6) | PASS. `crates/obscura-media/tests/stream.rs::serve_control` now decodes the adapter frame into the product `EncoderControlRequest` (`deny_unknown_fields`, tagged snake_case) and answers with `EncoderControlResponse`/`EncoderPermit::Granted`. The five `public_adapter_*` cases pass. | `raw/a12-stream-typed.log`/`.exit` (5/5) | checkpoint `b2584ee` + r6 delta | Confirm. |
| 2026-10-06T11:1x / A10 + A11 (r6) | PASS. The five `obscura-media::stream::public_adapter_*` cases and the twelve `obscura-host::webrtc_endpoint` cases (incl. the three scripted) stay green. `webrtc_endpoint.rs` is byte-identical to the r5 checkpoint. | `raw/a12-stream-typed.log`; `raw/a11-webrtc-endpoint.log`/`.exit` | checkpoint `b2584ee` + r6 delta | Confirm. |
| 2026-10-06T11:2x / A5 `focused-green` (r6) | PASS. endpoint + persistent + remote_admission_r1: 68/68, EXIT=0. Includes `BB-EX-MEDIA-35` (`exact_session_final_release_runs_finalizer_before_reader_join`), the applied-receipt cases, and the admin authority cases. | `raw/a5-focused.log`/`.exit` | checkpoint `b2584ee` + r6 delta | Builds/full suite. |
| 2026-10-06T11:0x / A6 `builds` (r6) | PASS. media (`--features webrtc`), host, cli (`--features render`) each EXIT=0. | `raw/a6-media-build.*`, `a6-host-build.*`, `a6-cli-build.*` | checkpoint `b2584ee` + r6 delta | Full suite. |
| 2026-10-06T11:4x / A7 `full-suite` (r6) | PASS. Four consecutive runs (env2-5) each `1798 tests run: 1798 passed, 4 skipped`, EXIT=0. Two runs carried a transient `LEAK` annotation on unrelated tests; not failures. | `raw/a7-full-suite-env2..5.log`/`.exit` | checkpoint `b2584ee` + r6 delta; default 32 test threads | Record A8. |
| 2026-10-06T11:4x / A8 `obstacle` (r6) | NOT_RUN. `cargo tree -p obscura-cli --features render --locked` EXIT=0 with 0 `obscura-host` edges. Rebuilt CLI SHA256 `c85e2790...` matches the r4 path-attributed value. | `raw/a8-cli-tree-locked.log`/`.exit`; `raw/a8-cli-sha256.log` | checkpoint `b2584ee` + r6 delta | Write A9. |
| 2026-10-06T11:5x / A9 `docs` (r6) | COMPLETE. Run docs, record `report.md`/`result.json`, and raw logs written. | this file; `report.md`; `result.json`; record `raw/` | checkpoint `b2584ee` + r6 delta | Return to controller. |

### Disclosed non-gate runs

- `raw/a7-full-suite-1..4.log` (EXIT=100): `OBSCURA_TEST_BINARY_DIR` was unset,
  so the `remote_admission_r1` and `webrtc_endpoint` fixtures could not find
  `target/release/obscura-media` / `obscura-webrtc-receiver`. Invocation-env
  mistake, not a product result; not used as gate evidence.
- `raw/a7-full-suite-env1.log` (EXIT=100): before the `click_receipt` fixture
  fix, that case failed with the same macOS nonblocking-accept `os error 35`,
  and `obscura-browser` `page_transport_prefetches_once_and_capture_reuses_the_bytes`
  failed with the same class of bug. Both root-caused; `click_receipt` is fixed,
  the browser case is pre-existing and out of scope.

### Out-of-scope finding (disclosed, not fixed)

`crates/obscura-browser` `page::tests::page_transport_prefetches_once_and_capture_reuses_the_bytes`
(`page.rs:7247`) has the same macOS nonblocking-accept read bug. It failed in 2
of the early full-suite runs and passed in the four env2-5 runs.
`crates/obscura-browser` is untouched by the whole task delta and is outside this
node's allowed write scope, so it was left unchanged.

## P3-r7 review-repair rows (r7)

Terminal: **COMPLETE for the r7 node**. Start HEAD `759b99a` (rejected candidate),
start HEAD tree `3fa367c1ea09b8f1d3946ffb8ae10cbcb64a743c`. All eight P1
findings B1-B8 are fixed at the owner each finding names; each has a real
regression that fails on `759b99a` and passes on this tree. The focused sets, the
three release builds, and the full gate (twice) are green; the real standalone
Host replay emits video. The full per-file evidence is in the worker record
`gcm-host-p3-r7/report.md`, `result.json`, and `raw/`.

| Time / node | Conclusion or state | Evidence | Input version / environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T13:00Z / B1 `denial-fence` (r7) | FIXED. Owner `crates/obscura-host/src/endpoint/channels.rs`. A correlated `REMOTE_AUTH_REQUIRED` / `REMOTE_PRINCIPAL_REVOKED` response is not a `Status`/`ProfileSelected` result, so `response_ends_media` returned false and the denial left the attachment and session id active; one `terminal_response` decision now serves both request paths, the terminal disposition and `terminal_pending` commit before `fence_media_state` clears the attachment-local admission, and the correlated response settles within `SETTLEMENT_DEADLINE`. | before EXIT=100, after EXIT=0 (record `raw/b1-denial-*`); two-observer `raw/b1-two-observer-after.log` EXIT=0 | source `759b99a`; target granted; `--features render` | B2 |
| 2026-10-06T13:00Z / B2 `frame-interest` (r7) | FIXED. Owner `crates/obscura-host/src/media.rs` (+ `daemon.rs`). One `FrameInterest` accounting owner (`acquire`/`release` via `send_modify`) shared by the legacy `frames.sock` and exact-session consumers; both independent local totals removed, so neither path can force interest to zero while the other is connected. | before EXIT=100, after EXIT=0 (`raw/b2-interest-*`) | source `759b99a`; target granted; `--features render` | B3 |
| 2026-10-06T13:00Z / B3 `group-settlement` (r7) | FIXED. Owner `crates/obscura-media/src/encoder_owner.rs`. `finalize` always revoke-awaits then `stop_kill_reap`; the group is SIGTERM/SIGKILL-settled and `wait_group_gone` before `reaped_status` releases the group identity, so an exited wrapper cannot strand a descendant. | before EXIT=100, after EXIT=0 (`raw/b3-group-*`) | source `759b99a`; target granted; media webrtc | B4 |
| 2026-10-06T13:00Z / B4 `revoke-ack` (r7) | FIXED. Owner `crates/obscura-host/src/endpoint/encoder_control.rs`. Typed `EncoderRevoke` plus a `revoke_changed` `Notify` keep a `Revoking` registration unacknowledged until `mark_revoked`; already-`Revoked` acknowledges idempotently. | before EXIT=100, after EXIT=0 (`raw/b4-revoke-*`) | source `759b99a`; target granted; `--features render` | B5 |
| 2026-10-06T13:00Z / B5 `bound-slot-drain` (r7) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs`. `SessionBindingGuard` owns the bound slot outside the cancellable timed future, drains after success, error and timeout, and its `Drop` releases on task cancellation. | before EXIT=100, after EXIT=0 (`raw/b5-cancel-*`) | source `759b99a`; target granted; `--features render` | B6 |
| 2026-10-06T13:00Z / B6 `registry-lock` (r7) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs`. `release_exact_session` marks the entry `finalizing`, takes durable/supervisor under a short lock, and never holds the registry mutex across the supervisor await; the completion is a detached task. Arrangement repair: `SessionGrant::acquire` waits on a per-registry finalization `watch` generation (not the registry mutex) and creates a fresh entry, preserving the r5/r6 stale-binding reconnect. | before EXIT=100, after EXIT=0 (`raw/b6-lock-*`); arrangement `raw/a5-focused-webrtc.log` EXIT=0 (12/12) | source `759b99a`; target granted; `--features render` | B7 |
| 2026-10-06T13:00Z / B7 `acquisition-rollback` (r7) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs`. `SessionAcquisition` owns rollback from the first directory creation and removes only this acquisition's directory and sockets on `Drop` unless `commit()` transfers ownership to `SessionDurable`. | before EXIT=100, after EXIT=0 (`raw/b7-rollback-*`) | source `759b99a`; target granted; `--features render` | B8 |
| 2026-10-06T13:00Z / B8 `public-consumer` (r7) | FIXED. Owner `crates/obscura-media/src/main.rs` (+ `tests/host-replay.py`). The standalone adapter binds and serves the typed control listener when no Endpoint provides `encoder-control.sock`, then opens the same `EncoderChildOwner`; CLI and ABI unchanged; the real replay version expectation is 11. | before EXIT=1, after EXIT=0 (`raw/b8-host-replay-*`); frames `raw/host-replay-final5` | source `759b99a`; target granted; render Host binary | A5 |
| 2026-10-06T13:20Z / A5 `focused-green` (r7) | PASS. endpoint + persistent + remote_admission_r1 `70/70`; webrtc_endpoint `12/12`; obscura-media::stream `5/5`; endpoint unit bin `10/10`. | record `raw/a5-focused-host.*`, `raw/a5-focused-webrtc.*`, `raw/a5-focused-stream.*`, `raw/a5-server-units.*` (all EXIT=0) | source `759b99a`; target granted; `--features render`; `-j 4` | A6 |
| 2026-10-06T13:23Z / A6 `builds` (r7) | PASS. media webrtc, host bins, cli render bins. | record `raw/a6-build-media.*`, `raw/a6-build-host.*`, `raw/a6-build-cli.*` (all EXIT=0) | source `759b99a`; target granted | A7 |
| 2026-10-06T13:25Z / A7 `full-suite` (r7) | PASS twice. `1806 passed, 4 skipped` each run. | record `raw/a7-full-suite-1.*`, `raw/a7-full-suite-2.*` (EXIT=0) | source `759b99a`; target granted; `-j 4` | A8 |
| 2026-10-06T13:25Z / A8 `obstacle-reuse` (r7) | NOT_RUN, reuse evidence. Zero `obscura-host`/`obscura-media` edges in the CLI tree; rebuilt CLI sha256 `4ee9e2dc...` (path-attributed, differs from the accepted r4 value). `git diff --check` over changed paths EXIT=0. | record `raw/a8-cli-tree-locked.*`, `raw/a8-cli-sha256.log`, `raw/a9-diff-check.*` | source `759b99a`; target granted | A9 |
| 2026-10-06T13:31Z / A9 `docs` (r7) | COMPLETE. Worker record `report.md`/`result.json`/`handoff.md`/`notes.md` and this whole-task doc section written. | record dir; this section | source `759b99a`; start HEAD tree `3fa367c1...` | Controller commit and review |

## r4 node log (history)

| Time / node | Conclusion or state | Evidence | Input version / environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T07:41Z / A1 `admission` | PASS. HEAD, branch, parent chain, clean tracked tree, and sole target lease verified. The inherited untracked WebRTC default directory was left untouched. | Record `notes.md` A1 row; `ob-shared-target-lease.json` | Source `6d64a02`; zsh; lease target | Start A4. |
| 2026-10-06T08:4x / A4 `bb35` | PASS. Root cause: an abrupt mTLS control close returns a rustls error, so `channels::control` left through `message?` and never reached `release_binding`. The exact-session reference leaked. Final fix in allowed `server.rs`: `SessionGrant` keeps its current exact-session reference in a shared `bound` slot; `release` clears it before the one `release_exact_session` call; `serve` drains the slot after `channels::control` returns. This covers the error return without a spawned task or a second finalizer. All r3 `bb35` diagnostics are removed. The public case now observes A's real adapter PID, A's reaped PID, and B's surviving adapter. The earlier r4 `Drop`-spawn variant was rejected because it broke three scripted WebRTC cases; that attempt is disclosed, not reused. | `raw/a4-repro.log` EXIT=100; `raw/a4-repro2.log` EXIT=100 with `is_err=true`; `raw/a4-cleanclose.log` EXIT=0; `raw/a4-fix4.log` EXIT=0, 1/1 PASS 6.847s; `raw/a4-scripted-webrtc.log` EXIT=100; `.exit` files | Source `6d64a02` + `server.rs` + `endpoint.rs`; release `--features render`; lease target | Run A5. |
| 2026-10-06T08:5x / A2 `admin-ops` | PASS inherited. The r3 real-daemon admin cases remain green in the A5 focused run. No duplicate evidence was rebuilt. | r3 `raw/a2-admin-tests.log`; `raw/a5-focused-r4.log` | r3 checkpoint plus r4 local delta; real host binary | Continue A5. |
| 2026-10-06T08:5x / A3 `receipt` | PASS inherited. The applied receipt v1 cases remain green in the A5 focused run. No duplicate evidence was rebuilt. | r3 `raw/a3-receipt-tests.log`; `raw/a5-focused-r4.log` | r3 checkpoint plus r4 local delta; real endpoint binary | Continue A5. |
| 2026-10-06T08:5x / A5 `focused-green` | PARTIAL. `endpoint`, `persistent`, and `remote_admission_r1` pass 68/68. `webrtc_endpoint` passes 9/12. The three scripted lifecycle cases fail with `Broken pipe` because they keep sending 12 frames after the receiver exits at three frames; the designed control-close release now removes A's encoded socket at that point. | `raw/a5-focused-r4.log` EXIT=0, 68/68; `raw/a5-webrtc-full.log` EXIT=100, 9 passed, 3 failed | Source `6d64a02` + local delta; release `--features render`; lease target | Record the failure and continue A6. |
| 2026-10-06T09:0x / A6 `builds` | PASS. Three separate release builds each exited 0. | `raw/a6-media-build-r4.log`/`.exit` EXIT=0; `raw/a6-host-build.log`/`.exit` EXIT=0; `raw/a6-cli-build.log`/`.exit` EXIT=0 | Source `6d64a02` + local delta; `--locked`, release, lease target | Run A7. |
| 2026-10-06T09:1x / A7 `full-suite` | FAIL. `cargo nextest run --locked --release --features render --no-fail-fast` exited 100: 1790 passed, 8 failed, 4 skipped. Failures: five `obscura-media::stream` cases and three scripted `obscura-host::webrtc_endpoint` cases. Both failing sets are outside the allowed write scope. | `raw/a7-full-suite.log` EXIT=100; isolated media reproduction `raw/a7-media-stream-isolated2.log` EXIT=100 | Source `6d64a02` + local delta; release `--features render`; lease target | Record A8 and A9 with the failure disclosed. |
| 2026-10-06T09:1x / A8 `obstacle` | NOT_RUN. `cargo tree -p obscura-cli --features render --edges normal --locked` exited 0 and has 0 `obscura-host` edges, so the CLI surface is not affected. The rebuilt CLI SHA256 is `c85e2790e948de877e9377367615813bfa5e256a919cd6775f23b0b142e3f4f9`. The accepted r4 value is `23eee1832f83d53e78dfaa3cf5f81727763e2b17d6b7544ac59893b759712592`. The hashes differ because the accepted value came from a different worktree path; the controller must reconcile this before any reuse claim. | `raw/a8-cli-tree-locked.log`/`.exit` EXIT=0; `raw/a8-cli-sha256.log`/`.exit` EXIT=0 | Source `6d64a02` + local delta; release CLI; lease target | Write A9 docs. |
| 2026-10-06T09:2x / A9 `docs` | COMPLETE for the partial result. The run docs, report, result, and raw evidence list are written. The overall task remains INCOMPLETE. | This file; `report.md`; `result.json`; record `raw/` | Source `6d64a02` + local delta | Return to the controller. |

## r4 blocking facts (resolved by r6)

1. `crates/obscura-host/tests/webrtc_endpoint.rs` is outside the allowed write
   scope and is explicitly forbidden. Its three scripted cases send frames after
   the receiver exits. The designed control-close release correctly closes the
   exact-session encoded socket, so the remaining sends fail with `Broken pipe`.
   This is a test arrangement defect, not a flake.
2. `crates/obscura-media/tests/stream.rs` is outside the allowed write scope.
   It starts the adapter without the `encoder-control.sock` listener that the
   current adapter main now requires. The adapter exits with
   `Open Host encoder control: No such file or directory`. The five stream
   cases fail in isolation. This is a stale media test arrangement, not a host
   regression.
3. The accepted CLI hash cannot be reproduced from this worktree path. The CLI
   dependency tree has no `obscura-host` edge. The controller must decide
   whether the accepted hash is path-bound or needs a fresh baseline.
