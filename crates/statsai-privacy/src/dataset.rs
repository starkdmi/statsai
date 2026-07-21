use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::LazyLock;

use chrono::Datelike;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use statsai_core::{ArchiveConversation, ArchiveItemKind};

use crate::{
    filter_text, DetectedSpan, DetectionConfidence, DetectorKind, DetectorMetadata,
    PrivacyCategory, PrivacyDetectorSet, PrivacyError, PrivacyReplacement,
};

pub const FILTERED_CONVERSATION_SCHEMA_VERSION: &str = "filtered_conversation.v1";
pub const FILTERED_DATASET_SCHEMA_VERSION: &str = "filtered_dataset.v1";
const FILTER_POLICY_VERSION: &str = "privacy_policy.v4";
const MAX_FILTER_PASSES: usize = 4;
static GENERATED_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[(?:SECRET|(?:ACCOUNT|ADDRESS|DATE|EMAIL|PERSON|PHONE|URL|PATH|HOST|IP|PROJECT|REPOSITORY|BRANCH|TOOL_CALL)_\d{6})\]",
    )
    .expect("valid generated placeholder regex")
});

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilteredFieldFinding {
    pub field_path: String,
    pub start: u64,
    pub end: u64,
    pub category: PrivacyCategory,
    pub detector: crate::DetectorKind,
    pub confidence: Option<crate::DetectionConfidence>,
    pub replacement: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetectorObservationSummary {
    pub findings_by_detector: BTreeMap<DetectorKind, u64>,
    pub cross_detector_overlaps: u64,
    pub detection_passes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FilterArchiveResult {
    pub conversation: FilteredConversation,
    pub findings: Vec<FilteredFieldFinding>,
    pub input_fingerprint: String,
    pub detector_observations: DetectorObservationSummary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FilteredConversation {
    pub schema_version: String,
    pub dataset_key: String,
    pub provider: String,
    pub day: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Value>,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FilteredDatasetManifest {
    pub schema_version: String,
    pub policy_fingerprint: String,
    pub conversation_schema: String,
    pub conversations: u64,
    pub pseudonym_namespace: String,
    pub detectors: Vec<DetectorMetadata>,
}

pub fn privacy_policy_fingerprint(metadata: &[DetectorMetadata]) -> String {
    let payload = serde_json::to_vec(&(FILTER_POLICY_VERSION, metadata))
        .expect("privacy policy metadata is serializable");
    hex_sha256(&payload)
}

pub fn archive_privacy_input_fingerprint(
    conversation: &ArchiveConversation,
) -> Result<String, PrivacyError> {
    let input = input_projection(conversation);
    let input_bytes = serde_json::to_vec(&input)
        .map_err(|_| PrivacyError::Protocol("serialize privacy input projection"))?;
    Ok(hex_sha256(&input_bytes))
}

pub fn filter_archive_conversation(
    conversation: &ArchiveConversation,
    dataset_key: String,
    detectors: &mut PrivacyDetectorSet,
    mut alias: impl FnMut(PrivacyCategory, &str) -> Result<u64, PrivacyError>,
) -> Result<FilterArchiveResult, PrivacyError> {
    let input = input_projection(conversation);
    let input_fingerprint = archive_privacy_input_fingerprint(conversation)?;
    let mut fields = Vec::new();
    collect_string_fields(&input, "", &mut fields);
    let structured_tool_spans = fields
        .iter()
        .map(|(path, text)| authoritative_tool_id_spans(conversation, path, text))
        .collect::<Result<Vec<_>, _>>()?;
    let detector_inputs = fields
        .iter()
        .zip(&structured_tool_spans)
        .map(|((_, text), spans)| mask_structured_spans(text, spans))
        .collect::<Vec<_>>();
    let texts = detector_inputs
        .iter()
        .map(|value| value.as_ref())
        .collect::<Vec<_>>();
    let mut detected = detectors.detect_batch(&texts)?;
    for (((path, text), spans), tool_spans) in
        fields.iter().zip(&mut detected).zip(structured_tool_spans)
    {
        exclude_structured_ranges(spans, &tool_spans);
        spans.extend(tool_spans);
        if let Some(category) = authoritative_project_field(path, text) {
            spans.push(DetectedSpan {
                start: 0,
                end: text.len(),
                category,
                detector: DetectorKind::Structured,
                confidence: Some(DetectionConfidence::High),
            });
        }
        spans.sort_by_key(|span| {
            (
                span.start,
                span.end,
                span.detector,
                span.category,
                span.confidence,
            )
        });
    }
    let mut detector_observations = DetectorObservationSummary::default();
    observe_detector_findings(&mut detector_observations, &detected);
    drop(texts);
    drop(detector_inputs);

    let mut converged = None;
    for pass in 0..MAX_FILTER_PASSES {
        let mut filtered_fields = Vec::with_capacity(fields.len());
        for ((path, text), spans) in fields.iter().zip(&detected) {
            let tool_call_id = authoritative_tool_call_id(conversation, path);
            filtered_fields.push(filter_text(text, spans.clone(), |category, value| {
                let canonical = if category == PrivacyCategory::ToolCallId {
                    tool_call_id.unwrap_or(value)
                } else {
                    value
                };
                alias(category, canonical)
            })?);
        }
        let masked_residuals = filtered_fields
            .iter()
            .map(|filtered| mask_generated_placeholders(&filtered.text))
            .collect::<Vec<_>>();
        let residual_texts = masked_residuals
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let residual_model = detectors.detect_batch(&residual_texts)?;
        observe_detector_findings(&mut detector_observations, &residual_model);
        let mut first_residual = None;
        let mut additions = 0usize;
        for (index, model) in residual_model.into_iter().enumerate() {
            for span in model {
                first_residual.get_or_insert_with(|| (fields[index].0.clone(), span.clone()));
                let mapped =
                    map_filtered_span_to_input(fields[index].1, &filtered_fields[index], &span)?;
                if detected[index]
                    .iter()
                    .any(|existing| mapped.start >= existing.start && mapped.end <= existing.end)
                {
                    continue;
                }
                detected[index].push(DetectedSpan {
                    start: mapped.start,
                    end: mapped.end,
                    category: span.category,
                    detector: span.detector,
                    confidence: span.confidence,
                });
                additions += 1;
            }
        }
        let Some((path, span)) = first_residual else {
            converged = Some(filtered_fields);
            break;
        };
        if additions == 0 || pass + 1 == MAX_FILTER_PASSES {
            return Err(residual_error(path, span));
        }
    }
    let filtered_fields = converged.ok_or(PrivacyError::Protocol(
        "privacy filtering did not produce a converged result",
    ))?;
    let mut filtered_values = BTreeMap::new();
    let mut findings = Vec::new();
    for ((path, _), filtered) in fields.iter().zip(filtered_fields) {
        findings.extend(
            filtered
                .replacements
                .iter()
                .map(|replacement| finding_from_replacement(path, replacement)),
        );
        filtered_values.insert(path.clone(), filtered.text);
    }
    drop(fields);
    let filtered_projection = replace_string_fields(input, "", &filtered_values);
    let filtered = filtered_from_projection(filtered_projection, dataset_key)?;
    Ok(FilterArchiveResult {
        conversation: filtered,
        findings,
        input_fingerprint,
        detector_observations,
    })
}

fn observe_detector_findings(
    summary: &mut DetectorObservationSummary,
    findings: &[Vec<DetectedSpan>],
) {
    summary.detection_passes += 1;
    for spans in findings {
        for span in spans {
            *summary
                .findings_by_detector
                .entry(span.detector)
                .or_default() += 1;
        }
        for (index, span) in spans.iter().enumerate() {
            for other in &spans[index + 1..] {
                if other.start >= span.end {
                    break;
                }
                if span.detector != other.detector && span.start < other.end {
                    summary.cross_detector_overlaps += 1;
                }
            }
        }
    }
}

fn residual_error(field_path: String, span: DetectedSpan) -> PrivacyError {
    PrivacyError::ResidualFinding {
        field_path,
        start: span.start,
        end: span.end,
        detector: span.detector,
        category: span.category,
    }
}

fn map_filtered_span_to_input(
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

fn authoritative_project_field(path: &str, value: &str) -> Option<PrivacyCategory> {
    if value.trim().is_empty() {
        return None;
    }
    match path {
        "project/name" => Some(PrivacyCategory::Project),
        "project/repository" => Some(PrivacyCategory::Repository),
        "project/branch" => Some(PrivacyCategory::Branch),
        "project/path" => Some(PrivacyCategory::Path),
        _ => None,
    }
}

fn authoritative_tool_id_spans(
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

fn authoritative_tool_call_id<'a>(
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

fn structured_tool_id_span(start: usize, end: usize) -> DetectedSpan {
    DetectedSpan {
        start,
        end,
        category: PrivacyCategory::ToolCallId,
        detector: DetectorKind::Structured,
        confidence: Some(DetectionConfidence::High),
    }
}

fn exclude_structured_ranges(spans: &mut Vec<DetectedSpan>, excluded: &[DetectedSpan]) {
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

fn mask_structured_spans<'a>(text: &'a str, spans: &[DetectedSpan]) -> Cow<'a, str> {
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut masked = text.to_string();
    for span in spans.iter().rev() {
        masked.replace_range(span.start..span.end, &" ".repeat(span.end - span.start));
    }
    Cow::Owned(masked)
}

fn mask_generated_placeholders(text: &str) -> String {
    GENERATED_PLACEHOLDER
        .replace_all(text, |matched: &regex::Captures<'_>| {
            " ".repeat(matched[0].len())
        })
        .into_owned()
}

fn input_projection(conversation: &ArchiveConversation) -> Value {
    let day = conversation
        .started_at
        .or(conversation.updated_at)
        .map(day_string);
    let project = conversation.project.as_ref().map(|project| {
        json!({
            "name": project.project_label,
            "repository": project.repo_label,
            "branch": project.branch_label,
            "path": project.path_label,
        })
    });
    let items = conversation
        .items
        .iter()
        .map(|item| {
            let parts = item
                .parts
                .iter()
                .filter_map(|part| {
                    if part.text.is_none()
                        && part.external_uri.is_none()
                        && part.name.is_none()
                        && part.mime_type.is_none()
                    {
                        return None;
                    }
                    Some(json!({
                        "ordinal": part.ordinal,
                        "kind": part.kind.as_str(),
                        "mime_type": part.mime_type,
                        "name": part.name,
                        "text": part.text,
                        "external_uri": part.external_uri,
                    }))
                })
                .collect::<Vec<_>>();
            json!({
                "ordinal": item.ordinal,
                "kind": item.kind.as_str(),
                "role": item.role.map(|role| role.as_str()),
                "day": item.created_at.map(day_string),
                "model": item.model,
                "tool_name": item.tool_name,
                "tool_call_id": item.tool_call_id,
                "status": item.status,
                "usage": item.usage,
                "parts": parts,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": FILTERED_CONVERSATION_SCHEMA_VERSION,
        "provider": conversation.provider,
        "day": day,
        "title": conversation.title,
        "project": project,
        "items": items,
    })
}

fn day_string(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        timestamp.year(),
        timestamp.month(),
        timestamp.day()
    )
}

fn filtered_from_projection(
    mut projection: Value,
    dataset_key: String,
) -> Result<FilteredConversation, PrivacyError> {
    let object = projection.as_object_mut().ok_or(PrivacyError::Protocol(
        "privacy projection is not an object",
    ))?;
    object.insert("dataset_key".to_string(), Value::String(dataset_key));
    serde_json::from_value(projection)
        .map_err(|_| PrivacyError::Protocol("deserialize filtered conversation"))
}

fn collect_string_fields<'a>(value: &'a Value, path: &str, output: &mut Vec<(String, &'a str)>) {
    match value {
        Value::String(text) => output.push((path.to_string(), text)),
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_string_fields(value, &join_path(path, &index.to_string()), output);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if matches!(
                    key.as_str(),
                    "schema_version" | "provider" | "kind" | "role" | "day"
                ) {
                    continue;
                }
                collect_string_fields(value, &join_path(path, key), output);
            }
        }
        _ => {}
    }
}

fn replace_string_fields(
    value: Value,
    path: &str,
    replacements: &BTreeMap<String, String>,
) -> Value {
    match value {
        Value::String(text) => replacements
            .get(path)
            .map_or(Value::String(text), |replacement| {
                Value::String(replacement.clone())
            }),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    replace_string_fields(value, &join_path(path, &index.to_string()), replacements)
                })
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    let child = join_path(path, &key);
                    (key, replace_string_fields(value, &child, replacements))
                })
                .collect(),
        ),
        other => other,
    }
}

