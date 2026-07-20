use crate::server_control::ServerControl;
use rand::{rngs::OsRng, RngCore};
use st_protocol::tunnel::{
    derive_session_key, CryptoContext, SessionKeyContext, TunnelKeys, TunnelMode,
};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Shared ureq agent for all signaling calls. The default ureq agent has no
/// read timeout, so a black-holed or slow API host would hang the registration
/// thread forever. Bound connect/read/write so every signaling call fails fast.
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build()
    })
}

/// Refresh STUN-derived candidates if they're older than this. UDP NAT
/// mappings expire after ~30–120 s of silence, so 25 s gives margin to
/// re-probe before the partner sees a dead public ip:port.
const STUN_REFRESH_TTL: Duration = Duration::from_secs(25);
const MAX_API_TOKEN_LEN: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRequest {
    pub session_id: String,
    pub generation: u64,
    pub owner_peer_id: String,
    pub owner_lease_id: String,
    pub partner_peer_id: String,
    pub partner_lease_id: String,
    pub context: String,
}

#[derive(Clone)]
struct PartnerSnapshot {
    peer_id: String,
    lease_id: String,
    shared_secret: [u8; 32],
    candidates: Vec<SocketAddr>,
}

/// Shared state produced by the API registration thread.
/// The server runtime reads this to set up encrypted tunnels for incoming clients.
pub struct ApiTunnelState {
    /// Random process lease, distinct from the persisted stable peer ID.
    lease_id: String,
    tunnel_keys: Mutex<TunnelKeys>,
    /// Key, candidates, and identity from one validated partner lease.
    partner: Mutex<Option<PartnerSnapshot>>,
    /// Request-scoped context cache for idempotent signaling retries.
    crypto: Mutex<Option<([u8; 32], Arc<CryptoContext>)>>,
    /// Process-lifetime UDP socket used for STUN and punching.
    punch_socket: Mutex<Option<UdpSocket>>,
    /// Local candidates advertised to the API server.
    local_candidates: Mutex<Vec<String>>,
    /// Last time we refreshed `local_candidates` via STUN.
    last_stun: Mutex<Option<Instant>>,
    /// External `ip:port` granted by the router via NAT-PMP. Independent of
    /// the STUN-discovered mapping: the router gives us a static forwarding
    /// rule that survives idle periods AND works on symmetric NATs.
    /// Refreshed periodically by `spawn_port_mapping_task`.
    portmap_external: Mutex<Option<SocketAddr>>,
    /// Latest client requests, including the process generation that owns them.
    pending_client_punch: Mutex<Option<PendingRequest>>,
    pending_client_relay: Mutex<Option<PendingRequest>>,
    /// TCP relay port advertised by the API server (None = relay disabled).
    relay_port: Mutex<Option<u16>>,
    /// True while a punched session is active on the shared socket.
    punch_session_active: AtomicBool,
    /// True while a relayed TCP tunnel session is active.
    relay_session_active: AtomicBool,
    /// Set when shared key, partner candidates, and a punch socket are all present.
    hole_punch_ready: AtomicBool,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

impl ApiTunnelState {
    pub fn new() -> Self {
        let mut lease = [0u8; 32];
        OsRng.fill_bytes(&mut lease);
        Self {
            lease_id: base64_encode(&lease),
            tunnel_keys: Mutex::new(TunnelKeys::generate()),
            partner: Mutex::new(None),
            crypto: Mutex::new(None),
            punch_socket: Mutex::new(None),
            local_candidates: Mutex::new(Vec::new()),
            last_stun: Mutex::new(None),
            portmap_external: Mutex::new(None),
            pending_client_punch: Mutex::new(None),
            pending_client_relay: Mutex::new(None),
            relay_port: Mutex::new(None),
            punch_session_active: AtomicBool::new(false),
            relay_session_active: AtomicBool::new(false),
            hole_punch_ready: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        }
    }

