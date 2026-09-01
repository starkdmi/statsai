use super::*;

impl Store {
    pub fn upsert_task_spans(&self, spans: &[TaskSpan]) -> Result<u64> {
        if spans.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| self.upsert_task_spans_in_tx(spans))
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

    pub(crate) fn task_span_targets_for_sources(
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

    pub(crate) fn task_span_targets_for_source_file_hashes(
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

    pub(crate) fn delete_task_span_targets_in_tx(
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
}
