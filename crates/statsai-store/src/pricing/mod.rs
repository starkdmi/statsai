//! Versioned automatic repricing of persisted normalized usage.

mod source;

pub use source::{PricingOffer, PricingRefreshState, StoredPricing, UnsupportedPricingFormat};

use super::{is_daily_rollup_summary, summary_period_bounds, Store, SyncRollupBucketKey};
use anyhow::Result;
use rusqlite::params;
use serde::de::DeserializeOwned;
use statsai_core::{CostAccumulator, CostInfo, ModelInfo, UsageEvent, UsageSummary};
use statsai_pricing::{
    active_pricing, estimate_priced_from_source_records, install_pricing, overlay_estimated_cost,
    unknown_cost, PricingTable,
};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

pub const APPLIED_PRICING_RULESET_VERSION_KEY: &str = "pricing.applied_ruleset_version";
pub const APPLIED_PRICING_CATALOG_VERSION_KEY: &str = "pricing.applied_catalog_version";

const EVENT_PAGE_SIZE: usize = 256;
const SUMMARY_PAGE_SIZE: usize = 128;
const TASK_SPAN_PAGE_SIZE: usize = 128;

/// What one application of a pricing table to a store did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepricingReport {
    /// The `version` of the pricing table that was applied.
    pub version: u64,
    /// That table's `catalog`.
    pub catalog: String,
    /// The higher version the store was already priced with, when it was.
    /// Repricing never goes backward, so nothing changed.
    pub store_ahead: Option<u64>,
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
    /// The store was already priced with this table's version.
    pub already_current: bool,
}

impl RepricingReport {
    fn for_table(table: &PricingTable) -> Self {
        Self {
            version: table.version(),
            catalog: table.catalog().to_string(),
            ..Self::default()
        }
    }

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
        if let Some(applied) = self.store_ahead {
            return write!(
                formatter,
                "store already priced with pricing version {applied}; this binary prices with {} and does not reprice backward",
                self.version
            );
        }
        if self.already_current {
            return write!(
                formatter,
                "pricing already at version {} ({})",
                self.version, self.catalog
            );
        }
        write!(
            formatter,
            "repriced store to pricing version {} ({}): examined_events={} changed_events={} skipped_unreadable_events={} changed_summaries={} refreshed_rollups={}",
            self.version,
            self.catalog,
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
    /// Brings persisted estimated prices to the newest pricing this process
    /// can use with this store, and makes that table the process's active
    /// table. The refresh state comes from [`Store::pricing_refresh_state`].
    ///
    /// This is not called from [`Store::open`]. The selected StatsAI binary
    /// owns the decision so development launchers compiled against a different
    /// catalog cannot reprice a store they did not select.
    pub fn ensure_current_pricing(&self) -> Result<RepricingReport> {
        self.ensure_current_pricing_with(self.pricing_refresh_state()?)
    }

    /// [`Store::ensure_current_pricing`] with an explicit refresh state:
    /// resolves the table ([`Store::current_pricing_table_with`]), installs it
    /// as the process's active table, and applies it to the store
    /// ([`Store::apply_pricing_table`]).
    ///
    /// The table is installed first, so the rollups the repricing rebuilds
    /// derive their cache figures from the same prices.
    ///
    /// # Errors
    ///
    /// Refuses, changing nothing, when the store keeps a pricing file whose
    /// format this binary cannot read.
    pub fn ensure_current_pricing_with(
        &self,
        refresh: PricingRefreshState,
    ) -> Result<RepricingReport> {
        let table = self.current_pricing_table_with(refresh)?;
        install_pricing(Arc::clone(&table));
        self.apply_pricing_table(&table)
    }

    /// Returns the last successfully applied pricing ruleset, if recorded.
    pub fn applied_pricing_ruleset_version(&self) -> Result<Option<u64>> {
        super::snapshot::parse_applied_pricing_ruleset_value(
            self.metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY)?
                .as_deref(),
        )
    }

