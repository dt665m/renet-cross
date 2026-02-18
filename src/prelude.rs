pub use crate::{
    BootstrapAuthError, BootstrapConfig, BootstrapError, InMemorySessionRegistry,
    MonotonicClientIdAllocator, SessionAuthPolicy, SessionCreateResponse, SessionIdAllocator,
    UnsecureDevAuthPolicy,
};

#[cfg(not(target_arch = "wasm32"))]
pub use crate::{
    BootstrapService, ClientNetworkSnapshot, DefaultBootstrapService, MixedServerTransport,
    MixedTransportBuilder, NativeClientError, NativeConnectOptions, SdpHttpAnswerResponse,
    SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest, ServerNetworkSnapshot, ServerPeer,
    TransportKind, UdpNetcodeClientTransport, UdpNetcodeServerTransport,
    WebRtcNetcodeServerTransport, accept_offer_and_add_peer, collect_server_network_snapshot,
};

#[cfg(all(not(target_arch = "wasm32"), feature = "axum"))]
pub use crate::{BootstrapAxumState, accept_offer_axum_json, bootstrap_router};

#[cfg(all(not(target_arch = "wasm32"), feature = "native-sync"))]
pub use crate::connect_via_session_http_blocking;

#[cfg(all(not(target_arch = "wasm32"), feature = "native-async"))]
pub use crate::connect_via_session_http_async;

#[cfg(target_arch = "wasm32")]
pub use crate::{WebRtcClientError, WebRtcNetcodeClientTransport, connect_via_sdp_http};

pub use crate::{
    ClientAuthentication, ConnectToken, NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES,
    NetcodeDisconnectReason, NetcodeError, ServerAuthentication, ServerConfig,
    TokenGenerationError, generate_random_bytes,
};
