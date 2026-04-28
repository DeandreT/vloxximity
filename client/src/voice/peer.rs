//! Main-thread peer metadata for UI and room state.

use crate::position::Position;
use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::time::{Duration, Instant};

const SPEAKING_TIMEOUT: Duration = Duration::from_millis(300);

/// Peer metadata tracked on the main thread.
pub struct VoicePeer {
    pub peer_id: String,
    pub player_name: String,
    /// Server-validated GW2 account handle, when the peer supplied a valid
    /// API key.
    pub account_name: Option<String>,
    /// Rooms the local client currently shares with this peer. The peer is
    /// dropped from the local map when this becomes empty (last shared
    /// room ended).
    pub room_ids: HashSet<String>,
    pub position: Position,
    pub front: Position,
    pub volume: f32,
    pub is_muted: bool,
    last_audio_time: Mutex<Instant>,
    last_position_update: Mutex<Instant>,
}

impl VoicePeer {
    pub fn new(peer_id: String, player_name: String, account_name: Option<String>) -> Result<Self> {
        Ok(Self {
            peer_id,
            player_name,
            account_name,
            room_ids: HashSet::new(),
            position: Position::default(),
            front: Position::new(0.0, 0.0, 1.0),
            volume: 1.0,
            is_muted: false,
            last_audio_time: Mutex::new(Instant::now() - SPEAKING_TIMEOUT),
            last_position_update: Mutex::new(Instant::now()),
        })
    }

    pub fn update_position(&mut self, position: Position, front: Position) {
        self.position = position;
        self.front = front;
        *self.last_position_update.lock() = Instant::now();
    }

    pub fn mark_audio_received(&self) {
        *self.last_audio_time.lock() = Instant::now();
    }

    pub fn is_speaking(&self) -> bool {
        self.last_audio_time.lock().elapsed() <= SPEAKING_TIMEOUT
    }

    pub fn distance_to(&self, listener_position: &Position) -> f32 {
        self.position.distance_to(listener_position)
    }

    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        self.last_position_update.lock().elapsed() > timeout
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 2.0);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.is_muted = muted;
    }
}

impl std::fmt::Debug for VoicePeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePeer")
            .field("peer_id", &self.peer_id)
            .field("player_name", &self.player_name)
            .field("position", &self.position)
            .field("is_speaking", &self.is_speaking())
            .field("is_muted", &self.is_muted)
            .finish()
    }
}
