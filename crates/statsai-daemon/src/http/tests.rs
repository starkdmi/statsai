use super::*;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn limits_for_tests() -> HttpLimits {
    HttpLimits {
        header_deadline: Duration::from_millis(400),
        body_deadline: Duration::from_millis(400),
        write_deadline: Duration::from_millis(400),
        ..HttpLimits::default()
    }
}

fn spawn_echo(limits: HttpLimits) -> std::net::SocketAddr {
    let server = Server::http_with_limits("127.0.0.1:0", limits).expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("expected a TCP listener");
    };
    thread::spawn(move || loop {
        match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(request)) => {
                let body = format!("{} {}", request.method(), request.url());
                let _ = request.respond(Response::from_string(body));
            }
            Ok(None) => {}
            Err(_) => break,
        }
    });
    addr
}

fn spawn_body_echo(limits: HttpLimits) -> std::net::SocketAddr {
    let server = Server::http_with_limits("127.0.0.1:0", limits).expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("expected a TCP listener");
    };
    thread::spawn(move || loop {
        match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(mut request)) => {
                let mut body = Vec::new();
                match request.as_reader().read_to_end(&mut body) {
                    Ok(_) => {
                        let _ = request.respond(Response::from_data(body));
                    }
                    Err(_) => drop(request),
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    });
    addr
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    stream
}

fn read_available(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if !buffer.is_empty() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::UnexpectedEof
                ) =>
            {
                // macOS can RST an unread excess connection instead of returning EOF.
                break;
            }
            Err(error) => panic!("read response: {error}"),
        }
    }
    buffer
}

fn write_or_closed(stream: &mut TcpStream, data: &[u8]) {
    match stream.write_all(data) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::UnexpectedEof
            ) => {}
        Err(error) => panic!("write request: {error}"),
    }
}

fn write_repeating_or_closed(stream: &mut TcpStream, byte: u8, count: usize) {
    let block = [byte; 4096];
    let mut remaining = count;
    while remaining > 0 {
        let n = remaining.min(block.len());
        match stream.write_all(&block[..n]) {
            Ok(()) => remaining -= n,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::BrokenPipe
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::UnexpectedEof
                ) =>
            {
                return;
            }
            Err(error) => panic!("write padding: {error}"),
        }
    }
}

fn recovered_body_echo(addr: std::net::SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if stream
                .write_all(
                    b"POST /recovered HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nping",
                )
                .is_ok()
            {
                last = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
                if last.contains("200") && last.contains("ping") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not recover after chunked framing rejection: {last}");
}

fn header_line(content_len: usize) -> Vec<u8> {
    let prefix = b"X-Pad: ";
    assert!(content_len >= prefix.len());
    let mut line = Vec::with_capacity(content_len + 2);
    line.extend_from_slice(prefix);
    line.extend(std::iter::repeat_n(b'a', content_len - prefix.len()));
    line.extend_from_slice(b"\r\n");
    line
}

#[test]
fn production_limits_match_the_security_bounds() {
    let limits = HttpLimits::default();
    assert_eq!(limits.max_header_line_bytes, 8 * 1024);
    assert_eq!(limits.max_header_bytes, 32 * 1024);
    assert_eq!(limits.max_headers, 100);
    assert_eq!(limits.max_active_connections, 32);
    assert_eq!(limits.max_queued_requests, 32);
    assert_eq!(limits.header_deadline, Duration::from_secs(5));
    assert_eq!(limits.body_deadline, Duration::from_secs(30));
    assert_eq!(limits.write_deadline, Duration::from_secs(30));
    assert_eq!(MAX_HEADER_LINE_BYTES, limits.max_header_line_bytes);
    assert_eq!(MAX_CHUNK_METADATA_BYTES, MAX_HEADER_LINE_BYTES);
    assert_eq!(MAX_HEADERS, limits.max_headers);
    assert_eq!(MAX_ACTIVE_CONNECTIONS, limits.max_active_connections);
    assert_eq!(MAX_QUEUED_REQUESTS, limits.max_queued_requests);
    assert_eq!(HEADER_DEADLINE, limits.header_deadline);
    assert_eq!(BODY_DEADLINE, limits.body_deadline);
    assert_eq!(WRITE_DEADLINE, limits.write_deadline);
    assert_eq!(MAX_HEADER_BYTES, limits.max_header_bytes);
}

#[test]
fn header_line_at_the_limit_is_accepted_and_one_extra_byte_is_rejected() {
    let addr = spawn_echo(limits_for_tests());
    let mut allowed = connect(addr);
    allowed.write_all(b"GET /exact-line HTTP/1.1\r\n").unwrap();
    allowed
        .write_all(&header_line(MAX_HEADER_LINE_BYTES))
        .unwrap();
    allowed.write_all(b"\r\n").unwrap();
    let response = read_available(&mut allowed);
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("200") && text.contains("GET /exact-line"),
        "{text}"
    );

    let mut rejected = connect(addr);
    rejected.write_all(b"GET /over-line HTTP/1.1\r\n").unwrap();
    rejected
        .write_all(&header_line(MAX_HEADER_LINE_BYTES + 1))
        .unwrap();
    rejected.write_all(b"\r\n").unwrap();
    let response = read_available(&mut rejected);
    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("431"), "{text}");
    assert_eq!(rejected.read(&mut [0; 8]).unwrap(), 0);
}

