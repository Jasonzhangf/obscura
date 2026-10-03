# M1 W5 resident H.264 recovery r2 notes

状态: `INTERNAL_MODULE_AUTHOR_VERIFIED`（内部库边界）；Android/Mac/Host/M1 用户面 UNVERIFIED。

范围: 只实现 `crates/obscura-media` 内部 resident H.264 边界与公开 consumer。
Host、公共 protocol、main.rs、webrtc.rs、endpoint、client/Android/Mac、共享
AGENTS、全仓 format/deps 只读或未使用。不注册 Collab/goal/订阅，不做 review/
merge/push/安装/设备/daemon 操作。无父 transcript，不 resume/fork。

## 基线（r2 覆盖原同名值）

| 项 | 值 | 证据 |
| --- | --- | --- |
| worktree | `/Volumes/Intel/playground/obscura/m1-media-resident-w5-r2-20261003` | `pwd` |
| branch | `codex/m1-media-resident-w5-r2-20261003` | `git branch --show-current` |
| base HEAD | `948f054b51af442306be046d0014cfa3da628f27` | `git rev-parse HEAD` |
| 恢复状态 | RESTORED / NOT_DISPATCHED，主控已 apply+解包，本轮不再次 apply/解包 | 合同；`git status` |
| patch hash | `aa93e3c4300be6d90410326645b5686cc710516be545ec1fea7a41fa79eb98f1` | `shasum -a 256 w5-interrupted-tracked.patch` |
| tar hash | `3900d1af9f6ed74a7e33ad0843be566f5ece13762b73a11884f49c8c92ddb9d1` | `shasum -a 256 w5-interrupted-new-source.tar.gz` |
| 产品源 sha256 | Cargo.toml `c1aafe65…`; lib.rs `221104ec…`; encoder.rs `6eb33f20…`; videotoolbox.rs `af978e2d…`; examples/resident-encoder.rs `d521f64a…` | `shasum -a 256`（恢复后首次绑定） |
| 旧 notes sha256 | `c165d0dd93a9b1cd8d81172815fc06a03abd81caf0faf8bfac5bd498dafec12a` | `shasum -a 256 docs/runs/m1-20261002/w5-resident-notes.md` |
| 设计合同 | `docs/m1/media-session-contract.md`，design `dbb092cf...` oauth/gpt-6.1-sol PASS | 主控只读合同；未变化，复用 |
| 能力源 | `w5cr-source/probe.mm` sha256 `cb64807b8bcb1c48b9c0ae6faede21d0760ea86065868530b5afee7da7405a21` | 合同；未变化，复用 |
| 环境 | macOS 26.6.2 arm64; cargo 1.97.1; nextest 0.9.143; ffmpeg 8.0.1; Apple clang 17.0.0 | `sw_vers`/`--version` |
| 独占 target | `/Volumes/Intel/playground/obscura/m1-media-resident-w5-20261003/.w5-resident-target`（lease，仅构建缓存可写） | 合同 |
| 新实验输出 | `<本 tree>/.w5-r2-tmp/` | 合同 |

MCPX 先查：`runtime_read(view=capabilities)` 可用，`workspace` 未注册本 worktree；
按合同 `scope 不可用按 skill 短 CLI 记录`，不注册新 workspace、不借共享 workspace、
不重启 MCPX；Git/构建用短 CLI 执行并在交付记录该事实。

## 阶段记录

| 时间/节点 | 结论/状态 | 来源/证据 | 输入版本及必要环境 | 下一步 |
| --- | --- | --- | --- | --- |
| 2026-10-03 03:44 / 入口与边界 | 只执行合同；产品只读；不注册 Collab/goal/订阅；不做 review/merge/push/安装/daemon；无新 writer（worktree 03:22 建立，本会话为唯一执行者） | 合同；全局/项目 `AGENTS.md`；`git status`/`git log` | base `948f054`; macOS 26.6.2 arm64 | 绑定恢复 hash，确认无输入漂移 |
| 2026-10-03 / 恢复 hash 绑定 | patch/tar hash 与合同一致；5 份产品源 + 旧 notes sha256 记录如上；tar 仅含 3 新源 + 旧 notes；未再次 apply/解包 | `shasum -a 256`；`tar -tzf` | 恢复后 tree | 复核现有 consumer 29 项断言与原始 bytes |
| 2026-10-03 / 现有 consumer 基线 | `cargo build --locked --release -p obscura-media --examples` exit0；consumer `RESULT passed=29 failed=0` exit0；`quality_actual_bytes_measured` 仅断言 sample 数量，非真实高低差异；依赖恢复/关闭 drain/callback 寿命无独立断言 | `.w5-r2-tmp/build-examples.log`; `.w5-r2-tmp/consumer-baseline.log`; `.w5-r2-tmp/consumer-baseline/quality-bytes.txt` | 恢复后 tree; 独占 target | 用 apply_patch 改 consumer 语义：真实码率测量 + 依赖恢复 + 关闭/drain + callback 寿命 |
| 2026-10-03 / consumer 语义收口 | apply_patch 改 `examples/resident-encoder.rs`（唯一改动产品文件）：低/高码率改同内容 + 300/150 窗口 + 正反序双 session 实测字节；新增依赖恢复、关闭取消/drain、callback 寿命、premultiplied 白底合成、黑盒 SPS 约束基线断言 | `git diff -- crates/obscura-media/examples/resident-encoder.rs` | 恢复后 tree | 真实 nextest/build/consumer 取得退出码 |
| 2026-10-03 / 码率测量收敛实验 | 100/40 窗口时正序 ratio 1.351、反序 0.984（差异不稳健）；300/150 窗口时正序 low_tail=1434279→high_tail=1874692（1.307），反序 high_first=1715224→low_second=1432212（0.835），4 次运行逐字节一致，方向在两种窗口顺序下都成立 | `.w5-r2-tmp/consumer-exp300*`; `.w5-r2-tmp/consumer-truth`; `.w5-r2-tmp/consumer-repro-*` | 独占 target | 冻结 300/150 为可复验测量 |
| 2026-10-03 / 真实 gate | `cargo nextest run --locked --release -p obscura-media` exit0（7 passed，1 次 LEAKY 为 tests/stream.rs 既有行为，非本改动）；`cargo build --locked --release -p obscura-media` exit0；`cargo build --locked --release -p obscura-media --examples` exit0；consumer exit0 `RESULT passed=42 failed=0` | `.w5-r2-tmp/nextest-final.log`; `.w5-r2-tmp/build-lib-final.log`; `.w5-r2-tmp/consumer-leased.log` | 独占 target `/Volumes/Intel/playground/obscura/m1-media-resident-w5-20261003/.w5-resident-target` | 写 delivery，allowlist 提交 |
| 2026-10-03 / 资源观察 | 本 tree 内出现第二份 `.w5-resident-target`（110M，birth 03:54:38，无 nextest 产物，binary 内容与 lease 产物同 hash `f19af2fc…`）；来源/归属未能确认，按“不删未确认自有资源”保留并上报 | `stat`/`du`/`shasum` | 本 tree | 不删除；交付记录 |
