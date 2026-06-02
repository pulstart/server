//! Session-side "game mode" detector for the in-session tray agent.
//!
//! The root service can't see the user's compositor, so the per-user tray agent
//! asks the compositor which window is focused and whether it's fullscreen, and
//! pushes a "game mode" hint to the service over the control socket (mirrors the
//! screen-wake-via-tray pattern). The service ORs it into `CursorState.app_grab`
//! so the client enters relative (mouselook) capture — which is what makes
//! fullscreen games work even where the warp detector can't (e.g. NVIDIA, no
//! cursor-position readback).
//!
//! Signal: focused window is **fullscreen** AND its app-class is **not** a known
//! browser / video player (those go fullscreen for content you still want to
//! click). Backends: KWin (event-driven via a loaded KWin script that calls back
//! over D-Bus), Hyprland & Sway (polled via `hyprctl` / `swaymsg`). Other
//! compositors get no auto-detection (manual only).
//!
//! `ST_GAME_MODE=0`/`false`/`no`/`off` disables the whole detector. Extra
//! excluded classes (comma-separated, case-insensitive substring match) via
//! `ST_GAME_MODE_EXCLUDE`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Raw focused-window state reported by whichever compositor backend is active.
#[derive(Default, Clone)]
struct FocusState {
    fullscreen: bool,
    class: String,
}

/// Built-in app-class fragments that are fullscreen-capable but are NOT games —
/// browsers and video players you still want to click. Matched case-insensitively
/// as substrings of the window's resource class / app-id.
const DEFAULT_EXCLUDED: &[&str] = &[
    "firefox",
    "chrome",
    "chromium",
    "brave",
    "vivaldi",
    "opera",
    "msedge",
    "mpv",
    "vlc",
    "mplayer",
    "smplayer",
    "celluloid",
    "totem",
    "kodi",
    "plasmashell",
    "haruna",
    "dragonplayer",
];

/// True when game-mode auto-detection is enabled (`ST_GAME_MODE`, default on).
fn enabled() -> bool {
    !matches!(
        std::env::var("ST_GAME_MODE").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

fn excluded_classes() -> Vec<String> {
    let mut v: Vec<String> = DEFAULT_EXCLUDED.iter().map(|s| s.to_string()).collect();
    if let Ok(extra) = std::env::var("ST_GAME_MODE_EXCLUDE") {
        for c in extra.split(',') {
            let c = c.trim().to_ascii_lowercase();
            if !c.is_empty() {
                v.push(c);
            }
        }
    }
    v
}

/// Apply the fullscreen + class-filter rule to a focused-window state.
fn is_game(state: &FocusState, excluded: &[String]) -> bool {
    if !state.fullscreen {
        return false;
    }
    let cls = state.class.to_ascii_lowercase();
    !excluded.iter().any(|e| cls.contains(e.as_str()))
}

/// Running detector. Holds its worker thread alive for the process lifetime; the
/// `stop` flag lets poll loops exit promptly. The KWin script is left loaded (a
/// reload by the same plugin name replaces it; it harmlessly idles otherwise).
pub struct GameModeWatcher {
    stop: Arc<AtomicBool>,
}

impl Drop for GameModeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Start the detector for the active compositor. `on_change(bool)` is invoked
/// (on the worker thread) whenever the game-mode verdict flips. Returns `None`
/// when disabled or the compositor is unsupported (→ no auto-detection).
pub fn start(on_change: Arc<dyn Fn(bool) + Send + Sync>) -> Option<GameModeWatcher> {
    if !enabled() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));

    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        spawn_wlroots_poll(stop.clone(), on_change, WlrootsKind::Hyprland);
        eprintln!("[gamemode] watching Hyprland (hyprctl)");
        return Some(GameModeWatcher { stop });
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        spawn_wlroots_poll(stop.clone(), on_change, WlrootsKind::Sway);
        eprintln!("[gamemode] watching Sway (swaymsg)");
        return Some(GameModeWatcher { stop });
    }
    if is_kwin() {
        spawn_kwin(stop.clone(), on_change);
        eprintln!("[gamemode] watching KWin (D-Bus script)");
        return Some(GameModeWatcher { stop });
    }

    eprintln!("[gamemode] compositor not supported for auto-detect; game mode stays manual");
    None
}

fn is_kwin() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.to_ascii_uppercase().contains("KDE"))
        .unwrap_or(false)
        || std::env::var_os("KDE_FULL_SESSION").is_some()
}

// ---- wlroots (Hyprland / Sway): poll a CLI -----------------------------------

#[derive(Clone, Copy)]
enum WlrootsKind {
    Hyprland,
    Sway,
}

