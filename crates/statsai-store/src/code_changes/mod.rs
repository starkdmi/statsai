mod git;
mod ids;

pub(crate) use git::*;
pub(crate) use ids::*;

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
        // Identity hashes of repositories that failed to scan, kept beside the
        // roots so a relocated repository is still recognised as one that failed
        // rather than one that vanished.
        let mut failed_repository_hashes = BTreeSet::<String>::new();
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
            // Resolved through the surviving ancestors rather than by
            // `canonicalize` alone, so a repository whose directory is gone
            // still matches the physical root its scan was stored under.
            let cache_path = statsai_core::canonical_path(&path);
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
            // Commits already measured stay this user's work after `user.email`
            // changes, so a scan matches every identity its repository has been
            // seen under rather than only the one configured now.
            //
            // A repository is looked up under both of its names, because either
            // can change on its own while the repository stays the same work.
            // Adding an origin remote re-keys it, so only the root still matches;
            // moving the worktree relocates it, so only the hash still matches.
            // Either lookup alone loses the identities in the case the other
            // covers, and the scan would then drop in-window commits made under
            // an earlier address and retire them remotely. The root is compared
            // exactly rather than by prefix so a repository nested inside another
            // never inherits its parent's identities.
            let resolved_hash = statsai_core::repository_identity_hash(&root).ok();
            let known_identities = stored_scans
                .iter()
                .filter(|scan| {
                    scan.repository_root == root
                        || resolved_hash
                            .as_ref()
                            .is_some_and(|hash| &scan.repository_hash == hash)
                })
                .flat_map(|scan| scan.committer_identities.iter().cloned())
                .collect::<BTreeSet<_>>();
            let Ok(mut scan) = scan_local_git_repository_cached(
                &root,
                project_id.as_deref(),
                cached_commits,
                &known_identities,
            ) else {
                // Recorded under both names. A repository that moved and then
                // failed to scan is reached by its new root, while its stored
                // snapshot still carries the old one, so retention keyed on the
                // root alone would drop the snapshot and retire history this
                // refresh is in no position to rebuild.
                if let Some(hash) = resolved_hash {
                    failed_repository_hashes.insert(hash);
                }
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
        for stored in stored_scans.iter() {
            if (failed_roots.contains(&stored.repository_root)
                || failed_repository_hashes.contains(&stored.repository_hash))
                && !refreshed_roots.contains(&stored.repository_root)
                && !scans
                    .iter()
                    .any(|scan| scan.repository_root == stored.repository_root)
            {
                scans.push(stored.clone());
            }
        }
        // Repositories nothing reaches any more. Their aged metrics are past the
        // scan window, so no rebuild can retire them and they would otherwise be
        // carried forward and republished in every authoritative snapshot
        // forever.
        //
        // A repository answers to two names that change independently: adding an
        // origin remote re-keys it, and moving the worktree relocates it. Rather
        // than exempting a stored repository from retirement whenever one of its
        // names survives, each is claimed by the fresh scan that *is* that
        // repository, and its aged metrics are rewritten onto the hash it goes by
        // now.
        //
        // Merely exempting it is not enough. A re-keyed repository leaves aged
        // metrics under a hash it has stopped using, and once that scan row is
        // gone no later refresh has any record of the old hash, so those rows
        // match nothing, survive every retirement decision, and are republished in
        // the authoritative snapshot forever. Rewriting keeps the lineage, so
        // retirement only ever compares against hashes still in use.
        //
        // Lineage is established by the same root, or failing that by a shared
        // commit: commit hashes are globally unique, so an overlap proves two
        // scans are the same repository even when both of its names changed in one
        // refresh.
        let retained_hashes = scans
            .iter()
            .map(|scan| scan.repository_hash.as_str())
            .collect::<BTreeSet<_>>();
        let mut current_repository_hashes = BTreeMap::<String, String>::new();
        for stored in &stored_scans {
            if retained_hashes.contains(stored.repository_hash.as_str()) {
                continue;
            }
            let stored_commits = stored
                .commits
                .iter()
                .map(|commit| commit.commit_hash.as_str())
                .collect::<BTreeSet<_>>();
            let claim = scans
                .iter()
                .find(|scan| scan.repository_root == stored.repository_root)
                .or_else(|| {
                    // Only for a stored root that is actually gone. Forks and
                    // second checkouts of the same upstream share commits while
                    // both still exist, so overlap alone would let one claim the
                    // other's aged metrics when a checkout is dropped. A
                    // repository that moved has left its old root behind, which
                    // distinguishes the two.
                    if stored.repository_root.exists() {
                        return None;
                    }
                    scans.iter().find(|scan| {
                        scan.commits
                            .iter()
                            .any(|commit| stored_commits.contains(commit.commit_hash.as_str()))
                    })
                });
            if let Some(claim) = claim {
                current_repository_hashes.insert(
                    stored.repository_hash.clone(),
                    claim.repository_hash.clone(),
                );
            }
        }
        let retired_repository_hashes = stored_scans
            .iter()
            .filter(|stored| {
                !retained_hashes.contains(stored.repository_hash.as_str())
                    && !current_repository_hashes.contains_key(&stored.repository_hash)
            })
            .map(|stored| stored.repository_hash.clone())
            .collect::<BTreeSet<_>>();
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
            .filter(|metric| {
                metric
                    .repository_hash
                    .as_deref()
                    .is_none_or(|hash| !retired_repository_hashes.contains(hash))
            })
            .map(|mut metric| {
                // Follow the repository through a re-key, so an aged metric is
                // never left filed under a hash that no scan will mention again.
                // The published ID is derived from the repository hash below, so
                // this has to happen before that: rewriting afterwards would keep
                // publishing an ID built from a hash the repository no longer has.
                if let Some(current) = metric
                    .repository_hash
                    .as_deref()
                    .and_then(|hash| current_repository_hashes.get(hash))
                {
                    metric.repository_hash = Some(current.clone());
                }
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
    ///
    /// Every aged metric is returned; the caller decides which repositories are
    /// still worth carrying, because only it knows which of them were retired
    /// this run as opposed to merely re-identified.
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
            // Identities accumulate: the scan carries in everything this
            // repository was already known by, and a repository whose identity
            // hash changed above has its set re-homed under the new hash.
            for identity_hash in &scan.committer_identities {
                self.conn.execute(
                    r#"
                    INSERT OR IGNORE INTO code_git_identities
                      (repository_hash, identity_hash, first_seen_at)
                    VALUES (?1, ?2, ?3)
                    "#,
                    params![&scan.repository_hash, identity_hash, Utc::now().to_rfc3339()],
                )?;
            }
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
            let mut identity_statement = self.conn.prepare(
                "SELECT identity_hash FROM code_git_identities WHERE repository_hash = ?1 ORDER BY identity_hash",
            )?;
            let committer_identities = identity_statement
                .query_map([&repository_hash], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?;
            scans.push(GitScan {
                repository_root: PathBuf::from(repository_path),
                repository_hash,
                commits,
                committer_identities,
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

    /// Removes a repository scan together with the rows it owns.
    ///
    /// The schema declares `ON DELETE CASCADE`, but the connection never
    /// enables `PRAGMA foreign_keys`, so the commits and remembered committer
    /// identities are deleted here.
    fn delete_git_scan_rows_inner(&self, repository_hash: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM code_git_commits WHERE repository_hash = ?1",
            [repository_hash],
        )?;
        self.conn.execute(
            "DELETE FROM code_git_identities WHERE repository_hash = ?1",
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

#[cfg(test)]
mod tests;
