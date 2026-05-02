//! WvW match lookup and team membership verification.
//!
//! When a peer tries to join a `wvw-team:<world_id>-<team_color_id>` room the
//! server verifies two things:
//!
//! 1. The claimed `world_id` matches the peer's API-key-verified home world.
//! 2. The claimed `team_color_id` matches the team that world is actually on
//!    in the current matchup, according to `GET /v2/wvw/matches?world=<id>`.
//!
//! Results are cached per world-id to avoid hammering the GW2 API on every
//! room join during an active WvW session.

use dashmap::DashMap;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a world→team mapping is trusted before we re-query.
/// Matchups rotate weekly, but linked-world assignments can change more
/// often; 5 minutes keeps us reasonably fresh without spamming the API.
const MATCH_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// HTTP timeout for the WvW matches API call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// MumbleLink `team_color_id` values for each WvW team.
/// These map to GW2 dye color IDs; values sourced from community
/// documentation and should be verified against live game data.
const COLOR_ID_RED: u32 = 9;
const COLOR_ID_BLUE: u32 = 5;
const COLOR_ID_GREEN: u32 = 23;

/// The three WvW team colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WvwTeam {
    Red,
    Blue,
    Green,
}

impl WvwTeam {
    /// Map a MumbleLink `team_color_id` to a team. Returns `None` for unknown
    /// color IDs (e.g. 0 = no team, or an undocumented value).
    pub fn from_color_id(id: u32) -> Option<Self> {
        match id {
            COLOR_ID_RED => Some(WvwTeam::Red),
            COLOR_ID_BLUE => Some(WvwTeam::Blue),
            COLOR_ID_GREEN => Some(WvwTeam::Green),
            _ => None,
        }
    }

    /// The team name as used in the GW2 WvW matches API response.
    pub fn as_str(self) -> &'static str {
        match self {
            WvwTeam::Red => "red",
            WvwTeam::Blue => "blue",
            WvwTeam::Green => "green",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedMatchTeam {
    pub team: WvwTeam,
    pub cached_at: Instant,
}

/// Thread-safe cache of world_id → WvW team. Keyed by world ID directly
/// (not hashed — world IDs are small non-sensitive integers).
pub type WvwMatchCache = Arc<DashMap<u32, CachedMatchTeam>>;

pub fn new_match_cache() -> WvwMatchCache {
    Arc::new(DashMap::new())
}

/// Verify that `world_id` is on `claimed_team` in the current WvW matchup.
///
/// Returns `Ok(())` when the claim is correct, or `Err(reason)` with a
/// user-visible message when:
/// - the GW2 API is unreachable (fail-closed)
/// - the world is not found in any team's `all_worlds` list
/// - the world is on a different team than claimed
pub async fn verify_team(
    client: &reqwest::Client,
    cache: &WvwMatchCache,
    world_id: u32,
    claimed_team: WvwTeam,
) -> Result<(), String> {
    // Serve from cache if fresh.
    if let Some(entry) = cache.get(&world_id) {
        if entry.cached_at.elapsed() < MATCH_CACHE_TTL {
            return check_team(entry.team, claimed_team);
        }
    }

    let actual_team = fetch_world_team(client, world_id).await?;
    cache.insert(
        world_id,
        CachedMatchTeam {
            team: actual_team,
            cached_at: Instant::now(),
        },
    );
    check_team(actual_team, claimed_team)
}

fn check_team(actual: WvwTeam, claimed: WvwTeam) -> Result<(), String> {
    if actual == claimed {
        Ok(())
    } else {
        Err("your world is not on that WvW team".to_string())
    }
}

/// Fetch which team `world_id` is on from the GW2 WvW matches API.
/// Searches `all_worlds.{red,blue,green}` so linked worlds are handled
/// automatically (the API includes both host and linked worlds in each list).
async fn fetch_world_team(client: &reqwest::Client, world_id: u32) -> Result<WvwTeam, String> {
    let url = format!(
        "https://api.guildwars2.com/v2/wvw/matches?world={}",
        world_id
    );

    let response = client
        .get(&url)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(
                "WvW matches API request failed for world {}: {}",
                world_id,
                e
            );
            "WvW team could not be verified — try again shortly".to_string()
        })?;

    if !response.status().is_success() {
        tracing::warn!(
            "WvW matches API returned {} for world {}",
            response.status(),
            world_id
        );
        return Err("WvW team could not be verified — try again shortly".to_string());
    }

    let body = response.json::<MatchResponse>().await.map_err(|e| {
        tracing::warn!("WvW matches API parse failed for world {}: {}", world_id, e);
        "WvW team could not be verified — try again shortly".to_string()
    })?;

    let worlds = &body.all_worlds;
    if worlds.red.contains(&world_id) {
        Ok(WvwTeam::Red)
    } else if worlds.blue.contains(&world_id) {
        Ok(WvwTeam::Blue)
    } else if worlds.green.contains(&world_id) {
        Ok(WvwTeam::Green)
    } else {
        Err("your world is not on that WvW team".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct MatchResponse {
    all_worlds: MatchWorlds,
}

#[derive(Debug, Deserialize)]
struct MatchWorlds {
    red: Vec<u32>,
    blue: Vec<u32>,
    green: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_color_id_round_trips() {
        assert_eq!(WvwTeam::from_color_id(COLOR_ID_RED), Some(WvwTeam::Red));
        assert_eq!(WvwTeam::from_color_id(COLOR_ID_BLUE), Some(WvwTeam::Blue));
        assert_eq!(WvwTeam::from_color_id(COLOR_ID_GREEN), Some(WvwTeam::Green));
        assert_eq!(WvwTeam::from_color_id(0), None);
        assert_eq!(WvwTeam::from_color_id(999), None);
    }

    #[test]
    fn as_str_matches_api_names() {
        assert_eq!(WvwTeam::Red.as_str(), "red");
        assert_eq!(WvwTeam::Blue.as_str(), "blue");
        assert_eq!(WvwTeam::Green.as_str(), "green");
    }

    #[tokio::test]
    async fn cache_hit_short_circuits_http() {
        let cache = new_match_cache();
        let client = reqwest::Client::new();
        // Prime cache for world 1001 → Red.
        cache.insert(
            1001,
            CachedMatchTeam {
                team: WvwTeam::Red,
                cached_at: Instant::now(),
            },
        );
        // Correct team succeeds without hitting the network.
        assert!(verify_team(&client, &cache, 1001, WvwTeam::Red)
            .await
            .is_ok());
        // Wrong team fails immediately from cache.
        assert!(verify_team(&client, &cache, 1001, WvwTeam::Blue)
            .await
            .is_err());
    }
}
