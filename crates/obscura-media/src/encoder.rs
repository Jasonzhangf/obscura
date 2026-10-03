//! Resident H.264 encoder internal API.
//!
//! One `ResidentEncoder` owns one VideoToolbox incarnation plus bounded raw,
//! in-flight and completed queues. The immutable submission context rides with
//! every sample through the async VideoToolbox callback, so a callback can
//! never be mistaken for newer source, settings, or PTS. Failures and overflow
//! are typed outcomes; there is no fallback and no empty success AU.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use obscura_host_protocol::{FrameInfo, PixelFormat};

#[cfg(target_os = "macos")]
use crate::videotoolbox::{PixelBuffer, VtSession, compose_pixel_buffer};

pub const MAX_RAW_QUEUE: usize = 4;
pub const MAX_IN_FLIGHT: usize = 4;
pub const MAX_COMPLETED: usize = 64;

/// Requested encoder settings. `apply` confirms this same shape only after a
/// real SDK readback; a requested value is never treated as applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub fps: u32,
    pub keyframe_interval: u32,
}

impl EncoderSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.bitrate_bps == 0 {
            return Err("bitrate_bps must be positive".into());
        }
        if self.fps == 0 || self.fps > 240 {
            return Err("fps must be between 1 and 240".into());
        }
        if self.keyframe_interval == 0 {
            return Err("keyframe_interval must be positive".into());
        }
        Ok(())
    }
}

/// Immutable context bound to one submitted frame. It is carried through the
/// async VideoToolbox callback and cannot be relabelled as newer state.
#[derive(Debug, Clone)]
pub struct SubmissionContext {
    pub source: FrameInfo,
    pub encoder_incarnation: u64,
    pub settings_generation: u64,
    pub pts_us: u64,
}

/// One real encoded Annex B access unit with its source-bound context.
#[derive(Debug, Clone)]
pub struct EncodedSample {
    pub context: SubmissionContext,
    pub keyframe: bool,
    pub coded_width: u32,
    pub coded_height: u32,
    pub vt_pts_us: i64,
    pub duration_us: u64,
    pub bytes: Vec<u8>,
}

/// What the encoder actor received from one async VideoToolbox callback.
pub enum CallbackEvent {
    Sample(EncodedSample),
    Failed { context: SubmissionContext, status: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyRejection {
    StaleGeneration,
    Closed,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { generation: u64, settings: EncoderSettings },
    Rejected(ApplyRejection),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderEvent {
    RawLatestWins { dropped: usize },
    SampleQueueOverflow { dropped: usize },
    DependencyBreak { reason: String },
    EncoderTerminated { message: String },
}

#[derive(Debug, Clone)]
pub struct CloseOutcome {
    pub samples: Vec<EncodedSample>,
    pub cancelled_raw: usize,
    pub events: Vec<EncoderEvent>,
}

#[derive(Debug, Clone)]
pub enum EncoderError {
    Unsupported,
    InvalidSettings(String),
    InvalidFrame(String),
    Closed,
    Terminated(String),
    SizeChangeRequiresNewIncarnation,
    CreateFailed(String),
    EncodeFailed(String),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncoderError::Unsupported => write!(f, "resident H.264 encoder is macOS-only"),
            EncoderError::InvalidSettings(m) => write!(f, "invalid encoder settings: {m}"),
            EncoderError::InvalidFrame(m) => write!(f, "invalid frame: {m}"),
            EncoderError::Closed => write!(f, "encoder is closed"),
            EncoderError::Terminated(m) => write!(f, "encoder incarnation terminated: {m}"),
            EncoderError::SizeChangeRequiresNewIncarnation => {
                write!(f, "coded size change requires a new encoder incarnation")
            }
            EncoderError::CreateFailed(m) => write!(f, "encoder create failed: {m}"),
            EncoderError::EncodeFailed(m) => write!(f, "encoder failed: {m}"),
        }
    }
}

impl std::error::Error for EncoderError {}

struct RawSubmission {
    buffer: PixelBuffer,
    context: SubmissionContext,
}

/// A resident Mac H.264 encoder: the internal public boundary a real library or
/// CLI consumer drives through create -> submit -> apply -> force IDR ->
/// next_sample -> close.
pub struct ResidentEncoder {
    width: u32,
    height: u32,
    settings: EncoderSettings,
    generation: u64,
    incarnation: u64,
    vt: Option<VtSession>,
    rx: Receiver<CallbackEvent>,
    raw: VecDeque<RawSubmission>,
    completed: VecDeque<EncodedSample>,
    events: Vec<EncoderEvent>,
    force_idr_pending: bool,
    closed: bool,
    terminated: bool,
}

impl ResidentEncoder {
    pub fn create(width: u32, height: u32, settings: EncoderSettings) -> Result<Self, EncoderError> {
        settings.validate().map_err(EncoderError::InvalidSettings)?;
        crate::validate_pixels(width, height, u64::from(width) * u64::from(height) * 4)
            .map_err(|e| EncoderError::InvalidFrame(e.to_string()))?;
        let (tx, rx): (SyncSender<CallbackEvent>, Receiver<CallbackEvent>) =
            std::sync::mpsc::sync_channel(MAX_IN_FLIGHT + MAX_COMPLETED);
        let vt = VtSession::create(
            width,
            height,
            settings.fps,
            settings.bitrate_bps,
            settings.keyframe_interval,
            tx,
        )
        .map_err(|e| EncoderError::CreateFailed(e.to_string()))?;
        Ok(Self {
            width,
            height,
            settings,
            generation: 0,
            incarnation: 1,
            vt: Some(vt),
            rx,
            raw: VecDeque::new(),
            completed: VecDeque::new(),
            events: Vec::new(),
            force_idr_pending: false,
            closed: false,
            terminated: false,
        })
    }

    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    pub fn coded_dimensions(&self) -> (u32, u32) {
        self.vt
            .as_ref()
            .map(|v| v.coded_dimensions())
            .unwrap_or(((self.width + 1) & !1, (self.height + 1) & !1))
    }

