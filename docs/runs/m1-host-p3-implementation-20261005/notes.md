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

## P3-r10 second review-finding closure (controller admission block)

Task `m1-host-p3-implementation-r10-20261005`, worker `gcm-host-p3-r10`, fresh
standalone GCM author. Controller owns git add/commit/merge/push, review and
installation. The author must not commit, reset, rebase, cherry-pick or override
hooksPath, and must not launch children or register Collab.

Admission facts the controller verified before writing this block:

- Start HEAD: `9be2f0826b6d90ad44838420cd27f4b3864fe6f4`
  (tree `bcd4b99f4627f8257b4dae0a18af0439bab04e54`), branch
  `codex/m1-host-p3-implementation-20261005`.
- Parent chain: `9d02931f` (task base) → `9f53cbe` → `baca971` → `6d64a02` →
  `dfdf495` → `b2584ee` → `759b99a` (rejected) → `c9264e2c` (r7) →
  `6b7710c6` (r8, review FAIL) → `9be2f082` (r9).
- Working tree: only the pre-existing untracked
  `docs/runs/m1-frame-forwarding-impl-r1-20261005/` is present. It must never be
  added, modified or deleted.
- Target grant: the sole Obscura target is
  `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`, held
  by this task for build and test only, with `CARGO_INCREMENTAL=0` and
  `CARGO_BUILD_JOBS=2`. No shared runtime, install, restart, device, OTA, merge
  or push is granted.
- `OBSCURA_TEST_BINARY_DIR` must point at
  `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/debug`.

Why this node exists: the independent implementation review of `9be2f082`
(task `m1-host-p3-implementation-20261005-r3`, `mode=base`, base `9d02931f`)
returned **FAIL with five P1 findings and zero P0**. The review record is at
`.agent-collab/review/m1-host-p3-implementation-20261005-r3/review.final.md`.
The r2 FAIL record is preserved beside it. The eight r7 fixes, the four r8 fixes
and the four r9 fixes are not re-opened, and every existing regression must stay
green.

| Time / node | Conclusion or state | Evidence path | Input version and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T21:2xZ / A0 `admission` (r10) | PREPARED by the controller. Author must confirm `git rev-parse HEAD` = `9be2f0826b6d90ad44838420cd27f4b3864fe6f4` and a clean tracked tree, then record its own A1 row. | this block; `git rev-parse HEAD`; `git status --porcelain`; `ob-shared-target-lease.json` | HEAD `9be2f082`; granted target | Close G1-G5 with black-box pairs. |
| 2026-10-06T21:1xZ / A1 `admission` (r10) | PASS. HEAD `9be2f0826b6d90ad44838420cd27f4b3864fe6f4`, branch `codex/m1-host-p3-implementation-20261005`. Working tree clean except the controller's admission-block edit to this `notes.md` and the protected untracked `docs/runs/m1-frame-forwarding-impl-r1-20261005/` directory (never add, modify or delete). Target lease `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` with `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`; `OBSCURA_TEST_BINARY_DIR` points at that target's `debug` dir. | record `notes.md` A1 row; `git rev-parse HEAD`; `git status --porcelain` | HEAD `9be2f082`; granted target | Close G1-G5 with black-box before/after pairs. |

## P3-r10 five-finding closure rows

Terminal: **COMPLETE for the r10 node**. The five P1 findings G1-G5 from the r3
implementation review of `9be2f082` are fixed, each with a real public-surface
discriminating before/after pair (before EXIT != 0 with the finding's assertion,
after EXIT = 0). The r7/r8/r9 regressions stay green; the focused suites, the
three release builds, the CLI dependency check, and two full workspace gates
(`1822 passed, 4 skipped`) are green. No product fix outside G1-G5 was made. No
commit, merge, install, restart, OTA, device, or runtime operation was
performed. Per-file delta sha256 and the full raw index are in the worker record
`gcm-host-p3-r10/report.md`, `result.json`, and `raw/`.

