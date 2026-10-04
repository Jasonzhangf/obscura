//! Sole Page owner. No attachment or transport state enters the browser thread.
//!
//! One worker owns one profile Session: one shared `BrowserContext` and the
//! ordered list of tab `Page`s bound to it. Tabs share the profile cookie jar
//! and localStorage owner; only the active tab is captured.
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use anyhow::{Context, Result};
use obscura_browser::{BrowserContext, Page};
use obscura_host_protocol::{
    Command, EditingReceipt, EditingState, EditorContext, EvaluationResult, FrameInfo,
    FramePacket, InputBinding, InputReceipt, InputState, NavigationState, PixelFormat,
    ResultValue, TabSummary,
};
use crate::media::{self, SharedFrame};
use obscura_browser::input::{EditError, Input, InputError};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};

pub struct Completion {
    pub result: Result<Option<ResultValue>, (&'static str, String)>,
    pub viewport: Option<(f32, f32)>,
    pub fault: Option<String>,
    pub document_revision: u64,
    pub viewport_revision: u64,
    pub presentation_revision: u64,
    pub editor_context: Option<EditorContext>,
    pub active_tab_id: String,
    pub tabs: Vec<TabSummary>,
    pub navigation: NavigationState,
}
pub struct PageUpdate { pub document_revision: u64, pub fault: Option<String> }
pub struct Work { pub kind: WorkKind, pub reply: oneshot::Sender<Completion> }
pub enum WorkKind {
    Browser { command: Command, document_revision: u64, operation_id: String },
    /// Host-assigned shared viewport; declarations apply to the active Page,
    /// while explicit Agent resize retains its admitted document fence.
    Viewport { width: u32, height: u32, revision: u64, document_revision: Option<u64>, cancel: Arc<AtomicBool> },
    /// Host-local maintenance drain. Flush this profile's cookie/localStorage
    /// store and report the typed outcome. Creates no child operation.
    Flush,
}
const BUDGET: Duration = Duration::from_secs(5);

fn empty_navigation() -> NavigationState {
    NavigationState { url: String::new(), title: String::new(), can_go_back: false, can_go_forward: false }
}

fn simple(
    result: Result<Option<ResultValue>, (&'static str, String)>,
    editor_context: Option<EditorContext>,
    viewport: Option<(f32, f32)>,
) -> Completion {
    Completion {
        result,
        viewport,
        fault: None,
        document_revision: 0,
        viewport_revision: 0,
        presentation_revision: 0,
        editor_context,
        active_tab_id: String::new(),
        tabs: Vec::new(),
        navigation: empty_navigation(),
    }
}

/// True when a command changes the active document and must bump the session
/// document revision (and invalidate old frame/editor bindings).
fn document_changing(command: &Command) -> bool {
    matches!(command,
        Command::Navigate { .. } | Command::Back | Command::Forward | Command::Reload
            | Command::TabCreate {} | Command::TabSelect { .. } | Command::TabClose { .. })
}

/// True when a command changes which tab is presented. `presentation_revision`
/// then distinguishes A -> B -> A so a stale frame for the first A is rejected.
fn presentation_changing(command: &Command) -> bool {
    matches!(command, Command::TabCreate {} | Command::TabSelect { .. } | Command::TabClose { .. })
}

struct Tab { id: String, page: Page }
struct Pages { session_id: String, tabs: Vec<Tab>, active: usize, next_seq: u64 }

impl Pages {
    fn new(session_id: String) -> Self {
        Self { session_id, tabs: Vec::new(), active: 0, next_seq: 0 }
    }
    fn active_page(&mut self) -> Option<&mut Page> {
        self.tabs.get_mut(self.active).map(|tab| &mut tab.page)
    }
    fn active_id(&self) -> String {
        self.tabs.get(self.active).map(|tab| tab.id.clone()).unwrap_or_default()
    }
    fn is_empty(&self) -> bool { self.tabs.is_empty() }
    fn summaries(&self) -> Vec<TabSummary> {
        self.tabs.iter().enumerate().map(|(index, tab)| TabSummary {
            tab_id: tab.id.clone(),
            url: tab.page.url_string(),
            title: tab.page.title.clone(),
            active: index == self.active,
        }).collect()
    }
    fn navigation(&self) -> NavigationState {
        match self.tabs.get(self.active) {
            Some(tab) => NavigationState {
                url: tab.page.url_string(),
                title: tab.page.title.clone(),
                can_go_back: tab.page.history_index > 0,
                can_go_forward: tab.page.history_index + 1 < tab.page.history.len(),
            },
            None => empty_navigation(),
        }
    }
    async fn create_tab(&mut self, context: &Arc<BrowserContext>) -> Result<(), obscura_browser::PageError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let tab_id = format!("{}:t{}", self.session_id, seq);
        let mut page = Page::new(tab_id.clone(), context.clone());
        page.navigate("about:blank").await?;
        self.tabs.push(Tab { id: tab_id, page });
        self.active = self.tabs.len() - 1;
        Ok(())
    }
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

pub fn start(id: String, profile_dir: Option<PathBuf>, network: bool, mut incoming: mpsc::Receiver<Work>,
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
            let context = match BrowserContext::with_profile_storage(id.clone(), profile_dir, false, None, network) {
                Ok(context) => Arc::new(context),
                Err(cause) => {
                    let message = cause.message;
                    let _ = ready.send(Err(message.clone()));
                    return Err(anyhow::anyhow!("profile storage initialization failed: {message}"));
                }
            };
            let mut pages = Pages::new(id.clone());
            if let Err(cause) = pages.create_tab(&context).await {
                let _ = ready.send(Err(cause.to_string()));
                return Err(cause).context("initialize session");
            }
            let initial_viewport = pages.active_page().map(|page| page.viewport).unwrap_or((800.0, 600.0));
            if ready.send(Ok(initial_viewport)).is_err() { return Ok(()); }
            let mut failure: Option<String> = None;
            let mut current_editor_context: Option<EditorContext> = None;
            let mut document_revision = 0;
            let mut viewport_revision = 0;
            let mut presentation_revision = 0;
            let mut frame_sequence = 0u64;
            let mut frame_clock = tokio::time::interval(Duration::from_millis(333));
            frame_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut idle = false;
            let mut frame_fence: Option<Arc<AtomicBool>> = None;
            let mut clock = tokio::time::interval(Duration::from_millis(20));
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                // Navigation caused by dispatched input/page scripts is page work,
                // after the input receipt. Never cancel it to inject another input.
                if failure.is_none() {
                    if let Some(page) = pages.active_page() {
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
                        if frame_fence.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire)) { continue; }
                        let frame = if let Some(cause) = &failure {
                            media::state(FramePacket::Unavailable { session_id: id.clone(), message: cause.clone() })
                        } else {
                            let tab_id = pages.active_id();
                            match pages.active_page() {
                                Some(page) => match capture(page).await {
                                    Ok((width, height, pixels)) => {
                                        frame_sequence = frame_sequence.checked_add(1).context("frame sequence exhausted")?;
                                        Arc::new(media::Frame {
                                            packet: FramePacket::Frame { info: FrameInfo { session_id: id.clone(), tab_id,
                                                sequence: frame_sequence, presentation_revision, document_revision,
                                                viewport_revision, width, height, stride: width * 4,
                                                byte_length: pixels.len() as u64, pixel_format: PixelFormat::PremultipliedRgba8 } },
                                            pixels,
                                        })
                                    }
                                    Err(message) => media::state(FramePacket::Unavailable { session_id: id.clone(), message }),
                                },
                                None => media::state(FramePacket::Closed { session_id: id.clone() }),
                            }
                        };
                        if frame_fence.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire)) { continue; }
                        frames.send_replace(frame);
                    }
                    work = incoming.recv() => {
                        let Some(work) = work else { break; };
                        let expected_document = match &work.kind {
                            WorkKind::Browser { document_revision, .. } => Some(*document_revision),
                            WorkKind::Viewport { document_revision, .. } => *document_revision,
                            WorkKind::Flush => None,
                        };
                        let drain_exempt = matches!(work.kind, WorkKind::Flush)
                            || matches!(work.kind, WorkKind::Browser { command: Command::CloseSession {}, .. });
                        let mut completion = if failure.is_some() && !drain_exempt {
                            simple(Err(("SESSION_FAULTED", failure.clone().unwrap())), current_editor_context.clone(), pages.active_page().map(|page| page.viewport))
                        } else if expected_document.is_some_and(|expected| expected != document_revision) {
                            simple(Err(("STALE_DOCUMENT", "Document changed before execution".into())), current_editor_context.clone(), pages.active_page().map(|page| page.viewport))
                        } else {
                            match work.kind {
                                WorkKind::Flush => {
                                    // The context owns the shared storage; a flush
                                    // failure must travel as PROFILE_STORAGE_FAILED,
                                    // never a false `saved` receipt.
                                    match context.save_profile_storage() {
                                        Ok(()) => simple(Ok(Some(ResultValue::ProfileFlush { flushed: true, profiles: 1 })), current_editor_context.clone(), pages.active_page().map(|page| page.viewport)),
                                        Err(cause) => simple(Err((cause.code, cause.message)), current_editor_context.clone(), pages.active_page().map(|page| page.viewport)),
                                    }
                                }
                                WorkKind::Viewport { width, height, revision, cancel, .. } => {
                                    frame_fence = Some(Arc::clone(&cancel));
                                    let mut fault = None;
                                    let result = if cancel.load(Ordering::Acquire) {
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                        Ok(None)
                                    } else if let Some(page) = pages.active_page() {
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
                                    if cancel.load(Ordering::Acquire) {
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                    }
                                    let mut completion = simple(result, current_editor_context.clone(), pages.active_page().map(|page| page.viewport));
                                    completion.fault = fault;
                                    completion.viewport_revision = viewport_revision;
                                    completion
                                }
                                WorkKind::Browser { command, operation_id, .. } => {
                                    let bump_document = document_changing(&command);
                                    let bump_presentation = presentation_changing(&command);
                                    let completion = execute(&mut pages, &context, command, &operation_id, current_editor_context.clone()).await;
                                    let ok = completion.result.is_ok();
                                    if ok && bump_document {
                                        document_revision += 1;
                                        frames.send_replace(media::state(FramePacket::Waiting { session_id: id.clone() }));
                                    }
                                    if ok && bump_presentation { presentation_revision += 1; }
                                    completion
                                }
                            }
                        };
                        completion.document_revision = document_revision;
                        completion.viewport_revision = viewport_revision;
                        completion.presentation_revision = presentation_revision;
                        completion.active_tab_id = pages.active_id();
                        completion.tabs = pages.summaries();
                        completion.navigation = pages.navigation();
                        current_editor_context = completion.editor_context.clone();
                        if completion.fault.is_some() { failure = completion.fault.clone(); }
                        if pages.is_empty() { frames.send_replace(media::state(FramePacket::Closed { session_id: id.clone() })); }
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
                    result = async {
                        if failure.is_some() || pages.is_empty() { return std::future::pending().await; }
                        if idle { clock.tick().await; }
                        page_work(pages.active_page().unwrap().run_autonomous_event_loop_turn()).await
                    } => match result {
                        Ok(next_idle) => idle = next_idle,
                        Err(cause) => {
                            failure = Some(cause.clone());
                            if updates.send(PageUpdate { document_revision, fault: Some(cause) }).await.is_err() { break; }
                        }
                    },
                }
            }
            if let Err(cause) = context.save_profile_storage() {
                return Err(anyhow::anyhow!("{}: {cause}", cause.code));
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

fn gesture_state(receipt: obscura_browser::input::GestureReceipt) -> InputState {
    match receipt {
        obscura_browser::input::GestureReceipt::Pending => InputState::Pending,
        obscura_browser::input::GestureReceipt::Succeeded => InputState::Succeeded,
        obscura_browser::input::GestureReceipt::Cancelled => InputState::Cancelled,
    }
}

fn editing_state(state: &str) -> EditingState {
    match state {
        "pending" => EditingState::Pending,
        "applied" => EditingState::Applied,
        "cancelled" => EditingState::Cancelled,
        "failed" => EditingState::Failed,
        "unknown" => EditingState::Unknown,
        _ => EditingState::Rejected,
    }
}

async fn execute(
    pages: &mut Pages,
    context: &Arc<BrowserContext>,
    command: Command,
    operation_id: &str,
    current_editor_context: Option<EditorContext>,
) -> Completion {
    let mut fault = None;
    let mut editor_context = current_editor_context;
    if matches!(command, Command::CloseSession {}) {
        // Persist the shared profile store before the Pages and their context
        // are dropped. A failed flush must not publish a false `closed:true`.
        if let Err(cause) = context.save_profile_storage() {
            return simple(Err((cause.code, cause.message)), editor_context, pages.active_page().map(|page| page.viewport));
        }
        pages.tabs.clear();
        editor_context = None;
        return simple(Ok(Some(ResultValue::Closed { closed: true })), None, None);
    }
    if pages.is_empty() {
        return simple(Err(("SESSION_CLOSED", "Session was explicitly closed".into())), editor_context, None);
    }
    let result = match command {
        Command::Navigate { url } => {
            let page = pages.active_page().expect("non-empty pages");
            match timeout(BUDGET, page_work(page.navigate(&url))).await {
                Ok(Ok(())) => { editor_context = None; Ok(None) }
                Ok(Err(cause)) => Err(("NAVIGATION_FAILED", cause.to_string())),
                Err(_) => {
                    let cause = "Navigation deadline: partial outcome; recreate session explicitly".to_string();
                    fault = Some(cause.clone());
                    Err(("OUTCOME_UNKNOWN", cause))
                }
            }
        }
        Command::Back | Command::Forward => {
            let backward = matches!(command, Command::Back);
            let page = pages.active_page().expect("non-empty pages");
            let index = if backward {
                if page.history_index == 0 { return simple(Err(("NO_HISTORY", "No earlier history entry".into())), editor_context, Some(page.viewport)); }
                page.history_index - 1
            } else {
                if page.history_index + 1 >= page.history.len() { return simple(Err(("NO_HISTORY", "No later history entry".into())), editor_context, Some(page.viewport)); }
                page.history_index + 1
            };
            match timeout(BUDGET, page_work(page.navigate_history_entry(index))).await {
                Ok(Ok(())) => { editor_context = None; Ok(None) }
                Ok(Err(cause)) => Err(("NAVIGATION_FAILED", cause.to_string())),
                Err(_) => {
                    let cause = "History navigation deadline: partial outcome; recreate session explicitly".to_string();
                    fault = Some(cause.clone());
                    Err(("OUTCOME_UNKNOWN", cause))
                }
            }
        }
        Command::Reload => {
            let page = pages.active_page().expect("non-empty pages");
            match timeout(BUDGET, page_work(page.reload())).await {
                Ok(Ok(())) => { editor_context = None; Ok(None) }
                Ok(Err(cause)) => Err(("NAVIGATION_FAILED", cause.to_string())),
                Err(_) => {
                    let cause = "Reload deadline: partial outcome; recreate session explicitly".to_string();
                    fault = Some(cause.clone());
                    Err(("OUTCOME_UNKNOWN", cause))
                }
            }
        }
        Command::TabCreate {} => match pages.create_tab(context).await {
            Ok(()) => { editor_context = None; Ok(None) }
            Err(cause) => Err(("NAVIGATION_FAILED", cause.to_string())),
        },
        Command::TabSelect { tab_id } => {
            match pages.tabs.iter().position(|tab| tab.id == tab_id) {
                Some(index) => {
                    if index != pages.active { pages.active = index; }
                    editor_context = None;
                    Ok(None)
                }
                None => Err(("UNKNOWN_TAB", "No tab with that id in this session".into())),
            }
        }
        Command::TabClose { tab_id } => {
            let Some(index) = pages.tabs.iter().position(|tab| tab.id == tab_id) else {
                return simple(Err(("UNKNOWN_TAB", "No tab with that id in this session".into())), editor_context, pages.active_page().map(|page| page.viewport));
            };
            if pages.tabs.len() == 1 {
                return simple(Err(("LAST_TAB", "A session must keep at least one tab".into())), editor_context, pages.active_page().map(|page| page.viewport));
            }
            pages.tabs.remove(index);
            if index < pages.active || pages.active >= pages.tabs.len() {
                pages.active = pages.active.saturating_sub(1);
            }
            editor_context = None;
            Ok(None)
        }
        Command::Evaluate { expression } => {
            let page = pages.active_page().expect("non-empty pages");
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
                        Ok(value) if !value.thrown => {
                            // Evaluate can mutate the focused editor (for example a page
                            // script changes value or selection). Publish the Engine's
                            // current context in the same completion so the Host does
                            // not keep fencing a binding against stale context.
                            let context = obscura_browser::input::editor_context_json(page)
                                .ok()
                                .and_then(|value| serde_json::from_value(value).ok());
                            editor_context = context;
                            Ok(Some(ResultValue::Evaluation { result: EvaluationResult { value: value.value, js_type: value.js_type } }))
                        }
                        Ok(value) => Err(("JAVASCRIPT_EXCEPTION", value.description)),
                        Err(cause) => { fault = Some(cause.clone()); Err(("OUTCOME_UNKNOWN", cause)) }
                    }
                }
            } else { Err(("RUNTIME_UNAVAILABLE", "Navigate before evaluation".into())) }
        }
        command @ (Command::Click { .. } | Command::InputText { .. } | Command::Scroll { .. }) => {
            let page = pages.active_page().expect("non-empty pages");
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
                    Ok(Ok(context)) if !fired => {
                        editor_context = context.and_then(|value| serde_json::from_value(value).ok());
                        Ok(Some(ResultValue::Input { input: InputReceipt { state: InputState::Succeeded } }))
                    }
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
        Command::Input { envelope } => {
            let page = pages.active_page().expect("non-empty pages");
            let event = match serde_json::to_value(&envelope.event) {
                Ok(event) => event,
                Err(cause) => {
                    return simple(Err(("INVALID_EDITOR_EVENT", cause.to_string())), editor_context, Some(page.viewport));
                }
            };
            match &envelope.binding {
                InputBinding::Gesture { .. } => {
                    if page.js.is_some() {
                        let js = page.js.as_mut().expect("checked runtime");
                        let watchdog = js.arm_watchdog(BUDGET);
                        let result = timeout(BUDGET, page_work(obscura_browser::input::perform_gesture(page, &event))).await;
                        let fired = page.js.as_mut().expect("input retains runtime").disarm_watchdog(watchdog);
                        match result {
                            Ok(Ok((receipt, context))) if !fired => {
                                editor_context = context.and_then(|value| serde_json::from_value(value).ok());
                                Ok(Some(ResultValue::Input { input: InputReceipt { state: gesture_state(receipt) } }))
                            }
                            Ok(Err(InputError::Rejected(cause))) if !fired => Err(("INPUT_REJECTED", cause)),
                            outcome => {
                                let cause = match outcome {
                                    Ok(Err(InputError::Unknown(cause))) => cause,
                                    _ => "Gesture deadline: injection outcome unknown; recreate session".into(),
                                };
                                fault = Some(cause.clone());
                                Err(("OUTCOME_UNKNOWN", cause))
                            }
                        }
                    } else { Err(("INPUT_REJECTED", "Input runtime unavailable".into())) }
                }
                InputBinding::Editor { edit_session_id, target_id, focus_revision, editor_revision, .. } => {
                    let binding = match serde_json::to_value(&envelope.binding) {
                        Ok(binding) => binding,
                        Err(cause) => {
                            return simple(Err(("INVALID_EDITOR_EVENT", cause.to_string())), editor_context, Some(page.viewport));
                        }
                    };
                    if page.js.is_some() {
                        let js = page.js.as_mut().expect("checked runtime");
                        let watchdog = js.arm_watchdog(BUDGET);
                        let result = page_work(async { obscura_browser::input::apply_editor_event(page, &binding, &event) }).await;
                        let fired = page.js.as_mut().expect("input retains runtime").disarm_watchdog(watchdog);
                        match result {
                            Ok(value) if !fired => {
                                editor_context = value.get("context").cloned()
                                    .and_then(|value| serde_json::from_value(value).ok());
                                let selection = value.get("selection").cloned()
                                    .and_then(|value| serde_json::from_value(value).ok());
                                let composition = value.get("composition").cloned()
                                    .and_then(|value| serde_json::from_value(value).ok());
                                let state = editing_state(value.get("state").and_then(|value| value.as_str()).unwrap_or("rejected"));
                                let confirmed_revision = value.get("editor_revision").and_then(|value| value.as_u64()).unwrap_or(*editor_revision);
                                Ok(Some(ResultValue::Editing { editing: EditingReceipt {
                                    operation_id: operation_id.to_string(),
                                    edit_session_id: edit_session_id.clone(),
                                    target_id: target_id.clone(),
                                    focus_revision: *focus_revision,
                                    editor_revision: confirmed_revision,
                                    event_sequence: envelope.event_sequence,
                                    state,
                                    selection,
                                    composition,
                                    message: None,
                                } }))
                            }
                            Err(EditError::Rejected(code, cause)) if !fired => Err((code, cause)),
                            outcome => {
                                let cause = match outcome {
                                    Err(EditError::Unknown(cause)) => cause,
                                    _ => "Editing deadline: injection outcome unknown; recreate session".into(),
                                };
                                fault = Some(cause.clone());
                                Err(("OUTCOME_UNKNOWN", cause))
                            }
                        }
                    } else { Err(("INPUT_REJECTED", "Input runtime unavailable".into())) }
                }
            }
        }
        _ => unreachable!("control commands stay on daemon thread"),
    };
    let mut completion = simple(result, editor_context, pages.active_page().map(|page| page.viewport));
    completion.fault = fault;
    completion
}
