use super::*;

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

    pub fn pending_sources_for_sync(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<Vec<SourceLocation>> {
        let mut changed = Vec::new();
        for source in sources {
            let payload = serde_json::to_string(source)?;
            if self.entity_requires_sync(
                sink,
                target,
                "source",
                &source.source_id.0,
                &hash_text(&payload),
            )? {
                changed.push(source.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_accounts_for_sync(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<Vec<ProviderAccount>> {
        let mut changed = Vec::new();
        for account in accounts {
            let payload = serde_json::to_string(account)?;
            if self.entity_requires_sync(
                sink,
                target,
                "account",
                &account.provider_account_id.0,
                &hash_text(&payload),
            )? {
                changed.push(account.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_source_account_assignments_for_sync(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<Vec<SourceAccountAssignment>> {
        let mut changed = Vec::new();
        for assignment in assignments {
            let payload = serde_json::to_string(assignment)?;
            if self.entity_requires_sync(
                sink,
                target,
                "source_account_assignment",
                &assignment.assignment_id.0,
                &hash_text(&payload),
            )? {
                changed.push(assignment.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_subscriptions_for_sync(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<Vec<Subscription>> {
        let mut changed = Vec::new();
        for subscription in subscriptions {
            let payload = serde_json::to_string(subscription)?;
            if self.entity_requires_sync(
                sink,
                target,
                "subscription",
                &subscription.subscription_id.0,
                &hash_text(&payload),
            )? {
                changed.push(subscription.clone());
            }
        }
        Ok(changed)
    }

    fn pending_serialized_entities_for_sync<T: Clone + Serialize>(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entities: &[T],
        entity_id: impl Fn(&T) -> &str,
    ) -> Result<Vec<T>> {
        let mut changed = Vec::new();
        for entity in entities {
            let payload = serde_json::to_string(entity)?;
            if self.entity_requires_sync(
                sink,
                target,
                entity_kind,
                entity_id(entity),
                &hash_text(&payload),
            )? {
                changed.push(entity.clone());
            }
        }
        Ok(changed)
    }

    pub fn pending_account_plan_projections_for_sync(
        &self,
        sink: &str,
        target: &str,
        projections: &[AccountPlanProjectionV1],
    ) -> Result<Vec<AccountPlanProjectionV1>> {
        self.pending_serialized_entities_for_sync(
            sink,
            target,
            "account_plan_observation",
            projections,
            |projection| projection.projection_id.as_str(),
        )
    }

    pub fn pending_account_evidence_summaries_for_sync(
        &self,
        sink: &str,
        target: &str,
        summaries: &[AccountEvidenceSummaryV1],
    ) -> Result<Vec<AccountEvidenceSummaryV1>> {
        self.pending_serialized_entities_for_sync(
            sink,
            target,
            "account_evidence_summary",
            summaries,
            |summary| summary.summary_id.as_str(),
        )
    }

    pub fn pending_summaries_for_sync(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<Vec<UsageSummary>> {
        let mut changed = Vec::new();
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )? {
                changed.push(summary.clone());
            }
        }
        Ok(changed)
    }

    pub fn sync_target_has_retired_entities(
        &self,
        sink: &str,
        target: &str,
        snapshot: &SyncAuthoritativeSnapshot,
    ) -> Result<bool> {
        Ok(!self
            .retired_sync_entity_ids(sink, target, snapshot)?
            .is_empty())
    }

    pub fn reconcile_sync_tracking_to_authoritative_snapshot(
        &self,
        sink: &str,
        target: &str,
        snapshot: &SyncAuthoritativeSnapshot,
    ) -> Result<u64> {
        let retired = self.retired_sync_entity_ids(sink, target, snapshot)?;
        if retired.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for (entity_kind, entity_id) in &retired {
                deleted += self.conn.execute(
                    r#"
                    DELETE FROM entity_sync_state
                    WHERE sink = ?1 AND target = ?2 AND entity_kind = ?3 AND entity_id = ?4
                    "#,
                    params![sink, target, entity_kind, entity_id],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    fn retired_sync_entity_ids(
        &self,
        sink: &str,
        target: &str,
        snapshot: &SyncAuthoritativeSnapshot,
    ) -> Result<Vec<(String, String)>> {
        let current_ids = BTreeMap::from([
            (
                "source",
                snapshot
                    .source_ids
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "account",
                snapshot
                    .provider_account_ids
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "source_account_assignment",
                snapshot
                    .source_account_assignment_ids
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "subscription",
                snapshot
                    .subscription_ids
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "summary",
                snapshot
                    .summary_ids
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "code_change_metric",
                snapshot
                    .code_change_metric_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "quota_cycle_contribution",
                snapshot
                    .quota_cycle_contribution_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "account_plan_observation",
                snapshot
                    .account_plan_observation_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "account_evidence_summary",
                snapshot
                    .account_evidence_summary_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
            ),
        ]);
        let mut statement = self.conn.prepare(
            r#"
            SELECT entity_kind, entity_id
            FROM entity_sync_state
            WHERE sink = ?1 AND target = ?2
              AND entity_kind IN (
                'source', 'account', 'source_account_assignment', 'subscription', 'summary',
                'code_change_metric', 'quota_cycle_contribution',
                'account_plan_observation', 'account_evidence_summary'
              )
            "#,
        )?;
        let tracked = statement
            .query_map(params![sink, target], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tracked
            .into_iter()
            .filter(|(entity_kind, entity_id)| {
                current_ids
                    .get(entity_kind.as_str())
                    .is_some_and(|ids| !ids.contains(entity_id.as_str()))
            })
            .collect())
    }

    pub fn pending_http_sync_rollup_summaries(&self, target: &str) -> Result<Vec<UsageSummary>> {
        self.pending_http_sync_rollup_summaries_with_projects(target, false)
    }

    pub fn pending_http_sync_rollup_summaries_with_projects(
        &self,
        target: &str,
        include_projects: bool,
    ) -> Result<Vec<UsageSummary>> {
        let rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_http_sync(summary, include_projects))
            .collect::<Vec<_>>();
        self.pending_summaries_for_sync("http", target, &rollups)
    }

    pub fn pending_http_sync_summary_counts(
        &self,
        target: &str,
        device_id: &str,
    ) -> Result<PendingSyncSummaryCounts> {
        self.pending_http_sync_summary_counts_with_projects(target, device_id, false)
    }

    pub fn pending_http_sync_summary_counts_with_projects(
        &self,
        target: &str,
        device_id: &str,
        include_projects: bool,
    ) -> Result<PendingSyncSummaryCounts> {
        let current_rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_http_sync(summary, include_projects))
            .collect::<Vec<_>>();
        let rollups = self.pending_summaries_for_sync("http", target, &current_rollups)?;
        let current_passthrough_summaries = self
            .summaries()?
            .into_iter()
            .filter(is_http_rollup_passthrough_summary)
            .map(|summary| sanitize_summary_for_http_sync(summary, include_projects))
            .collect::<Vec<_>>();
        let passthrough_summaries =
            self.pending_summaries_for_sync("http", target, &current_passthrough_summaries)?;
        let current_code_change_metrics = self
            .list_code_change_metrics(false)?
            .into_iter()
            .filter(|metric| metric.device_id == device_id)
            .map(|metric| sanitize_code_change_metric_for_sync(metric, include_projects))
            .collect::<Vec<_>>();
        let code_change_metrics = self.pending_code_change_metrics_for_sync(
            "http",
            target,
            &current_code_change_metrics,
        )?;
        let current_quota_cycle_contributions =
            self.quota_cycle_contributions(&QuotaQuery::default(), device_id)?;
        let current_account_plan_observations = self.account_plan_projections(device_id)?;
        let account_plan_observations = self.pending_account_plan_projections_for_sync(
            "http",
            target,
            &current_account_plan_observations,
        )?;
        let current_account_evidence_summaries = self.account_evidence_summaries(device_id)?;
        let account_evidence_summaries = self.pending_account_evidence_summaries_for_sync(
            "http",
            target,
            &current_account_evidence_summaries,
        )?;
        let current_snapshot = self.current_http_sync_authoritative_snapshot(
            &current_rollups,
            &current_passthrough_summaries,
            &current_code_change_metrics,
            &current_quota_cycle_contributions
                .iter()
                .map(|contribution| contribution.contribution_id.clone())
                .collect::<Vec<_>>(),
            &current_account_plan_observations,
            &current_account_evidence_summaries,
        )?;
        // A quota cycle can change without any summary changing: a reset moves,
        // or an observation carries no tokens. Counting only the summary-shaped
        // entities left those uploads invisible, so the menubar reported nothing
        // pending while a sync would still have sent them.
        let quota_cycle_contributions = self.pending_quota_cycle_contributions_for_sync(
            "http",
            target,
            &current_quota_cycle_contributions,
        )?;
        let retired_entities = self
            .retired_sync_entity_ids("http", target, &current_snapshot)?
            .len();
        let mut days = collect_pending_summary_days(rollups.iter());
        days.extend(collect_pending_summary_days(passthrough_summaries.iter()));
        days.extend(code_change_metrics.iter().map(|metric| metric.day));
        Ok(PendingSyncSummaryCounts {
            rollups: rollups.len() as u64,
            passthrough_summaries: passthrough_summaries.len() as u64,
            retired_entities: retired_entities as u64,
            quota_cycle_contributions: quota_cycle_contributions.len() as u64,
            total: rollups
                .len()
                .saturating_add(passthrough_summaries.len())
                .saturating_add(code_change_metrics.len())
                .saturating_add(quota_cycle_contributions.len())
                .saturating_add(account_plan_observations.len())
                .saturating_add(account_evidence_summaries.len())
                .saturating_add(retired_entities) as u64,
            days: days.len() as u64,
        })
    }

    fn current_http_sync_authoritative_snapshot(
        &self,
        rollups: &[UsageSummary],
        passthrough_summaries: &[UsageSummary],
        code_change_metrics: &[CodeChangeMetric],
        quota_cycle_contribution_ids: &[String],
        account_plan_observations: &[AccountPlanProjectionV1],
        account_evidence_summaries: &[AccountEvidenceSummaryV1],
    ) -> Result<SyncAuthoritativeSnapshot> {
        Ok(SyncAuthoritativeSnapshot {
            snapshot_id: String::new(),
            part_index: 0,
            part_count: 1,
            source_ids: self
                .list_sources()?
                .into_iter()
                .map(|source| source.source_id)
                .collect(),
            provider_account_ids: self
                .list_accounts()?
                .into_iter()
                .map(|account| account.provider_account_id)
                .collect(),
            source_account_assignment_ids: self
                .list_source_account_assignments()?
                .into_iter()
                .map(|assignment| assignment.assignment_id)
                .collect(),
            subscription_ids: self
                .list_subscriptions()?
                .into_iter()
                .map(|subscription| subscription.subscription_id)
                .collect(),
            summary_ids: rollups
                .iter()
                .chain(passthrough_summaries)
                .map(|summary| summary.summary_id.clone())
                .collect(),
            code_change_metric_ids: code_change_metrics
                .iter()
                .map(|metric| metric.metric_id.clone())
                .collect(),
            quota_cycle_contribution_ids: quota_cycle_contribution_ids.to_vec(),
            account_plan_observation_ids: account_plan_observations
                .iter()
                .map(|observation| observation.projection_id.clone())
                .collect(),
            account_evidence_summary_ids: account_evidence_summaries
                .iter()
                .map(|summary| summary.summary_id.clone())
                .collect(),
        })
    }

    pub fn record_sources_synced(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_sources_synced_in_transaction(sink, target, sources)
        })
    }

    fn record_sources_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        sources: &[SourceLocation],
    ) -> Result<()> {
        for source in sources {
            let payload = serde_json::to_string(source)?;
            self.record_entity_synced(
                sink,
                target,
                "source",
                &source.source_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_accounts_synced(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<()> {
        if accounts.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_accounts_synced_in_transaction(sink, target, accounts)
        })
    }

    fn record_accounts_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        accounts: &[ProviderAccount],
    ) -> Result<()> {
        for account in accounts {
            let payload = serde_json::to_string(account)?;
            self.record_entity_synced(
                sink,
                target,
                "account",
                &account.provider_account_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_source_account_assignments_synced(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<()> {
        if assignments.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_source_account_assignments_synced_in_transaction(sink, target, assignments)
        })
    }

    fn record_source_account_assignments_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        assignments: &[SourceAccountAssignment],
    ) -> Result<()> {
        for assignment in assignments {
            let payload = serde_json::to_string(assignment)?;
            self.record_entity_synced(
                sink,
                target,
                "source_account_assignment",
                &assignment.assignment_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_subscriptions_synced(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<()> {
        if subscriptions.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_subscriptions_synced_in_transaction(sink, target, subscriptions)
        })
    }

    fn record_subscriptions_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        subscriptions: &[Subscription],
    ) -> Result<()> {
        for subscription in subscriptions {
            let payload = serde_json::to_string(subscription)?;
            self.record_entity_synced(
                sink,
                target,
                "subscription",
                &subscription.subscription_id.0,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_summaries_synced(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<()> {
        if summaries.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_summaries_synced_in_transaction(sink, target, summaries)
        })
    }

    fn record_summaries_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<()> {
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            self.record_entity_synced(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )?;
        }
        Ok(())
    }

    pub fn record_code_change_metrics_synced(
        &self,
        sink: &str,
        target: &str,
        metrics: &[CodeChangeMetric],
    ) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_code_change_metrics_synced_in_transaction(sink, target, metrics)
        })
    }

    fn record_code_change_metrics_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        metrics: &[CodeChangeMetric],
    ) -> Result<()> {
        for metric in metrics {
            let payload = serde_json::to_string(metric)?;
            self.record_entity_synced(
                sink,
                target,
                "code_change_metric",
                &metric.metric_id,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_quota_cycle_contributions_synced(
        &self,
        sink: &str,
        target: &str,
        contributions: &[statsai_core::QuotaCycleContributionV1],
    ) -> Result<()> {
        if contributions.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_quota_cycle_contributions_synced_in_transaction(sink, target, contributions)
        })
    }

    fn record_quota_cycle_contributions_synced_in_transaction(
        &self,
        sink: &str,
        target: &str,
        contributions: &[statsai_core::QuotaCycleContributionV1],
    ) -> Result<()> {
        for contribution in contributions {
            let payload = serde_json::to_string(contribution)?;
            self.record_entity_synced(
                sink,
                target,
                "quota_cycle_contribution",
                &contribution.contribution_id,
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    fn record_serialized_entities_synced_in_transaction<T: Serialize>(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entities: &[T],
        entity_id: impl Fn(&T) -> &str,
    ) -> Result<()> {
        for entity in entities {
            let payload = serde_json::to_string(entity)?;
            self.record_entity_synced(
                sink,
                target,
                entity_kind,
                entity_id(entity),
                &hash_text(&payload),
            )?;
        }
        Ok(())
    }

    pub fn record_account_plan_projections_synced(
        &self,
        sink: &str,
        target: &str,
        projections: &[AccountPlanProjectionV1],
    ) -> Result<()> {
        if projections.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_serialized_entities_synced_in_transaction(
                sink,
                target,
                "account_plan_observation",
                projections,
                |projection| projection.projection_id.as_str(),
            )
        })
    }

    pub fn record_account_evidence_summaries_synced(
        &self,
        sink: &str,
        target: &str,
        summaries: &[AccountEvidenceSummaryV1],
    ) -> Result<()> {
        if summaries.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.record_serialized_entities_synced_in_transaction(
                sink,
                target,
                "account_evidence_summary",
                summaries,
                |summary| summary.summary_id.as_str(),
            )
        })
    }

    pub(crate) fn entity_requires_sync(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entity_id: &str,
        payload_hash: &str,
    ) -> Result<bool> {
        let existing: Option<String> = self
            .conn
            .query_row(
                r#"
                SELECT payload_hash
                FROM entity_sync_state
                WHERE sink = ?1 AND target = ?2 AND entity_kind = ?3 AND entity_id = ?4
                "#,
                params![sink, target, entity_kind, entity_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(existing.as_deref() != Some(payload_hash))
    }

    pub(crate) fn record_entity_synced(
        &self,
        sink: &str,
        target: &str,
        entity_kind: &str,
        entity_id: &str,
        payload_hash: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO entity_sync_state (
              sink, target, entity_kind, entity_id, payload_hash, synced_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(sink, target, entity_kind, entity_id) DO UPDATE SET
              payload_hash = excluded.payload_hash,
              synced_at = excluded.synced_at
            "#,
            params![
                sink,
                target,
                entity_kind,
                entity_id,
                payload_hash,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }
}
