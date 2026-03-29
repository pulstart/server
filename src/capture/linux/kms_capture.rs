use super::super::{CaptureBackend, CapturedCursor, CapturedFrame, DmaBufPlane, FrameData};
use super::target_frame_interval;
use crossbeam_channel::{Sender, TrySendError};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use drm::control::{self, Device as ControlDevice};
use drm::Device as BasicDevice;

/// Wrapper around a DRM card file descriptor that implements the drm crate traits.
struct Card(File);

impl AsRawFd for Card {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

impl Card {
    /// Try to open a DRM card device, iterating card0..card7.
    fn open() -> Result<Self, String> {
        for i in 0..8 {
            let path = format!("/dev/dri/card{i}");
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => {
                    let card = Card(file);
                    // Verify we can use modesetting on this device
                    if card.get_driver().is_ok() {
                        println!("[kms] Opened {path}");
                        return Ok(card);
                    }
                }
                Err(_) => continue,
            }
        }
        Err("No usable DRM card found (/dev/dri/card0..7)".into())
    }
}

pub struct KmsCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl KmsCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

/// Find the first active primary plane with a framebuffer attached.
fn find_active_plane(card: &Card) -> Result<control::plane::Handle, String> {
    let planes = card
        .plane_handles()
        .map_err(|e| format!("plane_handles: {e}"))?;

    for &handle in planes.iter() {
        if let Ok(plane) = card.get_plane(handle) {
            if plane.framebuffer().is_some() {
                // Skip cursor planes — we capture those separately
                if is_cursor_plane(card, handle) {
                    continue;
                }
                return Ok(handle);
            }
        }
    }
    Err("No active plane with framebuffer found".into())
}

/// Check if a plane is a cursor plane by reading its "type" property.
fn is_cursor_plane(card: &Card, plane_handle: control::plane::Handle) -> bool {
    // DRM_PLANE_TYPE_CURSOR = 2
    const DRM_PLANE_TYPE_CURSOR: u64 = 2;

    if let Ok(props) = card.get_properties(plane_handle) {
        for (prop_id, value) in props.iter() {
            if let Ok(info) = card.get_property(*prop_id) {
                if info.name().to_str() == Ok("type") && *value == DRM_PLANE_TYPE_CURSOR {
                    return true;
                }
            }
        }
    }
    false
}

/// Find the cursor plane for the same CRTC as the primary plane.
fn find_cursor_plane(
    card: &Card,
    primary_plane_handle: control::plane::Handle,
) -> Option<control::plane::Handle> {
    let primary = card.get_plane(primary_plane_handle).ok()?;
    let primary_crtc = primary.crtc()?;

    let planes = card.plane_handles().ok()?;
    for &handle in planes.iter() {
        if let Ok(plane) = card.get_plane(handle) {
            if plane.crtc() == Some(primary_crtc) && is_cursor_plane(card, handle) {
                return Some(handle);
            }
        }
    }
    None
}

/// Read cursor position from plane properties.
fn read_cursor_position(
    card: &Card,
    cursor_handle: control::plane::Handle,
) -> Option<(i32, i32, u32, u32)> {
    let props = card.get_properties(cursor_handle).ok()?;
    let mut crtc_x: Option<i32> = None;
    let mut crtc_y: Option<i32> = None;
    let mut crtc_w: Option<u32> = None;
    let mut crtc_h: Option<u32> = None;

    for (prop_id, value) in props.iter() {
        if let Ok(info) = card.get_property(*prop_id) {
            match info.name().to_str() {
                Ok("CRTC_X") => crtc_x = Some(*value as i32),
                Ok("CRTC_Y") => crtc_y = Some(*value as i32),
                Ok("CRTC_W") => crtc_w = Some(*value as u32),
                Ok("CRTC_H") => crtc_h = Some(*value as u32),
                _ => {}
            }
        }
    }

    Some((crtc_x?, crtc_y?, crtc_w?, crtc_h?))
}

/// Capture cursor image from its DRM plane by mmap'ing the cursor framebuffer.
/// Matches Sunshine's `update_cursor()` in kmsgrab.cpp.
fn capture_cursor(card: &Card, cursor_handle: control::plane::Handle) -> Option<CapturedCursor> {
    let plane = card.get_plane(cursor_handle).ok()?;

    // No framebuffer = cursor hidden
    let fb_handle = plane.framebuffer()?;

    let (x, y, dst_w, dst_h) = read_cursor_position(card, cursor_handle)?;

    // Get cursor framebuffer info — try FB2 first, fall back to FB1
    let fb2 = card.get_planar_framebuffer(fb_handle).ok()?;

    let cursor_w = fb2.size().0;
    let cursor_h = fb2.size().1;
    let pixel_format = fb2.pixel_format() as u32;

    // Only handle ARGB8888 cursors (standard for all known drivers)
    const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
    if pixel_format != DRM_FORMAT_ARGB8888 {
        return None;
    }

    let gem_buffers = fb2.buffers();
    let gem_handle = gem_buffers[0]?;
    let pitch = fb2.pitches()[0];

    // Export GEM handle as DMA-BUF fd for mmap
    let fd = card.buffer_to_prime_fd(gem_handle, 0x02).ok()?;
    let mapped_size = (pitch * cursor_h) as usize;

    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };

    if mapped == libc::MAP_FAILED {
        return None;
    }

    // Read cursor pixels with DMA-BUF sync (matching Sunshine's approach)
    // DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ = 1 | 4 = 5
    let sync_start: u64 = 5;
    let sync_end: u64 = 2 | 4; // DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ

    // DMA_BUF_IOCTL_SYNC = _IOW('b', 0, struct dma_buf_sync) = 0x40086200
    nix::ioctl_write_ptr_bad!(dma_buf_sync, 0x4008_6200u64, u64);

    unsafe {
        let _ = dma_buf_sync(fd.as_raw_fd(), &sync_start);
    }

    // Read the cursor pixels
    let row_bytes = (cursor_w * 4) as usize;
    let mut pixels = Vec::with_capacity(row_bytes * cursor_h as usize);
    let src = mapped as *const u8;

    for row in 0..cursor_h as usize {
        let start = row * pitch as usize;
        let slice = unsafe { std::slice::from_raw_parts(src.add(start), row_bytes) };
        pixels.extend_from_slice(slice);
    }

    unsafe {
        let _ = dma_buf_sync(fd.as_raw_fd(), &sync_end);
        libc::munmap(mapped, mapped_size);
    }

    Some(CapturedCursor {
        pixels,
        x,
        y,
        hotspot_x: 0,
        hotspot_y: 0,
        width: dst_w,
        height: dst_h,
        shape_serial: 0,
        visible: true,
    })
}

