//! Runtime configuration.
//!
//! The Go version read its configuration through `viper`, which resolved every setting in this
//! order: explicit CLI flag, then environment variable, then config file, then flag default.
//! Two properties of that resolution are reproduced here because live deployments depend on them:
//!
//! 1. Environment names are the flag name upper-cased with `-` replaced by `_`
//!    (`m3u-url` -> `M3U_URL`).
//! 2. An environment variable that is set but **empty** counts as unset (viper's
//!    `allowEmptyEnv = false`), so `USER=` falls back to the `usertest` default instead of
//!    authenticating against an empty username.
//!
//! The YAML config file is deliberately not ported. The Go build registered
//! `--iptv-proxy-config` with the literal default `"C"`, so `viper.SetConfigFile("C")` always ran
//! and `ReadInConfig()` always failed: no released image ever read a config file. The flag is
//! still accepted here (and ignored) so existing invocations keep starting.

use std::env;
use std::fmt;
use std::path::PathBuf;

/// Credentials that end up inside URLs. [`CredentialString::path_escape`] is the only transformation
/// applied to them when they are embedded in a generated playlist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialString(String);

impl CredentialString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Percent-encodes the credential the way Go's `url.PathEscape` does: everything outside the
    /// RFC 3986 *unreserved* set plus `$&+,/;:=?@` is escaped. `/` is escaped, which is what keeps
    /// a credential containing a slash from creating extra path segments.
    pub fn path_escape(&self) -> String {
        let mut out = String::with_capacity(self.0.len());
        for byte in self.0.as_bytes() {
            if !is_safe_path_byte(*byte) {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            } else {
                out.push(*byte as char);
            }
        }
        out
    }
}

impl fmt::Display for CredentialString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mirrors Go's `shouldEscape(c, encodePathSegment=false)`: `-_.~` and alphanumerics are kept, the
/// sub-delims plus `$&+,/;:=?@` are kept, everything else (including `/`, `?`, `#`) is escaped.
fn is_safe_path_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~')
        || matches!(byte, b'$' | b'&' | b'+' | b',' | b';' | b'=' | b':' | b'@')
}

/// Host part used when rewriting playlist URLs.
#[derive(Clone, Debug, Default)]
pub struct HostConfiguration {
    pub hostname: String,
    pub port: u16,
}

/// Everything the server needs to run. Field names mirror `pkg/config/config.go` one to one.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub host_config: HostConfiguration,
    pub remote_url: String,
    pub xtream_user: CredentialString,
    pub xtream_password: CredentialString,
    pub xtream_base_url: String,
    pub xtream_generate_api_get: bool,
    /// Hours.
    pub m3u_cache_expiration: u64,
    pub m3u_file_name: String,
    pub custom_endpoint: String,
    pub custom_id: String,
    pub advertised_port: u16,
    pub https: bool,
    pub user: CredentialString,
    pub password: CredentialString,
    /// Minutes a HTTP 458 keeps being retried before the error is surfaced.
    pub rate_limit_retry_timeout: u64,
    pub category_filters_path: PathBuf,
    pub redis_url: String,
}

impl ProxyConfig {
    /// True when an Xtream Codes provider was configured (manually or auto-detected).
    pub fn xtream_enabled(&self) -> bool {
        !self.xtream_base_url.is_empty()
    }

    /// `http` / `https`, chosen by [`ProxyConfig::https`].
    pub fn protocol(&self) -> &'static str {
        if self.https {
            "https"
        } else {
            "http"
        }
    }

    /// `{scheme}://{hostname}:{advertised_port}` — the base of every URL handed to clients.
    pub fn advertised_base(&self) -> String {
        format!(
            "{}://{}:{}",
            self.protocol(),
            self.host_config.hostname,
            self.advertised_port
        )
    }
}

/// Environment name for a flag: `m3u-url` -> `M3U_URL`.
fn env_name(flag: &str) -> String {
    flag.to_uppercase().replace('-', "_")
}

