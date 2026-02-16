use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use renet_server::{
    MonotonicClientIdAllocator, SdpHttpAnswerResponse, SdpHttpHookConfig, SdpHttpHookError, SdpHttpOfferRequest,
    SessionCreateResponse, accept_offer_axum_json,
};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::SharedNet;

#[derive(Clone)]
pub(super) struct HttpTlsConfig {
    pub(super) cert_path: PathBuf,
    pub(super) key_path: PathBuf,
}

#[derive(Clone)]
pub(super) struct AppState {
    allocator: Arc<MonotonicClientIdAllocator>,
    net: SharedNet,
    public_udp_addr: SocketAddr,
    public_http_base: String,
    webrtc_candidate_addr: SocketAddr,
}

impl AppState {
    pub(super) fn new(
        allocator: Arc<MonotonicClientIdAllocator>,
        net: SharedNet,
        public_udp_addr: SocketAddr,
        public_http_base: String,
        webrtc_candidate_addr: SocketAddr,
    ) -> Self {
        Self {
            allocator,
            net,
            public_udp_addr,
            public_http_base,
            webrtc_candidate_addr,
        }
    }
}

#[derive(Debug)]
enum ApiError {
    UnknownSession { client_id: u64 },
    Hook(SdpHttpHookError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::UnknownSession { client_id } => {
                let payload = serde_json::json!({
                    "error": format!("unknown or expired session id: {client_id}")
                });
                (axum::http::StatusCode::NOT_FOUND, Json(payload)).into_response()
            }
            ApiError::Hook(err) => err.into_response(),
        }
    }
}

pub(super) fn spawn_http_server_thread(
    state: AppState,
    http_bind: SocketAddr,
    client_dist: PathBuf,
    http_tls: Option<HttpTlsConfig>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("axum-http".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("failed to create dedicated tokio runtime for HTTP server: {err}");
                    return;
                }
            };

            runtime.block_on(async move {
                let app = Router::new()
                    .route("/healthz", get(healthz))
                    .route("/api/session/new", post(session_new))
                    .route("/api/webrtc/offer/{client_id}", post(webrtc_offer))
                    .fallback_service(ServeDir::new(client_dist))
                    .layer(TraceLayer::new_for_http())
                    .with_state(state);

                if let Some(tls) = http_tls {
                    let tls_config =
                        match RustlsConfig::from_pem_file(tls.cert_path.clone(), tls.key_path.clone()).await {
                            Ok(config) => config,
                            Err(err) => {
                                log::error!(
                                    "failed to load TLS cert/key cert_path={} key_path={}: {err}",
                                    tls.cert_path.display(),
                                    tls.key_path.display()
                                );
                                return;
                            }
                        };

                    log::info!("hybrid server listening on https://{http_bind}");
                    if let Err(err) = axum_server::bind_rustls(http_bind, tls_config)
                        .serve(app.into_make_service())
                        .await
                    {
                        log::error!("HTTPS server exited with error: {err}");
                    }
                } else {
                    let listener = match tokio::net::TcpListener::bind(http_bind).await {
                        Ok(listener) => listener,
                        Err(err) => {
                            log::error!("failed to bind HTTP listener on {http_bind}: {err}");
                            return;
                        }
                    };

                    log::info!("hybrid server listening on http://{http_bind}");
                    if let Err(err) = axum::serve(listener, app).await {
                        log::error!("HTTP server exited with error: {err}");
                    }
                }
            });
        })
        .expect("failed to spawn dedicated HTTP thread")
}

async fn healthz() -> &'static str {
    "ok"
}

async fn session_new(State(state): State<AppState>) -> Json<SessionCreateResponse> {
    let client_id = state.allocator.next();
    let _ = state.net.with_sessions(|sessions| sessions.issue(client_id));

    let response = SessionCreateResponse {
        client_id,
        udp_addr: state.public_udp_addr.to_string(),
        webrtc_addr: state.webrtc_candidate_addr.to_string(),
        webrtc_offer_url: format!("{}/api/webrtc/offer/{client_id}", state.public_http_base),
    };

    log::info!(
        "issued session client_id={} udp_addr={} webrtc_addr={} webrtc_offer_url={}",
        response.client_id,
        response.udp_addr,
        response.webrtc_addr,
        response.webrtc_offer_url
    );

    Json(response)
}

async fn webrtc_offer(
    State(state): State<AppState>,
    Path(client_id): Path<u64>,
    Json(offer): Json<SdpHttpOfferRequest>,
) -> Result<Json<SdpHttpAnswerResponse>, ApiError> {
    log::debug!(
        "received SDP offer client_id={} sdp_bytes={}",
        client_id,
        offer.sdp.len()
    );

    let known_session = state
        .net
        .with_sessions(|sessions| sessions.is_pending(client_id))
        .unwrap_or(false);

    if !known_session {
        log::warn!("rejecting offer for unknown/expired client_id={client_id}");
        return Err(ApiError::UnknownSession { client_id });
    }

    let answer = state
        .net
        .with_transport(|transport| {
            accept_offer_axum_json(
                transport.webrtc_mut(),
                client_id,
                offer,
                SdpHttpHookConfig {
                    local_candidate_addr: state.webrtc_candidate_addr,
                },
            )
        })
        .ok_or_else(|| {
            log::error!("failed to lock transport state for client_id={client_id}");
            ApiError::UnknownSession { client_id }
        })?
        .map_err(ApiError::Hook)?;

    log::info!("accepted SDP offer for client_id={client_id}");
    Ok(answer)
}
