use super::*;

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn store_archive_scan_with_code_changes(
        &self,
        source_id: &SourceId,
        conversations: &[ArchiveConversation],
        imported_entries: &[ScanFileStateEntry],
        artifact_dependencies: &[ArchiveArtifactDependency],
        trace_edits: &[TraceEdit],
        trace_coverage: CoverageStatus,
        quota_observations: &[QuotaObservationRecordV1],
    ) -> Result<ArchiveWriteResult> {
        self.with_immediate_transaction(|| {
            let result = self.upsert_archive_conversations(conversations)?;
            self.replace_archive_trace_edits_inner(
                source_id,
                imported_entries,
                trace_edits,
                trace_coverage,
            )?;
            self.record_archive_import_entries(source_id, imported_entries)?;
            self.replace_archive_artifact_dependencies(
                source_id,
                imported_entries,
                artifact_dependencies,
            )?;
            let source_file_path_hashes = imported_entries
                .iter()
                .map(|entry| hash_text(&entry.cache_key))
                .collect::<Vec<_>>();
            self.replace_quota_observations_for_source_files_inner(
                source_id,
                &source_file_path_hashes,
                quota_observations,
            )?;
            Ok(result)
        })
    }

    pub fn pending_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<Vec<ScanFileStateEntry>> {
        let mut statement = self.conn.prepare(
            "SELECT cache_signature FROM archive_import_state WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        let mut dependency_statement = self.conn.prepare(
            r#"
            SELECT artifact_path, metadata_signature
            FROM archive_artifact_dependencies
            WHERE source_id = ?1 AND cache_key = ?2
            "#,
        )?;
        let mut pending = Vec::new();
        for entry in entries {
            let existing = statement
                .query_row(params![&source_id.0, &entry.cache_key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?;
            let expected = archive_import_signature(&entry.cache_signature);
            if existing.as_deref() != Some(expected.as_str()) {
                pending.push(entry.clone());
                continue;
            }
            let dependencies = dependency_statement
                .query_map(params![&source_id.0, &entry.cache_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            for dependency in dependencies {
                let (path, stored_signature) = dependency?;
                if archive_artifact_metadata_signature(Path::new(&path)) != stored_signature {
                    pending.push(entry.clone());
                    break;
                }
            }
        }
        Ok(pending)
    }

    /// Number of archive files already imported from a source.
    pub fn archive_import_entry_count(&self, source_id: &SourceId) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM archive_import_state WHERE source_id = ?1",
            [&source_id.0],
            |row| row.get::<_, u64>(0),
        )?)
    }

    pub fn reconcile_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<u64> {
        let current_cache_keys = entries
            .iter()
            .map(|entry| entry.cache_key.as_str())
            .collect::<HashSet<_>>();
        self.with_immediate_transaction(|| {
            let mut statement = self.conn.prepare(
                "SELECT cache_key FROM archive_import_state WHERE source_id = ?1",
            )?;
            let stored_cache_keys = statement
                .query_map([&source_id.0], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let removed_cache_keys = stored_cache_keys
                .into_iter()
                .filter(|cache_key| !current_cache_keys.contains(cache_key.as_str()))
                .collect::<Vec<_>>();
            for cache_key in &removed_cache_keys {
                self.delete_archive_trace_entry_inner(source_id, cache_key)?;
                self.conn.execute(
                    "DELETE FROM archive_artifact_dependencies WHERE source_id = ?1 AND cache_key = ?2",
                    params![&source_id.0, cache_key],
                )?;
                self.conn.execute(
                    "DELETE FROM archive_import_state WHERE source_id = ?1 AND cache_key = ?2",
                    params![&source_id.0, cache_key],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2)",
                    params![&source_id.0, hash_text(cache_key)],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2",
                    params![&source_id.0, hash_text(cache_key)],
                )?;
            }
            self.conn.execute(
                "DELETE FROM quota_payloads WHERE payload_hash NOT IN (SELECT payload_hash FROM quota_observations)",
                [],
            )?;
            Ok(removed_cache_keys.len() as u64)
        })
    }

    pub(crate) fn record_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        let collected_at = Utc::now().to_rfc3339();
        let mut statement = self.conn.prepare(
            r#"
            INSERT INTO archive_import_state (source_id, cache_key, cache_signature, collected_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(source_id, cache_key) DO UPDATE SET
              cache_signature = excluded.cache_signature,
              collected_at = excluded.collected_at
            "#,
        )?;
        for entry in entries {
            statement.execute(params![
                &source_id.0,
                &entry.cache_key,
                archive_import_signature(&entry.cache_signature),
                &collected_at,
            ])?;
        }
        Ok(())
    }

    pub(crate) fn replace_archive_artifact_dependencies(
        &self,
        source_id: &SourceId,
        imported_entries: &[ScanFileStateEntry],
        dependencies: &[ArchiveArtifactDependency],
    ) -> Result<()> {
        let imported_cache_keys = imported_entries
            .iter()
            .map(|entry| entry.cache_key.as_str())
            .collect::<HashSet<_>>();
        let mut delete_statement = self.conn.prepare(
            "DELETE FROM archive_artifact_dependencies WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        for entry in imported_entries {
            delete_statement.execute(params![&source_id.0, &entry.cache_key])?;
        }

        let mut insert_statement = self.conn.prepare(
            r#"
            INSERT INTO archive_artifact_dependencies
              (source_id, cache_key, artifact_path, metadata_signature)
            VALUES (?1, ?2, ?3, ?4)
            "#,
        )?;
        for dependency in dependencies {
            ensure!(
                imported_cache_keys.contains(dependency.cache_key.as_str()),
                "archive artifact dependency does not match an imported cache entry: {}",
                dependency.cache_key
            );
            insert_statement.execute(params![
                &source_id.0,
                &dependency.cache_key,
                dependency.path.to_string_lossy().as_ref(),
                &dependency.metadata_signature,
            ])?;
        }
        Ok(())
    }
}