fn finding_from_replacement(
    field_path: &str,
    replacement: &PrivacyReplacement,
) -> FilteredFieldFinding {
    FilteredFieldFinding {
        field_path: field_path.to_string(),
        start: replacement.start as u64,
        end: replacement.end as u64,
        category: replacement.category,
        detector: replacement.detector,
        confidence: replacement.confidence,
        replacement: replacement.replacement.clone(),
    }
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use statsai_core::{
        ArchiveCompleteness, ArchiveContentKind, ArchiveContentPart, ArchiveConversation,
        ArchiveItem, ArchiveItemKind, ArchiveRole, ProjectInfo, SourceId,
        ARCHIVE_CONVERSATION_SCHEMA_VERSION,
    };

    use super::*;
    use crate::{DetectedSpan, DetectorKind, PrivacyDetector};

    struct EmailDetector;

    impl PrivacyDetector for EmailDetector {
        fn metadata(&self) -> DetectorMetadata {
            DetectorMetadata {
                kind: DetectorKind::OpenAiPrivacyFilter,
                implementation_version: "test".to_string(),
                model_revision: Some("test".to_string()),
                offline: true,
            }
        }

        fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
            Ok(texts
                .iter()
                .map(|text| {
                    ["person@example.com", "[EMAIL_000001]"]
                        .into_iter()
                        .find_map(|needle| text.find(needle).map(|start| (start, needle.len())))
                        .map(|(start, length)| {
                            vec![DetectedSpan {
                                start,
                                end: start + length,
                                category: PrivacyCategory::Email,
                                detector: DetectorKind::OpenAiPrivacyFilter,
                                confidence: None,
                            }]
                        })
                        .unwrap_or_default()
                })
                .collect())
        }
    }

    struct CascadingDetector {
        calls: usize,
    }

    impl PrivacyDetector for CascadingDetector {
        fn metadata(&self) -> DetectorMetadata {
            DetectorMetadata {
                kind: DetectorKind::OpenAiPrivacyFilter,
                implementation_version: "test".to_string(),
                model_revision: Some("test".to_string()),
                offline: true,
            }
        }

        fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
            self.calls += 1;
            Ok(texts
                .iter()
                .map(|text| {
                    let (needle, category) = match self.calls {
                        1 => ("Alice", PrivacyCategory::Person),
                        2 => ("https://example.test", PrivacyCategory::Url),
                        _ => return Vec::new(),
                    };
                    text.find(needle)
                        .map(|start| {
                            vec![DetectedSpan {
                                start,
                                end: start + needle.len(),
                                category,
                                detector: DetectorKind::OpenAiPrivacyFilter,
                                confidence: None,
                            }]
                        })
                        .unwrap_or_default()
                })
                .collect())
        }
    }

    struct StubbornResidualDetector {
        calls: usize,
    }

    impl PrivacyDetector for StubbornResidualDetector {
        fn metadata(&self) -> DetectorMetadata {
            DetectorMetadata {
                kind: DetectorKind::OpenAiPrivacyFilter,
                implementation_version: "test".to_string(),
                model_revision: Some("test".to_string()),
                offline: true,
            }
        }

        fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
            self.calls += 1;
            Ok(texts
                .iter()
                .map(|text| {
                    if self.calls == 1 || text.len() < 7 {
                        return Vec::new();
                    }
                    vec![DetectedSpan {
                        start: 0,
                        end: 7,
                        category: PrivacyCategory::Person,
                        detector: DetectorKind::OpenAiPrivacyFilter,
                        confidence: None,
                    }]
                })
                .collect())
        }
    }

    fn tool_conversation(provider: &str, call: &str, result: &str) -> ArchiveConversation {
        tool_conversation_with_id(provider, "call-private", call, result)
    }

    fn tool_conversation_with_id(
        provider: &str,
        tool_call_id: &str,
        call: &str,
        result: &str,
    ) -> ArchiveConversation {
        let item = |ordinal, kind, text: &str| ArchiveItem {
            item_id: format!("item-{ordinal}"),
            native_item_id: Some(format!("native-item-{ordinal}")),
            source_record_id: Some(format!("record-{ordinal}")),
            ordinal,
            kind,
            role: Some(ArchiveRole::Assistant),
            created_at: None,
            model: None,
            tool_name: Some("read".to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            status: Some("completed".to_string()),
            usage: None,
            parts_authoritative: true,
            parts: vec![ArchiveContentPart::text(
                format!("part-{ordinal}"),
                0,
                ArchiveContentKind::Text,
                text.to_string(),
            )],
        };
        ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "conversation".to_string(),
            provider: provider.to_string(),
            source_id: SourceId("source".to_string()),
            native_conversation_id: "native".to_string(),
            title: None,
            project: None,
            started_at: None,
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: vec![
                item(0, ArchiveItemKind::ToolCall, call),
                item(1, ArchiveItemKind::ToolResult, result),
            ],
        }
    }

    #[test]
    fn archive_filter_omits_raw_ids_binaries_and_exact_timestamps() {
        let conversation = ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "raw-conversation-id".to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("raw-source-id".to_string()),
            native_conversation_id: "native-id".to_string(),
            title: Some("Email person@example.com".to_string()),
            project: None,
            started_at: Some(Utc::now()),
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: vec![ArchiveItem {
                item_id: "raw-item-id".to_string(),
                native_item_id: Some("native-item".to_string()),
                source_record_id: Some("source-record".to_string()),
                ordinal: 0,
                kind: ArchiveItemKind::Message,
                role: Some(ArchiveRole::User),
                created_at: Some(Utc::now()),
                model: None,
                tool_name: None,
                tool_call_id: Some("call-id".to_string()),
                status: None,
                usage: None,
                parts_authoritative: true,
                parts: vec![
                    ArchiveContentPart::text(
                        "raw-content-id".to_string(),
                        0,
                        ArchiveContentKind::Text,
                        "person@example.com".to_string(),
                    ),
                    ArchiveContentPart::binary(
                        "binary-id".to_string(),
                        1,
                        ArchiveContentKind::Image,
                        Some("image/png".to_string()),
                        Some("attachment.png".to_string()),
                        "c2VjcmV0".to_string(),
                    )
                    .expect("valid base64"),
                ],
            }],
        };
        let input_fingerprint =
            archive_privacy_input_fingerprint(&conversation).expect("input fingerprint");
        let mut changed_binary = conversation.clone();
        changed_binary.items[0].parts[1].data_base64 = Some("AA==".to_string());
        changed_binary.items[0].parts[1].content_hash = "different-binary-hash".to_string();
        assert_eq!(
            archive_privacy_input_fingerprint(&changed_binary).expect("binary-only fingerprint"),
            input_fingerprint
        );
        changed_binary.items[0].parts[1].name = Some("renamed-attachment.png".to_string());
        assert_ne!(
            archive_privacy_input_fingerprint(&changed_binary).expect("metadata fingerprint"),
            input_fingerprint
        );
        let mut detectors = PrivacyDetectorSet::new(vec![Box::new(EmailDetector)]);
        let result = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |_, _| Ok(1),
        )
        .expect("filter archive");
        let payload = serde_json::to_string(&result.conversation).expect("payload");

        assert!(payload.contains("[EMAIL_000001]"));
        assert!(payload.contains("[TOOL_CALL_000001]"));
        for forbidden in [
            "raw-conversation-id",
            "raw-source-id",
            "native-id",
            "raw-item-id",
            "raw-content-id",
            "call-id",
            "c2VjcmV0",
            "person@example.com",
        ] {
            assert!(!payload.contains(forbidden), "payload contains {forbidden}");
        }
        assert!(payload.contains("attachment.png"));
        assert_eq!(result.findings.len(), 3);
        assert_eq!(
            result.detector_observations.findings_by_detector,
            BTreeMap::from([
                (DetectorKind::OpenAiPrivacyFilter, 2),
                (DetectorKind::Structured, 1),
            ])
        );
    }

    #[test]
    fn residual_scan_masks_only_generated_placeholders() {
        let text = "before [PERSON_000123] [TOOL_CALL_000456] [SECRET] [NOT_A_PLACEHOLDER] after";
        let masked = mask_generated_placeholders(text);

        assert_eq!(masked.len(), text.len());
        assert!(!masked.contains("[PERSON_000123]"));
        assert!(!masked.contains("[TOOL_CALL_000456]"));
        assert!(!masked.contains("[SECRET]"));
        assert!(masked.contains("[NOT_A_PLACEHOLDER]"));
    }

    #[test]
    fn second_pass_finding_converges_with_original_offsets() {
        let conversation = ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "conversation".to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("source".to_string()),
            native_conversation_id: "native".to_string(),
            title: Some("Alice visits https://example.test".to_string()),
            project: None,
            started_at: None,
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        };
        let mut detectors = PrivacyDetectorSet::new(vec![Box::new(CascadingDetector { calls: 0 })]);
        let result = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |_, _| Ok(1),
        )
        .expect("second-pass finding should converge");

        assert_eq!(
            result.conversation.title.as_deref(),
            Some("[PERSON_000001] visits [URL_000001]")
        );
        assert_eq!(result.findings.len(), 2);
        assert!(result.findings.iter().any(|finding| {
            finding.field_path == "title"
                && finding.start == 0
                && finding.end == 5
                && finding.category == PrivacyCategory::Person
        }));
        assert!(result.findings.iter().any(|finding| {
            finding.field_path == "title"
                && finding.start == 13
                && finding.end == 33
                && finding.category == PrivacyCategory::Url
        }));
    }

    #[test]
    fn residual_failure_reports_only_safe_location_metadata_when_no_progress_is_possible() {
        let conversation = ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "conversation".to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("source".to_string()),
            native_conversation_id: "native".to_string(),
            title: Some("private".to_string()),
            project: None,
            started_at: None,
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        };
        let mut detectors =
            PrivacyDetectorSet::new(vec![Box::new(StubbornResidualDetector { calls: 0 })]);
        let error = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |_, _| Ok(1),
        )
        .expect_err("repeated finding over an existing replacement must fail closed");

        assert!(matches!(
            error,
            PrivacyError::ResidualFinding {
                ref field_path,
                start: 0,
                end: 7,
                detector: DetectorKind::OpenAiPrivacyFilter,
                category: PrivacyCategory::Person,
            } if field_path == "title"
        ));
    }

    #[test]
    fn archive_filter_always_replaces_authoritative_project_metadata() {
        let conversation = ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "conversation".to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("source".to_string()),
            native_conversation_id: "native".to_string(),
            title: None,
            project: Some(ProjectInfo {
                project_id: "project-id".to_string(),
                project_label: Some("AI".to_string()),
                repo_remote_hash: None,
                repo_label: Some("go".to_string()),
                branch_hash: None,
                branch_label: Some("main".to_string()),
                path_hash: None,
                path_label: Some("/private/tmp/AI".to_string()),
            }),
            started_at: None,
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: vec![ArchiveItem {
                item_id: "item".to_string(),
                native_item_id: None,
                source_record_id: None,
                ordinal: 0,
                kind: ArchiveItemKind::Message,
                role: Some(ArchiveRole::User),
                created_at: None,
                model: None,
                tool_name: None,
                tool_call_id: None,
                status: None,
                usage: None,
                parts_authoritative: true,
                parts: vec![ArchiveContentPart::text(
                    "part".to_string(),
                    0,
                    ArchiveContentKind::Text,
                    "AI uses go on main".to_string(),
                )],
            }],
        };
        let mut detectors = PrivacyDetectorSet::default();
        let result = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |_, _| Ok(1),
        )
        .expect("filter project metadata");
        let project = result.conversation.project.expect("filtered project");

        assert_eq!(project["name"], "[PROJECT_000001]");
        assert_eq!(project["repository"], "[REPOSITORY_000001]");
        assert_eq!(project["branch"], "[BRANCH_000001]");
        assert_eq!(project["path"], "[PATH_000001]");
        assert_eq!(
            result.conversation.items[0]["parts"][0]["text"],
            "AI uses go on main"
        );
        assert!(result
            .findings
            .iter()
            .any(|finding| finding.field_path == "project/branch"));
        assert_eq!(
            result.detector_observations.findings_by_detector,
            BTreeMap::from([(DetectorKind::Structured, 4)])
        );
    }

    #[test]
    fn tool_protocol_schema_and_pairing_are_preserved_for_provider_shapes() {
        let cases = [
            (
                "claude_code",
                r#"{"type":"tool_use","id":"call-private","name":"read","input":{"id":"customer-123","call_id":"business-call"}}"#,
                r#"{"type":"tool_result","tool_use_id":"call-private","content":"contents"}"#,
                "id",
                "tool_use_id",
            ),
            (
                "codex",
                r#"{"type":"function_call","call_id":"call-private","name":"read","arguments":{"id":"customer-123","call_id":"business-call"}}"#,
                r#"{"type":"function_call_output","call_id":"call-private","output":"contents"}"#,
                "call_id",
                "call_id",
            ),
            (
                "opencode",
                r#"{"type":"tool","id":"part-private","callID":"call-private","tool":"read","state":{"input":{"id":"customer-123","call_id":"business-call"}}}"#,
                r#"{"type":"tool","id":"result-private","callID":"call-private","state":{"output":"contents"}}"#,
                "callID",
                "callID",
            ),
            (
                "grok_build",
                r#"{"type":"tool_call","tool_call_id":"call-private","arguments":{"id":"customer-123","call_id":"business-call"}}"#,
                r#"{"type":"tool_result","tool_call_id":"call-private","content":"contents"}"#,
                "tool_call_id",
                "tool_call_id",
            ),
        ];

        for (provider, call, result, call_key, result_key) in cases {
            let conversation = tool_conversation(provider, call, result);
            let mut detectors = PrivacyDetectorSet::default();
            let filtered = filter_archive_conversation(
                &conversation,
                "dataset-key".to_string(),
                &mut detectors,
                |category, value| {
                    assert_eq!(category, PrivacyCategory::ToolCallId);
                    assert_eq!(value, "call-private");
                    Ok(7)
                },
            )
            .expect("filter provider tool fixture");
            let items = &filtered.conversation.items;
            let call_text = items[0]["parts"][0]["text"].as_str().expect("call text");
            let result_text = items[1]["parts"][0]["text"].as_str().expect("result text");
            let call_value: Value = serde_json::from_str(call_text).expect("call JSON");
            let result_value: Value = serde_json::from_str(result_text).expect("result JSON");

            assert_eq!(items[0]["tool_call_id"], "[TOOL_CALL_000007]");
            assert_eq!(items[1]["tool_call_id"], "[TOOL_CALL_000007]");
            assert_eq!(call_value[call_key], "[TOOL_CALL_000007]");
            assert_eq!(result_value[result_key], "[TOOL_CALL_000007]");
            assert_eq!(
                call_text,
                call.replace("call-private", "[TOOL_CALL_000007]")
            );
            assert_eq!(
                result_text,
                result.replace("call-private", "[TOOL_CALL_000007]")
            );
            assert_eq!(
                call_value
                    .pointer("/input/id")
                    .or_else(|| call_value.pointer("/arguments/id"))
                    .or_else(|| call_value.pointer("/state/input/id")),
                Some(&Value::String("customer-123".to_string()))
            );
            assert!(call_text.contains("business-call"));
            assert!(!call_text.contains("call-private"));
            assert!(!result_text.contains("call-private"));
            if provider == "opencode" {
                assert_eq!(call_value["id"], "part-private");
                assert_eq!(result_value["id"], "result-private");
            }
            assert_eq!(filtered.findings.len(), 4);
            assert_eq!(
                filtered.detector_observations.findings_by_detector,
                BTreeMap::from([(DetectorKind::Structured, 4)])
            );
        }
    }

    #[test]
    fn malformed_and_plain_tool_text_preserve_content_while_replacing_the_link_id() {
        let conversation = tool_conversation(
            "opencode",
            r#"{"call-private":"business","type":"tool","callID":"\u0063all-private","output":"partial"#,
            "completed call-private successfully",
        );
        let mut detectors = PrivacyDetectorSet::default();
        let filtered = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |category, value| {
                assert_eq!(category, PrivacyCategory::ToolCallId);
                assert_eq!(value, "call-private");
                Ok(9)
            },
        )
        .expect("filter malformed tool fixture");

        assert_eq!(
            filtered.conversation.items[0]["parts"][0]["text"],
            r#"{"call-private":"business","type":"tool","callID":"[TOOL_CALL_000009]","output":"partial"#
        );
        assert_eq!(
            filtered.conversation.items[1]["parts"][0]["text"],
            "completed [TOOL_CALL_000009] successfully"
        );
    }

    #[test]
    fn escaped_json_tool_ids_are_replaced_without_rewriting_the_payload() {
        let call = r#"{"type":"tool_use","id":"\u0063all-private","note":"prefix-\u0063all-private-suffix","call-private":"business"}"#;
        let result =
            r#"{"type":"tool_result","tool_use_id":"\u0063all-private","content":"contents"}"#;
        let conversation = tool_conversation("claude_code", call, result);
        let mut detectors = PrivacyDetectorSet::default();
        let filtered = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |category, value| {
                assert_eq!(category, PrivacyCategory::ToolCallId);
                assert_eq!(value, "call-private");
                Ok(12)
            },
        )
        .expect("filter escaped tool IDs");
        let call_text = filtered.conversation.items[0]["parts"][0]["text"]
            .as_str()
            .expect("call text");
        let result_text = filtered.conversation.items[1]["parts"][0]["text"]
            .as_str()
            .expect("result text");

        assert_eq!(
            call_text,
            call.replace(r"\u0063all-private", "[TOOL_CALL_000012]")
        );
        assert_eq!(
            result_text,
            result.replace(r"\u0063all-private", "[TOOL_CALL_000012]")
        );
        let call_value: Value = serde_json::from_str(call_text).expect("valid filtered call JSON");
        assert_eq!(call_value["id"], "[TOOL_CALL_000012]");
        assert_eq!(call_value["note"], "prefix-[TOOL_CALL_000012]-suffix");
        assert_eq!(call_value["call-private"], "business");
        assert_eq!(filtered.findings.len(), 5);
    }

    #[test]
    fn quote_and_backslash_tool_ids_are_replaced_as_json_values() {
        let tool_call_id = "call\"private\\id";
        let encoded = serde_json::to_string(tool_call_id).expect("encode tool ID");
        let call = format!(r#"{{"type":"function_call","call_id":{encoded}}}"#);
        let result = format!(r#"{{"type":"function_call_output","call_id":{encoded}}}"#);
        let conversation = tool_conversation_with_id("codex", tool_call_id, &call, &result);
        let mut detectors = PrivacyDetectorSet::default();
        let filtered = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            |category, value| {
                assert_eq!(category, PrivacyCategory::ToolCallId);
                assert_eq!(value, tool_call_id);
                Ok(13)
            },
        )
        .expect("filter escaped punctuation in tool ID");

        for item in &filtered.conversation.items {
            assert_eq!(item["tool_call_id"], "[TOOL_CALL_000013]");
            let text = item["parts"][0]["text"].as_str().expect("tool text");
            let value: Value = serde_json::from_str(text).expect("valid filtered tool JSON");
            assert_eq!(value["call_id"], "[TOOL_CALL_000013]");
        }
        assert_eq!(filtered.findings.len(), 4);
    }

    #[test]
    fn detector_spans_are_split_around_authoritative_tool_ids() {
        let mut detected = vec![DetectedSpan {
            start: 0,
            end: 20,
            category: PrivacyCategory::Secret,
            detector: DetectorKind::Kingfisher,
            confidence: Some(DetectionConfidence::High),
        }];
        let authoritative = vec![structured_tool_id_span(5, 15)];

        exclude_structured_ranges(&mut detected, &authoritative);

        assert_eq!(detected.len(), 2);
        assert_eq!((detected[0].start, detected[0].end), (0, 5));
        assert_eq!((detected[1].start, detected[1].end), (15, 20));
        assert!(detected.iter().all(|span| {
            span.category == PrivacyCategory::Secret && span.detector == DetectorKind::Kingfisher
        }));
    }

    #[test]
    fn observations_count_pre_merge_detector_overlap() {
        let findings = vec![vec![
            DetectedSpan {
                start: 0,
                end: 10,
                category: PrivacyCategory::Person,
                detector: DetectorKind::OpenAiPrivacyFilter,
                confidence: None,
            },
            DetectedSpan {
                start: 5,
                end: 12,
                category: PrivacyCategory::Secret,
                detector: DetectorKind::Kingfisher,
                confidence: Some(DetectionConfidence::High),
            },
        ]];
        let mut summary = DetectorObservationSummary::default();

        observe_detector_findings(&mut summary, &findings);

        assert_eq!(summary.detection_passes, 1);
        assert_eq!(summary.cross_detector_overlaps, 1);
        assert_eq!(
            summary.findings_by_detector,
            BTreeMap::from([
                (DetectorKind::OpenAiPrivacyFilter, 1),
                (DetectorKind::Kingfisher, 1),
            ])
        );
    }
}
