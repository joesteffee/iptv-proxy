//! Playlist routes: the pre-baked file for a plain M3U source, the provider passthrough, and the
//! playlist generated from the Xtream API.

use crate::config::ProxyConfig;
use crate::generate;
use crate::handlers::{authenticated, error, serve_file, Req, Rsp};
use crate::m3u::{self, Playlist};
use crate::rewrite::rewrite_uri;
use crate::state::AppState;
use crate::url::parse_query;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

/// Cache name of the playlist built once at startup from `M3U_URL` in plain-M3U mode.
pub const BOOT_PLAYLIST: &str = "boot";

/// Serves the playlist captured at startup for a plain (non-Xtream) M3U source.
pub async fn get_m3u(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = match authenticated(&state, request).await {
        Ok(request) => request,
        Err(response) => return response,
    };

    match state.m3u_cache.path(BOOT_PLAYLIST) {
        Some(path) => serve_file(&path, m3u_headers(&state.config), &request.headers).await,
        None => {
            tracing::error!("no cached playlist available");
            error(StatusCode::NOT_FOUND, "Not Found")
        }
    }
}

/// `GET /{M3U_FILE_NAME}` in Xtream-auto mode: fetch the provider's own `get.php`, rewrite every
/// track URL so the client comes back through this proxy, and cache the result.
pub async fn xtream_get(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = match authenticated(&state, request).await {
        Ok(request) => request,
        Err(response) => return response,
    };

    let query = request.uri.query().map(parse_query).unwrap_or_default();
    serve_passthrough(&state, query, request).await
}

/// Same as [`xtream_get`], but the query parameters configured in `M3U_URL` are merged into the
/// client's request first — that is what lets an account pinned to `type=m3u_plus&output=ts`
/// downstream keep working when the app asks for a bare playlist.
pub async fn xtream_get_auto(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = match authenticated(&state, request).await {
        Ok(request) => request,
        Err(response) => return response,
    };

    let mut query = request.uri.query().map(parse_query).unwrap_or_default();
    for (name, values) in remote_query_groups(&state.config) {
        if name == "username" || name == "password" {
            continue;
        }
        query.push((name, values.join(",")));
    }

    serve_passthrough(&state, query, request).await
}

/// `GET /get.php` and `GET /apiget`: build a playlist by querying the Xtream API, so the client gets
/// only the categories it enabled and URLs that point at this proxy.
pub async fn xtream_api_get(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = match authenticated(&state, request).await {
        Ok(request) => request,
        Err(response) => return response,
    };

    let query = request.uri.query().map(parse_query).unwrap_or_default();
    let output = query
        .iter()
        .find(|(name, _)| name == "output")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    let cache_name = format!("apiget{output}");

    let building = Arc::clone(&state);
    let built_output = output;
    let built_key = cache_name.clone();
    let result = serve_cached_playlist(&state, &cache_name, move || async move {
        generate_playlist(&building, &built_key, &built_output).await
    })
    .await;

    match result {
        Ok(path) => serve_file(&path, m3u_headers(&state.config), &request.headers).await,
        Err(response) => response,
    }
}

async fn serve_passthrough(
    state: &Arc<AppState>,
    query: Vec<(String, String)>,
    request: Req,
) -> Rsp {
    let upstream = upstream_get_php(&state.config, &query);

    // `upstream` doubles as the cache key: two clients asking for different `type`/`output`
    // combinations legitimately get different files.
    let building = Arc::clone(state);
    let built_url = upstream.clone();
    let result = serve_cached_playlist(state, &upstream, move || async move {
        fetch_passthrough_playlist(&building, &built_url).await
    })
    .await;

    match result {
        Ok(path) => serve_file(&path, m3u_headers(&state.config), &request.headers).await,
        Err(response) => response,
    }
}

/// Returns a fresh cached playlist, or builds one exactly once even when several clients ask at the
/// same time.
async fn serve_cached_playlist<F, Fut>(
    state: &Arc<AppState>,
    key: &str,
    build: F,
) -> Result<PathBuf, Rsp>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<PathBuf, Rsp>> + Send + 'static,
{
    let expiration = state.config.m3u_cache_expiration;
    if let Some(path) = state.m3u_cache.fresh_path(key, expiration) {
        return Ok(path);
    }

    // A playlist build is the most expensive thing this service does, so it runs detached from the
    // request: an IPTV app gives up long before a huge catalogue finishes, and hanging up must not
    // throw away the fetching already done. Concurrent clients still wait for one build, because the
    // work holds the keyed lock.
    let running = Arc::clone(state);
    let lock_key = key.to_string();
    let check_key = key.to_string();

    let built = tokio::spawn(async move {
        running
            .locks
            .with(&lock_key, || {
                let running = Arc::clone(&running);
                async move {
                    if let Some(path) = running.m3u_cache.fresh_path(&check_key, expiration) {
                        return Ok(path);
                    }
                    build().await
                }
            })
            .await
    });

    built.await.map_err(|err| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("playlist build failed: {err}"),
        )
    })?
}

