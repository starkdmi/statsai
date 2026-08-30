use super::super::{canonical_display, hash_text};
use super::*;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use serde_json::Value;
use statsai_core::{
    archive_artifact_metadata_signature, archive_content_id, archive_item_id,
    ArchiveArtifactDependency, ArchiveContentKind, ArchiveContentPart, ArchiveItem,
    ArchiveItemKind, ArchiveRole, ModelInfo, UsageCounts,
};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use url::Url;

pub(crate) struct ItemInput<'a> {
    pub(crate) provider: &'a str,
    pub(crate) conversation_native_id: &'a str,
    pub(crate) native_item_id: &'a str,
    pub(crate) source_record_id: &'a str,
    pub(crate) ordinal: u64,
    pub(crate) kind: ArchiveItemKind,
    pub(crate) role: Option<ArchiveRole>,
    pub(crate) created_at: Option<DateTime<Utc>>,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) tool_name: Option<&'a str>,
    pub(crate) tool_call_id: Option<&'a str>,
    pub(crate) status: Option<&'a str>,
    pub(crate) usage: Option<UsageCounts>,
    pub(crate) content: &'a Value,
}

pub(crate) fn item_from_value(input: ItemInput<'_>) -> (ArchiveItem, u64) {
    // Rendered once and reused: the identifier hashes the same JSON that a
    // tool call or result goes on to store, and rendering a large tool payload
    // twice is pure duplicate work.
    let rendered = input.content.to_string();
    let fingerprint = hash_text(&rendered);
    let item_id = archive_item_id(
        input.provider,
        input.conversation_native_id,
        Some(input.native_item_id),
        input.ordinal,
        &fingerprint,
    );
    let mut missing = 0;
    let mut parts = Vec::new();
    let materialize_local_artifacts = local_artifacts_allowed(input.kind, input.role);
    if matches!(
        input.kind,
        ArchiveItemKind::ToolCall | ArchiveItemKind::ToolResult
    ) {
        if !input.content.is_null() {
            let text = match input.content {
                Value::String(value) => value.clone(),
                _ => rendered.clone(),
            };
            if !text.trim().is_empty() && text != "null" {
                push_text_part(&item_id, ArchiveContentKind::Json, text, &mut parts);
            }
        }
        extract_binary_content_parts(
            input.content,
            &item_id,
            &mut parts,
            &mut missing,
            materialize_local_artifacts,
        );
    } else {
        extract_content_parts(
            input.content,
            &item_id,
            &mut parts,
            &mut missing,
            materialize_local_artifacts,
        );
    }
    if parts.is_empty() && missing > 0 {
        push_text_part(
            &item_id,
            ArchiveContentKind::Json,
            r#"{"omitted_content":"invalid, unavailable, or oversized artifact"}"#.to_string(),
            &mut parts,
        );
    } else if parts.is_empty() && !input.content.is_null() {
        let text = match input.content {
            Value::String(value) => value.clone(),
            _ => rendered,
        };
        if !text.trim().is_empty() && text != "null" {
            push_text_part(&item_id, ArchiveContentKind::Json, text, &mut parts);
        }
    }
    match input.kind {
        ArchiveItemKind::ToolCall => {
            bound_text_parts(&mut parts, MAX_TOOL_CALL_TEXT_BYTES, 0);
        }
        ArchiveItemKind::ToolResult => {
            bound_text_parts(
                &mut parts,
                MAX_TOOL_RESULT_TEXT_BYTES,
                TOOL_RESULT_TAIL_BYTES,
            );
        }
        ArchiveItemKind::Message
        | ArchiveItemKind::ReasoningSummary
        | ArchiveItemKind::Artifact => {}
    }
    let parts_authoritative = missing == 0 && parts.iter().all(|part| !part.truncated);
    (
        ArchiveItem {
            item_id,
            native_item_id: Some(input.native_item_id.to_string()),
            source_record_id: Some(input.source_record_id.to_string()),
            ordinal: input.ordinal,
            kind: input.kind,
            role: input.role,
            created_at: input.created_at,
            model: input.model,
            tool_name: input.tool_name.map(ToOwned::to_owned),
            tool_call_id: input.tool_call_id.map(ToOwned::to_owned),
            status: input.status.map(ToOwned::to_owned),
            usage: input.usage,
            parts_authoritative,
            parts,
        },
        missing,
    )
}

