use axum::{
    extract::{Json, State, ConnectInfo},
    http::{StatusCode, HeaderMap},
    routing::{post, get},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, env, net::SocketAddr, time::SystemTime};
use std::fs;
use std::path::Path;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use std::sync::atomic::{AtomicUsize, Ordering};
use interceptor::registry::Registry;
// use tower_http::cors::{Any, CorsLayer}; // Unused in library, used in main.rs/server.rs
use bytes::Bytes;
use uuid::Uuid;
use tracing::{info, error, warn, debug};

use tokio::net::UdpSocket;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, 
        media_engine::MediaEngine,
        setting_engine::SettingEngine,
        APIBuilder,
        API,
    },
    data_channel::RTCDataChannel,
    ice_transport::{
        ice_candidate_type::RTCIceCandidateType,
        ice_server::RTCIceServer,
        ice_gathering_state::RTCIceGatheringState,
        ice_gatherer_state::RTCIceGathererState,
    },
    ice::{
        mdns::MulticastDnsMode,
        udp_network::{EphemeralUDP, UDPNetwork},
    },
    peer_connection::{
        configuration::RTCConfiguration, 
        peer_connection_state::RTCPeerConnectionState,
        sdp::{session_description::RTCSessionDescription, sdp_type::RTCSdpType},
        RTCPeerConnection,
    },
};

pub const DEFAULT_MAX_CONNECTIONS: usize = 500;
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 10;
pub const DEFAULT_ICE_GATHERING_TIMEOUT_SECS: u64 = 2;
pub const DEFAULT_OVERALL_REQUEST_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_PERIODIC_CLEANUP_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_STALE_CONNECTION_AGE_SECS: u64 = 120;
pub const DEFAULT_PORT: &str = "8080";
pub const DEFAULT_STUN_PORT: &str = "3478";
pub const DEFAULT_STUN_URL: &str = "auto";

pub const MAX_MESSAGE_SIZE: usize = 64 * 1024;
pub const MAX_DATA_CHANNELS_PER_PC: usize = 10;
pub const MAX_SDP_SIZE: usize = 64 * 1024;

pub const CONFIG_FILE: &str = ".env";
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# WebRTC UDP Test Server Configuration
# All values have sensible defaults

# Mode: 'web' (public/strict) or 'self' (self-hosted/flexible)
PLATFORM_MODE=self

PORT=8080
STUN_PORT=3478

# STUN URL: auto (detect local IP), stun:host:port, or none
STUN_URL=auto

# NAT 1-to-1 Mapping (for Docker/Cloud)
#NAT_1TO1_IP=

# Connection Limits
MAX_CONNECTIONS=500
MAX_CONNECTIONS_PER_IP=10

# ICE Port Range (UDP)
#ICE_PORT_MIN=
#ICE_PORT_MAX=

# Timeouts (seconds)
ICE_GATHERING_TIMEOUT_SECS=5
OVERALL_REQUEST_TIMEOUT_SECS=30
STALE_CONNECTION_AGE_SECS=120
PERIODIC_CLEANUP_INTERVAL_SECS=5

# Logging: trace, debug, info, warn, error
RUST_LOG=info
"#;

