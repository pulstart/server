/// Software encoding fallback (libx264 / libx265 / libsvtav1).
///
/// Matches Sunshine's software encoder path. Used when no hardware encoder is available.
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

        let sw_pix_fmt = if config.is_hdr() {
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
            // gop_size=0 means all-intra in libx264/libx265; use 250 as fallback
            (*ctx).gop_size = if config.gop_size == 0 {
                250
            } else {
                config.gop_size as i32
            };
            (*ctx).max_b_frames = config.max_b_frames as i32;
            (*ctx).bit_rate = config.bitrate_bps();
            (*ctx).rc_buffer_size = config.vbv_buffer_size(true) as i32;

            if config.low_delay {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            }
            (*ctx).flags |= ffi::AV_CODEC_FLAG_CLOSED_GOP as i32;

            // Set profile based on codec
            match config.codec {
                Codec::H264 => {
                    (*ctx).profile = if config.is_yuv444() {
                        244 // High 4:4:4 Predictive
                    } else {
                        100 // High
                    };
                }
                Codec::Hevc => {
                    (*ctx).profile = if config.is_hdr() {
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
                let stream_params = std::ffi::CString::new("repeat-headers=1:aud=1").unwrap();
                unsafe {
                    ffi::av_opt_set((*ctx).priv_data, forced_idr.as_ptr(), one.as_ptr(), 0);
                    ffi::av_opt_set((*ctx).priv_data, x264_params.as_ptr(), stream_params.as_ptr(), 0);
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
                    ffi::av_opt_set((*ctx).priv_data, x265_params.as_ptr(), stream_params.as_ptr(), 0);
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
        let dst_pixel = if config.is_hdr() {
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
        })
    }

    /// Encode a captured frame. DMA-BUF frames are read back directly into the
    /// pre-allocated BGRA frame (single copy, no intermediate Vec).
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedUnit>, String> {
        match &frame.data {
            FrameData::Ram(data) => self.fill_bgra_from_slice(data),
            FrameData::DmaBuf { planes, drm_format } => {
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

    /// Read DMA-BUF pixels directly into the pre-allocated BGRA frame via mmap.
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
            (*self.codec_ctx).rc_buffer_size = config.vbv_buffer_size(true) as i32;

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
