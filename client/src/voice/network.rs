//! Async network task and the command/event channel types that bridge it
//! with `VoiceManager`.

use tokio::sync::mpsc;

use crate::audio::IncomingAudioCommand;
use crate::network::{ConnectionState, PeerInfo, ServerMessage, SignalingClient};
use crate::position::Position;

/// Commands to send to the network task
#[derive(Debug)]
pub enum NetworkCommand {
    Connect,
    Disconnect,
    JoinRoom {
        room_id: String,
        player_name: String,
        api_key: Option<String>,
    },
    ValidateApiKey {
        api_key: String,
    },
    /// Leave a single room (`Some`) or every joined room (`None`).
    LeaveRoom {
        room_id: Option<String>,
    },
    UpdatePosition {
        position: Position,
        front: Position,
    },
    SendAudio {
        room_id: String,
        data: Vec<u8>,
    },
    /// Forward a debounced group snapshot to the server for clustering.
    IdentifyGroup {
        members: Vec<String>,
    },
}

/// Events received from the network task
#[derive(Debug)]
pub enum NetworkEvent {
    Connected {
        peer_id: String,
    },
    Disconnected,
    AccountValidated {
        account_name: Option<String>,
    },
    RoomJoined {
        room_id: String,
        peers: Vec<PeerInfo>,
    },
    JoinRejected {
        room_id: String,
        reason: String,
    },
    PeerJoined {
        room_id: String,
        peer_id: String,
        player_name: String,
        account_name: Option<String>,
    },
    PeerLeft {
        room_id: String,
        peer_id: String,
    },
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    AudioReceived {
        room_id: String,
        peer_id: String,
        data: Vec<u8>,
    },
    GroupIdentified {
        cluster_id: String,
    },
    Error {
        message: String,
    },
}

