# P3 host implementation r10: five P1 findings closure report

Status: **COMPLETE for the r10 node**. The five P1 findings G1-G5 from the r3
implementation review of `9be2f0826b6d90ad44838420cd27f4b3864fe6f4` are fixed,
each with a real public-surface discriminating before/after pair. The focused
suites, the three release builds, the CLI dependency check, and two full
workspace gates are green. No commit, merge, install, restart, OTA, device, or
runtime operation was performed.

## Identity

- Task: `m1-host-p3-implementation-r10-20261005`
- Worker: `gcm-host-p3-r10` (fresh standalone GCM author, not controller or
  reviewer)
- Worktree: `/Volumes/Intel/playground/obscura/m1-host-p3-implementation-20261005`
- Branch: `codex/m1-host-p3-implementation-20261005`
- Start source HEAD: `9be2f0826b6d90ad44838420cd27f4b3864fe6f4`
- Start tree: `bcd4b99f4627f8257b4dae0a18af0439bab04e54`
- Task base: `9d02931f24631e3680b3038601f4e3efe288c0ab`
- End source HEAD: `9be2f0826b6d90ad44838420cd27f4b3864fe6f4` (unchanged; the
  delta is an uncommitted working tree)
- End tree (product source + run docs applied over HEAD, computed with a
  temporary index): `5d09df94a0c5428d230b3d5945e2a8234ed36fc7`. The product-source
  subset (`crates/**` + `Cargo.toml` + `Cargo.lock`) is
  `8456479543ebb3308efc42d06b58137610c86428`.
