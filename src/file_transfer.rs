use crate::clipboard::SuppressedPaths;
use crossbeam_channel::{Receiver, Sender};
use sha2::{Digest, Sha256};
use st_protocol::file_transfer::*;
use st_protocol::ControlMessage;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Public UI-facing state (shared with control loop)
// ---------------------------------------------------------------------------

/// A file offered by the remote side, waiting for the local user to paste.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingOffer {
    pub transfer_id: u32,
    pub file_name: String,
    pub file_size: u64,
    pub received_at: Instant,
}

/// Status of a single transfer visible to the control loop / UI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TransferEntry {
    pub direction: TransferDirection,
    pub file_name: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub status: TransferStatus,
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
}

/// Shared state between the manager thread and the UI.
pub struct FileTransferShared {
    /// Active / recently completed transfers.
    pub entries: Vec<TransferEntry>,
    /// Remote file offers waiting for the user to accept (paste).
    pub pending_offers: Vec<PendingOffer>,
    /// Transfer IDs the UI wants to accept. The manager drains this each tick.
    pub accept_queue: Vec<u32>,
}

impl FileTransferShared {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending_offers: Vec::new(),
            accept_queue: Vec::new(),
        }
    }
}

/// Shared transfer state.
pub type SharedTransferState = Arc<Mutex<FileTransferShared>>;

pub fn new_shared_state() -> SharedTransferState {
    Arc::new(Mutex::new(FileTransferShared::new()))
}

// ---------------------------------------------------------------------------
// Internal events between control loop and manager
// ---------------------------------------------------------------------------

/// Messages from the control loop into the file transfer manager.
#[derive(Debug)]
#[allow(dead_code)]
pub enum FtInbound {
    /// A remote peer offered a file.
    OfferReceived {
        transfer_id: u32,
        file_size: u64,
        file_name: String,
    },
    /// A remote peer accepted/rejected our offer.
    AcceptReceived {
        transfer_id: u32,
        accepted: bool,
    },
    /// A chunk of file data from the remote peer.
    ChunkReceived {
        transfer_id: u32,
        chunk_index: u32,
        data: Vec<u8>,
    },
    /// Remote peer signals transfer complete.
    CompleteReceived {
        transfer_id: u32,
        total_chunks: u32,
        sha256: [u8; 32],
    },
    /// Remote peer cancelled.
    CancelReceived {
        transfer_id: u32,
    },
    /// Remote peer acknowledged progress.
    ProgressReceived {
        transfer_id: u32,
        chunks_received: u32,
    },
    /// Local side wants to send a file.
    SendFile {
        path: PathBuf,
    },
}

/// Messages from the file transfer manager to the control loop for sending.
pub type FtOutbound = ControlMessage;

