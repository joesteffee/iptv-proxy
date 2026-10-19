//! Shared server state: category filter, on-disk M3U cache, rate-limit tracker, HLS redirect map
//! and per-key locks.

use crate::cache::RedisCache;
use crate::config::ProxyConfig;
use crate::m3u::Track;
use crate::xtream::{AuthenticationResponse, XtreamClient};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Categories the UI and the filter know about.
pub const CATEGORY_TYPES: [&str; 3] = ["live", "movies", "series"];

/// Which category ids are enabled, per category type.
///
/// Identical semantics to the Go filter: anything not explicitly enabled is disabled, including
/// whole types the filter file never mentioned, and a missing file simply means "nothing enabled".
#[derive(Clone, Default)]
pub struct CategoryFilter {
    enabled: Arc<RwLock<BTreeMap<String, BTreeMap<String, bool>>>>,
}

impl CategoryFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// True only for ids explicitly set to `true`.
    pub fn is_category_enabled(&self, category_type: &str, id: &str) -> bool {
        let guard = self.enabled.read().expect("category filter poisoned");
        guard
            .get(category_type)
            .and_then(|ids| ids.get(id))
            .copied()
            .unwrap_or(false)
    }

    /// Enabled ids of one type, for the UI.
    pub fn enabled_ids(&self, category_type: &str) -> HashSet<String> {
        let guard = self.enabled.read().expect("category filter poisoned");
        guard
            .get(category_type)
            .map(|ids| {
                ids.iter()
                    .filter(|&(_id, enabled)| *enabled)
                    .map(|(id, _enabled)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_enabled(&self, enabled: BTreeMap<String, BTreeMap<String, bool>>) {
        *self.enabled.write().expect("category filter poisoned") = enabled;
    }

    /// Snapshot of the filter, used when answering `GET /api/categories`.
    pub fn snapshot(&self) -> BTreeMap<String, BTreeMap<String, bool>> {
        self.enabled
            .read()
            .expect("category filter poisoned")
            .clone()
    }

    /// Loads a filter file. A missing file is not an error — everything stays disabled, exactly like
    /// the Go behaviour.
    pub fn load_from_file(&self, path: &Path) -> Result<()> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    "no category filter file at {}, all categories disabled",
                    path.display()
                );
                return Ok(());
            }
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };

        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            tracing::warn!("WARNING: category filter file {} is empty", path.display());
            return Ok(());
        }

        let enabled: BTreeMap<String, BTreeMap<String, bool>> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse category filter {}", path.display()))?;
        self.set_enabled(enabled);
        Ok(())
    }

    /// Writes the filter with Go's `MarshalIndent(v, "", "  ")` layout and 0644 permissions.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let snapshot = self.snapshot();
        let json = serde_json::to_string_pretty(&snapshot).context("encode category filter")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }

        // Write-and-rename so a concurrent reader never sees a half-written filter.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json.as_bytes())
            .with_context(|| format!("write {}", temp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))
                .with_context(|| format!("chmod {}", temp.display()))?;
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("rename {} -> {}", temp.display(), path.display()))?;
        Ok(())
    }
}

/// Generated playlists kept on disk. Every entry is a real file so the HTTP layer can serve it with
/// range support and zero copying, which is what the Go version did.
#[derive(Clone)]
pub struct M3uCache {
    entries: Arc<RwLock<HashMap<String, Entry>>>,
    dir: PathBuf,
}

#[derive(Clone, Debug)]
struct Entry {
    path: PathBuf,
    written: Instant,
}

