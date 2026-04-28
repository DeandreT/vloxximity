//! Dead-connection sweeper.
//!
//! Walks the registered peers on a fixed interval and signals any whose
//! `last_seen` has aged past `IDLE_TIMEOUT`. The session loop watches
//! `peer.kick.notified()` alongside its WS receiver and breaks out cleanly
//! when notified, which triggers the existing `unregister_peer` path.
//!
//! The same task also ages out stale entries in the squad registry — it's
//! cheap, runs at the same cadence, and folding it in here avoids a second
//! background task with overlapping responsibilities.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};

use crate::limits::{IDLE_TIMEOUT, SWEEP_INTERVAL};
use crate::rooms::RoomManager;
use crate::squad::SquadRegistry;

/// Spawn the sweeper onto the current tokio runtime. Uses the configured
/// `IDLE_TIMEOUT` and `SWEEP_INTERVAL` constants.
pub fn spawn_sweeper(rooms: Arc<RoomManager>, squads: Arc<SquadRegistry>) {
    tokio::spawn(sweeper_loop(rooms, squads, IDLE_TIMEOUT, SWEEP_INTERVAL));
}

async fn sweeper_loop(
    rooms: Arc<RoomManager>,
    squads: Arc<SquadRegistry>,
    idle_timeout: Duration,
    interval: Duration,
) {
    let mut ticker = time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        sweep_once(&rooms, idle_timeout);
        squads.sweep();
    }
}

fn sweep_once(rooms: &RoomManager, idle_timeout: Duration) {
    for (peer_id, last_seen, kick) in rooms.peer_liveness_handles() {
        let elapsed = match last_seen.lock() {
            Ok(guard) => guard.elapsed(),
            Err(poisoned) => poisoned.into_inner().elapsed(),
        };
        if elapsed > idle_timeout {
            tracing::info!(
                "Sweeper kicking idle peer {} (idle for {:?})",
                peer_id,
                elapsed
            );
            kick.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::RoomManager;
    use std::time::Instant;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn sweeper_kicks_idle_peer() {
        let rooms = RoomManager::new();
        let (tx, _rx) = broadcast::channel(8);
        let registered = rooms.register_peer(tx);

        // Backdate last_seen so the peer is "idle".
        {
            let mut guard = registered.last_seen.lock().unwrap();
            *guard = Instant::now() - Duration::from_secs(60);
        }

        let kick_listener = registered.kick.clone();
        let notified = tokio::spawn(async move { kick_listener.notified().await });

        sweep_once(&rooms, Duration::from_secs(1));

        // The kick must be observable.
        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("kick should fire within timeout")
            .expect("notified task panicked");
    }

    #[tokio::test]
    async fn sweeper_leaves_fresh_peer_alone() {
        let rooms = RoomManager::new();
        let (tx, _rx) = broadcast::channel(8);
        let registered = rooms.register_peer(tx);

        let kick_listener = registered.kick.clone();
        let notified = tokio::spawn(async move { kick_listener.notified().await });

        sweep_once(&rooms, Duration::from_secs(60));

        // Fresh peer; no kick should fire.
        let res = tokio::time::timeout(Duration::from_millis(50), notified).await;
        assert!(res.is_err(), "fresh peer must not be kicked");
    }
}
