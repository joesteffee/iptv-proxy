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
/// Marshals back to `0` / `1`, never `true` / `false`, and keeps the quoting the provider used:
/// that type remembered whether the value arrived inside quotes and repeats it, so a client that
/// type-checks `is_trial` sees exactly what it saw from the Go proxy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlexBool {
    /// The parsed value.
    pub value: bool,
    quoted: bool,
}

impl FlexBool {
    /// A boolean that serialises unquoted.
    pub fn new(value: bool) -> Self {
        Self {
            value,
            quoted: false,
        }
    }

    /// The parsed value.
    pub fn get(self) -> bool {
        self.value
    }
}

impl Serialize for FlexBool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.quoted {
            serializer.collect_str(if self.value { &"1" } else { &"0" })
        } else {
            serializer.serialize_i8(i8::from(self.value))
        }
    }
}

struct FlexBoolVisitor;

impl<'de> serde::de::Visitor<'de> for FlexBoolVisitor {
    type Value = FlexBool;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("0, 1, true or false")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(FlexBool::new(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(FlexBool::new(v != 0))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(FlexBool::new(v != 0))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(FlexBool {
            value: matches!(v.trim(), "1" | "true"),
            quoted: true,
        })
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(FlexBool::new(false))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(FlexBool::new(false))
    }
}

impl<'de> Deserialize<'de> for FlexBool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FlexBoolVisitor)
    }
}

/// A unix timestamp that may arrive quoted. Like Go's `Timestamp` it remembers whether the
/// provider wrapped it in quotes and emits the same shape, because `,string` has no effect on a
/// type that marshals itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    seconds: i64,
    quoted: bool,
}

impl Timestamp {
    /// A timestamp that serialises unquoted.
    pub fn new(seconds: i64) -> Self {
        Self {
            seconds,
            quoted: false,
        }
    }

    /// Epoch seconds.
    pub fn get(self) -> i64 {
        self.seconds
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.quoted {
            serializer.collect_str(&self.seconds)
        } else {
            serializer.serialize_i64(self.seconds)
        }
    }
}

struct TimestampVisitor;

impl<'de> serde::de::Visitor<'de> for TimestampVisitor {
    type Value = Timestamp;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a unix timestamp, quoted or not")
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Timestamp::new(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(Timestamp::new(v as i64))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(Timestamp::new(v as i64))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let trimmed = v.trim();
        let seconds = trimmed
            .parse::<i64>()
            .map_err(|_| E::custom(format!("\"{trimmed}\" is not a timestamp")))?;
        Ok(Timestamp {
            seconds,
            quoted: true,
        })
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TimestampVisitor)
    }
}

/// Go decodes `null` into a struct as a no-op, so an object a provider sends as `null` has to
/// yield the zero value instead of an error. Servers do this for the episode `info` object.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Go's zero `time.Time` marshals to -62135596800, and that is what the Go release answered for a
/// listing without timestamps. Apps subtract one from the other, so keep the same sentinel.
const GO_ZERO_TIME_SECONDS: i64 = -62_135_596_800;

fn go_zero_timestamp() -> Timestamp {
    Timestamp::new(GO_ZERO_TIME_SECONDS)
}

fn epg_timestamp<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
    Ok(Option::<Timestamp>::deserialize(deserializer)?
        .unwrap_or_else(|| Timestamp::new(GO_ZERO_TIME_SECONDS)))
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
    #[serde(rename = "https_port", default)]
    pub https_port: FlexInt,
    #[serde(rename = "port", default)]
    pub port: FlexInt,
    #[serde(rename = "process", default)]
    pub process: bool,
    #[serde(rename = "rtmp_port", default)]
    pub rtmp_port: FlexInt,
    #[serde(
        rename = "server_protocol",
        default,
        deserialize_with = "lenient_string"
    )]
    pub protocol: String,
    #[serde(rename = "time_now", default, deserialize_with = "lenient_string")]
    pub time_now: String,
    #[serde(rename = "timestamp_now", default)]
    pub timestamp_now: Timestamp,
    #[serde(rename = "timezone", default, deserialize_with = "lenient_string")]
    pub timezone: String,
    #[serde(rename = "url", default, deserialize_with = "lenient_string")]
    pub url: String,
}

