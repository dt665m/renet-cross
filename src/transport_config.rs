//! Runtime packet tooling configuration. Attach a handle at construction to
//! observe and control the first netcode handshake as well as gameplay packets.
use crate::conditioner::ConditionerHandle;

#[derive(Debug, Clone, Default)]
pub struct ClientTransportConfig {
    /// None bypasses conditioning; Some exposes shared controls and queue stats.
    /// A disabled handle can be enabled later without replacing the transport.
    pub conditioner: Option<ConditionerHandle>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default)]
pub struct ServerTransportConfig {
    /// One handle can govern both backends of a mixed server.
    pub conditioner: Option<crate::server_conditioner::ServerConditionerHandle>,
}
