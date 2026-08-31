use super::*;

impl Store {
    pub(crate) fn replace_git_scan(&self, scan: &GitScan) -> Result<()> {
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

    pub(crate) fn list_git_scans(&self) -> Result<Vec<GitScan>> {
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

    pub(crate) fn delete_git_scans_except(&self, retained: &[GitScan]) -> Result<()> {
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
    pub(crate) fn delete_git_scan_rows_inner(&self, repository_hash: &str) -> Result<()> {
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

    pub(crate) fn replace_matches_and_metrics(
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
