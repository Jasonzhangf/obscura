# AgentBrowser persistent Host: implementation boundary

Status: local bootstrap implemented; full product Host remains in progress. Fork baseline:
`72c84adcc6ec3ea4a7144adb4e45d4d3038ebcda`. Owner: persistent-host task,
branch `work/persistent-host`. Existing upstream checkout is read-only.

## Next usable milestone

User priority: finish usable foundations before rebuilding AppSDK memory. Memory
reconstruction is deferred until this milestone; it is not a prerequisite for
implementation. Existing correctness and safety checks remain in force.

The milestone is a real Mac Host + Android 15T browser flow, not isolated probes:

1. Open a real page on the Mac Host; the phone displays that same page and can
   detach/reconnect without resetting its form or JS state.
2. Phone UI explicitly chooses observe or takeover. Observe leaves Agent work
   running. Takeover waits only for the current atomic operation, fences new
   Agent input immediately, and grants human control only after the terminal
   receipt. Explicit release returns control to Agent.
3. Human click, text input and scrolling work through the Host owner. All
   observers share the phone-sized layout; rotation preserves page state.
4. Repeat the full flow on 15T, including disconnect/reconnect and a rejected
   stale input. Distinguish completed operations from unknown outcomes.

Execution order: Host operation/control owner -> client connection and shared
frame path -> phone interaction -> whole-flow replay. Codec optimization, extra
platform polish and memory reconstruction follow the usable milestone. Existing
benchmark discrepancies remain recorded; they must not be disguised as passing
or pull ordinary development into a separate memory/governance project.

After all milestone flows and applicable gates pass, establish a reproducible
baseline and milestone version, then rebuild AppSDK memory from that frozen
version (explicit user request). The baseline binds AgentBrowser and Obscura
commit IDs, protocol/app versions, build commands, artifact checksums, Mac/15T
environment and actual acceptance evidence. Do not tag a local probe as the
usable milestone or rebuild memory before whole-flow acceptance.

## What the current source establishes

`crates/obscura-cdp/src/server.rs::run_connection` creates a dedicated connection
thread. `cdp_processor` owns `CdpContext`; its receive loop exits when its command
channel closes. `CdpContext::pages` in `dispatch.rs` owns the Page instances.
This arrangement isolates V8 by thread, but ties live document lifetime to the
connection. Cookie persistence does not preserve DOM, forms, timers or JS state.

`Page::set_viewport` in `crates/obscura-browser/src/page.rs` already updates an
existing page. The Host must invoke this owning operation; it must not navigate
or reconstruct a document to apply mobile layout. Existing CDP screencasting
produces PNG/JPEG; it is not the raw-frame H.264 transport.

## First runtime slice

The opt-in `obscura-host` binary owns one live Session/Page per process, independently
of its Unix socket connections. It reuses Page, BrowserContext and the existing
autonomous event-loop pump. Shared CDP input dispatch now lives in
`obscura-browser/src/input.rs`; CDP retains its navigation wait/event adapter.
`protocol/browser` owns the local bootstrap commands; this is not the final remote
input/media ABI. The next Host slices extend this same owner.

Current commands: attach (observe/agent), detach, status, request_takeover,
release_control, resume_agent, navigate, evaluate, resize, click, input_text,
scroll and close_session.
One Agent attachment may mutate while control is in the Agent phase; another
Agent attachment is rejected. Observers may request human control but cannot
execute diagnostic JavaScript. The granted human holder can click, insert text
and scroll through the same admission/receipt owner as Agent input.
Evaluate remains an Agent diagnostic surface, not the final atomic input API:
page-created timers/network continue after its completion, as before.
Request IDs correlate replies only; version 4 requires the separate operation
envelope including document_revision.
Typed SessionStatus returns the requesting attachment's connection-scoped ID
and mode as well as the shared Session ID and viewport. Reconnect creates a new
attachment identity; daemon restart creates a new Session identity. Observation
does not imply human control capability.

