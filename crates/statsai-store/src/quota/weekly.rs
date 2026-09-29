use super::*;
use crate::parse_rfc3339_for_row;
use anyhow::{bail, Context};
use chrono::NaiveDate;

/// Nominal length of a weekly reset cycle. Pure UTC; a device's timezone never
/// enters the arithmetic.
pub const WEEKLY_RESET_PERIOD_SECONDS: i64 = 604_800;

const MANUAL_WEEKLY_PROVIDER: &str = "claude_code";
const MANUAL_SCHEDULE_SOURCE: &str = "manual";

/// One manually entered weekly reset anchor.
///
/// Anchors that differ by whole weeks are the same schedule. The stored instant
/// is whichever whole-second UTC value was entered; readers normalize it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeeklyResetAnchor {
    pub provider: String,
    pub provider_account_id: ProviderAccountId,
    pub anchor: DateTime<Utc>,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The half-open cycle `[end - 7d, end)` that contains `instant`.
///
/// `end` is the unique instant strictly after `instant` with
/// `end ≡ anchor (mod 7d)`. An event timestamped exactly on a reset belongs to
/// the cycle that starts there.
pub fn weekly_cycle_containing(
    anchor: DateTime<Utc>,
    instant: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let end_epoch = cycle_end_after(anchor.timestamp(), instant.timestamp());
    let end = DateTime::from_timestamp(end_epoch, 0).expect("weekly cycle end");
    let start = DateTime::from_timestamp(end_epoch - WEEKLY_RESET_PERIOD_SECONDS, 0)
        .expect("weekly cycle start");
    (start, end)
}

impl Store {
    pub fn upsert_weekly_reset_anchor(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
        anchor: DateTime<Utc>,
    ) -> Result<WeeklyResetAnchor> {
        self.ensure_manual_weekly_provider(provider)?;
        let anchor = anchor
            .with_nanosecond(0)
            .expect("clearing subseconds stays in range");
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            r#"
            INSERT INTO weekly_reset_anchors (
              provider, provider_account_id, anchor_epoch_seconds, source, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
            ON CONFLICT(provider, provider_account_id) DO UPDATE SET
              anchor_epoch_seconds = excluded.anchor_epoch_seconds,
              source = excluded.source,
              updated_at = excluded.updated_at
            "#,
            params![
                MANUAL_WEEKLY_PROVIDER,
                &provider_account_id.0,
                anchor.timestamp(),
                MANUAL_SCHEDULE_SOURCE,
                now,
            ],
        )?;
        self.weekly_reset_anchor(provider, provider_account_id)?
            .context("weekly reset anchor missing after upsert")
    }

