//! Prompt-cache analysis over stored usage events.
//!
//! The detector compares each call with the one before it in its stream, so
//! it always reads whole sessions: a call on the first day of a report, or the
//! first call in a daily bucket, is judged against its real predecessor even
//! when that predecessor falls outside the range. A resumed session can follow
//! its predecessor by days, so no fixed look-back would do. Sessions are read
//! through `usage_events_session_started_idx`, which also bounds the lookup of
//! the calls after a changed one.

use super::*;
use statsai_core::{
    analyze_cache_calls, build_cache_report, provider_has_cache_diagnostics, AnalyzedCacheCall,
    CacheHealthV1, CacheReport, CacheReportFilters, CacheReportInput, CACHE_DIAGNOSTIC_PROVIDERS,
    CACHE_HEALTH_VERSION,
};
use std::collections::VecDeque;

/// Records which detector version the stored daily summaries were built with.
const CACHE_HEALTH_ROLLUPS_METADATA_KEY: &str = "sync_rollups.cache_health_version";

/// Per-target record of whether an HTTP receiver accepts `cache_health`.
const SYNC_CACHE_HEALTH_METADATA_PREFIX: &str = "sync.cache_health_accepted:";

/// Sessions kept analyzed during one rollup refresh. Buckets refresh in day
/// order, so the sessions a bucket shares with the next one are the recent
/// ones; a bound keeps a full rebuild from holding every session at once.
const CACHE_SESSION_MEMO_CAPACITY: usize = 64;

/// Account filter value for events no provider account was attributed to.
pub const CACHE_REPORT_UNASSIGNED_ACCOUNT: &str = "unassigned";

/// What `statsai report cache` and the local API ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheReportQuery {
    pub label: String,
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub provider: Option<String>,
    /// A provider account id, or [`CACHE_REPORT_UNASSIGNED_ACCOUNT`].
    pub account: Option<String>,
    /// A hashed `session_…` id, or a prefix of one.
    pub session: Option<String>,
    pub timeline: bool,
    pub details: bool,
}

impl CacheReportQuery {
    /// Resolves `--from` and `--to` like the other reports. Date-only values
    /// are UTC days, matching daily summaries; no bounds means the last seven
    /// days.
    pub fn for_range(from: Option<&str>, to: Option<&str>, now: DateTime<Utc>) -> Result<Self> {
        let period = match (from, to) {
            (None, None) => statsai_core::ReportPeriod::LastDays(7),
            (from, to) => statsai_core::report_period_from_range(from, to, now)?,
        };
        Ok(Self::for_period(&period, now))
    }

    /// Every stored call, through now.
    pub fn all_time(now: DateTime<Utc>) -> Self {
        Self::for_period(&statsai_core::ReportPeriod::AllTime, now)
    }

    fn for_period(period: &statsai_core::ReportPeriod, now: DateTime<Utc>) -> Self {
        let (since, until) = period.published_window(now);
        Self {
            label: period.label(now),
            since,
            until,
            provider: None,
            account: None,
            session: None,
            timeline: false,
            details: false,
        }
    }
}

/// A session's calls, by the event that holds them.
type AnalyzedSession = HashMap<String, Vec<AnalyzedCacheCall>>;

/// The columns that place an event in its daily bucket, read without decoding
/// the payload. [`cache_bucket_key`] turns a row of them into the key.
const CACHE_BUCKET_COLUMNS_SQL: &str = "provider, source_id, provider_account_id, started_at, \
     CASE WHEN json_valid(payload) THEN json_extract(payload, '$.project') END";

/// Sessions analyzed during one rollup refresh, so the buckets a session spans
/// do not each re-read and re-classify it.
#[derive(Default)]
pub(crate) struct CacheSessionMemo {
    sessions: HashMap<String, AnalyzedSession>,
    order: VecDeque<String>,
}

