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

## Open terminal

- Product runtime install / user black-box / device E2E: not permitted this task.
- Independent architecture review / merge / push / OTA: reserved to controller after author verification.
