use crate::api_client::ApiTunnelState;
use crate::encode_config::{Codec, QualityPreset};
use crate::server_control::{ConnectedClientSnapshot, ServerControl, UpdateStateSnapshot};
use crate::updater;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use ksni::blocking::TrayMethods as _;
#[cfg(target_os = "linux")]
use ksni::menu::{
    CheckmarkItem as LinuxCheckmarkItem, MenuItem as LinuxMenuItem,
    StandardItem as LinuxStandardItem, SubMenu as LinuxSubMenu,
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::time::Instant;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu, SubmenuBuilder,
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::{Icon as DesktopTrayIcon, TrayIcon, TrayIconBuilder};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use winit::application::ApplicationHandler;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(target_os = "windows")]
use windows::core::{w, Error as WindowsError, PCWSTR};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    GetStockObject, GetSysColorBrush, UpdateWindow, COLOR_3DFACE, DEFAULT_GUI_FONT,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BN_CLICKED, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW,
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, ES_LEFT,
    GWLP_USERDATA, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, HMENU, IDC_ARROW, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TranslateMessage, WNDCLASSW,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_NCCREATE, WM_SETFONT, WS_BORDER,
    WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_EX_TOPMOST, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE, CW_USEDEFAULT, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE,
};

#[cfg(any(target_os = "macos", target_os = "windows"))]
const ALLOW_CONNECTIONS_ID: &str = "allow-connections";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const CHECK_UPDATES_ID: &str = "check-updates";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const INSTALL_UPDATE_ID: &str = "install-update";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const QUIT_ID: &str = "quit";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const COPY_TOKEN_ID: &str = "copy-token";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const SET_TOKEN_ID: &str = "set-token";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const DROP_CLIENT_ID_PREFIX: &str = "drop-client:";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const VIDEO_CODEC_PREFIX: &str = "video-codec:";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const VIDEO_BITRATE_PREFIX: &str = "video-bitrate:";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const VIDEO_QUALITY_PREFIX: &str = "video-quality:";
#[cfg(target_os = "windows")]
const WINDOWS_TOKEN_DIALOG_OK_ID: usize = 1;
#[cfg(target_os = "windows")]
const WINDOWS_TOKEN_DIALOG_CANCEL_ID: usize = 2;
#[cfg(target_os = "windows")]
const WINDOWS_TOKEN_DIALOG_EDIT_ID: usize = 100;
#[cfg(target_os = "windows")]
static WINDOWS_TOKEN_DIALOG_CLASS: OnceLock<Result<(), String>> = OnceLock::new();

pub fn should_run_tray() -> bool {
    if std::env::var_os("ST_SERVER_NO_TRAY").is_some() {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    #[cfg(target_os = "macos")]
    {
        true
    }

    #[cfg(target_os = "windows")]
    {
        true
    }
}

pub fn run_tray(control: Arc<ServerControl>, tunnel_state: Option<Arc<ApiTunnelState>>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        run_linux_tray(control, tunnel_state)
    }

    #[cfg(target_os = "macos")]
    {
        run_desktop_tray(control, tunnel_state)
    }

    #[cfg(target_os = "windows")]
    {
        run_desktop_tray(control, tunnel_state)
    }
}

#[cfg(target_os = "linux")]
fn run_linux_tray(control: Arc<ServerControl>, tunnel_state: Option<Arc<ApiTunnelState>>) -> Result<(), String> {
    let mut last_version = control.ui_version();
    let mut last_api_connected = tunnel_state.as_ref().map(|ts| ts.is_connected());
    let handle = LinuxTray {
        control: Arc::clone(&control),
        tunnel_state: tunnel_state.clone(),
    }
    .assume_sni_available(true)
    .spawn()
    .map_err(|err| format!("Failed to create Linux tray: {err}"))?;

    while !control.shutdown_requested() && !handle.is_closed() {
        let version = control.ui_version();
        let api_connected = tunnel_state.as_ref().map(|ts| ts.is_connected());
        if version != last_version || api_connected != last_api_connected {
            last_version = version;
            last_api_connected = api_connected;
            let _ = handle.update(|_| {});
        }
        thread::sleep(Duration::from_millis(100));
    }

    handle.shutdown().wait();
    Ok(())
}

