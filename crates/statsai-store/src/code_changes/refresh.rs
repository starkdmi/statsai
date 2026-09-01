use super::*;

impl Store {
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

    pub(crate) fn refresh_code_changes_inner(
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
    pub(crate) fn historical_commit_metrics(
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

    pub(crate) fn repository_projects(&self) -> Result<BTreeMap<PathBuf, Option<String>>> {
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

    pub(crate) fn committed_metric_ids(
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

    pub(crate) fn trace_coverage(&self) -> Result<CoverageStatus> {
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
}