fn spawn_wlroots_poll(
    stop: Arc<AtomicBool>,
    on_change: Arc<dyn Fn(bool) + Send + Sync>,
    kind: WlrootsKind,
) {
    thread::spawn(move || {
        let excluded = excluded_classes();
        let mut last: Option<bool> = None;
        while !stop.load(Ordering::SeqCst) {
            if let Some(state) = match kind {
                WlrootsKind::Hyprland => query_hyprland(),
                WlrootsKind::Sway => query_sway(),
            } {
                let game = is_game(&state, &excluded);
                if last != Some(game) {
                    last = Some(game);
                    on_change(game);
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
    });
}

fn run_cli(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// `hyprctl -j activewindow` → JSON with `"fullscreen"` (0/1/int) and `"class"`.
/// Parsed with a tiny string scan to avoid pulling in a JSON dep here.
fn query_hyprland() -> Option<FocusState> {
    let json = run_cli("hyprctl", &["-j", "activewindow"])?;
    let fullscreen = json_number_field(&json, "fullscreen").map(|n| n > 0.0);
    let class = json_string_field(&json, "class").unwrap_or_default();
    Some(FocusState {
        fullscreen: fullscreen.unwrap_or(false),
        class,
    })
}

/// Sway: find the focused node, read its `fullscreen_mode` (>0) and `app_id`
/// (Wayland) / `window_properties.class` (XWayland).
fn query_sway() -> Option<FocusState> {
    let json = run_cli("swaymsg", &["-t", "get_tree"])?;
    // Locate the `"focused": true` node and read fields near it. get_tree is a
    // big nested object; a focused leaf has `"focused": true` plus its own
    // `fullscreen_mode` and `app_id`. Scan around the focused marker.
    let idx = json.find("\"focused\": true")?;
    // Search a window before/after the marker for the nearest fields.
    let window = &json[idx.saturating_sub(4000)..(idx + 4000).min(json.len())];
    let fullscreen = json_number_field(window, "fullscreen_mode")
        .map(|n| n > 0.0)
        .unwrap_or(false);
    let class = json_string_field(window, "app_id")
        .filter(|s| !s.is_empty())
        .or_else(|| json_string_field(window, "class"))
        .unwrap_or_default();
    Some(FocusState { fullscreen, class })
}

/// Read `"key": <number>` from a JSON fragment (first occurrence).
fn json_number_field(json: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

/// Read `"key": "value"` from a JSON fragment (first occurrence).
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ---- KWin: load a script that calls back over D-Bus --------------------------

const KWIN_SCRIPT: &str = r#"
function report(w) {
    var fs = false, cls = "";
    if (w) { fs = (w.fullScreen === true); cls = "" + (w.resourceClass || ""); }
    callDBus("org.st.GameMode", "/org/st/GameMode", "org.st.GameMode", "report", fs, cls);
}
function hook(w) {
    report(w);
    if (w && w.fullScreenChanged) {
        w.fullScreenChanged.connect(function() { report(workspace.activeWindow); });
    }
}
if (workspace.windowActivated) workspace.windowActivated.connect(hook);
hook(workspace.activeWindow);
"#;

/// D-Bus interface the KWin script calls back into. Each `report` updates the
/// shared focus state; the worker loop turns that into a game-mode verdict.
struct KwinReceiver {
    shared: Arc<Mutex<FocusState>>,
    dirty: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.st.GameMode")]
impl KwinReceiver {
    // KWin's callDBus uses the literal method name, so pin the wire name to
    // lowercase `report` (zbus would otherwise expose it as `Report`).
    #[zbus(name = "report")]
    fn report(&self, fullscreen: bool, class: String) {
        *self.shared.lock().unwrap() = FocusState { fullscreen, class };
        self.dirty.store(true, Ordering::SeqCst);
    }
}

fn spawn_kwin(stop: Arc<AtomicBool>, on_change: Arc<dyn Fn(bool) + Send + Sync>) {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[gamemode] tokio runtime failed ({e}); KWin detection off");
                return;
            }
        };
        runtime.block_on(async move {
            let shared = Arc::new(Mutex::new(FocusState::default()));
            let dirty = Arc::new(AtomicBool::new(false));
            let receiver = KwinReceiver {
                shared: shared.clone(),
                dirty: dirty.clone(),
            };
            // Own the well-known name and serve the callback interface BEFORE
            // loading the script, so the script's first callback lands.
            let _conn = match zbus::connection::Builder::session()
                .and_then(|b| b.name("org.st.GameMode"))
                .and_then(|b| b.serve_at("/org/st/GameMode", receiver))
            {
                Ok(b) => match b.build().await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[gamemode] zbus serve failed ({e}); KWin detection off");
                        return;
                    }
                },
                Err(e) => {
                    eprintln!("[gamemode] zbus builder failed ({e}); KWin detection off");
                    return;
                }
            };

            if let Err(e) = load_kwin_script().await {
                eprintln!("[gamemode] KWin script load failed ({e}); detection off");
                return;
            }

            let excluded = excluded_classes();
            let mut last: Option<bool> = None;
            while !stop.load(Ordering::SeqCst) {
                if dirty.swap(false, Ordering::SeqCst) {
                    let state = shared.lock().unwrap().clone();
                    let game = is_game(&state, &excluded);
                    if last != Some(game) {
                        last = Some(game);
                        on_change(game);
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
    });
}

/// Write the embedded KWin script to a temp file and load+start it over D-Bus.
async fn load_kwin_script() -> Result<(), String> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = format!("{dir}/st-gamemode.js");
    std::fs::write(&path, KWIN_SCRIPT).map_err(|e| format!("write script: {e}"))?;

    let conn = zbus::Connection::session()
        .await
        .map_err(|e| format!("session bus: {e}"))?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await
    .map_err(|e| format!("scripting proxy: {e}"))?;
    // loadScript(path, pluginName) -> id. Same plugin name replaces a prior load.
    let _id: i32 = proxy
        .call("loadScript", &(path.as_str(), "st-gamemode"))
        .await
        .map_err(|e| format!("loadScript: {e}"))?;
    proxy
        .call::<_, _, ()>("start", &())
        .await
        .map_err(|e| format!("start: {e}"))?;
    Ok(())
}
