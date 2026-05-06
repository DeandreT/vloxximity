//! Given a MumbleLink snapshot and the user's voice settings,
//! decide which `map:`, `wvw-team:`, and `pvp-team:`
//! rooms the client should currently be in. The manager diffs these targets
//! against its `current_*_room` slots and issues the join/leave commands.
use crate::position::mumble_link::{is_pvp_map_type, MAP_TYPE_WVW_MAX, MAP_TYPE_WVW_MIN};
use crate::position::PlayerState;

use super::types::VoiceSettings;

/// Map-derived room id (`map:<key>`). `None` while the player isn't in game.
pub fn map_room_for(state: &PlayerState) -> Option<String> {
    if state.is_in_game() {
        Some(format!("map:{}", state.room_key))
    } else {
        None
    }
}

/// WvW team room id (`wvw-team:<world>-<team>`). `None` when not in game,
/// `auto_join_wvw_rooms` is off, identity is missing, `team_color_id == 0`,
/// or the current `map_type` is outside the WvW range.
pub fn wvw_team_room_for(state: &PlayerState, settings: &VoiceSettings) -> Option<String> {
    if !state.is_in_game() || !settings.auto_join_wvw_rooms {
        return None;
    }
    let id = state.identity.as_ref()?;
    if id.team_color_id == 0 {
        return None;
    }
    if !(MAP_TYPE_WVW_MIN..=MAP_TYPE_WVW_MAX).contains(&state.map_type) {
        return None;
    }
    Some(format!("wvw-team:{}-{}", id.world_id, id.team_color_id))
}

/// PvP team room id (`pvp-team:<match_key>-<team>`). `None` when not in
/// game, `auto_join_pvp_rooms` is off, identity is missing,
/// `team_color_id == 0`, the current `map_type` is not a PvP arena type,
/// or the MumbleLink snapshot doesn't carry a `pvp_match_key`.
pub fn pvp_team_room_for(state: &PlayerState, settings: &VoiceSettings) -> Option<String> {
    if !state.is_in_game() || !settings.auto_join_pvp_rooms {
        return None;
    }
    let id = state.identity.as_ref()?;
    if id.team_color_id == 0 || !is_pvp_map_type(state.map_type) {
        return None;
    }
    let match_key = state.pvp_match_key.as_ref()?;
    Some(format!("pvp-team:{}-{}", match_key, id.team_color_id))
}