#[cfg(target_os = "linux")]
struct LinuxTray {
    control: Arc<ServerControl>,
    tunnel_state: Option<Arc<ApiTunnelState>>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for LinuxTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "st-server".into()
    }

    fn title(&self) -> String {
        tray_app_title()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let connected = !self.control.connected_clients().is_empty();
        vec![linux_icon(connected)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let connected = !self.control.connected_clients().is_empty();
        ksni::ToolTip {
            title: tray_app_title(),
            description: tray_tooltip_text(&self.control),
            icon_pixmap: vec![linux_icon(connected)],
            icon_name: String::new(),
        }
    }

    fn menu(&self) -> Vec<LinuxMenuItem<Self>> {
        let clients = self.control.connected_clients();
        let update_state = self.control.update_state();
        let api_status = match &self.tunnel_state {
            Some(ts) if ts.is_connected() => "API: Connected",
            Some(_) => "API: Disconnected",
            None => "API: Not configured",
        };
        vec![
            disabled_linux_item(tray_app_title()),
            disabled_linux_item(tray_status_text(&self.control)),
            disabled_linux_item(api_status.to_string()),
            LinuxStandardItem {
                label: format!("Token: {} (click to copy)", self.control.token()),
                activate: Box::new(|tray: &mut LinuxTray| {
                    copy_to_clipboard(&tray.control.token());
                }),
                ..Default::default()
            }
            .into(),
            LinuxStandardItem {
                label: "Set Token...".into(),
                activate: Box::new(|tray: &mut LinuxTray| {
                    let control = Arc::clone(&tray.control);
                    thread::spawn(move || {
                        if let Some(new_token) = show_token_input_dialog(&control.token()) {
                            if !new_token.is_empty() {
                                control.set_token(new_token);
                            }
                        }
                    });
                }),
                ..Default::default()
            }
            .into(),
            LinuxMenuItem::Separator,
            disabled_linux_item(tray_update_status_text(&update_state)),
            LinuxStandardItem {
                label: "Check For Updates".into(),
                enabled: update_check_enabled(&update_state),
                activate: Box::new(|tray: &mut LinuxTray| {
                    tray.control.begin_update_check();
                }),
                ..Default::default()
            }
            .into(),
            LinuxStandardItem {
                label: tray_install_update_label(&update_state),
                enabled: update_install_enabled(&update_state),
                activate: Box::new(|tray: &mut LinuxTray| {
                    tray.control.begin_update_install();
                }),
                ..Default::default()
            }
            .into(),
            LinuxMenuItem::Separator,
            LinuxCheckmarkItem {
                label: "Allow New Connections".into(),
                checked: self.control.allow_new_connections(),
                activate: Box::new(|tray: &mut LinuxTray| {
                    let next = !tray.control.allow_new_connections();
                    tray.control.set_allow_new_connections(next);
                }),
                ..Default::default()
            }
            .into(),
            LinuxMenuItem::Separator,
            LinuxSubMenu {
                label: "Video".into(),
                submenu: vec![
                    LinuxSubMenu {
                        label: format!("Codec: {}", codec_label(self.control.forced_codec())),
                        submenu: linux_codec_menu_items(&self.control),
                        ..Default::default()
                    }
                    .into(),
                    LinuxSubMenu {
                        label: format!("Bitrate: {}", bitrate_label(self.control.forced_bitrate_kbps())),
                        submenu: linux_bitrate_menu_items(&self.control),
                        ..Default::default()
                    }
                    .into(),
                    LinuxSubMenu {
                        label: format!("Quality: {}", quality_label(self.control.forced_quality())),
                        submenu: linux_quality_menu_items(&self.control),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            LinuxMenuItem::Separator,
            LinuxSubMenu {
                label: "Connected Clients".into(),
                submenu: linux_client_menu_items(&clients),
                ..Default::default()
            }
            .into(),
            LinuxMenuItem::Separator,
            LinuxStandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut LinuxTray| {
                    tray.control.request_shutdown();
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("[tray] Linux StatusNotifier watcher offline: {reason:?}");
        true
    }
}

#[cfg(target_os = "linux")]
fn disabled_linux_item(label: String) -> LinuxMenuItem<LinuxTray> {
    LinuxStandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

#[cfg(target_os = "linux")]
fn linux_client_menu_items(clients: &[ConnectedClientSnapshot]) -> Vec<LinuxMenuItem<LinuxTray>> {
    if clients.is_empty() {
        return vec![disabled_linux_item("No clients connected".into())];
    }

    clients
        .iter()
        .map(|client| {
            let client_id = client.id;
            LinuxStandardItem {
                label: format!("Disconnect {} ({})", client.addr, connected_since_label(client)),
                activate: Box::new(move |tray: &mut LinuxTray| {
                    let _ = tray.control.request_disconnect(client_id);
                }),
                ..Default::default()
            }
            .into()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_codec_menu_items(control: &Arc<ServerControl>) -> Vec<LinuxMenuItem<LinuxTray>> {
    let current = control.forced_codec();
    let options: [(Option<Codec>, &str); 4] = [
        (None, "Best Available (Default)"),
        (Some(Codec::H264), "H.264"),
        (Some(Codec::Hevc), "HEVC"),
        (Some(Codec::Av1), "AV1"),
    ];
    options
        .into_iter()
        .map(|(codec, label)| {
            LinuxCheckmarkItem {
                label: label.into(),
                checked: current == codec,
                activate: Box::new(move |tray: &mut LinuxTray| {
                    tray.control.set_forced_codec(codec);
                }),
                ..Default::default()
            }
            .into()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_bitrate_menu_items(control: &Arc<ServerControl>) -> Vec<LinuxMenuItem<LinuxTray>> {
    let current = control.forced_bitrate_kbps();
    let options: [(u32, &str); 11] = [
        (0, "Adaptive (Default)"),
        (1_000, "1 Mbps"),
        (5_000, "5 Mbps"),
        (10_000, "10 Mbps"),
        (20_000, "20 Mbps"),
        (30_000, "30 Mbps"),
        (50_000, "50 Mbps"),
        (80_000, "80 Mbps"),
        (100_000, "100 Mbps"),
        (150_000, "150 Mbps"),
        (200_000, "200 Mbps"),
    ];
    options
        .into_iter()
        .map(|(kbps, label)| {
            LinuxCheckmarkItem {
                label: label.into(),
                checked: current == kbps,
                activate: Box::new(move |tray: &mut LinuxTray| {
                    tray.control.set_forced_bitrate_kbps(kbps);
                }),
                ..Default::default()
            }
            .into()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_quality_menu_items(control: &Arc<ServerControl>) -> Vec<LinuxMenuItem<LinuxTray>> {
    let current = control.forced_quality();
    let options: [(Option<QualityPreset>, &str); 4] = [
        (None, "Balanced (Default)"),
        (Some(QualityPreset::LowLatency), "Low Latency"),
        (Some(QualityPreset::Balanced), "Balanced"),
        (Some(QualityPreset::HighQuality), "High Quality"),
    ];
    options
        .into_iter()
        .map(|(quality, label)| {
            LinuxCheckmarkItem {
                label: label.into(),
                checked: current == quality,
                activate: Box::new(move |tray: &mut LinuxTray| {
                    tray.control.set_forced_quality(quality);
                }),
                ..Default::default()
            }
            .into()
        })
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_desktop_tray(control: Arc<ServerControl>, _tunnel_state: Option<Arc<ApiTunnelState>>) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|err| format!("Failed to create tray event loop: {err}"))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(200)));
    let mut app = TrayApp::new(control);
    event_loop
        .run_app(&mut app)
        .map_err(|err| format!("Tray event loop failed: {err}"))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
struct TrayApp {
    control: Arc<ServerControl>,
    tray: Option<TrayIcon>,
    version_item: Option<MenuItem>,
    status_item: Option<MenuItem>,
    token_item: Option<MenuItem>,
    set_token_item: Option<MenuItem>,
    update_status_item: Option<MenuItem>,
    check_updates_item: Option<MenuItem>,
    install_update_item: Option<MenuItem>,
    allow_item: Option<CheckMenuItem>,
    clients_submenu: Option<Submenu>,
    client_items: Vec<MenuItem>,
    codec_submenu: Option<Submenu>,
    bitrate_submenu: Option<Submenu>,
    quality_submenu: Option<Submenu>,
    codec_items: Vec<CheckMenuItem>,
    bitrate_items: Vec<CheckMenuItem>,
    quality_items: Vec<CheckMenuItem>,
    last_version: usize,
    last_connected: bool,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl TrayApp {
    fn new(control: Arc<ServerControl>) -> Self {
        Self {
            control,
            tray: None,
            version_item: None,
            status_item: None,
            token_item: None,
            set_token_item: None,
            update_status_item: None,
            check_updates_item: None,
            install_update_item: None,
            allow_item: None,
            clients_submenu: None,
            client_items: Vec::new(),
            codec_submenu: None,
            bitrate_submenu: None,
            quality_submenu: None,
            codec_items: Vec::new(),
            bitrate_items: Vec::new(),
            quality_items: Vec::new(),
            last_version: 0,
            last_connected: false,
        }
    }

    fn init_tray(&mut self) -> Result<(), String> {
        if self.tray.is_some() {
            return Ok(());
        }

        let version_item = MenuItem::new(tray_app_title(), false, None);
        let status_item = MenuItem::new("Ready: no connected clients", false, None);
        let token_item = MenuItem::with_id(
            COPY_TOKEN_ID,
            format!("Token: {} (click to copy)", self.control.token()),
            true,
            None,
        );
        let set_token_item = MenuItem::with_id(SET_TOKEN_ID, "Set Token...", true, None);
        let update_status_item = MenuItem::new("Checking GitHub releases...", false, None);
        let check_updates_item = MenuItem::with_id(CHECK_UPDATES_ID, "Check For Updates", true, None);
        let install_update_item =
            MenuItem::with_id(INSTALL_UPDATE_ID, "Update To Latest", false, None);
        let allow_item = CheckMenuItem::with_id(
            ALLOW_CONNECTIONS_ID,
            "Allow New Connections",
            true,
            self.control.allow_new_connections(),
            None,
        );
        let clients_submenu = SubmenuBuilder::new()
            .text("Connected Clients")
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build clients submenu: {err}"))?;
        let quit_item = MenuItem::with_id(QUIT_ID, "Quit", true, None);

        // --- Video submenu ---
        let codec_options: [(Option<Codec>, &str); 4] = [
            (None, "Best Available (Default)"),
            (Some(Codec::H264), "H.264"),
            (Some(Codec::Hevc), "HEVC"),
            (Some(Codec::Av1), "AV1"),
        ];
        let current_codec = self.control.forced_codec();
        let codec_submenu = SubmenuBuilder::new()
            .text(format!("Codec: {}", codec_label(current_codec)))
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build codec submenu: {err}"))?;
        let mut codec_items = Vec::new();
        for (codec, label) in &codec_options {
            let id = format!("{VIDEO_CODEC_PREFIX}{}", codec.map_or("auto", |c| match c {
                Codec::H264 => "h264",
                Codec::Hevc => "hevc",
                Codec::Av1 => "av1",
            }));
            let item = CheckMenuItem::with_id(id, *label, true, current_codec == *codec, None);
            codec_submenu.append(&item).map_err(|err| format!("Failed to append codec item: {err}"))?;
            codec_items.push(item);
        }

        let bitrate_options: [(u32, &str); 11] = [
            (0, "Adaptive (Default)"),
            (1_000, "1 Mbps"),
            (5_000, "5 Mbps"),
            (10_000, "10 Mbps"),
            (20_000, "20 Mbps"),
            (30_000, "30 Mbps"),
            (50_000, "50 Mbps"),
            (80_000, "80 Mbps"),
            (100_000, "100 Mbps"),
            (150_000, "150 Mbps"),
            (200_000, "200 Mbps"),
        ];
        let current_bitrate = self.control.forced_bitrate_kbps();
        let bitrate_submenu = SubmenuBuilder::new()
            .text(format!("Bitrate: {}", bitrate_label(current_bitrate)))
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build bitrate submenu: {err}"))?;
        let mut bitrate_items = Vec::new();
        for (kbps, label) in &bitrate_options {
            let id = format!("{VIDEO_BITRATE_PREFIX}{kbps}");
            let item = CheckMenuItem::with_id(id, *label, true, current_bitrate == *kbps, None);
            bitrate_submenu.append(&item).map_err(|err| format!("Failed to append bitrate item: {err}"))?;
            bitrate_items.push(item);
        }

        let quality_options: [(Option<QualityPreset>, &str); 4] = [
            (None, "Balanced (Default)"),
            (Some(QualityPreset::LowLatency), "Low Latency"),
            (Some(QualityPreset::Balanced), "Balanced"),
            (Some(QualityPreset::HighQuality), "High Quality"),
        ];
        let current_quality = self.control.forced_quality();
        let quality_submenu = SubmenuBuilder::new()
            .text(format!("Quality: {}", quality_label(current_quality)))
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build quality submenu: {err}"))?;
        let mut quality_items = Vec::new();
        for (quality, label) in &quality_options {
            let id = format!("{VIDEO_QUALITY_PREFIX}{}", quality.map_or("auto", |q| match q {
                QualityPreset::LowLatency => "low-latency",
                QualityPreset::Balanced => "balanced",
                QualityPreset::HighQuality => "high-quality",
            }));
            let item = CheckMenuItem::with_id(id, *label, true, current_quality == *quality, None);
            quality_submenu.append(&item).map_err(|err| format!("Failed to append quality item: {err}"))?;
            quality_items.push(item);
        }

        let video_submenu = SubmenuBuilder::new()
            .text("Video")
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build video submenu: {err}"))?;
        video_submenu.append(&codec_submenu).map_err(|err| format!("Failed to append codec submenu: {err}"))?;
        video_submenu.append(&bitrate_submenu).map_err(|err| format!("Failed to append bitrate submenu: {err}"))?;
        video_submenu.append(&quality_submenu).map_err(|err| format!("Failed to append quality submenu: {err}"))?;

        // Use a real root menu for the tray popup. On Windows, attaching a
        // Submenu as the tray context menu goes through muda's submenu
        // subclass path, which is crash-prone when the popup activates.
        let root_menu = Menu::new();
        root_menu
            .append(&version_item)
            .map_err(|err| format!("Failed to append tray version item: {err}"))?;
        root_menu
            .append(&status_item)
            .map_err(|err| format!("Failed to append tray status item: {err}"))?;
        root_menu
            .append(&token_item)
            .map_err(|err| format!("Failed to append tray token item: {err}"))?;
        root_menu
            .append(&set_token_item)
            .map_err(|err| format!("Failed to append tray set-token item: {err}"))?;
        root_menu
            .append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("Failed to append tray separator: {err}"))?;
        root_menu
            .append(&update_status_item)
            .map_err(|err| format!("Failed to append tray update status item: {err}"))?;
        root_menu
            .append(&check_updates_item)
            .map_err(|err| format!("Failed to append tray check-updates item: {err}"))?;
        root_menu
            .append(&install_update_item)
            .map_err(|err| format!("Failed to append tray install-update item: {err}"))?;
        root_menu
            .append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("Failed to append tray separator: {err}"))?;
        root_menu
            .append(&allow_item)
            .map_err(|err| format!("Failed to append tray allow item: {err}"))?;
        root_menu
            .append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("Failed to append tray separator: {err}"))?;
        root_menu
            .append(&video_submenu)
            .map_err(|err| format!("Failed to append tray video submenu: {err}"))?;
        root_menu
            .append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("Failed to append tray separator: {err}"))?;
        root_menu
            .append(&clients_submenu)
            .map_err(|err| format!("Failed to append tray clients submenu: {err}"))?;
        root_menu
            .append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("Failed to append tray separator: {err}"))?;
        root_menu
            .append(&quit_item)
            .map_err(|err| format!("Failed to append tray quit item: {err}"))?;

        let builder = TrayIconBuilder::new()
            .with_tooltip("st-server")
            .with_title("st-server")
            .with_menu_on_left_click(false)
            .with_menu(Box::new(root_menu))
            .with_icon(desktop_tray_icon(false)?);

        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_as_template(false);

        let tray = builder
            .build()
            .map_err(|err| format!("Failed to create tray icon: {err}"))?;

        self.tray = Some(tray);
        self.version_item = Some(version_item);
        self.status_item = Some(status_item);
        self.token_item = Some(token_item);
        self.set_token_item = Some(set_token_item);
        self.update_status_item = Some(update_status_item);
        self.check_updates_item = Some(check_updates_item);
        self.install_update_item = Some(install_update_item);
        self.allow_item = Some(allow_item);
        self.clients_submenu = Some(clients_submenu);
        self.codec_submenu = Some(codec_submenu);
        self.bitrate_submenu = Some(bitrate_submenu);
        self.quality_submenu = Some(quality_submenu);
        self.codec_items = codec_items;
        self.bitrate_items = bitrate_items;
        self.quality_items = quality_items;
        self.sync_from_state()?;
        Ok(())
    }

    fn sync_from_state(&mut self) -> Result<(), String> {
        let version = self.control.ui_version();
        if version == self.last_version {
            return Ok(());
        }
        self.last_version = version;

        let clients = self.control.connected_clients();
        let update_state = self.control.update_state();
        let status_text = tray_status_text(&self.control);

        if let Some(version_item) = &self.version_item {
            version_item.set_text(tray_app_title());
        }
        if let Some(status_item) = &self.status_item {
            status_item.set_text(status_text.clone());
        }
        if let Some(token_item) = &self.token_item {
            token_item.set_text(format!("Token: {} (click to copy)", self.control.token()));
        }
        if let Some(update_status_item) = &self.update_status_item {
            update_status_item.set_text(tray_update_status_text(&update_state));
        }
        if let Some(check_updates_item) = &self.check_updates_item {
            check_updates_item.set_enabled(update_check_enabled(&update_state));
        }
        if let Some(install_update_item) = &self.install_update_item {
            install_update_item.set_text(tray_install_update_label(&update_state));
            install_update_item.set_enabled(update_install_enabled(&update_state));
        }
        if let Some(allow_item) = &self.allow_item {
            allow_item.set_checked(self.control.allow_new_connections());
        }
        let connected = !clients.is_empty();
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(tray_tooltip_text(&self.control)));
            if connected != self.last_connected {
                self.last_connected = connected;
                if let Ok(icon) = desktop_tray_icon(connected) {
                    let _ = tray.set_icon(Some(icon));
                }
            }
        }

        // Sync video option checkmarks and submenu labels
        let current_codec = self.control.forced_codec();
        if let Some(sub) = &self.codec_submenu {
            sub.set_text(format!("Codec: {}", codec_label(current_codec)));
        }
        let codec_values: [Option<Codec>; 4] = [None, Some(Codec::H264), Some(Codec::Hevc), Some(Codec::Av1)];
        for (item, value) in self.codec_items.iter().zip(codec_values.iter()) {
            item.set_checked(current_codec == *value);
        }
        let current_bitrate = self.control.forced_bitrate_kbps();
        if let Some(sub) = &self.bitrate_submenu {
            sub.set_text(format!("Bitrate: {}", bitrate_label(current_bitrate)));
        }
        let bitrate_values: [u32; 11] = [0, 1_000, 5_000, 10_000, 20_000, 30_000, 50_000, 80_000, 100_000, 150_000, 200_000];
        for (item, value) in self.bitrate_items.iter().zip(bitrate_values.iter()) {
            item.set_checked(current_bitrate == *value);
        }
        let current_quality = self.control.forced_quality();
        if let Some(sub) = &self.quality_submenu {
            sub.set_text(format!("Quality: {}", quality_label(current_quality)));
        }
        let quality_values: [Option<QualityPreset>; 4] = [
            None,
            Some(QualityPreset::LowLatency),
            Some(QualityPreset::Balanced),
            Some(QualityPreset::HighQuality),
        ];
        for (item, value) in self.quality_items.iter().zip(quality_values.iter()) {
            item.set_checked(current_quality == *value);
        }

        self.rebuild_clients_menu(&clients)
    }

    fn rebuild_clients_menu(&mut self, clients: &[ConnectedClientSnapshot]) -> Result<(), String> {
        let Some(clients_submenu) = &self.clients_submenu else {
            return Ok(());
        };

        while !clients_submenu.items().is_empty() {
            clients_submenu.remove_at(0);
        }
        self.client_items.clear();

        if clients.is_empty() {
            let empty_item = MenuItem::new("No clients connected", false, None);
            clients_submenu
                .append(&empty_item)
                .map_err(|err| format!("Failed to append empty-clients item: {err}"))?;
            self.client_items.push(empty_item);
            return Ok(());
        }

        for client in clients {
            let item = MenuItem::with_id(
                format!("{DROP_CLIENT_ID_PREFIX}{}", client.id),
                format!("Disconnect {} ({})", client.addr, connected_since_label(client)),
                true,
                None,
            );
            clients_submenu
                .append(&item)
                .map_err(|err| format!("Failed to append client tray item: {err}"))?;
            self.client_items.push(item);
        }

        Ok(())
    }

    fn handle_menu_events(&mut self) -> bool {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id.0.as_str();
            if id == ALLOW_CONNECTIONS_ID {
                let next = !self.control.allow_new_connections();
                self.control.set_allow_new_connections(next);
            } else if id == CHECK_UPDATES_ID {
                self.control.begin_update_check();
            } else if id == INSTALL_UPDATE_ID {
                self.control.begin_update_install();
            } else if id == COPY_TOKEN_ID {
                let token = self.control.token();
                std::thread::spawn(move || {
                    #[cfg(target_os = "windows")]
                    std::thread::sleep(Duration::from_millis(120));
                    copy_to_clipboard(&token);
                });
            } else if id == SET_TOKEN_ID {
                let control = Arc::clone(&self.control);
                std::thread::spawn(move || {
                    #[cfg(target_os = "windows")]
                    std::thread::sleep(Duration::from_millis(120));
                    if let Some(new_token) = show_desktop_token_input_dialog(&control.token()) {
                        if !new_token.is_empty() {
                            control.set_token(new_token);
                        }
                    }
                });
            } else if id == QUIT_ID {
                self.control.request_shutdown();
                return true;
            } else if let Some(client_id) = id.strip_prefix(DROP_CLIENT_ID_PREFIX) {
                if let Ok(client_id) = client_id.parse() {
                    let _ = self.control.request_disconnect(client_id);
                }
            } else if let Some(suffix) = id.strip_prefix(VIDEO_CODEC_PREFIX) {
                let codec = match suffix {
                    "h264" => Some(Codec::H264),
                    "hevc" => Some(Codec::Hevc),
                    "av1" => Some(Codec::Av1),
                    _ => None,
                };
                self.control.set_forced_codec(codec);
            } else if let Some(suffix) = id.strip_prefix(VIDEO_BITRATE_PREFIX) {
                if let Ok(kbps) = suffix.parse::<u32>() {
                    self.control.set_forced_bitrate_kbps(kbps);
                }
            } else if let Some(suffix) = id.strip_prefix(VIDEO_QUALITY_PREFIX) {
                let quality = match suffix {
                    "low-latency" => Some(QualityPreset::LowLatency),
                    "balanced" => Some(QualityPreset::Balanced),
                    "high-quality" => Some(QualityPreset::HighQuality),
                    _ => None,
                };
                self.control.set_forced_quality(quality);
            }
        }
        false
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl ApplicationHandler for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if let Err(err) = self.init_tray() {
            eprintln!("[tray] {err}");
            self.control.request_shutdown();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(200),
        ));
        if self.handle_menu_events() {
            event_loop.exit();
            return;
        }
        if let Err(err) = self.sync_from_state() {
            eprintln!("[tray] {err}");
        }
        if self.control.shutdown_requested() {
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }
}

fn codec_label(codec: Option<Codec>) -> &'static str {
    match codec {
        None => "Best Available",
        Some(Codec::H264) => "H.264",
        Some(Codec::Hevc) => "HEVC",
        Some(Codec::Av1) => "AV1",
    }
}

fn bitrate_label(kbps: u32) -> String {
    if kbps == 0 {
        "Adaptive".into()
    } else {
        format!("{} Mbps", kbps / 1_000)
    }
}

fn quality_label(quality: Option<QualityPreset>) -> &'static str {
    match quality {
        None => "Balanced",
        Some(q) => q.label(),
    }
}

fn tray_app_title() -> String {
    format!("st-server v{}", updater::current_version())
}

fn tray_tooltip_text(control: &ServerControl) -> String {
    format!(
        "{}\n{}",
        tray_status_text(control),
        tray_update_status_text(&control.update_state())
    )
}

fn tray_status_text(control: &ServerControl) -> String {
    let clients = control.connected_clients();
    let allow_connections = control.allow_new_connections();
    if clients.is_empty() {
        if allow_connections {
            "Ready: no connected clients".to_string()
        } else {
            "Blocking new connections".to_string()
        }
    } else {
        format!(
            "{} connected client{}{}",
            clients.len(),
            if clients.len() == 1 { "" } else { "s" },
            if allow_connections {
                ""
            } else {
                " • blocking new connections"
            }
        )
    }
}

fn tray_update_status_text(state: &UpdateStateSnapshot) -> String {
    match state {
        UpdateStateSnapshot::Unsupported(message) => format!("Updates unavailable: {}", trim_tray_label(message)),
        UpdateStateSnapshot::Idle => "Updates: ready to check GitHub releases".into(),
        UpdateStateSnapshot::Checking => "Updates: checking GitHub releases...".into(),
        UpdateStateSnapshot::UpToDate { version } => {
            format!("Updates: already on v{version}")
        }
        UpdateStateSnapshot::UpdateAvailable(release) => {
            format!("Updates: v{} is available", release.version)
        }
        UpdateStateSnapshot::Installing { version } => {
            format!("Updates: downloading and staging v{version}...")
        }
        UpdateStateSnapshot::ClosingForUpdate { version } => {
            format!("Updates: applying v{version} and restarting...")
        }
        UpdateStateSnapshot::Error(message) => {
            format!("Updates: {}", trim_tray_label(message))
        }
    }
}

fn tray_install_update_label(state: &UpdateStateSnapshot) -> String {
    match state {
        UpdateStateSnapshot::UpdateAvailable(release) => format!("Update To v{}", release.version),
        UpdateStateSnapshot::Installing { version } => format!("Installing v{version}..."),
        UpdateStateSnapshot::ClosingForUpdate { version } => format!("Applying v{version}..."),
        _ => "Install Latest Update".into(),
    }
}

fn update_check_enabled(state: &UpdateStateSnapshot) -> bool {
    !matches!(
        state,
        UpdateStateSnapshot::Unsupported(_)
            | UpdateStateSnapshot::Checking
            | UpdateStateSnapshot::Installing { .. }
            | UpdateStateSnapshot::ClosingForUpdate { .. }
    )
}

fn update_install_enabled(state: &UpdateStateSnapshot) -> bool {
    matches!(state, UpdateStateSnapshot::UpdateAvailable(_))
}

fn trim_tray_label(value: &str) -> String {
    const MAX_LEN: usize = 72;
    let mut trimmed = value.trim().replace('\n', " ");
    if trimmed.len() > MAX_LEN {
        trimmed.truncate(MAX_LEN.saturating_sub(3));
        trimmed.push_str("...");
    }
    trimmed
}

fn connected_since_label(client: &ConnectedClientSnapshot) -> String {
    let elapsed = client
        .connected_at
        .elapsed()
        .unwrap_or(Duration::ZERO)
        .as_secs();
    if elapsed >= 3600 {
        format!("{}h {}m", elapsed / 3600, (elapsed % 3600) / 60)
    } else if elapsed >= 60 {
        format!("{}m {}s", elapsed / 60, elapsed % 60)
    } else {
        format!("{elapsed}s")
    }
}

fn copy_to_clipboard(text: &str) {
    #[cfg(target_os = "windows")]
    const ATTEMPTS: usize = 8;
    #[cfg(not(target_os = "windows"))]
    const ATTEMPTS: usize = 1;

    for attempt in 0..ATTEMPTS {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(text) {
                Ok(()) => return,
                Err(err) => {
                    if attempt + 1 == ATTEMPTS {
                        eprintln!("[tray] Failed to copy to clipboard: {err}");
                        return;
                    }
                }
            },
            Err(err) => {
                if attempt + 1 == ATTEMPTS {
                    eprintln!("[tray] Failed to open clipboard: {err}");
                    return;
                }
            }
        }

        #[cfg(target_os = "windows")]
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(target_os = "linux")]
fn try_kdialog(current: &str) -> Result<Option<String>, String> {
    match std::process::Command::new("kdialog")
        .args([
            "--inputbox",
            "Enter new authentication token:",
            current,
            "--title",
            "Set Server Token",
        ])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
            } else {
                // Non-zero exit (e.g. user cancelled) is not an error
                Ok(None)
            }
        }
        Err(err) => Err(format!("kdialog not available: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn try_zenity(current: &str) -> Result<Option<String>, String> {
    match std::process::Command::new("zenity")
        .args([
            "--entry",
            "--title=Set Server Token",
            "--text=Enter new authentication token:",
            &format!("--entry-text={current}"),
        ])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
            } else {
                Ok(None)
            }
        }
        Err(err) => Err(format!("zenity not available: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn show_token_input_dialog(current: &str) -> Option<String> {
    // On KDE, prefer kdialog — it handles Wayland activation tokens natively.
    // zenity (GTK4/libadwaita) can fail to show its window on KDE Plasma Wayland
    // when launched from a background D-Bus callback (no XDG activation token).
    let prefer_kde = std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.contains("KDE"))
        .unwrap_or(false);

    let (first, second): (
        fn(&str) -> Result<Option<String>, String>,
        fn(&str) -> Result<Option<String>, String>,
    ) = if prefer_kde {
        (try_kdialog, try_zenity)
    } else {
        (try_zenity, try_kdialog)
    };

    match first(current) {
        Ok(result) => return result,
        Err(err) => eprintln!("[tray] {err}"),
    }
    match second(current) {
        Ok(result) => return result,
        Err(err) => eprintln!("[tray] {err}"),
    }
    eprintln!("[tray] No dialog tool found (tried zenity, kdialog)");
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn show_desktop_token_input_dialog(current: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return show_macos_token_input_dialog(current);
    }

    #[cfg(target_os = "windows")]
    {
        return show_windows_token_input_dialog(current);
    }
}

#[cfg(target_os = "macos")]
fn show_macos_token_input_dialog(current: &str) -> Option<String> {
    let escaped = current.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "set result to display dialog \"Enter new authentication token:\" \
         default answer \"{}\" with title \"Set Server Token\" \
         buttons {{\"Cancel\", \"OK\"}} default button \"OK\"\n\
         return text returned of result",
        escaped
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
struct WindowsTokenDialogState {
    edit: HWND,
    initial_text: Vec<u16>,
    result: Option<String>,
}

#[cfg(target_os = "windows")]
fn show_windows_token_input_dialog(current: &str) -> Option<String> {
    if let Err(err) = ensure_windows_token_dialog_class() {
        eprintln!("[tray] Failed to register Windows token dialog class: {err}");
        return None;
    }
    match run_windows_token_input_dialog(current) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("[tray] Windows token dialog failed: {err}");
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn ensure_windows_token_dialog_class() -> Result<(), String> {
    WINDOWS_TOKEN_DIALOG_CLASS
        .get_or_init(register_windows_token_dialog_class)
        .clone()
}

#[cfg(target_os = "windows")]
fn register_windows_token_dialog_class() -> Result<(), String> {
    let module = unsafe {
        GetModuleHandleW(None).map_err(|err| format!("GetModuleHandleW failed: {err}"))?
    };
    let cursor = unsafe {
        LoadCursorW(None, IDC_ARROW).map_err(|err| format!("LoadCursorW failed: {err}"))?
    };
    let window_class = WNDCLASSW {
        hCursor: cursor,
        hInstance: HINSTANCE(module.0),
        lpszClassName: w!("StServerTokenDialog"),
        lpfnWndProc: Some(windows_token_dialog_wndproc),
        hbrBackground: unsafe { GetSysColorBrush(COLOR_3DFACE) },
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 {
        return Err(format!("RegisterClassW failed: {}", WindowsError::from_win32()));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_token_input_dialog(current: &str) -> Result<Option<String>, String> {
    let module = unsafe {
        GetModuleHandleW(None).map_err(|err| format!("GetModuleHandleW failed: {err}"))?
    };
    let hinstance = HINSTANCE(module.0);
    let ex_style = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT | WS_EX_TOPMOST;
    let style = WS_CAPTION | WS_SYSMENU;
    let (width, height) = windows_token_dialog_outer_size(style, ex_style);
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let x = ((screen_width - width).max(0)) / 2;
    let y = ((screen_height - height).max(0)) / 2;
    let title = wide_null("Set Server Token");
    let state = Box::new(WindowsTokenDialogState {
        edit: HWND::default(),
        initial_text: wide_null(current),
        result: None,
    });
    let state_ptr = Box::into_raw(state);
    let hwnd = match unsafe {
        CreateWindowExW(
            ex_style,
            w!("StServerTokenDialog"),
            PCWSTR(title.as_ptr()),
            style,
            if x > 0 { x } else { CW_USEDEFAULT },
            if y > 0 { y } else { CW_USEDEFAULT },
            width,
            height,
            None,
            None,
            Some(hinstance),
            Some(state_ptr.cast()),
        )
    } {
        Ok(hwnd) => hwnd,
        Err(err) => {
            let _ = unsafe { Box::from_raw(state_ptr) };
            return Err(format!("CreateWindowExW failed: {err}"));
        }
    };

    unsafe {
        apply_windows_token_dialog_corner_style(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
    }

    let mut msg = MSG::default();
    loop {
        let status = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match status.0 {
            -1 => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                let mut state = unsafe { Box::from_raw(state_ptr) };
                state.result = None;
                return Err(format!("GetMessageW failed: {}", WindowsError::from_win32()));
            }
            0 => break,
            _ => {}
        }

        if msg.message == WM_KEYDOWN {
            if msg.wParam.0 as u16 == VK_RETURN.0 {
                let edit = unsafe { (*state_ptr).edit };
                if msg.hwnd == edit {
                    unsafe {
                        commit_windows_token_dialog(hwnd);
                    }
                    continue;
                }
            } else if msg.wParam.0 as u16 == VK_ESCAPE.0 {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                continue;
            }
        }

        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let mut state = unsafe { Box::from_raw(state_ptr) };
    Ok(state.result.take())
}

#[cfg(target_os = "windows")]
fn windows_token_dialog_outer_size(style: WINDOW_STYLE, ex_style: WINDOW_EX_STYLE) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 420,
        bottom: 132,
    };
    let _ = unsafe { AdjustWindowRectEx(&mut rect, style, false, ex_style) };
    (
        (rect.right - rect.left).max(420),
        (rect.bottom - rect.top).max(132),
    )
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn windows_dialog_control_id(id: usize) -> HMENU {
    HMENU(id as *mut std::ffi::c_void)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_token_dialog_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            if create.lpCreateParams.is_null() {
                return LRESULT(0);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            LRESULT(1)
        }
        WM_CREATE => match create_windows_token_dialog_controls(hwnd) {
            Ok(()) => LRESULT(0),
            Err(err) => {
                eprintln!("[tray] Failed to create Windows token dialog controls: {err}");
                LRESULT(-1)
            }
        },
        WM_COMMAND => {
            let control_id = wparam.0 & 0xffff;
            let notification = ((wparam.0 >> 16) & 0xffff) as u32;
            if control_id == WINDOWS_TOKEN_DIALOG_OK_ID && notification == BN_CLICKED {
                commit_windows_token_dialog(hwnd);
                return LRESULT(0);
            }
            if control_id == WINDOWS_TOKEN_DIALOG_CANCEL_ID && notification == BN_CLICKED {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(target_os = "windows")]
unsafe fn create_windows_token_dialog_controls(hwnd: HWND) -> Result<(), String> {
    let Some(state) = windows_token_dialog_state_mut(hwnd) else {
        return Err("dialog state missing".into());
    };
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let label_text = wide_null("Enter new authentication token:");
    let ok_text = wide_null("OK");
    let cancel_text = wide_null("Cancel");
    let label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        12,
        14,
        240,
        20,
        Some(hwnd),
        None,
        None,
        None,
    )
    .map_err(|err| format!("CreateWindowExW label failed: {err}"))?;
    let edit = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        PCWSTR(state.initial_text.as_ptr()),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_BORDER
            | WINDOW_STYLE((ES_LEFT | ES_AUTOHSCROLL) as u32),
        12,
        40,
        396,
        24,
        Some(hwnd),
        Some(windows_dialog_control_id(WINDOWS_TOKEN_DIALOG_EDIT_ID)),
        None,
        None,
    )
    .map_err(|err| format!("CreateWindowExW edit failed: {err}"))?;
    let ok_button = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(ok_text.as_ptr()),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        252,
        88,
        75,
        26,
        Some(hwnd),
        Some(windows_dialog_control_id(WINDOWS_TOKEN_DIALOG_OK_ID)),
        None,
        None,
    )
    .map_err(|err| format!("CreateWindowExW OK button failed: {err}"))?;
    let cancel_button = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(cancel_text.as_ptr()),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        333,
        88,
        75,
        26,
        Some(hwnd),
        Some(windows_dialog_control_id(WINDOWS_TOKEN_DIALOG_CANCEL_ID)),
        None,
        None,
    )
    .map_err(|err| format!("CreateWindowExW Cancel button failed: {err}"))?;

    for control in [label, edit, ok_button, cancel_button] {
        SendMessageW(
            control,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    state.edit = edit;
    let _ = SetFocus(Some(edit));
    Ok(())
}

#[cfg(target_os = "windows")]
unsafe fn windows_token_dialog_state_mut(hwnd: HWND) -> Option<&'static mut WindowsTokenDialogState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowsTokenDialogState;
    state_ptr.as_mut()
}

#[cfg(target_os = "windows")]
unsafe fn commit_windows_token_dialog(hwnd: HWND) {
    if let Some(state) = windows_token_dialog_state_mut(hwnd) {
        let text_len = GetWindowTextLengthW(state.edit).max(0) as usize;
        let mut text = vec![0u16; text_len + 1];
        let written = GetWindowTextW(state.edit, &mut text).max(0) as usize;
        text.truncate(written);
        state.result = Some(String::from_utf16_lossy(&text).trim().to_string());
    }
    let _ = DestroyWindow(hwnd);
}

#[cfg(target_os = "windows")]
unsafe fn apply_windows_token_dialog_corner_style(hwnd: HWND) {
    let preference = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &preference as *const _ as _,
        std::mem::size_of_val(&preference) as u32,
    );
}

/// Flat 2D monitor icon with status dot.
/// `connected`: green dot when true, dim gray dot when false.
fn server_icon_rgba(connected: bool) -> (Vec<u8>, u32, u32) {
    let width = 32u32;
    let height = 32u32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    let white: [u8; 4] = [240, 240, 240, 255];
    let light_gray: [u8; 4] = [180, 180, 180, 255];
    let dark: [u8; 4] = [60, 60, 60, 255];
    let screen: [u8; 4] = if connected {
        [220, 220, 220, 255]
    } else {
        [120, 120, 120, 255]
    };
    let status_dot: [u8; 4] = if connected {
        [80, 200, 100, 255]
    } else {
        [100, 100, 100, 255]
    };

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            let mut pixel = [0u8, 0u8, 0u8, 0u8];

            // Monitor frame (rounded rect): x 4..27, y 4..21
            if (4..=27).contains(&x) && (4..=21).contains(&y) {
                // Outer border
                pixel = white;
                // Inner screen area
                if (6..=25).contains(&x) && (6..=19).contains(&y) {
                    pixel = screen;
                }
            }

            // Stand neck: x 14..17, y 22..24
            if (14..=17).contains(&x) && (22..=24).contains(&y) {
                pixel = light_gray;
            }

            // Stand base: x 10..21, y 25..26
            if (10..=21).contains(&x) && (25..=26).contains(&y) {
                pixel = light_gray;
            }

            // Status dot: bottom-right of monitor, radius 3
            let dot_cx = 25i32;
            let dot_cy = 19i32;
            let ddx = x as i32 - dot_cx;
            let ddy = y as i32 - dot_cy;
            if ddx * ddx + ddy * ddy <= 9 {
                pixel = status_dot;
            }

            // Round the monitor corners
            if pixel == white {
                let corners = [
                    (4i32, 4i32), (4, 21), (27, 4), (27, 21),
                ];
                for (cx, cy) in corners {
                    if x as i32 == cx && y as i32 == cy {
                        pixel = [0, 0, 0, 0];
                    }
                }
            }

            // Dark outline on bottom of screen for depth
            if (6..=25).contains(&x) && y == 20 {
                pixel = dark;
            }

            rgba[idx..idx + 4].copy_from_slice(&pixel);
        }
    }

    (rgba, width, height)
}

#[cfg(target_os = "linux")]
fn linux_icon(connected: bool) -> ksni::Icon {
    let (mut rgba, width, height) = server_icon_rgba(connected);
    // ksni expects ARGB byte order
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: rgba,
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn desktop_tray_icon(connected: bool) -> Result<DesktopTrayIcon, String> {
    let (rgba, width, height) = server_icon_rgba(connected);
    DesktopTrayIcon::from_rgba(rgba, width, height)
        .map_err(|err| format!("Failed to build tray icon: {err}"))
}
