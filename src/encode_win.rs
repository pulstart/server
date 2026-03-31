/// Windows D3D11 hardware encoding via FFmpeg.
///
/// Captured DXGI textures stay on the GPU. Frames are converted from desktop
/// BGRA into NV12/P010 with the D3D11 video processor and handed to FFmpeg
/// hardware encoders without CPU readback.
use crate::capture::{CapturedFrame, D3D11FrameTexture, FrameData};
use crate::colorspace::Colorspace;
use crate::encode_config::{Codec, EncoderConfig};
use crate::transport::EncodedUnit;

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use std::ffi::{c_void, CString};
use std::mem::ManuallyDrop;
use std::ptr;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEX2D_ARRAY_VPOV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Dxgi::{
    Common::DXGI_RATIONAL, CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND,
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};

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

#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

#[repr(C)]
struct AVD3D11FrameDescriptor {
    texture: *mut c_void,
    index: isize,
}

#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut AVD3D11FrameDescriptor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsEncoderBackend {
    Nvenc,
    Amf,
    MediaFoundation,
}

impl WindowsEncoderBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "nvenc",
            Self::Amf => "amf",
            Self::MediaFoundation => "mf",
        }
    }

    fn codec_name(self, config: &EncoderConfig) -> &'static str {
        match self {
            Self::Nvenc => config.ffmpeg_nvenc_codec_name(),
            Self::Amf => config.ffmpeg_amf_codec_name(),
            Self::MediaFoundation => config.ffmpeg_mf_codec_name(),
        }
    }
}

/// Cross-adapter staging state: holds resources needed to copy frames from
/// the capture GPU to the encoder GPU via CPU memory.
struct CrossAdapterStaging {
    /// Staging texture on the CAPTURE device (GPU→CPU read).
    staging_texture: ID3D11Texture2D,
    /// Device context of the CAPTURE device (for CopyResource + Map).
    capture_context: ID3D11DeviceContext,
    /// Upload texture on the ENCODER device (CPU→GPU write).
    upload_texture: ID3D11Texture2D,
    /// Device context of the ENCODER device (for Map).
    encoder_context: ID3D11DeviceContext,
}

pub struct WindowsHwEncoder {
    backend: WindowsEncoderBackend,
    codec_ctx: *mut ffi::AVCodecContext,
    _device_ctx: HwBufRef,
    _frames_ctx: HwBufRef,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    colorspace: Colorspace,
    frame_index: i64,
    force_keyframe_next: bool,
    width: u32,
    height: u32,
    /// Present when encoder is on a different GPU than capture.
    cross_adapter: Option<CrossAdapterStaging>,
}

unsafe impl Send for WindowsHwEncoder {}

impl WindowsHwEncoder {
    pub fn backend(&self) -> WindowsEncoderBackend {
        self.backend
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.label()
    }

    pub fn with_config_and_backend(
        config: &EncoderConfig,
        capture_texture: &D3D11FrameTexture,
        backend: WindowsEncoderBackend,
    ) -> Result<Self, String> {
        if config.is_yuv444() {
            return Err("Windows hardware encoding currently supports YUV420 only".into());
        }
        if config.is_hdr() && backend == WindowsEncoderBackend::MediaFoundation {
            return Err("Media Foundation hardware encode is currently SDR-only".into());
        }

        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        let device: ID3D11Device = unsafe {
            capture_texture
                .texture
                .GetDevice()
                .map_err(|err| format!("ID3D11Texture2D::GetDevice failed: {err}"))?
        };
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let device_context = unsafe {
            device
                .GetImmediateContext()
                .map_err(|err| format!("ID3D11Device::GetImmediateContext failed: {err}"))?
        };
        let video_device = device
            .cast::<ID3D11VideoDevice>()
            .map_err(|err| format!("ID3D11Device->ID3D11VideoDevice cast failed: {err}"))?;
        let video_context = device_context
            .cast::<ID3D11VideoContext>()
            .map_err(|err| format!("ID3D11DeviceContext->ID3D11VideoContext cast failed: {err}"))?;

        let colorspace = Colorspace::for_dynamic_range(config.dynamic_range);
        let device_ctx = create_hw_device_ctx(&device)?;
        let frames_ctx = create_hw_frames_ctx(&device_ctx, config)?;
        let (processor_enum, processor) =
            create_video_processor(&video_device, &video_context, config)?;

        let codec_name = backend.codec_name(config);
        let codec_name_c = CString::new(codec_name).unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(codec_name_c.as_ptr()) };
        if codec.is_null() {
            return Err(format!(
                "{codec_name} encoder not found (is FFmpeg built with {codec_name}?)"
            ));
        }

