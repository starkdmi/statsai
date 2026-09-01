use super::*;

impl Store {
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

    pub(crate) fn pending_serialized_entities_for_sync<T: Clone + Serialize>(
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

    pub(crate) fn retired_sync_entity_ids(
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

    pub(crate) fn current_http_sync_authoritative_snapshot(
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
}
