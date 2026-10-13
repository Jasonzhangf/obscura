# M1 profile/navigation implementation run notes

Owner: OB protocol/Host/browser browsing-profile writer (single writer).
Scope: `protocol/browser/**`, `crates/obscura-host/src/{main,daemon,worker}.rs`,
affected endpoint files, Host integration tests, `crates/obscura-browser/src/{context,page,input}.rs`
and the storage/JS/runtime edges they need. Contract in the controller
`docs/m1/browsing-profile-contract.md` (commit `9c1f5f7`), binding report
`m1-profile-binding-r1-20261003/docs/runs/m1-20261002/w2n-binding-r1-report.md`.

## Baseline

| Time (PT) | Node | State | Evidence | Input version | Next |
| --- | --- | --- | --- | --- | --- |
| 2026-10-03 09:51 | baseline verify | PASS | `git rev-parse HEAD`=`e7db23ba5ea32f5ba4591a67990c5dfdc70a597b`, tree `48a9486e5735d7bc99a6761700caf88e4c36d367` = required W2 tree; `git status --porcelain` empty | branch `codex/m1-profile-navigation-impl-r1-20261003` | freeze ABI |
| 2026-10-03 09:51 | fixed binary | VERIFIED | `shasum -a 256 .../m1-engine-reconcile-20261002/target/release/obscura-host`=`e4d3aa1d07c353ce7169d87d2f658f3cc269cb9a67219c6592dd1565cc20bfa7` | fixed reader binary | red regression |
| 2026-10-03 09:52 | shared target lease | BLOCKED (BUILD_LEASE_WAIT) | `controller docs/runs/m1-20261002/ob-shared-target-lease.json`: state `waiting_readers`, owner_task `m1-profile-navigation-impl-r1-20261003`, readers still hold | lease read-only | source/ABI work only |

## Red regression (fixed binary, real Host Unix socket)

Consumer: `/tmp/ob_red_probe.py` (throwaway probe, not committed) started the
fixed binary with a fresh private `--socket-dir`, attached an Agent, navigated a
`data:text/html` page, then sent the desired typed commands. Raw result:

- `ready.version` = 10 (Host10); `navigate` succeeded (`document_revision` 1).
- `back`, `forward`, `reload`, `tab_create`, `tab_select`, `tab_close`,
  `profile_catalog`, `profile_create`, `profile_select` all returned
  `{"type":"error","id":0,"code":"INVALID_REQUEST","message":"Invalid protocol request"}`.
- `obscura-host --help` has only `--socket-dir` and `--allow-private-network`;
  no `--profile-root`, so no explicit persistence root exists.

Conclusion: the first missing capabilities are Back/Forward/Reload, tab
lifecycle, profile catalog/create/select, a profile root, and persistence. The
fixed binary exposes one Page and no durable profile, matching the binding
report. This is `SOURCE_RED`; the durable green consumer is
`crates/obscura-host/tests/navigation_profile.rs`.

## Implementation and gate

| Time (PT) | Node | State | Evidence | Input version | Next |
| --- | --- | --- | --- | --- | --- |
| 2026-10-03 16:05 | shared target lease | GRANTED | `ob-shared-target-lease.json`: state `granted`, owner_task `m1-profile-navigation-impl-r1-20261003`, target `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` | lease read-only | build candidate |
| 2026-10-03 16:20 | localStorage reachability | ROOT CAUSE | `bootstrap.js` `_mkStore` captured `Deno.core.ops.op_local_storage` at snapshot build time (null); fixed to resolve lazily per call. Probes: `localStorage.setItem` no longer realm-local; shared across tabs and survives reload | WIP source | rebuild |
| 2026-10-03 16:35 | opaque-origin file name | ROOT CAUSE | Long `data:` URL origin exceeded filesystem name limit; `flush()` failed `File name too long (os error 63)` -> `PROFILE_STORAGE_FAILED`. Fixed `storage_key`: tuple origins keep hex serialization, opaque origins use SHA-256 digest. `crates/obscura-browser/src/context.rs` | WIP source | rebuild + rerun consumer |

### Focused gate

Command (lease-bound target, candidate rebuild):

```sh
CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target \
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
OBSCURA_TEST_BINARY_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release \
cargo nextest run --locked --release --features render \
  -p obscura-host -p obscura-browser -p obscura-host-protocol --no-fail-fast -j4
```

