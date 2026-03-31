use super::super::{
    CaptureBackend, CapturedCursor, CapturedFrame, DmaBufPlane, FrameData, FrameLease,
    FrameLeaseOps,
};
use crossbeam_channel::{Sender, TrySendError};
use std::io::Cursor;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;

use drm_fourcc::DrmModifier;
use nix::unistd::dup;
use pipewire as pw;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{ChoiceValue, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use st_protocol::MOUSE_WHEEL_STEP_UNITS;

const PIPEWIRE_CURSOR_META_SIZE: i32 = (std::mem::size_of::<pw::spa::sys::spa_meta_cursor>()
    + std::mem::size_of::<pw::spa::sys::spa_meta_bitmap>()
    + 256 * 256 * 4) as i32;

fn spa_meta_cursor_is_valid_local(cursor: *const pw::spa::sys::spa_meta_cursor) -> bool {
    if cursor.is_null() {
        return false;
    }
    let cursor = unsafe { &*cursor };
    cursor.id != 0
}

fn spa_meta_bitmap_is_valid_local(bitmap: *const pw::spa::sys::spa_meta_bitmap) -> bool {
    if bitmap.is_null() {
        return false;
    }
    let bitmap = unsafe { &*bitmap };
    let bitmap_header_size = std::mem::size_of::<pw::spa::sys::spa_meta_bitmap>() as u32;
    bitmap.format != 0 && (bitmap.offset == 0 || bitmap.offset >= bitmap_header_size)
}

fn copy_mem_ptr_bgrx_frame(
    src: &[u8],
    offset: u32,
    size: u32,
    stride: i32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let row_bytes = width as usize * 4;
    let height = height as usize;
    let offset = offset as usize;
    let available = src
        .len()
        .checked_sub(offset)
        .ok_or_else(|| format!("PipeWire shared-memory offset {offset} exceeds buffer size {}", src.len()))?;
    let valid = if size > 0 {
        available.min(size as usize)
    } else {
        available
    };
    let src = &src[offset..offset + valid];

    if stride < 0 {
        return Err(format!("PipeWire shared-memory stride {stride} is negative"));
    }

    let stride = stride as usize;
    if stride == 0 || stride == row_bytes {
        let needed = row_bytes
            .checked_mul(height)
            .ok_or_else(|| "PipeWire shared-memory frame size overflow".to_string())?;
        if src.len() < needed {
            return Err(format!(
                "PipeWire shared-memory buffer too small: have {}, need {needed}",
                src.len()
            ));
        }
        return Ok(src[..needed].to_vec());
    }

    if stride < row_bytes {
        return Err(format!(
            "PipeWire shared-memory stride {stride} is smaller than row size {row_bytes}"
        ));
    }

    let needed = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| "PipeWire shared-memory layout overflow".to_string())?;
    if src.len() < needed {
        return Err(format!(
            "PipeWire shared-memory buffer too small for stride {stride}: have {}, need {needed}",
            src.len()
        ));
    }

    let mut out = vec![0u8; row_bytes * height];
    for row in 0..height {
        let src_start = row * stride;
        let dst_start = row * row_bytes;
        out[dst_start..dst_start + row_bytes]
            .copy_from_slice(&src[src_start..src_start + row_bytes]);
    }
    Ok(out)
}

/// Map SPA video format to DRM fourcc code.
fn video_format_to_drm_fourcc(fmt: VideoFormat) -> u32 {
    match fmt {
        VideoFormat::BGRx => 0x34325258, // DRM_FORMAT_XRGB8888
        VideoFormat::BGRA => 0x34325241, // DRM_FORMAT_ARGB8888
        VideoFormat::RGBx => 0x34324258, // DRM_FORMAT_XBGR8888
        VideoFormat::RGBA => 0x34324241, // DRM_FORMAT_ABGR8888
        VideoFormat::BGR => 0x20524742,  // DRM_FORMAT_BGR888
        VideoFormat::RGB => 0x20424752,  // DRM_FORMAT_RGB888
        _ => 0x34325258,                 // fallback to XRGB8888
    }
}

fn copy_dmabuf_bgrx_frame(
    fd: BorrowedFd<'_>,
    offset: u32,
    stride: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("PipeWire dmabuf frame has zero dimensions".into());
    }
    let stride = stride as usize;
    let row_bytes = width as usize * 4;
    if stride < row_bytes {
        return Err(format!(
            "PipeWire dmabuf stride {stride} is smaller than row size {row_bytes}"
        ));
    }
    let mapped_size = (offset as usize)
        .checked_add(
            stride
                .checked_mul(height as usize)
                .ok_or_else(|| "PipeWire dmabuf mapped size overflow".to_string())?,
        )
        .ok_or_else(|| "PipeWire dmabuf mapped size overflow".to_string())?;
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
            "PipeWire dmabuf mmap failed: {}",
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

#[derive(Clone, Copy)]
struct NegotiatedVideoInfo {
    width: u32,
    height: u32,
    drm_format: u32,
    modifier: u64,
    prefers_dmabuf: bool,
}

#[derive(Clone, Copy)]
struct PendingBufferRelease {
    stream: *mut pw::sys::pw_stream,
    buffer: *mut pw::sys::pw_buffer,
}

unsafe impl Send for PendingBufferRelease {}

struct PipeWireBufferLease {
    release_tx: pw::channel::Sender<PendingBufferRelease>,
    pending: Option<PendingBufferRelease>,
}

impl FrameLeaseOps for PipeWireBufferLease {
    fn release(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = self.release_tx.send(pending);
        }
    }
}

fn u64_to_spa_long_bits(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

fn build_format_object(
    format: VideoFormat,
    width: u32,
    height: u32,
    prefer_dmabuf: bool,
) -> pw::spa::pod::Object {
    let mut properties = vec![
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Id,
            format
        ),
        Property {
            key: pw::spa::param::format::FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle { width, height },
                    min: Rectangle {
                        width: 1,
                        height: 1,
                    },
                    max: Rectangle {
                        width: 8192,
                        height: 4096,
                    },
                },
            ))),
        },
        Property {
            key: pw::spa::param::format::FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Fraction(Fraction { num: 0, denom: 1 }),
        },
    ];

    if prefer_dmabuf {
        let modifier_flags = PropertyFlags::from_bits_retain(
            pw::spa::sys::SPA_POD_PROP_FLAG_MANDATORY
                | pw::spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE,
        );
        properties.push(Property {
            key: pw::spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: modifier_flags,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: u64_to_spa_long_bits(DrmModifier::Linear.into()),
                    alternatives: vec![u64_to_spa_long_bits(DrmModifier::Linear.into())],
                },
            ))),
        });
    }

    pw::spa::pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    }
}

fn serialize_object_pod(object: &pw::spa::pod::Object) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; 2048];
    pw::spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(&mut buf),
        &Value::Object(object.clone()),
    )
    .map_err(|e| format!("pod serialize: {e:?}"))?;
    Ok(buf)
}

