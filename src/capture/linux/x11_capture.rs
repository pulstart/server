/// X11 XShm screen capture, matching Sunshine's `x11grab.cpp`.
///
/// Uses the X11 SHM (shared memory) extension for fast CPU-accessible screen capture.
/// Needed as a fallback when NvFBC is used for capture but software encoding is required,
/// and also as a standalone capture backend for X11 desktops without NVIDIA.
///
/// NOTE: Does NOT work on XWayland — the root window doesn't contain Wayland desktop content.
/// On Wayland, use wlr-screencopy (grim) or PipeWire instead.
use super::super::{CaptureBackend, CapturedCursor, CapturedFrame, FrameData};
use super::target_frame_interval;
use crossbeam_channel::{Sender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Instant;

/// Raw X11 + XShm FFI bindings.
#[allow(non_snake_case, non_upper_case_globals, non_camel_case_types)]
mod x11_ffi {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_int, c_uint, c_ulong};

    pub type Display = c_void;
    pub type Window = c_ulong;
    pub type Drawable = c_ulong;
    pub type Visual = c_void;
    pub type Bool = c_int;

    pub const ZPixmap: c_int = 2;
    pub const AllPlanes: c_ulong = !0;

    #[repr(C)]
    pub struct XShmSegmentInfo {
        pub shmseg: c_ulong,
        pub shmid: c_int,
        pub shmaddr: *mut c_char,
        pub readOnly: Bool,
    }

    #[repr(C)]
    pub struct XImageRepr {
        pub width: c_int,
        pub height: c_int,
        pub xoffset: c_int,
        pub format: c_int,
        pub data: *mut c_char,
        pub byte_order: c_int,
        pub bitmap_unit: c_int,
        pub bitmap_bit_order: c_int,
        pub bitmap_pad: c_int,
        pub depth: c_int,
        pub bytes_per_line: c_int,
        pub bits_per_pixel: c_int,
        // ... more fields we don't need
    }

    /// XErrorEvent passed to error handlers.
    #[repr(C)]
    pub struct XErrorEvent {
        pub type_: c_int,
        pub display: *mut Display,
        pub resourceid: c_ulong,
        pub serial: c_ulong,
        pub error_code: u8,
        pub request_code: u8,
        pub minor_code: u8,
    }

    pub type XErrorHandler = Option<unsafe extern "C" fn(*mut Display, *mut XErrorEvent) -> c_int>;

    extern "C" {
        pub fn XOpenDisplay(display_name: *const c_char) -> *mut Display;
        pub fn XCloseDisplay(display: *mut Display) -> c_int;
        pub fn XDefaultScreen(display: *mut Display) -> c_int;
        pub fn XRootWindow(display: *mut Display, screen_number: c_int) -> Window;
        pub fn XDefaultVisual(display: *mut Display, screen_number: c_int) -> *mut Visual;
        pub fn XDefaultDepth(display: *mut Display, screen_number: c_int) -> c_int;
        pub fn XDisplayWidth(display: *mut Display, screen_number: c_int) -> c_int;
        pub fn XDisplayHeight(display: *mut Display, screen_number: c_int) -> c_int;

        pub fn XShmQueryExtension(display: *mut Display) -> Bool;
        pub fn XShmCreateImage(
            display: *mut Display,
            visual: *mut Visual,
            depth: c_uint,
            format: c_int,
            data: *mut c_char,
            shminfo: *mut XShmSegmentInfo,
            width: c_uint,
            height: c_uint,
        ) -> *mut XImageRepr;
        pub fn XShmAttach(display: *mut Display, shminfo: *mut XShmSegmentInfo) -> Bool;
        pub fn XShmDetach(display: *mut Display, shminfo: *mut XShmSegmentInfo) -> Bool;
        pub fn XShmGetImage(
            display: *mut Display,
            d: Drawable,
            image: *mut XImageRepr,
            x: c_int,
            y: c_int,
            plane_mask: c_ulong,
        ) -> Bool;
        pub fn XDestroyImage(ximage: *mut XImageRepr) -> c_int;
        pub fn XSync(display: *mut Display, discard: Bool) -> c_int;
        pub fn XSetErrorHandler(handler: XErrorHandler) -> XErrorHandler;
        pub fn XFree(data: *mut c_void) -> c_int;
    }

    // SysV shared memory
    extern "C" {
        pub fn shmget(key: c_int, size: usize, shmflg: c_int) -> c_int;
        pub fn shmat(shmid: c_int, shmaddr: *const c_void, shmflg: c_int) -> *mut c_void;
        pub fn shmdt(shmaddr: *const c_void) -> c_int;
        pub fn shmctl(shmid: c_int, cmd: c_int, buf: *mut c_void) -> c_int;
    }

    pub const IPC_PRIVATE: c_int = 0;
    pub const IPC_CREAT: c_int = 0o001000;
    pub const IPC_RMID: c_int = 0;

    // XFixes cursor image
    #[repr(C)]
    pub struct XFixesCursorImage {
        pub x: c_int,
        pub y: c_int,
        pub width: c_uint,
        pub height: c_uint,
        pub xhot: c_uint,
        pub yhot: c_uint,
        pub cursor_serial: c_ulong,
        pub pixels: *mut c_ulong, // ARGB pixels (each pixel is a `long`, even on 64-bit)
        pub atom: c_ulong,
        pub name: *const c_char,
    }

    extern "C" {
        pub fn XFixesQueryExtension(
            display: *mut Display,
            event_base_return: *mut c_int,
            error_base_return: *mut c_int,
        ) -> Bool;
        pub fn XFixesGetCursorImage(display: *mut Display) -> *mut XFixesCursorImage;
    }
}

