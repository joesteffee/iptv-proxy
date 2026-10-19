//! A very small URL type.
//!
//! `url` is not pulled in as a dependency: the proxy only needs to split URLs apart, splice new
//! credentials into the path and print them again. The behaviour deliberately mirrors
//! `net/url` in the two ways that matter for output compatibility:
//!
//! * the escaped path is preserved verbatim when it is re-printed (Go keeps it in `RawPath`), and
//! * a URL rebuilt without a query drops the query, exactly like `replaceURL` does.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    /// Raw user info as it appeared (`user`, or `user:pass`), percent-encoded.
    pub userinfo: Option<String>,
    pub host: String,
    /// Port including the leading colon, or empty.
    pub port: String,
    /// Escaped path, always starting with `/` when there is an authority.
    pub path: String,
    pub query: Option<String>,
}

impl Url {
    /// Parses an absolute URL. Errors mirror the cases `url.Parse` rejects: a missing scheme, an
    /// illegal character, or a non-numeric port.
    pub fn parse(input: &str) -> Result<Self, String> {
        for (index, byte) in input.bytes().enumerate() {
            if byte <= 0x20 || byte == 0x7f {
                return Err(format!(
                    "parse {input:?}: net/url: invalid control character in URL"
                ));
            }
            let _ = index;
        }

        let colon = input
            .find(':')
            .ok_or_else(|| format!("parse {input:?}: relative URL without a base"))?;

        let scheme = &input[..colon];
        if scheme.is_empty()
            || !scheme
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            || !scheme
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        {
            return Err(format!(
                "parse {input:?}: first path segment in URL cannot contain colon"
            ));
        }

        let rest = &input[colon + 1..];

        // `scheme://authority/path?query`; without the `//` the remainder is an opaque path.
        let (authority, path_and_query) = match rest.strip_prefix("//") {
            Some(after) => {
                let end = after.find(['/', '?', '#']).unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
            None => ("", rest),
        };

        let (userinfo, hostport) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };

        let (host, port) = split_host_port(hostport)?;

        let (path, query) = match path_and_query.find('?') {
            Some(q) => (
                &path_and_query[..q],
                (!path_and_query[q + 1..].is_empty()).then(|| path_and_query[q + 1..].to_string()),
            ),
            None => (path_and_query, None),
        };

        // Drop the fragment: nothing downstream uses it and it would leak into a request line.
        let path = match path.find('#') {
            Some(hash) => &path[..hash],
            None => path,
        };

        Ok(Url {
            scheme: scheme.to_ascii_lowercase(),
            userinfo: userinfo.filter(|u| !u.is_empty()).map(str::to_string),
            host: host.to_string(),
            port: port.to_string(),
            path: if path.is_empty() && !hostport.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            query,
        })
    }

    /// `host` without any port.
    pub fn host_only(&self) -> &str {
        &self.host
    }

    /// Last path segment, matching Go's `path.Base` for the shapes playlists contain.
    pub fn base(&self) -> &str {
        let trimmed = self.path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(slash) => &trimmed[slash + 1..],
            None => {
                if trimmed.is_empty() {
                    "/"
                } else {
                    trimmed
                }
            }
        }
    }

    /// Query pairs in source order, percent-decoded like `url.Values`.
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        match &self.query {
            Some(query) => parse_query(query),
            None => Vec::new(),
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme)?;
        if let Some(userinfo) = &self.userinfo {
            write!(f, "{userinfo}@")?;
        }
        write!(f, "{}{}", self.host, self.port)?;
        write!(f, "{}", self.path)?;
        if let Some(query) = &self.query {
            write!(f, "?{query}")?;
        }
        Ok(())
    }
}

