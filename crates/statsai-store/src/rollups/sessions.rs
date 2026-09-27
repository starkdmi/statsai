use crate::{
    safe_u64_to_i64, sqlite_in_clause_placeholders, sqlite_string_params, Store,
    EVENT_CONVERSATION_HASH_SQL, EVENT_SESSION_ID_SQL,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use statsai_core::{
    hash_text, project_has_stable_identity, provider_session_name,
    provider_session_title_is_unusable, task_title_is_generic, Confidence, CostAccumulator,
    CostInfo, ModelInfo, SessionName, SessionRollupV1, SessionTitleSource, SummaryModelUsage,
    UsageCounts, UsageEvent, SESSION_ROLLUP_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};

// v2 drops record-keyed rows and sessions with neither project nor title,
// leaves single-instant durations unknown, and joins Codex rollout-stem span
// titles. Hosted session tables were rebuilt for the same release, so the v2
// rebuild resends every session.
const SESSION_ROLLUPS_BACKFILL_KEY: &str = "session_rollups_backfilled_v2";
/// Matches the adapters' `provider_record_usage_event.v1` event key version.
const PROVIDER_RECORD_ID_PREFIX: &str = "provider_record_usage_event.v1:";
const UNASSIGNED_ACCOUNT: &str = "unassigned";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub provider: Option<String>,
    pub project_key: Option<String>,
    pub account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSort {
    #[default]
    Started,
    Tokens,
    Cost,
    Duration,
    Messages,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SessionStats {
    pub sessions: u64,
    pub total_tokens: u64,
    pub total_cost_micro_usd: i64,
    pub avg_tokens: Option<u64>,
    pub median_tokens: Option<u64>,
    pub avg_messages: Option<f64>,
    pub median_duration_seconds: Option<u64>,
    pub median_active_seconds: Option<u64>,
    pub total_messages: u64,
    pub top_model: Option<String>,
}

impl Store {
    pub(crate) fn backfill_session_rollups_if_needed(&self) -> Result<()> {
        if self
            .metadata_value(SESSION_ROLLUPS_BACKFILL_KEY)?
            .as_deref()
            == Some("1")
        {
            return Ok(());
        }
        self.rebuild_session_rollups()?;
        // Hosted session tables were rebuilt empty for this release. Resends
        // follow each target's acknowledged payload hash, so clearing the hash
        // makes the next sync send every session; where a hosted row survived,
        // that upsert writes nothing. The acknowledgements themselves stay, so
        // a session the rebuild dropped is still retired by the next snapshot.
        self.conn.execute(
            "UPDATE entity_sync_state SET payload_hash = '' WHERE entity_kind = 'session_rollup'",
            [],
        )?;
        self.set_metadata_value(SESSION_ROLLUPS_BACKFILL_KEY, "1")?;
        Ok(())
    }

    pub fn rebuild_session_rollups(&self) -> Result<u64> {
        let events = self.events()?;
        let mut grouped: BTreeMap<String, Vec<UsageEvent>> = BTreeMap::new();
        for event in events {
            if event_has_own_record_session(&event) {
                continue;
            }
            grouped
                .entry(event.session.session_id.clone())
                .or_default()
                .push(event);
        }
        self.with_immediate_transaction(|| {
            // Every session is resent after a rebuild, since the hosted table
            // may have been rebuilt with it. Unchanged rows keep their payload
            // and updated_at, so where the hosted row survived, the resend is
            // a no-op upsert that writes nothing.
            let existing = self.session_rollup_payload_hashes()?;
            let hashes = grouped
                .values()
                .flatten()
                .filter_map(|event| event.session.local_session_id_hash.clone())
                .collect::<Vec<_>>();
            let titles = self.session_titles_for_hashes(&hashes)?;
            let updated_at = Utc::now();
            let rollups = grouped
                .values()
                .map(|events| build_session_rollup(events, &titles, updated_at))
                .filter(session_rollup_is_identifiable)
                .collect::<Vec<_>>();
            let kept = rollups
                .iter()
                .map(|rollup| rollup.session_id.as_str())
                .collect::<BTreeSet<_>>();
            for session_id in existing.keys() {
                if !kept.contains(session_id.as_str()) {
                    self.conn.execute(
                        "DELETE FROM session_rollups WHERE session_id = ?1",
                        params![session_id],
                    )?;
                }
            }
            for rollup in &rollups {
                let session_id = &rollup.session_id;
                let payload_hash = session_rollup_content_hash(rollup)?;
                if existing.get(session_id) == Some(&payload_hash) {
                    self.conn.execute(
                        "UPDATE session_rollups SET dirty = 1 WHERE session_id = ?1",
                        params![session_id],
                    )?;
                    continue;
                }
                self.write_session_rollup(rollup, &payload_hash, 1)?;
            }
            Ok(rollups.len() as u64)
        })
    }

    pub(crate) fn refresh_session_rollups_for_keys(
        &self,
        session_ids: &BTreeSet<String>,
    ) -> Result<()> {
        if session_ids.is_empty() {
            return Ok(());
        }
        let mut present = BTreeMap::new();
        let mut hashes = Vec::new();
        for session_id in session_ids {
            let events = self.session_events(session_id)?;
            if events.is_empty() {
                self.conn.execute(
                    "DELETE FROM session_rollups WHERE session_id = ?1",
                    params![session_id],
                )?;
                continue;
            }
            hashes.extend(
                events
                    .iter()
                    .filter_map(|event| event.session.local_session_id_hash.clone()),
            );
            present.insert(session_id.clone(), events);
        }
        // Title resolution scans task spans and archives once per batch. Doing
        // it per session turns a scan into one full pass for every conversation.
        let titles = self.session_titles_for_hashes(&hashes)?;
        for (session_id, events) in present {
            let rollup = build_session_rollup(&events, &titles, Utc::now());
            if !session_rollup_is_identifiable(&rollup) {
                self.conn.execute(
                    "DELETE FROM session_rollups WHERE session_id = ?1",
                    params![session_id],
                )?;
                continue;
            }
            let payload_hash = session_rollup_content_hash(&rollup)?;
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT payload_hash FROM session_rollups WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some(payload_hash.as_str()) {
                continue;
            }
            self.write_session_rollup(&rollup, &payload_hash, 1)?;
        }
        Ok(())
    }

    /// Records the names providers hold for sessions and refreshes the sessions
    /// whose name changed, with the sub-agents that borrow it. Unchanged names
    /// write nothing. A session's own name is never replaced by a borrowed one.
    pub fn upsert_session_names(&self, names: &[SessionName]) -> Result<u64> {
        if names.is_empty() {
            return Ok(0);
        }
        let mut changed_hashes = Vec::new();
        {
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO session_names (local_session_id_hash, title, parent_hash, updated_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(local_session_id_hash) DO UPDATE SET
                  title = excluded.title,
                  parent_hash = excluded.parent_hash,
                  updated_at = excluded.updated_at
                WHERE (session_names.title IS NOT excluded.title
                       OR session_names.parent_hash IS NOT excluded.parent_hash)
                  AND (excluded.parent_hash IS NULL OR session_names.parent_hash IS NOT NULL)
                "#,
            )?;
            let now = Utc::now().to_rfc3339();
            for name in names {
                if statement.execute(params![
                    &name.local_session_id_hash,
                    &name.title,
                    &name.parent_local_session_id_hash,
                    &now
                ])? > 0
                {
                    changed_hashes.push(name.local_session_id_hash.clone());
                }
            }
        }
        for chunk in changed_hashes.clone().chunks(500) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let mut statement = self.conn.prepare(&format!(
                "SELECT local_session_id_hash FROM session_names WHERE parent_hash IN ({placeholders})"
            ))?;
            let params = sqlite_string_params(chunk);
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                changed_hashes.push(row?);
            }
        }
        let session_ids = self.session_ids_for_local_hashes(&changed_hashes)?;
        self.refresh_session_rollups_for_keys(&session_ids)?;
        Ok(changed_hashes.len() as u64)
    }

    pub(crate) fn refresh_session_rollups_for_raw_ids(&self, raw_ids: &[String]) -> Result<()> {
        let hashes = raw_ids
            .iter()
            .map(|raw_id| raw_id.trim())
            .filter(|raw_id| !raw_id.is_empty())
            .flat_map(|raw_id| std::iter::once(raw_id).chain(codex_rollout_session_uuid(raw_id)))
            .map(hash_text)
            .collect::<Vec<_>>();
        let session_ids = self.session_ids_for_local_hashes(&hashes)?;
        self.refresh_session_rollups_for_keys(&session_ids)
    }

    fn write_session_rollup(
        &self,
        rollup: &SessionRollupV1,
        payload_hash: &str,
        dirty: i64,
    ) -> Result<()> {
        let payload = serde_json::to_string(rollup)?;
        let day_key = rollup
            .started_at
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        self.conn.execute(
            r#"
            INSERT INTO session_rollups (
              session_id, provider, source_id, provider_account_id, day_key,
              started_at, updated_at, total_tokens, payload_hash, dirty, payload
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(session_id) DO UPDATE SET
              provider = excluded.provider,
              source_id = excluded.source_id,
              provider_account_id = excluded.provider_account_id,
              day_key = excluded.day_key,
              started_at = excluded.started_at,
              updated_at = excluded.updated_at,
              total_tokens = excluded.total_tokens,
              payload_hash = excluded.payload_hash,
              dirty = excluded.dirty,
              payload = excluded.payload
            "#,
            params![
                &rollup.session_id,
                &rollup.provider,
                &rollup.source_id.0,
                rollup.provider_account_id.as_ref().map(|id| id.0.as_str()),
                &day_key,
                rollup.started_at.to_rfc3339(),
                rollup.updated_at.to_rfc3339(),
                safe_u64_to_i64(rollup.usage.computed_total()),
                payload_hash,
                dirty,
                &payload,
            ],
        )?;
        Ok(())
    }

    pub fn all_session_rollups(&self) -> Result<Vec<SessionRollupV1>> {
        self.session_rollups_by_sql(
            "SELECT payload FROM session_rollups ORDER BY started_at, session_id",
        )
    }

    pub fn session_rollup_ids(&self) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT session_id FROM session_rollups ORDER BY session_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn dirty_session_rollups(&self) -> Result<Vec<SessionRollupV1>> {
        self.session_rollups_by_sql(
            "SELECT payload FROM session_rollups WHERE dirty = 1 ORDER BY started_at, session_id",
        )
    }

    pub fn mark_session_rollups_synced(&self, session_ids: &[String]) -> Result<()> {
        if session_ids.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction(|| {
            self.mark_session_rollups_synced_in_transaction(session_ids)
        })
    }

    pub(crate) fn mark_session_rollups_synced_in_transaction(
        &self,
        session_ids: &[String],
    ) -> Result<()> {
        for session_id in session_ids {
            self.conn.execute(
                "UPDATE session_rollups SET dirty = 0 WHERE session_id = ?1",
                params![session_id],
            )?;
        }
        Ok(())
    }

    pub fn mark_all_session_rollups_dirty(&self) -> Result<u64> {
        let updated = self.conn.execute(
            "UPDATE session_rollups SET dirty = 1, updated_at = ?1",
            params![Utc::now().to_rfc3339()],
        )? as u64;
        Ok(updated)
    }

    pub fn session_rollups_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
        sort: SessionSort,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionRollupV1>> {
        let mut rollups = self.filtered_session_rollups(since, until, filter)?;
        sort_sessions(&mut rollups, sort);
        Ok(rollups.into_iter().skip(offset).take(limit).collect())
    }

    pub fn session_stats_in_period(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
    ) -> Result<SessionStats> {
        let rollups = self.filtered_session_rollups(since, until, filter)?;
        Ok(session_stats(&rollups))
    }

    fn filtered_session_rollups(
        &self,
        since: Option<DateTime<Utc>>,
        until: DateTime<Utc>,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionRollupV1>> {
        let since = since.map(|value| value.to_rfc3339());
        let until = until.to_rfc3339();
        let provider = filter.provider.as_deref();
        let account = filter.account.as_deref();
        let mut statement = self.conn.prepare(
            r#"
            SELECT payload FROM session_rollups
            WHERE started_at <= ?1
              AND (?2 IS NULL OR started_at >= ?2)
              AND (?3 IS NULL OR provider = ?3)
              AND (
                ?4 IS NULL
                OR (?4 = ?5 AND provider_account_id IS NULL)
                OR provider_account_id = ?4
              )
            ORDER BY started_at, session_id
            "#,
        )?;
        let rows = statement.query_map(
            params![until, since, provider, account, UNASSIGNED_ACCOUNT],
            |row| row.get::<_, String>(0),
        )?;
        let mut rollups = Vec::new();
        for row in rows {
            let rollup: SessionRollupV1 = serde_json::from_str(&row?)?;
            if session_matches_project(&rollup, filter.project_key.as_deref()) {
                rollups.push(rollup);
            }
        }
        Ok(rollups)
    }

    fn session_rollups_by_sql(&self, sql: &str) -> Result<Vec<SessionRollupV1>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut rollups = Vec::new();
        for row in rows {
            rollups.push(serde_json::from_str(&row?)?);
        }
        Ok(rollups)
    }

    fn session_events(&self, session_id: &str) -> Result<Vec<UsageEvent>> {
        let sql = format!(
            "SELECT payload FROM usage_events WHERE {EVENT_SESSION_ID_SQL} = ?1 ORDER BY started_at, event_id"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![session_id], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            let event: UsageEvent = serde_json::from_str(&row?)?;
            if !event_has_own_record_session(&event) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn session_rollup_payload_hashes(&self) -> Result<BTreeMap<String, String>> {
        let mut statement = self
            .conn
            .prepare("SELECT session_id, payload_hash FROM session_rollups")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
    }

    fn session_ids_for_local_hashes(&self, hashes: &[String]) -> Result<BTreeSet<String>> {
        let mut session_ids = BTreeSet::new();
        if hashes.is_empty() {
            return Ok(session_ids);
        }
        for chunk in hashes.chunks(500) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let sql = format!(
                "SELECT DISTINCT json_extract(payload, '$.session.session_id')
                 FROM usage_events
                 WHERE {EVENT_CONVERSATION_HASH_SQL} IN ({placeholders})
                   AND json_extract(payload, '$.session.session_id') IS NOT NULL"
            );
            let mut statement = self.conn.prepare(&sql)?;
            let params = sqlite_string_params(chunk);
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                session_ids.insert(row?);
            }
        }
        Ok(session_ids)
    }

    /// Titles keyed by `local_session_id_hash`. Task spans replace archive titles.
    /// Generic and secret-looking titles are dropped.
    pub(crate) fn session_titles_for_hashes(
        &self,
        hashes: &[String],
    ) -> Result<BTreeMap<String, (String, SessionTitleSource)>> {
        let wanted = hashes
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut titles = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(titles);
        }
        // Rows are gathered across chunks and then ordered, so the latest title
        // wins no matter which chunk held it.
        let mut archive_rows = Vec::new();
        let mut span_rows = Vec::new();
        let mut name_rows = Vec::new();
        for chunk in wanted.chunks(300) {
            let placeholders = sqlite_in_clause_placeholders(chunk.len());
            let params = chunk
                .iter()
                .map(|hash| hash as &dyn rusqlite::types::ToSql)
                .collect::<Vec<_>>();
            let mut archive = self.conn.prepare(&format!(
                "SELECT updated_at, conversation_id, session_hash, title
                 FROM archive_conversations
                 WHERE session_hash IN ({placeholders}) AND title IS NOT NULL"
            ))?;
            for row in archive.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })? {
                archive_rows.push(row?);
            }
            // A Codex span may name its session by rollout path; the events
            // carry the thread id inside it.
            let mut spans = self.conn.prepare(&format!(
                "SELECT started_at, span_id,
                        CASE WHEN session_hash IN ({placeholders}) THEN session_hash
                             ELSE session_uuid_hash END,
                        title
                 FROM task_spans
                 WHERE is_meta = 0
                   AND (session_hash IN ({placeholders})
                        OR session_uuid_hash IN ({placeholders}))"
            ))?;
            let mut span_params = params.clone();
            span_params.extend(params.iter().copied());
            span_params.extend(params.iter().copied());
            for row in spans.query_map(span_params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })? {
                span_rows.push(row?);
            }
            // A borrowed name reads the parent's own name as it is now.
            let mut names = self.conn.prepare(&format!(
                "SELECT child.local_session_id_hash,
                        CASE WHEN child.parent_hash IS NULL THEN child.title
                             WHEN parent.title IS NOT NULL
                               THEN parent.title || ' · ' || child.title
                        END
                 FROM session_names child
                 LEFT JOIN session_names parent
                   ON parent.local_session_id_hash = child.parent_hash
                  AND parent.parent_hash IS NULL
                 WHERE child.local_session_id_hash IN ({placeholders})"
            ))?;
            for row in names.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })? {
                name_rows.push(row?);
            }
        }
        archive_rows.sort();
        for (_, _, hash, title) in archive_rows {
            remember_session_title(&mut titles, hash, Some(&title), SessionTitleSource::Archive);
        }
        span_rows.sort();
        for (_, _, hash, title) in span_rows {
            remember_session_title(
                &mut titles,
                hash,
                title.as_deref(),
                SessionTitleSource::TaskSpan,
            );
        }
        // A provider-held name outranks task and archive titles for its session.
        for (hash, title) in name_rows {
            let title = provider_session_name(title.as_deref(), 90);
            remember_session_title(
                &mut titles,
                hash,
                title.as_deref(),
                SessionTitleSource::Event,
            );
        }
        Ok(titles)
    }

    pub(crate) fn task_span_raw_session_ids_for_bucket(
        &self,
        project_bucket: &str,
    ) -> Result<BTreeSet<String>> {
        let mut statement = self.conn.prepare(
            "SELECT json_extract(payload, '$.session_id')
             FROM task_spans
             WHERE project_bucket = ?1",
        )?;
        let rows = statement.query_map(params![project_bucket], |row| {
            row.get::<_, Option<String>>(0)
        })?;
        let mut ids = BTreeSet::new();
        for row in rows {
            if let Some(session_id) = row?.filter(|value| !value.trim().is_empty()) {
                ids.insert(session_id);
            }
        }
        Ok(ids)
    }
}

