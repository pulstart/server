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

/// Utility-driven controller for the delayed-duplicate FrameStart (A/B probe).
///
/// The duplicate only pays off when *FrameStart loss* is making frames fail to
/// complete. On a congested link the extra packet can make loss *worse*, and on
/// a link where frames aren't failing it is pure waste. So rather than send it
/// whenever any loss appears, this measures whether it actually helps:
/// - frames start failing → turn it on as a **probe** and record the no-dup
///   frame-loss baseline;
/// - watch `PROBE_WINDOWS` feedback windows; keep it on **only** if frame loss
///   fell to ≤ `HELP_RATIO` of the baseline (it genuinely helped);
/// - if frame loss is unchanged (useless) or higher (harmful), turn it off and
///   back off exponentially before probing again;
/// - once the link stops dropping frames, stop on its own (nothing to protect).
///
/// Advisory only — the transport's `ST_DUP_FRAMESTART=off|on` modes still hard-
/// override; this drives the default `auto` mode.
pub struct DupFirstController {
    enabled: bool,
    probing: bool,
    baseline: f32,
    probe_sum: f32,
    probe_count: u32,
    clean_on_windows: u32,
    failures: u32,
    backoff_until: Instant,
}

impl Default for DupFirstController {
    fn default() -> Self {
        Self::new()
    }
}

