use super::*;

pub(crate) fn build_work_item(
    project_bucket: String,
    group: PendingGroup,
    verifications: &[TaskVerification],
    bucket_label_stats: &BucketLabelStats,
) -> (WorkItem, Vec<WorkItemMember>) {
    let PendingGroup {
        spans,
        continuation_reasons,
        manual_title,
        force_verified,
    } = group;
    let span_ids = spans
        .iter()
        .map(|context| context.span.span_id.clone())
        .collect::<Vec<_>>();
    let work_item_id = work_item_id(&project_bucket, &span_ids);
    let anchor_span_id = spans
        .first()
        .expect("group has at least one span")
        .span
        .span_id
        .clone();
    let tail_span_id = spans
        .last()
        .expect("group has at least one span")
        .span
        .span_id
        .clone();
    let started_at = spans
        .first()
        .expect("group has at least one span")
        .span
        .started_at;
    let ended_at = spans
        .last()
        .expect("group has at least one span")
        .ended_at();
    let duration_seconds = ended_at
        .signed_duration_since(started_at)
        .num_seconds()
        .try_into()
        .ok();

    let title = manual_title
        .unwrap_or_else(|| choose_work_item_title_with_stats(&spans, bucket_label_stats));
    let mut providers = BTreeSet::new();
    let mut issue_keys = BTreeSet::new();
    let mut branch_labels = BTreeSet::new();
    let mut summary_preview = None;
    let mut todo_excerpt = None;
    let mut repo_label = None;
    let mut path_label = None;
    let mut unique_event_ids = BTreeSet::<String>::new();
    let mut event_count = 0u64;
    let mut usage = UsageCounts::default();
    let mut estimated_cost = CostAccumulator::default();
    let mut no_git = true;

    for context in &spans {
        providers.insert(context.span.provider.clone());
        for issue_key in &context.span.issue_keys {
            issue_keys.insert(issue_key.clone());
        }
        if let Some(branch_label) = context
            .span
            .project
            .as_ref()
            .and_then(|project| project.branch_label.as_deref())
        {
            branch_labels.insert(branch_label.to_string());
        }
        if summary_preview.is_none() {
            summary_preview = context.span.summary_preview.clone();
        }
        if todo_excerpt.is_none() {
            todo_excerpt = context.span.todo_excerpt.clone();
        }
        if repo_label.is_none() {
            repo_label = context
                .span
                .project
                .as_ref()
                .and_then(|project| project.repo_label.clone());
        }
        if path_label.is_none() {
            path_label = context
                .span
                .project
                .as_ref()
                .and_then(|project| project.path_label.clone());
        }
        if context.span.has_git_anchor() {
            no_git = false;
        }
        usage = sum_usage_counts(&usage, &context.usage());
        estimated_cost.add_values(
            context.estimated_cost_micro_usd(),
            context.estimated_cost_usd(),
        );
        let linked_event_count = context.span.linked_event_ids.len() as u64;
        let extra_event_count = context.event_count().saturating_sub(linked_event_count);
        event_count = event_count.saturating_add(extra_event_count);
        for event_id in &context.span.linked_event_ids {
            unique_event_ids.insert(event_id.0.clone());
        }
    }
    event_count = event_count.saturating_add(unique_event_ids.len() as u64);

    let cross_provider = providers.len() > 1;
    let total_tokens = usage.computed_total();
    let has_usage_evidence = event_count > 0 || spans.iter().any(SpanContext::has_usage_evidence);
    let zero_token_usage = has_usage_evidence && total_tokens == 0;
    let total_messages = spans.iter().map(SpanContext::total_messages).sum::<u64>();
    let user_messages = spans.iter().map(SpanContext::user_messages).sum::<u64>();
    let assistant_messages = spans
        .iter()
        .map(SpanContext::assistant_messages)
        .sum::<u64>();
    let developer_messages = spans
        .iter()
        .map(SpanContext::developer_messages)
        .sum::<u64>();
    let mut review_reasons = Vec::<String>::new();
    let all_meta = spans
        .iter()
        .all(|context| context.span.is_meta || span_is_session_control_meta(&context.span));
    let all_low_signal = spans.iter().all(|context| {
        context.title_is_generic()
            || context.title_is_weak_signal()
            || span_is_session_control_meta(&context.span)
    });
    let low_volume_exchange = total_messages > 0
        && total_messages <= (spans.len() as u64).saturating_mul(4)
        && user_messages <= (spans.len() as u64).saturating_mul(2)
        && assistant_messages <= (spans.len() as u64).saturating_mul(2)
        && developer_messages <= spans.len() as u64;
    let low_signal_non_task =
        all_low_signal && low_volume_exchange && issue_keys.is_empty() && no_git && !cross_provider;
    let span_id_set = span_ids
        .iter()
        .map(|span_id| span_id.0.as_str())
        .collect::<HashSet<_>>();
    let mut status_override = None::<TaskStatus>;
    let mut renamed_title = None::<String>;
    if !has_usage_evidence {
        review_reasons.push("no_usage_evidence".to_string());
    }
    if zero_token_usage {
        review_reasons.push("zero_token_usage".to_string());
    }
    if no_git {
        review_reasons.push("no_git_anchor".to_string());
    }
    if task_title_is_generic(Some(title.as_str())) {
        review_reasons.push("generic_title".to_string());
    } else if task_title_is_weak_signal(Some(title.as_str())) {
        review_reasons.push("weak_title".to_string());
    }
    if task_title_corpus_specificity_score(title.as_str(), bucket_label_stats) <= 0
        && bucket_label_stats.document_count >= 4
    {
        review_reasons.push("low_specificity_title".to_string());
    }
    if cross_provider {
        review_reasons.push("cross_provider_merge".to_string());
    }
    if low_signal_non_task {
        review_reasons.push("low_signal_exchange".to_string());
    }
    if ended_at.signed_duration_since(started_at).num_hours() > 36
        && no_git
        && issue_keys.is_empty()
    {
        review_reasons.push("multi_day_no_anchor".to_string());
    }

    let confidence = if all_meta
        || low_signal_non_task
        || !has_usage_evidence
        || zero_token_usage
        || review_reasons.len() >= 2
    {
        Confidence::Low
    } else if review_reasons.is_empty() {
        Confidence::High
    } else {
        Confidence::Medium
    };
    let mut status = if all_meta || low_signal_non_task {
        TaskStatus::RejectedMeta
    } else if review_reasons.is_empty() {
        TaskStatus::Auto
    } else {
        TaskStatus::NeedsReview
    };
    for verification in verifications {
        match &verification.action {
            TaskVerificationAction::Accept { anchor_span_id, .. }
                if span_id_set.contains(anchor_span_id.0.as_str()) =>
            {
                status_override = Some(TaskStatus::Verified);
            }
            TaskVerificationAction::Reject {
                anchor_span_id,
                reason,
                ..
            } if span_id_set.contains(anchor_span_id.0.as_str()) => {
                status_override = Some(TaskStatus::RejectedMeta);
                review_reasons.push(format!("manual_reject:{:?}", reason));
            }
            TaskVerificationAction::Rename {
                anchor_span_id,
                title,
                ..
            } if span_id_set.contains(anchor_span_id.0.as_str()) => {
                renamed_title = Some(title.clone());
                status_override = Some(TaskStatus::Verified);
            }
            TaskVerificationAction::Merge {
                left_anchor_span_id,
                right_anchor_span_id,
                title,
                ..
            } if span_id_set.contains(left_anchor_span_id.0.as_str())
                && span_id_set.contains(right_anchor_span_id.0.as_str()) =>
            {
                if let Some(title) = title {
                    renamed_title = Some(title.clone());
                }
                status_override = Some(TaskStatus::Verified);
            }
            _ => {}
        }
    }
    if force_verified && !matches!(status_override, Some(TaskStatus::RejectedMeta)) {
        status_override.get_or_insert(TaskStatus::Verified);
    }
    if let Some(override_status) = status_override {
        status = override_status;
    }
    let title = renamed_title.unwrap_or(title);
    let normalized_title = normalize_task_title(&title);

    let work_item = WorkItem {
        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
        work_item_id: work_item_id.clone(),
        anchor_span_id,
        tail_span_id,
        project_bucket,
        title,
        normalized_title,
        status,
        confidence,
        started_at,
        ended_at,
        duration_seconds,
        span_count: spans.len() as u64,
        event_count,
        total_input_tokens: usage.input_tokens.unwrap_or(0),
        total_cache_creation_tokens: usage.cache_creation_tokens.unwrap_or(0),
        total_cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
        total_output_tokens: usage.output_tokens.unwrap_or(0),
        total_reasoning_tokens: usage.reasoning_tokens.unwrap_or(0),
        total_tokens,
        estimated_cost_usd: estimated_cost.cents_rounded(),
        estimated_cost_micro_usd: estimated_cost.micro_usd(),
        providers: providers.into_iter().collect(),
        issue_keys: issue_keys.into_iter().collect(),
        repo_label,
        branch_labels: branch_labels.into_iter().collect(),
        path_label,
        summary_preview,
        todo_excerpt,
        no_git,
        cross_provider,
        continuation_reasons: continuation_reasons.into_iter().collect(),
        review_reasons,
    };
    let members = span_ids
        .into_iter()
        .enumerate()
        .map(|(ordinal, span_id)| WorkItemMember {
            work_item_id: work_item_id.clone(),
            span_id,
            ordinal,
        })
        .collect();
    (work_item, members)
}

