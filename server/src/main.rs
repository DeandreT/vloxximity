//! Vloxximity Signaling Server
//!
//! Handles room management, position tracking, and WebRTC signaling for Vloxximity voice chat.

mod gw2;
mod rooms;
mod signaling;
mod test_peer;

use axum::{
    extract::ws::WebSocketUpgrade,
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use rooms::RoomManager;
use signaling::handle_socket;
use test_peer::TestPeerMode;

/// Server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub turn_secret: String,
    pub turn_urls: Vec<String>,
    pub turn_ttl: u64,
    pub test_peer_mode: Option<TestPeerMode>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            turn_secret: std::env::var("TURN_SECRET").unwrap_or_else(|_| "vloxximity-secret".to_string()),
            turn_urls: vec![
                "turn:turn.vloxximity.example.com:3478".to_string(),
                "turns:turn.vloxximity.example.com:5349".to_string(),
            ],
            turn_ttl: 86400, // 24 hours
            test_peer_mode: None,
        }
    }
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<RoomManager>,
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

    info!("Vloxximity Signaling Server starting...");

    let test_peer_mode = parse_test_peer_flag(std::env::args().skip(1));
    if let Some(mode) = test_peer_mode {
        info!("Test peer enabled: mode={:?}", mode);
    }

    // Create application state
    let mut config = ServerConfig::default();
    config.test_peer_mode = test_peer_mode;
    let http = reqwest::Client::builder()
        .user_agent("vloxximity-server/0.1")
        .build()
        .expect("reqwest client build");
    let state = AppState {
        rooms: Arc::new(RoomManager::new()),
        config: Arc::new(config),
        http,
        gw2_cache: gw2::new_cache(),
    };

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
    info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// WebSocket upgrade handler
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
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
                    Some(next) if !next.starts_with("--") => Some(
                        TestPeerMode::parse(&next).unwrap_or_else(|| {
                            tracing::warn!("Unknown test peer mode '{}', using orbit", next);
                            TestPeerMode::Orbit
                        }),
                    ),
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
