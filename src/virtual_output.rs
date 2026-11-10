//! Outputs the compositor creates for a client that has more monitors than
//! the server: a real desktop output (windows can be moved onto it) with no
//! connector behind it.
//!
//! Only KWin can do this today, through `zkde_screencast_unstable_v1`: the
//! output exists for as long as the Wayland client that asked for it keeps
//! the stream open, and its picture arrives as a PipeWire node rather than a
//! scanout, so its pipeline captures with PipeWire instead of KMS.
//!
//! Opt-in (`ST_VIRTUAL_OUTPUT=1`) until `live_virtual_output` has been run
//! against the compositor it is deployed on.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

mod kde {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(clippy::all)]

    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use kde::zkde_screencast_stream_unstable_v1::{self as stream, ZkdeScreencastStreamUnstableV1};
use kde::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

const SCREENCAST: &str = "zkde_screencast_unstable_v1";
/// `stream_virtual_output` arrived in version 2; `serial` events in 6.
const MIN_VERSION: u32 = 2;
const MAX_VERSION: u32 = 6;
/// Cursor as stream metadata, so clients draw it themselves.
const POINTER_METADATA: u32 = 4;
/// A session that reconnects (network blip, codec fallback) finds its output
/// still there: destroying and recreating it would reshuffle the desktop.
const LINGER: Duration = Duration::from_secs(10);

/// PipeWire node carrying a virtual output's picture.
#[derive(Clone, Debug)]
pub struct PipeWireNode {
    pub node_id: u32,
    /// Daemon to reach it through; `None` is this process's own.
    pub remote: Option<String>,
    pub width: u32,
    pub height: u32,
}

struct VirtualOutput {
    /// Compositor output name, as the desktop arrangement lists it.
    name: String,
    node: PipeWireNode,
    stop: Arc<AtomicBool>,
    client: Option<std::thread::JoinHandle<()>>,
}

impl VirtualOutput {
    /// The compositor ended the stream (output removed, session gone).
    fn closed(&self) -> bool {
        self.client.as_ref().is_none_or(|c| c.is_finished())
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(client) = self.client.take() {
            let _ = client.join();
        }
        println!("[virtual-output] removed {}", self.name);
    }
}

struct Entry {
    output: VirtualOutput,
    leases: usize,
    idle_since: Option<Instant>,
}

/// The virtual outputs this server created.
#[derive(Default)]
pub struct VirtualOutputs {
    entries: Mutex<Vec<Entry>>,
    next_index: AtomicU32,
}

/// Keeps a virtual output alive for one session.
pub struct VirtualLease {
    outputs: Arc<VirtualOutputs>,
    pub output_id: u32,
}

impl Drop for VirtualLease {
    fn drop(&mut self) {
        let mut entries = self.outputs.entries.lock().unwrap();
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| crate::outputs::output_id(&entry.output.name) == self.output_id)
        {
            entry.leases -= 1;
            if entry.leases == 0 {
                entry.idle_since = Some(Instant::now());
            }
        }
    }
}

impl VirtualOutputs {
    /// An output of `width`x`height`: an idle one of that size if a session
    /// just left it, else a new one.
    pub fn acquire(
        self: &Arc<Self>,
        width: u32,
        height: u32,
        refresh_millihz: u32,
    ) -> Result<VirtualLease, String> {
        if !enabled() {
            return Err("virtual outputs are off (ST_VIRTUAL_OUTPUT=1 enables them)".into());
        }
        let mut entries = self.entries.lock().unwrap();
        let index = match entries.iter().position(|entry| {
            entry.leases == 0
                && !entry.output.closed()
                && (entry.output.node.width, entry.output.node.height) == (width, height)
        }) {
            Some(index) => index,
            None => {
                let name = format!("st-{}", self.next_index.fetch_add(1, Ordering::Relaxed) + 1);
                let output = create(&name, width, height)?;
                set_refresh_rate(&output.name, width, height, refresh_millihz);
                entries.push(Entry {
                    output,
                    leases: 0,
                    idle_since: None,
                });
                entries.len() - 1
            }
        };
        let entry = &mut entries[index];
        entry.leases += 1;
        entry.idle_since = None;
        Ok(VirtualLease {
            outputs: Arc::clone(self),
            output_id: crate::outputs::output_id(&entry.output.name),
        })
    }

