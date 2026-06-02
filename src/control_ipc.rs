//! Control IPC: a Unix-domain socket that exposes the [`ServerControl`] surface
//! to an out-of-process client (the per-user tray agent in system-wide mode).
//!
//! In the default per-user install the tray holds an `Arc<ServerControl>`
//! directly. In system-wide mode the pipeline runs as a root system service
//! with no session bus, so the tray runs as a separate per-user process and
//! reaches the service over this socket instead.
//!
//! Wire format: one JSON-encoded [`Req`] per line, one JSON-encoded [`Resp`]
//! per line in reply. Connections are long-lived (the tray polls a snapshot on
//! a timer and issues setters on user action).

#![cfg(unix)]

use crate::api_client::ApiTunnelState;
use crate::encode_config::{Codec, QualityPreset};
use crate::server_control::{ServerControl, UpdateStateSnapshot};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// One connected client as seen over the wire (`SocketAddr` and `SystemTime`
/// flattened to serializable forms).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClientSnapshot {
    pub id: usize,
    pub addr: String,
    /// Seconds since the Unix epoch when the client connected.
    pub connected_at_unix: u64,
}

/// Serializable mirror of [`UpdateStateSnapshot`] (its `ReleaseInfo` payload is
/// flattened to the fields the tray actually renders).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum UpdateStateWire {
    Unsupported(String),
    Idle,
    Checking,
    UpToDate {
        version: String,
    },
    UpdateAvailable {
        version: String,
        asset_name: String,
        download_url: String,
    },
    Installing {
        version: String,
    },
    ClosingForUpdate {
        version: String,
    },
    Error(String),
}

/// Full control state in one round trip, so the tray renders a menu from a
/// single snapshot instead of N accessor calls.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StateSnapshot {
    pub version: usize,
    pub shutdown_requested: bool,
    pub token: String,
    pub peer_id: String,
    pub allow_new_connections: bool,
    /// 0 = auto, 1 = h264, 2 = hevc, 3 = av1 (matches the persisted encoding).
    pub forced_codec: u8,
    /// 0 = auto (adaptive bitrate), otherwise Kbps.
    pub forced_bitrate_kbps: u32,
    /// 0 = auto, 1 = low_latency, 2 = balanced, 3 = high_quality.
    pub forced_quality: u8,
    /// `None` when the API tunnel is not configured.
    pub api_connected: Option<bool>,
    pub update_state: UpdateStateWire,
    pub clients: Vec<ClientSnapshot>,
    /// Screen-wake request generation. The system-mode tray agent watches this
    /// across snapshots and runs the in-session display wake when it increments.
    #[serde(default)]
    pub wake_generation: u64,
}

#[derive(Serialize, Deserialize, Debug)]
enum Req {
    Snapshot,
    SetToken(String),
    SetAllowNewConnections(bool),
    SetForcedCodec(u8),
    SetForcedBitrateKbps(u32),
    SetForcedQuality(u8),
    BeginUpdateCheck,
    BeginUpdateInstall,
    RequestShutdown,
    RequestDisconnect(usize),
}

#[derive(Serialize, Deserialize, Debug)]
enum Resp {
    Snapshot(Box<StateSnapshot>),
    Bool(bool),
    Ack,
    Err(String),
}

/// Default control-socket path. `ST_CONTROL_SOCK` overrides it (used both as a
/// test hook and as the way `--system`/`--tray` agree on a non-default path).
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("ST_CONTROL_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/st-server/control.sock"))
}

fn codec_to_u8(c: Option<Codec>) -> u8 {
    match c {
        None => 0,
        Some(Codec::H264) => 1,
        Some(Codec::Hevc) => 2,
        Some(Codec::Av1) => 3,
    }
}

fn u8_to_codec(v: u8) -> Option<Codec> {
    match v {
        1 => Some(Codec::H264),
        2 => Some(Codec::Hevc),
        3 => Some(Codec::Av1),
        _ => None,
    }
}

fn quality_to_u8(q: Option<QualityPreset>) -> u8 {
    match q {
        None => 0,
        Some(QualityPreset::LowLatency) => 1,
        Some(QualityPreset::Balanced) => 2,
        Some(QualityPreset::HighQuality) => 3,
    }
}

fn u8_to_quality(v: u8) -> Option<QualityPreset> {
    match v {
        1 => Some(QualityPreset::LowLatency),
        2 => Some(QualityPreset::Balanced),
        3 => Some(QualityPreset::HighQuality),
        _ => None,
    }
}

