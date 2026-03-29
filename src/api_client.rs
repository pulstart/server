use crate::server_control::ServerControl;
use st_protocol::tunnel::{CryptoContext, TunnelKeys};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared state produced by the API registration thread.
/// The server runtime reads this to set up encrypted tunnels for incoming clients.
pub struct ApiTunnelState {
    tunnel_keys: Mutex<TunnelKeys>,
    /// Derived ChaCha20 key, ready for CryptoContext creation.
    shared_key: Mutex<Option<[u8; 32]>>,
    /// Cached CryptoContext (single instance shared across all callers so the
    /// atomic nonce counter is never reset).
    crypto: Mutex<Option<Arc<CryptoContext>>>,
    /// Partner (client) NAT candidates from the API server.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Process-lifetime UDP socket used for STUN and punching.
    punch_socket: Mutex<Option<UdpSocket>>,
    /// Local candidates advertised to the API server.
    local_candidates: Mutex<Vec<String>>,
    /// Latest client punch-request nonce observed from the API server.
    pending_client_punch_nonce: AtomicU64,
    /// True while a punched session is active on the shared socket.
    punch_session_active: AtomicBool,
    /// Set when shared key, partner candidates, and a punch socket are all present.
    hole_punch_ready: AtomicBool,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

impl ApiTunnelState {
    pub fn new() -> Self {
        Self {
            tunnel_keys: Mutex::new(TunnelKeys::generate()),
            shared_key: Mutex::new(None),
            crypto: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
            punch_socket: Mutex::new(None),
            local_candidates: Mutex::new(Vec::new()),
            pending_client_punch_nonce: AtomicU64::new(0),
            punch_session_active: AtomicBool::new(false),
            hole_punch_ready: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        }
    }

    /// Return the shared CryptoContext (same instance for all callers so the
    /// atomic nonce counter is never reset).
    pub fn crypto_context(&self) -> Option<Arc<CryptoContext>> {
        let cached = self.crypto.lock().unwrap();
        if cached.is_some() {
            return cached.clone();
        }
        drop(cached);
        let key = (*self.shared_key.lock().unwrap())?;
        let ctx = Arc::new(CryptoContext::new(key, true));
        *self.crypto.lock().unwrap() = Some(Arc::clone(&ctx));
        Some(ctx)
    }

    /// Clone the process-lifetime punch socket for one hole-punch attempt/session.
    pub fn clone_punch_socket(&self, listen_port: u16) -> Result<UdpSocket, String> {
        self.ensure_punch_socket(listen_port)?;
        self.punch_socket
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| "punch socket unavailable".to_string())?
            .try_clone()
            .map_err(|e| format!("clone punch socket: {e}"))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn is_hole_punch_ready(&self) -> bool {
        self.hole_punch_ready.load(Ordering::Relaxed)
    }

    pub fn pending_client_punch_nonce(&self) -> u64 {
        self.pending_client_punch_nonce.load(Ordering::Relaxed)
    }

    pub fn update_pending_client_punch_nonce(&self, nonce: u64) {
        self.pending_client_punch_nonce.fetch_max(nonce, Ordering::Relaxed);
    }

    pub fn is_punch_session_active(&self) -> bool {
        self.punch_session_active.load(Ordering::Relaxed)
    }

    pub fn set_punch_session_active(&self, active: bool) {
        self.punch_session_active.store(active, Ordering::Relaxed);
    }

    pub fn ensure_punch_socket(&self, listen_port: u16) -> Result<Vec<String>, String> {
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        if has_socket {
            let candidates = self.local_candidates.lock().unwrap().clone();
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }

        let mut socket_guard = self.punch_socket.lock().unwrap();
        if socket_guard.is_none() {
            let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind punch socket: {e}"))?;
            *socket_guard = Some(socket);
        }
        let socket = socket_guard
            .as_ref()
            .ok_or_else(|| "punch socket unavailable".to_string())?;
        let port = socket
            .local_addr()
            .map_err(|e| format!("punch socket local_addr: {e}"))?
            .port();
        let port = if port == 0 { listen_port } else { port };
        let candidates = st_protocol::tunnel::gather_candidates_with_stun(port, Some(socket));
        drop(socket_guard);

        *self.local_candidates.lock().unwrap() = candidates.clone();
        self.update_hole_punch_ready();
        Ok(candidates)
    }

