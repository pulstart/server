//! Universal user-session bridge.
//!
//! The system service runs as the `st` user, which by design has no access
//! to any logged-in user's session resources (D-Bus bus, PulseAudio cookie,
//! Wayland socket, XDG runtime directory). The user-session tray companion
//! *does* have that access, and it sends a `SessionContext` over the
//! control socket whenever a session appears / changes / disappears.
//!
//! This module is intentionally not audio-specific. Anything that depends
//! on user-session state (audio, later: notifications, cursor themes,
//! clipboard, Wayland remote-desktop) subscribes to the bridge and reacts
//! when the context changes.
//!
//! Flow:
//!   tray companion (user session)          system service (`st` user)
//!   --------------------------------       ------------------------------
//!   probes env + sockets + cookie
//!   set_session_context JSON  ───────▶     ControlSocket handler
//!                                          SessionBridge::set(Some(ctx))
//!                                          notify all subscribers
//!                                                    │
//!                                                    ▼
//!                                          AudioPipeline::apply_context,
//!                                          <future subscribers...>
//!
//!   user logs out / tray exits
//!   clear_session_context  ──────────▶     SessionBridge::set(None)
//!                                          subscribers tear down

use serde::{Deserialize, Serialize};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Kinds of per-user audio daemon we know how to talk to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudioKind {
    Pulse,
    Pipewire,
}

/// Everything needed to capture the user's default desktop audio from
/// inside the system service. `cookie_hex` is only meaningful for the
/// PulseAudio protocol (PipeWire's pulse-bridge also accepts it).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioEndpoint {
    pub kind: AudioKind,
    /// libpulse `server` string, e.g. `"unix:/run/user/1000/pulse/native"`.
    pub server: String,
    /// Name of the PulseAudio monitor source for the default sink,
    /// e.g. `"alsa_output.pci-0000_0c_00.6.analog-stereo.monitor"`.
    /// `None` means "let libpulse pick the default source".
    pub monitor_source: Option<String>,
    /// PulseAudio auth cookie as a lowercase hex string. Optional if the
    /// daemon was configured to accept anonymous local connections.
    pub cookie_hex: Option<String>,
}

/// Metadata for a live PipeWire screencast node offered by the tray.
/// The actual file descriptor for the PipeWire connection is passed
/// out-of-band via `SCM_RIGHTS` on the control socket; the server stashes
/// the `OwnedFd` in [`SessionBridge::pipewire_fd`] so the capture backend
/// can dup and claim it later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipewireOffer {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
}

/// Snapshot of the active user session as observed by the tray companion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContext {
    pub uid: u32,
    pub username: String,
    pub xdg_runtime_dir: PathBuf,
    #[serde(default)]
    pub audio: Option<AudioEndpoint>,
    // Reserved for future bridges. Add fields with #[serde(default)] so
    // old tray companions keep working after a server upgrade.
    #[serde(default)]
    pub wayland_display: Option<String>,
    #[serde(default)]
    pub x11_display: Option<String>,
    #[serde(default)]
    pub dbus_session_bus_address: Option<String>,
    #[serde(default)]
    pub pipewire_offer: Option<PipewireOffer>,
}

type Subscriber = Box<dyn Fn(Option<&SessionContext>) + Send + Sync>;

/// Central hub. `Arc<SessionBridge>` can be shared across threads. Set
/// the current context from the control-socket thread; consumers register
/// a callback via `subscribe` and get woken on every transition.
pub struct SessionBridge {
    current: Mutex<Option<SessionContext>>,
    subscribers: Mutex<Vec<Subscriber>>,
    /// The live PipeWire fd offered by the tray (if any). Kept out of
    /// `SessionContext` because file descriptors aren't `Clone` and are
    /// transmitted out-of-band (SCM_RIGHTS on the control socket). The
    /// capture backend consumes it via [`take_pipewire_fd`] when starting.
    pipewire_fd: Mutex<Option<OwnedFd>>,
}

impl SessionBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(None),
            subscribers: Mutex::new(Vec::new()),
            pipewire_fd: Mutex::new(None),
        })
    }

    /// Hand a freshly-received PipeWire fd to the bridge. Replaces (and
    /// closes) any previous offer via `OwnedFd`'s Drop.
    pub fn set_pipewire_fd(&self, fd: Option<OwnedFd>) {
        *self.pipewire_fd.lock().unwrap() = fd;
    }

    /// Take ownership of the current PipeWire fd. Used by the capture
    /// backend when it starts a stream against the offered connection.
    #[allow(dead_code)] // consumed in Slice 2 when capture backend is wired
    pub fn take_pipewire_fd(&self) -> Option<OwnedFd> {
        self.pipewire_fd.lock().unwrap().take()
    }

    /// Snapshot the current context. `None` means no active user session.
    #[allow(dead_code)] // called from tray companion + `get_state` op
    pub fn current(&self) -> Option<SessionContext> {
        self.current.lock().unwrap().clone()
    }

    /// Replace (or clear) the current context and notify subscribers.
    /// Notification is best-effort: callbacks run synchronously on the
    /// calling thread, so they MUST NOT block. Spawn a worker thread
    /// inside the callback if the reaction is expensive (re-opening a
    /// PulseAudio stream, for example).
    pub fn set(&self, ctx: Option<SessionContext>) {
        {
            let mut cur = self.current.lock().unwrap();
            *cur = ctx.clone();
        }
        let subs = self.subscribers.lock().unwrap();
        for sub in subs.iter() {
            sub(ctx.as_ref());
        }
    }

    /// Register a callback. Called once immediately with the current
    /// context so the subscriber can initialize from wherever we are.
    pub fn subscribe<F>(&self, cb: F)
    where
        F: Fn(Option<&SessionContext>) + Send + Sync + 'static,
    {
        // Fire once with the current value so the subscriber can warm up.
        let snapshot = self.current.lock().unwrap().clone();
        cb(snapshot.as_ref());
        self.subscribers.lock().unwrap().push(Box::new(cb));
    }
}

// ---------------------------------------------------------------------------
// Global singleton — one bridge per process. Keeps us from having to thread
// an `Arc<SessionBridge>` through every constructor.
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Arc<SessionBridge>> = OnceLock::new();

pub fn global() -> Arc<SessionBridge> {
    Arc::clone(GLOBAL.get_or_init(SessionBridge::new))
}

/// Parse a hex string into raw bytes. Used on the server side to turn
/// the tray-transmitted `cookie_hex` back into the 256-byte cookie that
/// libpulse expects on disk.
pub fn decode_cookie_hex(hex: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if hex.len() % 2 != 0 {
        return Err("cookie hex length must be even".into());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(chunk).map_err(|_| "cookie hex utf8".to_string())?;
        let b = u8::from_str_radix(s, 16).map_err(|e| format!("cookie hex: {e}"))?;
        out.push(b);
    }
    Ok(out)
}
