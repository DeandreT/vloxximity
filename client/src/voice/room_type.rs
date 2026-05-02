//! Client-side categorization of room ids.
//!
//! Room ids on the wire are opaque to the server. Clients agree on a
//! `<type>:<rest>` prefix scheme so per-type volume, spatial mode, and
//! speak-room selection can be derived locally.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomType {
    Map,
    Squad,
    Party,
    WvwTeam,
    PvpTeam,
}

impl RoomType {
    /// Parse the type prefix from a room id. Returns `None` for unknown
    /// prefixes — the UI rejects those before sending a join.
    pub fn from_room_id(room_id: &str) -> Option<Self> {
        let (prefix, _) = room_id.split_once(':')?;
        match prefix {
            "map" => Some(RoomType::Map),
            "squad" => Some(RoomType::Squad),
            "party" => Some(RoomType::Party),
            "wvw-team" => Some(RoomType::WvwTeam),
            "pvp-team" => Some(RoomType::PvpTeam),
            _ => None,
        }
    }

    /// Human-readable label for UI display.
    pub fn label(self) -> &'static str {
        match self {
            RoomType::Map => "Map",
            RoomType::Squad => "Squad",
            RoomType::Party => "Party",
            RoomType::WvwTeam => "WvW Team",
            RoomType::PvpTeam => "PvP Team",
        }
    }
}

/// Per-room-type volume gains. Stored as a flat struct (not a `HashMap`)
/// so the JSON form stays a small object with stable field names rather
/// than an array of tuples — keeps `settings.json` human-readable and
/// migration-friendly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct RoomTypeVolumes {
    pub map: f32,
    pub squad: f32,
    pub party: f32,
    pub wvw_team: f32,
    #[serde(default = "default_volume")]
    pub pvp_team: f32,
}

fn default_volume() -> f32 {
    1.0
}

impl Default for RoomTypeVolumes {
    fn default() -> Self {
        Self {
            map: 1.0,
            squad: 1.0,
            party: 1.0,
            wvw_team: 1.0,
            pvp_team: 1.0,
        }
    }
}

impl RoomTypeVolumes {
    pub fn get(&self, ty: RoomType) -> f32 {
        match ty {
            RoomType::Map => self.map,
            RoomType::Squad => self.squad,
            RoomType::Party => self.party,
            RoomType::WvwTeam => self.wvw_team,
            RoomType::PvpTeam => self.pvp_team,
        }
    }

    pub fn set(&mut self, ty: RoomType, value: f32) {
        let v = value.clamp(0.0, 2.0);
        match ty {
            RoomType::Map => self.map = v,
            RoomType::Squad => self.squad = v,
            RoomType::Party => self.party = v,
            RoomType::WvwTeam => self.wvw_team = v,
            RoomType::PvpTeam => self.pvp_team = v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_prefixes() {
        assert_eq!(RoomType::from_room_id("map:abc"), Some(RoomType::Map));
        assert_eq!(RoomType::from_room_id("squad:xyz"), Some(RoomType::Squad));
        assert_eq!(RoomType::from_room_id("party:1"), Some(RoomType::Party));
        assert_eq!(
            RoomType::from_room_id("wvw-team:1234-9"),
            Some(RoomType::WvwTeam)
        );
        assert_eq!(
            RoomType::from_room_id("pvp-team:aabbccdd-5"),
            Some(RoomType::PvpTeam)
        );
    }

    #[test]
    fn rejects_unknown_or_missing_prefix() {
        assert_eq!(RoomType::from_room_id("voice:room"), None);
        assert_eq!(RoomType::from_room_id("nopref"), None);
        assert_eq!(RoomType::from_room_id(""), None);
    }

    #[test]
    fn volumes_default_to_unity() {
        let v = RoomTypeVolumes::default();
        assert_eq!(v.get(RoomType::Map), 1.0);
        assert_eq!(v.get(RoomType::Squad), 1.0);
        assert_eq!(v.get(RoomType::Party), 1.0);
        assert_eq!(v.get(RoomType::WvwTeam), 1.0);
        assert_eq!(v.get(RoomType::PvpTeam), 1.0);
    }

    #[test]
    fn volumes_clamp_on_set() {
        let mut v = RoomTypeVolumes::default();
        v.set(RoomType::Map, 5.0);
        assert_eq!(v.get(RoomType::Map), 2.0);
        v.set(RoomType::Squad, -1.0);
        assert_eq!(v.get(RoomType::Squad), 0.0);
    }

    #[test]
    fn volumes_serde_partial_object() {
        // Settings JSON written by an older client may omit the field.
        // Defaults must apply.
        let json = r#"{}"#;
        let parsed: RoomTypeVolumes = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.map, 1.0);
        assert_eq!(parsed.squad, 1.0);
        assert_eq!(parsed.wvw_team, 1.0);
        assert_eq!(parsed.pvp_team, 1.0);
    }
}
