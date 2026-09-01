use super::*;

mod build;
mod summaries;

pub(crate) use build::*;
pub(crate) use summaries::*;

impl Store {
    pub fn sync_rollup_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sync_rollups", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    pub fn dirty_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups WHERE dirty = 1 ORDER BY updated_at, summary_id",
        )
    }

    pub fn all_sync_rollup_summaries(&self) -> Result<Vec<UsageSummary>> {
        self.ensure_current_sync_rollup_versions()?;
        self.sync_rollup_summaries_by_sql(
            "SELECT payload FROM sync_rollups ORDER BY updated_at, summary_id",
        )
    }

    pub fn mark_sync_rollups_synced(&self, summary_ids: &[SummaryId]) -> Result<()> {
        if summary_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_sync_rollups_synced_in_transaction(summary_ids)
        })
    }

    pub(crate) fn mark_sync_rollups_synced_in_transaction(
        &self,
        summary_ids: &[SummaryId],
    ) -> Result<()> {
        for summary_id in summary_ids {
            self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1",
                params![&summary_id.0],
            )?;
        }
        Ok(())
    }

    pub fn mark_all_sync_rollups_dirty(&self) -> Result<u64> {
        let updated = self.conn.execute(
            "UPDATE sync_rollups SET dirty = 1, updated_at = ?1",
            params![Utc::now().to_rfc3339()],
        )? as u64;
        Ok(updated)
    }

    pub fn rebuild_sync_rollups(&self) -> Result<u64> {
        let events = self.events()?;
        let keys: BTreeSet<_> = events.iter().map(sync_rollup_bucket_key).collect();

        self.with_immediate_transaction(|| {
            self.conn.execute("DELETE FROM sync_rollups", [])?;
            self.refresh_sync_rollups_for_keys(&keys)?;
            Ok(keys.len() as u64)
        })
    }

    pub(crate) fn ensure_current_sync_rollup_versions(&self) -> Result<()> {
        let stale_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_rollups
             WHERE json_extract(payload, '$.metadata.summary_format') = 'daily_rollup.v1'
               AND COALESCE(json_extract(payload, '$.metadata.summary_version'), '') != ?1",
            params![SYNC_ROLLUP_SUMMARY_VERSION],
            |row| row.get(0),
        )?;
        if stale_count > 0 {
            self.rebuild_sync_rollups()?;
        }
        Ok(())
    }

    pub fn sync_rollup_period_stats(&self, cutoff_day: NaiveDate) -> Result<RollupPeriodStats> {
        let mut tokens = 0u64;
        let mut requests = 0u64;
        for summary in self.all_sync_rollup_summaries()? {
            let day = summary_sync_day(&summary);
            if day < cutoff_day {
                continue;
            }
            tokens = tokens.saturating_add(summary.usage.computed_total());
            requests = requests.saturating_add(summary.usage.requests.unwrap_or(0));
        }
        Ok(RollupPeriodStats { tokens, requests })
    }

    pub fn usage_event_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        Ok(self.conn.query_row(
            r#"
            SELECT
              COALESCE(SUM(total_tokens), 0),
              COUNT(*)
            FROM usage_events
            WHERE started_at >= ?1
            "#,
            params![since.to_rfc3339()],
            |row| {
                Ok(RollupPeriodStats {
                    tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                })
            },
        )?)
    }

    pub fn reportable_summary_period_stats_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE datetime(COALESCE(period_start, observed_at)) >= datetime(?1)
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![since.to_rfc3339()],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn reportable_summary_period_stats_since_day(
        &self,
        cutoff_day: NaiveDate,
    ) -> Result<RollupPeriodStats> {
        let cutoff_day = cutoff_day.format("%Y-%m-%d").to_string();
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(total_tokens), 0),
                  COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0)
                FROM usage_summaries
                WHERE substr(COALESCE(period_start, observed_at), 1, 10) >= ?1
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
                  AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
                  AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)?.max(0) as u64,
                        requests: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn snapshot_rollup_view(
        &self,
        sink: &str,
        target: &str,
        week_cutoff: NaiveDate,
        today_cutoff: NaiveDate,
    ) -> Result<SnapshotRollupView> {
        let week_cutoff = week_cutoff.format("%Y-%m-%d").to_string();
        let today_cutoff = today_cutoff.format("%Y-%m-%d").to_string();
        let week = self.sync_rollup_stats_since_day(&week_cutoff)?;
        let today = self.sync_rollup_stats_since_day(&today_cutoff)?;
        let (pending_count, pending_days) = self.pending_sync_rollup_counts(sink, target)?;
        Ok(SnapshotRollupView {
            pending_count,
            pending_days,
            today,
            week,
        })
    }

    fn sync_rollup_stats_since_day(&self, cutoff_day: &str) -> Result<RollupPeriodStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
                  COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0)
                FROM sync_rollups
                WHERE day_key >= ?1
                "#,
                params![cutoff_day],
                |row| {
                    Ok(RollupPeriodStats {
                        tokens: row.get::<_, i64>(0)? as u64,
                        requests: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .map_err(Into::into)
    }

    const SYNC_ROLLUP_HASH_RECONCILE_KEY: &str = "sync_rollup_sync_hashes_reconciled_v1";

    pub fn reconcile_sync_rollup_sync_hashes_if_needed(&self) -> Result<u64> {
        if self
            .metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY)?
            .as_deref()
            == Some("1")
        {
            return Ok(0);
        }
        let updated = self.reconcile_sync_rollup_sync_hashes()?;
        self.set_metadata_value(Self::SYNC_ROLLUP_HASH_RECONCILE_KEY, "1")?;
        Ok(updated)
    }

    pub fn reconcile_sync_rollup_sync_hashes(&self) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT summary_id, payload FROM sync_rollups")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;

        self.with_immediate_transaction(|| {
            let mut updated = 0u64;
            for (summary_id, payload) in &rows {
                let summary: UsageSummary = serde_json::from_str(payload)?;
                let payload_hash = summary_sync_payload_hash(&summary)?;
                updated += self.conn.execute(
                    "UPDATE sync_rollups SET payload_hash = ?1 WHERE summary_id = ?2 AND payload_hash != ?1",
                    params![payload_hash, summary_id],
                )? as u64;
            }
            Ok(updated)
        })
    }

    fn pending_sync_rollup_counts(&self, sink: &str, target: &str) -> Result<(u64, u64)> {
        let rollups = self
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(sanitize_summary_for_default_http_sync)
            .collect::<Vec<_>>();
        let pending = self.pending_summaries_for_sync(sink, target, &rollups)?;
        let days = collect_pending_summary_days(pending.iter());
        Ok((pending.len() as u64, days.len() as u64))
    }

    pub fn reconcile_sync_rollup_dirty_flags(&self, sink: &str, target: &str) -> Result<u64> {
        self.ensure_current_sync_rollup_versions()?;
        let summaries = self.all_sync_rollup_summaries()?;
        self.with_immediate_transaction(|| {
            self.reconcile_sync_rollup_dirty_flags_in_transaction(sink, target, &summaries)
        })
    }

    pub(crate) fn reconcile_sync_rollup_dirty_flags_in_transaction(
        &self,
        sink: &str,
        target: &str,
        summaries: &[UsageSummary],
    ) -> Result<u64> {
        let mut cleared = 0u64;
        for summary in summaries {
            let payload_hash = summary_sync_payload_hash(summary)?;
            if self.entity_requires_sync(
                sink,
                target,
                "summary",
                &summary.summary_id.0,
                &payload_hash,
            )? {
                continue;
            }
            cleared += self.conn.execute(
                "UPDATE sync_rollups SET dirty = 0 WHERE summary_id = ?1 AND dirty = 1",
                params![&summary.summary_id.0],
            )? as u64;
        }
        Ok(cleared)
    }

    pub fn compute_daily_rollup(&self, date: &str, device_id: &str) -> Result<DailyRollup> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT payload FROM usage_events
            WHERE started_at >= ?1 AND started_at < ?2
            "#,
        )?;
        let end_date = {
            let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
            (parsed + chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string()
        };
        let rows = stmt.query_map(params![date, &end_date], |row| row.get::<_, String>(0))?;

        let mut total_input = 0u64;
        let mut total_cache_create = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_output = 0u64;
        let mut total_reasoning = 0u64;
        let mut total_tokens = 0u64;
        let mut total_events = 0u64;
        let mut sessions = std::collections::BTreeSet::new();
        let mut estimated_cost = CostAccumulator::default();
        let mut by_provider: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut by_account: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();

        for row in rows {
            let event: UsageEvent = serde_json::from_str(&row?)?;
            total_input = total_input.saturating_add(event.usage.input_tokens.unwrap_or(0));
            total_cache_create =
                total_cache_create.saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
            total_cache_read =
                total_cache_read.saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
            total_output = total_output.saturating_add(event.usage.output_tokens.unwrap_or(0));
            total_reasoning =
                total_reasoning.saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
            total_tokens = total_tokens.saturating_add(event.usage.computed_total());
            total_events = total_events.saturating_add(1);
            sessions.insert(event.session.session_id.clone());

            estimated_cost.add_estimated(&event.cost);

            let provider_entry = by_provider
                .entry(event.provider.clone())
                .or_insert_with(|| serde_json::json!({"tokens": 0, "events": 0}));
            provider_entry["tokens"] = serde_json::json!(provider_entry["tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(event.usage.computed_total()));
            provider_entry["events"] = serde_json::json!(provider_entry["events"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1));

            let account_key = event
                .provider_account_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "unassigned".to_string());
            let account_entry = by_account.entry(account_key).or_insert_with(
                || serde_json::json!({"tokens": 0, "events": 0, "provider": event.provider}),
            );
            account_entry["tokens"] = serde_json::json!(account_entry["tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(event.usage.computed_total()));
            account_entry["events"] = serde_json::json!(account_entry["events"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1));
        }

        Ok(DailyRollup {
            schema_version: statsai_core::DAILY_ROLLUP_SCHEMA_VERSION.to_string(),
            date: date.to_string(),
            device_id: device_id.to_string(),
            total_input_tokens: total_input,
            total_cache_creation_tokens: total_cache_create,
            total_cache_read_tokens: total_cache_read,
            total_output_tokens: total_output,
            total_reasoning_tokens: total_reasoning,
            total_tokens,
            total_events,
            total_sessions: sessions.len() as u64,
            estimated_cost_usd: estimated_cost.cents_rounded(),
            estimated_cost_micro_usd: estimated_cost.micro_usd(),
            by_provider: Some(serde_json::to_string(&by_provider)?),
            by_account: Some(serde_json::to_string(&by_account)?),
            updated_at: chrono::Utc::now(),
        })
    }

    pub fn upsert_daily_rollup(&self, rollup: &DailyRollup) -> Result<()> {
        let payload = serde_json::to_string(rollup)?;
        self.conn.execute(
            r#"
            INSERT INTO daily_rollups (
              date, device_id, total_tokens, total_events, total_sessions,
              estimated_cost_usd, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(date, device_id) DO UPDATE SET
              total_tokens = excluded.total_tokens,
              total_events = excluded.total_events,
              total_sessions = excluded.total_sessions,
              estimated_cost_usd = excluded.estimated_cost_usd,
              payload = excluded.payload
            "#,
            params![
                &rollup.date,
                &rollup.device_id,
                safe_u64_to_i64(rollup.total_tokens),
                safe_u64_to_i64(rollup.total_events),
                safe_u64_to_i64(rollup.total_sessions),
                rollup.estimated_cost_usd,
                &payload,
            ],
        )?;
        Ok(())
    }

    pub fn daily_rollups_between(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<DailyRollup>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload FROM daily_rollups WHERE date >= ?1 AND date <= ?2 ORDER BY date",
        )?;
        let rows = stmt.query_map(params![start_date, end_date], |row| row.get::<_, String>(0))?;
        let mut rollups = Vec::new();
        for row in rows {
            rollups.push(serde_json::from_str(&row?)?);
        }
        Ok(rollups)
    }

    pub fn delete_rollups_for_device(&self, device_id: &str) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM daily_rollups WHERE device_id = ?1",
            params![device_id],
        )? as u64;
        Ok(deleted)
    }

    fn sync_rollup_summaries_by_sql(&self, sql: &str) -> Result<Vec<UsageSummary>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(serde_json::from_str(&row?)?);
        }
        Ok(summaries)
    }

    pub(crate) fn refresh_sync_rollups_for_keys(
        &self,
        keys: &BTreeSet<SyncRollupBucketKey>,
    ) -> Result<()> {
        self.refresh_sync_rollups_for_keys_counted(keys).map(|_| ())
    }

    pub(crate) fn refresh_sync_rollups_for_keys_counted(
        &self,
        keys: &BTreeSet<SyncRollupBucketKey>,
    ) -> Result<u64> {
        let mut refreshed = 0u64;
        for key in keys {
            if self.refresh_sync_rollup_for_key(key)? {
                refreshed += 1;
            }
        }
        Ok(refreshed)
    }

    fn refresh_sync_rollup_for_key(&self, key: &SyncRollupBucketKey) -> Result<bool> {
        let events = self.sync_rollup_events(key)?;
        if events.is_empty() {
            let deleted = self.conn.execute(
                "DELETE FROM sync_rollups WHERE summary_id = ?1",
                params![sync_rollup_summary_id(key).0],
            )?;
            return Ok(deleted > 0);
        }

        let summary = build_sync_rollup_summary(&events);
        let payload = serde_json::to_string(&summary)?;
        let payload_hash = summary_sync_payload_hash(&summary)?;
        let existing: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT payload_hash, dirty FROM sync_rollups WHERE summary_id = ?1",
                params![&summary.summary_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if existing
            .as_ref()
            .is_some_and(|(existing_hash, _)| existing_hash == &payload_hash)
        {
            return Ok(false);
        }

        let dirty = existing.as_ref().map_or(1, |(_, dirty)| (*dirty).max(1));
        self.conn.execute(
            r#"
            INSERT INTO sync_rollups (
              summary_id, provider, source_id, provider_account_id, day_key,
              observed_at, updated_at, payload_hash, dirty, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(summary_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              day_key = excluded.day_key,
              observed_at = excluded.observed_at,
              updated_at = excluded.updated_at,
              payload_hash = excluded.payload_hash,
              dirty = excluded.dirty,
              payload = excluded.payload
            "#,
            params![
                &summary.summary_id.0,
                &summary.provider,
                &summary.source_id.0,
                summary.provider_account_id.as_ref().map(|id| id.0.as_str()),
                &key.day_key,
                summary.observed_at.to_rfc3339(),
                Utc::now().to_rfc3339(),
                &payload_hash,
                dirty,
                &payload,
            ],
        )?;
        Ok(true)
    }

    fn sync_rollup_events(&self, key: &SyncRollupBucketKey) -> Result<Vec<UsageEvent>> {
        let start = format!("{}T00:00:00+00:00", key.day_key);
        let end = {
            let day = NaiveDate::parse_from_str(&key.day_key, "%Y-%m-%d")?;
            format!(
                "{}T00:00:00+00:00",
                (day + chrono::Duration::days(1)).format("%Y-%m-%d")
            )
        };
        let sql = if key.provider_account_id.is_some() {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id = ?3
              AND started_at >= ?4
              AND started_at < ?5
            ORDER BY started_at, event_id
            "#
        } else {
            r#"
            SELECT payload FROM usage_events
            WHERE provider = ?1
              AND source_id = ?2
              AND provider_account_id IS NULL
              AND started_at >= ?3
              AND started_at < ?4
            ORDER BY started_at, event_id
            "#
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut events: Vec<UsageEvent> = Vec::new();
        if let Some(provider_account_id) = key.provider_account_id.as_deref() {
            let rows = stmt.query_map(
                params![
                    &key.provider,
                    &key.source_id,
                    provider_account_id,
                    &start,
                    &end
                ],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if let Ok(event) = serde_json::from_str(&row?) {
                    events.push(event);
                }
            }
        } else {
            let rows = stmt.query_map(
                params![&key.provider, &key.source_id, &start, &end],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if let Ok(event) = serde_json::from_str(&row?) {
                    events.push(event);
                }
            }
        }
        events.retain(|event| sync_rollup_project_key(event.project.as_ref()) == key.project_key);
        Ok(events)
    }

    pub(crate) fn delete_sync_rollups_for_sources_in_tx(
        &self,
        source_ids: &[SourceId],
    ) -> Result<u64> {
        let mut deleted = 0u64;
        for source_id in source_ids {
            deleted += self.conn.execute(
                "DELETE FROM sync_rollups WHERE source_id = ?1",
                params![&source_id.0],
            )? as u64;
        }
        Ok(deleted)
    }
}
