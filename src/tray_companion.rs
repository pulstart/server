//! User-session tray companion for the system-service deployment.
//!
//! When st-server runs under systemd as the `st` system user (see
//! `packaging/linux/st-server.service`), it has no access to any logged-in
//! user's D-Bus session bus, so it cannot publish a StatusNotifierItem
//! icon. This module runs inside the user's session instead — it reads
//! the token from `$ST_STATE_DIR/st-server-config.json`, polls service
//! status via `systemctl is-active`, and shells out to `pkexec systemctl`
//! for start/stop/restart.
//!
//! Invoked with `st-server --tray`, launched automatically via the
//! `/etc/xdg/autostart/st-server-tray.desktop` entry on desktop login.

#![cfg(target_os = "linux")]

use ksni::blocking::TrayMethods as _;
use ksni::menu::{MenuItem as LinuxMenuItem, StandardItem as LinuxStandardItem};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::tray::{copy_to_clipboard, server_icon_rgba};

const SERVICE_UNIT: &str = "st-server.service";
const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Default, Deserialize)]
struct PersistedSettings {
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Clone)]
struct Snapshot {
    service_active: bool,
    token: Option<String>,
}

struct CompanionTray {
    state: Arc<Mutex<Snapshot>>,
}

impl ksni::Tray for CompanionTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "st-server-tray".into()
    }

    fn title(&self) -> String {
        "st-server".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let active = self.state.lock().unwrap().service_active;
        vec![linux_icon(active)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let snap = self.state.lock().unwrap().clone();
        ksni::ToolTip {
            title: "st-server".into(),
            description: tooltip_description(&snap),
            icon_name: String::new(),
            icon_pixmap: vec![],
        }
    }

    fn menu(&self) -> Vec<LinuxMenuItem<Self>> {
        let snap = self.state.lock().unwrap().clone();
        let status_label = if snap.service_active {
            "Server: running".to_string()
        } else {
            "Server: stopped".to_string()
        };

        let token_label = match snap.token.as_deref() {
            Some(t) if !t.is_empty() => {
                let display = if t.len() > 10 { &t[..10] } else { t };
                format!("Copy token ({display}…)")
            }
            _ => "Copy token (unavailable)".to_string(),
        };
        let token_enabled = snap.token.as_deref().map(|t| !t.is_empty()).unwrap_or(false);

        vec![
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: status_label,
                enabled: false,
                ..Default::default()
            }),
            LinuxMenuItem::Separator,
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: token_label,
                enabled: token_enabled,
                activate: Box::new(|tray: &mut Self| {
                    if let Some(ref tok) = tray.state.lock().unwrap().token {
                        copy_to_clipboard(tok);
                    }
                }),
                ..Default::default()
            }),
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Open logs".into(),
                activate: Box::new(|_| open_logs()),
                ..Default::default()
            }),
            LinuxMenuItem::Separator,
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Start server".into(),
                enabled: !snap.service_active,
                activate: Box::new(|_| pkexec_systemctl("start")),
                ..Default::default()
            }),
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Stop server".into(),
                enabled: snap.service_active,
                activate: Box::new(|_| pkexec_systemctl("stop")),
                ..Default::default()
            }),
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Restart server".into(),
                enabled: snap.service_active,
                activate: Box::new(|_| pkexec_systemctl("restart")),
                ..Default::default()
            }),
            LinuxMenuItem::Separator,
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    // Match the old in-process tray's "Quit" behavior: tearing
                    // down the tray also tears down streaming. If the service
                    // is already stopped we skip the pkexec prompt.
                    let active = tray.state.lock().unwrap().service_active;
                    if active {
                        pkexec_systemctl("stop");
                    }
                    std::process::exit(0);
                }),
                ..Default::default()
            }),
        ]
    }
}

fn tooltip_description(snap: &Snapshot) -> String {
    let mut lines = Vec::new();
    lines.push(if snap.service_active {
        "Running".to_string()
    } else {
        "Stopped".to_string()
    });
    if let Some(ref t) = snap.token {
        if !t.is_empty() {
            let display = if t.len() > 10 { &t[..10] } else { t.as_str() };
            lines.push(format!("token {display}…"));
        }
    }
    lines.join("\n")
}

fn linux_icon(active: bool) -> ksni::Icon {
    let (mut rgba, width, height) = server_icon_rgba(active);
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: rgba,
    }
}

fn config_path() -> PathBuf {
    // Mirrors server_control::config_path() precedence, but the companion
    // never creates directories — it just reads.
    if let Some(dir) = std::env::var_os("ST_STATE_DIR") {
        return PathBuf::from(dir).join("st-server-config.json");
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("st").join("st-server-config.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("st")
            .join("st-server-config.json");
    }
    // Last-ditch default matching the system service layout.
    PathBuf::from("/var/lib/st-server/st-server-config.json")
}

fn read_token() -> Option<String> {
    // Try the primary path, then the system-service default as a fallback
    // so the autostart entry works even when ST_STATE_DIR is not set in
    // the user's shell env.
    let mut paths = vec![config_path()];
    let system_default = PathBuf::from("/var/lib/st-server/st-server-config.json");
    if !paths.contains(&system_default) {
        paths.push(system_default);
    }
    for path in paths {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<PersistedSettings>(&contents) {
                if let Some(tok) = parsed.token {
                    if !tok.is_empty() {
                        return Some(tok);
                    }
                }
            }
        }
    }
    None
}

fn service_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE_UNIT])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pkexec_systemctl(action: &str) {
    // pkexec will pop the polkit prompt on the user's session; the install
    // script adds the invoking user to the `st` group, so no extra polkit
    // rule is required for read operations — writes still ask for admin.
    let result = Command::new("pkexec")
        .args(["systemctl", action, SERVICE_UNIT])
        .status();
    if let Err(err) = result {
        eprintln!("[tray-companion] failed to run pkexec systemctl {action}: {err}");
    }
}

fn open_logs() {
    // Try the user's preferred terminal; fall back to anything reasonable.
    let candidates = [
        ("konsole", vec!["-e", "journalctl", "-u", SERVICE_UNIT, "-f"]),
        (
            "gnome-terminal",
            vec!["--", "journalctl", "-u", SERVICE_UNIT, "-f"],
        ),
        ("xterm", vec!["-e", "journalctl", "-u", SERVICE_UNIT, "-f"]),
    ];
    for (cmd, args) in &candidates {
        if Command::new(cmd).args(args).spawn().is_ok() {
            return;
        }
    }
    eprintln!(
        "[tray-companion] no supported terminal found; run: journalctl -u {SERVICE_UNIT} -f"
    );
}

/// Entry point invoked from `main.rs` when `--tray` is passed.
pub fn run() -> Result<(), String> {
    let state = Arc::new(Mutex::new(Snapshot {
        service_active: service_active(),
        token: read_token(),
    }));

    let tray = CompanionTray {
        state: Arc::clone(&state),
    };

    let handle = tray
        .spawn()
        .map_err(|err| format!("tray register: {err}"))?;

    // Poll thread — refresh status + token every POLL_INTERVAL and nudge
    // the tray so any open menu picks up the new state next time.
    let poll_state = Arc::clone(&state);
    thread::spawn(move || loop {
        thread::sleep(POLL_INTERVAL);
        let fresh = Snapshot {
            service_active: service_active(),
            token: read_token(),
        };
        *poll_state.lock().unwrap() = fresh;
    });

    // Drive the menu + icon refresh on the same interval.
    loop {
        thread::sleep(POLL_INTERVAL);
        handle.update(|_| {});
    }
}
