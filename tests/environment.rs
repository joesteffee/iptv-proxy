//! Environment-variable parity with the Go release.
//!
//! Every flag the Go build accepted (viper `AutomaticEnv` + `-`→`_` replacer) must still be
//! accepted here under the same name, with the same default and the same precedence, because
//! unRAID stacks and docker-compose files set these by hand.

use iptv_proxy::config::{build, flag_names, help_text, ConfigError};
use std::path::PathBuf;

/// Env vars are process-global, so every scenario takes the serial lock and clears the whole set
/// before applying its own.
fn with_environment<T>(vars: &[(&str, &str)], args: &[&str], run: impl FnOnce() -> T) -> T {
    let _guard = serial();
    for key in KNOWN_VARS {
        std::env::remove_var(key);
    }
    for (key, value) in vars {
        std::env::set_var(key, value);
    }

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            for key in KNOWN_VARS {
                std::env::remove_var(key);
            }
        }
    }
    let _restore = Restore;

    let _ = args;
    run()
}

const KNOWN_VARS: &[&str] = &[
    "M3U_URL",
    "M3U_FILE_NAME",
    "CUSTOM_ENDPOINT",
    "CUSTOM_ID",
    "PORT",
    "ADVERTISED_PORT",
    "HOSTNAME",
    "HTTPS",
    "USER",
    "PASSWORD",
    "XTREAM_USER",
    "XTREAM_PASSWORD",
    "XTREAM_BASE_URL",
    "M3U_CACHE_EXPIRATION",
    "XTREAM_API_GET",
    "RATE_LIMIT_RETRY_TIMEOUT",
    "CATEGORY_FILTERS_PATH",
    "REDIS_URL",
    "IPTV_PROXY_CONFIG",
];

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Another test in this binary holds it only if it panicked while holding it; recovering is
    // safe because every scenario resets the whole environment first.
    match LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn parse(args: &[&str]) -> iptv_proxy::config::ProxyConfig {
    let raw: Vec<String> = std::iter::once("iptv-proxy".to_string())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect();
    build(&raw).expect("configuration should parse")
}

#[test]
fn every_documented_environment_variable_is_read() {
    with_environment(
        &[
            (
                "M3U_URL",
                "http://provider.example/get.php?username=u&password=p",
            ),
            ("M3U_FILE_NAME", "subscription.m3u"),
            ("CUSTOM_ENDPOINT", "tv"),
            ("CUSTOM_ID", "abc123"),
            ("PORT", "9100"),
            ("ADVERTISED_PORT", "9443"),
            ("HOSTNAME", "tv.example"),
            ("HTTPS", "true"),
            ("USER", "streamuser"),
            ("PASSWORD", "streampass"),
            ("XTREAM_USER", "xu"),
            ("XTREAM_PASSWORD", "xp"),
            ("XTREAM_BASE_URL", "http://api.example"),
            ("M3U_CACHE_EXPIRATION", "12"),
            ("XTREAM_API_GET", "true"),
            ("RATE_LIMIT_RETRY_TIMEOUT", "3"),
            ("CATEGORY_FILTERS_PATH", "/data/filters.json"),
            ("REDIS_URL", "redis://cache:6379"),
        ],
        &[],
        || {
            let config = parse(&[]);
            assert_eq!(
                config.remote_url,
                "http://provider.example/get.php?username=u&password=p"
            );
            assert_eq!(config.m3u_file_name, "subscription.m3u");
            assert_eq!(config.custom_endpoint, "tv");
            assert_eq!(config.custom_id, "abc123");
            assert_eq!(config.host_config.port, 9100);
            assert_eq!(config.advertised_port, 9443);
            assert_eq!(config.host_config.hostname, "tv.example");
            assert!(config.https);
            assert_eq!(config.user.as_str(), "streamuser");
            assert_eq!(config.password.as_str(), "streampass");
            assert_eq!(config.xtream_user.as_str(), "xu");
            assert_eq!(config.xtream_password.as_str(), "xp");
            assert_eq!(config.xtream_base_url, "http://api.example");
            assert_eq!(config.m3u_cache_expiration, 12);
            assert!(config.xtream_generate_api_get);
            assert_eq!(config.rate_limit_retry_timeout, 3);
            assert_eq!(
                config.category_filters_path,
                PathBuf::from("/data/filters.json")
            );
            assert_eq!(config.redis_url, "redis://cache:6379");
        },
    );
}

#[test]
fn defaults_match_the_go_release() {
    with_environment(&[], &[], || {
        let config = parse(&[]);
        assert_eq!(config.host_config.port, 8080, "PORT default");
        assert_eq!(
            config.advertised_port, 8080,
            "ADVERTISED_PORT falls back to PORT"
        );
        assert_eq!(config.m3u_file_name, "iptv.m3u", "M3U_FILE_NAME default");
        assert_eq!(config.user.as_str(), "usertest", "USER default");
        assert_eq!(config.password.as_str(), "passwordtest", "PASSWORD default");
        assert_eq!(
            config.m3u_cache_expiration, 1,
            "M3U_CACHE_EXPIRATION default (hours)"
        );
        assert_eq!(
            config.rate_limit_retry_timeout, 10,
            "RATE_LIMIT_RETRY_TIMEOUT default (minutes)"
        );
        assert_eq!(
            config.category_filters_path,
            PathBuf::from("/root/iptv/category_filters.json"),
            "CATEGORY_FILTERS_PATH default"
        );
        assert!(!config.https, "HTTPS default");
        assert!(config.remote_url.is_empty(), "M3U_URL default");
        assert!(config.xtream_base_url.is_empty(), "XTREAM_BASE_URL default");
        assert!(config.redis_url.is_empty(), "REDIS_URL default");
        assert!(!config.xtream_generate_api_get, "XTREAM_API_GET default");
    });
}