#[test]
fn total_header_block_limit_counts_every_wire_byte() {
    let addr = spawn_echo(limits_for_tests());
    let request_line = b"GET / HTTP/1.1\r\n";
    let blank = b"\r\n";
    let header_budget = MAX_HEADER_BYTES - request_line.len() - blank.len();
    let full = header_line(MAX_HEADER_LINE_BYTES);
    let mut headers = Vec::new();
    while headers.len() + full.len() <= header_budget {
        headers.extend_from_slice(&full);
    }
    let remainder = header_budget - headers.len();
    assert!(remainder >= 2);
    headers.extend_from_slice(&header_line(remainder - 2));
    assert_eq!(
        request_line.len() + headers.len() + blank.len(),
        MAX_HEADER_BYTES
    );

    let mut allowed = connect(addr);
    allowed.write_all(request_line).unwrap();
    allowed.write_all(&headers).unwrap();
    allowed.write_all(blank).unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut allowed)).into_owned();
    assert!(response.contains("200"), "{response}");

    headers.push(b'a');
    let mut rejected = connect(addr);
    rejected.write_all(request_line).unwrap();
    rejected.write_all(&headers).unwrap();
    rejected.write_all(blank).unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut rejected)).into_owned();
    assert!(response.contains("431"), "{response}");
}

#[test]
fn the_hundred_and_first_header_is_rejected() {
    let addr = spawn_echo(limits_for_tests());
    let mut request = b"GET /headers HTTP/1.1\r\n".to_vec();
    for index in 0..MAX_HEADERS {
        request.extend_from_slice(format!("X-{index}: v\r\n").as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    let mut allowed = connect(addr);
    allowed.write_all(&request).unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut allowed)).into_owned();
    assert!(response.contains("200"), "{response}");

    request.truncate(request.len() - 2);
    request.extend_from_slice(b"X-Extra: v\r\n\r\n");
    let mut rejected = connect(addr);
    rejected.write_all(&request).unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut rejected)).into_owned();
    assert!(response.contains("431"), "{response}");
}

#[test]
fn an_unfinished_header_hits_the_absolute_deadline() {
    let addr = spawn_echo(HttpLimits {
        header_deadline: Duration::from_millis(200),
        ..limits_for_tests()
    });
    let mut stream = connect(addr);
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n")
        .unwrap();
    let started = Instant::now();
    let response = read_available(&mut stream);
    assert!(started.elapsed() < Duration::from_secs(2));
    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("408"), "{text}");
}

