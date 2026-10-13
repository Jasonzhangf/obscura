# Obscura CLI robots fixture lifecycle r1 notes

| time/node | state | evidence | input/env | next |
| --- | --- | --- | --- | --- |
| 2026-10-04T13:28:17Z / bind | confirmed | `git status --short --branch` clean; HEAD `6f3ce925f50a2de6e4aead6c184157da07a23f1f`; tree `f632e5c786f6b9ed39a53fda13c655b1e44e9421`; target lease owner is `m1-cli-robots-lifecycle-r1-20261004` with no readers | assigned worktree; `CARGO_TARGET_DIR=/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` | inspect frozen failure and edit only `crates/obscura-cli/tests/obey_robots.rs` |
| 2026-10-04T13:28:17Z / frozen-red | confirmed | C2 `full-nextest-aux` exit 100: fetch cases fail after 15.4s with navigation network errors; scrape case fails after 20.4s with `error:"timeout"` at 5013ms. C2 `full-nextest-threads4` exit 100: three fetch cases pass, scrape alone fails at 5.021s. C2 `full-nextest-threads1` exit 0: 1733 passed, 4 skipped. | C2 raw `/Volumes/Intel/playground/obscura/m1-core-host-c2-r1-20261004/docs/runs/m1-core-host-c2-r1-20261004/raw/` | separate fixture expiry from scrape timeout |
| 2026-10-04T13:28:17Z / scrape-causality | confirmed boundary | The frozen threads4 run proves the scrape timeout can remain after the three fetch cases pass. Prior read-only diagnostic `robots-startup-r4.final.md` ran 6500 exact-entry cases with no timeout and found the 5s budget spans worker exec, stdin/stdout readiness, and robots navigation. No causal leg was proven for the remaining timeout. | `full-nextest-threads4.stderr`; `robots-startup-r4.final.md` | do not raise or remove the scrape timeout without a fresh causal failure |
| 2026-10-04T13:28:17Z / fixture-owner | confirmed | `RobotsServer::spawn` stores no join handle or shutdown signal; its listener thread exits at `Instant::now() + 15s`. The listener is therefore not owned by the test lifetime. | `crates/obscura-cli/tests/obey_robots.rs` | add explicit stdlib shutdown and Drop join, preserving CLI behavior assertions |
| 2026-10-04T14:01:18Z / targeted-default | confirmed | After fixture ownership fix, `cargo nextest ... -p obscura-cli --test obey_robots --no-fail-fast` exits 100 consistently: 4 pass, scrape fails with `error:"timeout"` at 5000ms. Direct `obey_robots-67e8... --test-threads=5` passes in 0.20s; isolated nextest scrape passes in 6.5s. Worker sample during the failing nextest run shows `_dyld_start` with 112K resident. | `raw/targeted-default*`, `raw/targeted-worker-sample/worker-sample.txt`, `raw/targeted-exact-only` | separate scrape budget from product policy |
| 2026-10-04T14:01:18Z / scrape-budget | confirmed | The scrape timeout is an independent process-startup/loader stall under concurrent nextest processes, not a robots policy failure. The 5s `--timeout` was a test-imposed worker budget and is not part of the robots assertion. Removed it so the scrape case uses the CLI default. | same raw; worker sample `_dyld_start`; `run_parallel_scrape` worker timeout spans spawn | rerun targeted default, then full default gate |
| 2026-10-04T15:10:00Z / targeted-default | confirmed | `cargo nextest run --locked --release --features render -p obscura-cli --test obey_robots --no-fail-fast` exit 0: 5 passed, 0 skipped. Scrape case passes in 12.66s with the CLI default budget. | `raw/targeted-default-3/{stdout,stderr,exit}.txt`; shared target | run required original full default gate |
| 2026-10-04T15:25:00Z / full-default | failed (out of scope) | Exact `cargo nextest ... --features render --no-fail-fast` default concurrency exit 100: 1730 passed, 4 failed, 4 skipped. All five `obey_robots` cases PASS (fetch 14.4s, scrape 35.1s). Failures: `obscura-host::persistent click_receipt_precedes_navigation...` (WouldBlock at 8.39s), `obscura-host::endpoint mutual_tls_endpoint...` (listener did not initialize), `obscura-media::stream public_adapter_reports_encoder_failure_and_exits`, `obscura-media::stream public_adapter_preserves_host_identity_through_resize_and_close` (10s Elapsed). | `raw/full-default/{stdout,stderr,exit}.txt`; `OBSCURA_TEST_BINARY_DIR=<shared target>/release` | isolate the four failures to test load vs deterministic regression |
| 2026-10-04T15:30:00Z / failed-isolated | confirmed | The same four failures pass together in isolation: exit 0, 4/4 passed. They are load-sensitive, not deterministic. | `raw/failed-isolated/{stdout,stderr,exit}.txt` | compare against C2 baseline and rerun full gate |
| 2026-10-04T15:35:00Z / flake-baseline | confirmed pre-existing | C2 baseline (no robots edit) shows the same out-of-scope flakes: `page_transport_prefetches_once_and_capture_reuses_the_bytes` FAILED in `full-nextest`; `mutual_tls_endpoint...`, `public_adapter_preserves_host_identity...`, `public_adapter_reports_encoder_failure...` FAILED in `full-nextest`/`full-nextest-rerun`. All pass in C2 `threads1`/`threads4`. | C2 raw `/Volumes/Intel/playground/obscura/m1-core-host-c2-r1-20261004/docs/runs/m1-core-host-c2-r1-20261004/raw/` | rerun full default gate; robots cases expected green |
| 2026-10-04T15:40:00Z / full-default-2 | failed (out of scope) | Default concurrency exit 100: 1733 passed, 1 failed, 4 skipped. All five `obey_robots` cases PASS (scrape 27.99s). Only failure: `obscura-host::endpoint mutual_tls_endpoint...` `listener did not initialize` (10s deadline). Media cases passed this run. | `raw/full-default-2/{stdout,stderr,exit}.txt` | rerun to test flake randomness |
| 2026-10-04T15:45:00Z / full-default-3 | failed (out of scope) | Default concurrency exit 100: 1732 passed, 2 failed, 4 skipped. All five `obey_robots` cases PASS (scrape 24.65s). Failures: `obscura-browser page::tests::page_transport_prefetches_once_and_capture_reuses_the_bytes` (also failed in C2 baseline) and `obscura-host::persistent sleeping_page_remains_responsive_and_fetch_finishes_after_disconnect`. Endpoint/media passed this run. | `raw/full-default-3/{stdout,stderr,exit}.txt` | three runs, three different out-of-scope failure sets; robots cases green 3/3 |
| 2026-10-04T15:47:00Z / root-cause | confirmed | The remaining gate failures are pre-existing Host/Media/Browser load-sensitive tests with tight internal deadlines (10s socket init, 10s encoder wait, nonblocking read). They are read-only scope for this task. The robots fixture root cause is repaired and verified. | `full-default*`, `failed-isolated`, C2 raw | record INCOMPLETE for the exact full gate with first error and proposal; do not claim PASS |
| 2026-10-04T15:50:00Z / full-default-4 | failed (out of scope) | Default concurrency exit 100: 1732 passed, 2 failed, 4 skipped. All five `obey_robots` cases PASS (scrape 29.37s). Failures: `obscura-host::endpoint mutual_tls_endpoint...` (10s listener deadline) and `obscura-media::stream public_adapter_preserves_host_identity_through_resize_and_close` (10s Elapsed). Both failed in C2 baseline runs too. | `raw/full-default-4/{stdout,stderr,exit}.txt` | stop rerunning; four runs give four different out-of-scope failure sets |
| 2026-10-04T15:52:00Z / gate-summary | confirmed | Four exact default-concurrency full runs all exit 100 on out-of-scope flakes; the five `obey_robots` cases pass in every run (4/4). Failure sets: run1 host-persistent+endpoint+2 media; run2 endpoint; run3 browser-page+host-persistent; run4 endpoint+media. All four unique tests pass in isolation. C2 baseline shows the same failures without this edit. The robots blocker is fixed; the exact gate is still red only from pre-existing Host/Media/Browser load sensitivity. | `raw/full-default*`, `raw/failed-isolated`, C2 `raw/full-nextest*` | commit allowlist and report INCOMPLETE for the exact full gate |
| 2026-10-04T16:00:00Z / cli-identity | confirmed | Product diff vs `6f3ce925` is test-only: `git diff --name-only 6f3ce925 -- . ':!docs/runs'` returns only `crates/obscura-cli/tests/obey_robots.rs`. The shared `target/release/obscura` was rebuilt by the test build (sha256 `1d57258e...`, not C2 `ad44a6ed...`), so the on-disk digest changed while the product source is unchanged. | `git diff --name-only`, `shasum -a 256 target/release/obscura` | replay obstacle course with the current binary to confirm 33/33 |
| 2026-10-04T16:02:00Z / obstacle-course | confirmed | `OBSCURA_BIN=<shared target>/release/obscura python3 obstacle-course/run.py --runs 1 --warmup 0` exit 0: correctness 33/33 stages passed, median latency 3031.9ms. Benchmark HEAD `8bd11ade523fd7358c7b1fddc398b48e9040c3c9`. | `raw/obstacle-course/{stdout,stderr,exit}.txt` | commit allowlist; reuse C2 CLI identity, fresh 33/33 bound to current binary |

