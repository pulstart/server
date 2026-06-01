/// Software encoding fallback (libx264 / libx265 / libsvtav1).
///
/// Used when no hardware encoder is available.
/// This is the last-resort fallback — works on any system with FFmpeg built with
/// the corresponding codec libraries.
use crate::capture::{CapturedFrame, FrameData};
use crate::colorspace::Colorspace;
use crate::encode_config::{Codec, EncoderConfig};
use crate::transport::EncodedUnit;

extern crate ffmpeg_next as ffmpeg;
extern crate ffmpeg_sys_next as ffi;

use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as VideoFrame;

use std::ffi::CString;
use std::ptr;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

pub struct SoftwareEncoder {
    codec_ctx: *mut ffi::AVCodecContext,
    scaler: scaling::Context,
    colorspace: Colorspace,
    frame_index: i64,
    force_keyframe_next: bool,
    width: u32,
    height: u32,
    yuv_frame: VideoFrame,
    bgra_frame: VideoFrame,
    #[cfg(target_os = "windows")]
    staging_texture: Option<ID3D11Texture2D>,
}

unsafe impl Send for SoftwareEncoder {}

impl SoftwareEncoder {
    pub fn with_config(config: &EncoderConfig) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        let codec_name = config.ffmpeg_software_codec_name();
        let codec_name_c = std::ffi::CString::new(codec_name).unwrap();
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

        let colorspace = Colorspace::for_dynamic_range(config.dynamic_range);

        if config.is_yuv444() && config.is_hdr() {
            return Err("Software YUV444 HDR encoding is not implemented".into());
        }
        if config.is_yuv444() && config.codec == Codec::Av1 {
            return Err("Software AV1 YUV444 encoding is not implemented".into());
        }

        let sw_pix_fmt = if config.is_yuv444() {
            ffi::AVPixelFormat::AV_PIX_FMT_YUV444P
        } else if config.is_hdr() {
            ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_YUV420P
        };

        unsafe {
            (*ctx).width = config.width as i32;
            (*ctx).height = config.height as i32;
            (*ctx).pix_fmt = sw_pix_fmt;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: config.framerate as i32,
            };
            (*ctx).framerate = ffi::AVRational {
                num: config.framerate as i32,
                den: 1,
            };
            // gop_size=0 means all-intra in libx264/libx265 — the config layer
            // already maps "infinite" to EncoderConfig::INFINITE_GOP (i32::MAX),
            // so a literal 0 here would be an unintended all-intra request; guard
            // it back to infinite. libx264/libx265/SVT-AV1 all treat a very large
            // keyint as effectively infinite (keyframes come on demand only).
            (*ctx).gop_size = if config.gop_size == 0 {
                EncoderConfig::INFINITE_GOP as i32
            } else {
                config.gop_size as i32
            };
            (*ctx).max_b_frames = config.max_b_frames as i32;
            (*ctx).bit_rate = config.bitrate_bps();
            (*ctx).rc_buffer_size = config.vbv_buffer_size(true);

            if config.low_delay {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            }
            (*ctx).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;

            // Set profile based on codec
            match config.codec {
                Codec::H264 => {
                    (*ctx).profile = if config.is_yuv444() {
                        ffi::FF_PROFILE_H264_HIGH_444_PREDICTIVE
                    } else {
                        100 // High
                    };
                }
                Codec::Hevc => {
                    (*ctx).profile = if config.is_yuv444() {
                        ffi::FF_PROFILE_HEVC_REXT
                    } else if config.is_hdr() {
                        2 // Main 10
                    } else {
                        1 // Main
                    };
                }
                Codec::Av1 => {
                    (*ctx).profile = 0; // Main
                }
            }

            // Set thread count for software encoding
            (*ctx).thread_count = 0; // auto-detect

            // Apply colorspace metadata
            colorspace.apply_to_codec_ctx(ctx);

            // Per-codec min-QP floor (C3).
            if let Some(qmin) = config.min_qp() {
                (*ctx).qmin = qmin as i32;
            }