| Time / node | Conclusion or state | Evidence path | Input version and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T15:3xZ / A1 `admission` (r10) | PASS. HEAD `9be2f0826b6d90ad44838420cd27f4b3864fe6f4`, tree `bcd4b99f4627f8257b4dae0a18af0439bab04e54`; tracked tree clean except this admission edit; sole target lease `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` with `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`. | record `notes.md` A1; `git rev-parse HEAD`; `git status --porcelain` | HEAD `9be2f082`; granted target | Close G1-G5. |
| 2026-10-06T15:3xZ / G1 `unsettled-permit-retention` (r10) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs` (+ `channels.rs`). Completion state carries the finalization result; a failed release retains the registry entry, durable owner, live adapter and session paths, and shutdown surfaces the per-session failure. Pair: `blackbox_b9_unsettled_permit::unsettled_encoder_permit_is_an_explicit_terminal` before EXIT=100 (`G1: the failed release dropped the durable owner and SIGKILLed the retained adapter`) after EXIT=0. | `raw/g1-before.{log,exit}`; `raw/g1-after.{log,exit}` | HEAD `9be2f082` + r10 delta; debug nextest; real mTLS endpoint + real adapter | G2. |
| 2026-10-06T15:4xZ / G2 `pending-registration` (r10) | FIXED. Owner `crates/obscura-media/src/encoder_owner.rs`. `RegistrationState::{Unregistered,Pending,Armed,Refused}` records `Pending` before the `Register` write; an interrupt in that window resolves the typed acknowledgement and revokes before reaping. Pair: `registration_blackbox::standalone_adapter_resolves_pending_registration_before_reaping` before EXIT=100 (no revoke reached the endpoint) after EXIT=0. | `raw/g2-before.{log,exit}`; `raw/g2-after.{log,exit}` | HEAD `9be2f082` + r10 delta; real adapter + real control socket | G3. |
| 2026-10-06T15:4xZ / G3 `revoke-ack-before-reap` (r10) | FIXED. Owner `crates/obscura-media/src/encoder_owner.rs`. The registered path requires the typed non-refused `Revoke` acknowledgement before the group is addressed; `REVOKE_ACK_TIMEOUT` is 10s, covering the endpoint's 2s SIGTERM + 2s SIGKILL signal authority. Pair: `registration_blackbox::standalone_adapter_requires_revoke_ack_before_reaping_registered_leader` before EXIT=100 (leader reaped before ack) after EXIT=0. | `raw/g3-before.{log,exit}`; `raw/g3-after.{log,exit}` | HEAD `9be2f082` + r10 delta; real adapter + real control socket | G4. |
| 2026-10-06T15:4xZ / G4 `post-reap-signal` (r10) | FIXED. Owner `crates/obscura-media/src/encoder_owner.rs`. `run_encode` settles the group through `stop_kill_reap` while the leader identity is still pinned and `reaped_status` retires group/child/registration, so no group signal can address a reused pgid after `child.wait()`. Pair: `cleanup_blackbox::standalone_adapter_retires_group_authority_before_reaping_nonzero_encoder` and `..._empty_encoder_output` before EXIT=100 (marker `gone`) after EXIT=0 (`present`). | `raw/g4-before.{log,exit}`; `raw/g4-after.{log,exit}` | HEAD `9be2f082` + r10 delta; real standalone adapter + real process group | G5. |
| 2026-10-06T15:4xZ / G5 `idle-sigterm` (r10) | FIXED. Owner `crates/obscura-media/src/main.rs`. The adapter races its blocking header read, payload read, encode and output send against the lifetime SIGTERM/ctrl-c receiver; interruption finalizes only a live child and exits so the control-socket guard releases the path. Pair: `cleanup_blackbox::standalone_adapter_exits_on_sigterm_while_waiting_for_frames` before EXIT=100 (idle adapter ignored SIGTERM) after EXIT=0. | `raw/g5-before.{log,exit}`; `raw/g5-after.{log,exit}` | HEAD `9be2f082` + r10 delta; real standalone adapter + real filesystem socket | Focused suites. |
| 2026-10-06T15:5xZ / A5 `focused-green` (r10) | PASS. `obscura-host --features render`: 122/122 EXIT=0. `obscura-media --features webrtc`: 29/29 EXIT=0. Both `-j 4`, `OBSCURA_TEST_BINARY_DIR` = lease `target/debug`, `OBSCURA_WEBRTC_RAW_DIR` = record `raw/webrtc`. | `raw/a5-host-focused.{log,exit}`; `raw/a5-media-focused.{log,exit}` | HEAD `9be2f082` + r10 delta; granted target | Builds. |
| 2026-10-06T15:5xZ / A6 `builds` (r10) | PASS. media `--features webrtc`, host `--bins`, cli `--bins --features render`, each EXIT=0 in release. | `raw/a6-build-media.{log,exit}`; `raw/a6-build-host.{log,exit}`; `raw/a6-build-cli.{log,exit}` | HEAD `9be2f082` + r10 delta; granted target | Full gate. |
| 2026-10-06T15:5xZ / A7 `full-gate` (r10) | PASS twice. `cargo nextest run --workspace --features render -j 4 --no-fail-fast` in debug with `OBSCURA_TEST_BINARY_DIR` = lease `target/debug`: run 1 `1822 passed, 4 skipped` EXIT=0; run 2 same EXIT=0. The 1822 is the r9 baseline 1820 plus the new G2/G4/G5 regressions. One earlier run is retained red (`raw/a7-full-gate-1-red-continuous-timeout.*`, EXIT=100): `webrtc_endpoint::real_receiver_gets_continuous_host_media_and_rejects_stale_binding` hit its hard 180s fixture timeout under load; it passes isolated in 15.5s and in both green gates. | `raw/a7-full-gate-1.{log,exit}`; `raw/a7-full-gate-2.{log,exit}`; `raw/a7-full-gate-1-red-continuous-timeout.{log,exit}`; `raw/probe-continuous-iso.{log,exit}` | HEAD `9be2f082` + r10 delta; `-j 4`; granted target | A8. |
| 2026-10-06T15:5xZ / A8 `tree-and-diff` (r10) | PASS. `cargo tree -p obscura-cli --features render --locked` EXIT=0 with zero `obscura-host`/`obscura-media` edges. `git diff --check` EXIT=0. `protocol/browser/src/lib.rs` unchanged; `HOST_PROTOCOL_VERSION` remains 11. | `raw/a8-cli-tree.{log,exit}`; `raw/a8-diff-check.{log,exit}` | HEAD `9be2f082` + r10 delta | A9 docs. |
| 2026-10-06T15:5xZ / A9 `docs` (r10) | COMPLETE. Whole-task notes/report/result and the worker record (`notes.md`, `report.md`, `result.json`, `handoff.md`, `raw/`) written. | this file; `report.md`; `result.json`; record dir | HEAD `9be2f082` + r10 delta | Return to controller. |

### r10 disclosures

- The `before` runs revert only the named finding's fix in the working tree and
  restore the fixed source byte-for-byte afterwards (verified by sha256). The
  same r10 tests, target and environment were used for before and after.
- The `webrtc_endpoint` integration fixture defaults its raw output root into the
  protected untracked `docs/runs/m1-frame-forwarding-impl-r1-20261005/` tree.
  This task's first host focused run and first full gate wrote 37 UUID fixture
  directories there before `OBSCURA_WEBRTC_RAW_DIR` was set. The exact list was
  recorded (`gcm-host-p3-r10/raw/protected-dir-contamination.list`) and only
  those task-owned directories were removed, restoring the tree to its 70-entry
  pre-task state; all later runs used `OBSCURA_WEBRTC_RAW_DIR` under the worker
  record. No pre-existing entry was removed.
- Resource cleanup: this task's five `/tmp/ob9-*` fixture roots and the one
  `/tmp/ow-*` root left by the timed-out gate run were removed with exact-path
  `find -delete` (list: `gcm-host-p3-r10/raw/r10-fixture-dirs-cleanup.list`). No
  other owner's `/tmp` resources or processes were touched.

## P3-r9 review-finding closure (controller admission block)

Task `m1-host-p3-implementation-r9-20261005`, worker `gcm-host-p3-r9`, fresh
standalone GCM author. Controller owns git add/commit/merge/push, review and
installation. The author must not commit, reset, rebase, cherry-pick or override
hooksPath, and must not launch children or register Collab.

Admission facts the controller verified before writing this block:

- Start HEAD: `6b7710c6c44c1af1e6f061d7df325b398d627ccf`
  (tree `ddf2f4cca3b939455e6950dca15380d77d53e17f`), branch
  `codex/m1-host-p3-implementation-20261005`.
- Parent chain of this candidate: `9d02931f` (task base) → `9f53cbe` →
  `baca971` → `6d64a02` → `dfdf495` → `b2584ee` → `759b99a` (rejected) →
  `c9264e2c` (r7) → `6b7710c6` (r8).
- Working tree: only the pre-existing untracked
  `docs/runs/m1-frame-forwarding-impl-r1-20261005/` is present. It must never be
  added, modified or deleted.
- Target grant: the sole Obscura target is
  `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`, held
  by this task for build and test only, with `CARGO_INCREMENTAL=0` and
  `CARGO_BUILD_JOBS=2`. No shared runtime, install, restart, device, OTA, merge
  or push is granted.
- `OBSCURA_TEST_BINARY_DIR` must point at
  `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/debug`.

Why this node exists: the independent implementation review of `6b7710c6`
(task `m1-host-p3-implementation-20261005-r2`, `mode=base`, base `9d02931f`)
returned **FAIL with four P1 findings and zero P0**. The review record is at
`.agent-collab/review/m1-host-p3-implementation-20261005-r2/review.final.md`.
The eight r7 fixes are not re-opened and the four r8 black-box regressions must
stay green.

| Time / node | Conclusion or state | Evidence path | Input version and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T20:1xZ / A0 `admission` (r9) | PREPARED by the controller. Author must confirm `git rev-parse HEAD` = `6b7710c6c44c1af1e6f061d7df325b398d627ccf` and a clean tracked tree, then record its own A1 row. | this block; `git rev-parse HEAD`; `git status --porcelain`; `ob-shared-target-lease.json` | HEAD `6b7710c6`; granted target | Close F1-F4 with black-box pairs. |

## P3-r9 four-finding closure rows

Terminal: **COMPLETE for the r9 node**. The four P1 findings from the r8
implementation review are fixed, each with a public-surface discriminating
before/after pair. The four r8 regressions and the earlier r7 regressions stay
green; the focused suites, the three release builds, and the full workspace gate
(twice, 1816 passed, 4 skipped) are green. No product fix outside F1-F4 was
made. No commit, merge, install, restart, OTA, device, or runtime operation was
performed.

| Time / node | Conclusion or state | Evidence path | Input version and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T20:2xZ / A1 `admission` (r9) | PASS. `git rev-parse HEAD` = `6b7710c6c44c1af1e6f061d7df325b398d627ccf`; branch `codex/m1-host-p3-implementation-20261005`; `git status --porcelain` shows only the forbidden untracked `docs/runs/m1-frame-forwarding-impl-r1-20261005/` besides this node's own edits. Target lease `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` present with `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`. | `git rev-parse HEAD`; `git status --porcelain`; controller admission block | HEAD `6b7710c6`; zsh; granted target | Close F1-F4. |
| 2026-10-06T20:3xZ / A2 `F1 shutdown-join` (r9) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs`. `SessionMediaEntry` owns a finalization completion receiver, `snapshots()` returns finalizing entries too, and `finalize_session` awaits an in-progress completion. Public pair: `blackbox_b8_shutdown_join::endpoint_shutdown_joins_in_progress_final_release` before EXIT=100 (endpoint returned in ~1.6 ms during the release), after EXIT=0 (endpoint waited and reaped the adapter). | `raw/f1-before.{log,exit}`, `raw/f1-after.{log,exit}` | HEAD `6b7710c6` + r9 delta; debug nextest; real mTLS endpoint + real adapter | F2. |
| 2026-10-06T20:3xZ / A3 `F2 encode cleanup` (r9) | FIXED. Owner `crates/obscura-media/src/encoder_owner.rs`. Ordinary `run_encode` errors now route through `self.finalize()`; cleanup failure is surfaced with the original error preserved. Public pair: `cleanup_blackbox::standalone_adapter_settles_encoder_group_on_bounded_output_failure` before EXIT=100 (wrapper group alive after terminal failure), after EXIT=0 (group gone). | `raw/f2-before.{log,exit}`, `raw/f2-after.{log,exit}` | HEAD `6b7710c6` + r9 delta; real standalone adapter + real process group | F3. |
| 2026-10-06T20:4xZ / A4 `F3 unsettled permit` (r9) | FIXED. Owner `crates/obscura-host/src/endpoint/server.rs` (+ `crates/obscura-media/src/encoder_owner.rs`). An unsettled permit retains its `registration_id`, the endpoint uses cooperative adapter exit (no SIGKILL while unsettled), and returns the declared `SessionTerminal::EncoderPermitUnsettled` when settlement cannot be established. The adapter's pre-registration interrupt uses local refused-group cleanup because `armed` is false. Public pair: `blackbox_b9_unsettled_permit::unsettled_encoder_permit_is_an_explicit_terminal` before EXIT=100 (no terminal), after EXIT=0 (terminal with id 41). Supporting owner regression `encoder_owner_pre_registration_interrupt_reaps_locally_without_revoked` EXIT=0. | `raw/f3-before.{log,exit}`, `raw/f3-after.{log,exit}`, `raw/f3-prereg-interrupt.{log,exit}` | HEAD `6b7710c6` + r9 delta; real mTLS endpoint, granted permit, withheld Register | F4. |
| 2026-10-06T20:4xZ / A5 `F4 socket reuse` (r9) | FIXED. Owner `crates/obscura-media/src/main.rs`. `ControlSocketGuard` owns the standalone `encoder-control.sock` path across the run loop and removes only the path this invocation bound on every exit; a pre-existing path still fails bind. Public pair: `cleanup_blackbox::standalone_adapter_releases_its_control_socket_for_reuse` before EXIT=100 (stale control socket after first invocation), after EXIT=0 (both consecutive invocations succeed). | `raw/f4-before.{log,exit}`, `raw/f4-after.{log,exit}` | HEAD `6b7710c6` + r9 delta; two real standalone adapter invocations | Focused suites. |
| 2026-10-06T20:4xZ / A6 `focused-green` (r9) | PASS. `obscura-host --features render`: 122/122, EXIT=0. `obscura-media --features webrtc`: 23/23, EXIT=0. Both at `-j 4`; `OBSCURA_TEST_BINARY_DIR` = lease `target/debug`. | `raw/a5-host-focused.{log,exit}`, `raw/a5-media-focused.{log,exit}` | HEAD `6b7710c6` + r9 delta; granted target | Builds. |
| 2026-10-06T20:4xZ / A7 `builds` (r9) | PASS. media `--features webrtc`, host `--bins`, cli `--bins --features render`, each EXIT=0 in release. | `raw/a6-build-media.{log,exit}`, `raw/a6-build-host.{log,exit}`, `raw/a6-build-cli.{log,exit}` | HEAD `6b7710c6` + r9 delta; granted target | Full gate. |
| 2026-10-06T20:4xZ / A8 `full-gate` (r9) | PASS twice. `cargo nextest run --workspace --features render --no-fail-fast -j 4` in debug with `OBSCURA_TEST_BINARY_DIR` = lease `target/debug`: run 1 `1816 tests run: 1816 passed, 4 skipped` EXIT=0; run 2 same EXIT=0. The 1816 is the r8 baseline 1811 plus the five new public/supporting regressions. | `raw/a7-full-gate-1.{log,exit}`, `raw/a7-full-gate-2.{log,exit}` | HEAD `6b7710c6` + r9 delta; `-j 4`; granted target | A9. |
| 2026-10-06T20:4xZ / A9 `tree-and-docs` (r9) | PASS. `cargo tree -p obscura-cli --features render --locked` EXIT=0 with zero `obscura-host`/`obscura-media` edges. `git diff --check` EXIT=0. `protocol/browser/src/lib.rs` unchanged; `HOST_PROTOCOL_VERSION` remains 11. Run docs written. | `raw/a8-cli-tree.{log,exit}`; this file; `report.md`; `result.json` | HEAD `6b7710c6` + r9 delta | Return to controller. |

