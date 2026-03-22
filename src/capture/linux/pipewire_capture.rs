use super::super::{CaptureBackend, CapturedCursor, CapturedFrame, FrameData};
use crossbeam_channel::{Sender, TrySendError};
use std::io::Cursor;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;

use pipewire as pw;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;
use pw::spa::utils::Id;

const PIPEWIRE_CURSOR_META_SIZE: i32 = (std::mem::size_of::<pw::spa::sys::spa_meta_cursor>()
    + std::mem::size_of::<pw::spa::sys::spa_meta_bitmap>()
    + 256 * 256 * 4) as i32;

fn copy_dmabuf_bgrx_frame(
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
            "PipeWire dmabuf stride {stride} is smaller than row size {row_bytes}"
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
            "PipeWire dmabuf mmap failed: {}",
            std::io::Error::last_os_error()
        ));
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
        libc::munmap(mapped, mapped_size);
    }
    Ok(out)
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
            pixels: self.pixels.clone(),
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
        return cache.current_cursor();
    }
    if !unsafe { pw::spa::sys::spa_meta_cursor_is_valid(cursor_ptr.cast_const()) } {
        return cache.current_cursor();
    }

    let cursor = unsafe { &*cursor_ptr };
    cache.shape_serial = cursor.id as u64;
    if cursor.bitmap_offset as usize >= std::mem::size_of::<pw::spa::sys::spa_meta_cursor>() {
        let bitmap_ptr = unsafe {
            (cursor_ptr as *const u8).add(cursor.bitmap_offset as usize)
                as *const pw::spa::sys::spa_meta_bitmap
        };
        if !bitmap_ptr.is_null() && unsafe { pw::spa::sys::spa_meta_bitmap_is_valid(bitmap_ptr) } {
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
// Raw D-Bus portal interaction (matches Sunshine's portalgrab.cpp)
//
// Pattern per call (same as Sunshine's GDBus approach):
//   1. Generate handle_token and predict the request object path
//   2. Subscribe to Response signal on that path (BEFORE calling the method)
//   3. Call the portal method (returns immediately with request handle)
//   4. Wait for the Response signal (carries response_code + results dict)
// ---------------------------------------------------------------------------

/// Result of the portal ScreenCast session.
/// The `_runtime` field keeps the D-Bus connection alive — the portal closes the session
/// when the creator disconnects from D-Bus (per XDG portal spec).
struct PortalSession {
    pw_fd: OwnedFd,
    node_id: u32,
    _runtime: tokio::runtime::Runtime,
}

/// Call the xdg-desktop-portal ScreenCast API using raw zbus D-Bus calls.
/// Properly handles restore_token (ashpd 0.13 has a bug where it's never deserialized).
///
/// Returns a PortalSession that MUST be kept alive for the duration of the PipeWire stream.
/// Dropping the session drops the D-Bus connection, which tells the portal to close the
/// session and invalidate the PipeWire node.
fn request_screencast() -> Result<PortalSession, String> {
    let restore_token = load_restore_token();
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;

    let (pw_fd, node_id) = rt.block_on(async {
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

        // Counter for unique handle tokens (matches Sunshine's "Sunshine1", "Sunshine2", ...)
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
        // This closure implements the exact Sunshine pattern:
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

        // Extract PipeWire node_id from streams
        let node_id = start_results
            .get("streams")
            .ok_or("No streams in Start response")?;
        let node_id = extract_first_stream_node_id(node_id)?;

        println!("[capture] Portal granted PipeWire node {node_id}");

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

        // Spawn a keepalive task — holds the D-Bus connection open so the
        // portal session stays active for the lifetime of the runtime.
        // Without this, the portal closes the session when the connection drops,
        // which kills the PipeWire stream.
        tokio::spawn(async move {
            let _conn_keepalive = conn;
            futures_lite::future::pending::<()>().await;
        });

        Ok((pw_fd, node_id))
    })?;

    Ok(PortalSession {
        pw_fd,
        node_id,
        _runtime: rt,
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

/// Extract the first stream's PipeWire node ID from the streams value.
fn extract_first_stream_node_id(streams_val: &zvariant::OwnedValue) -> Result<u32, String> {
    use zvariant::Value;

    let val = Value::try_from(streams_val).map_err(|e| format!("streams value: {e}"))?;

    if let Value::Array(arr) = val {
        for item in arr.iter() {
            if let Value::Structure(s) = item {
                let fields = s.fields();
                if let Some(Value::U32(node_id)) = fields.first() {
                    return Ok(*node_id);
                }
            }
        }
    }

    Err("Could not extract PipeWire node ID from streams".into())
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
        let session = request_screencast()?;

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let (quit_tx, quit_rx) = pw::channel::channel();

        let handle = thread::spawn(move || {
            // Move the entire PortalSession into this thread to keep the D-Bus
            // connection alive for the duration of the PipeWire stream.
            let _session_keepalive = session._runtime;
            if let Err(e) =
                run_pipewire_stream(session.pw_fd, session.node_id, tx, running, quit_rx)
            {
                eprintln!("[capture] PipeWire stream error: {e}");
            }
            // _session_keepalive is dropped here, closing the D-Bus connection
            // and the portal session.
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
    }
}

// ---------------------------------------------------------------------------
// PipeWire stream
// ---------------------------------------------------------------------------

fn run_pipewire_stream(
    pw_fd: OwnedFd,
    node_id: u32,
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

    let video_info: Arc<Mutex<Option<(u32, u32, u32)>>> = Arc::new(Mutex::new(None));
    let frame_tx = Arc::new(Mutex::new(tx));
    let cursor_cache = Arc::new(Mutex::new(CursorCache::default()));

    let video_info_param = Arc::clone(&video_info);
    let video_info_process = Arc::clone(&video_info);
    let frame_tx_process = Arc::clone(&frame_tx);
    let cursor_cache_process = Arc::clone(&cursor_cache);
    let running_check = Arc::clone(&running);
    let process_counter = Arc::new(AtomicUsize::new(0));
    let dropped_counter = Arc::new(AtomicUsize::new(0));
    let process_counter_cb = Arc::clone(&process_counter);
    let dropped_counter_cb = Arc::clone(&dropped_counter);

    let mut pod_buf = vec![0u8; 1024];
    let _pod_result = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(&mut pod_buf),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: vec![
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaType,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_TYPE_video)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_mediaSubtype,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_MEDIA_SUBTYPE_raw)),
                },
                pw::spa::pod::Property {
                    key: pw::spa::sys::SPA_FORMAT_VIDEO_format,
                    flags: pw::spa::pod::PropertyFlags::empty(),
                    value: pw::spa::pod::Value::Id(Id(pw::spa::sys::SPA_VIDEO_FORMAT_BGRx)),
                },
            ],
        }),
    )
    .map_err(|e| format!("pod serialize: {e:?}"))?;

    let pod_ref = unsafe { Pod::from_raw(pod_buf.as_ptr() as *const pw::spa::sys::spa_pod) };

    let mainloop_ptr = mainloop.as_raw_ptr();
    let _quit_receiver = quit_rx.attach(mainloop.loop_(), move |_| unsafe {
        pw::sys::pw_main_loop_quit(mainloop_ptr);
    });

    let _listener = stream
        .add_local_listener::<()>()
        .param_changed(move |_stream, _user_data, id, param| {
            if id != pw::spa::sys::SPA_PARAM_Format {
                return;
            }
            if let Some(param) = param {
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_ok() {
                    let mut cursor_meta_buf = vec![0u8; 128];
                    let meta_result = pw::spa::pod::serialize::PodSerializer::serialize(
                        Cursor::new(&mut cursor_meta_buf),
                        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                            type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamMeta,
                            id: pw::spa::sys::SPA_PARAM_Meta,
                            properties: vec![
                                pw::spa::pod::Property {
                                    key: pw::spa::sys::SPA_PARAM_META_type,
                                    flags: pw::spa::pod::PropertyFlags::empty(),
                                    value: pw::spa::pod::Value::Id(Id(
                                        pw::spa::sys::SPA_META_Cursor,
                                    )),
                                },
                                pw::spa::pod::Property {
                                    key: pw::spa::sys::SPA_PARAM_META_size,
                                    flags: pw::spa::pod::PropertyFlags::empty(),
                                    value: pw::spa::pod::Value::Int(PIPEWIRE_CURSOR_META_SIZE),
                                },
                            ],
                        }),
                    );
                    match meta_result {
                        Ok(_) => {
                            let cursor_meta = unsafe {
                                Pod::from_raw(
                                    cursor_meta_buf.as_ptr() as *const pw::spa::sys::spa_pod,
                                )
                            };
                            let mut params = [cursor_meta];
                            if let Err(err) = _stream.update_params(&mut params) {
                                eprintln!(
                                    "[capture] PipeWire cursor metadata request failed: {err}"
                                );
                            } else if trace {
                                eprintln!(
                                    "[trace][pipewire] requested SPA_META_Cursor size={PIPEWIRE_CURSOR_META_SIZE}"
                                );
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "[capture] PipeWire cursor metadata pod build failed: {err:?}"
                            );
                        }
                    }

                    let w = info.size().width;
                    let h = info.size().height;
                    let fmt = info.format();
                    let drm = video_format_to_drm_fourcc(fmt);
                    println!(
                        "[capture] Negotiated format: {w}x{h} {fmt:?} (DRM fourcc 0x{drm:08x})"
                    );
                    *video_info_param.lock().unwrap() = Some((w, h, drm));
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
            let cursor = {
                let mut cache = cursor_cache_process.lock().unwrap();
                extract_cursor(spa_buffer, &mut cache)
            };
            let info = video_info_process.lock().unwrap();
            if let Some((w, h, _drm_format)) = *info {
                let raw_type = data.as_raw().type_;
                let chunk = data.chunk();
                let chunk_offset = chunk.offset();
                let chunk_size = chunk.size();
                let chunk_stride = chunk.stride();
                let frame = if raw_type == pw::spa::sys::SPA_DATA_DmaBuf {
                    let raw_fd = data.fd();
                    let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
                    match copy_dmabuf_bgrx_frame(
                        borrowed,
                        chunk_offset,
                        chunk_stride as u32,
                        w,
                        h,
                    ) {
                        Ok(bytes) => Some(CapturedFrame {
                            data: FrameData::Ram(bytes),
                            width: w,
                            height: h,
                            cursor: cursor.clone(),
                        }),
                        Err(err) => {
                            eprintln!("[capture] PipeWire dmabuf copy failed: {err}");
                            None
                        }
                    }
                } else if let Some(slice) = data.data() {
                    match copy_mem_ptr_bgrx_frame(
                        slice,
                        chunk_offset,
                        chunk_size,
                        chunk_stride,
                        w,
                        h,
                    ) {
                        Ok(bytes) => Some(CapturedFrame {
                            data: FrameData::Ram(bytes),
                            width: w,
                            height: h,
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
                    match frame_tx_process.lock().unwrap().try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            let drop_idx = dropped_counter_cb.fetch_add(1, Ordering::Relaxed);
                            if trace && drop_idx < 8 {
                                eprintln!(
                                    "[trace][pipewire] dropped captured frame because capture channel is full"
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            if trace {
                                eprintln!(
                                    "[trace][pipewire] capture channel disconnected; stopping main loop"
                                );
                            }
                            unsafe { pw::sys::pw_main_loop_quit(mainloop_ptr) };
                        }
                    }
                }
            }

            unsafe {
                let _ = pw::sys::pw_stream_queue_buffer(stream_ptr, latest);
            }
        })
        .register()
        .map_err(|e| format!("listener register: {e}"))?;

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod_ref],
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    println!("[capture] PipeWire stream connected, running main loop...");
    mainloop.run();
    println!("[capture] PipeWire main loop exited");
    Ok(())
}