    /// Keep an existing virtual output for one more session: every session
    /// viewing it holds a lease, whoever asked for it first.
    pub fn lease(self: &Arc<Self>, output_id: u32) -> Option<VirtualLease> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.iter_mut().find(|entry| {
            crate::outputs::output_id(&entry.output.name) == output_id && !entry.output.closed()
        })?;
        entry.leases += 1;
        entry.idle_since = None;
        Some(VirtualLease {
            outputs: Arc::clone(self),
            output_id,
        })
    }

    pub fn names(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .map(|entry| entry.output.name.clone())
            .collect()
    }

    pub fn node(&self, output_id: u32) -> Option<PipeWireNode> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .find(|entry| crate::outputs::output_id(&entry.output.name) == output_id)
            .map(|entry| entry.output.node.clone())
    }

    /// Remove outputs nobody has used for [`LINGER`], and ones the compositor
    /// closed. Returns whether the set changed.
    pub fn reap(&self) -> bool {
        let removed: Vec<Entry> = {
            let mut entries = self.entries.lock().unwrap();
            let gone = |entry: &Entry| {
                let expired = entry
                    .idle_since
                    .is_some_and(|since| since.elapsed() >= LINGER);
                expired || entry.leases == 0 && entry.output.closed()
            };
            let (removed, kept) = std::mem::take(&mut *entries).into_iter().partition(gone);
            *entries = kept;
            removed
        };
        // Removing an output waits for its Wayland client; not under the lock.
        !removed.is_empty()
    }
}