pub(crate) fn build_session_rollup(
    events: &[UsageEvent],
    titles: &BTreeMap<String, (String, SessionTitleSource)>,
    updated_at: DateTime<Utc>,
) -> SessionRollupV1 {
    let mut ordered = events.to_vec();
    ordered.sort_by(|left, right| {
        left.session
            .started_at
            .cmp(&right.session.started_at)
            .then(left.created_at.cmp(&right.created_at))
            .then(left.event_id.0.cmp(&right.event_id.0))
    });
    let events = ordered.as_slice();
    let oldest = events.first().expect("session rollup requires events");
    let newest = events.last().unwrap_or(oldest);
    let mut totals = UsageTotals::default();
    let mut estimated_cost = CostAccumulator::default();
    let mut provider_reported_cost = CostAccumulator::default();
    let mut has_provider_reported = false;
    let mut model_buckets: BTreeMap<String, (ModelInfo, ModelTotals)> = BTreeMap::new();
    let mut total_messages = 0u64;
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut developer_messages = 0u64;
    let mut saw_messages = false;

    for event in events {
        totals.add(&event.usage);
        let requests = event.usage.requests.unwrap_or(1);
        totals.requests = totals.requests.saturating_add(requests);
        estimated_cost.add_estimated(&event.cost);
        if event.cost.provider_reported_micro_usd.is_some()
            || event.cost.provider_reported_usd.is_some()
        {
            provider_reported_cost.add_values(
                event.cost.provider_reported_micro_usd,
                event.cost.provider_reported_usd,
            );
            has_provider_reported = true;
        }
        if let Some(runtime) = event.runtime.as_ref() {
            let derived_total = runtime.total_messages.or_else(|| {
                let derived = runtime
                    .user_messages
                    .unwrap_or(0)
                    .saturating_add(runtime.assistant_messages.unwrap_or(0))
                    .saturating_add(runtime.developer_messages.unwrap_or(0));
                (derived > 0).then_some(derived)
            });
            if derived_total.is_some()
                || runtime.user_messages.is_some()
                || runtime.assistant_messages.is_some()
                || runtime.developer_messages.is_some()
            {
                saw_messages = true;
            }
            total_messages = total_messages.saturating_add(derived_total.unwrap_or(0));
            user_messages = user_messages.saturating_add(runtime.user_messages.unwrap_or(0));
            assistant_messages =
                assistant_messages.saturating_add(runtime.assistant_messages.unwrap_or(0));
            developer_messages =
                developer_messages.saturating_add(runtime.developer_messages.unwrap_or(0));
        }

        let model = event.model.clone().unwrap_or_default();
        let entry = model_buckets
            .entry(session_model_key(&model))
            .or_insert_with(|| (model, ModelTotals::default()));
        entry.1.add(&event.usage, requests, &event.cost);
    }

    // A turn starts at its prompt, which can precede the turn's first event.
    let started_at = events
        .iter()
        .map(|event| {
            event
                .session
                .turn_started_at
                .map_or(event.session.started_at, |turn| {
                    turn.min(event.session.started_at)
                })
        })
        .min()
        .unwrap_or(oldest.session.started_at);
    let mut ended_at = events
        .iter()
        .map(|event| event.session.ended_at.unwrap_or(event.created_at))
        .max()
        .unwrap_or(newest.created_at);
    // A provider clock can record a completion before the session start.
    // Ordering the interval keeps duration non-negative.
    if ended_at < started_at {
        ended_at = started_at;
    }
    // One instant with no recorded end is a start, not a zero-length session.
    // Counting it as 0s pulled the median to zero for every source that
    // stamps a single time per row.
    let has_recorded_end = events
        .iter()
        .any(|event| event.session.ended_at.is_some() || event.session.duration_seconds.is_some());
    let duration_seconds = if has_recorded_end || ended_at > started_at {
        ended_at
            .signed_duration_since(started_at)
            .num_seconds()
            .try_into()
            .ok()
    } else {
        None
    };
    let active_seconds = session_active_seconds(events);
    let provider_account_id = events
        .iter()
        .rev()
        .find_map(|event| event.provider_account_id.clone());
    // The latest event that knows its project: a later event without project
    // metadata must not erase it, since a session with neither a project nor
    // a title is not kept.
    let project = events
        .iter()
        .rev()
        .find_map(|event| event.project.clone().filter(project_has_stable_identity));
    let (title, title_source) = resolve_title(events, titles);
    let models = model_buckets
        .into_iter()
        .map(|(_, (model, totals))| model_usage(model, totals))
        .collect::<Vec<_>>();
    let primary_model = primary_model_label(&models);
    let requests = totals.requests;

    SessionRollupV1 {
        schema_version: SESSION_ROLLUP_SCHEMA_VERSION.to_string(),
        session_id: oldest.session.session_id.clone(),
        device_id: newest.device_id.clone(),
        provider: newest.provider.clone(),
        source_id: newest.source_id.clone(),
        provider_account_id,
        started_at,
        ended_at,
        duration_seconds,
        active_seconds,
        usage: totals.into_counts(),
        requests,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: estimated_cost.cents_rounded(),
            provider_reported_usd: has_provider_reported
                .then(|| provider_reported_cost.cents_rounded())
                .flatten(),
            estimated_api_equivalent_micro_usd: estimated_cost.micro_usd(),
            provider_reported_micro_usd: has_provider_reported
                .then(|| provider_reported_cost.micro_usd())
                .flatten(),
            pricing_source: Some("local_rollup".to_string()),
            pricing_version: None,
            confidence: Confidence::Medium,
        },
        models,
        primary_model,
        total_messages: saw_messages
            .then_some(total_messages)
            .filter(|value| *value > 0),
        user_messages: (user_messages > 0).then_some(user_messages),
        assistant_messages: (assistant_messages > 0).then_some(assistant_messages),
        developer_messages: (developer_messages > 0).then_some(developer_messages),
        project,
        title,
        title_source,
        updated_at,
    }
}

