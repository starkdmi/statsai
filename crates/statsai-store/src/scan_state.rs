use super::*;

impl Store {
    pub fn pending_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<Vec<ScanFileStateEntry>> {
        let compatible_signatures = HashMap::new();
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                false,
                &compatible_signatures,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<ScanFileStateEntry>> {
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                false,
                compatible_signatures_by_key,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_task_requirement(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
    ) -> Result<Vec<ScanFileStateEntry>> {
        let compatible_signatures = HashMap::new();
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                require_tasks_collected,
                &compatible_signatures,
            )?
            .pending_entries)
    }

    pub fn pending_scan_file_entries_with_task_requirement_and_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<ScanFileStateEntry>> {
        Ok(self
            .select_scan_file_state_entries_with_task_requirement_and_compatibility(
                source_id,
                entries,
                require_tasks_collected,
                compatible_signatures_by_key,
            )?
            .pending_entries)
    }

    pub fn select_scan_file_state_entries_with_task_requirement_and_compatibility(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        require_tasks_collected: bool,
        compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    ) -> Result<ScanFileStateSelection> {
        let mut selection = ScanFileStateSelection::default();
        selection.pending_entries.reserve(entries.len());
        let mut stmt = self.conn.prepare(
            "SELECT cache_signature, tasks_collected FROM scan_file_state WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        for entry in entries {
            let existing = stmt
                .query_row(params![&source_id.0, &entry.cache_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })
                .optional()?;
            let Some((signature, tasks_collected)) = existing.as_ref() else {
                selection.pending_entries.push(entry.clone());
                continue;
            };
            let tasks_satisfied = !require_tasks_collected || *tasks_collected;
            if signature == &entry.cache_signature {
                if !tasks_satisfied {
                    selection.pending_entries.push(entry.clone());
                }
                continue;
            }
            let compatible_match = compatible_signatures_by_key
                .get(&entry.cache_key)
                .is_some_and(|compatible| {
                    compatible
                        .iter()
                        .any(|candidate_signature| candidate_signature == signature)
                });
            if compatible_match && tasks_satisfied {
                selection.compatible_entries_to_upgrade.push(entry.clone());
            } else {
                selection.pending_entries.push(entry.clone());
            }
        }
        Ok(selection)
    }

    pub fn record_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        self.record_scan_file_entries_with_tasks_collected(source_id, entries, false)
    }

    pub fn record_scan_file_entries_with_tasks_collected(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
        tasks_collected: bool,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let synced_at = Utc::now().to_rfc3339();
        let tasks_collected = i64::from(tasks_collected);
        self.with_immediate_transaction(|| {
            let mut stmt = self.conn.prepare(
                r#"
                INSERT INTO scan_file_state
                  (source_id, cache_key, cache_signature, synced_at, tasks_collected)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(source_id, cache_key) DO UPDATE SET
                  cache_signature = excluded.cache_signature,
                  synced_at = excluded.synced_at,
                  tasks_collected = CASE
                    WHEN scan_file_state.cache_signature = excluded.cache_signature
                    THEN MAX(scan_file_state.tasks_collected, excluded.tasks_collected)
                    ELSE excluded.tasks_collected
                  END
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    &source_id.0,
                    &entry.cache_key,
                    &entry.cache_signature,
                    &synced_at,
                    tasks_collected,
                ])?;
            }
            Ok(())
        })
    }

    pub fn upgrade_scan_file_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let synced_at = Utc::now().to_rfc3339();
        self.with_immediate_transaction(|| {
            let mut stmt = self.conn.prepare(
                r#"
                UPDATE scan_file_state
                   SET cache_signature = ?3,
                       synced_at = ?4
                 WHERE source_id = ?1
                   AND cache_key = ?2
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    &source_id.0,
                    &entry.cache_key,
                    &entry.cache_signature,
                    &synced_at,
                ])?;
            }
            Ok(())
        })
    }

    pub fn scan_file_entries(&self, source_id: &SourceId) -> Result<Vec<ScanFileStateEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, cache_signature
             FROM scan_file_state
             WHERE source_id = ?1
             ORDER BY cache_key",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| {
            Ok(ScanFileStateEntry {
                cache_key: row.get(0)?,
                cache_signature: row.get(1)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(Into::into)
    }

    pub fn delete_scan_file_entries(
        &self,
        source_id: &SourceId,
        cache_keys: &[String],
    ) -> Result<u64> {
        if cache_keys.is_empty() {
            return Ok(0);
        }
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            let mut stmt = self
                .conn
                .prepare("DELETE FROM scan_file_state WHERE source_id = ?1 AND cache_key = ?2")?;
            for cache_key in cache_keys {
                deleted += stmt.execute(params![&source_id.0, cache_key])? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn delete_scan_file_entries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM scan_file_state WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn replace_scan_file_records(
        &self,
        replacement: ScanFileReplacement<'_>,
    ) -> Result<ScanFileReplacementResult> {
        self.with_immediate_transaction(|| {
            self.delete_events_for_source_file_hashes(
                replacement.source_id,
                replacement.reconciled_file_hashes,
            )?;
            self.delete_summaries_for_source_file_hashes(
                replacement.source_id,
                replacement.reconciled_file_hashes,
            )?;
            let inserted_events = self.insert_events(replacement.events)?;
            let written_summaries = self.upsert_summaries(replacement.summaries)?;
            self.record_scan_file_entries(replacement.source_id, replacement.pending_entries)?;
            self.upgrade_scan_file_entries(
                replacement.source_id,
                replacement.compatible_entries_to_upgrade,
            )?;
            self.delete_scan_file_entries(replacement.source_id, replacement.removed_cache_keys)?;
            Ok(ScanFileReplacementResult {
                inserted_events,
                written_summaries,
            })
        })
    }

    pub fn source_records_missing_scan_file_hashes(&self, source_id: &SourceId) -> Result<bool> {
        let event_missing: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM usage_events
            WHERE source_id = ?1
              AND COALESCE(json_extract(payload, '$.parse_evidence.source_file_path_hash'), '') = ''
            "#,
            params![&source_id.0],
            |row| row.get(0),
        )?;
        if event_missing > 0 {
            return Ok(true);
        }

        let summary_missing: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM usage_summaries
            WHERE source_id = ?1
              AND COALESCE(json_extract(payload, '$.parse_evidence.source_file_path_hash'), '') = ''
            "#,
            params![&source_id.0],
            |row| row.get(0),
        )?;
        Ok(summary_missing > 0)
    }
}
