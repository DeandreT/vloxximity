//! WebSocket client. Owns the connection to the server and routes
//! incoming `ServerMessage` events out via an `mpsc` channel.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::position::Position;

use super::protocol::{
    decode_server_audio_frame, encode_client_audio_frame, ClientMessage, ConnectionState,
    ServerMessage,
};

/// Signaling client for WebSocket communication
pub struct SignalingClient {
    server_url: String,
    state: Arc<RwLock<ConnectionState>>,
    peer_id: Arc<RwLock<Option<String>>>,
    /// Rooms the local client is currently joined to. Updated as
    /// `RoomJoined` / `PeerLeft` (for self) flow through. Used by
    /// reconnect so every joined room can be re-issued at once.
    joined_rooms: Arc<RwLock<HashSet<String>>>,
    message_tx: Option<mpsc::UnboundedSender<ClientOutboundMessage>>,
    event_rx: Option<mpsc::UnboundedReceiver<ServerMessage>>,
    event_tx: Option<mpsc::UnboundedSender<ServerMessage>>,
}

enum ClientOutboundMessage {
    Text(ClientMessage),
    Audio { room_id: String, data: Vec<u8> },
}

impl SignalingClient {
    pub fn new(server_url: &str) -> Self {
        Self {
            server_url: server_url.to_string(),
            state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            peer_id: Arc::new(RwLock::new(None)),
            joined_rooms: Arc::new(RwLock::new(HashSet::new())),
            message_tx: None,
            event_rx: None,
            event_tx: None,
        }
    }

