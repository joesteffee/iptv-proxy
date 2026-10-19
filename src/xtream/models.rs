//! Typed models for the Xtream Codes API.
//!
//! The Go version proxied `player_api.php` through `github.com/tellytv/go.xtream-codes`, which
//! decodes the upstream JSON into typed structs and re-encodes them. That round trip is observable
//! to clients — Go struct tags such as `json:"port,string"` turn upstream numbers into JSON
//! strings, and fields the struct does not declare are dropped. The models below reproduce the same
//! field sets, field order and string/number encoding so existing clients keep working.
//!
//! Three deliberate deviations, all in the direction of not losing data:
//!
//! * FFMPEG `audio` / `video` objects are passed through untouched instead of being decoded into the
//!   ~40-field Go struct. Unknown upstream fields inside them survive.
//! * EPG `title` / `description` are passed through verbatim. Go base64-decoded and re-encoded them
//!   (base64url without padding), which is a no-op for canonical input and an error for anything
//!   else; passing them through is identical for canonical input and never fails.
//! * Field values of an unexpected JSON type (e.g. `null` where Go required a string) fall back to
//!   the zero value instead of aborting the whole request.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

// -------------------------------------------------------------------------
// Scalar helper types
// -------------------------------------------------------------------------

/// An integer that accepts a JSON number or a (possibly padded) JSON string, like Go's `FlexInt`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlexInt(pub i64);

impl FlexInt {
    pub fn get(self) -> i64 {
        self.0
    }
}

impl Serialize for FlexInt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.0)
    }
}

/// Serialises an integer as a JSON string — Go's `,string` struct tag.
pub fn quoted<S: Serializer>(value: &FlexInt, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&self::Quote(value.0))
}

struct Quote(i64);
impl std::fmt::Display for Quote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

struct FlexIntVisitor;

impl<'de> serde::de::Visitor<'de> for FlexIntVisitor {
    type Value = FlexInt;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an integer or a string containing an integer")
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FlexInt(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(FlexInt(v as i64))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(FlexInt(v as i64))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return Ok(FlexInt(0));
        }
        trimmed
            .parse::<i64>()
            .map(FlexInt)
            .or_else(|_| trimmed.parse::<f64>().map(|f| FlexInt(f as i64)))
            .map_err(|_| E::custom(format!("\"{trimmed}\" is not an integer")))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(FlexInt(0))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(FlexInt(0))
    }
}

impl<'de> Deserialize<'de> for FlexInt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FlexIntVisitor)
    }
}

/// A float that also accepts a string, like Go's `FlexFloat`. Serialised the way Go's
/// `encoding/json` would: whole numbers lose the `.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FlexFloat(pub f64);

impl FlexFloat {
    /// Formats the number the way Go's `%v` would: no exponent, no trailing `.0` on whole values.
    pub(crate) fn to_string_go(self) -> String {
        let value = self.0;
        if value.is_finite() && value == value.trunc() && value.abs() < 1e15 {
            return format!("{}", value as i64);
        }
        format!("{value}")
    }
}

impl Serialize for FlexFloat {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.is_finite() && value == value.trunc() && value.abs() < 1e15 {
            return serializer.serialize_i64(value as i64);
        }
        serializer.serialize_f64(value)
    }
}

struct FlexFloatVisitor;

impl<'de> serde::de::Visitor<'de> for FlexFloatVisitor {
    type Value = FlexFloat;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a number or a string containing a number")
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(FlexFloat(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FlexFloat(v as f64))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(FlexFloat(v as f64))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        if v.is_empty() {
            return Ok(FlexFloat(0.0));
        }
        // Go falls back to 0 for unparsable strings rather than failing the request.
        Ok(FlexFloat(v.trim().parse::<f64>().unwrap_or(0.0)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(FlexFloat(0.0))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(FlexFloat(0.0))
    }
}

impl<'de> Deserialize<'de> for FlexFloat {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FlexFloatVisitor)
    }
}

