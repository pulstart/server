use crate::updater::{self, ReleaseInfo};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime};

const AUTO_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

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
}

impl ServerControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            allow_new_connections: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            next_client_id: AtomicUsize::new(1),
            version: AtomicUsize::new(1),
            clients: Mutex::new(BTreeMap::new()),
            update_state: Mutex::new(initial_update_state()),
            update_task_running: AtomicBool::new(false),
            auto_update_checks_started: AtomicBool::new(false),
        })
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