        let ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 returned null".into());
        }

        unsafe {
            (*ctx).width = config.width as i32;
            (*ctx).height = config.height as i32;
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
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
            (*ctx).rc_min_rate = config.bitrate_bps();
            (*ctx).rc_max_rate = config.bitrate_bps();
            (*ctx).rc_buffer_size = config.vbv_buffer_size(false);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device_ctx.ptr);
            (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ctx.ptr);
            if config.low_delay {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            }
            (*ctx).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;

            match config.codec {
                Codec::H264 => {
                    (*ctx).profile = 100;
                }
                Codec::Hevc => {
                    (*ctx).profile = if config.is_hdr() { 2 } else { 1 };
                }
                Codec::Av1 => {
                    (*ctx).profile = 0;
                }
            }

            colorspace.apply_to_codec_ctx(ctx);
            apply_backend_options(ctx, backend, config)?;
        }

        let ret = unsafe { ffi::avcodec_open2(ctx, codec, ptr::null_mut()) };
        if ret < 0 {
            unsafe { ffi::avcodec_free_context(&mut { ctx }) };
            return Err(format!(
                "Failed to open {codec_name} encoder: {}",
                ffmpeg_err(ret)
            ));
        }

        println!(
            "[{}] {codec_name} encoder opened ({}x{}, {}kbps, {}fps)",
            backend.label(),
            config.width,
            config.height,
            config.bitrate_kbps,
            config.framerate
        );

        Ok(Self {
            backend,
            codec_ctx: ctx,
            _device_ctx: device_ctx,
            _frames_ctx: frames_ctx,
            video_device,
            video_context,
            processor_enum,
            processor,
            colorspace,
            frame_index: 0,
            force_keyframe_next: false,
            width: config.width,
            height: config.height,
            cross_adapter: None,
        })
    }

    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedUnit>, String> {
        match &frame.data {
            FrameData::D3D11Texture {
                texture,
                array_index,
            } => {
                if let Some(staging) = &self.cross_adapter {
                    // Cross-adapter path: capture GPU → staging → encoder GPU
                    let local_texture =
                        Self::stage_cross_adapter(&texture.texture, *array_index, staging)?;
                    self.encode_texture(&local_texture, 0)
                } else {
                    self.encode_texture(&texture.texture, *array_index)
                }
            }
            FrameData::Ram(_) => Err("Windows hardware encoder requires D3D11 capture frames".into()),
            #[cfg(target_os = "linux")]
            FrameData::DmaBuf { .. } => {
                Err("Windows hardware encoder does not support DMA-BUF input".into())
            }
        }
    }

    /// Copy a texture from the capture GPU to the encoder GPU via CPU staging.
    fn stage_cross_adapter(
        source: &ID3D11Texture2D,
        source_index: u32,
        staging: &CrossAdapterStaging,
    ) -> Result<ID3D11Texture2D, String> {
        unsafe {
            use windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE;

            // 1. Copy source texture to staging texture (both on capture GPU)
            // D3D11CalcSubresource(MipSlice, ArraySlice, MipLevels) = MipSlice + ArraySlice * MipLevels
            let src_sub = source_index; // mip 0, array=source_index, 1 mip level
            staging.capture_context.CopySubresourceRegion(
                &staging.staging_texture,
                0,
                0, 0, 0,
                source,
                src_sub,
                None,
            );

            // 2. Map staging texture for CPU read
            let mut mapped_src = D3D11_MAPPED_SUBRESOURCE::default();
            staging
                .capture_context
                .Map(&staging.staging_texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped_src))
                .map_err(|err| format!("Map staging texture failed: {err}"))?;

            // 3. Update encoder-side upload texture via UpdateSubresource
            let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
            staging.upload_texture.GetDesc(&mut tex_desc);
            let row_pitch = tex_desc.Width * 4; // BGRA = 4 bytes per pixel

            let box_region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: tex_desc.Width,
                bottom: tex_desc.Height,
                back: 1,
            };
            staging.encoder_context.UpdateSubresource(
                &staging.upload_texture,
                0,
                Some(&box_region),
                mapped_src.pData,
                mapped_src.RowPitch,
                0,
            );

            staging.capture_context.Unmap(&staging.staging_texture, 0);

            Ok(staging.upload_texture.clone())
        }
    }

    fn encode_texture(
        &mut self,
        source: &ID3D11Texture2D,
        source_array_index: u32,
    ) -> Result<Vec<EncodedUnit>, String> {
        unsafe {
            let hw_frame = ffi::av_frame_alloc();
            if hw_frame.is_null() {
                return Err("av_frame_alloc failed".into());
            }

            (*hw_frame).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32;
            (*hw_frame).width = self.width as i32;
            (*hw_frame).height = self.height as i32;

            let ret = ffi::av_hwframe_get_buffer((*self.codec_ctx).hw_frames_ctx, hw_frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { hw_frame });
                return Err(format!("av_hwframe_get_buffer failed: {}", ffmpeg_err(ret)));
            }

            let output_texture = borrowed_frame_texture(hw_frame)?;
            let output_array_index = (*hw_frame).data[1] as usize as u32;
            self.blit_texture(source, source_array_index, &output_texture, output_array_index)?;

            self.colorspace.apply_to_frame(hw_frame);
            (*hw_frame).pts = self.frame_index;
            self.frame_index += 1;
            if self.force_keyframe_next {
                (*hw_frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                self.force_keyframe_next = false;
            } else {
                (*hw_frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_NONE;
            }

            let result = self.send_and_receive(hw_frame);
            ffi::av_frame_free(&mut { hw_frame });
            result
        }
    }

    fn blit_texture(
        &self,
        source: &ID3D11Texture2D,
        source_array_index: u32,
        output_texture: &ID3D11Texture2D,
        output_array_index: u32,
    ) -> Result<(), String> {
        let input_view = create_input_view(
            &self.video_device,
            &self.processor_enum,
            source,
            source_array_index,
        )?;
        let output_view = create_output_view(
            &self.video_device,
            &self.processor_enum,
            output_texture,
            output_array_index,
        )?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view.clone())),
            ppFutureSurfaces: ptr::null_mut(),
            ppPastSurfacesRight: ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: ptr::null_mut(),
        };

        unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, std::slice::from_ref(&stream))
                .map_err(|err| format!("ID3D11VideoContext::VideoProcessorBlt failed: {err}"))?;
        }
        Ok(())
    }

    unsafe fn send_and_receive(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> Result<Vec<EncodedUnit>, String> {
        let ret = ffi::avcodec_send_frame(self.codec_ctx, frame);
        if ret < 0 {
            return Err(format!("avcodec_send_frame failed: {}", ffmpeg_err(ret)));
        }

        let mut encoded = Vec::new();
        let pkt = ffi::av_packet_alloc();
        if pkt.is_null() {
            return Err("av_packet_alloc failed".into());
        }

        loop {
            let ret = ffi::avcodec_receive_packet(self.codec_ctx, pkt);
            if ret == -ffi::EAGAIN || ret == ffi::AVERROR_EOF {
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
            encoded.push(EncodedUnit {
                data: data.to_vec(),
                is_recovery: ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0,
            });
            ffi::av_packet_unref(pkt);
        }
        ffi::av_packet_free(&mut { pkt });

        Ok(encoded)
    }

    pub fn reset_for_keyframe(&mut self) {
        self.force_keyframe_next = true;
        println!("[{}] next frame requested as IDR", self.backend.label());
    }

    pub fn update_bitrate(&mut self, config: &EncoderConfig) -> Result<(), String> {
        if config.width != self.width || config.height != self.height {
            return Err("hardware bitrate update requires unchanged resolution".into());
        }

        let bitrate_bps = config.bitrate_bps();
        let buffer_size = config.vbv_buffer_size(false) as i64;
        unsafe {
            (*self.codec_ctx).bit_rate = bitrate_bps;
            (*self.codec_ctx).rc_min_rate = bitrate_bps;
            (*self.codec_ctx).rc_max_rate = bitrate_bps;
            (*self.codec_ctx).rc_buffer_size = config.vbv_buffer_size(false);

            set_int_opt(self.codec_ctx.cast(), "b", bitrate_bps)?;
            set_int_opt(self.codec_ctx.cast(), "minrate", bitrate_bps)?;
            set_int_opt(self.codec_ctx.cast(), "maxrate", bitrate_bps)?;
            set_int_opt(self.codec_ctx.cast(), "bufsize", buffer_size)?;
        }

        Ok(())
    }

    pub fn flush(&mut self) -> Vec<EncodedUnit> {
        unsafe {
            let _ = ffi::avcodec_send_frame(self.codec_ctx, ptr::null());
        }
        let mut encoded = Vec::new();
        unsafe {
            let pkt = ffi::av_packet_alloc();
            if pkt.is_null() {
                return encoded;
            }
            loop {
                let ret = ffi::avcodec_receive_packet(self.codec_ctx, pkt);
                if ret < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                encoded.push(EncodedUnit {
                    data: data.to_vec(),
                    is_recovery: ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0,
                });
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
        }
        encoded
    }
}

impl Drop for WindowsHwEncoder {
    fn drop(&mut self) {
        if !self.codec_ctx.is_null() {
            unsafe { ffi::avcodec_free_context(&mut self.codec_ctx) };
        }
    }
}

/// Returns the preferred encoder backend order for the capture GPU.
///
/// On hybrid GPU laptops, the display may be on an Intel/AMD iGPU while an
/// NVIDIA dGPU is also present. We order the vendor-native backend first but
/// always include all backends so that cross-vendor encoders (if FFmpeg
/// supports them on this device) get a chance.
pub fn preferred_backend_order(
    capture_texture: &D3D11FrameTexture,
) -> Result<Vec<WindowsEncoderBackend>, String> {
    let device = unsafe {
        capture_texture
            .texture
            .GetDevice()
            .map_err(|err| format!("ID3D11Texture2D::GetDevice failed: {err}"))?
    };
    let dxgi_device = device
        .cast::<IDXGIDevice>()
        .map_err(|err| format!("ID3D11Device->IDXGIDevice cast failed: {err}"))?;
    let adapter: IDXGIAdapter = unsafe {
        dxgi_device
            .GetAdapter()
            .map_err(|err| format!("IDXGIDevice::GetAdapter failed: {err}"))?
    };
    let desc = unsafe {
        adapter
            .GetDesc()
            .map_err(|err| format!("IDXGIAdapter::GetDesc failed: {err}"))?
    };

    let adapter_name = String::from_utf16_lossy(
        &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len())],
    );
    let vendor_label = match desc.VendorId {
        0x10de => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x5143 | 0x4D4F4351 => "Qualcomm",
        _ => "Unknown",
    };
    println!("[encoder] Capture adapter: {adapter_name} (vendor: {vendor_label}, id: 0x{:04x})", desc.VendorId);

    use WindowsEncoderBackend::*;
    // Vendor-native first, then all others as fallbacks
    let order = match desc.VendorId {
        0x10de => vec![Nvenc, Amf, MediaFoundation],
        0x1002 | 0x1022 => vec![Amf, Nvenc, MediaFoundation],
        0x8086 => vec![MediaFoundation, Nvenc, Amf],
        0x5143 | 0x4D4F4351 => vec![MediaFoundation, Nvenc, Amf],
        _ => vec![Nvenc, Amf, MediaFoundation],
    };

    Ok(order)
}