    pub fn pending_keyframe(&self) -> bool {
        self.force_idr_pending
    }

    /// Validate and enqueue one premultiplied RGBA source frame. The raw queue
    /// is bounded and latest-wins before encoding; a dropped pre-encode frame
    /// cannot break a reference chain because it was never submitted.
    pub fn submit(
        &mut self,
        info: FrameInfo,
        rgba: &[u8],
        pts_us: u64,
    ) -> Result<(), EncoderError> {
        if self.closed {
            return Err(EncoderError::Closed);
        }
        if self.terminated {
            return Err(EncoderError::Terminated("incarnation already terminated".into()));
        }
        if self.observe_callback_overflow() {
            return Err(EncoderError::Terminated("callback event queue overflow".into()));
        }
        crate::validate_pixels(info.width, info.height, rgba.len() as u64)
            .map_err(|e| EncoderError::InvalidFrame(e.to_string()))?;
        if info.stride != info.width * 4 {
            return Err(EncoderError::InvalidFrame("stride must equal width*4".into()));
        }
        if info.pixel_format != PixelFormat::PremultipliedRgba8 {
            return Err(EncoderError::InvalidFrame("expected PremultipliedRgba8".into()));
        }
        if info.width != self.width || info.height != self.height {
            return Err(EncoderError::SizeChangeRequiresNewIncarnation);
        }
        let source_stride = info.stride;
        let context = SubmissionContext {
            source: info,
            encoder_incarnation: self.incarnation,
            settings_generation: self.generation,
            pts_us,
        };
        let buffer = compose_pixel_buffer(self.width, self.height, source_stride, rgba)
            .map_err(|e| EncoderError::EncodeFailed(e.to_string()))?;
        if self.raw.len() >= MAX_RAW_QUEUE {
            self.raw.pop_front();
            self.events.push(EncoderEvent::RawLatestWins { dropped: 1 });
        }
        self.raw.push_back(RawSubmission { buffer, context });
        self.pump();
        if self.terminated {
            return Err(EncoderError::Terminated("incarnation terminated during submit".into()));
        }
        Ok(())
    }

