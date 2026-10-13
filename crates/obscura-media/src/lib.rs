//! Bounded H.264 encoding. Owns no browser or control state.
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use anyhow::{ensure, Context, Result};
use obscura_host_protocol::VideoPacket;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "macos")]
pub mod encoder;
#[cfg(not(target_os = "macos"))]
pub mod encoder {
    use obscura_host_protocol::FrameInfo;
    use std::time::Duration;

    pub const MAX_RAW_QUEUE: usize = 4;
    pub const MAX_IN_FLIGHT: usize = 4;
    pub const MAX_COMPLETED: usize = 64;

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

    #[derive(Debug, Clone)]
    pub struct SubmissionContext {
        pub source: FrameInfo,
        pub encoder_incarnation: u64,
        pub settings_generation: u64,
        pub pts_us: u64,
    }

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
                EncoderError::Unsupported => {
                    write!(f, "resident H.264 encoder is macOS-only")
                }
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

    pub struct ResidentEncoder;

    impl ResidentEncoder {
        pub fn create(
            _width: u32,
            _height: u32,
            _settings: EncoderSettings,
        ) -> Result<Self, EncoderError> {
            Err(EncoderError::Unsupported)
        }

        pub fn incarnation(&self) -> u64 {
            0
        }

        pub fn generation(&self) -> u64 {
            0
        }

        pub fn settings(&self) -> EncoderSettings {
            EncoderSettings { bitrate_bps: 0, fps: 0, keyframe_interval: 0 }
        }

        pub fn coded_dimensions(&self) -> (u32, u32) {
            (0, 0)
        }

        pub fn pending_keyframe(&self) -> bool {
            false
        }

        pub fn submit(
            &mut self,
            _info: FrameInfo,
            _rgba: &[u8],
            _pts_us: u64,
        ) -> Result<(), EncoderError> {
            Err(EncoderError::Unsupported)
        }

        pub fn apply(&mut self, _generation: u64, _settings: EncoderSettings) -> ApplyOutcome {
            ApplyOutcome::Failed("resident H.264 encoder is macOS-only".into())
        }

        pub fn request_keyframe(&mut self) -> Result<(), EncoderError> {
            Err(EncoderError::Unsupported)
        }

        pub fn close(&mut self) -> CloseOutcome {
            CloseOutcome { samples: Vec::new(), cancelled_raw: 0, events: Vec::new() }
        }

        pub fn next_sample(&mut self, _timeout: Duration) -> Option<EncodedSample> {
            None
        }

        pub fn take_events(&mut self) -> Vec<EncoderEvent> {
            Vec::new()
        }

        pub fn is_terminated(&self) -> bool {
            true
        }
    }
}
#[cfg(target_os = "macos")]
pub(crate) mod videotoolbox;

pub mod encoder_owner;

#[cfg(any(feature = "webrtc-probe", feature = "webrtc"))]
pub mod webrtc;
#[cfg(feature = "webrtc")]
pub mod webrtc_endpoint;

/// One encoded Host frame shared by the endpoint's WSS and WebRTC adapters.
/// The packet descriptor remains typed source truth; bytes are only the H.264
/// access unit belonging to that descriptor.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub packet: VideoPacket,
    pub bytes: Arc<[u8]>,
}

impl EncodedFrame {
    pub fn wire_bytes(&self) -> Result<Vec<u8>> {
        let mut header = serde_json::to_vec(&self.packet)?;
        ensure!(header.len() < 4096, "Encoded media header exceeds limit");
        let mut bytes = Vec::with_capacity(4 + header.len() + self.bytes.len());
        bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
        bytes.append(&mut header);
        bytes.extend_from_slice(&self.bytes);
        Ok(bytes)
    }

    pub fn closed(&self) -> bool {
        matches!(self.packet, VideoPacket::Closed { .. })
    }
}
pub const MAX_PIXELS: u64 = 4_194_304;
pub const MAX_ACCESS_UNIT: usize = 4 * 1024 * 1024;

pub fn validate_pixels(width: u32, height: u32, length: u64) -> Result<()> {
    let count = u64::from(width) * u64::from(height);
    ensure!(width > 0 && height > 0 && width <= 4096 && height <= 4096 && count <= MAX_PIXELS,
        "Frame dimensions exceed media budget");
    ensure!(length == count * 4, "Frame pixel length does not match dimensions");
    Ok(())
}

async fn bounded_read(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= limit, "Encoder output exceeds limit");
    Ok(bytes)
}

/// One Annex B access unit including SPS/PPS and IDR. Odd viewport dimensions
/// are padded on the right/bottom; clients display only the source viewport.
pub async fn encode(binary: &Path, width: u32, height: u32, mut rgba: Vec<u8>) -> Result<Vec<u8>> {
    validate_pixels(width, height, rgba.len() as u64)?;
    // Composite premultiplied pixels on white before dropping alpha for YUV420.
    for pixel in rgba.chunks_exact_mut(4) {
        let background = 255 - pixel[3];
        for channel in &mut pixel[..3] { *channel = channel.saturating_add(background); }
        pixel[3] = 255;
    }
    // ponytail: one encoder process and one independent IDR per frame costs
    // bitrate/startup time. Replace inside this owner with a persistent encoder
    // when performance work begins; callers retain the same access-unit ABI.
    let mut child = tokio::process::Command::new(binary)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "rawvideo", "-pixel_format", "rgba", "-video_size"])
        .arg(format!("{width}x{height}"))
        .args(["-framerate", "3", "-i", "pipe:0", "-frames:v", "1", "-an", "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2:color=white,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
            "-c:v", "libx264", "-threads", "1", "-preset", "ultrafast", "-tune", "zerolatency",
            "-profile:v", "baseline", "-crf", "23", "-g", "1", "-bf", "0",
            "-x264-params", "repeat-headers=1:aud=1", "-colorspace", "bt709",
            "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv", "-f", "h264", "pipe:1"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn().context("Start configured FFmpeg H.264 encoder")?;
    let mut input = child.stdin.take().context("Encoder input missing")?;
    let output = child.stdout.take().context("Encoder output missing")?;
    let diagnostic = child.stderr.take().context("Encoder diagnostics missing")?;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let (_, bytes, errors, status) = tokio::try_join!(
            async { input.write_all(&rgba).await?; drop(input); Ok::<_, anyhow::Error>(()) },
            bounded_read(output, MAX_ACCESS_UNIT),
            bounded_read(diagnostic, 64 * 1024),
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
        )?;
        ensure!(status.success(), "H.264 encoder failed: {}", String::from_utf8_lossy(&errors));
        ensure!(!bytes.is_empty(), "H.264 encoder produced no access unit");
        Ok(bytes)
    }).await.context("H.264 encoder exceeded five second deadline")?;
    // kill_on_drop covers cancellation/error; normal completion already reaped.
    result
}