/// An encoder backend available on a different adapter than the capture GPU.
pub struct CrossAdapterBackend {
    pub backend: WindowsEncoderBackend,
    pub adapter: IDXGIAdapter1,
    pub adapter_name: String,
}

/// Enumerate encoder backends available on adapters OTHER than the capture GPU.
///
/// On hybrid GPU laptops, the capture may be on an Intel/AMD iGPU while an
/// NVIDIA dGPU has NVENC. This function finds those alternate adapters.
pub fn cross_adapter_backends(
    capture_texture: &D3D11FrameTexture,
) -> Vec<CrossAdapterBackend> {
    let capture_device = match unsafe { capture_texture.texture.GetDevice() } {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let capture_dxgi = match capture_device.cast::<IDXGIDevice>() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let capture_adapter: IDXGIAdapter = match unsafe { capture_dxgi.GetAdapter() } {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    let capture_desc = match unsafe { capture_adapter.GetDesc() } {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let capture_luid = capture_desc.AdapterLuid;

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    let mut i = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => a,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(_) => break,
        };
        i += 1;

        let desc = match unsafe { adapter.GetDesc() } {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip the capture adapter itself
        if desc.AdapterLuid.LowPart == capture_luid.LowPart
            && desc.AdapterLuid.HighPart == capture_luid.HighPart
        {
            continue;
        }
        // Skip software adapters
        if desc.VendorId == 0x1414 {
            continue;
        }

        let name = String::from_utf16_lossy(
            &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len())],
        );

        let backends: Vec<WindowsEncoderBackend> = match desc.VendorId {
            0x10de => vec![WindowsEncoderBackend::Nvenc],
            0x1002 | 0x1022 => vec![WindowsEncoderBackend::Amf],
            0x8086 => vec![WindowsEncoderBackend::MediaFoundation],
            _ => continue,
        };

        for backend in backends {
            println!(
                "[encoder] Cross-adapter candidate: {name} (vendor: 0x{:04x}) — {}",
                desc.VendorId,
                backend.label()
            );
            results.push(CrossAdapterBackend {
                backend,
                adapter: adapter.clone(),
                adapter_name: name.clone(),
            });
        }
    }
    results
}