    /// Connect to the signaling server
    pub async fn connect(&mut self) -> Result<()> {
        *self.state.write() = ConnectionState::Connecting;

        let url = match url::Url::parse(&self.server_url) {
            Ok(u) => u,
            Err(e) => {
                *self.state.write() = ConnectionState::Disconnected;
                return Err(e.into());
            }
        };
        let (ws_stream, _) = match connect_async(url).await {
            Ok(v) => v,
            Err(e) => {
                *self.state.write() = ConnectionState::Disconnected;
                return Err(e.into());
            }
        };

        log::info!("Connected to signaling server: {}", self.server_url);

        let (mut write, mut read) = ws_stream.split();

        // Create channels
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<ClientOutboundMessage>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<ServerMessage>();

        self.message_tx = Some(msg_tx);
        self.event_tx = Some(evt_tx.clone());
        self.event_rx = Some(evt_rx);

        let state = self.state.clone();
        let peer_id = self.peer_id.clone();
        let joined_rooms_read = self.joined_rooms.clone();

        // Spawn write task
        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                match msg {
                    ClientOutboundMessage::Text(msg) => {
                        let json = serde_json::to_string(&msg).unwrap();
                        log::debug!("Signaling outgoing: {}", json);
                        if write.send(Message::Text(json.into())).await.is_err() {
                            log::warn!("Signaling write task: failed to send message");
                            break;
                        }
                    }
                    ClientOutboundMessage::Audio { room_id, data } => {
                        let frame = encode_client_audio_frame(&room_id, &data);
                        if write.send(Message::Binary(frame.into())).await.is_err() {
                            log::warn!("Signaling write task: failed to send audio");
                            break;
                        }
                    }
                }
            }
        });

        // Spawn read task
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                match msg {
                    Message::Text(text) => {
                        log::debug!("Signaling incoming raw: {}", text);
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(server_msg) => {
                                // Handle welcome message
                                if let ServerMessage::Welcome { peer_id: id } = &server_msg {
                                    *peer_id.write() = Some(id.clone());
                                    *state.write() = ConnectionState::Connected;
                                    log::info!("Received peer ID: {}", id);
                                }

                                // Track joined-room set so reconnect can re-issue them.
                                if let ServerMessage::RoomJoined { room_id, .. } = &server_msg {
                                    joined_rooms_read.write().insert(room_id.clone());
                                    log::info!("Joined room: {}", room_id);
                                }

                                let _ = evt_tx.send(server_msg);
                            }
                            Err(err) => {
                                log::warn!("Failed to parse server message: {} -- err: {}", text, err);
                            }
                        }
                    }
                    Message::Binary(data) => {
                        if let Some((peer_id, room_id, audio)) = decode_server_audio_frame(&data) {
                            let _ = evt_tx.send(ServerMessage::PeerAudio {
                                room_id,
                                peer_id,
                                data: audio,
                            });
                        } else {
                            log::warn!("Failed to parse binary server message");
                        }
                    }
                    Message::Close(_) => {
                        *state.write() = ConnectionState::Disconnected;
                        break;
                    }
                    _ => {}
                }
            }
            *state.write() = ConnectionState::Disconnected;
        });

        Ok(())
    }

    /// Disconnect from the server
    pub fn disconnect(&mut self) {
        self.message_tx = None;
        self.event_rx = None;
        self.event_tx = None;
        *self.state.write() = ConnectionState::Disconnected;
        *self.peer_id.write() = None;
        self.joined_rooms.write().clear();
    }

    /// Send a message to the server
    pub fn send(&self, msg: ClientMessage) -> Result<()> {
        if let Some(tx) = &self.message_tx {
            tx.send(ClientOutboundMessage::Text(msg))?;
        }
        Ok(())
    }

    /// Join a room
    pub fn join_room(&self, room_id: &str, player_name: &str, api_key: Option<&str>) -> Result<()> {
        log::info!(
            "Client joining room '{}' as '{}' api_key={}",
            room_id,
            player_name,
            if api_key.is_some() { "yes" } else { "no" }
        );
        let res = self.send(ClientMessage::JoinRoom {
            room_id: room_id.to_string(),
            player_name: player_name.to_string(),
            api_key: api_key.map(|k| k.to_string()),
        });
        if res.is_err() {
            log::warn!("Failed to send JoinRoom message: {:?}", res.as_ref().err());
        }
        res
    }

    /// Leave a single room when `room_id` is `Some`, or every joined room
    /// when `None` (used at logout / re-auth / disconnect).
    pub fn leave_room(&self, room_id: Option<&str>) -> Result<()> {
        if let Some(id) = room_id {
            self.joined_rooms.write().remove(id);
        } else {
            self.joined_rooms.write().clear();
        }
        self.send(ClientMessage::LeaveRoom {
            room_id: room_id.map(str::to_string),
        })
    }

    /// Ask the server to validate an API key without touching room state.
    pub fn validate_api_key(&self, api_key: &str) -> Result<()> {
        self.send(ClientMessage::ValidateApiKey {
            api_key: api_key.to_string(),
        })
    }

    /// Update position
    pub fn update_position(&self, position: Position, front: Position) -> Result<()> {
        self.send(ClientMessage::UpdatePosition { position, front })
    }

    /// Report the local GW2 group's membership for server-side clustering.
    pub fn identify_group(&self, members: Vec<String>) -> Result<()> {
        self.send(ClientMessage::IdentifyGroup { members })
    }

    /// Send audio data as a binary Opus frame, tagged with the target room.
    pub fn send_audio(&self, room_id: &str, data: &[u8]) -> Result<()> {
        if let Some(tx) = &self.message_tx {
            tx.send(ClientOutboundMessage::Audio {
                room_id: room_id.to_string(),
                data: data.to_vec(),
            })?;
        }
        Ok(())
    }

    /// Send ping
    pub fn ping(&self) -> Result<()> {
        self.send(ClientMessage::Ping)
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<ServerMessage>> {
        self.event_rx.take()
    }

    /// Get connection state
    pub fn state(&self) -> ConnectionState {
        *self.state.read()
    }

    /// Get peer ID
    pub fn peer_id(&self) -> Option<String> {
        self.peer_id.read().clone()
    }

    /// Snapshot of every room the client is currently joined to.
    pub fn joined_rooms(&self) -> HashSet<String> {
        self.joined_rooms.read().clone()
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.state() == ConnectionState::Connected
    }
}

impl Default for SignalingClient {
    fn default() -> Self {
        Self::new("wss://0.0.0.0:8080")
    }
}
