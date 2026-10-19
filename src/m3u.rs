//! M3U parsing and serialisation.
//!
//! One parser serves all three sources (local file, remote URL, in-memory body) because the Go
//! version carried two byte-identical copies of it: `m3u.Parse` from the vendored
//! `jamesnetherton/m3u` library and `parseM3UFromReader` in `pkg/server/xtreamHandles.go`.
//!
//! Two deliberate deviations from the Go parser, both fixes for input the Go version mangled:
//!
//! * A tag value containing `=` (e.g. `tvg-logo="http://h/l.png?w=1"`) is kept whole. Go ran
//!   `strings.Split(tag, "=")` over the matched tag and kept element 1, truncating the value at the
//!   first `=` inside it.
//! * Lines longer than 64 KiB are accepted. Go used a `bufio.Scanner`, whose default token cap is
//!   64 KiB, so an unusually long `#EXTINF` aborted the whole playlist.

use std::fmt;
use std::io::{self, BufWriter, Write};

/// A `key="value"` pair on an `#EXTINF` line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub value: String,
}

impl Tag {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// One entry of a playlist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    pub name: String,
    pub length: i64,
    pub uri: String,
    pub tags: Vec<Tag>,
}

impl Track {
    pub fn new(name: impl Into<String>, length: i64, uri: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            length,
            uri: uri.into(),
            tags: Vec::new(),
        }
    }

    /// Value of tag `name`, if present.
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.name == name)
            .map(|tag| tag.value.as_str())
    }

    /// Appends `tag`, ignoring an empty value the way the Xtream generator did (it only emitted a
    /// tag when the API returned something for it).
    pub fn push_tag(&mut self, name: &str, value: &str) {
        if !value.is_empty() {
            self.tags.push(Tag::new(name, value));
        }
    }
}

/// A parsed playlist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Playlist {
    pub tracks: Vec<Track>,
}

impl Playlist {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }
}

/// Reasons a playlist cannot be parsed. The texts match the Go error strings so log output and any
/// client-visible message stay the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

const ERR_NO_HEADER: &str = "invalid m3u file format. Expected #EXTM3U file header";
const ERR_NO_COMMA: &str =
    "invalid m3u file format. Expected EXTINF metadata to contain track length and name data";
const ERR_BAD_LENGTH: &str = "unable to parse length";
const ERR_NO_TRACKS: &str = "URI provided for playlist with no tracks";

/// Parses playlist text.
pub fn parse(input: &str) -> Result<Playlist, ParseError> {
    let mut playlist = Playlist::default();
    let mut on_first_line = true;

    for raw_line in input.split('\n') {
        // Tolerate CRLF files: Go's scanner stripped the trailing \r.
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if on_first_line {
            if !line.starts_with("#EXTM3U") {
                return Err(ParseError(ERR_NO_HEADER.to_string()));
            }
            on_first_line = false;
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTINF") {
            playlist.tracks.push(parse_extinf(rest)?);
            continue;
        }

        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        let uri = line.trim();
        match playlist.tracks.last_mut() {
            Some(track) => track.uri = uri.to_string(),
            None => return Err(ParseError(ERR_NO_TRACKS.to_string())),
        }
    }

    Ok(playlist)
}

/// Parses one `#EXTINF...` line into a track whose `uri` is still empty.
fn parse_extinf(rest: &str) -> Result<Track, ParseError> {
    // Go removed every literal "#EXTINF:" occurrence, so a line such as
    // `#EXTINF:-1 foo#EXTINF:` lost both. Keep that behaviour: only the leading colon is required.
    let body = rest.strip_prefix(':').unwrap_or(rest);
    let body = body.replace("#EXTINF:", "");

    let mut parts = body.split(',');
    let head = parts.next().unwrap_or_default();
    let name = match parts.next() {
        Some(name) => name,
        None => return Err(ParseError(ERR_NO_COMMA.to_string())),
    };

    let length_token = head.split(' ').next().unwrap_or_default();
    let length = length_token
        .parse::<i64>()
        .map_err(|_| ParseError(ERR_BAD_LENGTH.to_string()))?;

    Ok(Track {
        name: name.trim().to_string(),
        length,
        uri: String::new(),
        tags: extract_tags(&body),
    })
}