/// Global flag set by the custom X error handler.
static X_ERROR_OCCURRED: AtomicBool = AtomicBool::new(false);

/// Custom X error handler that records errors instead of calling exit().
unsafe extern "C" fn x_error_handler(
    _display: *mut x11_ffi::Display,
    event: *mut x11_ffi::XErrorEvent,
) -> std::os::raw::c_int {
    let code = (*event).error_code;
    let request = (*event).request_code;
    eprintln!("[x11] X error: code={code}, request={request} (suppressed)");
    X_ERROR_OCCURRED.store(true, Ordering::SeqCst);
    0
}

/// Install our error handler and clear the error flag.
fn install_x_error_handler() {
    X_ERROR_OCCURRED.store(false, Ordering::SeqCst);
    unsafe { x11_ffi::XSetErrorHandler(Some(x_error_handler)) };
}

/// Check if an X error occurred since the last clear, syncing first to flush.
fn check_x_error(display: *mut x11_ffi::Display) -> bool {
    unsafe { x11_ffi::XSync(display, 0) };
    X_ERROR_OCCURRED.swap(false, Ordering::SeqCst)
}

/// Verify that X11 + XShm is available.
pub fn verify_x11() -> bool {
    let display = unsafe { x11_ffi::XOpenDisplay(std::ptr::null()) };
    if display.is_null() {
        return false;
    }
    let has_shm = unsafe { x11_ffi::XShmQueryExtension(display) } != 0;
    unsafe { x11_ffi::XCloseDisplay(display) };
    has_shm
}

