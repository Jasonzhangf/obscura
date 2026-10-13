# M1 W5 Media review-fix r4 notes

Status: `INTERNAL_FIX_IN_PROGRESS`; full repository gate not run.

Scope: only `crates/obscura-media` resident encoder, its public example, and
this task's notes/delivery. No Host, protocol, caller, endpoint, client,
install, review, merge, push, Collab, goal, child, or subscription work.

## Baseline

| Item | Value | Evidence |
| --- | --- | --- |
| Worktree | `/Volumes/Intel/playground/obscura/m1-media-review-fix-r4-20261003` | `pwd` |
| Branch | `codex/m1-media-review-fix-r4-20261003` | `git status --short --branch` |
| HEAD | `138bd4242cba7f71ddd703c77017ccabcc81fe6d` | `git rev-parse HEAD` |
| Restored product files | `lib.rs`, `encoder.rs`, `videotoolbox.rs`, `examples/resident-encoder.rs` | `git status --short` |
| Restored-source equivalence | Four files byte-equal to r3 terminal worktree; `cmp` exit 0 each | `cmp`; SHA256 in r4 notes below |
| r3 terminal patch SHA256 | `45af439f69f0e9ebfb2fc6401bd033493b7ad63752e5088b539b24cc8690650c` | `shasum -a 256` on controller patch |
| r3 green4 evidence | 57 checks, 55 PASS / 2 FAIL; `recovery_raw_latest_wins_dropped` drops=0 produced=41 submitted=41 | `/Volumes/Intel/playground/obscura/m1-media-review-fix-r3-20261003/.w5-r3-tmp/consumer-green4.log` |
| r3 final log warning | 57/0 exists but was obtained after a timing-dependent burst change; r4 contract rejects treating it as congestion proof | r4 contract; r3 final log |
| Design contract | Controller `docs/m1/media-session-contract.md` | read-only controller source |
| Design admission | Controller `docs/m1/media-design-admission.md` | read-only controller source |
| Video graph | Controller `docs/dagpipe/graphs/m1-video.graph.json` | read-only controller source |
| Quality graph | Controller `docs/dagpipe/graphs/m1-quality.graph.json` | read-only controller source |
| Cache lease | `/Volumes/Intel/playground/obscura/m1-media-resident-w5-20261003/.w5-resident-target` | r4 contract |
| Task tmp | `/Volumes/Intel/playground/obscura/m1-media-review-fix-r4-20261003/.w5-r4-tmp` | r4 contract |

Restored source SHA256:

| File | SHA256 |
| --- | --- |
| `crates/obscura-media/src/lib.rs` | `0c1f2c00162c7d00037a96c7adf281258fd7d03ef453500389c7185e9b962dac` |
| `crates/obscura-media/src/encoder.rs` | `c9c1aeba0d75b32a9b1d30427e036a816e21c21b9e51a68546270986b4a3c2a0` |
| `crates/obscura-media/src/videotoolbox.rs` | `5c6ddaec07b16ba304c82341d1b90a33104c4a3aaf141f53800f5a26ef58657e` |
| `crates/obscura-media/examples/resident-encoder.rs` | `6ca9526498834966190812d43e82c959aae4f9a1c93922b4a84917fe2a962149` |

## Stage Record

| Time / node | Conclusion or state | Evidence | Input and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-03 / entry | Contract, r3 contract, project `AGENTS.md`, r2 notes/delivery, review findings, design contract/admission, and both graphs read. No patch, cherry-pick, resume, or fork applied. | absolute reads; `git status`; `cmp` | HEAD `138bd42`; dirty four restored files; macOS | Bind controller hashes and graph validation, then reproduce r3 green4 red |
| 2026-10-03 / restored source | Four restored files byte-equal to old r3 terminal source; current dirty input is authorized and not another writer. | `cmp` exit 0 for each; SHA256 above | r4 worktree | Inspect current overflow/apply/FPS/recovery control flow |
| 2026-10-03 / first deviation | r3 green4 failure is a consumer-load issue: 40/64-frame burst did not deterministically fill raw+in-flight capacity, so drops=0. Source-level overflow separation already exists, but recovery proof remains timing-dependent. | r3 green4 log; current `resident_dependency_recovery` source | restored source | Construct bounded real congestion through public API, then force IDR and FFmpeg decode |

## Open Terminal

- Full gate is `FULL_GATE_WAIT`: workspace release nextest, exact CLI release
  build, and 33/33 obstacle course require the controller's serialized W2 cache
  window. This contract does not claim a full repository PASS.