#[derive(Debug, Clone, PartialEq)]
pub enum PlatformMode {
    Web,
    SelfHosted,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub port: String,
    pub stun_port: String,
    pub stun_url: String,
    pub stun_enabled: bool,
    pub platform_mode: PlatformMode,
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    pub ice_gathering_timeout: Duration,
    pub overall_request_timeout: Duration,
    pub periodic_cleanup_interval: Duration,
    pub stale_connection_age: Duration,
    pub nat_1to1_ip: Option<String>,
    pub ice_port_range: Option<(u16, u16)>,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, String> {
        let port = env::var("PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
        let stun_port = env::var("STUN_PORT").unwrap_or_else(|_| DEFAULT_STUN_PORT.to_string());
        let stun_url_raw = env::var("STUN_URL").unwrap_or_else(|_| DEFAULT_STUN_URL.to_string());
        
        // Parse STUN configuration
        let (stun_enabled, stun_url) = if stun_url_raw.to_lowercase() == "none" {
            // STUN disabled - LAN-only mode
            (false, String::new())
        } else if stun_url_raw.to_lowercase() == "auto" {
            // Auto mode: Server uses localhost for its own peer connection
            // Clients will auto-detect STUN URL from the server hostname they use to connect
            let server_stun_url = format!("stun:127.0.0.1:{}", stun_port);
            info!("STUN auto mode: Server will listen on all interfaces (0.0.0.0:{})", stun_port);
            info!("Server's peer connection will use: {}", server_stun_url);
            info!("Clients will auto-detect STUN URL from the server hostname they use to connect");
            (true, server_stun_url)
        } else {
            // Explicit STUN URL provided
            (true, stun_url_raw)
        };

        // Parse platform mode
        let platform_mode = match env::var("PLATFORM_MODE")
            .unwrap_or_else(|_| "self".to_string())
            .to_lowercase()
            .as_str()
        {
            "web" => PlatformMode::Web,
            "self" => PlatformMode::SelfHosted,
            other => {
                warn!("Invalid PLATFORM_MODE '{}', defaulting to 'self'", other);
                PlatformMode::SelfHosted
            }
        };
        
        let max_connections = env::var("MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);
            
        let max_connections_per_ip = env::var("MAX_CONNECTIONS_PER_IP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_IP);
            
        let ice_gathering_timeout_secs = env::var("ICE_GATHERING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_ICE_GATHERING_TIMEOUT_SECS);
            
        let overall_request_timeout_secs = env::var("OVERALL_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_OVERALL_REQUEST_TIMEOUT_SECS);
            
        let periodic_cleanup_interval_secs = env::var("PERIODIC_CLEANUP_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PERIODIC_CLEANUP_INTERVAL_SECS);
            
        let stale_connection_age_secs = env::var("STALE_CONNECTION_AGE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_STALE_CONNECTION_AGE_SECS);

        let nat_1to1_ip = env::var("NAT_1TO1_IP").ok().filter(|s| !s.is_empty())
            .or_else(|| {
                let ip = detect_lan_ip();
                if let Some(ref detected) = ip {
                    info!("NAT_1TO1_IP not set, auto-detected IP: {}", detected);
                }
                ip
            });

        let ice_port_min = env::var("ICE_PORT_MIN").ok().filter(|s| !s.is_empty())
            .and_then(|v| v.parse::<u16>().ok());
        let ice_port_max = env::var("ICE_PORT_MAX").ok().filter(|s| !s.is_empty())
            .and_then(|v| v.parse::<u16>().ok());

        let ice_port_range = match (ice_port_min, ice_port_max) {
            (Some(min), Some(max)) => {
                if min > max {
                    return Err(format!("ICE_PORT_MIN ({}) cannot be greater than ICE_PORT_MAX ({})", min, max));
                }
                if min == 0 || max == 0 {
                    return Err("ICE ports must be greater than 0".to_string());
                }
                Some((min, max))
            },
            (Some(_), None) => {
                warn!("ICE_PORT_MIN provided without ICE_PORT_MAX. Port range restriction will be disabled.");
                None
            },
            (None, Some(_)) => {
                warn!("ICE_PORT_MAX provided without ICE_PORT_MIN. Port range restriction will be disabled.");
                None
            },
            (None, None) => None,
        };

        // Validation
        
        // Validate port
        if let Err(e) = port.parse::<u16>() {
            return Err(format!("Invalid PORT '{}': {}", port, e));
        }
        
        // Validate connection limits
        if max_connections == 0 {
            return Err("MAX_CONNECTIONS must be greater than 0".to_string());
        }
        if max_connections > 100000 {
            warn!("MAX_CONNECTIONS ({}) is very high, ensure server has adequate resources", max_connections);
        }
        
        if max_connections_per_ip == 0 {
            return Err("MAX_CONNECTIONS_PER_IP must be greater than 0".to_string());
        }
        if max_connections_per_ip > max_connections {
            warn!("MAX_CONNECTIONS_PER_IP ({}) exceeds MAX_CONNECTIONS ({}), capping to {}", 
                max_connections_per_ip, max_connections, max_connections);
        }
        
        // Validate timeouts
        if ice_gathering_timeout_secs == 0 {
            return Err("ICE_GATHERING_TIMEOUT_SECS must be greater than 0".to_string());
        }
        if ice_gathering_timeout_secs > 60 {
            warn!("ICE_GATHERING_TIMEOUT_SECS ({}) is very high, may cause slow responses", ice_gathering_timeout_secs);
        }
        
        if overall_request_timeout_secs == 0 {
            return Err("OVERALL_REQUEST_TIMEOUT_SECS must be greater than 0".to_string());
        }
        if overall_request_timeout_secs <= ice_gathering_timeout_secs {
            return Err(format!(
                "OVERALL_REQUEST_TIMEOUT_SECS ({}) must be greater than ICE_GATHERING_TIMEOUT_SECS ({})",
                overall_request_timeout_secs, ice_gathering_timeout_secs
            ));
        }
        
        if periodic_cleanup_interval_secs == 0 {
            return Err("PERIODIC_CLEANUP_INTERVAL_SECS must be greater than 0".to_string());
        }
        
        if stale_connection_age_secs == 0 {
            return Err("STALE_CONNECTION_AGE_SECS must be greater than 0".to_string());
        }
        if stale_connection_age_secs < 30 {
            warn!("STALE_CONNECTION_AGE_SECS ({}) is very low, may prematurely close active connections", stale_connection_age_secs);
        }
        
        // Validate STUN/TURN URL format (only if STUN is enabled)
        if stun_enabled && !stun_url.starts_with("stun:") && !stun_url.starts_with("turn:") && !stun_url.starts_with("turns:") {
            return Err(format!(
                "Invalid STUN/TURN URL format '{}'. Must start with 'stun:', 'turn:', or 'turns:'",
                stun_url
            ));
        }
        
        // Validate STUN port
        if let Err(e) = stun_port.parse::<u16>() {
            return Err(format!("Invalid STUN_PORT '{}': {}", stun_port, e));
        }
        Ok(Self {
            port,
            stun_port,
            stun_url,
            stun_enabled,
            platform_mode,
            max_connections,
            max_connections_per_ip: max_connections_per_ip.min(max_connections),
            ice_gathering_timeout: Duration::from_secs(ice_gathering_timeout_secs),
            overall_request_timeout: Duration::from_secs(overall_request_timeout_secs),
            periodic_cleanup_interval: Duration::from_secs(periodic_cleanup_interval_secs),
            stale_connection_age: Duration::from_secs(stale_connection_age_secs),
            nat_1to1_ip,
            ice_port_range,
        })
    }

    /// Log all configuration values at startup
    pub fn log(&self) {
        info!("=== Server Configuration ===");
        info!("Platform Mode: {:?} ({})",
            self.platform_mode,
            match self.platform_mode {
                PlatformMode::Web => "Public web service with stricter limits",
                PlatformMode::SelfHosted => "Self-hosted with infinite test support",
            }
        );
        info!("HTTP Port: {}", self.port);
        if self.stun_enabled {
            info!("STUN Server: Enabled on port {} (UDP)", self.stun_port);
            info!("STUN URL for clients: {}", self.stun_url);
        } else {
            info!("STUN Server: Disabled (LAN-only mode)");
        }
        info!("Max Connections: {}", self.max_connections);
        info!("Max Connections Per IP: {}", self.max_connections_per_ip);
        info!("ICE Gathering Timeout: {:?}", self.ice_gathering_timeout);
        info!("Overall Request Timeout: {:?}", self.overall_request_timeout);
        info!("Periodic Cleanup Interval: {:?}", self.periodic_cleanup_interval);
        info!("Stale Connection Age: {:?}", self.stale_connection_age);
        if let Some(ip) = &self.nat_1to1_ip {
            info!("NAT 1-to-1 Mapping: {}", ip);
        }
        if let Some((min, max)) = self.ice_port_range {
            info!("ICE Port Range: {} - {} (UDP)", min, max);
        } else {
            info!("ICE Port Range: Unlimited (OS-managed)");
        }
        info!("============================");
    }
}

pub fn ensure_config_file(config_path: &Path) {
    if !config_path.exists() {
        info!("Config file '{}' not found, creating with default settings...", config_path.display());
        
        match fs::write(config_path, DEFAULT_CONFIG_TEMPLATE) {
            Ok(_) => {
                info!("Created default config file: {}", config_path.display());
                
                // Set restrictive file permissions (Unix/Linux only)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = fs::metadata(config_path) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(0o600); // Owner read/write only
                        if let Err(e) = fs::set_permissions(config_path, perms) {
                            warn!("Failed to set restrictive permissions on {}: {}", config_path.display(), e);
                        } else {
                            info!("Set file permissions to 600 (owner read/write only)");
                        }
                    }
                }
                
                info!("Please review and adjust settings for your environment.");
            },
            Err(e) => {
                warn!("Failed to create default config file: {}. Using hardcoded defaults.", e);
            }
        }
    }
}



pub fn detect_lan_ip() -> Option<String> {
    // We connect to a public IP (doesn't actually send any data)
    // to see which local interface the system would use to reach the internet
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => {
            if socket.connect("8.8.8.8:80").is_ok() {
                if let Ok(local_addr) = socket.local_addr() {
                    let ip = local_addr.ip().to_string();
                    debug!("Auto-detected LAN/Gateway IP: {}", ip);
                    return Some(ip);
                }
            }
            debug!("Failed to detect LAN IP via UDP trick (8.8.8.8 unreachable?)");
            None
        },
        Err(e) => {
            debug!("Failed to bind socket for LAN IP detection: {}", e);
            None
        }
    }
}

pub fn build_webrtc_api(config: &ServerConfig) -> API {
    let mut m = MediaEngine::default();
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)
        .expect("Failed to register default interceptors");
    
    // Configure SettingEngine to disable mDNS
    let mut s = SettingEngine::default();
    s.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);

    // NAT 1-to-1 Mapping logic:
    // 1. Check explicit config (from NAT_1TO1_IP env var)
    // 2. Fallback to runtime detection (for Docker/WSL/cloud environments)
    // 3. Else disabled
    let nat_ip = match config.nat_1to1_ip.as_deref() {
        Some("auto") => {
            let ip = detect_lan_ip();
            if ip.is_none() {
                warn!("NAT 1-to-1 mapping explicitly requested ('auto'), but IP detection failed. Connection issues may occur in NAT environments.");
            }
            ip
        }
        Some(ip) => Some(ip.to_string()),
        None => None,
    };

    if let Some(ip) = nat_ip {
        info!("Configuring WebRTC NAT 1-to-1 mapping for IP: {}", ip);
        s.set_nat_1to1_ips(vec![ip], RTCIceCandidateType::Host);
        s.set_ip_filter(Box::new(|addr: std::net::IpAddr| addr.is_ipv4()));
    }

    // Apply ICE port range restriction if configured
    if let Some((min, max)) = config.ice_port_range {
        let mut ephemeral_udp = EphemeralUDP::default();
        match ephemeral_udp.set_ports(min, max) {
            Ok(_) => {
                s.set_udp_network(UDPNetwork::Ephemeral(ephemeral_udp));
            }
            Err(e) => {
                error!("Failed to set ephemeral UDP port range {}-{}: {}", min, max, e);
            }
        }
    }
    
    APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(s)
        .build()
}



#[derive(Deserialize)]
pub struct OfferRequest {
    pub sdp: RTCSessionDescription,
}

#[derive(Serialize)]
pub struct AnswerResponse {
    pub sdp: RTCSessionDescription,
    pub pc_id: String,
}


#[derive(Deserialize, Serialize, Debug)]
pub struct PingMessage {
    pub seq: usize,
    pub timestamp: u64,
    #[serde(default)]
    pub s_rx: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub total_connections: usize,
    pub max_connections: usize,
    pub uptime_seconds: u64,
}



pub struct ConnectionMetadata {
    pub peer_connection: Option<Arc<RTCPeerConnection>>, // None = placeholder during initialization
    pub created_at: SystemTime,
    pub client_ip: String,
    pub data_channel_count: Arc<AtomicUsize>, // Track number of data channels (DoS protection)
}

pub struct AppState {
    pub peer_connections: Arc<RwLock<HashMap<String, ConnectionMetadata>>>,
    pub config: ServerConfig,
    pub webrtc_api: Arc<API>,
    pub start_time: SystemTime,
}



pub fn setup_routes(router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    router
        .route("/webrtc/offer", post(handle_offer))
        .route("/health", get(health_check))
}



pub async fn health_check(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let pcs = state.peer_connections.read().await;
    let uptime = state.start_time.elapsed().unwrap_or_default().as_secs();
    
    Json(HealthResponse {
        status: "healthy".to_string(),
        total_connections: pcs.len(),
        max_connections: state.config.max_connections,
        uptime_seconds: uptime,
    })
}



pub async fn periodic_cleanup(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(state.config.periodic_cleanup_interval);
    loop {
        interval.tick().await;
        
        let mut pcs = state.peer_connections.write().await;
        let mut to_remove = Vec::new();
        let now = SystemTime::now();
        
        for (id, metadata) in pcs.iter() {
            let age = now.duration_since(metadata.created_at).unwrap_or_default();
            
            match &metadata.peer_connection {
                Some(pc) => {
                    let pc_state = pc.connection_state();

                    // Platform-specific cleanup logic
                    let should_remove = match state.config.platform_mode {
                        PlatformMode::Web => {
                            matches!(pc_state,
                                RTCPeerConnectionState::Disconnected |
                                RTCPeerConnectionState::Failed |
                                RTCPeerConnectionState::Closed
                            ) || age > state.config.stale_connection_age
                        },
                        PlatformMode::SelfHosted => {
                            matches!(pc_state,
                                RTCPeerConnectionState::Disconnected |
                                RTCPeerConnectionState::Failed |
                                RTCPeerConnectionState::Closed
                            ) || (age > state.config.stale_connection_age
                                && pc_state != RTCPeerConnectionState::Connected)
                        }
                    };

                    if should_remove {
                        to_remove.push(id.clone());
                    }
                },
                None => {
                    if age.as_secs() > 30 {
                        to_remove.push(id.clone());
                    }
                }
            }
        }
        
        if !to_remove.is_empty() {
            info!("Periodic cleanup removing {} stale connections", to_remove.len());
            
            let removed: Vec<(String, ConnectionMetadata)> = to_remove
                .into_iter()
                .filter_map(|id| pcs.remove(&id).map(|m| (id, m)))
                .collect();
            
            drop(pcs);
            
            for (id, metadata) in removed {
                if let Some(pc) = metadata.peer_connection {
                    tokio::spawn(async move {
                        if let Err(e) = pc.close().await {
                            warn!("Error closing peer connection during cleanup: {}", e);
                        }
                    });
                }
                info!("Cleaned up stale connection: {}", id);
            }
        }
    }
}

// ============================================================================
// WEBRTC OFFER HANDLER
// ============================================================================

/// POST /webrtc/offer - Handle WebRTC offer from client
pub async fn handle_offer(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(offer): Json<OfferRequest>,
) -> Result<Json<AnswerResponse>, (StatusCode, String)> {
    let client_ip = if addr.ip().is_loopback() {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| addr.ip().to_string())
    } else {
        addr.ip().to_string()
    };
    
    let timeout_duration = state.config.overall_request_timeout;
    match timeout(timeout_duration, handle_offer_internal(state, offer, client_ip)).await {
        Ok(result) => result,
        Err(_) => {
            error!("Overall request timeout exceeded");
            Err((StatusCode::REQUEST_TIMEOUT, "Request processing timed out".to_string()))
        }
    }
}

async fn handle_offer_internal(
    state: Arc<AppState>,
    offer: OfferRequest,
    client_ip: String,
) -> Result<Json<AnswerResponse>, (StatusCode, String)> {
    if offer.sdp.sdp.len() > MAX_SDP_SIZE {
        warn!("SDP too large: {} bytes from {}", offer.sdp.sdp.len(), client_ip);
        return Err((
            StatusCode::BAD_REQUEST,
            "SDP exceeds maximum size".to_string()
        ));
    }
    
    if offer.sdp.sdp_type != RTCSdpType::Offer {
        warn!("Received invalid SDP type: {:?}", offer.sdp.sdp_type);
        return Err((
            StatusCode::BAD_REQUEST, 
            "Invalid SDP type, expected 'offer'".to_string()
        ));
    }

    let pc_id = Uuid::new_v4().to_string();

    {
        let mut pcs = state.peer_connections.write().await;
        
        if pcs.len() >= state.config.max_connections {
            warn!("Connection limit reached ({}/{})", pcs.len(), state.config.max_connections);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE, 
                "Server at capacity, try again later".to_string()
            ));
        }

