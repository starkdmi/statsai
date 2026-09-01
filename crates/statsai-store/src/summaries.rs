use super::*;

impl Store {
    pub fn upsert_summary(&self, summary: &UsageSummary) -> Result<bool> {
        let payload = serde_json::to_string(summary)?;
        let changed = self.conn.execute(
            r#"
            INSERT INTO usage_summaries (
              summary_id, provider, source_id, provider_account_id, period_start, period_end,
              observed_at, total_tokens, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(summary_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              period_start = excluded.period_start,
              period_end = excluded.period_end,
              observed_at = excluded.observed_at,
              total_tokens = excluded.total_tokens,
              payload = excluded.payload
            "#,
            params![
                &summary.summary_id.0,
                &summary.provider,
                &summary.source_id.0,
                summary.provider_account_id.as_ref().map(|id| id.0.as_str()),
                summary.period_start.map(|date| date.to_rfc3339()),
                summary.period_end.map(|date| date.to_rfc3339()),
                summary.observed_at.to_rfc3339(),
                safe_u64_to_i64(summary.usage.computed_total()),
                &payload,
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn upsert_summaries(&self, summaries: &[UsageSummary]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut changed = 0u64;
            for summary in summaries {
                if self.upsert_summary(summary)? {
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }

    pub fn delete_summaries_for_source_file_hashes(
        &self,
        source_id: &SourceId,
        file_hashes: &[String],
    ) -> Result<u64> {
        if file_hashes.is_empty() {
            return Ok(0);
        }

        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for file_hash in file_hashes {
                deleted += self.conn.execute(
                    r#"
                    DELETE FROM usage_summaries
                    WHERE source_id = ?1
                      AND json_extract(payload, '$.parse_evidence.source_file_path_hash') = ?2
                    "#,
                    params![&source_id.0, file_hash],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn summaries(&self) -> Result<Vec<UsageSummary>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM usage_summaries ORDER BY observed_at, summary_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub fn summaries_for_source(&self, source_id: &SourceId) -> Result<Vec<UsageSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM usage_summaries WHERE source_id = ?1 ORDER BY observed_at, summary_id",
        )?;
        let rows = stmt.query_map(params![&source_id.0], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub fn rewrite_summaries(&self, summaries: &[UsageSummary]) -> Result<u64> {
        if summaries.is_empty() {
            return Ok(0);
        }
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut changed = 0u64;
            for summary in summaries {
                if self.upsert_summary(summary)? {
                    changed += 1;
                }
            }
            Ok(changed)
        })();

        match result {
            Ok(changed) => {
                commit_transaction(&self.conn)?;
                Ok(changed)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn summaries_after(
        &self,
        cursor: Option<(&DateTime<Utc>, &str)>,
    ) -> Result<Vec<UsageSummary>> {
        let sql = if cursor.is_some() {
            r#"
            SELECT payload FROM usage_summaries
            WHERE observed_at > ?1 OR (observed_at = ?1 AND summary_id > ?2)
            ORDER BY observed_at, summary_id
            "#
        } else {
            "SELECT payload FROM usage_summaries ORDER BY observed_at, summary_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut summaries = Vec::new();
        if let Some((observed_at, summary_id)) = cursor {
            let rows = stmt.query_map(params![observed_at.to_rfc3339(), summary_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                summaries.push(serde_json::from_str(&row?)?);
            }
        } else {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                summaries.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(summaries)
    }

    pub fn delete_summaries_for_sources(&self, source_ids: &[SourceId]) -> Result<u64> {
        self.with_immediate_transaction(|| {
            let mut deleted = 0u64;
            for source_id in source_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_summaries WHERE source_id = ?1",
                    params![&source_id.0],
                )? as u64;
            }
            Ok(deleted)
        })
    }

    pub fn delete_summaries(&self, summary_ids: &[SummaryId]) -> Result<u64> {
        begin_immediate_transaction_with_retry(&self.conn)?;
        let result = (|| {
            let mut deleted = 0u64;
            for summary_id in summary_ids {
                deleted += self.conn.execute(
                    "DELETE FROM usage_summaries WHERE summary_id = ?1",
                    params![&summary_id.0],
                )? as u64;
            }
            Ok(deleted)
        })();

        match result {
            Ok(deleted) => {
                commit_transaction(&self.conn)?;
                Ok(deleted)
            }
            Err(error) => {
                rollback(&self.conn);
                Err(error)
            }
        }
    }

    pub fn summary_count(&self) -> Result<u64> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM usage_summaries", [], |row| row.get(0))?;
        Ok(count as u64)
    }
}
