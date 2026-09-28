use super::super::{
    CaptureBackend, CapturedCursor, CapturedFrame, DmaBufPlane, FrameCredit, FrameData,
};
use super::kms_gpu_copy::KmsStabilizer;
use super::target_frame_interval;
use crossbeam_channel::{Sender, TrySendError};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use drm::control::{self, Device as ControlDevice};
use drm::Device as BasicDevice;
use st_protocol::control::OutputInfo;

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
    ///
    /// On hybrid GPU laptops (AMD iGPU + NVIDIA dGPU), the first card may not
    /// be the one driving the display. We try all cards and prefer the one that
    /// has active primary planes with framebuffers — that's where the display
    /// compositor is rendering.
    fn open(verbose: bool) -> Result<(Self, Option<String>), String> {
        let mut fallback: Option<(Self, String, Option<String>)> = None;

        for i in 0..8 {
            let path = format!("/dev/dri/card{i}");
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                // We spawn helper processes (zenity/kdialog/loginctl) while capture
                // is live; without O_CLOEXEC every card fd we probe leaks into them.
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(f) => f,
                Err(_) => continue,
            };

            let card = Card(file);
            if card.get_driver().is_err() {
                continue;
            }
            card.drop_implicit_master(&path, verbose);

            let render_node = Self::render_node_for(&card);
            let driver_name = card
                .get_driver()
                .map(|d| d.name().to_string_lossy().to_string())
                .unwrap_or_default();

            // Enable universal planes temporarily to check for active displays
            let has_display = if card
                .set_client_capability(drm::ClientCapability::UniversalPlanes, true)
                .is_ok()
            {
                Self::has_active_display(&card)
            } else {
                false
            };

            if has_display {
                if verbose {
                    println!(
                        "[kms] Opened {path} (driver: {driver_name}, render: {})",
                        render_node.as_deref().unwrap_or("none")
                    );
                }
                return Ok((card, render_node));
            }

            // Keep as fallback if no card has an active display
            if fallback.is_none() {
                fallback = Some((card, path, render_node));
            }
        }

        if let Some((card, path, render_node)) = fallback {
            let driver_name = card
                .get_driver()
                .map(|d| d.name().to_string_lossy().to_string())
                .unwrap_or_default();
            if verbose {
                println!(
                    "[kms] Opened {path} as fallback (driver: {driver_name}, no active display found on other cards)"
                );
            }
            return Ok((card, render_node));
        }

        Err("No usable DRM card found (/dev/dri/card0..7)".into())
    }

    /// Release DRM master if opening the card node implicitly granted it.
    ///
    /// The kernel hands DRM master to whoever opens a primary node while no one
    /// else holds it. In system mode we run as root from the login screen, so we
    /// can win that race against the compositor/greeter — which then cannot
    /// become master and never puts an image on screen. Holding master also
    /// blocks the compositor from re-acquiring it across VT switches.
    ///
    /// Capture does not need master: `GETFB2` is gated on `CAP_SYS_ADMIN` (the
    /// file capability the installer grants) and `PRIME_HANDLE_TO_FD` is
    /// render-allowed, so both keep working after the drop.
    ///
    /// `DRM_IOCTL_DROP_MASTER` is a no-op returning `EINVAL` unless *this* fd is
    /// the current master, so calling it unconditionally cannot steal master
    /// from the compositor. `Ok` therefore means "we had implicit master and
    /// gave it back"; an error means we never held it.
    fn drop_implicit_master(&self, path: &str, verbose: bool) {
        match self.release_master_lock() {
            Ok(()) => {
                if verbose {
                    println!("[kms] Dropped implicit DRM master for {path}");
                }
            }
            Err(err) if verbose => {
                println!("[kms] {path} not DRM master ({err}); nothing to drop");
            }
            Err(_) => {}
        }
    }

    /// Get the render node path for this card (e.g. /dev/dri/renderD128).
    fn render_node_for(card: &Card) -> Option<String> {
        let node = drm::node::DrmNode::from_file(card).ok()?;
        let render_path = node.dev_path_with_type(drm::node::NodeType::Render)?;
        Some(render_path.to_string_lossy().to_string())
    }

    /// Check if this card has any active primary plane with a framebuffer.
    fn has_active_display(card: &Card) -> bool {
        let planes = match card.plane_handles() {
            Ok(p) => p,
            Err(_) => return false,
        };
        for &handle in planes.iter() {
            if let Ok(plane) = card.get_plane(handle) {
                if plane.framebuffer().is_some() && !is_cursor_plane(card, handle) {
                    return true;
                }
            }
        }
        false
    }
}

pub struct KmsCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    /// Render node for the card we're capturing from (e.g. /dev/dri/renderD128).
    /// Used to hint the encoder to open on the same GPU for zero-copy DMA-BUF.
    render_node: Option<String>,
    /// Output the client asked to capture (`OutputInfo::id`). `None` means
    /// "primary / first active plane" — the original single-output behavior.
    selected_output: Option<u32>,
}

impl KmsCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            render_node: None,
            selected_output: None,
        }
    }

    /// Returns the render node path of the GPU we're capturing from.
    pub fn render_node(&self) -> Option<&str> {
        self.render_node.as_deref()
    }
}

/// A connected display enumerated from DRM, plus the CRTC that scans it out.
struct KmsOutput {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    primary: bool,
    crtc: control::crtc::Handle,
}

/// Human-readable prefix for a connector type (e.g. "HDMI-A", "DP", "eDP").
fn interface_name(iface: control::connector::Interface) -> &'static str {
    use control::connector::Interface::*;
    match iface {
        VGA => "VGA",
        DVII => "DVI-I",
        DVID => "DVI-D",
        DVIA => "DVI-A",
        Composite => "Composite",
        SVideo => "S-Video",
        LVDS => "LVDS",
        Component => "Component",
        NinePinDIN => "DIN",
        DisplayPort => "DP",
        HDMIA => "HDMI-A",
        HDMIB => "HDMI-B",
        TV => "TV",
        EmbeddedDisplayPort => "eDP",
        Virtual => "Virtual",
        DSI => "DSI",
        DPI => "DPI",
        Writeback => "Writeback",
        SPI => "SPI",
        USB => "USB",
        _ => "Display",
    }
}

/// FNV-1a hash → stable, nonzero output id derived from the connector name.
/// Stable across runs because the connector name is stable, so the client's
/// remembered selection keeps resolving to the same physical monitor.
fn fnv1a_u32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// Decide which enumerated output index to capture for a requested id.
///
/// Pure helper (no DRM access) so the "unknown id falls back to primary" /
/// "known id picks the right monitor" logic is unit-testable — this is the
/// bug-prone decision behind capturing the wrong screen.
fn resolve_output_index(ids: &[u32], primary_index: usize, requested: Option<u32>) -> usize {
    match requested {
        Some(id) => ids.iter().position(|&x| x == id).unwrap_or(primary_index),
        None => primary_index,
    }
}

