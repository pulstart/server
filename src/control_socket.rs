//! Local control IPC for the user-session tray companion.
//!
//! Opens a Unix socket at `$RUNTIME_DIRECTORY/control.sock` (falling back to
//! `/run/st-server/control.sock`, then `$XDG_RUNTIME_DIR/st-server/control.sock`)
//! and speaks line-delimited JSON. One request object per line, one reply
//! object per line. No token/auth — file-system permissions are the gate:
//! the socket is `chmod 0660` and the containing `RuntimeDirectory` is
//! `0750`, so only the `st` user and members of the `st` group can reach it.
//!
//! Only started when `ST_STATE_DIR` is set (i.e. the system-service
//! deployment). User-mode runs keep using the in-process tray.

#![cfg(unix)]

use crate::encode_config::{Codec, QualityPreset};
use crate::server_control::{ConnectedClientSnapshot, ServerControl};
use crate::session_bridge::{self, PipewireOffer, SessionContext};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

const SOCKET_NAME: &str = "control.sock";

fn socket_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RUNTIME_DIRECTORY") {
        // systemd-provided path — already exists with the right owner+mode.
        return Some(PathBuf::from(dir).join(SOCKET_NAME));
    }
    // Fall back to the well-known path the system unit declares.
    let fallback = PathBuf::from("/run/st-server");
    if fallback.is_dir() {
        return Some(fallback.join(SOCKET_NAME));
    }
    // User-mode dev fallback (never used in production, handy for local runs).
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(rt).join("st-server");
        if std::fs::create_dir_all(&dir).is_ok() {
            return Some(dir.join(SOCKET_NAME));
        }
    }
    None
}

/// Start the control socket listener. Returns the bound path on success so
/// callers can log it; returns `None` (with a warning logged internally) if
/// we could not bind.
pub fn spawn(control: Arc<ServerControl>) -> Option<PathBuf> {
    let path = socket_path()?;
    // Best-effort cleanup of a stale socket from a crashed previous run.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[control-socket] bind {} failed: {err}", path.display());
            return None;
        }
    };

    // 0660 so members of the `st` group (the tray companion's user) can
    // connect. Containing RuntimeDirectory is 0750, so outsiders can't
    // even traverse to the socket.
    if let Err(err) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)) {
        eprintln!("[control-socket] chmod {} failed: {err}", path.display());
    }

    println!("[control-socket] Listening on {}", path.display());

    let path_for_return = path.clone();
    thread::Builder::new()
        .name("st-control-socket".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        let control = Arc::clone(&control);
                        thread::spawn(move || handle_client(stream, control));
                    }
                    Err(err) => {
                        eprintln!("[control-socket] accept error: {err}");
                        // Break to avoid tight loop if the listener is dead.
                        break;
                    }
                }
            }
        })
        .expect("spawn control-socket thread");

    Some(path_for_return)
}

fn handle_client(stream: UnixStream, control: Arc<ServerControl>) {
    let fd = stream.as_raw_fd();
    let mut writer = stream;

    // Line accumulator — recvmsg may deliver a partial line or several
    // concatenated lines. Ancillary fds are collected separately and
    // consumed in FIFO order by ops that need one (currently only
    // `offer_pipewire_fd`). This matches the tray's send pattern where a
    // single sendmsg carries both the fd via SCM_RIGHTS and the op bytes.
    let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut pending_fds: VecDeque<OwnedFd> = VecDeque::new();

    loop {
        let mut data = [0u8; 4096];
        let mut cmsg = [0u8; 256];
        let (n, fds) = match recvmsg_with_fds(fd, &mut data, &mut cmsg) {
            Ok((0, _)) => break, // peer closed
            Ok((n, fds)) => (n, fds),
            Err(err) => {
                eprintln!("[control-socket] recvmsg: {err}");
                break;
            }
        };
        for f in fds {
            pending_fds.push_back(f);
        }

        line_buf.extend_from_slice(&data[..n]);

        while let Some(pos) = line_buf.iter().position(|b| *b == b'\n') {
            let line_bytes = line_buf.drain(..=pos).collect::<Vec<_>>();
            let line = match std::str::from_utf8(&line_bytes[..pos]) {
                Ok(s) => s.trim(),
                Err(_) => continue,
            };
            if line.is_empty() {
                continue;
            }

            let reply = match serde_json::from_str::<Request>(line) {
                Ok(req) => handle_request(req, &control, &mut pending_fds),
                Err(err) => Response::err(format!("parse: {err}")),
            };

            let serialized = match serde_json::to_string(&reply) {
                Ok(s) => s,
                Err(err) => format!(r#"{{"ok":false,"error":"serialize: {err}"}}"#),
            };
            if writeln!(writer, "{serialized}").is_err() || writer.flush().is_err() {
                return;
            }
        }
    }
}

/// Receive a datagram of bytes + any SCM_RIGHTS file descriptors attached
/// to it. Returns `(bytes_read, fds)`. An Ok(0, _) return means the peer
/// closed the connection cleanly.
fn recvmsg_with_fds(
    fd: RawFd,
    data: &mut [u8],
    cmsg_buf: &mut [u8],
) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
    mhdr.msg_iov = &mut iov;
    mhdr.msg_iovlen = 1;
    mhdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    mhdr.msg_controllen = cmsg_buf.len() as _;

    // MSG_CMSG_CLOEXEC so any received fds land with FD_CLOEXEC set —
    // no need for a separate fcntl round-trip.
    let ret = unsafe { libc::recvmsg(fd, &mut mhdr, libc::MSG_CMSG_CLOEXEC) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = ret as usize;

    let mut fds = Vec::new();
    unsafe {
        let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&mhdr);
        while !cmsg_ptr.is_null() {
            let cmsg = &*cmsg_ptr;
            if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg_ptr) as *const RawFd;
                let header_len = libc::CMSG_LEN(0) as usize;
                let payload_len = cmsg.cmsg_len as usize - header_len;
                let count = payload_len / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let raw_fd = std::ptr::read_unaligned(data_ptr.add(i));
                    if raw_fd >= 0 {
                        fds.push(OwnedFd::from_raw_fd(raw_fd));
                    }
                }
            }
            cmsg_ptr = libc::CMSG_NXTHDR(&mhdr, cmsg_ptr);
        }
    }

    Ok((n, fds))
}

