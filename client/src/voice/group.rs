//! Local cache of the GW2 group as observed via Nexus RTAPI events.
//!
//! RTAPI exposes per-member events (`RTAPI_GROUP_MEMBER_JOINED/LEFT/UPDATE`)
//! with no polling endpoint for the member list, so we maintain state from
//! the event stream and periodically forward a snapshot to the server for
//! clustering. Group *shape* (`None` / `Party` / `Squad`) comes from the
//! polled RTAPI group snapshot in the manager update loop.

use std::collections::HashMap;
use nexus::rtapi::GroupType;

/// Subset of `nexus::rtapi::GroupMember` we keep — only the fields the
/// suggestion logic needs. Storing owned strings (vs the FFI struct's
/// fixed-size char arrays) keeps the rest of the codebase free of nexus
/// types.
#[derive(Debug, Clone)]
pub struct GroupMemberSnapshot {
    pub account_name: String,
    pub character_name: String,
    pub subgroup: u32,
    pub is_self: bool,
    pub is_commander: bool,
}

/// Differentiates the shapes the local user might be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    None,
    Party,
    Squad,
}

impl GroupKind {
    pub fn from_rtapi(group_type: GroupType, member_count: u32) -> Self {
        match group_type {
            GroupType::None => GroupKind::None,
            GroupType::Party => {
                if member_count >= 2 {
                    GroupKind::Party
                } else {
                    GroupKind::None
                }
            }
            GroupType::RaidSquad | GroupType::Squad => GroupKind::Squad,
        }
    }
}

/// Kind of event we received from RTAPI. The dispatcher converts the raw
/// FFI struct into one of these and hands it to the manager.
#[derive(Debug, Clone)]
pub enum GroupMemberEvent {
    Joined(GroupMemberSnapshot),
    Updated(GroupMemberSnapshot),
    Left {
        account_name: String,
    },
}

/// Local mirror of the player's RTAPI group. Keyed by account name.
#[derive(Debug, Default)]
pub struct GroupState {
    members: HashMap<String, GroupMemberSnapshot>,
}

impl GroupState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an RTAPI event to the cache. Returns `true` if the cache
    /// actually changed (caller uses this to decide whether to send a
    /// fresh `IdentifyGroup` to the server).
    pub fn apply(&mut self, event: GroupMemberEvent) -> bool {
        match event {
            GroupMemberEvent::Joined(snap) | GroupMemberEvent::Updated(snap) => {
                let key = snap.account_name.clone();
                let prev = self.members.insert(key, snap);
                match prev {
                    Some(old) => {
                        // Updates that don't change anything we care about
                        // shouldn't trigger a re-identify. Only return
                        // true when the relevant fields differ.
                        let new = self
                            .members
                            .get(&old.account_name)
                            .expect("just inserted");
                        old.subgroup != new.subgroup
                            || old.is_commander != new.is_commander
                            || old.character_name != new.character_name
                    }
                    None => true,
                }
            }
            GroupMemberEvent::Left { account_name } => {
                self.members.remove(&account_name).is_some()
            }
        }
    }

    /// Snapshot of all member account names (sorted for stable ordering
    /// in logs / tests).
    pub fn member_account_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.members.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Local player's snapshot, if RTAPI has reported them yet.
    pub fn own_member(&self) -> Option<&GroupMemberSnapshot> {
        self.members.values().find(|m| m.is_self)
    }

    /// Account name of the current commander, if any.
    pub fn commander_name(&self) -> Option<&str> {
        self.members
            .values()
            .find(|m| m.is_commander)
            .map(|m| m.account_name.as_str())
    }

    /// Drop every cached member. Used when RTAPI reports the local player
    /// has left the group (e.g., disbanded the squad).
    pub fn clear(&mut self) {
        self.members.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(account: &str, subgroup: u32, is_self: bool, is_cmd: bool) -> GroupMemberSnapshot {
        GroupMemberSnapshot {
            account_name: account.to_string(),
            character_name: format!("char_{}", account),
            subgroup,
            is_self,
            is_commander: is_cmd,
        }
    }

    #[test]
    fn join_then_leave_cycles_cache() {
        let mut g = GroupState::new();
        assert!(g.apply(GroupMemberEvent::Joined(snap("A.1", 1, true, true))));
        assert_eq!(g.member_count(), 1);
        assert_eq!(g.commander_name(), Some("A.1"));
        assert!(g.apply(GroupMemberEvent::Left { account_name: "A.1".into() }));
        assert_eq!(g.member_count(), 0);
        assert_eq!(g.commander_name(), None);
    }

    #[test]
    fn duplicate_left_returns_false() {
        let mut g = GroupState::new();
        let _ = g.apply(GroupMemberEvent::Joined(snap("A.1", 1, true, false)));
        assert!(g.apply(GroupMemberEvent::Left { account_name: "A.1".into() }));
        assert!(!g.apply(GroupMemberEvent::Left { account_name: "A.1".into() }));
    }

    #[test]
    fn no_op_update_returns_false() {
        let mut g = GroupState::new();
        let s = snap("A.1", 1, true, false);
        assert!(g.apply(GroupMemberEvent::Joined(s.clone())));
        // Identical update — nothing relevant changed.
        assert!(!g.apply(GroupMemberEvent::Updated(s)));
    }

    #[test]
    fn subgroup_change_returns_true() {
        let mut g = GroupState::new();
        let _ = g.apply(GroupMemberEvent::Joined(snap("A.1", 1, true, false)));
        assert!(g.apply(GroupMemberEvent::Updated(snap("A.1", 3, true, false))));
        assert_eq!(g.own_member().map(|m| m.subgroup), Some(3));
    }

    #[test]
    fn commander_change_returns_true() {
        let mut g = GroupState::new();
        let _ = g.apply(GroupMemberEvent::Joined(snap("A.1", 0, true, false)));
        assert!(g.apply(GroupMemberEvent::Updated(snap("A.1", 0, true, true))));
        assert_eq!(g.commander_name(), Some("A.1"));
    }

    #[test]
    fn rtapi_none_maps_to_none() {
        assert_eq!(GroupKind::from_rtapi(GroupType::None, 0), GroupKind::None);
    }

    #[test]
    fn rtapi_party_of_one_maps_to_none() {
        assert_eq!(GroupKind::from_rtapi(GroupType::Party, 1), GroupKind::None);
    }

    #[test]
    fn rtapi_party_maps_to_party() {
        assert_eq!(GroupKind::from_rtapi(GroupType::Party, 2), GroupKind::Party);
    }

    #[test]
    fn rtapi_squads_map_to_squad() {
        assert_eq!(GroupKind::from_rtapi(GroupType::Squad, 1), GroupKind::Squad);
        assert_eq!(GroupKind::from_rtapi(GroupType::RaidSquad, 1), GroupKind::Squad);
    }

    #[test]
    fn member_account_names_are_sorted() {
        let mut g = GroupState::new();
        let _ = g.apply(GroupMemberEvent::Joined(snap("Charlie.3", 0, false, false)));
        let _ = g.apply(GroupMemberEvent::Joined(snap("Alice.1", 0, true, true)));
        let _ = g.apply(GroupMemberEvent::Joined(snap("Bob.2", 0, false, false)));
        assert_eq!(
            g.member_account_names(),
            vec!["Alice.1", "Bob.2", "Charlie.3"]
        );
    }
}
