use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    BootstrapAuthError, BootstrapError, BootstrapService, MixedServerTransport,
    MonotonicClientIdAllocator, SdpHttpHookConfig, SdpHttpOfferRequest, SessionAuthPolicy,
    SessionCreateResponse, SessionIdAllocator, UnsecureDevAuthPolicy,
};

pub struct BootstrapAxumState<A = MonotonicClientIdAllocator, P = UnsecureDevAuthPolicy>
where
    A: SessionIdAllocator,
    P: SessionAuthPolicy,
{
    pub bootstrap: Arc<BootstrapService<A, P>>,
    pub transport: Arc<Mutex<MixedServerTransport>>,
    pub hook_config: SdpHttpHookConfig,
}

impl<A, P> Clone for BootstrapAxumState<A, P>
where
    A: SessionIdAllocator,
    P: SessionAuthPolicy,
{
    fn clone(&self) -> Self {
        Self {
            bootstrap: Arc::clone(&self.bootstrap),
            transport: Arc::clone(&self.transport),
            hook_config: self.hook_config,
        }
    }
}

#[derive(Debug)]
enum ApiError {
    Bootstrap(BootstrapError),
    TransportLockPoisoned,
}

impl From<BootstrapError> for ApiError {
    fn from(value: BootstrapError) -> Self {
        Self::Bootstrap(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }

        match self {
            ApiError::Bootstrap(BootstrapError::UnknownSession { client_id }) => {
                let body = ErrorBody {
                    error: format!("unknown or expired session id: {client_id}"),
                };
                (StatusCode::NOT_FOUND, Json(body)).into_response()
            }
            ApiError::Bootstrap(BootstrapError::Auth(err)) => {
                let status = match err {
                    BootstrapAuthError::MissingToken { .. }
                    | BootstrapAuthError::InvalidToken { .. } => StatusCode::UNAUTHORIZED,
                    BootstrapAuthError::Message { .. } => StatusCode::FORBIDDEN,
                };

                let body = ErrorBody {
                    error: err.to_string(),
                };
                (status, Json(body)).into_response()
            }
            ApiError::Bootstrap(BootstrapError::Hook(err)) => err.into_response(),
            ApiError::Bootstrap(BootstrapError::SessionRegistryPoisoned)
            | ApiError::Bootstrap(BootstrapError::Clock(_))
            | ApiError::TransportLockPoisoned => {
                let body = ErrorBody {
                    error: "internal server error".to_string(),
                };
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            }
        }
    }
}

pub fn bootstrap_router<A, P>(state: BootstrapAxumState<A, P>) -> Router
where
    A: SessionIdAllocator + Send + Sync + 'static,
    P: SessionAuthPolicy + Send + Sync + 'static,
{
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/session/new", post(session_new::<A, P>))
        .route("/api/webrtc/offer/{client_id}", post(webrtc_offer::<A, P>))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn session_new<A, P>(
    State(state): State<BootstrapAxumState<A, P>>,
) -> Result<Json<SessionCreateResponse>, ApiError>
where
    A: SessionIdAllocator + Send + Sync + 'static,
    P: SessionAuthPolicy + Send + Sync + 'static,
{
    let session = state.bootstrap.create_session()?;
    Ok(Json(session))
}

async fn webrtc_offer<A, P>(
    State(state): State<BootstrapAxumState<A, P>>,
    Path(client_id): Path<u64>,
    Json(offer): Json<SdpHttpOfferRequest>,
) -> Result<Json<crate::SdpHttpAnswerResponse>, ApiError>
where
    A: SessionIdAllocator + Send + Sync + 'static,
    P: SessionAuthPolicy + Send + Sync + 'static,
{
    let mut transport = state
        .transport
        .lock()
        .map_err(|_| ApiError::TransportLockPoisoned)?;

    let answer = state.bootstrap.accept_offer(
        transport.webrtc_mut(),
        client_id,
        offer,
        state.hook_config,
    )?;

    Ok(Json(answer))
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::{BootstrapAxumState, bootstrap_router};
    use crate::{
        BootstrapConfig, BootstrapService, MixedTransportBuilder, MonotonicClientIdAllocator,
        SdpHttpHookConfig, UnsecureDevAuthPolicy,
    };

    fn test_app() -> axum::Router {
        let transport = Arc::new(Mutex::new(
            MixedTransportBuilder::new(7)
                .udp_bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .webrtc_bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .build()
                .expect("transport"),
        ));

        let webrtc_addr = {
            let transport = transport.lock().expect("transport lock");
            transport
                .webrtc()
                .addresses()
                .first()
                .copied()
                .expect("webrtc address")
        };

        let bootstrap = Arc::new(BootstrapService::new(
            BootstrapConfig {
                session_ttl: Duration::from_secs(60),
                public_udp_addr: SocketAddr::from(([127, 0, 0, 1], 5000)),
                public_webrtc_addr: webrtc_addr,
                public_http_base: "http://127.0.0.1:8080".to_string(),
            },
            MonotonicClientIdAllocator::new(1),
            UnsecureDevAuthPolicy,
        ));

        let state = BootstrapAxumState {
            bootstrap,
            transport,
            hook_config: SdpHttpHookConfig::new(webrtc_addr),
        };

        bootstrap_router(state)
    }

    #[tokio::test]
    async fn session_new_returns_payload_with_optional_token() {
        let app = test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session/new")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");

        assert!(
            json.get("client_id")
                .and_then(|value| value.as_u64())
                .is_some()
        );
        assert_eq!(json.get("session_token"), Some(&serde_json::Value::Null));
    }

    #[tokio::test]
    async fn unknown_offer_session_returns_not_found() {
        let app = test_app();
        let offer = serde_json::json!({ "sdp": "v=0" });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/webrtc/offer/999")
                    .header("content-type", "application/json")
                    .body(Body::from(offer.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_offer_returns_bad_request() {
        let app = test_app();

        let session_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session/new")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("session response");
        assert_eq!(session_response.status(), StatusCode::OK);
        let session_body = to_bytes(session_response.into_body(), usize::MAX)
            .await
            .expect("session body");
        let session_json: serde_json::Value =
            serde_json::from_slice(&session_body).expect("session json");
        let client_id = session_json["client_id"].as_u64().expect("client id");

        let offer = serde_json::json!({ "sdp": "not sdp" });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/webrtc/offer/{client_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(offer.to_string()))
                    .expect("request"),
            )
            .await
            .expect("offer response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
