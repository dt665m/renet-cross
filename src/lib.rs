mod bootstrap;
#[cfg(not(target_arch = "wasm32"))]
mod netcode_result;
pub mod prelude;
#[cfg(target_arch = "wasm32")]
mod web_client;
#[cfg(not(target_arch = "wasm32"))]
mod error;
#[cfg(not(target_arch = "wasm32"))]
mod mixed_server;
#[cfg(not(target_arch = "wasm32"))]
mod sdp_http;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
mod udp_server;

pub use bootstrap::{MonotonicClientIdAllocator, SessionCreateResponse};
#[cfg(target_arch = "wasm32")]
pub use web_client::{
    connect_via_sdp_http, connect_via_sdp_http_with_overrides, WebRtcClientError, WebRtcNetcodeClientTransport,
};
#[cfg(not(target_arch = "wasm32"))]
pub use error::TransportError;
#[cfg(not(target_arch = "wasm32"))]
pub use mixed_server::MixedServerTransport;
#[cfg(not(target_arch = "wasm32"))]
pub use sdp_http::{
    accept_offer_and_add_peer, SdpHttpAnswerResponse, SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest,
};
#[cfg(not(target_arch = "wasm32"))]
pub use server::{ServerPeer, Str0mNetcodeServerTransport};
#[cfg(not(target_arch = "wasm32"))]
pub use udp_server::UdpNetcodeServerTransport;

#[cfg(feature = "axum")]
#[cfg(not(target_arch = "wasm32"))]
pub use sdp_http::accept_offer_axum_json;

pub use renetcode::{
    generate_random_bytes, ClientAuthentication, ConnectToken, DisconnectReason as NetcodeDisconnectReason, NetcodeError,
    ServerAuthentication, ServerConfig, TokenGenerationError, NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES,
};
#[cfg(not(target_arch = "wasm32"))]
pub use str0m::{channel::ChannelId, Rtc};
