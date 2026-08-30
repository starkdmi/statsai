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
mod grouping;
mod titles;

pub(crate) use benchmark::*;
pub(crate) use grouping::*;
pub(crate) use titles::*;

pub(crate) const TOPIC_COHESION_WINDOW_SPANS: usize = 2;
const SQLITE_BUCKET_CHUNK_SIZE: usize = 300;

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

pub fn derive_task_work_items(
    spans: Vec<TaskSpan>,
    verifications: &[TaskVerification],
) -> (Vec<WorkItem>, Vec<WorkItemMember>) {
    let contexts = spans.into_iter().map(SpanContext::from).collect::<Vec<_>>();
    let (work_items, members, _) = build_work_items(contexts, verifications);
    (work_items, members)
}

#[derive(Debug, Clone)]
pub(crate) struct SpanContext {
    span: TaskSpan,
    topic_tokens: BTreeSet<String>,
    title_is_generic: bool,
    title_is_weak_signal: bool,
    title_signal_score: i32,
}

impl From<TaskSpan> for SpanContext {
    fn from(span: TaskSpan) -> Self {
        let title_is_generic = task_title_is_generic(Some(span.title.as_str()));
        let title_is_weak_signal = task_title_is_weak_signal(Some(span.title.as_str()));
        let title_signal_score = task_title_signal_score(Some(span.title.as_str()));
        let mut topic_tokens = title_topic_tokens(&span.title);
        let should_expand_topic_context = topic_tokens.is_empty()
            || title_is_generic
            || title_is_weak_signal
            || title_signal_score < 8;
        if should_expand_topic_context {
            if let Some(summary_preview) = span.summary_preview.as_deref() {
                topic_tokens.extend(title_topic_tokens(summary_preview));
            }
            if let Some(todo_excerpt) = span.todo_excerpt.as_deref() {
                topic_tokens.extend(title_topic_tokens(todo_excerpt));
            }
        }
        Self {
            span,
            topic_tokens,
            title_is_generic,
            title_is_weak_signal,
            title_signal_score,
        }
    }
}

impl SpanContext {
    fn ended_at(&self) -> DateTime<Utc> {
        self.span.effective_ended_at()
    }

    fn session_key(&self) -> Option<&str> {
        self.span
            .thread_id
            .as_deref()
            .or(self.span.session_id.as_deref())
    }

    fn topic_tokens(&self) -> &BTreeSet<String> {
        &self.topic_tokens
    }

    fn title_is_generic(&self) -> bool {
        self.title_is_generic
    }

    fn title_is_weak_signal(&self) -> bool {
        self.title_is_weak_signal
    }

    fn title_signal_score(&self) -> i32 {
        self.title_signal_score
    }

    fn usage(&self) -> UsageCounts {
        self.span.usage.clone()
    }

    fn estimated_cost_usd(&self) -> Option<i64> {
        self.span.estimated_cost_usd
    }

    fn estimated_cost_micro_usd(&self) -> Option<i64> {
        self.span.estimated_cost_micro_usd
    }

    fn event_count(&self) -> u64 {
        self.span.effective_event_count()
    }

    fn has_usage_evidence(&self) -> bool {
        self.span.effective_has_usage_evidence()
    }

    fn total_messages(&self) -> u64 {
        self.span.total_messages
    }

    fn user_messages(&self) -> u64 {
        self.span.user_messages
    }

    fn assistant_messages(&self) -> u64 {
        self.span.assistant_messages
    }

    fn developer_messages(&self) -> u64 {
        self.span.developer_messages
    }
}

fn sqlite_in_clause_placeholders(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(",")
}

