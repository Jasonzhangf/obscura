//! Bounded H.264 encoding. Owns no browser or control state.
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use anyhow::{ensure, Context, Result};
use obscura_host_protocol::{VideoPacket, VideoQualityProfile, VideoQualitySettings};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QualityControl {
    Update { session_id: String, generation: u64, requested: VideoQualitySettings, effective: VideoQualitySettings },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderProfile { pub bitrate_kbps: u32, pub crf: u8, pub preset: &'static str, pub gop: u32 }

pub fn encoder_profile(profile: VideoQualityProfile) -> EncoderProfile {
    match profile {
        VideoQualityProfile::Low => EncoderProfile { bitrate_kbps: 400, crf: 32, preset: "veryfast", gop: 30 },
        VideoQualityProfile::Balanced => EncoderProfile { bitrate_kbps: 1200, crf: 26, preset: "fast", gop: 15 },
        VideoQualityProfile::High => EncoderProfile { bitrate_kbps: 3000, crf: 20, preset: "medium", gop: 5 },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderRuntime { pub low_latency: bool, pub b_frames: u32, pub rc_lookahead: u32, pub sync_lookahead: u32, pub thread_queue_size: u32 }

pub fn encoder_runtime(low_latency: bool) -> EncoderRuntime {
    if low_latency {
        EncoderRuntime { low_latency: true, b_frames: 0, rc_lookahead: 0, sync_lookahead: 0, thread_queue_size: 1 }
    } else {
        EncoderRuntime { low_latency: false, b_frames: 2, rc_lookahead: 40, sync_lookahead: 24, thread_queue_size: 8 }
    }
}

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
pub async fn encode(binary: &Path, width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>> {
    encode_with_settings(binary, width, height, rgba, VideoQualitySettings { profile: VideoQualityProfile::Balanced, low_latency: true }).await
}

pub async fn encode_with_settings(binary: &Path, width: u32, height: u32, mut rgba: Vec<u8>, settings: VideoQualitySettings) -> Result<Vec<u8>> {
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
    let profile = encoder_profile(settings.profile);
    let runtime = encoder_runtime(settings.low_latency);
    let mut child = tokio::process::Command::new(binary)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "rawvideo", "-pixel_format", "rgba", "-video_size"])
        .arg(format!("{width}x{height}"))
        .args(["-framerate", "3", "-thread_queue_size"])
        .arg(runtime.thread_queue_size.to_string())
        .args(["-i", "pipe:0", "-frames:v", "1", "-an", "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2:color=white,scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
            "-c:v", "libx264", "-threads", "1", "-preset"])
        .arg(profile.preset)
        .args(["-tune", if settings.low_latency { "zerolatency" } else { "film" },
            "-profile:v", "baseline", "-b:v"])
        .arg(format!("{}k", profile.bitrate_kbps))
        .args(["-crf"]).arg(profile.crf.to_string())
        .args(["-g"]).arg(profile.gop.to_string())
        .args(["-bf"])
        .arg(runtime.b_frames.to_string())
        .arg("-x264-params")
        .arg(format!("repeat-headers=1:aud=1:sync-lookahead={}:rc-lookahead={}", runtime.sync_lookahead, runtime.rc_lookahead))
        .args(["-colorspace", "bt709",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_map_to_distinct_encoder_parameters() {
        assert_ne!(encoder_profile(VideoQualityProfile::Low), encoder_profile(VideoQualityProfile::Balanced));
        assert_ne!(encoder_profile(VideoQualityProfile::Balanced), encoder_profile(VideoQualityProfile::High));
        assert_ne!(encoder_profile(VideoQualityProfile::Low), encoder_profile(VideoQualityProfile::High));
        assert_eq!(encoder_profile(VideoQualityProfile::Balanced).gop, 15);
        assert_eq!(encoder_profile(VideoQualityProfile::High).gop, 5);
    }

    #[test]
    fn low_latency_locks_bframes_and_lookahead_queues() {
        assert_eq!(encoder_runtime(true), EncoderRuntime { low_latency: true, b_frames: 0, rc_lookahead: 0, sync_lookahead: 0, thread_queue_size: 1 });
        assert_eq!(encoder_runtime(false), EncoderRuntime { low_latency: false, b_frames: 2, rc_lookahead: 40, sync_lookahead: 24, thread_queue_size: 8 });
    }

    #[test]
    fn quality_control_is_typed_strict_and_preserves_requested_effective_settings() {
        let command = QualityControl::Update {
            session_id: "session".into(),
            generation: 3,
            requested: VideoQualitySettings { profile: VideoQualityProfile::Low, low_latency: true },
            effective: VideoQualitySettings { profile: VideoQualityProfile::Balanced, low_latency: false },
        };
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["type"], "update");
        assert_eq!(encoded["generation"], 3);
        assert_eq!(encoded["requested"]["profile"], "low");
        assert_eq!(encoded["effective"]["low_latency"], false);
        assert_eq!(serde_json::from_value::<QualityControl>(encoded).unwrap(), command);

        let mut unknown = serde_json::json!({"type":"update","session_id":"session","generation":3,
            "requested":{"profile":"balanced","low_latency":false},
            "effective":{"profile":"balanced","low_latency":false},"bitrate_kbps":900});
        assert!(serde_json::from_value::<QualityControl>(unknown.clone()).is_err());
        unknown["type"] = serde_json::json!("update_v1");
        assert!(serde_json::from_value::<QualityControl>(unknown).is_err());

        let invalid_profile = serde_json::json!({"type":"update","session_id":"session","generation":3,
            "requested":{"profile":"ultra","low_latency":false},
            "effective":{"profile":"balanced","low_latency":false}});
        assert!(serde_json::from_value::<QualityControl>(invalid_profile).is_err());
    }
}
