use crossbeam_channel::{Receiver, Sender};
use st_protocol::ControlMessage;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CLIPBOARD_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);
const CLIPBOARD_SEND_MIN_INTERVAL: Duration = Duration::from_millis(500);
const FILE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_CLIPBOARD_TEXT_BYTES: usize = u16::MAX as usize;
const REMOTE_CHANNEL_BOUND: usize = 8;

fn trace_enabled() -> bool {
    std::env::var_os("ST_TRACE").is_some()
}

fn clamp_clipboard_text(text: &str) -> String {
    if text.len() <= MAX_CLIPBOARD_TEXT_BYTES {
        return text.to_string();
    }

    let mut end = MAX_CLIPBOARD_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Tracks paths that were placed into the clipboard by us (received files).
/// Used to prevent echo: when we receive a file and put it in clipboard,
/// the file detection loop must not re-send it.
pub type SuppressedPaths = Arc<Mutex<HashSet<PathBuf>>>;

pub fn new_suppressed_paths() -> SuppressedPaths {
    Arc::new(Mutex::new(HashSet::new()))
}

/// System mode: the root service has no display, so the in-session tray agent
/// mirrors the user's clipboard into this over the control socket.
#[derive(Default)]
pub struct SessionClipboard {
    state: Mutex<(u64, String)>,
}

impl SessionClipboard {
    /// Current (generation, text); empty text means nothing to share.
    pub fn get(&self) -> (u64, String) {
        self.state.lock().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.state.lock().unwrap().0
    }

    /// Store `text`, returning the new generation (unchanged if identical).
    pub fn set(&self, text: String) -> u64 {
        let mut state = self.state.lock().unwrap();
        if state.0 == 0 || state.1 != text {
            state.0 += 1;
            state.1 = text;
        }
        state.0
    }

    /// Forget the text once no client is left to share it with.
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.1.is_empty() {
            state.0 += 1;
            state.1.clear();
        }
    }
}

enum Board {
    Os(arboard::Clipboard),
    Session(Arc<SessionClipboard>),
}

impl Board {
    fn get_text(&mut self) -> Result<String, String> {
        match self {
            Board::Os(board) => board.get_text().map_err(|e| e.to_string()),
            Board::Session(session) => Some(session.get().1)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "empty".into()),
        }
    }

    fn set_text(&mut self, text: String) -> Result<(), String> {
        match self {
            Board::Os(board) => board.set_text(text).map_err(|e| e.to_string()),
            Board::Session(session) => {
                session.set(text);
                Ok(())
            }
        }
    }
}

pub struct ClipboardSync {
    remote_tx: Sender<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    file_thread: Option<JoinHandle<()>>,
}

