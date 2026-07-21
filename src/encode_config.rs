/// Unified encoder configuration.
///
/// Negotiated between client and server before the stream starts.
/// All encoder backends read from this struct instead of using hardcoded values.
///
/// Environment variable overrides:
///   ST_CODEC=auto|h264|hevc|av1  — prefer a video codec (default: auto)
///   ST_HDR=1                     — enable HDR (10-bit BT.2020+PQ)
///   ST_CHROMA=auto|yuv420|yuv444 — chroma sampling preference (default: auto)
///   ST_BITRATE=50000             — starting video bitrate in Kbps
///   ST_MIN_BITRATE=5000          — adaptive bitrate floor in Kbps
///   ST_MAX_BITRATE=100000        — adaptive bitrate ceiling in Kbps
///   ST_FPS=60                    — max/forced framerate (caps client request)
///   ST_GOP=120                   — max frames between keyframes (0 = infinite GOP)
///   ST_AUDIO=stereo|high_stereo|surround51|high_surround51|surround71|high_surround71
use st_protocol::{StreamConfig, VideoChromaSampling, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    LowLatency,
    Balanced,
    HighQuality,
}

impl QualityPreset {
    /// Parse `ST_QUALITY` (F3): `low`/`latency`, `balanced`, `high`/`quality`.
    /// `None` when unset so the caller keeps its default. Tray/control-socket
    /// `forced_quality` still overrides this at runtime.
    pub fn from_env() -> Option<Self> {
        match std::env::var("ST_QUALITY")
            .ok()?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "low" | "lowlatency" | "low-latency" | "latency" | "ll" => Some(Self::LowLatency),
            "balanced" | "balance" | "med" | "medium" => Some(Self::Balanced),
            "high" | "highquality" | "high-quality" | "quality" | "hq" => Some(Self::HighQuality),
            _ => None,
        }
    }

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
    // Start conservatively so weak Wi-Fi can lock in and let ABR probe upward.
    const DEFAULT_BITRATE_KBPS: u32 = 20_000;
    const DEFAULT_MIN_BITRATE_KBPS: u32 = 5_000;
    const DEFAULT_MAX_BITRATE_KBPS: u32 = 100_000;
    const DEFAULT_FRAMERATE: u32 = 60;
    const MAX_NEGOTIATED_FPS: u32 = 360;
    const SVTAV1_MIN_BUFFER_MS: i64 = 20;
    /// Effectively-infinite GOP. Keyframes are emitted on demand only
    /// (subscriber join, loss recovery, output switch) via `reset_for_keyframe`,
    /// never on a fixed periodic interval. A periodic IDR every `framerate`
    /// frames combined with the 1-frame VBV (`vbv_buffer_size`) forces the rate
    /// controller to either crush IDR quality or burst past the pacing budget
    /// once per second — the visible "periodic refresh"/pulsing artifact.
    pub const INFINITE_GOP: u32 = i32::MAX as u32;

    fn default_gop_size(_framerate: u32) -> u32 {
        // Infinite GOP is the documented design ("infinite GOP, IDR on demand").
        // Keyframes are forced on demand; no fixed periodic IDR.
        Self::INFINITE_GOP
    }

    /// Build config for the given resolution, reading overrides from env vars.
    #[cfg(target_os = "linux")]
    pub fn from_env(width: u32, height: u32) -> Self {
        Self::from_env_with_framerate_and_codec(
            width,
            height,
            Self::resolve_target_fps(None),
            Self::preferred_codec_from_env().unwrap_or(Codec::H264),
        )
    }

    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
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

        let chroma = Self::preferred_chroma_from_env().unwrap_or(ChromaSampling::Yuv420);

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
        // ST_GOP overrides the keyframe interval in frames. `0` means infinite
        // GOP (on-demand keyframes only), matching the default. Any positive
        // value forces a periodic IDR every N frames (debugging / lossy paths).
        let gop_size = std::env::var("ST_GOP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| if v == 0 { Self::INFINITE_GOP } else { v })
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
            // Latency-first default: this is a low-latency streaming server, so an
            // unforced session encodes with the LowLatency preset (NVENC p1/ull,
            // x26x ultrafast, svtav1 preset 12) rather than Balanced — the p4→p1
            // GPU-compute drop alone shaves ~0.3-1.5ms/frame on NVENC. The tray /
            // control-socket `forced_quality` and `ST_QUALITY` still override this.
            quality: QualityPreset::from_env().unwrap_or(QualityPreset::LowLatency),
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

    pub fn preferred_chroma_from_env() -> Option<ChromaSampling> {
        match std::env::var("ST_CHROMA")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "yuv444" => Some(ChromaSampling::Yuv444),
            "yuv420" => Some(ChromaSampling::Yuv420),
            "auto" | "" => None,
            _ => None,
        }
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

    fn buffer_size_for_duration_ms(&self, duration_ms: i64) -> i64 {
        self.bitrate_bps().saturating_mul(duration_ms) / 1000
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

    /// Per-codec minimum-QP floor (mirrors Sunshine's `enableMinQP`: h264=19,
    /// hevc=23, av1=23). Under CBR/VBV a static scene otherwise drives QP toward
    /// 0, over-spending bits and producing a visible quality pulse. Always-on;
    /// `ST_MIN_QP=0|off|false|no` disables, a numeric value overrides.
    pub fn min_qp(&self) -> Option<u32> {
        if let Ok(raw) = std::env::var("ST_MIN_QP") {
            let trimmed = raw.trim();
            return match trimmed.to_ascii_lowercase().as_str() {
                "0" | "off" | "false" | "no" => None,
                _ => trimmed.parse::<u32>().ok().map(|v| v.min(255)),
            };
        }
        Some(match self.codec {
            Codec::H264 => 19,
            Codec::Hevc => 23,
            Codec::Av1 => 23,
        })
    }

    /// Number of slices per encoded frame (C2). More than one slice means a
    /// single lost UDP packet corrupts only its slice instead of the whole
    /// frame, and lets the client's `FF_THREAD_SLICE` decoder actually
    /// parallelise. FFmpeg maps `AVCodecContext::slices` to NVENC `sliceMode=3`,
    /// VAAPI slices, and x264/x265 `slices`. Resolution-based default
    /// (auto-enabled per the auto-enable rule); `ST_SLICES` overrides, `1`
    /// disables.
    pub fn slices_per_frame(&self) -> u32 {
        if let Some(n) = std::env::var("ST_SLICES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            return n.clamp(1, 32);
        }
        match self.height {
            h if h >= 2160 => 4,
            h if h >= 1080 => 2,
            _ => 1,
        }
    }

    /// H.264 entropy coder selection. CABAC (default, current effective
    /// behavior) or CAVLC via `ST_H264_CODER=cavlc`. `None` for non-H.264.
    pub fn h264_coder(&self) -> Option<&'static str> {
        if self.codec != Codec::H264 {
            return None;
        }
        match std::env::var("ST_H264_CODER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "cavlc" | "vlc" => Some("cavlc"),
            _ => Some("cabac"),
        }
    }

    /// Intra-refresh recovery (A3, opt-in). Spreads intra coding across a wave of
    /// frames instead of periodic full IDRs, so loss recovery (paired with the
    /// client's `recovery_point` SEI parser) costs no bitrate spike.
    ///
    /// **Encoder side is validated.** `intra_refresh_loopback.rs` is a real
    /// packet-loss-injection convergence test (the CLAUDE.md §9 gate): encode a
    /// PIR stream, drop a mid-stream P-frame, and prove the libavcodec decoder
    /// re-converges to the clean reference within one refresh period with *no*
    /// intervening IDR — and it discriminates (the same test fails when PIR is
    /// disabled). So the encoder recovery property A3 relies on is no longer a
    /// probe-only claim.
    ///
    /// **Still opt-in, by design.** The headline A3 benefit — killing the
    /// post-loss IDR *storm* — is not realized until the client stops eagerly
    /// requesting a keyframe on every loss (`pipeline.rs` `request_recovery_
    /// keyframe`) and instead rides the PIR wave, exiting recovery on the next
    /// `recovery_point` SEI it already parses. That client change plus its live
    /// loss validation is the remaining blocker for default-on; flipping the
    /// server alone would still emit the forced IDR (no storm reduction) for a
    /// only-marginal recovery-latency gain. `ST_INTRA_REFRESH=1` opts in today.
    ///
    /// NVENC note: FFmpeg's NVENC wrapper emits no `recovery_point` SEI (validated
    /// live on an RTX 4080), so NVENC recovery stays IDR-based regardless.
    /// AV1/VAAPI have no portable FFmpeg knob, so this only affects
    /// libx264/libx265/NVENC.
    pub fn intra_refresh_enabled(&self) -> bool {
        matches!(
            std::env::var("ST_INTRA_REFRESH").as_deref(),
            Ok("1") | Ok("true") | Ok("yes") | Ok("on")
        )
    }

    /// Compute VBV buffer size in bits (bitrate / fps for HW, larger for SW).
    ///
    /// `libsvtav1` derives a VBV duration from `rc_buffer_size` and rejects values below 20 ms.
    /// High-refresh software AV1 sessions can otherwise dip under that minimum.
    pub fn vbv_buffer_size(&self, is_software: bool) -> i32 {
        let bps = self.bitrate_bps();
        let size = if is_software {
            bps.saturating_mul(15) / ((self.framerate.max(1) as i64) * 10)
        } else {
            bps / self.framerate.max(1) as i64
        };
        let size = if is_software && self.codec == Codec::Av1 {
            size.max(self.buffer_size_for_duration_ms(Self::SVTAV1_MIN_BUFFER_MS))
        } else {
            size
        };
        size.min(i32::MAX as i64) as i32
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "windows")]
    pub fn ffmpeg_amf_codec_name(&self) -> &'static str {
        match self.codec {
            Codec::H264 => "h264_amf",
            Codec::Hevc => "hevc_amf",
            Codec::Av1 => "av1_amf",
        }
    }

    #[cfg(target_os = "windows")]
    pub fn ffmpeg_mf_codec_name(&self) -> &'static str {
        match self.codec {
            Codec::H264 => "h264_mf",
            Codec::Hevc => "hevc_mf",
            Codec::Av1 => "av1_mf",
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
            // Assigned atomically when the pipeline publishes this profile.
            video_epoch: 0,
            codec: self.stream_codec(),
            width: self.width,
            height: self.height,
            framerate: self.framerate.min(u16::MAX as u32) as u16,
            audio_sample_rate: audio.sample_rate,
            // E4: advertise the downmixed (stereo) channel count so the
            // stereo-only client accepts surround presets.
            audio_channels: audio.output_channels().min(u8::MAX as u32) as u8,
            hdr: self.is_hdr(),
            chroma: match self.chroma {
                ChromaSampling::Yuv420 => VideoChromaSampling::Yuv420,
                ChromaSampling::Yuv444 => VideoChromaSampling::Yuv444,
            },
            packet_duration_ms: audio.packet_duration_ms.min(u8::MAX as u32) as u8,
        }
    }
}

