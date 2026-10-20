//! A stand-in Xtream Codes provider plus helpers to drive the proxy in-process.
//!
//! Every response here mirrors the shape a real provider sends, so the contract tests exercise the
//! same code paths as production without touching the network.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::header::{
    HeaderMap, HeaderName, HeaderValue, CONTENT_DISPOSITION, CONTENT_TYPE, LOCATION,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get as get_route};
use axum::Router;
use tower::ServiceExt;

use iptv_proxy::url::parse_query;

pub const PROVIDER_USER: &str = "provuser";
pub const PROVIDER_PASSWORD: &str = "provpass";

/// Proxy side credentials used by the harness.
pub const PROXY_USER: &str = "iptv";
pub const PROXY_PASSWORD: &str = "secret";

#[derive(Default)]
struct Mock {
    base: String,
    hits: Mutex<HashMap<String, usize>>,
    last_get_php_query: Mutex<String>,
    last_api_query: Mutex<String>,
    /// How many further requests to `/ratelimit` are answered with status 458.
    rate_limited_left: AtomicUsize,
    /// Requests currently inside `api`. `max_in_flight` of 1 reproduces a provider whose account
    /// is limited to a single concurrent connection, which is the common `max_connections: 1` case.
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// When non-zero, the catalogue actions answer with this many categories per type instead of
    /// the single fixture category, so a caller has enough work to keep requests overlapping.
    wide: AtomicUsize,
}

impl Mock {
    fn record(&self, key: &str) {
        *self
            .hits
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_insert(0) += 1;
    }

    fn hit(&self, key: &str) -> usize {
        *self.hits.lock().unwrap().get(key).unwrap_or(&0)
    }

    fn remember(&self, slot: &Mutex<String>, value: &str) {
        *slot.lock().unwrap() = value.to_string();
    }
}

/// The mock provider, listening on an ephemeral loopback port.
pub struct MockProvider {
    pub base: String,
    mock: Arc<Mock>,
}

impl MockProvider {
    pub async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let mock = Arc::new(Mock {
            base: format!("http://{addr}"),
            ..Mock::default()
        });

        let app = Router::new()
            .route("/player_api.php", any(api))
            .route("/get.php", get_route(upstream))
            .route("/xmltv.php", get_route(upstream))
            .route("/plain.m3u", get_route(upstream))
            .route("/live/{user}/{pass}/{id}", get_route(upstream))
            .route("/movie/{user}/{pass}/{id}", get_route(upstream))
            .route("/series/{user}/{pass}/{id}", get_route(upstream))
            .route("/timeshift/{user}/{pass}/{d}/{s}/{id}", get_route(upstream))
            .route("/play/{token}/{kind}", get_route(upstream))
            .route("/hls/{token}/{chunk}", get_route(upstream))
            .route(
                "/hlsr/{token}/{user}/{pass}/{channel}/{hash}/{chunk}",
                get_route(upstream),
            )
            // The generic xtream shape: `{base}/{user}/{pass}/{id}`.
            .route("/{user}/{pass}/{id}", get_route(upstream))
            .route("/hls-target/{id}", get_route(upstream))
            .route("/redirect/{id}", get_route(upstream))
            .route("/loop/{count}", get_route(upstream))
            .route("/ratelimit", get_route(upstream))
            .with_state(mock.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Arc::new(Self {
            base: mock.base.clone(),
            mock,
        })
    }

    /// How many requests the mock served for `key` (a path, or `player_api.php?action=get_...`).
    pub fn hits(&self, key: &str) -> usize {
        self.mock.hit(key)
    }

    /// The query string of the last `/get.php` request, sorted so assertions are stable.
    pub fn last_get_php_query(&self) -> String {
        self.mock.last_get_php_query.lock().unwrap().clone()
    }

    /// The query string of the last `player_api.php` request, sorted.
    pub fn last_api_query(&self) -> String {
        self.mock.last_api_query.lock().unwrap().clone()
    }

