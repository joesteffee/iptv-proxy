//! Xtream endpoints: the `player_api.php` façade, the EPG, and the media passthrough routes.

use crate::handlers::stream::{hls_playlist, response_with, stream};
use crate::handlers::{authenticated, error, json_error, Rsp};
use crate::state::AppState;
use crate::url::parse_query;
use crate::xtream::models::{FlexInt, ServerInfo, UserInfo};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response as AxumResponse;
use serde_json::{json, Value};
use std::sync::Arc;

/// Runs `handler` only once the client's credentials check out.
macro_rules! authed {
    ($state:expr, $request:expr) => {
        match authenticated($state, $request).await {
            Ok(request) => request,
            Err(response) => return response,
        }
    };
}

/// `GET /xmltv.php`.
pub async fn xmltv(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    authed!(&state, request);

    let Some(client) = state.xtream.as_ref() else {
        return error(StatusCode::BAD_REQUEST, "Xtream backend is not configured");
    };

    match client.xmltv().await {
        Ok(bytes) => AxumResponse::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/xml")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")),
        Err(err) => {
            tracing::error!("xmltv failed: {err}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

/// `GET /player_api.php` — credentials arrive in the query string.
pub async fn player_api_get(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = authed!(&state, request);
    let params = request
        .uri
        .query()
        .map(parse_query)
        .unwrap_or_else(|| vec![(String::new(), String::new())]);
    player_api(&state, &params).await
}

/// `POST /player_api.php` — credentials arrive in the url-encoded body, like the IPTV apps send them.
pub async fn player_api_post(State(state): State<Arc<AppState>>, request: Request) -> Rsp {
    let request = authed!(&state, request);
    let params = parse_query(String::from_utf8_lossy(&request.body).as_ref());
    player_api(&state, &params).await
}

/// Answers one `player_api.php` action the way the provider would, but with this proxy's
/// credentials, host and port substituted — that is what keeps an IPTV app streaming through us
/// after it "logs in".
async fn player_api(state: &Arc<AppState>, params: &[(String, String)]) -> Rsp {
    let Some(client) = state.xtream.as_ref() else {
        return error(StatusCode::BAD_REQUEST, "Xtream backend is not configured");
    };

    let value = match param(params, "action").as_deref() {
        Some("get_live_categories") => value_of(client.categories("live").await),
        Some("get_vod_categories") => value_of(client.categories("vod").await),
        Some("get_series_categories") => value_of(client.categories("series").await),
        Some("get_live_streams") => match required(params, "category_id") {
            Some(id) => value_of(client.streams("live", &id).await),
            None => return missing_param("category_id"),
        },
        Some("get_vod_streams") => match required(params, "category_id") {
            Some(id) => value_of(client.streams("vod", &id).await),
            None => return missing_param("category_id"),
        },
        Some("get_series") => match required(params, "category_id") {
            Some(id) => value_of(client.series(&id).await),
            None => return missing_param("category_id"),
        },
        Some("get_series_info") => match required(params, "series_id") {
            Some(id) => value_of(crate::generate::series_info(state, client, &id).await),
            None => return missing_param("series_id"),
        },
        Some("get_vod_info") => match required(params, "vod_id") {
            Some(id) => value_of(client.vod_info(&id).await),
            None => return missing_param("vod_id"),
        },
        Some("get_short_epg") => match required(params, "stream_id") {
            Some(id) => value_of(client.short_epg(&id, limit(params)).await),
            None => return missing_param("stream_id"),
        },
        Some("get_simple_data_table") => match required(params, "stream_id") {
            Some(id) => value_of(client.epg(&id).await),
            None => return missing_param("stream_id"),
        },
        // No action (or an unknown one) is a login: the app expects credentials back.
        _ => login(state).await,
    };

    AxumResponse::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap_or_else(
            |err| format!(r#"{{"error":"{err}"}}"#).into_bytes(),
        )))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

/// The rewritten login response: the app stores these credentials and uses them for every later
/// request, so they must point back here — proxy credentials, proxy host, proxy port.
async fn login(state: &Arc<AppState>) -> Value {
    let config = &state.config;
    let advertised = FlexInt(config.advertised_port as i64);
    let protocol = if config.https { "https" } else { "http" };

    let (user_info, server_info) = match state.authenticate().await {
        Ok(auth) => {
            let mut user = auth.user_info;
            user.username = config.user.as_str().to_string();
            user.password = config.password.as_str().to_string();

            let mut server = auth.server_info;
            server.url = config.advertised_base();
            server.port = advertised;
            server.https_port = advertised;
            server.rtmp_port = FlexInt(0);
            server.protocol = protocol.to_string();
            (user, server)
        }
        Err(err) => {
            // The Go version let the login error surface as an empty user info; clients read the
            // `status`/`auth` fields to decide whether the credentials worked, so say so explicitly.
            tracing::error!("provider login failed: {err}");
            (
                UserInfo {
                    username: config.user.as_str().to_string(),
                    password: config.password.as_str().to_string(),
                    status: "Mismatched Login".to_string(),
                    message: err.to_string(),
                    auth: crate::xtream::models::FlexBool(false),
                    ..UserInfo::default()
                },
                ServerInfo {
                    url: config.advertised_base(),
                    port: advertised,
                    https_port: advertised,
                    rtmp_port: FlexInt(0),
                    protocol: protocol.to_string(),
                    timezone: "UTC".to_string(),
                    ..ServerInfo::default()
                },
            )
        }
    };

    json!({ "user_info": user_info, "server_info": server_info })
}

fn value_of<T, E>(result: Result<T, E>) -> Value
where
    T: serde::Serialize,
    E: std::fmt::Display,
{
    match result {
        Ok(value) => {
            serde_json::to_value(&value).unwrap_or_else(|err| json!({"error": err.to_string()}))
        }
        Err(err) => {
            tracing::error!("upstream request failed: {err}");
            json!({"error": err.to_string()})
        }
    }
}

fn param(params: &[(String, String)], name: &str) -> Option<String> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

fn required(params: &[(String, String)], name: &str) -> Option<String> {
    let value = param(params, name)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn limit(params: &[(String, String)]) -> i64 {
    param(params, "limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn missing_param(name: &str) -> Rsp {
    let mut response = json_error(StatusCode::BAD_REQUEST, format!("missing \"{name}\""));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}

// ---------------------------------------------------------------------------
// Media passthrough
// ---------------------------------------------------------------------------
//
// Go registered these routes with the proxy's own credentials baked into the path
// (`/live/{user}/{password}/:id`) and no authenticate middleware, so matching the route *is* the
// authorisation. Whatever the client put in the path is discarded: the provider is always
// contacted with the configured Xtream credentials.

/// `{user}/{password}` as it appears in a provider path.
fn provider_credentials(state: &Arc<AppState>) -> String {
    format!(
        "{}/{}",
        state.config.xtream_user.path_escape(),
        state.config.xtream_password.path_escape()
    )
}

/// `GET /{user}/{password}/{ID}`.
pub async fn stream_generic(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Rsp {
    let url = format!(
        "{}/{}/{}",
        state.config.xtream_base_url,
        provider_credentials(&state),
        id
    );
    dispatch(&state, &url, &id, &headers).await
}

/// `GET /live/{user}/{password}/{ID}`. A trailing `.ts` is stripped — the Xtream live endpoint
/// takes the bare stream id, and player apps append the extension anyway.
pub async fn stream_live(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Rsp {
    let bare = id.strip_suffix(".ts").unwrap_or(&id);
    let url = format!(
        "{}/{}/{}",
        state.config.xtream_base_url,
        provider_credentials(&state),
        bare
    );
    dispatch(&state, &url, &id, &headers).await
}

/// `GET /movie/{user}/{password}/{ID}` — the extension is part of the id and is kept.
pub async fn stream_movie(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Rsp {
    let url = format!(
        "{}/movie/{}/{}",
        state.config.xtream_base_url,
        provider_credentials(&state),
        id
    );
    dispatch(&state, &url, &id, &headers).await
}

/// `GET /series/{user}/{password}/{ID}`.
pub async fn stream_series(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Rsp {
    let url = format!(
        "{}/series/{}/{}",
        state.config.xtream_base_url,
        provider_credentials(&state),
        id
    );
    dispatch(&state, &url, &id, &headers).await
}

/// `GET /timeshift/{user}/{password}/{duration}/{start}/{ID}` — always plain streaming, never HLS.
pub async fn stream_timeshift(
    State(state): State<Arc<AppState>>,
    Path((duration, start, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Rsp {
    let url = format!(
        "{}/timeshift/{}/",
        state.config.xtream_base_url,
        provider_credentials(&state)
    ) + &format!("{duration}/{start}/{id}");
    stream(&state, &url, &headers).await
}

/// `GET /play/{token}/{type}`. The token already authorises the stream, so no credentials are
/// added upstream.
pub async fn stream_play(
    State(state): State<Arc<AppState>>,
    Path((token, r#type)): Path<(String, String)>,
    headers: HeaderMap,
) -> Rsp {
    let url = format!("{}/play/{}/{}", state.config.xtream_base_url, token, r#type);
    // Go's xtreamStream keys off the `id` route parameter, which this route does not have, so a
    // play request is always a plain byte pipe.
    stream(&state, &url, &headers).await
}

/// `GET /hls/{token}/{chunk}` — the chunk is `{channel}_{sequence}`, and the real host is the one
/// the provider redirected to when the playlist was first fetched.
pub async fn hls_stream(
    State(state): State<Arc<AppState>>,
    Path((token, chunk)): Path<(String, String)>,
    headers: HeaderMap,
) -> Rsp {
    let channel = match hls_channel(&chunk) {
        Ok(channel) => channel,
        Err(response) => return response,
    };
    let host = match redirect_host(&state, &channel).await {
        Ok(host) => host,
        Err(response) => return response,
    };

    let url = format!("{host}/hls/{token}/{chunk}");
    stream(&state, &url, &headers).await
}

/// `GET /hlsr/{token}/{user}/{password}/{channel}/{hash}/{chunk}`.
pub async fn hlsr_stream(
    State(state): State<Arc<AppState>>,
    Path((token, channel, hash, chunk)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Rsp {
    let host = match redirect_host(&state, &channel).await {
        Ok(host) => host,
        Err(response) => return response,
    };

    let url = format!(
        "{host}/hlsr/{token}/{}/{}/{channel}/{hash}/{chunk}",
        state.config.xtream_user.path_escape(),
        state.config.xtream_password.path_escape(),
    );
    stream(&state, &url, &headers).await
}

/// `.m3u8` ids go through the HLS redirect dance, everything else is a plain byte pipe.
async fn dispatch(state: &Arc<AppState>, url: &str, id: &str, headers: &HeaderMap) -> Rsp {
    if id.ends_with(".m3u8") {
        match hls_playlist(state, url, headers, id).await {
            Ok((body, content_type)) => response_with(content_type.as_deref(), body),
            Err(response) => response,
        }
    } else {
        stream(state, url, headers).await
    }
}

fn hls_channel(chunk: &str) -> Result<String, Rsp> {
    let parts: Vec<&str> = chunk.split('_').collect();
    if parts.len() != 2 {
        tracing::error!("HLS malformed chunk: {chunk}");
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "HSL malformed chunk",
        ));
    }
    Ok(parts[0].to_string())
}

async fn redirect_host(state: &Arc<AppState>, channel: &str) -> Result<String, Rsp> {
    let key = format!("{channel}.m3u8");
    let Some(stored) = state.hls_redirect(&key) else {
        tracing::error!("HLS redirect url not found for {key}");
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "HSL redirect url not found",
        ));
    };

    match crate::url::Url::parse(&stored) {
        Ok(url) => Ok(format!("{}://{}{}", url.scheme, url.host, url.port)),
        Err(err) => {
            tracing::error!("cannot parse stored HLS redirect {stored}: {err}");
            Err(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "HSL malformed chunk",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_parsing() {
        assert_eq!(hls_channel("bbc_12345").unwrap(), "bbc");
        assert!(hls_channel("nounderscore").is_err());
        assert!(hls_channel("too_many_parts").is_err());
    }

    #[test]
    fn params_are_case_sensitive_like_the_provider() {
        let params = vec![
            ("action".to_string(), "get_live_streams".to_string()),
            ("category_id".to_string(), String::new()),
        ];
        assert_eq!(
            param(&params, "action").as_deref(),
            Some("get_live_streams")
        );
        assert_eq!(required(&params, "category_id"), None);
    }

    #[test]
    fn missing_param_reports_the_provider_message() {
        let response = missing_param("series_id");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn limit_is_optional_and_numeric() {
        let params = vec![("limit".to_string(), "20".to_string())];
        assert_eq!(limit(&params), 20);
        assert_eq!(limit(&[]), 0);
        assert_eq!(
            limit(&[("limit".to_string(), "lots".to_string())]),
            0,
            "a non numeric limit falls back to the provider default (0 = do not send the parameter)"
        );
    }
}