- Sole target: `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`
- Env: `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
  `OBSCURA_TEST_BINARY_DIR=<target>/debug`,
  `OBSCURA_WEBRTC_RAW_DIR=<record>/raw/webrtc`

## Per-finding verdicts

| Finding | Owner and fix | Public before/after pair | Before | After | Verdict |
| --- | --- | --- | --- | --- | --- |
| G1 unsettled permit must retain resource owner and failure | `crates/obscura-host/src/endpoint/server.rs` + `crates/obscura-host/src/endpoint/channels.rs`: completion state carries the finalization result; a failed release retains the registry entry, durable owner, live adapter, session directory and control path, and shutdown re-surfaces the per-session failure | `obscura-host::blackbox_b9_unsettled_permit::unsettled_encoder_permit_is_an_explicit_terminal` | EXIT=100 `G1: the failed release dropped the durable owner and SIGKILLed the retained adapter` | EXIT=0 | CLOSED |
| G2 cancellation during registration | `crates/obscura-media/src/encoder_owner.rs`: `RegistrationState::{Unregistered,Pending,Armed,Refused}` records `Pending` before the `Register` write, so an interrupt in the unread-ack window resolves the typed acknowledgement and revokes before reaping | `obscura-media::registration_blackbox::standalone_adapter_resolves_pending_registration_before_reaping` | EXIT=100 (no `RevokeEncoder` reached the endpoint) | EXIT=0 | CLOSED |
| G3 revoke acknowledgement before reaping | `crates/obscura-media/src/encoder_owner.rs`: the registered path requires the typed non-refused `Revoke` acknowledgement; `REVOKE_ACK_TIMEOUT` = 10s covers the endpoint's 2s SIGTERM + 2s SIGKILL signal authority | `obscura-media::registration_blackbox::standalone_adapter_requires_revoke_ack_before_reaping_registered_leader` | EXIT=100 (leader reaped while the ack was withheld) | EXIT=0 | CLOSED |
| G4 terminal encode cleanup after reap | `crates/obscura-media/src/encoder_owner.rs`: `run_encode` settles the group through `stop_kill_reap` while the leader identity is pinned and `reaped_status` retires group/child/registration, so no group signal addresses a reused pgid after `child.wait()` | `obscura-media::cleanup_blackbox::standalone_adapter_retires_group_authority_before_reaping_nonzero_encoder` and `..._empty_encoder_output` | EXIT=100 (observer marker `gone`) | EXIT=0 (marker `present`) | CLOSED |
| G5 adapter cancellation endpoint while idle | `crates/obscura-media/src/main.rs`: the header read, payload read, encode and output send all race the lifetime SIGTERM/ctrl-c receiver; interruption finalizes only a live child and exits so the control-socket guard removes the path | `obscura-media::cleanup_blackbox::standalone_adapter_exits_on_sigterm_while_waiting_for_frames` | EXIT=100 (idle adapter ignored SIGTERM) | EXIT=0 | CLOSED |

## Changed paths and per-file delta sha256

| Path | sha256 |
| --- | --- |
| `crates/obscura-host/src/endpoint/server.rs` | `9ca032c0d87df83d2b8ead12e322f5359820edf5008a7296cb4da3da406859e0` |
| `crates/obscura-host/src/endpoint/channels.rs` | `2b5ac717aacbd3f490c852d997bccc620f67c6369f09bfb4c59de3dc6590ca40` |
| `crates/obscura-host/tests/blackbox_b9_unsettled_permit.rs` | `a7501428ffb954cfe1e72b0c2e2e4a825114635649850b2b0695d5e5b5f2f5ff` |
| `crates/obscura-media/src/encoder_owner.rs` | `6b425e5d086138bd2d52a5b35bed91b5813a3e441bf776bbc7ea5936f5bc9f9f` |
| `crates/obscura-media/src/main.rs` | `216213861b0a7ebf50940ffc9460ffcf7847f2fd8c255a0d76dcd298e50dc1a0` |
| `crates/obscura-media/tests/cleanup_blackbox.rs` | `3183165d444a34b07cd1c8de3b2e7f3e907c58bc15e8b5726016c30204bf5ed1` |
| `crates/obscura-media/tests/encoder.rs` | `a41d79f96175f86eda152ff3b87cae8c170c713c7da5598ba4d5d58420637fc9` |
| `crates/obscura-media/tests/registration_blackbox.rs` (new) | `c4d16e655578943e7fbde3c5ef5b7b9db6395aa5c356964c283c40efffc5dc4f` |

`protocol/browser/src/lib.rs` is untouched; `HOST_PROTOCOL_VERSION` remains 11.
No `Cargo.toml`/`Cargo.lock` change was needed.

## Full gate results

- `cargo nextest run -p obscura-host --features render -j 4 --no-fail-fast`:
  122 passed, 0 skipped, EXIT=0 (`raw/a5-host-focused.*`).
- `cargo nextest run -p obscura-media --features webrtc -j 4 --no-fail-fast`:
  29 passed, 0 skipped, EXIT=0 (`raw/a5-media-focused.*`).
- Release builds: media `--features webrtc` EXIT=0; host `--bins` EXIT=0; cli
  `--bins --features render` EXIT=0 (`raw/a6-build-*`).
- `cargo nextest run --workspace --features render -j 4 --no-fail-fast`:
  run 1 `1822 passed, 4 skipped` EXIT=0; run 2 `1822 passed, 4 skipped` EXIT=0
  (`raw/a7-full-gate-1.*`, `raw/a7-full-gate-2.*`). The r9 baseline was
  `1816 passed, 4 skipped`; the increase is the new G2/G4/G5 regressions.
- `cargo tree -p obscura-cli --features render --locked`: EXIT=0, zero
  `obscura-host`/`obscura-media` edges (`raw/a8-cli-tree.*`).
- `git diff --check`: EXIT=0 (`raw/a8-diff-check.*`).

One earlier full-gate run is retained red:
`raw/a7-full-gate-1-red-continuous-timeout.*` (EXIT=100), where
`webrtc_endpoint::real_receiver_gets_continuous_host_media_and_rejects_stale_binding`
hit its hard 180s fixture timeout under load. It passes isolated in 15.5s
(`raw/probe-continuous-iso.*`) and in both green gates, so it is a load-induced
fixture timeout, not a product regression. The red log is preserved.

## Disclosures

- The `before` runs revert only the named finding's fix in the working tree and
  restore the fixed source byte-for-byte afterwards, verified by sha256. Before
  and after used the same tests, target and environment.
- The `webrtc_endpoint` integration fixture defaults its raw output root into the
  protected untracked `docs/runs/m1-frame-forwarding-impl-r1-20261005/` tree.
  This task's first host focused run and first full gate wrote 37 UUID fixture
  directories there before `OBSCURA_WEBRTC_RAW_DIR` was set. The exact list is
  recorded in `gcm-host-p3-r10/raw/protected-dir-contamination.list`; only those
  task-owned directories were removed, restoring the tree to its 70-entry
  pre-task state. No pre-existing entry was removed. All later runs used
  `OBSCURA_WEBRTC_RAW_DIR` under the worker record.
- Resource cleanup: this task's five `/tmp/ob9-*` fixture roots and the one
  `/tmp/ow-*` root left by the timed-out gate run were removed with exact-path
  `find -delete` (list: `gcm-host-p3-r10/raw/r10-fixture-dirs-cleanup.list`). No
  other owner's `/tmp` resources or processes were touched.
- The r9 `report.md`/`result.json` were superseded by this r10 pair; the r9
  content remains in git history at `9be2f082` and in the r9 notes section.

## What the controller must do next

1. Re-run the independent implementation review on the r10 working tree at end
   tree `5d09df94a0c5428d230b3d5945e2a8234ed36fc7` (product-source tree
   `8456479543ebb3308efc42d06b58137610c86428`).
2. On PASS, commit, merge to clean `main`, push to `origin/main`, and verify the
   remote receipt. No install/restart/OTA is in scope for this node.
