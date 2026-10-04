# Media stream fixture r1 notes

2026-10-04T15:57:17Z / bind / READY

- Result: source, lease, inputs, and target are bound before action.
- Evidence: own cwd `/Volumes/Intel/playground/obscura/m1-media-stream-fixture-r1-20261004`,
  branch `codex/m1-media-stream-fixture-r1-20261004`, HEAD
  `cac77e5f34b6503c42e4ad3a14d80c12527bcb18`, tree
  `ca41f4458edc04e34a76f0010a58edd27941782c`, clean.
- Evidence: `stream.rs` SHA-256
  `f666cbc337f54473495b381e29fada18141a815986af13fcf23587779fce53ad`.
- Evidence: shared target
  `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`
  exists and lease owner is this task. Current `release/obscura-media`
  SHA-256 is
  `c674ae0e0a96cfff0a6708b8878635d804ace5a9adcb40442f19b6a0dd409b13`.
- Input: frozen red in
  `/Volumes/Intel/playground/obscura/m1-cli-robots-lifecycle-r1-20261004/docs/runs/m1-cli-robots-lifecycle-r1-20261004/raw/`.
  Run 1 fails `exercise(true)` at `stream.rs:120` and `exercise(false)` at
  `stream.rs:116`; both are outer `Elapsed`. Run 4 fails only
  `exercise(false)`. Isolated media runs pass at 0.706 s and 0.878 s.
- Input: C2 cross-sample confirms the same two 10 s `Elapsed` failures in
  `full-nextest-rerun`, while `isolate-media-stream` passes at 0.656 s.
- Environment: `CARGO_TARGET_DIR` must be the leased target,
  `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`, and
  `OBSCURA_TEST_BINARY_DIR` must be the leased `target/release`.
- Next: run the original five-case stream test under default nextest
  concurrency, then add fixture-only stage/stderr evidence if needed.

2026-10-04T15:58:12Z / command-bind / INVALID_CONTRACT_COMMAND

- Result: the exact targeted command in the task contract is not executable.
  Cargo exits 101 before build because `obscura-media` has no `render` feature.
- Evidence: `raw/baseline-targeted.stderr.txt` reports
  `the package 'obscura-media' does not contain this feature: render`.
- Correction: use the same package, test, release profile, locked inputs, and
  default nextest concurrency without the inapplicable `render` feature.
  This is a command correction only, not a behavioral red.

2026-10-04T16:00:35Z / isolated-default-baseline / PASS_NO_ROOT

- Result: corrected default-concurrency targeted test passes.
- Command: `CARGO_TARGET_DIR=... CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
  OBSCURA_TEST_BINARY_DIR=... cargo nextest run --locked --release -p
  obscura-media --test stream --no-fail-fast`, exit 0.
- Evidence: `raw/baseline-targeted-corrected.stderr.txt`; five tests pass in
  0.399 s, including `exercise(true)` and `exercise(false)`.
- Digests after rebuild: `release/obscura-media`
  `d35a3480b894aa4c03595e726af25664f91ef7f53bb9c1ab24e3a419e3dc04de`;
  executed stream test binary candidate
  `157aaf8493e316075ab8747d975f057daef00e81f5fc80e1ba06ecf40c022172`.
- Interpretation: slow child startup and unconditional pipe deadlock are
  falsified. A full-suite/default-concurrency interaction remains active.

2026-10-04T16:25:40Z / full-default-diagnostic / MEDIA_PASS_ON_RUN1

- Result: diagnostic full gate exited 100, but the only failure was a Host
  persistent row (`click_receipt_precedes_navigation_and_old_document_input_is_rejected`).
  All five `obscura-media::stream` public cases passed.
- Command: exact full command with `OBSCURA_STREAM_DIAG` set; exit 100.
- Evidence: `raw/diag-full-default.stderr.txt`;
  media results at lines 1997-2002, media durations 5.355-5.521 s.
- Evidence: `raw/diag-full-default.log` shows media child spawn at ~1-3 ms,
  but raw/video accept readiness at ~5.34-5.40 s after exercise start under
  full default concurrency. No child stderr was logged.
- Interpretation: fixture-only stderr draining did not expose an early child
  failure; the delay was before encoder delivery and within the 10 s wrapper.
  Original media red was not reproduced on this purposeful run.

2026-10-04T16:34:00Z / full-default-diagnostic-2 / FULL_PUBLIC_GREEN

- Result: the second diagnostic full gate passed all 1734 tests, including all
  five media stream cases.
- Evidence: `raw/diag-full-default-2.stderr.txt`, exit 0, 1734/1734, 4 skipped;
  media rows pass at 2.518-2.635 s.
- Evidence: `raw/diag-full-default-2.log`; child ffmpeg-failure stderr was
  captured for `exercise(true)`. Both `exercise` children connected after
  about 2.508-2.509 s, so the previous 5.4 s accept delay was variable
  scheduler latency, not a deterministic product or fixture bug.
- Falsification: no early child failure, packet failure, close failure, or
  cleanup failure appeared. No fixture-owned causal root was observed.

2026-10-04T16:36:00Z / restore-and-final-targeted / PASS_RESTORED

- Result: all temporary diagnostic edits were removed. `stream.rs` SHA-256 is
  again `f666cbc337f54473495b381e29fada18141a815986af13fcf23587779fce53ad`,
  identical to the frozen input. Public assertions and 10 s deadlines are
  unchanged.
- Command: corrected targeted default-concurrency stream command, exit 0.
- Evidence: `raw/final-targeted.stderr.txt`; 5/5 pass in 1.586 s.

2026-10-04T16:39:00Z / final-full-default / FULL_PUBLIC_GREEN

- Result: exact full default command with `--features render` passed clean.
- Evidence: `raw/final-full-default.stderr.txt`; 1734/1734 pass, 4 skipped,
  19.435 s; media rows pass at 2.637-2.749 s.
- Digests: `target/release/obscura`
  `2ed75eed3e3a8282a200490a6b76c945103a772b7de2c0a2fd6b3eba0b6e3eb8`;
  `target/release/obscura-media`
  `c674ae0e0a96cfff0a6708b8878635d804ace5a9adcb40442f19b6a0dd409b13`.
- Interpretation: no fixture repair is justified because no causal media red
  exists in this candidate window. The original media timeout was intermittent
  and not reproducible after binding the actual candidate and command.

2026-10-04T16:41:00Z / benchmark / PASS

- Result: rebuilt `obscura` CLI digest changed from the frozen intake, so the
  contract required a benchmark replay. Exact offline obstacle course passed.
- Command: `OBSCURA_BIN=<leased target>/release/obscura python3
  obstacle-course/run.py --runs 1 --warmup 0`, exit 0.
- Evidence: `raw/obstacle-course.stdout.txt`; correctness 33/33.

2026-10-04T16:42:00Z / resources / CLEAN

- Result: no owned `obscura-media` child or `stream-*` test process remains.
- Evidence: exact process scan returned no matching child process.
- Result: source product files, other tests, Cargo/config/hooks, parent
  evidence, leases, and other worktrees were not modified. Only owned run
  evidence remains untracked. The shared target is retained for parent intake.
