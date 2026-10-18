use super::*;

impl Store {
    /// Writes observations without collecting payloads, which callers must arrange for.
    ///
    /// An observation is keyed by its file and line rather than its contents, so upserting
    /// a corrected record swaps its `payload_hash` and can leave the payload it used to
    /// point at with no referent. Nothing here notices that. Every current caller reaches
    /// this having already deleted the observations it is about to write --
    /// `delete_quota_observations_for_sources` or
    /// `delete_quota_observations_for_source_file_hashes`, both of which collect what they
    /// orphan -- so the upsert only ever inserts, and there is no stale hash to abandon.
    ///
    /// A caller that upserts over rows it did not first delete needs
    /// [`Store::replace_quota_observations_for_source_files`] instead, which reconciles and
    /// collects in one step.
    pub fn upsert_quota_observations(&self, records: &[QuotaObservationRecordV1]) -> Result<u64> {
        self.with_immediate_transaction(|| self.upsert_quota_observations_inner(records))
    }

    pub(crate) fn upsert_quota_observations_inner(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        Ok(self.upsert_quota_observations_counting(records)?.written)
    }

    /// [`Store::upsert_quota_observations_inner`], also counting the records whose
    /// stored row or windows it changed.
    fn upsert_quota_observations_counting(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<QuotaWriteCount> {
        let mut written = 0u64;
        let mut changed = 0u64;
        let mut assignments_by_source = HashMap::new();
        for record in records {
            let source_id = &record.observation.source_id;
            if !assignments_by_source.contains_key(source_id) {
                assignments_by_source.insert(
                    source_id.clone(),
                    self.list_source_account_assignments_for_source(source_id)?,
                );
            }
            let mut record = record.clone();
            let assignments = assignments_by_source
                .get(source_id)
                .expect("source assignments inserted above");
            record.observation.provider_account_id =
                assignment_for_timestamp(assignments, record.observation.observed_at)
                    .map(|assignment| assignment.provider_account_id.clone());

            if record.observation.usage_event_id.is_none() {
                let existing = self
                        .conn
                        .query_row(
                            "SELECT usage_event_id, usage_link_kind, payload FROM quota_observations WHERE observation_id = ?1",
                            [&record.observation.observation_id],
                            |row| {
                                Ok((
                                    row.get::<_, Option<String>>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()?;
                if let Some((Some(event_id), link_kind, payload)) = existing {
                    let existing_observation: QuotaObservationV1 = serde_json::from_str(&payload)?;
                    if matching_positive_usage_sample(&record.observation, &existing_observation) {
                        record.observation.usage_event_id = Some(EventId(event_id));
                        record.observation.usage_link_kind = parse_usage_link_kind(&link_kind);
                    }
                }
            }

            self.conn.execute(
                r#"
                    INSERT OR IGNORE INTO quota_payloads
                      (payload_hash, provider, payload, created_at)
                    VALUES (?1, ?2, ?3, ?4)
                    "#,
                params![
                    &record.observation.payload_hash,
                    &record.observation.provider,
                    serde_json::to_string(&record.raw_rate_limits)?,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            let observation_payload = serde_json::to_string(&record.observation)?;
            let observation_changed = self.conn.execute(
                r#"
                    INSERT INTO quota_observations (
                      observation_id, semantic_fingerprint, provider, source_id,
                      provider_account_id, observed_at, source_file_path_hash,
                      source_record_id, usage_event_id, usage_link_kind, payload_hash, payload
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    ON CONFLICT(observation_id) DO UPDATE SET
                      semantic_fingerprint = excluded.semantic_fingerprint,
                      provider = excluded.provider,
                      source_id = excluded.source_id,
                      provider_account_id = excluded.provider_account_id,
                      observed_at = excluded.observed_at,
                      source_file_path_hash = excluded.source_file_path_hash,
                      source_record_id = excluded.source_record_id,
                      usage_event_id = excluded.usage_event_id,
                      usage_link_kind = excluded.usage_link_kind,
                      payload_hash = excluded.payload_hash,
                      payload = excluded.payload
                    WHERE quota_observations.semantic_fingerprint IS NOT excluded.semantic_fingerprint
                       OR quota_observations.provider IS NOT excluded.provider
                       OR quota_observations.source_id IS NOT excluded.source_id
                       OR quota_observations.provider_account_id IS NOT excluded.provider_account_id
                       OR quota_observations.observed_at IS NOT excluded.observed_at
                       OR quota_observations.source_file_path_hash IS NOT excluded.source_file_path_hash
                       OR quota_observations.source_record_id IS NOT excluded.source_record_id
                       OR quota_observations.usage_event_id IS NOT excluded.usage_event_id
                       OR quota_observations.usage_link_kind IS NOT excluded.usage_link_kind
                       OR quota_observations.payload_hash IS NOT excluded.payload_hash
                       OR quota_observations.payload IS NOT excluded.payload
                    "#,
                params![
                    &record.observation.observation_id,
                    &record.observation.semantic_fingerprint,
                    &record.observation.provider,
                    &record.observation.source_id.0,
                    record
                        .observation
                        .provider_account_id
                        .as_ref()
                        .map(|id| id.0.as_str()),
                    record.observation.observed_at.to_rfc3339(),
                    &record.observation.source_file_path_hash,
                    &record.observation.source_record_id,
                    record
                        .observation
                        .usage_event_id
                        .as_ref()
                        .map(|id| id.0.as_str()),
                    usage_link_kind_label(record.observation.usage_link_kind),
                    &record.observation.payload_hash,
                    observation_payload,
                ],
            )? > 0;
            let mut incoming_windows = record
                .windows
                .iter()
                .map(|window| Ok((window, serde_json::to_string(window)?)))
                .collect::<Result<Vec<_>>>()?;
            incoming_windows.sort_by_key(|(window, _)| window.window_observation_id.as_str());
            let stored_windows = if observation_changed {
                Vec::new()
            } else {
                let mut statement = self.conn.prepare(
                    "SELECT window_observation_id, payload
                     FROM quota_window_observations
                     WHERE observation_id = ?1
                     ORDER BY window_observation_id",
                )?;
                let windows = statement
                    .query_map([&record.observation.observation_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                windows
            };
            let windows_changed = observation_changed
                || incoming_windows.len() != stored_windows.len()
                || incoming_windows.iter().zip(&stored_windows).any(
                    |((incoming_window, incoming_payload), (stored_id, stored_payload))| {
                        incoming_window.window_observation_id != *stored_id
                            || incoming_payload != stored_payload
                    },
                );
            if windows_changed {
                changed = changed.saturating_add(1);
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&record.observation.observation_id],
                )?;
                for (window, payload) in &incoming_windows {
                    self.conn.execute(
                        r#"
                        INSERT INTO quota_window_observations (
                          window_observation_id, observation_id, provider_slot, limit_id,
                          window_minutes, used_percent, resets_at, payload
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        "#,
                        params![
                            &window.window_observation_id,
                            &window.observation_id,
                            &window.provider_slot,
                            window.limit_id.as_deref(),
                            i64::try_from(window.window_minutes).unwrap_or(i64::MAX),
                            window.used_percent,
                            window.resets_at_epoch_seconds,
                            payload,
                        ],
                    )?;
                }
            }
            written = written.saturating_add(1);
        }
        Ok(QuotaWriteCount { written, changed })
    }

    pub(crate) fn replace_quota_observations_for_source_files_inner(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<QuotaWriteCount> {
        let source_file_path_hashes = source_file_path_hashes
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let retained_observation_ids = records
            .iter()
            .filter(|record| {
                record.observation.source_id == *source_id
                    && source_file_path_hashes
                        .contains(record.observation.source_file_path_hash.as_str())
            })
            .map(|record| record.observation.observation_id.as_str())
            .collect::<HashSet<_>>();

        let mut orphaned_payload_hashes = BTreeSet::new();
        let mut deleted = 0u64;
        for source_file_path_hash in source_file_path_hashes {
            let mut statement = self.conn.prepare(
                "SELECT observation_id, payload_hash FROM quota_observations
                 WHERE source_id = ?1 AND source_file_path_hash = ?2",
            )?;
            let stored_observations = statement
                .query_map(params![&source_id.0, source_file_path_hash], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            for (observation_id, payload_hash) in stored_observations {
                // Every stored hash is a candidate, retained rows included. An
                // observation is identified by its file and line, not by its contents,
                // so a corrected file keeps the same `observation_id` while the upsert
                // swaps its `payload_hash` -- and the payload it pointed at before can
                // be left with no referent at all. The check after the upsert is what
                // decides; this only has to avoid overlooking one.
                orphaned_payload_hashes.insert(payload_hash);
                if retained_observation_ids.contains(observation_id.as_str()) {
                    continue;
                }
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )? as u64);
            }
        }

        let mut count = self.upsert_quota_observations_counting(records)?;
        count.changed = count.changed.saturating_add(deleted);
        // After the upsert, so a payload the incoming records reuse is seen as still
        // referenced rather than deleted and immediately rewritten.
        self.delete_unreferenced_quota_payloads_for_hashes(&orphaned_payload_hashes)?;
        Ok(count)
    }

    pub fn replace_quota_observations_for_source_files(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            Ok(self
                .replace_quota_observations_for_source_files_inner(
                    source_id,
                    source_file_path_hashes,
                    records,
                )?
                .written)
        })
    }

    pub fn quota_observations(
        &self,
        query: &QuotaQuery,
        collapse_duplicates: bool,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        let mut observations = BTreeMap::<String, QuotaObservationRecordV1>::new();
        let observation_sql = if query.source_id.is_some() {
            r#"
            SELECT q.payload, q.provider_account_id, q.usage_event_id, q.usage_link_kind,
                   p.payload
            FROM quota_observations q
            JOIN quota_payloads p ON p.payload_hash = q.payload_hash
            WHERE q.source_id = ?1
            ORDER BY q.observed_at, q.observation_id
            "#
        } else {
            r#"
            SELECT q.payload, q.provider_account_id, q.usage_event_id, q.usage_link_kind,
                   p.payload
            FROM quota_observations q
            JOIN quota_payloads p ON p.payload_hash = q.payload_hash
            ORDER BY q.observed_at, q.observation_id
            "#
        };
        let source_bindings = query
            .source_id
            .as_ref()
            .map(|source_id| source_id.0.as_str())
            .into_iter();
        let mut statement = self.conn.prepare(observation_sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(source_bindings))?;
        while let Some(row) = rows.next()? {
            let payload = row.get::<_, String>(0)?;
            let account_id = row.get::<_, Option<String>>(1)?;
            let usage_event_id = row.get::<_, Option<String>>(2)?;
            let usage_link_kind = row.get::<_, String>(3)?;
            let raw_payload = row.get::<_, String>(4)?;
            let mut observation: QuotaObservationV1 = serde_json::from_str(&payload)?;
            observation.provider_account_id = account_id.map(ProviderAccountId);
            observation.usage_event_id = usage_event_id.map(EventId);
            observation.usage_link_kind = parse_usage_link_kind(&usage_link_kind);
            if !observation_matches_query(&observation, query) {
                continue;
            }
            observations.insert(
                observation.observation_id.clone(),
                QuotaObservationRecordV1 {
                    observation,
                    windows: Vec::new(),
                    raw_rate_limits: serde_json::from_str(&raw_payload)?,
                },
            );
        }
        drop(rows);
        drop(statement);

        let window_sql = if query.source_id.is_some() {
            r#"
            SELECT window.observation_id, window.payload
            FROM quota_observations observation INDEXED BY quota_observations_source_idx
            CROSS JOIN quota_window_observations window
              ON window.observation_id = observation.observation_id
            WHERE observation.source_id = ?1
            ORDER BY window.resets_at, window.window_observation_id
            "#
        } else {
            "SELECT observation_id, payload FROM quota_window_observations ORDER BY resets_at, window_observation_id"
        };
        let source_bindings = query
            .source_id
            .as_ref()
            .map(|source_id| source_id.0.as_str())
            .into_iter();
        let mut statement = self.conn.prepare(window_sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(source_bindings))?;
        while let Some(row) = rows.next()? {
            let observation_id = row.get::<_, String>(0)?;
            let payload = row.get::<_, String>(1)?;
            let Some(record) = observations.get_mut(&observation_id) else {
                continue;
            };
            let window: QuotaWindowObservationV1 = serde_json::from_str(&payload)?;
            if query
                .limit_id
                .as_deref()
                .is_some_and(|limit| window.limit_id.as_deref() != Some(limit))
            {
                continue;
            }
            record.windows.push(window);
        }
        let mut records = observations.into_values().collect::<Vec<_>>();
        records.sort_by(compare_observation_records);
        Ok(if collapse_duplicates {
            collapse_semantic_duplicates(records)
        } else {
            records
        })
    }

    /// The collapsed, attributed observations weekly cycle contributions are built
    /// from, each with its weekly windows only.
    ///
    /// The result is what `quota_observations(query, true)` returns, less the
    /// observations with no provider account and every window shorter than a week,
    /// and with the fields contributions never read -- status, usage sample, line
    /// number, source, raw rate limits -- left empty. Neither omission can change a
    /// contribution: reconstruction scopes never mix window lengths, a contribution
    /// needs an account, and an unattributed copy never outranks an attributed one
    /// when duplicates collapse.
    ///
    /// What it saves is the reading. [`Store::quota_observations`] scans the
    /// observation table, whose rows are mostly their JSON, and parses each
    /// observation's JSON, its raw rate-limit payload and every window's JSON.
    /// This reads the observations from two covering indexes,
    /// `quota_observations_scope_idx` and `quota_observations_semantic_idx`, and the
    /// windows in one sequential scan of their columns, with no JSON at all.
    ///
    /// Duplicates still collapse by the same ranking: windows of every length are
    /// counted, and linked usage is read from `quota_observations_usage_event_idx`.
    pub(crate) fn attributed_weekly_quota_observations(
        &self,
        query: &QuotaQuery,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        if query.source_id.is_some() {
            // Only the CLI scopes contributions to a source, and the source index
            // already narrows the full reader to that source's rows.
            let mut records = self.quota_observations(query, true)?;
            records.retain_mut(|record| {
                record
                    .windows
                    .retain(|window| window.window_minutes == QUOTA_WEEKLY_WINDOW_MINUTES);
                record.observation.provider_account_id.is_some() && !record.windows.is_empty()
            });
            return Ok(records);
        }

        let mut sql = String::from(
            "SELECT rowid, observation_id, provider, provider_account_id, observed_at
             FROM quota_observations
             WHERE provider_account_id IS NOT NULL",
        );
        let mut bindings = Vec::<&str>::new();
        if let Some(provider) = query.provider.as_deref() {
            bindings.push(provider);
            sql.push_str(&format!(" AND provider = ?{}", bindings.len()));
        }
        if let Some(account_id) = query.provider_account_id.as_ref() {
            bindings.push(&account_id.0);
            sql.push_str(&format!(" AND provider_account_id = ?{}", bindings.len()));
        }
        let mut ids = Vec::new();
        let mut scoped = Vec::new();
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(bindings))?;
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, String>(1)?);
            scoped.push(ScopedObservation {
                rowid: row.get(0)?,
                provider: row.get(2)?,
                provider_account_id: ProviderAccountId(row.get(3)?),
                observed_at: crate::parse_rfc3339_for_row(row.get_ref(4)?.as_str()?, 4)?,
            });
        }
        drop(rows);
        drop(statement);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let position = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<HashMap<_, _>>();

        // Every window is counted, as the collapse ranking counts them; only the
        // weekly ones are kept. A sequential scan reads fewer pages than looking
        // each weekly row up from `quota_window_observations_reconstruct_idx`.
        let mut windows = vec![Vec::<QuotaWindowObservationV1>::new(); ids.len()];
        let mut window_counts = vec![0usize; ids.len()];
        let window_sql = if query.limit_id.is_some() {
            "SELECT window_observation_id, observation_id, provider_slot, limit_id,
                    window_minutes, used_percent, resets_at
             FROM quota_window_observations
             WHERE limit_id = ?1"
        } else {
            "SELECT window_observation_id, observation_id, provider_slot, limit_id,
                    window_minutes, used_percent, resets_at
             FROM quota_window_observations"
        };
        let mut statement = self.conn.prepare(window_sql)?;
        let mut rows = match query.limit_id.as_deref() {
            Some(limit_id) => statement.query(params![limit_id])?,
            None => statement.query([])?,
        };
        while let Some(row) = rows.next()? {
            let Some(&index) = position.get(row.get_ref(1)?.as_str()?) else {
                continue;
            };
            window_counts[index] += 1;
            if row.get::<_, i64>(4)? != QUOTA_WEEKLY_WINDOW_MINUTES as i64 {
                continue;
            }
            let resets_at_epoch_seconds = row.get::<_, i64>(6)?;
            let resets_at = DateTime::from_timestamp(resets_at_epoch_seconds, 0).ok_or(
                rusqlite::Error::IntegralValueOutOfRange(6, resets_at_epoch_seconds),
            )?;
            windows[index].push(QuotaWindowObservationV1 {
                schema_version: String::new(),
                window_observation_id: row.get(0)?,
                observation_id: ids[index].clone(),
                provider_slot: row.get(2)?,
                limit_id: row.get(3)?,
                window_minutes: QUOTA_WEEKLY_WINDOW_MINUTES,
                used_percent: used_percent_as_parsed(row.get(5)?),
                resets_at,
                resets_at_epoch_seconds,
            });
        }
        drop(rows);
        drop(statement);

        let superseded =
            self.superseded_semantic_duplicates(&ids, &scoped, &position, |index| {
                window_counts[index]
            })?;

        let mut kept = (0..ids.len())
            .filter(|&index| !superseded.contains(&index) && !windows[index].is_empty())
            .collect::<Vec<_>>();
        kept.sort_by(|&left, &right| {
            scoped[left]
                .observed_at
                .cmp(&scoped[right].observed_at)
                .then_with(|| ids[left].cmp(&ids[right]))
        });
        let mut ids = ids.into_iter().map(Some).collect::<Vec<_>>();
        let mut scoped = scoped.into_iter().map(Some).collect::<Vec<_>>();
        Ok(kept
            .into_iter()
            .map(|index| {
                let observation_id = ids[index].take().expect("each observation is kept once");
                let scoped = scoped[index].take().expect("each observation is kept once");
                let mut windows = std::mem::take(&mut windows[index]);
                // The order `quota_observations` loads them in.
                windows.sort_by(|left, right| {
                    left.resets_at_epoch_seconds
                        .cmp(&right.resets_at_epoch_seconds)
                        .then_with(|| left.window_observation_id.cmp(&right.window_observation_id))
                });
                QuotaObservationRecordV1 {
                    observation: scoped.into_observation(observation_id),
                    windows,
                    raw_rate_limits: serde_json::Value::Null,
                }
            })
            .collect())
    }

    /// The positions, among `ids`, of observations that a semantic duplicate
    /// outranks, decided exactly as [`collapse_semantic_duplicates`] decides it.
    ///
    /// `quota_observations_semantic_idx` lists fingerprints in order, so copies
    /// sit next to each other and only they are ranked.
    fn superseded_semantic_duplicates(
        &self,
        ids: &[String],
        scoped: &[ScopedObservation],
        position: &HashMap<&str, usize>,
        window_count: impl Fn(usize) -> usize,
    ) -> Result<HashSet<usize>> {
        let mut duplicates = Vec::<(usize, String)>::new();
        let mut run = Vec::<usize>::new();
        let mut run_fingerprint = String::new();
        let mut statement = self.conn.prepare(
            "SELECT semantic_fingerprint, observation_id FROM quota_observations
             ORDER BY semantic_fingerprint",
        )?;
        let mut rows = statement.query([])?;
        loop {
            let row = rows.next()?;
            let fingerprint = match row {
                Some(row) => Some(row.get_ref(0)?.as_str()?),
                None => None,
            };
            if fingerprint != Some(run_fingerprint.as_str()) {
                if run.len() > 1 {
                    duplicates.extend(run.iter().map(|&index| (index, run_fingerprint.clone())));
                }
                run.clear();
                run_fingerprint.clear();
                run_fingerprint.push_str(fingerprint.unwrap_or_default());
            }
            let Some(row) = row else { break };
            if let Some(&index) = position.get(row.get_ref(1)?.as_str()?) {
                run.push(index);
            }
        }
        drop(rows);
        drop(statement);
        if duplicates.is_empty() {
            return Ok(HashSet::new());
        }

        let mut linked = HashSet::new();
        let mut statement = self
            .conn
            .prepare("SELECT rowid FROM quota_observations WHERE usage_event_id IS NOT NULL")?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        for rowid in rows {
            linked.insert(rowid?);
        }

        let mut records = duplicates
            .into_iter()
            .map(|(index, fingerprint)| {
                let mut observation = scoped[index].clone().into_observation(ids[index].clone());
                observation.semantic_fingerprint = fingerprint;
                observation.usage_event_id = linked
                    .contains(&scoped[index].rowid)
                    .then(|| EventId(String::new()));
                QuotaObservationRecordV1 {
                    observation,
                    windows: Vec::new(),
                    raw_rate_limits: serde_json::Value::Null,
                }
            })
            .collect::<Vec<_>>();
        records.sort_by(compare_observation_records);
        let candidates = records
            .iter()
            .map(|record| position[record.observation.observation_id.as_str()])
            .collect::<HashSet<_>>();
        let kept = collapse_semantic_duplicates_by(records, |record| {
            (
                record.observation.provider_account_id.is_some(),
                record.observation.usage_event_id.is_some(),
                window_count(position[record.observation.observation_id.as_str()]),
            )
        })
        .into_iter()
        .map(|record| position[record.observation.observation_id.as_str()])
        .collect::<HashSet<_>>();
        Ok(candidates.difference(&kept).copied().collect())
    }

    pub fn delete_quota_observations_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1)",
                    [&source_id.0],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1",
                    [&source_id.0],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    pub fn delete_quota_observations_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            let mut orphaned_payload_hashes = BTreeSet::new();
            for file_hash in file_hashes {
                let mut statement = self.conn.prepare(
                    "SELECT payload_hash FROM quota_observations
                     WHERE source_id = ?1 AND source_file_path_hash = ?2",
                )?;
                let rows = statement.query_map(params![&source_id.0, file_hash], |row| {
                    row.get::<_, String>(0)
                })?;
                for row in rows {
                    orphaned_payload_hashes.insert(row?);
                }
                drop(statement);

                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2)",
                    params![&source_id.0, file_hash],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2",
                    params![&source_id.0, file_hash],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads_for_hashes(&orphaned_payload_hashes)?;
            Ok(deleted)
        })
    }

