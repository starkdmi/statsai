use std::io::{self, ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use crate::http::tiny_http::connection::Connection;

pub(crate) fn remaining(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        None
    } else {
        Some(remaining)
    }
}

pub(crate) fn arm_read(socket: &Connection, deadline: Instant, message: &str) -> io::Result<()> {
    let Some(timeout) = remaining(deadline) else {
        return Err(io::Error::new(ErrorKind::TimedOut, message));
    };
    socket.set_read_timeout(Some(timeout))
}

pub(crate) fn arm_write(socket: &Connection, deadline: Instant, message: &str) -> io::Result<()> {
    let Some(timeout) = remaining(deadline) else {
        return Err(io::Error::new(ErrorKind::TimedOut, message));
    };
    socket.set_write_timeout(Some(timeout))
}

pub(crate) struct DeadlineRead<R> {
    pub(crate) inner: R,
    pub(crate) socket: Connection,
    pub(crate) deadline: Instant,
}

impl<R: Read> Read for DeadlineRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        arm_read(&self.socket, self.deadline, "body deadline exceeded")?;
        match self.inner.read(buf) {
            Err(error) if is_socket_deadline(&error) => Err(io::Error::new(
                ErrorKind::TimedOut,
                "body deadline exceeded",
            )),
            other => other,
        }
    }
}

pub(crate) struct DeadlineWrite<W> {
    pub(crate) inner: W,
    pub(crate) socket: Connection,
    pub(crate) deadline: Instant,
}

impl<W: Write> Write for DeadlineWrite<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        arm_write(&self.socket, self.deadline, "write deadline exceeded")?;
        match self.inner.write(buf) {
            Err(error) if is_socket_deadline(&error) => Err(io::Error::new(
                ErrorKind::TimedOut,
                "write deadline exceeded",
            )),
            other => other,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        arm_write(&self.socket, self.deadline, "write deadline exceeded")?;
        match self.inner.flush() {
            Err(error) if is_socket_deadline(&error) => Err(io::Error::new(
                ErrorKind::TimedOut,
                "write deadline exceeded",
            )),
            other => other,
        }
    }
}

fn is_socket_deadline(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}
