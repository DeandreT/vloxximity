use nexus::data_link::read_resource;
use sha2::{Digest, Sha256};

use super::transform::{Position, Transform};

/// MumbleLink data identifier used by Nexus
pub const MUMBLE_LINK_ID: &str = "DL_MUMBLE_LINK";

/// Raw MumbleLink structure matching GW2's format
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct LinkedMem {
    pub ui_version: u32,
    pub ui_tick: u32,
    pub avatar_position: [f32; 3],
    pub avatar_front: [f32; 3],
    pub avatar_top: [f32; 3],
    pub name: [u16; 256],
    pub camera_position: [f32; 3],
    pub camera_front: [f32; 3],
    pub camera_top: [f32; 3],
    pub identity: [u16; 256],
    pub context_len: u32,
    pub context: [u8; 256],
    pub description: [u16; 2048],
}

/// GW2-specific context data
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct MumbleContext {
    pub server_address: [u8; 28],
    pub map_id: u32,
    pub map_type: u32,
    pub shard_id: u32,
    pub instance: u32,
    pub build_id: u32,
    pub ui_state: u32,
    pub compass_width: u16,
    pub compass_height: u16,
    pub compass_rotation: f32,
    pub player_x: f32,
    pub player_y: f32,
    pub map_center_x: f32,
    pub map_center_y: f32,
    pub map_scale: f32,
    pub process_id: u32,
    pub mount_index: u8,
}

/// Player identity from MumbleLink JSON
#[derive(Debug, Clone, Default)]
pub struct PlayerIdentity {
    pub name: String,
    pub profession: u32,
    pub map_id: u32,
    pub world_id: u32,
    pub team_color_id: u32,
    pub commander: bool,
    pub fov: f32,
    pub ui_size: u32,
}

/// GW2 API MapType enum ordinals as reported in the MumbleLink context.
/// Values 10–18 cover the WvW game modes (Eternal Battlegrounds, Borderlands,
/// and newer WvW variants). Values 3, 7, and 9 cover PvP arenas and
/// tournaments. These constants let callers check game mode without
/// hardcoding raw integers at every call site.
pub const MAP_TYPE_WVW_MIN: u32 = 10;
pub const MAP_TYPE_WVW_MAX: u32 = 18;
pub const MAP_TYPE_COMPETITIVE: u32 = 3;
pub const MAP_TYPE_TOURNAMENT: u32 = 7;
pub const MAP_TYPE_USER_TOURNAMENT: u32 = 9;

/// Returns `true` when `map_type` is one of the three PvP arena types.
pub fn is_pvp_map_type(map_type: u32) -> bool {
    matches!(
        map_type,
        MAP_TYPE_COMPETITIVE | MAP_TYPE_TOURNAMENT | MAP_TYPE_USER_TOURNAMENT
    )
}

/// Player state snapshot
#[derive(Debug, Clone)]
pub struct PlayerState {
    pub transform: Transform,
    pub camera_transform: Transform,
    pub identity: Option<PlayerIdentity>,
    pub room_key: String,
    pub map_id: u32,
    pub map_type: u32,
    pub ui_tick: u32,
    /// Opaque key identifying the current PvP match instance. `Some` only
    /// when `map_type` is a PvP arena type; `None` otherwise. Derived from
    /// a hash of (server_address + map_id + instance) so it is unique per
    /// match but stable across the duration of the same match.
    pub pvp_match_key: Option<String>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            transform: Transform::default(),
            camera_transform: Transform::default(),
            identity: None,
            room_key: String::new(),
            map_id: 0,
            map_type: 0,
            ui_tick: 0,
            pvp_match_key: None,
        }
    }
}

impl PlayerState {
    /// Check if player is currently in game
    pub fn is_in_game(&self) -> bool {
        self.ui_tick > 0 && !self.room_key.is_empty()
    }
}

/// MumbleLink reader using nexus's data_link API
pub struct MumbleLink {
    last_ui_tick: u32,
    last_room_key: String,
}

impl MumbleLink {
    pub fn new() -> Self {
        Self {
            last_ui_tick: 0,
            last_room_key: String::new(),
        }
    }

    /// Initialize MumbleLink (nexus handles the actual memory mapping)
    pub fn init(&mut self) -> anyhow::Result<()> {
        log::info!("Using nexus data_link API for MumbleLink");
        Ok(())
    }