    /// Reprices the store's estimates with `table` when the store was last
    /// priced with a lower version, or never.
    ///
    /// Events, summaries, sync rollups (changed ones are marked dirty for the
    /// next incremental sync), session rollups and task spans are repriced in
    /// one transaction that also records the table's version and catalog.
    /// When the table is newer than the store's pricing file, or the store has
    /// no usable one, its text becomes the store's pricing file, so other
    /// binaries sharing the store price with it too.
    ///
    /// A store already at the table's version is left alone. So is a store
    /// priced with a higher version: repricing never goes backward, and the
    /// report says so in [`RepricingReport::store_ahead`].
    ///
    /// This does not change the process's active table.
    pub fn apply_pricing_table(&self, table: &Arc<PricingTable>) -> Result<RepricingReport> {
        if let Some(report) = self.settled_pricing_report(table)? {
            return Ok(report);
        }

        self.with_immediate_transaction(|| {
            if let Some(report) = self.settled_pricing_report(table)? {
                return Ok(report);
            }

            let mut report = RepricingReport::for_table(table);
            let mut dirty_keys = BTreeSet::new();
            let mut session_ids = BTreeSet::new();

            self.reprice_events_in_tx(table, &mut report, &mut dirty_keys, &mut session_ids)?;
            self.reprice_summaries_in_tx(table, &mut report)?;
            report.refreshed_rollups = self.refresh_sync_rollups_for_keys_counted(&dirty_keys)?;
            self.refresh_session_rollups_for_keys(&session_ids)?;
            self.reprice_task_spans_in_tx(&mut report)?;

            self.set_metadata_value(
                APPLIED_PRICING_RULESET_VERSION_KEY,
                &table.version().to_string(),
            )?;
            self.set_metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY, table.catalog())?;
            self.store_pricing_file_if_newer(table)?;
            Ok(report)
        })
    }

    /// The report for a store that `table` would not change: one already at
    /// its version, or one priced with a higher version.
    fn settled_pricing_report(&self, table: &PricingTable) -> Result<Option<RepricingReport>> {
        let applied = self.applied_pricing_ruleset_version()?;
        Ok(match applied {
            Some(applied) if applied > table.version() => Some(RepricingReport {
                store_ahead: Some(applied),
                ..RepricingReport::for_table(table)
            }),
            Some(applied) if applied == table.version() => Some(RepricingReport {
                already_current: true,
                ..RepricingReport::for_table(table)
            }),
            _ => None,
        })
    }
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
        table: &PricingTable,
        report: &mut RepricingReport,
        dirty_keys: &mut BTreeSet<SyncRollupBucketKey>,
        session_ids: &mut BTreeSet<String>,
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
                if let Some(updated) = reprice_event(table, &event) {
                    dirty_keys.insert(self.update_event_cost_payload(&updated)?);
                    session_ids.insert(updated.session.session_id.clone());
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

    fn reprice_summaries_in_tx(
        &self,
        table: &PricingTable,
        report: &mut RepricingReport,
    ) -> Result<()> {
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
                if let Some(updated) = reprice_summary(table, &summary) {
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

fn reprice_event(table: &PricingTable, event: &UsageEvent) -> Option<UsageEvent> {
    // The stored `normalized_name` caches a derivation of the observed name, so
    // a ruleset that teaches the normalizer a new model leaves it stale. It
    // feeds grouping as well as pricing, so refresh it here rather than only
    // working around it at lookup time.
    let model = event
        .model
        .as_ref()
        .map(|model| table.model_with_refreshed_normalization(model));
    let usage = usage_without_stale_request_count(event);
    // An adapter that priced from per-request rows beat what this event's
    // aggregate can express, so only the model normalization is refreshed here.
    // A rescan, which re-reads those rows, is what refreshes the estimate.
    let cost = if estimate_priced_from_source_records(&event.cost) {
        event.cost.clone()
    } else {
        let estimated = table.estimate_cost_at(
            &event.provider,
            model.as_ref(),
            &usage,
            &event.session.started_at,
        );
        overlay_estimated_cost(&event.cost, estimated)
    };
    if cost == event.cost && model == event.model && usage == event.usage {
        return None;
    }
    let mut updated = event.clone();
    updated.model = model;
    updated.usage = usage;
    updated.cost = cost;
    Some(updated)
}

/// Event kind of the Codex adapter's loose `usage` lines, read back from the
/// parse-evidence key `{key_version}:{event_kind}:...`.
const CODEX_HEADLESS_USAGE_EVENT_KIND: &str = "codex_headless_usage";

/// Usage with the `requests: 1` older Codex parsers stamped on loose `usage`
/// lines cleared. Such a line can be a thread's running total, and a
/// single-request count would apply a per-request context tier to all of it.
fn usage_without_stale_request_count(event: &UsageEvent) -> statsai_core::UsageCounts {
    let is_codex_headless_usage = event.provider == "codex"
        && event
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref())
            .and_then(|key| key.split(':').nth(1))
            == Some(CODEX_HEADLESS_USAGE_EVENT_KIND);
    let mut usage = event.usage.clone();
    if is_codex_headless_usage {
        usage.requests = None;
    }
    usage
}

/// Overlays the process's active pricing onto a summary.
///
/// Daily rollups are left unchanged. Returns the original summary when
/// estimated fields already match.
#[must_use]
pub fn apply_current_estimated_pricing(summary: UsageSummary) -> UsageSummary {
    if is_daily_rollup_summary(&summary) {
        return summary;
    }
    reprice_summary(&active_pricing(), &summary).unwrap_or(summary)
}

fn reprice_summary(table: &PricingTable, summary: &UsageSummary) -> Option<UsageSummary> {
    let (period_start, period_end) = summary_period_bounds(summary);
    let mut updated = summary.clone();
    let mut changed = clear_stats_cache_request_counts(&mut updated);
    let pricing = SummaryPricing {
        table,
        provider: &summary.provider,
        period_start: period_start.date_naive(),
        period_end: period_end.date_naive(),
        pricing_at: summary.period_end.unwrap_or(summary.observed_at),
    };

    if !updated.models.is_empty() {
        for model_usage in &mut updated.models {
            let next = pricing.estimated_cost(
                Some(&model_usage.model),
                &model_usage.usage,
                &model_usage.cost,
            );
            if next != model_usage.cost {
                model_usage.cost = next;
                changed = true;
            }
        }
    }

    let next = pricing.estimated_cost(updated.model.as_ref(), &updated.usage, &updated.cost);
    if next != updated.cost {
        updated.cost = next;
        changed = true;
    }

    changed.then_some(updated)
}

/// Clears the `requests: 1` older parsers stamped on Claude stats-cache totals.
///
/// Those totals span every cached session, and a single-request count would
/// let the estimator apply a per-request context tier to the whole aggregate.
fn clear_stats_cache_request_counts(summary: &mut UsageSummary) -> bool {
    if summary.metadata.summary_format != "claude_stats_cache" {
        return false;
    }
    let mut changed = summary.usage.requests.take().is_some();
    for model_usage in &mut summary.models {
        changed |= model_usage.usage.requests.take().is_some();
    }
    changed
}

/// How one summary's estimates are priced: with which table, for which
/// provider, over which period.
struct SummaryPricing<'a> {
    table: &'a PricingTable,
    provider: &'a str,
    period_start: chrono::NaiveDate,
    period_end: chrono::NaiveDate,
    pricing_at: chrono::DateTime<chrono::Utc>,
}

impl SummaryPricing<'_> {
    fn estimated_cost(
        &self,
        model: Option<&ModelInfo>,
        usage: &statsai_core::UsageCounts,
        existing: &CostInfo,
    ) -> CostInfo {
        // An adapter that priced from per-request records beat what this aggregate
        // can express, so recomputing here would replace a better number with a
        // worse one. Left untouched, including its catalog version: the estimate
        // really was produced under that catalog, and a rescan is what refreshes it.
        if estimate_priced_from_source_records(existing) {
            return existing.clone();
        }
        let estimated = if self.crosses_pricing_boundary(model) {
            unknown_cost()
        } else {
            self.table
                .estimate_cost_at(self.provider, model, usage, &self.pricing_at)
        };
        overlay_estimated_cost(existing, estimated)
    }

    /// Whether the table changes this model's price inside the period, so the
    /// aggregate's tokens cannot be split between the rates.
    fn crosses_pricing_boundary(&self, model: Option<&ModelInfo>) -> bool {
        model_pricing_names(model).into_iter().any(|name| {
            self.table
                .pricing_changes_between(&name, self.period_start, self.period_end)
        })
    }
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
