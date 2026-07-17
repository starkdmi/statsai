use std::io::{Read, Write};
use std::ops::Range;
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::{
    DetectedSpan, DetectionConfidence, DetectorKind, DetectorMetadata, PrivacyCategory,
    PrivacyDetector, PrivacyError,
};

const REQUEST_MAGIC: &[u8; 4] = b"SKF1";
const RESPONSE_MAGIC: &[u8; 4] = b"SKR1";
const OP_SCAN: u8 = 1;
const OP_PING: u8 = 2;
const OP_SHUTDOWN: u8 = 3;
const MAX_SEQUENCES: usize = 128;
const MAX_SEQUENCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_SPANS: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const PIPE_WRITE_CHUNK: usize = 4 * 1024;
const HELPER_VERSION: &str = "0.2.0";
const KINGFISHER_VERSION: &str = "1.106.0";
const KINGFISHER_REVISION: &str = "8fa4f142bcd32664ac0feb16fc8aabc67637660d";

#[derive(Clone, Debug)]
pub struct KingfisherOptions {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for KingfisherOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

pub struct KingfisherDetector {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    options: KingfisherOptions,
    available: bool,
}

impl KingfisherDetector {
    pub fn spawn(
        helper_executable: impl AsRef<Path>,
        options: KingfisherOptions,
    ) -> Result<Self, PrivacyError> {
        let mut child = Command::new(helper_executable.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(PrivacyError::Io)?;
        let input = child
            .stdin
            .take()
            .ok_or(PrivacyError::Protocol("missing Kingfisher helper stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or(PrivacyError::Protocol("missing Kingfisher helper stdout"))?;
        let mut detector = Self {
            child,
            input,
            output,
            options,
            available: true,
        };
        if let Err(error) =
            set_nonblocking(&detector.input).and_then(|()| set_nonblocking(&detector.output))
        {
            detector.terminate();
            return Err(PrivacyError::Io(error));
        }
        if let Err(error) = detector.ping() {
            detector.terminate();
            return Err(error);
        }
        Ok(detector)
    }

    fn ping(&mut self) -> Result<(), PrivacyError> {
        let request = control_request(OP_PING);
        let deadline = deadline_after(self.options.startup_timeout);
        write_all_before(&mut self.input, &request, deadline)?;
        let (status, count, identity_bytes) =
            read_response_header_before(&mut self.output, deadline, self.options.startup_timeout)?;
        let expected = expected_helper_identity();
        validate_ping_header(status, count, identity_bytes, expected.len())?;
        let mut identity = vec![0u8; expected.len()];
        read_exact_before(
            &mut self.output,
            &mut identity,
            deadline,
            self.options.startup_timeout,
        )?;
        validate_helper_identity(&identity, expected.as_bytes())
    }

    fn exchange_request(
        &mut self,
        texts: &[&str],
        request: &PreparedRequest,
    ) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        let deadline = deadline_after(self.options.request_timeout);
        write_all_before(&mut self.input, &request.bytes, deadline)?;
        let (status, count, declared_spans) =
            read_response_header_before(&mut self.output, deadline, self.options.request_timeout)?;
        if status != 0 {
            if declared_spans as usize > MAX_ERROR_BYTES {
                return Err(PrivacyError::Protocol(
                    "Kingfisher helper error exceeds limit",
                ));
            }
            let mut code = vec![0u8; declared_spans as usize];
            read_exact_before(
                &mut self.output,
                &mut code,
                deadline,
                self.options.request_timeout,
            )?;
            return Err(PrivacyError::Protocol("Kingfisher helper rejected request"));
        }
        validate_response_dimensions(texts.len(), count as usize, declared_spans as usize)?;

        let mut span_counts = Vec::with_capacity(texts.len());
        let mut counted_spans = 0usize;
        for expected in 0..count {
            let id = read_u64_before(&mut self.output, deadline, self.options.request_timeout)?;
            let span_count =
                read_u32_before(&mut self.output, deadline, self.options.request_timeout)? as usize;
            let _reserved =
                read_u32_before(&mut self.output, deadline, self.options.request_timeout)?;
            counted_spans = counted_spans
                .checked_add(span_count)
                .ok_or(PrivacyError::Protocol("Kingfisher span count overflow"))?;
            if id != expected as u64 || counted_spans > declared_spans as usize {
                return Err(PrivacyError::Protocol(
                    "invalid Kingfisher sequence metadata",
                ));
            }
            span_counts.push(span_count);
        }
        if counted_spans != declared_spans as usize {
            return Err(PrivacyError::Protocol(
                "Kingfisher span totals do not match",
            ));
        }

        let mut results = Vec::with_capacity(texts.len());
        for (text, span_count) in texts.iter().zip(span_counts) {
            let mut spans = Vec::with_capacity(span_count);
            for _ in 0..span_count {
                let start =
                    read_u32_before(&mut self.output, deadline, self.options.request_timeout)?
                        as usize;
                let end = read_u32_before(&mut self.output, deadline, self.options.request_timeout)?
                    as usize;
                let mut flags = [0u8; 4];
                read_exact_before(
                    &mut self.output,
                    &mut flags,
                    deadline,
                    self.options.request_timeout,
                )?;
                let confidence = match flags[0] {
                    1 => DetectionConfidence::Low,
                    2 => DetectionConfidence::Medium,
                    3 => DetectionConfidence::High,
                    _ => {
                        return Err(PrivacyError::Protocol("invalid Kingfisher confidence"));
                    }
                };
                let span = DetectedSpan {
                    start,
                    end,
                    category: PrivacyCategory::Secret,
                    detector: DetectorKind::Kingfisher,
                    confidence: Some(confidence),
                };
                span.validate_for(text)?;
                spans.push(span);
            }
            spans.sort_by_key(|span| (span.start, span.end));
            results.push(spans);
        }
        Ok(results)
    }

    fn terminate(&mut self) {
        self.available = false;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PrivacyDetector for KingfisherDetector {
    fn metadata(&self) -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::Kingfisher,
            implementation_version: kingfisher_implementation_version(),
            model_revision: Some(KINGFISHER_REVISION.to_string()),
            offline: true,
        }
    }

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        if !self.available {
            return Err(PrivacyError::Unavailable);
        }
        let mut results = Vec::with_capacity(texts.len());
        let lengths = texts.iter().map(|text| text.len()).collect::<Vec<_>>();
        for range in request_ranges(&lengths)? {
            let chunk = &texts[range];
            let request = prepare_request(chunk)?;
            match self.exchange_request(chunk, &request) {
                Ok(chunk_results) => results.extend(chunk_results),
                Err(error) => {
                    self.terminate();
                    return Err(error);
                }
            }
        }
        Ok(results)
    }
}

fn kingfisher_implementation_version() -> String {
    format!("statsai-kingfisher/{HELPER_VERSION}; kingfisher/{KINGFISHER_VERSION}")
}

fn expected_helper_identity() -> String {
    format!(
        "statsai-kingfisher/{HELPER_VERSION}\nkingfisher/{KINGFISHER_VERSION}\nrevision/{KINGFISHER_REVISION}"
    )
}

fn validate_ping_header(
    status: u8,
    count: u32,
    identity_bytes: u32,
    expected_bytes: usize,
) -> Result<(), PrivacyError> {
    if status != 0 || count != 0 || usize::try_from(identity_bytes).ok() != Some(expected_bytes) {
        return Err(PrivacyError::Protocol(
            "invalid Kingfisher startup response",
        ));
    }
    Ok(())
}

fn validate_helper_identity(identity: &[u8], expected: &[u8]) -> Result<(), PrivacyError> {
    if identity != expected {
        return Err(PrivacyError::Protocol(
            "Kingfisher helper identity does not match qualified build",
        ));
    }
    Ok(())
}

impl Drop for KingfisherDetector {
    fn drop(&mut self) {
        if !self.available {
            return;
        }
        let request = control_request(OP_SHUTDOWN);
        let deadline = deadline_after(self.options.shutdown_timeout);
        let _ = write_all_before(&mut self.input, &request, deadline);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => break,
            }
        }
        self.terminate();
    }
}

