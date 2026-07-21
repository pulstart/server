#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod adaptive_bitrate;
mod api_client;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod audio;
mod broadcast;
mod capture;
mod clipboard;
mod colorspace;
#[cfg(unix)]
mod control_ipc;
#[cfg(target_os = "linux")]
mod encode;
mod encode_config;
#[cfg(target_os = "linux")]
mod encode_cuda;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod encode_sw;
#[cfg(target_os = "linux")]
mod encode_vaapi;
#[cfg(target_os = "macos")]
mod encode_vt;
#[cfg(target_os = "windows")]
mod encode_win;
mod file_transfer;
#[cfg(target_os = "linux")]
mod game_mode;
mod input;
#[cfg(target_os = "linux")]
mod linux_uring;
#[cfg(target_os = "macos")]
mod macos_display;
mod screen_wake;
mod server_control;
#[cfg(target_os = "linux")]
mod session_follow;
mod transport;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod tray;
mod updater;
mod video_profile;

// Real-bitstream RS-FEC loss-injection integration test (encode → slice → drop
// → reconstruct → decode). Test-only; see recovery_loopback.rs.
#[cfg(test)]
mod recovery_loopback;
// A3 intra-refresh loss-recovery convergence test (encode PIR → drop a frame →
// assert decode re-converges within one period with no IDR). See
// intra_refresh_loopback.rs.
#[cfg(test)]
mod intra_refresh_loopback;

use adaptive_bitrate::{AdaptiveBitrateState, ClientRateController};
use broadcast::{Broadcaster, SubscriptionReservation};
use capture::{CaptureBackend, PlatformCapture};
use encode_config::EncoderConfig;
use input::{CursorVersionCursor, InputRuntime};
use server_control::ServerControl;
use transport::{EncodedAudioPacket, EncodedVideoFrame, UdpSender};
use video_profile::{AggregateVideoCapabilities, ClientVideoCapabilities, VideoCapabilityRegistry};

use crossbeam_channel::{bounded, Receiver, Sender};
use st_protocol::{
    control::OutputInfo, ClientDisplayInfo, ClockSyncPong, ControlMessage, InputSession,
    SessionDebugInfo, StreamConfig, VideoChromaSampling, VideoCodec, VideoCodecSupport,
};
use std::net::SocketAddr;
use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Constant-time byte comparison to prevent timing side-channels on token auth.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

const DEFAULT_APP_PORT: u16 = 28_480;
const DISCOVERY_PORT: u16 = 28_481;
const DISCOVERY_BEACON_INTERVAL: Duration = Duration::from_secs(2);
const VIDEO_SUBSCRIBER_CAPACITY: usize = 120;
const CAPTURE_QUEUE_CAPACITY: usize = 4;
/// Max encoded video units a transport sender drains+sends in one wakeup.
/// Bounds per-iteration work so audio drain / shutdown checks still get a turn
/// under sustained overload; the broadcaster's oldest-eviction is the backstop.
const MAX_VIDEO_SEND_BURST: usize = 16;
static TRACE_ENCODE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ENCODED_VIDEO_UNIT_SEQ: AtomicU64 = AtomicU64::new(0);
static NEXT_PIPELINE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
extern "C" {
    fn CVPixelBufferRelease(buf: *mut std::ffi::c_void);
}

