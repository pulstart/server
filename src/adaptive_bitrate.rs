use st_protocol::TransportFeedback;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct AdaptiveBitrateState {
    inner: Mutex<AdaptiveBitrateInner>,
}

struct AdaptiveBitrateInner {
    min_kbps: u32,
    max_kbps: u32,
    current_kbps: u32,
    clients: HashMap<u64, u32>,
}

impl AdaptiveBitrateState {
    pub fn new(initial_kbps: u32, min_kbps: u32, max_kbps: u32) -> Self {
        let min_kbps = min_kbps.min(max_kbps).max(250);
        let max_kbps = max_kbps.max(min_kbps);
        let current_kbps = initial_kbps.clamp(min_kbps, max_kbps);
        Self {
            inner: Mutex::new(AdaptiveBitrateInner {
                min_kbps,
                max_kbps,
                current_kbps,
                clients: HashMap::new(),
            }),
        }
    }

    pub fn limits(&self) -> (u32, u32, u32) {
        let inner = self.inner.lock().unwrap();
        (inner.min_kbps, inner.max_kbps, inner.current_kbps)
    }

    pub fn current_target_kbps(&self) -> u32 {
        self.inner.lock().unwrap().current_kbps
    }

    pub fn register_client(&self, client_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        let current_kbps = inner.current_kbps;
        inner.clients.insert(client_id, current_kbps);
        recompute_target(&mut inner);
    }

    pub fn unregister_client(&self, client_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.clients.remove(&client_id);
        recompute_target(&mut inner);
    }

    pub fn update_client_target(&self, client_id: u64, bitrate_kbps: u32) {
        let mut inner = self.inner.lock().unwrap();
        let bitrate_kbps = bitrate_kbps.clamp(inner.min_kbps, inner.max_kbps);
        inner.clients.insert(client_id, bitrate_kbps);
        recompute_target(&mut inner);
    }

    pub fn reset_all_clients(&self, bitrate_kbps: u32) {
        let mut inner = self.inner.lock().unwrap();
        let bitrate_kbps = bitrate_kbps.clamp(inner.min_kbps, inner.max_kbps);
        for target in inner.clients.values_mut() {
            *target = bitrate_kbps;
        }
        inner.current_kbps = bitrate_kbps;
    }
}

fn recompute_target(inner: &mut AdaptiveBitrateInner) {
    inner.current_kbps = inner
        .clients
        .values()
        .copied()
        .min()
        .unwrap_or(inner.max_kbps)
        .clamp(inner.min_kbps, inner.max_kbps);
}

pub struct ClientRateController {
    recommended_kbps: u32,
    min_kbps: u32,
    max_kbps: u32,
    stable_kbps: u32,
    clean_intervals: u32,
    last_decrease: Instant,
    last_increase: Instant,
    pending_probe_from_kbps: Option<u32>,
    pending_probe_started_at: Option<Instant>,
    probe_failures: u32,
    probe_backoff_until: Instant,
    seen_completed_frame: bool,
}

impl ClientRateController {
    pub fn from_state(state: &AdaptiveBitrateState) -> Self {
        let (min_kbps, max_kbps, current_kbps) = state.limits();
        Self::from_limits_at(min_kbps, max_kbps, current_kbps, Instant::now())
    }

    fn from_limits_at(min_kbps: u32, max_kbps: u32, current_kbps: u32, now: Instant) -> Self {
        Self {
            recommended_kbps: current_kbps,
            min_kbps,
            max_kbps,
            stable_kbps: current_kbps,
            clean_intervals: 0,
            last_decrease: now - Self::DOWNGRADE_COOLDOWN,
            last_increase: now,
            pending_probe_from_kbps: None,
            pending_probe_started_at: None,
            probe_failures: 0,
            probe_backoff_until: now - Duration::from_secs(1),
            seen_completed_frame: false,
        }
    }

    pub fn apply_feedback(&mut self, feedback: TransportFeedback) -> u32 {
        self.apply_feedback_at(feedback, Instant::now())
    }

    const DOWNGRADE_COOLDOWN: Duration = Duration::from_millis(750);
    const UPGRADE_COOLDOWN: Duration = Duration::from_secs(12);
    const CLEAN_INTERVALS_FOR_UPGRADE: u32 = 10;
    const STABLE_INTERVALS_FOR_PROMOTION: u32 = 24;
    const PROBE_FAILURE_WINDOW: Duration = Duration::from_secs(8);
    const BASE_PROBE_BACKOFF: Duration = Duration::from_secs(20);
    const MAX_PROBE_BACKOFF: Duration = Duration::from_secs(120);

