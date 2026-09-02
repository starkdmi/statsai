use super::*;

impl Store {
    /// Convert an attributed quota status into plan evidence. A plan label by itself never
    /// identifies an account, so records without an unambiguous source assignment are skipped.
    ///
    /// Quota rows arrive one per provider response, and a plan label barely ever changes between
    /// them: ten thousand rows saying "Pro" are one fact, not ten thousand. Keying each row's own
    /// `observed_at` made them ten thousand ledger rows, ten thousand `entity_requires_sync`
    /// queries, and a snapshot split across fifty-odd fragments. Consecutive rows carrying the
    /// same label for the same account therefore collapse into one observation spanning the run,
    /// timed at its most recent evidence and identified by its first, so that history stays put
    /// and only the open run re-syncs as it grows.
    pub fn upsert_quota_plan_observations(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        struct PlanRun<'a> {
            quota: &'a statsai_core::QuotaObservationV1,
            provider_account_id: ProviderAccountId,
            raw_plan_name: &'a str,
            started_at: chrono::DateTime<chrono::Utc>,
        }

        let mut assignments_by_source = HashMap::new();
        let mut attributed: Vec<(ProviderAccountId, &str, &statsai_core::QuotaObservationV1)> =
            Vec::new();
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
            attributed.push((provider_account_id, raw_plan_name, quota));
        }
        // Records reach here in scan order, which is not observation order once a
        // rotated file or a re-import interleaves them. A run is only meaningful
        // along the timeline, so sort before collapsing.
        attributed.sort_by(|left, right| {
            left.2
                .source_id
                .0
                .cmp(&right.2.source_id.0)
                .then_with(|| left.2.observed_at.cmp(&right.2.observed_at))
                .then_with(|| left.2.observation_id.cmp(&right.2.observation_id))
        });

        let mut runs: Vec<PlanRun<'_>> = Vec::new();
        for (provider_account_id, raw_plan_name, quota) in attributed {
            let continues_run = runs.last().is_some_and(|run| {
                run.quota.source_id == quota.source_id
                    && run.provider_account_id == provider_account_id
                    && run.raw_plan_name.eq_ignore_ascii_case(raw_plan_name)
            });
            if continues_run {
                let run = runs.last_mut().expect("checked above");
                run.quota = quota;
                run.raw_plan_name = raw_plan_name;
                continue;
            }
            runs.push(PlanRun {
                quota,
                provider_account_id,
                raw_plan_name,
                started_at: quota.observed_at,
            });
        }

        let observations = runs
            .into_iter()
            .map(|run| {
                let quota = run.quota;
                AccountPlanObservationV1 {
                    schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                    // Identified by where the run began, so extending it does not
                    // mint a new row and retire the old one on every scan.
                    observation_id: account_plan_observation_id(
                        &quota.source_id,
                        Some(&run.provider_account_id),
                        run.raw_plan_name,
                        &normalize_plan_name(run.raw_plan_name),
                        run.started_at,
                        AccountEvidenceKind::QuotaStatus,
                    ),
                    provider: quota.provider.clone(),
                    source_id: quota.source_id.clone(),
                    provider_account_id: Some(run.provider_account_id),
                    raw_plan_name: run.raw_plan_name.to_string(),
                    plan_name: normalize_plan_name(run.raw_plan_name),
                    observed_at: quota.observed_at,
                    // No period: the provider reported a plan while serving a
                    // request, it did not declare a billing window. It did
                    // report it *as of that moment*, though, which is what
                    // `is_current_snapshot` means -- and it is fresher than
                    // `auth.json`, which can sit on disk unchanged for weeks.
                    // Leaving this false meant an account whose logs say "plus"
                    // every day still read as `last_detected` as soon as its
                    // last declared provider period ran out.
                    active_from: None,
                    active_until: None,
                    is_current_snapshot: true,
                    evidence_kind: AccountEvidenceKind::QuotaStatus,
                    confidence: Confidence::High,
                    parser_version: "quota-plan-evidence.v1".to_string(),
                    artifact_path_hash: quota.source_file_path_hash.clone(),
                    record_fingerprint: quota.semantic_fingerprint.clone(),
                }
            })
            .collect::<Vec<_>>();
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
        let mut ordered: Vec<&AccountIdentityObservationV1> = observations.iter().collect();
        ordered.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.observation_id.cmp(&right.observation_id))
        });
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
            for observation in ordered {
                // Adapters collapse a run of same-identity telemetry/reload rows
                // to its endpoints, but an incremental scan only sees rows past
                // its checkpoint: each daemon pass would append a fresh pair and
                // the ledger would still grow with every scan. When the newest
                // persisted rows for the source already form a run of this
                // observation's identity, the incoming row is the run's new last
                // point, so it replaces the persisted endpoint instead of
                // stacking behind it. The run's first row is never touched and
                // an alternation always has a differing newest row, so every
                // switch survives.
                if let Some(superseded) = self.identity_run_endpoint_superseded_by(observation)? {
                    self.conn.execute(
                        "DELETE FROM account_identity_observations WHERE observation_id = ?1",
                        [&superseded],
                    )?;
                }
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

    /// Returns the persisted run endpoint an incoming observation supersedes, if any.
    ///
    /// The two newest persisted observations for the source must both continue the
    /// incoming observation's run — same collapsible kind, account, and email — and
    /// must not be newer than it. Requiring two matching rows keeps a run's first
    /// point in place, and any interleaved different identity breaks the match, so
    /// alternations are never collapsed away.
    fn identity_run_endpoint_superseded_by(
        &self,
        observation: &AccountIdentityObservationV1,
    ) -> Result<Option<String>> {
        if !matches!(
            observation.evidence_kind,
            AccountEvidenceKind::TelemetryIdentity | AccountEvidenceKind::AuthReload
        ) {
            return Ok(None);
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_identity_observations WHERE observation_id = ?1)",
            [&observation.observation_id],
            |row| row.get(0),
        )?;
        if exists {
            // A replayed row (full rescan) is already an endpoint; treating it
            // as a continuation would delete the endpoint it duplicates.
            return Ok(None);
        }
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload FROM account_identity_observations
            WHERE source_id = ?1
            ORDER BY observed_at DESC, observation_id DESC
            LIMIT 2
            "#,
        )?;
        let newest = statement
            .query_map([&observation.source_id.0], |row| row.get::<_, String>(0))?
            .map(|payload| {
                Ok(serde_json::from_str::<AccountIdentityObservationV1>(
                    &payload?,
                )?)
            })
            .collect::<Result<Vec<_>>>()?;
        let continues_run = newest.len() == 2
            && newest.iter().all(|persisted| {
                persisted.evidence_kind == observation.evidence_kind
                    && persisted.provider_account_id == observation.provider_account_id
                    && persisted.email_hash == observation.email_hash
                    && persisted.observed_at <= observation.observed_at
            });
        Ok(continues_run.then(|| newest[0].observation_id.clone()))
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

    /// Plan observations oldest first, ties broken by when they were collected.
    ///
    /// Two observations can share an `observed_at`: one cached artifact can be
    /// re-read after the provider revised what it claims for that same moment,
    /// which is exactly what the canonical plan in `account_plan_observation_id`
    /// keeps from being discarded. Ordering those ties by `observation_id` sorted
    /// them by hash, so the stale claim could outrank the corrected one and be
    /// reported as the latest plan. `rowid` is insertion order, so the row read
    /// most recently is the one that wins.
    pub fn account_plan_observations(&self) -> Result<Vec<AccountPlanObservationV1>> {
        let mut statement = self
            .conn
            .prepare("SELECT payload FROM account_plan_observations ORDER BY observed_at, rowid")?;
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
                // Conflicting bindings are weaker evidence than a person telling
                // us who owns this source. Clearing the account here dropped a
                // `UserConfigured` assignment and left `account_identity_source`
                // still naming it, so the event claimed a manual attribution to
                // an account that was no longer on it. Leave the event as it is
                // and let the conflict stay visible in the evidence summary.
                if !matches!(
                    event
                        .parse_evidence
                        .as_ref()
                        .map(|evidence| &evidence.account_identity_source),
                    Some(IdentitySource::UserConfigured)
                ) {
                    event.provider_account_id = None;
                }
                continue;
            }
            // A manual assignment also outranks a single agreeing binding: it is
            // the only source the user can correct by hand.
            if matches!(
                event
                    .parse_evidence
                    .as_ref()
                    .map(|evidence| &evidence.account_identity_source),
                Some(IdentitySource::UserConfigured)
            ) {
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
        // Nothing to reattribute without bindings, and this is the common case:
        // every scan of every Auto-verified source reached here. Checking first
        // avoids loading and deserializing the source's events for no reason.
        if self
            .conversation_account_bindings(Some(source_id))?
            .is_empty()
        {
            return Ok(0);
        }
        // `events()` reads every row in `usage_events` and deserializes each
        // payload before this filter sees it. On a store with hundreds of
        // thousands of events that is the whole table parsed to keep one
        // source's share; `events_for_source` pushes the same filter into SQL.
        let mut events = self.events_for_source(source_id)?;
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