fn control_request(opcode: u8) -> [u8; 16] {
    let mut request = [0u8; 16];
    request[..4].copy_from_slice(REQUEST_MAGIC);
    request[4] = opcode;
    request
}

fn deadline_after(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

struct PreparedRequest {
    bytes: Vec<u8>,
}

fn request_ranges(lengths: &[usize]) -> Result<Vec<Range<usize>>, PrivacyError> {
    if lengths.iter().any(|&length| length > MAX_SEQUENCE_BYTES) {
        return Err(PrivacyError::Protocol(
            "Kingfisher sequence exceeds byte limit",
        ));
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    while start < lengths.len() {
        let mut end = start;
        let mut total = 0usize;
        while end < lengths.len() && end - start < MAX_SEQUENCES {
            let next_total = total
                .checked_add(lengths[end])
                .ok_or(PrivacyError::Protocol("Kingfisher byte count overflow"))?;
            if end > start && next_total > MAX_REQUEST_BYTES {
                break;
            }
            total = next_total;
            end += 1;
        }
        ranges.push(start..end);
        start = end;
    }
    Ok(ranges)
}

fn prepare_request(texts: &[&str]) -> Result<PreparedRequest, PrivacyError> {
    if texts.is_empty() || texts.len() > MAX_SEQUENCES {
        return Err(PrivacyError::Protocol(
            "Kingfisher request exceeds sequence limits",
        ));
    }
    let total_bytes = texts.iter().try_fold(0usize, |total, text| {
        total
            .checked_add(text.len())
            .filter(|sum| *sum <= MAX_REQUEST_BYTES)
    });
    let Some(total_bytes) = total_bytes else {
        return Err(PrivacyError::Protocol(
            "Kingfisher request exceeds byte limit",
        ));
    };

    let mut bytes = Vec::with_capacity(16 + texts.len() * 16 + total_bytes);
    bytes.extend_from_slice(REQUEST_MAGIC);
    bytes.extend_from_slice(&[OP_SCAN, 0, 0, 0]);
    bytes.extend_from_slice(&(texts.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(total_bytes as u32).to_le_bytes());
    for (index, text) in texts.iter().enumerate() {
        bytes.extend_from_slice(&(index as u64).to_le_bytes());
        bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }
    for text in texts {
        bytes.extend_from_slice(text.as_bytes());
    }
    Ok(PreparedRequest { bytes })
}

fn validate_response_dimensions(
    expected_sequences: usize,
    actual_sequences: usize,
    declared_spans: usize,
) -> Result<(), PrivacyError> {
    if actual_sequences != expected_sequences || declared_spans > MAX_RESPONSE_SPANS {
        return Err(PrivacyError::Protocol(
            "invalid Kingfisher response dimensions",
        ));
    }
    Ok(())
}

fn set_nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)
        .map(OFlag::from_bits_truncate)
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))?;
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))
}