### r9 disclosures

- Two earlier probe runs (`raw/probe-b8-iso.*`, `raw/probe-b9-iso.*`) showed the
  fixed F1/F3 cases passing, but they were superseded by the `raw/f1-after.*`
  and `raw/f3-after.*` runs after the completion-state and F3 owner changes.
  The superseded probes are retained, not used as gate evidence.
- The pre-fix (`before`) runs revert only the named finding's fix in the working
  tree and restore byte-identical fixed source afterwards. The before runs used
  the same r9 tests, target, and environment as the after runs.
- The workspace gate is debug-profile because the task requires
  `OBSCURA_TEST_BINARY_DIR` = lease `target/debug`. The r8 baseline command was
  release; the test set is identical. Run 1 needed no fixture retry.
- Resource cleanup: the four `/tmp` fixture directories left by the intentional
  red before runs (`/tmp/ob8-4557810f`, `/tmp/ob8-64461887`,
  `/tmp/ob9-1b85135c`, `/tmp/ob9-7d198a86`) were removed with exact-path
  scoped `find -delete`. No other owner's resource was touched. `raw/webrtc`
  and `raw/webrtc2` are retained under the worker record as evidence.

## P3-r8 black-box evidence rows

Terminal: **COMPLETE**. This node closes the required black-box evidence gap for
B1, B4, B5, B6, and B7 without re-litigating any r7 fix. B1, B4, B6, B7 now
have public-surface discriminating before/after pairs; B5 has a documented
impossibility (A3). No product source was modified; all temporary source
reverts were restored byte-identical.

