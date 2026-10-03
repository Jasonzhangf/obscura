//! Public consumer harness for the resident Mac H.264 encoder boundary.
//!
//! It drives the public library API end to end (create -> submit -> apply ->
//! force IDR -> next_sample -> close) with real VideoToolbox output, writes raw
//! Annex B streams for an independent FFmpeg decoder, and records the legacy
//! per-frame `encode()` limitation as red evidence. It owns no browser or
//! control state.
//!
//! Usage:
//!   resident-encoder <outdir>

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use obscura_host_protocol::{FrameInfo, PixelFormat};
use obscura_media::encoder::{
    ApplyOutcome, EncoderEvent, EncoderSettings, ResidentEncoder, MAX_COMPLETED, MAX_IN_FLIGHT,
    MAX_RAW_QUEUE,
};
use tokio::runtime::Runtime;

fn frame_info(width: u32, height: u32, sequence: u64) -> FrameInfo {
    FrameInfo {
        session_id: "resident-consumer".into(),
        sequence,
        document_revision: 1,
        viewport_revision: 1,
        width,
        height,
        stride: width * 4,
        byte_length: u64::from(width) * u64::from(height) * 4,
        pixel_format: PixelFormat::PremultipliedRgba8,
    }
}

fn white_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![255u8; (width * height * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[0] = 0;
        pixel[1] = 0;
        pixel[2] = 0;
    }
    rgba
}

/// Premultiplied RGBA geometry scene matching the W5CR source oracle: each row
/// carries its low byte as eight 8px cells, the last eight source columns are
/// red, and the last eight rows of that marker are blue.
fn geometry_scene(width: u32, height: u32, sequence: u64) -> Vec<u8> {
    let mut rgba = vec![255u8; (width * height * 4) as usize];
    for y in 0..height {
        for bit in 0..8u32 {
            let value = if (y >> bit) & 1 == 1 { 0 } else { 255 };
            for k in 0..8u32 {
                let x = 8 + bit * 8 + k;
                if x >= width {
                    break;
                }
                let offset = ((y * width + x) * 4) as usize;
                rgba[offset] = value;
                rgba[offset + 1] = value;
                rgba[offset + 2] = value;
                rgba[offset + 3] = 255;
            }
        }
        for x in width.saturating_sub(8)..width {
            let offset = ((y * width + x) * 4) as usize;
            rgba[offset] = 255;
            rgba[offset + 1] = 0;
            rgba[offset + 2] = 0;
            rgba[offset + 3] = 255;
        }
        let _ = sequence;
    }
    for y in height.saturating_sub(8)..height {
        for x in width.saturating_sub(8)..width {
            let offset = ((y * width + x) * 4) as usize;
            rgba[offset] = 0;
            rgba[offset + 1] = 0;
            rgba[offset + 2] = 255;
            rgba[offset + 3] = 255;
        }
    }
    rgba
}

/// Moving opaque black square plus a fixed half-transparent premultiplied red
/// block: long contiguous motion for a real I/P chain and colour evidence.
fn moving_scene(width: u32, height: u32, sequence: u64) -> Vec<u8> {
    let mut rgba = vec![255u8; (width * height * 4) as usize];
    let size = (width / 5).max(4);
    let span = width.saturating_sub(size).max(1);
    let base_x = ((sequence as u32) * 3) % span;
    for y in 0..size.min(height) {
        for x in 0..size {
            let px = base_x + x;
            if px >= width {
                break;
            }
            let offset = ((y * width + px) * 4) as usize;
            rgba[offset] = 0;
            rgba[offset + 1] = 0;
            rgba[offset + 2] = 0;
            rgba[offset + 3] = 255;
        }
    }
    for y in 0..size.min(height) {
        for x in width.saturating_sub(size)..width {
            let offset = ((y * width + x) * 4) as usize;
            rgba[offset] = 128;
            rgba[offset + 1] = 0;
            rgba[offset + 2] = 0;
            rgba[offset + 3] = 128;
        }
    }
    let _ = (white_rgba as fn(u32, u32) -> Vec<u8>);
    rgba
}

/// Deterministic high-entropy opaque noise: real changing samples that let the
/// encoder actually trade bytes against AverageBitRate over a long window.
fn noise_scene(width: u32, height: u32, sequence: u64) -> Vec<u8> {
    let mut rgba = vec![255u8; (width * height * 4) as usize];
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (sequence.wrapping_mul(0x100_0000_01B3));
    for pixel in rgba.chunks_exact_mut(4) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pixel[0] = (state >> 33) as u8;
        pixel[1] = (state >> 21) as u8;
        pixel[2] = (state >> 45) as u8;
        pixel[3] = 255;
    }
    rgba
}

fn precomputed_noise_frames(width: u32, height: u32, count: usize) -> Vec<Vec<u8>> {
    (0..count as u64)
        .map(|sequence| noise_scene(width, height, sequence))
        .collect()
}