#[test]
fn the_go_fallbacks_for_zero_values_are_kept() {
    with_environment(
        &[("ADVERTISED_PORT", "0"), ("RATE_LIMIT_RETRY_TIMEOUT", "0")],
        &[],
        || {
            let config = parse(&[]);
            assert_eq!(config.advertised_port, 8080, "ADVERTISED_PORT=0 means PORT");
            assert_eq!(
                config.rate_limit_retry_timeout, 10,
                "0 means the default 10"
            );
        },
    );
}

#[test]
fn an_explicit_flag_beats_the_environment_and_the_environment_beats_the_default() {
    with_environment(&[("PORT", "9100"), ("USER", "fromenv")], &[], || {
        assert_eq!(parse(&[]).host_config.port, 9100, "env over default");
        assert_eq!(
            parse(&["--port", "9200"]).host_config.port,
            9200,
            "flag over env, like viper"
        );
        assert_eq!(parse(&[]).user.as_str(), "fromenv");
        assert_eq!(parse(&["--user", "fromflag"]).user.as_str(), "fromflag");
    });
}

#[test]
fn an_empty_environment_variable_is_treated_as_unset() {
    // viper's AutomaticEnv only accepts an empty value when `AllowEmptyEnv` is on, and it is off.
    with_environment(&[("PORT", ""), ("HOSTNAME", "")], &[], || {
        let config = parse(&[]);
        assert_eq!(config.host_config.port, 8080, "empty PORT is unset");
        assert_eq!(
            config.host_config.hostname, "",
            "empty HOSTNAME is still the empty default"
        );
    });
}

#[test]
fn the_xtream_provider_is_detected_from_the_subscription_url() {
    with_environment(
        &[(
            "M3U_URL",
            "https://provider.example:554/get.php?username=abc&password=def&type=m3u_plus",
        )],
        &[],
        || {
            let config = parse(&[]);
            assert_eq!(config.xtream_user.as_str(), "abc");
            assert_eq!(config.xtream_password.as_str(), "def");
            assert_eq!(config.xtream_base_url, "https://provider.example:554");
        },
    );

    // Explicit Xtream settings are never overwritten by the detection.
    with_environment(
        &[
            (
                "M3U_URL",
                "http://provider.example/get.php?username=abc&password=def",
            ),
            ("XTREAM_BASE_URL", "http://other.example"),
            ("XTREAM_USER", "explicit"),
            ("XTREAM_PASSWORD", "secret"),
        ],
        &[],
        || {
            let config = parse(&[]);
            assert_eq!(config.xtream_base_url, "http://other.example");
            assert_eq!(config.xtream_user.as_str(), "explicit");
            assert_eq!(config.xtream_password.as_str(), "secret");
        },
    );
}

#[test]
fn the_deprecated_config_file_flag_is_accepted_and_ignored() {
    // Go always failed to load it, so a stack that still passes it must keep starting.
    with_environment(&[], &[], || {
        let config = parse(&["--iptv-proxy-config", "/etc/whatever.yaml"]);
        assert_eq!(config.host_config.port, 8080);
    });
}

#[test]
fn bad_values_are_reported_the_same_way() {
    with_environment(&[("PORT", "not-a-number")], &[], || {
        let raw = vec!["iptv-proxy".to_string()];
        let error = build(&raw).expect_err("a non-numeric port must fail");
        assert!(
            matches!(error, ConfigError::InvalidNumber { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("port"), "{error}");
    });

    with_environment(&[("HTTPS", "maybe")], &[], || {
        let raw = vec!["iptv-proxy".to_string()];
        let error = build(&raw).expect_err("a non-boolean HTTPS must fail");
        assert!(
            matches!(error, ConfigError::InvalidBool { .. }),
            "{error:?}"
        );
    });
}

#[test]
fn the_flag_set_matches_the_go_command() {
    // cmd/root.go registered exactly these flags (plus the deprecated config file).
    let mut got = flag_names();
    got.sort_unstable();
    let expected = [
        "advertised-port",
        "category-filters-path",
        "custom-endpoint",
        "custom-id",
        "help",
        "hostname",
        "https",
        "iptv-proxy-config",
        "m3u-cache-expiration",
        "m3u-file-name",
        "m3u-url",
        "password",
        "port",
        "rate-limit-retry-timeout",
        "redis-url",
        "user",
        "xtream-api-get",
        "xtream-base-url",
        "xtream-password",
        "xtream-user",
    ];
    assert_eq!(got, expected, "the flag surface must not change");
}

#[test]
fn the_help_text_lists_every_variable_a_operator_needs() {
    let text = help_text();
    for needle in [
        "M3U_URL",
        "PORT",
        "HOSTNAME",
        "USER",
        "PASSWORD",
        "XTREAM_BASE_URL",
        "M3U_CACHE_EXPIRATION",
        "REDIS_URL",
        "CATEGORY_FILTERS_PATH",
    ] {
        assert!(text.contains(needle), "{needle} missing from --help");
    }
}