fn next_encoded_video_unit_seq() -> u64 {
    NEXT_ENCODED_VIDEO_UNIT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Result of the pipeline — either it started OK or it had an error.
enum PipelineResult {
    Started(StreamConfig, Arc<AdaptiveBitrateState>, SessionDebugInfo),
    Error(String),
}

/// Encoder wrapper for Linux (VAAPI → NVENC → Software fallback chain).
#[cfg(any(target_os = "linux", target_os = "windows"))]
enum EncoderKind {
    #[cfg(target_os = "linux")]
    Vaapi(encode_vaapi::VaapiEncoder),
    #[cfg(target_os = "linux")]
    Nvenc(encode::NvencEncoder),
    #[cfg(target_os = "windows")]
    Hardware(encode_win::WindowsHwEncoder),
    Software(encode_sw::SoftwareEncoder),
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderBackend {
    #[cfg(target_os = "linux")]
    Vaapi,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Nvenc,
    #[cfg(target_os = "windows")]
    Amf,
    #[cfg(target_os = "windows")]
    MediaFoundation,
    Software,
}

#[cfg(target_os = "linux")]
fn create_linux_encoder_with_hint(
    config: &EncoderConfig,
    render_node_hint: Option<&str>,
) -> Result<EncoderKind, String> {
    match encode_vaapi::VaapiEncoder::with_config(config, render_node_hint) {
        Ok(e) => {
            println!("[encoder] Using VAAPI ({:?})", config.codec);
            Ok(EncoderKind::Vaapi(e))
        }
        Err(vaapi_err) => {
            eprintln!("[encoder] VAAPI failed ({vaapi_err}), trying NVENC...");
            match encode::NvencEncoder::with_config(config) {
                Ok(e) => {
                    println!("[encoder] Using NVENC ({:?})", config.codec);
                    Ok(EncoderKind::Nvenc(e))
                }
                Err(nvenc_err) => {
                    eprintln!("[encoder] NVENC failed ({nvenc_err}), trying software...");
                    match encode_sw::SoftwareEncoder::with_config(config) {
                        Ok(e) => {
                            println!("[encoder] Using software encoder ({:?})", config.codec);
                            Ok(EncoderKind::Software(e))
                        }
                        Err(sw_err) => Err(format!(
                            "All encoders failed.\n  VAAPI: {vaapi_err}\n  NVENC: {nvenc_err}\n  Software: {sw_err}"
                        )),
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn single_codec_support(codec: VideoCodec) -> VideoCodecSupport {
    let mut support = VideoCodecSupport::empty();
    support.insert(codec);
    support
}

#[cfg(target_os = "linux")]
fn open_linux_encoder_for_aggregate(
    base: &EncoderConfig,
    capabilities: AggregateVideoCapabilities,
    control: &ServerControl,
    capture_render_node: Option<&str>,
) -> Result<(EncoderConfig, EncoderKind), String> {
    let codec_order = if let Some(codec) = control.forced_codec() {
        encode_config::Codec::preferred_order(Some(codec))
    } else {
        EncoderConfig::preferred_codec_order_from_env()
    };
    let codec_order = codec_order.map(encode_config::Codec::to_stream_codec);
    let mut codec_candidates = Vec::new();
    if control.forced_codec().is_none() {
        codec_candidates.extend(
            codec_order
                .iter()
                .copied()
                .filter(|codec| capabilities.hardware_codecs.supports(*codec)),
        );
    }
    for codec in codec_order {
        if capabilities.supported_codecs.supports(codec) && !codec_candidates.contains(&codec) {
            codec_candidates.push(codec);
        }
    }
    let mut failures = Vec::new();
    for codec in codec_candidates {
        let mut candidate_capabilities = capabilities;
        candidate_capabilities.supported_codecs = single_codec_support(codec);
        let config = aggregate_encoder_config(base, candidate_capabilities, control)?;
        match create_linux_encoder_with_hint(&config, capture_render_node) {
            Ok(encoder) => return Ok((config, encoder)),
            Err(error) => failures.push(format!("{}: {error}", codec_name(codec))),
        }
    }
    Err(format!(
        "No mutually supported video codec could start.\n  {}",
        failures.join("\n  ")
    ))
}

#[cfg(target_os = "linux")]
fn create_encoder_for_backend(
    config: &EncoderConfig,
    backend: EncoderBackend,
) -> Result<EncoderKind, String> {
    match backend {
        #[cfg(target_os = "linux")]
        EncoderBackend::Vaapi => encode_vaapi::VaapiEncoder::with_config(config, None)
            .map(EncoderKind::Vaapi)
            .map_err(|err| format!("VAAPI reconfigure failed: {err}")),
        #[cfg(target_os = "linux")]
        EncoderBackend::Nvenc => encode::NvencEncoder::with_config(config)
            .map(EncoderKind::Nvenc)
            .map_err(|err| format!("NVENC reconfigure failed: {err}")),
        EncoderBackend::Software => encode_sw::SoftwareEncoder::with_config(config)
            .map(EncoderKind::Software)
            .map_err(|err| format!("software reconfigure failed: {err}")),
    }
}

#[cfg(target_os = "windows")]
fn create_encoder_for_backend(
    config: &EncoderConfig,
    backend: EncoderBackend,
) -> Result<EncoderKind, String> {
    match backend {
        EncoderBackend::Nvenc | EncoderBackend::Amf | EncoderBackend::MediaFoundation => {
            Err("Windows hardware encoder rebuild requires a live D3D11 capture context".into())
        }
        EncoderBackend::Software => encode_sw::SoftwareEncoder::with_config(config)
            .map(EncoderKind::Software)
            .map_err(|err| format!("software reconfigure failed: {err}")),
    }
}

fn unix_time_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

fn trace_enabled() -> bool {
    std::env::var_os("ST_TRACE").is_some()
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

fn codec_support_summary(support: VideoCodecSupport) -> String {
    let mut entries = Vec::new();
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        if support.supports(codec) {
            entries.push(codec_name(codec));
        }
    }
    if entries.is_empty() {
        "-".to_string()
    } else {
        entries.join(" / ")
    }
}

fn client_supported_video_codecs(display: Option<ClientDisplayInfo>) -> VideoCodecSupport {
    display
        .map(|info| info.supported_video_codecs)
        .unwrap_or_else(VideoCodecSupport::h264_only)
}

fn client_hardware_video_codecs(display: Option<ClientDisplayInfo>) -> VideoCodecSupport {
    display
        .map(|info| info.hardware_video_codecs)
        .unwrap_or_else(VideoCodecSupport::empty)
}

fn client_supported_yuv444_video_codecs(display: Option<ClientDisplayInfo>) -> VideoCodecSupport {
    display
        .map(|info| info.supported_yuv444_video_codecs)
        .unwrap_or_else(VideoCodecSupport::empty)
}

fn client_hardware_yuv444_video_codecs(display: Option<ClientDisplayInfo>) -> VideoCodecSupport {
    display
        .map(|info| info.hardware_yuv444_video_codecs)
        .unwrap_or_else(VideoCodecSupport::empty)
}

/// Whether the connecting client's display can present HDR (D2 AND-gate). A
/// missing/legacy `ClientDisplayInfo` reports false so an SDR client never gets
/// a washed-out BT.2020/PQ stream.
fn client_hdr_display(display: Option<ClientDisplayInfo>) -> bool {
    display.map(|info| info.hdr_display).unwrap_or(false)
}

fn client_video_capabilities(display: Option<ClientDisplayInfo>) -> ClientVideoCapabilities {
    ClientVideoCapabilities {
        supported_codecs: client_supported_video_codecs(display),
        hardware_codecs: client_hardware_video_codecs(display),
        supported_yuv444_codecs: client_supported_yuv444_video_codecs(display),
        hardware_yuv444_codecs: client_hardware_yuv444_video_codecs(display),
        hdr_display: client_hdr_display(display),
        requested_fps: client_display_fps_hint(display),
    }
}

fn encoder_chroma_name(chroma: encode_config::ChromaSampling) -> &'static str {
    match chroma {
        encode_config::ChromaSampling::Yuv420 => "yuv420",
        encode_config::ChromaSampling::Yuv444 => "yuv444",
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn supports_yuv444_codec(codec: encode_config::Codec) -> bool {
    matches!(
        codec,
        encode_config::Codec::H264 | encode_config::Codec::Hevc
    )
}

fn stream_codec_to_encoder(codec: VideoCodec) -> encode_config::Codec {
    match codec {
        VideoCodec::H264 => encode_config::Codec::H264,
        VideoCodec::Hevc => encode_config::Codec::Hevc,
        VideoCodec::Av1 => encode_config::Codec::Av1,
    }
}

fn aggregate_encoder_config(
    current: &EncoderConfig,
    capabilities: AggregateVideoCapabilities,
    control: &ServerControl,
) -> Result<EncoderConfig, String> {
    let codec_order = if let Some(codec) = control.forced_codec() {
        encode_config::Codec::preferred_order(Some(codec))
    } else {
        EncoderConfig::preferred_codec_order_from_env()
    };
    let codec_order = codec_order.map(encode_config::Codec::to_stream_codec);
    let codec = if control.forced_codec().is_some() {
        capabilities.preferred_codec(codec_order)
    } else {
        capabilities.preferred_codec_hardware_first(codec_order)
    }
    .ok_or_else(|| "No video codec is supported by every connected client".to_string())?;
    let mut config = EncoderConfig::from_env_with_framerate_and_codec(
        current.width,
        current.height,
        EncoderConfig::resolve_target_fps(capabilities.requested_fps),
        stream_codec_to_encoder(codec),
    );
    config.bitrate_kbps = current.bitrate_kbps;
    config.min_bitrate_kbps = current.min_bitrate_kbps;
    config.max_bitrate_kbps = current.max_bitrate_kbps;
    config.quality = control.forced_quality().unwrap_or(current.quality);
    if !capabilities.hdr_display {
        config.dynamic_range = encode_config::DynamicRange::Sdr;
    }
    config.chroma = match EncoderConfig::preferred_chroma_from_env() {
        Some(encode_config::ChromaSampling::Yuv444)
            if capabilities.supported_yuv444_codecs.supports(codec)
                && supports_yuv444_codec(config.codec)
                && !config.is_hdr() =>
        {
            encode_config::ChromaSampling::Yuv444
        }
        Some(encode_config::ChromaSampling::Yuv444) => {
            return Err(format!(
                "Requested yuv444 profile is not supported by every client for {}",
                codec_name(codec)
            ));
        }
        Some(encode_config::ChromaSampling::Yuv420) => encode_config::ChromaSampling::Yuv420,
        None if capabilities.hardware_codecs.supports(codec)
            && !capabilities.hardware_yuv444_codecs.supports(codec) =>
        {
            encode_config::ChromaSampling::Yuv420
        }
        None => match capabilities.preferred_chroma(codec, config.is_hdr()) {
            VideoChromaSampling::Yuv420 => encode_config::ChromaSampling::Yuv420,
            VideoChromaSampling::Yuv444 => encode_config::ChromaSampling::Yuv444,
        },
    };
    Ok(config)
}

fn encoder_profiles_equal(left: &EncoderConfig, right: &EncoderConfig) -> bool {
    left.width == right.width
        && left.height == right.height
        && left.framerate == right.framerate
        && left.codec == right.codec
        && left.dynamic_range == right.dynamic_range
        && left.chroma == right.chroma
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encoder_name(encoder: &EncoderKind) -> &'static str {
    match encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(_) => "vaapi",
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(_) => "nvenc",
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => e.backend_name(),
        EncoderKind::Software(_) => "software",
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encoder_backend(encoder: &EncoderKind) -> EncoderBackend {
    match encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(_) => EncoderBackend::Vaapi,
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(_) => EncoderBackend::Nvenc,
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => match e.backend() {
            encode_win::WindowsEncoderBackend::Nvenc => EncoderBackend::Nvenc,
            encode_win::WindowsEncoderBackend::Amf => EncoderBackend::Amf,
            encode_win::WindowsEncoderBackend::MediaFoundation => EncoderBackend::MediaFoundation,
        },
        EncoderKind::Software(_) => EncoderBackend::Software,
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encoder_backend_name(backend: EncoderBackend) -> &'static str {
    match backend {
        #[cfg(target_os = "linux")]
        EncoderBackend::Vaapi => "vaapi",
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        EncoderBackend::Nvenc => "nvenc",
        #[cfg(target_os = "windows")]
        EncoderBackend::Amf => "amf",
        #[cfg(target_os = "windows")]
        EncoderBackend::MediaFoundation => "mf",
        EncoderBackend::Software => "software",
    }
}

/// Result of the pipeline start/subscribe handshake: the client subscription,
/// negotiated stream config, shared bitrate state, debug info, and the shared
/// capture handle.
type PipelineSetup = (
    ClientSubscription,
    StreamSnapshot,
    Arc<AdaptiveBitrateState>,
    SessionDebugInfo,
    Arc<SharedCaptureState>,
);

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn select_linux_encoder(
    width: u32,
    height: u32,
    framerate: u32,
    client_supported_codecs: VideoCodecSupport,
    client_hardware_codecs: VideoCodecSupport,
    client_supported_yuv444_codecs: VideoCodecSupport,
    client_hardware_yuv444_codecs: VideoCodecSupport,
    client_hdr_display: bool,
    control: &ServerControl,
    capture_render_node: Option<&str>,
) -> Result<(EncoderConfig, EncoderKind), String> {
    let seed = EncoderConfig::from_env_with_framerate_and_codec(
        width,
        height,
        framerate,
        encode_config::Codec::H264,
    );
    let capabilities = AggregateVideoCapabilities {
        supported_codecs: client_supported_codecs,
        hardware_codecs: client_hardware_codecs,
        supported_yuv444_codecs: client_supported_yuv444_codecs,
        hardware_yuv444_codecs: client_hardware_yuv444_codecs,
        hdr_display: client_hdr_display,
        requested_fps: Some(framerate),
    };
    let (config, encoder) =
        open_linux_encoder_for_aggregate(&seed, capabilities, control, capture_render_node)?;
    let backend = encoder_backend(&encoder);
    println!(
        "[encoder] Selected {} {} with {} backend",
        codec_name(config.stream_codec()),
        encoder_chroma_name(config.chroma),
        encoder_backend_name(backend),
    );
    Ok((config, encoder))
}

#[cfg(target_os = "windows")]
fn select_windows_encoder(
    first_frame: &capture::CapturedFrame,
    framerate: u32,
    client_supported_codecs: VideoCodecSupport,
    client_supported_yuv444_codecs: VideoCodecSupport,
    client_hdr_display: bool,
    control: &ServerControl,
) -> Result<(EncoderConfig, EncoderKind), String> {
    let width = first_frame.width;
    let height = first_frame.height;
    let forced_codec = control.forced_codec();
    let forced_quality = control.forced_quality();
    let codec_order = if let Some(codec) = forced_codec {
        encode_config::Codec::preferred_order(Some(codec))
    } else {
        EncoderConfig::preferred_codec_order_from_env()
    };

    let mut failures = Vec::new();
    let hardware_capture = match &first_frame.data {
        capture::FrameData::D3D11Texture { texture, .. } => Some(texture.as_ref()),
        capture::FrameData::Ram(_) => None,
    };
    let hardware_backends = if let Some(texture) = hardware_capture {
        match encode_win::preferred_backend_order(texture) {
            Ok(order) => order,
            Err(err) => {
                failures.push(format!("hardware backend detection failed: {err}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    // Enumerate encoder backends on OTHER adapters (e.g. NVENC on dGPU when
    // capture is on iGPU). Only used when same-adapter backends all fail.
    let cross_adapter_backends = if let Some(texture) = hardware_capture {
        encode_win::cross_adapter_backends(texture)
    } else {
        Vec::new()
    };

    for codec in codec_order {
        if !client_supported_codecs.supports(codec.to_stream_codec()) {
            failures.push(format!(
                "{} skipped: client does not support it",
                codec_name(codec.to_stream_codec())
            ));
            continue;
        }

        let mut config =
            EncoderConfig::from_env_with_framerate_and_codec(width, height, framerate, codec);
        if let Some(quality) = forced_quality {
            config.quality = quality;
        }
        if config.is_hdr() && !client_hdr_display {
            config.dynamic_range = encode_config::DynamicRange::Sdr;
        }
        if config.is_yuv444() {
            if !supports_yuv444_codec(codec)
                || !client_supported_yuv444_codecs.supports(codec.to_stream_codec())
            {
                failures.push(format!(
                    "{} yuv444 skipped: client does not support it",
                    codec_name(codec.to_stream_codec())
                ));
                continue;
            }
        }

        if let Some(texture) = hardware_capture {
            for backend in &hardware_backends {
                match encode_win::WindowsHwEncoder::with_config_and_backend(
                    &config, texture, *backend,
                ) {
                    Ok(encoder) => {
                        println!(
                            "[encoder] Selected {} with {} backend",
                            codec_name(config.stream_codec()),
                            backend.label()
                        );
                        return Ok((config, EncoderKind::Hardware(encoder)));
                    }
                    Err(err) => failures.push(format!(
                        "{} {} encode unavailable: {err}",
                        codec_name(codec.to_stream_codec()),
                        backend.label()
                    )),
                }
            }
        }

        // Try hardware encoding on OTHER adapters (cross-adapter staging)
        if let Some(texture) = hardware_capture {
            for cab in &cross_adapter_backends {
                match encode_win::WindowsHwEncoder::with_config_cross_adapter(
                    &config,
                    texture,
                    &cab.adapter,
                    cab.backend,
                    &cab.adapter_name,
                ) {
                    Ok(encoder) => {
                        println!(
                            "[encoder] Selected {} with {} on {} (cross-adapter)",
                            codec_name(config.stream_codec()),
                            cab.backend.label(),
                            cab.adapter_name
                        );
                        return Ok((config, EncoderKind::Hardware(encoder)));
                    }
                    Err(err) => failures.push(format!(
                        "{} {} cross-adapter ({}) unavailable: {err}",
                        codec_name(codec.to_stream_codec()),
                        cab.backend.label(),
                        cab.adapter_name
                    )),
                }
            }
        }

        match encode_sw::SoftwareEncoder::with_config(&config) {
            Ok(encoder) => {
                println!(
                    "[encoder] Selected {} with software backend",
                    codec_name(config.stream_codec())
                );
                return Ok((config, EncoderKind::Software(encoder)));
            }
            Err(err) => failures.push(format!(
                "{} software encode unavailable: {err}",
                codec_name(codec.to_stream_codec())
            )),
        }
    }

    Err(format!(
        "No mutually supported video codec could start.\n  {}",
        failures.join("\n  ")
    ))
}

#[cfg(target_os = "macos")]
fn select_macos_encoder(
    width: u32,
    height: u32,
    framerate: u32,
    client_supported_codecs: VideoCodecSupport,
    _client_supported_yuv444_codecs: VideoCodecSupport,
    control: &ServerControl,
) -> Result<(EncoderConfig, encode_vt::VTEncoder), String> {
    let forced_codec = control.forced_codec();
    let forced_quality = control.forced_quality();
    let codec_order = if let Some(codec) = forced_codec {
        encode_config::Codec::preferred_order(Some(codec))
    } else {
        EncoderConfig::preferred_codec_order_from_env()
    };

    let mut failures = Vec::new();
    for codec in codec_order {
        if !client_supported_codecs.supports(codec.to_stream_codec()) {
            failures.push(format!(
                "{} skipped: client does not support it",
                codec_name(codec.to_stream_codec())
            ));
            continue;
        }
        if codec != encode_config::Codec::H264 {
            failures.push(format!(
                "{} unavailable: macOS VideoToolbox encode is currently implemented for H.264 only",
                codec_name(codec.to_stream_codec())
            ));
            continue;
        }

        let mut config =
            EncoderConfig::from_env_with_framerate_and_codec(width, height, framerate, codec);
        if let Some(quality) = forced_quality {
            config.quality = quality;
        }
        if config.is_yuv444() {
            failures.push(format!(
                "{} yuv444 unavailable: macOS VideoToolbox encode is currently YUV420 only",
                codec_name(codec.to_stream_codec())
            ));
            continue;
        }
        if config.is_hdr() {
            eprintln!("[encoder] macOS VideoToolbox HDR encode is not implemented; forcing SDR");
            config.dynamic_range = encode_config::DynamicRange::Sdr;
        }

        match encode_vt::VTEncoder::new(
            width,
            height,
            config.bitrate_bps().min(u32::MAX as i64) as u32,
            config.framerate,
        ) {
            Ok(encoder) => {
                println!(
                    "[encoder] Selected {} with videotoolbox backend",
                    codec_name(config.stream_codec())
                );
                return Ok((config, encoder));
            }
            Err(err) => failures.push(format!(
                "{} videotoolbox encode unavailable: {err}",
                codec_name(codec.to_stream_codec())
            )),
        }
    }

    Err(format!(
        "No mutually supported video codec could start.\n  {}",
        failures.join("\n  ")
    ))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn request_next_keyframe(encoder: &mut EncoderKind) {
    match encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(e) => e.reset_for_keyframe(),
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(e) => e.reset_for_keyframe(),
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => e.reset_for_keyframe(),
        EncoderKind::Software(e) => e.reset_for_keyframe(),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn update_encoder_bitrate(encoder: &mut EncoderKind, config: &EncoderConfig) -> Result<(), String> {
    match encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(e) => e.update_bitrate(config),
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(e) => e.update_bitrate(config),
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => e.update_bitrate(config),
        EncoderKind::Software(e) => e.update_bitrate(config),
    }
}

/// Verifies that an in-place bitrate change actually took effect by measuring
/// the encoder's real output rate.
///
/// libx264 honors a runtime `av_opt_set` bitrate change (verified empirically),
/// but some encoder wrappers can accept a runtime change without applying it.
/// If that happened silently the server would think it lowered the bitrate
/// during congestion while the encoder kept blasting the old rate — the stream
/// would never recover. This verifier closes that loop: after a *downward*
/// change, it measures the actual output over a grace window; if the encoder is
/// still emitting near the old (much higher) rate, the change clearly did not
/// apply and the ABR loop escalates to an encoder rebuild. After repeated
/// contradictions it marks in-place as ineffective so future changes rebuild
/// directly.
///
/// Only downward changes are checked: an encoder legitimately produces far less
/// than the target ceiling on low-motion content, so a low measured rate after
/// an *upward* change is not evidence of failure. A downward cap, by contrast,
/// the encoder must honor — making it the verifiable (and stability-critical)
/// direction. Never trips when in-place works, so it is inert on good backends.
#[cfg(any(target_os = "linux", target_os = "windows"))]
struct BitrateVerifier {
    fps: u32,
    window_bytes: u64,
    window_frames: u32,
    window_start: Instant,
    pending: Option<(u32, Instant)>, // (target_kbps, deadline)
    consecutive_failures: u32,
    inplace_ineffective: bool,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl BitrateVerifier {
    const GRACE: Duration = Duration::from_millis(1500);
    // Trip only when the measured rate is far above the requested ceiling, so
    // normal VBV overshoot / content bursts never cause a false rebuild.
    const OVERSHOOT_TRIP_RATIO: f64 = 1.8;
    const FAILURES_TO_DISABLE_INPLACE: u32 = 2;

    fn new(fps: u32, now: Instant) -> Self {
        Self {
            fps: fps.max(1),
            window_bytes: 0,
            window_frames: 0,
            window_start: now,
            pending: None,
            consecutive_failures: 0,
            inplace_ineffective: false,
        }
    }

    fn record(&mut self, bytes: usize) {
        self.window_bytes = self.window_bytes.saturating_add(bytes as u64);
        self.window_frames = self.window_frames.saturating_add(1);
    }

    /// Arm verification for a downward in-place change to `target_kbps`.
    fn arm_downward(&mut self, target_kbps: u32, now: Instant) {
        self.pending = Some((target_kbps, now + Self::GRACE));
        self.window_bytes = 0;
        self.window_frames = 0;
        self.window_start = now;
    }

    fn measured_kbps(&self, now: Instant) -> Option<u32> {
        let elapsed = now.duration_since(self.window_start).as_secs_f64();
        // Need a real time span and enough frames (~half a second) to be stable.
        if elapsed < 0.5 || self.window_frames < self.fps / 2 {
            return None;
        }
        Some(((self.window_bytes as f64 * 8.0) / elapsed / 1000.0) as u32)
    }

    /// If a pending downward change has reached its deadline, decide whether it
    /// took effect. Returns `true` when the change clearly did NOT apply and the
    /// caller should escalate to an encoder rebuild.
    fn check_and_take_failure(&mut self, now: Instant) -> bool {
        let Some((target_kbps, deadline)) = self.pending else {
            return false;
        };
        if now < deadline {
            return false;
        }
        let Some(measured) = self.measured_kbps(now) else {
            // Not enough data yet — extend the window a little rather than guess.
            self.pending = Some((target_kbps, now + Duration::from_millis(500)));
            return false;
        };
        self.pending = None;
        if measured as f64 > target_kbps as f64 * Self::OVERSHOOT_TRIP_RATIO {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= Self::FAILURES_TO_DISABLE_INPLACE {
                self.inplace_ineffective = true;
            }
            true
        } else {
            // In-place worked: output tracked the new ceiling.
            self.consecutive_failures = 0;
            false
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn should_schedule_bitrate_reconfigure(
    current_kbps: u32,
    target_kbps: u32,
    last_reconfigure: Instant,
) -> bool {
    if current_kbps == target_kbps {
        return false;
    }

    let now = Instant::now();
    let delta_kbps = current_kbps.abs_diff(target_kbps);

    if target_kbps < current_kbps {
        let min_delta = (current_kbps / 10).max(2_500);
        now.duration_since(last_reconfigure) >= Duration::from_millis(750)
            && delta_kbps >= min_delta
    } else {
        let min_delta = (current_kbps / 10).max(5_000);
        now.duration_since(last_reconfigure) >= Duration::from_secs(4) && delta_kbps >= min_delta
    }
}

#[cfg(target_os = "macos")]
fn encoder_name(_encoder: &encode_vt::VTEncoder) -> &'static str {
    "videotoolbox"
}

fn build_session_debug(
    #[cfg(any(target_os = "linux", target_os = "windows"))] encoder: &EncoderKind,
    #[cfg(target_os = "macos")] encoder: &encode_vt::VTEncoder,
    config: &EncoderConfig,
    capture_backend: &str,
    input: &InputRuntime,
) -> SessionDebugInfo {
    SessionDebugInfo {
        encoder_name: format!(
            "{}-{}",
            encoder_name(encoder),
            encoder_chroma_name(config.chroma)
        ),
        capture_backend: capture_backend.to_string(),
        input_backend: input.backend_label(),
        target_bitrate_kbps: config.bitrate_kbps,
        quality_preset: config.quality.label().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Shared pipeline: one capture + one encoder + one audio pipeline,
// broadcasting encoded data to all connected clients.
// ---------------------------------------------------------------------------

/// Commands sent from a per-client control handler into the shared pipeline
/// thread. Capture is a single shared resource, so these affect every client.
enum CaptureCommand {
    /// Re-select the shared encoder against every active/tentative client. The
    /// response is sent only after the compatible encoder is active, or after
    /// the old stream has been left untouched on failure.
    SetVideoCapabilities(ProfileRequest),
}

struct ProfileRequest {
    capabilities: AggregateVideoCapabilities,
    registry_revision: u64,
    revision_source: Arc<AtomicU64>,
    commit_lock: Arc<Mutex<()>>,
    cancelled: Arc<AtomicBool>,
    admission: Option<AdmissionStamp>,
    response: Sender<Result<StreamSnapshot, String>>,
}

impl ProfileRequest {
    fn is_current(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.revision_source.load(Ordering::Acquire) == self.registry_revision
            && self.admission.as_ref().is_none_or(AdmissionStamp::is_current)
    }

    fn reject(self, reason: impl Into<String>) {
        let _ = self.response.send(Err(reason.into()));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamSnapshot {
    config: StreamConfig,
    generation: u64,
    video_epoch: u64,
    selection_generation: u64,
}

#[derive(Clone)]
struct DebugSnapshot {
    generation: u64,
    info: SessionDebugInfo,
}

/// State shared between the pipeline thread (producer) and per-client control
/// handlers (consumers): the enumerated outputs, the current selection, and the
/// stream-config update an output switch produces.
struct SharedCaptureState {
    cmd_tx: Sender<CaptureCommand>,
    available_outputs: Mutex<Vec<OutputInfo>>,
    /// Currently captured output id (0 = unknown / primary).
    selected_output: AtomicU32,
    requested_output: AtomicU32,
    requested_output_generation: AtomicU64,
    /// Canonical stream configuration. Keeping config and generation under one
    /// lock makes startup snapshots atomic with concurrent profile changes.
    stream: Mutex<Option<StreamSnapshot>>,
    active_video_epoch: Arc<AtomicU64>,
    debug: Mutex<Option<DebugSnapshot>>,
}

impl SharedCaptureState {
    fn new(cmd_tx: Sender<CaptureCommand>) -> Self {
        Self {
            cmd_tx,
            available_outputs: Mutex::new(Vec::new()),
            selected_output: AtomicU32::new(0),
            requested_output: AtomicU32::new(0),
            requested_output_generation: AtomicU64::new(0),
            stream: Mutex::new(None),
            active_video_epoch: Arc::new(AtomicU64::new(0)),
            debug: Mutex::new(None),
        }
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.available_outputs.lock().unwrap().clone()
    }

    fn selected(&self) -> u32 {
        self.selected_output.load(Ordering::SeqCst)
    }

    fn request_output(&self, id: u32) {
        self.requested_output.store(id, Ordering::Release);
        self.requested_output_generation
            .fetch_add(1, Ordering::AcqRel);
    }

    fn pending_output(&self, observed_generation: u64) -> Option<(u64, u32)> {
        let generation = self.requested_output_generation.load(Ordering::Acquire);
        (generation != observed_generation)
            .then(|| (generation, self.requested_output.load(Ordering::Acquire)))
    }

    fn initialize_config(&self, mut config: StreamConfig) -> StreamSnapshot {
        config.video_epoch = 1;
        let snapshot = StreamSnapshot {
            config,
            generation: 1,
            video_epoch: 1,
            selection_generation: 1,
        };
        *self.stream.lock().unwrap() = Some(snapshot);
        self.active_video_epoch
            .store(snapshot.video_epoch, Ordering::Release);
        snapshot
    }

    fn snapshot(&self) -> Option<StreamSnapshot> {
        *self.stream.lock().unwrap()
    }

    /// Commit an encoder swap. Every swap gets a new internal media epoch so
    /// queued output from the replaced encoder is rejected. The public config
    /// generation advances only when the wire-visible configuration changed.
    fn commit_encoder_config(&self, mut config: StreamConfig) -> StreamSnapshot {
        let mut stream = self.stream.lock().unwrap();
        let previous = stream.expect("stream config not initialized");
        config.video_epoch = previous.video_epoch;
        let config_changed = previous.config != config;
        let video_epoch = previous.video_epoch.wrapping_add(1);
        config.video_epoch = video_epoch;
        let snapshot = StreamSnapshot {
            config,
            generation: if config_changed {
                previous.generation.wrapping_add(1)
            } else {
                previous.generation
            },
            video_epoch,
            selection_generation: previous.selection_generation,
        };
        *stream = Some(snapshot);
        self.active_video_epoch
            .store(snapshot.video_epoch, Ordering::Release);
        snapshot
    }

    fn commit_output_selection(&self, mut config: StreamConfig) -> StreamSnapshot {
        let mut stream = self.stream.lock().unwrap();
        let previous = stream.expect("stream config not initialized");
        config.video_epoch = previous.video_epoch;
        let config_changed = previous.config != config;
        let video_epoch = previous.video_epoch.wrapping_add(1);
        config.video_epoch = video_epoch;
        let snapshot = StreamSnapshot {
            config,
            generation: if config_changed {
                previous.generation.wrapping_add(1)
            } else {
                previous.generation
            },
            video_epoch,
            selection_generation: previous.selection_generation.wrapping_add(1),
        };
        *stream = Some(snapshot);
        self.active_video_epoch
            .store(snapshot.video_epoch, Ordering::Release);
        snapshot
    }

    fn active_video_epoch(&self) -> u64 {
        self.active_video_epoch.load(Ordering::Acquire)
    }

    fn active_video_epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.active_video_epoch)
    }

    fn initialize_debug(&self, info: SessionDebugInfo) {
        *self.debug.lock().unwrap() = Some(DebugSnapshot {
            generation: 1,
            info,
        });
    }

    fn update_debug(&self, info: SessionDebugInfo) -> DebugSnapshot {
        let mut debug = self.debug.lock().unwrap();
        let generation = debug
            .as_ref()
            .map_or(1, |current| current.generation.wrapping_add(1));
        let snapshot = DebugSnapshot { generation, info };
        *debug = Some(snapshot.clone());
        snapshot
    }

    fn debug_snapshot(&self) -> Option<DebugSnapshot> {
        self.debug.lock().unwrap().clone()
    }
}

struct SharedPipeline {
    instance_id: u64,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    audio_bc: Arc<Broadcaster<EncodedAudioPacket>>,
    session_debug: SessionDebugInfo,
    rate_control: Arc<AdaptiveBitrateState>,
    capture_state: Arc<SharedCaptureState>,
    shutdown_tx: Sender<()>,
    pipeline_handle: std::thread::JoinHandle<()>,
}

impl SharedPipeline {
    #[allow(clippy::too_many_arguments)]
    fn start(
        video_capabilities: AggregateVideoCapabilities,
        input: Arc<InputRuntime>,
        control: Arc<ServerControl>,
        state: &Arc<ServerState>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(Self, ClientSubscription), String> {
        let instance_id = NEXT_PIPELINE_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        let video_bc = Arc::new(Broadcaster::new());
        let zero_state = Arc::downgrade(state);
        video_bc.set_on_zero(move || {
            if let Some(state) = zero_state.upgrade() {
                stop_pipeline_if_idle(&state, instance_id);
            }
        });
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let audio_bc = Arc::new(Broadcaster::new());
        let (vid_sub_id, vid_rx) = video_bc.subscribe(VIDEO_SUBSCRIBER_CAPACITY)?;
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let (aud_sub_id, aud_rx) = match audio_bc.subscribe(30) {
            Ok(subscription) => subscription,
            Err(error) => {
                video_bc.unsubscribe(vid_sub_id);
                return Err(error);
            }
        };

        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (status_tx, status_rx) = bounded::<PipelineResult>(1);
        let (capture_cmd_tx, capture_cmd_rx) = bounded::<CaptureCommand>(4);
        let capture_state = Arc::new(SharedCaptureState::new(capture_cmd_tx));
        let startup_cancelled = Arc::clone(&cancelled);

        let vbc = Arc::clone(&video_bc);
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let abc = Arc::clone(&audio_bc);
        let cs = Arc::clone(&capture_state);
        let input_owner = Arc::clone(&state.input_pipeline_owner);

        let handle = std::thread::spawn(move || {
            run_shared_pipeline(
                instance_id,
                shutdown_rx,
                startup_cancelled,
                status_tx,
                video_capabilities,
                input,
                control,
                vbc,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                abc,
                cs,
                capture_cmd_rx,
                input_owner,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if cancelled.load(Ordering::Acquire) || control.shutdown_requested() {
                break Err("pipeline startup was cancelled".to_string());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Err("pipeline startup timed out".to_string());
            }
            match status_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(status) => break Ok(status),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    break Err("pipeline startup thread exited without status".to_string())
                }
            }
        };

        match status {
            Ok(PipelineResult::Started(_stream_config, rate_control, session_debug)) => Ok((
                Self {
                    instance_id,
                    video_bc: Arc::clone(&video_bc),
                    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                    audio_bc: Arc::clone(&audio_bc),
                    session_debug,
                    rate_control,
                    capture_state,
                    shutdown_tx,
                    pipeline_handle: handle,
                },
                ClientSubscription {
                    pipeline_instance_id: instance_id,
                    vid_sub_id,
                    vid_rx: Some(vid_rx),
                    video_bc: Arc::clone(&video_bc),
                    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                    aud_sub_id,
                    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                    aud_rx: Some(aud_rx),
                    _cleanup: SubscriptionCleanup {
                        state: Arc::downgrade(state),
                        pipeline_instance_id: instance_id,
                        vid_sub_id,
                        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                        aud_sub_id,
                    },
                },
            )),
            Ok(PipelineResult::Error(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                let _ = shutdown_tx.send(());
                state.pipeline_stop.register(handle);
                Err(error)
            }
        }
    }

    fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.pipeline_handle.join();
    }
}

/// Per-client subscription handles.
struct SubscriptionCleanup {
    state: Weak<ServerState>,
    pipeline_instance_id: u64,
    vid_sub_id: u64,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    aud_sub_id: u64,
}

impl Drop for SubscriptionCleanup {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            unsubscribe_and_maybe_stop_pipeline(
                &state,
                self.pipeline_instance_id,
                self.vid_sub_id,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                self.aud_sub_id,
            );
        }
    }
}

struct ClientSubscription {
    pipeline_instance_id: u64,
    vid_sub_id: u64,
    vid_rx: Option<Receiver<Arc<EncodedVideoFrame>>>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    aud_sub_id: u64,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    aud_rx: Option<Receiver<Arc<EncodedAudioPacket>>>,
    _cleanup: SubscriptionCleanup,
}

struct ClientPipelineReservation {
    pipeline_instance_id: u64,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    video: SubscriptionReservation<EncodedVideoFrame>,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    audio: SubscriptionReservation<EncodedAudioPacket>,
    rate_control: Arc<AdaptiveBitrateState>,
    capture_state: Arc<SharedCaptureState>,
    state: Weak<ServerState>,
}

impl ClientPipelineReservation {
    fn new(pipeline: &SharedPipeline, state: &Arc<ServerState>) -> Result<Self, String> {
        let video = pipeline.video_bc.reserve()?;
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let audio = pipeline.audio_bc.reserve()?;
        Ok(Self {
            pipeline_instance_id: pipeline.instance_id,
            video_bc: Arc::clone(&pipeline.video_bc),
            video,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            audio,
            rate_control: Arc::clone(&pipeline.rate_control),
            capture_state: Arc::clone(&pipeline.capture_state),
            state: Arc::downgrade(state),
        })
    }

    fn commit(self, snapshot: StreamSnapshot) -> PipelineSetup {
        let (vid_sub_id, vid_rx) = self.video.commit(VIDEO_SUBSCRIBER_CAPACITY);
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let (aud_sub_id, aud_rx) = self.audio.commit(30);
        let session_debug = self
            .capture_state
            .debug_snapshot()
            .expect("active pipeline has no debug snapshot")
            .info;
        (
            ClientSubscription {
                pipeline_instance_id: self.pipeline_instance_id,
                vid_sub_id,
                vid_rx: Some(vid_rx),
                video_bc: self.video_bc,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                aud_sub_id,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                aud_rx: Some(aud_rx),
                _cleanup: SubscriptionCleanup {
                    state: self.state,
                    pipeline_instance_id: self.pipeline_instance_id,
                    vid_sub_id,
                    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                    aud_sub_id,
                },
            },
            snapshot,
            self.rate_control,
            session_debug,
            self.capture_state,
        )
    }
}

/// Global server state shared across all client handlers.
struct ServerState {
    pipeline: Mutex<Option<SharedPipeline>>,
    pipeline_starting: AtomicBool,
    pipeline_stop: Arc<PipelineStopCoordinator>,
    admission: Arc<AdmissionCoordinator>,
    /// Serializes exact-instance subscribe/unsubscribe/start/stop operations.
    pipeline_lifecycle: Mutex<()>,
    /// Registry revision checked by asynchronous encoder workers before commit.
    profile_revision: Arc<AtomicU64>,
    /// Makes registry mutations and encoder-profile commits mutually exclusive.
    profile_commit: Arc<Mutex<()>>,
    input_pipeline_owner: Arc<AtomicU64>,
    video_capabilities: Mutex<VideoCapabilityRegistry>,
    input: Arc<InputRuntime>,
    control: Arc<ServerControl>,
    listen_port: u16,
    /// Tunnel state from the API registration thread (key exchange + partner candidates).
    tunnel_state: Option<Arc<api_client::ApiTunnelState>>,
}

#[derive(Default)]
struct PipelineStopCoordinator {
    stopping: Mutex<bool>,
    changed: Condvar,
}

impl PipelineStopCoordinator {
    fn register(self: &Arc<Self>, handle: std::thread::JoinHandle<()>) {
        {
            let mut stopping = self.stopping.lock().unwrap();
            assert!(!*stopping, "pipeline stop already in progress");
            *stopping = true;
        }
        let coordinator = Arc::clone(self);
        std::thread::spawn(move || {
            let _ = handle.join();
            *coordinator.stopping.lock().unwrap() = false;
            coordinator.changed.notify_all();
        });
    }

    fn wait(&self, cancelled: &AtomicBool) -> Result<(), String> {
        let mut stopping = self.stopping.lock().unwrap();
        while *stopping {
            if cancelled.load(Ordering::Acquire) {
                return Err("pipeline setup cancelled while prior capture was stopping".into());
            }
            stopping = self
                .changed
                .wait_timeout(stopping, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_stopping(&self) -> bool {
        *self.stopping.lock().unwrap()
    }
}

#[derive(Default)]
struct AdmissionState {
    active: Option<u64>,
    waiting: BTreeSet<u64>,
}

#[derive(Default)]
struct AdmissionCoordinator {
    next: AtomicU64,
    state: Mutex<AdmissionState>,
    changed: Condvar,
}

impl AdmissionCoordinator {
    fn acquire(self: &Arc<Self>, cancelled: Arc<AtomicBool>) -> Result<AdmissionPermit, String> {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        state.waiting.insert(ticket);
        loop {
            if cancelled.load(Ordering::Acquire) {
                state.waiting.remove(&ticket);
                self.changed.notify_all();
                return Err("client admission was cancelled".into());
            }
            if state.active.is_none() && state.waiting.first() == Some(&ticket) {
                state.waiting.remove(&ticket);
                state.active = Some(ticket);
                return Ok(AdmissionPermit {
                    token: ticket,
                    coordinator: Arc::clone(self),
                    cancelled,
                });
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }

    fn is_current(&self, token: u64) -> bool {
        self.state.lock().unwrap().active == Some(token)
    }
}

struct AdmissionPermit {
    token: u64,
    coordinator: Arc<AdmissionCoordinator>,
    cancelled: Arc<AtomicBool>,
}

impl AdmissionPermit {
    fn is_current(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.coordinator.is_current(self.token)
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut state = self.coordinator.state.lock().unwrap();
        if state.active == Some(self.token) {
            state.active = None;
        }
        self.coordinator.changed.notify_all();
    }
}

#[derive(Clone)]
struct AdmissionStamp {
    token: u64,
    coordinator: Arc<AdmissionCoordinator>,
}

impl AdmissionStamp {
    fn is_current(&self) -> bool {
        self.coordinator.is_current(self.token)
    }
}

struct PipelineStartGuard<'a>(&'a AtomicBool);

impl Drop for PipelineStartGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct VideoMembership {
    id: u64,
    cancelled: Arc<AtomicBool>,
    state: Weak<ServerState>,
}

impl Drop for VideoMembership {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.cancelled.store(true, Ordering::Release);
        let (removed, has_members) = {
            let _commit_guard = state.profile_commit.lock().unwrap();
            let mut registry = state.video_capabilities.lock().unwrap();
            let removed = registry.remove(self.id);
            state
                .profile_revision
                .store(registry.revision(), Ordering::Release);
            (removed, !registry.is_empty())
        };
        if removed.is_some() && has_members {
            std::thread::spawn(move || {
                if let Err(error) = reconcile_video_profile(&state) {
                    eprintln!("[pipeline] remaining-client profile upgrade failed: {error}");
                }
            });
        }
    }
}

fn begin_video_membership(
    state: &Arc<ServerState>,
    capabilities: ClientVideoCapabilities,
) -> VideoMembership {
    let _commit_guard = state.profile_commit.lock().unwrap();
    let mut registry = state.video_capabilities.lock().unwrap();
    let id = registry.insert_tentative(capabilities);
    state
        .profile_revision
        .store(registry.revision(), Ordering::Release);
    VideoMembership {
        id,
        cancelled: Arc::new(AtomicBool::new(false)),
        state: Arc::downgrade(state),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
struct PendingEncoderRebuild {
    revision: u64,
    config: EncoderConfig,
    backend: EncoderBackend,
    purpose: RebuildPurpose,
    rx: Receiver<Result<(EncoderConfig, EncoderKind, EncoderBackend), String>>,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
enum RebuildPurpose {
    Bitrate,
    FrameRate,
    Profile(ProfileRequest),
}

/// Build a new encoder for `config`/`backend` on a background thread (so the
/// capture/encode loop never blocks on encoder open) and return the handle the
/// loop polls. The rebuilt encoder is swapped in — and starts with a keyframe —
/// once it is ready.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn spawn_encoder_rebuild(
    config: EncoderConfig,
    backend: EncoderBackend,
    revision: u64,
    purpose: RebuildPurpose,
) -> PendingEncoderRebuild {
    let (rebuild_tx, rebuild_rx) = bounded(1);
    let rebuild_config = config.clone();
    std::thread::spawn(move || {
        let result = create_encoder_for_backend(&rebuild_config, backend)
            .map(|encoder| (rebuild_config, encoder, backend));
        let _ = rebuild_tx.send(result);
    });
    PendingEncoderRebuild {
        revision,
        config,
        backend,
        purpose,
        rx: rebuild_rx,
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn rebuild_revision_is_current(result_revision: u64, current_revision: u64) -> bool {
    result_revision == current_revision
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn spawn_profile_encoder_rebuild(
    current: EncoderConfig,
    control: Arc<ServerControl>,
    capture_render_hint: Option<String>,
    revision: u64,
    request: ProfileRequest,
) -> PendingEncoderRebuild {
    let (rebuild_tx, rebuild_rx) = bounded(1);
    let placeholder = current.clone();
    let capabilities = request.capabilities;
    std::thread::spawn(move || {
        let result = open_linux_encoder_for_aggregate(
            &current,
            capabilities,
            &control,
            capture_render_hint.as_deref(),
        )
        .map(|(config, encoder)| {
            let backend = encoder_backend(&encoder);
            (config, encoder, backend)
        });
        let _ = rebuild_tx.send(result);
    });
    PendingEncoderRebuild {
        revision,
        // Replaced by the selected config in the worker result.
        config: placeholder,
        backend: EncoderBackend::Software,
        purpose: RebuildPurpose::Profile(request),
        rx: rebuild_rx,
    }
}

#[cfg(target_os = "windows")]
fn spawn_profile_encoder_rebuild(
    config: EncoderConfig,
    current_backend: EncoderBackend,
    revision: u64,
    request: ProfileRequest,
) -> PendingEncoderRebuild {
    let (rebuild_tx, rebuild_rx) = bounded(1);
    let rebuild_config = config.clone();
    std::thread::spawn(move || {
        let result = create_encoder_for_backend(&rebuild_config, current_backend)
            .map(|encoder| (rebuild_config, encoder, current_backend));
        let _ = rebuild_tx.send(result);
    });
    PendingEncoderRebuild {
        revision,
        config,
        backend: current_backend,
        purpose: RebuildPurpose::Profile(request),
        rx: rebuild_rx,
    }
}

// ---------------------------------------------------------------------------
// Shared pipeline thread
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_shared_pipeline(
    instance_id: u64,
    shutdown_rx: Receiver<()>,
    startup_cancelled: Arc<AtomicBool>,
    status_tx: Sender<PipelineResult>,
    video_capabilities: AggregateVideoCapabilities,
    input: Arc<InputRuntime>,
    control: Arc<ServerControl>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))] audio_bc: Arc<
        Broadcaster<EncodedAudioPacket>,
    >,
    capture_state: Arc<SharedCaptureState>,
    capture_cmd_rx: Receiver<CaptureCommand>,
    input_pipeline_owner: Arc<AtomicU64>,
) {
    let (frame_tx, mut frame_rx) = bounded(CAPTURE_QUEUE_CAPACITY);
    // Capture-command processing (output switching) is only meaningful on the
    // KMS path (Linux); on other platforms drain-suppress the unused channel.
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = &capture_cmd_rx;
    let trace = trace_enabled();

    let negotiated_fps = EncoderConfig::resolve_target_fps(video_capabilities.requested_fps);
    capture::set_target_fps(negotiated_fps);
    println!(
        "[pipeline] capture fps target={} (client_request={:?}fps, ST_FPS cap={:?})",
        negotiated_fps,
        video_capabilities.requested_fps,
        EncoderConfig::fps_cap_from_env()
    );

    let mut capture_backend = PlatformCapture::new();
    if startup_cancelled.load(Ordering::Acquire) {
        let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
        return;
    }
    if let Err(e) = capture_backend.start(frame_tx) {
        let msg = format!("Failed to start capture: {e}");
        eprintln!("{msg}");
        let _ = status_tx.send(PipelineResult::Error(msg));
        return;
    }
    if startup_cancelled.load(Ordering::Acquire) {
        capture_backend.stop();
        let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
        return;
    }

    // Get first frame to determine dimensions
    let (first_frame, first_frame_captured_micros) = loop {
        if startup_cancelled.load(Ordering::Acquire) {
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
            return;
        }
        crossbeam_channel::select! {
            recv(frame_rx) -> frame => match frame {
                Ok(frame) => break (frame, unix_time_micros()),
                Err(_) => {
                    let msg = "Capture channel closed before first frame".to_string();
                    eprintln!("{msg}");
                    capture_backend.stop();
                    let _ = status_tx.send(PipelineResult::Error(msg));
                    return;
                }
            },
            recv(shutdown_rx) -> _ => {
                startup_cancelled.store(true, Ordering::Release);
            },
            default(Duration::from_millis(100)) => {}
        }
    };
    if trace {
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        let first_has_cursor = first_frame.cursor.is_some();
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        let first_has_cursor = false;
        eprintln!(
            "[trace][server] first captured frame: {}x{} cursor={} capture_ts={}",
            first_frame.width, first_frame.height, first_has_cursor, first_frame_captured_micros
        );
    }
    let mut trace_capture_frames = 1usize;

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let audio_config = encode_config::AudioConfig::from_env();
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let audio_wire_kbps = adaptive_bitrate::audio_wire_kbps(
        audio_config.bitrate,
        audio_config.packet_duration_ms,
        transport::configured_audio_redundancy_depth(),
    );

    #[cfg(target_os = "linux")]
    let capture_render_hint = capture_backend
        .capture_render_node()
        .map(|s| s.to_string())
        .or_else(|| {
            // PipeWire/Wayland capture doesn't directly know its GPU.
            // Probe DRM cards to find the display GPU's render node.
            capture::linux::probe_display_gpu_render_node()
        });
    #[cfg(target_os = "linux")]
    let (config, mut encoder) = match select_linux_encoder(
        first_frame.width,
        first_frame.height,
        negotiated_fps,
        video_capabilities.supported_codecs,
        video_capabilities.hardware_codecs,
        video_capabilities.supported_yuv444_codecs,
        video_capabilities.hardware_yuv444_codecs,
        video_capabilities.hdr_display,
        &control,
        capture_render_hint.as_deref(),
    ) {
        Ok(selected) => selected,
        Err(msg) => {
            eprintln!("{msg}");
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error(msg));
            return;
        }
    };

    if startup_cancelled.load(Ordering::Acquire) {
        capture_backend.stop();
        let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
        return;
    }

    #[cfg(target_os = "windows")]
    let (config, mut encoder) = match select_windows_encoder(
        &first_frame,
        negotiated_fps,
        video_capabilities.supported_codecs,
        video_capabilities.supported_yuv444_codecs,
        video_capabilities.hdr_display,
        &control,
    ) {
        Ok(selected) => selected,
        Err(msg) => {
            eprintln!("{msg}");
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error(msg));
            return;
        }
    };

    #[cfg(target_os = "windows")]
    if startup_cancelled.load(Ordering::Acquire) {
        capture_backend.stop();
        let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
        return;
    }

    #[cfg(target_os = "macos")]
    let (config, mut encoder) = match select_macos_encoder(
        first_frame.width,
        first_frame.height,
        negotiated_fps,
        video_capabilities.supported_codecs,
        video_capabilities.supported_yuv444_codecs,
        &control,
    ) {
        Ok(selected) => selected,
        Err(e) => {
            let msg = format!("Failed to create encoder: {e}");
            eprintln!("{msg}");
            unsafe {
                CVPixelBufferRelease(first_frame.pixel_buffer_ptr);
            }
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error(msg));
            return;
        }
    };

    #[cfg(target_os = "macos")]
    if startup_cancelled.load(Ordering::Acquire) {
        capture_backend.stop();
        let _ = status_tx.send(PipelineResult::Error("Pipeline startup cancelled".into()));
        return;
    }

    let forced_bitrate = control.forced_bitrate_kbps();
    let rate_control = if forced_bitrate > 0 {
        Arc::new(AdaptiveBitrateState::new(
            forced_bitrate,
            forced_bitrate,
            forced_bitrate,
        ))
    } else {
        Arc::new(AdaptiveBitrateState::new(
            config.bitrate_kbps,
            config.min_bitrate_kbps,
            config.max_bitrate_kbps,
        ))
    };

    // Start audio pipeline — encode + relay run persistently; capture is
    // attached on-demand against whatever PulseAudio / PipeWire daemon is
    // visible to this process. Works because st-server runs in the user's
    // session and inherits XDG_RUNTIME_DIR / PULSE_SERVER naturally.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let audio_pipeline_shared = {
        let mut ap = audio::AudioPipeline::new();
        match ap.start(audio_config.clone(), audio_bc) {
            Ok(()) => println!("[pipeline] Audio pipeline started"),
            Err(e) => eprintln!("[pipeline] Audio pipeline failed (video-only): {e}"),
        }
        ap.apply_auto_detect();
        Arc::new(std::sync::Mutex::new(ap))
    };
    // System-wide mode: follow the active seat's user so audio re-attaches to
    // whoever is logged in. No-op in per-user mode (already in the session).
    #[cfg(target_os = "linux")]
    session_follow::maybe_spawn(Arc::clone(&audio_pipeline_shared));
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let audio_pipeline = Arc::clone(&audio_pipeline_shared);

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let stream_config = config.to_stream_config(&audio_config);
    let initial_snapshot = capture_state.initialize_config(stream_config);

    let capture_backend_name = capture_backend.backend_name().to_string();
    input_pipeline_owner.store(instance_id, Ordering::Release);
    input.refresh_backend(
        &capture_backend_name,
        stream_config.width,
        stream_config.height,
    );
    let session_debug =
        build_session_debug(&encoder, &config, &capture_backend_name, input.as_ref());
    capture_state.initialize_debug(session_debug.clone());

    println!(
        "Shared pipeline started: {}x{} (video: {:?} {:?} {:?})",
        first_frame.width, first_frame.height, config.codec, config.dynamic_range, config.chroma,
    );

    // Publish the capturable outputs so connecting clients can offer a monitor
    // picker. Empty on backends that can't enumerate (portal fallback, macOS,
    // Windows) — the client then hides the picker. Default the reported
    // selection to the primary so the client highlights it without re-sending.
    {
        let outputs = capture_backend.list_outputs();
        if let Some(primary) = outputs.iter().find(|o| o.is_primary) {
            capture_state
                .selected_output
                .store(primary.id, Ordering::SeqCst);
        }
        if outputs.len() > 1 {
            println!("[pipeline] {} capturable outputs available", outputs.len());
        }
        *capture_state.available_outputs.lock().unwrap() = outputs;
    }

    // Tell the control plane we started OK
    let _ = status_tx.send(PipelineResult::Started(
        stream_config,
        Arc::clone(&rate_control),
        session_debug,
    ));

    // Encode and broadcast the first frame
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let mut current_config = config.clone();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut pending_encoder_rebuild: Option<PendingEncoderRebuild> = None;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut encoder_revision = 1u64;
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let mut last_encoder_reconfigure = Instant::now();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut bitrate_verifier = BitrateVerifier::new(current_config.framerate, Instant::now());
    // Adaptive encode frame-rate: steps fps down when the box can't sustain the
    // target cadence (regular cadence → smaller client jitter buffer → lower
    // latency) and probes back up on sustained headroom. Ceiling = the
    // negotiated target; default-on, `ST_ADAPTIVE_FPS=0` forces the fixed rate.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut adaptive_fps =
        adaptive_bitrate::AdaptiveFrameRate::from_env(current_config.framerate, Instant::now());
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut frame_rate_tracker = adaptive_bitrate::EncodeRateTracker::new(Instant::now());
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if adaptive_fps.enabled() {
        println!(
            "[adapt-fps] enabled, ceiling {} fps (current {}); ST_ADAPTIVE_FPS=0 to disable",
            current_config.framerate,
            adaptive_fps.current_fps()
        );
    } else {
        println!(
            "[adapt-fps] disabled (ST_ADAPTIVE_FPS); fixed at {} fps",
            current_config.framerate
        );
    }
    encode_and_broadcast(
        &mut encoder,
        &video_bc,
        input.as_ref(),
        &first_frame,
        first_frame_captured_micros,
        initial_snapshot.video_epoch,
    );

    let mut observed_output_generation = 0u64;
    // Main loop
    'pipeline: loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        // Apply any pending capture/output switch. Rare, user-initiated, and
        // global to the shared stream: stop+restart capture pinned to the new
        // monitor, rebuild the encoder if the resolution changed, then publish
        // the new StreamConfig so every client re-syncs and gets a keyframe.
        let mut next_command = capture_state
            .pending_output(observed_output_generation)
            .map(|(generation, id)| {
                observed_output_generation = generation;
                CaptureCommand::SelectOutput(id)
            });
        loop {
            let Some(cmd) = next_command
                .take()
                .or_else(|| capture_cmd_rx.try_recv().ok())
            else {
                break;
            };
            match cmd {
                CaptureCommand::SelectOutput(id) => {
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    {
                        if !capture_backend.select_output(id) {
                            continue;
                        }
                        println!("[pipeline] switching capture to output id {id}");
                        capture_backend.stop();
                        let (new_tx, new_rx) = bounded(CAPTURE_QUEUE_CAPACITY);
                        if let Err(e) = capture_backend.start(new_tx) {
                            eprintln!(
                            "[pipeline] capture restart after output switch failed: {e}; stopping pipeline"
                        );
                            break 'pipeline;
                        }
                        frame_rx = new_rx;
                        let switched_frame = match frame_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                        {
                            Ok(f) => f,
                            Err(_) => {
                                eprintln!(
                                    "[pipeline] no frame within 5s after output switch; stopping pipeline"
                                );
                                break 'pipeline;
                            }
                        };
                        if switched_frame.width != current_config.width
                            || switched_frame.height != current_config.height
                        {
                            encoder_revision = encoder_revision.wrapping_add(1);
                            if let Some(pending) = pending_encoder_rebuild.take() {
                                if let RebuildPurpose::Profile(request) = pending.purpose {
                                    request.reject(
                                        "capture changed while the video profile was opening",
                                    );
                                }
                            }
                            let mut new_config = current_config.clone();
                            new_config.width = switched_frame.width;
                            new_config.height = switched_frame.height;
                            let backend = encoder_backend(&encoder);
                            match create_encoder_for_backend(&new_config, backend) {
                                Ok(mut new_encoder) => {
                                    request_next_keyframe(&mut new_encoder);
                                    encoder = new_encoder;
                                    current_config = new_config;
                                }
                                Err(e) => {
                                    eprintln!(
                                    "[pipeline] encoder rebuild for switched output failed: {e}; stopping pipeline"
                                );
                                    break 'pipeline;
                                }
                            }
                        } else {
                            request_next_keyframe(&mut encoder);
                        }
                        capture_state.selected_output.store(id, Ordering::SeqCst);
                        capture_state.commit_output_selection(
                            current_config.to_stream_config(&audio_config),
                        );
                        video_bc.clear_queued();
                        input.update_stream_dimensions(
                            current_config.width,
                            current_config.height,
                        );
                        println!(
                            "[pipeline] output switch complete: {}x{}",
                            current_config.width, current_config.height
                        );
                        encode_and_broadcast(
                            &mut encoder,
                            &video_bc,
                            input.as_ref(),
                            &switched_frame,
                            unix_time_micros(),
                            capture_state.active_video_epoch(),
                        );
                    }
                }
                CaptureCommand::SetVideoCapabilities(request) => {
                    if !request.is_current() {
                        request.reject("video profile request was cancelled");
                        continue;
                    }
                    #[cfg(any(target_os = "linux", target_os = "windows"))]
                    {
                        let desired = match aggregate_encoder_config(
                            &current_config,
                            request.capabilities,
                            &control,
                        ) {
                            Ok(desired) => desired,
                            Err(error) => {
                                request.reject(error);
                                continue;
                            }
                        };
                        if encoder_profiles_equal(&current_config, &desired) {
                            let _commit_guard = request.commit_lock.lock().unwrap();
                            if !request.is_current() {
                                drop(_commit_guard);
                                request.reject("video profile request became stale");
                                continue;
                            }
                            let snapshot = capture_state
                                .snapshot()
                                .expect("active pipeline has no stream config");
                            let _ = request.response.send(Ok(snapshot));
                            continue;
                        }
                        encoder_revision = encoder_revision.wrapping_add(1);
                        if let Some(pending) = pending_encoder_rebuild.take() {
                            if let RebuildPurpose::Profile(previous_request) = pending.purpose {
                                previous_request.reject("video profile request was superseded");
                            }
                        }
                        #[cfg(target_os = "linux")]
                        {
                            pending_encoder_rebuild = Some(spawn_profile_encoder_rebuild(
                                current_config.clone(),
                                Arc::clone(&control),
                                capture_render_hint.clone(),
                                encoder_revision,
                                request,
                            ));
                        }
                        #[cfg(target_os = "windows")]
                        {
                            let backend = encoder_backend(&encoder);
                            if backend != EncoderBackend::Software {
                                request.reject(format!(
                                    "dynamic video profile changes are unavailable on the active Windows {} encoder; reconnect with a compatible profile",
                                    encoder_backend_name(backend)
                                ));
                                continue;
                            }
                            pending_encoder_rebuild = Some(spawn_profile_encoder_rebuild(
                                desired,
                                backend,
                                encoder_revision,
                                request,
                            ));
                        }
                    }
                    #[cfg(target_os = "macos")]
                    {
                        let next_config = if request
                            .capabilities
                            .supported_codecs
                            .supports(VideoCodec::H264)
                        {
                            let mut config = current_config.clone();
                            config.codec = encode_config::Codec::H264;
                            config.chroma = encode_config::ChromaSampling::Yuv420;
                            config.dynamic_range = encode_config::DynamicRange::Sdr;
                            config.framerate = EncoderConfig::resolve_target_fps(
                                request.capabilities.requested_fps,
                            );
                            Ok(config)
                        } else {
                            Err(
                                "VideoToolbox server has no codec in common with every client"
                                    .into(),
                            )
                        };
                        match next_config.and_then(|next_config| {
                            if encoder_profiles_equal(&current_config, &next_config) {
                                return Ok((next_config, None));
                            }
                            encode_vt::VTEncoder::new(
                                next_config.width,
                                next_config.height,
                                next_config.bitrate_bps().min(u32::MAX as i64) as u32,
                                next_config.framerate,
                            )
                            .map(|next_encoder| (next_config, Some(next_encoder)))
                        }) {
                            Ok((next_config, next_encoder)) => {
                                let commit_guard = request.commit_lock.lock().unwrap();
                                if !request.is_current() {
                                    drop(commit_guard);
                                    request.reject("video profile request became stale");
                                    continue;
                                }
                                let rebuilt = next_encoder.is_some();
                                if let Some(next_encoder) = next_encoder {
                                    encoder = next_encoder;
                                }
                                current_config = next_config;
                                capture::set_target_fps(current_config.framerate);
                                let snapshot = if rebuilt {
                                    let snapshot = capture_state.commit_encoder_config(
                                        current_config.to_stream_config(&audio_config),
                                    );
                                    video_bc.clear_queued();
                                    snapshot
                                } else {
                                    capture_state
                                        .snapshot()
                                        .expect("active pipeline has no stream config")
                                };
                                let _ = request.response.send(Ok(snapshot));
                            }
                            Err(error) => {
                                request.reject(error);
                            }
                        }
                    }
                }
            }
        }

        let (frame, frame_captured_micros) =
            match frame_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(f) => (f, unix_time_micros()),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
        if trace && trace_capture_frames < 8 {
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            let frame_has_cursor = frame.cursor.is_some();
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            let frame_has_cursor = false;
            eprintln!(
                "[trace][server] captured frame #{trace_capture_frames}: {}x{} cursor={} capture_ts={}",
                frame.width,
                frame.height,
                frame_has_cursor,
                frame_captured_micros
            );
            trace_capture_frames += 1;
        }
        // Drain stale frames — only encode the newest
        let (frame, frame_captured_micros) = {
            let mut latest = frame;
            let mut latest_captured_micros = frame_captured_micros;
            // A capture-requested keyframe (e.g. KMS seat/session switch) must
            // survive frame-dropping — OR it across every drained frame.
            let mut force_keyframe = latest.force_keyframe;
            while let Ok(newer) = frame_rx.try_recv() {
                #[cfg(target_os = "macos")]
                unsafe {
                    CVPixelBufferRelease(latest.pixel_buffer_ptr);
                }
                force_keyframe |= newer.force_keyframe;
                latest = newer;
                latest_captured_micros = unix_time_micros();
            }
            latest.force_keyframe = force_keyframe;
            (latest, latest_captured_micros)
        };

        // Only encode when there are subscribers (save GPU/CPU when idle)
        if video_bc.subscriber_count() > 0 {
            // Live resolution change *not* driven by SelectOutput: the active
            // scanout started delivering a different size (remote display mode /
            // KDE scale change, monitor hotplug). Rebuild the encoder on the same
            // backend, re-publish StreamConfig so every client re-fits the video
            // and remaps cursor coordinates against the new dimensions, and force
            // a keyframe so the post-change bitstream is decodable. Gated to the
            // backends with the rebuild helpers (macOS VT manages this itself).
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if frame.width != current_config.width || frame.height != current_config.height {
                println!(
                    "[pipeline] capture resolution changed {}x{} -> {}x{}; reconfiguring stream",
                    current_config.width, current_config.height, frame.width, frame.height
                );
                let mut new_config = current_config.clone();
                new_config.width = frame.width;
                new_config.height = frame.height;
                encoder_revision = encoder_revision.wrapping_add(1);
                if let Some(pending) = pending_encoder_rebuild.take() {
                    if let RebuildPurpose::Profile(request) = pending.purpose {
                        request.reject("capture resolution changed during profile setup");
                    }
                }
                let backend = encoder_backend(&encoder);
                match create_encoder_for_backend(&new_config, backend) {
                    Ok(mut new_encoder) => {
                        request_next_keyframe(&mut new_encoder);
                        encoder = new_encoder;
                        current_config = new_config;
                        capture_state
                            .commit_encoder_config(current_config.to_stream_config(&audio_config));
                        input.update_stream_dimensions(
                            current_config.width,
                            current_config.height,
                        );
                        video_bc.clear_queued();
                    }
                    Err(e) => {
                        eprintln!(
                            "[pipeline] encoder rebuild for resolution change failed: {e}; stopping pipeline"
                        );
                        break 'pipeline;
                    }
                }
            }
            // Force IDR when a new subscriber just joined (so it can start decoding)
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if video_bc.take_keyframe_request() {
                if trace {
                    eprintln!("[trace][server] taking pending keyframe request");
                }
                request_next_keyframe(&mut encoder);
            }
            // A capture backend can demand a keyframe for a content discontinuity
            // the encoder can't infer from dimensions (KMS seat/user switch at the
            // same resolution).
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if frame.force_keyframe {
                if trace {
                    eprintln!("[trace][server] capture demanded keyframe (seat/session switch)");
                }
                request_next_keyframe(&mut encoder);
            }
            #[cfg(target_os = "macos")]
            let _ = video_bc.take_keyframe_request(); // VT encoder always starts with IDR

            #[cfg(target_os = "macos")]
            {
                let forced_br = control.forced_bitrate_kbps();
                let target_bitrate = if forced_br > 0 {
                    forced_br
                } else {
                    adaptive_bitrate::encoder_target_kbps(
                        rate_control.current_target_kbps(),
                        audio_wire_kbps,
                        adaptive_bitrate::fec_reserve_pct_from_env(),
                        current_config.min_bitrate_kbps,
                    )
                };
                if should_schedule_bitrate_reconfigure(
                    current_config.bitrate_kbps,
                    target_bitrate,
                    last_encoder_reconfigure,
                ) {
                    let next_config = if forced_br > 0 {
                        let mut c = current_config.clone();
                        c.bitrate_kbps = forced_br;
                        c
                    } else {
                        current_config.with_bitrate_kbps(target_bitrate)
                    };
                    match encoder
                        .update_bitrate(next_config.bitrate_bps().min(u32::MAX as i64) as u32)
                    {
                        Ok(()) => {
                            println!(
                                "[abr] videotoolbox bitrate {} -> {} kbps",
                                current_config.bitrate_kbps, next_config.bitrate_kbps
                            );
                            current_config = next_config;
                            last_encoder_reconfigure = Instant::now();
                        }
                        Err(err) => {
                            eprintln!(
                                "[abr] videotoolbox bitrate update failed at {} kbps: {err}",
                                next_config.bitrate_kbps
                            );
                            rate_control.reset_all_clients(current_config.bitrate_kbps);
                        }
                    }
                }
            }

            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                let rebuild_result = if let Some(pending) = pending_encoder_rebuild.as_ref() {
                    match pending.rx.try_recv() {
                        Ok(result) => Some(result),
                        Err(crossbeam_channel::TryRecvError::Empty) => None,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            Some(Err("encoder rebuild worker disconnected".to_string()))
                        }
                    }
                } else {
                    None
                };

                if let Some(result) = rebuild_result {
                    let pending = pending_encoder_rebuild
                        .take()
                        .expect("pending rebuild missing after result");
                    if !rebuild_revision_is_current(pending.revision, encoder_revision) {
                        if let RebuildPurpose::Profile(request) = pending.purpose {
                            request.reject("stale encoder rebuild discarded");
                        }
                        continue;
                    }
                    match result {
                        Ok((next_config, mut next_encoder, next_backend)) => {
                            let profile_commit_guard = if let RebuildPurpose::Profile(request) =
                                &pending.purpose
                            {
                                let guard = request.commit_lock.lock().unwrap();
                                if !request.is_current() {
                                    drop(guard);
                                    if let RebuildPurpose::Profile(request) = pending.purpose {
                                        request
                                            .reject("cancelled encoder rebuild result discarded");
                                    }
                                    continue;
                                }
                                Some(guard)
                            } else {
                                None
                            };
                            let fps_changed = next_config.framerate != current_config.framerate;
                            let profile_change =
                                matches!(&pending.purpose, RebuildPurpose::Profile(_));
                            if profile_change {
                                println!(
                                    "[pipeline] activating shared video profile: {} {} {}fps",
                                    codec_name(next_config.stream_codec()),
                                    encoder_chroma_name(next_config.chroma),
                                    next_config.framerate,
                                );
                            } else if fps_changed {
                                println!(
                                    "[adapt-fps] {} now encoding at {} fps",
                                    encoder_backend_name(next_backend),
                                    next_config.framerate
                                );
                            } else {
                                println!(
                                    "[abr] {} bitrate {} -> {} kbps",
                                    encoder_backend_name(next_backend),
                                    current_config.bitrate_kbps,
                                    next_config.bitrate_kbps
                                );
                            }
                            request_next_keyframe(&mut next_encoder);
                            encoder = next_encoder;
                            current_config = next_config;
                            last_encoder_reconfigure = Instant::now();
                            if fps_changed {
                                capture::set_target_fps(current_config.framerate);
                                adaptive_fps = adaptive_bitrate::AdaptiveFrameRate::from_env(
                                    current_config.framerate,
                                    Instant::now(),
                                );
                            }
                            let snapshot = capture_state.commit_encoder_config(
                                current_config.to_stream_config(&audio_config),
                            );
                            video_bc.clear_queued();
                            drop(profile_commit_guard);
                            if let RebuildPurpose::Profile(request) = pending.purpose {
                                let _ = request.response.send(Ok(snapshot));
                            }
                        }
                        Err(err) => {
                            if let RebuildPurpose::Profile(request) = pending.purpose {
                                eprintln!("[pipeline] video profile rebuild failed: {err}");
                                request.reject(err);
                            } else {
                                eprintln!(
                                    "[abr] {} encoder rebuild failed at {} kbps: {err}",
                                    encoder_backend_name(pending.backend),
                                    pending.config.bitrate_kbps
                                );
                                rate_control.reset_all_clients(current_config.bitrate_kbps);
                            }
                        }
                    }
                }

                // A prior in-place *downward* change that the encoder silently
                // ignored is detected here by measured output rate; escalate it
                // to a rebuild so the bitrate actually drops during congestion.
                if pending_encoder_rebuild.is_none()
                    && bitrate_verifier.check_and_take_failure(Instant::now())
                {
                    let backend = encoder_backend(&encoder);
                    eprintln!(
                        "[abr] {} ignored in-place bitrate change; rebuilding at {} kbps{}",
                        encoder_backend_name(backend),
                        current_config.bitrate_kbps,
                        if bitrate_verifier.inplace_ineffective {
                            " (in-place disabled for this encoder)"
                        } else {
                            ""
                        }
                    );
                    encoder_revision = encoder_revision.wrapping_add(1);
                    pending_encoder_rebuild = Some(spawn_encoder_rebuild(
                        current_config.clone(),
                        backend,
                        encoder_revision,
                        RebuildPurpose::Bitrate,
                    ));
                }

                let forced_br = control.forced_bitrate_kbps();
                // ABR targets the on-wire budget; the encoder gets that minus FEC
                // parity + audio overhead (B3) so on-wire rate matches intent.
                let target_bitrate = if forced_br > 0 {
                    forced_br
                } else {
                    adaptive_bitrate::encoder_target_kbps(
                        rate_control.current_target_kbps(),
                        audio_wire_kbps,
                        adaptive_bitrate::fec_reserve_pct_from_env(),
                        current_config.min_bitrate_kbps,
                    )
                };
                if pending_encoder_rebuild.is_none()
                    && should_schedule_bitrate_reconfigure(
                        current_config.bitrate_kbps,
                        target_bitrate,
                        last_encoder_reconfigure,
                    )
                {
                    let next_config = if forced_br > 0 {
                        let mut c = current_config.clone();
                        c.bitrate_kbps = forced_br;
                        c
                    } else {
                        current_config.with_bitrate_kbps(target_bitrate)
                    };
                    let backend = encoder_backend(&encoder);
                    let old_kbps = current_config.bitrate_kbps;
                    let downward = next_config.bitrate_kbps < old_kbps;

                    // Skip the in-place attempt entirely once this encoder has
                    // proven it ignores runtime bitrate changes — rebuild straight
                    // away so ABR stays effective.
                    if bitrate_verifier.inplace_ineffective {
                        if trace {
                            eprintln!(
                                "[trace][server] scheduling {} bitrate rebuild {old_kbps} -> {} kbps (in-place known ineffective)",
                                encoder_backend_name(backend),
                                next_config.bitrate_kbps
                            );
                        }
                        encoder_revision = encoder_revision.wrapping_add(1);
                        pending_encoder_rebuild = Some(spawn_encoder_rebuild(
                            next_config,
                            backend,
                            encoder_revision,
                            RebuildPurpose::Bitrate,
                        ));
                    } else {
                        match update_encoder_bitrate(&mut encoder, &next_config) {
                            Ok(()) => {
                                println!(
                                    "[abr] {} bitrate {old_kbps} -> {} kbps (in-place)",
                                    encoder_backend_name(backend),
                                    next_config.bitrate_kbps
                                );
                                current_config = next_config;
                                last_encoder_reconfigure = Instant::now();
                                // Verify downward changes actually took effect.
                                if downward {
                                    bitrate_verifier
                                        .arm_downward(current_config.bitrate_kbps, Instant::now());
                                }
                            }
                            Err(err) => {
                                if trace {
                                    eprintln!(
                                        "[trace][server] {} in-place bitrate update failed: {err}; scheduling rebuild {old_kbps} -> {} kbps",
                                        encoder_backend_name(backend),
                                        next_config.bitrate_kbps
                                    );
                                }
                                encoder_revision = encoder_revision.wrapping_add(1);
                                pending_encoder_rebuild = Some(spawn_encoder_rebuild(
                                    next_config,
                                    backend,
                                    encoder_revision,
                                    RebuildPurpose::Bitrate,
                                ));
                            }
                        }
                    }
                }
            }
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let encode_start = Instant::now();
            let _encoded_bytes = encode_and_broadcast(
                &mut encoder,
                &video_bc,
                input.as_ref(),
                &frame,
                frame_captured_micros,
                capture_state.active_video_epoch(),
            );
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                bitrate_verifier.record(_encoded_bytes);
                let now = Instant::now();
                let encode_us = now.duration_since(encode_start).as_micros() as u64;
                let budget_us = (1_000_000u64 / current_config.framerate.max(1) as u64).max(1);
                frame_rate_tracker.record(encode_us, budget_us);
                // Don't stack an fps rebuild on a pending bitrate/resolution one.
                if pending_encoder_rebuild.is_none() {
                    if let Some(sample) = frame_rate_tracker.take_sample(now) {
                        if let Some(new_fps) = adaptive_fps.apply_at(&sample, now) {
                            let mut new_config = current_config.clone();
                            new_config.framerate = new_fps;
                            let backend = encoder_backend(&encoder);
                            println!(
                                "[adapt-fps] {} fps {} -> {} (delivered {:.0}, overrun {:.0}%, encode {:.1}ms)",
                                encoder_backend_name(backend),
                                current_config.framerate,
                                new_fps,
                                sample.delivered_fps,
                                sample.overrun_ratio * 100.0,
                                sample.avg_encode_ms,
                            );
                            // Slow capture immediately to stop overrunning; the
                            // encoder swaps to the new fps when the rebuild lands.
                            capture::set_target_fps(new_fps);
                            encoder_revision = encoder_revision.wrapping_add(1);
                            pending_encoder_rebuild = Some(spawn_encoder_rebuild(
                                new_config,
                                backend,
                                encoder_revision,
                                RebuildPurpose::FrameRate,
                            ));
                        }
                    }
                }
            }
        } else {
            // Release frame resources without encoding
            #[cfg(target_os = "macos")]
            unsafe {
                CVPixelBufferRelease(frame.pixel_buffer_ptr);
            }
        }
    }

    // Cleanup
    #[cfg(target_os = "macos")]
    encoder.flush();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    match &mut encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(e) => {
            e.flush();
        }
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(e) => {
            e.flush();
        }
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => {
            e.flush();
        }
        EncoderKind::Software(e) => {
            e.flush();
        }
    }
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    if let Ok(mut ap) = audio_pipeline.lock() {
        ap.stop();
    }
    capture_backend.stop();
    if input_pipeline_owner
        .compare_exchange(instance_id, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        input.clear_for_stop();
    }
    println!("Shared pipeline stopped");
}

// ---------------------------------------------------------------------------
// Encode + broadcast (replaces encode_and_send)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
/// Encode one captured frame, broadcast the resulting access unit(s), and
/// return the total encoded bytes produced (used by ABR to verify the encoder
/// actually tracked a requested bitrate change). Returns 0 on encode error.
fn encode_and_broadcast(
    encoder: &mut encode_vt::VTEncoder,
    broadcaster: &Broadcaster<EncodedVideoFrame>,
    input: &InputRuntime,
    frame: &capture::CapturedFrame,
    captured_micros: u64,
    video_epoch: u64,
) -> usize {
    input.update_cursor(frame.cursor.as_ref());

    if !input.control_active() {
        if let Some(cursor) = &frame.cursor {
            const BGRA_PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"BGRA");
            let pixel_buffer = std::mem::ManuallyDrop::new(unsafe {
                screencapturekit::cv::CVPixelBuffer::from_ptr(frame.pixel_buffer_ptr)
            });
            match pixel_buffer.lock_read_write() {
                Ok(mut guard) => {
                    if guard.pixel_format() == BGRA_PIXEL_FORMAT {
                        let stride = guard.bytes_per_row();
                        if let Some(data) = guard.as_slice_mut() {
                            capture::composite_cursor_with_stride(
                                data,
                                stride,
                                frame.width,
                                frame.height,
                                cursor,
                            );
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[capture] macOS cursor composite lock failed: {err}");
                }
            };
        }
    }

    if let Err(e) = encoder.encode_pixel_buffer(frame.pixel_buffer_ptr) {
        eprintln!("encode error: {e}");
    }
    unsafe {
        CVPixelBufferRelease(frame.pixel_buffer_ptr);
    }

    let mut encoded_bytes = 0usize;
    for nal in encoder.receive_nals() {
        encoded_bytes += nal.data.len();
        broadcaster.broadcast(EncodedVideoFrame {
            data: nal.data,
            capture_micros: captured_micros,
            source_seq: next_encoded_video_unit_seq(),
            is_recovery: nal.is_recovery,
            video_epoch,
        });
    }
    encoded_bytes
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encode_and_broadcast(
    encoder: &mut EncoderKind,
    broadcaster: &Broadcaster<EncodedVideoFrame>,
    input: &InputRuntime,
    frame: &capture::CapturedFrame,
    captured_micros: u64,
    video_epoch: u64,
) -> usize {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    input.update_cursor(frame.cursor.as_ref());

    // Composite cursor onto RAM frames before encoding when no controller owns input
    // AND the cursor is NOT already being sent separately to clients.
    //
    // When separate_cursor is true (PipeWire, KMS, X11), cursor data is sent via
    // CursorShape/CursorState over TCP and the client renders it as an overlay.
    // Compositing into the video frame would be redundant and — for DMA-BUF frames —
    // catastrophically expensive: it forces a full GPU→CPU readback that breaks the
    // zero-copy encode path.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let separate_cursor = input.capabilities().separate_cursor;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let frame_with_cursor;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let frame_ref = if !input.control_active() && !separate_cursor {
        if let Some(cursor) = &frame.cursor {
            match &frame.data {
                capture::FrameData::Ram(data) => {
                    let mut composited = data.clone();
                    capture::composite_cursor(&mut composited, frame.width, frame.height, cursor);
                    frame_with_cursor = capture::CapturedFrame {
                        data: capture::FrameData::Ram(composited),
                        width: frame.width,
                        height: frame.height,
                        #[cfg(any(target_os = "linux", target_os = "windows"))]
                        cursor: None,
                        force_keyframe: frame.force_keyframe,
                    };
                    &frame_with_cursor
                }
                #[cfg(target_os = "windows")]
                capture::FrameData::D3D11Texture { .. } => frame,
                #[cfg(target_os = "linux")]
                capture::FrameData::DmaBuf { .. } => {
                    match capture::try_clone_frame_to_ram_bgra(frame) {
                        Ok(Some(mut composited)) => {
                            capture::composite_cursor(
                                &mut composited,
                                frame.width,
                                frame.height,
                                cursor,
                            );
                            frame_with_cursor = capture::CapturedFrame {
                                data: capture::FrameData::Ram(composited),
                                width: frame.width,
                                height: frame.height,
                                #[cfg(any(target_os = "linux", target_os = "windows"))]
                                cursor: None,
                                force_keyframe: frame.force_keyframe,
                            };
                            &frame_with_cursor
                        }
                        Ok(None) => frame,
                        Err(err) => {
                            eprintln!("[capture] DMA-BUF cursor readback failed: {err}");
                            frame
                        }
                    }
                }
            }
        } else {
            frame
        }
    } else {
        frame
    };

    let result = match encoder {
        #[cfg(target_os = "linux")]
        EncoderKind::Vaapi(e) => e.encode(frame_ref),
        #[cfg(target_os = "linux")]
        EncoderKind::Nvenc(e) => e.encode(frame_ref),
        #[cfg(target_os = "windows")]
        EncoderKind::Hardware(e) => e.encode(frame_ref),
        EncoderKind::Software(e) => e.encode(frame_ref),
    };
    match result {
        Ok(nals) => {
            if trace_enabled() {
                let log_index = TRACE_ENCODE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if log_index < 12 {
                    let total_bytes: usize = nals.iter().map(|nal| nal.data.len()).sum();
                    eprintln!(
                        "[trace][server] encoder produced {} unit(s), total={} bytes, capture_ts={captured_micros}",
                        nals.len(),
                        total_bytes
                    );
                }
            }
            let mut encoded_bytes = 0usize;
            for nal in nals {
                encoded_bytes += nal.data.len();
                broadcaster.broadcast(EncodedVideoFrame {
                    data: nal.data,
                    capture_micros: captured_micros,
                    source_seq: next_encoded_video_unit_seq(),
                    is_recovery: nal.is_recovery,
                    video_epoch,
                });
            }
            encoded_bytes
        }
        Err(e) => {
            eprintln!("encode error: {e}");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Per-client transport
// ---------------------------------------------------------------------------

/// B6 (interleave step): audio is interleaved ahead of video bursts on the
/// shared socket by default so a 5 ms Opus packet doesn't wait behind a 60-packet
/// 4K IDR. `ST_AUDIO_INTERLEAVE=0` reverts to draining audio only after the
/// video burst. Pure send-ordering change — no wire/format change, so it
/// auto-enables per the CLAUDE.md rule.
fn audio_interleave_enabled() -> bool {
    !matches!(
        std::env::var("ST_AUDIO_INTERLEAVE").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Drain queued Opus packets and send them immediately. Always drains the queue
/// (so it can't build up) but only transmits when the client enabled audio.
fn flush_pending_audio(
    sender: &mut UdpSender,
    aud_rx: &Option<Receiver<Arc<EncodedAudioPacket>>>,
    audio_enabled: &AtomicBool,
    audio_depth: &AtomicU8,
    peer: impl std::fmt::Display,
) {
    let Some(aud) = aud_rx else { return };
    let send = audio_enabled.load(Ordering::Relaxed);
    while let Ok(opus) = aud.try_recv() {
        if send {
            if let Err(e) = sender.send_audio(&opus, audio_depth) {
                eprintln!("[transport] audio send error to {peer}: {e}");
            }
        }
    }
}

/// Per-client unified transport: sends both video and audio on a single UDP socket.
#[allow(clippy::too_many_arguments)]
/// Hard cap→send latency ceiling (µs). When a queued video unit has been waiting
/// longer than this on the server, the path is bufferbloated faster than the
/// bitrate controller can drain it; we stop replaying the stale backlog and jump
/// to a fresh keyframe instead (bounds worst-case latency to one recovery on a
/// WiFi stall). `ST_MAX_QUEUE_LATENCY_MS=0`/`off` disables; default 500 ms is the
/// safety net, the bitrate controller's BACKLOG_REDUCE downshift is the primary.
fn max_queue_latency_us() -> Option<u32> {
    match std::env::var("ST_MAX_QUEUE_LATENCY_MS").ok().as_deref() {
        Some("0") | Some("off") | Some("false") | Some("no") => None,
        Some(v) => v
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|ms| *ms > 0)
            .map(|ms| ms.saturating_mul(1000))
            .or(Some(500_000)),
        None => Some(500_000),
    }
}

/// Fold one sent unit's cap→send dwell into the published backlog EWMA and report
/// whether its *instantaneous* dwell breached the hard latency ceiling (caller
/// then drains to a recovery keyframe). Shared by both transport loops so the
/// direct and punched paths stay identical.
fn observe_send_backlog(
    capture_micros: u64,
    now_us: u64,
    ewma_us: &mut u32,
    seen: &mut bool,
    published: &AtomicU32,
    hard_ceiling_us: Option<u32>,
) -> bool {
    let dwell = now_us.saturating_sub(capture_micros).min(u32::MAX as u64) as u32;
    *ewma_us = if *seen {
        ((*ewma_us as u64 * 7 + dwell as u64) / 8) as u32
    } else {
        *seen = true;
        dwell
    };
    published.store(*ewma_us, Ordering::Relaxed);
    hard_ceiling_us.is_some_and(|c| dwell >= c)
}

// Per-client transport setup: one Arc per independent adaptive-control signal
// (FEC, audio redundancy, dup-FrameStart, send backlog) plus the transport
// plumbing. Grouping them into a struct would only move the argument list.
#[allow(clippy::too_many_arguments)]
fn run_transport(
    addr: SocketAddr,
    vid_rx: Receiver<Arc<EncodedVideoFrame>>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    aud_rx: Option<Receiver<Arc<EncodedAudioPacket>>>,
    audio_enabled: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    crypto: Option<Arc<st_protocol::tunnel::CryptoContext>>,
    // A2: adaptive RS parity percentage, updated by the control loop from loss.
    fec_pct: Arc<AtomicU16>,
    // Optional adaptive verbatim audio-redundancy depth.
    audio_depth: Arc<AtomicU8>,
    // Auto-mode duplicate-FrameStart verdict from the DupFirstController.
    dup_first: Arc<AtomicBool>,
    // Server-side cap→send backlog (µs, EWMA) published for the bitrate controller
    // so it can react to WiFi bufferbloat that never shows up as packet loss.
    send_backlog_us: Arc<AtomicU32>,
    // B3: shared cell holding the client's current UDP media destination. The
    // input listener updates it when the authenticated client's source port
    // changes; this loop repoints the send socket to match.
    media_dest: Arc<Mutex<SocketAddr>>,
) {
    let mut sender = match UdpSender::new(addr, crypto) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[transport] Failed to create UDP sender for {addr}: {e}");
            return;
        }
    };
    let mut current_dest = addr;
    // B1: send a liveness keepalive at least this often so the client can tell a
    // dead UDP path from an idle one (static screen → no captured frames → no
    // video sent). Reset whenever video is sent so active streams add nothing.
    const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
    let mut last_keepalive = Instant::now();
    let trace = trace_enabled();
    let interleave_audio = audio_interleave_enabled();
    let mut sent_video_units = 0usize;
    let mut last_video_activity = std::time::Instant::now();
    let mut last_backlog_keyframe_request = Instant::now() - Duration::from_secs(1);
    let mut waiting_for_recovery_frame = false;
    let mut last_source_seq = None::<u64>;

    // Preserve the very first queued frame so a new subscriber starts from the
    // requested IDR. After startup, collapse backlog to the newest queued
    // frame so slow clients stay live instead of replaying stale video.

    let mut last_fec_pct = u16::MAX;
    let mut last_audio_depth = u8::MAX;
    let mut last_dup_first: Option<bool> = None;
    let mut backlog_ewma_us: u32 = 0;
    let mut backlog_seen = false;
    let hard_ceiling_us = max_queue_latency_us();
    while running.load(Ordering::SeqCst) {
        // B3: relearn the client's UDP return port if it moved (cheap once-per-
        // iteration check). IP changes require a new authenticated connection.
        let want_dest = *media_dest.lock().unwrap();
        if want_dest != current_dest {
            match sender.update_dest(want_dest) {
                Ok(()) => {
                    println!("[transport] media repointed {current_dest} -> {want_dest}");
                    current_dest = want_dest;
                    // The path changed underneath us — force a keyframe so the
                    // client can resync without waiting for the next IDR.
                    video_bc.request_keyframe();
                }
                Err(e) => eprintln!("[transport] repoint to {want_dest} failed: {e}"),
            }
        }
        // B1: keepalive so the client's media-stall watchdog doesn't mistake an
        // idle (static-screen) path for a dead one. Reset below on each video
        // send, so an active stream never emits keepalives.
        if last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            let _ = sender.send_keepalive();
            last_keepalive = Instant::now();
        }
        // A2: pick up the latest adaptive FEC strength before sending.
        let pct = fec_pct.load(Ordering::Relaxed);
        if pct != last_fec_pct {
            sender.set_fec_pct(pct);
            last_fec_pct = pct;
        }
        // E5: pick up the latest adaptive audio-redundancy depth.
        let depth = audio_depth.load(Ordering::Relaxed);
        if depth != last_audio_depth {
            sender.set_audio_redundancy_depth(depth as usize);
            last_audio_depth = depth;
        }
        // Duplicate-FrameStart: send only while the controller says it helps.
        let dup_on = dup_first.load(Ordering::Relaxed);
        if last_dup_first != Some(dup_on) {
            sender.set_dup_first(dup_on);
            last_dup_first = Some(dup_on);
        }
        // Video: blocking recv with short timeout
        match vid_rx.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(frame) => {
                // Send every encoded unit in FIFO order. Drain transient backlog by
                // SENDING it, never by collapsing to the newest unit: encoded units
                // are inter-frame (P-frame) deltas, so dropping an intermediate unit
                // breaks the decoder's reference chain and forces perpetual keyframe
                // recovery — the "video refreshes every couple seconds" slideshow.
                // Genuine overload is absorbed upstream by the broadcaster evicting
                // the oldest queued unit, which surfaces here as a real source_seq
                // gap and routes through the recovery-keyframe path below.
                let mut pending = Some(frame);
                let mut burst = 0usize;
                while let Some(frame) = pending.take() {
                    // B6: push queued audio ahead of this video unit.
                    if interleave_audio {
                        flush_pending_audio(
                            &mut sender,
                            &aud_rx,
                            &audio_enabled,
                            &audio_depth,
                            addr,
                        );
                    }
                    let source_gap = last_source_seq
                        .map(|last| frame.source_seq.saturating_sub(last.saturating_add(1)))
                        .unwrap_or(0);
                    last_source_seq = Some(frame.source_seq);

                    // Server-side cap→send latency for this unit; publish the EWMA
                    // for the bitrate controller and trip a recovery drain if a
                    // single unit has bloated past the hard ceiling (WiFi stall).
                    let frame_now_us = unix_time_micros();
                    if observe_send_backlog(
                        frame.capture_micros,
                        frame_now_us,
                        &mut backlog_ewma_us,
                        &mut backlog_seen,
                        &send_backlog_us,
                        hard_ceiling_us,
                    ) && !frame.is_recovery
                        && !waiting_for_recovery_frame
                    {
                        waiting_for_recovery_frame = true;
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] cap→send backlog {}µs over ceiling for {addr}; draining to recovery keyframe",
                                frame_now_us.saturating_sub(frame.capture_micros)
                            );
                        }
                    }

                    if source_gap > 0 {
                        waiting_for_recovery_frame = true;
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] detected {source_gap} dropped video unit(s) for {addr} (broadcaster eviction); requesting recovery keyframe"
                            );
                        }
                    }

                    if waiting_for_recovery_frame && !frame.is_recovery {
                        // Discard P-frames until a fresh IDR arrives; keep nudging
                        // the encoder for one (throttled).
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] holding non-recovery video unit source_seq={} for {addr} while waiting for recovery",
                                frame.source_seq
                            );
                        }
                    } else {
                        if waiting_for_recovery_frame && frame.is_recovery {
                            waiting_for_recovery_frame = false;
                            if trace {
                                eprintln!(
                                    "[trace][server] resumed video for {addr} on recovery unit source_seq={}",
                                    frame.source_seq
                                );
                            }
                        }
                        if trace && sent_video_units < 12 {
                            eprintln!(
                                "[trace][server] transport send video unit #{sent_video_units} to {addr}: bytes={} capture_ts={}",
                                frame.data.len(),
                                frame.capture_micros
                            );
                        }
                        sent_video_units = sent_video_units.saturating_add(1);
                        last_video_activity = std::time::Instant::now();
                        // B1: real video is liveness — defer the next keepalive.
                        last_keepalive = std::time::Instant::now();
                        if let Err(e) = sender.send_frame(&frame, frame_now_us) {
                            eprintln!("[transport] video send error to {addr}: {e}");
                        }
                    }

                    burst += 1;
                    if burst >= MAX_VIDEO_SEND_BURST {
                        break;
                    }
                    pending = vid_rx.try_recv().ok();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if trace
                    && sent_video_units > 0
                    && last_video_activity.elapsed() >= std::time::Duration::from_millis(500)
                {
                    eprintln!(
                        "[trace][server] transport idle for {:?} waiting on encoded video for {addr}",
                        last_video_activity.elapsed()
                    );
                    last_video_activity = std::time::Instant::now();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Audio: drain queue (prevents buildup), only send if client enabled audio
        if let Some(ref aud) = aud_rx {
            let send_audio = audio_enabled.load(Ordering::Relaxed);
            while let Ok(opus) = aud.try_recv() {
                if send_audio {
                    if let Err(e) = sender.send_audio(&opus, &audio_depth) {
                        eprintln!("[transport] audio send error to {addr}: {e}");
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct ClientStartupPrefs {
    display: Option<ClientDisplayInfo>,
    pending_control: Vec<u8>,
}

fn client_display_fps_hint(display: Option<ClientDisplayInfo>) -> Option<u32> {
    let millihz = display?.max_refresh_millihz;
    if millihz == 0 {
        return None;
    }

    let fps = ((millihz + 500) / 1000).clamp(1, 360);
    if fps < 20 {
        None
    } else {
        Some(fps)
    }
}

fn wait_for_previous_pipeline_stop(state: &ServerState) {
    if let Some(handle) = state.pending_pipeline_stop.lock().unwrap().take() {
        println!("[pipeline] Waiting for previous pipeline to finish stopping...");
        let _ = handle.join();
        println!("[pipeline] Previous pipeline stopped.");
    }
}

fn request_video_profile(
    capture_state: &SharedCaptureState,
    capabilities: AggregateVideoCapabilities,
    registry_revision: u64,
    revision_source: Arc<AtomicU64>,
    commit_lock: Arc<Mutex<()>>,
    cancelled: Arc<AtomicBool>,
    admission: Option<AdmissionStamp>,
) -> Result<StreamSnapshot, String> {
    let (response_tx, response_rx) = bounded(1);
    capture_state
        .cmd_tx
        .send_timeout(
            CaptureCommand::SetVideoCapabilities(ProfileRequest {
                capabilities,
                registry_revision,
                revision_source,
                commit_lock,
                cancelled: Arc::clone(&cancelled),
                admission,
                response: response_tx,
            }),
            Duration::from_secs(1),
        )
        .map_err(|_| "shared pipeline is not accepting profile changes".to_string())?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("video profile request was cancelled".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cancelled.store(true, Ordering::Release);
            return Err("timed out opening the shared video profile".into());
        }
        match response_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(result) => return result,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err("shared video profile worker disconnected".into());
            }
        }
    }
}

fn setup_client_pipeline(
    state: &Arc<ServerState>,
    membership_id: u64,
    cancelled: Arc<AtomicBool>,
    admission: &AdmissionPermit,
) -> Result<PipelineSetup, String> {
    loop {
        if cancelled.load(Ordering::Acquire) || !admission.is_current() {
            return Err("joining video capability membership was cancelled".into());
        }
        state.pipeline_stop.wait(&cancelled)?;
        let (capabilities, registry_revision) = {
            let registry = state.video_capabilities.lock().unwrap();
            if !registry.is_tentative(membership_id) {
                return Err("joining video capability membership was cancelled".into());
            }
            (registry.aggregate()?, registry.revision())
        };

        let mut dead_pipeline = None;
        let reservation = {
            let _lifecycle_guard = state.pipeline_lifecycle.lock().unwrap();
            let mut pipeline = state.pipeline.lock().unwrap();
            if pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.pipeline_handle.is_finished())
            {
                println!("[pipeline] Pipeline thread died, will restart...");
                dead_pipeline = pipeline.take();
                None
            } else {
                pipeline
                    .as_ref()
                    .map(|pipeline| ClientPipelineReservation::new(pipeline, state))
                    .transpose()?
            }
        };
        if let Some(dead) = dead_pipeline {
            dead.stop();
            continue;
        }

        if let Some(reservation) = reservation {
            let snapshot = match request_video_profile(
                &reservation.capture_state,
                capabilities,
                registry_revision,
                Arc::clone(&state.profile_revision),
                Arc::clone(&state.profile_commit),
                Arc::clone(&cancelled),
                Some(AdmissionStamp {
                    token: admission.token,
                    coordinator: Arc::clone(&admission.coordinator),
                }),
            ) {
                Ok(snapshot) => snapshot,
                Err(_error)
                    if !cancelled.load(Ordering::Acquire)
                        && state.profile_revision.load(Ordering::Acquire) != registry_revision =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };

            let _commit_guard = state.profile_commit.lock().unwrap();
            let mut registry = state.video_capabilities.lock().unwrap();
            let _lifecycle_guard = state.pipeline_lifecycle.lock().unwrap();
            let pipeline = state.pipeline.lock().unwrap();
            let pipeline_is_current = pipeline.as_ref().is_some_and(|pipeline| {
                pipeline.instance_id == reservation.pipeline_instance_id
                    && !pipeline.pipeline_handle.is_finished()
            });
            if cancelled.load(Ordering::Acquire)
                || !admission.is_current()
                || registry.revision() != registry_revision
                || !registry.is_tentative(membership_id)
                || !pipeline_is_current
            {
                if cancelled.load(Ordering::Acquire) {
                    return Err("joining video capability membership was cancelled".into());
                }
                continue;
            }
            if !registry.activate_if_revision(membership_id, registry_revision) {
                continue;
            }
            state
                .profile_revision
                .store(registry.revision(), Ordering::Release);
            return Ok(reservation.commit(snapshot));
        }

        if state
            .pipeline_starting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let _start_guard = PipelineStartGuard(&state.pipeline_starting);
        println!("[pipeline] Starting shared pipeline...");
        let (started, subscription) = match SharedPipeline::start(
            capabilities,
            Arc::clone(&state.input),
            Arc::clone(&state.control),
            state,
            Arc::clone(&cancelled),
        ) {
            Ok(started) => started,
            Err(_error)
                if !cancelled.load(Ordering::Acquire)
                    && state.profile_revision.load(Ordering::Acquire) != registry_revision =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let snapshot = started
            .capture_state
            .snapshot()
            .ok_or_else(|| "pipeline started without a stream config".to_string())?;
        let mut started = Some(started);
        let mut subscription = Some(subscription);

        let committed = {
            let _commit_guard = state.profile_commit.lock().unwrap();
            let mut registry = state.video_capabilities.lock().unwrap();
            let _lifecycle_guard = state.pipeline_lifecycle.lock().unwrap();
            let mut pipeline = state.pipeline.lock().unwrap();
            if !cancelled.load(Ordering::Acquire)
                && admission.is_current()
                && registry.revision() == registry_revision
                && registry.is_tentative(membership_id)
                && pipeline.is_none()
                && registry.activate_if_revision(membership_id, registry_revision)
            {
                let pipeline_to_commit = started.as_ref().expect("started pipeline missing");
                state
                    .profile_revision
                    .store(registry.revision(), Ordering::Release);
                let setup = (
                    subscription.take().expect("started subscription missing"),
                    snapshot,
                    Arc::clone(&pipeline_to_commit.rate_control),
                    pipeline_to_commit.session_debug.clone(),
                    Arc::clone(&pipeline_to_commit.capture_state),
                );
                *pipeline = started.take();
                Some(setup)
            } else {
                None
            }
        };
        if let Some(setup) = committed {
            return Ok(setup);
        }
        started.expect("uncommitted pipeline missing").stop();
        if cancelled.load(Ordering::Acquire) {
            return Err("joining video capability membership was cancelled".into());
        }
    }
}

fn admit_client_pipeline(
    state: &Arc<ServerState>,
    capabilities: ClientVideoCapabilities,
    cancelled: Arc<AtomicBool>,
) -> Result<(PipelineSetup, VideoMembership), String> {
    let admission = state.admission.acquire(Arc::clone(&cancelled))?;
    let membership = begin_video_membership(state, capabilities);
    let setup = setup_client_pipeline(
        state,
        membership.id,
        Arc::clone(&cancelled),
        &admission,
    )?;
    Ok((setup, membership))
}

fn reconcile_video_profile(state: &Arc<ServerState>) -> Result<(), String> {
    loop {
        let (capabilities, registry_revision) = {
            let registry = state.video_capabilities.lock().unwrap();
            if registry.is_empty() {
                return Ok(());
            }
            (registry.aggregate()?, registry.revision())
        };
        let capture_state = {
            let _lifecycle_guard = state.pipeline_lifecycle.lock().unwrap();
            let pipeline = state.pipeline.lock().unwrap();
            let Some(active) = pipeline.as_ref() else {
                return Ok(());
            };
            Arc::clone(&active.capture_state)
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        match request_video_profile(
            &capture_state,
            capabilities,
            registry_revision,
            Arc::clone(&state.profile_revision),
            Arc::clone(&state.profile_commit),
            cancelled,
            None,
        ) {
            Ok(_) if state.profile_revision.load(Ordering::Acquire) == registry_revision => {
                return Ok(())
            }
            Ok(_) => continue,
            Err(_error) if state.profile_revision.load(Ordering::Acquire) != registry_revision => {
                continue
            }
            Err(error) => return Err(error),
        }
    }
}

/// Unsubscribe from broadcasters and stop the pipeline (in a background thread)
/// if no subscribers remain.
fn unsubscribe_and_maybe_stop_pipeline(
    state: &Arc<ServerState>,
    pipeline_instance_id: u64,
    vid_sub_id: u64,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))] aud_sub_id: u64,
) {
    let _lifecycle_guard = state.pipeline_lifecycle.lock().unwrap();
    let mut pipeline = state.pipeline.lock().unwrap();
    let should_stop = if let Some(p) = pipeline
        .as_ref()
        .filter(|pipeline| pipeline.instance_id == pipeline_instance_id)
    {
        p.video_bc.unsubscribe(vid_sub_id);
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        p.audio_bc.unsubscribe(aud_sub_id);
        p.video_bc.occupied_count() == 0
    } else {
        false
    };
    if should_stop {
        let pipeline = pipeline.take().expect("pipeline checked above");
        println!("[pipeline] No viewers left, stopping shared pipeline...");
        let stop_handle = std::thread::spawn(move || {
            pipeline.stop();
        });
        *state.pending_pipeline_stop.lock().unwrap() = Some(stop_handle);
    }
}

fn client_media_port(display: Option<ClientDisplayInfo>) -> u16 {
    display
        .map(|info| info.udp_port)
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_APP_PORT)
}

fn configured_listen_port() -> u16 {
    match std::env::var("ST_PORT") {
        Ok(value) => match value.parse::<u16>() {
            Ok(port) if port != 0 => port,
            _ => {
                eprintln!(
                    "[config] Invalid ST_PORT='{}', falling back to {}",
                    value, DEFAULT_APP_PORT
                );
                DEFAULT_APP_PORT
            }
        },
        Err(_) => DEFAULT_APP_PORT,
    }
}

/// Read the Authenticate message from the client and verify the token.
/// Returns `true` if authentication succeeds; sends AuthResult either way.
async fn authenticate_client(
    stream: &mut tokio::net::TcpStream,
    expected_token: &str,
) -> Result<bool, std::io::Error> {
    let mut buf = [0u8; 512];
    let mut pending = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        // Try to parse what we have
        let mut consumed = 0usize;
        while let Some((msg, used)) = ControlMessage::deserialize(&pending[consumed..]) {
            consumed += used;
            if let ControlMessage::Authenticate(token) = msg {
                if consumed > 0 {
                    pending.drain(..consumed);
                }
                let ok = constant_time_eq(token.as_bytes(), expected_token.as_bytes());
                let _ = stream
                    .write_all(&ControlMessage::AuthResult(ok).serialize())
                    .await;
                return Ok(ok);
            }
        }
        if consumed > 0 {
            pending.drain(..consumed);
        }

        let read_result = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await;
        match read_result {
            Ok(Ok(0)) => {
                // Connection closed before auth
                return Ok(false);
            }
            Ok(Ok(n)) => pending.extend_from_slice(&buf[..n]),
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                // Timeout — no auth message received
                return Ok(false);
            }
        }
    }
}

async fn read_client_startup_prefs(
    stream: &mut tokio::net::TcpStream,
) -> Result<ClientStartupPrefs, std::io::Error> {
    let mut prefs = ClientStartupPrefs::default();
    let mut buf = [0u8; 256];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(150);

    loop {
        let read_result = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await;
        match read_result {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                prefs.pending_control.extend_from_slice(&buf[..n]);
                let mut consumed = 0usize;
                while let Some((msg, used)) =
                    ControlMessage::deserialize(&prefs.pending_control[consumed..])
                {
                    consumed += used;
                    if let ControlMessage::ClientDisplayInfo(info) = msg {
                        prefs.display = Some(info);
                    }
                }
                if consumed > 0 {
                    prefs.pending_control.drain(..consumed);
                }
                if prefs.display.is_some() {
                    break;
                }
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => break,
        }
    }

    Ok(prefs)
}

async fn wait_for_client_media_ready(
    stream: &mut tokio::net::TcpStream,
    control_buf: &mut Vec<u8>,
) -> Result<bool, std::io::Error> {
    let mut buf = [0u8; 128];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    loop {
        let mut consumed = 0usize;
        while let Some((msg, used)) = ControlMessage::deserialize(&control_buf[consumed..]) {
            consumed += used;
            if matches!(msg, ControlMessage::ClientReadyForMedia) {
                if consumed > 0 {
                    control_buf.drain(..consumed);
                }
                return Ok(true);
            }
        }
        if consumed > 0 {
            control_buf.drain(..consumed);
        }

        let read_result = tokio::time::timeout_at(deadline, stream.read(&mut buf)).await;
        match read_result {
            Ok(Ok(0)) => return Ok(false),
            Ok(Ok(n)) => control_buf.extend_from_slice(&buf[..n]),
            Ok(Err(err)) => return Err(err),
            Err(_) => return Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

enum TunnelDetect {
    Tunnel,
    Normal,
}

/// Concurrency cap on tunnel sessions (direct-preamble + relay), which run on
/// dedicated OS threads rather than cheap tokio tasks. Bounds the cost of
/// unauthenticated peers that open a tunnel connection and then stall through
/// the auth window.
static ACTIVE_TUNNEL_SESSIONS: AtomicUsize = AtomicUsize::new(0);
const MAX_TUNNEL_SESSIONS: usize = 64;

/// RAII slot for a tunnel session. `acquire()` returns `None` when the cap is
/// already reached, so the caller drops the connection instead of spawning.
struct TunnelSessionSlot;

impl TunnelSessionSlot {
    fn acquire() -> Option<Self> {
        let prev = ACTIVE_TUNNEL_SESSIONS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_TUNNEL_SESSIONS).then_some(n + 1)
        });
        prev.ok().map(|_| TunnelSessionSlot)
    }
}

impl Drop for TunnelSessionSlot {
    fn drop(&mut self) {
        ACTIVE_TUNNEL_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Peek (without consuming) the first bytes of a fresh control connection to
/// see whether the client requested TCP tunnel framing — the fallback used
/// when its UDP path is blocked. Normal clients send a `ControlMessage`
/// first, whose leading byte can never match the preamble, so the very first
/// byte disambiguates instantly and never adds latency for them; only a
/// matching-prefix connection waits for the rest of the preamble. The deadline
/// is aligned with the auth budget (not a tight 750 ms) so a tunnel client on
/// a high-RTT path — exactly where the TCP fallback is needed — is not
/// misclassified as Normal when its preamble arrives one slow RTT late.
async fn detect_tunnel_preamble(stream: &mut tokio::net::TcpStream) -> TunnelDetect {
    use st_protocol::tcp_tunnel::TCP_TUNNEL_PREAMBLE;
    let want = TCP_TUNNEL_PREAMBLE.len();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 8];
    loop {
        match tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await {
            Ok(Ok(0)) => return TunnelDetect::Normal,
            Ok(Ok(n)) => {
                let check = n.min(want);
                if buf[..check] != TCP_TUNNEL_PREAMBLE[..check] {
                    return TunnelDetect::Normal;
                }
                if n >= want {
                    return TunnelDetect::Tunnel;
                }
                // Matching prefix but incomplete — peek returns the same bytes
                // immediately, so yield briefly while the rest arrives.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(Err(_)) | Err(_) => return TunnelDetect::Normal,
        }
    }
}

async fn handle_client(
    mut stream: tokio::net::TcpStream,
    addr: SocketAddr,
    state: Arc<ServerState>,
) {
    println!("Client connected: {addr}");
    let _ = stream.set_nodelay(true);

    // TCP media fallback: clients whose UDP path is blocked open the same
    // control port but lead with a tunnel preamble; the whole session
    // (control + media) then runs over this one TCP connection using the
    // tunnel framing, through the same handler as hole-punched sessions.
    if matches!(
        detect_tunnel_preamble(&mut stream).await,
        TunnelDetect::Tunnel
    ) {
        // Bound concurrent tunnel sessions before committing OS threads, so a
        // flood of preamble-then-silence connections can't exhaust threads/FDs.
        let Some(slot) = TunnelSessionSlot::acquire() else {
            eprintln!("[tcp-tunnel] Rejecting {addr}: tunnel session cap reached");
            return;
        };
        let mut preamble = [0u8; st_protocol::tcp_tunnel::TCP_TUNNEL_PREAMBLE.len()];
        if stream.read_exact(&mut preamble).await.is_err() {
            return;
        }
        println!("[tcp-tunnel] Client {addr} requested TCP tunnel mode");
        let std_stream = match stream.into_std() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[tcp-tunnel] into_std failed for {addr}: {e}");
                return;
            }
        };
        let _ = std_stream.set_nonblocking(false);
        let state2 = Arc::clone(&state);
        std::thread::spawn(move || {
            let _slot = slot; // released when the session ends
            match st_protocol::tcp_tunnel::TcpTunnel::new(std_stream, None, Vec::new()) {
                Ok(tunnel) => handle_punched_client(Arc::new(tunnel), state2),
                Err(e) => eprintln!("[tcp-tunnel] setup failed for {addr}: {e}"),
            }
        });
        return;
    }

    // Authenticate before anything else
    match authenticate_client(&mut stream, &state.control.token()).await {
        Ok(true) => println!("[auth] Client {addr} authenticated"),
        Ok(false) => {
            eprintln!("[auth] Client {addr} failed authentication");
            let _ = stream
                .write_all(&ControlMessage::Error("Authentication failed.".into()).serialize())
                .await;
            return;
        }
        Err(err) => {
            eprintln!("[auth] Error reading auth from {addr}: {err}");
            return;
        }
    }

    let registered_client = state.control.register_client(addr);
    let client_id = state.input.allocate_client_id();

    let startup_prefs = match read_client_startup_prefs(&mut stream).await {
        Ok(prefs) => prefs,
        Err(err) => {
            eprintln!("Failed to read startup preferences from {addr}: {err}");
            return;
        }
    };
    if registered_client.disconnect_requested() {
        return;
    }
    let video_capabilities = client_video_capabilities(startup_prefs.display);
    let client_requested_fps = video_capabilities.requested_fps;
    if let Some(display) = startup_prefs.display {
        println!(
            "[client {addr}] display refresh hint: {:.3} Hz, media udp port: {}",
            display.max_refresh_millihz as f32 / 1000.0,
            client_media_port(Some(display))
        );
        println!(
            "[client {addr}] video decode support: supported={} hardware={} yuv444={} yuv444-hw={}",
            codec_support_summary(display.supported_video_codecs),
            codec_support_summary(display.hardware_video_codecs),
            codec_support_summary(display.supported_yuv444_video_codecs),
            codec_support_summary(display.hardware_yuv444_video_codecs),
        );
    }

    // Tentative membership participates in the all-client profile before this
    // handler can subscribe to media. The RAII guard removes it on every direct
    // cancellation/error path and triggers an upgrade after active disconnect.
    let video_membership = begin_video_membership(&state, video_capabilities);
    let state2 = Arc::clone(&state);
    let membership_id = video_membership.id;
    let membership_cancelled = Arc::clone(&video_membership.cancelled);
    let setup_worker_cancelled = Arc::clone(&membership_cancelled);
    let mut setup_task = tokio::task::spawn_blocking(move || -> Result<PipelineSetup, String> {
        // Wake the display on every client connect, before any first-frame wait.
        // On Wayland (PipeWire / wlroots) and KMS the compositor/kernel stops
        // driving frames when the monitor is in DPMS off, which would otherwise
        // stall the first-frame recv until the 30s pipeline timeout. Fires on
        // every connect (debounced in ServerControl) so a second client reaching
        // a re-blanked display also wakes it, not only the first. Disable with
        // ST_WAKE_ON_CONNECT=0.
        trigger_screen_wake(&state2.control);
        setup_client_pipeline(&state2, membership_id, setup_worker_cancelled)
    });
    let setup = loop {
        tokio::select! {
            result = &mut setup_task => {
                break result.unwrap_or_else(|error| Err(format!("pipeline setup worker failed: {error}")));
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let mut probe = [0u8; 1];
                let peer_closed = matches!(
                    tokio::time::timeout(Duration::from_millis(1), stream.peek(&mut probe)).await,
                    Ok(Ok(0)) | Ok(Err(_))
                );
                if registered_client.disconnect_requested() || peer_closed {
                    membership_cancelled.store(true, Ordering::Release);
                    return;
                }
            }
        }
    };

    let (sub, startup_snapshot, rate_control, session_debug, capture_state) = match setup {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Pipeline error for {addr}: {e}");
            let msg = ControlMessage::Error(e).serialize();
            let _ = stream.write_all(&msg).await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            println!("Waiting for clients...");
            return;
        }
    };
    if registered_client.disconnect_requested() {
        rate_control.unregister_client(sub.vid_sub_id);
        let _ = state.input.release_control(client_id);
        unsubscribe_and_maybe_stop_pipeline(
            &state,
            sub.pipeline_instance_id,
            sub.vid_sub_id,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            sub.aud_sub_id,
        );
        return;
    }
    rate_control.register_client(sub.vid_sub_id);
    let mut bitrate_controller = ClientRateController::from_state(rate_control.as_ref());
    let controller_state = state.input.controller_state_for(client_id);
    let stream_config = startup_snapshot.config;
    if let Some(requested_fps) = client_requested_fps {
        if stream_config.framerate as u32 != requested_fps {
            println!(
                "[client {addr}] negotiated {} fps (requested {} fps)",
                stream_config.framerate, requested_fps
            );
        }
    }

    let transport_addr = SocketAddr::new(addr.ip(), client_media_port(startup_prefs.display));
    let input_credential = input::generate_input_credential();
    // Register before publishing InputSession: Android sends a neutral input
    // snapshot before ClientReadyForMedia to reveal its live UDP receive port.
    let media_dest = Arc::new(Mutex::new(transport_addr));
    let input_registration = state.input.register_direct_client(
        client_id,
        input_credential,
        addr.ip(),
        Arc::clone(&media_dest),
    );

    // Send stream/session metadata first. The client will bind UDP, start its
    // receive path, and acknowledge readiness before we start transport.
    let mut control_buf = ControlMessage::StreamConfig(stream_config).serialize();
    control_buf
        .extend_from_slice(&ControlMessage::SessionDebugInfo(session_debug.clone()).serialize());
    control_buf.extend_from_slice(
        &ControlMessage::InputSession(InputSession {
            client_id,
            credential: input_credential,
        })
        .serialize(),
    );
    control_buf.extend_from_slice(
        &ControlMessage::InputCapabilities(state.input.capabilities()).serialize(),
    );
    control_buf.extend_from_slice(&ControlMessage::ControllerState(controller_state).serialize());
    let available_outputs = capture_state.outputs();
    if available_outputs.len() > 1 {
        control_buf
            .extend_from_slice(&ControlMessage::AvailableOutputs(available_outputs).serialize());
        // Tell the client which output is currently captured so it can
        // highlight it (server→client `SelectOutput` means "current selection").
        control_buf
            .extend_from_slice(&ControlMessage::SelectOutput(capture_state.selected()).serialize());
    }
    if let Err(e) = stream.write_all(&control_buf).await {
        eprintln!("Failed to send stream config to {addr}: {e}");
    } else if trace_enabled() {
        eprintln!(
            "[trace][server] sent startup control bundle to {addr}: bytes={} media_port={}",
            control_buf.len(),
            client_media_port(startup_prefs.display)
        );
    }
    let mut control_buf = startup_prefs.pending_control;
    match wait_for_client_media_ready(&mut stream, &mut control_buf).await {
        Ok(true) => {
            if trace_enabled() {
                eprintln!("[trace][server] received ClientReadyForMedia from {addr}");
            }
        }
        Ok(false) => {
            eprintln!("[client {addr}] timed out waiting for media-ready ack");
            rate_control.unregister_client(sub.vid_sub_id);
            let _ = state.input.release_control(client_id);
            unsubscribe_and_maybe_stop_pipeline(
                &state,
                sub.pipeline_instance_id,
                sub.vid_sub_id,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                sub.aud_sub_id,
            );
            return;
        }
        Err(err) => {
            eprintln!("[client {addr}] failed waiting for media-ready ack: {err}");
            rate_control.unregister_client(sub.vid_sub_id);
            let _ = state.input.release_control(client_id);
            unsubscribe_and_maybe_stop_pipeline(
                &state,
                sub.pipeline_instance_id,
                sub.vid_sub_id,
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                sub.aud_sub_id,
            );
            return;
        }
    }
    if registered_client.disconnect_requested() {
        rate_control.unregister_client(sub.vid_sub_id);
        let _ = state.input.release_control(client_id);
        unsubscribe_and_maybe_stop_pipeline(
            &state,
            sub.pipeline_instance_id,
            sub.vid_sub_id,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            sub.aud_sub_id,
        );
        return;
    }

    // Per-client audio enable flag (toggled by client via SetAudio control message)
    let audio_enabled = Arc::new(AtomicBool::new(true));

    // Start per-client unified transport (video + audio on single UDP socket)
    let transport_running = Arc::new(AtomicBool::new(true));

    sub.video_bc.request_keyframe();
    let vid_rx = sub.vid_rx;
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let aud_rx = Some(sub.aud_rx);
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let aud_rx: Option<Receiver<Arc<EncodedAudioPacket>>> = None;
    let transport_running_clone = Arc::clone(&transport_running);
    let audio_enabled_transport = Arc::clone(&audio_enabled);
    let video_bc = Arc::clone(&sub.video_bc);
    // A2: adaptive FEC strength shared from the control loop to the sender.
    let mut fec_controller = adaptive_bitrate::FecController::from_env();
    let fec_pct_shared = Arc::new(AtomicU16::new(fec_controller.current_pct()));
    let fec_pct_transport = Arc::clone(&fec_pct_shared);
    // Duplicate-FrameStart utility controller (A/B probe): only keeps the
    // duplicate on while it measurably reduces frame loss.
    let mut dup_first_controller = adaptive_bitrate::DupFirstController::new();
    let dup_first_shared = Arc::new(AtomicBool::new(dup_first_controller.enabled()));
    let dup_first_transport = Arc::clone(&dup_first_shared);
    // Server-side cap→send backlog (µs, EWMA) published by the transport loop and
    // read by the bitrate controller to react to WiFi bufferbloat (zero loss).
    let send_backlog_shared = Arc::new(AtomicU32::new(0));
    let send_backlog_transport = Arc::clone(&send_backlog_shared);
    // Default to fixed verbatim redundancy from stream start because 5 ms CELT
    // packets have no usable Opus LBRR. Adaptation is an explicit opt-in.
    let audio_redundancy_adaptive = transport::audio_adaptive_redundancy_enabled();
    let audio_redundancy_max = transport::configured_audio_redundancy_depth() as u8;
    let mut audio_redundancy_controller =
        adaptive_bitrate::AudioRedundancyController::new(audio_redundancy_max);
    let audio_depth_init = if audio_redundancy_adaptive {
        audio_redundancy_controller.current_depth()
    } else {
        audio_redundancy_max
    };
    let audio_depth_shared = Arc::new(AtomicU8::new(audio_depth_init));
    let audio_depth_transport = Arc::clone(&audio_depth_shared);
    let media_dest_transport = Arc::clone(&media_dest);
    let transport_handle = std::thread::spawn(move || {
        run_transport(
            transport_addr,
            vid_rx,
            video_bc,
            aud_rx,
            audio_enabled_transport,
            transport_running_clone,
            None,
            fec_pct_transport,
            audio_depth_transport,
            dup_first_transport,
            send_backlog_transport,
            media_dest_transport,
        );
    });
    if let Err(err) = stream
        .write_all(&ControlMessage::StreamStarted.serialize())
        .await
    {
        eprintln!("Failed to send stream-started to {addr}: {err}");
    } else if trace_enabled() {
        eprintln!("[trace][server] sent StreamStarted to {addr}");
    }

    println!("[pipeline] Client {addr} subscribed (transport started)");
    let (clipboard_control_tx, clipboard_control_rx) = bounded::<ControlMessage>(8);
    let (file_detect_tx, file_detect_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(8);
    let suppressed_paths = clipboard::new_suppressed_paths();
    let mut clipboard_sync = clipboard::ClipboardSync::start_with_file_detection(
        "server",
        clipboard_control_tx,
        file_detect_tx,
        Arc::clone(&suppressed_paths),
    );
    let mut ft_manager = file_transfer::FileTransferManager::start_auto_accept(
        st_protocol::file_transfer::TransportMode::Direct,
        file_transfer::new_shared_state(),
        suppressed_paths,
    );

    // Hold TCP open — read control messages from client
    let mut buf = [0u8; 64];
    let mut cursor_versions = CursorVersionCursor::default();
    let mut last_transport_recovery_keyframe = Instant::now() - Duration::from_secs(1);
    let mut last_controller_state = controller_state;
    let mut last_config_generation = startup_snapshot.generation;
    let mut last_video_epoch = startup_snapshot.video_epoch;
    let mut last_selection_generation = startup_snapshot.selection_generation;
    // Direct-path liveness: the OS TCP keepalive default is ~2h, so a silent
    // network drop (wifi off, NAT rebind, peer crash without RST) would leave
    // this loop spinning on the 16ms read timeout forever, holding the
    // broadcaster subscription, transport thread and input-control ownership.
    // The client sends TransportFeedback (and clock-sync pings) over this TCP
    // channel continuously while connected, so treat prolonged inbound silence
    // as a dead client and tear down — mirroring the punched path's timeout.
    const DIRECT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(15);
    let mut last_inbound = Instant::now();
    loop {
        if registered_client.disconnect_requested() {
            break;
        }
        let controller_state = state.input.controller_state_for(client_id);
        if controller_state != last_controller_state {
            if stream
                .write_all(&ControlMessage::ControllerState(controller_state).serialize())
                .await
                .is_err()
            {
                break;
            }
            last_controller_state = controller_state;
        }
        // An output switch (any client) reconfigures the shared stream — push
        // the new StreamConfig so this client re-inits its decoder for the new
        // resolution. The rebuilt encoder already starts with a keyframe.
        if let Some(snapshot) = capture_state.snapshot().filter(|snapshot| {
            snapshot.generation != last_config_generation
                || snapshot.video_epoch != last_video_epoch
                || snapshot.selection_generation != last_selection_generation
        }) {
            last_config_generation = snapshot.generation;
            let mut buf = ControlMessage::StreamConfig(snapshot.config).serialize();
            buf.extend_from_slice(
                &ControlMessage::SelectOutput(capture_state.selected()).serialize(),
            );
            if stream.write_all(&buf).await.is_err() {
                break;
            }
            last_video_epoch = snapshot.video_epoch;
            last_selection_generation = snapshot.selection_generation;
        }
        let mut clipboard_write_failed = false;
        while let Ok(message) = clipboard_control_rx.try_recv() {
            if stream.write_all(&message.serialize()).await.is_err() {
                clipboard_write_failed = true;
                break;
            }
        }
        if clipboard_write_failed {
            break;
        }
        while let Ok(path) = file_detect_rx.try_recv() {
            let _ = ft_manager
                .inbound_tx
                .try_send(file_transfer::FtInbound::SendFile { path });
        }
        let mut ft_write_failed = false;
        while let Ok(message) = ft_manager.outbound_rx.try_recv() {
            if stream.write_all(&message.serialize()).await.is_err() {
                ft_write_failed = true;
                break;
            }
        }
        if ft_write_failed {
            break;
        }
        let mut cursor_write_failed = false;
        for message in state.input.cursor_messages(client_id, &mut cursor_versions) {
            if stream.write_all(&message.serialize()).await.is_err() {
                cursor_write_failed = true;
                break;
            }
        }
        if cursor_write_failed {
            break;
        }

        match tokio::time::timeout(std::time::Duration::from_millis(16), stream.read(&mut buf))
            .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                last_inbound = Instant::now();
                control_buf.extend_from_slice(&buf[..n]);
                let mut consumed = 0usize;
                while let Some((msg, used)) = ControlMessage::deserialize(&control_buf[consumed..])
                {
                    consumed += used;
                    match msg {
                        ControlMessage::SetAudio(enabled) => {
                            audio_enabled.store(enabled, Ordering::SeqCst);
                            println!(
                                "[client {addr}] audio: {}",
                                if enabled { "on" } else { "off" }
                            );
                        }
                        ControlMessage::TransportFeedback(feedback) => {
                            if (feedback.dropped_frames > 0
                                || (feedback.lost_packets > 0 && feedback.completed_frames == 0))
                                && last_transport_recovery_keyframe.elapsed()
                                    >= Duration::from_millis(250)
                            {
                                sub.video_bc.request_keyframe();
                                last_transport_recovery_keyframe = Instant::now();
                                if trace_enabled() {
                                    eprintln!(
                                        "[trace][server] requested recovery keyframe from transport feedback: lost_packets={} dropped_frames={} completed_frames={}",
                                        feedback.lost_packets,
                                        feedback.dropped_frames,
                                        feedback.completed_frames
                                    );
                                }
                            }
                            // A2: raise/decay RS FEC strength on the same signal.
                            let fec_pct = fec_controller.apply_feedback(&feedback);
                            fec_pct_shared.store(fec_pct, Ordering::Relaxed);
                            // Optional adaptation uses the same aggregate loss signal.
                            if audio_redundancy_adaptive {
                                audio_redundancy_controller.synchronize_sender_depth(
                                    audio_depth_shared.load(Ordering::Relaxed),
                                );
                                let depth = audio_redundancy_controller.apply_feedback(&feedback);
                                audio_depth_shared.store(depth, Ordering::Relaxed);
                            }
                            // Duplicate-FrameStart A/B: keep it only while it helps.
                            let dup_on = dup_first_controller.apply_feedback(&feedback);
                            dup_first_shared.store(dup_on, Ordering::Relaxed);
                            // Bufferbloat: feed the server-side cap→send backlog so
                            // ABR can downshift on WiFi queue growth (zero loss).
                            bitrate_controller
                                .note_send_backlog_us(send_backlog_shared.load(Ordering::Relaxed));
                            let next_kbps = bitrate_controller.apply_feedback(feedback);
                            rate_control.update_client_target(sub.vid_sub_id, next_kbps);
                        }
                        ControlMessage::ClientBitratePreference(max_kbps) => {
                            // B4: clamp this client's ABR ceiling to its declared max.
                            rate_control.set_client_ceiling(sub.vid_sub_id, max_kbps);
                            if trace_enabled() {
                                eprintln!(
                                    "[trace][server] client {addr} declared bitrate ceiling {max_kbps} kbps"
                                );
                            }
                        }
                        ControlMessage::ClockSyncPing(ping) => {
                            let server_recv_micros = unix_time_micros();
                            let pong = ControlMessage::ClockSyncPong(ClockSyncPong {
                                client_send_micros: ping.client_send_micros,
                                server_recv_micros,
                                server_send_micros: unix_time_micros(),
                                bitrate_kbps: rate_control.current_target_kbps(),
                            });
                            let _ = stream.write_all(&pong.serialize()).await;
                        }
                        ControlMessage::AcquireControl => {
                            let next_state = state.input.acquire_control(client_id);
                            let state_msg = ControlMessage::ControllerState(next_state);
                            cursor_versions = CursorVersionCursor::default();
                            last_controller_state = next_state;
                            let _ = stream.write_all(&state_msg.serialize()).await;
                        }
                        ControlMessage::ReleaseControl => {
                            let next_state = state.input.release_control(client_id);
                            let state_msg = ControlMessage::ControllerState(next_state);
                            last_controller_state = next_state;
                            let _ = stream.write_all(&state_msg.serialize()).await;
                        }
                        ControlMessage::RequestKeyframe => {
                            sub.video_bc.request_keyframe();
                        }
                        ControlMessage::SelectOutput(id) => {
                            println!("[client {addr}] requested capture output {id}");
                            capture_state.request_output(id);
                        }
                        ControlMessage::ClipboardText(text) => {
                            clipboard_sync.set_remote_text(text);
                        }
                        ControlMessage::TextInput(text) => {
                            let _ = state.input.handle_text_input(client_id, &text);
                        }
                        ControlMessage::FileOffer {
                            transfer_id,
                            file_size,
                            file_name,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::OfferReceived {
                                    transfer_id,
                                    file_size,
                                    file_name,
                                },
                            );
                        }
                        ControlMessage::FileAccept {
                            transfer_id,
                            accepted,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::AcceptReceived {
                                    transfer_id,
                                    accepted,
                                },
                            );
                        }
                        ControlMessage::FileChunk {
                            transfer_id,
                            chunk_index,
                            data,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ChunkReceived {
                                    transfer_id,
                                    chunk_index,
                                    data,
                                },
                            );
                        }
                        ControlMessage::FileComplete {
                            transfer_id,
                            total_chunks,
                            sha256,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::CompleteReceived {
                                    transfer_id,
                                    total_chunks,
                                    sha256,
                                },
                            );
                        }
                        ControlMessage::FileCancel { transfer_id } => {
                            let _ = ft_manager
                                .inbound_tx
                                .try_send(file_transfer::FtInbound::CancelReceived { transfer_id });
                        }
                        ControlMessage::FileProgress {
                            transfer_id,
                            chunks_received,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ProgressReceived {
                                    transfer_id,
                                    chunks_received,
                                },
                            );
                        }
                        ControlMessage::ClientDisplayInfo(_)
                        | ControlMessage::ClientReadyForMedia
                        | ControlMessage::InputSession(_)
                        | ControlMessage::ControllerState(_)
                        | ControlMessage::CursorShape(_)
                        | ControlMessage::CursorState(_) => {}
                        _ => {}
                    }
                }
                if consumed > 0 {
                    control_buf.drain(..consumed);
                }
            }
            // Read timed out (no inbound bytes this tick) — normal. Use it to
            // check for a dead client that stopped sending feedback entirely.
            Err(_) => {
                if last_inbound.elapsed() > DIRECT_INACTIVITY_TIMEOUT {
                    println!(
                        "[client {addr}] no control traffic for {}s — assuming disconnected",
                        DIRECT_INACTIVITY_TIMEOUT.as_secs()
                    );
                    break;
                }
            }
        }
    }

    println!("Client {addr} disconnected.");
    clipboard_sync.stop();
    ft_manager.stop();
    transport_running.store(false, Ordering::SeqCst);
    rate_control.unregister_client(sub.vid_sub_id);
    let _ = state.input.release_control(client_id);
    drop(input_registration);

    unsubscribe_and_maybe_stop_pipeline(
        &state,
        sub.pipeline_instance_id,
        sub.vid_sub_id,
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        sub.aud_sub_id,
    );

    let _ = transport_handle.join();

    println!("Waiting for clients...");
}

// ---------------------------------------------------------------------------
// Startup probe (Linux only)
// ---------------------------------------------------------------------------

/// Non-interactive probe at startup: detect available backends without starting capture.
#[cfg(target_os = "linux")]
fn probe_backends() {
    println!("--- Probing backends ---");

    let ds = if let Ok(st) = std::env::var("XDG_SESSION_TYPE") {
        st.to_lowercase()
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        "wayland".into()
    } else if std::env::var("DISPLAY").is_ok() {
        "x11".into()
    } else {
        "unknown".into()
    };
    println!("[probe] Display server: {ds}");

    let x11_ok = std::env::var("DISPLAY").is_ok();
    let wlr_ok = capture::linux::wl_capture::verify_wayland();
    let nvfbc_probe = if ds == "x11" {
        Some(capture::linux::probe_nvfbc())
    } else {
        None
    };
    let pipewire_ok = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.portal.Desktop",
            "--print-reply",
            "--type=method_call",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.DBus.Properties.Get",
            "string:org.freedesktop.portal.ScreenCast",
            "string:version",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let nvfbc_ok = matches!(
        &nvfbc_probe,
        Some(Ok(capture::linux::NvfbcProbe {
            is_capture_possible: true,
            ..
        }))
    );
    println!(
        "[probe] Capture backends: NvFBC={nvfbc_ok}, X11={x11_ok}, wlr-screencopy={wlr_ok}, PipeWire(portal)={pipewire_ok}"
    );
    match &nvfbc_probe {
        Some(Ok(status)) => {
            println!(
                "[probe] NvFBC: available (capture_possible={}, can_create_now={})",
                status.is_capture_possible, status.can_create_now
            );
        }
        Some(Err(err)) => {
            println!("[probe] NvFBC: unavailable ({err})");
        }
        None => {
            println!("[probe] NvFBC: unavailable (requires X11)");
        }
    }

    let capture_override = std::env::var("ST_CAPTURE").ok();
    let capture_backend = if let Some(ref forced) = capture_override {
        // Honor ST_CAPTURE explicitly — this is the path the system-service
        // unit takes (ST_CAPTURE=kms), and it would be misleading to report
        // "none available" just because this probe didn't try to open
        // `/dev/dri/card*`. The real selection happens later in select_backend().
        match forced.to_lowercase().as_str() {
            "kms" => "KMS (ST_CAPTURE override)",
            "nvfbc" => "NvFBC (ST_CAPTURE override)",
            "pipewire" | "portal" => "PipeWire (ST_CAPTURE override)",
            "wayland" => "Wayland screencopy (ST_CAPTURE override)",
            "x11" => "X11 XShm (ST_CAPTURE override)",
            "ext-image-copy" => "ext-image-copy-capture-v1 (ST_CAPTURE override)",
            _ => "unknown ST_CAPTURE value",
        }
    } else {
        match ds.as_str() {
            "wayland" => {
                if wlr_ok {
                    "Wayland (wlr-screencopy)"
                } else if pipewire_ok {
                    "PipeWire (xdg-desktop-portal)"
                } else {
                    "none available"
                }
            }
            "x11" => {
                if matches!(
                    &nvfbc_probe,
                    Some(Ok(capture::linux::NvfbcProbe {
                        is_capture_possible: true,
                        can_create_now: true,
                    }))
                ) {
                    "NvFBC (NVIDIA)"
                } else if x11_ok {
                    "X11 (XShm)"
                } else {
                    "none available"
                }
            }
            _ => {
                if pipewire_ok {
                    "PipeWire (xdg-desktop-portal)"
                } else {
                    "none available"
                }
            }
        }
    };
    println!("[probe] Selected capture: {capture_backend}");

    let (width, height) = get_screen_resolution().unwrap_or((1920, 1080));
    println!("[probe] Screen resolution: {width}x{height}");

    let config = EncoderConfig::from_env(width, height);
    println!(
        "[probe] Config: {:?} {:?} {}kbps {}fps",
        config.codec, config.dynamic_range, config.bitrate_kbps, config.framerate
    );

    match encode_vaapi::VaapiEncoder::with_config(&config, None) {
        Ok(_) => println!("[probe] Encoder: VAAPI"),
        Err(e) => {
            println!("[probe] Encoder: VAAPI unavailable ({e})");
            match encode::NvencEncoder::with_config(&config) {
                Ok(_) => println!("[probe] Encoder: NVENC"),
                Err(e) => {
                    println!("[probe] Encoder: NVENC unavailable ({e})");
                    match encode_sw::SoftwareEncoder::with_config(&config) {
                        Ok(_) => println!("[probe] Encoder: Software"),
                        Err(e) => eprintln!("[probe] Encoder: NONE ({e})"),
                    }
                }
            }
        }
    }

    let audio_config = encode_config::AudioConfig::from_env();
    let monitor = audio::capture::detect_monitor_source();
    println!(
        "[probe] Audio: {}ch {}Hz (source: {})",
        audio_config.channels,
        audio_config.sample_rate,
        monitor.as_deref().unwrap_or("default")
    );

    println!("--- Probing complete ---\n");
}

#[cfg(target_os = "linux")]
fn get_screen_resolution() -> Option<(u32, u32)> {
    let output = std::process::Command::new("xdpyinfo").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("dimensions:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let dim: Vec<&str> = parts[1].split('x').collect();
                if dim.len() == 2 {
                    let w = dim[0].parse().ok()?;
                    let h = dim[1].parse().ok()?;
                    return Some((w, h));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

async fn run_discovery_beacon(control: Arc<ServerControl>, listen_port: u16) {
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "st-server".to_string())
        });

    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[discovery] Failed to bind UDP socket: {err}");
            return;
        }
    };
    if let Err(err) = sock.set_broadcast(true) {
        eprintln!("[discovery] Failed to enable broadcast: {err}");
        return;
    }

    let dest: std::net::SocketAddr = ([255, 255, 255, 255], DISCOVERY_PORT).into();
    println!("[discovery] Broadcasting beacon on port {DISCOVERY_PORT} (hostname={hostname})");

    loop {
        if control.shutdown_requested() {
            break;
        }
        let token = control.token();
        let peer_id = control.peer_id();
        let packet = format!("ST_DISCOVER\n{hostname}\n{listen_port}\n{token}\n{peer_id}");
        let _ = sock.send_to(packet.as_bytes(), dest).await;
        tokio::time::sleep(DISCOVERY_BEACON_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Hole-punch background task: monitors ApiTunnelState and attempts UDP hole
// punching when the signaling exchange is complete.
// ---------------------------------------------------------------------------

fn spawn_hole_punch_task(state: Arc<ServerState>) {
    let tunnel = match state.tunnel_state.clone() {
        Some(t) => t,
        None => return,
    };
    let listen_port = state.listen_port;
    let state = Arc::clone(&state);
    // Retry a punch nonce a few times before giving up on it. A single failed
    // attempt is often just a timing miss (the client wasn't probing yet, or
    // its candidates hadn't propagated); consuming the nonce on the first
    // failure meant no second chance until the client posted a brand-new nonce.
    const MAX_PUNCH_ATTEMPTS: u32 = 3;
    std::thread::spawn(move || {
        let mut last_handled_punch = None;
        let mut attempted_punch = None;
        let mut punch_attempts: u32 = 0;
        loop {
            if state.control.shutdown_requested() {
                break;
            }
            let Some(pending_punch) = tunnel.pending_client_punch() else {
                attempted_punch = None;
                punch_attempts = 0;
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            if last_handled_punch.as_ref() == Some(&pending_punch)
                || tunnel.is_punch_session_active()
            {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            if attempted_punch.as_ref() != Some(&pending_punch) {
                attempted_punch = Some(pending_punch.clone());
                punch_attempts = 0;
            }
            if !tunnel.is_hole_punch_ready() {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            let socket = match tunnel.clone_punch_socket(listen_port) {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("[hole-punch] Failed to prepare punch socket: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };

            let Some(candidates) = tunnel.partner_candidates(&pending_punch) else {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            if candidates.is_empty() {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            let crypto = match tunnel
                .crypto_context(&pending_punch, st_protocol::tunnel::TunnelMode::Punch)
            {
                Some(c) => c,
                None => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };

            println!(
                "[hole-punch] Attempting to punch through to {} candidate(s)...",
                candidates.len()
            );
            match st_protocol::tunnel::hole_punch(
                &socket,
                &candidates,
                &crypto,
                Duration::from_secs(10),
            ) {
                Ok(peer) => {
                    last_handled_punch = Some(pending_punch.clone());
                    attempted_punch = None;
                    punch_attempts = 0;
                    println!("[hole-punch] Success! Peer confirmed at {peer}");
                    tunnel.set_punch_session_active(true);
                    let punched: Arc<dyn st_protocol::tcp_tunnel::TunnelLink> = Arc::new(
                        st_protocol::reliable_udp::PunchedSocket::new(socket, peer, crypto),
                    );
                    let state2 = Arc::clone(&state);
                    let tunnel2 = Arc::clone(&tunnel);
                    // Run the punched-client handler in a blocking thread.
                    std::thread::spawn(move || {
                        struct ActivePunchGuard(Arc<api_client::ApiTunnelState>);
                        impl Drop for ActivePunchGuard {
                            fn drop(&mut self) {
                                self.0.set_punch_session_active(false);
                            }
                        }
                        let _guard = ActivePunchGuard(Arc::clone(&tunnel2));
                        handle_punched_client(punched, state2);
                    });
                }
                Err(e) => {
                    punch_attempts += 1;
                    eprintln!(
                        "[hole-punch] Failed (attempt {punch_attempts}/{MAX_PUNCH_ATTEMPTS}): {e}"
                    );
                    if punch_attempts >= MAX_PUNCH_ATTEMPTS {
                        last_handled_punch = Some(pending_punch.clone());
                        attempted_punch = None;
                        punch_attempts = 0;
                        eprintln!(
                            "[hole-punch] Giving up on request generation {} after {MAX_PUNCH_ATTEMPTS} attempts",
                            pending_punch.generation
                        );
                    }
                    // Brief backoff; partner candidates refresh on the API poll
                    // cadence, so the next attempt may pick up a fresher mapping.
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });
}

/// TCP-relay background task: when the client posts a relay nonce via the API
/// server (meaning both direct TCP and UDP hole punching failed on its side),
/// dial the relay, complete pairing, and run the standard tunnel session over
/// an end-to-end encrypted TCP tunnel. The relay only ever sees ciphertext.
fn spawn_relay_task(state: Arc<ServerState>, api_url: String) {
    use st_protocol::tcp_tunnel::{connect_relay, resolve_relay_addr};
    let tunnel = match state.tunnel_state.clone() {
        Some(t) => t,
        None => return,
    };
    let state = Arc::clone(&state);
    std::thread::spawn(move || {
        let mut last_handled_relay = None;
        let mut retrying_relay = None;
        let mut retry_failures = 0u32;
        let mut retry_at = Instant::now();
        loop {
            if state.control.shutdown_requested() {
                break;
            }
            let Some(pending) = tunnel.pending_client_relay() else {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            if last_handled_relay.as_ref() == Some(&pending) || tunnel.is_relay_session_active() {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            if retrying_relay.as_ref() != Some(&pending) {
                retrying_relay = Some(pending.clone());
                retry_failures = 0;
                retry_at = Instant::now();
            }
            if Instant::now() < retry_at {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let mut retry = |message: String| {
                retry_failures = retry_failures.saturating_add(1);
                let shift = retry_failures.saturating_sub(1).min(4);
                let delay = Duration::from_millis(250 * (1u64 << shift));
                retry_at = Instant::now() + delay.min(Duration::from_secs(4));
                eprintln!("[relay] {message}; retrying in {}ms", delay.as_millis());
            };
            let Some(crypto) =
                tunnel.crypto_context(&pending, st_protocol::tunnel::TunnelMode::Relay)
            else {
                retry("lease-bound relay crypto is not ready".into());
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            let relay_addr = match resolve_relay_addr(
                &api_url,
                tunnel.relay_port(),
                std::env::var("ST_RELAY_ADDR").ok(),
            ) {
                Some(addr) => addr,
                None => {
                    retry("client requested relay but no relay address is available".into());
                    continue;
                }
            };
            let relay_ticket = match api_client::claim_relay_ticket(
                &api_url,
                &state.control.token(),
                state.control.peer_id(),
                tunnel.lease_id(),
                &pending,
            ) {
                Ok(ticket) => ticket,
                Err(api_client::RelayClaimError::Terminal(error)) => {
                    eprintln!("[relay] {error}; request is no longer owned by this lease pair");
                    last_handled_relay = Some(pending.clone());
                    retrying_relay = None;
                    continue;
                }
                Err(api_client::RelayClaimError::Transient(error)) => {
                    retry(error);
                    continue;
                }
            };
            println!("[relay] Client requested TCP relay; dialing {relay_addr}...");
            match connect_relay(&relay_addr, "host", &relay_ticket, Duration::from_secs(45)) {
                Ok(stream) => {
                    match st_protocol::tcp_tunnel::TcpTunnel::new(stream, Some(crypto), Vec::new())
                    {
                        Ok(tcp_tunnel) => {
                            // Bound concurrent tunnel sessions like the direct
                            // path; drop the pairing if we are already at cap.
                            let Some(slot) = TunnelSessionSlot::acquire() else {
                                retry("tunnel session cap reached after relay pairing".into());
                                continue;
                            };
                            println!("[relay] Paired with client via {relay_addr}");
                            tunnel.set_relay_session_active(true);
                            let state2 = Arc::clone(&state);
                            let tunnel2 = Arc::clone(&tunnel);
                            std::thread::spawn(move || {
                                let _slot = slot;
                                struct ActiveRelayGuard(Arc<api_client::ApiTunnelState>);
                                impl Drop for ActiveRelayGuard {
                                    fn drop(&mut self) {
                                        self.0.set_relay_session_active(false);
                                    }
                                }
                                let _guard = ActiveRelayGuard(tunnel2);
                                handle_punched_client(Arc::new(tcp_tunnel), state2);
                            });
                            last_handled_relay = Some(pending.clone());
                            retrying_relay = None;
                        }
                        Err(e) => {
                            retry(format!("tunnel setup failed after pairing: {e}"));
                        }
                    }
                }
                Err(e) => retry(e),
            }
        }
    });
}

/// Handle a client connection over a tunnel link: a hole-punched UDP socket
/// or a TCP fallback tunnel (direct upgrade or API-server relay). All control
/// and media traffic flows through the single link.
fn handle_punched_client(
    punched: Arc<dyn st_protocol::tcp_tunnel::TunnelLink>,
    state: Arc<ServerState>,
) {
    use st_protocol::reliable_udp::PunchedMessage;
    let peer = punched.peer();
    let reliable = punched.is_reliable();
    let tag = if reliable { "tcp-tunnel" } else { "punched" };
    println!("[{tag}] Client connected: {peer}");

    // Set short read timeout for the handshake phase.
    let _ = punched.set_read_timeout(Some(Duration::from_millis(100)));

    // --- Authentication ---
    let token = state.control.token();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut authenticated = false;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            if let Some((ControlMessage::Authenticate(client_token), _)) =
                ControlMessage::deserialize(&data)
            {
                if constant_time_eq(client_token.as_bytes(), token.as_bytes()) {
                    authenticated = true;
                    let resp = ControlMessage::AuthResult(true).serialize();
                    let _ = punched.send_control(&resp);
                    break;
                } else {
                    let resp = ControlMessage::AuthResult(false).serialize();
                    let _ = punched.send_control(&resp);
                    eprintln!("[{tag}] Auth failed from {peer}");
                    return;
                }
            }
        }
    }
    if !authenticated {
        eprintln!("[{tag}] Auth timeout from {peer}");
        return;
    }
    println!("[{tag}] Client {peer} authenticated");

    // --- Read ClientDisplayInfo ---
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client_display: Option<ClientDisplayInfo> = None;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            if let Some((ControlMessage::ClientDisplayInfo(info), _)) =
                ControlMessage::deserialize(&data)
            {
                client_display = Some(info);
                break;
            }
        }
    }
    let client_display = match client_display {
        Some(d) => d,
        None => {
            eprintln!("[{tag}] Timeout waiting for ClientDisplayInfo from {peer}");
            return;
        }
    };

    let registered_client = state.control.register_client(peer);
    let client_id = state.input.allocate_client_id();

    let video_capabilities = client_video_capabilities(Some(client_display));
    let video_membership = begin_video_membership(&state, video_capabilities);

    // --- Start/subscribe to pipeline ---
    trigger_screen_wake(&state.control);
    let (setup_tx, setup_rx) = bounded(1);
    let setup_state = Arc::clone(&state);
    let setup_cancelled = Arc::clone(&video_membership.cancelled);
    let setup_worker_cancelled = Arc::clone(&setup_cancelled);
    let membership_id = video_membership.id;
    std::thread::spawn(move || {
        let result = setup_client_pipeline(&setup_state, membership_id, setup_worker_cancelled);
        let _ = setup_tx.send(result);
    });
    let setup_deadline = Instant::now() + Duration::from_secs(35);
    let setup = loop {
        match setup_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => break result,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break Err("pipeline setup worker disconnected".into())
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                punched.tick();
                if registered_client.disconnect_requested()
                    || punched.is_closed()
                    || Instant::now() >= setup_deadline
                {
                    setup_cancelled.store(true, Ordering::Release);
                    return;
                }
            }
        }
    };

    let (sub, startup_snapshot, rate_control, session_debug, capture_state) = match setup {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[{tag}] Pipeline error for {peer}: {e}");
            let _ = punched.send_control(&ControlMessage::Error(e).serialize());
            return;
        }
    };

    rate_control.register_client(sub.vid_sub_id);
    let controller_state = state.input.controller_state_for(client_id);
    let stream_config = startup_snapshot.config;
    let input_credential = input::generate_input_credential();
    let input_registration = state
        .input
        .register_tunnel_client(client_id, input_credential);

    // --- Send startup control bundle ---
    let mut control_buf = ControlMessage::StreamConfig(stream_config).serialize();
    control_buf.extend_from_slice(&ControlMessage::SessionDebugInfo(session_debug).serialize());
    control_buf.extend_from_slice(
        &ControlMessage::InputSession(InputSession {
            client_id,
            credential: input_credential,
        })
        .serialize(),
    );
    control_buf.extend_from_slice(
        &ControlMessage::InputCapabilities(state.input.capabilities()).serialize(),
    );
    control_buf.extend_from_slice(&ControlMessage::ControllerState(controller_state).serialize());
    let available_outputs = capture_state.outputs();
    if available_outputs.len() > 1 {
        control_buf
            .extend_from_slice(&ControlMessage::AvailableOutputs(available_outputs).serialize());
        control_buf
            .extend_from_slice(&ControlMessage::SelectOutput(capture_state.selected()).serialize());
    }
    let _ = punched.send_control(&control_buf);

    // Wait for ClientReadyForMedia.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            let mut offset = 0;
            while let Some((msg, used)) = ControlMessage::deserialize(&data[offset..]) {
                offset += used;
                if matches!(msg, ControlMessage::ClientReadyForMedia) {
                    ready = true;
                    break;
                }
            }
            if ready {
                break;
            }
        }
    }
    if !ready {
        eprintln!("[{tag}] Timeout waiting for ClientReadyForMedia from {peer}");
        rate_control.unregister_client(sub.vid_sub_id);
        let _ = state.input.release_control(client_id);
        unsubscribe_and_maybe_stop_pipeline(
            &state,
            sub.pipeline_instance_id,
            sub.vid_sub_id,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            sub.aud_sub_id,
        );
        return;
    }

    // --- Start transport (video + audio) over punched socket ---
    let audio_enabled = Arc::new(AtomicBool::new(true));
    let transport_running = Arc::new(AtomicBool::new(true));
    sub.video_bc.request_keyframe();
    let vid_rx = sub.vid_rx;
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let aud_rx = Some(sub.aud_rx);
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let aud_rx: Option<Receiver<Arc<EncodedAudioPacket>>> = None;
    let transport_running_clone = Arc::clone(&transport_running);
    let audio_enabled_transport = Arc::clone(&audio_enabled);
    let punched_transport = Arc::clone(&punched);
    let video_bc = Arc::clone(&sub.video_bc);
    // A2: adaptive FEC strength shared from the control loop to the sender.
    // Reliable (TCP) links pin every loss-recovery knob to zero — there is no
    // packet loss to recover from, so the extra bytes are pure overhead.
    let mut fec_controller = adaptive_bitrate::FecController::from_env();
    let fec_pct_shared = Arc::new(AtomicU16::new(if reliable {
        0
    } else {
        fec_controller.current_pct()
    }));
    let fec_pct_transport = Arc::clone(&fec_pct_shared);
    // Duplicate-FrameStart utility controller (A/B probe).
    let mut dup_first_controller = adaptive_bitrate::DupFirstController::new();
    let dup_first_shared = Arc::new(AtomicBool::new(dup_first_controller.enabled()));
    let dup_first_transport = Arc::clone(&dup_first_shared);
    // Server-side cap→send backlog (µs, EWMA) for bufferbloat-aware bitrate.
    let send_backlog_shared = Arc::new(AtomicU32::new(0));
    let send_backlog_transport = Arc::clone(&send_backlog_shared);
    // Fixed configured redundancy is default; adaptation is explicit opt-in.
    let audio_redundancy_adaptive = transport::audio_adaptive_redundancy_enabled();
    let audio_redundancy_max = transport::configured_audio_redundancy_depth() as u8;
    let mut audio_redundancy_controller =
        adaptive_bitrate::AudioRedundancyController::new(audio_redundancy_max);
    let audio_depth_init = if reliable {
        0
    } else if audio_redundancy_adaptive {
        audio_redundancy_controller.current_depth()
    } else {
        audio_redundancy_max
    };
    let audio_depth_shared = Arc::new(AtomicU8::new(audio_depth_init));
    let audio_depth_transport = Arc::clone(&audio_depth_shared);
    let transport_handle = std::thread::spawn(move || {
        run_punched_transport(
            punched_transport,
            vid_rx,
            video_bc,
            aud_rx,
            audio_enabled_transport,
            transport_running_clone,
            fec_pct_transport,
            audio_depth_transport,
            dup_first_transport,
            send_backlog_transport,
        );
    });

    let _ = punched.send_control(&ControlMessage::StreamStarted.serialize());
    println!("[{tag}] Client {peer} subscribed (transport started)");
    let (clipboard_control_tx, clipboard_control_rx) = bounded::<ControlMessage>(8);
    let (file_detect_tx, file_detect_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(8);
    let suppressed_paths = clipboard::new_suppressed_paths();
    let mut clipboard_sync = clipboard::ClipboardSync::start_with_file_detection(
        "server-punched",
        clipboard_control_tx,
        file_detect_tx,
        Arc::clone(&suppressed_paths),
    );
    let mut ft_manager = file_transfer::FileTransferManager::start_auto_accept(
        st_protocol::file_transfer::TransportMode::Punched,
        file_transfer::new_shared_state(),
        suppressed_paths,
    );

    // --- Control loop: read from punched socket ---
    let mut bitrate_controller = ClientRateController::from_state(rate_control.as_ref());
    let mut cursor_versions = CursorVersionCursor::default();
    let mut last_transport_recovery_keyframe = Instant::now() - Duration::from_secs(1);
    let mut last_controller_state = controller_state;
    let mut last_config_generation = startup_snapshot.generation;
    let mut last_video_epoch = startup_snapshot.video_epoch;
    let mut last_selection_generation = startup_snapshot.selection_generation;
    let _ = punched.set_nonblocking(false);
    let _ = punched.set_read_timeout(Some(Duration::from_millis(50)));

    // UDP has no FIN; if the peer vanishes (crash, network loss, normal disconnect)
    // nothing tells us. Without this timeout the loop sits here forever holding
    // `punch_session_active = true`, which blocks the next hole-punch attempt in
    // `spawn_hole_punch_task` (see is_punch_session_active gate). The client sends
    // TransportFeedback every ~500ms while a stream is running, so anything beyond
    // a few seconds of silence means the client is gone.
    const PUNCHED_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5);
    let mut last_peer_activity = Instant::now();

    loop {
        if registered_client.disconnect_requested() || state.control.shutdown_requested() {
            break;
        }
        if last_peer_activity.elapsed() > PUNCHED_INACTIVITY_TIMEOUT {
            println!(
                "[{tag}] No traffic from {peer} for {}s — treating as disconnected",
                PUNCHED_INACTIVITY_TIMEOUT.as_secs()
            );
            break;
        }
        punched.tick();
        let controller_state = state.input.controller_state_for(client_id);
        if controller_state != last_controller_state {
            let _ = punched
                .send_control(&ControlMessage::ControllerState(controller_state).serialize());
            last_controller_state = controller_state;
        }
        // Re-sync this client's decoder after an output switch reconfigured the
        // shared stream (see direct-path handler for rationale).
        if let Some(snapshot) = capture_state.snapshot().filter(|snapshot| {
            snapshot.generation != last_config_generation
                || snapshot.video_epoch != last_video_epoch
                || snapshot.selection_generation != last_selection_generation
        }) {
            last_config_generation = snapshot.generation;
            if punched
                .send_control(&ControlMessage::StreamConfig(snapshot.config).serialize())
                .is_err()
            {
                break;
            }
            let _ = punched
                .send_control(&ControlMessage::SelectOutput(capture_state.selected()).serialize());
            last_video_epoch = snapshot.video_epoch;
            last_selection_generation = snapshot.selection_generation;
        }
        while let Ok(message) = clipboard_control_rx.try_recv() {
            let _ = punched.send_control(&message.serialize());
        }
        while let Ok(path) = file_detect_rx.try_recv() {
            let _ = ft_manager
                .inbound_tx
                .try_send(file_transfer::FtInbound::SendFile { path });
        }
        while let Ok(message) = ft_manager.outbound_rx.try_recv() {
            let _ = punched.send_control(&message.serialize());
        }

        // Read from punched socket.
        match punched.try_recv() {
            Some(PunchedMessage::Control(data)) => {
                last_peer_activity = Instant::now();
                let mut offset = 0;
                while let Some((msg, used)) = ControlMessage::deserialize(&data[offset..]) {
                    offset += used;
                    match msg {
                        ControlMessage::SetAudio(enabled) => {
                            audio_enabled.store(enabled, Ordering::Relaxed);
                        }
                        ControlMessage::TransportFeedback(fb) => {
                            // Loss-recovery controllers stay parked at zero on
                            // reliable (TCP) links — nothing to recover from.
                            if !reliable {
                                // A2: drive RS FEC strength from the same loss signal.
                                let fec_pct = fec_controller.apply_feedback(&fb);
                                fec_pct_shared.store(fec_pct, Ordering::Relaxed);
                                // Optional adaptation uses the same aggregate loss signal.
                                if audio_redundancy_adaptive {
                                    audio_redundancy_controller.synchronize_sender_depth(
                                        audio_depth_shared.load(Ordering::Relaxed),
                                    );
                                    let depth = audio_redundancy_controller.apply_feedback(&fb);
                                    audio_depth_shared.store(depth, Ordering::Relaxed);
                                }
                                // Duplicate-FrameStart A/B: keep it only while it helps.
                                let dup_on = dup_first_controller.apply_feedback(&fb);
                                dup_first_shared.store(dup_on, Ordering::Relaxed);
                            }
                            // Bufferbloat: feed the server-side cap→send backlog.
                            bitrate_controller
                                .note_send_backlog_us(send_backlog_shared.load(Ordering::Relaxed));
                            let next_kbps = bitrate_controller.apply_feedback(fb);
                            rate_control.update_client_target(sub.vid_sub_id, next_kbps);
                            if (fb.lost_packets > 0 || fb.dropped_frames > 0)
                                && last_transport_recovery_keyframe.elapsed()
                                    >= Duration::from_millis(250)
                            {
                                sub.video_bc.request_keyframe();
                                last_transport_recovery_keyframe = Instant::now();
                            }
                        }
                        ControlMessage::ClientBitratePreference(max_kbps) => {
                            // B4: clamp this client's ABR ceiling to its declared max.
                            rate_control.set_client_ceiling(sub.vid_sub_id, max_kbps);
                        }
                        ControlMessage::AcquireControl => {
                            let next_state = state.input.acquire_control(client_id);
                            let state_msg = ControlMessage::ControllerState(next_state);
                            cursor_versions = CursorVersionCursor::default();
                            last_controller_state = next_state;
                            let _ = punched.send_control(&state_msg.serialize());
                        }
                        ControlMessage::ReleaseControl => {
                            let next_state = state.input.release_control(client_id);
                            let state_msg = ControlMessage::ControllerState(next_state);
                            last_controller_state = next_state;
                            let _ = punched.send_control(&state_msg.serialize());
                        }
                        ControlMessage::RequestKeyframe => {
                            sub.video_bc.request_keyframe();
                        }
                        ControlMessage::SelectOutput(id) => {
                            capture_state.request_output(id);
                        }
                        ControlMessage::ClipboardText(text) => {
                            clipboard_sync.set_remote_text(text);
                        }
                        ControlMessage::TextInput(text) => {
                            let _ = state.input.handle_text_input(client_id, &text);
                        }
                        ControlMessage::FileOffer {
                            transfer_id,
                            file_size,
                            file_name,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::OfferReceived {
                                    transfer_id,
                                    file_size,
                                    file_name,
                                },
                            );
                        }
                        ControlMessage::FileAccept {
                            transfer_id,
                            accepted,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::AcceptReceived {
                                    transfer_id,
                                    accepted,
                                },
                            );
                        }
                        ControlMessage::FileChunk {
                            transfer_id,
                            chunk_index,
                            data,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ChunkReceived {
                                    transfer_id,
                                    chunk_index,
                                    data,
                                },
                            );
                        }
                        ControlMessage::FileComplete {
                            transfer_id,
                            total_chunks,
                            sha256,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::CompleteReceived {
                                    transfer_id,
                                    total_chunks,
                                    sha256,
                                },
                            );
                        }
                        ControlMessage::FileCancel { transfer_id } => {
                            let _ = ft_manager
                                .inbound_tx
                                .try_send(file_transfer::FtInbound::CancelReceived { transfer_id });
                        }
                        ControlMessage::FileProgress {
                            transfer_id,
                            chunks_received,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ProgressReceived {
                                    transfer_id,
                                    chunks_received,
                                },
                            );
                        }
                        _ => {}
                    }
                }
            }
            Some(PunchedMessage::Media(data)) => {
                last_peer_activity = Instant::now();
                // Demux input packets from the media channel. The punched peer
                // address is fixed by the hole punch (no media_dest cell), so
                // the relearn is a no-op here.
                if let Some((header, credential, packet)) =
                    st_protocol::InputPacket::deserialize(&data)
                {
                    state
                        .input
                        .handle_tunnel_input_packet(header.seq, credential, packet, client_id);
                }
            }
            None => {}
        }

        // Send cursor updates.
        for message in state.input.cursor_messages(client_id, &mut cursor_versions) {
            let serialized: Vec<u8> = message.serialize();
            let _ = punched.send_control(&serialized);
        }
    }

    // Cleanup.
    clipboard_sync.stop();
    ft_manager.stop();
    transport_running.store(false, Ordering::SeqCst);
    let _ = state.input.release_control(client_id);
    drop(input_registration);
    let _ = transport_handle.join();
    rate_control.unregister_client(sub.vid_sub_id);
    unsubscribe_and_maybe_stop_pipeline(
        &state,
        sub.pipeline_instance_id,
        sub.vid_sub_id,
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        sub.aud_sub_id,
    );
    println!("[{tag}] Client {peer} disconnected");
}

/// Per-client transport loop for punched connections.
// Same adaptive-control Arc set as run_transport, over the punched socket.
#[allow(clippy::too_many_arguments)]
fn run_punched_transport(
    punched: Arc<dyn st_protocol::tcp_tunnel::TunnelLink>,
    vid_rx: Receiver<Arc<EncodedVideoFrame>>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    aud_rx: Option<Receiver<Arc<EncodedAudioPacket>>>,
    audio_enabled: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    // A2: adaptive RS parity percentage from the control loop.
    fec_pct: Arc<AtomicU16>,
    // Optional adaptive verbatim audio-redundancy depth.
    audio_depth: Arc<AtomicU8>,
    // Auto-mode duplicate-FrameStart verdict from the DupFirstController.
    dup_first: Arc<AtomicBool>,
    // Server-side cap→send backlog (µs, EWMA) for the bitrate controller.
    send_backlog_us: Arc<AtomicU32>,
) {
    let peer = punched.peer();
    let mut sender = UdpSender::from_tunnel(punched);
    let trace = trace_enabled();
    let interleave_audio = audio_interleave_enabled();
    let mut last_backlog_keyframe_request = Instant::now() - Duration::from_secs(1);
    let mut waiting_for_recovery_frame = false;
    let mut last_source_seq = None::<u64>;
    let mut last_fec_pct = u16::MAX;
    let mut last_audio_depth = u8::MAX;
    let mut last_dup_first: Option<bool> = None;
    let mut backlog_ewma_us: u32 = 0;
    let mut backlog_seen = false;
    let hard_ceiling_us = max_queue_latency_us();
    // B1: keepalive cadence so the punched client's inactivity timeout doesn't
    // fire on an idle (static-screen) path. Reset on each video send below.
    const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
    let mut last_keepalive = Instant::now();
    while running.load(Ordering::SeqCst) {
        if last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            let _ = sender.send_keepalive();
            last_keepalive = Instant::now();
        }
        let pct = fec_pct.load(Ordering::Relaxed);
        if pct != last_fec_pct {
            sender.set_fec_pct(pct);
            last_fec_pct = pct;
        }
        let depth = audio_depth.load(Ordering::Relaxed);
        if depth != last_audio_depth {
            sender.set_audio_redundancy_depth(depth as usize);
            last_audio_depth = depth;
        }
        let dup_on = dup_first.load(Ordering::Relaxed);
        if last_dup_first != Some(dup_on) {
            sender.set_dup_first(dup_on);
            last_dup_first = Some(dup_on);
        }
        match vid_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(frame) => {
                // FIFO drain — send every encoded unit in order, never collapse to
                // newest. See run_client_transport for the rationale: dropping an
                // intermediate P-frame unit breaks the decode chain and traps the
                // stream in keyframe-only recovery. Overload is handled by
                // broadcaster eviction → source gap → recovery path below.
                let mut pending = Some(frame);
                let mut burst = 0usize;
                while let Some(frame) = pending.take() {
                    // B6: push queued audio ahead of this video unit.
                    if interleave_audio {
                        flush_pending_audio(
                            &mut sender,
                            &aud_rx,
                            &audio_enabled,
                            &audio_depth,
                            peer,
                        );
                    }
                    let source_gap = last_source_seq
                        .map(|last| frame.source_seq.saturating_sub(last.saturating_add(1)))
                        .unwrap_or(0);
                    last_source_seq = Some(frame.source_seq);

                    // Server-side cap→send latency for this unit (see run_transport).
                    let frame_now_us = unix_time_micros();
                    if observe_send_backlog(
                        frame.capture_micros,
                        frame_now_us,
                        &mut backlog_ewma_us,
                        &mut backlog_seen,
                        &send_backlog_us,
                        hard_ceiling_us,
                    ) && !frame.is_recovery
                        && !waiting_for_recovery_frame
                    {
                        waiting_for_recovery_frame = true;
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] punched cap→send backlog {}µs over ceiling; draining to recovery keyframe",
                                frame_now_us.saturating_sub(frame.capture_micros)
                            );
                        }
                    }

                    if source_gap > 0 {
                        waiting_for_recovery_frame = true;
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] detected {source_gap} dropped punched video unit(s) (broadcaster eviction); requesting recovery keyframe"
                            );
                        }
                    }

                    if waiting_for_recovery_frame && !frame.is_recovery {
                        if last_backlog_keyframe_request.elapsed() >= Duration::from_millis(250) {
                            video_bc.request_keyframe();
                            last_backlog_keyframe_request = Instant::now();
                        }
                        if trace {
                            eprintln!(
                                "[trace][server] holding punched non-recovery video unit source_seq={} while waiting for recovery",
                                frame.source_seq
                            );
                        }
                    } else {
                        if waiting_for_recovery_frame && frame.is_recovery {
                            waiting_for_recovery_frame = false;
                            if trace {
                                eprintln!(
                                    "[trace][server] resumed punched video on recovery unit source_seq={}",
                                    frame.source_seq
                                );
                            }
                        }
                        // B1: real video is liveness — defer the next keepalive.
                        last_keepalive = Instant::now();
                        if let Err(e) = sender.send_frame(&frame, frame_now_us) {
                            eprintln!("[punched-transport] video send error: {e}");
                        }
                    }

                    burst += 1;
                    if burst >= MAX_VIDEO_SEND_BURST {
                        break;
                    }
                    pending = vid_rx.try_recv().ok();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(ref aud) = aud_rx {
            let send_audio = audio_enabled.load(Ordering::Relaxed);
            while let Ok(opus) = aud.try_recv() {
                if send_audio {
                    if let Err(e) = sender.send_audio(&opus, &audio_depth) {
                        eprintln!("[punched-transport] audio send error: {e}");
                    }
                }
            }
        }
    }
}