/// `user_info` from the authentication call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserInfo {
    #[serde(rename = "active_cons", default, alias = "active_connections")]
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
    #[serde(rename = "is_trial", default)]
    pub is_trial: FlexBool,
    #[serde(rename = "max_connections", default)]
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
    #[serde(rename = "category_id", default)]
    pub id: FlexInt,
    #[serde(rename = "category_name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "parent_id", default)]
    pub parent: FlexInt,
}

/// One live or VOD stream, as returned by `get_live_streams` / `get_vod_streams`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stream {
    #[serde(
        rename = "added",
        default = "go_zero_timestamp",
        deserialize_with = "epg_timestamp"
    )]
    pub added: Timestamp,
    #[serde(rename = "category_id", default)]
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
    // Go picked the id with `getFieldValue`, trying `stream_id`, `streamId`, `StreamID` then `ID`
    // (and, on the VOD path, `vod_id`/`series_id`). Aliases cover the same spellings; when a payload
    // carries several of them the last one in the document wins rather than the first in Go's chain.
    #[serde(
        rename = "stream_id",
        alias = "id",
        alias = "ID",
        alias = "streamId",
        alias = "StreamID",
        alias = "vod_id",
        alias = "series_id",
        default
    )]
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
    #[serde(rename = "category_id", default)]
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
    /// Go modelled this as an integer, so a provider sending `"rating": 8.1` broke the whole
    /// `get_series` answer (empty body). The decimal is kept instead — a client that worked before
    /// still sees an integer, one that got nothing before now gets the list.
    #[serde(rename = "rating", default)]
    pub rating: FlexFloat,
    #[serde(rename = "rating_5based", default)]
    pub rating_5based: FlexFloat,
    #[serde(rename = "releaseDate", default, deserialize_with = "lenient_string")]
    pub release_date: String,
    #[serde(
        rename = "series_id",
        alias = "seriesid",
        alias = "SeriesID",
        alias = "id",
        alias = "ID",
        default
    )]
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
    #[serde(rename = "info", default, deserialize_with = "null_to_default")]
    pub info: EpisodeInfo,
    #[serde(rename = "season", default)]
    pub season: FlexInt,
    #[serde(rename = "title", default, deserialize_with = "lenient_string")]
    pub title: String,
}

/// The `info` block of an episode. `audio` and `video` are passed through verbatim.
/// The `disposition` object inside an FFMPEG stream description.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Disposition {
    #[serde(rename = "attached_pic", default)]
    pub attached_pic: FlexInt,
    #[serde(rename = "clean_effects", default)]
    pub clean_effects: FlexInt,
    #[serde(rename = "comment", default)]
    pub comment: FlexInt,
    #[serde(rename = "default", default)]
    pub default: FlexInt,
    #[serde(rename = "dub", default)]
    pub dub: FlexInt,
    #[serde(rename = "forced", default)]
    pub forced: FlexInt,
    #[serde(rename = "hearing_impaired", default)]
    pub hearing_impaired: FlexInt,
    #[serde(rename = "karaoke", default)]
    pub karaoke: FlexInt,
    #[serde(rename = "lyrics", default)]
    pub lyrics: FlexInt,
    #[serde(rename = "original", default)]
    pub original: FlexInt,
    #[serde(rename = "visual_impaired", default)]
    pub visual_impaired: FlexInt,
}

