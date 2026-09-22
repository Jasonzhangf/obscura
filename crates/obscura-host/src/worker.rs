//! Sole Page owner. No attachment or transport state enters the browser thread.
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use anyhow::{Context, Result};
use obscura_browser::{BrowserContext, Page};
use obscura_host_protocol::{Command, Device, EvaluationResult, FrameInfo, FramePacket, InputEvent, InputReceipt, InputState, Operation, PixelFormat, PointerButton, ResultValue, RotationIdentity, SourceDimensions};
use crate::media::{self, SharedFrame};
use obscura_browser::input::{Input, InputError};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};

pub struct Completion {
    pub result: Result<Option<ResultValue>, (&'static str, String)>,
    pub viewport: Option<(f32, f32)>,
    pub focus: Option<FocusSnapshot>,
    pub fault: Option<String>,
    pub document_revision: u64,
    pub viewport_revision: u64,
    pub rotation: Option<RotationIdentity>,
}
pub struct PageUpdate {
    pub document_revision: u64,
    pub focus: Option<FocusSnapshot>,
    pub fault: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusSnapshot {
    pub focused: bool,
    pub editable: bool,
}
pub struct Work { pub kind: WorkKind, pub reply: oneshot::Sender<Completion> }
pub enum WorkKind {
    Browser { command: Command, operation: Operation },
    /// Host-assigned shared viewport; declarations apply to the current Page,
    /// while explicit Agent resize retains its admitted document fence.
    Viewport { width: u32, height: u32, revision: u64, device: Device, rotation: Option<RotationIdentity>, document_revision: Option<u64>, cancel: Arc<AtomicBool> },
}
const BUDGET: Duration = Duration::from_secs(5);

/// Minimum interval between captured frames.
///
/// The 3 fps ceiling this replaces was a latency floor, not a protection: the
/// capture path is a synchronous paint that measured in single-digit
/// milliseconds, so the tick rate was the dominant delay for every viewer.
/// `OBSCURA_FRAME_PERIOD_MS` tunes it for benchmarking; it is clamped to a
/// range that keeps a misconfigured value from either spinning the paint loop
/// or reintroducing the old ceiling.
fn frame_period() -> Duration {
    let configured = std::env::var("OBSCURA_FRAME_PERIOD_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(33);
    Duration::from_millis(configured.clamp(8, 1000))
}

// Bound actual synchronous polls, never parked timers/network futures.
async fn page_work<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let (cancel, stopped) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if stopped.recv_timeout(Duration::from_secs(10)).is_err_and(|e| matches!(e, std::sync::mpsc::RecvTimeoutError::Timeout)) {
                eprintln!("Host synchronous browser work exceeded hard deadline; session outcome unknown");
                std::process::exit(70);
            }
        });
        let result = future.as_mut().poll(cx);
        drop(cancel);
        watchdog.join().expect("process watchdog failed");
        result
    }).await
}