impl DupFirstController {
    const PROBE_WINDOWS: u32 = 3;
    /// Frame loss must fall to ≤70% of the no-dup baseline to count as helping.
    const HELP_RATIO: f32 = 0.7;
    /// Don't bother probing unless at least this fraction of frames are failing.
    const MIN_BASELINE: f32 = 0.01;
    /// Clean windows while enabled before we stop (nothing left to protect).
    const CLEAN_ON_TO_DISABLE: u32 = 4;
    const BASE_BACKOFF: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self {
            enabled: false,
            probing: false,
            baseline: 0.0,
            probe_sum: 0.0,
            probe_count: 0,
            clean_on_windows: 0,
            failures: 0,
            // No initial backoff — the first frame-loss window may probe at once.
            backoff_until: Instant::now() - Self::BASE_BACKOFF,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn apply_feedback(&mut self, feedback: &TransportFeedback) -> bool {
        self.apply_feedback_at(feedback, Instant::now())
    }

    fn apply_feedback_at(&mut self, feedback: &TransportFeedback, now: Instant) -> bool {
        let total_frames = feedback
            .completed_frames
            .saturating_add(feedback.dropped_frames);
        let frame_loss = if total_frames > 0 {
            feedback.dropped_frames as f32 / total_frames as f32
        } else {
            0.0
        };
        let frames_failing = feedback.dropped_frames > 0;

        if !self.enabled {
            // Off: only start a probe when frames are actually failing (the thing
            // the duplicate fixes) and we're past any backoff from a prior dud.
            if frames_failing && frame_loss >= Self::MIN_BASELINE && now >= self.backoff_until {
                self.enabled = true;
                self.probing = true;
                self.baseline = frame_loss;
                self.probe_sum = 0.0;
                self.probe_count = 0;
                self.clean_on_windows = 0;
            }
        } else if self.probing {
            self.probe_sum += frame_loss;
            self.probe_count += 1;
            if self.probe_count >= Self::PROBE_WINDOWS {
                let avg = self.probe_sum / self.probe_count as f32;
                self.probing = false;
                if avg <= self.baseline * Self::HELP_RATIO {
                    // Genuinely reduced frame loss — keep it on, clear backoff.
                    self.failures = 0;
                    self.clean_on_windows = 0;
                } else {
                    // Useless (flat) or harmful (worse) — stop and back off so we
                    // don't keep paying for a duplicate that isn't earning it.
                    self.enabled = false;
                    self.failures = self.failures.saturating_add(1);
                    let shift = self.failures.saturating_sub(1).min(3);
                    self.backoff_until = now + Self::BASE_BACKOFF * (1u32 << shift);
                }
            }
        } else {
            // Proven helpful and running. Stop once frames stop failing — the
            // link recovered and there's nothing left to protect.
            if frames_failing {
                self.clean_on_windows = 0;
            } else {
                self.clean_on_windows = self.clean_on_windows.saturating_add(1);
                if self.clean_on_windows >= Self::CLEAN_ON_TO_DISABLE {
                    self.enabled = false;
                    self.clean_on_windows = 0;
                    // Recovered on its own (not a dud) — allow a quick re-probe.
                    self.failures = 0;
                    self.backoff_until = now;
                }
            }
        }
        self.enabled
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

/// One feedback window's worth of encode-pipeline load, fed to
/// [`AdaptiveFrameRate`]. Built in the encode loop from the frames actually
/// produced this window (capture + copy + encode), not just the encoder call.
#[derive(Clone, Copy, Debug)]
pub struct EncodeLoadSample {
    /// Frames actually encoded per second over the window. The *primary*
    /// can't-sustain signal: if the box can't hold the target cadence the
    /// delivered rate sags below it regardless of where the bottleneck is
    /// (KMS GPU-copy, capture, or encode).
    pub delivered_fps: f32,
    /// Average encoder call time over the window (ms). Used only as an upward
    /// guard — never step up if the encoder itself is already near budget.
    pub avg_encode_ms: f32,
    /// Fraction of frames whose end-to-end time exceeded the frame budget.
    pub overrun_ratio: f32,
}

/// Per-window accumulator that turns per-frame encode timings into an
/// [`EncodeLoadSample`] once enough of a window has elapsed.
pub struct EncodeRateTracker {
    window_start: Instant,
    frames: u32,
    encode_us_sum: u64,
    overrun_frames: u32,
}

impl EncodeRateTracker {
    const WINDOW: Duration = Duration::from_millis(1000);
    const MIN_FRAMES: u32 = 10;

    pub fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            frames: 0,
            encode_us_sum: 0,
            overrun_frames: 0,
        }
    }

    /// Record one encoded frame: its end-to-end time and the per-frame budget
    /// (both microseconds). A frame counts as an overrun when it took longer
    /// than the budget for the current target fps.
    pub fn record(&mut self, encode_us: u64, budget_us: u64) {
        self.frames = self.frames.saturating_add(1);
        self.encode_us_sum = self.encode_us_sum.saturating_add(encode_us);
        if budget_us > 0 && encode_us > budget_us {
            self.overrun_frames = self.overrun_frames.saturating_add(1);
        }
    }

    /// Emit a sample and reset once the window has elapsed with enough frames.
    pub fn take_sample(&mut self, now: Instant) -> Option<EncodeLoadSample> {
        let elapsed = now.duration_since(self.window_start).as_secs_f32();
        // A long span with too few frames means the encoder went idle (no
        // subscribers). Restart the window without emitting so the idle gap
        // isn't misread as the box failing to sustain the target fps.
        if elapsed > 3.0 && self.frames < Self::MIN_FRAMES {
            *self = Self::new(now);
            return None;
        }
        if elapsed < Self::WINDOW.as_secs_f32() || self.frames < Self::MIN_FRAMES {
            return None;
        }
        let frames = self.frames.max(1) as f32;
        let sample = EncodeLoadSample {
            delivered_fps: self.frames as f32 / elapsed,
            avg_encode_ms: (self.encode_us_sum as f32 / frames) / 1000.0,
            overrun_ratio: self.overrun_frames as f32 / frames,
        };
        *self = Self::new(now);
        Some(sample)
    }
}

/// Adaptive *encode* frame-rate controller (latency-first).
///
/// At high resolution the GPU/encoder may not sustain a high target fps; the
/// KMS capture then overruns its timerfd and delivers an irregular cadence,
/// which the client reads as jitter and absorbs by growing its playout buffer
/// — added latency for no benefit. This controller steps the encode fps down a
/// fixed ladder until the box holds a steady cadence, and probes cautiously
/// back up only after a long clean period (with exponential backoff on repeated
/// failed up-probes, so it settles instead of oscillating).
///
/// Default-on; `ST_ADAPTIVE_FPS=0`/`false`/`no`/`off` forces the fixed target.
pub struct AdaptiveFrameRate {
    enabled: bool,
    ceiling_fps: u32,
    floor_fps: u32,
    current_fps: u32,
    ladder: Vec<u32>,
    clean_windows: u32,
    last_change: Instant,
    last_change_was_up: bool,
    up_failures: u32,
    up_backoff_until: Instant,
}

impl AdaptiveFrameRate {
    const LADDER: [u32; 5] = [120, 90, 60, 48, 30];
    const FLOOR_FPS: u32 = 30;
    /// Latency-first: react down quickly, but not so fast we thrash on a single
    /// noisy window.
    const DOWN_COOLDOWN: Duration = Duration::from_secs(2);
    const UP_COOLDOWN: Duration = Duration::from_secs(8);
    /// Sustained clean windows required before an up-probe.
    const CLEAN_WINDOWS_FOR_UP: u32 = 6;
    /// A down-step within this long of an up-step counts the up-probe as failed.
    const PROBE_WINDOW: Duration = Duration::from_secs(6);
    const BASE_UP_BACKOFF: Duration = Duration::from_secs(15);
    /// Can't-sustain thresholds (latency-first — bias toward stepping down).
    const OVERRUN_TRIP: f32 = 0.10;
    const DELIVER_TRIP: f32 = 0.92;
    /// Upward guard: only probe up with real encoder headroom and a near-full
    /// delivered cadence at the current level.
    const UP_ENCODE_HEADROOM: f32 = 0.65;
    const UP_DELIVER_OK: f32 = 0.97;