impl WindowsHwEncoder {
    /// Create a hardware encoder on a DIFFERENT adapter than the capture device.
    ///
    /// Frames are staged through CPU memory: capture GPU → staging (CPU read) →
    /// encoder GPU upload texture → video processor → encode.
    /// This is slower than same-adapter encode but much faster than full
    /// software encoding, and enables NVENC on hybrid GPU laptops where the
    /// display is on an Intel/AMD iGPU.
    pub fn with_config_cross_adapter(
        config: &EncoderConfig,
        capture_texture: &D3D11FrameTexture,
        target_adapter: &IDXGIAdapter1,
        backend: WindowsEncoderBackend,
        adapter_name: &str,
    ) -> Result<Self, String> {
        if config.is_yuv444() {
            return Err("Windows hardware encoding currently supports YUV420 only".into());
        }
        if config.is_hdr() && backend == WindowsEncoderBackend::MediaFoundation {
            return Err("Media Foundation hardware encode is currently SDR-only".into());
        }

        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        // Create a new D3D11 device on the target (encoder) adapter
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut enc_device = None;
        let mut enc_context = None;
        unsafe {
            D3D11CreateDevice(
                target_adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut enc_device),
                None,
                Some(&mut enc_context),
            )
            .map_err(|err| format!("D3D11CreateDevice on {adapter_name} failed: {err}"))?;
        }

