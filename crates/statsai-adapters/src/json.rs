use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;

#[cfg(test)]
use std::io::{BufReader, Cursor};

pub(crate) const MAX_JSONL_RECORD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedLineRead {
    Eof,
    Complete,
    Oversized,
}

pub(crate) fn read_bounded_jsonl_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<BoundedLineRead> {
    line.clear();
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if !saw_bytes {
                return Ok(BoundedLineRead::Eof);
            }
            if oversized || line.len() > max_bytes {
                line.clear();
                return Ok(BoundedLineRead::Oversized);
            }
            return Ok(BoundedLineRead::Complete);
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let ended = available.get(consumed.saturating_sub(1)) == Some(&b'\n');
        saw_bytes = true;
        if !oversized {
            let delimiter_bytes = if ended {
                let preceding_byte = if consumed >= 2 {
                    available.get(consumed - 2)
                } else {
                    line.last()
                };
                1 + usize::from(preceding_byte == Some(&b'\r'))
            } else {
                0
            };
            let record_bytes = line
                .len()
                .saturating_add(consumed)
                .saturating_sub(delimiter_bytes);
            let deferred_cr = !ended
                && record_bytes == max_bytes.saturating_add(1)
                && available.get(consumed - 1) == Some(&b'\r');
            if record_bytes <= max_bytes || deferred_cr {
                line.extend_from_slice(&available[..consumed]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(consumed);
        if ended {
            return Ok(if oversized {
                BoundedLineRead::Oversized
            } else {
                BoundedLineRead::Complete
            });
        }
    }
}

pub(crate) fn number_at_any(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(value_as_u64))
}

pub(crate) fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_i64()
                .and_then(|value| (value >= 0).then_some(value as u64))
        })
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
}

pub(crate) fn timestamp_from_nested_value(value: &Value) -> Option<DateTime<Utc>> {
    for candidate in [
        value.get("timestamp"),
        value.get("created_at"),
        value.get("createdAt"),
        value.get("time"),
        value.pointer("/message/timestamp"),
        value.pointer("/data/timestamp"),
        value.pointer("/result/timestamp"),
        value.pointer("/response/timestamp"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(timestamp) = timestamp_from_scalar(candidate) {
            return Some(timestamp);
        }
    }
    None
}

pub(crate) fn timestamp_from_scalar(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(millis) = text.parse::<i64>() {
            return timestamp_from_number(millis);
        }
    }
    value.as_i64().and_then(timestamp_from_number)
}

pub(crate) fn stats_cache_date_end(value: &Value) -> Option<DateTime<Utc>> {
    timestamp_from_scalar(value).or_else(|| {
        let text = value.as_str()?;
        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        Some(date.and_hms_opt(23, 59, 59)?.and_utc())
    })
}

pub(crate) fn timestamp_from_number(value: i64) -> Option<DateTime<Utc>> {
    if value > 10_000_000_000 {
        Utc.timestamp_millis_opt(value).single()
    } else {
        Utc.timestamp_opt(value, 0).single()
    }
}

pub(crate) fn timestamp_from_millis(value: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value).single()
}

pub(crate) fn file_modified_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(DateTime::<Utc>::from)
}

pub(crate) fn read_json_file(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[test]
fn bounded_jsonl_reader_discards_an_oversized_record_and_recovers_next_line() {
    let input = format!("{}\n{{\"ok\":true}}\n", "x".repeat(9));
    let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
    let mut line = Vec::new();

    assert_eq!(
        read_bounded_jsonl_line(&mut reader, &mut line, 8).expect("oversized line"),
        BoundedLineRead::Oversized
    );
    assert!(line.is_empty());
    assert_eq!(
        read_bounded_jsonl_line(&mut reader, &mut line, 32).expect("next line"),
        BoundedLineRead::Complete
    );
    assert_eq!(line, b"{\"ok\":true}\n");
}

#[test]
fn bounded_jsonl_reader_excludes_eof_lf_and_crlf_delimiters_from_the_limit() {
    for input in [b"12345678".as_slice(), b"12345678\n", b"12345678\r\n"] {
        let mut reader = BufReader::new(Cursor::new(input));
        let mut line = Vec::new();

        assert_eq!(
            read_bounded_jsonl_line(&mut reader, &mut line, 8).expect("boundary line"),
            BoundedLineRead::Complete,
            "input {input:?}"
        );
        assert_eq!(line, input, "input {input:?}");
    }
}

#[test]
fn bounded_jsonl_reader_handles_crlf_split_across_buffers_at_the_limit() {
    let input = b"12345678\r\n";
    let mut reader = BufReader::with_capacity(9, Cursor::new(input));
    let mut line = Vec::new();

    assert_eq!(
        read_bounded_jsonl_line(&mut reader, &mut line, 8).expect("split CRLF line"),
        BoundedLineRead::Complete
    );
    assert_eq!(line, input);

    let mut unterminated_reader = BufReader::new(Cursor::new(b"12345678\r"));
    assert_eq!(
        read_bounded_jsonl_line(&mut unterminated_reader, &mut line, 8)
            .expect("unterminated trailing CR"),
        BoundedLineRead::Oversized
    );
    assert!(line.is_empty());
}
