use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use statsai_core::{ArchiveConversation, ArchiveItemKind};

use crate::{
    DetectedSpan, DetectionConfidence, DetectorKind, PrivacyCategory, PrivacyError,
    PrivacyReplacement,
};

pub(super) const MAX_FILTER_PASSES: usize = 4;
static GENERATED_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[(?:SECRET|(?:ACCOUNT|ADDRESS|DATE|EMAIL|PERSON|PHONE|URL|PATH|HOST|IP|PROJECT|REPOSITORY|BRANCH|TOOL_CALL)_\d{6})\]",
    )
    .expect("valid generated placeholder regex")
});

pub(super) fn residual_error(field_path: String, span: DetectedSpan) -> PrivacyError {
    PrivacyError::ResidualFinding {
        field_path,
        start: span.start,
        end: span.end,
        detector: span.detector,
        category: span.category,
    }
}

pub(super) fn map_filtered_span_to_input(
    input: &str,
    filtered: &crate::FilteredText,
    span: &DetectedSpan,
) -> Result<Range<usize>, PrivacyError> {
    span.validate_for(&filtered.text)?;
    let start = map_filtered_boundary(&filtered.replacements, input.len(), span.start, false)
        .ok_or(PrivacyError::Protocol(
            "map residual start to privacy input",
        ))?;
    let end = map_filtered_boundary(&filtered.replacements, input.len(), span.end, true)
        .ok_or(PrivacyError::Protocol("map residual end to privacy input"))?;
    if start >= end
        || end > input.len()
        || !input.is_char_boundary(start)
        || !input.is_char_boundary(end)
    {
        return Err(PrivacyError::InvalidSpan);
    }
    Ok(start..end)
}

fn map_filtered_boundary(
    replacements: &[PrivacyReplacement],
    input_len: usize,
    offset: usize,
    end_boundary: bool,
) -> Option<usize> {
    let mut input_cursor = 0usize;
    let mut output_cursor = 0usize;
    for replacement in replacements {
        let unchanged = replacement.start.checked_sub(input_cursor)?;
        let unchanged_end = output_cursor.checked_add(unchanged)?;
        if offset <= unchanged_end {
            return input_cursor.checked_add(offset.checked_sub(output_cursor)?);
        }
        output_cursor = unchanged_end;
        let replacement_end = output_cursor.checked_add(replacement.replacement.len())?;
        if offset < replacement_end {
            return Some(if end_boundary {
                replacement.end
            } else {
                replacement.start
            });
        }
        if offset == replacement_end {
            return Some(replacement.end);
        }
        input_cursor = replacement.end;
        output_cursor = replacement_end;
    }
    let trailing = input_len.checked_sub(input_cursor)?;
    let output_end = output_cursor.checked_add(trailing)?;
    (offset <= output_end).then(|| input_cursor + (offset - output_cursor))
}

pub(super) fn authoritative_tool_id_spans(
    conversation: &ArchiveConversation,
    path: &str,
    text: &str,
) -> Result<Vec<DetectedSpan>, PrivacyError> {
    let Some((item, suffix)) = archive_item_for_field(conversation, path) else {
        return Ok(Vec::new());
    };
    let Some(tool_call_id) = item
        .tool_call_id
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let is_tool_text = matches!(
        item.kind,
        ArchiveItemKind::ToolCall | ArchiveItemKind::ToolResult
    ) && is_part_text_path(suffix);
    if suffix == "tool_call_id" && text == tool_call_id {
        return Ok(vec![structured_tool_id_span(0, text.len())]);
    }
    if !is_tool_text {
        return Ok(Vec::new());
    }
    Ok(tool_id_text_ranges(text, tool_call_id)?
        .into_iter()
        .map(|range| structured_tool_id_span(range.start, range.end))
        .collect())
}

pub(super) fn authoritative_tool_call_id<'a>(
    conversation: &'a ArchiveConversation,
    path: &str,
) -> Option<&'a str> {
    archive_item_for_field(conversation, path)?
        .0
        .tool_call_id
        .as_deref()
        .filter(|value| !value.is_empty())
}

