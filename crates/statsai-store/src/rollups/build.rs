use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SyncRollupBucketKey {
    pub(crate) provider: String,
    pub(crate) source_id: String,
    pub(crate) provider_account_id: Option<String>,
    pub(crate) day_key: String,
    pub(crate) project_key: String,
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

pub(crate) fn sync_rollup_summary_id(key: &SyncRollupBucketKey) -> SummaryId {
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
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) cache_creation_5m_tokens: u64,
    pub(crate) cache_creation_1h_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) requests: u64,
    pub(crate) generated_tps_samples: u64,
    pub(crate) generated_tps_sum: f64,
    pub(crate) estimated_cost: CostAccumulator,
    pub(crate) provider_reported_cost: CostAccumulator,
    pub(crate) has_provider_reported_usd: bool,
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
