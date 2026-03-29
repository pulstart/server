/// Native wlr-screencopy-unstable-v1 screen capture for wlroots compositors.
///
/// Replaces the `grim` subprocess approach with a direct Wayland protocol implementation,
/// matching Sunshine's `wayland.cpp` + `wlgrab.cpp` pattern.
///
/// Uses the `zwlr_screencopy_manager_v1` protocol to capture output frames into SHM buffers.
/// The compositor renders the cursor into the captured frame (overlay_cursor=1), so no
/// separate cursor handling is needed.
///
/// Protocol flow per frame:
///   1. `capture_output(overlay_cursor=1, output)` → creates a screencopy frame
///   2. Compositor sends `buffer` events (SHM format/size requirements)
///   3. We create a SHM buffer matching the requirements and call `copy(buffer)`
///   4. Compositor sends `ready` when the frame is captured
///   5. We read pixel data from the SHM mapping and send it downstream
///
/// Performance: eliminates process-spawn overhead of `grim` (~15-30ms per frame),
/// enables steady 60 FPS capture on wlroots compositors (Sway, Hyprland, river, etc.).
use super::super::{CaptureBackend, CapturedFrame, FrameData};
use super::target_frame_interval;
use crossbeam_channel::{Sender, TrySendError};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use wayland_client::{
    delegate_noop,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

// ---------------------------------------------------------------------------
// Wayland state machine
// ---------------------------------------------------------------------------

/// SHM buffer requirements received from the compositor.
struct ShmBufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

/// State of the current frame capture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameState {
    /// Waiting for compositor to send buffer requirements.
    Negotiating,
    /// Buffer submitted, waiting for compositor to fill it.
    Copying,
    /// Frame is ready in SHM buffer.
    Ready,
    /// Capture failed (compositor denied or output disconnected).
    Failed,
}

/// Internal state for the Wayland capture session.
struct WaylandState {
    // Globals
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    screencopy_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,

    // Per-frame state
    buffer_info: Option<ShmBufferInfo>,
    frame_state: FrameState,
    y_invert: bool,
    buffer_released: bool,
}

impl WaylandState {
    fn new() -> Self {
        Self {
            shm: None,
            output: None,
            screencopy_manager: None,
            buffer_info: None,
            frame_state: FrameState::Negotiating,
            y_invert: false,
            buffer_released: true,
        }
    }

    fn reset_frame(&mut self) {
        self.buffer_info = None;
        self.frame_state = FrameState::Negotiating;
        self.y_invert = false;
    }
}

// ---------------------------------------------------------------------------
// Wayland protocol dispatch implementations
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    // Bind the first output only (multi-monitor can be added later)
                    if state.output.is_none() {
                        state.output = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // Accept XRGB8888 or ARGB8888 (both are BGRX/BGRA in memory on LE).
                // Prefer formats our encoders handle natively.
                if let WEnum::Value(fmt) = format {
                    let dominated = state.buffer_info.as_ref().map_or(false, |existing| {
                        // Prefer Argb8888 over Xrgb8888 (keeps alpha for cursor)
                        existing.format == wl_shm::Format::Argb8888
                    });
                    let dominated = dominated
                        || (fmt != wl_shm::Format::Xrgb8888 && fmt != wl_shm::Format::Argb8888);
                    if !dominated {
                        state.buffer_info = Some(ShmBufferInfo {
                            format: fmt,
                            width,
                            height,
                            stride,
                        });
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame_state = FrameState::Ready;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_state = FrameState::Failed;
            }
            // BufferDone (v3), LinuxDmabuf, Damage — not needed for SHM path
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            state.buffer_released = true;
        }
    }
}

// Objects without events we need to handle
delegate_noop!(WaylandState: ignore wl_shm::WlShm);
delegate_noop!(WaylandState: ignore wl_output::WlOutput);
delegate_noop!(WaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

// ---------------------------------------------------------------------------
// SHM buffer management
// ---------------------------------------------------------------------------

/// Holds a shared-memory buffer used for screencopy.
struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    data: *mut u8,
    size: usize,
    _fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
}

