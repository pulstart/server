//! Vulkan Video encoding via FFmpeg (`h264_vulkan` / `hevc_vulkan` / `av1_vulkan`).
//!
//! On NVIDIA every NVENC-API submission is time-sliced behind a GPU-bound game
//! (measured ~34 ms/frame under a saturated 3D queue vs ~1 ms idle), and no
//! context/queue priority changes that. Vulkan's transfer and video-encode
//! queues run on engines the 3D load does not block (~3 ms/frame either way),
//! so this backend keeps the stream smooth while the host GPU is pegged.
//! SDR 4:2:0 H.264/HEVC only: FFmpeg's `av1_vulkan` writes an undecodable
//! sequence header once rate control is set. `ST_VULKAN_ENCODE=0` disables it.

use crate::capture::{CapturedFrame, FrameData, Nv12Claim};
use crate::colorspace::Colorspace;
use crate::encode::{ffmpeg_err, send_and_collect};
use crate::encode_config::{Codec, EncoderConfig};
use crate::transport::EncodedUnit;

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use std::ffi::CString;
use std::ptr;
use std::sync::Mutex;

/// `hevc_vulkan` rejects GOPs above u16 on NVIDIA; one IDR per ~9 min at 120 fps.
const MAX_GOP: u32 = 65_535;

/// Share of the frame interval a codec may spend encoding. NVIDIA's HEVC path
/// costs ~4x H.264 (7.5 vs 1.9 ms at 1440p), so HEVC at high fps falls through
/// to the next codec instead of capping the frame rate.
const MAX_FRAME_BUDGET_SHARE: f64 = 0.6;

pub fn enabled() -> bool {
    !matches!(
        std::env::var("ST_VULKAN_ENCODE").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

fn codec_name(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 => Some("h264_vulkan"),
        Codec::Hevc => Some("hevc_vulkan"),
        Codec::Av1 => None,
    }
}

struct DeviceRef(*mut ffi::AVBufferRef);

// SAFETY: AVBufferRef refcounting is atomic; the device is only read through it.
unsafe impl Send for DeviceRef {}

/// Vulkan device creation is expensive; ABR/fps rebuilds reuse one device for
/// the process lifetime.
static DEVICE: Mutex<Option<(Option<String>, DeviceRef)>> = Mutex::new(None);

/// A new reference to the shared device. `None` reuses whatever device exists
/// (encoder rebuilds don't know the capture node).
fn device_ref(render_node: Option<&str>) -> Result<*mut ffi::AVBufferRef, String> {
    let mut cached = DEVICE.lock().unwrap();
    if let Some((node, dev)) = cached.as_ref() {
        if render_node.is_none() || node.as_deref() == render_node {
            let reference = unsafe { ffi::av_buffer_ref(dev.0) };
            if !reference.is_null() {
                return Ok(reference);
            }
        }
    }
    let device = unsafe { create_device(render_node)? };
    if let Some((_, mut old)) = cached.take() {
        unsafe { ffi::av_buffer_unref(&mut old.0) };
    }
    *cached = Some((render_node.map(str::to_owned), DeviceRef(device)));
    Ok(unsafe { ffi::av_buffer_ref(device) })
}

unsafe fn create_device(render_node: Option<&str>) -> Result<*mut ffi::AVBufferRef, String> {
    // Derive from the capture GPU's DRM node so multi-GPU hosts encode on the
    // GPU that captured.
    if let Some(node) = render_node.and_then(|n| CString::new(n).ok()) {
        let mut drm = ptr::null_mut();
        if ffi::av_hwdevice_ctx_create(
            &mut drm,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        ) >= 0
        {
            let mut vk = ptr::null_mut();
            let ret = ffi::av_hwdevice_ctx_create_derived(
                &mut vk,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                drm,
                0,
            );
            ffi::av_buffer_unref(&mut drm);
            if ret >= 0 {
                return Ok(vk);
            }
        }
    }
    let mut vk = ptr::null_mut();
    let ret = ffi::av_hwdevice_ctx_create(
        &mut vk,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
        ptr::null(),
        ptr::null_mut(),
        0,
    );
    if ret < 0 {
        return Err(format!("Vulkan device: {}", ffmpeg_err(ret)));
    }
    Ok(vk)
}

/// How much of a freshly opened encoder to exercise before use.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenCheck {
    /// Encode test frames and reject a codec too slow for the frame rate.
    Gate,
    /// Encode test frames to prove the device works end-to-end.
    Prove,
    /// The same codec already runs live; test frames would only contend with
    /// it on the encode engine and stall the stream.
    Trust,
}