    pub fn public_key_b64(&self) -> String {
        let keys = self.tunnel_keys.lock().unwrap();
        base64_encode(&keys.public_key_bytes())
    }

    pub fn update_shared_key_from_partner_b64(&self, partner_b64: Option<&str>) {
        let Some(partner_b64) = partner_b64 else {
            self.set_shared_key(None);
            return;
        };
        let Some(partner_bytes) = base64_decode(partner_b64) else {
            self.set_shared_key(None);
            return;
        };
        if partner_bytes.len() != 32 {
            self.set_shared_key(None);
            return;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&partner_bytes);
        let shared = {
            let keys = self.tunnel_keys.lock().unwrap();
            keys.derive_shared_key(&arr)
        };
        self.set_shared_key(Some(shared));
    }

    pub fn set_partner_candidates(&self, candidates: Vec<SocketAddr>) {
        *self.partner_candidates.lock().unwrap() = candidates;
        self.update_hole_punch_ready();
    }

    pub fn clear_partner_state(&self) {
        self.set_partner_candidates(Vec::new());
        self.set_shared_key(None);
    }

    fn set_shared_key(&self, shared_key: Option<[u8; 32]>) {
        let mut current = self.shared_key.lock().unwrap();
        if *current != shared_key {
            let had_key = current.is_some();
            let has_key = shared_key.is_some();
            *current = shared_key;
            *self.crypto.lock().unwrap() = None;
            if has_key && !had_key {
                println!("[api] Shared key derived");
            }
        }
        drop(current);
        self.update_hole_punch_ready();
    }

