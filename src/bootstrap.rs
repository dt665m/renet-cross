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

use crate::session_security::*;
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

    pub fn try_next(&self) -> Option<ClientId> {
        self.next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
    }
    pub fn next(&self) -> ClientId {
        self.try_next().expect("client identity exhausted")
    }
}

pub trait SessionIdAllocator: Send + Sync {
    fn next_client_id(&self) -> ClientId;
    fn try_next_client_id(&self) -> Option<ClientId> {
        Some(self.next_client_id())
    }
}

impl SessionIdAllocator for MonotonicClientIdAllocator {
    fn try_next_client_id(&self) -> Option<ClientId> {
        self.try_next()
    }
    fn next_client_id(&self) -> ClientId {
        self.next()
    }
}

impl<T> SessionIdAllocator for std::sync::Arc<T>
where
    T: SessionIdAllocator + ?Sized,
{
    fn try_next_client_id(&self) -> Option<ClientId> {
        (**self).try_next_client_id()
    }
    fn next_client_id(&self) -> ClientId {
        (**self).next_client_id()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionCreateResponse {
    pub client_id: ClientId,
    pub udp_addr: String,
    pub webrtc_addr: String,
    pub webrtc_offer_url: String,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub security: Option<SessionSecurity>,
}
impl std::fmt::Debug for SessionCreateResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCreateResponse")
            .field("client_id", &self.client_id)
            .field("security", &self.security)
            .finish_non_exhaustive()
    }
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
    limits: BootstrapLimits,
    details: HashMap<ClientId, SessionDetails>,
    used_grants: HashMap<[u8; 32], Instant>,
}
#[derive(Debug, Clone, Copy)]
pub struct BootstrapLimits {
    pub pending_sessions: usize,
    pub active_sessions: usize,
    pub replay_grants: usize,
}
impl Default for BootstrapLimits {
    fn default() -> Self {
        Self {
            pending_sessions: 1024,
            active_sessions: 256,
            replay_grants: 8192,
        }
    }
}
struct SessionDetails {
    #[cfg(not(target_arch = "wasm32"))]
    token: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    grant: Option<SessionGrant>,
    #[cfg(not(target_arch = "wasm32"))]
    user_data: Option<[u8; 256]>,
    offer_claimed: bool,
    expires: Option<Instant>,
}
impl std::fmt::Debug for SessionDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDetails")
            .field("offer_claimed", &self.offer_claimed)
            .finish_non_exhaustive()
    }
}