/// Fetches the provider playlist, validates it, and stores the rewritten copy.
async fn fetch_passthrough_playlist(state: &Arc<AppState>, url: &str) -> Result<PathBuf, Rsp> {
    let response = crate::http::api_client()
        .get(url)
        .send()
        .await
        .map_err(|err| {
            tracing::error!("Failed to fetch M3U from Xtream: {err}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        })?;

    let status = response.status();
    let body = response.bytes().await.map_err(|err| {
        tracing::error!("Failed to read M3U response: {err}");
        error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    })?;

    let text = String::from_utf8_lossy(&body).to_string();
    let trimmed = text.trim();

    if !status.is_success() {
        tracing::error!(
            "Xtream server returned status {} for M3U request. Response: {}",
            status.as_u16(),
            truncate(trimmed, 500)
        );
        tracing::debug!("DEBUG: Request URL was: {url}");

        // Some providers answer with a non-standard status but a usable playlist.
        if trimmed.contains("#EXTM3U") {
            tracing::warn!(
                "Non-200 status but response contains #EXTM3U, attempting to parse anyway"
            );
        } else {
            return Err(Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap_or_else(|_| error(status, "error")));
        }
    }

    if trimmed.is_empty() || trimmed == "#EXTM3U" || !trimmed.contains("#EXTINF") {
        tracing::warn!(
            "Xtream server returned empty or invalid M3U file (length: {} bytes). Response preview: {}",
            body.len(),
            truncate(trimmed, 200)
        );
        tracing::debug!("DEBUG: Request URL was: {url}");
    }

    let playlist = m3u::parse(&text).map_err(|err| {
        tracing::error!("Failed to parse M3U content: {err}");
        error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    })?;

    if playlist.is_empty() {
        tracing::warn!("Parsed M3U playlist contains 0 tracks. This might indicate an authentication or configuration issue.");
    } else {
        tracing::info!("Successfully parsed M3U with {} tracks", playlist.len());
    }

    store_rewritten(state, url, &playlist).await
}

/// Builds the playlist from the Xtream API and stores it.
async fn generate_playlist(state: &Arc<AppState>, key: &str, output: &str) -> Result<PathBuf, Rsp> {
    let tracks = generate::generate_tracks(state, output)
        .await
        .map_err(|err| {
            tracing::error!("unable to generate playlist: {err}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        })?;

    tracing::info!("Generated playlist with {} tracks", tracks.len());
    store_rewritten(state, key, &Playlist { tracks }).await
}

/// Serialises `playlist` with every URI pointed at this proxy, then caches it under `key`.
async fn store_rewritten(
    state: &Arc<AppState>,
    key: &str,
    playlist: &Playlist,
) -> Result<PathBuf, Rsp> {
    let config = Arc::clone(&state.config);
    let anti_collision = state.anti_collision.clone();

    let mut out = Vec::with_capacity(playlist.len() * 160 + 32);
    let kept = m3u::marshall_into(&mut Cursor::new(&mut out), playlist, |uri, index| {
        rewrite_uri(&config, &anti_collision, uri, index, true)
    })
    .expect("writing into a Vec cannot fail");

    if kept.len() != playlist.len() {
        tracing::warn!(
            "{} track(s) dropped while rewriting the playlist",
            playlist.len() - kept.len()
        );
    }

    state.m3u_cache.store(key, out).await.map_err(|err| {
        tracing::error!("Failed to cache M3U: {err}");
        error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    })
}

/// Loads `M3U_URL` once at startup for plain-M3U mode: parse, rewrite every URI to this proxy, cache
/// the file, and keep the surviving tracks so their stream routes can be registered.
///
/// Returns the number of tracks. In Xtream-auto mode this is skipped entirely — the Go version
/// downloaded that playlist on every start even though nothing ever served it.
pub async fn bootstrap(state: &Arc<AppState>) -> anyhow::Result<usize> {
    if state.config.remote_url.is_empty() {
        anyhow::bail!("M3U_URL is not set");
    }
    // Same rule as the vendored `m3u.Parse`: an `http://`/`https://` prefix means download, anything
    // else is a path on disk. `M3U_URL=/root/iptv/iptv.m3u` is the packaged default and has no
    // scheme to parse, so the prefix check happens on the raw string.
    let remote_url = state.config.remote_url.as_str();
    let text = if remote_url.starts_with("http://") || remote_url.starts_with("https://") {
        tracing::info!("downloading playlist from {remote_url}");
        let response = crate::http::api_client().get(remote_url).send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            anyhow::bail!("M3U_URL returned status {}", status.as_u16());
        }
        String::from_utf8_lossy(&bytes).to_string()
    } else {
        tracing::info!("reading playlist from {remote_url}");
        tokio::fs::read_to_string(remote_url)
            .await
            .map_err(|err| anyhow::anyhow!("cannot read {remote_url}: {err}"))?
    };

    let playlist = m3u::parse(&text)?;
    let total = playlist.len();

    let config = Arc::clone(&state.config);
    let anti_collision = state.anti_collision.clone();

    let mut out = Vec::with_capacity(total * 160 + 32);
    let kept = m3u::marshall_into(&mut Cursor::new(&mut out), &playlist, |uri, index| {
        rewrite_uri(&config, &anti_collision, uri, index, false)
    })?;

    state.m3u_cache.store(BOOT_PLAYLIST, out).await?;
    *state.tracks.write().expect("tracks lock") = kept;

    tracing::info!(
        "playlist ready: {} of {total} tracks served",
        state.tracks.read().expect("tracks lock").len()
    );
    Ok(total)
}

/// The query groups of `M3U_URL`, in the order they appear.
fn remote_query_groups(config: &ProxyConfig) -> Vec<(String, Vec<String>)> {
    let Ok(remote) = crate::url::Url::parse(&config.remote_url) else {
        return Vec::new();
    };
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in remote.query_pairs() {
        match groups.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, values)) => values.push(value),
            None => groups.push((name, vec![value])),
        }
    }
    groups
}

/// Assembles the upstream `get.php` URL: configured credentials plus the client's own parameters.
fn upstream_get_php(config: &ProxyConfig, query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(query.len() + 2);
    for (name, value) in query {
        if name == "username" || name == "password" {
            continue;
        }
        pairs.push((name.clone(), value.clone()));
    }
    // The configured provider credentials always win over anything the client sent.
    pairs.push((
        "username".to_string(),
        config.xtream_user.as_str().to_string(),
    ));
    pairs.push((
        "password".to_string(),
        config.xtream_password.as_str().to_string(),
    ));

    let encoded: Vec<String> = pairs
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                crate::xtream::percent_encode(name),
                crate::xtream::percent_encode(value)
            )
        })
        .collect();

    format!("{}/get.php?{}", config.xtream_base_url, encoded.join("&"))
}

