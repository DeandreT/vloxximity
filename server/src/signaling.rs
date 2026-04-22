//! WebSocket signaling protocol handling.

use axum::extract::ws::{Message, WebSocket};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};

use crate::rooms::{Position, RoomEvent};
use crate::AppState;

const AUDIO_FRAME_KIND: u8 = 1;

/// Messages from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    JoinRoom {
        room_id: String,
        player_name: String,
    },
    LeaveRoom,
    UpdatePosition {
        position: Position,
        front: Position,
    },
    SdpOffer {
        target_peer: String,
        sdp: String,
    },
    SdpAnswer {
        target_peer: String,
        sdp: String,
    },
    IceCandidate {
        target_peer: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    RequestTurnCredentials,
    Ping,
}

/// Messages from server to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        peer_id: String,
    },
    RoomJoined {
        room_id: String,
        peers: Vec<PeerInfo>,
    },
    PeerJoined {
        peer: PeerInfo,
    },
    PeerLeft {
        peer_id: String,
    },
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    SdpOffer {
        from_peer: String,
        sdp: String,
    },
    SdpAnswer {
        from_peer: String,
        sdp: String,
    },
    IceCandidate {
        from_peer: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    TurnCredentials {
        username: String,
        credential: String,
        urls: Vec<String>,
        ttl: u64,
    },
    Error {
        message: String,
    },
    Pong,
}

enum OutgoingWsMessage {
    Text(ServerMessage),
    Binary(Vec<u8>),
}

/// Peer info for room listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub player_name: String,
    pub position: Option<Position>,
    pub front: Option<Position>,
}

impl From<crate::rooms::PeerInfo> for PeerInfo {
    fn from(p: crate::rooms::PeerInfo) -> Self {
        Self {
            peer_id: p.peer_id,
            player_name: p.player_name,
            position: p.position,
            front: p.front,
        }
    }
}

/// Handle a WebSocket connection
pub async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Create broadcast channel for room events
    let (room_tx, mut room_rx) = broadcast::channel::<RoomEvent>(256);

    // Create channel for direct messages to this peer
    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Register peer
    let peer_id = state.rooms.register_peer(room_tx);

    // Send welcome message
    let welcome = ServerMessage::Welcome {
        peer_id: peer_id.clone(),
    };
    if let Ok(json) = serde_json::to_string(&welcome) {
        let _ = ws_sender.send(Message::Text(json)).await;
    }

    tracing::info!("New connection: {}", peer_id);

    // Spawn task to handle outgoing messages (room events + direct messages)
    let peer_id_out = peer_id.clone();
    let out_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle room events
                result = room_rx.recv() => {
                    match result {
                        Ok(event) => {
                            if let Some(msg) = room_event_to_message(event, &peer_id_out) {
                                match msg {
                                    OutgoingWsMessage::Text(msg) => {
                                        if let Ok(json) = serde_json::to_string(&msg) {
                                            if ws_sender.send(Message::Text(json)).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                    OutgoingWsMessage::Binary(data) => {
                                        if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                // Handle direct messages
                Some(msg) = direct_rx.recv() => {
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if ws_sender.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    // Handle incoming messages
    let peer_id_in = peer_id.clone();
    let state_in = state.clone();
    let direct_tx_in = direct_tx.clone();

    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            Message::Text(text) => {
                tracing::info!("Incoming WS from {}: {}", peer_id_in, text);
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(client_msg) => {
                        handle_client_message(&peer_id_in, client_msg, &state_in, &direct_tx_in).await;
                    }
                    Err(err) => {
                        tracing::warn!("Failed to parse client message from {}: {} -- err: {}", peer_id_in, text, err);
                    }
                }
            }
            Message::Binary(data) => {
                handle_client_binary_message(&peer_id_in, &data, &state_in).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Clean up
    out_task.abort();
    state.rooms.unregister_peer(&peer_id);
    tracing::info!("Connection closed: {}", peer_id);
}

/// Convert room event to server message, filtering by peer
fn room_event_to_message(event: RoomEvent, self_peer_id: &str) -> Option<OutgoingWsMessage> {
    match event {
        RoomEvent::PeerJoined { peer_id, player_name } => {
            if peer_id == self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::PeerJoined {
                peer: PeerInfo {
                    peer_id,
                    player_name,
                    position: None,
                    front: None,
                },
            }))
        }
        RoomEvent::PeerLeft { peer_id } => {
            if peer_id == self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::PeerLeft { peer_id }))
        }
        RoomEvent::PeerPosition { peer_id, position, front } => {
            if peer_id == self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::PeerPosition { peer_id, position, front }))
        }
        RoomEvent::PeerAudio { peer_id, data } => {
            if peer_id == self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Binary(encode_server_audio_frame(&peer_id, &data)))
        }
        RoomEvent::SdpOffer { from_peer, to_peer, sdp } => {
            if to_peer != self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::SdpOffer { from_peer, sdp }))
        }
        RoomEvent::SdpAnswer { from_peer, to_peer, sdp } => {
            if to_peer != self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::SdpAnswer { from_peer, sdp }))
        }
        RoomEvent::IceCandidate { from_peer, to_peer, candidate, sdp_mid, sdp_mline_index } => {
            if to_peer != self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::IceCandidate {
                from_peer,
                candidate,
                sdp_mid,
                sdp_mline_index,
            }))
        }
    }
}