/// A boolean arriving as `0`, `1`, `true`, `false` (quoted or not), like Go's `ConvertibleBoolean`.
/// Marshals back to `0` / `1`, never `true` / `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlexBool(pub bool);

impl Serialize for FlexBool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i8(i8::from(self.0))
    }
}

/// The `,string` variant of [`FlexBool`].
pub fn quoted_bool<S: Serializer>(value: &FlexBool, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(if value.0 { "1" } else { "0" })
}

struct FlexBoolVisitor;

impl<'de> serde::de::Visitor<'de> for FlexBoolVisitor {
    type Value = FlexBool;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("0, 1, true or false")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(FlexBool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FlexBool(v != 0))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(FlexBool(v != 0))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        match v.trim() {
            "1" | "true" => Ok(FlexBool(true)),
            _ => Ok(FlexBool(false)),
        }
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(FlexBool(false))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(FlexBool(false))
    }
}

impl<'de> Deserialize<'de> for FlexBool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FlexBoolVisitor)
    }
}

/// A unix timestamp that may arrive quoted. Serialises to epoch seconds; [`quoted_timestamp`]
/// emits the string form Go's `,string` tag produces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp(pub i64);

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.0)
    }
}

pub fn quoted_timestamp<S: Serializer>(
    value: &Timestamp,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&Quote(value.0))
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        FlexInt::deserialize(deserializer).map(|v| Timestamp(v.0))
    }
}

pub fn quoted_opt_timestamp<S: Serializer>(
    value: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(ts) => serializer.collect_str(&Quote(ts.0)),
        None => serializer.serialize_none(),
    }
}

pub fn quoted_opt_int<S: Serializer>(
    value: &Option<FlexInt>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => serializer.collect_str(&Quote(v.0)),
        None => serializer.serialize_none(),
    }
}

/// A string that tolerates the JSON types real Xtream servers actually send for it.
fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(v.to_string())
        }
        fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
            Ok(v)
        }
        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v.to_string())
        }
        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
            Ok(FlexFloat(v).to_string_go())
        }
        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
            Ok(v.to_string())
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(String::new())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(String::new())
        }
    }
    deserializer.deserialize_any(V)
}

// -------------------------------------------------------------------------
// API models
// -------------------------------------------------------------------------

/// `server_info` from the authentication call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerInfo {
    #[serde(rename = "https_port", serialize_with = "quoted", default)]
    pub https_port: FlexInt,
    #[serde(rename = "port", serialize_with = "quoted", default)]
    pub port: FlexInt,
    #[serde(rename = "process", default)]
    pub process: bool,
    #[serde(rename = "rtmp_port", serialize_with = "quoted", default)]
    pub rtmp_port: FlexInt,
    #[serde(
        rename = "server_protocol",
        default,
        deserialize_with = "lenient_string"
    )]
    pub protocol: String,
    #[serde(rename = "time_now", default, deserialize_with = "lenient_string")]
    pub time_now: String,
    #[serde(rename = "timestamp_now", serialize_with = "quoted_timestamp", default)]
    pub timestamp_now: Timestamp,
    #[serde(rename = "timezone", default, deserialize_with = "lenient_string")]
    pub timezone: String,
    #[serde(rename = "url", default, deserialize_with = "lenient_string")]
    pub url: String,
}