fn handle_request(
    req: Request,
    control: &Arc<ServerControl>,
    pending_fds: &mut VecDeque<OwnedFd>,
) -> Response {
    match req {
        Request::GetState => Response::State(build_state(control)),
        Request::SetCodec { codec } => {
            let parsed = parse_codec(&codec);
            control.set_forced_codec(parsed);
            Response::ok()
        }
        Request::SetBitrate { kbps } => {
            control.set_forced_bitrate_kbps(kbps);
            Response::ok()
        }
        Request::SetQuality { quality } => {
            let parsed = parse_quality(&quality);
            control.set_forced_quality(parsed);
            Response::ok()
        }
        Request::RegenToken => {
            let new_token = random_hex_16();
            control.set_token(new_token.clone());
            Response::Token { token: new_token }
        }
        Request::SetToken { token } => {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                return Response::err("token cannot be empty");
            }
            control.set_token(trimmed.to_string());
            Response::ok()
        }
        Request::DisconnectAll => {
            control.disconnect_all_clients();
            Response::ok()
        }
        Request::Shutdown => {
            control.request_shutdown();
            Response::ok()
        }
        Request::SetSessionContext { mut context } => {
            // If the tray sent a PulseAudio cookie, persist it to disk so
            // libpulse can pick it up via $PULSE_COOKIE (set below). We
            // write it under ST_STATE_DIR to stay inside the service's
            // ReadWritePaths sandbox.
            if let Some(ref audio) = context.audio {
                if let Some(ref hex) = audio.cookie_hex {
                    match persist_pulse_cookie(hex) {
                        Ok(path) => {
                            // Env is process-global; safe because the audio
                            // capture thread is torn down and restarted by
                            // the bridge subscriber on the same thread.
                            std::env::set_var("PULSE_COOKIE", &path);
                        }
                        Err(err) => {
                            eprintln!("[control-socket] persist pulse cookie: {err}");
                            // Strip the cookie — libpulse will try anon auth.
                            if let Some(ref mut a) = context.audio {
                                a.cookie_hex = None;
                            }
                        }
                    }
                }
            }
            session_bridge::global().set(Some(context));
            Response::ok()
        }
        Request::ClearSessionContext => {
            session_bridge::global().set(None);
            session_bridge::global().set_pipewire_fd(None);
            Response::ok()
        }
        Request::OfferPipewireFd { node_id, width, height } => {
            let Some(fd) = pending_fds.pop_front() else {
                return Response::err(
                    "offer_pipewire_fd requires an SCM_RIGHTS attachment with the PipeWire fd",
                );
            };
            eprintln!(
                "[control-socket] received PipeWire offer fd={} node={} size={}x{}",
                fd.as_raw_fd(),
                node_id,
                width,
                height
            );
            let bridge = session_bridge::global();
            bridge.set_pipewire_fd(Some(fd));
            // Stamp the offer into the current session context too so
            // subscribers (capture-backend selector, etc.) see the update.
            let new_ctx = match bridge.current() {
                Some(mut ctx) => {
                    ctx.pipewire_offer = Some(PipewireOffer { node_id, width, height });
                    ctx
                }
                None => SessionContext {
                    uid: unsafe { libc::getuid() },
                    username: "unknown".into(),
                    xdg_runtime_dir: PathBuf::from("/run/user/0"),
                    audio: None,
                    wayland_display: None,
                    x11_display: None,
                    dbus_session_bus_address: None,
                    pipewire_offer: Some(PipewireOffer { node_id, width, height }),
                },
            };
            bridge.set(Some(new_ctx));
            Response::ok()
        }
        Request::ClearPipewireFd => {
            let bridge = session_bridge::global();
            bridge.set_pipewire_fd(None);
            if let Some(mut ctx) = bridge.current() {
                ctx.pipewire_offer = None;
                bridge.set(Some(ctx));
            }
            Response::ok()
        }
    }
}

