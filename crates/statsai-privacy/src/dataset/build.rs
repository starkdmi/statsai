use std::collections::BTreeMap;

use chrono::Datelike;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use statsai_core::ArchiveConversation;

use crate::{DetectedSpan, PrivacyCategory, PrivacyError, PrivacyReplacement};

use super::{
    DetectorObservationSummary, FilteredConversation, FilteredFieldFinding,
    FILTERED_CONVERSATION_SCHEMA_VERSION,
};

pub(super) fn observe_detector_findings(
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

pub(super) fn authoritative_project_field(path: &str, value: &str) -> Option<PrivacyCategory> {
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

pub(super) fn input_projection(conversation: &ArchiveConversation) -> Value {
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

pub(super) fn filtered_from_projection(
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

pub(super) fn collect_string_fields<'a>(
    value: &'a Value,
    path: &str,
    output: &mut Vec<(String, &'a str)>,
) {
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

pub(super) fn replace_string_fields(
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

pub(super) fn finding_from_replacement(
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

pub(super) fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
