# Remote video latency: capture cadence

Owner: OB-01 capture cadence (`crates/obscura-host/src/worker.rs`).
Bug: 0a17d02.

## What is fixed here

Capture cadence was a hardcoded `interval(333 ms)`, i.e. a hard **3 fps**
ceiling that set the floor on glass-to-glass latency regardless of how fast the
page or the encoder was.

`frame_period()` now defaults to 33 ms and is tunable with
`OBSCURA_FRAME_PERIOD_MS` (clamped 8..1000 ms).

Measured on real weibo.com at 1280x800: 3.00 fps before (30 frames / 10 s),
4.2-11.1 fps after depending on how much of the page is repainting. Capture
itself measured ~1 ms; paint is the remaining bound.

## What was reverted, and why

A persistent-encoder rework in `crates/obscura-media` was implemented and then
**reverted** in the same worktree; `crates/obscura-media` is byte-identical to
base `7633e80` here.

Two real defects were found along the way and are recorded so the next attempt
does not repeat them:

1. A persistent encoder keeps stdin open, so x264's queueing lookahead
   (`bf=2`, `rc-lookahead=40` from the non-low-latency profile) buffers
   indefinitely and emits no access unit at all. The old one-shot path only
   worked because it closed stdin after one frame, forcing a flush.
2. Access-unit framing cannot be recovered from the read boundaries of an
   FFmpeg pipe. A desktop access unit is megabytes and always spans several
   reads, so any rule that treats a read (or a read whose buffer happens to
   start with an AUD) as one unit hands the client a fragment. Reproduced
   deterministically: at 1280x800 the adapter failed on frame 1-2 with
   `H.264 encoder exceeded frame deadline`, and a direct harness observed a
   44346-byte fragment with zero AUDs. At 391x701 the same code passed 240
   frames because those units fit in one read.

That regression reached the deployed Host and produced a user-visible failure:
the client's logcat showed a repeating
`Host ENCODER_UNAVAILABLE: Encoder for session ... H.264 encoder exceeded frame
deadline`, the media adapter exited, and the app reported
`Transport: IO error: Software caused connection abort (os error 103)` on every
reconnect. Reverting the media change restored the session.

Framing a persistent encoder correctly needs an explicit frame boundary from
the encoder (a length-prefixed or NAL-counted container) rather than inference
from pipe reads. That is separate work.

## Verification after the revert

- `cargo test --release -p obscura-media` -> 3 + 2 + 7 passed, 0 failed.
- `cargo test --release -p obscura-media --test stream` x6 runs -> 0 failures.
- Deployed Host (`08219e37`) + endpoint with the reverted media binary: a
  60-second session on real weibo.com produced 249 frames at 4.15 fps with
  **0 stalls over 3 s**, and the media adapter stayed alive with 0
  `exceeded frame deadline` entries.

## Not proven

- No 15T device replay driving the live session end to end.
- Capture remains paint-bound; raw RGBA still crosses the Host->media socket
  (3.9 MiB per 1280x800 frame).
- Deployed transport is still WSS/TCP; UDP/WebRTC is not on the product route.
- macOS still runs libx264 with `-threads 1`; videotoolbox is unused.
- No merge, push, remote receipt, or M1 baseline.
