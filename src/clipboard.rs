use crossbeam_channel::{Receiver, Sender};
use st_protocol::ControlMessage;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CLIPBOARD_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CLIPBOARD_TEXT_BYTES: usize = u16::MAX as usize;

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

pub struct ClipboardSync {
    remote_tx: Sender<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
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
        let (remote_tx, remote_rx) = crossbeam_channel::unbounded::<String>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            run_clipboard_loop(
                label,
                send_initial_snapshot_on_activate,
                is_active,
                outbound_tx,
                remote_rx,
                stop_flag,
            );
        });
        Self {
            remote_tx,
            stop,
            thread: Some(thread),
        }
    }

    pub fn set_remote_text(&self, text: String) {
        let _ = self.remote_tx.send(text);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_clipboard_loop<F>(
    label: &'static str,
    send_initial_snapshot_on_activate: bool,
    is_active: F,
    outbound_tx: Sender<ControlMessage>,
    remote_rx: Receiver<String>,
    stop: Arc<AtomicBool>,
) where
    F: Fn() -> bool + Send + 'static,
{
    let mut clipboard: Option<arboard::Clipboard> = None;
    let mut last_log = Instant::now() - CLIPBOARD_ERROR_LOG_INTERVAL;
    let mut pending_remote: Option<String> = None;
    let mut last_synced_text: Option<String> = None;
    let mut was_active = false;

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

        let active = is_active();
        if active {
            let send_snapshot = send_initial_snapshot_on_activate && !was_active;
            match clipboard.as_mut().unwrap().get_text() {
                Ok(text) => {
                    let text = clamp_clipboard_text(&text);
                    let changed = last_synced_text.as_deref() != Some(text.as_str());
                    if send_snapshot || changed {
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
                    }
                }
                Err(_) => {}
            }
        }
        was_active = active;
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
    }
}