// SAFETY: The mmap'd pointer is only accessed from the capture thread.
unsafe impl Send for ShmBuffer {}

impl ShmBuffer {
    /// Create a new SHM buffer matching the compositor's requirements.
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WaylandState>,
        info: &ShmBufferInfo,
    ) -> Result<Self, String> {
        let size = (info.stride * info.height) as usize;

        // Create anonymous shared-memory fd via memfd
        let fd = nix::sys::memfd::memfd_create(
            c"wlr-screencopy",
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC,
        )
        .map_err(|e| format!("memfd_create: {e}"))?;

        nix::unistd::ftruncate(&fd, size as i64).map_err(|e| format!("ftruncate: {e}"))?;

        // mmap the buffer for CPU access
        let data = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if data == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }

        // Create Wayland SHM pool and buffer
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            info.width as i32,
            info.height as i32,
            info.stride as i32,
            info.format,
            qh,
            (),
        );
        pool.destroy();

        Ok(Self {
            buffer,
            data: data as *mut u8,
            size,
            _fd: fd,
            width: info.width,
            height: info.height,
            stride: info.stride,
        })
    }

    /// Read the buffer contents as BGRA pixel data (matching our encoder input format).
    fn read_pixels(&self, y_invert: bool) -> Vec<u8> {
        let row_bytes = (self.width * 4) as usize;
        let data = unsafe { std::slice::from_raw_parts(self.data, self.size) };

        if !y_invert && self.stride as usize == row_bytes {
            // Fast path: tightly packed, no flip — single memcpy
            data[..row_bytes * self.height as usize].to_vec()
        } else if !y_invert {
            // Strip stride padding
            let mut out = Vec::with_capacity(row_bytes * self.height as usize);
            for row in 0..self.height as usize {
                let start = row * self.stride as usize;
                out.extend_from_slice(&data[start..start + row_bytes]);
            }
            out
        } else {
            // Y-inverted: read rows bottom-to-top
            let mut out = Vec::with_capacity(row_bytes * self.height as usize);
            for row in (0..self.height as usize).rev() {
                let start = row * self.stride as usize;
                out.extend_from_slice(&data[start..start + row_bytes]);
            }
            out
        }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        if !self.data.is_null() {
            unsafe {
                libc::munmap(self.data as *mut _, self.size);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Verify that Wayland + wlr-screencopy is available.
pub fn verify_wayland() -> bool {
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        return false;
    }
    match Connection::connect_to_env() {
        Ok(conn) => {
            let display = conn.display();
            let mut eq = conn.new_event_queue();
            let qh = eq.handle();
            let mut state = WaylandState::new();
            let _registry = display.get_registry(&qh, ());
            if eq.roundtrip(&mut state).is_err() {
                return false;
            }
            state.screencopy_manager.is_some()
        }
        Err(_) => false,
    }
}

pub struct WaylandCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WaylandCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for WaylandCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Wayland capture already running".into());
        }

        // Quick validation: connect and check for screencopy support
        {
            let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
            let display = conn.display();
            let mut eq = conn.new_event_queue();
            let qh = eq.handle();
            let mut state = WaylandState::new();
            let _registry = display.get_registry(&qh, ());
            eq.roundtrip(&mut state)
                .map_err(|e| format!("Wayland roundtrip: {e}"))?;

            if state.screencopy_manager.is_none() {
                return Err(
                    "zwlr_screencopy_manager_v1 not available (not a wlroots compositor?)".into(),
                );
            }
            if state.shm.is_none() {
                return Err("wl_shm not available".into());
            }
            if state.output.is_none() {
                return Err("No wl_output found".into());
            }
        }

        println!("[wayland] wlr-screencopy protocol available, starting native capture");

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        let handle = thread::spawn(move || {
            if let Err(e) = run_capture_loop(tx, running) {
                eprintln!("[wayland] Capture error: {e}");
            }
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

// ---------------------------------------------------------------------------
// Capture loop
// ---------------------------------------------------------------------------

fn run_capture_loop(tx: Sender<CapturedFrame>, running: Arc<AtomicBool>) -> Result<(), String> {
    // Create a fresh connection for this thread (Wayland connections are not thread-safe)
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = WaylandState::new();
    let _registry = display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| format!("roundtrip: {e}"))?;

    let screencopy = state
        .screencopy_manager
        .as_ref()
        .ok_or("zwlr_screencopy_manager_v1 not bound")?
        .clone();
    let shm = state.shm.as_ref().ok_or("wl_shm not bound")?.clone();
    let output = state.output.as_ref().ok_or("No wl_output")?.clone();

    let mut shm_buffer: Option<ShmBuffer> = None;
    let target_interval = target_frame_interval();
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut dropped_frames = 0usize;

    while running.load(Ordering::SeqCst) {
        let frame_start = Instant::now();

        // The compositor may still be reading from the previous SHM buffer
        // after we get the screencopy "ready" event. Wait for wl_buffer.release
        // before reusing or dropping the backing storage.
        while shm_buffer.is_some() && !state.buffer_released {
            event_queue
                .blocking_dispatch(&mut state)
                .map_err(|e| format!("dispatch (release): {e}"))?;
        }

        // Reset per-frame state
        state.reset_frame();

        // Request a frame capture with cursor overlay
        let frame = screencopy.capture_output(1, &output, &qh, ());

        // Dispatch until we receive buffer requirements
        // The compositor sends `buffer` events immediately after our request.
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| format!("roundtrip (buffer): {e}"))?;

        let info = match state.buffer_info.take() {
            Some(i) => i,
            None => {
                eprintln!("[wayland] No suitable SHM format offered by compositor");
                frame.destroy();
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        // Create or recreate SHM buffer if dimensions changed
        let need_new = match &shm_buffer {
            Some(b) => b.width != info.width || b.height != info.height || b.stride != info.stride,
            None => true,
        };

        if need_new {
            // Drop the old buffer before creating a new one
            shm_buffer = None;
            match ShmBuffer::new(&shm, &qh, &info) {
                Ok(b) => {
                    println!(
                        "[wayland] SHM buffer: {}x{} stride={} format={:?}",
                        b.width, b.height, b.stride, info.format
                    );
                    shm_buffer = Some(b);
                    state.buffer_released = true;
                }
                Err(e) => {
                    eprintln!("[wayland] SHM buffer creation failed: {e}");
                    frame.destroy();
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }

        let buf = shm_buffer.as_ref().unwrap();

        // Submit buffer for copy
        state.frame_state = FrameState::Copying;
        state.buffer_released = false;
        frame.copy(&buf.buffer);

        // Dispatch until ready or failed
        loop {
            match state.frame_state {
                FrameState::Ready | FrameState::Failed => break,
                _ => {
                    event_queue
                        .blocking_dispatch(&mut state)
                        .map_err(|e| format!("dispatch (copy): {e}"))?;
                }
            }
        }

        frame.destroy();

        if state.frame_state == FrameState::Failed {
            eprintln!("[wayland] Frame capture failed (output disconnected?)");
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        // Read pixels from SHM buffer
        let pixels = buf.read_pixels(state.y_invert);

        let captured = CapturedFrame {
            data: FrameData::Ram(pixels),
            width: buf.width,
            height: buf.height,
            cursor: None, // wlr-screencopy embeds cursor via overlay_cursor=1
        };

        match tx.try_send(captured) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if trace && dropped_frames < 8 {
                    eprintln!(
                        "[trace][wayland] dropped captured frame because capture channel is full"
                    );
                }
                dropped_frames = dropped_frames.saturating_add(1);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }

        // Throttle to target frame rate
        let elapsed = frame_start.elapsed();
        if elapsed < target_interval {
            thread::sleep(target_interval - elapsed);
        }
    }

    // Cleanup (ShmBuffer drop handles munmap + buffer.destroy)
    drop(shm_buffer);

    println!("[wayland] Capture loop exited");
    Ok(())
}
