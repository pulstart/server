//! Test-pattern outputs with no display behind them: `ST_CAPTURE=synthetic`
//! streams `ST_SYNTHETIC_OUTPUTS` (default 2) fake monitors, so everything
//! above capture — one pipeline per output, live output switches, aux
//! sessions — can be exercised end to end without a compositor or the
//! capability KMS capture needs. `ST_SYNTHETIC_NOISE=<percent>` refreshes that
//! share of every frame's rows with noise, so the encoder spends its whole
//! bitrate like a game does (a full frame of noise cannot be squeezed under
//! ~7 Mbit/s at 720p; 10% follows any target).

use super::super::{
    CaptureBackend, CaptureControl, CaptureOutput, CapturedFrame, FrameData, RamPool,
};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

pub fn enabled() -> bool {
    std::env::var("ST_CAPTURE").is_ok_and(|value| value.eq_ignore_ascii_case("synthetic"))
}

/// Alternating landscape and portrait outputs, so a switch between
/// neighbours always changes the stream size.
pub fn list_outputs() -> Vec<CaptureOutput> {
    let count = std::env::var("ST_SYNTHETIC_OUTPUTS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(2)
        .clamp(1, 8);
    (1..=count)
        .map(|index| {
            let name = format!("SYN-{index}");
            let (width, height) = if index % 2 == 1 {
                (1280, 720)
            } else {
                (720, 1280)
            };
            CaptureOutput {
                id: crate::outputs::output_id(&name),
                name,
                width,
                height,
                refresh_millihz: 60_000,
                plane_rotated: false,
            }
        })
        .collect()
}

pub struct SyntheticCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    output: Option<u32>,
}

impl SyntheticCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            output: None,
        }
    }
}

impl CaptureBackend for SyntheticCapture {
    fn start(
        &mut self,
        tx: Sender<CapturedFrame>,
        control: &Arc<CaptureControl>,
    ) -> Result<(), String> {
        let outputs = list_outputs();
        let (index, output) = match self.output {
            Some(id) => outputs
                .iter()
                .enumerate()
                .find(|(_, output)| output.id == id)
                .ok_or_else(|| format!("output {id:#x} is not connected"))?,
            None => (0, &outputs[0]),
        };
        let (width, height) = (output.width, output.height);
        // Each output has its own flat colour; the bar shows it is live.
        let colour = [
            [0x60, 0x30, 0x10, 0xff],
            [0x10, 0x60, 0x30, 0xff],
            [0x30, 0x10, 0x60, 0xff],
        ][index % 3];
        let noise_rows = std::env::var("ST_SYNTHETIC_NOISE")
            .ok()
            .and_then(|percent| percent.parse::<usize>().ok())
            .map_or(0, |percent| height as usize * percent.min(100) / 100);
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let control = Arc::clone(control);
        self.handle = Some(thread::spawn(move || {
            let pool = RamPool::default();
            let row = width as usize * 4;
            let mut tick = 0usize;
            let mut seed = 0x9e37_79b9_7f4a_7c15u64;
            while running.load(Ordering::SeqCst) {
                let started = Instant::now();
                let mut data = pool.take(row * height as usize);
                let bar = (tick * 8) % width as usize;
                let (noisy, flat) = data.split_at_mut(noise_rows * row);
                for pixels in noisy.chunks_exact_mut(8) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    pixels.copy_from_slice(&seed.to_le_bytes());
                }
                for line in flat.chunks_exact_mut(row) {
                    for (x, pixel) in line.chunks_exact_mut(4).enumerate() {
                        let lit = x.abs_diff(bar) < 16;
                        pixel.copy_from_slice(if lit { &[0xff; 4] } else { &colour });
                    }
                }
                tick += 1;
                let frame = CapturedFrame {
                    data: FrameData::Ram(data),
                    width,
                    height,
                    cursor: None,
                    force_keyframe: false,
                    captured_at: started,
                };
                if let Err(TrySendError::Disconnected(_)) = tx.try_send(frame) {
                    break;
                }
                thread::sleep(control.frame_interval().saturating_sub(started.elapsed()));
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn list_outputs(&self) -> Vec<CaptureOutput> {
        list_outputs()
    }

    fn select_output(&mut self, id: u32) {
        self.output = Some(id);
    }
}
