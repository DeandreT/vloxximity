//! Per-type push-to-talk state and speak-room resolution.
//!
//! The keybind handler (called from the input thread) updates the held
//! flags + per-type press timestamps. Each outgoing-audio tick reads the
//! state and resolves which joined room to send the encoded frame into.
//!
//! Resolution rules (configured by the user in the design):
//! 1. If any *per-type* PTT is held, pick the type whose key was pressed
//!    most recently. Drop the frame if no joined room matches that type.
//! 2. Otherwise, if the default PTT is held (or we're in VoiceActivity /
//!    AlwaysOn mode), use the fallback chain:
//!    `squad > party > map`.
//! 3. If no PTT is held and the mode requires PTT, drop the frame.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::room_type::RoomType;

/// Cheap-to-clone handle to the shared key state. Cloned into the keybind
/// handler closures and into the voice manager.
#[derive(Clone)]
pub struct ActiveSpeak {
    state: Arc<Mutex<State>>,
}

struct State {
    default_held: bool,
    per_type: [KeyState; 4],
}

#[derive(Default, Clone, Copy)]
struct KeyState {
    held: bool,
    last_press: Option<Instant>,
}

fn type_index(ty: RoomType) -> usize {
    match ty {
        RoomType::Map => 0,
        RoomType::Squad => 1,
        RoomType::Party => 2,
        RoomType::WvwTeam => 3,
    }
}

impl ActiveSpeak {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                default_held: false,
                per_type: [KeyState::default(); 4],
            })),
        }
    }

    /// Update the default PTT held state (no specific type).
    pub fn set_default(&self, held: bool) {
        self.state.lock().default_held = held;
    }

    /// Update a per-type PTT held state; stamps the press timestamp on
    /// the rising edge so the resolver can break ties between multiple
    /// keys held at once.
    pub fn set_per_type(&self, ty: RoomType, held: bool) {
        let mut s = self.state.lock();
        let key = &mut s.per_type[type_index(ty)];
        if held && !key.held {
            key.last_press = Some(Instant::now());
        }
        key.held = held;
    }

    /// True if any PTT (default or per-type) is currently held.
    pub fn any_ptt_held(&self) -> bool {
        let s = self.state.lock();
        s.default_held || s.per_type.iter().any(|k| k.held)
    }

    /// Resolve the room to send audio into.
    ///
    /// `joined_rooms` maps room id → joined-at timestamp. `ptt_required`
    /// is true for `PushToTalk` mode and false for `VoiceActivity` /
    /// `AlwaysOn` (which transmit even with no key held).
    pub fn resolve(
        &self,
        joined_rooms: &HashMap<String, Instant>,
        ptt_required: bool,
    ) -> Option<String> {
        if joined_rooms.is_empty() {
            return None;
        }

        let s = self.state.lock();

        // 1. Per-type PTTs: pick the latest-pressed held key.
        let mut held: Vec<(RoomType, Instant)> = Vec::new();
        for (idx, ty) in [RoomType::Map, RoomType::Squad, RoomType::Party, RoomType::WvwTeam]
            .iter()
            .enumerate()
        {
            let key = &s.per_type[idx];
            if key.held {
                held.push((*ty, key.last_press.unwrap_or_else(Instant::now)));
            }
        }
        if !held.is_empty() {
            held.sort_by(|a, b| b.1.cmp(&a.1));
            // Strict route: per-type held = only that type, no fallback.
            // The user explicitly asked to talk into that channel.
            let target = held[0].0;
            return latest_room_of_type(joined_rooms, target);
        }

        // 2/3. Default held, or PTT not required → fallback chain.
        if ptt_required && !s.default_held {
            return None;
        }

        fallback_chain(joined_rooms)
    }
}

impl Default for ActiveSpeak {
    fn default() -> Self {
        Self::new()
    }
}

