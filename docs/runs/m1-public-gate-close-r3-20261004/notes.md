# m1-public-gate-close-r3 worker notes

Worker: standalone GCM implementation/diagnosis worker. Not controller/reviewer.
CWD: /Volumes/Intel/playground/obscura/m1-public-gate-close-r3-20261004
Branch: codex/m1-public-gate-close-r3-20261004
Base HEAD: 67d936eb036d3063c4bedf981bb98975a10750ec
Base tree: 3ee3d23809d82283cd6210c96323b72b0cac6901
Sole target lease: /Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target

## Node log

| UTC | Node | Status | Evidence | Input/env | Next |
|---|---|---|---|---|---|
| 2026-10-04T23:44:00Z | setup: cwd/branch/write scope | done | git HEAD 67d936eb036d3063c4bedf981bb98975a10750ec tree 3ee3d23809d82283cd6210c96323b72b0cac6901; status shows only the 3 owned test files plus docs/runs/m1-public-gate-close-r3-20261004 | branch codex/m1-public-gate-close-r3-20261004 | continue |
| 2026-10-04T23:48:00Z | rp24 denial fixtures | done | raw/rp24-red/stderr.txt exit 0: 1 passed, 36 skipped | cargo nextest run --locked --release --features render -p obscura-host --test remote_admission_r1 rp24 --no-fail-fast | none |
| 2026-10-04T23:54:00Z | persistent HTTP fixtures | done | raw/persistent-green/stderr.txt exit 0: 25 passed, 1 leaky, 0 failed | cargo nextest run --locked --release --features render -p obscura-host --test persistent --no-fail-fast | none |
| 2026-10-04T23:59:00Z | webrtc diagnosis attempt 1 | failed | raw/webrtc-baseline/stderr.txt exit 1; first phase exit 1 "timed out waiting for WebRTC browser response"; endpoint.stderr "WebRTC endpoint task ended: Host media frame sequence regressed or repeated" then "Error: Host media ended without Closed"; host.stderr empty | nextest rebuilt host/endpoint only; media/receiver were pre-candidate artifacts | rebuild media then rerun |
| 2026-10-04T23:06:46Z | rebuild obscura-media webrtc | done | raw/rebuild-media/stderr.txt exit 0, Finished release profile in 21.76s | cargo build --locked --release -p obscura-media --features webrtc --bins | rerun webrtc |
| 2026-10-04T23:09:42Z | artifact identities | done | see Artifact receipts below | release dir in sole target lease | rerun webrtc |
| 2026-10-04T23:10:30Z | webrtc diagnosis attempt 2 (rebuilt media) | failed | raw/webrtc-rebuilt/stderr.txt exit 100: 1 passed, 1 failed; encoder control green; same failure reproduced | CARGO_TARGET_DIR sole lease; media/receiver rebuilt at 23:06 from unchanged candidate | root-cause traced |
| 2026-10-04T23:12:00Z | webrtc first product deviation | root-caused (product) | see WebRTC causal chain below | raw/webrtc/ow-10d87e0953b44b25acedb965e20dd122/ | report to parent; no product edit |
| 2026-10-04T23:20:00Z | decisive differential: --skip-click | done | raw/webrtc-skipclick/stderr.txt exit 100 (only forced-skip post-condition); no endpoint death, no duplicate seq | temporary in-scope test change, reverted (diff -q MATCH_BAK) | record |
| 2026-10-04T23:26:00Z | affected combined gate (RP/Core) | partial | raw/combined/stderr.txt exit 100: 186 run, 185 passed, 3 leaky, 0 skipped, 1 failed (only the known WebRTC product failure); raw/webrtc/ow-fa065723f4d343d6b0b16fdb3fbb127d/ shows identical endpoint.stderr | cargo nextest -p obscura-host -p obscura-browser -p obscura-host-protocol | full gate |
| 2026-10-05T00:00:02Z | full workspace gate | partial | raw/full/stderr.txt exit 100: 1775 run, 1774 passed (4 leaky), 1 failed, 4 skipped; single FAIL line is the known WebRTC product failure, confirmed by grepping raw/full/stderr.txt. raw/webrtc/ow-c0471fef01df42d7a8c05d9a416dea1d/endpoint.stderr still has exactly 1 "regressed or repeated" | cargo nextest run --locked --release --features render --no-fail-fast | release build + obstacle course |
| 2026-10-05T00:02:00Z | release build host+cli | done | raw/build/stderr.txt exit 0, Finished release profile in 0.88s | cargo build --locked --release --features render -p obscura-host -p obscura-cli --bins | obstacle course |
| 2026-10-05T00:04:00Z | obstacle course | done | raw/obstacle/stdout.txt exit 0: correctness 33/33 stages passed, median latency 3022.8ms | cwd /Volumes/Intel/playground/obscura-benchmark/m1-io-serial-repair-20261002 at 8bd11ade523fd7358c7b1fddc398b48e9040c3c9; OBSCURA_BIN=sole target/release/obscura sha256 b56c9a0d047b3672283f0967ddda7f23c82dee1e7917d3896ee1bb6b811ef521 | commit + cleanup |
| 2026-10-05T00:07:00Z | commit | done | HEAD c60c68755515965189e67c4b0a8e37490593f7ab, tree 40aa8d9866de8ea2371bdf1c2f6e5f1fb07e173e; working tree clean; base-to-head only allowed paths | git commit -m "test: public gate r3 fixtures and WebRTC diagnosis" | final report |