async fn handle_client_binary_message(peer_id: &str, data: &[u8], state: &AppState) {
    if let Some(audio) = decode_client_audio_frame(data) {
        state.rooms.broadcast_audio(peer_id, audio.to_vec());
    } else {
        tracing::warn!("Invalid binary frame from {}", peer_id);
    }
}

/// Handle a client message
async fn handle_client_message(
    peer_id: &str,
    msg: ClientMessage,
    state: &AppState,
    direct_tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    match msg {
        ClientMessage::JoinRoom { room_id, player_name } => {
            tracing::info!("Received JoinRoom from {} -> room={} player_name={}", peer_id, room_id, player_name);
            if let Some(peers) = state.rooms.join_room(peer_id, &room_id, &player_name) {
                let response = ServerMessage::RoomJoined {
                    room_id: room_id.clone(),
                    peers: peers.into_iter().map(Into::into).collect(),
                };
                let send_ok = direct_tx.send(response).is_ok();
                tracing::info!("Sent RoomJoined to {} (ok={})", peer_id, send_ok);
            } else {
                tracing::warn!("join_room returned None for peer {}", peer_id);
            }
        }

        ClientMessage::LeaveRoom => {
            if let Some(room_id) = state.rooms.get_peer_room(peer_id) {
                state.rooms.leave_room(peer_id, &room_id);
            }
        }

        ClientMessage::UpdatePosition { position, front } => {
            state.rooms.update_position(peer_id, position, front);
        }

        ClientMessage::SdpOffer { target_peer, sdp } => {
            state.rooms.forward_sdp_offer(peer_id, &target_peer, &sdp);
        }

        ClientMessage::SdpAnswer { target_peer, sdp } => {
            state.rooms.forward_sdp_answer(peer_id, &target_peer, &sdp);
        }

        ClientMessage::IceCandidate {
            target_peer,
            candidate,
            sdp_mid,
            sdp_mline_index,
        } => {
            state.rooms.forward_ice_candidate(
                peer_id,
                &target_peer,
                &candidate,
                sdp_mid,
                sdp_mline_index,
            );
        }

        ClientMessage::RequestTurnCredentials => {
            let creds = generate_turn_credentials(&state.config.turn_secret, state.config.turn_ttl);
            let response = ServerMessage::TurnCredentials {
                username: creds.0,
                credential: creds.1,
                urls: state.config.turn_urls.clone(),
                ttl: state.config.turn_ttl,
            };
            let _ = direct_tx.send(response);
        }

        ClientMessage::Ping => {
            let _ = direct_tx.send(ServerMessage::Pong);
        }
    }
}

fn decode_client_audio_frame(data: &[u8]) -> Option<&[u8]> {
    let (&kind, payload) = data.split_first()?;
    if kind != AUDIO_FRAME_KIND || payload.is_empty() {
        return None;
    }
    Some(payload)
}

fn encode_server_audio_frame(peer_id: &str, audio: &[u8]) -> Vec<u8> {
    let peer_id_bytes = peer_id.as_bytes();
    let peer_id_len = u16::try_from(peer_id_bytes.len()).unwrap_or(u16::MAX);
    let peer_id_len_usize = peer_id_len as usize;

    let mut frame = Vec::with_capacity(1 + 2 + peer_id_len_usize + audio.len());
    frame.push(AUDIO_FRAME_KIND);
    frame.extend_from_slice(&peer_id_len.to_le_bytes());
    frame.extend_from_slice(&peer_id_bytes[..peer_id_len_usize]);
    frame.extend_from_slice(audio);
    frame
}

/// Generate time-limited TURN credentials
/// Uses the coturn REST API credential format
fn generate_turn_credentials(secret: &str, ttl: u64) -> (String, String) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + ttl;

    let username = format!("{}:vloxximity", timestamp);

    // Generate HMAC-SHA1 credential
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(username.as_bytes());
    let result = mac.finalize();
    let credential = BASE64.encode(result.into_bytes());

    (username, credential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turn_credentials() {
        let (username, credential) = generate_turn_credentials("test-secret", 86400);

        // Username should contain timestamp
        assert!(username.contains(":vloxximity"));

        // Credential should be base64
        assert!(BASE64.decode(&credential).is_ok());
    }
}