/// `user_info` from the authentication call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserInfo {
    #[serde(
        rename = "active_cons",
        serialize_with = "quoted",
        default,
        alias = "active_connections"
    )]
    pub active_connections: FlexInt,
    #[serde(
        rename = "allowed_output_formats",
        default,
        deserialize_with = "lenient_strings"
    )]
    pub allowed_output_formats: Vec<String>,
    #[serde(rename = "auth", default)]
    pub auth: FlexBool,
    #[serde(rename = "created_at", default)]
    pub created_at: Timestamp,
    #[serde(rename = "exp_date", default)]
    pub exp_date: Option<Timestamp>,
    #[serde(rename = "is_trial", serialize_with = "quoted_bool", default)]
    pub is_trial: FlexBool,
    #[serde(rename = "max_connections", serialize_with = "quoted", default)]
    pub max_connections: FlexInt,
    #[serde(rename = "message", default, deserialize_with = "lenient_string")]
    pub message: String,
    #[serde(rename = "password", default, deserialize_with = "lenient_string")]
    pub password: String,
    #[serde(rename = "status", default, deserialize_with = "lenient_string")]
    pub status: String,
    #[serde(rename = "username", default, deserialize_with = "lenient_string")]
    pub username: String,
}

fn lenient_strings<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        One(String),
        Many(Vec<String>),
        Other(Value),
    }
    Ok(match Either::deserialize(deserializer)? {
        Either::One(single) => vec![single],
        Either::Many(many) => many,
        Either::Other(other) => vec![other.to_string()],
    })
}

/// The body of a successful `player_api.php` login.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthenticationResponse {
    #[serde(rename = "user_info", default)]
    pub user_info: UserInfo,
    #[serde(rename = "server_info", default)]
    pub server_info: ServerInfo,
}

/// One category, as returned by `get_live_categories` and friends.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Category {
    #[serde(rename = "category_id", serialize_with = "quoted", default)]
    pub id: FlexInt,
    #[serde(rename = "category_name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "parent_id", default)]
    pub parent: FlexInt,
}

/// One live or VOD stream, as returned by `get_live_streams` / `get_vod_streams`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stream {
    #[serde(rename = "added", default)]
    pub added: Option<Timestamp>,
    #[serde(rename = "category_id", serialize_with = "quoted", default)]
    pub category_id: FlexInt,
    #[serde(rename = "category_name", default, deserialize_with = "lenient_string")]
    pub category_name: String,
    #[serde(
        rename = "container_extension",
        default,
        deserialize_with = "lenient_string"
    )]
    pub container_extension: String,
    #[serde(rename = "custom_sid", default, deserialize_with = "lenient_string")]
    pub custom_sid: String,
    #[serde(
        rename = "direct_source",
        default,
        deserialize_with = "lenient_string",
        skip_serializing_if = "String::is_empty"
    )]
    pub direct_source: String,
    #[serde(
        rename = "epg_channel_id",
        default,
        deserialize_with = "lenient_string"
    )]
    pub epg_channel_id: String,
    #[serde(rename = "stream_icon", default, deserialize_with = "lenient_string")]
    pub icon: String,
    #[serde(rename = "stream_id", default)]
    pub id: FlexInt,
    #[serde(rename = "name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "num", default)]
    pub number: FlexInt,
    #[serde(rename = "rating", default)]
    pub rating: FlexFloat,
    #[serde(rename = "rating_5based", default)]
    pub rating_5based: FlexFloat,
    #[serde(rename = "tv_archive", default)]
    pub tv_archive: FlexInt,
    #[serde(rename = "tv_archive_duration", default)]
    pub tv_archive_duration: Option<FlexInt>,
    #[serde(rename = "stream_type", default, deserialize_with = "lenient_string")]
    pub stream_type: String,
}

