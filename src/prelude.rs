pub use crate::{MonotonicClientIdAllocator, SessionCreateResponse};

#[cfg(not(target_arch = "wasm32"))]
pub use crate::{
    MixedServerTransport, SdpHttpAnswerResponse, SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest,
    ServerPeer, Str0mNetcodeServerTransport, UdpNetcodeServerTransport, accept_offer_and_add_peer,
};

#[cfg(all(not(target_arch = "wasm32"), feature = "axum"))]
pub use crate::accept_offer_axum_json;

#[cfg(target_arch = "wasm32")]
pub use crate::{WebRtcClientError, WebRtcNetcodeClientTransport, connect_via_sdp_http};

pub use crate::{
    ClientAuthentication, ConnectToken, NETCODE_KEY_BYTES, NETCODE_USER_DATA_BYTES, NetcodeDisconnectReason,
    NetcodeError, ServerAuthentication, ServerConfig, TokenGenerationError, generate_random_bytes,
};
