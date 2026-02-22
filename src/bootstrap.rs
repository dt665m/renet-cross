use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use renet::ClientId;
use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use crate::{
    SdpHttpAnswerResponse, SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest,
    WebRtcNetcodeServerTransport, accept_offer_and_add_peer,
};

#[derive(Debug, Default)]
pub struct MonotonicClientIdAllocator {
    next_id: AtomicU64,
}

impl MonotonicClientIdAllocator {
    pub fn new(start_at: ClientId) -> Self {
        Self {
            next_id: AtomicU64::new(start_at),
        }
    }

    pub fn next(&self) -> ClientId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

pub trait SessionIdAllocator: Send + Sync {
    fn next_client_id(&self) -> ClientId;
}

impl SessionIdAllocator for MonotonicClientIdAllocator {
    fn next_client_id(&self) -> ClientId {
        self.next()
    }
}

impl<T> SessionIdAllocator for std::sync::Arc<T>
where
    T: SessionIdAllocator + ?Sized,
{
    fn next_client_id(&self) -> ClientId {
        (**self).next_client_id()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateResponse {
    pub client_id: ClientId,
    pub udp_addr: String,
    pub webrtc_addr: String,
    pub webrtc_offer_url: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub session_ttl: Duration,
    pub public_udp_addr: SocketAddr,
    pub public_webrtc_addr: SocketAddr,
    pub public_http_base: String,
}

impl BootstrapConfig {
    pub fn offer_url(&self, client_id: ClientId) -> String {
        format!(
            "{}/api/webrtc/offer/{client_id}",
            self.public_http_base.trim_end_matches('/')
        )
    }
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_secs(120),
            public_udp_addr: SocketAddr::from(([127, 0, 0, 1], 5000)),
            public_webrtc_addr: SocketAddr::from(([127, 0, 0, 1], 5001)),
            public_http_base: "http://127.0.0.1:8080".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct InMemorySessionRegistry {
    pending: HashMap<ClientId, Instant>,
    active: HashSet<ClientId>,
    ttl: Duration,
}

impl InMemorySessionRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            active: HashSet::new(),
            ttl,
        }
    }

    pub fn issue(&mut self, client_id: ClientId) {
        self.cleanup();
        self.pending.insert(client_id, Instant::now());
    }

    pub fn is_pending(&mut self, client_id: ClientId) -> bool {
        self.cleanup();
        self.pending.contains_key(&client_id)
    }

    pub fn activate(&mut self, client_id: ClientId) -> bool {
        self.cleanup();
        if self.pending.remove(&client_id).is_some() {
            self.active.insert(client_id);
            true
        } else {
            false
        }
    }

    pub fn deactivate(&mut self, client_id: ClientId) {
        self.pending.remove(&client_id);
        self.active.remove(&client_id);
    }

    pub fn cleanup(&mut self) {
        let ttl = self.ttl;
        self.pending.retain(|_, started| started.elapsed() <= ttl);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapAuthError {
    #[error("missing bootstrap token for client {client_id}")]
    MissingToken { client_id: ClientId },
    #[error("invalid bootstrap token for client {client_id}")]
    InvalidToken { client_id: ClientId },
    #[error("bootstrap auth error: {message}")]
    Message { message: String },
}

pub trait SessionAuthPolicy: Send + Sync {
    fn issue_token(
        &self,
        client_id: ClientId,
        now: Duration,
    ) -> Result<Option<String>, BootstrapAuthError>;

    fn verify_offer(
        &self,
        client_id: ClientId,
        token: Option<&str>,
        now: Duration,
    ) -> Result<(), BootstrapAuthError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnsecureDevAuthPolicy;

impl SessionAuthPolicy for UnsecureDevAuthPolicy {
    fn issue_token(
        &self,
        _client_id: ClientId,
        _now: Duration,
    ) -> Result<Option<String>, BootstrapAuthError> {
        Ok(None)
    }

    fn verify_offer(
        &self,
        _client_id: ClientId,
        _token: Option<&str>,
        _now: Duration,
    ) -> Result<(), BootstrapAuthError> {
        Ok(())
    }
}

impl<T> SessionAuthPolicy for std::sync::Arc<T>
where
    T: SessionAuthPolicy + ?Sized,
{
    fn issue_token(
        &self,
        client_id: ClientId,
        now: Duration,
    ) -> Result<Option<String>, BootstrapAuthError> {
        (**self).issue_token(client_id, now)
    }

    fn verify_offer(
        &self,
        client_id: ClientId,
        token: Option<&str>,
        now: Duration,
    ) -> Result<(), BootstrapAuthError> {
        (**self).verify_offer(client_id, token, now)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("session registry lock poisoned")]
    SessionRegistryPoisoned,
    #[error("unknown or expired session id: {client_id}")]
    UnknownSession { client_id: ClientId },
    #[error(transparent)]
    Auth(#[from] BootstrapAuthError),
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    Hook(#[from] SdpHttpHookError),
    #[error(transparent)]
    Clock(#[from] std::time::SystemTimeError),
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct BootstrapService<A = MonotonicClientIdAllocator, P = UnsecureDevAuthPolicy>
where
    A: SessionIdAllocator,
    P: SessionAuthPolicy,
{
    allocator: A,
    registry: Mutex<InMemorySessionRegistry>,
    auth_policy: P,
    config: BootstrapConfig,
}

#[cfg(not(target_arch = "wasm32"))]
impl<A, P> BootstrapService<A, P>
where
    A: SessionIdAllocator,
    P: SessionAuthPolicy,
{
    pub fn new(config: BootstrapConfig, allocator: A, auth_policy: P) -> Self {
        Self {
            allocator,
            registry: Mutex::new(InMemorySessionRegistry::new(config.session_ttl)),
            auth_policy,
            config,
        }
    }

    pub fn config(&self) -> &BootstrapConfig {
        &self.config
    }

    pub fn create_session(&self) -> Result<SessionCreateResponse, BootstrapError> {
        let client_id = self.allocator.next_client_id();
        let now = unix_now_duration()?;
        let session_token = self.auth_policy.issue_token(client_id, now)?;

        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        registry.issue(client_id);

        Ok(SessionCreateResponse {
            client_id,
            udp_addr: self.config.public_udp_addr.to_string(),
            webrtc_addr: self.config.public_webrtc_addr.to_string(),
            webrtc_offer_url: self.config.offer_url(client_id),
            session_token,
        })
    }

    pub fn accept_offer(
        &self,
        transport: &mut WebRtcNetcodeServerTransport,
        client_id: ClientId,
        offer: SdpHttpOfferRequest,
        hook: SdpHttpHookConfig,
    ) -> Result<SdpHttpAnswerResponse, BootstrapError> {
        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
            if !registry.is_pending(client_id) {
                return Err(BootstrapError::UnknownSession { client_id });
            }
        }

        let now = unix_now_duration()?;
        self.auth_policy
            .verify_offer(client_id, offer.session_token.as_deref(), now)?;

        let answer = accept_offer_and_add_peer(transport, client_id, offer, hook)?;
        Ok(answer)
    }

    pub fn on_client_connected(&self, client_id: ClientId) -> Result<(), BootstrapError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;

        if registry.activate(client_id) {
            Ok(())
        } else {
            Err(BootstrapError::UnknownSession { client_id })
        }
    }

    pub fn on_client_disconnected(&self, client_id: ClientId) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.deactivate(client_id);
        }
    }

    pub fn cleanup_sessions(&self) -> Result<(), BootstrapError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        registry.cleanup();
        Ok(())
    }

    pub fn is_pending_session(&self, client_id: ClientId) -> Result<bool, BootstrapError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        Ok(registry.is_pending(client_id))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub type DefaultBootstrapService =
    BootstrapService<MonotonicClientIdAllocator, UnsecureDevAuthPolicy>;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn unix_now_duration() -> Result<Duration, std::time::SystemTimeError> {
    SystemTime::now().duration_since(UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        InMemorySessionRegistry, MonotonicClientIdAllocator, SessionAuthPolicy,
        UnsecureDevAuthPolicy,
    };

    #[test]
    fn monotonic_allocator_is_monotonic() {
        let allocator = MonotonicClientIdAllocator::new(10);
        assert_eq!(allocator.next(), 10);
        assert_eq!(allocator.next(), 11);
        assert_eq!(allocator.next(), 12);
    }

    #[test]
    fn monotonic_allocator_unique_in_sequence() {
        let allocator = MonotonicClientIdAllocator::new(1);
        let mut values = vec![];
        for _ in 0..256 {
            values.push(allocator.next());
        }

        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), 256);
    }

    #[test]
    fn session_registry_lifecycle() {
        let mut registry = InMemorySessionRegistry::new(Duration::from_secs(60));
        registry.issue(5);
        assert!(registry.is_pending(5));
        assert!(registry.activate(5));
        assert!(!registry.is_pending(5));
        assert!(!registry.activate(5));
        registry.deactivate(5);
        assert!(!registry.is_pending(5));
    }

    #[test]
    fn session_registry_expires_pending_entries() {
        let mut registry = InMemorySessionRegistry::new(Duration::ZERO);
        registry.issue(1);
        std::thread::sleep(Duration::from_millis(1));
        registry.cleanup();
        assert!(!registry.is_pending(1));
    }

    #[test]
    fn unsecure_auth_policy_accepts_missing_token() {
        let policy = UnsecureDevAuthPolicy;
        let issued = policy
            .issue_token(10, Duration::from_secs(5))
            .expect("issue token");
        assert!(issued.is_none());
        policy
            .verify_offer(10, None, Duration::from_secs(5))
            .expect("verify token");
    }
}