async fn run_server(state: Arc<ServerState>) -> Result<(), String> {
    let listen_addr = format!("0.0.0.0:{}", state.listen_port);
    let listener = TcpListener::bind(&listen_addr)
        .await
        .map_err(|err| format!("Failed to bind TCP listener on {listen_addr}: {err}"))?;

    // Spawn discovery beacon
    tokio::spawn(run_discovery_beacon(
        Arc::clone(&state.control),
        state.listen_port,
    ));

    // Spawn API server registration
    const API_SERVER_URL: &str = "https://st-api.kubemaxx.io";
    let api_url = std::env::var("ST_API_URL")
        .unwrap_or_else(|_| API_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    {
        let tunnel = state
            .tunnel_state
            .clone()
            .unwrap_or_else(|| Arc::new(api_client::ApiTunnelState::new()));
        // Ask the router for an explicit external port forward via NAT-PMP.
        // When it succeeds, the resulting candidate works even on symmetric
        // NATs and survives idle periods — a strict superset of what STUN+
        // hole-punch can do alone. When the router doesn't speak NAT-PMP,
        // this is a quiet no-op.
        api_client::start_port_mapping(Arc::clone(&state.control), Arc::clone(&tunnel));
        api_client::start_api_registration(
            api_url.clone(),
            Arc::clone(&state.control),
            state.listen_port,
            tunnel,
        );
    }

    // Spawn hole-punch background task.
    spawn_hole_punch_task(Arc::clone(&state));

    // Spawn TCP-relay fallback task (dials the API server's relay when the
    // client signals that both direct TCP and UDP hole punching failed).
    spawn_relay_task(Arc::clone(&state), api_url);

    // Handle Ctrl+C so the API thread can unregister cleanly.
    {
        let ctrl = Arc::clone(&state.control);
        // Detached fire-and-forget handler; the JoinHandle is intentionally
        // dropped (the task runs until the process exits).
        let _ctrl_c_task = tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            println!("\n[server] Shutting down...");
            ctrl.request_shutdown();
        });
    }

    println!("Server started. Waiting for clients on {listen_addr}...");
    println!(
        "  Overrides: ST_PORT, ST_CODEC, ST_HDR, ST_BITRATE, ST_MIN_BITRATE, ST_MAX_BITRATE, ST_FPS, ST_GOP, ST_AUDIO, ST_CAPTURE, ST_TOKEN, ST_API_URL"
    );

    while !state.control.shutdown_requested() {
        match tokio::time::timeout(Duration::from_millis(250), listener.accept()).await {
            Err(_) => continue,
            Ok(Ok((mut stream, addr))) => {
                if !state.control.allow_new_connections() {
                    println!("[server] Rejecting blocked client connection from {addr}");
                    let _ = stream
                        .write_all(
                            &ControlMessage::Error(
                                "Server is currently blocking new connections.".into(),
                            )
                            .serialize(),
                        )
                        .await;
                    continue;
                }
                let state = Arc::clone(&state);
                tokio::spawn(handle_client(stream, addr, state));
            }
            Ok(Err(e)) => eprintln!("Accept error: {e}"),
        }
    }

    state.control.disconnect_all_clients();
    // Give the API registration thread time to unregister (it polls every 500ms).
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}

