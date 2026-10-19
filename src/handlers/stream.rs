//! Media passthrough: the retry/redirect loop the Go version ran around every upstream stream, and
//! the HLS variant that rewrites the playlist it gets back.

use crate::handlers::{error, json_error, merge_headers, Rsp};
use crate::state::AppState;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Upstream status the provider returns when the account hit its connection cap.
pub const RATE_LIMIT_STATUS: u16 = 458;
/// Pause between 458 retries, and the cap on any single wait for another client's retry.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How long a URL stays marked as rate limited once a 458 is seen.
pub const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
/// Give up on a redirect chain after this many hops.
pub const MAX_REDIRECTS: usize = 10;

/// Proxies one upstream URL back to the client, retrying rate limits and following redirects itself.
///
/// Redirects are followed server-side rather than handed to the client: IPTV clients do not always
/// follow them, and the provider's URLs are short-lived.
pub async fn stream(state: &Arc<AppState>, url: &str, request_headers: &HeaderMap) -> Rsp {
    let retry_timeout = Duration::from_secs(state.config.rate_limit_retry_timeout * 60);
    let mut current = url.to_string();
    let mut first_458: Option<Instant> = None;
    let mut redirects = 0usize;

    let response = loop {
        if redirects >= MAX_REDIRECTS {
            tracing::error!("too many redirects ({redirects}) for URL: {current}");
            return json_error(
                StatusCode::BAD_GATEWAY,
                format!("Too many redirects ({redirects}), possible redirect loop"),
            );
        }

        state
            .rate_limits
            .wait_if_rate_limited(&current, RETRY_INTERVAL)
            .await;

        let response = match forward_get(&state.stream_client, &current, request_headers).await {
            Ok(response) => response,
            Err(err) => {
                tracing::error!("upstream request for {current} failed: {err}");
                return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
            }
        };
        let status = response.status().as_u16();

        if (300..400).contains(&status) {
            match redirect_target(&current, response.headers()) {
                Some(target) => {
                    tracing::debug!("following redirect {redirects} from {url} to {target}");
                    drop(response);
                    redirects += 1;
                    first_458 = None;
                    current = target;
                    continue;
                }
                None => {
                    // A 3xx with no Location is malformed but does happen; Go streamed the body it
                    // came with instead of failing the client, so do the same.
                    tracing::warn!(
                        "WARNING: {status} response missing Location header for URL: {current}"
                    );
                }
            }
        }

        if status == RATE_LIMIT_STATUS {
            drop(response);

            let started = *first_458.get_or_insert_with(|| {
                tracing::debug!(
                    "rate limit (458) detected, will retry for up to {retry_timeout:?}"
                );
                Instant::now()
            });

            let elapsed = started.elapsed();
            if elapsed >= retry_timeout {
                tracing::error!(
                    "rate limit (458) persisted for {retry_timeout:?}, returning error"
                );
                return Response::builder()
                    .status(
                        StatusCode::from_u16(RATE_LIMIT_STATUS).unwrap_or(StatusCode::BAD_GATEWAY),
                    )
                    .body(Body::empty())
                    .unwrap_or_else(|_| {
                        error(
                            StatusCode::from_u16(RATE_LIMIT_STATUS)
                                .unwrap_or(StatusCode::BAD_GATEWAY),
                            "rate limited",
                        )
                    });
            }

            let backoff = RETRY_INTERVAL.min(retry_timeout - elapsed);
            state
                .rate_limits
                .mark_rate_limited(&current, RATE_LIMIT_COOLDOWN)
                .await;
            tracing::debug!("rate limited, retrying in {backoff:?}");
            tokio::time::sleep(backoff).await;
            continue;
        }

        state.rate_limits.clear_rate_limit(&current).await;
        break response;
    };

    let status = response.status();
    if status.as_u16() >= 400 {
        tracing::warn!("WARNING: received error status {status} from upstream for URL: {current}");
    }

    let mut headers = HeaderMap::new();
    merge_headers(
        &mut headers,
        response
            .headers()
            .iter()
            .map(|(name, value)| (Some(name.clone()), value)),
    );

    let mut builder = Response::builder().status(status);
    if let Some(headers_ref) = builder.headers_mut() {
        for (name, value) in headers {
            if let Some(name) = name {
                headers_ref.append(name, value);
            }
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

/// Issues the upstream `GET`, copying the client's headers across the way Go's `mergeHttpHeader`
/// did. `Host` and `Content-Length` are left to the client.
async fn forward_get(
    client: &reqwest::Client,
    url: &str,
    request_headers: &HeaderMap,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut builder = client.get(url);
    for (name, value) in request_headers.iter() {
        if matches!(
            name.as_str(),
            "host"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "upgrade"
                | "transfer-encoding"
                | "expect"
        ) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder.send().await
}

/// Resolves the `Location` header of a redirect against the URL that produced it.
fn redirect_target(current: &str, headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::LOCATION)?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(resolve_against(current, raw))
}

/// `net/url`'s `ResolveReference` for the cases a Location header actually contains.
fn resolve_against(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }

    let base_url = match crate::url::Url::parse(base) {
        Ok(url) => url,
        Err(_) => return location.to_string(),
    };
    let authority = format!("{}://{}{}", base_url.scheme, base_url.host, base_url.port);

    if let Some(rest) = location.strip_prefix('/') {
        return format!("{authority}/{rest}");
    }

    let directory = match base_url.path.rfind('/') {
        Some(slash) => &base_url.path[..slash + 1],
        None => "/",
    };
    format!("{authority}{directory}{location}")
}

/// Fetches an HLS playlist, records the redirect it came from and rewrites its credential paths.
///
/// Returns the rewritten playlist bytes together with the upstream `Content-Type`.
pub async fn hls_playlist(
    state: &Arc<AppState>,
    url: &str,
    request_headers: &HeaderMap,
    id: &str,
) -> Result<(Vec<u8>, Option<String>), Rsp> {
    let retry_timeout = Duration::from_secs(state.config.rate_limit_retry_timeout * 60);
    let current = url.to_string();
    let mut first_458: Option<Instant> = None;

    let response = loop {
        state
            .rate_limits
            .wait_if_rate_limited(&current, RETRY_INTERVAL)
            .await;

        let response = match forward_get(&state.hls_client, &current, request_headers).await {
            Ok(response) => response,
            Err(err) => {
                tracing::error!("hls upstream request for {current} failed: {err}");
                return Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                ));
            }
        };
        let status = response.status().as_u16();

        if status == RATE_LIMIT_STATUS {
            drop(response);
            let started = *first_458.get_or_insert(Instant::now());
            let elapsed = started.elapsed();
            if elapsed >= retry_timeout {
                return Err(Response::builder()
                    .status(
                        StatusCode::from_u16(RATE_LIMIT_STATUS).unwrap_or(StatusCode::BAD_GATEWAY),
                    )
                    .body(Body::empty())
                    .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "rate limited")));
            }
            let backoff = RETRY_INTERVAL.min(retry_timeout - elapsed);
            state
                .rate_limits
                .mark_rate_limited(&current, RATE_LIMIT_COOLDOWN)
                .await;
            tokio::time::sleep(backoff).await;
            continue;
        }

        state.rate_limits.clear_rate_limit(&current).await;
        break response;
    };

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    if status != 302 {
        // Go surfaced the status and dropped the body here.
        return Err(Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
            .body(Body::empty())
            .unwrap_or_else(|_| {
                error(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    "hls error",
                )
            }));
    }

    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let Some(location) = location else {
        tracing::error!("hls redirect without a Location header for {url}");
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to HLS stream",
        ));
    };

    if !location.contains(id) {
        tracing::error!("hls redirect {location} does not contain stream id {id}");
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to HLS stream",
        ));
    }

    state.remember_hls_redirect(id, &location);

    // Second hop: the real playlist. Same rate-limit policy.
    let playlist = loop {
        state
            .rate_limits
            .wait_if_rate_limited(&location, RETRY_INTERVAL)
            .await;

        let response = match forward_get(&state.hls_client, &location, request_headers).await {
            Ok(response) => response,
            Err(err) => {
                tracing::error!("hls playlist request for {location} failed: {err}");
                return Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                ));
            }
        };

        if response.status().as_u16() == RATE_LIMIT_STATUS {
            drop(response);
            let started = *first_458.get_or_insert(Instant::now());
            if started.elapsed() >= retry_timeout {
                return Err(Response::builder()
                    .status(
                        StatusCode::from_u16(RATE_LIMIT_STATUS).unwrap_or(StatusCode::BAD_GATEWAY),
                    )
                    .body(Body::empty())
                    .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "rate limited")));
            }
            state
                .rate_limits
                .mark_rate_limited(&location, RATE_LIMIT_COOLDOWN)
                .await;
            tokio::time::sleep(RETRY_INTERVAL).await;
            continue;
        }

        match response.bytes().await {
            Ok(bytes) => break bytes,
            Err(err) => {
                tracing::error!("cannot read hls playlist: {err}");
                return Err(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                ));
            }
        }
    };

    // Point the playlist's segment URLs back at this proxy.
    let from = format!(
        "/{}/{}?",
        state.config.xtream_user.as_str(),
        state.config.xtream_password.as_str()
    );
    let to = format!(
        "/{}/{}?",
        state.config.user.as_str(),
        state.config.password.as_str()
    );
    let text = String::from_utf8_lossy(&playlist).to_string();
    let (from_path, to_path) = (
        format!(
            "/{}/{}/",
            state.config.xtream_user.as_str(),
            state.config.xtream_password.as_str()
        ),
        format!(
            "/{}/{}/",
            state.config.user.as_str(),
            state.config.password.as_str()
        ),
    );

    let rewritten = if text.contains(&from) || text.contains(&from_path) {
        text.replace(&from, &to).replace(&from_path, &to_path)
    } else {
        text
    };

    Ok((rewritten.into_bytes(), content_type))
}

/// Builds a header map from an upstream response for a caller that already has the bytes.
pub fn response_with(content_type: Option<&str>, bytes: Vec<u8>) -> Rsp {
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(content_type) = content_type {
        if let Ok(value) = HeaderValue::from_str(content_type) {
            builder = builder.header(header::CONTENT_TYPE, value);
        }
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_resolution_matches_url_resolve_reference() {
        assert_eq!(
            resolve_against("http://a:80/x/y", "http://b:81/z"),
            "http://b:81/z"
        );
        assert_eq!(
            resolve_against("http://a:80/x/y", "/abs/z"),
            "http://a:80/abs/z"
        );
        assert_eq!(
            resolve_against("http://a:80/x/y", "rel/z"),
            "http://a:80/x/rel/z"
        );
        assert_eq!(resolve_against("http://a/x", "z"), "http://a/z");
    }

    #[test]
    fn rate_limit_constants_match_go() {
        assert_eq!(RATE_LIMIT_STATUS, 458);
        assert_eq!(RETRY_INTERVAL, Duration::from_secs(5));
        assert_eq!(RATE_LIMIT_COOLDOWN, Duration::from_secs(30));
        assert_eq!(MAX_REDIRECTS, 10);
    }
}