pub fn start(id: String, storage_dir: Option<std::path::PathBuf>, network: bool, mut incoming: mpsc::Receiver<Work>,
    ready: oneshot::Sender<Result<(f32, f32), String>>,
    updates: mpsc::Sender<PageUpdate>,
    frames: tokio::sync::watch::Sender<SharedFrame>,
    mut interested: tokio::sync::watch::Receiver<usize>,
) -> Result<std::thread::JoinHandle<Result<()>>> {
    // Same first-isolate initialization requirement as the existing CDP server.
    drop(obscura_js::runtime::ObscuraJsRuntime::new());
    Ok(std::thread::Builder::new().name("obscura-page".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async move {
            let context = Arc::new(BrowserContext::with_storage_and_network(id.clone(), None, false, None, storage_dir, network));
            let mut page = Page::new(id.clone(), context);
            if let Err(cause) = page_work(page.navigate("about:blank")).await {
                let _ = ready.send(Err(cause.to_string()));
                return Err(cause).context("initialize session");
            }
            if ready.send(Ok(page.viewport)).is_err() { return Ok(()); }
            let mut page = Some(page);
            let mut failure: Option<String> = None;
            let mut document_revision = 0;
            let mut viewport_revision = 0;
            // Identity is owned by the elected attachment's device. The Host
            // starts on its build-time default and adopts the first declared
            // device, so a phone that takes the session browses as a phone.
            let mut identity_device: Option<Device> = None;
            let mut rotation: Option<RotationIdentity> = None;
            let mut frame_sequence = 0u64;
            // Capture cadence. This was a hardcoded 333 ms (3 fps) ceiling,
            // which set the floor on glass-to-glass latency no matter how fast
            // the page or encoder was; measured capture itself costs single
            // digit milliseconds. Default to a 30 fps budget and allow an
            // operator override for tuning without a rebuild.
            let mut frame_clock = tokio::time::interval(frame_period());
            frame_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut idle = false;
            let mut frame_fence: Option<Arc<AtomicBool>> = None;
            let mut clock = tokio::time::interval(Duration::from_millis(20));
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                // Navigation caused by dispatched input/page scripts is page work,
                // after the input receipt. Never cancel it to inject another input.
                if failure.is_none() {
                    if let Some(page) = page.as_mut() {
                        // A script or input can request navigation while an
                        // older stylesheet/script continuation is still alive.
                        // Consume that request at the top of the next worker
                        // turn so navigate_single cancels the old Page-owned
                        // continuation before it can materialize into the new
                        // document.
                        match timeout(BUDGET, page_work(page.process_pending_navigation())).await {
                            Ok(Ok(false)) => {},
                            outcome => {
                                document_revision += 1;
                                frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                let navigated = match outcome {
                                    Ok(Ok(true)) => true,
                                    Ok(Err(cause)) => {
                                        failure = Some(format!("Page navigation failed: {cause}"));
                                        false
                                    }
                                    Err(_) => {
                                        failure = Some("Page navigation outcome unknown after deadline".into());
                                        false
                                    }
                                    _ => unreachable!(),
                                };
                                let focus = if navigated && failure.is_none() {
                                    page_work(async { focus_snapshot(page) }).await
                                } else { None };
                                if updates.send(PageUpdate { document_revision, focus, fault: failure.clone() }).await.is_err() { break; }
                            }
                        }
                    }
                }
                let navigation_exclusive = page
                    .as_ref()
                    .is_some_and(Page::navigation_continuation_exclusive);
                tokio::select! {
                    changed = interested.changed() => { if changed.is_err() { break; } }
                    _ = frame_clock.tick(), if *interested.borrow() > 0 && !navigation_exclusive => {
                        if frame_fence.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire)) { continue; }
                        let frame = if let Some(cause) = &failure {
                            media::state(FramePacket::Unavailable { session_id: id.clone(), message: cause.clone() })
                        } else if let Some(page) = page.as_mut() {
                            match capture(page).await {
                                Ok((width, height, pixels)) => {
                                    frame_sequence = frame_sequence.checked_add(1).context("frame sequence exhausted")?;
                                    Arc::new(media::Frame {
                                        packet: FramePacket::Frame { info: FrameInfo { session_id: id.clone(), sequence: frame_sequence,
                                            document_revision, viewport_revision, width, height, stride: width * 4,
                                            rotation, source_dimensions: SourceDimensions::from_raw(width, height),
                                            byte_length: pixels.len() as u64, pixel_format: PixelFormat::PremultipliedRgba8 } },
                                        pixels,
                                    })
                                }
                                Err(message) => media::state(FramePacket::Unavailable { session_id: id.clone(), message }),
                            }
                        } else { media::state(FramePacket::Closed { session_id: id.clone() }) };
                        if frame_fence.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire)) { continue; }
                        frames.send_replace(frame);
                    }
                    work = incoming.recv(), if !navigation_exclusive => {
                        let Some(work) = work else { break; };
                        let expected_document = match &work.kind {
                            WorkKind::Browser { operation, .. } => Some(operation.document_revision),
                            WorkKind::Viewport { document_revision, .. } => *document_revision,
                        };
                        let mut completion = if failure.is_some() && !matches!(work.kind, WorkKind::Browser { command: Command::CloseSession {}, .. }) {
                            Completion {
                                result: Err(("SESSION_FAULTED", failure.clone().unwrap())),
                                viewport: page.as_ref().map(|page| page.viewport),
                                focus: None,
                                fault: failure.clone(),
                                document_revision,
                                viewport_revision,
                                rotation,
                            }
                        } else if expected_document.is_some_and(|expected| expected != document_revision) {
                            Completion { result: Err(("STALE_DOCUMENT", "Document changed before execution".into())), viewport: page.as_ref().map(|page| page.viewport), focus: None, fault: None, document_revision, viewport_revision, rotation }
                        } else {
                            match work.kind {
                                WorkKind::Viewport { width, height, revision, device, rotation: target_rotation, cancel, .. } => {
                                    frame_fence = Some(Arc::clone(&cancel));
                                    let mut fault = None;
                                    let result = if cancel.load(Ordering::Acquire) {
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                        Ok(None)
                                    } else if let Some(page) = page.as_mut() {
                                        if identity_device != Some(device) {
                                            // Identity follows whoever holds the session. The live
                                            // document is never reloaded for this: attaching a device
                                            // must not destroy page state, and the next navigation is
                                            // the first document that must see the new identity.
                                            let identity = match device {
                                                Device::Phone => obscura_browser::BrowserIdentity::phone(),
                                                Device::Desktop => obscura_browser::BrowserIdentity::desktop(),
                                            };
                                            match page_work(page.adopt_identity(&identity)).await {
                                                Ok(()) => identity_device = Some(device),
                                                Err(cause) => fault = Some(format!("Identity adoption failed: {cause}")),
                                            }
                                        }
                                        if fault.is_none() && page.viewport != (width as f32, height as f32) {
                                            // Invalidate before changing layout. The same Page retains DOM,
                                            // JS, form state and pending tasks; no attachment-specific render.
                                            frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                            let watchdog = page.js.as_mut().map(|js| js.arm_watchdog(BUDGET));
                                            page_work(async { page.set_viewport((width as f32, height as f32)); }).await;
                                            if watchdog.is_some_and(|watchdog| page.js.as_mut().expect("viewport retains runtime").disarm_watchdog(watchdog)) {
                                                fault = Some("Viewport callback deadline: partial layout outcome; recreate session explicitly".to_string());
                                            }
                                        }
                                        if let Some(cause) = &fault { Err(("OUTCOME_UNKNOWN", cause.clone())) }
                                        else { viewport_revision = revision; rotation = target_rotation; Ok(None) }
                                    } else { Err(("SESSION_CLOSED", "Session was explicitly closed".into())) };
                                    if cancel.load(Ordering::Acquire) {
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                    }
                                    Completion { result, viewport: page.as_ref().map(|page| page.viewport), focus: None, fault, document_revision, viewport_revision, rotation }
                                }
                                WorkKind::Browser { command, operation } => {
                                    let navigation = matches!(&command, Command::Navigate { .. });
                                    let focus_after = matches!(&command, Command::Click { .. } | Command::InputText { .. } | Command::InputEvent { .. } | Command::Navigate { .. });
                                    let mut completion = execute(&mut page, command, &operation).await;
                                    if navigation {
                                        document_revision += 1;
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                    }
                                    if focus_after && completion.result.is_ok() && completion.fault.is_none() {
                                        if let Some(page) = page.as_mut() {
                                            completion.focus = page_work(async { focus_snapshot(page) }).await;
                                        }
                                    }
                                    stamp_input_receipt(&mut completion.result, &operation, document_revision, viewport_revision);
                                    completion
                                }
                            }
                        };
                        completion.document_revision = document_revision;
                        completion.viewport_revision = viewport_revision;
                        completion.rotation = rotation;
                        if completion.fault.is_some() { failure = completion.fault.clone(); }
                        if page.is_none() { frames.send_replace(media::state(FramePacket::Closed { session_id: id.clone() })); }
                        else if let Some(message) = &failure { frames.send_replace(media::state(FramePacket::Unavailable { session_id: id.clone(), message: message.clone() })); }
                        let _ = work.reply.send(completion);
                        // A cancelled viewport keeps its fence until the daemon queues
                        // and acknowledges a replacement layout. Completed work with
                        // a live fence no longer needs to retain that allocation.
                        if frame_fence.as_ref().is_some_and(|fence| !fence.load(Ordering::Acquire)) {
                            frame_fence = None;
                        }
                        idle = false;
                    }
                    page_turn = async {
                        if failure.is_some() || page.is_none() {
                            return std::future::pending().await;
                        }
                        if page.as_ref().is_some_and(Page::has_navigation_continuation) {
                            page_work(page.as_mut().unwrap().run_navigation_continuation_turn())
                                .await
                                .map(|done| (true, done))
                        } else {
                            if idle { clock.tick().await; }
                            page_work(page.as_mut().unwrap().run_autonomous_event_loop_turn())
                                .await
                                .map(|next_idle| (false, next_idle))
                        }
                    } => match page_turn {
                        Ok((true, _)) => { idle = false; }
                        Ok((false, next_idle)) => { idle = next_idle; }
                        Err(cause) => {
                            failure = Some(cause.clone());
                            if updates.send(PageUpdate { document_revision, focus: None, fault: Some(cause) }).await.is_err() { break; }
                        }
                    },
                }
            }
            // The Page owns the only BrowserContext instance. Persist its
            // cookie jar and the current document's localStorage while still
            // on the Page thread, before V8 and the context are dropped.
            if let Some(page) = page.as_mut() {
                page.context.save_cookies();
                page.persist_local_storage().map_err(anyhow::Error::msg)?;
            }
            Ok(())
        })
    })?)
}