        let device = enc_device.ok_or("D3D11CreateDevice returned null device")?;
        let _enc_ctx = enc_context.ok_or("D3D11CreateDevice returned null context")?;

        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let device_context = unsafe {
            device
                .GetImmediateContext()
                .map_err(|err| format!("GetImmediateContext failed: {err}"))?
        };
        let video_device = device
            .cast::<ID3D11VideoDevice>()
            .map_err(|err| format!("ID3D11VideoDevice cast failed: {err}"))?;
        let video_context = device_context
            .cast::<ID3D11VideoContext>()
            .map_err(|err| format!("ID3D11VideoContext cast failed: {err}"))?;

        let colorspace = Colorspace::for_dynamic_range(config.dynamic_range);
        let device_ctx = create_hw_device_ctx(&device)?;
        let frames_ctx = create_hw_frames_ctx(&device_ctx, config)?;
        let (processor_enum, processor) =
            create_video_processor(&video_device, &video_context, config)?;

        // Set up cross-adapter staging: capture GPU → CPU → encoder GPU
        let capture_device: ID3D11Device = unsafe {
            capture_texture.texture.GetDevice()
                .map_err(|err| format!("GetDevice (capture) failed: {err}"))?
        };
        let capture_context = unsafe {
            capture_device.GetImmediateContext()
                .map_err(|err| format!("GetImmediateContext (capture) failed: {err}"))?
        };

