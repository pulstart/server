//! Session-side "game mode" detector for the in-session tray agent.
//!
//! The root service can't see the user's compositor, so the per-user tray agent
//! asks the compositor which window is focused and whether it's fullscreen, and
//! pushes a "game mode" hint to the service over the control socket (mirrors the
//! screen-wake-via-tray pattern). This lets the service interpret an absent KMS
//! cursor as hidden while a game is focused. The client then enters relative
//! capture; a visible menu cursor still restores absolute input.
//!
//! Signal: the focused window is a game when EITHER
//!   - its app-class matches a known game class (`steam_app_…`, `gamescope`, …),
//!     regardless of fullscreen — catches **windowed / borderless** games; OR
//!   - its identity is a known standalone game (`z3d`); OR
//!   - it is **fullscreen** AND its app-class is **not** a known browser / video
//!     player (those go fullscreen for content you still want to click).
//!
//! Backends: KWin (event-driven via a loaded KWin script that calls back over
//! D-Bus), Hyprland & Sway (polled via `hyprctl` / `swaymsg`). Other compositors
//! get no auto-detection (manual only).
//!
//! `ST_GAME_MODE=0`/`false`/`no`/`off` disables the whole detector. Extra
//! excluded classes via `ST_GAME_MODE_EXCLUDE`; extra always-game classes (for a
//! windowed game with an arbitrary class) via `ST_GAME_MODE_CLASSES`. Both are
//! comma-separated, case-insensitive substring matches.
//! Windows without an app-class use the owning executable's basename, resolved
//! from the compositor-reported PID. Window titles are not used as identities.

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

/// Built-in app-class fragments that are games regardless of fullscreen state —
/// matched so windowed / borderless games still trigger. Matched
/// case-insensitively as substrings of the window's resource class / app-id.
const DEFAULT_GAME_CLASSES: &[&str] = &["steam_app_", "gamescope", "lutris"];

/// Exact identities: short standalone names must not match unrelated apps.
const STANDALONE_GAMES: &[&str] = &["z3d"];