/// `squad > party > wvw-team > map`.
fn fallback_chain(joined_rooms: &HashMap<String, Instant>) -> Option<String> {
    if let Some(r) = latest_room_of_type(joined_rooms, RoomType::Squad) {
        return Some(r);
    }
    if let Some(r) = latest_room_of_type(joined_rooms, RoomType::Party) {
        return Some(r);
    }
    latest_room_of_type(joined_rooms, RoomType::WvwTeam)
        .or_else(|| latest_room_of_type(joined_rooms, RoomType::Map))
}

fn latest_room_of_type(
    joined_rooms: &HashMap<String, Instant>,
    target: RoomType,
) -> Option<String> {
    latest_with_time(joined_rooms, target).map(|(r, _)| r)
}

fn latest_with_time(
    joined_rooms: &HashMap<String, Instant>,
    target: RoomType,
) -> Option<(String, Instant)> {
    joined_rooms
        .iter()
        .filter(|(id, _)| RoomType::from_room_id(id) == Some(target))
        .max_by_key(|(_, t)| **t)
        .map(|(id, t)| (id.clone(), *t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn rooms(items: &[(&str, Instant)]) -> HashMap<String, Instant> {
        items.iter().map(|(k, t)| (k.to_string(), *t)).collect()
    }

    #[test]
    fn no_rooms_returns_none() {
        let a = ActiveSpeak::new();
        a.set_default(true);
        assert_eq!(a.resolve(&HashMap::new(), true), None);
    }

    #[test]
    fn ptt_required_no_key_returns_none() {
        let a = ActiveSpeak::new();
        let r = rooms(&[("map:a", Instant::now())]);
        assert_eq!(a.resolve(&r, true), None);
    }

    #[test]
    fn voice_activity_uses_fallback_without_ptt() {
        let a = ActiveSpeak::new();
        let r = rooms(&[("map:a", Instant::now())]);
        assert_eq!(a.resolve(&r, false), Some("map:a".to_string()));
    }

    #[test]
    fn default_ptt_routes_to_squad_first() {
        let a = ActiveSpeak::new();
        a.set_default(true);
        let now = Instant::now();
        let r = rooms(&[("map:a", now), ("squad:b", now), ("party:c", now)]);
        assert_eq!(a.resolve(&r, true), Some("squad:b".to_string()));
    }

    #[test]
    fn default_ptt_falls_back_to_party_before_map() {
        let a = ActiveSpeak::new();
        a.set_default(true);
        let now = Instant::now();
        let r = rooms(&[("map:m", now), ("party:p", now)]);
        assert_eq!(a.resolve(&r, true), Some("party:p".to_string()));
    }

    #[test]
    fn per_type_ptt_routes_strictly_to_that_type() {
        let a = ActiveSpeak::new();
        a.set_per_type(RoomType::Map, true);
        let now = Instant::now();
        let r = rooms(&[("map:a", now), ("squad:b", now)]);
        // Map-PTT routes to map, NOT to squad even though squad would
        // otherwise win the fallback.
        assert_eq!(a.resolve(&r, true), Some("map:a".to_string()));
    }

    #[test]
    fn per_type_ptt_no_matching_room_drops() {
        let a = ActiveSpeak::new();
        a.set_per_type(RoomType::Squad, true);
        let r = rooms(&[("map:a", Instant::now())]);
        assert_eq!(a.resolve(&r, true), None);
    }

    #[test]
    fn most_recently_pressed_per_type_wins() {
        let a = ActiveSpeak::new();
        a.set_per_type(RoomType::Map, true);
        sleep(Duration::from_millis(2));
        a.set_per_type(RoomType::Squad, true);
        let now = Instant::now();
        let r = rooms(&[("map:a", now), ("squad:b", now)]);
        assert_eq!(a.resolve(&r, true), Some("squad:b".to_string()));
    }

    #[test]
    fn release_clears_held_state() {
        let a = ActiveSpeak::new();
        a.set_per_type(RoomType::Squad, true);
        a.set_per_type(RoomType::Squad, false);
        let r = rooms(&[("squad:b", Instant::now())]);
        assert_eq!(a.resolve(&r, true), None);
    }
}