/// Pull a non-empty player name out of the MumbleLink identity, falling
/// back to "Unknown" so room joins always have a display name. Returns the
/// raw (untrimmed) name when its trimmed form is non-empty, matching the
/// behavior the manager has shipped with.
pub fn player_name_from(state: &PlayerState) -> String {
    state
        .identity
        .as_ref()
        .and_then(|i| {
            if i.name.trim().is_empty() {
                None
            } else {
                Some(i.name.clone())
            }
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::mumble_link::{
        PlayerIdentity, MAP_TYPE_COMPETITIVE, MAP_TYPE_TOURNAMENT, MAP_TYPE_USER_TOURNAMENT,
    };

    fn in_game_state() -> PlayerState {
        PlayerState {
            ui_tick: 1,
            room_key: "abc".to_string(),
            ..PlayerState::default()
        }
    }

    fn identity_with(team_color_id: u32, world_id: u32, name: &str) -> PlayerIdentity {
        PlayerIdentity {
            name: name.to_string(),
            profession: 0,
            map_id: 0,
            world_id,
            team_color_id,
            commander: false,
            fov: 0.0,
            ui_size: 0,
        }
    }

    #[test]
    fn map_room_none_when_not_in_game() {
        let mut state = in_game_state();
        state.ui_tick = 0;
        assert_eq!(map_room_for(&state), None);
    }

    #[test]
    fn map_room_uses_room_key() {
        let state = in_game_state();
        assert_eq!(map_room_for(&state), Some("map:abc".to_string()));
    }

    #[test]
    fn wvw_room_at_map_type_boundaries() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(9, 1001, "Alice"));

        for outside in [
            MAP_TYPE_WVW_MIN - 1,
            MAP_TYPE_WVW_MAX + 1,
            MAP_TYPE_COMPETITIVE,
        ] {
            state.map_type = outside;
            assert_eq!(
                wvw_team_room_for(&state, &VoiceSettings::default()),
                None,
                "expected None at map_type={outside}"
            );
        }

        for inside in [MAP_TYPE_WVW_MIN, MAP_TYPE_WVW_MAX, 14] {
            state.map_type = inside;
            assert_eq!(
                wvw_team_room_for(&state, &VoiceSettings::default()),
                Some("wvw-team:1001-9".to_string()),
                "expected Some at map_type={inside}"
            );
        }
    }

    #[test]
    fn wvw_room_skipped_when_team_zero() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(0, 1001, "Alice"));
        state.map_type = MAP_TYPE_WVW_MIN;
        assert_eq!(wvw_team_room_for(&state, &VoiceSettings::default()), None);
    }

    #[test]
    fn wvw_room_skipped_when_identity_missing() {
        let mut state = in_game_state();
        state.identity = None;
        state.map_type = MAP_TYPE_WVW_MIN;
        assert_eq!(wvw_team_room_for(&state, &VoiceSettings::default()), None);
    }

    #[test]
    fn wvw_room_skipped_when_settings_disabled() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(9, 1001, "Alice"));
        state.map_type = MAP_TYPE_WVW_MIN;
        let settings = VoiceSettings {
            auto_join_wvw_rooms: false,
            ..VoiceSettings::default()
        };
        assert_eq!(wvw_team_room_for(&state, &settings), None);
    }

    #[test]
    fn wvw_room_skipped_when_not_in_game() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(9, 1001, "Alice"));
        state.map_type = MAP_TYPE_WVW_MIN;
        state.ui_tick = 0;
        assert_eq!(wvw_team_room_for(&state, &VoiceSettings::default()), None);
    }

    #[test]
    fn pvp_room_requires_match_key() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(5, 1, "Alice"));
        state.map_type = MAP_TYPE_COMPETITIVE;

        state.pvp_match_key = None;
        assert_eq!(pvp_team_room_for(&state, &VoiceSettings::default()), None);

        state.pvp_match_key = Some("xyz".to_string());
        assert_eq!(
            pvp_team_room_for(&state, &VoiceSettings::default()),
            Some("pvp-team:xyz-5".to_string())
        );
    }

    #[test]
    fn pvp_room_accepts_each_pvp_map_type() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(5, 1, "Alice"));
        state.pvp_match_key = Some("xyz".to_string());

        for pvp_map in [
            MAP_TYPE_COMPETITIVE,
            MAP_TYPE_TOURNAMENT,
            MAP_TYPE_USER_TOURNAMENT,
        ] {
            state.map_type = pvp_map;
            assert_eq!(
                pvp_team_room_for(&state, &VoiceSettings::default()),
                Some("pvp-team:xyz-5".to_string()),
                "expected Some at map_type={pvp_map}"
            );
        }
    }

    #[test]
    fn pvp_room_skipped_on_wvw_map() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(5, 1, "Alice"));
        state.map_type = MAP_TYPE_WVW_MIN;
        state.pvp_match_key = Some("xyz".to_string());
        assert_eq!(pvp_team_room_for(&state, &VoiceSettings::default()), None);
    }

    #[test]
    fn pvp_room_skipped_when_team_zero() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(0, 1, "Alice"));
        state.map_type = MAP_TYPE_COMPETITIVE;
        state.pvp_match_key = Some("xyz".to_string());
        assert_eq!(pvp_team_room_for(&state, &VoiceSettings::default()), None);
    }

    #[test]
    fn pvp_room_skipped_when_settings_disabled() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(5, 1, "Alice"));
        state.map_type = MAP_TYPE_COMPETITIVE;
        state.pvp_match_key = Some("xyz".to_string());
        let settings = VoiceSettings {
            auto_join_pvp_rooms: false,
            ..VoiceSettings::default()
        };
        assert_eq!(pvp_team_room_for(&state, &settings), None);
    }

    #[test]
    fn player_name_falls_back_to_unknown_when_missing() {
        let mut state = in_game_state();
        state.identity = None;
        assert_eq!(player_name_from(&state), "Unknown");
    }

    #[test]
    fn player_name_falls_back_to_unknown_when_blank() {
        let mut state = in_game_state();
        state.identity = Some(identity_with(0, 0, "   "));
        assert_eq!(player_name_from(&state), "Unknown");
    }

    #[test]
    fn player_name_returns_untrimmed_when_non_blank() {
        let mut state = in_game_state();
        // Untrimmed form is preserved — only the emptiness check trims.
        state.identity = Some(identity_with(0, 0, "  Alice  "));
        assert_eq!(player_name_from(&state), "  Alice  ");
    }
}