// ---------------------------------------------------------------------------
// FileTransferManager
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct FileTransferManager {
    pub inbound_tx: Sender<FtInbound>,
    pub outbound_rx: Receiver<FtOutbound>,
    pub shared_state: SharedTransferState,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FileTransferManager {
    #[allow(dead_code)]
    pub fn start(mode: TransportMode) -> Self {
        Self::start_with_state(mode, new_shared_state())
    }

    #[allow(dead_code)]
    pub fn start_with_state(mode: TransportMode, shared_state: SharedTransferState) -> Self {
        Self::start_full(mode, shared_state, crate::clipboard::new_suppressed_paths())
    }

    #[allow(dead_code)]
    pub fn start_full(
        mode: TransportMode,
        shared_state: SharedTransferState,
        suppressed_paths: SuppressedPaths,
    ) -> Self {
        Self::start_configured(mode, shared_state, suppressed_paths, false)
    }

    #[allow(dead_code)]
    pub fn start_auto_accept(
        mode: TransportMode,
        shared_state: SharedTransferState,
        suppressed_paths: SuppressedPaths,
    ) -> Self {
        Self::start_configured(mode, shared_state, suppressed_paths, true)
    }

    fn start_configured(
        mode: TransportMode,
        shared_state: SharedTransferState,
        suppressed_paths: SuppressedPaths,
        auto_accept: bool,
    ) -> Self {
        let (inbound_tx, inbound_rx) = crossbeam_channel::bounded::<FtInbound>(64);
        let (outbound_tx, outbound_rx) = crossbeam_channel::bounded::<FtOutbound>(64);
        let state_clone = Arc::clone(&shared_state);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            run_manager(mode, inbound_rx, outbound_tx, state_clone, stop_flag, suppressed_paths, auto_accept);
        });

        Self {
            inbound_tx,
            outbound_rx,
            shared_state,
            stop,
            thread: Some(thread),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for FileTransferManager {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Manager thread
// ---------------------------------------------------------------------------

struct SendState {
    info: TransferInfo,
    file_path: PathBuf,
    last_acked_chunk: u32,
    last_activity: Instant,
}

struct RecvState {
    info: TransferInfo,
    part_path: PathBuf,
    final_path: PathBuf,
    hasher: Sha256,
    file: fs::File,
    next_expected_chunk: u32,
    last_activity: Instant,
}

fn run_manager(
    mode: TransportMode,
    inbound_rx: Receiver<FtInbound>,
    outbound_tx: Sender<FtOutbound>,
    shared_state: SharedTransferState,
    stop: Arc<AtomicBool>,
    suppressed_paths: SuppressedPaths,
    auto_accept: bool,
) {
    let mut next_transfer_id: u32 = 1;
    let mut sending: HashMap<u32, SendState> = HashMap::new();
    let mut receiving: HashMap<u32, RecvState> = HashMap::new();

    while !stop.load(Ordering::Relaxed) {
        // Process inbound events.
        let mut had_work = false;
        while let Ok(event) = inbound_rx.try_recv() {
            had_work = true;
            match event {
                FtInbound::SendFile { path } => {
                    handle_send_file(
                        &path,
                        mode,
                        &mut next_transfer_id,
                        &mut sending,
                        &outbound_tx,
                        &shared_state,
                    );
                }
                FtInbound::OfferReceived {
                    transfer_id,
                    file_size,
                    file_name,
                } => {
                    if auto_accept {
                        // Server has no UI — accept immediately.
                        accept_offer(
                            transfer_id,
                            file_size,
                            &file_name,
                            mode,
                            &mut receiving,
                            &outbound_tx,
                            &shared_state,
                        );
                    } else {
                        handle_offer_received(
                            transfer_id,
                            file_size,
                            &file_name,
                            mode,
                            &outbound_tx,
                            &shared_state,
                        );
                    }
                }
                FtInbound::AcceptReceived {
                    transfer_id,
                    accepted,
                } => {
                    if let Some(state) = sending.get_mut(&transfer_id) {
                        if accepted {
                            state.info.status = TransferStatus::Active;
                            state.last_activity = Instant::now();
                        } else {
                            state.info.status = TransferStatus::Cancelled;
                            update_shared_entry(&shared_state, &state.info);
                            sending.remove(&transfer_id);
                        }
                    }
                }
                FtInbound::ChunkReceived {
                    transfer_id,
                    chunk_index,
                    data,
                } => {
                    handle_chunk_received(
                        transfer_id,
                        chunk_index,
                        &data,
                        &mut receiving,
                        &outbound_tx,
                        &shared_state,
                    );
                }
                FtInbound::CompleteReceived {
                    transfer_id,
                    total_chunks: _,
                    sha256,
                } => {
                    handle_complete_received(
                        transfer_id,
                        &sha256,
                        &mut receiving,
                        &shared_state,
                        &suppressed_paths,
                    );
                }
                FtInbound::CancelReceived { transfer_id } => {
                    if let Some(mut state) = sending.remove(&transfer_id) {
                        state.info.status = TransferStatus::Cancelled;
                        update_shared_entry(&shared_state, &state.info);
                    }
                    if let Some(mut state) = receiving.remove(&transfer_id) {
                        state.info.status = TransferStatus::Cancelled;
                        update_shared_entry(&shared_state, &state.info);
                        let _ = fs::remove_file(&state.part_path);
                    }
                    // Also remove from pending offers if not yet accepted.
                    remove_pending_offer(&shared_state, transfer_id);
                }
                FtInbound::ProgressReceived {
                    transfer_id,
                    chunks_received,
                } => {
                    if let Some(state) = sending.get_mut(&transfer_id) {
                        state.last_acked_chunk = chunks_received;
                        state.last_activity = Instant::now();
                    }
                }
            }
        }

        // Pump outbound chunks for active sends.
        for state in sending.values_mut() {
            if state.info.status != TransferStatus::Active {
                continue;
            }
            pump_send_chunks(state, &outbound_tx, &shared_state);
        }

        // Check timeouts.
        let now = Instant::now();
        let timed_out_send: Vec<u32> = sending
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_activity) >= TRANSFER_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out_send {
            if let Some(mut s) = sending.remove(&id) {
                s.info.status = TransferStatus::Failed;
                update_shared_entry(&shared_state, &s.info);
                let _ = outbound_tx.try_send(ControlMessage::FileCancel { transfer_id: id });
                eprintln!("[file-transfer] send timed out: {}", s.info.file_name);
            }
        }
        let timed_out_recv: Vec<u32> = receiving
            .iter()
            .filter(|(_, r)| now.duration_since(r.last_activity) >= TRANSFER_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out_recv {
            if let Some(mut r) = receiving.remove(&id) {
                r.info.status = TransferStatus::Failed;
                update_shared_entry(&shared_state, &r.info);
                let _ = fs::remove_file(&r.part_path);
                let _ = outbound_tx.try_send(ControlMessage::FileCancel { transfer_id: id });
                eprintln!("[file-transfer] receive timed out: {}", r.info.file_name);
            }
        }

        // Drain the accept queue — user clicked paste in the UI.
        {
            let mut state = shared_state.lock().unwrap();
            let accepted_ids: Vec<u32> = state.accept_queue.drain(..).collect();
            // Also collect the pending offer info we need to start receiving.
            let mut offers_to_accept: Vec<(u32, u64, String)> = Vec::new();
            for id in &accepted_ids {
                if let Some(offer) = state.pending_offers.iter().find(|o| o.transfer_id == *id) {
                    offers_to_accept.push((offer.transfer_id, offer.file_size, offer.file_name.clone()));
                }
            }
            for id in &accepted_ids {
                state.pending_offers.retain(|o| o.transfer_id != *id);
            }
            drop(state);

            for (transfer_id, file_size, file_name) in offers_to_accept {
                accept_offer(
                    transfer_id,
                    file_size,
                    &file_name,
                    mode,
                    &mut receiving,
                    &outbound_tx,
                    &shared_state,
                );
            }
        }

        // Prune completed entries from shared state after 10 seconds.
        {
            let mut state = shared_state.lock().unwrap();
            state.entries.retain(|e| match e.status {
                TransferStatus::Completed | TransferStatus::Cancelled | TransferStatus::Failed => {
                    e.completed_at
                        .map(|t| t.elapsed().as_secs() < 10)
                        .unwrap_or(true)
                }
                _ => true,
            });
        }

        if !had_work {
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // Cleanup on stop: cancel all active transfers.
    for (_, r) in receiving.drain() {
        let _ = fs::remove_file(&r.part_path);
    }
}

// ---------------------------------------------------------------------------
// Handler helpers
// ---------------------------------------------------------------------------

fn handle_send_file(
    path: &Path,
    mode: TransportMode,
    next_id: &mut u32,
    sending: &mut HashMap<u32, SendState>,
    outbound_tx: &Sender<FtOutbound>,
    shared_state: &SharedTransferState,
) {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[file-transfer] cannot stat {}: {e}", path.display());
            return;
        }
    };
    if !metadata.is_file() {
        return;
    }
    let file_size = metadata.len();
    if file_size > mode.max_file_size() {
        eprintln!(
            "[file-transfer] {} too large ({} > {})",
            path.display(),
            format_bytes(file_size),
            format_bytes(mode.max_file_size())
        );
        return;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let file_name = match sanitize_filename(&file_name) {
        Some(n) => n,
        None => "file".to_string(),
    };

    let transfer_id = *next_id;
    *next_id += 1;

    let info = TransferInfo::new_send(transfer_id, file_name.clone(), file_size, mode);

    let _ = outbound_tx.try_send(ControlMessage::FileOffer {
        transfer_id,
        file_size,
        file_name: file_name.clone(),
    });

    add_shared_entry(shared_state, &info);

    sending.insert(
        transfer_id,
        SendState {
            info,
            file_path: path.to_path_buf(),
            last_acked_chunk: 0,
            last_activity: Instant::now(),
        },
    );

    eprintln!(
        "[file-transfer] offering {} ({}) id={}",
        file_name,
        format_bytes(file_size),
        transfer_id
    );
}

/// Store the offer as pending — the user must click paste to accept.
fn handle_offer_received(
    transfer_id: u32,
    file_size: u64,
    file_name: &str,
    mode: TransportMode,
    outbound_tx: &Sender<FtOutbound>,
    shared_state: &SharedTransferState,
) {
    if file_size > mode.max_file_size() {
        let _ = outbound_tx.try_send(ControlMessage::FileAccept {
            transfer_id,
            accepted: false,
        });
        eprintln!("[file-transfer] rejected offer: too large ({file_size} bytes)");
        return;
    }

    let safe_name = sanitize_filename(file_name).unwrap_or_else(|| "received_file".to_string());

    add_pending_offer(shared_state, PendingOffer {
        transfer_id,
        file_name: safe_name.clone(),
        file_size,
        received_at: Instant::now(),
    });

    eprintln!(
        "[file-transfer] offer pending: {} ({}) id={} — waiting for paste",
        safe_name,
        format_bytes(file_size),
        transfer_id
    );
}

/// Actually accept an offer (called when user clicks paste).
fn accept_offer(
    transfer_id: u32,
    file_size: u64,
    file_name: &str,
    mode: TransportMode,
    receiving: &mut HashMap<u32, RecvState>,
    outbound_tx: &Sender<FtOutbound>,
    shared_state: &SharedTransferState,
) {
    let safe_name = sanitize_filename(file_name).unwrap_or_else(|| "received_file".to_string());

    let recv_dir = match receive_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[file-transfer] cannot create receive dir: {e}");
            let _ = outbound_tx.try_send(ControlMessage::FileAccept {
                transfer_id,
                accepted: false,
            });
            return;
        }
    };

    let final_path = unique_dest_path(&recv_dir, &safe_name);
    let part_path = final_path.with_extension(
        final_path
            .extension()
            .map(|e| format!("{}.part", e.to_string_lossy()))
            .unwrap_or_else(|| "part".to_string()),
    );

    let file = match fs::File::create(&part_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[file-transfer] cannot create {}: {e}", part_path.display());
            let _ = outbound_tx.try_send(ControlMessage::FileAccept {
                transfer_id,
                accepted: false,
            });
            return;
        }
    };

    let info = TransferInfo::new_receive(transfer_id, safe_name.clone(), file_size, mode);
    add_shared_entry(shared_state, &info);

    receiving.insert(
        transfer_id,
        RecvState {
            info,
            part_path,
            final_path,
            hasher: Sha256::new(),
            file,
            next_expected_chunk: 0,
            last_activity: Instant::now(),
        },
    );

    let _ = outbound_tx.try_send(ControlMessage::FileAccept {
        transfer_id,
        accepted: true,
    });

    eprintln!(
        "[file-transfer] accepted {} ({}) id={}",
        safe_name,
        format_bytes(file_size),
        transfer_id
    );
}

fn handle_chunk_received(
    transfer_id: u32,
    chunk_index: u32,
    data: &[u8],
    receiving: &mut HashMap<u32, RecvState>,
    outbound_tx: &Sender<FtOutbound>,
    shared_state: &SharedTransferState,
) {
    let state = match receiving.get_mut(&transfer_id) {
        Some(s) => s,
        None => return,
    };

    if chunk_index != state.next_expected_chunk {
        // Out-of-order over TCP/reliable-UDP shouldn't happen, but handle gracefully.
        return;
    }

    if state.file.write_all(data).is_err() {
        state.info.status = TransferStatus::Failed;
        update_shared_entry(shared_state, &state.info);
        let _ = outbound_tx.try_send(ControlMessage::FileCancel { transfer_id });
        let _ = fs::remove_file(&state.part_path);
        receiving.remove(&transfer_id);
        return;
    }

    state.hasher.update(data);
    state.next_expected_chunk += 1;
    state.info.chunks_done = state.next_expected_chunk;
    state.last_activity = Instant::now();
    update_shared_entry(shared_state, &state.info);

    // Send periodic progress ACKs.
    if state.next_expected_chunk % PROGRESS_ACK_INTERVAL == 0
        || state.next_expected_chunk == state.info.chunks_total
    {
        let _ = outbound_tx.try_send(ControlMessage::FileProgress {
            transfer_id,
            chunks_received: state.next_expected_chunk,
        });
    }
}

fn handle_complete_received(
    transfer_id: u32,
    expected_sha256: &[u8; 32],
    receiving: &mut HashMap<u32, RecvState>,
    shared_state: &SharedTransferState,
    suppressed_paths: &SuppressedPaths,
) {
    let state = match receiving.remove(&transfer_id) {
        Some(s) => s,
        None => return,
    };

    let computed = state.hasher.finalize();
    if computed.as_slice() != expected_sha256 {
        eprintln!(
            "[file-transfer] SHA-256 mismatch for {}, deleting partial file",
            state.info.file_name
        );
        let _ = fs::remove_file(&state.part_path);
        let mut info = state.info;
        info.status = TransferStatus::Failed;
        update_shared_entry_completed(shared_state, &info);
        return;
    }

    // Rename .part → final.
    if let Err(e) = fs::rename(&state.part_path, &state.final_path) {
        eprintln!("[file-transfer] rename failed: {e}");
        let _ = fs::remove_file(&state.part_path);
        let mut info = state.info;
        info.status = TransferStatus::Failed;
        update_shared_entry_completed(shared_state, &info);
        return;
    }

    // Set permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&state.final_path, fs::Permissions::from_mode(0o644));
    }

    let mut info = state.info;
    info.status = TransferStatus::Completed;
    update_shared_entry_completed(shared_state, &info);

    // Register the path so the file detection loop won't echo it back.
    suppressed_paths
        .lock()
        .unwrap()
        .insert(state.final_path.clone());

    // Place the received file into the OS clipboard so the user can paste it.
    set_clipboard_file(&state.final_path);

    eprintln!(
        "[file-transfer] completed: {} → {} (clipboard ready)",
        info.file_name,
        state.final_path.display()
    );
}

