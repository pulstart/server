#[cfg(target_os = "linux")]
use crate::capture::linux::{active_remote_desktop_session, RemoteDesktopPortalSession};
use rand::{rngs::OsRng, RngCore};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use st_protocol::MOUSE_WHEEL_STEP_UNITS;
use st_protocol::{
    ControlMessage, ControllerState, CursorShape, CursorState, InputCapabilities, InputCredential,
    InputPacket, KeyboardKey, KEYBOARD_STATE_BYTES, MAX_TEXT_INPUT_BYTES, MOUSE_BUTTON_EXTRA1,
    MOUSE_BUTTON_EXTRA2, MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_PRIMARY, MOUSE_BUTTON_SECONDARY,
};
use std::collections::{BTreeMap, HashMap};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::POINT;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

const MAX_CURSOR_SHAPE_RGBA_BYTES: usize = u16::MAX as usize - 16;
const TEXT_INPUT_RATE_WINDOW: Duration = Duration::from_secs(1);
const TEXT_INPUT_RATE_BYTES: usize = 16 * 1024;
const TEXT_INPUT_RATE_MESSAGES: u32 = 32;

pub fn generate_input_credential() -> InputCredential {
    let mut bytes = [0u8; st_protocol::INPUT_CREDENTIAL_BYTES];
    OsRng.fill_bytes(&mut bytes);
    InputCredential::from_bytes(bytes)
}

// ---- Warp / mouselook-without-hide detector (ST_WARP_DETECT) ----------------
// Cursor observations accumulated before a window is judged.
const WARP_WINDOW_SAMPLES: u32 = 6;
// Minimum commanded net travel (stream px, manhattan) for a window to count —
// below this the user is essentially idle and the verdict is held, not changed.
const WARP_MIN_COMMANDED: i64 = 120;
// Captured cursor must move at least commanded * NUM/DEN to count as "tracking".
// Below that the app is eating the motion (warp-to-centre or raw-input park).
const WARP_CONVERGE_NUM: i64 = 1;
const WARP_CONVERGE_DEN: i64 = 4;
// Consecutive diverging windows to enter app_grab, converging windows to leave.
const WARP_ENTER_STREAK: u32 = 3;
const WARP_EXIT_STREAK: u32 = 2;

/// Detects a remote app warping/parking the pointer for mouselook *without
/// hiding the cursor* (many XWayland/Proton FPS titles): the client keeps
/// commanding the cursor across the screen, but the real cursor — read back from
/// the capture backend's cursor plane / metadata — does not follow (it snaps
/// back to centre, or never moves). Both signals live on the server, so the
/// comparison is local; the verdict (`app_grab`) ships to the client as a
/// relative-capture trigger independent of cursor visibility.
///
/// **Opt-in** via `ST_WARP_DETECT=1` (`true`/`yes`/`on`); off by default until
/// live-validated. The divergence metric cannot distinguish a real warp from a
/// backend that simply reports a static/unavailable cursor position (e.g. KMS
/// drivers with no CRTC_X/Y) — both look like "captured doesn't follow
/// commanded" — so an unguarded detector hides the *desktop* cursor on such a
/// backend. Guard: `position_trusted` requires first observing the captured
/// cursor actually track commanded motion (a converging window) before any
/// `app_grab=true` is allowed. A backend that never tracks is never trusted and
/// never grabs. Recoverable regardless via the client's force-release.
struct WarpDetector {
    enabled: bool,
    /// Set once the captured cursor has been seen to follow commanded motion (a
    /// converging window). Until then no warp verdict is allowed — this is what
    /// stops a position-unreliable backend from hiding the desktop cursor.
    position_trusted: bool,
    /// Virtual position we have commanded the cursor to (stream coords): set
    /// absolutely by MouseAbsolute, accumulated by MouseRelative. Anchored to the
    /// first observed real cursor position.
    commanded: Option<(i64, i64)>,
    window_commanded_start: Option<(i64, i64)>,
    window_captured_start: Option<(i64, i64)>,
    window_samples: u32,
    diverge_streak: u32,
    converge_streak: u32,
    app_grab: bool,
}

impl WarpDetector {
    fn new() -> Self {
        Self {
            enabled: warp_detect_enabled(),
            position_trusted: false,
            commanded: None,
            window_commanded_start: None,
            window_captured_start: None,
            window_samples: 0,
            diverge_streak: 0,
            converge_streak: 0,
            app_grab: false,
        }
    }

    /// Drop all accumulated state (controller change, backend refresh, cursor
    /// reset). Keeps `enabled`; clears the verdict and the position-trust latch.
    fn reset(&mut self) {
        self.position_trusted = false;
        self.commanded = None;
        self.window_commanded_start = None;
        self.window_captured_start = None;
        self.window_samples = 0;
        self.diverge_streak = 0;
        self.converge_streak = 0;
        self.app_grab = false;
    }

    fn observe_command_absolute(&mut self, sx: i64, sy: i64) {
        if !self.enabled {
            return;
        }
        self.commanded = Some((sx, sy));
    }

    fn observe_command_relative(&mut self, dx: i64, dy: i64) {
        if !self.enabled {
            return;
        }
        if let Some((cx, cy)) = self.commanded {
            self.commanded = Some((cx + dx, cy + dy));
        }
    }

    /// Feed one capture-reported cursor position (stream coords). Returns the
    /// (possibly updated) verdict.
    fn observe_cursor(&mut self, x: i32, y: i32) -> bool {
        if !self.enabled {
            return false;
        }
        let captured = (i64::from(x), i64::from(y));
        // Anchor commanded to the real cursor on the first observation so a
        // window opens from a consistent origin.
        let commanded = *self.commanded.get_or_insert(captured);
        if self.window_captured_start.is_none() {
            self.window_captured_start = Some(captured);
            self.window_commanded_start = Some(commanded);
            self.window_samples = 0;
        }
        self.window_samples += 1;
        if self.window_samples < WARP_WINDOW_SAMPLES {
            return self.app_grab;
        }

        let (cmd0x, cmd0y) = self.window_commanded_start.unwrap_or(commanded);
        let (cap0x, cap0y) = self.window_captured_start.unwrap_or(captured);
        let cmd_net = (commanded.0 - cmd0x).abs() + (commanded.1 - cmd0y).abs();
        let cap_net = (captured.0 - cap0x).abs() + (captured.1 - cap0y).abs();

        if cmd_net >= WARP_MIN_COMMANDED {
            // Enough commanded motion to judge this window.
            if cap_net * WARP_CONVERGE_DEN < cmd_net * WARP_CONVERGE_NUM {
                self.diverge_streak += 1;
                self.converge_streak = 0;
                // Only grab once the position source has proven itself by
                // tracking at least once. A backend that reports a static or
                // unavailable cursor position diverges forever but is never
                // trusted, so it can never hide the desktop cursor.
                if self.position_trusted && self.diverge_streak >= WARP_ENTER_STREAK {
                    self.app_grab = true;
                }
            } else {
                // Captured tracked commanded: the position source is live.
                self.position_trusted = true;
                self.converge_streak += 1;
                self.diverge_streak = 0;
                if self.converge_streak >= WARP_EXIT_STREAK {
                    self.app_grab = false;
                }
            }
        }
        // Idle window (cmd_net below threshold): hold the current verdict and
        // touch no streaks, so an afk player stays captured and a stray
        // false-positive only releases once real motion is seen to track.

        // Open the next window from the current positions.
        self.window_captured_start = Some(captured);
        self.window_commanded_start = Some(commanded);
        self.window_samples = 0;
        self.app_grab
    }
}