## Artifact receipts

All under /Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target/release.

| Binary | sha256 | mtime |
|---|---|---|
| obscura-host | 3b6423f25ed5951437fbc031eecaefd32aa513bff6900e93e814e4d532a4c35a | Oct  4 15:48:34 2026 |
| obscura-endpoint | eb29958e9415cdaf0a2695b0eabfc7ab11563aad3abf9c10d83dc0462b96465d | Oct  4 15:48:28 2026 |
| obscura-media | a7a65f21b65aacac2c6ce87aabae6be192bfd28c7339661658cbbc0fc3305961 | Oct  4 16:06:46 2026 |
| obscura-webrtc-receiver | 5b6ae487b773c57ed837f95dcc6940316e78f66ed82c14a638659fa2245cec8f | Oct  4 16:06:57 2026 |

Final receipts after the full gate rebuild (source unchanged; host/endpoint/test deps rebuilt
from the same tree).

| Binary | sha256 | mtime |
|---|---|---|
| obscura | b56c9a0d047b3672283f0967ddda7f23c82dee1e7917d3896ee1bb6b811ef521 | Oct  4 16:50:16 2026 |
| obscura-host | 1ccaec7c2809b1bf6e8b4080b7206152566f69421060724b9bbbb069f94a4249 | Oct  4 16:53:58 2026 |
| obscura-endpoint | aed448149635227b12d4a74660fd570173f049c677c8efa03adc82e5b1fab9e4 | Oct  4 16:54:39 2026 |
| obscura-media | c674ae0e0a96cfff0a6708b8878635d804ace5a9adcb40442f19b6a0dd409b13 | Oct  4 16:56:17 2026 |
| obscura-webrtc-receiver | c23390faac5b963389f0f25ecf5e884f409900d2443f21f439fadd16512c70fc | Oct  4 16:56:33 2026 |

## Owned process state

Protected shared Hosts 33820 and 95552 are running and untouched.

Owned children from webrtc attempt 1: host PID 91895, endpoint PID 93588. Both absent at
2026-10-04T23:09:42Z. No broad kill used at any point.

## WebRTC causal chain

Status: AUTHOR_PARTIAL. First product deviation is outside this worker's write scope.

### Red evidence

Both attempts fail identically. Attempt 2 used media/receiver rebuilt from the unchanged
candidate, so the pre-candidate artifact explanation is eliminated.

