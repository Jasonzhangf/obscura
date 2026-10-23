# MCP Fixture R1 Delivery

Status: author candidate complete; independent review, integration, full gate, install, and M1 verification remain with the controller.

## Scope

- Worktree: `/Volumes/Intel/playground/obscura/m1-mcp-fixture-fix-r1-20261003`
- Branch: `codex/m1-mcp-fixture-fix-r1-20261003`
- Base: `948f054b51af442306be046d0014cfa3da628f27`
- Source change: `crates/obscura-cli/tests/mcp_client.rs` only
- Read-only consumer: `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release/obscura`
- Consumer SHA-256: `d41658d58b35c452ac55c7f29c725446aa10f14e6a6c4ef90a5f6a591e2b4c69`
- Worker SHA-256: `120bc191e004c91c6746f881517466a7f34eb136318ac925ddd745ec1e61d73f`
- Delivery graph: `/Volumes/Intel/playground/agentbrowser/m1-repair-model-20261002/docs/dagpipe/graphs/m1-delivery.graph.json`, SHA-256 `896e2c39105ca5bfb9c62bd770a2755e913035f72b793f3cde58cb960b45c2bb`, node `作者完成候选与开发验证`. The graph was read only and was not copied or modified.

## Root Cause

`TestPageServer` bound a nonblocking listener. On Darwin the accepted stream inherited `O_NONBLOCK`; the fixture made one `read`, ignored `EAGAIN`, then wrote a successful HTTP response. When the public MCP navigation request had not landed yet, that response and close could race the client's later request and surface as `Network error`.

The same boundary also responded after a partial request, before the HTTP header terminator arrived.

## Fix

The fixture now:

1. Exits immediately when Drop wakes the nonblocking listener after setting the stop flag.
2. Sets the accepted stream blocking for real client requests.
3. Reads until the HTTP header terminator instead of responding after one partial read.
4. Propagates read errors and rejects EOF before headers, so an error cannot fall through to a fake successful response.

The page body, existing assertions, timeout, success conditions, and Drop wake-up resource path were not weakened.

## Regression

Added `fixture_waits_for_delayed_request` in the same file. It connects a real client socket, delays the request, asserts the fixture does not respond before the request, sends only part of the HTTP header, asserts no response before the header terminator, then completes the request and asserts a real `200 OK` response containing `Example Domain`.

Red evidence:

- Old fixture: `fixture responded before request: Ok(1)`, exit 101.
- Blocking single-read repair: `fixture responded before request headers completed: Ok(1)`, exit 101.
- Raw: `.mcp-fixture-r1-tmp/r1-delayed-red.{stdout,stderr}`, `.mcp-fixture-r1-tmp/r1-partial-red.{stdout,stderr}`.

Green evidence:

- Exact `test_navigate_and_snapshot`: 1 passed, exit 0.
- Full `mcp_client` consumer: 19 passed, exit 0.
- Repeated `test_navigate_and_snapshot`: 200/200 exit 0.
- Repeated delayed/segmented fixture regression: 50/50 exit 0.
- Public race control: unmodified accepted stream failed 4/200 navigations, all after `read_errno=35`; blocking accepted stream failed 0/200.

Compile binding:

- `rustc --edition=2021 --test crates/obscura-cli/tests/mcp_client.rs`
- `CARGO_BIN_EXE_obscura=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release/obscura`
- `--extern serde_json=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release/deps/libserde_json-ac735387abb1c73b.rlib`
- serde_json rlib SHA-256: `0baa8e90cf2b8e6cab211dbe0b7b08411f26abbf74cf44d2929f87c32c736ebd`
- Green test binary SHA-256: `f1970756a8104880b330eb06adbc644acb624fb20f7978993c9d43a82b0d7e9f`

No Cargo, cache, runtime, or product-code command was run. The read-only engine-reconcile binary was used directly.

## Resource State

The task-owned fixture, CLI, worker, and test processes exited. No owned `mcp_client`, `mcp_fixture_race`, `obscura mcp`, worker, or listener remained at the audit. Raw evidence is retained in `.mcp-fixture-r1-tmp/`; the worktree is left for controller review and cleanup.

## Not Complete

- Independent architecture review and full repository gate.
- Combination with latest `origin/main`, merge, push, or any install.
- Core full-repo, robots, Host mTLS/receiver, installed user binary, and M1 verification.
- The delivery graph and business connection graph were not modified.