fn split_host_port(hostport: &str) -> Result<(String, String), String> {
    if let Some(stripped) = hostport.strip_prefix('[') {
        let end = stripped
            .find(']')
            .ok_or_else(|| format!("parse {hostport:?}: missing ']' in host"))?;
        let host = &hostport[..end + 2]; // keep both brackets
        let rest = &stripped[end + 1..];
        let port = rest.strip_prefix(':').unwrap_or("");
        if !port.is_empty() && !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("parse {hostport:?}: invalid port {port:?}"));
        }
        return Ok((
            host.to_string(),
            if port.is_empty() {
                String::new()
            } else {
                format!(":{port}")
            },
        ));
    }

    match hostport.rfind(':') {
        Some(colon) => {
            let port = &hostport[colon + 1..];
            if port.is_empty() {
                return Ok((hostport[..colon].to_string(), String::new()));
            }
            if !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("parse {hostport:?}: invalid port {port:?}"));
            }
            Ok((hostport[..colon].to_string(), format!(":{port}")))
        }
        None => Ok((hostport.to_string(), String::new())),
    }
}

/// Splits a query string into decoded pairs, treating `+` as a space like `net/url` does.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// Percent-decodes a query component, mapping `+` to a space. Invalid escapes are kept verbatim,
/// which is what `net/url` does for query values it cannot decode.
pub fn decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() => {
                let value = hex(bytes[i + 1]).unwrap() * 16 + hex(bytes[i + 2]).unwrap();
                // Only emit the byte as UTF-8 when it forms a valid sequence on its own; otherwise
                // keep the escape so round-tripping stays lossless.
                let mut buf = [0u8; 4];
                buf[0] = value;
                if value & 0x80 == 0 {
                    out.push(buf[0] as char);
                } else {
                    out.push('%');
                    out.push(char::from(bytes[i + 1]));
                    out.push(char::from(bytes[i + 2]));
                }
                i += 3;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            _ => {
                // Copy the next run of plain bytes.
                let start = i;
                while i < bytes.len() && bytes[i] != b'%' && bytes[i] != b'+' {
                    i += 1;
                }
                out.push_str(&input[start..i]);
            }
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|value| value as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_urls() {
        let url = Url::parse("http://example.com:8080/live/u/p/12.ts?x=1").unwrap();
        assert_eq!(url.scheme, "http");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, ":8080");
        assert_eq!(url.path, "/live/u/p/12.ts");
        assert_eq!(url.query.as_deref(), Some("x=1"));
        assert_eq!(url.base(), "12.ts");
    }

    #[test]
    fn keeps_userinfo_and_ipv6_hosts() {
        let url = Url::parse("https://a:b@[::1]:9000/x").unwrap();
        assert_eq!(url.userinfo.as_deref(), Some("a:b"));
        assert_eq!(url.host, "[::1]");
        assert_eq!(url.port, ":9000");
        assert_eq!(url.to_string(), "https://a:b@[::1]:9000/x");
    }

    #[test]
    fn escaped_paths_survive_round_trips() {
        let url = Url::parse("http://h/a%20b/c%2Fd").unwrap();
        assert_eq!(url.path, "/a%20b/c%2Fd");
        assert_eq!(url.to_string(), "http://h/a%20b/c%2Fd");
    }

    #[test]
    fn rejects_the_cases_net_url_rejects() {
        assert!(Url::parse("not a url").is_err());
        assert!(Url::parse("http://host:port/x").is_err());
        assert!(Url::parse("/just/a/path").is_err());
    }

    #[test]
    fn base_matches_go_path_base() {
        assert_eq!(Url::parse("http://h/a/b/").unwrap().base(), "b");
        assert_eq!(Url::parse("http://h/").unwrap().base(), "/");
        assert_eq!(Url::parse("http://h/x.ts").unwrap().base(), "x.ts");
    }

    #[test]
    fn query_pairs_decode_plus_and_percent() {
        let url = Url::parse("http://h/?a=b%20c&d=e+f&flag=").unwrap();
        assert_eq!(
            url.query_pairs(),
            vec![
                ("a".to_string(), "b c".to_string()),
                ("d".to_string(), "e f".to_string()),
                ("flag".to_string(), String::new())
            ]
        );
    }
}