impl ClipboardSync {
    /// `session`: system mode's tray-mirrored clipboard instead of the OS one
    /// (file detection needs the OS clipboard, so it's off there).
    pub fn start_with_file_detection(
        label: &'static str,
        outbound_tx: Sender<ControlMessage>,
        file_tx: Sender<PathBuf>,
        suppressed: SuppressedPaths,
        session: Option<Arc<SessionClipboard>>,
    ) -> Self {
        let (remote_tx, remote_rx) = crossbeam_channel::bounded::<String>(REMOTE_CHANNEL_BOUND);
        let stop = Arc::new(AtomicBool::new(false));

        let file_thread = session.is_none().then(|| {
            let stop_flag = Arc::clone(&stop);
            thread::spawn(move || run_file_clipboard_loop(file_tx, stop_flag, suppressed))
        });
        let stop_flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            run_clipboard_loop(label, outbound_tx, remote_rx, stop_flag, session);
        });

        Self {
            remote_tx,
            stop,
            thread: Some(thread),
            file_thread,
        }
    }

    pub fn set_remote_text(&self, text: String) {
        let _ = self.remote_tx.try_send(text);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.file_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_clipboard_loop(
    label: &'static str,
    outbound_tx: Sender<ControlMessage>,
    remote_rx: Receiver<String>,
    stop: Arc<AtomicBool>,
    session: Option<Arc<SessionClipboard>>,
) {
    let mut clipboard: Option<Board> = session.map(Board::Session);
    let mut last_log = Instant::now() - CLIPBOARD_ERROR_LOG_INTERVAL;
    let mut pending_remote: Option<String> = None;
    let mut last_synced_text: Option<String> = None;
    let mut last_sent = Instant::now() - CLIPBOARD_SEND_MIN_INTERVAL;

    while !stop.load(Ordering::Relaxed) {
        while let Ok(text) = remote_rx.try_recv() {
            pending_remote = Some(clamp_clipboard_text(&text));
        }

        if clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(instance) => clipboard = Some(Board::Os(instance)),
                Err(err) => {
                    if last_log.elapsed() >= CLIPBOARD_ERROR_LOG_INTERVAL {
                        eprintln!("[clipboard] {label}: unavailable: {err}");
                        last_log = Instant::now();
                    }
                    thread::sleep(CLIPBOARD_POLL_INTERVAL);
                    continue;
                }
            }
        }

        let mut applied_remote = false;
        if let Some(text) = pending_remote.clone() {
            match clipboard.as_mut().unwrap().set_text(text.clone()) {
                Ok(()) => {
                    if trace_enabled() {
                        eprintln!(
                            "[clipboard] {label}: applied remote text ({} bytes)",
                            text.len()
                        );
                    }
                    last_synced_text = Some(text);
                    pending_remote = None;
                    applied_remote = true;
                }
                Err(err) => {
                    if last_log.elapsed() >= CLIPBOARD_ERROR_LOG_INTERVAL {
                        eprintln!("[clipboard] {label}: set failed: {err}");
                        last_log = Instant::now();
                    }
                    clipboard = None;
                    thread::sleep(CLIPBOARD_POLL_INTERVAL);
                    continue;
                }
            }
        }

        // Poll local clipboard for text changes and sync outbound.
        // Skip this tick if we just applied remote text — reading the clipboard
        // back immediately can race with the OS clipboard commit and return stale
        // content, which would echo the old text back to the sender.
        if applied_remote {
            thread::sleep(CLIPBOARD_POLL_INTERVAL);
            continue;
        }
        if let Ok(text) = clipboard.as_mut().unwrap().get_text() {
            let text = clamp_clipboard_text(&text);
            let changed = last_synced_text.as_deref() != Some(text.as_str());
            let rate_ok = last_sent.elapsed() >= CLIPBOARD_SEND_MIN_INTERVAL;
            if changed && rate_ok {
                if outbound_tx
                    .send(ControlMessage::ClipboardText(text.clone()))
                    .is_err()
                {
                    break;
                }
                if trace_enabled() {
                    eprintln!(
                        "[clipboard] {label}: sent local text ({} bytes)",
                        text.len()
                    );
                }
                last_synced_text = Some(text);
                last_sent = Instant::now();
            }
        }
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
    }
}

/// The service's [`SessionClipboard`] as seen from the tray agent.
pub trait SessionClipboardRemote {
    /// A client is connected (the clipboard is only read while one is).
    fn active(&self) -> bool;
    /// Latest known generation (from the polled snapshot; no round trip).
    fn generation(&self) -> u64;
    fn get(&self) -> Option<(u64, String)>;
    fn set(&self, text: String) -> Option<u64>;
}

/// The user's clipboard as the mirror sees it.
trait TextBoard {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: String) -> Result<(), String>;
}

impl TextBoard for arboard::Clipboard {
    fn get_text(&mut self) -> Result<String, String> {
        arboard::Clipboard::get_text(self).map_err(|e| e.to_string())
    }

    fn set_text(&mut self, text: String) -> Result<(), String> {
        arboard::Clipboard::set_text(self, text).map_err(|e| e.to_string())
    }
}

/// Tray-side sync state between the user's clipboard and the service's.
struct Mirror {
    seen: u64,
    last: Option<String>,
}