fn nal_types(stream: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut index = 0usize;
    while index + 3 <= stream.len() {
        if stream[index] == 0 && stream[index + 1] == 0 && stream[index + 2] == 1 {
            if index + 3 < stream.len() {
                types.push(stream[index + 3] & 0x1f);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    types
}

fn write_stream(path: &Path, samples: &[obscura_media::encoder::EncodedSample]) -> std::io::Result<()> {
    std::fs::write(path, concat_stream(samples))
}

fn concat_stream(samples: &[obscura_media::encoder::EncodedSample]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for sample in samples {
        bytes.extend_from_slice(&sample.bytes);
    }
    bytes
}

/// (profile_idc, constraint_flags, level_idc) from the first SPS NAL.
fn sps_profile(stream: &[u8]) -> Option<(u8, u8, u8)> {
    let mut index = 0usize;
    while index + 4 <= stream.len() {
        if stream[index] == 0 && stream[index + 1] == 0 && stream[index + 2] == 1 {
            if stream[index + 3] & 0x1f == 7 && index + 6 < stream.len() {
                return Some((stream[index + 4], stream[index + 5], stream[index + 6]));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    None
}

fn decode_frame(path: &Path, index: u32, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i"])
        .arg(path)
        .arg("-frames:v")
        .arg((index + 1).to_string())
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("ffmpeg launch: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let expected = width as usize * height as usize * 3;
    if output.stdout.len() != expected {
        return Err(format!("decoded {} bytes, expected {expected}", output.stdout.len()));
    }
    Ok(output.stdout)
}

/// Decode every access unit in the stream and report the frame count. A broken
/// reference chain makes FFmpeg error or emit fewer frames than AUs.
fn decode_all_frames(path: &Path, width: u32, height: u32) -> Result<usize, String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i"])
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("ffmpeg launch: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let frame_bytes = width as usize * height as usize * 3;
    if frame_bytes == 0 || output.stdout.len() % frame_bytes != 0 {
        return Err(format!("decoded {} bytes not frame aligned", output.stdout.len()));
    }
    Ok(output.stdout.len() / frame_bytes)
}

fn rgb_at(frame: &[u8], width: u32, x: u32, y: u32) -> (u8, u8, u8) {
    let i = (y * width + x) as usize * 3;
    (frame[i], frame[i + 1], frame[i + 2])
}

struct Check {
    name: String,
    passed: bool,
    detail: String,
}

struct Harness {
    checks: Vec<Check>,
}

impl Harness {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!("CHECK name={name} passed={passed} {detail}");
        self.checks.push(Check { name: name.into(), passed, detail });
    }

    fn summary(&self) -> i32 {
        let failed = self.checks.iter().filter(|c| !c.passed).count();
        println!("RESULT passed={} failed={failed}", self.checks.len() - failed);
        i32::from(failed != 0)
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let outdir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".w5-resident-tmp"));
    std::fs::create_dir_all(&outdir).expect("create outdir");
    let mut h = Harness::new();
    red_legacy_limitation(&outdir, &mut h);
    resident_even(&outdir, &mut h);
    resident_odd(&outdir, &mut h);
    resident_alpha_color(&outdir, &mut h);
    resident_quality_apply(&outdir, &mut h);
    resident_pending_apply_boundary(&outdir, &mut h);
    resident_fps_keyframe_apply(&outdir, &mut h);
    resident_callback_saturation(&outdir, &mut h);
    resident_dependency_recovery(&outdir, &mut h);
    resident_close_lifecycle(&outdir, &mut h);
    resident_rejections(&outdir, &mut h);
    std::process::exit(h.summary());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("resident H.264 encoder is macOS-only");
    std::process::exit(2);
}

fn drain_samples(
    encoder: &mut ResidentEncoder,
    want: usize,
    timeout: Duration,
) -> Vec<obscura_media::encoder::EncodedSample> {
    let mut samples = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    while samples.len() < want {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let Some(sample) = encoder.next_sample(remaining) else {
            break;
        };
        samples.push(sample);
    }
    samples
}

fn control_old_burst(
    width: u32,
    height: u32,
    settings: EncoderSettings,
    frames: &[Vec<u8>],
    count: usize,
) -> Vec<obscura_media::encoder::EncodedSample> {
    let mut encoder = ResidentEncoder::create(width, height, settings).expect("old control create");
    let mut pts = 0u64;
    for sequence in 0..count {
        encoder
            .submit(
                frame_info(width, height, sequence as u64 + 1),
                &frames[sequence % frames.len()],
                pts,
            )
            .expect("old control submit");
        pts += 33_333;
    }
    let samples = drain_samples(&mut encoder, count, Duration::from_secs(30));
    let _ = encoder.close();
    samples
}

fn control_after_apply(
    width: u32,
    height: u32,
    old: EncoderSettings,
    new: EncoderSettings,
    frames: &[Vec<u8>],
    count: usize,
) -> Vec<obscura_media::encoder::EncodedSample> {
    let mut encoder = ResidentEncoder::create(width, height, old).expect("new control create");
    let outcome = encoder.apply(1, new);
    assert!(matches!(outcome, ApplyOutcome::Applied { generation: 1, .. }), "{outcome:?}");
    let mut samples = Vec::new();
    let mut pts = 0u64;
    for sequence in 0..count {
        encoder
            .submit(
                frame_info(width, height, sequence as u64 + 1),
                &frames[sequence % frames.len()],
                pts,
            )
            .expect("new control submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(10)) {
            samples.push(sample);
        }
        pts += 33_333;
    }
    let _ = encoder.close();
    samples
}

fn red_legacy_limitation(_outdir: &Path, h: &mut Harness) {
    let width = 64u32;
    let height = 48u32;
    let runtime = Runtime::new().expect("legacy red runtime");
    let mut idr_only = true;
    let mut has_p = false;
    let mut succeeded = 0usize;
    let mut timed_out = 0usize;
    for sequence in 0..2u64 {
        let rgba = moving_scene(width, height, sequence);
        match runtime.block_on(obscura_media::encode(Path::new("ffmpeg"), width, height, rgba)) {
            Ok(encoded) => {
                let types = nal_types(&encoded);
                idr_only &= types.contains(&5) && !types.contains(&1);
                has_p |= types.contains(&1);
                succeeded += 1;
            }
            Err(error) => {
                // The old API hard-codes a five second per-frame deadline and
                // spawns a fresh encoder process every call; a startup miss is
                // itself part of the per-frame limitation.
                timed_out += 1;
                println!("LEGACY_CALL_ERROR {error}");
            }
        }
    }
    h.check(
        "legacy_no_resident_session",
        idr_only && !has_p,
        format!(
            "legacy encode() succeeded={succeeded} timed_out={timed_out}; each success is a lone IDR, no resident apply/keyframe API"
        ),
    );
}

fn resident_even(outdir: &Path, h: &mut Harness) {
    let width = 390u32;
    let height = 844u32;
    let mut encoder = ResidentEncoder::create(
        width,
        height,
        EncoderSettings { bitrate_bps: 1_200_000, fps: 30, keyframe_interval: 60 },
    )
    .expect("even create");
    let count = 6usize;
    for sequence in 0..count as u64 {
        let rgba = geometry_scene(width, height, sequence);
        let info = frame_info(width, height, sequence + 1);
        encoder
            .submit(info, &rgba, sequence * 33_333)
            .expect("even submit");
    }
    let samples = drain_samples(&mut encoder, count, Duration::from_secs(10));
    h.check("even_sample_count", samples.len() == count, format!("got {}", samples.len()));
    h.check(
        "even_coded_dims",
        encoder.coded_dimensions() == (width, height),
        format!("{:?}", encoder.coded_dimensions()),
    );
    let pts_increasing = samples.windows(2).all(|w| w[1].context.pts_us > w[0].context.pts_us);
    h.check("even_pts_increasing", pts_increasing, format!("{} samples", samples.len()));
    let first_sync = samples.first().map(|s| s.keyframe).unwrap_or(false);
    let first_has_idr = samples.first().map(|s| nal_types(&s.bytes).contains(&5)).unwrap_or(false);
    h.check("even_first_idr", first_sync && first_has_idr, "first sample sync");
    let has_p = samples.iter().any(|s| nal_types(&s.bytes).contains(&1));
    h.check("even_has_p", has_p, "at least one real P frame");
    if samples.len() == count {
        write_stream(&outdir.join("even.h264"), &samples).expect("write even.h264");
        // Black-box SPS check on the real bitstream: Constrained Baseline is
        // profile_idc 66 with constraint_set1_flag set; level is whatever the
        // SDK negotiated.
        match sps_profile(&concat_stream(&samples)) {
            Some((profile, constraints, level)) => h.check(
                "even_sps_constrained_baseline",
                profile == 66 && constraints & 0x40 != 0,
                format!("profile_idc={profile} constraint_flags=0x{constraints:02x} level_idc={level}"),
            ),
            None => h.check("even_sps_constrained_baseline", false, "no SPS NAL found"),
        }
        let manifest_path = outdir.join("even.manifest.json");
        let manifest = serde_json::to_string_pretty(&samples.iter().map(|s| {
            serde_json::json!({
                "sequence": s.context.source.sequence,
                "pts_us": s.context.pts_us,
                "generation": s.context.settings_generation,
                "keyframe": s.keyframe,
                "coded_width": s.coded_width,
                "coded_height": s.coded_height,
                "duration_us": s.duration_us,
                "bytes": s.bytes.len(),
            })
        }).collect::<Vec<_>>()).expect("manifest json");
        std::fs::write(manifest_path, manifest).expect("write even.manifest.json");
        match decode_frame(&outdir.join("even.h264"), 0, width, height) {
            Ok(frame) => {
                let corner = rgb_at(&frame, width, width - 1, height - 1);
                h.check(
                    "even_decode_bottom_right_blue",
                    corner.2 > 120 && corner.2 > corner.0,
                    format!("decoded bottom-right rgb {corner:?}"),
                );
                let middle = rgb_at(&frame, width, width - 4, height / 2);
                h.check(
                    "even_decode_last_col_red",
                    middle.0 > 120 && middle.0 > middle.1,
                    format!("decoded last valid col rgb {middle:?}"),
                );
            }
            Err(error) => h.check("even_decode", false, format!("decode failed: {error}")),
        }
    }
    let outcome = encoder.close();
    h.check(
        "even_close_quiescent",
        outcome.samples.len() + samples.len() == count,
        format!("close drained {}", outcome.samples.len()),
    );
}

/// Keep raw and in-flight work pending across a bitrate-only change. The old
/// generation must be encoded with the old bitrate before apply returns; only
/// subsequent frames may use the new bitrate.
fn resident_pending_apply_boundary(outdir: &Path, h: &mut Harness) {
    let width = 640u32;
    let height = 480u32;
    let old = EncoderSettings { bitrate_bps: 300_000, fps: 30, keyframe_interval: 300 };
    let new = EncoderSettings { bitrate_bps: 3_000_000, ..old };
    let pending = MAX_IN_FLIGHT + MAX_RAW_QUEUE;
    let new_count = 24usize;
    let frames = precomputed_noise_frames(width, height, 16);
    let mut encoder = ResidentEncoder::create(width, height, old).expect("pending apply create");
    let mut pts = 0u64;
    for sequence in 0..pending {
        encoder
            .submit(
                frame_info(width, height, sequence as u64 + 1),
                &frames[sequence % frames.len()],
                pts,
            )
            .expect("pending apply submit");
        pts += 33_333;
    }
    let outcome = encoder.apply(1, new);
    h.check(
        "pending_apply_ack",
        matches!(outcome, ApplyOutcome::Applied { generation: 1, .. }),
        format!("{outcome:?}"),
    );
    let old_samples = drain_samples(&mut encoder, pending, Duration::from_secs(30));
    let old_generation = old_samples.len() == pending
        && old_samples.iter().all(|s| s.context.settings_generation == 0);
    let old_keyframes = old_samples.iter().filter(|s| s.keyframe).count();
    h.check(
        "pending_apply_old_count",
        old_samples.len() == pending,
        format!("old samples={} pending={pending}", old_samples.len()),
    );
    h.check(
        "pending_apply_old_generation",
        old_generation,
        format!("old samples={} keyframes={old_keyframes}", old_samples.len()),
    );
    h.check(
        "pending_apply_old_gop_boundary",
        old_keyframes <= 1,
        format!("old samples={} keyframes={old_keyframes}", old_samples.len()),
    );

    let mut new_samples = Vec::new();
    for sequence in 0..new_count {
        encoder
            .submit(
                frame_info(width, height, pending as u64 + sequence as u64 + 1),
                &frames[sequence % frames.len()],
                pts,
            )
            .expect("pending apply new submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(10)) {
            new_samples.push(sample);
        }
        pts += 33_333;
    }
    let new_generation = new_samples.len() == new_count
        && new_samples.iter().all(|s| s.context.settings_generation == 1);
    let new_keyframes = new_samples.iter().filter(|s| s.keyframe).count();
    h.check(
        "pending_apply_new_count",
        new_samples.len() == new_count,
        format!("new samples={} expected={new_count}", new_samples.len()),
    );
    h.check(
        "pending_apply_new_generation",
        new_generation,
        format!("new samples={} keyframes={new_keyframes}", new_samples.len()),
    );
    h.check(
        "pending_apply_new_gop_boundary",
        !new_samples.is_empty() && new_keyframes <= 1,
        format!("new samples={} keyframes={new_keyframes}", new_samples.len()),
    );
    if old_samples.len() == pending {
        let path = outdir.join("pending-boundary-old.h264");
        write_stream(&path, &old_samples).expect("write pending old stream");
        match decode_all_frames(&path, width, height) {
            Ok(count) => h.check(
                "pending_apply_old_decode",
                count == old_samples.len(),
                format!("decoded {count} of {} AUs", old_samples.len()),
            ),
            Err(error) => h.check("pending_apply_old_decode", false, format!("decode failed: {error}")),
        }
    }
    if new_samples.len() == new_count {
        let path = outdir.join("pending-boundary-new.h264");
        write_stream(&path, &new_samples).expect("write pending new stream");
        match decode_all_frames(&path, width, height) {
            Ok(count) => h.check(
                "pending_apply_new_decode",
                count == new_samples.len(),
                format!("decoded {count} of {} AUs", new_samples.len()),
            ),
            Err(error) => h.check("pending_apply_new_decode", false, format!("decode failed: {error}")),
        }
    }
    let old_control = control_old_burst(width, height, old, &frames, pending);
    let new_control = control_after_apply(width, height, old, new, &frames, new_count);
    let old_matches_control = old_samples.len() == pending
        && old_control.len() == pending
        && old_samples
            .iter()
            .zip(&old_control)
            .all(|(actual, control)| actual.bytes.len() == control.bytes.len());
    let old_control_bytes: usize = old_control.iter().map(|s| s.bytes.len()).sum();
    let new_control_bytes: usize = new_control.iter().map(|s| s.bytes.len()).sum();
    h.check(
        "pending_apply_old_matches_control",
        old_matches_control,
        format!(
            "old_samples={} old_control={} actual_lengths={:?} control_lengths={:?}",
            old_samples.len(),
            old_control.len(),
            old_samples.iter().map(|s| s.bytes.len()).collect::<Vec<_>>(),
            old_control.iter().map(|s| s.bytes.len()).collect::<Vec<_>>()
        ),
    );
    let new_tail_avg = new_samples
        .iter()
        .skip(4)
        .map(|s| s.bytes.len())
        .sum::<usize>() as f64
        / new_samples.len().saturating_sub(4).max(1) as f64;
    let new_control_tail_avg = new_control
        .iter()
        .skip(8)
        .map(|s| s.bytes.len())
        .sum::<usize>() as f64
        / new_control.len().saturating_sub(8).max(1) as f64;
    let new_tail_ratio = new_tail_avg / new_control_tail_avg.max(1.0);
    h.check(
        "pending_apply_new_control_tail",
        new_samples.len() == new_count
            && new_control.len() == new_count
            && (new_tail_ratio - 1.0).abs() <= 0.15,
        format!(
            "new_samples={} new_control={} tail_avg={new_tail_avg:.1} control_tail_avg={new_control_tail_avg:.1} ratio={new_tail_ratio:.3}",
            new_samples.len(),
            new_control.len(),
        ),
    );
    h.check(
        "pending_apply_controls_differ",
        old_control_bytes != new_control_bytes,
        format!(
            "old_control_bytes={old_control_bytes} new_control_bytes={new_control_bytes}"
        ),
    );
    let close = encoder.close();
    h.check(
        "pending_apply_close",
        close.samples.is_empty() && close.cancelled_raw == 0,
        format!("close samples={} cancelled={}", close.samples.len(), close.cancelled_raw),
    );
}

/// Applied FPS and keyframe interval must be confirmed by the SDK and used by
/// subsequent samples. A failed apply must not report Applied.
fn resident_fps_keyframe_apply(outdir: &Path, h: &mut Harness) {
    let width = 320u32;
    let height = 240u32;
    let initial = EncoderSettings { bitrate_bps: 1_000_000, fps: 30, keyframe_interval: 30 };
    let requested = EncoderSettings { fps: 10, keyframe_interval: 1, ..initial };
    let mut encoder = ResidentEncoder::create(width, height, initial).expect("fps create");
    let mut baseline = Vec::new();
    for sequence in 0..6u64 {
        encoder
            .submit(
                frame_info(width, height, sequence + 1),
                &noise_scene(width, height, sequence),
                sequence * 33_333,
            )
            .expect("fps baseline submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(5)) {
            baseline.push(sample);
        }
    }
    let outcome = encoder.apply(1, requested);
    h.check(
        "fps_keyframe_apply_ack",
        matches!(outcome, ApplyOutcome::Applied { generation: 1, .. }),
        format!("{outcome:?}"),
    );
    let mut samples = Vec::new();
    for sequence in 0..6u64 {
        encoder
            .submit(
                frame_info(width, height, sequence + 100),
                &noise_scene(width, height, sequence),
                6 * 33_333 + sequence * 100_000,
            )
            .expect("fps submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(5)) {
            samples.push(sample);
        }
    }
    let duration_ok = !samples.is_empty()
        && samples.iter().all(|s| s.duration_us.abs_diff(100_000) <= 1_000);
    let baseline_duration_ok = baseline.len() == 6
        && baseline.iter().all(|s| s.duration_us.abs_diff(33_333) <= 1_500);
    let keyframes = samples.iter().filter(|s| s.keyframe).count();
    h.check(
        "fps_baseline_duration",
        baseline_duration_ok,
        format!(
            "baseline durations={:?}",
            baseline.iter().map(|s| s.duration_us).collect::<Vec<_>>()
        ),
    );
    h.check(
        "fps_effective_sample_duration",
        samples.len() == 6 && duration_ok,
        format!(
            "durations={:?}",
            samples.iter().map(|s| s.duration_us).collect::<Vec<_>>()
        ),
    );
    h.check(
        "keyframe_interval_effective",
        samples.len() == 6 && keyframes == samples.len(),
        format!("samples={} keyframes={keyframes}", samples.len()),
    );
    h.check(
        "fps_settings_readback",
        encoder.settings() == requested && encoder.generation() == 1,
        format!("settings={:?} generation={}", encoder.settings(), encoder.generation()),
    );
    h.check(
        "fps_baseline_present",
        baseline.len() == 6,
        format!("baseline={}", baseline.len()),
    );
    if !samples.is_empty() {
        let path = outdir.join("fps-keyframe.h264");
        write_stream(&path, &samples).expect("write fps stream");
        match decode_all_frames(&path, width, height) {
            Ok(count) => h.check(
                "fps_keyframe_decode",
                count == samples.len(),
                format!("decoded {count} of {} AUs", samples.len()),
            ),
            Err(error) => h.check("fps_keyframe_decode", false, format!("decode failed: {error}")),
        }
    }
    let bad_fps = encoder.apply(2, EncoderSettings { fps: 0, ..requested });
    h.check(
        "fps_invalid_not_applied",
        matches!(bad_fps, ApplyOutcome::Failed(_)),
        format!("{bad_fps:?}"),
    );
    let bad_interval = encoder.apply(3, EncoderSettings { keyframe_interval: 0, ..requested });
    h.check(
        "keyframe_invalid_not_applied",
        matches!(bad_interval, ApplyOutcome::Failed(_)),
        format!("{bad_interval:?}"),
    );
    let _ = encoder.close();
}

/// Fill the bounded callback channel without consuming it. Overflow must become
/// a typed terminal outcome, not a fabricated in-flight reservation.
fn resident_callback_saturation(outdir: &Path, h: &mut Harness) {
    let width = 320u32;
    let height = 240u32;
    let settings = EncoderSettings { bitrate_bps: 1_000_000, fps: 30, keyframe_interval: 300 };
    let mut encoder = ResidentEncoder::create(width, height, settings).expect("saturation create");
    let burst = MAX_IN_FLIGHT + MAX_COMPLETED + 16;
    let mut accepted = 0usize;
    let mut submit_errors = 0usize;
    for sequence in 0..burst as u64 {
        match encoder.submit(
            frame_info(width, height, sequence + 1),
            &noise_scene(width, height, sequence),
            sequence * 33_333,
        ) {
            Ok(()) => accepted += 1,
            Err(error) => {
                submit_errors += 1;
                println!("SATURATION_SUBMIT_ERROR {error}");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut drained = Vec::new();
    while let Some(sample) = encoder.next_sample(Duration::from_millis(200)) {
        drained.push(sample);
    }
    let terminal = encoder.is_terminated();
    let events = encoder.take_events();
    let typed_overflow = events.iter().any(|event| {
        matches!(
            event,
            EncoderEvent::SampleQueueOverflow { .. } | EncoderEvent::EncoderTerminated { .. }
        )
    });
    h.check(
        "saturation_typed_terminal",
        terminal && typed_overflow,
        format!("accepted={accepted} submit_errors={submit_errors} drained={} terminal={terminal} events={events:?}", drained.len()),
    );
    h.check(
        "saturation_stops_success",
        submit_errors > 0,
        format!("accepted={accepted} submit_errors={submit_errors}"),
    );
    h.check(
        "saturation_exceeds_capacity",
        accepted > MAX_IN_FLIGHT + MAX_COMPLETED,
        format!(
            "accepted={accepted} capacity={}",
            MAX_IN_FLIGHT + MAX_COMPLETED
        ),
    );
    let close = encoder.close();
    h.check(
        "saturation_close_returns",
        close.samples.len() + drained.len() <= accepted,
        format!(
            "accepted={accepted} drained={} close_samples={} events={:?}",
            drained.len(),
            close.samples.len(),
            close.events
        ),
    );
    if !drained.is_empty() {
        write_stream(&outdir.join("saturation-drained.h264"), &drained)
            .expect("write saturation stream");
    }
}

fn resident_odd(outdir: &Path, h: &mut Harness) {
    let width = 385u32;
    let height = 847u32;
    let coded = ((width + 1) & !1, (height + 1) & !1);
    let mut encoder = ResidentEncoder::create(
        width,
        height,
        EncoderSettings { bitrate_bps: 1_200_000, fps: 30, keyframe_interval: 60 },
    )
    .expect("odd create");
    let count = 6usize;
    for sequence in 0..count as u64 {
        let rgba = geometry_scene(width, height, sequence);
        let info = frame_info(width, height, sequence + 1);
        encoder
            .submit(info, &rgba, sequence * 33_333)
            .expect("odd submit");
    }
    let samples = drain_samples(&mut encoder, count, Duration::from_secs(10));
    h.check("odd_sample_count", samples.len() == count, format!("got {}", samples.len()));
    h.check(
        "odd_coded_dims",
        encoder.coded_dimensions() == coded,
        format!("{:?}", encoder.coded_dimensions()),
    );
    if samples.len() == count {
        write_stream(&outdir.join("odd.h264"), &samples).expect("write odd.h264");
        // The coded stream is ceil-even; the decoder returns the coded frame,
        // with source pixels in the top-left and white padding beyond.
        match decode_frame(&outdir.join("odd.h264"), 0, coded.0, coded.1) {
            Ok(frame) => {
                let corner = rgb_at(&frame, coded.0, width - 1, height - 1);
                h.check(
                    "odd_decode_bottom_right_blue",
                    corner.2 > 120 && corner.2 > corner.0,
                    format!("decoded bottom-right rgb {corner:?}"),
                );
                let middle = rgb_at(&frame, coded.0, width - 4, height / 2);
                h.check(
                    "odd_decode_last_col_red",
                    middle.0 > 120 && middle.0 > middle.1,
                    format!("decoded last valid col rgb {middle:?}"),
                );
                // Row 1 low byte is 1: its first 8px cell is black and its
                // second is white, which pins the source row order.
                let cell0 = rgb_at(&frame, coded.0, 9, 1);
                let cell1 = rgb_at(&frame, coded.0, 17, 1);
                h.check(
                    "odd_decode_row_marker",
                    cell0.0 < 130 && cell1.0 > 130,
                    format!("row1 cell0 {cell0:?} dark, cell1 {cell1:?} light"),
                );
                let padding_col = rgb_at(&frame, coded.0, coded.0 - 1, height / 2);
                h.check(
                    "odd_decode_white_padding_col",
                    padding_col.0 > 180 && padding_col.1 > 180 && padding_col.2 > 180,
                    format!("coded pad col rgb {padding_col:?}"),
                );
            }
            Err(error) => h.check("odd_decode", false, format!("decode failed: {error}")),
        }
    }
    let outcome = encoder.close();
    h.check(
        "odd_close_quiescent",
        outcome.samples.len() + samples.len() == count,
        format!("close drained {}", outcome.samples.len()),
    );
}

/// Encode one deterministic content window, draining one sample per submit.
fn quality_window_samples(
    encoder: &mut ResidentEncoder,
    width: u32,
    height: u32,
    window: usize,
    pts: &mut u64,
) -> Vec<obscura_media::encoder::EncodedSample> {
    let mut samples = Vec::with_capacity(window);
    for sequence in 0..window as u64 {
        encoder
            .submit(frame_info(width, height, sequence + 1), &noise_scene(width, height, sequence), *pts)
            .expect("quality submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(5)) {
            samples.push(sample);
        }
        *pts += 33_333;
    }
    samples
}

fn quality_tail_bytes(samples: &[obscura_media::encoder::EncodedSample], settle: usize) -> usize {
    samples[settle.min(samples.len())..].iter().map(|s| s.bytes.len()).sum()
}

/// Fresh session, high tier first then low, identical content both windows. If
/// the tier drives the bytes (and not warmup/order), high must still exceed low.
fn quality_measure_reversed(
    width: u32,
    height: u32,
    high: EncoderSettings,
    low: EncoderSettings,
    window: usize,
    settle: usize,
) -> (usize, usize) {
    let mut encoder = ResidentEncoder::create(width, height, high).expect("reverse quality create");
    let mut pts = 0u64;
    let high_samples = quality_window_samples(&mut encoder, width, height, window, &mut pts);
    let outcome = encoder.apply(1, low);
    assert!(matches!(outcome, ApplyOutcome::Applied { generation: 1, .. }), "{outcome:?}");
    let low_samples = quality_window_samples(&mut encoder, width, height, window, &mut pts);
    let _ = encoder.close();
    (quality_tail_bytes(&high_samples, settle), quality_tail_bytes(&low_samples, settle))
}

fn resident_quality_apply(outdir: &Path, h: &mut Harness) {
    let width = 320u32;
    let height = 240u32;
    let low = EncoderSettings { bitrate_bps: 300_000, fps: 30, keyframe_interval: 300 };
    let high = EncoderSettings { bitrate_bps: 3_000_000, ..low };
    let mut encoder = ResidentEncoder::create(width, height, low).expect("quality create");
    let window = 300usize;
    let settle = 150usize;
    let mut pts = 0u64;
    let mut low_samples = Vec::with_capacity(window);
    for sequence in 0..window as u64 {
        let rgba = noise_scene(width, height, sequence);
        encoder
            .submit(frame_info(width, height, sequence + 1), &rgba, pts)
            .expect("quality submit");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(5)) {
            low_samples.push(sample);
        }
        pts += 33_333;
    }
    let low_tail: usize = low_samples[settle.min(low_samples.len())..]
        .iter()
        .map(|s| s.bytes.len())
        .sum();

    let outcome = encoder.apply(1, high);
    let ack = matches!(&outcome, ApplyOutcome::Applied { generation: 1, .. });
    h.check(
        "quality_apply_ack",
        ack,
        format!("{:?}", outcome),
    );
    h.check(
        "quality_generation_advanced",
        encoder.generation() == 1 && encoder.settings() == high,
        format!("generation={}", encoder.generation()),
    );

    let mut high_samples = Vec::with_capacity(window);
    // Same deterministic content as the low window so the only variable is the
    // applied AverageBitRate, not the scene.
    for sequence in 0..window as u64 {
        let rgba = noise_scene(width, height, sequence);
        encoder
            .submit(frame_info(width, height, sequence + 1), &rgba, pts)
            .expect("quality submit high");
        if let Some(sample) = encoder.next_sample(Duration::from_secs(5)) {
            high_samples.push(sample);
        }
        pts += 33_333;
    }
    let high_tail: usize = high_samples[settle.min(high_samples.len())..]
        .iter()
        .map(|s| s.bytes.len())
        .sum();
    let high_generation = high_samples
        .iter()
        .all(|s| s.context.settings_generation == 1);
    h.check(
        "quality_high_generation_bound",
        high_generation,
        "post-apply samples carry generation 1",
    );

    // Forced IDR on the live same-sized incarnation.
    encoder.request_keyframe().expect("request keyframe");
    let forced = noise_scene(width, height, 999);
    encoder
        .submit(frame_info(width, height, 999), &forced, pts)
        .expect("forced idr submit");
    let forced_sample = drain_samples(&mut encoder, 1, Duration::from_secs(10));
    let forced_ok = forced_sample
        .first()
        .map(|s| s.keyframe && nal_types(&s.bytes).contains(&5))
        .unwrap_or(false);
    h.check("quality_forced_idr", forced_ok, "real sync sample after request_keyframe");

    // Reverse-order session: high then low, same content. Rules out warmup or
    // window-order as the cause of the byte difference.
    let (rev_high_tail, rev_low_tail) = quality_measure_reversed(width, height, high, low, window, settle);
    let rev_ratio = rev_low_tail as f64 / rev_high_tail.max(1) as f64;

    let bytes_report = outdir.join("quality-bytes.txt");
    let low_all: usize = low_samples.iter().map(|s| s.bytes.len()).sum();
    let high_all: usize = high_samples.iter().map(|s| s.bytes.len()).sum();
    let ratio = high_tail as f64 / low_tail.max(1) as f64;
    std::fs::write(
        &bytes_report,
        format!(
            "low_all_bytes={low_all} low_samples={} low_tail_bytes={low_tail}\nhigh_all_bytes={high_all} high_samples={} high_tail_bytes={high_tail}\ntail_ratio={ratio:.3} window={window} settle={settle}\nreversed_high_first_tail_bytes={rev_high_tail} reversed_low_second_tail_bytes={rev_low_tail} reversed_ratio={rev_ratio:.3}\n",
            low_samples.len(),
            high_samples.len()
        ),
    )
    .expect("write quality bytes");
    h.check(
        "quality_windows_complete",
        low_samples.len() == window && high_samples.len() == window,
        format!("low_samples={} high_samples={}", low_samples.len(), high_samples.len()),
    );
    // Real encoded bytes on identical content. Assert only the direction a
    // higher AverageBitRate must produce; no invented magnitude threshold.
    h.check(
        "quality_high_exceeds_low_bytes",
        low_samples.len() == window && high_samples.len() == window && high_tail > low_tail,
        format!(
            "low_tail={low_tail} high_tail={high_tail} ratio={ratio:.3} low_all={low_all} high_all={high_all}"
        ),
    );
    h.check(
        "quality_high_exceeds_low_bytes_reversed_order",
        rev_high_tail > rev_low_tail,
        format!("high_first={rev_high_tail} low_second={rev_low_tail} ratio={rev_ratio:.3}"),
    );

    let outcome = encoder.close();
    h.check(
        "quality_close_clean",
        outcome.samples.is_empty() && outcome.cancelled_raw == 0,
        format!("drained={} cancelled_raw={} events={}", outcome.samples.len(), outcome.cancelled_raw, outcome.events.len()),
    );
}

/// Premultiplied RGBA must be white-composited per source row before YUV420. A
/// 50%-alpha premultiplied gray has to decode to the defined white blend, not
/// the raw premultiplied value and not a black-background composite.
fn resident_alpha_color(outdir: &Path, h: &mut Harness) {
    let width = 64u32;
    let height = 48u32;
    let mut encoder = ResidentEncoder::create(
        width,
        height,
        EncoderSettings { bitrate_bps: 1_200_000, fps: 30, keyframe_interval: 60 },
    )
    .expect("alpha create");
    // Premultiplied 50%-alpha gray (64,64,64,128); white compositing yields
    // 64 + (255 - 128) = 191 on every channel.
    let mut rgba = vec![255u8; (width * height * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[0] = 64;
        pixel[1] = 64;
        pixel[2] = 64;
        pixel[3] = 128;
    }
    encoder
        .submit(frame_info(width, height, 1), &rgba, 0)
        .expect("alpha submit");
    let samples = drain_samples(&mut encoder, 1, Duration::from_secs(10));
    h.check("alpha_sample", samples.len() == 1, format!("got {}", samples.len()));
    if !samples.is_empty() {
        write_stream(&outdir.join("alpha.h264"), &samples).expect("write alpha.h264");
        match decode_frame(&outdir.join("alpha.h264"), 0, width, height) {
            Ok(frame) => {
                let (r, g, b) = rgb_at(&frame, width, width / 2, height / 2);
                let grayish = (i32::from(r) - i32::from(g)).abs() <= 12
                    && (i32::from(g) - i32::from(b)).abs() <= 12;
                let white_composited = r > 160 && r < 220;
                h.check(
                    "alpha_premultiplied_white_composite",
                    grayish && white_composited,
                    format!("decoded premultiplied gray rgb ({r},{g},{b}), expected ~191 gray"),
                );
            }
            Err(error) => h.check("alpha_decode", false, format!("decode failed: {error}")),
        }
    }
    let _ = encoder.close();
}

/// Bounded raw queue drop plus a real forced IDR that restarts a continuous
/// reference chain on the live incarnation.
fn resident_dependency_recovery(outdir: &Path, h: &mut Harness) {
    let width = 1280u32;
    let height = 720u32;
    let settings = EncoderSettings { bitrate_bps: 1_000_000, fps: 30, keyframe_interval: 300 };
    let frames = precomputed_noise_frames(width, height, 16);
    let mut encoder = ResidentEncoder::create(width, height, settings).expect("recovery create");
    // Burst far more frames than the bounded raw queue while occupying the
    // in-flight slots, so latest-wins drops are forced before encoding.
    let burst = 64u64;
    let mut pts = 0u64;
    let mut submitted = 0u64;
    let mut events = Vec::new();
    let mut submit_error = None;
    while submitted < burst {
        let frame = &frames[(submitted as usize) % frames.len()];
        match encoder.submit(
            frame_info(width, height, submitted + 1),
            frame,
            pts,
        ) {
            Ok(()) => {
                submitted += 1;
                pts += 33_333;
            }
            Err(error) => {
                println!("RECOVERY_SUBMIT_ERROR {error}");
                submit_error = Some(error.to_string());
                break;
            }
        }
        events.extend(encoder.take_events());
    }
    events.extend(encoder.take_events());
    let drops: usize = events
        .iter()
        .filter_map(|event| match event {
            EncoderEvent::RawLatestWins { dropped } => Some(*dropped),
            _ => None,
        })
        .sum();
    // Drain the surviving pre-encode chain to quiescence.
    let expected = (submitted as usize).saturating_sub(drops);
    let mut samples = drain_samples(&mut encoder, expected, Duration::from_secs(60));
    let pre_forced_count = samples.len();
    // A real forced IDR fences the drop and restarts a continuous reference chain.
    let mut forced_ok = false;
    let forced_pts = pts;
    match encoder.request_keyframe() {
        Ok(()) => match encoder.submit(
            frame_info(width, height, submitted + 1),
            &frames[0],
            forced_pts,
        ) {
            Ok(()) => {
                if let Some(sample) = encoder.next_sample(Duration::from_secs(10)) {
                    forced_ok = sample.keyframe && nal_types(&sample.bytes).contains(&5);
                    samples.push(sample);
                }
            }
            Err(error) => println!("RECOVERY_FORCED_SUBMIT_ERROR {error}"),
        },
        Err(error) => println!("RECOVERY_REQUEST_KEYFRAME_ERROR {error}"),
    }
    let outcome = encoder.close();
    let close_samples = outcome.samples.len();
    let close_cancelled = outcome.cancelled_raw;
    samples.extend(outcome.samples);
    events.extend(outcome.events);
    let drops: usize = events
        .iter()
        .filter_map(|e| match e {
            EncoderEvent::RawLatestWins { dropped } => Some(*dropped),
            _ => None,
        })
        .sum();
    let terminal = events
        .iter()
        .any(|event| matches!(event, EncoderEvent::EncoderTerminated { .. }));
    h.check(
        "recovery_pre_forced_drain_complete",
        pre_forced_count == expected,
        format!("drained={pre_forced_count} expected={expected} drops={drops} submitted={submitted}"),
    );
    h.check(
        "recovery_raw_latest_wins_dropped",
        drops >= 1,
        format!("drops={drops} produced={} submitted={}", samples.len(), submitted + 1),
    );
    h.check(
        "recovery_bounded_produced_less_than_submitted",
        samples.len() < (submitted as usize + 1),
        format!("produced={} submitted={}", samples.len(), submitted + 1),
    );
    h.check(
        "recovery_no_terminal",
        !terminal && submit_error.is_none(),
        format!("terminal={terminal} submit_error={submit_error:?}"),
    );
    h.check(
        "recovery_forced_idr_after_drop",
        forced_ok,
        format!("forced keyframe={forced_ok}"),
    );
    h.check(
        "recovery_close_clean",
        close_samples == 0 && close_cancelled == 0,
        format!(
            "close samples={} cancelled={}",
            close_samples, close_cancelled
        ),
    );
    if !samples.is_empty() {
        write_stream(&outdir.join("recovery.h264"), &samples).expect("write recovery.h264");
        match decode_all_frames(&outdir.join("recovery.h264"), width, height) {
            Ok(count) => h.check(
                "recovery_chain_decodes",
                count == samples.len(),
                format!("decoded {count} of {} AUs", samples.len()),
            ),
            Err(error) => h.check("recovery_chain_decodes", false, format!("decode failed: {error}")),
        }
    }
}

/// Close stops input, cancels unsubmitted raw frames, drains in-flight callbacks
/// to quiescence, and rejects further input without a late callback resurrecting
/// state.
fn resident_close_lifecycle(outdir: &Path, h: &mut Harness) {
    let _ = std::fs::create_dir_all(outdir);
    let width = 320u32;
    let height = 240u32;
    let settings = EncoderSettings { bitrate_bps: 1_000_000, fps: 30, keyframe_interval: 300 };
    let submitted = 6u64;
    let mut encoder = ResidentEncoder::create(width, height, settings).expect("close create");
    for sequence in 0..submitted {
        encoder
            .submit(frame_info(width, height, sequence + 1), &moving_scene(width, height, sequence), sequence * 33_333)
            .expect("close submit");
    }
    let outcome = encoder.close();
    h.check(
        "close_cancel_drain_conserved",
        outcome.samples.len() + outcome.cancelled_raw == submitted as usize,
        format!("drained={} cancelled_raw={}", outcome.samples.len(), outcome.cancelled_raw),
    );
    let quiescent = !outcome.events.iter().any(|e| {
        matches!(e, EncoderEvent::DependencyBreak { .. } | EncoderEvent::EncoderTerminated { .. })
    });
    h.check("close_callback_quiescent", quiescent, format!("events={:?}", outcome.events));
    let rejected = encoder
        .submit(frame_info(width, height, 99), &moving_scene(width, height, 9), 0)
        .is_err()
        && matches!(encoder.apply(2, settings), ApplyOutcome::Rejected(_));
    h.check("close_rejects_new_input", rejected, "submit/apply rejected after close");
    // Callback lifetime: repeated mid-flight close must not crash or leave a
    // late callback holding a freed refcon.
    let mut cycles = 0usize;
    for _ in 0..8 {
        let mut e = ResidentEncoder::create(width, height, settings).expect("cycle create");
        for sequence in 0..submitted {
            e.submit(frame_info(width, height, sequence + 1), &moving_scene(width, height, sequence), sequence * 33_333)
                .expect("cycle submit");
        }
        let o = e.close();
        if o.samples.len() + o.cancelled_raw != submitted as usize {
            break;
        }
        cycles += 1;
    }
    h.check("close_repeated_no_crash", cycles == 8, format!("completed {cycles} close cycles"));
}

fn resident_rejections(outdir: &Path, h: &mut Harness) {
    let width = 128u32;
    let height = 96u32;
    let settings = EncoderSettings { bitrate_bps: 800_000, fps: 30, keyframe_interval: 60 };
    let mut encoder = ResidentEncoder::create(width, height, settings).expect("reject create");

    let bad_length = vec![0u8; (width * height * 4) as usize - 4];
    let bad_length_err = encoder.submit(frame_info(width, height, 1), &bad_length, 0).is_err();
    h.check("reject_bad_length", bad_length_err, "short buffer rejected");

    let good = moving_scene(width, height, 1);
    let mut wrong_stride = frame_info(width, height, 2);
    wrong_stride.stride = width * 3;
    let bad_stride = encoder.submit(wrong_stride, &good, 0).is_err();
    h.check("reject_bad_stride", bad_stride, "non width*4 stride rejected");

    let mut other_size = frame_info(width + 8, height, 3);
    other_size.byte_length = u64::from(width + 8) * u64::from(height) * 4;
    let size_err = matches!(
        encoder.submit(other_size, &vec![0u8; ((width + 8) * height * 4) as usize], 0),
        Err(obscura_media::encoder::EncoderError::SizeChangeRequiresNewIncarnation)
    );
    h.check("reject_size_change", size_err, "size change requires new incarnation");

    let stale = encoder.apply(0, settings);
    h.check(
        "reject_stale_generation",
        matches!(stale, ApplyOutcome::Rejected(_)),
        format!("{stale:?}"),
    );

    let bad_settings = encoder.apply(5, EncoderSettings { bitrate_bps: 0, ..settings });
    h.check(
        "reject_invalid_settings",
        matches!(bad_settings, ApplyOutcome::Failed(_)),
        format!("{bad_settings:?}"),
    );

    let create_err = ResidentEncoder::create(0, 0, settings).is_err();
    h.check("reject_create_zero_dims", create_err, "zero-dimension create rejected");

    let _ = std::fs::create_dir_all(outdir);
    let outcome = encoder.close();
    h.check(
        "reject_close",
        outcome.samples.is_empty(),
        format!("cancelled_raw={} events={}", outcome.cancelled_raw, outcome.events.len()),
    );
}
