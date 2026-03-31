use super::{
    target_fps, CaptureBackend, CapturedCursor, CapturedFrame, D3D11FrameTexture, FrameData,
};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetObjectW,
    ReleaseDC, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, HCURSOR, ICONINFO, CURSORINFO,
    CURSOR_SHOWING, DI_NORMAL, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

const DXGI_FRAME_POOL_SIZE: usize = 4;

enum Backend {
    Dxgi(DxgiCapture),
    Gdi(GdiCapture),
}

pub struct PlatformCapture {
    backend: Backend,
}

impl PlatformCapture {
    pub fn new() -> Self {
        match std::env::var("ST_CAPTURE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "gdi" => {
                println!("[capture] ST_CAPTURE=gdi override: using GDI capture");
                Self {
                    backend: Backend::Gdi(GdiCapture::new()),
                }
            }
            "dxgi" | "duplication" => {
                println!("[capture] ST_CAPTURE=dxgi override: using DXGI duplication");
                Self {
                    backend: Backend::Dxgi(DxgiCapture::new()),
                }
            }
            _ => Self {
                backend: Backend::Dxgi(DxgiCapture::new()),
            },
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match &self.backend {
            Backend::Dxgi(_) => "dxgi-dup",
            Backend::Gdi(_) => "gdi",
        }
    }
}

impl CaptureBackend for PlatformCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        match &mut self.backend {
            Backend::Dxgi(capture) => match capture.start(tx.clone()) {
                Ok(()) => Ok(()),
                Err(dxgi_err) => {
                    eprintln!("[capture] DXGI duplication failed ({dxgi_err}), falling back to GDI...");
                    let mut gdi = GdiCapture::new();
                    gdi.start(tx)?;
                    self.backend = Backend::Gdi(gdi);
                    Ok(())
                }
            },
            Backend::Gdi(capture) => capture.start(tx),
        }
    }

    fn stop(&mut self) {
        match &mut self.backend {
            Backend::Dxgi(capture) => capture.stop(),
            Backend::Gdi(capture) => capture.stop(),
        }
    }
}

fn frame_interval() -> Duration {
    Duration::from_secs_f64(1.0 / target_fps().max(1) as f64)
}

fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        thread::sleep(deadline - now);
    }
}

