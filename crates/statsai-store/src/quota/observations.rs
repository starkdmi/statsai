use super::*;

impl Store {
    pub fn upsert_quota_observations(&self, records: &[QuotaObservationRecordV1]) -> Result<u64> {
        self.with_immediate_transaction(|| self.upsert_quota_observations_inner(records))
    }

    pub(crate) fn upsert_quota_observations_inner(
        &self,
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        let mut written = 0u64;
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
        Ok(written)
    }

    pub(crate) fn replace_quota_observations_for_source_files_inner(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
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

        for source_file_path_hash in source_file_path_hashes {
            let mut statement = self.conn.prepare(
                "SELECT observation_id FROM quota_observations
                 WHERE source_id = ?1 AND source_file_path_hash = ?2",
            )?;
            let stored_observation_ids = statement
                .query_map(params![&source_id.0, source_file_path_hash], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            for observation_id in stored_observation_ids {
                if retained_observation_ids.contains(observation_id.as_str()) {
                    continue;
                }
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
            }
        }

        let written = self.upsert_quota_observations_inner(records)?;
        self.delete_unreferenced_quota_payloads()?;
        Ok(written)
    }

    pub fn replace_quota_observations_for_source_files(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
        records: &[QuotaObservationRecordV1],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            self.replace_quota_observations_for_source_files_inner(
                source_id,
                source_file_path_hashes,
                records,
            )
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
        records.sort_by_key(|record| {
            (
                record.observation.observed_at,
                record.observation.observation_id.clone(),
            )
        });
        Ok(if collapse_duplicates {
            collapse_semantic_duplicates(records)
        } else {
            records
        })
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
            for file_hash in file_hashes {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2)",
                    params![&source_id.0, file_hash],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2",
                    params![&source_id.0, file_hash],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    /// Deletes this source's quota rows that no file in `source_file_path_hashes` accounts for.
    ///
    /// A full rescan replaces every file it can see, but `replace_quota_observations_for_source_files`
    /// only reconciles the hashes handed to it, so rows written for a file that has since disappeared
    /// -- and legacy rows stored before file hashes were recorded -- outlive the scan that should have
    /// retired them. Deleting the whole source instead would retire them, and that is what `--no-cache`
    /// used to do: on a store with six figures of observations it walked every row, every window, and
    /// the payload table, which is the stall this replaces. The unmatched set is normally empty, and the
    /// source index covers the lookup, so the common case reads an index range and writes nothing.
    pub fn delete_quota_observations_for_source_outside_file_hashes(
        &self,
        source_id: &SourceId,
        source_file_path_hashes: &[String],
    ) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let retained = source_file_path_hashes
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let mut statement = self.conn.prepare(
                "SELECT observation_id, source_file_path_hash FROM quota_observations
                 WHERE source_id = ?1",
            )?;
            let orphaned = statement
                .query_map([&source_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, file_hash)| !retained.contains(file_hash.as_str()))
                .map(|(observation_id, _)| observation_id)
                .collect::<Vec<_>>();
            drop(statement);
            if orphaned.is_empty() {
                return Ok(0);
            }
            let mut deleted = 0u64;
            for observation_id in orphaned {
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id = ?1",
                    [&observation_id],
                )?;
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM quota_observations WHERE observation_id = ?1",
                    [&observation_id],
                )? as u64);
            }
            self.delete_unreferenced_quota_payloads()?;
            Ok(deleted)
        })
    }

    pub(crate) fn delete_unreferenced_quota_payloads(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM quota_payloads WHERE payload_hash NOT IN (SELECT payload_hash FROM quota_observations)",
            [],
        )?;
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
