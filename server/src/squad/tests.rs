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
        cluster.last_seen =
            Instant::now() - SQUAD_CLUSTER_TTL - std::time::Duration::from_secs(1);
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
        cluster.last_seen =
            Instant::now() - SQUAD_CLUSTER_TTL - std::time::Duration::from_secs(1);
    }
    // Even with full overlap, a TTL-expired cluster should not match
    // — sweep would drop it, and we don't want to resurrect it.
    let id2 = r.identify(members(&["A.1", "B.2", "C.3"]));
    assert_ne!(id, id2);
}