pub(crate) fn local_artifacts_allowed(kind: ArchiveItemKind, role: Option<ArchiveRole>) -> bool {
    role == Some(ArchiveRole::User)
        && matches!(kind, ArchiveItemKind::Message | ArchiveItemKind::Artifact)
}

pub(crate) fn extract_binary_content_parts(
    value: &Value,
    item_id: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
    materialize_local_artifacts: bool,
) {
    match value {
        Value::String(value) => {
            if let Some((mime_type, encoded)) = parse_data_url(value) {
                push_binary_part(item_id, mime_type, None, encoded, parts, missing);
            }
        }
        Value::Array(values) => {
            for value in values {
                extract_binary_content_parts(
                    value,
                    item_id,
                    parts,
                    missing,
                    materialize_local_artifacts,
                );
            }
        }
        Value::Object(object) => {
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if let Some(source) = object.get("source").and_then(Value::as_object) {
                if source.get("type").and_then(Value::as_str) == Some("base64") {
                    let mime_type = artifact_mime_type(object, content_type, None);
                    if let Some(data) = source.get("data").and_then(Value::as_str) {
                        push_binary_part(
                            item_id,
                            &mime_type,
                            object.get("name").and_then(Value::as_str),
                            data,
                            parts,
                            missing,
                        );
                        return;
                    }
                    *missing += 1;
                    return;
                }
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(artifact) = artifact_reference(object) {
                    if let Some((mime_type, encoded)) = parse_data_url(artifact) {
                        push_binary_part(
                            item_id,
                            mime_type,
                            object.get("name").and_then(Value::as_str),
                            encoded,
                            parts,
                            missing,
                        );
                    } else {
                        let bytes = materialize_local_artifacts
                            .then(|| read_explicit_local_artifact(artifact))
                            .flatten();
                        if let Some(bytes) = bytes {
                            let mime_type =
                                artifact_mime_type(object, content_type, Some(artifact));
                            push_binary_bytes(
                                item_id,
                                &mime_type,
                                object.get("name").and_then(Value::as_str),
                                &bytes,
                                parts,
                            );
                        } else {
                            push_external_part(item_id, content_type, artifact, parts);
                            *missing += 1;
                        }
                    }
                    return;
                }
                *missing += 1;
                return;
            }
            for value in object.values() {
                extract_binary_content_parts(
                    value,
                    item_id,
                    parts,
                    missing,
                    materialize_local_artifacts,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

pub(crate) fn extract_content_parts(
    value: &Value,
    item_id: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
    materialize_local_artifacts: bool,
) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
        Value::String(text) => {
            if let Some((mime_type, encoded)) = parse_data_url(text) {
                push_binary_part(item_id, mime_type, None, encoded, parts, missing);
            } else if !text.trim().is_empty() {
                push_text_part(item_id, ArchiveContentKind::Text, text.clone(), parts);
            }
        }
        Value::Array(values) => {
            for value in values {
                extract_content_parts(value, item_id, parts, missing, materialize_local_artifacts);
            }
        }
        Value::Object(object) => {
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if let Some(source) = object.get("source").and_then(Value::as_object) {
                if source.get("type").and_then(Value::as_str) == Some("base64") {
                    let mime_type = artifact_mime_type(object, content_type, None);
                    if let Some(data) = source.get("data").and_then(Value::as_str) {
                        push_binary_part(
                            item_id,
                            &mime_type,
                            object.get("name").and_then(Value::as_str),
                            data,
                            parts,
                            missing,
                        );
                        return;
                    }
                    *missing += 1;
                    return;
                }
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(value) = artifact_reference(object) {
                    if let Some((mime_type, encoded)) = parse_data_url(value) {
                        push_binary_part(
                            item_id,
                            mime_type,
                            object.get("name").and_then(Value::as_str),
                            encoded,
                            parts,
                            missing,
                        );
                    } else {
                        let bytes = materialize_local_artifacts
                            .then(|| read_explicit_local_artifact(value))
                            .flatten();
                        if let Some(bytes) = bytes {
                            let mime_type = artifact_mime_type(object, content_type, Some(value));
                            push_binary_bytes(
                                item_id,
                                &mime_type,
                                object.get("name").and_then(Value::as_str),
                                &bytes,
                                parts,
                            );
                        } else {
                            push_external_part(item_id, content_type, value, parts);
                            *missing += 1;
                        }
                    }
                    return;
                }
                *missing += 1;
                return;
            }
            if matches!(content_type, "text" | "input_text" | "output_text") {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    push_text_part(item_id, ArchiveContentKind::Text, text.to_string(), parts);
                    return;
                }
            }
            if matches!(content_type, "thinking" | "reasoning" | "reasoning_summary") {
                for key in ["text", "thinking", "summary"] {
                    if let Some(value) = object.get(key) {
                        extract_content_parts(
                            value,
                            item_id,
                            parts,
                            missing,
                            materialize_local_artifacts,
                        );
                    }
                }
                return;
            }
            if matches!(content_type, "tool_use" | "tool_call" | "tool_result") {
                // Serialized borrowed: cloning the block first copied every
                // nested value only to render and drop it.
                let compact = serde_json::to_string(object).unwrap_or_default();
                push_text_part(item_id, ArchiveContentKind::Json, compact, parts);
                return;
            }
            for key in ["text", "content", "message", "output"] {
                if let Some(value) = object.get(key) {
                    extract_content_parts(
                        value,
                        item_id,
                        parts,
                        missing,
                        materialize_local_artifacts,
                    );
                }
            }
        }
    }
}

pub(crate) fn artifact_reference(object: &serde_json::Map<String, Value>) -> Option<&str> {
    ["image_url", "file_data", "url", "data", "source"]
        .into_iter()
        .filter_map(|key| object.get(key))
        .find_map(|value| match value {
            Value::String(value) => Some(value.as_str()),
            Value::Object(value) => value
                .get("url")
                .or_else(|| value.get("data"))
                .and_then(Value::as_str),
            _ => None,
        })
}

pub(crate) fn artifact_mime_type(
    object: &serde_json::Map<String, Value>,
    content_type: &str,
    reference: Option<&str>,
) -> String {
    if let Some(mime_type) = object
        .get("mime_type")
        .or_else(|| object.get("media_type"))
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("source")
                .and_then(Value::as_object)
                .and_then(|source| source.get("media_type"))
                .and_then(Value::as_str)
        })
    {
        return mime_type.to_string();
    }
    if let Some(mime_type) = reference
        .and_then(mime_type_from_artifact_reference)
        .or_else(|| {
            object
                .get("name")
                .and_then(Value::as_str)
                .and_then(mime_type_from_path)
        })
    {
        return mime_type.to_string();
    }
    if content_type.contains("image") {
        "image/unknown".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

pub(crate) fn mime_type_from_artifact_reference(reference: &str) -> Option<&'static str> {
    let path = if reference.starts_with("file:") {
        Url::parse(reference).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(reference)
    };
    mime_type_from_path(path.to_str()?)
}

pub(crate) fn mime_type_from_path(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "avif" => Some("image/avif"),
        "bmp" => Some("image/bmp"),
        "gif" => Some("image/gif"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "ico" => Some("image/x-icon"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "svg" | "svgz" => Some("image/svg+xml"),
        "tif" | "tiff" => Some("image/tiff"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub(crate) fn collect_artifact_dependencies(
    value: &Value,
    candidate_path: &Path,
    dependencies: &mut ArtifactDependencyMap,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_artifact_dependencies(value, candidate_path, dependencies);
            }
        }
        Value::Object(object) => {
            let is_embedded_base64 = object
                .get("source")
                .and_then(Value::as_object)
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str)
                == Some("base64");
            let content_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if is_embedded_base64 {
                return;
            }
            if matches!(
                content_type,
                "image" | "input_image" | "file" | "input_file"
            ) {
                if let Some(path) =
                    artifact_reference(object).and_then(explicit_local_artifact_path)
                {
                    let signature = archive_artifact_metadata_signature(&path);
                    dependencies.insert((canonical_display(candidate_path), path), signature);
                }
                return;
            }
            if matches!(
                content_type,
                "text" | "input_text" | "output_text" | "tool_use" | "tool_call" | "tool_result"
            ) {
                return;
            }
            let keys = if matches!(content_type, "thinking" | "reasoning" | "reasoning_summary") {
                &["text", "thinking", "summary"][..]
            } else {
                &["text", "content", "message", "output"][..]
            };
            for value in keys.iter().filter_map(|key| object.get(*key)) {
                collect_artifact_dependencies(value, candidate_path, dependencies);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(crate) fn finish_artifact_dependencies(
    dependencies: ArtifactDependencyMap,
) -> Vec<ArchiveArtifactDependency> {
    dependencies
        .into_iter()
        .map(
            |((cache_key, path), metadata_signature)| ArchiveArtifactDependency {
                cache_key,
                path,
                metadata_signature,
            },
        )
        .collect()
}

pub(crate) fn push_text_part(
    item_id: &str,
    kind: ArchiveContentKind,
    text: String,
    parts: &mut Vec<ArchiveContentPart>,
) {
    if text.trim().is_empty() {
        return;
    }
    let ordinal = parts.len() as u64;
    parts.push(ArchiveContentPart::text(
        archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        text,
    ));
}

pub(crate) fn bound_text_parts(
    parts: &mut Vec<ArchiveContentPart>,
    max_bytes: usize,
    tail_bytes: usize,
) {
    let text_values = parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>();
    let total_bytes = text_values.iter().map(|text| text.len()).sum::<usize>();
    if total_bytes <= max_bytes {
        return;
    }

    let original = text_values.join("\n");
    let Some(first_text_index) = parts.iter().position(|part| part.text.is_some()) else {
        return;
    };
    let marker = if tail_bytes == 0 {
        "\n[truncated]"
    } else {
        "\n[... truncated ...]\n"
    };
    let content_budget = max_bytes.saturating_sub(marker.len());
    let retained_tail_bytes = tail_bytes.min(content_budget);
    let retained_head_bytes = content_budget.saturating_sub(retained_tail_bytes);
    let head_end = previous_char_boundary(&original, retained_head_bytes);
    let tail_start = next_char_boundary(
        &original,
        original.len().saturating_sub(retained_tail_bytes),
    );
    let bounded = if retained_tail_bytes == 0 {
        format!("{}{}", &original[..head_end], marker)
    } else {
        format!(
            "{}{}{}",
            &original[..head_end],
            marker,
            &original[tail_start..]
        )
    };
    let first = &mut parts[first_text_index];
    first.text = Some(bounded);
    first.content_hash = hash_text(&original);
    first.original_bytes = total_bytes as u64;
    first.truncated = true;

    let mut retained_first_text = false;
    parts.retain(|part| {
        if part.text.is_none() {
            return true;
        }
        if retained_first_text {
            false
        } else {
            retained_first_text = true;
            true
        }
    });
}

pub(crate) fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) fn next_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub(crate) fn push_binary_part(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    encoded: &str,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
) {
    push_binary_part_with_limit(
        item_id,
        mime_type,
        name,
        encoded,
        MAX_ARTIFACT_BYTES,
        parts,
        missing,
    );
}

pub(crate) fn push_binary_part_with_limit(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    encoded: &str,
    max_bytes: u64,
    parts: &mut Vec<ArchiveContentPart>,
    missing: &mut u64,
) {
    let Some(expected_bytes) = decoded_base64_len(encoded) else {
        *missing += 1;
        return;
    };
    if expected_bytes > max_bytes {
        *missing += 1;
        return;
    }
    let Ok(bytes) = BASE64.decode(encoded) else {
        *missing += 1;
        return;
    };
    if bytes.len() as u64 != expected_bytes || bytes.len() as u64 > max_bytes {
        *missing += 1;
        return;
    }
    let ordinal = parts.len() as u64;
    let kind = content_kind_for_mime(mime_type);
    parts.push(ArchiveContentPart::binary_bytes(
        archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        Some(mime_type.to_string()),
        name.map(ToOwned::to_owned),
        &bytes,
    ));
}

pub(crate) fn decoded_base64_len(encoded: &str) -> Option<u64> {
    let encoded_len = u64::try_from(encoded.len()).ok()?;
    if encoded_len % 4 != 0 {
        return None;
    }
    let padding = if encoded.ends_with("==") {
        2
    } else if encoded.ends_with('=') {
        1
    } else {
        0
    };
    encoded_len
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

pub(crate) fn push_binary_bytes(
    item_id: &str,
    mime_type: &str,
    name: Option<&str>,
    bytes: &[u8],
    parts: &mut Vec<ArchiveContentPart>,
) {
    let ordinal = parts.len() as u64;
    parts.push(ArchiveContentPart::binary_bytes(
        archive_content_id(item_id, ordinal),
        ordinal,
        content_kind_for_mime(mime_type),
        Some(mime_type.to_string()),
        name.map(ToOwned::to_owned),
        bytes,
    ));
}

pub(crate) fn push_external_part(
    item_id: &str,
    content_type: &str,
    uri: &str,
    parts: &mut Vec<ArchiveContentPart>,
) {
    let ordinal = parts.len() as u64;
    let kind = if content_type.contains("image") {
        ArchiveContentKind::Image
    } else {
        ArchiveContentKind::File
    };
    parts.push(ArchiveContentPart {
        content_id: archive_content_id(item_id, ordinal),
        ordinal,
        kind,
        mime_type: None,
        name: None,
        text: None,
        data_base64: None,
        external_uri: Some(uri.to_string()),
        content_hash: hash_text(uri),
        original_bytes: 0,
        truncated: false,
    });
}

pub(crate) fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("data:")?;
    let (metadata, encoded) = value.split_once(',')?;
    let mime_type = metadata.strip_suffix(";base64")?;
    Some((mime_type, encoded))
}

pub(crate) fn read_explicit_local_artifact(value: &str) -> Option<Vec<u8>> {
    let path = explicit_local_artifact_path(value)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_ARTIFACT_BYTES).then_some(bytes)
}

pub(crate) fn explicit_local_artifact_path(value: &str) -> Option<PathBuf> {
    if value.starts_with("file:") {
        return Url::parse(value).ok()?.to_file_path().ok();
    }
    Path::new(value).is_absolute().then(|| PathBuf::from(value))
}

pub(crate) fn content_kind_for_mime(mime_type: &str) -> ArchiveContentKind {
    if mime_type.starts_with("image/") {
        ArchiveContentKind::Image
    } else if mime_type.starts_with("audio/") {
        ArchiveContentKind::Audio
    } else {
        ArchiveContentKind::File
    }
}

pub(crate) fn value_has_readable_content(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(value_has_readable_content),
        Value::Object(object) => {
            object.get("source").is_some()
                || object.get("image_url").is_some()
                || [
                    "text", "content", "message", "output", "summary", "thinking",
                ]
                .into_iter()
                .filter_map(|key| object.get(key))
                .any(value_has_readable_content)
        }
        Value::Bool(_) | Value::Number(_) => false,
    }
}

pub(crate) fn native_id_from_value(value: &Value) -> Option<String> {
    ["id", "uuid", "message_id", "messageId"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

pub(crate) fn item_fingerprint(item: &ArchiveItem) -> String {
    hash_text(
        &item
            .parts
            .iter()
            .map(|part| part.content_hash.as_str())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

pub(crate) fn timestamp_from_epoch(value: i64) -> Option<DateTime<Utc>> {
    let millis = if value.abs() < 10_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    };
    DateTime::from_timestamp_millis(millis)
}
