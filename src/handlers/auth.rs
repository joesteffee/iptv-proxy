//! Client authentication, mirroring the `authenticate` / `appAuthenticate` gin middlewares.

use crate::config::ProxyConfig;
use crate::handlers::{error, Rsp};
use crate::url::parse_query;
use axum::http::{header, HeaderMap, StatusCode, Uri};

/// Collects the `username` / `password` a client supplied.
///
/// Query parameters come first, then — for a url-encoded POST body — the form fields, which is how
/// `ParseForm` merged them. A client that sends the credentials in the query always wins.
pub fn credentials(
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> (Option<String>, Option<String>) {
    let mut pairs: Vec<(String, String)> = uri.query().map(parse_query).unwrap_or_default();

    if is_form(headers) {
        pairs.extend(parse_query(String::from_utf8_lossy(body).as_ref()));
    }

    (lookup(&pairs, "username"), lookup(&pairs, "password"))
}

/// Verifies client credentials taken from the query string (or a url-encoded form body).
pub fn authenticate(
    config: &ProxyConfig,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), Rsp> {
    let (username, password) = credentials(uri, headers, body);

    let Some(username) = username else {
        tracing::debug!("missing username");
        return Err(error(
            StatusCode::BAD_REQUEST,
            "username or password missing",
        ));
    };
    let Some(password) = password else {
        tracing::debug!("missing password");
        return Err(error(
            StatusCode::BAD_REQUEST,
            "username or password missing",
        ));
    };

    if username != config.user.as_str() || password != config.password.as_str() {
        return Err(error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    Ok(())
}

/// Verifies the credentials an IPTV app posts inside the `player_api.php` body.
pub fn app_authenticate(config: &ProxyConfig, body: &[u8]) -> Result<(), Rsp> {
    let pairs = parse_query(String::from_utf8_lossy(body).as_ref());
    let username = lookup(&pairs, "username");
    let password = lookup(&pairs, "password");

    let (Some(username), Some(password)) = (username, password) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "bad body url query parameters",
        ));
    };

    if username != config.user.as_str() || password != config.password.as_str() {
        return Err(error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    Ok(())
}

fn lookup(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().unwrap_or(value).trim() == "application/x-www-form-urlencoded"
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FLAG_SPECS;
    use axum::http::HeaderValue;

    fn config(user: &str, password: &str) -> ProxyConfig {
        let mut raw = vec![
            "--user".to_string(),
            user.to_string(),
            "--password".to_string(),
            password.to_string(),
        ];
        for spec in FLAG_SPECS {
            if !spec.default.is_empty() && !raw.contains(&format!("--{}", spec.name)) {
                raw.push(format!("--{}", spec.name));
                raw.push(spec.default.to_string());
            }
        }
        crate::config::build(&raw).expect("config builds")
    }

    fn headers(content_type: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn query_credentials_are_accepted() {
        let config = config("admin", "s3cret");
        let uri = Uri::from_static("http://x/get.php?username=admin&password=s3cret");
        assert!(authenticate(&config, &uri, &headers(None), &[]).is_ok());
    }

    #[test]
    fn wrong_credentials_are_rejected_with_401() {
        let config = config("admin", "s3cret");
        let uri = Uri::from_static("http://x/get.php?username=admin&password=nope");
        let response = authenticate(&config, &uri, &headers(None), &[]).unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn missing_credentials_are_rejected_with_400() {
        let config = config("admin", "s3cret");
        let uri = Uri::from_static("http://x/get.php?username=admin");
        let response = authenticate(&config, &uri, &headers(None), &[]).unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn form_body_credentials_work_and_query_wins() {
        let config = config("admin", "s3cret");
        let uri = Uri::from_static("http://x/player_api.php");
        let form = headers(Some("application/x-www-form-urlencoded; charset=utf-8"));
        assert!(authenticate(&config, &uri, &form, b"username=admin&password=s3cret").is_ok());

        let uri = Uri::from_static("http://x/p?username=admin");
        assert!(
            authenticate(
                &config,
                &uri,
                &headers(None),
                b"username=admin&password=s3cret"
            )
            .is_err(),
            "a non-form body must be ignored, matching gin's Bind on GET-ish paths"
        );
    }

    #[test]
    fn app_authenticates_from_the_body() {
        let config = config("admin", "s3cret");
        assert!(app_authenticate(&config, b"username=admin&password=s3cret").is_ok());
        assert_eq!(
            app_authenticate(&config, b"foo=bar").unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app_authenticate(&config, b"username=x&password=y")
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