The control loop and Page run on separate OS threads. `daemon.rs` owns
attachments and admission; `worker.rs` creates, executes and drops Page/V8 on
its dedicated thread. One bounded work channel permits one accepted browser
command at a time. Completion crosses back through a typed one-shot response;
the control loop never awaits V8 inline. `operation_running` reports accepted
work awaiting completion, not a replayable operation identity. Status and new
observer connections remain available while synchronous JavaScript runs.
The viewport exposed in status is the latest completed worker result; resize
revision advances only after successful completion. Worker failure fences
execution both locally and at admission, including work already queued when
an autonomous page callback fails. Graceful shutdown waits for accepted work
before joining the owning thread.

### Local operation and takeover contract (version 4)

Every browser mutation, including diagnostic evaluation and explicit close,
requires `operation`: session_id, attachment_id, sequence, control_epoch,
viewport_revision and document_revision. The first three fields identify the operation. The Session
checks this envelope before execution; control requests must not carry it.
Sequences begin at one and advance only on accepted work. Status exposes the
next sequence and authoritative control epoch. No operation queue is replayed.

The Host retains the latest accepted operation and bounded terminal response
per live attachment (at most 16 receipts, each at most 1 MiB). An identical
retry returns that receipt with the new request correlation ID; changed content
under the same identity conflicts. Earlier sequences return OPERATION_EXPIRED.
Detach retires the connection's attachment identity; attaching again requires
a new connection (RECONNECT_REQUIRED). Reconnect gives a new attachment ID,
so old envelopes return STALE_ATTACHMENT
and cannot re-execute. Old receipts are not recoverable after detach; callers
must report unavailable outcomes rather than inventing a new operation to retry.
This is bounded replay protection, not durable exactly-once execution.

Control phases are Agent, Waiting(attachment), Human(attachment), and Paused.
Takeover acceptance advances the epoch and enters Waiting immediately; completion
of accepted work grants Human under another new epoch. With no work it grants
immediately. An unknown outcome leaves Waiting with a session fault and never
grants control. A human holder explicitly releases to Agent. Holder/requester
disconnect enters Paused; Agent must explicitly resume at an idle boundary, or
an observer can request a new takeover. Status remains available to existing
attachments after faults. Explicit close by the attached Agent is the recovery
exception when faulted; it disposes the session rather than granting execution.

Control commands carry the expected epoch, so delayed takeover/release/resume
requests cannot change newer ownership. Real click probes cover takeover during
mousedown and grant only after release/click dispatch. The current supported
inputs are a primary-button click, committed text (writable input/textarea only,
4096 Unicode characters maximum), and wheel scrolling (finite CSS coordinates,
4096 pixels maximum per axis). Coordinate input requires the render feature.
IME composition, keyboard shortcuts, drag and multi-touch remain unsupported.

Input completion reports `input.state=succeeded` only after dispatch returns.
Invalid text targets return INPUT_REJECTED before editing. Injection failures
or deadlines report OUTCOME_UNKNOWN and fence the session; this slice does not
claim failed_stopped recovery from held pointers. Diagnostic evaluation remains
Agent-only. Shared input scripts propagate injection errors instead of converting
them to successful null; the CDP adapter also receives those errors.

Navigation triggered by input/page scripts runs on the worker after the input
receipt. The transport/control loop stays responsive throughout page loading.
The worker increments document_revision for explicit navigation and autonomous
navigation/history transitions, conservatively invalidating old document input.
It checks the document revision again immediately before queued work executes.
Typed worker updates and completions are projected monotonically by the daemon;
neither logs nor DOM variables reconstruct control ownership or these revisions.

The daemon creates a new private directory (0700) and socket (0600); possession
of the same OS user identity is the local trust boundary. Existing directories
are rejected, never reused/unlinked. This is not Relay account authentication or
endpoint E2E. Connections are capped at 16, requests at 64 KiB, replies at 1 MiB,
and writes at two seconds. Page work has an asynchronous deadline and a V8
watchdog; a synchronous Rust poll exceeding ten seconds terminates this isolated
Host with exit 70. Restart creates a new Session ID, never reconstructs success.

Loss of an accepted evaluation reply does not cancel or replay it. Timed-out or
engine-failed evaluation faults the Session; further mutations and attachments
are rejected. A remaining Agent may explicitly close it. Otherwise restart is
explicit, with a new identity. JS exceptions are errors, not successful nulls.