async fn capture(page: &mut Page) -> Result<(u32, u32, Vec<u8>), String> {
    #[cfg(feature = "render")]
    {
        // Local raw diagnostic stream, bounded independently of the control
        // reply budget. Video encoding will consume these pixels directly.
        if page.viewport.0 * page.viewport.1 > 4_194_304.0 { return Err("Raw frame exceeds 4 megapixel budget".into()); }
        page_work(async {
            page.prepare_screenshot_resources(50).await;
            page.render_frame()
        }).await
    }
    #[cfg(not(feature = "render"))]
    { let _ = page; Err("Frames require a render-enabled Host".into()) }
}

async fn execute(page: &mut Option<Page>, command: Command, operation: &Operation) -> Completion {
    let mut fault = None;
    let result = if matches!(command, Command::CloseSession {}) {
        if let Some(page) = page.as_mut() {
            page.context.save_cookies();
            match page.persist_local_storage() {
                Ok(()) => {}
                Err(error) => {
                    return Completion {
                        result: Err(("PROFILE_STORAGE_FAILED", error)),
                        viewport: Some(page.viewport),
                        focus: None,
                        fault: None,
                        document_revision: 0,
                        viewport_revision: 0,
                        rotation: None,
                    };
                }
            }
        }
        *page = None;
        Ok(Some(ResultValue::Closed { closed: true }))
    } else if let Some(page) = page.as_mut() {
        match command {
            Command::Navigate { url } => match timeout(
                BUDGET,
                page_work(page.navigate_for_host(&url)),
            )
            .await
            {
                Ok(Ok(())) => Ok(Some(ResultValue::Input { input: input_receipt(operation) })),
                Ok(Err(cause)) => Err(("NAVIGATION_FAILED", cause.to_string())),
                Err(_) => {
                    let cause =
                        "Navigation deadline: partial outcome; recreate session explicitly".to_string();
                    fault = Some(cause.clone());
                    Err(("OUTCOME_UNKNOWN", cause))
                }
            },
            Command::Evaluate { expression } => {
                if let Some(js) = page.js.as_mut() {
                    let watchdog = js.arm_watchdog(BUDGET);
                    let result = timeout(BUDGET, page_work(page.evaluate_for_cdp_with_timeout(&expression, true, true, 4000))).await;
                    let fired = page.js.as_mut().expect("evaluation retains runtime").disarm_watchdog(watchdog);
                    if fired || result.is_err() {
                        let cause = "Evaluation deadline: partial outcome; recreate session explicitly".to_string();
                        fault = Some(cause.clone());
                        Err(("OUTCOME_UNKNOWN", cause))
                    } else {
                        match result.unwrap() {
                            Ok(value) if !value.thrown => Ok(Some(ResultValue::Evaluation { result: EvaluationResult { value: value.value, js_type: value.js_type } })),
                            Ok(value) => Err(("JAVASCRIPT_EXCEPTION", value.description)),
                            Err(cause) => { fault = Some(cause.clone()); Err(("OUTCOME_UNKNOWN", cause)) }
                        }
                    }
                } else { Err(("RUNTIME_UNAVAILABLE", "Navigate before evaluation".into())) }
            }
            command @ (Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. } | Command::InputEvent { .. }) => {
                let input = match &command {
                    Command::Click { x, y } => Input::Click { x: *x, y: *y },
                    Command::InputText { text } => Input::Text(text),
                    Command::Scroll { x, y, delta_x, delta_y } => Input::Scroll { x: *x, y: *y, delta_x: *delta_x, delta_y: *delta_y },
                    Command::InputEvent { event } => match event {
                        InputEvent::Key { key, code, modifiers, text } => Input::Key {
                            key, code, modifiers: *modifiers, text,
                        },
                        InputEvent::Click { x, y } => Input::Click { x: *x, y: *y },
                        InputEvent::Scroll { x, y, delta_x, delta_y } => Input::Scroll {
                            x: *x, y: *y, delta_x: *delta_x, delta_y: *delta_y,
                        },
                        InputEvent::PointerDown { pointer_id, x, y, button, buttons, modifiers } => Input::PointerDown {
                            pointer_id: *pointer_id, x: *x, y: *y, button: pointer_button_name(*button), buttons: *buttons, modifiers: *modifiers,
                        },
                        InputEvent::PointerMove { pointer_id, x, y, button, buttons, modifiers } => Input::PointerMove {
                            pointer_id: *pointer_id, x: *x, y: *y, button: pointer_button_name(*button), buttons: *buttons, modifiers: *modifiers,
                        },
                        InputEvent::PointerUp { pointer_id, x, y, button, buttons, modifiers } => Input::PointerUp {
                            pointer_id: *pointer_id, x: *x, y: *y, button: pointer_button_name(*button), buttons: *buttons, modifiers: *modifiers,
                        },
                        InputEvent::PointerCancel { pointer_id, x, y, button, buttons, modifiers } => Input::PointerCancel {
                            pointer_id: *pointer_id, x: *x, y: *y, button: pointer_button_name(*button), buttons: *buttons, modifiers: *modifiers,
                        },
                    },
                    _ => unreachable!(),
                };
                if let Some(js) = page.js.as_mut() {
                    let watchdog = js.arm_watchdog(BUDGET);
                    let result = timeout(BUDGET, page_work(obscura_browser::input::perform(page, input))).await;
                    let fired = page.js.as_mut().expect("input retains runtime").disarm_watchdog(watchdog);
                    match result {
                        Ok(Ok(())) if !fired => Ok(Some(ResultValue::Input { input: input_receipt(operation) })),
                        Ok(Err(InputError::Rejected(cause))) if !fired => Err(("INPUT_REJECTED", cause)),
                        outcome => {
                            let cause = match outcome {
                                Ok(Err(InputError::Unknown(cause))) => cause,
                                _ => "Input deadline: injection outcome unknown; recreate session".into(),
                            };
                            fault = Some(cause.clone());
                            Err(("OUTCOME_UNKNOWN", cause))
                        }
                    }
                } else { Err(("INPUT_REJECTED", "Input runtime unavailable".into())) }
            }
            _ => unreachable!("control commands stay on daemon thread"),
        }
    } else { Err(("SESSION_CLOSED", "Session was explicitly closed".into())) };
    Completion { result, viewport: page.as_ref().map(|page| page.viewport), focus: None, fault, document_revision: 0, viewport_revision: 0, rotation: None }
}