#[test]
fn pipelined_requests_stay_outstanding_one_at_a_time() {
    let server = Server::http_with_limits("127.0.0.1:0", limits_for_tests()).expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    let server = std::sync::Arc::new(server);
    let handle = {
        let server = std::sync::Arc::clone(&server);
        thread::spawn(move || {
            let first = server.recv_timeout(Duration::from_secs(2)).expect("first");
            let first = first.expect("first request");
            assert!(server.try_recv().expect("poll").is_none());
            thread::sleep(Duration::from_millis(150));
            assert!(server.try_recv().expect("still one").is_none());
            first
                .respond(Response::from_string("first"))
                .expect("respond");
            let second = server
                .recv_timeout(Duration::from_secs(2))
                .expect("second")
                .expect("second request");
            assert_eq!(second.url(), "/second");
            second
                .respond(Response::from_string("second"))
                .expect("respond");
        })
    };
    let mut stream = connect(addr);
    stream
        .write_all(b"GET /first HTTP/1.1\r\nHost: localhost\r\n\r\nGET /second HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(response.contains("first"), "{response}");
    assert!(response.contains("second"), "{response}");
    handle.join().expect("server thread");
}

#[test]
fn excess_connections_are_closed_and_the_server_recovers() {
    let limits = HttpLimits {
        max_active_connections: 1,
        max_queued_requests: 1,
        ..limits_for_tests()
    };
    let server = Server::http_with_limits("127.0.0.1:0", limits).expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    thread::spawn(move || {
        for _ in 0..2 {
            if let Ok(Some(request)) = server.recv_timeout(Duration::from_secs(2)) {
                let url = request.url().to_string();
                let _ = request.respond(Response::from_string(url));
            }
        }
    });
    let mut held = connect(addr);
    held.write_all(b"GET /held HTTP/1.1\r\nHost: localhost\r\n")
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    let mut extra = connect(addr);
    extra.write_all(b"GET /extra HTTP/1.1\r\n\r\n").unwrap();
    let rejected = read_available(&mut extra);
    assert!(
        rejected.is_empty(),
        "excess connection must close without a response: {}",
        String::from_utf8_lossy(&rejected)
    );

    held.write_all(b"\r\n").unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut held)).into_owned();
    assert!(response.contains("/held"), "{response}");
    held.shutdown(Shutdown::Both).ok();

    let mut recovered = None;
    for _ in 0..20 {
        if let Ok(stream) = TcpStream::connect(addr) {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            recovered = Some(stream);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let mut recovered = recovered.expect("server accepts a connection after the slot frees");
    recovered
        .write_all(b"GET /recovered HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut recovered)).into_owned();
    assert!(response.contains("/recovered"), "{response}");
}

#[test]
fn a_stalled_body_hits_the_absolute_deadline() {
    let server = Server::http_with_limits(
        "127.0.0.1:0",
        HttpLimits {
            body_deadline: Duration::from_millis(200),
            ..limits_for_tests()
        },
    )
    .expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    let handle = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(2))
            .expect("recv")
            .expect("request");
        let started = Instant::now();
        let mut body = Vec::new();
        let error = request
            .as_reader()
            .read_to_end(&mut body)
            .expect_err("deadline");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    });
    let mut stream = connect(addr);
    stream
        .write_all(b"POST /body HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2048\r\n\r\nx")
        .unwrap();
    handle.join().expect("server thread");
}

#[test]
fn the_request_queue_rejects_work_beyond_its_cap() {
    let server = Server::http_with_limits(
        "127.0.0.1:0",
        HttpLimits {
            max_active_connections: 2,
            max_queued_requests: 1,
            ..limits_for_tests()
        },
    )
    .expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    let server = std::sync::Arc::new(server);
    let (release, wait) = std::sync::mpsc::channel();
    let handle = {
        let server = std::sync::Arc::clone(&server);
        thread::spawn(move || {
            wait.recv().expect("both requests were written");
            thread::sleep(Duration::from_millis(100));
            let first = server
                .recv_timeout(Duration::from_secs(2))
                .expect("recv")
                .expect("first");
            assert_eq!(first.url(), "/queued");
            assert!(server.try_recv().expect("poll").is_none());
            first
                .respond(Response::from_string("queued"))
                .expect("respond");
        })
    };
    let mut queued = connect(addr);
    queued
        .write_all(b"GET /queued HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    let mut overflow = connect(addr);
    overflow
        .write_all(b"GET /overflow HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    release.send(()).expect("release handler");
    let overflow_response = String::from_utf8_lossy(&read_available(&mut overflow)).into_owned();
    assert!(
        !overflow_response.contains("/overflow"),
        "{overflow_response}"
    );
    let queued_response = String::from_utf8_lossy(&read_available(&mut queued)).into_owned();
    assert!(queued_response.contains("queued"), "{queued_response}");
    handle.join().expect("server thread");
}

#[test]
fn write_deadline_starts_when_the_response_is_written() {
    let server = Server::http_with_limits(
        "127.0.0.1:0",
        HttpLimits {
            write_deadline: Duration::from_millis(150),
            ..limits_for_tests()
        },
    )
    .expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    let handle = thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(2))
            .expect("recv")
            .expect("request");
        thread::sleep(Duration::from_millis(250));
        request
            .respond(Response::from_string("late"))
            .expect("queue wait must not consume the write deadline");
    });
    let mut stream = connect(addr);
    stream
        .write_all(b"GET /late HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(response.contains("late"), "{response}");
    handle.join().expect("server thread");
}