            // Multi-slice encoding (C2): x264/x265 honor avctx->slices.
            (*ctx).slices = config.slices_per_frame() as i32;
        }

        // Codec-specific options (preset driven by quality setting)
        match config.codec {
            Codec::H264 => {
                let preset = std::ffi::CString::new("preset").unwrap();
                let preset_val = std::ffi::CString::new(config.quality.sw_x26x_preset()).unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, preset.as_ptr(), preset_val.as_ptr(), 0);
                }
                let tune = std::ffi::CString::new("tune").unwrap();
                let zerolatency = std::ffi::CString::new("zerolatency").unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, tune.as_ptr(), zerolatency.as_ptr(), 0);
                }
                let forced_idr = std::ffi::CString::new("forced-idr").unwrap();
                let one = std::ffi::CString::new("1").unwrap();
                let x264_params = std::ffi::CString::new("x264-params").unwrap();
                // A3 (opt-in): periodic intra-refresh wave. x264 emits a
                // recovery_point SEI at each wave end, which the client now parses
                // to exit recovery without a full-IDR spike.
                let mut params = String::from("repeat-headers=1:aud=1");
                if config.intra_refresh_enabled() {
                    params.push_str(":intra-refresh=1");
                    println!("[sw] x264 intra-refresh enabled (ST_INTRA_REFRESH)");
                }
                let stream_params = std::ffi::CString::new(params).unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, forced_idr.as_ptr(), one.as_ptr(), 0);
                    ffi::av_opt_set(
                        (*ctx).priv_data,
                        x264_params.as_ptr(),
                        stream_params.as_ptr(),
                        0,
                    );
                    // H.264 entropy coder (F1). CABAC default; ST_H264_CODER=cavlc.
                    if let Some(coder) = config.h264_coder() {
                        let coder_key = std::ffi::CString::new("coder").unwrap();
                        let coder_val = std::ffi::CString::new(coder).unwrap();
                        ffi::av_opt_set(
                            (*ctx).priv_data,
                            coder_key.as_ptr(),
                            coder_val.as_ptr(),
                            0,
                        );
                    }
                }
            }
            Codec::Hevc => {
                let preset = std::ffi::CString::new("preset").unwrap();
                let preset_val = std::ffi::CString::new(config.quality.sw_x26x_preset()).unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, preset.as_ptr(), preset_val.as_ptr(), 0);
                }
                let tune = std::ffi::CString::new("tune").unwrap();
                let zerolatency = std::ffi::CString::new("zerolatency").unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, tune.as_ptr(), zerolatency.as_ptr(), 0);
                }
                let forced_idr = std::ffi::CString::new("forced-idr").unwrap();
                let one = std::ffi::CString::new("1").unwrap();
                let x265_params = std::ffi::CString::new("x265-params").unwrap();
                let stream_params = std::ffi::CString::new("repeat-headers=1:aud=1").unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, forced_idr.as_ptr(), one.as_ptr(), 0);
                    ffi::av_opt_set(
                        (*ctx).priv_data,
                        x265_params.as_ptr(),
                        stream_params.as_ptr(),
                        0,
                    );
                }
            }
            Codec::Av1 => {
                let preset = std::ffi::CString::new("preset").unwrap();
                let preset_val = std::ffi::CString::new(config.quality.sw_svtav1_preset()).unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, preset.as_ptr(), preset_val.as_ptr(), 0);
                }
            }
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
            "[sw] {codec_name} encoder opened ({}x{}, {}kbps, {}fps)",
            config.width, config.height, config.bitrate_kbps, config.framerate
        );

        // Build BGRA→YUV scaler
        let dst_pixel = if config.is_yuv444() {
            Pixel::YUV444P
        } else if config.is_hdr() {
            Pixel::YUV420P10LE
        } else {
            Pixel::YUV420P
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
        .map_err(|e| format!("scaler: {e}"))?;

        let yuv_frame = VideoFrame::new(dst_pixel, config.width, config.height);
        let bgra_frame = VideoFrame::new(Pixel::BGRA, config.width, config.height);

        Ok(Self {
            codec_ctx: ctx,
            scaler,
            colorspace,
            frame_index: 0,
            force_keyframe_next: false,
            width: config.width,
            height: config.height,
            yuv_frame,
            bgra_frame,
            #[cfg(target_os = "windows")]
            staging_texture: None,
        })
    }

    /// Encode a captured frame. DMA-BUF frames are read back directly into the
    /// pre-allocated BGRA frame (single copy, no intermediate Vec).
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedUnit>, String> {
        match &frame.data {
            FrameData::Ram(data) => self.fill_bgra_from_slice(data),
            #[cfg(target_os = "windows")]
            FrameData::D3D11Texture {
                texture,
                array_index,
            } => {
                self.fill_bgra_from_d3d11(texture, *array_index, frame.width, frame.height)?;
            }
            #[cfg(target_os = "linux")]
            FrameData::DmaBuf {
                planes, drm_format, ..
            } => {
                self.fill_bgra_from_dmabuf(planes, *drm_format, frame.width, frame.height)?;
            }
        }

        // Scale BGRA → YUV420P (reuses both frames)
        self.scaler
            .run(&self.bgra_frame, &mut self.yuv_frame)
            .map_err(|e| format!("scale: {e}"))?;

        self.yuv_frame.set_pts(Some(self.frame_index));
        self.frame_index += 1;

        // Apply colorspace metadata and send to encoder
        unsafe {
            let frame_ptr = self.yuv_frame.as_mut_ptr();
            self.colorspace.apply_to_frame(frame_ptr);
            if self.force_keyframe_next {
                (*frame_ptr).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                self.force_keyframe_next = false;
            } else {
                (*frame_ptr).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_NONE;
            }
            self.send_and_receive(frame_ptr)
        }
    }

    unsafe fn send_and_receive(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> Result<Vec<EncodedUnit>, String> {
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
            nals.push(EncodedUnit {
                data: data.to_vec(),
                is_recovery: ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0,
            });
            ffi::av_packet_unref(pkt);
        }
        ffi::av_packet_free(&mut { pkt });

        Ok(nals)
    }

    /// Copy RAM pixel data into the pre-allocated BGRA frame.
    fn fill_bgra_from_slice(&mut self, data: &[u8]) {
        let dst_stride = self.bgra_frame.stride(0);
        let src_stride = (self.width as usize) * 4;

        if dst_stride == src_stride {
            let total = src_stride * self.height as usize;
            let usable = data.len().min(total);
            self.bgra_frame.data_mut(0)[..usable].copy_from_slice(&data[..usable]);
        } else {
            for row in 0..self.height as usize {
                let src_start = row * src_stride;
                let src_end = src_start + src_stride;
                let dst_start = row * dst_stride;
                if src_end <= data.len() {
                    self.bgra_frame.data_mut(0)[dst_start..dst_start + src_stride]
                        .copy_from_slice(&data[src_start..src_end]);
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn fill_bgra_from_d3d11(
        &mut self,
        texture: &crate::capture::D3D11FrameTexture,
        array_index: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let source = &texture.texture;
        let device = unsafe {
            source
                .GetDevice()
                .map_err(|err| format!("ID3D11Texture2D::GetDevice failed: {err}"))?
        };
        let context = unsafe {
            device
                .GetImmediateContext()
                .map_err(|err| format!("ID3D11Device::GetImmediateContext failed: {err}"))?
        };

        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            source.GetDesc(&mut source_desc);
        }

        let recreate_staging = self
            .staging_texture
            .as_ref()
            .map(|staging| {
                let mut staging_desc = D3D11_TEXTURE2D_DESC::default();
                unsafe {
                    staging.GetDesc(&mut staging_desc);
                }
                staging_desc.Width != source_desc.Width
                    || staging_desc.Height != source_desc.Height
                    || staging_desc.Format != source_desc.Format
            })
            .unwrap_or(true);

        if recreate_staging {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: source_desc.Width,
                Height: source_desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: source_desc.Format,
                SampleDesc: source_desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe {
                device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .map_err(|err| format!("CreateTexture2D for staging failed: {err}"))?;
            }
            self.staging_texture = Some(
                staging.ok_or_else(|| "CreateTexture2D for staging returned null".to_string())?,
            );
        }

        let staging = self
            .staging_texture
            .as_ref()
            .ok_or_else(|| "staging texture missing after creation".to_string())?;
        unsafe {
            if array_index == 0 && source_desc.ArraySize <= 1 {
                context.CopyResource(staging, source);
            } else {
                let source_subresource = array_index.saturating_mul(source_desc.MipLevels);
                context.CopySubresourceRegion(
                    staging,
                    0,
                    0,
                    0,
                    0,
                    source,
                    source_subresource,
                    None,
                );
            }
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|err| format!("ID3D11DeviceContext::Map failed: {err}"))?;
        }

        let result = (|| -> Result<(), String> {
            let src_stride = mapped.RowPitch as usize;
            let row_bytes = (width as usize) * 4;
            let dst_stride = self.bgra_frame.stride(0);
            let src = mapped.pData as *const u8;

            for row in 0..height as usize {
                let src_row =
                    unsafe { std::slice::from_raw_parts(src.add(row * src_stride), row_bytes) };
                let dst_start = row * dst_stride;
                self.bgra_frame.data_mut(0)[dst_start..dst_start + row_bytes]
                    .copy_from_slice(src_row);
            }
            Ok(())
        })();

        unsafe {
            context.Unmap(staging, 0);
        }
        result
    }

    /// Read DMA-BUF pixels directly into the pre-allocated BGRA frame via mmap.
    #[cfg(target_os = "linux")]
    fn fill_bgra_from_dmabuf(
        &mut self,
        planes: &[crate::capture::DmaBufPlane],
        _drm_format: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        use std::os::fd::AsRawFd;

        if planes.is_empty() {
            return Err("DMA-BUF has no planes".into());
        }

        let plane = &planes[0];
        let pitch = plane.pitch as usize;
        let row_bytes = (width as usize) * 4;
        let total_size = plane.offset as usize + pitch * height as usize;

        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total_size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                plane.fd.as_raw_fd(),
                0,
            )
        };

        if mapped == libc::MAP_FAILED {
            return Err(format!(
                "mmap DMA-BUF failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let sync_start: u64 = 5;
        let sync_end: u64 = 2 | 4;

        nix::ioctl_write_ptr_bad!(dma_buf_sync, 0x4008_6200u64, u64);

        unsafe {
            let _ = dma_buf_sync(plane.fd.as_raw_fd(), &sync_start);
        }

        let src = (mapped as *const u8).wrapping_add(plane.offset as usize);
        let dst_stride = self.bgra_frame.stride(0);

        if pitch == dst_stride {
            let total = dst_stride * height as usize;
            let src_slice = unsafe { std::slice::from_raw_parts(src, total) };
            self.bgra_frame.data_mut(0)[..total].copy_from_slice(src_slice);
        } else {
            for row in 0..height as usize {
                let src_row =
                    unsafe { std::slice::from_raw_parts(src.add(row * pitch), row_bytes) };
                let dst_start = row * dst_stride;
                self.bgra_frame.data_mut(0)[dst_start..dst_start + row_bytes]
                    .copy_from_slice(src_row);
            }
        }

        unsafe {
            let _ = dma_buf_sync(plane.fd.as_raw_fd(), &sync_end);
            libc::munmap(mapped, total_size);
        }

        Ok(())
    }

    /// Reset the encoder so the next frame is an IDR keyframe.
    pub fn reset_for_keyframe(&mut self) {
        self.force_keyframe_next = true;
        println!("[software] next frame requested as IDR");
    }

    /// Best-effort in-place bitrate update for software ABR changes.
    pub fn update_bitrate(&mut self, config: &EncoderConfig) -> Result<(), String> {
        if config.width != self.width || config.height != self.height {
            return Err("software bitrate update requires unchanged resolution".into());
        }

        let bitrate_bps = config.bitrate_bps();
        let buffer_size = config.vbv_buffer_size(true) as i64;
        unsafe {
            (*self.codec_ctx).bit_rate = bitrate_bps;
            (*self.codec_ctx).rc_min_rate = bitrate_bps;
            (*self.codec_ctx).rc_max_rate = bitrate_bps;
            (*self.codec_ctx).rc_buffer_size = config.vbv_buffer_size(true);

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
                nals.push(EncodedUnit {
                    data: data.to_vec(),
                    is_recovery: ((*pkt).flags & ffi::AV_PKT_FLAG_KEY) != 0,
                });
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
        }
        nals
    }
}

impl Drop for SoftwareEncoder {
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

unsafe fn set_int_opt(target: *mut std::ffi::c_void, name: &str, value: i64) -> Result<(), String> {
    let key = CString::new(name).unwrap();
    let ret = ffi::av_opt_set_int(target, key.as_ptr(), value, 0);
    if ret >= 0 || ret == ffi::AVERROR_OPTION_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("{name}: {}", ffmpeg_err(ret)))
    }
}

#[cfg(test)]
mod inplace_bitrate_tests {
    use super::*;

    // Encode `count` frames of changing content and return the average packet
    // size in bytes. Mirrors what update_bitrate's av_opt_set path relies on.
    unsafe fn avg_packet_bytes(
        ctx: *mut ffi::AVCodecContext,
        frame: *mut ffi::AVFrame,
        start_pts: i64,
        count: i64,
    ) -> f64 {
        let mut total: u64 = 0;
        let mut packets: u64 = 0;
        let pkt = ffi::av_packet_alloc();
        for i in 0..count {
            let y = (*frame).data[0];
            let ls = (*frame).linesize[0] as usize;
            let h = (*frame).height as usize;
            let iu = i as usize;
            for row in 0..h {
                let base = y.add(row * ls);
                for x in 0..ls {
                    *base.add(x) = (((iu * 7 + row * 3 + x) & 0xff) as u8).wrapping_mul(3);
                }
            }
            (*frame).pts = start_pts + i;
            ffi::avcodec_send_frame(ctx, frame);
            loop {
                let r = ffi::avcodec_receive_packet(ctx, pkt);
                if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF || r < 0 {
                    break;
                }
                total += (*pkt).size as u64;
                packets += 1;
                ffi::av_packet_unref(pkt);
            }
        }
        ffi::av_packet_free(&mut { pkt });
        if packets == 0 {
            0.0
        } else {
            total as f64 / packets as f64
        }
    }

    // Regression guard: libx264 must honor a runtime bitrate change through the
    // same av_opt_set("b"/"maxrate"/"bufsize") mechanism update_bitrate uses. If
    // a future ffmpeg/option change silently breaks this, ABR would stop working
    // on the software path and this test fails loudly. (The BitrateVerifier in
    // main.rs is the runtime safety net for backends that DON'T honor it.)
    #[test]
    fn libx264_honors_runtime_bitrate_change() {
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(c"libx264".as_ptr());
            if codec.is_null() {
                eprintln!("libx264 not available; skipping");
                return;
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).width = 640;
            (*ctx).height = 360;
            (*ctx).time_base = ffi::AVRational { num: 1, den: 60 };
            (*ctx).framerate = ffi::AVRational { num: 60, den: 1 };
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*ctx).gop_size = i32::MAX;
            (*ctx).max_b_frames = 0;
            let low_bps: i64 = 1_000_000;
            let high_bps: i64 = 20_000_000;
            (*ctx).bit_rate = low_bps;
            (*ctx).rc_max_rate = low_bps;
            (*ctx).rc_buffer_size = (low_bps / 60) as i32;
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();
            ffi::av_opt_set((*ctx).priv_data, c"preset".as_ptr(), preset.as_ptr(), 0);
            ffi::av_opt_set((*ctx).priv_data, c"tune".as_ptr(), tune.as_ptr(), 0);
            assert!(
                ffi::avcodec_open2(ctx, codec, ptr::null_mut()) >= 0,
                "open libx264"
            );

            let frame = ffi::av_frame_alloc();
            (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*frame).width = 640;
            (*frame).height = 360;
            ffi::av_frame_get_buffer(frame, 32);

            let before = avg_packet_bytes(ctx, frame, 0, 90);

            (*ctx).bit_rate = high_bps;
            (*ctx).rc_max_rate = high_bps;
            (*ctx).rc_buffer_size = (high_bps / 60) as i32;
            set_int_opt(ctx.cast(), "b", high_bps).unwrap();
            set_int_opt(ctx.cast(), "maxrate", high_bps).unwrap();
            set_int_opt(ctx.cast(), "bufsize", high_bps / 60).unwrap();

            let after = avg_packet_bytes(ctx, frame, 1000, 90);

            ffi::av_frame_free(&mut { frame });
            ffi::avcodec_free_context(&mut { ctx });

            assert!(
                after > before * 1.5,
                "libx264 ignored runtime bitrate change: {before:.0} -> {after:.0} bytes/pkt"
            );
        }
    }
}

