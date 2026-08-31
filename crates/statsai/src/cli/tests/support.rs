use super::*;

pub(super) use chrono::{DateTime, Duration, TimeZone};

pub(super) use statsai_core::{
    branch_family, event_id, hash_text, normalize_task_title, project_bucket_key, subscription_id,
    summary_id, task_span_id, BillingPeriod, Confidence, CostInfo, EventSource, IdentitySource,
    ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, ProviderAccount, SessionInfo,
    SourceKind, Subscription, SubscriptionStatus, SummaryMetadata, TaskBucketSnapshot, TaskSpan,
    TaskSpanId, TaskStatus, TaskVerdict, TaskVerification, TaskVerificationAction,
    TaskVerificationId, UsageCounts, UsageSummary, WorkItem, WorkItemId, WorkItemMember,
    PROVIDER_ACCOUNT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION,
    TASK_VERIFICATION_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    WORK_ITEM_SCHEMA_VERSION,
};

pub(super) use std::path::Path;

pub(super) use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct TestAdapter {
    pub(super) provider: &'static str,
    pub(super) discovered: Vec<SourceLocation>,
    pub(super) candidates: Vec<ScanCandidateFile>,
    pub(super) scan_result: statsai_adapters::AdapterScan,
    pub(super) probe_result: Option<VerifiedSourceState>,
    pub(super) scan_calls: Option<Arc<Mutex<u64>>>,
}

impl ProviderAdapter for TestAdapter {
    fn id(&self) -> &'static str {
        "test"
    }

    fn version(&self) -> &'static str {
        "0"
    }

    fn provider(&self) -> &'static str {
        self.provider
    }

    fn discover(&self) -> Vec<SourceLocation> {
        self.discovered.clone()
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(self.candidates.clone())
    }

    fn probe_verified_source_state(
        &self,
        _source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        Ok(self
            .probe_result
            .clone()
            .or_else(|| self.scan_result.verified_source_state.clone())
            .map(Box::new)
            .map(VerifiedSourceObservation::Verified)
            .unwrap_or(VerifiedSourceObservation::Unavailable))
    }

    fn scan(
        &self,
        _source: &SourceLocation,
        _options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        if let Some(scan_calls) = &self.scan_calls {
            let mut calls = scan_calls.lock().expect("scan call mutex");
            *calls += 1;
        }
        Ok(self.scan_result.clone())
    }
}

pub(super) fn test_sync_command(sink: &str) -> SyncCommand {
    SyncCommand {
        sink: sink.to_string(),
        output: None,
        endpoint: None,
        auth_token: None,
        rebuild_rollups: false,
        full: false,
        since_last: false,
        status: false,
        verify: false,
        reset_remote: false,
        yes: false,
        dry_run: false,
        include_projects: false,
        exclude_projects: false,
        include_tasks: false,
        exclude_tasks: false,
    }
}

pub(super) struct TokenParts {
    pub(super) input: u64,
    pub(super) cached_input: u64,
    pub(super) output: u64,
    pub(super) reasoning: u64,
    pub(super) total: u64,
    pub(super) cost: Option<i64>, // cents
}

impl TokenParts {
    pub(super) fn total(total: u64) -> Self {
        Self {
            input: 0,
            cached_input: 0,
            output: 0,
            reasoning: 0,
            total,
            cost: None,
        }
    }
}

pub(super) fn test_account(
    provider: &str,
    label: Option<&str>,
    email: Option<&str>,
    provider_user_id: Option<&str>,
    plan_name: Option<&str>,
    now: DateTime<Utc>,
) -> ProviderAccount {
    let provider_account_id = provider_account_id_from_identity(provider, provider_user_id, email)
        .unwrap_or_else(|| provider_account_id(provider, label.expect("label")));
    let normalized_email = email.map(normalize_email);
    ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id,
        provider: provider.to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: provider_user_id.map(ToOwned::to_owned),
        provider_user_id_hash: provider_user_id.map(hash_text),
        email_hash: normalized_email.as_deref().map(hash_text),
        email: normalized_email,
        org_id_hash: None,
        account_label: label.map(ToOwned::to_owned),
        plan_name: plan_name.map(ToOwned::to_owned),
        confidence: if email.is_some() || provider_user_id.is_some() {
            Confidence::High
        } else {
            Confidence::Medium
        },
        verified_at: email.map(|_| now),
        created_at: now,
        updated_at: now,
    }
}

pub(super) fn test_assignment(
    source: &SourceLocation,
    provider_account_id: &statsai_core::ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> SourceAccountAssignment {
    SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(
            &source.source_id,
            provider_account_id,
            started_at,
        ),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: provider_account_id.clone(),
        started_at,
        ended_at,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        created_at: now,
        updated_at: now,
    }
}