fn build_server_state(
    control: Arc<ServerControl>,
    listen_port: u16,
    tunnel_state: Option<Arc<api_client::ApiTunnelState>>,
) -> Arc<ServerState> {
    let input = InputRuntime::new();
    input.spawn_listener(listen_port);
    Arc::new(ServerState {
        pipeline: Mutex::new(None),
        pipeline_starting: AtomicBool::new(false),
        pending_pipeline_stop: Mutex::new(None),
        pipeline_lifecycle: Mutex::new(()),
        profile_revision: Arc::new(AtomicU64::new(0)),
        profile_commit: Arc::new(Mutex::new(())),
        video_capabilities: Mutex::new(VideoCapabilityRegistry::default()),
        input,
        control,
        listen_port,
        tunnel_state,
    })
}

fn run_server_runtime(state: Arc<ServerState>) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("Failed to build Tokio runtime: {err}"))?;
    runtime.block_on(run_server(state))
}

fn join_server_thread(handle: std::thread::JoinHandle<Result<(), String>>) -> ! {
    match handle.join() {
        Ok(Ok(())) => std::process::exit(0),
        Ok(Err(err)) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("Server runtime thread panicked.");
            std::process::exit(1);
        }
    }
}

/// How this process was launched.
///
/// - `Normal`: default per-user behavior — full pipeline plus an in-process tray.
/// - `System`: system-wide service — full pipeline, no tray, state under
///   `/var/lib/st-server`, control socket hosted for a per-user tray agent.
/// - `Tray`: the per-user tray agent — connects to a system service's control
///   socket and shows the tray only (Linux).
#[derive(PartialEq, Eq, Clone, Copy)]
enum RunMode {
    Normal,
    System,
    Tray,
}

