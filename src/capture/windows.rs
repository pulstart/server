use super::{target_fps, CaptureBackend, CapturedCursor, CapturedFrame, FrameData};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    GetObjectW, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, HCURSOR, ICONINFO, CURSORINFO,
    CURSOR_SHOWING, DI_NORMAL, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

pub struct PlatformCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PlatformCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    pub fn backend_name(&self) -> &'static str {
        "gdi"
    }
}

impl CaptureBackend for PlatformCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("capture already running".into());
        }

        let session = GdiCaptureSession::new()?;
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let handle = thread::spawn(move || run_capture_loop(session, tx, running));
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

fn run_capture_loop(
    mut session: GdiCaptureSession,
    tx: Sender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let mut target_interval = frame_interval();
    let mut next_metrics_check = Instant::now();

    while running.load(Ordering::SeqCst) {
        let frame_started = Instant::now();
        if next_metrics_check <= frame_started {
            if let Err(err) = session.refresh_if_needed() {
                eprintln!("[capture] Windows capture refresh failed: {err}");
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            target_interval = frame_interval();
            next_metrics_check = frame_started + Duration::from_secs(1);
        }

        match session.capture_frame() {
            Ok(frame) => match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            },
            Err(err) => {
                eprintln!("[capture] Windows capture failed: {err}");
                thread::sleep(Duration::from_millis(250));
                if let Err(refresh_err) = session.recreate() {
                    eprintln!("[capture] Windows capture recreate failed: {refresh_err}");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }

        let elapsed = frame_started.elapsed();
        if elapsed < target_interval {
            thread::sleep(target_interval - elapsed);
        }
    }
}

fn frame_interval() -> Duration {
    Duration::from_secs_f64(1.0 / target_fps().max(1) as f64)
}

struct GdiCaptureSession {
    screen_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut core::ffi::c_void,
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,
    cursor_cache: Option<CapturedCursor>,
}

unsafe impl Send for GdiCaptureSession {}

impl GdiCaptureSession {
    fn new() -> Result<Self, String> {
        unsafe {
            let screen_dc = GetDC(None);
            if screen_dc.is_invalid() {
                return Err("GetDC(NULL) failed".into());
            }
            let memory_dc = CreateCompatibleDC(Some(screen_dc));
            if memory_dc.is_invalid() {
                let _ = ReleaseDC(None, screen_dc);
                return Err("CreateCompatibleDC failed".into());
            }

            let mut session = Self {
                screen_dc,
                memory_dc,
                bitmap: HBITMAP::default(),
                old_bitmap: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                origin_x: 0,
                origin_y: 0,
                width: 0,
                height: 0,
                cursor_cache: None,
            };
            session.recreate_bitmap()?;
            Ok(session)
        }
    }

    fn recreate(&mut self) -> Result<(), String> {
        self.recreate_bitmap()
    }

    fn refresh_if_needed(&mut self) -> Result<(), String> {
        let (origin_x, origin_y, width, height) = current_virtual_screen_metrics();
        if origin_x != self.origin_x
            || origin_y != self.origin_y
            || width != self.width
            || height != self.height
        {
            self.recreate_bitmap()?;
        }
        Ok(())
    }

    fn capture_frame(&mut self) -> Result<CapturedFrame, String> {
        unsafe {
            BitBlt(
                self.memory_dc,
                0,
                0,
                self.width,
                self.height,
                Some(self.screen_dc),
                self.origin_x,
                self.origin_y,
                SRCCOPY | CAPTUREBLT,
            )
            .map_err(|err| format!("BitBlt failed: {err}"))?;

            let len = (self.width as usize)
                .saturating_mul(self.height as usize)
                .saturating_mul(4);
            let pixels = std::slice::from_raw_parts(self.bits as *const u8, len).to_vec();
            let cursor = self.capture_cursor();
            Ok(CapturedFrame {
                data: FrameData::Ram(pixels),
                width: self.width as u32,
                height: self.height as u32,
                cursor,
            })
        }
    }

    fn capture_cursor(&mut self) -> Option<CapturedCursor> {
        unsafe {
            let mut info = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if GetCursorInfo(&mut info).is_err() {
                return None;
            }

            let serial = info.hCursor.0 as usize as u64;
            let cached_hotspot = self
                .cursor_cache
                .as_ref()
                .map(|cursor| (cursor.hotspot_x, cursor.hotspot_y))
                .unwrap_or((0, 0));

            if info.flags != CURSOR_SHOWING || serial == 0 {
                return Some(CapturedCursor {
                    pixels: Vec::new(),
                    x: info.ptScreenPos.x - self.origin_x - cached_hotspot.0 as i32,
                    y: info.ptScreenPos.y - self.origin_y - cached_hotspot.1 as i32,
                    hotspot_x: cached_hotspot.0,
                    hotspot_y: cached_hotspot.1,
                    width: 0,
                    height: 0,
                    shape_serial: serial,
                    visible: false,
                });
            }

            let needs_refresh = self
                .cursor_cache
                .as_ref()
                .map(|cursor| cursor.shape_serial != serial)
                .unwrap_or(true);
            if needs_refresh {
                match self.load_cursor_shape(info.hCursor) {
                    Ok(cursor) => self.cursor_cache = Some(cursor),
                    Err(err) => {
                        eprintln!("[capture] Windows cursor capture failed: {err}");
                        return None;
                    }
                }
            }

            let mut cursor = self.cursor_cache.clone()?;
            cursor.shape_serial = serial;
            cursor.x = info.ptScreenPos.x - self.origin_x - cursor.hotspot_x as i32;
            cursor.y = info.ptScreenPos.y - self.origin_y - cursor.hotspot_y as i32;
            cursor.visible = true;
            Some(cursor)
        }
    }

    fn load_cursor_shape(&self, cursor: HCURSOR) -> Result<CapturedCursor, String> {
        unsafe {
            let mut icon_info = ICONINFO::default();
            GetIconInfo(cursor.into(), &mut icon_info)
                .map_err(|err| format!("GetIconInfo failed: {err}"))?;

            let shape = (|| {
                let (width, height) =
                    cursor_bitmap_dimensions(icon_info.hbmColor, icon_info.hbmMask)?;
                if width <= 0 || height <= 0 {
                    return Err("cursor bitmap size is invalid".to_string());
                }

                let cursor_dc = CreateCompatibleDC(Some(self.screen_dc));
                if cursor_dc.is_invalid() {
                    return Err("CreateCompatibleDC for cursor failed".to_string());
                }

                let mut bmi = BITMAPINFO::default();
                bmi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                };

                let result = (|| {
                    let mut bits = std::ptr::null_mut();
                    let bitmap = CreateDIBSection(
                        Some(self.screen_dc),
                        &bmi,
                        DIB_RGB_COLORS,
                        &mut bits,
                        None,
                        0,
                    )
                    .map_err(|err| format!("CreateDIBSection for cursor failed: {err}"))?;
                    if bitmap.is_invalid() || bits.is_null() {
                        return Err("CreateDIBSection for cursor returned invalid objects".to_string());
                    }

                    let old_bitmap = SelectObject(cursor_dc, bitmap.into());
                    if old_bitmap.is_invalid() {
                        let _ = DeleteObject(bitmap.into());
                        return Err("SelectObject for cursor failed".to_string());
                    }

                    let len = (width as usize)
                        .saturating_mul(height as usize)
                        .saturating_mul(4);
                    std::ptr::write_bytes(bits.cast::<u8>(), 0, len);
                    DrawIconEx(cursor_dc, 0, 0, cursor.into(), width, height, 0, None, DI_NORMAL)
                        .map_err(|err| format!("DrawIconEx for cursor failed: {err}"))?;

                    let pixels = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
                    let _ = SelectObject(cursor_dc, old_bitmap);
                    let _ = DeleteObject(bitmap.into());

                    Ok(CapturedCursor {
                        pixels,
                        x: 0,
                        y: 0,
                        hotspot_x: icon_info.xHotspot,
                        hotspot_y: icon_info.yHotspot,
                        width: width as u32,
                        height: height as u32,
                        shape_serial: cursor.0 as usize as u64,
                        visible: true,
                    })
                })();

                let _ = DeleteDC(cursor_dc);
                result
            })();

            if !icon_info.hbmColor.is_invalid() {
                let _ = DeleteObject(icon_info.hbmColor.into());
            }
            if !icon_info.hbmMask.is_invalid() {
                let _ = DeleteObject(icon_info.hbmMask.into());
            }

            shape
        }
    }

    fn recreate_bitmap(&mut self) -> Result<(), String> {
        unsafe {
            let (origin_x, origin_y, width, height) = current_virtual_screen_metrics();
            if width <= 0 || height <= 0 {
                return Err("virtual desktop size is invalid".into());
            }

            if !self.bitmap.is_invalid() {
                let _ = SelectObject(self.memory_dc, self.old_bitmap);
                let _ = DeleteObject(self.bitmap.into());
                self.bitmap = HBITMAP::default();
                self.old_bitmap = HGDIOBJ::default();
                self.bits = std::ptr::null_mut();
            }

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };

            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                Some(self.screen_dc),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )
            .map_err(|err| format!("CreateDIBSection failed: {err}"))?;
            if bitmap.is_invalid() || bits.is_null() {
                return Err("CreateDIBSection failed".into());
            }
            let old_bitmap = SelectObject(self.memory_dc, bitmap.into());
            if old_bitmap.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                return Err("SelectObject failed".into());
            }

            self.bitmap = bitmap;
            self.old_bitmap = old_bitmap;
            self.bits = bits;
            self.origin_x = origin_x;
            self.origin_y = origin_y;
            self.width = width;
            self.height = height;
            Ok(())
        }
    }
}

