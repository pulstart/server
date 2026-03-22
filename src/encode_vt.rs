#![allow(dead_code, non_upper_case_globals)]

use crossbeam_channel::{bounded, Receiver, Sender};
use std::ffi::c_void;
use std::ptr;

// ── CoreFoundation FFI ──────────────────────────────────────────────────
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFTypeRef = *const c_void;
type CFNumberRef = *const c_void;
type CFBooleanRef = *const c_void;
type CVPixelBufferRef = *mut c_void;
type CMSampleBufferRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CMTime = CMTimeRepr;

#[repr(C)]
#[derive(Copy, Clone)]
struct CMTimeRepr {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const K_CM_TIME_FLAGS_VALID: u32 = 1;

fn cm_time(value: i64, timescale: i32) -> CMTime {
    CMTimeRepr {
        value,
        timescale,
        flags: K_CM_TIME_FLAGS_VALID,
        epoch: 0,
    }
}

const K_CM_TIME_INVALID: CMTime = CMTimeRepr {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

// ── CoreFoundation functions ─────────────────────────────────────────────
extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;

    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
}

const K_CF_NUMBER_SINT32_TYPE: isize = 3;

fn cf_number_i32(val: i32) -> CFNumberRef {
    unsafe {
        CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_SINT32_TYPE,
            &val as *const i32 as *const c_void,
        )
    }
}

// ── CoreMedia FFI ────────────────────────────────────────────────────────
extern "C" {
    fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sbuf: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> *const c_void;
    fn CMBlockBufferGetDataLength(block: CMBlockBufferRef) -> usize;
    fn CMBlockBufferGetDataPointer(
        block: CMBlockBufferRef,
        offset: usize,
        length_at_offset: *mut usize,
        total_length: *mut usize,
        data_pointer: *mut *mut u8,
    ) -> i32;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format_desc: CMFormatDescriptionRef,
        parameter_set_index: usize,
        parameter_set_pointer_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> i32;

    static kCMSampleAttachmentKey_NotSync: CFStringRef;
}

// ── VideoToolbox FFI ─────────────────────────────────────────────────────
type VTCompressionSessionRef = *mut c_void;

type VTCompressionOutputCallback = extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: i32,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

extern "C" {
    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: VTCompressionOutputCallback,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> i32;

    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> i32;

    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> i32;

    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    fn VTSessionSetProperty(
        session: *mut c_void,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> i32;
}

// VideoToolbox property keys
extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
}

const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331; // 'avc1'

// ── Encoder ──────────────────────────────────────────────────────────────
pub struct VTEncoder {
    session: VTCompressionSessionRef,
    _callback_ctx: *mut Sender<Vec<u8>>,
    nal_rx: Receiver<Vec<u8>>,
    frame_count: i64,
}

unsafe impl Send for VTEncoder {}

impl VTEncoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32, framerate: u32) -> Result<Self, String> {
        let (nal_tx, nal_rx) = bounded(64);
        let ctx_ptr = Box::into_raw(Box::new(nal_tx));

        let mut session: VTCompressionSessionRef = ptr::null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                kCFAllocatorDefault,
                width as i32,
                height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                vt_output_callback,
                ctx_ptr as *mut c_void,
                &mut session,
            )
        };
        if status != 0 {
            unsafe {
                drop(Box::from_raw(ctx_ptr));
            }
            return Err(format!("VTCompressionSessionCreate failed: {status}"));
        }

        unsafe {
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue as CFTypeRef,
            );
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse as CFTypeRef,
            );
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel as CFTypeRef,
            );

            let bitrate = cf_number_i32(bitrate_bps.min(i32::MAX as u32) as i32);
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_AverageBitRate,
                bitrate as CFTypeRef,
            );
            CFRelease(bitrate as CFTypeRef);

            let keyframe_interval = cf_number_i32(framerate.max(1).min(i32::MAX as u32) as i32);
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                keyframe_interval as CFTypeRef,
            );
            CFRelease(keyframe_interval as CFTypeRef);

            let fps = cf_number_i32(framerate.min(i32::MAX as u32) as i32);
            VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                fps as CFTypeRef,
            );
            CFRelease(fps as CFTypeRef);
        }

        Ok(Self {
            session,
            _callback_ctx: ctx_ptr,
            nal_rx,
            frame_count: 0,
        })
    }

    pub fn encode_pixel_buffer(&mut self, pixel_buffer: CVPixelBufferRef) -> Result<(), String> {
        let pts = cm_time(self.frame_count, 60);
        self.frame_count += 1;

        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                pts,
                K_CM_TIME_INVALID,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(format!("VTCompressionSessionEncodeFrame failed: {status}"));
        }
        Ok(())
    }

    pub fn receive_nals(&self) -> Vec<Vec<u8>> {
        let mut nals = Vec::new();
        while let Ok(nal) = self.nal_rx.try_recv() {
            nals.push(nal);
        }
        nals
    }

    pub fn flush(&self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, K_CM_TIME_INVALID);
        }
    }
}

