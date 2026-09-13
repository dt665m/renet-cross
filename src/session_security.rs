//! Credential-aware bootstrap using renetcode's existing authenticated tokens.
use crate::{BootstrapAuthError, BootstrapConfig, SessionAuthPolicy, SessionCreateResponse};
use base64::{Engine, engine::general_purpose::STANDARD};
use renetcode::{ClientAuthentication, ConnectToken, NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES};
use serde::{Deserialize, Serialize};
use std::{fmt, io::Cursor, net::SocketAddr, time::Duration};

pub const MAX_SESSION_CREDENTIAL_BYTES: usize = 4096;
pub const MAX_SESSION_REQUEST_BYTES: usize = 8192;
pub const MAX_SESSION_RESPONSE_BYTES: usize = 16384;
pub const MAX_SDP_BODY_BYTES: usize = 65536;
pub const MAX_SERVICE_BYTES: usize = 32;
pub const MAX_MATCH_BYTES: usize = 64;
pub const MAX_GRANT_BYTES: usize = 128;
pub const MAX_CONNECT_TOKEN_BYTES: usize = 2048;
pub const MAX_ENCODED_CONNECT_TOKEN_BYTES: usize = 2732;
pub const MAX_SESSION_TOKEN_BYTES: usize = 128;

/// Opaque application credentials. The library does not interpret accounts or
/// trust the requested service/match: the host's admission verifier decides.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionCreateRequest {
    pub protocol_id: Option<u64>,
    #[serde(deserialize_with = "service_string")]
    pub service: String,
    #[serde(deserialize_with = "match_string")]
    pub match_id: String,
    #[serde(deserialize_with = "credential_string")]
    pub credential: String,
    pub require_secure: bool,
}
impl fmt::Debug for SessionCreateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCreateRequest")
            .field("protocol_id", &self.protocol_id)
            .field("service", &self.service)
            .field("match_id", &self.match_id)
            .field("credential", &"[redacted]")
            .field("require_secure", &self.require_secure)
            .finish()
    }
}
impl SessionCreateRequest {
    pub fn validate(&self) -> Result<(), BootstrapAuthError> {
        if self.credential.len() > MAX_SESSION_CREDENTIAL_BYTES
            || self.service.len() > MAX_SERVICE_BYTES
            || self.match_id.len() > MAX_MATCH_BYTES
        {
            return Err(BootstrapAuthError::InvalidRequest);
        }
        Ok(())
    }
    pub fn requests_authentication(&self) -> bool {
        self.require_secure
            || self.protocol_id.is_some()
            || !self.service.is_empty()
            || !self.match_id.is_empty()
            || !self.credential.is_empty()
    }
}

/// This grant must come from an explicit trusted verifier. `replay_key` identifies
/// one admission ticket and is retained until expires_at even after disconnect.
/// A guest verifier is valid, but must be explicitly selected by the host and
/// must not claim that a guest request authenticated an account.
#[derive(Clone)]
pub struct SessionGrant {
    pub protocol_id: u64,
    pub service: String,
    pub match_id: String,
    pub expires_at: u64,
    pub replay_key: [u8; 32],
    pub application: Vec<u8>,
}
impl fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionGrant")
            .field("protocol_id", &self.protocol_id)
            .field("service", &self.service)
            .field("match_id", &self.match_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}
