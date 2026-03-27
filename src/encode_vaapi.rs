/// VAAPI hardware encoding (Intel/AMD GPUs).
///
/// Matches Sunshine's VAAPI encoder path in `platform/linux/vaapi.cpp`.
/// Supports H.264, HEVC, and AV1 codecs with configurable parameters.
/// Handles both DMA-BUF (zero-copy GPU import) and RAM (software upload) frames.
use crate::capture::{CapturedFrame, DmaBufPlane, FrameData};
use crate::colorspace::Colorspace;
use crate::encode_config::{Codec, EncoderConfig};

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as VideoFrame;

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::ptr;

/// RAII wrapper for `AVBufferRef*` (used for hw device and hw frames contexts).
struct HwBufRef {
    ptr: *mut ffi::AVBufferRef,
}

impl HwBufRef {
    unsafe fn from_raw(ptr: *mut ffi::AVBufferRef) -> Self {
        assert!(!ptr.is_null());
        Self { ptr }
    }
}

impl Drop for HwBufRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.ptr) };
        }
    }
}

/// Find the first available DRM render node (/dev/dri/renderD128..135).
fn find_render_node() -> Result<String, String> {
    for i in 128..136 {
        let path = format!("/dev/dri/renderD{i}");
        if std::path::Path::new(&path).exists() {
            return Ok(path);
        }
    }
    Err("No DRM render node found (/dev/dri/renderD128..135)".into())
}

/// Check if the render node belongs to an NVIDIA GPU.
/// nvidia-vaapi-driver is decode-only — VAAPI encode will never work on NVIDIA.
fn is_nvidia_render_node(path: &str) -> bool {
    use drm::Device as BasicDevice;
    use std::fs::OpenOptions;

    struct RenderNode(std::fs::File);
    impl std::os::fd::AsFd for RenderNode {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl std::os::fd::AsRawFd for RenderNode {
        fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
            self.0.as_raw_fd()
        }
    }
    impl BasicDevice for RenderNode {}

    if let Ok(file) = OpenOptions::new().read(true).write(true).open(path) {
        let node = RenderNode(file);
        if let Ok(driver) = node.get_driver() {
            let name = driver.name().to_string_lossy();
            if name.contains("nvidia") {
                return true;
            }
        }
    }
    false
}

/// Detect GPU vendor from DRM driver name for rate-control tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuVendor {
    Intel,
    Amd,
    Other,
}

fn detect_gpu_vendor(path: &str) -> GpuVendor {
    use drm::Device as BasicDevice;
    use std::fs::OpenOptions;

    struct RenderNode(std::fs::File);
    impl std::os::fd::AsFd for RenderNode {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl std::os::fd::AsRawFd for RenderNode {
        fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
            self.0.as_raw_fd()
        }
    }
    impl BasicDevice for RenderNode {}

    if let Ok(file) = OpenOptions::new().read(true).write(true).open(path) {
        let node = RenderNode(file);
        if let Ok(driver) = node.get_driver() {
            let name = driver.name().to_string_lossy().to_lowercase();
            if name.contains("i915") || name.contains("xe") {
                return GpuVendor::Intel;
            }
            if name.contains("amdgpu") || name.contains("radeon") {
                return GpuVendor::Amd;
            }
        }
    }
    GpuVendor::Other
}

pub struct VaapiEncoder {
    codec_ctx: *mut ffi::AVCodecContext,
    _device_ctx: HwBufRef,
    _frames_ctx: HwBufRef,
    scaler: Option<scaling::Context>,
    gpu_vendor: GpuVendor,
    colorspace: Colorspace,
    frame_index: i64,
    force_keyframe_next: bool,
    width: u32,
    height: u32,
    bgra_frame: Option<VideoFrame>,
    nv12_frame: Option<VideoFrame>,
}