    pub fn weekly_reset_anchor(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
    ) -> Result<Option<WeeklyResetAnchor>> {
        if provider != MANUAL_WEEKLY_PROVIDER {
            return Ok(None);
        }
        self.conn
            .query_row(
                r#"
                SELECT provider, provider_account_id, anchor_epoch_seconds, source, created_at, updated_at
                FROM weekly_reset_anchors
                WHERE provider = ?1 AND provider_account_id = ?2
                "#,
                params![provider, &provider_account_id.0],
                |row| {
                    Ok(WeeklyResetAnchor {
                        provider: row.get(0)?,
                        provider_account_id: ProviderAccountId(row.get(1)?),
                        anchor: DateTime::from_timestamp(row.get(2)?, 0)
                            .expect("stored weekly reset anchor"),
                        source: row.get(3)?,
                        created_at: parse_rfc3339_for_row(&row.get::<_, String>(4)?, 4)?,
                        updated_at: parse_rfc3339_for_row(&row.get::<_, String>(5)?, 5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_weekly_reset_anchor(
        &self,
        provider: &str,
        provider_account_id: &ProviderAccountId,
    ) -> Result<bool> {
        let deleted = self.conn.execute(
            r#"
            DELETE FROM weekly_reset_anchors
            WHERE provider = ?1 AND provider_account_id = ?2
            "#,
            params![provider, &provider_account_id.0],
        )?;
        Ok(deleted > 0)
    }

    pub fn move_weekly_reset_anchor(
        &self,
        provider: &str,
        from_provider_account_id: &ProviderAccountId,
        to_provider_account_id: &ProviderAccountId,
    ) -> Result<bool> {
        let updated_at = Utc::now().to_rfc3339();
        let moved = self.conn.execute(
            r#"
            UPDATE weekly_reset_anchors
            SET provider_account_id = ?3, updated_at = ?4
            WHERE provider = ?1 AND provider_account_id = ?2
            "#,
            params![
                provider,
                &from_provider_account_id.0,
                &to_provider_account_id.0,
                updated_at,
            ],
        )?;
        Ok(moved > 0)
    }

    pub fn weekly_reset_anchor_count(
        &self,
        provider: Option<&str>,
        provider_account_id: &ProviderAccountId,
    ) -> Result<usize> {
        let count: i64 = if let Some(provider) = provider {
            self.conn.query_row(
                r#"
                SELECT COUNT(*) FROM weekly_reset_anchors
                WHERE provider = ?1 AND provider_account_id = ?2
                "#,
                params![provider, &provider_account_id.0],
                |row| row.get(0),
            )?
        } else {
            self.conn.query_row(
                r#"
                SELECT COUNT(*) FROM weekly_reset_anchors
                WHERE provider_account_id = ?1
                "#,
                params![&provider_account_id.0],
                |row| row.get(0),
            )?
        };
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Completed manual weekly contributions for the accounts that have an anchor.
    ///
    /// `now` decides which cycles have ended. The cycle that contains `now` is
    /// never emitted.
    pub(crate) fn manual_weekly_contributions(
        &self,
        query: &QuotaQuery,
        device_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<QuotaCycleContributionV1>> {
        if query
            .provider
            .as_deref()
            .is_some_and(|provider| provider != MANUAL_WEEKLY_PROVIDER)
        {
            return Ok(Vec::new());
        }
        let anchors = self.weekly_reset_anchors(query.provider_account_id.as_ref())?;
        let mut contributions = Vec::new();
        for anchor in anchors {
            contributions
                .extend(self.manual_weekly_contributions_for_anchor(device_id, &anchor, now)?);
        }
        Ok(contributions)
    }

    fn weekly_reset_anchors(
        &self,
        provider_account_id: Option<&ProviderAccountId>,
    ) -> Result<Vec<WeeklyResetAnchor>> {
        let mut statement = if provider_account_id.is_some() {
            self.conn.prepare(
                r#"
                SELECT provider, provider_account_id, anchor_epoch_seconds, source, created_at, updated_at
                FROM weekly_reset_anchors
                WHERE provider = ?1 AND provider_account_id = ?2
                ORDER BY provider_account_id
                "#,
            )?
        } else {
            self.conn.prepare(
                r#"
                SELECT provider, provider_account_id, anchor_epoch_seconds, source, created_at, updated_at
                FROM weekly_reset_anchors
                WHERE provider = ?1
                ORDER BY provider_account_id
                "#,
            )?
        };
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(WeeklyResetAnchor {
                provider: row.get(0)?,
                provider_account_id: ProviderAccountId(row.get(1)?),
                anchor: DateTime::from_timestamp(row.get(2)?, 0)
                    .expect("stored weekly reset anchor"),
                source: row.get(3)?,
                created_at: parse_rfc3339_for_row(&row.get::<_, String>(4)?, 4)?,
                updated_at: parse_rfc3339_for_row(&row.get::<_, String>(5)?, 5)?,
            })
        };
        let rows = if let Some(account_id) = provider_account_id {
            statement.query_map(params![MANUAL_WEEKLY_PROVIDER, &account_id.0], map_row)?
        } else {
            statement.query_map(params![MANUAL_WEEKLY_PROVIDER], map_row)?
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn manual_weekly_contributions_for_anchor(
        &self,
        device_id: &str,
        anchor: &WeeklyResetAnchor,
        now: DateTime<Utc>,
    ) -> Result<Vec<QuotaCycleContributionV1>> {
        let usage_days = self.claude_code_usage_days(&anchor.provider_account_id)?;
        let ends =
            emitted_manual_cycle_ends(anchor.anchor.timestamp(), now.timestamp(), &usage_days);
        if ends.is_empty() {
            return Ok(Vec::new());
        }
        let mut slices_by_end = BTreeMap::<i64, Vec<QuotaUsageSliceV1>>::new();
        for end_epoch in &ends {
            let start = DateTime::from_timestamp(end_epoch - WEEKLY_RESET_PERIOD_SECONDS, 0)
                .expect("weekly cycle start");
            let end = DateTime::from_timestamp(*end_epoch, 0).expect("weekly cycle end");
            let builders = boundary_slice_builders(
                start,
                end,
                MANUAL_WEEKLY_PROVIDER,
                &anchor.provider_account_id,
            );
            slices_by_end.insert(*end_epoch, builders.slices);
        }
        self.fill_manual_boundary_slices(&anchor.provider_account_id, &mut slices_by_end)?;

        let mut contributions = Vec::with_capacity(ends.len());
        for end_epoch in ends {
            let end = DateTime::from_timestamp(end_epoch, 0).expect("weekly cycle end");
            contributions.push(QuotaCycleContributionV1 {
                schema_version: QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION.to_string(),
                contribution_id: manual_contribution_id(
                    device_id,
                    &anchor.provider_account_id.0,
                    end_epoch,
                ),
                provider: MANUAL_WEEKLY_PROVIDER.to_string(),
                provider_account_id: anchor.provider_account_id.clone(),
                limit_id: None,
                window_minutes: QUOTA_WEEKLY_WINDOW_MINUTES,
                representative_reset: end,
                representative_reset_epoch_seconds: end_epoch,
                has_schedule_overlap: false,
                daily_envelopes: Vec::new(),
                boundary_slices: slices_by_end.remove(&end_epoch).unwrap_or_default(),
            });
        }
        Ok(contributions)
    }

    fn claude_code_usage_days(
        &self,
        provider_account_id: &ProviderAccountId,
    ) -> Result<BTreeSet<NaiveDate>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT DISTINCT substr(started_at, 1, 10)
            FROM usage_events
            WHERE provider = 'claude_code' AND provider_account_id = ?1
            "#,
        )?;
        let rows = statement.query_map(params![&provider_account_id.0], |row| {
            row.get::<_, String>(0)
        })?;
        let mut days = BTreeSet::new();
        for row in rows {
            let day = row?;
            if let Ok(parsed) = NaiveDate::parse_from_str(&day, "%Y-%m-%d") {
                days.insert(parsed);
            }
        }
        Ok(days)
    }

    fn fill_manual_boundary_slices(
        &self,
        provider_account_id: &ProviderAccountId,
        slices_by_end: &mut BTreeMap<i64, Vec<QuotaUsageSliceV1>>,
    ) -> Result<()> {
        let mut days = BTreeSet::new();
        for slices in slices_by_end.values() {
            for slice in slices {
                days.insert(slice.period_start.date_naive());
            }
        }
        if days.is_empty() {
            return Ok(());
        }
        let day_list = days.into_iter().collect::<Vec<_>>();
        for event in self.claude_code_events_on_utc_days(provider_account_id, &day_list)? {
            let estimated_cost = event.cost.estimated_micro_usd();
            for slices in slices_by_end.values_mut() {
                for slice in slices.iter_mut() {
                    if event.session.started_at < slice.period_start
                        || event.session.started_at >= slice.period_end
                    {
                        continue;
                    }
                    slice.add_usage(&event.usage, estimated_cost);
                }
            }
        }
        Ok(())
    }

    fn claude_code_events_on_utc_days(
        &self,
        provider_account_id: &ProviderAccountId,
        days: &[NaiveDate],
    ) -> Result<Vec<statsai_core::UsageEvent>> {
        if days.is_empty() {
            return Ok(Vec::new());
        }
        const CHUNK_SIZE: usize = 64;
        let mut events = Vec::new();
        for chunk in days.chunks(CHUNK_SIZE) {
            let mut sql = String::from(
                "SELECT payload FROM usage_events
                 WHERE provider = 'claude_code'
                   AND provider_account_id = ?1
                   AND (",
            );
            let mut bindings: Vec<Box<dyn rusqlite::types::ToSql>> =
                vec![Box::new(provider_account_id.0.clone())];
            for (index, day) in chunk.iter().enumerate() {
                if index > 0 {
                    sql.push_str(" OR ");
                }
                let start_index = bindings.len() + 1;
                let end_index = start_index + 1;
                sql.push_str(&format!(
                    "(started_at >= ?{start_index} AND started_at < ?{end_index})"
                ));
                let start = day.and_hms_opt(0, 0, 0).expect("utc midnight").and_utc();
                let end = start + Duration::days(1);
                bindings.push(Box::new(start.to_rfc3339()));
                bindings.push(Box::new(end.to_rfc3339()));
            }
            sql.push_str(") ORDER BY started_at, event_id");
            let mut statement = self.conn.prepare(&sql)?;
            let params = bindings
                .iter()
                .map(|value| value.as_ref() as &dyn rusqlite::types::ToSql)
                .collect::<Vec<_>>();
            let rows = statement.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(events)
    }

    fn ensure_manual_weekly_provider(&self, provider: &str) -> Result<()> {
        if provider != MANUAL_WEEKLY_PROVIDER {
            bail!("weekly reset anchors are only supported for claude_code");
        }
        Ok(())
    }
}

fn cycle_end_after(anchor_epoch: i64, instant_epoch: i64) -> i64 {
    let phase = anchor_epoch.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS);
    let instant_mod = instant_epoch.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS);
    if instant_mod < phase {
        instant_epoch - instant_mod + phase
    } else {
        instant_epoch - instant_mod + phase + WEEKLY_RESET_PERIOD_SECONDS
    }
}

fn first_weekly_end_on_or_after(anchor_epoch: i64, instant_epoch: i64) -> i64 {
    let phase = anchor_epoch.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS);
    let instant_mod = instant_epoch.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS);
    let delta = (phase - instant_mod).rem_euclid(WEEKLY_RESET_PERIOD_SECONDS);
    instant_epoch + delta
}

/// Completed cycle ends a usage day can force this device to emit.
///
/// A day triggers every cycle whose start day or end day is that UTC day, which
/// is the half-open epoch window `[day_start, day_start + 8d)`.
pub(crate) fn emitted_manual_cycle_ends(
    anchor_epoch: i64,
    now_epoch: i64,
    usage_days: &BTreeSet<NaiveDate>,
) -> Vec<i64> {
    let mut ends = BTreeSet::new();
    for day in usage_days {
        let day_start = day
            .and_hms_opt(0, 0, 0)
            .expect("utc midnight")
            .and_utc()
            .timestamp();
        let window_end = day_start + 86_400 + WEEKLY_RESET_PERIOD_SECONDS;
        let mut end = first_weekly_end_on_or_after(anchor_epoch, day_start);
        while end < window_end {
            if end <= now_epoch {
                ends.insert(end);
            }
            end += WEEKLY_RESET_PERIOD_SECONDS;
        }
    }
    ends.into_iter().collect()
}

fn manual_contribution_id(device_id: &str, account_id: &str, end_epoch: i64) -> String {
    let digest = hash_text(&format!(
        "quota_cycle_contribution.v1:{device_id}:claude_code:{account_id}:default:10080:manual:{end_epoch}"
    ));
    format!("quota_cycle_{}", &digest[..32])
}
