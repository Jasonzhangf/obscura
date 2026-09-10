# Integration Host replay verification

This record binds the replay outputs in this directory to the reviewed
integration candidate.

| Field | Value |
| --- | --- |
| Working tree | `/Volumes/extension/code/obscura/playground/m1-obscura-integration-c12-20260910` |
| Source commit | `fa38663f3095a51a6d3c7728937d337e3f19ea13` |
| Source tree | `470dba780e6545bc16b4aa2f28da34e5abbe4161` |
| Host binary SHA-256 | `51e3d737ba731dfb5fda5bf5769ae5c1eaff4d7bc9afe09e9e84a8df3dc83bb9` |
| Media binary SHA-256 | `955a026b36dc92c90c6f6ee693e338831e65a34bc3dcc759bbb562ca2365a283` |
| FFmpeg | `ffprobe version 8.0.1` |
| Replay exit status | `0` |
| Replay result | `pass: true; flow: Host -> raw RGBA -> H264 -> decoded pixel change -> resize preserves form -> close` |

Exact command:

```sh
python3 crates/obscura-media/tests/host-replay.py \
  target/release/obscura-host target/release/obscura-media \
  evidence/m1-obscura-integration-c12/host-replay-20260910-1610
```

Output hashes:

| File | SHA-256 |
| --- | --- |
| `before.h264` | `c938ee51e31aab6615fe1a8ba175b8cd900418d894053c8f87460e8af655e675` |
| `after.h264` | `d5e4d2f51a31880acdc1c961f16342cfbc044d9a288218009779a562b7fbfaa2` |
| `resized.h264` | `9285bb3062d9608c54af3667cb6e985ee854926903d463a45eadca32eb343f91` |
| `resized.json` | `dc4684c4149561695e4a71da47f11aad32b72d28f31b7fa6c3d1566576da812e` |

The empty `host.log` and `media.log` are retained because the replay harness
captures process stderr there; the successful result is recorded above.