fn build_format_pod_buffers(width: u32, height: u32) -> Result<Vec<Vec<u8>>, String> {
    let mut buffers = Vec::with_capacity(4);
    for (format, prefer_dmabuf) in [
        (VideoFormat::BGRx, true),
        (VideoFormat::BGRA, true),
        (VideoFormat::BGRx, false),
        (VideoFormat::BGRA, false),
    ] {
        let object = build_format_object(format, width, height, prefer_dmabuf);
        buffers.push(serialize_object_pod(&object)?);
    }
    Ok(buffers)
}

fn build_stream_param_buffers(
    prefers_dmabuf: bool,
    include_cursor_meta: bool,
) -> Result<Vec<Vec<u8>>, String> {
    let buffer_types = if prefers_dmabuf {
        1 << pw::spa::sys::SPA_DATA_DmaBuf
    } else {
        1 << pw::spa::sys::SPA_DATA_MemPtr
    };

    let mut pods = Vec::with_capacity(4);
    let buffers = pw::spa::pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(buffer_types),
        }],
    };
    pods.push(serialize_object_pod(&buffers)?);

    let header_meta = pw::spa::pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(pw::spa::sys::SPA_META_Header)),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(std::mem::size_of::<pw::spa::sys::spa_meta_header>() as i32),
            },
        ],
    };
    pods.push(serialize_object_pod(&header_meta)?);

    let damage_meta = pw::spa::pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(pw::spa::sys::SPA_META_VideoDamage)),
            },
            Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int((std::mem::size_of::<pw::spa::sys::spa_meta_region>() * 16) as i32),
            },
        ],
    };
    pods.push(serialize_object_pod(&damage_meta)?);

    if include_cursor_meta {
        let cursor_meta = pw::spa::pod::Object {
            type_: SpaTypes::ObjectParamMeta.as_raw(),
            id: pw::spa::param::ParamType::Meta.as_raw(),
            properties: vec![
                Property {
                    key: pw::spa::sys::SPA_PARAM_META_type,
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(pw::spa::sys::SPA_META_Cursor)),
                },
                Property {
                    key: pw::spa::sys::SPA_PARAM_META_size,
                    flags: PropertyFlags::empty(),
                    value: Value::Int(PIPEWIRE_CURSOR_META_SIZE),
                },
            ],
        };
        pods.push(serialize_object_pod(&cursor_meta)?);
    }

    Ok(pods)
}

fn duplicate_dmabuf_planes(
    datas: &[pw::spa::sys::spa_data],
    modifier: u64,
) -> Result<Vec<DmaBufPlane>, String> {
    let mut planes = Vec::with_capacity(datas.len());
    for data in datas {
        if data.type_ != pw::spa::sys::SPA_DATA_DmaBuf {
            continue;
        }
        if data.chunk.is_null() {
            return Err("PipeWire DmaBuf plane is missing a chunk descriptor".into());
        }
        let raw_fd = i32::try_from(data.fd).map_err(|_| {
            format!("PipeWire DmaBuf fd {} does not fit into a RawFd", data.fd)
        })?;
        let fd = dup(raw_fd)
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
            .map_err(|e| format!("dup dmabuf fd: {e}"))?;
        let pitch = unsafe { (*data.chunk).stride.max(0) as u32 };
        planes.push(DmaBufPlane {
            fd,
            offset: unsafe { (*data.chunk).offset },
            pitch,
            modifier,
        });
    }

    if planes.is_empty() {
        return Err("PipeWire DmaBuf frame did not expose any planes".into());
    }

    Ok(planes)
}

#[derive(Default)]
struct CursorCache {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    hotspot_x: u32,
    hotspot_y: u32,
    shape_serial: u64,
    x: i32,
    y: i32,
    visible: bool,
    has_state: bool,
}

impl CursorCache {
    fn current_cursor(&self) -> Option<CapturedCursor> {
        if !self.has_state {
            return None;
        }
        Some(CapturedCursor {
            pixels: self.pixels.clone().into(),
            x: self.x,
            y: self.y,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            width: self.width,
            height: self.height,
            shape_serial: self.shape_serial,
            visible: self.visible,
        })
    }

    fn mark_hidden(&mut self) -> Option<CapturedCursor> {
        if !self.has_state {
            return None;
        }
        self.visible = false;
        self.current_cursor()
    }
}

