# P3-r4 host authority closure node log

Status: INCOMPLETE. A4 is green. The authority focused set and all three
release builds are green. The full nextest gate is not green because two
out-of-scope test files still encode behavior from before the designed
control-close release. No commit, merge, install, restart, OTA, device, or
runtime operation was performed.

Source HEAD: `6d64a027d0f888b8c2a16a51281bc9649b3baacb`

Tree: `a027202f771a5e235f479491e8c0d24d8ea82a32`

Branch: `codex/m1-host-p3-implementation-20261005`

Task base: `9d02931f24631e3680b3038601f4e3efe288c0ab`

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

## Blocking facts

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
