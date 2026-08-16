use super::Store;
use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde::Serialize;
use sha2::Sha256;
use statsai_core::{
    build_code_change_metrics, hash_text, match_trace_edits_to_commits,
    scan_local_git_repository_cached, AttributionConfidence, CodeChangeMatch, CodeChangeMetric,
    CodeChangeMetricKind, CoverageStatus, GitCommitChange, GitScan, GitScanError, SourceId,
    TraceEdit,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodeChangeRefreshReport {
    pub repositories: u64,
    pub trace_edits: u64,
    pub commits: u64,
    pub matches: u64,
    pub metrics: u64,
    pub trace_coverage: CoverageStatus,
    pub git_coverage: CoverageStatus,
}

impl Default for CodeChangeRefreshReport {
    fn default() -> Self {
        Self {
            repositories: 0,
            trace_edits: 0,
            commits: 0,
            matches: 0,
            metrics: 0,
            trace_coverage: CoverageStatus::Unavailable,
            git_coverage: CoverageStatus::Unavailable,
        }
    }
}

impl Store {
    pub(crate) fn ingest_code_change_metrics_inner(
        &self,
        metrics: &[CodeChangeMetric],
    ) -> Result<()> {
        for metric in metrics {
            self.conn.execute(
                r#"
                INSERT INTO code_change_metrics
                  (metric_id, device_id, day, project_id, repository_hash, commit_hash,
                   kind, payload, dirty)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
                ON CONFLICT(metric_id) DO UPDATE SET
                  device_id = excluded.device_id,
                  day = excluded.day,
                  project_id = excluded.project_id,
                  repository_hash = excluded.repository_hash,
                  commit_hash = excluded.commit_hash,
                  kind = excluded.kind,
                  payload = excluded.payload,
                  dirty = 0
                WHERE code_change_metrics.payload IS NOT excluded.payload
                "#,
                params![
                    &metric.metric_id,
                    &metric.device_id,
                    metric.day.to_string(),
                    &metric.project_id,
                    &metric.repository_hash,
                    &metric.commit_hash,
                    metric_kind_name(metric.kind),
                    serde_json::to_string(metric)?,
                ],
            )?;
        }
        Ok(())
    }

    pub(crate) fn replace_archive_trace_edits_inner(
        &self,
        source_id: &statsai_core::SourceId,
        imported_entries: &[super::ScanFileStateEntry],
        trace_edits: &[TraceEdit],
        coverage: CoverageStatus,
    ) -> Result<()> {
        let updated_at = Utc::now().to_rfc3339();
        for entry in imported_entries {
            self.delete_archive_trace_entry_inner(source_id, &entry.cache_key)?;
            self.conn.execute(
                r#"
                INSERT INTO code_trace_coverage (source_id, cache_key, coverage, updated_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(source_id, cache_key) DO UPDATE SET
                  coverage = excluded.coverage,
                  updated_at = excluded.updated_at
                "#,
                params![
                    &source_id.0,
                    &entry.cache_key,
                    coverage_name(coverage),
                    &updated_at
                ],
            )?;
        }
        for edit in trace_edits {
            let payload = serde_json::to_string(edit)?;
            self.conn.execute(
                r#"
                INSERT INTO code_trace_edits
                  (trace_edit_id, source_id, cache_key, conversation_id, source_record_id,
                   occurred_at, project_id, repository_path, relative_path, payload)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(trace_edit_id) DO UPDATE SET
                  cache_key = excluded.cache_key,
                  occurred_at = excluded.occurred_at,
                  project_id = excluded.project_id,
                  repository_path = excluded.repository_path,
                  relative_path = excluded.relative_path,
                  payload = excluded.payload
                "#,
                params![
                    &edit.trace_edit_id,
                    &edit.source_id.0,
                    &edit.cache_key,
                    &edit.conversation_id,
                    &edit.source_record_id,
                    edit.occurred_at.map(|value| value.to_rfc3339()),
                    &edit.project_id,
                    edit.repository_path
                        .as_ref()
                        .map(|value| value.to_string_lossy().into_owned()),
                    edit.relative_path.to_string_lossy(),
                    payload,
                ],
            )?;
        }
        Ok(())
    }

    pub(crate) fn delete_archive_trace_entry_inner(
        &self,
        source_id: &statsai_core::SourceId,
        cache_key: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            DELETE FROM code_trace_edits WHERE source_id = ?1 AND cache_key = ?2
            "#,
            params![&source_id.0, cache_key],
        )?;
        self.conn.execute(
            "DELETE FROM code_trace_coverage WHERE source_id = ?1 AND cache_key = ?2",
            params![&source_id.0, cache_key],
        )?;
        Ok(())
    }

    /// Retires everything an archive import owns for the given sources.
    ///
    /// Returns the number of trace edits dropped. Three things travel together
    /// here. The conversations are the archived copy of the source's data, and
    /// a request to delete a source's data that leaves them behind has not
    /// honoured it. The import state marks a file as already read, so leaving
    /// it would make a re-added source look fully imported and silently never
    /// rebuild what was deleted. The traces are derived from both.
    ///
    /// Deletion is ordered child-first because the foreign keys are enforced;
    /// the full-text index and the privacy-derived tables follow through their
    /// own delete triggers.
    ///
    /// Callers must follow this with a refresh — the metrics derived from these
    /// edits are already materialized, and the authoritative snapshot keeps
    /// republishing them until a refresh rebuilds it without them.
    pub fn delete_archive_import_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0_u64;
            for source_id in source_ids {
                deleted = deleted.saturating_add(self.conn.execute(
                    "DELETE FROM code_trace_edits WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64);
                self.conn.execute(
                    r#"
                    DELETE FROM archive_content_parts
                    WHERE item_id IN (
                      SELECT i.item_id
                      FROM archive_items i
                      JOIN archive_conversations c ON c.conversation_id = i.conversation_id
                      WHERE c.source_id = ?1
                    )
                    "#,
                    params![&source_id.0],
                )?;
                self.conn.execute(
                    r#"
                    DELETE FROM archive_items
                    WHERE conversation_id IN (
                      SELECT conversation_id FROM archive_conversations WHERE source_id = ?1
                    )
                    "#,
                    params![&source_id.0],
                )?;
                for table in [
                    "archive_conversations",
                    "code_trace_coverage",
                    "archive_artifact_dependencies",
                    "archive_import_state",
                ] {
                    self.conn.execute(
                        &format!("DELETE FROM {table} WHERE source_id = ?1"),
                        params![&source_id.0],
                    )?;
                }
            }
            Ok(deleted)
        })
    }

    pub fn list_trace_edits(&self) -> Result<Vec<TraceEdit>> {
        let mut statement = self
            .conn
            .prepare("SELECT payload FROM code_trace_edits ORDER BY occurred_at, trace_edit_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut edits = Vec::new();
        for row in rows {
            edits.push(serde_json::from_str(&row?)?);
        }
        Ok(edits)
    }

    /// Stored metrics, with the Git object ID the payload deliberately omits
    /// restored from its column.
    ///
    /// `commit_hash` is `#[serde(skip)]` so it can never reach a sync payload
    /// by any route, which also means it is absent from the stored JSON. Every
    /// read rehydrates it from the column, so no caller has to know that the
    /// payload alone under-reports the metric.
    pub fn list_code_change_metrics(&self, dirty_only: bool) -> Result<Vec<CodeChangeMetric>> {
        let sql = if dirty_only {
            "SELECT payload, commit_hash FROM code_change_metrics WHERE dirty = 1 ORDER BY day, metric_id"
        } else {
            "SELECT payload, commit_hash FROM code_change_metrics ORDER BY day, metric_id"
        };
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut metrics = Vec::new();
        for row in rows {
            let (payload, commit_hash) = row?;
            let mut metric: CodeChangeMetric = serde_json::from_str(&payload)?;
            metric.commit_hash = commit_hash;
            metrics.push(metric);
        }
        Ok(metrics)
    }

    pub fn pending_code_change_metrics_for_sync(
        &self,
        sink: &str,
        target: &str,
        metrics: &[CodeChangeMetric],
    ) -> Result<Vec<CodeChangeMetric>> {
        let mut pending = Vec::new();
        for metric in metrics {
            let payload = serde_json::to_string(metric)?;
            if self.entity_requires_sync(
                sink,
                target,
                "code_change_metric",
                &metric.metric_id,
                &hash_text(&payload),
            )? {
                pending.push(metric.clone());
            }
        }
        Ok(pending)
    }

    pub fn mark_code_change_metrics_synced(&self, metric_ids: &[String]) -> Result<()> {
        self.with_immediate_transaction(|| {
            for metric_id in metric_ids {
                self.conn.execute(
                    "UPDATE code_change_metrics SET dirty = 0 WHERE metric_id = ?1",
                    [metric_id],
                )?;
            }
            Ok(())
        })
    }

    pub fn refresh_code_changes(&self, device_id: &str) -> Result<CodeChangeRefreshReport> {
        self.refresh_code_changes_inner(device_id, None)
    }

    /// Rebuilds code-change metrics with committed IDs blinded by an
    /// account-scoped key shared by that account's devices.
    pub fn refresh_code_changes_with_identity_key(
        &self,
        device_id: &str,
        identity_key: &[u8; 32],
    ) -> Result<CodeChangeRefreshReport> {
        self.refresh_code_changes_inner(device_id, Some(identity_key))
    }

    fn refresh_code_changes_inner(
        &self,
        device_id: &str,
        identity_key: Option<&[u8; 32]>,
    ) -> Result<CodeChangeRefreshReport> {
        let now = Utc::now();
        let trace_edits = self.list_trace_edits()?;
        let mut repositories = self.repository_projects()?;
        for edit in &trace_edits {
            if let Some(path) = edit.repository_path.as_ref() {
                repositories.insert(path.clone(), edit.project_id.clone());
            }
        }
        let stored_scans = self.list_git_scans()?;
        let mut available_scans = stored_scans.clone();
        let mut scans = Vec::new();
        let mut refreshed_roots = BTreeSet::new();
        let mut failed_roots = BTreeSet::new();
        // Roots whose recorded path could not be resolved this run. Whether
        // that is a lost measurement is only decided once every root has been
        // scanned, because a sibling path usually still covers the repository.
        let mut unresolved_roots = BTreeSet::new();
        let mut failed_scans = 0_u64;
        // Dozens of recorded project paths routinely resolve to a handful of
        // repositories. Roots are resolved first, with one cheap read-only
        // command each, so history inspection runs once per repository instead
        // of once per project path.
        let mut project_ids_by_root = BTreeMap::<PathBuf, Option<String>>::new();
        for (path, project_id) in repositories {
            let cache_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let cached_root = available_scans
                .iter()
                .filter(|scan| cache_path.starts_with(&scan.repository_root))
                .max_by_key(|scan| scan.repository_root.components().count())
                .map(|scan| scan.repository_root.clone());
            let resolved = if path.is_dir() {
                statsai_core::resolve_git_repository_root(&path)
            } else {
                Err(GitScanError::NotRepository(path.clone()))
            };
            match resolved {
                Ok(root) => {
                    // A commit can span multiple nested projects in one Git
                    // root. Until file-level partitioning is available, retain
                    // a project identity only when every observed path agrees
                    // on it.
                    match project_ids_by_root.entry(root) {
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert(project_id);
                        }
                        std::collections::btree_map::Entry::Occupied(mut slot) => {
                            if slot.get() != &project_id {
                                slot.insert(None);
                            }
                        }
                    }
                }
                // Agents also run outside repositories, and a recorded path can
                // be a scratch directory or a subdirectory that has since been
                // removed. That is nothing to measure rather than a failed
                // measurement, so it must not degrade Git coverage on its own.
                // A root some earlier scan did cover is only a regression when
                // no surviving path reaches it either, which is settled below.
                Err(GitScanError::NotRepository(_)) => {
                    if let Some(cached_root) = cached_root {
                        unresolved_roots.insert(cached_root);
                    }
                }
                // Git itself could not answer: a missing binary or an
                // unreadable object database leaves real history unmeasured.
                Err(_) => {
                    if let Some(cached_root) = cached_root {
                        failed_roots.insert(cached_root);
                    }
                    failed_scans = failed_scans.saturating_add(1);
                }
            }
        }
        for (root, project_id) in project_ids_by_root {
            let cached_commits = available_scans
                .iter()
                .filter(|scan| root.starts_with(&scan.repository_root))
                .max_by_key(|scan| scan.repository_root.components().count())
                .map_or(&[][..], |scan| scan.commits.as_slice());
            let Ok(mut scan) =
                scan_local_git_repository_cached(&root, project_id.as_deref(), cached_commits)
            else {
                failed_roots.insert(root);
                failed_scans = failed_scans.saturating_add(1);
                continue;
            };
            for commit in &mut scan.commits {
                commit.project_id.clone_from(&project_id);
            }
            self.replace_git_scan(&scan)?;
            refreshed_roots.insert(scan.repository_root.clone());
            available_scans.retain(|stored| stored.repository_root != scan.repository_root);
            available_scans.push(scan.clone());
            scans.push(scan);
        }
        // Now that every reachable root has been scanned, a path that vanished
        // is only a lost measurement when nothing else reached its repository.
        // Deleting a subdirectory of a project that still scans cleanly must
        // not report the repository as partially observed.
        for root in unresolved_roots {
            if !refreshed_roots.contains(&root) {
                failed_roots.insert(root);
                failed_scans = failed_scans.saturating_add(1);
            }
        }
        for stored in stored_scans {
            if failed_roots.contains(&stored.repository_root)
                && !refreshed_roots.contains(&stored.repository_root)
                && !scans
                    .iter()
                    .any(|scan| scan.repository_root == stored.repository_root)
            {
                scans.push(stored);
            }
        }
        self.delete_git_scans_except(&scans)?;
        let matches = match_trace_edits_to_commits(&trace_edits, &scans);
        let trace_coverage = self.trace_coverage()?;
        let committed_metric_ids = self.committed_metric_ids(device_id, &scans, identity_key)?;
        let build = build_code_change_metrics(
            now,
            device_id,
            &trace_edits,
            &scans,
            &matches,
            &committed_metric_ids,
            trace_coverage,
        )?;
        // Building can discover churn it declines to publish, so the coverage
        // it reports back supersedes the stored archive coverage.
        let trace_coverage = build.trace_coverage;
        let mut metrics = build.metrics;
        if failed_scans > 0 && !scans.is_empty() {
            for metric in &mut metrics {
                metric.git_coverage = CoverageStatus::Partial;
            }
        }
        // Commits older than the rolling scan window can no longer be rederived
        // from local history, so their already-materialized metrics are carried
        // forward instead of being deleted here and retired remotely.
        let retained = self
            .historical_commit_metrics(device_id, statsai_core::git_observation_start_day(now))?
            .into_iter()
            .map(|mut metric| {
                // A metric materialized before hosted login, against a keyless
                // endpoint, or under a different account carries an ID no other
                // device can derive. Re-keying it once an account key exists
                // restores cross-device deduplication for exactly the commits
                // the rolling scan can no longer reach and rebuild.
                if let (Some(identity_key), Some(repository_hash), Some(commit_hash)) = (
                    identity_key,
                    metric.repository_hash.as_deref(),
                    metric.commit_hash.as_deref(),
                ) {
                    metric.metric_id =
                        blinded_committed_metric_id(identity_key, repository_hash, commit_hash);
                }
                metric
            })
            .collect::<Vec<_>>();
        let rebuilt_ids = metrics
            .iter()
            .map(|metric| metric.metric_id.clone())
            .collect::<BTreeSet<_>>();
        metrics.extend(
            retained
                .into_iter()
                .filter(|metric| !rebuilt_ids.contains(&metric.metric_id)),
        );
        metrics.sort_by(|left, right| left.metric_id.cmp(&right.metric_id));
        // Two retained rows can re-key onto one blinded identity when an
        // earlier keyless refresh minted separate IDs for the same commit.
        metrics.dedup_by(|left, right| left.metric_id == right.metric_id);
        self.replace_matches_and_metrics(device_id, &matches, &metrics)?;
        let mut git_coverage = scans
            .iter()
            .map(|scan| scan.coverage)
            .reduce(CoverageStatus::combine)
            .unwrap_or(CoverageStatus::Unavailable);
        if failed_scans > 0 && !scans.is_empty() {
            git_coverage = CoverageStatus::Partial;
        }
        Ok(CodeChangeRefreshReport {
            repositories: scans.len() as u64,
            trace_edits: trace_edits.len() as u64,
            commits: scans.iter().map(|scan| scan.commits.len() as u64).sum(),
            matches: matches.len() as u64,
            metrics: metrics.len() as u64,
            trace_coverage,
            git_coverage,
        })
    }

    /// Commit-derived metrics for days the rolling Git scan no longer observes.
    ///
    /// The scan's cutoff is an instant on `observation_start_day`, so commits
    /// earlier in that same day are already unobservable. The boundary day is
    /// therefore retained too, and any commit still rescanned that day wins by
    /// metric ID over its retained copy.
    ///
    /// Only exact committed churn is carried forward. A trace-matched metric is
    /// an attribution claim that depends on both a commit and the archived
    /// trace it was matched against; once the commit ages out, that claim can
    /// no longer be reverified, corrected, or retired when the trace behind it
    /// is deleted. Attribution is therefore a rolling window while the
    /// committed totals it splits remain complete.
    fn historical_commit_metrics(
        &self,
        device_id: &str,
        observation_start_day: NaiveDate,
    ) -> Result<Vec<CodeChangeMetric>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload, commit_hash
            FROM code_change_metrics
            WHERE device_id = ?1
              AND day <= ?2
              AND kind = 'committed'
            ORDER BY metric_id
            "#,
        )?;
        let rows = statement.query_map(
            params![device_id, observation_start_day.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )?;
        let mut metrics = Vec::new();
        for row in rows {
            let (payload, commit_hash) = row?;
            let mut metric: CodeChangeMetric = serde_json::from_str(&payload)?;
            // The Git object ID is deliberately absent from the serialized
            // payload, so it is rehydrated from the column that does keep it.
            // Rewriting the row from the payload alone would null the column
            // and lose the identity that lets an opaque metric ID already
            // issued for that commit be reused if the day is ever rescanned.
            metric.commit_hash = commit_hash;
            metrics.push(metric);
        }
        Ok(metrics)
    }

    fn repository_projects(&self) -> Result<BTreeMap<PathBuf, Option<String>>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT
              path_label,
              CASE WHEN COUNT(DISTINCT project_id) = 1 THEN MIN(project_id) END
            FROM (
              SELECT
                json_extract(payload, '$.project.path_label') AS path_label,
                json_extract(payload, '$.project.project_id') AS project_id
              FROM usage_events
              UNION ALL
              SELECT
                json_extract(payload, '$.project.path_label') AS path_label,
                json_extract(payload, '$.project.project_id') AS project_id
              FROM usage_summaries
            )
            WHERE path_label IS NOT NULL AND trim(path_label) != ''
            GROUP BY path_label
            ORDER BY path_label
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut repositories = BTreeMap::new();
        for row in rows {
            let (path, project_id) = row?;
            repositories.insert(PathBuf::from(path), project_id);
        }
        Ok(repositories)
    }

    fn committed_metric_ids(
        &self,
        device_id: &str,
        scans: &[GitScan],
        identity_key: Option<&[u8; 32]>,
    ) -> Result<BTreeMap<String, String>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT repository_hash, commit_hash, metric_id
            FROM code_change_metrics
            WHERE device_id = ?1 AND kind = 'committed'
              AND repository_hash IS NOT NULL AND commit_hash IS NOT NULL
            "#,
        )?;
        let existing = statement
            .query_map([device_id], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let mut opaque_ids = BTreeMap::new();
        for commit in scans.iter().flat_map(|scan| &scan.commits) {
            if opaque_ids.contains_key(&commit.deduplication_id) {
                continue;
            }
            let commit_key = (commit.repository_hash.clone(), commit.commit_hash.clone());
            let metric_id = if let Some(identity_key) = identity_key {
                blinded_committed_metric_id(
                    identity_key,
                    &commit.repository_hash,
                    &commit.commit_hash,
                )
            } else {
                existing
                    .get(&commit_key)
                    .filter(|metric_id| metric_id.as_str() != commit.deduplication_id)
                    .cloned()
                    .map_or_else(new_opaque_committed_metric_id, Ok)?
            };
            opaque_ids.insert(commit.deduplication_id.clone(), metric_id);
        }
        Ok(opaque_ids)
    }

    fn trace_coverage(&self) -> Result<CoverageStatus> {
        let mut statement = self
            .conn
            .prepare("SELECT coverage FROM code_trace_coverage ORDER BY source_id, cache_key")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut coverage = None;
        for row in rows {
            let next = parse_coverage(&row?);
            coverage = Some(coverage.map_or(next, |current: CoverageStatus| current.combine(next)));
        }
        Ok(coverage.unwrap_or(CoverageStatus::Unavailable))
    }

    fn replace_git_scan(&self, scan: &GitScan) -> Result<()> {
        self.with_immediate_transaction(|| {
            // A repository whose identity changed (for example when an origin
            // remote is added) keeps its rows under the previous hash. Cascades
            // are declared but inert without `PRAGMA foreign_keys`, so commit
            // rows are removed explicitly.
            let mut superseded_statement = self.conn.prepare(
                "SELECT repository_hash FROM code_git_scans WHERE repository_path = ?1 AND repository_hash != ?2",
            )?;
            let superseded_hashes = superseded_statement
                .query_map(
                    params![
                        scan.repository_root.to_string_lossy(),
                        &scan.repository_hash
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(superseded_statement);
            for repository_hash in &superseded_hashes {
                self.delete_git_scan_rows_inner(repository_hash)?;
            }
            self.conn.execute(
                r#"
                INSERT INTO code_git_scans (repository_hash, repository_path, coverage, scanned_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(repository_hash) DO UPDATE SET
                  repository_path = excluded.repository_path,
                  coverage = excluded.coverage,
                  scanned_at = excluded.scanned_at
                "#,
                params![
                    &scan.repository_hash,
                    scan.repository_root.to_string_lossy(),
                    coverage_name(scan.coverage),
                    Utc::now().to_rfc3339(),
                ],
            )?;
            let incoming = scan
                .commits
                .iter()
                .map(|commit| commit.deduplication_id.as_str())
                .collect::<BTreeSet<_>>();
            let mut existing_statement = self.conn.prepare(
                "SELECT deduplication_id FROM code_git_commits WHERE repository_hash = ?1",
            )?;
            let existing = existing_statement
                .query_map([&scan.repository_hash], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for deduplication_id in existing {
                if !incoming.contains(deduplication_id.as_str()) {
                    self.conn.execute(
                        "DELETE FROM code_git_commits WHERE deduplication_id = ?1",
                        [&deduplication_id],
                    )?;
                }
            }
            for commit in &scan.commits {
                upsert_git_commit(&self.conn, commit)?;
            }
            Ok(())
        })
    }

    fn list_git_scans(&self) -> Result<Vec<GitScan>> {
        let mut scan_statement = self.conn.prepare(
            "SELECT repository_hash, repository_path, coverage FROM code_git_scans ORDER BY repository_hash",
        )?;
        let rows = scan_statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut scans = Vec::new();
        for row in rows {
            let (repository_hash, repository_path, coverage) = row?;
            let mut commit_statement = self.conn.prepare(
                "SELECT payload FROM code_git_commits WHERE repository_hash = ?1 ORDER BY committed_at, commit_hash",
            )?;
            let commits = commit_statement
                .query_map([&repository_hash], |row| row.get::<_, String>(0))?
                .map(|row| serde_json::from_str::<GitCommitChange>(&row?).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?;
            scans.push(GitScan {
                repository_root: PathBuf::from(repository_path),
                repository_hash,
                commits,
                coverage: parse_coverage(&coverage),
            });
        }
        Ok(scans)
    }

    fn delete_git_scans_except(&self, retained: &[GitScan]) -> Result<()> {
        let retained_hashes = retained
            .iter()
            .map(|scan| scan.repository_hash.as_str())
            .collect::<BTreeSet<_>>();
        self.with_immediate_transaction(|| {
            let mut statement = self
                .conn
                .prepare("SELECT repository_hash FROM code_git_scans")?;
            let stored_hashes = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for repository_hash in stored_hashes {
                if !retained_hashes.contains(repository_hash.as_str()) {
                    self.delete_git_scan_rows_inner(&repository_hash)?;
                }
            }
            Ok(())
        })
    }

    /// Removes a repository scan together with the commit rows it owns.
    ///
    /// The schema declares `ON DELETE CASCADE`, but the connection never
    /// enables `PRAGMA foreign_keys`, so the commits are deleted here.
    fn delete_git_scan_rows_inner(&self, repository_hash: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM code_git_commits WHERE repository_hash = ?1",
            [repository_hash],
        )?;
        self.conn.execute(
            "DELETE FROM code_git_scans WHERE repository_hash = ?1",
            [repository_hash],
        )?;
        Ok(())
    }

    fn replace_matches_and_metrics(
        &self,
        device_id: &str,
        matches: &[CodeChangeMatch],
        metrics: &[CodeChangeMetric],
    ) -> Result<()> {
        self.with_immediate_transaction(|| {
            self.conn.execute("DELETE FROM code_change_matches", [])?;
            for matched in matches {
                self.conn.execute(
                    r#"
                    INSERT INTO code_change_matches
                      (match_id, trace_edit_id, commit_deduplication_id, confidence, payload)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    "#,
                    params![
                        &matched.match_id,
                        &matched.trace_edit_id,
                        &matched.commit_deduplication_id,
                        confidence_name(matched.confidence),
                        serde_json::to_string(matched)?,
                    ],
                )?;
            }
            let incoming = metrics
                .iter()
                .map(|metric| metric.metric_id.as_str())
                .collect::<BTreeSet<_>>();
            let existing = self
                .conn
                .prepare("SELECT metric_id FROM code_change_metrics WHERE device_id = ?1")?
                .query_map([device_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for metric_id in existing {
                if !incoming.contains(metric_id.as_str()) {
                    self.conn.execute(
                        "DELETE FROM code_change_metrics WHERE metric_id = ?1",
                        [&metric_id],
                    )?;
                }
            }
            for metric in metrics {
                let payload = serde_json::to_string(metric)?;
                self.conn.execute(
                    r#"
                    INSERT INTO code_change_metrics
                      (metric_id, device_id, day, project_id, repository_hash, commit_hash,
                       kind, payload, dirty)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)
                    ON CONFLICT(metric_id) DO UPDATE SET
                      device_id = excluded.device_id,
                      day = excluded.day,
                      project_id = excluded.project_id,
                      repository_hash = excluded.repository_hash,
                      commit_hash = excluded.commit_hash,
                      kind = excluded.kind,
                      dirty = CASE
                        WHEN code_change_metrics.payload = excluded.payload
                        THEN code_change_metrics.dirty ELSE 1 END,
                      payload = excluded.payload
                    "#,
                    params![
                        &metric.metric_id,
                        &metric.device_id,
                        metric.day.to_string(),
                        &metric.project_id,
                        &metric.repository_hash,
                        &metric.commit_hash,
                        metric_kind_name(metric.kind),
                        payload,
                    ],
                )?;
            }
            Ok(())
        })
    }
}

fn upsert_git_commit(conn: &rusqlite::Connection, commit: &GitCommitChange) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO code_git_commits
          (deduplication_id, repository_hash, commit_hash, committed_at, project_id, payload)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(deduplication_id) DO UPDATE SET
          committed_at = excluded.committed_at,
          project_id = excluded.project_id,
          payload = excluded.payload
        "#,
        params![
            &commit.deduplication_id,
            &commit.repository_hash,
            &commit.commit_hash,
            commit.committed_at.to_rfc3339(),
            &commit.project_id,
            serde_json::to_string(commit)?,
        ],
    )?;
    Ok(())
}

fn coverage_name(coverage: CoverageStatus) -> &'static str {
    match coverage {
        CoverageStatus::Complete => "complete",
        CoverageStatus::Partial => "partial",
        CoverageStatus::Unavailable => "unavailable",
    }
}

fn parse_coverage(value: &str) -> CoverageStatus {
    match value {
        "complete" => CoverageStatus::Complete,
        "partial" => CoverageStatus::Partial,
        _ => CoverageStatus::Unavailable,
    }
}

fn confidence_name(confidence: AttributionConfidence) -> &'static str {
    match confidence {
        AttributionConfidence::High => "high",
        AttributionConfidence::Medium => "medium",
    }
}

