//! Contract tests for Xtream mode: playlist generation, the player API, EPG and streaming.

mod support;

use support::{
    app, config, enable_all, fetch, header_value, post_form, post_json, state, xtream_app, AUTH,
    PROVIDER_PASSWORD, PROVIDER_USER,
};

/// The playlist the mock catalogue must produce once every category is enabled.
const EXPECTED: &str = concat!(
    "#EXTM3U\n",
    "#EXTINF:-1 tvg-id=\"alpha.epg\" tvg-name=\"Alpha TV\" tvg-logo=\"http://img/alpha.png\" group-title=\"Live Pack\", Alpha TV\n",
    "http://proxy.test:8080/live/iptv/secret/101\n",
    "#EXTINF:-1 tvg-id=\"beta.epg\" tvg-name=\"Beta TV\" tvg-logo=\"http://img/beta.png\" group-title=\"Live Pack\", Beta TV\n",
    "http://proxy.test:8080/live/iptv/secret/102\n",
    "#EXTINF:-1 tvg-name=\"Big Movie\" tvg-logo=\"http://img/movie.jpg\" group-title=\"Movie Pack\", Big Movie\n",
    "http://proxy.test:8080/movie/iptv/secret/201.mp4\n",
    "#EXTINF:-1 tvg-logo=\"http://img/cover.jpg\" tvg-name=\"Great Show - S01E01 - Pilot\" group-title=\"Show Pack\", Great Show - S01E01 - Pilot\n",
    "http://proxy.test:8080/series/iptv/secret/401.mkv\n",
    "#EXTINF:-1 tvg-logo=\"http://img/cover.jpg\" tvg-name=\"Great Show - S01E02\" group-title=\"Show Pack\", Great Show - S01E02\n",
    "http://proxy.test:8080/series/iptv/secret/402.mkv\n",
);

#[tokio::test]
async fn get_php_builds_the_playlist_from_the_provider_api() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, headers, body) =
        fetch(&app, &format!("/get.php?{AUTH}&type=m3u_plus&output=ts")).await;

    assert_eq!(status, 200);
    assert_eq!(body, EXPECTED);
    assert_eq!(
        header_value(&headers, "content-disposition").as_deref(),
        Some("attachment; filename=\"iptv.m3u\"")
    );
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("application/octet-stream")
    );
}

/// A provider whose account allows one connection at a time answers `500` to everything that
/// arrives while another request is in flight — the real service this proxy exists for reports
/// `max_connections: 1`. The Go version fetched sequentially so it never noticed; the concurrent
/// build must fall back to one request at a time instead of losing the playlist.
#[tokio::test]
async fn a_provider_with_one_allowed_connection_still_gets_a_full_playlist() {
    let provider = support::MockProvider::start().await;
    provider.set_max_concurrent(1);
    let app = xtream_app(&provider).await;

    let (status, _, body) = fetch(&app, &format!("/get.php?{AUTH}&type=m3u_plus&output=ts")).await;

    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body, EXPECTED);
}

#[tokio::test]
async fn a_generated_playlist_is_not_built_twice() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let uri = &format!("/get.php?{AUTH}&type=m3u_plus&output=ts");
    let (first, _, first_body) = fetch(&app, uri).await;
    let streams_after_first = provider.hits("player_api.php?action=get_live_streams");

    let (second, _, second_body) = fetch(&app, uri).await;

    assert_eq!((first, second), (200, 200));
    assert_eq!(first_body, second_body);
    assert_eq!(
        streams_after_first, 1,
        "the catalogue must be fetched once, then cached"
    );
    assert_eq!(
        provider.hits("player_api.php?action=get_live_streams"),
        streams_after_first
    );
}

#[tokio::test]
async fn nothing_is_generated_until_a_category_is_enabled() {
    let provider = support::MockProvider::start().await;
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
    let app = app(state(cfg).await);

    let (status, _, body) = fetch(&app, &format!("/get.php?{AUTH}&output=ts")).await;

    assert_eq!(status, 200);
    assert_eq!(body, "#EXTM3U\n", "categories are disabled by default");

    // Enabling one live category is enough to make it appear.
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/categories")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({"enabled": {"live": {"1": true}}}).to_string(),
        ))
        .unwrap();
    let response = tower::ServiceExt::oneshot(app.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let (status, _, body) = fetch(&app, &format!("/get.php?{AUTH}&output=ts")).await;
    assert_eq!(status, 200);
    assert!(body.contains("Alpha TV"), "{body}");
    assert!(!body.contains("Big Movie"), "movies stay disabled: {body}");
}

