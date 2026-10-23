//! Apple VideoToolbox resident H.264 encoder binding.
//!
//! One `VtSession` owns one `VTCompressionSession` (a real resident encoder
//! incarnation). Premultiplied RGBA is white-composited per source row into a
//! BGRA `CVPixelBuffer` whose stride comes from the real API, then submitted.
//! Samples are delivered on VideoToolbox's callback thread; each frame's refcon
//! owns the immutable submission context, so a callback can never read freed
//! state. This module owns no browser or control state.

use std::os::raw::{c_char, c_void};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;

use crate::encoder::{CallbackEvent, EncodedSample, SubmissionContext};

pub type OsStatus = i32;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFArrayRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CVPixelBufferRef = *mut c_void;
pub type CMSampleBufferRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMFormatDescriptionRef = *mut c_void;
pub type VTCompressionSessionRef = *mut c_void;

const K_CF_NUMBER_SINT32_TYPE: i32 = 3;
const K_CF_NUMBER_INT_TYPE: i32 = 9;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CV_PIXEL_FORMAT_TYPE_32BGRA: u32 = 0x4247_5241;
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x6176_6331;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

type VTCompressionOutputCallback = extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OsStatus,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

#[allow(non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(allocator: CFTypeRef, the_type: i32, value_ptr: *const c_void) -> CFNumberRef;
    fn CFNumberGetValue(number: CFNumberRef, the_type: i32, value_ptr: *mut c_void) -> u8;
    fn CFDictionaryCreateMutable(
        allocator: CFTypeRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFDictionaryContainsKey(dict: CFDictionaryRef, key: *const c_void) -> u8;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> *const c_void;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferCreate(
        allocator: CFTypeRef,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMTimeMake(value: i64, timescale: i32) -> CMTime;
    fn CMSampleBufferGetPresentationTimeStamp(sample_buffer: CMSampleBufferRef) -> CMTime;
    fn CMSampleBufferGetDuration(sample_buffer: CMSampleBufferRef) -> CMTime;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample_buffer: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> CFArrayRef;
    fn CMSampleBufferGetFormatDescription(sample_buffer: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetDataBuffer(sample_buffer: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMBlockBufferGetDataLength(block_buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        block_buffer: CMBlockBufferRef,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut u8,
    ) -> OsStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format_description: CMFormatDescriptionRef,
        parameter_set_index: usize,
        parameter_set_pointer_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> OsStatus;
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    static kCMTimeInvalid: CMTime;
}

#[allow(non_upper_case_globals)]
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTCompressionSessionCreate(
        allocator: CFTypeRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFTypeRef,
        output_callback: VTCompressionOutputCallback,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OsStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OsStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OsStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OsStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    fn VTSessionSetProperty(
        session: VTCompressionSessionRef,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OsStatus;
    fn VTSessionCopyProperty(
        session: VTCompressionSessionRef,
        property_key: CFStringRef,
        allocator: CFTypeRef,
        property_value_out: *mut CFTypeRef,
    ) -> OsStatus;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
}

/// Typed SDK failure: the OSStatus plus the operation that produced it.
#[derive(Debug, Clone)]
pub struct VtError {
    pub status: OsStatus,
    pub context: String,
}

impl VtError {
    fn new(status: OsStatus, context: impl Into<String>) -> Self {
        Self { status, context: context.into() }
    }
}

impl std::fmt::Display for VtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VideoToolbox {} failed (status {})", self.context, self.status)
    }
}

impl std::error::Error for VtError {}

fn dict_callbacks() -> (*const c_void, *const c_void) {
    unsafe {
        (
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        )
    }
}

unsafe fn cfnumber_i32(value: i32) -> CFNumberRef {
    CFNumberCreate(ptr::null(), K_CF_NUMBER_SINT32_TYPE, &value as *const i32 as *const c_void)
}

/// Owns one real `CVPixelBuffer`; released on drop.
pub struct PixelBuffer(CVPixelBufferRef);

impl PixelBuffer {
    fn as_ref(&self) -> CVPixelBufferRef {
        self.0
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

/// White-composite premultiplied RGBA into a real BGRA `CVPixelBuffer` sized to
/// the ceil-even coded dimensions. Source rows are written at the source stride
/// into the coded stride reported by the SDK; padding stays white.
pub fn compose_pixel_buffer(
    src_width: u32,
    src_height: u32,
    src_stride: u32,
    rgba: &[u8],
) -> Result<PixelBuffer, VtError> {
    let coded_width = (src_width + 1) & !1;
    let coded_height = (src_height + 1) & !1;
    let (key_cb, value_cb) = dict_callbacks();
    let attrs = unsafe { CFDictionaryCreateMutable(ptr::null(), 0, key_cb, value_cb) };
    if attrs.is_null() {
        return Err(VtError::new(-1, "pixel buffer attributes"));
    }
    unsafe {
        let number = cfnumber_i32(K_CV_PIXEL_FORMAT_TYPE_32BGRA as i32);
        CFDictionarySetValue(attrs, kCVPixelBufferPixelFormatTypeKey, number);
        CFRelease(number);
        let io = CFDictionaryCreate(ptr::null(), ptr::null(), ptr::null(), 0, key_cb, value_cb);
        CFDictionarySetValue(attrs, kCVPixelBufferIOSurfacePropertiesKey, io);
        CFRelease(io);
    }
    let mut raw: CVPixelBufferRef = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            ptr::null(),
            coded_width as usize,
            coded_height as usize,
            K_CV_PIXEL_FORMAT_TYPE_32BGRA,
            attrs,
            &mut raw,
        )
    };
    unsafe { CFRelease(attrs) };
    if status != 0 || raw.is_null() {
        return Err(VtError::new(status, "CVPixelBufferCreate"));
    }
    let buffer = PixelBuffer(raw);
    let lock = unsafe { CVPixelBufferLockBaseAddress(buffer.as_ref(), 0) };
    if lock != 0 {
        return Err(VtError::new(lock, "CVPixelBufferLockBaseAddress"));
    }
    let base = unsafe { CVPixelBufferGetBaseAddress(buffer.as_ref()) };
    let stride = unsafe { CVPixelBufferGetBytesPerRow(buffer.as_ref()) };
    let pool_width = unsafe { CVPixelBufferGetWidth(buffer.as_ref()) } as u32;
    let pool_height = unsafe { CVPixelBufferGetHeight(buffer.as_ref()) } as u32;
    if base.is_null() || pool_width < coded_width || pool_height < coded_height {
        unsafe { CVPixelBufferUnlockBaseAddress(buffer.as_ref(), 0) };
        return Err(VtError::new(-1, "CVPixelBuffer geometry"));
    }
    unsafe {
        for row in 0..coded_height as usize {
            ptr::write_bytes(base.add(row * stride), 0xFFu8, coded_width as usize * 4);
        }
        for y in 0..src_height as usize {
            let src_row = &rgba[y * src_stride as usize
                ..y * src_stride as usize + src_width as usize * 4];
            let dst_row = base.add(y * stride);
            for x in 0..src_width as usize {
                let alpha = src_row[x * 4 + 3];
                let background = 255 - alpha;
                *dst_row.add(x * 4) = src_row[x * 4 + 2].saturating_add(background);
                *dst_row.add(x * 4 + 1) = src_row[x * 4 + 1].saturating_add(background);
                *dst_row.add(x * 4 + 2) = src_row[x * 4].saturating_add(background);
                *dst_row.add(x * 4 + 3) = 255;
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer.as_ref(), 0);
    }
    Ok(buffer)
}

fn annex_b_from_sample(
    format: CMFormatDescriptionRef,
    sample: CMSampleBufferRef,
    is_sync: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    if is_sync && !format.is_null() {
        let mut index = 0usize;
        loop {
            let mut pointer: *const u8 = ptr::null();
            let mut size = 0usize;
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format,
                    index,
                    &mut pointer,
                    &mut size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != 0 || pointer.is_null() || size == 0 {
                break;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(unsafe { slice::from_raw_parts(pointer, size) });
            index += 1;
        }
    }
    let block = unsafe { CMSampleBufferGetDataBuffer(sample) };
    if block.is_null() {
        return out;
    }
    let total = unsafe { CMBlockBufferGetDataLength(block) };
    let mut tmp = vec![0u8; total];
    if unsafe { CMBlockBufferCopyDataBytes(block, 0, total, tmp.as_mut_ptr()) } != 0 {
        return out;
    }
    let mut offset = 0usize;
    while offset + 4 <= tmp.len() {
        let len = u32::from_be_bytes([tmp[offset], tmp[offset + 1], tmp[offset + 2], tmp[offset + 3]])
            as usize;
        offset += 4;
        if len == 0 || offset + len > tmp.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&tmp[offset..offset + len]);
        offset += len;
    }
    out
}

fn sample_is_sync(sample: CMSampleBufferRef) -> bool {
    let attachments = unsafe { CMSampleBufferGetSampleAttachmentsArray(sample, 0) };
    if attachments.is_null() || unsafe { CFArrayGetCount(attachments) } == 0 {
        return true;
    }
    let dict = unsafe { CFArrayGetValueAtIndex(attachments, 0) };
    if dict.is_null() || unsafe { CFDictionaryContainsKey(dict, kCMSampleAttachmentKey_NotSync) } == 0
    {
        return true;
    }
    let value = unsafe { CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync) };
    value != unsafe { kCFBooleanTrue }
}

fn cmtime_to_us(time: CMTime) -> Option<u64> {
    if time.timescale <= 0 {
        return None;
    }
    let value = i128::from(time.value) * 1_000_000i128 / i128::from(time.timescale);
    u64::try_from(value).ok()
}

/// Shared callback context. Lives as long as the VT session; the callback never
/// blocks and never touches the owner's queues directly.
pub struct CallbackCtx {
    tx: SyncSender<CallbackEvent>,
    live: AtomicUsize,
    overflowed: AtomicUsize,
    coded_width: u32,
    coded_height: u32,
}

extern "C" fn output_callback(
    ref_con: *mut c_void,
    source_ref_con: *mut c_void,
    status: OsStatus,
    _info_flags: u32,
    sample: CMSampleBufferRef,
) {
    let ctx = unsafe { &*(ref_con as *const CallbackCtx) };
    ctx.live.fetch_sub(1, Ordering::AcqRel);
    let submission: SubmissionContext =
        unsafe { *Box::from_raw(source_ref_con as *mut SubmissionContext) };
    let event = if status == 0 && !sample.is_null() {
        let sync = sample_is_sync(sample);
        let format = unsafe { CMSampleBufferGetFormatDescription(sample) };
        let bytes = annex_b_from_sample(format, sample, sync);
        let pts = unsafe { CMSampleBufferGetPresentationTimeStamp(sample) };
        let vt_pts_us = cmtime_to_us(pts).map(|v| v as i64).unwrap_or(submission.pts_us as i64);
        let duration = unsafe { CMSampleBufferGetDuration(sample) };
        CallbackEvent::Sample(EncodedSample {
            context: submission,
            keyframe: sync,
            coded_width: ctx.coded_width,
            coded_height: ctx.coded_height,
            vt_pts_us,
            duration_us: cmtime_to_us(duration).unwrap_or(0),
            bytes,
        })
    } else {
        CallbackEvent::Failed { context: submission, status }
    };
    if ctx.tx.try_send(event).is_err() {
        // The actor observes this separate signal and terminates the
        // incarnation. Never fabricate an in-flight callback reservation.
        ctx.overflowed.fetch_add(1, Ordering::AcqRel);
    }
}

/// A resident VideoToolbox H.264 compression session (one encoder incarnation).
pub struct VtSession {
    session: VTCompressionSessionRef,
    coded_width: u32,
    coded_height: u32,
    fps: AtomicU32,
    ctx: Box<CallbackCtx>,
}

impl VtSession {
    pub fn create(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        keyframe_interval: u32,
        tx: SyncSender<CallbackEvent>,
    ) -> Result<Self, VtError> {
        let coded_width = (width + 1) & !1;
        let coded_height = (height + 1) & !1;
        let ctx = Box::new(CallbackCtx {
            tx,
            live: AtomicUsize::new(0),
            overflowed: AtomicUsize::new(0),
            coded_width,
            coded_height,
        });
        let mut session: VTCompressionSessionRef = ptr::null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                coded_width as i32,
                coded_height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                output_callback,
                &*ctx as *const CallbackCtx as *mut c_void,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            return Err(VtError::new(status, "VTCompressionSessionCreate"));
        }
        let this = Self { session, coded_width, coded_height, fps: AtomicU32::new(fps), ctx };
        this.configure(bitrate_bps, keyframe_interval)?;
        Ok(this)
    }

    fn set_bool(&self, key: CFStringRef, value: CFBooleanRef) -> Result<(), VtError> {
        let status = unsafe { VTSessionSetProperty(self.session, key, value) };
        if status == 0 {
            Ok(())
        } else {
            Err(VtError::new(status, "VTSessionSetProperty(bool)"))
        }
    }

    fn set_i32(&self, key: CFStringRef, value: i32) -> Result<(), VtError> {
        let number = unsafe { cfnumber_i32(value) };
        let status = unsafe { VTSessionSetProperty(self.session, key, number) };
        unsafe { CFRelease(number) };
        if status == 0 {
            Ok(())
        } else {
            Err(VtError::new(status, "VTSessionSetProperty(i32)"))
        }
    }

    fn read_i32(&self, key: CFStringRef) -> Result<i32, VtError> {
        let mut value: CFTypeRef = ptr::null();
        let status = unsafe { VTSessionCopyProperty(self.session, key, ptr::null(), &mut value) };
        if status != 0 || value.is_null() {
            return Err(VtError::new(status, "VTSessionCopyProperty"));
        }
        let mut out: i32 = 0;
        let ok = unsafe {
            CFNumberGetValue(value, K_CF_NUMBER_SINT32_TYPE, &mut out as *mut i32 as *mut c_void)
        };
        unsafe { CFRelease(value) };
        if ok == 0 {
            Err(VtError::new(-1, "CFNumberGetValue"))
        } else {
            Ok(out)
        }
    }

    fn configure(&self, bitrate_bps: u32, keyframe_interval: u32) -> Result<(), VtError> {
        self.set_bool(unsafe { kVTCompressionPropertyKey_RealTime }, unsafe { kCFBooleanTrue })?;
        self.set_bool(
            unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
            unsafe { kCFBooleanFalse },
        )?;
        let fps = self.fps.load(Ordering::Acquire);
        self.set_i32(unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, fps as i32)?;
        let fps_readback = self.read_i32(unsafe { kVTCompressionPropertyKey_ExpectedFrameRate })?;
        if fps_readback != fps as i32 {
            return Err(VtError::new(
                -1,
                format!("ExpectedFrameRate readback {fps_readback} != requested {fps}"),
            ));
        }
        self.set_i32(
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
            keyframe_interval as i32,
        )?;
        let interval_readback =
            self.read_i32(unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval })?;
        if interval_readback != keyframe_interval as i32 {
            return Err(VtError::new(
                -1,
                format!(
                    "MaxKeyFrameInterval readback {interval_readback} != requested {keyframe_interval}"
                ),
            ));
        }
        self.set_i32(unsafe { kVTCompressionPropertyKey_AverageBitRate }, bitrate_bps as i32)?;
        let bitrate_readback =
            self.read_i32(unsafe { kVTCompressionPropertyKey_AverageBitRate })?;
        if bitrate_readback != bitrate_bps as i32 {
            return Err(VtError::new(
                -1,
                format!("AverageBitRate readback {bitrate_readback} != requested {bitrate_bps}"),
            ));
        }
        let status = unsafe {
            VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
            )
        };
        if status != 0 {
            return Err(VtError::new(status, "ProfileLevel"));
        }
        for (key, value) in [
            (
                unsafe { kVTCompressionPropertyKey_ColorPrimaries },
                unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 },
            ),
            (
                unsafe { kVTCompressionPropertyKey_TransferFunction },
                unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 },
            ),
            (
                unsafe { kVTCompressionPropertyKey_YCbCrMatrix },
                unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 },
            ),
        ] {
            let status = unsafe { VTSessionSetProperty(self.session, key, value) };
            if status != 0 {
                return Err(VtError::new(status, "colour property"));
            }
        }
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(self.session) };
        if status != 0 {
            return Err(VtError::new(status, "VTCompressionSessionPrepareToEncodeFrames"));
        }
        Ok(())
    }

    /// Apply a new average bitrate to the live session and confirm it with a
    /// real SDK readback. Only a confirmed readback counts as applied.
    pub fn apply_bitrate(&self, bitrate_bps: u32) -> Result<(), VtError> {
        self.set_i32(unsafe { kVTCompressionPropertyKey_AverageBitRate }, bitrate_bps as i32)?;
        let readback = self.read_i32(unsafe { kVTCompressionPropertyKey_AverageBitRate })?;
        if readback == bitrate_bps as i32 {
            Ok(())
        } else {
            Err(VtError::new(
                -1,
                format!("AverageBitRate readback {readback} != requested {}", bitrate_bps),
            ))
        }
    }

    pub fn apply_fps(&self, fps: u32) -> Result<(), VtError> {
        self.set_i32(unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, fps as i32)?;
        let readback = self.read_i32(unsafe { kVTCompressionPropertyKey_ExpectedFrameRate })?;
        if readback != fps as i32 {
            return Err(VtError::new(
                -1,
                format!("ExpectedFrameRate readback {readback} != requested {fps}"),
            ));
        }
        self.fps.store(fps, Ordering::Release);
        Ok(())
    }

    pub fn encode(
        &self,
        buffer: &PixelBuffer,
        pts_us: u64,
        force_keyframe: bool,
        context: Box<SubmissionContext>,
    ) -> Result<(), VtError> {
        let pts = unsafe { CMTimeMake(pts_us as i64, 1_000_000) };
        let fps = self.fps.load(Ordering::Acquire);
        let duration = unsafe { CMTimeMake(1, fps as i32) };
        let (key_cb, value_cb) = dict_callbacks();
        let props = if force_keyframe {
            let dict = unsafe { CFDictionaryCreateMutable(ptr::null(), 0, key_cb, value_cb) };
            unsafe {
                CFDictionarySetValue(dict, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)
            };
            dict
        } else {
            ptr::null_mut()
        };
        let raw = Box::into_raw(context) as *mut c_void;
        let mut flags = 0u32;
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                buffer.as_ref(),
                pts,
                duration,
                props,
                raw,
                &mut flags,
            )
        };
        if !props.is_null() {
            unsafe { CFRelease(props) };
        }
        if status != 0 {
            // No callback will fire, so reclaim the submission context here.
            unsafe { drop(Box::from_raw(raw as *mut SubmissionContext)) };
            return Err(VtError::new(status, "VTCompressionSessionEncodeFrame"));
        }
        self.ctx.live.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub fn complete_frames(&self) -> Result<(), VtError> {
        let status = unsafe { VTCompressionSessionCompleteFrames(self.session, kCMTimeInvalid) };
        if status == 0 {
            Ok(())
        } else {
            Err(VtError::new(status, "VTCompressionSessionCompleteFrames"))
        }
    }

    pub fn live(&self) -> usize {
        self.ctx.live.load(Ordering::Acquire)
    }

    pub fn take_callback_overflows(&self) -> usize {
        self.ctx.overflowed.swap(0, Ordering::AcqRel)
    }

    pub fn coded_dimensions(&self) -> (u32, u32) {
        (self.coded_width, self.coded_height)
    }

    /// Read back the negotiated profile level string as evidence.
    pub fn profile_level(&self) -> Option<String> {
        let mut value: CFTypeRef = ptr::null();
        let status = unsafe {
            VTSessionCopyProperty(
                self.session,
                kVTCompressionPropertyKey_ProfileLevel,
                ptr::null(),
                &mut value,
            )
        };
        if status != 0 || value.is_null() {
            return None;
        }
        let mut buf = [0u8; 128];
        let ok = unsafe {
            CFStringGetCString(
                value as CFStringRef,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        unsafe { CFRelease(value) };
        if ok == 0 {
            return None;
        }
        let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).copied().collect();
        String::from_utf8(bytes).ok()
    }
}

impl Drop for VtSession {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTCompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            self.session = ptr::null_mut();
        }
    }
}
