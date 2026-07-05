use super::*;
use statsai_core::{
    normalize_task_title, summarize_task_text, task_title_from_prompt, task_title_is_generic,
    task_title_is_session_meta, task_title_is_weak_signal, task_title_signal_score,
    task_verification_id, title_topic_tokens, work_item_id, Confidence, TaskSpan, TaskSpanId,
    TaskStatus, TaskVerification, TaskVerificationAction, UsageCounts, WorkItem, WorkItemId,
    WorkItemMember, TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

const TOPIC_COHESION_WINDOW_SPANS: usize = 2;
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
struct SpanContext {
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

#[derive(Debug, Default)]
struct PendingGroup {
    spans: Vec<SpanContext>,
    continuation_reasons: BTreeSet<String>,
    manual_title: Option<String>,
    force_verified: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BuildWorkItemsTimings {
    grouping_ms: u64,
    title_selection_ms: u64,
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

#[derive(Debug, Clone, Default)]
struct BucketLabelStats {
    document_count: usize,
    title_document_frequency: HashMap<String, usize>,
    token_document_frequency: HashMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum TitleCandidateSource {
    SpanTitle,
    SummaryPreview,
    TodoExcerpt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanTitleOrigin {
    UserPrompt,
    ThreadName,
    SessionTitle,
    SessionTitleWeak,
    SummaryDerived,
    TodoDerived,
    Default,
    Other,
}

#[derive(Debug, Clone)]
struct TitleCandidate {
    title: String,
    normalized: String,
    signal_score: i32,
    source: TitleCandidateSource,
    span_title_origin: Option<SpanTitleOrigin>,
    span_index: usize,
    topic_tokens: Vec<String>,
}

#[derive(Debug, Clone)]
struct ContinuationDecision {
    score: i32,
    reasons: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct BoundaryEvidence {
    local_similarity: f64,
    adjacent_overlap: usize,
    strong_topic_boundary: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct DistributionStats {
    mean: f64,
    std_dev: f64,
}

impl DistributionStats {
    fn from_values(values: &[f64]) -> Self {
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

    fn has_variation(self) -> bool {
        self.std_dev > f64::EPSILON
    }

    fn low_outlier_threshold(self) -> f64 {
        (self.mean - (self.std_dev * 0.5)).max(0.0)
    }

    fn high_outlier_threshold(self) -> f64 {
        self.mean + (self.std_dev * 0.5)
    }
}

impl Store {
    pub fn upsert_task_spans(&self, spans: &[TaskSpan]) -> Result<u64> {
        if spans.is_empty() {
            return Ok(0);
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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
        })();

        match result {
            Ok(changed) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(changed)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn delete_task_spans_for_sources(
        &self,
        source_ids: &[SourceId],
    ) -> Result<TaskDeletionImpact> {
        if source_ids.is_empty() {
            return Ok(TaskDeletionImpact::default());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let targets = self.task_span_targets_for_sources(source_ids)?;
            self.delete_task_span_targets_in_tx(&targets)
        })();

        match result {
            Ok(impact) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(impact)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn delete_task_spans_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<TaskDeletionImpact> {
        if file_hashes.is_empty() {
            return Ok(TaskDeletionImpact::default());
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            let targets = self.task_span_targets_for_source_file_hashes(source_id, file_hashes)?;
            self.delete_task_span_targets_in_tx(&targets)
        })();

        match result {
            Ok(impact) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(impact)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
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
        let action_keys = anchor_task_verification_action_keys(&action)
            .unwrap_or_else(|| vec![action_key.clone()]);
        let existing = self.latest_task_verification_by_action_keys(&action_keys)?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.delete_task_verifications_by_action_keys(&action_keys)?;
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
                self.conn.execute_batch("COMMIT")?;
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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
            Ok(report)
        })();

        match result {
            Ok(report) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(report)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    fn task_spans_by_sql(
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
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
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
            Ok(report)
        })();

        match result {
            Ok(report) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(report)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
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

    fn latest_task_verification_by_action_keys(
        &self,
        action_keys: &[String],
    ) -> Result<Option<TaskVerification>> {
        Ok(self
            .task_verifications_by_action_keys(action_keys)?
            .into_iter()
            .max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
            }))
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

    fn delete_task_verifications_by_action_keys(&self, action_keys: &[String]) -> Result<()> {
        let mut statement = self
            .conn
            .prepare("DELETE FROM task_verifications WHERE action_key = ?1")?;
        for action_key in action_keys {
            statement.execute(params![action_key])?;
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

fn build_work_items(
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

fn build_bucket_label_stats(groups: &[PendingGroup]) -> BucketLabelStats {
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

fn group_spans(
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

fn continuation_decision(
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

fn compute_boundary_evidence(contexts: &[SpanContext]) -> Vec<BoundaryEvidence> {
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

fn boundary_window_similarity(topic_sets: &[BTreeSet<String>], boundary_index: usize) -> f64 {
    let left_start = boundary_index.saturating_sub(TOPIC_COHESION_WINDOW_SPANS - 1);
    let left_counts = aggregate_topic_counts(&topic_sets[left_start..=boundary_index]);
    let right_end = (boundary_index + TOPIC_COHESION_WINDOW_SPANS).min(topic_sets.len() - 1);
    let right_counts = aggregate_topic_counts(&topic_sets[boundary_index + 1..=right_end]);
    weighted_jaccard_similarity(&left_counts, &right_counts)
}

fn boundary_depth_scores(similarities: &[f64]) -> Vec<f64> {
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

fn aggregate_topic_counts(topic_sets: &[BTreeSet<String>]) -> HashMap<String, usize> {
    let mut counts = HashMap::<String, usize>::new();
    for topic_set in topic_sets {
        for token in topic_set {
            *counts.entry(token.clone()).or_default() += 1;
        }
    }
    counts
}

fn weighted_jaccard_similarity(
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

fn spans_share_issue_keys(left: &SpanContext, right: &SpanContext) -> bool {
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

fn spans_share_non_generic_title(left: &SpanContext, right: &SpanContext) -> bool {
    !left.span.normalized_title.is_empty()
        && left.span.normalized_title == right.span.normalized_title
        && !left.title_is_generic()
}

fn spans_have_contentful_topic_signal(left: &SpanContext, right: &SpanContext) -> bool {
    span_has_contentful_topic_signal(left) && span_has_contentful_topic_signal(right)
}

fn span_has_contentful_topic_signal(span: &SpanContext) -> bool {
    let title_signal = span.title_signal_score();
    let topic_token_count = span.topic_tokens().len();
    title_signal > 0 && topic_token_count >= 3
}

fn build_work_item(
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
    let mut estimated_cost_usd: Option<i64> = None;
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
        estimated_cost_usd = match (estimated_cost_usd, context.estimated_cost_usd()) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (Some(left), None) => Some(left),
            (None, right) => right,
        };
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
        estimated_cost_usd,
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

fn span_is_session_control_meta(span: &TaskSpan) -> bool {
    [
        Some(span.title.as_str()),
        span.summary_preview.as_deref(),
        span.todo_excerpt.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| task_title_is_session_meta(Some(value)))
}

fn apply_split_verifications(
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

fn apply_merge_verifications(
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

#[derive(Debug, Clone)]
struct GroundTruthData {
    cluster_by_span: HashMap<String, String>,
    rejected_span_ids: HashSet<String>,
    adjacent_truth: Vec<(String, String, bool)>,
    verified_span_ids: HashSet<String>,
    verified_adjacent_pairs: u64,
}

#[derive(Debug, Clone)]
enum BenchmarkStrategy {
    GapHours(i64),
    RepoTitle,
    RepoBranchTitle,
}

impl BenchmarkStrategy {
    fn name(&self) -> String {
        match self {
            Self::GapHours(hours) => format!("gap_only_{}h", hours),
            Self::RepoTitle => "repo_plus_title".to_string(),
            Self::RepoBranchTitle => "repo_plus_branch_plus_title".to_string(),
        }
    }
}

fn ground_truth_from_store(
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

fn next_span_id_in_bucket(
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

fn split_right_span_id(
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

fn evaluate_prediction(
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

fn pairwise_cluster_counts(
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

fn anchor_task_verification_action_keys(action: &TaskVerificationAction) -> Option<Vec<String>> {
    let anchor_span_id = action.anchor_span_id()?;
    Some(vec![
        format!("anchor:{}", anchor_span_id.0),
        format!("accept:{}", anchor_span_id.0),
        format!("reject:{}", anchor_span_id.0),
        format!("rename:{}", anchor_span_id.0),
    ])
}

fn work_item_members_map_from_members(members: &[WorkItemMember]) -> HashMap<String, String> {
    let mut assignments = HashMap::new();
    for member in members {
        assignments.insert(member.span_id.0.clone(), member.work_item_id.0.clone());
    }
    assignments
}

fn resolve_task_verifications(verifications: Vec<TaskVerification>) -> Vec<TaskVerification> {
    let mut anchor_verifications = HashMap::<String, TaskVerification>::new();
    let mut non_anchor_verifications = Vec::<TaskVerification>::new();
    for verification in verifications {
        if let Some(anchor_span_id) = verification.action.anchor_span_id() {
            anchor_verifications.insert(anchor_span_id.0.clone(), verification);
        } else {
            non_anchor_verifications.push(verification);
        }
    }
    non_anchor_verifications.extend(anchor_verifications.into_values());
    non_anchor_verifications.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
    });
    non_anchor_verifications
}

fn meta_counts(
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

fn build_baseline_assignments(
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

fn rejected_span_ids_from_work_items(
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

fn manual_constraints_preserved(
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

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn f1(true_positive: u64, predicted_positive: u64, truth_positive: u64) -> f64 {
    let precision = ratio(true_positive, predicted_positive);
    let recall = ratio(true_positive, truth_positive);
    if precision == 0.0 && recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

#[cfg(test)]
fn choose_work_item_title(spans: &[SpanContext]) -> String {
    choose_work_item_title_with_stats(spans, &BucketLabelStats::default())
}

fn choose_work_item_title_with_stats(
    spans: &[SpanContext],
    bucket_label_stats: &BucketLabelStats,
) -> String {
    let primary_candidates = collect_primary_title_candidates(spans);
    if primary_title_candidates_are_sufficient(&primary_candidates) {
        if let Some(title) = best_title_from_candidates(&primary_candidates, bucket_label_stats) {
            return title;
        }
    }
    let ordered_candidates = collect_title_candidates(spans);
    if let Some(title) = best_title_from_candidates(&ordered_candidates, bucket_label_stats) {
        return title;
    }
    for context in spans {
        let Some(branch_family) = context.span.branch_family.as_deref() else {
            continue;
        };
        let Some(title) = humanize_branch_family(branch_family) else {
            continue;
        };
        if !task_title_is_generic(Some(title.as_str()))
            && !task_title_is_weak_signal(Some(title.as_str()))
        {
            return title;
        }
    }
    if let Some(title) = choose_relaxed_work_item_title_with_stats(spans, bucket_label_stats) {
        return title;
    }
    "Unresolved work item".to_string()
}

fn primary_title_candidates_are_sufficient(candidates: &[TitleCandidate]) -> bool {
    candidates.iter().any(|candidate| {
        matches!(
            candidate.span_title_origin,
            Some(
                SpanTitleOrigin::UserPrompt
                    | SpanTitleOrigin::SummaryDerived
                    | SpanTitleOrigin::TodoDerived
            )
        ) && candidate.signal_score > 0
    })
}

fn best_title_from_candidates(
    ordered_candidates: &[TitleCandidate],
    bucket_label_stats: &BucketLabelStats,
) -> Option<String> {
    let mut best_title = None::<String>;
    let mut best_score = i32::MIN;
    let mut frequencies = HashMap::<String, usize>::new();
    let mut topic_frequencies = HashMap::<String, usize>::new();
    let mut source_support = HashMap::<String, BTreeSet<TitleCandidateSource>>::new();

    for candidate in ordered_candidates {
        *frequencies.entry(candidate.normalized.clone()).or_default() += 1;
        for token in &candidate.topic_tokens {
            *topic_frequencies.entry(token.clone()).or_default() += 1;
        }
        source_support
            .entry(candidate.normalized.clone())
            .or_default()
            .insert(candidate.source);
    }

    for candidate in ordered_candidates {
        let frequency = frequencies.get(&candidate.normalized).copied().unwrap_or(1);
        let topic_overlap = candidate
            .topic_tokens
            .iter()
            .map(|token| {
                topic_frequencies
                    .get(token)
                    .copied()
                    .unwrap_or_default()
                    .saturating_sub(1)
            })
            .sum::<usize>();
        let score = title_candidate_score(
            candidate,
            frequency,
            topic_overlap,
            source_support
                .get(&candidate.normalized)
                .map_or(1, BTreeSet::len),
            ordered_candidates,
            &frequencies,
            bucket_label_stats,
        );
        if score > best_score {
            best_score = score;
            best_title = Some(candidate.title.clone());
        }
    }

    if let Some(title) = best_title {
        return Some(title);
    }
    None
}

fn title_candidate_score(
    candidate: &TitleCandidate,
    frequency: usize,
    topic_overlap: usize,
    source_support_count: usize,
    ordered_candidates: &[TitleCandidate],
    frequencies: &HashMap<String, usize>,
    bucket_label_stats: &BucketLabelStats,
) -> i32 {
    let title = candidate.title.as_str();
    let token_count = candidate.normalized.split_whitespace().count();
    let length = title.chars().count();
    let lowercase = title.to_ascii_lowercase();
    let digit_count = title
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    let alpha_count = title
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let opaque_token_count = title
        .split_whitespace()
        .filter(|token| looks_like_opaque_candidate_token(token))
        .count();

    let mut score = 0;
    let mut code_penalties = 0;
    score += match token_count {
        3..=9 => 10,
        2..=12 => 7,
        13..=18 => 2,
        0..=1 => -8,
        _ => -4,
    };
    score += match length {
        18..=72 => 6,
        10..=96 => 2,
        97..=140 => -2,
        _ => -6,
    };
    if title
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '-' | '=' | '`' | '"' | '['))
    {
        score -= 4;
    }
    if title
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        score -= 3;
    }
    if digit_count > alpha_count && digit_count >= 4 {
        score -= 4;
    }
    if lowercase.ends_with('?') {
        score -= 2;
    }
    score -= (opaque_token_count.min(3) as i32) * 4;

    for fragment in [
        "%%bash",
        "pip install",
        "mkdir -p",
        "export ",
        "/users/",
        "/kaggle/",
        ".jsonl",
        ".csv",
        ".ipynb",
        "http://",
        "https://",
        " | ",
        " = ",
        "==",
        "```",
        "<turn|>",
        "automation:",
        "tool web_search",
        "tool apply_patch",
        "token=eyj",
        "jupyter-proxy.kaggle.net",
    ] {
        if lowercase.contains(fragment) {
            code_penalties += 1;
        }
    }

    score -= code_penalties * 5;
    score += candidate.signal_score * 2;
    score += task_title_corpus_specificity_score(title, bucket_label_stats) * 2;
    score += task_title_corpus_phraseness_score(candidate, bucket_label_stats);
    score -= title_candidate_completeness_penalty(candidate, ordered_candidates, frequencies);
    score += title_candidate_source_bonus(candidate, source_support_count);
    score += title_candidate_context_score(candidate, topic_overlap);
    score += title_candidate_position_bonus(candidate);
    if code_penalties == 0 && (source_support_count > 1 || topic_overlap > 0) {
        score += (frequency.saturating_sub(1).min(2) as i32) * 2;
    }
    score += (topic_overlap.min(12) as i32) * 2;
    if frequency == 1 && topic_overlap == 0 {
        score -= 4;
    }
    if matches!(candidate.source, TitleCandidateSource::SpanTitle)
        && source_support_count == 1
        && topic_overlap == 0
    {
        score -= 8;
    }
    if matches!(candidate.source, TitleCandidateSource::SpanTitle)
        && source_support_count == 1
        && task_title_corpus_specificity_score(title, bucket_label_stats) <= 0
    {
        score -= 6;
    }
    if matches!(
        candidate.span_title_origin,
        Some(SpanTitleOrigin::ThreadName | SpanTitleOrigin::SessionTitleWeak)
    ) && source_support_count == 1
        && topic_overlap == 0
    {
        score -= 10;
    }
    if matches!(candidate.span_title_origin, Some(SpanTitleOrigin::Default)) {
        score -= 10;
    }

    score
}

fn smoothed_inverse_document_frequency(document_count: usize, document_frequency: usize) -> f64 {
    if document_count == 0 {
        return 0.0;
    }
    ((document_count as f64 + 1.0) / (document_frequency as f64 + 1.0)).ln()
}

fn task_title_corpus_specificity_score(title: &str, bucket_label_stats: &BucketLabelStats) -> i32 {
    if bucket_label_stats.document_count <= 1 {
        return 0;
    }
    let normalized = normalize_task_title(title);
    if normalized.is_empty() {
        return -6;
    }
    let title_document_frequency = bucket_label_stats
        .title_document_frequency
        .get(&normalized)
        .copied()
        .unwrap_or(1);
    let title_ratio = title_document_frequency as f64 / bucket_label_stats.document_count as f64;
    let title_idf = smoothed_inverse_document_frequency(
        bucket_label_stats.document_count,
        title_document_frequency,
    );
    let topic_tokens = title_topic_tokens(title);
    let average_token_idf = if topic_tokens.is_empty() {
        0.0
    } else {
        topic_tokens
            .iter()
            .map(|token| {
                let token_document_frequency = bucket_label_stats
                    .token_document_frequency
                    .get(token)
                    .copied()
                    .unwrap_or(1);
                smoothed_inverse_document_frequency(
                    bucket_label_stats.document_count,
                    token_document_frequency,
                )
            })
            .sum::<f64>()
            / topic_tokens.len() as f64
    };
    let content_bonus = (topic_tokens.len().min(6) as f64) * 0.4;

    ((average_token_idf * 4.0) + (title_idf * 2.5) + content_bonus - (title_ratio * 14.0)).round()
        as i32
}

fn task_title_corpus_phraseness_score(
    candidate: &TitleCandidate,
    bucket_label_stats: &BucketLabelStats,
) -> i32 {
    if bucket_label_stats.document_count <= 1 || candidate.topic_tokens.len() < 2 {
        return 0;
    }
    let title_document_frequency = bucket_label_stats
        .title_document_frequency
        .get(&candidate.normalized)
        .copied()
        .unwrap_or(1);
    let phrase_probability =
        title_document_frequency as f64 / bucket_label_stats.document_count as f64;
    let independent_probability = candidate
        .topic_tokens
        .iter()
        .map(|token| {
            bucket_label_stats
                .token_document_frequency
                .get(token)
                .copied()
                .unwrap_or(1) as f64
                / bucket_label_stats.document_count as f64
        })
        .product::<f64>();
    if phrase_probability <= 0.0 || independent_probability <= 0.0 {
        return 0;
    }
    ((phrase_probability / independent_probability).ln() * 2.0)
        .clamp(-6.0, 8.0)
        .round() as i32
}

fn title_candidate_source_bonus(candidate: &TitleCandidate, source_support_count: usize) -> i32 {
    let source_bonus = match candidate.source {
        TitleCandidateSource::SpanTitle => match candidate.span_title_origin {
            Some(SpanTitleOrigin::UserPrompt) => 6,
            Some(SpanTitleOrigin::SummaryDerived) => 5,
            Some(SpanTitleOrigin::TodoDerived) => 6,
            Some(SpanTitleOrigin::ThreadName) => -4,
            Some(SpanTitleOrigin::SessionTitle) => -2,
            Some(SpanTitleOrigin::SessionTitleWeak) => -5,
            Some(SpanTitleOrigin::Default) => -8,
            Some(SpanTitleOrigin::Other) | None => 0,
        },
        TitleCandidateSource::SummaryPreview => 5,
        TitleCandidateSource::TodoExcerpt => 6,
    };
    source_bonus + ((source_support_count.saturating_sub(1).min(2) as i32) * 2)
}

fn title_candidate_position_bonus(candidate: &TitleCandidate) -> i32 {
    match candidate.span_index {
        0 => 2,
        1 => 1,
        _ => 0,
    }
}

fn title_candidate_context_score(candidate: &TitleCandidate, topic_overlap: usize) -> i32 {
    let topic_token_count = candidate.topic_tokens.len();
    if topic_token_count == 0 {
        return -6;
    }
    let average_overlap = topic_overlap as f64 / topic_token_count as f64;
    match average_overlap {
        overlap if overlap >= 3.0 => 6,
        overlap if overlap >= 1.5 => 3,
        0.0 => -2,
        _ => 0,
    }
}

fn title_candidate_completeness_penalty(
    candidate: &TitleCandidate,
    ordered_candidates: &[TitleCandidate],
    frequencies: &HashMap<String, usize>,
) -> i32 {
    let candidate_frequency = frequencies.get(&candidate.normalized).copied().unwrap_or(1);
    if candidate_frequency == 0 {
        return 0;
    }
    let candidate_token_set = candidate
        .topic_tokens
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut max_conditional_support = 0.0f64;
    for other in ordered_candidates {
        if other.normalized == candidate.normalized || other.title.len() <= candidate.title.len() {
            continue;
        }
        let subsumes = other.normalized.contains(&candidate.normalized)
            || (!candidate_token_set.is_empty()
                && candidate_token_set.len() >= 2
                && candidate_token_set.iter().all(|token| {
                    other
                        .topic_tokens
                        .iter()
                        .any(|other_token| other_token.as_str() == *token)
                }));
        if !subsumes {
            continue;
        }
        let other_frequency = frequencies.get(&other.normalized).copied().unwrap_or(1);
        let conditional_support = other_frequency as f64 / candidate_frequency as f64;
        if conditional_support > max_conditional_support {
            max_conditional_support = conditional_support;
        }
    }
    if max_conditional_support >= 0.75 {
        8
    } else if max_conditional_support >= 0.5 {
        4
    } else {
        0
    }
}

fn collect_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
        if !span_title_needs_fallback_candidates(context) {
            continue;
        }
        push_title_candidate(
            &mut candidates,
            context.span.summary_preview.as_deref(),
            TitleCandidateSource::SummaryPreview,
            None,
            span_index,
        );
        push_title_candidate(
            &mut candidates,
            context.span.todo_excerpt.as_deref(),
            TitleCandidateSource::TodoExcerpt,
            None,
            span_index,
        );
    }
    candidates
}

fn collect_primary_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
    }
    candidates
}

fn push_title_candidate(
    candidates: &mut Vec<TitleCandidate>,
    raw: Option<&str>,
    source: TitleCandidateSource,
    title_source: Option<&str>,
    span_index: usize,
) {
    let Some(title) = materialize_title_candidate(raw, source) else {
        return;
    };
    if task_title_is_generic(Some(title.as_str()))
        || task_title_is_weak_signal(Some(title.as_str()))
    {
        return;
    }
    let normalized = normalize_task_title(&title);
    if normalized.is_empty() {
        return;
    }
    let signal_score = task_title_signal_score(Some(title.as_str()));
    let topic_tokens = title_topic_tokens(&title).into_iter().collect::<Vec<_>>();
    candidates.push(TitleCandidate {
        title,
        normalized,
        signal_score,
        source,
        span_title_origin: span_title_origin(source, title_source),
        span_index,
        topic_tokens,
    });
}

fn choose_relaxed_work_item_title_with_stats(
    spans: &[SpanContext],
    bucket_label_stats: &BucketLabelStats,
) -> Option<String> {
    let ordered_candidates = collect_relaxed_title_candidates(spans);
    let best_title = best_title_from_candidates(&ordered_candidates, bucket_label_stats)?;
    (task_title_signal_score(Some(best_title.as_str())) > -6).then_some(best_title)
}

fn collect_relaxed_title_candidates(spans: &[SpanContext]) -> Vec<TitleCandidate> {
    let mut candidates = Vec::<TitleCandidate>::new();
    for (span_index, context) in spans.iter().enumerate() {
        push_relaxed_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref(),
            span_index,
        );
        if !span_title_needs_fallback_candidates(context) {
            continue;
        }
        push_relaxed_title_candidate(
            &mut candidates,
            context.span.summary_preview.as_deref(),
            TitleCandidateSource::SummaryPreview,
            None,
            span_index,
        );
        push_relaxed_title_candidate(
            &mut candidates,
            context.span.todo_excerpt.as_deref(),
            TitleCandidateSource::TodoExcerpt,
            None,
            span_index,
        );
    }
    candidates
}

fn push_relaxed_title_candidate(
    candidates: &mut Vec<TitleCandidate>,
    raw: Option<&str>,
    source: TitleCandidateSource,
    title_source: Option<&str>,
    span_index: usize,
) {
    if materialize_title_candidate(raw, source).is_some() {
        return;
    }
    let Some(title) = raw.and_then(|value| summarize_task_text(Some(value), 90)) else {
        return;
    };
    if task_title_is_session_meta(Some(title.as_str())) {
        return;
    }
    if !relaxed_candidate_looks_contentful(title.as_str()) {
        return;
    }
    let normalized = normalize_task_title(&title);
    if normalized.is_empty() {
        return;
    }
    let topic_tokens = title_topic_tokens(&title).into_iter().collect::<Vec<_>>();
    if topic_tokens.is_empty() && task_title_signal_score(Some(title.as_str())) < 0 {
        return;
    }
    let signal_score = task_title_signal_score(Some(title.as_str()));
    candidates.push(TitleCandidate {
        title,
        normalized,
        signal_score,
        source,
        span_title_origin: span_title_origin(source, title_source),
        span_index,
        topic_tokens,
    });
}

fn relaxed_candidate_looks_contentful(title: &str) -> bool {
    if task_title_is_generic(Some(title)) || task_title_is_weak_signal(Some(title)) {
        return false;
    }
    let signal_score = task_title_signal_score(Some(title));
    if signal_score <= 0 {
        return false;
    }
    let alpha_count = title
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let digit_count = title
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    let topic_token_count = title_topic_tokens(title).len();
    alpha_count > digit_count && (2..=8).contains(&topic_token_count)
}

fn span_title_needs_fallback_candidates(context: &SpanContext) -> bool {
    if context.title_is_generic()
        || context.title_is_weak_signal()
        || context.title_signal_score() < 8
    {
        return true;
    }
    !matches!(
        span_title_origin(
            TitleCandidateSource::SpanTitle,
            context.span.title_source.as_deref()
        ),
        Some(
            SpanTitleOrigin::UserPrompt
                | SpanTitleOrigin::SummaryDerived
                | SpanTitleOrigin::TodoDerived
        )
    )
}

fn materialize_title_candidate(raw: Option<&str>, source: TitleCandidateSource) -> Option<String> {
    match source {
        TitleCandidateSource::SpanTitle => raw
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        TitleCandidateSource::SummaryPreview | TitleCandidateSource::TodoExcerpt => {
            task_title_from_prompt(raw)
        }
    }
}

fn span_title_origin(
    source: TitleCandidateSource,
    title_source: Option<&str>,
) -> Option<SpanTitleOrigin> {
    if !matches!(source, TitleCandidateSource::SpanTitle) {
        return None;
    }
    Some(match title_source.unwrap_or_default() {
        "user_prompt" => SpanTitleOrigin::UserPrompt,
        "thread_name" => SpanTitleOrigin::ThreadName,
        "session_title" => SpanTitleOrigin::SessionTitle,
        "session_title_weak" => SpanTitleOrigin::SessionTitleWeak,
        "summary" | "summary_diffs" | "generated_title" | "session_summary" => {
            SpanTitleOrigin::SummaryDerived
        }
        "todo_excerpt" => SpanTitleOrigin::TodoDerived,
        "default" => SpanTitleOrigin::Default,
        _ => SpanTitleOrigin::Other,
    })
}

fn looks_like_opaque_candidate_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '[' | ']'
        )
    });
    if trimmed.len() < 8 {
        return false;
    }
    let has_upper = trimmed
        .chars()
        .any(|character| character.is_ascii_uppercase());
    let has_lower = trimmed
        .chars()
        .any(|character| character.is_ascii_lowercase());
    let has_digit = trimmed.chars().any(|character| character.is_ascii_digit());
    let safe_chars = trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    safe_chars && has_digit && (has_upper || has_lower)
}

fn humanize_branch_family(value: &str) -> Option<String> {
    let normalized = normalize_task_title(value);
    if normalized.is_empty() || looks_like_issue_key_family(value) {
        return None;
    }
    let mut characters = normalized.chars();
    let first = characters.next()?.to_ascii_uppercase();
    Some(format!("{first}{}", characters.collect::<String>()))
}

fn looks_like_issue_key_family(value: &str) -> bool {
    let Some((left, right)) = value.trim().split_once('-') else {
        return false;
    };
    !left.is_empty()
        && !right.is_empty()
        && left
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && right.chars().all(|character| character.is_ascii_digit())
}

fn sum_usage_counts(left: &UsageCounts, right: &UsageCounts) -> UsageCounts {
    fn sum_field(left: Option<u64>, right: Option<u64>) -> Option<u64> {
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
    fn resolve_task_verifications_prefers_latest_anchor_verification() {
        let created_at = Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap();
        let anchor_span_id = TaskSpanId("span-anchor".to_string());
        let work_item_id = WorkItemId("work-anchor".to_string());
        let reject = TaskVerification {
            schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
            verification_id: task_verification_id("reject", "reject:span-anchor"),
            action_key: "reject:span-anchor".to_string(),
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
        assert_eq!(resolved.len(), 1);
        assert!(matches!(
            resolved[0].action,
            TaskVerificationAction::Rename { .. }
        ));
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
