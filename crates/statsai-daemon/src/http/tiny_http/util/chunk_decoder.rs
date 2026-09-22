// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded HTTP chunked-body decoder.
//!
//! Adapted from rust-chunked-transfer 1.5.0 `Decoder`
//! (<https://github.com/frewsxcv/rust-chunked-transfer>). Original work
//! copyright The tiny-http Contributors and The rust-chunked-transfer
//! Contributors, licensed under MIT OR Apache-2.0. This copy parses
//! chunk sizes without buffering the size line or extensions.

use std::error::Error;
use std::fmt;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Result as IoResult;
use std::net::Shutdown;

use crate::http::limits::MAX_CHUNK_METADATA_BYTES;
use crate::http::tiny_http::connection::Connection;

/// Reads HTTP chunks and yields decoded payload bytes.
///
/// Chunk-size lines and chunk-ext data are counted against
/// [`MAX_CHUNK_METADATA_BYTES`] and never stored. Oversized or malformed
/// framing shuts the cloned connection down when one is provided.
pub struct ChunkDecoder<R> {
    source: R,
    remaining_chunks_size: Option<usize>,
    close_unread: Option<Connection>,
    finished: bool,
}

impl<R> ChunkDecoder<R> {
    fn shutdown_unread(&mut self) {
        if let Some(socket) = self.close_unread.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

impl<R> ChunkDecoder<R>
where
    R: Read,
{
    pub fn new(source: R) -> ChunkDecoder<R> {
        Self::with_unread_close(source, None)
    }

    pub fn with_unread_close(source: R, close_unread: Option<Connection>) -> ChunkDecoder<R> {
        ChunkDecoder {
            source,
            remaining_chunks_size: None,
            close_unread,
            finished: false,
        }
    }

    fn read_chunk_size(&mut self) -> IoResult<usize> {
        let mut size: u64 = 0;
        let mut saw_hex = false;
        let mut hex_done = false;
        let mut in_ext = false;
        let mut metadata_bytes = 0usize;

        loop {
            let byte = self.read_byte()?;
            if byte == b'\r' {
                break;
            }

            metadata_bytes += 1;
            if metadata_bytes > MAX_CHUNK_METADATA_BYTES {
                return self.fail_invalid();
            }

            if in_ext {
                continue;
            }

            if let Some(digit) = hex_value(byte) {
                if hex_done {
                    return self.fail_invalid();
                }
                size = match size
                    .checked_mul(16)
                    .and_then(|value| value.checked_add(u64::from(digit)))
                {
                    Some(value) => value,
                    None => return self.fail_invalid(),
                };
                saw_hex = true;
                continue;
            }

            match byte {
                b' ' | b'\t' if saw_hex => hex_done = true,
                b' ' | b'\t' if !saw_hex => {}
                b';' if saw_hex => in_ext = true,
                _ => return self.fail_invalid(),
            }
        }

        self.read_line_feed()?;

        if !saw_hex {
            return self.fail_invalid();
        }

        usize::try_from(size).or_else(|_| self.fail_invalid())
    }

    fn read_byte(&mut self) -> IoResult<u8> {
        loop {
            let mut buf = [0_u8; 1];
            match self.source.read(&mut buf) {
                Ok(0) => return self.fail_invalid(),
                Ok(_) => return Ok(buf[0]),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    self.shutdown_unread();
                    return Err(err);
                }
            }
        }
    }

    fn read_carriage_return(&mut self) -> IoResult<()> {
        match self.read_byte()? {
            b'\r' => Ok(()),
            _ => self.fail_invalid(),
        }
    }

    fn read_line_feed(&mut self) -> IoResult<()> {
        match self.read_byte()? {
            b'\n' => Ok(()),
            _ => self.fail_invalid(),
        }
    }

    fn fail_invalid<T>(&mut self) -> IoResult<T> {
        self.shutdown_unread();
        Err(IoError::new(ErrorKind::InvalidInput, DecoderError))
    }

    fn read_chunk_payload(&mut self, buf: &mut [u8], remaining: usize) -> IoResult<usize> {
        if buf.is_empty() {
            self.remaining_chunks_size = Some(remaining);
            return Ok(0);
        }
        let read = self.source.read(buf)?;
        if read == 0 {
            return self.fail_invalid();
        }
        Ok(read)
    }
}

impl<R> Read for ChunkDecoder<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.finished {
            return Ok(0);
        }

