use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum_server::tls_rustls::RustlsConfig;
use renet_cross::{
    BootstrapAxumState, DefaultBootstrapService, MixedServerTransport, SdpHttpHookConfig,
    bootstrap_router,
};
use tower_http::{services::ServeDir, trace::TraceLayer};

#[derive(Clone)]
pub(super) struct HttpTlsConfig {
    pub(super) cert_path: PathBuf,
    pub(super) key_path: PathBuf,
}

#[derive(Clone)]
pub(super) struct AppState {
    bootstrap: Arc<DefaultBootstrapService>,
    transport: Arc<Mutex<MixedServerTransport>>,
    webrtc_candidate_addr: SocketAddr,
}

impl AppState {
    pub(super) fn new(
        bootstrap: Arc<DefaultBootstrapService>,
        transport: Arc<Mutex<MixedServerTransport>>,
        webrtc_candidate_addr: SocketAddr,
    ) -> Self {
        Self {
            bootstrap,
            transport,
            webrtc_candidate_addr,
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
                let bootstrap_state = BootstrapAxumState {
                    bootstrap: state.bootstrap,
                    transport: state.transport,
                    hook_config: SdpHttpHookConfig {
                        local_candidate_addr: state.webrtc_candidate_addr,
                    },
                };

                let app = bootstrap_router(bootstrap_state)
                    .fallback_service(ServeDir::new(client_dist))
                    .layer(TraceLayer::new_for_http());

                if let Some(tls) = http_tls {
                    let tls_config = match RustlsConfig::from_pem_file(
                        tls.cert_path.clone(),
                        tls.key_path.clone(),
                    )
                    .await
                    {
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