impl Store {
    pub fn cache_report<Tz: chrono::TimeZone>(
        &self,
        query: &CacheReportQuery,
        timeline_zone: &Tz,
    ) -> Result<CacheReport>
    where
        Tz::Offset: std::fmt::Display,
    {
        let mut events = Vec::new();
        let mut calls = Vec::new();
        for session_id in self.cache_report_sessions(query)? {
            let mut session_events = Vec::new();
            let mut in_scope = BTreeSet::new();
            for (event, in_period) in self.cache_session_events_in_period(&session_id, query)? {
                // Another provider's events can share the session id; they
                // are counted with the providers that have no diagnostics.
                if !provider_has_cache_diagnostics(&event.provider) {
                    continue;
                }
                if in_period && cache_report_includes(query, &event) {
                    in_scope.insert(event.event_id.0.clone());
                    events.push(event.clone());
                }
                session_events.push(event);
            }
            calls.extend(
                analyze_and_price(&session_events)
                    .into_iter()
                    .filter(|call| in_scope.contains(&call.event_id.0)),
            );
        }
        // Other providers have no diagnostics but count toward the token totals.
        events.extend(
            self.cache_report_other_events(query)?
                .into_iter()
                .filter(|event| cache_report_includes(query, event)),
        );
        events.sort_by(|left, right| {
            left.session
                .started_at
                .cmp(&right.session.started_at)
                .then_with(|| left.event_id.0.cmp(&right.event_id.0))
        });
        // Summaries with no events behind them sync alongside the daily
        // rollups, so they count toward the same token totals. A session
        // filter has no summaries to match.
        let summaries = if query.session.is_some() {
            Vec::new()
        } else {
            self.summaries()?
                .into_iter()
                .filter(|summary| {
                    is_http_rollup_passthrough_summary(summary)
                        && cache_report_includes_summary(query, summary)
                })
                .collect::<Vec<_>>()
        };
        calls.sort_by(|left, right| {
            left.completed_at
                .cmp(&right.completed_at)
                .then_with(|| left.event_id.0.cmp(&right.event_id.0))
                .then_with(|| left.call_index.cmp(&right.call_index))
        });
        Ok(build_cache_report(
            CacheReportInput {
                label: query.label.clone(),
                since: query.since,
                until: query.until,
                filters: CacheReportFilters {
                    provider: query.provider.clone(),
                    account: query.account.clone(),
                    session: query.session.clone(),
                },
                events: &events,
                summaries: &summaries,
                calls,
                include_details: query.details,
            },
            query.timeline.then_some(timeline_zone),
        ))
    }

    /// Records what an HTTP receiver's preflight said about `cache_health`.
    pub fn record_sync_target_cache_health_support(
        &self,
        sink: &str,
        target: &str,
        accepted: bool,
    ) -> Result<()> {
        self.set_metadata_value(
            &format!("{SYNC_CACHE_HEALTH_METADATA_PREFIX}{sink}:{target}"),
            if accepted { "1" } else { "0" },
        )
    }

    /// Whether summaries for this target keep `cache_health`. An HTTP receiver
    /// keeps it only after its preflight advertised support: one that would
    /// discard the object must not be sent it, or the summary would look
    /// synced with the diagnostics silently lost. Local sinks write what they
    /// are given.
    pub fn sync_target_accepts_cache_health(&self, sink: &str, target: &str) -> Result<bool> {
        if sink != "http" {
            return Ok(true);
        }
        Ok(self
            .metadata_value(&format!(
                "{SYNC_CACHE_HEALTH_METADATA_PREFIX}{sink}:{target}"
            ))?
            .as_deref()
            == Some("1"))
    }

    /// Diagnostics for one daily bucket, or `None` for providers the detector
    /// does not cover.
    pub(crate) fn bucket_cache_health(
        &self,
        key: &SyncRollupBucketKey,
        events: &[UsageEvent],
        memo: &mut CacheSessionMemo,
    ) -> Result<Option<CacheHealthV1>> {
        if !provider_has_cache_diagnostics(&key.provider) {
            return Ok(None);
        }
        let mut health = CacheHealthV1::default();
        for event in events {
            health.add_event_input(event);
            let session = self.analyzed_session(&event.session.session_id, memo)?;
            for call in session.get(&event.event_id.0).into_iter().flatten() {
                health.add_call(call);
            }
        }
        Ok(Some(health))
    }

