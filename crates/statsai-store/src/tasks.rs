use super::*;
use statsai_core::{
    normalize_task_title, task_title_from_prompt, task_title_is_generic,
    task_title_is_session_meta, task_title_is_weak_signal, task_title_signal_score,
    task_verification_id, title_topic_tokens, work_item_id, Confidence, TaskSpan, TaskSpanId,
    TaskStatus, TaskVerification, TaskVerificationAction, UsageCounts, UsageEvent, WorkItem,
    WorkItemId, WorkItemMember, TASK_VERIFICATION_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
    let contexts = spans
        .into_iter()
        .map(|span| SpanContext {
            span,
            linked_events: Vec::new(),
        })
        .collect::<Vec<_>>();
    build_work_items(contexts, verifications)
}

#[derive(Debug, Clone)]
struct SpanContext {
    span: TaskSpan,
    linked_events: Vec<UsageEvent>,
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

    fn topic_tokens(&self) -> BTreeSet<String> {
        let mut tokens = title_topic_tokens(&self.span.title);
        if let Some(summary_preview) = self.span.summary_preview.as_deref() {
            tokens.extend(title_topic_tokens(summary_preview));
        }
        tokens
    }

    fn usage(&self) -> UsageCounts {
        if self.linked_events.is_empty() {
            return self.span.usage.clone();
        }
        self.linked_events
            .iter()
            .fold(UsageCounts::default(), |usage, event| {
                sum_usage_counts(&usage, &event.usage)
            })
    }

    fn estimated_cost_usd(&self) -> Option<i64> {
        if self.linked_events.is_empty() {
            return self.span.estimated_cost_usd;
        }
        self.linked_events
            .iter()
            .filter_map(|event| event.cost.estimated_api_equivalent_usd)
            .reduce(i64::saturating_add)
            .or(self.span.estimated_cost_usd)
    }

    fn total_messages(&self) -> u64 {
        self.linked_events
            .iter()
            .filter_map(|event| {
                event
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.total_messages)
            })
            .sum()
    }

    fn user_messages(&self) -> u64 {
        self.linked_events
            .iter()
            .filter_map(|event| {
                event
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.user_messages)
            })
            .sum()
    }

    fn assistant_messages(&self) -> u64 {
        self.linked_events
            .iter()
            .filter_map(|event| {
                event
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.assistant_messages)
            })
            .sum()
    }

    fn developer_messages(&self) -> u64 {
        self.linked_events
            .iter()
            .filter_map(|event| {
                event
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.developer_messages)
            })
            .sum()
    }
}

#[derive(Debug, Default)]
struct PendingGroup {
    spans: Vec<SpanContext>,
    continuation_reasons: BTreeSet<String>,
    manual_title: Option<String>,
    force_verified: bool,
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

#[derive(Debug, Clone)]
struct TitleCandidate {
    title: String,
    normalized: String,
    source: TitleCandidateSource,
    topic_tokens: Vec<String>,
}

#[derive(Debug, Clone)]
struct ContinuationDecision {
    score: i32,
    reasons: BTreeSet<String>,
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
                  normalized_title, is_meta, confidence, source_file_path_hash, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
            let impact = self.delete_task_span_targets_in_tx(&targets)?;
            self.delete_task_work_items_for_project_buckets_in_tx(
                &impact.affected_project_buckets,
            )?;
            Ok(impact)
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
            let impact = self.delete_task_span_targets_in_tx(&targets)?;
            self.delete_task_work_items_for_project_buckets_in_tx(
                &impact.affected_project_buckets,
            )?;
            Ok(impact)
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
        let predicted = self.work_items()?;
        let predicted_members = self.work_item_members_map()?;
        let verifications = self.task_verifications()?;
        let truth =
            ground_truth_from_store(&spans, &predicted, &predicted_members, &verifications)?;
        let current_metrics = evaluate_prediction(
            &truth,
            &predicted_members,
            &rejected_span_ids_from_work_items(&predicted, &predicted_members),
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
            manual_constraints_preserved(&predicted_members, &spans, &verifications);
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
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT project_bucket FROM task_spans ORDER BY project_bucket")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut buckets = BTreeSet::new();
        for row in rows {
            buckets.insert(row?);
        }
        self.rebuild_task_work_items_for_project_buckets(&buckets)
    }

