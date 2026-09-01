use super::*;
pub(super) use crate::dedupe::model_key;
pub(super) use crate::rollups::{
    build_sync_rollup_summary, sanitize_summary_for_default_http_sync,
};
pub(super) use crate::sql::is_sqlite_busy_or_locked;
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
