//! iptv-proxy — put an Xtream Codes or M3U IPTV subscription behind your own host.
//!
//! The binary reads its whole configuration from flags and environment variables, builds the Axum
//! router for the mode it is in, and serves until SIGINT/SIGTERM.

use iptv_proxy::{config, generate, handlers, routes, state::AppState};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    if config::help_requested(&raw) {
        print!("{}", config::help_text());
        return;
    }

    let cfg = match config::build(&raw) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("iptv-proxy: {err}");
            eprintln!("run `iptv-proxy --help` for the list of options");
            std::process::exit(2);
        }
    };

    init_tracing();

    let state = match AppState::startup(cfg).await {
        Ok(state) => Arc::new(state),
        Err(err) => {
            tracing::error!("cannot start: {err:#}");
            std::process::exit(1);
        }
    };

    if let Err(err) = prepare(&state).await {
        tracing::error!("cannot start: {err:#}");
        std::process::exit(1);
    }

    let port = state.config.host_config.port;
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!("cannot listen on port {port}: {err}");
            std::process::exit(1);
        }
    };

    let provider = if state.config.xtream_base_url.is_empty() {
        String::new()
    } else {
        format!(" for {}", state.config.xtream_base_url)
    };
    tracing::info!(
        "listening on {port}{provider} as {} ({})",
        state.config.host_config.hostname,
        if state.config.xtream_enabled() {
            "xtream"
        } else {
            "m3u"
        }
    );

    if let Err(err) = axum::serve(listener, routes::router(Arc::clone(&state)))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {err}");
        std::process::exit(1);
    }

    tracing::info!("shutdown complete");
}

/// Loads the category filters, warms the playlist for plain-M3U mode, and starts the cache refresh
/// worker when Redis is configured.
async fn prepare(state: &Arc<AppState>) -> anyhow::Result<()> {
    // A missing filter file is normal (it is created the first time the web UI saves a selection).
    state
        .filter
        .load_from_file(&state.config.category_filters_path)?;

    // In xtream-auto mode the playlist is fetched per request, so there is nothing to warm here.
    if !routes::xtream_auto(&state.config) && !state.config.remote_url.is_empty() {
        match handlers::playlist::bootstrap(state).await {
            Ok(tracks) => tracing::info!("startup playlist contains {tracks} track(s)"),
            Err(err) => {
                // Keep serving: the Xtream routes work without it, and the file may appear later.
                tracing::error!("could not load the startup playlist: {err:#}");
            }
        }
    }

    if let Some(redis) = state.redis.clone() {
        let mut worker = iptv_proxy::cache::RefreshWorker::new(redis);
        generate::register_refresh_callbacks(&mut worker, state);
        // The handle stops the worker when dropped, so it has to live as long as the server.
        let _refresh = worker.start();
    }

    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .init();
}

/// Resolves on SIGINT or SIGTERM so `axum::serve` can drain in-flight streams.
async fn shutdown_signal() {
    let terminate = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                    tracing::info!("SIGTERM received");
                }
                Err(err) => tracing::warn!("cannot install SIGTERM handler: {err}"),
            }
        }
        tokio::signal::ctrl_c().await.ok();
    };

    terminate.await;
    tracing::info!("shutting down");
}
