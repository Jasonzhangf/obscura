# Continuous WebRTC forwarding: implementation review input

Controller intake, 2026-10-05. Review base is
`ac300cb5d79d33cabf0b33fcdbf33069ceba19cb`; product candidate is
`cf9dee1d99e67aef4e4c77b00044ca02d44c3526` (tree
`de13d0ae84bdecc972fc0dd3d4f1bd860c9e2394`). This document only makes the
existing author evidence accessible in the review diff. It adds no behavior.

The product delta is exactly `crates/obscura-host/src/endpoint/channels.rs`
and `crates/obscura-host/tests/webrtc_endpoint.rs`. Each admitted source packet
is published once; identical encoded bytes with a new source identity remain
a new frame. A foreign latest packet or initial None does not fence the
admitted session while its MediaState is unchanged. A genuine binding/control
change terminates the old stream; reconnect creates a new admitted attachment.

## Source and evidence equivalence

The controller read the terminal recovery report, exit0, clean candidate and
scope. `git diff --exit-code 56f929a5 HEAD -- Cargo.toml Cargo.lock crates
protocol scripts` is empty. Source, test, lock, features and scripts equal the
original author-verified product. Existing test/build evidence is reused on
that basis; no target or runtime change occurred during source recovery.

Full author report (including commands, artifact hashes and process violation):
`/Volumes/Intel/playground/agentbrowser/m1-repair-model-20261002/docs/runs/m1-20261002/frame-forwarding-commit-recovery-r1-author-report.md`.

Original public behavior, design admission and raw receiver index:
`/Volumes/Intel/playground/obscura/m1-frame-forwarding-impl-r1-20261005/docs/runs/m1-frame-forwarding-impl-r1-20261005/README.md`.
Its `raw/webrtc/` contains actual independent WebRTC receiver results, not mock
calls: normal continuity/click mutation/stale denial/reconnect, same-content
new sequences, foreign-session-then-A, initial-None-then-A, and encoder failure.
Original command logs are retained in
`/Volumes/Intel/playground/obscura/.worker-runs/m1-frame-forwarding-impl-r1-20261005/gcm-host-r1/`.

Decisive author gates: frozen RED, reverse RED, restored receiver 5/5; affected
RP24 1/1 and persistent 25/25; latest combined 189/189 (one leaky); full
workspace 1778/1778 (four skipped); release Host/CLI and media builds; real CLI
obstacle course 33/33. The controller consumed raw full/combined/CLI outcomes
in the preceding intake and does not rerun unchanged gates for this document.

## Limits and preserved failures

The first combined run was 187/189 with failures in
`page_transport_prefetches_once_and_capture_reuses_the_bytes` and
`click_receipt_precedes_navigation_and_old_document_input_is_rejected`.
Later green runs do not prove these failures were pre-existing or root-cause
closed. They remain open for combination/user acceptance. Persistent and
combined leaky results remain recorded. This is review admission for the
narrow forwarding implementation, not approval of paired installed M1.

The original product commit was rejected for hooksPath override. Recovery also
initially attempted `git -c core.hooksPath= commit`, then used soft reset and
plain commit to produce cf9dee1. The forbidden intermediate attempt and object
are disclosed, not accepted as a compliant route. The final normal commit is
content-equivalent; actual hooksPath is unset and only sample hooks exist.
Future worker commits are controller-owned. No hook was modified or removed.

Per-session media producer/finalization is the separately owned Media r8/P3
task. This slice does not claim installed Mac/Android, Tailscale user blackbox,
profile restart, merge, remote receipt, resource closure, or M1 completion.
