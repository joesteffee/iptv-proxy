//! IPTV proxy library.
//!
//! The binary in `src/main.rs` is a thin wrapper around this crate: everything else lives behind a
//! module boundary so the integration tests in `tests/` can drive the same pieces the server does.
//!
//! Module map:
//!
//! * [`config`] — flags and environment variables, identical surface to the Go release.
//! * [`m3u`] — M3U parse/serialise, byte compatible with the playlists clients already have.
//! * [`rewrite`] — turns a provider URL into a proxy URL.
//! * [`generate`] — builds a playlist from an Xtream provider.
//! * [`xtream`] — Xtream Codes API client and the wire models it re-exposes.
//! * [`cache`] — Redis payload cache, in-process API cache and the refresh worker.
//! * [`state`] — shared server state.
//! * [`http`] — outbound clients.
//! * [`handlers`] + [`routes`] — the HTTP surface.

// Handlers answer with the response they want to send, error included, so `Response` is the error
// type of a few helpers. Boxing it would only add an allocation per request to satisfy a lint.
#![allow(clippy::result_large_err)]

pub mod cache;
pub mod config;
pub mod generate;
pub mod handlers;
pub mod http;
pub mod m3u;
pub mod rewrite;
pub mod routes;
pub mod state;
pub mod url;
pub mod xtream;