        let remaining_chunks_size = match self.remaining_chunks_size {
            Some(c) => c,
            None => {
                let chunk_size = self.read_chunk_size()?;
                if chunk_size == 0 {
                    self.read_carriage_return()?;
                    self.read_line_feed()?;
                    self.finished = true;
                    return Ok(0);
                }
                chunk_size
            }
        };

        if buf.len() < remaining_chunks_size {
            let read = self.read_chunk_payload(buf, remaining_chunks_size)?;
            self.remaining_chunks_size = Some(remaining_chunks_size - read);
            return Ok(read);
        }

        let buf = &mut buf[..remaining_chunks_size];
        let read = self.read_chunk_payload(buf, remaining_chunks_size)?;

        self.remaining_chunks_size = if read == remaining_chunks_size {
            self.read_carriage_return()?;
            self.read_line_feed()?;
            None
        } else {
            Some(remaining_chunks_size - read)
        };

        Ok(read)
    }
}

impl<R> Drop for ChunkDecoder<R> {
    fn drop(&mut self) {
        if !self.finished {
            self.shutdown_unread();
        }
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Copy, Clone)]
struct DecoderError;

impl fmt::Display for DecoderError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(fmt, "invalid chunked transfer framing")
    }
}

impl Error for DecoderError {}

#[cfg(test)]
mod test {
    use super::{ChunkDecoder, MAX_CHUNK_METADATA_BYTES};
    use crate::http::limits::MAX_CHUNK_METADATA_BYTES as LIMITS_MAX_CHUNK_METADATA_BYTES;
    use std::io;
    use std::io::Read;

    fn read_size(s: &str) -> io::Result<usize> {
        ChunkDecoder::new(s.as_bytes()).read_chunk_size()
    }

    #[test]
    fn metadata_cap_matches_the_http_header_line_cap() {
        assert_eq!(MAX_CHUNK_METADATA_BYTES, 8 * 1024);
        assert_eq!(MAX_CHUNK_METADATA_BYTES, LIMITS_MAX_CHUNK_METADATA_BYTES);
    }

