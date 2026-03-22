mod adaptive_bitrate;
#[cfg(target_os = "linux")]
mod audio;
mod broadcast;
mod capture;
mod colorspace;
#[cfg(target_os = "linux")]
mod encode;
mod encode_config;
#[cfg(target_os = "linux")]
mod encode_sw;
#[cfg(target_os = "linux")]
mod encode_vaapi;
#[cfg(target_os = "macos")]
mod encode_vt;
mod input;
mod transport;

use adaptive_bitrate::{AdaptiveBitrateState, ClientRateController};
use broadcast::Broadcaster;
use capture::{CaptureBackend, PlatformCapture};
use encode_config::EncoderConfig;
use input::{CursorVersionCursor, InputRuntime};
use transport::{EncodedVideoFrame, UdpSender};

use crossbeam_channel::{bounded, Receiver, Sender};
use st_protocol::{
    ClientDisplayInfo, ClockSyncPong, ControlMessage, InputSession, SessionDebugInfo, StreamConfig,
};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const UDP_PORT: u16 = 5000;
const VIDEO_SUBSCRIBER_CAPACITY: usize = 120;
static TRACE_ENCODE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
extern "C" {
    fn CVPixelBufferRelease(buf: *mut std::ffi::c_void);
}

/// Result of the pipeline — either it started OK or it had an error.
enum PipelineResult {
    Started(StreamConfig, Arc<AdaptiveBitrateState>, SessionDebugInfo),
    Error(String),
}

/// Encoder wrapper for Linux (VAAPI → NVENC → Software fallback chain).
#[cfg(target_os = "linux")]
enum EncoderKind {
    Vaapi(encode_vaapi::VaapiEncoder),
    Nvenc(encode::NvencEncoder),
    Software(encode_sw::SoftwareEncoder),
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
enum EncoderBackend {
    Vaapi,
    Nvenc,
    Software,
}

#[cfg(target_os = "linux")]
fn create_linux_encoder(config: &EncoderConfig) -> Result<EncoderKind, String> {
    match encode_vaapi::VaapiEncoder::with_config(config) {
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
fn create_linux_encoder_for_backend(
    config: &EncoderConfig,
    backend: EncoderBackend,
) -> Result<EncoderKind, String> {
    match backend {
        EncoderBackend::Vaapi => encode_vaapi::VaapiEncoder::with_config(config)
            .map(EncoderKind::Vaapi)
            .map_err(|err| format!("VAAPI reconfigure failed: {err}")),
        EncoderBackend::Nvenc => encode::NvencEncoder::with_config(config)
            .map(EncoderKind::Nvenc)
            .map_err(|err| format!("NVENC reconfigure failed: {err}")),
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

#[cfg(target_os = "linux")]
fn encoder_name(encoder: &EncoderKind) -> &'static str {
    match encoder {
        EncoderKind::Vaapi(_) => "vaapi",
        EncoderKind::Nvenc(_) => "nvenc",
        EncoderKind::Software(_) => "software",
    }
}

#[cfg(target_os = "linux")]
fn encoder_backend(encoder: &EncoderKind) -> EncoderBackend {
    match encoder {
        EncoderKind::Vaapi(_) => EncoderBackend::Vaapi,
        EncoderKind::Nvenc(_) => EncoderBackend::Nvenc,
        EncoderKind::Software(_) => EncoderBackend::Software,
    }
}

#[cfg(target_os = "linux")]
fn encoder_backend_name(backend: EncoderBackend) -> &'static str {
    match backend {
        EncoderBackend::Vaapi => "vaapi",
        EncoderBackend::Nvenc => "nvenc",
        EncoderBackend::Software => "software",
    }
}

#[cfg(target_os = "linux")]
fn request_next_keyframe(encoder: &mut EncoderKind) {
    match encoder {
        EncoderKind::Vaapi(e) => e.reset_for_keyframe(),
        EncoderKind::Nvenc(e) => e.reset_for_keyframe(),
        EncoderKind::Software(e) => e.reset_for_keyframe(),
    }
}

#[cfg(target_os = "linux")]
fn update_encoder_bitrate(encoder: &mut EncoderKind, config: &EncoderConfig) -> Result<(), String> {
    match encoder {
        EncoderKind::Vaapi(e) => e.update_bitrate(config),
        EncoderKind::Nvenc(e) => e.update_bitrate(config),
        EncoderKind::Software(e) => e.update_bitrate(config),
    }
}

#[cfg(target_os = "linux")]
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
        now.duration_since(last_reconfigure) >= Duration::from_secs(4)
            && delta_kbps >= min_delta
    }
}

#[cfg(target_os = "macos")]
fn encoder_name(_encoder: &encode_vt::VTEncoder) -> &'static str {
    "videotoolbox"
}