/// Time the agent was working: the union of the intervals the provider
/// recorded for its turns. Codex records a turn's start and completion,
/// OpenCode each message's, and Claude Code events carry the prompt that
/// started their turn. Gaps between turns are idle and do not count; nothing
/// here guesses where a session went idle.
///
/// An event that aggregates a whole session (OpenCode's `sqlite:session`
/// fallback) carries the session's lifetime, which is a span, not a turn.
fn session_active_seconds(events: &[UsageEvent]) -> Option<u64> {
    let mut intervals = events
        .iter()
        .filter(|event| !event.source.source_type.ends_with(":session"))
        .filter_map(|event| {
            let session = &event.session;
            let (start, mut end) = match session.turn_started_at {
                Some(turn) => (turn, session.ended_at.unwrap_or(session.started_at)),
                None => (session.started_at, session.ended_at?),
            };
            // A reported duration wins over the completion time. Codex writes a
            // turn's completion when the thread next runs, so a turn that
            // worked 21 minutes can complete four days after it started.
            if let Some(reported) = session
                .duration_seconds
                .and_then(|seconds| i64::try_from(seconds).ok())
            {
                end = end.min(start + chrono::Duration::seconds(reported));
            }
            (end >= start).then_some((start, end))
        })
        .collect::<Vec<_>>();
    intervals.sort();
    let mut intervals = intervals.into_iter();
    let (mut start, mut end) = intervals.next()?;
    let mut total = chrono::Duration::zero();
    for (next_start, next_end) in intervals {
        if next_start <= end {
            end = end.max(next_end);
        } else {
            total += end - start;
            (start, end) = (next_start, next_end);
        }
    }
    total += end - start;
    u64::try_from(total.num_seconds()).ok()
}

