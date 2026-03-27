use crate::encode_config::{Codec, QualityPreset};
use crate::updater::{self, ReleaseInfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime};

const AUTO_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const CONFIG_FILENAME: &str = "st-server-config.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codec: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    bitrate_kbps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_id: Option<String>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl PersistedSettings {
    fn codec_value(&self) -> u8 {
        match self.codec.as_deref() {
            Some("h264") => 1,
            Some("hevc") => 2,
            Some("av1") => 3,
            _ => 0,
        }
    }

    fn quality_value(&self) -> u8 {
        match self.quality.as_deref() {
            Some("low_latency") => 1,
            Some("balanced") => 2,
            Some("high_quality") => 3,
            _ => 0,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    // Prefer XDG state directory for stable persistence across rebuilds.
    if let Some(state_dir) = std::env::var_os("XDG_STATE_HOME") {
        let dir = PathBuf::from(state_dir).join("st");
        let _ = std::fs::create_dir_all(&dir);
        return Some(dir.join(CONFIG_FILENAME));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let dir = PathBuf::from(home).join(".local").join("state").join("st");
        let _ = std::fs::create_dir_all(&dir);
        return Some(dir.join(CONFIG_FILENAME));
    }
    // Fall back to exe directory.
    std::env::current_exe().ok().and_then(|exe| {
        exe.parent().map(|dir| dir.join(CONFIG_FILENAME))
    })
}

fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    let a = h.finish();
    let mut h2 = s.build_hasher();
    h2.write_u64(a ^ 0xdeadbeef);
    let b = h2.finish();
    format!("{a:016x}{b:016x}")
}

fn load_settings() -> PersistedSettings {
    let Some(path) = config_path() else {
        return PersistedSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => PersistedSettings::default(),
    }
}

fn resolve_peer_id(saved: &mut PersistedSettings) -> String {
    if let Some(ref id) = saved.peer_id {
        if !id.is_empty() {
            return id.clone();
        }
    }
    let id = generate_token(); // reuse the same hex generator
    saved.peer_id = Some(id.clone());
    if let Some(path) = config_path() {
        if let Ok(json) = serde_json::to_string_pretty(saved) {
            let _ = std::fs::write(&path, json);
        }
    }
    id
}