fn update_state_to_wire(s: UpdateStateSnapshot) -> UpdateStateWire {
    match s {
        UpdateStateSnapshot::Unsupported(m) => UpdateStateWire::Unsupported(m),
        UpdateStateSnapshot::Idle => UpdateStateWire::Idle,
        UpdateStateSnapshot::Checking => UpdateStateWire::Checking,
        UpdateStateSnapshot::UpToDate { version } => UpdateStateWire::UpToDate { version },
        UpdateStateSnapshot::UpdateAvailable(r) => UpdateStateWire::UpdateAvailable {
            version: r.version,
            asset_name: r.asset_name,
            download_url: r.download_url,
        },
        UpdateStateSnapshot::Installing { version } => UpdateStateWire::Installing { version },
        UpdateStateSnapshot::ClosingForUpdate { version } => {
            UpdateStateWire::ClosingForUpdate { version }
        }
        UpdateStateSnapshot::Error(m) => UpdateStateWire::Error(m),
    }
}

fn build_snapshot(
    control: &Arc<ServerControl>,
    tunnel: &Option<Arc<ApiTunnelState>>,
) -> StateSnapshot {
    let clients = control
        .connected_clients()
        .into_iter()
        .map(|c| ClientSnapshot {
            id: c.id,
            addr: c.addr.to_string(),
            connected_at_unix: c
                .connected_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
        .collect();
    StateSnapshot {
        version: control.ui_version(),
        shutdown_requested: control.shutdown_requested(),
        token: control.token(),
        peer_id: control.peer_id().to_string(),
        allow_new_connections: control.allow_new_connections(),
        forced_codec: codec_to_u8(control.forced_codec()),
        forced_bitrate_kbps: control.forced_bitrate_kbps(),
        forced_quality: quality_to_u8(control.forced_quality()),
        api_connected: tunnel.as_ref().map(|t| t.is_connected()),
        update_state: update_state_to_wire(control.update_state()),
        clients,
        wake_generation: control.wake_generation(),
    }
}

fn handle_request(
    req: Req,
    control: &Arc<ServerControl>,
    tunnel: &Option<Arc<ApiTunnelState>>,
) -> Resp {
    match req {
        Req::Snapshot => Resp::Snapshot(Box::new(build_snapshot(control, tunnel))),
        Req::SetToken(t) => {
            control.set_token(t);
            Resp::Ack
        }
        Req::SetAllowNewConnections(b) => {
            control.set_allow_new_connections(b);
            Resp::Ack
        }
        Req::SetForcedCodec(v) => {
            control.set_forced_codec(u8_to_codec(v));
            Resp::Ack
        }
        Req::SetForcedBitrateKbps(k) => {
            control.set_forced_bitrate_kbps(k);
            Resp::Ack
        }
        Req::SetForcedQuality(v) => {
            control.set_forced_quality(u8_to_quality(v));
            Resp::Ack
        }
        Req::BeginUpdateCheck => {
            control.begin_update_check();
            Resp::Ack
        }
        Req::BeginUpdateInstall => {
            control.begin_update_install();
            Resp::Ack
        }
        Req::RequestShutdown => {
            control.request_shutdown();
            Resp::Ack
        }
        Req::RequestDisconnect(id) => Resp::Bool(control.request_disconnect(id)),
    }
}

/// Bind the control socket at `path` and serve requests until the process
/// exits. Blocks; run it on its own thread.
pub fn serve(
    control: Arc<ServerControl>,
    tunnel: Option<Arc<ApiTunnelState>>,
    path: &Path,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A stale socket from a previous run would make bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;
    // 0660: the owning service plus members of the socket's group (the tray
    // agent's user, added to the `st-server` group by the installer) may
    // connect. World access is denied — the token lives behind this socket.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    println!("[control-ipc] listening on {}", path.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let control = Arc::clone(&control);
                let tunnel = tunnel.clone();
                std::thread::spawn(move || handle_conn(stream, control, tunnel));
            }
            Err(err) => eprintln!("[control-ipc] accept error: {err}"),
        }
    }
    Ok(())
}

fn handle_conn(
    stream: UnixStream,
    control: Arc<ServerControl>,
    tunnel: Option<Arc<ApiTunnelState>>,
) {
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[control-ipc] clone failed: {err}");
            return;
        }
    };
    let reader = BufReader::new(read_half);
    let mut writer = stream;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Req>(&line) {
            Ok(req) => handle_request(req, &control, &tunnel),
            Err(err) => Resp::Err(format!("bad request: {err}")),
        };
        let mut out = serde_json::to_string(&resp)
            .unwrap_or_else(|_| "{\"Err\":\"serialize failed\"}".into());
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

