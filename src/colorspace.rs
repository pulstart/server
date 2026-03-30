/// Colorspace definitions matching Sunshine's `video_colorspace.cpp`.
///
/// Defines RGB-to-YUV conversion parameters for SDR and HDR.
/// Used by encoder backends to set AVFrame colorspace metadata.

#[cfg(any(target_os = "linux", target_os = "windows"))]
extern crate ffmpeg_sys_next as ffi;

use crate::encode_config::DynamicRange;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorStandard {
    Bt709,
    Bt2020Hdr,
}

#[derive(Debug, Clone, Copy)]
pub struct Colorspace {
    pub standard: ColorStandard,
    pub bit_depth: u32,
}

impl Colorspace {
    /// SDR Rec.709 with limited range, 8-bit — the most common desktop colorspace.
    pub fn sdr_rec709() -> Self {
        Self {
            standard: ColorStandard::Bt709,
            bit_depth: 8,
        }
    }

    /// HDR BT.2020 with limited range, 10-bit.
    pub fn hdr_bt2020() -> Self {
        Self {
            standard: ColorStandard::Bt2020Hdr,
            bit_depth: 10,
        }
    }

    /// Select the appropriate colorspace based on dynamic range.
    pub fn for_dynamic_range(dr: DynamicRange) -> Self {
        match dr {
            DynamicRange::Sdr => Self::sdr_rec709(),
            DynamicRange::Hdr => Self::hdr_bt2020(),
        }
    }

    /// Apply colorspace metadata to an FFmpeg AVFrame.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub unsafe fn apply_to_frame(&self, frame: *mut ffi::AVFrame) {
        match self.standard {
            ColorStandard::Bt709 => {
                (*frame).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
                (*frame).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
                (*frame).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            }
            ColorStandard::Bt2020Hdr => {
                (*frame).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
                (*frame).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
                (*frame).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTEST2084;
            }
        }

        // Streaming always uses limited (MPEG) range
        (*frame).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
    }

    /// Apply colorspace metadata to an FFmpeg AVCodecContext.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub unsafe fn apply_to_codec_ctx(&self, ctx: *mut ffi::AVCodecContext) {
        match self.standard {
            ColorStandard::Bt709 => {
                (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
                (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
                (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            }
            ColorStandard::Bt2020Hdr => {
                (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
                (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
                (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTEST2084;
            }
        }

        // Streaming always uses limited (MPEG) range
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
    }

    /// Software pixel format for this colorspace.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn sw_pixel_format(&self) -> ffi::AVPixelFormat {
        if self.bit_depth > 8 {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        }
    }
}
