#[cfg(all(feature = "axum", not(target_arch = "wasm32")))]
mod axum_bootstrap;
mod bootstrap;
#[cfg(not(target_arch = "wasm32"))]
mod diagnostics;
#[cfg(not(target_arch = "wasm32"))]
mod error;
#[cfg(not(target_arch = "wasm32"))]
mod mixed_server;
#[cfg(not(target_arch = "wasm32"))]
mod native_client;
#[cfg(not(target_arch = "wasm32"))]
mod netcode_result;
pub mod prelude;
#[cfg(not(target_arch = "wasm32"))]
mod sdp_http;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod transport_builder;
#[cfg(not(target_arch = "wasm32"))]
mod udp_server;
#[cfg(target_arch = "wasm32")]
mod web_client;

#[cfg(all(feature = "axum", not(target_arch = "wasm32")))]
pub use axum_bootstrap::{BootstrapAxumState, bootstrap_router};
pub use bootstrap::{
    BootstrapAuthError, BootstrapConfig, BootstrapError, InMemorySessionRegistry,
    MonotonicClientIdAllocator, SessionAuthPolicy, SessionCreateResponse, SessionIdAllocator,
    UnsecureDevAuthPolicy,
};
#[cfg(not(target_arch = "wasm32"))]
pub use bootstrap::{BootstrapService, DefaultBootstrapService};
#[cfg(not(target_arch = "wasm32"))]
pub use diagnostics::{
    ClientNetworkSnapshot, ServerNetworkSnapshot, TransportKind, collect_server_network_snapshot,
};
#[cfg(not(target_arch = "wasm32"))]
pub use error::TransportError;
#[cfg(not(target_arch = "wasm32"))]
pub use mixed_server::MixedServerTransport;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-async"))]
pub use native_client::connect_via_session_http_async;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-sync"))]
pub use native_client::connect_via_session_http_blocking;
#[cfg(not(target_arch = "wasm32"))]
pub use native_client::{NativeClientError, NativeConnectOptions, UdpNetcodeClientTransport};
#[cfg(not(target_arch = "wasm32"))]
pub use sdp_http::{
    SdpHttpAnswerResponse, SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest,
    accept_offer_and_add_peer,
};
#[cfg(not(target_arch = "wasm32"))]
pub use server::{ServerPeer, WebRtcNetcodeServerTransport};
#[cfg(not(target_arch = "wasm32"))]
pub use transport_builder::MixedTransportBuilder;
#[cfg(not(target_arch = "wasm32"))]
pub use udp_server::UdpNetcodeServerTransport;
#[cfg(target_arch = "wasm32")]
pub use web_client::{
    WebRtcClientError, WebRtcNetcodeClientTransport, connect_via_sdp_http,
    connect_via_sdp_http_with_overrides,
};

#[cfg(feature = "axum")]
#[cfg(not(target_arch = "wasm32"))]
pub use sdp_http::accept_offer_axum_json;

pub use renetcode::{
    ClientAuthentication, ConnectToken, DisconnectReason as NetcodeDisconnectReason,
    NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES, NetcodeError, ServerAuthentication, ServerConfig,
    TokenGenerationError, generate_random_bytes,
};
#[cfg(not(target_arch = "wasm32"))]
pub use str0m::{Rtc, channel::ChannelId};