/// Precedence: the name the provider holds now, then a title stamped on an
/// event when it was parsed, then a task-span title, then an archive title.
fn resolve_title(
    events: &[UsageEvent],
    titles: &BTreeMap<String, (String, SessionTitleSource)>,
) -> (Option<String>, Option<SessionTitleSource>) {
    let hashes = || {
        events
            .iter()
            .rev()
            .filter_map(|event| event.session.local_session_id_hash.as_deref())
    };
    if let Some(title) = hashes().find_map(|hash| {
        titles
            .get(hash)
            .filter(|(_, source)| *source == SessionTitleSource::Event)
            .map(|(title, _)| title.clone())
    }) {
        return (Some(title), Some(SessionTitleSource::Event));
    }
    if let Some(title) = events.iter().rev().find_map(|event| {
        acceptable_session_title(event.session.title.as_deref(), SessionTitleSource::Event)
    }) {
        return (Some(title), Some(SessionTitleSource::Event));
    }
    for hash in hashes() {
        if let Some((title, source)) = titles.get(hash) {
            return (Some(title.clone()), Some(*source));
        }
    }
    (None, None)
}

/// A session with neither a project nor a title cannot be told apart from any
/// other, so it is not kept. Cursor cloud agents are the case: the export names
/// the agent by id only and records no project.
fn session_rollup_is_identifiable(rollup: &SessionRollupV1) -> bool {
    rollup.project.is_some() || rollup.title.is_some()
}