Resize changes the one existing Page viewport. Host assigns the next viewport
revision and commits the matching worker completion; the worker only projects
that assignment into frames. Equal dimensions perform no layout or revision
change. Input rejects old viewport revisions instead of reinterpreting coordinates.
Control-state push subscriptions are not implemented; clients query status.

### Shared mobile viewport declaration (version 4)

The sole ABI is `protocol/browser/src/lib.rs`. Observe attachments may include
`viewport` in `attach`, then update it with `declare_viewport`. Both are control
requests without an `operation` envelope. The declaration contains `device`
(`phone` or `desktop`), `css_width`, `css_height` and explicit `orientation`
(`portrait` or `landscape`). An Agent attachment cannot declare a viewer area.
Clients measure the actual available page container, excluding occupied system
bars, app chrome and keyboard space. Screen dimensions and decoded frame sizes
are not substitutes. Device orientation is independent of the available area's
aspect ratio, which keyboard occupation can invert.

Host rejects dimensions outside 1..4096 or an area above 4,194,304 CSS pixels,
matching the raw capture budget. It never clamps or scales a declaration.
Phone declarations outrank Desktop declarations. Within one device class,
Host selects the minimum `(width * height, width, attachment_id)` tuple and
uses that attachment's complete width/height pair. It never minimizes the axes
independently. Updates, detach and EOF cause reelection. Equal-size ownership
changes do not resize. Without declarations, the last committed viewport remains;
Agent `resize` is allowed only in this unmanaged state, otherwise it returns
`VIEWPORT_MANAGED`. Layout selection does not grant input authority or change
the control epoch.

Declarations return promptly, even while an atomic operation runs.
`viewport_pending` distinguishes accepted intent or an executing resize from a
committed result. `viewport` and `viewport_revision` describe applied layout;
`viewport_owner` identifies its elected attachment, or null when unmanaged.
New mutations receive `VIEWPORT_PENDING` while layout is pending. The current
accepted operation completes before the latest elected layout executes on the
same Page. Pending declarations coalesce; they do not form an unbounded queue.
Internal layout work targets the current document, while explicit Agent resize
retains its operation's document fence. Unknown outcomes fault the session and
do not apply waiting layout or grant pending takeover. Session close remains an
explicit recovery operation. Layout callbacks run under the V8 deadline and
process watchdog; a partial layout failure retains the last committed geometry
and revision in Host status while fencing the faulted Page.

All observers consume one shared buffer. Native clients must stop input while
layout is pending and until the committed revision has actually been presented.
They must discard old decoded/displayed revisions, never relabel an old frame
with a newer status revision. Source width/height define the display ratio;
H.264 even-dimension padding is separate. Other-sized viewers display the entire
source proportionally, without stretching or cropping webpage content.

### Shared local raw frames

`Page::render_frame` exports premultiplied RGBA8 from the same prepared paint
implementation used by PNG screenshots. Runtime frame delivery does not encode
or decode PNG. A future H.264 encoder can consume these pixels directly.

The independent `frames.sock` uses the same private directory and 0600 local
trust boundary as `host.sock`. It accepts at most 16 read-only media readers.
Each packet is a JSON line shorter than 4096 bytes; a `frame` packet is followed
by exactly `info.byte_length` raw bytes. Typed headers carry session, sequence,
document/viewport revisions, dimensions, stride and pixel format. `waiting`,
`unavailable` and `closed` packets have no pixels. Media access grants no control
ownership and does not elect a viewport.

With readers present, the Page worker samples at approximately three frames per
second, capped at 4,194,304 pixels (16 MiB). All readers share one immutable latest
frame through an Arc; there is no frame history queue. Slow writes time out after
two seconds. A reader may skip old frames and reconnect to the current session.
Viewport/document transitions invalidate the cached frame. Capture failures are
explicitly unavailable. This socket is a local raw diagnostic seam, not H.264,
WebRTC, Relay authentication or evidence of remote phone playback.

### Local H.264 encoder adapter

