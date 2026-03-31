use st_protocol::{
    ControlMessage, ControllerState, CursorShape, CursorState, InputCapabilities, InputPacket,
    KeyboardKey, KEYBOARD_STATE_BYTES, MOUSE_BUTTON_EXTRA1, MOUSE_BUTTON_EXTRA2,
    MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_PRIMARY, MOUSE_BUTTON_SECONDARY,
    MOUSE_WHEEL_STEP_UNITS,
};
#[cfg(target_os = "linux")]
use crate::capture::linux::{active_remote_desktop_session, RemoteDesktopPortalSession};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::UdpSocket;
use std::sync::{
    atomic::{AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::POINT;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WHEEL_DELTA, XBUTTON1, XBUTTON2,
};

const MAX_CURSOR_SHAPE_RGBA_BYTES: usize = u16::MAX as usize - 16;
static TRACE_CURSOR_UPDATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRACE_CURSOR_SEND_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "linux")]
static PORTAL_ERROR_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Log Portal D-Bus errors sparingly (first 3, then every 100th).
#[cfg(target_os = "linux")]
fn log_portal_error(method: &str, err: impl std::fmt::Display) {
    let n = PORTAL_ERROR_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 3 || n % 100 == 0 {
        eprintln!("[input] Portal {method} failed (count={n}): {err}");
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use crate::capture::CapturedCursor;

pub struct InputRuntime {
    next_client_id: AtomicU32,
    inner: Mutex<InputRuntimeInner>,
}

fn trace_enabled() -> bool {
    std::env::var_os("ST_TRACE").is_some()
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn resize_rgba_premultiplied_nearest(
    src: &[u8],
    src_width: u16,
    src_height: u16,
    dst_width: u16,
    dst_height: u16,
) -> Vec<u8> {
    let src_width = src_width.max(1) as usize;
    let src_height = src_height.max(1) as usize;
    let dst_width = dst_width.max(1) as usize;
    let dst_height = dst_height.max(1) as usize;
    let mut out = vec![0u8; dst_width * dst_height * 4];
    for y in 0..dst_height {
        let src_y = y * src_height / dst_height;
        for x in 0..dst_width {
            let src_x = x * src_width / dst_width;
            let src_idx = (src_y * src_width + src_x) * 4;
            let dst_idx = (y * dst_width + x) * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
        }
    }
    out
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn fit_cursor_shape_to_payload_budget(mut shape: CursorShape) -> (CursorShape, bool) {
    if shape.rgba.len() <= MAX_CURSOR_SHAPE_RGBA_BYTES {
        return (shape, false);
    }

    let src_width = shape.width.max(1);
    let src_height = shape.height.max(1);
    let src_pixels = src_width as usize * src_height as usize;
    let max_pixels = MAX_CURSOR_SHAPE_RGBA_BYTES / 4;
    let scale = (max_pixels as f64 / src_pixels as f64).sqrt();
    let mut dst_width = ((src_width as f64) * scale).floor().max(1.0) as u16;
    let mut dst_height = ((src_height as f64) * scale).floor().max(1.0) as u16;

    while dst_width as usize * dst_height as usize * 4 > MAX_CURSOR_SHAPE_RGBA_BYTES {
        if dst_width >= dst_height && dst_width > 1 {
            dst_width -= 1;
        } else if dst_height > 1 {
            dst_height -= 1;
        } else {
            break;
        }
    }

    shape.rgba = resize_rgba_premultiplied_nearest(
        &shape.rgba,
        src_width,
        src_height,
        dst_width,
        dst_height,
    );
    shape.width = dst_width;
    shape.height = dst_height;

    let dst_w = dst_width.max(1) as u32;
    let dst_h = dst_height.max(1) as u32;
    shape.hotspot_x = (((shape.hotspot_x as u32) * dst_w + src_width as u32 / 2)
        / src_width as u32)
        .min(dst_w.saturating_sub(1)) as u16;
    shape.hotspot_y = (((shape.hotspot_y as u32) * dst_h + src_height as u32 / 2)
        / src_height as u32)
        .min(dst_h.saturating_sub(1)) as u16;

    (shape, true)
}

struct InputRuntimeInner {
    backend: InputBackend,
    backend_label: String,
    capabilities: InputCapabilities,
    controller_id: Option<u32>,
    last_input_seq_client_id: Option<u32>,
    last_input_seq: Option<u16>,
    button_mask: u8,
    keyboard_state: [u8; KEYBOARD_STATE_BYTES],
    cursor_shape: Option<CursorShape>,
    cursor_state: CursorState,
    cursor_shape_version: u64,
    cursor_state_version: u64,
}

enum InputBackend {
    Unavailable,
    #[cfg(target_os = "linux")]
    X11(X11InputController),
    #[cfg(target_os = "linux")]
    Uinput(UinputMouseController),
    #[cfg(target_os = "linux")]
    PortalRemoteDesktop(Arc<RemoteDesktopPortalSession>),
    #[cfg(target_os = "windows")]
    Windows(WindowsInputController),
    #[cfg(target_os = "macos")]
    Macos(MacosMouseController),
}

pub struct CursorVersionCursor {
    pub shape: u64,
    pub state: u64,
}

impl Default for CursorVersionCursor {
    fn default() -> Self {
        Self { shape: 0, state: 0 }
    }
}

impl InputRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_client_id: AtomicU32::new(1),
            inner: Mutex::new(InputRuntimeInner {
                backend: InputBackend::Unavailable,
                backend_label: "unavailable".to_string(),
                capabilities: InputCapabilities::default(),
                controller_id: None,
                last_input_seq_client_id: None,
                last_input_seq: None,
                button_mask: 0,
                keyboard_state: [0u8; KEYBOARD_STATE_BYTES],
                cursor_shape: None,
                cursor_state: CursorState::default(),
                cursor_shape_version: 0,
                cursor_state_version: 0,
            }),
        })
    }

    pub fn spawn_listener(self: &Arc<Self>, port: u16) {
        let runtime = Arc::clone(self);
        std::thread::spawn(move || {
            runtime.listen_loop(port);
        });
    }

    pub fn allocate_client_id(&self) -> u32 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn controller_state_for(&self, client_id: u32) -> ControllerState {
        let inner = self.inner.lock().unwrap();
        match (&inner.backend, inner.controller_id) {
            (&InputBackend::Unavailable, _) => ControllerState::Unavailable,
            (_, Some(owner)) if owner == client_id => ControllerState::OwnedByYou,
            (_, Some(_)) => ControllerState::OwnedByOther,
            _ => ControllerState::Available,
        }
    }

    pub fn capabilities(&self) -> InputCapabilities {
        self.inner.lock().unwrap().capabilities
    }

    pub fn backend_label(&self) -> String {
        self.inner.lock().unwrap().backend_label.clone()
    }

    pub fn acquire_control(&self, client_id: u32) -> ControllerState {
        let mut inner = self.inner.lock().unwrap();
        if matches!(&inner.backend, InputBackend::Unavailable) {
            return ControllerState::Unavailable;
        }
        inner.release_all_inputs();
        inner.controller_id = Some(client_id);
        inner.button_mask = 0;
        inner.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
        ControllerState::OwnedByYou
    }

    pub fn release_control(&self, client_id: u32) -> ControllerState {
        let mut inner = self.inner.lock().unwrap();
        if inner.controller_id == Some(client_id) {
            inner.release_all_inputs();
            inner.controller_id = None;
        }
        match &inner.backend {
            InputBackend::Unavailable => ControllerState::Unavailable,
            #[cfg(target_os = "linux")]
            InputBackend::X11(_) => ControllerState::Available,
            #[cfg(target_os = "linux")]
            InputBackend::Uinput(_) => ControllerState::Available,
            #[cfg(target_os = "linux")]
            InputBackend::PortalRemoteDesktop(_) => ControllerState::Available,
            #[cfg(target_os = "windows")]
            InputBackend::Windows(_) => ControllerState::Available,
            #[cfg(target_os = "macos")]
            InputBackend::Macos(_) => ControllerState::Available,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    pub fn control_active(&self) -> bool {
        self.inner.lock().unwrap().controller_id.is_some()
    }

    pub fn refresh_backend(&self, capture_backend: &str, _stream_width: u32, _stream_height: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.release_all_inputs();
        inner.controller_id = None;
        inner.button_mask = 0;
        inner.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
        inner.cursor_shape = None;
        inner.cursor_state = CursorState::default();
        inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
        inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);

        #[cfg(target_os = "linux")]
        {
            let next = select_linux_backend(capture_backend, _stream_width, _stream_height);
            match next {
                Ok((backend, capabilities, label)) => {
                    println!("[input] {label} input enabled for {capture_backend}");
                    inner.backend = backend;
                    inner.backend_label = label.to_string();
                    inner.capabilities = capabilities;
                }
                Err(err) => {
                    eprintln!("[input] input unavailable for {capture_backend}: {err}");
                    inner.backend = InputBackend::Unavailable;
                    inner.backend_label = format!("unavailable ({err})");
                    inner.capabilities = InputCapabilities::default();
                }
            }
            return;
        }

        #[cfg(target_os = "macos")]
        {
            match MacosMouseController::new() {
                Ok(controller) => {
                    println!("[input] macOS input enabled for {capture_backend}");
                    inner.backend = InputBackend::Macos(controller);
                    inner.backend_label = "macos/quartz".to_string();
                    inner.capabilities = InputCapabilities {
                        mouse_absolute: true,
                        mouse_relative: true,
                        keyboard: true,
                        separate_cursor: true,
                        hover_capture: true,
                    };
                }
                Err(err) => {
                    eprintln!("[input] macOS input unavailable: {err}");
                    inner.backend = InputBackend::Unavailable;
                    inner.backend_label = format!("unavailable ({err})");
                    inner.capabilities = InputCapabilities::default();
                }
            }
            return;
        }

        #[cfg(target_os = "windows")]
        {
            match WindowsInputController::new() {
                Ok(controller) => {
                    println!("[input] Windows input enabled for {capture_backend}");
                    inner.backend = InputBackend::Windows(controller);
                    inner.backend_label = "windows/sendinput".to_string();
                    inner.capabilities = InputCapabilities {
                        mouse_absolute: true,
                        mouse_relative: true,
                        keyboard: true,
                        separate_cursor: true,
                        hover_capture: true,
                    };
                }
                Err(err) => {
                    eprintln!("[input] Windows input unavailable: {err}");
                    inner.backend = InputBackend::Unavailable;
                    inner.backend_label = format!("unavailable ({err})");
                    inner.capabilities = InputCapabilities::default();
                }
            }
            return;
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = capture_backend;
            inner.backend = InputBackend::Unavailable;
            inner.backend_label = "unavailable".to_string();
            inner.capabilities = InputCapabilities::default();
        }
    }

    pub fn clear_for_stop(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.release_all_inputs();
        inner.backend = InputBackend::Unavailable;
        inner.backend_label = "unavailable".to_string();
        inner.capabilities = InputCapabilities::default();
        inner.controller_id = None;
        inner.button_mask = 0;
        inner.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
        inner.cursor_shape = None;
        inner.cursor_state = CursorState::default();
        inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
        inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    pub fn update_cursor(&self, cursor: Option<&CapturedCursor>) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.capabilities.separate_cursor {
            if inner.cursor_shape.is_some() || inner.cursor_state.visible {
                inner.cursor_shape = None;
                inner.cursor_state = CursorState::default();
                inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
                inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
            }
            return;
        }

        let Some(cursor) = cursor else {
            return;
        };

        if !cursor.visible || cursor.width == 0 || cursor.height == 0 || cursor.pixels.is_empty() {
            let serial = if cursor.shape_serial != 0 {
                cursor.shape_serial
            } else {
                inner
                    .cursor_shape
                    .as_ref()
                    .map(|shape| shape.serial)
                    .unwrap_or(0)
            };
            let next_state = CursorState {
                serial,
                x: cursor.x,
                y: cursor.y,
                visible: cursor.visible,
            };
            if inner.cursor_state != next_state {
                inner.cursor_state = next_state;
                inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
                if trace_enabled() {
                    let log_idx =
                        TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                    if log_idx < 12 {
                        eprintln!(
                            "[trace][cursor] updated state serial={} pos=({}, {}) visible={} (no-shape)",
                            inner.cursor_state.serial,
                            inner.cursor_state.x,
                            inner.cursor_state.y,
                            inner.cursor_state.visible
                        );
                    }
                }
            }
            return;
        }

        let (next_shape, resized) = fit_cursor_shape_to_payload_budget(CursorShape {
            serial: cursor.shape_serial,
            width: cursor.width.min(u16::MAX as u32) as u16,
            height: cursor.height.min(u16::MAX as u32) as u16,
            hotspot_x: cursor.hotspot_x.min(u16::MAX as u32) as u16,
            hotspot_y: cursor.hotspot_y.min(u16::MAX as u32) as u16,
            rgba: bgra_to_rgba_premultiplied(&cursor.pixels),
        });
        if inner
            .cursor_shape
            .as_ref()
            .map(|shape| {
                shape.serial != next_shape.serial
                    || shape.width != next_shape.width
                    || shape.height != next_shape.height
                    || shape.hotspot_x != next_shape.hotspot_x
                    || shape.hotspot_y != next_shape.hotspot_y
                    || shape.rgba.as_slice() != next_shape.rgba.as_slice()
            })
            .unwrap_or(true)
        {
            inner.cursor_shape = Some(next_shape);
            inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
            if trace_enabled() {
                let log_idx = TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if log_idx < 12 {
                    let shape = inner.cursor_shape.as_ref().unwrap();
                    eprintln!(
                        "[trace][cursor] updated shape serial={} {}x{} hotspot=({}, {}) resized={resized}",
                        shape.serial, shape.width, shape.height, shape.hotspot_x, shape.hotspot_y
                    );
                }
            }
        }

        let serial = inner
            .cursor_shape
            .as_ref()
            .map(|shape| shape.serial)
            .unwrap_or(0);
        let next_state = CursorState {
            serial,
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible,
        };
        if inner.cursor_state != next_state {
            inner.cursor_state = next_state;
            inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
            if trace_enabled() {
                let log_idx = TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if log_idx < 12 {
                    eprintln!(
                        "[trace][cursor] updated state serial={} pos=({}, {}) visible={}",
                        inner.cursor_state.serial,
                        inner.cursor_state.x,
                        inner.cursor_state.y,
                        inner.cursor_state.visible
                    );
                }
            }
        }
    }

    pub fn cursor_messages(
        &self,
        client_id: u32,
        versions: &mut CursorVersionCursor,
    ) -> Vec<ControlMessage> {
        let inner = self.inner.lock().unwrap();
        if inner.controller_id != Some(client_id) {
            return Vec::new();
        }

        let mut messages = Vec::new();
        if inner.cursor_shape_version > versions.shape {
            versions.shape = inner.cursor_shape_version;
            if let Some(shape) = inner.cursor_shape.clone() {
                if shape.rgba.len() <= MAX_CURSOR_SHAPE_RGBA_BYTES {
                    if trace_enabled() {
                        let log_idx = TRACE_CURSOR_SEND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                        if log_idx < 12 {
                            eprintln!(
                                "[trace][cursor] sending CursorShape serial={} {}x{} bytes={}",
                                shape.serial,
                                shape.width,
                                shape.height,
                                shape.rgba.len()
                            );
                        }
                    }
                    messages.push(ControlMessage::CursorShape(shape));
                } else {
                    eprintln!(
                        "[input] cursor shape {}x{} exceeds control payload budget ({} bytes), skipping",
                        shape.width,
                        shape.height,
                        shape.rgba.len()
                    );
                }
            }
        }
        if inner.cursor_state_version > versions.state {
            versions.state = inner.cursor_state_version;
            if !(inner.cursor_state.serial == 0
                && !inner.cursor_state.visible
                && inner.cursor_shape.is_none())
            {
                if trace_enabled() {
                    let log_idx = TRACE_CURSOR_SEND_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                    if log_idx < 12 {
                        eprintln!(
                            "[trace][cursor] sending CursorState serial={} pos=({}, {}) visible={}",
                            inner.cursor_state.serial,
                            inner.cursor_state.x,
                            inner.cursor_state.y,
                            inner.cursor_state.visible
                        );
                    }
                }
                messages.push(ControlMessage::CursorState(inner.cursor_state));
            }
        }
        messages
    }

    fn listen_loop(self: Arc<Self>, port: u16) {
        let socket = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => socket,
            Err(err) => {
                eprintln!("[input] bind UDP {port} failed: {err}");
                return;
            }
        };
        let mut buf = [0u8; 1500];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, _addr)) => {
                    if let Some((header, packet)) = InputPacket::deserialize(&buf[..n]) {
                        self.handle_input_packet(header.seq, packet);
                    }
                }
                Err(err) => {
                    eprintln!("[input] UDP receive failed: {err}");
                    break;
                }
            }
        }
    }

    pub fn handle_input_packet(&self, seq: u16, packet: InputPacket) {
        let mut inner = self.inner.lock().unwrap();
        let client_id = match packet {
            InputPacket::MouseAbsolute(packet) => packet.client_id,
            InputPacket::MouseRelative(packet) => packet.client_id,
            InputPacket::MouseButtons(packet) => packet.client_id,
            InputPacket::MouseWheel(packet) => packet.client_id,
            InputPacket::KeyboardState(packet) => packet.client_id,
        };
        if inner.controller_id != Some(client_id) {
            return;
        }
        if !inner.accept_input_seq(client_id, seq) {
            return;
        }

        match packet {
            InputPacket::MouseAbsolute(packet) => {
                inner.sync_buttons(packet.buttons);
                inner.move_absolute(packet.x, packet.y);
            }
            InputPacket::MouseRelative(packet) => {
                inner.sync_buttons(packet.buttons);
                inner.move_relative(packet.dx, packet.dy);
            }
            InputPacket::MouseButtons(packet) => {
                inner.sync_buttons(packet.buttons);
            }
            InputPacket::MouseWheel(packet) => {
                inner.sync_buttons(packet.buttons);
                inner.scroll(packet.delta_x, packet.delta_y);
            }
            InputPacket::KeyboardState(packet) => {
                inner.sync_keyboard(packet.pressed);
            }
        }
    }
}

