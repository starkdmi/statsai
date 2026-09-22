//! Bounded JSON decoding for HTTP response bodies.

use flate2::read::GzDecoder;
use serde::de::DeserializeOwned;
use std::io::{self, Read};
use thiserror::Error;

/// Health responses, including the daemon `/health` payload.
pub const JSON_RESPONSE_LIMIT_HEALTH: usize = 64 * 1024;
/// Authentication and device-session responses.
pub const JSON_RESPONSE_LIMIT_AUTH: usize = 1024 * 1024;
/// Sync acknowledgements, task feeds, and other JSON API payloads.
pub const JSON_RESPONSE_LIMIT_DEFAULT: usize = 10 * 1024 * 1024;

/// Failure while reading or parsing a size-limited JSON payload.
#[derive(Debug, Error)]
pub enum JsonLimitError {
    /// The decompressed payload is larger than the caller's limit.
    #[error("JSON response exceeds the {limit} byte limit")]
    TooLarge {
        /// Maximum accepted decompressed size.
        limit: usize,
    },
    /// The reader failed before a complete payload was available.
    #[error("read JSON response")]
    Read(#[source] io::Error),
    /// `serde_json` rejected the payload.
    #[error("parse JSON response")]
    Parse(#[source] serde_json::Error),
    /// The `Content-Encoding` value is not identity or gzip.
    #[error("unsupported JSON content encoding {encoding}")]
    UnsupportedEncoding {
        /// The header value that was rejected.
        encoding: String,
    },
}

/// Reads at most `limit` bytes and deserializes them as JSON.
///
/// One extra byte is read so a payload of exactly `limit + 1` is rejected
/// before deserialization.
pub fn read_json_limited<T, R>(reader: R, limit: usize) -> Result<T, JsonLimitError>
where
    T: DeserializeOwned,
    R: Read,
{
    read_encoded_json_limited(reader, None, limit)
}

/// Like [`read_json_limited`], counting bytes after gzip decompression.
///
/// `content_encoding` accepts `gzip`, `x-gzip`, `identity`, and an absent
/// header. The declared content length is ignored because the reader is the
/// already-decoded HTTP body.
pub fn read_encoded_json_limited<T, R>(
    reader: R,
    content_encoding: Option<&str>,
    limit: usize,
) -> Result<T, JsonLimitError>
where
    T: DeserializeOwned,
    R: Read,
{
    let bytes = read_limited_bytes(reader, content_encoding, limit)?;
    serde_json::from_slice(&bytes).map_err(JsonLimitError::Parse)
}

fn read_limited_bytes<R: Read>(
    reader: R,
    content_encoding: Option<&str>,
    limit: usize,
) -> Result<Vec<u8>, JsonLimitError> {
    let encoding = content_encoding.unwrap_or("identity").trim();
    let mut decoded: Box<dyn Read> = match encoding.to_ascii_lowercase().as_str() {
        "" | "identity" => Box::new(reader),
        "gzip" | "x-gzip" => Box::new(GzDecoder::new(reader)),
        _ => {
            return Err(JsonLimitError::UnsupportedEncoding {
                encoding: encoding.to_string(),
            })
        }
    };
    let mut buffer = Vec::new();
    decoded
        .by_ref()
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut buffer)
        .map_err(JsonLimitError::Read)?;
    if buffer.len() > limit {
        return Err(JsonLimitError::TooLarge { limit });
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{Cursor, Read, Write};

    #[test]
    fn exact_limit_parses_and_one_extra_byte_is_rejected() {
        let payload = br#"{"ok":true}"#;
        let parsed: serde_json::Value =
            read_json_limited(Cursor::new(payload), payload.len()).expect("exact");
        assert_eq!(parsed["ok"], true);
        let error =
            read_json_limited::<serde_json::Value, _>(Cursor::new(payload), payload.len() - 1)
                .expect_err("overflow");
        assert!(matches!(error, JsonLimitError::TooLarge { .. }));
    }

    #[test]
    fn gzip_expansion_is_counted_after_decompression() {
        let json = br#"{"token":"expanded"}"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json).expect("compress");
        let compressed = encoder.finish().expect("finish");
        assert!(!compressed.is_empty());
        let parsed: serde_json::Value =
            read_encoded_json_limited(Cursor::new(compressed.clone()), Some("gzip"), json.len())
                .expect("gzip within limit");
        assert_eq!(parsed["token"], "expanded");
        let error = read_encoded_json_limited::<serde_json::Value, _>(
            Cursor::new(compressed),
            Some("gzip"),
            json.len() - 1,
        )
        .expect_err("gzip overflow");
        assert!(matches!(error, JsonLimitError::TooLarge { .. }));
    }

    #[test]
    fn malformed_json_is_rejected_after_the_size_check() {
        let error = read_json_limited::<serde_json::Value, _>(Cursor::new(b"{"), 16)
            .expect_err("malformed");
        assert!(matches!(error, JsonLimitError::Parse(_)));
    }

    #[test]
    fn response_limit_defaults_match_the_security_bounds() {
        assert_eq!(JSON_RESPONSE_LIMIT_HEALTH, 64 * 1024);
        assert_eq!(JSON_RESPONSE_LIMIT_AUTH, 1024 * 1024);
        assert_eq!(JSON_RESPONSE_LIMIT_DEFAULT, 10 * 1024 * 1024);
    }

    #[test]
    fn chunked_gzip_and_misleading_lengths_are_bounded_on_the_wire() {
        let exact = r#"{"ok":true}"#;
        let response = fetch_bytes(&chunked_response(exact));
        let parsed: serde_json::Value =
            read_json_limited(response.into_reader(), exact.len()).expect("chunked exact");
        assert_eq!(parsed["ok"], true);

        let overflow = fetch_bytes(&chunked_response(exact));
        let error =
            read_json_limited::<serde_json::Value, _>(overflow.into_reader(), exact.len() - 1)
                .expect_err("chunked overflow");
        assert!(matches!(error, JsonLimitError::TooLarge { .. }));

        let json = br#"{"token":"expanded-through-http"}"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json).expect("compress");
        let compressed = encoder.finish().expect("finish");
        let within = fetch_bytes(&framed_response(
            &format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                compressed.len()
            ),
            &compressed,
        ));
        // ureq's default gzip feature decompresses and removes the header.
        // The helper still caps whichever bytes the HTTP client yields.
        let within_encoding = within.header("Content-Encoding").map(str::to_owned);
        let parsed: serde_json::Value =
            read_encoded_json_limited(within.into_reader(), within_encoding.as_deref(), json.len())
                .expect("gzip http");
        assert_eq!(parsed["token"], "expanded-through-http");

        let expanded = fetch_bytes(&framed_response(
            &format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                compressed.len()
            ),
            &compressed,
        ));
        let expanded_encoding = expanded.header("Content-Encoding").map(str::to_owned);
        let error = read_encoded_json_limited::<serde_json::Value, _>(
            expanded.into_reader(),
            expanded_encoding.as_deref(),
            json.len() - 1,
        )
        .expect_err("gzip http overflow");
        assert!(matches!(error, JsonLimitError::TooLarge { .. }));

        let misleading_body = vec![b'{'; 32];
        let mut misleading =
            b"HTTP/1.1 200 OK\r\nContent-Length: 4000000000\r\nConnection: close\r\n\r\n".to_vec();
        misleading.extend_from_slice(&misleading_body);
        let response = fetch_bytes(&misleading);
        let error = read_json_limited::<serde_json::Value, _>(response.into_reader(), 16)
            .expect_err("declared length must not raise the read cap");
        assert!(matches!(error, JsonLimitError::TooLarge { .. }));

        let malformed =
            fetch_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{");
        let error = read_json_limited::<serde_json::Value, _>(malformed.into_reader(), 16)
            .expect_err("malformed");
        assert!(matches!(error, JsonLimitError::Parse(_)));
    }

    fn chunked_response(payload: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{size:X}\r\n{payload}\r\n0\r\n\r\n",
            size = payload.len()
        )
        .into_bytes()
    }

    fn framed_response(header: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    fn fetch_bytes(response_bytes: &[u8]) -> ureq::Response {
        use std::net::TcpListener;
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let response_bytes = response_bytes.to_vec();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut seen = Vec::new();
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 {
                seen.push(byte[0]);
                if seen.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream.write_all(&response_bytes);
        });
        ureq::get(&format!("http://{address}/payload"))
            .timeout(std::time::Duration::from_secs(2))
            .call()
            .expect("response")
    }
}
