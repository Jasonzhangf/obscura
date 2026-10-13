# Controller DAG binding and validation for the r4/r5 candidate

Owner: controller `m1-repair-model-20261002`. This record answers the single P1
of `m1-frame-terminal-response-impl-20261005-r4` ("Required feature/repair DAG
artifact and validation"). It is controller evidence, not an author artifact.

Revision note: the first version of this record, committed in the r4 candidate
`c77c8d4`, cited `m1-connection@1` and `m1-video@1`. Those are the copies that
sit on the controller orchestration branch. They are stale. The current project
graph artifacts are the newer versions on the design lineage, and this revision
cites and validates those. The correction is recorded in the divergence section
below and is re-reviewed with the r5 candidate.

## Finding disposition

The reviewer's factual observation is correct and is closed here: the r4 run
record's verification list carried no DAG binding and no `dagpipe graph
validate` result. The reviewer could not resolve the project graphs because the
M1 program's graph artifacts live in the controller repository, while this
candidate is an Obscura worktree whose own `AGENTS.md` declares no DAG gate.

The finding's proposed remediation ("bind this repair to an existing
project-owned graph, or add the minimal project graph if none exists") resolves
to a binding, not to a new graph, for the reason recorded below.

## Binding

The project-owned M1 graph artifacts are
`docs/dagpipe/graphs/m1-*.graph.json` in the AgentBrowser repository. The
execution plan, `docs/goals/m1-user-browser-execution-plan-20261003.md:56`,
states the rule this repair falls under: a new unknown capability or a missing
link fixes only the affected existing graph and then re-codes after `dagpipe
graph validate` and design admission; for unchanged graphs valid evidence is
reused; and an ordinary modification must not build a new governance skeleton.

This repair is an ordinary modification. It adds no capability and no missing
link:

- The terminal fence it reacts to already exists at the candidate base. The
  `terminal_pending` watch is declared at `crates/obscura-media/src/webrtc_endpoint.rs:60`
  and `:72`, initialized at `:120`, and already used to stop emitting video at
  `:168` and `:180`. The r4 diff reuses that same watch on the request path.
- The producing side of that fence is the Host request pump in
  `crates/obscura-host/src/endpoint/channels.rs:195-213`, which signals
  terminal-pending and then awaits bounded settlement.
- The change is node-internal control flow: a `biased` select between the
  bounded-queue send and the existing fence watch, plus an early `continue`. It
  introduces no new node, no new arc, and no cross-node back edge into another
  independent function's source of truth. The global DAG invariant permits
  arbitrary control flow inside a node.
- Owning nodes, unchanged: `m1-connection@5`
  `obscura.host.apply_attachment_lifecycle` ("完成final Attach并绑定或释放调用方attachment")
  owns the final Attach and the binding or release of the caller's attachment,
  which is the release endpoint this repair makes reachable; and `m1-video@2`
  `obscura.endpoint.send_media` ("有界发送并维护解码依赖") owns the endpoint's
  bounded-send discipline on the same DataChannel that the request path shares.
- Therefore no graph artifact is missing and no graph is inconsistent with the
  implementation. Creating a new graph here would be exactly the new governance
  skeleton the plan forbids for an ordinary modification.

## Validation against this candidate

Run by the controller with `/Users/fanzhang/.cargo/bin/dagpipe` on the design
lineage worktree, whose graph tree is unmodified
(`git status --porcelain=v1 -- docs/dagpipe/graphs/` prints nothing):

- `dagpipe graph validate docs/dagpipe/graphs/m1-connection.graph.json` →
  `valid DAG: m1-connection@5 (9 nodes, 8 edges, 9 waves)`, exit 0.
- `dagpipe graph validate docs/dagpipe/graphs/m1-video.graph.json` →
  `valid DAG: m1-video@2 (8 nodes, 7 edges, 8 waves)`, exit 0.
- All thirteen project graphs validate: `m1-chrome-lifecycle@1`,
  `m1-chrome-request@1`, `m1-connection@5`, `m1-control@1`, `m1-delivery@1`,
  `m1-editing@1`, `m1-focus@1`, `m1-navigation@2`, `m1-pointer@1`,
  `m1-profile@3`, `m1-quality@1`, `m1-video@2`, `m1-viewport@1`, each exit 0.

Artifact hashes at validation time:

- `m1-connection.graph.json` sha256
  `ff28565a178c1074e25bddd083ad428d10611ccd57042cae4da6bc2ee00b11e4`
- `m1-video.graph.json` sha256
  `8a64907954032453ddc827035e96dd7e888661e3c1a817ac9f8fbf28a44f1289`

Each command also printed the tool's own scope note that operator bindings are
syntactically present and that the project `compile()` remains the authoritative
registry, schema and effect gate. That gate is not part of this repair's scope
and is not claimed here.

## Disclosed divergence in the graph artifacts

The AgentBrowser repository currently carries two different versions of the same
graph files on two lines of history:

- The design lineage (`codex/m1-media-terminal-closure-*`,
  `codex/m1-session-media-*`, `codex/m1-frame-forwarding-design-*`) carries the
  current versions, advanced by `bcfee47` and `b8a0587`: `m1-connection@5`,
  `m1-video@2`, `m1-profile@3`.
- The controller orchestration branch `codex/m1-repair-model-20261002` carries
  older copies last touched by `7cbe1c8`: `m1-connection@1`, `m1-video@1`,
  `m1-profile@2`. `b8a0587` is an ancestor of the design lineage and is not an
  ancestor of the orchestration branch; the two lines share merge base
  `c171c205f3a37e08c6b487e1c55a448ef694315c` and have 43 orchestration-only and
  15 design-only commits respectively.
- `origin/main` (`f05eadd2e9e21607bd37e3b03152f629379965c9`) contains no
  `docs/dagpipe/` tree at all.

Consequences, recorded rather than papered over: the current project graph truth
is the design-lineage version, which is what this record validates; a validation
run inside the orchestration branch would validate stale copies and must not be
used as this repair's evidence; and the graph artifacts are not yet on the
remote mainline. Reconciling the two lines and landing the graphs on
`origin/main` belongs to R3 ("两仓正确集成与远端回执"). This repair does not
merge, rebase or rewrite either line.

## Effect on the reviewed candidate

This record is added to the reviewed candidate so the binding and the validation
result are inside the reviewed revision rather than only in controller notes.
The Rust delta, the public regression case and every previously recorded
verification result are unchanged; the behavioral evidence is reused because the
source and the graphs did not change.