    /// Answers the next `times` requests to `/ratelimit` with status 458.
    pub fn set_rate_limited(&self, times: usize) {
        self.mock.rate_limited_left.store(times, Ordering::SeqCst);
    }

    /// Answers the three `get_*_categories` actions with `count` categories (ids `1..=count`,
    /// `100..`, `200..`) and each list action with one item per requested category.
    pub fn set_wide_catalogue(&self, count: usize) {
        self.mock.wide.store(count, Ordering::SeqCst);
    }

    /// Rejects any `player_api.php` request that arrives while `limit` others are still being
    /// served, the way a connection-capped account does. `0` (the default) accepts everything.
    pub fn set_max_concurrent(&self, limit: usize) {
        self.mock.max_in_flight.store(limit, Ordering::SeqCst);
    }
}

/// Holds one slot of the mock's connection budget for as long as a request is being served.
struct Slot<'a> {
    mock: &'a Mock,
    over: bool,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.mock.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The multi-category fixtures used by [`MockProvider::set_wide_catalogue`].
fn wide_body(action: &str, query: &HashMap<String, String>, count: usize) -> serde_json::Value {
    let ids = |offset: i64| -> Vec<serde_json::Value> {
        (0..count as i64)
            .map(|i| {
                serde_json::json!({
                    "category_id": offset + i,
                    "category_name": format!("Pack {}", offset + i),
                    "parent_id": 0
                })
            })
            .collect()
    };
    let category = query
        .get("category_id")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    match action {
        "get_live_categories" => serde_json::Value::Array(ids(1)),
        "get_vod_categories" => serde_json::Value::Array(ids(100)),
        "get_series_categories" => serde_json::Value::Array(ids(200)),
        "get_live_streams" => serde_json::json!([{
            "stream_id": 1000 + category, "name": format!("Channel {category}"),
            "stream_type": "live", "category_id": category, "container_extension": "",
            "custom_sid": "", "epg_channel_id": "", "stream_icon": "", "num": 1, "added": null,
            "rating": 0, "rating_5based": 0, "tv_archive": 0, "tv_archive_duration": 0
        }]),
        "get_vod_streams" => serde_json::json!([{
            "stream_id": 2000 + category, "name": format!("Film {category}"),
            "stream_type": "movie", "category_id": category, "container_extension": "mp4",
            "custom_sid": "", "epg_channel_id": null, "stream_icon": "", "num": 1, "added": null,
            "rating": 0, "rating_5based": 0, "tv_archive": 0, "tv_archive_duration": 0,
            "direct_source": ""
        }]),
        "get_series" => serde_json::json!([{
            "series_id": 3000 + category, "name": format!("Show {category}"),
            "category_id": category, "cover": "", "plot": "", "cast": "", "genre": "",
            "release_date": "", "last_episode_added": "", "backdrop_path": [],
            "rating": 0, "rating_5based": 0, "youtube_trailer": ""
        }]),
        "get_series_info" => serde_json::json!({
            "episodes": { "1": [{ "id": format!("{}", 4000 + category), "episode_num": "1",
                "season": "1", "title": "Premiere", "container_extension": "mkv",
                "info": {}, "custom_sid": "", "direct_source": "" }] },
            "info": { "name": format!("Show {category}"), "cover": "", "series_id": 3000 + category,
                "category_id": category }
        }),
        _ => serde_json::json!([]),
    }
}