    /// Check and update hole_punch_ready based on current state.
    pub fn update_hole_punch_ready(&self) {
        let has_key = self.shared_key.lock().unwrap().is_some();
        let has_candidates = !self.partner_candidates.lock().unwrap().is_empty();
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        self.hole_punch_ready
            .store(has_key && has_candidates && has_socket, Ordering::Relaxed);
    }
}

fn retry_secs(consecutive_failures: u32) -> u64 {
    match consecutive_failures {
        0 => 10,
        1 => 30,
        _ => 60,
    }
}

/// Sleep for `secs` seconds, but wake early if shutdown is requested.
fn interruptible_sleep(control: &ServerControl, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if control.shutdown_requested() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=');
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let a = val(*chunk.first()?)?;
        let b = val(chunk.get(1).copied()?)?;
        out.push((a << 2) | (b >> 4));
        if let Some(&c) = chunk.get(2) {
            let c = val(c)?;
            out.push((b << 4) | (c >> 2));
            if let Some(&d) = chunk.get(3) {
                let d = val(d)?;
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

/// Spawn a background thread that registers with the API server,
/// exchanges keys, polls session state, and retries with backoff on failure.
pub fn start_api_registration(
    api_url: String,
    control: Arc<ServerControl>,
    listen_port: u16,
    tunnel_state: Arc<ApiTunnelState>,
) {
    std::thread::spawn(move || {
        if let Err(e) = tunnel_state.ensure_punch_socket(listen_port) {
            eprintln!("[api] Failed to prepare punch socket: {e}");
        }
        let peer_id = control.peer_id().to_string();
        let hostname = get_hostname();
        let mut failures: u32 = 0;

        loop {
            if control.shutdown_requested() {
                break;
            }

            let token = control.token();
            let local_candidates = match tunnel_state.ensure_punch_socket(listen_port) {
                Ok(candidates) => candidates,
                Err(e) => {
                    eprintln!("[api] Failed to prepare punch socket: {e}");
                    tunnel_state.local_candidates.lock().unwrap().clear();
                    Vec::new()
                }
            };

            let body = serde_json::json!({
                "token": token,
                "role": "host",
                "peer_id": peer_id,
                "hostname": hostname,
                "candidates": local_candidates,
            })
            .to_string();
            let ok = ureq::post(&format!("{api_url}/api/register"))
                .set("Content-Type", "application/json")
                .send_string(&body)
                .is_ok();

            if ok {
                if failures > 0 || !tunnel_state.is_connected() {
                    println!("[api] Connected to API server");
                }
                failures = 0;
                tunnel_state.connected.store(true, Ordering::Relaxed);

                let key_body = serde_json::json!({
                    "token": token,
                    "role": "host",
                    "public_key": tunnel_state.public_key_b64(),
                })
                .to_string();
                match ureq::post(&format!("{api_url}/api/key"))
                    .set("Content-Type", "application/json")
                    .send_string(&key_body)
                {
                    Ok(resp) => {
                        if let Ok(text) = resp.into_string() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                tunnel_state.update_shared_key_from_partner_b64(
                                    v["partner_key"].as_str(),
                                );
                            } else {
                                tunnel_state.set_shared_key(None);
                            }
                        } else {
                            tunnel_state.set_shared_key(None);
                        }
                    }
                    Err(_) => tunnel_state.set_shared_key(None),
                }

                let cand_body = serde_json::json!({
                    "token": token,
                    "role": "host",
                    "candidates": local_candidates,
                })
                .to_string();
                match ureq::post(&format!("{api_url}/api/candidates"))
                    .set("Content-Type", "application/json")
                    .send_string(&cand_body)
                {
                    Ok(resp) => {
                        if let Ok(text) = resp.into_string() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                let addrs: Vec<SocketAddr> = v["partner_candidates"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|value| value.as_str()?.parse().ok())
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                tunnel_state.set_partner_candidates(addrs);
                            } else {
                                tunnel_state.set_partner_candidates(Vec::new());
                            }
                        } else {
                            tunnel_state.set_partner_candidates(Vec::new());
                        }
                    }
                    Err(_) => tunnel_state.set_partner_candidates(Vec::new()),
                }

                let session_body = format!(r#"{{"token":"{token}"}}"#);
                let mut client_joined = false;
                match ureq::post(&format!("{api_url}/api/session"))
                    .set("Content-Type", "application/json")
                    .send_string(&session_body)
                {
                    Ok(resp) => {
                        if let Ok(text) = resp.into_string() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                client_joined = v["client_joined"].as_bool().unwrap_or(false);
                                let punch_nonce = v["client_punch_nonce"].as_u64().unwrap_or(0);
                                tunnel_state.update_pending_client_punch_nonce(punch_nonce);
                                if !client_joined {
                                    tunnel_state.clear_partner_state();
                                }
                            } else {
                                tunnel_state.clear_partner_state();
                            }
                        } else {
                            tunnel_state.clear_partner_state();
                        }
                    }
                    Err(_) => tunnel_state.clear_partner_state(),
                }

                let secs = if client_joined {
                    1
                } else if tunnel_state.shared_key.lock().unwrap().is_some() {
                    3
                } else {
                    3
                };
                if interruptible_sleep(&control, secs) {
                    break;
                }
            } else {
                tunnel_state.connected.store(false, Ordering::Relaxed);
                let secs = retry_secs(failures);
                failures = failures.saturating_add(1);
                eprintln!("[api] Registration failed, retrying in {secs}s");
                if interruptible_sleep(&control, secs) {
                    break;
                }
            }
        }

        let token = control.token();
        let body = serde_json::json!({
            "token": token,
            "role": "host",
            "peer_id": peer_id,
        })
        .to_string();
        let _ = ureq::post(&format!("{api_url}/api/unregister"))
            .set("Content-Type", "application/json")
            .send_string(&body);
        tunnel_state.connected.store(false, Ordering::Relaxed);
        println!("[api] Unregistered from API server");
    });
}

fn get_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        })
}