pub struct X11Capture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl X11Capture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for X11Capture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("X11 capture already running".into());
        }

        // Install our error handler so X errors don't crash the process
        install_x_error_handler();

        let display = unsafe { x11_ffi::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            return Err("Cannot open X11 display (DISPLAY not set?)".into());
        }

        let has_shm = unsafe { x11_ffi::XShmQueryExtension(display) } != 0;
        if !has_shm {
            unsafe { x11_ffi::XCloseDisplay(display) };
            return Err("X11 SHM extension not available".into());
        }

        let screen = unsafe { x11_ffi::XDefaultScreen(display) };
        let root = unsafe { x11_ffi::XRootWindow(display, screen) };
        let visual = unsafe { x11_ffi::XDefaultVisual(display, screen) };
        let depth = unsafe { x11_ffi::XDefaultDepth(display, screen) };
        let width = unsafe { x11_ffi::XDisplayWidth(display, screen) } as u32;
        let height = unsafe { x11_ffi::XDisplayHeight(display, screen) } as u32;

        println!("[x11] Screen: {width}x{height}, depth: {depth}");

        let mut shminfo: x11_ffi::XShmSegmentInfo = unsafe { std::mem::zeroed() };
        let ximage = unsafe {
            x11_ffi::XShmCreateImage(
                display,
                visual,
                depth as u32,
                x11_ffi::ZPixmap,
                std::ptr::null_mut(),
                &mut shminfo,
                width,
                height,
            )
        };
        if ximage.is_null() {
            unsafe { x11_ffi::XCloseDisplay(display) };
            return Err("XShmCreateImage failed".into());
        }

        let image_size = unsafe { (*ximage).bytes_per_line as usize * (*ximage).height as usize };
        let shmid = unsafe {
            x11_ffi::shmget(x11_ffi::IPC_PRIVATE, image_size, x11_ffi::IPC_CREAT | 0o600)
        };
        if shmid < 0 {
            unsafe {
                x11_ffi::XDestroyImage(ximage);
                x11_ffi::XCloseDisplay(display);
            }
            return Err("shmget failed".into());
        }

        let shmaddr = unsafe { x11_ffi::shmat(shmid, std::ptr::null(), 0) };
        if shmaddr == (-1isize as *mut std::ffi::c_void) {
            unsafe {
                x11_ffi::shmctl(shmid, x11_ffi::IPC_RMID, std::ptr::null_mut());
                x11_ffi::XDestroyImage(ximage);
                x11_ffi::XCloseDisplay(display);
            }
            return Err("shmat failed".into());
        }

        shminfo.shmid = shmid;
        shminfo.shmaddr = shmaddr as *mut i8;
        shminfo.readOnly = 0;
        unsafe { (*ximage).data = shmaddr as *mut i8 };

        if unsafe { x11_ffi::XShmAttach(display, &mut shminfo) } == 0 {
            unsafe {
                x11_ffi::shmdt(shmaddr);
                x11_ffi::shmctl(shmid, x11_ffi::IPC_RMID, std::ptr::null_mut());
                x11_ffi::XDestroyImage(ximage);
                x11_ffi::XCloseDisplay(display);
            }
            return Err("XShmAttach failed".into());
        }

        // Mark segment for removal after detach
        unsafe { x11_ffi::shmctl(shmid, x11_ffi::IPC_RMID, std::ptr::null_mut()) };

        // Test capture: XShmGetImage + XSync to verify it actually works.
        // On XWayland, the root window can't be captured and this will fail.
        unsafe {
            x11_ffi::XShmGetImage(display, root, ximage, 0, 0, x11_ffi::AllPlanes);
        }
        if check_x_error(display) {
            unsafe {
                x11_ffi::XShmDetach(display, &mut shminfo);
                x11_ffi::shmdt(shmaddr as *const std::ffi::c_void);
                x11_ffi::XDestroyImage(ximage);
                x11_ffi::XCloseDisplay(display);
            }
            return Err("XShmGetImage failed (XWayland root window not capturable?)".into());
        }

        // Check for XFixes cursor support
        let has_xfixes = unsafe {
            let mut event_base = 0i32;
            let mut error_base = 0i32;
            x11_ffi::XFixesQueryExtension(display, &mut event_base, &mut error_base) != 0
        };
        if has_xfixes {
            println!("[x11] XFixes cursor capture available");
        } else {
            println!("[x11] XFixes not available — cursor will not be captured");
        }

        println!("[x11] XShm capture initialized ({width}x{height}, {image_size} bytes)");

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        // SAFETY: display, ximage, shminfo are used only from this thread.
        struct XState {
            display: *mut x11_ffi::Display,
            ximage: *mut x11_ffi::XImageRepr,
            shminfo: x11_ffi::XShmSegmentInfo,
            root: x11_ffi::Window,
            width: u32,
            height: u32,
            image_size: usize,
            has_xfixes: bool,
        }
        unsafe impl Send for XState {}

        let state = XState {
            display,
            ximage,
            shminfo,
            root,
            width,
            height,
            image_size,
            has_xfixes,
        };

        let handle = thread::spawn(move || {
            let mut state = state;
            let target_interval = target_frame_interval();
            let trace = std::env::var_os("ST_TRACE").is_some();
            let mut dropped_frames = 0usize;

            while running.load(Ordering::SeqCst) {
                let frame_start = Instant::now();

                let ok = unsafe {
                    x11_ffi::XShmGetImage(
                        state.display,
                        state.root,
                        state.ximage,
                        0,
                        0,
                        x11_ffi::AllPlanes,
                    )
                };

                if ok != 0 && !X_ERROR_OCCURRED.load(Ordering::SeqCst) {
                    let data = unsafe {
                        std::slice::from_raw_parts(
                            state.shminfo.shmaddr as *const u8,
                            state.image_size,
                        )
                    };

                    // Capture cursor via XFixes
                    let cursor = if state.has_xfixes {
                        capture_xfixes_cursor(state.display)
                    } else {
                        None
                    };

                    let frame = CapturedFrame {
                        data: FrameData::Ram(data.to_vec()),
                        width: state.width,
                        height: state.height,
                        cursor,
                    };

                    match tx.try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            if trace && dropped_frames < 8 {
                                eprintln!(
                                    "[trace][x11] dropped captured frame because capture channel is full"
                                );
                            }
                            dropped_frames = dropped_frames.saturating_add(1);
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                } else {
                    // Clear the error flag and continue
                    X_ERROR_OCCURRED.store(false, Ordering::SeqCst);
                }

                let elapsed = frame_start.elapsed();
                if elapsed < target_interval {
                    thread::sleep(target_interval - elapsed);
                }
            }

            // Cleanup
            unsafe {
                x11_ffi::XShmDetach(state.display, &mut state.shminfo);
                x11_ffi::shmdt(state.shminfo.shmaddr as *const std::ffi::c_void);
                x11_ffi::XDestroyImage(state.ximage);
                x11_ffi::XCloseDisplay(state.display);
            }

            println!("[x11] Capture loop exited");
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

/// Capture cursor image via XFixes extension.
/// Matches Sunshine's x11grab.cpp cursor capture path.
///
/// XFixesCursorImage.pixels is an array of `unsigned long` (8 bytes on 64-bit),
/// each containing one ARGB pixel. We compact them into ARGB8888 (4 bytes each).
fn capture_xfixes_cursor(display: *mut x11_ffi::Display) -> Option<CapturedCursor> {
    let cursor_image = unsafe { x11_ffi::XFixesGetCursorImage(display) };
    if cursor_image.is_null() {
        return None;
    }

    let ci = unsafe { &*cursor_image };
    let w = ci.width as usize;
    let h = ci.height as usize;
    let pixel_count = w * h;

    if pixel_count == 0 {
        unsafe { x11_ffi::XFree(cursor_image as *mut _) };
        return None;
    }

    // Convert unsigned long pixels to ARGB8888 bytes
    let src = unsafe { std::slice::from_raw_parts(ci.pixels, pixel_count) };
    let mut pixels = Vec::with_capacity(pixel_count * 4);
    for &px in src {
        // Each `unsigned long` is an ARGB pixel; extract bottom 32 bits
        let argb = px as u32;
        pixels.extend_from_slice(&argb.to_ne_bytes());
    }

    // Position: XFixes reports the cursor hotspot position on screen
    let x = ci.x - ci.xhot as i32;
    let y = ci.y - ci.yhot as i32;

    unsafe { x11_ffi::XFree(cursor_image as *mut _) };

    Some(CapturedCursor {
        pixels: pixels.into(),
        x,
        y,
        hotspot_x: ci.xhot,
        hotspot_y: ci.yhot,
        width: w as u32,
        height: h as u32,
        shape_serial: ci.cursor_serial as u64,
        visible: true,
    })
}

/// Composite cursor onto a BGRA frame in-place (software alpha blending).
/// Used to embed cursor data into RAM frames before encoding.
///
/// The cursor pixels are ARGB8888 (native byte order). The frame is BGRA8888.
/// On little-endian systems: ARGB in memory = [B, G, R, A] which matches BGRA.
#[allow(dead_code)]
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

            // Cursor is ARGB8888 in native byte order (= BGRA in memory on LE)
            let cb = cursor.pixels[cursor_offset];
            let cg = cursor.pixels[cursor_offset + 1];
            let cr = cursor.pixels[cursor_offset + 2];
            let ca = cursor.pixels[cursor_offset + 3];

            if ca == 0 {
                continue;
            }

            if ca == 255 {
                // Fully opaque — direct copy
                frame_data[frame_offset] = cb;
                frame_data[frame_offset + 1] = cg;
                frame_data[frame_offset + 2] = cr;
                frame_data[frame_offset + 3] = 255;
            } else {
                // Alpha blend: out = cursor * alpha + frame * (1 - alpha)
                let alpha = ca as u32;
                let inv_alpha = 255 - alpha;

                frame_data[frame_offset] =
                    ((cb as u32 * alpha + frame_data[frame_offset] as u32 * inv_alpha) / 255) as u8;
                frame_data[frame_offset + 1] = ((cg as u32 * alpha
                    + frame_data[frame_offset + 1] as u32 * inv_alpha)
                    / 255) as u8;
                frame_data[frame_offset + 2] = ((cr as u32 * alpha
                    + frame_data[frame_offset + 2] as u32 * inv_alpha)
                    / 255) as u8;
                frame_data[frame_offset + 3] = 255;
            }
        }
    }
}

/// Composite cursor using pre-multiplied alpha (faster, matches Sunshine).
/// Pre-multiplied: out = cursor_premul + frame * (1 - alpha)
#[allow(dead_code)]
pub fn composite_cursor_premultiplied(
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

            let ca = cursor.pixels[cursor_offset + 3];
            if ca == 0 {
                continue;
            }

            let inv_alpha = 255 - ca as u32;

            // Pre-multiplied: out = src + dst * (1 - alpha)
            frame_data[frame_offset] = (cursor.pixels[cursor_offset] as u32
                + frame_data[frame_offset] as u32 * inv_alpha / 255)
                as u8;
            frame_data[frame_offset + 1] = (cursor.pixels[cursor_offset + 1] as u32
                + frame_data[frame_offset + 1] as u32 * inv_alpha / 255)
                as u8;
            frame_data[frame_offset + 2] = (cursor.pixels[cursor_offset + 2] as u32
                + frame_data[frame_offset + 2] as u32 * inv_alpha / 255)
                as u8;
            frame_data[frame_offset + 3] = 255;
        }
    }
}