/// One entry of an FFMPEG stream's `side_data_list`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SideData {
    #[serde(rename = "displaymatrix", default, deserialize_with = "lenient_string")]
    pub displaymatrix: String,
    #[serde(rename = "rotation", default)]
    pub rotation: FlexInt,
    #[serde(rename = "side_data_size", default)]
    pub side_data_size: FlexInt,
    #[serde(
        rename = "side_data_type",
        default,
        deserialize_with = "lenient_string"
    )]
    pub side_data_type: String,
}

/// The `audio`/`video` blocks the provider produces by running ffprobe over the file. The Go
/// release re-marshalled its own struct for these, so a provider that omitted them still got a
/// full object of zeroes back — apps read `audio.channels` and friends unconditionally. Same here,
/// except anything the provider adds on top is kept in `extra` instead of being dropped.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FfmpegStreamInfo {
    #[serde(
        rename = "avg_frame_rate",
        default,
        deserialize_with = "lenient_string"
    )]
    pub avg_frame_rate: String,
    #[serde(rename = "bit_rate", default, deserialize_with = "lenient_string")]
    pub bit_rate: String,
    #[serde(
        rename = "bits_per_raw_sample",
        default,
        deserialize_with = "lenient_string"
    )]
    pub bits_per_raw_sample: String,
    #[serde(rename = "bits_per_sample", default)]
    pub bits_per_sample: FlexInt,
    #[serde(
        rename = "channel_layout",
        default,
        deserialize_with = "lenient_string"
    )]
    pub channel_layout: String,
    #[serde(rename = "channels", default)]
    pub channels: FlexInt,
    #[serde(
        rename = "chroma_location",
        default,
        deserialize_with = "lenient_string"
    )]
    pub chroma_location: String,
    #[serde(
        rename = "codec_long_name",
        default,
        deserialize_with = "lenient_string"
    )]
    pub codec_long_name: String,
    #[serde(rename = "codec_name", default, deserialize_with = "lenient_string")]
    pub codec_name: String,
    #[serde(rename = "codec_tag", default, deserialize_with = "lenient_string")]
    pub codec_tag: String,
    #[serde(
        rename = "codec_tag_string",
        default,
        deserialize_with = "lenient_string"
    )]
    pub codec_tag_string: String,
    #[serde(
        rename = "codec_time_base",
        default,
        deserialize_with = "lenient_string"
    )]
    pub codec_time_base: String,
    #[serde(rename = "codec_type", default, deserialize_with = "lenient_string")]
    pub codec_type: String,
    #[serde(rename = "coded_height", default)]
    pub coded_height: FlexInt,
    #[serde(rename = "coded_width", default)]
    pub coded_width: FlexInt,
    #[serde(
        rename = "color_primaries",
        default,
        deserialize_with = "lenient_string"
    )]
    pub color_primaries: String,
    #[serde(rename = "color_range", default, deserialize_with = "lenient_string")]
    pub color_range: String,
    #[serde(rename = "color_space", default, deserialize_with = "lenient_string")]
    pub color_space: String,
    #[serde(
        rename = "color_transfer",
        default,
        deserialize_with = "lenient_string"
    )]
    pub color_transfer: String,
    #[serde(
        rename = "display_aspect_ratio",
        default,
        deserialize_with = "lenient_string"
    )]
    pub display_aspect_ratio: String,
    #[serde(rename = "divx_packed", default, deserialize_with = "lenient_string")]
    pub divx_packed: String,
    #[serde(rename = "dmix_mode", default, deserialize_with = "lenient_string")]
    pub dmix_mode: String,
    #[serde(rename = "duration", default, deserialize_with = "lenient_string")]
    pub duration: String,
    #[serde(rename = "duration_ts", default)]
    pub duration_ts: FlexInt,
    #[serde(rename = "has_b_frames", default)]
    pub has_b_frames: FlexInt,
    #[serde(rename = "height", default)]
    pub height: FlexInt,
    #[serde(rename = "id", default, deserialize_with = "lenient_string")]
    pub id: String,
    #[serde(rename = "index", default)]
    pub index: FlexInt,
    #[serde(rename = "is_avc", default, deserialize_with = "lenient_string")]
    pub is_avc: String,
    #[serde(rename = "level", default)]
    pub level: FlexInt,
    #[serde(rename = "loro_cmixlev", default, deserialize_with = "lenient_string")]
    pub loro_cmixlev: String,
    #[serde(
        rename = "loro_surmixlev",
        default,
        deserialize_with = "lenient_string"
    )]
    pub loro_surmixlev: String,
    #[serde(rename = "ltrt_cmixlev", default, deserialize_with = "lenient_string")]
    pub ltrt_cmixlev: String,
    #[serde(
        rename = "ltrt_surmixlev",
        default,
        deserialize_with = "lenient_string"
    )]
    pub ltrt_surmixlev: String,
    #[serde(rename = "max_bit_rate", default, deserialize_with = "lenient_string")]
    pub max_bit_rate: String,
    #[serde(
        rename = "nal_length_size",
        default,
        deserialize_with = "lenient_string"
    )]
    pub nal_length_size: String,
    #[serde(rename = "nb_frames", default, deserialize_with = "lenient_string")]
    pub nb_frames: String,
    #[serde(rename = "pix_fmt", default, deserialize_with = "lenient_string")]
    pub pix_fmt: String,
    #[serde(rename = "profile", default, deserialize_with = "lenient_string")]
    pub profile: String,
    #[serde(
        rename = "quarter_sample",
        default,
        deserialize_with = "lenient_string"
    )]
    pub quarter_sample: String,
    #[serde(rename = "r_frame_rate", default, deserialize_with = "lenient_string")]
    pub r_frame_rate: String,
    #[serde(rename = "refs", default)]
    pub refs: FlexInt,
    #[serde(
        rename = "sample_aspect_ratio",
        default,
        deserialize_with = "lenient_string"
    )]
    pub sample_aspect_ratio: String,
    #[serde(rename = "sample_fmt", default, deserialize_with = "lenient_string")]
    pub sample_fmt: String,
    #[serde(rename = "sample_rate", default, deserialize_with = "lenient_string")]
    pub sample_rate: String,
    #[serde(rename = "start_pts", default)]
    pub start_pts: FlexInt,
    #[serde(rename = "start_time", default, deserialize_with = "lenient_string")]
    pub start_time: String,
    #[serde(rename = "time_base", default, deserialize_with = "lenient_string")]
    pub time_base: String,
    #[serde(rename = "timecode", default, deserialize_with = "lenient_string")]
    pub timecode: String,
    #[serde(rename = "width", default)]
    pub width: FlexInt,
    /// ffprobe's free-form tag map. `null` when the provider has nothing to say.
    #[serde(rename = "tags", default)]
    pub tags: Option<Value>,
    #[serde(rename = "disposition", default, deserialize_with = "null_to_default")]
    pub disposition: Disposition,
    #[serde(rename = "side_data_list", default)]
    pub side_data_list: Option<Vec<SideData>>,
    /// Fields the provider adds beyond ffprobe's vocabulary.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpisodeInfo {
    #[serde(rename = "audio", default, deserialize_with = "null_to_default")]
    pub audio: FfmpegStreamInfo,
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
    #[serde(rename = "video", default, deserialize_with = "null_to_default")]
    pub video: FfmpegStreamInfo,
    /// Anything the provider sends beyond the fields the Go release knew about.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `get_vod_info`: `info` plus `movie_data`, typed the way the Go release typed them so a sparse