impl M3uCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            dir: dir.into(),
        }
    }

    /// Path of a still-fresh entry. `expiration_hours == 0` means "never expires" is *not* what the
    /// Go code did — it compared `hours >= 0`, so 0 expired everything. Keep that.
    pub fn fresh_path(&self, name: &str, expiration_hours: u64) -> Option<PathBuf> {
        let guard = self.entries.read().expect("m3u cache poisoned");
        let entry = guard.get(name)?;
        let age_hours = entry.written.elapsed().as_secs_f64() / 3600.0;
        if age_hours >= expiration_hours as f64 {
            return None;
        }
        Some(entry.path.clone())
    }

    /// Path of an entry regardless of age; used after [`M3uCache::store`] or when the caller already
    /// knows the entry exists.
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        self.entries
            .read()
            .expect("m3u cache poisoned")
            .get(name)
            .map(|entry| entry.path.clone())
    }

    /// Writes `contents` to a uniquely named file and records it under `name`, replacing and unlinking
    /// any previous file.
    pub async fn store(&self, name: &str, contents: Vec<u8>) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("create cache dir {}", self.dir.display()))?;

        let final_path = self
            .dir
            .join(format!("{}.iptv-proxy.m3u", uuid::Uuid::new_v4()));
        let temp_path = self
            .dir
            .join(format!("{}.iptv-proxy.m3u.part", uuid::Uuid::new_v4()));

        tokio::fs::write(&temp_path, &contents)
            .await
            .with_context(|| format!("write {}", temp_path.display()))?;
        tokio::fs::rename(&temp_path, &final_path)
            .await
            .with_context(|| format!("rename {}", final_path.display()))?;

        let previous = {
            let mut guard = self.entries.write().expect("m3u cache poisoned");
            guard.insert(
                name.to_string(),
                Entry {
                    path: final_path.clone(),
                    written: Instant::now(),
                },
            )
        };

        if let Some(old) = previous {
            let _ = tokio::fs::remove_file(&old.path).await;
        }

        Ok(final_path)
    }

    /// Drops every entry and unlinks its file — what `POST /api/categories` does after the enabled
    /// set changes.
    pub async fn clear(&self) {
        let removed: Vec<PathBuf> = {
            let mut guard = self.entries.write().expect("m3u cache poisoned");
            guard.drain().map(|(_, entry)| entry.path).collect()
        };
        for path in removed {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().expect("m3u cache poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Tracks URLs currently being throttled by the upstream (HTTP 458) so concurrent clients waiting
/// for the same stream share one retry loop instead of each hammering the provider.
#[derive(Clone, Default)]
pub struct RateLimitTracker {
    limited: Arc<Mutex<HashMap<String, Instant>>>,
}

impl RateLimitTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sleeps out any active penalty for `url`, capped at `max_wait`. Returns true when it waited.
    pub async fn wait_if_rate_limited(&self, url: &str, max_wait: Duration) -> bool {
        let remaining = {
            let guard = self.limited.lock().await;
            match guard.get(url) {
                Some(expiry) => expiry.saturating_duration_since(Instant::now()),
                None => Duration::ZERO,
            }
        };
        if remaining.is_zero() {
            return false;
        }
        let wait = remaining.min(max_wait);
        tracing::debug!("rate limited upstream, waiting {wait:?} for {url}");
        tokio::time::sleep(wait).await;
        true
    }

    pub async fn mark_rate_limited(&self, url: &str, duration: Duration) {
        self.limited
            .lock()
            .await
            .insert(url.to_string(), Instant::now() + duration);
    }

    pub async fn clear_rate_limit(&self, url: &str) {
        self.limited.lock().await.remove(url);
    }

    pub async fn is_limited(&self, url: &str) -> bool {
        let guard = self.limited.lock().await;
        guard
            .get(url)
            .is_some_and(|expiry| *expiry > Instant::now())
    }
}

/// One async mutex per key, so two clients asking for the same playlist trigger one generation
/// while the second waits and then reads the result.
#[derive(Clone, Default)]
pub struct KeyedLocks {
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl KeyedLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` while holding the lock for `key`, then forgets the lock if nothing else refers to
    /// it. Keeps the map bounded to the keys with work in flight.
    pub async fn with<T, F, Fut>(&self, key: &str, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let lock = {
            let mut guard = self.locks.lock().await;
            Arc::clone(
                guard
                    .entry(key.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };

        let _guard = lock.lock().await;
        let output = f().await;
        drop(_guard);
        drop(lock);

        let mut guard = self.locks.lock().await;
        if guard
            .get(key)
            .is_some_and(|existing| Arc::strong_count(existing) == 1)
        {
            guard.remove(key);
        }
        output
    }

    pub async fn len(&self) -> usize {
        self.locks.lock().await.len()
    }

    /// True when no per-key lock is currently held.
    pub async fn is_empty(&self) -> bool {
        self.locks.lock().await.is_empty()
    }
}

/// Everything a request handler needs. Wrapped in [`Arc`] and cloned into every handler.
pub struct AppState {
    pub config: Arc<ProxyConfig>,
    /// Client used for streaming: connection reuse, no overall timeout, redirects followed manually.
    pub stream_client: reqwest::Client,
    /// Client used for HLS: identical, but redirects are returned to the caller untouched.
    pub hls_client: reqwest::Client,
    /// Present when an Xtream provider is configured.
    pub xtream: Option<XtreamClient>,
    pub redis: Option<Arc<RedisCache>>,
    pub filter: CategoryFilter,
    pub m3u_cache: M3uCache,
    /// Xtream API responses kept between playlists (see [`crate::cache::ApiCache`]).
    pub api_cache: crate::cache::ApiCache,
    pub rate_limits: RateLimitTracker,
    pub locks: KeyedLocks,
    /// `channel.m3u8` -> the redirect target learned from the last HLS request.
    pub hls_redirects: RwLock<HashMap<String, String>>,
    /// Cached `player_api.php` login payload: the Go client re-authenticated on every single API
    /// call, which doubled upstream traffic for no benefit.
    pub auth: Mutex<Option<CachedAuth>>,
    /// Tracks of the remote M3U in non-Xtream mode; the stream routes are built from these.
    pub tracks: RwLock<Vec<Track>>,
    /// Anti-collision token embedded in generated stream paths.
    pub anti_collision: String,
}

/// A cached authentication payload with the time it was fetched.
pub struct CachedAuth {
    pub response: AuthenticationResponse,
    pub fetched: Instant,
}

/// How long a cached login payload stays valid. Login is only needed to answer the default
/// `player_api.php` action, where the numbers are months-old at best.
pub const AUTH_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

impl AppState {
    pub fn new(
        config: Arc<ProxyConfig>,
        xtream: Option<XtreamClient>,
        redis: Option<Arc<RedisCache>>,
    ) -> Self {
        let anti_collision = if config.custom_id.is_empty() {
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        } else {
            config.custom_id.trim_matches('/').to_string()
        };

        Self {
            config,
            stream_client: crate::http::streaming_client(),
            hls_client: crate::http::streaming_client(),
            xtream,
            redis,
            filter: CategoryFilter::new(),
            m3u_cache: M3uCache::new(std::env::temp_dir()),
            api_cache: crate::cache::ApiCache::new(),
            rate_limits: RateLimitTracker::new(),
            locks: KeyedLocks::new(),
            hls_redirects: RwLock::new(HashMap::new()),
            auth: Mutex::new(None),
            tracks: RwLock::new(Vec::new()),
            anti_collision,
        }
    }

    /// Builds the state for a running server: connects Redis when configured and creates the
    /// Xtream client when the subscription is an Xtream Codes backend.
    pub async fn startup(config: ProxyConfig) -> Result<Self> {
        let xtream = config.xtream_enabled().then(|| {
            XtreamClient::new(
                crate::http::api_client(),
                config.xtream_base_url.clone(),
                config.xtream_user.clone(),
                config.xtream_password.clone(),
            )
        });

        let redis = if config.redis_url.is_empty() {
            None
        } else {
            Some(Arc::new(
                RedisCache::connect(&config.redis_url)
                    .await
                    .context("connect to redis")?,
            ))
        };

        Ok(Self::new(Arc::new(config), xtream, redis))
    }

    /// Returns the cached login payload, fetching it once if needed.
    pub async fn authenticate(&self) -> Result<AuthenticationResponse> {
        if let Some(cached) = self.auth.lock().await.as_ref() {
            if cached.fetched.elapsed() < AUTH_CACHE_TTL {
                return Ok(cached.response.clone());
            }
        }
        let client = self
            .xtream
            .as_ref()
            .context("no xtream provider configured")?;
        let response = client.authenticate().await.context("xtream login")?;
        *self.auth.lock().await = Some(CachedAuth {
            response: response.clone(),
            fetched: Instant::now(),
        });
        Ok(response)
    }

    /// Records a learned HLS redirect target.
    pub fn remember_hls_redirect(&self, channel: &str, location: &str) {
        self.hls_redirects
            .write()
            .expect("hls map poisoned")
            .insert(channel.to_string(), location.to_string());
    }

    pub fn hls_redirect(&self, channel: &str) -> Option<String> {
        self.hls_redirects
            .read()
            .expect("hls map poisoned")
            .get(channel)
            .cloned()
    }
}

impl AppState {
    /// Puts generated playlist files in `dir` instead of the system temp directory. The integration
    /// tests use this so parallel runs cannot observe each other's playlists.
    #[must_use]
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.m3u_cache = M3uCache::new(dir);
        self
    }
}

/// Builds a state for unit tests: no provider, no Redis, playlist files in a temp directory.
#[cfg(test)]
pub fn test_state(config: crate::config::ProxyConfig) -> Arc<AppState> {
    let dir = tempfile::tempdir().expect("temp dir");
    AppState::new(Arc::new(config), None, None)
        .with_cache_dir(dir.keep())
        .into_shared()
}

impl AppState {
    /// Convenience for callers that need `Arc<AppState>` immediately.
    #[must_use]
    pub fn into_shared(self) -> Arc<AppState> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_categories_are_disabled() {
        let filter = CategoryFilter::new();
        assert!(!filter.is_category_enabled("live", "1"));
        filter.set_enabled(BTreeMap::from([(
            "live".to_string(),
            BTreeMap::from([("1".to_string(), true), ("2".to_string(), false)]),
        )]));
        assert!(filter.is_category_enabled("live", "1"));
        assert!(!filter.is_category_enabled("live", "2"));
        assert!(!filter.is_category_enabled("movies", "1"));
    }

    #[test]
    fn filter_round_trips_through_the_go_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("category_filters.json");
        let filter = CategoryFilter::new();
        filter.set_enabled(BTreeMap::from([(
            "live".to_string(),
            BTreeMap::from([("7".to_string(), true)]),
        )]));
        filter.save_to_file(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\n  \"live\": {\n    \"7\": true\n  }\n}");

        let reloaded = CategoryFilter::new();
        reloaded.load_from_file(&path).unwrap();
        assert!(reloaded.is_category_enabled("live", "7"));
    }

    #[test]
    fn missing_filter_file_is_not_an_error() {
        let filter = CategoryFilter::new();
        filter
            .load_from_file(Path::new("/nonexistent/category_filters.json"))
            .unwrap();
        assert!(filter.snapshot().is_empty());
    }

    #[tokio::test]
    async fn m3u_cache_serves_fresh_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = M3uCache::new(dir.path());
        cache
            .store("apiget.ts", b"#EXTM3U\n".to_vec())
            .await
            .unwrap();

        assert!(cache.fresh_path("apiget.ts", 1).is_some());
        assert!(cache.fresh_path("apiget.ts", 0).is_none());
        assert!(cache.fresh_path("missing", 1).is_none());
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn storing_replaces_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = M3uCache::new(dir.path());
        let first = cache.store("k", b"a".to_vec()).await.unwrap();
        let second = cache.store("k", b"b".to_vec()).await.unwrap();
        assert_ne!(first, second);
        assert!(!first.exists(), "the replaced file should be unlinked");
        assert!(second.exists());
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn clear_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = M3uCache::new(dir.path());
        let path = cache.store("k", b"a".to_vec()).await.unwrap();
        cache.clear().await;
        assert!(cache.is_empty());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn rate_limit_wait_is_capped_and_expires() {
        let tracker = RateLimitTracker::new();
        let url = "http://up/1";
        assert!(!tracker.is_limited(url).await);

        tracker
            .mark_rate_limited(url, Duration::from_millis(50))
            .await;
        assert!(tracker.is_limited(url).await);

        let start = Instant::now();
        assert!(
            tracker
                .wait_if_rate_limited(url, Duration::from_millis(10))
                .await
        );
        assert!(
            start.elapsed() < Duration::from_millis(45),
            "wait must be capped"
        );

        tracker.clear_rate_limit(url).await;
        assert!(
            !tracker
                .wait_if_rate_limited(url, Duration::from_millis(10))
                .await
        );
    }

    #[tokio::test]
    async fn keyed_locks_serialise_and_prune() {
        let locks = KeyedLocks::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let locks = locks.clone();
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    locks
                        .with("k", || {
                            let counter = Arc::clone(&counter);
                            async move {
                                let seen = counter.load(std::sync::atomic::Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(5)).await;
                                counter.store(seen + 1, std::sync::atomic::Ordering::SeqCst);
                            }
                        })
                        .await;
                })
            })
            .collect();

        for task in tasks {
            task.await.unwrap();
        }

        // Every task saw the previous state: the critical section really was exclusive.
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 8);
        // and the map is empty again once nothing holds the key.
        assert_eq!(locks.len().await, 0);
    }
}
