# P3 host suite closure: run report (r6 terminal)

Status: **COMPLETE for the r6 node**. The two open blockers are settled, the
full nextest gate is green with zero failures on four consecutive runs, the
three release builds and the focused sets are green, and the docs are written.
No commit, merge, install, restart, OTA, device, or runtime operation was
performed. The r4 section below is retained as history.

## Identity

- Task: `m1-host-p3-implementation-r4-20261005`
- Worker: `gcm-host-p3-r4`
- Worktree: `/Volumes/Intel/playground/obscura/m1-host-p3-implementation-20261005`
- Branch: `codex/m1-host-p3-implementation-20261005`
- Source HEAD: `6d64a027d0f888b8c2a16a51281bc9649b3baacb`
- Tree: `a027202f771a5e235f479491e8c0d24d8ea82a32`
- Task base: `9d02931f24631e3680b3038601f4e3efe288c0ab`
- Sole target: `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`
- Env: `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
  `OBSCURA_WEBRTC_RAW_DIR=<record>/raw/webrtc`,
  `OBSCURA_TEST_BINARY_DIR=<target>/release`

## Milestones

| Node | State | Command | Exit | Evidence |
| --- | --- | --- | --- | --- |
| A1 admission | PASS | `git log -1`, `git status --porcelain`, lease read | 0 | record `notes.md` A1 row; `ob-shared-target-lease.json` |
| A2 admin-ops | PASS inherited | confirmed inside the A5 focused run | 0 | r3 `raw/a2-admin-tests.log`; `raw/a5-focused-r4.log` |
| A3 receipt | PASS inherited | confirmed inside the A5 focused run | 0 | r3 `raw/a3-receipt-tests.log`; `raw/a5-focused-r4.log` |
| A4 bb35 | PASS | `cargo nextest --test endpoint -E 'test(exact_session_final_release_runs_finalizer_before_reader_join)'` | 0 | `raw/a4-fix4.log`/`.exit` (1/1 PASS 6.847s) |
| A5 focused | PARTIAL | `cargo nextest -p obscura-host --test endpoint --test persistent --test remote_admission_r1` | 0 | `raw/a5-focused-r4.log`/`.exit` (68/68) |
| A5 webrtc | FAIL | `cargo nextest -p obscura-host --test webrtc_endpoint` | 100 | `raw/a5-webrtc-full.log`/`.exit` (9 passed, 3 failed) |
| A6 media build | PASS | `cargo build --locked --release -p obscura-media --features webrtc --bins` | 0 | `raw/a6-media-build-r4.log`/`.exit` |
| A6 host build | PASS | `cargo build --locked --release -p obscura-host --bins` | 0 | `raw/a6-host-build.log`/`.exit` |
| A6 cli build | PASS | `cargo build --locked --release -p obscura-cli --bins --features render` | 0 | `raw/a6-cli-build.log`/`.exit` |
| A7 full suite | FAIL | `cargo nextest run --locked --release --features render --no-fail-fast` | 100 | `raw/a7-full-suite.log`/`.exit` (1790 passed, 8 failed, 4 skipped) |
| A8 obstacle | NOT_RUN | `cargo tree -p obscura-cli --features render --edges normal --locked` | 0 | `raw/a8-cli-tree-locked.log`/`.exit`; `raw/a8-cli-sha256.log`/`.exit` |
| A9 docs | COMPLETE | this report, `result.json`, record `report.md`/`result.json` | 0 | worktree and record docs |

`git diff --check -- crates/obscura-host/src/endpoint/server.rs
crates/obscura-host/tests/endpoint.rs` exits 0.

## A4 root cause and fix

The r3 checkpoint failed `crates/obscura-host/tests/endpoint.rs::exact_session_final_release_runs_finalizer_before_reader_join`
(`BB-EX-MEDIA-35`) because an abrupt mTLS control close returns a rustls error.
`channels::control` left its loop through `message?` and never reached the
trailing `release_binding`. The exact-session registry reference leaked, so
`SessionDurable::finalize` never ran and A's adapter and directory survived.
Real process observation confirmed the leak: endpoint stderr showed the media
acquire but no finalizer.

The fix stays in the allowed file `crates/obscura-host/src/endpoint/server.rs`:

- `SessionGrant` keeps the exact-session reference it currently holds in a
  shared `bound` slot.
- `SessionGrant::release` clears the matching slot and then calls the single
  `release_exact_session` owner.
- `serve` drains the slot after `channels::control` returns and releases it.

This runs cleanup on the error return path without a spawned task and without a
second finalizer. An earlier r4 attempt that released from a spawned `Drop` was
rejected because it broke three scripted WebRTC cases; it is disclosed in
`raw/a4-scripted-webrtc.log` and is not the retained design.

All seven r3 `bb35 finalize:` diagnostics and the r4 test-only `/tmp/bb35-endpoint.err`
writes were removed. The case now observes A's and B's real adapter PIDs, asserts
A's adapter is reaped, and asserts B's adapter survives.

## Failing public cases

All eight failures are reproduced in `raw/a7-full-suite.log` (EXIT=100).

1. `obscura-media::stream::public_adapter_rejects_session_switch_and_exits`
2. `obscura-media::stream::public_adapter_reports_encoder_failure_and_exits`
3. `obscura-media::stream::public_adapter_preserves_host_identity_through_resize_and_close`
4. `obscura-media::stream::public_adapter_stops_after_first_closed_packet`
5. `obscura-media::stream::public_adapter_recovers_after_host_unavailable`
6. `obscura-host::webrtc_endpoint::same_content_new_sequence_publishes_each_frame`
7. `obscura-host::webrtc_endpoint::foreign_session_then_matching_a_is_held_until_admitted`
8. `obscura-host::webrtc_endpoint::initial_none_waits_for_first_admitted_frame`

Cause of 1-5: `crates/obscura-media/tests/stream.rs` starts the adapter with
only `--frames-socket` and `--video-socket`. The current adapter main also opens
`encoder-control.sock` beside the video socket, so the fixture has no listener
and the adapter exits with `Open Host encoder control: No such file or directory`.
The five cases fail in isolation (`raw/a7-media-stream-isolated2.log`, EXIT=100).
The file is outside the allowed write scope.

Cause of 6-8: `crates/obscura-host/tests/webrtc_endpoint.rs` sends twelve frames
after the receiver exits at three frames. The designed control-close release now
removes A's encoded socket at that point, so the later writes fail with
`Broken pipe (os error 32)`. This is a test arrangement defect, not a product
regression and not a flake. The file is outside the allowed write scope.

Neither failing set is caused by the r4 host delta. The r4 delta touches only
`crates/obscura-host/src/endpoint/server.rs` and
`crates/obscura-host/tests/endpoint.rs`; the media fixture fails independently
of the host changes.

## NOT_EXECUTED cases

These cases need a fault-injection affordance the product does not expose and
the allowed file set cannot supply. No product test hook, environment-variable
fault switch, or second code path was added.

| Case | Missing affordance |
| --- | --- |
| BB-EX-MEDIA-09 | A test-owned wrapper that holds the adapter child before it connects to the encoded socket. The allowed files cannot add a product pause hook. |
| BB-EX-MEDIA-10 | A test-owned Unix transport relay that forwards only a partial header and holds the socket open. No relay entrance is exposed. |
| BB-EX-MEDIA-11 | A test-owned Unix transport relay that forwards a complete header plus a short payload and holds the socket open. No relay entrance is exposed. |
| BB-EX-MEDIA-12 | A public surface that exposes the terminal envelope identity from the aggregate counter. The case needs to observe aggregate zero versus enqueue order, which the allowed files do not expose. |
| BB-EX-MEDIA-13 | A test-owned transport relay that stops forwarding consumer-to-endpoint packets after the terminal slot commit/enqueue. No relay entrance is exposed. |
| BB-EX-MEDIA-14 | A test-owned transport or process fault that closes the DataChannel/PeerConnection or raises `OnError` during the terminal send. No fault entrance is exposed. |
| BB-EX-MEDIA-31 | A wrapper encoder that inherits the encoder stdout pipe and holds it open after the wrapper exits. The allowed files cannot add a product wrapper hook. |
| BB-EX-MEDIA-33 | A way to pause the real adapter between the FFmpeg spawn and `RegisterEncoder`. No product pause hook is exposed. |
| BB-EX-MEDIA-34 | A way to panic the encoder-control reader while the adapter awaits an ACK. No panic-injection entrance is exposed. |
| BB-EX-MEDIA-36 (endpoint-side half) | A test-owned `encoder-control.sock` relay the adapter can be pointed at. The adapter hardcodes the encoder-control path beside the video socket, so it cannot be redirected to a relay. |

## A8 obstacle and CLI hash

The CLI surface is not affected. `cargo tree -p obscura-cli --features render
--edges normal --locked` exits 0 and has zero `obscura-host` edges
(`raw/a8-cli-tree-locked.log`).

The rebuilt CLI SHA256 is
`c85e2790e948de877e9377367615813bfa5e256a919cd6775f23b0b142e3f4f9`
(`raw/a8-cli-sha256.log`). The accepted r4 value is
`23eee1832f83d53e78dfaa3cf5f81727763e2b17d6b7544ac59893b759712592`.
The two differ because the accepted value came from a different worktree path.
The controller must reconcile this before any reuse claim. The obstacle course
was not run because the CLI surface is unaffected.

## Task delta per-file SHA256

Full-task delta `9d02931f...` to the current tree:

```text
f0e02479ecb51c96f1aa97b5deed6de14728b2cec1a2d973e47b09feb1006e11  Cargo.lock
944022817dddfd14f2d4d5fd1aeaefd12d97fcb9d63e8982f6e7f4d188a30ce9  Cargo.toml
2385f381908b240bce40765fe773462e5e8c40033764f08137d6a5cb6e62af5b  crates/obscura-host/Cargo.toml
009ad64c2c8861ada327b8993c634a1444eb8ad0c4b49597f3978d7d6ecb7ccf  crates/obscura-host/src/catalog.rs
21a77697f2bd3581588d5e523fbba4bdb8492cc9ccd176b579faf0b8318e8d8b  crates/obscura-host/src/daemon.rs
628e9f1be701c6ab6fcda30024e033f13772748219dee1e0c0f9a16d5ea2008a  crates/obscura-host/src/endpoint/channels.rs
46ebd10d7386425e0ed8e7a8b1da547d544885b66cdedf4658679c1763224247  crates/obscura-host/src/endpoint/encoder_control.rs
96024a3afeca01c23345d47e864f60c9d355439fb1b3bcc6cad243bac2c08e92  crates/obscura-host/src/endpoint/main.rs
88cb53b756ab6edb9a1f800e2c4f84f6ad2779c6b7d37e8a34a382dc4c30512b  crates/obscura-host/src/endpoint/server.rs
c05e38fad733ead9c5e035e988c59693126208baa9763cf5cfc56b0124deec6f  crates/obscura-host/src/media.rs
f0ddfb8a4a0d477dd7abd0dba1df704ba1c760b65b8277f735e280eae99fe21e  crates/obscura-host/src/remote_authorization.rs
09beb22274927948365a6cb58d1ac95686e1a591efcdf1c290c49d5cf4641c8d  crates/obscura-host/tests/endpoint.rs
6cac1f0ea21a1814f5757db6887d6cd88b8e7e2a68f486cf7b6313150410a73d  crates/obscura-host/tests/persistent.rs
1ac35f16d216dfba68c9e4ddc3ab3262bbc7d65ee63fb48e5f41ab4048059028  crates/obscura-host/tests/remote_admission_r1.rs
99954e1d0337797dce0cf3363616dfae7b0a655dac1033ee0eff4d348507f965  crates/obscura-host/tests/webrtc_endpoint.rs
bb88554ff3647b729585c543baccfb6ddf4c89223f09775c6347e4648fc5c897  crates/obscura-media/Cargo.toml
d5879045a53384982bbf9a04fbddee5d5264c3c54866f137dd08eeb08f87d3cf  crates/obscura-media/src/encoder_owner.rs
49666d57a3dbdb64ea9b8e633ef07bf2a57c934bac42e4fe52f22035f496a7f8  crates/obscura-media/src/lib.rs
12a359c6584c29d0e6a0488264dc31c023252f529d5826311f0485a01e74aa0c  crates/obscura-media/src/main.rs
11a5d30e70a3b13940e0df3d1dd8a792da615950bf45770d6572ddc606b01fbc  crates/obscura-media/tests/encoder.rs
```

The two r4-local files are `endpoint/server.rs` and `tests/endpoint.rs`; the
rest is the inherited r2/r3 checkpoint delta.

## Covered cases and observations

- Catalog create, select and info through the real `host-admin.sock`, and the
  membership grant/removal contract, are one real-daemon test:
  `persistent::admin_authority_commits_catalog_and_refuses_invalid_membership`
  PASS in `raw/a5-focused-r4.log`. It asserts mode 0600 on `host-admin.sock`,
  create/select/info revisions, that `select` never creates a profile, an
  export-eligible grant receipt bound to the committed revision and applied
  members, that replay with a changed field is `ADMIN_FRAME_MALFORMED`, that
  revision conflicts are `CATALOG_REVISION_CONFLICT`, that revoked/absent/stale
  devices return `DEVICE_REVOKED`/`DEVICE_NOT_REGISTERED`/`DEVICE_GRANT_STALE`,
  that removal refuses while the target is present (`PROFILE_ACCESS_FAILED`),
  and that the `removed` receipt does not contain the removed target.
- Malformed, partial, unknown and wrong-intent admin frames:
  `persistent::admin_authority_refuses_malformed_partial_unknown_and_wrong_intent`
  PASS in `raw/a5-focused-r4.log`.
- Applied receipt v1 after bind, shape, 0600, and typed refusal:
  `endpoint::applied_receipt_is_written_after_bind_with_v1_shape_and_0600` and
  `endpoint::applied_receipt_is_absent_when_listener_bind_fails` PASS in
  `raw/a5-focused-r4.log`.
- BB-EX-MEDIA-35 real-process release and finalizer-before-reader-join:
  `endpoint::exact_session_final_release_runs_finalizer_before_reader_join`
  PASS in `raw/a4-fix4.log`.

## Inputs that invalidate which evidence

- Any change to `crates/obscura-host/src/endpoint/server.rs` or
  `crates/obscura-host/tests/endpoint.rs` invalidates the A4, A5 focused, A6 and
  A7 evidence.
- Any change to `crates/obscura-host/tests/webrtc_endpoint.rs` invalidates the
  A5 webrtc and A7 full-suite evidence for the scripted cases.
- Any change to `crates/obscura-media/tests/stream.rs` or the media adapter main
  invalidates the A7 media failure evidence.
- The A8 CLI hash is path-bound; a rebuild from a different worktree invalidates
  it.

## Controller next steps

1. Do not review or merge this partial result.
2. Authorize the test-only repairs outside the current write scope:
   add the missing `encoder-control.sock` listener in
   `crates/obscura-media/tests/stream.rs`, and make the three scripted
   `crates/obscura-host/tests/webrtc_endpoint.rs` cases stop sending once the
   receiver exits.
3. Reconcile the accepted CLI hash `23eee183...` against the rebuilt
   `c85e2790...`; decide whether the accepted value is path-bound or needs a
   fresh baseline.
4. Re-run the full suite, then the A8 obstacle decision, on the repaired tree.

---

## P3-r6 terminal (supersedes the r4 status above)

### Identity

- Task: `m1-host-p3-implementation-r6-20261005`
- Worker: `gcm-host-p3-r6`
- Final candidate: checkpoint HEAD `b2584ee01b702fcff59f69c5b5f45cf6d60d307d`
  (tree `33dc4afb4fc277e97132e059e61b247596f42ef2`) plus two r6 working-tree
  changes: `crates/obscura-host/tests/persistent.rs` and
  `crates/obscura-media/tests/stream.rs`.
- Env: `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
  `CARGO_TARGET_DIR=<sole target>`, `OBSCURA_TEST_BINARY_DIR=<target>/release`,
  `OBSCURA_WEBRTC_RAW_DIR=<record>/raw/webrtc`.