- raw/webrtc-rebuilt/stderr.txt exit 100, 1 passed, 1 failed.
  `real_receiver_gets_typed_encoder_unavailable_error` PASS (control preserved).
  `real_receiver_gets_continuous_host_media_and_rejects_stale_binding` FAIL.
- Attempt dir: raw/webrtc/ow-10d87e0953b44b25acedb965e20dd122/
- endpoint.stderr:
  `WebRTC endpoint task ended: Host media frame sequence regressed or repeated`
  then `endpoint connection ended: IO error: peer closed connection without sending TLS close_notify`
  then `Error: Host media ended without Closed`
- host.stderr empty. host.stdout empty.
- first/stderr.txt: `Error: timed out waiting for WebRTC browser response`, exit 1.
- Receiver never printed a report; it had not reached its own frame loop deadline.

### Deduction

The endpoint task fails at crates/obscura-media/src/webrtc_endpoint.rs:274:

    ensure!(source.sequence > *sequence, "Host media frame sequence regressed or repeated");

The guard only ever fires when the endpoint emits the same sequence twice, or a lower
one. The Media adapter proves the source is strictly increasing:

- crates/obscura-media/src/main.rs:52-56 rejects `info.sequence > sequence` violations with
  "Regressive Host frame identity" and advances `previous` per frame.
- The Media adapter is not the duplicate source.
- The raw Host media path cannot either: crates/obscura-host/src/daemon.rs:1105-1107
  feeds one `media::stream` per socket, and crates/obscura-host/src/media.rs:20-32 only
  forwards the latest value after `frames.changed()`; it never re-emits the current value.
- `frame_sequence` increments exactly once per captured frame at
  crates/obscura-host/src/worker.rs:215 and never resets.

The only remaining forwarder between the encoded stream and the WebRTC task is the
per-attachment filter, and it is stateless with respect to the frame it last published:

- crates/obscura-host/src/endpoint/channels.rs:205 creates the filtered watch.
- lines 207-218 run a loop that republishes `source.borrow_and_update()` every iteration,
  with no `last_published` memory.
- lines 219-222 wait on `source.changed()` OR `state.changed()`.

A `state.changed()` wakeup that carries an unchanged frame therefore republishes the same
`Arc<EncodedFrame>`, which is not detectable by `Sender::send_replace` because the receiver
holds the same `Arc` pointer. `send_replace` has no identity guard and does no equality
comparison. The endpoint then re-emits that same sequence and trips the guard.

### Trigger in this consumer

The receiver makes one WebRTC DataChannel browser request that produces a no-op media
state update:

- id 101 `Command::RequestTakeover` at webrtc_receiver.rs:203-205 resolves to
  `ResultValue::Status` via crates/obscura-host/src/daemon.rs:606-609 and 417.
- crates/obscura-host/src/endpoint/channels.rs:278-281 forwards the request on the Host
  admission stream, line 279 then calls `update_media_state`.
- update_media_state (channels.rs:258-262) calls `active.send_modify` for every
  `ResultValue::Status`, even when the value is identical. `send_modify` always bumps the
  watch version and notifies receivers, so `state.changed()` fires even though
  `attached`/`session_id`/`generation` did not change.
- The filter's `state.changed()` future wakes, the loop body re-runs, and
  line 215 republishes the same `Arc<EncodedFrame>` the filter already sent.
- The endpoint's loop (webrtc_endpoint.rs:143-150) reads it and calls `emit_frame`, whose
  guard fails.

The endpoint dies first, before sending the forwarded browser response. The receiver then
times out on the `BrowserResponse` it was waiting for at webrtc_receiver.rs:586-602 and
prints "timed out waiting for WebRTC browser response". So the receiver message is a
consequence; the first failing correlated command is the endpoint task death on the
no-op media state update.

### Rule out: not a timer, not a stall, not an encoder issue

- No timeout, deadline, or sleep was changed or extended.
- The encoder control arm passes, so H.264 encode, packetization, DataChannel control
  messages and the typed `ENCODER_UNAVAILABLE` path are intact.
