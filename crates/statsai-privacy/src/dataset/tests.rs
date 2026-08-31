use super::*;
use chrono::Utc;
use statsai_core::{
    ArchiveCompleteness, ArchiveContentKind, ArchiveContentPart, ArchiveConversation, ArchiveItem,
    ArchiveItemKind, ArchiveRole, ProjectInfo, SourceId, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};

use super::redact::structured_tool_id_span;
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
    let result = r#"{"type":"tool_result","tool_use_id":"\u0063all-private","content":"contents"}"#;
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