/// provider still gets a complete object, with `extra` keeping whatever else came along.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VodInfo {
    #[serde(rename = "info", default, deserialize_with = "null_to_default")]
    pub info: VodInfoBlock,
    #[serde(rename = "movie_data", default, deserialize_with = "null_to_default")]
    pub movie_data: VodMovieData,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `info` block of `get_vod_info`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VodInfoBlock {
    #[serde(rename = "audio", default, deserialize_with = "null_to_default")]
    pub audio: FfmpegStreamInfo,
    #[serde(
        rename = "backdrop_path",
        default,
        deserialize_with = "opt_lenient_strings"
    )]
    pub backdrop_path: Option<Vec<String>>,
    #[serde(rename = "bitrate", default)]
    pub bitrate: FlexInt,
    #[serde(rename = "cast", default, deserialize_with = "lenient_string")]
    pub cast: String,
    #[serde(rename = "director", default, deserialize_with = "lenient_string")]
    pub director: String,
    #[serde(rename = "duration", default, deserialize_with = "lenient_string")]
    pub duration: String,
    #[serde(rename = "duration_secs", default)]
    pub duration_secs: FlexInt,
    #[serde(rename = "genre", default, deserialize_with = "lenient_string")]
    pub genre: String,
    #[serde(rename = "movie_image", default, deserialize_with = "lenient_string")]
    pub movie_image: String,
    #[serde(rename = "plot", default, deserialize_with = "lenient_string")]
    pub plot: String,
    #[serde(rename = "rating", default)]
    pub rating: FlexFloat,
    #[serde(rename = "releasedate", default, deserialize_with = "lenient_string")]
    pub release_date: String,
    #[serde(rename = "tmdb_id", default)]
    pub tmdb_id: FlexInt,
    #[serde(rename = "video", default, deserialize_with = "null_to_default")]
    pub video: FfmpegStreamInfo,
    #[serde(
        rename = "youtube_trailer",
        default,
        deserialize_with = "lenient_string"
    )]
    pub youtube_trailer: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `movie_data` block of `get_vod_info`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VodMovieData {
    #[serde(
        rename = "added",
        default = "go_zero_timestamp",
        deserialize_with = "epg_timestamp"
    )]
    pub added: Timestamp,
    #[serde(rename = "category_id", default)]
    pub category_id: FlexInt,
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
    #[serde(rename = "name", default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(rename = "stream_id", default)]
    pub stream_id: FlexInt,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The body of `get_series_info`. `BTreeMap` keeps season keys sorted, which is what Go's