    /// Deletes this source's quota rows that carry no source file hash: rows stored
    /// before file hashes were recorded.
    ///
    /// A scan that rereads every current file replaces their rows by file hash, which
    /// cannot reach these. Rows of files that are gone keep their hash and stay: the
    /// store keeps what a provider deleted from disk. The source index covers the
    /// lookup, and the set is normally empty, so the common case writes nothing.
    pub fn delete_quota_observations_without_source_file_hash(
        &self,
        source_id: &SourceId,
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut statement = self.conn.prepare(
                "SELECT observation_id, payload_hash FROM quota_observations
                 WHERE source_id = ?1 AND COALESCE(source_file_path_hash, '') = ''",
            )?;
            let orphaned = statement
                .query_map([&source_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            if orphaned.is_empty() {
                return Ok(0);
            }
            let mut deleted = 0u64;
            let mut orphaned_payload_hashes = BTreeSet::new();
            for (observation_id, payload_hash) in orphaned {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )? as u64);
                orphaned_payload_hashes.insert(payload_hash);
            }
            self.delete_unreferenced_quota_payloads_for_hashes(&orphaned_payload_hashes)?;
            Ok(deleted)
        })
    }

    /// Drops every payload no observation references, anywhere in the store.
    ///
    /// This reads `quota_payloads` in full and probes `quota_observations` for each
    /// row, which is proportional to the archive rather than to what was just deleted.
    /// Paths that know which payloads they orphaned should use
    /// [`Store::delete_unreferenced_quota_payloads_for_hashes`].
    pub(crate) fn delete_unreferenced_quota_payloads(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM quota_payloads WHERE payload_hash NOT IN (SELECT payload_hash FROM quota_observations)",
            [],
        )?;
        Ok(())
    }

    /// Drops the payloads among `payload_hashes` that nothing references any more.
    ///
    /// Payloads are shared: identical rate-limit responses collapse onto one row, so a
    /// deleted observation only orphans its payload when it was the last holder. The
    /// `NOT EXISTS` probe rides `quota_observations_payload_hash_idx` as a covering
    /// index, making this proportional to the deletion instead of the table.
    pub(crate) fn delete_unreferenced_quota_payloads_for_hashes(
        &self,
        payload_hashes: &BTreeSet<String>,
    ) -> Result<()> {
        if payload_hashes.is_empty() {
            return Ok(());
        }
        let mut statement = self.conn.prepare(
            "DELETE FROM quota_payloads WHERE payload_hash = ?1
             AND NOT EXISTS (
               SELECT 1 FROM quota_observations o WHERE o.payload_hash = quota_payloads.payload_hash
             )",
        )?;
        for payload_hash in payload_hashes {
            statement.execute([payload_hash])?;
        }
        Ok(())
    }

    pub fn reattribute_quota_observations(&self, source_id: &SourceId) -> Result<u64> {
        let assignments = self.list_source_account_assignments_for_source(source_id)?;
        let records = self.quota_observations_for_source(source_id)?;
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            for mut record in records {
                let account_id = assignment_for_timestamp(
                    &assignments,
                    record.observation.observed_at,
                )
                .map(|assignment| assignment.provider_account_id.clone());
                if record.observation.provider_account_id == account_id {
                    continue;
                }
                record.observation.provider_account_id = account_id;
                changed = changed.saturating_add(self.conn.execute(
                    "UPDATE quota_observations SET provider_account_id = ?2, payload = ?3 WHERE observation_id = ?1",
                    params![
                        &record.observation.observation_id,
                        record.observation.provider_account_id.as_ref().map(|id| id.0.as_str()),
                        serde_json::to_string(&record.observation)?,
                    ],
                )? as u64);
            }
            Ok(changed)
        })
    }

    pub(crate) fn quota_observations_for_source(
        &self,
        source_id: &SourceId,
    ) -> Result<Vec<QuotaObservationRecordV1>> {
        self.quota_observations(
            &QuotaQuery {
                source_id: Some(source_id.clone()),
                ..QuotaQuery::default()
            },
            false,
        )
    }
}