#[test]
fn an_unread_enormous_body_closes_promptly_without_growing_the_discard_buffer() {
    let addr = spawn_echo(limits_for_tests());
    let before = rss_bytes();
    let started = Instant::now();
    let mut stream = connect(addr);
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4000000000\r\n\r\n")
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(response.contains("200"), "{response}");
    assert_eq!(stream.read(&mut [0; 8]).unwrap_or(1), 0);
    if let (Some(before), Some(after)) = (before, rss_bytes()) {
        assert!(
            after.saturating_sub(before) < 64 * 1024 * 1024,
            "declared Content-Length must not size an allocation: before={before} after={after}"
        );
    }

    let mut recovered = connect(addr);
    recovered
        .write_all(b"GET /recovered HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut recovered)).into_owned();
    assert!(response.contains("/recovered"), "{response}");
}

#[test]
fn unsupported_http_version_releases_connection_slots() {
    let limits = HttpLimits {
        max_active_connections: 2,
        max_queued_requests: 2,
        ..limits_for_tests()
    };
    let addr = spawn_echo(limits);
    let mut stuck = Vec::new();
    for _ in 0..4 {
        let mut stream = connect(addr);
        stream
            .write_all(b"GET / HTTP/2.0\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
        assert!(response.contains("505"), "{response}");
        stuck.push(stream);
    }
    drop(stuck);

    for _ in 0..4 {
        let mut stream = connect(addr);
        stream
            .write_all(b"GET / HTTP/2.0\r\nHost: localhost\r\n\r\n")
            .unwrap();
        drop(stream);
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if stream
                .write_all(b"GET /after-version HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .is_ok()
            {
                last = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
                if last.contains("/after-version") {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        last.contains("/after-version"),
        "slot recovered after HTTP/2.0 rejections and disconnects: {last}"
    );
}

#[test]
fn oversized_chunk_metadata_is_rejected_and_the_server_recovers() {
    let addr = spawn_body_echo(limits_for_tests());
    let before = rss_bytes();
    let started = Instant::now();
    let mut stream = connect(addr);
    write_or_closed(
        &mut stream,
        b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2",
    );
    write_repeating_or_closed(&mut stream, b' ', MAX_CHUNK_METADATA_BYTES);
    write_or_closed(&mut stream, b"\r\n{}\r\n0\r\n\r\n");
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?} {response}",
        started.elapsed()
    );
    assert!(
        !response.contains("{}"),
        "decoded payload must not be accepted after oversized chunk metadata: {response}"
    );
    assert_eq!(stream.read(&mut [0; 8]).unwrap_or(1), 0);
    if let (Some(before), Some(after)) = (before, rss_bytes()) {
        assert!(
            after.saturating_sub(before) < 64 * 1024 * 1024,
            "chunk-size line must not size an allocation: before={before} after={after}"
        );
    }
    recovered_body_echo(addr);
}

#[test]
fn unfinished_chunk_metadata_hits_the_absolute_deadline() {
    let server = Server::http_with_limits(
        "127.0.0.1:0",
        HttpLimits {
            body_deadline: Duration::from_millis(200),
            ..limits_for_tests()
        },
    )
    .expect("bind");
    let ListenAddr::IP(addr) = server.server_addr() else {
        panic!("tcp");
    };
    let handle = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(2))
            .expect("recv")
            .expect("request");
        let started = Instant::now();
        let mut body = Vec::new();
        let error = request
            .as_reader()
            .read_to_end(&mut body)
            .expect_err("deadline");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(body.is_empty());
    });
    let mut stream = connect(addr);
    stream
        .write_all(
            b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2",
        )
        .unwrap();
    handle.join().expect("server thread");
}

#[test]
fn overflowing_chunk_size_is_rejected_and_the_server_recovers() {
    let addr = spawn_body_echo(limits_for_tests());
    let mut stream = connect(addr);
    write_or_closed(
        &mut stream,
        b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n{}\r\n0\r\n\r\n",
    );
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(
        !response.contains("{}"),
        "overflowing chunk size must not decode a body: {response}"
    );
    assert_eq!(stream.read(&mut [0; 8]).unwrap_or(1), 0);
    recovered_body_echo(addr);
}

#[test]
fn valid_chunked_requests_decode_the_payload() {
    let addr = spawn_body_echo(limits_for_tests());
    let mut stream = connect(addr);
    stream
        .write_all(
            b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
        )
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(response.contains("200"), "{response}");
    assert!(
        response.contains("{}"),
        "valid chunked body must be decoded: {response}"
    );

    let mut with_ext = connect(addr);
    with_ext
        .write_all(
            b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2;ext=1\r\n{}\r\n0\r\n\r\n",
        )
        .unwrap();
    let response = String::from_utf8_lossy(&read_available(&mut with_ext)).into_owned();
    assert!(
        response.contains("200") && response.contains("{}"),
        "{response}"
    );
}

#[test]
fn incomplete_chunk_payload_on_half_close_is_rejected() {
    let addr = spawn_body_echo(limits_for_tests());
    let mut stream = connect(addr);
    write_or_closed(
        &mut stream,
        b"POST /chunk HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{}",
    );
    stream.shutdown(Shutdown::Write).ok();
    let response = String::from_utf8_lossy(&read_available(&mut stream)).into_owned();
    assert!(
        !response.contains("{}"),
        "truncated chunk payload must not be treated as a complete body: {response}"
    );
    recovered_body_echo(addr);
}

fn rss_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(value) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let kb: usize = value.split_whitespace().next()?.parse().ok()?;
        return Some(kb.saturating_mul(1024));
    }
    None
}