fn sorted(query: &str) -> String {
    let mut pairs = parse_query(query);
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------- provider API

async fn api(
    State(mock): State<Arc<Mock>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let action = query.get("action").map(String::as_str).unwrap_or("login");
    let active = mock.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    let limit = mock.max_in_flight.load(Ordering::SeqCst);
    let slot = Slot {
        mock: &mock,
        over: limit > 0 && active > limit,
    };
    if slot.over {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if limit > 0 {
        // Hold the connection long enough that a concurrent caller genuinely overlaps.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    mock.record(&format!("player_api.php?action={action}"));
    mock.remember(
        &mock.last_api_query,
        &sorted(
            &query
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&"),
        ),
    );

    // A real provider rejects bad credentials with auth:0.
    if query.get("username").map(String::as_str) != Some(PROVIDER_USER)
        || query.get("password").map(String::as_str) != Some(PROVIDER_PASSWORD)
    {
        return json(serde_json::json!({
            "user_info": {"username": "", "password": "", "auth": 0, "status": "Disabled"},
            "server_info": {"url": "", "port": "0"}
        }));
    }

    let wide = mock.wide.load(Ordering::SeqCst);
    let body = if wide > 0 {
        wide_body(action, &query, wide)
    } else {
        match action {
            "get_live_categories" => serde_json::json!([
                {"category_id": "1", "category_name": "Live Pack", "parent_id": 0}
            ]),
            "get_vod_categories" => serde_json::json!([
                {"category_id": "10", "category_name": "Movie Pack", "parent_id": 0}
            ]),
            "get_series_categories" => serde_json::json!([
                {"category_id": "20", "category_name": "Show Pack", "parent_id": 0}
            ]),
            "get_live_streams" => serde_json::json!([
                {
                    "num": "1", "name": "Alpha TV", "stream_type": "live", "stream_id": 101,
                    "stream_icon": "http://img/alpha.png", "epg_channel_id": "alpha.epg",
                    "category_id": "1", "category_uuid": "u1", "custom_sid": "",
                    "tv_archive": 0, "direct_source": "", "tv_archive_duration": 0,
                    "rating": "0.00", "rating_5based": 0, "added": "1700000000",
                    "container_extension": ""
                },
                {
                    "num": "2", "name": "Beta TV", "stream_type": "live", "stream_id": 102,
                    "stream_icon": "http://img/beta.png", "epg_channel_id": "beta.epg",
                    "category_id": "1", "category_uuid": "u1", "custom_sid": "",
                    "tv_archive": 0, "direct_source": "", "tv_archive_duration": 0,
                    "rating": "0.00", "rating_5based": 0, "added": null,
                    "container_extension": ""
                }
            ]),
            "get_vod_streams" => serde_json::json!([
                {
                    "num": "1", "name": "Big Movie", "stream_type": "movie", "stream_id": 201,
                    "stream_icon": "http://img/movie.jpg", "epg_channel_id": null,
                    "category_id": "10", "custom_sid": "", "container_extension": "mp4",
                    "rating": "5.25", "rating_5based": 4.2, "added": "1700000000",
                    "tv_archive": 0, "tv_archive_duration": 0, "direct_source": ""
                }
            ]),
            "get_series" => serde_json::json!([
                {
                    "series_id": "301", "name": "Great Show", "cover": "http://img/cover.jpg",
                    "plot": "A show.", "cast": "", "genre": "Drama", "release_date": "2020-01-01",
                    "last_episode_added": "1700000000", "backdrop_path": ["http://img/bd.jpg"],
                    "rating": "8.5", "rating_5based": 4.25, "category_id": "20", "youtube_trailer": ""
                }
            ]),
            "get_series_info" => serde_json::json!({
                "episodes": {
                    "1": [
                        {
                            "id": "401", "episode_num": "1", "season": "1", "title": "Pilot",
                            "container_extension": "mkv", "info": {"movie_image": "http://img/e1.jpg"},
                            "custom_sid": "", "direct_source": ""
                        },
                        {
                            "id": "402", "episode_num": "2", "season": "1", "title": "",
                            "container_extension": "mkv", "info": {"movie_image": "http://img/e2.jpg"},
                            "custom_sid": "", "direct_source": ""
                        }
                    ]
                },
                "info": {
                    "name": "Great Show", "cover": "http://img/cover.jpg", "plot": "A show.",
                    "cast": "", "genre": "Drama", "release_date": "2020-01-01",
                    "backdrop_path": "http://img/bd.jpg", "rating": "8.5", "rating_5based": 4.25,
                    "youtube_trailer": "", "series_id": "301", "category_id": "20"
                }
            }),
            "get_vod_info" => serde_json::json!({
                "info": {"movie_image": "http://img/movie.jpg", "name": "Big Movie", "rating": "5.25"},
                "movie_data": {"stream_id": 201, "name": "Big Movie", "container_extension": "mp4"}
            }),
            "get_short_epg" | "get_simple_data_table" => serde_json::json!({
                "epg_listings": [
                    {"id": "1", "title": "Now", "description": "Right now",
                     "start": "20260101120000", "stop": "20260101130000",
                     "channel_id": "alpha.epg", "start_timestamp": 1767268800,
                     "stop_timestamp": 1767272400}
                ]
            }),
            _ => serde_json::json!({
                "user_info": {
                    "username": PROVIDER_USER, "password": PROVIDER_PASSWORD,
                    "message": "Welcome", "auth": 1, "status": "Active",
                    "exp_date": null, "is_trial": "0", "active_cons": "1",
                    "created_at": "1700000000", "max_connections": "2",
                    "allowed_output_formats": ["m3u_plus", "ts", "json"]
                },
                "server_info": {
                    "url": "provider.example", "port": "25461", "https_port": "443",
                    "server_protocol": "http", "rtmp_port": "9981", "timezone": "UTC",
                    "time_now": "2026-01-01 12:00:00", "timestamp_now": 1767268800,
                    "process": false
                }
            }),
        }
    };

    json(body)
}

fn json(body: serde_json::Value) -> Response {
    with_headers(
        &[(CONTENT_TYPE.as_str(), "application/json")],
        body.to_string(),
    )
}

fn with_headers(headers: &[(&str, &str)], body: String) -> Response {
    let mut response = Response::builder();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("valid header name");
        response = response.header(
            name,
            HeaderValue::from_str(value).expect("valid header value"),
        );
    }
    response.body(Body::from(body)).unwrap()
}

fn redirect(target: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, target)
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------- provider content

/// Answers every non-API provider route by looking at the requested path.
async fn upstream(State(mock): State<Arc<Mock>>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    mock.record(&path);

    if path == "/get.php" {
        mock.remember(&mock.last_get_php_query, &sorted(&query));
        let body = format!(
            "#EXTM3U\n\
             #EXTINF:-1 tvg-id=\"raw.one\" tvg-name=\"Raw One\" group-title=\"Raw\", Raw One\n\
             {base}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/900.ts\n\
             #EXTINF:-1 tvg-name=\"Raw HLS\" group-title=\"Raw\", Raw HLS\n\
             {base}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/alpha.m3u8\n",
            base = mock.base
        );
        return with_headers(
            &[
                (
                    CONTENT_DISPOSITION.as_str(),
                    "attachment; filename=\"provider.m3u\"",
                ),
                (CONTENT_TYPE.as_str(), "application/octet-stream"),
            ],
            body,
        );
    }

    if path == "/plain.m3u" {
        // The track index is part of the proxy URL, so this order is a contract:
        // 0 media, 1 HLS playlist, 2 redirect loop, 3 rate limited, 4 single redirect.
        let body = format!(
            "#EXTM3U\n\
             #EXTINF:-1 tvg-name=\"Plain One\" group-title=\"Plain\", Plain One\n\
             {base}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/900.ts\n\
             #EXTINF:-1 tvg-name=\"Plain HLS\" group-title=\"Plain\", Plain HLS\n\
             {base}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/alpha.m3u8\n\
             #EXTINF:-1 tvg-name=\"Loop\" group-title=\"Plain\", Loop\n\
             {base}/loop/12\n\
             #EXTINF:-1 tvg-name=\"Limited\" group-title=\"Plain\", Limited\n\
             {base}/ratelimit\n\
             #EXTINF:-1 tvg-name=\"Redirected\" group-title=\"Plain\", Redirected\n\
             {base}/redirect/555\n",
            base = mock.base
        );
        return with_headers(&[(CONTENT_TYPE.as_str(), "application/octet-stream")], body);
    }

    if path == "/xmltv.php" {
        return with_headers(
            &[(CONTENT_TYPE.as_str(), "application/xml")],
            "<?xml version=\"1.0\"?>\n<tv></tv>\n".to_string(),
        );
    }

    if path == "/ratelimit" {
        // `try_update` is newer than the crate MSRV.
        #[allow(deprecated)]
        let previous = mock
            .rate_limited_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                Some(left.saturating_sub(1))
            })
            .unwrap_or(0);
        if previous > 0 {
            return Response::builder()
                .status(StatusCode::from_u16(458).expect("458 is a status code"))
                .body(Body::empty())
                .unwrap();
        }
        return with_headers(
            &[(CONTENT_TYPE.as_str(), "video/mp2t")],
            "AFTER-LIMIT".to_string(),
        );
    }

    if let Some(id) = path.strip_prefix("/loop/") {
        let left: usize = id.parse().unwrap_or(0);
        let target = if left <= 1 {
            format!("/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/done")
        } else {
            format!("/loop/{}", left - 1)
        };
        return redirect(&target);
    }

    if let Some(id) = path.strip_prefix("/redirect/") {
        return redirect(&format!(
            "{}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/{id}",
            mock.base
        ));
    }

    // The signed HLS target still carries the provider credentials, exactly like a real provider's,
    // plus an absolute segment URL on the provider's own host.
    if let Some(id) = path.strip_prefix("/hls-target/") {
        let body = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.000,\n\
             {base}/live/{PROVIDER_USER}/{PROVIDER_PASSWORD}/{id}.seg1.ts\n\
             {base}/hls/tok1/{stem}_seg1.ts\n\
             #EXT-X-ENDLIST\n",
            base = mock.base,
            id = id,
            stem = stem(id),
        );
        return with_headers(
            &[(CONTENT_TYPE.as_str(), "application/vnd.apple.mpegurl")],
            body,
        );
    }

    let last = path.rsplit('/').next().unwrap_or_default().to_string();
    if last.ends_with(".m3u8") {
        // A master playlist request is answered with a signed redirect, like providers that hand
        // out time-limited HLS targets.
        return redirect(&format!("{}/hls-target/{last}", mock.base));
    }

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
    headers.insert("x-upstream", HeaderValue::from_static("yes"));
    headers.insert(
        "x-requested-path",
        HeaderValue::from_str(&path).unwrap_or_else(|_| HeaderValue::from_static("invalid")),
    );
    (headers, format!("MEDIA {path}")).into_response()
}