### Milestones

| Node | State | Command | Exit | Evidence |
| --- | --- | --- | --- | --- |
| A1 admission | PASS | `git log -1`, `git status --porcelain`, lease read | 0 | record `notes.md` A1 row |
| A2 admin-ops | PASS inherited | confirmed in the A5 focused run | 0 | r3 `raw/a2-admin-tests.log`; `raw/a5-focused.log` |
| A3 receipt | PASS inherited | confirmed in the A5 focused run | 0 | r3 `raw/a3-receipt-tests.log`; `raw/a5-focused.log` |
| A4 bb35 | PASS | `endpoint::exact_session_final_release_runs_finalizer_before_reader_join` | 0 | `raw/a5-focused.log` (4.197s PASS) |
| A5 focused | PASS | `cargo nextest ... -p obscura-host --test endpoint --test persistent --test remote_admission_r1` | 0 | `raw/a5-focused.log`/`.exit` (68/68) |
| A6 builds | PASS | media `--features webrtc`, host, cli `--features render` | 0 | `raw/a6-media-build.*`, `a6-host-build.*`, `a6-cli-build.*` |
| A7 full suite | PASS | `cargo nextest run --locked --release --features render --no-fail-fast` | 0 | `raw/a7-full-suite-env2..5.log`/`.exit` (1798 passed, 4 skipped) |
| A8 obstacle | NOT_RUN | `cargo tree -p obscura-cli --features render --locked` | 0 | `raw/a8-cli-tree-locked.log`/`.exit`; `raw/a8-cli-sha256.log` |
| A10 stream-fixture | PASS | `cargo nextest ... -p obscura-media --test stream` | 0 | `raw/a12-stream-typed.log`/`.exit` (5/5) |
| A11 scripted | PASS | `cargo nextest ... -p obscura-host --test webrtc_endpoint` | 0 | `raw/a11-webrtc-endpoint.log`/`.exit` (12/12) |
| A12 typed-fixture | PASS | `serve_control` uses the product types | 0 | `raw/a12-stream-typed.log`/`.exit` |
| A13 persistent | PASS | isolated + four full-parallel runs | 0 | `raw/a13-persistent-isolated.log`/`.exit`; `raw/a7-full-suite-env2..5.log` |
| A9 docs | COMPLETE | this report, `result.json`, record docs, raw logs | 0 | worktree and record docs |