    /// Apply a new settings generation to the same-sized incarnation. The SDK
    /// set and a real readback must both confirm before the outcome is Applied;
    /// a partial failure terminates this incarnation instead of claiming the
    /// old settings are still in effect.
    pub fn apply(&mut self, generation: u64, settings: EncoderSettings) -> ApplyOutcome {
        if self.closed {
            return ApplyOutcome::Rejected(ApplyRejection::Closed);
        }
        if self.terminated {
            return ApplyOutcome::Rejected(ApplyRejection::Terminated);
        }
        if self.observe_callback_overflow() {
            return ApplyOutcome::Rejected(ApplyRejection::Terminated);
        }
        if generation <= self.generation {
            return ApplyOutcome::Rejected(ApplyRejection::StaleGeneration);
        }
        if let Err(message) = settings.validate() {
            return ApplyOutcome::Failed(message);
        }
        if let Err(message) = self.settle_settings_boundary() {
            self.terminate(message.clone());
            return ApplyOutcome::Failed(message);
        }
        if settings.keyframe_interval != self.settings.keyframe_interval {
            // VideoToolbox can accept and read back a new interval while the
            // current GOP continues under the old cadence. Establish a real
            // new incarnation so the confirmed interval starts from frame 0.
            self.vt = None;
            let (tx, rx) = std::sync::mpsc::sync_channel(MAX_IN_FLIGHT + MAX_COMPLETED);
            match VtSession::create(
                self.width,
                self.height,
                settings.fps,
                settings.bitrate_bps,
                settings.keyframe_interval,
                tx,
            ) {
                Ok(vt) => {
                    self.vt = Some(vt);
                    self.rx = rx;
                    self.incarnation += 1;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.terminate(message.clone());
                    return ApplyOutcome::Failed(message);
                }
            }
        } else {
            let Some(vt) = &self.vt else {
                return ApplyOutcome::Failed("encoder incarnation missing".into());
            };
            let mut failures = Vec::new();
            if settings.bitrate_bps != self.settings.bitrate_bps {
                if let Err(e) = vt.apply_bitrate(settings.bitrate_bps) {
                    failures.push(e.to_string());
                }
            }
            if settings.fps != self.settings.fps {
                if let Err(e) = vt.apply_fps(settings.fps) {
                    failures.push(e.to_string());
                }
            }
            if !failures.is_empty() {
                let message = failures.join("; ");
                self.terminate(message.clone());
                return ApplyOutcome::Failed(message);
            }
        }
        self.settings = settings;
        self.generation = generation;
        self.force_idr_pending = true;
        ApplyOutcome::Applied { generation, settings }
    }

    pub fn request_keyframe(&mut self) -> Result<(), EncoderError> {
        if self.closed {
            return Err(EncoderError::Closed);
        }
        if self.terminated {
            return Err(EncoderError::Terminated("incarnation already terminated".into()));
        }
        self.force_idr_pending = true;
        Ok(())
    }

    /// Stop accepting input, cancel unsubmitted raw frames, complete in-flight
    /// work, wait for callback quiescence, then release the session.
    ///
    /// Returns the drained samples, the count of cancelled raw frames, and the
    /// accumulated events. `vt` is dropped only after callbacks have quiesced,
    /// so no late callback can read freed source, pixel buffers, or refcons.
    pub fn close(&mut self) -> CloseOutcome {
        self.closed = true;
        let cancelled_raw = self.raw.len();
        self.raw.clear();
        let mut samples: Vec<EncodedSample> = self.completed.drain(..).collect();
        if let Some(vt) = &self.vt {
            if let Err(e) = vt.complete_frames() {
                self.events.push(EncoderEvent::DependencyBreak {
                    reason: format!("complete_frames failed: {e}"),
                });
            }
        }
        while let Some(sample) = self.next_sample(Duration::from_secs(5)) {
            samples.push(sample);
        }
        self.observe_callback_overflow();
        if let Some(vt) = &self.vt {
            if vt.live() != 0 {
                self.events.push(EncoderEvent::DependencyBreak {
                    reason: format!("{} callbacks still in flight at close", vt.live()),
                });
            }
        }
        self.vt = None;
        while let Ok(event) = self.rx.try_recv() {
            self.process_event(event, &mut samples);
        }
        CloseOutcome {
            samples,
            cancelled_raw,
            events: std::mem::take(&mut self.events),
        }
    }