fn wait_for_io(fd: &impl AsFd, events: PollFlags, deadline: Instant) -> Result<(), PrivacyError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PrivacyError::Timeout);
        }
        let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        let mut descriptors = [PollFd::new(fd.as_fd(), events)];
        match poll(&mut descriptors, timeout) {
            Ok(0) => return Err(PrivacyError::Timeout),
            Ok(_) => return Ok(()),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => {
                return Err(PrivacyError::Io(std::io::Error::from_raw_os_error(
                    errno as i32,
                )));
            }
        }
    }
}

fn write_all_before(
    writer: &mut (impl Write + AsFd),
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), PrivacyError> {
    while !bytes.is_empty() {
        wait_for_io(writer, PollFlags::POLLOUT, deadline)?;
        let chunk_len = bytes.len().min(PIPE_WRITE_CHUNK);
        match writer.write(&bytes[..chunk_len]) {
            Ok(0) => return Err(PrivacyError::Io(std::io::ErrorKind::WriteZero.into())),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(PrivacyError::Io(error)),
        }
    }
    Ok(())
}

fn read_exact_before(
    reader: &mut ChildStdout,
    mut bytes: &mut [u8],
    deadline: Instant,
    _timeout: Duration,
) -> Result<(), PrivacyError> {
    while !bytes.is_empty() {
        wait_for_io(reader, PollFlags::POLLIN, deadline)?;
        match reader.read(bytes) {
            Ok(0) => {
                return Err(PrivacyError::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            Ok(read) => bytes = &mut bytes[read..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(PrivacyError::Io(error)),
        }
    }
    Ok(())
}

fn read_u32_before(
    reader: &mut ChildStdout,
    deadline: Instant,
    timeout: Duration,
) -> Result<u32, PrivacyError> {
    let mut bytes = [0u8; 4];
    read_exact_before(reader, &mut bytes, deadline, timeout)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_before(
    reader: &mut ChildStdout,
    deadline: Instant,
    timeout: Duration,
) -> Result<u64, PrivacyError> {
    let mut bytes = [0u8; 8];
    read_exact_before(reader, &mut bytes, deadline, timeout)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_response_header_before(
    reader: &mut ChildStdout,
    deadline: Instant,
    timeout: Duration,
) -> Result<(u8, u32, u32), PrivacyError> {
    let mut header = [0u8; 16];
    read_exact_before(reader, &mut header, deadline, timeout)?;
    if &header[..4] != RESPONSE_MAGIC {
        return Err(PrivacyError::Protocol("invalid Kingfisher response magic"));
    }
    Ok((
        header[4],
        u32::from_le_bytes([header[8], header[9], header[10], header[11]]),
        u32::from_le_bytes([header[12], header[13], header[14], header[15]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn spawn_times_out_when_helper_never_becomes_ready() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("unresponsive-helper");
        fs::write(&helper, "#!/bin/sh\nexec sleep 10\n").unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();

        let options = KingfisherOptions {
            startup_timeout: Duration::from_millis(50),
            shutdown_timeout: Duration::from_millis(50),
            ..KingfisherOptions::default()
        };
        let started = Instant::now();
        let error = KingfisherDetector::spawn(&helper, options)
            .err()
            .expect("unresponsive helper should fail startup");
        assert!(matches!(error, PrivacyError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn batches_respect_aggregate_byte_limit() {
        let ranges = request_ranges(&[MAX_SEQUENCE_BYTES; 5]).unwrap();
        assert_eq!(ranges, vec![0..4, 4..5]);
    }

    #[test]
    fn response_span_limit_is_independent_of_request_bytes() {
        assert!(validate_response_dimensions(1, 1, 2).is_ok());
        assert!(validate_response_dimensions(1, 1, MAX_RESPONSE_SPANS).is_ok());
        assert!(matches!(
            validate_response_dimensions(1, 1, MAX_RESPONSE_SPANS + 1),
            Err(PrivacyError::Protocol(_))
        ));
    }

    #[test]
    fn helper_identity_requires_exact_implementation_and_source_revision() {
        let expected = expected_helper_identity();
        assert_eq!(
            kingfisher_implementation_version(),
            "statsai-kingfisher/0.2.0; kingfisher/1.106.0"
        );
        assert!(validate_ping_header(0, 0, expected.len() as u32, expected.len()).is_ok());
        assert!(matches!(
            validate_ping_header(0, 0, 0, expected.len()),
            Err(PrivacyError::Protocol(_))
        ));
        assert!(validate_helper_identity(expected.as_bytes(), expected.as_bytes()).is_ok());

        for stale in [
            "statsai-kingfisher/0.1.0\nkingfisher/1.106.0\nrevision/8fa4f142bcd32664ac0feb16fc8aabc67637660d",
            "statsai-kingfisher/0.2.0\nkingfisher/1.105.0\nrevision/8fa4f142bcd32664ac0feb16fc8aabc67637660d",
            "statsai-kingfisher/0.2.0\nkingfisher/1.106.0\nrevision/0000000000000000000000000000000000000000",
        ] {
            assert!(matches!(
                validate_helper_identity(stale.as_bytes(), expected.as_bytes()),
                Err(PrivacyError::Protocol(_))
            ));
        }
    }

    #[test]
    fn nonblocking_write_honors_timeout_when_pipe_is_full() {
        let (_reader, writer) = nix::unistd::pipe().unwrap();
        let mut writer = File::from(writer);
        set_nonblocking(&writer).unwrap();
        let payload = vec![0u8; MAX_SEQUENCE_BYTES];
        let started = Instant::now();
        let error = write_all_before(
            &mut writer,
            &payload,
            deadline_after(Duration::from_millis(50)),
        )
        .unwrap_err();
        assert!(matches!(error, PrivacyError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[ignore = "requires the separately built statsai-kingfisher helper"]
    fn detects_secret_without_returning_secret_text() {
        let executable = std::env::var_os("STATSAI_KINGFISHER_HELPER")
            .expect("STATSAI_KINGFISHER_HELPER must point to the helper");
        let mut detector =
            KingfisherDetector::spawn(executable, KingfisherOptions::default()).unwrap();
        let metadata = detector.metadata();
        assert_eq!(
            metadata.implementation_version,
            "statsai-kingfisher/0.2.0; kingfisher/1.106.0"
        );
        assert_eq!(
            metadata.model_revision.as_deref(),
            Some(KINGFISHER_REVISION)
        );
        let token = ["ghp_", "EZopZDMWeildfoFzyH0KnWyQ5Yy3vy0Y2SU6"].concat();
        let texts = [format!("café token = {token}"), format!("token = {token}")];
        let text_refs = texts.iter().map(String::as_str).collect::<Vec<_>>();
        let detections = detector.detect_batch(&text_refs).unwrap();
        assert_eq!(detections.len(), 2);
        for (text, spans) in texts.iter().zip(detections) {
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].category, PrivacyCategory::Secret);
            assert_eq!(
                &text[spans[0].start..spans[0].end],
                "EZopZDMWeildfoFzyH0KnWyQ5Yy3vy"
            );
        }

        let oversized = "x".repeat(MAX_SEQUENCE_BYTES + 1);
        assert!(matches!(
            detector.detect(&oversized),
            Err(PrivacyError::Protocol(_))
        ));
        assert_eq!(detector.detect("ordinary text").unwrap(), Vec::new());

        let large = "x".repeat(MAX_SEQUENCE_BYTES);
        let large_batch = [large.as_str(); 5];
        let detections = detector.detect_batch(&large_batch).unwrap();
        assert_eq!(detections.len(), large_batch.len());
        assert!(detections.iter().all(Vec::is_empty));
    }
}
