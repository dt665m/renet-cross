use super::Direction;
use crate::conditioner::{ConditionerDirection, ConditionerHandle, PacketConditioner};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

// Sharing lets browser callbacks enqueue at receipt time, independently of frames.
#[derive(Debug, Clone)]
pub(crate) struct PacketGate<T>(Arc<Mutex<State<T>>>);

#[derive(Debug)]
struct State<T> {
    engine: Option<PacketConditioner<(T, Vec<u8>)>>,
    clock: Clock,
}

impl<T> Drop for State<T> {
    fn drop(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.clear_session();
        }
    }
}

impl<T> Default for PacketGate<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State {
            engine: None,
            clock: Clock::default(),
        })))
    }
}

impl<T> PacketGate<T> {
    pub fn attach(&self, handle: ConditionerHandle) {
        self.detach();
        self.0.lock().unwrap().engine = Some(PacketConditioner::new(handle));
    }
    pub fn handle(&self) -> Option<ConditionerHandle> {
        self.0
            .lock()
            .unwrap()
            .engine
            .as_ref()
            .map(PacketConditioner::handle)
    }
    pub fn detach(&self) {
        self.reset();
        self.0.lock().unwrap().engine = None;
    }
    pub fn reset(&self) {
        if let Some(engine) = &mut self.0.lock().unwrap().engine {
            engine.clear_session();
        }
    }
    /// Borrow packets when unattached; the optional scheduler owns intercepted packets.
    pub fn defer(&self, direction: Direction, metadata: T, bytes: &[u8]) -> bool {
        let mut state = self.0.lock().unwrap();
        let now = state.clock.now();
        let Some(engine) = &mut state.engine else {
            return false;
        };
        // Poll even when disabled to observe configuration transitions/outages.
        // enqueue returns the packet on bypass; the caller still owns its bytes.
        engine
            .enqueue(
                direction.into(),
                now,
                (metadata, bytes.to_vec()),
                bytes.len(),
            )
            .is_none()
    }
    pub fn drain(&self, direction: Direction) -> Vec<(T, Vec<u8>)> {
        let mut state = self.0.lock().unwrap();
        let now = state.clock.now();
        state
            .engine
            .as_mut()
            .map(|engine| engine.drain_ready(direction.into(), now))
            .unwrap_or_default()
    }
}

impl From<Direction> for ConditionerDirection {
    fn from(value: Direction) -> Self {
        match value {
            Direction::Incoming => Self::Incoming,
            Direction::Outgoing => Self::Outgoing,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct Clock(std::time::Instant);
#[cfg(not(target_arch = "wasm32"))]
impl Default for Clock {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl Clock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
struct Clock {}
#[cfg(target_arch = "wasm32")]
impl Clock {
    fn now(&self) -> Duration {
        Duration::from_secs_f64(
            web_sys::window()
                .expect("browser Window required")
                .performance()
                .expect("browser Performance API required")
                .now()
                / 1000.0,
        )
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::conditioner::ConditionerConfig;

    #[test]
    fn shared_callback_queue_and_session_cleanup() {
        let gate = PacketGate::<usize>::default();
        let callback = gate.clone();
        let handle = ConditionerHandle::new(ConditionerConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        gate.attach(handle.clone());
        assert!(callback.defer(Direction::Incoming, 7, b"encrypted"));
        assert_eq!(
            gate.drain(Direction::Incoming),
            vec![(7, b"encrypted".to_vec())]
        );
        assert!(gate.defer(Direction::Outgoing, 8, b"stale"));
        gate.reset();
        assert!(callback.drain(Direction::Outgoing).is_empty());
        assert!(gate.handle().is_some());
        gate.detach();
        assert!(!callback.defer(Direction::Incoming, 9, b"bypass"));
        assert!(gate.handle().is_none());
    }

    #[test]
    fn dropping_last_owner_clears_external_queue_stats() {
        let handle = ConditionerHandle::new(ConditionerConfig {
            enabled: true,
            latency: Duration::from_secs(60),
            ..Default::default()
        })
        .unwrap();
        {
            let gate = PacketGate::<()>::default();
            gate.attach(handle.clone());
            assert!(gate.defer(Direction::Outgoing, (), b"queued"));
            assert_eq!(handle.stats().outgoing.queued_packets, 1);
        }
        assert_eq!(handle.stats().outgoing.queued_packets, 0);
    }

    #[test]
    fn native_transport_remains_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<crate::native_client::UdpNetcodeClientTransport>();
    }
}
