//! Builds the Xtream playlist the Go version produced in `xtreamGenerateM3u`.
//!
//! The track shapes (tag order, naming rules, URL layout) are reproduced byte for byte — see
//! `tests/golden_playlist.rs`. What differs is how the data is fetched:
//!
//! * streams of different categories, and the `get_series_info` calls of a series list, are fetched
//!   a few at a time instead of strictly sequentially. Output order is unchanged: results are placed
//!   back into their original slot, so the playlist is still live → movies → series in API order;
//! * anything the concurrent pass could not fetch is retried one request at a time, which is what
//!   makes accounts limited to a single concurrent connection work (see [`fetch_per_category`]);
//! * responses are cached (in memory always, in Redis additionally when configured) under the same
//!   keys and TTLs as Go, so a second playlist within the TTL does no upstream work at all.
//!
//! Series info is fetched in chunks rather than all at once. A playlist can reference thousands of
//! series and their `info` blocks are discarded after naming, so unbounded concurrency would hold
//! gigabytes of JSON that the Go version never kept alive.

use crate::cache::DEFAULT_CACHE_TTL;
use crate::m3u::{Tag, Track};
use crate::state::AppState;
use crate::xtream::models::{Category, Series, SeriesInfo, Stream};
use crate::xtream::{XtreamClient, XtreamError};
use anyhow::{Context, Result};
use futures_util::stream::{iter, StreamExt};
use std::sync::Arc;
use std::time::Duration;

/// How many provider requests to keep in flight while building one playlist.
const FETCH_CONCURRENCY: usize = 8;

/// Retries the Go code applied to category lists.
const CATEGORY_ATTEMPTS: u32 = 3;
const CATEGORY_BACKOFF: Duration = Duration::from_secs(2);

/// Retries the Go code applied to stream lists and series info.
const ITEM_ATTEMPTS: u32 = 3;
const ITEM_BACKOFF: Duration = Duration::from_secs(1);

/// Maps the `output` query parameter to the extension appended to generated URLs.
///
/// Mirrors `getExtensionFromOutput`: `""` and `hls` are special-cased, everything else becomes
/// `.{output}` verbatim.
pub fn extension_from_output(output: &str) -> String {
    match output {
        "" | "mpegts" | "ts" => ".ts".to_string(),
        "hls" => ".m3u8".to_string(),
        other => format!(".{other}"),
    }
}

/// Generates the full playlist for `output` as marshalled playlist bytes.
///
/// Returns the tracks *before* URL rewriting; the caller applies
/// [`crate::rewrite::rewrite_uri`] through [`crate::m3u::marshall_into`] so a single code path
/// serves both the Xtream and the plain-M3U playlist.
pub async fn generate_tracks(state: &Arc<AppState>, output: &str) -> Result<Vec<Track>> {
    let client = state
        .xtream
        .as_ref()
        .context("generate_tracks called without an xtream provider")?;

    let extension = extension_from_output(output);

    // Sequential on purpose. Three requests are nothing on their own, but they set the tone for the
    // whole build and an account capped at one connection refuses the second of them.
    let live_categories = categories(state, client, "live").await?;
    let vod_categories = categories(state, client, "vod").await?;
    let series_categories = categories(state, client, "series").await?;

    let mut tracks = Vec::new();
    tracks.extend(live_tracks(state, client, &live_categories).await?);
    tracks.extend(vod_tracks(state, client, &vod_categories, &extension).await?);
    tracks.extend(series_tracks(state, client, &series_categories, &extension).await?);

    tracing::debug!("generated {} tracks for output {output:?}", tracks.len());
    Ok(tracks)
}