    fn apply_feedback_at(&mut self, feedback: TransportFeedback, now: Instant) -> u32 {
        if feedback.received_packets == 0
            && feedback.lost_packets == 0
            && feedback.completed_frames == 0
            && feedback.dropped_frames == 0
        {
            return self.recommended_kbps;
        }

        if feedback.completed_frames > 0 {
            self.seen_completed_frame = true;
        }

        let startup = !self.seen_completed_frame;

        let packet_total = feedback
            .received_packets
            .saturating_add(feedback.lost_packets);
        let packet_loss_ratio = if packet_total > 0 {
            feedback.lost_packets as f32 / packet_total as f32
        } else {
            0.0
        };

        let frame_total = feedback
            .completed_frames
            .saturating_add(feedback.dropped_frames);
        let frame_loss_ratio = if frame_total > 0 {
            feedback.dropped_frames as f32 / frame_total as f32
        } else {
            0.0
        };

        let late_ratio = if feedback.received_packets > 0 {
            feedback.late_packets as f32 / feedback.received_packets as f32
        } else {
            0.0
        };

        let has_any_impairment =
            feedback.dropped_frames > 0 || feedback.lost_packets > 0 || feedback.late_packets > 0;
        let probe_failed = has_any_impairment && self.probe_failed_recently(now);

        if frame_loss_ratio >= 0.10 || packet_loss_ratio >= 0.08 {
            self.clean_intervals = 0;
            if probe_failed {
                self.revert_failed_probe(now);
            } else if startup || self.can_decrease(now) {
                let factor = if startup { 50 } else { 70 };
                self.recommended_kbps = ((self.recommended_kbps as u64 * factor) / 100) as u32;
                self.recommended_kbps = self.recommended_kbps.max(self.min_kbps);
                self.last_decrease = now;
                self.stable_kbps = self.recommended_kbps;
                self.clear_pending_probe();
            }
        } else if frame_loss_ratio >= 0.03 || packet_loss_ratio >= 0.02 || late_ratio >= 0.10 {
            self.clean_intervals = 0;
            if probe_failed {
                self.revert_failed_probe(now);
            } else if startup || self.can_decrease(now) {
                let factor = if startup { 50 } else { 85 };
                self.recommended_kbps = ((self.recommended_kbps as u64 * factor) / 100) as u32;
                self.recommended_kbps = self.recommended_kbps.max(self.min_kbps);
                self.last_decrease = now;
                self.stable_kbps = self.recommended_kbps;
                self.clear_pending_probe();
            }
        } else if has_any_impairment {
            self.clean_intervals = 0;
            if probe_failed {
                self.revert_failed_probe(now);
            }
        } else {
            self.clean_intervals = self.clean_intervals.saturating_add(1);
            self.promote_stable_bitrate(now);
            if self.clean_intervals >= Self::CLEAN_INTERVALS_FOR_UPGRADE
                && self.can_increase(now)
                && self.recommended_kbps < self.max_kbps
            {
                let step = self.increase_step_kbps();
                let next = self.recommended_kbps.saturating_add(step).min(self.max_kbps);
                if next > self.recommended_kbps {
                    self.pending_probe_from_kbps = Some(self.recommended_kbps);
                    self.pending_probe_started_at = Some(now);
                    self.recommended_kbps = next;
                    self.last_increase = now;
                    self.clean_intervals = 0;
                }
            }
        }

        self.recommended_kbps
    }

    fn can_decrease(&self, now: Instant) -> bool {
        now.duration_since(self.last_decrease) >= Self::DOWNGRADE_COOLDOWN
    }

    fn can_increase(&self, now: Instant) -> bool {
        now.duration_since(self.last_increase) >= Self::UPGRADE_COOLDOWN
            && now >= self.probe_backoff_until
    }

    fn probe_failed_recently(&self, now: Instant) -> bool {
        if self.pending_probe_started_at
            .map(|started| now.duration_since(started) <= Self::PROBE_FAILURE_WINDOW)
            .unwrap_or(false)
        {
            return true;
        }

        self.recommended_kbps > self.stable_kbps
            && now.duration_since(self.last_increase) <= Self::PROBE_FAILURE_WINDOW
    }

    fn revert_failed_probe(&mut self, now: Instant) {
        let fallback = self
            .pending_probe_from_kbps
            .unwrap_or(self.stable_kbps)
            .clamp(self.min_kbps, self.max_kbps);
        self.recommended_kbps = fallback;
        self.stable_kbps = fallback;
        self.clean_intervals = 0;
        self.last_decrease = now;
        self.probe_failures = self.probe_failures.saturating_add(1);
        let backoff_secs = (Self::BASE_PROBE_BACKOFF.as_secs()
            * (1u64 << self.probe_failures.saturating_sub(1).min(3)))
            .min(Self::MAX_PROBE_BACKOFF.as_secs());
        self.probe_backoff_until = now + Duration::from_secs(backoff_secs);
        self.clear_pending_probe();
    }

