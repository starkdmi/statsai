use super::*;

fn merge_source_totals(existing: &mut SourceUsageTotals, incoming: SourceUsageTotals) {
    if incoming.tokens > existing.tokens {
        *existing = incoming;
        return;
    }
    if incoming.tokens == existing.tokens {
        existing.events = existing.events.max(incoming.events);
        existing.estimated_cost_cents =
            max_optional_i64(existing.estimated_cost_cents, incoming.estimated_cost_cents);
    }
}

fn merge_additive_source_totals(existing: &mut SourceUsageTotals, incoming: SourceUsageTotals) {
    existing.events = existing.events.saturating_add(incoming.events);
    existing.tokens = existing.tokens.saturating_add(incoming.tokens);
    existing.estimated_cost_cents =
        match (existing.estimated_cost_cents, incoming.estimated_cost_cents) {
            (Some(existing), Some(incoming)) => Some(existing.saturating_add(incoming)),
            (Some(existing), None) => Some(existing),
            (None, Some(incoming)) => Some(incoming),
            (None, None) => None,
        };
}

fn max_optional_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

impl Store {
    pub fn upsert_source(&self, source: &SourceLocation) -> Result<()> {
        let payload = serde_json::to_string(source)?;
        self.conn.execute(
            r#"
            INSERT INTO sources (source_id, provider, source_kind, location_origin, payload, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(source_id) DO UPDATE SET
              provider = excluded.provider,
              source_kind = excluded.source_kind,
              location_origin = excluded.location_origin,
              payload = excluded.payload,
              updated_at = excluded.updated_at
            "#,
            params![
                &source.source_id.0,
                &source.provider,
                format!("{:?}", source.source_kind),
                format!("{:?}", source.location_origin),
                &payload,
                source.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceLocation>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM sources ORDER BY provider, source_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut sources = Vec::new();
        for row in rows {
            sources.push(serde_json::from_str(&row?)?);
        }
        Ok(sources)
    }

    pub fn event_counts_by_source(&self) -> Result<HashMap<String, u64>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT source_id, COUNT(*)
            FROM usage_events
            GROUP BY source_id
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut counts = HashMap::new();
        for row in rows {
            let (source_id, count) = row?;
            counts.insert(source_id, count.max(0) as u64);
        }
        Ok(counts)
    }

    pub fn usage_totals_by_source(&self) -> Result<HashMap<String, SourceUsageTotals>> {
        let mut totals = HashMap::new();
        let mut rollup_stmt = self.conn.prepare(
            r#"
            SELECT
              source_id,
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0),
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER) * 10000,
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER) * 10000
              ))
            FROM sync_rollups
            GROUP BY source_id
            "#,
        )?;
        let rollup_rows = rollup_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row
                        .get::<_, Option<i64>>(3)?
                        .map(micro_usd_to_cents_rounded),
                },
            ))
        })?;
        for row in rollup_rows {
            let (source_id, source_totals) = row?;
            totals.insert(source_id, source_totals);
        }

        let mut summary_stmt = self.conn.prepare(
            r#"
            SELECT
              source_id,
              COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0),
              COALESCE(SUM(total_tokens), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER) * 10000,
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER) * 10000
              ))
            FROM usage_summaries
            GROUP BY source_id
            "#,
        )?;
        let summary_rows = summary_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row
                        .get::<_, Option<i64>>(3)?
                        .map(micro_usd_to_cents_rounded),
                },
            ))
        })?;
        for row in summary_rows {
            let (source_id, summary_totals) = row?;
            match totals.get_mut(&source_id) {
                Some(existing) => merge_source_totals(existing, summary_totals),
                None => {
                    totals.insert(source_id, summary_totals);
                }
            }
        }
        Ok(totals)
    }

    pub fn menu_usage_totals_by_provider(&self) -> Result<HashMap<String, SourceUsageTotals>> {
        let mut totals = HashMap::new();
        let mut rollup_stmt = self.conn.prepare(
            r#"
            SELECT
              provider,
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.requests') AS INTEGER)), 0),
              COALESCE(SUM(CAST(json_extract(payload, '$.usage.total_tokens') AS INTEGER)), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER) * 10000,
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER) * 10000
              ))
            FROM sync_rollups
            GROUP BY provider
            "#,
        )?;
        let rollup_rows = rollup_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row
                        .get::<_, Option<i64>>(3)?
                        .map(micro_usd_to_cents_rounded),
                },
            ))
        })?;
        for row in rollup_rows {
            let (provider, provider_totals) = row?;
            totals.insert(provider, provider_totals);
        }

        let mut summary_stmt = self.conn.prepare(
            r#"
            SELECT
              provider,
              COALESCE(SUM(COALESCE(CAST(json_extract(payload, '$.usage.requests') AS INTEGER), 1)), 0),
              COALESCE(SUM(total_tokens), 0),
              SUM(COALESCE(
                CAST(json_extract(payload, '$.cost.provider_reported_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.provider_reported_usd') AS INTEGER) * 10000,
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_micro_usd') AS INTEGER),
                CAST(json_extract(payload, '$.cost.estimated_api_equivalent_usd') AS INTEGER) * 10000
              ))
            FROM usage_summaries
            WHERE COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'daily_rollup.v1'
              AND COALESCE(json_extract(payload, '$.metadata.summary_format'), '') != 'claude_stats_cache'
              AND COALESCE(json_extract(payload, '$.source.source_kind'), '') != 'local_summary'
            GROUP BY provider
            "#,
        )?;
        let summary_rows = summary_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceUsageTotals {
                    events: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    estimated_cost_cents: row
                        .get::<_, Option<i64>>(3)?
                        .map(micro_usd_to_cents_rounded),
                },
            ))
        })?;
        for row in summary_rows {
            let (provider, summary_totals) = row?;
            let entry = totals.entry(provider).or_default();
            merge_additive_source_totals(entry, summary_totals);
        }
        Ok(totals)
    }

    pub fn source(&self, source_id: &SourceId) -> Result<Option<SourceLocation>> {
        self.conn
            .query_row(
                "SELECT payload FROM sources WHERE source_id = ?1",
                params![&source_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| Ok(serde_json::from_str(&payload)?))
            .transpose()
    }

    pub fn set_source_enabled(
        &self,
        source_id: &SourceId,
        enabled: bool,
    ) -> Result<Option<SourceLocation>> {
        let Some(mut source) = self.source(source_id)? else {
            return Ok(None);
        };
        source.enabled = enabled;
        source.updated_at = Utc::now();
        self.upsert_source(&source)?;
        Ok(Some(source))
    }

    pub fn delete_source(&self, source_id: &SourceId) -> Result<bool> {
        self.with_immediate_transaction(|| {
            self.conn.execute(
                "DELETE FROM source_account_assignments WHERE source_id = ?1",
                params![&source_id.0],
            )?;
            Ok(self.conn.execute(
                "DELETE FROM sources WHERE source_id = ?1",
                params![&source_id.0],
            )? > 0)
        })
    }
}