fn pointer_button_name(button: PointerButton) -> &'static str {
    match button {
        PointerButton::None => "none",
        PointerButton::Left => "left",
        PointerButton::Middle => "middle",
        PointerButton::Right => "right",
        PointerButton::Back => "back",
        PointerButton::Forward => "forward",
    }
}

const REMOTE_FOCUS_QUERY: &str = r#"(function () {
    var active = document.activeElement;
    var body = document.body;
    var root = document.documentElement;
    var focused = !!active && active !== body && active !== root;
    var editable = false;
    if (focused) {
        var tag = String(active.localName || '').toLowerCase();
        var disabled = typeof globalThis.__obscura_isDisabled === 'function' && globalThis.__obscura_isDisabled(active);
        var type = String(active.type || 'text').toLowerCase();
        var textInput = tag === 'textarea' || (tag === 'input' && ['text','search','email','url','tel','password','number'].includes(type));
        editable = !disabled && !active.readOnly && !active.hasAttribute('readonly') && textInput;
        if (!editable && typeof active.getAttribute === 'function') {
            var element = active;
            while (element && element !== document) {
                var contenteditable = element.getAttribute('contenteditable');
                if (contenteditable !== null) {
                    editable = contenteditable.toLowerCase() !== 'false';
                    break;
                }
                element = element.parentElement;
            }
        }
    }
    return { focused: focused, editable: editable };
})()"#;