/// The two headers Go's handlers set before handing the file to `ctx.File`.
pub fn m3u_headers(config: &ProxyConfig) -> Vec<(&'static str, String)> {
    vec![
        (
            "content-disposition",
            format!(
                "attachment; filename={}",
                m3u::quote_value(&config.m3u_file_name)
            ),
        ),
        ("content-type", "application/octet-stream".to_string()),
    ]
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(remote: &str) -> ProxyConfig {
        let raw = vec![
            "--m3u-url".to_string(),
            remote.to_string(),
            "--hostname".to_string(),
            "proxy.test".to_string(),
            "--xtream-base-url".to_string(),
            "http://provider.test:8080".to_string(),
            "--xtream-user".to_string(),
            "xu".to_string(),
            "--xtream-password".to_string(),
            "xp".to_string(),
        ];
        crate::config::build(&raw).expect("config builds")
    }

    #[test]
    fn upstream_url_keeps_client_params_and_forces_provider_credentials() {
        let config =
            config("http://provider.test:8080/get.php?username=a&password=b&type=m3u_plus");
        let query = vec![
            ("username".to_string(), "spoofed".to_string()),
            ("type".to_string(), "m3u".to_string()),
            ("output".to_string(), "ts".to_string()),
        ];
        let url = upstream_get_php(&config, &query);
        assert!(url.starts_with("http://provider.test:8080/get.php?"));
        assert!(url.contains("type=m3u"), "{url}");
        assert!(url.contains("output=ts"), "{url}");
        assert!(url.contains("username=xu"), "{url}");
        assert!(url.contains("password=xp"), "{url}");
        assert!(!url.contains("spoofed"), "{url}");
    }

    #[test]
    fn provider_credentials_are_percent_encoded() {
        let mut config = config("http://provider.test/get.php");
        config.xtream_user = crate::config::CredentialString::new("user name&x");
        config.xtream_password = crate::config::CredentialString::new("p/ss+1");
        let url = upstream_get_php(&config, &[]);
        assert!(url.contains("username=user%20name%26x"), "{url}");
        assert!(url.contains("password=p%2Fss%2B1"), "{url}");
    }

    #[test]
    fn remote_query_groups_preserve_order_and_duplicates() {
        let config = config("http://p/get.php?username=u&password=p&type=m3u_plus&output=ts");
        let groups = remote_query_groups(&config);
        let names: Vec<&str> = groups.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["username", "password", "type", "output"]);
    }

    #[test]
    fn content_disposition_quotes_the_file_name() {
        let config = config("http://p/iptv.m3u");
        let headers = m3u_headers(&config);
        assert_eq!(headers[0].0, "content-disposition");
        assert_eq!(headers[0].1, r#"attachment; filename="iptv.m3u""#);
        assert_eq!(headers[1].1, "application/octet-stream");
    }

    #[test]
    fn truncation_is_char_safe() {
        // Go slices bytes (`body[:500]`) and can cut a rune in half; we snap back to a boundary so
        // the log line stays valid UTF-8. The byte budget itself is unchanged.
        assert_eq!(truncate("héllo", 3), "hé");
        assert_eq!(truncate("héllo", 4), "hél");
        assert_eq!(truncate("héllo", 5), "héll");
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("héllo", 0), "");
    }
}