    /// Read current player state from MumbleLink via nexus
    pub fn read(&mut self) -> Option<PlayerState> {
        // Read a snapshot of the shared LinkedMem to avoid torn/inconsistent reads
        let link = unsafe { read_resource::<LinkedMem>(MUMBLE_LINK_ID) }?;

        // Check if data is valid
        if link.ui_tick == 0 {
            return None;
        }

        self.last_ui_tick = link.ui_tick;

        // Extract transforms
        let transform = Transform {
            position: Position::from_array(link.avatar_position),
            front: Position::from_array(link.avatar_front),
            top: Position::from_array(link.avatar_top),
        };

        let camera_transform = Transform {
            position: Position::from_array(link.camera_position),
            front: Position::from_array(link.camera_front),
            top: Position::from_array(link.camera_top),
        };

        // Parse identity JSON
        let identity = parse_identity(&link.identity);

        // Parse context.
        //
        // GW2 reports `context_len` in the 48–85 byte range — the meaningful
        // prefix of the 256-byte context buffer. Our `MumbleContext` is 88
        // bytes after C struct padding, so checking `>= size_of::<...>()`
        // silently falls through to defaults on every read and the room_key
        // hash never changes when the player waypoints. Only the first 44
        // bytes (server_address + map_id + map_type + shard_id + instance)
        // are needed for the room key, and the underlying buffer is 256
        // bytes, so the wider `read_unaligned` is memory-safe even when
        // GW2 has only populated a prefix — fields past `context_len` may
        // contain stale bytes but we don't read them for routing.
        const MIN_CONTEXT_LEN_FOR_ROOM_KEY: usize = 44;
        let context = if (link.context_len as usize) >= MIN_CONTEXT_LEN_FOR_ROOM_KEY {
            unsafe { std::ptr::read_unaligned(link.context.as_ptr() as *const MumbleContext) }
        } else {
            MumbleContext::default()
        };

        let map_id = context.map_id;
        let map_type = context.map_type;

        // Generate room key
        let room_key = generate_room_key(&context);

        // Log room changes
        if room_key != self.last_room_key && !room_key.is_empty() {
            log::info!("Room changed: {} -> {}", self.last_room_key, room_key);
            self.last_room_key = room_key.clone();
        }

        let pvp_match_key = if is_pvp_map_type(map_type) {
            Some(generate_pvp_match_key(&context))
        } else {
            None
        };

        Some(PlayerState {
            transform,
            camera_transform,
            identity,
            room_key,
            map_id,
            map_type,
            ui_tick: link.ui_tick,
            pvp_match_key,
        })
    }
}

impl Default for MumbleLink {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse player identity from wide string JSON
fn parse_identity(identity: &[u16; 256]) -> Option<PlayerIdentity> {
    // Convert wide string to UTF-8
    let identity_str = String::from_utf16_lossy(identity)
        .trim_matches('\0')
        .to_string();

    if identity_str.is_empty() {
        return None;
    }

    #[derive(serde::Deserialize)]
    struct RawIdentity {
        name: Option<String>,
        profession: Option<u32>,
        #[serde(rename = "map_id")]
        map_id: Option<u32>,
        #[serde(rename = "world_id")]
        world_id: Option<u32>,
        #[serde(rename = "team_color_id")]
        team_color_id: Option<u32>,
        commander: Option<bool>,
        fov: Option<f32>,
        #[serde(rename = "uisz")]
        ui_size: Option<u32>,
    }

    // First attempt normal JSON parse
    match serde_json::from_str::<RawIdentity>(&identity_str) {
        Ok(raw) => Some(PlayerIdentity {
            name: raw.name.unwrap_or_default(),
            profession: raw.profession.unwrap_or(0),
            map_id: raw.map_id.unwrap_or(0),
            world_id: raw.world_id.unwrap_or(0),
            team_color_id: raw.team_color_id.unwrap_or(0),
            commander: raw.commander.unwrap_or(false),
            fov: raw.fov.unwrap_or(0.0),
            ui_size: raw.ui_size.unwrap_or(0),
        }),
        Err(e) => {
            // Try a more permissive parse to extract `name` field if possible
            log::trace!("Failed to parse identity JSON: {}", e);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&identity_str) {
                if let Some(name_val) = v.get("name").and_then(|n| n.as_str()) {
                    return Some(PlayerIdentity {
                        name: name_val.to_string(),
                        profession: 0,
                        map_id: 0,
                        world_id: 0,
                        team_color_id: 0,
                        commander: false,
                        fov: 0.0,
                        ui_size: 0,
                    });
                }
            }

            // Fallback: simple string search for "name":"..."
            if let Some(pos) = identity_str.find("\"name\"") {
                if let Some(colon) = identity_str[pos..].find(':') {
                    let rest = &identity_str[pos + colon + 1..];
                    if let Some(start_quote) = rest.find('"') {
                        let rest2 = &rest[start_quote + 1..];
                        if let Some(end_quote) = rest2.find('"') {
                            let name = &rest2[..end_quote];
                            if !name.trim().is_empty() {
                                return Some(PlayerIdentity {
                                    name: name.to_string(),
                                    profession: 0,
                                    map_id: 0,
                                    world_id: 0,
                                    team_color_id: 0,
                                    commander: false,
                                    fov: 0.0,
                                    ui_size: 0,
                                });
                            }
                        }
                    }
                }
            }

            log::trace!(
                "Failed to extract name from identity JSON: {}",
                identity_str
            );
            None
        }
    }
}

/// Generate a stable match key for a PvP instance. Hashes only the fields
/// that identify a specific match (server address, map id, instance id),
/// returning the first 16 hex chars. Intentionally excludes map_type and
/// shard_id so the key is stable across the duration of a single match.
fn generate_pvp_match_key(context: &MumbleContext) -> String {
    let mut hasher = Sha256::new();
    hasher.update(&context.server_address);
    hasher.update(context.map_id.to_le_bytes());
    hasher.update(context.instance.to_le_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8])
}

/// Generate a unique room key from MumbleLink context
fn generate_room_key(context: &MumbleContext) -> String {
    let mut hasher = Sha256::new();

    // Hash server address (identifies the map server)
    hasher.update(&context.server_address);

    // Hash map ID
    hasher.update(context.map_id.to_le_bytes());

    // Hash map type
    hasher.update(context.map_type.to_le_bytes());

    // Hash shard ID
    hasher.update(context.shard_id.to_le_bytes());

    // Hash instance
    hasher.update(context.instance.to_le_bytes());

    let result = hasher.finalize();
    hex::encode(&result[..16])
}