        let ip_connections = pcs.values()
            .filter(|m| m.client_ip == client_ip)
            .count();
        
        if ip_connections >= state.config.max_connections_per_ip {
            warn!("Per-IP connection limit reached for {} ({}/{})", 
                client_ip, ip_connections, state.config.max_connections_per_ip);
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "Too many connections from your IP".to_string()
            ));
        }

        pcs.insert(pc_id.clone(), ConnectionMetadata {
            peer_connection: None,
            created_at: SystemTime::now(),
            client_ip: client_ip.clone(),
            data_channel_count: Arc::new(AtomicUsize::new(0)),
        });
    }

    let peer_connection = match create_peer_connection(&state).await {
        Ok(pc) => pc,
        Err(e) => {
            state.peer_connections.write().await.remove(&pc_id);
            return Err(e);
        }
    };

    info!("New PeerConnection created: {} from IP: {}", pc_id, client_ip);
    
    let dc_count = {
        let mut pcs = state.peer_connections.write().await;
        let metadata = pcs.get_mut(&pc_id).ok_or_else(|| {
            error!("Connection metadata missing for {}", pc_id);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal state error".to_string())
        })?;
        
        metadata.peer_connection = Some(Arc::clone(&peer_connection));
        Arc::clone(&metadata.data_channel_count)
    };

    peer_connection.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
        let d_label = d.label().to_owned();
        let d_id = d.id();
        
        let current_count = dc_count.fetch_add(1, Ordering::SeqCst) + 1;
        if current_count > MAX_DATA_CHANNELS_PER_PC {
            error!("Data channel limit exceeded: {} > {}", current_count, MAX_DATA_CHANNELS_PER_PC);
            dc_count.fetch_sub(1, Ordering::SeqCst);
            return Box::pin(async {});
        }
        
        info!("New DataChannel: {} {} (total: {})", d_label, d_id, current_count);

        let rx_count = Arc::new(AtomicUsize::new(0));

        let dc_count_for_close = dc_count.clone();
        d.on_close(Box::new(move || {
            dc_count_for_close.fetch_sub(1, Ordering::SeqCst);
            Box::pin(async {})
        }));

        d.on_error(Box::new(move |e| {
            warn!("DataChannel error: {}", e);
            Box::pin(async {})
        }));

        Box::pin(async move {
            let d2 = d.clone();
            let rx_count_clone = rx_count.clone();
            
            d.on_message(Box::new(move |msg| {
                let d3 = d2.clone();
                let rx_count = rx_count_clone.clone();
                
                Box::pin(async move {
                    let data = msg.data;
                    
                    if data.len() > MAX_MESSAGE_SIZE {
                        warn!("Message too large: {} bytes, dropping", data.len());
                        return;
                    }
                    
                    if let Ok(mut ping) = serde_json::from_slice::<PingMessage>(&data) {
                        let count = rx_count.fetch_add(1, Ordering::SeqCst) + 1;
                        ping.s_rx = count;
                        
                        if let Ok(resp_json) = serde_json::to_string(&ping) {
                            let resp_data = Bytes::from(resp_json);
                            if let Err(err) = d3.send(&resp_data).await {
                                error!("Failed to send JSON response: {}", err);
                            }
                        }
                    } else {
                        let msg_data = Bytes::from(data.to_vec());
                        if let Err(err) = d3.send(&msg_data).await {
                            error!("Failed to echo raw data: {}", err);
                        }
                    }
                })
            }));

            d.on_open(Box::new(move || {
                info!("DataChannel '{}'-'{}' open", d_label, d_id);
                Box::pin(async {})
            }));
        })
    }));

    let state_clone = Arc::clone(&state);
    let pc_id_clone = pc_id.clone();
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("Peer Connection State ({}): {}", pc_id_clone, s);
        
        if matches!(s, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
            let state = Arc::clone(&state_clone);
            let id = pc_id_clone.clone();
            
            tokio::spawn(async move {
                state.peer_connections.write().await.remove(&id);
                info!("Cleaned up PeerConnection: {}", id);
            });
        }

        Box::pin(async {})
    }));

    if let Err(e) = peer_connection.set_remote_description(offer.sdp).await {
        error!("Failed to set remote description ({}): {}", pc_id, e);
        cleanup_connection(&state, &pc_id, &peer_connection).await;
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid offer SDP".to_string()
        ));
    }

    let answer = match peer_connection.create_answer(None).await {
        Ok(ans) => ans,
        Err(e) => {
            error!("Failed to create answer ({}): {}", pc_id, e);
            cleanup_connection(&state, &pc_id, &peer_connection).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create answer".to_string()
            ));
        }
    };
    
    if let Err(e) = peer_connection.set_local_description(answer).await {
        error!("Failed to set local description ({}): {}", pc_id, e);
        cleanup_connection(&state, &pc_id, &peer_connection).await;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to set local description".to_string()
        ));
    }

    let (ice_complete_tx, ice_complete_rx) = tokio::sync::oneshot::channel();
    let ice_tx = Arc::new(tokio::sync::Mutex::new(Some(ice_complete_tx)));
    
    let already_complete = peer_connection.ice_gathering_state() == RTCIceGatheringState::Complete;
    
    if already_complete {
        if let Some(tx) = ice_tx.lock().await.take() {
            let _ = tx.send(());
        }
    } else {
        let ice_tx_clone = ice_tx.clone();
        peer_connection.on_ice_gathering_state_change(Box::new(move |state: RTCIceGathererState| {
            if state == RTCIceGathererState::Complete {
                let ice_tx = ice_tx_clone.clone();
                tokio::spawn(async move {
                    if let Some(tx) = ice_tx.lock().await.take() {
                        let _ = tx.send(());
                    }
                });
            }
            Box::pin(async {})
        }));
        
        if peer_connection.ice_gathering_state() == RTCIceGatheringState::Complete {
            if let Some(tx) = ice_tx.lock().await.take() {
                let _ = tx.send(());
            }
        }
    }

    let ice_timeout = state.config.ice_gathering_timeout;
    match timeout(ice_timeout, ice_complete_rx).await {
        Ok(Ok(())) => {
            info!("ICE gathering complete for {}", pc_id);
        },
        Ok(Err(_)) => {
            warn!("ICE gathering channel closed for {}", pc_id);
            cleanup_connection(&state, &pc_id, &peer_connection).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "ICE gathering failed".to_string()
            ));
        },
        Err(_) => {
            warn!("ICE gathering timeout for {} but continuing with gathered candidates", pc_id);
            // Continue and return the SDP with whatever candidates we have instead of failing the connection.
        }
    }

    let final_answer = match peer_connection.local_description().await {
        Some(desc) => desc,
        None => {
            error!("Failed to get local description for {}", pc_id);
            cleanup_connection(&state, &pc_id, &peer_connection).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get local description".to_string()
            ));
        }
    };

    info!("Successfully created answer for {}", pc_id);
    Ok(Json(AnswerResponse {
        sdp: final_answer,
        pc_id: pc_id.clone()
    }))
}



