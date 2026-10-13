# Core/Profile/Media/MCP combined r3 run notes

Status: `IN_PROGRESS`. Fresh GCM product/verification successor. No child,
Collab, goal, subscription, or review. Hooks preserved (no `--no-verify`).

## Identity

| Item | Value |
| --- | --- |
| Worktree | `/Volumes/Intel/playground/obscura/m1-core-profile-media-mcp-combined-r3-20261004` |
| Branch | `codex/m1-core-profile-media-mcp-combined-r3-20261004` |
| Base main | `948f054b51af442306be046d0014cfa3da628f27` |
| Combined source | `4f06467cd8606be9657f2be5fc33efe054a5d549` / tree `f44826fb9b47542166d8aac9e1f6808e317228f4` |
| Shared target (sole writer) | `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` |
| Task tmp | `<worktree>/.combined-r3-tmp/` |
| Frozen patch | `/Volumes/Intel/playground/agentbrowser/m1-repair-model-20261002/docs/runs/m1-20261002/core-r2-resident-frameinfo.patch` |
| Frozen patch SHA256 | `1e6fb2a8ecafa75652ae0c21dabf8036eb099adec313885b2d8e518eb64e2f76` |

Environment: `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
`OBSCURA_TEST_BINARY_DIR=<target>/release`, macOS 26.6.2 arm64,
rustc/cargo 1.97.1, nextest 0.9.143, ffmpeg 8.0.1.

## Stage record

| Time / node | Conclusion or state | Evidence | Input and environment | Next |
| --- | --- | --- | --- | --- |
| 2026-10-03 / entry | Fresh worktree clean at `4f06467`; r2 evidence read; frozen patch hash read and matched; target lease owner = this task. | `git status`, `git rev-parse`, `shasum`, `ob-shared-target-lease.json` | HEAD `4f06467`; clean | Apply frozen 2-line example patch |
| 2026-10-03 / patch | Applied frozen patch via `apply_patch`; diff equals frozen patch modulo `index` line. | `git diff`; `diff` vs frozen | clean tree | Run Linux check, consumer, full gate, artifact, obstacle |
| 2026-10-03 / Linux check | `cargo check --locked --release -p obscura-media --target x86_64-unknown-linux-gnu --lib --examples` exit 0, warnings only. | `.combined-r3-tmp/evidence/linux-check.{stdout,stderr,exit}` | target installed; shared target | Build+run resident consumer |
| 2026-10-03 / consumer | Built example exit 0 and ran real public consumer: 71 checks, 0 failed, exit 0; all 7 emitted H.264 streams decode with FFmpeg exit 0, 0 stderr. | `resident-consumer.*`, `ffmpeg-decode.report`, `ffmpeg-frames.report`, `resident-streams.sha256` | macOS arm64; ffmpeg 8.0.1 | Run full workspace gate |
| 2026-10-03 / full gate | `cargo nextest run -j4 --locked --release --features render --no-fail-fast` exit 0; Summary `1705 tests run: 1705 passed, 4 skipped`. r2's compile-101 E0063 is resolved by the frozen patch. | `.combined-r3-tmp/evidence/full-nextest.{stdout,stderr,exit}` | shared target; render feature | Focused Host/MCP, rebuild artifacts, obstacle |
| 2026-10-03 / focused | Host/browser/protocol: 132 passed, 0 skipped, exit 0. MCP `mcp_client`: 19 passed, 0 skipped, exit 0. Both re-run on this r3 source (not only reused). | `focused-host.*`, `focused-mcp.*` | render feature | Rebuild binaries and hash |
| 2026-10-03 / rebuild | Host (render), CLI (render), Media (webrtc) bins all build exit 0. Artifact SHA256 (see `rebuilt-artifacts.sha256`): host `e78bbc25...`, cli `8e5240ee...`, media `a7a65f21...`, receiver `5b6ae487...`. CLI/media/receiver match r2; host differs (path-dependent build strings). | `build-{host,cli,media}.{stdout,stderr,exit}`, `rebuilt-artifacts.{sha256,stat}` | render + webrtc features | Obstacle course |
| 2026-10-03 / obstacle | `OBSCURA_BIN=<target>/release/obscura python3 run.py --runs 1 --warmup 0` at benchmark HEAD `8bd11ade...` exit 0; `correctness: 33/33 stages passed`. Fixture read-only, unmodified. | `.combined-r3-tmp/evidence/obstacle-course.{stdout,stderr,exit}` | released obscura bin | Commit allowlist, final receipt |
| 2026-10-03 / commit | Allowlist commit `f618720559e85fd51735a95cdfc0430d7ca8fc75`, tree `e6d49a4ac7c5fd86bdc08bdb19ed45cedddfc164`. Only the example and this notes file staged; hooks intact (no bypass). Committed example blob = compiled blob `d316ca0`. | `commit.log`, `git rev-parse`, `patch-binding.txt` | branches; `.combined-r3-tmp/` left untracked | Final receipt |

## Candidate binding

| Item | Value |
| --- | --- |
| Candidate commit | `f618720559e85fd51735a95cdfc0430d7ca8fc75` |
| Candidate tree | `e6d49a4ac7c5fd86bdc08bdb19ed45cedddfc164` |
| Branch HEAD | this notes commit; product candidate is `f618720` (see below) |
| Source base | `4f06467cd8606be9657f2be5fc33efe054a5d549` / `f44826fb9b47542166d8aac9e1f6808e317228f4` |
| Example compile blob | `d316ca0175dde97479538082d529691a7286af96` (== frozen patch `index` new) |
| Protocol `protocol/browser/src/lib.rs` SHA256 | `2344ad88b52e34af1e8d6ccbd6de998770bbeca080e6ec57cd0e047aeb6edab9` |
| Protocol versions | `HOST_PROTOCOL_VERSION = 11`, `WEBRTC_PROTOCOL_VERSION = 10` |

All r3 gates ran against the same worktree whose example file hashes to
`835f5110c12ad3d5fe2ab815f2c2ab659d7b507ece9ddc4b7b2bf4b8b2a500d3`
(git blob `d316ca0`), which is the exact blob in candidate commit `f618720`.
Only this notes file differs between the verified worktree and the commit
(the notes were written during the run, example/product source unchanged).
The follow-up on top of `f618720` changes only this notes file; the product
source is identical to candidate `f618720`.

## Evidence index (`.combined-r3-tmp/evidence/`)

| Gate | Result | Files |
| --- | --- | --- |
| Frozen patch identity | matches | `patch-binding.txt` |
| Linux cross-check | exit 0 | `linux-check.{stdout,stderr,exit}` |
| Resident consumer | 71 checks / 0 failed, exit 0 | `resident-consumer.{stdout,stderr,exit,outdir}` |
| Consumer FFmpeg decode | 7/7 streams exit 0, 0 stderr | `ffmpeg-decode.report`, `ffmpeg-frames.report`, `resident-streams.sha256` |
| Full workspace gate | 1705 passed, 4 skipped, exit 0 | `full-nextest.{stdout,stderr,exit}` |
| Focused Host/browser/protocol | 132 passed, 0 skipped, exit 0 | `focused-host.*` |
| Focused MCP | 19 passed, 0 skipped, exit 0 | `focused-mcp.*` |
| Artifact rebuild | host/cli/media exit 0 | `build-{host,cli,media}.*`, `rebuilt-artifacts.{sha256,stat}` |
| Obstacle course | 33/33, exit 0 | `obstacle-course.{stdout,stderr,exit}` |
| Protocol | hash + versions | `protocol.sha256`, `protocol-versions.txt` |

## Reuse vs. invalidation

Reused (source/deps unchanged; r2 evidence binds to `4f06467`, this run binds to
the same combined source plus one example-only field adaptation):

- W2/profile source equivalence and protocol owner (profile source unchanged).
- r2 Media green `7 passed / 0 skipped` on `obscura-media`; reproduced here
  transitively by the example compile + 71/71 consumer run.

Invalidated / re-run this task:

- r2 artifact hashes: rebuilt here. CLI `8e5240ee...`, media `a7a65f21...`,
  receiver `5b6ae487...` match r2; host `e78bbc25...` differs from r2's
  `2b25b75e...` (path-dependent build strings).
- r2 full gate (compile 101, no run): re-run here -> 1705 passed / 4 skipped.
- r2 focused Host/MCP: re-run here on r3 source -> 132/19.
- Consumer + independent FFmpeg decode: not evidenced in r2; run fresh here.
- Obstacle course: run fresh here.

## Resource receipt

- Worktree: `<worktree>` (this task, retained for controller).
- Shared target: `<target>`, sole writer, `ob-shared-target-lease.json` owner =
  this task; not released (controller retains cache for successors).
- Task tmp: `.combined-r3-tmp/` (evidence + `resident-out-*/` streams), untracked,
  retained as this task's evidence.
- PIDs/sockets/profile roots: the example and consumer spawn no long-lived
  server; only short-lived ffmpeg child processes, all exited.
- No product/runtime/device install; no system IME/BB evidence.

## Open terminal

- Product runtime install / user black-box / device E2E: not permitted this task.
- Independent architecture review / merge / push / OTA: reserved to controller after author verification.
