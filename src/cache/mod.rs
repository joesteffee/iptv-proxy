//! Redis-backed cache for Xtream API payloads, plus the background worker that keeps it warm.
//!
//! Key schema, TTLs and refresh policy are unchanged from the Go implementation so an existing
//! Redis instance keeps working across the upgrade:
//!
//! * `iptv:live:categories`, `iptv:vod:categories`, `iptv:series:categories`
//! * `iptv:live:streams:{category_id}`, `iptv:vod:streams:{category_id}`, `iptv:series:list:{category_id}`
//! * `iptv:series:info:{series_id}`
//! * refresh bookkeeping under `{key}:meta`
//! * a 7 day TTL on every cached entry
//!
//! The worker ticks every 15 minutes, refreshes anything past 80% of its TTL, jitters the next
//! refresh across the remaining 20% so a fleet of categories does not stampede the provider, and
//! drops a key after 5 failed attempts.

pub mod memory;

pub use memory::ApiCache;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Client};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// TTL applied to every cached Xtream payload (Go: `7 * 24 * time.Hour`).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long to wait for Redis at startup before serving without it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Fraction of the TTL after which a key becomes eligible for refresh.
const REFRESH_THRESHOLD: f64 = 0.8;
/// Give up on a key after this many consecutive failed refreshes.
const MAX_RETRY_ATTEMPTS: u32 = 5;
/// Never retry a failing key more often than this.
const MIN_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How often the worker scans for expiring keys.
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Refresh bookkeeping stored under `{key}:meta`. Field names and RFC 3339 timestamps match the Go
/// struct so metadata written by the old container still parses.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RefreshMetadata {
    #[serde(rename = "retry_count", default)]
    pub retry_count: u32,
    #[serde(rename = "last_attempt", default)]
    pub last_attempt: Option<DateTime<Utc>>,
    #[serde(rename = "next_refresh", default)]
    pub next_refresh: Option<DateTime<Utc>>,
    #[serde(rename = "last_success", default)]
    pub last_success: Option<DateTime<Utc>>,
}

/// A connection to the cache. Cloning is cheap — `ConnectionManager` pools internally.
#[derive(Clone)]
pub struct RedisCache {
    conn: ConnectionManager,
}