impl InMemorySessionRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            active: HashSet::new(),
            ttl,
            limits: BootstrapLimits::default(),
            details: HashMap::new(),
            used_grants: HashMap::new(),
        }
    }

    pub fn with_limits(ttl: Duration, limits: BootstrapLimits) -> Result<Self, BootstrapError> {
        if limits.pending_sessions == 0 || limits.active_sessions == 0 || limits.replay_grants == 0
        {
            return Err(BootstrapError::Capacity);
        }
        let mut registry = Self::new(ttl);
        registry.limits = limits;
        Ok(registry)
    }
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.pending.len(),
            self.active.len(),
            self.used_grants.len(),
        )
    }
    pub fn try_issue(&mut self, client_id: ClientId) -> Result<(), BootstrapError> {
        self.try_issue_at(client_id, Instant::now())
    }
    fn try_issue_at(&mut self, client_id: ClientId, now: Instant) -> Result<(), BootstrapError> {
        self.cleanup_at(now);
        if self.pending.len() >= self.limits.pending_sessions
            || self.active.len() >= self.limits.active_sessions
        {
            return Err(BootstrapError::Capacity);
        }
        if self.pending.contains_key(&client_id) || self.active.contains(&client_id) {
            return Err(BootstrapError::DuplicateSession);
        }
        self.pending.insert(client_id, now);
        Ok(())
    }
    /// Legacy insertion; callers needing an explicit capacity verdict use try_issue.
    pub fn issue(&mut self, client_id: ClientId) {
        self.issue_at(client_id, Instant::now());
    }

    fn issue_at(&mut self, client_id: ClientId, now: Instant) {
        let _ = self.try_issue_at(client_id, now);
    }

    pub fn is_pending(&mut self, client_id: ClientId) -> bool {
        self.is_pending_at(client_id, Instant::now())
    }

    fn is_pending_at(&mut self, client_id: ClientId, now: Instant) -> bool {
        self.cleanup_at(now);
        self.pending.contains_key(&client_id)
    }

    pub fn activate(&mut self, client_id: ClientId) -> bool {
        self.activate_at(client_id, Instant::now())
    }

    fn activate_at(&mut self, client_id: ClientId, now: Instant) -> bool {
        self.cleanup_at(now);
        if self.active.len() >= self.limits.active_sessions {
            return false;
        }
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
        self.details.remove(&client_id);
    }

    /// Remove pending sessions whose age is at least the configured TTL.
    /// Active sessions are retained until explicitly deactivated.
    pub fn cleanup(&mut self) {
        self.cleanup_at(Instant::now());
    }

    fn cleanup_at(&mut self, now: Instant) {
        let ttl = self.ttl;
        let details = &self.details;
        self.pending.retain(|id, started| {
            now.saturating_duration_since(*started) < ttl
                && details
                    .get(id)
                    .is_none_or(|detail| detail.expires.is_none_or(|expires| expires > now))
        });
        self.details
            .retain(|id, _| self.pending.contains_key(id) || self.active.contains(id));
        self.used_grants.retain(|_, expires| *expires > now);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapAuthError {
    #[error("invalid or oversized bootstrap request")]
    InvalidRequest,
    #[error("credential-aware secure admission is required")]
    AdmissionRequired,
    #[error("invalid or expired application session grant")]
    InvalidGrant,
    #[error("session grant was already consumed")]
    Replay,
    #[error("connect token generation failed")]
    TokenGeneration,
    #[error("missing bootstrap token for client {client_id}")]
    MissingToken { client_id: ClientId },
    #[error("invalid bootstrap token for client {client_id}")]
    InvalidToken { client_id: ClientId },
    #[error("bootstrap auth error: {message}")]
    Message { message: String },
}

pub trait SessionAuthPolicy: Send + Sync {
    /// Legacy policies must never silently ignore a request for authentication.
    fn issue_session(
        &self,
        client_id: ClientId,
        request: &SessionCreateRequest,
        now: Duration,
        _: &BootstrapConfig,
    ) -> Result<SessionIssuance, BootstrapAuthError> {
        request.validate()?;
        if request.requests_authentication() {
            return Err(BootstrapAuthError::AdmissionRequired);
        }
        Ok(SessionIssuance {
            session_token: self.issue_token(client_id, now)?,
            security: None,
            grant: None,
        })
    }
    fn verify_issued_offer(
        &self,
        client_id: ClientId,
        token: Option<&str>,
        _: Option<&str>,
        now: Duration,
    ) -> Result<(), BootstrapAuthError> {
        self.verify_offer(client_id, token, now)
    }

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
    fn issue_session(
        &self,
        client_id: ClientId,
        request: &SessionCreateRequest,
        now: Duration,
        config: &BootstrapConfig,
    ) -> Result<SessionIssuance, BootstrapAuthError> {
        (**self).issue_session(client_id, request, now, config)
    }
    fn verify_issued_offer(
        &self,
        client_id: ClientId,
        token: Option<&str>,
        issued: Option<&str>,
        now: Duration,
    ) -> Result<(), BootstrapAuthError> {
        (**self).verify_issued_offer(client_id, token, issued, now)
    }
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
    #[error("bootstrap session capacity reached")]
    Capacity,
    #[error("bootstrap client id is already issued")]
    DuplicateSession,
    #[error("bootstrap request or response exceeds its size limit")]
    BodyTooLarge,
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

    pub fn with_limits(
        config: BootstrapConfig,
        allocator: A,
        auth_policy: P,
        limits: BootstrapLimits,
    ) -> Result<Self, BootstrapError> {
        Ok(Self {
            allocator,
            registry: Mutex::new(InMemorySessionRegistry::with_limits(
                config.session_ttl,
                limits,
            )?),
            auth_policy,
            config,
        })
    }
    /// Explicit legacy development path. Secure policies reject the empty request.
    pub fn create_session(&self) -> Result<SessionCreateResponse, BootstrapError> {
        self.create_session_with_request(&SessionCreateRequest::default())
    }
    pub fn create_session_with_request(
        &self,
        request: &SessionCreateRequest,
    ) -> Result<SessionCreateResponse, BootstrapError> {
        request.validate()?;
        let now = unix_now_duration()?;
        let monotonic = Instant::now();
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        registry.cleanup_at(monotonic);
        if registry.pending.len() >= registry.limits.pending_sessions
            || registry.active.len() >= registry.limits.active_sessions
        {
            return Err(BootstrapError::Capacity);
        }
        let client_id = self
            .allocator
            .try_next_client_id()
            .ok_or(BootstrapError::Capacity)?;
        let issued = self
            .auth_policy
            .issue_session(client_id, request, now, &self.config)?;
        if issued.security.as_ref().is_some_and(|security| {
            security.service.len() > MAX_SERVICE_BYTES
                || security.match_id.len() > MAX_MATCH_BYTES
                || security.udp_connect_token.len() > MAX_ENCODED_CONNECT_TOKEN_BYTES
                || security.webrtc_connect_token.len() > MAX_ENCODED_CONNECT_TOKEN_BYTES
        }) || self.config.public_http_base.len() > 2048
        {
            return Err(BootstrapError::BodyTooLarge);
        }
        if issued
            .session_token
            .as_ref()
            .is_some_and(|token| token.len() > MAX_SESSION_TOKEN_BYTES)
        {
            return Err(BootstrapError::BodyTooLarge);
        }
        if issued.security.is_some() != issued.grant.is_some()
            || (request.requests_authentication() && issued.security.is_none())
        {
            return Err(BootstrapAuthError::AdmissionRequired.into());
        }
        let user_data = issued
            .grant
            .as_ref()
            .map(SessionGrant::user_data)
            .transpose()?;
        let replay = if let Some(grant) = &issued.grant {
            grant.validate(now)?;
            if registry.used_grants.contains_key(&grant.replay_key) {
                return Err(BootstrapAuthError::Replay.into());
            }
            if registry.used_grants.len() >= registry.limits.replay_grants {
                return Err(BootstrapError::Capacity);
            }
            let expires = monotonic
                .checked_add(Duration::from_secs(grant.expires_at - now.as_secs()))
                .ok_or(BootstrapAuthError::InvalidGrant)?;
            Some((grant.replay_key, expires))
        } else {
            None
        };
        let response = SessionCreateResponse {
            client_id,
            udp_addr: self.config.public_udp_addr.to_string(),
            webrtc_addr: self.config.public_webrtc_addr.to_string(),
            webrtc_offer_url: self.config.offer_url(client_id),
            session_token: issued.session_token.clone(),
            security: issued.security,
        };
        if serde_json::to_vec(&response)
            .map_err(|_| BootstrapError::BodyTooLarge)?
            .len()
            > MAX_SESSION_RESPONSE_BYTES
        {
            return Err(BootstrapError::BodyTooLarge);
        }
        registry.try_issue_at(client_id, monotonic)?;
        registry.details.insert(
            client_id,
            SessionDetails {
                token: issued.session_token,
                grant: issued.grant,
                user_data,
                offer_claimed: false,
                expires: replay.map(|(_, expiry)| expiry),
            },
        );
        if let Some((key, expiry)) = replay {
            registry.used_grants.insert(key, expiry);
        }
        Ok(response)
    }

    pub fn accept_offer(
        &self,
        transport: &mut WebRtcNetcodeServerTransport,
        client_id: ClientId,
        offer: SdpHttpOfferRequest,
        hook: SdpHttpHookConfig,
    ) -> Result<SdpHttpAnswerResponse, BootstrapError> {
        if offer.sdp.len() > MAX_SDP_BODY_BYTES
            || offer
                .session_token
                .as_ref()
                .is_some_and(|t| t.len() > MAX_SESSION_TOKEN_BYTES)
        {
            return Err(BootstrapError::BodyTooLarge);
        }
        let now = unix_now_duration()?;
        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
            if !registry.is_pending(client_id) {
                return Err(BootstrapError::UnknownSession { client_id });
            }
            let detail = registry
                .details
                .get_mut(&client_id)
                .ok_or(BootstrapError::UnknownSession { client_id })?;
            if detail.offer_claimed
                || detail
                    .grant
                    .as_ref()
                    .is_some_and(|g| g.expires_at <= now.as_secs())
            {
                return Err(BootstrapError::UnknownSession { client_id });
            }
            self.auth_policy.verify_issued_offer(
                client_id,
                offer.session_token.as_deref(),
                detail.token.as_deref(),
                now,
            )?;
            // Claim before constructing RTC state. Failed offers consume the claim;
            // retry requires a new admitted session, not repeated unauthenticated allocation.
            detail.offer_claimed = true;
        }

        let answer = accept_offer_and_add_peer(transport, client_id, offer, hook)?;
        Ok(answer)
    }

    /// Legacy activation. Secure sessions require the authenticated netcode
    /// user_data returned by the transport's connection event/accessor.
    pub fn on_client_connected(&self, client_id: ClientId) -> Result<(), BootstrapError> {
        self.activate_session(client_id, None)
    }
    pub fn on_client_connected_with_user_data(
        &self,
        client_id: ClientId,
        user_data: &[u8; renetcode::NETCODE_USER_DATA_BYTES],
    ) -> Result<(), BootstrapError> {
        self.activate_session(client_id, Some(user_data))
    }
    fn activate_session(
        &self,
        client_id: ClientId,
        user_data: Option<&[u8; 256]>,
    ) -> Result<(), BootstrapError> {
        use subtle::ConstantTimeEq;
        let now = unix_now_duration()?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        if !registry.is_pending(client_id) {
            return Err(BootstrapError::UnknownSession { client_id });
        }
        if let Some(detail) = registry.details.get(&client_id) {
            if detail
                .grant
                .as_ref()
                .is_some_and(|g| g.expires_at <= now.as_secs())
            {
                return Err(BootstrapAuthError::InvalidGrant.into());
            }
            if let Some(expected) = &detail.user_data {
                let actual = user_data.ok_or(BootstrapAuthError::AdmissionRequired)?;
                if !bool::from(actual.ct_eq(expected)) {
                    return Err(BootstrapAuthError::InvalidGrant.into());
                }
            }
        }
        if registry.activate(client_id) {
            Ok(())
        } else {
            Err(BootstrapError::UnknownSession { client_id })
        }
    }
    pub fn session_grant(
        &self,
        client_id: ClientId,
    ) -> Result<Option<SessionGrant>, BootstrapError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| BootstrapError::SessionRegistryPoisoned)?;
        registry.cleanup();
        Ok(registry
            .details
            .get(&client_id)
            .and_then(|detail| detail.grant.clone()))
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
    use std::time::{Duration, Instant};

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
    fn session_registry_expires_at_exact_deadline() {
        let start = Instant::now();
        let ttl = Duration::from_secs(5);
        let deadline = start + ttl;
        let mut registry = InMemorySessionRegistry::new(ttl);
        registry.issue_at(1, start);
        registry.issue_at(2, start);
        assert!(registry.is_pending_at(1, deadline - Duration::from_nanos(1)));
        // Activation itself performs expiry cleanup; no preceding cleanup call.
        assert!(!registry.activate_at(2, deadline));
        assert!(!registry.is_pending_at(1, deadline));
        assert!(registry.pending.is_empty());
        assert!(registry.active.is_empty());
    }

    #[test]
    fn session_registry_zero_ttl_expires_without_clock_advance() {
        let now = Instant::now();
        let mut registry = InMemorySessionRegistry::new(Duration::ZERO);
        registry.issue_at(1, now);
        assert!(!registry.is_pending_at(1, now));
        assert!(!registry.activate_at(1, now));
    }

    #[test]
    fn session_registry_cleanup_retains_active_and_newer_pending_sessions() {
        let start = Instant::now();
        let ttl = Duration::from_secs(5);
        let mut registry = InMemorySessionRegistry::new(ttl);
        registry.issue_at(1, start);
        registry.issue_at(2, start);
        assert!(registry.activate_at(1, start + Duration::from_secs(1)));
        registry.issue_at(3, start + Duration::from_secs(4));
        registry.cleanup_at(start + ttl);
        assert!(registry.active.contains(&1));
        assert!(!registry.pending.contains_key(&1));
        assert!(!registry.pending.contains_key(&2));
        assert!(registry.pending.contains_key(&3));
        assert!(!registry.activate_at(1, start + ttl));
        registry.cleanup_at(start + Duration::from_secs(100));
        assert!(registry.active.contains(&1));
        assert!(registry.pending.is_empty());
    }

    #[test]
    fn session_registry_deactivate_and_reissue_gets_a_fresh_ttl() {
        let start = Instant::now();
        let mut registry = InMemorySessionRegistry::new(Duration::from_secs(5));
        registry.issue_at(1, start);
        assert!(registry.activate_at(1, start));
        registry.deactivate(1);
        assert!(registry.active.is_empty());
        registry.issue_at(1, start + Duration::from_secs(10));
        assert!(registry.is_pending_at(1, start + Duration::from_secs(14)));
        registry.deactivate(1);
        assert!(!registry.activate_at(1, start + Duration::from_secs(14)));
        registry.issue_at(1, start + Duration::from_secs(20));
        assert!(!registry.activate_at(1, start + Duration::from_secs(25)));
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
