//! Room management for voice chat instances.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, Notify};
use uuid::Uuid;

use crate::limits::MAX_CONNECTIONS_PER_ACCOUNT;

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
    pub account_name: Option<String>,
    /// All rooms the peer is currently a member of. A peer may be in many
    /// rooms simultaneously (e.g. a `map:` room plus a `squad:` room) and
    /// receives audio from every one of them.
    pub room_ids: HashSet<String>,
    pub position: Position,
    pub front: Position,
    pub last_update: Instant,
    pub tx: broadcast::Sender<RoomEvent>,
    /// Timestamp of the most recent inbound WS message of any kind. Used by
    /// the dead-connection sweeper. The session task is the only writer; the
    /// sweeper is the only reader, so the mutex is uncontended in practice.
    pub last_seen: Arc<Mutex<Instant>>,
    /// Sweeper signals here to terminate the connection. The session loop
    /// awaits `kick.notified()` alongside the WS receiver.
    pub kick: Arc<Notify>,
}

/// Handles handed back from `register_peer` so the session task can update
/// liveness and watch for kicks without going through the DashMap on the hot
/// path.
#[derive(Clone)]
pub struct RegisteredPeer {
    pub peer_id: String,
    pub last_seen: Arc<Mutex<Instant>>,
    pub kick: Arc<Notify>,
}

#[derive(Debug)]
pub struct AccountCapExceeded;

#[derive(Debug, Default)]
struct AccountState {
    peer_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    pub room_ids: HashSet<String>,
    pub position: Position,
    pub front: Position,
}

impl Peer {
    pub fn new(tx: broadcast::Sender<RoomEvent>) -> Self {
        let now = Instant::now();
        Self {
            id: Uuid::new_v4().to_string(),
            player_name: String::new(),
            account_name: None,
            room_ids: HashSet::new(),
            position: Position::default(),
            front: Position::new(0.0, 0.0, 1.0),
            last_update: now,
            tx,
            last_seen: Arc::new(Mutex::new(now)),
            kick: Arc::new(Notify::new()),
        }
    }
}

impl Position {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Events broadcast within a room. The `Peer*` prefix is deliberate — it
/// names the subject (a peer, not the room itself). Membership-scoped events
/// (`PeerJoined`, `PeerLeft`, `PeerAudio`) carry the `room_id` so a listener
/// in many rooms can route the event correctly. `PeerPosition` is global —
/// world position is per-peer, not per-room.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum RoomEvent {
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
    PeerAudio {
        room_id: String,
        peer_id: String,
        data: Vec<u8>,
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
    pub account_name: Option<String>,
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
                account_name: peer.account_name.clone(),
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
    /// Index from GW2 account name to the peer ids currently bound to it.
    /// Maintained by `try_set_account_name` and `unregister_peer`. Used to
    /// enforce the per-account connection cap.
    accounts: DashMap<String, AccountState>,
}

impl RoomManager {
    pub fn new() -> Self {
        Self {
            rooms: DashMap::new(),
            peers: DashMap::new(),
            accounts: DashMap::new(),
        }
    }

    /// Register a new peer. Returns handles the session task uses for the
    /// hot path (liveness updates, kick signalling).
    pub fn register_peer(&self, tx: broadcast::Sender<RoomEvent>) -> RegisteredPeer {
        let peer = Peer::new(tx);
        let registered = RegisteredPeer {
            peer_id: peer.id.clone(),
            last_seen: peer.last_seen.clone(),
            kick: peer.kick.clone(),
        };
        self.peers.insert(peer.id.clone(), peer);
        tracing::info!("Peer registered: {}", registered.peer_id);
        registered
    }

    /// Unregister a peer
    pub fn unregister_peer(&self, peer_id: &str) {
        if let Some((_, peer)) = self.peers.remove(peer_id) {
            for room_id in &peer.room_ids {
                self.leave_room(peer_id, room_id);
            }
            if let Some(account) = &peer.account_name {
                self.unbind_account(account, peer_id);
            }
        }
        tracing::info!("Peer unregistered: {}", peer_id);
    }

    /// Drop `peer_id` from `account_name`'s bound list, removing the account
    /// entry entirely if it would become empty.
    fn unbind_account(&self, account_name: &str, peer_id: &str) {
        let now_empty = if let Some(mut entry) = self.accounts.get_mut(account_name) {
            entry.peer_ids.retain(|id| id != peer_id);
            entry.peer_ids.is_empty()
        } else {
            return;
        };
        if now_empty {
            self.accounts.remove(account_name);
        }
    }