fn sqlite_string_params(values: &[String]) -> Vec<&dyn rusqlite::types::ToSql> {
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
    pub fn upsert_task_spans(&self, spans: &[TaskSpan]) -> Result<u64> {
        if spans.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| self.upsert_task_spans_in_tx(spans))
    }

    pub fn replace_task_bucket_snapshot(&self, snapshot: &TaskBucketSnapshot) -> Result<()> {
        self.with_immediate_transaction(|| {
            self.delete_task_bucket_snapshot_in_tx(&snapshot.project_bucket)?;
            self.upsert_task_spans_in_tx(&snapshot.spans)?;
            self.insert_work_items_in_tx(&snapshot.work_items, &snapshot.members)?;
            let mut changed_buckets = BTreeSet::new();
            changed_buckets.insert(snapshot.project_bucket.clone());
            self.mark_task_buckets_dirty_in_tx(&changed_buckets)?;
            Ok(())
        })
    }

    pub fn task_bucket_has_newer_verifications(
        &self,
        project_bucket: &str,
        cursor: Option<&TaskVerificationCursor>,
    ) -> Result<bool> {
        let mut project_buckets = BTreeSet::new();
        project_buckets.insert(project_bucket.to_string());
        let relevant = self.relevant_task_verifications(&project_buckets)?;
        let latest = latest_task_verification(relevant.iter());
        Ok(latest
            .as_ref()
            .is_some_and(|verification| task_verification_is_after_cursor(verification, cursor)))
    }

    pub fn delete_task_spans_for_sources(
        &self,
        source_ids: &[SourceId],
    ) -> Result<TaskDeletionImpact> {
        if source_ids.is_empty() {
            return Ok(TaskDeletionImpact::default());
        }
        self.with_immediate_transaction(|| {
            let targets = self.task_span_targets_for_sources(source_ids)?;
            self.delete_task_span_targets_in_tx(&targets)
        })
    }

    pub fn delete_task_spans_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<TaskDeletionImpact> {
        if file_hashes.is_empty() {
            return Ok(TaskDeletionImpact::default());
        }
        self.with_immediate_transaction(|| {
            let targets = self.task_span_targets_for_source_file_hashes(source_id, file_hashes)?;
            self.delete_task_span_targets_in_tx(&targets)
        })
    }

    pub fn task_spans(&self) -> Result<Vec<TaskSpan>> {
        self.task_spans_by_sql(
            "SELECT payload FROM task_spans ORDER BY project_bucket, started_at, span_id",
            &[],
        )
    }

    pub fn task_spans_for_work_item(&self, work_item_id: &WorkItemId) -> Result<Vec<TaskSpan>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT s.payload
            FROM task_work_item_members m
            JOIN task_spans s ON s.span_id = m.span_id
            WHERE m.work_item_id = ?1
            ORDER BY m.ordinal, s.started_at, s.span_id
            "#,
        )?;
        let rows = statement.query_map(params![&work_item_id.0], |row| row.get::<_, String>(0))?;
        let mut spans = Vec::new();
        for row in rows {
            spans.push(serde_json::from_str(&row?)?);
        }
        Ok(spans)
    }

    pub fn work_items(&self) -> Result<Vec<WorkItem>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload
            FROM task_work_items
            ORDER BY
              CASE status
                WHEN 'needs_review' THEN 0
                WHEN 'auto' THEN 1
                WHEN 'verified' THEN 2
                ELSE 3
              END,
              CASE confidence
                WHEN 'low' THEN 0
                WHEN 'medium' THEN 1
                ELSE 2
              END,
              total_tokens DESC,
              ended_at DESC,
              work_item_id
            "#,
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut work_items = Vec::new();
        for row in rows {
            work_items.push(serde_json::from_str(&row?)?);
        }
        Ok(work_items)
    }

    pub fn work_item(&self, work_item_id: &WorkItemId) -> Result<Option<WorkItem>> {
        self.conn
            .query_row(
                "SELECT payload FROM task_work_items WHERE work_item_id = ?1",
                params![&work_item_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    pub fn task_stats(&self) -> Result<TaskStats> {
        let total_spans: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM task_spans", [], |row| row.get(0))?;
        let total_work_items: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM task_work_items", [], |row| row.get(0))?;
        let total_members: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM task_work_item_members", [], |row| {
                    row.get(0)
                })?;
        let work_items = self.work_items()?;
        let total_work_items_f64 = total_work_items.max(0) as f64;
        let ratio = |predicate: fn(&WorkItem) -> bool| -> f64 {
            if total_work_items_f64 == 0.0 {
                0.0
            } else {
                work_items.iter().filter(|item| predicate(item)).count() as f64 * 100.0
                    / total_work_items_f64
            }
        };
        Ok(TaskStats {
            total_spans: total_spans.max(0) as u64,
            total_work_items: total_work_items.max(0) as u64,
            verified_percentage: ratio(|item| item.status == TaskStatus::Verified),
            no_git_percentage: ratio(|item| item.no_git),
            cross_provider_percentage: ratio(|item| item.cross_provider),
            rejected_meta_percentage: ratio(|item| item.status == TaskStatus::RejectedMeta),
            average_spans_per_work_item: if total_work_items == 0 {
                0.0
            } else {
                total_members.max(0) as f64 / total_work_items as f64
            },
        })
    }

    pub fn upsert_task_verification(
        &self,
        action: TaskVerificationAction,
    ) -> Result<TaskVerification> {
        let now = Utc::now();
        let action_kind = action.action_kind().to_string();
        let action_key = action.action_key();
        let conflicting = self.conflicting_task_verifications(&action)?;
        let existing = latest_task_verification(conflicting.iter());
        let verification = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: existing
                .as_ref()
                .map(|verification| verification.verification_id.clone())
                .unwrap_or_else(|| task_verification_id(&action_kind, &action_key)),
            action_key: action_key.clone(),
            action,
            created_at: existing
                .as_ref()
                .map(|verification| verification.created_at)
                .unwrap_or(now),
            updated_at: now,
        };
        let payload = serde_json::to_string(&verification)?;
        super::begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.delete_task_verifications_by_ids(
                &conflicting
                    .iter()
                    .map(|verification| verification.verification_id.0.as_str())
                    .collect::<Vec<_>>(),
            )?;
            self.conn.execute(
                r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    &verification.verification_id.0,
                    &action_kind,
                    &verification.action_key,
                    verification.updated_at.to_rfc3339(),
                    &payload,
                ],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                commit_transaction(&self.conn)?;
            }
            Err(error) => {
                rollback(&self.conn);
                return Err(error);
            }
        }
        Ok(verification)
    }

    pub fn task_verifications(&self) -> Result<Vec<TaskVerification>> {
        let mut statement = self.conn.prepare(
            "SELECT payload FROM task_verifications ORDER BY updated_at, verification_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut verifications = Vec::new();
        for row in rows {
            verifications.push(serde_json::from_str(&row?)?);
        }
        Ok(resolve_task_verifications(verifications))
    }

    pub fn task_project_buckets(&self) -> Result<BTreeSet<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT project_bucket FROM task_spans ORDER BY project_bucket")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut buckets = BTreeSet::new();
        for row in rows {
            buckets.insert(row?);
        }
        Ok(buckets)
    }

    pub fn task_bucket_snapshot(
        &self,
        project_bucket: &str,
        applied_verification_cursor: Option<TaskVerificationCursor>,
    ) -> Result<TaskBucketSnapshot> {
        let spans = self.task_spans_by_sql(
            "SELECT payload FROM task_spans WHERE project_bucket = ?1 ORDER BY started_at, span_id",
            &[&project_bucket],
        )?;
        let work_items = self.work_items_for_project_bucket(project_bucket)?;
        let members = self.work_item_members_for_project_bucket(project_bucket)?;
        Ok(TaskBucketSnapshot {
            project_bucket: project_bucket.to_string(),
            generated_at: Utc::now(),
            applied_verification_cursor,
            work_items,
            members,
            spans,
        })
    }

    pub fn pending_task_bucket_snapshots_for_sync(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
        full: bool,
        applied_verification_cursor: Option<TaskVerificationCursor>,
    ) -> Result<Vec<TaskBucketSnapshot>> {
        let local_buckets = self.task_project_buckets()?;
        let tracked_dirty_buckets =
            self.dirty_task_bucket_keys_for_sync(sink, target, device_id)?;
        let bucket_ids = if full {
            local_buckets
                .union(&tracked_dirty_buckets)
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            local_buckets
                .iter()
                .filter(|bucket| {
                    !self.task_bucket_is_clean_for_sync(sink, target, device_id, bucket)
                })
                .cloned()
                .chain(tracked_dirty_buckets.difference(&local_buckets).cloned())
                .collect::<BTreeSet<_>>()
        };
        bucket_ids
            .into_iter()
            .map(|bucket| self.task_bucket_snapshot(&bucket, applied_verification_cursor.clone()))
            .collect()
    }

    pub fn pending_task_verifications_for_sync(
        &self,
        sink: &str,
        target: &str,
    ) -> Result<Vec<TaskVerification>> {
        let verifications = self.task_verifications()?;
        let mut pending = Vec::new();
        for verification in verifications {
            let payload_hash = task_verification_payload_hash(&verification)?;
            if self.entity_requires_sync(
                sink,
                target,
                "task_verification",
                &verification.verification_id.0,
                &payload_hash,
            )? {
                pending.push(verification);
            }
        }
        Ok(pending)
    }

    pub fn record_task_bucket_snapshots_synced(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
        snapshots: &[TaskBucketSnapshot],
    ) -> Result<()> {
        if snapshots.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_task_bucket_snapshots_synced_in_transaction(
                sink, target, device_id, snapshots,
            )
        })
    }

    pub(super) fn record_task_bucket_snapshots_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
        snapshots: &[TaskBucketSnapshot],
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let mut statement = self.conn.prepare(
            r#"
            INSERT INTO task_bucket_sync_state (
              sink, target, device_id, project_bucket, dirty, payload_hash, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)
            ON CONFLICT(sink, target, device_id, project_bucket) DO UPDATE SET
              dirty = 0,
              payload_hash = excluded.payload_hash,
              updated_at = excluded.updated_at
            "#,
        )?;
        for snapshot in snapshots {
            statement.execute(params![
                sink,
                target,
                device_id,
                &snapshot.project_bucket,
                task_bucket_snapshot_payload_hash(snapshot)?,
                &now,
            ])?;
        }
        Ok(())
    }

    pub fn record_task_verifications_synced(
        &self,
        sink: &str,
        target: &str,
        verifications: &[TaskVerification],
    ) -> Result<()> {
        if verifications.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_task_verifications_synced_in_transaction(sink, target, verifications)
        })
    }

    pub(super) fn record_task_verifications_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        verifications: &[TaskVerification],
    ) -> Result<()> {
        for verification in verifications {
            self.record_entity_synced(
                sink,
                target,
                "task_verification",
                &verification.verification_id.0,
                &task_verification_payload_hash(verification)?,
            )?;
        }
        Ok(())
    }

    pub fn merge_task_verification(&self, verification: &TaskVerification) -> Result<bool> {
        let conflicting = self.conflicting_task_verifications(&verification.action)?;
        let existing = latest_task_verification(conflicting.iter());
        if existing
            .as_ref()
            .is_some_and(|current| !task_verification_is_newer(verification, current))
        {
            return Ok(false);
        }

        let mut verification = verification.clone();
        verification.action_key = verification.action.action_key();
        let payload = serde_json::to_string(&verification)?;
        self.with_immediate_transaction(|| {
            self.delete_task_verifications_by_ids(
                &conflicting
                    .iter()
                    .map(|current| current.verification_id.0.as_str())
                    .collect::<Vec<_>>(),
            )?;
            self.conn.execute(
                r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    &verification.verification_id.0,
                    verification.action.action_kind(),
                    &verification.action_key,
                    verification.updated_at.to_rfc3339(),
                    &payload,
                ],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    pub fn project_buckets_for_task_verification(
        &self,
        verification: &TaskVerification,
    ) -> Result<BTreeSet<String>> {
        let span_ids = verification
            .action
            .span_ids()
            .into_iter()
            .map(|span_id| span_id.0.clone())
            .collect::<Vec<_>>();
        if span_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let mut buckets = BTreeSet::new();
        let mut statement = self
            .conn
            .prepare("SELECT project_bucket FROM task_spans WHERE span_id = ?1")?;
        for span_id in &span_ids {
            if let Some(project_bucket) = statement
                .query_row(params![span_id], |row| row.get::<_, String>(0))
                .optional()?
            {
                buckets.insert(project_bucket);
            }
        }
        Ok(buckets)
    }

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

    pub(crate) fn task_spans_by_sql(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
    ) -> Result<Vec<TaskSpan>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
        let mut spans = Vec::new();
        for row in rows {
            spans.push(serde_json::from_str(&row?)?);
        }
        Ok(spans)
    }

    pub(crate) fn upsert_task_spans_in_tx(&self, spans: &[TaskSpan]) -> Result<u64> {
        let mut changed = 0u64;
        let mut span_stmt = self.conn.prepare(
            r#"
            INSERT INTO task_spans (
              span_id, provider, source_id, project_bucket, started_at, ended_at, title,
              normalized_title, is_meta, confidence, source_file_path_hash, event_count,
              has_usage_evidence, total_messages, user_messages, assistant_messages,
              developer_messages, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            ON CONFLICT(span_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              project_bucket = excluded.project_bucket,
              started_at = excluded.started_at,
              ended_at = excluded.ended_at,
              title = excluded.title,
              normalized_title = excluded.normalized_title,
              is_meta = excluded.is_meta,
              confidence = excluded.confidence,
              source_file_path_hash = excluded.source_file_path_hash,
              event_count = excluded.event_count,
              has_usage_evidence = excluded.has_usage_evidence,
              total_messages = excluded.total_messages,
              user_messages = excluded.user_messages,
              assistant_messages = excluded.assistant_messages,
              developer_messages = excluded.developer_messages,
              payload = excluded.payload
            "#,
        )?;
        let mut delete_links = self
            .conn
            .prepare("DELETE FROM task_span_event_links WHERE span_id = ?1")?;
        let mut link_stmt = self.conn.prepare(
            r#"
            INSERT INTO task_span_event_links (span_id, event_id)
            VALUES (?1, ?2)
            ON CONFLICT(span_id, event_id) DO NOTHING
            "#,
        )?;
        for span in spans {
            let payload = serde_json::to_string(span)?;
            changed += span_stmt.execute(params![
                &span.span_id.0,
                &span.provider,
                &span.source_id.0,
                &span.project_bucket,
                span.started_at.to_rfc3339(),
                span.ended_at.map(|value| value.to_rfc3339()),
                &span.title,
                &span.normalized_title,
                bool_to_i64(span.is_meta),
                confidence_as_str(span.confidence.clone()),
                span.source_file_path_hash.as_deref(),
                safe_u64_to_i64(span.effective_event_count()),
                bool_to_i64(span.effective_has_usage_evidence()),
                safe_u64_to_i64(span.total_messages),
                safe_u64_to_i64(span.user_messages),
                safe_u64_to_i64(span.assistant_messages),
                safe_u64_to_i64(span.developer_messages),
                &payload,
            ])? as u64;
            delete_links.execute(params![&span.span_id.0])?;
            for event_id in &span.linked_event_ids {
                link_stmt.execute(params![&span.span_id.0, &event_id.0])?;
            }
        }
        Ok(changed)
    }

    fn delete_task_bucket_snapshot_in_tx(&self, project_bucket: &str) -> Result<()> {
        let span_ids = self
            .task_spans_by_sql(
                "SELECT payload FROM task_spans WHERE project_bucket = ?1 ORDER BY started_at, span_id",
                &[&project_bucket],
            )?
            .into_iter()
            .map(|span| span.span_id)
            .collect::<Vec<_>>();
        let mut delete_links = self
            .conn
            .prepare("DELETE FROM task_span_event_links WHERE span_id = ?1")?;
        let mut delete_spans = self
            .conn
            .prepare("DELETE FROM task_spans WHERE span_id = ?1")?;
        for span_id in &span_ids {
            delete_links.execute(params![&span_id.0])?;
            delete_spans.execute(params![&span_id.0])?;
        }
        self.conn.execute(
            r#"
            DELETE FROM task_work_item_members
            WHERE work_item_id IN (
              SELECT work_item_id
              FROM task_work_items
              WHERE project_bucket = ?1
            )
            "#,
            params![project_bucket],
        )?;
        self.conn.execute(
            "DELETE FROM task_work_items WHERE project_bucket = ?1",
            params![project_bucket],
        )?;
        Ok(())
    }

    fn task_span_targets_for_sources(
        &self,
        source_ids: &[SourceId],
    ) -> Result<Vec<DeletedTaskSpanRef>> {
        let mut targets = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT span_id, project_bucket, started_at FROM task_spans WHERE source_id = ?1 ORDER BY started_at, span_id",
        )?;
        for source_id in source_ids {
            let rows = statement.query_map(params![&source_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (span_id, project_bucket, started_at) = row?;
                targets.push(DeletedTaskSpanRef {
                    span_id: TaskSpanId(span_id),
                    project_bucket,
                    started_at: parse_rfc3339_utc(&started_at)?,
                });
            }
        }
        Ok(targets)
    }

    fn task_span_targets_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<Vec<DeletedTaskSpanRef>> {
        let mut targets = Vec::new();
        let mut statement = self.conn.prepare(
            r#"
            SELECT span_id, project_bucket, started_at
            FROM task_spans
            WHERE source_id = ?1 AND source_file_path_hash = ?2
            ORDER BY started_at, span_id
            "#,
        )?;
        for file_hash in file_hashes {
            let rows = statement.query_map(params![&source_id.0, file_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (span_id, project_bucket, started_at) = row?;
                targets.push(DeletedTaskSpanRef {
                    span_id: TaskSpanId(span_id),
                    project_bucket,
                    started_at: parse_rfc3339_utc(&started_at)?,
                });
            }
        }
        Ok(targets)
    }

    fn delete_task_span_targets_in_tx(
        &self,
        targets: &[DeletedTaskSpanRef],
    ) -> Result<TaskDeletionImpact> {
        if targets.is_empty() {
            return Ok(TaskDeletionImpact::default());
        }
        let mut delete_links = self
            .conn
            .prepare("DELETE FROM task_span_event_links WHERE span_id = ?1")?;
        let mut delete_spans = self
            .conn
            .prepare("DELETE FROM task_spans WHERE span_id = ?1")?;
        let mut deleted = 0u64;
        let mut affected_project_buckets = BTreeSet::new();
        for target in targets {
            affected_project_buckets.insert(target.project_bucket.clone());
            delete_links.execute(params![&target.span_id.0])?;
            deleted += delete_spans.execute(params![&target.span_id.0])? as u64;
        }
        Ok(TaskDeletionImpact {
            deleted,
            affected_project_buckets,
            deleted_spans: targets.to_vec(),
        })
    }

    fn delete_task_work_items_for_project_buckets_in_tx(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<u64> {
        if project_buckets.is_empty() {
            return Ok(0);
        }
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        let mut deleted = 0u64;
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let params = sqlite_string_params(chunk);
            let count_sql = format!(
                "SELECT COUNT(*) FROM task_work_items WHERE project_bucket IN ({placeholders})"
            );
            deleted += self
                .conn
                .query_row(&count_sql, params.as_slice(), |row| row.get::<_, u64>(0))?;

            let delete_members_sql = format!(
                r#"
                DELETE FROM task_work_item_members
                WHERE work_item_id IN (
                  SELECT work_item_id
                  FROM task_work_items
                  WHERE project_bucket IN ({placeholders})
                )
                "#
            );
            self.conn.execute(&delete_members_sql, params.as_slice())?;

            let delete_items_sql =
                format!("DELETE FROM task_work_items WHERE project_bucket IN ({placeholders})");
            self.conn.execute(&delete_items_sql, params.as_slice())?;
        }
        Ok(deleted)
    }

    fn insert_work_items_in_tx(
        &self,
        work_items: &[WorkItem],
        members: &[WorkItemMember],
    ) -> Result<()> {
        let mut item_stmt = self.conn.prepare(
            r#"
            INSERT INTO task_work_items (
              work_item_id, anchor_span_id, project_bucket, started_at, ended_at, status,
              confidence, total_tokens, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )?;
        let mut member_stmt = self.conn.prepare(
            r#"
            INSERT INTO task_work_item_members (work_item_id, span_id, ordinal)
            VALUES (?1, ?2, ?3)
            "#,
        )?;
        for work_item in work_items {
            let payload = serde_json::to_string(work_item)?;
            item_stmt.execute(params![
                &work_item.work_item_id.0,
                &work_item.anchor_span_id.0,
                &work_item.project_bucket,
                work_item.started_at.to_rfc3339(),
                work_item.ended_at.to_rfc3339(),
                task_status_as_str(&work_item.status),
                confidence_as_str(work_item.confidence.clone()),
                safe_u64_to_i64(work_item.total_tokens),
                &payload,
            ])?;
        }
        for member in members {
            member_stmt.execute(params![
                &member.work_item_id.0,
                &member.span_id.0,
                member.ordinal as i64,
            ])?;
        }
        Ok(())
    }

    fn load_span_contexts_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<Vec<SpanContext>> {
        let mut contexts = Vec::<SpanContext>::new();
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql = format!(
                "SELECT payload FROM task_spans \
                 WHERE project_bucket IN ({placeholders}) \
                 ORDER BY project_bucket, started_at, span_id"
            );
            let params = sqlite_string_params(chunk);
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                contexts.push(SpanContext::from(serde_json::from_str::<TaskSpan>(&row?)?));
            }
        }
        Ok(contexts)
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

    fn work_item_members_map(&self) -> Result<HashMap<String, String>> {
        let mut statement = self.conn.prepare(
            "SELECT work_item_id, span_id FROM task_work_item_members ORDER BY work_item_id, ordinal",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut assignments = HashMap::new();
        for row in rows {
            let (work_item_id, span_id) = row?;
            assignments.insert(span_id, work_item_id);
        }
        Ok(assignments)
    }

    fn work_items_for_project_bucket(&self, project_bucket: &str) -> Result<Vec<WorkItem>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload
            FROM task_work_items
            WHERE project_bucket = ?1
            ORDER BY started_at, work_item_id
            "#,
        )?;
        let rows = statement.query_map(params![project_bucket], |row| row.get::<_, String>(0))?;
        let mut work_items = Vec::new();
        for row in rows {
            work_items.push(serde_json::from_str(&row?)?);
        }
        Ok(work_items)
    }

    fn work_item_members_for_project_bucket(
        &self,
        project_bucket: &str,
    ) -> Result<Vec<WorkItemMember>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT m.work_item_id, m.span_id, m.ordinal
            FROM task_work_item_members m
            JOIN task_work_items w ON w.work_item_id = m.work_item_id
            WHERE w.project_bucket = ?1
            ORDER BY w.started_at, m.ordinal, m.span_id
            "#,
        )?;
        let rows = statement.query_map(params![project_bucket], |row| {
            Ok(WorkItemMember {
                work_item_id: WorkItemId(row.get(0)?),
                span_id: TaskSpanId(row.get(1)?),
                ordinal: row.get::<_, i64>(2)?.max(0) as usize,
            })
        })?;
        let mut members = Vec::new();
        for row in rows {
            members.push(row?);
        }
        Ok(members)
    }

    fn dirty_task_bucket_keys_for_sync(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
    ) -> Result<BTreeSet<String>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT project_bucket
            FROM task_bucket_sync_state
            WHERE sink = ?1 AND target = ?2 AND device_id = ?3 AND dirty = 1
            ORDER BY project_bucket
            "#,
        )?;
        let rows = statement.query_map(params![sink, target, device_id], |row| {
            row.get::<_, String>(0)
        })?;
        let mut buckets = BTreeSet::new();
        for row in rows {
            buckets.insert(row?);
        }
        Ok(buckets)
    }

    fn task_bucket_is_clean_for_sync(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
        project_bucket: &str,
    ) -> bool {
        self.conn
            .query_row(
                r#"
                SELECT dirty
                FROM task_bucket_sync_state
                WHERE sink = ?1 AND target = ?2 AND device_id = ?3 AND project_bucket = ?4
                "#,
                params![sink, target, device_id, project_bucket],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|row| row.is_some_and(|dirty| dirty == 0))
            .unwrap_or(false)
    }

    fn mark_task_buckets_dirty_in_tx(&self, project_buckets: &BTreeSet<String>) -> Result<()> {
        if project_buckets.is_empty() {
            return Ok(());
        }
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        let now = Utc::now().to_rfc3339();
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql = format!(
                "UPDATE task_bucket_sync_state \
                 SET dirty = 1, updated_at = ?1 \
                 WHERE project_bucket IN ({placeholders})"
            );
            let mut params: Vec<&dyn rusqlite::types::ToSql> = vec![&now];
            params.extend(sqlite_string_params(chunk));
            self.conn.execute(&sql, params.as_slice())?;
        }
        Ok(())
    }

    fn conflicting_task_verifications(
        &self,
        action: &TaskVerificationAction,
    ) -> Result<Vec<TaskVerification>> {
        Ok(self
            .task_verifications_by_action_keys(&task_verification_lookup_keys(action))?
            .into_iter()
            .filter(|current| {
                task_verification_resolution_key(&current.action)
                    == task_verification_resolution_key(action)
            })
            .collect())
    }

    fn task_verifications_by_action_keys(
        &self,
        action_keys: &[String],
    ) -> Result<Vec<TaskVerification>> {
        let mut statement = self
            .conn
            .prepare("SELECT payload FROM task_verifications WHERE action_key = ?1")?;
        let mut verifications = Vec::new();
        for action_key in action_keys {
            let rows = statement.query_map(params![action_key], |row| row.get::<_, String>(0))?;
            for row in rows {
                verifications.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(verifications)
    }

    fn delete_task_verifications_by_ids(&self, verification_ids: &[&str]) -> Result<()> {
        let mut statement = self
            .conn
            .prepare("DELETE FROM task_verifications WHERE verification_id = ?1")?;
        for verification_id in verification_ids {
            statement.execute(params![verification_id])?;
        }
        Ok(())
    }

    fn relevant_task_verifications(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<Vec<TaskVerification>> {
        if project_buckets.is_empty() {
            return Ok(Vec::new());
        }
        let verifications = self.task_verifications()?;
        let relevant_span_ids = self.span_ids_for_project_buckets(project_buckets)?;
        Ok(verifications
            .into_iter()
            .filter(|verification| {
                verification
                    .action
                    .span_ids()
                    .into_iter()
                    .any(|span_id| relevant_span_ids.contains(span_id.0.as_str()))
            })
            .collect())
    }

    fn span_ids_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<HashSet<String>> {
        let buckets = project_buckets.iter().cloned().collect::<Vec<_>>();
        let mut span_ids = HashSet::new();
        for chunk in buckets.chunks(SQLITE_BUCKET_CHUNK_SIZE) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql =
                format!("SELECT span_id FROM task_spans WHERE project_bucket IN ({placeholders})");
            let params = sqlite_string_params(chunk);
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                span_ids.insert(row?);
            }
        }
        Ok(span_ids)
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

fn task_verification_lookup_keys(action: &TaskVerificationAction) -> Vec<String> {
    match action.anchor_span_id() {
        Some(anchor_span_id) => vec![
            format!("status:{}", anchor_span_id.0),
            format!("rename:{}", anchor_span_id.0),
            format!("anchor:{}", anchor_span_id.0),
            format!("accept:{}", anchor_span_id.0),
            format!("reject:{}", anchor_span_id.0),
        ],
        None => vec![action.action_key()],
    }
}

fn task_verification_resolution_key(action: &TaskVerificationAction) -> String {
    match action {
        TaskVerificationAction::Accept { anchor_span_id, .. }
        | TaskVerificationAction::Reject { anchor_span_id, .. } => {
            format!("status:{}", anchor_span_id.0)
        }
        TaskVerificationAction::Rename { anchor_span_id, .. } => {
            format!("rename:{}", anchor_span_id.0)
        }
        TaskVerificationAction::Split { .. } | TaskVerificationAction::Merge { .. } => {
            action.action_key()
        }
    }
}

fn latest_task_verification<'a>(
    verifications: impl Iterator<Item = &'a TaskVerification>,
) -> Option<TaskVerification> {
    verifications
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
        })
        .cloned()
}