fn parse_run_mode() -> RunMode {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--system" => return RunMode::System,
            "--tray" => return RunMode::Tray,
            _ => {}
        }
    }
    match std::env::var("ST_MODE").as_deref() {
        Ok("system") => RunMode::System,
        Ok("tray") => RunMode::Tray,
        _ => RunMode::Normal,
    }
}

/// Wake the remote display when a client connects. Splits by run mode because
/// only one side of the process tree has the session env needed to talk to the
/// compositor/screensaver:
/// - System mode (root service, no session bus): bump the wake counter so the
///   per-user tray agent — which lives in the graphical session and has
///   WAYLAND_DISPLAY/DBUS_SESSION_BUS_ADDRESS/XDG_RUNTIME_DIR — runs the actual
///   unblank in-session. The root service calling `screen_wake` directly would
///   only fail noisily as root.
/// - Per-user / `cargo run`: this process is already in the session, so wake the
///   display directly.
///
/// `screen_wake::wake_display` and `ServerControl::request_wake` both honor the
/// `ST_WAKE_ON_CONNECT=0` / debounce escape hatches, so this is safe to call on
/// every connect.
fn trigger_screen_wake(control: &crate::server_control::ServerControl) {
    if std::env::var_os("ST_SYSTEM_MODE").is_some() {
        control.request_wake();
    } else {
        screen_wake::wake_display();
    }
}