    /// Add `peer_id` to `room_id`. Joins are *additive*: a peer may be in
    /// many rooms simultaneously. Re-joining a room the peer is already in
    /// is a no-op (no PeerJoined re-broadcast); the current peer list is
    /// still returned so the client can refresh state.
    ///
    /// Uses whatever account_name is already bound on the peer (set via
    /// `try_set_account_name`).
    pub fn join_room(
        &self,
        peer_id: &str,
        room_id: &str,
        player_name: &str,
    ) -> Option<Vec<PeerInfo>> {
        let was_already_member;
        let account_name;
        {
            let mut peer = self.peers.get_mut(peer_id)?;
            peer.player_name = player_name.to_string();
            account_name = peer.account_name.clone();
            was_already_member = !peer.room_ids.insert(room_id.to_string());
        }

        let room = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(|| Arc::new(Room::new()))
            .clone();

        let existing_peers = room.get_peers();

        if was_already_member {
            return Some(existing_peers);
        }

        // Re-borrow the peer to feed up-to-date info into the room's listing.
        if let Some(peer) = self.peers.get(peer_id) {
            room.add_peer(&peer);
        } else {
            // Peer vanished between the get_mut and now — bail.
            return None;
        }

        let event = RoomEvent::PeerJoined {
            room_id: room_id.to_string(),
            peer_id: peer_id.to_string(),
            player_name: player_name.to_string(),
            account_name,
        };

        for other_peer in self.peers.iter() {
            if other_peer.id != peer_id && other_peer.room_ids.contains(room_id) {
                let _ = other_peer.tx.send(event.clone());
            }
        }

        tracing::info!("Peer {} joined room {}", peer_id, room_id);
        Some(existing_peers)
    }

    /// Leave a single room. Idempotent: a no-op if the peer wasn't in it.
    pub fn leave_room(&self, peer_id: &str, room_id: &str) {
        // Drop the membership flag first. If the peer is gone (unregister
        // path already removed it), proceed with room-side cleanup anyway —
        // the peer may still be listed in `room.peers` and other peers still
        // need a PeerLeft.
        let was_member = match self.peers.get_mut(peer_id) {
            Some(mut peer) => peer.room_ids.remove(room_id),
            None => true,
        };

        if !was_member {
            return;
        }

        if let Some(room) = self.rooms.get(room_id) {
            room.remove_peer(peer_id);

            let event = RoomEvent::PeerLeft {
                room_id: room_id.to_string(),
                peer_id: peer_id.to_string(),
            };
            for other_peer in self.peers.iter() {
                if other_peer.id != peer_id && other_peer.room_ids.contains(room_id) {
                    let _ = other_peer.tx.send(event.clone());
                }
            }

            if room.is_empty() {
                drop(room);
                self.rooms.remove(room_id);
                tracing::info!("Room {} removed (empty)", room_id);
            }
        }

        tracing::info!("Peer {} left room {}", peer_id, room_id);
    }

    /// Leave every room the peer is currently in. Used at disconnect or when
    /// the client explicitly sends `LeaveRoom { room_id: None }`.
    pub fn leave_all_rooms(&self, peer_id: &str) {
        let rooms: Vec<String> = self
            .peers
            .get(peer_id)
            .map(|p| p.room_ids.iter().cloned().collect())
            .unwrap_or_default();
        for room_id in rooms {
            self.leave_room(peer_id, &room_id);
        }
    }

    /// Bind (or clear) a peer's account name, enforcing the per-account
    /// connection cap.
    ///
    /// Returns `Err(AccountCapExceeded)` only when binding to a *new* account
    /// would push that account over `MAX_CONNECTIONS_PER_ACCOUNT`. Re-binding
    /// to the same account is a no-op and always succeeds; clearing
    /// (`account_name = None`) always succeeds.
    pub fn try_set_account_name(
        &self,
        peer_id: &str,
        account_name: Option<String>,
    ) -> Result<(), AccountCapExceeded> {
        let current = self.peers.get(peer_id).and_then(|p| p.account_name.clone());

        if current == account_name {
            return Ok(());
        }

        // Reserve a slot in the new account first (so on cap-exceeded we
        // don't disturb the old binding).
        if let Some(ref new_name) = account_name {
            let mut entry = self.accounts.entry(new_name.clone()).or_default();
            if !entry.peer_ids.iter().any(|id| id == peer_id)
                && entry.peer_ids.len() >= MAX_CONNECTIONS_PER_ACCOUNT
            {
                return Err(AccountCapExceeded);
            }
            if !entry.peer_ids.iter().any(|id| id == peer_id) {
                entry.peer_ids.push(peer_id.to_string());
            }
        }

        if let Some(old_name) = current {
            self.unbind_account(&old_name, peer_id);
        }

        let room_ids: Vec<String> = if let Some(mut peer) = self.peers.get_mut(peer_id) {
            peer.account_name = account_name.clone();
            peer.room_ids.iter().cloned().collect()
        } else {
            Vec::new()
        };
        for room_id in &room_ids {
            if let Some(room) = self.rooms.get(room_id) {
                if let Some(mut info) = room.peers.get_mut(peer_id) {
                    info.account_name = account_name.clone();
                }
            }
        }
        Ok(())
    }