pub struct VulkanEncoder {
    codec_ctx: *mut ffi::AVCodecContext,
    frames_ref: *mut ffi::AVBufferRef,
    /// NV12 source frame: borrows `RamNv12` planes, or owns CPU-converted
    /// pixels for BGRA input.
    nv12: *mut ffi::AVFrame,
    nv12_owned: bool,
    scaler: *mut ffi::SwsContext,
    colorspace: Colorspace,
    frame_index: i64,
    force_keyframe_next: bool,
    width: u32,
    height: u32,
    _nv12_claim: Nv12Claim,
    /// `ST_TRACE`: per-frame (upload, encode) times, logged in batches.
    stage_times: Option<Vec<(std::time::Duration, std::time::Duration)>>,
}

unsafe impl Send for VulkanEncoder {}

impl VulkanEncoder {
    /// `gate_frame_cost`: reject a codec too slow for the frame rate. Only
    /// meaningful with no other session encoding (initial selection); a
    /// rebuild's self-test shares the encode engine with the live session.
    pub fn with_config(
        config: &EncoderConfig,
        render_node: Option<&str>,
        check: OpenCheck,
    ) -> Result<Self, String> {
        if config.is_hdr() || config.is_yuv444() {
            return Err("Vulkan encode path is SDR 4:2:0 only".into());
        }
        if !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) {
            return Err("Vulkan encode path needs even dimensions".into());
        }
        let name = codec_name(config.codec).ok_or("Vulkan encode path has no AV1 support")?;
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        let name_c = CString::new(name).unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name_c.as_ptr()) };
        if codec.is_null() {
            return Err(format!("{name} not available in this FFmpeg build"));
        }

        let mut encoder = Self {
            codec_ctx: ptr::null_mut(),
            frames_ref: ptr::null_mut(),
            nv12: unsafe { ffi::av_frame_alloc() },
            nv12_owned: false,
            scaler: ptr::null_mut(),
            colorspace: Colorspace::sdr_rec709(),
            frame_index: 0,
            force_keyframe_next: true,
            width: config.width,
            height: config.height,
            _nv12_claim: Nv12Claim::new(),
            stage_times: None,
        };
        if encoder.nv12.is_null() {
            return Err("av_frame_alloc failed".into());
        }
        unsafe {
            encoder.init_frames(render_node)?;
            encoder.open(codec, name, config)?;
        }
        let mut cost = String::new();
        if check != OpenCheck::Trust {
            // Opening can succeed on a device that then fails to encode; prove
            // frames end-to-end so selection falls through instead.
            let frame_cost = unsafe { encoder.measure_frame_cost() }
                .map_err(|e| format!("{name} self-test: {e}"))?;
            let budget = std::time::Duration::from_secs_f64(
                MAX_FRAME_BUDGET_SHARE / config.framerate.max(1) as f64,
            );
            if check == OpenCheck::Gate && frame_cost > budget {
                return Err(format!(
                    "{name} too slow for {}fps ({frame_cost:.1?}/frame, budget {budget:.1?})",
                    config.framerate
                ));
            }
            cost = format!(", {frame_cost:.1?}/frame");
        }
        encoder.force_keyframe_next = true;
        encoder.stage_times = std::env::var_os("ST_TRACE").map(|_| Vec::with_capacity(240));
        println!(
            "[vulkan] {name} encoder opened ({}x{}, {}kbps, {}fps{cost})",
            config.width, config.height, config.bitrate_kbps, config.framerate
        );
        Ok(encoder)
    }

    unsafe fn init_frames(&mut self, render_node: Option<&str>) -> Result<(), String> {
        let mut device = device_ref(render_node)?;
        self.frames_ref = ffi::av_hwframe_ctx_alloc(device);
        ffi::av_buffer_unref(&mut device);
        if self.frames_ref.is_null() {
            return Err("av_hwframe_ctx_alloc failed".into());
        }
        let frames = (*self.frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
        (*frames).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames).width = self.width as i32;
        (*frames).height = self.height as i32;
        let ret = ffi::av_hwframe_ctx_init(self.frames_ref);
        if ret < 0 {
            return Err(format!("Vulkan frames: {}", ffmpeg_err(ret)));
        }
        Ok(())
    }

    unsafe fn open(
        &mut self,
        codec: *const ffi::AVCodec,
        name: &str,
        config: &EncoderConfig,
    ) -> Result<(), String> {
        let ctx = ffi::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 returned null".into());
        }
        self.codec_ctx = ctx;
        (*ctx).width = config.width as i32;
        (*ctx).height = config.height as i32;
        (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
        (*ctx).sw_pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).hw_frames_ctx = ffi::av_buffer_ref(self.frames_ref);
        (*ctx).time_base = ffi::AVRational {
            num: 1,
            den: config.framerate as i32,
        };
        (*ctx).framerate = ffi::AVRational {
            num: config.framerate as i32,
            den: 1,
        };
        (*ctx).gop_size = config.gop_size.min(MAX_GOP) as i32;
        (*ctx).max_b_frames = 0;
        (*ctx).refs = config.ref_frames as i32;
        (*ctx).bit_rate = config.bitrate_bps();
        (*ctx).rc_max_rate = config.bitrate_bps();
        (*ctx).rc_buffer_size = config.vbv_buffer_size(false);
        if let Some(qmin) = config.min_qp() {
            (*ctx).qmin = qmin as i32;
        }
        if config.low_delay {
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
        }
        (*ctx).profile = match config.codec {
            Codec::H264 => 100, // High
            Codec::Hevc => 1,   // Main
            Codec::Av1 => unreachable!("rejected above"),
        };
        self.colorspace.apply_to_codec_ctx(ctx);

        let rc_mode = if config.cbr_forced() { "cbr" } else { "vbr" };
        for (key, value) in [
            ("async_depth", "1"),
            // NVIDIA's "ull" doubles HEVC encode time with no latency gain at
            // async_depth=1 without B-frames.
            ("tune", "ll"),
            ("usage", "stream"),
            ("content", "rendered"),
            ("rc_mode", rc_mode),
        ] {
            let key = CString::new(key).unwrap();
            let value = CString::new(value).unwrap();
            ffi::av_opt_set((*ctx).priv_data, key.as_ptr(), value.as_ptr(), 0);
        }

        let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
        if ret < 0 {
            return Err(format!("Failed to open {name}: {}", ffmpeg_err(ret)));
        }
        Ok(())
    }

    /// Point the NV12 frame at owned storage (BGRA conversion target).
    unsafe fn own_nv12(&mut self) -> Result<(), String> {
        if self.nv12_owned {
            return Ok(());
        }
        ffi::av_frame_unref(self.nv12);
        (*self.nv12).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*self.nv12).width = self.width as i32;
        (*self.nv12).height = self.height as i32;
        let ret = ffi::av_frame_get_buffer(self.nv12, 0);
        if ret < 0 {
            return Err(format!("NV12 frame: {}", ffmpeg_err(ret)));
        }
        self.nv12_owned = true;
        Ok(())
    }

    /// Encode black frames; returns the fastest once the GPU has clocked up
    /// (an idle GPU runs the first frames at low clocks).
    unsafe fn measure_frame_cost(&mut self) -> Result<std::time::Duration, String> {
        self.own_nv12()?;
        let (w, h) = (self.width as usize, self.height as usize);
        for row in 0..h {
            ptr::write_bytes(
                (*self.nv12).data[0].add(row * (*self.nv12).linesize[0] as usize),
                16,
                w,
            );
        }
        for row in 0..h / 2 {
            ptr::write_bytes(
                (*self.nv12).data[1].add(row * (*self.nv12).linesize[1] as usize),
                128,
                w,
            );
        }
        let mut best = std::time::Duration::MAX;
        for i in 0..14 {
            let start = std::time::Instant::now();
            if self.upload_and_encode()?.is_empty() {
                return Err("no packet produced".into());
            }
            if i >= 6 {
                best = best.min(start.elapsed());
            }
        }
        Ok(best)
    }

    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedUnit>, String> {
        if frame.width != self.width || frame.height != self.height {
            return Err("Vulkan encoder frame size mismatch".into());
        }
        unsafe {
            match &frame.data {
                FrameData::RamNv12(data) => self.borrow_nv12(data)?,
                FrameData::Ram(data) => self.convert_bgra(data)?,
                FrameData::DmaBuf { .. } => {
                    let bgra = crate::capture::try_clone_frame_to_ram_bgra(frame)?
                        .ok_or("Vulkan encoder: unsupported DMA-BUF format")?;
                    self.convert_bgra(&bgra)?;
                }
            }
            self.upload_and_encode()
        }
    }

    unsafe fn borrow_nv12(&mut self, data: &[u8]) -> Result<(), String> {
        let (w, h) = (self.width as usize, self.height as usize);
        if data.len() < w * h * 3 / 2 {
            return Err("NV12 frame too small".into());
        }
        if self.nv12_owned {
            ffi::av_frame_unref(self.nv12);
            self.nv12_owned = false;
        }
        let frame = self.nv12;
        (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*frame).width = w as i32;
        (*frame).height = h as i32;
        // Read-only for the upload; FFmpeg only copies out of these planes.
        (*frame).data[0] = data.as_ptr() as *mut u8;
        (*frame).data[1] = data.as_ptr().add(w * h) as *mut u8;
        (*frame).linesize[0] = w as i32;
        (*frame).linesize[1] = w as i32;
        Ok(())
    }

    unsafe fn convert_bgra(&mut self, data: &[u8]) -> Result<(), String> {
        let (w, h) = (self.width as i32, self.height as i32);
        if data.len() < (w * h * 4) as usize {
            return Err("BGRA frame too small".into());
        }
        self.own_nv12()?;
        if self.scaler.is_null() {
            self.scaler = ffi::sws_getContext(
                w,
                h,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                w,
                h,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                1, // SWS_FAST_BILINEAR (enum in FFmpeg 8+, define before)
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if self.scaler.is_null() {
                return Err("sws_getContext(BGRA→NV12) failed".into());
            }
            self.colorspace.apply_to_scaler(self.scaler);
        }
        let src = [data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
        let src_stride = [w * 4, 0, 0, 0];
        ffi::sws_scale(
            self.scaler,
            src.as_ptr(),
            src_stride.as_ptr(),
            0,
            h,
            (*self.nv12).data.as_ptr(),
            (*self.nv12).linesize.as_ptr(),
        );
        Ok(())
    }

    unsafe fn upload_and_encode(&mut self) -> Result<Vec<EncodedUnit>, String> {
        let mut hw = ffi::av_frame_alloc();
        if hw.is_null() {
            return Err("av_frame_alloc failed".into());
        }
        let start = std::time::Instant::now();
        let mut uploaded = start;
        let result = (|| {
            let ret = ffi::av_hwframe_get_buffer(self.frames_ref, hw, 0);
            if ret < 0 {
                return Err(format!("av_hwframe_get_buffer: {}", ffmpeg_err(ret)));
            }
            let ret = ffi::av_hwframe_transfer_data(hw, self.nv12, 0);
            if ret < 0 {
                return Err(format!("upload: {}", ffmpeg_err(ret)));
            }
            uploaded = std::time::Instant::now();
            (*hw).pts = self.frame_index;
            self.frame_index += 1;
            self.colorspace.apply_to_frame(hw);
            (*hw).pict_type = if std::mem::take(&mut self.force_keyframe_next) {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            send_and_collect(self.codec_ctx, hw)
        })();
        ffi::av_frame_free(&mut hw);
        if let Some(times) = self.stage_times.as_mut() {
            times.push((uploaded - start, uploaded.elapsed()));
            if times.len() == times.capacity() {
                let pct = |v: &mut Vec<std::time::Duration>, q: usize| {
                    v.sort();
                    v[v.len() * q / 100]
                };
                let (mut up, mut enc): (Vec<_>, Vec<_>) = times.drain(..).unzip();
                eprintln!(
                    "[vulkan] upload p50={:.2?} p95={:.2?} | encode p50={:.2?} p95={:.2?}",
                    pct(&mut up, 50),
                    pct(&mut up, 95),
                    pct(&mut enc, 50),
                    pct(&mut enc, 95)
                );
            }
        }
        result
    }

    pub fn reset_for_keyframe(&mut self) {
        self.force_keyframe_next = true;
    }

    pub fn flush(&mut self) -> Vec<EncodedUnit> {
        unsafe { send_and_collect(self.codec_ctx, ptr::null_mut()).unwrap_or_default() }
    }
}

impl Drop for VulkanEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.codec_ctx.is_null() {
                ffi::avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.scaler.is_null() {
                ffi::sws_freeContext(self.scaler);
            }
            // Borrowed planes have no buf[] refs, so freeing never touches them.
            ffi::av_frame_free(&mut self.nv12);
            ffi::av_buffer_unref(&mut self.frames_ref);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CapturedFrame;
    use std::time::{Duration, Instant};

    fn live_enabled() -> bool {
        std::env::var_os("ST_TEST_VULKAN_ENCODE").is_some()
    }

    fn render_node() -> String {
        std::env::var("ST_TEST_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
    }

    fn frame(data: FrameData, width: u32, height: u32) -> CapturedFrame {
        CapturedFrame {
            data,
            width,
            height,
            cursor: None,
            force_keyframe: false,
            captured_at: std::time::Instant::now(),
        }
    }

    fn nv12_solid(w: u32, h: u32, (y, u, v): (u8, u8, u8)) -> Vec<u8> {
        let (w, h) = (w as usize, h as usize);
        let mut buf = vec![y; w * h * 3 / 2];
        for pair in buf[w * h..].chunks_exact_mut(2) {
            pair[0] = u;
            pair[1] = v;
        }
        buf
    }

    type Yuv = (u8, u8, u8);

    fn decode_centre(codec: Codec, units: &[EncodedUnit]) -> (Vec<Yuv>, i32) {
        let (frames, matrix) = decode_at(codec, units, &[(0.5, 0.5)]);
        (frames.into_iter().map(|points| points[0]).collect(), matrix)
    }

    /// Decode an Annex-B stream in software; returns (Y, Cb, Cr) at each
    /// relative `(x, y)` point of every decoded frame plus the colour matrix.
    fn decode_at(
        codec: Codec,
        units: &[EncodedUnit],
        points: &[(f32, f32)],
    ) -> (Vec<Vec<Yuv>>, i32) {
        unsafe {
            let decoder = ffi::avcodec_find_decoder(match codec {
                Codec::H264 => ffi::AVCodecID::AV_CODEC_ID_H264,
                Codec::Hevc => ffi::AVCodecID::AV_CODEC_ID_HEVC,
                Codec::Av1 => ffi::AVCodecID::AV_CODEC_ID_AV1,
            });
            assert!(!decoder.is_null(), "no software decoder for {codec:?}");
            let mut ctx = ffi::avcodec_alloc_context3(decoder);
            assert!(ffi::avcodec_open2(ctx, decoder, ptr::null_mut()) >= 0);
            let mut pkt = ffi::av_packet_alloc();
            let mut out = ffi::av_frame_alloc();
            let mut centres = Vec::new();
            let mut matrix = -1;
            let mut drain = |ctx: *mut ffi::AVCodecContext| {
                while ffi::avcodec_receive_frame(ctx, out) >= 0 {
                    let (w, h) = ((*out).width as f32, (*out).height as f32);
                    let samples = points
                        .iter()
                        .map(|&(px, py)| {
                            let (cx, cy) = ((w * px) as usize, (h * py) as usize);
                            let y = *(*out).data[0].add(cy * (*out).linesize[0] as usize + cx);
                            let (u, v) =
                                if (*out).format == ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
                                    let p = (*out).data[1]
                                        .add(cy / 2 * (*out).linesize[1] as usize + cx / 2 * 2);
                                    (*p, *p.add(1))
                                } else {
                                    (
                                        *(*out).data[1]
                                            .add(cy / 2 * (*out).linesize[1] as usize + cx / 2),
                                        *(*out).data[2]
                                            .add(cy / 2 * (*out).linesize[2] as usize + cx / 2),
                                    )
                                };
                            (y, u, v)
                        })
                        .collect();
                    matrix = (*out).colorspace as i32;
                    centres.push(samples);
                    ffi::av_frame_unref(out);
                }
            };
            for unit in units {
                (*pkt).data = unit.data.as_ptr() as *mut u8;
                (*pkt).size = unit.data.len() as i32;
                let ret = ffi::avcodec_send_packet(ctx, pkt);
                assert!(
                    ret >= 0,
                    "decoder rejected packet ({}): {} bytes {:02x?} extradata={}",
                    ffmpeg_err(ret),
                    unit.data.len(),
                    &unit.data[..unit.data.len().min(16)],
                    (*ctx).extradata_size
                );
                drain(ctx);
            }
            ffi::avcodec_send_packet(ctx, ptr::null());
            drain(ctx);
            (*pkt).data = ptr::null_mut();
            ffi::av_packet_free(&mut pkt);
            ffi::av_frame_free(&mut out);
            ffi::avcodec_free_context(&mut ctx);
            (centres, matrix)
        }
    }

    fn close(actual: (u8, u8, u8), expected: (u8, u8, u8), tol: i32) -> bool {
        (actual.0 as i32 - expected.0 as i32).abs() <= tol
            && (actual.1 as i32 - expected.1 as i32).abs() <= tol
            && (actual.2 as i32 - expected.2 as i32).abs() <= tol
    }

    /// Regression guard for default-on: every codec must emit a decodable
    /// stream that starts with an IDR, honours a forced IDR mid-stream, keeps
    /// NV12 colours, and converts BGRA with the BT.709 matrix it signals.
    #[test]
    fn vulkan_encode_roundtrip() {
        if !live_enabled() {
            eprintln!("skip: set ST_TEST_VULKAN_ENCODE=1 on a Vulkan Video capable GPU");
            return;
        }
        // BT.709 limited-range pure red.
        const RED_709: (u8, u8, u8) = (63, 102, 240);
        let (w, h) = (640u32, 360u32);
        let av1 = EncoderConfig::from_env_with_framerate_and_codec(w, h, 60, Codec::Av1);
        assert!(VulkanEncoder::with_config(&av1, Some(&render_node()), OpenCheck::Prove).is_err());
        for codec in [Codec::H264, Codec::Hevc] {
            let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 60, codec);
            let mut enc =
                VulkanEncoder::with_config(&config, Some(&render_node()), OpenCheck::Prove)
                    .unwrap_or_else(|e| panic!("{codec:?}: {e}"));
            let mut units = Vec::new();
            let mut keyframes = Vec::new();
            for i in 0..20 {
                if i == 12 {
                    enc.reset_for_keyframe();
                }
                let data = if i % 2 == 0 {
                    FrameData::RamNv12(nv12_solid(w, h, RED_709).into())
                } else {
                    let mut bgra = vec![0u8; (w * h * 4) as usize];
                    for px in bgra.chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 255, 255]);
                    }
                    FrameData::Ram(bgra.into())
                };
                let out = enc.encode(&frame(data, w, h)).expect("encode");
                assert!(!out.is_empty(), "{codec:?}: frame {i} produced no packet");
                keyframes.push(out.iter().any(|u| u.is_recovery));
                units.extend(out);
            }
            assert!(keyframes[0], "{codec:?}: stream must start with an IDR");
            assert!(keyframes[12], "{codec:?}: forced IDR not honoured");
            assert_eq!(
                keyframes.iter().filter(|k| **k).count(),
                2,
                "{codec:?}: unexpected periodic keyframes {keyframes:?}"
            );
            let (centres, matrix) = decode_centre(codec, &units);
            assert_eq!(centres.len(), 20, "{codec:?}: decoded frame count");
            for (i, c) in centres.iter().enumerate() {
                assert!(close(*c, RED_709, 4), "{codec:?}: frame {i} decoded {c:?}");
            }
            assert_eq!(
                matrix,
                ffi::AVColorSpace::AVCOL_SPC_BT709 as i32,
                "{codec:?}: stream must signal BT.709"
            );
        }
    }

    /// Full NVIDIA path: scanout → stabilizer NV12 readback (enabled by the
    /// encoder's claim) → Vulkan encode → decode, picture upright in BT.709.
    #[test]
    fn kms_readback_nv12_encodes_upright() {
        use crate::capture::linux::kms_gpu_copy::tests as kms;
        if !live_enabled() {
            return;
        }
        const RED_709: (u8, u8, u8) = (63, 102, 240);
        const BLUE_709: (u8, u8, u8) = (32, 240, 118);
        let (w, h) = (640u32, 360u32);
        let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 60, Codec::Hevc);
        let mut enc = VulkanEncoder::with_config(&config, Some(&render_node()), OpenCheck::Prove)
            .expect("vulkan");
        let (mut stab, src) = kms::painted_readback_source(&render_node(), w, h);
        let mut units = Vec::new();
        for _ in 0..5 {
            let data = kms::stabilize_source(&mut stab, &src, w, h);
            assert!(
                matches!(data, FrameData::RamNv12(_)),
                "claim must switch readback to NV12"
            );
            units.extend(enc.encode(&frame(data, w, h)).expect("encode"));
        }
        kms::destroy_source(&stab, src);
        let (frames, _) = decode_at(Codec::Hevc, &units, &[(0.5, 0.25), (0.5, 0.75)]);
        assert_eq!(frames.len(), 5);
        for points in frames {
            assert!(close(points[0], RED_709, 4), "top {:?}", points[0]);
            assert!(close(points[1], BLUE_709, 4), "bottom {:?}", points[1]);
        }
    }

    /// Live: real KMS scanout (needs CAP_SYS_ADMIN, e.g. `sudo -E`) through the
    /// stabilizer's NV12 readback into the Vulkan encoder, idle and with the
    /// GPU saturated. Asserts every frame decodes; prints capture→packet time.
    /// `ST_TEST_VULKAN_KMS=1 sudo -E <test-binary> live_kms_vulkan_pipeline --nocapture`
    #[test]
    fn live_kms_vulkan_pipeline() {
        use crate::capture::linux::kms_capture::KmsCapture;
        use crate::capture::CaptureBackend;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        if std::env::var_os("ST_TEST_VULKAN_KMS").is_none() {
            return;
        }
        crate::capture::set_target_fps(120);
        // Full-rate capture: damage skip would pace a static desktop at 4 fps.
        std::env::set_var("ST_KMS_DAMAGE", "0");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let mut capture = KmsCapture::new();
        capture
            .start(tx)
            .expect("KMS capture (needs CAP_SYS_ADMIN)");
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first frame");
        let (w, h) = (first.width, first.height);
        drop(first);
        let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 120, Codec::H264);
        let mut enc = VulkanEncoder::with_config(&config, Some(&render_node()), OpenCheck::Prove)
            .expect("vulkan");
        for loaded in [false, true] {
            let stop = Arc::new(AtomicBool::new(false));
            let load = loaded.then(|| {
                crate::capture::linux::kms_gpu_copy::tests::spawn_gpu_load(
                    render_node(),
                    w,
                    h,
                    Arc::clone(&stop),
                )
            });
            std::thread::sleep(Duration::from_millis(300));
            while rx.try_recv().is_ok() {}
            let (mut units, mut times, mut nv12) = (Vec::new(), Vec::new(), 0);
            let start = Instant::now();
            let mut frames = 0;
            while frames < 240 {
                let Ok(f) = rx.recv_timeout(Duration::from_millis(500)) else {
                    continue;
                };
                nv12 += usize::from(matches!(f.data, FrameData::RamNv12(_)));
                let t = Instant::now();
                let out = enc.encode(&f).expect("encode");
                times.push(t.elapsed());
                units.extend(out);
                frames += 1;
            }
            let fps = frames as f64 / start.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            if let Some(load) = load {
                load.join().unwrap();
            }
            times.sort();
            eprintln!(
                "[live] {} {w}x{h}: {fps:.0} fps delivered, nv12 {nv12}/240, encode p50={:.2?} p95={:.2?}",
                if loaded { "gpu-load" } else { "idle" },
                times[times.len() / 2],
                times[times.len() * 95 / 100]
            );
            enc.reset_for_keyframe();
            let (decoded, _) = decode_at(Codec::H264, &units, &[(0.5, 0.5)]);
            assert_eq!(decoded.len(), 240, "every captured frame must decode");
        }
        capture.stop();
    }

    /// Desktop-like NV12 (text-ish high-frequency blocks over gradients)
    /// scrolling 4 px per frame; solid frames encode trivially fast.
    struct ScrollingSource {
        wide: Vec<u8>,
        stride: usize,
        w: usize,
        h: usize,
    }

    impl ScrollingSource {
        fn new(w: u32, h: u32) -> Self {
            let (w, h) = (w as usize, h as usize);
            let stride = w + 1024;
            let mut wide = vec![0u8; stride * h * 3 / 2];
            let hash = |a: usize, b: usize| {
                let mut x =
                    (a as u32).wrapping_mul(0x9e37_79b1) ^ (b as u32).wrapping_mul(0x85eb_ca6b);
                x ^= x >> 15;
                x.wrapping_mul(0x2c1b_3c6d) >> 24
            };
            for y in 0..h {
                for x in 0..stride {
                    let glyph = hash(x / 3, y / 5) & 3 == 0 && (y / 24) % 3 != 2;
                    wide[y * stride + x] = if glyph {
                        20 + (hash(x, y) & 31) as u8
                    } else {
                        150 + ((x / 7 + y / 11) % 80) as u8
                    };
                }
            }
            for y in 0..h / 2 {
                for x in 0..stride / 2 {
                    let i = stride * h + y * stride + x * 2;
                    wide[i] = 110 + ((x / 40 + y / 30) % 40) as u8;
                    wide[i + 1] = 120 + ((x / 25) % 30) as u8;
                }
            }
            Self { wide, stride, w, h }
        }

        fn frame(&self, n: usize) -> Vec<u8> {
            let (w, h, stride) = (self.w, self.h, self.stride);
            let off = (n * 4) % 1024;
            let mut out = vec![0u8; w * h * 3 / 2];
            for y in 0..h {
                out[y * w..(y + 1) * w]
                    .copy_from_slice(&self.wide[y * stride + off..y * stride + off + w]);
            }
            for y in 0..h / 2 {
                let src = stride * h + y * stride + off;
                out[w * h + y * w..w * h + (y + 1) * w].copy_from_slice(&self.wide[src..src + w]);
            }
            out
        }
    }

    /// Paced 1440p120 encode of scrolling desktop content, split into upload
    /// (memcpy + transfer submit) and encode (submit + wait) per frame.
    /// `ST_TEST_VULKAN_ENCODE=1 cargo test --release vulkan_paced_bench -- --nocapture`
    #[test]
    fn vulkan_paced_bench() {
        if !live_enabled() {
            return;
        }
        let (w, h) = (2560u32, 1440u32);
        let source = ScrollingSource::new(w, h);
        let frames: Vec<Vec<u8>> = (0..64).map(|n| source.frame(n)).collect();
        let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 120, Codec::H264);
        let mut enc = VulkanEncoder::with_config(&config, Some(&render_node()), OpenCheck::Prove)
            .expect("vulkan");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let load = std::env::var_os("ST_TEST_PACED_LOAD").map(|_| {
            crate::capture::linux::kms_gpu_copy::tests::spawn_gpu_load(
                render_node(),
                w,
                h,
                std::sync::Arc::clone(&stop),
            )
        });
        let (mut upload, mut encode, mut bytes) = (Vec::new(), Vec::new(), 0usize);
        let interval = Duration::from_micros(8333);
        let mut next = Instant::now();
        for i in 0..600 {
            unsafe {
                enc.borrow_nv12(&frames[i % frames.len()]).unwrap();
                let mut hw = ffi::av_frame_alloc();
                let t0 = Instant::now();
                assert!(ffi::av_hwframe_get_buffer(enc.frames_ref, hw, 0) >= 0);
                assert!(ffi::av_hwframe_transfer_data(hw, enc.nv12, 0) >= 0);
                let t1 = Instant::now();
                (*hw).pts = enc.frame_index;
                enc.frame_index += 1;
                let out = send_and_collect(enc.codec_ctx, hw).unwrap();
                let t2 = Instant::now();
                ffi::av_frame_free(&mut hw);
                if i >= 60 {
                    upload.push(t1 - t0);
                    encode.push(t2 - t1);
                    bytes += out.iter().map(|u| u.data.len()).sum::<usize>();
                }
            }
            next += interval;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(load) = load {
            load.join().unwrap();
        }
        let p = |v: &mut Vec<Duration>, q: usize| {
            v.sort();
            v[(v.len() * q / 100).min(v.len() - 1)]
        };
        eprintln!(
            "[paced] upload p50={:.2?} p95={:.2?} | encode p50={:.2?} p95={:.2?} p99={:.2?} | {:.1} Mbps",
            p(&mut upload, 50),
            p(&mut upload, 95),
            p(&mut encode, 50),
            p(&mut encode, 95),
            p(&mut encode, 99),
            bytes as f64 * 8.0 / (540.0 / 120.0) / 1e6
        );
    }

    /// Per-frame NV12 encode latency at 1440p, Vulkan vs NVENC, idle and with
    /// a separate context saturating the 3D queue.
    /// `ST_TEST_VULKAN_ENCODE=1 cargo test --release vulkan_encode_bench -- --nocapture`
    #[test]
    fn vulkan_encode_bench() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        if !live_enabled() {
            return;
        }
        let (w, h) = (2560u32, 1440u32);
        let nv12 = nv12_solid(w, h, (80, 120, 140));
        let bgra = vec![0x40u8; (w * h * 4) as usize];
        let nvenc_config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 120, Codec::Hevc);
        let measure = |label: &str, encode: &mut dyn FnMut() -> usize| {
            let mut times = Vec::new();
            for i in 0..140 {
                let start = Instant::now();
                assert!(encode() > 0, "{label}: no packet");
                if i >= 20 {
                    times.push(start.elapsed());
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            times.sort();
            let p = |q: usize| times[times.len() * q / 100];
            eprintln!(
                "[bench] {label:<28} p50={:>7.2?} p95={:>7.2?} max={:>7.2?}",
                p(50),
                p(95),
                times[times.len() - 1]
            );
        };
        let mut vulkan: Vec<(String, VulkanEncoder)> = [(Codec::H264, 120), (Codec::Hevc, 60)]
            .into_iter()
            .map(|(codec, fps)| {
                let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, fps, codec);
                let enc =
                    VulkanEncoder::with_config(&config, Some(&render_node()), OpenCheck::Prove)
                        .unwrap_or_else(|e| panic!("{codec:?}@{fps}: {e}"));
                (format!("{codec:?}@{fps}").to_lowercase(), enc)
            })
            .collect();
        let mut nvenc = crate::encode::NvencEncoder::with_config(&nvenc_config).ok();
        for loaded in [false, true] {
            let stop = Arc::new(AtomicBool::new(false));
            let load = loaded.then(|| {
                let load = crate::capture::linux::kms_gpu_copy::tests::spawn_gpu_load(
                    render_node(),
                    w,
                    h,
                    Arc::clone(&stop),
                );
                std::thread::sleep(Duration::from_millis(500));
                load
            });
            let tag = if loaded { "gpu-load" } else { "idle" };
            for (name, vk) in vulkan.iter_mut() {
                measure(&format!("vulkan {name} {tag}"), &mut || {
                    vk.encode(&frame(FrameData::RamNv12(nv12.clone().into()), w, h))
                        .unwrap()
                        .len()
                });
            }
            if let Some(nvenc) = nvenc.as_mut() {
                measure(&format!("nvenc hevc@120 {tag}"), &mut || {
                    nvenc
                        .encode(&frame(FrameData::Ram(bgra.clone().into()), w, h))
                        .unwrap()
                        .len()
                });
            }
            stop.store(true, Ordering::Relaxed);
            if let Some(load) = load {
                load.join().unwrap();
            }
        }
    }
}