impl SessionGrant {
    pub fn validate(&self, now: Duration) -> Result<(), BootstrapAuthError> {
        if self.service.is_empty()
            || self.service.len() > MAX_SERVICE_BYTES
            || self.match_id.is_empty()
            || self.match_id.len() > MAX_MATCH_BYTES
            || self.application.len() > MAX_GRANT_BYTES
            || self.expires_at <= now.as_secs()
        {
            return Err(BootstrapAuthError::InvalidGrant);
        }
        Ok(())
    }
    /// Service, match, protocol, expiry and opaque grant are authenticated inside
    /// the existing encrypted netcode user_data field, not a custom signature.
    pub fn user_data(&self) -> Result<[u8; NETCODE_USER_DATA_BYTES], BootstrapAuthError> {
        if self.service.len() > MAX_SERVICE_BYTES
            || self.match_id.len() > MAX_MATCH_BYTES
            || self.application.len() > MAX_GRANT_BYTES
        {
            return Err(BootstrapAuthError::InvalidGrant);
        }
        let mut data = [0; NETCODE_USER_DATA_BYTES];
        data[0] = 1;
        data[1..9].copy_from_slice(&self.protocol_id.to_le_bytes());
        data[9..17].copy_from_slice(&self.expires_at.to_le_bytes());
        let mut cursor = 17;
        for bytes in [
            self.service.as_bytes(),
            self.match_id.as_bytes(),
            self.application.as_slice(),
        ] {
            data[cursor] = bytes.len() as u8;
            cursor += 1;
            data[cursor..cursor + bytes.len()].copy_from_slice(bytes);
            cursor += bytes.len();
        }
        debug_assert!(cursor <= NETCODE_USER_DATA_BYTES);
        Ok(data)
    }
}
pub trait SessionAdmission: Send + Sync {
    /// Verify credential authenticity/expiry, authorization and expected protocol,
    /// service and match. Return a stable replay key for a one-use ticket.
    fn admit(
        &self,
        request: &SessionCreateRequest,
        now: Duration,
    ) -> Result<SessionGrant, BootstrapAuthError>;
}
impl<T: SessionAdmission + ?Sized> SessionAdmission for std::sync::Arc<T> {
    fn admit(
        &self,
        request: &SessionCreateRequest,
        now: Duration,
    ) -> Result<SessionGrant, BootstrapAuthError> {
        (**self).admit(request, now)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionSecurity {
    pub protocol_id: u64,
    #[serde(deserialize_with = "service_string")]
    pub service: String,
    #[serde(deserialize_with = "match_string")]
    pub match_id: String,
    pub expires_at: u64,
    #[serde(deserialize_with = "connect_token_string")]
    pub udp_connect_token: String,
    #[serde(deserialize_with = "connect_token_string")]
    pub webrtc_connect_token: String,
}
impl fmt::Debug for SessionSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSecurity")
            .field("protocol_id", &self.protocol_id)
            .field("service", &self.service)
            .field("match_id", &self.match_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}
pub struct SessionIssuance {
    pub session_token: Option<String>,
    pub security: Option<SessionSecurity>,
    pub grant: Option<SessionGrant>,
}

/// Secure token issuer. Supply the same private key/protocol to the transport.
/// Keys and credentials are intentionally excluded from Debug output.
pub struct SecureSessionAuthPolicy<V> {
    verifier: V,
    private_key: [u8; NETCODE_KEY_BYTES],
    timeout_seconds: i32,
}
impl<V> SecureSessionAuthPolicy<V> {
    pub fn new(verifier: V, private_key: [u8; NETCODE_KEY_BYTES]) -> Self {
        Self {
            verifier,
            private_key,
            timeout_seconds: 15,
        }
    }
}
impl<V: SessionAdmission> SessionAuthPolicy for SecureSessionAuthPolicy<V> {
    fn issue_token(&self, _: u64, _: Duration) -> Result<Option<String>, BootstrapAuthError> {
        Err(BootstrapAuthError::AdmissionRequired)
    }
    fn verify_offer(&self, _: u64, _: Option<&str>, _: Duration) -> Result<(), BootstrapAuthError> {
        Err(BootstrapAuthError::AdmissionRequired)
    }
    fn issue_session(
        &self,
        client_id: u64,
        request: &SessionCreateRequest,
        now: Duration,
        config: &BootstrapConfig,
    ) -> Result<SessionIssuance, BootstrapAuthError> {
        request.validate()?;
        let grant = self.verifier.admit(request, now)?;
        grant.validate(now)?;
        if request.protocol_id != Some(grant.protocol_id)
            || request.service != grant.service
            || request.match_id != grant.match_id
            || grant.expires_at - now.as_secs() > config.session_ttl.as_secs()
        {
            return Err(BootstrapAuthError::InvalidGrant);
        }
        let user_data = grant.user_data()?;
        let token = |addr| -> Result<String, BootstrapAuthError> {
            let token = ConnectToken::generate(
                now,
                grant.protocol_id,
                grant.expires_at - now.as_secs(),
                client_id,
                self.timeout_seconds,
                vec![addr],
                Some(&user_data),
                &self.private_key,
            )
            .map_err(|_| BootstrapAuthError::TokenGeneration)?;
            encode_connect_token(&token)
        };
        let security = SessionSecurity {
            protocol_id: grant.protocol_id,
            service: grant.service.clone(),
            match_id: grant.match_id.clone(),
            expires_at: grant.expires_at,
            udp_connect_token: token(config.public_udp_addr)?,
            webrtc_connect_token: token(config.public_webrtc_addr)?,
        };
        let session_token = STANDARD.encode(renetcode::generate_random_bytes::<32>());
        Ok(SessionIssuance {
            session_token: Some(session_token),
            security: Some(security),
            grant: Some(grant),
        })
    }
    fn verify_issued_offer(
        &self,
        client_id: u64,
        token: Option<&str>,
        issued: Option<&str>,
        _: Duration,
    ) -> Result<(), BootstrapAuthError> {
        use subtle::ConstantTimeEq;
        let provided = token.ok_or(BootstrapAuthError::MissingToken { client_id })?;
        let expected = issued.ok_or(BootstrapAuthError::InvalidToken { client_id })?;
        if !bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
            return Err(BootstrapAuthError::InvalidToken { client_id });
        }
        Ok(())
    }
}
pub fn encode_connect_token(token: &ConnectToken) -> Result<String, BootstrapAuthError> {
    let mut bytes = Vec::with_capacity(MAX_CONNECT_TOKEN_BYTES);
    token
        .write(&mut bytes)
        .map_err(|_| BootstrapAuthError::TokenGeneration)?;
    if bytes.len() > MAX_CONNECT_TOKEN_BYTES {
        return Err(BootstrapAuthError::TokenGeneration);
    }
    Ok(STANDARD.encode(bytes))
}

#[derive(Debug, thiserror::Error)]
pub enum SessionSecurityError {
    #[error("secure bootstrap credentials are required")]
    SecureRequired,
    #[error("invalid or oversized session response")]
    InvalidResponse,
    #[error("invalid connect token")]
    InvalidToken,
    #[error("connect token identity or endpoint does not match this session")]
    BindingMismatch,
    #[error("session connect token expired or not yet valid")]
    Expired,
}
#[derive(Debug, Clone, Copy)]
pub enum SessionTransport {
    Udp,
    WebRtc,
}
impl SessionCreateResponse {
    /// Validate server bootstrap binding before any connection attempt. This
    /// client-side parse is not token authentication: renetcode verifies private
    /// token data at the server. Never fall back if a secure envelope is present.
    pub fn authentication(
        &self,
        protocol_id: u64,
        server_addr: SocketAddr,
        transport: SessionTransport,
        now: Duration,
        require_secure: bool,
        request: &SessionCreateRequest,
    ) -> Result<ClientAuthentication, SessionSecurityError> {
        request
            .validate()
            .map_err(|_| SessionSecurityError::InvalidResponse)?;
        if self.udp_addr.len() > 128
            || self.webrtc_addr.len() > 128
            || self.webrtc_offer_url.len() > 2048
            || self
                .session_token
                .as_ref()
                .is_some_and(|t| t.len() > MAX_SESSION_TOKEN_BYTES)
        {
            return Err(SessionSecurityError::InvalidResponse);
        }
        let Some(security) = &self.security else {
            if require_secure || request.requests_authentication() {
                return Err(SessionSecurityError::SecureRequired);
            }
            return Ok(ClientAuthentication::Unsecure {
                protocol_id,
                client_id: self.client_id,
                server_addr,
                user_data: None,
            });
        };
        if security.protocol_id != protocol_id
            || request.protocol_id.is_some_and(|p| p != protocol_id)
            || (!request.service.is_empty() && request.service != security.service)
            || (!request.match_id.is_empty() && request.match_id != security.match_id)
        {
            return Err(SessionSecurityError::BindingMismatch);
        }
        if security.service.is_empty()
            || security.service.len() > MAX_SERVICE_BYTES
            || security.match_id.is_empty()
            || security.match_id.len() > MAX_MATCH_BYTES
        {
            return Err(SessionSecurityError::InvalidResponse);
        }
        let encoded = match transport {
            SessionTransport::Udp => &security.udp_connect_token,
            SessionTransport::WebRtc => &security.webrtc_connect_token,
        };
        if encoded.len() > MAX_ENCODED_CONNECT_TOKEN_BYTES {
            return Err(SessionSecurityError::InvalidToken);
        }
        let mut bytes = [0; MAX_CONNECT_TOKEN_BYTES];
        let length = STANDARD
            .decode_slice(encoded, &mut bytes)
            .map_err(|_| SessionSecurityError::InvalidToken)?;
        let mut reader = Cursor::new(&bytes[..length]);
        let token =
            ConnectToken::read(&mut reader).map_err(|_| SessionSecurityError::InvalidToken)?;
        if reader.position() as usize != length
            || token.client_id != self.client_id
            || token.protocol_id != protocol_id
            || token.server_addresses[0] != Some(server_addr)
            || token.server_addresses.iter().skip(1).any(Option::is_some)
            || token.expire_timestamp != security.expires_at
            || token.timeout_seconds <= 0
        {
            return Err(SessionSecurityError::BindingMismatch);
        }
        if token.create_timestamp > now.as_secs()
            || token.expire_timestamp <= now.as_secs()
            || token.expire_timestamp <= token.create_timestamp
        {
            return Err(SessionSecurityError::Expired);
        }
        Ok(ClientAuthentication::Secure {
            connect_token: token,
        })
    }
}

fn bounded_string<'de, D: serde::Deserializer<'de>, const LIMIT: usize>(
    deserializer: D,
) -> Result<String, D::Error> {
    struct Bounded<const N: usize>;
    impl<'de, const N: usize> serde::de::Visitor<'de> for Bounded<N> {
        type Value = String;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a bounded bootstrap string")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > N {
                return Err(E::custom("bootstrap field exceeds size limit"));
            }
            Ok(value.to_owned())
        }
        fn visit_string<E: serde::de::Error>(self, value: String) -> Result<String, E> {
            if value.len() > N {
                return Err(E::custom("bootstrap field exceeds size limit"));
            }
            Ok(value)
        }
    }
    deserializer.deserialize_string(Bounded::<LIMIT>)
}
fn credential_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string::<D, MAX_SESSION_CREDENTIAL_BYTES>(d)
}
fn service_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string::<D, MAX_SERVICE_BYTES>(d)
}
fn match_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string::<D, MAX_MATCH_BYTES>(d)
}
fn connect_token_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_string::<D, MAX_ENCODED_CONNECT_TOKEN_BYTES>(d)
}