/// Async network task that handles signaling and audio relay
pub(super) async fn network_task(
    server_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<NetworkCommand>,
    event_tx: std::sync::mpsc::Sender<NetworkEvent>,
    incoming_audio_tx: crossbeam_channel::Sender<IncomingAudioCommand>,
) {
    log::info!("Network task started");

    let mut signaling_client = SignalingClient::new(&server_url);
    let mut signaling_event_rx: Option<mpsc::UnboundedReceiver<ServerMessage>> = None;
    let mut was_connected = false;

    let mut reconnect_timer = tokio::time::interval(std::time::Duration::from_secs(2));
    reconnect_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't race the initial Connect command.
    reconnect_timer.tick().await;

    loop {
        tokio::select! {
            // Handle commands from VoiceManager
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    NetworkCommand::Connect => {
                        log::info!("Connecting to signaling server...");
                        match signaling_client.connect().await {
                            Ok(()) => {
                                signaling_event_rx = signaling_client.take_event_receiver();
                            }
                            Err(e) => {
                                log::error!("Failed to connect: {}", e);
                                let _ = event_tx.send(NetworkEvent::Error {
                                    message: format!("Connection failed: {}", e),
                                });
                            }
                        }
                    }
                    NetworkCommand::Disconnect => {
                        log::info!("Disconnecting...");
                        signaling_client.disconnect();
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::ResetIncoming);
                        let _ = event_tx.send(NetworkEvent::Disconnected);
                        break;
                    }
                    NetworkCommand::JoinRoom { room_id, player_name, api_key } => {
                        if let Err(e) = signaling_client.join_room(
                            &room_id,
                            &player_name,
                            api_key.as_deref(),
                        ) {
                            log::error!("Failed to join room: {}", e);
                        }
                    }
                    NetworkCommand::ValidateApiKey { api_key } => {
                        if let Err(e) = signaling_client.validate_api_key(&api_key) {
                            log::error!("Failed to send ValidateApiKey: {}", e);
                        }
                    }
                    NetworkCommand::LeaveRoom { room_id } => {
                        if let Err(e) = signaling_client.leave_room(room_id.as_deref()) {
                            log::error!("Failed to leave room: {}", e);
                        }
                    }
                    NetworkCommand::UpdatePosition { position, front } => {
                        let _ = signaling_client.update_position(position, front);
                    }
                    NetworkCommand::SendAudio { room_id, data } => {
                        let _ = signaling_client.send_audio(&room_id, &data);
                    }
                    NetworkCommand::IdentifyGroup { members } => {
                        if let Err(e) = signaling_client.identify_group(members) {
                            log::warn!("Failed to send IdentifyGroup: {}", e);
                        }
                    }
                }
            }

            // Handle signaling events
            msg = async {
                if let Some(ref mut rx) = signaling_event_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match msg {
                    Some(ServerMessage::Welcome { peer_id }) => {
                        let _ = event_tx.send(NetworkEvent::Connected { peer_id });
                    }
                    Some(ServerMessage::AccountValidated { account_name }) => {
                        let _ = event_tx.send(NetworkEvent::AccountValidated { account_name });
                    }
                    Some(ServerMessage::JoinRejected { room_id, reason }) => {
                        let _ = event_tx.send(NetworkEvent::JoinRejected { room_id, reason });
                    }
                    Some(ServerMessage::RoomJoined { room_id, peers }) => {
                        for peer in &peers {
                            let _ = incoming_audio_tx.send(IncomingAudioCommand::UpsertPeer {
                                peer_id: peer.peer_id.clone(),
                                player_name: peer.player_name.clone(),
                            });
                            if let (Some(position), Some(front)) = (peer.position, peer.front) {
                                let _ = incoming_audio_tx.send(IncomingAudioCommand::SetPeerPosition {
                                    peer_id: peer.peer_id.clone(),
                                    position,
                                    front,
                                });
                            }
                        }
                        let _ = event_tx.send(NetworkEvent::RoomJoined { room_id, peers });
                    }
                    Some(ServerMessage::PeerJoined { room_id, peer }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::UpsertPeer {
                            peer_id: peer.peer_id.clone(),
                            player_name: peer.player_name.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::PeerJoined {
                            room_id,
                            peer_id: peer.peer_id,
                            player_name: peer.player_name,
                            account_name: peer.account_name,
                        });
                    }
                    Some(ServerMessage::PeerLeft { room_id, peer_id }) => {
                        // Phase 2: audio thread is peer-keyed, so we can't
                        // drop "(peer, this room)" alone — let the manager
                        // decide when the peer's last shared room ended and
                        // emit RemovePeer then.
                        let _ = event_tx.send(NetworkEvent::PeerLeft { room_id, peer_id });
                    }
                    Some(ServerMessage::PeerPosition { peer_id, position, front }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::SetPeerPosition {
                            peer_id: peer_id.clone(),
                            position,
                            front,
                        });
                        let _ = event_tx.send(NetworkEvent::PeerPosition { peer_id, position, front });
                    }
                    Some(ServerMessage::PeerAudio { room_id, peer_id, data }) => {
                        let _ = incoming_audio_tx.send(IncomingAudioCommand::PushPeerOpus {
                            peer_id: peer_id.clone(),
                            room_id: room_id.clone(),
                            data: data.clone(),
                        });
                        let _ = event_tx.send(NetworkEvent::AudioReceived {
                            room_id,
                            peer_id,
                            data,
                        });
                    }
                    Some(ServerMessage::Error { message }) => {
                        log::error!("Server error: {}", message);
                        let _ = event_tx.send(NetworkEvent::Error { message });
                    }
                    Some(ServerMessage::Kicked { reason }) => {
                        log::warn!("Server kicked us: {}", reason);
                        let _ = event_tx.send(NetworkEvent::Error {
                            message: format!("Disconnected by server: {}", reason),
                        });
                    }
                    Some(ServerMessage::GroupIdentified { cluster_id }) => {
                        let _ = event_tx.send(NetworkEvent::GroupIdentified { cluster_id });
                    }
                    Some(ServerMessage::Pong) => {
                        // Keepalive response
                    }
                    None => {
                        // Signaling read task ended — the server dropped us.
                        signaling_event_rx = None;
                    }
                }
            }

            _ = reconnect_timer.tick() => {
                // Periodic wake-up to evaluate reconnection below.
            }
        }

        let state = signaling_client.state();
        if state == ConnectionState::Connected {
            was_connected = true;
        } else if state == ConnectionState::Disconnected {
            if was_connected {
                was_connected = false;
                let _ = incoming_audio_tx.send(IncomingAudioCommand::ResetIncoming);
                let _ = event_tx.send(NetworkEvent::Disconnected);
            }
            log::info!("Signaling disconnected; attempting reconnect...");
            match signaling_client.connect().await {
                Ok(()) => {
                    signaling_event_rx = signaling_client.take_event_receiver();
                    log::info!("Signaling reconnect succeeded");
                }
                Err(e) => {
                    log::warn!("Signaling reconnect failed: {}", e);
                }
            }
        }
    }

    log::info!("Network task stopped");
}
