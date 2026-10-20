//! HTTP handlers.
//!
//! Every handler returns a finished [`Response`] rather than a `Result`, mirroring how the Gin
//! handlers wrote the status directly. Two helper constructors ([`error`] and [`json_error`]) keep
//! the error shapes the Go version produced: plain text for aborted requests, JSON where the Go code
//! used `AbortWithStatusJSON`.

pub mod auth;
pub mod playlist;
pub mod stream;
pub mod ui;
pub mod xtream;

use crate::state::AppState;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use std::path::Path;

/// Anything a handler can hand back to a client.
pub type Rsp = Response<Body>;

/// Plain-text error, the shape Gin's `AbortWithStatus` produced.
/// Failure answer. gin's `AbortWithError` wrote the status and nothing else — the message only ever
/// reached the server log — so clients of the Go release saw an empty body here and see one now.
pub fn error(status: StatusCode, message: impl std::fmt::Display) -> Rsp {
    tracing::warn!("{}: {message}", status.as_u16());
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response_body())
}

/// JSON error, the shape Gin's `AbortWithStatusJSON` produced.
pub fn json_error(status: StatusCode, message: impl std::fmt::Display) -> Rsp {
    let body = serde_json::json!({ "error": message.to_string() });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response_body())
}

/// Minimal `IntoResponse` replacement so the builders above cannot panic on a bad header.
trait IntoResponseBody {
    fn into_response_body(self) -> Rsp;
}

impl IntoResponseBody for StatusCode {
    fn into_response_body(self) -> Rsp {
        Response::new(Body::from(self.as_u16().to_string()))
    }
}

/// A request broken into the pieces handlers need.
///
/// Authentication can depend on the body — an IPTV app posts its credentials url-encoded — so routes
/// that authenticate read the body up front and then hand it to [`check_auth`].
pub struct Req {
    pub uri: axum::http::Uri,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// Largest request body any route will buffer. Playlists and media are streamed, never buffered; only
/// the category payload and `player_api.php` POSTs land here.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Splits a request, reading the body only when `read_body` is set.
pub async fn split(request: Request, read_body: bool) -> Req {
    let (parts, body) = request.into_parts();
    let body = if read_body {
        axum::body::to_bytes(body, MAX_BODY)
            .await
            .map(|bytes| bytes.to_vec())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Req {
        uri: parts.uri,
        headers: parts.headers,
        body,
    }
}

/// Splits a request and verifies the proxy password, reading the body only where it can carry
/// credentials (a POST form), exactly like Gin's `Bind` on those routes.
pub async fn authenticated(state: &AppState, request: Request) -> Result<Req, Rsp> {
    let read_body = request.method() == axum::http::Method::POST;
    let request = split(request, read_body).await;
    check_auth(state, &request)?;
    Ok(request)
}

/// Enforces `USER`/`PASSWORD` for a route whose credentials arrive in the query or a form body.
pub fn check_auth(state: &AppState, request: &Req) -> Result<(), Rsp> {
    crate::handlers::auth::authenticate(
        &state.config,
        &request.uri,
        &request.headers,
        &request.body,
    )
}

/// Enforces the credentials an IPTV app posts inside the `player_api.php` body.
pub fn check_app_auth(state: &AppState, request: &Req) -> Result<(), Rsp> {
    crate::handlers::auth::app_authenticate(&state.config, &request.body)
}

/// Copies `src` into `dst`, adding only values not already present — Go's `mergeHttpHeader`.
///
/// Hop-by-hop headers are skipped: Go forwarded them and the framing then belonged to the wrong
/// connection (an upstream `Connection: close` would tear down an unrelated client connection).
pub fn merge_headers<'a, I>(dst: &mut HeaderMap, src: I)
where
    I: IntoIterator<Item = (Option<HeaderName>, &'a HeaderValue)>,
{
    for (name, value) in src {
        let Some(name) = name else { continue };
        if is_hop_by_hop(&name) {
            continue;
        }
        let already_present = dst.get_all(&name).iter().any(|existing| existing == value);
        if !already_present {
            dst.append(name, value.clone());
        }
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    )
}

/// Range-aware file server shared by every playlist route.
///
/// `extra` headers are written first (the `Content-Disposition` / `Content-Type` pair the Go handlers
/// set before calling `ctx.File`). A single `Range` is honoured; a malformed or unsatisfiable range
/// is ignored and the whole file is sent, which is what `http.ServeContent` effectively did for the
/// shapes IPTV clients send.
pub async fn serve_file(
    path: &Path,
    extra: Vec<(&'static str, String)>,
    request_headers: &HeaderMap,
) -> Rsp {
    let length = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            tracing::error!("cannot read {}: {err}", path.display());
            return error(StatusCode::NOT_FOUND, "Not Found");
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    for (name, value) in extra {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_lowercase(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }

    let (start, end) = resolve_range(
        request_headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_range),
        length,
    );

    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(err) => {
            tracing::error!("cannot open {}: {err}", path.display());
            return error(StatusCode::NOT_FOUND, "Not Found");
        }
    };

    use tokio::io::AsyncSeekExt;
    let mut file = file;
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    }

    let limited = tokio::io::AsyncReadExt::take(file, end - start + 1);
    let content_length = end - start + 1;
    let stream = tokio_util::io::ReaderStream::new(limited);

    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).expect("digits are valid"),
    );
    let partial = start > 0 || end + 1 < length;
    if partial {
        let value = format!("bytes {start}-{end}/{length}");
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }

    let status = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

/// Turns a parsed range into concrete byte bounds against a file of `length` bytes.
///
/// The `(u64::MAX, n)` marker produced by [`parse_range`] means "the last n bytes".
fn resolve_range(range: Option<(u64, u64)>, length: u64) -> (u64, u64) {
    let whole = (0, length.saturating_sub(1));
    match range {
        Some((u64::MAX, suffix)) if suffix > 0 => {
            let start = length.saturating_sub(suffix);
            (start, length.saturating_sub(1))
        }
        Some((start, end)) if start < length => (start, end.min(length.saturating_sub(1))),
        _ => whole,
    }
}

/// Parses `bytes=start-end`, including the open-ended and suffix forms.
fn parse_range(value: &str) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    // Only single ranges, like Go's ServeContent which ignores multi-range requests.
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start = start.trim();
    let end = end.trim();

    match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let start: u64 = start.parse().ok()?;
            let end: u64 = end.parse().ok()?;
            if end < start {
                return None;
            }
            Some((start, end))
        }
        (false, true) => start.parse::<u64>().ok().map(|start| (start, u64::MAX)),
        (true, false) => {
            let suffix: u64 = end.parse().ok()?;
            if suffix == 0 {
                None
            } else {
                Some((u64::MAX, suffix)) // marker, resolved by the caller against the file size
            }
        }
        (true, true) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-49"), Some((0, 49)));
        assert_eq!(parse_range("bytes=100-"), Some((100, u64::MAX)));
        assert_eq!(parse_range("bytes=-200"), Some((u64::MAX, 200)));
        assert_eq!(parse_range("bytes=50-10"), None);
        assert_eq!(parse_range("bytes=0-1,5-9"), None);
        assert_eq!(parse_range("items=0-1"), None);
    }

    #[test]
    fn ranges_resolve_against_the_file_length() {
        assert_eq!(resolve_range(None, 100), (0, 99));
        assert_eq!(resolve_range(Some((0, u64::MAX)), 100), (0, 99));
        assert_eq!(resolve_range(Some((50, u64::MAX)), 100), (50, 99));
        assert_eq!(resolve_range(Some((50, 5000)), 100), (50, 99));
        assert_eq!(resolve_range(Some((u64::MAX, 20)), 100), (80, 99));
        // Unsatisfiable start falls back to the whole file, like Go ignoring the range.
        assert_eq!(resolve_range(Some((500, u64::MAX)), 100), (0, 99));
        assert_eq!(resolve_range(None, 0), (0, 0));
    }

    #[test]
    fn merge_headers_adds_missing_values_only() {
        let mut dst = HeaderMap::new();
        dst.insert("x-a", HeaderValue::from_static("1"));

        let mut src = HeaderMap::new();
        src.append("x-a", HeaderValue::from_static("1"));
        src.append("x-a", HeaderValue::from_static("2"));
        src.insert("x-b", HeaderValue::from_static("3"));
        src.insert(header::CONNECTION, HeaderValue::from_static("close"));

        merge_headers(&mut dst, src.iter().map(|(k, v)| (Some(k.clone()), v)));

        assert_eq!(
            dst.get_all("x-a").iter().collect::<Vec<_>>(),
            vec![HeaderValue::from_static("1"), HeaderValue::from_static("2")]
        );
        assert_eq!(dst.get("x-b").unwrap(), "3");
        assert!(dst.get(header::CONNECTION).is_none());
    }
}