fn build_state(control: &ServerControl) -> StatePayload {
    StatePayload {
        token: control.token(),
        peer_id: control.peer_id().to_string(),
        codec: codec_label(control.forced_codec()),
        bitrate_kbps: control.forced_bitrate_kbps(),
        quality: quality_label(control.forced_quality()),
        accepting_clients: control.allow_new_connections(),
        clients: control
            .connected_clients()
            .into_iter()
            .map(client_to_json)
            .collect(),
    }
}

fn client_to_json(snap: ConnectedClientSnapshot) -> ClientPayload {
    ClientPayload {
        id: snap.id,
        addr: addr_str(snap.addr),
        connected_unix: snap
            .connected_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

fn addr_str(addr: SocketAddr) -> String {
    addr.to_string()
}

fn codec_label(c: Option<Codec>) -> String {
    match c {
        Some(Codec::H264) => "h264",
        Some(Codec::Hevc) => "hevc",
        Some(Codec::Av1) => "av1",
        None => "auto",
    }
    .into()
}

fn parse_codec(s: &str) -> Option<Codec> {
    match s.trim().to_lowercase().as_str() {
        "h264" => Some(Codec::H264),
        "hevc" | "h265" => Some(Codec::Hevc),
        "av1" => Some(Codec::Av1),
        _ => None,
    }
}

fn quality_label(q: Option<QualityPreset>) -> String {
    match q {
        Some(QualityPreset::LowLatency) => "low_latency",
        Some(QualityPreset::Balanced) => "balanced",
        Some(QualityPreset::HighQuality) => "high_quality",
        None => "auto",
    }
    .into()
}

fn parse_quality(s: &str) -> Option<QualityPreset> {
    match s.trim().to_lowercase().as_str() {
        "low_latency" => Some(QualityPreset::LowLatency),
        "balanced" => Some(QualityPreset::Balanced),
        "high_quality" => Some(QualityPreset::HighQuality),
        _ => None,
    }
}

fn persist_pulse_cookie(hex: &str) -> Result<PathBuf, String> {
    let bytes = session_bridge::decode_cookie_hex(hex)?;
    let dir = std::env::var_os("ST_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/st-server"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join("pulse-cookie");
    std::fs::write(&path, &bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(path)
}

fn random_hex_16() -> String {
    use rand::Rng;
    let bytes: [u8; 16] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    GetState,
    SetCodec { codec: String },
    SetBitrate { kbps: u32 },
    SetQuality { quality: String },
    RegenToken,
    SetToken { token: String },
    DisconnectAll,
    Shutdown,
    SetSessionContext { context: SessionContext },
    ClearSessionContext,
    /// Tray offers its portal-derived PipeWire connection to the server.
    /// Must be sent with an SCM_RIGHTS attachment carrying the PipeWire fd.
    OfferPipewireFd { node_id: u32, width: u32, height: u32 },
    /// Tray revokes the offer (session teardown / portal session died).
    ClearPipewireFd,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Ok { ok: bool },
    Token { token: String },
    State(StatePayload),
    Err { ok: bool, error: String },
}

impl Response {
    fn ok() -> Self {
        Response::Ok { ok: true }
    }
    fn err(msg: impl Into<String>) -> Self {
        Response::Err {
            ok: false,
            error: msg.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct StatePayload {
    token: String,
    peer_id: String,
    codec: String,
    bitrate_kbps: u32,
    quality: String,
    accepting_clients: bool,
    clients: Vec<ClientPayload>,
}

#[derive(Debug, Serialize)]
struct ClientPayload {
    id: usize,
    addr: String,
    connected_unix: u64,
}