/// Native Wayland apps may omit app_id (including Z3D's default winit window).
/// Resolve their identity in the user session, where /proc/PID/exe is readable.
/// A missing/exited process leaves the identity unknown; never infer from title.
fn window_identity(class: &str, pid: Option<u32>) -> String {
    if !class.is_empty() {
        return class.to_string();
    }
    pid.filter(|&pid| pid > 0)
        .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// True when game-mode auto-detection is enabled (`ST_GAME_MODE`, default on).
fn enabled() -> bool {
    !matches!(
        std::env::var("ST_GAME_MODE").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// Merge a built-in fragment list with a comma-separated env override into a
/// lowercased substring-match list.
fn class_list(builtin: &[&str], env_key: &str) -> Vec<String> {
    let mut v: Vec<String> = builtin.iter().map(|s| s.to_string()).collect();
    if let Ok(extra) = std::env::var(env_key) {
        for c in extra.split(',') {
            let c = c.trim().to_ascii_lowercase();
            if !c.is_empty() {
                v.push(c);
            }
        }
    }
    v
}

fn excluded_classes() -> Vec<String> {
    class_list(DEFAULT_EXCLUDED, "ST_GAME_MODE_EXCLUDE")
}

fn game_classes() -> Vec<String> {
    class_list(DEFAULT_GAME_CLASSES, "ST_GAME_MODE_CLASSES")
}

/// Decide whether the focused window is a game. A known game class wins
/// regardless of fullscreen (windowed / borderless games); otherwise it must be
/// fullscreen and not a known browser / video player.
fn is_game(state: &FocusState, excluded: &[String], games: &[String]) -> bool {
    let cls = state.class.to_ascii_lowercase();
    if STANDALONE_GAMES.contains(&cls.as_str()) || games.iter().any(|g| cls.contains(g.as_str())) {
        return true;
    }
    if !state.fullscreen || cls.is_empty() {
        return false;
    }
    !excluded.iter().any(|e| cls.contains(e.as_str()))
}

/// Running detector. Holds its worker thread alive for the process lifetime; the
/// `stop` flag lets poll loops exit promptly. The KWin script is replaced on
/// the next start so the new receiver gets an initial focused-window report.
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
        let games = game_classes();
        let mut last: Option<bool> = None;
        while !stop.load(Ordering::SeqCst) {
            if let Some(state) = match kind {
                WlrootsKind::Hyprland => query_hyprland(),
                WlrootsKind::Sway => query_sway(),
            } {
                let game = is_game(&state, &excluded, &games);
                if last != Some(game) {
                    last = Some(game);
                    eprintln!(
                        "[gamemode] game={game} fullscreen={} identity={:?}",
                        state.fullscreen, state.class
                    );
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

/// `hyprctl -j activewindow` reports the focused window directly.
fn query_hyprland() -> Option<FocusState> {
    let json = run_cli("hyprctl", &["-j", "activewindow"])?;
    let window: serde_json::Value = serde_json::from_str(&json).ok()?;
    Some(FocusState {
        fullscreen: window["fullscreen"].as_u64().unwrap_or(0) > 0,
        class: window_identity(
            window["class"].as_str().unwrap_or_default(),
            window["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok()),
        ),
    })
}

/// Sway returns a tree, including both tiled and floating containers. Read
/// only the focused node's identity, inheriting fullscreen from its ancestors.
fn query_sway() -> Option<FocusState> {
    let json = run_cli("swaymsg", &["-t", "get_tree"])?;
    parse_sway_focus(&json)
}

fn parse_sway_focus(json: &str) -> Option<FocusState> {
    fn focused(node: &serde_json::Value, parent_fullscreen: bool) -> Option<FocusState> {
        let fullscreen = parent_fullscreen || node["fullscreen_mode"].as_u64().unwrap_or(0) > 0;
        if node["focused"].as_bool() == Some(true) {
            let class = node["app_id"]
                .as_str()
                .filter(|class| !class.is_empty())
                .or_else(|| node["window_properties"]["class"].as_str())
                .unwrap_or_default();
            let class = window_identity(
                class,
                node["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()),
            );
            // An empty focused workspace is not a fullscreen application.
            return Some(FocusState {
                fullscreen: fullscreen && !class.is_empty(),
                class,
            });
        }
        for field in ["nodes", "floating_nodes"] {
            if let Some(children) = node[field].as_array() {
                for child in children {
                    if let Some(state) = focused(child, fullscreen) {
                        return Some(state);
                    }
                }
            }
        }
        None
    }

    let tree: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(focused(&tree, false).unwrap_or_default())
}

// ---- KWin: load a script that calls back over D-Bus --------------------------

const KWIN_SCRIPT: &str = r#"
function report(w) {
    var fs = false, cls = "", pid = "";
    if (w) {
        fs = (w.fullScreen === true);
        cls = "" + (w.resourceClass || "");
        pid = "" + (w.pid || "");
    }
    callDBus("org.st.GameMode", "/org/st/GameMode", "org.st.GameMode", "report", fs, cls, pid);
}
function hook(w) {
    report(w);
    if (w && w.fullScreenChanged) {
        w.fullScreenChanged.connect(function() { report(workspace.activeWindow); });
    }
    if (w && w.windowClassChanged) {
        w.windowClassChanged.connect(function() { report(workspace.activeWindow); });
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
    fn report(&self, fullscreen: bool, class: String, pid: String) {
        *self.shared.lock().unwrap() = FocusState {
            fullscreen,
            class: window_identity(&class, pid.parse().ok()),
        };
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
            let games = game_classes();
            let mut last: Option<bool> = None;
            while !stop.load(Ordering::SeqCst) {
                if dirty.swap(false, Ordering::SeqCst) {
                    let state = shared.lock().unwrap().clone();
                    let game = is_game(&state, &excluded, &games);
                    if last != Some(game) {
                        last = Some(game);
                        eprintln!(
                            "[gamemode] game={game} fullscreen={} identity={:?}",
                            state.fullscreen, state.class
                        );
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
    // KWin returns -1 for an already-loaded plugin; it does not replace it.
    // Unload first so restarting the tray refreshes both the code and its
    // initial focus report. Deletion is deferred in KWin's event loop.
    proxy
        .call::<_, _, bool>("unloadScript", &("st-gamemode",))
        .await
        .map_err(|e| format!("unloadScript: {e}"))?;
    for _ in 0..20 {
        let id: i32 = proxy
            .call("loadScript", &(path.as_str(), "st-gamemode"))
            .await
            .map_err(|e| format!("loadScript: {e}"))?;
        if id >= 0 {
            proxy
                .call::<_, _, ()>("start", &())
                .await
                .map_err(|e| format!("start: {e}"))?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err("KWin did not unload the previous game-mode script".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(fullscreen: bool, class: &str) -> FocusState {
        FocusState {
            fullscreen,
            class: class.to_string(),
        }
    }

    fn lists() -> (Vec<String>, Vec<String>) {
        (
            DEFAULT_EXCLUDED.iter().map(|s| s.to_string()).collect(),
            DEFAULT_GAME_CLASSES.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn fullscreen_game_is_game() {
        let (ex, ga) = lists();
        assert!(is_game(&st(true, "cs2"), &ex, &ga));
    }

    #[test]
    fn fullscreen_browser_is_not_game() {
        let (ex, ga) = lists();
        assert!(!is_game(&st(true, "firefox"), &ex, &ga));
        assert!(!is_game(&st(true, "org.mozilla.firefox"), &ex, &ga));
    }

    #[test]
    fn windowed_non_game_is_not_game() {
        let (ex, ga) = lists();
        assert!(!is_game(&st(false, "Alacritty"), &ex, &ga));
    }

    #[test]
    fn windowed_steam_game_is_game() {
        // A known game class wins even when the window is not fullscreen.
        let (ex, ga) = lists();
        assert!(is_game(&st(false, "steam_app_730"), &ex, &ga));
        assert!(is_game(&st(false, "gamescope"), &ex, &ga));
    }

    #[test]
    fn windowed_z3d_is_game() {
        let (ex, ga) = lists();
        assert!(is_game(&st(false, "z3d"), &ex, &ga));
        assert!(!is_game(&st(false, "z3d-editor"), &ex, &ga));
    }

    #[test]
    fn window_identity_preserves_class_and_resolves_missing_app_id() {
        let pid = Some(std::process::id());
        assert_eq!(window_identity("firefox", pid), "firefox");
        let executable = std::env::current_exe().unwrap();
        assert_eq!(
            window_identity("", pid),
            executable.file_name().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn unknown_identity_does_not_grab_even_when_fullscreen() {
        let (ex, ga) = lists();
        for pid in [None, Some(0), Some(u32::MAX)] {
            let identity = window_identity("", pid);
            assert!(identity.is_empty());
            assert!(!is_game(&st(true, &identity), &ex, &ga));
        }
    }

    #[test]
    fn kwin_report_resolves_pid_when_app_id_is_empty() {
        let receiver = KwinReceiver {
            shared: Arc::new(Mutex::new(FocusState::default())),
            dirty: Arc::new(AtomicBool::new(false)),
        };
        receiver.report(false, String::new(), std::process::id().to_string());
        assert!(receiver.dirty.load(Ordering::SeqCst));
        let state = receiver.shared.lock().unwrap().clone();
        assert!(!state.fullscreen);
        assert_eq!(state.class, window_identity("", Some(std::process::id())));
        // Losing focus clears the fallback identity as well.
        receiver.report(false, String::new(), String::new());
        assert!(receiver.shared.lock().unwrap().class.is_empty());
    }

    #[test]
    fn game_class_beats_exclusion_and_fullscreen() {
        // game-class match short-circuits before the fullscreen + exclude rule.
        let (ex, ga) = lists();
        assert!(is_game(&st(false, "steam_app_42"), &ex, &ga));
    }

    #[test]
    fn sway_reads_focused_floating_game_instead_of_nearby_browser() {
        let tree = serde_json::json!({
            "nodes": [{"app_id": "firefox", "focused": false, "fullscreen_mode": 0}],
            "floating_nodes": [{
                "fullscreen_mode": 1,
                "nodes": [{"focused": true, "app_id": null,
                    "window_properties": {"class": "game-\"世界\""}}]
            }]
        });
        let focus = parse_sway_focus(&tree.to_string()).unwrap();
        assert_eq!(focus.class, "game-\"世界\"");
        assert!(focus.fullscreen);
        let (ex, ga) = lists();
        assert!(is_game(&focus, &ex, &ga));
    }

    #[test]
    fn sway_does_not_inherit_unfocused_games_identity() {
        let tree = serde_json::json!({"nodes": [
            {"app_id": "steam_app_730", "fullscreen_mode": 1, "focused": false},
            {"app_id": "firefox", "fullscreen_mode": 1, "focused": true}
        ]});
        let focus = parse_sway_focus(&tree.to_string()).unwrap();
        let (ex, ga) = lists();
        assert!(!is_game(&focus, &ex, &ga));
    }

    #[test]
    fn sway_clears_game_mode_on_empty_focus() {
        let (ex, ga) = lists();
        for tree in [r#"{"focused":true,"type":"workspace"}"#, r#"{"nodes":[]}"#] {
            assert!(!is_game(&parse_sway_focus(tree).unwrap(), &ex, &ga));
        }
        assert!(parse_sway_focus("invalid JSON").is_none());
    }
}