/// Capture a single frame from the given plane, returning a CapturedFrame with DMA-BUF planes.
fn capture_frame(
    card: &Card,
    plane_handle: control::plane::Handle,
    cursor_handle: Option<control::plane::Handle>,
) -> Result<CapturedFrame, String> {
    let plane = card
        .get_plane(plane_handle)
        .map_err(|e| format!("get_plane: {e}"))?;

    let fb_handle = plane
        .framebuffer()
        .ok_or("Plane has no framebuffer attached")?;

    // Get FB2 info (planar framebuffer with modifiers)
    let fb2 = card
        .get_planar_framebuffer(fb_handle)
        .map_err(|e| format!("get_planar_framebuffer: {e}"))?;

    let width = fb2.size().0;
    let height = fb2.size().1;
    let drm_format = fb2.pixel_format() as u32;
    let modifier: u64 = fb2
        .modifier()
        .unwrap_or(drm_fourcc::DrmModifier::Linear)
        .into();

    // Export each plane's GEM handle as a DMA-BUF fd
    let mut planes = Vec::new();
    let gem_buffers = fb2.buffers();
    for i in 0..4 {
        let gem_handle = match gem_buffers[i] {
            Some(h) => h,
            None => break,
        };
        // DRM_RDWR = 0x02
        let owned_fd = card
            .buffer_to_prime_fd(gem_handle, 0x02)
            .map_err(|e| format!("buffer_to_prime_fd: {e}"))?;

        planes.push(DmaBufPlane {
            fd: owned_fd,
            offset: fb2.offsets()[i],
            pitch: fb2.pitches()[i],
            modifier,
        });
    }

    if planes.is_empty() {
        return Err("Framebuffer has no planes".into());
    }

    // Capture cursor from its separate plane
    let cursor = cursor_handle.and_then(|h| capture_cursor(card, h));

    Ok(CapturedFrame {
        data: FrameData::DmaBuf { planes, drm_format },
        width,
        height,
        cursor,
    })
}

impl CaptureBackend for KmsCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("KMS capture already running".into());
        }

        // Open and validate the card before spawning the thread
        let card = Card::open()?;

        // Enable universal planes so we can see overlay/cursor/primary planes
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .map_err(|e| format!("set UniversalPlanes capability: {e}"))?;

        // Find an active plane to validate we can capture
        let plane_handle = find_active_plane(&card)?;
        println!("[kms] Found active plane: {plane_handle:?}");

        // Find cursor plane for this CRTC
        let cursor_handle = find_cursor_plane(&card, plane_handle);
        if let Some(ch) = cursor_handle {
            println!("[kms] Found cursor plane: {ch:?}");
        } else {
            println!("[kms] No cursor plane found (cursor may not be captured)");
        }

        // Test-capture one frame to verify GEM handle export works.
        // On Wayland, non-DRM-master processes can't read framebuffer handles.
        let test_frame = capture_frame(&card, plane_handle, cursor_handle)
            .map_err(|e| format!("KMS test capture failed (not DRM master?): {e}"))?;
        println!(
            "[kms] Test capture OK ({}x{})",
            test_frame.width, test_frame.height
        );

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        let handle = thread::spawn(move || {
            let target_interval = target_frame_interval();
            let trace = std::env::var_os("ST_TRACE").is_some();
            let mut dropped_frames = 0usize;

            while running.load(Ordering::SeqCst) {
                let frame_start = Instant::now();

                // Re-find the active plane each iteration (may change on compositor flip)
                let current_plane = match find_active_plane(&card) {
                    Ok(p) => p,
                    Err(_) => {
                        // Plane might momentarily disappear during a modeset
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                };

                match capture_frame(&card, current_plane, cursor_handle) {
                    Ok(frame) => {
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                if trace && dropped_frames < 8 {
                                    eprintln!(
                                        "[trace][kms] dropped captured frame because capture channel is full"
                                    );
                                }
                                dropped_frames = dropped_frames.saturating_add(1);
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(e) => {
                        eprintln!("[kms] capture_frame error: {e}");
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                }

                // Throttle to ~60 FPS
                let elapsed = frame_start.elapsed();
                if elapsed < target_interval {
                    thread::sleep(target_interval - elapsed);
                }
            }

            println!("[kms] Capture loop exited");
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
