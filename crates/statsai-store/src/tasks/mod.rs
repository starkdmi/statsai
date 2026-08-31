use super::*;
use statsai_core::{
    normalize_task_title, summarize_task_text, task_title_from_prompt, task_title_is_generic,
    task_title_is_session_meta, task_title_is_weak_signal, task_title_signal_score,
    task_verification_id, title_topic_tokens, work_item_id, Confidence, TaskBucketSnapshot,
    TaskSpan, TaskSpanId, TaskStatus, TaskVerification, TaskVerificationAction,
    TaskVerificationCursor, UsageCounts, WorkItem, WorkItemId, WorkItemMember,
    TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

mod benchmark;
mod buckets;
mod grouping;
mod spans;
mod titles;
mod verification;
mod work_items;

pub(crate) use benchmark::*;
pub(crate) use grouping::*;
pub(crate) use titles::*;
pub(crate) use verification::*;
pub use work_items::derive_task_work_items;
pub(crate) use work_items::SpanContext;

pub(crate) const TOPIC_COHESION_WINDOW_SPANS: usize = 2;
pub(crate) const SQLITE_BUCKET_CHUNK_SIZE: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct TaskBenchmarkMetrics {
    pub adjacent_precision: f64,
    pub adjacent_recall: f64,
    pub adjacent_f1: f64,
    pub cluster_precision: f64,
    pub cluster_recall: f64,
    pub cluster_f1: f64,
    pub meta_precision: f64,
    pub meta_recall: f64,
    pub meta_f1: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedTaskBenchmark {
    pub name: String,
    pub metrics: TaskBenchmarkMetrics,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskBenchmarkReport {
    pub verified_adjacent_pairs: u64,
    pub verified_spans: u64,
    pub has_verified_ground_truth: bool,
    pub has_verified_pairwise_ground_truth: bool,
    pub manual_constraints_preserved: bool,
    pub beats_all_baselines: bool,
    pub shipping_gate_ready: bool,
    pub failing_baselines: Vec<String>,
    pub gate_blockers: Vec<String>,
    pub current: TaskBenchmarkMetrics,
    pub baselines: Vec<NamedTaskBenchmark>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskDeletionImpact {
    pub deleted: u64,
    pub affected_project_buckets: BTreeSet<String>,
    pub deleted_spans: Vec<DeletedTaskSpanRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedTaskSpanRef {
    pub span_id: TaskSpanId,
    pub project_bucket: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRebuildTimings {
    pub delete_ms: u64,
    pub span_load_ms: u64,
    pub verification_load_ms: u64,
    pub grouping_ms: u64,
    pub title_selection_ms: u64,
    pub insert_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRebuildReport {
    pub work_items_rebuilt: u64,
    pub work_items_deleted: u64,
    pub affected_bucket_count: u64,
    pub affected_segment_count: u64,
    pub touched_span_count: u64,
    pub timings: TaskRebuildTimings,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskStats {
    pub total_spans: u64,
    pub total_work_items: u64,
    pub verified_percentage: f64,
    pub no_git_percentage: f64,
    pub cross_provider_percentage: f64,
    pub rejected_meta_percentage: f64,
    pub average_spans_per_work_item: f64,
}

pub(crate) fn sqlite_in_clause_placeholders(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(",")
}

pub(crate) fn sqlite_string_params(values: &[String]) -> Vec<&dyn rusqlite::types::ToSql> {
    values
        .iter()
        .map(|value| value as &dyn rusqlite::types::ToSql)
        .collect()
}

#[derive(Debug, Clone)]
struct ExistingWorkItemLayout {
    work_item_id: WorkItemId,
    project_bucket: String,
    span_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct LocalizedRebuildSegment {
    contexts: Vec<SpanContext>,
}

#[derive(Debug, Clone, Default)]
struct LocalizedRebuildPlan {
    work_item_ids_to_delete: BTreeSet<String>,
    segments: Vec<LocalizedRebuildSegment>,
    touched_span_count: u64,
}

impl Store {
    pub fn task_benchmark_report(&self) -> Result<TaskBenchmarkReport> {
        let spans = self.task_spans()?;
        let current_output = self.work_items()?;
        let current_output_members = self.work_item_members_map()?;
        let verifications = self.task_verifications()?;
        let (predicted, predicted_members) = derive_task_work_items(spans.clone(), &[]);
        let predicted_member_map = work_item_members_map_from_members(&predicted_members);
        let truth = ground_truth_from_store(
            &spans,
            &current_output,
            &current_output_members,
            &verifications,
        )?;
        let current_metrics = evaluate_prediction(
            &truth,
            &predicted_member_map,
            &rejected_span_ids_from_work_items(&predicted, &predicted_member_map),
        );
        let baseline_strategies = vec![
            BenchmarkStrategy::GapHours(2),
            BenchmarkStrategy::GapHours(6),
            BenchmarkStrategy::GapHours(12),
            BenchmarkStrategy::GapHours(24),
            BenchmarkStrategy::RepoTitle,
            BenchmarkStrategy::RepoBranchTitle,
        ];
        let baselines = baseline_strategies
            .into_iter()
            .map(|strategy| {
                let assignments = build_baseline_assignments(&spans, strategy.clone());
                NamedTaskBenchmark {
                    name: strategy.name().to_string(),
                    metrics: evaluate_prediction(&truth, &assignments, &HashSet::new()),
                }
            })
            .collect::<Vec<_>>();
        let has_verified_ground_truth = !truth.verified_span_ids.is_empty();
        let has_verified_pairwise_ground_truth = truth.verified_adjacent_pairs > 0;
        let manual_constraints_preserved =
            manual_constraints_preserved(&current_output_members, &spans, &verifications);
        let failing_baselines = if has_verified_pairwise_ground_truth {
            baselines
                .iter()
                .filter(|baseline| current_metrics.adjacent_f1 <= baseline.metrics.adjacent_f1)
                .map(|baseline| baseline.name.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let beats_all_baselines =
            has_verified_pairwise_ground_truth && failing_baselines.is_empty();
        let mut gate_blockers = Vec::new();
        if !has_verified_ground_truth {
            gate_blockers.push("missing_verified_ground_truth".to_string());
        } else if !has_verified_pairwise_ground_truth {
            gate_blockers.push("missing_pairwise_ground_truth".to_string());
        }
        if !manual_constraints_preserved {
            gate_blockers.push("manual_constraints_not_preserved".to_string());
        }
        if !failing_baselines.is_empty() {
            gate_blockers.push("baseline_regressions".to_string());
        }
        let shipping_gate_ready = gate_blockers.is_empty();
        Ok(TaskBenchmarkReport {
            verified_adjacent_pairs: truth.verified_adjacent_pairs,
            verified_spans: truth.verified_span_ids.len() as u64,
            has_verified_ground_truth,
            has_verified_pairwise_ground_truth,
            manual_constraints_preserved,
            beats_all_baselines,
            shipping_gate_ready,
            failing_baselines,
            gate_blockers,
            current: current_metrics,
            baselines,
        })
    }

    pub fn rebuild_all_task_work_items(&self) -> Result<u64> {
        Ok(self
            .rebuild_all_task_work_items_report()?
            .work_items_rebuilt)
    }

    pub fn rebuild_all_task_work_items_report(&self) -> Result<TaskRebuildReport> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT project_bucket FROM task_spans ORDER BY project_bucket")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut buckets = BTreeSet::new();
        for row in rows {
            buckets.insert(row?);
        }
        self.rebuild_task_work_items_for_project_buckets_report(&buckets)
    }

    pub fn rebuild_task_work_items_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<u64> {
        Ok(self
            .rebuild_task_work_items_for_project_buckets_report(project_buckets)?
            .work_items_rebuilt)
    }

    pub fn rebuild_task_work_items_for_project_buckets_report(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<TaskRebuildReport> {
        if project_buckets.is_empty() {
            return Ok(TaskRebuildReport::default());
        }
        self.with_immediate_transaction(|| {
            let mut report = TaskRebuildReport {
                affected_bucket_count: project_buckets.len() as u64,
                ..TaskRebuildReport::default()
            };
            let delete_started_at = Instant::now();
            report.work_items_deleted =
                self.delete_task_work_items_for_project_buckets_in_tx(project_buckets)?;
            report.timings.delete_ms = delete_started_at.elapsed().as_millis() as u64;

            let span_load_started_at = Instant::now();
            let contexts = self.load_span_contexts_for_project_buckets(project_buckets)?;
            report.touched_span_count = contexts.len() as u64;
            report.timings.span_load_ms = span_load_started_at.elapsed().as_millis() as u64;

            let verification_started_at = Instant::now();
            let verifications = self.relevant_task_verifications(project_buckets)?;
            report.timings.verification_load_ms =
                verification_started_at.elapsed().as_millis() as u64;

            let (work_items, members, build_timings) = build_work_items(contexts, &verifications);
            report.timings.grouping_ms = build_timings.grouping_ms;
            report.timings.title_selection_ms = build_timings.title_selection_ms;
            report.affected_segment_count = work_items.len() as u64;

            let insert_started_at = Instant::now();
            self.insert_work_items_in_tx(&work_items, &members)?;
            report.timings.insert_ms = insert_started_at.elapsed().as_millis() as u64;
            report.work_items_rebuilt = work_items.len() as u64;
            self.mark_task_buckets_dirty_in_tx(project_buckets)?;
            Ok(report)
        })
    }

    pub fn rebuild_task_work_items_for_changes_report(
        &self,
        project_buckets: &BTreeSet<String>,
        changed_span_ids: &BTreeSet<String>,
        deleted_spans: &[DeletedTaskSpanRef],
    ) -> Result<TaskRebuildReport> {
        if project_buckets.is_empty() || (changed_span_ids.is_empty() && deleted_spans.is_empty()) {
            return Ok(TaskRebuildReport::default());
        }
        self.with_immediate_transaction(|| {
            let mut report = TaskRebuildReport {
                affected_bucket_count: project_buckets.len() as u64,
                ..TaskRebuildReport::default()
            };

            let span_load_started_at = Instant::now();
            let contexts = self.load_span_contexts_for_project_buckets(project_buckets)?;
            let layouts =
                self.load_existing_work_item_layouts_for_project_buckets(project_buckets)?;
            report.timings.span_load_ms = span_load_started_at.elapsed().as_millis() as u64;

            let verification_started_at = Instant::now();
            let verifications = self.relevant_task_verifications(project_buckets)?;
            report.timings.verification_load_ms =
                verification_started_at.elapsed().as_millis() as u64;

            let grouping_started_at = Instant::now();
            let plan = build_localized_rebuild_plan(
                contexts,
                layouts,
                changed_span_ids,
                deleted_spans,
                &verifications,
            );
            report.timings.grouping_ms = grouping_started_at.elapsed().as_millis() as u64;
            report.touched_span_count = plan.touched_span_count;
            report.affected_segment_count = plan.segments.len() as u64;

            let delete_started_at = Instant::now();
            report.work_items_deleted =
                self.delete_task_work_items_by_ids_in_tx(&plan.work_item_ids_to_delete)?;
            report.timings.delete_ms = delete_started_at.elapsed().as_millis() as u64;

            let mut work_items = Vec::new();
            let mut members = Vec::new();
            let mut build_timings = BuildWorkItemsTimings::default();
            for segment in plan.segments {
                let (segment_items, segment_members, segment_timings) =
                    build_work_items(segment.contexts, &verifications);
                work_items.extend(segment_items);
                members.extend(segment_members);
                build_timings.grouping_ms = build_timings
                    .grouping_ms
                    .saturating_add(segment_timings.grouping_ms);
                build_timings.title_selection_ms = build_timings
                    .title_selection_ms
                    .saturating_add(segment_timings.title_selection_ms);
            }
            report.timings.grouping_ms = report
                .timings
                .grouping_ms
                .saturating_add(build_timings.grouping_ms);
            report.timings.title_selection_ms = build_timings.title_selection_ms;

            let insert_started_at = Instant::now();
            self.insert_work_items_in_tx(&work_items, &members)?;
            report.timings.insert_ms = insert_started_at.elapsed().as_millis() as u64;
            report.work_items_rebuilt = work_items.len() as u64;
            self.mark_task_buckets_dirty_in_tx(project_buckets)?;
            Ok(report)
        })
    }

    fn load_existing_work_item_layouts_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<Vec<ExistingWorkItemLayout>> {
        if project_buckets.is_empty() {
            return Ok(Vec::new());
        }
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        let mut layouts = Vec::new();
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql = format!(
                r#"
                SELECT w.work_item_id, w.project_bucket, m.span_id
                FROM task_work_items w
                JOIN task_work_item_members m ON m.work_item_id = w.work_item_id
                WHERE w.project_bucket IN ({placeholders})
                ORDER BY w.project_bucket, w.started_at, w.work_item_id, m.ordinal, m.span_id
                "#
            );
            let params = sqlite_string_params(chunk);
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut current_layout = None::<ExistingWorkItemLayout>;
            for row in rows {
                let (work_item_id, project_bucket, span_id) = row?;
                match current_layout.as_mut() {
                    Some(layout) if layout.work_item_id.0 == work_item_id => {
                        layout.span_ids.push(span_id);
                    }
                    Some(layout) => {
                        layouts.push(layout.clone());
                        *layout = ExistingWorkItemLayout {
                            work_item_id: WorkItemId(work_item_id),
                            project_bucket,
                            span_ids: vec![span_id],
                        };
                    }
                    None => {
                        current_layout = Some(ExistingWorkItemLayout {
                            work_item_id: WorkItemId(work_item_id),
                            project_bucket,
                            span_ids: vec![span_id],
                        });
                    }
                }
            }
            if let Some(layout) = current_layout {
                layouts.push(layout);
            }
        }
        Ok(layouts)
    }

    fn delete_task_work_items_by_ids_in_tx(&self, work_item_ids: &BTreeSet<String>) -> Result<u64> {
        if work_item_ids.is_empty() {
            return Ok(0);
        }
        let ids = work_item_ids.iter().cloned().collect::<Vec<_>>();
        let mut deleted = 0u64;
        for chunk in ids.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let params = sqlite_string_params(chunk);
            let delete_members_sql = format!(
                "DELETE FROM task_work_item_members WHERE work_item_id IN ({placeholders})"
            );
            self.conn.execute(&delete_members_sql, params.as_slice())?;

            let count_sql = format!(
                "SELECT COUNT(*) FROM task_work_items WHERE work_item_id IN ({placeholders})"
            );
            deleted += self
                .conn
                .query_row(&count_sql, params.as_slice(), |row| row.get::<_, u64>(0))?;

            let delete_items_sql =
                format!("DELETE FROM task_work_items WHERE work_item_id IN ({placeholders})");
            self.conn.execute(&delete_items_sql, params.as_slice())?;
        }
        Ok(deleted)
    }
}

fn build_localized_rebuild_plan(
    contexts: Vec<SpanContext>,
    layouts: Vec<ExistingWorkItemLayout>,
    changed_span_ids: &BTreeSet<String>,
    deleted_spans: &[DeletedTaskSpanRef],
    verifications: &[TaskVerification],
) -> LocalizedRebuildPlan {
    let mut contexts_by_bucket = BTreeMap::<String, Vec<SpanContext>>::new();
    for context in contexts {
        contexts_by_bucket
            .entry(context.span.project_bucket.clone())
            .or_default()
            .push(context);
    }

    let mut layouts_by_bucket = BTreeMap::<String, Vec<ExistingWorkItemLayout>>::new();
    for layout in layouts {
        layouts_by_bucket
            .entry(layout.project_bucket.clone())
            .or_default()
            .push(layout);
    }

    let mut deleted_by_bucket = BTreeMap::<String, Vec<DeletedTaskSpanRef>>::new();
    for deleted in deleted_spans {
        deleted_by_bucket
            .entry(deleted.project_bucket.clone())
            .or_default()
            .push(deleted.clone());
    }

    let changed_span_ids = changed_span_ids.iter().cloned().collect::<HashSet<_>>();
    let all_buckets = contexts_by_bucket
        .keys()
        .chain(layouts_by_bucket.keys())
        .chain(deleted_by_bucket.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut plan = LocalizedRebuildPlan::default();

    for bucket in all_buckets {
        let bucket_contexts = contexts_by_bucket.remove(&bucket).unwrap_or_default();
        let bucket_layouts = layouts_by_bucket.remove(&bucket).unwrap_or_default();
        let bucket_deleted = deleted_by_bucket.remove(&bucket).unwrap_or_default();
        let deleted_span_ids = bucket_deleted
            .iter()
            .map(|deleted| deleted.span_id.0.clone())
            .collect::<HashSet<_>>();

        if bucket_contexts.is_empty() {
            for layout in &bucket_layouts {
                if layout
                    .span_ids
                    .iter()
                    .any(|span_id| deleted_span_ids.contains(span_id))
                {
                    plan.work_item_ids_to_delete
                        .insert(layout.work_item_id.0.clone());
                }
            }
            continue;
        }

        let index_map = bucket_contexts
            .iter()
            .enumerate()
            .map(|(index, context)| (context.span.span_id.0.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut ranges = initial_rebuild_ranges(
            &bucket_contexts,
            &index_map,
            &changed_span_ids,
            &bucket_deleted,
        );

        if ranges.is_empty() {
            for layout in &bucket_layouts {
                if layout
                    .span_ids
                    .iter()
                    .any(|span_id| deleted_span_ids.contains(span_id))
                {
                    plan.work_item_ids_to_delete
                        .insert(layout.work_item_id.0.clone());
                }
            }
            continue;
        }

        ranges = merge_index_ranges(expand_ranges_by_window(
            &merge_index_ranges(ranges),
            bucket_contexts.len(),
            TOPIC_COHESION_WINDOW_SPANS,
        ));

        // A touched layout can expand the rebuild segment far enough to reach other
        // existing layouts. Keep expanding until both the ranges and delete set
        // stabilize so rebuilt inserts never race leftover rows.
        loop {
            let delete_count_before = plan.work_item_ids_to_delete.len();
            let mut additional_bounds = Vec::new();

            for layout in &bucket_layouts {
                let touched_by_deleted = layout
                    .span_ids
                    .iter()
                    .any(|span_id| deleted_span_ids.contains(span_id));
                let touched_by_changed = layout
                    .span_ids
                    .iter()
                    .any(|span_id| changed_span_ids.contains(span_id));
                if !(touched_by_deleted
                    || touched_by_changed
                    || ranges_intersect_layout(&ranges, &index_map, layout))
                {
                    continue;
                }
                plan.work_item_ids_to_delete
                    .insert(layout.work_item_id.0.clone());
                if let Some(bounds) = layout_bounds(layout, &index_map) {
                    additional_bounds.push(bounds);
                }
            }

            for verification in verifications {
                let TaskVerificationAction::Merge {
                    left_anchor_span_id,
                    right_anchor_span_id,
                    ..
                } = &verification.action
                else {
                    continue;
                };
                let left_touched = range_or_deleted_contains_span_id(
                    &ranges,
                    &index_map,
                    &deleted_span_ids,
                    &left_anchor_span_id.0,
                );
                let right_touched = range_or_deleted_contains_span_id(
                    &ranges,
                    &index_map,
                    &deleted_span_ids,
                    &right_anchor_span_id.0,
                );
                if !left_touched && !right_touched {
                    continue;
                }
                for layout in &bucket_layouts {
                    if !layout.span_ids.iter().any(|span_id| {
                        span_id == &left_anchor_span_id.0 || span_id == &right_anchor_span_id.0
                    }) {
                        continue;
                    }
                    plan.work_item_ids_to_delete
                        .insert(layout.work_item_id.0.clone());
                    if let Some(bounds) = layout_bounds(layout, &index_map) {
                        additional_bounds.push(bounds);
                    }
                }
            }

            let mut expanded_ranges = ranges.clone();
            expanded_ranges.extend(additional_bounds);
            expanded_ranges = merge_index_ranges(expanded_ranges);
            let stabilized = expanded_ranges == ranges
                && plan.work_item_ids_to_delete.len() == delete_count_before;
            ranges = expanded_ranges;
            if stabilized {
                break;
            }
        }

        for (start, end) in ranges {
            if start >= bucket_contexts.len() || start > end {
                continue;
            }
            let slice = bucket_contexts[start..=end].to_vec();
            if slice.is_empty() {
                continue;
            }
            plan.touched_span_count = plan.touched_span_count.saturating_add(slice.len() as u64);
            plan.segments
                .push(LocalizedRebuildSegment { contexts: slice });
        }
    }

    plan
}

fn initial_rebuild_ranges(
    bucket_contexts: &[SpanContext],
    index_map: &HashMap<String, usize>,
    changed_span_ids: &HashSet<String>,
    deleted_spans: &[DeletedTaskSpanRef],
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for (span_id, index) in index_map {
        if changed_span_ids.contains(span_id) {
            ranges.push((*index, *index));
        }
    }
    for deleted in deleted_spans {
        let insertion_index = bucket_contexts
            .binary_search_by(|context| context.span.started_at.cmp(&deleted.started_at))
            .unwrap_or_else(|index| index);
        if bucket_contexts.is_empty() {
            continue;
        }
        let start = insertion_index.saturating_sub(1);
        let end = insertion_index.min(bucket_contexts.len().saturating_sub(1));
        ranges.push((start, end));
    }
    merge_index_ranges(ranges)
}

fn merge_index_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut merged = Vec::with_capacity(ranges.len());
    let mut current = ranges[0];
    for range in ranges.into_iter().skip(1) {
        if range.0 <= current.1.saturating_add(1) {
            current.1 = current.1.max(range.1);
        } else {
            merged.push(current);
            current = range;
        }
    }
    merged.push(current);
    merged
}

fn expand_ranges_by_window(
    ranges: &[(usize, usize)],
    context_len: usize,
    window: usize,
) -> Vec<(usize, usize)> {
    if context_len == 0 {
        return Vec::new();
    }
    ranges
        .iter()
        .map(|(start, end)| {
            (
                start.saturating_sub(window),
                end.saturating_add(window)
                    .min(context_len.saturating_sub(1)),
            )
        })
        .collect()
}

fn layout_bounds(
    layout: &ExistingWorkItemLayout,
    index_map: &HashMap<String, usize>,
) -> Option<(usize, usize)> {
    let mut indices = layout
        .span_ids
        .iter()
        .filter_map(|span_id| index_map.get(span_id).copied());
    let first = indices.next()?;
    let mut min_index = first;
    let mut max_index = first;
    for index in indices {
        min_index = min_index.min(index);
        max_index = max_index.max(index);
    }
    Some((min_index, max_index))
}

fn ranges_intersect_layout(
    ranges: &[(usize, usize)],
    index_map: &HashMap<String, usize>,
    layout: &ExistingWorkItemLayout,
) -> bool {
    layout
        .span_ids
        .iter()
        .filter_map(|span_id| index_map.get(span_id).copied())
        .any(|index| {
            ranges
                .iter()
                .any(|(start, end)| *start <= index && index <= *end)
        })
}

fn range_or_deleted_contains_span_id(
    ranges: &[(usize, usize)],
    index_map: &HashMap<String, usize>,
    deleted_span_ids: &HashSet<String>,
    span_id: &str,
) -> bool {
    deleted_span_ids.contains(span_id)
        || index_map.get(span_id).is_some_and(|index| {
            ranges
                .iter()
                .any(|(start, end)| *start <= *index && *index <= *end)
        })
}

fn work_item_members_map_from_members(members: &[WorkItemMember]) -> HashMap<String, String> {
    let mut assignments = HashMap::new();
    for member in members {
        assignments.insert(member.span_id.0.clone(), member.work_item_id.0.clone());
    }
    assignments
}

pub(crate) fn task_status_as_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Auto => "auto",
        TaskStatus::NeedsReview => "needs_review",
        TaskStatus::Verified => "verified",
        TaskStatus::RejectedMeta => "rejected_meta",
    }
}

pub(crate) fn confidence_as_str(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}

pub(crate) fn bool_to_i64(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

pub(crate) fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests;