/// Set the environment defaults for system-wide mode before any subsystem reads
/// them. KMS is the only capture backend that works at the login screen and
/// follows the active seat across user switches; the tray lives in a separate
/// per-user agent; state goes to a root-owned dir. Each is only set if the user
/// hasn't already overridden it, preserving the escape hatches.
#[cfg(target_os = "linux")]
fn apply_system_mode_env() {
    if std::env::var_os("ST_CAPTURE").is_none() {
        std::env::set_var("ST_CAPTURE", "kms");
    }
    std::env::set_var("ST_SERVER_NO_TRAY", "1");
    std::env::set_var("ST_SYSTEM_MODE", "1");
    if std::env::var_os("ST_STATE_DIR").is_none() {
        std::env::set_var("ST_STATE_DIR", "/var/lib/st-server");
    }
    println!(
        "[system] system-wide mode: capture={}, tray disabled, state={}, control socket={}",
        std::env::var("ST_CAPTURE").unwrap_or_else(|_| "kms".into()),
        std::env::var("ST_STATE_DIR").unwrap_or_else(|_| "/var/lib/st-server".into()),
        control_ipc::default_socket_path().display(),
    );
}

fn main() {
    match updater::maybe_run_apply_update_from_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            eprintln!("[updater] {err}");
            std::process::exit(1);
        }
    }

    let mode = parse_run_mode();

    // Per-user tray agent: connect to the system service's control socket and
    // run the tray only. No pipeline in this process.
    #[cfg(target_os = "linux")]
    if mode == RunMode::Tray {
        let socket = control_ipc::default_socket_path();
        match tray::run_tray_agent(&socket) {
            Ok(()) => return,
            Err(err) => {
                eprintln!("[tray] {err}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    if mode == RunMode::Tray {
        eprintln!("[tray] --tray agent mode is Linux-only");
        std::process::exit(1);
    }

    #[cfg(target_os = "linux")]
    if mode == RunMode::System {
        apply_system_mode_env();
    }

    #[cfg(target_os = "linux")]
    probe_backends();

    let listen_port = configured_listen_port();
    let control = ServerControl::new();

    let tunnel_state = Some(Arc::new(api_client::ApiTunnelState::new()));
    let state = build_server_state(Arc::clone(&control), listen_port, tunnel_state.clone());

    // Wire the session game-mode hint (tray agent → control socket → here) to the
    // input runtime, which ORs it into CursorState.app_grab.
    control.set_game_mode_hook(Box::new({
        let input = Arc::clone(&state.input);
        move |on| input.set_game_mode(on)
    }));

    // System-wide mode: no tray in this process. Host the control socket so a
    // per-user `st-server --tray` agent can drive it, then run headless.
    #[cfg(unix)]
    if mode == RunMode::System {
        control.start_automatic_update_checks();
        // Bring up (and follow) the active user's tray agent. enable-at-login
        // covers fresh logins; this covers a manually-started service, a quit
        // tray, and user switches without a re-login.
        #[cfg(target_os = "linux")]
        session_follow::spawn_tray_follow();
        let ipc_control = Arc::clone(&control);
        let ipc_tunnel = tunnel_state.clone();
        std::thread::spawn(move || {
            let path = control_ipc::default_socket_path();
            if let Err(err) = control_ipc::serve(ipc_control, ipc_tunnel, &path) {
                eprintln!("[control-ipc] serve failed: {err}");
            }
        });
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    if mode == RunMode::Normal && tray::should_run_tray() {
        control.start_automatic_update_checks();
        let server_state = Arc::clone(&state);
        let server_handle = std::thread::spawn(move || run_server_runtime(server_state));
        match tray::run_tray(Arc::clone(&control), tunnel_state.clone()) {
            Ok(()) => {
                control.request_shutdown();
                join_server_thread(server_handle);
            }
            Err(err) => {
                eprintln!("[tray] {err}");
                eprintln!("[tray] falling back to headless mode");
                join_server_thread(server_handle);
            }
        }
    }

    if let Err(err) = run_server_runtime(state) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod video_transition_tests {
    use super::*;

    fn config(codec: VideoCodec, framerate: u16) -> StreamConfig {
        StreamConfig {
            video_epoch: 0,
            codec,
            width: 1920,
            height: 1080,
            framerate,
            audio_sample_rate: 48_000,
            audio_channels: 2,
            hdr: false,
            chroma: VideoChromaSampling::Yuv420,
            packet_duration_ms: 5,
        }
    }

    #[test]
    fn stale_rebuild_revision_cannot_replace_new_profile() {
        assert!(!rebuild_revision_is_current(4, 5));
        assert!(rebuild_revision_is_current(5, 5));
    }

    #[test]
    fn startup_snapshot_observes_concurrent_config_generation() {
        let (command_tx, _command_rx) = bounded(1);
        let state = SharedCaptureState::new(command_tx);
        let startup = state.initialize_config(config(VideoCodec::Hevc, 120));
        let current = state.commit_encoder_config(config(VideoCodec::H264, 60));

        assert!(current.generation > startup.generation);
        assert_eq!(state.snapshot(), Some(current));
        assert_ne!(startup.video_epoch, current.video_epoch);
    }

    #[test]
    fn same_resolution_output_switch_publishes_new_selection_and_media_epoch() {
        let (command_tx, _command_rx) = bounded(1);
        let state = SharedCaptureState::new(command_tx);
        let initial = state.initialize_config(config(VideoCodec::H264, 60));
        let switched = state.commit_output_selection(config(VideoCodec::H264, 60));

        assert_eq!(switched.generation, initial.generation);
        assert!(switched.selection_generation > initial.selection_generation);
        assert!(switched.video_epoch > initial.video_epoch);
        assert_eq!(switched.config.video_epoch, switched.video_epoch);
    }

    #[test]
    fn cancelled_or_stale_profile_request_cannot_commit() {
        let revision_source = Arc::new(AtomicU64::new(7));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (response, _response_rx) = bounded(1);
        let request = ProfileRequest {
            capabilities: AggregateVideoCapabilities {
                supported_codecs: VideoCodecSupport::h264_only(),
                hardware_codecs: VideoCodecSupport::h264_only(),
                supported_yuv444_codecs: VideoCodecSupport::empty(),
                hardware_yuv444_codecs: VideoCodecSupport::empty(),
                hdr_display: false,
                requested_fps: Some(60),
            },
            registry_revision: 7,
            revision_source: Arc::clone(&revision_source),
            commit_lock: Arc::new(Mutex::new(())),
            cancelled: Arc::clone(&cancelled),
            response,
        };
        assert!(request.is_current());
        revision_source.store(8, Ordering::Release);
        assert!(!request.is_current());
        revision_source.store(7, Ordering::Release);
        cancelled.store(true, Ordering::Release);
        assert!(!request.is_current());
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod bitrate_verifier_tests {
    use super::*;

    fn feed(verifier: &mut BitrateVerifier, kbps: u32, fps: u32, secs: u32) {
        // Feed `secs` seconds of frames whose sizes correspond to `kbps`.
        let bytes_per_frame = (kbps as u64 * 1000 / 8 / fps as u64) as usize;
        for _ in 0..(fps * secs) {
            verifier.record(bytes_per_frame);
        }
    }

    #[test]
    fn does_not_trip_when_output_tracks_lowered_bitrate() {
        let t0 = Instant::now();
        let mut v = BitrateVerifier::new(60, t0);
        // Asked encoder to drop to 5000 kbps; encoder complies and emits ~5000.
        v.arm_downward(5_000, t0);
        feed(&mut v, 5_000, 60, 2);
        let failed = v.check_and_take_failure(t0 + Duration::from_millis(1600));
        assert!(!failed, "in-place change that took effect must not trip");
        assert!(!v.inplace_ineffective);
    }

    #[test]
    fn trips_when_encoder_ignores_downward_change() {
        let t0 = Instant::now();
        let mut v = BitrateVerifier::new(60, t0);
        // Asked for 5000 kbps but encoder keeps blasting ~20000 kbps.
        v.arm_downward(5_000, t0);
        feed(&mut v, 20_000, 60, 2);
        let failed = v.check_and_take_failure(t0 + Duration::from_millis(1600));
        assert!(failed, "encoder ignoring the cap must trigger a rebuild");
    }

    #[test]
    fn disables_inplace_after_repeated_failures() {
        let t0 = Instant::now();
        let mut v = BitrateVerifier::new(60, t0);
        for i in 0..BitrateVerifier::FAILURES_TO_DISABLE_INPLACE {
            let base = t0 + Duration::from_secs(i as u64 * 3);
            v.arm_downward(5_000, base);
            feed(&mut v, 20_000, 60, 2);
            assert!(v.check_and_take_failure(base + Duration::from_millis(1600)));
        }
        assert!(
            v.inplace_ineffective,
            "after repeated contradictions, in-place must be marked ineffective"
        );
    }

    #[test]
    fn waits_for_grace_window_before_judging() {
        let t0 = Instant::now();
        let mut v = BitrateVerifier::new(60, t0);
        v.arm_downward(5_000, t0);
        feed(&mut v, 20_000, 60, 1);
        // Before the grace deadline, no verdict yet.
        assert!(!v.check_and_take_failure(t0 + Duration::from_millis(500)));
    }
}