### Blocker 1: the load-dependent `persistent` failure

Root cause = the fixture, not the product. `sleeping_page_remains_responsive_and_fetch_finishes_after_disconnect`
binds a nonblocking `TcpListener` to poll `accept`. On macOS the accepted
`TcpStream` inherits `O_NONBLOCK`, so `stream.set_read_timeout(...)` (SO_RCVTIMEO)
is inert and `read_complete_http_request` at `persistent.rs:657` returns `EAGAIN`
(`Resource temporarily unavailable`, os error 35) whenever the client's request
bytes have not yet arrived. Under 32-way load the accept/read races ahead of the
fetch; in the focused run the bytes are usually buffered, so it passed at 6.5s.
Fix (fixture setup only, all assertions untouched): restore blocking mode on the
accepted socket before the read timeout.

Sibling instance: `click_receipt_precedes_navigation_and_old_document_input_is_rejected`
(`persistent.rs:562`) has the identical bug and flaked in `raw/a7-full-suite-env1.log`
(EXIT=100, same `os error 35`); it received the same fix. This is a second
instance in the same in-scope file and the same defect class as the named case.

Stability: isolated 1/1 PASS (`raw/a13-persistent-isolated.log`/`.exit`); the
case passes in 4/4 consecutive 32-way full-suite runs
(`raw/a7-full-suite-env2..5.log`), each `1798 passed, 4 skipped`, EXIT=0.

