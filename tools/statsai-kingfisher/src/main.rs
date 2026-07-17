use std::io::{self, Read, Write};
use std::sync::Arc;

use kingfisher_rules::{Confidence, Rule, RulesDatabase, get_builtin_rules};
use kingfisher_scanner::{Scanner, ScannerConfig};

const REQUEST_MAGIC: &[u8; 4] = b"SKF1";
const RESPONSE_MAGIC: &[u8; 4] = b"SKR1";
const OP_SCAN: u8 = 1;
const OP_PING: u8 = 2;
const OP_SHUTDOWN: u8 = 3;
const MAX_SEQUENCES: usize = 128;
const MAX_SEQUENCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_SPANS: usize = 1024 * 1024;
const HELPER_IDENTITY: &str = concat!(
    "statsai-kingfisher/",
    env!("CARGO_PKG_VERSION"),
    "\nkingfisher/",
    env!("STATSAI_KINGFISHER_VERSION"),
    "\nrevision/",
    env!("STATSAI_KINGFISHER_REVISION")
);

enum ScanRequestError {
    InvalidRequest,
    ScannerFailure,
    ResponseLimit,
}

fn main() {
    if run().is_err() {
        eprintln!("statsai-kingfisher: helper failed");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ()> {
    let rules = get_builtin_rules(Some(Confidence::Medium)).map_err(|_| ())?;
    let compiled = rules
        .iter_rules()
        .map(|syntax| Rule::new(syntax.clone()))
        .collect();
    let database = RulesDatabase::from_rules(compiled).map_err(|_| ())?;
    let scanner = Scanner::with_config(
        Arc::new(database),
        ScannerConfig {
            enable_base64_decoding: true,
            enable_dedup: false,
            min_entropy_override: None,
            language_hint: None,
            redact_secrets: true,
            max_base64_depth: 2,
        },
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let mut header = [0u8; 16];
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(_) => return Err(()),
        }
        if &header[..4] != REQUEST_MAGIC {
            return Err(());
        }
        let opcode = header[4];
        let count = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        let declared_bytes =
            u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
        match opcode {
            OP_SHUTDOWN => return Ok(()),
            OP_PING if count == 0 && declared_bytes == 0 => {
                write_ping_response(&mut output).map_err(|_| ())?;
            }
            OP_SCAN => {
                if let Err(error) =
                    scan_request(&scanner, &mut input, &mut output, count, declared_bytes)
                {
                    let code = match error {
                        ScanRequestError::InvalidRequest => b"invalid_request".as_slice(),
                        ScanRequestError::ScannerFailure => b"scanner_failure".as_slice(),
                        ScanRequestError::ResponseLimit => b"response_limit".as_slice(),
                    };
                    write_error(&mut output, code).map_err(|_| ())?;
                }
            }
            _ => write_error(&mut output, b"invalid_opcode").map_err(|_| ())?,
        }
    }
}

fn write_ping_response(output: &mut impl Write) -> io::Result<()> {
    let identity_bytes = u32::try_from(HELPER_IDENTITY.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "helper identity is too large"))?;
    write_header(output, 0, 0, identity_bytes)?;
    output.write_all(HELPER_IDENTITY.as_bytes())?;
    output.flush()
}

