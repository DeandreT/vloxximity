//! Per-peer WebSocket session: lifecycle, dispatch, validation, and rate
//! limiting. Wire types and frame codec live in `protocol`.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

use crate::protocol::{
    decode_client_audio_frame, encode_server_audio_frame, ClientMessage, PeerInfo, ServerMessage,
};
use crate::rate_limit::PeerRateLimits;
use crate::rooms::RoomEvent;
use crate::AppState;

/// Length caps on string fields. Enforced before any DB / API work to keep
/// a misbehaving peer from spending unbounded CPU/memory on the server.
const MAX_ROOM_ID_LEN: usize = 64;
const MAX_PLAYER_NAME_LEN: usize = 64;
const MAX_API_KEY_LEN: usize = 256;

enum OutgoingWsMessage {
    Text(ServerMessage),
    Binary(Vec<u8>),
}

/// Handle a WebSocket connection for one peer until it closes.
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
                                        if ws_sender.send(Message::Binary(data)).await.is_err() {
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
    let mut rates = PeerRateLimits::new();

    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            Message::Text(text) => {
                let client_msg = match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(m) => m,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse client message from {}: err={}",
                            peer_id_in, err
                        );
                        continue;
                    }
                };
                if !validate_message_lengths(&peer_id_in, &client_msg) {
                    continue;
                }
                if !apply_rate_limit(&peer_id_in, &client_msg, &mut rates) {
                    if rates.record_overage() {
                        tracing::warn!(
                            "Disconnecting {} after repeated rate-limit hits",
                            peer_id_in
                        );
                        break;
                    }
                    continue;
                }
                handle_client_message(&peer_id_in, client_msg, &state_in, &direct_tx_in).await;
            }
            Message::Binary(data) => {
                if !rates.audio.try_take() {
                    if rates.record_overage() {
                        tracing::warn!(
                            "Disconnecting {} after repeated audio rate-limit hits",
                            peer_id_in
                        );
                        break;
                    }
                    continue;
                }
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
        RoomEvent::PeerJoined { peer_id, player_name, account_name } => {
            if peer_id == self_peer_id {
                return None;
            }
            Some(OutgoingWsMessage::Text(ServerMessage::PeerJoined {
                peer: PeerInfo {
                    peer_id,
                    player_name,
                    account_name,
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
        ClientMessage::JoinRoom { room_id, player_name, api_key } => {
            tracing::info!(
                "Received JoinRoom from {} -> room={} player_name={} api_key={}",
                peer_id, room_id, player_name, if api_key.is_some() { "yes" } else { "no" }
            );
            let account_name = match api_key.as_deref() {
                Some(key) if !key.trim().is_empty() => {
                    crate::gw2::validate_api_key(&state.http, &state.gw2_cache, key.trim()).await
                }
                _ => None,
            };

            // Always tell the client the validation outcome, so its UI can
            // leave the "Validating..." state deterministically even when
            // they didn't provide a key (in which case account_name is None).
            let _ = direct_tx.send(ServerMessage::AccountValidated {
                account_name: account_name.clone(),
            });

            let Some(account_name) = account_name else {
                let reason = if api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false) {
                    "GW2 API key rejected — check the key and the 'account' permission"
                } else {
                    "GW2 API key required to join rooms (set one in Vloxximity settings)"
                };
                tracing::info!(
                    "Rejecting JoinRoom from {} (room={}): {}",
                    peer_id, room_id, reason
                );
                let _ = direct_tx.send(ServerMessage::JoinRejected {
                    reason: reason.to_string(),
                });
                return;
            };

            if let Some(peers) =
                state.rooms.join_room(peer_id, &room_id, &player_name, Some(account_name))
            {
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

        ClientMessage::ValidateApiKey { api_key } => {
            let trimmed = api_key.trim();
            let account_name = if trimmed.is_empty() {
                None
            } else {
                crate::gw2::validate_api_key(&state.http, &state.gw2_cache, trimmed).await
            };
            state
                .rooms
                .set_account_name(peer_id, account_name.clone());
            let _ = direct_tx.send(ServerMessage::AccountValidated { account_name });
        }

        ClientMessage::LeaveRoom => {
            if let Some(room_id) = state.rooms.get_peer_room(peer_id) {
                state.rooms.leave_room(peer_id, &room_id);
            }
        }

        ClientMessage::UpdatePosition { position, front } => {
            state.rooms.update_position(peer_id, position, front);
        }

        ClientMessage::Ping => {
            let _ = direct_tx.send(ServerMessage::Pong);
        }
    }
}

/// Reject messages whose string fields exceed our caps. Returns false to drop.
fn validate_message_lengths(peer_id: &str, msg: &ClientMessage) -> bool {
    let too_long = |what: &str, len: usize, max: usize| -> bool {
        if len > max {
            tracing::warn!(
                "Dropping {} from {}: {} length {} > cap {}",
                msg_kind(msg), peer_id, what, len, max
            );
            true
        } else {
            false
        }
    };
    match msg {
        ClientMessage::JoinRoom { room_id, player_name, api_key } => {
            if too_long("room_id", room_id.len(), MAX_ROOM_ID_LEN) { return false; }
            if too_long("player_name", player_name.len(), MAX_PLAYER_NAME_LEN) { return false; }
            if let Some(key) = api_key {
                if too_long("api_key", key.len(), MAX_API_KEY_LEN) { return false; }
            }
            true
        }
        ClientMessage::ValidateApiKey { api_key } => {
            !too_long("api_key", api_key.len(), MAX_API_KEY_LEN)
        }
        _ => true,
    }
}

/// Charge the appropriate token bucket. Returns false when the bucket is
/// empty (caller drops the message and records an overage).
fn apply_rate_limit(peer_id: &str, msg: &ClientMessage, rates: &mut PeerRateLimits) -> bool {
    let bucket = match msg {
        ClientMessage::JoinRoom { .. } => &mut rates.join_room,
        ClientMessage::ValidateApiKey { .. } => &mut rates.validate_api_key,
        ClientMessage::UpdatePosition { .. } => &mut rates.update_position,
        // LeaveRoom and Ping are cheap and bounded by the connection itself.
        ClientMessage::LeaveRoom | ClientMessage::Ping => return true,
    };
    if bucket.try_take() {
        true
    } else {
        tracing::warn!("Rate limit hit on {} from {}", msg_kind(msg), peer_id);
        false
    }
}

fn msg_kind(msg: &ClientMessage) -> &'static str {
    match msg {
        ClientMessage::JoinRoom { .. } => "JoinRoom",
        ClientMessage::ValidateApiKey { .. } => "ValidateApiKey",
        ClientMessage::LeaveRoom => "LeaveRoom",
        ClientMessage::UpdatePosition { .. } => "UpdatePosition",
        ClientMessage::Ping => "Ping",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join_room(room: &str, name: &str, key: Option<&str>) -> ClientMessage {
        ClientMessage::JoinRoom {
            room_id: room.to_string(),
            player_name: name.to_string(),
            api_key: key.map(str::to_string),
        }
    }

    #[test]
    fn length_caps_pass_normal_messages() {
        assert!(validate_message_lengths("p", &join_room("room", "Alice", Some("k"))));
        assert!(validate_message_lengths("p", &join_room("room", "Alice", None)));
        assert!(validate_message_lengths(
            "p",
            &ClientMessage::ValidateApiKey { api_key: "k".to_string() }
        ));
        assert!(validate_message_lengths("p", &ClientMessage::LeaveRoom));
        assert!(validate_message_lengths("p", &ClientMessage::Ping));
    }

    #[test]
    fn length_caps_reject_oversize_room_id() {
        let too_long = "x".repeat(MAX_ROOM_ID_LEN + 1);
        assert!(!validate_message_lengths("p", &join_room(&too_long, "A", None)));
    }

    #[test]
    fn length_caps_reject_oversize_player_name() {
        let too_long = "x".repeat(MAX_PLAYER_NAME_LEN + 1);
        assert!(!validate_message_lengths("p", &join_room("r", &too_long, None)));
    }

    #[test]
    fn length_caps_reject_oversize_api_key_in_join() {
        let too_long = "x".repeat(MAX_API_KEY_LEN + 1);
        assert!(!validate_message_lengths(
            "p",
            &join_room("r", "A", Some(&too_long))
        ));
    }

    #[test]
    fn length_caps_reject_oversize_api_key_in_validate() {
        let too_long = "x".repeat(MAX_API_KEY_LEN + 1);
        assert!(!validate_message_lengths(
            "p",
            &ClientMessage::ValidateApiKey { api_key: too_long }
        ));
    }

    #[test]
    fn length_caps_accept_at_boundary() {
        let exact = "x".repeat(MAX_API_KEY_LEN);
        assert!(validate_message_lengths(
            "p",
            &ClientMessage::ValidateApiKey { api_key: exact }
        ));
    }

    #[test]
    fn rate_limit_charges_correct_bucket() {
        let mut rates = PeerRateLimits::new();
        // JoinRoom bucket has burst=2; third call within the same instant
        // must be rejected.
        assert!(apply_rate_limit("p", &join_room("r", "A", None), &mut rates));
        assert!(apply_rate_limit("p", &join_room("r", "A", None), &mut rates));
        assert!(!apply_rate_limit("p", &join_room("r", "A", None), &mut rates));
        // LeaveRoom and Ping bypass the buckets — they should still pass.
        assert!(apply_rate_limit("p", &ClientMessage::LeaveRoom, &mut rates));
        assert!(apply_rate_limit("p", &ClientMessage::Ping, &mut rates));
    }
}
