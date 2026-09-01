use super::*;
pub(super) use crate::CURRENT_SCHEMA_VERSION;
pub(super) use chrono::Utc;
pub(super) use statsai_core::{
    event_id, summary_id, task_span_id, Confidence, CostInfo, EventSource, LocationOrigin,
    ModelInfo, PrivacyInfo, PrivacyMode, SessionInfo, SourceKind, SummaryMetadata, TaskSpan,
    UsageCounts, UsageEvent, UsageSummary, TASK_SPAN_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
pub(super) use std::path::Path;
pub(super) use std::sync::{Arc, Barrier};

pub(super) fn test_model(name: &str) -> ModelInfo {
    ModelInfo {
        name: Some(name.to_string()),
        normalized_name: Some(name.to_string()),
        provider_model_id: Some(name.to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    }
}

pub(super) fn parse_utc(value: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .expect("valid timestamp")
        .with_timezone(&chrono::Utc)
}

pub(super) fn test_source(path: &str) -> statsai_core::SourceLocation {
    statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new(path),
        LocationOrigin::Configured,
    )
}

pub(super) fn test_event(
    source: &statsai_core::SourceLocation,
    started_at: chrono::DateTime<Utc>,
    record_id: &str,
    model: &str,
    usage: UsageCounts,
    cost: CostInfo,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id("codex", &source.source_id, record_id, None, started_at),
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
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: Some(test_model(model)),
        usage,
        runtime: None,
        cost,
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: started_at,
        imported_at: started_at,
    }
}

pub(super) fn missing_cost() -> CostInfo {
    CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: None,
        provider_reported_usd: None,
        estimated_api_equivalent_micro_usd: None,
        provider_reported_micro_usd: None,
        pricing_source: Some("unknown".to_string()),
        pricing_version: None,
        confidence: Confidence::Low,
    }
}

pub(super) fn million_token_usage() -> UsageCounts {
    UsageCounts {
        input_tokens: Some(1_000_000),
        cache_creation_tokens: Some(1_000_000),
        cache_read_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        total_tokens: Some(4_000_000),
        ..UsageCounts::default()
    }
}

pub(super) fn expected_review_cost(started_at: chrono::DateTime<Utc>) -> CostInfo {
    estimate_cost_at(
        "codex",
        Some(&test_model("codex-auto-review")),
        &million_token_usage(),
        &started_at,
    )
}

pub(super) fn store_with_source(path: &str) -> (Store, statsai_core::SourceLocation) {
    let store = Store::in_memory().expect("store");
    let source = test_source(path);
    store.upsert_source(&source).expect("source");
    (store, source)
}

pub(super) fn stored_event(store: &Store, event_id: &str) -> UsageEvent {
    store
        .event_by_id(event_id)
        .expect("load event")
        .expect("event present")
}

pub(super) fn test_span(
    source: &statsai_core::SourceLocation,
    started_at: chrono::DateTime<Utc>,
    record_id: &str,
    linked_event_ids: Vec<statsai_core::EventId>,
    estimated_cost_usd: Option<i64>,
    estimated_cost_micro_usd: Option<i64>,
) -> TaskSpan {
    TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: task_span_id("codex", &source.source_id, record_id),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        span_kind: "codex_session".to_string(),
        source_record_id: Some(record_id.to_string()),
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some("session".to_string()),
        thread_id: None,
        title: "Review".to_string(),
        normalized_title: "review".to_string(),
        title_source: Some("summary".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: "none".to_string(),
        project: None,
        git: None,
        usage: million_token_usage(),
        estimated_cost_usd,
        estimated_cost_micro_usd,
        event_count: linked_event_ids.len() as u64,
        has_usage_evidence: !linked_event_ids.is_empty(),
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids,
        confidence: Confidence::Medium,
        is_meta: false,
        started_at,
        ended_at: Some(started_at),
        duration_seconds: Some(0),
    }
}

pub(super) fn test_summary(
    source: &statsai_core::SourceLocation,
    model: &str,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
    cost: CostInfo,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id("codex", &source.source_id, "period"),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "stats-cache.json".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some("period".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: Some(test_model(model)),
        models: Vec::new(),
        usage: million_token_usage(),
        cost,
        parse_evidence: None,
        project: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        metrics: None,
        period_start: Some(start),
        period_end: Some(end),
        observed_at: end,
        metadata: SummaryMetadata {
            summary_format: "grok_build_session_summary".to_string(),
            summary_version: Some("1".to_string()),
            total_sessions: Some(1),
            total_messages: Some(2),
            last_computed_at: Some(end),
        },
        imported_at: end,
    }
}
