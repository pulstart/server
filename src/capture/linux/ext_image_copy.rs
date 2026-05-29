//! Native ext-image-copy-capture-v1 screen capture (staging protocol).
//!
//! This is the standards-track successor to wlr-screencopy-unstable-v1 and is
//! supported by modern Wayland compositors (Mutter 47+, KWin 6.x, wlroots
//! 0.19+). It uses `ext-image-capture-source-v1` to describe the capture
//! source and `ext-image-copy-capture-v1` for the per-frame protocol.
//!
//! The backend probes at runtime for the required globals and returns an
//! error on `start()` if they are not advertised, so it can fall back cleanly
//! to wlr-screencopy or PipeWire.
//!
//! Current implementation uses SHM buffers. DMA-BUF import (for zero-copy
//! paths) is left as a follow-up (Phase 2.2 / 2.3).

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
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameState {
    Pending,
    Ready,
    Failed,
}

struct State {
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    copy_manager: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,

    // Session constraints
    size: Option<(u32, u32)>,
    shm_formats: Vec<wl_shm::Format>,
    constraints_done: bool,
    session_stopped: bool,

    // Per-frame
    frame_state: FrameState,
    transform: wl_output::Transform,
}

impl State {
    fn new() -> Self {
        Self {
            shm: None,
            output: None,
            source_manager: None,
            copy_manager: None,
            size: None,
            shm_formats: Vec::new(),
            constraints_done: false,
            session_stopped: false,
            frame_state: FrameState::Pending,
            transform: wl_output::Transform::Normal,
        }
    }