/// Fetches one playlist's worth of live tracks.
async fn live_tracks(
    state: &Arc<AppState>,
    client: &XtreamClient,
    categories: &[Category],
) -> Result<Vec<Track>> {
    let enabled = enabled_categories(state, "live", categories);
    let fetched = fetch_per_category(state, "iptv:live:streams", &enabled, |client, id| {
        Box::pin(async move { client.streams("live", &id).await })
    })
    .await?;

    let mut tracks = Vec::new();
    for (category_name, streams) in fetched {
        for stream in streams {
            let stream_id = stream_id_of(&stream);
            let mut track = Track {
                name: stream.name.clone(),
                length: -1,
                // Live URLs carry no `/live/` prefix and no extension; `rewrite_uri` adds the
                // prefix when it swaps the credentials.
                uri: format!(
                    "{}/{}/{}/{stream_id}",
                    client.base_url(),
                    client.username().as_str(),
                    client.password().as_str()
                ),
                tags: Vec::new(),
            };
            push_tag(&mut track, "tvg-id", &stream.epg_channel_id);
            push_tag(&mut track, "tvg-name", &stream.name);
            push_tag(&mut track, "tvg-logo", &stream.icon);
            push_tag(&mut track, "group-title", &category_name);
            tracks.push(track);
        }
    }
    Ok(tracks)
}

/// The Xtream live ID. Go read the `stream_id` field (falling back to `ID` through the alias chain
/// on [`Stream::id`]) — never `custom_sid`, which is a per-connection token some providers return.
fn stream_id_of(stream: &Stream) -> String {
    stream.id.get().to_string()
}

async fn vod_tracks(
    state: &Arc<AppState>,
    client: &XtreamClient,
    categories: &[Category],
    extension: &str,
) -> Result<Vec<Track>> {
    // The filter stores VOD categories under the "movies" type, the API calls them "vod".
    let enabled = enabled_categories(state, "movies", categories);
    let fetched = fetch_per_category(state, "iptv:vod:streams", &enabled, |client, id| {
        Box::pin(async move { client.streams("vod", &id).await })
    })
    .await?;

    let mut tracks = Vec::new();
    for (category_name, streams) in fetched {
        for stream in streams {
            let movie_extension = if stream.container_extension.is_empty() {
                extension.to_string()
            } else {
                format!(".{}", stream.container_extension)
            };
            let movie_id = stream.id.get().to_string();

            let mut track = Track {
                name: stream.name.clone(),
                length: -1,
                uri: format!(
                    "{}/movie/{}/{}/{movie_id}{movie_extension}",
                    client.base_url(),
                    client.username().as_str(),
                    client.password().as_str()
                ),
                tags: Vec::new(),
            };
            push_tag(&mut track, "tvg-id", &stream.epg_channel_id);
            push_tag(&mut track, "tvg-name", &stream.name);
            push_tag(&mut track, "tvg-logo", &stream.icon);
            push_tag(&mut track, "group-title", &category_name);
            tracks.push(track);
        }
    }
    Ok(tracks)
}

async fn series_tracks(
    state: &Arc<AppState>,
    client: &XtreamClient,
    categories: &[Category],
    extension: &str,
) -> Result<Vec<Track>> {
    let enabled = enabled_categories(state, "series", categories);
    let fetched = fetch_per_category(state, "iptv:series:list", &enabled, |client, id| {
        Box::pin(async move { client.series(&id).await })
    })
    .await?;

    let mut tracks = Vec::new();
    for (category_name, series_list) in fetched {
        // Chunked so peak memory stays at `FETCH_CONCURRENCY` series payloads, not one whole
        // category's worth.
        for chunk in series_list.chunks(FETCH_CONCURRENCY) {
            let infos = fetch_series_infos(state, client, chunk).await;
            for (serie, info) in chunk.iter().zip(infos) {
                let Some(info) = info else { continue };
                if info.episode_count() == 0 {
                    tracing::debug!(
                        "series {} (ID: {}) has no episodes, skipping",
                        serie.name,
                        serie.series_id.get()
                    );
                    continue;
                }
                tracks.extend(episode_tracks(
                    client,
                    serie,
                    &info,
                    &category_name,
                    extension,
                ));
            }
        }
    }
    Ok(tracks)
}