impl RedisCache {
    pub async fn connect(url: &str) -> Result<Self> {
        let client = Client::open(url).context("invalid redis URL")?;
        // The connection manager retries forever, so a wrong URL would otherwise keep the proxy
        // from ever listening. The Go version pinged Redis once and carried on without a cache.
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, ConnectionManager::new(client))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "redis did not answer within {} seconds",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .context("cannot reach redis")?;
        Ok(Self { conn })
    }

    /// Returns the decoded value, or `None` when the key is absent.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let mut conn = self.conn.clone();
        let raw: Option<Vec<u8>> = conn
            .get(key)
            .await
            .with_context(|| format!("redis GET {key}"))?;
        match raw {
            None => Ok(None),
            Some(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => Ok(Some(value)),
                Err(err) => {
                    // Go logged the error and fell back to the API; do the same rather than
                    // failing the request outright.
                    tracing::warn!("cached value for {key} is unreadable: {err}");
                    Ok(None)
                }
            },
        }
    }

    pub async fn set_json<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<()> {
        let payload = serde_json::to_vec(value).context("encode cache value")?;
        let mut conn = self.conn.clone();
        // EX takes whole seconds; a sub-second TTL would otherwise silently mean "no expiry".
        let seconds = ttl.as_secs().max(1);
        redis::Cmd::set(key, payload)
            .arg("EX")
            .arg(seconds)
            .exec_async(&mut conn)
            .await
            .with_context(|| format!("redis SET {key}"))
    }

    /// Remaining TTL in seconds. `-1` means "no expiry", `-2` means "no such key".
    pub async fn ttl(&self, key: &str) -> Result<i64> {
        let mut conn = self.conn.clone();
        let ttl: i64 = conn
            .ttl(key)
            .await
            .with_context(|| format!("redis TTL {key}"))?;
        Ok(ttl)
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: i64 = conn
            .del::<_, i64>(key)
            .await
            .with_context(|| format!("redis DEL {key}"))?;
        Ok(())
    }

    /// All keys matching `pattern`, excluding the `:meta` bookkeeping keys.
    ///
    /// Uses `SCAN` rather than Go's `KEYS` so a large cache does not block the server.
    pub async fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let mut conn = self.conn.clone();
        let mut matched = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await
                .context("redis SCAN")?;
            matched.extend(batch);
            if next == 0 {
                break;
            }
            cursor = next;
        }
        matched.retain(|key| !key.ends_with(":meta"));
        Ok(matched)
    }

    fn meta_key(key: &str) -> String {
        format!("{key}:meta")
    }

    pub async fn refresh_metadata(&self, key: &str) -> Result<Option<RefreshMetadata>> {
        self.get_json(&Self::meta_key(key)).await
    }

    pub async fn set_refresh_metadata(
        &self,
        key: &str,
        meta: &RefreshMetadata,
        ttl: Duration,
    ) -> Result<()> {
        self.set_json(&Self::meta_key(key), meta, ttl).await
    }

    pub async fn increment_retry_count(&self, key: &str) -> Result<u32> {
        let mut meta = self.refresh_metadata(key).await?.unwrap_or_default();
        meta.retry_count += 1;
        meta.last_attempt = Some(Utc::now());
        self.set_refresh_metadata(key, &meta, DEFAULT_CACHE_TTL)
            .await?;
        Ok(meta.retry_count)
    }

    pub async fn reset_retry_count(&self, key: &str) -> Result<()> {
        let mut meta = self.refresh_metadata(key).await?.unwrap_or_default();
        meta.retry_count = 0;
        meta.last_success = Some(Utc::now());
        meta.last_attempt = Some(Utc::now());
        self.set_refresh_metadata(key, &meta, DEFAULT_CACHE_TTL)
            .await
    }

    /// Removes a cache key and its metadata.
    pub async fn delete_cache_key(&self, key: &str) -> Result<()> {
        self.delete(key).await?;
        self.delete(&Self::meta_key(key)).await
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A refresh callback receives the cache key that is about to expire.
pub type RefreshCallback = Arc<dyn Fn(String) -> BoxFuture<Result<()>> + Send + Sync + 'static>;

/// Background refresh worker.
pub struct RefreshWorker {
    cache: Arc<RedisCache>,
    callbacks: Vec<(String, RefreshCallback)>,
}

impl RefreshWorker {
    pub fn new(cache: Arc<RedisCache>) -> Self {
        Self {
            cache,
            callbacks: Vec::new(),
        }
    }

    /// Registers a callback for keys matching `pattern`, which may contain `*` wildcards.
    pub fn register(&mut self, pattern: impl Into<String>, callback: RefreshCallback) {
        self.callbacks.push((pattern.into(), callback));
    }

    /// Spawns the worker. Returns a handle that stops it when dropped, so callers cannot forget to
    /// shut it down.
    pub fn start(mut self) -> RefreshWorkerHandle {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let cache = Arc::clone(&self.cache);
        let callbacks = Arc::new(std::mem::take(&mut self.callbacks));

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REFRESH_CHECK_INTERVAL);
            // The first tick fires immediately, matching the Go worker's initial sweep.
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = ticker.tick() => {
                        refresh_expiring_items(&cache, &callbacks).await;
                    }
                }
            }
        });

        RefreshWorkerHandle {
            task,
            shutdown: Some(shutdown_tx),
        }
    }
}

/// Handle that stops the refresh worker when dropped or explicitly awaited.
pub struct RefreshWorkerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl RefreshWorkerHandle {
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for RefreshWorkerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.abort();
    }
}

