//! Optional transport-boundary pacing. No packet queue is owned here.
use std::{collections::HashMap, hash::Hash, time::Duration};

/// Units charged by the ceiling. WebRTC framing/retransmissions are browser/SCTP owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressBasis {
    NativeUdpIp,
    EncryptedLogical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressConfig {
    pub(crate) basis: EgressBasis,
    pub(crate) bytes_per_second: u64,
    pub(crate) burst_bytes: u64,
    pub(crate) control_reserve_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EgressConfigError {
    #[error(
        "egress rate must be 1..=1000000000 bytes/sec; burst must be 2048..=67108864 bytes; reserve must be 256..=burst-1448"
    )]
    InvalidBudget,
    #[error("egress accounting basis does not match this transport")]
    BasisMismatch,
}
impl EgressConfig {
    pub fn new(
        basis: EgressBasis,
        bytes_per_second: u64,
        burst_bytes: u64,
        control_reserve_bytes: u64,
    ) -> Result<Self, EgressConfigError> {
        if !(1..=1_000_000_000).contains(&bytes_per_second)
            || !(2048..=67_108_864).contains(&burst_bytes)
            || control_reserve_bytes < 256
            || control_reserve_bytes > burst_bytes - 1448
        {
            return Err(EgressConfigError::InvalidBudget);
        }
        Ok(Self {
            basis,
            bytes_per_second,
            burst_bytes,
            control_reserve_bytes,
        })
    }
    pub fn basis(self) -> EgressBasis {
        self.basis
    }
    pub fn bytes_per_second(self) -> u64 {
        self.bytes_per_second
    }
    pub fn burst_bytes(self) -> u64 {
        self.burst_bytes
    }
    pub fn control_reserve_bytes(self) -> u64 {
        self.control_reserve_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressStats {
    pub basis: EgressBasis,
    pub config: Option<EgressConfig>,
    pub sent_packets: u64,
    pub encrypted_logical_bytes: u64,
    /// Only native UDP sends: encrypted bytes plus destination-family UDP/IP headers.
    pub native_udp_ip_bytes: u64,
    pub control_packets: u64,
    pub pacing_drops: u64,
    pub cap_drops: u64,
    pub backend_drops: u64,
    /// Always zero: pacing drops instead of retaining stale encrypted packets.
    pub deferred_packets: u64,
    /// Packets assigned to the shared overflow bucket once peer tracking is full.
    pub peer_capacity_packets: u64,
}
impl EgressStats {
    fn new(basis: EgressBasis) -> Self {
        Self {
            basis,
            config: None,
            sent_packets: 0,
            encrypted_logical_bytes: 0,
            native_udp_ip_bytes: 0,
            control_packets: 0,
            pacing_drops: 0,
            cap_drops: 0,
            backend_drops: 0,
            deferred_packets: 0,
            peer_capacity_packets: 0,
        }
    }
}
/// Current data credit in the transport's declared accounting units.
/// This read-only sample never refills, spends, or creates a peer bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressAllowance {
    pub basis: EgressBasis,
    pub config: Option<EgressConfig>,
    pub available_data_bytes: u64,
    pub maximum_data_bytes: u64,
    pub ip_udp_overhead: u64,
}
impl EgressAllowance {
    /// Deferred encrypted packets have already consumed their application queue
    /// slots, but still reserve credit until their final transport send attempt.
    pub(crate) fn reserve_queued(&mut self, logical_bytes: usize, packets: usize) {
        let committed = (logical_bytes as u64)
            .saturating_add((packets as u64).saturating_mul(self.ip_udp_overhead));
        self.available_data_bytes = self.available_data_bytes.saturating_sub(committed);
    }
}
#[derive(Debug)]
struct Bucket {
    credit: u128,
    at: Duration,
    stats: EgressStats,
}
impl Bucket {
    fn new(basis: EgressBasis, config: Option<EgressConfig>, at: Duration) -> Self {
        let mut stats = EgressStats::new(basis);
        stats.config = config;
        Self {
            credit: config.map_or(0, |c| c.burst_bytes as u128 * 1_000_000_000),
            at,
            stats,
        }
    }
    fn allowance(&self, now: Duration) -> EgressAllowance {
        let (available, maximum) = self.stats.config.map_or((u64::MAX, u64::MAX), |c| {
            let credit = self
                .credit
                .saturating_add(
                    now.saturating_sub(self.at)
                        .as_nanos()
                        .saturating_mul(c.bytes_per_second as u128),
                )
                .min(c.burst_bytes as u128 * 1_000_000_000)
                / 1_000_000_000;
            (
                (credit as u64).saturating_sub(c.control_reserve_bytes),
                c.burst_bytes - c.control_reserve_bytes,
            )
        });
        EgressAllowance {
            basis: self.stats.basis,
            config: self.stats.config,
            available_data_bytes: available,
            maximum_data_bytes: maximum,
            ip_udp_overhead: 0,
        }
    }
    fn admit(&mut self, bytes: u64, control: bool, now: Duration) -> bool {
        let Some(c) = self.stats.config else {
            return true;
        };
        let elapsed = now.saturating_sub(self.at);
        self.at = self.at.max(now);
        let cap = c.burst_bytes as u128 * 1_000_000_000;
        self.credit = self
            .credit
            .saturating_add(
                elapsed
                    .as_nanos()
                    .saturating_mul(c.bytes_per_second as u128),
            )
            .min(cap);
        let reserve = if control { 0 } else { c.control_reserve_bytes };
        if bytes > c.burst_bytes - reserve {
            self.stats.cap_drops = self.stats.cap_drops.saturating_add(1);
            return false;
        }
        if self.credit < (bytes + reserve) as u128 * 1_000_000_000 {
            self.stats.pacing_drops = self.stats.pacing_drops.saturating_add(1);
            return false;
        }
        self.credit -= bytes as u128 * 1_000_000_000;
        true
    }
}

/// A bounded set; entries retain credit through reconnects. Overflow shares one
/// bucket, so untrusted source churn cannot allocate memory or mint fresh credit.
#[derive(Debug)]
pub(crate) struct Egress<K> {
    basis: EgressBasis,
    config: Option<EgressConfig>,
    buckets: HashMap<K, Bucket>,
    overflow: Bucket,
    total: EgressStats,
    #[cfg(not(target_arch = "wasm32"))]
    epoch: std::time::Instant,
    #[cfg(target_arch = "wasm32")]
    epoch: f64,
}
const MAX_PEERS: usize = 4096;
impl<K: Eq + Hash + Copy> Egress<K> {
    pub fn new(basis: EgressBasis) -> Self {
        Self {
            basis,
            config: None,
            buckets: HashMap::new(),
            overflow: Bucket::new(basis, None, Duration::ZERO),
            total: EgressStats::new(basis),
            #[cfg(not(target_arch = "wasm32"))]
            epoch: std::time::Instant::now(),
            #[cfg(target_arch = "wasm32")]
            epoch: browser_now(),
        }
    }
    fn now(&self) -> Duration {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.epoch.elapsed()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Duration::from_secs_f64(((browser_now() - self.epoch) / 1000.0).max(0.0))
        }
    }
    pub fn configure(&mut self, config: Option<EgressConfig>) -> Result<(), EgressConfigError> {
        if config.is_some_and(|c| c.basis != self.basis) {
            return Err(EgressConfigError::BasisMismatch);
        }
        let now = self.now();
        for b in self
            .buckets
            .values_mut()
            .chain(std::iter::once(&mut self.overflow))
        {
            // Reconfiguration never restores spent credit. New policies after
            // unpaced operation begin empty; first construction has a burst.
            b.credit = b
                .credit
                .min(config.map_or(0, |c| c.burst_bytes as u128 * 1_000_000_000));
            b.at = now;
            b.stats.config = config;
        }
        self.config = config;
        self.total.config = config;
        Ok(())
    }
    pub fn stats(&self) -> EgressStats {
        self.total
    }
    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub fn peer_stats(&self, key: K) -> Option<EgressStats> {
        self.buckets.get(&key).map(|b| b.stats)
    }
    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub fn peer_allowance(&self, key: K) -> EgressAllowance {
        let now = self.now();
        if let Some(bucket) = self.buckets.get(&key) {
            bucket.allowance(now)
        } else if self.buckets.len() >= MAX_PEERS {
            self.overflow.allowance(now)
        } else {
            Bucket::new(self.basis, self.config, now).allowance(now)
        }
    }
    fn bucket(&mut self, key: K, now: Duration) -> &mut Bucket {
        if !self.buckets.contains_key(&key) && self.buckets.len() < MAX_PEERS {
            self.buckets
                .insert(key, Bucket::new(self.basis, self.config, now));
        }
        if let Some(b) = self.buckets.get_mut(&key) {
            b
        } else {
            &mut self.overflow
        }
    }
    /// Called only at the final unconditioned backend send boundary.
    pub fn admit(&mut self, key: K, payload: &[u8], ip_overhead: u64) -> bool {
        self.admit_at(key, payload, ip_overhead, self.now())
    }
    fn admit_at(&mut self, key: K, payload: &[u8], ip_overhead: u64, now: Duration) -> bool {
        if self.buckets.len() == MAX_PEERS && !self.buckets.contains_key(&key) {
            self.total.peer_capacity_packets = self.total.peer_capacity_packets.saturating_add(1);
        }
        let control = is_control(payload);
        let charge = payload.len() as u64 + ip_overhead;
        let b = self.bucket(key, now);
        let before = b.stats;
        let accepted = b.admit(charge, control, now);
        let pacing = b.stats.pacing_drops - before.pacing_drops;
        let cap = b.stats.cap_drops - before.cap_drops;
        self.total.pacing_drops = self.total.pacing_drops.saturating_add(pacing);
        self.total.cap_drops = self.total.cap_drops.saturating_add(cap);
        accepted
    }
    pub fn complete(&mut self, key: K, payload: &[u8], overhead: u64, sent: bool) {
        let now = self.now();
        let b = self.bucket(key, now);
        record(&mut b.stats, payload, overhead, sent);
        record(&mut self.total, payload, overhead, sent);
    }
}
fn record(stats: &mut EgressStats, payload: &[u8], overhead: u64, sent: bool) {
    if sent {
        stats.sent_packets = stats.sent_packets.saturating_add(1);
        stats.encrypted_logical_bytes = stats
            .encrypted_logical_bytes
            .saturating_add(payload.len() as u64);
        if stats.basis == EgressBasis::NativeUdpIp {
            stats.native_udp_ip_bytes = stats
                .native_udp_ip_bytes
                .saturating_add(payload.len() as u64 + overhead);
        }
        if is_control(payload) {
            stats.control_packets = stats.control_packets.saturating_add(1);
        }
    } else {
        stats.backend_drops = stats.backend_drops.saturating_add(1);
    }
}
fn is_control(payload: &[u8]) -> bool {
    payload.first().is_some_and(|b| b & 0x0f != 5)
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn ip_overhead(addr: std::net::SocketAddr) -> u64 {
    if addr.ip().to_canonical().is_ipv4() {
        28
    } else {
        48
    }
}
#[cfg(target_arch = "wasm32")]
fn browser_now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> EgressConfig {
        EgressConfig::new(EgressBasis::NativeUdpIp, 2000, 4096, 512).unwrap()
    }
    #[test]
    fn allowance_matches_admission_without_minting_credit_or_tracking_entries() {
        let config = policy();
        let mut bucket = Bucket::new(EgressBasis::NativeUdpIp, Some(config), Duration::ZERO);
        assert!(bucket.admit(3000, false, Duration::ZERO));
        let at = Duration::from_millis(100);
        let preview = bucket.allowance(at);
        assert_eq!(preview.available_data_bytes, 784);
        assert_eq!(preview.maximum_data_bytes, 3584);
        assert_eq!(bucket.credit, 1096 * 1_000_000_000);
        assert!(bucket.admit(preview.available_data_bytes, false, at));
        assert!(!bucket.admit(1, false, at));
        let mut e = Egress::<usize>::new(EgressBasis::NativeUdpIp);
        e.configure(Some(config)).unwrap();
        for key in 0..MAX_PEERS + 10 {
            assert_eq!(e.peer_allowance(key).available_data_bytes, 3584);
        }
        assert!(e.buckets.is_empty());
        // A readonly overflow lookup uses the already spent shared bucket.
        for key in 0..MAX_PEERS {
            e.bucket(key, Duration::ZERO);
        }
        e.overflow.admit(3000, false, Duration::ZERO);
        assert!(e.peer_allowance(MAX_PEERS).available_data_bytes < 3584);
        assert_eq!(e.buckets.len(), MAX_PEERS);
    }
    #[test]
    fn wall_credit_reserve_caps_and_reconfiguration_are_bounded() {
        let mut e = Egress::<u64>::new(EgressBasis::NativeUdpIp);
        e.configure(Some(policy())).unwrap();
        let data = vec![5; 1000];
        let at = Duration::from_secs(1);
        for _ in 0..3 {
            assert!(e.admit_at(1, &data, 48, at));
        }
        assert!(!e.admit_at(1, &data, 48, at));
        // Control uses the reserved remainder, while still obeying the total cap.
        assert!(e.admit_at(1, &vec![4; 800], 48, at));
        assert!(!e.admit_at(1, &[4; 100], 48, at));
        assert!(!e.admit_at(1, &data, 48, Duration::ZERO));
        assert!(!e.admit_at(1, &vec![5; 4096], 48, at));
        assert!(e.admit_at(1, &data, 48, at + Duration::from_secs(1)));
        assert_eq!(e.stats().cap_drops, 1);
        assert_eq!(e.stats().pacing_drops, 3);
        let before = e.stats();
        let wrong = EgressConfig::new(EgressBasis::EncryptedLogical, 2000, 4096, 512).unwrap();
        assert_eq!(
            e.configure(Some(wrong)),
            Err(EgressConfigError::BasisMismatch)
        );
        assert_eq!(e.stats(), before);
        assert!(EgressConfig::new(EgressBasis::NativeUdpIp, 0, 0, 0).is_err());
        // Long inactivity cannot create more than one burst.
        let later = Duration::MAX;
        for _ in 0..3 {
            assert!(e.admit_at(1, &data, 48, later));
        }
        assert!(!e.admit_at(1, &data, 48, later));
    }
    #[test]
    fn peer_churn_has_a_fixed_memory_bound_and_shared_overflow_credit() {
        let mut e = Egress::<usize>::new(EgressBasis::NativeUdpIp);
        e.configure(Some(policy())).unwrap();
        for key in 0..MAX_PEERS {
            assert!(e.admit_at(key, &[5; 1000], 28, Duration::from_secs(1)));
        }
        let mut allowed = 0;
        for key in MAX_PEERS..MAX_PEERS + 1000 {
            allowed += usize::from(e.admit_at(key, &[5; 1000], 28, Duration::from_secs(10)));
        }
        assert_eq!(allowed, 3);
        assert_eq!(e.buckets.len(), MAX_PEERS);
        assert_eq!(e.stats().peer_capacity_packets, 1000);
        assert_eq!(e.stats().deferred_packets, 0);
    }
    #[test]
    fn native_header_units_and_web_logical_counters_are_distinct() {
        let mut e = Egress::<()>::new(EgressBasis::EncryptedLogical);
        e.complete((), &[5; 100], 0, true);
        e.complete((), &[4; 10], 0, false);
        assert_eq!(e.stats().encrypted_logical_bytes, 100);
        assert_eq!(e.stats().native_udp_ip_bytes, 0);
        assert_eq!(e.stats().backend_drops, 1);
        assert_eq!(e.peer_stats(()).unwrap().sent_packets, 1);
        #[cfg(not(target_arch = "wasm32"))]
        {
            assert_eq!(ip_overhead("127.0.0.1:1".parse().unwrap()), 28);
            assert_eq!(ip_overhead("[::1]:1".parse().unwrap()), 48);
        }
    }
}
