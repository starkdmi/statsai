use super::*;

impl Store {
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

    pub(crate) fn record_task_bucket_snapshots_synced_in_transaction(
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

    pub(crate) fn delete_task_bucket_snapshot_in_tx(&self, project_bucket: &str) -> Result<()> {
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

    pub(crate) fn dirty_task_bucket_keys_for_sync(
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

    pub(crate) fn task_bucket_is_clean_for_sync(
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

    pub(crate) fn mark_task_buckets_dirty_in_tx(
        &self,
        project_buckets: &BTreeSet<String>,
    ) -> Result<()> {
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
}

pub(crate) fn task_bucket_snapshot_payload_hash(snapshot: &TaskBucketSnapshot) -> Result<String> {
    Ok(hash_text(&serde_json::to_string(snapshot)?))
}