/// Audio stream configuration.
/// Default Opus frame duration (E1). 5 ms — the low-latency game-streaming choice
/// (matches Sunshine) — shaves ~15 ms vs the old 20 ms. The client derives all of
/// its sequence-gap / concealment timing from this over the wire
/// (`StreamConfig.packet_duration_ms`); `ST_AUDIO_FRAME_MS=20` restores the old
/// behavior. Opus-valid (2.5/5/10/20/40/60 ms).
pub const DEFAULT_OPUS_FRAME_MS: u32 = 5;

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate: u32,
    pub packet_duration_ms: u32,
}

impl AudioConfig {
    /// Build audio config from ST_AUDIO env var. The Opus frame duration defaults
    /// to [`DEFAULT_OPUS_FRAME_MS`] (5 ms, E1 low-latency default); `ST_AUDIO_FRAME_MS`
    /// overrides it (e.g. `=20` restores the old conservative framing). Opus only
    /// supports 2.5/5/10/20/40/60 ms frames, so the value is snapped to the
    /// nearest valid duration. The client derives all of its timing from this
    /// over the wire (`StreamConfig.packet_duration_ms`), so changing it here is
    /// sufficient.
    pub fn from_env() -> Self {
        let mut config = match std::env::var("ST_AUDIO")
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
        };
        if let Some(ms) = std::env::var("ST_AUDIO_FRAME_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            config.packet_duration_ms = Self::snap_opus_frame_ms(ms);
        }
        config
    }

    /// Snap an arbitrary ms value to the nearest Opus-supported frame duration.
    fn snap_opus_frame_ms(ms: u32) -> u32 {
        const VALID: [u32; 5] = [5, 10, 20, 40, 60];
        VALID
            .iter()
            .copied()
            .min_by_key(|v| v.abs_diff(ms))
            .unwrap_or(20)
    }

    pub fn stereo() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bitrate: 96_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn high_stereo() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bitrate: 512_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn surround51() -> Self {
        Self {
            sample_rate: 48000,
            channels: 6,
            bitrate: 256_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn high_surround51() -> Self {
        Self {
            sample_rate: 48000,
            channels: 6,
            bitrate: 1_536_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn surround71() -> Self {
        Self {
            sample_rate: 48000,
            channels: 8,
            bitrate: 450_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn high_surround71() -> Self {
        Self {
            sample_rate: 48000,
            channels: 8,
            bitrate: 2_048_000,
            packet_duration_ms: DEFAULT_OPUS_FRAME_MS,
        }
    }

    pub fn samples_per_frame(&self) -> u32 {
        self.packet_duration_ms * self.sample_rate / 1000
    }

    pub fn total_samples_per_frame(&self) -> usize {
        (self.samples_per_frame() * self.channels) as usize
    }

    /// E4 MVP: the client Opus decoder is stereo-only and rejects any stream
    /// with `audio_channels != 2`, so the 5.1/7.1 capture presets produce **no
    /// audio at all** today. Fold them to stereo on the server (capture still
    /// grabs all `channels` from the monitor; the encoder + advertised
    /// `StreamConfig` use `output_channels()`). Front L/R pass through
    /// unchanged, so this is strictly better than silence even on a driver
    /// whose surround channel order differs. `ST_AUDIO_DOWNMIX=0` restores raw
    /// passthrough (original behavior, still rejected by the stereo client).
    pub fn downmix_to_stereo_enabled(&self) -> bool {
        self.channels > 2
            && !matches!(
                std::env::var("ST_AUDIO_DOWNMIX").ok().as_deref(),
                Some("0") | Some("false") | Some("no") | Some("off")
            )
    }

    /// Channel count the Opus encoder and the on-wire `StreamConfig` actually
    /// use (stereo when surround is downmixed; otherwise the capture count).
    pub fn output_channels(&self) -> u32 {
        if self.downmix_to_stereo_enabled() {
            2
        } else {
            self.channels
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_presets_default_to_5ms_low_latency_frame() {
        // E1: every preset ships the 5 ms low-latency frame by default.
        for cfg in [
            AudioConfig::stereo(),
            AudioConfig::high_stereo(),
            AudioConfig::surround51(),
            AudioConfig::high_surround51(),
            AudioConfig::surround71(),
            AudioConfig::high_surround71(),
        ] {
            assert_eq!(cfg.packet_duration_ms, DEFAULT_OPUS_FRAME_MS);
            assert_eq!(DEFAULT_OPUS_FRAME_MS, 5);
            // 5 ms @ 48 kHz = 240 samples/channel — a valid Opus frame size.
            assert_eq!(cfg.samples_per_frame(), 240);
        }
        // 20 ms restore path stays a valid Opus frame (960 samples/channel).
        let mut twenty = AudioConfig::stereo();
        twenty.packet_duration_ms = 20;
        assert_eq!(twenty.samples_per_frame(), 960);
    }

    #[test]
    fn snap_opus_frame_ms_picks_nearest_valid() {
        assert_eq!(AudioConfig::snap_opus_frame_ms(5), 5);
        assert_eq!(AudioConfig::snap_opus_frame_ms(4), 5);
        assert_eq!(AudioConfig::snap_opus_frame_ms(13), 10);
        assert_eq!(AudioConfig::snap_opus_frame_ms(20), 20);
        assert_eq!(AudioConfig::snap_opus_frame_ms(1000), 60);
    }

    fn config(codec: Codec, framerate: u32) -> EncoderConfig {
        EncoderConfig {
            width: 1920,
            height: 1080,
            framerate,
            bitrate_kbps: 20_000,
            min_bitrate_kbps: 5_000,
            max_bitrate_kbps: 100_000,
            codec,
            dynamic_range: DynamicRange::Sdr,
            chroma: ChromaSampling::Yuv420,
            gop_size: EncoderConfig::default_gop_size(framerate),
            max_b_frames: 0,
            low_delay: true,
            quality: QualityPreset::Balanced,
        }
    }

    #[test]
    fn software_av1_vbv_buffer_is_clamped_at_high_refresh_rates() {
        let config = config(Codec::Av1, 144);

        assert_eq!(config.vbv_buffer_size(true), 400_000);
    }

    #[test]
    fn software_av1_vbv_buffer_keeps_existing_size_when_already_large_enough() {
        let config = config(Codec::Av1, 60);

        assert_eq!(config.vbv_buffer_size(true), 500_000);
    }

    #[test]
    fn software_h264_vbv_buffer_keeps_existing_high_refresh_behavior() {
        let config = config(Codec::H264, 144);

        assert_eq!(config.vbv_buffer_size(true), 208_333);
    }
}