fn work_item_members_map_from_members(members: &[WorkItemMember]) -> HashMap<String, String> {
    let mut assignments = HashMap::new();
    for member in members {
        assignments.insert(member.span_id.0.clone(), member.work_item_id.0.clone());
    }
    assignments
}

fn task_bucket_snapshot_payload_hash(snapshot: &TaskBucketSnapshot) -> Result<String> {
    Ok(hash_text(&serde_json::to_string(snapshot)?))
}

fn task_verification_payload_hash(verification: &TaskVerification) -> Result<String> {
    Ok(hash_text(&serde_json::to_string(verification)?))
}

fn task_verification_is_newer(left: &TaskVerification, right: &TaskVerification) -> bool {
    left.updated_at > right.updated_at
        || (left.updated_at == right.updated_at && left.verification_id.0 > right.verification_id.0)
}

fn task_verification_is_after_cursor(
    verification: &TaskVerification,
    cursor: Option<&TaskVerificationCursor>,
) -> bool {
    let Some(cursor) = cursor else {
        return true;
    };
    verification.updated_at > cursor.updated_at
        || (verification.updated_at == cursor.updated_at
            && verification.verification_id.0 > cursor.verification_id.0)
}

fn resolve_task_verifications(verifications: Vec<TaskVerification>) -> Vec<TaskVerification> {
    let mut resolved_by_key = HashMap::<String, TaskVerification>::new();
    for verification in verifications {
        let resolution_key = task_verification_resolution_key(&verification.action);
        let should_replace = resolved_by_key
            .get(&resolution_key)
            .is_none_or(|current| task_verification_is_newer(&verification, current));
        if should_replace {
            resolved_by_key.insert(resolution_key, verification);
        }
    }
    let mut resolved = resolved_by_key.into_values().collect::<Vec<_>>();
    resolved.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
    });
    resolved
}

