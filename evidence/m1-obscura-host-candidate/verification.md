# M1 Obscura Host candidate verification

This evidence belongs to the isolated candidate worktree:

```text
worktree: /Volumes/extension/code/obscura/playground/m1-obscura-host-candidate-20260910
branch: codex/m1-obscura-host-candidate-20260910
source baseline HEAD: 38ba617b276427ada945509b66583156a0d8fced
source baseline tree: d0d9b500db5a9913705a6d7ffc061b63fcd9fa14
baseline status: clean before evidence files
```

The source tree is unchanged from the stated baseline. The only retained
change in this candidate result is this evidence directory. No merge, push,
release, or M1 completion claim is made here.

## Ordered verification results

All commands below ran from the candidate worktree. Exit status is the shell
exit status reported by the command.

1. The first broad persistent-host attempt used the default nextest
   concurrency:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test persistent
   ```

   Exit status: `1`. Nextest started 25 tests; `0 passed, 25 failed`.
   Every failure stopped at `persistent.rs:32` with `daemon startup timed out`.
   This run launched all 25 daemon fixtures concurrently.

2. The same persistent suite with one test process at a time:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test persistent -j 1
   ```

   Exit status: `0`. Nextest run ID
   `1c3d0e7e-7303-4387-9717-0b2a4b974a77`; `25 tests run: 25 passed, 0
   skipped` in `82.056s`. The first daemon took `53.573s` to start and the
   remaining tests passed after it completed. This establishes a concurrency
   and startup-resource boundary; it does not prove a Host source defect.

3. The initial WebRTC endpoint attempt before its required media artifact was
   built:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test webrtc_endpoint
   ```

   Exit status: `100`. Both tests failed before endpoint assertions with the
   explicit precondition `Build target/release/obscura-media with --features
   webrtc first`.

4. The required media binary was then built with the feature it declares:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-media --features webrtc
   ```

   Exit status: `0`; release profile finished successfully.

5. With that artifact present, the endpoint suite passed:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test webrtc_endpoint
   ```

   Exit status: `0`. Final run ID
   `fcb3d692-5a65-4069-8822-d217e6a29038`; `2 tests run: 2 passed, 0
   skipped` in `6.361s`. This includes the continuous Host media and stale
   binding checks and the typed encoder-unavailable receiver check.

6. A command copied from the broad media invocation was invalid because this
   crate has no `render` feature:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-media --test stream
   ```

   Exit status: `101`; Cargo reported `the package 'obscura-media' does not
   contain this feature: render`. The valid media tests were run without that
   feature.

7. Media stream adapter tests:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release -p obscura-media --test stream
   ```

   Exit status: `0`; `5 tests run: 5 passed, 0 skipped` in `0.494s`.

8. Encoder tests:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release -p obscura-media --test encoder
   ```

   Exit status: `0`; `2 tests run: 2 passed, 0 skipped` in `0.107s`.

9. Local WebRTC transport probe:

   ```sh
   CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features webrtc-probe -p obscura-media --test webrtc_probe
   ```

   Exit status: `0`; run ID
   `77d33ff7-ced6-40ac-908e-4bbb6192c27e`; `3 tests run: 3 passed, 0
   skipped` in `1.714s`. The passing cases cover local UDP RTP decode and
   typed control roundtrip, codec/version rejection, and auth-binding
   rejection before transport setup.

10. Host release build:

    ```sh
    CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-host --features render
    ```

    Exit status: `0`; release profile finished successfully. Existing
    `cosmic-text` lifetime and `obscura-render` unused-variable warnings were
    emitted and did not fail the build.

11. Media release build at the final source state was repeated after the
    endpoint audit:

    ```sh
    CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-media --features webrtc
    ```

    Exit status: `0`; release profile finished successfully.

12. Actual Host entrypoint replay, using a fresh evidence subdirectory:

    ```sh
    python3 crates/obscura-media/tests/host-replay.py target/release/obscura-host target/release/obscura-media evidence/m1-obscura-host-candidate/host-replay-20260910-0755
    ```

    Exit status: `0`. The script returned:

    ```json
    {"pass": true, "flow": "Host -> raw RGBA -> H264 -> decoded pixel change -> resize preserves form -> close", "evidence": "evidence/m1-obscura-host-candidate/host-replay-20260910-0755"}
    ```

    The subdirectory contains `before.h264`, `after.h264`, `resized.h264`,
    `resized.json`, and empty `host.log`/`media.log` files.

13. Final endpoint WSS test after the exploratory audit was reverted:

    ```sh
    CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test endpoint
    ```

    Exit status: `0`; run ID
    `82d8c1c4-74cf-416a-95fe-6ecd14aac2b5`; `1 test run: 1 passed, 0
    skipped` in `3.747s`.

## Audit decision and retained source scope

An exploratory test added during the audit asserted that remote `navigate`
must be rejected, based on `docs/Persistent-host.md:334-336`. Before the
corresponding source edit, the test failed with the actual response
`OPERATION_REQUIRED` instead of `REMOTE_COMMAND_FORBIDDEN`, proving that the
current endpoint whitelist admits the command. Removing the whitelist item
made the existing WebRTC endpoint acceptance fail: its real receiver uses a
human-controlled DataChannel navigation to construct the page fixture and
returned `REMOTE_COMMAND_FORBIDDEN`. The exploratory test and source edit
were both reverted. The final source tree therefore has no product or test
diff beyond the pre-existing candidate baseline.

The navigation rule conflict remains an explicit follow-up: the endpoint
implementation at `crates/obscura-host/src/endpoint/channels.rs:149-157`
permits `Command::Navigate`, while `docs/Persistent-host.md:334-336` and
`AgentBrowser/docs/architecture.md:61` describe navigation as endpoint
forbidden. Resolving that conflict needs an owner decision that preserves or
updates the existing WebRTC acceptance contract; this evidence-only result
does not silently choose one.

The broad build snippets at `docs/Persistent-host.md:287` and `:429` also
combine `-p obscura-host -p obscura-media --features render`, which Cargo
rejects because `render` belongs to `obscura-host`, not `obscura-media`.
Separate host/render and media/webrtc builds above are the executable build
evidence.

## Evidence limits

These results cover the candidate Host/daemon, local raw-frame and H.264
adapter, direct mTLS/WSS endpoint, and loopback WebRTC probe. They do not
prove native Mac or Android client integration, Relay behavior, Tailscale or
other network paths, native viewport measurement/presentation, installed
artifacts, deployment, or the repository-wide obstacle-course 33/33 gate.
M1 is therefore not declared complete.