/// Enumerate connected outputs and the CRTC scanning each one out.
fn enumerate_outputs(card: &Card) -> Vec<KmsOutput> {
    let res = match card.resource_handles() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut outputs = Vec::new();
    for &conn_handle in res.connectors() {
        let conn = match card.get_connector(conn_handle, false) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if conn.state() != control::connector::State::Connected {
            continue;
        }
        // Resolve the active CRTC via the connector's current encoder. A
        // connected-but-disabled output (no encoder/CRTC) is not capturable.
        let crtc_handle = conn
            .current_encoder()
            .and_then(|enc| card.get_encoder(enc).ok())
            .and_then(|enc| enc.crtc());
        let crtc_handle = match crtc_handle {
            Some(c) => c,
            None => continue,
        };
        let crtc = match card.get_crtc(crtc_handle) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (width, height) = match crtc.mode() {
            Some(mode) => {
                let (w, h) = mode.size();
                (w as u32, h as u32)
            }
            None => continue,
        };
        let (x, y) = crtc.position();
        let name = format!(
            "{}-{}",
            interface_name(conn.interface()),
            conn.interface_id()
        );
        let id = fnv1a_u32(name.as_bytes());
        outputs.push(KmsOutput {
            id,
            name,
            width,
            height,
            x: x as i32,
            y: y as i32,
            primary: x == 0 && y == 0,
            crtc: crtc_handle,
        });
    }

    // Guarantee exactly one primary: if no output sits at (0,0), promote the
    // first so the client always has a sensible default.
    if !outputs.iter().any(|o| o.primary) {
        if let Some(first) = outputs.first_mut() {
            first.primary = true;
        }
    }
    outputs
}

/// Find the primary (non-cursor) plane currently bound to a specific CRTC.
fn find_plane_for_crtc(card: &Card, crtc: control::crtc::Handle) -> Option<control::plane::Handle> {
    let planes = card.plane_handles().ok()?;
    for &handle in planes.iter() {
        if let Ok(plane) = card.get_plane(handle) {
            if plane.crtc() == Some(crtc)
                && plane.framebuffer().is_some()
                && !is_cursor_plane(card, handle)
            {
                return Some(handle);
            }
        }
    }
    None
}

/// Open the card and capture exactly one real frame to validate that KMS
/// capture actually works on this system. On Wayland the compositor holds
/// DRM-master, so PRIME-exporting the scanout buffer fails without
/// `cap_sys_admin` — this probe is what gates the KMS-preferred default and
/// makes the portal fallback kick in when the capability is missing.
pub fn probe_can_capture() -> Result<(), String> {
    let (card, _render_node) = Card::open(false)?;
    card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
        .map_err(|e| format!("set UniversalPlanes: {e}"))?;
    let plane = find_active_plane(&card)?;
    let cursor = find_cursor_plane(&card, plane);
    let frame = capture_frame(&card, plane, cursor, None).map_err(|e| {
        format!("KMS probe capture failed (not DRM master / missing cap_sys_admin?): {e}")
    })?;
    if frame.width == 0 || frame.height == 0 {
        return Err("KMS probe produced a zero-sized frame".into());
    }
    Ok(())
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

/// The cursor plane that can drive the primary plane's CRTC, bound or not: a
/// hidden cursor leaves it unbound, and matching only the current binding lost
/// the cursor for a whole session started while a game hid it.
fn find_cursor_plane(
    card: &Card,
    primary_plane_handle: control::plane::Handle,
) -> Option<control::plane::Handle> {
    let crtc = card.get_plane(primary_plane_handle).ok()?.crtc()?;
    let resources = card.resource_handles().ok()?;
    card.plane_handles()
        .ok()?
        .iter()
        .filter_map(|&handle| Some((handle, card.get_plane(handle).ok()?)))
        .filter(|(_, plane)| {
            plane.crtc() == Some(crtc)
                || resources
                    .filter_crtcs(plane.possible_crtcs())
                    .contains(&crtc)
        })
        .filter(|&(handle, _)| is_cursor_plane(card, handle))
        .max_by_key(|(_, plane)| plane.crtc() == Some(crtc))
        .map(|(handle, _)| handle)
}

/// Read the cursor's on-screen position from atomic plane properties.
///
/// Returns `(x, y)` in CRTC pixels, or `(0, 0)` where the props are hidden:
/// nvidia-drm shows `CRTC_X`/`CRTC_Y` only to atomic clients, and this fd isn't
/// one (a position change would then trigger a full capture). Hover-absolute
/// renders at the client's own pointer, so an unknown position is harmless there.
fn read_cursor_position(
    card: &Card,
    cursor_handle: control::plane::Handle,
    cached_props: &mut CrtcPosProps,
) -> (i32, i32) {
    if matches!(cached_props, Some(None)) {
        return (0, 0);
    }
    let Ok(props) = card.get_properties(cursor_handle) else {
        return (0, 0);
    };
    let ids = *cached_props.get_or_insert_with(|| {
        let mut x_id = None;
        let mut y_id = None;
        for (prop_id, _value) in props.iter() {
            if let Ok(info) = card.get_property(*prop_id) {
                match info.name().to_str() {
                    Ok("CRTC_X") => x_id = Some(*prop_id),
                    Ok("CRTC_Y") => y_id = Some(*prop_id),
                    _ => {}
                }
            }
        }
        x_id.zip(y_id)
    });
    let Some((crtc_x_id, crtc_y_id)) = ids else {
        return (0, 0);
    };

    let mut crtc_x = 0;
    let mut crtc_y = 0;
    for (prop_id, value) in props.iter() {
        if *prop_id == crtc_x_id {
            crtc_x = *value as i32;
        } else if *prop_id == crtc_y_id {
            crtc_y = *value as i32;
        }
    }
    (crtc_x, crtc_y)
}

/// One-shot diagnostic: log the first few `capture_cursor` early-exits so a
/// silently-missing remote cursor (e.g. NVIDIA/KWin HW cursor plane in an
/// unexpected layout) is debuggable without `ST_URING_TRACE`.
static CURSOR_DIAG_COUNT: AtomicU32 = AtomicU32::new(0);
fn cursor_diag(reason: &str) {
    if CURSOR_DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < 8 {
        eprintln!("[kms][cursor] no cursor captured: {reason}");
    }
}

/// Per-capture-thread cache for KMS cursor dirty-tracking (C5). KWin only swaps
/// the cursor plane's framebuffer when the cursor *shape* changes, so while the
/// fb handle is unchanged the pixels are identical: we skip the
/// PRIME-export + mmap + row-copy and reuse the cached pixels, reading only the
/// (cheap) position. `serial` increments on every shape change so the control
/// publish layer can de-dup unchanged shapes.
#[derive(Default)]
struct CursorCache {
    fb_handle: Option<control::framebuffer::Handle>,
    pixels: Option<Arc<[u8]>>,
    width: u32,
    height: u32,
    serial: u64,
    crtc_pos_props: CrtcPosProps,
}

/// CRTC_X / CRTC_Y property handles, resolved once per plane (property IDs are
/// stable); `Some(None)` = the plane has none (NVIDIA legacy cursor).
type CrtcPosProps = Option<Option<(control::property::Handle, control::property::Handle)>>;

/// Capture cursor image from its DRM plane by mmap'ing the cursor framebuffer.
/// With a `cache`, an unchanged framebuffer handle short-circuits the heavy
/// export+mmap+copy (C5).
fn capture_cursor(
    card: &Card,
    cursor_handle: control::plane::Handle,
    mut cache: Option<&mut CursorCache>,
) -> Option<CapturedCursor> {
    let plane = match card.get_plane(cursor_handle) {
        Ok(p) => p,
        Err(e) => {
            cursor_diag(&format!("get_plane failed: {e}"));
            return None;
        }
    };

    // No framebuffer = cursor hidden (or KWin not using this HW cursor plane)
    let Some(fb_handle) = plane.framebuffer() else {
        cursor_diag("cursor plane has no framebuffer (hidden or SW cursor)");
        if let Some(c) = &mut cache {
            c.fb_handle = None;
            c.pixels = None;
        }
        return None;
    };

    // Read the position via the cache's resolved prop handles (falls back to a
    // local scratch when no cache is provided).
    let mut scratch_props = None;
    let cached_props = match &mut cache {
        Some(c) => &mut c.crtc_pos_props,
        None => &mut scratch_props,
    };
    let (x, y) = read_cursor_position(card, cursor_handle, cached_props);

    // C5 fast path: framebuffer unchanged → cached pixels are still valid.
    if let Some(c) = cache.as_deref() {
        if c.fb_handle == Some(fb_handle) {
            if let Some(px) = &c.pixels {
                return Some(CapturedCursor {
                    pixels: px.clone(),
                    x,
                    y,
                    hotspot_x: 0,
                    hotspot_y: 0,
                    width: c.width,
                    height: c.height,
                    shape_serial: c.serial,
                    visible: true,
                });
            }
        }
    }

    // Get cursor framebuffer info — try FB2 first, fall back to FB1
    let fb2 = match card.get_planar_framebuffer(fb_handle) {
        Ok(f) => f,
        Err(e) => {
            cursor_diag(&format!("get_planar_framebuffer failed: {e}"));
            return None;
        }
    };

    let cursor_w = fb2.size().0;
    let cursor_h = fb2.size().1;
    let pixel_format = fb2.pixel_format() as u32;

    // Only handle ARGB8888 cursors (standard for all known drivers)
    const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
    if pixel_format != DRM_FORMAT_ARGB8888 {
        let f = pixel_format.to_le_bytes();
        cursor_diag(&format!(
            "cursor format fourcc={}{}{}{} (0x{:08x}) not ARGB8888, {}x{}",
            f[0] as char,
            f[1] as char,
            f[2] as char,
            f[3] as char,
            pixel_format,
            cursor_w,
            cursor_h
        ));
        return None;
    }

    let gem_buffers = fb2.buffers();
    let Some(gem_handle) = gem_buffers[0] else {
        cursor_diag("cursor framebuffer has no GEM handle");
        return None;
    };
    let pitch = fb2.pitches()[0];

    // Export GEM handle as DMA-BUF fd for mmap
    let exported = card.buffer_to_prime_fd(gem_handle, 0x02);
    close_gem_handles(card, &gem_buffers);
    let fd = match exported {
        Ok(fd) => fd,
        Err(e) => {
            cursor_diag(&format!("cursor buffer_to_prime_fd failed: {e}"));
            return None;
        }
    };
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
        cursor_diag("cursor mmap failed");
        return None;
    }

    // Read cursor pixels with DMA-BUF sync
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
        unsafe { copy_from_wc(&mut pixels, src.add(row * pitch as usize), row_bytes) };
    }

    unsafe {
        let _ = dma_buf_sync(fd.as_raw_fd(), &sync_end);
        libc::munmap(mapped, mapped_size);
    }

    let pixels_arc: Arc<[u8]> = pixels.into();
    // Update the cache and derive a shape serial that increments on each fb
    // change, so the publish layer can de-dup unchanged shapes (C5).
    let serial = if let Some(c) = cache {
        if c.fb_handle != Some(fb_handle) {
            c.serial = c.serial.wrapping_add(1);
        }
        c.fb_handle = Some(fb_handle);
        c.pixels = Some(pixels_arc.clone());
        c.width = cursor_w;
        c.height = cursor_h;
        c.serial
    } else {
        0
    };

    if CURSOR_DIAG_COUNT.fetch_add(1, Ordering::Relaxed) < 3 {
        eprintln!("[kms][cursor] captured cursor fb={cursor_w}x{cursor_h} pos=({x}, {y})");
    }

    Some(CapturedCursor {
        pixels: pixels_arc,
        x,
        y,
        hotspot_x: 0,
        hotspot_y: 0,
        // The pixel buffer is the framebuffer, so report its dimensions (the
        // displayed CRTC_W/H, when present, can differ; the client scales).
        width: cursor_w,
        height: cursor_h,
        shape_serial: serial,
        visible: true,
    })
}

