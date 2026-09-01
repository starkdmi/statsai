use super::*;
use chrono::{DateTime, Utc};
use std::path::Path;
pub(super) fn test_source(provider: &str, path: &str) -> SourceLocation {
    SourceLocation::local_adapter(
        provider,
        "test",
        "0",
        Path::new(path),
        LocationOrigin::Configured,
    )
}

pub(super) fn test_event(
    provider: &str,
    source: &SourceLocation,
    started_at: DateTime<Utc>,
    tokens: u64,
    cost_cents: Option<i64>,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id(provider, &source.source_id, "rec", None, started_at),
        device_id: "d".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: None,
            source_record_id: Some("rec".to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "s".to_string(),
            local_session_id_hash: None,
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: Some(tokens / 2),
            output_tokens: Some(tokens / 2),
            total_tokens: Some(tokens),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: cost_cents,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
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
        created_at: started_at,
        imported_at: started_at,
    }
}

pub(super) fn test_summary(
    provider: &str,
    source: &SourceLocation,
    observed_at: DateTime<Utc>,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    tokens: u64,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(provider, &source.source_id, "sum"),
        device_id: "d".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalSummary,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "cache".to_string(),
            source_path_hash: None,
            source_record_id: Some("rec".to_string()),
            parse_confidence: Confidence::Medium,
        },
        model: None,
        models: Vec::new(),
        usage: UsageCounts {
            input_tokens: Some(tokens),
            total_tokens: Some(tokens),
            ..UsageCounts::default()
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: None,
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
        period_start: Some(period_start),
        period_end: Some(period_end),
        observed_at,
        metadata: SummaryMetadata {
            summary_format: "stats_cache".to_string(),
            summary_version: None,
            total_sessions: Some(1),
            total_messages: Some(10),
            last_computed_at: Some(observed_at),
        },
        imported_at: observed_at,
    }
}

pub(super) fn mk_dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
        .expect("valid date")
}
