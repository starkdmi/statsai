use super::*;

impl Store {
    pub fn replace_task_bucket_snapshot(&self, snapshot: &TaskBucketSnapshot) -> Result<()> {
        self.with_immediate_transaction(|| {
            let mut raw_session_ids =
                self.task_span_raw_session_ids_for_bucket(&snapshot.project_bucket)?;
            self.delete_task_bucket_snapshot_in_tx(&snapshot.project_bucket)?;
            self.upsert_task_spans_in_tx(&snapshot.spans)?;
            for span in &snapshot.spans {
                if let Some(session_id) = span
                    .session_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    raw_session_ids.insert(session_id.to_string());
                }
            }
            self.refresh_session_rollups_for_raw_ids(
                &raw_session_ids.into_iter().collect::<Vec<_>>(),
            )?;
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
        let pending = self.task_bucket_sync_selection(sink, target, device_id)?;
        let bucket_ids = if full {
            pending.local.union(&pending.pending).cloned().collect()
        } else {
            pending.pending
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
              sink, target, device_id, project_bucket, dirty, payload_hash, updated_at,
              sanitizer_version
            )
            VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)
            ON CONFLICT(sink, target, device_id, project_bucket) DO UPDATE SET
              dirty = 0,
              payload_hash = excluded.payload_hash,
              updated_at = excluded.updated_at,
              sanitizer_version = excluded.sanitizer_version
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
                TASK_BUCKET_SYNC_SANITIZER_VERSION,
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

    /// Which task buckets the target still needs, by the one predicate both
    /// sync selection and sync status use: [`task_bucket_needs_sync`].
    pub(crate) fn task_bucket_sync_selection(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
    ) -> Result<TaskBucketSyncSelection> {
        let local = self.task_project_buckets()?;
        let mut statement = self.conn.prepare(
            r#"
            SELECT project_bucket, dirty, sanitizer_version
            FROM task_bucket_sync_state
            WHERE sink = ?1 AND target = ?2 AND device_id = ?3
            "#,
        )?;
        let rows = statement.query_map(params![sink, target, device_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                TrackedTaskBucket {
                    dirty: row.get::<_, i64>(1)? != 0,
                    sanitizer_version: row.get::<_, i64>(2)?,
                },
            ))
        })?;
        let mut tracked = BTreeMap::new();
        for row in rows {
            let (project_bucket, state) = row?;
            tracked.insert(project_bucket, state);
        }
        let pending = local
            .iter()
            .chain(tracked.keys())
            .filter(|bucket| {
                task_bucket_needs_sync(
                    tracked.get(bucket.as_str()),
                    local.contains(bucket.as_str()),
                )
            })
            .cloned()
            .collect();
        let total = local
            .iter()
            .filter(|bucket| !tracked.contains_key(bucket.as_str()))
            .count()
            + tracked.len();
        Ok(TaskBucketSyncSelection {
            local,
            pending,
            total,
        })
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

pub(crate) struct TaskBucketSyncSelection {
    /// Buckets that exist locally.
    pub(crate) local: BTreeSet<String>,
    /// Buckets an incremental sync sends: local or tracked ones the target
    /// does not hold as they are now.
    pub(crate) pending: BTreeSet<String>,
    /// Local buckets plus tracked ones that no longer exist locally.
    pub(crate) total: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TrackedTaskBucket {
    pub(crate) dirty: bool,
    pub(crate) sanitizer_version: i64,
}

/// Whether the target needs this bucket sent, given its acknowledgement (if
/// any) and whether the bucket still exists locally.
///
/// A local bucket is pending until acknowledged, after any change, and when
/// it was acknowledged under an older sync sanitizer: the target then holds a
/// payload the current sanitizer would not send. A bucket that no longer
/// exists locally can only go out as an empty deletion tombstone, which
/// carries no labels, so only a change since its acknowledgement makes it
/// pending -- a sanitizer change alone would re-send a tombstone forever
/// without changing anything the target holds.
pub(crate) fn task_bucket_needs_sync(tracked: Option<&TrackedTaskBucket>, local: bool) -> bool {
    match tracked {
        None => local,
        Some(state) => {
            state.dirty || (local && state.sanitizer_version < TASK_BUCKET_SYNC_SANITIZER_VERSION)
        }
    }
}

pub(crate) fn task_bucket_snapshot_payload_hash(snapshot: &TaskBucketSnapshot) -> Result<String> {
    Ok(hash_text(&serde_json::to_string(snapshot)?))
}
