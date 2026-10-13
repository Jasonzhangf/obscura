# Token-only remote connection — conversion record (2026-10-11)

Worktree: `/Volumes/Intel/playground/obscura/m1-media-encoder-ownership-fix-r8-20261009`
(base commit `35b58be`; everything below is uncommitted)

## Contract now in the tree

`.abconnection = { format, version, endpoint, token }`. The client sends
`Authorization: Bearer <token>` on the WSS `/control` upgrade. The endpoint
compares that token and answers a mismatch with the typed
`REMOTE_TOKEN_INVALID` frame, then closes. Nothing else authenticates or
authorizes a remote connection. No client certificate, CA, registry,
projection, scope, membership, grant or revision exists on the remote path.

## One secret, one comparison, owned by the Host

| Source | File:line | Meaning |
| --- | --- | --- |
| `--access-token <TOKEN>` | `crates/obscura-host/src/main.rs` | Host-side authoritative compare in `resolve_remote_admission` |

The endpoint has no token flag and no edge compare. It copies the client's
`Authorization: Bearer <token>` value verbatim into the admission frame, so the
Host owns the single comparison. The endpoint maps the Host denial
(`RemoteAdmissionResult::Err`) to the typed client error frame.

## Admission frame

`{ "request_id": <u64>, "token": <string> }` — nothing else.
`crates/obscura-host/src/remote_admission.rs`.
The viewport is not in this frame: the upgrade rejects query strings and the
client declares the viewport on `Command::Attach { mode, viewport }` after
`Ready`.

## Applied receipt

Exactly `schema_version`, `endpoint_generation`, `endpoint_pid`,
`bound_address` (`crates/obscura-host/src/endpoint/server.rs`, local
`EndpointStatusReceipt`). `authorization_revision`, `registry_sha256` and
`projection_sha256` are gone. AB `_verify_endpoint_applied` must stop requiring
them.

## Removed in this conversion

- `crates/obscura-host/src/remote_authorization.rs` — deleted (435 lines:
  `RegistryFile`, `ProjectionFile`, `RemoteScope`, `InitialProfileSelection`,
  `RegistryDevice`, `principal_for_cert`, `authorize`, `grants`).
- `daemon.rs` — `verify_current_authorization`, `remote_request_gate`,
  `RemotePrincipalContext`, `AuthFailure`, `AdminConnState`, `AdminMembershipReceipt`,
  `admin_membership_set`, `validate_membership_devices`, `same_attempt`,
  `Event::Admin`, `handle_admin` membership branch.
- `catalog.rs` — `ProfileAccess`, `MembershipReceipt`, `access_for`,
  `assign_access`, `LOCAL_DEVICE_ID`, the `access` field and its parse/persist
  branch. A profile entry is `{profile_id, display_name, default}`.
- `remote_admission.rs` — `RemotePrincipalContext`, `RemoteScope`,
  `InitialProfileSelection`, `InitialProfileGrant`.
- `endpoint/main.rs` — `--client-ca`, `--authorized-clients`, `--token`,
  `Args::placeholder`.
- `endpoint/server.rs` — client-auth verifier, root store, projection hash,
  `Grant.peer`; TLS is now `with_no_client_auth().with_single_cert(..)`.
- `crates/obscura-host/tests/blackbox_b1_denial_fence.rs` — deleted.
- `endpoint.rs` — the B1 denial-fence case deleted; the three
  `REMOTE_COMMAND_FORBIDDEN` assertions replaced by the real surviving
  behaviour (`AGENT_BUSY`, `OPERATION_REQUIRED`, and a served
  `flush_profiles`); the certificate-crossing media assertion replaced by the
  wrong-token refusal.
- `crates/obscura-media/src/bin/webrtc_receiver.rs` — `--client-cert`,
  `--client-key` and the mTLS client config replaced by `--token` plus the
  `Authorization` header; the `REMOTE_COMMAND_FORBIDDEN` assertion replaced by
  `AGENT_REQUIRED` on an `Evaluate` that carries a real operation identity. A
  mutation without an identity is refused with `OPERATION_REQUIRED` first, so
  the identity is what reaches the surviving observer gate.

Deliberate keeps: endpoint server cert/key (listener identity), the
per-session `/media` grant token from `x-obscura-media-token` (session binding,
not identity), session reserve/spawn and the media/render path, admission
framing and bounds, the local `host-admin.sock` catalog ops, and the TLS
listener itself.

## Evidence

```
cargo test -p obscura-host --features render --test endpoint \
  token_endpoint_preserves_host_and_serves_remote_operations -- --nocapture
test result: ok. 1 passed; 0 failed; finished in 10.04s
```

Log: `docs/runs/m1-task47-admission-ablation-20261011/e2e-token-only-endpoint.log`.
The case drives the real Host and the real endpoint: wrong token →
`REMOTE_TOKEN_INVALID` and no Ready; correct token → Ready v11; remote agent
attach → `AGENT_BUSY`; remote observe attach with a declared viewport; remote
resize without an operation identity → `OPERATION_REQUIRED`; `profile_catalog`
served; remote `flush_profiles` now served; `/media` with the session token
delivers a real `access_unit` 391x701 (coded 392x702); a real click returns
`input.state == "succeeded"` and the local attachment then observes
`window.clicked == 1`; close delivers the `closed` media header; both binaries
stop on SIGTERM.

