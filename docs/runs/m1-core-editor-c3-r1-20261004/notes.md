# C3 editor and storage mirror source notes

Owner: fresh GCM source author for `m1-core-editor-c3-r1-20261004`.
Stage: `SOURCE_AUTHORING_ONLY`. No build, target, runtime, install, review,
merge, or push is in scope while C1 owns the shared target.

| Time (PT) | Node | Conclusion/state | Source/evidence | Input version/environment | Next |
| --- | --- | --- | --- | --- | --- |
| 2026-10-04 | worktree anchor | PASS | Owned worktree `/Volumes/Intel/playground/obscura/m1-core-editor-c3-r1-20261004`; branch `codex/m1-core-editor-c3-r1-20261004`; HEAD `a40d1bb626831cb030b945758f83be217b1e78d2`; tree `f8001162ec6e0cfc990b62dabce345e77931d5cc`; `fork/main` is `948f054`; branch is ahead 7. | `git status --short --branch`, `git rev-parse`, `git log --oneline --decorate` | keep all edits within the two product files and two owned test files |
| 2026-10-04 | design truth read | PASS | Read r2 `repair-design.md`, `implementation-contracts.md`, `blackbox-plan.md`, r3 `implementation-handoff.md`/`design-delta.md`, and r4 admitted review JSON. They freeze F3/F4/F7 and require `beforeinput` before the composition event and `input`; direct cancel already passes and C2 owns disconnect. | read-only AB paths under `/Volumes/Intel/playground/agentbrowser/m1-core-public-harness-repair-r4-20261004` | implement only F3/F4 plus the bootstrap storage mirror |
| 2026-10-04 | C1 interface check | OBSERVED | C1's uncommitted worktree exposes `LocalStorageBackend::load(&self, origin: &str) -> String` and `op_local_storage(..) -> Option<String>`: absent backend is `None`/JS `null`, a configured empty backend remains present `"{}"`, and other commands return `Some("")`. This is interface evidence only, not accepted implementation truth. | read-only C1 worktree `/Volumes/Intel/playground/obscura/m1-core-storage-c1-r1-20261004`; frozen C3 base still declares `load -> String` | keep the C3 JS mirror minimal and declare the combined-interface handoff |
| 2026-10-04 | editor source | SOURCE_IMPLEMENTED | `bootstrap.js` invalidates the focused editable revision on page-originated value/selection/content writes, guards editor-owned writes from double bumping, dispatches cancelable `beforeinput` for composition begin/update/commit, returns `cancelled` without mutation, restores the snapshot on cancel, and keeps commit/cancel single-bump terminal semantics. Event order is `beforeinput`, composition event, `input`. | `crates/obscura-js/js/bootstrap.js`; design truth above | add public runtime and Host consumers |
| 2026-10-04 | storage mirror source | SOURCE_IMPLEMENTED | `bootstrap.js` `_mkStore.sync()` now keeps the standalone in-memory mirror only when `load` is `null`/`undefined`; an authoritative present snapshot, including `"{}"`, refreshes the mirror. No dual sentinel or old empty-string fallback was added. | `crates/obscura-js/js/bootstrap.js`; C1 typed-presence interface observed above | keep F7 marked dependent on C1 terminal composition; no green claim |
| 2026-10-04 | JS public consumer | SOURCE_WRITTEN | `editor_events_r2.rs` drives public `ObscuraJsRuntime` evaluate with real page listeners. It covers ordering, prevented begin/update/commit, commit and cancel terminal revision behavior, page value and selection mutation invalidation, contenteditable mutation, and the standalone versus valid-empty backend mirror distinction. | `crates/obscura-js/tests/editor_events_r2.rs` | syntax/diff validation only; cargo is withheld by the C3 grant |
| 2026-10-04 | Host public consumer | SOURCE_WRITTEN | `editor_events_public_r2.rs` drives the real Host Unix socket. It covers stale revision rejection plus fresh-click recovery, public composition begin/update/commit ordering, prevented edit cancellation, cancel snapshot restoration and lease release, stale target, and stale control epoch. | `crates/obscura-host/tests/editor_events_public_r2.rs` | syntax/diff validation only; cargo is withheld by the C3 grant |
| 2026-10-04 | revision invalidation tightening | SOURCE_IMPLEMENTED | Input value writes now compare the previous and current value before bumping; focused contenteditable/textarea descendant text writes invalidate the owning target; editor-owned value/selection writes use a guard so apply and snapshot restore do not double-bump. The Host recovery assertion now checks that the fresh context differs from the stale binding rather than pinning an incidental revision count. | `crates/obscura-js/js/bootstrap.js`; `crates/obscura-host/tests/editor_events_public_r2.rs` | keep all behavior claims at source-only until the combined builder runs |
| 2026-10-04 | non-build validation | PASS | `node --check crates/obscura-js/js/bootstrap.js` exit 0; `rustfmt --emit stdout --edition 2021` exit 0 for both new test files; `git diff --check a40d1bb626831cb030b945758f83be217b1e78d2 --` exit 0. No cargo/nextest/build/runtime command was run. | tool exit codes in this worktree | commit the allowed source/tests/notes, then record the exact handoff SHA/tree |

## Interface handoff

The combined successor must consume all of the following in one candidate:

1. C1's terminal typed storage presence at `op_local_storage`:
   `Option<String>` where absent is `None` and a valid empty configured
   snapshot is `Some("{}")`.
2. C3's bootstrap consumer:
   `null`/`undefined` preserves the standalone in-memory mirror; a present
   snapshot refreshes the backend view. The old empty-string sentinel is not a
   fallback.
3. C3's Engine editor revision and composition semantics as implemented in
   `bootstrap.js`.

The C3 source tests in this branch compile only against the combined C1
interface for the storage-mirror cases. On the frozen `a40d1bb` base,
`LocalStorageBackend::load` still returns `String`, so the combined successor
is the first candidate that can run the full C3 suite and the copied public
constructor `SUMMARY=PASS` consumer.

## Withheld evidence

- No cargo/nextest/build command was run.
- No target directory, V8 cache, binary, or runtime was created or started.
- No review, merge, push, install, device, or IME acceptance was performed.
- F3/F4 source behavior is implemented but not build- or E2E-verified.
- F7 remains dependent on C1's terminal implementation and combined
  composition.
