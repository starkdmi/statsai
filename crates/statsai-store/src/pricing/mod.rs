//! Versioned automatic repricing of persisted normalized usage.

use super::{is_daily_rollup_summary, summary_period_bounds, Store, SyncRollupBucketKey};
use anyhow::Result;
use rusqlite::params;
use serde::de::DeserializeOwned;
use statsai_core::{CostAccumulator, CostInfo, ModelInfo, UsageEvent, UsageSummary};
use statsai_pricing::{
    estimate_cost_at, overlay_estimated_cost, pricing_changes_between, unknown_cost,
    PRICING_CATALOG_VERSION, PRICING_RULESET_VERSION,
};
use std::collections::{BTreeSet, HashMap};
use std::fmt;

pub const APPLIED_PRICING_RULESET_VERSION_KEY: &str = "pricing.applied_ruleset_version";
pub const APPLIED_PRICING_CATALOG_VERSION_KEY: &str = "pricing.applied_catalog_version";

const EVENT_PAGE_SIZE: usize = 256;
const SUMMARY_PAGE_SIZE: usize = 128;
const TASK_SPAN_PAGE_SIZE: usize = 128;

/// Counts produced by one automatic pricing-ruleset application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepricingReport {
    pub examined_events: u64,
    pub changed_events: u64,
    pub skipped_unreadable_events: u64,
    pub examined_summaries: u64,
    pub changed_summaries: u64,
    pub skipped_unreadable_summaries: u64,
    pub refreshed_rollups: u64,
    pub changed_task_spans: u64,
    pub skipped_unreadable_spans: u64,
    pub rebuilt_work_items: u64,
    pub already_current: bool,
}

impl RepricingReport {
    #[must_use]
    pub fn did_work(&self) -> bool {
        self.changed_events > 0
            || self.changed_summaries > 0
            || self.refreshed_rollups > 0
            || self.changed_task_spans > 0
            || self.rebuilt_work_items > 0
    }
}

impl fmt::Display for RepricingReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.already_current {
            return write!(
                formatter,
                "pricing already at ruleset {PRICING_RULESET_VERSION} ({PRICING_CATALOG_VERSION})"
            );
        }
        write!(
            formatter,
            "repriced store to ruleset {PRICING_RULESET_VERSION} ({PRICING_CATALOG_VERSION}): examined_events={} changed_events={} skipped_unreadable_events={} changed_summaries={} refreshed_rollups={}",
            self.examined_events,
            self.changed_events,
            self.skipped_unreadable_events,
            self.changed_summaries,
            self.refreshed_rollups
        )?;
        let skipped_other = self
            .skipped_unreadable_summaries
            .saturating_add(self.skipped_unreadable_spans);
        if skipped_other > 0 {
            write!(
                formatter,
                " skipped_unreadable_summaries={} skipped_unreadable_spans={}",
                self.skipped_unreadable_summaries, self.skipped_unreadable_spans
            )?;
        }
        Ok(())
    }
}

impl Store {
    /// Ensures persisted estimated prices match this binary's pricing ruleset.
    ///
    /// This is not called from [`Store::open`]. The selected StatsAI binary
    /// owns the decision so development launchers compiled against a different
    /// catalog cannot reprice a store they did not select.
    pub fn ensure_current_pricing(&self) -> Result<RepricingReport> {
        self.ensure_pricing_ruleset(PRICING_RULESET_VERSION, PRICING_CATALOG_VERSION)
    }