    pub fn rebuild_task_work_items_for_project_buckets(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<u64> {
        if project_buckets.is_empty() {
            return Ok(0);
        }
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.delete_task_work_items_for_project_buckets_in_tx(project_buckets)?;
            let contexts = self.load_span_contexts_for_project_buckets(project_buckets)?;
            let verifications = self.relevant_task_verifications(project_buckets)?;
            let (work_items, members) = build_work_items(contexts, &verifications);
            self.insert_work_items_in_tx(&work_items, &members)?;
            Ok(work_items.len() as u64)
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
    ) -> Result<Vec<(TaskSpanId, String)>> {
        let mut targets = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT span_id, project_bucket FROM task_spans WHERE source_id = ?1 ORDER BY started_at, span_id",
        )?;
        for source_id in source_ids {
            let rows = statement.query_map(params![&source_id.0], |row| {
                Ok((TaskSpanId(row.get(0)?), row.get::<_, String>(1)?))
            })?;
            for row in rows {
                targets.push(row?);
            }
        }
        Ok(targets)
    }

    fn task_span_targets_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<Vec<(TaskSpanId, String)>> {
        let mut targets = Vec::new();
        let mut statement = self.conn.prepare(
            r#"
            SELECT span_id, project_bucket
            FROM task_spans
            WHERE source_id = ?1 AND source_file_path_hash = ?2
            ORDER BY started_at, span_id
            "#,
        )?;
        for file_hash in file_hashes {
            let rows = statement.query_map(params![&source_id.0, file_hash], |row| {
                Ok((TaskSpanId(row.get(0)?), row.get::<_, String>(1)?))
            })?;
            for row in rows {
                targets.push(row?);
            }
        }
        Ok(targets)
    }

    fn delete_task_span_targets_in_tx(
        &self,
        targets: &[(TaskSpanId, String)],
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
        for (span_id, bucket) in targets {
            affected_project_buckets.insert(bucket.clone());
            delete_links.execute(params![&span_id.0])?;
            deleted += delete_spans.execute(params![&span_id.0])? as u64;
        }
        Ok(TaskDeletionImpact {
            deleted,
            affected_project_buckets,
        })
    }