fn archive_item_for_field<'a, 'p>(
    conversation: &'a ArchiveConversation,
    path: &'p str,
) -> Option<(&'a statsai_core::ArchiveItem, &'p str)> {
    let item_path = path.strip_prefix("items/")?;
    let (item_index, suffix) = item_path.split_once('/')?;
    let item = item_index
        .parse::<usize>()
        .ok()
        .and_then(|index| conversation.items.get(index))?;
    Some((item, suffix))
}

fn tool_id_text_ranges(text: &str, tool_call_id: &str) -> Result<Vec<Range<usize>>, PrivacyError> {
    let parsed = serde_json::from_str::<Value>(text);
    let valid_json = parsed.is_ok();
    let tokens = json_string_tokens(text);
    let mut ranges = json_string_value_ranges(text, tool_call_id, &tokens);
    if !valid_json {
        let key_ranges = tokens
            .iter()
            .filter(|token| token.is_key)
            .map(|token| &token.content)
            .collect::<Vec<_>>();
        ranges.extend(
            text.match_indices(tool_call_id)
                .map(|(start, value)| start..start + value.len())
                .filter(|range| {
                    !key_ranges
                        .iter()
                        .any(|key| range.start >= key.start && range.end <= key.end)
                }),
        );
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.dedup();
    if let Ok(value) = parsed {
        let expected = count_json_value_occurrences(&value, tool_call_id);
        if ranges.len() != expected {
            return Err(PrivacyError::Protocol(
                "map JSON tool-call identifier offsets",
            ));
        }
    }
    Ok(ranges)
}

fn count_json_value_occurrences(value: &Value, tool_call_id: &str) -> usize {
    match value {
        Value::String(text) => text.match_indices(tool_call_id).count(),
        Value::Array(values) => values
            .iter()
            .map(|value| count_json_value_occurrences(value, tool_call_id))
            .sum(),
        Value::Object(values) => values
            .values()
            .map(|value| count_json_value_occurrences(value, tool_call_id))
            .sum(),
        _ => 0,
    }
}

fn json_string_value_ranges(
    text: &str,
    tool_call_id: &str,
    tokens: &[JsonStringToken],
) -> Vec<Range<usize>> {
    tokens
        .iter()
        .filter(|token| !token.is_key)
        .filter_map(|token| {
            decoded_json_string_ranges(
                &text[token.content.clone()],
                tool_call_id,
                token.content.start,
            )
        })
        .flatten()
        .collect()
}

#[derive(Clone, Debug)]
struct JsonStringToken {
    content: Range<usize>,
    is_key: bool,
}

fn json_string_tokens(text: &str) -> Vec<JsonStringToken> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let content_start = cursor + 1;
        let mut content_end = content_start;
        let mut closed = false;
        while content_end < bytes.len() {
            match bytes[content_end] {
                b'\\' => content_end = (content_end + 2).min(bytes.len()),
                b'"' => {
                    closed = true;
                    break;
                }
                _ => content_end += 1,
            }
        }
        let is_key = closed
            && text.as_bytes()[content_end + 1..]
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| byte == b':');
        tokens.push(JsonStringToken {
            content: content_start..content_end,
            is_key,
        });
        cursor = if closed { content_end + 1 } else { bytes.len() };
    }
    tokens
}

#[derive(Clone, Debug)]
struct JsonStringUnit {
    decoded: Range<usize>,
    source: Range<usize>,
}