fn convert_cursor_bitmap_to_bgra(
    format: u32,
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    if stride < row_bytes {
        return None;
    }

    let mut out = Vec::with_capacity(row_bytes.checked_mul(height)?);
    for row in 0..height {
        let start = row.checked_mul(stride)?;
        let end = start.checked_add(row_bytes)?;
        let row_data = src.get(start..end)?;
        match format {
            pw::spa::sys::SPA_VIDEO_FORMAT_BGRA | pw::spa::sys::SPA_VIDEO_FORMAT_BGRx => {
                out.extend_from_slice(row_data);
                if format == pw::spa::sys::SPA_VIDEO_FORMAT_BGRx {
                    let row_start = out.len() - row_bytes;
                    for alpha in out[(row_start + 3)..].iter_mut().step_by(4) {
                        *alpha = 0xFF;
                    }
                }
            }
            pw::spa::sys::SPA_VIDEO_FORMAT_RGBA | pw::spa::sys::SPA_VIDEO_FORMAT_RGBx => {
                for chunk in row_data.chunks_exact(4) {
                    out.push(chunk[2]);
                    out.push(chunk[1]);
                    out.push(chunk[0]);
                    out.push(if format == pw::spa::sys::SPA_VIDEO_FORMAT_RGBx {
                        0xFF
                    } else {
                        chunk[3]
                    });
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn extract_cursor(
    spa_buffer: *mut pw::spa::sys::spa_buffer,
    cache: &mut CursorCache,
) -> Option<CapturedCursor> {
    if spa_buffer.is_null() {
        return cache.current_cursor();
    }

    let cursor_ptr = unsafe {
        pw::spa::sys::spa_buffer_find_meta_data(
            spa_buffer.cast_const(),
            pw::spa::sys::SPA_META_Cursor,
            std::mem::size_of::<pw::spa::sys::spa_meta_cursor>(),
        ) as *mut pw::spa::sys::spa_meta_cursor
    };
    if cursor_ptr.is_null() {
        return cache.mark_hidden();
    }
    if !spa_meta_cursor_is_valid_local(cursor_ptr.cast_const()) {
        return cache.mark_hidden();
    }

    let cursor = unsafe { &*cursor_ptr };
    cache.shape_serial = cursor.id as u64;
    if cursor.bitmap_offset as usize >= std::mem::size_of::<pw::spa::sys::spa_meta_cursor>() {
        let bitmap_ptr = unsafe {
            (cursor_ptr as *const u8).add(cursor.bitmap_offset as usize)
                as *const pw::spa::sys::spa_meta_bitmap
        };
        if spa_meta_bitmap_is_valid_local(bitmap_ptr) {
            let bitmap = unsafe { &*bitmap_ptr };
            if bitmap.offset == 0 {
                cache.x = cursor.position.x - cache.hotspot_x as i32;
                cache.y = cursor.position.y - cache.hotspot_y as i32;
                cache.visible = false;
                cache.has_state = true;
                return cache.current_cursor();
            }
            let width = bitmap.size.width as usize;
            let height = bitmap.size.height as usize;
            let stride = bitmap.stride.max(0) as usize;
            let total_bytes = stride.checked_mul(height)?;
            let data_ptr =
                unsafe { (bitmap_ptr as *const u8).add(bitmap.offset as usize) as *const u8 };
            let src = unsafe { std::slice::from_raw_parts(data_ptr, total_bytes) };
            let pixels = convert_cursor_bitmap_to_bgra(bitmap.format, src, stride, width, height)?;
            cache.pixels = pixels;
            cache.width = width as u32;
            cache.height = height as u32;
            cache.hotspot_x = cursor.hotspot.x.max(0) as u32;
            cache.hotspot_y = cursor.hotspot.y.max(0) as u32;
        }
    }
    cache.x = cursor.position.x - cache.hotspot_x as i32;
    cache.y = cursor.position.y - cache.hotspot_y as i32;
    cache.visible = true;
    cache.has_state = true;
    cache.current_cursor()
}

/// Portal + PipeWire capture for Wayland desktops.
pub struct PipeWireCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    quit_tx: Option<pw::channel::Sender<()>>,
}

impl PipeWireCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
            quit_tx: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Restore token persistence
// ---------------------------------------------------------------------------

fn token_path() -> PathBuf {
    let state_dir = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        });
    state_dir.join("st").join("portal_token")
}

fn load_restore_token() -> Option<String> {
    let path = token_path();
    match std::fs::read_to_string(&path) {
        Ok(token) => {
            let token = token.trim().to_string();
            if token.is_empty() {
                None
            } else {
                println!(
                    "[capture] Loaded portal restore token from {}",
                    path.display()
                );
                Some(token)
            }
        }
        Err(_) => None,
    }
}

fn save_restore_token(token: &str) {
    let path = token_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, token) {
        Ok(()) => println!("[capture] Saved portal restore token to {}", path.display()),
        Err(e) => eprintln!("[capture] Failed to save restore token: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Raw D-Bus portal interaction
//
// Pattern per call:
//   1. Generate handle_token and predict the request object path
//   2. Subscribe to Response signal on that path (BEFORE calling the method)
//   3. Call the portal method (returns immediately with request handle)
//   4. Wait for the Response signal (carries response_code + results dict)
// ---------------------------------------------------------------------------

/// Result of the portal ScreenCast session.
/// Keep the runtime, D-Bus connection, and session path alive for the full
/// lifetime of the PipeWire node so we can explicitly close the portal session
/// on teardown.
struct PortalSession {
    pw_fd: Option<OwnedFd>,
    node_id: u32,
    logical_width: u32,
    logical_height: u32,
    runtime: tokio::runtime::Runtime,
    connection: zbus::Connection,
    session_path: String,
}

impl PortalSession {
    fn take_pw_fd(&mut self) -> OwnedFd {
        self
            .pw_fd
            .take()
            .expect("portal session PipeWire fd already taken")
    }
}

enum EitherPortalSession {
    ScreenCast(PortalSession),
    RemoteDesktop {
        session: Arc<RemoteDesktopPortalSession>,
        pw_fd: OwnedFd,
        node_id: u32,
        logical_width: u32,
        logical_height: u32,
    },
}

struct PortalStreamInfo {
    node_id: u32,
    logical_width: f64,
    logical_height: f64,
}

struct RemoteDesktopPortalState {
    runtime: tokio::runtime::Runtime,
    connection: zbus::Connection,
}

pub(crate) struct RemoteDesktopPortalSession {
    state: Mutex<RemoteDesktopPortalState>,
    session_path: String,
    stream_node_id: u32,
    logical_width: Mutex<f64>,
    logical_height: Mutex<f64>,
    tracked_pos: Mutex<Option<(f64, f64)>>,
    pending_scroll_units: Mutex<(i32, i32)>,
}

impl RemoteDesktopPortalSession {
    fn new(
        runtime: tokio::runtime::Runtime,
        connection: zbus::Connection,
        session_path: String,
        stream_info: PortalStreamInfo,
    ) -> Self {
        Self {
            state: Mutex::new(RemoteDesktopPortalState { runtime, connection }),
            session_path,
            stream_node_id: stream_info.node_id,
            logical_width: Mutex::new(stream_info.logical_width),
            logical_height: Mutex::new(stream_info.logical_height),
            tracked_pos: Mutex::new(None),
            pending_scroll_units: Mutex::new((0, 0)),
        }
    }

    pub(crate) fn set_logical_size(&self, width: u32, height: u32) {
        let mut changed = false;
        if width > 0 {
            let mut w = self.logical_width.lock().unwrap();
            if *w != width as f64 {
                *w = width as f64;
                changed = true;
            }
        }
        if height > 0 {
            let mut h = self.logical_height.lock().unwrap();
            if *h != height as f64 {
                *h = height as f64;
                changed = true;
            }
        }
        // Reset tracked cursor position so the next absolute move re-establishes
        // the initial position via NotifyPointerMotionAbsolute, avoiding stale
        // deltas computed from the old resolution.
        if changed {
            *self.tracked_pos.lock().unwrap() = None;
        }
    }

    fn with_remote_desktop_proxy<R>(
        &self,
        f: impl FnOnce(
            &tokio::runtime::Runtime,
            &zbus::Connection,
            &str,
        ) -> Result<R, String>,
    ) -> Result<R, String> {
        let state = self.state.lock().unwrap();
        f(&state.runtime, &state.connection, &self.session_path)
    }

    pub(crate) fn notify_pointer_motion_absolute(&self, x: u16, y: u16) -> Result<(), String> {
        let width = (*self.logical_width.lock().unwrap()).max(1.0);
        let height = (*self.logical_height.lock().unwrap()).max(1.0);
        let target_x = (x as f64 / 65535.0) * (width - 1.0).max(0.0);
        let target_y = (y as f64 / 65535.0) * (height - 1.0).max(0.0);
        let mut tracked = self.tracked_pos.lock().unwrap();
        if let Some((prev_x, prev_y)) = *tracked {
            let dx = target_x - prev_x;
            let dy = target_y - prev_y;
            *tracked = Some((target_x, target_y));
            drop(tracked);
            if dx.abs() < 0.001 && dy.abs() < 0.001 {
                return Ok(());
            }
            self.with_remote_desktop_proxy(|runtime, connection, session_path| {
                runtime.block_on(async {
                    let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                        .destination("org.freedesktop.portal.Desktop")
                        .map_err(|e| format!("portal dest: {e}"))?
                        .path("/org/freedesktop/portal/desktop")
                        .map_err(|e| format!("portal path: {e}"))?
                        .interface("org.freedesktop.portal.RemoteDesktop")
                        .map_err(|e| format!("portal iface: {e}"))?
                        .build()
                        .await
                        .map_err(|e| format!("portal proxy: {e}"))?;
                    let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                    let session = zvariant::ObjectPath::try_from(session_path)
                        .map_err(|e| format!("session path: {e}"))?;
                    let _: () = proxy
                        .call(
                            "NotifyPointerMotion",
                            &(&session, opts, dx, dy),
                        )
                        .await
                        .map_err(|e| format!("NotifyPointerMotion: {e}"))?;
                    Ok(())
                })
            })
        } else {
            *tracked = Some((target_x, target_y));
            drop(tracked);
            self.with_remote_desktop_proxy(|runtime, connection, session_path| {
                runtime.block_on(async {
                    let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                        .destination("org.freedesktop.portal.Desktop")
                        .map_err(|e| format!("portal dest: {e}"))?
                        .path("/org/freedesktop/portal/desktop")
                        .map_err(|e| format!("portal path: {e}"))?
                        .interface("org.freedesktop.portal.RemoteDesktop")
                        .map_err(|e| format!("portal iface: {e}"))?
                        .build()
                        .await
                        .map_err(|e| format!("portal proxy: {e}"))?;
                    let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                    let session = zvariant::ObjectPath::try_from(session_path)
                        .map_err(|e| format!("session path: {e}"))?;
                    let _: () = proxy
                        .call(
                            "NotifyPointerMotionAbsolute",
                            &(&session, opts, self.stream_node_id, target_x, target_y),
                        )
                        .await
                        .map_err(|e| format!("NotifyPointerMotionAbsolute: {e}"))?;
                    Ok(())
                })
            })
        }
    }

    pub(crate) fn notify_pointer_motion_relative(&self, dx: i16, dy: i16) -> Result<(), String> {
        {
            let width = (*self.logical_width.lock().unwrap()).max(1.0);
            let height = (*self.logical_height.lock().unwrap()).max(1.0);
            let mut tracked = self.tracked_pos.lock().unwrap();
            if let Some((ref mut tx, ref mut ty)) = *tracked {
                *tx = (*tx + dx as f64).clamp(0.0, (width - 1.0).max(0.0));
                *ty = (*ty + dy as f64).clamp(0.0, (height - 1.0).max(0.0));
            }
        }
        self.with_remote_desktop_proxy(|runtime, connection, session_path| {
            runtime.block_on(async {
                let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                    .destination("org.freedesktop.portal.Desktop")
                    .map_err(|e| format!("portal dest: {e}"))?
                    .path("/org/freedesktop/portal/desktop")
                    .map_err(|e| format!("portal path: {e}"))?
                    .interface("org.freedesktop.portal.RemoteDesktop")
                    .map_err(|e| format!("portal iface: {e}"))?
                    .build()
                    .await
                    .map_err(|e| format!("portal proxy: {e}"))?;
                let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                let session = zvariant::ObjectPath::try_from(session_path)
                    .map_err(|e| format!("session path: {e}"))?;
                let _: () = proxy
                    .call(
                        "NotifyPointerMotion",
                        &(&session, opts, dx as f64, dy as f64),
                    )
                    .await
                    .map_err(|e| format!("NotifyPointerMotion: {e}"))?;
                Ok(())
            })
        })
    }

    pub(crate) fn notify_pointer_button(&self, button: u16, pressed: bool) -> Result<(), String> {
        self.with_remote_desktop_proxy(|runtime, connection, session_path| {
            runtime.block_on(async {
                let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                    .destination("org.freedesktop.portal.Desktop")
                    .map_err(|e| format!("portal dest: {e}"))?
                    .path("/org/freedesktop/portal/desktop")
                    .map_err(|e| format!("portal path: {e}"))?
                    .interface("org.freedesktop.portal.RemoteDesktop")
                    .map_err(|e| format!("portal iface: {e}"))?
                    .build()
                    .await
                    .map_err(|e| format!("portal proxy: {e}"))?;
                let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                let session = zvariant::ObjectPath::try_from(session_path)
                    .map_err(|e| format!("session path: {e}"))?;
                let _: () = proxy
                    .call(
                        "NotifyPointerButton",
                        &(&session, opts, button as i32, if pressed { 1u32 } else { 0u32 }),
                    )
                    .await
                    .map_err(|e| format!("NotifyPointerButton: {e}"))?;
                Ok(())
            })
        })
    }

    pub(crate) fn notify_pointer_axis_discrete(
        &self,
        delta_x: i16,
        delta_y: i16,
    ) -> Result<(), String> {
        self.with_remote_desktop_proxy(|runtime, connection, session_path| {
            runtime.block_on(async {
                let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                    .destination("org.freedesktop.portal.Desktop")
                    .map_err(|e| format!("portal dest: {e}"))?
                    .path("/org/freedesktop/portal/desktop")
                    .map_err(|e| format!("portal path: {e}"))?
                    .interface("org.freedesktop.portal.RemoteDesktop")
                    .map_err(|e| format!("portal iface: {e}"))?
                    .build()
                    .await
                    .map_err(|e| format!("portal proxy: {e}"))?;
                let session = zvariant::ObjectPath::try_from(session_path)
                    .map_err(|e| format!("session path: {e}"))?;
                let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                if delta_y != 0 {
                    let _: () = proxy
                        .call(
                            "NotifyPointerAxisDiscrete",
                            &(&session, opts.clone(), 0u32, delta_y as i32),
                        )
                        .await
                        .map_err(|e| format!("NotifyPointerAxisDiscrete(vertical): {e}"))?;
                }
                if delta_x != 0 {
                    let _: () = proxy
                        .call(
                            "NotifyPointerAxisDiscrete",
                            &(&session, opts, 1u32, delta_x as i32),
                        )
                        .await
                        .map_err(|e| format!("NotifyPointerAxisDiscrete(horizontal): {e}"))?;
                }
                Ok(())
            })
        })
    }

    pub(crate) fn notify_pointer_axis_units(
        &self,
        delta_x: i16,
        delta_y: i16,
    ) -> Result<(), String> {
        let (step_x, step_y) = {
            let mut pending = self.pending_scroll_units.lock().unwrap();
            pending.0 += i32::from(delta_x);
            pending.1 += i32::from(delta_y);
            let step_units = i32::from(MOUSE_WHEEL_STEP_UNITS);
            let step_x = (pending.0 / step_units)
                .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            let step_y = (pending.1 / step_units)
                .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            pending.0 -= i32::from(step_x) * step_units;
            pending.1 -= i32::from(step_y) * step_units;
            (step_x, step_y)
        };
        if step_x == 0 && step_y == 0 {
            return Ok(());
        }
        self.notify_pointer_axis_discrete(step_x, step_y)
    }

    pub(crate) fn notify_keyboard_keycode(
        &self,
        keycode: u16,
        pressed: bool,
    ) -> Result<(), String> {
        self.with_remote_desktop_proxy(|runtime, connection, session_path| {
            runtime.block_on(async {
                let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
                    .destination("org.freedesktop.portal.Desktop")
                    .map_err(|e| format!("portal dest: {e}"))?
                    .path("/org/freedesktop/portal/desktop")
                    .map_err(|e| format!("portal path: {e}"))?
                    .interface("org.freedesktop.portal.RemoteDesktop")
                    .map_err(|e| format!("portal iface: {e}"))?
                    .build()
                    .await
                    .map_err(|e| format!("portal proxy: {e}"))?;
                let opts = std::collections::HashMap::<&str, zvariant::Value<'_>>::new();
                let session = zvariant::ObjectPath::try_from(session_path)
                    .map_err(|e| format!("session path: {e}"))?;
                let _: () = proxy
                    .call(
                        "NotifyKeyboardKeycode",
                        &(&session, opts, keycode as i32, if pressed { 1u32 } else { 0u32 }),
                    )
                    .await
                    .map_err(|e| format!("NotifyKeyboardKeycode: {e}"))?;
                Ok(())
            })
        })
    }
}

fn close_portal_session(
    runtime: &tokio::runtime::Runtime,
    connection: &zbus::Connection,
    session_path: &str,
) {
    if session_path.is_empty() {
        return;
    }

    let close_result = runtime.block_on(async {
        let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(connection)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("portal dest: {e}"))?
            .path(session_path)
            .map_err(|e| format!("session path: {e}"))?
            .interface("org.freedesktop.portal.Session")
            .map_err(|e| format!("session iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("session proxy: {e}"))?;
        let _: () = proxy
            .call("Close", &())
            .await
            .map_err(|e| format!("Session.Close: {e}"))?;
        Ok::<(), String>(())
    });

    match close_result {
        Ok(()) => {
            println!("[capture] Closed portal session {session_path}");
        }
        Err(err) => {
            eprintln!("[capture] Failed to close portal session {session_path}: {err}");
        }
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        close_portal_session(&self.runtime, &self.connection, &self.session_path);
    }
}

impl Drop for RemoteDesktopPortalSession {
    fn drop(&mut self) {
        match self.state.get_mut() {
            Ok(state) => close_portal_session(&state.runtime, &state.connection, &self.session_path),
            Err(poisoned) => {
                let state = poisoned.into_inner();
                close_portal_session(&state.runtime, &state.connection, &self.session_path);
            }
        }
    }
}

static ACTIVE_REMOTE_DESKTOP_SESSION: OnceLock<Mutex<Option<Arc<RemoteDesktopPortalSession>>>> =
    OnceLock::new();

fn active_remote_desktop_slot() -> &'static Mutex<Option<Arc<RemoteDesktopPortalSession>>> {
    ACTIVE_REMOTE_DESKTOP_SESSION.get_or_init(|| Mutex::new(None))
}

pub(crate) fn active_remote_desktop_session() -> Option<Arc<RemoteDesktopPortalSession>> {
    active_remote_desktop_slot().lock().unwrap().clone()
}

fn set_active_remote_desktop_session(session: Arc<RemoteDesktopPortalSession>) {
    *active_remote_desktop_slot().lock().unwrap() = Some(session);
}

fn clear_active_remote_desktop_session(session: &Arc<RemoteDesktopPortalSession>) {
    let mut slot = active_remote_desktop_slot().lock().unwrap();
    if slot
        .as_ref()
        .map(|active| Arc::ptr_eq(active, session))
        .unwrap_or(false)
    {
        *slot = None;
    }
}

fn request_remote_desktop_screencast(
) -> Result<(Arc<RemoteDesktopPortalSession>, OwnedFd, u32, u32, u32), String> {
    let restore_token = load_restore_token();
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;

    let (conn, session_path, pw_fd, stream_info) = rt.block_on(async {
        use futures_lite::StreamExt;
        use std::collections::HashMap;
        use zvariant::{ObjectPath, OwnedObjectPath, Value};

        let conn = zbus::Connection::session()
            .await
            .map_err(|e| format!("D-Bus session bus: {e}"))?;

        let unique_name = conn
            .unique_name()
            .ok_or("D-Bus connection has no unique name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");

        let mut token_counter: u32 = 0;
        let mut next_token = || -> String {
            token_counter += 1;
            format!("st{token_counter}")
        };

        let remote_desktop = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("proxy dest: {e}"))?
            .path("/org/freedesktop/portal/desktop")
            .map_err(|e| format!("proxy path: {e}"))?
            .interface("org.freedesktop.portal.RemoteDesktop")
            .map_err(|e| format!("proxy iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("remote desktop proxy: {e}"))?;
        let screen_cast = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("proxy dest: {e}"))?
            .path("/org/freedesktop/portal/desktop")
            .map_err(|e| format!("proxy path: {e}"))?
            .interface("org.freedesktop.portal.ScreenCast")
            .map_err(|e| format!("proxy iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("screen cast proxy: {e}"))?;

        let session_token = next_token();
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe CreateSession Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));
        let _reply: OwnedObjectPath = remote_desktop
            .call("CreateSession", &(opts,))
            .await
            .map_err(|e| format!("CreateSession call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("CreateSession: Response stream ended")?;
        let (code, results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("CreateSession denied (code {code})"));
        }
        drop(response_stream);
        drop(request_proxy);

        let session_path = results
            .get("session_handle")
            .and_then(|v| try_extract_string(v))
            .unwrap_or_else(|| {
                format!("/org/freedesktop/portal/desktop/session/{unique_name}/{session_token}")
            });
        let session_obj = ObjectPath::try_from(session_path.as_str())
            .map_err(|e| format!("session path: {e}"))?;

        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe SelectDevices Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("types", Value::U32(0b11));
        opts.insert("persist_mode", Value::U32(2));
        if let Some(ref token) = restore_token {
            opts.insert("restore_token", Value::from(token.as_str()));
        }
        let _reply: OwnedObjectPath = remote_desktop
            .call("SelectDevices", &(&session_obj, opts))
            .await
            .map_err(|e| format!("SelectDevices call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("SelectDevices: Response stream ended")?;
        let (code, _) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("SelectDevices denied (code {code})"));
        }
        drop(response_stream);
        drop(request_proxy);

        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe SelectSources Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("types", Value::U32(1));
        opts.insert("cursor_mode", Value::U32(4));
        opts.insert("multiple", Value::Bool(false));
        let _reply: OwnedObjectPath = screen_cast
            .call("SelectSources", &(&session_obj, opts))
            .await
            .map_err(|e| format!("SelectSources call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("SelectSources: Response stream ended")?;
        let (code, _) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("SelectSources denied (code {code})"));
        }
        drop(response_stream);
        drop(request_proxy);

        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe Start Response: {e}"))?;

        let opts: HashMap<&str, Value<'_>> =
            [("handle_token", Value::from(request_token.as_str()))]
                .into_iter()
                .collect();
        let _reply: OwnedObjectPath = remote_desktop
            .call("Start", &(&session_obj, "", opts))
            .await
            .map_err(|e| format!("Start call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("Start: Response stream ended")?;
        let (code, start_results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("Start denied (code {code})"));
        }
        drop(response_stream);
        drop(request_proxy);

        if let Some(token) = start_results
            .get("restore_token")
            .and_then(|v| try_extract_string(v))
        {
            save_restore_token(&token);
        }

        let stream_info = start_results
            .get("streams")
            .ok_or_else(|| "No streams in Start response".to_string())
            .and_then(extract_first_stream_info)?;

        let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = screen_cast
            .call_method("OpenPipeWireRemote", &(&session_obj, empty_opts))
            .await
            .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
        let pw_fd: OwnedFd = reply
            .body()
            .deserialize::<zvariant::OwnedFd>()
            .map_err(|e| format!("OpenPipeWireRemote fd: {e}"))?
            .into();

        Ok((conn, session_path, pw_fd, stream_info))
    })?;

    let session = Arc::new(RemoteDesktopPortalSession::new(
        rt,
        conn,
        session_path,
        PortalStreamInfo {
            node_id: stream_info.node_id,
            logical_width: stream_info.logical_width,
            logical_height: stream_info.logical_height,
        },
    ));
    Ok((
        session,
        pw_fd,
        stream_info.node_id,
        stream_info.logical_width.max(1.0) as u32,
        stream_info.logical_height.max(1.0) as u32,
    ))
}

/// Call the xdg-desktop-portal ScreenCast API using raw zbus D-Bus calls.
/// Properly handles restore_token (ashpd 0.13 has a bug where it's never deserialized).
///
/// Returns a PortalSession that MUST be kept alive for the duration of the PipeWire stream.
/// The session drop path explicitly closes the portal session.
fn request_screencast() -> Result<PortalSession, String> {
    let restore_token = load_restore_token();
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;

    let (conn, session_path, pw_fd, stream_info) = rt.block_on(async {
        use futures_lite::StreamExt;
        use std::collections::HashMap;
        use zvariant::{ObjectPath, OwnedObjectPath, Value};

        let conn = zbus::Connection::session()
            .await
            .map_err(|e| format!("D-Bus session bus: {e}"))?;

        let unique_name = conn
            .unique_name()
            .ok_or("D-Bus connection has no unique name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");

        // Counter for unique handle tokens ("st1", "st2", ...)
        let mut token_counter: u32 = 0;
        let mut next_token = || -> String {
            token_counter += 1;
            format!("st{token_counter}")
        };

        // Portal proxy on the ScreenCast interface
        let portal = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("proxy dest: {e}"))?
            .path("/org/freedesktop/portal/desktop")
            .map_err(|e| format!("proxy path: {e}"))?
            .interface("org.freedesktop.portal.ScreenCast")
            .map_err(|e| format!("proxy iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("portal proxy: {e}"))?;

        // ---- Helper: make a portal call with Response signal handling ----
        // This closure implements the portal call pattern:
        //   subscribe → call → wait for signal

        // ---- 1. CreateSession ----
        let session_token = next_token();
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");

        // Step A: Subscribe to Response signal BEFORE calling the method
        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe CreateSession Response: {e}"))?;

        // Step B: Call the method
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));

        let _reply: OwnedObjectPath = portal
            .call("CreateSession", &(opts,))
            .await
            .map_err(|e| format!("CreateSession call: {e}"))?;

        // Step C: Wait for the Response signal
        let signal = response_stream
            .next()
            .await
            .ok_or("CreateSession: Response stream ended")?;
        let (code, results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("CreateSession denied (code {code})"));
        }
        drop(response_stream);
        drop(request_proxy);

        // Extract session_handle from results (or construct from token)
        let session_path = results
            .get("session_handle")
            .and_then(|v| try_extract_string(v))
            .unwrap_or_else(|| {
                format!("/org/freedesktop/portal/desktop/session/{unique_name}/{session_token}")
            });
        let session_obj = ObjectPath::try_from(session_path.as_str())
            .map_err(|e| format!("session path: {e}"))?;

        println!("[capture] Portal session created: {session_path}");

        // ---- 2. SelectSources ----
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");

        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe SelectSources Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("types", Value::U32(1)); // SOURCE_TYPE_MONITOR
        opts.insert("cursor_mode", Value::U32(4)); // CURSOR_MODE_METADATA
        opts.insert("persist_mode", Value::U32(2)); // PERSIST_UNTIL_REVOKED
        opts.insert("multiple", Value::Bool(false));
        if let Some(ref token) = restore_token {
            opts.insert("restore_token", Value::from(token.as_str()));
        }

        let _reply: OwnedObjectPath = portal
            .call("SelectSources", &(&session_obj, opts))
            .await
            .map_err(|e| format!("SelectSources call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("SelectSources: Response stream ended")?;
        let (code, _) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!(
                "SelectSources denied (code {code}). User may have cancelled the dialog."
            ));
        }
        drop(response_stream);
        drop(request_proxy);

        println!("[capture] Portal sources selected");

        // ---- 3. Start ----
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");

        let request_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req proxy: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy build: {e}"))?;
        let mut response_stream = request_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe Start Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));

        let _reply: OwnedObjectPath = portal
            .call("Start", &(&session_obj, "", opts))
            .await
            .map_err(|e| format!("Start call: {e}"))?;

        let signal = response_stream
            .next()
            .await
            .ok_or("Start: Response stream ended")?;
        let (code, start_results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!(
                "Start denied (code {code}). User may have cancelled the screen picker."
            ));
        }
        drop(response_stream);
        drop(request_proxy);

        // Extract restore_token from Start response
        if let Some(token) = start_results
            .get("restore_token")
            .and_then(|v| try_extract_string(v))
        {
            save_restore_token(&token);
        } else {
            println!("[capture] Portal did not return a restore token");
        }

        let stream_info = start_results
            .get("streams")
            .ok_or_else(|| "No streams in Start response".to_string())
            .and_then(extract_first_stream_info)?;

        println!("[capture] Portal granted PipeWire node {}", stream_info.node_id);

        // ---- 4. OpenPipeWireRemote ----
        let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = portal
            .call_method("OpenPipeWireRemote", &(&session_obj, empty_opts))
            .await
            .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
        let pw_fd: OwnedFd = reply
            .body()
            .deserialize::<zvariant::OwnedFd>()
            .map_err(|e| format!("OpenPipeWireRemote fd: {e}"))?
            .into();

        Ok((conn, session_path, pw_fd, stream_info))
    })?;

    Ok(PortalSession {
        pw_fd: Some(pw_fd),
        node_id: stream_info.node_id,
        logical_width: stream_info.logical_width.max(1.0) as u32,
        logical_height: stream_info.logical_height.max(1.0) as u32,
        runtime: rt,
        connection: conn,
        session_path,
    })
}

/// Parse a portal Response signal into (response_code, results_dict).
fn parse_response(
    signal: &zbus::message::Message,
) -> Result<(u32, std::collections::HashMap<String, zvariant::OwnedValue>), String> {
    let body = signal.body();
    body.deserialize()
        .map_err(|e| format!("deserialize Response: {e}"))
}

/// Try to extract a String from an OwnedValue (handles Variant wrapping).
fn try_extract_string(v: &zvariant::OwnedValue) -> Option<String> {
    if let Ok(s) = <&str>::try_from(v) {
        return Some(s.to_string());
    }
    if let Ok(val) = zvariant::Value::try_from(v) {
        if let zvariant::Value::Str(s) = val {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_first_stream_info(
    streams_val: &zvariant::OwnedValue,
) -> Result<PortalStreamInfo, String> {
    let value = zvariant::Value::try_from(streams_val).map_err(|e| format!("streams value: {e}"))?;
    let streams: Vec<(u32, std::collections::HashMap<String, zvariant::OwnedValue>)> =
        value
            .try_into()
            .map_err(|e| format!("streams value: {e}"))?;

    for (node_id, props) in streams {
        let mut logical_width = 0.0;
        let mut logical_height = 0.0;
        if let Some(size) = props.get("size") {
            if let Ok(value) = zvariant::Value::try_from(size) {
                if let Ok((width, height)) = <(i32, i32)>::try_from(value) {
                    logical_width = width.max(0) as f64;
                    logical_height = height.max(0) as f64;
                }
            }
        }
        return Ok(PortalStreamInfo {
            node_id,
            logical_width,
            logical_height,
        });
    }

    Err("Could not extract PipeWire stream info from streams".into())
}

// ---------------------------------------------------------------------------
// CaptureBackend implementation
// ---------------------------------------------------------------------------

impl CaptureBackend for PipeWireCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Capture already running".into());
        }

        println!("[capture] Requesting screen share via xdg-desktop-portal...");
        let remote_desktop = match request_remote_desktop_screencast() {
            Ok((session, pw_fd, node_id, logical_width, logical_height)) => {
                println!("[capture] RemoteDesktop portal input enabled");
                Some((session, pw_fd, node_id, logical_width, logical_height))
            }
            Err(err) => {
                eprintln!(
                    "[capture] RemoteDesktop portal input unavailable ({err}), falling back to screencast-only portal session"
                );
                None
            }
        };
        let session = if let Some((remote_session, pw_fd, node_id, logical_width, logical_height)) = remote_desktop {
            set_active_remote_desktop_session(Arc::clone(&remote_session));
            EitherPortalSession::RemoteDesktop {
                session: remote_session,
                pw_fd,
                node_id,
                logical_width,
                logical_height,
            }
        } else {
            EitherPortalSession::ScreenCast(request_screencast()?)
        };

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let (quit_tx, quit_rx) = pw::channel::channel();

        let handle = thread::spawn(move || {
            let (mut screen_session, pw_fd, node_id, logical_width, logical_height, remote_session) = match session {
                EitherPortalSession::ScreenCast(mut session) => {
                    let pw_fd = session.take_pw_fd();
                    let node_id = session.node_id;
                    let logical_width = session.logical_width;
                    let logical_height = session.logical_height;
                    (Some(session), pw_fd, node_id, logical_width, logical_height, None)
                }
                EitherPortalSession::RemoteDesktop {
                    session,
                    pw_fd,
                    node_id,
                    logical_width,
                    logical_height,
                } => (None, pw_fd, node_id, logical_width, logical_height, Some(session)),
            };
            if let Err(e) =
                run_pipewire_stream(pw_fd, node_id, logical_width, logical_height, tx, running, quit_rx)
            {
                eprintln!("[capture] PipeWire stream error: {e}");
            }
            drop(screen_session.take());
            if let Some(session) = remote_session.as_ref() {
                clear_active_remote_desktop_session(session);
            }
        });

        self.quit_tx = Some(quit_tx);
        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(quit_tx) = self.quit_tx.take() {
            let _ = quit_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        *active_remote_desktop_slot().lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------
// PipeWire stream
// ---------------------------------------------------------------------------

fn run_pipewire_stream(
    pw_fd: OwnedFd,
    node_id: u32,
    logical_width: u32,
    logical_height: u32,
    tx: Sender<CapturedFrame>,
    running: Arc<AtomicBool>,
    quit_rx: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();
    let trace = std::env::var_os("ST_TRACE").is_some_and(|v| v != "0");

    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(|e| format!("MainLoop: {e}"))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| format!("Context: {e}"))?;

    let core = context
        .connect_fd(pw_fd, None)
        .map_err(|e| format!("connect_fd: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "st-screen-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("Stream: {e}"))?;

    let video_info: Arc<Mutex<Option<NegotiatedVideoInfo>>> = Arc::new(Mutex::new(None));
    let cursor_cache = Arc::new(Mutex::new(CursorCache::default()));

    let video_info_param = Arc::clone(&video_info);
    let video_info_process = Arc::clone(&video_info);
    let cursor_cache_process = Arc::clone(&cursor_cache);
    let running_check = Arc::clone(&running);
    let process_counter = Arc::new(AtomicUsize::new(0));
    let dropped_counter = Arc::new(AtomicUsize::new(0));
    let process_counter_cb = Arc::clone(&process_counter);
    let dropped_counter_cb = Arc::clone(&dropped_counter);
    let frame_tx_process = tx;

    let format_pod_buffers = build_format_pod_buffers(logical_width.max(1), logical_height.max(1))?;
    let mut format_pods: Vec<&Pod> = format_pod_buffers
        .iter()
        .map(|buf| unsafe { Pod::from_raw(buf.as_ptr() as *const pw::spa::sys::spa_pod) })
        .collect();

    let mainloop_ptr = mainloop.as_raw_ptr();
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), move |_| unsafe {
        pw::sys::pw_main_loop_quit(mainloop_ptr);
    });
    let (release_tx, release_rx) = pw::channel::channel::<PendingBufferRelease>();
    let _release_receiver = release_rx.attach(mainloop.loop_(), move |pending| unsafe {
        if pending.stream.is_null() || pending.buffer.is_null() {
            return;
        }
        let _ = pw::sys::pw_stream_queue_buffer(pending.stream, pending.buffer);
    });
    let release_tx_process = release_tx.clone();

    let _listener = stream
        .add_local_listener::<()>()
        .param_changed(move |_stream, _user_data, id, param| {
            if id != pw::spa::sys::SPA_PARAM_Format {
                return;
            }
            if let Some(param) = param {
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_ok() {
                    let w = info.size().width;
                    let h = info.size().height;
                    let fmt = info.format();
                    let drm = video_format_to_drm_fourcc(fmt);
                    let modifier_present = unsafe {
                        !pw::spa::sys::spa_pod_find_prop(
                            param.as_raw_ptr().cast_const(),
                            std::ptr::null_mut(),
                            pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
                        )
                        .is_null()
                    };
                    let negotiated = NegotiatedVideoInfo {
                        width: w,
                        height: h,
                        drm_format: drm,
                        modifier: info.modifier(),
                        prefers_dmabuf: modifier_present
                            && drm != 0
                            && info.modifier() != u64::from(DrmModifier::Invalid),
                    };
                    match build_stream_param_buffers(negotiated.prefers_dmabuf, true) {
                        Ok(param_buffers) => {
                            let mut params: Vec<&Pod> = param_buffers
                                .iter()
                                .map(|buf| unsafe {
                                    Pod::from_raw(buf.as_ptr() as *const pw::spa::sys::spa_pod)
                                })
                                .collect();
                            if let Err(err) = _stream.update_params(&mut params) {
                                eprintln!("[capture] PipeWire stream param update failed: {err}");
                            }
                        }
                        Err(err) => {
                            eprintln!("[capture] PipeWire stream param build failed: {err}");
                        }
                    }
                    println!(
                        "[capture] Negotiated format: {w}x{h} {fmt:?} (DRM fourcc 0x{drm:08x}, modifier=0x{:016x}, buffers={})",
                        negotiated.modifier,
                        if negotiated.prefers_dmabuf { "dmabuf" } else { "mem" }
                    );
                    *video_info_param.lock().unwrap() = Some(negotiated);
                }
            }
        })
        .process(move |stream, _user_data| {
            let process_idx = process_counter_cb.fetch_add(1, Ordering::Relaxed);
            if trace && process_idx < 8 {
                eprintln!("[trace][pipewire] process callback #{process_idx}");
            }
            if !running_check.load(Ordering::SeqCst) {
                unsafe { pw::sys::pw_main_loop_quit(mainloop_ptr) };
                return;
            }

            // Drain all pending buffers and keep only the newest one.
            let stream_ptr = stream.as_raw_ptr();
            let mut latest = unsafe { pw::sys::pw_stream_dequeue_buffer(stream_ptr) };
            if latest.is_null() {
                return;
            }
            loop {
                let newer = unsafe { pw::sys::pw_stream_dequeue_buffer(stream_ptr) };
                if newer.is_null() {
                    break;
                }
                unsafe {
                    let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                }
                latest = newer;
            }

            let spa_buffer = unsafe { (*latest).buffer };
            if spa_buffer.is_null() {
                unsafe {
                    let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                }
                return;
            }

            if unsafe { (*spa_buffer).n_datas == 0 || (*spa_buffer).datas.is_null() } {
                unsafe {
                    let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                }
                return;
            }
            let datas = unsafe {
                std::slice::from_raw_parts_mut((*spa_buffer).datas, (*spa_buffer).n_datas as usize)
            };
            let data = unsafe { &mut *(datas.as_mut_ptr() as *mut pw::spa::buffer::Data) };
            let chunk = data.chunk();
            if chunk
                .flags()
                .contains(pw::spa::buffer::ChunkFlags::CORRUPTED)
            {
                if trace && process_idx < 16 {
                    eprintln!("[trace][pipewire] dropped corrupted PipeWire chunk");
                }
                unsafe {
                    let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                }
                return;
            }
            let cursor = {
                let mut cache = cursor_cache_process.lock().unwrap();
                extract_cursor(spa_buffer, &mut cache)
            };
            let info = *video_info_process.lock().unwrap();
            if let Some(info) = info {
                let raw_type = data.as_raw().type_;
                let chunk_offset = chunk.offset();
                let chunk_size = chunk.size();
                let chunk_stride = chunk.stride();
                if trace && process_idx < 16 {
                    let raw_type_label = if raw_type == pw::spa::sys::SPA_DATA_DmaBuf {
                        "dmabuf"
                    } else {
                        "mem"
                    };
                    eprintln!(
                        "[trace][pipewire] buffer #{process_idx}: type={raw_type_label} raw_type={} offset={} size={} stride={}",
                        raw_type,
                        chunk_offset,
                        chunk_size,
                        chunk_stride
                    );
                }
                let mut queue_latest_immediately = true;
                let frame = if raw_type == pw::spa::sys::SPA_DATA_DmaBuf
                    && info.drm_format != 0
                    && info.prefers_dmabuf
                {
                    match duplicate_dmabuf_planes(datas, info.modifier) {
                        Ok(planes) => {
                            queue_latest_immediately = false;
                            Some(CapturedFrame {
                                data: FrameData::DmaBuf {
                                    planes,
                                    drm_format: info.drm_format,
                                    _lease: Some(FrameLease::new(PipeWireBufferLease {
                                        release_tx: release_tx_process.clone(),
                                        pending: Some(PendingBufferRelease {
                                            stream: stream_ptr,
                                            buffer: latest,
                                        }),
                                    })),
                                },
                                width: info.width,
                                height: info.height,
                                cursor,
                            })
                        }
                        Err(err) => {
                            eprintln!("[capture] PipeWire dmabuf import setup failed: {err}");
                            None
                        }
                    }
                } else if raw_type == pw::spa::sys::SPA_DATA_DmaBuf {
                    let raw_fd = data.fd();
                    if raw_fd < 0 {
                        eprintln!("[capture] PipeWire dmabuf RAM fallback failed: invalid fd {raw_fd}");
                        None
                    } else if chunk_stride <= 0 {
                        eprintln!(
                            "[capture] PipeWire dmabuf RAM fallback failed: invalid stride {chunk_stride}"
                        );
                        None
                    } else {
                        let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
                        match copy_dmabuf_bgrx_frame(
                            borrowed,
                            chunk_offset,
                            chunk_stride as u32,
                            info.width,
                            info.height,
                        ) {
                            Ok(bytes) => Some(CapturedFrame {
                                data: FrameData::Ram(bytes),
                                width: info.width,
                                height: info.height,
                                cursor,
                            }),
                            Err(err) => {
                                eprintln!("[capture] PipeWire dmabuf RAM fallback failed: {err}");
                                None
                            }
                        }
                    }
                } else if let Some(slice) = data.data() {
                    match copy_mem_ptr_bgrx_frame(
                        slice,
                        chunk_offset,
                        chunk_size,
                        chunk_stride,
                        info.width,
                        info.height,
                    ) {
                        Ok(bytes) => Some(CapturedFrame {
                            data: FrameData::Ram(bytes),
                            width: info.width,
                            height: info.height,
                            cursor,
                        }),
                        Err(err) => {
                            eprintln!("[capture] PipeWire shared-memory copy failed: {err}");
                            None
                        }
                    }
                } else {
                    None
                };

                if let Some(frame) = frame {
                    match frame_tx_process.try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(frame)) => {
                            drop(frame);
                            let drop_idx = dropped_counter_cb.fetch_add(1, Ordering::Relaxed);
                            if trace && drop_idx < 8 {
                                eprintln!(
                                    "[trace][pipewire] dropped captured frame because capture channel is full"
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(frame)) => {
                            drop(frame);
                            if trace {
                                eprintln!(
                                    "[trace][pipewire] capture channel disconnected; stopping main loop"
                                );
                            }
                            unsafe { pw::sys::pw_main_loop_quit(mainloop_ptr) };
                        }
                    }
                }
                if queue_latest_immediately {
                    unsafe {
                        let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                    }
                }
            } else {
                unsafe {
                    let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
                }
            }
        })
        .register()
        .map_err(|e| format!("listener register: {e}"))?;

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            format_pods.as_mut_slice(),
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    println!("[capture] PipeWire stream connected, running main loop...");
    mainloop.run();
    println!("[capture] PipeWire main loop exited");
    Ok(())
}
