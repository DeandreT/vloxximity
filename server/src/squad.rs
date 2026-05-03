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
mod tests;
