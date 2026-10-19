//! Byte-for-byte playlist compatibility with the Go release.
//!
//! `tests/fixtures/go-playlist-slice.m3u` was captured from the running Go container
//! (`GET /get.php?username=…&password=…&type=m3u_plus`) and trimmed to a header plus live, movie
//! and series entries. Reading it and writing it back out again has to reproduce the file exactly —
//! that is the property IPTV apps depend on.

use iptv_proxy::m3u;

const FIXTURE: &str = include_str!("fixtures/go-playlist-slice.m3u");

#[test]
fn a_playlist_from_the_go_release_round_trips_byte_for_byte() {
    let playlist = m3u::parse(FIXTURE).expect("the go playlist parses");
    assert!(!playlist.tracks.is_empty());

    let bytes = m3u::marshall_bytes(&playlist);
    assert_eq!(
        String::from_utf8(bytes).expect("utf-8 output"),
        FIXTURE,
        "re-writing a playlist captured from go must not change a single byte"
    );
}

#[test]
fn the_slice_covers_every_track_shape() {
    let playlist = m3u::parse(FIXTURE).unwrap();

    let live = playlist
        .tracks
        .iter()
        .filter(|t| t.uri.contains("/live/"))
        .count();
    let movies = playlist
        .tracks
        .iter()
        .filter(|t| t.uri.contains("/movie/"))
        .count();
    let series = playlist
        .tracks
        .iter()
        .filter(|t| t.uri.contains("/series/"))
        .count();

    assert!(
        live > 0 && movies > 0 && series > 0,
        "{live}/{movies}/{series}"
    );

    // The tags the Go generator emits, in the order it emits them.
    let first = &playlist.tracks[0];
    let names: Vec<&str> = first.tags.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"tvg-name")
            && names.contains(&"tvg-logo")
            && names.contains(&"group-title"),
        "{names:?}"
    );

    // Live entries carry no extension; movie and series entries keep the container the provider
    // reported (this provider serves .mp4 films and .mkv episodes).
    assert!(!first.uri.ends_with(".ts"), "{}", first.uri);
    assert!(
        playlist
            .tracks
            .iter()
            .any(|t| t.uri.contains("/movie/") && t.uri.ends_with(".mp4")),
        "movie entries must keep their container extension"
    );
    assert!(
        playlist
            .tracks
            .iter()
            .any(|t| t.uri.contains("/series/") && t.uri.ends_with(".mkv")),
        "series entries must keep their container extension"
    );

    // Series tags: logo, name, group title — in that order, and no tvg-id.
    let episode = playlist
        .tracks
        .iter()
        .find(|t| t.uri.contains("/series/"))
        .unwrap();
    let episode_tags: Vec<&str> = episode.tags.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(episode_tags, ["tvg-logo", "tvg-name", "group-title"]);
}

#[test]
fn every_track_keeps_the_go_display_name_and_length() {
    let playlist = m3u::parse(FIXTURE).unwrap();

    for track in &playlist.tracks {
        assert_eq!(track.length, -1, "{}", track.name);
        assert!(!track.name.is_empty());
        assert!(
            !track.name.starts_with(' '),
            "the leading space after the comma is part of the layout, not the name: {:?}",
            track.name
        );
        assert!(track.uri.starts_with("http://"), "{}", track.uri);
    }
}
