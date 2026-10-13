# Profile Host source salvage and admitted implementation r2 handoff

Status: `SOURCE_IMPLEMENTED_BUILD_E2E_WAIT` (never `AUTHOR_VERIFIED`, `PASS`, `BB`, or `M1`).
Source commit `ffccee81930378094ac5c7dcd266068660f870ce` /
tree `ff9b9b670cb60570250a8a211cbbdf4278c48a07` on branch
`codex/m1-profile-host-admission-r2-20261004`. This file is committed as the
evidence follow-up that records that source commit.

## Source scope

Allowed and modified:

- `crates/obscura-host/src/catalog.rs`
- `crates/obscura-host/src/daemon.rs`
- `crates/obscura-host/src/endpoint/channels.rs`
- `crates/obscura-host/src/endpoint/main.rs`
- `crates/obscura-host/src/endpoint/server.rs`
- `crates/obscura-host/src/main.rs`
- `crates/obscura-host/Cargo.toml` (added workspace `serde` and locked `sha2 0.10`)
- New `crates/obscura-host/src/remote_admission.rs`
- New `crates/obscura-host/src/remote_authorization.rs`
- `crates/obscura-host/tests/endpoint.rs`
- `crates/obscura-host/tests/webrtc_endpoint.rs`
- New `crates/obscura-host/tests/remote_admission_r1.rs`

Forbidden and untouched:

- All other crates, protocol, Engine, JS, CLI, Media, workspace config, `Cargo.lock`, AB/apps/host-service/trust registry producer, original r1 salvage tree, controller notes, leases, hooks, AGENTS files, and shared runtime/device/OTA.
- The r1 salvage tree at `/Volumes/Intel/playground/obscura/m1-profile-host-admission-r1-20261004` was read-only and is preserved for successor intake.

## Frozen typed interfaces

`RemoteAdmissionRequest`:

- `principal: RemotePrincipalContext { device_id, client_cert_sha256: [u8; 32], scopes: BTreeSet<RemoteScope>, authorization_revision: u64 }`
- `initial: InitialProfileGrant { device_id, authorization_revision: u64, selection: InitialProfileSelection }`
- `request_id: u64`

`RemoteAdmissionResult` (tagged `type` = `ok` | `err`):

- `Ok { request_id, reservation_id, session_id }`
- `Err { request_id: Option<u64>, code, message }`
- `request_id: None` is legal only with `REMOTE_ADMISSION_MALFORMED`
- decoded denial keeps `Some(id)`
- empty EOF is `AdmissionRead::NoFrameEof`, no result frame
- `REMOTE_ADMISSION_EOF` is retired from the result-code surface

`RemoteAdmissionErrorCode` frozen strings:

`REMOTE_AUTH_REQUIRED`, `REMOTE_PRINCIPAL_REVOKED`,
`REMOTE_PROFILE_SCOPE_REQUIRED`, `REMOTE_INITIAL_PROFILE_UNBOUND`,
`REMOTE_INITIAL_PROFILE_ACCESS_DENIED`, `PROFILE_ACCESS_MIGRATION_REQUIRED`,
`REMOTE_ADMISSION_MALFORMED`, `REMOTE_ADMISSION_CANCELED`.

Frozen bounds and ordering:

- admission request bound 64 KiB, result bound 1 MiB, result/request write deadline 2 s
- no admission read deadline
- `take(MAX + 1)` then `read_until`; `n > MAX` is oversized before delimiter/decode, including a newline at `MAX + 1`
- one `into_split`, same `BufReader<OwnedReadHalf>`/`OwnedWriteHalf` through admission -> result -> Ready -> one `browser_loop`
- no resplit, reconstruct, `into_inner`, second socket, accept-order pairing, or read-ahead loss

`RemoteScope` wire names:

`observe_human_control`, `profile_read`, `profile_create`, `profile_select`.

`InitialProfileSelection`:

- `default_profile` (string)
- `{"profile_id": "..."}` (object)

## Host catalog and membership

- `ProfileAccess { owner_device_id, members: BTreeSet<String> }` is the single Host-side membership source
- owner is non-empty and always a member; a private profile is `members == {owner_device_id}`
- legacy catalog entries without `access` refuse remote resolution with `PROFILE_ACCESS_MIGRATION_REQUIRED`
- `catalog.create(display_name, make_default, owner_device_id)` persists owner/members before returning
- `assign_access` returns `MembershipReceipt { catalog_revision, profile_id, device_id }`
- `remote_catalog(device_id)` filters public `ProfileRecord`s to profiles that grant the device

## Host admission and routing

- 0600 `remote-admission.sock` inside the existing 0700 `SocketDir`, removed on drop
- local `host.sock` stays Ready-first
- `remote_connection` splits once, reads one admission, writes one typed result, writes `Ready` only after `Ok`, then runs the shared `browser_loop`
- `conn_route`/`conn_principal` are Host-local typed control state; they never travel in browser payload/metadata
- every remote request re-reads the current registry and re-checks target profile access
- `FlushProfiles`, `CloseSession`, `Evaluate`, `Resize`, `ResumeAgent`, and Agent-mode `Attach` are remote-refused
- media rights require final `Attach`/rebind and exact session identity; the endpoint drops frames from other sessions

