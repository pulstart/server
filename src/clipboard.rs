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

pub struct ClipboardSync {
    remote_tx: Sender<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    file_thread: Option<JoinHandle<()>>,
}

impl ClipboardSync {
    pub fn start<F>(
        label: &'static str,
        send_initial_snapshot_on_activate: bool,
        is_active: F,
        outbound_tx: Sender<ControlMessage>,
    ) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        Self::start_inner(label, send_initial_snapshot_on_activate, is_active, outbound_tx, None, None)
    }

    pub fn start_with_file_detection<F>(
        label: &'static str,
        send_initial_snapshot_on_activate: bool,
        is_active: F,
        outbound_tx: Sender<ControlMessage>,
        file_tx: Sender<PathBuf>,
        suppressed: SuppressedPaths,
    ) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        Self::start_inner(
            label,
            send_initial_snapshot_on_activate,
            is_active,
            outbound_tx,
            Some(file_tx),
            Some(suppressed),
        )
    }

    fn start_inner<F>(
        label: &'static str,
        send_initial_snapshot_on_activate: bool,
        is_active: F,
        outbound_tx: Sender<ControlMessage>,
        file_tx: Option<Sender<PathBuf>>,
        suppressed: Option<SuppressedPaths>,
    ) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        let (remote_tx, remote_rx) = crossbeam_channel::bounded::<String>(REMOTE_CHANNEL_BOUND);
        let stop = Arc::new(AtomicBool::new(false));

        let stop_flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            // is_active / send_initial_snapshot_on_activate are unused now — clipboard
            // text sync is always active while connected. Keep the params in the public
            // API for backward compat but drop them here.
            let _ = (is_active, send_initial_snapshot_on_activate);
            run_clipboard_loop(label, outbound_tx, remote_rx, stop_flag);
        });

        let file_thread = file_tx.map(|tx| {
            let stop_flag = Arc::clone(&stop);
            let suppressed = suppressed.unwrap_or_else(new_suppressed_paths);
            thread::spawn(move || {
                run_file_clipboard_loop(tx, stop_flag, suppressed);
            })
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
) {
    let mut clipboard: Option<arboard::Clipboard> = None;
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
                Ok(instance) => clipboard = Some(instance),
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
        // No control-ownership gate — clipboard sync is always active while connected.
        match clipboard.as_mut().unwrap().get_text() {
            Ok(text) => {
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
            Err(_) => {}
        }
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
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
    // Windows CF_HDROP detection — not yet implemented.
    // When implemented, use OpenClipboard + GetClipboardData(CF_HDROP) + DragQueryFileW.
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_clipboard_files() -> Vec<PathBuf> {
    Vec::new()
}

/// Parse `text/uri-list` content into local file paths.
fn parse_file_uris(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.trim();
            if let Some(path) = line.strip_prefix("file://") {
                Some(PathBuf::from(url_decode(path)))
            } else {
                None
            }
        })
        .collect()
}

/// Simple percent-decoding for file URIs.
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(|c| hex_val(c));
            let lo = chars.next().and_then(|c| hex_val(c));
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h << 4 | l) as char);
            }
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