`crates/obscura-media` consumes `frames.sock` and writes `VideoPacket` headers
plus Annex B bytes to a separately supplied consumer-owned private Unix socket.
It owns no browser state or network listener. The eventual authenticated endpoint
must own one adapter and fan out its encoded output; starting one adapter per
viewer is not the product architecture. The current standalone adapter proves
the encoder boundary before that endpoint exists.

`--ffmpeg` selects an external FFmpeg binary with libx264; default is `ffmpeg` on
PATH. This initial backend uses H.264 baseline, YUV420, BT.709 limited-range and
one independently decodable SPS/PPS/IDR access unit per source frame. Each frame
starts a bounded encoder process; throughput/bitrate optimization and native
VideoToolbox lifetime management remain future work inside this same owner.
No HEVC negotiation or automatic codec/backend fallback is implemented. External
FFmpeg is not bundled; binary/license packaging remains a release requirement.

Input is capped at four megapixels, output at 4 MiB/access unit, diagnostics at
64 KiB. Encoder work has a five-second deadline; incomplete raw pixels and output
backpressure have two-second deadlines. On encoder failure the adapter sends
Unavailable and exits nonzero. Invalid framing, inconsistent sizes or regressive
frame identity fail explicitly; EOF without Closed is not success. Child encoder
processes use kill-on-drop on error/cancellation. Odd viewport sizes pad to even
coded dimensions; the display must crop to source.width/source.height. Alpha is
composited over white before YUV conversion.

VideoPacket preserves FrameInfo from the Host and adds encoder incarnation,
monotonic receipt-time PTS in microseconds, coded size, codec, keyframe and encoded
byte length. PTS is sampled on receipt of the raw header, not a claim of native
capture time. Source.byte_length still describes RGBA; the outer byte_length
describes the following H.264 bytes. A new adapter has a new encoder_id, resets
its PTS origin and never restores old input/control authority.

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-host -p obscura-media --features render
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release -p obscura-media
python3 crates/obscura-media/tests/host-replay.py target/release/obscura-host target/release/obscura-media /tmp/obscura-h264-new-proof
```

The replay creates an isolated Host and consumer socket, tests real page input,
encodes/decodes resulting pixels, checks resize/form retention and closes its own
processes. It requires a fresh evidence directory and installed FFmpeg. A manual
endpoint consumer supplies `obscura-media --frames-socket HOST_DIR/frames.sock
--video-socket CONSUMER_SOCKET`; this does not expose a remote endpoint.

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-host --features render
target/release/obscura-host --socket-dir /tmp/obscura-local-session
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --no-fail-fast
```

The socket path must fit the OS Unix socket limit. `--allow-private-network` is
an explicit local test opt-in. No persistent disk profile, file navigation,
network listening or running browser session is adopted by default.

One session owns one dedicated V8 thread and page event loop. The daemon registry
owns the session sender independently of attachments. Attachment disconnect only
removes that attachment; explicit session close or daemon shutdown disposes the
page. Continue pumping timers and requests with zero attachments. Do not move a
Page/isolate across threads and do not use socket activity as its clock.

First acceptance uses a local authenticated connection and one tab per session.
Relay integration, UDP/WebRTC, video encoding, restart restoration, Linux host
support and multi-tab UX are subsequent slices. Profile disk persistence and
live document persistence remain separate claims. Reconnection within the same
daemon incarnation must recover the same page; daemon restart must report a new
incarnation and must not claim continuation of an old in-flight operation.

## Explicitly paired direct endpoint

`obscura-endpoint` is an opt-in second binary in the Host crate, owned by
`crates/obscura-host/src/endpoint`. It connects to an existing local Host and
never creates a second Page. The operator supplies an explicit listen address,
DER server leaf certificate, PKCS#8 DER key with no group/world permissions, and
a DER client CA for prepaired native clients. TLS verifies client certificate
identity; clients must verify the Host certificate/hostname. This is the separate
prepaired local authorization policy, not Relay account login or automatic route
selection. Tailscale reachability alone grants no application capability.