## Endpoint

- New required flag `--authorized-clients <authorized-clients.json>` (schema_version 2)
- mTLS peer DER SHA-256 must match the projection entry; otherwise the control connection is refused
- endpoint connects `host_dir/remote-admission.sock`, writes one admission, reads result, validates correlation, reads `Ready` from the same retained reader, validates `session_id`, then forwards raw `Request`/`Response`
- endpoint accepts `None`+`MALFORMED` only for its sole outstanding admission; mismatched `Some`, illegal `None`, or `MALFORMED` with `Some` are protocol errors with no `Ready`
- `FlushProfiles` stays endpoint-refused
- typed status receipt on stderr: `{ status: "ready", authorization_revision, registry_sha256, projection_sha256, endpoint_pid }`
- `projection_sha256` is the digest of the exact read buffer of `authorized-clients.json`, never a content field

## RP01-44 coverage (written, NOT_EXECUTED)

Real-process socket tests in `crates/obscura-host/tests/remote_admission_r1.rs`:

- RP-01..RP-05 `rp01`..`rp05`: remote catalog filter, remote create owner, select rebind, missing scope denial, two-profile storage isolation.
- RP-06 legacy access migration `rp_migration_legacy_catalog_without_access_refuses_remote_binding`: a catalog entry without `access` refuses remote binding with `PROFILE_ACCESS_MIGRATION_REQUIRED` while the local Ready-first catalog is unaffected.
- RP-07 other observers `rp07_other_observer_keeps_its_own_session_after_peer_select`: a peer rebind does not detach another observer's session or profile.
- RP-08 store/rebind failure is covered by the existing C2 public test `profile_replay_r2.rs` (`PROFILE_STORAGE_FAILED`), left unchanged.
- RP-09/RP-10/RP-15 `rp09`: unauthorized initial binding and reconnect denial, first frame is the typed denial so no `Ready`/`session_id` leaks.
- RP-11/RP-12 `rp11`/`rp12`: unbound initial selection, stale or revoked grant.
- RP-13/RP-14 `rp13`/`rp14`: two registered devices share one session; revoking the second keeps the owner.
- RP-16 real endpoint bootstrap success is covered by the existing `endpoint.rs` real mTLS test (catalog forwarded, create/select forwarded, FlushProfiles refused).
- RP-17..RP-27 `rp17`..`rp27`: reservation cleanup, failed Attach, local Ready-first, revision change, concurrent admissions, interleaved local accept, repeated admission, denial with no route, EOF before/after `Ok`/`Ready`.
- RP-28 failed Attach refusal is asserted in `rp18`; RP-29 revoked/expired after admission in `rp20` (revision) and `rp12` (revoked).
- RP-30 `rp30`: shutdown removes all three owned sockets.
- RP-31..RP-37 `rp31`..`rp37`: coalesced, fragmented, malformed newline, structurally invalid, oversized no delimiter, partial frame at EOF, empty EOF.
- RP-38 `rp38`, RP-42 `rp42`, RP-43 `rp43`, RP-44 `rp44`: real `obscura-endpoint` binary against a controlled Host admission socket for missing result, mismatched `Some(id)`, illegal `None`, legitimate `None`/`MALFORMED`.
- RP-39 decoded denial keeps `Some(id)`: asserted by `expect_denial` in `rp04`/`rp09`/`rp11`/`rp12`/`rp24`.
- RP-40/RP-41 `rp40`/`rp41`: failing stream isolation and newline at `MAX + 1`.

Every assertion is a real socket exchange or a persisted catalog read. No test
mocks an internal function or asserts on source text.

## Preserved risks and gaps

1. Host multi-session media:
   `crates/obscura-media/src/main.rs` enforces one session per media-adapter connection and the endpoint spawns exactly one adapter bound to `frames.sock`; Host `frames.sock` serves only `presented`. A remote peer attached to a non-presented profile receives no frames (endpoint-side exact-session gate drops them) instead of another profile's frames. Smallest fix is per-session `frames.sock` routing or an adapter session selector in the read-only `obscura-media` ABI. Owner: Media40859/controller.
2. `Cargo.lock`:
   `crates/obscura-host/Cargo.toml` adds `serde` and `sha2` (explicitly allowed), but the lockfile's `obscura-host` dependency array does not yet list them. The prepared `--locked` commands will refuse until an owner refreshes that entry. Smallest fix: regenerate the lock for the `obscura-host` package during the future build grant. `Cargo.lock` was read-only for this task.
3. RP tests are written but not executed. Source correctness by inspection is not a build result.

## Future granted commands (NOT executed here)

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --locked --release --features render -p obscura-host -p obscura-browser -p obscura-host-protocol
cargo nextest run --release --features render --no-fail-fast
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-cli --bins --features render
```

Obstacle course in `/Volumes/Intel/playground/obscura-benchmark/m1-io-serial-repair-20261002` (expected 33/33), and the paired BB01/09/10/11/13 stack are controller-scheduled, not run here.

## NOT_EXECUTED

- cargo/rustc/nextest/build/test runner
- target read/write, second V8 cache, candidate binary, listener/browser
- install/restart/device/OTA/shared keys/profile
- live crypto/network/socket consumer beyond source inspection
- obstacle course and paired product E2E
