use super::*;

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
    pub(crate) span: TaskSpan,
    pub(crate) topic_tokens: BTreeSet<String>,
    pub(crate) title_is_generic: bool,
    pub(crate) title_is_weak_signal: bool,
    pub(crate) title_signal_score: i32,
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
    pub(crate) fn ended_at(&self) -> DateTime<Utc> {
        self.span.effective_ended_at()
    }

    pub(crate) fn session_key(&self) -> Option<&str> {
        self.span
            .thread_id
            .as_deref()
            .or(self.span.session_id.as_deref())
    }

    pub(crate) fn topic_tokens(&self) -> &BTreeSet<String> {
        &self.topic_tokens
    }

    pub(crate) fn title_is_generic(&self) -> bool {
        self.title_is_generic
    }

    pub(crate) fn title_is_weak_signal(&self) -> bool {
        self.title_is_weak_signal
    }

    pub(crate) fn title_signal_score(&self) -> i32 {
        self.title_signal_score
    }

    pub(crate) fn usage(&self) -> UsageCounts {
        self.span.usage.clone()
    }

    pub(crate) fn estimated_cost_usd(&self) -> Option<i64> {
        self.span.estimated_cost_usd
    }

    pub(crate) fn estimated_cost_micro_usd(&self) -> Option<i64> {
        self.span.estimated_cost_micro_usd
    }

    pub(crate) fn event_count(&self) -> u64 {
        self.span.effective_event_count()
    }

    pub(crate) fn has_usage_evidence(&self) -> bool {
        self.span.effective_has_usage_evidence()
    }

    pub(crate) fn total_messages(&self) -> u64 {
        self.span.total_messages
    }

    pub(crate) fn user_messages(&self) -> u64 {
        self.span.user_messages
    }

    pub(crate) fn assistant_messages(&self) -> u64 {
        self.span.assistant_messages
    }

    pub(crate) fn developer_messages(&self) -> u64 {
        self.span.developer_messages
    }
}

impl Store {
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

    pub(crate) fn delete_task_work_items_for_project_buckets_in_tx(
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

    pub(crate) fn insert_work_items_in_tx(
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

    pub(crate) fn load_span_contexts_for_project_buckets(
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

    pub(crate) fn work_item_members_map(&self) -> Result<HashMap<String, String>> {
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

    pub(crate) fn work_items_for_project_bucket(
        &self,
        project_bucket: &str,
    ) -> Result<Vec<WorkItem>> {
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

    pub(crate) fn work_item_members_for_project_bucket(
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
}
