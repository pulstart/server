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
    /// Per-client declared bitrate ceilings (B4). A client's effective target is
    /// clamped to its ceiling so the ABR prober can't overshoot a link the
    /// client already told us is thin.
    client_ceilings: HashMap<u64, u32>,
}

impl AdaptiveBitrateInner {
    fn effective_ceiling(&self, client_id: u64) -> u32 {
        self.client_ceilings
            .get(&client_id)
            .copied()
            .unwrap_or(self.max_kbps)
            .min(self.max_kbps)
    }
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
                client_ceilings: HashMap::new(),
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
        inner.client_ceilings.remove(&client_id);
        recompute_target(&mut inner);
    }

    pub fn update_client_target(&self, client_id: u64, bitrate_kbps: u32) {
        let mut inner = self.inner.lock().unwrap();
        let ceiling = inner.effective_ceiling(client_id);
        let bitrate_kbps = bitrate_kbps.clamp(inner.min_kbps, ceiling);
        inner.clients.insert(client_id, bitrate_kbps);
        recompute_target(&mut inner);
    }

    /// Apply a client's declared bitrate ceiling (B4). Clamped into
    /// `[min_kbps, max_kbps]`; immediately re-clamps that client's current
    /// target so the prober converges without waiting for the next feedback.
    pub fn set_client_ceiling(&self, client_id: u64, max_kbps: u32) {
        let mut inner = self.inner.lock().unwrap();
        let ceiling = max_kbps.clamp(inner.min_kbps, inner.max_kbps);
        inner.client_ceilings.insert(client_id, ceiling);
        if let Some(target) = inner.clients.get_mut(&client_id) {
            *target = (*target).min(ceiling);
        }
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

/// Reserve fraction of the wire budget for FEC parity overhead (B3). The
/// single-XOR path adds a parity packet plus a duplicate FrameStart on top of
/// the encoded video; once the adaptive RS controller (A2) lands it drives this
/// value from measured loss. `ST_FEC_RESERVE_PCT` overrides; default 3 %.
pub fn fec_reserve_pct_from_env() -> u32 {
    std::env::var("ST_FEC_RESERVE_PCT")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3)
        .min(90)
}

/// Convert an on-wire media budget into the encoder bitrate target (B3).
///
/// The on-wire rate is `encoded_video + FEC_parity + audio`, so pointing the
/// encoder directly at the wire budget overshoots it and biases the loss the
/// ABR then measures. Subtract the reserved FEC percentage and the audio
/// bitrate so the actual on-wire rate converges to `wire_kbps`. Floored at
/// `min_kbps` so fat audio / high FEC can't starve video below the minimum.
pub fn encoder_target_kbps(
    wire_kbps: u32,
    audio_kbps: u32,
    fec_reserve_pct: u32,
    min_kbps: u32,
) -> u32 {
    let fec_reserve_pct = fec_reserve_pct.min(90);
    let after_fec = ((wire_kbps as u64) * (100 - fec_reserve_pct) as u64 / 100) as u32;
    after_fec.saturating_sub(audio_kbps).max(min_kbps)
}

/// Adaptive FEC-strength controller (A2). Drives the RS parity percentage from
/// measured loss: raises immediately when packets are lost (protect *before*
/// cutting bitrate, per the plan), decays slowly back to the floor over clean
/// intervals. Shares the spirit of [`ClientRateController`]'s fast-down/slow-up
/// hysteresis so FEC and bitrate don't oscillate against each other.
///
/// RS is the default FEC mode (A1), so this controller is live by default; on
/// `ST_FEC=xor` the slicer ignores `fec_pct` and this output is inert.
pub struct FecController {
    floor_pct: u16,
    max_pct: u16,
    current_pct: u16,
    last_raise: Instant,
    last_decay: Instant,
}

impl FecController {
    /// Raise reacts within one feedback window; decay waits for a sustained
    /// clean period so a single good window doesn't drop protection.
    const DECAY_COOLDOWN: Duration = Duration::from_secs(4);
    const DECAY_STEP: u16 = 3;