/// A series entry in `get_series`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SeriesInfo {
    #[serde(
        rename = "backdrop_path",
        default,
        deserialize_with = "opt_backdrop",
        skip_serializing_if = "Option::is_none"
    )]
    pub backdrop_path: Option<Backdrop>,
    #[serde(rename = "cast", default, deserialize_with = "lenient_string")]
    pub cast: String,
    #[serde(rename = "category_id", serialize_with = "quoted_opt_int", default)]
    pub category_id: Option<FlexInt>,
    #[serde(rename = "cover", default, deserialize_with = "lenient_string")]
    pub cover: String,
    #[serde(rename = "director", default, deserialize_with = "lenient_string")]
    pub director: String,
    #[serde(
        rename = "episode_run_time",
        default,
        deserialize_with = "lenient_string"
    )]
    pub episode_run_time: String,
    #[serde(rename = "genre", default, deserialize_with = "lenient_string")]
    pub genre: String,
    #[serde(
        rename = "last_modified",
        serialize_with = "quoted_opt_timestamp",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_modified: Option<Timestamp>,
    #[serde(rename = "name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "num", default)]
    pub num: FlexInt,
    #[serde(rename = "plot", default, deserialize_with = "lenient_string")]
    pub plot: String,
    #[serde(rename = "rating", serialize_with = "quoted", default)]
    pub rating: FlexInt,
    #[serde(rename = "rating_5based", default)]
    pub rating_5based: FlexFloat,
    #[serde(rename = "releaseDate", default, deserialize_with = "lenient_string")]
    pub release_date: String,
    #[serde(rename = "series_id", default)]
    pub series_id: FlexInt,
    #[serde(rename = "stream_type", default, deserialize_with = "lenient_string")]
    pub stream_type: String,
    #[serde(
        rename = "youtube_trailer",
        default,
        deserialize_with = "lenient_string"
    )]
    pub youtube_trailer: String,
}

/// `backdrop_path` is an array in most responses and a bare string in some.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backdrop {
    Single(String),
    Many(Vec<String>),
}

impl Serialize for Backdrop {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Backdrop::Single(one) => serializer.serialize_str(one),
            Backdrop::Many(many) => many.serialize(serializer),
        }
    }
}

fn opt_backdrop<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Backdrop>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => None,
        Some(Raw::One(one)) => Some(Backdrop::Single(one)),
        Some(Raw::Many(many)) => Some(Backdrop::Many(many)),
    })
}

/// One episode inside `get_series_info`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SeriesEpisode {
    #[serde(rename = "added", default, deserialize_with = "lenient_string")]
    pub added: String,
    #[serde(
        rename = "container_extension",
        default,
        deserialize_with = "lenient_string"
    )]
    pub container_extension: String,
    #[serde(rename = "custom_sid", default, deserialize_with = "lenient_string")]
    pub custom_sid: String,
    #[serde(rename = "direct_source", default, deserialize_with = "lenient_string")]
    pub direct_source: String,
    #[serde(rename = "episode_num", default)]
    pub episode_num: FlexInt,
    #[serde(rename = "id", default, deserialize_with = "lenient_string")]
    pub id: String,
    #[serde(rename = "info", default)]
    pub info: EpisodeInfo,
    #[serde(rename = "season", default)]
    pub season: FlexInt,
    #[serde(rename = "title", default, deserialize_with = "lenient_string")]
    pub title: String,
}

/// The `info` block of an episode. `audio` and `video` are passed through verbatim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpisodeInfo {
    #[serde(rename = "audio", default)]
    pub audio: Value,
    #[serde(rename = "bitrate", default)]
    pub bitrate: FlexInt,
    #[serde(rename = "duration", default, deserialize_with = "lenient_string")]
    pub duration: String,
    #[serde(rename = "duration_secs", default)]
    pub duration_secs: FlexInt,
    #[serde(rename = "movie_image", default, deserialize_with = "lenient_string")]
    pub movie_image: String,
    #[serde(rename = "name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "plot", default, deserialize_with = "lenient_string")]
    pub plot: String,
    #[serde(rename = "rating", default)]
    pub rating: FlexFloat,
    #[serde(rename = "releasedate", default, deserialize_with = "lenient_string")]
    pub release_date: String,
    #[serde(rename = "video", default)]
    pub video: Value,
}

/// The body of `get_series_info`. `BTreeMap` keeps season keys sorted, which is what Go's
/// `encoding/json` did for its `map[string][]SeriesEpisode`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Series {
    #[serde(rename = "episodes", default)]
    pub episodes: BTreeMap<String, Vec<SeriesEpisode>>,
    #[serde(rename = "info", default)]
    pub info: SeriesInfo,
    #[serde(rename = "seasons", default)]
    pub seasons: Vec<Value>,
}