    pub fn from_env(ceiling_fps: u32, now: Instant) -> Self {
        Self::with_enabled(adaptive_fps_enabled_from_env(), ceiling_fps, now)
    }

    fn with_enabled(enabled: bool, ceiling_fps: u32, now: Instant) -> Self {
        let ceiling_fps = ceiling_fps.max(1);
        let floor_fps = Self::FLOOR_FPS.min(ceiling_fps);
        Self {
            enabled,
            ceiling_fps,
            floor_fps,
            current_fps: ceiling_fps,
            ladder: Self::build_ladder(ceiling_fps, floor_fps),
            clean_windows: 0,
            last_change: now,
            last_change_was_up: false,
            up_failures: 0,
            up_backoff_until: now,
        }
    }

    /// Descending ladder of allowed fps within `[floor, ceiling]`, with the
    /// ceiling guaranteed as the top entry (so a non-standard request like 75
    /// still has a home).
    fn build_ladder(ceiling: u32, floor: u32) -> Vec<u32> {
        let mut v: Vec<u32> = Self::LADDER
            .iter()
            .copied()
            .filter(|&f| f < ceiling && f >= floor)
            .collect();
        v.push(ceiling);
        if !v.contains(&floor) {
            v.push(floor);
        }
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.dedup();
        v
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn current_fps(&self) -> u32 {
        self.current_fps
    }

    fn step_down(&self) -> Option<u32> {
        // Ladder is descending; the first entry strictly below current is the
        // largest sustainable-candidate below it.
        self.ladder
            .iter()
            .copied()
            .find(|&f| f < self.current_fps && f >= self.floor_fps)
    }

    fn step_up(&self) -> Option<u32> {
        // Smallest ladder entry strictly above current, never past the ceiling.
        self.ladder
            .iter()
            .rev()
            .copied()
            .find(|&f| f > self.current_fps && f <= self.ceiling_fps)
    }

    /// Feed one window's load. Returns `Some(new_fps)` when the target should
    /// change (caller rebuilds the encoder + repoints capture), else `None`.
    pub fn apply_at(&mut self, sample: &EncodeLoadSample, now: Instant) -> Option<u32> {
        if !self.enabled {
            return None;
        }

        let cant_sustain = sample.overrun_ratio > Self::OVERRUN_TRIP
            || sample.delivered_fps < self.current_fps as f32 * Self::DELIVER_TRIP;

        if cant_sustain {
            self.clean_windows = 0;
            if now.duration_since(self.last_change) < Self::DOWN_COOLDOWN {
                return None;
            }
            let next = self.step_down()?;
            // If we dropped right after probing up, that up-probe failed — back
            // the next attempt off exponentially so we settle.
            if self.last_change_was_up && now.duration_since(self.last_change) <= Self::PROBE_WINDOW
            {
                self.up_failures = self.up_failures.saturating_add(1);
                let shift = self.up_failures.saturating_sub(1).min(3);
                self.up_backoff_until = now + Self::BASE_UP_BACKOFF * (1u32 << shift);
            }
            self.current_fps = next;
            self.last_change = now;
            self.last_change_was_up = false;
            return Some(next);
        }

        // Sustaining the current level.
        self.clean_windows = self.clean_windows.saturating_add(1);
        // Reward a long clean stretch by forgiving one past up-failure.
        if self
            .clean_windows
            .is_multiple_of(Self::CLEAN_WINDOWS_FOR_UP * 4)
        {
            self.up_failures = self.up_failures.saturating_sub(1);
        }

        let budget_ms = 1000.0 / self.current_fps as f32;
        let has_headroom = sample.avg_encode_ms < budget_ms * Self::UP_ENCODE_HEADROOM
            && sample.delivered_fps >= self.current_fps as f32 * Self::UP_DELIVER_OK;
        if has_headroom
            && self.current_fps < self.ceiling_fps
            && self.clean_windows >= Self::CLEAN_WINDOWS_FOR_UP
            && now >= self.up_backoff_until
            && now.duration_since(self.last_change) >= Self::UP_COOLDOWN
        {
            if let Some(next) = self.step_up() {
                self.current_fps = next;
                self.last_change = now;
                self.last_change_was_up = true;
                self.clean_windows = 0;
                return Some(next);
            }
        }
        None
    }
}

/// `ST_ADAPTIVE_FPS` tri-state: default-on, disabled by `0`/`false`/`no`/`off`.
pub fn adaptive_fps_enabled_from_env() -> bool {
    match std::env::var("ST_ADAPTIVE_FPS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
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

        // Late packets are reordering/redundancy, not loss — a healthy FEC stream
        // always carries some (parity + delayed-duplicate FrameStart arrive after
        // their frame completes). They depress the bitrate only through the
        // dedicated `late_ratio >= 0.15` branch above. Treating *any* late packet
        // as a hard impairment here would zero the clean-interval counter every
        // window, so the up-probe could never fire and the controller stayed
        // pinned at its floor on an otherwise clean link.
        let has_any_impairment = feedback.dropped_frames > 0 || feedback.lost_packets > 0;
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
    fn baseline_late_from_fec_does_not_block_upprobe() {
        // Regression guard for the "stuck at floor" bug: a healthy FEC stream
        // reports a steady trickle of `late_packets` (one per multi-packet frame
        // from parity + the delayed-duplicate FrameStart). With no loss and a low
        // late ratio, the controller must still accumulate clean intervals and
        // probe the bitrate up — not pin itself at the floor.
        let start = Instant::now();
        let mut controller = ClientRateController::from_limits_at(5_000, 100_000, 5_000, start);

        // 1121 received, 66 late (≈1 per frame, late_ratio 0.059 — well under the
        // 0.15 reduce ratio), zero loss — exactly the overlay numbers (rx=1121,
        // late=66, loss=0).
        let clean_with_fec_late = TransportFeedback {
            interval_ms: 500,
            received_packets: 1121,
            lost_packets: 0,
            late_packets: 66,
            completed_frames: 66,
            dropped_frames: 0,
            ..Default::default()
        };

        let mut now = start;
        for _ in 0..ClientRateController::CLEAN_INTERVALS_FOR_UPGRADE {
            now += Duration::from_millis(500);
            controller.apply_feedback_at(clean_with_fec_late, now);
        }
        now += ClientRateController::UPGRADE_COOLDOWN;
        let probed = controller.apply_feedback_at(clean_with_fec_late, now);
        assert!(
            probed > 5_000,
            "baseline FEC late must not prevent the up-probe; got {probed}"
        );
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

    fn frame_fb(completed: u32, dropped: u32) -> TransportFeedback {
        TransportFeedback {
            interval_ms: 500,
            completed_frames: completed,
            dropped_frames: dropped,
            ..Default::default()
        }
    }

    #[test]
    fn dup_off_when_no_frame_loss() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        assert!(!d.apply_feedback_at(&frame_fb(60, 0), start));
        assert!(!d.enabled());
    }

    #[test]
    fn dup_probes_on_frame_loss() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        // 5 of 65 frames failing → above MIN_BASELINE → start a probe.
        assert!(d.apply_feedback_at(&frame_fb(60, 5), start));
        assert!(d.enabled());
    }

    #[test]
    fn dup_stays_on_only_when_it_reduces_frame_loss() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        let mut t = start;
        d.apply_feedback_at(&frame_fb(60, 5), t); // baseline 5/65 ≈ 0.077, probing
                                                  // Three probe windows where the duplicate clearly cut frame loss.
        for _ in 0..3 {
            t += Duration::from_millis(500);
            d.apply_feedback_at(&frame_fb(64, 1), t); // ≈0.015 ≤ 0.077·0.7
        }
        assert!(d.enabled(), "must stay on when it demonstrably helps");
    }