    /// Block until a real sample is available (or the incarnation is
    /// terminated) within `timeout`.
    pub fn next_sample(&mut self, timeout: Duration) -> Option<EncodedSample> {
        let start = Instant::now();
        loop {
            self.pump();
            if let Some(sample) = self.completed.pop_front() {
                return Some(sample);
            }
            if self.terminated || self.closed {
                return None;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return None;
            }
            match self.rx.recv_timeout(timeout - elapsed) {
                Ok(event) => {
                    let mut sink = Vec::new();
                    self.process_event(event, &mut sink);
                    self.completed.append(&mut sink.into());
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    self.terminated = true;
                    return None;
                }
            }
        }
    }

    pub fn take_events(&mut self) -> Vec<EncoderEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    fn terminate(&mut self, message: String) {
        if !self.terminated {
            self.terminated = true;
            self.events.push(EncoderEvent::EncoderTerminated { message });
        }
    }

    fn observe_callback_overflow(&mut self) -> bool {
        let dropped = self
            .vt
            .as_ref()
            .map(|vt| vt.take_callback_overflows())
            .unwrap_or(0);
        if dropped == 0 {
            return false;
        }
        self.events.push(EncoderEvent::SampleQueueOverflow { dropped });
        self.terminate("callback event queue overflow".into());
        true
    }

    /// Wait until no generation-N raw frame or callback remains before the
    /// session properties are changed for generation N+1.
    fn settle_settings_boundary(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.pump();
            if self.terminated {
                return Err("encoder terminated while draining settings boundary".into());
            }
            let pending = !self.raw.is_empty()
                || self.vt.as_ref().map(|vt| vt.live() > 0).unwrap_or(false);
            if !pending {
                let mut sink = Vec::new();
                while let Ok(event) = self.rx.try_recv() {
                    self.process_event(event, &mut sink);
                }
                self.completed.extend(sink);
                if self.terminated {
                    return Err("encoder terminated while draining settings boundary".into());
                }
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out draining old settings generation".into());
            }
            match self.rx.recv_timeout(remaining) {
                Ok(event) => {
                    let mut sink = Vec::new();
                    self.process_event(event, &mut sink);
                    self.completed.extend(sink);
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err("timed out draining old settings generation".into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("callback channel disconnected during settings boundary".into());
                }
            }
        }
    }

    /// Move queued raw frames into the encoder while in-flight capacity remains.
    fn pump(&mut self) {
        if self.observe_callback_overflow() {
            return;
        }
        if self.terminated {
            return;
        }
        loop {
            if self.observe_callback_overflow() {
                return;
            }
            let Some(vt) = self.vt.as_ref() else {
                return;
            };
            if vt.live() >= MAX_IN_FLIGHT || self.raw.is_empty() {
                return;
            }
            let submission = self.raw.pop_front().expect("raw checked non-empty");
            let RawSubmission { buffer, context } = submission;
            let force = self.force_idr_pending;
            self.force_idr_pending = false;
            if let Err(e) = vt.encode(&buffer, context.pts_us, force, Box::new(context.clone())) {
                self.terminate(e.to_string());
                return;
            }
        }
    }

    fn process_event(&mut self, event: CallbackEvent, sink: &mut Vec<EncodedSample>) {
        match event {
            CallbackEvent::Sample(sample) => {
                if sample.bytes.is_empty() {
                    self.terminate("VideoToolbox produced an empty access unit".into());
                    return;
                }
                if self.completed.len() + sink.len() >= MAX_COMPLETED {
                    self.terminate("completed sample queue overflowed".into());
                    self.events.push(EncoderEvent::SampleQueueOverflow { dropped: 1 });
                    return;
                }
                sink.push(sample);
            }
            CallbackEvent::Failed { context, status } => {
                self.terminate(format!(
                    "VideoToolbox encode failed for sequence {} (status {status})",
                    context.source.sequence
                ));
            }
        }
    }
}