fn pump_send_chunks(
    state: &mut SendState,
    outbound_tx: &Sender<FtOutbound>,
    shared_state: &SharedTransferState,
) {
    let mut sent_this_tick = 0;

    while sent_this_tick < CHUNKS_PER_TICK
        && state.info.chunks_done < state.info.chunks_total
        && state.info.chunks_done < state.last_acked_chunk + FLOW_CONTROL_WINDOW
    {
        let chunk_index = state.info.chunks_done;
        let offset = chunk_index as u64 * state.info.chunk_size as u64;
        let remaining = state.info.file_size - offset;
        let read_len = (remaining as usize).min(state.info.chunk_size);

        let mut data = vec![0u8; read_len];
        let ok = (|| -> std::io::Result<()> {
            let mut f = fs::File::open(&state.file_path)?;
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(offset))?;
            f.read_exact(&mut data)?;
            Ok(())
        })();

        if ok.is_err() {
            state.info.status = TransferStatus::Failed;
            update_shared_entry_completed(shared_state, &state.info);
            let _ = outbound_tx.try_send(ControlMessage::FileCancel {
                transfer_id: state.info.transfer_id,
            });
            return;
        }

        if outbound_tx
            .try_send(ControlMessage::FileChunk {
                transfer_id: state.info.transfer_id,
                chunk_index,
                data,
            })
            .is_err()
        {
            return; // Channel full, try next tick.
        }

        state.info.chunks_done += 1;
        state.last_activity = Instant::now();
        sent_this_tick += 1;
    }

    update_shared_entry(shared_state, &state.info);

    // All chunks sent — send FileComplete.
    if state.info.chunks_done == state.info.chunks_total
        && state.info.status == TransferStatus::Active
    {
        // Compute SHA-256 of the whole file.
        let sha256 = match compute_file_sha256(&state.file_path) {
            Some(h) => h,
            None => {
                state.info.status = TransferStatus::Failed;
                update_shared_entry_completed(shared_state, &state.info);
                return;
            }
        };

        let _ = outbound_tx.try_send(ControlMessage::FileComplete {
            transfer_id: state.info.transfer_id,
            total_chunks: state.info.chunks_total,
            sha256,
        });

        state.info.status = TransferStatus::Completed;
        update_shared_entry_completed(shared_state, &state.info);

        eprintln!(
            "[file-transfer] sent complete: {}",
            state.info.file_name
        );
    }
}

