# M1 browsing/profile ABI freeze and consumer handoff

Status: `ABI_FROZEN`. Source of truth: `protocol/browser/src/lib.rs` on branch
`codex/m1-profile-navigation-impl-r1-20261003`. This is the only browser wire;
AB consumes it and must not mirror it into a second protocol.

| Item | Value |
| --- | --- |
| OB base | fresh fork/main `948f054b51af442306be046d0014cfa3da628f27` |
| W2 source | commit `ff50bd99e959189f697f69f7472d7a82e66e2eff`, tree `48a9486e5735d7bc99a6761700caf88e4c36d367` |
| Implementation HEAD | `e7db23ba5ea32f5ba4591a67990c5dfdc70a597b`, tree `48a9486e5735d7bc99a6761700caf88e4c36d367` |
| Protocol crate check | `CARGO_TARGET_DIR=/tmp/ob-proto-check cargo check --locked -p obscura-host-protocol` exit 0 |
| Version pair | `HOST_PROTOCOL_VERSION = 11`, `WEBRTC_PROTOCOL_VERSION = 10` |

Version 11/10 is a deliberate bump. `SessionStatus`, `PresentedFrameIdentity`,
`FrameInfo`, `InputBinding::Editor` and `ResultValue` are strict
(`deny_unknown_fields`), so adding tab/presentation/profile fields while keeping
Host10/WebRTC9 is not a compatible change. Every consumer must move together.

## New commands

All are `Command` variants in `protocol/browser/src/lib.rs`.

```text
Back
Forward
Reload
TabCreate {}
TabSelect { tab_id: String }
TabClose { tab_id: String }
ProfileCatalog {}
ProfileCreate { display_name: String, make_default: bool }
ProfileSelect { profile_id: String }
FlushProfiles {}
```

Authorization classes:

- `Back` / `Forward` / `Reload` / `TabCreate` / `TabSelect` / `TabClose` are
  controlled browser mutations: they carry `Operation` identity and require the
  same control ownership as `Navigate` (Agent holder, or human holder).
- `ProfileCatalog` is a read-only query: no `Operation`, no write authority, no
  worker start. Any attached connection may read it.
- `ProfileCreate` / `ProfileSelect` are attachment/profile mutations: they carry
  `Operation` identity but do not require page control ownership. They never
  grant page write authority; a rebind lands the caller as an observer.
- `FlushProfiles` is the Host-local maintenance drain. It carries no
  `Operation`, refuses with `MAINTENANCE_BUSY` while an operation or another
  drain is in flight, and flushes every loaded profile store before answering.
  The service may terminate the process only after it sees
  `ProfileFlush { flushed: true, .. }`. It is never permitted on the mTLS
  endpoint and never carries a control-plane truth in payload metadata.

`ProfileSelect` input is profile id only. A profile root or filesystem path is
forbidden on the wire; the root comes from Host CLI/service configuration.

## New result values

```text
ResultValue::ProfileCatalog { catalog: ProfileCatalog }
ResultValue::ProfileCreated { profile: ProfileRecord, catalog: ProfileCatalog }
ResultValue::ProfileSelected { binding: ProfileBinding, status: SessionStatus }
ResultValue::ProfileFlush { flushed: bool, profiles: u32 }
```

`Back` / `Forward` / `Reload` / `TabCreate` / `TabSelect` / `TabClose` return
`Ok(None)`, which the daemon answers with the authoritative `SessionStatus`
after the operation, exactly like `Navigate`.

New supporting structs: `TabSummary`, `NavigationState`, `ProfileRecord`,
`ProfileCatalog`, `ProfileBinding`.

## Status projection

`SessionStatus` gains:

```text
profile_id: String
presentation_revision: u64
active_tab_id: String
tabs: Vec<TabSummary>
navigation: NavigationState
```

`navigation.url` / `navigation.title` and each `TabSummary` URL/title are read
from the actual Page after the committed operation. The UI mirrors this and
never becomes a second tab/history registry.

## Fences

`PresentedFrameIdentity` gains `tab_id: String` and
`presentation_revision: u64`. `FrameInfo` gains the same two fields.

Host validation for input envelopes:

- `frame.session_id` == session, `frame.tab_id` == `active_tab_id`,
  `frame.presentation_revision` == `presentation_revision`,
  `frame.document_revision` == current, `frame.viewport_revision` == current.

This rejects an A -> B -> A stale frame/editor binding even though the tab id is
again A: `presentation_revision` differs. Editor bindings are additionally
fenced by the session `document_revision`, which bumps on every committed
active-document change (navigate, history, reload, tab create/select/close).

## Typed errors

New stable codes: `NO_HISTORY`, `UNKNOWN_TAB`, `LAST_TAB`, `PROFILE_NOT_FOUND`,
`PROFILE_ROOT_UNCONFIGURED`, `PROFILE_CATALOG_FAILED`, `PROFILE_STORAGE_FAILED`,
`REBIND_BUSY`, `PROFILE_LIMIT`, `MAINTENANCE_BUSY`. Existing codes are
unchanged.

## Remote principal scope

`ProfileCatalog` / `ProfileCreate` / `ProfileSelect` are **not** in the mTLS
endpoint `permitted_remote_command` allowlist. A paired remote principal
carries no Host-level profile scope, so those commands return
`REMOTE_COMMAND_FORBIDDEN` there. They are actionable only over the local
authenticated Host socket. This is a deliberate refusal, not a widened
allowlist: do not add them back to the remote boundary without binding an
explicit typed profile scope and target-profile access check.

## Consumer migration scope

Must move to Host11/WebRTC10 in the same delivery:

- OB `crates/obscura-host/tests/endpoint.rs` (`ready["version"]` literal 10 -> 11).
- OB `crates/obscura-host/tests/persistent.rs`, `remote_editing.rs`,
  `webrtc_endpoint.rs` frame/editor identities (add `tab_id`,
  `presentation_revision`).
- OB `crates/obscura-media/**` uses `WEBRTC_PROTOCOL_VERSION` symbolically; the
  version bump flows through with no source edit.
- AB `packages/client-connection/src/transport.rs` (currently pins version 4).
- AB native op map `packages/android-bridge/src/lib.rs` and Android
  `MainActivity.java` (add back/forward/reload/tab/profile op numbers).
- AB `packages/client-domain/probe.ts` (add navigation/tab/profile fields).
- AB `scripts/host-service/host_service.py` (version pin and `profile_root`
  argv/schema).

None of these AB files are edited by this writer.