// ---------------------------------------------------------------------------
// Shared pipeline: one capture + one encoder + one audio pipeline,
// broadcasting encoded data to all connected clients.
// ---------------------------------------------------------------------------

struct SharedPipeline {
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(target_os = "linux")]
    audio_bc: Arc<Broadcaster<Vec<u8>>>,
    stream_config: StreamConfig,
    session_debug: SessionDebugInfo,
    rate_control: Arc<AdaptiveBitrateState>,
    shutdown_tx: Sender<()>,
    pipeline_handle: std::thread::JoinHandle<()>,
}

impl SharedPipeline {
    fn start(
        client_requested_fps: Option<u32>,
        input: Arc<InputRuntime>,
    ) -> Result<(Self, ClientSubscription), String> {
        let video_bc = Arc::new(Broadcaster::new());
        #[cfg(target_os = "linux")]
        let audio_bc = Arc::new(Broadcaster::new());
        let (vid_sub_id, vid_rx) = video_bc.subscribe(VIDEO_SUBSCRIBER_CAPACITY);
        #[cfg(target_os = "linux")]
        let (aud_sub_id, aud_rx) = audio_bc.subscribe(30);

        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (status_tx, status_rx) = bounded::<PipelineResult>(1);

        let vbc = Arc::clone(&video_bc);
        #[cfg(target_os = "linux")]
        let abc = Arc::clone(&audio_bc);

        let handle = std::thread::spawn(move || {
            run_shared_pipeline(
                shutdown_rx,
                status_tx,
                client_requested_fps,
                input,
                vbc,
                #[cfg(target_os = "linux")]
                abc,
            );
        });

        match status_rx.recv() {
            Ok(PipelineResult::Started(stream_config, rate_control, session_debug)) => Ok((
                Self {
                    video_bc: Arc::clone(&video_bc),
                    #[cfg(target_os = "linux")]
                    audio_bc: Arc::clone(&audio_bc),
                    stream_config,
                    session_debug,
                    rate_control,
                    shutdown_tx,
                    pipeline_handle: handle,
                },
                ClientSubscription {
                    vid_sub_id,
                    vid_rx,
                    video_bc: Arc::clone(&video_bc),
                    #[cfg(target_os = "linux")]
                    aud_sub_id,
                    #[cfg(target_os = "linux")]
                    aud_rx,
                },
            )),
            Ok(PipelineResult::Error(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err("Pipeline thread crashed".into())
            }
        }
    }

    fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.pipeline_handle.join();
    }
}

/// Per-client subscription handles.
struct ClientSubscription {
    vid_sub_id: u64,
    vid_rx: Receiver<Arc<EncodedVideoFrame>>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(target_os = "linux")]
    aud_sub_id: u64,
    #[cfg(target_os = "linux")]
    aud_rx: Receiver<Arc<Vec<u8>>>,
}

/// Global server state shared across all client handlers.
struct ServerState {
    pipeline: Mutex<Option<SharedPipeline>>,
    /// When the last subscriber leaves, the pipeline stop runs in a background
    /// thread. New pipeline starts must wait for this to complete first.
    pending_pipeline_stop: Mutex<Option<std::thread::JoinHandle<()>>>,
    input: Arc<InputRuntime>,
}