/// Extracts every `name="value"` pair, mirroring Go's `([a-zA-Z0-9-]+?)="([^"]+)"` (leftmost,
/// non-overlapping, at least one character in the value).
fn extract_tags(line: &str) -> Vec<Tag> {
    let bytes = line.as_bytes();
    let mut tags = Vec::new();
    let mut cursor = 0usize;

    while cursor + 2 < bytes.len() {
        // Locate the next `="` at or after `cursor`.
        let eq = match bytes[cursor..].iter().position(|b| *b == b'=') {
            Some(offset) => cursor + offset,
            None => break,
        };
        if eq + 1 >= bytes.len() || bytes[eq + 1] != b'"' {
            cursor = eq + 1;
            continue;
        }

        // Walk left over the tag name. The non-greedy Go pattern still starts at the leftmost
        // position that can match, i.e. the beginning of the run of name characters.
        let mut start = eq;
        while start > 0 && is_name_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start == eq || start < cursor {
            // No name characters before the `=`, so Go would not have matched here.
            cursor = eq + 1;
            continue;
        }

        let value_start = eq + 2;
        let value_end = match bytes[value_start..]
            .iter()
            .position(|b| *b == b'"')
            .map(|offset| value_start + offset)
        {
            Some(end) if end > value_start => end,
            Some(_) => {
                // Empty value: `[^"]+` requires at least one character.
                cursor = value_start;
                continue;
            }
            None => break,
        };

        tags.push(Tag::new(&line[start..eq], &line[value_start..value_end]));
        cursor = value_end + 1;
    }

    tags
}

fn is_name_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-')
}