Out-of-scope sibling (disclosed, not fixed): `crates/obscura-browser`
`page::tests::page_transport_prefetches_once_and_capture_reuses_the_bytes`
(`page.rs:7247`) has the same macOS nonblocking-accept bug. It failed in 2 of the
early full-suite runs and passed in the four env2-5 runs. `crates/obscura-browser`
is untouched by the whole task delta and is outside this node's allowed write
scope, so it was left unchanged.

### Blocker 2: the typed `serve_control` fixture

`crates/obscura-media/tests/stream.rs::serve_control` now decodes the adapter's
frame into the product `EncoderControlRequest` and answers with
`EncoderControlResponse`/`EncoderPermit::Granted`, replacing the hand-written
`serde_json::Value` match. The product enum is
`#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]`, so a
renamed variant, an added field, a wrong tag, or a malformed payload now fails
the decode and the case, where the old `request["op"].as_str()` match silently
accepted anything tagged `acquire`/`register`/`revoke`/`settle`. The response is
serialized from the product type, so the wire shape cannot drift. The five
`public_adapter_*` cases pass (`raw/a12-stream-typed.log`/`.exit`, 5/5).

### Full-suite gate

`cargo nextest run --locked --release --features render --no-fail-fast`, with
`OBSCURA_TEST_BINARY_DIR=<target>/release`, default 32 test threads. Four
consecutive runs (env2-5) each reported `1798 tests run: 1798 passed, 4 skipped`,
EXIT=0. Two runs carried a transient `LEAK` annotation on unrelated tests (an
`obscura-js` runtime test; `obscura-host::lease_cancel_r2`), which is not a
failure. Evidence: `raw/a7-full-suite-env2..5.log`/`.exit`.

