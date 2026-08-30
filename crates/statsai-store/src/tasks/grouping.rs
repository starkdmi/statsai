use super::*;

#[derive(Debug, Default)]
pub(crate) struct PendingGroup {
    pub(crate) spans: Vec<SpanContext>,
    pub(crate) continuation_reasons: BTreeSet<String>,
    pub(crate) manual_title: Option<String>,
    pub(crate) force_verified: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BuildWorkItemsTimings {
    pub(crate) grouping_ms: u64,
    pub(crate) title_selection_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ContinuationDecision {
    pub(crate) score: i32,
    pub(crate) reasons: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BoundaryEvidence {
    pub(crate) local_similarity: f64,
    pub(crate) adjacent_overlap: usize,
    pub(crate) strong_topic_boundary: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DistributionStats {
    pub(crate) mean: f64,
    pub(crate) std_dev: f64,
}

impl DistributionStats {
    pub(crate) fn from_values(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = values
            .iter()
            .map(|value| {
                let delta = value - mean;
                delta * delta
            })
            .sum::<f64>()
            / values.len() as f64;
        Self {
            mean,
            std_dev: variance.sqrt(),
        }
    }

    pub(crate) fn has_variation(self) -> bool {
        self.std_dev > f64::EPSILON
    }

    pub(crate) fn low_outlier_threshold(self) -> f64 {
        (self.mean - (self.std_dev * 0.5)).max(0.0)
    }

    pub(crate) fn high_outlier_threshold(self) -> f64 {
        self.mean + (self.std_dev * 0.5)
    }
}

pub(crate) fn build_work_items(
    contexts: Vec<SpanContext>,
    verifications: &[TaskVerification],
) -> (Vec<WorkItem>, Vec<WorkItemMember>, BuildWorkItemsTimings) {
    let mut by_bucket = BTreeMap::<String, Vec<SpanContext>>::new();
    for context in contexts {
        by_bucket
            .entry(context.span.project_bucket.clone())
            .or_default()
            .push(context);
    }

    let mut work_items = Vec::new();
    let mut members = Vec::new();
    let mut timings = BuildWorkItemsTimings::default();
    for (bucket, mut bucket_contexts) in by_bucket {
        bucket_contexts.sort_by(|left, right| {
            left.span
                .started_at
                .cmp(&right.span.started_at)
                .then_with(|| left.span.span_id.0.cmp(&right.span.span_id.0))
        });
        let grouping_started_at = Instant::now();
        let groups = group_spans(bucket_contexts, verifications);
        timings.grouping_ms = timings
            .grouping_ms
            .saturating_add(grouping_started_at.elapsed().as_millis() as u64);
        let bucket_label_stats = build_bucket_label_stats(&groups);
        for group in groups {
            let title_started_at = Instant::now();
            let (work_item, group_members) =
                build_work_item(bucket.clone(), group, verifications, &bucket_label_stats);
            timings.title_selection_ms = timings
                .title_selection_ms
                .saturating_add(title_started_at.elapsed().as_millis() as u64);
            members.extend(group_members);
            work_items.push(work_item);
        }
    }
    (work_items, members, timings)
}

pub(crate) fn build_bucket_label_stats(groups: &[PendingGroup]) -> BucketLabelStats {
    let mut stats = BucketLabelStats::default();
    for group in groups {
        let candidates = collect_title_candidates(&group.spans);
        let mut document_titles = BTreeSet::new();
        let mut document_tokens = BTreeSet::new();
        for candidate in candidates {
            if candidate.normalized.is_empty() {
                continue;
            }
            document_titles.insert(candidate.normalized);
            for token in candidate.topic_tokens {
                document_tokens.insert(token);
            }
        }
        if document_titles.is_empty() && document_tokens.is_empty() {
            continue;
        }
        stats.document_count += 1;
        for title in document_titles {
            *stats.title_document_frequency.entry(title).or_default() += 1;
        }
        for token in document_tokens {
            *stats.token_document_frequency.entry(token).or_default() += 1;
        }
    }
    stats
}

pub(crate) fn group_spans(
    contexts: Vec<SpanContext>,
    verifications: &[TaskVerification],
) -> Vec<PendingGroup> {
    let mut groups = Vec::<PendingGroup>::new();
    let boundary_evidence = compute_boundary_evidence(&contexts);
    let mut iter = contexts.into_iter();
    let Some(first) = iter.next() else {
        return groups;
    };
    let mut current = PendingGroup {
        spans: vec![first],
        continuation_reasons: BTreeSet::new(),
        manual_title: None,
        force_verified: false,
    };
    for (boundary_index, next) in iter.enumerate() {
        let previous = current
            .spans
            .last()
            .expect("pending group has at least one span");
        let decision = continuation_decision(
            previous,
            &next,
            boundary_evidence.get(boundary_index).copied(),
        );
        let protected_anchor =
            decision.reasons.contains("same_issue_key") || decision.reasons.contains("same_title");
        let strong_anchor = protected_anchor
            || (decision.reasons.contains("same_session")
                && !decision.reasons.contains("topic_boundary"));
        let blocked_by_topic_boundary =
            decision.reasons.contains("topic_boundary") && !protected_anchor;
        let gap_hours = next
            .span
            .started_at
            .signed_duration_since(previous.ended_at())
            .num_hours();
        let should_continue = !blocked_by_topic_boundary
            && (decision.score >= 4 || (decision.score >= 2 && strong_anchor && gap_hours <= 24));
        if should_continue {
            current.continuation_reasons.extend(decision.reasons);
            current.spans.push(next);
        } else {
            groups.push(current);
            current = PendingGroup {
                spans: vec![next],
                continuation_reasons: BTreeSet::new(),
                manual_title: None,
                force_verified: false,
            };
        }
    }
    groups.push(current);
    let groups = apply_split_verifications(groups, verifications);
    apply_merge_verifications(groups, verifications)
}

pub(crate) fn continuation_decision(
    previous: &SpanContext,
    next: &SpanContext,
    boundary_evidence: Option<BoundaryEvidence>,
) -> ContinuationDecision {
    let mut score = 0;
    let mut reasons = BTreeSet::new();
    let same_session =
        previous.session_key().is_some() && previous.session_key() == next.session_key();
    let previous_generic = previous.title_is_generic();
    let next_generic = next.title_is_generic();
    let previous_issue_keys = previous
        .span
        .issue_keys
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let next_issue_keys = next.span.issue_keys.iter().cloned().collect::<HashSet<_>>();
    let shared_issue_keys = previous_issue_keys
        .intersection(&next_issue_keys)
        .cloned()
        .collect::<HashSet<_>>();
    if !shared_issue_keys.is_empty() {
        score += 6;
        reasons.insert("same_issue_key".to_string());
    } else if !previous_issue_keys.is_empty() && !next_issue_keys.is_empty() {
        score -= 6;
    }

    if same_session {
        score += 5;
        reasons.insert("same_session".to_string());
    }

    if previous.span.branch_family.is_some()
        && previous.span.branch_family == next.span.branch_family
    {
        score += 3;
        reasons.insert("same_branch_family".to_string());
    } else if previous.span.branch_family.is_some() && next.span.branch_family.is_some() {
        score -= 2;
    }

    if !previous.span.normalized_title.is_empty()
        && previous.span.normalized_title == next.span.normalized_title
        && !previous.title_is_generic()
    {
        score += 4;
        reasons.insert("same_title".to_string());
    } else {
        let previous_tokens = previous.topic_tokens();
        let next_tokens = next.topic_tokens();
        let overlap = previous_tokens.intersection(next_tokens).count();
        if overlap >= 2 {
            score += 2;
            reasons.insert("shared_topic".to_string());
        } else if !previous_tokens.is_empty() && !next_tokens.is_empty() {
            score -= 2;
        }
    }

    let gap_hours = next
        .span
        .started_at
        .signed_duration_since(previous.ended_at())
        .num_hours();
    if gap_hours <= 1 {
        score += 2;
        reasons.insert("close_time_gap".to_string());
    } else if gap_hours <= 6 {
        score += 1;
        reasons.insert("same_day_continuation".to_string());
    } else if gap_hours > 72 {
        score -= 3;
    } else if gap_hours > 24 {
        score -= 1;
    }

    if previous_generic || next_generic {
        score -= 2;
    }
    if previous_generic && next_generic && !same_session {
        score -= 3;
    }
    if previous.span.is_meta != next.span.is_meta {
        score -= 4;
    } else if previous.span.is_meta && next.span.is_meta && !same_session {
        score -= 2;
    }

    if let Some(boundary) = boundary_evidence {
        if boundary.local_similarity > 0.0 && boundary.adjacent_overlap >= 2 {
            score += 1;
            reasons.insert("windowed_topic_cohesion".to_string());
        }
        if boundary.strong_topic_boundary {
            score -= 4;
            reasons.insert("topic_boundary".to_string());
        }
    }

    ContinuationDecision { score, reasons }
}

pub(crate) fn compute_boundary_evidence(contexts: &[SpanContext]) -> Vec<BoundaryEvidence> {
    if contexts.len() < 2 {
        return Vec::new();
    }

    let topic_sets = contexts
        .iter()
        .map(|context| context.topic_tokens().clone())
        .collect::<Vec<_>>();
    let similarities = (0..topic_sets.len() - 1)
        .map(|boundary_index| boundary_window_similarity(&topic_sets, boundary_index))
        .collect::<Vec<_>>();
    let depth_scores = boundary_depth_scores(&similarities);
    let similarity_stats = DistributionStats::from_values(&similarities);
    let depth_stats = DistributionStats::from_values(&depth_scores);
    let similarity_count = similarities.len();

    similarities
        .into_iter()
        .enumerate()
        .map(|(boundary_index, local_similarity)| {
            let adjacent_overlap = topic_sets[boundary_index]
                .intersection(&topic_sets[boundary_index + 1])
                .count();
            let previous = &contexts[boundary_index];
            let next = &contexts[boundary_index + 1];
            let same_session =
                previous.session_key().is_some() && previous.session_key() == next.session_key();
            let anchored = spans_share_non_generic_title(previous, next)
                || spans_share_issue_keys(previous, next);
            let pairwise_boundary = similarity_count == 1
                && same_session
                && !anchored
                && adjacent_overlap == 0
                && local_similarity <= 0.0
                && spans_have_contentful_topic_signal(previous, next);
            let statistical_boundary = same_session
                && !anchored
                && adjacent_overlap == 0
                && similarity_stats.has_variation()
                && depth_stats.has_variation()
                && local_similarity <= similarity_stats.low_outlier_threshold()
                && depth_scores[boundary_index] >= depth_stats.high_outlier_threshold();
            let strong_topic_boundary = pairwise_boundary || statistical_boundary;
            BoundaryEvidence {
                local_similarity,
                adjacent_overlap,
                strong_topic_boundary,
            }
        })
        .collect()
}

pub(crate) fn boundary_window_similarity(
    topic_sets: &[BTreeSet<String>],
    boundary_index: usize,
) -> f64 {
    let left_start = boundary_index.saturating_sub(TOPIC_COHESION_WINDOW_SPANS - 1);
    let left_counts = aggregate_topic_counts(&topic_sets[left_start..=boundary_index]);
    let right_end = (boundary_index + TOPIC_COHESION_WINDOW_SPANS).min(topic_sets.len() - 1);
    let right_counts = aggregate_topic_counts(&topic_sets[boundary_index + 1..=right_end]);
    weighted_jaccard_similarity(&left_counts, &right_counts)
}

pub(crate) fn boundary_depth_scores(similarities: &[f64]) -> Vec<f64> {
    if similarities.is_empty() {
        return Vec::new();
    }

    (0..similarities.len())
        .map(|index| {
            let left_start = index.saturating_sub(TOPIC_COHESION_WINDOW_SPANS);
            let right_end = (index + TOPIC_COHESION_WINDOW_SPANS).min(similarities.len() - 1);
            let current = similarities[index];
            let left_peak = similarities[left_start..=index]
                .iter()
                .copied()
                .fold(current, f64::max);
            let right_peak = similarities[index..=right_end]
                .iter()
                .copied()
                .fold(current, f64::max);
            (left_peak - current).max(0.0) + (right_peak - current).max(0.0)
        })
        .collect()
}

pub(crate) fn aggregate_topic_counts(topic_sets: &[BTreeSet<String>]) -> HashMap<String, usize> {
    let mut counts = HashMap::<String, usize>::new();
    for topic_set in topic_sets {
        for token in topic_set {
            *counts.entry(token.clone()).or_default() += 1;
        }
    }
    counts
}

pub(crate) fn weighted_jaccard_similarity(
    left: &HashMap<String, usize>,
    right: &HashMap<String, usize>,
) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    let mut tokens = left.keys().cloned().collect::<HashSet<_>>();
    tokens.extend(right.keys().cloned());
    let (intersection, union) = tokens.into_iter().fold((0usize, 0usize), |acc, token| {
        let left_count = left.get(&token).copied().unwrap_or_default();
        let right_count = right.get(&token).copied().unwrap_or_default();
        (
            acc.0 + left_count.min(right_count),
            acc.1 + left_count.max(right_count),
        )
    });
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

pub(crate) fn spans_share_issue_keys(left: &SpanContext, right: &SpanContext) -> bool {
    let left_issue_keys = left.span.issue_keys.iter().cloned().collect::<HashSet<_>>();
    let right_issue_keys = right
        .span
        .issue_keys
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    !left_issue_keys.is_empty()
        && left_issue_keys
            .intersection(&right_issue_keys)
            .next()
            .is_some()
}

pub(crate) fn spans_share_non_generic_title(left: &SpanContext, right: &SpanContext) -> bool {
    !left.span.normalized_title.is_empty()
        && left.span.normalized_title == right.span.normalized_title
        && !left.title_is_generic()
}

pub(crate) fn spans_have_contentful_topic_signal(left: &SpanContext, right: &SpanContext) -> bool {
    span_has_contentful_topic_signal(left) && span_has_contentful_topic_signal(right)
}

pub(crate) fn span_has_contentful_topic_signal(span: &SpanContext) -> bool {
    let title_signal = span.title_signal_score();
    let topic_token_count = span.topic_tokens().len();
    title_signal > 0 && topic_token_count >= 3
}

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