fn enabled() -> bool {
    matches!(
        std::env::var("ST_VIRTUAL_OUTPUT").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// Whether a client may ask for a virtual output right now.
pub fn supported() -> bool {
    enabled()
        && connect()
            .and_then(|session| probe(&session.connection))
            .is_ok()
}

struct Session {
    connection: Connection,
    /// PipeWire daemon of that session when it is not this process's own.
    pipewire_remote: Option<String>,
}

/// Connect to the compositor of the desktop being streamed. A system service
/// has no session of its own and reaches the active user's.
fn connect() -> Result<Session, String> {
    if std::env::var_os("ST_SYSTEM_MODE").is_none() {
        return Ok(Session {
            connection: Connection::connect_to_env()
                .map_err(|e| format!("no Wayland compositor: {e}"))?,
            pipewire_remote: None,
        });
    }
    let run_dir = crate::session_follow::active_runtime_dir()
        .ok_or_else(|| "no active user session".to_string())?;
    let socket = std::fs::read_dir(&run_dir)
        .map_err(|e| format!("{}: {e}", run_dir.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        })
        .ok_or_else(|| format!("no Wayland socket in {}", run_dir.display()))?;
    let stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    Ok(Session {
        connection: Connection::from_socket(stream).map_err(|e| format!("Wayland: {e}"))?,
        pipewire_remote: Some(run_dir.join("pipewire-0").to_string_lossy().into_owned()),
    })
}

#[derive(Default)]
struct Client {
    screencast: Option<(u32, u32)>,
    outcome: Option<Result<u32, String>>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == SCREENCAST {
                state.screencast = Some((name, version));
            }
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: stream::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            stream::Event::Created { node } => state.outcome = Some(Ok(node)),
            stream::Event::Failed { error } => state.outcome = Some(Err(error)),
            stream::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

fn probe(connection: &Connection) -> Result<(), String> {
    let mut queue = connection.new_event_queue();
    let mut client = Client::default();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    queue
        .roundtrip(&mut client)
        .map_err(|e| format!("Wayland roundtrip: {e}"))?;
    match client.screencast {
        Some((_, version)) if version >= MIN_VERSION => Ok(()),
        Some((_, version)) => Err(format!("{SCREENCAST} v{version} cannot create outputs")),
        None => Err(format!("compositor has no {SCREENCAST}")),
    }
}

/// Ask the compositor for an output and wait until its stream exists. The
/// returned output owns the Wayland client that keeps it alive.
fn create(name: &str, width: u32, height: u32) -> Result<VirtualOutput, String> {
    let session = connect()?;
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let client = {
        let (name, stop) = (name.to_string(), Arc::clone(&stop));
        let connection = session.connection;
        std::thread::Builder::new()
            .name("st-virtual-output".into())
            .spawn(move || run_client(connection, &name, width, height, ready_tx, &stop))
            .map_err(|e| format!("spawn: {e}"))?
    };
    let node_id = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(node_id)) => node_id,
        outcome => {
            stop.store(true, Ordering::Release);
            let _ = client.join();
            return Err(match outcome {
                Ok(Err(error)) => format!("compositor refused the virtual output: {error}"),
                _ => "compositor did not create the virtual output".into(),
            });
        }
    };
    let output = VirtualOutput {
        name: format!("Virtual-{name}"),
        node: PipeWireNode {
            node_id,
            remote: session.pipewire_remote,
            width,
            height,
        },
        stop,
        client: Some(client),
    };
    println!(
        "[virtual-output] created {} {width}x{height} (PipeWire node {node_id})",
        output.name
    );
    Ok(output)
}

fn run_client(
    connection: Connection,
    name: &str,
    width: u32,
    height: u32,
    ready: crossbeam_channel::Sender<Result<u32, String>>,
    stop: &AtomicBool,
) {
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    let mut client = Client::default();
    let registry = connection.display().get_registry(&handle, ());
    if let Err(error) = queue.roundtrip(&mut client) {
        let _ = ready.send(Err(format!("Wayland roundtrip: {error}")));
        return;
    }
    let Some((global, version)) = client.screencast.filter(|&(_, v)| v >= MIN_VERSION) else {
        let _ = ready.send(Err(format!("compositor has no usable {SCREENCAST}")));
        return;
    };
    let screencast: ZkdeScreencastUnstableV1 =
        registry.bind(global, version.min(MAX_VERSION), &handle, ());
    let stream = screencast.stream_virtual_output(
        name.to_string(),
        width as i32,
        height as i32,
        1.0,
        POINTER_METADATA,
        &handle,
        (),
    );

    let mut announced = false;
    while !stop.load(Ordering::Acquire) && !client.closed {
        if queue.dispatch_pending(&mut client).is_err() || connection.flush().is_err() {
            break;
        }
        if !announced {
            if let Some(outcome) = client.outcome.take() {
                announced = true;
                let failed = outcome.is_err();
                let _ = ready.send(outcome);
                if failed {
                    break;
                }
            }
        }
        let Some(guard) = connection.prepare_read() else {
            continue;
        };
        let mut poll = libc::pollfd {
            fd: guard.connection_fd().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` is one valid pollfd for the duration of the call.
        let readable = unsafe { libc::poll(&mut poll, 1, 200) } > 0;
        if readable && guard.read().is_err() {
            break;
        }
    }
    // Closing the stream is what makes the compositor remove the output.
    stream.close();
    screencast.destroy();
    let _ = connection.flush();
}

/// A virtual output starts at 60 Hz; give it the client's rate with a custom
/// mode. Best effort: a refused mode leaves the stream at 60 Hz.
fn set_refresh_rate(output: &str, width: u32, height: u32, refresh_millihz: u32) {
    if refresh_millihz <= 60_500 {
        return;
    }
    let hz = (refresh_millihz + 500) / 1000;
    let run = |arg: String| {
        let mut command = crate::host_tools::host_command("kscreen-doctor");
        command
            .arg(&arg)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if std::env::var_os("ST_SYSTEM_MODE").is_some()
            && crate::session_follow::run_as_active_user(&mut command).is_none()
        {
            return false;
        }
        command.status().is_ok_and(|status| status.success())
    };
    let added = run(format!(
        "output.{output}.addCustomMode.{width}.{height}.{refresh_millihz}.full"
    ));
    let selected = added && run(format!("output.{output}.mode.{width}x{height}@{hz}"));
    if selected {
        println!("[virtual-output] {output} runs at {hz} Hz");
    } else {
        eprintln!("[virtual-output] {output} stays at 60 Hz: the compositor refused {hz} Hz");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether this desktop's compositor can create outputs; creates nothing.
    /// `cargo test compositor_offers_virtual_outputs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn compositor_offers_virtual_outputs() {
        let session = connect().expect("compositor");
        probe(&session.connection).expect("screencast protocol");
    }

    /// Creates a real output on the running desktop and removes it again.
    /// `ST_TEST_VIRTUAL_OUTPUT=1 cargo test live_virtual_output -- --nocapture`
    #[test]
    fn live_virtual_output() {
        if std::env::var_os("ST_TEST_VIRTUAL_OUTPUT").is_none() {
            return;
        }
        std::env::set_var("ST_VIRTUAL_OUTPUT", "1");
        let session = connect().expect("compositor");
        probe(&session.connection).expect("screencast protocol");
        let outputs = Arc::new(VirtualOutputs::default());
        let lease = outputs.acquire(1280, 720, 60_000).expect("virtual output");
        let name = outputs.names().pop().unwrap();
        let node = outputs.node(lease.output_id).expect("node");
        assert!(node.node_id != 0 && (node.width, node.height) == (1280, 720));

        // The compositor must list it as a desktop output with a place.
        let listed = |name: &str| {
            crate::outputs::query_layout()
                .is_some_and(|layout| layout.iter().any(|output| output.name == name))
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !listed(&name) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(listed(&name), "{name} is not in the desktop arrangement");

        // Same size, right after the session left: the output is reused.
        drop(lease);
        assert!(!outputs.reap());
        let again = outputs.acquire(1280, 720, 60_000).expect("reuse");
        assert_eq!(outputs.names(), vec![name.clone()]);
        drop(again);

        outputs.entries.lock().unwrap().clear();
        let deadline = Instant::now() + Duration::from_secs(3);
        while listed(&name) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!listed(&name), "{name} outlived its stream");
    }
}
