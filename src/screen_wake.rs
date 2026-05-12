//! Best-effort screen wake fired right before a fresh shared pipeline starts.
//!
//! Why this exists: on Wayland sessions (PipeWire portal, wlroots screencopy)
//! and on KMS, the capture backend only produces frames while the compositor /
//! kernel is actually scanning out. When the monitor is in DPMS standby and a
//! client connects, the pipeline's first-frame wait at `frame_rx.recv()` in
//! `run_shared_pipeline` stalls until the 30s `SharedPipeline::start` timeout
//! fires, and the client sees `Pipeline thread crashed`. Native X11 sessions
//! aren't affected (the X server keeps a software root pixmap), but we still
//! reset the screensaver there so the user's monitor blanks back on for them.
//!
//! Method order on Linux:
//!   1. X11 — `DPMSForceLevel(DPMSModeOn)` + `XForceScreenSaver(Reset)` via
//!      the libX11/libXext bindings already linked by `build.rs`.
//!   2. D-Bus `org.freedesktop.ScreenSaver.SimulateUserActivity` — answered by
//!      GNOME, KDE, MATE, Cinnamon's session screensaver services.
//!   3. Compositor CLIs — `hyprctl dispatch dpms on`, `swaymsg output * power on`,
//!      `wlopm --on '*'`, `kscreen-doctor output.*.enable` — covers wlroots
//!      stacks where D-Bus SimulateUserActivity is ignored.
//!
//! Default-on. Set `ST_WAKE_ON_CONNECT=0` (also `false`/`no`/`off`) to disable
//! — useful if a specific compositor's `SimulateUserActivity` handler misbehaves
//! and you'd rather take the 30s first-frame stall.

pub fn wake_display() {
    if !enabled() {
        return;
    }
    #[cfg(target_os = "linux")]
    linux::wake();
}

