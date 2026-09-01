use super::*;

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

    /// Reads the subscriptions this one-shot conversion can act on, skipping rows it cannot parse.
    ///
    /// This runs inside `Store::migrate`, so an error here fails `Store::open` for good: the
    /// transaction rolls back, the completion flag is never written, and every later launch
    /// retries the same row and fails again. One payload written by a newer build, or corrupted
    /// on disk, is not a reason to make the store unopenable — and an unparsable record could
    /// not have been converted anyway. It stays where it is, still visible as a legacy row.
    fn subscriptions_for_legacy_plan_migration(&self) -> Result<Vec<statsai_core::Subscription>> {
        let mut statement = self.conn.prepare(
            "SELECT payload, provider_account_id FROM subscriptions ORDER BY provider, subscription_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut subscriptions = Vec::new();
        for row in rows {
            let (payload, provider_account_id) = row?;
            if let Ok(subscription) =
                super::deserialize_subscription_payload(&payload, provider_account_id.as_deref())
            {
                subscriptions.push(subscription);
            }
        }
        Ok(subscriptions)
    }

    /// The account counterpart of [`Self::subscriptions_for_legacy_plan_migration`], and skipping
    /// for the same reason.
    fn accounts_for_legacy_plan_migration(&self) -> Result<Vec<statsai_core::ProviderAccount>> {
        let mut statement = self.conn.prepare(
            "SELECT payload FROM provider_accounts ORDER BY provider, provider_account_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut accounts = Vec::new();
        for row in rows {
            if let Ok(account) = serde_json::from_str(&row?) {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    /// Converts historical Codex subscriptions and account-level plan fields synthesized from
    /// local authentication into provider-plan evidence. User-entered billing records are
    /// deliberately left untouched.
    ///
    /// Conversion and retirement happen in one transaction so a failed evidence write can never
    /// discard the legacy record. The deterministic observation ID makes this safe to repeat.
    pub fn migrate_legacy_codex_local_auth_subscriptions_to_plan_evidence(&self) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let subscriptions = self.subscriptions_for_legacy_plan_migration()?;
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
                .accounts_for_legacy_plan_migration()?
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
            // previous account is not resumed unless a later explicit boundary supports it, so
            // only evidence that could supply such a boundary is allowed to end an interval.
            for observation in strong
                .iter()
                .filter(|observation| observation.evidence_kind.ends_source_attribution())
            {
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
            //
            // Both searches below read source-wide evidence only, for the same reason the
            // truncation above does: a turn-scoped reset-history entry neither proves the source
            // was signed in as that account nor, naming another, that it stopped being.
            for (index, boundary) in strong.iter().enumerate().filter(|(_, observation)| {
                observation.evidence_kind == AccountEvidenceKind::AuthReload
            }) {
                let account_id = boundary
                    .provider_account_id
                    .as_ref()
                    .expect("filtered account identity");
                let mut confirmation = None;
                let mut ended_at = None;
                for observation in strong[index + 1..]
                    .iter()
                    .filter(|observation| observation.evidence_kind.ends_source_attribution())
                {
                    if observation.provider_account_id.as_ref() == Some(account_id) {
                        confirmation = Some(observation);
                    } else {
                        ended_at = Some(observation.observed_at);
                        break;
                    }
                }
                let Some(confirmation) = confirmation else {
                    continue;
                };
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
}
