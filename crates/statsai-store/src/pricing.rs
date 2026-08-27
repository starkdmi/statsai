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
mod tests {
    use super::*;
    use crate::CURRENT_SCHEMA_VERSION;
    use chrono::Utc;
    use statsai_core::{
        event_id, summary_id, task_span_id, Confidence, CostInfo, EventSource, LocationOrigin,
        ModelInfo, PrivacyInfo, PrivacyMode, SessionInfo, SourceKind, SummaryMetadata, TaskSpan,
        UsageCounts, UsageEvent, UsageSummary, TASK_SPAN_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    fn test_model(name: &str) -> ModelInfo {
        ModelInfo {
            name: Some(name.to_string()),
            normalized_name: Some(name.to_string()),
            provider_model_id: Some(name.to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }
    }

    fn parse_utc(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc)
    }

    fn test_source(path: &str) -> statsai_core::SourceLocation {
        statsai_core::SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new(path),
            LocationOrigin::Configured,
        )
    }

    fn test_event(
        source: &statsai_core::SourceLocation,
        started_at: chrono::DateTime<Utc>,
        record_id: &str,
        model: &str,
        usage: UsageCounts,
        cost: CostInfo,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id("codex", &source.source_id, record_id, None, started_at),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some(record_id.to_string()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "session".to_string(),
                local_session_id_hash: Some("same-session".to_string()),
                title: None,
                started_at,
                ended_at: None,
                duration_seconds: None,
            },
            model: Some(test_model(model)),
            usage,
            runtime: None,
            cost,
            parse_evidence: None,
            project: None,
            git: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: started_at,
            imported_at: started_at,
        }
    }

    fn missing_cost() -> CostInfo {
        CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: Some("unknown".to_string()),
            pricing_version: None,
            confidence: Confidence::Low,
        }
    }

    fn million_token_usage() -> UsageCounts {
        UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            total_tokens: Some(4_000_000),
            ..UsageCounts::default()
        }
    }

    fn expected_review_cost(started_at: chrono::DateTime<Utc>) -> CostInfo {
        estimate_cost_at(
            "codex",
            Some(&test_model("codex-auto-review")),
            &million_token_usage(),
            &started_at,
        )
    }

    fn store_with_source(path: &str) -> (Store, statsai_core::SourceLocation) {
        let store = Store::in_memory().expect("store");
        let source = test_source(path);
        store.upsert_source(&source).expect("source");
        (store, source)
    }

    fn stored_event(store: &Store, event_id: &str) -> UsageEvent {
        store
            .event_by_id(event_id)
            .expect("load event")
            .expect("event present")
    }

    fn test_span(
        source: &statsai_core::SourceLocation,
        started_at: chrono::DateTime<Utc>,
        record_id: &str,
        linked_event_ids: Vec<statsai_core::EventId>,
        estimated_cost_usd: Option<i64>,
        estimated_cost_micro_usd: Option<i64>,
    ) -> TaskSpan {
        TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: task_span_id("codex", &source.source_id, record_id),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            span_kind: "codex_session".to_string(),
            source_record_id: Some(record_id.to_string()),
            source_file_path_hash: None,
            summary_id: None,
            session_id: Some("session".to_string()),
            thread_id: None,
            title: "Review".to_string(),
            normalized_title: "review".to_string(),
            title_source: Some("summary".to_string()),
            summary_preview: None,
            todo_excerpt: None,
            issue_keys: Vec::new(),
            branch_family: None,
            project_bucket: "none".to_string(),
            project: None,
            git: None,
            usage: million_token_usage(),
            estimated_cost_usd,
            estimated_cost_micro_usd,
            event_count: linked_event_ids.len() as u64,
            has_usage_evidence: !linked_event_ids.is_empty(),
            total_messages: 0,
            user_messages: 0,
            assistant_messages: 0,
            developer_messages: 0,
            linked_event_ids,
            confidence: Confidence::Medium,
            is_meta: false,
            started_at,
            ended_at: Some(started_at),
            duration_seconds: Some(0),
        }
    }

    fn test_summary(
        source: &statsai_core::SourceLocation,
        model: &str,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
        cost: CostInfo,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id("codex", &source.source_id, "period"),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some("period".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: Some(test_model(model)),
            models: Vec::new(),
            usage: million_token_usage(),
            cost,
            parse_evidence: None,
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(start),
            period_end: Some(end),
            observed_at: end,
            metadata: SummaryMetadata {
                summary_format: "grok_build_session_summary".to_string(),
                summary_version: Some("1".to_string()),
                total_sessions: Some(1),
                total_messages: Some(2),
                last_computed_at: Some(end),
            },
            imported_at: end,
        }
    }

    #[test]
    fn store_open_does_not_reprice() {
        let (store, source) = store_with_source("/tmp/codex-open-no-reprice");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");

        assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
        assert!(stored_event(&store, &event.event_id.0)
            .cost
            .estimated_api_equivalent_usd
            .is_none());
    }

    #[test]
    fn legacy_codex_auto_review_event_is_repriced_without_source_files() {
        let (store, source) = store_with_source("/tmp/codex-legacy-review");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");

        let report = store.ensure_current_pricing().expect("reprice");

        assert_eq!(report.examined_events, 1);
        assert_eq!(report.changed_events, 1);
        assert!(!report.already_current);
        let stored = stored_event(&store, &event.event_id.0);
        assert_eq!(stored.cost, expected_review_cost(started_at));
        assert_eq!(stored.event_id, event.event_id);
        assert_eq!(stored.usage, event.usage);
        assert_eq!(stored.session.started_at, event.session.started_at);
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
        assert_eq!(
            store
                .metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)
                .expect("catalog"),
            Some(PRICING_CATALOG_VERSION.to_string())
        );
    }

    #[test]
    fn date_aware_codex_auto_review_boundary_is_preserved() {
        let (store, source) = store_with_source("/tmp/codex-review-boundary");
        let before = parse_utc("2026-07-29T23:59:59Z");
        let after = parse_utc("2026-07-30T00:00:00Z");
        let before_event = test_event(
            &source,
            before,
            "before-boundary",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        let after_event = test_event(
            &source,
            after,
            "after-boundary",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store
            .insert_events(&[before_event.clone(), after_event.clone()])
            .expect("insert");

        store.ensure_current_pricing().expect("reprice");

        let before_cost = stored_event(&store, &before_event.event_id.0).cost;
        let after_cost = stored_event(&store, &after_event.event_id.0).cost;
        assert_eq!(before_cost, expected_review_cost(before));
        assert_eq!(after_cost, expected_review_cost(after));
        assert_ne!(
            before_cost.estimated_api_equivalent_usd,
            after_cost.estimated_api_equivalent_usd
        );
        assert_eq!(after_cost.estimated_api_equivalent_usd, Some(167));
    }

    #[test]
    fn applied_metadata_advances_only_after_success() {
        let (store, source) = store_with_source("/tmp/codex-reprice-success-meta");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        store
            .insert_event(&test_event(
                &source,
                started_at,
                "legacy-review",
                "codex-auto-review",
                million_token_usage(),
                missing_cost(),
            ))
            .expect("insert");

        assert_eq!(
            store.applied_pricing_ruleset_version().expect("before"),
            None
        );
        store.ensure_current_pricing().expect("reprice");
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("after"),
            Some(PRICING_RULESET_VERSION)
        );
    }

    #[test]
    fn second_invocation_at_the_same_version_is_a_noop() {
        let (store, source) = store_with_source("/tmp/codex-reprice-noop");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        store
            .insert_event(&test_event(
                &source,
                started_at,
                "legacy-review",
                "codex-auto-review",
                million_token_usage(),
                missing_cost(),
            ))
            .expect("insert");

        let first = store.ensure_current_pricing().expect("first");
        let payload_before = store.events().expect("events");
        let second = store.ensure_current_pricing().expect("second");

        assert_eq!(first.changed_events, 1);
        assert!(second.already_current);
        assert_eq!(second.changed_events, 0);
        assert_eq!(second.examined_events, 0);
        assert_eq!(store.events().expect("events after noop"), payload_before);
    }

    #[test]
    fn provider_reported_cost_and_provenance_survive_repricing() {
        let (store, source) = store_with_source("/tmp/codex-provider-reported");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let mut cost = missing_cost();
        cost.provider_reported_usd = Some(99);
        cost.provider_reported_micro_usd = Some(990_000);
        cost.pricing_source = Some("provider_invoice".to_string());
        cost.confidence = Confidence::High;
        let event = test_event(
            &source,
            started_at,
            "reported",
            "codex-auto-review",
            million_token_usage(),
            cost,
        );
        store.insert_event(&event).expect("insert");

        store.ensure_current_pricing().expect("reprice");
        let stored = stored_event(&store, &event.event_id.0);
        assert_eq!(stored.cost.provider_reported_usd, Some(99));
        assert_eq!(stored.cost.provider_reported_micro_usd, Some(990_000));
        assert_eq!(
            stored.cost.pricing_source.as_deref(),
            Some("provider_invoice")
        );
        assert_eq!(stored.cost.confidence, Confidence::High);
        assert_eq!(
            stored.cost.estimated_api_equivalent_usd,
            expected_review_cost(started_at).estimated_api_equivalent_usd
        );
        assert_eq!(
            stored.cost.pricing_version.as_deref(),
            Some(PRICING_CATALOG_VERSION)
        );
    }

    #[test]
    fn apply_current_estimated_pricing_overlays_stale_estimated_only_summary() {
        let source = test_source("/tmp/codex-import-overlay");
        let start = parse_utc("2026-07-29T00:00:00Z");
        let end = parse_utc("2026-07-29T23:59:59Z");
        let mut summary = test_summary(&source, "codex-auto-review", start, end, missing_cost());
        summary.cost.estimated_api_equivalent_usd = Some(1);
        summary.cost.estimated_api_equivalent_micro_usd = Some(10_000);
        summary.cost.pricing_source = Some("official:stale".to_string());
        summary.cost.pricing_version = Some("official:stale".to_string());

        let priced = apply_current_estimated_pricing(summary);
        assert_eq!(priced.cost, expected_review_cost(end));
    }

    #[test]
    fn apply_current_estimated_pricing_keeps_provider_reported_amount() {
        let source = test_source("/tmp/codex-import-provider-reported");
        let start = parse_utc("2026-07-29T00:00:00Z");
        let end = parse_utc("2026-07-29T23:59:59Z");
        let mut cost = missing_cost();
        cost.provider_reported_usd = Some(99);
        cost.provider_reported_micro_usd = Some(990_000);
        cost.pricing_source = Some("provider_invoice".to_string());
        cost.confidence = Confidence::High;
        let summary = test_summary(&source, "codex-auto-review", start, end, cost);

        let priced = apply_current_estimated_pricing(summary);
        assert_eq!(priced.cost.provider_reported_usd, Some(99));
        assert_eq!(priced.cost.provider_reported_micro_usd, Some(990_000));
        assert_eq!(
            priced.cost.pricing_source.as_deref(),
            Some("provider_invoice")
        );
        assert_eq!(
            priced.cost.estimated_api_equivalent_usd,
            expected_review_cost(end).estimated_api_equivalent_usd
        );
    }

    #[test]
    fn unknown_model_stays_unknown_while_ruleset_is_marked_applied() {
        let (store, source) = store_with_source("/tmp/codex-unknown-model");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "unknown",
            "not-a-real-model",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = stored_event(&store, &event.event_id.0);
        assert_eq!(report.changed_events, 0);
        assert!(stored.cost.estimated_api_equivalent_usd.is_none());
        assert_eq!(stored.cost.pricing_source.as_deref(), Some("unknown"));
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
    }

    #[test]
    fn summary_spanning_a_pricing_boundary_remains_unknown() {
        let (store, source) = store_with_source("/tmp/codex-boundary-summary");
        let start = parse_utc("2026-07-29T00:00:00Z");
        let end = parse_utc("2026-07-31T00:00:00Z");
        let mut summary = UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id("codex", &source.source_id, "crossing"),
            device_id: "device".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some("crossing".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: Some(test_model("codex-auto-review")),
            models: Vec::new(),
            usage: million_token_usage(),
            cost: missing_cost(),
            parse_evidence: None,
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(start),
            period_end: Some(end),
            observed_at: end,
            metadata: SummaryMetadata {
                summary_format: "claude_stats_cache".to_string(),
                summary_version: Some("1".to_string()),
                total_sessions: Some(1),
                total_messages: Some(2),
                last_computed_at: Some(end),
            },
            imported_at: end,
        };
        summary.cost.estimated_api_equivalent_usd = Some(999);
        store.upsert_summary(&summary).expect("summary");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store
            .summaries()
            .expect("summaries")
            .into_iter()
            .next()
            .expect("one summary");
        assert_eq!(report.changed_summaries, 1);
        assert!(stored.cost.estimated_api_equivalent_usd.is_none());
        assert_eq!(stored.cost.pricing_source.as_deref(), Some("unknown"));
    }

    #[test]
    fn changed_sync_rollups_are_refreshed_and_marked_dirty() {
        let (store, source) = store_with_source("/tmp/codex-reprice-rollups");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store.rebuild_sync_rollups().expect("rebuild");
        let rollups = store.all_sync_rollup_summaries().expect("rollups");
        store
            .mark_sync_rollups_synced(
                &rollups
                    .iter()
                    .map(|summary| summary.summary_id.clone())
                    .collect::<Vec<_>>(),
            )
            .expect("mark synced");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("clean")
            .is_empty());

        let report = store.ensure_current_pricing().expect("reprice");
        let dirty = store.dirty_sync_rollup_summaries().expect("dirty");
        assert_eq!(report.refreshed_rollups, 1);
        assert_eq!(dirty.len(), 1);
        assert_eq!(
            dirty[0].cost.estimated_api_equivalent_usd,
            expected_review_cost(started_at).estimated_api_equivalent_usd
        );
    }

    #[test]
    fn injected_mid_operation_error_rolls_back_payloads_rollups_and_metadata() {
        let (store, source) = store_with_source("/tmp/codex-reprice-rollback");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let first = test_event(
            &source,
            started_at,
            "first",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        let second = test_event(
            &source,
            started_at + chrono::Duration::seconds(1),
            "second",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store
            .insert_events(&[first.clone(), second.clone()])
            .expect("insert");
        store.rebuild_sync_rollups().expect("rebuild");
        let rollups = store.all_sync_rollup_summaries().expect("rollups");
        store
            .mark_sync_rollups_synced(
                &rollups
                    .iter()
                    .map(|summary| summary.summary_id.clone())
                    .collect::<Vec<_>>(),
            )
            .expect("mark synced");
        let payloads_before = store.events().expect("events before");

        fail_repricing_after_event_writes(Some(1));
        let error = store
            .ensure_current_pricing()
            .expect_err("injected failure");
        fail_repricing_after_event_writes(None);

        assert!(error.to_string().contains("injected repricing failure"));
        assert_eq!(store.events().expect("events after"), payloads_before);
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("dirty after rollback")
            .is_empty());
        assert_eq!(store.applied_pricing_ruleset_version().expect("meta"), None);
    }

    #[test]
    fn older_ruleset_refuses_a_newer_store_and_does_not_mutate_it() {
        let (store, source) = store_with_source("/tmp/codex-forward-pricing");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store
            .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "99")
            .expect("future ruleset");
        store
            .set_metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY, "future")
            .expect("future catalog");
        let payload_before =
            serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

        let error = store
            .ensure_pricing_ruleset(1, PRICING_CATALOG_VERSION)
            .expect_err("forward pricing must refuse");

        assert!(error
            .to_string()
            .contains("pricing ruleset version 99 is newer than this StatsAI binary supports (1)"));
        assert_eq!(
            serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
            payload_before
        );
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("unchanged"),
            Some(99)
        );
    }

    #[test]
    fn concurrent_callers_do_not_publish_partial_or_duplicate_repricing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("statsai.sqlite");
        let setup = Store::open(&path).expect("create store");
        let source = test_source("/tmp/codex-concurrent-reprice");
        setup.upsert_source(&source).expect("source");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        setup
            .insert_event(&test_event(
                &source,
                started_at,
                "legacy-review",
                "codex-auto-review",
                million_token_usage(),
                missing_cost(),
            ))
            .expect("insert");
        drop(setup);

        let barrier = Arc::new(Barrier::new(2));
        let reports = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let second_barrier = Arc::clone(&barrier);
            let first_path = path.clone();
            let second_path = path.clone();
            let first = scope.spawn(move || {
                let store = Store::open(&first_path).expect("open first");
                first_barrier.wait();
                store.ensure_current_pricing()
            });
            let second = scope.spawn(move || {
                let store = Store::open(&second_path).expect("open second");
                second_barrier.wait();
                store.ensure_current_pricing()
            });
            vec![
                first.join().expect("first thread"),
                second.join().expect("second thread"),
            ]
        });

        let reports = reports
            .into_iter()
            .map(|result| result.expect("repricing"))
            .collect::<Vec<_>>();
        let workers = reports
            .iter()
            .filter(|report| !report.already_current)
            .count();
        let noops = reports
            .iter()
            .filter(|report| report.already_current)
            .count();
        assert_eq!(workers, 1);
        assert_eq!(noops, 1);
        assert_eq!(
            reports
                .iter()
                .map(|report| report.changed_events)
                .sum::<u64>(),
            1
        );

        let store = Store::open(&path).expect("reopen");
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
        let events = store.events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cost, expected_review_cost(started_at));
    }

    #[test]
    fn daily_rollups_are_unused_by_report_and_sync_paths() {
        let (store, source) = store_with_source("/tmp/codex-daily-rollup-exclusion");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        store
            .insert_event(&test_event(
                &source,
                started_at,
                "legacy-review",
                "codex-auto-review",
                million_token_usage(),
                missing_cost(),
            ))
            .expect("insert");
        let stale = store
            .compute_daily_rollup("2026-07-29", "device")
            .expect("compute");
        store
            .upsert_daily_rollup(&stale)
            .expect("seed unused table");
        let before = store
            .daily_rollups_between("2026-07-29", "2026-07-29")
            .expect("before");

        store.ensure_current_pricing().expect("reprice");

        let after = store
            .daily_rollups_between("2026-07-29", "2026-07-29")
            .expect("after");
        assert_eq!(before, after);
        let events = store.events().expect("events");
        assert_eq!(events[0].cost, expected_review_cost(started_at));
        // Pin: bump this when CURRENT_SCHEMA_VERSION changes, after confirming
        // the daily_rollups table is still unused by report/sync/snapshot.
        assert_eq!(CURRENT_SCHEMA_VERSION, 22);
    }

    #[test]
    fn task_spans_with_linked_events_are_repriced_from_persisted_events() {
        let (store, source) = store_with_source("/tmp/codex-task-span-reprice");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store
            .upsert_task_spans(&[test_span(
                &source,
                started_at,
                "span",
                vec![event.event_id.clone()],
                Some(0),
                Some(0),
            )])
            .expect("span");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store.task_spans().expect("spans");
        assert_eq!(report.changed_task_spans, 1);
        assert_eq!(
            stored[0].estimated_cost_usd,
            expected_review_cost(started_at).estimated_api_equivalent_usd
        );
    }

    #[test]
    fn stale_task_spans_are_repriced_when_events_already_match() {
        let (store, source) = store_with_source("/tmp/codex-stale-span-current-event");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let expected = expected_review_cost(started_at);
        let event = test_event(
            &source,
            started_at,
            "already-priced",
            "codex-auto-review",
            million_token_usage(),
            expected.clone(),
        );
        store.insert_event(&event).expect("insert");
        store
            .upsert_task_spans(&[test_span(
                &source,
                started_at,
                "stale-span",
                vec![event.event_id.clone()],
                Some(0),
                Some(0),
            )])
            .expect("span");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store.task_spans().expect("spans");
        assert_eq!(report.changed_events, 0);
        assert_eq!(report.changed_task_spans, 1);
        assert!(!report.already_current);
        assert_eq!(
            stored[0].estimated_cost_usd,
            expected.estimated_api_equivalent_usd
        );
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
    }

    #[test]
    fn unlinked_task_spans_are_left_unchanged() {
        let (store, source) = store_with_source("/tmp/codex-unlinked-span");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        store
            .insert_event(&test_event(
                &source,
                started_at,
                "priced",
                "codex-auto-review",
                million_token_usage(),
                expected_review_cost(started_at),
            ))
            .expect("insert");
        store
            .upsert_task_spans(&[test_span(
                &source,
                started_at,
                "unlinked",
                Vec::new(),
                Some(0),
                Some(0),
            )])
            .expect("span");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store.task_spans().expect("spans");
        assert_eq!(report.changed_task_spans, 0);
        assert_eq!(stored[0].estimated_cost_usd, Some(0));
        assert_eq!(stored[0].estimated_cost_micro_usd, Some(0));
    }

    #[test]
    fn unreadable_usage_payloads_are_skipped_without_blocking_repricing() {
        let (store, source) = store_with_source("/tmp/codex-corrupt-usage-payload");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store
            .conn
            .execute(
                "INSERT INTO usage_events (
                   event_id, provider, source_id, started_at, total_tokens, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    "aaa-corrupt-event",
                    "codex",
                    source.source_id.0.as_str(),
                    started_at.to_rfc3339(),
                    0,
                    "{ this is not json",
                ],
            )
            .expect("corrupt event");
        store
            .conn
            .execute(
                "INSERT INTO usage_summaries (
                   summary_id, provider, source_id, observed_at, total_tokens, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    "aaa-corrupt-summary",
                    "codex",
                    source.source_id.0.as_str(),
                    started_at.to_rfc3339(),
                    0,
                    "{ also not json",
                ],
            )
            .expect("corrupt summary");

        let report = store.ensure_current_pricing().expect("reprice");
        assert_eq!(report.skipped_unreadable_events, 1);
        assert_eq!(report.skipped_unreadable_summaries, 1);
        assert_eq!(report.changed_events, 1);
        assert_eq!(report.refreshed_rollups, 1);
        assert_eq!(
            stored_event(&store, &event.event_id.0).cost,
            expected_review_cost(started_at)
        );
        assert_eq!(
            store.applied_pricing_ruleset_version().expect("applied"),
            Some(PRICING_RULESET_VERSION)
        );
        let corrupt = store
            .conn
            .query_row(
                "SELECT payload FROM usage_events WHERE event_id = ?1",
                ["aaa-corrupt-event"],
                |row| row.get::<_, String>(0),
            )
            .expect("corrupt row remains");
        assert_eq!(corrupt, "{ this is not json");
    }

    #[test]
    fn dangling_task_span_links_clear_stale_cost() {
        let (store, source) = store_with_source("/tmp/codex-dangling-span-link");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "missing-later",
            "codex-auto-review",
            million_token_usage(),
            expected_review_cost(started_at),
        );
        store.insert_event(&event).expect("insert");
        store
            .upsert_task_spans(&[test_span(
                &source,
                started_at,
                "dangling",
                vec![event.event_id.clone()],
                Some(99),
                Some(990_000),
            )])
            .expect("span");
        store
            .conn
            .execute(
                "DELETE FROM usage_events WHERE event_id = ?1",
                [&event.event_id.0],
            )
            .expect("drop linked event");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store.task_spans().expect("spans");
        assert_eq!(report.changed_task_spans, 1);
        assert!(stored[0].estimated_cost_usd.is_none());
        assert!(stored[0].estimated_cost_micro_usd.is_none());
    }

    #[test]
    fn summary_inside_one_pricing_window_is_repriced() {
        let (store, source) = store_with_source("/tmp/codex-window-summary");
        let start = parse_utc("2026-07-28T00:00:00Z");
        let end = parse_utc("2026-07-29T23:59:59Z");
        store
            .upsert_summary(&test_summary(
                &source,
                "codex-auto-review",
                start,
                end,
                missing_cost(),
            ))
            .expect("summary");

        let report = store.ensure_current_pricing().expect("reprice");
        let stored = store
            .summaries()
            .expect("summaries")
            .into_iter()
            .next()
            .expect("one summary");
        assert_eq!(report.changed_summaries, 1);
        assert_eq!(stored.cost, expected_review_cost(end));
    }

    #[test]
    fn invalid_applied_ruleset_metadata_fails_closed() {
        let (store, source) = store_with_source("/tmp/codex-invalid-ruleset-meta");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store
            .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "not-a-number")
            .expect("write invalid metadata");
        let payload_before =
            serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize");

        let error = store
            .ensure_current_pricing()
            .expect_err("invalid metadata must fail");

        assert!(error
            .to_string()
            .contains("invalid pricing.applied_ruleset_version"));
        assert_eq!(
            serde_json::to_string(&stored_event(&store, &event.event_id.0)).expect("serialize"),
            payload_before
        );
    }

    #[test]
    fn repriced_task_buckets_are_marked_dirty_for_incremental_sync() {
        let (store, source) = store_with_source("/tmp/codex-task-bucket-dirty");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let event = test_event(
            &source,
            started_at,
            "legacy-review",
            "codex-auto-review",
            million_token_usage(),
            missing_cost(),
        );
        store.insert_event(&event).expect("insert");
        store
            .upsert_task_spans(&[test_span(
                &source,
                started_at,
                "span",
                vec![event.event_id.clone()],
                Some(0),
                Some(0),
            )])
            .expect("span");
        store
            .rebuild_task_work_items_for_project_buckets(&BTreeSet::from(["none".to_string()]))
            .expect("seed work items");
        let snapshots = store
            .pending_task_bucket_snapshots_for_sync(
                "http",
                "https://example.invalid/api/sync/batches",
                "device",
                true,
                None,
            )
            .expect("initial snapshots");
        assert!(!snapshots.is_empty());
        store
            .record_task_bucket_snapshots_synced(
                "http",
                "https://example.invalid/api/sync/batches",
                "device",
                &snapshots,
            )
            .expect("mark synced");
        assert!(store
            .pending_task_bucket_snapshots_for_sync(
                "http",
                "https://example.invalid/api/sync/batches",
                "device",
                false,
                None,
            )
            .expect("clean pending")
            .is_empty());

        let report = store.ensure_current_pricing().expect("reprice");
        assert_eq!(report.changed_task_spans, 1);
        assert!(report.rebuilt_work_items > 0);
        let pending = store
            .pending_task_bucket_snapshots_for_sync(
                "http",
                "https://example.invalid/api/sync/batches",
                "device",
                false,
                None,
            )
            .expect("dirty pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].project_bucket, "none");
        let work_items = store.work_items().expect("work items");
        assert_eq!(
            work_items[0].estimated_cost_usd,
            expected_review_cost(started_at).estimated_api_equivalent_usd
        );
    }

    #[test]
    fn representative_fixture_streams_events_instead_of_loading_the_whole_store() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("large.sqlite");
        let store = Store::open(&path).expect("open");
        let source = test_source("/tmp/codex-large-reprice");
        store.upsert_source(&source).expect("source");
        let started_at = parse_utc("2026-07-29T12:00:00Z");
        let events = (0..512)
            .map(|index| {
                test_event(
                    &source,
                    started_at + chrono::Duration::seconds(index),
                    &format!("event-{index}"),
                    "codex-auto-review",
                    million_token_usage(),
                    missing_cost(),
                )
            })
            .collect::<Vec<_>>();
        store.insert_events(&events).expect("insert");

        let started = std::time::Instant::now();
        let report = store.ensure_current_pricing().expect("reprice");
        let elapsed = started.elapsed();

        assert_eq!(report.examined_events, 512);
        assert_eq!(report.changed_events, 512);
        assert!(
            elapsed.as_secs() < 30,
            "repricing 512 events should stay well under 30s, took {elapsed:?}"
        );
        eprintln!(
            "representative fixture: examined={} changed={} elapsed={elapsed:?}",
            report.examined_events, report.changed_events
        );
    }

    #[test]
    fn read_only_applied_version_probe_does_not_open_or_reprice() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("probe.sqlite");
        assert_eq!(
            crate::database_applied_pricing_ruleset_version(&path).expect("missing"),
            None
        );
        let store = Store::open(&path).expect("create");
        assert_eq!(
            crate::database_applied_pricing_ruleset_version(&path).expect("legacy"),
            None
        );
        store
            .set_metadata_value(APPLIED_PRICING_RULESET_VERSION_KEY, "7")
            .expect("write");
        drop(store);
        assert_eq!(
            crate::database_applied_pricing_ruleset_version(&path).expect("present"),
            Some(7)
        );
    }
}
