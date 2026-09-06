//! Sole Page owner. No attachment or transport state enters the browser thread.
use std::sync::Arc;
use anyhow::{Context, Result};
use obscura_browser::{BrowserContext, Page};
use obscura_host_protocol::{Command, EvaluationResult, FrameInfo, FramePacket, InputReceipt, InputState, PixelFormat, ResultValue};
use crate::media::{self, SharedFrame};
use obscura_browser::input::{Input, InputError};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};

pub struct Completion {
    pub result: Result<Option<ResultValue>, (&'static str, String)>,
    pub viewport: Option<(f32, f32)>,
    pub fault: Option<String>,
    pub document_revision: u64,
    pub viewport_revision: u64,
}
pub struct PageUpdate { pub document_revision: u64, pub fault: Option<String> }
pub struct Work { pub kind: WorkKind, pub reply: oneshot::Sender<Completion> }
pub enum WorkKind {
    Browser { command: Command, document_revision: u64 },
    /// Host-assigned shared viewport; declarations apply to the current Page,
    /// while explicit Agent resize retains its admitted document fence.
    Viewport { width: u32, height: u32, revision: u64, document_revision: Option<u64> },
}
const BUDGET: Duration = Duration::from_secs(5);

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

pub fn start(id: String, network: bool, mut incoming: mpsc::Receiver<Work>,
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
            let context = Arc::new(BrowserContext::with_storage_and_network(id.clone(), None, false, None, None, network));
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
            let mut frame_sequence = 0u64;
            let mut frame_clock = tokio::time::interval(Duration::from_millis(333));
            frame_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut idle = false;
            let mut clock = tokio::time::interval(Duration::from_millis(20));
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                // Navigation caused by dispatched input/page scripts is page work,
                // after the input receipt. Never cancel it to inject another input.
                if failure.is_none() {
                    if let Some(page) = page.as_mut() {
                        match timeout(BUDGET, page_work(page.process_pending_navigation())).await {
                            Ok(Ok(false)) => {},
                            outcome => {
                                document_revision += 1;
                                frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                match outcome {
                                    Ok(Ok(true)) => {},
                                    Ok(Err(cause)) => failure = Some(format!("Page navigation failed: {cause}")),
                                    Err(_) => failure = Some("Page navigation outcome unknown after deadline".into()),
                                    _ => unreachable!(),
                                }
                                if updates.send(PageUpdate { document_revision, fault: failure.clone() }).await.is_err() { break; }
                            }
                        }
                    }
                }
                tokio::select! {
                    changed = interested.changed() => { if changed.is_err() { break; } }
                    _ = frame_clock.tick(), if *interested.borrow() > 0 => {
                        let frame = if let Some(cause) = &failure {
                            media::state(FramePacket::Unavailable { session_id: id.clone(), message: cause.clone() })
                        } else if let Some(page) = page.as_mut() {
                            match capture(page).await {
                                Ok((width, height, pixels)) => {
                                    frame_sequence = frame_sequence.checked_add(1).context("frame sequence exhausted")?;
                                    Arc::new(media::Frame {
                                        packet: FramePacket::Frame { info: FrameInfo { session_id: id.clone(), sequence: frame_sequence,
                                            document_revision, viewport_revision, width, height, stride: width * 4,
                                            byte_length: pixels.len() as u64, pixel_format: PixelFormat::PremultipliedRgba8 } },
                                        pixels,
                                    })
                                }
                                Err(message) => media::state(FramePacket::Unavailable { session_id: id.clone(), message }),
                            }
                        } else { media::state(FramePacket::Closed { session_id: id.clone() }) };
                        frames.send_replace(frame);
                    }
                    work = incoming.recv() => {
                        let Some(work) = work else { break; };
                        let expected_document = match &work.kind {
                            WorkKind::Browser { document_revision, .. } => Some(*document_revision),
                            WorkKind::Viewport { document_revision, .. } => *document_revision,
                        };
                        let mut completion = if failure.is_some() && !matches!(work.kind, WorkKind::Browser { command: Command::CloseSession {}, .. }) {
                            Completion {
                                result: Err(("SESSION_FAULTED", failure.clone().unwrap())),
                                viewport: page.as_ref().map(|page| page.viewport),
                                fault: failure.clone(),
                                document_revision,
                                viewport_revision,
                            }
                        } else if expected_document.is_some_and(|expected| expected != document_revision) {
                            Completion { result: Err(("STALE_DOCUMENT", "Document changed before execution".into())), viewport: page.as_ref().map(|page| page.viewport), fault: None, document_revision, viewport_revision }
                        } else {
                            match work.kind {
                                WorkKind::Viewport { width, height, revision, .. } => {
                                    let mut fault = None;
                                    let result = if let Some(page) = page.as_mut() {
                                        if page.viewport != (width as f32, height as f32) {
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
                                        else { viewport_revision = revision; Ok(None) }
                                    } else { Err(("SESSION_CLOSED", "Session was explicitly closed".into())) };
                                    Completion { result, viewport: page.as_ref().map(|page| page.viewport), fault, document_revision, viewport_revision }
                                }
                                WorkKind::Browser { command, .. } => {
                                    let navigation = matches!(command, Command::Navigate { .. });
                                    let completion = execute(&mut page, command).await;
                                    if navigation {
                                        document_revision += 1;
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                    }
                                    completion
                                }
                            }
                        };
                        completion.document_revision = document_revision;
                        completion.viewport_revision = viewport_revision;
                        if completion.fault.is_some() { failure = completion.fault.clone(); }
                        if page.is_none() { frames.send_replace(media::state(FramePacket::Closed { session_id: id.clone() })); }
                        else if let Some(message) = &failure { frames.send_replace(media::state(FramePacket::Unavailable { session_id: id.clone(), message: message.clone() })); }
                        let _ = work.reply.send(completion);
                        idle = false;
                    }
                    result = async {
                        if failure.is_some() || page.is_none() { return std::future::pending().await; }
                        if idle { clock.tick().await; }
                        page_work(page.as_mut().unwrap().run_autonomous_event_loop_turn()).await
                    } => match result {
                        Ok(next_idle) => idle = next_idle,
                        Err(cause) => {
                            failure = Some(cause.clone());
                            if updates.send(PageUpdate { document_revision, fault: Some(cause) }).await.is_err() { break; }
                        }
                    },
                }
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

async fn execute(page: &mut Option<Page>, command: Command) -> Completion {
    let mut fault = None;
    let result = if matches!(command, Command::CloseSession {}) {
        *page = None;
        Ok(Some(ResultValue::Closed { closed: true }))
    } else if let Some(page) = page.as_mut() {
        match command {
            Command::Navigate { url } => match timeout(BUDGET, page_work(page.navigate(&url))).await {
                Ok(Ok(())) => Ok(None),
                Ok(Err(cause)) => Err(("NAVIGATION_FAILED", cause.to_string())),
                Err(_) => {
                    let cause = "Navigation deadline: partial outcome; recreate session explicitly".to_string();
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
            command @ (Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. }) => {
                let input = match &command {
                    Command::Click { x, y } => Input::Click { x: *x, y: *y },
                    Command::InputText { text } => Input::Text(text),
                    Command::Scroll { x, y, delta_x, delta_y } => Input::Scroll { x: *x, y: *y, delta_x: *delta_x, delta_y: *delta_y },
                    _ => unreachable!(),
                };
                if let Some(js) = page.js.as_mut() {
                    let watchdog = js.arm_watchdog(BUDGET);
                    let result = timeout(BUDGET, page_work(obscura_browser::input::perform(page, input))).await;
                    let fired = page.js.as_mut().expect("input retains runtime").disarm_watchdog(watchdog);
                    match result {
                        Ok(Ok(())) if !fired => Ok(Some(ResultValue::Input { input: InputReceipt { state: InputState::Succeeded } })),
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
    Completion { result, viewport: page.as_ref().map(|page| page.viewport), fault, document_revision: 0, viewport_revision: 0 }
}
