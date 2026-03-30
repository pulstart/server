use super::{target_fps, CaptureBackend, CapturedCursor, CapturedFrame};
use crate::macos_display::{describe_display, select_capture_display};
use crossbeam_channel::{Sender, TrySendError};
use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSCursor};
use screencapturekit::prelude::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

extern "C" {
    fn CVPixelBufferRetain(buf: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CVPixelBufferRelease(buf: *mut std::ffi::c_void);
    fn CGEventCreate(source: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> NativePoint;
    fn CFRelease(ptr: *const std::ffi::c_void);
}

const MAX_CURSOR_CAPTURE_ERRORS: usize = 12;

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct NativePoint {
    x: f64,
    y: f64,
}

struct OutputHandler {
    tx: Sender<CapturedFrame>,
    cursor_tracker: Mutex<CursorTracker>,
    cursor_errors: AtomicUsize,
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

        let cursor = self.cursor_tracker.lock().ok().and_then(|mut tracker| {
            match tracker.capture_cursor() {
                Ok(cursor) => cursor,
                Err(err) => {
                    let log_idx = self.cursor_errors.fetch_add(1, Ordering::Relaxed);
                    if log_idx < MAX_CURSOR_CAPTURE_ERRORS {
                        eprintln!("[capture] macOS cursor capture failed: {err}");
                    }
                    None
                }
            }
        });

        match self.tx.try_send(CapturedFrame {
            pixel_buffer_ptr: ptr,
            width,
            height,
            cursor,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(frame)) | Err(TrySendError::Disconnected(frame)) => unsafe {
                CVPixelBufferRelease(frame.pixel_buffer_ptr);
            },
        }
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
        let display = select_capture_display()?;
        let width = display.width().max(1);
        let height = display.height().max(1);

        println!(
            "[capture] macOS ScreenCaptureKit using {}",
            describe_display(&display)
        );

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(screencapturekit::prelude::PixelFormat::BGRA)
            .with_shows_cursor(false)
            .with_minimum_frame_interval(&CMTime::new(1, target_fps() as i32));

        let mut stream = SCStream::new_with_delegate(
            &filter,
            &config,
            ErrorHandler::new(|e| eprintln!("capture: stream error: {e:?}")),
        );
        stream.add_output_handler(
            OutputHandler {
                tx,
                cursor_tracker: Mutex::new(CursorTracker::new(display.frame())),
                cursor_errors: AtomicUsize::new(0),
            },
            SCStreamOutputType::Screen,
        );

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

struct CursorTracker {
    display_frame: screencapturekit::cg::CGRect,
    cached_shape: Option<CapturedCursor>,
}

impl CursorTracker {
    fn new(display_frame: screencapturekit::cg::CGRect) -> Self {
        Self {
            display_frame,
            cached_shape: None,
        }
    }

    fn capture_cursor(&mut self) -> Result<Option<CapturedCursor>, String> {
        let location =
            current_cursor_location().ok_or_else(|| "CGEventGetLocation returned null".to_string())?;
        let cursor = current_system_cursor();
        let serial = cursor
            .as_ref()
            .map(|cursor| cursor_shape_serial(cursor))
            .or_else(|| self.cached_shape.as_ref().map(|shape| shape.shape_serial))
            .unwrap_or(0);
        let cached_hotspot = self
            .cached_shape
            .as_ref()
            .map(|shape| (shape.hotspot_x, shape.hotspot_y))
            .unwrap_or((0, 0));
        let visible = point_in_rect(location, self.display_frame);
        if !visible {
            return Ok(Some(hidden_cursor_state(
                location,
                self.display_frame,
                cached_hotspot,
                serial,
            )));
        }

        if let Some(cursor) = cursor.as_ref() {
            let needs_refresh = self
                .cached_shape
                .as_ref()
                .map(|shape| shape.shape_serial != serial)
                .unwrap_or(true);
            if needs_refresh {
                self.cached_shape = Some(load_cursor_shape(cursor)?);
            }
        }

        let mut captured = self
            .cached_shape
            .clone()
            .ok_or_else(|| "cursor shape unavailable".to_string())?;
        captured.shape_serial = serial;
        captured.x = (location.x - self.display_frame.x - captured.hotspot_x as f64).round() as i32;
        captured.y = (location.y - self.display_frame.y - captured.hotspot_y as f64).round() as i32;
        captured.visible = true;
        Ok(Some(captured))
    }
}

fn hidden_cursor_state(
    location: NativePoint,
    display_frame: screencapturekit::cg::CGRect,
    hotspot: (u32, u32),
    serial: u64,
) -> CapturedCursor {
    CapturedCursor {
        pixels: Vec::new(),
        x: (location.x - display_frame.x - hotspot.0 as f64).round() as i32,
        y: (location.y - display_frame.y - hotspot.1 as f64).round() as i32,
        hotspot_x: hotspot.0,
        hotspot_y: hotspot.1,
        width: 0,
        height: 0,
        shape_serial: serial,
        visible: false,
    }
}

fn point_in_rect(point: NativePoint, rect: screencapturekit::cg::CGRect) -> bool {
    point.x >= rect.x
        && point.y >= rect.y
        && point.x < rect.x + rect.width
        && point.y < rect.y + rect.height
}

fn current_cursor_location() -> Option<NativePoint> {
    let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
    if event.is_null() {
        return None;
    }
    let point = unsafe { CGEventGetLocation(event) };
    unsafe {
        CFRelease(event);
    }
    Some(point)
}

#[allow(deprecated)]
fn current_system_cursor() -> Option<objc2::rc::Retained<NSCursor>> {
    NSCursor::currentSystemCursor().or_else(|| Some(NSCursor::currentCursor()))
}

fn cursor_shape_serial(cursor: &NSCursor) -> u64 {
    cursor as *const NSCursor as usize as u64
}

fn load_cursor_shape(cursor: &NSCursor) -> Result<CapturedCursor, String> {
    let image = cursor.image();
    let hotspot = cursor.hotSpot();
    let bitmap = image
        .TIFFRepresentation()
        .and_then(|data| NSBitmapImageRep::imageRepWithData(&data))
        .ok_or_else(|| "failed to materialize cursor image".to_string())?;

    if bitmap.isPlanar() {
        return Err("planar cursor bitmaps are unsupported".into());
    }

    let width = bitmap.pixelsWide();
    let height = bitmap.pixelsHigh();
    let samples_per_pixel = bitmap.samplesPerPixel();
    let bits_per_sample = bitmap.bitsPerSample();
    let bytes_per_row = bitmap.bytesPerRow();
    if width <= 0 || height <= 0 {
        return Err("cursor bitmap has invalid dimensions".into());
    }
    if bits_per_sample != 8 {
        return Err(format!(
            "cursor bitmap uses unsupported {}-bit channels",
            bits_per_sample
        ));
    }
    if samples_per_pixel != 3 && samples_per_pixel != 4 {
        return Err(format!(
            "cursor bitmap uses unsupported {} samples per pixel",
            samples_per_pixel
        ));
    }

    let src_ptr = bitmap.bitmapData();
    if src_ptr.is_null() {
        return Err("cursor bitmap has no pixel data".into());
    }

    let pixels = convert_bitmap_to_bgra(
        unsafe { std::slice::from_raw_parts(src_ptr as *const u8, bytes_per_row as usize * height as usize) },
        width as usize,
        height as usize,
        bytes_per_row as usize,
        samples_per_pixel as usize,
        bitmap.bitmapFormat(),
    );

    Ok(CapturedCursor {
        pixels,
        x: 0,
        y: 0,
        hotspot_x: clamp_hotspot(hotspot.x, width as u32),
        hotspot_y: clamp_hotspot(hotspot.y, height as u32),
        width: width as u32,
        height: height as u32,
        shape_serial: cursor_shape_serial(cursor),
        visible: true,
    })
}

fn convert_bitmap_to_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
    samples_per_pixel: usize,
    bitmap_format: NSBitmapFormat,
) -> Vec<u8> {
    let alpha_first = bitmap_format.contains(NSBitmapFormat::AlphaFirst);
    let alpha_nonpremultiplied = bitmap_format.contains(NSBitmapFormat::AlphaNonpremultiplied);
    let little_endian = bitmap_format.contains(NSBitmapFormat::ThirtyTwoBitLittleEndian);
    let mut out = vec![0u8; width * height * 4];

    for y in 0..height {
        let src_row = &src[y * bytes_per_row..(y + 1) * bytes_per_row];
        for x in 0..width {
            let src_offset = x * samples_per_pixel;
            let dst_offset = (y * width + x) * 4;
            if src_offset + samples_per_pixel > src_row.len() {
                continue;
            }

            let pixel = &src_row[src_offset..src_offset + samples_per_pixel];
            let (mut b, mut g, mut r, a) = match samples_per_pixel {
                4 if alpha_first && little_endian => (pixel[0], pixel[1], pixel[2], pixel[3]),
                4 if alpha_first => (pixel[3], pixel[2], pixel[1], pixel[0]),
                4 if little_endian => (pixel[1], pixel[2], pixel[3], pixel[0]),
                4 => (pixel[2], pixel[1], pixel[0], pixel[3]),
                3 => (pixel[2], pixel[1], pixel[0], 255),
                _ => unreachable!(),
            };

            if alpha_nonpremultiplied && a < 255 {
                let alpha = a as u16;
                b = ((b as u16 * alpha) / 255) as u8;
                g = ((g as u16 * alpha) / 255) as u8;
                r = ((r as u16 * alpha) / 255) as u8;
            }

            out[dst_offset] = b;
            out[dst_offset + 1] = g;
            out[dst_offset + 2] = r;
            out[dst_offset + 3] = a;
        }
    }

    out
}

fn clamp_hotspot(value: f64, limit: u32) -> u32 {
    if limit == 0 {
        return 0;
    }
    value
        .round()
        .clamp(0.0, (limit.saturating_sub(1)) as f64) as u32
}