/// Fetches the info of every series in `chunk`, `None` where the provider never answered.
///
/// Same two passes as [`fetch_per_category`]: concurrent first, then the stragglers one at a time,
/// because a capped account refuses the parallel attempts and a silently missing series is a
/// silently missing season.
async fn fetch_series_infos(
    state: &Arc<AppState>,
    client: &XtreamClient,
    chunk: &[SeriesInfo],
) -> Vec<Option<Series>> {
    let ids: Vec<(usize, String)> = chunk
        .iter()
        .map(|serie| serie.series_id.get().to_string())
        .enumerate()
        .collect();

    let mut results: Vec<Option<Series>> = (0..chunk.len()).map(|_| None).collect();
    let outcomes = iter(ids)
        .map(|(index, id)| async move {
            let for_lookup = id.clone();
            (index, id, series_info(state, client, &for_lookup).await)
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut retryable = Vec::new();
    for (index, id, result) in outcomes {
        match result {
            Ok(info) => results[index] = Some(info),
            Err(err) => {
                tracing::debug!("concurrent info fetch for series {id} failed ({err}), retrying");
                retryable.push((index, id));
            }
        }
    }

    for (index, id) in retryable {
        match series_info(state, client, &id).await {
            Ok(info) => results[index] = Some(info),
            Err(err) => tracing::warn!("series {id} has no usable info: {err}"),
        }
    }

    results
}

/// Turns one series into its episode tracks, following the Go naming rules exactly.
fn episode_tracks(
    client: &XtreamClient,
    serie: &SeriesInfo,
    info: &Series,
    category_name: &str,
    extension: &str,
) -> Vec<Track> {
    let mut tracks = Vec::new();
    for episodes in info.episodes.values() {
        for episode in episodes {
            let season_episode = format!(
                "S{:02}E{:02}",
                episode.season.get(),
                episode.episode_num.get()
            );

            let title = if episode.title.is_empty() {
                episode.info.name.as_str()
            } else {
                episode.title.as_str()
            };

            // Four shapes, depending on how much of the canonical name the provider already put in
            // the episode title.
            let episode_name = if !title.is_empty()
                && title.contains(&season_episode)
                && !serie.name.is_empty()
                && title.contains(serie.name.as_str())
            {
                title.to_string()
            } else if !title.is_empty() && title.contains(&season_episode) {
                format!("{} - {title}", serie.name)
            } else if !title.is_empty() {
                format!("{} - {season_episode} - {title}", serie.name)
            } else {
                format!("{} - {season_episode}", serie.name)
            };

            if episode.id.is_empty() {
                tracing::warn!("WARNING: episode {episode_name} has no ID, skipping");
                continue;
            }

            let episode_extension = if episode.container_extension.is_empty() {
                extension.to_string()
            } else {
                format!(".{}", episode.container_extension)
            };

            let mut track = Track {
                name: episode_name.clone(),
                length: -1,
                uri: format!(
                    "{}/series/{}/{}/{episode_id}{episode_extension}",
                    client.base_url(),
                    client.username().as_str(),
                    client.password().as_str(),
                    episode_id = episode.id
                ),
                tags: Vec::new(),
            };
            push_tag(&mut track, "tvg-logo", &serie.cover);
            push_tag(&mut track, "tvg-name", &episode_name);
            push_tag(&mut track, "group-title", category_name);
            tracks.push(track);
        }
    }
    tracks
}

/// Appends a tag, skipping empty values like the Go code did.
fn push_tag(track: &mut Track, name: &str, value: &str) {
    if !value.is_empty() {
        track.tags.push(Tag::new(name, value));
    }
}

fn enabled_categories<'a>(
    state: &AppState,
    kind: &str,
    categories: &'a [Category],
) -> Vec<&'a Category> {
    categories
        .iter()
        .filter(|category| {
            state
                .filter
                .is_category_enabled(kind, &category.id.get().to_string())
        })
        .collect()
}