fn compute_file_sha256(path: &Path) -> Option<[u8; 32]> {
    let mut f = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    Some(out)
}

// ---------------------------------------------------------------------------
// Shared state helpers
// ---------------------------------------------------------------------------

fn add_shared_entry(shared: &SharedTransferState, info: &TransferInfo) {
    let entry = TransferEntry {
        direction: info.direction,
        file_name: info.file_name.clone(),
        total_bytes: info.file_size,
        transferred_bytes: info.bytes_transferred(),
        status: info.status,
        started_at: Instant::now(),
        completed_at: None,
    };
    shared.lock().unwrap().entries.push(entry);
}

fn update_shared_entry(shared: &SharedTransferState, info: &TransferInfo) {
    update_shared_entry_with_completion(shared, info, None);
}

fn update_shared_entry_completed(shared: &SharedTransferState, info: &TransferInfo) {
    update_shared_entry_with_completion(shared, info, Some(Instant::now()));
}

fn update_shared_entry_with_completion(
    shared: &SharedTransferState,
    info: &TransferInfo,
    completed_at: Option<Instant>,
) {
    let mut state = shared.lock().unwrap();
    if let Some(entry) = state
        .entries
        .iter_mut()
        .find(|e| e.file_name == info.file_name && e.direction == info.direction)
    {
        entry.transferred_bytes = info.bytes_transferred();
        entry.status = info.status;
        if let Some(t) = completed_at {
            entry.completed_at = Some(t);
        }
    }
}

