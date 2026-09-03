use super::*;

mod pending;
mod record;

impl Store {
    pub fn sync_state(&self, sink: &str, target: &str) -> Result<Option<SyncState>> {
        self.conn
            .query_row(
                r#"
                SELECT sink, target, last_success_at, last_batch_id, last_event_started_at,
                       last_event_id, last_summary_observed_at, last_summary_id,
                       last_task_verification_updated_at, last_task_verification_id,
                       failure_count, pending_resume_batch_id
                FROM sync_state
                WHERE sink = ?1 AND target = ?2
                "#,
                params![sink, target],
                sync_state_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sync_states(&self) -> Result<Vec<SyncState>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT sink, target, last_success_at, last_batch_id, last_event_started_at,
                   last_event_id, last_summary_observed_at, last_summary_id,
                   last_task_verification_updated_at, last_task_verification_id,
                   failure_count, pending_resume_batch_id
            FROM sync_state
            ORDER BY sink, target
            "#,
        )?;
        let rows = stmt.query_map([], sync_state_from_row)?;
        let mut states = Vec::new();
        for row in rows {
            states.push(row?);
        }
        Ok(states)
    }

    /// Writes sync cursors back verbatim, for callers that have to carry them across
    /// a database being replaced. Only rows strictly newer than what is already here
    /// are written, so restoring a stale cursor cannot rewind a fresher one.
    pub fn restore_sync_states(&self, states: &[SyncState]) -> Result<usize> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut restored = 0;
            for state in states {
                let existing: Option<DateTime<Utc>> = self
                    .conn
                    .query_row(
                        "SELECT last_success_at FROM sync_state WHERE sink = ?1 AND target = ?2",
                        params![state.sink, state.target],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .and_then(|value| {
                        DateTime::parse_from_rfc3339(&value)
                            .ok()
                            .map(|parsed| parsed.with_timezone(&Utc))
                    });
                if existing.is_some_and(|current| current >= state.last_success_at) {
                    continue;
                }
                self.conn.execute(
                    r#"
                    INSERT INTO sync_state (
                        sink, target, last_success_at, last_batch_id, last_event_started_at,
                        last_event_id, last_summary_observed_at, last_summary_id,
                        last_task_verification_updated_at, last_task_verification_id,
                        failure_count, pending_resume_batch_id
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    ON CONFLICT(sink, target) DO UPDATE SET
                        last_success_at = excluded.last_success_at,
                        last_batch_id = excluded.last_batch_id,
                        last_event_started_at = excluded.last_event_started_at,
                        last_event_id = excluded.last_event_id,
                        last_summary_observed_at = excluded.last_summary_observed_at,
                        last_summary_id = excluded.last_summary_id,
                        last_task_verification_updated_at = excluded.last_task_verification_updated_at,
                        last_task_verification_id = excluded.last_task_verification_id,
                        failure_count = excluded.failure_count,
                        pending_resume_batch_id = excluded.pending_resume_batch_id
                    "#,
                    params![
                        state.sink,
                        state.target,
                        state.last_success_at.to_rfc3339(),
                        state.last_batch_id,
                        state.last_event_started_at.map(|value| value.to_rfc3339()),
                        state.last_event_id,
                        state.last_summary_observed_at.map(|value| value.to_rfc3339()),
                        state.last_summary_id,
                        state
                            .last_task_verification_updated_at
                            .map(|value| value.to_rfc3339()),
                        state.last_task_verification_id,
                        state.failure_count,
                        state.pending_resume_batch_id,
                    ],
                )?;
                restored += 1;
            }
            Ok(restored)
        })();