fn decoded_json_string_ranges(
    source: &str,
    tool_call_id: &str,
    source_offset: usize,
) -> Option<Vec<Range<usize>>> {
    let decoded = serde_json::from_str::<String>(&format!("\"{source}\"")).ok()?;
    if !decoded.contains(tool_call_id) {
        return Some(Vec::new());
    }
    let mut units = Vec::new();
    let mut cursor = 0usize;
    let mut decoded_cursor = 0usize;
    while cursor < source.len() {
        let (unit_end, decoded_len) = if source.as_bytes()[cursor] == b'\\' {
            let unit_end = json_escape_end(source, cursor)?;
            let decoded_piece =
                serde_json::from_str::<String>(&format!("\"{}\"", &source[cursor..unit_end]))
                    .ok()?;
            (unit_end, decoded_piece.len())
        } else {
            let character = source[cursor..].chars().next()?;
            (cursor + character.len_utf8(), character.len_utf8())
        };
        units.push(JsonStringUnit {
            decoded: decoded_cursor..decoded_cursor + decoded_len,
            source: source_offset + cursor..source_offset + unit_end,
        });
        decoded_cursor += decoded_len;
        cursor = unit_end;
    }
    if decoded_cursor != decoded.len() {
        return None;
    }

    Some(
        decoded
            .match_indices(tool_call_id)
            .filter_map(|(start, value)| {
                let end = start + value.len();
                let source_start = units
                    .iter()
                    .find(|unit| unit.decoded.start == start)?
                    .source
                    .start;
                let source_end = units
                    .iter()
                    .find(|unit| unit.decoded.end == end)?
                    .source
                    .end;
                Some(source_start..source_end)
            })
            .collect(),
    )
}

fn json_escape_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let escape = *bytes.get(start + 1)?;
    if escape != b'u' {
        return Some(start + 2);
    }
    let mut end = start.checked_add(6)?;
    let code =
        u16::from_str_radix(std::str::from_utf8(bytes.get(start + 2..end)?).ok()?, 16).ok()?;
    if (0xD800..=0xDBFF).contains(&code) {
        if bytes.get(end..end + 2) != Some(b"\\u") {
            return None;
        }
        let low_end = end.checked_add(6)?;
        let low = u16::from_str_radix(std::str::from_utf8(bytes.get(end + 2..low_end)?).ok()?, 16)
            .ok()?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        end = low_end;
    }
    Some(end)
}

fn is_part_text_path(path: &str) -> bool {
    let mut segments = path.split('/');
    matches!(segments.next(), Some("parts"))
        && segments
            .next()
            .is_some_and(|value| value.parse::<usize>().is_ok())
        && matches!(segments.next(), Some("text"))
        && segments.next().is_none()
}

pub(super) fn structured_tool_id_span(start: usize, end: usize) -> DetectedSpan {
    DetectedSpan {
        start,
        end,
        category: PrivacyCategory::ToolCallId,
        detector: DetectorKind::Structured,
        confidence: Some(DetectionConfidence::High),
    }
}

pub(super) fn exclude_structured_ranges(spans: &mut Vec<DetectedSpan>, excluded: &[DetectedSpan]) {
    if excluded.is_empty() {
        return;
    }
    let mut retained = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        let mut cursor = span.start;
        for excluded_span in excluded {
            if excluded_span.end <= cursor {
                continue;
            }
            if excluded_span.start >= span.end {
                break;
            }
            if cursor < excluded_span.start {
                retained.push(DetectedSpan {
                    start: cursor,
                    end: excluded_span.start,
                    category: span.category,
                    detector: span.detector,
                    confidence: span.confidence,
                });
            }
            cursor = cursor.max(excluded_span.end);
            if cursor >= span.end {
                break;
            }
        }
        if cursor < span.end {
            retained.push(DetectedSpan {
                start: cursor,
                end: span.end,
                category: span.category,
                detector: span.detector,
                confidence: span.confidence,
            });
        }
    }
    *spans = retained;
}

pub(super) fn mask_structured_spans<'a>(text: &'a str, spans: &[DetectedSpan]) -> Cow<'a, str> {
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut masked = text.to_string();
    for span in spans.iter().rev() {
        masked.replace_range(span.start..span.end, &" ".repeat(span.end - span.start));
    }
    Cow::Owned(masked)
}

pub(super) fn mask_generated_placeholders(text: &str) -> String {
    GENERATED_PLACEHOLDER
        .replace_all(text, |matched: &regex::Captures<'_>| {
            " ".repeat(matched[0].len())
        })
        .into_owned()
}
