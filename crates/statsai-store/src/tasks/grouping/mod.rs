use super::*;

mod items;

pub(crate) use items::*;

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
