//! Contract tests for the plain (non-Xtream) mode: the proxy serves its own playlist and streams
//! every track through itself.

mod support;

use support::{
    config, fetch, fetch_with, header_value, plain_app, state, AUTH, PROXY_PASSWORD, PROXY_USER,
};

const EXPECTED_PLAYLIST: &str = concat!(
    "#EXTM3U\n",
    "#EXTINF:-1 tvg-name=\"Plain One\" group-title=\"Plain\", Plain One\n",
    "http://proxy.test:8080/anti/iptv/secret/0/900.ts\n",
    "#EXTINF:-1 tvg-name=\"Plain HLS\" group-title=\"Plain\", Plain HLS\n",
    "http://proxy.test:8080/anti/iptv/secret/1/alpha.m3u8\n",
    "#EXTINF:-1 tvg-name=\"Loop\" group-title=\"Plain\", Loop\n",
    "http://proxy.test:8080/anti/iptv/secret/2/12\n",
    "#EXTINF:-1 tvg-name=\"Limited\" group-title=\"Plain\", Limited\n",
    "http://proxy.test:8080/anti/iptv/secret/3/ratelimit\n",
    "#EXTINF:-1 tvg-name=\"Redirected\" group-title=\"Plain\", Redirected\n",
    "http://proxy.test:8080/anti/iptv/secret/4/555\n",
);

#[tokio::test]
async fn the_playlist_is_served_with_proxy_urls_and_download_headers() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, headers, body) = fetch(&app, &format!("/iptv.m3u?{AUTH}")).await;

    assert_eq!(status, 200);
    assert_eq!(body, EXPECTED_PLAYLIST);
    assert_eq!(
        header_value(&headers, "content-disposition").as_deref(),
        Some("attachment; filename=\"iptv.m3u\"")
    );
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        header_value(&headers, "accept-ranges").as_deref(),
        Some("bytes")
    );
    assert_eq!(
        header_value(&headers, "content-length").as_deref(),
        Some(EXPECTED_PLAYLIST.len().to_string().as_str())
    );
}

#[tokio::test]
async fn the_playlist_requires_the_configured_credentials() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    // No credentials at all: the Go version answered 400 because the fields were `required`.
    let (status, _, _) = fetch(&app, "/iptv.m3u").await;
    assert_eq!(status, 400);

    // Only one of the two.
    let (status, _, _) = fetch(&app, "/iptv.m3u?username=iptv").await;
    assert_eq!(status, 400);

    // Wrong credentials.
    let (status, _, _) = fetch(&app, "/iptv.m3u?username=iptv&password=nope").await;
    assert_eq!(status, 401);

    // Credentials posted as a form body are accepted too.
    let (status, _, body) = support::post_form(
        &app,
        "/iptv.m3u",
        &format!("username={PROXY_USER}&password={PROXY_PASSWORD}"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, EXPECTED_PLAYLIST);
}

#[tokio::test]
async fn the_playlist_supports_range_requests() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, headers, body) =
        fetch_with(&app, &format!("/iptv.m3u?{AUTH}"), "range", "bytes=0-10").await;
    assert_eq!(status, 206);
    assert_eq!(body, "#EXTM3U\n#EX");
    assert_eq!(
        header_value(&headers, "content-range").as_deref(),
        Some(format!("bytes 0-10/{}", EXPECTED_PLAYLIST.len()).as_str())
    );

    // A suffix range counts back from the end.
    let (status, _, body) =
        fetch_with(&app, &format!("/iptv.m3u?{AUTH}"), "range", "bytes=-5").await;
    assert_eq!(status, 206);
    assert_eq!(body, "/555\n");
}

#[tokio::test]
async fn a_track_is_streamed_from_the_provider() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, headers, body) = fetch(&app, "/anti/iptv/secret/0/900.ts").await;

    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /live/provuser/provpass/900.ts");
    assert_eq!(header_value(&headers, "x-upstream").as_deref(), Some("yes"));
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("video/mp2t")
    );
    // The stream routes are not authenticated in the Go version, and clients depend on that.
    assert_eq!(provider.hits("/live/provuser/provpass/900.ts"), 1);
}

