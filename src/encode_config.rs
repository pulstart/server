/// Unified encoder configuration, matching Sunshine's `video::config_t`.
///
/// Negotiated between client and server before the stream starts.
/// All encoder backends read from this struct instead of using hardcoded values.
///
/// Environment variable overrides (matching Sunshine's config approach):
///   ST_CODEC=auto|h264|hevc|av1  — prefer a video codec (default: auto)
///   ST_HDR=1                     — enable HDR (10-bit BT.2020+PQ)
///   ST_CHROMA=yuv444             — use YUV 4:4:4 chroma sampling
///   ST_BITRATE=50000             — starting video bitrate in Kbps
///   ST_MIN_BITRATE=5000          — adaptive bitrate floor in Kbps
///   ST_MAX_BITRATE=100000        — adaptive bitrate ceiling in Kbps
///   ST_FPS=60                    — max/forced framerate (caps client request)
///   ST_GOP=120                   — max frames between keyframes (0 = infinite GOP)
///   ST_AUDIO=stereo|high_stereo|surround51|high_surround51|surround71|high_surround71
use st_protocol::{StreamConfig, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    LowLatency,
    Balanced,
    HighQuality,
}

impl QualityPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::LowLatency => "Low Latency",
            Self::Balanced => "Balanced",
            Self::HighQuality => "High Quality",
        }
    }

    pub fn nvenc_preset(self) -> &'static str {
        match self {
            Self::LowLatency => "p1",
            Self::Balanced => "p4",
            Self::HighQuality => "p7",
        }
    }

    pub fn nvenc_tune(self) -> &'static str {
        match self {
            Self::LowLatency => "ull",
            Self::Balanced => "ll",
            Self::HighQuality => "hq",
        }
    }

    pub fn sw_x26x_preset(self) -> &'static str {
        match self {
            Self::LowLatency => "ultrafast",
            Self::Balanced => "veryfast",
            Self::HighQuality => "medium",
        }
    }

    pub fn sw_svtav1_preset(self) -> &'static str {
        match self {
            Self::LowLatency => "12",
            Self::Balanced => "8",
            Self::HighQuality => "4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "h264" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            "auto" | "" => None,
            _ => None,
        }
    }

    pub fn preferred_order(explicit: Option<Self>) -> [Self; 3] {
        let default = [Self::Av1, Self::Hevc, Self::H264];
        match explicit {
            Some(codec) => match codec {
                Self::Av1 => [Self::Av1, Self::Hevc, Self::H264],
                Self::Hevc => [Self::Hevc, Self::Av1, Self::H264],
                Self::H264 => [Self::H264, Self::Hevc, Self::Av1],
            },
            None => default,
        }
    }

    pub fn to_stream_codec(self) -> VideoCodec {
        match self {
            Self::H264 => VideoCodec::H264,
            Self::Hevc => VideoCodec::Hevc,
            Self::Av1 => VideoCodec::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicRange {
    Sdr,
    Hdr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaSampling {
    Yuv420,
    Yuv444,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub codec: Codec,
    pub dynamic_range: DynamicRange,
    pub chroma: ChromaSampling,
    pub gop_size: u32,
    pub max_b_frames: u32,
    pub low_delay: bool,
    pub quality: QualityPreset,
}

impl EncoderConfig {
    const DEFAULT_BITRATE_KBPS: u32 = 50_000;
    const DEFAULT_MIN_BITRATE_KBPS: u32 = 5_000;
    const DEFAULT_MAX_BITRATE_KBPS: u32 = 100_000;
    const DEFAULT_FRAMERATE: u32 = 60;
    const MAX_NEGOTIATED_FPS: u32 = 360;

    fn default_gop_size(framerate: u32) -> u32 {
        framerate.clamp(30, 120)
    }

    /// Build config for the given resolution, reading overrides from env vars.
    pub fn from_env(width: u32, height: u32) -> Self {
        Self::from_env_with_framerate_and_codec(
            width,
            height,
            Self::resolve_target_fps(None),
            Self::preferred_codec_from_env().unwrap_or(Codec::H264),
        )
    }

    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn from_env_with_framerate(width: u32, height: u32, framerate: u32) -> Self {
        Self::from_env_with_framerate_and_codec(
            width,
            height,
            framerate,
            Self::preferred_codec_from_env().unwrap_or(Codec::H264),
        )
    }

    pub fn from_env_with_framerate_and_codec(
        width: u32,
        height: u32,
        framerate: u32,
        codec: Codec,
    ) -> Self {

        let dynamic_range = if std::env::var("ST_HDR").unwrap_or_default() == "1" {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        };

        let chroma = if std::env::var("ST_CHROMA").unwrap_or_default() == "yuv444" {
            ChromaSampling::Yuv444
        } else {
            ChromaSampling::Yuv420
        };

        let max_bitrate_kbps = std::env::var("ST_MAX_BITRATE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_MAX_BITRATE_KBPS)
            .max(250);
        let min_bitrate_kbps = std::env::var("ST_MIN_BITRATE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_MIN_BITRATE_KBPS)
            .min(max_bitrate_kbps)
            .max(250);
        let bitrate_kbps = std::env::var("ST_BITRATE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_BITRATE_KBPS)
            .clamp(min_bitrate_kbps, max_bitrate_kbps);
        let gop_size = std::env::var("ST_GOP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or_else(|| Self::default_gop_size(framerate));

        Self {
            width,
            height,
            framerate: framerate.clamp(1, Self::MAX_NEGOTIATED_FPS),
            bitrate_kbps,
            min_bitrate_kbps,
            max_bitrate_kbps,
            codec,
            dynamic_range,
            chroma,
            gop_size,
            max_b_frames: 0,
            low_delay: true,
            quality: QualityPreset::Balanced,
        }
    }

    pub fn preferred_codec_from_env() -> Option<Codec> {
        std::env::var("ST_CODEC")
            .ok()
            .and_then(|value| Codec::from_env_value(&value))
    }

    pub fn preferred_codec_order_from_env() -> [Codec; 3] {
        Codec::preferred_order(Self::preferred_codec_from_env())
    }

    pub fn fps_cap_from_env() -> Option<u32> {
        std::env::var("ST_FPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|fps| *fps > 0)
            .map(|fps| fps.min(Self::MAX_NEGOTIATED_FPS))
    }

    pub fn resolve_target_fps(client_requested_fps: Option<u32>) -> u32 {
        let client_requested_fps = client_requested_fps
            .filter(|fps| *fps > 0)
            .map(|fps| fps.min(Self::MAX_NEGOTIATED_FPS));

        match (Self::fps_cap_from_env(), client_requested_fps) {
            (Some(cap), Some(requested)) => requested.min(cap),
            (Some(cap), None) => cap,
            (None, Some(requested)) => requested,
            (None, None) => Self::DEFAULT_FRAMERATE,
        }
    }

    pub fn bitrate_bps(&self) -> i64 {
        self.bitrate_kbps as i64 * 1000
    }

    pub fn with_bitrate_kbps(&self, bitrate_kbps: u32) -> Self {
        let mut next = self.clone();
        next.bitrate_kbps = bitrate_kbps.clamp(self.min_bitrate_kbps, self.max_bitrate_kbps);
        next
    }

    pub fn is_hdr(&self) -> bool {
        self.dynamic_range == DynamicRange::Hdr
    }

    pub fn is_yuv444(&self) -> bool {
        self.chroma == ChromaSampling::Yuv444
    }

    /// Compute VBV buffer size in bits (Sunshine style: bitrate / fps for HW, larger for SW).
    pub fn vbv_buffer_size(&self, is_software: bool) -> i32 {
        let bps = self.bitrate_bps();
        let size = if is_software {
            bps / ((self.framerate as i64 * 10) / 15)
        } else {
            bps / self.framerate as i64
        };
        size as i32
    }

    pub fn ffmpeg_vaapi_codec_name(&self) -> &'static str {
        match self.codec {
            Codec::H264 => "h264_vaapi",
            Codec::Hevc => "hevc_vaapi",
            Codec::Av1 => "av1_vaapi",
        }
    }

    pub fn ffmpeg_nvenc_codec_name(&self) -> &'static str {
        match self.codec {
            Codec::H264 => "h264_nvenc",
            Codec::Hevc => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }

    pub fn ffmpeg_software_codec_name(&self) -> &'static str {
        match self.codec {
            Codec::H264 => "libx264",
            Codec::Hevc => "libx265",
            Codec::Av1 => "libsvtav1",
        }
    }

    pub fn stream_codec(&self) -> VideoCodec {
        self.codec.to_stream_codec()
    }

    pub fn to_stream_config(&self, audio: &AudioConfig) -> StreamConfig {
        StreamConfig {
            codec: self.stream_codec(),
            width: self.width,
            height: self.height,
            framerate: self.framerate.min(u16::MAX as u32) as u16,
            audio_sample_rate: audio.sample_rate,
            audio_channels: audio.channels.min(u8::MAX as u32) as u8,
            hdr: self.is_hdr(),
        }
    }
}

/// Audio stream configuration, matching Sunshine's `opus_stream_config_t`.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate: u32,
    pub packet_duration_ms: u32,
}

impl AudioConfig {
    /// Build audio config from ST_AUDIO env var.
    pub fn from_env() -> Self {
        match std::env::var("ST_AUDIO")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "high_stereo" => Self::high_stereo(),
            "surround51" | "5.1" => Self::surround51(),
            "high_surround51" | "high_5.1" => Self::high_surround51(),
            "surround71" | "7.1" => Self::surround71(),
            "high_surround71" | "high_7.1" => Self::high_surround71(),
            _ => Self::stereo(),
        }
    }

    pub fn stereo() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bitrate: 96_000,
            packet_duration_ms: 20,
        }
    }

    pub fn high_stereo() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bitrate: 512_000,
            packet_duration_ms: 20,
        }
    }

    pub fn surround51() -> Self {
        Self {
            sample_rate: 48000,
            channels: 6,
            bitrate: 256_000,
            packet_duration_ms: 20,
        }
    }

    pub fn high_surround51() -> Self {
        Self {
            sample_rate: 48000,
            channels: 6,
            bitrate: 1_536_000,
            packet_duration_ms: 20,
        }
    }

    pub fn surround71() -> Self {
        Self {
            sample_rate: 48000,
            channels: 8,
            bitrate: 450_000,
            packet_duration_ms: 20,
        }
    }

    pub fn high_surround71() -> Self {
        Self {
            sample_rate: 48000,
            channels: 8,
            bitrate: 2_048_000,
            packet_duration_ms: 20,
        }
    }

    pub fn samples_per_frame(&self) -> u32 {
        self.packet_duration_ms * self.sample_rate / 1000
    }

    pub fn total_samples_per_frame(&self) -> usize {
        (self.samples_per_frame() * self.channels) as usize
    }
}