/// GETFB2 creates a new GEM handle per call; each pins its buffer and a prime
/// cache entry in this fd until closed.
fn close_gem_handles(card: &Card, handles: &[Option<drm::buffer::Handle>; 4]) {
    for (i, handle) in handles.iter().enumerate() {
        if let Some(h) = *handle {
            if !handles[..i].contains(&Some(h)) {
                let _ = card.close_buffer(h);
            }
        }
    }
}

/// Append `len` bytes from a write-combined mapping (a cursor buffer in VRAM).
/// Plain loads cost a PCIe round trip each (256 KiB: 11–15 ms); SSE4.1
/// streaming loads fetch whole lines (~1 ms).
///
/// # Safety
/// `src..src+len` must be readable.
unsafe fn copy_from_wc(dst: &mut Vec<u8>, src: *const u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.1") {
        return copy_from_wc_sse41(dst, src, len);
    }
    dst.extend_from_slice(std::slice::from_raw_parts(src, len));
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn copy_from_wc_sse41(dst: &mut Vec<u8>, src: *const u8, len: usize) {
    use std::arch::x86_64::{__m128i, _mm_storeu_si128, _mm_stream_load_si128};
    let head = src.align_offset(64).min(len);
    let lines = (len - head) / 64;
    dst.extend_from_slice(std::slice::from_raw_parts(src, head));
    dst.reserve(lines * 64);
    let out = dst.as_mut_ptr().add(dst.len()) as *mut __m128i;
    let body = src.add(head) as *const __m128i;
    for line in 0..lines {
        let (s, d) = (body.add(line * 4), out.add(line * 4));
        let v = [
            _mm_stream_load_si128(s),
            _mm_stream_load_si128(s.add(1)),
            _mm_stream_load_si128(s.add(2)),
            _mm_stream_load_si128(s.add(3)),
        ];
        for (i, v) in v.into_iter().enumerate() {
            _mm_storeu_si128(d.add(i), v);
        }
    }
    dst.set_len(dst.len() + lines * 64);
    let done = head + lines * 64;
    dst.extend_from_slice(std::slice::from_raw_parts(src.add(done), len - done));
}

