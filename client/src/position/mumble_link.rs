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

/// Player state snapshot
#[derive(Debug, Clone)]
pub struct PlayerState {
    pub transform: Transform,
    pub camera_transform: Transform,
    pub identity: Option<PlayerIdentity>,
    pub room_key: String,
    pub map_id: u32,
    pub ui_tick: u32,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            transform: Transform::default(),
            camera_transform: Transform::default(),
            identity: None,
            room_key: String::new(),
            map_id: 0,
            ui_tick: 0,
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

        // Parse context
        let context = if (link.context_len as usize) >= std::mem::size_of::<MumbleContext>() {
            // Read the context as a possibly unaligned MumbleContext from the local copy
            unsafe { std::ptr::read_unaligned(link.context.as_ptr() as *const MumbleContext) }
        } else {
            MumbleContext::default()
        };

        let map_id = context.map_id;

        // Generate room key
        let room_key = generate_room_key(&context);

        // Log room changes
        if room_key != self.last_room_key && !room_key.is_empty() {
            log::info!("Room changed: {} -> {}", self.last_room_key, room_key);
            self.last_room_key = room_key.clone();
        }

        Some(PlayerState {
            transform,
            camera_transform,
            identity,
            room_key,
            map_id,
            ui_tick: link.ui_tick,
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

    // Log raw identity for debugging
    log::debug!("Raw identity JSON: {}", identity_str);

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

            log::trace!("Failed to extract name from identity JSON: {}", identity_str);
            None
        }
    }
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