#[tokio::test]
async fn the_category_endpoint_lists_the_provider_catalogue_with_flags() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, _, body) = fetch(&app, "/api/categories").await;
    assert_eq!(status, 200);

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json categories");
    assert_eq!(parsed["live"][0]["id"], "1");
    assert_eq!(parsed["live"][0]["name"], "Live Pack");
    assert_eq!(parsed["live"][0]["type"], "live");
    assert_eq!(parsed["live"][0]["enabled"], true);
    assert_eq!(parsed["movies"][0]["name"], "Movie Pack");
    assert_eq!(parsed["series"][0]["name"], "Show Pack");
}

#[tokio::test]
async fn live_movie_and_series_routes_use_the_provider_credentials() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    // A live request goes to `{base}/{user}/{pass}/{id}` — no `/live/` prefix — and the `.ts`
    // suffix a player appends is trimmed first.
    let (status, headers, body) = fetch(&app, "/live/iptv/secret/101.ts").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /provuser/provpass/101");
    assert_eq!(
        header_value(&headers, "x-requested-path").as_deref(),
        Some("/provuser/provpass/101")
    );

    let (status, _, body) = fetch(&app, "/movie/iptv/secret/201.mp4").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /movie/provuser/provpass/201.mp4");

    let (status, _, body) = fetch(&app, "/series/iptv/secret/401.mkv").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /series/provuser/provpass/401.mkv");

    // The generic route re-uses the provider credentials, not the ones in the path.
    let (status, _, body) = fetch(&app, "/iptv/secret/777").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /provuser/provpass/777");

    // Credentials the proxy was not configured with do not match any route.
    let (status, _, _) = fetch(&app, "/someone/else/777").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn timeshift_and_play_routes_are_proxied() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, _, body) = fetch(&app, "/timeshift/iptv/secret/3600/1700000000/101").await;
    assert_eq!(status, 200);
    assert_eq!(
        body, "MEDIA /timeshift/provuser/provpass/3600/1700000000/101",
        "the provider credentials replace the path ones"
    );

    let (status, _, body) = fetch(&app, "/play/tok1/movie").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /play/tok1/movie");
}

#[tokio::test]
async fn an_hls_playlist_keeps_working_with_proxy_credentials() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, headers, body) = fetch(&app, "/live/iptv/secret/alpha.m3u8").await;

    assert_eq!(status, 200);
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("application/vnd.apple.mpegurl")
    );
    assert!(body.contains("#EXTM3U"), "{body}");
    assert!(
        body.contains("/live/iptv/secret/"),
        "the playlist must point back at the proxy: {body}"
    );
    assert!(!body.contains(PROVIDER_USER), "provider leak: {body}");
    assert!(!body.contains(PROVIDER_PASSWORD), "provider leak: {body}");
}

#[tokio::test]
async fn an_hls_segment_uses_the_redirect_learned_from_the_playlist() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    // Without a playlist request the proxy has no signed target for the channel.
    let (status, _, body) = fetch(&app, "/hls/tok1/alpha_seg1.ts").await;
    assert_eq!(status, 500);
    assert!(body.contains("HSL redirect url not found"), "{body}");

    fetch(&app, "/live/iptv/secret/alpha.m3u8").await;

    let (status, _, body) = fetch(&app, "/hls/tok1/alpha_seg1.ts").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /hls/tok1/alpha_seg1.ts");

    let (status, _, body) = fetch(&app, "/hlsr/tok2/iptv/secret/alpha/hash1/alpha_seg1.ts").await;
    assert_eq!(status, 200);
    assert_eq!(
        body, "MEDIA /hlsr/tok2/provuser/provpass/alpha/hash1/alpha_seg1.ts",
        "the hlsr route substitutes the provider credentials"
    );

    // A chunk without the `channel_index` shape is rejected.
    let (status, _, body) = fetch(&app, "/hls/tok1/malformed").await;
    assert_eq!(status, 500);
    assert!(body.contains("HSL malformed chunk"), "{body}");
}