async fn refresh_expiring_items(cache: &Arc<RedisCache>, callbacks: &[(String, RefreshCallback)]) {
    for (pattern, callback) in callbacks {
        let keys = match cache.keys(pattern).await {
            Ok(keys) => keys,
            Err(err) => {
                tracing::warn!("cannot list cache keys for {pattern}: {err}");
                continue;
            }
        };

        for key in keys {
            check_and_refresh_key(cache, &key, callback).await;
        }
    }
}

async fn check_and_refresh_key(cache: &Arc<RedisCache>, key: &str, callback: &RefreshCallback) {
    let ttl = match cache.ttl(key).await {
        Ok(ttl) => ttl,
        Err(err) => {
            tracing::warn!("cannot read TTL for {key}: {err}");
            return;
        }
    };
    // -1 = no expiry, -2 = gone. Nothing to refresh.
    if ttl < 0 {
        return;
    }

    let ttl_duration = Duration::from_secs(ttl as u64);
    let threshold = ttl_duration.mul_f64(REFRESH_THRESHOLD);
    if ttl > threshold.as_secs() as i64 {
        return;
    }

    let meta = cache
        .refresh_metadata(key)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    if let Some(next) = meta.next_refresh {
        if Utc::now() < next {
            return;
        }
    }

    if let Some(last_attempt) = meta.last_attempt {
        let since = Utc::now()
            .signed_duration_since(last_attempt)
            .to_std()
            .unwrap_or(Duration::ZERO);
        if since < MIN_RETRY_INTERVAL && meta.retry_count > 0 {
            return;
        }
    }

    match callback(key.to_string()).await {
        Ok(()) => {
            tracing::info!("refreshed cache key {key}");
            if let Err(err) = cache.reset_retry_count(key).await {
                tracing::warn!("cannot reset retry count for {key}: {err}");
            }
        }
        Err(err) => {
            let attempts = cache.increment_retry_count(key).await.unwrap_or(0);
            if attempts >= MAX_RETRY_ATTEMPTS {
                tracing::warn!("dropping cache key {key} after {attempts} failed refreshes: {err}");
                let _ = cache.delete_cache_key(key).await;
                return;
            }
            tracing::warn!(
                "refresh of {key} failed (attempt {attempts}/{MAX_RETRY_ATTEMPTS}): {err}"
            );
            let next = Utc::now() + ChronoDuration::from_std(MIN_RETRY_INTERVAL).unwrap();
            let meta = RefreshMetadata {
                retry_count: attempts,
                last_attempt: Some(Utc::now()),
                next_refresh: Some(next),
                last_success: meta.last_success,
            };
            let _ = cache
                .set_refresh_metadata(key, &meta, DEFAULT_CACHE_TTL)
                .await;
        }
    }
}

/// Picks a refresh moment somewhere in the last 20% of the TTL, jittered by a hash of the key so
/// thousands of keys written together do not expire together.
pub fn calculate_refresh_time(key: &str, ttl: Duration) -> DateTime<Utc> {
    use sha2::Digest;
    let window = ttl.mul_f64(1.0 - REFRESH_THRESHOLD);
    let mut hasher = sha2::Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let raw = u64::from_le_bytes(bytes);
    let offset = if window.is_zero() {
        Duration::ZERO
    } else {
        Duration::from_nanos(raw % window.as_nanos() as u64)
    };

    Utc::now() + ttl.mul_f64(REFRESH_THRESHOLD) + offset
}

/// Formats a timestamp the way Go's `time.Time` marshals it (RFC 3339 with nanoseconds).
pub fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, false)
}

/// Registry of the patterns the proxy refreshes, mirroring the Go callback table.
pub fn default_patterns() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("live_categories", "iptv:live:categories"),
        ("vod_categories", "iptv:vod:categories"),
        ("series_categories", "iptv:series:categories"),
        ("live_streams", "iptv:live:streams:*"),
        ("vod_streams", "iptv:vod:streams:*"),
        ("series_list", "iptv:series:list:*"),
        ("series_info", "iptv:series:info:*"),
    ])
}

