/// Zero-copy broadcaster: one producer, many consumers.
///
/// Each subscriber gets an `Arc<T>` clone of the broadcast item, avoiding
/// per-client data copies. Slow clients are skipped (try_send), disconnected
/// clients are automatically removed.
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

/// Maximum number of concurrent subscribers per broadcaster.
const MAX_SUBSCRIBERS: usize = 16;

pub struct Broadcaster<T: Send + Sync + 'static> {
    subscribers: Mutex<Vec<(u64, Sender<Arc<T>>)>>,
    next_id: AtomicU64,
    /// Set when a new subscriber is added; the producer should emit a keyframe.
    keyframe_requested: AtomicBool,
}

impl<T: Send + Sync + 'static> Broadcaster<T> {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            keyframe_requested: AtomicBool::new(false),
        }
    }

    /// Add a subscriber. Returns (id, receiver), or an error if at capacity.
    /// Also sets the keyframe-requested flag so the encoder produces a fresh IDR.
    pub fn subscribe(&self, capacity: usize) -> Result<(u64, Receiver<Arc<T>>), String> {
        let mut subs = self.subscribers.lock().unwrap();
        if subs.len() >= MAX_SUBSCRIBERS {
            return Err(format!("subscriber limit reached ({MAX_SUBSCRIBERS})"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = bounded(capacity);
        subs.push((id, tx));
        self.keyframe_requested.store(true, Ordering::Release);
        Ok((id, rx))
    }

    /// Check and clear the keyframe-requested flag.
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::AcqRel)
    }

    /// Explicitly request a fresh keyframe for existing subscribers.
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    /// Remove a subscriber by id.
    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().unwrap().retain(|(i, _)| *i != id);
    }

    /// Broadcast an item to all subscribers.
    /// Skips full channels (slow clients). Removes disconnected subscribers.
    pub fn broadcast(&self, item: T) {
        let arc = Arc::new(item);
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|(_, tx)| match tx.try_send(Arc::clone(&arc)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}