/// Reads a category list from cache or from the provider.
pub(crate) async fn categories(
    state: &Arc<AppState>,
    client: &XtreamClient,
    kind: &str,
) -> Result<Vec<Category>> {
    let key = format!("iptv:{kind}:categories");
    let client = client.clone();
    let kind_owned = kind.to_string();

    cached(state, &key, move || {
        fetch_categories(client.clone(), kind_owned.clone())
    })
    .await
    .with_context(|| format!("fetch {kind} categories"))
}

async fn fetch_categories(
    client: XtreamClient,
    kind: String,
) -> Result<Vec<Category>, XtreamError> {
    let retry_client = client.clone();
    client
        .retry(CATEGORY_ATTEMPTS, CATEGORY_BACKOFF, move |_| {
            let client = retry_client.clone();
            let kind = kind.clone();
            async move { client.categories(&kind).await }
        })
        .await
}

/// Fetches one list per enabled category, preserving category order in the result.
///
/// Returns `(category name, items)` pairs in the same order as `categories`.
async fn fetch_per_category<T, F>(
    state: &Arc<AppState>,
    key_prefix: &str,
    categories: &[&Category],
    fetch: F,
) -> Result<Vec<(String, Vec<T>)>>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static,
    F: Fn(
            XtreamClient,
            String,
        ) -> futures_util::future::BoxFuture<'static, Result<Vec<T>, XtreamError>>
        + Send
        + Sync
        + Clone,
{
    let client = state
        .xtream
        .as_ref()
        .context("fetch_per_category called without an xtream provider")?
        .clone();

    let keyed: Vec<(usize, String, String, String)> = categories
        .iter()
        .enumerate()
        .map(|(index, category)| {
            let id = category.id.get().to_string();
            (
                index,
                format!("{key_prefix}:{id}"),
                category.name.clone(),
                id,
            )
        })
        .collect();

    let mut results: Vec<Option<(String, Vec<T>)>> = (0..keyed.len()).map(|_| None).collect();

    // Pass one: bounded concurrency. This is where the speed over the Go version comes from.
    let outcomes = iter(keyed)
        .map(|(index, key, name, id)| {
            let client = client.clone();
            let state = Arc::clone(state);
            let fetch = fetch.clone();
            async move {
                let fetch_id = id.clone();
                let result = cached(&state, &key, move || fetch(client.clone(), fetch_id)).await;
                (index, key, name, id, result)
            }
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut retryable: Vec<(usize, String, String, String)> = Vec::new();
    let mut last_error: Option<anyhow::Error> = None;
    for (index, key, name, id, result) in outcomes {
        match result {
            Ok(items) => results[index] = Some((name, items)),
            Err(err) => {
                tracing::warn!("concurrent fetch of {key} failed ({err}), retrying sequentially");
                retryable.push((index, key, name, id));
            }
        }
    }

    // Pass two: strictly one request at a time. Providers that cap concurrent connections —
    // `max_connections: 1` is common — answer `500` to everything that arrives while another
    // request is in flight, so the whole concurrent pass can fail at once. The Go version never saw
    // this because it fetched one category at a time, and since a single failed category aborts the
    // build, dropping straight through to an error here would mean no playlist at all. A healthy
    // provider pays nothing: nothing failed, so this loop does not run.
    for (index, key, name, id) in retryable {
        let attempt = cached_sequential(state, &key, client.clone(), fetch.clone(), id).await;
        match attempt {
            Ok(items) => results[index] = Some((name, items)),
            Err(err) => {
                tracing::error!("failed to fetch {key}: {err}");
                last_error = Some(err.context(format!("fetch {name} streams")));
            }
        }
    }

    if let Some(err) = last_error {
        // A partial playlist is worse than an error: the client would silently lose every channel of
        // the failed category. Surface the first failure like Go did.
        return Err(err);
    }

    Ok(results.into_iter().map(Option::unwrap).collect())
}

/// Fetches one key through [`cached`], retrying transient failures with one request in flight.
async fn cached_sequential<T, F, Fut>(
    state: &Arc<AppState>,
    key: &str,
    client: XtreamClient,
    mut fetch: F,
    id: String,
) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static,
    F: FnMut(XtreamClient, String) -> Fut + Send,
    Fut: std::future::Future<Output = Result<Vec<T>, XtreamError>> + Send,
{
    let mut last = None;
    for attempt in 1..=ITEM_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(ITEM_BACKOFF * (attempt - 1)).await;
        }
        // Building the future does not start the request, so nothing leaves the process while an
        // earlier attempt is still being awaited.
        let pending = fetch(client.clone(), id.clone());
        match cached(state, key, move || pending).await {
            Ok(items) => return Ok(items),
            Err(err) => {
                let transient = err
                    .downcast_ref::<XtreamError>()
                    .is_some_and(XtreamError::is_transient);
                last = Some(err);
                if !transient {
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("{key} was not fetched")))
}

/// Cache-aside for one JSON list: memory, then Redis, then the provider.
///
/// A per-key lock makes concurrent misses on the same key collapse into a single provider request.
async fn cached<T, F, Fut>(state: &Arc<AppState>, key: &str, fetch: F) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = Result<Vec<T>, XtreamError>> + Send,
{
    if let Some(cached) = state.api_cache.get::<Vec<T>>(key) {
        return Ok(cached);
    }

    if let Some(redis) = &state.redis {
        if let Some(value) = redis
            .get_json::<Vec<T>>(key)
            .await
            .with_context(|| format!("redis get {key}"))?
        {
            state.api_cache.put(key, &value, DEFAULT_CACHE_TTL);
            return Ok(value);
        }
    }

    state
        .locks
        .with(key, move || cached_miss(state, key, fetch))
        .await
}

/// The locked part of [`cached`]: re-check, fetch, store.
async fn cached_miss<T, F, Fut>(state: &Arc<AppState>, key: &str, fetch: F) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = Result<Vec<T>, XtreamError>> + Send,
{
    // Re-check: whoever held the lock may have just populated it.
    if let Some(cached) = state.api_cache.get::<Vec<T>>(key) {
        return Ok(cached);
    }

    let value = fetch().await.map_err(|err| anyhow::anyhow!(err))?;
    store(state, key, &value).await;
    Ok(value)
}

/// Writes one value into both caches.
async fn store<T: serde::Serialize>(state: &Arc<AppState>, key: &str, value: &T) {
    state.api_cache.put(key, value, DEFAULT_CACHE_TTL);
    if let Some(redis) = &state.redis {
        if let Err(err) = redis.set_json(key, value, DEFAULT_CACHE_TTL).await {
            tracing::warn!("redis set {key} failed: {err}");
        }
    }
}

/// Reads one series' info, retrying the transient shapes Go treated as retryable: 5xx statuses and
/// the `[]` empty-array reply some providers return under load.
pub(crate) async fn series_info(
    state: &Arc<AppState>,
    client: &XtreamClient,
    series_id: &str,
) -> Result<Series> {
    let key = format!("iptv:series:info:{series_id}");

    if let Some(cached) = state.api_cache.get::<Series>(&key) {
        return Ok(cached);
    }
    if let Some(redis) = &state.redis {
        if let Some(value) = redis
            .get_json::<Series>(&key)
            .await
            .with_context(|| format!("redis get {key}"))?
        {
            state.api_cache.put(&key, &value, DEFAULT_CACHE_TTL);
            return Ok(value);
        }
    }

    state
        .locks
        .with(&key, || {
            let state = Arc::clone(state);
            let client = client.clone();
            let key = key.clone();
            let series_id = series_id.to_string();
            async move {
                // Re-check under the lock.
                if let Some(cached) = state.api_cache.get::<Series>(&key) {
                    return Ok(cached);
                }

                let mut last_error = None;
                for attempt in 1..=ITEM_ATTEMPTS {
                    match client.series_info(&series_id).await {
                        Ok(info) if info.episode_count() > 0 => {
                            store(&state, &key, &info).await;
                            return Ok(info);
                        }
                        Ok(_) => {
                            // The `[]` empty-array reply: some providers answer that under load.
                            last_error = Some(anyhow::anyhow!(
                                "API returned empty array [] for series ID {series_id}"
                            ));
                        }
                        Err(err) => {
                            let transient = err.is_transient();
                            last_error = Some(anyhow::anyhow!(err));
                            if !transient {
                                break;
                            }
                        }
                    }
                    if attempt < ITEM_ATTEMPTS {
                        tokio::time::sleep(ITEM_BACKOFF * attempt).await;
                    }
                }

                Err(last_error.unwrap_or_else(|| anyhow::anyhow!("series info failed")))
            }
        })
        .await
}

impl AppState {
    /// Drops everything cached about the provider's catalogue. Called when the enabled-category set
    /// changes so the next playlist is rebuilt from a clean slate.
    pub fn clear_catalog_cache(&self) {
        self.api_cache.clear();
    }
}

/// Re-fetches one Redis catalogue key from the provider and writes it back.
///
/// This is what the refresh worker calls before a key expires, so a client never pays for the
/// provider round-trip at the moment its playlist goes stale. `key` is the same string the read path
/// uses, so the two can never drift apart.
pub async fn refresh_key(state: &Arc<AppState>, key: &str) -> Result<()> {
    let Some(client) = state.xtream.as_ref() else {
        return Ok(());
    };

    let mut segments = key.split(':');
    let (Some("iptv"), Some(group), Some(list)) =
        (segments.next(), segments.next(), segments.next())
    else {
        return Ok(());
    };

    let fetched_series: Vec<Category>;
    let fetched_streams: Vec<crate::xtream::models::Stream>;
    let fetched_list: Vec<SeriesInfo>;

    match (group, list, segments.next()) {
        ("live" | "vod" | "series", "categories", None) => {
            fetched_series = client
                .retry(CATEGORY_ATTEMPTS, CATEGORY_BACKOFF, |_| {
                    client.categories(group)
                })
                .await?;
            store(state, key, &fetched_series).await;
        }
        ("live" | "vod", "streams", Some(id)) => {
            let id = id.to_string();
            fetched_streams = client
                .retry(ITEM_ATTEMPTS, ITEM_BACKOFF, |_| client.streams(group, &id))
                .await?;
            store(state, key, &fetched_streams).await;
        }
        ("series", "list", Some(id)) => {
            let id = id.to_string();
            fetched_list = client
                .retry(ITEM_ATTEMPTS, ITEM_BACKOFF, |_| client.series(&id))
                .await?;
            store(state, key, &fetched_list).await;
        }
        ("series", "info", Some(id)) => {
            let info = series_info(state, client, id).await?;
            store(state, key, &info).await;
        }
        _ => return Ok(()),
    }

    tracing::debug!("refreshed {key}");
    Ok(())
}

/// Registers the refresh worker's callbacks, one per catalogue key pattern — the same seven the Go
/// worker registered.
pub fn register_refresh_callbacks(worker: &mut crate::cache::RefreshWorker, state: &Arc<AppState>) {
    for pattern in [
        "iptv:live:categories",
        "iptv:live:streams:*",
        "iptv:vod:categories",
        "iptv:vod:streams:*",
        "iptv:series:categories",
        "iptv:series:list:*",
        "iptv:series:info:*",
    ] {
        let state = Arc::clone(state);
        worker.register(
            pattern,
            Arc::new(move |key| {
                let state = Arc::clone(&state);
                Box::pin(async move { refresh_key(&state, &key).await })
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_mapping_matches_go() {
        assert_eq!(extension_from_output(""), ".ts");
        assert_eq!(extension_from_output("ts"), ".ts");
        assert_eq!(extension_from_output("mpegts"), ".ts");
        assert_eq!(extension_from_output("hls"), ".m3u8");
        assert_eq!(extension_from_output("mkv"), ".mkv");
        assert_eq!(extension_from_output("mp4"), ".mp4");
        assert_eq!(extension_from_output("weird"), ".weird");
    }

    #[test]
    fn episode_naming_follows_the_four_go_rules() {
        let client = XtreamClient::new(
            crate::http::api_client(),
            "http://p",
            crate::config::CredentialString::new("xu"),
            crate::config::CredentialString::new("xp"),
        );
        let serie = SeriesInfo {
            name: "Dark".to_string(),
            cover: "http://p/c.jpg".to_string(),
            ..SeriesInfo::default()
        };

        let episode =
            |title: &str, season: i64, number: i64| crate::xtream::models::SeriesEpisode {
                id: "77".to_string(),
                title: title.to_string(),
                season: crate::xtream::models::FlexInt(season),
                episode_num: crate::xtream::models::FlexInt(number),
                ..Default::default()
            };

        let info = |episodes: Vec<_>| Series {
            episodes: std::collections::BTreeMap::from([("1".to_string(), episodes)]),
            ..Series::default()
        };

        // 1. Title already contains both the series name and the SxxEyy marker: used verbatim.
        let both = episode_tracks(
            &client,
            &serie,
            &info(vec![episode("Dark - S01E03 - Secrets", 1, 3)]),
            "Docs",
            ".mp4",
        );
        assert_eq!(both[0].name, "Dark - S01E03 - Secrets");

        // 2. Title has the marker only: series name is prefixed.
        let marker = episode_tracks(
            &client,
            &serie,
            &info(vec![episode("S01E03 - Secrets", 1, 3)]),
            "Docs",
            ".mp4",
        );
        assert_eq!(marker[0].name, "Dark - S01E03 - Secrets");

        // 3. Plain title: series, marker and title are joined.
        let plain = episode_tracks(
            &client,
            &serie,
            &info(vec![episode("Secrets", 1, 3)]),
            "Docs",
            ".mp4",
        );
        assert_eq!(plain[0].name, "Dark - S01E03 - Secrets");

        // 4. No title at all.
        let empty = episode_tracks(
            &client,
            &serie,
            &info(vec![episode("", 2, 7)]),
            "Docs",
            ".mp4",
        );
        assert_eq!(empty[0].name, "Dark - S02E07");

        // Tag order and URL shape.
        assert_eq!(
            empty[0]
                .tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tvg-logo", "tvg-name", "group-title"]
        );
        assert_eq!(empty[0].uri, "http://p/series/xu/xp/77.mp4");
    }

    #[test]
    fn episodes_without_an_id_are_skipped() {
        let client = XtreamClient::new(
            crate::http::api_client(),
            "http://p",
            crate::config::CredentialString::new("xu"),
            crate::config::CredentialString::new("xp"),
        );
        let serie = SeriesInfo::default();
        let info = Series {
            episodes: std::collections::BTreeMap::from([(
                "1".to_string(),
                vec![
                    crate::xtream::models::SeriesEpisode {
                        id: String::new(),
                        ..Default::default()
                    },
                    crate::xtream::models::SeriesEpisode {
                        id: "5".to_string(),
                        ..Default::default()
                    },
                ],
            )]),
            ..Series::default()
        };
        let tracks = episode_tracks(&client, &serie, &info, "Cat", ".ts");
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].uri.ends_with("/5.ts"));
    }
}
