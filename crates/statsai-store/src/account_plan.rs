use super::{
    assignment_for_timestamp, is_verified_source_assignment, reattribute_source_records,
    validate_time_window, Store,
};
use anyhow::Result;
use rusqlite::params;
use statsai_core::{
    account_plan_observation_id, conversation_account_binding_id, hash_text, normalize_plan_name,
    periods_overlap, plan_projection_from_observation, source_account_assignment_id,
    AccountEvidenceCheckpointV1, AccountEvidenceKind, AccountEvidenceSummaryV1,
    AccountIdentityObservationV1, AccountPlanObservationV1, AccountPlanProjectionV1, Confidence,
    ConversationAccountBindingV1, IdentitySource, ProviderAccountId, QuotaObservationRecordV1,
    SourceAccountAssignment, SourceId, UsageEvent, ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION,
    ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountEvidenceReferenceCounts {
    pub identity_observations: usize,
    pub plan_observations: usize,
    pub conversation_bindings: usize,
}

impl AccountEvidenceReferenceCounts {
    #[must_use]
    pub const fn total(self) -> usize {
        self.identity_observations + self.plan_observations + self.conversation_bindings
    }
}

impl Store {
    pub fn account_evidence_checkpoints(
        &self,
        source_id: &SourceId,
    ) -> Result<Vec<AccountEvidenceCheckpointV1>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload
            FROM account_evidence_checkpoints
            WHERE source_id = ?1
            ORDER BY artifact_path_hash, parser_version
            "#,
        )?;
        let rows = statement.query_map([&source_id.0], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn upsert_account_evidence_checkpoints(
        &self,
        checkpoints: &[AccountEvidenceCheckpointV1],
    ) -> Result<u64> {
        if checkpoints.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut written = 0u64;
            let updated_at = chrono::Utc::now().to_rfc3339();
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO account_evidence_checkpoints (
                  source_id, artifact_path_hash, parser_version, maximum_row_id,
                  checkpoint_row_fingerprint, database_size, database_modified_nanos,
                  wal_size, wal_modified_nanos, payload, updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(source_id, artifact_path_hash, parser_version) DO UPDATE SET
                  maximum_row_id = excluded.maximum_row_id,
                  checkpoint_row_fingerprint = excluded.checkpoint_row_fingerprint,
                  database_size = excluded.database_size,
                  database_modified_nanos = excluded.database_modified_nanos,
                  wal_size = excluded.wal_size,
                  wal_modified_nanos = excluded.wal_modified_nanos,
                  payload = excluded.payload,
                  updated_at = excluded.updated_at
                "#,
            )?;
            for checkpoint in checkpoints {
                written += statement.execute(params![
                    &checkpoint.source_id.0,
                    &checkpoint.artifact_path_hash,
                    &checkpoint.parser_version,
                    checkpoint.maximum_row_id,
                    &checkpoint.checkpoint_row_fingerprint,
                    checkpoint.database_size,
                    checkpoint.database_modified_nanos,
                    checkpoint.wal_size,
                    checkpoint.wal_modified_nanos,
                    serde_json::to_string(checkpoint)?,
                    &updated_at,
                ])? as u64;
            }
            Ok(written)
        })
    }

    pub fn account_evidence_reference_counts(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
    ) -> Result<AccountEvidenceReferenceCounts> {
        let count = |table: &str| -> Result<usize> {
            let value = self.conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE provider = ?1 AND provider_account_id = ?2"
                ),
                params![provider, &provider_account_id.0],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(usize::try_from(value).unwrap_or(usize::MAX))
        };
        Ok(AccountEvidenceReferenceCounts {
            identity_observations: count("account_identity_observations")?,
            plan_observations: count("account_plan_observations")?,
            conversation_bindings: count("conversation_account_bindings")?,
        })
    }

    /// Moves every local evidence contribution to a canonical account reference.
    ///
    /// Plan and conversation identifiers encode the account ID, so they are regenerated while
    /// identity observations retain their account-independent deterministic identifiers.
    pub fn rekey_account_evidence(
        &self,
        provider: &str,
        from_provider_account_id: &ProviderAccountId,
        target_provider_account_id: &ProviderAccountId,
    ) -> Result<AccountEvidenceReferenceCounts> {
        self.with_immediate_transaction(|| {
            let identities = self
                .account_identity_observations(None)?
                .into_iter()
                .filter(|observation| observation.provider == provider)
                .filter(|observation| {
                    observation.provider_account_id.as_ref() == Some(from_provider_account_id)
                })
                .collect::<Vec<_>>();
            for mut observation in identities.iter().cloned() {
                observation.provider_account_id = Some(target_provider_account_id.clone());
                self.conn.execute(
                    r#"
                    UPDATE account_identity_observations
                    SET provider_account_id = ?1, payload = ?2
                    WHERE observation_id = ?3
                    "#,
                    params![
                        &target_provider_account_id.0,
                        serde_json::to_string(&observation)?,
                        &observation.observation_id,
                    ],
                )?;
            }

            let plans = self
                .account_plan_observations()?
                .into_iter()
                .filter(|observation| observation.provider == provider)
                .filter(|observation| {
                    observation.provider_account_id.as_ref() == Some(from_provider_account_id)
                })
                .collect::<Vec<_>>();
            for mut observation in plans.iter().cloned() {
                let previous_id = observation.observation_id.clone();
                observation.provider_account_id = Some(target_provider_account_id.clone());
                observation.observation_id = account_plan_observation_id(
                    &observation.source_id,
                    Some(target_provider_account_id),
                    &observation.raw_plan_name,
                    observation.observed_at,
                    observation.evidence_kind,
                );
                self.upsert_account_plan_observations(std::slice::from_ref(&observation))?;
                if previous_id != observation.observation_id {
                    self.conn.execute(
                        "DELETE FROM account_plan_observations WHERE observation_id = ?1",
                        [&previous_id],
                    )?;
                }
            }

            let bindings = self
                .conversation_account_bindings(None)?
                .into_iter()
                .filter(|binding| binding.provider == provider)
                .filter(|binding| binding.provider_account_id == *from_provider_account_id)
                .collect::<Vec<_>>();
            for mut binding in bindings.iter().cloned() {
                let previous_id = binding.binding_id.clone();
                binding.provider_account_id = target_provider_account_id.clone();
                binding.binding_id = conversation_account_binding_id(
                    &binding.source_id,
                    &binding.conversation_id_hash,
                    binding.turn_id_hash.as_deref(),
                    target_provider_account_id,
                );
                self.upsert_conversation_account_bindings(std::slice::from_ref(&binding))?;
                if previous_id != binding.binding_id {
                    self.conn.execute(
                        "DELETE FROM conversation_account_bindings WHERE binding_id = ?1",
                        [&previous_id],
                    )?;
                }
            }

            Ok(AccountEvidenceReferenceCounts {
                identity_observations: identities.len(),
                plan_observations: plans.len(),
                conversation_bindings: bindings.len(),
            })
        })
    }

    /// Removes evidence records already present in the append-only ledger.
    ///
    /// Collectors can safely rediscover deterministic records, but filtering before the scanner
    /// decides whether it has work avoids opening a write transaction for an unchanged artifact.
    pub fn retain_unseen_account_evidence(
        &self,
        source_id: &SourceId,
        identity_observations: &mut Vec<AccountIdentityObservationV1>,
        plan_observations: &mut Vec<AccountPlanObservationV1>,
        conversation_bindings: &mut Vec<ConversationAccountBindingV1>,
    ) -> Result<()> {
        let load_ids = |table: &str, id_column: &str| -> Result<HashSet<String>> {
            let mut statement = self.conn.prepare(&format!(
                "SELECT {id_column} FROM {table} WHERE source_id = ?1"
            ))?;
            let rows = statement.query_map([&source_id.0], |row| row.get::<_, String>(0))?;
            rows.map(|row| Ok(row?)).collect()
        };
        let identity_ids = load_ids("account_identity_observations", "observation_id")?;
        let plan_ids = load_ids("account_plan_observations", "observation_id")?;
        let binding_ids = load_ids("conversation_account_bindings", "binding_id")?;
        identity_observations
            .retain(|observation| !identity_ids.contains(&observation.observation_id));
        plan_observations.retain(|observation| !plan_ids.contains(&observation.observation_id));
        conversation_bindings.retain(|binding| !binding_ids.contains(&binding.binding_id));
        Ok(())
    }

    /// Deletes every local identity/plan evidence row owned by the supplied scanner sources.
    ///
    /// This deliberately includes direct conversation bindings: their hashed locators remain
    /// source-scoped personal data even when the corresponding usage events have been removed.
    pub fn delete_account_evidence_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                for table in [
                    "account_identity_observations",
                    "account_plan_observations",
                    "conversation_account_bindings",
                    "account_evidence_checkpoints",
                ] {
                    deleted = deleted.saturating_add(self.conn.execute(
                        &format!("DELETE FROM {table} WHERE source_id = ?1"),
                        [&source_id.0],
                    )? as u64);
                }
            }
            Ok(deleted)
        })
    }

    /// Converts historical Codex subscriptions and account-level plan fields synthesized from
    /// local authentication into provider-plan evidence. User-entered billing records are
    /// deliberately left untouched.
    ///
    /// Conversion and retirement happen in one transaction so a failed evidence write can never
    /// discard the legacy record. The deterministic observation ID makes this safe to repeat.
    pub fn migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence(&self) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let subscriptions = self.list_subscriptions()?;
            let mut converted = 0u64;
            for subscription in subscriptions.into_iter().filter(|subscription| {
                subscription.provider.eq_ignore_ascii_case("codex")
                    && subscription.record_source == IdentitySource::LocalAuth
            }) {
                let source_id = SourceId(format!(
                    "legacy_local_auth_{}",
                    &hash_text(&format!(
                        "legacy_local_auth_source.v1:{}:{}",
                        subscription.provider, subscription.provider_account_id.0
                    ))[..32]
                ));
                let observed_at = subscription
                    .verified_at
                    .or(subscription.paid_at)
                    .unwrap_or(subscription.started_at);
                let record_fingerprint = hash_text(&format!(
                    "legacy_local_auth_subscription.v1:{}",
                    subscription.subscription_id.0
                ));
                let observation = AccountPlanObservationV1 {
                    schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                    observation_id: account_plan_observation_id(
                        &source_id,
                        Some(&subscription.provider_account_id),
                        &subscription.plan_name,
                        observed_at,
                        AccountEvidenceKind::LegacyLocalAuth,
                    ),
                    provider: subscription.provider.clone(),
                    source_id,
                    provider_account_id: Some(subscription.provider_account_id.clone()),
                    raw_plan_name: subscription.plan_name.clone(),
                    plan_name: normalize_plan_name(&subscription.plan_name),
                    observed_at,
                    active_from: Some(subscription.started_at),
                    active_until: subscription
                        .current_period_ends_at
                        .or(subscription.ended_at),
                    is_current_snapshot: false,
                    evidence_kind: AccountEvidenceKind::LegacyLocalAuth,
                    confidence: Confidence::Medium,
                    parser_version: "legacy-local-auth-migration.v1".to_string(),
                    artifact_path_hash: hash_text("legacy_local_auth_subscription"),
                    record_fingerprint,
                };
                self.upsert_account_plan_observations(&[observation])?;
                self.delete_subscription(&subscription.subscription_id)?;
                converted += 1;
            }

            let mut preserved_plans = self
                .account_plan_observations()?
                .into_iter()
                .filter(|observation| {
                    observation.evidence_kind == AccountEvidenceKind::LegacyLocalAuth
                })
                .filter_map(|observation| {
                    observation.provider_account_id.map(|account_id| {
                        (account_id, observation.raw_plan_name.trim().to_lowercase())
                    })
                })
                .collect::<HashSet<_>>();
            for mut account in self
                .list_accounts()?
                .into_iter()
                .filter(|account| account.provider.eq_ignore_ascii_case("codex"))
            {
                let Some(raw_plan_name) = account.plan_name.take() else {
                    continue;
                };
                let raw_plan_name = raw_plan_name.trim().to_string();
                let plan_key = (
                    account.provider_account_id.clone(),
                    raw_plan_name.to_lowercase(),
                );
                if !raw_plan_name.is_empty() && !preserved_plans.contains(&plan_key) {
                    let source_id = SourceId(format!(
                        "legacy_local_auth_{}",
                        &hash_text(&format!(
                            "legacy_local_auth_source.v1:{}:{}",
                            account.provider, account.provider_account_id.0
                        ))[..32]
                    ));
                    let observed_at = account.verified_at.unwrap_or(account.updated_at);
                    let observation = AccountPlanObservationV1 {
                        schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                        observation_id: account_plan_observation_id(
                            &source_id,
                            Some(&account.provider_account_id),
                            &raw_plan_name,
                            observed_at,
                            AccountEvidenceKind::LegacyLocalAuth,
                        ),
                        provider: account.provider.clone(),
                        source_id,
                        provider_account_id: Some(account.provider_account_id.clone()),
                        raw_plan_name: raw_plan_name.clone(),
                        plan_name: normalize_plan_name(&raw_plan_name),
                        observed_at,
                        active_from: None,
                        active_until: None,
                        is_current_snapshot: false,
                        evidence_kind: AccountEvidenceKind::LegacyLocalAuth,
                        confidence: Confidence::Medium,
                        parser_version: "legacy-account-plan-migration.v1".to_string(),
                        artifact_path_hash: hash_text("legacy_provider_account_plan"),
                        record_fingerprint: hash_text(&format!(
                            "legacy_provider_account_plan.v1:{}:{}",
                            account.provider_account_id.0, raw_plan_name
                        )),
                    };
                    self.upsert_account_plan_observations(&[observation])?;
                    preserved_plans.insert(plan_key);
                }
                self.upsert_account(&account)?;
            }
            Ok(converted)
        })
    }

    /// Repairs automatic source-account intervals from explicit auth boundaries while keeping
    /// unsupported history unattributed. Manual intervals are immutable here; conflicting direct
    /// evidence remains available for conversation- or turn-specific attribution.
    pub fn reconcile_source_account_evidence_assignments(
        &self,
        source_id: &SourceId,
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let Some(source) = self.source(source_id)? else {
                return Ok(0);
            };
            let mut strong = self
                .account_identity_observations(Some(source_id))?
                .into_iter()
                .filter(|observation| {
                    observation.evidence_kind.is_strong_identity()
                        && observation.provider_account_id.is_some()
                })
                .collect::<Vec<_>>();
            strong.sort_by(|left, right| {
                left.observed_at
                    .cmp(&right.observed_at)
                    .then_with(|| left.observation_id.cmp(&right.observation_id))
            });
            if strong.is_empty() {
                return Ok(0);
            }

            let mut changed = 0u64;
            // A contradictory strong point invalidates generated continuity from that point. The
            // previous account is not resumed unless a later explicit boundary supports it.
            for observation in &strong {
                let account_id = observation
                    .provider_account_id
                    .as_ref()
                    .expect("filtered account identity");
                for mut assignment in self.list_source_account_assignments_for_source(source_id)? {
                    if assignment.record_source == IdentitySource::UserConfigured
                        || !is_verified_source_assignment(&assignment)
                        || assignment.provider_account_id == *account_id
                        || observation.observed_at < assignment.started_at
                        || assignment
                            .ended_at
                            .is_some_and(|ended_at| observation.observed_at >= ended_at)
                    {
                        continue;
                    }
                    if observation.observed_at == assignment.started_at {
                        changed += self
                            .delete_source_account_assignment(&assignment.assignment_id)?
                            as u64;
                    } else {
                        validate_time_window(
                            assignment.started_at,
                            Some(observation.observed_at),
                            "source connection",
                        )?;
                        assignment.ended_at = Some(observation.observed_at);
                        assignment.updated_at = chrono::Utc::now();
                        self.upsert_source_account_assignment(&assignment)?;
                        changed += 1;
                    }
                }
            }

            // A reload is an explicit boundary, but only a later strong point for the same account
            // promotes it into a source interval. Current auth snapshots are handled by the
            // existing verified-auth reconciliation path.
            for (index, boundary) in strong.iter().enumerate().filter(|(_, observation)| {
                observation.evidence_kind == AccountEvidenceKind::AuthReload
            }) {
                let account_id = boundary
                    .provider_account_id
                    .as_ref()
                    .expect("filtered account identity");
                let confirmation = strong[index + 1..].iter().find(|observation| {
                    observation.provider_account_id.as_ref() == Some(account_id)
                });
                let Some(confirmation) = confirmation else {
                    continue;
                };
                let ended_at = strong[index + 1..]
                    .iter()
                    .find(|observation| {
                        observation.provider_account_id.as_ref() != Some(account_id)
                    })
                    .map(|observation| observation.observed_at);
                let assignments = self.list_source_account_assignments_for_source(source_id)?;
                if assignments.iter().any(|assignment| {
                    assignment.record_source == IdentitySource::UserConfigured
                        && periods_overlap(
                            boundary.observed_at,
                            ended_at,
                            assignment.started_at,
                            assignment.ended_at,
                        )
                }) {
                    continue;
                }
                let overlapping_same = assignments.iter().find(|assignment| {
                    assignment.provider_account_id == *account_id
                        && is_verified_source_assignment(assignment)
                        && periods_overlap(
                            boundary.observed_at,
                            ended_at,
                            assignment.started_at,
                            assignment.ended_at,
                        )
                });
                if overlapping_same.is_some_and(|existing| {
                    existing.started_at <= boundary.observed_at
                        && existing.ended_at == ended_at
                        && existing
                            .verified_at
                            .is_some_and(|verified_at| verified_at >= confirmation.observed_at)
                }) {
                    continue;
                }
                let now = chrono::Utc::now();
                let (started_at, merged_end, created_at, previous_id) = overlapping_same.map_or(
                    (boundary.observed_at, ended_at, now, None),
                    |existing| {
                        let merged_end = match (existing.ended_at, ended_at) {
                            (None, _) | (_, None) => None,
                            (Some(left), Some(right)) => Some(left.max(right)),
                        };
                        (
                            existing.started_at.min(boundary.observed_at),
                            merged_end,
                            existing.created_at,
                            Some(existing.assignment_id.clone()),
                        )
                    },
                );
                let assignment = SourceAccountAssignment {
                    schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                    assignment_id: source_account_assignment_id(source_id, account_id, started_at),
                    source_id: source_id.clone(),
                    provider: source.provider.clone(),
                    provider_account_id: account_id.clone(),
                    started_at,
                    ended_at: merged_end,
                    record_source: IdentitySource::LocalAuth,
                    verified_at: Some(confirmation.observed_at),
                    created_at,
                    updated_at: now,
                };
                if let Some(previous_id) =
                    previous_id.filter(|previous_id| *previous_id != assignment.assignment_id)
                {
                    self.delete_source_account_assignment(&previous_id)?;
                }
                self.upsert_source_account_assignment(&assignment)?;
                changed += 1;
            }
            if changed > 0 {
                reattribute_source_records(self, source_id)?;
            }
            Ok(changed)
        })
    }

    /// Convert an attributed quota status into plan evidence. A plan label by itself never
    /// identifies an account, so records without an unambiguous source assignment are skipped.
    pub fn upsert_quota_plan_observations(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        let mut assignments_by_source = HashMap::new();
        let mut observations = Vec::new();
        let mut seen = HashSet::new();
        for record in records {
            let quota = &record.observation;
            let Some(raw_plan_name) = quota
                .status
                .plan_type
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if !assignments_by_source.contains_key(&quota.source_id) {
                assignments_by_source.insert(
                    quota.source_id.clone(),
                    self.list_source_account_assignments_for_source(&quota.source_id)?,
                );
            }
            let provider_account_id = quota.provider_account_id.clone().or_else(|| {
                assignments_by_source
                    .get(&quota.source_id)
                    .and_then(|assignments| {
                        assignment_for_timestamp(assignments, quota.observed_at)
                    })
                    .map(|assignment| assignment.provider_account_id.clone())
            });
            let Some(provider_account_id) = provider_account_id else {
                continue;
            };
            let observation_id = account_plan_observation_id(
                &quota.source_id,
                Some(&provider_account_id),
                raw_plan_name,
                quota.observed_at,
                AccountEvidenceKind::QuotaStatus,
            );
            if !seen.insert(observation_id.clone()) {
                continue;
            }
            observations.push(AccountPlanObservationV1 {
                schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id,
                provider: quota.provider.clone(),
                source_id: quota.source_id.clone(),
                provider_account_id: Some(provider_account_id),
                raw_plan_name: raw_plan_name.to_string(),
                plan_name: normalize_plan_name(raw_plan_name),
                observed_at: quota.observed_at,
                active_from: None,
                active_until: None,
                is_current_snapshot: false,
                evidence_kind: AccountEvidenceKind::QuotaStatus,
                confidence: Confidence::High,
                parser_version: "quota-plan-evidence.v1".to_string(),
                artifact_path_hash: quota.source_file_path_hash.clone(),
                record_fingerprint: quota.semantic_fingerprint.clone(),
            });
        }
        self.upsert_account_plan_observations(&observations)
    }

    /// Rebuilds every plan observation derived from quota status for one source.
    ///
    /// Quota rows are mutable scan projections, while the general plan ledger is append-only.
    /// Removing the old derived subset before recreating it prevents corrected files or changed
    /// account attribution from leaving stale plan/account claims behind.
    pub fn rebuild_quota_plan_observations_for_source(&self, source_id: &SourceId) -> Result<u64> {
        self.with_immediate_transaction(|| {
            self.conn.execute(
                "DELETE FROM account_plan_observations WHERE source_id = ?1 AND evidence_kind = ?2",
                params![
                    &source_id.0,
                    serde_json::to_string(&AccountEvidenceKind::QuotaStatus)?
                ],
            )?;
            let records = self.quota_observations_for_source(source_id)?;
            self.upsert_quota_plan_observations(&records)
        })
    }

    pub fn upsert_account_identity_observations(
        &self,
        observations: &[AccountIdentityObservationV1],
    ) -> Result<u64> {
        if observations.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut written = 0u64;
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO account_identity_observations (
                  observation_id, provider, source_id, provider_account_id, observed_at,
                  evidence_kind, conversation_id_hash, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(observation_id) DO NOTHING
                "#,
            )?;
            for observation in observations {
                written += statement.execute(params![
                    &observation.observation_id,
                    &observation.provider,
                    &observation.source_id.0,
                    observation
                        .provider_account_id
                        .as_ref()
                        .map(|value| value.0.as_str()),
                    observation.observed_at.to_rfc3339(),
                    serde_json::to_string(&observation.evidence_kind)?,
                    observation.conversation_id_hash.as_deref(),
                    serde_json::to_string(observation)?,
                ])? as u64;
            }
            Ok(written)
        })
    }

    pub fn account_identity_observations(
        &self,
        source_id: Option<&SourceId>,
    ) -> Result<Vec<AccountIdentityObservationV1>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload
            FROM account_identity_observations
            WHERE (?1 IS NULL OR source_id = ?1)
            ORDER BY observed_at, observation_id
            "#,
        )?;
        let rows = statement
            .query_map(params![source_id.map(|value| value.0.as_str())], |row| {
                row.get::<_, String>(0)
            })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn upsert_account_plan_observations(
        &self,
        observations: &[AccountPlanObservationV1],
    ) -> Result<u64> {
        if observations.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut written = 0u64;
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO account_plan_observations (
                  observation_id, provider, source_id, provider_account_id, observed_at,
                  active_from, active_until, plan_name, evidence_kind, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(observation_id) DO NOTHING
                "#,
            )?;
            for observation in observations {
                written += statement.execute(params![
                    &observation.observation_id,
                    &observation.provider,
                    &observation.source_id.0,
                    observation
                        .provider_account_id
                        .as_ref()
                        .map(|value| value.0.as_str()),
                    observation.observed_at.to_rfc3339(),
                    observation.active_from.map(|value| value.to_rfc3339()),
                    observation.active_until.map(|value| value.to_rfc3339()),
                    &observation.plan_name,
                    serde_json::to_string(&observation.evidence_kind)?,
                    serde_json::to_string(observation)?,
                ])? as u64;
            }
            Ok(written)
        })
    }

    pub fn account_plan_observations(&self) -> Result<Vec<AccountPlanObservationV1>> {
        let mut statement = self.conn.prepare(
            "SELECT payload FROM account_plan_observations ORDER BY observed_at, observation_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn upsert_conversation_account_bindings(
        &self,
        bindings: &[ConversationAccountBindingV1],
    ) -> Result<u64> {
        if bindings.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut written = 0u64;
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO conversation_account_bindings (
                  binding_id, provider, source_id, provider_account_id,
                  conversation_id_hash, turn_id_hash, observed_at, payload
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(binding_id) DO NOTHING
                "#,
            )?;
            for binding in bindings {
                written += statement.execute(params![
                    &binding.binding_id,
                    &binding.provider,
                    &binding.source_id.0,
                    &binding.provider_account_id.0,
                    &binding.conversation_id_hash,
                    binding.turn_id_hash.as_deref(),
                    binding.observed_at.to_rfc3339(),
                    serde_json::to_string(binding)?,
                ])? as u64;
            }
            Ok(written)
        })
    }

    pub fn conversation_account_bindings(
        &self,
        source_id: Option<&SourceId>,
    ) -> Result<Vec<ConversationAccountBindingV1>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload
            FROM conversation_account_bindings
            WHERE (?1 IS NULL OR source_id = ?1)
            ORDER BY observed_at, binding_id
            "#,
        )?;
        let rows = statement
            .query_map(params![source_id.map(|value| value.0.as_str())], |row| {
                row.get::<_, String>(0)
            })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn apply_conversation_account_bindings(
        &self,
        source_id: &SourceId,
        events: &mut [UsageEvent],
    ) -> Result<()> {
        let bindings = self.conversation_account_bindings(Some(source_id))?;
        let mut accounts_by_conversation: HashMap<&str, HashSet<&ProviderAccountId>> =
            HashMap::new();
        for binding in bindings
            .iter()
            // A turn-scoped reset-history record cannot safely relabel every event in its
            // conversation. Usage events currently expose only conversation identity, so retain
            // that binding as evidence until an exact turn locator is available.
            .filter(|binding| binding.turn_id_hash.is_none())
        {
            accounts_by_conversation
                .entry(binding.conversation_id_hash.as_str())
                .or_default()
                .insert(&binding.provider_account_id);
        }
        for event in events {
            let Some(conversation_id_hash) = event.session.local_session_id_hash.as_deref() else {
                continue;
            };
            let Some(accounts) = accounts_by_conversation.get(conversation_id_hash) else {
                continue;
            };
            if accounts.len() != 1 {
                event.provider_account_id = None;
                continue;
            }
            event.provider_account_id = accounts.iter().next().map(|value| (*value).clone());
            if let Some(parse_evidence) = event.parse_evidence.as_mut() {
                parse_evidence.account_identity_source = IdentitySource::LocalAuth;
            }
        }
        Ok(())
    }

    pub fn reattribute_conversation_bound_events(&self, source_id: &SourceId) -> Result<u64> {
        let mut events = self
            .events()?
            .into_iter()
            .filter(|event| event.source_id == *source_id)
            .collect::<Vec<_>>();
        let previous_accounts = events
            .iter()
            .map(|event| event.provider_account_id.clone())
            .collect::<Vec<_>>();
        self.apply_conversation_account_bindings(source_id, &mut events)?;
        let mut changed = 0u64;
        let mut dirty_keys = BTreeSet::new();
        for (event, previous_account) in events.iter().zip(previous_accounts) {
            if event.provider_account_id == previous_account {
                continue;
            }
            dirty_keys.extend(self.update_event_payload(event)?);
            changed += 1;
        }
        self.refresh_sync_rollups_for_keys(&dirty_keys)?;
        Ok(changed)
    }

    pub fn account_plan_projections(
        &self,
        device_id: &str,
    ) -> Result<Vec<AccountPlanProjectionV1>> {
        Ok(self
            .account_plan_observations()?
            .iter()
            .filter_map(|observation| plan_projection_from_observation(observation, device_id))
            .collect())
    }

    pub fn account_evidence_summaries(
        &self,
        device_id: &str,
    ) -> Result<Vec<AccountEvidenceSummaryV1>> {
        let observations = self.account_identity_observations(None)?;
        let bindings = self.conversation_account_bindings(None)?;
        let mut conversations_by_account = HashMap::<ProviderAccountId, HashSet<String>>::new();
        let mut accounts_by_conversation = HashMap::<String, HashSet<ProviderAccountId>>::new();
        for binding in &bindings {
            conversations_by_account
                .entry(binding.provider_account_id.clone())
                .or_default()
                .insert(binding.conversation_id_hash.clone());
            accounts_by_conversation
                .entry(binding.conversation_id_hash.clone())
                .or_default()
                .insert(binding.provider_account_id.clone());
        }
        let mut conflicting_conversations_by_account = HashMap::<ProviderAccountId, u64>::new();
        for accounts in accounts_by_conversation
            .values()
            .filter(|accounts| accounts.len() > 1)
        {
            for account in accounts {
                *conflicting_conversations_by_account
                    .entry(account.clone())
                    .or_default() += 1;
            }
        }
        let mut assignments_by_source = HashMap::new();
        for observation in &observations {
            if !assignments_by_source.contains_key(&observation.source_id) {
                assignments_by_source.insert(
                    observation.source_id.clone(),
                    self.list_source_account_assignments_for_source(&observation.source_id)?,
                );
            }
        }
        let mut grouped: BTreeMap<(String, ProviderAccountId), Vec<&AccountIdentityObservationV1>> =
            BTreeMap::new();
        for observation in &observations {
            let Some(provider_account_id) = observation.provider_account_id.clone() else {
                continue;
            };
            grouped
                .entry((observation.provider.clone(), provider_account_id))
                .or_default()
                .push(observation);
        }
        let mut summaries = Vec::with_capacity(grouped.len());
        for ((provider, provider_account_id), observations) in grouped {
            let strong = observations
                .iter()
                .filter(|observation| observation.evidence_kind.is_strong_identity())
                .copied()
                .collect::<Vec<_>>();
            let evidence_kinds = observations
                .iter()
                .map(|observation| observation.evidence_kind)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let directly_bound_conversations = conversations_by_account
                .get(&provider_account_id)
                .map_or(0, |conversations| conversations.len() as u64);
            let conflicting_conversations = conflicting_conversations_by_account
                .get(&provider_account_id)
                .copied()
                .unwrap_or_default();
            let mut uncovered_gap_count = 0u64;
            let mut assignment_conflict_count = 0u64;
            let mut previous_uncovered_by_source = HashMap::<&SourceId, bool>::new();
            for observation in &strong {
                let directly_bound = observation.conversation_id_hash.as_deref().is_some_and(
                    |conversation_id_hash| {
                        conversations_by_account
                            .get(&provider_account_id)
                            .is_some_and(|conversations| {
                                conversations.contains(conversation_id_hash)
                            })
                    },
                );
                let assignments = assignments_by_source
                    .get(&observation.source_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let active = assignments
                    .iter()
                    .filter(|assignment| {
                        observation.observed_at >= assignment.started_at
                            && assignment
                                .ended_at
                                .is_none_or(|ended_at| observation.observed_at < ended_at)
                    })
                    .collect::<Vec<_>>();
                let has_matching_assignment = active
                    .iter()
                    .any(|assignment| assignment.provider_account_id == provider_account_id);
                let has_conflicting_assignment = active
                    .iter()
                    .any(|assignment| assignment.provider_account_id != provider_account_id);
                if has_conflicting_assignment && !has_matching_assignment {
                    assignment_conflict_count += 1;
                }
                let uncovered = !directly_bound && !has_matching_assignment;
                let previous_uncovered = previous_uncovered_by_source
                    .insert(&observation.source_id, uncovered)
                    .unwrap_or(false);
                if uncovered && !previous_uncovered {
                    uncovered_gap_count += 1;
                }
            }
            let summary_id = format!(
                "account_evidence_summary_{}",
                &statsai_core::hash_text(&format!(
                    "account_evidence_summary.v1:{device_id}:{provider}:{}",
                    provider_account_id.0
                ))[..32]
            );
            summaries.push(AccountEvidenceSummaryV1 {
                schema_version: ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION.to_string(),
                summary_id,
                device_id: device_id.to_string(),
                provider,
                provider_account_id,
                first_strong_observed_at: strong.iter().map(|value| value.observed_at).min(),
                last_strong_observed_at: strong.iter().map(|value| value.observed_at).max(),
                strong_observation_count: strong.len() as u64,
                directly_bound_conversations,
                uncovered_gap_count,
                conflict_count: conflicting_conversations + assignment_conflict_count,
                evidence_kinds,
            });
        }
        Ok(summaries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use statsai_core::{
        LocationOrigin, SourceLocation, ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION,
        ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION, CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION,
    };
    use std::path::Path;

    #[test]
    fn source_evidence_cleanup_removes_identity_plan_and_conversation_rows() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/account-evidence-cleanup"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let observed_at = Utc::now();
        let account_id = ProviderAccountId("account-cleanup".to_string());
        let identity = AccountIdentityObservationV1 {
            schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "identity-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            provider_user_id_hash: Some("a".repeat(64)),
            email_hash: None,
            conversation_id_hash: Some("b".repeat(64)),
            turn_id_hash: None,
            observed_at,
            evidence_kind: AccountEvidenceKind::TelemetryIdentity,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "d".repeat(64),
        };
        let plan = AccountPlanObservationV1 {
            schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "plan-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            raw_plan_name: "pro".to_string(),
            plan_name: "Pro".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: false,
            evidence_kind: AccountEvidenceKind::QuotaStatus,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "e".repeat(64),
        };
        let binding = ConversationAccountBindingV1 {
            schema_version: CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: "binding-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account_id,
            conversation_id_hash: "b".repeat(64),
            turn_id_hash: None,
            observed_at,
            evidence_kind: AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
        };
        store
            .upsert_account_identity_observations(std::slice::from_ref(&identity))
            .expect("identity evidence");
        store
            .upsert_account_plan_observations(std::slice::from_ref(&plan))
            .expect("plan evidence");
        store
            .upsert_conversation_account_bindings(std::slice::from_ref(&binding))
            .expect("conversation evidence");
        let checkpoint = AccountEvidenceCheckpointV1 {
            schema_version: ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION.to_string(),
            source_id: source.source_id.clone(),
            artifact_path_hash: "f".repeat(64),
            parser_version: "test.v1".to_string(),
            maximum_row_id: 42,
            checkpoint_row_fingerprint: Some("1".repeat(64)),
            database_size: 100,
            database_modified_nanos: 200,
            wal_size: 10,
            wal_modified_nanos: 300,
        };
        store
            .upsert_account_evidence_checkpoints(std::slice::from_ref(&checkpoint))
            .expect("checkpoint");

        let mut identities = vec![identity];
        let mut plans = vec![plan];
        let mut bindings = vec![binding];
        store
            .retain_unseen_account_evidence(
                &source.source_id,
                &mut identities,
                &mut plans,
                &mut bindings,
            )
            .expect("filter known evidence");
        assert!(identities.is_empty() && plans.is_empty() && bindings.is_empty());

        assert_eq!(
            store
                .delete_account_evidence_for_sources(std::slice::from_ref(&source.source_id))
                .expect("delete source evidence"),
            4
        );
        assert!(store
            .account_identity_observations(Some(&source.source_id))
            .expect("identities")
            .is_empty());
        assert!(store.account_plan_observations().expect("plans").is_empty());
        assert!(store
            .conversation_account_bindings(Some(&source.source_id))
            .expect("bindings")
            .is_empty());
        assert!(store
            .account_evidence_checkpoints(&source.source_id)
            .expect("checkpoints")
            .is_empty());
    }

    #[test]
    fn account_evidence_checkpoint_persists_and_rolls_back_transactionally() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("statsai.sqlite");
        let source_id = SourceId("checkpoint-source".to_string());
        let checkpoint = AccountEvidenceCheckpointV1 {
            schema_version: ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION.to_string(),
            source_id: source_id.clone(),
            artifact_path_hash: "a".repeat(64),
            parser_version: "test.v1".to_string(),
            maximum_row_id: 84,
            checkpoint_row_fingerprint: Some("2".repeat(64)),
            database_size: 200,
            database_modified_nanos: 300,
            wal_size: 20,
            wal_modified_nanos: 400,
        };
        {
            let store = Store::open(&path).expect("open store");
            store
                .upsert_account_evidence_checkpoints(std::slice::from_ref(&checkpoint))
                .expect("persist checkpoint");
        }
        let store = Store::open(&path).expect("reopen store");
        assert_eq!(
            store
                .account_evidence_checkpoints(&source_id)
                .expect("load checkpoint"),
            vec![checkpoint.clone()]
        );

        let replacement = AccountEvidenceCheckpointV1 {
            maximum_row_id: 100,
            ..checkpoint.clone()
        };
        let error = store
            .apply_scan_update(|store| -> Result<()> {
                store.upsert_account_evidence_checkpoints(std::slice::from_ref(&replacement))?;
                anyhow::bail!("force rollback")
            })
            .expect_err("transaction must fail");
        assert_eq!(error.to_string(), "force rollback");
        assert_eq!(
            store
                .account_evidence_checkpoints(&source_id)
                .expect("load checkpoint after rollback"),
            vec![checkpoint]
        );
    }
}