impl Mirror {
    /// One poll; `Err` means the local clipboard must be reopened.
    fn step(
        &mut self,
        local: &mut impl TextBoard,
        remote: &impl SessionClipboardRemote,
    ) -> Result<(), String> {
        if remote.generation() != self.seen {
            if let Some((generation, text)) = remote.get() {
                self.seen = generation;
                if !text.is_empty() && self.last.as_deref() != Some(text.as_str()) {
                    let applied = local.set_text(text.clone());
                    // Even on failure: a read-back must not send it back.
                    self.last = Some(text);
                    // Don't read back this tick: the commit can race the read.
                    return applied;
                }
            }
        }
        if let Ok(text) = local.get_text() {
            let text = clamp_clipboard_text(&text);
            if self.last.as_deref() != Some(text.as_str()) {
                if let Some(generation) = remote.set(text.clone()) {
                    self.seen = generation;
                    self.last = Some(text);
                }
            }
        }
        Ok(())
    }
}

/// Tray agent side of system mode's clipboard: push the user's clipboard to
/// the service and apply text a client sent, while a client is connected.
/// Never returns.
pub fn mirror_session_clipboard(remote: impl SessionClipboardRemote) {
    let mut board: Option<arboard::Clipboard> = None;
    let mut mirror = Mirror {
        seen: remote.generation(),
        last: None,
    };
    let mut last_log = Instant::now() - CLIPBOARD_ERROR_LOG_INTERVAL;
    loop {
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
        if !remote.active() {
            board = None;
            mirror = Mirror {
                seen: remote.generation(),
                last: None,
            };
            continue;
        }
        let local = match board.as_mut() {
            Some(board) => board,
            None => match arboard::Clipboard::new() {
                Ok(new) => board.insert(new),
                Err(err) => {
                    if last_log.elapsed() >= CLIPBOARD_ERROR_LOG_INTERVAL {
                        eprintln!("[clipboard] session: unavailable: {err}");
                        last_log = Instant::now();
                    }
                    continue;
                }
            },
        };
        if mirror.step(local, &remote).is_err() {
            board = None;
        }
    }
}

// ---------------------------------------------------------------------------
// File clipboard detection (platform-specific)
// ---------------------------------------------------------------------------

fn run_file_clipboard_loop(
    file_tx: Sender<PathBuf>,
    stop: Arc<AtomicBool>,
    suppressed: SuppressedPaths,
) {
    let mut last_files: Vec<PathBuf> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(FILE_POLL_INTERVAL);

        let files = detect_clipboard_files();
        if !files.is_empty() && files != last_files {
            // Filter out files we placed into the clipboard ourselves (echo suppression).
            let suppress_set = suppressed.lock().unwrap();
            for path in &files {
                if path.is_file() && !suppress_set.contains(path) {
                    let _ = file_tx.try_send(path.clone());
                }
            }
            last_files = files;
        }
    }
}

/// Detect file URIs in the OS clipboard.
///
/// Returns a list of local file paths, or empty if the clipboard does not
/// contain files.
#[cfg(target_os = "linux")]
fn detect_clipboard_files() -> Vec<PathBuf> {
    // Try Wayland first, then X11.
    let output = if std::env::var("WAYLAND_DISPLAY").is_ok() {
        std::process::Command::new("wl-paste")
            .args(["--type", "text/uri-list", "--no-newline"])
            .output()
            .ok()
    } else {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-target", "text/uri-list", "-o"])
            .output()
            .ok()
    };

    let output = match output {
        Some(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    parse_file_uris(&text)
}

#[cfg(target_os = "macos")]
fn detect_clipboard_files() -> Vec<PathBuf> {
    // Use osascript to get all file URLs from the clipboard.
    // The script returns one POSIX path per line for multi-file selections.
    let script = r#"try
    set theFiles to the clipboard as «class furl»
    set fileList to {}
    repeat with f in (the clipboard as list)
        try
            set end of fileList to POSIX path of (f as alias)
        end try
    end repeat
    set AppleScript's text item delimiters to linefeed
    return fileList as text
end try"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok();

    let output = match output {
        Some(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(target_os = "windows")]
fn detect_clipboard_files() -> Vec<PathBuf> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::UI::Shell::DragQueryFileW;

    const CF_HDROP: u32 = 15;
    let mut files = Vec::new();

    unsafe {
        if OpenClipboard(None).is_err() {
            return files;
        }

        let handle = GetClipboardData(CF_HDROP);
        if let Ok(handle) = handle {
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal);
            if !ptr.is_null() {
                let hdrop = windows::Win32::UI::Shell::HDROP(ptr as _);
                let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None) as usize;
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                    let path = String::from_utf16_lossy(&buf[..len]);
                    files.push(PathBuf::from(path));
                }
                let _ = GlobalUnlock(hglobal);
            }
        }

        let _ = CloseClipboard();
    }

    files
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_clipboard_files() -> Vec<PathBuf> {
    Vec::new()
}

/// Parse `text/uri-list` content into local file paths.
#[allow(dead_code)]
fn parse_file_uris(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("file://")
                .map(|path| PathBuf::from(url_decode(path)))
        })
        .collect()
}

