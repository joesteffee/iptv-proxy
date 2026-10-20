//! Xtream endpoints: the `player_api.php` façade, the EPG, and the media passthrough routes.

use crate::handlers::stream::{hls_playlist, response_with, stream};
use crate::handlers::{authenticated, error, Rsp};
use crate::state::AppState;
use crate::url::parse_query;
use crate::xtream::models::{FlexInt, ServerInfo, UserInfo};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response as AxumResponse;
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
        // The Go release failed to build the provider client here and aborted with a bare 500.
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Xtream backend is not configured",
        );
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
        // `xtreamapi.New` failed in the Go version: a bare 500.
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Xtream backend is not configured",
        );
    };

    // Only `series_id`, `vod_id` and `stream_id` are validated by the Go version. The three stream
    // list actions take `category_id` optionally and answer with every item when it is absent, so
    // they must not be gated on it.
    let value: Result<serde_json::Value, anyhow::Error> = match param(params, "action").as_deref() {
        Some("get_live_categories") => map_err(client.categories("live").await),
        Some("get_vod_categories") => map_err(client.categories("vod").await),
        Some("get_series_categories") => map_err(client.categories("series").await),
        Some("get_live_streams") => map_err(
            client
                .streams("live", &optional(params, "category_id"))
                .await,
        ),
        Some("get_vod_streams") => map_err(
            client
                .streams("vod", &optional(params, "category_id"))
                .await,
        ),
        Some("get_series") => map_err(client.series(&optional(params, "category_id")).await),
        Some("get_series_info") => match required(params, "series_id") {
            Some(id) => crate::generate::series_info(state, client, &id)
                .await
                .and_then(|value| Ok(serde_json::to_value(value)?)),
            None => return missing_param("series_id"),
        },
        Some("get_vod_info") => match required(params, "vod_id") {
            Some(id) => map_err(client.vod_info(&id).await),
            None => return missing_param("vod_id"),
        },
        Some("get_short_epg") => match (required(params, "stream_id"), epg_limit(params)) {
            (Some(id), Some(limit)) => map_err(client.short_epg(&id, limit).await),
            (None, _) => return missing_param("stream_id"),
            // `strconv.Atoi` on anything unparsable was a 500 in the Go version.
            (_, None) => return error(StatusCode::INTERNAL_SERVER_ERROR, "limit is not a number"),
        },
        Some("get_simple_data_table") => match required(params, "stream_id") {
            Some(id) => map_err(client.epg(&id).await),
            None => return missing_param("stream_id"),
        },
        // No action (or an unknown one) is a login: the app expects credentials back.
        _ => return login(state).await,
    };

    match value {
        Ok(value) => json_response(&value),
        Err(err) => action_failure(param(params, "action").as_deref().unwrap_or("login"), &err),
    }
}

/// Keep every provider call in one error type so a single arm can answer the Go release's shape.
fn map_err<T: serde::Serialize>(
    result: Result<T, crate::xtream::XtreamError>,
) -> Result<serde_json::Value, anyhow::Error> {
    Ok(serde_json::to_value(result?)?)
}

/// `category_id` and friends for the actions that treat them as optional: missing or empty both
/// mean "not given", exactly like `len(q["category_id"]) > 0`.
fn optional(params: &[(String, String)], name: &str) -> String {
    param(params, name).unwrap_or_default()
}

/// `Some(limit)` to send (0 means "no limit param"), `None` when the caller sent something that is
/// not an integer.
fn epg_limit(params: &[(String, String)]) -> Option<i64> {
    match param(params, "limit") {
        Some(value) => value.parse::<i64>().ok(),
        None => Some(0),
    }
}

/// Answer `200 application/json; charset=utf-8` with anything serialisable, as `gin.Context.JSON` did.
fn json_response<T: serde::Serialize>(value: &T) -> Rsp {
    AxumResponse::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_vec(value).unwrap_or_else(
            |err| format!(r#"{{"error":"{err}"}}"#).into_bytes(),
        )))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"))
}