    /// Returns the last successfully applied pricing ruleset, if recorded.
    pub fn applied_pricing_ruleset_version(&self) -> Result<Option<u64>> {
        super::snapshot::parse_applied_pricing_ruleset_value(
            self.metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY)?
                .as_deref(),
        )
    }

    pub(crate) fn ensure_pricing_ruleset(
        &self,
        current_ruleset: u64,
        catalog_version: &str,
    ) -> Result<RepricingReport> {
        match self.applied_pricing_ruleset_version()? {
            Some(applied) if applied > current_ruleset => {
                return Err(forward_pricing_version_error(applied, current_ruleset));
            }
            Some(applied) if applied == current_ruleset => {
                return Ok(RepricingReport {
                    already_current: true,
                    ..RepricingReport::default()
                });
            }
            _ => {}
        }

        self.with_immediate_transaction(|| {
            match self.applied_pricing_ruleset_version()? {
                Some(applied) if applied > current_ruleset => {
                    return Err(forward_pricing_version_error(applied, current_ruleset));
                }
                Some(applied) if applied == current_ruleset => {
                    return Ok(RepricingReport {
                        already_current: true,
                        ..RepricingReport::default()
                    });
                }
                _ => {}
            }

            let mut report = RepricingReport::default();
            let mut dirty_keys = BTreeSet::new();

            self.reprice_events_in_tx(&mut report, &mut dirty_keys)?;
            self.reprice_summaries_in_tx(&mut report)?;
            report.refreshed_rollups = self.refresh_sync_rollups_for_keys_counted(&dirty_keys)?;
            self.reprice_task_spans_in_tx(&mut report)?;

            self.set_metadata_value(
                APPLIED_PRICING_RULESET_VERSION_KEY,
                &current_ruleset.to_string(),
            )?;
            self.set_metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY, catalog_version)?;
            Ok(report)
        })
    }
}

fn forward_pricing_version_error(applied: u64, supported: u64) -> anyhow::Error {
    anyhow::anyhow!(
        "database pricing ruleset version {applied} is newer than this StatsAI binary supports ({supported}); upgrade StatsAI or use a compatible database"
    )
}

struct RepricePage<T> {
    items: Vec<T>,
    last_id: Option<String>,
    fetched: usize,
    skipped: u64,
}

fn decode_id_payload_page<T: DeserializeOwned>(
    rows: impl Iterator<Item = rusqlite::Result<(String, String)>>,
) -> Result<RepricePage<T>> {
    let mut page = RepricePage {
        items: Vec::new(),
        last_id: None,
        fetched: 0,
        skipped: 0,
    };
    for row in rows {
        let (id, payload) = row?;
        page.fetched += 1;
        page.last_id = Some(id);
        match serde_json::from_str(&payload) {
            Ok(item) => page.items.push(item),
            Err(_) => page.skipped += 1,
        }
    }
    Ok(page)
}

impl Store {
    fn reprice_events_in_tx(
        &self,
        report: &mut RepricingReport,
        dirty_keys: &mut BTreeSet<SyncRollupBucketKey>,
    ) -> Result<()> {
        let mut after: Option<String> = None;
        loop {
            let page = self.event_page_after(after.as_deref(), EVENT_PAGE_SIZE)?;
            if page.fetched == 0 {
                break;
            }
            after = page.last_id;
            report.skipped_unreadable_events += page.skipped;
            for event in page.items {
                report.examined_events += 1;
                maybe_fail_after_event_writes(report.changed_events)?;
                if let Some(updated) = reprice_event(&event) {
                    dirty_keys.insert(self.update_event_cost_payload(&updated)?);
                    report.changed_events += 1;
                }
            }
            if page.fetched < EVENT_PAGE_SIZE {
                break;
            }
        }
        Ok(())
    }

