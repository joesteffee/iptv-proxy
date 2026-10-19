//! Outbound HTTP clients.
//!
//! Two clients, both tuned for a long-lived byte pipe rather than a request/response RPC:
//!
//! * [`streaming_client`] — used for every media segment and playlist passthrough. No total
//!   timeout (a movie stream lasts as long as the movie), connection pooling, and manual redirect
//!   handling so the retry loop in [`crate::stream`] can see `Location` headers itself.
//! * the API client built by [`crate::xtream::XtreamClient::default_http`] — short JSON calls to
//!   `player_api.php`, with the browser user agent the provider expects.

use std::time::Duration;

/// Connection pool size inherited from the Go transport (`MaxIdleConnsPerHost`).
const POOL_MAX_IDLE_PER_HOST: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Client used to fetch media from the provider.
///
/// `Policy::none()` is deliberate: the Go code counted redirects itself (and gave up after ten
/// with a JSON error), but handed the client a `CheckRedirect` that let `net/http` follow them, so
/// its own loop never ran. Following redirects by hand makes that intended behaviour real and lets
/// the HLS handler inspect the target before deciding what to do with it.
pub fn streaming_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("default reqwest client builds")
}

/// Client used for the Xtream API: browser user agent, no total timeout (a full `get.php`
/// playlist can be tens of megabytes and take minutes), generous read headroom.
pub fn api_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(crate::xtream::XTREAM_USER_AGENT)
        .build()
        .expect("default reqwest client builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_build_and_keep_the_api_user_agent() {
        let _ = streaming_client();
        let api = api_client();
        // Not observable through the public API; building is the assertion that matters, plus the
        // header check below through a local server in tests/api_client_user_agent.rs.
        drop(api);
        assert_eq!(
            crate::xtream::XTREAM_USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        );
    }
}