/// Capture a single frame from the given plane, returning a CapturedFrame with DMA-BUF planes.
/// Whether to route captured scanout buffers through the GPU stabilizing copy
/// (see [`KmsStabilizer`]). Default-on: validated live to fix the tearing +
/// frame-jumping caused by handing the compositor's recycled scanout buffer to
/// the async encoder. `ST_KMS_COPY=0` (also `false`/`no`/`off`) is the escape
/// hatch back to the direct (tearing-prone) path, per CLAUDE.md's auto-enable
/// rule. Init failure also falls back to direct automatically.
fn kms_copy_enabled() -> bool {
    !matches!(
        std::env::var("ST_KMS_COPY").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// Damage skip: the compositor page-flips to a different framebuffer whenever
/// it repaints, so an unchanged primary-plane framebuffer means byte-identical
/// content and nothing is captured until it (or the cursor plane) changes, bar
/// a [`DAMAGE_KEEPALIVE`] resend. `ST_KMS_DAMAGE=0` (also `false`/`no`/`off`)
/// captures every interval, for a compositor that re-renders into the bound
/// framebuffer in place (none of KWin/Mutter/wlroots do).
fn kms_damage_skip_enabled() -> bool {
    !matches!(
        std::env::var("ST_KMS_DAMAGE").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// Re-send an unchanged frame at this cadence so late joiners, keyframe
/// requests, and loss recovery never wait on the next real content change.
const DAMAGE_KEEPALIVE: Duration = Duration::from_millis(250);

/// How long `start()` retries plane acquisition + the test capture before giving
/// up. The display can be DPMS-off when a client connects: `trigger_screen_wake`
/// fires just before the pipeline starts, but powering the monitor back on and
/// having the compositor re-attach a scanout framebuffer to the plane is
/// asynchronous (~1-2s; in system mode the wake is routed through the per-user
/// tray agent's 100ms poll, slower still). Retrying for a bounded window lets a
/// just-woken display come back instead of failing the whole pipeline start with
/// "No active plane with framebuffer found". `ST_KMS_START_TIMEOUT_MS` overrides
/// the window (0 = single immediate check, the pre-retry behavior).
fn start_plane_timeout() -> Duration {
    let ms = std::env::var("ST_KMS_START_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5000);
    Duration::from_millis(ms)
}

/// Replace a captured scanout `FrameData::DmaBuf` with a private, stable copy.
/// Borrows the source planes (the stabilizer imports + `glFinish`-copies them),
/// so the original `frame` can be dropped by the caller afterwards. Non-DMA-BUF
/// frames pass through unchanged.
fn stabilize_frame(
    stab: &mut KmsStabilizer,
    frame: &CapturedFrame,
) -> Result<CapturedFrame, String> {
    let data = match &frame.data {
        FrameData::DmaBuf {
            planes, drm_format, ..
        } => stab.stabilize(planes, *drm_format, frame.width, frame.height)?,
        FrameData::Ram(_) | FrameData::RamNv12(_) => {
            return Err("stabilizer expects DMA-BUF frames".into())
        }
    };
    Ok(CapturedFrame {
        data,
        width: frame.width,
        height: frame.height,
        cursor: frame.cursor.clone(),
        // Preserve a keyframe demand set on the source frame by the loop.
        force_keyframe: frame.force_keyframe,
        captured_at: frame.captured_at,
    })
}

fn capture_frame(
    card: &Card,
    plane_handle: control::plane::Handle,
    cursor_handle: Option<control::plane::Handle>,
    cursor_cache: Option<&mut CursorCache>,
) -> Result<CapturedFrame, String> {
    let captured_at = Instant::now();
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

    // Export each plane's GEM handle as a DMA-BUF fd (DRM_RDWR = 0x02)
    let gem_buffers = fb2.buffers();
    let planes = gem_buffers
        .iter()
        .map_while(|gem| *gem)
        .enumerate()
        .map(|(i, gem_handle)| {
            Ok(DmaBufPlane {
                fd: card.buffer_to_prime_fd(gem_handle, 0x02)?,
                offset: fb2.offsets()[i],
                pitch: fb2.pitches()[i],
                modifier,
            })
        })
        .collect::<std::io::Result<Vec<_>>>();
    close_gem_handles(card, &gem_buffers);
    let planes = planes.map_err(|e| format!("buffer_to_prime_fd: {e}"))?;

    if planes.is_empty() {
        return Err("Framebuffer has no planes".into());
    }

    // Capture cursor from its separate plane
    let cursor = cursor_handle.and_then(|h| capture_cursor(card, h, cursor_cache));

    Ok(CapturedFrame {
        data: FrameData::DmaBuf {
            planes,
            drm_format,
            _lease: None,
        },
        width,
        height,
        cursor,
        force_keyframe: false,
        captured_at,
    })
}

impl CaptureBackend for KmsCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("KMS capture already running".into());
        }

        // Open and validate the card before spawning the thread
        let (card, capture_render_node) = Card::open(true)?;
        self.render_node = capture_render_node;

        // Enable universal planes so we can see overlay/cursor/primary planes
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .map_err(|e| format!("set UniversalPlanes capability: {e}"))?;

        // Resolve the requested output (if any) to a fixed CRTC. `None` keeps
        // the original "first active plane" behavior (primary output).
        let target_crtc = match self.selected_output {
            Some(id) => {
                let outputs = enumerate_outputs(&card);
                if outputs.is_empty() {
                    None
                } else {
                    let ids: Vec<u32> = outputs.iter().map(|o| o.id).collect();
                    let primary_index = outputs.iter().position(|o| o.primary).unwrap_or(0);
                    let idx = resolve_output_index(&ids, primary_index, Some(id));
                    if outputs[idx].id != id {
                        eprintln!(
                            "[kms] requested output {id} not found; capturing '{}'",
                            outputs[idx].name
                        );
                    } else {
                        println!(
                            "[kms] Capturing output '{}' ({}x{})",
                            outputs[idx].name, outputs[idx].width, outputs[idx].height
                        );
                    }
                    Some(outputs[idx].crtc)
                }
            }
            None => None,
        };

        // Acquire an active plane and validate a real capture, retrying for a
        // bounded window. The display may be DPMS-off at connect time:
        // `trigger_screen_wake` fires just before the pipeline starts, but waking
        // the monitor (hardware powerup + the compositor re-attaching a scanout
        // framebuffer to the plane) is asynchronous, and in system mode the wake
        // is routed through the per-user tray agent's poll loop. A one-shot check
        // would fail with "No active plane with framebuffer found" before the
        // just-woken display comes back, failing the whole pipeline start. The
        // runtime loop already tolerates transient plane loss; this gives startup
        // the same resilience. ST_KMS_START_TIMEOUT_MS overrides the window.
        let start_timeout = start_plane_timeout();
        let deadline = Instant::now() + start_timeout;
        let mut last_start_err: String;
        let mut announced_wait = false;
        let (plane_handle, cursor_handle, test_frame) = loop {
            // Re-walk planes each attempt — a waking compositor re-binds the
            // framebuffer to the plane mid-window, so a cached handle goes stale.
            let plane = match target_crtc {
                Some(crtc) => find_plane_for_crtc(&card, crtc),
                None => find_active_plane(&card).ok(),
            };
            match plane {
                Some(p) => {
                    let cursor = find_cursor_plane(&card, p);
                    // On Wayland, non-DRM-master processes can't read framebuffer
                    // handles — the test capture proves the export actually works.
                    match capture_frame(&card, p, cursor, None) {
                        Ok(frame) => break (p, cursor, frame),
                        Err(e) => {
                            last_start_err =
                                format!("KMS test capture failed (not DRM master?): {e}");
                        }
                    }
                }
                None => {
                    last_start_err = match target_crtc {
                        Some(_) => "No plane bound to the selected output's CRTC".into(),
                        None => "No active plane with framebuffer found".into(),
                    };
                }
            }
            if Instant::now() >= deadline {
                return Err(last_start_err);
            }
            if !announced_wait {
                println!(
                    "[kms] no capturable scanout plane yet (display waking from \
                     DPMS-off?); retrying up to {start_timeout:?}"
                );
                announced_wait = true;
            }
            thread::sleep(Duration::from_millis(100));
        };
        println!("[kms] Found active plane: {plane_handle:?}");
        if let Some(ch) = cursor_handle {
            println!("[kms] Found cursor plane: {ch:?}");
        } else {
            println!("[kms] No cursor plane found (cursor may not be captured)");
        }
        println!(
            "[kms] Test capture OK ({}x{})",
            test_frame.width, test_frame.height
        );

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let copy_render_node = self.render_node.clone();
        let copy_enabled = kms_copy_enabled();

        let handle = thread::spawn(move || {
            st_protocol::thread_priority::promote_current_thread(
                st_protocol::thread_priority::ThreadRole::Capture,
            );
            let mut target_interval = target_frame_interval();
            let trace = std::env::var_os("ST_TRACE").is_some();
            let mut dropped_frames = 0usize;
            // Active-session switch tracking. On a VT / fast-user switch the
            // foreground compositor changes DRM-master; with cap_sys_admin we
            // keep exporting the new active scanout, but there's a brief window
            // where the framebuffer handle is unreadable. Throttle that error
            // (it used to spam every 16ms and look like a wedge) and announce
            // the recovery + any resolution change once.
            let mut capture_err_streak = 0usize;
            let mut last_err_log: Option<Instant> = None;
            let mut last_dims: Option<(u32, u32)> = None;
            let mut logged_fmt = false;
            // Consecutive stabilize failures before we give up on the GPU copy.
            // Transient failures (all ring slots momentarily in-flight) just
            // drop a frame; only a sustained streak means a real GL/EGL fault.
            let mut stab_fail_streak = 0usize;
            const STAB_FAIL_LIMIT: usize = 30;

            // Optional GPU stabilizing copy: decouples the encoder from KWin's
            // live scanout buffer cycle (fixes tearing + frame jumping). Built
            // on the capture thread so its EGL/GL context stays single-threaded.
            // Any failure logs once and falls back to the direct path.
            let mut stabilizer = if copy_enabled {
                match copy_render_node.as_deref() {
                    Some(node) => match KmsStabilizer::new(node) {
                        Ok(s) => {
                            println!("[kms] GPU stabilizing copy enabled ({node})");
                            Some(s)
                        }
                        Err(e) => {
                            eprintln!(
                                "[kms] stabilizer init failed ({e}); using direct scanout \
                                 (may tear). Set ST_CAPTURE=pipewire if tearing appears."
                            );
                            None
                        }
                    },
                    None => {
                        eprintln!("[kms] render node unknown; using direct scanout (may tear)");
                        None
                    }
                }
            } else {
                println!("[kms] ST_KMS_COPY=0: GPU stabilizing copy disabled (direct scanout)");
                None
            };

            // Damage-skip state (see kms_damage_skip_enabled).
            let damage_skip = kms_damage_skip_enabled();
            if !damage_skip {
                println!("[kms] ST_KMS_DAMAGE=0: damage skip disabled (full-fps capture)");
            }
            let mut scheduler = FlipScheduler::new(target_interval, Instant::now());
            let mut last_scanout_fb: Option<control::framebuffer::Handle> = None;
            let mut last_cursor_sig: Option<(Option<control::framebuffer::Handle>, i32, i32)> =
                None;

            // C4: cache the resolved plane handle. Plane handles are stable; only
            // the framebuffer bound to a plane flips. So we reuse the validated
            // handle and only re-walk all planes (the N+N×M `type`-property reads)
            // when a capture actually fails — i.e. a modeset moved the binding.
            let mut current_plane = plane_handle;
            // C5: per-thread cursor dirty-tracking cache.
            let mut cursor_cache = CursorCache::default();
            let mut last_overrun_log: Option<Instant> = None;

            while running.load(Ordering::SeqCst) {
                // Adaptive fps retargets a running capture.
                let wanted = target_frame_interval();
                if wanted != target_interval {
                    target_interval = wanted;
                    scheduler.set_interval(wanted);
                }

                // Probe failures and capture-error streaks always take the full
                // capture path below — it owns error handling and plane
                // re-acquisition.
                let probe = (capture_err_streak == 0)
                    .then(|| card.get_plane(current_plane).ok()?.framebuffer())
                    .flatten();
                let now = Instant::now();
                let capture_now = match probe {
                    Some(fb) => {
                        let cursor_sig = cursor_handle.map(|h| {
                            let cursor_fb = card.get_plane(h).ok().and_then(|p| p.framebuffer());
                            let (x, y) =
                                read_cursor_position(&card, h, &mut cursor_cache.crtc_pos_props);
                            (cursor_fb, x, y)
                        });
                        let flipped = !damage_skip || last_scanout_fb != Some(fb);
                        let cursor_moved = last_cursor_sig != cursor_sig;
                        last_scanout_fb = Some(fb);
                        last_cursor_sig = cursor_sig;
                        scheduler.tick(now, flipped, cursor_moved)
                    }
                    None => {
                        last_scanout_fb = None;
                        scheduler.force(now);
                        true
                    }
                };
                for _ in 0..scheduler.take_idle_slots() {
                    super::super::record_unchanged_capture_tick();
                }
                if !capture_now {
                    thread::sleep(FLIP_POLL);
                    continue;
                }

                let scanout;
                match capture_frame(&card, current_plane, cursor_handle, Some(&mut cursor_cache)) {
                    Ok(mut frame) => {
                        scanout = now.elapsed();
                        let dims = (frame.width, frame.height);
                        if !logged_fmt {
                            if let FrameData::DmaBuf {
                                drm_format, planes, ..
                            } = &frame.data
                            {
                                let f = drm_format.to_le_bytes();
                                let modifier = planes.first().map(|p| p.modifier).unwrap_or(0);
                                println!(
                                    "[kms] scanout format fourcc={}{}{}{} (0x{:08x}) modifier=0x{:016x} {}x{}",
                                    f[0] as char,
                                    f[1] as char,
                                    f[2] as char,
                                    f[3] as char,
                                    drm_format,
                                    modifier,
                                    dims.0,
                                    dims.1
                                );
                            }
                            logged_fmt = true;
                        }
                        let recovered = capture_err_streak > 0;
                        let dims_changed = last_dims.is_some() && last_dims != Some(dims);
                        if recovered {
                            println!(
                                "[kms] capture recovered after {capture_err_streak} stalled \
                                 frame(s) (active session resumed)"
                            );
                            capture_err_streak = 0;
                            last_err_log = None;
                        }
                        if last_dims != Some(dims) {
                            if dims_changed {
                                println!(
                                    "[kms] scanout changed to {}x{} (output or user switch)",
                                    dims.0, dims.1
                                );
                            }
                            last_dims = Some(dims);
                        }
                        // A seat/user switch jumps content discontinuously; demand a
                        // keyframe so the client doesn't decode garbage inter-frames
                        // until the next on-demand IDR. Same-resolution switches
                        // wouldn't otherwise trigger an encoder rebuild + keyframe.
                        if recovered || dims_changed {
                            frame.force_keyframe = true;
                        }
                        // Route through the GPU stabilizing copy when active. On
                        // success the original scanout frame is dropped here (its
                        // exported FDs close). A transient failure (e.g. all ring
                        // slots in-flight) just drops this frame; only a sustained
                        // failure streak disables the copy and falls back to the
                        // direct (tearing) scanout.
                        let to_send = if let Some(stab) = stabilizer.as_mut() {
                            match stabilize_frame(stab, &frame) {
                                Ok(stable) => {
                                    stab_fail_streak = 0;
                                    Some(stable)
                                }
                                Err(e) => {
                                    stab_fail_streak += 1;
                                    if stab_fail_streak >= STAB_FAIL_LIMIT {
                                        eprintln!(
                                            "[kms] stabilizing copy failed {stab_fail_streak}x \
                                             ({e}); disabling it and falling back to direct \
                                             scanout (may tear)"
                                        );
                                        stabilizer = None;
                                        Some(frame)
                                    } else {
                                        if trace {
                                            eprintln!("[trace][kms] stabilize skipped frame: {e}");
                                        }
                                        None
                                    }
                                }
                            }
                        } else {
                            Some(frame)
                        };

                        if to_send.is_none() {
                            scheduler.missed();
                        }
                        if let Some(frame) = to_send {
                            match tx.try_send(frame) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    scheduler.missed();
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
                    }
                    Err(e) => {
                        capture_err_streak += 1;
                        let now = Instant::now();
                        let should_log = capture_err_streak == 1
                            || last_err_log
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2));
                        if should_log {
                            eprintln!(
                                "[kms] capture stalled ({e}); active session may be switching \
                                 (lost DRM scanout access) — retrying"
                            );
                            last_err_log = Some(now);
                        }
                        // C4: a failed capture means the framebuffer binding moved
                        // (modeset / active-session switch) — re-walk the planes
                        // once to re-acquire it, then retry next iteration.
                        if let Some(p) = match target_crtc {
                            Some(crtc) => find_plane_for_crtc(&card, crtc),
                            None => find_active_plane(&card).ok(),
                        } {
                            current_plane = p;
                        }
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                }

                let took = now.elapsed();
                if took > target_interval
                    && last_overrun_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                {
                    eprintln!(
                        "[kms] capture took {took:.2?} (> {target_interval:.2?} interval; \
                         scanout+cursor {scanout:.2?}, copy {:.2?})",
                        took - scanout
                    );
                    last_overrun_log = Some(Instant::now());
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

    fn list_outputs(&self) -> Vec<OutputInfo> {
        let card = match Card::open(false) {
            Ok((card, _)) => card,
            Err(_) => return Vec::new(),
        };
        let _ = card.set_client_capability(drm::ClientCapability::UniversalPlanes, true);
        enumerate_outputs(&card)
            .into_iter()
            .map(|o| OutputInfo {
                id: o.id,
                name: o.name,
                width: o.width,
                height: o.height,
                x: o.x,
                y: o.y,
                is_primary: o.primary,
            })
            .collect()
    }

    fn select_output(&mut self, id: u32) -> bool {
        if self.selected_output == Some(id) {
            return false;
        }
        self.selected_output = Some(id);
        true
    }
}

/// Flip-driven pacing: the primary plane's framebuffer changes when the
/// compositor commits a frame (already rendered — KWin/NVIDIA waits before
/// committing), so polling it and capturing on change reads each frame within
/// [`FLIP_POLL`] of its commit instead of up to a full interval later on a
/// fixed tick. A [`FrameCredit`] caps the rate; a flip arriving without credit
/// is skipped for the next one (e.g. 144 Hz → 120 fps drops 1 in 6) unless
/// none follows within [`HOLD_INTERVALS`].
struct FlipScheduler {
    interval: Duration,
    credit: FrameCredit,
    captured_at: Instant,
    dirty: bool,
    hold_until: Option<Instant>,
    idle_slots: f64,
}

const FLIP_POLL: Duration = Duration::from_millis(1);
const HOLD_INTERVALS: f64 = 1.5;

impl FlipScheduler {
    fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            credit: FrameCredit::new(now),
            captured_at: now.checked_sub(DAMAGE_KEEPALIVE).unwrap_or(now),
            dirty: true,
            hold_until: None,
            idle_slots: 0.0,
        }
    }

    fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// `flipped`: new primary framebuffer; `changed`: anything else visible
    /// (cursor). Returns whether to capture now.
    fn tick(&mut self, now: Instant, flipped: bool, changed: bool) -> bool {
        let unused = self.credit.refill(now, self.interval);
        if !self.dirty {
            self.idle_slots += unused;
        }
        self.dirty |= flipped || changed;
        let capture = if !self.dirty {
            now.saturating_duration_since(self.captured_at) >= DAMAGE_KEEPALIVE
        } else if self.credit.available() >= 1.0 {
            flipped || self.hold_until.is_none_or(|h| now >= h)
        } else {
            if flipped {
                self.hold_until = Some(now + self.interval.mul_f64(HOLD_INTERVALS));
            }
            false
        };
        if capture {
            self.consume(now);
        }
        capture
    }

    fn force(&mut self, now: Instant) {
        self.credit.refill(now, self.interval);
        self.consume(now);
    }

    /// The capture produced no frame; its content is still owed.
    fn missed(&mut self) {
        self.dirty = true;
    }

    /// Frame slots that passed with nothing to send, so adaptive fps counts a
    /// static or slow-updating screen as delivered.
    fn take_idle_slots(&mut self) -> u32 {
        let whole = self.idle_slots.floor();
        self.idle_slots -= whole;
        whole as u32
    }

    fn consume(&mut self, now: Instant) {
        self.credit.take();
        self.captured_at = now;
        self.dirty = false;
        self.hold_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_output_known_id_picks_that_output() {
        let ids = [10u32, 20, 30];
        // primary is index 1; requesting 30 must pick index 2, not primary.
        assert_eq!(resolve_output_index(&ids, 1, Some(30)), 2);
        assert_eq!(resolve_output_index(&ids, 1, Some(10)), 0);
    }

    #[test]
    fn resolve_output_unknown_id_falls_back_to_primary() {
        let ids = [10u32, 20, 30];
        assert_eq!(resolve_output_index(&ids, 1, Some(999)), 1);
    }

    #[test]
    fn resolve_output_none_uses_primary() {
        let ids = [10u32, 20, 30];
        assert_eq!(resolve_output_index(&ids, 2, None), 2);
    }

    /// Regression guard for the implicit-DRM-master bug class: opening a
    /// primary node grants master when nobody else holds it, and keeping it
    /// stops the compositor/greeter from ever becoming master (black screen,
    /// failed VT switches). `Card::open` must therefore always give it back.
    ///
    /// `release_master_lock` only succeeds when *this* fd is the current master,
    /// so a second call succeeding means the first one never happened — exactly
    /// the state this test exists to catch. Skips where there is no DRM device
    /// or no permission to open one (CI, containers).
    #[test]
    fn card_open_does_not_retain_drm_master() {
        let Ok((card, _)) = Card::open(false) else {
            eprintln!("skipping: no openable DRM card");
            return;
        };
        assert!(
            card.release_master_lock().is_err(),
            "Card::open left the process holding DRM master; the compositor \
             cannot acquire it and the screen stays black"
        );
    }

    /// Copy-engine vs GL readback of the real scanout (root): per-frame
    /// latency and capture-thread CPU, idle and with the GPU saturated, and
    /// both paths must produce the same NV12.
    /// `ST_TEST_VULKAN_KMS=1 sudo -E <test-binary> live_readback_ab --nocapture`
    #[test]
    fn live_readback_ab() {
        use super::super::kms_gpu_copy::tests as kms;
        use std::sync::atomic::AtomicBool;
        if std::env::var_os("ST_TEST_VULKAN_KMS").is_none() {
            return;
        }
        st_protocol::thread_priority::promote_current_thread(
            st_protocol::thread_priority::ThreadRole::Capture,
        );
        let (card, node) = Card::open(false).unwrap();
        let node = node.unwrap();
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .unwrap();
        let plane = find_active_plane(&card).unwrap();
        let mut vk = kms::nv12_stabilizer(&node, true);
        let mut gl = kms::nv12_stabilizer(&node, false);
        let stabilize = |stab: &mut KmsStabilizer| {
            let frame = capture_frame(&card, plane, None, None).unwrap();
            stabilize_frame(stab, &frame).unwrap()
        };
        let FrameData::RamNv12(a) = stabilize(&mut vk).data else {
            panic!("copy engine: expected NV12")
        };
        assert!(kms::served_by_copy_engine(&vk), "copy engine not used");
        let FrameData::RamNv12(b) = stabilize(&mut gl).data else {
            panic!("gl: expected NV12")
        };
        let diff = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| x.abs_diff(*y) as u64)
            .sum::<u64>() as f64
            / a.len() as f64;
        eprintln!("[ab] mean |copy-engine - gl| = {diff:.3}");
        assert!(diff < 1.0, "paths disagree (mean abs diff {diff})");
        for loaded in [false, true] {
            let stop = Arc::new(AtomicBool::new(false));
            let load = loaded.then(|| {
                let load = kms::spawn_gpu_load(node.clone(), 2560, 1440, Arc::clone(&stop));
                thread::sleep(Duration::from_millis(500));
                load
            });
            for (label, stab) in [("copy-engine", &mut vk), ("gl", &mut gl)] {
                let mut times = Vec::new();
                let cpu = kms::thread_cpu_time();
                let mut next = Instant::now();
                for _ in 0..240 {
                    let t = Instant::now();
                    drop(stabilize(stab));
                    times.push(t.elapsed());
                    next += Duration::from_micros(8333);
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                }
                let cpu = (kms::thread_cpu_time() - cpu) / 240;
                times.sort();
                eprintln!(
                    "[ab] {label:<11} {:<8} p50={:>7.2?} p95={:>7.2?} p99={:>7.2?} max={:>7.2?} thread-cpu/frame={cpu:.2?}",
                    if loaded { "gpu-load" } else { "idle" },
                    times[120],
                    times[228],
                    times[237],
                    times[239]
                );
            }
            stop.store(true, Ordering::Relaxed);
            if let Some(load) = load {
                load.join().unwrap();
            }
        }
    }

    struct Sim {
        captures: Vec<f64>,
        ages: Vec<f64>,
        idle_slots: u32,
    }

    /// Polls a 120 fps scheduler every FLIP_POLL (1 ms) against flips at the
    /// given ms offsets; `every_tick` marks each poll as changed (damage skip off).
    fn simulate(flips: &[f64], total_ms: u32, every_tick: bool) -> Sim {
        let base = Instant::now();
        let at = |ms: f64| base + Duration::from_secs_f64(ms / 1000.0);
        let interval = Duration::from_secs_f64(1.0 / 120.0);
        let mut sched = FlipScheduler::new(interval, base);
        let mut sim = Sim {
            captures: Vec::new(),
            ages: Vec::new(),
            idle_slots: 0,
        };
        let (mut next, mut newest) = (0, 0.0);
        for t in 0..total_ms {
            let t = t as f64;
            let mut flipped = t == 0.0 || every_tick;
            while next < flips.len() && flips[next] <= t {
                newest = flips[next];
                next += 1;
                flipped = true;
            }
            if sched.tick(at(t), flipped, false) {
                sim.captures.push(t);
                sim.ages.push(t - newest);
            }
            sim.idle_slots += sched.take_idle_slots();
        }
        sim
    }

    fn periodic(period_ms: f64, total_ms: u32) -> Vec<f64> {
        (1..)
            .map(|i| i as f64 * period_ms)
            .take_while(|&t| t < total_ms as f64)
            .collect()
    }

    #[test]
    fn flip_scheduler_decimates_faster_display_on_flip_edges() {
        let sim = simulate(&periodic(1000.0 / 144.0, 10_000), 10_000, false);
        assert!(
            (1190..=1201).contains(&sim.captures.len()),
            "{}",
            sim.captures.len()
        );
        let worst = sim.ages.iter().cloned().fold(0.0, f64::max);
        assert!(worst < 1.0, "captured a flip {worst} ms late");
    }

    #[test]
    fn flip_scheduler_takes_every_flip_below_target() {
        let flips = periodic(1000.0 / 60.0, 10_000);
        let sim = simulate(&flips, 10_000, false);
        assert_eq!(sim.captures.len(), flips.len() + 1);
        assert!(sim.ages.iter().all(|&a| a < 1.0));
        let delivered = sim.captures.len() as u32 + sim.idle_slots;
        assert!((1190..=1201).contains(&delivered), "{delivered}");
    }

    #[test]
    fn flip_scheduler_follows_jittered_commits() {
        let pattern = [3.9, 7.8, 8.7, 5.1, 9.2, 6.4, 7.6];
        let mut t = 0.0;
        let flips: Vec<f64> = (0..)
            .map(|i| {
                t += pattern[i % pattern.len()];
                t
            })
            .take_while(|&t| t < 10_000.0)
            .collect();
        let sim = simulate(&flips, 10_000, false);
        assert!(
            (1190..=1201).contains(&sim.captures.len()),
            "{}",
            sim.captures.len()
        );
        let mean = sim.ages.iter().sum::<f64>() / sim.ages.len() as f64;
        assert!(mean < 1.5, "mean flip age {mean} ms");
    }

    #[test]
    fn flip_scheduler_never_strands_the_last_update() {
        let sim = simulate(&[1.0, 2.0, 3.0], 100, false);
        let last = *sim.captures.last().unwrap();
        assert!(
            (3.0..=3.0 + 1.5 * 1000.0 / 120.0 + 1.0).contains(&last),
            "{last}"
        );
    }

    #[test]
    fn flip_scheduler_static_screen_sends_keepalives_and_counts_idle_slots() {
        let sim = simulate(&[], 1_000, false);
        assert_eq!(sim.captures, vec![0.0, 250.0, 500.0, 750.0]);
        let delivered = sim.captures.len() as u32 + sim.idle_slots;
        assert!((118..=121).contains(&delivered), "{delivered}");
    }

    #[test]
    fn flip_scheduler_without_damage_skip_holds_target_rate() {
        let sim = simulate(&[], 10_000, true);
        assert!(
            (1190..=1201).contains(&sim.captures.len()),
            "{}",
            sim.captures.len()
        );
    }

    /// Age of each delivered frame's content (time since the scanout flip it
    /// shows, seen by an independent 200 µs poller) on the real capture loop.
    /// `ST_TEST_FLIP_AGE=1 sudo -E <test-binary> live_capture_flip_age --nocapture`
    #[test]
    fn live_capture_flip_age() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;
        if std::env::var_os("ST_TEST_FLIP_AGE").is_none() {
            return;
        }
        super::super::super::set_target_fps(120);
        let _claim = super::super::super::Nv12Claim::new();
        let (card, _) = Card::open(false).unwrap();
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .unwrap();
        let plane = find_active_plane(&card).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flips = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let observer = {
            let (stop, flips) = (Arc::clone(&stop), Arc::clone(&flips));
            thread::spawn(move || {
                st_protocol::thread_priority::promote_current_thread(
                    st_protocol::thread_priority::ThreadRole::Capture,
                );
                let mut last = None;
                while !stop.load(Ordering::Relaxed) {
                    let fb = card.get_plane(plane).ok().and_then(|p| p.framebuffer());
                    if fb != last {
                        last = fb;
                        flips.lock().unwrap().push(Instant::now());
                    }
                    thread::sleep(Duration::from_micros(200));
                }
            })
        };
        let (tx, rx) = crossbeam_channel::bounded(2);
        let mut capture = KmsCapture::new();
        capture.start(tx).unwrap();
        thread::sleep(Duration::from_secs(1));
        while rx.try_recv().is_ok() {}
        let begin = Instant::now();
        let mut received = Vec::new();
        while begin.elapsed() < Duration::from_secs(10) {
            if let Ok(frame) = rx.recv_timeout(Duration::from_millis(100)) {
                received.push((frame.captured_at, Instant::now()));
            }
        }
        capture.stop();
        stop.store(true, Ordering::Relaxed);
        observer.join().unwrap();
        let flips = flips.lock().unwrap();
        let flip_rate = flips.iter().filter(|&&f| f >= begin).count() as f64 / 10.0;
        let stats = |mut v: Vec<Duration>| {
            v.sort();
            let n = v.len();
            format!(
                "mean={:.2?} p50={:.2?} p90={:.2?} p99={:.2?}",
                v.iter().sum::<Duration>() / n as u32,
                v[n / 2],
                v[n * 9 / 10],
                v[n * 99 / 100]
            )
        };
        let ages: Vec<Duration> = received
            .iter()
            .filter_map(|&(c, _)| {
                // The observer can see a flip up to one poll after the capture.
                let i = flips.partition_point(|&f| f <= c + Duration::from_micros(200));
                (i > 0).then(|| c.saturating_duration_since(flips[i - 1]))
            })
            .collect();
        let durations = received.iter().map(|&(c, r)| r - c).collect();
        eprintln!(
            "[flip-age] flips {flip_rate:.0}/s delivered {:.0} fps | flip->sample {} | sample->frame {}",
            received.len() as f64 / 10.0,
            stats(ages),
            stats(durations)
        );
    }

    /// Flip-driven pacing reads a framebuffer the moment it is committed, so
    /// its content must already be final: a readback right at the flip must
    /// equal one taken just after, with the GPU saturated (where a late render
    /// would show). Needs an animating window (e.g. `vkcube --wsi wayland`).
    /// `ST_TEST_FLIP_FINAL=1 sudo -E <test-binary> live_flip_content_is_final --nocapture`
    #[test]
    fn live_flip_content_is_final() {
        use super::super::kms_gpu_copy::tests as kms;
        use std::sync::atomic::AtomicBool;
        if std::env::var_os("ST_TEST_FLIP_FINAL").is_none() {
            return;
        }
        st_protocol::thread_priority::promote_current_thread(
            st_protocol::thread_priority::ThreadRole::Capture,
        );
        let (card, node) = Card::open(false).unwrap();
        let node = node.unwrap();
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .unwrap();
        let plane = find_active_plane(&card).unwrap();
        let mut vk = kms::nv12_stabilizer(&node, true);
        let stop = Arc::new(AtomicBool::new(false));
        let load = kms::spawn_gpu_load(node.clone(), 2560, 1440, Arc::clone(&stop));
        let fb = || card.get_plane(plane).ok().and_then(|p| p.framebuffer());
        let mut last = fb();
        let (mut flips, mut compared, mut mismatched) = (0, 0, 0);
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end {
            thread::sleep(Duration::from_micros(250));
            let now = fb();
            if now == last {
                continue;
            }
            last = now;
            flips += 1;
            let Ok(frame) = capture_frame(&card, plane, None, None) else {
                continue;
            };
            let (Ok(a), Ok(b)) = (
                stabilize_frame(&mut vk, &frame),
                stabilize_frame(&mut vk, &frame),
            ) else {
                continue;
            };
            if fb() != now {
                continue;
            }
            let (FrameData::RamNv12(a), FrameData::RamNv12(b)) = (&a.data, &b.data) else {
                panic!("expected NV12");
            };
            compared += 1;
            if a[..] != b[..] {
                mismatched += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        load.join().unwrap();
        eprintln!("[flip-final] flips={flips} compared={compared} mismatched={mismatched}");
        assert!(compared >= 20, "too few flips to judge; animate the screen");
        assert_eq!(mismatched, 0, "scanout changed after its flip was seen");
    }

    #[test]
    fn fnv1a_is_stable_and_nonzero() {
        assert_eq!(fnv1a_u32(b"HDMI-A-1"), fnv1a_u32(b"HDMI-A-1"));
        assert_ne!(fnv1a_u32(b"HDMI-A-1"), fnv1a_u32(b"DP-2"));
        assert_ne!(fnv1a_u32(b""), 0);
    }

    #[test]
    fn copy_from_wc_matches_plain_copy_at_any_alignment() {
        let src: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
        for offset in [0, 1, 15, 16, 63, 64, 100] {
            for len in [0, 1, 63, 64, 65, 1000, 3000] {
                let mut out = vec![9u8; 5];
                unsafe { copy_from_wc(&mut out, src.as_ptr().add(offset), len) };
                assert_eq!(out[..5], [9; 5]);
                assert_eq!(
                    out[5..],
                    src[offset..offset + len],
                    "offset {offset} len {len}"
                );
            }
        }
    }

    /// Capture must not leak GETFB2's GEM handles, the cursor plane must be
    /// found, and reading its image must not stall the capture thread (root).
    /// `ST_TEST_KMS_CURSOR=1 sudo -E <test-binary> live_cursor_and_handles --nocapture`
    #[test]
    fn live_cursor_and_handles() {
        if std::env::var_os("ST_TEST_KMS_CURSOR").is_none() {
            return;
        }
        let (card, _) = Card::open(false).unwrap();
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .unwrap();
        let plane = find_active_plane(&card).unwrap();
        for _ in 0..100 {
            drop(capture_frame(&card, plane, None, None).unwrap());
        }
        let fb = card.get_plane(plane).unwrap().framebuffer().unwrap();
        let handles = card.get_planar_framebuffer(fb).unwrap().buffers();
        close_gem_handles(&card, &handles);
        let next = u32::from(handles[0].unwrap());
        assert!(
            next <= 4,
            "GEM handles leak: handle {next} after 100 captures"
        );

        let cursor = find_cursor_plane(&card, plane).expect("cursor plane");
        let mut times = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            if capture_cursor(&card, cursor, None).is_none() {
                eprintln!("[cursor] hidden; read cost not measured");
                return;
            }
            times.push(t.elapsed());
        }
        times.sort();
        eprintln!("[cursor] read p50={:.2?} max={:.2?}", times[10], times[19]);
        assert!(
            times[10] < Duration::from_millis(3),
            "cursor read {:.2?}",
            times[10]
        );
    }
}