- The endpoint fails inside 25.78s of test time, while the receiver is still inside its
  20s per-request window on request id 101.
- No frame, ICE or capability assertion failed on the receiver side.

### First product owner

Owner file: `crates/obscura-host/src/endpoint/channels.rs`
Function: `exact_session_stream` (lines 201-226)
Commit that introduced the filter: `ffccee81` "feat(host): add remote profile admission slice"
Tokio version in Cargo.lock: 1.52.3

### Required minimal diff

Make the filter skip re-publishing the frame it already published, then wake on either
watch. Two changes inside the spawned task, no signature, protocol, auth or capability
change:

    let mut last_published: Option<Arc<EncodedFrame>> = None;

            match source.borrow_and_update().clone() {
                Some(frame) if packet_session(&frame.packet).as_deref() == Some(session.as_str())
                    && last_published.as_deref() != frame.as_ref() => {
                    last_published = Some(frame);
                    filtered.send_replace(Some(frame));
                }
                _ => { last_published = None; filtered.send_replace(None); }
            }

Alternative with the same semantics, using the watch API already in the crate:
`filtered.send_if_modified(..., |current| **current != frame)`.

### Causal red for that owner

- Reproduced twice on this candidate: raw/webrtc/ow-424c6c430efe4e68861a7c347f7bb2f6/
  (pre-candidate media) and raw/webrtc/ow-10d87e0953b44b25acedb965e20dd122/ (rebuilt
  candidate media). Identical endpoint.stderr in both.
- Decisive differential: raw/webrtc-skipclick/stderr.txt exit 100 (see below). The same
  suite run with the first receiver's `--skip-click` makes zero WebRTC DataChannel browser
  requests, so no `RequestTakeover` and no no-op `update_media_state`. The endpoint stayed
  alive and the receiver completed its own frame loop with no duplicate sequence:

      {"pass":true,"browser_path":"webrtc_data_channel",
       "frames":[{"sequence":2,...},{"sequence":3,...},{"sequence":4,...}],
       "ice":{"transport":"udp","local_candidate":{"protocol":"udp","candidate_type":"host"},
              "remote_candidate":{"protocol":"udp","candidate_type":"host"}}}

  Endpoint stderr in that run had no "regressed or repeated" line, and no
  "timed out waiting for WebRTC browser response" in the receiver. The only assertion that
  failed was this worker's own forced-skip post-condition
  "Host click did not produce a different WebRTC frame", which is expected because
  `--skip-click` deliberately skips the click path. That proves the failure needs the
  DataChannel browser request, matching the `update_media_state` trigger above.
  The temporary `--skip-click` change was reverted; `webrtc_endpoint.rs` byte-matches the
  pre-experiment file (verified with `diff -q`, MATCH_BAK).

### Admission gap

No admitted contract in the r7 Profile model or the controller run docs covers the
per-attachment encoded-stream filter as a stateful forwarder with a publish-once
invariant. The docs describe exact-session filtering as a property
(`/Volumes/Intel/playground/obscura/m1-profile-eof-contract-r7-20261004/docs/runs/
m1-remote-profile-access-design-r2-20261004/summary.md` lists "`frames.sock` serves only
`sessions[&presented]`" as RP-09/RP-13) but do not state that a state-change-only wakeup
must not re-present a frame the peer already consumed. Parent must admit this as a product
fix with the WebRTC public consumer as the red-green gate before a Product owner implements.

### In-scope work finished anyway

`crates/obscura-host/tests/webrtc_endpoint.rs` now captures per-attempt raw stdout/stderr
and child PIDs for Host, endpoint, and each receiver phase, while keeping every original
assertion: UDP ICE host/host selection, >=3 continuous frames with full descriptors,
click pixel mutation, remote-command and unauthorized-input denial, stale control and
stale document rejection, human takeover and release, same-session reconnect with a new
attachment id, and the typed encoder control. No timer was extended, no frame faked, no
assertion weakened. The captured logs are what produced this chain.
