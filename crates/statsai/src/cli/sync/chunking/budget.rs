use super::*;

pub(crate) fn fit_http_rollup_batches_to_d1_budget(chunks: Vec<SyncBatch>) -> Vec<SyncBatch> {
    let mut fitted = Vec::new();
    for chunk in chunks {
        fitted.extend(fit_http_rollup_batch_to_d1_budget(&chunk));
    }
    fitted
}

pub(crate) fn fit_http_rollup_batch_to_d1_budget(batch: &SyncBatch) -> Vec<SyncBatch> {
    if estimate_http_rollup_d1_queries(batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET {
        return vec![batch.clone()];
    }

    let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(batch);
    if smaller_chunks.len() <= 1 {
        return vec![batch.clone()];
    }

    fit_http_rollup_batches_to_d1_budget(smaller_chunks)
}

pub(crate) fn estimate_http_rollup_d1_queries(batch: &SyncBatch) -> usize {
    let authenticated_device_queries = 2;
    let existing_batch_lookup_queries = 1;
    let final_sync_bookkeeping_queries = 2;
    let account_alias_lookup_queries = usize::from(!batch.accounts.is_empty());
    // The backend reads existing ownership for every non-empty plan batch and
    // may spend one more query deleting a fingerprint the device repointed.
    // The client cannot know whether that cleanup will be needed until the
    // server reads its ownership state, so reserve the worst case here.
    let account_plan_ownership_queries =
        usize::from(!batch.account_plan_observations.is_empty()) * 2;
    let semantic_lookup_queries =
        http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .source_account_assignments
                    .iter()
                    .map(|assignment| assignment.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .subscriptions
                    .iter()
                    .map(|subscription| subscription.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(batch.summaries.iter().filter_map(|summary| {
                summary.provider_account_id.as_ref().map(|id| id.0.as_str())
            }))
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .account_plan_observations
                    .iter()
                    .map(|observation| observation.provider_account_id.0.as_str())
                    .chain(
                        batch
                            .account_evidence_summaries
                            .iter()
                            .map(|summary| summary.provider_account_id.0.as_str()),
                    ),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        );
    let existing_summary_state_queries =
        http_rollup_query_chunks(batch.summaries.len(), HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE);
    let project_location_lookup_queries = http_rollup_query_chunks(
        http_rollup_project_location_count(batch),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let metadata_write_queries = batch.sources.len()
        + batch.accounts.len()
        + batch.source_account_assignments.len()
        + batch.subscriptions.len()
        + batch.account_plan_observations.len()
        + batch.account_evidence_summaries.len();
    let project_write_queries =
        http_rollup_project_count(batch) + http_rollup_project_location_count(batch);
    let daily_rollup_write_queries = http_rollup_query_chunks(
        batch.summaries.len(),
        HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY,
    );
    let monthly_rollup_queries = http_rollup_summary_month_count(batch);
    let dashboard_snapshot_queries = usize::from(!batch.summaries.is_empty());
    // The backend batches metrics into one json_each upsert and one ownership
    // statement regardless of metric count.
    let code_change_metric_queries = usize::from(!batch.code_change_metrics.is_empty()) * 2;
    let quota_cycle_contribution_queries =
        usize::from(!batch.quota_cycle_contributions.is_empty()) * 2;
    let code_change_owner_metadata_refresh_queries = usize::from(
        batch.schema_version == SYNC_BATCH_SCHEMA_VERSION
            && batch
                .authoritative_snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.part_index.saturating_add(1) == snapshot.part_count
                }),
    );

    authenticated_device_queries
        + existing_batch_lookup_queries
        + account_alias_lookup_queries
        + account_plan_ownership_queries
        + semantic_lookup_queries
        + existing_summary_state_queries
        + project_location_lookup_queries
        + metadata_write_queries
        + project_write_queries
        + daily_rollup_write_queries
        + monthly_rollup_queries
        + dashboard_snapshot_queries
        + code_change_metric_queries
        + quota_cycle_contribution_queries
        + code_change_owner_metadata_refresh_queries
        + estimate_http_rollup_task_queries(batch)
        + final_sync_bookkeeping_queries
}

pub(crate) fn estimate_http_rollup_task_queries(batch: &SyncBatch) -> usize {
    let source_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let source_id = span.source_id.0.trim();
                (!source_id.is_empty()).then_some(source_id)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let project_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let project = span.project.as_ref()?;
                Some(
                    [
                        Some(project.project_id.as_str()),
                        project.path_hash.as_deref(),
                        project.repo_remote_hash.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(":"),
                )
            })
            .filter(|descriptor| !descriptor.is_empty())
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let verification_span_lookup_queries = http_rollup_query_chunks(
        batch
            .task_verifications
            .iter()
            .flat_map(|verification| verification.action.span_ids())
            .filter_map(|span_id| {
                let value = span_id.0.trim();
                (!value.is_empty()).then_some(value)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let write_queries = batch
        .task_buckets
        .iter()
        .map(estimate_task_bucket_write_queries)
        .sum::<usize>()
        + batch.task_verifications.len();

    source_lookup_queries
        + project_lookup_queries
        + verification_span_lookup_queries
        + write_queries
}

pub(crate) fn estimate_task_bucket_write_queries(bucket: &TaskBucketSnapshot) -> usize {
    let member_work_item_ids_by_span_id = bucket
        .members
        .iter()
        .map(|member| (member.span_id.0.as_str(), member.work_item_id.0.as_str()))
        .collect::<HashMap<_, _>>();
    let spans_by_id = bucket
        .spans
        .iter()
        .map(|span| (span.span_id.0.as_str(), span))
        .collect::<HashMap<_, _>>();
    4 + count_task_sync_json_chunks(&bucket.spans, |span| {
        estimate_task_sync_span_insert_row_json(
            span,
            member_work_item_ids_by_span_id
                .get(span.span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.work_items, |work_item| {
        estimate_task_sync_work_item_insert_row_json(
            work_item,
            spans_by_id
                .get(work_item.anchor_span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.members, estimate_task_sync_member_insert_row_json)
}

pub(crate) fn count_task_sync_json_chunks<T>(
    rows: &[T],
    serialize_row: impl Fn(&T) -> String,
) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let mut chunk_count = 0usize;
    let mut current_row_count = 0usize;
    let mut current_bytes = 2usize;
    for row in rows {
        let serialized = serialize_row(row);
        let row_bytes = serialized.len();
        let next_bytes = if current_row_count == 0 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
        if current_row_count > 0
            && (current_row_count >= TASK_SYNC_SQL_MAX_ROWS_PER_CHUNK
                || next_bytes > TASK_SYNC_SQL_MAX_JSON_BYTES_PER_CHUNK)
        {
            chunk_count += 1;
            current_row_count = 0;
            current_bytes = 2;
        }
        current_row_count += 1;
        current_bytes = if current_row_count == 1 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
    }
    chunk_count + usize::from(current_row_count > 0)
}

pub(crate) fn estimate_task_sync_project_fields(span: &TaskSpan) -> (Option<&str>, Option<String>) {
    let Some(project) = span.project.as_ref() else {
        return (None, None);
    };
    let project_location_id = [
        project.path_hash.as_deref(),
        project.repo_remote_hash.as_deref(),
        Some(project.project_id.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(":");
    (
        Some(project.project_id.as_str()),
        (!project_location_id.is_empty()).then_some(project_location_id),
    )
}

pub(crate) fn estimate_task_sync_span_insert_row_json(
    span: &TaskSpan,
    work_item_id: Option<&str>,
) -> String {
    let (project_id, project_location_id) = estimate_task_sync_project_fields(span);
    json!({
        "span_id": span.span_id.0,
        "work_item_id": work_item_id,
        "provider": span.provider,
        "provider_account_id": Value::Null,
        "project_id": project_id,
        "project_location_id": project_location_id,
        "started_at": span.started_at.to_rfc3339(),
        "ended_at": span.ended_at.map(|timestamp| timestamp.to_rfc3339()),
        "payload_json": serde_json::to_string(span).unwrap_or_default(),
    })
    .to_string()
}

pub(crate) fn estimate_task_sync_work_item_insert_row_json(
    work_item: &WorkItem,
    anchor_span: Option<&TaskSpan>,
) -> String {
    let (project_id, project_location_id) = anchor_span
        .map(estimate_task_sync_project_fields)
        .unwrap_or((None, None));
    json!({
        "work_item_id": work_item.work_item_id.0,
        "anchor_span_id": work_item.anchor_span_id.0,
        "status": work_item.status,
        "confidence": work_item.confidence,
        "started_at": work_item.started_at.to_rfc3339(),
        "ended_at": work_item.ended_at.to_rfc3339(),
        "project_id": project_id,
        "project_location_id": project_location_id,
        "payload_json": serde_json::to_string(work_item).unwrap_or_default(),
    })
    .to_string()
}

pub(crate) fn estimate_task_sync_member_insert_row_json(
    member: &statsai_core::WorkItemMember,
) -> String {
    json!({
        "work_item_id": member.work_item_id.0,
        "span_id": member.span_id.0,
        "ordinal": member.ordinal,
    })
    .to_string()
}

pub(crate) fn http_rollup_query_chunks(item_count: usize, chunk_size: usize) -> usize {
    if item_count == 0 {
        0
    } else {
        item_count.div_ceil(chunk_size.max(1))
    }
}

pub(crate) fn unique_non_empty_provider_account_ids<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'a str> {
    values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

pub(crate) fn http_rollup_summary_month_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .map(http_rollup_summary_month_key)
        .collect::<BTreeSet<_>>()
        .len()
}

pub(crate) fn http_rollup_summary_month_key(summary: &UsageSummary) -> String {
    let anchor = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    format!("{:04}-{:02}", anchor.year(), anchor.month())
}

pub(crate) fn http_rollup_project_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_key)
        .collect::<BTreeSet<_>>()
        .len()
}

pub(crate) fn http_rollup_summary_project_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if !project_has_stable_identity(project) {
        return None;
    }
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}"));
    }
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    Some(format!("project:{}", project.project_id))
}

pub(crate) fn http_rollup_project_location_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_location_key)
        .collect::<BTreeSet<_>>()
        .len()
}

pub(crate) fn http_rollup_summary_project_location_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if !project_has_stable_identity(project) {
        return None;
    }
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}:{}", project.project_id));
    }
    Some(format!("project:{}", project.project_id))
}