    fn event_page_after(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RepricePage<UsageEvent>> {
        if let Some(after) = after {
            let mut statement = self.conn.prepare(
                "SELECT event_id, payload FROM usage_events WHERE event_id > ?1 ORDER BY event_id LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        } else {
            let mut statement = self
                .conn
                .prepare("SELECT event_id, payload FROM usage_events ORDER BY event_id LIMIT ?1")?;
            let rows = statement.query_map(params![limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        }
    }

    fn reprice_summaries_in_tx(&self, report: &mut RepricingReport) -> Result<()> {
        let mut after: Option<String> = None;
        loop {
            let page = self.summary_page_after(after.as_deref(), SUMMARY_PAGE_SIZE)?;
            if page.fetched == 0 {
                break;
            }
            after = page.last_id;
            report.skipped_unreadable_summaries += page.skipped;
            for summary in page.items {
                report.examined_summaries += 1;
                if is_daily_rollup_summary(&summary) {
                    continue;
                }
                if let Some(updated) = reprice_summary(&summary) {
                    self.upsert_summary(&updated)?;
                    report.changed_summaries += 1;
                }
            }
            if page.fetched < SUMMARY_PAGE_SIZE {
                break;
            }
        }
        Ok(())
    }

    fn summary_page_after(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RepricePage<UsageSummary>> {
        if let Some(after) = after {
            let mut statement = self.conn.prepare(
                "SELECT summary_id, payload FROM usage_summaries WHERE summary_id > ?1 ORDER BY summary_id LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        } else {
            let mut statement = self.conn.prepare(
                "SELECT summary_id, payload FROM usage_summaries ORDER BY summary_id LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        }
    }

    fn reprice_task_spans_in_tx(&self, report: &mut RepricingReport) -> Result<()> {
        let mut after: Option<String> = None;
        let mut changed_buckets = BTreeSet::new();
        loop {
            let page = self.linked_task_span_page_after(after.as_deref(), TASK_SPAN_PAGE_SIZE)?;
            if page.fetched == 0 {
                break;
            }
            after = page.last_id;
            report.skipped_unreadable_spans += page.skipped;
            let event_ids = page
                .items
                .iter()
                .flat_map(|span| {
                    span.linked_event_ids
                        .iter()
                        .map(|event_id| event_id.0.clone())
                })
                .collect::<BTreeSet<_>>();
            let events = self.events_by_ids(&event_ids)?;
            let mut updated_spans = Vec::new();
            for mut span in page.items {
                let Some((cents, micro)) =
                    estimated_cost_for_loaded_events(&span.linked_event_ids, &events)
                else {
                    continue;
                };
                if span.estimated_cost_usd != cents || span.estimated_cost_micro_usd != micro {
                    span.estimated_cost_usd = cents;
                    span.estimated_cost_micro_usd = micro;
                    changed_buckets.insert(span.project_bucket.clone());
                    updated_spans.push(span);
                    report.changed_task_spans += 1;
                }
            }
            if !updated_spans.is_empty() {
                self.upsert_task_spans_in_tx(&updated_spans)?;
            }
            if page.fetched < TASK_SPAN_PAGE_SIZE {
                break;
            }
        }
        if !changed_buckets.is_empty() {
            report.rebuilt_work_items =
                self.rebuild_task_work_items_for_project_buckets(&changed_buckets)?;
        }
        Ok(())
    }

    fn linked_task_span_page_after(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RepricePage<statsai_core::TaskSpan>> {
        let sql = if after.is_some() {
            "SELECT span_id, payload FROM task_spans
             WHERE span_id > ?1
               AND EXISTS (
                 SELECT 1 FROM task_span_event_links WHERE span_id = task_spans.span_id
               )
             ORDER BY span_id LIMIT ?2"
        } else {
            "SELECT span_id, payload FROM task_spans
             WHERE EXISTS (
               SELECT 1 FROM task_span_event_links WHERE span_id = task_spans.span_id
             )
             ORDER BY span_id LIMIT ?1"
        };
        if let Some(after) = after {
            let mut statement = self.conn.prepare(sql)?;
            let rows = statement.query_map(params![after, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        } else {
            let mut statement = self.conn.prepare(sql)?;
            let rows = statement.query_map(params![limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            decode_id_payload_page(rows)
        }
    }

    fn events_by_ids(&self, event_ids: &BTreeSet<String>) -> Result<HashMap<String, UsageEvent>> {
        let mut events = HashMap::new();
        let ids = event_ids.iter().cloned().collect::<Vec<_>>();
        for chunk in ids.chunks(128) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT event_id, payload FROM usage_events WHERE event_id IN ({placeholders})"
            );
            let mut statement = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = statement.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (event_id, payload) = row?;
                if let Ok(event) = serde_json::from_str(&payload) {
                    events.insert(event_id, event);
                }
            }
        }
        Ok(events)
    }
}

fn estimated_cost_for_loaded_events(
    event_ids: &[statsai_core::EventId],
    events: &HashMap<String, UsageEvent>,
) -> Option<(Option<i64>, Option<i64>)> {
    if event_ids.is_empty() {
        return None;
    }
    let mut estimated = CostAccumulator::default();
    let mut found = 0usize;
    for event_id in event_ids {
        if let Some(event) = events.get(&event_id.0) {
            estimated.add_estimated(&event.cost);
            found += 1;
        }
    }
    if found == 0 {
        return Some((None, None));
    }
    Some((estimated.cents_rounded(), estimated.micro_usd()))
}

fn reprice_event(event: &UsageEvent) -> Option<UsageEvent> {
    let estimated = estimate_cost_at(
        &event.provider,
        event.model.as_ref(),
        &event.usage,
        &event.session.started_at,
    );
    let cost = overlay_estimated_cost(&event.cost, estimated);
    if cost == event.cost {
        return None;
    }
    let mut updated = event.clone();
    updated.cost = cost;
    Some(updated)
}

/// Overlays this binary's estimated pricing onto a summary.
///
/// Daily rollups are left unchanged. Returns the original summary when
/// estimated fields already match.
#[must_use]
pub fn apply_current_estimated_pricing(summary: UsageSummary) -> UsageSummary {
    if is_daily_rollup_summary(&summary) {
        return summary;
    }
    reprice_summary(&summary).unwrap_or(summary)
}

fn reprice_summary(summary: &UsageSummary) -> Option<UsageSummary> {
    let (period_start, period_end) = summary_period_bounds(summary);
    let pricing_at = summary.period_end.unwrap_or(summary.observed_at);
    let mut updated = summary.clone();
    let mut changed = false;

    if !updated.models.is_empty() {
        for model_usage in &mut updated.models {
            let next = estimated_summary_cost(
                &updated.provider,
                Some(&model_usage.model),
                &model_usage.usage,
                period_start.date_naive(),
                period_end.date_naive(),
                pricing_at,
                &model_usage.cost,
            );
            if next != model_usage.cost {
                model_usage.cost = next;
                changed = true;
            }
        }
    }

    let next = estimated_summary_cost(
        &updated.provider,
        updated.model.as_ref(),
        &updated.usage,
        period_start.date_naive(),
        period_end.date_naive(),
        pricing_at,
        &updated.cost,
    );
    if next != updated.cost {
        updated.cost = next;
        changed = true;
    }

    changed.then_some(updated)
}

fn estimated_summary_cost(
    provider: &str,
    model: Option<&ModelInfo>,
    usage: &statsai_core::UsageCounts,
    period_start: chrono::NaiveDate,
    period_end: chrono::NaiveDate,
    pricing_at: chrono::DateTime<chrono::Utc>,
    existing: &CostInfo,
) -> CostInfo {
    let estimated = if summary_crosses_pricing_boundary(model, period_start, period_end) {
        unknown_cost()
    } else {
        estimate_cost_at(provider, model, usage, &pricing_at)
    };
    overlay_estimated_cost(existing, estimated)
}

fn summary_crosses_pricing_boundary(
    model: Option<&ModelInfo>,
    period_start: chrono::NaiveDate,
    period_end: chrono::NaiveDate,
) -> bool {
    model_pricing_names(model)
        .into_iter()
        .any(|name| pricing_changes_between(&name, period_start, period_end))
}

fn model_pricing_names(model: Option<&ModelInfo>) -> Vec<String> {
    let Some(model) = model else {
        return Vec::new();
    };
    [
        model.normalized_name.as_deref(),
        model.name.as_deref(),
        model.provider_model_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(ToOwned::to_owned)
    .collect()
}

fn maybe_fail_after_event_writes(changed_events: u64) -> Result<()> {
    #[cfg(test)]
    {
        FAIL_AFTER_EVENT_WRITES.with(|cell| {
            if cell.get().is_some_and(|limit| changed_events >= limit) {
                anyhow::bail!("injected repricing failure after {changed_events} event writes")
            } else {
                Ok(())
            }
        })?;
    }
    let _ = changed_events;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_EVENT_WRITES: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_repricing_after_event_writes(limit: Option<u64>) {
    FAIL_AFTER_EVENT_WRITES.with(|cell| cell.set(limit));
}

#[cfg(test)]
mod tests;