#[cfg(target_os = "linux")]
struct PendingEncoderRebuild {
    config: EncoderConfig,
    backend: EncoderBackend,
    rx: Receiver<Result<EncoderKind, String>>,
}

// ---------------------------------------------------------------------------
// Shared pipeline thread
// ---------------------------------------------------------------------------

fn run_shared_pipeline(
    shutdown_rx: Receiver<()>,
    status_tx: Sender<PipelineResult>,
    client_requested_fps: Option<u32>,
    input: Arc<InputRuntime>,
    video_bc: Arc<Broadcaster<EncodedVideoFrame>>,
    #[cfg(target_os = "linux")] audio_bc: Arc<Broadcaster<Vec<u8>>>,
) {
    let (frame_tx, frame_rx) = bounded(1);
    let trace = trace_enabled();

    let negotiated_fps = EncoderConfig::resolve_target_fps(client_requested_fps);
    capture::set_target_fps(negotiated_fps);
    println!(
        "[pipeline] capture fps target={} (client_request={:?}fps, ST_FPS cap={:?})",
        negotiated_fps,
        client_requested_fps,
        EncoderConfig::fps_cap_from_env()
    );

    let mut capture_backend = PlatformCapture::new();
    if let Err(e) = capture_backend.start(frame_tx) {
        let msg = format!("Failed to start capture: {e}");
        eprintln!("{msg}");
        let _ = status_tx.send(PipelineResult::Error(msg));
        return;
    }

    // Get first frame to determine dimensions
    let (first_frame, first_frame_captured_micros) = match frame_rx.recv() {
        Ok(f) => (f, unix_time_micros()),
        Err(_) => {
            let msg = "Capture channel closed before first frame".to_string();
            eprintln!("{msg}");
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error(msg));
            return;
        }
    };
    if trace {
        #[cfg(target_os = "linux")]
        let first_has_cursor = first_frame.cursor.is_some();
        #[cfg(not(target_os = "linux"))]
        let first_has_cursor = false;
        eprintln!(
            "[trace][server] first captured frame: {}x{} cursor={} capture_ts={}",
            first_frame.width,
            first_frame.height,
            first_has_cursor,
            first_frame_captured_micros
        );
    }
    let mut trace_capture_frames = 1usize;

    let config = EncoderConfig::from_env_with_framerate(
        first_frame.width,
        first_frame.height,
        negotiated_fps,
    );
    let rate_control = Arc::new(AdaptiveBitrateState::new(
        config.bitrate_kbps,
        config.min_bitrate_kbps,
        config.max_bitrate_kbps,
    ));
    #[cfg(target_os = "linux")]
    let audio_config = encode_config::AudioConfig::from_env();

    #[cfg(target_os = "macos")]
    let mut encoder = match encode_vt::VTEncoder::new(
        first_frame.width,
        first_frame.height,
        config.bitrate_bps().min(u32::MAX as i64) as u32,
        config.framerate,
    ) {
        Ok(e) => e,
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

    #[cfg(target_os = "linux")]
    let mut encoder = match create_linux_encoder(&config) {
        Ok(encoder) => encoder,
        Err(msg) => {
            eprintln!("{msg}");
            capture_backend.stop();
            let _ = status_tx.send(PipelineResult::Error(msg));
            return;
        }
    };

    // Start audio pipeline (Linux only) — broadcasts to audio_bc
    #[cfg(target_os = "linux")]
    let mut audio_pipeline = {
        let mut ap = audio::AudioPipeline::new();
        match ap.start(audio_config.clone(), audio_bc) {
            Ok(()) => println!("[pipeline] Audio pipeline started"),
            Err(e) => eprintln!("[pipeline] Audio pipeline failed (video-only): {e}"),
        }
        ap
    };

    #[cfg(target_os = "linux")]
    let stream_config = config.to_stream_config(&audio_config);
    #[cfg(target_os = "macos")]
    let stream_config = StreamConfig {
        codec: st_protocol::VideoCodec::H264,
        width: first_frame.width,
        height: first_frame.height,
        framerate: config.framerate.min(u16::MAX as u32) as u16,
        audio_sample_rate: 48_000,
        audio_channels: 2,
        hdr: false,
    };

    let capture_backend_name = capture_backend.backend_name().to_string();
    input.refresh_backend(&capture_backend_name);
    let session_debug = SessionDebugInfo {
        encoder_name: encoder_name(&encoder).to_string(),
        capture_backend: capture_backend_name,
        input_backend: input.backend_label(),
        target_bitrate_kbps: config.bitrate_kbps,
    };

    println!(
        "Shared pipeline started: {}x{} (video: {:?} {:?})",
        first_frame.width, first_frame.height, config.codec, config.dynamic_range,
    );

    // Tell the control plane we started OK
    let _ = status_tx.send(PipelineResult::Started(
        stream_config,
        Arc::clone(&rate_control),
        session_debug,
    ));

    // Encode and broadcast the first frame
    #[cfg(target_os = "linux")]
    let mut current_config = config.clone();
    #[cfg(target_os = "linux")]
    let mut pending_encoder_rebuild: Option<PendingEncoderRebuild> = None;
    #[cfg(target_os = "linux")]
    let mut last_encoder_reconfigure = Instant::now();
    encode_and_broadcast(
        &mut encoder,
        &video_bc,
        input.as_ref(),
        &first_frame,
        first_frame_captured_micros,
    );

    // Main loop
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let (frame, frame_captured_micros) =
            match frame_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(f) => (f, unix_time_micros()),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
        if trace && trace_capture_frames < 8 {
            #[cfg(target_os = "linux")]
            let frame_has_cursor = frame.cursor.is_some();
            #[cfg(not(target_os = "linux"))]
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
            while let Ok(newer) = frame_rx.try_recv() {
                #[cfg(target_os = "macos")]
                unsafe {
                    CVPixelBufferRelease(latest.pixel_buffer_ptr);
                }
                latest = newer;
                latest_captured_micros = unix_time_micros();
            }
            (latest, latest_captured_micros)
        };

        // Only encode when there are subscribers (save GPU/CPU when idle)
        if video_bc.subscriber_count() > 0 {
            // Force IDR when a new subscriber just joined (so it can start decoding)
            #[cfg(target_os = "linux")]
            if video_bc.take_keyframe_request() {
                if trace {
                    eprintln!("[trace][server] taking pending keyframe request");
                }
                request_next_keyframe(&mut encoder);
            }
            #[cfg(target_os = "macos")]
            let _ = video_bc.take_keyframe_request(); // VT encoder always starts with IDR

            #[cfg(target_os = "linux")]
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
                    match result {
                        Ok(mut next_encoder) => {
                            println!(
                                "[abr] {} bitrate {} -> {} kbps",
                                encoder_backend_name(pending.backend),
                                current_config.bitrate_kbps,
                                pending.config.bitrate_kbps
                            );
                            request_next_keyframe(&mut next_encoder);
                            encoder = next_encoder;
                            current_config = pending.config;
                            last_encoder_reconfigure = Instant::now();
                        }
                        Err(err) => {
                            eprintln!(
                                "[abr] {} encoder rebuild failed at {} kbps: {err}",
                                encoder_backend_name(pending.backend),
                                pending.config.bitrate_kbps
                            );
                            rate_control.reset_all_clients(current_config.bitrate_kbps);
                        }
                    }
                }

                let target_bitrate = rate_control.current_target_kbps();
                if pending_encoder_rebuild.is_none()
                    && should_schedule_bitrate_reconfigure(
                        current_config.bitrate_kbps,
                        target_bitrate,
                        last_encoder_reconfigure,
                    )
                {
                    let next_config = current_config.with_bitrate_kbps(target_bitrate);
                    let backend = encoder_backend(&encoder);
                    match update_encoder_bitrate(&mut encoder, &next_config) {
                        Ok(()) => {
                            println!(
                                "[abr] {} bitrate {} -> {} kbps (in-place)",
                                encoder_backend_name(backend),
                                current_config.bitrate_kbps,
                                next_config.bitrate_kbps
                            );
                            current_config = next_config;
                            last_encoder_reconfigure = Instant::now();
                        }
                        Err(err) => {
                            if trace {
                                eprintln!(
                                    "[trace][server] {} in-place bitrate update failed: {err}",
                                    encoder_backend_name(backend)
                                );
                            }
                            let (rebuild_tx, rebuild_rx) = bounded(1);
                            let rebuild_config = next_config.clone();
                            std::thread::spawn(move || {
                                let result =
                                    create_linux_encoder_for_backend(&rebuild_config, backend);
                                let _ = rebuild_tx.send(result);
                            });
                            if trace {
                                eprintln!(
                                    "[trace][server] scheduling {} bitrate rebuild {} -> {} kbps",
                                    encoder_backend_name(backend),
                                    current_config.bitrate_kbps,
                                    next_config.bitrate_kbps
                                );
                            }
                            pending_encoder_rebuild = Some(PendingEncoderRebuild {
                                config: next_config,
                                backend,
                                rx: rebuild_rx,
                            });
                        }
                    }
                }
            }
            encode_and_broadcast(
                &mut encoder,
                &video_bc,
                input.as_ref(),
                &frame,
                frame_captured_micros,
            );
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
    #[cfg(target_os = "linux")]
    match &mut encoder {
        EncoderKind::Vaapi(e) => {
            e.flush();
        }
        EncoderKind::Nvenc(e) => {
            e.flush();
        }
        EncoderKind::Software(e) => {
            e.flush();
        }
    }
    #[cfg(target_os = "linux")]
    audio_pipeline.stop();
    capture_backend.stop();
    input.clear_for_stop();
    println!("Shared pipeline stopped");
}