/// `alpha.m3u8` -> `alpha`.
fn stem(name: &str) -> String {
    name.trim_end_matches(".m3u8").to_string()
}

// ---------------------------------------------------------------- proxy harness

/// Builds a proxy configuration from flags; flags always win over the environment.
/// A writable path for test-owned state files.
pub fn temp_path(name: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "iptv-proxy-tests-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name).to_string_lossy().into_owned()
}

pub fn config(provider_base: &str, extra: &[&str]) -> iptv_proxy::config::ProxyConfig {
    let mut args: Vec<String> = vec![
        "--hostname".to_string(),
        "proxy.test".to_string(),
        "--port".to_string(),
        "8080".to_string(),
        "--user".to_string(),
        PROXY_USER.to_string(),
        "--password".to_string(),
        PROXY_PASSWORD.to_string(),
    ];
    // The image default (/root/iptv/category_filters.json) is not writable in a test sandbox, so
    // every harness config persists filters to a private temp file instead.
    if !args.iter().any(|arg| arg == "--category-filters-path") {
        args.push("--category-filters-path".to_string());
        args.push(temp_path("category_filters.json"));
    }
    args.extend(extra.iter().map(|arg| arg.to_string()));
    if !provider_base.is_empty() && !args.iter().any(|arg| arg == "--m3u-url") {
        args.push("--m3u-url".to_string());
        args.push(format!("{provider_base}/get.php"));
    }
    iptv_proxy::config::build(&args).expect("valid configuration")
}

