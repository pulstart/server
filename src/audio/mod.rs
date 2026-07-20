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
use crate::transport::EncodedAudioPacket;
use crossbeam_channel::{unbounded, Receiver, RecvError, Sender};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
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
    next_source_seq: Arc<AtomicU64>,
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
            next_source_seq: Arc::new(AtomicU64::new(0)),
            config: None,
            capture_active: false,
        }
    }

    /// Start the shared encode + relay threads. Does NOT start capture —
    /// capture must be attached via [`apply_auto_detect`].
    pub fn start(
        &mut self,
        config: AudioConfig,
        broadcaster: Arc<Broadcaster<EncodedAudioPacket>>,
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
                match recv_with_backlog_limit(&packet_rx, max_packet_backlog) {
                    Ok((packet, dropped_packets)) => {
                        if relay_trace && dropped_packets > 0 && backlog_logs < 12 {
                            eprintln!(
                                "[trace][audio] relay dropped {} stale encoded packet(s)",
                                dropped_packets
                            );
                            backlog_logs += 1;
                        }
                        broadcaster.broadcast(packet);
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
        if let Err(err) = self.capture.start(
            config,
            monitor_source,
            sample_tx,
            Arc::clone(&self.next_source_seq),
        ) {
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

pub(super) fn recv_with_backlog_limit<T>(
    rx: &Receiver<T>,
    max_backlog: usize,
) -> Result<(T, usize), RecvError> {
    let mut item = rx.recv()?;
    let mut dropped = 0;
    while rx.len() > max_backlog {
        match rx.try_recv() {
            Ok(newer) => {
                item = newer;
                dropped += 1;
            }
            Err(_) => break,
        }
    }
    Ok((item, dropped))
}

#[cfg(test)]
mod tests {
    use super::{recv_with_backlog_limit, Broadcaster, EncodedAudioPacket};
    use crossbeam_channel::unbounded;

    fn packet(source_seq: u64) -> EncodedAudioPacket {
        EncodedAudioPacket {
            source_seq,
            data: vec![source_seq as u8],
        }
    }

    #[test]
    fn relay_backlog_drop_preserves_source_gap() {
        let (tx, rx) = unbounded();
        tx.send(packet(10)).unwrap();
        let (first, dropped) = recv_with_backlog_limit(&rx, 1).unwrap();
        assert_eq!(first.source_seq, 10);
        assert_eq!(dropped, 0);

        for seq in 11..=14 {
            tx.send(packet(seq)).unwrap();
        }
        let (next, dropped) = recv_with_backlog_limit(&rx, 1).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(next.source_seq, 13);
        assert_eq!(next.source_seq - first.source_seq, 3);
    }

    #[test]
    fn broadcaster_eviction_preserves_source_gap() {
        let broadcaster = Broadcaster::new();
        let (_, rx) = broadcaster.subscribe(1).unwrap();

        broadcaster.broadcast(packet(20));
        let first = rx.recv().unwrap();
        broadcaster.broadcast(packet(21));
        broadcaster.broadcast(packet(22));
        let next = rx.recv().unwrap();

        assert_eq!(first.source_seq, 20);
        assert_eq!(next.source_seq, 22);
    }
}
