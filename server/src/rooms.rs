//! Room management for voice chat instances.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Position in 3D space
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Peer information
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub player_name: String,
    pub room_id: Option<String>,
    pub position: Position,
    pub front: Position,
    pub last_update: Instant,
    pub tx: broadcast::Sender<RoomEvent>,
}

#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    pub room_id: Option<String>,
    pub position: Position,
    pub front: Position,
}

impl Peer {
    pub fn new(tx: broadcast::Sender<RoomEvent>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            player_name: String::new(),
            room_id: None,
            position: Position::default(),
            front: Position::new(0.0, 0.0, 1.0),
            last_update: Instant::now(),
            tx,
        }
    }
}

impl Position {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Events broadcast within a room
#[derive(Debug, Clone)]
pub enum RoomEvent {
    PeerJoined {
        peer_id: String,
        player_name: String,
    },
    PeerLeft {
        peer_id: String,
    },
    PeerPosition {
        peer_id: String,
        position: Position,
        front: Position,
    },
    PeerAudio {
        peer_id: String,
        data: Vec<u8>,
    },
    SdpOffer {
        from_peer: String,
        to_peer: String,
        sdp: String,
    },
    SdpAnswer {
        from_peer: String,
        to_peer: String,
        sdp: String,
    },
    IceCandidate {
        from_peer: String,
        to_peer: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

/// Room containing multiple peers
#[derive(Debug)]
pub struct Room {
    pub peers: DashMap<String, PeerInfo>,
}

/// Lightweight peer info for room listing
#[derive(Debug, Clone, Serialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub player_name: String,
    pub position: Option<Position>,
    pub front: Option<Position>,
}

impl Room {
    pub fn new() -> Self {
        Self {
            peers: DashMap::new(),
        }
    }

    pub fn add_peer(&self, peer: &Peer) {
        self.peers.insert(
            peer.id.clone(),
            PeerInfo {
                peer_id: peer.id.clone(),
                player_name: peer.player_name.clone(),
                position: Some(peer.position),
                front: Some(peer.front),
            },
        );
    }

    pub fn remove_peer(&self, peer_id: &str) {
        self.peers.remove(peer_id);
    }

    pub fn update_position(&self, peer_id: &str, position: Position, front: Position) {
        if let Some(mut peer) = self.peers.get_mut(peer_id) {
            peer.position = Some(position);
            peer.front = Some(front);
        }
    }

