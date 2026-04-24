//! GW2 REST API identity validation.
//!
//! A peer's client sends its API key on `JoinRoom`; the server calls
//! `/v2/account` with the key and extracts the stable account handle (e.g.
//! `Example.1234`). Returning that as the canonical identity is what makes
//! anti-spoof persistent mutes possible — clients cannot self-report a
//! name, because the server only broadcasts what GW2 attests to.
//!
//! Lookups are cached with a TTL so room rejoin churn doesn't re-query the
//! GW2 API. The cache is keyed by API key hash to keep raw keys out of the
//! process image where possible.

use dashmap::DashMap;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a validated (key → account) mapping is trusted before we hit
/// the GW2 API again. 60 minutes balances rejoin churn against handling
/// key rotations reasonably quickly.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// HTTP timeout for the GW2 API call. Short enough that an unresponsive
/// network doesn't stall room joins indefinitely.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// Thread-safe cache of validated API keys. Keys are stored as SHA-256
/// hashes to minimise the time the raw key sits in heap memory beyond the
/// single request that validated it.
pub type Gw2Cache = Arc<DashMap<String, CachedAccount>>;

#[derive(Debug, Clone)]
pub struct CachedAccount {
    pub account_name: Option<String>,
    pub validated_at: Instant,
}

pub fn new_cache() -> Gw2Cache {
    Arc::new(DashMap::new())
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Validate a GW2 API key against `https://api.guildwars2.com/v2/account`.
/// Returns the account handle (e.g. `Example.1234`) on success, or `None`
/// when the key is invalid, the API is unreachable, or the cache says it
/// was recently invalid.
pub async fn validate_api_key(
    client: &reqwest::Client,
    cache: &Gw2Cache,
    api_key: &str,
) -> Option<String> {
    let hash = hash_key(api_key);

    // Serve from cache if the entry is still fresh.
    if let Some(entry) = cache.get(&hash) {
        if entry.validated_at.elapsed() < CACHE_TTL {
            return entry.account_name.clone();
        }
    }

    let account_name = fetch_account_name(client, api_key).await;
    cache.insert(
        hash,
        CachedAccount {
            account_name: account_name.clone(),
            validated_at: Instant::now(),
        },
    );
    account_name
}

#[derive(Debug, Deserialize)]
struct AccountResponse {
    name: String,
}

async fn fetch_account_name(client: &reqwest::Client, api_key: &str) -> Option<String> {
    let url = "https://api.guildwars2.com/v2/account";
    let response = match client
        .get(url)
        .bearer_auth(api_key)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("GW2 API request failed: {}", e);
            return None;
        }
    };

    if !response.status().is_success() {
        tracing::warn!("GW2 API rejected key (status {})", response.status());
        return None;
    }

    match response.json::<AccountResponse>().await {
        Ok(body) => Some(body.name),
        Err(e) => {
            tracing::warn!("GW2 API response parse failed: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_hit_short_circuits_http() {
        // Prime the cache with a fake entry. A subsequent `validate_api_key`
        // call with the same key must return the cached value without
        // touching the network — we pass a default reqwest client that
        // would otherwise try to resolve a real hostname.
        let cache = new_cache();
        let client = reqwest::Client::new();
        let key = "test-key-abc123";
        let hash = hash_key(key);
        cache.insert(
            hash.clone(),
            CachedAccount {
                account_name: Some("Cached.0001".to_string()),
                validated_at: Instant::now(),
            },
        );

        let got = validate_api_key(&client, &cache, key).await;
        assert_eq!(got.as_deref(), Some("Cached.0001"));

        // Negative-cache entries are also honoured: asking again for a key
        // we previously determined invalid returns None without re-hitting
        // the network.
        let bad_key = "bad-key";
        cache.insert(
            hash_key(bad_key),
            CachedAccount {
                account_name: None,
                validated_at: Instant::now(),
            },
        );
        assert_eq!(validate_api_key(&client, &cache, bad_key).await, None);
    }

    #[test]
    fn hash_key_is_deterministic() {
        assert_eq!(hash_key("abc"), hash_key("abc"));
        assert_ne!(hash_key("abc"), hash_key("abcd"));
    }
}