/// Simple percent-decoding for file URIs.
#[allow(dead_code)]
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val);
            let lo = chars.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h << 4 | l) as char);
            }
        } else {
            result.push(b as char);
        }
    }
    result
}

#[allow(dead_code)]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_text(rx: &Receiver<ControlMessage>) -> Option<String> {
        match rx.recv_timeout(Duration::from_millis(900)) {
            Ok(ControlMessage::ClipboardText(text)) => Some(text),
            _ => None,
        }
    }

    #[derive(Default)]
    struct FakeBoard(Option<String>);

    impl TextBoard for FakeBoard {
        fn get_text(&mut self) -> Result<String, String> {
            self.0.clone().ok_or_else(|| "empty".into())
        }

        fn set_text(&mut self, text: String) -> Result<(), String> {
            self.0 = Some(text);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRemote {
        session: SessionClipboard,
        pushes: std::cell::Cell<u32>,
    }

    impl SessionClipboardRemote for &FakeRemote {
        fn active(&self) -> bool {
            true
        }

        fn generation(&self) -> u64 {
            self.session.generation()
        }

        fn get(&self) -> Option<(u64, String)> {
            Some(self.session.get())
        }

        fn set(&self, text: String) -> Option<u64> {
            self.pushes.set(self.pushes.get() + 1);
            Some(self.session.set(text))
        }
    }

    #[test]
    fn tray_mirror_pushes_copies_and_applies_client_text_without_echo() {
        let remote = FakeRemote::default();
        let mut local = FakeBoard::default();
        let mut mirror = Mirror {
            seen: 0,
            last: None,
        };
        let mut step = |local: &mut FakeBoard| mirror.step(local, &&remote).unwrap();
        // The user copies: pushed once.
        local.0 = Some("host".into());
        step(&mut local);
        step(&mut local);
        assert_eq!(remote.session.get().1, "host");
        assert_eq!(remote.pushes.get(), 1);
        // A client's text lands locally and is not pushed back.
        remote.session.set("client".into());
        step(&mut local);
        step(&mut local);
        assert_eq!(local.0.as_deref(), Some("client"));
        assert_eq!(remote.pushes.get(), 1);
        // The last client leaving clears the service copy, not the user's.
        remote.session.clear();
        step(&mut local);
        assert_eq!(local.0.as_deref(), Some("client"));
        assert_eq!(remote.pushes.get(), 1);
        // A new copy is pushed again.
        local.0 = Some("again".into());
        step(&mut local);
        assert_eq!(remote.session.get().1, "again");
    }

    #[test]
    fn session_clipboard_relays_both_ways_without_echo() {
        let session = Arc::new(SessionClipboard::default());
        let (tx, rx) = crossbeam_channel::bounded(8);
        let (file_tx, _file_rx) = crossbeam_channel::bounded(1);
        let sync = ClipboardSync::start_with_file_detection(
            "test",
            tx,
            file_tx,
            new_suppressed_paths(),
            Some(Arc::clone(&session)),
        );
        // Nothing copied yet: nothing sent.
        assert_eq!(next_text(&rx), None);
        // The tray agent pushes the user's copy: the client gets it once.
        session.set("from host".into());
        assert_eq!(next_text(&rx).as_deref(), Some("from host"));
        // The client copies: it lands in the session, not echoed back.
        sync.set_remote_text("from client".into());
        thread::sleep(Duration::from_millis(600));
        assert_eq!(session.get().1, "from client");
        assert_eq!(next_text(&rx), None);
    }
}
