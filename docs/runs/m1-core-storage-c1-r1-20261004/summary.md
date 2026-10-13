# Core C1 storage product repair summary

Status: `AUTHOR_SCOPE_VERIFIED / COMBINED_E2E_WAIT`.

Scope: only the admitted C1 contract. No parent transcript, resume, fork,
children, Collab, goal, subscription, self-review, merge, push, install,
restart of a formal service, device, profile, credential, or production change.

## Frozen inputs

| Item | Value |
| --- | --- |
| Owned worktree | `/Volumes/Intel/playground/obscura/m1-core-storage-c1-r1-20261004` |
| Branch | `codex/m1-core-storage-c1-r1-20261004` |
| Frozen base commit | `a40d1bb626831cb030b945758f83be217b1e78d2` |
| Frozen base tree | `f8001162ec6e0cfc990b62dabce345e77931d5cc` |
| OB `fork/main` | `948f054b51af442306be046d0014cfa3da628f27` (ancestor of the base) |
| Granted target | `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` |
| Frozen Host sha256 | `e78bbc25cc27e0b639f48c4c0fe2446f82823f98b5f78cbe48418c8ffaa70539` |
| Candidate Host sha256 | `d751b403fd5f1b5fc9180b09c8f8e36db56716b95ca01e749d4fac508c550b4f` |
| Frozen CLI sha256 | `8e5240ee2966216c53345eae66135075a949acbde9a0ed8e7b52d812e183779b` |

## Changes

Product (C1-owned only):

- `crates/obscura-js/src/ops.rs`: `op_local_storage` now returns
  `Option<String>`. With no profile backend `load` returns `None` (JS `null`);
  with a backend it returns `Some(snapshot)`, so a valid empty backend snapshot
  (`"{}"`) is distinct from absence. Present-backend `set`/`remove`/`clear`
  keep their effect and `Some("")` receipt. No empty-string sentinel, fallback,
  or second store.
- `crates/obscura-browser/src/context.rs`: `SharedLocalStorage::flush` holds the
  owner lock, writes every authoritative origin snapshot, then removes owned
  `<hex-origin>.json` files that are no longer authoritative. A failed write or
  removal returns the existing typed `PROFILE_STORAGE_FAILED` and stops the
  flush; unrelated files and the storage root are preserved. No journal or
  output-layer fix.

Tests (owned paths only):

- `crates/obscura-js/tests/local_storage_public_r2.rs`: public
  `set_local_storage`/`run_page_init`/`evaluate` set/get/remove/clear,
  hydration, and backend side effects.
- `crates/obscura-browser/tests/storage_clear_r2.rs`: authoritative write,
  clear-then-flush stale-file removal, cold-reload no-resurrection, unrelated
  file/root preservation, and typed `PROFILE_STORAGE_FAILED` on write and
  removal failure with recoverable state.

## Verification evidence

| Gate | Result | Raw |
| --- | --- | --- |
| Scoped nextest (`-p obscura-js --test local_storage_public_r2 -p obscura-browser --test storage_clear_r2`) | Exit `0`; 6/6 passed | `raw/scoped-nextest.{exit,stderr}` |
| Candidate Host build (`-p obscura-host --bin obscura-host --features render`) | Exit `0` | `raw/candidate-host-build.{exit,stderr}` |
| F1 storage-clear-restart (candidate Host, repaired probe) | Exit `0`; stale snapshot absent; cold `getItem` null; `resurrection_observed=false` | `raw/f1-storage-clear-restart.{exit,stdout,stderr}` |
| Copied decode constructor consumer | Exit `1`; `ABSENT_PHASE=FAIL`, `VALID_EMPTY_PHASE=PASS`, `SUMMARY=FAIL` | `raw/constructor-run.{exit,stdout,stderr}` |
| Consumer decoder-negative tests | Exit `0`; 3/3 labelled failures | `raw/consumer-decoder-tests.{exit,stderr}` |

The storage-failure consumer (`storage_clear_r2.rs`) drives the public
`SharedLocalStorage` boundary from an external test crate and shows the
explicit `PROFILE_STORAGE_FAILED` error, no success receipt, and a recoverable
in-memory state.

## Honest gaps (not claimed)

- F7 (`ObscuraJsRuntime::new()` with no backend keeps a standalone in-memory
  store distinct from a valid empty backend) stays RED at the public JS
  boundary: the C3-owned `bootstrap.js` mirror still treats a `null` load as an
  empty snapshot and clears the standalone store. C1 makes the Rust
  `None`/`Some("{}")` distinction real, but the copied consumer shows
  `ABSENT_PHASE=FAIL` until C3 lands the matching mirror. Reported as
  `COMBINED_E2E_WAIT`; no fake green.
- Full release workspace nextest, the exact CLI release build, and the 33-stage
  obstacle course remain mandatory for the completed Core combination and are
  root-assigned to the combined verification/review author. The granted lease
  does not permit CLI/full-workspace/Media builds, so they were not run here.
- No independent architecture review, install, merge, push, or OTA.

## Resource receipt

- Probe Host PIDs `62570` and `62781` were stopped by explicit `SIGTERM`; their
  `/tmp/m1r3sock-*` socket roots and the `/tmp/m1c1f1-20261004` work root were
  removed. Two unrelated `obscura-host` processes (`33820`, `95552`) from other
  playgrounds were left untouched.
- The candidate Host replaced the frozen Host binary in the granted target; the
  before/after digests are recorded. No target copy, cache delete, or second V8
  target was created.