```
cargo build --release -p obscura-host --features render
shasum -a 256 target/release/obscura-host   # c3ca4a22c1dd2dfc118774f8a749cf78b65f6a4b0e02a9de4f97a21284a190c9
shasum -a 256 target/release/obscura-endpoint # feb03a0bcfc37e1dee2838e46b2fea390160eedd4e8b6c6144d8495ea2f85469
shasum -a 256 target/release/obscura-media   # b79566a434cec7a572e4cdfb9f4d5b76ae6c7b541587370dcadc0242585ec4e8
```

## Converted test files

Every test file that used the removed argv is converted. Recipe per file: delete
the client key/cert generation and the `client.der`/`client.key` writes; replace
`write_authorized(..)` with `write_catalog(root.join("profiles"))?`; host
`--authorization-registry` → `--access-token TOKEN`; endpoint
`--client-ca`/`--authorized-clients`/`--token` and receiver
`--client-cert`/`--client-key` → none / `--token TOKEN`;
`.with_client_auth_cert(..)` → `.with_no_client_auth()`; `connect_control` adds
the `Authorization` header (`tungstenite::client::IntoClientRequest`).

| File | Result |
| --- | --- |
| `endpoint.rs` | 1 passed (10.04s) |
| `blackbox_b4_encoder_revoke.rs` | 1 passed (10.9s) |
| `blackbox_b5_cancel_probe.rs` | 1 passed (8.5s) |
| `blackbox_b6_unrelated_finalize.rs` (renamed from `blackbox_b6_registry_lock.rs`) | 1 passed (11.8s) |
| `blackbox_b7_spawn_rollback.rs` | 1 passed (8.5s) |
| `blackbox_b8_shutdown_join.rs` | 2 passed (42.0s) |
| `blackbox_b9_unsettled_permit.rs` | 1 passed (17.8s) |
| `blackbox_b10_real_endpoint_adapter.rs` | 13 passed (202.1s) |
| `persistent.rs` | 28 passed (38.9s) |
| `remote_admission_r1.rs` | 30 passed (9.9s) |
| `webrtc_endpoint.rs` | 19 passed, 3 ignored, 0 failed (308.0s) |

The three ignored cases are the F5 external-observer devices: they need
`f5-blackbox.py`, which is not in this worktree. They are ignored for that
reason and not for the token change.

Three build and sweep facts:

- `obscura-webrtc-receiver` must be rebuilt with `--features webrtc`
  (`cargo build --release -p obscura-media --features webrtc --bin
  obscura-webrtc-receiver`). The stale `target/release` binary still demanded
  `--client-cert`, which failed 18 of the 19 cases before the rebuild.
- The receiver's evaluation probe must carry an operation identity. A mutation
  without one is refused with `OPERATION_REQUIRED` before the agent gate, so the
  observer's surviving refusal (`AGENT_REQUIRED`) needs
  `operation: Some(operation_for(&status)?)`.
- Ruling 1 deleted the endpoint `--token` flag after `b4`, `b5` and `b7` were
  first converted, so those three still passed `--token` to the endpoint. The
  final sweep found it and removed it; all three are re-run and green above.
  `cargo check --tests -p obscura-host --features render` now compiles every
  obscura-host test target, and no removed flag remains in `crates/*/src` or
  `crates/*/tests`.

Measured evidence from the re-anchored cases:

```
B6: unrelated-session admission completed in 4.204583ms; A adapter alive=true pid=90089
test bb13_wrong_token_is_refused_without_creating_a_session ... ok
```

## Cases whose premise the token-only frame deleted

The admission frame is `{ token, request_id }`: it carries no principal, scope,
grant or initial profile selection. Every remote admission binds the catalog
default profile. A peer reaches another profile only with
`Command::ProfileSelect { profile_id }` plus an operation identity on its own
attachment. Consequences:

- `blackbox_b6` — re-anchored. Peer H holds the default session; peer A selects
  `p1` and its adapter finalizes slowly; a fresh admission to the still bound
  default session must be served in under 2 s while A's adapter is alive. The
  old "second session served during the first finalize" probe is unreachable,
  because no admission can create that second session on its own.
- `remote_admission_r1.rs` — deleted `rp02` (remote owner record), `rp04`
  (reduced scope set) and `rp11` (unknown initial selection); re-anchored `rp21`
  (two peers land on the default, one selects `p1` with an operation, so the two
  routes must stay distinct); renamed `rp13`; added the BB13 wrong-token case
  (`REMOTE_TOKEN_INVALID`, no session id, no new session socket).
- `webrtc_endpoint.rs` — re-anchored the two-receiver case: both receivers now
  present the one token and bind the default session, so the case asserts two
  concurrent streams on one session instead of two distinct sessions.
- `persistent.rs` — keeps the `host-admin.sock` catalog ops and their two cases.
  The AB Mac bridge reads that socket as its liveness probe
  (`admin_socket_available()` → `provisioning_available`), so removing the
  listener would close the provisioning sheet. Follow-up: re-anchor
  `provisioning_available` on the service trust root, then delete the ops.
