//! Optional server-side raw packet impairment with independent bounded peer queues.
//!
//! A handle belongs to one server; its clones can control both UDP and WebRTC
//! pumps in a mixed server. Unknown UDP handshake sources count toward `max_peers`.
//! Excess peers are dropped while impairment is enabled, never silently bypassed.
//! Reconfiguration flushes pending packets; removing a peer discards its session.

use crate::conditioner::{
    ConditionerConfig, ConditionerDirection, ConditionerHandle, ConditionerStats, DirectionStats,
    PacketConditioner,
};
pub use crate::packet_io::ServerPeerId;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConditionerConfig {
    pub packets: ConditionerConfig,
    /// Global bound across both transports, including untrusted handshake sources.
    pub max_peers: usize,
    /// Inactive entries are reclaimed during transport polling. Pending packets
    /// are discarded if their peer expires before their delivery deadline.
    pub idle_timeout: Duration,
}
impl Default for ServerConditionerConfig {
    fn default() -> Self {
        Self {
            packets: ConditionerConfig::default(),
            max_peers: 1024,
            idle_timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConditionerConfigError;
impl fmt::Display for ServerConditionerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid packet conditioner config, zero peer limit, or zero idle timeout")
    }
}
impl std::error::Error for ServerConditionerConfigError {}
impl ServerConditionerConfig {
    pub fn validate(&self) -> Result<(), ServerConditionerConfigError> {
        if self.packets.validate().is_err() || self.max_peers == 0 || self.idle_timeout.is_zero() {
            Err(ServerConditionerConfigError)
        } else {
            Ok(())
        }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ServerConditionerStats {
    /// Current queue sizes and cumulative packet drop counters, including removed peers.
    pub packets: ConditionerStats,
    pub peers: usize,
    /// Packets dropped because no peer entry could be allocated.
    pub peer_limit_drops: u64,
    pub expired_peers: u64,
}
#[derive(Debug)]
struct Peer {
    owner: u64,
    last_seen: Duration,
    engine: PacketConditioner<Vec<u8>>,
}
#[derive(Debug)]
struct State {
    config: ServerConditionerConfig,
    peers: BTreeMap<ServerPeerId, Peer>,
    retired: ConditionerStats,
    peer_limit_drops: u64,
    expired_peers: u64,
    next_owner: u64,
    now: Duration,
    next_cleanup: Duration,
    outage_until: Option<Duration>,
}
#[derive(Debug, Clone)]
pub struct ServerConditionerHandle {
    state: Arc<Mutex<State>>,
    epoch: Instant,
}
impl Default for ServerConditionerHandle {
    fn default() -> Self {
        Self::new(ServerConditionerConfig::default()).expect("valid defaults")
    }
}
impl ServerConditionerHandle {
    pub fn new(config: ServerConditionerConfig) -> Result<Self, ServerConditionerConfigError> {
        config.validate()?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                config,
                peers: BTreeMap::new(),
                retired: ConditionerStats::default(),
                peer_limit_drops: 0,
                expired_peers: 0,
                next_owner: 1,
                now: Duration::ZERO,
                next_cleanup: Duration::ZERO,
                outage_until: None,
            })),
            epoch: Instant::now(),
        })
    }
    pub fn config(&self) -> ServerConditionerConfig {
        self.state.lock().unwrap().config.clone()
    }
    /// Atomically validate settings, then discard every pending packet. Reducing
    /// the peer limit evicts the least recently active peers. Counters are retained.
    pub fn configure(
        &self,
        config: ServerConditionerConfig,
    ) -> Result<(), ServerConditionerConfigError> {
        config.validate()?;
        let mut state = self.state.lock().unwrap();
        for (id, peer) in &state.peers {
            peer.engine
                .handle()
                .configure(peer_config(&config.packets, *id))
                .expect("validated config");
        }
        state.config = config;
        state.outage_until = None;
        state.next_cleanup = Duration::ZERO;
        while state.peers.len() > state.config.max_peers {
            let id = *state
                .peers
                .iter()
                .min_by_key(|(_, peer)| peer.last_seen)
                .unwrap()
                .0;
            state.retire(id);
        }
        Ok(())
    }
    pub fn stats(&self) -> ServerConditionerStats {
        let state = self.state.lock().unwrap();
        let mut packets = state.retired;
        for peer in state.peers.values() {
            add_stats(&mut packets, peer.engine.handle().stats());
        }
        packets.outage_active = state.outage_until.is_some_and(|end| state.now < end);
        ServerConditionerStats {
            packets,
            peers: state.peers.len(),
            peer_limit_drops: state.peer_limit_drops,
            expired_peers: state.expired_peers,
        }
    }
    pub fn per_peer_stats(&self) -> BTreeMap<ServerPeerId, ConditionerStats> {
        self.state
            .lock()
            .unwrap()
            .peers
            .iter()
            .map(|(id, peer)| (*id, peer.engine.handle().stats()))
            .collect()
    }
    /// Drop both directions for every peer, including peers arriving during the
    /// outage. Starts at the request's monotonic wall time; polling continues.
    pub fn outage(&self, duration: Duration) {
        self.outage_at(duration, self.elapsed());
    }
    fn outage_at(&self, duration: Duration, now: Duration) {
        let mut state = self.state.lock().unwrap();
        let now = state.poll(now);
        for peer in state.peers.values() {
            let handle = peer.engine.handle();
            handle.configure(handle.config()).expect("validated config");
        }
        state.outage_until = (!duration.is_zero()).then(|| now.saturating_add(duration));
    }
    pub(crate) fn elapsed(&self) -> Duration {
        self.epoch.elapsed()
    }
    pub(crate) fn allocate_owner(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        let owner = state.next_owner;
        state.next_owner = owner
            .checked_add(1)
            .expect("server conditioner owner IDs exhausted");
        owner
    }
    pub(crate) fn remove_owner(&self, owner: u64) {
        let mut state = self.state.lock().unwrap();
        let ids: Vec<_> = state
            .peers
            .iter()
            .filter(|(_, peer)| peer.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            state.retire(id);
        }
    }
    pub(crate) fn remove_peer(&self, owner: u64, id: ServerPeerId) {
        let mut state = self.state.lock().unwrap();
        if state.peers.get(&id).is_some_and(|peer| peer.owner == owner) {
            state.retire(id);
        }
    }
    pub(crate) fn queued_outgoing(&self, owner: u64, id: ServerPeerId) -> DirectionStats {
        self.state
            .lock()
            .unwrap()
            .peers
            .get(&id)
            .filter(|peer| peer.owner == owner)
            .map(|peer| peer.engine.handle().stats().outgoing)
            .unwrap_or_default()
    }
    pub(crate) fn defer_at(
        &self,
        owner: u64,
        direction: ConditionerDirection,
        id: ServerPeerId,
        bytes: &[u8],
        now: Duration,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = state.poll(now);
        // Reconfiguration already invalidated all prior queues. Disabled means
        // zero allocation for new source addresses, including handshake floods.
        let outage_remaining = state.outage_until.map(|end| end.saturating_sub(now));
        if !state.config.packets.enabled && outage_remaining.is_none() {
            return false;
        }
        if !state.peers.contains_key(&id) {
            if state.peers.len() >= state.config.max_peers {
                state.peer_limit_drops = state.peer_limit_drops.saturating_add(1);
                return true;
            }
            let handle = ConditionerHandle::new(peer_config(&state.config.packets, id))
                .expect("validated config");
            state.peers.insert(
                id,
                Peer {
                    owner,
                    last_seen: now,
                    engine: PacketConditioner::new(handle),
                },
            );
        }
        let peer = state.peers.get_mut(&id).unwrap();
        if peer.owner != owner {
            state.peer_limit_drops = state.peer_limit_drops.saturating_add(1);
            return true;
        }
        peer.last_seen = now;
        if let Some(remaining) = outage_remaining {
            peer.engine.handle().outage(remaining);
        }
        peer.engine
            .enqueue(direction, now, bytes.to_vec(), bytes.len())
            .is_none()
    }
    pub(crate) fn drain_at(
        &self,
        owner: u64,
        direction: ConditionerDirection,
        now: Duration,
    ) -> Vec<(ServerPeerId, Vec<u8>)> {
        let mut state = self.state.lock().unwrap();
        let now = state.poll(now);
        let outage_remaining = state.outage_until.map(|end| end.saturating_sub(now));
        let mut result = Vec::new();
        for (id, peer) in &mut state.peers {
            if peer.owner == owner {
                if let Some(remaining) = outage_remaining {
                    peer.engine.handle().outage(remaining);
                }
                result.extend(
                    peer.engine
                        .drain_ready(direction, now)
                        .into_iter()
                        .map(|bytes| (*id, bytes)),
                );
            }
        }
        result
    }
}
impl State {
    fn poll(&mut self, now: Duration) -> Duration {
        self.now = now.max(self.now);
        if self.outage_until.is_some_and(|end| self.now >= end) {
            self.outage_until = None;
        }
        if self.now >= self.next_cleanup {
            let ids: Vec<_> = self
                .peers
                .iter()
                .filter(|(_, peer)| {
                    self.now.saturating_sub(peer.last_seen) >= self.config.idle_timeout
                })
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                self.retire(id);
                self.expired_peers = self.expired_peers.saturating_add(1);
            }
            self.next_cleanup = self
                .now
                .saturating_add(self.config.idle_timeout.min(Duration::from_secs(1)));
        }
        self.now
    }
    fn retire(&mut self, id: ServerPeerId) {
        if let Some(peer) = self.peers.remove(&id) {
            let handle = peer.engine.handle();
            handle.configure(handle.config()).expect("validated config");
            add_stats(&mut self.retired, handle.stats());
        }
    }
}
fn peer_config(config: &ConditionerConfig, id: ServerPeerId) -> ConditionerConfig {
    // Stable FNV-1a mixing avoids identical impairment patterns on every peer.
    let mut hash = 0xcbf29ce484222325_u64;
    let mut mix = |byte: u8| {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    };
    match id {
        ServerPeerId::Udp(addr) => {
            mix(0);
            match addr.ip() {
                std::net::IpAddr::V4(ip) => {
                    mix(4);
                    for b in ip.octets() {
                        mix(b);
                    }
                }
                std::net::IpAddr::V6(ip) => {
                    mix(6);
                    for b in ip.octets() {
                        mix(b);
                    }
                }
            }
            for b in addr.port().to_be_bytes() {
                mix(b);
            }
        }
        ServerPeerId::WebRtc(id) => {
            mix(1);
            for b in id.to_be_bytes() {
                mix(b);
            }
        }
    }
    ConditionerConfig {
        seed: config.seed ^ hash,
        ..config.clone()
    }
}
fn add_direction(into: &mut DirectionStats, from: DirectionStats) {
    into.queued_packets = into.queued_packets.saturating_add(from.queued_packets);
    into.queued_bytes = into.queued_bytes.saturating_add(from.queued_bytes);
    into.simulated_loss_drops = into
        .simulated_loss_drops
        .saturating_add(from.simulated_loss_drops);
    into.outage_drops = into.outage_drops.saturating_add(from.outage_drops);
    into.overflow_drops = into.overflow_drops.saturating_add(from.overflow_drops);
    into.transition_drops = into.transition_drops.saturating_add(from.transition_drops);
}
fn add_stats(into: &mut ConditionerStats, from: ConditionerStats) {
    add_direction(&mut into.incoming, from.incoming);
    add_direction(&mut into.outgoing, from.outgoing);
    into.outage_active |= from.outage_active;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConditionerDirection::{Incoming, Outgoing};
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn config() -> ServerConditionerConfig {
        ServerConditionerConfig {
            packets: ConditionerConfig {
                enabled: true,
                latency: ms(100),
                max_queue_packets: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn rtc(id: u64) -> ServerPeerId {
        ServerPeerId::WebRtc(id)
    }
    fn udp(id: u16) -> ServerPeerId {
        ServerPeerId::Udp(([127, 0, 0, 1], id).into())
    }
    #[test]
    fn delayed_outgoing_packets_reserve_credit_until_release_without_cross_peer_charging() {
        let mut config = config();
        config.packets.max_queue_packets = 8;
        let handle = ServerConditionerHandle::new(config).unwrap();
        let owner = handle.allocate_owner();
        for (time, bytes) in [(0, 1000), (20, 1000), (40, 500)] {
            assert!(handle.defer_at(owner, Outgoing, udp(1), &vec![5; bytes], ms(time)));
        }
        assert!(handle.defer_at(owner, Incoming, udp(1), &[5; 600], ms(40)));
        assert!(handle.defer_at(owner, Outgoing, udp(2), &[5; 700], ms(40)));
        let queue = handle.queued_outgoing(owner, udp(1));
        assert_eq!((queue.queued_bytes, queue.queued_packets), (2500, 3));
        assert_eq!(handle.queued_outgoing(owner + 1, udp(1)).queued_bytes, 0);
        for overhead in [0, 28, 48] {
            let mut allowance = crate::EgressAllowance {
                basis: if overhead == 0 {
                    crate::EgressBasis::EncryptedLogical
                } else {
                    crate::EgressBasis::NativeUdpIp
                },
                config: None,
                available_data_bytes: 3600,
                maximum_data_bytes: 3600,
                ip_udp_overhead: overhead,
            };
            allowance.reserve_queued(queue.queued_bytes, queue.queued_packets);
            assert_eq!(allowance.available_data_bytes, 1100 - 3 * overhead);
            assert_eq!(allowance.maximum_data_bytes, 3600);
        }
        assert!(handle.drain_at(owner, Outgoing, ms(99)).is_empty());
        assert_eq!(handle.drain_at(owner, Outgoing, ms(100)).len(), 1);
        let queue = handle.queued_outgoing(owner, udp(1));
        assert_eq!((queue.queued_bytes, queue.queued_packets), (1500, 2));
        // Once released, these packets are charged by Egress::admit; their queue
        // reservation disappears. No packet is charged to another peer/direction.
        assert_eq!(handle.drain_at(owner, Outgoing, ms(140)).len(), 3);
        assert_eq!(handle.queued_outgoing(owner, udp(1)).queued_bytes, 0);
        assert_eq!(handle.per_peer_stats()[&udp(1)].incoming.queued_bytes, 600);
    }
    #[test]
    fn peers_and_directions_have_independent_queues() {
        let handle = ServerConditionerHandle::new(config()).unwrap();
        let owner = handle.allocate_owner();
        assert!(handle.defer_at(owner, Incoming, rtc(1), b"one", ms(0)));
        assert!(handle.defer_at(owner, Incoming, rtc(1), b"overflow", ms(0)));
        assert!(handle.defer_at(owner, Incoming, rtc(2), b"two", ms(0)));
        assert!(handle.defer_at(owner, Outgoing, rtc(1), b"out", ms(0)));
        assert_eq!(handle.stats().packets.incoming.overflow_drops, 1);
        assert_eq!(handle.per_peer_stats()[&rtc(2)].incoming.overflow_drops, 0);
        assert!(handle.drain_at(owner, Incoming, ms(99)).is_empty());
        assert_eq!(
            handle.drain_at(owner, Incoming, ms(100)),
            vec![(rtc(1), b"one".to_vec()), (rtc(2), b"two".to_vec())]
        );
        assert_eq!(
            handle.drain_at(owner, Outgoing, ms(100)),
            vec![(rtc(1), b"out".to_vec())]
        );
    }
    #[test]
    fn mixed_owners_share_limit_but_never_drain_or_clear_each_other() {
        let handle = ServerConditionerHandle::new(ServerConditionerConfig {
            max_peers: 2,
            ..config()
        })
        .unwrap();
        let a = handle.allocate_owner();
        let b = handle.allocate_owner();
        handle.defer_at(a, Incoming, udp(1), b"udp", ms(0));
        handle.defer_at(b, Incoming, rtc(1), b"rtc", ms(0));
        handle.defer_at(a, Incoming, udp(2), b"drop", ms(0));
        assert_eq!(handle.stats().peers, 2);
        assert_eq!(handle.stats().peer_limit_drops, 1);
        handle.remove_owner(a);
        assert!(handle.drain_at(a, Incoming, ms(100)).is_empty());
        assert_eq!(
            handle.drain_at(b, Incoming, ms(100)),
            vec![(rtc(1), b"rtc".to_vec())]
        );
        assert_eq!(handle.stats().packets.incoming.transition_drops, 1);
        handle.remove_peer(b, rtc(1));
        assert_eq!(handle.stats().peers, 0);
    }
    #[test]
    fn disable_and_reconfigure_flush_and_apply_to_future_peers() {
        let handle = ServerConditionerHandle::new(config()).unwrap();
        let owner = handle.allocate_owner();
        handle.defer_at(owner, Incoming, rtc(1), b"old", ms(0));
        let mut settings = config();
        settings.packets.enabled = false;
        handle.configure(settings).unwrap();
        assert!(!handle.defer_at(owner, Incoming, rtc(2), b"bypass", ms(10)));
        assert_eq!(handle.stats().peers, 1);
        assert!(handle.drain_at(owner, Incoming, ms(100)).is_empty());
        let mut settings = config();
        settings.packets.packet_loss = 1.0;
        handle.configure(settings).unwrap();
        handle.defer_at(owner, Incoming, rtc(3), b"loss", ms(101));
        assert_eq!(
            handle.per_peer_stats()[&rtc(3)]
                .incoming
                .simulated_loss_drops,
            1
        );
        assert_eq!(handle.stats().packets.incoming.transition_drops, 1);
    }
    #[test]
    fn idle_cleanup_discards_old_sessions_and_frees_unknown_udp_capacity() {
        let handle = ServerConditionerHandle::new(ServerConditionerConfig {
            max_peers: 1,
            idle_timeout: ms(50),
            ..config()
        })
        .unwrap();
        let owner = handle.allocate_owner();
        handle.defer_at(owner, Incoming, udp(1), b"old", ms(0));
        handle.defer_at(owner, Incoming, udp(2), b"new", ms(50));
        assert_eq!(handle.stats().peers, 1);
        assert_eq!(handle.stats().expired_peers, 1);
        assert_eq!(handle.stats().packets.incoming.transition_drops, 1);
        assert!(handle.per_peer_stats().contains_key(&udp(2)));
    }
    #[test]
    fn activity_refreshes_idle_timeout_and_reduced_limit_evicts_oldest() {
        let handle = ServerConditionerHandle::new(ServerConditionerConfig {
            idle_timeout: ms(50),
            ..config()
        })
        .unwrap();
        let owner = handle.allocate_owner();
        handle.defer_at(owner, Incoming, rtc(1), b"one", ms(0));
        handle.defer_at(owner, Incoming, rtc(2), b"two", ms(1));
        handle.defer_at(owner, Outgoing, rtc(1), b"refresh", ms(40));
        handle.drain_at(owner, Incoming, ms(50));
        assert_eq!(handle.stats().expired_peers, 0);
        let mut settings = handle.config();
        settings.max_peers = 1;
        handle.configure(settings).unwrap();
        assert!(handle.per_peer_stats().contains_key(&rtc(1)));
        assert_eq!(handle.stats().peers, 1);
    }
    #[test]
    fn startup_and_new_arrivals_experience_one_global_outage_deadline() {
        let handle = ServerConditionerHandle::default();
        let owner = handle.allocate_owner();
        handle.outage_at(ms(100), ms(0));
        assert!(handle.defer_at(owner, Incoming, rtc(1), b"early", ms(10)));
        assert!(handle.defer_at(owner, Outgoing, udp(2), b"late", ms(90)));
        assert_eq!(handle.stats().packets.incoming.outage_drops, 1);
        assert_eq!(handle.stats().packets.outgoing.outage_drops, 1);
        assert!(handle.stats().packets.outage_active);
        assert!(!handle.defer_at(owner, Incoming, rtc(1), b"recovered", ms(100)));
        assert!(!handle.stats().packets.outage_active);
        assert!(handle.drain_at(owner, Outgoing, ms(100)).is_empty());
    }
    #[test]
    fn outage_discards_pending_and_configuration_cancels_outage() {
        let handle = ServerConditionerHandle::new(config()).unwrap();
        let owner = handle.allocate_owner();
        handle.defer_at(owner, Incoming, rtc(1), b"old", ms(0));
        handle.outage_at(ms(100), ms(1));
        assert_eq!(handle.stats().packets.incoming.transition_drops, 1);
        handle.defer_at(owner, Incoming, rtc(1), b"drop", ms(2));
        handle.configure(config()).unwrap();
        handle.defer_at(owner, Incoming, rtc(1), b"new", ms(3));
        assert_eq!(
            handle.drain_at(owner, Incoming, ms(103)),
            vec![(rtc(1), b"new".to_vec())]
        );
    }
    #[test]
    fn invalid_config_does_not_change_existing_settings() {
        let handle = ServerConditionerHandle::new(config()).unwrap();
        for bad in [
            ServerConditionerConfig {
                max_peers: 0,
                ..config()
            },
            ServerConditionerConfig {
                idle_timeout: Duration::ZERO,
                ..config()
            },
        ] {
            assert!(handle.configure(bad).is_err());
        }
        assert_eq!(handle.config(), config());
    }
    #[test]
    fn independent_peer_seeds_are_stable() {
        assert_eq!(
            peer_config(&config().packets, rtc(1)),
            peer_config(&config().packets, rtc(1))
        );
        assert_ne!(
            peer_config(&config().packets, rtc(1)).seed,
            peer_config(&config().packets, rtc(2)).seed
        );
        assert_ne!(
            peer_config(&config().packets, rtc(1)).seed,
            peer_config(&config().packets, udp(1)).seed
        );
    }
    #[test]
    fn server_handles_remain_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<ServerConditionerHandle>();
        check::<crate::packet_io::ServerPacketGate>();
    }
}