pub(crate) fn span_is_session_control_meta(span: &TaskSpan) -> bool {
    [
        Some(span.title.as_str()),
        span.summary_preview.as_deref(),
        span.todo_excerpt.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| task_title_is_session_meta(Some(value)))
}

pub(crate) fn apply_split_verifications(
    mut groups: Vec<PendingGroup>,
    verifications: &[TaskVerification],
) -> Vec<PendingGroup> {
    for verification in verifications {
        let TaskVerificationAction::Split {
            after_span_id,
            before_span_id,
            left_title,
            right_title,
        } = &verification.action
        else {
            continue;
        };
        let mut split_result = None::<(usize, PendingGroup, PendingGroup)>;
        for (group_index, group) in groups.iter().enumerate() {
            let Some(span_index) = group
                .spans
                .iter()
                .position(|context| context.span.span_id == *after_span_id)
            else {
                continue;
            };
            if span_index + 1 >= group.spans.len() {
                continue;
            }
            if before_span_id.as_ref().is_some_and(|before_span_id| {
                group.spans[span_index + 1].span.span_id != *before_span_id
            }) {
                continue;
            }
            let mut left = PendingGroup::default();
            let mut right = PendingGroup::default();
            left.continuation_reasons = group.continuation_reasons.clone();
            right.continuation_reasons = group.continuation_reasons.clone();
            left.spans = group.spans[..=span_index].to_vec();
            right.spans = group.spans[(span_index + 1)..].to_vec();
            left.manual_title = left_title.clone();
            right.manual_title = right_title.clone();
            left.force_verified = true;
            right.force_verified = true;
            split_result = Some((group_index, left, right));
            break;
        }
        if let Some((group_index, left, right)) = split_result {
            groups.remove(group_index);
            groups.insert(group_index, right);
            groups.insert(group_index, left);
        }
    }
    groups
}