fn resolve_token(saved: &mut PersistedSettings) -> String {
    // ST_TOKEN env var overrides everything
    if let Ok(env_token) = std::env::var("ST_TOKEN") {
        let t = env_token.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    // Use persisted token or generate a new one
    if let Some(ref t) = saved.token {
        if !t.is_empty() {
            return t.clone();
        }
    }
    let t = generate_token();
    saved.token = Some(t.clone());
    // Persist immediately so the token is stable across restarts
    match config_path() {
        Some(path) => match serde_json::to_string_pretty(saved) {
            Ok(json) => {
                if let Err(err) = std::fs::write(&path, &json) {
                    eprintln!("[config] Failed to persist token to {}: {err}", path.display());
                } else {
                    eprintln!("[config] Token persisted to {}", path.display());
                }
            }
            Err(err) => eprintln!("[config] Failed to serialize settings: {err}"),
        },
        None => eprintln!("[config] Warning: cannot determine config path, token will not persist across restarts"),
    }
    t
}

#[derive(Clone, Debug)]
pub struct ConnectedClientSnapshot {
    pub id: usize,
    pub addr: SocketAddr,
    pub connected_at: SystemTime,
}

#[derive(Clone, Debug)]
pub enum UpdateStateSnapshot {
    Unsupported(String),
    Idle,
    Checking,
    UpToDate { version: String },
    UpdateAvailable(ReleaseInfo),
    Installing { version: String },
    ClosingForUpdate { version: String },
    Error(String),
}

pub struct RegisteredClient {
    snapshot: ConnectedClientSnapshot,
    disconnect_requested: Arc<AtomicBool>,
    control: Arc<ServerControl>,
}

struct ConnectedClientEntry {
    snapshot: ConnectedClientSnapshot,
    disconnect_requested: Arc<AtomicBool>,
}

pub struct ServerControl {
    allow_new_connections: AtomicBool,
    shutdown_requested: AtomicBool,
    next_client_id: AtomicUsize,
    version: AtomicUsize,
    clients: Mutex<BTreeMap<usize, ConnectedClientEntry>>,
    update_state: Mutex<UpdateStateSnapshot>,
    update_task_running: AtomicBool,
    auto_update_checks_started: AtomicBool,
    // Video overrides (0 = auto for all)
    forced_codec: AtomicU8,
    forced_bitrate_kbps: AtomicU32,
    forced_quality: AtomicU8,
    /// Authentication token for client connections.
    token: Mutex<String>,
    /// Stable peer identifier, persisted across restarts.
    peer_id: String,
}

impl ServerControl {
    pub fn new() -> Arc<Self> {
        let mut saved = load_settings();
        let token = resolve_token(&mut saved);
        let peer_id = resolve_peer_id(&mut saved);
        println!("[auth] Server token: {token}");
        println!("[auth] Peer ID: {peer_id}");
        Arc::new(Self {
            allow_new_connections: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            next_client_id: AtomicUsize::new(1),
            version: AtomicUsize::new(1),
            clients: Mutex::new(BTreeMap::new()),
            update_state: Mutex::new(initial_update_state()),
            update_task_running: AtomicBool::new(false),
            auto_update_checks_started: AtomicBool::new(false),
            forced_codec: AtomicU8::new(saved.codec_value()),
            forced_bitrate_kbps: AtomicU32::new(saved.bitrate_kbps),
            forced_quality: AtomicU8::new(saved.quality_value()),
            token: Mutex::new(token),
            peer_id,
        })
    }

    /// Returns the server authentication token.
    pub fn token(&self) -> String {
        self.token.lock().unwrap().clone()
    }

    /// Returns the stable peer identifier.
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Replace the token, persist to config, and disconnect all current clients.
    pub fn set_token(&self, new_token: String) {
        {
            let mut t = self.token.lock().unwrap();
            *t = new_token;
        }
        println!("[auth] Token updated: {}", self.token());
        self.save_video_settings();
        self.disconnect_all_clients();
        self.bump_version();
    }

    pub fn allow_new_connections(&self) -> bool {
        self.allow_new_connections.load(Ordering::SeqCst)
    }

    pub fn set_allow_new_connections(&self, allow: bool) {
        self.allow_new_connections.store(allow, Ordering::SeqCst);
        self.bump_version();
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    pub fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        self.disconnect_all_clients();
        self.bump_version();
    }

    pub fn ui_version(&self) -> usize {
        self.version.load(Ordering::SeqCst)
    }

    pub fn connected_clients(&self) -> Vec<ConnectedClientSnapshot> {
        let clients = self.clients.lock().unwrap();
        clients.values().map(|entry| entry.snapshot.clone()).collect()
    }

    pub fn update_state(&self) -> UpdateStateSnapshot {
        self.update_state.lock().unwrap().clone()
    }

    pub fn start_automatic_update_checks(self: &Arc<Self>) {
        if self
            .auto_update_checks_started
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        if matches!(self.update_state(), UpdateStateSnapshot::Unsupported(_)) {
            return;
        }

        let control = Arc::clone(self);
        thread::spawn(move || {
            control.begin_update_check();
            loop {
                if control.shutdown_requested() {
                    break;
                }
                thread::sleep(AUTO_UPDATE_CHECK_INTERVAL);
                if control.shutdown_requested() {
                    break;
                }
                match control.update_state() {
                    UpdateStateSnapshot::Checking
                    | UpdateStateSnapshot::Installing { .. }
                    | UpdateStateSnapshot::ClosingForUpdate { .. } => {}
                    _ => control.begin_update_check(),
                }
            }
        });
    }

    pub fn begin_update_check(self: &Arc<Self>) {
        if matches!(self.update_state(), UpdateStateSnapshot::Unsupported(_)) {
            return;
        }
        if self
            .update_task_running
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        self.set_update_state(UpdateStateSnapshot::Checking);
        let control = Arc::clone(self);
        thread::spawn(move || {
            let next_state = match updater::check_latest_release() {
                Ok(updater::CheckOutcome::UpToDate { latest_version: version }) => {
                    UpdateStateSnapshot::UpToDate { version }
                }
                Ok(updater::CheckOutcome::UpdateAvailable(release)) => {
                    UpdateStateSnapshot::UpdateAvailable(release)
                }
                Err(err) => UpdateStateSnapshot::Error(err),
            };
            control.set_update_state(next_state);
            control
                .update_task_running
                .store(false, Ordering::SeqCst);
        });
    }

    pub fn begin_update_install(self: &Arc<Self>) {
        if self
            .update_task_running
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        let release = match self.update_state() {
            UpdateStateSnapshot::UpdateAvailable(release) => release,
            _ => {
                self.update_task_running.store(false, Ordering::SeqCst);
                return;
            }
        };

        let version = release.version.clone();
        self.set_update_state(UpdateStateSnapshot::Installing {
            version: version.clone(),
        });

        let control = Arc::clone(self);
        thread::spawn(move || match updater::prepare_and_spawn_update(&release) {
            Ok(()) => {
                control.set_update_state(UpdateStateSnapshot::ClosingForUpdate { version });
                control.request_shutdown();
            }
            Err(err) => {
                control.set_update_state(UpdateStateSnapshot::Error(err));
                control
                    .update_task_running
                    .store(false, Ordering::SeqCst);
            }
        });
    }

    pub fn register_client(self: &Arc<Self>, addr: SocketAddr) -> RegisteredClient {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let disconnect_requested = Arc::new(AtomicBool::new(false));
        let snapshot = ConnectedClientSnapshot {
            id,
            addr,
            connected_at: SystemTime::now(),
        };
        let entry = ConnectedClientEntry {
            snapshot: snapshot.clone(),
            disconnect_requested: Arc::clone(&disconnect_requested),
        };
        self.clients.lock().unwrap().insert(id, entry);
        self.bump_version();
        notify_client_connection(&snapshot);
        RegisteredClient {
            snapshot,
            disconnect_requested,
            control: Arc::clone(self),
        }
    }

    pub fn request_disconnect(&self, id: usize) -> bool {
        let clients = self.clients.lock().unwrap();
        let Some(entry) = clients.get(&id) else {
            return false;
        };
        entry.disconnect_requested.store(true, Ordering::SeqCst);
        true
    }

    pub fn disconnect_all_clients(&self) {
        let clients = self.clients.lock().unwrap();
        for entry in clients.values() {
            entry.disconnect_requested.store(true, Ordering::SeqCst);
        }
    }

    // --- Video overrides ---

    pub fn forced_codec(&self) -> Option<Codec> {
        match self.forced_codec.load(Ordering::SeqCst) {
            1 => Some(Codec::H264),
            2 => Some(Codec::Hevc),
            3 => Some(Codec::Av1),
            _ => None,
        }
    }

    pub fn set_forced_codec(&self, codec: Option<Codec>) {
        let v = match codec {
            None => 0,
            Some(Codec::H264) => 1,
            Some(Codec::Hevc) => 2,
            Some(Codec::Av1) => 3,
        };
        self.forced_codec.store(v, Ordering::SeqCst);
        self.bump_version();
        self.save_video_settings();
    }

    /// Returns 0 for auto (adaptive bitrate).
    pub fn forced_bitrate_kbps(&self) -> u32 {
        self.forced_bitrate_kbps.load(Ordering::SeqCst)
    }

    /// Set to 0 for auto (adaptive bitrate).
    pub fn set_forced_bitrate_kbps(&self, kbps: u32) {
        self.forced_bitrate_kbps.store(kbps, Ordering::SeqCst);
        self.bump_version();
        self.save_video_settings();
    }

    pub fn forced_quality(&self) -> Option<QualityPreset> {
        match self.forced_quality.load(Ordering::SeqCst) {
            1 => Some(QualityPreset::LowLatency),
            2 => Some(QualityPreset::Balanced),
            3 => Some(QualityPreset::HighQuality),
            _ => None,
        }
    }

    pub fn set_forced_quality(&self, quality: Option<QualityPreset>) {
        let v = match quality {
            None => 0,
            Some(QualityPreset::LowLatency) => 1,
            Some(QualityPreset::Balanced) => 2,
            Some(QualityPreset::HighQuality) => 3,
        };
        self.forced_quality.store(v, Ordering::SeqCst);
        self.bump_version();
        self.save_video_settings();
    }

    fn save_video_settings(&self) {
        let settings = PersistedSettings {
            codec: match self.forced_codec.load(Ordering::SeqCst) {
                1 => Some("h264".into()),
                2 => Some("hevc".into()),
                3 => Some("av1".into()),
                _ => None,
            },
            bitrate_kbps: self.forced_bitrate_kbps.load(Ordering::SeqCst),
            quality: match self.forced_quality.load(Ordering::SeqCst) {
                1 => Some("low_latency".into()),
                2 => Some("balanced".into()),
                3 => Some("high_quality".into()),
                _ => None,
            },
            token: Some(self.token.lock().unwrap().clone()),
            peer_id: Some(self.peer_id.clone()),
        };
        if let Some(path) = config_path() {
            match serde_json::to_string_pretty(&settings) {
                Ok(json) => {
                    if let Err(err) = std::fs::write(&path, json) {
                        eprintln!("[config] Failed to save {}: {err}", path.display());
                    }
                }
                Err(err) => eprintln!("[config] Failed to serialize settings: {err}"),
            }
        }
    }

    fn unregister_client(&self, id: usize) {
        let removed = self.clients.lock().unwrap().remove(&id);
        if removed.is_some() {
            self.bump_version();
        }
    }

    fn set_update_state(&self, next_state: UpdateStateSnapshot) {
        *self.update_state.lock().unwrap() = next_state;
        self.bump_version();
    }

    fn bump_version(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }
}

impl RegisteredClient {
    pub fn disconnect_requested(&self) -> bool {
        self.disconnect_requested.load(Ordering::SeqCst) || self.control.shutdown_requested()
    }
}

impl Drop for RegisteredClient {
    fn drop(&mut self) {
        self.control.unregister_client(self.snapshot.id);
    }
}

fn initial_update_state() -> UpdateStateSnapshot {
    match updater::supported_target_label() {
        Ok(_) => UpdateStateSnapshot::Idle,
        Err(err) => UpdateStateSnapshot::Unsupported(err),
    }
}

fn notify_client_connection(snapshot: &ConnectedClientSnapshot) {
    let age_hint = snapshot
        .connected_at
        .elapsed()
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let title = "st-server";
    let body = if age_hint == 0 {
        format!("Client connected: {}", snapshot.addr)
    } else {
        format!("Client connected: {} ({age_hint}s)", snapshot.addr)
    };

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {} with title {}",
            apple_script_string(&body),
            apple_script_string(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("notify-send").arg(title).arg(&body).status();
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