The direct WSS `/control` route forwards protocol-v4 JSON Request/Response text.
The endpoint restricts remote commands to observe attachment, status, detach,
declare_viewport, request_takeover, release_control and human click/text/scroll. Agent attachment,
diagnostic evaluation, navigation, resize, close and resume are rejected here.
Host still owns and checks every input identity, epoch and revision. Disconnection
does not cause operation replay. Native clients only: HTTP Origin and query-string
requests are rejected, preventing ambient client-certificate use by websites.

The control upgrade returns an unpredictable `x-obscura-media-token` header.
The `/media` upgrade requires `Authorization: Bearer TOKEN`, the same paired
client leaf certificate, and a successfully attached, still-live control
connection. Each token can open one media connection. Detach/control EOF revokes
its grant and ends media; reopening media requires a new control attachment.
Connection lifetime is at most one hour; local pairing trust is loaded on startup.
Immediate local revocation requires endpoint restart with updated trust material.
Do not claim Relay token expiry/revocation or end-to-end Relay encryption here.

The endpoint launches one obscura-media process and owns a fresh 0700 directory
with `encoded.sock` at 0600. One latest encoded packet is shared across viewers;
slow viewers skip old packets, with at most one bounded in-progress send per
connection. Media WSS binary messages contain a four-byte big-endian JSON-header
length, that many UTF-8 VideoPacket bytes (no newline), then exactly the outer
byte_length encoded bytes for AccessUnit. State packets contain no encoded bytes.
Maximum header is 4095 bytes, access unit 4 MiB. Control messages are at most
64 KiB; total endpoint connections are capped at 32, TLS/upgrade at five seconds,
writes at two seconds. Media ingestion failure closes endpoint connections;
there is no automatic reconnect/replay or hidden alternate transport. Endpoint
exit stops its encoder and leaves the independent Host/Page alive.

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-host -p obscura-media --features render
target/release/obscura-endpoint --listen HOST_IP:PORT --host-dir HOST_DIR --socket-dir NEW_ENDPOINT_DIR --server-cert SERVER.der --server-key KEY.der --client-ca CLIENT_CA.der --media-bin target/release/obscura-media
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo nextest run --release --features render -p obscura-host --test endpoint
```

Build the media binary before endpoint acceptance; its public test starts the real
Host and encoder. Optional device verification sets `OBSCURA_ENDPOINT_BIND_IP`
to the current reachable Host IP and `OBSCURA_ENDPOINT_ADB_SERIAL` to the current
explicit ADB serial before that same test command. The test uses a fresh private
CA, copies three temporary PEM credentials to its own 15T directory, verifies
WSS upgrade/Host ready with system curl, then removes those device credentials.
It does not install a system trust root or claim native Android media acceptance.

The remaining connection work is client-side native ingress and UI integration,
native viewport measurement/presentation, UDP/WebRTC and Relay adapters. The direct endpoint
does not implement candidate selection or label a Tailscale path as peer-to-peer.

## Session and input invariants

- Session state is owned by the Host, never reconstructed from Relay directory
  snapshots, UI state or logs. Attachments carry identity, mode and layout intent.
- Observe permits frames/state only. Takeover marks admission closed to new Agent
  mutations immediately, waits for the current atomic operation's terminal result,
  then grants a new control generation. It does not wait for an entire agent task.
- Operation identities and the controlling generation are checked by the owning
  session before execution. A repeated completed operation returns its prior result;
  it does not execute again. Duplicate IDs with different content are rejected.
- Unknown execution outcome blocks handover and replay. Disconnection does not
  convert unknown into success or permission to run another operation.
- Click press/release, text input, key press/release and complete drag are atomic
  operations. IME composition remains open until commit/cancel. Reads do not claim
  mutation ownership. Pending viewport changes wait for input operation boundaries.
- All viewers share the same viewport/render buffer. A phone attachment selects
  phone layout; observe does not change control ownership. Orientation is explicit.
  Multiple phone candidates require deterministic selection; never independently
  minimize width and height into a viewport no device requested.
- Layout changes call the existing page viewport operation, preserve document and
  JS/form state, and increment a viewport revision. Input carrying an old viewport
  revision is rejected; no best-effort coordinate reinterpretation.
- Explicit release/Agent resume and holder disconnect recovery must be typed Host
  transitions. Neither the Relay nor the client may grant control locally.

## Required evidence before completion

1. Create a session with a local fixture containing a mutable form, JS counter,
   timer and a delayed request. Attach, detach every viewer, reconnect; document
   identity, form and JS values persist and the timer/request continued.
2. Attach a phone, rotate, attach a desktop observer: all viewers receive one
   viewport revision, with preserved state. Reject old-revision pointer input.
3. Request takeover during a held click/drag/IME operation: new Agent operations
   and premature human input are rejected. Finish the current operation, grant
   one new generation, reject all stale-generation inputs.
4. Replay an operation ID, send a conflicting duplicate, disconnect an executor,
   and report unknown outcome: prove no duplicate side effect or accidental grant.
5. Verify unauthorized attachment, explicit session close, connection churn and
   daemon shutdown. Keep session/attachment/queue limits bounded.

For runtime changes run focused release nextest, the repository-wide release
nextest/render gate, the required release CLI build and the companion obstacle
course (33/33), as required by AGENTS.md. Add actual Host-entrypoint replay and
AGY review. Plan text, isolated state-machine tests and socket health do not prove
that the browser remains alive after detach.

## Viewport v4 candidate evidence

The viewport candidate imports the authorized persistent-host snapshot at
`d80af25` from source HEAD `72c84adcc6ec3ea4a7144adb4e45d4d3038ebcda`.
The initial attach-with-viewport regression failed with `INVALID_REQUEST`.
The callback-deadline regression independently failed because layout stayed
pending, then passed with the worker watchdog and committed-state fence.
Final scoped release nextest for `obscura-host` and `obscura-media` passed
25 tests, with four nextest leaky classifications and no skipped tests:
`/tmp/obscura-mobile-viewport-focused-final.log`. This includes actual Host,
shared raw frames, and mTLS/WSS declaration/media/input paths; it does not
prove Android native presentation.

The task owner explicitly requested the scoped candidate without waiting for
the new worktree's long first repository-wide build. That full nextest command
was interrupted during compilation and has no test verdict
(`/tmp/obscura-mobile-viewport-full-nextest.log`). The final CLI build, obstacle
rerun and AGY verdict remain pending at candidate handoff. The historical 32/33
obstacle result below does not certify this candidate or satisfy the 33/33 gate.
This is a reviewable source candidate, not a completed integrated release.

## Prior endpoint candidate evidence

The paired endpoint slice passed a real Host/encoder/mTLS/WSS test: missing
client certificate denied, remote Agent attachment denied, observer attached,
unknown media token and cross-certificate token denied, H.264 packet matched
Host identity, human takeover/click changed the actual page, explicit release
restored Agent, and control EOF revoked media. Evidence:
`/tmp/obscura-endpoint-focused.log`. The optional real 15T probe bound the current
Mac Tailscale address and received WSS 101 plus the actual Host session using
Android system curl (`/tmp/obscura-endpoint-15t-final.log`). Its temporary device
credentials were removed. This proves paired direct transport readiness, not
native Android decoding/interaction. The initial device fixture used identical
default CA/leaf names and Android rejected it as self-signed; the fixture now
uses a distinct CA name, with verification still enabled. Initial evidence:
`/tmp/obscura-endpoint-15t.log`.

Final full release nextest passed 1652 tests, four skipped, no leaky classification
(`/tmp/obscura-endpoint-full-final.log`). The first run failed an unchanged
resource-prefetch request-line assertion and marked two other tests leaky
(`/tmp/obscura-endpoint-full.log`); the prefetch test passed independently
(`/tmp/obscura-endpoint-prefetch.log`). No root cause is claimed for that
intermittent assertion. Exact CLI and Host/media/endpoint release builds passed
(`/tmp/obscura-endpoint-cli-build.log`, `/tmp/obscura-endpoint-binaries.log`), with
checksums in `/tmp/obscura-endpoint-artifacts.sha256`. The obstacle rerun remains
32/33 with observer-intersection (`/tmp/obscura-endpoint-obstacle.json`). Paint
sources are unchanged from the shared-frame slice; its render evidence and
limitations remain applicable. Android Annex B decoder work runs independently
in the Android owner task; it is not yet an integrated remote browser release.
Initial AGY controller task `obscura-host-paired-endpoint` returned PASS but its
module list omitted new untracked directories, so that was not used as endpoint
coverage. Follow-up `obscura-paired-endpoint-untracked-coverage` returned PASS
with Host/endpoint and media included. Its summary's test counts are not the
test authority; the exact full-suite evidence above is authoritative.

The H.264 slice passed four focused tests (`/tmp/obscura-h264-focused.log`) and
actual Host -> RGBA -> H.264 -> decoded red/green pixel transition -> odd-size
resize -> retained form -> explicit close (`/tmp/obscura-h264-host-replay.log`,
artifacts `/tmp/obscura-h264-host-proof`). Full release nextest passed 1651 tests,
four skipped; one existing Host takeover test received a leaky classification
(`/tmp/obscura-h264-full.log`). Host/media and exact CLI release builds passed
(`/tmp/obscura-h264-binaries.log`, `/tmp/obscura-h264-cli-build.log`). This slice
reran the leaky-classified takeover test independently: PASS without that
classification (`/tmp/obscura-h264-takeover-check.log`). The encoded artifact was
probed as H.264 Constrained Baseline, YUV420, BT.709 limited-range, 392x846 for the
391x845 source viewport.
This slice
does not change the paint implementation; prior rendering evidence and its
limitations remain applicable. Remote transport and Android playback remain
unimplemented by this adapter. No milestone, integration or memory reconstruction
is claimed.
The new obstacle run remains 32/33, failing only observer-intersection
(`/tmp/obscura-h264-obstacle.json`). Binary/encoded-artifact checksums are in
`/tmp/obscura-h264-artifacts.sha256`.
AGY controller `obscura-host-h264-adapter` returned PASS with no findings. The
Android coordination task received the verified local codec boundary; its local
MP4 probe remains unchanged. Current 15T Tailscale/ADB availability was checked,
but device connectivity is not remote browser acceptance.

The shared-frame slice adds two-viewer byte identity, live click-to-pixel change,
viewport revision/dimension changes and media reconnect coverage (14 Host tests,
`/tmp/obscura-frame-focused.log`). A Page fixture verifies raw/PNG pixel identity
for DOM, canvas and scroll while resize retains form state. The full release run
passed 1647 tests, four skipped (`/tmp/obscura-frame-full.log`); the exact CLI
release build passed (`/tmp/obscura-frame-build.log`). Socket pixels and visual
evidence are under `/tmp/obscura-frame-proof.LTEUFJ`.

Rendering verification generated 64 paired deterministic fixtures. The harness
failed 12 Chromium-side geometry assertions; no Obscura-side behavior assertion
failed (`fixtures/analysis.json`). The installed Chrome 152.0.7977.77 was used
because Playwright's pinned browser was absent. Do not claim this as a passing
paired fixture gate. The representative corpus captured top and bottom for 15
sites each; each set has nine fidelity-eligible pairs, with the rest excluded
for unavailable or unstable capture state. Apple top/bottom visual inspection
shows shared major structure but text metrics, wrapping and footer spacing
differences. These current cross-engine comparisons do not establish regression
against a prior Obscura binary. The obstacle rerun remains 32/33 with unchanged
observer-intersection failure (`/tmp/obscura-frame-obstacle.json`). Required gates
and whole-product acceptance remain incomplete; no milestone or memory rebuild.
AGY controller review `obscura-host-shared-frames` returned PASS with no findings.
Both representative capture processes retained all 15 page records but did not
exit after the final page. Their owned Python/Playwright driver PIDs were stopped
after confirming no browser children remained; these runs have no successful
process-exit evidence. The top process sample is
`/tmp/obscura-frame-top-process.txt`. Capture artifacts remain available.

The input slice moves existing CDP input semantics into the Page owner and adds
Host click/text/scroll, dispatch receipts, autonomous navigation and document
revision fencing. Host tests prove real DOM effects, Unicode/escaped text and
non-duplicating replay, takeover during mousedown, stuck pointer dispatch fencing,
and input receipts before delayed navigation completion. Current focused evidence:
`/tmp/obscura-input-focused.log` (13/13); the earlier affected-module run covered
291 Browser/CDP/Host tests with three skipped (`/tmp/obscura-input-affected.log`).
The final whole-workspace release run passed 1645 tests, four skipped
(`/tmp/obscura-input-full.log`); the required CLI release build also passed
(`/tmp/obscura-input-build.log`).
The new obstacle run remains 32/33, with observer-intersection expecting io:50
and receiving an empty result (`/tmp/obscura-input-obstacle.json`). The required
33/33 milestone gate is still unmet.
Whole-product Mac/15T acceptance, remote media and milestone publication remain
pending. The original test page's overlapping spacer was the actual hit target,
so the input fixture now uses non-overlapping boxes; this does not establish
stacked-layout hit-test parity (`/tmp/obscura-input-text-debug.log`).

The control slice adds public socket tests for takeover accepted during busy
JavaScript, pipelined old-generation input rejection, explicit release, holder
disconnect/pause/resume, repeated/conflicting/expired operation receipts, stale
viewport and reconnect rejection, partial-effect exception replay protection,
and unknown-outcome handover fencing. Evidence for the current candidate is in
`/tmp/obscura-control-focused.log` and `/tmp/obscura-control-full.log`.
Final focused coverage passed 10/10; full release coverage passed 1642 tests
(one leaky classification, four skipped). The CLI release build passed
(`/tmp/obscura-control-build.log`). During verification, a Host test's
file-existence startup probe raced bind/listen; it now probes socket connection
readiness. A separate full run failed unchanged CLI `mcp_client::test_evaluate`
with an empty title; that evidence is retained in
`/tmp/obscura-control-full-attempt2-fail.log`, and the final full rerun passed.
The obstacle rerun is still 32/33 with the same observer-intersection discrepancy
(`/tmp/obscura-control-obstacle.json`); the repository's 33/33 gate is unmet.

The execution-thread slice adds a real socket regression: synchronous JS runs
for 1.5 seconds while an observer queries status within 500 ms. The test failed
before thread separation and passed afterward. All six Host tests passed in
the focused release run; the updated full suite passed 1638 tests, four skipped.
The required CLI release build passed. Current logs:
`/tmp/obscura-host-thread-focused.log`, `/tmp/obscura-host-thread-full.log`,
`/tmp/obscura-host-thread-build.log`. This slice establishes responsive control
admission; takeover, operation identity/replay protection and human input are
still pending and must not be inferred from the thread split.
The companion obstacle course was rerun after this slice: 32/33, with the same
`observer-intersection` discrepancy described below. Evidence:
`/tmp/obscura-host-thread-obstacle.json`. The required 33/33 gate remains unmet.

Before the thread split, all five release-mode public-entrypoint tests passed: detach/reconnect
preserves document, form and timers; resize preserves form; observer mutation,
duplicate Agent, invalid viewport and explicit close are checked. Further
fixtures prove delayed fetch after EOF, sleeping timer responsiveness, explicit
JS exceptions, oversized response errors, timeout fencing, private socket
permissions, existing-path refusal and rejection of invalid/oversized frames.
The full release nextest run passed 1636 tests (one leaky classification in
unchanged `obscura::select_semantics`, four skipped). Subsequent Host/protocol
changes received the final five-test focused rerun; other engine sources are
unchanged. Logs: `/tmp/obscura-host-full-nextest-20260905.log` and
`/tmp/obscura-host-verified-focused-20260905.log`. Runtime sources have not been
committed, merged or pushed in this slice.

The unchanged CLI release build passed. Companion benchmark HEAD
`6ebac8293d7477f59e837768bfd4e74173f04f1c` returned 32/33; isolated rerun confirmed
`observer-intersection` expects `io:50` but gets an empty result. Its fixture
assumes repeated callbacks while a single observed sentinel remains intersecting.
No engine or benchmark code has been changed to satisfy this historical check.
This remains an unmet repository gate, not a waived PASS.

After runtime admission, extend the control owner with real human input and
terminal input receipts, then mobile viewport election and frame subscriptions.
Do not expose diagnostic evaluation as human input or claim the remote browser
chain is complete from this local bootstrap.