/// Quotes a tag value the way Go's `%q` did: `"`, `\` and ASCII control characters are escaped,
/// printable non-ASCII text is passed through untouched.
pub fn quote_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 || (control as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Serialises a playlist, rewriting every track URI through `rewrite`.
///
/// `rewrite` receives the original URI and the index the track will occupy in the **emitted**
/// playlist. A track whose rewrite fails is skipped and logged, exactly like `marshallInto` did, and
/// the returned vector is the surviving tracks in emitted order — those are the tracks the stream
/// routes are registered for, so file and routes always agree.
pub fn marshall_into<W, E>(
    into: &mut W,
    playlist: &Playlist,
    mut rewrite: impl FnMut(&str, usize) -> Result<String, E>,
) -> io::Result<Vec<Track>>
where
    W: Write,
    E: fmt::Display,
{
    let mut writer = BufWriter::with_capacity(1 << 20, &mut *into);
    let mut kept = Vec::with_capacity(playlist.tracks.len());

    writer.write_all(b"#EXTM3U\n")?;

    for track in &playlist.tracks {
        let uri = match rewrite(&track.uri, kept.len()) {
            Ok(uri) => uri,
            Err(err) => {
                tracing::error!("ERROR: track: {}: {}", track.name, err);
                continue;
            }
        };

        let mut header = String::with_capacity(64 + track.tags.len() * 24);
        header.push_str("#EXTINF:");
        header.push_str(&track.length.to_string());
        header.push(' ');
        for (index, tag) in track.tags.iter().enumerate() {
            header.push_str(&tag.name);
            header.push('=');
            header.push_str(&quote_value(&tag.value));
            if index + 1 != track.tags.len() {
                header.push(' ');
            }
        }

        writeln!(writer, "{header}, {name}", name = track.name)?;
        writeln!(writer, "{uri}")?;

        let mut kept_track = track.clone();
        kept_track.uri = uri;
        kept.push(kept_track);
    }

    writer.flush()?;
    Ok(kept)
}

/// Serialises a playlist into a byte vector.
pub fn marshall_bytes(playlist: &Playlist) -> Vec<u8> {
    let mut out = Vec::with_capacity(playlist.len() * 160 + 32);
    let mut cursor = std::io::Cursor::new(&mut out);
    let _ = marshall_into(&mut cursor, playlist, |uri, _| {
        Ok::<_, std::convert::Infallible>(uri.to_string())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_track_with_all_tags() {
        let input = "#EXTM3U\n#EXTINF:-1 tvg-id=\"test1\" tvg-name=\"Test Channel\" tvg-logo=\"http://example.com/logo.png\" group-title=\"Test Group\",Test Channel Name\nhttp://example.com/stream1.m3u8\n";
        let playlist = parse(input).expect("parses");

        assert_eq!(playlist.len(), 1);
        let track = &playlist.tracks[0];
        assert_eq!(track.name, "Test Channel Name");
        assert_eq!(track.uri, "http://example.com/stream1.m3u8");
        assert_eq!(track.length, -1);
        assert_eq!(track.tag("tvg-id"), Some("test1"));
        assert_eq!(track.tag("tvg-name"), Some("Test Channel"));
        assert_eq!(track.tag("tvg-logo"), Some("http://example.com/logo.png"));
        assert_eq!(track.tag("group-title"), Some("Test Group"));
    }

    #[test]
    fn tag_order_is_preserved() {
        let input = "#EXTM3U\n#EXTINF:-1 group-title=\"G\" tvg-id=\"1\",Name\nhttp://e/x\n";
        let playlist = parse(input).unwrap();
        let names: Vec<&str> = playlist.tracks[0]
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["group-title", "tvg-id"]);
    }

    #[test]
    fn keeps_equals_signs_inside_tag_values() {
        // Go truncated this to "http://h/l.png?w".
        let input = "#EXTM3U\n#EXTINF:-1 tvg-logo=\"http://h/l.png?w=1&a=2\",Name\nhttp://e/x\n";
        let playlist = parse(input).unwrap();
        assert_eq!(
            playlist.tracks[0].tag("tvg-logo"),
            Some("http://h/l.png?w=1&a=2")
        );
    }

    #[test]
    fn accepts_lines_longer_than_64kib() {
        let input = format!(
            "#EXTM3U\n#EXTINF:-1 {} ,Test\nhttp://example.com/stream\n",
            "tvg-name=\"Test\" ".repeat(6000)
        );
        assert_eq!(parse(&input).unwrap().len(), 1);
    }

    #[test]
    fn rejects_missing_header() {
        let err = parse("#EXTINF:-1,Name\nhttp://e/x\n").unwrap_err();
        assert_eq!(err.0, ERR_NO_HEADER);
    }

    #[test]
    fn accepts_header_only() {
        assert_eq!(parse("#EXTM3U\n").unwrap().len(), 0);
    }

    #[test]
    fn rejects_uri_before_any_track() {
        let err = parse("#EXTM3U\nhttp://e/x\n").unwrap_err();
        assert_eq!(err.0, ERR_NO_TRACKS);
    }

    #[test]
    fn rejects_missing_comma() {
        let err =
            parse("#EXTM3U\n#EXTINF:-1 tvg-name=\"Test\" Test Channel\nhttp://e/x\n").unwrap_err();
        assert_eq!(err.0, ERR_NO_COMMA);
    }

    #[test]
    fn rejects_unparsable_length() {
        let err = parse("#EXTM3U\n#EXTINF:invalid tvg-name=\"Test\",Test Channel\nhttp://e/x\n")
            .unwrap_err();
        assert_eq!(err.0, ERR_BAD_LENGTH);
    }

    #[test]
    fn skips_blank_and_comment_lines() {
        let input = "#EXTM3U\n\n#EXTINF:-1 tvg-name=\"T\",T\nhttp://e/1\n#EXTVLCOPT:foo\n#EXTINF:-1,T2\nhttp://e/2\n\n";
        let playlist = parse(input).unwrap();
        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist.tracks[1].name, "T2");
    }

    #[test]
    fn tolerates_crlf() {
        let input = "#EXTM3U\r\n#EXTINF:-1,T\r\nhttp://e/1\r\n";
        let playlist = parse(input).unwrap();
        assert_eq!(playlist.tracks[0].uri, "http://e/1");
    }

    #[test]
    fn truncates_names_at_the_second_comma_like_go() {
        // Documented Go behaviour: the name is only the text between the first and second comma.
        let input = "#EXTM3U\n#EXTINF:-1,First, Second\nhttp://e/x\n";
        assert_eq!(parse(input).unwrap().tracks[0].name, "First");
    }

    #[test]
    fn marshalls_with_go_quoting_and_layout() {
        let playlist = Playlist {
            tracks: vec![Track {
                name: "Chan \"One\"".to_string(),
                length: -1,
                uri: "http://proxy/1".to_string(),
                tags: vec![
                    Tag::new("tvg-id", "a\\b"),
                    Tag::new("group-title", "Sport/HD"),
                ],
            }],
        };
        let bytes = marshall_bytes(&playlist);
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            // Tags go through `%q`; the display name is written raw, exactly like Go's
            // `fmt.Sprintf("%s, %s\n%s\n", buffer, track.Name, uri)`.
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"a\\\\b\" group-title=\"Sport/HD\", Chan \"One\"\nhttp://proxy/1\n"
        );
    }

    #[test]
    fn marshalls_a_track_without_tags_like_go() {
        let playlist = Playlist {
            tracks: vec![Track::new("Name", 5, "http://e/1")],
        };
        let text = String::from_utf8(marshall_bytes(&playlist)).unwrap();
        assert_eq!(text, "#EXTM3U\n#EXTINF:5 , Name\nhttp://e/1\n");
    }

    #[test]
    fn marshall_rewrites_and_numbers_by_emitted_index() {
        let playlist = Playlist {
            tracks: vec![
                Track::new("ok1", -1, "good"),
                Track::new("bad", -1, "drop-me"),
                Track::new("ok2", -1, "good"),
            ],
        };
        let mut out = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut out);
        let kept = marshall_into(&mut cursor, &playlist, |uri, index| {
            if uri == "drop-me" {
                Err("boom")
            } else {
                Ok(format!("{uri}#{index}"))
            }
        })
        .unwrap();

        // Emitted indices are 0 and 1 — the same numbers Go computed as `i - ret`.
        assert_eq!(kept.len(), 2);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "#EXTM3U\n#EXTINF:-1 , ok1\ngood#0\n#EXTINF:-1 , ok2\ngood#1\n"
        );
    }
}
