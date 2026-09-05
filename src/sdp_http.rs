use std::{net::SocketAddr, time::Instant};

use renet::ClientId;
use serde::{Deserialize, Serialize};
use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::IceError, net::Protocol};

use crate::WebRtcNetcodeServerTransport;

#[derive(Debug, Clone, Copy)]
pub struct SdpHttpHookConfig {
    pub local_candidate_addr: SocketAddr,
}

impl SdpHttpHookConfig {
    pub fn new(local_candidate_addr: SocketAddr) -> Self {
        Self {
            local_candidate_addr,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SdpHttpOfferRequest {
    pub sdp: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SdpHttpAnswerResponse {
    pub client_id: ClientId,
    pub sdp: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SdpHttpHookError {
    #[error("client {client_id} already exists in webrtc transport")]
    DuplicateClientId { client_id: ClientId },
    #[error("webrtc peer capacity reached ({max_clients})")]
    CapacityReached { max_clients: usize },
    #[error("invalid SDP offer: {message}")]
    InvalidOffer { message: String },
    #[error(transparent)]
    Ice(#[from] IceError),
    #[error(transparent)]
    Rtc(#[from] RtcError),
}

pub fn accept_offer_and_add_peer(
    transport: &mut WebRtcNetcodeServerTransport,
    client_id: ClientId,
    offer: SdpHttpOfferRequest,
    config: SdpHttpHookConfig,
) -> Result<SdpHttpAnswerResponse, SdpHttpHookError> {
    if transport.peer(client_id).is_some() {
        return Err(SdpHttpHookError::DuplicateClientId { client_id });
    }
    // Count pending ICE/DTLS negotiations too: netcode's connected-client limit
    // alone does not bound the memory spent on unauthenticated RTC instances.
    if transport.peer_count() >= transport.max_clients() {
        return Err(SdpHttpHookError::CapacityReached {
            max_clients: transport.max_clients(),
        });
    }

    let offer =
        SdpOffer::from_sdp_string(&offer.sdp).map_err(|err| SdpHttpHookError::InvalidOffer {
            message: err.to_string(),
        })?;

    let mut rtc = Rtc::builder().build(Instant::now());
    let candidate = Candidate::host(config.local_candidate_addr, Protocol::Udp)?;
    rtc.add_local_candidate(candidate);

    let answer = rtc.sdp_api().accept_offer(offer)?;
    transport.add_peer(client_id, rtc);

    Ok(SdpHttpAnswerResponse {
        client_id,
        sdp: answer.to_sdp_string(),
    })
}

#[cfg(feature = "axum")]
pub fn accept_offer_axum_json(
    transport: &mut WebRtcNetcodeServerTransport,
    client_id: ClientId,
    offer: SdpHttpOfferRequest,
    config: SdpHttpHookConfig,
) -> Result<axum::Json<SdpHttpAnswerResponse>, SdpHttpHookError> {
    let response = accept_offer_and_add_peer(transport, client_id, offer, config)?;
    Ok(axum::Json(response))
}

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for SdpHttpHookError {
    fn into_response(self) -> axum::response::Response {
        use axum::{Json, http::StatusCode};

        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }

        let status = match self {
            SdpHttpHookError::DuplicateClientId { .. } => StatusCode::CONFLICT,
            SdpHttpHookError::CapacityReached { .. } => StatusCode::SERVICE_UNAVAILABLE,
            SdpHttpHookError::InvalidOffer { .. } => StatusCode::BAD_REQUEST,
            SdpHttpHookError::Ice(_) | SdpHttpHookError::Rtc(_) => StatusCode::UNPROCESSABLE_ENTITY,
        };

        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{net::UdpSocket, time::Duration};

    use renetcode::{ServerAuthentication, ServerConfig};

    use super::{
        SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest, accept_offer_and_add_peer,
    };
    use crate::WebRtcNetcodeServerTransport;

    #[test]
    fn invalid_offer_is_rejected() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("socket");
        let addr = socket.local_addr().expect("addr");
        let config = ServerConfig {
            current_time: Duration::ZERO,
            max_clients: 8,
            protocol_id: 7,
            public_addresses: vec![addr],
            authentication: ServerAuthentication::Unsecure,
        };
        let mut transport = WebRtcNetcodeServerTransport::new(config, socket).expect("transport");

        let result = accept_offer_and_add_peer(
            &mut transport,
            1,
            SdpHttpOfferRequest {
                sdp: "not sdp".to_string(),
                session_token: None,
            },
            SdpHttpHookConfig::new(addr),
        );

        assert!(matches!(result, Err(SdpHttpHookError::InvalidOffer { .. })));
    }

    #[test]
    fn duplicate_client_is_rejected() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("socket");
        let addr = socket.local_addr().expect("addr");
        let config = ServerConfig {
            current_time: Duration::ZERO,
            max_clients: 8,
            protocol_id: 7,
            public_addresses: vec![addr],
            authentication: ServerAuthentication::Unsecure,
        };
        let mut transport = WebRtcNetcodeServerTransport::new(config, socket).expect("transport");

        // pre-register id
        let rtc = str0m::Rtc::builder().build(std::time::Instant::now());
        transport.add_peer(42, rtc);

        let result = accept_offer_and_add_peer(
            &mut transport,
            42,
            SdpHttpOfferRequest {
                sdp: "v=0".to_string(),
                session_token: None,
            },
            SdpHttpHookConfig::new(addr),
        );

        assert!(matches!(
            result,
            Err(SdpHttpHookError::DuplicateClientId { client_id: 42 })
        ));
    }

    #[test]
    fn pending_peers_use_capacity_before_parsing_an_offer() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let mut transport = WebRtcNetcodeServerTransport::new(
            ServerConfig {
                current_time: Duration::ZERO,
                max_clients: 1,
                protocol_id: 7,
                public_addresses: vec![addr],
                authentication: ServerAuthentication::Unsecure,
            },
            socket,
        )
        .unwrap();
        transport.add_peer(1, str0m::Rtc::new(std::time::Instant::now()));
        assert_eq!(transport.connected_clients(), 0);
        let result = accept_offer_and_add_peer(
            &mut transport,
            2,
            SdpHttpOfferRequest {
                sdp: "not even parsed at capacity".into(),
                session_token: None,
            },
            SdpHttpHookConfig::new(addr),
        );
        assert!(matches!(
            result,
            Err(SdpHttpHookError::CapacityReached { max_clients: 1 })
        ));
        assert!(transport.peer(2).is_none());
        transport.remove_peer(1);
        let result = accept_offer_and_add_peer(
            &mut transport,
            2,
            SdpHttpOfferRequest {
                sdp: "now parsed".into(),
                session_token: None,
            },
            SdpHttpHookConfig::new(addr),
        );
        assert!(matches!(result, Err(SdpHttpHookError::InvalidOffer { .. })));
    }
}