fn metric_kind_name(kind: CodeChangeMetricKind) -> &'static str {
    match kind {
        CodeChangeMetricKind::AgentEdit => "agent_edit",
        CodeChangeMetricKind::Committed => "committed",
        CodeChangeMetricKind::TraceMatchedCommitted => "trace_matched_committed",
    }
}

fn new_opaque_committed_metric_id() -> Result<String> {
    let mut random = [0_u8; 32];
    getrandom::getrandom(&mut random).context("generate opaque committed metric id")?;
    Ok(format!("ccm_{}", hex::encode(random)))
}

fn blinded_committed_metric_id(
    identity_key: &[u8; 32],
    repository_hash: &str,
    commit_hash: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(identity_key)
        .expect("HMAC accepts fixed-length identity keys");
    mac.update(b"statsai.committed-metric.v1\0");
    update_hmac_field(&mut mac, repository_hash);
    update_hmac_field(&mut mac, commit_hash);
    format!("ccm_{}", hex::encode(mac.finalize().into_bytes()))
}

fn update_hmac_field(mac: &mut Hmac<Sha256>, value: &str) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use statsai_core::{
        CodeCategory, CodeLineCounts, SyncBatch, CODE_CHANGE_METRIC_SCHEMA_VERSION,
        SYNC_BATCH_SCHEMA_VERSION,
    };
    use std::{fs, path::Path, process::Command};
    use tempfile::TempDir;

    #[test]
    fn retiring_an_archive_file_removes_its_edits_whatever_the_record_id_looks_like() {
        let store = Store::in_memory().expect("open store");
        let source_id = statsai_core::SourceId("source".to_string());
        let edit = |cache_key: &str, source_record_id: &str, trace_edit_id: &str| TraceEdit {
            schema_version: statsai_core::TRACE_EDIT_SCHEMA_VERSION.to_string(),
            trace_edit_id: trace_edit_id.to_string(),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            cache_key: cache_key.to_string(),
            conversation_id: "conversation".to_string(),
            // Deliberately unlike `{cache_key}:{ordinal}` — the shape a future
            // provider might use, which the old prefix delete relied on.
            source_record_id: source_record_id.to_string(),
            occurred_at: None,
            project_id: None,
            repository_path: None,
            relative_path: PathBuf::from("src/lib.rs"),
            category: CodeCategory::Source,
            mutation_kind: statsai_core::MutationKind::StructuredEdit,
            counts: CodeLineCounts::classified(CodeCategory::Source, 1, 0),
            added_line_fingerprints: Vec::new(),
            deleted_line_fingerprints: Vec::new(),
        };
        let entry = |cache_key: &str| super::super::ScanFileStateEntry {
            cache_key: cache_key.to_string(),
            cache_signature: "signature".to_string(),
        };
        store
            .upsert_archive_conversations(&[statsai_core::ArchiveConversation {
                schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
                conversation_id: "conversation".to_string(),
                provider: "codex".to_string(),
                source_id: source_id.clone(),
                native_conversation_id: "thread".to_string(),
                title: None,
                project: None,
                started_at: None,
                updated_at: None,
                completeness: statsai_core::ArchiveCompleteness::Complete,
                missing_content_count: 0,
                missing_content_scope_id: None,
                discarded_source_record_ids: Vec::new(),
                superseded_conversation_ids: Vec::new(),
                items: Vec::new(),
            }])
            .expect("seed owning conversation");
        store
            .replace_archive_trace_edits_inner(
                &source_id,
                &[
                    entry("/archive/first.jsonl"),
                    entry("/archive/second.jsonl"),
                ],
                &[
                    edit("/archive/first.jsonl", "record|1", "edit-first"),
                    edit("/archive/second.jsonl", "record|2", "edit-second"),
                ],
                CoverageStatus::Complete,
            )
            .expect("seed trace edits");
        assert_eq!(store.list_trace_edits().expect("seeded").len(), 2);

        store
            .delete_archive_trace_entry_inner(&source_id, "/archive/first.jsonl")
            .expect("retire the first archive file");

        let remaining = store.list_trace_edits().expect("remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].trace_edit_id, "edit-second");
    }

    #[test]
    fn refresh_discovers_git_history_from_usage_projects_without_trace_edits() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        let payload = serde_json::json!({
            "project": {
                "project_id": "project-from-summary",
                "path_label": repository.path().to_string_lossy(),
            }
        });
        store
            .conn
            .execute(
                r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
                params!["summary", payload.to_string()],
            )
            .expect("insert project evidence");

        let report = store
            .refresh_code_changes("device")
            .expect("refresh changes");
        assert_eq!(report.repositories, 1);
        assert_eq!(report.commits, 1);
        let metrics = store.list_code_change_metrics(false).expect("metrics");
        assert!(metrics
            .iter()
            .any(|metric| metric.kind == CodeChangeMetricKind::Committed));

        run_test_git(
            repository.path(),
            &["remote", "add", "origin", "https://example.com/renamed.git"],
        );
        let refreshed = store
            .refresh_code_changes("device")
            .expect("refresh after identity change");
        assert_eq!(refreshed.repositories, 1);
        assert_eq!(refreshed.commits, 1);
        let scan_count: u64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
            .expect("scan count");
        assert_eq!(scan_count, 1);
        let committed_metrics = store
            .list_code_change_metrics(false)
            .expect("refreshed metrics")
            .into_iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .count();
        assert_eq!(committed_metrics, 1);
    }

    #[test]
    fn losing_the_committer_identity_retains_already_measured_commits() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        let payload = serde_json::json!({
            "project": {
                "project_id": "project",
                "path_label": repository.path().to_string_lossy(),
            }
        });
        store
            .conn
            .execute(
                r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
                params!["summary", payload.to_string()],
            )
            .expect("insert project evidence");

        let report = store
            .refresh_code_changes("device")
            .expect("refresh changes");
        assert_eq!(report.commits, 1);

        // Losing the configured identity means this scan cannot tell whose
        // commits these are. That is an unanswerable question, not an answer of
        // "none": the commits it already measured must survive, exactly as they
        // do when Git itself cannot be run.
        // Unsetting the repository value would fall back to the global one, so
        // the configured identity is emptied outright.
        run_test_git(repository.path(), &["config", "user.email", ""]);
        let refreshed = store
            .refresh_code_changes("device")
            .expect("refresh without identity");
        assert_eq!(refreshed.commits, 1);
        assert_eq!(refreshed.git_coverage, CoverageStatus::Partial);
        let committed = store
            .list_code_change_metrics(false)
            .expect("metrics")
            .into_iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .count();
        assert_eq!(committed, 1);
    }

    #[test]
    fn refresh_retires_scan_after_repository_loses_all_references() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "summary");
        let initial = store
            .refresh_code_changes("device")
            .expect("initial refresh");
        assert_eq!(initial.repositories, 1);

        store
            .conn
            .execute("DELETE FROM usage_summaries", [])
            .expect("remove project evidence");
        let refreshed = store
            .refresh_code_changes("device")
            .expect("refresh without references");

        assert_eq!(refreshed.repositories, 0);
        assert_eq!(refreshed.commits, 0);
        assert_eq!(refreshed.metrics, 0);
        let scan_count: u64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
            .expect("scan count");
        assert_eq!(scan_count, 0);
        assert!(store
            .list_code_change_metrics(false)
            .expect("metrics")
            .is_empty());
    }

    #[test]
    fn refresh_retains_cached_scan_when_referenced_repository_temporarily_fails() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "summary");
        store
            .refresh_code_changes("device")
            .expect("initial refresh");
        fs::rename(
            repository.path().join(".git"),
            repository.path().join(".git-disabled"),
        )
        .expect("disable repository metadata");

        let refreshed = store
            .refresh_code_changes("device")
            .expect("refresh during transient failure");

        assert_eq!(refreshed.repositories, 1);
        assert_eq!(refreshed.commits, 1);
        assert_eq!(refreshed.git_coverage, CoverageStatus::Partial);
        let scan_count: u64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM code_git_scans", [], |row| row.get(0))
            .expect("scan count");
        assert_eq!(scan_count, 1);
    }

    #[test]
    fn project_paths_outside_any_repository_do_not_degrade_git_coverage() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);
        // A scratch directory an agent ran in once, and a directory that has
        // since been removed. Neither was ever a repository.
        let scratch = TempDir::new().expect("scratch directory");
        let removed = TempDir::new().expect("removed directory");
        let removed_path = removed.path().to_path_buf();
        drop(removed);

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "summary");
        insert_project_evidence(&store, scratch.path(), "scratch", "scratch-summary");
        insert_project_evidence(&store, &removed_path, "removed", "removed-summary");

        let report = store
            .refresh_code_changes("device")
            .expect("refresh with non-repository paths");

        assert_eq!(report.repositories, 1);
        assert_eq!(report.commits, 1);
        assert_eq!(report.git_coverage, CoverageStatus::Complete);
        assert!(store
            .list_code_change_metrics(false)
            .expect("metrics")
            .iter()
            .all(|metric| metric.git_coverage == CoverageStatus::Complete));
    }

    #[test]
    fn a_deleted_subdirectory_of_a_healthy_repository_keeps_git_coverage_complete() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);
        let workspace = repository.path().join("fixtures");
        fs::create_dir_all(&workspace).expect("create subdirectory");

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "root-summary");
        insert_project_evidence(&store, &workspace, "project", "subdirectory-summary");
        assert_eq!(
            store
                .refresh_code_changes("device")
                .expect("initial refresh")
                .git_coverage,
            CoverageStatus::Complete
        );

        // The agent's working subdirectory is removed, but the repository it
        // lives in still scans cleanly through the recorded root path.
        fs::remove_dir_all(&workspace).expect("remove subdirectory");
        let refreshed = store
            .refresh_code_changes("device")
            .expect("refresh after the subdirectory was removed");

        assert_eq!(refreshed.repositories, 1);
        assert_eq!(refreshed.commits, 1);
        assert_eq!(
            refreshed.git_coverage,
            CoverageStatus::Complete,
            "a vanished subdirectory of a healthy repository is not a lost measurement"
        );
    }

    #[test]
    fn refresh_retains_committed_metrics_older_than_the_git_scan_window() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "summary");
        let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
        let aged_metric = |metric_id: &str, day: NaiveDate| CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: metric_id.to_string(),
            device_id: "device".to_string(),
            day,
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("old-commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 7, 2),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Unavailable,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics(
                "device",
                &[],
                &[
                    aged_metric(
                        "ccm_historical",
                        observation_start_day.pred_opt().expect("historical day"),
                    ),
                    // The scan's cutoff is an instant on this day, so commits
                    // earlier in it are no longer rescanned either.
                    aged_metric("ccm_boundary", observation_start_day),
                ],
            )
            .expect("seed aged metrics");

        store
            .refresh_code_changes("device")
            .expect("refresh after the commits aged out");

        let stored = store.list_code_change_metrics(false).expect("metrics");
        for metric_id in ["ccm_historical", "ccm_boundary"] {
            let retained = stored
                .iter()
                .find(|metric| metric.metric_id == metric_id)
                .unwrap_or_else(|| panic!("{metric_id} survives the rolling window"));
            assert_eq!(retained.counts.source_additions, 7);
        }
        assert!(stored.iter().any(|metric| {
            !metric.metric_id.starts_with("ccm_")
                || (metric.metric_id != "ccm_historical" && metric.metric_id != "ccm_boundary")
        }));
    }

    #[test]
    fn retained_committed_metrics_keep_the_commit_identity_their_payload_omits() {
        let store = Store::in_memory().expect("open store");
        let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
        let aged = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "ccm_aged".to_string(),
            device_id: "device".to_string(),
            day: observation_start_day.pred_opt().expect("historical day"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("aged-commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 5, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Unavailable,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&aged))
            .expect("seed aged metric");

        // Two refreshes: the first carries the metric forward, the second reads
        // back what the first one wrote.
        for _ in 0..2 {
            store
                .refresh_code_changes("device")
                .expect("refresh after the commit aged out");
        }

        let stored_commit_hash: Option<String> = store
            .conn
            .query_row(
                "SELECT commit_hash FROM code_change_metrics WHERE metric_id = 'ccm_aged'",
                [],
                |row| row.get(0),
            )
            .expect("retained metric row");
        assert_eq!(stored_commit_hash.as_deref(), Some("aged-commit"));
    }

    #[test]
    fn retained_committed_metrics_are_rekeyed_once_an_account_identity_key_exists() {
        let store = Store::in_memory().expect("open store");
        let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
        // Materialized before hosted login: a random ID no other device derives.
        let keyless = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "ccm_keyless".to_string(),
            device_id: "device".to_string(),
            day: observation_start_day.pred_opt().expect("historical day"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("aged-commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 9, 3),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Unavailable,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&keyless))
            .expect("seed keyless retained metric");

        let identity_key = [7_u8; 32];
        store
            .refresh_code_changes_with_identity_key("device", &identity_key)
            .expect("refresh after login");

        let expected_id = blinded_committed_metric_id(&identity_key, "repository", "aged-commit");
        let stored = store.list_code_change_metrics(false).expect("metrics");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].metric_id, expected_id);
        assert_eq!(stored[0].counts.source_additions, 9);
        assert!(
            !stored
                .iter()
                .any(|metric| metric.metric_id == "ccm_keyless"),
            "the underivable identity is retired rather than kept alongside"
        );

        // A second device on the same account converges on that identity.
        let other = Store::in_memory().expect("second store");
        let mut other_metric = keyless.clone();
        other_metric.metric_id = "ccm_other_random".to_string();
        other_metric.device_id = "device-b".to_string();
        other
            .replace_matches_and_metrics("device-b", &[], std::slice::from_ref(&other_metric))
            .expect("seed second device");
        other
            .refresh_code_changes_with_identity_key("device-b", &identity_key)
            .expect("refresh second device");
        assert_eq!(
            other.list_code_change_metrics(false).expect("metrics")[0].metric_id,
            expected_id
        );
    }

    #[test]
    fn trace_matched_metrics_are_not_carried_past_the_git_scan_window() {
        let store = Store::in_memory().expect("open store");
        let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
        let aged_match = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "aged-attribution".to_string(),
            device_id: "device".to_string(),
            day: observation_start_day.pred_opt().expect("historical day"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("aged-commit".to_string()),
            kind: CodeChangeMetricKind::TraceMatchedCommitted,
            counts: CodeLineCounts::classified(CodeCategory::Source, 4, 0),
            attribution_confidence: Some(AttributionConfidence::High),
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&aged_match))
            .expect("seed aged attribution");

        store
            .refresh_code_changes("device")
            .expect("refresh after the commit aged out");

        assert!(
            store
                .list_code_change_metrics(false)
                .expect("metrics")
                .is_empty(),
            "an attribution that can no longer be reverified is retired, not frozen"
        );
    }

    #[test]
    fn retiring_a_repository_scan_also_removes_its_commit_rows() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("open store");
        insert_project_evidence(&store, repository.path(), "project", "summary");
        store.refresh_code_changes("device").expect("first refresh");
        // Adding a remote changes the repository identity hash.
        run_test_git(
            repository.path(),
            &["remote", "add", "origin", "https://example.com/renamed.git"],
        );
        store
            .refresh_code_changes("device")
            .expect("refresh after identity change");

        let commit_count = |store: &Store| -> u64 {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM code_git_commits", [], |row| {
                    row.get(0)
                })
                .expect("commit count")
        };
        assert_eq!(
            commit_count(&store),
            1,
            "superseded identity leaves no rows"
        );

        store
            .conn
            .execute("DELETE FROM usage_summaries", [])
            .expect("remove project evidence");
        store
            .refresh_code_changes("device")
            .expect("refresh without references");

        assert_eq!(
            commit_count(&store),
            0,
            "retired scan leaves no commit rows"
        );
    }

    #[test]
    fn replacing_identical_metrics_is_idempotent_and_does_not_redirty_them() {
        let store = Store::in_memory().expect("open store");
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "metric-1".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };

        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
            .expect("first replace");
        store
            .mark_code_change_metrics_synced(std::slice::from_ref(&metric.metric_id))
            .expect("mark synced");
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
            .expect("second replace");

        assert_eq!(store.list_code_change_metrics(false).expect("all").len(), 1);
        assert!(store
            .list_code_change_metrics(true)
            .expect("dirty")
            .is_empty());
    }

    #[test]
    fn local_refresh_preserves_metrics_ingested_from_another_device() {
        let store = Store::in_memory().expect("open store");
        let remote_metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "remote-metric".to_string(),
            device_id: "remote-device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
            project_id: None,
            repository_hash: Some("remote-repository".to_string()),
            commit_hash: Some("remote-commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .ingest_sync_batch(&SyncBatch {
                schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
                batch_id: "remote-batch".to_string(),
                device_id: "remote-device".to_string(),
                sources: Vec::new(),
                accounts: Vec::new(),
                source_account_assignments: Vec::new(),
                subscriptions: Vec::new(),
                events: Vec::new(),
                summaries: Vec::new(),
                task_buckets: Vec::new(),
                task_verifications: Vec::new(),
                code_change_metrics: vec![remote_metric.clone()],
                authoritative_snapshot: None,
                created_at: Utc::now(),
            })
            .expect("ingest remote batch");
        let mut stale_local_metric = remote_metric.clone();
        stale_local_metric.metric_id = "stale-local-metric".to_string();
        stale_local_metric.device_id = "local-device".to_string();
        store
            .replace_matches_and_metrics(
                "local-device",
                &[],
                std::slice::from_ref(&stale_local_metric),
            )
            .expect("store stale local metric");

        store
            .refresh_code_changes("local-device")
            .expect("refresh local metrics");

        // The Git object ID survives the round trip even though the payload
        // omits it, so no reader sees a metric that under-reports itself.
        assert_eq!(
            store.list_code_change_metrics(false).expect("metrics"),
            vec![remote_metric]
        );
    }

    #[test]
    fn pending_metrics_are_tracked_independently_per_sync_target() {
        let store = Store::in_memory().expect("open store");
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "metric-per-target".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
            project_id: None,
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
            .expect("store metric");

        assert_eq!(
            store
                .pending_code_change_metrics_for_sync(
                    "http",
                    "target-a",
                    std::slice::from_ref(&metric)
                )
                .expect("target a pending")
                .len(),
            1
        );
        store
            .record_code_change_metrics_synced("http", "target-a", std::slice::from_ref(&metric))
            .expect("record target a");
        assert!(store
            .pending_code_change_metrics_for_sync("http", "target-a", std::slice::from_ref(&metric))
            .expect("target a settled")
            .is_empty());
        assert_eq!(
            store
                .pending_code_change_metrics_for_sync(
                    "http",
                    "target-b",
                    std::slice::from_ref(&metric)
                )
                .expect("target b pending")
                .len(),
            1
        );
    }

    #[test]
    fn ingesting_duplicate_cross_device_commit_metrics_deduplicates_by_metric_id() {
        let store = Store::in_memory().expect("open store");
        let mut first = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "repository-plus-commit".to_string(),
            device_id: "device-a".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
            project_id: None,
            repository_hash: Some("repository".to_string()),
            commit_hash: Some("commit".to_string()),
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&first))
            .expect("ingest first");
        first.device_id = "device-b".to_string();
        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&first))
            .expect("ingest duplicate");

        let stored = store.list_code_change_metrics(false).expect("metrics");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].device_id, "device-b");
    }

    #[test]
    fn ingesting_corrected_metric_replaces_stale_columns_and_payload() {
        let store = Store::in_memory().expect("open store");
        let original = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "corrected-metric".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("original date"),
            project_id: None,
            repository_hash: None,
            commit_hash: None,
            kind: CodeChangeMetricKind::AgentEdit,
            counts: CodeLineCounts::classified(CodeCategory::Source, 1, 0),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Partial,
            git_coverage: CoverageStatus::Partial,
        };
        let corrected = CodeChangeMetric {
            day: NaiveDate::from_ymd_opt(2026, 8, 2).expect("corrected date"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            kind: CodeChangeMetricKind::TraceMatchedCommitted,
            counts: CodeLineCounts::classified(CodeCategory::Source, 5, 2),
            attribution_confidence: Some(AttributionConfidence::High),
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
            ..original.clone()
        };

        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&original))
            .expect("ingest original metric");
        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&corrected))
            .expect("ingest corrected metric");

        assert_eq!(
            store.list_code_change_metrics(false).expect("metrics"),
            vec![corrected]
        );
        let stored_columns = store
            .conn
            .query_row(
                r#"
                SELECT day, project_id, repository_hash, kind, dirty
                FROM code_change_metrics
                WHERE metric_id = 'corrected-metric'
                "#,
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                },
            )
            .expect("stored columns");
        assert_eq!(
            stored_columns,
            (
                "2026-08-02".to_string(),
                Some("project".to_string()),
                Some("repository".to_string()),
                "trace_matched_committed".to_string(),
                0,
            )
        );
    }

    #[test]
    fn ingesting_identical_metric_preserves_existing_dirty_state() {
        let store = Store::in_memory().expect("open store");
        let metric = CodeChangeMetric {
            schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: "unchanged-metric".to_string(),
            device_id: "device".to_string(),
            day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
            project_id: Some("project".to_string()),
            repository_hash: Some("repository".to_string()),
            commit_hash: None,
            kind: CodeChangeMetricKind::Committed,
            counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
            attribution_confidence: None,
            trace_coverage: CoverageStatus::Complete,
            git_coverage: CoverageStatus::Complete,
        };
        store
            .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
            .expect("store dirty local metric");

        store
            .ingest_code_change_metrics_inner(std::slice::from_ref(&metric))
            .expect("ingest identical metric");

        assert_eq!(
            store.list_code_change_metrics(true).expect("dirty metrics"),
            vec![metric]
        );
    }

    #[test]
    fn committed_metric_ids_are_stable_per_user_across_stores() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        run_test_git(repository.path(), &["add", "main.rs"]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let first = Store::in_memory().expect("first store");
        let legacy_scan = scan_local_git_repository_cached(repository.path(), None, &[])
            .expect("scan legacy commit");
        let legacy_commit = legacy_scan.commits.first().expect("legacy commit");
        let legacy_metric_id = legacy_commit.deduplication_id.clone();
        first
            .replace_matches_and_metrics(
                "device-a",
                &[],
                &[CodeChangeMetric {
                    schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
                    metric_id: legacy_metric_id.clone(),
                    device_id: "device-a".to_string(),
                    day: legacy_commit.committed_at.date_naive(),
                    project_id: Some("project".to_string()),
                    repository_hash: Some(legacy_commit.repository_hash.clone()),
                    commit_hash: Some(legacy_commit.commit_hash.clone()),
                    kind: CodeChangeMetricKind::Committed,
                    counts: CodeLineCounts::default(),
                    attribution_confidence: None,
                    trace_coverage: CoverageStatus::Unavailable,
                    git_coverage: CoverageStatus::Complete,
                }],
            )
            .expect("seed legacy derivable metric id");
        insert_project_evidence(&first, repository.path(), "project", "first-summary");
        let user_identity_key = [7_u8; 32];
        first
            .refresh_code_changes_with_identity_key("device-a", &user_identity_key)
            .expect("first refresh");
        let first_id = first
            .list_code_change_metrics(false)
            .expect("first metrics")
            .into_iter()
            .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .expect("first committed metric")
            .metric_id;
        first
            .refresh_code_changes_with_identity_key("device-a", &user_identity_key)
            .expect("repeat refresh");
        let repeated_id = first
            .list_code_change_metrics(false)
            .expect("repeated metrics")
            .into_iter()
            .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .expect("repeated committed metric")
            .metric_id;

        let second = Store::in_memory().expect("second store");
        insert_project_evidence(&second, repository.path(), "project", "second-summary");
        second
            .refresh_code_changes_with_identity_key("device-b", &user_identity_key)
            .expect("second refresh");
        let second_id = second
            .list_code_change_metrics(false)
            .expect("second metrics")
            .into_iter()
            .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .expect("second committed metric")
            .metric_id;

        assert_eq!(first_id, repeated_id);
        assert_ne!(first_id, legacy_metric_id);
        assert_eq!(first_id, second_id);
        assert!(first_id.starts_with("ccm_"));
        assert!(second_id.starts_with("ccm_"));

        let third = Store::in_memory().expect("third store");
        insert_project_evidence(&third, repository.path(), "project", "third-summary");
        third
            .refresh_code_changes_with_identity_key("device-c", &[9_u8; 32])
            .expect("third refresh");
        let third_id = third
            .list_code_change_metrics(false)
            .expect("third metrics")
            .into_iter()
            .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .expect("third committed metric")
            .metric_id;
        assert_ne!(first_id, third_id);
    }

    #[test]
    fn shared_git_roots_do_not_assign_all_commits_to_one_nested_project() {
        let repository = TempDir::new().expect("temporary repository");
        run_test_git(repository.path(), &["init", "-q"]);
        run_test_git(
            repository.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_test_git(repository.path(), &["config", "user.name", "Test"]);
        let project_a = repository.path().join("packages/a");
        let project_b = repository.path().join("packages/b");
        fs::create_dir_all(&project_a).expect("create project a");
        fs::create_dir_all(&project_b).expect("create project b");
        fs::write(project_a.join("a.rs"), "pub fn a() {}\n").expect("write project a");
        fs::write(project_b.join("b.rs"), "pub fn b() {}\n").expect("write project b");
        run_test_git(repository.path(), &["add", "."]);
        run_test_git(repository.path(), &["commit", "-qm", "initial"]);

        let store = Store::in_memory().expect("store");
        insert_project_evidence(&store, &project_a, "project-a", "summary-a");
        insert_project_evidence(&store, &project_b, "project-b", "summary-b");

        let report = store
            .refresh_code_changes("device")
            .expect("refresh shared root");
        let committed = store
            .list_code_change_metrics(false)
            .expect("metrics")
            .into_iter()
            .filter(|metric| metric.kind == CodeChangeMetricKind::Committed)
            .collect::<Vec<_>>();

        assert_eq!(report.repositories, 1);
        assert_eq!(committed.len(), 1);
        assert!(committed[0].project_id.is_none());
    }

    #[test]
    fn repository_projects_preserve_only_unambiguous_project_ids_per_path() {
        let store = Store::in_memory().expect("store");
        let ambiguous_path = Path::new("/workspace/ambiguous");
        insert_project_evidence(&store, ambiguous_path, "project-a", "ambiguous-a");
        insert_project_evidence(&store, ambiguous_path, "project-b", "ambiguous-b");
        let consistent_path = Path::new("/workspace/consistent");
        insert_project_evidence(&store, consistent_path, "project-c", "consistent-a");
        insert_project_evidence(&store, consistent_path, "project-c", "consistent-b");
        insert_optional_project_evidence(&store, consistent_path, None, "consistent-null");
        let unidentified_path = Path::new("/workspace/unidentified");
        insert_optional_project_evidence(&store, unidentified_path, None, "unidentified");

        let projects = store.repository_projects().expect("repository projects");

        assert_eq!(projects.get(ambiguous_path), Some(&None));
        assert_eq!(
            projects.get(consistent_path),
            Some(&Some("project-c".to_string()))
        );
        assert_eq!(projects.get(unidentified_path), Some(&None));
    }

    fn insert_project_evidence(store: &Store, path: &Path, project_id: &str, summary_id: &str) {
        insert_optional_project_evidence(store, path, Some(project_id), summary_id);
    }

    fn insert_optional_project_evidence(
        store: &Store,
        path: &Path,
        project_id: Option<&str>,
        summary_id: &str,
    ) {
        let payload = serde_json::json!({
            "project": {
                "project_id": project_id,
                "path_label": path.to_string_lossy(),
            }
        });
        store
            .conn
            .execute(
                r#"
                INSERT INTO usage_summaries
                  (summary_id, provider, source_id, observed_at, total_tokens, payload)
                VALUES (?1, 'codex', 'source', '2026-08-01T00:00:00Z', 0, ?2)
                "#,
                params![summary_id, payload.to_string()],
            )
            .expect("insert project evidence");
    }

    fn run_test_git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success());
    }
}