    fn reset_frame(&mut self) {
        self.frame_state = FrameState::Pending;
        self.transform = wl_output::Transform::Normal;
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
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
                    if state.output.is_none() {
                        state.output = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.copy_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => {
                state.shm_formats.push(fmt);
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { .. }
            | ext_image_copy_capture_session_v1::Event::DmabufDevice { .. } => {
                // DMA-BUF support is deferred.
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.constraints_done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform {
                transform: WEnum::Value(t),
            } => {
                state.transform = t;
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_state = FrameState::Ready;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                state.frame_state = FrameState::Failed;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
delegate_noop!(
    State:
    ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1
);
delegate_noop!(
    State:
    ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1
);

// ---------------------------------------------------------------------------
// SHM buffer
// ---------------------------------------------------------------------------

struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    data: *mut u8,
    size: usize,
    _fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
}

// SAFETY: mmap pointer accessed only from the capture thread that owns this struct.
unsafe impl Send for ShmBuffer {}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        format: wl_shm::Format,
    ) -> Result<Self, String> {
        let stride = width.checked_mul(4).ok_or("stride overflow")?;
        let size = (stride as usize)
            .checked_mul(height as usize)
            .ok_or("size overflow")?;

        let fd = nix::sys::memfd::memfd_create(
            c"ext-image-copy",
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC,
        )
        .map_err(|e| format!("memfd_create: {e}"))?;

        nix::unistd::ftruncate(&fd, size as i64).map_err(|e| format!("ftruncate: {e}"))?;

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

        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        pool.destroy();

        Ok(Self {
            buffer,
            data: data as *mut u8,
            size,
            _fd: fd,
            width,
            height,
            stride,
            format,
        })
    }

    fn read_bgra(&self) -> Vec<u8> {
        let row_bytes = (self.width * 4) as usize;
        let data = unsafe { std::slice::from_raw_parts(self.data, self.size) };

        // Normalize RGB{A,X} channel order to the BGRA/BGRX our downstream
        // encoders expect. wl_shm formats Argb8888/Xrgb8888 are already
        // little-endian BGRA/BGRX in memory — no swap needed.
        let swap = matches!(
            self.format,
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888
        );

        let mut out = Vec::with_capacity(row_bytes * self.height as usize);
        for row in 0..self.height as usize {
            let start = row * self.stride as usize;
            let slice = &data[start..start + row_bytes];
            if swap {
                for px in slice.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            } else {
                out.extend_from_slice(slice);
            }
        }
        out
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        if !self.data.is_null() {
            unsafe { libc::munmap(self.data as *mut _, self.size) };
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns true when the compositor advertises ext-image-copy-capture-v1.
pub fn verify_ext_image_copy() -> bool {
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        return false;
    }
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();
    let mut state = State::new();
    let _registry = display.get_registry(&qh, ());
    if eq.roundtrip(&mut state).is_err() {
        return false;
    }
    state.source_manager.is_some() && state.copy_manager.is_some()
}

pub struct ExtImageCopyCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ExtImageCopyCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for ExtImageCopyCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("ext-image-copy capture already running".into());
        }

        // Fast validation on the caller's thread.
        {
            let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
            let display = conn.display();
            let mut eq = conn.new_event_queue();
            let qh = eq.handle();
            let mut state = State::new();
            let _registry = display.get_registry(&qh, ());
            eq.roundtrip(&mut state)
                .map_err(|e| format!("Wayland roundtrip: {e}"))?;
            if state.copy_manager.is_none() {
                return Err("ext_image_copy_capture_manager_v1 not available".into());
            }
            if state.source_manager.is_none() {
                return Err("ext_output_image_capture_source_manager_v1 not available".into());
            }
            if state.shm.is_none() {
                return Err("wl_shm not available".into());
            }
            if state.output.is_none() {
                return Err("No wl_output found".into());
            }
        }

        println!("[capture] ext-image-copy-capture-v1 available, starting native capture");
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        let handle = thread::spawn(move || {
            if let Err(e) = run_capture_loop(tx, running) {
                eprintln!("[ext-image-copy] Capture error: {e}");
            }
        });

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Capture loop
// ---------------------------------------------------------------------------

fn pick_shm_format(formats: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    // Prefer formats the rest of our pipeline already handles (Xrgb/Argb are
    // BGRX/BGRA in memory on LE, matching our encoders).
    let preference = [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Abgr8888,
    ];
    preference.iter().copied().find(|f| formats.contains(f))
}

fn run_capture_loop(tx: Sender<CapturedFrame>, running: Arc<AtomicBool>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect: {e}"))?;
    let display = conn.display();
    let mut eq = conn.new_event_queue();
    let qh = eq.handle();

    let mut state = State::new();
    let _registry = display.get_registry(&qh, ());
    eq.roundtrip(&mut state)
        .map_err(|e| format!("roundtrip: {e}"))?;

    let source_manager = state
        .source_manager
        .clone()
        .ok_or("ext_output_image_capture_source_manager_v1 missing")?;
    let copy_manager = state
        .copy_manager
        .clone()
        .ok_or("ext_image_copy_capture_manager_v1 missing")?;
    let shm = state.shm.clone().ok_or("wl_shm missing")?;
    let output = state.output.clone().ok_or("wl_output missing")?;

    // Create the capture source (bound to the output) and persistent session.
    let source = source_manager.create_source(&output, &qh, ());
    let session = copy_manager.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::PaintCursors,
        &qh,
        (),
    );

    // Wait for initial buffer constraints.
    state.size = None;
    state.shm_formats.clear();
    state.constraints_done = false;
    while !state.constraints_done && !state.session_stopped {
        eq.blocking_dispatch(&mut state)
            .map_err(|e| format!("dispatch(constraints): {e}"))?;
    }
    if state.session_stopped {
        return Err("capture session stopped during setup".into());
    }
    let (mut width, mut height) = state.size.ok_or("compositor did not send buffer_size")?;
    let mut chosen_format =
        pick_shm_format(&state.shm_formats).ok_or("compositor offered no usable wl_shm format")?;

    let target_interval = target_frame_interval();
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut dropped_frames = 0usize;
    let mut shm_buffer: Option<ShmBuffer> = None;
    let mut last_log = Instant::now();

    while running.load(Ordering::SeqCst) {
        if state.session_stopped {
            return Err("capture session stopped".into());
        }

        // Constraints may have changed; re-check size/format.
        if state.constraints_done {
            if let Some((w, h)) = state.size {
                if w != width || h != height {
                    width = w;
                    height = h;
                    shm_buffer = None;
                }
            }
            if let Some(f) = pick_shm_format(&state.shm_formats) {
                if f != chosen_format {
                    chosen_format = f;
                    shm_buffer = None;
                }
            }
        }

        if shm_buffer.is_none() {
            match ShmBuffer::new(&shm, &qh, width, height, chosen_format) {
                Ok(b) => {
                    println!(
                        "[ext-image-copy] SHM buffer {}x{} stride={} format={:?}",
                        b.width, b.height, b.stride, b.format
                    );
                    shm_buffer = Some(b);
                }
                Err(e) => {
                    eprintln!("[ext-image-copy] SHM alloc failed: {e}");
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }

        let frame_start = Instant::now();
        state.reset_frame();

        let buf = shm_buffer.as_ref().unwrap();
        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
        frame.capture();

        loop {
            match state.frame_state {
                FrameState::Ready | FrameState::Failed => break,
                FrameState::Pending => {
                    eq.blocking_dispatch(&mut state)
                        .map_err(|e| format!("dispatch(capture): {e}"))?;
                    if state.session_stopped {
                        break;
                    }
                }
            }
        }

        frame.destroy();

        if state.session_stopped {
            return Err("capture session stopped".into());
        }
        if state.frame_state == FrameState::Failed {
            eprintln!("[ext-image-copy] frame failed; retrying");
            // Compositor may re-emit constraints after buffer_constraints failures.
            state.constraints_done = false;
            state.size = None;
            state.shm_formats.clear();
            let _ = eq.roundtrip(&mut state);
            continue;
        }

        let pixels = buf.read_bgra();
        let captured = CapturedFrame {
            data: FrameData::Ram(pixels),
            width: buf.width,
            height: buf.height,
            cursor: None, // PAINT_CURSORS=1 embeds cursor into the frame
        };

        match tx.try_send(captured) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if trace && dropped_frames < 8 {
                    eprintln!(
                        "[trace][ext-image-copy] dropped frame because capture channel is full"
                    );
                }
                dropped_frames = dropped_frames.saturating_add(1);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }

        let elapsed = frame_start.elapsed();
        if elapsed < target_interval {
            thread::sleep(target_interval - elapsed);
        }

        if trace && last_log.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "[trace][ext-image-copy] steady: last_frame={:?} dropped={}",
                elapsed, dropped_frames
            );
            last_log = Instant::now();
        }
    }

    session.destroy();
    source.destroy();
    drop(shm_buffer);
    println!("[ext-image-copy] Capture loop exited");
    Ok(())
}
