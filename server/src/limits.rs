//! Server-wide abuse / lifecycle limits.
//!
//! Centralized so they're easy to find and tune. See the dead-connection
//! sweeper in `sweeper.rs` and the per-account cap in `rooms.rs`.

use std::time::Duration;

/// A connection idle for this long (no inbound WS message of any kind) is
/// considered dead and gets kicked. Generous because a "listening" client
/// still streams position updates, so genuine silence here means a stuck
/// or half-open socket.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often the sweeper walks the peer list looking for idle connections.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum concurrent authenticated WS connections per GW2 account.
/// Enforced after API-key validation; an account at the cap can't bind a
/// 5th connection until one of the existing ones disconnects (or is swept).
pub const MAX_CONNECTIONS_PER_ACCOUNT: usize = 4;

/// Cap on `IdentifyGroup.members` length. GW2's largest group is a 50-peer
/// open-world squad; 60 is a small headroom over that.
pub const MAX_GROUP_REPORT_MEMBERS: usize = 60;

/// Squad clusters that haven't received a fresh `IdentifyGroup` report
/// within this window are dropped by the sweeper. Long enough to cover
/// short downtime (loading screens, brief afk) without indefinitely
/// pinning identifiers.
pub const SQUAD_CLUSTER_TTL: Duration = Duration::from_secs(10 * 60);
