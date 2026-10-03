# M1 W5 resident H.264 internal encoder notes

状态: `BASELINE_LOCKED`.

范围: 只实现 `crates/obscura-media` 内部 resident H.264 边界与公开 consumer。
Host、公共 protocol、main.rs、webrtc.rs、endpoint、client/Android/Mac、共享
AGENTS、全仓 format/deps 只读或未使用。不注册 Collab/goal/订阅，不做 review/
merge/push/安装/设备/daemon 操作。

## 基线

| 项 | 值 | 证据 |
| --- | --- | --- |
| worktree | `/Volumes/Intel/playground/obscura/m1-media-resident-w5-20261003` | 当前目录 |
| branch | `codex/m1-media-resident-w5-20261003` | `git branch --show-current` |
| base HEAD | `948f054b51af442306be046d0014cfa3da628f27` | `git rev-parse HEAD` |
| base 状态 | clean | `git status --porcelain` 空 |
| 设计合同 | `docs/m1/media-session-contract.md`，design `dbb092cf...` oauth/gpt-6.1-sol PASS | 主控只读合同 |
| 能力源 | `docs/runs/m1-20261002/w5cr-source/probe.mm` | sha256 `cb64807b8bcb1c48b9c0ae6faede21d0760ea86065868530b5afee7da7405a21` |
| m1-video 图 | sha256 `172c697027c44a308e5168262d9aac9d24447ea91cbcf73a9f1382ca14f64d89` | `shasum -a 256` |
| m1-quality 图 | sha256 `1f5ac33d2c058e35d89cbcd0d824025add32e4ee242b2f720ad44326d1eacb6f` | `shasum -a 256` |
| 环境 | macOS 26.6.2 arm64; cargo 1.97.1; nextest 0.9.143; ffmpeg 8.0.1; Apple clang 17.0.0 | `sw_vers`, `uname -m`, `--version` |
| 独占 target | `<worktree>/.w5-resident-target` | 合同 |
| 实验输出 | `<worktree>/.w5-resident-tmp/` | 合同 |

MCPX 先查：`runtime_read(view=capabilities)` 可用，`workspace` 未注册本 worktree
（仅有 `m1-*` 旧 playground 与其它项目）。按合同 `scope 不可用按 skill 短 CLI 记录`，
不注册新 workspace、不借共享 workspace、不重启 MCPX；Git/构建用短 CLI 执行并在交付
记录该事实。

## 阶段记录

| 时间/节点 | 结论/状态 | 来源/证据 | 输入版本及必要环境 | 下一步 |
| --- | --- | --- | --- | --- |
| 2026-10-03 / 入口与边界 | 只执行合同；产品只读；不注册 Collab/goal/订阅；不做 review/merge/push/安装/daemon | 合同；全局/项目 `AGENTS.md`；`coding-principals`；`codex-orchestrator`；`mcpx-commit` | base `948f054`; macOS 26.6.2 arm64 | 读能力源与现有 owner，写 red consumer |
| 2026-10-03 / 图与能力核对 | m1-video/m1-quality 两图 hash 与合同一致；能力源 hash 一致；现有 `encode()` 是唯一逐帧 FFmpeg owner，`keyframe` 恒 true | `shasum -a 256`；`crates/obscura-media/src/{lib.rs,main.rs}` | base `948f054` | 复现旧逐帧限制并记录 red |
