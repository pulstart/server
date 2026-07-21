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
    state: Mutex<BroadcasterState<T>>,
    next_id: AtomicU64,
    on_zero: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Set when a new subscriber is added; the producer should emit a keyframe.
    keyframe_requested: AtomicBool,
}

struct BroadcasterState<T: Send + Sync + 'static> {
    subscribers: Vec<Subscriber<T>>,
    reservations: usize,
}

pub struct SubscriptionReservation<T: Send + Sync + 'static> {
    broadcaster: Arc<Broadcaster<T>>,
    active: bool,
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
            state: Mutex::new(BroadcasterState {
                subscribers: Vec::new(),
                reservations: 0,
            }),
            next_id: AtomicU64::new(0),
            on_zero: Mutex::new(None),
            keyframe_requested: AtomicBool::new(false),
        }
    }

    pub fn set_on_zero(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.on_zero.lock().unwrap() = Some(Arc::new(callback));
    }

    fn notify_zero(&self) {
        let callback = self.on_zero.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Add a subscriber. Returns (id, receiver), or an error if at capacity.
    /// Also sets the keyframe-requested flag so the encoder produces a fresh IDR.
    pub fn subscribe(&self, capacity: usize) -> Result<(u64, Receiver<Arc<T>>), String> {
        let mut state = self.state.lock().unwrap();
        if state.subscribers.len() + state.reservations >= MAX_SUBSCRIBERS {
            return Err(format!("subscriber limit reached ({MAX_SUBSCRIBERS})"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = bounded(capacity);
        state.subscribers.push(Subscriber {
            id,
            tx,
            drop_rx: rx.clone(),
        });
        self.keyframe_requested.store(true, Ordering::Release);
        Ok((id, rx))
    }

    /// Reserve subscriber capacity before performing a profile transition.
    /// Dropping the reservation rolls it back without exposing a subscriber.
    pub fn reserve(self: &Arc<Self>) -> Result<SubscriptionReservation<T>, String> {
        let mut state = self.state.lock().unwrap();
        if state.subscribers.len() + state.reservations >= MAX_SUBSCRIBERS {
            return Err(format!("subscriber limit reached ({MAX_SUBSCRIBERS})"));
        }
        state.reservations += 1;
        Ok(SubscriptionReservation {
            broadcaster: Arc::clone(self),
            active: true,
        })
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
        let became_empty = {
            let mut state = self.state.lock().unwrap();
            let before = state.subscribers.len() + state.reservations;
            state.subscribers.retain(|sub| sub.id != id);
            before > 0 && state.subscribers.is_empty() && state.reservations == 0
        };
        if became_empty {
            self.notify_zero();
        }
    }

    /// Broadcast an item to all subscribers.
    /// When a subscriber queue is full, evict its oldest queued item so the
    /// newest frame still reaches the slow client. Removes disconnected
    /// subscribers.
    pub fn broadcast(&self, item: T) {
        let arc = Arc::new(item);
        let became_empty = {
            let mut state = self.state.lock().unwrap();
            let before = state.subscribers.len() + state.reservations;
            state
                .subscribers
                .retain(|sub| match sub.tx.try_send(Arc::clone(&arc)) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => match sub.drop_rx.try_recv() {
                        Ok(_) | Err(TryRecvError::Empty) => !matches!(
                            sub.tx.try_send(Arc::clone(&arc)),
                            Err(TrySendError::Disconnected(_))
                        ),
                        Err(TryRecvError::Disconnected) => false,
                    },
                    Err(TrySendError::Disconnected(_)) => false,
                });
            before > 0 && state.subscribers.is_empty() && state.reservations == 0
        };
        if became_empty {
            self.notify_zero();
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.state.lock().unwrap().subscribers.len()
    }

    pub fn occupied_count(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.subscribers.len() + state.reservations
    }

    /// Drop every item queued before an encoder epoch transition. Subscribers
    /// stay attached and immediately receive the next recovery frame.
    pub fn clear_queued(&self) {
        for subscriber in self.state.lock().unwrap().subscribers.iter() {
            while subscriber.drop_rx.try_recv().is_ok() {}
        }
    }
}

impl<T: Send + Sync + 'static> SubscriptionReservation<T> {
    pub fn commit(mut self, capacity: usize) -> (u64, Receiver<Arc<T>>) {
        let id = self.broadcaster.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = bounded(capacity);
        {
            // Convert the reservation into a subscriber atomically under the
            // lock: occupancy goes reservation -> subscriber without ever
            // dipping to zero, so no zero-occupancy notification is due here.
            let mut state = self.broadcaster.state.lock().unwrap();
            debug_assert!(self.active && state.reservations > 0);
            state.reservations -= 1;
            state.subscribers.push(Subscriber {
                id,
                tx,
                drop_rx: rx.clone(),
            });
        }
        self.active = false;
        self.broadcaster
            .keyframe_requested
            .store(true, Ordering::Release);
        (id, rx)
    }
}

impl<T: Send + Sync + 'static> Drop for SubscriptionReservation<T> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let became_empty = {
            let mut state = self.broadcaster.state.lock().unwrap();
            debug_assert!(state.reservations > 0);
            state.reservations -= 1;
            state.subscribers.is_empty() && state.reservations == 0
        };
        // Abandoning the last reservation can leave the pipeline idle; fire the
        // zero-occupancy callback so it stops, matching unsubscribe().
        if became_empty {
            self.broadcaster.notify_zero();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Broadcaster, MAX_SUBSCRIBERS};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

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

    #[test]
    fn dropped_reservation_restores_subscriber_capacity() {
        let broadcaster = Arc::new(Broadcaster::<u32>::new());
        let mut reservations = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS {
            reservations.push(broadcaster.reserve().expect("reserve"));
        }
        assert!(broadcaster.subscribe(1).is_err());

        reservations.pop();
        assert!(broadcaster.subscribe(1).is_ok());
    }

    #[test]
    fn dropping_final_reservation_notifies_lifecycle_once() {
        let broadcaster = Arc::new(Broadcaster::<u32>::new());
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        broadcaster.set_on_zero(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });

        let reservation = broadcaster.reserve().unwrap();
        drop(reservation);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }
}
