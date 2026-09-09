# M1 desktop WebRTC probe handoff

## Tree and scope

- Worktree: `/Volumes/extension/code/AgentBrowser/playground/obscura-fork/playground/m1-desktop-webrtc`
- Branch: `codex/m1-desktop-webrtc`
- Base: `origin/main` = `72c84adcc6ec3ea4a7144adb4e45d4d3038ebcda`
- Base committed tree: `7a714bfbbcb5e61e8ebadb20578a2addbdeb62a3`
- Candidate scope is committed on this branch; no merge, push, install,
  restart, or cleanup has occurred.
- Collab remains unavailable because no live tmux pane exists; this work is
  isolated and has no shared writes.

Changed scope:

- `crates/obscura-media/Cargo.toml`
- `crates/obscura-media/src/lib.rs`
- `crates/obscura-media/src/webrtc.rs`
- `crates/obscura-media/tests/webrtc_probe.rs`
- `protocol/browser/src/lib.rs`
- `Cargo.lock`

The probe is feature-gated as `webrtc-probe`, uses two independent loopback
UDP sockets and in-memory SDP only, and leaves `product_enabled=false`. It does
not add a Host endpoint, WSS fallback, Android/Mac client integration, Relay
signaling, or product rollout.

## Proven

Latest full feature-gated run:

```text
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
CARGO_TARGET_DIR=/tmp/obscura-target-m1-desktop-webrtc \
cargo nextest run --release -p obscura-media \
  --features webrtc-probe --no-fail-fast --no-capture
```

Result: 7 tests passed, 0 failed, 0 skipped (`bab9d56f-34b3-4cff-86b9-4c54efaba06d`).

The positive report from that run proved:

- selected candidates: `127.0.0.1:56804` and `127.0.0.1:49690`, both `host/udp`;
- network path `local`, transport `udp`, codec `h264_annex_b`;
- DataChannel `obscura.control.v1`, `hello_ack=true`, `ping_pong=true`;
- 4 received RTP packets;
- 1064-byte reassembled H.264 access unit;
- 12288 decoded RGBA bytes and checksum `6375394345140543121`;
- `product_enabled=false`.

Additional evidence:

- Positive probe stress run: 5/5 iterations passed
  (`77bdb598-a85c-40a6-81c1-c9cf74df178c`).
- Default `obscura-media` nextest without the feature: 4/4 passed
  (`744c8b4a-398c-44bf-b6f7-faa7669adb06`).
- `cargo build --release -p obscura-host --features render`: passed.
- `cargo build --release -p obscura-media --features webrtc-probe`: passed.
- Targeted rustfmt checks for the two new Rust files: passed.
- `git diff --check` and whitespace checks for both new files: passed.
- Full `cargo fmt --all -- --check` is not clean because the inherited tree has
  unrelated pre-existing formatting drift; it was not used to rewrite other
  owners' files.

The first failing behavior was isolated: one sample opens the remote track but
does not leave a packet observable through `TrackRemote::poll`; the probe now
sends the second sample only after the receiver reports the typed track-open
signal. This is probe pacing, not a product retry/fallback.

## Not proven / next gate

- No product endpoint integration or client decode/display path is implemented.
- No Android, Mac, Relay, Tailscale, public-network, WSS, or deployed-entrypoint
  evidence exists.
- No install/restart evidence applies to this isolated library probe.
- AGY review `m1-desktop-webrtc-20260905-r2` passed with controller verdict
  `pass`, `failureClass=null`, `findings=[]`, and recommendation `deliver`.
  Receipt: `.agent-collab/review/m1-desktop-webrtc-20260905-r2/review.final.md`.
- Candidate commit identity review receipt (final delivery gate):
  `.agent-collab/review/m1-desktop-webrtc-20260905-final/review.final.md`.
- Mainline merge, remote receipt, freeze, and worktree cleanup remain pending.
