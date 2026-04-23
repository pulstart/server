/// Audio pipeline orchestration.
///
/// Architecture (shared pipeline):
///   Capture thread → sample queue → Encode thread → relay → Broadcaster
///
/// Transport is per-client (managed by main.rs).
///
/// Capture is restartable at runtime: when the user-session tray companion
/// pushes a new `SessionContext` over the control socket, we tear down the
/// current capture thread and start a new one against the user's PulseAudio
/// daemon. Encode + relay stay up across reconfigurations.
pub mod capture;
pub mod encode;

use crate::broadcast::Broadcaster;
use crate::encode_config::AudioConfig;
use crate::session_bridge::{AudioEndpoint, SessionContext};
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
    current_endpoint: Option<AudioEndpoint>,
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
            current_endpoint: None,
        }
    }

    /// Start the shared encode + relay threads. Does NOT start capture —
    /// on Linux the capture thread is started once a `SessionContext`
    /// arrives from the tray (so PulseAudio connects as the logged-in
    /// user). On macOS / Windows / user-mode Linux, call
    /// [`apply_endpoint(None)`] right after and the default auto-detect
    /// path fires.
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

    /// Swap the capture endpoint at runtime. `None` tears down capture
    /// (silence to clients); `Some` restarts against the given
    /// PulseAudio/PipeWire server. Returns `Ok(())` even if the new
    /// connection fails — the pipeline stays alive video-only and the
    /// next reconfigure can try again.
    pub fn apply_endpoint(&mut self, endpoint: Option<AudioEndpoint>) {
        // Stop whatever's running. Safe to call even if idle.
        self.capture.stop();
        self.capture = capture::AudioCapture::new();

        let Some(endpoint) = endpoint else {
            println!("[audio] No session endpoint — capture idle (silent stream)");
            self.current_endpoint = None;
            return;
        };

        let Some(config) = self.config.clone() else {
            eprintln!("[audio] apply_endpoint called before start()");
            return;
        };
        let Some(sample_tx) = self.sample_tx.as_ref().map(Sender::clone) else {
            eprintln!("[audio] apply_endpoint called without active sample_tx");
            return;
        };

        println!(
            "[audio] Starting capture against {} (source: {})",
            endpoint.server,
            endpoint.monitor_source.as_deref().unwrap_or("default"),
        );
        if let Err(err) = self.capture.start(
            config,
            endpoint.monitor_source.clone(),
            Some(endpoint.server.clone()),
            sample_tx,
        ) {
            eprintln!("[audio] Capture start failed: {err}");
            return;
        }
        self.current_endpoint = Some(endpoint);
    }

    /// Apply the audio piece of a full session context. Convenience for
    /// hooking up to `SessionBridge::subscribe`.
    pub fn apply_context(&mut self, ctx: Option<&SessionContext>) {
        let endpoint = ctx.and_then(|c| c.audio.clone());
        self.apply_endpoint(endpoint);
    }

    /// User-mode fallback: rely on the ambient PulseAudio / PipeWire
    /// daemon discoverable in the current process's env (what the server
    /// used to do unconditionally). No-op if called before `start()`.
    pub fn apply_auto_detect(&mut self) {
        let Some(config) = self.config.clone() else {
            eprintln!("[audio] apply_auto_detect called before start()");
            return;
        };
        let Some(sample_tx) = self.sample_tx.as_ref().map(Sender::clone) else {
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
        if let Err(err) = self.capture.start(config, monitor_source, None, sample_tx) {
            eprintln!("[audio] Capture start failed: {err}");
            return;
        }
        // Synthetic endpoint to flag "capture is live" — details don't matter.
        self.current_endpoint = Some(AudioEndpoint {
            kind: crate::session_bridge::AudioKind::Pulse,
            server: "auto".into(),
            monitor_source: None,
            cookie_hex: None,
        });
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
    /// is silent (no session bridged, or the last connect attempt failed).
    #[allow(dead_code)] // surfaced to clients via the control socket later
    pub fn has_active_capture(&self) -> bool {
        self.current_endpoint.is_some()
    }
}