    pub fn get_peers(&self) -> Vec<PeerInfo> {
        self.peers.iter().map(|p| p.value().clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Room manager handling all rooms
pub struct RoomManager {
    rooms: DashMap<String, Arc<Room>>,
    peers: DashMap<String, Peer>,
}

impl RoomManager {
    pub fn new() -> Self {
        Self {
            rooms: DashMap::new(),
            peers: DashMap::new(),
        }
    }

    /// Register a new peer
    pub fn register_peer(&self, tx: broadcast::Sender<RoomEvent>) -> String {
        let peer = Peer::new(tx);
        let peer_id = peer.id.clone();
        self.peers.insert(peer_id.clone(), peer);
        tracing::info!("Peer registered: {}", peer_id);
        peer_id
    }

    /// Unregister a peer
    pub fn unregister_peer(&self, peer_id: &str) {
        if let Some((_, peer)) = self.peers.remove(peer_id) {
            if let Some(room_id) = &peer.room_id {
                self.leave_room(peer_id, room_id);
            }
        }
        tracing::info!("Peer unregistered: {}", peer_id);
    }

    /// Join a room
    pub fn join_room(&self, peer_id: &str, room_id: &str, player_name: &str) -> Option<Vec<PeerInfo>> {
        // Update peer info
        let mut peer = self.peers.get_mut(peer_id)?;
        peer.player_name = player_name.to_string();

        // Leave current room if any
        if let Some(old_room_id) = peer.room_id.take() {
            drop(peer); // Release lock
            self.leave_room(peer_id, &old_room_id);
            peer = self.peers.get_mut(peer_id)?;
        }

        peer.room_id = Some(room_id.to_string());

        // Get or create room
        let room = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(|| Arc::new(Room::new()))
            .clone();

        // Get existing peers before adding new one
        let existing_peers = room.get_peers();

        // Add peer to room
        room.add_peer(&peer);

        // Release the per-peer write lock before iterating `self.peers`, or
        // DashMap will deadlock when `iter()` tries to read-lock the same shard.
        drop(peer);

        // Notify other peers
        let event = RoomEvent::PeerJoined {
            peer_id: peer_id.to_string(),
            player_name: player_name.to_string(),
        };

        for other_peer in self.peers.iter() {
            if other_peer.id != peer_id {
                if let Some(ref other_room) = other_peer.room_id {
                    if other_room == room_id {
                        let _ = other_peer.tx.send(event.clone());
                    }
                }
            }
        }

        tracing::info!("Peer {} joined room {}", peer_id, room_id);
        Some(existing_peers)
    }

    /// Leave a room
    pub fn leave_room(&self, peer_id: &str, room_id: &str) {
        // Remove from room
        if let Some(room) = self.rooms.get(room_id) {
            room.remove_peer(peer_id);

            // Notify other peers
            let event = RoomEvent::PeerLeft {
                peer_id: peer_id.to_string(),
            };

            for other_peer in self.peers.iter() {
                if other_peer.id != peer_id {
                    if let Some(ref other_room) = other_peer.room_id {
                        if other_room == room_id {
                            let _ = other_peer.tx.send(event.clone());
                        }
                    }
                }
            }

            // Clean up empty rooms
            if room.is_empty() {
                drop(room);
                self.rooms.remove(room_id);
                tracing::info!("Room {} removed (empty)", room_id);
            }
        }

        // Update peer
        if let Some(mut peer) = self.peers.get_mut(peer_id) {
            peer.room_id = None;
        }

        tracing::info!("Peer {} left room {}", peer_id, room_id);
    }

    /// Update peer position
    pub fn update_position(&self, peer_id: &str, position: Position, front: Position) {
        if let Some(mut peer) = self.peers.get_mut(peer_id) {
            peer.position = position;
            peer.front = front;
            peer.last_update = Instant::now();

            if let Some(ref room_id) = peer.room_id {
                // Update in room
                if let Some(room) = self.rooms.get(room_id) {
                    room.update_position(peer_id, position, front);
                }

                let room_id = room_id.clone();
                drop(peer);

                // Broadcast to other peers in room
                let event = RoomEvent::PeerPosition {
                    peer_id: peer_id.to_string(),
                    position,
                    front,
                };

                for other_peer in self.peers.iter() {
                    if other_peer.id != peer_id {
                        if let Some(ref other_room) = other_peer.room_id {
                            if other_room == &room_id {
                                let _ = other_peer.tx.send(event.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Broadcast audio to all other peers in the same room.
    pub fn broadcast_audio(&self, peer_id: &str, data: Vec<u8>) {
        if let Some(peer) = self.peers.get(peer_id) {
            if let Some(ref room_id) = peer.room_id {
                let room_id = room_id.clone();
                drop(peer);

                let event = RoomEvent::PeerAudio {
                    peer_id: peer_id.to_string(),
                    data,
                };

                for other_peer in self.peers.iter() {
                    if other_peer.id != peer_id {
                        if let Some(ref other_room) = other_peer.room_id {
                            if other_room == &room_id {
                                let _ = other_peer.tx.send(event.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    /// Forward SDP offer
    pub fn forward_sdp_offer(&self, from_peer: &str, to_peer: &str, sdp: &str) {
        if let Some(target) = self.peers.get(to_peer) {
            let event = RoomEvent::SdpOffer {
                from_peer: from_peer.to_string(),
                to_peer: to_peer.to_string(),
                sdp: sdp.to_string(),
            };
            let _ = target.tx.send(event);
        }
    }

    /// Forward SDP answer
    pub fn forward_sdp_answer(&self, from_peer: &str, to_peer: &str, sdp: &str) {
        if let Some(target) = self.peers.get(to_peer) {
            let event = RoomEvent::SdpAnswer {
                from_peer: from_peer.to_string(),
                to_peer: to_peer.to_string(),
                sdp: sdp.to_string(),
            };
            let _ = target.tx.send(event);
        }
    }

    /// Forward ICE candidate
    pub fn forward_ice_candidate(
        &self,
        from_peer: &str,
        to_peer: &str,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) {
        if let Some(target) = self.peers.get(to_peer) {
            let event = RoomEvent::IceCandidate {
                from_peer: from_peer.to_string(),
                to_peer: to_peer.to_string(),
                candidate: candidate.to_string(),
                sdp_mid,
                sdp_mline_index,
            };
            let _ = target.tx.send(event);
        }
    }

    /// Get room count
    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Get total peer count
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get peer's current room
    pub fn get_peer_room(&self, peer_id: &str) -> Option<String> {
        self.peers.get(peer_id)?.room_id.clone()
    }

    pub fn get_peer_snapshot(&self, peer_id: &str) -> Option<PeerSnapshot> {
        let peer = self.peers.get(peer_id)?;
        Some(PeerSnapshot {
            room_id: peer.room_id.clone(),
            position: peer.position,
            front: peer.front,
        })
    }

    /// Snapshot `(room_id, peer_ids)` for every live room.
    pub fn rooms_with_peers(&self) -> Vec<(String, Vec<String>)> {
        self.rooms
            .iter()
            .map(|entry| {
                let peer_ids = entry.value().peers.iter().map(|p| p.key().clone()).collect();
                (entry.key().clone(), peer_ids)
            })
            .collect()
    }

    /// Find any peer in the given room other than `exclude`, if one exists.
    pub fn first_other_peer_in(&self, room_id: &str, exclude: &str) -> Option<String> {
        let room = self.rooms.get(room_id)?;
        let found = room
            .peers
            .iter()
            .map(|p| p.key().clone())
            .find(|id| id != exclude);
        found
    }
}

impl Default for RoomManager {
    fn default() -> Self {
        Self::new()
    }
}
