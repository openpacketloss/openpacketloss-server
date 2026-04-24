use std::sync::Arc;
use std::time::SystemTime;
use std::collections::HashMap;
use tokio::sync::RwLock;
use tracing::{info, error, warn};
use axum::Router;
use openpacketloss_server as lib;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    lib::ensure_config_file(std::path::Path::new(".env"));

    if let Err(e) = dotenvy::dotenv() {
        warn!("Could not load .env file: {}. Using environment variables or defaults.", e);
    } else {
        info!("Loaded configuration from .env file");
    }

    let config = match lib::ServerConfig::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Configuration error: {}", e);
            error!("Please fix your .env file or environment variables and try again.");
            std::process::exit(1);
        }
    };

    
    config.log();

    info!("Initializing WebRTC service...");
    let webrtc_api = Arc::new(lib::build_webrtc_api(&config));

    let addr_str = format!("0.0.0.0:{}", config.port);
    info!("Starting WebRTC UDP test server on {}", addr_str);

    let shared_state = Arc::new(lib::AppState {
        peer_connections: Arc::new(RwLock::new(HashMap::new())),
        config: config.clone(),
        webrtc_api,
        start_time: SystemTime::now(),
    });

    let cleanup_state = Arc::clone(&shared_state);
    tokio::spawn(async move {
        lib::periodic_cleanup(cleanup_state).await;
    });

    if config.stun_enabled {
        let stun_addr = format!("0.0.0.0:{}", config.stun_port);
        info!("STUN server starting on {}", stun_addr);
        tokio::spawn(async move {
            if let Err(e) = lib::run_stun_server(&stun_addr).await {
                error!("STUN server failed: {}", e);
            }
        });
    }

    let app = lib::setup_routes(Router::new())
        .with_state(shared_state);

    let listener = tokio::net::TcpListener::bind(&addr_str)
        .await
        .expect("Failed to bind server address");

    info!("Server listening on {}", addr_str);
    
    let shutdown_signal = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C signal handler");
        info!("Shutdown signal received, closing connections...");
    };

    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal)
        .await
        .unwrap();
}