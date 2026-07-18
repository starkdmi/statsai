use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::LazyLock;

use chrono::Datelike;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use statsai_core::ArchiveConversation;

use crate::{
    filter_text, DetectedSpan, DetectionConfidence, DetectorKind, DetectorMetadata,
    DeterministicDetector, PrivacyCategory, PrivacyDetector, PrivacyDetectorSet, PrivacyError,
    PrivacyReplacement,
};

pub const FILTERED_CONVERSATION_SCHEMA_VERSION: &str = "filtered_conversation.v1";
pub const FILTERED_DATASET_SCHEMA_VERSION: &str = "filtered_dataset.v1";
const FILTER_POLICY_VERSION: &str = "privacy_policy.v2";
const MAX_FILTER_PASSES: usize = 4;
static GENERATED_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[(?:SECRET|(?:ACCOUNT|ADDRESS|DATE|EMAIL|PERSON|PHONE|URL|PATH|HOST|IP|PROJECT|REPOSITORY|BRANCH)_\d{6})\]",
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
    structural: &mut DeterministicDetector,
    mut alias: impl FnMut(PrivacyCategory, &str) -> Result<u64, PrivacyError>,
) -> Result<(FilteredConversation, Vec<FilteredFieldFinding>, String), PrivacyError> {
    let input = input_projection(conversation);
    let input_fingerprint = archive_privacy_input_fingerprint(conversation)?;
    let mut fields = Vec::new();
    collect_string_fields(&input, "", &mut fields);
    let texts = fields.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    let mut detected = detectors.detect_batch(&texts)?;
    let structural_findings = structural.detect_batch(&texts)?;
    for (spans, additions) in detected.iter_mut().zip(structural_findings) {
        spans.extend(additions);
    }
    for ((path, text), spans) in fields.iter().zip(&mut detected) {
        if let Some(category) = authoritative_project_field(path, text) {
            spans.push(DetectedSpan {
                start: 0,
                end: text.len(),
                category,
                detector: DetectorKind::Deterministic,
                confidence: Some(DetectionConfidence::High),
            });
        }
    }
    drop(texts);

    let mut converged = None;
    for pass in 0..MAX_FILTER_PASSES {
        let mut filtered_fields = Vec::with_capacity(fields.len());
        for ((_, text), spans) in fields.iter().zip(&detected) {
            filtered_fields.push(filter_text(text, spans.clone(), &mut alias)?);
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
        let residual_structural = structural.detect_batch(&residual_texts)?;
        let mut first_residual = None;
        let mut additions = 0usize;
        for (index, (model, structural_spans)) in residual_model
            .into_iter()
            .zip(residual_structural)
            .enumerate()
        {
            for span in model.into_iter().chain(structural_spans) {
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
    Ok((filtered, findings, input_fingerprint))
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
    use crate::{DetectedSpan, DetectorKind, KnownPrivateValue, PrivacyDetector};

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
        let mut structural = DeterministicDetector::default();
        let (filtered, findings, _) = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            &mut structural,
            |_, _| Ok(1),
        )
        .expect("filter archive");
        let payload = serde_json::to_string(&filtered).expect("payload");

        assert!(payload.contains("[EMAIL_000001]"));
        for forbidden in [
            "raw-conversation-id",
            "raw-source-id",
            "native-id",
            "raw-item-id",
            "raw-content-id",
            "c2VjcmV0",
            "person@example.com",
        ] {
            assert!(!payload.contains(forbidden), "payload contains {forbidden}");
        }
        assert!(payload.contains("attachment.png"));
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn residual_scan_masks_only_generated_placeholders() {
        let text = "before [PERSON_000123] [SECRET] [NOT_A_PLACEHOLDER] after";
        let masked = mask_generated_placeholders(text);

        assert_eq!(masked.len(), text.len());
        assert!(!masked.contains("[PERSON_000123]"));
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
        let mut structural = DeterministicDetector::default();

        let (filtered, findings, _) = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            &mut structural,
            |_, _| Ok(1),
        )
        .expect("second-pass finding should converge");

        assert_eq!(
            filtered.title.as_deref(),
            Some("[PERSON_000001] visits [URL_000001]")
        );
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|finding| {
            finding.field_path == "title"
                && finding.start == 0
                && finding.end == 5
                && finding.category == PrivacyCategory::Person
        }));
        assert!(findings.iter().any(|finding| {
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
        let mut structural = DeterministicDetector::default();

        let error = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            &mut structural,
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
        let mut structural = DeterministicDetector::new(vec![
            KnownPrivateValue {
                category: PrivacyCategory::Project,
                value: "AI".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Repository,
                value: "go".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Branch,
                value: "main".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Path,
                value: "/private/tmp/AI".to_string(),
            },
        ]);

        let (filtered, findings, _) = filter_archive_conversation(
            &conversation,
            "dataset-key".to_string(),
            &mut detectors,
            &mut structural,
            |_, _| Ok(1),
        )
        .expect("filter project metadata");
        let project = filtered.project.expect("filtered project");

        assert_eq!(project["name"], "[PROJECT_000001]");
        assert_eq!(project["repository"], "[REPOSITORY_000001]");
        assert_eq!(project["branch"], "[BRANCH_000001]");
        assert_eq!(project["path"], "[PATH_000001]");
        assert_eq!(
            filtered.items[0]["parts"][0]["text"],
            "[PROJECT_000001] uses [REPOSITORY_000001] on main"
        );
        assert!(findings
            .iter()
            .any(|finding| finding.field_path == "project/branch"));
    }
}
