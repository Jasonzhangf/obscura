# Core Host C2 run notes

Input: branch `codex/m1-core-host-c2-r1-20261004`, base `2306122cc00f88c9b0963aafc50c6fd57f0da4fd` /
tree `0ecc8670b6269aa4ac61a22290855124f645e353`. Target lease
`/Volumes/Intel/playground/obscura/m1-engine-reconcile-20261002/target`.

| time/node | state | evidence | input/env | next |
| --- | --- | --- | --- | --- |
| baseline F1/F3 | reproduced | `raw/baseline-remote-editing.{exit,stdout,stderr}` exit 100, request 39 `STALE_EDITOR_REVISION` at remote_editing.rs:515 | frozen candidate, render | fix Evaluate completion context |
| F1/F3 fix | done | `raw/f1-f4-focused.*` exit 0; `editor_context_public_c2.rs` covers evaluate mutation -> new binding, stale binding refused with no side effect, real blur -> null | worker.rs Evaluate refresh | F2 |
| F2 fix | done | `raw/f2-lease-cancel.*` exit 0; `lease_cancel_r2.rs` disconnect begin/update -> Gone -> same target reattach -> lease null, composition null, value restored, new begin pending; direct same-session cancel green | daemon.rs disconnect terminal delivery via existing Command::Input path | F5/F6 |
| F5/F6 fix | done | `raw/f5-f6-profile-replay.*` exit 0; `profile_replay_r2.rs` replay receipt identical, conflict/old-seq/stale-binding rejected, catalog stable; close flushes before Closed, cold restart value, real EISDIR failure -> PROFILE_STORAGE_FAILED and recoverable | daemon.rs HostMutation admission, worker.rs close flush | targeted gate |
| targeted gate | pass | `raw/targeted-gate.*` exit 0, 17/17 across remote_editing, navigation_profile, editor_events_public_r2, editor_context_public_c2, lease_cancel_r2, profile_replay_r2 | frozen candidate, render | full gate |
| full nextest default | fail out of scope | `raw/full-nextest-aux.*` exit 100, 1729/1733; only 4 `obscura-cli::obey_robots` tests fail at their 15s/5s fixture deadlines under default concurrency; all 4 pass isolated (`raw/isolate-obey-robots.*`) | default nextest concurrency, webrtc aux via `OBSCURA_TEST_BINARY_DIR=/private/tmp/obscura-host-c2-r1-aux` | serial full gate |
| full nextest serial | pass | `raw/full-nextest-threads1.*` exit 0, 1733 passed, 4 skipped, 0 failed, 138.907s | same target, same tests, `--test-threads 1`, webrtc aux | CLI build |
| CLI build | pass | `raw/cli-build.*` exit 0 | frozen candidate, render | benchmark |
| graph validation | pass | `raw/graph-{editing,control,profile,connection}.*` exit 0; admitted graphs valid, no new cross-owner edge added | r4 admitted graph files | commit |
| final Host build | pass | `raw/host-build-final.*` exit 0; `raw/host-sha256-final.txt` Host sha256 `ba566a3a078f1793299e5e6a638ebde0ea33303b3688ca65347e0543da785f37` | final commit source, render | public harness |
| public harness | pass | `raw/harness-composition-cancel-final.*` exit 0 result PASS; `raw/harness-storage-clear-restart-final.*` exit 0 result UNREPRODUCED, cold_get null, resurrection false | admitted `host_probe.py`, final Host sha256 `ba566a3a...` | benchmark |
| obstacle course | pass | `raw/obstacle-course.*` exit 0, 33/33 stages, median latency 3043.0ms | benchmark HEAD `8bd11ade...` tree `de59c6c1...`, CLI sha256 `ad44a6ed...` | commit |

Raw root: `docs/runs/m1-core-host-c2-r1-20261004/raw/`.

The exact default-concurrency full command remains red only because the
out-of-scope `obscura-cli/tests/obey_robots.rs` fixture servers expire while
the suite saturates the host. The same 1733-test suite passes with one test
thread. Handoff owner: Obscura CLI test harness; proposed change is to keep the
fixture server alive until the test drops it instead of using fixed 15-second
and 5-second wall-clock limits.