impl Drop for GdiCaptureSession {
    fn drop(&mut self) {
        unsafe {
            if !self.bitmap.is_invalid() {
                let _ = SelectObject(self.memory_dc, self.old_bitmap);
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.memory_dc.is_invalid() {
                let _ = DeleteDC(self.memory_dc);
            }
            if !self.screen_dc.is_invalid() {
                let _ = ReleaseDC(None, self.screen_dc);
            }
        }
    }
}

fn current_virtual_screen_metrics() -> (i32, i32, i32, i32) {
    unsafe {
        let origin_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let origin_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        (origin_x, origin_y, width, height)
    }
}

fn cursor_bitmap_dimensions(color_bitmap: HBITMAP, mask_bitmap: HBITMAP) -> Result<(i32, i32), String> {
    let (bitmap, monochrome) = if !color_bitmap.is_invalid() {
        (color_bitmap, false)
    } else if !mask_bitmap.is_invalid() {
        (mask_bitmap, true)
    } else {
        return Err("cursor icon has no usable bitmaps".into());
    };

    let mut info = BITMAP::default();
    let bytes_written = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut info as *mut _ as *mut core::ffi::c_void),
        )
    };
    if bytes_written <= 0 {
        return Err("GetObjectW failed for cursor bitmap".into());
    }

    let height = if monochrome {
        info.bmHeight / 2
    } else {
        info.bmHeight
    };
    Ok((info.bmWidth, height))
}
