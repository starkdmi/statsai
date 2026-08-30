use super::*;

pub(crate) fn summary_sync_payload_hash(summary: &UsageSummary) -> Result<String> {
    let payload = serde_json::to_string(&sanitize_summary_for_sync(summary.clone()))?;
    Ok(hash_text(&payload))
}

pub(crate) fn sanitize_summary_for_default_http_sync(summary: UsageSummary) -> UsageSummary {
    sanitize_summary_for_http_sync(summary, false)
}

pub(crate) fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

pub(crate) fn sanitize_summary_for_http_sync(
    summary: UsageSummary,
    include_projects: bool,
) -> UsageSummary {
    let mut summary = sanitize_summary_for_sync(summary);
    if !include_projects {
        summary.project = None;
    }
    summary
}

pub(crate) fn summary_sync_day(summary: &UsageSummary) -> NaiveDate {
    summary
        .period_start
        .map(|start| start.date_naive())
        .unwrap_or_else(|| summary.observed_at.date_naive())
}

pub(crate) fn summary_period_bounds(summary: &UsageSummary) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = summary
        .period_start
        .or(summary.period_end)
        .unwrap_or(summary.observed_at);
    let end = summary
        .period_end
        .or(summary.period_start)
        .unwrap_or(summary.observed_at);
    if end < start {
        (end, start)
    } else {
        (start, end)
    }
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let (start, end) = summary_period_bounds(summary);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = end - start;
    duration >= chrono::Duration::zero() && duration <= chrono::Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

pub(crate) fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if summary.metadata.summary_format == "daily_rollup.v1" {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

pub(crate) fn collect_pending_summary_days<'a>(
    summaries: impl IntoIterator<Item = &'a UsageSummary>,
) -> BTreeSet<NaiveDate> {
    let mut days = BTreeSet::new();
    for summary in summaries {
        if summary.metadata.summary_format == "daily_rollup.v1"
            || (is_exact_daily_passthrough_summary(summary)
                && summary_fits_single_daily_report_day(summary))
        {
            days.insert(summary_sync_day(summary));
            continue;
        }

        let (start, end) = summary_period_bounds(summary);
        let mut day = start.date_naive();
        let end_day = end.date_naive();
        loop {
            days.insert(day);
            if day >= end_day {
                break;
            }
            let Some(next_day) = day.succ_opt() else {
                break;
            };
            day = next_day;
        }
    }
    days
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SyncRollupBucketKey {
    provider: String,
    source_id: String,
    provider_account_id: Option<String>,
    day_key: String,
    project_key: String,
}

pub(crate) fn sync_rollup_bucket_key(event: &UsageEvent) -> SyncRollupBucketKey {
    SyncRollupBucketKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        provider_account_id: event.provider_account_id.as_ref().map(|id| id.0.clone()),
        day_key: event.session.started_at.date_naive().to_string(),
        project_key: sync_rollup_project_key(event.project.as_ref()),
    }
}

fn sync_rollup_summary_id(key: &SyncRollupBucketKey) -> SummaryId {
    summary_id(
        &key.provider,
        &SourceId(key.source_id.clone()),
        &format!(
            "daily_stats:{}:{}:{}",
            key.day_key,
            key.provider_account_id.as_deref().unwrap_or("unlinked"),
            hash_text(&key.project_key),
        ),
    )
}

pub(crate) fn sync_rollup_project_key(project: Option<&statsai_core::ProjectInfo>) -> String {
    daily_rollup_project_key(project)
}

pub(crate) fn event_with_valid_project(event: &UsageEvent) -> UsageEvent {
    let mut event = event.clone();
    if event
        .project
        .as_ref()
        .is_some_and(|project| !project_has_stable_identity(project))
    {
        event.project = None;
    }
    event
}

