# M1 browsing / tabs / persistent profile - author delivery

Status: `SOURCE_IMPLEMENTED` (author scope). Independent review, install, merge,
push, and OTA remain main control's step. Nothing here claims user delivery.

## Identity

| Item | Value |
| --- | --- |
| Branch | `codex/m1-profile-navigation-impl-r1-20261003` |
| Base (fresh fork/main) | `948f054b51af442306be046d0014cfa3da628f27` |
| W2 edit source | commit `ff50bd99e959189f697f69f7472d7a82e66e2eff`, tree `48a9486e5735d7bc99a6761700caf88e4c36d367` |
| HEAD | `e7db23ba5ea32f5ba4591a67990c5dfdc70a597b`, tree `48a9486e5735d7bc99a6761700caf88e4c36d367` |
| Worktree | `/Volumes/Intel/playground/obscura/m1-profile-navigation-impl-r1-20261003` (dirty = the implementation below) |
| Tracked diff SHA-256 | `668d3d628c2fcbd29aee6147451e0bbe924fa6b42a7dea70c946eb27ca515021` |
| Shared target | `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` (lease `granted`, owner this task) |

Untracked new files (SHA-256):

```text
6fd648eb3349b62c7d23a70f05ad7f39d3c732e8124c45c56236d1ded8cd2ac4  crates/obscura-host/src/catalog.rs
64d7317f51505012e44cd01ccb0a0dc66e1a81bcc53e3fa71dc1dc33af1d7e32  crates/obscura-host/tests/navigation_profile.rs
6e9d254dc537e421f351082d1c5d07ce02a0ac224f671a7cc7486ff843ab52fa  docs/runs/m1-profile-navigation-impl-r1-20261003/abi-handoff.md
```

## What changed

Phase A (history / tabs / presentation):

- `protocol/browser/src/lib.rs`: Host11 / WebRTC10. New commands
  `Back`, `Forward`, `Reload`, `TabCreate`, `TabSelect`, `TabClose`.
  `SessionStatus` gains `profile_id`, `presentation_revision`, `active_tab_id`,
  `tabs`, `navigation`. `PresentedFrameIdentity` / `FrameInfo` gain
  `tab_id` and `presentation_revision`.
- `crates/obscura-browser/src/page.rs`: `navigate_history_entry`, `reload`.
- `crates/obscura-host/src/worker.rs`: tab registry (`Pages` behind one shared
  `BrowserContext`), history/reload, one active-tab frame projection, real
  URL/title read from the actual Page.
- `crates/obscura-host/src/daemon.rs`: tab/presentation projection, unified
  active-tab view for every observer, A -> B -> A presentation-revision fence.

Phase B (persistent profile):

- `crates/obscura-host/src/catalog.rs` (new): typed persistent catalog,
  `--profile-root`, `PROFILE_CATALOG_FAILED` / `PROFILE_ROOT_UNCONFIGURED`,
  write-probe on `prepare_root`.
- `crates/obscura-host/src/daemon.rs`: `ProfileCatalog` / `ProfileCreate` /
  `ProfileSelect` (rebind caller to observer), runtime profile -> `SessionHandle`
  registry, `FlushProfiles` maintenance drain.
- `crates/obscura-browser/src/context.rs`: shared origin-partitioned
  `SharedLocalStorage`, typed cookie/localStorage load/save/flush. Opaque
  origins key by SHA-256; tuple origins keep hex serialization.
- `crates/obscura-js/{js/bootstrap.js,src/ops.rs,src/runtime.rs}`: lazy-resolved
  `op_local_storage` with a shared mirror so localStorage reaches the context
  backend across tabs and restarts.
- `crates/obscura-host/src/endpoint/channels.rs`: profile commands deliberately
  excluded from the mTLS remote allowlist (`REMOTE_COMMAND_FORBIDDEN`), not a
  widened scope.
- `crates/obscura-host/src/main.rs`: `--profile-root` CLI flag.

New stable error codes: `NO_HISTORY`, `UNKNOWN_TAB`, `LAST_TAB`,
`PROFILE_NOT_FOUND`, `PROFILE_ROOT_UNCONFIGURED`, `PROFILE_CATALOG_FAILED`,
`PROFILE_STORAGE_FAILED`, `REBIND_BUSY`, `PROFILE_LIMIT`, `MAINTENANCE_BUSY`.

## Real commands and raw exits

Focused gate (candidate binaries bound):

```sh
cargo nextest run --locked --release --features render \
  -p obscura-host -p obscura-browser -p obscura-host-protocol --no-fail-fast -j4
```

exit 0, `132 tests run: 132 passed, 0 skipped`.

Full gate:

```sh
OBSCURA_TEST_BINARY_DIR=<target>/release \
cargo nextest run -j4 --locked --release --features render --no-fail-fast
```

exit 0, `1704 tests run: 1704 passed, 4 skipped`.