fn add_pending_offer(shared: &SharedTransferState, offer: PendingOffer) {
    shared.lock().unwrap().pending_offers.push(offer);
}

fn remove_pending_offer(shared: &SharedTransferState, transfer_id: u32) {
    shared
        .lock()
        .unwrap()
        .pending_offers
        .retain(|o| o.transfer_id != transfer_id);
}

// ---------------------------------------------------------------------------
// Filesystem helpers (not in protocol crate since they need `dirs`)
// ---------------------------------------------------------------------------

/// Return the staging directory for received files.
///
/// Files land here during transfer and remain until the next transfer overwrites
/// them or the OS cleans up. The user pastes from clipboard, which references
/// this path.
pub fn receive_dir() -> std::io::Result<PathBuf> {
    let dir = if let Ok(custom) = std::env::var("ST_FILE_RECEIVE_DIR") {
        PathBuf::from(custom)
    } else {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|_| std::env::temp_dir().canonicalize())
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        base.join(DEFAULT_RECEIVE_DIR)
    };
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Place a received file's path into the OS clipboard so the user can paste it.
#[cfg(target_os = "linux")]
pub fn set_clipboard_file(path: &Path) {
    let uri = format!("file://{}", path.display());
    // Try Wayland first, then X11.
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        let _ = std::process::Command::new("wl-copy")
            .arg("--type")
            .arg("text/uri-list")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(ref mut stdin) = child.stdin {
                    use std::io::Write;
                    let _ = write!(stdin, "{uri}\n");
                }
                child.wait()
            });
    } else {
        let _ = std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-target", "text/uri-list", "-i"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(ref mut stdin) = child.stdin {
                    use std::io::Write;
                    let _ = write!(stdin, "{uri}\n");
                }
                child.wait()
            });
    }
}