// SAFETY: The FFmpeg contexts are only accessed from the pipeline thread.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn with_config(config: &EncoderConfig) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        let render_node = find_render_node()?;

        // nvidia-vaapi-driver is decode-only; skip straight to NVENC.
        if is_nvidia_render_node(&render_node) {
            return Err("NVIDIA GPU detected — VAAPI encode not supported (use NVENC)".into());
        }

        let gpu_vendor = detect_gpu_vendor(&render_node);
        println!("[vaapi] GPU vendor: {gpu_vendor:?}");

        let render_node_c =
            std::ffi::CString::new(render_node.as_str()).map_err(|e| format!("CString: {e}"))?;

        let colorspace = Colorspace::for_dynamic_range(config.dynamic_range);

        // 1. Create VAAPI hardware device context
        let mut device_ctx_ptr: *mut ffi::AVBufferRef = ptr::null_mut();
        let ret = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device_ctx_ptr,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                render_node_c.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            return Err(format!(
                "av_hwdevice_ctx_create failed: {} (is VAAPI working on {render_node}?)",
                ffmpeg_err(ret)
            ));
        }
        let device_ctx = unsafe { HwBufRef::from_raw(device_ctx_ptr) };
        println!("[vaapi] Hardware device context created on {render_node}");

        // 2. Create hardware frames context
        let sw_format = colorspace.sw_pixel_format();

        let frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ctx.ptr) };
        if frames_ref.is_null() {
            return Err("av_hwframe_ctx_alloc returned null".into());
        }

        unsafe {
            let frames_ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
            (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*frames_ctx).sw_format = sw_format;
            (*frames_ctx).width = config.width as i32;
            (*frames_ctx).height = config.height as i32;
            (*frames_ctx).initial_pool_size = 0; // Lazy allocation (matching Sunshine)
        }

        let ret = unsafe { ffi::av_hwframe_ctx_init(frames_ref) };
        if ret < 0 {
            unsafe { ffi::av_buffer_unref(&mut { frames_ref }) };
            return Err(format!("av_hwframe_ctx_init failed: {}", ffmpeg_err(ret)));
        }
        let frames_ctx = unsafe { HwBufRef::from_raw(frames_ref) };
        println!(
            "[vaapi] Hardware frames context initialized ({}x{} {:?} {:?})",
            config.width, config.height, config.dynamic_range, config.codec
        );

        // 3. Open the selected codec's VAAPI encoder
        let codec_name = config.ffmpeg_vaapi_codec_name();
        let codec_name_c = std::ffi::CString::new(codec_name).unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(codec_name_c.as_ptr()) };
        if codec.is_null() {
            return Err(format!(
                "{codec_name} encoder not found (is FFmpeg built with VAAPI support for this codec?)"
            ));
        }

        // Profile selection per codec (highest quality first, matching Sunshine)
        let profiles = match config.codec {
            Codec::H264 => {
                const HIGH: i32 = 100;
                const MAIN: i32 = 77;
                const CB: i32 = 66 | (1 << 9);
                vec![("high", HIGH), ("main", MAIN), ("constrained_baseline", CB)]
            }
            Codec::Hevc => {
                if config.is_hdr() {
                    vec![("main10", 2)]
                } else {
                    vec![("main", 1), ("main10", 2)]
                }
            }
            Codec::Av1 => {
                vec![("main", 0)]
            }
        };

        // Try low-power entrypoint first on Intel (faster encoding), then normal
        let low_power_attempts = if gpu_vendor == GpuVendor::Intel {
            vec![true, false]
        } else {
            vec![false]
        };

        let mut codec_ctx: *mut ffi::AVCodecContext = ptr::null_mut();
        let mut last_err = String::new();

        'outer: for try_lp in &low_power_attempts {
            for (profile_name, profile_id) in &profiles {
                let ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
                if ctx.is_null() {
                    return Err("avcodec_alloc_context3 returned null".into());
                }

                unsafe {
                    (*ctx).width = config.width as i32;
                    (*ctx).height = config.height as i32;
                    (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                    (*ctx).time_base = ffi::AVRational {
                        num: 1,
                        den: config.framerate as i32,
                    };
                    (*ctx).framerate = ffi::AVRational {
                        num: config.framerate as i32,
                        den: 1,
                    };
                    (*ctx).gop_size = config.gop_size as i32;
                    (*ctx).max_b_frames = config.max_b_frames as i32;
                    (*ctx).bit_rate = config.bitrate_bps();
                    (*ctx).rc_buffer_size = config.vbv_buffer_size(false);
                    (*ctx).profile = *profile_id;
                    (*ctx).hw_device_ctx = ffi::av_buffer_ref(device_ctx.ptr);
                    (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ctx.ptr);

                    if config.low_delay {
                        (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
                    }
                    (*ctx).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;

                    colorspace.apply_to_codec_ctx(ctx);

                    // Rate control per vendor (matching Sunshine's vaapi.cpp)
                    match gpu_vendor {
                        GpuVendor::Intel => {
                            // Intel: VBR — only cap the max rate, single-frame VBV
                            (*ctx).rc_max_rate = config.bitrate_bps();
                        }
                        _ => {
                            // AMD/Other: CBR — pin min and max to bitrate
                            (*ctx).rc_min_rate = config.bitrate_bps();
                            (*ctx).rc_max_rate = config.bitrate_bps();
                        }
                    }

                    // Minimal encoder pipeline depth — only 1 frame in-flight
                    // (matches Sunshine video.cpp: async_depth=1 for all VAAPI encoders)
                    let async_key = std::ffi::CString::new("async_depth").unwrap();
                    let one = std::ffi::CString::new("1").unwrap();
                    ffi::av_opt_set((*ctx).priv_data, async_key.as_ptr(), one.as_ptr(), 0);

                    if matches!(config.codec, Codec::H264 | Codec::Hevc) {
                        let aud_key = std::ffi::CString::new("aud").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, aud_key.as_ptr(), one.as_ptr(), 0);
                    }

                    // Low-power entrypoint (Intel LP mode)
                    if *try_lp {
                        let lp_key = std::ffi::CString::new("low_power").unwrap();
                        let lp_val = std::ffi::CString::new("1").unwrap();
                        ffi::av_opt_set((*ctx).priv_data, lp_key.as_ptr(), lp_val.as_ptr(), 0);
                    }
                }

                let ret = unsafe { ffi::avcodec_open2(ctx, codec, ptr::null_mut()) };
                if ret == 0 {
                    let lp_str = if *try_lp { " low-power" } else { "" };
                    println!(
                        "[vaapi] {codec_name} encoder opened (profile: {profile_name}{lp_str})"
                    );
                    codec_ctx = ctx;
                    break 'outer;
                }

                last_err = format!("{profile_name}: {}", ffmpeg_err(ret));
                unsafe { ffi::avcodec_free_context(&mut { ctx }) };
            }
            if *try_lp {
                println!("[vaapi] Low-power entrypoint not available, trying normal...");
            }
        }

        if codec_ctx.is_null() {
            return Err(format!(
                "{codec_name}: no usable profile (last error: {last_err})"
            ));
        }

        // Build BGRA→NV12/P010 scaler for RAM frame fallback
        let dst_pixel = if config.is_hdr() {
            Pixel::P010LE
        } else {
            Pixel::NV12
        };

        let scaler = scaling::Context::get(
            Pixel::BGRA,
            config.width,
            config.height,
            dst_pixel,
            config.width,
            config.height,
            scaling::Flags::FAST_BILINEAR,
        )
        .ok();

        // Pre-allocate BGRA and NV12 frames for RAM path (avoid per-frame allocation)
        let bgra_frame = scaler
            .as_ref()
            .map(|_| VideoFrame::new(Pixel::BGRA, config.width, config.height));
        let nv12_frame = scaler
            .as_ref()
            .map(|_| VideoFrame::new(dst_pixel, config.width, config.height));

        Ok(Self {
            codec_ctx,
            _device_ctx: device_ctx,
            _frames_ctx: frames_ctx,
            scaler,
            gpu_vendor,
            colorspace,
            frame_index: 0,
            force_keyframe_next: false,
            width: config.width,
            height: config.height,
            bgra_frame,
            nv12_frame,
        })
    }

    /// Encode a captured frame (DMA-BUF or RAM), returning encoded NAL unit buffers.
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<Vec<u8>>, String> {
        match &frame.data {
            FrameData::DmaBuf { planes, drm_format } => {
                self.encode_dmabuf(planes, *drm_format, frame.width, frame.height)
            }
            FrameData::Ram(data) => self.encode_ram(data),
        }
    }

    /// Encode a DMA-BUF frame by importing it into VAAPI via DRM PRIME.
    fn encode_dmabuf(
        &mut self,
        planes: &[DmaBufPlane],
        _drm_format: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<Vec<u8>>, String> {
        unsafe {
            let mut desc: ffi::AVDRMFrameDescriptor = std::mem::zeroed();
            desc.nb_objects = planes.len() as i32;
            for (i, plane) in planes.iter().enumerate() {
                desc.objects[i].fd = plane.fd.as_raw_fd();
                desc.objects[i].size = 0;
                desc.objects[i].format_modifier = plane.modifier as u64;
            }
            desc.nb_layers = 1;
            desc.layers[0].format = _drm_format;
            desc.layers[0].nb_planes = planes.len() as i32;
            for (i, plane) in planes.iter().enumerate() {
                desc.layers[0].planes[i].object_index = i as i32;
                desc.layers[0].planes[i].offset = plane.offset as isize;
                desc.layers[0].planes[i].pitch = plane.pitch as isize;
            }

            let drm_frame = ffi::av_frame_alloc();
            if drm_frame.is_null() {
                return Err("av_frame_alloc (drm) failed".into());
            }
            (*drm_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*drm_frame).width = width as i32;
            (*drm_frame).height = height as i32;
            (*drm_frame).data[0] = &mut desc as *mut _ as *mut u8;
            let buf_ref = ffi::av_buffer_alloc(1);
            if buf_ref.is_null() {
                ffi::av_frame_free(&mut { drm_frame });
                return Err("av_buffer_alloc failed".into());
            }
            (*drm_frame).buf[0] = buf_ref;

            let vaapi_frame = ffi::av_frame_alloc();
            if vaapi_frame.is_null() {
                ffi::av_frame_free(&mut { drm_frame });
                return Err("av_frame_alloc (vaapi) failed".into());
            }
            (*vaapi_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;

            let ret = ffi::av_hwframe_get_buffer((*self.codec_ctx).hw_frames_ctx, vaapi_frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { drm_frame });
                ffi::av_frame_free(&mut { vaapi_frame });
                return Err(format!("av_hwframe_get_buffer failed: {}", ffmpeg_err(ret)));
            }

            let ret = ffi::av_hwframe_transfer_data(vaapi_frame, drm_frame, 0);
            ffi::av_frame_free(&mut { drm_frame });
            if ret < 0 {
                ffi::av_frame_free(&mut { vaapi_frame });
                return Err(format!(
                    "av_hwframe_transfer_data (dmabuf→vaapi) failed: {}",
                    ffmpeg_err(ret)
                ));
            }

            self.colorspace.apply_to_frame(vaapi_frame);
            (*vaapi_frame).pts = self.frame_index;
            self.frame_index += 1;
            if self.force_keyframe_next {
                (*vaapi_frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                self.force_keyframe_next = false;
            } else {
                (*vaapi_frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_NONE;
            }

            let result = self.send_and_receive(vaapi_frame);
            ffi::av_frame_free(&mut { vaapi_frame });
            result
        }
    }

    /// Encode a RAM (BGRA) frame by uploading to VAAPI via software conversion.
    fn encode_ram(&mut self, bgra_data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let bgra_frame = self
            .bgra_frame
            .as_mut()
            .ok_or("BGRA frame not initialized")?;

        // Reuse pre-allocated BGRA frame
        let dst_stride = bgra_frame.stride(0);
        let src_stride = (self.width as usize) * 4;

        if dst_stride == src_stride {
            let total = src_stride * self.height as usize;
            let usable = bgra_data.len().min(total);
            bgra_frame.data_mut(0)[..usable].copy_from_slice(&bgra_data[..usable]);
        } else {
            for row in 0..self.height as usize {
                let src_start = row * src_stride;
                let src_end = src_start + src_stride;
                let dst_start = row * dst_stride;
                if src_end <= bgra_data.len() {
                    bgra_frame.data_mut(0)[dst_start..dst_start + src_stride]
                        .copy_from_slice(&bgra_data[src_start..src_end]);
                }
            }
        }

        let scaler = self
            .scaler
            .as_mut()
            .ok_or("BGRA→NV12 scaler not initialized")?;
        let nv12_frame = self
            .nv12_frame
            .as_mut()
            .ok_or("NV12 frame not initialized")?;
        scaler
            .run(bgra_frame, nv12_frame)
            .map_err(|e| format!("scale: {e}"))?;

        unsafe {
            let vaapi_frame = ffi::av_frame_alloc();
            if vaapi_frame.is_null() {
                return Err("av_frame_alloc (vaapi) failed".into());
            }
            (*vaapi_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;

            let ret = ffi::av_hwframe_get_buffer((*self.codec_ctx).hw_frames_ctx, vaapi_frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { vaapi_frame });
                return Err(format!("av_hwframe_get_buffer failed: {}", ffmpeg_err(ret)));
            }

            let sw_frame = nv12_frame.as_mut_ptr();
            let ret = ffi::av_hwframe_transfer_data(vaapi_frame, sw_frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { vaapi_frame });
                return Err(format!(
                    "av_hwframe_transfer_data (ram→vaapi) failed: {}",
                    ffmpeg_err(ret)
                ));
            }

            self.colorspace.apply_to_frame(vaapi_frame);
            (*vaapi_frame).pts = self.frame_index;
            self.frame_index += 1;

            let result = self.send_and_receive(vaapi_frame);
            ffi::av_frame_free(&mut { vaapi_frame });
            result
        }
    }

    unsafe fn send_and_receive(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> Result<Vec<Vec<u8>>, String> {
        let ret = ffi::avcodec_send_frame(self.codec_ctx, frame);
        if ret < 0 {
            return Err(format!("avcodec_send_frame failed: {}", ffmpeg_err(ret)));
        }

        let mut nals = Vec::new();
        let pkt = ffi::av_packet_alloc();
        if pkt.is_null() {
            return Err("av_packet_alloc failed".into());
        }

        loop {
            let ret = ffi::avcodec_receive_packet(self.codec_ctx, pkt);
            if ret == averror(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                break;
            }
            if ret < 0 {
                ffi::av_packet_free(&mut { pkt });
                return Err(format!(
                    "avcodec_receive_packet failed: {}",
                    ffmpeg_err(ret)
                ));
            }
            let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
            nals.push(data.to_vec());
            ffi::av_packet_unref(pkt);
        }
        ffi::av_packet_free(&mut { pkt });

        Ok(nals)
    }

    /// Reset the encoder so the next frame is an IDR keyframe.
    pub fn reset_for_keyframe(&mut self) {
        self.force_keyframe_next = true;
        println!("[vaapi] next frame requested as IDR");
    }

    /// Best-effort in-place bitrate update for steady-state ABR changes.
    pub fn update_bitrate(&mut self, config: &EncoderConfig) -> Result<(), String> {
        if config.width != self.width || config.height != self.height {
            return Err("VAAPI bitrate update requires unchanged resolution".into());
        }

        let bitrate_bps = config.bitrate_bps();
        let buffer_size = config.vbv_buffer_size(false) as i64;
        unsafe {
            (*self.codec_ctx).bit_rate = bitrate_bps;
            (*self.codec_ctx).rc_buffer_size = config.vbv_buffer_size(false);
            match self.gpu_vendor {
                GpuVendor::Intel => {
                    (*self.codec_ctx).rc_min_rate = 0;
                    (*self.codec_ctx).rc_max_rate = bitrate_bps;
                }
                _ => {
                    (*self.codec_ctx).rc_min_rate = bitrate_bps;
                    (*self.codec_ctx).rc_max_rate = bitrate_bps;
                }
            }

            set_int_opt(self.codec_ctx.cast(), "b", bitrate_bps)?;
            set_int_opt(self.codec_ctx.cast(), "maxrate", bitrate_bps)?;
            set_int_opt(self.codec_ctx.cast(), "bufsize", buffer_size)?;
            if !matches!(self.gpu_vendor, GpuVendor::Intel) {
                set_int_opt(self.codec_ctx.cast(), "minrate", bitrate_bps)?;
            }
        }

        Ok(())
    }

    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        unsafe {
            let _ = ffi::avcodec_send_frame(self.codec_ctx, ptr::null());
        }
        let mut nals = Vec::new();
        unsafe {
            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                return nals;
            }
            loop {
                let ret = ffi::avcodec_receive_packet(self.codec_ctx, pkt);
                if ret < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                nals.push(data.to_vec());
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
        }
        nals
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        if !self.codec_ctx.is_null() {
            unsafe { ffi::avcodec_free_context(&mut self.codec_ctx) };
        }
    }
}

fn ffmpeg_err(code: i32) -> String {
    let mut buf = [0u8; 256];
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr() as *mut i8, buf.len());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).to_string()
}

fn averror(e: i32) -> i32 {
    -e
}

unsafe fn set_int_opt(target: *mut std::ffi::c_void, name: &str, value: i64) -> Result<(), String> {
    let key = CString::new(name).unwrap();
    let ret = ffi::av_opt_set_int(target, key.as_ptr(), value, 0);
    if ret >= 0 || ret == ffi::AVERROR_OPTION_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("{name}: {}", ffmpeg_err(ret)))
    }
}