/// Reads `flag` from the environment, treating an empty value as unset (see module docs).
fn from_env(flag: &str) -> Option<String> {
    match env::var(env_name(flag)) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// Errors produced while building a [`ProxyConfig`].
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A numeric setting held something that is not a number.
    InvalidNumber { flag: &'static str, value: String },
    /// `--m3u-url` (or `M3U_URL`) could not be parsed as a URL or local path.
    InvalidRemoteUrl(String),
    /// A flag was used without a value.
    MissingValue(String),
    /// A boolean flag was given an explicit value that is not true/false.
    InvalidBool { flag: &'static str, value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidNumber { flag, value } => {
                write!(
                    f,
                    "invalid number {value:?} for {flag} (use a whole number)"
                )
            }
            ConfigError::InvalidRemoteUrl(value) => {
                write!(f, "cannot parse m3u url or file {value:?}")
            }
            ConfigError::MissingValue(flag) => write!(f, "flag {flag} requires a value"),
            ConfigError::InvalidBool { flag, value } => {
                write!(
                    f,
                    "invalid boolean {value:?} for {flag} (use true or false)"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parses a numeric setting. Go used `cast.ToInt`, which yields 0 for junk; failing loudly is
/// better here because a 0 port silently listens on a random one.
fn parse_number<T: std::str::FromStr>(flag: &'static str, value: &str) -> Result<T, ConfigError> {
    value
        .trim()
        .parse::<T>()
        .map_err(|_| ConfigError::InvalidNumber {
            flag,
            value: value.to_string(),
        })
}

fn parse_bool(flag: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "t" | "true" | "yes" | "on" => Ok(true),
        "0" | "f" | "false" | "no" | "off" | "" => Ok(false),
        _ => Err(ConfigError::InvalidBool {
            flag,
            value: value.to_string(),
        }),
    }
}

/// One declarative description of every flag, so the resolver, `--help` and the tests all read the
/// same table. `kind` decides how the textual value is turned into a Rust value.
pub(crate) struct FlagSpec {
    pub(crate) name: &'static str,
    pub(crate) short: Option<char>,
    pub(crate) default: &'static str,
    pub(crate) help: &'static str,
    /// `true` for `--flag` with no value (a switch).
    pub(crate) is_switch: bool,
}

/// All flags accepted by the binary, in the order the Go version declared them.
pub(crate) const FLAG_SPECS: &[FlagSpec] = &[
    FlagSpec {
        name: "m3u-url",
        short: Some('u'),
        default: "",
        help: "Iptv m3u file or url e.g: \"http://example.com/iptv.m3u\"",
        is_switch: false,
    },
    FlagSpec {
        name: "m3u-file-name",
        short: None,
        default: "iptv.m3u",
        help: "Name of the new proxified m3u file e.g \"http://proxy.com/iptv.m3u\"",
        is_switch: false,
    },
    FlagSpec {
        name: "custom-endpoint",
        short: None,
        default: "",
        help: "Custom endpoint \"http://proxy.com/<custom-endpoint>/iptv.m3u\"",
        is_switch: false,
    },
    FlagSpec {
        name: "custom-id",
        short: None,
        default: "",
        help: "Custom anti-collision ID for each track \"http://proxy.com/<custom-id>/...\"",
        is_switch: false,
    },
    FlagSpec {
        name: "port",
        short: None,
        default: "8080",
        help: "Iptv-proxy listening port",
        is_switch: false,
    },
    FlagSpec {
        name: "advertised-port",
        short: None,
        default: "0",
        help: "Port to expose the IPTV file and xtream (by default, it's taking value from port) \
               useful to put behind a reverse proxy",
        is_switch: false,
    },
    FlagSpec {
        name: "hostname",
        short: None,
        default: "",
        help: "Hostname or IP to expose the IPTVs endpoints",
        is_switch: false,
    },
    FlagSpec {
        name: "https",
        short: None,
        default: "false",
        help: "Activate https for urls proxy",
        is_switch: true,
    },
    FlagSpec {
        name: "user",
        short: None,
        default: "usertest",
        help: "User auth to access proxy (m3u/xtream)",
        is_switch: false,
    },
    FlagSpec {
        name: "password",
        short: None,
        default: "passwordtest",
        help: "Password auth to access proxy (m3u/xtream)",
        is_switch: false,
    },
    FlagSpec {
        name: "xtream-user",
        short: None,
        default: "",
        help: "Xtream-code user login",
        is_switch: false,
    },
    FlagSpec {
        name: "xtream-password",
        short: None,
        default: "",
        help: "Xtream-code password login",
        is_switch: false,
    },
    FlagSpec {
        name: "xtream-base-url",
        short: None,
        default: "",
        help: "Xtream-code base url e.g(http://example.tv:8080)",
        is_switch: false,
    },
    FlagSpec {
        name: "m3u-cache-expiration",
        short: None,
        default: "1",
        help: "M3U cache expiration in hour",
        is_switch: false,
    },
    FlagSpec {
        name: "xtream-api-get",
        short: None,
        default: "false",
        help: "Generate get.php from xtream API instead of get.php original endpoint",
        is_switch: true,
    },
    FlagSpec {
        name: "rate-limit-retry-timeout",
        short: None,
        default: "10",
        help: "Rate limit retry timeout in minutes (default: 10, env: RATE_LIMIT_RETRY_TIMEOUT)",
        is_switch: false,
    },
    FlagSpec {
        name: "category-filters-path",
        short: None,
        default: "",
        help: "Path to category filters JSON file (default: /root/iptv/category_filters.json, \
               env: CATEGORY_FILTERS_PATH)",
        is_switch: false,
    },
    FlagSpec {
        name: "redis-url",
        short: None,
        default: "",
        help: "Redis connection URL for caching (e.g., redis://localhost:6379, env: REDIS_URL)",
        is_switch: false,
    },
];

/// Accepted for CLI compatibility with the Go build; the YAML config file it named was never read
/// (see the module documentation).
const DEPRECATED_FLAGS: &[&str] = &["iptv-proxy-config"];

/// Raw textual view of the configuration after flag + env resolution, before typed parsing.
type Resolved = Vec<(&'static str, String)>;

/// Resolves every flag to its effective textual value: explicit flag beats environment beats
/// default.
fn resolve(raw: &[String]) -> Result<Resolved, ConfigError> {
    let explicit = parse_cli(raw)?;
    Ok(FLAG_SPECS
        .iter()
        .map(|spec| {
            let value = explicit
                .iter()
                .find(|(name, _)| *name == spec.name)
                .map(|(_, value)| value.clone())
                .or_else(|| from_env(spec.name))
                .unwrap_or_else(|| spec.default.to_string());
            (spec.name, value)
        })
        .collect())
}

/// Minimal `--flag value` / `--flag=value` / `-u value` / `--switch` parser covering the exact CLI
/// surface the Go (cobra/pflag) binary accepted.
fn parse_cli(raw: &[String]) -> Result<Vec<(&'static str, String)>, ConfigError> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut index = 0;

    while index < raw.len() {
        let arg = raw[index].as_str();
        index += 1;

        if arg != "--help" && arg != "-h" && !arg.starts_with('-') {
            // Positional argument: cobra ignores extra positional args on the root command.
            continue;
        }
        if arg == "--help" || arg == "-h" {
            // Handled by the caller, which prints help and exits successfully.
            break;
        }

        let body = arg.trim_start_matches('-');
        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (body, None),
        };

        if DEPRECATED_FLAGS.contains(&name) {
            if inline.is_none() && index < raw.len() && !raw[index].starts_with('-') {
                index += 1;
            }
            continue;
        }

        // `-u value` and `-abc` (bundled switches) both address flags by their short letter.
        let spec = lookup_spec(name).ok_or(ConfigError::MissingValue(name.to_string()))?;

        let supplied_inline = inline.is_some();
        let value = if spec.is_switch {
            inline.unwrap_or_else(|| "true".to_string())
        } else {
            match inline {
                Some(value) => value,
                None => raw
                    .get(index)
                    .cloned()
                    .ok_or(ConfigError::MissingValue(format!("--{name}")))?,
            }
        };
        if !spec.is_switch && !supplied_inline {
            index += 1;
        }

        out.push((spec.name, value));
    }

    Ok(out)
}

/// Resolves a flag name or a single-letter short name to its spec.
fn lookup_spec(name: &str) -> Option<&'static FlagSpec> {
    FLAG_SPECS.iter().find(|spec| {
        spec.name == name || (name.chars().count() == 1 && spec.short == name.chars().next())
    })
}

/// Turned into `true` when `--help`/`-h` was passed.
pub fn help_requested(raw: &[String]) -> bool {
    raw.iter().any(|arg| arg == "--help" || arg == "-h")
}

/// Prints the flag table in a cobra-like layout.
pub fn help_text() -> String {
    let mut out = String::new();
    out.push_str("Reverse proxy on iptv m3u file and xtream codes server api\n\n");
    out.push_str("Usage:\n  iptv-proxy [flags]\n\nFlags:\n");

    let lines: Vec<(String, &FlagSpec)> = FLAG_SPECS
        .iter()
        .map(|spec| {
            let left = match spec.short {
                Some(short) => format!("-{short}, --{:<24}", spec.name),
                None => format!("    --{:<24}", spec.name),
            };
            (left, spec)
        })
        .collect();

    let width = lines.iter().map(|(left, _)| left.len()).max().unwrap_or(0);
    for (left, spec) in &lines {
        // Naming the variable on the same line is what an operator reading a compose file needs.
        out.push_str(&format!(
            "{left:<width$}  {} (env {})",
            spec.help,
            env_name(spec.name),
            width = width
        ));
        if !spec.is_switch && !spec.default.is_empty() {
            out.push_str(&format!(" [default: {}]", spec.default));
        }
        out.push('\n');
    }
    out.push_str(
        "\nEvery flag can be set through an environment variable built from its name in \
upper case with '_' separators, e.g. --m3u-url -> M3U_URL.\n",
    );
    out
}

/// Builds the effective configuration from `argv` (without the program name) and the process
/// environment.
pub fn build(raw: &[String]) -> Result<ProxyConfig, ConfigError> {
    let resolved = resolve(raw)?;
    let get = |name: &str| -> String {
        resolved
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };

    let remote_url = get("m3u-url");
    // The Go version refused to start when the value was not a parseable URL, but it also accepted
    // a local path (`/root/iptv/iptv.m3u`) because `url.Parse` happily treats that as an opaque
    // path. Only a genuinely broken value (control characters) is rejected here.
    if remote_url.contains(char::is_control) {
        return Err(ConfigError::InvalidRemoteUrl(remote_url));
    }

    let port: u16 = parse_number("port", &get("port"))?;
    let advertised_port: u16 = parse_number("advertised-port", &get("advertised-port"))?;
    let advertised_port = if advertised_port == 0 {
        port
    } else {
        advertised_port
    };

    let mut xtream_user = CredentialString::new(get("xtream-user"));
    let mut xtream_password = CredentialString::new(get("xtream-password"));
    let mut xtream_base_url = get("xtream-base-url");

    // Auto-detect an Xtream provider from a get.php playlist URL, exactly like cmd/root.go.
    let (query_user, query_password) = get_php_credentials(&remote_url);
    if xtream_base_url.is_empty()
        && xtream_password.as_str().is_empty()
        && xtream_user.as_str().is_empty()
        && !query_user.is_empty()
        && !query_password.is_empty()
    {
        xtream_user = CredentialString::new(query_user.clone());
        xtream_password = CredentialString::new(query_password.clone());
        xtream_base_url = format!(
            "{scheme}://{host}",
            scheme = url_scheme(&remote_url),
            host = url_authority(&remote_url)
        );
        tracing::info!(
            "[iptv-proxy] INFO: It's seams you are using an Xtream provider! xtream service \
                 enable with xtream base url: {:?} xtream username: {:?} xtream password: {:?}",
            xtream_base_url,
            xtream_user.as_str(),
            "***"
        );
    }

    let rate_limit_retry_timeout: u64 =
        parse_number("rate-limit-retry-timeout", &get("rate-limit-retry-timeout"))?;
    let rate_limit_retry_timeout = if rate_limit_retry_timeout == 0 {
        10
    } else {
        rate_limit_retry_timeout
    };

    let category_filters_path = {
        let raw = get("category-filters-path");
        if raw.is_empty() {
            PathBuf::from("/root/iptv/category_filters.json")
        } else {
            PathBuf::from(raw)
        }
    };

    Ok(ProxyConfig {
        host_config: HostConfiguration {
            hostname: get("hostname"),
            port,
        },
        remote_url,
        xtream_user,
        xtream_password,
        xtream_base_url,
        xtream_generate_api_get: parse_bool("xtream-api-get", &get("xtream-api-get"))?,
        m3u_cache_expiration: parse_number("m3u-cache-expiration", &get("m3u-cache-expiration"))?,
        m3u_file_name: get("m3u-file-name"),
        custom_endpoint: get("custom-endpoint"),
        custom_id: get("custom-id"),
        advertised_port,
        https: parse_bool("https", &get("https"))?,
        user: CredentialString::new(get("user")),
        password: CredentialString::new(get("password")),
        rate_limit_retry_timeout,
        category_filters_path,
        redis_url: get("redis-url"),
    })
}

/// `username` / `password` of a `get.php?...` playlist URL, as cmd/root.go extracted them.
fn get_php_credentials(remote_url: &str) -> (String, String) {
    if !remote_url.contains("/get.php") {
        return (String::new(), String::new());
    }
    let query = match remote_url.split_once('?') {
        Some((_, query)) => query,
        None => return (String::new(), String::new()),
    };
    let mut user = String::new();
    let mut password = String::new();
    for (key, value) in url::query_pairs(query) {
        match key.as_str() {
            "username" => user = value.to_string(),
            "password" => password = value.to_string(),
            _ => {}
        }
    }
    (user, password)
}

/// `scheme` of a URL string, defaulting to `http` the way Go's `url.Parse` leaves an empty scheme
/// for a bare path.
fn url_scheme(value: &str) -> &str {
    match value.split_once("://") {
        Some((scheme, _)) => scheme,
        None => "",
    }
}

/// `host:port` of a URL string (everything between `://` and the next `/`, `?` or `#`).
fn url_authority(value: &str) -> String {
    let rest = match value.split_once("://") {
        Some((_, rest)) => rest,
        None => value,
    };
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Tiny query-string helper that keeps this module free of a URL crate while matching Go's
/// `url.Values` decoding (percent-decoding, `+` as space).
mod url {
    /// Splits a query string into decoded `(key, value)` pairs.
    pub fn query_pairs(query: &str) -> Vec<(String, String)> {
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = match pair.split_once('=') {
                    Some((key, value)) => (key, value),
                    None => (pair, ""),
                };
                (decode(key), decode(value))
            })
            .collect()
    }

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(byte) => {
                            out.push(byte);
                            index += 3;
                        }
                        Err(_) => {
                            out.push(bytes[index]);
                            index += 1;
                        }
                    }
                }
                b'+' => {
                    out.push(b' ');
                    index += 1;
                }
                other => {
                    out.push(other);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

/// Every flag name, in declaration order. Used by `--help` and by the env-parity test.
/// Every flag the binary accepts, including `--help` and the deprecated ones that are accepted and
/// ignored for compatibility with existing stacks.
pub fn flag_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = FLAG_SPECS.iter().map(|spec| spec.name).collect();
    names.extend(DEPRECATED_FLAGS);
    names.push("help");
    names
}