impl InputRuntimeInner {
    fn accept_input_seq(&mut self, client_id: u32, seq: u16) -> bool {
        if self.last_input_seq_client_id == Some(client_id) {
            if let Some(last_seq) = self.last_input_seq {
                if !input_seq_is_newer(seq, last_seq) {
                    return false;
                }
            }
        }

        self.last_input_seq_client_id = Some(client_id);
        self.last_input_seq = Some(seq);
        true
    }

    fn release_all_inputs(&mut self) {
        self.release_buttons();
        self.release_keyboard();
    }

    fn release_buttons(&mut self) {
        self.sync_buttons(0);
    }

    fn release_keyboard(&mut self) {
        self.sync_keyboard([0u8; KEYBOARD_STATE_BYTES]);
    }

    fn sync_buttons(&mut self, next: u8) {
        let changed = self.button_mask ^ next;
        if changed == 0 {
            return;
        }
        for (bit, button) in button_mappings() {
            if changed & bit != 0 {
                let pressed = next & bit != 0;
                match &mut self.backend {
                    InputBackend::Unavailable => {}
                    #[cfg(target_os = "linux")]
                    InputBackend::X11(controller) => controller.button(button, pressed),
                    #[cfg(target_os = "linux")]
                    InputBackend::Uinput(controller) => controller.button(button, pressed),
                    #[cfg(target_os = "linux")]
                    InputBackend::PortalRemoteDesktop(controller) => {
                        if let Some(code) = linux_button_code(button) {
                            if let Err(e) = controller.notify_pointer_button(code, pressed) {
                                log_portal_error("notify_pointer_button", e);
                            }
                        }
                    }
                    #[cfg(target_os = "windows")]
                    InputBackend::Windows(controller) => controller.button(button, pressed),
                    #[cfg(target_os = "macos")]
                    InputBackend::Macos(controller) => controller.button(button, pressed),
                }
            }
        }
        self.button_mask = next;
    }