fn enabled() -> bool {
    match std::env::var("ST_WAKE_ON_CONNECT").ok().as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => true,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::{Command, Stdio};

    pub(super) fn wake() {
        let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || matches!(
                std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
                Some("wayland")
            );
        let mut tried: Vec<String> = Vec::new();

        // On Wayland, XWayland's DISPLAY is usually set too, but its DPMS state
        // is decoupled from the real Wayland outputs — forcing XWayland DPMS on
        // doesn't unblank anything. Try compositor-native paths first.
        if is_wayland {
            match dbus_simulate_user_activity() {
                Ok(()) => {
                    eprintln!("[wake] D-Bus SimulateUserActivity delivered");
                    return;
                }
                Err(e) => tried.push(format!("dbus: {e}")),
            }
            match compositor_cli_wake() {
                Ok(label) => {
                    eprintln!("[wake] {label}");
                    return;
                }
                Err(e) => tried.push(format!("cli: {e}")),
            }
        }

        // Native X11 (or last-resort fallback if Wayland paths all failed).
        if std::env::var_os("DISPLAY").is_some() {
            match x11_wake() {
                Ok(()) => {
                    eprintln!("[wake] X11 DPMS forced on / screensaver reset");
                    return;
                }
                Err(e) => tried.push(format!("x11: {e}")),
            }
        }

        if !tried.is_empty() {
            eprintln!("[wake] no method succeeded: {}", tried.join("; "));
        }
    }

    fn x11_wake() -> Result<(), String> {
        let display = unsafe { ffi::XOpenDisplay(std::ptr::null()) };
        if display.is_null() {
            return Err("XOpenDisplay returned null".into());
        }

        let mut event_base: std::os::raw::c_int = 0;
        let mut error_base: std::os::raw::c_int = 0;
        let has_dpms =
            unsafe { ffi::DPMSQueryExtension(display, &mut event_base, &mut error_base) } != 0;

        unsafe {
            if has_dpms {
                // DPMSEnable is required before DPMSForceLevel is honored on
                // most X servers — calling Force on a disabled DPMS extension
                // raises BadMatch.
                ffi::DPMSEnable(display);
                ffi::DPMSForceLevel(display, ffi::DPMS_MODE_ON);
            }
            ffi::XForceScreenSaver(display, ffi::SCREEN_SAVER_RESET);
            ffi::XSync(display, 0);
            ffi::XCloseDisplay(display);
        }
        if has_dpms {
            Ok(())
        } else {
            // ScreenSaverReset still went through; report it so the caller
            // doesn't try noisier fallbacks.
            Err("DPMS extension missing; only screensaver reset issued".into())
        }
    }

    fn dbus_simulate_user_activity() -> Result<(), String> {
        // Use dbus-send if present — pulls in no async runtime and lets us
        // stay synchronous. Most desktop Linux distros ship it via dbus-tools.
        for bin in ["dbus-send", "gdbus", "busctl"] {
            let cmd = build_dbus_cmd(bin);
            let output = match cmd {
                Some(mut c) => c
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .output()
                    .ok(),
                None => continue,
            };
            let Some(output) = output else { continue };
            if output.status.success() {
                return Ok(());
            }
            // Fall through to the next tool on failure (e.g. no session bus,
            // service unavailable on this DE).
        }
        Err("no D-Bus client (dbus-send/gdbus/busctl) succeeded".into())
    }

    fn build_dbus_cmd(bin: &str) -> Option<Command> {
        let mut c = Command::new(bin);
        match bin {
            "dbus-send" => {
                c.args([
                    "--session",
                    "--type=method_call",
                    "--dest=org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    "org.freedesktop.ScreenSaver.SimulateUserActivity",
                ]);
            }
            "gdbus" => {
                c.args([
                    "call",
                    "--session",
                    "--dest",
                    "org.freedesktop.ScreenSaver",
                    "--object-path",
                    "/org/freedesktop/ScreenSaver",
                    "--method",
                    "org.freedesktop.ScreenSaver.SimulateUserActivity",
                ]);
            }
            "busctl" => {
                c.args([
                    "--user",
                    "call",
                    "org.freedesktop.ScreenSaver",
                    "/org/freedesktop/ScreenSaver",
                    "org.freedesktop.ScreenSaver",
                    "SimulateUserActivity",
                ]);
            }
            _ => return None,
        }
        Some(c)
    }

    fn compositor_cli_wake() -> Result<String, String> {
        let attempts: &[(&str, &[&str])] = &[
            ("hyprctl dispatch dpms on", &["hyprctl", "dispatch", "dpms", "on"]),
            (
                "swaymsg output * power on",
                &["swaymsg", "output", "*", "power", "on"],
            ),
            ("wlopm --on '*'", &["wlopm", "--on", "*"]),
            (
                "kscreen-doctor output.1.enable",
                &["kscreen-doctor", "output.1.enable"],
            ),
        ];
        let mut errs = Vec::new();
        for (label, argv) in attempts {
            let (program, rest) = argv.split_first().unwrap();
            let status = Command::new(program)
                .args(rest)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() => return Ok((*label).to_string()),
                Ok(s) => errs.push(format!("{label}: exit {s}")),
                Err(_) => {} // tool not installed — silently skip
            }
        }
        if errs.is_empty() {
            Err("no compositor CLI tool available".into())
        } else {
            Err(errs.join("; "))
        }
    }

    #[allow(non_snake_case, non_camel_case_types)]
    mod ffi {
        use std::ffi::c_void;
        use std::os::raw::{c_char, c_int};

        pub type Display = c_void;

        // X.h: ScreenSaverReset / ScreenSaverActive
        pub const SCREEN_SAVER_RESET: c_int = 0;
        // dpms.h: DPMSModeOn / Standby / Suspend / Off — On is 0.
        pub const DPMS_MODE_ON: u16 = 0;

        extern "C" {
            pub fn XOpenDisplay(name: *const c_char) -> *mut Display;
            pub fn XCloseDisplay(display: *mut Display) -> c_int;
            pub fn XSync(display: *mut Display, discard: c_int) -> c_int;
            pub fn XForceScreenSaver(display: *mut Display, mode: c_int) -> c_int;

            // libXext: DPMS extension. Returns Status (1 == ok); QueryExtension
            // tells us whether the server supports it at all, so we can skip
            // Enable/Force calls that would otherwise print "extension missing"
            // warnings to stderr.
            pub fn DPMSQueryExtension(
                display: *mut Display,
                event_base: *mut c_int,
                error_base: *mut c_int,
            ) -> c_int;
            pub fn DPMSEnable(display: *mut Display) -> c_int;
            pub fn DPMSForceLevel(display: *mut Display, level: u16) -> c_int;
        }
    }
}