Disclosed non-gate runs:

- `raw/a7-full-suite-1..4.log` (EXIT=100): `OBSCURA_TEST_BINARY_DIR` was unset,
  so the `remote_admission_r1` and `webrtc_endpoint` fixtures could not find
  `target/release/obscura-media` / `obscura-webrtc-receiver`. Invocation-env
  mistake, not a product result.
- `raw/a7-full-suite-env1.log` (EXIT=100): before the `click_receipt` fix, that
  case failed with the same macOS nonblocking-accept `os error 35`, and the
  `obscura-browser` case failed with the same class of bug.

### A8 obstacle and CLI hash

`cargo tree -p obscura-cli --features render --locked` exits 0 with zero
`obscura-host` edges (`raw/a8-cli-tree-locked.log`/`.exit`), so the obstacle
course was not run. The rebuilt CLI SHA256 is
`c85e2790e948de877e9377367615813bfa5e256a919cd6775f23b0b142e3f4f9`, matching the
r4 path-attributed value; the accepted r4 value
`23eee1832f83d53e78dfaa3cf5f81727763e2b17d6b7544ac59893b759712592` came from a
different worktree path. Record, not a gate.

### Covered cases and controller next

- Catalog create/select/info and membership grant/removal through the real
  `host-admin.sock`, and the applied-receipt v1 cases, are green in
  `raw/a5-focused.log`.
- `BB-EX-MEDIA-35` `exact_session_final_release_runs_finalizer_before_reader_join`
  is green in `raw/a5-focused.log` (4.197s) and the full suite.
- The five `obscura-media::stream::public_adapter_*` cases and the twelve
  `obscura-host::webrtc_endpoint` cases (incl. the three scripted) are green;
  `webrtc_endpoint.rs` is byte-identical to the r5 checkpoint.
- The NOT_EXECUTED list is unchanged from r4 (BB-EX-MEDIA-09/10/11/12/13/14/31/33/34
  and the endpoint-side half of BB-EX-MEDIA-36); the exact missing affordances
  are recorded in the record `report.md`. No mock promoted a real case.
- Controller next: commit the two working-tree files and the rewritten docs as
  the final candidate (do not rebase, squash, or rewrite the checkpoint chain);
  run the independent architecture review; route the disclosed `obscura-browser`
  nonblocking-accept flake to a browser-scope owner if it recurs.
