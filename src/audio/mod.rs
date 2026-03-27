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
use crossbeam_channel::bounded;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

/// Sunshine uses a 30-frame queue between capture and encode.
const SAMPLE_QUEUE_CAPACITY: usize = 30;
/// Encoded packet queue between encode and relay.
const PACKET_QUEUE_CAPACITY: usize = 60;

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
        let (sample_tx, sample_rx) = bounded(SAMPLE_QUEUE_CAPACITY);
        // Channel: encode → relay
        let (packet_tx, packet_rx) = bounded(PACKET_QUEUE_CAPACITY);

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
        let relay_handle = thread::spawn(move || {
            while relay_running.load(Ordering::SeqCst) {
                match packet_rx.recv() {
                    Ok(packet) => broadcaster.broadcast(packet.data),
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
