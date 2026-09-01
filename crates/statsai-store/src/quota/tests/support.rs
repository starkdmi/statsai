use super::*;
pub(super) use serde_json::json;
pub(super) use statsai_core::{
    event_id, Confidence, CostInfo, EventSource, IdentitySource, LocationOrigin, PrivacyInfo,
    PrivacyMode, QuotaCreditsV1, QuotaStatusV1, SessionInfo, SourceAccountAssignment,
    SourceAccountAssignmentId, SourceKind, SourceLocation, UsageCounts, UsageEvent,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION,
};
pub(super) use std::collections::HashSet;
pub(super) use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub(super) fn sample_record(
    source_id: SourceId,
    observation_id: &str,
    semantic_fingerprint: &str,
    observed_at: DateTime<Utc>,
    reset_epoch: i64,
    slot: &str,
    window_minutes: u64,
    used_percent: f64,
) -> QuotaObservationRecordV1 {
    let raw_rate_limits = serde_json::json!({
        slot: {
            "window_minutes": window_minutes,
            "used_percent": used_percent,
            "resets_at": reset_epoch
        },
        "credits": {"balance": "5.00"}
    });
    let payload_hash = hash_text(&serde_json::to_string(&raw_rate_limits).expect("payload"));
    QuotaObservationRecordV1 {
        observation: QuotaObservationV1 {
            schema_version: "quota_observation.v1".to_string(),
            observation_id: observation_id.to_string(),
            semantic_fingerprint: semantic_fingerprint.to_string(),
            provider: "codex".to_string(),
            source_id,
            provider_account_id: None,
            observed_at,
            source_file_path_hash: format!("file-{observation_id}"),
            source_record_id: format!("record-{observation_id}"),
            source_line_number: 1,
            payload_hash,
            usage_sample: None,
            usage_event_id: None,
            usage_link_kind: QuotaUsageLinkKind::None,
            status: QuotaStatusV1 {
                plan_type: Some("pro".to_string()),
                credits: QuotaCreditsV1 {
                    balance: Some("5".to_string()),
                    balance_raw: Some(serde_json::json!("5.00")),
                    ..QuotaCreditsV1::default()
                },
                ..QuotaStatusV1::default()
            },
        },
        windows: vec![QuotaWindowObservationV1 {
            schema_version: "quota_window_observation.v1".to_string(),
            window_observation_id: format!("window-{observation_id}"),
            observation_id: observation_id.to_string(),
            provider_slot: slot.to_string(),
            limit_id: Some("subscription".to_string()),
            window_minutes,
            used_percent,
            resets_at: DateTime::from_timestamp(reset_epoch, 0).expect("reset"),
            resets_at_epoch_seconds: reset_epoch,
        }],
        raw_rate_limits,
    }
}

pub(super) fn assigned_source(
    store: &Store,
    started_at: DateTime<Utc>,
) -> (SourceId, ProviderAccountId) {
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "test",
        Path::new("/tmp/quota-source"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let account_id = ProviderAccountId("account-codex".to_string());
    let now = Utc::now();
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: SourceAccountAssignmentId("assignment-quota".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::UserConfigured,
            verified_at: Some(now),
            created_at: now,
            updated_at: now,
        })
        .expect("assignment");
    (source.source_id, account_id)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sample_usage_event(
    source_id: &SourceId,
    account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    record_id: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    estimated_cost_micro_usd: i64,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id("codex", source_id, record_id, None, started_at),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        provider_account_id: Some(account_id.clone()),
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: Some("quota-event".to_string()),
            source_record_id: Some(record_id.to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: record_id.to_string(),
            local_session_id_hash: Some(record_id.to_string()),
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            cache_read_tokens: Some(cache_read_tokens),
            reasoning_tokens: Some(reasoning_tokens),
            total_tokens: Some(input_tokens + cache_read_tokens + output_tokens + reasoning_tokens),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: Some(estimated_cost_micro_usd),
            provider_reported_micro_usd: None,
            pricing_source: Some("test".to_string()),
            pricing_version: None,
            confidence: Confidence::High,
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