        let bgra_format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
        let sample_desc = windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };

        // Staging texture on capture device (GPU → CPU read)
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: config.width, Height: config.height,
            MipLevels: 1, ArraySize: 1,
            Format: bgra_format, SampleDesc: sample_desc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let staging_texture = unsafe {
            let mut tex = None;
            capture_device.CreateTexture2D(&staging_desc, None, Some(&mut tex))
                .map_err(|err| format!("CreateTexture2D (staging) failed: {err}"))?;
            tex.ok_or("staging texture is null")?
        };

        // Upload texture on encoder device (CPU → GPU write)
        let upload_desc = D3D11_TEXTURE2D_DESC {
            Width: config.width, Height: config.height,
            MipLevels: 1, ArraySize: 1,
            Format: bgra_format, SampleDesc: sample_desc,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let upload_texture = unsafe {
            let mut tex = None;
            device.CreateTexture2D(&upload_desc, None, Some(&mut tex))
                .map_err(|err| format!("CreateTexture2D (upload) failed: {err}"))?;
            tex.ok_or("upload texture is null")?
        };

        let codec_name = backend.codec_name(config);
        let codec_name_c = CString::new(codec_name).unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(codec_name_c.as_ptr()) };
        if codec.is_null() {
            return Err(format!("{codec_name} encoder not found"));
        }

        let ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 returned null".into());
        }

        unsafe {
            (*ctx).width = config.width as i32;
            (*ctx).height = config.height as i32;
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
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
            (*ctx).rc_min_rate = config.bitrate_bps();
            (*ctx).rc_max_rate = config.bitrate_bps();
            (*ctx).rc_buffer_size = config.vbv_buffer_size(false);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(device_ctx.ptr);
            (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ctx.ptr);
            if config.low_delay {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            }
            (*ctx).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;

            match config.codec {
                Codec::H264 => { (*ctx).profile = 100; }
                Codec::Hevc => { (*ctx).profile = if config.is_hdr() { 2 } else { 1 }; }
                Codec::Av1 => { (*ctx).profile = 0; }
            }

            colorspace.apply_to_codec_ctx(ctx);
            apply_backend_options(ctx, backend, config)?;

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!(
                    "avcodec_open2 failed for {codec_name}: {}",
                    ffmpeg_err(ret)
                ));
            }
        }

        println!(
            "[encoder] Cross-adapter {codec_name} encoder opened on {adapter_name} ({}x{}, {}kbps, {}fps) — staging via CPU",
            config.width, config.height, config.bitrate_kbps, config.framerate
        );

        Ok(Self {
            backend,
            codec_ctx: ctx,
            _device_ctx: device_ctx,
            _frames_ctx: frames_ctx,
            video_device,
            video_context,
            processor_enum,
            processor,
            colorspace,
            frame_index: 0,
            force_keyframe_next: false,
            width: config.width,
            height: config.height,
            cross_adapter: Some(CrossAdapterStaging {
                staging_texture,
                capture_context,
                upload_texture,
                encoder_context: device_context,
            }),
        })
    }
}

