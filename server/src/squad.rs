//! Server-side squad/group identity clustering.
//!
//! GW2 has no native squad UID and the addon's RTAPI only exposes per-member
//! events, so deriving a stable squad room id purely client-side breaks for
//! late joiners (members who never saw the original commander). The server
//! solves this by clustering overlapping member-list reports: when a peer
//! sends `IdentifyGroup { members }`, the registry returns the id of the
//! existing cluster that overlaps significantly with the report, or mints a
//! fresh one. All peers in the same GW2 group converge on the same cluster
//! id even as members rotate in and out.
//!
use dashmap::DashMap;
use std::collections::HashSet;
use std::time::Instant;
use uuid::Uuid;

use crate::limits::SQUAD_CLUSTER_TTL;

/// Minimum number of shared account names required to merge a fresh report
/// into an existing cluster. Two prevents a single shared player from
/// accidentally fusing two unrelated groups; it also means a party of two
/// can't cluster at all (fresh id every time), which is acceptable —
/// clustering is mainly useful at squad scale.
const MIN_OVERLAP: usize = 2;

#[derive(Debug, Clone)]
struct Cluster {
    id: String,
    members: HashSet<String>,
    last_seen: Instant,
}

/// In-memory registry of active GW2 group clusters.
///
/// Cheap to share between the session handler and the sweeper via `Arc`.
/// All operations are constant or linear in the number of clusters; with
/// only thousands of concurrent groups this stays trivial.
pub struct SquadRegistry {
    clusters: DashMap<String, Cluster>,
}

impl SquadRegistry {
    pub fn new() -> Self {
        Self {
            clusters: DashMap::new(),
        }
    }

    /// Match `report` against existing clusters; return the chosen cluster
    /// id. If a cluster overlaps by at least [`MIN_OVERLAP`] members, its
    /// member set is replaced with the latest snapshot and its
    /// `last_seen` refreshed. Otherwise a fresh cluster is created.
    pub fn identify(&self, report: HashSet<String>) -> String {
        // Linear scan is fine at the scale we operate (thousands of
        // groups max). Each comparison is `O(|report|)` because we
        // probe `cluster.members` (a `HashSet`).
        let now = Instant::now();
        let mut best: Option<(String, usize)> = None;
        for entry in self.clusters.iter() {
            let cluster = entry.value();
            if now.duration_since(cluster.last_seen) > SQUAD_CLUSTER_TTL {
                continue;
            }
            let overlap = report
                .iter()
                .filter(|name| cluster.members.contains(name.as_str()))
                .count();
            if overlap < MIN_OVERLAP {
                continue;
            }
            if best.as_ref().map_or(true, |(_, o)| overlap > *o) {
                best = Some((cluster.id.clone(), overlap));
            }
        }

        if let Some((id, _)) = best {
            if let Some(mut cluster) = self.clusters.get_mut(&id) {
                cluster.members = report;
                cluster.last_seen = now;
                return id;
            }
            // Cluster vanished between iter and get_mut (extremely unlikely
            // — we don't expose external removal except via sweep). Fall
            // through and mint a new id.
        }

        let id = Uuid::new_v4().to_string();
        self.clusters.insert(
            id.clone(),
            Cluster {
                id: id.clone(),
                members: report,
                last_seen: now,
            },
        );
        id
    }

    /// Drop clusters whose `last_seen` is older than [`SQUAD_CLUSTER_TTL`].
    pub fn sweep(&self) {
        let now = Instant::now();
        let stale: Vec<String> = self
            .clusters
            .iter()
            .filter(|entry| now.duration_since(entry.value().last_seen) > SQUAD_CLUSTER_TTL)
            .map(|entry| entry.key().clone())
            .collect();
        for id in stale {
            self.clusters.remove(&id);
        }
    }

    /// Number of live clusters. Used by the sweeper logs.
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }
}

