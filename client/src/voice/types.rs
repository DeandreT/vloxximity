//! Public data types shared across the voice subsystem.

use serde::{Deserialize, Serialize};

use super::group::GroupKind;
use super::room_type::RoomTypeVolumes;
use crate::position::Position;

/// Result of a local GW2 API key validation. Lives in a shared slot on
/// `VoiceManager` so a background tokio task can write the result and the
/// UI can read it on the next frame.
#[derive(Debug, Clone)]
pub enum ApiKeyStatus {
    /// No key entered, or the current key hasn't been validated yet.
    Unknown,
    /// A validation request is in flight.
    Validating,
    /// GW2 returned an account handle for this key.
    Valid { account_name: String },
    /// GW2 rejected the key or the request failed. `message` is a short,
    /// user-visible reason.
    Invalid { message: String },
}

/// Voice activation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceMode {
    /// Push-to-talk
    PushToTalk,
    /// Voice activity detection
    VoiceActivity,
    /// Always transmit
    AlwaysOn,
}

/// Default signaling server URL
pub const DEFAULT_SERVER_URL: &str = "ws://localhost:8080/ws";

/// Voice manager settings. `#[serde(default)]` on the struct keeps older
/// on-disk configs loadable as we add new fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    pub mode: VoiceMode,
    pub ptt_key: u32,
    pub min_distance: f32,
    pub max_distance: f32,
    pub input_volume: f32,
    pub output_volume: f32,
    /// Session-only
    #[serde(skip)]
    pub is_muted: bool,
    /// Session-only
    #[serde(skip)]
    pub is_deafened: bool,
    /// Master switch for directional cues. When false, all peers play centered
    /// (mono → both ears) with distance attenuation only.
    pub directional_audio_enabled: bool,
    /// When directional audio is on, selects 3D filter model vs legacy 2D pan.
    pub spatial_3d_enabled: bool,
    pub show_peer_markers: bool,
    pub server_url: String,
    #[serde(skip)]
    pub gw2_api_key: String,
    /// Per-room-type playback gain. Map rooms keep the spatial pipeline;
    /// squad/party play centered with this gain on top of the
    /// per-peer and master output volumes.
    #[serde(default)]
    pub room_type_volumes: RoomTypeVolumes,
    /// When true, the client auto-joins a `squad:<cluster>` or
    /// `party:<cluster>` room while the local player is in a GW2
    /// squad/party. The map room is always auto-managed off MumbleLink.
    #[serde(default = "default_true")]
    pub auto_join_group_rooms: bool,
    /// When true, auto-joins a `wvw-team:<world_id>-<team_color_id>` room
    /// while the local player is in a WvW map. Leaves the room when the
    /// player exits WvW or the team assignment changes.
    #[serde(default = "default_true")]
    pub auto_join_wvw_rooms: bool,
    /// When true, auto-joins a `pvp-team:<match_key>-<team_color_id>` room
    /// while the local player is in a PvP arena. Leaves the room when the
    /// match ends or the player returns to the lobby.
    #[serde(default = "default_true")]
    pub auto_join_pvp_rooms: bool,
    #[serde(default)]
    pub speaking_indicator: SpeakingIndicatorSettings,
}

/// Customisation for the floating "who is speaking" overlay. Defaults
/// preserve the legacy top-right pill behaviour so existing users don't
/// see a sudden change after upgrading.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeakingIndicatorSettings {
    pub enabled: bool,
    /// When true, the overlay stays visible even with no active speakers
    /// so the user can right-click it to tweak settings or drag it.
    pub show_when_silent: bool,
    /// When true, the overlay sticks to its configured position.
    pub locked: bool,
    /// User-chosen screen position. `None` falls back to the auto-placed
    /// top-right corner.
    pub position: Option<[f32; 2]>,
    pub show_mute_buttons: bool,
    pub show_coordinates: bool,
    pub show_account_names: bool,
    pub max_visible: u32,
    pub bg_alpha: f32,
}

impl Default for SpeakingIndicatorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_when_silent: false,
            locked: true,
            position: None,
            show_mute_buttons: false,
            show_coordinates: false,
            show_account_names: false,
            max_visible: 5,
            bg_alpha: 0.7,
        }
    }
}

pub fn default_true() -> bool {
    true
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            mode: VoiceMode::PushToTalk,
            ptt_key: 0,
            min_distance: 100.0,
            max_distance: 5000.0,
            input_volume: 1.0,
            output_volume: 1.0,
            is_muted: false,
            is_deafened: false,
            directional_audio_enabled: true,
            spatial_3d_enabled: true,
            show_peer_markers: false,
            server_url: DEFAULT_SERVER_URL.to_string(),
            gw2_api_key: String::new(),
            room_type_volumes: RoomTypeVolumes::default(),
            auto_join_group_rooms: true,
            auto_join_wvw_rooms: true,
            auto_join_pvp_rooms: true,
            speaking_indicator: SpeakingIndicatorSettings::default(),
        }
    }
}

/// Voice manager state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Disconnected,
    Connecting,
    Connected,
    InRoom,
}

/// Suggested rooms derived from RTAPI group state + the server's
/// clustering reply. Surfaced in the settings UI as click-to-join rows.
#[derive(Debug, Clone)]
pub struct GroupSuggestions {
    pub room_id: String,
    pub member_count: usize,
    pub commander_account_name: Option<String>,
    pub kind: GroupKind,
}

/// Snapshot of a peer for UI display.
#[derive(Debug, Clone)]
pub struct NearbyPeer {
    pub peer_id: String,
    pub player_name: String,
    /// Server-validated GW2 account handle, when known.
    pub account_name: Option<String>,
    pub is_speaking: bool,
    pub is_muted: bool,
    pub position: Position,
    /// Distance from the local listener. `None` if no listener position is known yet.
    pub distance: Option<f32>,
}
