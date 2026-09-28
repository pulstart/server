use crossbeam_channel::Sender;
use st_protocol::control::OutputInfo;

#[cfg(target_os = "windows")]
use ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

static TARGET_FPS: AtomicU32 = AtomicU32::new(60);
static CAPTURE_KICK: AtomicBool = AtomicBool::new(false);

/// Ask a damage-driven capture loop for a frame now: a keyframe request on a
/// static screen would otherwise wait out the keepalive.
pub fn kick_capture() {
    CAPTURE_KICK.store(true, Ordering::Release);
}

#[cfg(target_os = "linux")]
pub fn take_capture_kick() -> bool {
    CAPTURE_KICK.swap(false, Ordering::AcqRel)
}

#[cfg(target_os = "linux")]
static UNCHANGED_CAPTURE_TICKS: AtomicU32 = AtomicU32::new(0);

/// Intentional damage skips are successful capture opportunities, not overload.
#[cfg(target_os = "linux")]
pub fn record_unchanged_capture_tick() {
    UNCHANGED_CAPTURE_TICKS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub fn take_unchanged_capture_ticks() -> u32 {
    UNCHANGED_CAPTURE_TICKS.swap(0, Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
static NV12_RAM_CLAIMS: AtomicU32 = AtomicU32::new(0);

/// Held by an encoder that consumes [`FrameData::RamNv12`]. While any claim is
/// alive, backends that already convert on the GPU emit NV12 instead of BGRA,
/// cutting readback 62% and skipping a CPU colour conversion.
#[cfg(target_os = "linux")]
pub struct Nv12Claim(());

#[cfg(target_os = "linux")]
impl Nv12Claim {
    pub fn new() -> Self {
        NV12_RAM_CLAIMS.fetch_add(1, Ordering::AcqRel);
        Self(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for Nv12Claim {
    fn drop(&mut self) {
        NV12_RAM_CLAIMS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(target_os = "linux")]
pub fn nv12_ram_preferred() -> bool {
    NV12_RAM_CLAIMS.load(Ordering::Acquire) > 0
}

/// A single plane of a DMA-BUF (GPU-accessible buffer exported via DRM).
#[cfg(target_os = "linux")]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

#[cfg(target_os = "linux")]
pub trait FrameLeaseOps: Send {
    fn release(&mut self);
}

#[cfg(target_os = "linux")]
pub struct FrameLease {
    inner: Option<Box<dyn FrameLeaseOps>>,
}

#[cfg(target_os = "linux")]
impl FrameLease {
    pub fn new(inner: impl FrameLeaseOps + 'static) -> Self {
        Self {
            inner: Some(Box::new(inner)),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for FrameLease {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            inner.release();
        }
    }
}

/// CPU frame bytes. Pooled buffers return to their [`RamPool`] on drop, so a
/// multi-MB frame is recycled instead of reallocated and page-faulted per frame.
pub struct RamBuf {
    data: Vec<u8>,
    recycle: Option<crossbeam_channel::Sender<Vec<u8>>>,
}

impl std::ops::Deref for RamBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl std::ops::DerefMut for RamBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl From<Vec<u8>> for RamBuf {
    fn from(data: Vec<u8>) -> Self {
        Self {
            data,
            recycle: None,
        }
    }
}

impl Drop for RamBuf {
    fn drop(&mut self) {
        if let Some(recycle) = self.recycle.take() {
            let _ = recycle.try_send(std::mem::take(&mut self.data));
        }
    }
}

/// Producer-side recycler for [`RamBuf`]s of one frame geometry.
pub struct RamPool {
    tx: crossbeam_channel::Sender<Vec<u8>>,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
}

impl Default for RamPool {
    fn default() -> Self {
        // Covers the capture queue plus the frame being encoded; extras drop.
        let (tx, rx) = crossbeam_channel::bounded(8);
        Self { tx, rx }
    }
}

impl RamPool {
    /// A buffer of exactly `len` bytes with unspecified contents; the caller
    /// overwrites all of it.
    pub fn take(&self, len: usize) -> RamBuf {
        let data = loop {
            match self.rx.try_recv() {
                Ok(data) if data.len() == len => break data,
                Ok(_) => continue,
                Err(_) => break vec![0u8; len],
            }
        };
        RamBuf {
            data,
            recycle: Some(self.tx.clone()),
        }
    }

    pub fn copy_from(&self, src: &[u8]) -> RamBuf {
        let mut buf = self.take(src.len());
        buf.copy_from_slice(src);
        buf
    }
}

/// Frame payload: either CPU-accessible bytes or GPU DMA-BUF planes.
pub enum FrameData {
    /// Tightly packed BGRA.
    Ram(RamBuf),
    /// Tightly packed NV12 (BT.709 limited): Y rows then interleaved CbCr rows.
    #[cfg(target_os = "linux")]
    RamNv12(RamBuf),
    #[cfg(target_os = "linux")]
    DmaBuf {
        planes: Vec<DmaBufPlane>,
        drm_format: u32,
        _lease: Option<FrameLease>,
    },
    #[cfg(target_os = "windows")]
    D3D11Texture {
        texture: Arc<D3D11FrameTexture>,
        array_index: u32,
    },
}

#[cfg(target_os = "windows")]
pub struct D3D11FrameTexture {
    pub texture: ID3D11Texture2D,
}

/// Cursor image captured alongside the main frame.
///
/// Used by backends that can expose a separate cursor plane or metadata.
/// Today that includes KMS, X11, PipeWire cursor metadata mode, Windows GDI,
/// and macOS ScreenCaptureKit with AppKit cursor extraction.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[derive(Clone)]
pub struct CapturedCursor {
    /// ARGB8888 pixel data (pre-multiplied alpha), row-major.
    pub pixels: Arc<[u8]>,
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
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    pub cursor: Option<CapturedCursor>,
    /// Set by a capture backend to demand the encoder emit a keyframe for this
    /// frame — used when the captured content discontinuously jumps (e.g. KMS
    /// active-seat / user switch at the same resolution, where a dimension
    /// change wouldn't otherwise trigger a rebuild+IDR). The encode loop ORs
    /// this across drained frames so the request survives frame-dropping.
    pub force_keyframe: bool,
    /// When the content was sampled; latency accounting starts here.
    pub captured_at: std::time::Instant,
}

// SAFETY: The CVPixelBufferRef is retained and owned by this struct.
// On Linux, OwnedFd is Send and Vec<u8> is Send. On Windows, Ram frames are Vec<u8>
// and D3D11 frame textures are carried behind Arc-wrapped COM handles.
unsafe impl Send for CapturedFrame {}

#[cfg(target_os = "linux")]
fn readback_dmabuf_bgrx_plane(
    fd: BorrowedFd<'_>,
    offset: u32,
    stride: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let stride = stride as usize;
    let row_bytes = width as usize * 4;
    if stride < row_bytes {
        return Err(format!(
            "DMA-BUF stride {stride} is smaller than row size {row_bytes}"
        ));
    }

    let mapped_size = offset as usize + stride * height as usize;
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
        return Err(format!(
            "DMA-BUF mmap failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let sync_start: u64 = 5; // DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ
    let sync_end: u64 = 2 | 4; // DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ
    nix::ioctl_write_ptr_bad!(dma_buf_sync, 0x4008_6200u64, u64);
    unsafe {
        let _ = dma_buf_sync(fd.as_raw_fd(), &sync_start);
    }

    let src = unsafe { (mapped as *const u8).add(offset as usize) };
    let mut out = vec![0u8; row_bytes * height as usize];
    if stride == row_bytes {
        unsafe {
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), out.len());
        }
    } else {
        for row in 0..height as usize {
            let src_row = unsafe { src.add(row * stride) };
            let dst_row = row * row_bytes;
            unsafe {
                std::ptr::copy_nonoverlapping(src_row, out[dst_row..].as_mut_ptr(), row_bytes);
            }
        }
    }

    unsafe {
        let _ = dma_buf_sync(fd.as_raw_fd(), &sync_end);
        libc::munmap(mapped, mapped_size);
    }

    Ok(out)
}

#[cfg(target_os = "linux")]
pub fn try_clone_frame_to_ram_bgra(frame: &CapturedFrame) -> Result<Option<Vec<u8>>, String> {
    const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
    const DRM_FORMAT_ARGB8888: u32 = 0x34325241;

    match &frame.data {
        FrameData::Ram(data) => Ok(Some(data.to_vec())),
        FrameData::RamNv12(_) => Ok(None),
        FrameData::DmaBuf {
            planes, drm_format, ..
        } => {
            if !matches!(*drm_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
                return Ok(None);
            }
            let plane = planes
                .first()
                .ok_or_else(|| "DMA-BUF frame has no planes".to_string())?;
            readback_dmabuf_bgrx_plane(
                plane.fd.as_fd(),
                plane.offset,
                plane.pitch,
                frame.width,
                frame.height,
            )
            .map(Some)
        }
    }
}

/// Composite cursor onto a BGRA frame in-place (software alpha blending).
///
/// Cursor pixels are stored as BGRA bytes in memory, which matches the frame
/// memory layout used by the software encode path on little-endian targets.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn composite_cursor(
    frame_data: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    cursor: &CapturedCursor,
) {
    composite_cursor_with_stride(
        frame_data,
        frame_width as usize * 4,
        frame_width,
        frame_height,
        cursor,
    );
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn composite_cursor_with_stride(
    frame_data: &mut [u8],
    frame_stride: usize,
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
            let frame_offset = fy as usize * frame_stride + fx as usize * 4;

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

pub trait CaptureBackend: Send {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String>;
    fn stop(&mut self);

    /// Displays this backend can capture. An empty list means the backend
    /// cannot enumerate outputs (the caller then treats the single active
    /// stream as the only "output" and hides the picker).
    fn list_outputs(&self) -> Vec<OutputInfo> {
        Vec::new()
    }

    /// Select which output to capture, by `OutputInfo::id`. Returns `true` when
    /// the selection actually changed and the capture must be restarted
    /// (`stop()` then `start()`) for it to take effect.
    fn select_output(&mut self, _id: u32) -> bool {
        false
    }
}

pub fn set_target_fps(fps: u32) {
    TARGET_FPS.store(fps.max(1), Ordering::Relaxed);
}

pub fn target_fps() -> u32 {
    TARGET_FPS.load(Ordering::Relaxed).max(1)
}

/// Token bucket pacing frames to a target interval: one credit per interval,
/// banked up to [`FrameCredit::CAP`] so an irregular source (compositor commit
/// jitter) keeps its average rate without being forced onto a fixed grid.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub struct FrameCredit {
    credit: f64,
    refilled_at: std::time::Instant,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl FrameCredit {
    pub const CAP: f64 = 2.0;

    pub fn new(now: std::time::Instant) -> Self {
        Self {
            credit: Self::CAP,
            refilled_at: now,
        }
    }

    /// Adds the credit earned since the last refill; returns what the cap
    /// discarded (frame slots that went unused).
    pub fn refill(&mut self, now: std::time::Instant, interval: std::time::Duration) -> f64 {
        self.credit += now
            .saturating_duration_since(self.refilled_at)
            .as_secs_f64()
            / interval.as_secs_f64();
        self.refilled_at = now;
        let overflow = (self.credit - Self::CAP).max(0.0);
        self.credit -= overflow;
        overflow
    }

    pub fn available(&self) -> f64 {
        self.credit
    }

    /// Time until the balance reaches `level`, as of the last refill.
    pub fn until(&self, level: f64, interval: std::time::Duration) -> std::time::Duration {
        interval.mul_f64((level - self.credit).max(0.0))
    }

    pub fn take(&mut self) {
        self.credit = (self.credit - 1.0).max(-1.0);
    }
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

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod tests {
    use super::FrameCredit;
    use std::time::{Duration, Instant};

    /// The encode gate's use: a 120 fps source into a 60 fps encoder passes
    /// every other frame on time and never loses the latest one.
    #[test]
    fn frame_credit_halves_a_double_rate_source() {
        let base = Instant::now();
        let interval = Duration::from_secs_f64(1.0 / 60.0);
        let mut credit = FrameCredit::new(base);
        let mut passed = 0;
        for i in 0..1200 {
            let now = base + Duration::from_secs_f64(i as f64 / 120.0);
            credit.refill(now, interval);
            if credit.until(0.75, interval).is_zero() {
                credit.take();
                passed += 1;
            }
        }
        assert!((600..=602).contains(&passed), "{passed}");
        credit.refill(base + Duration::from_secs(20), interval);
        assert_eq!(credit.available(), FrameCredit::CAP);
    }
}