#[tokio::test]
async fn the_player_api_answers_with_proxy_details() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, _, body) = fetch(&app, &format!("/player_api.php?{AUTH}")).await;
    assert_eq!(status, 200);

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("login json");
    assert_eq!(parsed["user_info"]["username"], "iptv");
    assert_eq!(parsed["user_info"]["password"], "secret");
    assert_eq!(parsed["user_info"]["auth"], 1);
    // Go builds `server_info.url` as `protocol + "://" + Hostname` — no port — and reports the
    // advertised port in `port`, `https_port` and `rtmp_port` as numbers, because its FlexInt
    // ignores the `,string` tag (it marshals itself).
    assert_eq!(parsed["server_info"]["url"], "http://proxy.test");
    assert_eq!(parsed["server_info"]["port"], 8080);
    assert_eq!(parsed["server_info"]["https_port"], 8080);
    assert_eq!(parsed["server_info"]["rtmp_port"], 8080);
    assert_eq!(parsed["server_info"]["server_protocol"], "http");
    // Field order matches the Go struct declaration, which a `serde_json::Value` would sort.
    let user_at = body.find("\"user_info\"").expect("user_info");
    let server_at = body.find("\"server_info\"").expect("server_info");
    assert!(user_at < server_at, "user_info must come first: {body}");

    // The provider was asked with its own credentials.
    let api_query = provider.last_api_query();
    assert!(api_query.contains("username=provuser"), "{api_query}");
    assert!(api_query.contains("password=provpass"), "{api_query}");
}

#[tokio::test]
async fn player_api_actions_are_proxied_and_validated() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    for (action, needle) in [
        ("get_live_categories", "Live Pack"),
        ("get_vod_categories", "Movie Pack"),
        ("get_series_categories", "Show Pack"),
        ("get_live_streams", "Alpha TV"),
        ("get_vod_streams", "Big Movie"),
        ("get_series", "Great Show"),
        ("get_series_info", "Pilot"),
        ("get_vod_info", "movie_data"),
        // The vendored client unwraps `epg_listings`, so the proxy answers with the bare array.
        ("get_short_epg", "\"channel_id\":\"alpha.epg\""),
        ("get_simple_data_table", "\"channel_id\":\"alpha.epg\""),
    ] {
        let (status, headers, body) = fetch(
            &app,
            &format!("/player_api.php?{AUTH}&action={action}&category_id=1&series_id=301&vod_id=201&stream_id=101"),
        )
        .await;
        assert_eq!(status, 200, "{action} -> {body}");
        assert_eq!(
            header_value(&headers, "content-type").as_deref(),
            Some("application/json"),
            "{action}"
        );
        assert!(body.contains(needle), "{action} -> {body}");
    }

    // Required parameters are enforced with the Go error text.
    let (status, _, body) = fetch(
        &app,
        &format!("/player_api.php?{AUTH}&action=get_series_info"),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body, "{\"error\":\"missing \\\"series_id\\\"\"}");

    let (status, _, body) = fetch(
        &app,
        &format!("/player_api.php?{AUTH}&action=get_short_epg"),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body, "{\"error\":\"missing \\\"stream_id\\\"\"}");
}

#[tokio::test]
async fn the_player_api_accepts_a_posted_form() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, _, body) = post_form(
        &app,
        "/player_api.php",
        "username=iptv&password=secret&action=get_live_categories",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("Live Pack"), "{body}");

    // A body without credentials is refused before the provider is contacted.
    let (status, _, _) = post_form(&app, "/player_api.php", "action=get_live_categories").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn the_epg_is_passed_through_as_xml() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, headers, body) = fetch(&app, &format!("/xmltv.php?{AUTH}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("application/xml")
    );
    assert_eq!(body, "<?xml version=\"1.0\"?>\n<tv></tv>\n");
}