fn scan_request(
    scanner: &Scanner,
    input: &mut impl Read,
    output: &mut impl Write,
    count: usize,
    declared_bytes: usize,
) -> Result<(), ScanRequestError> {
    if count == 0 || count > MAX_SEQUENCES || declared_bytes > MAX_REQUEST_BYTES {
        return Err(ScanRequestError::InvalidRequest);
    }
    let mut sequences = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let id = read_u64(input).map_err(|_| ScanRequestError::InvalidRequest)?;
        let length = read_u32(input).map_err(|_| ScanRequestError::InvalidRequest)? as usize;
        let _reserved = read_u32(input).map_err(|_| ScanRequestError::InvalidRequest)?;
        if length > MAX_SEQUENCE_BYTES {
            return Err(ScanRequestError::InvalidRequest);
        }
        total = total
            .checked_add(length)
            .ok_or(ScanRequestError::InvalidRequest)?;
        sequences.push((id, length));
    }
    if total != declared_bytes || total > MAX_REQUEST_BYTES {
        return Err(ScanRequestError::InvalidRequest);
    }

    let mut results = Vec::with_capacity(count);
    let mut total_spans = 0usize;
    for (id, length) in sequences {
        let mut bytes = vec![0u8; length];
        input
            .read_exact(&mut bytes)
            .map_err(|_| ScanRequestError::InvalidRequest)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| ScanRequestError::InvalidRequest)?;
        let mut spans = scanner
            .try_scan_bytes(&bytes)
            .map_err(|_| ScanRequestError::ScannerFailure)?
            .into_iter()
            .map(|finding| {
                checked_finding_span(
                    text,
                    finding.location.start_offset,
                    finding.location.end_offset,
                    finding.confidence,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        spans.sort_unstable();
        spans.dedup();
        total_spans = total_spans
            .checked_add(spans.len())
            .ok_or(ScanRequestError::InvalidRequest)?;
        if total_spans > MAX_RESPONSE_SPANS {
            return Err(ScanRequestError::ResponseLimit);
        }
        results.push((id, spans));
    }
    let total_spans = u32::try_from(total_spans).map_err(|_| ScanRequestError::InvalidRequest)?;
    write_header(output, 0, count as u32, total_spans)
        .map_err(|_| ScanRequestError::InvalidRequest)?;
    for (id, spans) in &results {
        write_u64(output, *id).map_err(|_| ScanRequestError::InvalidRequest)?;
        write_u32(output, spans.len() as u32).map_err(|_| ScanRequestError::InvalidRequest)?;
        write_u32(output, 0).map_err(|_| ScanRequestError::InvalidRequest)?;
    }
    for (_, spans) in results {
        for (start, end, confidence) in spans {
            write_u32(output, start).map_err(|_| ScanRequestError::InvalidRequest)?;
            write_u32(output, end).map_err(|_| ScanRequestError::InvalidRequest)?;
            output
                .write_all(&[confidence, 0, 0, 0])
                .map_err(|_| ScanRequestError::InvalidRequest)?;
        }
    }
    output
        .flush()
        .map_err(|_| ScanRequestError::InvalidRequest)?;
    Ok(())
}

fn checked_finding_span(
    text: &str,
    start: usize,
    end: usize,
    confidence: Confidence,
) -> Result<(u32, u32, u8), ScanRequestError> {
    if start >= end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return Err(ScanRequestError::ScannerFailure);
    }
    let start = u32::try_from(start).map_err(|_| ScanRequestError::ScannerFailure)?;
    let end = u32::try_from(end).map_err(|_| ScanRequestError::ScannerFailure)?;
    Ok((start, end, confidence_code(confidence)))
}

const fn confidence_code(confidence: Confidence) -> u8 {
    match confidence {
        Confidence::Low => 1,
        Confidence::Medium => 2,
        Confidence::High => 3,
    }
}

fn write_header(output: &mut impl Write, status: u8, count: u32, spans: u32) -> io::Result<()> {
    output.write_all(RESPONSE_MAGIC)?;
    output.write_all(&[status, 0, 0, 0])?;
    write_u32(output, count)?;
    write_u32(output, spans)
}

fn write_error(output: &mut impl Write, code: &[u8]) -> io::Result<()> {
    write_header(output, 1, 0, code.len() as u32)?;
    output.write_all(code)?;
    output.flush()
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_u32(output: &mut impl Write, value: u32) -> io::Result<()> {
    output.write_all(&value.to_le_bytes())
}

fn write_u64(output: &mut impl Write, value: u64) -> io::Result<()> {
    output.write_all(&value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_offsets_must_be_nonempty_in_bounds_utf8_ranges() {
        let text = "café";
        assert!(matches!(
            checked_finding_span(text, 0, text.len(), Confidence::High),
            Ok((0, end, 3)) if end == text.len() as u32
        ));
        for (start, end) in [(0, 0), (4, 3), (0, 6), (3, 4), (4, 5)] {
            assert!(matches!(
                checked_finding_span(text, start, end, Confidence::High),
                Err(ScanRequestError::ScannerFailure)
            ));
        }
    }

    #[test]
    fn ping_response_identifies_the_qualified_helper_and_source() {
        let mut response = Vec::new();
        write_ping_response(&mut response).unwrap();

        assert_eq!(&response[..4], RESPONSE_MAGIC);
        assert_eq!(response[4], 0);
        assert_eq!(u32::from_le_bytes(response[8..12].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(response[12..16].try_into().unwrap()) as usize,
            HELPER_IDENTITY.len()
        );
        assert_eq!(&response[16..], HELPER_IDENTITY.as_bytes());
        assert!(HELPER_IDENTITY.contains(env!("CARGO_PKG_VERSION")));
        assert!(HELPER_IDENTITY.contains(env!("STATSAI_KINGFISHER_VERSION")));
        assert!(HELPER_IDENTITY.contains(env!("STATSAI_KINGFISHER_REVISION")));
    }
}