/// `encoding/json` did for its `map[string][]SeriesEpisode`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Series {
    #[serde(rename = "episodes", default, deserialize_with = "null_to_default")]
    pub episodes: BTreeMap<String, Vec<SeriesEpisode>>,
    #[serde(rename = "info", default, deserialize_with = "null_to_default")]
    pub info: SeriesInfo,
    #[serde(rename = "seasons", default, deserialize_with = "null_to_default")]
    pub seasons: Vec<Value>,
}

impl Series {
    /// Total episode count across all seasons.
    pub fn episode_count(&self) -> usize {
        self.episodes.values().map(Vec::len).sum()
    }
}

/// A list that some providers send as a single string. `null` stays `null`, like Go's nil slice.
fn opt_lenient_strings<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        One(String),
        Many(Vec<String>),
        Other(Value),
    }
    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => None,
        Some(Raw::One(one)) => Some(vec![one]),
        Some(Raw::Many(many)) => Some(many),
        Some(Raw::Other(other)) => Some(vec![other.to_string()]),
    })
}

/// One EPG listing. `title` and `description` stay exactly as the upstream sent them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpgInfo {
    #[serde(rename = "channel_id", default, deserialize_with = "lenient_string")]
    pub channel_id: String,
    #[serde(rename = "description", default, deserialize_with = "lenient_string")]
    pub description: String,
    #[serde(rename = "end", default, deserialize_with = "lenient_string")]
    pub end: String,
    #[serde(rename = "epg_id", default)]
    pub epg_id: FlexInt,
    #[serde(rename = "has_archive", default)]
    pub has_archive: FlexBool,
    #[serde(rename = "id", default)]
    pub id: FlexInt,
    #[serde(rename = "lang", default, deserialize_with = "lenient_string")]
    pub lang: String,
    #[serde(rename = "now_playing", default)]
    pub now_playing: FlexBool,
    #[serde(rename = "start", default, deserialize_with = "lenient_string")]
    pub start: String,
    #[serde(
        rename = "start_timestamp",
        default = "go_zero_timestamp",
        deserialize_with = "epg_timestamp"
    )]
    pub start_timestamp: Timestamp,
    #[serde(
        rename = "stop_timestamp",
        default = "go_zero_timestamp",
        deserialize_with = "epg_timestamp"
    )]
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
    fn a_string_tag_does_not_quote_a_type_that_marshals_itself() {
        // Go's `,string` tag is ignored for FlexInt because the type has its own MarshalJSON, so
        // `category_id` leaves the Go proxy as a number even though the tag says otherwise.
        // Verified against the live Go release: {"category_id": 1011, ...}
        let category: Category =
            serde_json::from_str(r#"{"category_id":"123","category_name":"Sport"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&category).unwrap(),
            r#"{"category_id":123,"category_name":"Sport","parent_id":0}"#
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
            text.contains("\"category_id\":0"),
            "category_id is a number: {text}"
        );
        assert!(
            text.contains(&format!("\"added\":{GO_ZERO_TIME_SECONDS}")),
            "a missing added is Go's zero time, not null: {text}"
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
    fn flex_bool_round_trips_as_zero_or_one_and_keeps_quoting() {
        // Go's ConvertibleBoolean remembered whether the provider quoted the value and repeats it,
        // which is why `auth` leaves as 1 while `is_trial` leaves as "0" for this provider.
        assert!(serde_json::from_str::<FlexBool>("\"1\"").unwrap().get());
        assert_eq!(serde_json::to_string(&FlexBool::new(true)).unwrap(), "1");
        assert_eq!(serde_json::to_string(&FlexBool::new(false)).unwrap(), "0");
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<FlexBool>("\"0\"").unwrap()).unwrap(),
            "\"0\""
        );
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<FlexBool>("1").unwrap()).unwrap(),
            "1"
        );
    }

    #[test]
    fn timestamps_keep_the_provider_quoting() {
        let quoted: Timestamp = serde_json::from_str("\"1640708634\"").unwrap();
        assert_eq!(quoted.get(), 1_640_708_634);
        assert_eq!(serde_json::to_string(&quoted).unwrap(), "\"1640708634\"");
        let plain: Timestamp = serde_json::from_str("1640708634").unwrap();
        assert_eq!(serde_json::to_string(&plain).unwrap(), "1640708634");
        // `added` arrives quoted on this provider and the Go proxy echoed it quoted.
        let stream: Stream =
            serde_json::from_str(r#"{"stream_id":1,"added":"1640708634"}"#).unwrap();
        assert!(serde_json::to_string(&stream)
            .unwrap()
            .contains(r#""added":"1640708634""#));
    }

    #[test]
    fn timestamps_serialise_as_epoch_seconds() {
        let info: ServerInfo = serde_json::from_str(r#"{"timestamp_now":"1700000000"}"#).unwrap();
        assert_eq!(info.timestamp_now.get(), 1700000000);
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
        assert!(auth.user_info.auth.get());
        assert_eq!(auth.user_info.exp_date, None);
        let out = serde_json::to_string(&auth).unwrap();
        assert!(out.contains(r#""user_info""#));
        assert!(out.contains(r#""max_connections":1"#));
    }

    #[test]
    fn a_null_info_object_is_the_zero_info_object_like_go() {
        // Providers send `"info": null` for episodes without metadata. Go decoded that without a
        // word; refusing it made the whole `get_series_info` answer come back empty.
        let series: Series = serde_json::from_str(
            r#"{"episodes":{"1":[{"id":"401","info":null}]},"info":null,"seasons":null}"#,
        )
        .unwrap();
        assert_eq!(series.episode_count(), 1);
        assert_eq!(series.info.name, "");
        assert!(series.seasons.is_empty());
        let out = serde_json::to_string(&series).unwrap();
        assert!(out.contains(r#""info":{"audio":"#), "{out}");
    }

    #[test]
    fn epg_timestamps_without_a_value_keep_the_go_sentinel() {
        let listing: EpgInfo =
            serde_json::from_str(r#"{"channel_id":"a.epg","title":"VGVzdA"}"#).unwrap();
        assert_eq!(listing.start_timestamp.get(), GO_ZERO_TIME_SECONDS);
        assert_eq!(listing.stop_timestamp.get(), GO_ZERO_TIME_SECONDS);
        let out = serde_json::to_string(&listing).unwrap();
        assert!(out.contains(r#""start_timestamp":-62135596800"#), "{out}");

        let given: EpgInfo =
            serde_json::from_str(r#"{"start_timestamp":1700000000,"stop_timestamp":null}"#)
                .unwrap();
        assert_eq!(given.start_timestamp.get(), 1_700_000_000);
        assert_eq!(given.stop_timestamp.get(), GO_ZERO_TIME_SECONDS);
    }

    #[test]
    fn a_sparse_provider_still_gets_the_full_ffprobe_objects_the_go_release_invented() {
        // A real provider that leaves `audio` out entirely: the Go release answered with a full
        // ffprobe object of zero values, and apps read audio.channels out of it unconditionally.
        let info: EpisodeInfo = serde_json::from_str(r#"{"duration":"00:45:00"}"#).unwrap();
        let out = serde_json::to_string(&info.audio).unwrap();
        assert!(
            out.starts_with(r#"{"avg_frame_rate":"","bit_rate":"""#),
            "{out}"
        );
        assert!(out.contains(r#""channels":0"#), "{out}");
        assert!(out.contains(r#""tags":null,"disposition":{"#), "{out}");
        assert!(out.contains(r#""side_data_list":null}"#), "{out}");
        // All 53 ffprobe keys, in Go's declaration order (serde_json's preserve_order keeps it).
        let value: Value = serde_json::from_str(&out).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 53, "{keys:?}");
        assert_eq!(keys[0], "avg_frame_rate");
        assert_eq!(keys[8], "codec_name");
        assert_eq!(keys[49], "width");
        assert_eq!(
            &keys[50..],
            &["tags", "disposition", "side_data_list"][..],
            "{keys:?}"
        );

        // A provider that sends its own shape keeps it, and adds what it left out.
        let given: EpisodeInfo =
            serde_json::from_str(r#"{"audio":{"channels":"2","codec_name":"aac","hd":"yes"}}"#)
                .unwrap();
        let out = serde_json::to_string(&given.audio).unwrap();
        assert!(out.contains(r#""channels":2"#), "{out}");
        assert!(out.contains(r#""codec_name":"aac""#), "{out}");
        assert!(out.ends_with(r#""hd":"yes"}"#), "{out}");
    }

    #[test]
    fn vod_info_is_typed_the_way_the_go_release_typed_it() {
        let info: VodInfo = serde_json::from_str(
            r#"{"info":{"plot":"A film","rating":8.1,"backdrop_path":"one.png"},
                 "movie_data":{"name":"A film","stream_id":"42"},
                 "kids_info":{"a":1}}"#,
        )
        .unwrap();
        let out = serde_json::to_string(&info).unwrap();
        assert!(
            out.starts_with(r#"{"info":{"audio":{"avg_frame_rate":"""#),
            "{out}"
        );
        assert!(out.contains(r#""backdrop_path":["one.png"]"#), "{out}");
        assert!(out.contains(r#""rating":8.1"#), "{out}");
        assert!(
            out.contains(r#""added":-62135596800,"category_id":0"#),
            "{out}"
        );
        assert!(out.contains(r#""stream_id":42"#), "{out}");
        // Something Go threw away is still there.
        assert!(out.contains(r#""kids_info":{"a":1}"#), "{out}");
    }
}