#[tokio::test]
async fn playlist_requests_are_refused_without_credentials() {
    let provider = support::MockProvider::start().await;
    let app = xtream_app(&provider).await;

    let (status, _, _) = fetch(&app, "/get.php").await;
    assert_eq!(status, 400);

    let (status, _, _) = fetch(&app, "/get.php?username=iptv&password=wrong").await;
    assert_eq!(status, 401);

    let (status, _, _) = fetch(&app, "/apiget?username=iptv&password=wrong").await;
    assert_eq!(status, 401);

    let (status, _, _) = fetch(&app, "/player_api.php?username=iptv").await;
    assert_eq!(status, 400);

    let (status, _, _) = fetch(&app, "/xmltv.php?username=iptv&password=wrong").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn an_xtream_subscription_is_detected_from_the_playlist_url() {
    let provider = support::MockProvider::start().await;

    // No xtream flags: the credentials in M3U_URL turn the subscription into an Xtream one, and the
    // playlist file name becomes the route that serves the provider playlist.
    let cfg = config(
        "",
        &[
            "--m3u-url",
            &format!(
                "{}/get.php?username={PROVIDER_USER}&password={PROVIDER_PASSWORD}&type=m3u_plus&output=ts",
                provider.base
            ),
        ],
    );
    assert_eq!(cfg.xtream_user.as_str(), PROVIDER_USER);
    assert_eq!(cfg.xtream_base_url, provider.base);

    let app = app(state(cfg).await);
    let (status, _, body) = fetch(&app, "/iptv.m3u?username=iptv&password=secret").await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        concat!(
            "#EXTM3U\n",
            "#EXTINF:-1 tvg-id=\"raw.one\" tvg-name=\"Raw One\" group-title=\"Raw\", Raw One\n",
            "http://proxy.test:8080/live/iptv/secret/900.ts\n",
            "#EXTINF:-1 tvg-name=\"Raw HLS\" group-title=\"Raw\", Raw HLS\n",
            "http://proxy.test:8080/live/iptv/secret/alpha.m3u8\n",
        )
    );

    // The provider saw its own credentials plus the query from M3U_URL.
    let query = provider.last_get_php_query();
    assert!(query.contains("username=provuser"), "{query}");
    assert!(query.contains("password=provpass"), "{query}");
    assert!(query.contains("type=m3u_plus"), "{query}");
    assert!(query.contains("output=ts"), "{query}");
    assert!(
        !query.contains("iptv"),
        "proxy credentials leaked upstream: {query}"
    );
}

#[tokio::test]
async fn auto_detection_does_not_register_the_generic_stream_route() {
    let provider = support::MockProvider::start().await;
    let cfg = config(
        "",
        &[
            "--m3u-url",
            &format!(
                "{}/get.php?username={PROVIDER_USER}&password={PROVIDER_PASSWORD}",
                provider.base
            ),
        ],
    );
    let app = app(state(cfg).await);

    // `/live/...` is registered, the catch-all `/{user}/{pass}/{id}` is not.
    let (status, _, _) = fetch(&app, "/live/iptv/secret/101.ts").await;
    assert_eq!(status, 200);

    let app2 = {
        let _ = enable_all; // keep the import used in every cfg
        app
    };
    let (status, _, _) = fetch(&app2, "/whatever/credentials/777").await;
    assert_eq!(status, 404);
}

/// The wide-catalogue version of the single-connection test: enough categories that the concurrent
/// build genuinely overlaps, which is what makes a connection-capped provider refuse requests.
#[tokio::test]
async fn a_connection_capped_provider_falls_back_to_one_request_at_a_time() {
    let provider = support::MockProvider::start().await;
    provider.set_wide_catalogue(6);
    provider.set_max_concurrent(1);
    let app = xtream_app(&provider).await;

    let ids = |offset: i64| -> serde_json::Map<String, serde_json::Value> {
        (offset..offset + 6)
            .map(|i| (i.to_string(), serde_json::Value::Bool(true)))
            .collect()
    };
    let mut enabled = serde_json::Map::new();
    enabled.insert("live".into(), serde_json::Value::Object(ids(1)));
    enabled.insert("movies".into(), serde_json::Value::Object(ids(100)));
    enabled.insert("series".into(), serde_json::Value::Object(ids(200)));
    let enabled = serde_json::json!({ "enabled": enabled });
    let (status, _, _) = post_json(&app, "/api/categories", &enabled.to_string()).await;
    assert_eq!(status, 200);

    let (status, _, body) = fetch(&app, &format!("/get.php?{AUTH}&output=ts")).await;
    assert_eq!(status, 200, "body: {body}");

    // 6 live + 6 movie + 6 series episodes, each exactly once: nothing lost to the capped provider.
    let tracks: Vec<&str> = body.lines().skip(1).step_by(2).collect();
    assert_eq!(tracks.len(), 18, "playlist:\n{body}");
    for offset in 0..6 {
        assert!(
            body.contains(&format!("Channel {}", 1 + offset)),
            "missing a live channel"
        );
        assert!(
            body.contains(&format!("Film {}", 100 + offset)),
            "missing a movie"
        );
        assert!(
            body.contains(&format!("Show {}", 200 + offset)),
            "missing a series"
        );
    }
    assert_eq!(body.matches("#EXTINF").count(), 18);
}
