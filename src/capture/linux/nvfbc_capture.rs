use super::super::{CaptureBackend, CapturedFrame, FrameData};
use super::target_fps;
use crossbeam_channel::Sender;
use nvfbc::{system::CaptureMethod, BufferFormat, SystemCapturer};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

struct CapturerSendWrapper(SystemCapturer);
unsafe impl Send for CapturerSendWrapper {}

pub struct NvfbcCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl NvfbcCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for NvfbcCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Capture already running".into());
        }

        let mut capturer = SystemCapturer::new().map_err(|e| format!("{:?}", e))?;

        let status = capturer.status().map_err(|e| format!("{:?}", e))?;
        if !status.can_create_now {
            return Err("Cannot create NVFBC capture session".into());
        }

        capturer
            .start(BufferFormat::Bgra, target_fps())
            .map_err(|e| format!("{:?}", e))?;

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        // Wrap capturer to allow sending it to another thread.
        let wrapped_capturer = CapturerSendWrapper(capturer);

        let handle = thread::spawn(move || {
            let mut capturer = wrapped_capturer;
            while running.load(Ordering::SeqCst) {
                match capturer
                    .0
                    .next_frame(CaptureMethod::Blocking, Some(Duration::from_millis(50)))
                {
                    Ok(frame_info) => {
                        let frame = CapturedFrame {
                            data: FrameData::Ram(frame_info.buffer.to_vec()),
                            width: frame_info.width,
                            height: frame_info.height,
                            cursor: None, // NvFBC captures cursor natively
                        };
                        if tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        // Timeout or another error. We continue to check `running`.
                        // On normal timeout, it's just no new frame.
                    }
                }
            }
            let _ = capturer.0.stop();
        });

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
