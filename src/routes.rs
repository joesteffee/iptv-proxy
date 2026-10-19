//! Route table. Mirrors `pkg/server/routes.go`, including the exact precedence: the Xtream routes
//! win whenever a provider is configured, and the `/{M3U_FILE_NAME}` route is served by the Xtream
//! passthrough only when the proxy sits in front of the very provider named in `M3U_URL`.

use crate::config::ProxyConfig;
use crate::handlers::{playlist, xtream, Rsp};
use crate::state::AppState;
use crate::url::Url;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, MethodRouter};

/// Every handler takes the shared state by `Arc`.
type S = std::sync::Arc<AppState>;
use axum::Router;
use std::sync::Arc;

/// True when the proxy is fronting the very provider named in `M3U_URL`: credentials match and the
/// provider host appears in the base URL, exactly like the Go condition.
pub fn xtream_auto(config: &ProxyConfig) -> bool {
    let Ok(remote) = Url::parse(&config.remote_url) else {
        return false;
    };

    let user_matches = config.xtream_user.as_str()
        == remote
            .query_pairs()
            .iter()
            .find(|(name, _)| name == "username")
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
    let password_matches = config.xtream_password.as_str()
        == remote
            .query_pairs()
            .iter()
            .find(|(name, _)| name == "password")
            .map(|(_, value)| value.as_str())
            .unwrap_or("");

    config.xtream_base_url.contains(remote.host_only()) && user_matches && password_matches
}

/// Builds the whole application.
///
/// `CUSTOM_ENDPOINT` is baked into every path instead of applied with `Router::nest`, so a proxy on
/// `CUSTOM_ENDPOINT=/tv` serves `/tv`, `/tv/get.php`, `/tv/api/categories` … the way the gin route
/// group did.
pub fn router(state: Arc<AppState>) -> Router {
    let prefix = endpoint_prefix(&state.config);
    let mut app = Router::new();

    for (path, methods) in route_table(&state, &prefix) {
        app = app.route(&path, methods);
    }

    if !prefix.is_empty() {
        // gin's `RedirectTrailingSlash` answered `GET /tv` with a 301 to `/tv/`.
        let redirect_target = format!("{prefix}/");
        app = app.route(
            &prefix,
            get(move || {
                let target = redirect_target.clone();
                async move {
                    axum::response::Response::builder()
                        .status(StatusCode::MOVED_PERMANENTLY)
                        .header(axum::http::header::LOCATION, target)
                        .body(axum::body::Body::empty())
                        .unwrap_or_else(|_| {
                            crate::handlers::error(StatusCode::MOVED_PERMANENTLY, "")
                        })
                }
            }),
        );
    }

    app.fallback(not_found).with_state(state)
}

/// `/tv` for `CUSTOM_ENDPOINT=/tv/`, or `""` when unset.
fn endpoint_prefix(config: &ProxyConfig) -> String {
    let endpoint = config.custom_endpoint.trim_matches('/');
    if endpoint.is_empty() {
        String::new()
    } else {
        format!("/{endpoint}")
    }
}

/// Every route of the current mode, as `(path, method router)` pairs.
fn route_table(state: &Arc<AppState>, prefix: &str) -> Vec<(String, MethodRouter<S>)> {
    let mut table = vec![
        (format!("{prefix}/"), get(crate::handlers::ui::serve_web_ui)),
        (
            format!("{prefix}/api/categories"),
            get(crate::handlers::ui::get_categories).post(crate::handlers::ui::update_categories),
        ),
    ];

    if state.config.xtream_enabled() {
        xtream_routes(state, prefix, &mut table);
    } else {
        m3u_routes(state, prefix, &mut table);
    }

    table
}

/// The Xtream Codes surface: the playlist, the app API, the EPG and one route per stream shape.
fn xtream_routes(state: &Arc<AppState>, prefix: &str, table: &mut Vec<(String, MethodRouter<S>)>) {
    table.push((
        format!("{prefix}/get.php"),
        get(playlist::xtream_api_get).post(playlist::xtream_api_get),
    ));
    table.push((format!("{prefix}/apiget"), get(playlist::xtream_api_get)));
    table.push((
        format!("{prefix}/player_api.php"),
        get(xtream::player_api_get).post(xtream::player_api_post),
    ));
    table.push((format!("{prefix}/xmltv.php"), get(xtream::xmltv)));

    // Route literals carry the *escaped* credentials because that is what appears on the wire; a
    // client sending anything else does not match and gets a 404, as in Go.
    let user = state.config.user.path_escape();
    let password = state.config.password.path_escape();

    table.push((
        format!("{prefix}/live/{user}/{password}/{{id}}"),
        get(xtream::stream_live),
    ));
    table.push((
        format!("{prefix}/timeshift/{user}/{password}/{{duration}}/{{start}}/{{id}}"),
        get(xtream::stream_timeshift),
    ));
    table.push((
        format!("{prefix}/movie/{user}/{password}/{{id}}"),
        get(xtream::stream_movie),
    ));
    table.push((
        format!("{prefix}/series/{user}/{password}/{{id}}"),
        get(xtream::stream_series),
    ));

    if xtream_auto(&state.config) {
        // Go returns right here: the playlist route replaces the generic stream route.
        table.push((
            format!("{prefix}/{}", state.config.m3u_file_name.trim_matches('/')),
            get(playlist::xtream_get_auto).post(playlist::xtream_get_auto),
        ));
        return;
    }

    table.push((
        format!("{prefix}/{user}/{password}/{{id}}"),
        get(xtream::stream_generic),
    ));
    table.push((
        format!("{prefix}/hlsr/{{token}}/{user}/{password}/{{channel}}/{{hash}}/{{chunk}}"),
        get(xtream::hlsr_stream),
    ));
    table.push((
        format!("{prefix}/hls/{{token}}/{{chunk}}"),
        get(xtream::hls_stream),
    ));
    table.push((
        format!("{prefix}/play/{{token}}/{{type}}"),
        get(xtream::stream_play),
    ));
}