#[cfg(test)]
mod slice_qp_tests {
    use super::*;

    // Count H.264 slice NAL units (type 1 = non-IDR coded slice, 5 = IDR slice)
    // in an Annex-B bitstream by scanning 3-/4-byte start codes.
    fn count_slice_nals(data: &[u8]) -> usize {
        let mut count = 0usize;
        let mut i = 0usize;
        while i + 3 < data.len() {
            let sc4 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
            let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
            let (hdr, adv) = if sc4 {
                (data.get(i + 4).copied(), 4)
            } else if sc3 {
                (data.get(i + 3).copied(), 3)
            } else {
                i += 1;
                continue;
            };
            if let Some(b) = hdr {
                let nal_type = b & 0x1f;
                if nal_type == 1 || nal_type == 5 {
                    count += 1;
                }
            }
            i += adv;
        }
        count
    }

    // C2 regression guard: avctx->slices must reach libx264's slice mode so each
    // frame is split into multiple slice NALs. The client runs FF_THREAD_SLICE
    // and a lost packet then corrupts only one slice rather than the whole frame.
    // If a future ffmpeg/x264 change stops honoring avctx->slices this fails loud.
    #[test]
    fn libx264_emits_multiple_slices_per_frame() {
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(c"libx264".as_ptr());
            if codec.is_null() {
                eprintln!("libx264 not available; skipping");
                return;
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).width = 640;
            (*ctx).height = 480;
            (*ctx).time_base = ffi::AVRational { num: 1, den: 60 };
            (*ctx).framerate = ffi::AVRational { num: 60, den: 1 };
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*ctx).gop_size = i32::MAX;
            (*ctx).max_b_frames = 0;
            (*ctx).bit_rate = 4_000_000;
            (*ctx).slices = 4; // exactly the field encode_sw sets from slices_per_frame
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();
            ffi::av_opt_set((*ctx).priv_data, c"preset".as_ptr(), preset.as_ptr(), 0);
            ffi::av_opt_set((*ctx).priv_data, c"tune".as_ptr(), tune.as_ptr(), 0);
            assert!(
                ffi::avcodec_open2(ctx, codec, ptr::null_mut()) >= 0,
                "open libx264"
            );