    fn move_absolute(&mut self, x: u16, y: u16) {
        match &mut self.backend {
            InputBackend::Unavailable => {}
            #[cfg(target_os = "linux")]
            InputBackend::X11(controller) => controller.move_absolute(x, y),
            #[cfg(target_os = "linux")]
            InputBackend::Uinput(controller) => controller.move_absolute(x, y),
            #[cfg(target_os = "linux")]
            InputBackend::PortalRemoteDesktop(controller) => {
                if let Err(e) = controller.notify_pointer_motion_absolute(x, y) {
                    log_portal_error("notify_pointer_motion_absolute", e);
                }
            }
            #[cfg(target_os = "windows")]
            InputBackend::Windows(controller) => controller.move_absolute(x, y),
            #[cfg(target_os = "macos")]
            InputBackend::Macos(controller) => controller.move_absolute(x, y),
        }
    }

    fn move_relative(&mut self, dx: i16, dy: i16) {
        match &mut self.backend {
            InputBackend::Unavailable => {}
            #[cfg(target_os = "linux")]
            InputBackend::X11(controller) => controller.move_relative(dx, dy),
            #[cfg(target_os = "linux")]
            InputBackend::Uinput(controller) => controller.move_relative(dx, dy),
            #[cfg(target_os = "linux")]
            InputBackend::PortalRemoteDesktop(controller) => {
                if let Err(e) = controller.notify_pointer_motion_relative(dx, dy) {
                    log_portal_error("notify_pointer_motion_relative", e);
                }
            }
            #[cfg(target_os = "windows")]
            InputBackend::Windows(controller) => controller.move_relative(dx, dy),
            #[cfg(target_os = "macos")]
            InputBackend::Macos(controller) => controller.move_relative(dx, dy),
        }
    }

    fn scroll(&mut self, delta_x: i16, delta_y: i16) {
        match &mut self.backend {
            InputBackend::Unavailable => {}
            #[cfg(target_os = "linux")]
            InputBackend::X11(controller) => controller.scroll(delta_x, delta_y),
            #[cfg(target_os = "linux")]
            InputBackend::Uinput(controller) => controller.scroll(delta_x, delta_y),
            #[cfg(target_os = "linux")]
            InputBackend::PortalRemoteDesktop(controller) => {
                if let Err(e) = controller.notify_pointer_axis_units(delta_x, delta_y) {
                    log_portal_error("notify_pointer_axis_units", e);
                }
            }
            #[cfg(target_os = "windows")]
            InputBackend::Windows(controller) => controller.scroll(delta_x, delta_y),
            #[cfg(target_os = "macos")]
            InputBackend::Macos(controller) => controller.scroll(delta_x, delta_y),
        }
    }

