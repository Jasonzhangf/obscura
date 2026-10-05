# Controller DAG binding and validation for the r4 candidate

Owner: controller `m1-repair-model-20261002`. This record answers the single P1
of `m1-frame-terminal-response-impl-20261005-r4` ("Required feature/repair DAG
artifact and validation"). It is controller evidence, not an author artifact.

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
`docs/dagpipe/graphs/m1-*.graph.json` in the controller repository
`/Volumes/Intel/playground/agentbrowser/m1-repair-model-20261002`. The execution
plan, `docs/goals/m1-user-browser-execution-plan-20261003.md:56`, states the
rule this repair falls under: a new unknown capability or a missing link fixes
only the affected existing graph and then re-codes after `dagpipe graph
validate` and design admission; for unchanged graphs valid evidence is reused;
and an ordinary modification must not build a new governance skeleton.

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
- Owning nodes, unchanged: `m1-connection@1`
  `obscura.host.apply_attachment_lifecycle` ("绑定或释放观察连接与旧输入") owns
  binding and releasing the observation attachment whose release endpoint this
  repair makes reachable, and `m1-video@1` `obscura.endpoint.send_media`
  ("有界发送并维护解码依赖") owns the endpoint's bounded-send discipline on the
  same DataChannel that the request path shares.
- Therefore no graph artifact is missing and no graph is inconsistent with the
  implementation. Creating a new graph here would be exactly the new governance
  skeleton the plan forbids for an ordinary modification.

## Validation against this candidate

Run by the controller in the controller repository, whose graph tree is
unmodified:

- `git status --porcelain=v1 -- docs/dagpipe/graphs/` is empty, so the graph
  artifacts are identical for the reviewed candidate and for its base.
- `dagpipe graph validate docs/dagpipe/graphs/m1-connection.graph.json` →
  `valid DAG: m1-connection@1 (4 nodes, 3 edges, 4 waves)`, exit 0.
- `dagpipe graph validate docs/dagpipe/graphs/m1-video.graph.json` →
  `valid DAG: m1-video@1 (5 nodes, 4 edges, 5 waves)`, exit 0.
- `dagpipe graph validate docs/dagpipe/graphs/m1-control.graph.json` →
  `valid DAG: m1-control@1 (4 nodes, 3 edges, 4 waves)`, exit 0.
- `dagpipe graph validate docs/dagpipe/graphs/m1-navigation.graph.json` →
  `valid DAG: m1-navigation@2 (4 nodes, 3 edges, 4 waves)`, exit 0.

Each command also printed the tool's own scope note that operator bindings are
syntactically present and that the project `compile()` remains the authoritative
registry, schema and effect gate. That gate is not part of this repair's scope
and is not claimed here.

Artifact hashes at validation time:

- `m1-connection.graph.json` sha256
  `28fdadb0dd21aa0a34f25ee4ce0fd73912fb6cd9cddd665968ac1f4903ead5a7`
- `m1-video.graph.json` sha256
  `172c697027c44a308e5168262d9aac9d24447ea91cbcf73a9f1382ca14f64d89`

## Effect on the reviewed candidate

This record is added to the reviewed candidate so the binding and the validation
result are inside the reviewed revision rather than only in controller notes.
The Rust delta, the public regression case and every previously recorded
verification result are unchanged; the behavioral evidence is reused because the
source and the graphs did not change.