Result: exit 0, `132 tests run: 132 passed, 0 skipped`. Includes the new
durable public consumer `navigation_profile` (history/tabs/profile/persistence
across restart), `corrupt_profile_catalog_fails_startup_with_a_typed_error`,
the endpoint remote-principal refusal assertions, and the previously failing
`webrtc_endpoint::real_receiver_gets_continuous_host_media_and_rejects_stale_binding`.

### Maintenance flush

Added typed Host-local `FlushProfiles` command and `ProfileFlush` result: the
Host drains one loaded profile at a time, flushes each cookie/localStorage
store, and answers `flushed: true` only after every loaded profile reported a
saved store. A store failure stops the drain with `PROFILE_STORAGE_FAILED`.
`FlushProfiles` is never on the mTLS endpoint allowlist.

### Remote principal scope

Removed `profile_catalog` / `profile_create` / `profile_select` from the mTLS
endpoint `permitted_remote_command` allowlist. A paired remote principal has no
profile scope, so those commands return `REMOTE_COMMAND_FORBIDDEN` on the
endpoint and are actionable only over the local authenticated Host socket.

### Full gate (lease target, candidate rebuild)

Fresh consumers had to move with the Host11/WebRTC10 bump: `FrameInfo` gained
required `tab_id` / `presentation_revision`, so the four test-only literals in
`crates/obscura-media/{tests/stream.rs,src/webrtc_endpoint.rs,src/bin/webrtc_receiver.rs}`
were updated. Naive full gate (no `OBSCURA_TEST_BINARY_DIR`) failed only because
`webrtc_endpoint` read a nonexistent worktree `target/release`; binding the
candidate binary dir fixed it.

```sh
CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target \
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
OBSCURA_TEST_BINARY_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release \
cargo nextest run -j4 --locked --release --features render --no-fail-fast
```

Result: exit 0, `1704 tests run: 1704 passed, 4 skipped`. Two earlier runs
failed one *different* `obscura-cli` wall-clock test each
(`mcp_client::test_evaluate`, then `obey_robots_reaches_scrape_worker_and_blocks_target_request`);
both pass in isolation and the final run is green. This matches the known
load-sensitive robots/mTLS flake window main control already owns; no assertion
was weakened and no timeout was raised.

### Builds, obstacle course, binaries

| Time (PT) | Node | State | Evidence | Input version | Next |
| --- | --- | --- | --- | --- | --- |
| 2026-10-03 15:45 | build host bins | PASS | `cargo build --locked --release --features render -p obscura-host --bins` exit 0 | candidate source | build cli |
| 2026-10-03 15:46 | build cli bins | PASS | `cargo build --locked --release -p obscura-cli --bins --features render` exit 0 | candidate source | obstacle |
| 2026-10-03 15:50 | obstacle course | PASS | benchmark `obscura-benchmark/m1-io-serial-repair-20261002/obstacle-course` HEAD `8bd11ade`, `OBSCURA_BIN=<candidate obscura> python3 run.py --runs 1 --warmup 0` exit 0, correctness 33/33 | candidate CLI | delivery |
| 2026-10-03 15:53 | media bins (webrtc) | PASS | `cargo build --locked --release -p obscura-media --bins --features webrtc` exit 0; `webrtc_endpoint` 2/2 pass against rebuilt binaries | candidate source | delivery |

Candidate binary hashes (target/release):

```text
8b2e9c31468d5cfcca295502aa1a39324a88ddb037c02f60ec261d36ad722bb3  obscura-host
dcccf089bb8d41df53b377768faeac7d7f2f1a27af686e020d4276b4af958503  obscura
3bce11c7049ef3468f25db41f617a16d0b5b406d534e54201f2f1efea15c02a1  obscura-media
7e3277cc144d28067d00c7e931e82cdc7959698616dbde9e8023e1bf7512e024  obscura-webrtc-receiver
```

### Unwritable root refusal

```text
obscura-host --socket-dir <root>/sock --profile-root <root>/profiles   (chmod 500)
exit 1
stderr: Error: PROFILE_CATALOG_FAILED: profile root is not writable: Permission denied (os error 13)
socket_left: no
```

Conclusion: all applicable source gates pass on the candidate tree. No install,
review, merge, or push performed; that remains the main control's step.