/// What a quota write did: the records it was given, and the rows it changed --
/// written with different contents, or deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct QuotaWriteCount {
    pub(crate) written: u64,
    pub(crate) changed: u64,
}

/// An attributed observation as `quota_observations_scope_idx` covers it.
#[derive(Clone)]
struct ScopedObservation {
    rowid: i64,
    provider: String,
    provider_account_id: ProviderAccountId,
    observed_at: DateTime<Utc>,
}

impl ScopedObservation {
    /// The observation with every field the index does not cover left empty.
    fn into_observation(self, observation_id: String) -> QuotaObservationV1 {
        QuotaObservationV1 {
            schema_version: String::new(),
            observation_id,
            semantic_fingerprint: String::new(),
            provider: self.provider,
            source_id: SourceId(String::new()),
            provider_account_id: Some(self.provider_account_id),
            observed_at: self.observed_at,
            source_file_path_hash: String::new(),
            source_record_id: String::new(),
            source_line_number: 0,
            payload_hash: String::new(),
            usage_sample: None,
            usage_event_id: None,
            usage_link_kind: QuotaUsageLinkKind::None,
            status: QuotaStatusV1::default(),
        }
    }
}

/// A window's `used_percent` as [`Store::quota_observations`] reads it back from
/// the window's JSON.
///
/// The column holds the value as observed. The JSON holds the shortest decimal
/// that round-trips, but serde_json's default parser can land one unit in the last
/// place away from it for a long fraction, and every result built so far, synced
/// ones included, carries the parsed value. Whole percentages, all Codex reports,
/// are exact either way and skip the detour.
fn used_percent_as_parsed(value: f64) -> f64 {
    if value.fract() == 0.0 {
        return value;
    }
    serde_json::to_string(&value)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(value)
}
