use crossbeam_channel::Sender;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};

static TARGET_FPS: AtomicU32 = AtomicU32::new(60);

/// A single plane of a DMA-BUF (GPU-accessible buffer exported via DRM).
#[cfg(target_os = "linux")]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

/// Frame payload: either CPU-accessible bytes or GPU DMA-BUF planes.
pub enum FrameData {
    Ram(Vec<u8>),
    #[cfg(target_os = "linux")]
    DmaBuf {
        planes: Vec<DmaBufPlane>,
        drm_format: u32,
    },
}

/// Cursor image captured alongside the main frame.
///
/// Used by backends that can expose a separate cursor plane or metadata.
/// Today that includes KMS, X11, PipeWire cursor metadata mode, and Windows GDI.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone)]
pub struct CapturedCursor {
    /// ARGB8888 pixel data (pre-multiplied alpha), row-major.
    pub pixels: Vec<u8>,
    /// Position relative to the captured output's top-left corner.
    pub x: i32,
    pub y: i32,
    /// Cursor hotspot inside the image.
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// Cursor image dimensions.
    pub width: u32,
    pub height: u32,
    /// Stable cursor shape serial when the backend exposes one.
    pub shape_serial: u64,
    /// Whether the cursor is currently visible.
    pub visible: bool,
}

pub struct CapturedFrame {
    /// Raw CVPixelBufferRef on macOS (retained — caller must release).
    #[cfg(target_os = "macos")]
    pub pixel_buffer_ptr: *mut std::ffi::c_void,
    #[cfg(not(target_os = "macos"))]
    pub data: FrameData,
    pub width: u32,
    pub height: u32,
    /// Cursor data for backends that capture cursor separately.
    /// `None` when cursor is already embedded in the frame or currently hidden.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub cursor: Option<CapturedCursor>,
}

// SAFETY: The CVPixelBufferRef is retained and owned by this struct.
// On Linux, OwnedFd is Send and Vec<u8> is Send. On Windows, Ram frames are Vec<u8>.
unsafe impl Send for CapturedFrame {}

/// Composite cursor onto a BGRA frame in-place (software alpha blending).
///
/// Cursor pixels are stored as BGRA bytes in memory, which matches the frame
/// memory layout used by the software encode path on little-endian targets.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn composite_cursor(
    frame_data: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    cursor: &CapturedCursor,
) {
    if !cursor.visible || cursor.pixels.is_empty() {
        return;
    }

    let fw = frame_width as i32;
    let fh = frame_height as i32;
    let cw = cursor.width as i32;
    let ch = cursor.height as i32;

    for cy in 0..ch {
        let fy = cursor.y + cy;
        if fy < 0 || fy >= fh {
            continue;
        }

        for cx in 0..cw {
            let fx = cursor.x + cx;
            if fx < 0 || fx >= fw {
                continue;
            }

            let cursor_offset = ((cy * cw + cx) * 4) as usize;
            let frame_offset = ((fy * fw + fx) * 4) as usize;

            if cursor_offset + 3 >= cursor.pixels.len() || frame_offset + 3 >= frame_data.len() {
                continue;
            }

            let cb = cursor.pixels[cursor_offset];
            let cg = cursor.pixels[cursor_offset + 1];
            let cr = cursor.pixels[cursor_offset + 2];
            let ca = cursor.pixels[cursor_offset + 3];

            if ca == 0 {
                continue;
            }

            if ca == 255 {
                frame_data[frame_offset] = cb;
                frame_data[frame_offset + 1] = cg;
                frame_data[frame_offset + 2] = cr;
                frame_data[frame_offset + 3] = 255;
            } else {
                let alpha = ca as u32;
                let inv_alpha = 255 - alpha;

                frame_data[frame_offset] =
                    ((cb as u32 * alpha + frame_data[frame_offset] as u32 * inv_alpha) / 255) as u8;
                frame_data[frame_offset + 1] = ((cg as u32
                    * alpha
                    + frame_data[frame_offset + 1] as u32 * inv_alpha)
                    / 255) as u8;
                frame_data[frame_offset + 2] = ((cr as u32
                    * alpha
                    + frame_data[frame_offset + 2] as u32 * inv_alpha)
                    / 255) as u8;
                frame_data[frame_offset + 3] = 255;
            }
        }
    }
}

pub trait CaptureBackend: Send {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String>;
    fn stop(&mut self);
}

pub fn set_target_fps(fps: u32) {
    TARGET_FPS.store(fps.max(1), Ordering::Relaxed);
}

pub fn target_fps() -> u32 {
    TARGET_FPS.load(Ordering::Relaxed).max(1)
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::PlatformCapture;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::PlatformCapture;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::PlatformCapture;