fn create_hw_device_ctx(device: &ID3D11Device) -> Result<HwBufRef, String> {
    let mut device_ref =
        unsafe { ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA) };
    if device_ref.is_null() {
        return Err("av_hwdevice_ctx_alloc returned null".into());
    }

    let result = unsafe {
        let device_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
        let d3d11_ctx = (*device_ctx).hwctx as *mut AVD3D11VADeviceContext;
        ptr::write_bytes(d3d11_ctx, 0, 1);
        (*d3d11_ctx).device = device.clone().into_raw();
        (*d3d11_ctx).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
        (*d3d11_ctx).misc_flags = 0;

        let ret = ffi::av_hwdevice_ctx_init(device_ref);
        if ret < 0 {
            Err(format!("av_hwdevice_ctx_init failed: {}", ffmpeg_err(ret)))
        } else {
            Ok(())
        }
    };

    match result {
        Ok(()) => Ok(unsafe { HwBufRef::from_raw(device_ref) }),
        Err(err) => {
            unsafe {
                ffi::av_buffer_unref(&mut device_ref);
            }
            Err(err)
        }
    }
}

fn create_hw_frames_ctx(
    device_ctx: &HwBufRef,
    config: &EncoderConfig,
) -> Result<HwBufRef, String> {
    let frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ctx.ptr) };
    if frames_ref.is_null() {
        return Err("av_hwframe_ctx_alloc returned null".into());
    }

    let result = unsafe {
        let frames_ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*frames_ctx).sw_format = Colorspace::for_dynamic_range(config.dynamic_range).sw_pixel_format();
        (*frames_ctx).width = config.width as i32;
        (*frames_ctx).height = config.height as i32;
        (*frames_ctx).initial_pool_size = 1;

        let d3d11_frames = (*frames_ctx).hwctx as *mut AVD3D11VAFramesContext;
        ptr::write_bytes(d3d11_frames, 0, 1);
        (*d3d11_frames).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
        (*d3d11_frames).misc_flags = 0;

        let ret = ffi::av_hwframe_ctx_init(frames_ref);
        if ret < 0 {
            Err(format!("av_hwframe_ctx_init failed: {}", ffmpeg_err(ret)))
        } else {
            Ok(())
        }
    };

    match result {
        Ok(()) => Ok(unsafe { HwBufRef::from_raw(frames_ref) }),
        Err(err) => {
            unsafe {
                ffi::av_buffer_unref(&mut { frames_ref });
            }
            Err(err)
        }
    }
}

fn create_video_processor(
    video_device: &ID3D11VideoDevice,
    video_context: &ID3D11VideoContext,
    config: &EncoderConfig,
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor), String> {
    let frame_rate = config.framerate.max(1);
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: frame_rate,
            Denominator: 1,
        },
        InputWidth: config.width,
        InputHeight: config.height,
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: frame_rate,
            Denominator: 1,
        },
        OutputWidth: config.width,
        OutputHeight: config.height,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };

    let enumerator = unsafe {
        video_device
            .CreateVideoProcessorEnumerator(&desc)
            .map_err(|err| format!("CreateVideoProcessorEnumerator failed: {err}"))?
    };
    let processor = unsafe {
        video_device
            .CreateVideoProcessor(&enumerator, 0)
            .map_err(|err| format!("CreateVideoProcessor failed: {err}"))?
    };

    let rect = RECT {
        left: 0,
        top: 0,
        right: config.width as i32,
        bottom: config.height as i32,
    };
    unsafe {
        video_context.VideoProcessorSetOutputTargetRect(
            &processor,
            true,
            Some(&rect as *const RECT),
        );
        video_context.VideoProcessorSetStreamSourceRect(
            &processor,
            0,
            true,
            Some(&rect as *const RECT),
        );
    }

    Ok((enumerator, processor))
}

fn create_input_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
    array_index: u32,
) -> Result<ID3D11VideoProcessorInputView, String> {
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: array_index,
            },
        },
    };

    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorInputView(texture, processor_enum, &desc, Some(&mut view))
            .map_err(|err| format!("CreateVideoProcessorInputView failed: {err}"))?;
    }
    view.ok_or_else(|| "CreateVideoProcessorInputView returned null view".into())
}