/// Client side of the control socket, used by the per-user tray agent.
pub struct IpcClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl IpcClient {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            reader,
            writer: stream,
        })
    }

    fn call(&mut self, req: Req) -> io::Result<Resp> {
        let mut line = serde_json::to_string(&req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;

        let mut resp_line = String::new();
        if self.reader.read_line(&mut resp_line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "control socket closed",
            ));
        }
        serde_json::from_str(&resp_line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn expect_ack(&mut self, req: Req) -> io::Result<()> {
        match self.call(req)? {
            Resp::Ack => Ok(()),
            Resp::Err(e) => Err(io::Error::other(e)),
            other => Err(unexpected(&other)),
        }
    }

    pub fn snapshot(&mut self) -> io::Result<StateSnapshot> {
        match self.call(Req::Snapshot)? {
            Resp::Snapshot(s) => Ok(*s),
            other => Err(unexpected(&other)),
        }
    }

    pub fn set_token(&mut self, token: String) -> io::Result<()> {
        self.expect_ack(Req::SetToken(token))
    }

    pub fn set_allow_new_connections(&mut self, allow: bool) -> io::Result<()> {
        self.expect_ack(Req::SetAllowNewConnections(allow))
    }

    pub fn set_forced_codec(&mut self, codec: u8) -> io::Result<()> {
        self.expect_ack(Req::SetForcedCodec(codec))
    }

    pub fn set_forced_bitrate_kbps(&mut self, kbps: u32) -> io::Result<()> {
        self.expect_ack(Req::SetForcedBitrateKbps(kbps))
    }

    pub fn set_forced_quality(&mut self, quality: u8) -> io::Result<()> {
        self.expect_ack(Req::SetForcedQuality(quality))
    }

    pub fn begin_update_check(&mut self) -> io::Result<()> {
        self.expect_ack(Req::BeginUpdateCheck)
    }

    pub fn begin_update_install(&mut self) -> io::Result<()> {
        self.expect_ack(Req::BeginUpdateInstall)
    }

    pub fn request_shutdown(&mut self) -> io::Result<()> {
        self.expect_ack(Req::RequestShutdown)
    }

    pub fn request_disconnect(&mut self, id: usize) -> io::Result<bool> {
        match self.call(Req::RequestDisconnect(id))? {
            Resp::Bool(b) => Ok(b),
            other => Err(unexpected(&other)),
        }
    }
}

fn unexpected(resp: &Resp) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected control response: {resp:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_setters_round_trip() {
        // Isolate persisted state to a temp dir so the test does not touch the
        // real ~/.local/state/st config.
        let tmp = std::env::temp_dir().join(format!("st-ipc-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("XDG_STATE_HOME", &tmp);
        // Pin a known token so the assertion is deterministic.
        std::env::set_var("ST_TOKEN", "deadbeef");

        let control = ServerControl::new();
        let sock = tmp.join("control.sock");

        let server_control = Arc::clone(&control);
        let server_sock = sock.clone();
        std::thread::spawn(move || {
            let _ = serve(server_control, None, &server_sock);
        });

        // Wait for the socket to appear (serve binds on its own thread).
        let mut client = None;
        for _ in 0..200 {
            if let Ok(c) = IpcClient::connect(&sock) {
                client = Some(c);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut client = client.expect("connect to control socket");

        let snap = client.snapshot().expect("snapshot");
        assert_eq!(snap.token, "deadbeef");
        assert_eq!(snap.forced_bitrate_kbps, 0);
        assert!(snap.allow_new_connections);

        client.set_token("cafef00d".into()).expect("set token");
        client.set_forced_bitrate_kbps(25_000).expect("set bitrate");
        client.set_forced_codec(2).expect("set codec");
        client.set_allow_new_connections(false).expect("set allow");

        let snap = client.snapshot().expect("snapshot 2");
        assert_eq!(snap.token, "cafef00d");
        assert_eq!(snap.forced_bitrate_kbps, 25_000);
        assert_eq!(snap.forced_codec, 2);
        assert!(!snap.allow_new_connections);

        assert!(!client.request_disconnect(999).expect("disconnect unknown"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Regression guard for the screen-wake-on-connect path in system mode: the
    // root service (no session env) cannot unblank the display itself, so it
    // signals the in-session tray agent by bumping `wake_generation`. That
    // counter must survive the JSON snapshot round trip over the control socket,
    // and `request_wake()` must debounce so a reconnect burst doesn't spam the
    // compositor. If this regresses, a blanked remote never wakes (the original
    // "screen off => can't connect" bug) or wakes on every poll tick.
    #[test]
    fn wake_generation_propagates_and_debounces_over_socket() {
        let tmp = std::env::temp_dir().join(format!("st-wake-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("XDG_STATE_HOME", &tmp);

        let control = ServerControl::new();
        let sock = tmp.join("wake.sock");

        let server_control = Arc::clone(&control);
        let server_sock = sock.clone();
        std::thread::spawn(move || {
            let _ = serve(server_control, None, &server_sock);
        });

        let mut client = None;
        for _ in 0..200 {
            if let Ok(c) = IpcClient::connect(&sock) {
                client = Some(c);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut client = client.expect("connect to control socket");

        let base = client.snapshot().expect("snapshot").wake_generation;

        // First request bumps the generation, visible to the remote client.
        control.request_wake();
        assert_eq!(
            client.snapshot().expect("snapshot 2").wake_generation,
            base + 1,
            "wake_generation must increment over the wire"
        );

        // A second request inside the debounce window must NOT bump again.
        control.request_wake();
        assert_eq!(
            client.snapshot().expect("snapshot 3").wake_generation,
            base + 1,
            "back-to-back wake requests must debounce to a single bump"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