/// An event whose session identity is its own provider record belongs to no
/// session. Cursor's usage export names no session, so each local row is keyed
/// by itself, and treating those rows as sessions listed every request as one.
pub(crate) fn event_has_own_record_session(event: &UsageEvent) -> bool {
    let Some(session_hash) = event.session.local_session_id_hash.as_deref() else {
        return false;
    };
    event
        .parse_evidence
        .as_ref()
        .and_then(|evidence| evidence.source_record_id.as_deref())
        .and_then(|record_id| record_id.strip_prefix(PROVIDER_RECORD_ID_PREFIX))
        .and_then(|rest| rest.rsplit_once(':'))
        .is_some_and(|(_, record_hash)| record_hash == session_hash)
}

/// Older Codex task spans name their session by the rollout file stem, while
/// events hash the UUID the file declares. The stem ends in that UUID.
fn codex_rollout_session_uuid(raw_id: &str) -> Option<&str> {
    let stem = raw_id.rsplit('/').next()?;
    if !stem.starts_with("rollout-") {
        return None;
    }
    let uuid = stem.get(stem.len().checked_sub(36)?..)?;
    let is_uuid = uuid.char_indices().all(|(index, character)| match index {
        8 | 13 | 18 | 23 => character == '-',
        _ => character.is_ascii_hexdigit(),
    });
    is_uuid.then_some(uuid)
}