    /// Buckets holding calls that may follow a changed event in its stream.
    ///
    /// A call's verdict depends on its predecessor, so a late arrival, a
    /// correction, or a deletion can change the bucket of the next call, which
    /// may be a later day. Every event of the session at or after the earliest
    /// change is included, found as a range of the session index; refreshing
    /// an unchanged bucket writes nothing.
    pub(crate) fn cache_successor_buckets(
        &self,
        floors: &BTreeMap<String, DateTime<Utc>>,
    ) -> Result<BTreeSet<SyncRollupBucketKey>> {
        let sql = format!(
            "SELECT {CACHE_BUCKET_COLUMNS_SQL} FROM usage_events
             WHERE {EVENT_SESSION_ID_SQL} = ?1 AND started_at >= ?2"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let mut buckets = BTreeSet::new();
        for (session_id, floor) in floors {
            let rows =
                statement.query_map(params![session_id, floor.to_rfc3339()], cache_bucket_key)?;
            for key in rows {
                buckets.extend(key?.filter(|key| provider_has_cache_diagnostics(&key.provider)));
            }
        }
        Ok(buckets)
    }

    /// Rebuilds the daily summaries this report changes, once per detector
    /// version: those of the covered providers, so history whose transcripts
    /// are gone gains diagnostics too, and any other provider's that hold cache
    /// writes, whose hit ratio now counts them. Summaries whose content is
    /// unchanged are left alone, so nothing is queued for sync unless its
    /// payload actually changed.
    pub(crate) fn ensure_cache_health_rollups(&self) -> Result<()> {
        let version = CACHE_HEALTH_VERSION.to_string();
        if self
            .metadata_value(CACHE_HEALTH_ROLLUPS_METADATA_KEY)?
            .as_deref()
            == Some(version.as_str())
        {
            return Ok(());
        }
        let keys = self.cache_report_bucket_keys()?;
        self.with_immediate_transaction(|| {
            self.refresh_sync_rollups_for_keys(&keys)?;
            self.set_metadata_value(CACHE_HEALTH_ROLLUPS_METADATA_KEY, &version)
        })
    }

    fn cache_report_bucket_keys(&self) -> Result<BTreeSet<SyncRollupBucketKey>> {
        let placeholders = sqlite_in_clause_placeholders(CACHE_DIAGNOSTIC_PROVIDERS.len());
        let sql = format!(
            "SELECT {CACHE_BUCKET_COLUMNS_SQL} FROM usage_events
             WHERE provider IN ({placeholders})
                OR (json_valid(payload)
                    AND json_extract(payload, '$.usage.cache_creation_tokens') > 0)"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(CACHE_DIAGNOSTIC_PROVIDERS),
            cache_bucket_key,
        )?;
        let mut keys = BTreeSet::new();
        for key in rows {
            keys.extend(key?);
        }
        Ok(keys)
    }

    fn analyzed_session<'memo>(
        &self,
        session_id: &str,
        memo: &'memo mut CacheSessionMemo,
    ) -> Result<&'memo AnalyzedSession> {
        if !memo.sessions.contains_key(session_id) {
            let events = self.cache_session_events(session_id)?;
            let mut calls_by_event = AnalyzedSession::new();
            for call in analyze_and_price(&events) {
                calls_by_event
                    .entry(call.event_id.0.clone())
                    .or_default()
                    .push(call);
            }
            if memo.order.len() >= CACHE_SESSION_MEMO_CAPACITY {
                if let Some(evicted) = memo.order.pop_front() {
                    memo.sessions.remove(&evicted);
                }
            }
            memo.order.push_back(session_id.to_string());
            memo.sessions.insert(session_id.to_string(), calls_by_event);
        }
        Ok(&memo.sessions[session_id])
    }

    /// Every event of a session, in time order.
    fn cache_session_events(&self, session_id: &str) -> Result<Vec<UsageEvent>> {
        let sql = format!(
            "SELECT payload FROM usage_events WHERE {EVENT_SESSION_ID_SQL} = ?1
             ORDER BY started_at, event_id"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![session_id], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    /// Every event of a session, each marked with whether it starts inside the
    /// report's period. The bounds compare as `events_in_period` compares them.
    fn cache_session_events_in_period(
        &self,
        session_id: &str,
        query: &CacheReportQuery,
    ) -> Result<Vec<(UsageEvent, bool)>> {
        let sql = format!(
            "SELECT payload, (?2 IS NULL OR started_at >= ?2) AND started_at <= ?3
             FROM usage_events WHERE {EVENT_SESSION_ID_SQL} = ?1
             ORDER BY started_at, event_id"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                session_id,
                query.since.map(|since| since.to_rfc3339()),
                query.until.to_rfc3339()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )?;
        let mut events = Vec::new();
        for row in rows {
            let (payload, in_period) = row?;
            events.push((serde_json::from_str(&payload)?, in_period));
        }
        Ok(events)
    }

    /// Sessions of the covered providers with an event in the report's period
    /// that its provider and session filters allow.
    fn cache_report_sessions(&self, query: &CacheReportQuery) -> Result<Vec<String>> {
        let providers = CACHE_DIAGNOSTIC_PROVIDERS
            .iter()
            .filter(|provider| {
                query
                    .provider
                    .as_deref()
                    .is_none_or(|filter| filter == **provider)
            })
            .map(|provider| (*provider).to_string())
            .collect::<Vec<_>>();
        if providers.is_empty() {
            return Ok(Vec::new());
        }
        let mut bounds = vec![query.until.to_rfc3339()];
        let since_sql = match query.since {
            Some(since) => {
                bounds.push(since.to_rfc3339());
                "AND started_at >= ?"
            }
            None => "",
        };
        let sql = format!(
            "SELECT DISTINCT {EVENT_SESSION_ID_SQL} FROM usage_events
             WHERE started_at <= ? {since_sql} AND provider IN ({})",
            sqlite_in_clause_placeholders(providers.len())
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(bounds.iter().chain(&providers)),
            |row| row.get::<_, Option<String>>(0),
        )?;
        let mut sessions = Vec::new();
        for session_id in rows {
            let Some(session_id) = session_id? else {
                continue;
            };
            if query
                .session
                .as_deref()
                .is_none_or(|filter| session_id.starts_with(filter))
            {
                sessions.push(session_id);
            }
        }
        Ok(sessions)
    }

    /// Events in the report's period from providers without diagnostics.
    fn cache_report_other_events(&self, query: &CacheReportQuery) -> Result<Vec<UsageEvent>> {
        if query
            .provider
            .as_deref()
            .is_some_and(provider_has_cache_diagnostics)
        {
            return Ok(Vec::new());
        }
        let mut bounds = vec![query.until.to_rfc3339()];
        let since_sql = match query.since {
            Some(since) => {
                bounds.push(since.to_rfc3339());
                "AND started_at >= ?"
            }
            None => "",
        };
        let sql = format!(
            "SELECT payload FROM usage_events
             WHERE started_at <= ? {since_sql} AND provider NOT IN ({})",
            sqlite_in_clause_placeholders(CACHE_DIAGNOSTIC_PROVIDERS.len())
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(
                bounds
                    .iter()
                    .map(String::as_str)
                    .chain(CACHE_DIAGNOSTIC_PROVIDERS.iter().copied()),
            ),
            |row| row.get::<_, String>(0),
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }
}

/// A summary in the form a target stores it. Selection and the recorded
/// payload hash both see this form, so a receiver without `cache_health`
/// support is not re-sent summaries that differ only by it.
pub fn summary_for_sync_target(summary: UsageSummary, accepts_cache_health: bool) -> UsageSummary {
    if accepts_cache_health {
        summary
    } else {
        summary.without_cache_health()
    }
}

/// A summary that starts within the report's range. The dashboard's period
/// views count a summary the same way, including one that spans several days.
fn cache_report_includes_summary(query: &CacheReportQuery, summary: &UsageSummary) -> bool {
    let (start, _) = summary_period_bounds(summary);
    let in_range = query.since.is_none_or(|since| start >= since) && start <= query.until;
    let provider_matches = query
        .provider
        .as_deref()
        .is_none_or(|provider| summary.provider == provider);
    let account_matches =
        query
            .account
            .as_deref()
            .is_none_or(|account| match summary.provider_account_id.as_ref() {
                Some(id) => id.0 == account,
                None => account == CACHE_REPORT_UNASSIGNED_ACCOUNT,
            });
    in_range && provider_matches && account_matches
}

fn cache_report_includes(query: &CacheReportQuery, event: &UsageEvent) -> bool {
    let provider_matches = query
        .provider
        .as_deref()
        .is_none_or(|provider| event.provider == provider);
    let account_matches =
        query
            .account
            .as_deref()
            .is_none_or(|account| match event.provider_account_id.as_ref() {
                Some(id) => id.0 == account,
                None => account == CACHE_REPORT_UNASSIGNED_ACCOUNT,
            });
    let session_matches = query
        .session
        .as_deref()
        .is_none_or(|session| event.session.session_id.starts_with(session));
    provider_matches && account_matches && session_matches
}

/// Classifies a session's calls and prices the reuse each suspected loss
/// missed.
fn analyze_and_price(events: &[UsageEvent]) -> Vec<AnalyzedCacheCall> {
    let by_id = events
        .iter()
        .map(|event| (event.event_id.0.as_str(), event))
        .collect::<HashMap<_, _>>();
    let mut calls = analyze_cache_calls(events);
    for call in &mut calls {
        let (Some(missed), Some(event)) = (
            call.verdict.missed_tokens,
            by_id.get(call.event_id.0.as_str()),
        ) else {
            continue;
        };
        call.verdict.missed_cost_micro_usd = missed_reuse_cost(event, call, missed);
    }
    calls
}

/// The call's cost at API prices minus its cost had `missed` tokens been
/// cache reads. A lost cache is written again, so they come out of the call's
/// cache writes first, then out of its ordinary input. `None` when the model
/// has no known pricing.
fn missed_reuse_cost(event: &UsageEvent, call: &AnalyzedCacheCall, missed: u64) -> Option<u64> {
    // A call inside an aggregated event has no split of its writes by cache
    // lifetime, and is priced by its own model alone: the event's model
    // identity, such as its provider model id, belongs to the turn's last call.
    let single_call = event
        .context
        .as_ref()
        .is_none_or(|context| context.calls.is_empty());
    let model = match call.model.as_deref() {
        Some(name) if !single_call => Some(statsai_core::ModelInfo {
            name: Some(name.to_string()),
            normalized_name: Some(name.to_string()),
            ..statsai_core::ModelInfo::default()
        }),
        _ => event.model.clone(),
    };
    // One request, so per-request pricing such as long-context rates applies.
    let actual = UsageCounts {
        input_tokens: Some(call.input_tokens),
        cache_read_tokens: Some(call.cache_read_tokens),
        cache_creation_tokens: call.cache_creation_tokens,
        cache_creation_5m_tokens: event.usage.cache_creation_5m_tokens.filter(|_| single_call),
        cache_creation_1h_tokens: event.usage.cache_creation_1h_tokens.filter(|_| single_call),
        requests: Some(1),
        ..UsageCounts::default()
    };
    let reused = usage_with_reads(&actual, missed);
    let cost = |usage: &UsageCounts| {
        statsai_pricing::estimate_cost_at(&call.provider, model.as_ref(), usage, &call.completed_at)
            .estimated_api_equivalent_micro_usd
    };
    let difference = cost(&actual)?.saturating_sub(cost(&reused)?);
    Some(u64::try_from(difference).unwrap_or(0))
}

/// `usage` with `tokens` moved into cache reads from cache writes, keeping
/// the split by cache lifetime in proportion, and then from ordinary input.
fn usage_with_reads(usage: &UsageCounts, tokens: u64) -> UsageCounts {
    let writes = usage.cache_creation_tokens.unwrap_or(0);
    let from_writes = tokens.min(writes);
    let from_input = (tokens - from_writes).min(usage.input_tokens.unwrap_or(0));
    let shrink = |part: Option<u64>| {
        part.map(|part| {
            let removed = u128::from(part) * u128::from(from_writes) / u128::from(writes.max(1));
            part.saturating_sub(u64::try_from(removed).unwrap_or(part))
        })
    };
    UsageCounts {
        input_tokens: usage.input_tokens.map(|input| input - from_input),
        cache_read_tokens: Some(
            usage
                .cache_read_tokens
                .unwrap_or(0)
                .saturating_add(from_writes + from_input),
        ),
        cache_creation_tokens: usage.cache_creation_tokens.map(|_| writes - from_writes),
        cache_creation_5m_tokens: shrink(usage.cache_creation_5m_tokens),
        cache_creation_1h_tokens: shrink(usage.cache_creation_1h_tokens),
        ..usage.clone()
    }
}

/// The daily bucket of a row of [`CACHE_BUCKET_COLUMNS_SQL`], or `None` when
/// its start time does not parse.
fn cache_bucket_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<SyncRollupBucketKey>> {
    let started_at = row.get::<_, String>(3)?;
    let Ok(started_at) = DateTime::parse_from_rfc3339(&started_at) else {
        return Ok(None);
    };
    let project = row.get::<_, Option<String>>(4)?;
    let project = project
        .as_deref()
        .and_then(|project| serde_json::from_str::<statsai_core::ProjectInfo>(project).ok());
    Ok(Some(SyncRollupBucketKey {
        provider: row.get(0)?,
        source_id: row.get(1)?,
        provider_account_id: row.get(2)?,
        day_key: started_at.with_timezone(&Utc).date_naive().to_string(),
        project_key: sync_rollup_project_key(project.as_ref()),
    }))
}