fn task_status_as_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Auto => "auto",
        TaskStatus::NeedsReview => "needs_review",
        TaskStatus::Verified => "verified",
        TaskStatus::RejectedMeta => "rejected_meta",
    }
}

fn confidence_as_str(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}

fn bool_to_i64(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use statsai_core::{EventId, ProjectInfo, SourceId, TaskVerdict, TASK_SPAN_SCHEMA_VERSION};

    fn test_span(
        title: &str,
        summary_preview: Option<&str>,
        branch_family: Option<&str>,
    ) -> SpanContext {
        test_span_with_title_source(title, summary_preview, branch_family, "test")
    }

    fn test_span_with_title_source(
        title: &str,
        summary_preview: Option<&str>,
        branch_family: Option<&str>,
        title_source: &str,
    ) -> SpanContext {
        SpanContext::from(TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: TaskSpanId("span_test".to_string()),
            provider: "codex".to_string(),
            source_id: SourceId("source_test".to_string()),
            span_kind: "codex_task".to_string(),
            source_record_id: None,
            source_file_path_hash: None,
            summary_id: None,
            session_id: Some("session".to_string()),
            thread_id: None,
            title: title.to_string(),
            normalized_title: normalize_task_title(title),
            title_source: Some(title_source.to_string()),
            summary_preview: summary_preview.map(ToOwned::to_owned),
            todo_excerpt: None,
            issue_keys: Vec::new(),
            branch_family: branch_family.map(ToOwned::to_owned),
            project_bucket: "bucket".to_string(),
            project: None,
            git: None,
            usage: UsageCounts::default(),
            estimated_cost_usd: None,
            estimated_cost_micro_usd: None,
            event_count: 0,
            has_usage_evidence: false,
            total_messages: 0,
            user_messages: 0,
            assistant_messages: 0,
            developer_messages: 0,
            linked_event_ids: Vec::new(),
            confidence: Confidence::Medium,
            is_meta: task_title_is_generic(Some(title)),
            started_at: Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap(),
            ended_at: Some(Utc.with_ymd_and_hms(2026, 6, 30, 12, 5, 0).unwrap()),
            duration_seconds: Some(300),
        })
    }

    fn test_span_with_options(
        span_id: &str,
        provider: &str,
        session_id: Option<&str>,
        project_bucket: &str,
        started_at: DateTime<Utc>,
        title: &str,
        summary_preview: Option<&str>,
    ) -> SpanContext {
        SpanContext::from(TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: TaskSpanId(span_id.to_string()),
            provider: provider.to_string(),
            source_id: SourceId(format!("source_{provider}")),
            span_kind: "task".to_string(),
            source_record_id: None,
            source_file_path_hash: None,
            summary_id: None,
            session_id: session_id.map(ToOwned::to_owned),
            thread_id: None,
            title: title.to_string(),
            normalized_title: normalize_task_title(title),
            title_source: Some("test".to_string()),
            summary_preview: summary_preview.map(ToOwned::to_owned),
            todo_excerpt: None,
            issue_keys: Vec::new(),
            branch_family: None,
            project_bucket: project_bucket.to_string(),
            project: None,
            git: None,
            usage: UsageCounts::default(),
            estimated_cost_usd: None,
            estimated_cost_micro_usd: None,
            event_count: 0,
            has_usage_evidence: false,
            total_messages: 0,
            user_messages: 0,
            assistant_messages: 0,
            developer_messages: 0,
            linked_event_ids: Vec::new(),
            confidence: Confidence::Medium,
            is_meta: task_title_is_generic(Some(title)),
            started_at,
            ended_at: Some(started_at + chrono::Duration::minutes(5)),
            duration_seconds: Some(300),
        })
    }

    fn test_work_item(
        work_item_id: &str,
        anchor_span_id: &str,
        status: TaskStatus,
        confidence: Confidence,
        total_tokens: u64,
        ended_at: DateTime<Utc>,
    ) -> WorkItem {
        WorkItem {
            schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
            work_item_id: WorkItemId(work_item_id.to_string()),
            anchor_span_id: TaskSpanId(anchor_span_id.to_string()),
            tail_span_id: TaskSpanId(anchor_span_id.to_string()),
            project_bucket: "bucket".to_string(),
            title: format!("Title {work_item_id}"),
            normalized_title: format!("title {work_item_id}"),
            status,
            confidence,
            started_at: ended_at - chrono::Duration::minutes(5),
            ended_at,
            duration_seconds: Some(300),
            span_count: 1,
            event_count: 1,
            total_input_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_output_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens,
            estimated_cost_usd: None,
            estimated_cost_micro_usd: None,
            providers: vec!["codex".to_string()],
            issue_keys: Vec::new(),
            repo_label: None,
            branch_labels: Vec::new(),
            path_label: None,
            summary_preview: None,
            todo_excerpt: None,
            no_git: true,
            cross_provider: false,
            continuation_reasons: Vec::new(),
            review_reasons: Vec::new(),
        }
    }

    fn test_git_project(branch_label: &str) -> ProjectInfo {
        ProjectInfo {
            project_id: "project-test".to_string(),
            project_label: Some("project-test".to_string()),
            repo_remote_hash: Some("repo-test".to_string()),
            repo_label: Some("owner/project-test".to_string()),
            branch_hash: Some(format!("branch-{branch_label}")),
            branch_label: Some(branch_label.to_string()),
            path_hash: Some("path-test".to_string()),
            path_label: Some("/tmp/project-test".to_string()),
        }
    }

    fn test_task_bucket_snapshot(
        project_bucket: &str,
        span_id: &str,
        title: &str,
        started_at: DateTime<Utc>,
    ) -> TaskBucketSnapshot {
        let span = test_span_with_options(
            span_id,
            "codex",
            Some("session-a"),
            project_bucket,
            started_at,
            title,
            Some(title),
        )
        .span;
        let spans = vec![span];
        let (work_items, members) = derive_task_work_items(spans.clone(), &[]);
        TaskBucketSnapshot {
            project_bucket: project_bucket.to_string(),
            generated_at: started_at + chrono::Duration::minutes(1),
            applied_verification_cursor: None,
            work_items,
            members,
            spans,
        }
    }

    #[test]
    fn derived_work_item_rounds_cost_after_aggregating_exact_micro_usd() {
        let started_at = Utc.with_ymd_and_hms(2026, 7, 25, 12, 0, 0).unwrap();
        let spans = (0..3)
            .map(|index| {
                let mut span = test_span_with_options(
                    &format!("span-cost-{index}"),
                    "codex",
                    Some("session-cost"),
                    "bucket-cost",
                    started_at + chrono::Duration::minutes(index),
                    "Implement exact task pricing",
                    Some("Implement exact task pricing"),
                )
                .span;
                span.estimated_cost_usd = Some(0);
                span.estimated_cost_micro_usd = Some(2_250);
                span.event_count = 1;
                span.has_usage_evidence = true;
                span
            })
            .collect::<Vec<_>>();

        let (work_items, _) = derive_task_work_items(spans, &[]);

        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].estimated_cost_micro_usd, Some(6_750));
        assert_eq!(work_items[0].estimated_cost_usd, Some(1));
    }

    #[test]
    fn chooses_branch_family_when_span_titles_are_only_generic() {
        let title = choose_work_item_title(&[test_span(
            "Review uncommitted changes",
            Some(">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e Wall time: 1.0006 seconds Process running with session ID 32988 Original token count: 30 Output:"),
            Some("add project token tracking"),
        )]);
        assert_eq!(title, "Add project token tracking");
    }

    #[test]
    fn replacing_task_bucket_snapshot_marks_existing_sync_state_dirty() {
        let store = Store::in_memory().expect("store");
        let initial_snapshot = test_task_bucket_snapshot(
            "bucket-a",
            "span-a",
            "Implement sync dirty tracking",
            Utc.with_ymd_and_hms(2026, 7, 6, 10, 0, 0).unwrap(),
        );
        store
            .replace_task_bucket_snapshot(&initial_snapshot)
            .expect("replace initial snapshot");
        store
            .record_task_bucket_snapshots_synced(
                "http",
                "https://example.invalid/api/sync/batches",
                "device-1",
                std::slice::from_ref(&initial_snapshot),
            )
            .expect("record synced snapshot");

        let clean_pending = store
            .pending_task_bucket_snapshots_for_sync(
                "http",
                "https://example.invalid/api/sync/batches",
                "device-1",
                false,
                None,
            )
            .expect("pending snapshots before replacement");
        assert!(clean_pending.is_empty());

        let updated_snapshot = test_task_bucket_snapshot(
            "bucket-a",
            "span-a",
            "Implement sync dirty tracking v2",
            Utc.with_ymd_and_hms(2026, 7, 6, 11, 0, 0).unwrap(),
        );
        store
            .replace_task_bucket_snapshot(&updated_snapshot)
            .expect("replace updated snapshot");

        let pending = store
            .pending_task_bucket_snapshots_for_sync(
                "http",
                "https://example.invalid/api/sync/batches",
                "device-1",
                false,
                None,
            )
            .expect("pending snapshots after replacement");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].project_bucket, "bucket-a");
        assert_eq!(pending[0].spans.len(), 1);
        assert_eq!(
            pending[0].spans[0].title,
            "Implement sync dirty tracking v2"
        );
    }

    #[test]
    fn falls_back_to_unresolved_when_no_signal_survives() {
        let title = choose_work_item_title(&[test_span(
            "<environment_context> <cwd>/Users/example/workspace/project</cwd>",
            Some(">>> TRANSCRIPT DELTA START [288] tool exec_command result: Chunk ID: 84e62e Wall time: 1.0006 seconds Process running with session ID 32988 Original token count: 30 Output:"),
            None,
        )]);
        assert_eq!(title, "Unresolved work item");
        assert!(task_title_is_generic(Some(title.as_str())));
    }

    #[test]
    fn prefers_cleaner_candidate_over_code_heavy_prompt_dump() {
        let title = choose_work_item_title(&[
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "I have interesting data from asr 1k 125 steps with eval.",
                Some("I have interesting data from asr 1k 125 steps with eval."),
                None,
            ),
        ]);
        assert_eq!(
            title,
            "I have interesting data from asr 1k 125 steps with eval"
        );
    }

    #[test]
    fn repeated_code_heavy_candidate_does_not_beat_cleaner_single_candidate() {
        let title = choose_work_item_title(&[
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1",
                Some("Okay, I could run qwen3_s100_val_prompt_completion_max16384.jsonl and %%bash set -e export PYTHONUNBUFFERED=1"),
                None,
            ),
            test_span(
                "I have interesting data from asr 1k 125 steps with eval.",
                Some("I have interesting data from asr 1k 125 steps with eval."),
                None,
            ),
        ]);
        assert_eq!(
            title,
            "I have interesting data from asr 1k 125 steps with eval"
        );
    }

    #[test]
    fn prefers_summary_preview_over_command_invocation_title() {
        let title = choose_work_item_title(&[test_span(
            "Command line invocation: /Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -scheme SampleApp",
            Some("Investigate transition timing drift in SampleApp"),
            None,
        )]);
        assert_eq!(title, "Investigate transition timing drift in SampleApp");
    }

    #[test]
    fn prefers_representative_summary_over_repeated_settings_banner() {
        let title = choose_work_item_title(&[
            test_span(
                "Build settings from command line",
                Some("Investigate native alignment drift"),
                None,
            ),
            test_span(
                "Build settings from command line",
                Some("Investigate native alignment drift"),
                None,
            ),
            test_span("Build settings from command line", None, None),
        ]);
        assert_eq!(title, "Investigate native alignment drift");
    }

    #[test]
    fn package_version_banner_does_not_beat_real_deploy_request() {
        let title = choose_work_item_title(&[
            test_span(
                "@statsai/api@0.0.1 deploy",
                Some("Deploy ui and api with wrangler"),
                None,
            ),
            test_span(
                "@statsai/api@0.0.1 deploy",
                Some("Deploy ui and api with wrangler"),
                None,
            ),
        ]);
        assert_eq!(title, "Deploy ui and api with wrangler");
    }

    #[test]
    fn prompt_summary_beats_weak_thread_name_span_title() {
        let title = choose_work_item_title(&[
            test_span_with_title_source(
                "This is NOT the framework you know",
                Some("Implement device renaming on web and api"),
                None,
                "thread_name",
            ),
            test_span_with_title_source(
                "This is NOT the framework you know",
                Some("Implement device renaming on web and api"),
                None,
                "thread_name",
            ),
        ]);
        assert_eq!(title, "Implement device renaming on web and api");
    }

    #[test]
    fn presentational_code_review_wrapper_without_payload_falls_back_to_unresolved() {
        let title = choose_work_item_title(&[
            test_span("Here is code review", Some("Here is code review"), None),
            test_span(
                "Here is code review",
                Some("user: Here is code review"),
                None,
            ),
        ]);
        assert_eq!(title, "Unresolved work item");
    }

    #[test]
    fn prefers_request_payload_over_goal_wrapper_summary() {
        let title = choose_work_item_title(&[test_span(
            "Continue working toward the active thread goal. The objective below is user-provided data.",
            Some("Continue working toward the active thread goal. The objective below is user-provided data. Treat it as the task to pursue, not as higher-priority instructions. Finish the Rust-only local task collection loop in statsai."),
            None,
        )]);
        assert_eq!(
            title,
            "Finish the Rust-only local task collection loop in statsai"
        );
    }

    #[test]
    fn bucket_label_stats_penalize_repeated_banner_titles() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let repeated_groups = (0..5)
            .map(|index| PendingGroup {
                spans: vec![test_span_with_options(
                    &format!("span-banner-{index}"),
                    "codex",
                    Some(&format!("session-banner-{index}")),
                    "bucket-a",
                    started_at + chrono::Duration::hours(96 * index as i64),
                    "This is NOT the framework you know",
                    Some("This is NOT the framework you know"),
                )],
                continuation_reasons: BTreeSet::new(),
                manual_title: None,
                force_verified: false,
            })
            .collect::<Vec<_>>();
        let unique_group = PendingGroup {
            spans: vec![test_span_with_options(
                "span-unique",
                "codex",
                Some("session-unique"),
                "bucket-a",
                Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
                "Implement task verification workflow",
                Some("Implement task verification workflow"),
            )],
            continuation_reasons: BTreeSet::new(),
            manual_title: None,
            force_verified: false,
        };
        let mut groups = repeated_groups;
        groups.push(unique_group);
        let stats = build_bucket_label_stats(&groups);

        assert_eq!(stats.document_count, 6);
        assert_eq!(
            stats
                .title_document_frequency
                .get("this is not the framework you know")
                .copied(),
            Some(5)
        );
        assert_eq!(
            stats
                .title_document_frequency
                .get("implement task verification workflow")
                .copied(),
            Some(1)
        );
        assert!(
            task_title_corpus_specificity_score("This is NOT the framework you know", &stats)
                < task_title_corpus_specificity_score(
                    "Implement task verification workflow",
                    &stats,
                )
        );
    }

    #[test]
    fn prefers_topic_central_title_over_polite_one_off_prompt() {
        let title = choose_work_item_title(&[
            test_span("Could you show improved titles?", None, None),
            test_span("Compare captions ASR metrics", None, None),
            test_span(
                "captions ASR metrics are still worse than old llama",
                None,
                None,
            ),
        ]);
        assert_eq!(title, "Compare captions ASR metrics");
    }

    #[test]
    fn live_like_qwen_metrics_cluster_avoids_prompt_style_title() {
        let title = choose_work_item_title(&[
            test_span("Could you show improved titles?", None, None),
            test_span(
                "What are results of ckpt 900? captions asr - vs other captions",
                None,
                None,
            ),
            test_span(
                "Maybe float16 instead of bfloat16 was the issue of new 4bit quants",
                None,
                None,
            ),
            test_span(
                "show me few ids from 900 repetitions, I'll check other rep pens",
                None,
                None,
            ),
        ]);
        assert_ne!(title, "show improved titles");
        assert!(
            title.contains("captions")
                || title.contains("ckpt")
                || title.contains("4bit")
                || title.contains("float16")
        );
    }

    #[test]
    fn interruption_only_cluster_falls_back_to_unresolved() {
        let interruption = "The user interrupted the previous turn on purpose. Any running unified exec processes may still be running in the background. If any tools/commands were aborted, they may have partially executed.";
        let title = choose_work_item_title(&[
            test_span(interruption, Some(interruption), None),
            test_span(interruption, Some(interruption), None),
        ]);
        assert_eq!(title, "Unresolved work item");
    }

    #[test]
    fn prefers_meaningful_candidate_over_tool_wrapper_title() {
        let title = choose_work_item_title(&[
            test_span(
                "I want to have ability to track tokens usage also by projects",
                Some("I want to have ability to track tokens usage also by projects"),
                None,
            ),
            test_span(
                "tool web_search call: {\"type\":\"open_page\",\"url\":\"https://base-ui.com/react/overview/releases/v1-4-0\"}",
                Some("tool web_search call: {\"type\":\"open_page\",\"url\":\"https://base-ui.com/react/overview/releases/v1-4-0\"}"),
                None,
            ),
        ]);
        assert_eq!(
            title,
            "I want to have ability to track tokens usage also by projects"
        );
    }

    #[test]
    fn prefers_real_title_over_abstract_followups_and_tool_metadata() {
        let title = choose_work_item_title(&[
            test_span(
                "Improve, replace, or remove existing work as needed to satisfy the actual objective",
                Some("Improve, replace, or remove existing work as needed to satisfy the actual objective"),
                None,
            ),
            test_span(
                "\"tool_title\": \"Get Test List\"",
                Some("\"tool_title\": \"Get Test List\""),
                None,
            ),
            test_span("Fix CLI device login", Some("Fix CLI device login"), None),
        ]);
        assert_eq!(title, "Fix CLI device login");
    }

    #[test]
    fn prefers_meaningful_candidate_over_single_cell_shell() {
        let title = choose_work_item_title(&[
            test_span("single cell, 8 only", Some("single cell, 8 only"), None),
            test_span(
                "I have interesting data from asr 1k 125 steps with eval",
                Some("I have interesting data from asr 1k 125 steps with eval"),
                None,
            ),
        ]);
        assert_eq!(
            title,
            "I have interesting data from asr 1k 125 steps with eval"
        );
    }

    #[test]
    fn prefers_meaningful_candidate_over_url_dump_title() {
        let title = choose_work_item_title(&[
            test_span(
                "notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9",
                Some("notebook https://proxy.example.invalid/session?token=eyJhbGciOiJIUzI1NiJ9"),
                None,
            ),
            test_span(
                "Explore chapter-llama finetuning attempts",
                Some("Explore chapter-llama finetuning attempts"),
                None,
            ),
        ]);
        assert_eq!(title, "Explore chapter-llama finetuning attempts");
    }

    #[test]
    fn prefers_meaningful_candidate_over_apply_patch_result_title() {
        let title = choose_work_item_title(&[
            test_span(
                "Success. Updated the following files: M /Users/example/workspace/project/ui/components/ui/sheet.tsx",
                Some("Success. Updated the following files: M /Users/example/workspace/project/ui/components/ui/sheet.tsx"),
                None,
            ),
            test_span(
                "Track tokens usage by project directory",
                Some("Track tokens usage by project directory"),
                None,
            ),
        ]);
        assert_eq!(title, "Track tokens usage by project directory");
    }

    #[test]
    fn prefers_real_intent_over_repeated_metric_result_labels() {
        let title = choose_work_item_title(&[
            test_span(
                "we had Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                Some(
                    "we had Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                ),
                None,
            ),
            test_span(
                "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
                Some("coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230"),
                None,
            ),
            test_span(
                "I want to choose the best adapters to average",
                Some("I want to choose the best adapters to average"),
                None,
            ),
        ]);
        assert_eq!(title, "I want to choose the best adapters to average");
    }

    #[test]
    fn progress_output_cluster_falls_back_to_unresolved() {
        let title = choose_work_item_title(&[
            test_span(
                "[DEBUG] ChapterLlamaBoundaryFinder: Wrote stage1 transcript to /tmp/stage1.txt",
                Some("[DEBUG] ChapterLlamaBoundaryFinder: Wrote stage1 transcript to /tmp/stage1.txt"),
                None,
            ),
            test_span(
                "Generating train split: 10 examples [00:00, 674.63 examples/s]",
                Some("Generating train split: 10 examples [00:00, 674.63 examples/s]"),
                None,
            ),
        ]);
        assert_eq!(title, "Unresolved work item");
    }

    #[test]
    fn metric_only_cluster_falls_back_to_unresolved() {
        let title = choose_work_item_title(&[
            test_span(
                "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                Some(
                    "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
                ),
                None,
            ),
            test_span(
                "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
                Some("coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230"),
                None,
            ),
        ]);
        assert_eq!(title, "Unresolved work item");
    }

    #[test]
    fn same_session_investigation_spans_stay_one_work_item() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at,
                "Investigate rollout failure in task collection",
                Some("Investigate rollout failure in task collection"),
            ),
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at + chrono::Duration::minutes(12),
                "Debug rollout failure in local task collection",
                Some("Debug rollout failure in local task collection"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 2);
        assert_eq!(work_items[0].span_count, 2);
    }

    #[test]
    fn two_span_same_session_topic_shift_splits_without_distribution_stats() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at,
                "Investigate SQLite migration failure in local task store",
                Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
            ),
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at + chrono::Duration::minutes(18),
                "Design benchmark export dashboard for task review",
                Some("Plan benchmark export dashboard metrics and review workflow"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 2);
        assert_eq!(members.len(), 2);
        assert_eq!(work_items[0].span_count, 1);
        assert_eq!(work_items[1].span_count, 1);
    }

    #[test]
    fn same_session_topic_shift_splits_on_cohesion_boundary() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at,
                "Investigate SQLite migration failure in local task store",
                Some("Analyze sqlite migration failure and schema upgrade rollback behavior"),
            ),
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at + chrono::Duration::minutes(18),
                "Design CLI task verification commands",
                Some("Plan accept reject split merge task verification commands"),
            ),
            test_span_with_options(
                "span-c",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at + chrono::Duration::minutes(31),
                "Implement CLI task verification commands",
                Some("Implement accept reject split merge task verification output"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 2);
        assert_eq!(members.len(), 3);
        assert_eq!(work_items[0].span_count, 1);
        assert_eq!(work_items[1].span_count, 2);
    }

    #[test]
    fn shared_issue_key_overrides_same_session_topic_boundary() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut span_a = test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Stabilize menubar wake handling",
            Some("Fix tray wake handling and sleep resume edge cases"),
        );
        span_a.span.issue_keys = vec!["OPS-42".to_string()];
        let mut span_b = test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(18),
            "Design benchmark JSON export gate",
            Some("Plan benchmark json export schema and gate metrics"),
        );
        span_b.span.issue_keys = vec!["OPS-42".to_string()];
        let mut span_c = test_span_with_options(
            "span-c",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(30),
            "Implement benchmark JSON export gate",
            Some("Implement benchmark json export schema and gate metrics"),
        );
        span_c.span.issue_keys = vec!["OPS-42".to_string()];

        let (work_items, members, _) = build_work_items(vec![span_a, span_b, span_c], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 3);
        assert_eq!(work_items[0].span_count, 3);
    }

    #[test]
    fn recurring_generic_review_shells_split_without_anchor() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at,
                "Review uncommitted changes",
                Some("Review uncommitted changes"),
            ),
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-b"),
                "bucket-a",
                started_at + chrono::Duration::hours(96),
                "Review uncommitted changes",
                Some("Review uncommitted changes"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 2);
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn same_title_in_different_project_buckets_never_merges() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "repo:a|path:a|branch:a",
                started_at,
                "Implement task verification workflow",
                Some("Implement task verification workflow"),
            ),
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-a"),
                "repo:b|path:b|branch:b",
                started_at + chrono::Duration::minutes(10),
                "Implement task verification workflow",
                Some("Implement task verification workflow"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 2);
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn no_git_path_only_workspace_still_produces_work_item() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "repo:none|path:abc|branch:none",
            started_at,
            "Implement local task collection",
            Some("Implement local task collection"),
        )];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(work_items[0].title, "Implement local task collection");
        assert!(work_items[0].no_git);
        assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
    }

    #[test]
    fn cross_provider_same_session_can_merge_but_stays_reviewable() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let contexts = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                "bucket-a",
                started_at,
                "Implement benchmark reporting",
                Some("Implement benchmark reporting"),
            ),
            test_span_with_options(
                "span-b",
                "opencode",
                Some("session-a"),
                "bucket-a",
                started_at + chrono::Duration::minutes(8),
                "Implement benchmark reporting",
                Some("Implement benchmark reporting"),
            ),
        ];

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 2);
        assert!(work_items[0].cross_provider);
        assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
    }

    #[test]
    fn git_anchored_work_item_with_event_evidence_stays_auto_high() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut context = test_span_with_options(
            "span-a",
            "claude_code",
            Some("session-a"),
            "bucket-a",
            started_at,
            "SwiftUI Paywall Sheet Race Condition Fix",
            Some("SwiftUI Paywall Sheet Race Condition Fix"),
        );
        context.span.project = Some(test_git_project("main"));
        context.span.linked_event_ids = vec![EventId("event-a".to_string())];
        context.span.usage = UsageCounts {
            input_tokens: Some(100),
            output_tokens: Some(20),
            ..UsageCounts::default()
        };

        let (work_items, members, _) = build_work_items(vec![context], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(work_items[0].status, TaskStatus::Auto);
        assert_eq!(work_items[0].confidence, Confidence::High);
        assert_eq!(work_items[0].event_count, 1);
        assert_eq!(work_items[0].total_tokens, 120);
        assert!(!work_items[0].no_git);
        assert!(work_items[0].review_reasons.is_empty());
    }

    #[test]
    fn repeated_banner_titles_with_real_usage_do_not_merge() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut contexts = Vec::new();
        for index in 0..5 {
            let timestamp = started_at + chrono::Duration::hours(96 * index as i64);
            let mut context = test_span_with_options(
                &format!("span-banner-{index}"),
                "codex",
                Some(&format!("session-banner-{index}")),
                "bucket-a",
                timestamp,
                "This is NOT the framework you know",
                Some("This is NOT the framework you know"),
            );
            context.span.project = Some(test_git_project("main"));
            context.span.linked_event_ids = vec![EventId(format!("event-banner-{index}"))];
            context.span.event_count = 1;
            context.span.has_usage_evidence = true;
            context.span.total_messages = 8;
            context.span.user_messages = 3;
            context.span.assistant_messages = 3;
            context.span.usage = UsageCounts {
                input_tokens: Some(100),
                output_tokens: Some(20),
                ..UsageCounts::default()
            };
            contexts.push(context);
        }

        let (work_items, members, _) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 5);
        assert_eq!(members.len(), 5);
        assert!(work_items.iter().all(|item| item.span_count == 1));
        assert!(work_items
            .iter()
            .all(|item| item.title == "This is NOT the framework you know"));
        assert!(work_items.iter().all(|item| item
            .review_reasons
            .contains(&"low_specificity_title".to_string())));
    }

    #[test]
    fn git_anchored_work_item_without_event_evidence_needs_review_low() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut context = test_span_with_options(
            "span-a",
            "claude_code",
            Some("session-a"),
            "bucket-a",
            started_at,
            "SwiftUI Paywall Sheet Race Condition Fix",
            Some("SwiftUI Paywall Sheet Race Condition Fix"),
        );
        context.span.project = Some(test_git_project("main"));

        let (work_items, members, _) = build_work_items(vec![context], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(work_items[0].status, TaskStatus::NeedsReview);
        assert_eq!(work_items[0].confidence, Confidence::Low);
        assert_eq!(work_items[0].event_count, 0);
        assert_eq!(work_items[0].total_tokens, 0);
        assert!(!work_items[0].no_git);
        assert!(work_items[0]
            .review_reasons
            .contains(&"no_usage_evidence".to_string()));
    }

    #[test]
    fn session_control_item_without_event_evidence_is_rejected_meta() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut context = test_span_with_options(
            "span-a",
            "claude_code",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Clearing Conversation History",
            Some("Clearing Conversation History"),
        );
        context.span.project = Some(test_git_project("main"));

        let (work_items, members, _) = build_work_items(vec![context], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
        assert_eq!(work_items[0].confidence, Confidence::Low);
        assert_eq!(work_items[0].title, "Clearing Conversation History");
        assert!(work_items[0]
            .review_reasons
            .contains(&"no_usage_evidence".to_string()));
    }

    #[test]
    fn low_volume_generic_exchange_is_rejected_meta() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut context = test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Morning Greetings",
            Some("Say hi, nothing else"),
        );
        context.span.linked_event_ids = vec![EventId("event-a".to_string())];
        context.span.event_count = 1;
        context.span.has_usage_evidence = true;
        context.span.total_messages = 2;
        context.span.user_messages = 1;
        context.span.assistant_messages = 1;

        let (work_items, members, _) = build_work_items(vec![context], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 1);
        assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
        assert_eq!(work_items[0].confidence, Confidence::Low);
        assert!(work_items[0]
            .review_reasons
            .contains(&"low_signal_exchange".to_string()));
    }

    #[test]
    fn repeated_low_volume_generic_shells_are_rejected_meta() {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut morning = test_span_with_options(
            "span-a",
            "codex",
            Some("quota-session"),
            "bucket-a",
            started_at,
            "Morning Greetings",
            Some("Say hi, nothing else"),
        );
        morning.span.linked_event_ids = vec![EventId("event-a".to_string())];
        morning.span.event_count = 1;
        morning.span.has_usage_evidence = true;
        morning.span.total_messages = 2;
        morning.span.user_messages = 1;
        morning.span.assistant_messages = 1;

        let mut lunch = test_span_with_options(
            "span-b",
            "codex",
            Some("quota-session"),
            "bucket-a",
            started_at + chrono::Duration::hours(4),
            "Lunch Greetings",
            Some("Say hi, nothing else"),
        );
        lunch.span.linked_event_ids = vec![EventId("event-b".to_string())];
        lunch.span.event_count = 1;
        lunch.span.has_usage_evidence = true;
        lunch.span.total_messages = 2;
        lunch.span.user_messages = 1;
        lunch.span.assistant_messages = 1;

        let mut evening = test_span_with_options(
            "span-c",
            "codex",
            Some("quota-session"),
            "bucket-a",
            started_at + chrono::Duration::hours(8),
            "Evening Greetings",
            Some("Say hi, nothing else"),
        );
        evening.span.linked_event_ids = vec![EventId("event-c".to_string())];
        evening.span.event_count = 1;
        evening.span.has_usage_evidence = true;
        evening.span.total_messages = 2;
        evening.span.user_messages = 1;
        evening.span.assistant_messages = 1;

        let (work_items, members, _) = build_work_items(vec![morning, lunch, evening], &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 3);
        assert_eq!(work_items[0].status, TaskStatus::RejectedMeta);
        assert_eq!(work_items[0].confidence, Confidence::Low);
        assert!(work_items[0]
            .review_reasons
            .contains(&"low_signal_exchange".to_string()));
    }

    #[test]
    fn work_items_are_ordered_for_review_queue() {
        let store = Store::in_memory().expect("store");
        let ended_base = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let work_items = vec![
            test_work_item(
                "verified-high",
                "span-verified",
                TaskStatus::Verified,
                Confidence::High,
                999,
                ended_base + chrono::Duration::hours(5),
            ),
            test_work_item(
                "auto-low",
                "span-auto",
                TaskStatus::Auto,
                Confidence::Low,
                9999,
                ended_base + chrono::Duration::hours(4),
            ),
            test_work_item(
                "needs-medium",
                "span-medium",
                TaskStatus::NeedsReview,
                Confidence::Medium,
                100,
                ended_base + chrono::Duration::hours(3),
            ),
            test_work_item(
                "needs-low-earlier",
                "span-low-earlier",
                TaskStatus::NeedsReview,
                Confidence::Low,
                500,
                ended_base + chrono::Duration::hours(1),
            ),
            test_work_item(
                "needs-low-later",
                "span-low-later",
                TaskStatus::NeedsReview,
                Confidence::Low,
                500,
                ended_base + chrono::Duration::hours(2),
            ),
        ];
        let members = work_items
            .iter()
            .map(|item| WorkItemMember {
                work_item_id: item.work_item_id.clone(),
                span_id: item.anchor_span_id.clone(),
                ordinal: 0,
            })
            .collect::<Vec<_>>();

        store
            .insert_work_items_in_tx(&work_items, &members)
            .expect("insert work items");

        let ordered = store.work_items().expect("ordered work items");
        let ids = ordered
            .iter()
            .map(|item| item.work_item_id.0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "needs-low-later",
                "needs-low-earlier",
                "needs-medium",
                "auto-low",
                "verified-high",
            ]
        );
    }

    #[test]
    fn resolve_task_verifications_keeps_latest_status_and_rename_per_anchor() {
        let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
        let anchor_span_id = TaskSpanId("span-anchor".to_string());
        let work_item_id = WorkItemId("work-anchor".to_string());
        let reject = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: task_verification_id("reject", "status:span-anchor"),
            action_key: "status:span-anchor".to_string(),
            action: TaskVerificationAction::Reject {
                work_item_id: work_item_id.clone(),
                anchor_span_id: anchor_span_id.clone(),
                reason: TaskVerdict::Meta,
            },
            created_at,
            updated_at: created_at,
        };
        let rename = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: task_verification_id("rename", "rename:span-anchor"),
            action_key: "rename:span-anchor".to_string(),
            action: TaskVerificationAction::Rename {
                work_item_id,
                anchor_span_id,
                title: "Verified renamed task".to_string(),
            },
            created_at,
            updated_at: created_at + chrono::Duration::minutes(5),
        };

        let resolved = resolve_task_verifications(vec![reject, rename]);
        assert_eq!(resolved.len(), 2);
        assert!(matches!(
            resolved[0].action,
            TaskVerificationAction::Reject { .. }
        ));
        assert!(matches!(
            resolved[1].action,
            TaskVerificationAction::Rename { .. }
        ));
    }

    #[test]
    fn merge_task_verification_canonicalizes_legacy_anchor_keys_before_insert() {
        let store = Store::in_memory().expect("store");
        let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
        let anchor_span_id = TaskSpanId("span-anchor".to_string());
        let work_item_id = WorkItemId("work-anchor".to_string());
        let legacy_rename = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: TaskVerificationId("legacy-rename".to_string()),
            action_key: "anchor:span-anchor".to_string(),
            action: TaskVerificationAction::Rename {
                work_item_id: work_item_id.clone(),
                anchor_span_id: anchor_span_id.clone(),
                title: "Legacy rename".to_string(),
            },
            created_at,
            updated_at: created_at,
        };
        let payload = serde_json::to_string(&legacy_rename).expect("legacy payload");
        store
            .conn
            .execute(
                r#"
                INSERT INTO task_verifications (
                  verification_id, action_kind, action_key, updated_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                rusqlite::params![
                    &legacy_rename.verification_id.0,
                    legacy_rename.action.action_kind(),
                    &legacy_rename.action_key,
                    legacy_rename.updated_at.to_rfc3339(),
                    &payload,
                ],
            )
            .expect("insert legacy rename");

        let legacy_reject = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: TaskVerificationId("legacy-reject".to_string()),
            action_key: "anchor:span-anchor".to_string(),
            action: TaskVerificationAction::Reject {
                work_item_id,
                anchor_span_id,
                reason: TaskVerdict::Meta,
            },
            created_at: created_at + chrono::Duration::minutes(1),
            updated_at: created_at + chrono::Duration::minutes(1),
        };

        assert!(store
            .merge_task_verification(&legacy_reject)
            .expect("merge legacy reject"));

        let stored = store.task_verifications().expect("task verifications");
        assert_eq!(stored.len(), 2);
        assert!(stored.iter().any(|verification| {
            matches!(verification.action, TaskVerificationAction::Rename { .. })
                && verification.action_key == "anchor:span-anchor"
        }));
        assert!(stored.iter().any(|verification| {
            matches!(verification.action, TaskVerificationAction::Reject { .. })
                && verification.action_key == "status:span-anchor"
        }));
    }

    #[test]
    fn manual_split_preservation_uses_explicit_right_boundary() {
        let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap();
        let span_a = test_span_with_options(
            "span-a",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at,
            "Implement task benchmark reporting",
            Some("Implement task benchmark reporting"),
        );
        let span_x = test_span_with_options(
            "span-x",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(1),
            "Implement task benchmark reporting",
            Some("Implement task benchmark reporting"),
        );
        let span_b = test_span_with_options(
            "span-b",
            "codex",
            Some("session-a"),
            "bucket-a",
            started_at + chrono::Duration::minutes(2),
            "Implement task benchmark reporting",
            Some("Implement task benchmark reporting"),
        );
        let predicted_assignments = HashMap::from([
            ("span-a".to_string(), "work-left".to_string()),
            ("span-x".to_string(), "work-right".to_string()),
            ("span-b".to_string(), "work-left".to_string()),
        ]);
        let verification = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: task_verification_id("split", "split:span-a:span-b"),
            action_key: "split:span-a:span-b".to_string(),
            action: TaskVerificationAction::Split {
                after_span_id: TaskSpanId("span-a".to_string()),
                before_span_id: Some(TaskSpanId("span-b".to_string())),
                left_title: None,
                right_title: None,
            },
            created_at: started_at,
            updated_at: started_at,
        };

        assert!(!manual_constraints_preserved(
            &predicted_assignments,
            &[span_a.span, span_x.span, span_b.span],
            &[verification],
        ));
    }

    #[test]
    fn localized_rebuild_deletes_layouts_reached_by_merged_ranges() {
        let store = Store::in_memory().expect("store");
        let started_at = Utc.with_ymd_and_hms(2026, 7, 2, 11, 0, 0).unwrap();
        let bucket = "bucket-a".to_string();
        let spans = vec![
            test_span_with_options(
                "span-a",
                "codex",
                Some("session-a"),
                &bucket,
                started_at,
                "Alpha payments cleanup",
                Some("Alpha payments cleanup"),
            )
            .span,
            test_span_with_options(
                "span-b",
                "codex",
                Some("session-b"),
                &bucket,
                started_at + chrono::Duration::minutes(10),
                "Vector search benchmark",
                Some("Vector search benchmark"),
            )
            .span,
            test_span_with_options(
                "span-c",
                "codex",
                Some("session-c"),
                &bucket,
                started_at + chrono::Duration::minutes(20),
                "Kernel tuning audit",
                Some("Kernel tuning audit"),
            )
            .span,
            test_span_with_options(
                "span-d",
                "codex",
                Some("session-d"),
                &bucket,
                started_at + chrono::Duration::minutes(30),
                "Latency regression report",
                Some("Latency regression report"),
            )
            .span,
            test_span_with_options(
                "span-e",
                "codex",
                Some("session-e"),
                &bucket,
                started_at + chrono::Duration::minutes(40),
                "Schema export polish",
                Some("Schema export polish"),
            )
            .span,
        ];
        store.upsert_task_spans(&spans).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild without merge");

        let initial = store.work_items().expect("initial work items");
        assert_eq!(initial.len(), 5);
        let left = initial
            .iter()
            .find(|item| item.anchor_span_id.0 == "span-a")
            .expect("left work item");
        let right = initial
            .iter()
            .find(|item| item.anchor_span_id.0 == "span-e")
            .expect("right work item");
        store
            .upsert_task_verification(TaskVerificationAction::Merge {
                left_work_item_id: left.work_item_id.clone(),
                right_work_item_id: right.work_item_id.clone(),
                left_anchor_span_id: TaskSpanId("span-a".to_string()),
                right_anchor_span_id: TaskSpanId("span-e".to_string()),
                title: Some("Merged endpoint work".to_string()),
            })
            .expect("merge verification");
        store
            .rebuild_all_task_work_items()
            .expect("rebuild merged layouts");

        let merged = store.work_items().expect("merged work items");
        assert_eq!(merged.len(), 4);

        let report = store
            .rebuild_task_work_items_for_changes_report(
                &BTreeSet::from([bucket.clone()]),
                &BTreeSet::from(["span-a".to_string()]),
                &[],
            )
            .expect("localized rebuild after endpoint merge");
        assert_eq!(report.work_items_deleted, 4);
        assert_eq!(report.work_items_rebuilt, 4);
        assert_eq!(report.touched_span_count, 5);

        let after = store
            .work_items()
            .expect("work items after localized rebuild");
        assert_eq!(after.len(), 4);
        let members = store.work_item_members_map().expect("member map");
        assert_eq!(members.len(), 5);
        assert_eq!(members.values().cloned().collect::<HashSet<_>>().len(), 4);
        assert!(members.contains_key("span-d"));
    }
}
