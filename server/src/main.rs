//! Vloxximity Relay Server
//!
//! Handles room management, position tracking, and audio relay for Vloxximity voice chat.

mod gw2;
mod limits;
mod protocol;
mod rate_limit;
mod rooms;
mod session;
mod squad;
mod sweeper;
mod test_peer;

use axum::{
    extract::ws::WebSocketUpgrade, extract::State, response::IntoResponse, routing::get, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use rooms::RoomManager;
use session::handle_socket;
use squad::SquadRegistry;
use test_peer::TestPeerMode;

/// Server configuration
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub test_peer_mode: Option<TestPeerMode>,
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<RoomManager>,
    pub squads: Arc<SquadRegistry>,
    pub config: Arc<ServerConfig>,
    pub http: reqwest::Client,
    pub gw2_cache: gw2::Gw2Cache,
}

#[tokio::main]
async fn main() {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install default rustls CryptoProvider");

    info!("Vloxximity Relay Server starting...");

    let test_peer_mode = parse_test_peer_flag(std::env::args().skip(1));
    if let Some(mode) = test_peer_mode {
        info!("Test peer enabled: mode={:?}", mode);
    }

    // Create application state
    let config = ServerConfig { test_peer_mode };
    let http = reqwest::Client::builder()
        .user_agent("vloxximity-server/0.1")
        .build()
        .expect("reqwest client build");
    let state = AppState {
        rooms: Arc::new(RoomManager::new()),
        squads: Arc::new(SquadRegistry::new()),
        config: Arc::new(config),
        http,
        gw2_cache: gw2::new_cache(),
    };

    sweeper::spawn_sweeper(state.rooms.clone(), state.squads.clone());

    if let Some(mode) = state.config.test_peer_mode {
        test_peer::spawn_supervisor(state.rooms.clone(), mode);
    }

    // Build router
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state);

    // Run server
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    match (
        std::env::var("VLOXXIMITY_TLS_CERT").ok(),
        std::env::var("VLOXXIMITY_TLS_KEY").ok(),
    ) {
        (Some(cert), Some(key)) => {
            let tls = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .expect("loading TLS cert/key");
            info!("Listening on {} (TLS)", addr);
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        _ => {
            warn!("TLS disabled — set VLOXXIMITY_TLS_CERT and VLOXXIMITY_TLS_KEY for production");
            info!("Listening on {} (plaintext)", addr);
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        }
    }
}

const MAX_WS_MESSAGE_SIZE: usize = 64 * 1024;

/// WebSocket upgrade handler
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(MAX_WS_MESSAGE_SIZE)
        .max_frame_size(MAX_WS_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

/// Health check endpoint
async fn health_handler() -> &'static str {
    "OK"
}

/// Parse `--testpeer` / `--testpeer=<mode>` / `--testpeer <mode>` from CLI args.
/// Bare `--testpeer` defaults to orbit mode. Unknown modes fall back to orbit
/// with a warning.
fn parse_test_peer_flag<I: IntoIterator<Item = String>>(args: I) -> Option<TestPeerMode> {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--testpeer") {
            if rest.is_empty() {
                // Allow `--testpeer <mode>` as a follow-on argument.
                return match iter.next() {
                    Some(next) if !next.starts_with("--") => {
                        Some(TestPeerMode::parse(&next).unwrap_or_else(|| {
                            tracing::warn!("Unknown test peer mode '{}', using orbit", next);
                            TestPeerMode::Orbit
                        }))
                    }
                    _ => Some(TestPeerMode::Orbit),
                };
            } else if let Some(mode_str) = rest.strip_prefix('=') {
                return Some(TestPeerMode::parse(mode_str).unwrap_or_else(|| {
                    tracing::warn!("Unknown test peer mode '{}', using orbit", mode_str);
                    TestPeerMode::Orbit
                }));
            }
        }
    }
    None
}