#[tokio::test]
async fn a_media_track_only_answers_for_its_own_file_name() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    // Go registered a media track under `path.Base(track.URI)`, so another name is a miss.
    let (status, _, _) = fetch(&app, "/anti/iptv/secret/0/whatever.ts").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn an_hls_track_answers_for_any_name_and_rewrites_the_upstream() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    // `m3u8ReverseProxy` swaps the playlist file name for whatever the client asked for, so
    // relative segment requests resolve back to the same track.
    let (status, _, body) = fetch(&app, "/anti/iptv/secret/1/segment5.ts").await;
    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /live/provuser/provpass/segment5.ts");
}

#[tokio::test]
async fn an_hls_track_follows_the_provider_redirect() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, headers, body) = fetch(&app, "/anti/iptv/secret/1/alpha.m3u8").await;

    assert_eq!(status, 200);
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("application/vnd.apple.mpegurl")
    );
    // Plain mode streams the playlist verbatim (Go's m3u8ReverseProxy has no credential rewriting),
    // so the provider credentials are still in the body.
    assert!(body.contains("/live/provuser/provpass/"), "{body}");
    assert!(body.contains("#EXT-X-ENDLIST"), "{body}");
}

#[tokio::test]
async fn an_unknown_track_index_is_not_found() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, _, _) = fetch(&app, "/anti/iptv/secret/99/900.ts").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_redirect_loop_stops_after_ten_redirects() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, _, body) = fetch(&app, "/anti/iptv/secret/2/12").await;

    assert_eq!(status, 502);
    assert!(
        body.contains("Too many redirects"),
        "expected the redirect-loop error, got {body}"
    );
}

#[tokio::test]
async fn a_relative_redirect_is_resolved_against_the_current_url() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, _, body) = fetch(&app, "/anti/iptv/secret/4/555").await;

    assert_eq!(status, 200);
    assert_eq!(body, "MEDIA /live/provuser/provpass/555");
}

#[tokio::test]
async fn a_rate_limited_upstream_is_retried_and_then_streamed() {
    let provider = support::MockProvider::start().await;
    let cfg = config(
        &provider.base,
        &[
            "--m3u-url",
            &format!("{}/plain.m3u", provider.base),
            "--custom-id",
            "anti",
            "--rate-limit-retry-timeout",
            "1",
        ],
    );
    let loaded = state(cfg).await;
    iptv_proxy::handlers::playlist::bootstrap(&loaded)
        .await
        .expect("bootstrap");
    let app = support::app(loaded);

    provider.set_rate_limited(1);
    let (status, _, body) = fetch(&app, "/anti/iptv/secret/3/ratelimit").await;

    assert_eq!(status, 200, "the 458 must be retried, body was {body}");
    assert_eq!(body, "AFTER-LIMIT");
    assert_eq!(provider.hits("/ratelimit"), 2);
}

#[tokio::test]
async fn the_web_ui_and_the_404_page_are_served_without_credentials() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, headers, body) = fetch(&app, "/").await;
    assert_eq!(status, 200);
    assert_eq!(
        header_value(&headers, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert!(body.contains("IPTV Proxy - Category Manager"), "{body}");

    let (status, _, body) = fetch(&app, "/definitely-not-a-track").await;
    assert_eq!(status, 404);
    assert_eq!(body, "404 page not found");
}

#[tokio::test]
async fn categories_cannot_be_changed_without_an_xtream_provider() {
    let provider = support::MockProvider::start().await;
    let app = plain_app(&provider).await;

    let (status, _, body) = fetch(&app, "/api/categories").await;
    assert_eq!(status, 400);
    assert!(body.contains("error"), "{body}");
}