    fn delete_task_work_items_for_project_buckets_in_tx(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<u64> {
        if project_buckets.is_empty() {
            return Ok(0);
        }
        let mut count_stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM task_work_items WHERE project_bucket = ?1")?;
        let mut delete_members = self.conn.prepare(
            r#"
            DELETE FROM task_work_item_members
            WHERE work_item_id IN (
              SELECT work_item_id
              FROM task_work_items
              WHERE project_bucket = ?1
            )
            "#,
        )?;
        let mut delete_items = self
            .conn
            .prepare("DELETE FROM task_work_items WHERE project_bucket = ?1")?;
        let mut deleted = 0u64;
        for bucket in project_buckets {
            deleted += count_stmt.query_row(params![bucket], |row| row.get::<_, u64>(0))?;
            delete_members.execute(params![bucket])?;
            delete_items.execute(params![bucket])?;
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
        let mut spans = Vec::<TaskSpan>::new();
        let mut statement = self.conn.prepare(
            "SELECT payload FROM task_spans WHERE project_bucket = ?1 ORDER BY started_at, span_id",
        )?;
        for bucket in project_buckets {
            let rows = statement.query_map(params![bucket], |row| row.get::<_, String>(0))?;
            for row in rows {
                spans.push(serde_json::from_str(&row?)?);
            }
        }
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        let mut contexts = spans
            .iter()
            .cloned()
            .map(|span| {
                (
                    span.span_id.0.clone(),
                    SpanContext {
                        span,
                        linked_events: Vec::new(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        let mut event_lookup = self.conn.prepare(
            r#"
            SELECT l.span_id, e.payload
            FROM task_span_event_links l
            JOIN usage_events e ON e.event_id = l.event_id
            JOIN task_spans s ON s.span_id = l.span_id
            WHERE s.project_bucket = ?1
            ORDER BY s.started_at, l.event_id
            "#,
        )?;
        for bucket in project_buckets {
            let rows = event_lookup.query_map(params![bucket], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (span_id, payload) = row?;
                if let Some(context) = contexts.get_mut(&span_id) {
                    context.linked_events.push(serde_json::from_str(&payload)?);
                }
            }
        }

        let mut ordered = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(context) = contexts.remove(&span.span_id.0) {
                ordered.push(context);
            }
        }
        Ok(ordered)
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
        let span_bucket_map = self.span_project_bucket_map()?;
        Ok(verifications
            .into_iter()
            .filter(|verification| {
                verification
                    .action
                    .span_ids()
                    .into_iter()
                    .filter_map(|span_id| span_bucket_map.get(span_id.0.as_str()))
                    .any(|bucket| project_buckets.contains(bucket))
            })
            .collect())
    }

    fn span_project_bucket_map(&self) -> Result<HashMap<String, String>> {
        let mut statement = self
            .conn
            .prepare("SELECT span_id, project_bucket FROM task_spans ORDER BY span_id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (span_id, project_bucket) = row?;
            map.insert(span_id, project_bucket);
        }
        Ok(map)
    }
}

fn build_work_items(
    contexts: Vec<SpanContext>,
    verifications: &[TaskVerification],
) -> (Vec<WorkItem>, Vec<WorkItemMember>) {
    let mut by_bucket = BTreeMap::<String, Vec<SpanContext>>::new();
    for context in contexts {
        by_bucket
            .entry(context.span.project_bucket.clone())
            .or_default()
            .push(context);
    }

    let mut work_items = Vec::new();
    let mut members = Vec::new();
    for (bucket, mut bucket_contexts) in by_bucket {
        bucket_contexts.sort_by(|left, right| {
            left.span
                .started_at
                .cmp(&right.span.started_at)
                .then_with(|| left.span.span_id.0.cmp(&right.span.span_id.0))
        });
        let groups = group_spans(bucket_contexts, verifications);
        let bucket_label_stats = build_bucket_label_stats(&groups);
        for group in groups {
            let (work_item, group_members) =
                build_work_item(bucket.clone(), group, verifications, &bucket_label_stats);
            members.extend(group_members);
            work_items.push(work_item);
        }
    }
    (work_items, members)
}

fn build_bucket_label_stats(groups: &[PendingGroup]) -> BucketLabelStats {
    let mut stats = BucketLabelStats {
        document_count: groups.len(),
        ..BucketLabelStats::default()
    };
    for group in groups {
        let mut seen_titles = HashSet::<String>::new();
        let mut seen_tokens = HashSet::<String>::new();
        for candidate in collect_title_candidates(&group.spans) {
            seen_titles.insert(candidate.normalized);
            seen_tokens.extend(candidate.topic_tokens);
        }
        for normalized in seen_titles {
            *stats
                .title_document_frequency
                .entry(normalized)
                .or_default() += 1;
        }
        for token in seen_tokens {
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
    for next in iter {
        let previous = current
            .spans
            .last()
            .expect("pending group has at least one span");
        let decision = continuation_decision(previous, &next);
        let strong_anchor = decision.reasons.contains("same_issue_key")
            || decision.reasons.contains("same_session")
            || decision.reasons.contains("same_title");
        let gap_hours = next
            .span
            .started_at
            .signed_duration_since(previous.ended_at())
            .num_hours();
        let should_continue =
            decision.score >= 4 || (decision.score >= 2 && strong_anchor && gap_hours <= 24);
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

fn continuation_decision(previous: &SpanContext, next: &SpanContext) -> ContinuationDecision {
    let mut score = 0;
    let mut reasons = BTreeSet::new();
    let same_session =
        previous.session_key().is_some() && previous.session_key() == next.session_key();
    let previous_generic = task_title_is_generic(Some(previous.span.title.as_str()));
    let next_generic = task_title_is_generic(Some(next.span.title.as_str()));
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
        && !task_title_is_generic(Some(previous.span.title.as_str()))
    {
        score += 4;
        reasons.insert("same_title".to_string());
    } else {
        let previous_tokens = previous.topic_tokens();
        let next_tokens = next.topic_tokens();
        let overlap = previous_tokens.intersection(&next_tokens).count();
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

    ContinuationDecision { score, reasons }
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
    let mut event_ids = BTreeSet::<String>::new();
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
        for event_id in &context.span.linked_event_ids {
            event_ids.insert(event_id.0.clone());
        }
    }

    let cross_provider = providers.len() > 1;
    let total_tokens = usage.computed_total();
    let has_usage_evidence = !event_ids.is_empty();
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
        task_title_is_generic(Some(context.span.title.as_str()))
            || task_title_is_weak_signal(Some(context.span.title.as_str()))
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
        event_count: event_ids.len() as u64,
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
                break;
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
                if let Some(next_span_id) = next_span_id_in_bucket(after_span_id, &spans_by_id) {
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
            TaskVerificationAction::Split { after_span_id, .. } => {
                let Some(next_span_id) = next_span_id_in_bucket(after_span_id, &spans_by_id) else {
                    continue;
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
    let synthetic_group = PendingGroup {
        spans: spans.to_vec(),
        continuation_reasons: BTreeSet::new(),
        manual_title: None,
        force_verified: false,
    };
    let bucket_label_stats = build_bucket_label_stats(&[synthetic_group]);
    choose_work_item_title_with_stats(spans, &bucket_label_stats)
}

fn choose_work_item_title_with_stats(
    spans: &[SpanContext],
    bucket_label_stats: &BucketLabelStats,
) -> String {
    let mut best_title = None::<String>;
    let mut best_score = i32::MIN;
    let mut frequencies = HashMap::<String, usize>::new();
    let mut topic_frequencies = HashMap::<String, usize>::new();
    let mut source_support = HashMap::<String, BTreeSet<TitleCandidateSource>>::new();
    let ordered_candidates = collect_title_candidates(spans);

    for candidate in &ordered_candidates {
        *frequencies.entry(candidate.normalized.clone()).or_default() += 1;
        for token in &candidate.topic_tokens {
            *topic_frequencies.entry(token.clone()).or_default() += 1;
        }
        source_support
            .entry(candidate.normalized.clone())
            .or_default()
            .insert(candidate.source);
    }

    for candidate in &ordered_candidates {
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
            &ordered_candidates,
            &frequencies,
            bucket_label_stats,
        );
        if score > best_score {
            best_score = score;
            best_title = Some(candidate.title.clone());
        }
    }

    if let Some(title) = best_title {
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
    "Unresolved work item".to_string()
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
    let normalized = normalize_task_title(title);
    let token_count = normalized.split_whitespace().count();
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
    score += task_title_signal_score(Some(title)) * 2;
    score += task_title_corpus_specificity_score(title, bucket_label_stats) * 2;
    score += task_title_corpus_phraseness_score(candidate, bucket_label_stats);
    score -= title_candidate_completeness_penalty(candidate, ordered_candidates, frequencies);
    score += title_candidate_source_bonus(candidate.source, source_support_count);
    score += title_candidate_context_score(candidate, topic_overlap);
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

fn title_candidate_source_bonus(source: TitleCandidateSource, source_support_count: usize) -> i32 {
    let source_bonus = match source {
        TitleCandidateSource::SpanTitle => 0,
        TitleCandidateSource::SummaryPreview => 4,
        TitleCandidateSource::TodoExcerpt => 5,
    };
    source_bonus + ((source_support_count.saturating_sub(1).min(2) as i32) * 2)
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
    for context in spans {
        push_title_candidate(
            &mut candidates,
            Some(context.span.title.as_str()),
            TitleCandidateSource::SpanTitle,
        );
        push_title_candidate(
            &mut candidates,
            context.span.summary_preview.as_deref(),
            TitleCandidateSource::SummaryPreview,
        );
        push_title_candidate(
            &mut candidates,
            context.span.todo_excerpt.as_deref(),
            TitleCandidateSource::TodoExcerpt,
        );
    }
    candidates
}

fn push_title_candidate(
    candidates: &mut Vec<TitleCandidate>,
    raw: Option<&str>,
    source: TitleCandidateSource,
) {
    let Some(title) = task_title_from_prompt(raw) else {
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
    let topic_tokens = title_topic_tokens(&title).into_iter().collect::<Vec<_>>();
    candidates.push(TitleCandidate {
        title,
        normalized,
        source,
        topic_tokens,
    });
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
        SpanContext {
            span: TaskSpan {
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
                title_source: Some("test".to_string()),
                summary_preview: summary_preview.map(ToOwned::to_owned),
                todo_excerpt: None,
                issue_keys: Vec::new(),
                branch_family: branch_family.map(ToOwned::to_owned),
                project_bucket: "bucket".to_string(),
                project: None,
                git: None,
                usage: UsageCounts::default(),
                estimated_cost_usd: None,
                linked_event_ids: Vec::new(),
                confidence: Confidence::Medium,
                is_meta: task_title_is_generic(Some(title)),
                started_at: Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap(),
                ended_at: Some(Utc.with_ymd_and_hms(2026, 6, 30, 12, 5, 0).unwrap()),
                duration_seconds: Some(300),
            },
            linked_events: Vec::new(),
        }
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
        SpanContext {
            span: TaskSpan {
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
                linked_event_ids: Vec::new(),
                confidence: Confidence::Medium,
                is_meta: task_title_is_generic(Some(title)),
                started_at,
                ended_at: Some(started_at + chrono::Duration::minutes(5)),
                duration_seconds: Some(300),
            },
            linked_events: Vec::new(),
        }
    }

    fn test_usage_event_with_message_counts(
        event_id: &str,
        provider: &str,
        session_id: &str,
        started_at: DateTime<Utc>,
        total_messages: u64,
        user_messages: u64,
        assistant_messages: u64,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: statsai_core::USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: EventId(event_id.to_string()),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: SourceId(format!("source_{provider}")),
            provider_account_id: None,
            subscription_id: None,
            source: statsai_core::EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: statsai_core::SourceKind::LocalAdapter,
                location_origin: Some(statsai_core::LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: None,
                source_record_id: Some(event_id.to_string()),
                parse_confidence: Confidence::High,
            },
            session: statsai_core::SessionInfo {
                session_id: session_id.to_string(),
                local_session_id_hash: Some(session_id.to_string()),
                title: None,
                started_at,
                ended_at: Some(started_at + chrono::Duration::minutes(5)),
                duration_seconds: Some(300),
            },
            model: None,
            usage: UsageCounts {
                input_tokens: Some(12),
                output_tokens: Some(3),
                total_tokens: Some(15),
                ..UsageCounts::default()
            },
            runtime: Some(statsai_core::RuntimeInfo {
                runtime_name: None,
                host_id: None,
                latency_ms: Some(1_000),
                latency_source: Some(statsai_core::LatencySource::Explicit),
                time_to_first_token_ms: None,
                prompt_eval_duration_ms: None,
                eval_duration_ms: None,
                total_messages: Some(total_messages),
                user_messages: Some(user_messages),
                assistant_messages: Some(assistant_messages),
                developer_messages: Some(0),
            }),
            cost: statsai_core::CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                pricing_source: Some("unknown".to_string()),
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            git: None,
            privacy: statsai_core::PrivacyInfo {
                mode: statsai_core::PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: started_at,
            imported_at: started_at,
        }
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
    fn corpus_specificity_penalizes_repeated_banner_titles() {
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

        let (work_items, members) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 1);
        assert_eq!(members.len(), 2);
        assert_eq!(work_items[0].span_count, 2);
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

        let (work_items, members) = build_work_items(contexts, &[]);
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

        let (work_items, members) = build_work_items(contexts, &[]);
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

        let (work_items, members) = build_work_items(contexts, &[]);
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

        let (work_items, members) = build_work_items(contexts, &[]);
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

        let (work_items, members) = build_work_items(vec![context], &[]);
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
    fn repeated_banner_titles_with_real_usage_need_review() {
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
            context.span.usage = UsageCounts {
                input_tokens: Some(100),
                output_tokens: Some(20),
                ..UsageCounts::default()
            };
            context
                .linked_events
                .push(test_usage_event_with_message_counts(
                    &format!("event-banner-{index}"),
                    "codex",
                    &format!("session-banner-{index}"),
                    timestamp,
                    8,
                    3,
                    3,
                ));
            contexts.push(context);
        }

        let (work_items, members) = build_work_items(contexts, &[]);
        assert_eq!(work_items.len(), 5);
        assert_eq!(members.len(), 5);
        assert!(work_items
            .iter()
            .all(|item| item.status == TaskStatus::NeedsReview));
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

        let (work_items, members) = build_work_items(vec![context], &[]);
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

        let (work_items, members) = build_work_items(vec![context], &[]);
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
        context
            .linked_events
            .push(test_usage_event_with_message_counts(
                "event-a",
                "codex",
                "session-a",
                started_at,
                2,
                1,
                1,
            ));

        let (work_items, members) = build_work_items(vec![context], &[]);
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
        morning
            .linked_events
            .push(test_usage_event_with_message_counts(
                "event-a",
                "codex",
                "quota-session",
                started_at,
                2,
                1,
                1,
            ));

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
        lunch
            .linked_events
            .push(test_usage_event_with_message_counts(
                "event-b",
                "codex",
                "quota-session",
                started_at + chrono::Duration::hours(4),
                2,
                1,
                1,
            ));

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
        evening
            .linked_events
            .push(test_usage_event_with_message_counts(
                "event-c",
                "codex",
                "quota-session",
                started_at + chrono::Duration::hours(8),
                2,
                1,
                1,
            ));

        let (work_items, members) = build_work_items(vec![morning, lunch, evening], &[]);
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
}