pub(super) fn test_event(
    provider: &str,
    source: &SourceLocation,
    started_at: DateTime<Utc>,
    provider_account_id: Option<statsai_core::ProviderAccountId>,
    tokens: TokenParts,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id(
            provider,
            &source.source_id,
            &started_at.to_rfc3339(),
            None,
            started_at,
        ),
        device_id: "device".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(started_at.to_rfc3339()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "session".to_string(),
            local_session_id_hash: None,
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: (tokens.input > 0).then_some(tokens.input),
            output_tokens: (tokens.output > 0).then_some(tokens.output),
            cache_read_tokens: (tokens.cached_input > 0).then_some(tokens.cached_input),
            reasoning_tokens: (tokens.reasoning > 0).then_some(tokens.reasoning),
            total_tokens: Some(tokens.total),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: tokens.cost,
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
        created_at: started_at,
        imported_at: started_at,
    }
}

pub(super) fn test_summary(
    provider: &str,
    source: &SourceLocation,
    now: DateTime<Utc>,
    total: u64,
    provider_account_id: Option<statsai_core::ProviderAccountId>,
) -> UsageSummary {
    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(provider, &source.source_id, "summary"),
        device_id: "device".to_string(),
        provider: provider.to_string(),
        source_id: source.source_id.clone(),
        provider_account_id,
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
        period_start: Some(now - Duration::days(30)),
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

pub(super) fn test_scan_candidate(path: &str, cache_signature: &str) -> ScanCandidateFile {
    ScanCandidateFile {
        path: PathBuf::from(path),
        cache_key: path.to_string(),
        cache_signature: cache_signature.to_string(),
        compatible_cache_signatures: Vec::new(),
    }
}

pub(super) fn test_scan_event(
    source: &SourceLocation,
    file_path: &str,
    started_at: DateTime<Utc>,
    record_id: &str,
    total_tokens: u64,
) -> UsageEvent {
    let mut event = test_event(
        "codex",
        source,
        started_at,
        None,
        TokenParts::total(total_tokens),
    );
    event.source.source_record_id = Some(record_id.to_string());
    event.parse_evidence = Some(ParseEvidence {
        event_key_version: "test-scan.v1".to_string(),
        source_file_path_hash: Some(hash_text(file_path)),
        source_line_number: Some(1),
        source_record_id: Some(record_id.to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    event
}

pub(super) fn test_task_span(
    source: &SourceLocation,
    file_path: &str,
    started_at: DateTime<Utc>,
    record_id: &str,
    title: &str,
    event: &UsageEvent,
) -> TaskSpan {
    TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: task_span_id("codex", &source.source_id, record_id),
        provider: "codex".to_string(),
        source_id: source.source_id.clone(),
        span_kind: "codex_task".to_string(),
        source_record_id: Some(record_id.to_string()),
        source_file_path_hash: Some(hash_text(file_path)),
        summary_id: None,
        session_id: Some("session-test".to_string()),
        thread_id: None,
        title: title.to_string(),
        normalized_title: normalize_task_title(title),
        title_source: Some("thread_name".to_string()),
        summary_preview: Some(title.to_string()),
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: project_bucket_key(event.project.as_ref()),
        project: event.project.clone(),
        git: None,
        usage: event.usage.clone(),
        estimated_cost_usd: event.cost.estimated_api_equivalent_usd,
        estimated_cost_micro_usd: event.cost.estimated_api_equivalent_micro_usd,
        event_count: 1,
        has_usage_evidence: true,
        total_messages: event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.total_messages)
            .unwrap_or(0),
        user_messages: event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.user_messages)
            .unwrap_or(0),
        assistant_messages: event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.assistant_messages)
            .unwrap_or(0),
        developer_messages: event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.developer_messages)
            .unwrap_or(0),
        linked_event_ids: vec![event.event_id.clone()],
        confidence: Confidence::High,
        is_meta: false,
        started_at,
        ended_at: Some(started_at),
        duration_seconds: Some(0),
    }
}

pub(super) fn test_unattributed_quota_record(source_id: &str) -> QuotaObservationRecordV1 {
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
        .single()
        .expect("observed at");
    let reset = observed_at + Duration::days(7);
    QuotaObservationRecordV1 {
        observation: statsai_core::QuotaObservationV1 {
            schema_version: "quota_observation.v1".to_string(),
            observation_id: format!("observation-{source_id}"),
            semantic_fingerprint: format!("semantic-{source_id}"),
            provider: "codex".to_string(),
            source_id: SourceId(source_id.to_string()),
            provider_account_id: None,
            observed_at,
            source_file_path_hash: format!("file-{source_id}"),
            source_record_id: format!("record-{source_id}"),
            source_line_number: 1,
            payload_hash: format!("payload-{source_id}"),
            usage_sample: None,
            usage_event_id: None,
            usage_link_kind: statsai_core::QuotaUsageLinkKind::None,
            status: statsai_core::QuotaStatusV1::default(),
        },
        windows: vec![statsai_core::QuotaWindowObservationV1 {
            schema_version: "quota_window_observation.v1".to_string(),
            window_observation_id: format!("window-observation-{source_id}"),
            observation_id: format!("observation-{source_id}"),
            provider_slot: "primary".to_string(),
            limit_id: Some("subscription".to_string()),
            window_minutes: 10_080,
            used_percent: 20.0,
            resets_at: reset,
            resets_at_epoch_seconds: reset.timestamp(),
        }],
        raw_rate_limits: json!({}),
    }
}