    pub fn crypto_context(
        &self,
        request: &PendingRequest,
        mode: TunnelMode,
    ) -> Option<Arc<CryptoContext>> {
        let partner = self.partner.lock().unwrap().clone()?;
        if partner.peer_id != request.owner_peer_id
            || partner.lease_id != request.owner_lease_id
            || request.partner_lease_id != self.lease_id
        {
            return None;
        }
        let key = derive_session_key(
            &partner.shared_secret,
            &SessionKeyContext {
                request_context: &request.context,
                session_id: &request.session_id,
                mode,
                generation: request.generation,
                host_peer_id: &request.partner_peer_id,
                host_lease_id: &request.partner_lease_id,
                client_peer_id: &request.owner_peer_id,
                client_lease_id: &request.owner_lease_id,
            },
        )
        .ok()?;
        let mut cache = self.crypto.lock().unwrap();
        if let Some((cached_key, ctx)) = cache.as_ref() {
            if *cached_key == key {
                return Some(Arc::clone(ctx));
            }
        }
        let ctx = Arc::new(CryptoContext::new(key, true));
        *cache = Some((key, Arc::clone(&ctx)));
        Some(ctx)
    }

    pub fn partner_candidates(&self, request: &PendingRequest) -> Option<Vec<SocketAddr>> {
        let partner = self.partner.lock().unwrap();
        let partner = partner.as_ref()?;
        (partner.peer_id == request.owner_peer_id && partner.lease_id == request.owner_lease_id)
            .then(|| partner.candidates.clone())
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

    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    pub fn pending_client_punch(&self) -> Option<PendingRequest> {
        self.pending_client_punch.lock().unwrap().clone()
    }

    pub fn update_pending_client_punch(&self, request: Option<PendingRequest>) {
        *self.pending_client_punch.lock().unwrap() = request;
    }

    pub fn pending_client_relay(&self) -> Option<PendingRequest> {
        self.pending_client_relay.lock().unwrap().clone()
    }

    pub fn update_pending_client_relay(&self, request: Option<PendingRequest>) {
        *self.pending_client_relay.lock().unwrap() = request;
    }

    pub fn relay_port(&self) -> Option<u16> {
        *self.relay_port.lock().unwrap()
    }

    pub fn set_relay_port(&self, port: Option<u16>) {
        *self.relay_port.lock().unwrap() = port;
    }

    pub fn is_relay_session_active(&self) -> bool {
        self.relay_session_active.load(Ordering::Relaxed)
    }

    pub fn set_relay_session_active(&self, active: bool) {
        self.relay_session_active.store(active, Ordering::Relaxed);
    }

    pub fn is_punch_session_active(&self) -> bool {
        self.punch_session_active.load(Ordering::Relaxed)
    }

    pub fn set_punch_session_active(&self, active: bool) {
        self.punch_session_active.store(active, Ordering::Relaxed);
    }

    pub fn ensure_punch_socket(&self, listen_port: u16) -> Result<Vec<String>, String> {
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        let cached = self.local_candidates.lock().unwrap().clone();
        let stun_fresh = match *self.last_stun.lock().unwrap() {
            Some(t) => t.elapsed() < STUN_REFRESH_TTL,
            None => false,
        };
        // Reuse cached candidates if they're fresh OR if a live punched
        // session owns the socket (a STUN recv would steal its packets).
        let session_active = self.is_punch_session_active();
        if has_socket && !cached.is_empty() && (stun_fresh || session_active) {
            return Ok(self.augment_with_portmap(cached));
        }

        let mut socket_guard = self.punch_socket.lock().unwrap();
        if socket_guard.is_none() {
            let socket =
                UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind punch socket: {e}"))?;
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
        *self.last_stun.lock().unwrap() = Some(Instant::now());
        let augmented = self.augment_with_portmap(candidates);
        self.update_hole_punch_ready();
        Ok(augmented)
    }

    /// Append the NAT-PMP-granted external `ip:port` to the candidate list
    /// (if any), de-duplicating against existing entries. We keep this
    /// separate from STUN caching so the next `/api/candidates` upload
    /// picks up a freshly-renewed mapping without re-running STUN.
    fn augment_with_portmap(&self, mut candidates: Vec<String>) -> Vec<String> {
        if let Some(addr) = *self.portmap_external.lock().unwrap() {
            let c = addr.to_string();
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }
        candidates
    }

    /// Record (or clear) the NAT-PMP-granted external address. Called by the
    /// port-mapping renewal thread.
    pub fn set_portmap_external(&self, addr: Option<SocketAddr>) {
        let mut current = self.portmap_external.lock().unwrap();
        if *current != addr {
            *current = addr;
            if let Some(a) = addr {
                println!("[portmap] External mapping acquired: {a}");
            } else {
                println!("[portmap] External mapping cleared");
            }
        }
    }

    /// Local port that the punch socket is bound to, if known. Used by the
    /// port-mapping renewal thread as the "internal" port to map.
    pub fn punch_socket_port(&self) -> Option<u16> {
        let guard = self.punch_socket.lock().unwrap();
        guard
            .as_ref()
            .and_then(|s| s.local_addr().ok().map(|a| a.port()))
    }

    pub fn public_key_b64(&self) -> String {
        let keys = self.tunnel_keys.lock().unwrap();
        base64_encode(&keys.public_key_bytes())
    }

    fn shared_secret_from_partner_b64(&self, partner_b64: &str) -> Option<[u8; 32]> {
        let partner_bytes = base64_decode(partner_b64)?;
        if partner_bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&partner_bytes);
        Some({
            let keys = self.tunnel_keys.lock().unwrap();
            keys.derive_shared_key(&arr)
        })
    }

    fn set_partner_snapshot(
        &self,
        peer_id: String,
        lease_id: String,
        shared_secret: [u8; 32],
        candidates: Vec<SocketAddr>,
    ) {
        *self.partner.lock().unwrap() = Some(PartnerSnapshot {
            peer_id,
            lease_id,
            shared_secret,
            candidates,
        });
        self.update_hole_punch_ready();
    }

    pub fn clear_partner_state(&self) {
        *self.partner.lock().unwrap() = None;
        self.update_hole_punch_ready();
    }

    /// Check and update hole_punch_ready based on current state.
    pub fn update_hole_punch_ready(&self) {
        let has_partner = self
            .partner
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|partner| !partner.candidates.is_empty());
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        self.hole_punch_ready
            .store(has_partner && has_socket, Ordering::Relaxed);
    }
}

fn retry_secs(consecutive_failures: u32) -> u64 {
    match consecutive_failures {
        0 => 10,
        1 => 30,
        _ => 60,
    }
}

fn parse_pending_request(
    value: &serde_json::Value,
    session_id: &str,
    host_peer_id: &str,
    host_lease_id: &str,
) -> Option<PendingRequest> {
    if session_id.is_empty()
        || value["expected_partner_peer_id"].as_str()? != host_peer_id
        || value["partner_peer_id"].as_str()? != host_peer_id
        || value["partner_lease_id"].as_str()? != host_lease_id
    {
        return None;
    }
    Some(PendingRequest {
        session_id: session_id.to_string(),
        generation: value["generation"]
            .as_u64()
            .filter(|generation| *generation != 0)?,
        owner_peer_id: value["owner_peer_id"].as_str()?.to_string(),
        owner_lease_id: value["owner_lease_id"].as_str()?.to_string(),
        partner_peer_id: value["partner_peer_id"].as_str()?.to_string(),
        partner_lease_id: value["partner_lease_id"].as_str()?.to_string(),
        context: value["context"].as_str()?.to_string(),
    })
}

fn unregister_api_peer(api_url: &str, token: &str, peer_id: &str, lease_id: &str) -> bool {
    let body = serde_json::json!({
        "token": token,
        "role": "host",
        "peer_id": peer_id,
        "lease_id": lease_id,
    })
    .to_string();
    http_agent()
        .post(&format!("{api_url}/api/unregister"))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .is_ok()
}

fn post_api_value(
    api_url: &str,
    endpoint: &str,
    body: serde_json::Value,
) -> Option<serde_json::Value> {
    http_agent()
        .post(&format!("{api_url}/api/{endpoint}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .ok()?
        .into_string()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// Exchange a live client relay request for the host's short-lived one-use ticket.
pub enum RelayClaimError {
    Terminal(String),
    Transient(String),
}

fn relay_conflict_is_terminal(message: &str) -> bool {
    [
        "ownership changed",
        "partner identity changed",
        "partner process lease changed",
        "different live peer",
        "newer process lease",
        "live process lease",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

pub fn claim_relay_ticket(
    api_url: &str,
    token: &str,
    peer_id: &str,
    lease_id: &str,
    request: &PendingRequest,
) -> Result<String, RelayClaimError> {
    let body = serde_json::json!({
        "token": token,
        "role": "host",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": request.owner_peer_id,
        "expected_partner_lease_id": request.owner_lease_id,
        "generation": request.generation,
        "mode": "join",
    })
    .to_string();
    let response = http_agent()
        .post(&format!("{api_url}/api/relay"))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|error| match error {
            ureq::Error::Status(409, response) => {
                let status_text = response.status_text().to_string();
                let detail = response
                    .into_string()
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|value| value["error"].as_str().map(str::to_string))
                    .unwrap_or(status_text);
                let message = format!("claim relay ticket: {detail}");
                if relay_conflict_is_terminal(&detail) {
                    RelayClaimError::Terminal(message)
                } else {
                    RelayClaimError::Transient(message)
                }
            }
            error => RelayClaimError::Transient(format!("claim relay ticket: {error}")),
        })?;
    let text = response
        .into_string()
        .map_err(|error| RelayClaimError::Transient(format!("read relay response: {error}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| RelayClaimError::Transient(format!("parse relay response: {error}")))?;
    if value["session_id"].as_str() != Some(request.session_id.as_str())
        || value["mode"].as_str() != Some("relay")
        || value["generation"].as_u64() != Some(request.generation)
        || value["owner_peer_id"].as_str() != Some(request.owner_peer_id.as_str())
        || value["owner_lease_id"].as_str() != Some(request.owner_lease_id.as_str())
        || value["partner_peer_id"].as_str() != Some(peer_id)
        || value["partner_lease_id"].as_str() != Some(lease_id)
        || value["context"].as_str() != Some(request.context.as_str())
    {
        return Err(RelayClaimError::Terminal(
            "relay response did not match the lease-bound session signal".into(),
        ));
    }
    value["ticket"]
        .as_str()
        .filter(|ticket| !ticket.is_empty())
        .map(str::to_string)
        .ok_or_else(|| RelayClaimError::Transient("API server returned no relay ticket".into()))
}

/// Sleep for `secs` seconds, but wake early if shutdown is requested.
fn interruptible_sleep(control: &ServerControl, secs: u64) -> bool {
    interruptible_sleep_ms(control, secs.saturating_mul(1000))
}

/// Sleep for `millis` milliseconds, but wake early if shutdown is requested.
fn interruptible_sleep_ms(control: &ServerControl, millis: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(millis);
    while std::time::Instant::now() < deadline {
        if control.shutdown_requested() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50).min(Duration::from_millis(millis)));
    }
    false
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
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
/// Spawn a background thread that maintains a NAT-PMP UDP port mapping for
/// the punch socket. Re-acquires the lease at lifetime/2 to keep it live;
/// retries every 5 min when the gateway doesn't speak NAT-PMP. Clears the
/// candidate on hard failure so we don't keep advertising a dead address.
pub fn start_port_mapping(control: Arc<ServerControl>, tunnel_state: Arc<ApiTunnelState>) {
    std::thread::spawn(move || {
        // Wait until the punch socket exists.
        let mut internal_port: u16;
        loop {
            if control.shutdown_requested() {
                return;
            }
            if let Some(p) = tunnel_state.punch_socket_port() {
                internal_port = p;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        let mut consecutive_failures: u32 = 0;
        loop {
            if control.shutdown_requested() {
                return;
            }

            // The punch socket can in principle be rebound to a different port
            // over the process lifetime; re-read each cycle to stay accurate.
            if let Some(p) = tunnel_state.punch_socket_port() {
                internal_port = p;
            }

            let next_sleep = match st_protocol::portmap::try_acquire(internal_port) {
                Some(mapping) => {
                    println!(
                        "[portmap] {:?} mapping {} (lease {}s)",
                        mapping.method,
                        mapping.external_addr,
                        mapping.lifetime.as_secs()
                    );
                    tunnel_state.set_portmap_external(Some(mapping.external_addr));
                    consecutive_failures = 0;
                    // Renew at lifetime/2, clamped to [60s, 30min] so we don't
                    // spin too fast on tiny leases or wait too long on huge ones.
                    let half = mapping.lifetime / 2;
                    half.clamp(Duration::from_secs(60), Duration::from_secs(1800))
                }
                None => {
                    // Don't clobber a previously-good mapping just because one
                    // probe timed out — wait until two consecutive failures.
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= 2 {
                        tunnel_state.set_portmap_external(None);
                    }
                    // Back off: 1 min, then 5 min, then 15 min.
                    match consecutive_failures {
                        0..=1 => Duration::from_secs(60),
                        2..=4 => Duration::from_secs(300),
                        _ => Duration::from_secs(900),
                    }
                }
            };

            // Sleep but wake on shutdown.
            let deadline = Instant::now() + next_sleep;
            while Instant::now() < deadline {
                if control.shutdown_requested() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200).min(next_sleep));
            }
        }
    });
}

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
        let lease_id = tunnel_state.lease_id().to_string();
        let hostname = get_hostname();
        let mut failures: u32 = 0;
        let mut registered_token: Option<String> = None;

        loop {
            if control.shutdown_requested() {
                break;
            }

            let token = control.token();
            if registered_token
                .as_deref()
                .is_some_and(|registered| registered != token)
            {
                let old_token = registered_token
                    .take()
                    .expect("registered token disappeared during credential rotation");
                if !unregister_api_peer(&api_url, &old_token, &peer_id, &lease_id) {
                    eprintln!("[api] Failed to unregister previous token; it will expire");
                }
                tunnel_state.connected.store(false, Ordering::Relaxed);
                tunnel_state.clear_partner_state();
                tunnel_state.update_pending_client_punch(None);
                tunnel_state.update_pending_client_relay(None);
            }
            if token.is_empty() || token.len() > MAX_API_TOKEN_LEN {
                tunnel_state.connected.store(false, Ordering::Relaxed);
                eprintln!("[api] Token must contain 1..={MAX_API_TOKEN_LEN} bytes");
                if interruptible_sleep(&control, 10) {
                    break;
                }
                continue;
            }
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
                "lease_id": lease_id,
                "hostname": hostname,
                "candidates": local_candidates,
                "public_key": tunnel_state.public_key_b64(),
            })
            .to_string();
            let ok = http_agent()
                .post(&format!("{api_url}/api/register"))
                .set("Content-Type", "application/json")
                .send_string(&body)
                .is_ok();

            if ok {
                if failures > 0 || !tunnel_state.is_connected() {
                    println!("[api] Connected to API server");
                }
                failures = 0;
                tunnel_state.connected.store(true, Ordering::Relaxed);
                registered_token = Some(token.clone());

                let discovery =
                    post_api_value(&api_url, "session", serde_json::json!({"token": token}));
                let client_identity = discovery.as_ref().and_then(|value| {
                    Some((
                        value["client"]["peer_id"].as_str()?.to_string(),
                        value["client"]["lease_id"].as_str()?.to_string(),
                    ))
                });
                let client_joined = client_identity.is_some();
                if let Some(value) = discovery.as_ref() {
                    tunnel_state.set_relay_port(value["relay_port"].as_u64().map(|p| p as u16));
                }

                let synchronized = client_identity.and_then(|(client_peer_id, client_lease_id)| {
                    let session = post_api_value(
                        &api_url,
                        "session",
                        serde_json::json!({
                            "token": token,
                            "role": "host",
                            "peer_id": peer_id,
                            "lease_id": lease_id,
                            "expected_partner_peer_id": client_peer_id,
                            "expected_partner_lease_id": client_lease_id,
                        }),
                    )?;
                    if session["client"]["peer_id"].as_str() != Some(&client_peer_id)
                        || session["client"]["lease_id"].as_str() != Some(&client_lease_id)
                    {
                        return None;
                    }
                    let key = post_api_value(
                        &api_url,
                        "key",
                        serde_json::json!({
                            "token": token,
                            "role": "host",
                            "peer_id": peer_id,
                            "lease_id": lease_id,
                            "expected_partner_peer_id": client_peer_id,
                            "expected_partner_lease_id": client_lease_id,
                            "public_key": tunnel_state.public_key_b64(),
                        }),
                    )?;
                    let candidates = post_api_value(
                        &api_url,
                        "candidates",
                        serde_json::json!({
                            "token": token,
                            "role": "host",
                            "peer_id": peer_id,
                            "lease_id": lease_id,
                            "expected_partner_peer_id": client_peer_id,
                            "expected_partner_lease_id": client_lease_id,
                            "candidates": local_candidates,
                        }),
                    )?;
                    for response in [&key, &candidates] {
                        if response["partner_peer_id"].as_str() != Some(&client_peer_id)
                            || response["partner_lease_id"].as_str() != Some(&client_lease_id)
                        {
                            return None;
                        }
                    }
                    let shared_secret = tunnel_state
                        .shared_secret_from_partner_b64(key["partner_key"].as_str()?)?;
                    let addrs = candidates["partner_candidates"]
                        .as_array()?
                        .iter()
                        .filter_map(|value| value.as_str()?.parse().ok())
                        .collect();
                    Some((
                        session,
                        client_peer_id,
                        client_lease_id,
                        shared_secret,
                        addrs,
                    ))
                });

                if let Some((session, client_peer_id, client_lease_id, shared_secret, addrs)) =
                    synchronized
                {
                    let session_id = session["session_id"].as_str().unwrap_or_default();
                    tunnel_state.set_partner_snapshot(
                        client_peer_id,
                        client_lease_id,
                        shared_secret,
                        addrs,
                    );
                    tunnel_state.update_pending_client_punch(parse_pending_request(
                        &session["client_punch_request"],
                        session_id,
                        &peer_id,
                        &lease_id,
                    ));
                    tunnel_state.update_pending_client_relay(parse_pending_request(
                        &session["client_relay_request"],
                        session_id,
                        &peer_id,
                        &lease_id,
                    ));
                } else {
                    tunnel_state.clear_partner_state();
                    tunnel_state.update_pending_client_punch(None);
                    tunnel_state.update_pending_client_relay(None);
                }

                // Poll cadence:
                //   - 250 ms while the client is joined and no session is
                //     active yet — the client may post a /api/punch nonce at
                //     any moment, and we need the hole-punch task to see it
                //     fast so the host starts probing while the client is
                //     still inside its 10 s hole_punch window.
                //   - 1 s once a session is established (no urgency).
                //   - 3 s when no client is joined (idle polling).
                let session_active = tunnel_state.is_punch_session_active();
                let sleep_ms = if session_active {
                    1000
                } else if client_joined {
                    250
                } else {
                    3000
                };
                if interruptible_sleep_ms(&control, sleep_ms) {
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

        if let Some(token) = registered_token {
            if !unregister_api_peer(&api_url, &token, &peer_id, &lease_id) {
                eprintln!("[api] Failed to unregister from API server");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request_json() -> serde_json::Value {
        serde_json::json!({
            "generation": 7,
            "owner_peer_id": "client-peer",
            "owner_lease_id": "client-lease",
            "expected_partner_peer_id": "host-peer",
            "partner_peer_id": "host-peer",
            "partner_lease_id": "host-lease",
            "context": "request-context",
        })
    }

    #[test]
    fn absent_request_stays_absent() {
        assert!(parse_pending_request(
            &serde_json::Value::Null,
            "session",
            "host-peer",
            "host-lease"
        )
        .is_none());
    }

    #[test]
    fn api_session_generation_distinguishes_restart_requests() {
        let before = parse_pending_request(
            &request_json(),
            "api-session-before",
            "host-peer",
            "host-lease",
        )
        .unwrap();
        let after = parse_pending_request(
            &request_json(),
            "api-session-after",
            "host-peer",
            "host-lease",
        )
        .unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn only_relay_ownership_conflicts_are_terminal() {
        assert!(relay_conflict_is_terminal(
            "relay request generation or ownership changed"
        ));
        assert!(relay_conflict_is_terminal("partner process lease changed"));
        assert!(!relay_conflict_is_terminal("no live partner relay request"));
        assert!(!relay_conflict_is_terminal("relay disabled"));
    }
}
