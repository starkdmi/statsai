mod build;
mod redact;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use statsai_core::ArchiveConversation;

use crate::{
    filter_text, DetectedSpan, DetectionConfidence, DetectorKind, DetectorMetadata,
    PrivacyCategory, PrivacyDetectorSet, PrivacyError,
};

use self::build::{
    authoritative_project_field, collect_string_fields, filtered_from_projection,
    finding_from_replacement, hex_sha256, input_projection, observe_detector_findings,
    replace_string_fields,
};
use self::redact::{
    authoritative_tool_call_id, authoritative_tool_id_spans, exclude_structured_ranges,
    map_filtered_span_to_input, mask_generated_placeholders, mask_structured_spans, residual_error,
    MAX_FILTER_PASSES,
};

pub const FILTERED_CONVERSATION_SCHEMA_VERSION: &str = "filtered_conversation.v1";
pub const FILTERED_DATASET_SCHEMA_VERSION: &str = "filtered_dataset.v1";
const FILTER_POLICY_VERSION: &str = "privacy_policy.v4";

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

#[cfg(test)]
mod tests;