pub(crate) fn build_sync_rollup_summary(events: &[UsageEvent]) -> UsageSummary {
    let first = events.first().expect("rollup bucket must contain events");
    // Events arrive oldest first. A bucket can now span a repository rename, so
    // project metadata comes from the newest event: it names the remote the
    // checkout has now, which is what lets the backend move this location onto
    // the renamed project instead of waiting for the next day of usage.
    let newest = events.last().unwrap_or(first);
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_creation = 0u64;
    let mut total_cache_creation_5m = 0u64;
    let mut total_cache_creation_1h = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_reasoning = 0u64;
    let mut total_tokens = 0u64;
    let mut total_events = 0u64;
    let mut estimated_cost = CostAccumulator::default();
    let mut provider_reported_cost = CostAccumulator::default();
    let mut has_provider_reported_usd = false;
    let mut observed_at = first.created_at;
    let mut model_buckets: BTreeMap<String, (ModelInfo, SyncRollupModelTotals)> = BTreeMap::new();
    let mut session_ids = BTreeSet::new();
    let mut active_seconds = 0.0_f64;
    let mut latency_values = Vec::new();
    let mut ttft_values = Vec::new();
    let mut generated_tps_values = Vec::new();
    let mut visible_tps_values = Vec::new();
    let mut cache_hit_ratio_values = Vec::new();
    let mut reasoning_share_values = Vec::new();
    let mut total_messages = 0u64;
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut developer_messages = 0u64;
    let mut tracked_requests = 0u64;
    let mut tracked_output_tokens = 0u64;
    let mut tracked_reasoning_tokens = 0u64;

    for event in events {
        let mut event_generated_tps = None;
        total_input = total_input.saturating_add(event.usage.input_tokens.unwrap_or(0));
        total_output = total_output.saturating_add(event.usage.output_tokens.unwrap_or(0));
        total_cache_creation =
            total_cache_creation.saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        total_cache_creation_5m = total_cache_creation_5m
            .saturating_add(event.usage.cache_creation_5m_tokens.unwrap_or(0));
        total_cache_creation_1h = total_cache_creation_1h
            .saturating_add(event.usage.cache_creation_1h_tokens.unwrap_or(0));
        total_cache_read =
            total_cache_read.saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        total_reasoning = total_reasoning.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        total_tokens = total_tokens.saturating_add(event.usage.computed_total());
        total_events = total_events.saturating_add(1);
        estimated_cost.add_estimated(&event.cost);
        if event.cost.provider_reported_micro_usd.is_some()
            || event.cost.provider_reported_usd.is_some()
        {
            provider_reported_cost.add_values(
                event.cost.provider_reported_micro_usd,
                event.cost.provider_reported_usd,
            );
            has_provider_reported_usd = true;
        }
        if event.created_at > observed_at {
            observed_at = event.created_at;
        }
        session_ids.insert(
            event
                .session
                .local_session_id_hash
                .clone()
                .unwrap_or_else(|| event.session.session_id.clone()),
        );
        let is_tracked_turn = event
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.latency_ms)
            .is_some();
        if let Some(runtime) = event.runtime.as_ref() {
            let derived_total_messages = runtime.total_messages.or_else(|| {
                let derived = runtime
                    .user_messages
                    .unwrap_or(0)
                    .saturating_add(runtime.assistant_messages.unwrap_or(0))
                    .saturating_add(runtime.developer_messages.unwrap_or(0));
                (derived > 0).then_some(derived)
            });
            total_messages = total_messages.saturating_add(derived_total_messages.unwrap_or(0));
            user_messages = user_messages.saturating_add(runtime.user_messages.unwrap_or(0));
            assistant_messages =
                assistant_messages.saturating_add(runtime.assistant_messages.unwrap_or(0));
            developer_messages =
                developer_messages.saturating_add(runtime.developer_messages.unwrap_or(0));

            if let Some(latency_ms) = runtime.latency_ms {
                let latency_ms_f64 = latency_ms as f64;
                active_seconds += latency_ms_f64 / 1000.0;

                if runtime_latency_supports_distribution_metrics(runtime) {
                    latency_values.push(latency_ms_f64);
                }

                if latency_ms > 0 && runtime_latency_supports_distribution_metrics(runtime) {
                    let duration_seconds = latency_ms_f64 / 1000.0;
                    let generated_tokens = event
                        .usage
                        .output_tokens
                        .unwrap_or(0)
                        .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
                    let generated_tps = generated_tokens as f64 / duration_seconds;
                    event_generated_tps = Some(generated_tps);
                    generated_tps_values.push(generated_tps);
                    visible_tps_values
                        .push(event.usage.output_tokens.unwrap_or(0) as f64 / duration_seconds);
                }
            }

            if let Some(ttft_ms) = runtime.time_to_first_token_ms {
                ttft_values.push(ttft_ms as f64);
            }
        }
        if is_tracked_turn {
            tracked_requests = tracked_requests.saturating_add(1);
            tracked_output_tokens =
                tracked_output_tokens.saturating_add(event.usage.output_tokens.unwrap_or(0));
            tracked_reasoning_tokens =
                tracked_reasoning_tokens.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        }

        let prompt_tokens = event
            .usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        if prompt_tokens > 0 {
            cache_hit_ratio_values
                .push(event.usage.cache_read_tokens.unwrap_or(0) as f64 / prompt_tokens as f64);
        }
        let generated_tokens = event
            .usage
            .output_tokens
            .unwrap_or(0)
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        if generated_tokens > 0 {
            reasoning_share_values
                .push(event.usage.reasoning_tokens.unwrap_or(0) as f64 / generated_tokens as f64);
        }

        let model = event.model.clone().unwrap_or_default();
        let entry = model_buckets
            .entry(sync_rollup_model_key(&model))
            .or_insert_with(|| (model.clone(), SyncRollupModelTotals::default()));
        if let Some(generated_tps) = event_generated_tps {
            entry.1.generated_tps_samples = entry.1.generated_tps_samples.saturating_add(1);
            entry.1.generated_tps_sum += generated_tps;
        }
        entry.1.input_tokens = entry
            .1
            .input_tokens
            .saturating_add(event.usage.input_tokens.unwrap_or(0));
        entry.1.output_tokens = entry
            .1
            .output_tokens
            .saturating_add(event.usage.output_tokens.unwrap_or(0));
        entry.1.cache_creation_tokens = entry
            .1
            .cache_creation_tokens
            .saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        entry.1.cache_creation_5m_tokens = entry
            .1
            .cache_creation_5m_tokens
            .saturating_add(event.usage.cache_creation_5m_tokens.unwrap_or(0));
        entry.1.cache_creation_1h_tokens = entry
            .1
            .cache_creation_1h_tokens
            .saturating_add(event.usage.cache_creation_1h_tokens.unwrap_or(0));
        entry.1.cache_read_tokens = entry
            .1
            .cache_read_tokens
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        entry.1.reasoning_tokens = entry
            .1
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        entry.1.total_tokens = entry
            .1
            .total_tokens
            .saturating_add(event.usage.computed_total());
        entry.1.requests = entry.1.requests.saturating_add(1);
        entry.1.estimated_cost.add_estimated(&event.cost);
        if event.cost.provider_reported_micro_usd.is_some()
            || event.cost.provider_reported_usd.is_some()
        {
            entry.1.provider_reported_cost.add_values(
                event.cost.provider_reported_micro_usd,
                event.cost.provider_reported_usd,
            );
            entry.1.has_provider_reported_usd = true;
        }
    }

    let day = first.session.started_at.date_naive();
    let period_start = day
        .and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or(first.session.started_at);
    let period_end = period_start + chrono::Duration::days(1);
    let bucket_key = sync_rollup_bucket_key(first);
    let models = model_buckets
        .into_values()
        .map(|(model, totals)| SummaryModelUsage {
            model,
            usage: UsageCounts {
                input_tokens: Some(totals.input_tokens),
                output_tokens: Some(totals.output_tokens),
                cache_creation_tokens: Some(totals.cache_creation_tokens),
                cache_creation_5m_tokens: Some(totals.cache_creation_5m_tokens),
                cache_creation_1h_tokens: Some(totals.cache_creation_1h_tokens),
                cache_read_tokens: Some(totals.cache_read_tokens),
                reasoning_tokens: Some(totals.reasoning_tokens),
                total_tokens: Some(totals.total_tokens),
                requests: Some(totals.requests),
                local_prompt_eval_tokens: None,
                local_eval_tokens: None,
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: totals.estimated_cost.cents_rounded(),
                provider_reported_usd: totals
                    .has_provider_reported_usd
                    .then(|| totals.provider_reported_cost.cents_rounded())
                    .flatten(),
                estimated_api_equivalent_micro_usd: totals.estimated_cost.micro_usd(),
                provider_reported_micro_usd: totals
                    .has_provider_reported_usd
                    .then(|| totals.provider_reported_cost.micro_usd())
                    .flatten(),
                pricing_source: Some("local_rollup".to_string()),
                pricing_version: None,
                confidence: Confidence::Medium,
            },
            metrics: (totals.generated_tps_samples > 0).then_some(SummaryModelMetrics {
                generated_tps: Some(SummaryMetricTotals {
                    samples: totals.generated_tps_samples,
                    sum: totals.generated_tps_sum,
                }),
            }),
        })
        .collect();
    let summary_metrics = summary_metrics_or_none(SummaryMetrics {
        active_seconds: (active_seconds > 0.0).then_some(active_seconds),
        tracked_requests: (tracked_requests > 0).then_some(tracked_requests),
        tracked_output_tokens: (tracked_output_tokens > 0).then_some(tracked_output_tokens),
        tracked_reasoning_tokens: (tracked_reasoning_tokens > 0)
            .then_some(tracked_reasoning_tokens),
        latency_ms: finalize_metric_stats(latency_values),
        time_to_first_token_ms: finalize_metric_stats(ttft_values),
        generated_tps: finalize_metric_stats(generated_tps_values),
        visible_tps: finalize_metric_stats(visible_tps_values),
        overall_generated_tps: (active_seconds > 0.0).then_some(
            tracked_output_tokens.saturating_add(tracked_reasoning_tokens) as f64 / active_seconds,
        ),
        overall_visible_tps: (active_seconds > 0.0)
            .then_some(tracked_output_tokens as f64 / active_seconds),
        cache_hit_ratio: finalize_metric_stats(cache_hit_ratio_values),
        reasoning_share: finalize_metric_stats(reasoning_share_values),
        total_messages: (total_messages > 0).then_some(total_messages),
        user_messages: (user_messages > 0).then_some(user_messages),
        assistant_messages: (assistant_messages > 0).then_some(assistant_messages),
        developer_messages: (developer_messages > 0).then_some(developer_messages),
    });
    let total_sessions = (!session_ids.is_empty()).then_some(session_ids.len() as u64);
    let total_messages_metadata = summary_metrics
        .as_ref()
        .and_then(|metrics| metrics.total_messages);

    UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: sync_rollup_summary_id(&bucket_key),
        device_id: first.device_id.clone(),
        provider: first.provider.clone(),
        source_id: first.source_id.clone(),
        provider_account_id: first.provider_account_id.clone(),
        source: EventSource {
            source_record_id: None,
            ..first.source.clone()
        },
        model: None,
        models,
        usage: UsageCounts {
            input_tokens: Some(total_input),
            output_tokens: Some(total_output),
            cache_creation_tokens: Some(total_cache_creation),
            cache_creation_5m_tokens: Some(total_cache_creation_5m),
            cache_creation_1h_tokens: Some(total_cache_creation_1h),
            cache_read_tokens: Some(total_cache_read),
            reasoning_tokens: Some(total_reasoning),
            total_tokens: Some(total_tokens),
            requests: Some(total_events),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: estimated_cost.cents_rounded(),
            provider_reported_usd: has_provider_reported_usd
                .then(|| provider_reported_cost.cents_rounded())
                .flatten(),
            estimated_api_equivalent_micro_usd: estimated_cost.micro_usd(),
            provider_reported_micro_usd: has_provider_reported_usd
                .then(|| provider_reported_cost.micro_usd())
                .flatten(),
            pricing_source: Some("local_rollup".to_string()),
            pricing_version: None,
            confidence: Confidence::Medium,
        },
        parse_evidence: None,
        project: newest
            .project
            .as_ref()
            .filter(|project| project_has_stable_identity(project))
            .cloned(),
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: project_contains_file_paths(newest.project.as_ref()),
        },
        metrics: summary_metrics,
        period_start: Some(period_start),
        period_end: Some(period_end),
        observed_at,
        metadata: SummaryMetadata {
            summary_format: "daily_rollup.v1".to_string(),
            summary_version: Some(SYNC_ROLLUP_SUMMARY_VERSION.to_string()),
            total_sessions,
            total_messages: total_messages_metadata,
            last_computed_at: Some(observed_at),
        },
        imported_at: observed_at,
    }
}