/// A proxy state with its playlist files in a private temp directory.
pub async fn state(cfg: iptv_proxy::config::ProxyConfig) -> Arc<iptv_proxy::state::AppState> {
    let dir = tempfile::tempdir().expect("temp dir");
    Arc::new(
        iptv_proxy::state::AppState::startup(cfg)
            .await
            .expect("proxy startup")
            .with_cache_dir(dir.keep()),
    )
}

pub fn app(state: Arc<iptv_proxy::state::AppState>) -> Router {
    iptv_proxy::routes::router(state)
}

/// An xtream-mode proxy with every category of the mock catalogue enabled.
pub async fn xtream_app(provider: &Arc<MockProvider>) -> Router {
    let (app, _) = xtream_state_app(provider).await;
    enable_all(&app).await;
    app
}

/// The same as [`xtream_app`], but also returns the `AppState` so a test can inspect the cache.
pub async fn xtream_state_app(
    provider: &Arc<MockProvider>,
) -> (Router, Arc<iptv_proxy::state::AppState>) {
    let cfg = config(
        &provider.base,
        &[
            "--xtream-base-url",
            &provider.base,
            "--xtream-user",
            PROVIDER_USER,
            "--xtream-password",
            PROVIDER_PASSWORD,
        ],
    );
    let state = state(cfg).await;
    let app = app(Arc::clone(&state));
    (app, state)
}