    fn sync_keyboard(&mut self, next: [u8; KEYBOARD_STATE_BYTES]) {
        for index in 0..KeyboardKey::COUNT {
            let byte = index / 8;
            let bit = 1 << (index % 8);
            let was_pressed = self.keyboard_state[byte] & bit != 0;
            let now_pressed = next[byte] & bit != 0;
            if was_pressed == now_pressed {
                continue;
            }
            let Some(key) = KeyboardKey::from_u8(index as u8) else {
                continue;
            };
            match &mut self.backend {
                InputBackend::Unavailable => {}
                #[cfg(target_os = "linux")]
                InputBackend::X11(controller) => controller.key(key, now_pressed),
                #[cfg(target_os = "linux")]
                InputBackend::Uinput(controller) => controller.key(key, now_pressed),
                #[cfg(target_os = "linux")]
                InputBackend::PortalRemoteDesktop(controller) => {
                    if let Some(code) = linux_key_code(key) {
                        if let Err(e) = controller.notify_keyboard_keycode(code, now_pressed) {
                            log_portal_error("notify_keyboard_keycode", e);
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                InputBackend::Windows(controller) => controller.key(key, now_pressed),
                #[cfg(target_os = "macos")]
                InputBackend::Macos(controller) => controller.key(key, now_pressed),
            }
        }
        self.keyboard_state = next;
    }
}

fn input_seq_is_newer(seq: u16, last_seq: u16) -> bool {
    let delta = seq.wrapping_sub(last_seq);
    delta != 0 && delta < 0x8000
}

fn button_mappings() -> [(u8, u32); 5] {
    [
        (MOUSE_BUTTON_PRIMARY, 1),
        (MOUSE_BUTTON_MIDDLE, 2),
        (MOUSE_BUTTON_SECONDARY, 3),
        (MOUSE_BUTTON_EXTRA1, 8),
        (MOUSE_BUTTON_EXTRA2, 9),
    ]
}

#[derive(Default)]
struct WheelAccumulator {
    x_units: i32,
    y_units: i32,
}

impl WheelAccumulator {
    fn push_and_take_steps(&mut self, delta_x: i16, delta_y: i16) -> (i16, i16) {
        (
            wheel_units_to_steps(&mut self.x_units, delta_x),
            wheel_units_to_steps(&mut self.y_units, delta_y),
        )
    }
}

fn wheel_units_to_steps(pending_units: &mut i32, delta_units: i16) -> i16 {
    *pending_units += i32::from(delta_units);
    let step_units = i32::from(MOUSE_WHEEL_STEP_UNITS);
    let steps = (*pending_units / step_units)
        .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    *pending_units -= i32::from(steps) * step_units;
    steps
}

#[cfg(target_os = "windows")]
struct WindowsInputController {
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,
    tracked_x: i32,
    tracked_y: i32,
}

#[cfg(target_os = "windows")]
impl WindowsInputController {
    fn new() -> Result<Self, String> {
        let (origin_x, origin_y, width, height) = windows_virtual_screen_metrics();
        if width <= 0 || height <= 0 {
            return Err("virtual desktop size unavailable".into());
        }

        let mut point = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
        }

        Ok(Self {
            origin_x,
            origin_y,
            width,
            height,
            tracked_x: point.x,
            tracked_y: point.y,
        })
    }

    fn move_absolute(&mut self, x: u16, y: u16) {
        self.refresh_virtual_screen();
        let width = self.width.max(1) as i64;
        let height = self.height.max(1) as i64;
        self.tracked_x =
            self.origin_x + ((x as i64 * (width - 1).max(0) + 32767) / 65535) as i32;
        self.tracked_y =
            self.origin_y + ((y as i64 * (height - 1).max(0) + 32767) / 65535) as i32;

        self.send_mouse(MOUSEINPUT {
            dx: normalize_windows_absolute(self.tracked_x - self.origin_x, self.width),
            dy: normalize_windows_absolute(self.tracked_y - self.origin_y, self.height),
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    fn move_relative(&mut self, dx: i16, dy: i16) {
        self.refresh_virtual_screen();
        self.tracked_x =
            (self.tracked_x + dx as i32).clamp(self.origin_x, self.origin_x + self.width - 1);
        self.tracked_y =
            (self.tracked_y + dy as i32).clamp(self.origin_y, self.origin_y + self.height - 1);
        self.send_mouse(MOUSEINPUT {
            dx: dx as i32,
            dy: dy as i32,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    fn button(&mut self, button: u32, pressed: bool) {
        let (flags, mouse_data) = match (button, pressed) {
            (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (1, false) => (MOUSEEVENTF_LEFTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (8, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (8, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (9, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (9, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
            _ => return,
        };
        self.send_mouse(MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: mouse_data,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    fn scroll(&mut self, delta_x: i16, delta_y: i16) {
        if delta_y != 0 {
            self.send_mouse(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: delta_y as i32 as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
        }
        if delta_x != 0 {
            self.send_mouse(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: delta_x as i32 as u32,
                dwFlags: MOUSEEVENTF_HWHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
        }
    }

    fn key(&mut self, key: KeyboardKey, pressed: bool) {
        let Some((scan_code, extended)) = windows_key_scan_code(key) else {
            return;
        };
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        self.send_keyboard(KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: scan_code,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    fn refresh_virtual_screen(&mut self) {
        let (origin_x, origin_y, width, height) = windows_virtual_screen_metrics();
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        self.width = width.max(1);
        self.height = height.max(1);
    }

    fn send_mouse(&self, input: MOUSEINPUT) {
        let inputs = [INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: input },
        }];
        unsafe {
            let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn send_keyboard(&self, input: KEYBDINPUT) {
        let inputs = [INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: input },
        }];
        unsafe {
            let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_virtual_screen_metrics() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

#[cfg(target_os = "windows")]
fn normalize_windows_absolute(coord: i32, span: i32) -> i32 {
    if span <= 1 {
        0
    } else {
        (((coord as i64) * 65535 + ((span - 1) as i64 / 2)) / ((span - 1) as i64)) as i32
    }
}

#[cfg(target_os = "windows")]
fn windows_key_scan_code(key: KeyboardKey) -> Option<(u16, bool)> {
    Some(match key {
        KeyboardKey::Escape => (0x01, false),
        KeyboardKey::Tab => (0x0F, false),
        KeyboardKey::Backspace => (0x0E, false),
        KeyboardKey::Enter => (0x1C, false),
        KeyboardKey::Space => (0x39, false),
        KeyboardKey::Insert => (0x52, true),
        KeyboardKey::Delete => (0x53, true),
        KeyboardKey::Home => (0x47, true),
        KeyboardKey::End => (0x4F, true),
        KeyboardKey::PageUp => (0x49, true),
        KeyboardKey::PageDown => (0x51, true),
        KeyboardKey::ArrowUp => (0x48, true),
        KeyboardKey::ArrowDown => (0x50, true),
        KeyboardKey::ArrowLeft => (0x4B, true),
        KeyboardKey::ArrowRight => (0x4D, true),
        KeyboardKey::Minus => (0x0C, false),
        KeyboardKey::Equals => (0x0D, false),
        KeyboardKey::OpenBracket => (0x1A, false),
        KeyboardKey::CloseBracket => (0x1B, false),
        KeyboardKey::Backslash => (0x2B, false),
        KeyboardKey::Semicolon => (0x27, false),
        KeyboardKey::Quote => (0x28, false),
        KeyboardKey::Backtick => (0x29, false),
        KeyboardKey::Comma => (0x33, false),
        KeyboardKey::Period => (0x34, false),
        KeyboardKey::Slash => (0x35, false),
        KeyboardKey::Num0 => (0x0B, false),
        KeyboardKey::Num1 => (0x02, false),
        KeyboardKey::Num2 => (0x03, false),
        KeyboardKey::Num3 => (0x04, false),
        KeyboardKey::Num4 => (0x05, false),
        KeyboardKey::Num5 => (0x06, false),
        KeyboardKey::Num6 => (0x07, false),
        KeyboardKey::Num7 => (0x08, false),
        KeyboardKey::Num8 => (0x09, false),
        KeyboardKey::Num9 => (0x0A, false),
        KeyboardKey::A => (0x1E, false),
        KeyboardKey::B => (0x30, false),
        KeyboardKey::C => (0x2E, false),
        KeyboardKey::D => (0x20, false),
        KeyboardKey::E => (0x12, false),
        KeyboardKey::F => (0x21, false),
        KeyboardKey::G => (0x22, false),
        KeyboardKey::H => (0x23, false),
        KeyboardKey::I => (0x17, false),
        KeyboardKey::J => (0x24, false),
        KeyboardKey::K => (0x25, false),
        KeyboardKey::L => (0x26, false),
        KeyboardKey::M => (0x32, false),
        KeyboardKey::N => (0x31, false),
        KeyboardKey::O => (0x18, false),
        KeyboardKey::P => (0x19, false),
        KeyboardKey::Q => (0x10, false),
        KeyboardKey::R => (0x13, false),
        KeyboardKey::S => (0x1F, false),
        KeyboardKey::T => (0x14, false),
        KeyboardKey::U => (0x16, false),
        KeyboardKey::V => (0x2F, false),
        KeyboardKey::W => (0x11, false),
        KeyboardKey::X => (0x2D, false),
        KeyboardKey::Y => (0x15, false),
        KeyboardKey::Z => (0x2C, false),
        KeyboardKey::F1 => (0x3B, false),
        KeyboardKey::F2 => (0x3C, false),
        KeyboardKey::F3 => (0x3D, false),
        KeyboardKey::F4 => (0x3E, false),
        KeyboardKey::F5 => (0x3F, false),
        KeyboardKey::F6 => (0x40, false),
        KeyboardKey::F7 => (0x41, false),
        KeyboardKey::F8 => (0x42, false),
        KeyboardKey::F9 => (0x43, false),
        KeyboardKey::F10 => (0x44, false),
        KeyboardKey::F11 => (0x57, false),
        KeyboardKey::F12 => (0x58, false),
        KeyboardKey::LeftShift => (0x2A, false),
        KeyboardKey::LeftCtrl => (0x1D, false),
        KeyboardKey::LeftAlt => (0x38, false),
        KeyboardKey::LeftMeta => (0x5B, true),
        KeyboardKey::RightShift => (0x36, false),
        KeyboardKey::RightCtrl => (0x1D, true),
        KeyboardKey::RightAlt => (0x38, true),
        KeyboardKey::RightMeta => (0x5C, true),
    })
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn bgra_to_rgba_premultiplied(src: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(src.len());
    for chunk in src.chunks_exact(4) {
        rgba.push(chunk[2]);
        rgba.push(chunk[1]);
        rgba.push(chunk[0]);
        rgba.push(chunk[3]);
    }
    rgba
}

#[cfg(target_os = "linux")]
fn select_linux_backend(
    capture_backend: &str,
    stream_width: u32,
    stream_height: u32,
) -> Result<(InputBackend, InputCapabilities, &'static str), String> {
    match capture_backend {
        "x11" => match X11InputController::new() {
            Ok(controller) => Ok((
                InputBackend::X11(controller),
                InputCapabilities {
                    mouse_absolute: true,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: true,
                    hover_capture: true,
                },
                "x11/xtest",
            )),
            Err(x11_err) => match UinputMouseController::new() {
                Ok(controller) => Ok((
                    InputBackend::Uinput(controller),
                    InputCapabilities {
                        mouse_absolute: false,
                        mouse_relative: true,
                        keyboard: true,
                        separate_cursor: true,
                        hover_capture: false,
                    },
                    "uinput(rel)",
                )),
                Err(uinput_err) => Err(format!("x11={x11_err}; uinput={uinput_err}")),
            },
        },
        "nvfbc" => match X11InputController::new() {
            Ok(controller) => Ok((
                InputBackend::X11(controller),
                InputCapabilities {
                    mouse_absolute: true,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: false,
                    hover_capture: true,
                },
                "x11/xtest",
            )),
            Err(x11_err) => match UinputMouseController::new() {
                Ok(controller) => Ok((
                    InputBackend::Uinput(controller),
                    InputCapabilities {
                        mouse_absolute: false,
                        mouse_relative: true,
                        keyboard: true,
                        separate_cursor: false,
                        hover_capture: false,
                    },
                    "uinput(rel)",
                )),
                Err(uinput_err) => Err(format!("x11={x11_err}; uinput={uinput_err}")),
            },
        },
        "wayland-screencopy" => UinputMouseController::new().map(|controller| {
            (
                InputBackend::Uinput(controller),
                InputCapabilities {
                    mouse_absolute: false,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: false,
                    hover_capture: false,
                },
                "uinput(rel)",
            )
        }),
        "pipewire" => {
            if let Some(session) = active_remote_desktop_session() {
                session.set_logical_size(stream_width, stream_height);
                Ok((
                    InputBackend::PortalRemoteDesktop(session),
                    InputCapabilities {
                        mouse_absolute: true,
                        mouse_relative: true,
                        keyboard: true,
                        separate_cursor: true,
                        hover_capture: true,
                    },
                    "portal/remote-desktop",
                ))
            } else {
                UinputMouseController::new().map(|controller| {
                    (
                        InputBackend::Uinput(controller),
                        InputCapabilities {
                            mouse_absolute: false,
                            mouse_relative: true,
                            keyboard: true,
                            separate_cursor: true,
                            hover_capture: false,
                        },
                        "uinput(rel)",
                    )
                })
            }
        }
        "kms" => UinputMouseController::new().map(|controller| {
            (
                InputBackend::Uinput(controller),
                InputCapabilities {
                    mouse_absolute: false,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: true,
                    hover_capture: false,
                },
                "uinput(rel)",
            )
        }),
        other => Err(format!("unsupported capture backend '{other}'")),
    }
}

#[cfg(target_os = "linux")]
const UINPUT_NAME_LEN: usize = 80;
#[cfg(target_os = "linux")]
const BUS_USB: u16 = 0x03;
#[cfg(target_os = "linux")]
const EV_SYN: u16 = 0x00;
#[cfg(target_os = "linux")]
const EV_KEY: u16 = 0x01;
#[cfg(target_os = "linux")]
const EV_REL: u16 = 0x02;
#[cfg(target_os = "linux")]
const SYN_REPORT: u16 = 0;
#[cfg(target_os = "linux")]
const BTN_LEFT: u16 = 0x110;
#[cfg(target_os = "linux")]
const BTN_RIGHT: u16 = 0x111;
#[cfg(target_os = "linux")]
const BTN_MIDDLE: u16 = 0x112;
#[cfg(target_os = "linux")]
const BTN_SIDE: u16 = 0x113;
#[cfg(target_os = "linux")]
const BTN_EXTRA: u16 = 0x114;
#[cfg(target_os = "linux")]
const REL_X: u16 = 0x00;
#[cfg(target_os = "linux")]
const REL_Y: u16 = 0x01;
#[cfg(target_os = "linux")]
const REL_HWHEEL: u16 = 0x06;
#[cfg(target_os = "linux")]
const REL_WHEEL: u16 = 0x08;
#[cfg(target_os = "linux")]
const REL_WHEEL_HI_RES: u16 = 0x0b;
#[cfg(target_os = "linux")]
const REL_HWHEEL_HI_RES: u16 = 0x0c;
#[cfg(target_os = "linux")]
const KEY_ESC: u16 = 1;
#[cfg(target_os = "linux")]
const KEY_1: u16 = 2;
#[cfg(target_os = "linux")]
const KEY_2: u16 = 3;
#[cfg(target_os = "linux")]
const KEY_3: u16 = 4;
#[cfg(target_os = "linux")]
const KEY_4: u16 = 5;
#[cfg(target_os = "linux")]
const KEY_5: u16 = 6;
#[cfg(target_os = "linux")]
const KEY_6: u16 = 7;
#[cfg(target_os = "linux")]
const KEY_7: u16 = 8;
#[cfg(target_os = "linux")]
const KEY_8: u16 = 9;
#[cfg(target_os = "linux")]
const KEY_9: u16 = 10;
#[cfg(target_os = "linux")]
const KEY_0: u16 = 11;
#[cfg(target_os = "linux")]
const KEY_MINUS: u16 = 12;
#[cfg(target_os = "linux")]
const KEY_EQUAL: u16 = 13;
#[cfg(target_os = "linux")]
const KEY_BACKSPACE: u16 = 14;
#[cfg(target_os = "linux")]
const KEY_TAB: u16 = 15;
#[cfg(target_os = "linux")]
const KEY_Q: u16 = 16;
#[cfg(target_os = "linux")]
const KEY_W: u16 = 17;
#[cfg(target_os = "linux")]
const KEY_E: u16 = 18;
#[cfg(target_os = "linux")]
const KEY_R: u16 = 19;
#[cfg(target_os = "linux")]
const KEY_T: u16 = 20;
#[cfg(target_os = "linux")]
const KEY_Y: u16 = 21;
#[cfg(target_os = "linux")]
const KEY_U: u16 = 22;
#[cfg(target_os = "linux")]
const KEY_I: u16 = 23;
#[cfg(target_os = "linux")]
const KEY_O: u16 = 24;
#[cfg(target_os = "linux")]
const KEY_P: u16 = 25;
#[cfg(target_os = "linux")]
const KEY_LEFTBRACE: u16 = 26;
#[cfg(target_os = "linux")]
const KEY_RIGHTBRACE: u16 = 27;
#[cfg(target_os = "linux")]
const KEY_ENTER: u16 = 28;
#[cfg(target_os = "linux")]
const KEY_LEFTCTRL: u16 = 29;
#[cfg(target_os = "linux")]
const KEY_A: u16 = 30;
#[cfg(target_os = "linux")]
const KEY_S: u16 = 31;
#[cfg(target_os = "linux")]
const KEY_D: u16 = 32;
#[cfg(target_os = "linux")]
const KEY_F: u16 = 33;
#[cfg(target_os = "linux")]
const KEY_G: u16 = 34;
#[cfg(target_os = "linux")]
const KEY_H: u16 = 35;
#[cfg(target_os = "linux")]
const KEY_J: u16 = 36;
#[cfg(target_os = "linux")]
const KEY_K: u16 = 37;
#[cfg(target_os = "linux")]
const KEY_L: u16 = 38;
#[cfg(target_os = "linux")]
const KEY_SEMICOLON: u16 = 39;
#[cfg(target_os = "linux")]
const KEY_APOSTROPHE: u16 = 40;
#[cfg(target_os = "linux")]
const KEY_GRAVE: u16 = 41;
#[cfg(target_os = "linux")]
const KEY_LEFTSHIFT: u16 = 42;
#[cfg(target_os = "linux")]
const KEY_BACKSLASH: u16 = 43;
#[cfg(target_os = "linux")]
const KEY_Z: u16 = 44;
#[cfg(target_os = "linux")]
const KEY_X: u16 = 45;
#[cfg(target_os = "linux")]
const KEY_C: u16 = 46;
#[cfg(target_os = "linux")]
const KEY_V: u16 = 47;
#[cfg(target_os = "linux")]
const KEY_B: u16 = 48;
#[cfg(target_os = "linux")]
const KEY_N: u16 = 49;
#[cfg(target_os = "linux")]
const KEY_M: u16 = 50;
#[cfg(target_os = "linux")]
const KEY_COMMA: u16 = 51;
#[cfg(target_os = "linux")]
const KEY_DOT: u16 = 52;
#[cfg(target_os = "linux")]
const KEY_SLASH: u16 = 53;
#[cfg(target_os = "linux")]
const KEY_RIGHTSHIFT: u16 = 54;
#[cfg(target_os = "linux")]
const KEY_LEFTALT: u16 = 56;
#[cfg(target_os = "linux")]
const KEY_SPACE: u16 = 57;
#[cfg(target_os = "linux")]
const KEY_F1: u16 = 59;
#[cfg(target_os = "linux")]
const KEY_F2: u16 = 60;
#[cfg(target_os = "linux")]
const KEY_F3: u16 = 61;
#[cfg(target_os = "linux")]
const KEY_F4: u16 = 62;
#[cfg(target_os = "linux")]
const KEY_F5: u16 = 63;
#[cfg(target_os = "linux")]
const KEY_F6: u16 = 64;
#[cfg(target_os = "linux")]
const KEY_F7: u16 = 65;
#[cfg(target_os = "linux")]
const KEY_F8: u16 = 66;
#[cfg(target_os = "linux")]
const KEY_F9: u16 = 67;
#[cfg(target_os = "linux")]
const KEY_F10: u16 = 68;
#[cfg(target_os = "linux")]
const KEY_F11: u16 = 87;
#[cfg(target_os = "linux")]
const KEY_F12: u16 = 88;
#[cfg(target_os = "linux")]
const KEY_RIGHTCTRL: u16 = 97;
#[cfg(target_os = "linux")]
const KEY_HOME: u16 = 102;
#[cfg(target_os = "linux")]
const KEY_UP: u16 = 103;
#[cfg(target_os = "linux")]
const KEY_PAGEUP: u16 = 104;
#[cfg(target_os = "linux")]
const KEY_LEFT: u16 = 105;
#[cfg(target_os = "linux")]
const KEY_RIGHT: u16 = 106;
#[cfg(target_os = "linux")]
const KEY_END: u16 = 107;
#[cfg(target_os = "linux")]
const KEY_DOWN: u16 = 108;
#[cfg(target_os = "linux")]
const KEY_PAGEDOWN: u16 = 109;
#[cfg(target_os = "linux")]
const KEY_INSERT: u16 = 110;
#[cfg(target_os = "linux")]
const KEY_DELETE: u16 = 111;
#[cfg(target_os = "linux")]
const KEY_LEFTMETA: u16 = 125;
#[cfg(target_os = "linux")]
const KEY_RIGHTMETA: u16 = 126;
#[cfg(target_os = "linux")]
const KEY_RIGHTALT: u16 = 100;

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxInputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct UinputSetup {
    id: LinuxInputId,
    name: [u8; UINPUT_NAME_LEN],
    ff_effects_max: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxInputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[cfg(target_os = "linux")]
nix::ioctl_write_int!(ui_set_evbit, b'U', 100);
#[cfg(target_os = "linux")]
nix::ioctl_write_int!(ui_set_keybit, b'U', 101);
#[cfg(target_os = "linux")]
nix::ioctl_write_int!(ui_set_relbit, b'U', 102);
#[cfg(target_os = "linux")]
nix::ioctl_write_ptr!(ui_dev_setup, b'U', 3, UinputSetup);
#[cfg(target_os = "linux")]
nix::ioctl_none!(ui_dev_create, b'U', 1);
#[cfg(target_os = "linux")]
nix::ioctl_none!(ui_dev_destroy, b'U', 2);

#[cfg(target_os = "linux")]
struct UinputMouseController {
    file: File,
    wheel_accumulator: WheelAccumulator,
}

#[cfg(target_os = "linux")]
impl UinputMouseController {
    fn new() -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
            .or_else(|_| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/input/uinput")
            })
            .map_err(|e| format!("open uinput: {e}"))?;

        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        unsafe {
            ui_set_evbit(fd, EV_KEY as _).map_err(|e| format!("UI_SET_EVBIT key: {e}"))?;
            ui_set_evbit(fd, EV_REL as _).map_err(|e| format!("UI_SET_EVBIT rel: {e}"))?;
            ui_set_keybit(fd, BTN_LEFT as _).map_err(|e| format!("UI_SET_KEYBIT left: {e}"))?;
            ui_set_keybit(fd, BTN_RIGHT as _).map_err(|e| format!("UI_SET_KEYBIT right: {e}"))?;
            ui_set_keybit(fd, BTN_MIDDLE as _).map_err(|e| format!("UI_SET_KEYBIT middle: {e}"))?;
            ui_set_keybit(fd, BTN_SIDE as _).map_err(|e| format!("UI_SET_KEYBIT side: {e}"))?;
            ui_set_keybit(fd, BTN_EXTRA as _).map_err(|e| format!("UI_SET_KEYBIT extra: {e}"))?;
            for key in SUPPORTED_KEYBOARD_KEYS {
                if let Some(code) = linux_key_code(key) {
                    ui_set_keybit(fd, code as _)
                        .map_err(|e| format!("UI_SET_KEYBIT keyboard {code}: {e}"))?;
                }
            }
            ui_set_relbit(fd, REL_X as _).map_err(|e| format!("UI_SET_RELBIT x: {e}"))?;
            ui_set_relbit(fd, REL_Y as _).map_err(|e| format!("UI_SET_RELBIT y: {e}"))?;
            ui_set_relbit(fd, REL_WHEEL as _).map_err(|e| format!("UI_SET_RELBIT wheel: {e}"))?;
            ui_set_relbit(fd, REL_HWHEEL as _).map_err(|e| format!("UI_SET_RELBIT hwheel: {e}"))?;
            ui_set_relbit(fd, REL_WHEEL_HI_RES as _)
                .map_err(|e| format!("UI_SET_RELBIT wheel hi-res: {e}"))?;
            ui_set_relbit(fd, REL_HWHEEL_HI_RES as _)
                .map_err(|e| format!("UI_SET_RELBIT hwheel hi-res: {e}"))?;
        }

        let mut setup = UinputSetup {
            id: LinuxInputId {
                bustype: BUS_USB,
                vendor: 0x1209,
                product: 0x5354,
                version: 1,
            },
            name: [0u8; UINPUT_NAME_LEN],
            ff_effects_max: 0,
        };
        let name = b"st-virtual-mouse";
        setup.name[..name.len()].copy_from_slice(name);

        unsafe {
            ui_dev_setup(fd, &setup).map_err(|e| format!("UI_DEV_SETUP: {e}"))?;
            ui_dev_create(fd).map_err(|e| format!("UI_DEV_CREATE: {e}"))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        Ok(Self {
            file,
            wheel_accumulator: WheelAccumulator::default(),
        })
    }

    fn move_absolute(&mut self, _x: u16, _y: u16) {}

    fn move_relative(&mut self, dx: i16, dy: i16) {
        if dx != 0 {
            let _ = self.emit(EV_REL, REL_X, dx as i32);
        }
        if dy != 0 {
            let _ = self.emit(EV_REL, REL_Y, dy as i32);
        }
        let _ = self.sync();
    }

    fn button(&mut self, button: u32, pressed: bool) {
        let Some(code) = linux_button_code(button) else {
            return;
        };
        let _ = self.emit(EV_KEY, code, i32::from(pressed));
        let _ = self.sync();
    }

    fn scroll(&mut self, delta_x: i16, delta_y: i16) {
        let mut emitted = false;
        if delta_y != 0 {
            let _ = self.emit(EV_REL, REL_WHEEL_HI_RES, delta_y as i32);
            emitted = true;
        }
        if delta_x != 0 {
            let _ = self.emit(EV_REL, REL_HWHEEL_HI_RES, delta_x as i32);
            emitted = true;
        }
        let (step_x, step_y) = self.wheel_accumulator.push_and_take_steps(delta_x, delta_y);
        if step_y != 0 {
            let _ = self.emit(EV_REL, REL_WHEEL, step_y as i32);
            emitted = true;
        }
        if step_x != 0 {
            let _ = self.emit(EV_REL, REL_HWHEEL, step_x as i32);
            emitted = true;
        }
        if emitted {
            let _ = self.sync();
        }
    }

    fn key(&mut self, key: KeyboardKey, pressed: bool) {
        let Some(code) = linux_key_code(key) else {
            return;
        };
        let _ = self.emit(EV_KEY, code, i32::from(pressed));
        let _ = self.sync();
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<(), String> {
        let event = LinuxInputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let raw = unsafe {
            std::slice::from_raw_parts(
                &event as *const LinuxInputEvent as *const u8,
                std::mem::size_of::<LinuxInputEvent>(),
            )
        };
        self.file
            .write_all(raw)
            .map_err(|e| format!("uinput write: {e}"))
    }

    fn sync(&mut self) -> Result<(), String> {
        self.emit(EV_SYN, SYN_REPORT, 0)?;
        self.file.flush().map_err(|e| format!("uinput flush: {e}"))
    }
}

#[cfg(target_os = "linux")]
impl Drop for UinputMouseController {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            let _ = ui_dev_destroy(self.file.as_raw_fd());
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_button_code(button: u32) -> Option<u16> {
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        8 => Some(BTN_SIDE),
        9 => Some(BTN_EXTRA),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
const SUPPORTED_KEYBOARD_KEYS: [KeyboardKey; 82] = [
    KeyboardKey::Escape,
    KeyboardKey::Tab,
    KeyboardKey::Backspace,
    KeyboardKey::Enter,
    KeyboardKey::Space,
    KeyboardKey::Insert,
    KeyboardKey::Delete,
    KeyboardKey::Home,
    KeyboardKey::End,
    KeyboardKey::PageUp,
    KeyboardKey::PageDown,
    KeyboardKey::ArrowUp,
    KeyboardKey::ArrowDown,
    KeyboardKey::ArrowLeft,
    KeyboardKey::ArrowRight,
    KeyboardKey::Minus,
    KeyboardKey::Equals,
    KeyboardKey::OpenBracket,
    KeyboardKey::CloseBracket,
    KeyboardKey::Backslash,
    KeyboardKey::Semicolon,
    KeyboardKey::Quote,
    KeyboardKey::Backtick,
    KeyboardKey::Comma,
    KeyboardKey::Period,
    KeyboardKey::Slash,
    KeyboardKey::Num0,
    KeyboardKey::Num1,
    KeyboardKey::Num2,
    KeyboardKey::Num3,
    KeyboardKey::Num4,
    KeyboardKey::Num5,
    KeyboardKey::Num6,
    KeyboardKey::Num7,
    KeyboardKey::Num8,
    KeyboardKey::Num9,
    KeyboardKey::A,
    KeyboardKey::B,
    KeyboardKey::C,
    KeyboardKey::D,
    KeyboardKey::E,
    KeyboardKey::F,
    KeyboardKey::G,
    KeyboardKey::H,
    KeyboardKey::I,
    KeyboardKey::J,
    KeyboardKey::K,
    KeyboardKey::L,
    KeyboardKey::M,
    KeyboardKey::N,
    KeyboardKey::O,
    KeyboardKey::P,
    KeyboardKey::Q,
    KeyboardKey::R,
    KeyboardKey::S,
    KeyboardKey::T,
    KeyboardKey::U,
    KeyboardKey::V,
    KeyboardKey::W,
    KeyboardKey::X,
    KeyboardKey::Y,
    KeyboardKey::Z,
    KeyboardKey::F1,
    KeyboardKey::F2,
    KeyboardKey::F3,
    KeyboardKey::F4,
    KeyboardKey::F5,
    KeyboardKey::F6,
    KeyboardKey::F7,
    KeyboardKey::F8,
    KeyboardKey::F9,
    KeyboardKey::F10,
    KeyboardKey::F11,
    KeyboardKey::F12,
    KeyboardKey::LeftShift,
    KeyboardKey::LeftCtrl,
    KeyboardKey::LeftAlt,
    KeyboardKey::LeftMeta,
    KeyboardKey::RightShift,
    KeyboardKey::RightCtrl,
    KeyboardKey::RightAlt,
    KeyboardKey::RightMeta,
];

#[cfg(target_os = "linux")]
fn linux_key_code(key: KeyboardKey) -> Option<u16> {
    Some(match key {
        KeyboardKey::Escape => KEY_ESC,
        KeyboardKey::Tab => KEY_TAB,
        KeyboardKey::Backspace => KEY_BACKSPACE,
        KeyboardKey::Enter => KEY_ENTER,
        KeyboardKey::Space => KEY_SPACE,
        KeyboardKey::Insert => KEY_INSERT,
        KeyboardKey::Delete => KEY_DELETE,
        KeyboardKey::Home => KEY_HOME,
        KeyboardKey::End => KEY_END,
        KeyboardKey::PageUp => KEY_PAGEUP,
        KeyboardKey::PageDown => KEY_PAGEDOWN,
        KeyboardKey::ArrowUp => KEY_UP,
        KeyboardKey::ArrowDown => KEY_DOWN,
        KeyboardKey::ArrowLeft => KEY_LEFT,
        KeyboardKey::ArrowRight => KEY_RIGHT,
        KeyboardKey::Minus => KEY_MINUS,
        KeyboardKey::Equals => KEY_EQUAL,
        KeyboardKey::OpenBracket => KEY_LEFTBRACE,
        KeyboardKey::CloseBracket => KEY_RIGHTBRACE,
        KeyboardKey::Backslash => KEY_BACKSLASH,
        KeyboardKey::Semicolon => KEY_SEMICOLON,
        KeyboardKey::Quote => KEY_APOSTROPHE,
        KeyboardKey::Backtick => KEY_GRAVE,
        KeyboardKey::Comma => KEY_COMMA,
        KeyboardKey::Period => KEY_DOT,
        KeyboardKey::Slash => KEY_SLASH,
        KeyboardKey::Num0 => KEY_0,
        KeyboardKey::Num1 => KEY_1,
        KeyboardKey::Num2 => KEY_2,
        KeyboardKey::Num3 => KEY_3,
        KeyboardKey::Num4 => KEY_4,
        KeyboardKey::Num5 => KEY_5,
        KeyboardKey::Num6 => KEY_6,
        KeyboardKey::Num7 => KEY_7,
        KeyboardKey::Num8 => KEY_8,
        KeyboardKey::Num9 => KEY_9,
        KeyboardKey::A => KEY_A,
        KeyboardKey::B => KEY_B,
        KeyboardKey::C => KEY_C,
        KeyboardKey::D => KEY_D,
        KeyboardKey::E => KEY_E,
        KeyboardKey::F => KEY_F,
        KeyboardKey::G => KEY_G,
        KeyboardKey::H => KEY_H,
        KeyboardKey::I => KEY_I,
        KeyboardKey::J => KEY_J,
        KeyboardKey::K => KEY_K,
        KeyboardKey::L => KEY_L,
        KeyboardKey::M => KEY_M,
        KeyboardKey::N => KEY_N,
        KeyboardKey::O => KEY_O,
        KeyboardKey::P => KEY_P,
        KeyboardKey::Q => KEY_Q,
        KeyboardKey::R => KEY_R,
        KeyboardKey::S => KEY_S,
        KeyboardKey::T => KEY_T,
        KeyboardKey::U => KEY_U,
        KeyboardKey::V => KEY_V,
        KeyboardKey::W => KEY_W,
        KeyboardKey::X => KEY_X,
        KeyboardKey::Y => KEY_Y,
        KeyboardKey::Z => KEY_Z,
        KeyboardKey::F1 => KEY_F1,
        KeyboardKey::F2 => KEY_F2,
        KeyboardKey::F3 => KEY_F3,
        KeyboardKey::F4 => KEY_F4,
        KeyboardKey::F5 => KEY_F5,
        KeyboardKey::F6 => KEY_F6,
        KeyboardKey::F7 => KEY_F7,
        KeyboardKey::F8 => KEY_F8,
        KeyboardKey::F9 => KEY_F9,
        KeyboardKey::F10 => KEY_F10,
        KeyboardKey::F11 => KEY_F11,
        KeyboardKey::F12 => KEY_F12,
        KeyboardKey::LeftShift => KEY_LEFTSHIFT,
        KeyboardKey::LeftCtrl => KEY_LEFTCTRL,
        KeyboardKey::LeftAlt => KEY_LEFTALT,
        KeyboardKey::LeftMeta => KEY_LEFTMETA,
        KeyboardKey::RightShift => KEY_RIGHTSHIFT,
        KeyboardKey::RightCtrl => KEY_RIGHTCTRL,
        KeyboardKey::RightAlt => KEY_RIGHTALT,
        KeyboardKey::RightMeta => KEY_RIGHTMETA,
    })
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
type CGDirectDisplayID = u32;

#[cfg(target_os = "macos")]
extern "C" {
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
    fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;
    fn CGEventCreate(source: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *mut std::ffi::c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *mut std::ffi::c_void;
    fn CGEventCreateKeyboardEvent(
        source: *mut std::ffi::c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut std::ffi::c_void;
    fn CGEventCreateScrollWheelEvent(
        source: *mut std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> *mut std::ffi::c_void;
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(ptr: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
const KCG_HID_EVENT_TAP: u32 = 0;
#[cfg(target_os = "macos")]
const KCG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
#[cfg(target_os = "macos")]
const KCG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
#[cfg(target_os = "macos")]
const KCG_EVENT_LEFT_MOUSE_UP: u32 = 2;
#[cfg(target_os = "macos")]
const KCG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
#[cfg(target_os = "macos")]
const KCG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
#[cfg(target_os = "macos")]
const KCG_EVENT_MOUSE_MOVED: u32 = 5;
#[cfg(target_os = "macos")]
const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
#[cfg(target_os = "macos")]
const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
#[cfg(target_os = "macos")]
const KCG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
#[cfg(target_os = "macos")]
const KCG_EVENT_OTHER_MOUSE_UP: u32 = 26;
#[cfg(target_os = "macos")]
const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

#[cfg(target_os = "macos")]
struct MacosMouseController {
    width: f64,
    height: f64,
    current_pos: CGPoint,
    button_mask: u8,
    wheel_accumulator: WheelAccumulator,
}

#[cfg(target_os = "macos")]
impl MacosMouseController {
    fn new() -> Result<Self, String> {
        let display = unsafe { CGMainDisplayID() };
        let width = unsafe { CGDisplayPixelsWide(display) } as f64;
        let height = unsafe { CGDisplayPixelsHigh(display) } as f64;
        if width <= 0.0 || height <= 0.0 {
            return Err("main display size unavailable".into());
        }

        let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
        let current_pos = if event.is_null() {
            CGPoint {
                x: width * 0.5,
                y: height * 0.5,
            }
        } else {
            let pos = unsafe { CGEventGetLocation(event) };
            unsafe {
                CFRelease(event);
            }
            pos
        };

        Ok(Self {
            width,
            height,
            current_pos,
            button_mask: 0,
            wheel_accumulator: WheelAccumulator::default(),
        })
    }

    fn move_absolute(&mut self, x: u16, y: u16) {
        self.current_pos = CGPoint {
            x: (x as f64 / 65535.0) * (self.width - 1.0).max(0.0),
            y: (y as f64 / 65535.0) * (self.height - 1.0).max(0.0),
        };
        self.post_move_event();
    }

    fn move_relative(&mut self, dx: i16, dy: i16) {
        self.current_pos.x =
            (self.current_pos.x + dx as f64).clamp(0.0, (self.width - 1.0).max(0.0));
        self.current_pos.y =
            (self.current_pos.y + dy as f64).clamp(0.0, (self.height - 1.0).max(0.0));
        self.post_move_event();
    }

    fn button(&mut self, button: u32, pressed: bool) {
        self.update_button_mask(button, pressed);
        let (event_type, mouse_button) = macos_button_event(button, pressed);
        self.post_mouse_event(event_type, mouse_button);
    }

    fn scroll(&mut self, delta_x: i16, delta_y: i16) {
        let (step_x, step_y) = self.wheel_accumulator.push_and_take_steps(delta_x, delta_y);
        if step_x == 0 && step_y == 0 {
            return;
        }
        let event = unsafe {
            CGEventCreateScrollWheelEvent(
                std::ptr::null_mut(),
                KCG_SCROLL_EVENT_UNIT_LINE,
                2,
                step_y as i32,
                step_x as i32,
            )
        };
        if !event.is_null() {
            unsafe {
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn key(&mut self, key: KeyboardKey, pressed: bool) {
        let Some(code) = macos_key_code(key) else {
            return;
        };
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), code, pressed) };
        if !event.is_null() {
            unsafe {
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_move_event(&mut self) {
        let (event_type, mouse_button) = if self.button_mask & MOUSE_BUTTON_PRIMARY != 0 {
            (KCG_EVENT_LEFT_MOUSE_DRAGGED, 0)
        } else if self.button_mask & MOUSE_BUTTON_SECONDARY != 0 {
            (KCG_EVENT_RIGHT_MOUSE_DRAGGED, 1)
        } else if self.button_mask
            & (MOUSE_BUTTON_MIDDLE | MOUSE_BUTTON_EXTRA1 | MOUSE_BUTTON_EXTRA2)
            != 0
        {
            (KCG_EVENT_OTHER_MOUSE_DRAGGED, 2)
        } else {
            (KCG_EVENT_MOUSE_MOVED, 0)
        };
        self.post_mouse_event(event_type, mouse_button);
    }

    fn post_mouse_event(&self, event_type: u32, mouse_button: u32) {
        let event = unsafe {
            CGEventCreateMouseEvent(
                std::ptr::null_mut(),
                event_type,
                self.current_pos,
                mouse_button,
            )
        };
        if !event.is_null() {
            unsafe {
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn update_button_mask(&mut self, button: u32, pressed: bool) {
        let mask = match button {
            1 => MOUSE_BUTTON_PRIMARY,
            2 => MOUSE_BUTTON_MIDDLE,
            3 => MOUSE_BUTTON_SECONDARY,
            8 => MOUSE_BUTTON_EXTRA1,
            9 => MOUSE_BUTTON_EXTRA2,
            _ => 0,
        };
        if mask == 0 {
            return;
        }
        if pressed {
            self.button_mask |= mask;
        } else {
            self.button_mask &= !mask;
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_button_event(button: u32, pressed: bool) -> (u32, u32) {
    match button {
        1 => (
            if pressed {
                KCG_EVENT_LEFT_MOUSE_DOWN
            } else {
                KCG_EVENT_LEFT_MOUSE_UP
            },
            0,
        ),
        3 => (
            if pressed {
                KCG_EVENT_RIGHT_MOUSE_DOWN
            } else {
                KCG_EVENT_RIGHT_MOUSE_UP
            },
            1,
        ),
        2 => (
            if pressed {
                KCG_EVENT_OTHER_MOUSE_DOWN
            } else {
                KCG_EVENT_OTHER_MOUSE_UP
            },
            2,
        ),
        8 => (
            if pressed {
                KCG_EVENT_OTHER_MOUSE_DOWN
            } else {
                KCG_EVENT_OTHER_MOUSE_UP
            },
            3,
        ),
        9 => (
            if pressed {
                KCG_EVENT_OTHER_MOUSE_DOWN
            } else {
                KCG_EVENT_OTHER_MOUSE_UP
            },
            4,
        ),
        _ => (KCG_EVENT_MOUSE_MOVED, 0),
    }
}

#[cfg(target_os = "macos")]
fn macos_key_code(key: KeyboardKey) -> Option<u16> {
    Some(match key {
        KeyboardKey::A => 0,
        KeyboardKey::S => 1,
        KeyboardKey::D => 2,
        KeyboardKey::F => 3,
        KeyboardKey::H => 4,
        KeyboardKey::G => 5,
        KeyboardKey::Z => 6,
        KeyboardKey::X => 7,
        KeyboardKey::C => 8,
        KeyboardKey::V => 9,
        KeyboardKey::B => 11,
        KeyboardKey::Q => 12,
        KeyboardKey::W => 13,
        KeyboardKey::E => 14,
        KeyboardKey::R => 15,
        KeyboardKey::Y => 16,
        KeyboardKey::T => 17,
        KeyboardKey::Num1 => 18,
        KeyboardKey::Num2 => 19,
        KeyboardKey::Num3 => 20,
        KeyboardKey::Num4 => 21,
        KeyboardKey::Num6 => 22,
        KeyboardKey::Num5 => 23,
        KeyboardKey::Equals => 24,
        KeyboardKey::Num9 => 25,
        KeyboardKey::Num7 => 26,
        KeyboardKey::Minus => 27,
        KeyboardKey::Num8 => 28,
        KeyboardKey::Num0 => 29,
        KeyboardKey::CloseBracket => 30,
        KeyboardKey::O => 31,
        KeyboardKey::U => 32,
        KeyboardKey::OpenBracket => 33,
        KeyboardKey::I => 34,
        KeyboardKey::P => 35,
        KeyboardKey::Enter => 36,
        KeyboardKey::L => 37,
        KeyboardKey::J => 38,
        KeyboardKey::Quote => 39,
        KeyboardKey::K => 40,
        KeyboardKey::Semicolon => 41,
        KeyboardKey::Backslash => 42,
        KeyboardKey::Comma => 43,
        KeyboardKey::Slash => 44,
        KeyboardKey::N => 45,
        KeyboardKey::M => 46,
        KeyboardKey::Period => 47,
        KeyboardKey::Tab => 48,
        KeyboardKey::Space => 49,
        KeyboardKey::Backtick => 50,
        KeyboardKey::Backspace => 51,
        KeyboardKey::Escape => 53,
        KeyboardKey::LeftMeta => 55,
        KeyboardKey::LeftShift => 56,
        KeyboardKey::LeftAlt => 58,
        KeyboardKey::LeftCtrl => 59,
        KeyboardKey::RightShift => 60,
        KeyboardKey::RightAlt => 61,
        KeyboardKey::RightCtrl => 62,
        KeyboardKey::RightMeta => 54,
        KeyboardKey::F5 => 96,
        KeyboardKey::F6 => 97,
        KeyboardKey::F7 => 98,
        KeyboardKey::F3 => 99,
        KeyboardKey::F8 => 100,
        KeyboardKey::F9 => 101,
        KeyboardKey::F11 => 103,
        KeyboardKey::F10 => 109,
        KeyboardKey::F12 => 111,
        KeyboardKey::Insert => 114,
        KeyboardKey::Home => 115,
        KeyboardKey::PageUp => 116,
        KeyboardKey::Delete => 117,
        KeyboardKey::F4 => 118,
        KeyboardKey::End => 119,
        KeyboardKey::F2 => 120,
        KeyboardKey::PageDown => 121,
        KeyboardKey::F1 => 122,
        KeyboardKey::ArrowLeft => 123,
        KeyboardKey::ArrowRight => 124,
        KeyboardKey::ArrowDown => 125,
        KeyboardKey::ArrowUp => 126,
    })
}

#[cfg(target_os = "linux")]
#[allow(non_snake_case, non_upper_case_globals, non_camel_case_types)]
#[allow(dead_code)]
mod x11_ffi {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_ulong};

    pub type Display = c_void;
    pub type Window = c_ulong;
    pub type Bool = c_int;
    pub type KeySym = c_ulong;

    extern "C" {
        pub fn XOpenDisplay(display_name: *const c_char) -> *mut Display;
        pub fn XCloseDisplay(display: *mut Display) -> c_int;
        pub fn XDefaultScreen(display: *mut Display) -> c_int;
        pub fn XRootWindow(display: *mut Display, screen_number: c_int) -> Window;
        pub fn XDisplayWidth(display: *mut Display, screen_number: c_int) -> c_int;
        pub fn XDisplayHeight(display: *mut Display, screen_number: c_int) -> c_int;
        pub fn XKeysymToKeycode(display: *mut Display, keysym: KeySym) -> c_uchar;
        pub fn XStringToKeysym(string: *const c_char) -> KeySym;
        pub fn XSync(display: *mut Display, discard: Bool) -> c_int;
        pub fn XQueryPointer(
            display: *mut Display,
            w: Window,
            root_return: *mut Window,
            child_return: *mut Window,
            root_x_return: *mut c_int,
            root_y_return: *mut c_int,
            win_x_return: *mut c_int,
            win_y_return: *mut c_int,
            mask_return: *mut c_uint,
        ) -> Bool;
    }

    extern "C" {
        pub fn XTestQueryExtension(
            display: *mut Display,
            event_base_return: *mut c_int,
            error_base_return: *mut c_int,
            major_return: *mut c_int,
            minor_return: *mut c_int,
        ) -> Bool;
        pub fn XTestFakeMotionEvent(
            display: *mut Display,
            screen_number: c_int,
            x: c_int,
            y: c_int,
            delay: c_ulong,
        ) -> c_int;
        pub fn XTestFakeRelativeMotionEvent(
            display: *mut Display,
            x: c_int,
            y: c_int,
            delay: c_ulong,
        ) -> c_int;
        pub fn XTestFakeButtonEvent(
            display: *mut Display,
            button: c_uint,
            is_press: Bool,
            delay: c_ulong,
        ) -> c_int;
        pub fn XTestFakeKeyEvent(
            display: *mut Display,
            keycode: c_uint,
            is_press: Bool,
            delay: c_ulong,
        ) -> c_int;
    }
}

#[cfg(target_os = "linux")]
struct X11InputController {
    display: *mut x11_ffi::Display,
    _screen: i32,
    _root: x11_ffi::Window,
    width: i32,
    height: i32,
    tracked_x: i32,
    tracked_y: i32,
    wheel_accumulator: WheelAccumulator,
}

#[cfg(target_os = "linux")]
unsafe impl Send for X11InputController {}

#[cfg(target_os = "linux")]
impl X11InputController {
    fn new() -> Result<Self, String> {
        let display = unsafe { x11_ffi::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            return Err("cannot open X11 display".into());
        }
        let screen = unsafe { x11_ffi::XDefaultScreen(display) };
        let mut event_base = 0;
        let mut error_base = 0;
        let mut major = 0;
        let mut minor = 0;
        let has_xtest = unsafe {
            x11_ffi::XTestQueryExtension(
                display,
                &mut event_base,
                &mut error_base,
                &mut major,
                &mut minor,
            ) != 0
        };
        if !has_xtest {
            unsafe {
                x11_ffi::XCloseDisplay(display);
            }
            return Err("XTest extension not available".into());
        }
        let root = unsafe { x11_ffi::XRootWindow(display, screen) };
        let width = unsafe { x11_ffi::XDisplayWidth(display, screen) };
        let height = unsafe { x11_ffi::XDisplayHeight(display, screen) };
        let (tracked_x, tracked_y) = unsafe {
            let mut root_ret: x11_ffi::Window = 0;
            let mut child_ret: x11_ffi::Window = 0;
            let mut root_x: std::os::raw::c_int = 0;
            let mut root_y: std::os::raw::c_int = 0;
            let mut win_x: std::os::raw::c_int = 0;
            let mut win_y: std::os::raw::c_int = 0;
            let mut mask: std::os::raw::c_uint = 0;
            x11_ffi::XQueryPointer(
                display, root,
                &mut root_ret, &mut child_ret,
                &mut root_x, &mut root_y,
                &mut win_x, &mut win_y,
                &mut mask,
            );
            (root_x, root_y)
        };
        Ok(Self {
            display,
            _screen: screen,
            _root: root,
            width,
            height,
            tracked_x,
            tracked_y,
            wheel_accumulator: WheelAccumulator::default(),
        })
    }

    fn move_absolute(&mut self, x: u16, y: u16) {
        let width = self.width.max(1) as i64;
        let height = self.height.max(1) as i64;
        let target_x = ((x as i64 * (width - 1).max(0) + 32767) / 65535) as i32;
        let target_y = ((y as i64 * (height - 1).max(0) + 32767) / 65535) as i32;
        let dx = target_x - self.tracked_x;
        let dy = target_y - self.tracked_y;
        self.tracked_x = target_x;
        self.tracked_y = target_y;
        if dx != 0 || dy != 0 {
            unsafe {
                x11_ffi::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0);
                x11_ffi::XSync(self.display, 0);
            }
        }
    }

    fn move_relative(&mut self, dx: i16, dy: i16) {
        self.tracked_x = (self.tracked_x + dx as i32).clamp(0, (self.width - 1).max(0));
        self.tracked_y = (self.tracked_y + dy as i32).clamp(0, (self.height - 1).max(0));
        unsafe {
            x11_ffi::XTestFakeRelativeMotionEvent(self.display, dx as i32, dy as i32, 0);
            x11_ffi::XSync(self.display, 0);
        }
    }

    fn button(&mut self, button: u32, pressed: bool) {
        unsafe {
            x11_ffi::XTestFakeButtonEvent(self.display, button, i32::from(pressed), 0);
            x11_ffi::XSync(self.display, 0);
        }
    }

    fn scroll(&mut self, delta_x: i16, delta_y: i16) {
        let (step_x, step_y) = self.wheel_accumulator.push_and_take_steps(delta_x, delta_y);
        self.scroll_axis(step_y, 4, 5);
        self.scroll_axis(step_x, 6, 7);
    }

    fn key(&mut self, key: KeyboardKey, pressed: bool) {
        let Some(key_name) = x11_key_name(key) else {
            return;
        };
        let keysym = unsafe { x11_ffi::XStringToKeysym(key_name.as_ptr()) };
        if keysym == 0 {
            return;
        }
        let keycode = unsafe { x11_ffi::XKeysymToKeycode(self.display, keysym) };
        if keycode == 0 {
            return;
        }
        unsafe {
            x11_ffi::XTestFakeKeyEvent(self.display, keycode as u32, i32::from(pressed), 0);
            x11_ffi::XSync(self.display, 0);
        }
    }

    fn scroll_axis(&mut self, delta: i16, positive_button: u32, negative_button: u32) {
        let clicks = delta.saturating_abs().min(32) as usize;
        if clicks == 0 {
            return;
        }
        let button = if delta > 0 {
            positive_button
        } else {
            negative_button
        };
        for _ in 0..clicks {
            self.button(button, true);
            self.button(button, false);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for X11InputController {
    fn drop(&mut self) {
        unsafe {
            x11_ffi::XCloseDisplay(self.display);
        }
    }
}

#[cfg(target_os = "linux")]
fn x11_key_name(key: KeyboardKey) -> Option<&'static std::ffi::CStr> {
    Some(match key {
        KeyboardKey::Escape => c"Escape",
        KeyboardKey::Tab => c"Tab",
        KeyboardKey::Backspace => c"BackSpace",
        KeyboardKey::Enter => c"Return",
        KeyboardKey::Space => c"space",
        KeyboardKey::Insert => c"Insert",
        KeyboardKey::Delete => c"Delete",
        KeyboardKey::Home => c"Home",
        KeyboardKey::End => c"End",
        KeyboardKey::PageUp => c"Page_Up",
        KeyboardKey::PageDown => c"Page_Down",
        KeyboardKey::ArrowUp => c"Up",
        KeyboardKey::ArrowDown => c"Down",
        KeyboardKey::ArrowLeft => c"Left",
        KeyboardKey::ArrowRight => c"Right",
        KeyboardKey::Minus => c"minus",
        KeyboardKey::Equals => c"equal",
        KeyboardKey::OpenBracket => c"bracketleft",
        KeyboardKey::CloseBracket => c"bracketright",
        KeyboardKey::Backslash => c"backslash",
        KeyboardKey::Semicolon => c"semicolon",
        KeyboardKey::Quote => c"apostrophe",
        KeyboardKey::Backtick => c"grave",
        KeyboardKey::Comma => c"comma",
        KeyboardKey::Period => c"period",
        KeyboardKey::Slash => c"slash",
        KeyboardKey::Num0 => c"0",
        KeyboardKey::Num1 => c"1",
        KeyboardKey::Num2 => c"2",
        KeyboardKey::Num3 => c"3",
        KeyboardKey::Num4 => c"4",
        KeyboardKey::Num5 => c"5",
        KeyboardKey::Num6 => c"6",
        KeyboardKey::Num7 => c"7",
        KeyboardKey::Num8 => c"8",
        KeyboardKey::Num9 => c"9",
        KeyboardKey::A => c"a",
        KeyboardKey::B => c"b",
        KeyboardKey::C => c"c",
        KeyboardKey::D => c"d",
        KeyboardKey::E => c"e",
        KeyboardKey::F => c"f",
        KeyboardKey::G => c"g",
        KeyboardKey::H => c"h",
        KeyboardKey::I => c"i",
        KeyboardKey::J => c"j",
        KeyboardKey::K => c"k",
        KeyboardKey::L => c"l",
        KeyboardKey::M => c"m",
        KeyboardKey::N => c"n",
        KeyboardKey::O => c"o",
        KeyboardKey::P => c"p",
        KeyboardKey::Q => c"q",
        KeyboardKey::R => c"r",
        KeyboardKey::S => c"s",
        KeyboardKey::T => c"t",
        KeyboardKey::U => c"u",
        KeyboardKey::V => c"v",
        KeyboardKey::W => c"w",
        KeyboardKey::X => c"x",
        KeyboardKey::Y => c"y",
        KeyboardKey::Z => c"z",
        KeyboardKey::F1 => c"F1",
        KeyboardKey::F2 => c"F2",
        KeyboardKey::F3 => c"F3",
        KeyboardKey::F4 => c"F4",
        KeyboardKey::F5 => c"F5",
        KeyboardKey::F6 => c"F6",
        KeyboardKey::F7 => c"F7",
        KeyboardKey::F8 => c"F8",
        KeyboardKey::F9 => c"F9",
        KeyboardKey::F10 => c"F10",
        KeyboardKey::F11 => c"F11",
        KeyboardKey::F12 => c"F12",
        KeyboardKey::LeftShift => c"Shift_L",
        KeyboardKey::LeftCtrl => c"Control_L",
        KeyboardKey::LeftAlt => c"Alt_L",
        KeyboardKey::LeftMeta => c"Super_L",
        KeyboardKey::RightShift => c"Shift_R",
        KeyboardKey::RightCtrl => c"Control_R",
        KeyboardKey::RightAlt => c"Alt_R",
        KeyboardKey::RightMeta => c"Super_R",
    })
}

#[cfg(test)]
mod tests {
    use super::input_seq_is_newer;

    #[test]
    fn input_seq_rejects_duplicates_and_older_packets() {
        assert!(!input_seq_is_newer(10, 10));
        assert!(!input_seq_is_newer(9, 10));
        assert!(!input_seq_is_newer(0, 0x8000));
    }

    #[test]
    fn input_seq_accepts_forward_progress_and_wraparound() {
        assert!(input_seq_is_newer(11, 10));
        assert!(input_seq_is_newer(0, u16::MAX));
        assert!(input_seq_is_newer(2, u16::MAX));
    }
}