        match result {
            Ok(restored) => {
                commit_transaction(&self.conn)?;
                Ok(restored)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn clear_sync_tracking(&self) -> Result<()> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.conn.execute("DELETE FROM entity_sync_state", [])?;
            self.conn
                .execute("DELETE FROM task_bucket_sync_state", [])?;
            self.conn.execute("DELETE FROM sync_state", [])?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                commit_transaction(&self.conn)?;
                Ok(())
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn clear_sync_tracking_for_target(&self, sink: &str, target: &str) -> Result<()> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM entity_sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            self.conn.execute(
                "DELETE FROM task_bucket_sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            self.conn.execute(
                "DELETE FROM sync_state WHERE sink = ?1 AND target = ?2",
                params![sink, target],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                commit_transaction(&self.conn)?;
                Ok(())
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn record_sync_success(
        &self,
        sink: &str,
        target: &str,
        batch_id: &str,
        events: &[UsageEvent],
        summaries: &[UsageSummary],
        task_verification_cursor: Option<&TaskVerificationCursor>,
    ) -> Result<()> {
        let event_cursor = events
            .iter()
            .max_by(|left, right| {
                left.session
                    .started_at
                    .cmp(&right.session.started_at)
                    .then_with(|| left.event_id.0.cmp(&right.event_id.0))
            })
            .map(|event| (event.session.started_at, event.event_id.0.as_str()));
        let summary_cursor = summaries
            .iter()
            .max_by(|left, right| {
                left.observed_at
                    .cmp(&right.observed_at)
                    .then_with(|| left.summary_id.0.cmp(&right.summary_id.0))
            })
            .map(|summary| (summary.observed_at, summary.summary_id.0.as_str()));
        let existing = self.sync_state(sink, target)?;
        let event_started_at = event_cursor.map(|(date, _)| date).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_event_started_at)
        });
        let event_id = event_cursor.map(|(_, id)| id.to_string()).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_event_id.clone())
        });
        let summary_observed_at = summary_cursor.map(|(date, _)| date).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_summary_observed_at)
        });
        let summary_id = summary_cursor.map(|(_, id)| id.to_string()).or_else(|| {
            existing
                .as_ref()
                .and_then(|state| state.last_summary_id.clone())
        });
        let task_verification_updated_at = task_verification_cursor
            .map(|cursor| cursor.updated_at)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|state| state.last_task_verification_updated_at)
            });
        let task_verification_id = task_verification_cursor
            .map(|cursor| cursor.verification_id.0.clone())
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|state| state.last_task_verification_id.clone())
            });
        let now = Utc::now();

        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, last_event_started_at,
              last_event_id, last_summary_observed_at, last_summary_id,
              last_task_verification_updated_at, last_task_verification_id, failure_count
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
            ON CONFLICT(sink, target) DO UPDATE SET
              last_success_at = excluded.last_success_at,
              last_batch_id = excluded.last_batch_id,
              last_event_started_at = excluded.last_event_started_at,
              last_event_id = excluded.last_event_id,
              last_summary_observed_at = excluded.last_summary_observed_at,
              last_summary_id = excluded.last_summary_id,
              last_task_verification_updated_at = excluded.last_task_verification_updated_at,
              last_task_verification_id = excluded.last_task_verification_id,
              failure_count = 0
            "#,
            params![
                sink,
                target,
                now.to_rfc3339(),
                batch_id,
                event_started_at.map(|date| date.to_rfc3339()),
                event_id,
                summary_observed_at.map(|date| date.to_rfc3339()),
                summary_id,
                task_verification_updated_at.map(|date| date.to_rfc3339()),
                task_verification_id,
            ],
        )?;
        Ok(())
    }

    fn sync_batch_task_verification_cursor(batch: &SyncBatch) -> Option<TaskVerificationCursor> {
        batch
            .task_buckets
            .iter()
            .filter_map(|bucket| bucket.applied_verification_cursor.clone())
            .max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
            })
    }

    pub fn record_rollup_chunk_sync_success(
        &self,
        sink: &str,
        target: &str,
        logical_batch_id: &str,
        batch: &SyncBatch,
    ) -> Result<()> {
        self.ensure_current_sync_rollup_versions()?;
        let passthrough_summaries: Vec<_> = batch
            .summaries
            .iter()
            .filter(|summary| !is_daily_rollup_summary(summary))
            .cloned()
            .collect();
        let rollup_summary_ids: Vec<_> = batch
            .summaries
            .iter()
            .filter(|summary| is_daily_rollup_summary(summary))
            .map(|summary| summary.summary_id.clone())
            .collect();
        let rollup_summaries = self.all_sync_rollup_summaries()?;
        let task_verification_cursor = Self::sync_batch_task_verification_cursor(batch);

        self.with_immediate_transaction(|| {
            self.record_sync_success(
                sink,
                target,
                logical_batch_id,
                &batch.events,
                &passthrough_summaries,
                task_verification_cursor.as_ref(),
            )?;
            self.mark_pending_sync_resume(sink, target, logical_batch_id)?;
            self.mark_sync_rollups_synced_in_transaction(&rollup_summary_ids)?;
            self.reconcile_sync_rollup_dirty_flags_in_transaction(sink, target, &rollup_summaries)?;
            self.record_summaries_synced_in_transaction(sink, target, &batch.summaries)?;
            self.record_sources_synced_in_transaction(sink, target, &batch.sources)?;
            self.record_accounts_synced_in_transaction(sink, target, &batch.accounts)?;
            self.record_source_account_assignments_synced_in_transaction(
                sink,
                target,
                &batch.source_account_assignments,
            )?;
            self.record_subscriptions_synced_in_transaction(sink, target, &batch.subscriptions)?;
            self.record_code_change_metrics_synced_in_transaction(
                sink,
                target,
                &batch.code_change_metrics,
            )?;
            self.record_quota_cycle_contributions_synced_in_transaction(
                sink,
                target,
                &batch.quota_cycle_contributions,
            )?;
            self.record_serialized_entities_synced_in_transaction(
                sink,
                target,
                "account_plan_observation",
                &batch.account_plan_observations,
                |projection| projection.projection_id.as_str(),
            )?;
            self.record_serialized_entities_synced_in_transaction(
                sink,
                target,
                "account_evidence_summary",
                &batch.account_evidence_summaries,
                |summary| summary.summary_id.as_str(),
            )?;
            self.record_task_bucket_snapshots_synced_in_transaction(
                sink,
                target,
                &batch.device_id,
                &batch.task_buckets,
            )?;
            self.record_task_verifications_synced_in_transaction(
                sink,
                target,
                &batch.task_verifications,
            )?;
            Ok(())
        })
    }

    pub fn sync_task_verification_cursor(
        &self,
        sink: &str,
        target: &str,
    ) -> Result<Option<TaskVerificationCursor>> {
        let Some(state) = self.sync_state(sink, target)? else {
            return Ok(None);
        };
        let Some(updated_at) = state.last_task_verification_updated_at else {
            return Ok(None);
        };
        let Some(verification_id) = state.last_task_verification_id else {
            return Ok(None);
        };
        Ok(Some(TaskVerificationCursor {
            updated_at,
            verification_id: TaskVerificationId(verification_id),
        }))
    }

    pub fn task_bucket_sync_status(
        &self,
        sink: &str,
        target: &str,
        device_id: &str,
    ) -> Result<TaskBucketSyncStatus> {
        let local_buckets = self.task_project_buckets()?;
        let mut statement = self.conn.prepare(
            r#"
            SELECT project_bucket, dirty
            FROM task_bucket_sync_state
            WHERE sink = ?1 AND target = ?2 AND device_id = ?3
            "#,
        )?;
        let rows = statement.query_map(params![sink, target, device_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut tracked = HashMap::<String, i64>::new();
        for row in rows {
            let (project_bucket, dirty) = row?;
            tracked.insert(project_bucket, dirty);
        }
        let tracked_total = tracked.len() as u64;
        let tracked_dirty = tracked.values().filter(|dirty| **dirty == 1).count() as u64;
        let missing_local = local_buckets
            .iter()
            .filter(|project_bucket| !tracked.contains_key(project_bucket.as_str()))
            .count() as u64;
        Ok(TaskBucketSyncStatus {
            total: tracked_total.saturating_add(missing_local),
            dirty: tracked_dirty.saturating_add(missing_local),
        })
    }

    pub fn record_sync_failure(&self, sink: &str, target: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, failure_count, pending_resume_batch_id
            )
            VALUES (?1, ?2, ?3, '', 1, NULL)
            ON CONFLICT(sink, target) DO UPDATE SET
              failure_count = failure_count + 1
            "#,
            params![sink, target, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn mark_pending_sync_resume(&self, sink: &str, target: &str, batch_id: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (
              sink, target, last_success_at, last_batch_id, failure_count, pending_resume_batch_id
            )
            VALUES (?1, ?2, ?3, '', 0, ?4)
            ON CONFLICT(sink, target) DO UPDATE SET
              pending_resume_batch_id = excluded.pending_resume_batch_id
            "#,
            params![sink, target, Utc::now().to_rfc3339(), batch_id],
        )?;
        Ok(())
    }

    pub fn clear_pending_sync_resume(&self, sink: &str, target: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_state SET pending_resume_batch_id = NULL WHERE sink = ?1 AND target = ?2",
            params![sink, target],
        )?;
        Ok(())
    }
}