## Exact full-gate first error

Run 1 `raw/full-default/stderr.txt` first failure:

```
FAIL [   8.391s] obscura-host::persistent click_receipt_precedes_navigation_and_old_document_input_is_rejected
panicked at crates/obscura-host/tests/persistent.rs:544:82:
called `Result::unwrap()` on an `Err` value: Os { code: 35, kind: WouldBlock, ... }
```

The failure is in `obscura-host`, not in `crates/obscura-cli/tests/obey_robots.rs`.
The same test passes in isolation (`raw/failed-isolated/exit.txt` = 0).

## Proposed owner action

Owner: `obscura-host` and `obscura-media` test authors. The remaining default
full-gate failures are load-sensitive internal deadlines in read-only scopes
(10s endpoint listener init, 10s media encoder wait, nonblocking host read).
They reproduce in the C2 baseline without the robots edit. A narrow successor
should give those tests load-tolerant deadlines or isolate their startup budget
from nextest default concurrency. This task cannot edit them.

Separately, `run_parallel_scrape` folds worker process spawn and V8 startup into
the scrape `--timeout` budget. The test no longer injects a 5s budget, so the
CLI default (60s) applies. A product-level successor could separate worker
startup from the navigation budget in `crates/obscura-cli/src/main.rs`, but that
is out of this task's test-only write scope.

## Resource receipt

- Worktree `/Volumes/Intel/playground/obscura/m1-cli-robots-lifecycle-r1-20261004` retained for parent review.
- Shared target `/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target` retained and not deleted or copied.
- Owned fixture sockets bound to `127.0.0.1:0`, closed by `Drop`; no socket files.
- Owned CLI/worker children reaped by `Command::output`; no orphan `obscura-worker` or `obey_robots` process at close.
- No installed Host/service restart, no device, no network injection, no second V8/target cache.
- CLI sha256 `1d57258e853638a0f2670312db282cd1c0c786ee72ace0b1c7c21124cab4cefb` (test rebuild of the unchanged product source; C2 identity `ad44a6ed...`).
- `obscura-worker` sha256 `76c5e84325b48b1ebba2d940fe4e042f913b298829fe228cd53f79b8b2c4a646`.
- Commands and exits: targeted nextest `0` (5/5); full nextest default runs 1-4 `100` (robots 5/5 each, only out-of-scope flakes); failed-isolated nextest `0` (4/4); obstacle course `0` (33/33).
