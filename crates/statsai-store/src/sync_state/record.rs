use super::*;

impl Store {
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

    pub(crate) fn record_sources_synced_in_transaction(
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

    pub(crate) fn record_accounts_synced_in_transaction(
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

    pub(crate) fn record_source_account_assignments_synced_in_transaction(
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

    pub(crate) fn record_subscriptions_synced_in_transaction(
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

    pub(crate) fn record_summaries_synced_in_transaction(
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

    pub(crate) fn record_code_change_metrics_synced_in_transaction(
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

    pub(crate) fn record_quota_cycle_contributions_synced_in_transaction(
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

    pub(crate) fn record_serialized_entities_synced_in_transaction<T: Serialize>(
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