struct DxgiCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DxgiCapture {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for DxgiCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("DXGI capture already running".into());
        }

        let session = DxgiCaptureSession::new()?;
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        self.handle = Some(thread::spawn(move || run_dxgi_capture_loop(session, tx, running)));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct GdiCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GdiCapture {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for GdiCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("GDI capture already running".into());
        }

        let session = GdiCaptureSession::new()?;
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        self.handle = Some(thread::spawn(move || run_gdi_capture_loop(session, tx, running)));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_dxgi_capture_loop(
    mut session: DxgiCaptureSession,
    tx: Sender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let mut last_texture: Option<Weak<D3D11FrameTexture>> = None;
    let mut next_capture_at = Instant::now();

    while running.load(Ordering::SeqCst) {
        let target_interval = frame_interval();
        sleep_until(next_capture_at);
        next_capture_at = Instant::now() + target_interval;

        match session.try_acquire_texture() {
            Ok(Some(texture)) => last_texture = Some(Arc::downgrade(&texture)),
            Ok(None) => {}
            Err(err) => {
                eprintln!("[capture] DXGI capture failed: {err}");
                thread::sleep(Duration::from_millis(250));
                match DxgiCaptureSession::new() {
                    Ok(next) => {
                        session = next;
                        last_texture = None;
                    }
                    Err(recreate_err) => {
                        eprintln!("[capture] DXGI capture recreate failed: {recreate_err}");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                continue;
            }
        }

        if let Some(texture) = last_texture.as_ref().and_then(Weak::upgrade) {
            let cursor = session.capture_cursor();
            let frame = CapturedFrame {
                data: FrameData::D3D11Texture {
                    texture,
                    array_index: 0,
                },
                width: session.width,
                height: session.height,
                cursor,
            };
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
    }
}

fn run_gdi_capture_loop(
    mut session: GdiCaptureSession,
    tx: Sender<CapturedFrame>,
    running: Arc<AtomicBool>,
) {
    let mut target_interval = frame_interval();
    let mut next_metrics_check = Instant::now();
    let mut next_capture_at = Instant::now();

    while running.load(Ordering::SeqCst) {
        sleep_until(next_capture_at);
        let frame_started = Instant::now();
        if next_metrics_check <= frame_started {
            if let Err(err) = session.refresh_if_needed() {
                eprintln!("[capture] Windows GDI refresh failed: {err}");
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            target_interval = frame_interval();
            next_metrics_check = frame_started + Duration::from_secs(1);
        }
        next_capture_at = frame_started + target_interval;

        match session.capture_frame() {
            Ok(frame) => match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            },
            Err(err) => {
                eprintln!("[capture] Windows GDI capture failed: {err}");
                thread::sleep(Duration::from_millis(250));
                if let Err(refresh_err) = session.recreate() {
                    eprintln!("[capture] Windows GDI recreate failed: {refresh_err}");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}

struct DxgiCaptureSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    cursor: CursorCapture,
    frame_pool: Vec<Arc<D3D11FrameTexture>>,
    next_slot: usize,
}

impl DxgiCaptureSession {
    fn new() -> Result<Self, String> {
        let (adapter, output, desc) = select_output()?;
        let (device, context) = create_device_for_output(&adapter)?;
        let duplication = unsafe {
            output
                .DuplicateOutput(&device)
                .map_err(|err| format!("DuplicateOutput failed: {err}"))?
        };

        let width = desc
            .DesktopCoordinates
            .right
            .saturating_sub(desc.DesktopCoordinates.left) as u32;
        let height = desc
            .DesktopCoordinates
            .bottom
            .saturating_sub(desc.DesktopCoordinates.top) as u32;
        let cursor = CursorCapture::new(desc.DesktopCoordinates.left, desc.DesktopCoordinates.top)?;

        println!(
            "[capture] Using DXGI duplication ({}x{} @ {},{})",
            width,
            height,
            desc.DesktopCoordinates.left,
            desc.DesktopCoordinates.top
        );

        Ok(Self {
            device,
            context,
            duplication,
            width,
            height,
            cursor,
            frame_pool: Vec::new(),
            next_slot: 0,
        })
    }

    fn try_acquire_texture(&mut self) -> Result<Option<Arc<D3D11FrameTexture>>, String> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired =
            unsafe { self.duplication.AcquireNextFrame(0, &mut frame_info, &mut resource) };

        match acquired {
            Ok(()) => {
                let copy_result = (|| -> Result<Option<Arc<D3D11FrameTexture>>, String> {
                    let resource =
                        resource.ok_or_else(|| "AcquireNextFrame returned no resource".to_string())?;
                    let source = resource.cast::<ID3D11Texture2D>().map_err(|err| {
                        format!("IDXGIResource->ID3D11Texture2D cast failed: {err}")
                    })?;
                    self.copy_into_pool(&source)
                })();
                let release_result = unsafe {
                    self.duplication
                        .ReleaseFrame()
                        .map_err(|err| format!("ReleaseFrame failed: {err}"))
                };
                match (copy_result, release_result) {
                    (Err(err), _) => Err(err),
                    (Ok(_), Err(err)) => Err(err),
                    (Ok(frame), Ok(())) => Ok(frame),
                }
            }
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                Err("DXGI duplication access lost".into())
            }
            Err(err) => Err(format!("AcquireNextFrame failed: {err}")),
        }
    }

    fn copy_into_pool(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<Option<Arc<D3D11FrameTexture>>, String> {
        if self.frame_pool.is_empty() {
            self.frame_pool = create_frame_pool(&self.device, source)?;
        }

        for offset in 0..self.frame_pool.len() {
            let slot_index = (self.next_slot + offset) % self.frame_pool.len();
            let slot = &self.frame_pool[slot_index];
            if Arc::strong_count(slot) != 1 {
                continue;
            }

            unsafe {
                self.context.CopyResource(&slot.texture, source);
            }
            self.next_slot = (slot_index + 1) % self.frame_pool.len();
            return Ok(Some(Arc::clone(slot)));
        }

        Ok(None)
    }

    fn capture_cursor(&mut self) -> Option<CapturedCursor> {
        self.cursor.capture_cursor()
    }
}

fn create_frame_pool(
    device: &ID3D11Device,
    source: &ID3D11Texture2D,
) -> Result<Vec<Arc<D3D11FrameTexture>>, String> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        source.GetDesc(&mut source_desc);
    }

    let desc = D3D11_TEXTURE2D_DESC {
        Width: source_desc.Width,
        Height: source_desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: source_desc.Format,
        SampleDesc: source_desc.SampleDesc,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut pool = Vec::with_capacity(DXGI_FRAME_POOL_SIZE);
    for _ in 0..DXGI_FRAME_POOL_SIZE {
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|err| format!("CreateTexture2D for DXGI frame pool failed: {err}"))?;
        }
        pool.push(Arc::new(D3D11FrameTexture {
            texture: texture.ok_or_else(|| {
                "CreateTexture2D for DXGI frame pool returned null texture".to_string()
            })?,
        }));
    }
    Ok(pool)
}

struct CursorCapture {
    screen_dc: HDC,
    origin_x: i32,
    origin_y: i32,
    cursor_cache: Option<CapturedCursor>,
}

// SAFETY: CursorCapture owns the screen DC and is moved into a single capture
// thread before use; it is not accessed concurrently across threads.
unsafe impl Send for CursorCapture {}

impl CursorCapture {
    fn new(origin_x: i32, origin_y: i32) -> Result<Self, String> {
        let screen_dc = unsafe { GetDC(None) };
        if screen_dc.is_invalid() {
            return Err("GetDC(NULL) for cursor capture failed".into());
        }

        Ok(Self {
            screen_dc,
            origin_x,
            origin_y,
            cursor_cache: None,
        })
    }

    fn set_origin(&mut self, origin_x: i32, origin_y: i32) {
        self.origin_x = origin_x;
        self.origin_y = origin_y;
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
                    pixels: Vec::new().into(),
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
                        return Err(
                            "CreateDIBSection for cursor returned invalid objects".to_string()
                        );
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
                        pixels: pixels.into(),
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
}

impl Drop for CursorCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.screen_dc.is_invalid() {
                let _ = ReleaseDC(None, self.screen_dc);
            }
        }
    }
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
    cursor: CursorCapture,
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

            let (origin_x, origin_y, _, _) = current_virtual_screen_metrics();
            let cursor = CursorCapture::new(origin_x, origin_y)?;

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
                cursor,
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
            let cursor = self.cursor.capture_cursor();
            Ok(CapturedFrame {
                data: FrameData::Ram(pixels),
                width: self.width as u32,
                height: self.height as u32,
                cursor,
            })
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
            self.cursor.set_origin(origin_x, origin_y);
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

fn select_output() -> Result<(IDXGIAdapter1, IDXGIOutput1, DXGI_OUTPUT_DESC), String> {
    let factory: IDXGIFactory1 = unsafe {
        CreateDXGIFactory1().map_err(|err| format!("CreateDXGIFactory1 failed: {err}"))?
    };

    let mut fallback: Option<(IDXGIAdapter1, IDXGIOutput1, DXGI_OUTPUT_DESC)> = None;
    let mut adapter_index = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(format!("EnumAdapters1 failed: {err}")),
        };
        adapter_index += 1;

        let (adapter_name, vendor_id) = unsafe { adapter.GetDesc() }
            .map(|d| {
                let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
                (String::from_utf16_lossy(&d.Description[..end]), d.VendorId)
            })
            .unwrap_or_default();

        let mut output_index = 0;
        let mut adapter_has_output = false;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(err) => return Err(format!("EnumOutputs failed: {err}")),
            };
            output_index += 1;

            let output1 = output
                .cast::<IDXGIOutput1>()
                .map_err(|err| format!("IDXGIOutput->IDXGIOutput1 cast failed: {err}"))?;
            let desc = unsafe {
                output
                    .GetDesc()
                    .map_err(|err| format!("IDXGIOutput::GetDesc failed: {err}"))?
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }

            adapter_has_output = true;
            if rect_contains_point(&desc.DesktopCoordinates, 0, 0) {
                println!(
                    "[capture] Selected adapter: {adapter_name} (vendor: 0x{vendor_id:04x}) — primary display output"
                );
                return Ok((adapter, output1, desc));
            }

            if fallback.is_none() {
                fallback = Some((adapter.clone(), output1, desc));
            }
        }
        if !adapter_has_output {
            println!("[capture] Adapter {adapter_name} (vendor: 0x{vendor_id:04x}) — no display output");
        }
    }

    if let Some((ref adapter, _, _)) = fallback {
        if let Ok(d) = unsafe { adapter.GetDesc() } {
            let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
            let name = String::from_utf16_lossy(&d.Description[..end]);
            println!("[capture] Selected adapter: {name} — fallback (no primary at 0,0)");
        }
    }
    fallback.ok_or_else(|| "No attached desktop output found for DXGI duplication".into())
}

fn create_device_for_output(
    adapter: &IDXGIAdapter1,
) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_0;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .map_err(|err| format!("D3D11CreateDevice failed: {err}"))?;
    }

    let device = device.ok_or_else(|| "D3D11CreateDevice returned null device".to_string())?;
    let context =
        context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;

    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }

    println!("[capture] DXGI device created at feature level 0x{:x}", feature_level.0);
    Ok((device, context))
}

fn rect_contains_point(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
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

fn cursor_bitmap_dimensions(
    color_bitmap: HBITMAP,
    mask_bitmap: HBITMAP,
) -> Result<(i32, i32), String> {
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