/// Names the provider put on its events are kept unless unsafe. Task-span and
/// archive titles can come from prompts, so they must not be generic.
fn acceptable_session_title(value: Option<&str>, source: SessionTitleSource) -> Option<String> {
    let trimmed = value?.trim();
    let rejected = match source {
        SessionTitleSource::Event => provider_session_title_is_unusable(Some(trimmed)),
        _ => task_title_is_generic(Some(trimmed)),
    };
    (!trimmed.is_empty() && !rejected).then(|| trimmed.to_string())
}

fn remember_session_title(
    titles: &mut BTreeMap<String, (String, SessionTitleSource)>,
    hash: String,
    title: Option<&str>,
    source: SessionTitleSource,
) {
    if let Some(title) = acceptable_session_title(title, source) {
        titles.insert(hash, (title, source));
    }
}

/// The hashed keys a task span's session is looked up by: its session id, and
/// for a Codex rollout path, the thread id inside it.
pub(crate) fn task_span_session_hashes(
    session_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) else {
        return (None, None);
    };
    (
        Some(hash_text(session_id)),
        codex_rollout_session_uuid(session_id).map(hash_text),
    )
}

fn session_rollup_content_hash(rollup: &SessionRollupV1) -> Result<String> {
    let mut comparable = rollup.clone();
    comparable.updated_at = DateTime::<Utc>::UNIX_EPOCH;
    Ok(hash_text(&serde_json::to_string(&comparable)?))
}

