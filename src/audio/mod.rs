/// Audio pipeline orchestration, matching Sunshine's `audio.cpp`.
///
/// Architecture (shared pipeline):
///   Capture thread → sample queue → Encode thread → relay → Broadcaster
///
/// Transport is per-client (managed by main.rs).
pub mod capture;
pub mod encode;

use crate::broadcast::Broadcaster;
use crate::encode_config::AudioConfig;
use crossbeam_channel::unbounded;
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
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            capture: capture::AudioCapture::new(),
            running: Arc::new(AtomicBool::new(false)),
            encode_handle: None,
            relay_handle: None,
        }
    }

    /// Start the shared audio pipeline: capture → encode → broadcast.
    /// Transport is per-client and managed externally.
    pub fn start(
        &mut self,
        config: AudioConfig,
        broadcaster: Arc<Broadcaster<Vec<u8>>>,
    ) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Audio pipeline already running".into());
        }

        self.running.store(true, Ordering::SeqCst);

        // Channel: capture → encode
        let (sample_tx, sample_rx) = unbounded();
        // Channel: encode → relay
        let (packet_tx, packet_rx) = unbounded();

        // Detect PulseAudio monitor source
        let monitor_source = capture::detect_monitor_source();
        println!(
            "[audio] Using source: {}",
            monitor_source.as_deref().unwrap_or("default")
        );

        // 1. Start capture thread
        self.capture
            .start(config.clone(), monitor_source, sample_tx)?;

        // 2. Start encode thread (outputs to packet_tx)
        let encode_running = Arc::clone(&self.running);
        self.encode_handle = Some(encode::run_encode_thread(config, sample_rx, packet_tx, encode_running));

        // 3. Relay: forward encoded packets to broadcaster
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

        println!("[audio] Pipeline started (shared)");
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.capture.stop();
        if let Some(handle) = self.encode_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.relay_handle.take() {
            let _ = handle.join();
        }
        println!("[audio] Pipeline stopped");
    }
}
