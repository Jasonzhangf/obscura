# m1-core-review-public-red-r1 run notes

Contract: fresh standalone GCM diagnostic/test author. No product edits.
Return channel is original exec JSON/final. Owned cwd is
`/Volumes/Intel/playground/obscura/m1-core-review-public-red-r1-20261004`.

## D0 baseline bindings

- Source HEAD: `c10e1ea864bdfa9d9e545507ea80913f95bd0249`
- Source tree: `4387643ac5b66e60e491ae60194641bdd1b13947`
- Branch: `codex/m1-core-review-public-red-r1-20261004`
- fork/main base: `948f054b51af442306be046d0014cfa3da628f27`
- Target: `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`
- Grant: typed lease `m1-core-review-public-red-r1-20261004`, sole test-only consumer.
- Target Host SHA256: `b2fb8b7da88d51e937745861a237aab4e7903324f21fb36f7d20481785215c72`
  (pre-build baseline; after the scoped rebuild the artifact is
  `ba566a3a078f1793299e5e6a638ebde0ea33303b3688ca65347e0543da785f37`)
- Target Worker SHA256: `794208844d717b1f77920b0660d8e5787baf9ee08506efd44c595186dce53743`

Read global AGENTS, project AGENTS, coding-principals SKILL/references/debug.md,
codex-orchestrator worker-contract.md, controller notes/execution-plan/review/
graphs/interaction model, review.final.md, and existing Host public tests.

## P1 hypotheses from review

H1: explicit `Detach` calls `Session::detach` directly, deleting attachment and
operation record while an input lease stays open. Later EOF cannot schedule
Engine terminal cleanup. Public effect: lease remains, new input gets
`INPUT_LEASE_BUSY`, takeover stays `waiting`.

H2: Evaluate refreshes editor context only for `Ok(value)` when `thrown` is
false. A script that mutates focused input then throws returns
`JAVASCRIPT_EXCEPTION` but Host Status retains the old editor revision, so the
next edit with the advertised binding fails with `STALE_EDITOR_REVISION`.

## Test additions

Added exact-named public cases:

1. `evaluate_mutation_exception_publishes_current_editor_context`
   in `crates/obscura-host/tests/editor_context_public_c2.rs`.
2. `explicit_detach_cancels_composition_before_handover`
   in `crates/obscura-host/tests/lease_cancel_r2.rs`.
3. `explicit_detach_cancels_gesture_before_handover`
   in `crates/obscura-host/tests/lease_cancel_r2.rs`.

Each uses the real Host child, Unix `host.sock`, Ready/Attach/Status, and typed
browser operations through the existing Daemon/Client helpers in those files.
No product code, protocol, or shared target/test binary is modified. Raw
evidence will be recorded in `raw/`.

## D1 build and targeted run

- Env: `CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`,
  `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
  `OBSCURA_TEST_BINARY_DIR=<target>/release`.
- Build: `cargo build --locked --release --features render -p obscura-host --bins`
  -> exit 0 (~1m29s).
- Run: `cargo nextest run --locked --release --features render -p obscura-host
  --test editor_context_public_c2 --test lease_cancel_r2 --no-fail-fast`
  -> exit 100 (intentional diagnostic red). Summary: 6 run, 3 passed, 3 failed.
- Bound artifacts after rebuild: Host
  `ba566a3a078f1793299e5e6a638ebde0ea33303b3688ca65347e0543da785f37`; Worker
  `794208844d717b1f77920b0660d8e5787baf9ee08506efd44c595186dce53743`.
- Raw logs: `raw/targeted-nextest.log`, `raw/targeted-nextest-r2.log`.

## D2 observations (first external deviation)

Green controls passed:
`evaluate_mutation_publishes_new_context_and_keeps_stale_binding_rejected`,
`disconnect_terminates_engine_composition_before_lease_release`,
`direct_same_session_composition_cancel_stays_green`.

- `explicit_detach_cancels_gesture_before_handover` (RED): after explicit
  `detach` while a gesture is open, new-attachment Status still shows
  `input_lease.kind=gesture` (`operation_id=lease-op-2`, retired
  `attachment_id=2`). New `pointer_down` returns `INPUT_LEASE_BUSY`; takeover
  stays `control.phase.type=waiting`.
- `explicit_detach_cancels_composition_before_handover` (RED): after explicit
  `detach` while composition is open, Status still shows
  `input_lease.kind=composition` and Engine retains
  `composition={"data":"中文","start":0,"end":2}` with `surrounding.text="中文seed"`.
  New `composition_begin` returns `INPUT_LEASE_BUSY`; takeover stays `waiting`.
- `evaluate_mutation_exception_publishes_current_editor_context` (RED): Evaluate
  correctly returns `JAVASCRIPT_EXCEPTION`, but Status publishes
  `editor_revision=0` with empty selection/surrounding. The next edit built from
  that advertised binding gets `STALE_EDITOR_REVISION`.

These are the first externally visible deviations from the public entrypoint.
No private state or source-string assertions were used.

## D3 product repair proposals (not implemented)

- Owner `crates/obscura-host/src/daemon.rs`: route explicit `Detach` through the
  same terminal scheduling used by disconnect before removing the attachment and
  operation record, or reject/queue Detach while an input lease is open.
- Owner `crates/obscura-host/src/worker.rs`: refresh Engine editor context for
  every completed `Ok` evaluation result, including `thrown=true`, while
  preserving the `JAVASCRIPT_EXCEPTION` response.
