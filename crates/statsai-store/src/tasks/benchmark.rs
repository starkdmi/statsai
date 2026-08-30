use super::*;

#[derive(Debug, Clone)]
pub(crate) struct GroundTruthData {
    pub(crate) cluster_by_span: HashMap<String, String>,
    pub(crate) rejected_span_ids: HashSet<String>,
    pub(crate) adjacent_truth: Vec<(String, String, bool)>,
    pub(crate) verified_span_ids: HashSet<String>,
    pub(crate) verified_adjacent_pairs: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum BenchmarkStrategy {
    GapHours(i64),
    RepoTitle,
    RepoBranchTitle,
}

impl BenchmarkStrategy {
    pub(crate) fn name(&self) -> String {
        match self {
            Self::GapHours(hours) => format!("gap_only_{}h", hours),
            Self::RepoTitle => "repo_plus_title".to_string(),
            Self::RepoBranchTitle => "repo_plus_branch_plus_title".to_string(),
        }
    }
}

pub(crate) fn ground_truth_from_store(
    spans: &[TaskSpan],
    work_items: &[WorkItem],
    member_map: &HashMap<String, String>,
    verifications: &[TaskVerification],
) -> Result<GroundTruthData> {
    let spans_by_id = spans
        .iter()
        .cloned()
        .map(|span| (span.span_id.0.clone(), span))
        .collect::<HashMap<_, _>>();
    let work_items_by_id = work_items
        .iter()
        .cloned()
        .map(|work_item| (work_item.work_item_id.0.clone(), work_item))
        .collect::<HashMap<_, _>>();
    let mut verified_work_item_ids = HashSet::<String>::new();
    let mut rejected_work_item_ids = HashSet::<String>::new();
    for verification in verifications {
        match &verification.action {
            TaskVerificationAction::Accept { anchor_span_id, .. }
            | TaskVerificationAction::Rename { anchor_span_id, .. } => {
                if let Some(work_item_id) = member_map.get(anchor_span_id.0.as_str()) {
                    verified_work_item_ids.insert(work_item_id.clone());
                }
            }
            TaskVerificationAction::Reject { anchor_span_id, .. } => {
                if let Some(work_item_id) = member_map.get(anchor_span_id.0.as_str()) {
                    verified_work_item_ids.insert(work_item_id.clone());
                    rejected_work_item_ids.insert(work_item_id.clone());
                }
            }
            TaskVerificationAction::Merge {
                left_anchor_span_id,
                right_anchor_span_id,
                ..
            } => {
                if let Some(work_item_id) = member_map.get(left_anchor_span_id.0.as_str()) {
                    verified_work_item_ids.insert(work_item_id.clone());
                }
                if let Some(work_item_id) = member_map.get(right_anchor_span_id.0.as_str()) {
                    verified_work_item_ids.insert(work_item_id.clone());
                }
            }
            TaskVerificationAction::Split { after_span_id, .. } => {
                if let Some(work_item_id) = member_map.get(after_span_id.0.as_str()) {
                    verified_work_item_ids.insert(work_item_id.clone());
                }
                if let Some(next_span_id) = split_right_span_id(&verification.action, &spans_by_id)
                {
                    if let Some(work_item_id) = member_map.get(next_span_id.as_str()) {
                        verified_work_item_ids.insert(work_item_id.clone());
                    }
                }
            }
        }
    }

    let mut cluster_by_span = HashMap::<String, String>::new();
    let mut rejected_span_ids = HashSet::<String>::new();
    for work_item_id in &verified_work_item_ids {
        if let Some(work_item) = work_items_by_id.get(work_item_id) {
            for (span_id, assigned_work_item_id) in member_map {
                if assigned_work_item_id == work_item_id {
                    cluster_by_span.insert(span_id.clone(), work_item.work_item_id.0.clone());
                    if rejected_work_item_ids.contains(work_item_id) {
                        rejected_span_ids.insert(span_id.clone());
                    }
                }
            }
        }
    }

    let mut adjacent_truth = Vec::new();
    let mut spans_by_bucket = BTreeMap::<String, Vec<&TaskSpan>>::new();
    for span in spans {
        spans_by_bucket
            .entry(span.project_bucket.clone())
            .or_default()
            .push(span);
    }
    for bucket_spans in spans_by_bucket.values_mut() {
        bucket_spans.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.span_id.0.cmp(&right.span_id.0))
        });
        for pair in bucket_spans.windows(2) {
            let left = pair[0];
            let right = pair[1];
            let Some(left_cluster) = cluster_by_span.get(left.span_id.0.as_str()) else {
                continue;
            };
            let Some(right_cluster) = cluster_by_span.get(right.span_id.0.as_str()) else {
                continue;
            };
            adjacent_truth.push((
                left.span_id.0.clone(),
                right.span_id.0.clone(),
                left_cluster == right_cluster,
            ));
        }
    }

    Ok(GroundTruthData {
        verified_adjacent_pairs: adjacent_truth.len() as u64,
        verified_span_ids: cluster_by_span.keys().cloned().collect(),
        cluster_by_span,
        rejected_span_ids,
        adjacent_truth,
    })
}

