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
mod tests;
