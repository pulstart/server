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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::tray::{copy_to_clipboard, server_icon_rgba};

const SERVICE_UNIT: &str = "st-server.service";
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const CONTROL_SOCKET: &str = "/run/st-server/control.sock";
const SESSION_PUSH_INTERVAL: Duration = Duration::from_secs(15);

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
            LinuxMenuItem::Standard(LinuxStandardItem {
                label: "Update to latest".into(),
                activate: Box::new(|_| install_latest_via_installer()),
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
                    clear_session_context();
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

fn install_latest_via_installer() {
    // Re-runs install.sh from the main branch. pkexec pops a polkit dialog
    // on the user's desktop session → the script runs as root → stops the
    // service → swaps /opt/st-server/ with the newest release → restarts.
    // This reuses the one tested path for upgrades instead of the broken
    // in-process auto-updater (which tries to write to a root-owned dir
    // from the unprivileged `st` service user and gets stuck on pkexec
    // having no polkit agent under systemd).
    let cmdline = "curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash";
    let result = Command::new("pkexec")
        .args(["bash", "-c", cmdline])
        .status();
    match result {
        Ok(status) if status.success() => {
            eprintln!("[tray-companion] installer ran successfully");
        }
        Ok(status) => {
            eprintln!(
                "[tray-companion] installer exited with {:?} (polkit cancelled?)",
                status.code()
            );
        }
        Err(err) => {
            eprintln!("[tray-companion] pkexec bash failed: {err}");
        }
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

// ---------------------------------------------------------------------------
// Session-context bridge: detect the user's audio endpoint + runtime dir,
// push them to the server over the control socket so the system service
// can bind the user's PulseAudio/PipeWire cookie and actually capture
// desktop audio. See server/src/session_bridge.rs for the server side.
// ---------------------------------------------------------------------------

fn user_uid() -> u32 {
    // SAFETY: getuid is always safe.
    unsafe { libc::getuid() }
}

fn username_for(uid: u32) -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| uid.to_string())
}

fn xdg_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", user_uid())))
}

fn read_pulse_cookie(runtime_dir: &Path) -> Option<String> {
    // libpulse cookie paths, in the order libpulse itself checks:
    //   $PULSE_COOKIE → $XDG_CONFIG_HOME/pulse/cookie → ~/.config/pulse/cookie
    //   → $XDG_RUNTIME_DIR/pulse/cookie → ~/.pulse-cookie
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("PULSE_COOKIE") {
        candidates.push(PathBuf::from(p));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".config/pulse/cookie"));
        candidates.push(home.join(".pulse-cookie"));
    }
    candidates.push(runtime_dir.join("pulse/cookie"));

    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if !bytes.is_empty() {
                let mut hex = String::with_capacity(bytes.len() * 2);
                for b in bytes {
                    hex.push_str(&format!("{b:02x}"));
                }
                return Some(hex);
            }
        }
    }
    None
}

fn detect_default_monitor() -> Option<String> {
    let output = Command::new("pactl").arg("info").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(sink) = line.strip_prefix("Default Sink: ") {
            return Some(format!("{}.monitor", sink.trim()));
        }
    }
    None
}

fn build_session_payload() -> Option<serde_json::Value> {
    let uid = user_uid();
    let runtime = xdg_runtime_dir();
    let pulse_socket = runtime.join("pulse/native");
    let pw_socket = runtime.join("pipewire-0");

    let audio = if pulse_socket.exists() {
        Some(serde_json::json!({
            "kind": "pulse",
            "server": format!("unix:{}", pulse_socket.display()),
            "monitor_source": detect_default_monitor(),
            "cookie_hex": read_pulse_cookie(&runtime),
        }))
    } else if pw_socket.exists() {
        // PipeWire's PulseAudio bridge exposes the same `pulse/native`
        // socket when `pipewire-pulse` is installed. Raw PipeWire without
        // the bridge isn't supported by our capture backend yet — log and
        // skip so we don't advertise broken audio.
        eprintln!(
            "[tray-companion] PipeWire detected but no Pulse bridge socket at {} — audio disabled",
            pulse_socket.display()
        );
        None
    } else {
        None
    };

    Some(serde_json::json!({
        "uid": uid,
        "username": username_for(uid),
        "xdg_runtime_dir": runtime.to_string_lossy(),
        "audio": audio,
        "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
        "x11_display": std::env::var("DISPLAY").ok(),
        "dbus_session_bus_address": std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
    }))
}

fn send_control(op: serde_json::Value) -> Result<(), String> {
    let mut sock = UnixStream::connect(CONTROL_SOCKET)
        .map_err(|e| format!("connect {CONTROL_SOCKET}: {e}"))?;
    let line = serde_json::to_string(&op).map_err(|e| format!("serialize: {e}"))?;
    sock.write_all(line.as_bytes())
        .and_then(|_| sock.write_all(b"\n"))
        .map_err(|e| format!("write: {e}"))?;
    sock.flush().ok();
    // Drain one reply so the server fully processes the message before we close.
    let mut reader = BufReader::new(sock);
    let mut buf = String::new();
    let _ = reader.read_line(&mut buf);
    Ok(())
}

fn push_session_context() {
    let Some(ctx) = build_session_payload() else {
        return;
    };
    let req = serde_json::json!({ "op": "set_session_context", "context": ctx });
    if let Err(err) = send_control(req) {
        // Server might be stopped or the socket group perms not granted
        // yet — log once-per-interval rather than spam every 15s. For a
        // v1 tray, eprintln! is fine (goes into the user journal).
        eprintln!("[tray-companion] push session context failed: {err}");
    }
}

fn clear_session_context() {
    let req = serde_json::json!({ "op": "clear_session_context" });
    if let Err(err) = send_control(req) {
        eprintln!("[tray-companion] clear session context failed: {err}");
    }
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

    // Push the user's session context (audio socket + cookie, runtime dir,
    // env) to the server on startup and then on a slow cadence so the
    // server rebinds if the daemon restarts.
    push_session_context();
    thread::spawn(move || loop {
        thread::sleep(SESSION_PUSH_INTERVAL);
        push_session_context();
    });

    // On SIGTERM / SIGINT — fired when the user logs out or quits the
    // tray — tell the server to drop the session context so it doesn't
    // keep a stale PA cookie around expecting a gone daemon.
    let _ = ctrlc_cleanup();

    // Drive the menu + icon refresh on the same interval.
    loop {
        thread::sleep(POLL_INTERVAL);
        handle.update(|_| {});
    }
}

fn ctrlc_cleanup() -> Result<(), String> {
    // Use libc::signal directly to avoid pulling in the ctrlc crate — the
    // tray doesn't need fancy handling, just a chance to flush the
    // session context on the way out.
    extern "C" fn handler(_: libc::c_int) {
        clear_session_context();
        std::process::exit(0);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
    Ok(())
}
