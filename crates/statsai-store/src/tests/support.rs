use super::*;
pub(super) use crate::dedupe::model_key;
pub(super) use crate::rollups::build_sync_rollup_summary;
pub(super) use crate::sql::is_sqlite_busy_or_locked;
pub(super) use crate::sync_payload::sanitize_summary_for_sync_with_projects;
pub(super) use crate::verified::upsert_verified_source_assignment;
// Task spans still key on the remote-inclusive bucket; rollups do not.
pub(super) use chrono::{TimeZone, Utc};
pub(super) use statsai_core::project_bucket_key;
pub(super) use statsai_core::{
    event_id, summary_id, Confidence, CostInfo, EventSource, LocationOrigin, ModelInfo,
    ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, ReasoningLevel, SessionInfo, SourceKind,
    SummaryMetadata, UsageCounts, UsageSummary, SYNC_BATCH_SCHEMA_VERSION,
    USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
};
pub(super) use std::path::Path;

pub(super) fn test_store_event(
    source: &statsai_core::SourceLocation,
    now: chrono::DateTime<Utc>,
    record_id: &str,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id("codex", &source.source_id, record_id, None, now),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(record_id.to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "session".to_string(),
            local_session_id_hash: Some("same-session".to_string()),
            title: None,
            started_at: now,
            ended_at: None,
            duration_seconds: None,
            turn_started_at: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: Some(12),
            output_tokens: Some(3),
            total_tokens: Some(15),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: Some("unknown".to_string()),
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: now,
        imported_at: now,
        context: None,
    }
}

pub(super) fn test_store_summary(
    source: &statsai_core::SourceLocation,
    now: chrono::DateTime<Utc>,
    total: u64,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(&source.provider, &source.source_id, "summary"),
        device_id: "device".to_string(),
        provider: source.provider.clone(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalSummary,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "stats-cache.json".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some("summary".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: Some(ModelInfo {
            name: Some("claude-test".to_string()),
            normalized_name: Some("claude-test".to_string()),
            provider_model_id: Some("claude-test".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }),
        models: Vec::new(),
        usage: UsageCounts {
            input_tokens: Some(total),
            total_tokens: Some(total),
            ..UsageCounts::default()
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: Some("unknown".to_string()),
            pricing_version: None,
            confidence: Confidence::Low,
        },
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(now - chrono::Duration::days(1)),
        period_end: Some(now),
        observed_at: now,
        metadata: SummaryMetadata {
            summary_format: "test".to_string(),
            summary_version: Some("1".to_string()),
            total_sessions: Some(1),
            total_messages: Some(2),
            last_computed_at: Some(now),
        },
        imported_at: now,
    }
}

pub(crate) fn task_span(
    source: &statsai_core::SourceLocation,
    raw_session_id: &str,
    title: &str,
    started_at: chrono::DateTime<Utc>,
) -> statsai_core::TaskSpan {
    statsai_core::TaskSpan {
        schema_version: statsai_core::TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: statsai_core::TaskSpanId(format!("span-{raw_session_id}-{title}")),
        provider: source.provider.clone(),
        source_id: source.source_id.clone(),
        span_kind: "test".to_string(),
        source_record_id: None,
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some(raw_session_id.to_string()),
        thread_id: None,
        title: title.to_string(),
        normalized_title: statsai_core::normalize_task_title(title),
        title_source: Some("test".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: "bucket".to_string(),
        project: None,
        git: None,
        usage: UsageCounts::default(),
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        event_count: 0,
        has_usage_evidence: false,
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids: Vec::new(),
        confidence: Confidence::Medium,
        is_meta: statsai_core::task_title_is_generic(Some(title)),
        started_at,
        ended_at: Some(started_at),
        duration_seconds: Some(0),
    }
}
