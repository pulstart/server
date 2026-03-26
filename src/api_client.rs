use crate::server_control::ServerControl;
use st_protocol::tunnel::{CryptoContext, TunnelKeys};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared state produced by the API registration thread.
/// The server runtime reads this to set up encrypted tunnels for incoming clients.
pub struct ApiTunnelState {
    /// Derived ChaCha20 key, ready for CryptoContext creation.
    pub shared_key: Mutex<Option<[u8; 32]>>,
    /// Partner (client) NAT candidates from the API server.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Pre-bound UDP socket for hole punching (taken once by the punch attempt).
    pub punch_socket: Mutex<Option<UdpSocket>>,
    /// Set when both shared_key and partner_candidates are populated.
    pub hole_punch_ready: AtomicBool,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

impl ApiTunnelState {
    pub fn new() -> Self {
        Self {
            shared_key: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
            punch_socket: Mutex::new(None),
            hole_punch_ready: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        }
    }

    /// Build a CryptoContext for the host side if a shared key has been negotiated.
    pub fn crypto_context(&self) -> Option<Arc<CryptoContext>> {
        self.shared_key
            .lock()
            .unwrap()
            .map(|key| Arc::new(CryptoContext::new(key, true)))
    }

    /// Take the pre-bound punch socket for use in hole punching (one-shot).
    pub fn take_punch_socket(&self) -> Option<UdpSocket> {
        self.punch_socket.lock().unwrap().take()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn is_hole_punch_ready(&self) -> bool {
        self.hole_punch_ready.load(Ordering::Relaxed)
    }

    /// Check and update hole_punch_ready based on current state.
    pub fn update_hole_punch_ready(&self) {
        let has_key = self.shared_key.lock().unwrap().is_some();
        let has_candidates = !self.partner_candidates.lock().unwrap().is_empty();
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        self.hole_punch_ready.store(has_key && has_candidates && has_socket, Ordering::Relaxed);
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
            return true; // interrupted
        }
        std::thread::sleep(Duration::from_millis(500));
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
/// exchanges keys, and retries with backoff on failure.
pub fn start_api_registration(
    api_url: String,
    control: Arc<ServerControl>,
    listen_port: u16,
    tunnel_state: Arc<ApiTunnelState>,
) {
    std::thread::spawn(move || {
        // Bind a dedicated punch socket and advertise its port in candidates.
        let punch_port = match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
                *tunnel_state.punch_socket.lock().unwrap() = Some(sock);
                port
            }
            Err(e) => {
                eprintln!("[api] Failed to bind punch socket: {e}");
                listen_port
            }
        };
        // Gather local candidates + discover public IP via STUN on the punch socket.
        let candidates = {
            let sock_guard = tunnel_state.punch_socket.lock().unwrap();
            st_protocol::tunnel::gather_candidates_with_stun(
                punch_port,
                sock_guard.as_ref(),
            )
        };
        let peer_id = control.peer_id().to_string();
        let hostname = get_hostname();
        println!("[api] Registering with {api_url} (peer_id={peer_id}, hostname={hostname}, punch_port={punch_port}, candidates: {candidates:?})");

        let keys = TunnelKeys::generate();
        let pub_key_b64 = base64_encode(&keys.public_key_bytes());
        let keys = Mutex::new(Some(keys));
        let mut failures: u32 = 0;

        loop {
            if control.shutdown_requested() {
                break;
            }

            let token = control.token();
            let cands_json =
                serde_json::to_string(&candidates).unwrap_or_else(|_| "[]".into());

            // 1. Register
            let body = format!(
                r#"{{"token":"{token}","role":"host","peer_id":"{peer_id}","hostname":"{hostname}","candidates":{cands_json}}}"#,
            );
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

                // 2. Upload our public key and try to get partner's key back
                let key_body = format!(
                    r#"{{"token":"{token}","role":"host","public_key":"{pub_key_b64}"}}"#,
                );
                if let Ok(resp) = ureq::post(&format!("{api_url}/api/key"))
                    .set("Content-Type", "application/json")
                    .send_string(&key_body)
                {
                    if let Ok(text) = resp.into_string() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(partner_key_b64) = v["partner_key"].as_str() {
                                if let Some(partner_bytes) = base64_decode(partner_key_b64) {
                                    if partner_bytes.len() == 32 {
                                        let mut arr = [0u8; 32];
                                        arr.copy_from_slice(&partner_bytes);
                                        if let Some(k) = keys.lock().unwrap().take() {
                                            let shared = k.derive_shared_key(&arr);
                                            *tunnel_state.shared_key.lock().unwrap() =
                                                Some(shared);
                                            println!("[api] Shared key derived");
                                            tunnel_state.update_hole_punch_ready();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // 3. Share candidates and fetch partner's candidates
                let cand_body = format!(
                    r#"{{"token":"{token}","role":"host","candidates":{cands_json}}}"#,
                );
                if let Ok(resp) = ureq::post(&format!("{api_url}/api/candidates"))
                    .set("Content-Type", "application/json")
                    .send_string(&cand_body)
                {
                    if let Ok(text) = resp.into_string() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(arr) = v["partner_candidates"].as_array() {
                                let addrs: Vec<SocketAddr> = arr
                                    .iter()
                                    .filter_map(|v| v.as_str()?.parse().ok())
                                    .collect();
                                if !addrs.is_empty() {
                                    *tunnel_state.partner_candidates.lock().unwrap() = addrs;
                                    tunnel_state.update_hole_punch_ready();
                                }
                            }
                        }
                    }
                }

                // Normal interval when connected.
                let has_key = tunnel_state.shared_key.lock().unwrap().is_some();
                let secs = if has_key { 30 } else { 3 };
                if interruptible_sleep(&control, secs) {
                    break;
                }
            } else {
                // Failed — backoff retry.
                tunnel_state.connected.store(false, Ordering::Relaxed);
                let secs = retry_secs(failures);
                failures = failures.saturating_add(1);
                eprintln!("[api] Registration failed, retrying in {secs}s");
                if interruptible_sleep(&control, secs) {
                    break;
                }
            }
        }

        // Unregister on shutdown.
        let token = control.token();
        let body = format!(r#"{{"token":"{token}","role":"host","peer_id":"{peer_id}"}}"#);
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