/// The rewritten login response: the app stores these credentials and uses them for every later
/// request, so they must point back here — proxy credentials, proxy host, proxy port.
/// The login answer. A struct rather than a `serde_json::Value` so the two objects leave in the
/// order Go declared them (`user_info` first); a `Value` would sort them alphabetically.
#[derive(serde::Serialize)]
struct LoginResponse {
    user_info: UserInfo,
    server_info: ServerInfo,
}

async fn login(state: &Arc<AppState>) -> Rsp {
    let config = &state.config;
    let advertised = FlexInt(config.advertised_port as i64);
    let protocol = if config.https { "https" } else { "http" };

    let auth = match state.authenticate().await {
        Ok(auth) => auth,
        // The Go release logged in while *building* the provider client, so a provider that would
        // not answer failed here with a bare 500 rather than inventing a payload.
        Err(err) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("provider login failed: {err}"),
            )
        }
    };

    let (mut user, mut server) = (auth.user_info, auth.server_info);
    user.username = config.user.as_str().to_string();
    user.password = config.password.as_str().to_string();
    server.url = config.advertised_url();
    server.port = advertised;
    server.https_port = advertised;
    server.rtmp_port = advertised;
    server.protocol = protocol.to_string();
    let (user_info, server_info) = (user, server);

    json_response(&LoginResponse {
        user_info,
        server_info,
    })
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

/// `validateParams` failed: `AbortWithError(400, err)` — the status alone.
fn missing_param(name: &str) -> Rsp {
    error(StatusCode::BAD_REQUEST, format!("missing \"{name}\""))
}

/// A failed provider call. The Go release left the status variable unset on every `Action` error,
/// so gin answered **200 with an empty body**; apps that polled a degraded provider saw exactly
/// that, so the shape is kept even though the message is more honest.
fn action_failure(action: &str, err: &anyhow::Error) -> Rsp {
    tracing::warn!("player_api {action} failed: {err}");
    AxumResponse::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .unwrap_or_else(|_| AxumResponse::new(Body::empty()))
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
        assert_eq!(
            optional(&params, "category_id"),
            "",
            "an absent category is not an error, it means every item"
        );
    }

    #[tokio::test]
    async fn missing_param_is_a_bare_400() {
        let response = missing_param("series_id");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get("content-type").is_none());
        assert_eq!(
            body_of(response).await.len(),
            0,
            "gin's AbortWithError sent the status and nothing else"
        );
    }

    #[tokio::test]
    async fn a_failure_answer_is_empty_and_carries_no_content_type() {
        let response = error(StatusCode::NOT_FOUND, "cannot reach the provider");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response.headers().get("content-type").is_none(),
            "an error answer must not advertise a body"
        );
        assert_eq!(body_of(response).await.len(), 0);
    }

    #[tokio::test]
    async fn a_provider_failure_answers_the_go_release_empty_200() {
        // The Go version left the status unset for every failed Action, so a broken provider
        // produced `200` with no body at all. Unpretty, but clients are built against it.
        let raw = vec![
            "--hostname".to_string(),
            "proxy.test".to_string(),
            "--xtream-base-url".to_string(),
            "http://provider.invalid:1".to_string(),
            "--xtream-user".to_string(),
            "pu".to_string(),
            "--xtream-password".to_string(),
            "pp".to_string(),
        ];
        let config = crate::config::build(&raw).expect("config builds");
        let state = std::sync::Arc::new(
            crate::state::AppState::startup(config)
                .await
                .expect("state builds"),
        );
        let response = player_api(
            &state,
            &[("action".to_string(), "get_live_streams".to_string())],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await.len(), 0);
    }

    #[tokio::test]
    async fn json_answers_say_so_the_way_gin_did() {
        let response = json_response(&serde_json::json!({ "ok": true }));
        assert_eq!(
            response.headers()["content-type"],
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn limit_is_optional_and_a_broken_one_is_an_error() {
        let params = vec![("limit".to_string(), "20".to_string())];
        assert_eq!(epg_limit(&params), Some(20));
        assert_eq!(epg_limit(&[]), Some(0), "absent means do not send it");
        assert_eq!(
            epg_limit(&[("limit".to_string(), "lots".to_string())]),
            None,
            "`strconv.Atoi` failed in the Go version, which was a bare 500"
        );
    }

    async fn body_of(response: AxumResponse) -> axum::body::Bytes {
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body reads")
    }
}
