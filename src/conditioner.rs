//! Deterministic, bounded packet impairment at the transport boundary.
//!
//! One handle controls one client transport. Clones are for UI/control access, not
//! sharing between clients. Configuration changes and session resets discard queued
//! packets. A requested outage starts on the next transport poll, even when normal
//! conditioning is disabled. Keep polling the transport during an outage.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Impairment applied independently to incoming and outgoing packets.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionerConfig {
    pub enabled: bool,
    /// Added one-way delay; total added RTT is approximately twice this value.
    pub latency: Duration,
    /// Uniform jitter in `[-jitter, jitter]`, clamped at zero delay.
    pub jitter: Duration,
    /// Independent packet loss probability in the inclusive range 0..=1.
    pub packet_loss: f32,
    pub max_queue_packets: usize,
    pub max_queue_bytes: usize,
    pub seed: u64,
}

impl Default for ConditionerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            packet_loss: 0.0,
            max_queue_packets: 1024,
            max_queue_bytes: 2 * 1024 * 1024,
            seed: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionerConfigError;
impl fmt::Display for ConditionerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("packet loss must be finite and between 0 and 1; queue limits must be nonzero")
    }
}
impl std::error::Error for ConditionerConfigError {}

impl ConditionerConfig {
    pub fn validate(&self) -> Result<(), ConditionerConfigError> {
        if !self.packet_loss.is_finite()
            || !(0.0..=1.0).contains(&self.packet_loss)
            || self.max_queue_packets == 0
            || self.max_queue_bytes == 0
        {
            Err(ConditionerConfigError)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionerDirection {
    Incoming,
    Outgoing,
}
impl ConditionerDirection {
    fn index(self) -> usize {
        match self {
            Self::Incoming => 0,
            Self::Outgoing => 1,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirectionStats {
    pub queued_packets: usize,
    pub queued_bytes: usize,
    pub simulated_loss_drops: u64,
    pub outage_drops: u64,
    pub overflow_drops: u64,
    pub transition_drops: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ConditionerStats {
    pub incoming: DirectionStats,
    pub outgoing: DirectionStats,
    pub outage_active: bool,
}
impl ConditionerStats {
    fn direction(&mut self, direction: ConditionerDirection) -> &mut DirectionStats {
        match direction {
            ConditionerDirection::Incoming => &mut self.incoming,
            ConditionerDirection::Outgoing => &mut self.outgoing,
        }
    }
    fn discard_queues(&mut self) {
        for stats in [&mut self.incoming, &mut self.outgoing] {
            stats.transition_drops = stats
                .transition_drops
                .saturating_add(stats.queued_packets as u64);
            stats.queued_packets = 0;
            stats.queued_bytes = 0;
        }
    }
}

#[derive(Debug)]
struct Control {
    config: ConditionerConfig,
    stats: ConditionerStats,
    revision: u64,
    pending_outage: Option<Duration>,
}

/// Thread-safe controls and statistics, independent of any engine or UI.
#[derive(Debug, Clone)]
pub struct ConditionerHandle(Arc<Mutex<Control>>);
impl Default for ConditionerHandle {
    fn default() -> Self {
        Self::new(ConditionerConfig::default()).expect("valid defaults")
    }
}
impl ConditionerHandle {
    pub fn new(config: ConditionerConfig) -> Result<Self, ConditionerConfigError> {
        config.validate()?;
        Ok(Self(Arc::new(Mutex::new(Control {
            config,
            stats: ConditionerStats::default(),
            revision: 0,
            pending_outage: None,
        }))))
    }
    /// Discard queued packets and cancel an outage. Invalid changes are atomic no-ops.
    pub fn configure(&self, config: ConditionerConfig) -> Result<(), ConditionerConfigError> {
        config.validate()?;
        let mut control = self.0.lock().unwrap();
        control.stats.discard_queues();
        control.stats.outage_active = false;
        control.pending_outage = None;
        control.config = config;
        control.revision = control.revision.wrapping_add(1);
        Ok(())
    }
    pub fn config(&self) -> ConditionerConfig {
        self.0.lock().unwrap().config.clone()
    }
    pub fn stats(&self) -> ConditionerStats {
        self.0.lock().unwrap().stats
    }
    pub fn is_active(&self) -> bool {
        let control = self.0.lock().unwrap();
        control.config.enabled || control.stats.outage_active
    }
    /// Discard pending packets and drop both directions for this duration, starting
    /// at the next transport poll. A zero duration cancels the current outage.
    pub fn outage(&self, duration: Duration) {
        let mut control = self.0.lock().unwrap();
        control.stats.discard_queues();
        control.pending_outage = (!duration.is_zero()).then_some(duration);
        control.stats.outage_active = !duration.is_zero();
        control.revision = control.revision.wrapping_add(1);
    }
    /// Start a new session with the same configuration, no queued packets or outage,
    /// and fresh statistics. Existing engines notice this before processing packets.
    pub fn reset_session(&self) {
        let mut control = self.0.lock().unwrap();
        control.stats = ConditionerStats::default();
        control.pending_outage = None;
        control.revision = control.revision.wrapping_add(1);
    }
}

#[derive(Debug)]
struct Scheduled<T> {
    due: Duration,
    payload: T,
    bytes: usize,
}

#[derive(Debug)]
pub(crate) struct PacketConditioner<T> {
    handle: ConditionerHandle,
    queues: [VecDeque<Scheduled<T>>; 2],
    revision: u64,
    rng: u64,
    last_now: Duration,
    outage_until: Option<Duration>,
}
impl<T> PacketConditioner<T> {
    pub(crate) fn new(handle: ConditionerHandle) -> Self {
        handle.reset_session();
        Self {
            handle,
            queues: [VecDeque::new(), VecDeque::new()],
            revision: u64::MAX,
            rng: 1,
            last_now: Duration::ZERO,
            outage_until: None,
        }
    }
    pub(crate) fn handle(&self) -> ConditionerHandle {
        self.handle.clone()
    }
    fn sync(&mut self, control: &mut Control, now: Duration) -> Duration {
        let now = now.max(self.last_now);
        self.last_now = now;
        if self.revision != control.revision {
            self.queues.iter_mut().for_each(VecDeque::clear);
            self.revision = control.revision;
            self.rng = control.config.seed;
            self.outage_until = control.pending_outage.take().map(|d| now.saturating_add(d));
        }
        if self.outage_until.is_some_and(|end| now >= end) {
            self.outage_until = None;
        }
        control.stats.outage_active = self.outage_until.is_some();
        now
    }
    // SplitMix64: reproducible, including a zero seed. This is not a security RNG.
    fn random(&mut self) -> f64 {
        self.rng = self.rng.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^= z >> 31;
        (z >> 11) as f64 / ((1u64 << 53) as f64)
    }
    /// Returns a packet only for immediate bypass. Otherwise it is queued or dropped.
    pub(crate) fn enqueue(
        &mut self,
        direction: ConditionerDirection,
        now: Duration,
        payload: T,
        byte_len: usize,
    ) -> Option<T> {
        let handle = self.handle.clone();
        let mut control = handle.0.lock().unwrap();
        let now = self.sync(&mut control, now);
        if self.outage_until.is_some() {
            let stats = control.stats.direction(direction);
            stats.outage_drops = stats.outage_drops.saturating_add(1);
            return None;
        }
        if !control.config.enabled {
            return Some(payload);
        }
        if self.random() < control.config.packet_loss as f64 {
            let stats = control.stats.direction(direction);
            stats.simulated_loss_drops = stats.simulated_loss_drops.saturating_add(1);
            return None;
        }
        let config = &control.config;
        let stats = match direction {
            ConditionerDirection::Incoming => control.stats.incoming,
            ConditionerDirection::Outgoing => control.stats.outgoing,
        };
        if stats.queued_packets >= config.max_queue_packets
            || byte_len > config.max_queue_bytes.saturating_sub(stats.queued_bytes)
        {
            let stats = control.stats.direction(direction);
            stats.overflow_drops = stats.overflow_drops.saturating_add(1);
            return None;
        }
        let delay_secs = (config.latency.as_secs_f64()
            + (self.random() * 2.0 - 1.0) * config.jitter.as_secs_f64())
        .max(0.0);
        let delay = Duration::try_from_secs_f64(delay_secs).unwrap_or(Duration::MAX);
        let due = now.saturating_add(delay);
        let queue = &mut self.queues[direction.index()];
        let position = queue
            .iter()
            .position(|packet| packet.due > due)
            .unwrap_or(queue.len());
        queue.insert(
            position,
            Scheduled {
                due,
                payload,
                bytes: byte_len,
            },
        );
        let stats = control.stats.direction(direction);
        stats.queued_packets += 1;
        stats.queued_bytes += byte_len;
        None
    }
    pub(crate) fn drain_ready(&mut self, direction: ConditionerDirection, now: Duration) -> Vec<T> {
        let handle = self.handle.clone();
        let mut control = handle.0.lock().unwrap();
        let now = self.sync(&mut control, now);
        let queue = &mut self.queues[direction.index()];
        let mut ready = Vec::new();
        let stats = control.stats.direction(direction);
        while queue.front().is_some_and(|packet| packet.due <= now) {
            let packet = queue.pop_front().unwrap();
            stats.queued_packets -= 1;
            stats.queued_bytes -= packet.bytes;
            ready.push(packet.payload);
        }
        ready
    }
    pub(crate) fn clear_session(&mut self) {
        self.handle.reset_session();
        self.queues.iter_mut().for_each(VecDeque::clear);
        self.last_now = Duration::ZERO;
        self.outage_until = None;
    }
}

/// Baseline RTT sampled only while impairment is inactive. Uses an exponential
/// moving average (1/8 new sample); call `reset` and observe with impairment off
/// to recalibrate. This measures transport RTT, not input acknowledgement age.
#[derive(Debug, Default, Clone)]
pub struct RttCalibration {
    baseline: Option<Duration>,
    was_active: bool,
    resume_after: Duration,
    last_now: Duration,
}
impl RttCalibration {
    pub fn observe(&mut self, rtt: Duration, conditioning_active: bool) {
        if conditioning_active || rtt.is_zero() {
            return;
        }
        self.baseline = Some(match self.baseline {
            None => rtt,
            Some(old) => old.mul_f64(0.875).saturating_add(rtt.mul_f64(0.125)),
        });
    }
    /// Time-aware sampling with a five-second settling period after impairment is
    /// disabled. This limits contamination by Renet's smoothed RTT; it cannot
    /// guarantee that its entire sample history has expired.
    pub fn observe_at(&mut self, now: Duration, rtt: Duration, conditioning_active: bool) {
        let now = now.max(self.last_now);
        self.last_now = now;
        if self.was_active && !conditioning_active {
            self.resume_after = now.saturating_add(Duration::from_secs(5));
        }
        self.was_active = conditioning_active;
        if now >= self.resume_after {
            self.observe(rtt, conditioning_active);
        }
    }
    /// Clears the baseline, preserving any post-impairment settling period.
    pub fn reset(&mut self) {
        self.baseline = None;
    }
    pub fn baseline(&self) -> Option<Duration> {
        self.baseline
    }
    /// None means no unconditioned baseline is available. Zero means the target
    /// is already at or below baseline; packet conditioning cannot reduce RTT.
    pub fn added_delay_for_target(&self, target: Duration) -> Option<Duration> {
        self.baseline
            .map(|baseline| target.saturating_sub(baseline) / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConditionerDirection::{Incoming, Outgoing};
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn active() -> ConditionerConfig {
        ConditionerConfig {
            enabled: true,
            latency: ms(100),
            ..Default::default()
        }
    }
    fn engine(config: ConditionerConfig) -> (ConditionerHandle, PacketConditioner<u32>) {
        let handle = ConditionerHandle::new(config).unwrap();
        let engine = PacketConditioner::new(handle.clone());
        (handle, engine)
    }
    #[test]
    fn disabled_bypasses_without_queueing() {
        let (handle, mut engine) = engine(ConditionerConfig::default());
        assert_eq!(engine.enqueue(Incoming, ms(0), 1, 9999999), Some(1));
        assert_eq!(engine.enqueue(Outgoing, ms(0), 2, 9999999), Some(2));
        assert_eq!(handle.stats(), ConditionerStats::default());
    }
    #[test]
    fn schedules_both_directions_and_preserves_equal_deadline_order() {
        let (handle, mut engine) = engine(active());
        for value in 0..3 {
            assert_eq!(engine.enqueue(Incoming, ms(10), value, 10), None);
        }
        engine.enqueue(Outgoing, ms(10), 99, 4);
        assert!(engine.drain_ready(Incoming, ms(109)).is_empty());
        assert_eq!(engine.drain_ready(Incoming, ms(110)), vec![0, 1, 2]);
        assert_eq!(handle.stats().outgoing.queued_packets, 1);
        assert_eq!(engine.drain_ready(Outgoing, ms(110)), vec![99]);
        assert_eq!(handle.stats(), ConditionerStats::default());
    }
    #[test]
    fn packet_and_byte_limits_are_per_direction() {
        let (handle, mut engine) = engine(ConditionerConfig {
            max_queue_packets: 2,
            max_queue_bytes: 10,
            ..active()
        });
        engine.enqueue(Incoming, ms(0), 1, 6);
        engine.enqueue(Incoming, ms(0), 2, 5); // Byte overflow.
        engine.enqueue(Incoming, ms(0), 3, 4);
        engine.enqueue(Incoming, ms(0), 4, 0); // Packet overflow.
        engine.enqueue(Outgoing, ms(0), 5, 10);
        engine.enqueue(Outgoing, ms(0), 6, usize::MAX);
        assert_eq!(handle.stats().incoming.overflow_drops, 2);
        assert_eq!(handle.stats().outgoing.overflow_drops, 1);
        assert_eq!(handle.stats().incoming.queued_bytes, 10);
        assert_eq!(engine.drain_ready(Incoming, ms(100)), vec![1, 3]);
        assert_eq!(engine.drain_ready(Outgoing, ms(100)), vec![5]);
    }
    #[test]
    fn total_loss_has_separate_counter_and_no_queue_usage() {
        let (handle, mut engine) = engine(ConditionerConfig {
            packet_loss: 1.0,
            ..active()
        });
        for value in 0..10 {
            engine.enqueue(Incoming, ms(0), value, 4);
        }
        assert_eq!(handle.stats().incoming.simulated_loss_drops, 10);
        assert_eq!(handle.stats().incoming.queued_packets, 0);
        assert_eq!(handle.stats().incoming.overflow_drops, 0);
        assert!(engine.drain_ready(Incoming, ms(1000)).is_empty());
    }
    #[test]
    fn seeded_jitter_is_repeatable_bounded_and_can_reorder() {
        let config = ConditionerConfig {
            jitter: ms(40),
            seed: 123,
            ..active()
        };
        let (_, mut a) = engine(config.clone());
        let (_, mut b) = engine(config);
        for value in 0..100 {
            a.enqueue(Incoming, ms(0), value, 4);
            b.enqueue(Incoming, ms(0), value, 4);
        }
        assert!(a.drain_ready(Incoming, ms(59)).is_empty());
        assert!(b.drain_ready(Incoming, ms(59)).is_empty());
        let mut order = vec![];
        for t in 60..=140 {
            let actual = a.drain_ready(Incoming, ms(t));
            assert_eq!(actual, b.drain_ready(Incoming, ms(t)));
            order.extend(actual);
        }
        assert_eq!(order.len(), 100);
        assert_ne!(order, (0..100).collect::<Vec<_>>());
    }
    #[test]
    fn negative_jitter_delay_is_clamped_to_zero() {
        let (_, mut engine) = engine(ConditionerConfig {
            latency: Duration::ZERO,
            jitter: ms(100),
            ..active()
        });
        for value in 0..100 {
            engine.enqueue(Incoming, ms(0), value, 4);
        }
        let immediate = engine.drain_ready(Incoming, ms(0));
        assert!(!immediate.is_empty());
        assert!(immediate.len() < 100);
        assert_eq!(
            immediate.len() + engine.drain_ready(Incoming, ms(100)).len(),
            100
        );
    }
    #[test]
    fn seeded_partial_loss_is_reproducible() {
        let config = ConditionerConfig {
            packet_loss: 0.25,
            seed: 0,
            ..active()
        };
        let (ha, mut a) = engine(config.clone());
        let (hb, mut b) = engine(config);
        for value in 0..1000 {
            a.enqueue(Incoming, ms(0), value, 4);
            b.enqueue(Incoming, ms(0), value, 4);
        }
        let losses = ha.stats().incoming.simulated_loss_drops;
        assert!((180..320).contains(&losses));
        assert_eq!(ha.stats(), hb.stats());
        assert_eq!(
            a.drain_ready(Incoming, ms(100)),
            b.drain_ready(Incoming, ms(100))
        );
    }
    #[test]
    fn invalid_configuration_is_atomic() {
        let (handle, mut engine) = engine(active());
        engine.enqueue(Incoming, ms(0), 1, 4);
        for packet_loss in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
            assert!(
                handle
                    .configure(ConditionerConfig {
                        packet_loss,
                        ..active()
                    })
                    .is_err()
            );
        }
        assert!(
            handle
                .configure(ConditionerConfig {
                    max_queue_packets: 0,
                    ..active()
                })
                .is_err()
        );
        assert!(
            handle
                .configure(ConditionerConfig {
                    max_queue_bytes: 0,
                    ..active()
                })
                .is_err()
        );
        assert_eq!(handle.config(), active());
        assert_eq!(engine.drain_ready(Incoming, ms(100)), vec![1]);
    }
    #[test]
    fn disable_drops_old_packets_and_immediately_bypasses_new_packets() {
        let (handle, mut engine) = engine(active());
        engine.enqueue(Incoming, ms(0), 1, 4);
        engine.enqueue(Outgoing, ms(0), 2, 4);
        handle.configure(ConditionerConfig::default()).unwrap();
        assert_eq!(handle.stats().incoming.transition_drops, 1);
        assert_eq!(handle.stats().outgoing.transition_drops, 1);
        assert_eq!(handle.stats().incoming.queued_bytes, 0);
        assert_eq!(engine.enqueue(Incoming, ms(20), 3, 4), Some(3));
        assert!(engine.drain_ready(Incoming, ms(1000)).is_empty());
        assert!(engine.drain_ready(Outgoing, ms(1000)).is_empty());
    }
    #[test]
    fn reconfiguration_flushes_and_applies_new_delay() {
        let (handle, mut engine) = engine(active());
        engine.enqueue(Incoming, ms(0), 1, 4);
        handle
            .configure(ConditionerConfig {
                latency: ms(10),
                ..active()
            })
            .unwrap();
        engine.enqueue(Incoming, ms(1), 2, 4);
        assert_eq!(engine.drain_ready(Incoming, ms(11)), vec![2]);
        assert!(engine.drain_ready(Incoming, ms(200)).is_empty());
        assert_eq!(handle.stats().incoming.transition_drops, 1);
    }
    #[test]
    fn outage_starts_on_poll_works_while_disabled_and_recovers() {
        let (handle, mut engine) = engine(ConditionerConfig::default());
        handle.outage(ms(100));
        assert!(handle.is_active());
        assert!(engine.drain_ready(Incoming, ms(500)).is_empty());
        assert_eq!(engine.enqueue(Incoming, ms(500), 1, 4), None);
        assert_eq!(engine.enqueue(Outgoing, ms(599), 2, 4), None);
        assert_eq!(engine.enqueue(Incoming, ms(600), 3, 4), Some(3));
        assert!(!handle.is_active());
        assert_eq!(handle.stats().incoming.outage_drops, 1);
        assert_eq!(handle.stats().outgoing.outage_drops, 1);
    }
    #[test]
    fn outage_flushes_pending_packets_and_zero_cancels() {
        let (handle, mut engine) = engine(active());
        engine.enqueue(Incoming, ms(0), 1, 4);
        handle.outage(ms(100));
        assert!(engine.drain_ready(Incoming, ms(1)).is_empty());
        assert_eq!(handle.stats().incoming.transition_drops, 1);
        handle.outage(Duration::ZERO);
        engine.enqueue(Incoming, ms(2), 2, 4);
        assert!(!handle.stats().outage_active);
        assert_eq!(engine.drain_ready(Incoming, ms(102)), vec![2]);
    }
    #[test]
    fn session_reset_prevents_stale_packets_and_resets_statistics() {
        let (handle, mut engine) = engine(active());
        engine.enqueue(Incoming, ms(0), 1, 4);
        handle.reset_session();
        assert!(engine.drain_ready(Incoming, ms(100)).is_empty());
        assert_eq!(handle.stats(), ConditionerStats::default());
        engine.enqueue(Incoming, ms(100), 2, 4);
        engine.clear_session();
        engine.enqueue(Incoming, ms(0), 3, 4);
        assert_eq!(engine.drain_ready(Incoming, ms(100)), vec![3]);
        assert_eq!(handle.stats(), ConditionerStats::default());
    }
    #[test]
    fn constructing_replacement_engine_clears_prior_handle_state() {
        let (handle, mut old) = engine(active());
        old.enqueue(Incoming, ms(0), 1, 4);
        let mut new = PacketConditioner::<u32>::new(handle.clone());
        assert_eq!(handle.stats(), ConditionerStats::default());
        assert!(new.drain_ready(Incoming, ms(1000)).is_empty());
        assert!(old.drain_ready(Incoming, ms(1000)).is_empty());
    }
    #[test]
    fn backward_clock_is_clamped_and_extreme_delays_do_not_panic() {
        let (handle, mut engine) = engine(active());
        engine.drain_ready(Incoming, ms(1000));
        engine.enqueue(Incoming, ms(0), 1, 4);
        assert!(engine.drain_ready(Incoming, ms(1099)).is_empty());
        assert_eq!(engine.drain_ready(Incoming, ms(1100)), vec![1]);
        handle
            .configure(ConditionerConfig {
                latency: Duration::MAX,
                jitter: Duration::MAX,
                ..active()
            })
            .unwrap();
        engine.enqueue(Incoming, ms(1200), 2, 4);
        assert_eq!(engine.drain_ready(Incoming, Duration::MAX), vec![2]);
    }
    #[test]
    fn rtt_baseline_freezes_and_target_never_adds_negative_delay() {
        let mut rtt = RttCalibration::default();
        assert_eq!(rtt.added_delay_for_target(ms(300)), None);
        rtt.observe(ms(40), false);
        rtt.observe(ms(300), true);
        assert_eq!(rtt.baseline(), Some(ms(40)));
        assert_eq!(rtt.added_delay_for_target(ms(300)), Some(ms(130)));
        assert_eq!(rtt.added_delay_for_target(ms(30)), Some(Duration::ZERO));
        rtt.reset();
        assert_eq!(rtt.baseline(), None);
    }
    #[test]
    fn rtt_settling_ignores_conditioned_filter_tail_even_after_reset() {
        let mut rtt = RttCalibration::default();
        rtt.observe_at(ms(0), ms(40), false);
        rtt.observe_at(ms(1), ms(300), true);
        rtt.observe_at(ms(2), ms(250), false);
        rtt.reset();
        rtt.observe_at(ms(5001), ms(100), false);
        assert_eq!(rtt.baseline(), None);
        rtt.observe_at(ms(5002), ms(40), false);
        assert_eq!(rtt.baseline(), Some(ms(40)));
    }
}
