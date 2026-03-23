use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug)]
pub struct ConnectedClientSnapshot {
    pub id: usize,
    pub addr: SocketAddr,
    pub connected_at: SystemTime,
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
}

impl ServerControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            allow_new_connections: AtomicBool::new(true),
            shutdown_requested: AtomicBool::new(false),
            next_client_id: AtomicUsize::new(1),
            version: AtomicUsize::new(1),
            clients: Mutex::new(BTreeMap::new()),
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