/// A plain (non-xtream) proxy serving the provider's static playlist, with tracks loaded.
pub async fn plain_app(provider: &Arc<MockProvider>) -> Router {
    let cfg = config(
        &provider.base,
        &[
            "--m3u-url",
            &format!("{}/plain.m3u", provider.base),
            "--custom-id",
            "anti",
        ],
    );
    let state = state(cfg).await;
    iptv_proxy::handlers::playlist::bootstrap(&state)
        .await
        .expect("bootstrap loads the remote playlist");
    app(state)
}

/// Enables every category of the mock catalogue on the proxy.
/// POSTs a JSON body (the `/api/categories` update takes JSON, not a form).
pub async fn post_json(app: &Router, uri: &str, body: &str) -> (u16, HeaderMap, String) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, request).await
}

pub async fn enable_all(app: &Router) {
    let body = serde_json::json!({
        "enabled": {"live": {"1": true}, "movies": {"10": true}, "series": {"20": true}}
    });
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/categories")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 200, "category update must work");
}

pub const AUTH: &str = "username=iptv&password=secret";

/// GETs `uri`; returns `(status, headers, body)`.
pub async fn fetch(app: &Router, uri: &str) -> (u16, HeaderMap, String) {
    let request = axum::http::Request::builder()
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

/// GETs `uri` with one extra header.
pub async fn fetch_with(
    app: &Router,
    uri: &str,
    name: &str,
    value: &str,
) -> (u16, HeaderMap, String) {
    let request = axum::http::Request::builder()
        .uri(uri)
        .header(name, value)
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

/// POSTs an urlencoded form.
pub async fn post_form(app: &Router, uri: &str, form: &str) -> (u16, HeaderMap, String) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap();
    send(app, request).await
}

async fn send(app: &Router, request: axum::http::Request<Body>) -> (u16, HeaderMap, String) {
    let response: Response = app.clone().oneshot(request).await.expect("service call");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .expect("read body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// The header value for `name`, if present.
pub fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}