    /// Number of WS connections currently bound to `account_name`.
    pub fn account_connection_count(&self, account_name: &str) -> usize {
        self.accounts
            .get(account_name)
            .map(|s| s.peer_ids.len())
            .unwrap_or(0)
    }

    /// Iterate over `(peer_id, last_seen, kick)` for every registered peer.
    /// Used by the dead-connection sweeper.
    pub fn peer_liveness_handles(&self) -> Vec<(String, Arc<Mutex<Instant>>, Arc<Notify>)> {
        self.peers
            .iter()
            .map(|p| (p.id.clone(), p.last_seen.clone(), p.kick.clone()))
            .collect()
    }

    /// Refresh a peer's liveness timestamp without going through a WS
    /// message. Used by synthetic test peers that don't have an inbound
    /// socket but should not be culled by the sweeper.
    pub fn touch_peer(&self, peer_id: &str) {
        if let Some(peer) = self.peers.get(peer_id) {
            if let Ok(mut guard) = peer.last_seen.lock() {
                *guard = Instant::now();
            }
        }
    }

    /// Update peer position. Position is *peer-global* — the same world
    /// coordinates apply across every room the peer is in. Recipients are
    /// the union of all peers who share at least one room with the sender,
    /// deduplicated automatically because each peer has a single tx channel.
    pub fn update_position(&self, peer_id: &str, position: Position, front: Position) {
        let sender_rooms: HashSet<String> = {
            let Some(mut peer) = self.peers.get_mut(peer_id) else {
                return;
            };
            peer.position = position;
            peer.front = front;
            peer.last_update = Instant::now();
            peer.room_ids.clone()
        };

        if sender_rooms.is_empty() {
            return;
        }

        for room_id in &sender_rooms {
            if let Some(room) = self.rooms.get(room_id) {
                room.update_position(peer_id, position, front);
            }
        }

        let event = RoomEvent::PeerPosition {
            peer_id: peer_id.to_string(),
            position,
            front,
        };

        for other_peer in self.peers.iter() {
            if other_peer.id == peer_id {
                continue;
            }
            if other_peer.room_ids.is_disjoint(&sender_rooms) {
                continue;
            }
            let _ = other_peer.tx.send(event.clone());
        }
    }

    /// Broadcast audio tagged to a specific room. Drops the frame if the
    /// sender isn't a member of that room (defensive against malicious
    /// clients trying to inject audio into rooms they haven't joined).
    pub fn broadcast_audio(&self, peer_id: &str, room_id: &str, data: Vec<u8>) {
        let sender_in_room = self
            .peers
            .get(peer_id)
            .map(|p| p.room_ids.contains(room_id))
            .unwrap_or(false);
        if !sender_in_room {
            return;
        }

        let event = RoomEvent::PeerAudio {
            room_id: room_id.to_string(),
            peer_id: peer_id.to_string(),
            data,
        };

        for other_peer in self.peers.iter() {
            if other_peer.id != peer_id && other_peer.room_ids.contains(room_id) {
                let _ = other_peer.tx.send(event.clone());
            }
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

    /// Get the set of rooms a peer is currently in.
    pub fn get_peer_rooms(&self, peer_id: &str) -> HashSet<String> {
        self.peers
            .get(peer_id)
            .map(|p| p.room_ids.clone())
            .unwrap_or_default()
    }

    /// Get the GW2 account name bound to `peer_id`, if any. Used by the
    /// `IdentifyGroup` handler to verify the sender is in their own
    /// reported member list.
    pub fn peer_account_name(&self, peer_id: &str) -> Option<String> {
        self.peers.get(peer_id)?.account_name.clone()
    }

    pub fn get_peer_snapshot(&self, peer_id: &str) -> Option<PeerSnapshot> {
        let peer = self.peers.get(peer_id)?;
        Some(PeerSnapshot {
            room_ids: peer.room_ids.clone(),
            position: peer.position,
            front: peer.front,
        })
    }

    /// Snapshot `(room_id, peer_ids)` for every live room.
    pub fn rooms_with_peers(&self) -> Vec<(String, Vec<String>)> {
        self.rooms
            .iter()
            .map(|entry| {
                let peer_ids = entry
                    .value()
                    .peers
                    .iter()
                    .map(|p| p.key().clone())
                    .collect();
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

#[cfg(test)]
mod tests;