    fn clear_pending_probe(&mut self) {
        self.pending_probe_from_kbps = None;
        self.pending_probe_started_at = None;
    }

    fn promote_stable_bitrate(&mut self, now: Instant) {
        if self.pending_probe_started_at.is_some() {
            if self.clean_intervals >= Self::STABLE_INTERVALS_FOR_PROMOTION
                && self
                    .pending_probe_started_at
                    .map(|started| now.duration_since(started) >= Self::PROBE_FAILURE_WINDOW)
                    .unwrap_or(false)
            {
                self.stable_kbps = self.stable_kbps.max(self.recommended_kbps);
                self.clear_pending_probe();
            }
            return;
        }

        if self.clean_intervals > 0
            && self.clean_intervals % Self::STABLE_INTERVALS_FOR_PROMOTION == 0
        {
            self.stable_kbps = self.stable_kbps.max(self.recommended_kbps);
            self.probe_failures = self.probe_failures.saturating_sub(1);
        }
    }

    fn increase_step_kbps(&self) -> u32 {
        let base = (self.stable_kbps.max(self.recommended_kbps) / 24).clamp(1_000, 4_000);
        let divisor = self.probe_failures.saturating_add(1);
        (base / divisor).max(1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_reduces_bitrate_on_heavy_loss() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 8_000, 8_000, start);
        let next = controller.apply_feedback_at(TransportFeedback {
            interval_ms: 500,
            received_packets: 200,
            lost_packets: 40,
            late_packets: 0,
            completed_frames: 50,
            dropped_frames: 8,
        }, start + Duration::from_millis(500));
        assert!(next < 8_000);
        assert!(next >= 2_000);
    }

    #[test]
    fn controller_reverts_failed_probe_and_waits_longer() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 12_000, 6_000, start);
        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 180,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 60,
            dropped_frames: 0,
        };

        let mut now = start;
        for _ in 0..ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE {
            now += Duration::from_millis(500);
            controller.apply_feedback_at(clean, now);
        }
        now += ClientRateController::UPGRADE_COOLDOWN;
        let probed = controller.apply_feedback_at(clean, now);
        assert!(probed > 6_000);
        assert_eq!(controller.stable_kbps, 6_000);
        assert_eq!(controller.pending_probe_from_kbps, Some(6_000));
        assert_eq!(controller.pending_probe_started_at, Some(now));

        let reverted = controller.apply_feedback_at(
            TransportFeedback {
                interval_ms: 500,
                received_packets: 180,
                lost_packets: 0,
                late_packets: 4,
                completed_frames: 60,
                dropped_frames: 2,
            },
            now + Duration::from_millis(500),
        );
        assert_eq!(reverted, 6_000);

        let before_retry_window = controller.apply_feedback_at(
            clean,
            now + ClientRateController::BASE_PROBE_BACKOFF - Duration::from_secs(1),
        );
        assert_eq!(before_retry_window, 6_000);
    }

    #[test]
    fn controller_only_retries_after_long_backoff() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 12_000, 6_000, start);
        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 180,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 60,
            dropped_frames: 0,
        };

        let mut now = start;
        for _ in 0..ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE {
            now += Duration::from_millis(500);
            controller.apply_feedback_at(clean, now);
        }
        now += ClientRateController::UPGRADE_COOLDOWN;
        controller.apply_feedback_at(clean, now);
        controller.apply_feedback_at(
            TransportFeedback {
                interval_ms: 500,
                received_packets: 180,
                lost_packets: 0,
                late_packets: 2,
                completed_frames: 60,
                dropped_frames: 1,
            },
            now + Duration::from_millis(500),
        );

        let retry_time = now + ClientRateController::BASE_PROBE_BACKOFF + Duration::from_secs(2);
        let mut current = retry_time;
        for _ in 0..ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE {
            current += Duration::from_millis(500);
            controller.apply_feedback_at(clean, current);
        }
        current += ClientRateController::UPGRADE_COOLDOWN;
        let retried = controller.apply_feedback_at(clean, current);
        assert!(retried > 6_000);
    }

    #[test]
    fn controller_recovers_only_after_long_clean_period() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 8_000, 4_000, start);

        let lowered = controller.apply_feedback_at(TransportFeedback {
            interval_ms: 500,
            received_packets: 200,
            lost_packets: 30,
            late_packets: 0,
            completed_frames: 45,
            dropped_frames: 5,
        }, start + Duration::from_millis(500));
        assert!(lowered < 4_000);

        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 180,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 60,
            dropped_frames: 0,
        };

        let mut now = start + Duration::from_secs(13);
        for _ in 0..ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE {
            now += Duration::from_millis(500);
            controller.apply_feedback_at(clean, now);
        }
        now += ClientRateController::UPGRADE_COOLDOWN;
        let recovered = controller.apply_feedback_at(clean, now);
        assert!(recovered >= lowered);
    }
}