#[cfg(target_os = "macos")]
pub fn set_clipboard_file(path: &Path) {
    // Use osascript to set file on clipboard.
    let script = format!(
        "set the clipboard to (POSIX file \"{}\")",
        path.display()
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status();
}

#[cfg(target_os = "windows")]
pub fn set_clipboard_file(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    const CF_HDROP: u32 = 15;

    // DROPFILES struct: 20 bytes header + null-terminated wide string list + double-null
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().collect();
    // DROPFILES { pFiles: u32, pt: POINT(0,0), fNC: BOOL(0), fWide: BOOL(1) }
    let header_size = 20u32;
    let data_size = header_size as usize + (wide_path.len() + 2) * 2; // path + null + final null

    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();

        let hmem = GlobalAlloc(GMEM_MOVEABLE, data_size);
        let hmem = match hmem {
            Ok(h) => h,
            Err(_) => {
                let _ = CloseClipboard();
                return;
            }
        };

        let ptr = GlobalLock(hmem);
        if !ptr.is_null() {
            let bytes = ptr as *mut u8;
            // Write DROPFILES header
            std::ptr::copy_nonoverlapping(
                &header_size as *const u32 as *const u8,
                bytes,
                4,
            );
            // pt.x = 0, pt.y = 0 (bytes 4..12)
            std::ptr::write_bytes(bytes.add(4), 0, 8);
            // fNC = 0 (bytes 12..16)
            std::ptr::write_bytes(bytes.add(12), 0, 4);
            // fWide = 1 (bytes 16..20)
            let fwide: u32 = 1;
            std::ptr::copy_nonoverlapping(
                &fwide as *const u32 as *const u8,
                bytes.add(16),
                4,
            );
            // Write file path as wide string
            let wide_dest = bytes.add(header_size as usize) as *mut u16;
            std::ptr::copy_nonoverlapping(wide_path.as_ptr(), wide_dest, wide_path.len());
            // Null terminator for path
            *wide_dest.add(wide_path.len()) = 0;
            // Final null terminator for file list
            *wide_dest.add(wide_path.len() + 1) = 0;

            let _ = GlobalUnlock(hmem);
        }

        let _ = SetClipboardData(CF_HDROP, std::mem::transmute(hmem));
        let _ = CloseClipboard();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn set_clipboard_file(_path: &Path) {}

/// Build a unique destination path, appending _1, _2, etc. on collision.
pub fn unique_dest_path(dir: &Path, name: &str) -> PathBuf {
    let base = dir.join(name);
    if !base.exists() {
        return base;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for i in 1u32.. {
        let candidate = if let Some(ext) = ext {
            dir.join(format!("{stem}_{i}.{ext}"))
        } else {
            dir.join(format!("{stem}_{i}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}