            let frame = ffi::av_frame_alloc();
            (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*frame).width = 640;
            (*frame).height = 480;
            ffi::av_frame_get_buffer(frame, 32);
            // gradient luma so the slices carry residual (not skip-only)
            let y = (*frame).data[0];
            let ls = (*frame).linesize[0] as usize;
            for row in 0..480usize {
                let base = y.add(row * ls);
                for x in 0..ls {
                    *base.add(x) = ((row * 3 + x) & 0xff) as u8;
                }
            }
            (*frame).pts = 0;

            let pkt = ffi::av_packet_alloc();
            ffi::avcodec_send_frame(ctx, frame);
            ffi::avcodec_send_frame(ctx, ptr::null_mut()); // flush
            let mut slice_nals = 0usize;
            loop {
                let r = ffi::avcodec_receive_packet(ctx, pkt);
                if r < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                slice_nals += count_slice_nals(data);
                ffi::av_packet_unref(pkt);
            }
            ffi::av_packet_free(&mut { pkt });
            ffi::av_frame_free(&mut { frame });
            ffi::avcodec_free_context(&mut { ctx });

            assert!(
                slice_nals > 1,
                "expected >1 slice NAL with avctx->slices=4, got {slice_nals}"
            );
        }
    }

    // Encode `count` frames of active (changing) content at a generous CBR budget
    // and return average packet bytes. With no min-QP, rate control pours the
    // budget into low-QP frames; a min-QP floor caps QP and curbs that spend.
    unsafe fn avg_bytes_with_qmin(qmin: i32, count: i64) -> f64 {
        let codec = ffi::avcodec_find_encoder_by_name(c"libx264".as_ptr());
        let ctx = ffi::avcodec_alloc_context3(codec);
        (*ctx).width = 640;
        (*ctx).height = 480;
        (*ctx).time_base = ffi::AVRational { num: 1, den: 60 };
        (*ctx).framerate = ffi::AVRational { num: 60, den: 1 };
        (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
        (*ctx).gop_size = i32::MAX;
        (*ctx).max_b_frames = 0;
        let bps: i64 = 50_000_000; // generous, so qmin=0 would pick a very low QP
        (*ctx).bit_rate = bps;
        (*ctx).rc_max_rate = bps;
        (*ctx).rc_buffer_size = (bps / 60) as i32;
        if qmin > 0 {
            (*ctx).qmin = qmin; // exactly the field encode_sw sets from min_qp()
        }
        let preset = CString::new("ultrafast").unwrap();
        let tune = CString::new("zerolatency").unwrap();
        ffi::av_opt_set((*ctx).priv_data, c"preset".as_ptr(), preset.as_ptr(), 0);
        ffi::av_opt_set((*ctx).priv_data, c"tune".as_ptr(), tune.as_ptr(), 0);
        assert!(
            ffi::avcodec_open2(ctx, codec, ptr::null_mut()) >= 0,
            "open libx264"
        );

        let frame = ffi::av_frame_alloc();
        (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
        (*frame).width = 640;
        (*frame).height = 480;
        ffi::av_frame_get_buffer(frame, 32);

        let pkt = ffi::av_packet_alloc();
        let mut total: u64 = 0;
        let mut packets: u64 = 0;
        let ls = (*frame).linesize[0] as usize;
        let h = (*frame).height as usize;
        for i in 0..count {
            let y = (*frame).data[0];
            let iu = i as usize;
            for row in 0..h {
                let base = y.add(row * ls);
                for x in 0..ls {
                    *base.add(x) = (((iu * 7 + row * 3 + x) & 0xff) as u8).wrapping_mul(3);
                }
            }
            (*frame).pts = i;
            ffi::avcodec_send_frame(ctx, frame);
            loop {
                let r = ffi::avcodec_receive_packet(ctx, pkt);
                if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF || r < 0 {
                    break;
                }
                total += (*pkt).size as u64;
                packets += 1;
                ffi::av_packet_unref(pkt);
            }
        }
        ffi::av_packet_free(&mut { pkt });
        ffi::av_frame_free(&mut { frame });
        ffi::avcodec_free_context(&mut { ctx });
        if packets == 0 {
            0.0
        } else {
            total as f64 / packets as f64
        }
    }

    // C3 regression guard: the per-codec min-QP floor must actually reach the
    // encoder and curb CBR over-spend. At a generous budget, no-floor encodes
    // low-QP (large) frames; a qmin floor caps QP so average packet bytes must
    // drop. Mirrors Sunshine's enableMinQP. If (*ctx).qmin stops being honored
    // this fails loud.
    #[test]
    fn min_qp_floor_curbs_overspend() {
        unsafe {
            if ffi::avcodec_find_encoder_by_name(c"libx264".as_ptr()).is_null() {
                eprintln!("libx264 not available; skipping");
                return;
            }
            let no_floor = avg_bytes_with_qmin(0, 60);
            let with_floor = avg_bytes_with_qmin(35, 60);
            assert!(
                with_floor < no_floor,
                "min-QP floor did not curb over-spend: {no_floor:.0} -> {with_floor:.0} bytes/pkt"
            );
        }
    }
}