    /// Taken from Hyper via rust-chunked-transfer 1.5.0.
    #[test]
    fn test_read_chunk_size() {
        fn read(s: &str, expected: usize) {
            assert_eq!(expected, read_size(s).unwrap());
        }

        fn read_err(s: &str) {
            assert_eq!(
                read_size(s).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }

        read("1\r\n", 1);
        read("01\r\n", 1);
        read("0\r\n", 0);
        read("00\r\n", 0);
        read("A\r\n", 10);
        read("a\r\n", 10);
        read("Ff\r\n", 255);
        read("Ff   \r\n", 255);
        read_err("F\rF");
        read_err("F");
        read_err("X\r\n");
        read_err("1X\r\n");
        read_err("-\r\n");
        read_err("-1\r\n");
        read("1;extension\r\n", 1);
        read("a;ext name=value\r\n", 10);
        read("1;extension;extension2\r\n", 1);
        read("1;;;  ;\r\n", 1);
        read("2; extension...\r\n", 2);
        read("3   ; extension=123\r\n", 3);
        read("3   ;\r\n", 3);
        read("3   ;   \r\n", 3);
        read_err("1 invalid extension\r\n");
        read_err("1 A\r\n");
        read_err("1;no CRLF");
    }

    #[test]
    fn chunk_size_at_the_metadata_cap_is_accepted() {
        let mut line = vec![b'2'];
        line.extend(std::iter::repeat_n(b' ', MAX_CHUNK_METADATA_BYTES - 1));
        line.extend_from_slice(b"\r\n");
        assert_eq!(
            ChunkDecoder::new(line.as_slice())
                .read_chunk_size()
                .unwrap(),
            2
        );
    }

    #[test]
    fn oversized_chunk_size_line_is_rejected() {
        let mut line = vec![b'2'];
        line.extend(std::iter::repeat_n(b' ', MAX_CHUNK_METADATA_BYTES));
        line.extend_from_slice(b"\r\n{}\r\n0\r\n\r\n");
        let mut decoder = ChunkDecoder::new(line.as_slice());
        let mut body = Vec::new();
        assert_eq!(
            decoder.read_to_end(&mut body).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(body.is_empty());
    }

    #[test]
    fn oversized_chunk_extension_is_rejected() {
        let mut line = b"1;".to_vec();
        line.extend(std::iter::repeat_n(b'x', MAX_CHUNK_METADATA_BYTES - 1));
        line.extend_from_slice(b"\r\n");
        assert_eq!(
            ChunkDecoder::new(line.as_slice())
                .read_chunk_size()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn unfinished_chunk_size_line_is_rejected() {
        let mut decoder = ChunkDecoder::new(b"2".as_slice());
        let mut body = Vec::new();
        assert_eq!(
            decoder.read_to_end(&mut body).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn numeric_overflow_is_rejected() {
        // 2^64 does not fit in u64. Leading zeros that fit still parse.
        assert_eq!(
            read_size("10000000000000000\r\n").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(read_size("00000000000000001\r\n").unwrap(), 1);
        match usize::try_from(u64::MAX) {
            Ok(max) => assert_eq!(read_size("ffffffffffffffff\r\n").unwrap(), max),
            Err(_) => assert!(read_size("ffffffffffffffff\r\n").is_err()),
        }
    }

    #[test]
    fn test_valid_chunk_decode() {
        let source = io::Cursor::new(b"3\r\nhel\r\nb\r\nlo world!!!\r\n0\r\n\r\n".to_vec());
        let mut decoded = ChunkDecoder::new(source);
        let mut string = String::new();
        decoded.read_to_string(&mut string).unwrap();
        assert_eq!(string, "hello world!!!");
    }

    #[test]
    fn test_decode_zero_length() {
        let mut decoder = ChunkDecoder::new(b"0\r\n\r\n".as_slice());
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, "");
    }

    #[test]
    fn test_decode_invalid_chunk_length() {
        let mut decoder = ChunkDecoder::new(b"m\r\n\r\n".as_slice());
        let mut decoded = String::new();
        assert!(decoder.read_to_string(&mut decoded).is_err());
    }

    #[test]
    fn invalid_input1() {
        let source = io::Cursor::new(b"2\r\nhel\r\nb\r\nlo world!!!\r\n0\r\n".to_vec());
        let mut decoded = ChunkDecoder::new(source);
        let mut string = String::new();
        assert!(decoded.read_to_string(&mut string).is_err());
    }

    #[test]
    fn invalid_input2() {
        let source = io::Cursor::new(b"3\rhel\r\nb\r\nlo world!!!\r\n0\r\n".to_vec());
        let mut decoded = ChunkDecoder::new(source);
        let mut string = String::new();
        assert!(decoded.read_to_string(&mut string).is_err());
    }

    #[test]
    fn json_object_chunk_decodes() {
        let mut decoder = ChunkDecoder::new(b"2\r\n{}\r\n0\r\n\r\n".as_slice());
        let mut body = String::new();
        decoder.read_to_string(&mut body).unwrap();
        assert_eq!(body, "{}");
    }

    #[test]
    fn premature_eof_after_partial_chunk_payload_is_rejected() {
        let mut decoder = ChunkDecoder::new(b"3\r\n{}".as_slice());
        let mut body = Vec::new();
        assert_eq!(
            decoder.read_to_end(&mut body).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn premature_eof_on_a_short_read_buffer_is_rejected() {
        let mut decoder = ChunkDecoder::new(b"3\r\n{".as_slice());
        let mut byte = [0_u8; 1];
        assert_eq!(decoder.read(&mut byte).unwrap(), 1);
        assert_eq!(byte[0], b'{');
        assert_eq!(
            decoder.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn empty_read_buffer_does_not_finish_a_chunk() {
        let mut decoder = ChunkDecoder::new(b"3\r\nhel\r\n0\r\n\r\n".as_slice());
        assert_eq!(decoder.read(&mut []).unwrap(), 0);
        let mut body = String::new();
        decoder.read_to_string(&mut body).unwrap();
        assert_eq!(body, "hel");
    }
}