| Time / node | Conclusion or state | Evidence path | Input version and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-06T14:20Z / A1 `admission` (r8) | PASS. `git rev-parse HEAD` = `c9264e2cb459e791e31a7888233ba7e27cd45afd`; branch = `codex/m1-host-p3-implementation-20261005`; `git status --porcelain` shows only the pre-existing untracked `docs/runs/m1-frame-forwarding-impl-r1-20261005/`. The sole Obscura target grant is recorded at the task-owned stage-notes header for owner `m1-host-p3-implementation-r8-20261005` / `gcm-host-p3-r8`, with `CARGO_INCREMENTAL=0` and `CARGO_BUILD_JOBS=2`. | `git rev-parse HEAD`; `git branch --show-current`; `git status --porcelain`; task-owned notes header | HEAD `c9264e2c`; zsh; granted target `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` | Design and add public-surface regressions for B1, B4, B5, B6, B7. |
| 2026-10-06T17:0xZ / A2 `pairs` (r8) | PASS. Four new public-surface tests discriminate: B1 `blackbox_b1_denial_fence.rs::datachannel_denial_fences_only_the_denied_attachment_media` (100→0), B4 `blackbox_b4_encoder_revoke.rs::revoke_ack_waits_for_endpoint_group_signal_authority` (100→0), B6 `blackbox_b6_registry_lock.rs::slow_session_finalize_does_not_block_a_second_session` (100→0), B7 `blackbox_b7_spawn_rollback.rs::failed_adapter_spawn_rolls_back_and_retry_serves_media` (100→0). Each before run reverted only that finding's fix. | `raw/b1-dc-*`, `raw/b4-revoke-*`, `raw/b6-registry-r8-*`, `raw/b7-rollback-*` | HEAD `c9264e2c` + new tests; real binaries; `--features render` | Record A3. |
| 2026-10-06T17:0xZ / A3 `impossibility` (r8) | PASS (documented impossible). B5 fires only when the hard-coded 3600s connection-lifetime future is cancelled while the endpoint runs. Arrangement 1 (abrupt close) and arrangement 2 (endpoint SIGTERM + `finalize_all`) both pass on reverted and fixed source (`b5-abrupt-*` 0/0, `b5-shutdown-*` 0/0). No public config changes the timeout. | `raw/b5-abrupt-*.{log,exit}`, `raw/b5-shutdown-*.{log,exit}`; `blackbox_b5_cancel_probe.rs` | HEAD `c9264e2c` + probe | Record A5-A8. |
| 2026-10-06T17:2xZ / A5 `focused-green` (r8) | PASS. endpoint 5/5, persistent 28/28, remote_admission_r1 37/37, webrtc_endpoint 12/12, obscura-endpoint unit bin 10/10, obscura-media stream 5/5; each own `.exit`=0, run at `-j 4`. | `raw/a5-*.{log,exit}` | HEAD `c9264e2c` + new tests; granted target | Builds. |
| 2026-10-06T17:3xZ / A6 `builds` (r8) | PASS. media `--features webrtc`, host `--bins`, cli `--bins --features render`, each `.exit`=0. | `raw/a6-media-build.*`, `raw/a6-host-build.*`, `raw/a6-cli-build.*` | HEAD `c9264e2c` + new tests; granted target | Full gate. |
| 2026-10-06T17:5xZ / A7 `full-gate` (r8) | PASS. Two runs, each `1811 tests run: 1811 passed, 4 skipped`, `.exit`=0. Pre-bump run 1 failed 3 cases with `listener did not initialize` at the fixtures' old 10s deadline (`1808 passed, 3 failed, 4 skipped`); recovered and retained as `raw/a7-full-gate-1-pre-bump.{log,exit}`; fixture socket deadline raised to 60s; then two clean runs. | `raw/a7-full-gate-1-pre-bump.*` (red, recovered), `raw/a7-full-gate-1.*`, `raw/a7-full-gate-2.*` | HEAD `c9264e2c` + new tests; `-j 4`; granted target | Record A8. |
| 2026-10-06T18:0xZ / A8 `obstacle-reuse` (r8) | NOT_RUN. `cargo tree -p obscura-cli --features render --locked` `.exit`=0 with zero `obscura-host` / `obscura-media` edges. Rebuilt CLI SHA256 `4ee9e2dce274ef1d8c1a163f557ca215dbe2be8ef3957513364e5bfdde208f45`. | `raw/a8-cli-tree.*`, `raw/a8-cli-sha256.log` | HEAD `c9264e2c` + new tests; granted target | Write A9. |
| 2026-10-06T18:1xZ / A9 `docs` (r8) | COMPLETE. Whole-task notes/report/result and the worker record (`notes.md`, `report.md`, `result.json`, `handoff.md`) written. Source reverts restored (`server.rs` `2e1e0a8b`, `channels.rs` `cee34521`, `encoder_control.rs` `f9b60b05`); orphaned test adapters reaped by explicit PID. | this file; `report.md`; `result.json`; record docs | HEAD `c9264e2c` + new tests | Return to controller. |

### Disclosed non-gate runs (r8)

- `raw/a5-remote-admission-loadflake1.{log,exit}` (EXIT=100): first
  `remote_admission_r1` run without `-j 4` at host load average ~38 from
  unrelated tasks; three cases hit a bind-to-listen `Connection refused` race.
  Re-run at `-j 4` is 37/37. Load/invocation artifact, not a product result; the
  red log is retained.
- `raw/a7-full-gate-1-pre-bump.{log,exit}` (EXIT=100): the first full-gate run,
  before the fixture socket deadline was raised. It failed 3 cases with
  `listener did not initialize` at the old 10s deadline
  (`1811 tests run: 1808 passed, 3 failed, 4 skipped`). The live log was
  overwritten by the green re-run, so the run summary was recovered verbatim
  from the worker `events.jsonl` and written to this path; the run's own `run.sh`
  wrote `EXIT=100`. Retained as history; not a product result.
- `raw/b1-denial-before.exit`=0 and `raw/b6-registry-before.exit`=0 are r7's
  endpoint-level tests, retained as history; they confirm those arrangements do
  not discriminate.

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