fn finalize_metric_stats(mut values: Vec<f64>) -> Option<MetricStats> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let samples = values.len() as u64;
    let sum = values.iter().copied().sum::<f64>();
    Some(MetricStats {
        samples,
        avg: Some(sum / samples as f64),
        min: values.first().copied(),
        max: values.last().copied(),
        p50: percentile_nearest_rank(&values, 0.50),
        p95: percentile_nearest_rank(&values, 0.95),
        sum: Some(sum),
    })
}

fn runtime_latency_supports_distribution_metrics(runtime: &statsai_core::RuntimeInfo) -> bool {
    !matches!(runtime.latency_source, Some(LatencySource::Inferred))
}

fn percentile_nearest_rank(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let rank = ((values.len() as f64) * percentile).ceil() as usize;
    values
        .get(rank.saturating_sub(1).min(values.len().saturating_sub(1)))
        .copied()
}

fn summary_metrics_or_none(metrics: SummaryMetrics) -> Option<SummaryMetrics> {
    let has_metrics = metrics.active_seconds.is_some()
        || metrics.tracked_requests.is_some()
        || metrics.tracked_output_tokens.is_some()
        || metrics.tracked_reasoning_tokens.is_some()
        || metrics.latency_ms.is_some()
        || metrics.time_to_first_token_ms.is_some()
        || metrics.generated_tps.is_some()
        || metrics.visible_tps.is_some()
        || metrics.overall_generated_tps.is_some()
        || metrics.overall_visible_tps.is_some()
        || metrics.cache_hit_ratio.is_some()
        || metrics.reasoning_share.is_some()
        || metrics.total_messages.is_some()
        || metrics.user_messages.is_some()
        || metrics.assistant_messages.is_some()
        || metrics.developer_messages.is_some();
    has_metrics.then_some(metrics)
}

