/// Zero-copy broadcaster: one producer, many consumers.
///
/// Each subscriber gets an `Arc<T>` clone of the broadcast item, avoiding
/// per-client data copies. Slow clients are skipped (try_send), disconnected
/// clients are automatically removed.
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

/// Maximum number of concurrent subscribers per broadcaster.
const MAX_SUBSCRIBERS: usize = 16;

pub struct Broadcaster<T: Send + Sync + 'static> {
    subscribers: Mutex<Vec<Subscriber<T>>>,
    next_id: AtomicU64,
    /// Set when a new subscriber is added; the producer should emit a keyframe.
    keyframe_requested: AtomicBool,
}

struct Subscriber<T: Send + Sync + 'static> {
    id: u64,
    tx: Sender<Arc<T>>,
    // A private receiver clone lets the broadcaster evict the oldest queued
    // frame when a slow subscriber falls behind, keeping the channel live.
    drop_rx: Receiver<Arc<T>>,
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
        subs.push(Subscriber {
            id,
            tx,
            drop_rx: rx.clone(),
        });
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
        self.subscribers.lock().unwrap().retain(|sub| sub.id != id);
    }

    /// Broadcast an item to all subscribers.
    /// When a subscriber queue is full, evict its oldest queued item so the
    /// newest frame still reaches the slow client. Removes disconnected
    /// subscribers.
    pub fn broadcast(&self, item: T) {
        let arc = Arc::new(item);
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| match sub.tx.try_send(Arc::clone(&arc)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => match sub.drop_rx.try_recv() {
                Ok(_) | Err(TryRecvError::Empty) => {
                    !matches!(sub.tx.try_send(Arc::clone(&arc)), Err(TrySendError::Disconnected(_)))
                }
                Err(TryRecvError::Disconnected) => false,
            },
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::Broadcaster;

    #[test]
    fn full_subscriber_queue_keeps_newest_item() {
        let broadcaster = Broadcaster::new();
        let (_id, rx) = broadcaster.subscribe(2).expect("subscribe");

        broadcaster.broadcast(1u32);
        broadcaster.broadcast(2u32);
        broadcaster.broadcast(3u32);

        assert_eq!(*rx.recv().expect("first queued item"), 2);
        assert_eq!(*rx.recv().expect("second queued item"), 3);
    }
}