fn create_output_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
    array_index: u32,
) -> Result<ID3D11VideoProcessorOutputView, String> {
    let mut texture_desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut texture_desc);
    }

    let desc = if texture_desc.ArraySize > 1 || array_index > 0 {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                    MipSlice: 0,
                    FirstArraySlice: array_index,
                    ArraySize: 1,
                },
            },
        }
    } else {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        }
    };

    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorOutputView(texture, processor_enum, &desc, Some(&mut view))
            .map_err(|err| format!("CreateVideoProcessorOutputView failed: {err}"))?;
    }
    view.ok_or_else(|| "CreateVideoProcessorOutputView returned null view".into())
}

unsafe fn borrowed_frame_texture(frame: *mut ffi::AVFrame) -> Result<ID3D11Texture2D, String> {
    let raw = (*frame).data[0] as *mut c_void;
    ID3D11Texture2D::from_raw_borrowed(&raw)
        .cloned()
        .ok_or_else(|| "AVFrame D3D11 output texture was null".into())
}

unsafe fn apply_backend_options(
    ctx: *mut ffi::AVCodecContext,
    backend: WindowsEncoderBackend,
    config: &EncoderConfig,
) -> Result<(), String> {
    match backend {
        WindowsEncoderBackend::Nvenc => {
            set_str_opt((*ctx).priv_data, "preset", config.quality.nvenc_preset())?;
            set_str_opt((*ctx).priv_data, "tune", config.quality.nvenc_tune())?;
            set_str_opt((*ctx).priv_data, "rc", "cbr")?;
            set_int_opt((*ctx).priv_data, "delay", 0)?;
            set_int_opt((*ctx).priv_data, "forced-idr", 1)?;
            set_int_opt((*ctx).priv_data, "zerolatency", 1)?;
            set_int_opt((*ctx).priv_data, "surfaces", 1)?;
            set_int_opt((*ctx).priv_data, "cbr_padding", 0)?;
            set_int_opt((*ctx).priv_data, "rc-lookahead", 0)?;
            if matches!(config.codec, Codec::H264 | Codec::Hevc) {
                set_int_opt((*ctx).priv_data, "aud", 1)?;
            }
        }
        WindowsEncoderBackend::Amf => {
            set_int_opt((*ctx).priv_data, "filler_data", 0)?;
            set_int_opt((*ctx).priv_data, "forced_idr", 1)?;
            set_int_opt((*ctx).priv_data, "async_depth", 1)?;
            set_str_opt((*ctx).priv_data, "rc", "cbr")?;
            set_int_opt((*ctx).priv_data, "skip_frame", 0)?;
            set_int_opt((*ctx).priv_data, "frame_skipping", 0)?;
            if config.codec == Codec::Hevc {
                set_str_opt((*ctx).priv_data, "header_insertion_mode", "idr")?;
            }
        }
        WindowsEncoderBackend::MediaFoundation => {
            set_int_opt((*ctx).priv_data, "hw_encoding", 1)?;
            set_str_opt((*ctx).priv_data, "rate_control", "cbr")?;
            set_str_opt((*ctx).priv_data, "scenario", "display_remoting")?;
        }
    }
    Ok(())
}

fn ffmpeg_err(code: i32) -> String {
    let mut buf = [0u8; 256];
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr() as *mut i8, buf.len());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).to_string()
}

unsafe fn set_int_opt(target: *mut c_void, name: &str, value: i64) -> Result<(), String> {
    let key = CString::new(name).unwrap();
    let ret = ffi::av_opt_set_int(target, key.as_ptr(), value, 0);
    if ret >= 0 || ret == ffi::AVERROR_OPTION_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("{name}: {}", ffmpeg_err(ret)))
    }
}

unsafe fn set_str_opt(target: *mut c_void, name: &str, value: &str) -> Result<(), String> {
    let key = CString::new(name).unwrap();
    let value = CString::new(value).unwrap();
    let ret = ffi::av_opt_set(target, key.as_ptr(), value.as_ptr(), 0);
    if ret >= 0 || ret == ffi::AVERROR_OPTION_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("{name}: {}", ffmpeg_err(ret)))
    }
}