// ---------------------------------------------------------------------------
// Encode + broadcast (replaces encode_and_send)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn encode_and_broadcast(
    encoder: &mut encode_vt::VTEncoder,
    broadcaster: &Broadcaster<EncodedVideoFrame>,
    _input: &InputRuntime,
    frame: &capture::CapturedFrame,
    captured_micros: u64,
) {
    if let Err(e) = encoder.encode_pixel_buffer(frame.pixel_buffer_ptr) {
        eprintln!("encode error: {e}");
    }
    unsafe {
        CVPixelBufferRelease(frame.pixel_buffer_ptr);
    }

    for nal in encoder.receive_nals() {
        broadcaster.broadcast(EncodedVideoFrame {
            data: nal,
            capture_micros: captured_micros,
        });
    }
}

#[cfg(target_os = "linux")]
fn encode_and_broadcast(
    encoder: &mut EncoderKind,
    broadcaster: &Broadcaster<EncodedVideoFrame>,
    input: &InputRuntime,
    frame: &capture::CapturedFrame,
    captured_micros: u64,
) {
    input.update_cursor(frame.cursor.as_ref());

    // Composite cursor onto RAM frames before encoding when no controller owns input.
    // During active control, the cursor is sent separately to the client and kept
    // out of the encoded frame.
    let frame_with_cursor;
    let frame_ref = if !input.control_active() {
        if let Some(cursor) = &frame.cursor {
            if let capture::FrameData::Ram(ref data) = frame.data {
                let mut composited = data.clone();
                capture::linux::x11_capture::composite_cursor(
                    &mut composited,
                    frame.width,
                    frame.height,
                    cursor,
                );
                frame_with_cursor = capture::CapturedFrame {
                    data: capture::FrameData::Ram(composited),
                    width: frame.width,
                    height: frame.height,
                    cursor: None,
                };
                &frame_with_cursor
            } else {
                frame
            }
        } else {
            frame
        }
    } else {
        frame
    };

    let result = match encoder {
        EncoderKind::Vaapi(e) => e.encode(frame_ref),
        EncoderKind::Nvenc(e) => e.encode(frame_ref),
        EncoderKind::Software(e) => e.encode(frame_ref),
    };
    match result {
        Ok(nals) => {
            if trace_enabled() {
                let log_index = TRACE_ENCODE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if log_index < 12 {
                    let total_bytes: usize = nals.iter().map(|nal| nal.len()).sum();
                    eprintln!(
                        "[trace][server] encoder produced {} unit(s), total={} bytes, capture_ts={captured_micros}",
                        nals.len(),
                        total_bytes
                    );
                }
            }
            for nal in nals {
                broadcaster.broadcast(EncodedVideoFrame {
                    data: nal,
                    capture_micros: captured_micros,
                });
            }
        }
        Err(e) => eprintln!("encode error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Per-client transport
// ---------------------------------------------------------------------------

/// Per-client unified transport: sends both video and audio on a single UDP socket.
fn run_transport(
    addr: SocketAddr,
    vid_rx: Receiver<Arc<EncodedVideoFrame>>,
    aud_rx: Option<Receiver<Arc<Vec<u8>>>>,
    audio_enabled: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    let mut sender = match UdpSender::new(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[transport] Failed to create UDP sender for {addr}: {e}");
            return;
        }
    };
    let trace = trace_enabled();
    let mut sent_video_units = 0usize;
    let mut last_video_activity = std::time::Instant::now();

    // Do not drain the per-subscriber queue here. It starts empty at subscribe
    // time, and the earliest queued frame is typically the fresh IDR requested
    // for that subscriber during handshake. Dropping it can leave the client
    // starting from an undecodable P-frame.

    while running.load(Ordering::SeqCst) {
        // Video: blocking recv with short timeout
        match vid_rx.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(frame) => {
                if trace && sent_video_units < 12 {
                    eprintln!(
                        "[trace][server] transport send video unit #{sent_video_units} to {addr}: bytes={} capture_ts={}",
                        frame.data.len(),
                        frame.capture_micros
                    );
                }
                sent_video_units = sent_video_units.saturating_add(1);
                last_video_activity = std::time::Instant::now();
                if let Err(e) = sender.send_frame(&frame, unix_time_micros()) {
                    eprintln!("[transport] video send error to {addr}: {e}");
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
                    if let Err(e) = sender.send_audio(&opus) {
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

/// Unsubscribe from broadcasters and stop the pipeline (in a background thread)
/// if no subscribers remain.
fn unsubscribe_and_maybe_stop_pipeline(
    state: &Arc<ServerState>,
    vid_sub_id: u64,
    #[cfg(target_os = "linux")] aud_sub_id: u64,
) {
    let pipeline_to_stop = {
        let mut pipeline = state.pipeline.lock().unwrap();
        let should_stop = if let Some(p) = pipeline.as_ref() {
            p.video_bc.unsubscribe(vid_sub_id);
            #[cfg(target_os = "linux")]
            p.audio_bc.unsubscribe(aud_sub_id);
            p.video_bc.subscriber_count() == 0
        } else {
            false
        };
        if should_stop {
            pipeline.take()
        } else {
            None
        }
    };
    if let Some(pipeline) = pipeline_to_stop {
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
        .unwrap_or(UDP_PORT)
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

async fn handle_client(
    mut stream: tokio::net::TcpStream,
    addr: SocketAddr,
    state: Arc<ServerState>,
) {
    println!("Client connected: {addr}");
    let client_id = state.input.allocate_client_id();

    let startup_prefs = match read_client_startup_prefs(&mut stream).await {
        Ok(prefs) => prefs,
        Err(err) => {
            eprintln!("Failed to read startup preferences from {addr}: {err}");
            return;
        }
    };
    let client_requested_fps = client_display_fps_hint(startup_prefs.display);
    if let Some(display) = startup_prefs.display {
        println!(
            "[client {addr}] display refresh hint: {:.3} Hz, media udp port: {}",
            display.max_refresh_millihz as f32 / 1000.0,
            client_media_port(Some(display))
        );
    }

    // Ensure shared pipeline is running and subscribe (blocking work)
    let state2 = Arc::clone(&state);
    let requested_fps_for_setup = client_requested_fps;
    let setup = tokio::task::spawn_blocking(
        move || -> Result<
            (
                ClientSubscription,
                StreamConfig,
                Arc<AdaptiveBitrateState>,
                SessionDebugInfo,
            ),
            String,
        > {
            // Wait for any previous pipeline stop to finish before starting a new one.
            // Without this, the new capture backend may fail because the old one still
            // holds exclusive resources (PipeWire portal session, KMS, etc.).
            if let Some(handle) = state2.pending_pipeline_stop.lock().unwrap().take() {
                println!("[pipeline] Waiting for previous pipeline to finish stopping...");
                let _ = handle.join();
                println!("[pipeline] Previous pipeline stopped.");
            }

            let mut pipeline = state2.pipeline.lock().unwrap();
            // Remove dead pipeline (capture died, portal closed, etc.)
            if let Some(p) = pipeline.as_ref() {
                if p.pipeline_handle.is_finished() {
                    println!("[pipeline] Pipeline thread died, will restart...");
                    let p = pipeline.take().unwrap();
                    p.stop();
                }
            }
            if pipeline.is_none() {
                println!("[pipeline] Starting shared pipeline...");
                let (started, sub) = SharedPipeline::start(
                    requested_fps_for_setup,
                    Arc::clone(&state2.input),
                )?;
                let stream_config = started.stream_config;
                let rate_control = Arc::clone(&started.rate_control);
                let session_debug = started.session_debug.clone();
                *pipeline = Some(started);
                return Ok((sub, stream_config, rate_control, session_debug));
            }
            let p = pipeline.as_ref().unwrap();
            let (vid_id, vid_rx) = p.video_bc.subscribe(VIDEO_SUBSCRIBER_CAPACITY);
            #[cfg(target_os = "linux")]
            let (aud_id, aud_rx) = p.audio_bc.subscribe(30);
            Ok((
                ClientSubscription {
                    vid_sub_id: vid_id,
                    vid_rx,
                    video_bc: Arc::clone(&p.video_bc),
                    #[cfg(target_os = "linux")]
                    aud_sub_id: aud_id,
                    #[cfg(target_os = "linux")]
                    aud_rx,
                },
                p.stream_config,
                Arc::clone(&p.rate_control),
                p.session_debug.clone(),
            ))
        },
    )
    .await
    .unwrap();

    let (sub, stream_config, rate_control, session_debug) = match setup {
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
    rate_control.register_client(sub.vid_sub_id);
    let mut bitrate_controller = ClientRateController::from_state(rate_control.as_ref());
    let controller_state = state.input.controller_state_for(client_id);
    if let Some(requested_fps) = client_requested_fps {
        if stream_config.framerate as u32 != requested_fps {
            println!(
                "[client {addr}] negotiated {} fps (requested {} fps)",
                stream_config.framerate, requested_fps
            );
        }
    }

    // Send stream/session metadata first. The client will bind UDP, start its
    // receive path, and acknowledge readiness before we start transport.
    let mut control_buf = ControlMessage::StreamConfig(stream_config).serialize();
    control_buf
        .extend_from_slice(&ControlMessage::SessionDebugInfo(session_debug.clone()).serialize());
    control_buf
        .extend_from_slice(&ControlMessage::InputSession(InputSession { client_id }).serialize());
    control_buf.extend_from_slice(
        &ControlMessage::InputCapabilities(state.input.capabilities()).serialize(),
    );
    control_buf.extend_from_slice(&ControlMessage::ControllerState(controller_state).serialize());
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
            unsubscribe_and_maybe_stop_pipeline(&state, sub.vid_sub_id, #[cfg(target_os = "linux")] sub.aud_sub_id);
            return;
        }
        Err(err) => {
            eprintln!("[client {addr}] failed waiting for media-ready ack: {err}");
            rate_control.unregister_client(sub.vid_sub_id);
            let _ = state.input.release_control(client_id);
            unsubscribe_and_maybe_stop_pipeline(&state, sub.vid_sub_id, #[cfg(target_os = "linux")] sub.aud_sub_id);
            return;
        }
    }

    // Per-client audio enable flag (toggled by client via SetAudio control message)
    let audio_enabled = Arc::new(AtomicBool::new(true));

    // Start per-client unified transport (video + audio on single UDP socket)
    let transport_running = Arc::new(AtomicBool::new(true));

    let transport_addr = SocketAddr::new(addr.ip(), client_media_port(startup_prefs.display));
    sub.video_bc.request_keyframe();
    let vid_rx = sub.vid_rx;
    #[cfg(target_os = "linux")]
    let aud_rx = Some(sub.aud_rx);
    #[cfg(not(target_os = "linux"))]
    let aud_rx: Option<Receiver<Arc<Vec<u8>>>> = None;
    let transport_running_clone = Arc::clone(&transport_running);
    let audio_enabled_transport = Arc::clone(&audio_enabled);
    let transport_handle = std::thread::spawn(move || {
        run_transport(
            transport_addr,
            vid_rx,
            aud_rx,
            audio_enabled_transport,
            transport_running_clone,
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

    // Hold TCP open — read control messages from client
    let mut buf = [0u8; 64];
    let mut cursor_versions = CursorVersionCursor::default();
    loop {
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
                            let next_kbps = bitrate_controller.apply_feedback(feedback);
                            rate_control.update_client_target(sub.vid_sub_id, next_kbps);
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
                            let state_msg = ControlMessage::ControllerState(
                                state.input.acquire_control(client_id),
                            );
                            cursor_versions = CursorVersionCursor::default();
                            let _ = stream.write_all(&state_msg.serialize()).await;
                        }
                        ControlMessage::ReleaseControl => {
                            let state_msg = ControlMessage::ControllerState(
                                state.input.release_control(client_id),
                            );
                            let _ = stream.write_all(&state_msg.serialize()).await;
                        }
                        ControlMessage::RequestKeyframe => {
                            sub.video_bc.request_keyframe();
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
            Err(_) => {}
        }
    }

    println!("Client {addr} disconnected.");
    transport_running.store(false, Ordering::SeqCst);
    rate_control.unregister_client(sub.vid_sub_id);
    let _ = state.input.release_control(client_id);

    unsubscribe_and_maybe_stop_pipeline(&state, sub.vid_sub_id, #[cfg(target_os = "linux")] sub.aud_sub_id);

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

    println!("[probe] Capture backends: X11={x11_ok}, wlr-screencopy={wlr_ok}, PipeWire(portal)={pipewire_ok}");

    let capture_backend = match ds.as_str() {
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
            if x11_ok {
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
    };
    println!("[probe] Selected capture: {capture_backend}");

    let (width, height) = get_screen_resolution().unwrap_or((1920, 1080));
    println!("[probe] Screen resolution: {width}x{height}");

    let config = EncoderConfig::from_env(width, height);
    println!(
        "[probe] Config: {:?} {:?} {}kbps {}fps",
        config.codec, config.dynamic_range, config.bitrate_kbps, config.framerate
    );

    match encode_vaapi::VaapiEncoder::with_config(&config) {
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

#[tokio::main]
async fn main() {
    #[cfg(target_os = "linux")]
    probe_backends();

    let input = InputRuntime::new();
    input.spawn_listener(UDP_PORT);
    let state = Arc::new(ServerState {
        pipeline: Mutex::new(None),
        pending_pipeline_stop: Mutex::new(None),
        input,
    });

    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind TCP listener on 0.0.0.0:8080");

    println!("Server started. Waiting for clients on 0.0.0.0:8080...");
    println!(
        "  Overrides: ST_CODEC, ST_HDR, ST_BITRATE, ST_MIN_BITRATE, ST_MAX_BITRATE, ST_FPS, ST_GOP, ST_AUDIO, ST_CAPTURE"
    );

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let state = Arc::clone(&state);
                tokio::spawn(handle_client(stream, addr, state));
            }
            Err(e) => eprintln!("Accept error: {e}"),
        }
    }
}