impl Series {
    /// Total episode count across all seasons.
    pub fn episode_count(&self) -> usize {
        self.episodes.values().map(Vec::len).sum()
    }
}

/// `get_vod_info`: the whole payload is forwarded, with the two well-known objects typed loosely.
pub type VodInfo = Map<String, Value>;

/// One EPG listing. `title` and `description` stay exactly as the upstream sent them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpgInfo {
    #[serde(rename = "channel_id", default, deserialize_with = "lenient_string")]
    pub channel_id: String,
    #[serde(rename = "description", default, deserialize_with = "lenient_string")]
    pub description: String,
    #[serde(rename = "end", default, deserialize_with = "lenient_string")]
    pub end: String,
    #[serde(rename = "epg_id", serialize_with = "quoted", default)]
    pub epg_id: FlexInt,
    #[serde(rename = "has_archive", default)]
    pub has_archive: FlexBool,
    #[serde(rename = "id", serialize_with = "quoted", default)]
    pub id: FlexInt,
    #[serde(rename = "lang", default, deserialize_with = "lenient_string")]
    pub lang: String,
    #[serde(rename = "now_playing", default)]
    pub now_playing: FlexBool,
    #[serde(rename = "start", default, deserialize_with = "lenient_string")]
    pub start: String,
    #[serde(rename = "start_timestamp", default)]
    pub start_timestamp: Timestamp,
    #[serde(rename = "stop_timestamp", default)]
    pub stop_timestamp: Timestamp,
    #[serde(rename = "title", default, deserialize_with = "lenient_string")]
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flex_int_accepts_quoted_and_unquoted() {
        assert_eq!(
            serde_json::from_str::<FlexInt>("1471382").unwrap().0,
            1471382
        );
        assert_eq!(
            serde_json::from_str::<FlexInt>("\"1471382\"").unwrap().0,
            1471382
        );
        assert_eq!(serde_json::from_str::<FlexInt>("\" 42 \"").unwrap().0, 42);
        assert_eq!(serde_json::from_str::<FlexInt>("null").unwrap().0, 0);
    }

    #[test]
    fn flex_int_never_produces_scientific_notation() {
        let text = serde_json::to_string(&FlexInt(1471382)).unwrap();
        assert_eq!(text, "1471382");
        assert!(!text.contains('e') && !text.contains('E'));
    }

    #[test]
    fn category_id_is_emitted_as_string_like_go_string_tag() {
        let category: Category =
            serde_json::from_str(r#"{"category_id":123,"category_name":"Sport"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&category).unwrap(),
            r#"{"category_id":"123","category_name":"Sport","parent_id":0}"#
        );
    }

    #[test]
    fn stream_keeps_go_field_order_and_optionality() {
        // A `serde_json::Value` would sort the keys, so the order is read from the serialised text.
        let stream: Stream =
            serde_json::from_str(r#"{"stream_id":7,"name":"One","direct_source":""}"#).unwrap();
        let text = serde_json::to_string(&stream).unwrap();

        let order = [
            "added",
            "category_id",
            "category_name",
            "container_extension",
            "custom_sid",
            "epg_channel_id",
            "stream_icon",
            "stream_id",
            "name",
            "num",
            "rating",
            "rating_5based",
            "tv_archive",
            "tv_archive_duration",
            "stream_type",
        ];

        let mut previous = 0usize;
        for key in order {
            let marker = format!("\"{key}\"");
            let position = text
                .find(&marker)
                .unwrap_or_else(|| panic!("{key} missing from {text}"));
            assert!(
                position > previous,
                "{key} is out of order in {text} (Go emits {order:?})"
            );
            previous = position;
        }

        assert!(
            !text.contains("\"direct_source\""),
            "an empty direct_source is dropped, matching Go's `omitempty`"
        );
        assert!(
            text.contains("\"stream_id\":7"),
            "stream_id is a number: {text}"
        );
        assert!(
            text.contains("\"category_id\":\"0\""),
            "category_id is quoted: {text}"
        );
        assert!(
            text.contains("\"added\":null"),
            "a missing timestamp stays null: {text}"
        );
    }

    #[test]
    fn flex_float_drops_the_fraction_for_whole_numbers() {
        assert_eq!(serde_json::to_string(&FlexFloat(4.5)).unwrap(), "4.5");
        assert_eq!(serde_json::to_string(&FlexFloat(0.0)).unwrap(), "0");
        assert_eq!(serde_json::from_str::<FlexFloat>("\"\"").unwrap().0, 0.0);
        assert_eq!(
            serde_json::from_str::<FlexFloat>("\"nope\"").unwrap().0,
            0.0
        );
    }

    #[test]
    fn flex_bool_round_trips_as_zero_or_one() {
        assert!(serde_json::from_str::<FlexBool>("\"1\"").unwrap().0);
        assert_eq!(serde_json::to_string(&FlexBool(true)).unwrap(), "1");
        let auth: UserInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(serde_json::to_value(&auth).unwrap()["is_trial"], json!("0"));
    }

    #[test]
    fn timestamps_serialise_as_epoch_seconds() {
        let info: ServerInfo = serde_json::from_str(r#"{"timestamp_now":"1700000000"}"#).unwrap();
        assert_eq!(info.timestamp_now.0, 1700000000);
        assert_eq!(
            serde_json::to_value(&info).unwrap()["timestamp_now"],
            json!("1700000000")
        );
    }

    #[test]
    fn backdrop_accepts_string_and_array() {
        let single: SeriesInfo = serde_json::from_str(r#"{"backdrop_path":"one.jpg"}"#).unwrap();
        assert_eq!(
            single.backdrop_path,
            Some(Backdrop::Single("one.jpg".to_string()))
        );
        let many: SeriesInfo =
            serde_json::from_str(r#"{"backdrop_path":["a.jpg","b.jpg"]}"#).unwrap();
        assert_eq!(
            many.backdrop_path,
            Some(Backdrop::Many(vec![
                "a.jpg".to_string(),
                "b.jpg".to_string()
            ]))
        );
        let missing: SeriesInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.backdrop_path, None);
        assert!(serde_json::to_value(&missing)
            .unwrap()
            .get("backdrop_path")
            .is_none());
    }

    #[test]
    fn series_episodes_keep_sorted_season_keys() {
        let series: Series = serde_json::from_str(
            r#"{"episodes":{"2":[{"id":"b"}],"10":[{"id":"c"}],"1":[{"id":"a"}]}}"#,
        )
        .unwrap();
        let keys: Vec<&str> = series.episodes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["1", "10", "2"]);
        assert_eq!(series.episode_count(), 3);
    }

    #[test]
    fn lenient_fields_survive_unexpected_types() {
        let stream: Stream = serde_json::from_str(r#"{"name":null,"stream_icon":12345}"#).unwrap();
        assert_eq!(stream.name, "");
        assert_eq!(stream.icon, "12345");
    }

    #[test]
    fn auth_response_matches_library_field_names() {
        let auth: AuthenticationResponse = serde_json::from_str(
            r#"{"server_info":{"port":"80","url":"http://x"},"user_info":{"username":"u","auth":1,"exp_date":null,"max_connections":"1"}}"#,
        )
        .unwrap();
        assert_eq!(auth.server_info.port.0, 80);
        assert_eq!(auth.user_info.username, "u");
        assert!(auth.user_info.auth.0);
        assert_eq!(auth.user_info.exp_date, None);
        let out = serde_json::to_string(&auth).unwrap();
        assert!(out.contains(r#""user_info""#));
        assert!(out.contains(r#""max_connections":"1""#));
    }
}