pub async fn create_peer_connection(
    state: &AppState,
) -> Result<Arc<RTCPeerConnection>, (StatusCode, String)> {
    let config = RTCConfiguration {
        ice_servers: if state.config.stun_enabled && state.config.nat_1to1_ip.is_none() {
            vec![RTCIceServer {
                urls: vec![state.config.stun_url.clone()],
                ..Default::default()
            }]
        } else {
            vec![]
        },
        ..Default::default()
    };

    state.webrtc_api.new_peer_connection(config).await
        .map(Arc::new)
        .map_err(|e| {
            error!("Failed to create peer connection: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create connection".to_string())
        })
}



pub async fn run_stun_server(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind(addr).await?;
    info!("STUN server listening on {}", addr);
    
    let mut buf = vec![0u8; 1500];
    
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, peer_addr)) => {
                let data = &buf[..len];
                
                if len < 20 {
                    continue;
                }
                
                if data[4] != 0x21 || data[5] != 0x12 || data[6] != 0xA4 || data[7] != 0x42 {
                    continue;
                }
                
                let msg_type = u16::from_be_bytes([data[0], data[1]]);
                if msg_type != 0x0001 {
                    continue;
                }
                
                debug!("STUN Binding Request from {}", peer_addr);
                
                let mut response = vec![0u8; 32];
                response[0] = 0x01;
                response[1] = 0x01;
                response[2] = 0x00;
                response[3] = 0x0c;
                response[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
                response[8..20].copy_from_slice(&data[8..20]);
                response[20] = 0x00;
                response[21] = 0x20;
                response[22] = 0x00;
                response[23] = 0x08;
                response[24] = 0x00;
                response[25] = 0x01;
                
                let port = peer_addr.port();
                let xor_port = port ^ 0x2112;
                response[26..28].copy_from_slice(&xor_port.to_be_bytes());
                
                if let SocketAddr::V4(addr_v4) = peer_addr {
                    let ip_octets = addr_v4.ip().octets();
                    response[28] = ip_octets[0] ^ 0x21;
                    response[29] = ip_octets[1] ^ 0x12;
                    response[30] = ip_octets[2] ^ 0xA4;
                    response[31] = ip_octets[3] ^ 0x42;
                    
                    if let Err(e) = socket.send_to(&response, peer_addr).await {
                        warn!("Failed to send STUN response to {}: {}", peer_addr, e);
                    } else {
                        debug!("STUN response sent to {}", peer_addr);
                    }
                } else {
                    debug!("IPv6 STUN request from {}, not supported", peer_addr);
                }
            },
            Err(e) => {
                error!("STUN server socket error: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

pub async fn cleanup_connection(
    state: &Arc<AppState>,
    pc_id: &str,
    peer_connection: &Arc<RTCPeerConnection>,
) {
    state.peer_connections.write().await.remove(pc_id);
    
    if let Err(e) = peer_connection.close().await {
        warn!("Error closing peer connection during cleanup: {}", e);
    }
}