    #[test]
    fn dup_disables_when_not_helping() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        let mut t = start;
        d.apply_feedback_at(&frame_fb(60, 5), t); // baseline ≈0.077
                                                  // Frame loss unchanged with the duplicate on → it isn't earning it.
        for _ in 0..3 {
            t += Duration::from_millis(500);
            d.apply_feedback_at(&frame_fb(60, 5), t);
        }
        assert!(!d.enabled(), "flat frame loss ⇒ stop sending the duplicate");
    }

    #[test]
    fn dup_disables_when_link_clears() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        let mut t = start;
        // Get it on + proven helpful.
        d.apply_feedback_at(&frame_fb(60, 5), t);
        for _ in 0..3 {
            t += Duration::from_millis(500);
            d.apply_feedback_at(&frame_fb(64, 1), t);
        }
        assert!(d.enabled());
        // Link recovers: no frames failing for several windows → stop.
        for _ in 0..DupFirstController::CLEAN_ON_TO_DISABLE {
            t += Duration::from_millis(500);
            d.apply_feedback_at(&frame_fb(64, 0), t);
        }
        assert!(!d.enabled(), "nothing left to protect ⇒ stop");
    }

    #[test]
    fn dup_backs_off_after_a_useless_probe() {
        let start = Instant::now();
        let mut d = DupFirstController::new();
        let mut t = start;
        // Useless probe → disable + backoff.
        d.apply_feedback_at(&frame_fb(60, 5), t);
        for _ in 0..3 {
            t += Duration::from_millis(500);
            d.apply_feedback_at(&frame_fb(60, 5), t);
        }
        assert!(!d.enabled());
        // Still failing, but within backoff → must NOT immediately re-enable.
        t += Duration::from_millis(500);
        assert!(!d.apply_feedback_at(&frame_fb(60, 5), t));
        // Past the backoff → may probe again.
        t += Duration::from_secs(11);
        assert!(d.apply_feedback_at(&frame_fb(60, 5), t));
    }

    fn load(delivered_fps: f32, avg_encode_ms: f32, overrun_ratio: f32) -> EncodeLoadSample {
        EncodeLoadSample {
            delivered_fps,
            avg_encode_ms,
            overrun_ratio,
        }
    }

    #[test]
    fn adaptive_fps_disabled_never_changes() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(false, 120, start);
        let res = afr.apply_at(&load(20.0, 30.0, 1.0), start + Duration::from_secs(5));
        assert_eq!(res, None);
        assert_eq!(afr.current_fps(), 120);
    }

    #[test]
    fn adaptive_fps_steps_down_on_low_delivered() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 120, start);
        // 66 fps delivered at a 120 target — far under 0.92·120.
        let res = afr.apply_at(&load(66.0, 14.0, 0.0), start + Duration::from_secs(3));
        assert_eq!(res, Some(90));
        assert_eq!(afr.current_fps(), 90);
    }

    #[test]
    fn adaptive_fps_steps_down_on_overrun_even_if_delivered_ok() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 120, start);
        // Delivered looks fine, but most frames blew the budget — still drop.
        let res = afr.apply_at(&load(118.0, 12.0, 0.40), start + Duration::from_secs(3));
        assert_eq!(res, Some(90));
    }

    #[test]
    fn adaptive_fps_converges_to_sustainable_and_holds() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 120, start);
        // Box tops out at ~66 fps; high encode time denies any up-probe.
        let cap = load(66.0, 14.0, 0.0);
        let mut t = start;
        for _ in 0..10 {
            t += Duration::from_secs(3);
            afr.apply_at(&cap, t);
        }
        // 120 -> 90 -> 60 (66 >= 0.92·60, so 60 sticks); never below.
        assert_eq!(afr.current_fps(), 60);
        // Further windows neither drop below 60 nor climb (no headroom).
        for _ in 0..5 {
            t += Duration::from_secs(3);
            assert_eq!(afr.apply_at(&cap, t), None);
        }
        assert_eq!(afr.current_fps(), 60);
    }

    #[test]
    fn adaptive_fps_probes_up_after_long_clean() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 120, start);
        let mut t = start;
        // Drive down to 60.
        t += Duration::from_secs(3);
        afr.apply_at(&load(66.0, 14.0, 0.0), t);
        t += Duration::from_secs(3);
        afr.apply_at(&load(66.0, 14.0, 0.0), t);
        assert_eq!(afr.current_fps(), 60);
        // Sustained clean with real headroom → cautiously probe up one step.
        let good = load(60.0, 5.0, 0.0);
        let mut probed = None;
        for _ in 0..30 {
            t += Duration::from_secs(2);
            if let Some(f) = afr.apply_at(&good, t) {
                probed = Some(f);
                break;
            }
        }
        assert_eq!(probed, Some(90));
    }

    #[test]
    fn adaptive_fps_backs_off_after_failed_upprobe() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 120, start);
        let mut t = start;
        // Down to 60, then probe up to 90.
        t += Duration::from_secs(3);
        afr.apply_at(&load(66.0, 14.0, 0.0), t);
        t += Duration::from_secs(3);
        afr.apply_at(&load(66.0, 14.0, 0.0), t);
        let good = load(60.0, 5.0, 0.0);
        loop {
            t += Duration::from_secs(2);
            if afr.apply_at(&good, t) == Some(90) {
                break;
            }
        }
        // The up-probe immediately fails (90 unsustainable) → back to 60.
        t += Duration::from_secs(2);
        assert_eq!(afr.apply_at(&load(66.0, 14.0, 0.0), t), Some(60));
        // During the exponential backoff it must NOT re-probe up, even clean.
        let mut upped = None;
        for _ in 0..3 {
            t += Duration::from_secs(2);
            if let Some(f) = afr.apply_at(&good, t) {
                upped = Some(f);
            }
        }
        assert_eq!(upped, None, "must hold off during up-probe backoff");
        assert_eq!(afr.current_fps(), 60);
        // Well past the backoff it may probe again.
        for _ in 0..40 {
            t += Duration::from_secs(2);
            if let Some(f) = afr.apply_at(&good, t) {
                upped = Some(f);
                break;
            }
        }
        assert_eq!(upped, Some(90));
    }

    #[test]
    fn adaptive_fps_ladder_respects_ceiling_and_floor() {
        let start = Instant::now();
        let mut afr = AdaptiveFrameRate::with_enabled(true, 60, start);
        let mut t = start;
        // Even with awful load it never exceeds the 60 ceiling and never drops
        // below the 30 floor: 60 -> 48 -> 30 and stop.
        for _ in 0..10 {
            t += Duration::from_secs(3);
            afr.apply_at(&load(10.0, 40.0, 1.0), t);
            assert!(afr.current_fps() <= 60 && afr.current_fps() >= 30);
        }
        assert_eq!(afr.current_fps(), 30);
    }

    #[test]
    fn encode_rate_tracker_emits_after_window() {
        let start = Instant::now();
        let budget_us = 1_000_000 / 60;
        let mut tr = EncodeRateTracker::new(start);
        for _ in 0..60 {
            tr.record(8_000, budget_us); // 8ms, within budget
        }
        // Window not yet elapsed.
        assert!(tr.take_sample(start + Duration::from_millis(500)).is_none());
        let s = tr
            .take_sample(start + Duration::from_millis(1000))
            .expect("sample after full window");
        assert!((s.delivered_fps - 60.0).abs() < 1.0);
        assert!((s.avg_encode_ms - 8.0).abs() < 0.1);
        assert_eq!(s.overrun_ratio, 0.0);
        // Reset after emit: too few frames now.
        assert!(tr.take_sample(start + Duration::from_secs(3)).is_none());
    }

    #[test]
    fn encode_rate_tracker_counts_overruns() {
        let start = Instant::now();
        let budget_us = 1_000_000 / 60;
        let mut tr = EncodeRateTracker::new(start);
        for _ in 0..30 {
            tr.record(25_000, budget_us); // 25ms > 16.6ms budget → overrun
        }
        let s = tr.take_sample(start + Duration::from_millis(1100)).unwrap();
        assert_eq!(s.overrun_ratio, 1.0);
    }
}