Builds:

```sh
cargo build --locked --release --features render -p obscura-host --bins      # exit 0
cargo build --locked --release -p obscura-cli --bins --features render       # exit 0
cargo build --locked --release -p obscura-media --bins --features webrtc     # exit 0
```

Obstacle course (benchmark `obscura-benchmark/m1-io-serial-repair-20261002`,
HEAD `8bd11ade523fd7358c7b1fddc398b48e9040c3c9`, run from `obstacle-course/`):

```sh
OBSCURA_BIN=<target>/release/obscura python3 run.py --runs 1 --warmup 0
```

exit 0, `correctness: 33/33 stages passed`.

Note: the task contract named
`/Volumes/Intel/playground/obscura/m1-io-serial-repair-20261002/obstacle-course/`;
that path does not exist. The benchmark actually lives at
`/Volumes/Intel/playground/obscura-benchmark/m1-io-serial-repair-20261002/obstacle-course/`.
No benchmark file was modified.

Candidate binary hashes (target/release):

```text
8b2e9c31468d5cfcca295502aa1a39324a88ddb037c02f60ec261d36ad722bb3  obscura-host
dcccf089bb8d41df53b377768faeac7d7f2f1a27af686e020d4276b4af958503  obscura
3bce11c7049ef3468f25db41f617a16d0b5b406d534e54201f2f1efea15c02a1  obscura-media
7e3277cc144d28067d00c7e931e82cdc7959698616dbde9e8023e1bf7512e024  obscura-webrtc-receiver
```

## Public behavior evidence

Durable public consumer `crates/obscura-host/tests/navigation_profile.rs`
(`navigation_tabs_and_profiles_keep_actual_page_state_across_restart`) drives a
real Host over its Unix socket and asserts:

- Back/Forward with no history -> `NO_HISTORY`; real back restores the previous
  document and its JS/`localStorage`; forward navigation after back truncates
  forward (`NO_HISTORY`).
- Tab create/select preserves each Page's JS, form value, and scroll; selecting
  back A -> B -> A changes `presentation_revision` even though the tab id
  matches; closing the last tab -> `LAST_TAB`.
- `ProfileCreate` makes an isolated store; `ProfileSelect` lands the caller as
  `observe` (old Agent rejected with `AGENT_REQUIRED`) and does not kill other
  sessions; a fresh Agent on the selected profile sees an empty store; selecting
  default back finds the original document intact.
- `FlushProfiles` reports `flushed: true, profiles: 2`; restart restores cookies
  and localStorage but mints a new session/epoch/attachment.

`corrupt_profile_catalog_fails_startup_with_a_typed_error`: a malformed
`catalog.json` fails startup with `PROFILE_CATALOG_FAILED`, non-zero exit, and
no bound socket.

Unwritable root (manual, candidate binary):

```text
exit 1
stderr: Error: PROFILE_CATALOG_FAILED: profile root is not writable: Permission denied (os error 13)
socket_left: no
```

`crates/obscura-host/tests/webrtc_endpoint.rs` (real WebRTC over a private Unix
socket + mTLS) passes 2/2 against the rebuilt `obscura-media` /
`obscura-webrtc-receiver` binaries: continuous Host media plus stale-binding
rejection, and typed encoder-unavailable.

Endpoint remote-principal refusal: `crates/obscura-host/tests/endpoint.rs`
asserts `profile_catalog` / `profile_create` / `profile_select` return
`REMOTE_COMMAND_FORBIDDEN` for a paired remote principal.

## Known flake windows

Three full-gate runs each failed exactly one `obscura-cli` wall-clock test under
`-j4` load (`mcp_client::test_evaluate`, then
`obey_robots_reaches_scrape_worker_and_blocks_target_request`), a different test
each time; each passes in isolation and the final run is fully green. This is
the robots/timeout flake window main control already owns. No assertion was
weakened and no timeout was raised.

## Native / service handoff items (not this writer)

- AB `packages/client-connection/src/transport.rs` version pin (4 -> 11).
- AB native op map `packages/android-bridge/src/lib.rs` and Android
  `MainActivity.java`: back/forward/reload/tab/profile op numbers.
- AB `packages/client-domain/probe.ts`: navigation/tab/profile fields.
- AB `scripts/host-service/host_service.py`: version pin and `profile_root`
  argv/schema; service must confirm `ProfileFlush { flushed: true }` before
  terminating/restarting Host.
- AB service root and maintenance parameters are main control's to move; this
  writer did not edit AB.

## Resource receipt

- Temporary consumer roots under `/tmp` created by this session were removed by
  their own `Drop`/cleanup; throwaway probes (`/tmp/ob_*`) are best-effort and
  contain no credential or runtime state.
- No install, formal Host runtime, AB service, or other author's tree was
  touched. No process outside this writer's spawned Host instances was signaled.