impl Drop for VTEncoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, K_CM_TIME_INVALID);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as CFTypeRef);
            drop(Box::from_raw(self._callback_ctx));
        }
    }
}

// ── Output callback ──────────────────────────────────────────────────────

/// Check if this sample is a keyframe (sync sample).
unsafe fn is_keyframe(sample_buffer: CMSampleBufferRef) -> bool {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, 0);
    if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
        // No attachments → treat as keyframe
        return true;
    }
    let dict = CFArrayGetValueAtIndex(attachments, 0);
    if dict.is_null() {
        return true;
    }
    let not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync as *const c_void);
    if not_sync.is_null() {
        return true; // key absent → is sync
    }
    CFBooleanGetValue(not_sync) == 0
}

/// Extract SPS and PPS from the format description as Annex B NAL units.
unsafe fn extract_parameter_sets(format_desc: CMFormatDescriptionRef) -> Vec<u8> {
    let mut result = Vec::new();
    let mut param_count: usize = 0;

    // Get the number of parameter sets
    let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format_desc,
        0,
        ptr::null_mut(),
        ptr::null_mut(),
        &mut param_count,
        ptr::null_mut(),
    );
    if status != 0 {
        return result;
    }

    for i in 0..param_count {
        let mut param_ptr: *const u8 = ptr::null();
        let mut param_size: usize = 0;
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format_desc,
            i,
            &mut param_ptr,
            &mut param_size,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        if status == 0 && !param_ptr.is_null() && param_size > 0 {
            result.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            result.extend_from_slice(std::slice::from_raw_parts(param_ptr, param_size));
        }
    }

    result
}

extern "C" fn vt_output_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: u32,
    sample_buffer: CMSampleBufferRef,
) {
    if status != 0 || sample_buffer.is_null() {
        return;
    }

    let tx = unsafe { &*(output_callback_ref_con as *const Sender<Vec<u8>>) };

    let block_buffer = unsafe { CMSampleBufferGetDataBuffer(sample_buffer) };
    if block_buffer.is_null() {
        return;
    }

    let mut data_ptr: *mut u8 = ptr::null_mut();
    let mut total_len: usize = 0;
    let status = unsafe {
        CMBlockBufferGetDataPointer(
            block_buffer,
            0,
            ptr::null_mut(),
            &mut total_len,
            &mut data_ptr,
        )
    };
    if status != 0 || data_ptr.is_null() || total_len == 0 {
        return;
    }

    let keyframe = unsafe { is_keyframe(sample_buffer) };

    // Build Annex B output
    let mut annex_b = Vec::with_capacity(total_len + 128);

    // For keyframes: prepend SPS/PPS from format description
    if keyframe {
        let format_desc = unsafe { CMSampleBufferGetFormatDescription(sample_buffer) };
        if !format_desc.is_null() {
            let params = unsafe { extract_parameter_sets(format_desc) };
            annex_b.extend_from_slice(&params);
        }
    }

    // Convert AVCC (length-prefixed) → Annex B (start code prefixed)
    let data = unsafe { std::slice::from_raw_parts(data_ptr, total_len) };
    let mut offset = 0;

    while offset + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + nal_len > data.len() {
            break;
        }

        annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        annex_b.extend_from_slice(&data[offset..offset + nal_len]);
        offset += nal_len;
    }

    if !annex_b.is_empty() {
        let _ = tx.try_send(annex_b);
    }
}