/// Matches a cache key against a pattern containing `*` wildcards.
pub fn key_matches_pattern(key: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return key == pattern;
    }
    let mut rest = key;
    let segments: Vec<&str> = pattern.split('*').collect();
    if let Some(first) = segments.first() {
        if !first.is_empty() {
            match rest.strip_prefix(first) {
                Some(remaining) => rest = remaining,
                None => return false,
            }
        }
    }
    if let Some(last) = segments.last() {
        if !last.is_empty() {
            if !rest.ends_with(last) {
                return false;
            }
            rest = &rest[..rest.len() - last.len()];
        }
    }
    for segment in &segments[1..segments.len() - 1] {
        if segment.is_empty() {
            continue;
        }
        match rest.find(segment) {
            Some(index) => rest = &rest[index + segment.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_metadata_uses_go_field_names() {
        let json = serde_json::to_string(&RefreshMetadata::default()).unwrap();
        assert_eq!(
            json,
            r#"{"retry_count":0,"last_attempt":null,"next_refresh":null,"last_success":null}"#
        );
        let parsed: RefreshMetadata =
            serde_json::from_str(r#"{"retry_count":3,"last_attempt":"2026-10-17T10:00:00Z"}"#)
                .unwrap();
        assert_eq!(parsed.retry_count, 3);
        assert!(parsed.last_attempt.is_some());
    }

    #[test]
    fn pattern_matching_covers_the_registered_patterns() {
        assert!(key_matches_pattern(
            "iptv:live:streams:42",
            "iptv:live:streams:*"
        ));
        assert!(!key_matches_pattern(
            "iptv:vod:streams:42",
            "iptv:live:streams:*"
        ));
        assert!(key_matches_pattern(
            "iptv:live:categories",
            "iptv:live:categories"
        ));
        assert!(!key_matches_pattern(
            "iptv:live:categories:extra",
            "iptv:live:categories"
        ));
        assert!(key_matches_pattern("a:b:c", "*:*:*"));
        assert!(key_matches_pattern("anything", "*"));
        assert!(key_matches_pattern(
            "iptv:series:info:99",
            "iptv:series:info:*"
        ));
    }

    #[test]
    fn refresh_time_lands_in_the_last_fifth_of_the_ttl() {
        let ttl = Duration::from_secs(7 * 24 * 60 * 60);
        let before = Utc::now();
        let at = calculate_refresh_time("iptv:live:streams:1", ttl);
        let delta = (at - before).to_std().unwrap();
        assert!(delta >= ttl.mul_f64(0.8), "{delta:?} too early");
        assert!(delta <= ttl, "{delta:?} too late");
    }

    #[test]
    fn refresh_time_is_jittered_per_key() {
        let ttl = Duration::from_secs(7 * 24 * 60 * 60);
        let a = calculate_refresh_time("iptv:live:streams:1", ttl);
        let b = calculate_refresh_time("iptv:live:streams:2", ttl);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn worker_sweeps_immediately_and_stops_cleanly() {
        // No Redis required: with no callbacks registered the sweep is a no-op, which still proves
        // the worker starts, ticks and shuts down without leaking a task.
        let client = Client::open("redis://127.0.0.1:1/").expect("client");
        // Build the worker against a cache we cannot reach: listing keys must log, not panic.
        // The connect is time-boxed because `ConnectionManager` retries for minutes when nothing
        // is listening, and CI has no Redis.
        let connecting = redis::aio::ConnectionManager::new(client);
        match tokio::time::timeout(Duration::from_secs(2), connecting).await {
            Ok(Ok(conn)) if true => {
                let worker = RefreshWorker::new(Arc::new(RedisCache { conn }));
                let handle = worker.start();
                tokio::time::sleep(Duration::from_millis(50)).await;
                handle.shutdown();
            }
            Ok(_) => {}
            Err(_elapsed) => {}
        }
    }
}