impl Default for SquadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fresh_report_creates_new_cluster() {
        let r = SquadRegistry::new();
        let id = r.identify(members(&["A.1", "B.2", "C.3"]));
        assert!(!id.is_empty());
        assert_eq!(r.cluster_count(), 1);
    }

    #[test]
    fn overlapping_report_matches_existing_cluster() {
        let r = SquadRegistry::new();
        let id1 = r.identify(members(&["A.1", "B.2", "C.3"]));
        // Two members in common: matches.
        let id2 = r.identify(members(&["A.1", "B.2", "D.4"]));
        assert_eq!(id1, id2, "overlap of 2 must reuse cluster");
        assert_eq!(r.cluster_count(), 1);
    }

    #[test]
    fn single_member_overlap_does_not_match() {
        let r = SquadRegistry::new();
        let id1 = r.identify(members(&["A.1", "B.2", "C.3"]));
        // Only one shared member: must not merge.
        let id2 = r.identify(members(&["A.1", "X.9", "Y.8"]));
        assert_ne!(id1, id2, "overlap of 1 must allocate a fresh cluster");
        assert_eq!(r.cluster_count(), 2);
    }

    #[test]
    fn cluster_member_set_is_replaced_with_latest() {
        let r = SquadRegistry::new();
        let _ = r.identify(members(&["A.1", "B.2", "C.3"]));
        // Late joiner: original commander A.1 has left; new member D.4 joined.
        // Reports {B.2, C.3, D.4} — overlap 2 with the existing cluster, so
        // it merges. After this call, the cluster's authoritative member
        // set should be exactly the latest report.
        let id2 = r.identify(members(&["B.2", "C.3", "D.4"]));
        // A new fully-disjoint join from someone who only overlaps with
        // *D.4* now (not the original A.1) must still match — proves the
        // member set rolled forward.
        let id3 = r.identify(members(&["C.3", "D.4", "E.5"]));
        assert_eq!(id2, id3);
    }

    #[test]
    fn picks_cluster_with_highest_overlap() {
        let r = SquadRegistry::new();
        let id_left = r.identify(members(&["A.1", "B.2", "C.3", "D.4"]));
        let id_right = r.identify(members(&["X.1", "Y.2", "Z.3", "W.4"]));
        assert_ne!(id_left, id_right);
        // Overlaps with `id_left` by 3 (A,B,C) and with `id_right` by 0.
        let merged = r.identify(members(&["A.1", "B.2", "C.3", "X.1"]));
        // Wait — that's 1 overlap with id_right; still strictly less than 3.
        assert_eq!(merged, id_left);
    }

    #[test]
    fn sweep_drops_stale_clusters() {
        let r = SquadRegistry::new();
        let id = r.identify(members(&["A.1", "B.2", "C.3"]));
        // Forge a stale `last_seen` on the cluster directly.
        if let Some(mut cluster) = r.clusters.get_mut(&id) {
            cluster.last_seen = Instant::now() - SQUAD_CLUSTER_TTL - std::time::Duration::from_secs(1);
        }
        assert_eq!(r.cluster_count(), 1);
        r.sweep();
        assert_eq!(r.cluster_count(), 0);
    }

    #[test]
    fn sweep_keeps_fresh_clusters() {
        let r = SquadRegistry::new();
        let _ = r.identify(members(&["A.1", "B.2", "C.3"]));
        r.sweep();
        assert_eq!(r.cluster_count(), 1, "fresh cluster must survive sweep");
    }

    #[test]
    fn ttl_expired_cluster_does_not_merge() {
        let r = SquadRegistry::new();
        let id = r.identify(members(&["A.1", "B.2", "C.3"]));
        if let Some(mut cluster) = r.clusters.get_mut(&id) {
            cluster.last_seen = Instant::now() - SQUAD_CLUSTER_TTL - std::time::Duration::from_secs(1);
        }
        // Even with full overlap, a TTL-expired cluster should not match
        // — sweep would drop it, and we don't want to resurrect it.
        let id2 = r.identify(members(&["A.1", "B.2", "C.3"]));
        assert_ne!(id, id2);
    }
}
