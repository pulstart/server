use super::{target_fps, CaptureBackend, CapturedFrame};
use crossbeam_channel::Sender;
use screencapturekit::prelude::*;

extern "C" {
    fn CVPixelBufferRetain(buf: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

struct OutputHandler {
    tx: Sender<CapturedFrame>,
}

impl SCStreamOutputTrait for OutputHandler {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }

        let pixel_buffer = match sample_buffer.image_buffer() {
            Some(pb) => pb,
            None => return,
        };

        let width = pixel_buffer.width() as u32;
        let height = pixel_buffer.height() as u32;
        let ptr = pixel_buffer.as_ptr();

        // Retain the CVPixelBuffer so it outlives this callback.
        // The pipeline will release it after encoding.
        unsafe {
            CVPixelBufferRetain(ptr);
        }

        let _ = self.tx.try_send(CapturedFrame {
            pixel_buffer_ptr: ptr,
            width,
            height,
        });
    }
}

pub struct PlatformCapture {
    stream: Option<SCStream>,
}

impl PlatformCapture {
    pub fn new() -> Self {
        Self { stream: None }
    }

    pub fn backend_name(&self) -> &'static str {
        "screencapturekit"
    }
}

impl CaptureBackend for PlatformCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        let content = SCShareableContent::get()
            .map_err(|e| format!("Failed to get shareable content: {e:?}"))?;
        let displays = content.displays();
        let display = displays.first().ok_or("No displays found")?;

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(1920)
            .with_height(1080)
            .with_pixel_format(screencapturekit::prelude::PixelFormat::BGRA)
            .with_shows_cursor(false)
            .with_minimum_frame_interval(&CMTime::new(1, target_fps() as i32));

        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(|e| eprintln!("capture: stream error: {e:?}")),
        );
        stream.add_output_handler(OutputHandler { tx }, SCStreamOutputType::Screen);

        stream
            .start_capture()
            .map_err(|e| format!("Failed to start capture: {e:?}"))?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.stop_capture();
        }
    }
}