    pub fn new(floor_pct: u16, max_pct: u16) -> Self {
        let floor_pct = floor_pct.min(100);
        let max_pct = max_pct.clamp(floor_pct, 100);
        let now = Instant::now();
        Self {
            floor_pct,
            max_pct,
            current_pct: floor_pct,
            last_raise: now,
            last_decay: now,
        }
    }

    /// Build from env: floor = `ST_FEC_PCT` (default 0 — clean links decay to the
    /// floor and pay only `ST_FEC_MIN_PARITY` recovery shards, ≈ XOR's single
    /// parity packet), ceiling = `ST_FEC_MAX_PCT` (default 50). The controller
    /// ramps `fec_pct` up on measured loss and decays back to the floor, so RS
    /// default-on costs ≈ XOR when the link is clean but recovers multiple
    /// losses/unit when it is not.
    pub fn from_env() -> Self {
        let floor = std::env::var("ST_FEC_PCT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(0)
            .min(100);
        let max = std::env::var("ST_FEC_MAX_PCT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(50)
            .min(100);
        Self::new(floor, max)
    }

    pub fn current_pct(&self) -> u16 {
        self.current_pct
    }

    pub fn apply_feedback(&mut self, feedback: &TransportFeedback) -> u16 {
        self.apply_feedback_at(feedback, Instant::now())
    }

    fn apply_feedback_at(&mut self, feedback: &TransportFeedback, now: Instant) -> u16 {
        let total = feedback
            .received_packets
            .saturating_add(feedback.lost_packets);
        let loss_ratio = if total > 0 {
            feedback.lost_packets as f32 / total as f32
        } else {
            0.0
        };
        let losing = feedback.lost_packets > 0 || feedback.dropped_frames > 0;

        if losing {
            // Parity to cover ratio p (with margin) ≈ 1.5·p. Jump straight to the
            // needed level — FEC must protect the very next frames.
            let want = ((loss_ratio * 150.0).ceil() as u16)
                .max(self.floor_pct + 1)
                .clamp(self.floor_pct, self.max_pct);
            if want > self.current_pct {
                self.current_pct = want;
                self.last_raise = now;
            }
            // Any loss resets the decay clock.
            self.last_decay = now;
        } else if self.current_pct > self.floor_pct
            && now.duration_since(self.last_decay) >= Self::DECAY_COOLDOWN
        {
            self.current_pct = self
                .current_pct
                .saturating_sub(Self::DECAY_STEP)
                .max(self.floor_pct);
            self.last_decay = now;
        }
        self.current_pct
    }
}

/// Adaptive verbatim audio-redundancy controller (E5). Ramps redundancy depth
/// up by one per lossy feedback window (capped at the configured max) and decays
/// back toward 0 over sustained clean intervals, so a perfect LAN pays no
/// redundancy overhead while a lossy path gets burst protection. Default-on;
/// `ST_AUDIO_ADAPTIVE_REDUNDANCY=0` restores the fixed legacy depth.
pub struct AudioRedundancyController {
    max_depth: u8,
    current: u8,
    last_decay: Instant,
}

impl AudioRedundancyController {
    const DECAY_COOLDOWN: Duration = Duration::from_secs(5);

    pub fn new(max_depth: u8) -> Self {
        Self {
            max_depth,
            current: 0,
            last_decay: Instant::now(),
        }
    }

    pub fn current_depth(&self) -> u8 {
        self.current
    }

    pub fn apply_feedback(&mut self, feedback: &TransportFeedback) -> u8 {
        self.apply_feedback_at(feedback, Instant::now())
    }

    fn apply_feedback_at(&mut self, feedback: &TransportFeedback, now: Instant) -> u8 {
        let losing = feedback.lost_packets > 0 || feedback.dropped_frames > 0;
        if losing {
            if self.current < self.max_depth {
                self.current += 1;
            }
            self.last_decay = now;
        } else if self.current > 0 && now.duration_since(self.last_decay) >= Self::DECAY_COOLDOWN {
            self.current -= 1;
            self.last_decay = now;
        }
        self.current
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

    const DOWNGRADE_COOLDOWN: Duration = Duration::from_secs(2);
    const UPGRADE_COOLDOWN: Duration = Duration::from_secs(8);
    const CLEAN_INTERVALS_FOR_UPGRADE: u32 = 6;
    const STABLE_INTERVALS_FOR_PROMOTION: u32 = 18;
    const PROBE_FAILURE_WINDOW: Duration = Duration::from_secs(8);
    const BASE_PROBE_BACKOFF: Duration = Duration::from_secs(16);
    const MAX_PROBE_BACKOFF: Duration = Duration::from_secs(90);
    /// One-way-delay trend (µs over the feedback window) above which we treat the
    /// bottleneck queue as building and refuse to probe the bitrate up — react to
    /// congestion *before* it turns into loss (B1, GCC-style delay gradient).
    const OWD_RISING_US: i32 = 4_000;

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

        if frame_loss_ratio >= 0.15 || packet_loss_ratio >= 0.10 {
            // Heavy loss — significant reduction
            self.clean_intervals = 0;
            if probe_failed {
                self.revert_failed_probe(now);
            } else if startup || self.can_decrease(now) {
                let factor = if startup { 50 } else { 80 };
                self.recommended_kbps = ((self.recommended_kbps as u64 * factor) / 100) as u32;
                self.recommended_kbps = self.recommended_kbps.max(self.min_kbps);
                self.last_decrease = now;
                self.stable_kbps = self.recommended_kbps;
                self.clear_pending_probe();
            }
        } else if frame_loss_ratio >= 0.05 || packet_loss_ratio >= 0.04 || late_ratio >= 0.15 {
            // Moderate loss — gentle reduction
            self.clean_intervals = 0;
            if probe_failed {
                self.revert_failed_probe(now);
            } else if startup || self.can_decrease(now) {
                let factor = if startup { 50 } else { 92 };
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
        } else if feedback.owd_trend_us > Self::OWD_RISING_US {
            // B1: no loss yet, but one-way delay is trending up — the bottleneck
            // queue is filling. Hold the bitrate (don't probe up) and reset the
            // clean-interval counter so a probe only resumes after the delay
            // trend flattens. This is the pre-loss congestion signal Sunshine
            // acts on for nothing.
            self.clean_intervals = 0;
            self.promote_stable_bitrate(now);
        } else {
            self.clean_intervals = self.clean_intervals.saturating_add(1);
            self.promote_stable_bitrate(now);
            if self.clean_intervals >= Self::CLEAN_INTERVALS_FOR_UPGRADE
                && self.can_increase(now)
                && self.recommended_kbps < self.max_kbps
            {
                let step = self.increase_step_kbps();
                let mut next = self
                    .recommended_kbps
                    .saturating_add(step)
                    .min(self.max_kbps);
                // B1: clamp the probe ceiling to ~110% of measured receive rate
                // so we never probe far past what the path is actually carrying.
                if feedback.recv_video_kbps > 0 {
                    let capacity_ceiling =
                        ((feedback.recv_video_kbps as u64 * 110) / 100).min(u32::MAX as u64) as u32;
                    next = next.min(capacity_ceiling.max(self.recommended_kbps));
                }
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
        if self
            .pending_probe_started_at
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
            && self
                .clean_intervals
                .is_multiple_of(Self::STABLE_INTERVALS_FOR_PROMOTION)
        {
            self.stable_kbps = self.stable_kbps.max(self.recommended_kbps);
            self.probe_failures = self.probe_failures.saturating_sub(1);
        }
    }

    fn increase_step_kbps(&self) -> u32 {
        let base = (self.stable_kbps.max(self.recommended_kbps) / 12).clamp(2_000, 8_000);
        let divisor = self.probe_failures.saturating_add(1);
        (base / divisor).max(1_500)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_target_subtracts_fec_and_audio_overhead() {
        // 20 Mbps wire, 10% FEC reserve, 256 kbps audio → 18000 − 256 = 17744.
        assert_eq!(encoder_target_kbps(20_000, 256, 10, 1_000), 17_744);
        // No overhead → unchanged.
        assert_eq!(encoder_target_kbps(20_000, 0, 0, 1_000), 20_000);
        // Floor wins when overhead would push below min.
        assert_eq!(encoder_target_kbps(1_000, 2_000, 10, 5_000), 5_000);
    }

    #[test]
    fn fec_controller_raises_on_loss_and_decays_clean() {
        let start = Instant::now();
        let mut fec = FecController::new(10, 50);
        assert_eq!(fec.current_pct(), 10);

        // 20% loss → jumps up immediately, well above the floor.
        let lossy = TransportFeedback {
            interval_ms: 500,
            received_packets: 160,
            lost_packets: 40,
            completed_frames: 50,
            ..Default::default()
        };
        let raised = fec.apply_feedback_at(&lossy, start + Duration::from_millis(500));
        assert!(raised > 10, "FEC must rise on loss, got {raised}");
        assert!(raised <= 50, "FEC capped at max");

        // Clean windows below the decay cooldown hold protection.
        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 200,
            completed_frames: 60,
            ..Default::default()
        };
        let mut now = start + Duration::from_millis(500);
        now += Duration::from_secs(1);
        let held = fec.apply_feedback_at(&clean, now);
        assert_eq!(held, raised, "must not decay before cooldown");

        // After the decay cooldown, it steps back down toward the floor.
        now += FecController::DECAY_COOLDOWN;
        let decayed = fec.apply_feedback_at(&clean, now);
        assert!(decayed < raised, "FEC must decay on sustained clean link");
        assert!(decayed >= 10, "never below floor");
    }

    /// Closed-loop guard for RS-default-on: the controller floor of 0 means a
    /// clean link emits exactly one parity packet (== XOR cost) yet still
    /// recovers one loss, and a lossy window ramps the parity count up so RS
    /// recovers multiple losses/unit. Spans FecController → FrameSlicer(RS) →
    /// FrameAssembler, the path the transport wires at runtime.
    #[test]
    fn rs_default_floor_zero_is_xor_cost_when_clean_then_ramps_to_multi_recovery() {
        use st_protocol::frame_assembler::FrameAssembler;
        use st_protocol::frame_slicer::{FecConfig, FrameSlicer};
        use st_protocol::packet::{frame_type, FecMode};
        use st_protocol::FrameTimingMeta;

        let start = Instant::now();
        let mut fec = FecController::new(0, 50); // the new default floor
        assert_eq!(fec.current_pct(), 0);

        // A clean window keeps the floor at 0.
        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 500,
            completed_frames: 60,
            ..Default::default()
        };
        let clean_pct = fec.apply_feedback_at(&clean, start + Duration::from_millis(500));
        assert_eq!(clean_pct, 0, "clean link sits at floor 0");

        // RS slicer at pct=0, min_parity=1 ⇒ exactly one parity packet.
        let payload = vec![0xC3u8; 6_000];
        let slice = |pct: u16, fid: u32| {
            let mut slicer = FrameSlicer::with_config(
                600,
                FecConfig {
                    mode: FecMode::Rs,
                    fec_pct: pct,
                    min_parity: 1,
                },
            );
            let (d, p) = slicer.slice_with_meta_parts(
                &payload,
                fid,
                FrameTimingMeta::default(),
                frame_type::IDR,
            );
            (d.to_vec(), p.to_vec())
        };

        let (data, parity) = slice(clean_pct, 1);
        assert!(data.len() > 1, "payload must be multi-packet");
        assert_eq!(
            parity.len(),
            1,
            "floor-0 clean link emits a single parity packet, matching XOR"
        );

        // That one parity packet still recovers a single lost data packet.
        let recover = |data: &[Vec<u8>], parity: &[Vec<u8>], drop: &[usize]| {
            let mut asm = FrameAssembler::new();
            let mut done = None;
            for (i, p) in data.iter().enumerate() {
                if drop.contains(&i) {
                    continue;
                }
                if let Some(f) = asm.ingest(p) {
                    done = Some(f);
                }
            }
            for p in parity {
                if let Some(f) = asm.ingest(p) {
                    done = Some(f);
                }
            }
            done
        };
        assert_eq!(
            recover(&data, &parity, &[2])
                .expect("1 parity recovers 1 loss")
                .data,
            payload
        );

        // A lossy window ramps the parity above the floor.
        let lossy = TransportFeedback {
            interval_ms: 500,
            received_packets: 80,
            lost_packets: 20,
            completed_frames: 25,
            ..Default::default()
        };
        let ramped = fec.apply_feedback_at(&lossy, start + Duration::from_secs(1));
        assert!(
            ramped >= 2,
            "loss must ramp FEC above the floor, got {ramped}"
        );

        let (data2, parity2) = slice(ramped, 2);
        assert!(
            parity2.len() >= 2,
            "ramped FEC emits multiple parity packets, got {}",
            parity2.len()
        );
        // Drop two data packets — only RS multi-recovery can rebuild this.
        assert_eq!(
            recover(&data2, &parity2, &[2, 4])
                .expect("ramped RS recovers two losses")
                .data,
            payload
        );
    }

    #[test]
    fn audio_redundancy_ramps_on_loss_and_decays() {
        let start = Instant::now();
        let mut ctl = AudioRedundancyController::new(3);
        assert_eq!(ctl.current_depth(), 0);

        let lossy = TransportFeedback {
            interval_ms: 500,
            received_packets: 100,
            lost_packets: 5,
            ..Default::default()
        };
        let mut now = start;
        // Two lossy windows ramp depth to 2 (capped at max 3).
        now += Duration::from_millis(500);
        assert_eq!(ctl.apply_feedback_at(&lossy, now), 1);
        now += Duration::from_millis(500);
        assert_eq!(ctl.apply_feedback_at(&lossy, now), 2);

        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 100,
            ..Default::default()
        };
        // Clean but within cooldown → holds.
        now += Duration::from_secs(1);
        assert_eq!(ctl.apply_feedback_at(&clean, now), 2);
        // After cooldown → decays one step.
        now += AudioRedundancyController::DECAY_COOLDOWN;
        assert_eq!(ctl.apply_feedback_at(&clean, now), 1);
    }

    #[test]
    fn controller_holds_on_rising_owd_without_loss() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 12_000, 6_000, start);
        let clean_but_congested = TransportFeedback {
            interval_ms: 500,
            received_packets: 180,
            completed_frames: 60,
            owd_trend_us: 8_000, // queue building
            recv_video_kbps: 6_000,
            ..Default::default()
        };

        let mut now = start;
        // Even after many "clean" (no-loss) intervals, a rising OWD must prevent
        // the prober from increasing the bitrate.
        for _ in 0..(ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE + 4) {
            now += Duration::from_millis(500);
            controller.apply_feedback_at(clean_but_congested, now);
        }
        now += ClientRateController::UPGRADE_COOLDOWN;
        let held = controller.apply_feedback_at(clean_but_congested, now);
        assert_eq!(held, 6_000, "must not probe up while OWD is rising");
    }

    #[test]
    fn controller_reduces_bitrate_on_heavy_loss() {
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(2_000, 8_000, 8_000, start);
        let next = controller.apply_feedback_at(
            TransportFeedback {
                interval_ms: 500,
                received_packets: 200,
                lost_packets: 40,
                late_packets: 0,
                completed_frames: 50,
                dropped_frames: 8,
                ..Default::default()
            },
            start + Duration::from_millis(500),
        );
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
            ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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

        let lowered = controller.apply_feedback_at(
            TransportFeedback {
                interval_ms: 500,
                received_packets: 200,
                lost_packets: 30,
                late_packets: 0,
                completed_frames: 45,
                dropped_frames: 5,
                ..Default::default()
            },
            start + Duration::from_millis(500),
        );
        assert!(lowered < 4_000);

        let clean = TransportFeedback {
            interval_ms: 500,
            received_packets: 180,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 60,
            dropped_frames: 0,
            ..Default::default()
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
