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
#[cfg(target_os = "macos")]
use std::time::Instant;
#[cfg(target_os = "macos")]
use tray_icon::menu::{
    CheckMenuItem, MenuEvent, MenuItem, PredefinedMenuItem, Submenu, SubmenuBuilder,
};
#[cfg(target_os = "macos")]
use tray_icon::{Icon as MacTrayIcon, TrayIcon, TrayIconBuilder};
#[cfg(target_os = "macos")]
use winit::application::ApplicationHandler;
#[cfg(target_os = "macos")]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

#[cfg(target_os = "macos")]
const ALLOW_CONNECTIONS_ID: &str = "allow-connections";
#[cfg(target_os = "macos")]
const CHECK_UPDATES_ID: &str = "check-updates";
#[cfg(target_os = "macos")]
const INSTALL_UPDATE_ID: &str = "install-update";
#[cfg(target_os = "macos")]
const QUIT_ID: &str = "quit";
#[cfg(target_os = "macos")]
const DROP_CLIENT_ID_PREFIX: &str = "drop-client:";

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
}

pub fn run_tray(control: Arc<ServerControl>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        run_linux_tray(control)
    }

    #[cfg(target_os = "macos")]
    {
        run_macos_tray(control)
    }
}

#[cfg(target_os = "linux")]
fn run_linux_tray(control: Arc<ServerControl>) -> Result<(), String> {
    let mut last_version = control.ui_version();
    let handle = LinuxTray {
        control: Arc::clone(&control),
        icon: server_icon(),
    }
    .assume_sni_available(true)
    .spawn()
    .map_err(|err| format!("Failed to create Linux tray: {err}"))?;

    while !control.shutdown_requested() && !handle.is_closed() {
        let version = control.ui_version();
        if version != last_version {
            last_version = version;
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
    icon: ksni::Icon,
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
        vec![self.icon.clone()]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: tray_app_title(),
            description: tray_tooltip_text(&self.control),
            icon_pixmap: vec![self.icon.clone()],
            icon_name: String::new(),
        }
    }

    fn menu(&self) -> Vec<LinuxMenuItem<Self>> {
        let clients = self.control.connected_clients();
        let update_state = self.control.update_state();
        vec![
            disabled_linux_item(tray_app_title()),
            disabled_linux_item(tray_status_text(&self.control)),
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

#[cfg(target_os = "macos")]
fn run_macos_tray(control: Arc<ServerControl>) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|err| format!("Failed to create tray event loop: {err}"))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(200)));
    let mut app = TrayApp::new(control);
    event_loop
        .run_app(&mut app)
        .map_err(|err| format!("Tray event loop failed: {err}"))
}

#[cfg(target_os = "macos")]
struct TrayApp {
    control: Arc<ServerControl>,
    tray: Option<TrayIcon>,
    version_item: Option<MenuItem>,
    status_item: Option<MenuItem>,
    update_status_item: Option<MenuItem>,
    check_updates_item: Option<MenuItem>,
    install_update_item: Option<MenuItem>,
    allow_item: Option<CheckMenuItem>,
    clients_submenu: Option<Submenu>,
    client_items: Vec<MenuItem>,
    last_version: usize,
}

#[cfg(target_os = "macos")]
impl TrayApp {
    fn new(control: Arc<ServerControl>) -> Self {
        Self {
            control,
            tray: None,
            version_item: None,
            status_item: None,
            update_status_item: None,
            check_updates_item: None,
            install_update_item: None,
            allow_item: None,
            clients_submenu: None,
            client_items: Vec::new(),
            last_version: 0,
        }
    }

    fn init_tray(&mut self) -> Result<(), String> {
        if self.tray.is_some() {
            return Ok(());
        }

        let version_item = MenuItem::new(tray_app_title(), false, None);
        let status_item = MenuItem::new("Ready: no connected clients", false, None);
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

        let root_menu = SubmenuBuilder::new()
            .text("st-server")
            .enabled(true)
            .build()
            .map_err(|err| format!("Failed to build tray menu: {err}"))?;
        root_menu
            .append(&version_item)
            .map_err(|err| format!("Failed to append tray version item: {err}"))?;
        root_menu
            .append(&status_item)
            .map_err(|err| format!("Failed to append tray status item: {err}"))?;
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
            .with_icon(server_icon()?);

        let builder = builder.with_icon_as_template(false);

        let tray = builder
            .build()
            .map_err(|err| format!("Failed to create tray icon: {err}"))?;

        self.tray = Some(tray);
        self.version_item = Some(version_item);
        self.status_item = Some(status_item);
        self.update_status_item = Some(update_status_item);
        self.check_updates_item = Some(check_updates_item);
        self.install_update_item = Some(install_update_item);
        self.allow_item = Some(allow_item);
        self.clients_submenu = Some(clients_submenu);
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
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(tray_tooltip_text(&self.control)));
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
            } else if id == QUIT_ID {
                self.control.request_shutdown();
                return true;
            } else if let Some(client_id) = id.strip_prefix(DROP_CLIENT_ID_PREFIX) {
                if let Ok(client_id) = client_id.parse() {
                    let _ = self.control.request_disconnect(client_id);
                }
            }
        }
        false
    }
}

#[cfg(target_os = "macos")]
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

fn server_icon_rgba() -> (Vec<u8>, u32, u32) {
    let width = 32u32;
    let height = 32u32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            let mut pixel = [0u8, 0u8, 0u8, 0u8];

            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            let dist2 = dx * dx + dy * dy;
            if dist2 <= 15 * 15 {
                pixel = [24, 28, 44, 255];
            }
            if (7..=24).contains(&x) && (8..=20).contains(&y) {
                pixel = [72, 163, 255, 255];
            }
            if (10..=21).contains(&x) && (11..=17).contains(&y) {
                pixel = [235, 244, 255, 255];
            }
            if (13..=18).contains(&x) && (21..=22).contains(&y) {
                pixel = [72, 163, 255, 255];
            }
            if (20..=25).contains(&x) && (21..=26).contains(&y) {
                let dot_dx = x as i32 - 22;
                let dot_dy = y as i32 - 23;
                if dot_dx * dot_dx + dot_dy * dot_dy <= 9 {
                    pixel = [56, 214, 118, 255];
                }
            }

            rgba[idx..idx + 4].copy_from_slice(&pixel);
        }
    }

    (rgba, width, height)
}

#[cfg(target_os = "linux")]
fn server_icon() -> ksni::Icon {
    let (mut rgba, width, height) = server_icon_rgba();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: rgba,
    }
}

#[cfg(target_os = "macos")]
fn server_icon() -> Result<MacTrayIcon, String> {
    let (rgba, width, height) = server_icon_rgba();
    MacTrayIcon::from_rgba(rgba, width, height)
        .map_err(|err| format!("Failed to build tray icon: {err}"))
}