pub(crate) fn apply_merge_verifications(
    mut groups: Vec<PendingGroup>,
    verifications: &[TaskVerification],
) -> Vec<PendingGroup> {
    for verification in verifications {
        let TaskVerificationAction::Merge {
            left_anchor_span_id,
            right_anchor_span_id,
            title,
            ..
        } = &verification.action
        else {
            continue;
        };
        let left_index = groups.iter().position(|group| {
            group
                .spans
                .iter()
                .any(|context| context.span.span_id == *left_anchor_span_id)
        });
        let right_index = groups.iter().position(|group| {
            group
                .spans
                .iter()
                .any(|context| context.span.span_id == *right_anchor_span_id)
        });
        let (Some(left_index), Some(right_index)) = (left_index, right_index) else {
            continue;
        };
        if left_index == right_index {
            continue;
        }
        let (keep_index, remove_index) = if left_index < right_index {
            (left_index, right_index)
        } else {
            (right_index, left_index)
        };
        let removed = groups.remove(remove_index);
        let kept = &mut groups[keep_index];
        kept.spans.extend(removed.spans);
        kept.spans.sort_by(|left, right| {
            left.span
                .started_at
                .cmp(&right.span.started_at)
                .then_with(|| left.span.span_id.0.cmp(&right.span.span_id.0))
        });
        kept.continuation_reasons
            .extend(removed.continuation_reasons);
        kept.continuation_reasons.insert("manual_merge".to_string());
        kept.force_verified = true;
        if let Some(title) = title {
            kept.manual_title = Some(title.clone());
        } else if kept.manual_title.is_none() {
            kept.manual_title = removed.manual_title;
        }
    }
    groups
}

pub(crate) fn sum_usage_counts(left: &UsageCounts, right: &UsageCounts) -> UsageCounts {
    pub(crate) fn sum_field(left: Option<u64>, right: Option<u64>) -> Option<u64> {
        if left.is_some() || right.is_some() {
            Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0)))
        } else {
            None
        }
    }

    UsageCounts {
        input_tokens: sum_field(left.input_tokens, right.input_tokens),
        output_tokens: sum_field(left.output_tokens, right.output_tokens),
        cache_creation_tokens: sum_field(left.cache_creation_tokens, right.cache_creation_tokens),
        cache_creation_5m_tokens: sum_field(
            left.cache_creation_5m_tokens,
            right.cache_creation_5m_tokens,
        ),
        cache_creation_1h_tokens: sum_field(
            left.cache_creation_1h_tokens,
            right.cache_creation_1h_tokens,
        ),
        cache_read_tokens: sum_field(left.cache_read_tokens, right.cache_read_tokens),
        reasoning_tokens: sum_field(left.reasoning_tokens, right.reasoning_tokens),
        total_tokens: sum_field(left.total_tokens, right.total_tokens),
        requests: sum_field(left.requests, right.requests),
        local_prompt_eval_tokens: sum_field(
            left.local_prompt_eval_tokens,
            right.local_prompt_eval_tokens,
        ),
        local_eval_tokens: sum_field(left.local_eval_tokens, right.local_eval_tokens),
    }
}