#[derive(Debug, Default)]
struct SyncRollupModelTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_creation_5m_tokens: u64,
    cache_creation_1h_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    requests: u64,
    generated_tps_samples: u64,
    generated_tps_sum: f64,
    estimated_cost: CostAccumulator,
    provider_reported_cost: CostAccumulator,
    has_provider_reported_usd: bool,
}

fn sync_rollup_model_key(model: &ModelInfo) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        model.normalized_name.as_deref().unwrap_or(""),
        model.provider_model_id.as_deref().unwrap_or(""),
        model.name.as_deref().unwrap_or(""),
        model.speed.as_deref().unwrap_or(""),
        model
            .reasoning_level
            .as_ref()
            .map(|level| level.as_str())
            .unwrap_or(""),
        model.reasoning_level_raw.as_deref().unwrap_or("")
    )
}

impl Store {
    pub fn sync_rollup_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sync_rollups", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn dirty_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups WHERE dirty = 1 ORDER BY updated_at, summary_id",
        )
    }

    pub fn all_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups ORDER BY updated_at, summary_id",
        )
    }

    pub fn mark_sync_rollups_synced(&self, summary_ids: &[SummaryId]) -> Result<()> {
        if summary_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_sync_rollups_synced_in_transaction(summary_ids)
        })
    }

    pub(crate) fn mark_sync_rollups_synced_in_transaction(
        &self,
        summary_ids: &[SummaryId],
    ) -> Result<()> {
        for summary_id in summary_ids {
            self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1",
                params![&summary_id.0],
            )?;
        }
        Ok(())
    }

    pub fn mark_all_sync_rollups_dirty(&self) -> Result<u64> {
        let updated = self.conn.execute(
            "UPDATE sync_rollups SET dirty = 1, updated_at = ?1",
            params![Utc::now().to_rfc3339()],
        )? as u64;
        Ok(updated)
    }

    pub fn rebuild_sync_rollups(&self) -> Result<u64> {
        let events = self.events()?;
        let keys: BTreeSet<_> = events.iter().map(sync_rollup_bucket_key).collect();

        self.with_immediate_transaction(|| {
            self.conn.execute("DELETE FROM sync_rollups", [])?;
            self.refresh_sync_rollups_for_keys(&keys)?;
            Ok(keys.len() as u64)
        })
    }

    pub(crate) fn ensure_current_sync_rollup_versions(&self) -> Result<()> {
        let stale_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_rollups
             WHERE json_extract(payload, '$.metadata.summary_format') = 'daily_rollup.v1'
               AND COALESCE(json_extract(payload, '$.metadata.summary_version'), '') != ?1",
            params![SYNC_ROLLUP_SUMMARY_VERSION],
            |row| row.get(0),
        )?;
        if stale_count > 0 {
            self.rebuild_sync_rollups()?;
        }
        Ok(())
    }

    pub fn sync_rollup_period_stats(&self, cutoff_day: NaiveDate) -> Result<RollupPeriodStats> {
        let mut tokens = 0u64;
        let mut requests = 0u64;
        for summary in self.all_sync_rollup_summaries()? {
            let day = summary_sync_day(&summary);
            if day < cutoff_day {
                continue;
            }
            tokens = tokens.saturating_add(summary.usage.computed_total());
            requests = requests.saturating_add(summary.usage.requests.unwrap_or(0));
        }
        Ok(RollupPeriodStats { tokens, requests })
    }

    pub fn usage_event_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        Ok(self.conn.query_row(
            r#"
            SELECT
              COALESCE(SUM(total_tokens), 0),
              COUNT(*)
            FROM usage_events
            WHERE started_at >= ?1
            "#,
            params![since.to_rfc3339()],
            |row| {
                Ok(RollupPeriodStats {
                    tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                })
            },
        )?)
    }

    pub fn reportable_summary_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE datetime(COALESCE(period_start, observed_at)) >= datetime(?1)
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![since.to_rfc3339()],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn reportable_summary_period_stats_since_day(
        &self,
        cutoff_day: NaiveDate,
    ) -> Result<RollupPeriodStats> {
        let cutoff_day = cutoff_day.format("%Y-%m-%d").to_string();
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE substr(COALESCE(period_start, observed_at), 1, 10) >= ?1
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn snapshot_rollup_view(
        &self,
        sink: &str,
        target: &str,
        week_cutoff: NaiveDate,
        today_cutoff: NaiveDate,
    ) -> Result<SnapshotRollupView> {
        let week_cutoff = week_cutoff.format("%Y-%m-%d").to_string();
        let today_cutoff = today_cutoff.format("%Y-%m-%d").to_string();
        let week = self.sync_rollup_stats_since_day(&week_cutoff)?;
        let today = self.sync_rollup_stats_since_day(&today_cutoff)?;
        let (pending_count, pending_days) = self.pending_sync_rollup_counts(sink, target)?;
        Ok(SnapshotRollupView {
            pending_count,
            pending_days,
            today,
            week,
        })
    }

    fn sync_rollup_stats_since_day(&self, cutoff_day: &str) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0)
                FROM sync_rollups
                WHERE day_key >= ?1
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)? as u64,
                        requests: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    const SYNC_ROLLUP_HASH_RECONCILE_KEY: &str = "sync_rollup_sync_hashes_reconciled_v1";

    pub fn reconcile_sync_rollup_sync_hashes_if_needed(&self) -> Result<u64> {
        if self
            .metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY)?
            .as_deref()
            == Some("1")
        {
            return Ok(0);
        }
        let updated = self.reconcile_sync_rollup_sync_hashes()?;
        self.set_metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY, "1")?;
        Ok(updated)
    }

    pub fn reconcile_sync_rollup_sync_hashes(&self) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT summary_id, payload FROM sync_rollups")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;

        self.with_immediate_transaction(|| {
            let mut updated = 0u64;
            for (summary_id, payload) in &rows {
                let summary: UsageSummary = serde_json::from_str(payload)?;
                let payload_hash = summary_sync_payload_hash(&summary)?;
                updated += self.conn.execute(
                    "UPDATE sync_rollups SET payload_hash = ?1 WHERE summary_id = ?2 AND payload_hash != ?1",
                    params![payload_hash, summary_id],
                )? as u64;
            }
            Ok(updated)
        })
    }

    fn pending_sync_rollup_counts(&self, sink: &str, target: &str) -> Result<(u64, u64)> {
        let rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(sanitize_summary_for_default_http_sync)
            .collect::<Vec<_>>();
        let pending = self.pending_summaries_for_sync(sink, target, &rollups)?;
        let days = collect_pending_summary_days(pending.iter());
        Ok((pending.len() as u64, days.len() as u64))
    }

    pub fn reconcile_sync_rollup_dirty_flags(&self, sink: &str, target: &str) -> Result<u64> {
        self.ensure_current_sync_rollup_versions()?;
        let summaries = self.all_sync_rollup_summaries()?;
        self.with_immediate_transaction(|| {
            self.reconcile_sync_rollup_dirty_flags_in_transaction(sink, target, &summaries)
        })
    }

    pub(crate) fn reconcile_sync_rollup_dirty_flags_in_transaction(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<u64> {
        let mut cleared = 0u64;
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )? {
                continue;
            }
            cleared += self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1 AND dirty = 1",
                params![&summary.summary_id.0],
            )? as u64;
        }
        Ok(cleared)
    }

    pub fn compute_daily_rollup(&self, date: &str, device_id: &str) -> Result<DailyRollup> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload FROM usage_events
            WHERE started_at >= ?1 AND started_at < ?2
            "#,
        )?;
        let end_date = {
            let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
            (parsed + chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string()
        };
        let rows = stmt.query_map(params![date, &end_date], |row| row.get::<_, String>(0))?;

        let mut total_input = 0u64;
        let mut total_cache_create = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_output = 0u64;
        let mut total_reasoning = 0u64;
        let mut total_tokens = 0u64;
        let mut total_events = 0u64;
        let mut sessions = std::collections::BTreeSet::new();
        let mut estimated_cost = CostAccumulator::default();
        let mut by_provider: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut by_account: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();

        for row in rows {
            let event: UsageEvent = serde_json::from_str(&row?)?;
            total_input = total_input.saturating_add(event.usage.input_tokens.unwrap_or(0));
            total_cache_create =
                total_cache_create.saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
            total_cache_read =
                total_cache_read.saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
            total_output = total_output.saturating_add(event.usage.output_tokens.unwrap_or(0));
            total_reasoning =
                total_reasoning.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
            total_tokens = total_tokens.saturating_add(event.usage.computed_total());
            total_events = total_events.saturating_add(1);
            sessions.insert(event.session.session_id.clone());

            estimated_cost.add_estimated(&event.cost);

            let provider_entry = by_provider
                .entry(event.provider.clone())
                .or_insert_with(|| serde_json::json!({"tokens": 0, "events": 0}));
            provider_entry["tokens"] = serde_json::json!(provider_entry["tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(event.usage.computed_total()));
            provider_entry["events"] = serde_json::json!(provider_entry["events"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1));

            let account_key = event
                .provider_account_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "unassigned".to_string());
            let account_entry = by_account.entry(account_key).or_insert_with(
                || serde_json::json!({"tokens": 0, "events": 0, "provider": event.provider}),
            );
            account_entry["tokens"] = serde_json::json!(account_entry["tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(event.usage.computed_total()));
            account_entry["events"] = serde_json::json!(account_entry["events"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1));
        }

        Ok(DailyRollup {
            schema_version: statsai_core::DAILY_ROLLUP_SCHEMA_VERSION.to_string(),
            date: date.to_string(),
            device_id: device_id.to_string(),
            total_input_tokens: total_input,
            total_cache_creation_tokens: total_cache_create,
            total_cache_read_tokens: total_cache_read,
            total_output_tokens: total_output,
            total_reasoning_tokens: total_reasoning,
            total_tokens,
            total_events,
            total_sessions: sessions.len() as u64,
            estimated_cost_usd: estimated_cost.cents_rounded(),
            estimated_cost_micro_usd: estimated_cost.micro_usd(),
            by_provider: Some(serde_json::to_string(&by_provider)?),
            by_account: Some(serde_json::to_string(&by_account)?),
            updated_at: chrono::Utc::now(),
        })
    }

    pub fn upsert_daily_rollup(&self, rollup: &DailyRollup) -> Result<()> {
        let payload = serde_json::to_string(rollup)?;
        self.conn.execute(
            r#"
            INSERT INTO daily_rollups (
              date, device_id, total_tokens, total_events, total_sessions,
              estimated_cost_usd, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(date, device_id) DO UPDATE SET
              total_tokens = excluded.total_tokens,
              total_events = excluded.total_events,
              total_sessions = excluded.total_sessions,
              estimated_cost_usd = excluded.estimated_cost_usd,
              payload = excluded.payload
            "#,
            params![
                &rollup.date,
                &rollup.device_id,
                safe_u64_to_i64(rollup.total_tokens),
                safe_u64_to_i64(rollup.total_events),
                safe_u64_to_i64(rollup.total_sessions),
                rollup.estimated_cost_usd,
                &payload,
            ],
        )?;
        Ok(())
    }

    pub fn daily_rollups_between(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<DailyRollup>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM daily_rollups WHERE date >= ?1 AND date <= ?2 ORDER BY date",
        )?;
        let rows = stmt.query_map(params![start_date, end_date], |row| row.get::<_, String>(0))?;
        let mut rollups = Vec::new();
        for row in rows {
            rollups.push(serde_json::from_str(&row?)?);
        }
        Ok(rollups)
    }

    pub fn delete_rollups_for_device(&self, device_id: &str) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM daily_rollups WHERE device_id = ?1",
            params![device_id],
        )? as u64;
        Ok(deleted)
    }

    fn sync_rollup_summaries_by_sql(&self, sql: &str) -> Result<Vec<UsageSummary>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub(crate) fn refresh_sync_rollups_for_keys(
        &self,
        keys: &BTreeSet<SyncRollupBucketKey>,
    ) -> Result<()> {
        self.refresh_sync_rollups_for_keys_counted(keys).map(|_| ())
    }

    pub(crate) fn refresh_sync_rollups_for_keys_counted(
        &self,
        keys: &BTreeSet<SyncRollupBucketKey>,
    ) -> Result<u64> {
        let mut refreshed = 0u64;
        for key in keys {
            if self.refresh_sync_rollup_for_key(key)? {
                refreshed += 1;
            }
        }
        Ok(refreshed)
    }

    fn refresh_sync_rollup_for_key(&self, key: &SyncRollupBucketKey) -> Result<bool> {
        let events = self.sync_rollup_events(key)?;
        if events.is_empty() {
            let deleted = self.conn.execute(
                "DELETE FROM sync_rollups WHERE summary_id = ?1",
                params![sync_rollup_summary_id(key).0],
            )?;
            return Ok(deleted > 0);
        }

        let summary = build_sync_rollup_summary(&events);
        let payload = serde_json::to_string(&summary)?;
        let payload_hash = summary_sync_payload_hash(&summary)?;
        let existing: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT payload_hash, dirty FROM sync_rollups WHERE summary_id = ?1",
                params![&summary.summary_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if existing
            .as_ref()
            .is_some_and(|(existing_hash, _)| existing_hash == &payload_hash)
        {
            return Ok(false);
        }

        let dirty = existing.as_ref().map_or(1, |(_, dirty)| (*dirty).max(1));
        self.conn.execute(
            r#"
            INSERT INTO sync_rollups (
              summary_id, provider, source_id, provider_account_id, day_key,
              observed_at, updated_at, payload_hash, dirty, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(summary_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              day_key = excluded.day_key,
              observed_at = excluded.observed_at,
              updated_at = excluded.updated_at,
              payload_hash = excluded.payload_hash,
              dirty = excluded.dirty,
              payload = excluded.payload
            "#,
            params![
                &summary.summary_id.0,
                &summary.provider,
                &summary.source_id.0,
                summary.provider_account_id.as_ref().map(|id| id.0.as_str()),
                &key.day_key,
                summary.observed_at.to_rfc3339(),
                Utc::now().to_rfc3339(),
                &payload_hash,
                dirty,
                &payload,
            ],
        )?;
        Ok(true)
    }

    fn sync_rollup_events(&self, key: &SyncRollupBucketKey) -> Result<Vec<UsageEvent>> {
        let start = format!("{}T00:00:00+00:00", key.day_key);
        let end = {
            let day = NaiveDate::parse_from_str(&key.day_key, "%Y-%m-%d")?;
            format!(
                "{}T00:00:00+00:00",
                (day + chrono::Duration::days(1)).format("%Y-%m-%d")
            )
        };
        let sql = if key.provider_account_id.is_some() {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id = ?3
              AND started_at >= ?4
              AND started_at < ?5
            ORDER BY started_at, event_id
            "#
        } else {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id IS NULL
              AND started_at >= ?3
              AND started_at < ?4
            ORDER BY started_at, event_id
            "#
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut events: Vec<UsageEvent> = Vec::new();
        if let Some(provider_account_id) = key.provider_account_id.as_deref() {
            let rows = stmt.query_map(
                params![
                    &key.provider,
                    &key.source_id,
                    provider_account_id,
                    &start,
                    &end
                ],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if let Ok(event) = serde_json::from_str(&row?) {
                    events.push(event);
                }
            }
        } else {
            let rows = stmt.query_map(
                params![&key.provider, &key.source_id, &start, &end],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if let Ok(event) = serde_json::from_str(&row?) {
                    events.push(event);
                }
            }
        }
        events.retain(|event| sync_rollup_project_key(event.project.as_ref()) == key.project_key);
        Ok(events)
    }

    pub(crate) fn delete_sync_rollups_for_sources_in_tx(
        &self,
        source_ids: &[SourceId],
    ) -> Result<u64> {
        let mut deleted = 0u64;
        for source_id in source_ids {
            deleted += self.conn.execute(
                "DELETE FROM sync_rollups WHERE source_id = ?1",
                params![&source_id.0],
            )? as u64;
        }
        Ok(deleted)
    }
}
