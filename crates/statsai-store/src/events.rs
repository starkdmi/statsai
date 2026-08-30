use super::*;
use crate::dedupe::*;

#[derive(Debug)]
struct EventInsertOutcome {
    inserted: bool,
    canonical_event_id: EventId,
    dirty_keys: BTreeSet<SyncRollupBucketKey>,
}

impl Store {
    pub fn insert_event(&self, event: &UsageEvent) -> Result<bool> {
        let event = event_with_valid_project(event);
        let fingerprint = event_fingerprint(&event);
        if let Some(existing_id) = self.find_semantic_duplicate_event_id(&event, &fingerprint)? {
            let existing = self.event_by_id(&existing_id)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), &event, existing_id.as_str());
            let dirty_keys = self.update_event_payload(&refreshed)?;
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            return Ok(false);
        }

        let payload = serde_json::to_string(&event)?;
        let changed = self.conn.execute(
            r#"
            INSERT OR IGNORE INTO usage_events (
              event_id, provider, source_id, provider_account_id, started_at, total_tokens,
              semantic_fingerprint, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                &fingerprint,
                &payload
            ],
        )?;
        if changed == 0 {
            let existing = self.event_by_id(&event.event_id.0)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), &event, event.event_id.0.as_str());
            let dirty_keys = self.update_event_payload(&refreshed)?;
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
        } else {
            self.refresh_sync_rollups_for_keys(&BTreeSet::from([sync_rollup_bucket_key(&event)]))?;
        }
        Ok(changed > 0)
    }

    pub fn insert_events(&self, events: &[UsageEvent]) -> Result<u64> {
        Ok(self.insert_events_with_resolution(events)?.inserted)
    }

    pub fn insert_events_with_resolution(
        &self,
        events: &[UsageEvent],
    ) -> Result<EventInsertBatchResult> {
        let events = events
            .iter()
            .map(event_with_valid_project)
            .collect::<Vec<_>>();
        let fingerprints: Vec<String> = events.iter().map(event_fingerprint).collect();
        let conflict_keys: Vec<ConflictLookupKey> = events
            .iter()
            .zip(fingerprints.iter())
            .map(|(event, fingerprint)| conflict_lookup_key(event, fingerprint))
            .collect();
        self.with_immediate_transaction(|| {
            let mut conflict_map = self.batch_load_conflicts(&conflict_keys)?;
            let mut inserted = 0u64;
            let mut canonical_event_ids = HashMap::with_capacity(events.len());
            let mut dirty_keys = BTreeSet::new();
            for (index, event) in events.iter().enumerate() {
                let incoming_event_id = event.event_id.clone();
                let matched_event =
                    conflict_map
                        .get(&conflict_keys[index])
                        .and_then(|candidates| {
                            exact_or_semantic_conflict(Some(candidates.as_slice()), event).map(
                                |candidate| (candidate.event_id.clone(), candidate.event.clone()),
                            )
                        });
                let matched_event = if matched_event.is_some() {
                    matched_event
                } else if let Some(existing_id) =
                    self.find_codex_fallback_duplicate_event_id(event)?
                {
                    self.event_by_id(&existing_id)?
                        .map(|existing| (existing_id, existing))
                } else {
                    None
                };
                if let Some((existing_id, existing)) = matched_event {
                    let refreshed =
                        refreshed_duplicate_event(Some(&existing), event, existing_id.as_str());
                    dirty_keys.extend(self.update_event_payload(&refreshed)?);
                    canonical_event_ids.insert(incoming_event_id, EventId(existing_id.clone()));
                    let candidates = conflict_map
                        .entry(conflict_keys[index].clone())
                        .or_default();
                    if let Some(candidate) = candidates
                        .iter_mut()
                        .find(|candidate| candidate.event_id == existing_id)
                    {
                        candidate.event = refreshed;
                    } else {
                        candidates.push(ConflictCandidate {
                            event_id: existing_id,
                            event: refreshed,
                        });
                    }
                    continue;
                }
                let fingerprint = &fingerprints[index];
                let outcome = self.insert_event_in_batch(event, fingerprint)?;
                if outcome.inserted {
                    inserted += 1;
                }
                canonical_event_ids.insert(incoming_event_id, outcome.canonical_event_id.clone());
                dirty_keys.extend(outcome.dirty_keys);
                conflict_map
                    .entry(conflict_keys[index].clone())
                    .or_default()
                    .push(ConflictCandidate {
                        event_id: outcome.canonical_event_id.0,
                        event: event.clone(),
                    });
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(EventInsertBatchResult {
                inserted,
                canonical_event_ids,
            })
        })
    }

    pub(crate) fn update_event_payload(
        &self,
        event: &UsageEvent,
    ) -> Result<BTreeSet<SyncRollupBucketKey>> {
        let existing_bucket = self
            .event_by_id(&event.event_id.0)?
            .map(|existing| sync_rollup_bucket_key(&existing));
        let bucket = self.update_event_cost_payload(event)?;
        let mut dirty_keys = BTreeSet::new();
        if let Some(existing_bucket) = existing_bucket {
            dirty_keys.insert(existing_bucket);
        }
        dirty_keys.insert(bucket);
        Ok(dirty_keys)
    }

    /// Updates a persisted event's payload without re-reading it.
    ///
    /// Repricing only changes estimated cost, so the sync-rollup bucket is the
    /// in-memory event's bucket.
    pub(crate) fn update_event_cost_payload(
        &self,
        event: &UsageEvent,
    ) -> Result<SyncRollupBucketKey> {
        let payload = serde_json::to_string(event)?;
        let fingerprint = event_fingerprint(event);
        self.conn.execute(
            r#"
            UPDATE usage_events
            SET provider = ?2,
                source_id = ?3,
                provider_account_id = ?4,
                started_at = ?5,
                total_tokens = ?6,
                semantic_fingerprint = ?7,
                payload = ?8
            WHERE event_id = ?1
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                &fingerprint,
                &payload
            ],
        )?;
        Ok(sync_rollup_bucket_key(event))
    }

    fn insert_event_in_batch(
        &self,
        event: &UsageEvent,
        fingerprint: &str,
    ) -> Result<EventInsertOutcome> {
        let payload = serde_json::to_string(event)?;
        let changed = self.conn.execute(
            r#"
            INSERT OR IGNORE INTO usage_events (
              event_id, provider, source_id, provider_account_id, started_at, total_tokens,
              semantic_fingerprint, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                &event.event_id.0,
                &event.provider,
                &event.source_id.0,
                event.provider_account_id.as_ref().map(|id| id.0.as_str()),
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
                fingerprint,
                &payload
            ],
        )?;
        if changed == 0 {
            let existing = self.event_by_id(&event.event_id.0)?;
            let refreshed =
                refreshed_duplicate_event(existing.as_ref(), event, event.event_id.0.as_str());
            return Ok(EventInsertOutcome {
                inserted: false,
                canonical_event_id: event.event_id.clone(),
                dirty_keys: self.update_event_payload(&refreshed)?,
            });
        }
        Ok(EventInsertOutcome {
            inserted: true,
            canonical_event_id: event.event_id.clone(),
            dirty_keys: BTreeSet::from([sync_rollup_bucket_key(event)]),
        })
    }

    fn batch_load_conflicts(
        &self,
        keys: &[ConflictLookupKey],
    ) -> Result<std::collections::HashMap<ConflictLookupKey, Vec<ConflictCandidate>>> {
        let mut conflicts = std::collections::HashMap::new();
        if keys.is_empty() {
            return Ok(conflicts);
        }

        // Keep the query within SQLite's common 999-parameter limit:
        // each lookup uses provider, source_id, and semantic_fingerprint.
        const CHUNK_SIZE: usize = 300;
        for chunk in keys.chunks(CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let base = (i * 3) + 1;
                    format!(
                        "(provider = ?{base} AND source_id = ?{} AND semantic_fingerprint = ?{})",
                        base + 1,
                        base + 2
                    )
                })
                .collect();
            let sql = format!(
                "SELECT provider, source_id, event_id, semantic_fingerprint, payload \
                 FROM usage_events WHERE {}",
                placeholders.join(" OR ")
            );

            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .flat_map(|key| {
                    [
                        &key.provider as &dyn rusqlite::types::ToSql,
                        &key.source_id as &dyn rusqlite::types::ToSql,
                        &key.fingerprint as &dyn rusqlite::types::ToSql,
                    ]
                })
                .collect();

            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;

            for row in rows {
                let (provider, source_id, event_id, fingerprint, payload) = row?;
                let event: UsageEvent = serde_json::from_str(&payload)?;
                conflicts
                    .entry(ConflictLookupKey {
                        provider,
                        source_id,
                        fingerprint,
                    })
                    .or_insert_with(Vec::new)
                    .push(ConflictCandidate { event_id, event });
            }
        }
        Ok(conflicts)
    }

    fn find_semantic_duplicate_event_id(
        &self,
        event: &UsageEvent,
        fingerprint: &str,
    ) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT event_id, payload
            FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND semantic_fingerprint = ?3
            "#,
        )?;
        let rows = stmt.query_map(
            params![&event.provider, &event.source_id.0, fingerprint,],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (event_id, payload) = row?;
            if event_id == event.event_id.0 {
                return Ok(None);
            }
            let candidate: UsageEvent = serde_json::from_str(&payload)?;
            if semantically_same_event(&candidate, event) {
                return Ok(Some(event_id));
            }
        }
        self.find_codex_fallback_duplicate_event_id(event)
    }

    fn find_codex_fallback_duplicate_event_id(&self, event: &UsageEvent) -> Result<Option<String>> {
        if event.provider != "codex" {
            return Ok(None);
        }
        let mut fallback = self.conn.prepare(
            r#"
            SELECT event_id, payload
            FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND started_at = ?3
              AND total_tokens = ?4
            "#,
        )?;
        let fallback_rows = fallback.query_map(
            params![
                &event.provider,
                &event.source_id.0,
                event.session.started_at.to_rfc3339(),
                safe_u64_to_i64(event.usage.computed_total()),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in fallback_rows {
            let (event_id, payload) = row?;
            if event_id == event.event_id.0 {
                return Ok(None);
            }
            let candidate: UsageEvent = serde_json::from_str(&payload)?;
            if semantically_same_event(&candidate, event) {
                return Ok(Some(event_id));
            }
        }
        Ok(None)
    }

    pub fn event_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn token_total(&self) -> Result<u64> {
        let count: Option<i64> =
            self.conn
                .query_row("SELECT SUM(total_tokens) FROM usage_events", [], |row| {
                    row.get(0)
                })?;
        Ok(count.unwrap_or(0) as u64)
    }

    pub fn usage_period_stats(&self, since: DateTime<Utc>) -> Result<UsagePeriodStats> {
        let since = since.to_rfc3339();
        self.conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM usage_events WHERE started_at >= ?1",
                params![since],
                |row| {
                    Ok(UsagePeriodStats {
                        events: row.get::<_, i64>(0)? as u64,
                        tokens: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn unsynced_event_count(&self, cursor: Option<(&DateTime<Utc>, &str)>) -> Result<u64> {
        let count: i64 = if let Some((started_at, event_id)) = cursor {
            self.conn.query_row(
                r#"
                SELECT COUNT(*) FROM usage_events
                WHERE started_at > ?1 OR (started_at = ?1 AND event_id > ?2)
                "#,
                params![started_at.to_rfc3339(), event_id],
                |row| row.get(0),
            )?
        } else {
            self.conn
                .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))?
        };
        Ok(count as u64)
    }

    pub fn events(&self) -> Result<Vec<UsageEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM usage_events ORDER BY started_at, event_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut events: Vec<UsageEvent> = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    pub fn events_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
    ) -> Result<Vec<UsageEvent>> {
        let mut events = Vec::new();
        if let Some(since) = since {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT payload FROM usage_events
                WHERE started_at >= ?1 AND started_at <= ?2
                ORDER BY started_at, event_id
                "#,
            )?;
            let rows = stmt.query_map(params![since.to_rfc3339(), until.to_rfc3339()], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT payload FROM usage_events
                WHERE started_at <= ?1
                ORDER BY started_at, event_id
                "#,
            )?;
            let rows =
                stmt.query_map(params![until.to_rfc3339()], |row| row.get::<_, String>(0))?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(events)
    }

    pub fn events_for_source(&self, source_id: &SourceId) -> Result<Vec<UsageEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM usage_events WHERE source_id = ?1 ORDER BY started_at, event_id",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    pub fn events_after(&self, cursor: Option<(&DateTime<Utc>, &str)>) -> Result<Vec<UsageEvent>> {
        let sql = if cursor.is_some() {
            r#"
            SELECT payload FROM usage_events
            WHERE started_at > ?1 OR (started_at = ?1 AND event_id > ?2)
            ORDER BY started_at, event_id
            "#
        } else {
            "SELECT payload FROM usage_events ORDER BY started_at, event_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut events: Vec<UsageEvent> = Vec::new();
        if let Some((started_at, event_id)) = cursor {
            let rows = stmt.query_map(params![started_at.to_rfc3339(), event_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        } else {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(events)
    }

    pub fn rewrite_events(&self, events: &[UsageEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            let mut dirty_keys = BTreeSet::new();
            for event in events {
                dirty_keys.extend(self.update_event_payload(event)?);
                changed += 1;
            }
            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(changed)
        })
    }

    pub fn delete_events_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_events WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            self.delete_sync_rollups_for_sources_in_tx(source_ids)?;
            Ok(deleted)
        })
    }

    pub fn delete_events_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        if file_hashes.is_empty() {
            return Ok(0);
        }

        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            let mut dirty_keys = BTreeSet::new();

            for file_hash in file_hashes {
                let payloads: Vec<String>;
                {
                    let mut stmt = self.conn.prepare(
                        r#"
                        SELECT payload
                        FROM usage_events
                        WHERE source_id = ?1
                          AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                        "#,
                    )?;
                    let rows =
                        stmt.query_map(params![&source_id.0, file_hash], |row| row.get(0))?;
                    payloads = rows.collect::<Result<Vec<_>, _>>()?;
                }

                for payload in payloads {
                    let event: UsageEvent = serde_json::from_str(&payload)?;
                    dirty_keys.insert(sync_rollup_bucket_key(&event));
                }

                deleted += self.conn.execute(
                    r#"
                    DELETE FROM usage_events
                    WHERE source_id = ?1
                      AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                    "#,
                    params![&source_id.0, file_hash],
                )? as u64;
            }

            self.refresh_sync_rollups_for_keys(&dirty_keys)?;
            Ok(deleted)
        })
    }

    pub(crate) fn event_by_id(&self, event_id: &str) -> Result<Option<UsageEvent>> {
        self.conn
            .query_row(
                "SELECT payload FROM usage_events WHERE event_id = ?1",
                params![event_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }
}
