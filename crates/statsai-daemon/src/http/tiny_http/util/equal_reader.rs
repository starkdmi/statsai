use std::io::Read;
use std::io::Result as IoResult;
use std::net::Shutdown;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

use crate::http::tiny_http::connection::Connection;

const DISCARD_BUFFER_BYTES: usize = 8192;

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, remaining bytes are either discarded
/// through a fixed-size buffer or the connection is shut down. The declared
/// remainder is never used as an allocation size.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
    close_unread: Option<Connection>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        Self::with_unread_close(reader, size, None)
    }

    pub fn with_unread_close(
        reader: R,
        size: usize,
        close_unread: Option<Connection>,
    ) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
            close_unread,
        };

        (r, rx)
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    fn drop(&mut self) {
        if self.size == 0 {
            return;
        }

        // Rejected and abandoned bodies close the connection instead of
        // draining a client-declared remainder that can be gigabytes.
        if let Some(socket) = self.close_unread.take() {
            let _ = socket.shutdown(Shutdown::Both);
            self.size = 0;
            let _ = self.last_read_signal.send(Ok(()));
            return;
        }

        let mut remaining_to_read = self.size;
        self.size = 0;
        let mut buf = [0_u8; DISCARD_BUFFER_BYTES];
        while remaining_to_read > 0 {
            let chunk = remaining_to_read.min(buf.len());
            match self.reader.read(&mut buf[..chunk]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
                    break;
                }
                Ok(other) => {
                    remaining_to_read -= other;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EqualReader, DISCARD_BUFFER_BYTES};
    use std::io::Read;

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn drop_never_allocates_the_declared_remainder() {
        struct BoundedRead;
        impl Read for BoundedRead {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    buf.len() <= DISCARD_BUFFER_BYTES,
                    "discard buffer grew to {}",
                    buf.len()
                );
                Ok(0)
            }
        }

        let (reader, _) = EqualReader::new(BoundedRead, usize::MAX / 2);
        drop(reader);
    }
}
