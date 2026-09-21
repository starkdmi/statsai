//! Shared HTTP server for the daemon and the OAuth loopback callback.
//!
//! The implementation is tiny_http 0.12.0, patched in this crate so `cargo install`
//! and packaged binaries enforce the same limits. Upstream licenses are
//! `src/http/tiny_http/LICENSE-MIT` and `LICENSE-APACHE`.
//! Upstream: <https://github.com/tiny-http/tiny-http/tree/0.12.0>.

mod limits;
#[allow(clippy::all, unused, missing_docs, clippy::pedantic, unexpected_cfgs)]
mod tiny_http;

#[cfg(test)]
mod tests;

pub use limits::{
    HttpLimits, BODY_DEADLINE, HEADER_DEADLINE, MAX_ACTIVE_CONNECTIONS, MAX_HEADERS,
    MAX_HEADER_BYTES, MAX_HEADER_LINE_BYTES, MAX_QUEUED_REQUESTS, WRITE_DEADLINE,
};
pub use tiny_http::{Header, ListenAddr, Method, Request, Response, Server, StatusCode};
