//! Xtream Codes API client.
//!
//! The Go version built on `github.com/tellytv/go.xtream-codes`, which performs a full
//! authentication request to `player_api.php` inside `New()`. Because a fresh client was created
//! for every playlist generation, every category list and every `player_api.php` call, each proxied
//! API request cost two upstream round trips. Here the client is created once, held in
//! [`crate::state::AppState`], and the authentication payload is fetched lazily and cached.

pub mod models;

use crate::config::CredentialString;
use anyhow::{anyhow, Result};
use reqwest::{header, Response, StatusCode};
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

pub use models::*;

/// User agent the Go version sent for every Xtream request. Cloudflare-fronted providers reject the
/// Go default agent, and the incoming client agent is deliberately ignored.
pub const XTREAM_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Status `tellytv` treated as fatal: anything above 399.
const MAX_OK_STATUS: u16 = 399;

/// Errors returned by the Xtream upstream.
#[derive(Debug)]
pub enum XtreamError {
    /// The upstream answered with a status above 399.
    Status(StatusCode),
    /// The request could not be completed.
    Transport(reqwest::Error),
    /// The payload could not be decoded.
    Decode(serde_json::Error),
    /// A caller passed an unusable value (empty series id, bad limit, ...).
    BadInput(String),
}

impl fmt::Display for XtreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The wording matches the Go client: callers substring-match on "520" and friends to
            // decide whether an error is worth retrying.
            XtreamError::Status(status) => {
                write!(f, "status code was {}, expected 2XX-3XX", status.as_u16())
            }
            XtreamError::Transport(err) => write!(f, "cannot reach server. {err}"),
            XtreamError::Decode(err) => write!(f, "error unmarshaling json: {err}"),
            XtreamError::BadInput(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for XtreamError {}

impl XtreamError {
    /// True for the transient Cloudflare/origin statuses the Go version retried.
    pub fn is_transient(&self) -> bool {
        match self {
            XtreamError::Status(status) => {
                matches!(status.as_u16(), 500 | 502 | 503 | 520)
            }
            _ => false,
        }
    }
}

/// A client bound to one Xtream account. Cheap to clone: it shares the underlying connection pool.
#[derive(Clone)]
pub struct XtreamClient {
    http: reqwest::Client,
    base_url: String,
    username: CredentialString,
    password: CredentialString,
}

impl XtreamClient {
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        username: CredentialString,
        password: CredentialString,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username,
            password,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn username(&self) -> &CredentialString {
        &self.username
    }

    pub fn password(&self) -> &CredentialString {
        &self.password
    }

    /// Builds a `player_api.php` URL. Credentials are percent-encoded, which the Go client did not
    /// do for this endpoint (it interpolated them raw into the query string).
    fn api_url(&self, action: Option<&str>, params: &[(&str, String)]) -> String {
        let mut url = format!(
            "{}/player_api.php?username={}&password={}",
            self.base_url,
            percent_encode(self.username.as_str()),
            percent_encode(self.password.as_str())
        );
        if let Some(action) = action {
            url.push_str("&action=");
            url.push_str(&percent_encode(action));
        }
        for (key, value) in params {
            url.push('&');
            url.push_str(&percent_encode(key));
            url.push('=');
            url.push_str(&percent_encode(value));
        }
        url
    }

    async fn send(&self, url: &str) -> Result<Response, XtreamError> {
        let response = self
            .http
            .get(url)
            .header(header::USER_AGENT, XTREAM_USER_AGENT)
            .send()
            .await
            .map_err(XtreamError::Transport)?;

        if response.status().as_u16() > MAX_OK_STATUS {
            return Err(XtreamError::Status(response.status()));
        }
        Ok(response)
    }

    async fn bytes(&self, url: &str) -> Result<bytes::Bytes, XtreamError> {
        self.send(url)
            .await?
            .bytes()
            .await
            .map_err(XtreamError::Transport)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        action: Option<&str>,
        params: &[(&str, String)],
    ) -> Result<T, XtreamError> {
        let url = self.api_url(action, params);
        let body = self.bytes(&url).await?;
        serde_json::from_slice(&body).map_err(XtreamError::Decode)
    }

    /// Requests an action, retrying transient failures the way the Go code did: three attempts,
    /// `attempt * backoff` seconds apart.
    pub async fn retry<T, F, Fut>(
        &self,
        attempts: u32,
        backoff: Duration,
        mut f: F,
    ) -> Result<T, XtreamError>
    where
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<T, XtreamError>>,
    {
        let mut attempt = 1u32;
        loop {
            match f(attempt).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < attempts && err.is_transient() => {
                    tracing::warn!(
                        "transient error on attempt {attempt}/{attempts}, retrying: {err}"
                    );
                    tokio::time::sleep(backoff * attempt).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// `player_api.php` with no action: the authentication payload.
    pub async fn authenticate(&self) -> Result<AuthenticationResponse, XtreamError> {
        self.get_json(None, &[]).await
    }

    /// `get_{live,vod,series}_categories`.
    pub async fn categories(&self, kind: &str) -> Result<Vec<Category>, XtreamError> {
        self.get_json(Some(&format!("get_{kind}_categories")), &[])
            .await
    }

    /// `get_live_streams` / `get_vod_streams`. The series variant has no `_streams` suffix.
    pub async fn streams(&self, kind: &str, category_id: &str) -> Result<Vec<Stream>, XtreamError> {
        let action = if kind == "series" {
            "get_series".to_string()
        } else {
            format!("get_{kind}_streams")
        };
        let params = category_param(category_id);
        self.get_json(Some(&action), &params).await
    }

    /// `get_series` for one category.
    pub async fn series(&self, category_id: &str) -> Result<Vec<SeriesInfo>, XtreamError> {
        let params = category_param(category_id);
        self.get_json(Some("get_series"), &params).await
    }

    /// `get_series_info` for one series.
    pub async fn series_info(&self, series_id: &str) -> Result<Series, XtreamError> {
        if series_id.is_empty() {
            return Err(XtreamError::BadInput(
                "series ID can not be empty".to_string(),
            ));
        }
        let params = [("series_id", series_id.to_string())];
        self.get_json(Some("get_series_info"), &params).await
    }

    /// `get_vod_info`: forwarded verbatim, so the whole object is kept as JSON.
    pub async fn vod_info(&self, vod_id: &str) -> Result<serde_json::Value, XtreamError> {
        let params = [("vod_id", vod_id.to_string())];
        self.get_json(Some("get_vod_info"), &params).await
    }

    /// `get_short_epg`. `limit == 0` means "let the server choose", matching the Go behaviour where
    /// the parameter is only sent when the client supplied one.
    pub async fn short_epg(
        &self,
        stream_id: &str,
        limit: i64,
    ) -> Result<Vec<EpgInfo>, XtreamError> {
        let mut params = vec![("stream_id", stream_id.to_string())];
        if limit > 0 {
            params.push(("limit", limit.to_string()));
        }
        let container: EpgContainer = self.get_json(Some("get_short_epg"), &params).await?;
        Ok(container.epg_listings)
    }

    /// `get_simple_data_table`: the full EPG for one stream.
    pub async fn epg(&self, stream_id: &str) -> Result<Vec<EpgInfo>, XtreamError> {
        let params = [("stream_id", stream_id.to_string())];
        let container: EpgContainer = self
            .get_json(Some("get_simple_data_table"), &params)
            .await?;
        Ok(container.epg_listings)
    }

    /// `xmltv.php`, returned as raw bytes.
    pub async fn xmltv(&self) -> Result<bytes::Bytes, XtreamError> {
        let url = format!(
            "{}/xmltv.php?username={}&password={}",
            self.base_url,
            percent_encode(self.username.as_str()),
            percent_encode(self.password.as_str())
        );
        self.bytes(&url).await
    }

    /// Runs an arbitrary action with the given query, returning the upstream JSON untouched. Used by
    /// the `player_api.php` proxy for actions with no dedicated model.
    pub async fn raw_action(
        &self,
        action: &str,
        params: Vec<(String, String)>,
    ) -> Result<serde_json::Value, XtreamError> {
        let owned: Vec<(&str, String)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        self.get_json(Some(action), &owned).await
    }

    /// Fetches `text/plain`-ish payloads such as the M3U produced by `get.php`.
    pub async fn text(&self, url: &str) -> Result<bytes::Bytes, XtreamError> {
        self.bytes(url).await
    }
}

fn category_param(category_id: &str) -> Vec<(&'static str, String)> {
    if category_id.is_empty() {
        Vec::new()
    } else {
        vec![("category_id", category_id.to_string())]
    }
}

/// The `{ "epg_listings": [...] }` envelope around EPG responses.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct EpgContainer {
    #[serde(rename = "epg_listings", default)]
    pub epg_listings: Vec<EpgInfo>,
}

/// Escapes a query component the way Go's `url.QueryEscape` does (alphanumerics and `-_.~` are
/// kept), except that a space becomes `%20` rather than `+`. Both decode to a space everywhere a
/// query is parsed, and `%20` never gets mangled by an intermediary that treats `+` literally.
pub fn percent_encode(value: &str) -> String {
    const QUERY: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, QUERY).to_string()
}

/// Convenience used by handlers that must turn any error into a 500 with a log line.
pub fn describe(err: &XtreamError) -> anyhow::Error {
    anyhow!("{err}").context("xtream request failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> XtreamClient {
        XtreamClient::new(
            reqwest::Client::new(),
            "http://provider.example:8080/",
            CredentialString::new("user&x"),
            CredentialString::new("p ss"),
        )
    }

    #[test]
    fn api_url_encodes_credentials_and_action() {
        let url = client().api_url(
            Some("get_live_streams"),
            &[("category_id", "12".to_string())],
        );
        assert_eq!(
            url,
            "http://provider.example:8080/player_api.php?username=user%26x&password=p%20ss&action=get_live_streams&category_id=12"
        );
    }

    #[test]
    fn api_url_without_action_or_params() {
        assert_eq!(
            client().api_url(None, &[]),
            "http://provider.example:8080/player_api.php?username=user%26x&password=p%20ss"
        );
    }

    #[test]
    fn transient_statuses_are_the_cloudflare_set() {
        for code in [500u16, 502, 503, 520] {
            assert!(
                XtreamError::Status(StatusCode::from_u16(code).unwrap()).is_transient(),
                "{code} should retry"
            );
        }
        assert!(!XtreamError::Status(StatusCode::NOT_FOUND).is_transient());
        assert!(!XtreamError::BadInput("x".into()).is_transient());
    }

    #[test]
    fn error_text_matches_the_go_client() {
        let err = XtreamError::Status(StatusCode::from_u16(520).unwrap());
        assert_eq!(err.to_string(), "status code was 520, expected 2XX-3XX");
    }

    #[tokio::test]
    async fn retry_stops_after_the_configured_attempts() {
        let client = client();
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result: Result<(), XtreamError> = client
            .retry(3, Duration::from_millis(1), |_| async {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(XtreamError::Status(StatusCode::NOT_FOUND))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_retries_transient_failures() {
        let client = client();
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result: Result<(), XtreamError> = client
            .retry(3, Duration::from_millis(1), |_| async {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err(XtreamError::Status(StatusCode::from_u16(520).unwrap()))
                } else {
                    Ok(())
                }
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