fn session_matches_project(rollup: &SessionRollupV1, project: Option<&str>) -> bool {
    let Some(project) = project.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let Some(info) = rollup.project.as_ref() else {
        return false;
    };
    if statsai_core::daily_rollup_project_key(Some(info)) == project || info.project_id == project {
        return true;
    }
    [
        info.project_label.as_deref(),
        info.repo_label.as_deref(),
        info.path_label.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|label| label.eq_ignore_ascii_case(project))
}

fn sort_sessions(sessions: &mut [SessionRollupV1], sort: SessionSort) {
    sessions.sort_by(|left, right| {
        let metric = match sort {
            SessionSort::Started => std::cmp::Ordering::Equal,
            SessionSort::Tokens => right
                .usage
                .computed_total()
                .cmp(&left.usage.computed_total()),
            SessionSort::Cost => right
                .cost
                .estimated_micro_usd()
                .unwrap_or(0)
                .cmp(&left.cost.estimated_micro_usd().unwrap_or(0)),
            // Active time: a thread resumed weeks later is not a long session.
            SessionSort::Duration => right
                .active_seconds
                .unwrap_or(0)
                .cmp(&left.active_seconds.unwrap_or(0)),
            SessionSort::Messages => right
                .total_messages
                .unwrap_or(0)
                .cmp(&left.total_messages.unwrap_or(0)),
        };
        metric
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
}

fn session_stats(rollups: &[SessionRollupV1]) -> SessionStats {
    let sessions = rollups.len() as u64;
    let total_tokens = rollups.iter().fold(0u64, |sum, rollup| {
        sum.saturating_add(rollup.usage.computed_total())
    });
    let total_cost_micro_usd = rollups.iter().fold(0i64, |sum, rollup| {
        sum.saturating_add(rollup.cost.estimated_micro_usd().unwrap_or(0))
    });
    let total_messages = rollups.iter().fold(0u64, |sum, rollup| {
        sum.saturating_add(rollup.total_messages.unwrap_or(0))
    });
    let message_samples = rollups
        .iter()
        .filter_map(|rollup| rollup.total_messages)
        .collect::<Vec<_>>();
    let avg_messages = (!message_samples.is_empty())
        .then(|| message_samples.iter().sum::<u64>() as f64 / message_samples.len() as f64);
    let mut token_samples = rollups
        .iter()
        .map(|rollup| rollup.usage.computed_total())
        .collect::<Vec<_>>();
    let median_tokens = median_u64(&mut token_samples);
    let mut durations = rollups
        .iter()
        .filter_map(|rollup| rollup.duration_seconds)
        .collect::<Vec<_>>();
    let mut active = rollups
        .iter()
        .filter_map(|rollup| rollup.active_seconds)
        .collect::<Vec<_>>();
    // Each session's per-model usage, not its primary model's whole total: a
    // session that is 60% one model and 40% another counts both. A session
    // without per-model usage counts its primary model.
    let mut model_tokens: BTreeMap<String, u64> = BTreeMap::new();
    for rollup in rollups {
        let mut counted = false;
        for usage in &rollup.models {
            if let Some(label) = model_label(&usage.model) {
                let entry = model_tokens.entry(label).or_default();
                *entry = (*entry).saturating_add(usage.usage.computed_total());
                counted = true;
            }
        }
        if !counted {
            if let Some(model) = rollup.primary_model.clone() {
                let entry = model_tokens.entry(model).or_default();
                *entry = (*entry).saturating_add(rollup.usage.computed_total());
            }
        }
    }
    let top_model = model_tokens
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(model, _)| model);
    SessionStats {
        sessions,
        total_tokens,
        total_cost_micro_usd,
        // Lazily: `then_some` would divide by zero when nothing matches.
        avg_tokens: (sessions > 0).then(|| total_tokens / sessions),
        median_tokens,
        avg_messages,
        median_duration_seconds: median_u64(&mut durations),
        median_active_seconds: median_u64(&mut active),
        total_messages,
        top_model,
    }
}

fn median_u64(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some(values[mid - 1].saturating_add(values[mid]) / 2)
    }
}