pub(crate) fn next_span_id_in_bucket(
    after_span_id: &TaskSpanId,
    spans_by_id: &HashMap<String, TaskSpan>,
) -> Option<String> {
    let anchor = spans_by_id.get(after_span_id.0.as_str())?;
    let mut same_bucket = spans_by_id
        .values()
        .filter(|span| span.project_bucket == anchor.project_bucket)
        .collect::<Vec<_>>();
    same_bucket.sort_by(|left, right| {
        left.started_at
            .cmp(&right.started_at)
            .then_with(|| left.span_id.0.cmp(&right.span_id.0))
    });
    let index = same_bucket
        .iter()
        .position(|span| span.span_id == *after_span_id)?;
    same_bucket
        .get(index + 1)
        .map(|span| span.span_id.0.clone())
}

pub(crate) fn split_right_span_id(
    action: &TaskVerificationAction,
    spans_by_id: &HashMap<String, TaskSpan>,
) -> Option<String> {
    let TaskVerificationAction::Split {
        after_span_id,
        before_span_id,
        ..
    } = action
    else {
        return None;
    };
    let anchor = spans_by_id.get(after_span_id.0.as_str())?;
    if let Some(before_span_id) = before_span_id.as_ref() {
        let right = spans_by_id.get(before_span_id.0.as_str())?;
        if right.project_bucket == anchor.project_bucket {
            return Some(before_span_id.0.clone());
        }
        return None;
    }
    next_span_id_in_bucket(after_span_id, spans_by_id)
}

pub(crate) fn evaluate_prediction(
    truth: &GroundTruthData,
    predicted_assignments: &HashMap<String, String>,
    predicted_rejected_spans: &HashSet<String>,
) -> TaskBenchmarkMetrics {
    let adjacent_counts = truth.adjacent_truth.iter().fold(
        (0u64, 0u64, 0u64),
        |(tp, pred_pos, truth_pos), (left_span_id, right_span_id, truth_same)| {
            let predicted_same = predicted_assignments
                .get(left_span_id.as_str())
                .zip(predicted_assignments.get(right_span_id.as_str()))
                .is_some_and(|(left, right)| left == right);
            (
                tp + u64::from(predicted_same && *truth_same),
                pred_pos + u64::from(predicted_same),
                truth_pos + u64::from(*truth_same),
            )
        },
    );
    let cluster_counts = pairwise_cluster_counts(truth, predicted_assignments);
    let meta_counts = meta_counts(truth, predicted_rejected_spans);
    TaskBenchmarkMetrics {
        adjacent_precision: ratio(adjacent_counts.0, adjacent_counts.1),
        adjacent_recall: ratio(adjacent_counts.0, adjacent_counts.2),
        adjacent_f1: f1(adjacent_counts.0, adjacent_counts.1, adjacent_counts.2),
        cluster_precision: ratio(cluster_counts.0, cluster_counts.1),
        cluster_recall: ratio(cluster_counts.0, cluster_counts.2),
        cluster_f1: f1(cluster_counts.0, cluster_counts.1, cluster_counts.2),
        meta_precision: ratio(meta_counts.0, meta_counts.1),
        meta_recall: ratio(meta_counts.0, meta_counts.2),
        meta_f1: f1(meta_counts.0, meta_counts.1, meta_counts.2),
    }
}

pub(crate) fn pairwise_cluster_counts(
    truth: &GroundTruthData,
    predicted_assignments: &HashMap<String, String>,
) -> (u64, u64, u64) {
    let span_ids = truth.verified_span_ids.iter().cloned().collect::<Vec<_>>();
    let mut tp = 0u64;
    let mut pred_pos = 0u64;
    let mut truth_pos = 0u64;
    for left_index in 0..span_ids.len() {
        for right_index in (left_index + 1)..span_ids.len() {
            let left_span_id = &span_ids[left_index];
            let right_span_id = &span_ids[right_index];
            let truth_same = truth
                .cluster_by_span
                .get(left_span_id.as_str())
                .zip(truth.cluster_by_span.get(right_span_id.as_str()))
                .is_some_and(|(left, right)| left == right);
            let predicted_same = predicted_assignments
                .get(left_span_id.as_str())
                .zip(predicted_assignments.get(right_span_id.as_str()))
                .is_some_and(|(left, right)| left == right);
            tp += u64::from(truth_same && predicted_same);
            pred_pos += u64::from(predicted_same);
            truth_pos += u64::from(truth_same);
        }
    }
    (tp, pred_pos, truth_pos)
}