/// `ST_WARP_DETECT`: **opt-in** (off by default until live-validated). Enabled by
/// `1`/`true`/`yes`/`on`; anything else (incl. unset) keeps it off.
fn warp_detect_enabled() -> bool {
    match std::env::var("ST_WARP_DETECT") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

static TRACE_CURSOR_UPDATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRACE_CURSOR_SEND_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "linux")]
static PORTAL_ERROR_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Log Portal D-Bus errors sparingly (first 3, then every 100th).
#[cfg(target_os = "linux")]
fn log_portal_error(method: &str, err: impl std::fmt::Display) {
    let n = PORTAL_ERROR_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 3 || n.is_multiple_of(100) {
        eprintln!("[input] Portal {method} failed (count={n}): {err}");
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use crate::capture::CapturedCursor;

pub struct InputRuntime {
    next_client_id: AtomicU32,
    active_controller_id: AtomicU32,
    /// Session-sourced "game mode" hint: the in-session tray agent detected a
    /// fullscreen game-class window focused (compositor query — root can't see
    /// the session) and pushed it over the control socket. ORed into
    /// `CursorState.app_grab` so the client enters relative capture. Works where
    /// the warp detector can't (e.g. NVIDIA, no cursor-position readback).
    game_mode: AtomicBool,
    inner: Mutex<InputRuntimeInner>,
    active_clients: Mutex<HashMap<u32, ActiveInputClient>>,
}

enum ActiveInputClient {
    Direct {
        credential: InputCredential,
        source_ip: std::net::IpAddr,
        media_dest: Arc<Mutex<SocketAddr>>,
        text_rate: TextInputRateLimiter,
    },
    Tunnel {
        credential: InputCredential,
        text_rate: TextInputRateLimiter,
    },
}

struct TextInputRateLimiter {
    window_started: Instant,
    bytes: usize,
    messages: u32,
}

impl Default for TextInputRateLimiter {
    fn default() -> Self {
        Self {
            window_started: Instant::now(),
            bytes: 0,
            messages: 0,
        }
    }
}

impl TextInputRateLimiter {
    fn allow(&mut self, bytes: usize, now: Instant) -> bool {
        if now.duration_since(self.window_started) >= TEXT_INPUT_RATE_WINDOW {
            self.window_started = now;
            self.bytes = 0;
            self.messages = 0;
        }
        if self.messages >= TEXT_INPUT_RATE_MESSAGES
            || self.bytes.saturating_add(bytes) > TEXT_INPUT_RATE_BYTES
        {
            return false;
        }
        self.messages += 1;
        self.bytes += bytes;
        true
    }
}

pub struct RegisteredInputClient {
    runtime: Arc<InputRuntime>,
    client_id: u32,
}

impl Drop for RegisteredInputClient {
    fn drop(&mut self) {
        self.runtime.unregister_client(self.client_id);
    }
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
    shape.hotspot_x = (((shape.hotspot_x as u32) * dst_w + src_width as u32 / 2) / src_width as u32)
        .min(dst_w.saturating_sub(1)) as u16;
    shape.hotspot_y = (((shape.hotspot_y as u32) * dst_h + src_height as u32 / 2)
        / src_height as u32)
        .min(dst_h.saturating_sub(1)) as u16;

    (shape, true)
}

/// Crop a BGRA (premultiplied) cursor image to its opaque bounding box.
///
/// Hardware cursor planes are frequently a fixed over-allocation — NVIDIA's KMS
/// cursor plane is a 256×256 buffer with the actual ~32px cursor drawn in one
/// corner and the rest fully transparent. Sending the whole buffer blows the
/// control payload budget, forcing `fit_cursor_shape_to_payload_budget` to
/// downscale the *entire* image, which shrinks the visible cursor on the client.
/// Trimming to the opaque content sends the real cursor at native size.
///
/// Returns `(pixels, width, height, offset_x, offset_y)` (offset = top-left of
/// the opaque region within the source), or `None` if nothing needs cropping
/// (already tight, or fully transparent — caller keeps the original).
fn crop_cursor_to_opaque(
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || pixels.len() < w * h * 4 {
        return None;
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            // BGRA premultiplied: alpha is the 4th byte.
            if pixels[(y * w + x) * 4 + 3] != 0 {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    let cw = max_x - min_x + 1;
    let ch = max_y - min_y + 1;
    if cw == w && ch == h {
        return None; // already tight
    }
    let mut out = Vec::with_capacity(cw * ch * 4);
    for y in min_y..=max_y {
        let row = (y * w + min_x) * 4;
        out.extend_from_slice(&pixels[row..row + cw * 4]);
    }
    Some((out, cw as u32, ch as u32, min_x as u32, min_y as u32))
}

/// Estimate a cursor's hotspot when the capture backend can't report one (KMS:
/// the legacy NVIDIA cursor plane exposes no hotspot). `pixels` is BGRA
/// premultiplied, `w`×`h` the cropped (tight) cursor.
///
/// Heuristic by horizontal symmetry of the opaque mask: symmetric cursors (text
/// I-beam, crosshair, move/resize) get a center hotspot; asymmetric ones (arrow,
/// hand) get a top-left hotspot, which after the opaque-bbox crop is the visual
/// tip. Approximate, but nails the two dominant desktop cases — an arrow whose
/// click point is the tip, and an I-beam whose click point is its center (the
/// off-by-half-a-line text-selection bug from assuming (0,0) for everything).
fn estimate_cursor_hotspot(pixels: &[u8], w: u32, h: u32) -> (u32, u32) {
    let wu = w as usize;
    let hu = h as usize;
    if wu < 2 || hu == 0 || pixels.len() < wu * hu * 4 {
        return (0, 0);
    }
    let mut matched = 0usize;
    let mut total = 0usize;
    for y in 0..hu {
        for x in 0..wu / 2 {
            let left = pixels[(y * wu + x) * 4 + 3] != 0;
            let right = pixels[(y * wu + (wu - 1 - x)) * 4 + 3] != 0;
            total += 1;
            if left == right {
                matched += 1;
            }
        }
    }
    let symmetric = total > 0 && matched * 100 >= total * 85;
    if symmetric {
        (w / 2, h / 2)
    } else {
        (0, 0)
    }
}

struct InputRuntimeInner {
    backend: InputBackend,
    backend_label: String,
    capabilities: InputCapabilities,
    controller_id: Option<u32>,
    last_input_seq_by_client: BTreeMap<u32, u16>,
    button_mask: u8,
    keyboard_state: [u8; KEYBOARD_STATE_BYTES],
    cursor_shape: Option<CursorShape>,
    cursor_state: CursorState,
    cursor_shape_version: u64,
    cursor_state_version: u64,
    stream_width: u32,
    stream_height: u32,
    warp: WarpDetector,
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

#[derive(Default)]
pub struct CursorVersionCursor {
    pub shape: u64,
    pub state: u64,
}

impl InputRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_client_id: AtomicU32::new(1),
            active_controller_id: AtomicU32::new(0),
            game_mode: AtomicBool::new(false),
            inner: Mutex::new(InputRuntimeInner {
                backend: InputBackend::Unavailable,
                backend_label: "unavailable".to_string(),
                capabilities: InputCapabilities::default(),
                controller_id: None,
                last_input_seq_by_client: BTreeMap::new(),
                button_mask: 0,
                keyboard_state: [0u8; KEYBOARD_STATE_BYTES],
                cursor_shape: None,
                cursor_state: CursorState::default(),
                cursor_shape_version: 0,
                cursor_state_version: 0,
                stream_width: 0,
                stream_height: 0,
                warp: WarpDetector::new(),
            }),
            active_clients: Mutex::new(HashMap::new()),
        })
    }

    /// Register an authenticated direct client before its startup bundle is sent.
    /// Input may update the UDP port, but it must come from the TCP peer's IP.
    pub fn register_direct_client(
        self: &Arc<Self>,
        client_id: u32,
        credential: InputCredential,
        source_ip: std::net::IpAddr,
        media_dest: Arc<Mutex<SocketAddr>>,
    ) -> RegisteredInputClient {
        self.active_clients.lock().unwrap().insert(
            client_id,
            ActiveInputClient::Direct {
                credential,
                source_ip,
                media_dest,
                text_rate: TextInputRateLimiter::default(),
            },
        );
        RegisteredInputClient {
            runtime: Arc::clone(self),
            client_id,
        }
    }

    pub fn register_tunnel_client(
        self: &Arc<Self>,
        client_id: u32,
        credential: InputCredential,
    ) -> RegisteredInputClient {
        self.active_clients.lock().unwrap().insert(
            client_id,
            ActiveInputClient::Tunnel {
                credential,
                text_rate: TextInputRateLimiter::default(),
            },
        );
        RegisteredInputClient {
            runtime: Arc::clone(self),
            client_id,
        }
    }

    fn unregister_client(&self, client_id: u32) {
        // Keep the lock order identical to packet handling so a packet cannot
        // recreate sequence state after its registration has been removed.
        let mut active_clients = self.active_clients.lock().unwrap();
        active_clients.remove(&client_id);
        let mut inner = self.inner.lock().unwrap();
        inner.last_input_seq_by_client.remove(&client_id);
        if inner.controller_id == Some(client_id) {
            inner.release_all_inputs();
            inner.controller_id = None;
            inner.warp.reset();
        }
        self.set_active_controller(inner.controller_id);
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

    pub fn handle_text_input(&self, client_id: u32, text: &str) -> bool {
        if text.is_empty() || text.len() > MAX_TEXT_INPUT_BYTES || text.contains('\0') {
            return false;
        }

        let mut active_clients = self.active_clients.lock().unwrap();
        let text_rate = match active_clients.get_mut(&client_id) {
            Some(ActiveInputClient::Direct { text_rate, .. })
            | Some(ActiveInputClient::Tunnel { text_rate, .. }) => text_rate,
            None => return false,
        };
        if !text_rate.allow(text.len(), Instant::now()) {
            return false;
        }

        let mut inner = self.inner.lock().unwrap();
        drop(active_clients);
        if !inner.capabilities.keyboard
            || !inner.capabilities.text_input
            || matches!(inner.backend, InputBackend::Unavailable)
        {
            return false;
        }
        if inner.controller_id.is_some_and(|owner| owner != client_id) {
            return false;
        }
        inner.activate_controller(client_id);
        self.set_active_controller(Some(client_id));
        inner.inject_text(text)
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
        self.set_active_controller(Some(client_id));
        ControllerState::OwnedByYou
    }

    pub fn release_control(&self, client_id: u32) -> ControllerState {
        let mut inner = self.inner.lock().unwrap();
        if inner.controller_id == Some(client_id) {
            inner.release_all_inputs();
            inner.controller_id = None;
        }
        self.set_active_controller(inner.controller_id);
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
        self.active_controller_id.load(Ordering::Relaxed) != 0
    }

    pub fn refresh_backend(&self, capture_backend: &str, _stream_width: u32, _stream_height: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.release_all_inputs();
        inner.controller_id = None;
        inner.last_input_seq_by_client.clear();
        inner.button_mask = 0;
        inner.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
        inner.cursor_shape = None;
        inner.cursor_state = CursorState::default();
        inner.warp.reset();
        inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
        inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
        inner.stream_width = _stream_width;
        inner.stream_height = _stream_height;
        self.set_active_controller(None);

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
                        cursor_position_reliable: true,
                        text_input: true,
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
                        cursor_position_reliable: true,
                        text_input: true,
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
        inner.last_input_seq_by_client.clear();
        inner.button_mask = 0;
        inner.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
        inner.cursor_shape = None;
        inner.cursor_state = CursorState::default();
        inner.warp.reset();
        inner.cursor_shape_version = inner.cursor_shape_version.wrapping_add(1);
        inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
        inner.stream_width = 0;
        inner.stream_height = 0;
        self.set_active_controller(None);
    }

    /// Set the session-sourced game-mode hint (pushed by the in-session tray
    /// agent when a focused window is a game — fullscreen, or a known game
    /// class even when windowed).
    ///
    /// game_mode does NOT itself force relative capture. It *gates* how
    /// `update_cursor` reads a hidden HW cursor: on the desktop a hidden cursor
    /// is preserved (NVIDIA's legacy plane reports "no framebuffer" for an idle
    /// pointer, indistinguishable from truly gone), but inside a game a hidden
    /// cursor means the game grabbed the pointer for mouselook, so we publish
    /// `visible=false` and the client enters relative. When the same game shows
    /// its cursor again (an in-game menu / inventory), `update_cursor` publishes
    /// `visible=true` and the pointer comes back — the per-frame cursor sample
    /// is the real signal, fullscreen is only the gate.
    pub fn set_game_mode(&self, on: bool) {
        if self.game_mode.swap(on, Ordering::SeqCst) == on {
            return;
        }
        eprintln!("[input] session game_mode={on}");
        if on {
            // Entry needs no forced state: the next captured frame's
            // `update_cursor` republishes the correct visible/app_grab within
            // one frame (it runs every frame regardless of cursor presence).
            return;
        }
        // Exit must act. The last KMS sample may have been a hidden (None)
        // cursor we published as `visible=false`; on the desktop a None sample
        // is preserved, so without an explicit restore the pointer would stay
        // hidden/relative after the user alt-tabs out of the game.
        let mut inner = self.inner.lock().unwrap();
        inner.warp.reset();
        if !inner.cursor_state.visible || inner.cursor_state.app_grab {
            inner.cursor_state.visible = true;
            inner.cursor_state.app_grab = false;
            inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    pub fn update_cursor(&self, cursor: Option<&CapturedCursor>) {
        let Ok(mut inner) = self.inner.try_lock() else {
            // Input injection can block on portal/X11/uinput calls. Dropping one
            // cursor metadata update is preferable to stalling capture/encode.
            return;
        };
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
            // No HW cursor framebuffer this frame. On the desktop we can't tell
            // an idle/parked pointer (NVIDIA's legacy cursor plane reports no
            // framebuffer when idle) from a truly hidden one, so we preserve the
            // last state to avoid flickering the pointer off. Inside a game
            // (session tray reported a focused game window), a hidden cursor
            // means the game grabbed the pointer for mouselook → publish a
            // definite hidden state so the client enters relative capture.
            if self.game_mode.load(Ordering::Relaxed) {
                inner.warp.reset();
                let next_state = CursorState {
                    serial: inner.cursor_state.serial,
                    x: inner.cursor_state.x,
                    y: inner.cursor_state.y,
                    visible: false,
                    app_grab: false,
                };
                if inner.cursor_state != next_state {
                    inner.cursor_state = next_state;
                    inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
                    eprintln!("[input] game_mode hidden cursor → visible=false (mouselook)");
                }
            }
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
            // Cursor hidden: the visibility-based relative path takes over, so
            // clear any warp verdict and reset the detector window for a clean
            // start when the cursor reappears.
            inner.warp.reset();
            let next_state = CursorState {
                serial,
                x: cursor.x,
                y: cursor.y,
                visible: cursor.visible,
                app_grab: false,
            };
            if inner.cursor_state != next_state {
                inner.cursor_state = next_state;
                inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
                if trace_enabled() {
                    let log_idx = TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
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

        // Trim over-allocated HW cursor buffers (e.g. NVIDIA's 256×256 plane with
        // the cursor in one corner) to their opaque content, so the visible cursor
        // is sent at native size instead of being downscaled to fit the budget.
        let (crop_pixels, crop_w, crop_h, crop_dx, crop_dy) =
            match crop_cursor_to_opaque(&cursor.pixels, cursor.width, cursor.height) {
                Some(c) => c,
                None => (cursor.pixels.to_vec(), cursor.width, cursor.height, 0, 0),
            };
        // Backends that expose a real hotspot (pipewire/x11) keep it, shifted by
        // the crop. KMS can't read the hotspot (legacy NVIDIA cursor plane) and
        // delivers (0,0); estimate it from the cropped shape so arrows anchor at
        // the tip and I-beams/crosshairs at their center.
        let (hotspot_x, hotspot_y) = if cursor.hotspot_x == 0 && cursor.hotspot_y == 0 {
            estimate_cursor_hotspot(&crop_pixels, crop_w, crop_h)
        } else {
            (
                cursor.hotspot_x.saturating_sub(crop_dx),
                cursor.hotspot_y.saturating_sub(crop_dy),
            )
        };
        let (next_shape, resized) = fit_cursor_shape_to_payload_budget(CursorShape {
            serial: cursor.shape_serial,
            width: crop_w.min(u16::MAX as u32) as u16,
            height: crop_h.min(u16::MAX as u32) as u16,
            hotspot_x: hotspot_x.min(u16::MAX as u32) as u16,
            hotspot_y: hotspot_y.min(u16::MAX as u32) as u16,
            rgba: bgra_to_rgba_premultiplied(&crop_pixels),
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
        // Cropping moved the visible top-left by (crop_dx, crop_dy) within the
        // source buffer, so the reported on-screen position shifts to match.
        let cursor_x = cursor.x + crop_dx as i32;
        let cursor_y = cursor.y + crop_dy as i32;
        // A visible HW cursor means the app is showing a pointer — even inside a
        // game this is a menu / inventory the user must click, so game_mode does
        // NOT force a grab here (that's what hid the menu cursor before). The
        // only relative signal left on a *visible* cursor is the warp detector:
        // if the client keeps commanding the cursor but this position does not
        // follow, the app is doing mouselook without hiding the cursor.
        let app_grab = inner.warp.observe_cursor(cursor_x, cursor_y);
        let next_state = CursorState {
            serial,
            x: cursor_x,
            y: cursor_y,
            visible: cursor.visible,
            app_grab,
        };
        let app_grab_changed = inner.cursor_state.app_grab != next_state.app_grab;
        if inner.cursor_state != next_state {
            inner.cursor_state = next_state;
            inner.cursor_state_version = inner.cursor_state_version.wrapping_add(1);
            if app_grab_changed {
                eprintln!("[input][warp] app_grab={app_grab} (mouselook-without-hide detection)");
            }
            if trace_enabled() {
                let log_idx = TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                if log_idx < 12 {
                    eprintln!(
                        "[trace][cursor] updated state serial={} pos=({}, {}) visible={} app_grab={}",
                        inner.cursor_state.serial,
                        inner.cursor_state.x,
                        inner.cursor_state.y,
                        inner.cursor_state.visible,
                        inner.cursor_state.app_grab
                    );
                }
            }
        }
    }

    fn set_active_controller(&self, controller_id: Option<u32>) {
        self.active_controller_id
            .store(controller_id.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn cursor_messages(
        &self,
        _client_id: u32,
        versions: &mut CursorVersionCursor,
    ) -> Vec<ControlMessage> {
        let inner = self.inner.lock().unwrap();
        if inner.controller_id.is_none() {
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
                Ok((n, src)) => {
                    if let Some((header, credential, packet)) = InputPacket::deserialize(&buf[..n])
                    {
                        self.handle_input_packet(header.seq, credential, packet, src);
                    }
                }
                Err(err) => {
                    eprintln!("[input] UDP receive failed: {err}");
                    break;
                }
            }
        }
    }

    pub fn handle_input_packet(
        &self,
        seq: u16,
        credential: InputCredential,
        packet: InputPacket,
        src: SocketAddr,
    ) {
        self.handle_registered_input_packet(
            seq,
            credential,
            packet,
            InputPacketSource::Direct(src),
        );
    }

    pub fn handle_tunnel_input_packet(
        &self,
        seq: u16,
        credential: InputCredential,
        packet: InputPacket,
        client_id: u32,
    ) {
        if input_packet_client_id(&packet) != client_id {
            return;
        }
        self.handle_registered_input_packet(seq, credential, packet, InputPacketSource::Tunnel);
    }

    fn handle_registered_input_packet(
        &self,
        seq: u16,
        credential: InputCredential,
        packet: InputPacket,
        source: InputPacketSource,
    ) {
        let client_id = input_packet_client_id(&packet);
        let active_clients = self.active_clients.lock().unwrap();
        let credential_matches = match active_clients.get(&client_id) {
            Some(ActiveInputClient::Direct {
                credential: expected,
                ..
            })
            | Some(ActiveInputClient::Tunnel {
                credential: expected,
                ..
            }) => *expected == credential,
            None => false,
        };
        if !credential_matches {
            return;
        }
        let media_dest = match (active_clients.get(&client_id), source) {
            (
                Some(ActiveInputClient::Direct {
                    source_ip,
                    media_dest,
                    ..
                }),
                InputPacketSource::Direct(src),
            ) if *source_ip == src.ip() => Some((Arc::clone(media_dest), src)),
            (Some(ActiveInputClient::Tunnel { .. }), InputPacketSource::Tunnel) => None,
            _ => return,
        };

        let mut inner = self.inner.lock().unwrap();
        if !inner.accept_input_seq(client_id, seq) {
            return;
        }
        if let Some((cell, src)) = media_dest {
            let mut dest = cell.lock().unwrap();
            if *dest != src {
                println!(
                    "[input] client {client_id} return path moved {} -> {src}",
                    *dest
                );
                *dest = src;
            }
        }
        drop(active_clients);
        if matches!(inner.backend, InputBackend::Unavailable) {
            return;
        }
        // B5: implicit control grab — first client to send input takes control —
        // but a stray/late/duplicate packet from a *non-owner* must not steal
        // control from the active owner. Stealing would release the owner's held
        // buttons/keys and hijack the session, so ignore non-owner input while
        // someone else holds control (ownership frees on disconnect/idle-timeout).
        if let Some(owner) = inner.controller_id {
            if owner != client_id {
                return;
            }
        } else if input_packet_is_neutral(&packet) {
            // Bootstrap snapshots establish the return path but must not claim control.
            return;
        }
        inner.activate_controller(client_id);
        self.set_active_controller(Some(client_id));

        match packet {
            InputPacket::MouseAbsolute(packet) => {
                inner.sync_buttons(packet.buttons);
                inner.move_absolute(packet.x, packet.y);
                // Cursor position is no longer predicted from injected input.
                // `CursorState` is sourced solely from the capture backend's
                // real cursor metadata (PipeWire SPA_META_Cursor / XFixes), so
                // there is a single source of truth and no feedback loop with
                // the client's locally-drawn cursor.
                let (sx, sy) = (
                    i64::from(normalized_to_stream_coord(packet.x, inner.stream_width)),
                    i64::from(normalized_to_stream_coord(packet.y, inner.stream_height)),
                );
                inner.warp.observe_command_absolute(sx, sy);
            }
            InputPacket::MouseRelative(packet) => {
                inner.sync_buttons(packet.buttons);
                inner.move_relative(packet.dx, packet.dy);
                inner
                    .warp
                    .observe_command_relative(i64::from(packet.dx), i64::from(packet.dy));
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

#[derive(Clone, Copy)]
enum InputPacketSource {
    Direct(SocketAddr),
    Tunnel,
}

fn input_packet_client_id(packet: &InputPacket) -> u32 {
    match packet {
        InputPacket::MouseAbsolute(packet) => packet.client_id,
        InputPacket::MouseRelative(packet) => packet.client_id,
        InputPacket::MouseButtons(packet) => packet.client_id,
        InputPacket::MouseWheel(packet) => packet.client_id,
        InputPacket::KeyboardState(packet) => packet.client_id,
    }
}

impl InputRuntimeInner {
    fn accept_input_seq(&mut self, client_id: u32, seq: u16) -> bool {
        if let Some(last_seq) = self.last_input_seq_by_client.get(&client_id).copied() {
            if !input_seq_is_newer(seq, last_seq) {
                return false;
            }
        }

        self.last_input_seq_by_client.insert(client_id, seq);
        true
    }

    fn activate_controller(&mut self, client_id: u32) {
        if self.controller_id == Some(client_id) {
            return;
        }
        self.release_all_inputs();
        self.controller_id = Some(client_id);
        self.button_mask = 0;
        self.keyboard_state = [0u8; KEYBOARD_STATE_BYTES];
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

    #[cfg(test)]
    fn predict_cursor_absolute(&mut self, x: u16, y: u16) {
        if !self.cursor_prediction_active() {
            return;
        }

        let (hotspot_x, hotspot_y) = self.cursor_hotspot();
        let target_x = normalized_to_stream_coord(x, self.stream_width);
        let target_y = normalized_to_stream_coord(y, self.stream_height);
        let next_state = CursorState {
            serial: self.cursor_state.serial,
            x: target_x - hotspot_x,
            y: target_y - hotspot_y,
            visible: true,
            app_grab: self.cursor_state.app_grab,
        };
        self.set_predicted_cursor_state(next_state);
    }

    #[cfg(test)]
    fn predict_cursor_relative(&mut self, dx: i16, dy: i16) {
        if !self.cursor_prediction_active() {
            return;
        }

        let (hotspot_x, hotspot_y) = self.cursor_hotspot();
        let target_x = clamp_stream_coord(
            i64::from(self.cursor_state.x) + i64::from(hotspot_x) + i64::from(dx),
            self.stream_width,
        );
        let target_y = clamp_stream_coord(
            i64::from(self.cursor_state.y) + i64::from(hotspot_y) + i64::from(dy),
            self.stream_height,
        );
        let next_state = CursorState {
            serial: self.cursor_state.serial,
            x: target_x - hotspot_x,
            y: target_y - hotspot_y,
            visible: true,
            app_grab: self.cursor_state.app_grab,
        };
        self.set_predicted_cursor_state(next_state);
    }

    #[cfg(test)]
    fn cursor_prediction_active(&self) -> bool {
        self.capabilities.separate_cursor
            && self.cursor_state.visible
            && self.stream_width > 0
            && self.stream_height > 0
    }

    #[cfg(test)]
    fn cursor_hotspot(&self) -> (i32, i32) {
        self.cursor_shape
            .as_ref()
            .map(|shape| (i32::from(shape.hotspot_x), i32::from(shape.hotspot_y)))
            .unwrap_or((0, 0))
    }

    #[cfg(test)]
    fn set_predicted_cursor_state(&mut self, next_state: CursorState) {
        if self.cursor_state == next_state {
            return;
        }
        self.cursor_state = next_state;
        self.cursor_state_version = self.cursor_state_version.wrapping_add(1);
        if trace_enabled() {
            let log_idx = TRACE_CURSOR_UPDATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if log_idx < 12 {
                eprintln!(
                    "[trace][cursor] predicted state serial={} pos=({}, {}) visible={}",
                    self.cursor_state.serial,
                    self.cursor_state.x,
                    self.cursor_state.y,
                    self.cursor_state.visible
                );
            }
        }
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
        for_each_keyboard_transition(self.keyboard_state, next, |key, now_pressed| {
            self.inject_key(key, now_pressed);
        });
        self.keyboard_state = next;
    }

    fn inject_text(&mut self, _text: &str) -> bool {
        let held_modifiers: Vec<_> = KEYBOARD_MODIFIERS
            .into_iter()
            .filter(|key| keyboard_state_contains(&self.keyboard_state, *key))
            .collect();
        for key in held_modifiers.iter().rev() {
            self.inject_key(*key, false);
        }
        let injected = match &mut self.backend {
            #[cfg(target_os = "windows")]
            InputBackend::Windows(controller) => controller.text(_text),
            #[cfg(target_os = "macos")]
            InputBackend::Macos(controller) => controller.text(_text),
            _ => false,
        };
        for key in held_modifiers {
            self.inject_key(key, true);
        }
        injected
    }

    fn inject_key(&mut self, key: KeyboardKey, pressed: bool) {
        match &mut self.backend {
            InputBackend::Unavailable => {}
            #[cfg(target_os = "linux")]
            InputBackend::X11(controller) => controller.key(key, pressed),
            #[cfg(target_os = "linux")]
            InputBackend::Uinput(controller) => controller.key(key, pressed),
            #[cfg(target_os = "linux")]
            InputBackend::PortalRemoteDesktop(controller) => {
                if let Some(code) = linux_key_code(key) {
                    if let Err(e) = controller.notify_keyboard_keycode(code, pressed) {
                        log_portal_error("notify_keyboard_keycode", e);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            InputBackend::Windows(controller) => controller.key(key, pressed),
            #[cfg(target_os = "macos")]
            InputBackend::Macos(controller) => controller.key(key, pressed),
        }
    }
}

const KEYBOARD_MODIFIERS: [KeyboardKey; 8] = [
    KeyboardKey::LeftShift,
    KeyboardKey::LeftCtrl,
    KeyboardKey::LeftAlt,
    KeyboardKey::LeftMeta,
    KeyboardKey::RightShift,
    KeyboardKey::RightCtrl,
    KeyboardKey::RightAlt,
    KeyboardKey::RightMeta,
];

fn keyboard_state_contains(state: &[u8; KEYBOARD_STATE_BYTES], key: KeyboardKey) -> bool {
    let (byte, bit) = key.bit();
    state[byte] & bit != 0
}

fn input_packet_is_neutral(packet: &InputPacket) -> bool {
    match packet {
        InputPacket::MouseButtons(packet) => packet.buttons == 0,
        InputPacket::KeyboardState(packet) => packet.pressed.iter().all(|byte| *byte == 0),
        _ => false,
    }
}

fn for_each_keyboard_transition(
    previous: [u8; KEYBOARD_STATE_BYTES],
    next: [u8; KEYBOARD_STATE_BYTES],
    mut transition: impl FnMut(KeyboardKey, bool),
) {
    // Release ordinary keys before modifiers, then press modifiers before ordinary keys.
    // This preserves chord semantics even when an intermediate UDP snapshot is lost.
    for phase in 0..4 {
        for index in 0..KeyboardKey::COUNT {
            let byte = index / 8;
            let bit = 1 << (index % 8);
            let was_pressed = previous[byte] & bit != 0;
            let now_pressed = next[byte] & bit != 0;
            if was_pressed == now_pressed {
                continue;
            }
            let Some(key) = KeyboardKey::from_u8(index as u8) else {
                continue;
            };
            let modifier = matches!(
                key,
                KeyboardKey::LeftShift
                    | KeyboardKey::LeftCtrl
                    | KeyboardKey::LeftAlt
                    | KeyboardKey::LeftMeta
                    | KeyboardKey::RightShift
                    | KeyboardKey::RightCtrl
                    | KeyboardKey::RightAlt
                    | KeyboardKey::RightMeta
            );
            let in_phase = match phase {
                0 => !now_pressed && !modifier,
                1 => !now_pressed && modifier,
                2 => now_pressed && modifier,
                _ => now_pressed && !modifier,
            };
            if in_phase {
                transition(key, now_pressed);
            }
        }
    }
}

fn input_seq_is_newer(seq: u16, last_seq: u16) -> bool {
    let delta = seq.wrapping_sub(last_seq);
    delta != 0 && delta < 0x8000
}

fn normalized_to_stream_coord(value: u16, span: u32) -> i32 {
    if span <= 1 {
        0
    } else {
        ((i64::from(value) * i64::from(span - 1) + 32767) / 65535) as i32
    }
}

#[cfg(test)]
fn clamp_stream_coord(value: i64, span: u32) -> i32 {
    if span <= 1 {
        0
    } else {
        value.clamp(0, i64::from(span - 1)) as i32
    }
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
struct WheelAccumulator {
    x_units: i32,
    y_units: i32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl WheelAccumulator {
    fn push_and_take_steps(&mut self, delta_x: i16, delta_y: i16) -> (i16, i16) {
        (
            wheel_units_to_steps(&mut self.x_units, delta_x),
            wheel_units_to_steps(&mut self.y_units, delta_y),
        )
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wheel_units_to_steps(pending_units: &mut i32, delta_units: i16) -> i16 {
    *pending_units += i32::from(delta_units);
    let step_units = i32::from(MOUSE_WHEEL_STEP_UNITS);
    let steps =
        (*pending_units / step_units).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
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
        self.tracked_x = self.origin_x + ((x as i64 * (width - 1).max(0) + 32767) / 65535) as i32;
        self.tracked_y = self.origin_y + ((y as i64 * (height - 1).max(0) + 32767) / 65535) as i32;

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
        if let Some(virtual_key) = windows_key_virtual_key(key) {
            self.send_keyboard(KEYBDINPUT {
                wVk: VIRTUAL_KEY(virtual_key),
                wScan: 0,
                dwFlags: if pressed {
                    Default::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            });
            return;
        }
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

    fn text(&mut self, text: &str) -> bool {
        let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
        for unit in text.encode_utf16() {
            for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        if inputs.is_empty() {
            return false;
        }
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize == inputs.len() }
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

#[cfg(any(target_os = "windows", test))]
fn windows_key_virtual_key(key: KeyboardKey) -> Option<u16> {
    match key {
        KeyboardKey::Pause => Some(0x13),
        _ => None,
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

#[cfg(any(target_os = "windows", test))]
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
        KeyboardKey::CapsLock => (0x3A, false),
        KeyboardKey::NumLock => (0x45, false),
        KeyboardKey::ScrollLock => (0x46, false),
        KeyboardKey::PrintScreen => (0x37, true),
        KeyboardKey::Application => (0x5D, true),
        KeyboardKey::Numpad0 => (0x52, false),
        KeyboardKey::Numpad1 => (0x4F, false),
        KeyboardKey::Numpad2 => (0x50, false),
        KeyboardKey::Numpad3 => (0x51, false),
        KeyboardKey::Numpad4 => (0x4B, false),
        KeyboardKey::Numpad5 => (0x4C, false),
        KeyboardKey::Numpad6 => (0x4D, false),
        KeyboardKey::Numpad7 => (0x47, false),
        KeyboardKey::Numpad8 => (0x48, false),
        KeyboardKey::Numpad9 => (0x49, false),
        KeyboardKey::NumpadDecimal => (0x53, false),
        KeyboardKey::NumpadDivide => (0x35, true),
        KeyboardKey::NumpadMultiply => (0x37, false),
        KeyboardKey::NumpadSubtract => (0x4A, false),
        KeyboardKey::NumpadAdd => (0x4E, false),
        KeyboardKey::NumpadEnter => (0x1C, true),
        KeyboardKey::NumpadEquals => (0x59, false),
        KeyboardKey::NumpadComma => (0x7E, false),
        KeyboardKey::F13 => (0x64, false),
        KeyboardKey::F14 => (0x65, false),
        KeyboardKey::F15 => (0x66, false),
        KeyboardKey::F16 => (0x67, false),
        KeyboardKey::F17 => (0x68, false),
        KeyboardKey::F18 => (0x69, false),
        KeyboardKey::F19 => (0x6A, false),
        KeyboardKey::F20 => (0x6B, false),
        KeyboardKey::F21 => (0x6C, false),
        KeyboardKey::F22 => (0x6D, false),
        KeyboardKey::F23 => (0x6E, false),
        KeyboardKey::F24 => (0x76, false),
        KeyboardKey::VolumeMute => (0x20, true),
        KeyboardKey::VolumeDown => (0x2E, true),
        KeyboardKey::VolumeUp => (0x30, true),
        KeyboardKey::MediaPrevious => (0x10, true),
        KeyboardKey::MediaNext => (0x19, true),
        KeyboardKey::MediaPlayPause => (0x22, true),
        KeyboardKey::MediaStop => (0x24, true),
        KeyboardKey::IntlBackslash => (0x56, false),
        KeyboardKey::Pause => return None,
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
                    cursor_position_reliable: true,
                    text_input: false,
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
                        // Capture is still X11/XFixes → real cursor position.
                        cursor_position_reliable: true,
                        text_input: false,
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
                    cursor_position_reliable: false,
                    text_input: false,
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
                        cursor_position_reliable: false,
                        text_input: false,
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
                    cursor_position_reliable: false,
                    text_input: false,
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
                        cursor_position_reliable: true,
                        text_input: false,
                    },
                    "portal/remote-desktop",
                ))
            } else {
                UinputMouseController::new().map(|controller| {
                    let absolute = controller.supports_absolute();
                    (
                        InputBackend::Uinput(controller),
                        InputCapabilities {
                            mouse_absolute: absolute,
                            mouse_relative: true,
                            keyboard: true,
                            separate_cursor: true,
                            hover_capture: absolute,
                            // PipeWire SPA_META_Cursor → real cursor position.
                            cursor_position_reliable: true,
                            text_input: false,
                        },
                        if absolute {
                            "uinput(abs+rel)"
                        } else {
                            "uinput(rel)"
                        },
                    )
                })
            }
        }
        "kms" => UinputMouseController::new().map(|controller| {
            let absolute = controller.supports_absolute();
            (
                InputBackend::Uinput(controller),
                InputCapabilities {
                    mouse_absolute: absolute,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: true,
                    hover_capture: absolute,
                    // KMS cursor plane reports (0,0) on NVIDIA's legacy plane —
                    // a separate cursor image but no usable position.
                    cursor_position_reliable: false,
                    text_input: false,
                },
                if absolute {
                    "uinput(abs+rel)"
                } else {
                    "uinput(rel)"
                },
            )
        }),
        // ext-image-copy-capture-v1 paints the cursor into the frame (no
        // separate cursor metadata), so it behaves like wlroots screencopy:
        // uinput relative injection, no absolute/hover, no separate cursor.
        // Without this arm input falls through to Unavailable on compositors
        // that land on the ext-image-copy fallback path.
        "ext-image-copy" => UinputMouseController::new().map(|controller| {
            (
                InputBackend::Uinput(controller),
                InputCapabilities {
                    mouse_absolute: false,
                    mouse_relative: true,
                    keyboard: true,
                    separate_cursor: false,
                    hover_capture: false,
                    cursor_position_reliable: false,
                    text_input: false,
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
const EV_ABS: u16 = 0x03;
#[cfg(target_os = "linux")]
const ABS_X: u16 = 0x00;
#[cfg(target_os = "linux")]
const ABS_Y: u16 = 0x01;
/// Range of the absolute pointer axes. The client sends absolute coordinates
/// already normalized to 0..=u16::MAX across the captured output, so mapping
/// the axis range to the same span makes injection a direct pass-through.
#[cfg(target_os = "linux")]
const ABS_AXIS_MAX: i32 = u16::MAX as i32;
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
const KEY_KPASTERISK: u16 = 55;
#[cfg(target_os = "linux")]
const KEY_CAPSLOCK: u16 = 58;
#[cfg(target_os = "linux")]
const KEY_NUMLOCK: u16 = 69;
#[cfg(target_os = "linux")]
const KEY_SCROLLLOCK: u16 = 70;
#[cfg(target_os = "linux")]
const KEY_KP7: u16 = 71;
#[cfg(target_os = "linux")]
const KEY_KP8: u16 = 72;
#[cfg(target_os = "linux")]
const KEY_KP9: u16 = 73;
#[cfg(target_os = "linux")]
const KEY_KPMINUS: u16 = 74;
#[cfg(target_os = "linux")]
const KEY_KP4: u16 = 75;
#[cfg(target_os = "linux")]
const KEY_KP5: u16 = 76;
#[cfg(target_os = "linux")]
const KEY_KP6: u16 = 77;
#[cfg(target_os = "linux")]
const KEY_KPPLUS: u16 = 78;
#[cfg(target_os = "linux")]
const KEY_KP1: u16 = 79;
#[cfg(target_os = "linux")]
const KEY_KP2: u16 = 80;
#[cfg(target_os = "linux")]
const KEY_KP3: u16 = 81;
#[cfg(target_os = "linux")]
const KEY_KP0: u16 = 82;
#[cfg(target_os = "linux")]
const KEY_KPDOT: u16 = 83;
#[cfg(target_os = "linux")]
const KEY_102ND: u16 = 86;
#[cfg(target_os = "linux")]
const KEY_KPENTER: u16 = 96;
#[cfg(target_os = "linux")]
const KEY_KPSLASH: u16 = 98;
#[cfg(target_os = "linux")]
const KEY_SYSRQ: u16 = 99;
#[cfg(target_os = "linux")]
const KEY_MUTE: u16 = 113;
#[cfg(target_os = "linux")]
const KEY_VOLUMEDOWN: u16 = 114;
#[cfg(target_os = "linux")]
const KEY_VOLUMEUP: u16 = 115;
#[cfg(target_os = "linux")]
const KEY_KPEQUAL: u16 = 117;
#[cfg(target_os = "linux")]
const KEY_PAUSE: u16 = 119;
#[cfg(target_os = "linux")]
const KEY_KPCOMMA: u16 = 121;
#[cfg(target_os = "linux")]
const KEY_MENU: u16 = 139;
#[cfg(target_os = "linux")]
const KEY_NEXTSONG: u16 = 163;
#[cfg(target_os = "linux")]
const KEY_PLAYPAUSE: u16 = 164;
#[cfg(target_os = "linux")]
const KEY_PREVIOUSSONG: u16 = 165;
#[cfg(target_os = "linux")]
const KEY_STOPCD: u16 = 166;
#[cfg(target_os = "linux")]
const KEY_F13: u16 = 183;
#[cfg(target_os = "linux")]
const KEY_F14: u16 = 184;
#[cfg(target_os = "linux")]
const KEY_F15: u16 = 185;
#[cfg(target_os = "linux")]
const KEY_F16: u16 = 186;
#[cfg(target_os = "linux")]
const KEY_F17: u16 = 187;
#[cfg(target_os = "linux")]
const KEY_F18: u16 = 188;
#[cfg(target_os = "linux")]
const KEY_F19: u16 = 189;
#[cfg(target_os = "linux")]
const KEY_F20: u16 = 190;
#[cfg(target_os = "linux")]
const KEY_F21: u16 = 191;
#[cfg(target_os = "linux")]
const KEY_F22: u16 = 192;
#[cfg(target_os = "linux")]
const KEY_F23: u16 = 193;
#[cfg(target_os = "linux")]
const KEY_F24: u16 = 194;

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
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// Mirrors the kernel `struct uinput_abs_setup` (UI_ABS_SETUP). `#[repr(C)]`
/// reproduces the 2-byte pad after `code` before the i32-aligned `absinfo`.
#[cfg(target_os = "linux")]
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
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
nix::ioctl_write_int!(ui_set_absbit, b'U', 103);
#[cfg(target_os = "linux")]
nix::ioctl_write_ptr!(ui_dev_setup, b'U', 3, UinputSetup);
#[cfg(target_os = "linux")]
nix::ioctl_write_ptr!(ui_abs_setup, b'U', 4, UinputAbsSetup);
#[cfg(target_os = "linux")]
nix::ioctl_none!(ui_dev_create, b'U', 1);
#[cfg(target_os = "linux")]
nix::ioctl_none!(ui_dev_destroy, b'U', 2);

#[cfg(target_os = "linux")]
struct UinputMouseController {
    file: File,
    /// Optional absolute pointer device. Present only when `ST_UINPUT_ABSOLUTE`
    /// is enabled and the device was created successfully; otherwise absolute
    /// injection is unavailable and the backend advertises `mouse_absolute=false`.
    abs_file: Option<File>,
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
            for wire_id in 0..KeyboardKey::COUNT as u8 {
                if let Some(code) = KeyboardKey::from_u8(wire_id).and_then(linux_key_code) {
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

        // Absolute positioning needs a second device with ABS axes; uinput
        // relative motion goes through libinput acceleration and cannot land at
        // an exact target. Default-on (validated live); ST_UINPUT_ABSOLUTE=0
        // forces relative-only. On any failure fall back to relative-only,
        // never breaking input.
        let abs_file = if uinput_absolute_enabled() {
            match create_uinput_abs_device() {
                Ok(abs) => {
                    println!("[input] uinput absolute pointer device enabled");
                    Some(abs)
                }
                Err(e) => {
                    eprintln!("[input] uinput absolute device unavailable ({e}); relative-only");
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            file,
            abs_file,
            wheel_accumulator: WheelAccumulator::default(),
        })
    }

    fn supports_absolute(&self) -> bool {
        self.abs_file.is_some()
    }

    fn move_absolute(&mut self, x: u16, y: u16) {
        let Some(abs_file) = self.abs_file.as_mut() else {
            return;
        };
        // Client coordinates are already normalized to 0..=u16::MAX across the
        // captured output and the axis range is the same span, so emit them
        // directly. Buttons stay on the relative device: libinput merges every
        // pointer in the seat into a single cursor, so a click from the relative
        // device lands wherever this absolute motion put the cursor.
        let _ = write_uinput_event(abs_file, EV_ABS, ABS_X, x as i32);
        let _ = write_uinput_event(abs_file, EV_ABS, ABS_Y, y as i32);
        let _ = write_uinput_event(abs_file, EV_SYN, SYN_REPORT, 0);
        let _ = abs_file.flush();
    }

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
        write_uinput_event(&mut self.file, type_, code, value)
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
            if let Some(abs_file) = self.abs_file.as_ref() {
                let _ = ui_dev_destroy(abs_file.as_raw_fd());
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn write_uinput_event(file: &mut File, type_: u16, code: u16, value: i32) -> Result<(), String> {
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
    file.write_all(raw)
        .map_err(|e| format!("uinput write: {e}"))
}

#[cfg(target_os = "linux")]
fn uinput_absolute_enabled() -> bool {
    // Default-on after live validation (CLAUDE.md auto-enable rule). The env var
    // is now an escape hatch to force the old relative-only behavior.
    !matches!(
        std::env::var("ST_UINPUT_ABSOLUTE").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Create a second uinput device exposing absolute X/Y axes, so the server can
/// position the cursor exactly (no libinput acceleration applied to ABS events).
/// Declaring the standard mouse buttons makes libinput classify it as an
/// absolute pointer rather than a touchscreen/tablet; the buttons themselves are
/// never emitted here (clicks ride the relative device's single shared cursor).
#[cfg(target_os = "linux")]
fn create_uinput_abs_device() -> Result<File, String> {
    use std::os::fd::AsRawFd;
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

    let fd = file.as_raw_fd();
    unsafe {
        ui_set_evbit(fd, EV_KEY as _).map_err(|e| format!("UI_SET_EVBIT key: {e}"))?;
        ui_set_evbit(fd, EV_ABS as _).map_err(|e| format!("UI_SET_EVBIT abs: {e}"))?;
        ui_set_keybit(fd, BTN_LEFT as _).map_err(|e| format!("UI_SET_KEYBIT left: {e}"))?;
        ui_set_keybit(fd, BTN_RIGHT as _).map_err(|e| format!("UI_SET_KEYBIT right: {e}"))?;
        ui_set_keybit(fd, BTN_MIDDLE as _).map_err(|e| format!("UI_SET_KEYBIT middle: {e}"))?;
        ui_set_absbit(fd, ABS_X as _).map_err(|e| format!("UI_SET_ABSBIT x: {e}"))?;
        ui_set_absbit(fd, ABS_Y as _).map_err(|e| format!("UI_SET_ABSBIT y: {e}"))?;
    }

    let mut setup = UinputSetup {
        id: LinuxInputId {
            bustype: BUS_USB,
            vendor: 0x1209,
            product: 0x5355,
            version: 1,
        },
        name: [0u8; UINPUT_NAME_LEN],
        ff_effects_max: 0,
    };
    let name = b"st-virtual-abs-mouse";
    setup.name[..name.len()].copy_from_slice(name);

    unsafe {
        ui_dev_setup(fd, &setup).map_err(|e| format!("UI_DEV_SETUP: {e}"))?;
        for code in [ABS_X, ABS_Y] {
            let abs_setup = UinputAbsSetup {
                code,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: ABS_AXIS_MAX,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            ui_abs_setup(fd, &abs_setup).map_err(|e| format!("UI_ABS_SETUP {code}: {e}"))?;
        }
        ui_dev_create(fd).map_err(|e| format!("UI_DEV_CREATE: {e}"))?;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    Ok(file)
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
        KeyboardKey::CapsLock => KEY_CAPSLOCK,
        KeyboardKey::NumLock => KEY_NUMLOCK,
        KeyboardKey::ScrollLock => KEY_SCROLLLOCK,
        KeyboardKey::PrintScreen => KEY_SYSRQ,
        KeyboardKey::Pause => KEY_PAUSE,
        KeyboardKey::Application => KEY_MENU,
        KeyboardKey::Numpad0 => KEY_KP0,
        KeyboardKey::Numpad1 => KEY_KP1,
        KeyboardKey::Numpad2 => KEY_KP2,
        KeyboardKey::Numpad3 => KEY_KP3,
        KeyboardKey::Numpad4 => KEY_KP4,
        KeyboardKey::Numpad5 => KEY_KP5,
        KeyboardKey::Numpad6 => KEY_KP6,
        KeyboardKey::Numpad7 => KEY_KP7,
        KeyboardKey::Numpad8 => KEY_KP8,
        KeyboardKey::Numpad9 => KEY_KP9,
        KeyboardKey::NumpadDecimal => KEY_KPDOT,
        KeyboardKey::NumpadDivide => KEY_KPSLASH,
        KeyboardKey::NumpadMultiply => KEY_KPASTERISK,
        KeyboardKey::NumpadSubtract => KEY_KPMINUS,
        KeyboardKey::NumpadAdd => KEY_KPPLUS,
        KeyboardKey::NumpadEnter => KEY_KPENTER,
        KeyboardKey::NumpadEquals => KEY_KPEQUAL,
        KeyboardKey::NumpadComma => KEY_KPCOMMA,
        KeyboardKey::F13 => KEY_F13,
        KeyboardKey::F14 => KEY_F14,
        KeyboardKey::F15 => KEY_F15,
        KeyboardKey::F16 => KEY_F16,
        KeyboardKey::F17 => KEY_F17,
        KeyboardKey::F18 => KEY_F18,
        KeyboardKey::F19 => KEY_F19,
        KeyboardKey::F20 => KEY_F20,
        KeyboardKey::F21 => KEY_F21,
        KeyboardKey::F22 => KEY_F22,
        KeyboardKey::F23 => KEY_F23,
        KeyboardKey::F24 => KEY_F24,
        KeyboardKey::VolumeMute => KEY_MUTE,
        KeyboardKey::VolumeDown => KEY_VOLUMEDOWN,
        KeyboardKey::VolumeUp => KEY_VOLUMEUP,
        KeyboardKey::MediaPrevious => KEY_PREVIOUSSONG,
        KeyboardKey::MediaNext => KEY_NEXTSONG,
        KeyboardKey::MediaPlayPause => KEY_PLAYPAUSE,
        KeyboardKey::MediaStop => KEY_STOPCD,
        KeyboardKey::IntlBackslash => KEY_102ND,
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
    fn CGEventKeyboardSetUnicodeString(
        event: *mut std::ffi::c_void,
        string_length: usize,
        unicode_string: *const u16,
    );
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

    fn text(&mut self, text: &str) -> bool {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if utf16.is_empty() {
            return false;
        }
        let down = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), 0, true) };
        if down.is_null() {
            return false;
        }
        unsafe {
            CGEventKeyboardSetUnicodeString(down, utf16.len(), utf16.as_ptr());
            CGEventPost(KCG_HID_EVENT_TAP, down);
            CFRelease(down);
        }
        let up = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), 0, false) };
        if !up.is_null() {
            unsafe {
                CGEventPost(KCG_HID_EVENT_TAP, up);
                CFRelease(up);
            }
        }
        true
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

#[cfg(any(target_os = "macos", test))]
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
        KeyboardKey::CapsLock => 57,
        KeyboardKey::Application => 110,
        KeyboardKey::Numpad0 => 82,
        KeyboardKey::Numpad1 => 83,
        KeyboardKey::Numpad2 => 84,
        KeyboardKey::Numpad3 => 85,
        KeyboardKey::Numpad4 => 86,
        KeyboardKey::Numpad5 => 87,
        KeyboardKey::Numpad6 => 88,
        KeyboardKey::Numpad7 => 89,
        KeyboardKey::Numpad8 => 91,
        KeyboardKey::Numpad9 => 92,
        KeyboardKey::NumpadDecimal => 65,
        KeyboardKey::NumpadDivide => 75,
        KeyboardKey::NumpadMultiply => 67,
        KeyboardKey::NumpadSubtract => 78,
        KeyboardKey::NumpadAdd => 69,
        KeyboardKey::NumpadEnter => 76,
        KeyboardKey::NumpadEquals => 81,
        KeyboardKey::NumpadComma => 95,
        KeyboardKey::F13 => 105,
        KeyboardKey::F14 => 107,
        KeyboardKey::F15 => 113,
        KeyboardKey::F16 => 106,
        KeyboardKey::F17 => 64,
        KeyboardKey::F18 => 79,
        KeyboardKey::F19 => 80,
        KeyboardKey::F20 => 90,
        KeyboardKey::VolumeMute => 74,
        KeyboardKey::VolumeDown => 73,
        KeyboardKey::VolumeUp => 72,
        KeyboardKey::IntlBackslash => 10,
        KeyboardKey::NumLock
        | KeyboardKey::ScrollLock
        | KeyboardKey::PrintScreen
        | KeyboardKey::Pause
        | KeyboardKey::F21
        | KeyboardKey::F22
        | KeyboardKey::F23
        | KeyboardKey::F24
        | KeyboardKey::MediaPrevious
        | KeyboardKey::MediaNext
        | KeyboardKey::MediaPlayPause
        | KeyboardKey::MediaStop => return None,
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
                display,
                root,
                &mut root_ret,
                &mut child_ret,
                &mut root_x,
                &mut root_y,
                &mut win_x,
                &mut win_y,
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
        self.tracked_x = target_x;
        self.tracked_y = target_y;
        // True absolute warp: the cursor lands exactly at the client position
        // every time, with no accumulated drift. Desktop control sends absolute
        // coordinates; game mouselook sends relative deltas through
        // move_relative (XTestFakeRelativeMotionEvent), which is what produces
        // the XI2 RawMotion events games read for camera rotation.
        unsafe {
            x11_ffi::XTestFakeMotionEvent(self.display, self._screen, target_x, target_y, 0);
            x11_ffi::XSync(self.display, 0);
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
        KeyboardKey::CapsLock => c"Caps_Lock",
        KeyboardKey::NumLock => c"Num_Lock",
        KeyboardKey::ScrollLock => c"Scroll_Lock",
        KeyboardKey::PrintScreen => c"Print",
        KeyboardKey::Pause => c"Pause",
        KeyboardKey::Application => c"Menu",
        KeyboardKey::Numpad0 => c"KP_0",
        KeyboardKey::Numpad1 => c"KP_1",
        KeyboardKey::Numpad2 => c"KP_2",
        KeyboardKey::Numpad3 => c"KP_3",
        KeyboardKey::Numpad4 => c"KP_4",
        KeyboardKey::Numpad5 => c"KP_5",
        KeyboardKey::Numpad6 => c"KP_6",
        KeyboardKey::Numpad7 => c"KP_7",
        KeyboardKey::Numpad8 => c"KP_8",
        KeyboardKey::Numpad9 => c"KP_9",
        KeyboardKey::NumpadDecimal => c"KP_Decimal",
        KeyboardKey::NumpadDivide => c"KP_Divide",
        KeyboardKey::NumpadMultiply => c"KP_Multiply",
        KeyboardKey::NumpadSubtract => c"KP_Subtract",
        KeyboardKey::NumpadAdd => c"KP_Add",
        KeyboardKey::NumpadEnter => c"KP_Enter",
        KeyboardKey::NumpadEquals => c"KP_Equal",
        KeyboardKey::NumpadComma => c"KP_Separator",
        KeyboardKey::F13 => c"F13",
        KeyboardKey::F14 => c"F14",
        KeyboardKey::F15 => c"F15",
        KeyboardKey::F16 => c"F16",
        KeyboardKey::F17 => c"F17",
        KeyboardKey::F18 => c"F18",
        KeyboardKey::F19 => c"F19",
        KeyboardKey::F20 => c"F20",
        KeyboardKey::F21 => c"F21",
        KeyboardKey::F22 => c"F22",
        KeyboardKey::F23 => c"F23",
        KeyboardKey::F24 => c"F24",
        KeyboardKey::VolumeMute => c"XF86AudioMute",
        KeyboardKey::VolumeDown => c"XF86AudioLowerVolume",
        KeyboardKey::VolumeUp => c"XF86AudioRaiseVolume",
        KeyboardKey::MediaPrevious => c"XF86AudioPrev",
        KeyboardKey::MediaNext => c"XF86AudioNext",
        KeyboardKey::MediaPlayPause => c"XF86AudioPlay",
        KeyboardKey::MediaStop => c"XF86AudioStop",
        // XKeysymToKeycode("less") may resolve the top-row comma key instead of
        // the physical ISO <LSGT> key. This backend lacks an XKB key-name lookup.
        KeyboardKey::IntlBackslash => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDENTIAL: InputCredential = InputCredential::from_bytes([0x31; 16]);
    const WRONG_CREDENTIAL: InputCredential = InputCredential::from_bytes([0x32; 16]);

    /// Force-enabled detector regardless of `ST_WARP_DETECT` in the env.
    fn enabled_warp() -> WarpDetector {
        let mut w = WarpDetector::new();
        w.enabled = true;
        w.reset();
        w
    }

    #[test]
    fn warp_detector_disabled_never_grabs() {
        let mut w = WarpDetector::new();
        w.enabled = false;
        let mut cx = 0i64;
        for _ in 0..(WARP_WINDOW_SAMPLES * (WARP_ENTER_STREAK + 2)) {
            cx += 60;
            w.observe_command_absolute(cx, 0);
            assert!(!w.observe_cursor(500, 500));
        }
        assert!(!w.app_grab);
    }

    /// Drive converging windows (captured tracks commanded 1:1) until the
    /// detector trusts the position source. Returns the next position cursor.
    fn establish_trust(w: &mut WarpDetector, start: i64) -> i64 {
        let mut p = start;
        for _ in 0..(WARP_WINDOW_SAMPLES * WARP_EXIT_STREAK) {
            p += 60;
            w.observe_command_absolute(p, 0);
            w.observe_cursor(p as i32, 0);
        }
        assert!(w.position_trusted, "tracking cursor must establish trust");
        assert!(!w.app_grab);
        p
    }

    #[test]
    fn warp_detector_untrusted_position_never_grabs() {
        // Backend reports a static / unavailable cursor position: the client
        // commands the cursor all over the desktop but the captured position
        // never moves. Without trust this looks like a warp — it must NOT grab,
        // or the desktop cursor would vanish. (The live regression guard.)
        let mut w = enabled_warp();
        let mut cx = 0i64;
        for _ in 0..(WARP_WINDOW_SAMPLES * (WARP_ENTER_STREAK + 5)) {
            cx += 60;
            w.observe_command_absolute(cx, 0);
            w.observe_cursor(500, 500);
        }
        assert!(!w.position_trusted);
        assert!(!w.app_grab, "untrusted position must never grab");
    }

    #[test]
    fn warp_detector_enters_then_exits() {
        let mut w = enabled_warp();
        let mut cx = establish_trust(&mut w, 0);
        let pin = cx; // captured parks here while the client keeps commanding

        // Diverge: client commands the cursor across the screen, captured pinned
        // (warp-to-centre / raw-input park).
        for _ in 0..(WARP_WINDOW_SAMPLES * WARP_ENTER_STREAK) {
            cx += 60;
            w.observe_command_absolute(cx, 0);
            w.observe_cursor(pin as i32, 0);
        }
        assert!(w.app_grab, "sustained divergence must grab once trusted");

        // Converge: the app released — the captured cursor tracks again.
        let mut pos = pin;
        for _ in 0..(WARP_WINDOW_SAMPLES * WARP_EXIT_STREAK) {
            cx += 60;
            pos += 60;
            w.observe_command_absolute(cx, 0);
            w.observe_cursor(pos as i32, 0);
        }
        assert!(!w.app_grab, "tracking cursor must release");
    }

    #[test]
    fn warp_detector_holds_verdict_while_idle() {
        let mut w = enabled_warp();
        let mut cx = establish_trust(&mut w, 0);
        let pin = cx;
        for _ in 0..(WARP_WINDOW_SAMPLES * WARP_ENTER_STREAK) {
            cx += 60;
            w.observe_command_absolute(cx, 0);
            w.observe_cursor(pin as i32, 0);
        }
        assert!(w.app_grab);
        // Idle: barely any commanded motion. The verdict must not flip on its own
        // (an afk player stays captured; a false positive only clears once real
        // motion is seen to track).
        for _ in 0..(WARP_WINDOW_SAMPLES * 4) {
            cx += 2;
            w.observe_command_absolute(cx, 0);
            w.observe_cursor(pin as i32, 0);
        }
        assert!(w.app_grab, "idle must hold the existing verdict");
    }

    fn predictive_cursor_inner() -> InputRuntimeInner {
        InputRuntimeInner {
            backend: InputBackend::Unavailable,
            backend_label: "test".to_string(),
            capabilities: InputCapabilities {
                mouse_absolute: true,
                mouse_relative: true,
                keyboard: true,
                separate_cursor: true,
                hover_capture: true,
                cursor_position_reliable: true,
                text_input: false,
            },
            controller_id: Some(1),
            last_input_seq_by_client: BTreeMap::new(),
            button_mask: 0,
            keyboard_state: [0u8; KEYBOARD_STATE_BYTES],
            cursor_shape: Some(CursorShape {
                serial: 7,
                width: 16,
                height: 16,
                hotspot_x: 3,
                hotspot_y: 5,
                rgba: vec![0; 16 * 16 * 4],
            }),
            cursor_state: CursorState {
                serial: 7,
                x: 10,
                y: 20,
                visible: true,
                app_grab: false,
            },
            cursor_shape_version: 1,
            cursor_state_version: 1,
            stream_width: 1920,
            stream_height: 1080,
            warp: WarpDetector::new(),
        }
    }

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

    #[test]
    fn neutral_bootstrap_relearns_media_port_without_claiming_control() {
        let runtime = InputRuntime::new();
        let initial = SocketAddr::from(([127, 0, 0, 1], 5000));
        let live = SocketAddr::from(([127, 0, 0, 1], 61000));
        let media_dest = Arc::new(Mutex::new(initial));
        let registration =
            runtime.register_direct_client(1, CREDENTIAL, initial.ip(), Arc::clone(&media_dest));

        runtime.handle_input_packet(
            7,
            CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            }),
            live,
        );

        assert_eq!(*media_dest.lock().unwrap(), live);
        {
            let inner = runtime.inner.lock().unwrap();
            assert_eq!(inner.controller_id, None);
            assert_eq!(inner.last_input_seq_by_client.get(&1), Some(&7));
        }

        drop(registration);
        assert!(!runtime.active_clients.lock().unwrap().contains_key(&1));
        runtime.handle_input_packet(
            8,
            CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            }),
            SocketAddr::from(([127, 0, 0, 1], 62000)),
        );
        assert_eq!(*media_dest.lock().unwrap(), live);
        assert!(!runtime
            .inner
            .lock()
            .unwrap()
            .last_input_seq_by_client
            .contains_key(&1));
    }

    #[test]
    fn unknown_or_wrong_ip_input_does_not_allocate_sequence_or_relearn_destination() {
        let runtime = InputRuntime::new();
        let initial = SocketAddr::from(([127, 0, 0, 1], 5000));
        let media_dest = Arc::new(Mutex::new(initial));
        let _registration =
            runtime.register_direct_client(1, CREDENTIAL, initial.ip(), Arc::clone(&media_dest));

        runtime.handle_input_packet(
            1,
            CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 99,
                buttons: 0,
            }),
            SocketAddr::from(([127, 0, 0, 1], 61000)),
        );
        runtime.handle_input_packet(
            2,
            CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            }),
            SocketAddr::from(([127, 0, 0, 2], 61000)),
        );
        runtime.handle_input_packet(
            3,
            WRONG_CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            }),
            SocketAddr::from(([127, 0, 0, 1], 62000)),
        );

        assert_eq!(*media_dest.lock().unwrap(), initial);
        let inner = runtime.inner.lock().unwrap();
        assert!(!inner.last_input_seq_by_client.contains_key(&99));
        assert!(!inner.last_input_seq_by_client.contains_key(&1));
    }

    #[test]
    fn wrong_credential_is_rejected_before_sequence_and_destination_updates() {
        let runtime = InputRuntime::new();
        let initial = SocketAddr::from(([127, 0, 0, 1], 5000));
        let forged_source = SocketAddr::from(([127, 0, 0, 1], 61000));
        let media_dest = Arc::new(Mutex::new(initial));
        let _registration =
            runtime.register_direct_client(1, CREDENTIAL, initial.ip(), Arc::clone(&media_dest));

        runtime.handle_input_packet(
            55,
            WRONG_CREDENTIAL,
            InputPacket::MouseButtons(st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            }),
            forged_source,
        );

        assert_eq!(*media_dest.lock().unwrap(), initial);
        assert!(!runtime
            .inner
            .lock()
            .unwrap()
            .last_input_seq_by_client
            .contains_key(&1));
    }

    #[test]
    fn registration_drop_releases_owned_input_and_controller_state() {
        let runtime = InputRuntime::new();
        let registration = runtime.register_tunnel_client(7, CREDENTIAL);
        {
            let mut inner = runtime.inner.lock().unwrap();
            inner.controller_id = Some(7);
            inner.button_mask = MOUSE_BUTTON_PRIMARY;
            let (byte, bit) = KeyboardKey::A.bit();
            inner.keyboard_state[byte] = bit;
        }
        runtime.set_active_controller(Some(7));

        drop(registration);

        let inner = runtime.inner.lock().unwrap();
        assert_eq!(inner.controller_id, None);
        assert_eq!(inner.button_mask, 0);
        assert_eq!(inner.keyboard_state, [0; KEYBOARD_STATE_BYTES]);
        assert_eq!(runtime.active_controller_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn neutral_packet_classification_excludes_motion() {
        assert!(input_packet_is_neutral(&InputPacket::MouseButtons(
            st_protocol::MouseButtonsInput {
                client_id: 1,
                buttons: 0,
            },
        )));
        assert!(input_packet_is_neutral(&InputPacket::KeyboardState(
            st_protocol::KeyboardStateInput {
                client_id: 1,
                pressed: [0; KEYBOARD_STATE_BYTES],
            },
        )));
        assert!(!input_packet_is_neutral(&InputPacket::MouseRelative(
            st_protocol::MouseRelativeInput {
                client_id: 1,
                dx: 1,
                dy: 0,
                buttons: 0,
            },
        )));
    }

    #[test]
    fn keyboard_chords_press_modifiers_first_and_release_them_last() {
        let mut chord = [0u8; KEYBOARD_STATE_BYTES];
        for key in [
            KeyboardKey::A,
            KeyboardKey::LeftShift,
            KeyboardKey::LeftCtrl,
        ] {
            let (byte, bit) = key.bit();
            chord[byte] |= bit;
        }

        let mut down = Vec::new();
        for_each_keyboard_transition([0; KEYBOARD_STATE_BYTES], chord, |key, pressed| {
            down.push((key, pressed));
        });
        assert_eq!(
            down,
            vec![
                (KeyboardKey::LeftShift, true),
                (KeyboardKey::LeftCtrl, true),
                (KeyboardKey::A, true),
            ]
        );

        let mut up = Vec::new();
        for_each_keyboard_transition(chord, [0; KEYBOARD_STATE_BYTES], |key, pressed| {
            up.push((key, pressed));
        });
        assert_eq!(
            up,
            vec![
                (KeyboardKey::A, false),
                (KeyboardKey::LeftShift, false),
                (KeyboardKey::LeftCtrl, false),
            ]
        );
    }

    #[test]
    fn text_input_rate_limit_bounds_messages_and_bytes_then_resets() {
        let start = Instant::now();
        let mut message_limited = TextInputRateLimiter {
            window_started: start,
            bytes: 0,
            messages: 0,
        };
        for _ in 0..TEXT_INPUT_RATE_MESSAGES {
            assert!(message_limited.allow(1, start));
        }
        assert!(!message_limited.allow(1, start));
        assert!(message_limited.allow(1, start + TEXT_INPUT_RATE_WINDOW));

        let mut byte_limited = TextInputRateLimiter {
            window_started: start,
            bytes: 0,
            messages: 0,
        };
        for _ in 0..(TEXT_INPUT_RATE_BYTES / MAX_TEXT_INPUT_BYTES) {
            assert!(byte_limited.allow(MAX_TEXT_INPUT_BYTES, start));
        }
        assert!(!byte_limited.allow(1, start));
    }

    #[test]
    fn text_input_requires_registration_capability_and_nonempty_safe_payload() {
        let runtime = InputRuntime::new();
        assert!(!runtime.handle_text_input(1, "text"));
        let _registration = runtime.register_tunnel_client(1, CREDENTIAL);
        assert!(!runtime.handle_text_input(1, ""));
        assert!(!runtime.handle_text_input(1, "a\0b"));
        assert!(!runtime.handle_text_input(1, &"x".repeat(MAX_TEXT_INPUT_BYTES + 1)));
        assert!(!runtime.handle_text_input(1, "still unavailable"));
        assert_eq!(runtime.inner.lock().unwrap().controller_id, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expanded_linux_keys_preserve_physical_identity() {
        for wire_id in 0..KeyboardKey::COUNT as u8 {
            let key = KeyboardKey::from_u8(wire_id).unwrap();
            assert!(linux_key_code(key).is_some(), "missing Linux key {key:?}");
            assert_eq!(
                x11_key_name(key).is_none(),
                key == KeyboardKey::IntlBackslash,
                "unexpected X11 support result for {key:?}"
            );
        }
        assert_ne!(
            linux_key_code(KeyboardKey::Num1),
            linux_key_code(KeyboardKey::Numpad1)
        );
        assert_ne!(
            linux_key_code(KeyboardKey::Enter),
            linux_key_code(KeyboardKey::NumpadEnter)
        );
        assert_eq!(linux_key_code(KeyboardKey::CapsLock), Some(KEY_CAPSLOCK));
        assert_eq!(linux_key_code(KeyboardKey::Application), Some(KEY_MENU));
        assert_eq!(
            linux_key_code(KeyboardKey::MediaPlayPause),
            Some(KEY_PLAYPAUSE)
        );
        assert_eq!(linux_key_code(KeyboardKey::IntlBackslash), Some(KEY_102ND));
        assert_ne!(
            x11_key_name(KeyboardKey::Num1),
            x11_key_name(KeyboardKey::Numpad1)
        );
    }

    #[test]
    fn expanded_windows_keys_preserve_physical_identity() {
        for wire_id in 0..KeyboardKey::COUNT as u8 {
            let key = KeyboardKey::from_u8(wire_id).unwrap();
            assert!(
                windows_key_virtual_key(key).is_some() || windows_key_scan_code(key).is_some(),
                "missing Windows key {key:?}"
            );
        }
        assert_ne!(
            windows_key_scan_code(KeyboardKey::Num1),
            windows_key_scan_code(KeyboardKey::Numpad1)
        );
        assert_ne!(
            windows_key_scan_code(KeyboardKey::Enter),
            windows_key_scan_code(KeyboardKey::NumpadEnter)
        );
        assert_eq!(windows_key_virtual_key(KeyboardKey::Pause), Some(0x13));
        assert_eq!(windows_key_scan_code(KeyboardKey::Pause), None);
        assert_eq!(windows_key_virtual_key(KeyboardKey::NumpadEquals), None);
        assert_eq!(windows_key_virtual_key(KeyboardKey::NumpadComma), None);
        assert_eq!(
            windows_key_scan_code(KeyboardKey::NumpadEquals),
            Some((0x59, false))
        );
        assert_eq!(
            windows_key_scan_code(KeyboardKey::NumpadComma),
            Some((0x7E, false))
        );
        assert_eq!(
            windows_key_scan_code(KeyboardKey::MediaPlayPause),
            Some((0x22, true))
        );
    }

    #[test]
    fn macos_key_mapping_reports_unsupported_keys_without_aliasing() {
        let unsupported = [
            KeyboardKey::NumLock,
            KeyboardKey::ScrollLock,
            KeyboardKey::PrintScreen,
            KeyboardKey::Pause,
            KeyboardKey::F21,
            KeyboardKey::F22,
            KeyboardKey::F23,
            KeyboardKey::F24,
            KeyboardKey::MediaPrevious,
            KeyboardKey::MediaNext,
            KeyboardKey::MediaPlayPause,
            KeyboardKey::MediaStop,
        ];
        for wire_id in 0..KeyboardKey::COUNT as u8 {
            let key = KeyboardKey::from_u8(wire_id).unwrap();
            assert_eq!(
                macos_key_code(key).is_none(),
                unsupported.contains(&key),
                "unexpected macOS support result for {key:?}"
            );
        }
        assert_ne!(
            macos_key_code(KeyboardKey::Num1),
            macos_key_code(KeyboardKey::Numpad1)
        );
        assert_ne!(
            macos_key_code(KeyboardKey::Enter),
            macos_key_code(KeyboardKey::NumpadEnter)
        );
    }

    #[test]
    fn cursor_prediction_updates_relative_position_without_capture_frame() {
        let mut inner = predictive_cursor_inner();

        inner.predict_cursor_relative(10, -5);

        assert_eq!(inner.cursor_state.x, 20);
        assert_eq!(inner.cursor_state.y, 15);
        assert_eq!(inner.cursor_state_version, 2);
    }

    #[test]
    fn cursor_prediction_updates_absolute_position_without_capture_frame() {
        let mut inner = predictive_cursor_inner();

        inner.predict_cursor_absolute(u16::MAX, 0);

        assert_eq!(inner.cursor_state.x, 1916);
        assert_eq!(inner.cursor_state.y, -5);
        assert_eq!(inner.cursor_state_version, 2);
    }

    #[test]
    fn cursor_prediction_does_not_make_hidden_cursor_visible() {
        let mut inner = predictive_cursor_inner();
        inner.cursor_state.visible = false;

        inner.predict_cursor_relative(10, 10);

        assert_eq!(inner.cursor_state.x, 10);
        assert_eq!(inner.cursor_state.y, 20);
        assert_eq!(inner.cursor_state_version, 1);
    }
}
