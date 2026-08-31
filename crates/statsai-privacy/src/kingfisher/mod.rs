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
const SEQUENCE_OVERLAP_BYTES: usize = 64 * 1024;
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
    #[must_use]
    pub fn qualified_metadata() -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::Kingfisher,
            implementation_version: kingfisher_implementation_version(),
            model_revision: Some(KINGFISHER_REVISION.to_string()),
            offline: true,
        }
    }

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
        Self::qualified_metadata()
    }

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        if !self.available {
            return Err(PrivacyError::Unavailable);
        }
        let chunks = sequence_chunks(texts)?;
        let lengths = chunks
            .iter()
            .map(|chunk| chunk.range.len())
            .collect::<Vec<_>>();
        let mut results = vec![Vec::new(); texts.len()];
        for range in request_ranges(&lengths)? {
            let request_chunks = &chunks[range];
            let request_texts = request_chunks
                .iter()
                .map(|chunk| &texts[chunk.text_index][chunk.range.clone()])
                .collect::<Vec<_>>();
            let request = prepare_request(&request_texts)?;
            match self.exchange_request(&request_texts, &request) {
                Ok(chunk_results) => {
                    if let Err(error) =
                        append_chunk_results(texts, request_chunks, chunk_results, &mut results)
                    {
                        self.terminate();
                        return Err(error);
                    }
                }
                Err(error) => {
                    self.terminate();
                    return Err(error);
                }
            }
        }
        normalize_chunk_results(&mut results);
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct SequenceChunk {
    text_index: usize,
    range: Range<usize>,
}

fn sequence_chunks(texts: &[&str]) -> Result<Vec<SequenceChunk>, PrivacyError> {
    let mut chunks = Vec::new();
    for (text_index, text) in texts.iter().enumerate() {
        chunks.extend(
            chunk_ranges(text, MAX_SEQUENCE_BYTES, SEQUENCE_OVERLAP_BYTES)?
                .into_iter()
                .map(|range| SequenceChunk { text_index, range }),
        );
    }
    Ok(chunks)
}

fn chunk_ranges(
    text: &str,
    max_bytes: usize,
    overlap_bytes: usize,
) -> Result<Vec<Range<usize>>, PrivacyError> {
    if max_bytes == 0 || overlap_bytes >= max_bytes {
        return Err(PrivacyError::Protocol(
            "invalid Kingfisher chunk configuration",
        ));
    }
    if text.len() <= max_bytes {
        return Ok(std::iter::once(0..text.len()).collect());
    }

    let mut ranges = Vec::with_capacity(text.len().div_ceil(max_bytes - overlap_bytes));
    let mut start = 0usize;
    loop {
        let mut end = start.saturating_add(max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            return Err(PrivacyError::Protocol(
                "Kingfisher chunk has no UTF-8 boundary",
            ));
        }
        ranges.push(start..end);
        if end == text.len() {
            break;
        }
        let mut next = end.saturating_sub(overlap_bytes);
        while next > start && !text.is_char_boundary(next) {
            next -= 1;
        }
        if next <= start {
            return Err(PrivacyError::Protocol("Kingfisher chunk does not advance"));
        }
        start = next;
    }
    Ok(ranges)
}

fn append_chunk_results(
    texts: &[&str],
    chunks: &[SequenceChunk],
    chunk_results: Vec<Vec<DetectedSpan>>,
    combined: &mut [Vec<DetectedSpan>],
) -> Result<(), PrivacyError> {
    if chunks.len() != chunk_results.len() || combined.len() != texts.len() {
        return Err(PrivacyError::Protocol(
            "Kingfisher chunk result count differs from input",
        ));
    }
    for (chunk, spans) in chunks.iter().zip(chunk_results) {
        let text = texts
            .get(chunk.text_index)
            .ok_or(PrivacyError::Protocol("invalid Kingfisher chunk source"))?;
        let output = combined
            .get_mut(chunk.text_index)
            .ok_or(PrivacyError::Protocol("invalid Kingfisher output source"))?;
        for mut span in spans {
            span.start = chunk
                .range
                .start
                .checked_add(span.start)
                .ok_or(PrivacyError::Protocol("Kingfisher span offset overflow"))?;
            span.end = chunk
                .range
                .start
                .checked_add(span.end)
                .ok_or(PrivacyError::Protocol("Kingfisher span offset overflow"))?;
            span.validate_for(text)?;
            output.push(span);
        }
    }
    Ok(())
}

fn normalize_chunk_results(results: &mut [Vec<DetectedSpan>]) {
    for spans in results {
        spans.sort_by_key(|span| {
            (
                span.start,
                span.end,
                span.category,
                span.detector,
                span.confidence,
            )
        });
        spans.dedup();
    }
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
mod tests;