fn primary_model_label(models: &[SummaryModelUsage]) -> Option<String> {
    models
        .iter()
        .filter_map(|usage| {
            model_label(&usage.model).map(|label| (label, usage.usage.computed_total()))
        })
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(label, _)| label)
}

fn model_label(model: &ModelInfo) -> Option<String> {
    model
        .normalized_name
        .clone()
        .or(model.name.clone())
        .or(model.provider_model_id.clone())
        .filter(|value| !value.trim().is_empty())
}

fn model_usage(model: ModelInfo, totals: ModelTotals) -> SummaryModelUsage {
    SummaryModelUsage {
        model,
        usage: UsageCounts {
            input_tokens: Some(totals.input_tokens),
            output_tokens: Some(totals.output_tokens),
            cache_creation_tokens: Some(totals.cache_creation_tokens),
            cache_creation_5m_tokens: Some(totals.cache_creation_5m_tokens),
            cache_creation_1h_tokens: Some(totals.cache_creation_1h_tokens),
            cache_read_tokens: Some(totals.cache_read_tokens),
            reasoning_tokens: Some(totals.reasoning_tokens),
            total_tokens: Some(totals.total_tokens),
            requests: Some(totals.requests),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        },
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: totals.estimated_cost.cents_rounded(),
            provider_reported_usd: totals
                .has_provider_reported
                .then(|| totals.provider_reported_cost.cents_rounded())
                .flatten(),
            estimated_api_equivalent_micro_usd: totals.estimated_cost.micro_usd(),
            provider_reported_micro_usd: totals
                .has_provider_reported
                .then(|| totals.provider_reported_cost.micro_usd())
                .flatten(),
            pricing_source: Some("local_rollup".to_string()),
            pricing_version: None,
            confidence: Confidence::Medium,
        },
        metrics: None,
    }
}

fn session_model_key(model: &ModelInfo) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        model.normalized_name.as_deref().unwrap_or(""),
        model.provider_model_id.as_deref().unwrap_or(""),
        model.name.as_deref().unwrap_or(""),
        model.speed.as_deref().unwrap_or(""),
        model
            .reasoning_level
            .as_ref()
            .map(|level| level.as_str())
            .unwrap_or(""),
        model.reasoning_level_raw.as_deref().unwrap_or("")
    )
}

#[derive(Debug, Default)]
struct UsageTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_creation_5m_tokens: u64,
    cache_creation_1h_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    requests: u64,
}

impl UsageTotals {
    fn add(&mut self, usage: &UsageCounts) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(usage.cache_creation_tokens.unwrap_or(0));
        self.cache_creation_5m_tokens = self
            .cache_creation_5m_tokens
            .saturating_add(usage.cache_creation_5m_tokens.unwrap_or(0));
        self.cache_creation_1h_tokens = self
            .cache_creation_1h_tokens
            .saturating_add(usage.cache_creation_1h_tokens.unwrap_or(0));
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        self.total_tokens = self.total_tokens.saturating_add(usage.computed_total());
    }

    fn into_counts(self) -> UsageCounts {
        UsageCounts {
            input_tokens: Some(self.input_tokens),
            output_tokens: Some(self.output_tokens),
            cache_creation_tokens: Some(self.cache_creation_tokens),
            cache_creation_5m_tokens: Some(self.cache_creation_5m_tokens),
            cache_creation_1h_tokens: Some(self.cache_creation_1h_tokens),
            cache_read_tokens: Some(self.cache_read_tokens),
            reasoning_tokens: Some(self.reasoning_tokens),
            total_tokens: Some(self.total_tokens),
            requests: Some(self.requests),
            local_prompt_eval_tokens: None,
            local_eval_tokens: None,
        }
    }
}

#[derive(Debug, Default)]
struct ModelTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_creation_5m_tokens: u64,
    cache_creation_1h_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    requests: u64,
    estimated_cost: CostAccumulator,
    provider_reported_cost: CostAccumulator,
    has_provider_reported: bool,
}

impl ModelTotals {
    fn add(&mut self, usage: &UsageCounts, requests: u64, cost: &CostInfo) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(usage.cache_creation_tokens.unwrap_or(0));
        self.cache_creation_5m_tokens = self
            .cache_creation_5m_tokens
            .saturating_add(usage.cache_creation_5m_tokens.unwrap_or(0));
        self.cache_creation_1h_tokens = self
            .cache_creation_1h_tokens
            .saturating_add(usage.cache_creation_1h_tokens.unwrap_or(0));
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        self.total_tokens = self.total_tokens.saturating_add(usage.computed_total());
        self.requests = self.requests.saturating_add(requests);
        self.estimated_cost.add_estimated(cost);
        if cost.provider_reported_micro_usd.is_some() || cost.provider_reported_usd.is_some() {
            self.provider_reported_cost
                .add_values(cost.provider_reported_micro_usd, cost.provider_reported_usd);
            self.has_provider_reported = true;
        }
    }
}
