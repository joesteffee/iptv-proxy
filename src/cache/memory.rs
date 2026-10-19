//! In-process cache for Xtream API responses.
//!
//! The Go version kept these in Redis only. Without `REDIS_URL` that meant a cold run re-fetched
//! every category page and every `get_series_info` call for *each* playlist download — thousands of
//! round trips. This cache mirrors the same TTLs in memory, so the second playlist within the TTL
//! costs nothing, with or without Redis.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Entries older than this are dropped on the next lookup regardless of their own TTL, which keeps
/// the map from growing without bound when clients pass odd `output` values.
const HARD_LIMIT: Duration = Duration::from_secs(60 * 60 * 24);

#[derive(Clone)]
struct Entry {
    value: Value,
    expires: Instant,
}

/// A tiny TTL map of JSON responses keyed by the same Redis key schema the Go version used.
#[derive(Default)]
pub struct ApiCache {
    entries: RwLock<HashMap<String, Entry>>,
}

impl ApiCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a still-valid entry, deserialised into `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let entry = {
            let guard = self.entries.read().ok()?;
            let entry = guard.get(key)?;
            if entry.expires <= Instant::now() {
                return None;
            }
            entry.clone()
        };
        serde_json::from_value(entry.value).ok()
    }

    /// Stores `value` under `key`, clamped to [`HARD_LIMIT`].
    pub fn put<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) {
        let Ok(serialised) = serde_json::to_value(value) else {
            return;
        };
        let Some(mut guard) = self.entries.write().ok() else {
            return;
        };
        if guard.len() > 4096 {
            let now = Instant::now();
            guard.retain(|_, entry| entry.expires > now);
        }
        guard.insert(
            key.to_string(),
            Entry {
                value: serialised,
                expires: Instant::now() + ttl.min(HARD_LIMIT),
            },
        );
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.write() {
            guard.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().map(|guard| guard.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_values() {
        let cache = ApiCache::new();
        cache.put("k", &vec![1i64, 2, 3], Duration::from_secs(60));
        let got: Vec<i64> = cache.get("k").unwrap();
        assert_eq!(got, vec![1, 2, 3]);
        assert!(
            cache.get::<Vec<String>>("k").is_none(),
            "wrong type is a miss"
        );
    }

    #[test]
    fn expired_entries_are_misses() {
        let cache = ApiCache::new();
        cache.put("k", &"v", Duration::ZERO);
        assert!(cache.get::<String>("k").is_none());
    }

    #[test]
    fn put_overwrites_and_ttl_is_clamped() {
        let cache = ApiCache::new();
        cache.put("k", &1u32, Duration::from_secs(10));
        cache.put("k", &2u32, Duration::from_secs(10));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get::<u32>("k"), Some(2));

        // A ten year TTL is clamped to the hard limit, so it is still retrievable now.
        cache.put("long", &"x", Duration::from_secs(60 * 60 * 24 * 3650));
        assert_eq!(cache.get::<String>("long").as_deref(), Some("x"));
    }

    #[test]
    fn clear_empties_the_cache() {
        let cache = ApiCache::new();
        cache.put("a", &1u32, Duration::from_secs(10));
        cache.clear();
        assert!(cache.is_empty());
    }
}
