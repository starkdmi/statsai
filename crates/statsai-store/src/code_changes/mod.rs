mod git;
mod ids;
mod refresh;
mod scans;

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
}

#[cfg(test)]
mod tests;
