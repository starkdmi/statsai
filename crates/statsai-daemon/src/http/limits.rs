//! Bounds applied before a request reaches a StatsAI handler.

use std::time::Duration;

/// Maximum bytes in one request-line or header line, excluding the CRLF.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
/// Maximum bytes in one chunk-size line including extensions, excluding CRLF.
pub const MAX_CHUNK_METADATA_BYTES: usize = MAX_HEADER_LINE_BYTES;
/// Maximum bytes from the request line through the blank line, including CRLFs.
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
/// Maximum number of header fields. The request line is not counted.
pub const MAX_HEADERS: usize = 100;
/// Connections accepted into the server. Further connections are closed.
pub const MAX_ACTIVE_CONNECTIONS: usize = 32;
/// Parsed requests waiting for a handler.
pub const MAX_QUEUED_REQUESTS: usize = 32;
/// Absolute time allowed to finish reading one request's headers.
pub const HEADER_DEADLINE: Duration = Duration::from_secs(5);
/// Absolute time allowed to read one request body.
pub const BODY_DEADLINE: Duration = Duration::from_secs(30);
/// Absolute time allowed to write one response.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(30);

/// Limits for the vendored HTTP server.
///
/// [`HttpLimits::default`] is what the daemon and the OAuth callback use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    pub max_header_line_bytes: usize,
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub max_active_connections: usize,
    pub max_queued_requests: usize,
    pub header_deadline: Duration,
    pub body_deadline: Duration,
    pub write_deadline: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_header_line_bytes: MAX_HEADER_LINE_BYTES,
            max_header_bytes: MAX_HEADER_BYTES,
            max_headers: MAX_HEADERS,
            max_active_connections: MAX_ACTIVE_CONNECTIONS,
            max_queued_requests: MAX_QUEUED_REQUESTS,
            header_deadline: HEADER_DEADLINE,
            body_deadline: BODY_DEADLINE,
            write_deadline: WRITE_DEADLINE,
        }
    }
}