/// The plain-M3U surface: the playlist file plus one streaming route for every parsed track.
///
/// Go registered a literal route per track (`/{anti}/{user}/{pass}/{index}/{file}`, or
/// `/{anti}/{user}/{pass}/{index}/{id}` for HLS masters). One parameterised route covers all of them
/// and resolves the track by index, which keeps registration at O(1) instead of O(tracks) — with a
/// 50 000-track playlist that is the difference between a millisecond and a several-second startup.
fn m3u_routes(state: &Arc<AppState>, prefix: &str, table: &mut Vec<(String, MethodRouter<S>)>) {
    let user = state.config.user.path_escape();
    let password = state.config.password.path_escape();

    table.push((
        format!("{prefix}/{}", state.config.m3u_file_name.trim_matches('/')),
        get(playlist::get_m3u).post(playlist::get_m3u),
    ));
    table.push((
        format!(
            "{prefix}/{}/{user}/{password}/{{index}}/{{rest}}",
            state.anti_collision
        ),
        get(track_route),
    ));
}

/// `GET /{anti}/{user}/{pass}/{index}/{rest}` — streams the track the served playlist assigned to
/// `index`, rewriting HLS master playlists so their segments come back through the proxy.
async fn track_route(
    State(state): State<Arc<AppState>>,
    Path((index, rest)): Path<(usize, String)>,
    request: Request,
) -> Rsp {
    let track = {
        let tracks = state.tracks.read().expect("tracks lock");
        tracks.get(index).cloned()
    };

    let Some(track) = track else {
        return not_found().await;
    };

    if track.uri.ends_with(".m3u8") {
        // `m3u8ReverseProxy`: replace the playlist's own file name with whatever the client asked
        // for, so relative segment requests resolve back through here.
        let target = track.uri.replace(&base_name(&track.uri), &rest);
        return crate::handlers::stream::stream(&state, &target, request.headers()).await;
    }

    crate::handlers::stream::stream(&state, &track.uri, request.headers()).await
}

fn base_name(uri: &str) -> String {
    Url::parse(uri)
        .map(|url| url.base().to_string())
        .unwrap_or_else(|_| uri.rsplit('/').next().unwrap_or(uri).to_string())
}

async fn not_found() -> Rsp {
    axum::http::Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(axum::body::Body::from("404 page not found"))
        .unwrap_or_else(|_| crate::handlers::error(StatusCode::NOT_FOUND, "Not Found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn config(remote: &str, base: &str, user: &str, password: &str) -> ProxyConfig {
        let raw = vec![
            "--m3u-url".to_string(),
            remote.to_string(),
            "--hostname".to_string(),
            "proxy.test".to_string(),
            "--xtream-base-url".to_string(),
            base.to_string(),
            "--xtream-user".to_string(),
            user.to_string(),
            "--xtream-password".to_string(),
            password.to_string(),
        ];
        crate::config::build(&raw).expect("config builds")
    }

    #[test]
    fn auto_mode_needs_matching_host_and_credentials() {
        let remote = "http://prov.test:8080/get.php?username=iptv&password=pw&type=m3u_plus";

        assert!(xtream_auto(&config(
            remote,
            "http://prov.test:8080",
            "iptv",
            "pw"
        )));
        assert!(
            !xtream_auto(&config(remote, "http://prov.test:8080", "iptv", "wrong")),
            "a password mismatch must not take the auto path"
        );
        assert!(
            !xtream_auto(&config(remote, "http://other.test:8080", "iptv", "pw")),
            "a host mismatch must not take the auto path"
        );
        assert!(
            !xtream_auto(&config("not a url", "http://prov.test:8080", "iptv", "pw")),
            "an unparseable M3U_URL is simply not auto mode"
        );
    }

    #[tokio::test]
    async fn ui_is_served_at_the_root() {
        let state = crate::state::test_state(config("", "", "u", "p"));
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn custom_endpoint_prefixes_every_route() {
        let raw = vec![
            "--hostname".to_string(),
            "proxy.test".to_string(),
            "--xtream-base-url".to_string(),
            "http://prov.test".to_string(),
            "--custom-endpoint".to_string(),
            "/tv".to_string(),
        ];
        let state = crate::state::test_state(crate::config::build(&raw).unwrap());
        let app = router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tv/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tv/get.php?username=usertest&password=passwordtest")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "the xtream playlist route must live under the custom endpoint"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/get.php")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "routes outside the custom endpoint must disappear"
        );
    }

    #[tokio::test]
    async fn bare_custom_endpoint_redirects_to_the_slash() {
        let raw = vec![
            "--hostname".to_string(),
            "proxy.test".to_string(),
            "--custom-endpoint".to_string(),
            "tv".to_string(),
        ];
        let app = router(crate::state::test_state(
            crate::config::build(&raw).unwrap(),
        ));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/tv")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            response.headers().get("location").unwrap(),
            "/tv/",
            "matching gin's RedirectTrailingSlash"
        );
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        let app = router(crate::state::test_state(config("", "", "u", "p")));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