fn focus_snapshot(page: &mut Page) -> Option<FocusSnapshot> {
    let js = page.js.as_mut()?;
    let watchdog = js.arm_watchdog(BUDGET);
    let value = js.evaluate(REMOTE_FOCUS_QUERY);
    let fired = page.js.as_mut().expect("focus sampling retains runtime").disarm_watchdog(watchdog);
    if fired { return None; }
    let value = value.ok()?;
    let object = value.as_object()?;
    Some(FocusSnapshot {
        focused: object.get("focused")?.as_bool()?,
        editable: object.get("editable")?.as_bool()?,
    })
}

fn input_receipt(operation: &Operation) -> InputReceipt {
    InputReceipt {
        operation_id: operation.operation_id.clone(),
        session_id: operation.session_id.clone(),
        attachment_id: operation.attachment_id,
        sequence: operation.sequence,
        control_epoch: operation.control_epoch,
        viewport_revision: operation.viewport_revision,
        document_revision: operation.document_revision,
        state: InputState::Succeeded,
    }
}

fn stamp_input_receipt(
    result: &mut Result<Option<ResultValue>, (&'static str, String)>,
    operation: &Operation,
    document_revision: u64,
    viewport_revision: u64,
) {
    if let Ok(Some(ResultValue::Input { input })) = result {
        input.document_revision = document_revision;
        input.viewport_revision = viewport_revision;
        input.operation_id = operation.operation_id.clone();
        input.session_id = operation.session_id.clone();
        input.attachment_id = operation.attachment_id;
        input.sequence = operation.sequence;
        input.control_epoch = operation.control_epoch;
    }
}

#[cfg(test)]
mod tests {
    use super::stamp_input_receipt;
    use obscura_host_protocol::{InputReceipt, InputState, Operation, OperationId, ResultValue};

    fn operation() -> Operation {
        Operation { operation_id: OperationId("op-1".into()), session_id: "session".into(), attachment_id: 7,
            sequence: 3, control_epoch: 4, viewport_revision: 5, document_revision: 6 }
    }

    #[test]
    fn receipt_uses_completion_revisions_and_operation_identity() {
        let operation = operation();
        let mut result = Ok(Some(ResultValue::Input { input: InputReceipt {
            operation_id: OperationId("wrong".into()), session_id: "wrong".into(), attachment_id: 0,
            sequence: 0, control_epoch: 0, viewport_revision: 0, document_revision: 0, state: InputState::Succeeded,
        } }));
        stamp_input_receipt(&mut result, &operation, 9, 10);
        let Some(ResultValue::Input { input }) = result.unwrap() else { panic!("missing input receipt") };
        assert_eq!(input.operation_id, operation.operation_id);
        assert_eq!(input.session_id, operation.session_id);
        assert_eq!(input.attachment_id, 7);
        assert_eq!(input.sequence, 3);
        assert_eq!(input.control_epoch, 4);
        assert_eq!(input.viewport_revision, 10);
        assert_eq!(input.document_revision, 9);
    }
}