pub(crate) fn meta_counts(
    truth: &GroundTruthData,
    predicted_rejected_spans: &HashSet<String>,
) -> (u64, u64, u64) {
    let mut tp = 0u64;
    let mut pred_pos = 0u64;
    let mut truth_pos = 0u64;
    for span_id in &truth.verified_span_ids {
        let predicted = predicted_rejected_spans.contains(span_id);
        let truth_positive = truth.rejected_span_ids.contains(span_id);
        tp += u64::from(predicted && truth_positive);
        pred_pos += u64::from(predicted);
        truth_pos += u64::from(truth_positive);
    }
    (tp, pred_pos, truth_pos)
}

pub(crate) fn build_baseline_assignments(
    spans: &[TaskSpan],
    strategy: BenchmarkStrategy,
) -> HashMap<String, String> {
    let mut by_bucket = BTreeMap::<String, Vec<&TaskSpan>>::new();
    for span in spans {
        by_bucket
            .entry(span.project_bucket.clone())
            .or_default()
            .push(span);
    }
    let mut assignments = HashMap::<String, String>::new();
    for (bucket, bucket_spans) in by_bucket {
        match strategy {
            BenchmarkStrategy::GapHours(hours) => {
                let mut ordered = bucket_spans;
                ordered.sort_by(|left, right| {
                    left.started_at
                        .cmp(&right.started_at)
                        .then_with(|| left.span_id.0.cmp(&right.span_id.0))
                });
                let mut cluster_index = 0usize;
                let mut current_cluster = format!("{}:gap:{}:{}", bucket, hours, cluster_index);
                let mut previous_end = None::<DateTime<Utc>>;
                for span in ordered {
                    if let Some(previous_end) = previous_end {
                        let gap = span
                            .started_at
                            .signed_duration_since(previous_end)
                            .num_hours();
                        if gap > hours {
                            cluster_index += 1;
                            current_cluster = format!("{}:gap:{}:{}", bucket, hours, cluster_index);
                        }
                    }
                    assignments.insert(span.span_id.0.clone(), current_cluster.clone());
                    previous_end = Some(span.effective_ended_at());
                }
            }
            BenchmarkStrategy::RepoTitle => {
                for span in bucket_spans {
                    assignments.insert(
                        span.span_id.0.clone(),
                        format!("{}:title:{}", bucket, span.normalized_title),
                    );
                }
            }
            BenchmarkStrategy::RepoBranchTitle => {
                for span in bucket_spans {
                    assignments.insert(
                        span.span_id.0.clone(),
                        format!(
                            "{}:branch_title:{}:{}",
                            bucket,
                            span.branch_family.as_deref().unwrap_or("none"),
                            span.normalized_title
                        ),
                    );
                }
            }
        }
    }
    assignments
}

pub(crate) fn rejected_span_ids_from_work_items(
    work_items: &[WorkItem],
    member_map: &HashMap<String, String>,
) -> HashSet<String> {
    let rejected_work_item_ids = work_items
        .iter()
        .filter(|item| item.status == TaskStatus::RejectedMeta)
        .map(|item| item.work_item_id.0.clone())
        .collect::<HashSet<_>>();
    member_map
        .iter()
        .filter(|(_, work_item_id)| rejected_work_item_ids.contains(*work_item_id))
        .map(|(span_id, _)| span_id.clone())
        .collect()
}

pub(crate) fn manual_constraints_preserved(
    predicted_assignments: &HashMap<String, String>,
    spans: &[TaskSpan],
    verifications: &[TaskVerification],
) -> bool {
    let spans_by_id = spans
        .iter()
        .cloned()
        .map(|span| (span.span_id.0.clone(), span))
        .collect::<HashMap<_, _>>();
    for verification in verifications {
        match &verification.action {
            TaskVerificationAction::Split { .. } => {
                let Some(next_span_id) = split_right_span_id(&verification.action, &spans_by_id)
                else {
                    continue;
                };
                let after_span_id = match &verification.action {
                    TaskVerificationAction::Split { after_span_id, .. } => after_span_id,
                    _ => unreachable!(),
                };
                let left = predicted_assignments.get(after_span_id.0.as_str());
                let right = predicted_assignments.get(next_span_id.as_str());
                if left.is_some() && left == right {
                    return false;
                }
            }
            TaskVerificationAction::Merge {
                left_anchor_span_id,
                right_anchor_span_id,
                ..
            } => {
                let left = predicted_assignments.get(left_anchor_span_id.0.as_str());
                let right = predicted_assignments.get(right_anchor_span_id.0.as_str());
                if left.is_none() || right.is_none() || left != right {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

pub(crate) fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

pub(crate) fn f1(true_positive: u64, predicted_positive: u64, truth_positive: u64) -> f64 {
    let precision = ratio(true_positive, predicted_positive);
    let recall = ratio(true_positive, truth_positive);
    if precision == 0.0 && recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}
