use super::super::{CaptureBackend, CapturedCursor, CapturedFrame, DmaBufPlane, FrameData};
use super::kms_gpu_copy::KmsStabilizer;
use super::target_frame_interval;
use crossbeam_channel::{Sender, TrySendError};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
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
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let card = Card(file);
            if card.get_driver().is_err() {
                continue;
            }

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

/// Read the cursor's on-screen position from atomic plane properties.
///
/// Returns `(x, y)` in CRTC pixels. Atomic drivers (AMD/Intel) expose `CRTC_X`/
/// `CRTC_Y`; NVIDIA drives the cursor through the *legacy* DRM cursor API
/// (`drmModeMoveCursor`) and exposes none of the `CRTC_*` props, and drm-rs
/// doesn't surface `drmModeGetPlane`'s legacy `crtc_x/y`. There is no readback
/// path for the legacy position, so we fall back to `(0, 0)`: in hover-absolute
/// the client renders the cursor at its own local pointer and only needs the
/// shape + a visible flag, so an unknown server position is harmless there.
fn read_cursor_position(card: &Card, cursor_handle: control::plane::Handle) -> (i32, i32) {
    let Ok(props) = card.get_properties(cursor_handle) else {
        return (0, 0);
    };
    let mut crtc_x = 0;
    let mut crtc_y = 0;
    for (prop_id, value) in props.iter() {
        if let Ok(info) = card.get_property(*prop_id) {
            match info.name().to_str() {
                Ok("CRTC_X") => crtc_x = *value as i32,
                Ok("CRTC_Y") => crtc_y = *value as i32,
                _ => {}
            }
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
}

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

    let (x, y) = read_cursor_position(card, cursor_handle);

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
    let fd = match card.buffer_to_prime_fd(gem_handle, 0x02) {
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
        let start = row * pitch as usize;
        let slice = unsafe { std::slice::from_raw_parts(src.add(start), row_bytes) };
        pixels.extend_from_slice(slice);
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
        FrameData::Ram(_) => return Err("stabilizer expects DMA-BUF frames".into()),
    };
    Ok(CapturedFrame {
        data,
        width: frame.width,
        height: frame.height,
        cursor: frame.cursor.clone(),
        // Preserve a keyframe demand set on the source frame by the loop.
        force_keyframe: frame.force_keyframe,
    })
}

fn capture_frame(
    card: &Card,
    plane_handle: control::plane::Handle,
    cursor_handle: Option<control::plane::Handle>,
    cursor_cache: Option<&mut CursorCache>,
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
    for (i, gem) in gem_buffers.iter().enumerate() {
        let gem_handle = match gem {
            Some(h) => *h,
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

        // Find an active plane to validate we can capture
        let plane_handle = match target_crtc {
            Some(crtc) => find_plane_for_crtc(&card, crtc)
                .ok_or("No plane bound to the selected output's CRTC")?,
            None => find_active_plane(&card)?,
        };
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
        let test_frame = capture_frame(&card, plane_handle, cursor_handle, None)
            .map_err(|e| format!("KMS test capture failed (not DRM master?): {e}"))?;
        println!(
            "[kms] Test capture OK ({}x{})",
            test_frame.width, test_frame.height
        );

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let copy_render_node = self.render_node.clone();
        let copy_enabled = kms_copy_enabled();

        let handle = thread::spawn(move || {
            let target_interval = target_frame_interval();
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

            // Prefer a kernel timerfd pacer so the capture cadence doesn't drift
            // under load the way `thread::sleep(remainder)` does. If the kernel
            // rejects the syscall for some reason, fall back to the legacy
            // sleep-based loop.
            let mut pacer = TimerFdPacer::new(target_interval).ok();
            if pacer.is_none() {
                eprintln!("[kms] timerfd unavailable; falling back to sleep-based pacer");
            }

            // C4: cache the resolved plane handle. Plane handles are stable; only
            // the framebuffer bound to a plane flips. So we reuse the validated
            // handle and only re-walk all planes (the N+N×M `type`-property reads)
            // when a capture actually fails — i.e. a modeset moved the binding.
            let mut current_plane = plane_handle;
            // C5: per-thread cursor dirty-tracking cache.
            let mut cursor_cache = CursorCache::default();
            // C9: throttle for capture-overrun (coalesced timerfd expiration) logs.
            let mut last_overshoot_log: Option<Instant> = None;

            while running.load(Ordering::SeqCst) {
                let frame_start = Instant::now();

                match capture_frame(&card, current_plane, cursor_handle, Some(&mut cursor_cache)) {
                    Ok(mut frame) => {
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

                        if let Some(frame) = to_send {
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

                match pacer.as_mut() {
                    Some(p) => {
                        // Block on the timerfd; coalesced expirations (capture was
                        // slower than the target interval) mean we skip ahead.
                        match p.wait() {
                            // C9: >1 expiration = capture overran the frame
                            // interval and the kernel coalesced the missed ticks.
                            Ok(expirations) if expirations > 1 => {
                                let now = Instant::now();
                                if last_overshoot_log
                                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2))
                                {
                                    eprintln!(
                                        "[kms] capture overran frame interval by {} tick(s) \
                                         (encode/copy too slow for {:?} cadence)",
                                        expirations - 1,
                                        target_interval
                                    );
                                    last_overshoot_log = Some(now);
                                }
                            }
                            _ => {}
                        }
                    }
                    None => {
                        let elapsed = frame_start.elapsed();
                        if elapsed < target_interval {
                            thread::sleep(target_interval - elapsed);
                        }
                    }
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

/// Periodic kernel timer wrapping `timerfd_create` / `timerfd_settime`. Provides
/// a drift-free pacing source for the capture loop — `read()` blocks until the
/// next expiration and returns the number of missed ticks if capture overran
/// the target interval.
struct TimerFdPacer {
    fd: OwnedFd,
}

impl TimerFdPacer {
    fn new(interval: Duration) -> io::Result<Self> {
        let raw = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let spec = libc::itimerspec {
            it_interval: duration_to_timespec(interval),
            it_value: duration_to_timespec(interval),
        };
        let rc = unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Block until the next expiration. Returns the number of expirations that
    /// occurred since the last read (>= 1). EINTR is transparently retried.
    fn wait(&mut self) -> io::Result<u64> {
        let mut expirations: u64 = 0;
        loop {
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    &mut expirations as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            return Ok(expirations);
        }
    }
}

fn duration_to_timespec(d: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: d.as_secs() as libc::time_t,
        tv_nsec: d.subsec_nanos() as libc::c_long,
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

    #[test]
    fn fnv1a_is_stable_and_nonzero() {
        assert_eq!(fnv1a_u32(b"HDMI-A-1"), fnv1a_u32(b"HDMI-A-1"));
        assert_ne!(fnv1a_u32(b"HDMI-A-1"), fnv1a_u32(b"DP-2"));
        assert_ne!(fnv1a_u32(b""), 0);
    }
}
