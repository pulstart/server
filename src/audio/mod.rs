/// Audio pipeline orchestration.
///
/// Architecture (shared pipeline):
///   Capture thread → sample queue → Encode thread → relay → Broadcaster
///
/// Transport is per-client (managed by main.rs).
///
/// Capture runs against whatever PulseAudio / PipeWire daemon is visible in
/// this process's env. Because the server runs as a user systemd unit, that
/// is always the user's own session.
pub mod capture;
pub mod encode;

/// Best-effort elevation of the calling thread's scheduling priority for audio
/// (E3). Audio capture/encode must not be starved by the video encoder under
/// contention, or playback drops out. Per platform: Linux `SCHED_RR` → negative
/// `nice` ladder; macOS QoS class (interactive/user-initiated); Windows thread
/// priority (time-critical/highest). `ST_AUDIO_RT_PRIO=0` (`false`/`no`/`off`)
/// disables. Never panics if the privilege (CAP_SYS_NICE / equivalent) is missing.
pub fn set_realtime_priority(role: &str) {
    if matches!(
        std::env::var("ST_AUDIO_RT_PRIO").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    ) {
        return;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        // Capture is the most latency-critical (it gates everything downstream);
        // encode runs a notch lower so it can't preempt capture.
        let rt_prio = if role == "capture" { 10 } else { 5 };
        let param = libc::sched_param {
            sched_priority: rt_prio,
        };
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_RR, &param) == 0 {
            return;
        }
        // SCHED_RR denied — fall back to a nicer nice value (best-effort).
        let _ = libc::nice(-10);
    }
    #[cfg(target_os = "macos")]
    {
        // macOS has no SCHED_RR for normal processes; the QoS class system is the
        // supported mechanism. Capture rides the interactive class, encode the
        // (slightly lower) user-initiated class. Best-effort; never fails hard.
        let class = if role == "capture" {
            libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE
        } else {
            libc::qos_class_t::QOS_CLASS_USER_INITIATED
        };
        unsafe {
            let _ = libc::pthread_set_qos_class_self_np(class, 0);
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
            THREAD_PRIORITY_TIME_CRITICAL,
        };
        // Capture gets TIME_CRITICAL, encode HIGHEST — mirrors the Linux ladder
        // (capture a notch above encode). Best-effort; ignore failure.
        let prio = if role == "capture" {
            THREAD_PRIORITY_TIME_CRITICAL
        } else {
            THREAD_PRIORITY_HIGHEST
        };
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), prio);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = role;
}

use crate::broadcast::Broadcaster;
use crate::encode_config::AudioConfig;
use crossbeam_channel::{unbounded, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

const DEFAULT_MAX_PACKET_BACKLOG: usize = 2;

pub struct AudioPipeline {
    capture: capture::AudioCapture,
    running: Arc<AtomicBool>,
    encode_handle: Option<thread::JoinHandle<()>>,
    relay_handle: Option<thread::JoinHandle<()>>,
    sample_tx: Option<Sender<capture::AudioSamples>>,
    config: Option<AudioConfig>,
    capture_active: bool,
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            capture: capture::AudioCapture::new(),
            running: Arc::new(AtomicBool::new(false)),
            encode_handle: None,
            relay_handle: None,
            sample_tx: None,
            config: None,
            capture_active: false,
        }
    }

    /// Start the shared encode + relay threads. Does NOT start capture —
    /// capture must be attached via [`apply_auto_detect`].
    pub fn start(
        &mut self,
        config: AudioConfig,
        broadcaster: Arc<Broadcaster<Vec<u8>>>,
    ) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Audio pipeline already running".into());
        }

        self.running.store(true, Ordering::SeqCst);

        // Channel: capture → encode. The sender stays alive as long as the
        // pipeline is running so the encode thread doesn't exit on
        // capture restart.
        let (sample_tx, sample_rx) = unbounded();
        // Channel: encode → relay
        let (packet_tx, packet_rx) = unbounded();

        // 1. Encode thread (drains sample_rx forever).
        let encode_running = Arc::clone(&self.running);
        self.encode_handle = Some(encode::run_encode_thread(
            config.clone(),
            sample_rx,
            packet_tx,
            encode_running,
        ));

        // 2. Relay thread — forwards encoded packets to the broadcaster.
        let relay_running = Arc::clone(&self.running);
        let relay_trace = std::env::var_os("ST_TRACE").is_some();
        let max_packet_backlog = std::env::var("ST_AUDIO_MAX_PACKET_BACKLOG")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .map(|value| value.clamp(1, 8))
            .unwrap_or(DEFAULT_MAX_PACKET_BACKLOG);
        let relay_handle = thread::spawn(move || {
            let mut backlog_logs = 0usize;
            while relay_running.load(Ordering::SeqCst) {
                match packet_rx.recv() {
                    Ok(mut packet) => {
                        let mut dropped_packets = 0usize;
                        while packet_rx.len() > max_packet_backlog {
                            match packet_rx.try_recv() {
                                Ok(newer) => {
                                    packet = newer;
                                    dropped_packets += 1;
                                }
                                Err(_) => break,
                            }
                        }
                        if relay_trace && dropped_packets > 0 && backlog_logs < 12 {
                            eprintln!(
                                "[trace][audio] relay dropped {} stale encoded packet(s)",
                                dropped_packets
                            );
                            backlog_logs += 1;
                        }
                        broadcaster.broadcast(packet.data);
                    }
                    Err(_) => break,
                }
            }
            println!("[audio] Relay thread exited");
        });
        self.relay_handle = Some(relay_handle);

        self.sample_tx = Some(sample_tx);
        self.config = Some(config);

        println!("[audio] Pipeline scaffold started (waiting for capture endpoint)");
        Ok(())
    }

    /// Attach capture against the ambient PulseAudio / PipeWire daemon.
    pub fn apply_auto_detect(&mut self) {
        let Some(config) = self.config.clone() else {
            eprintln!("[audio] apply_auto_detect called before start()");
            return;
        };
        let Some(sample_tx) = self.sample_tx.clone() else {
            eprintln!("[audio] apply_auto_detect called without active sample_tx");
            return;
        };
        self.capture.stop();
        self.capture = capture::AudioCapture::new();

        let monitor_source = capture::detect_monitor_source();
        println!(
            "[audio] Auto-detect using source: {}",
            monitor_source.as_deref().unwrap_or("default")
        );
        if let Err(err) = self.capture.start(config, monitor_source, sample_tx) {
            eprintln!("[audio] Capture start failed: {err}");
            self.capture_active = false;
            return;
        }
        self.capture_active = true;
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.capture.stop();
        self.sample_tx = None; // drops the sender → encode exits
        if let Some(handle) = self.encode_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.relay_handle.take() {
            let _ = handle.join();
        }
        println!("[audio] Pipeline stopped");
    }

    /// Whether audio is currently being captured. `false` means the stream
    /// is silent (last connect attempt failed).
    #[allow(dead_code)]
    pub fn has_active_capture(&self) -> bool {
        self.capture_active
    }
}
