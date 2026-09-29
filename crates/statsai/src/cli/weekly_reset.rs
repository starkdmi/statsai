use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::Serialize;
use statsai_core::{display_account_identity, ProviderAccount};
use statsai_store::{
    weekly_cycle_containing, Store, WeeklyResetAnchor, WEEKLY_RESET_PERIOD_SECONDS,
};

use super::account::resolve_existing_provider_account_selector;
use super::args::WeeklyResetSubcommand;
use super::source::canonical_provider;

const MANUAL_WEEKLY_PROVIDER: &str = "claude_code";

pub(crate) fn weekly_reset(command: WeeklyResetSubcommand, store: &Store) -> Result<()> {
    let view = match command {
        WeeklyResetSubcommand::Set {
            provider,
            account,
            at,
        } => {
            let anchor = parse_weekly_reset_timestamp(&at)?;
            let account = resolve_manual_weekly_account(store, &provider, &account)?;
            let stored = store.upsert_weekly_reset_anchor(
                MANUAL_WEEKLY_PROVIDER,
                &account.provider_account_id,
                anchor,
            )?;
            weekly_reset_view(&account, &stored, Utc::now())
        }
        WeeklyResetSubcommand::Show { provider, account } => {
            let account = resolve_manual_weekly_account(store, &provider, &account)?;
            let stored = store
                .weekly_reset_anchor(MANUAL_WEEKLY_PROVIDER, &account.provider_account_id)?
                .context("no weekly reset anchor for this account")?;
            weekly_reset_view(&account, &stored, Utc::now())
        }
        WeeklyResetSubcommand::Clear { provider, account } => {
            let account = resolve_manual_weekly_account(store, &provider, &account)?;
            let cleared = store
                .delete_weekly_reset_anchor(MANUAL_WEEKLY_PROVIDER, &account.provider_account_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&WeeklyResetClear {
                    provider: MANUAL_WEEKLY_PROVIDER.to_string(),
                    provider_account_id: account.provider_account_id.0,
                    cleared,
                })?
            );
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WeeklyResetView {
    pub(crate) provider: String,
    pub(crate) provider_account_id: String,
    pub(crate) account: String,
    /// The end of the cycle that contains `now`. Equivalent anchors, including
    /// ones that differ by whole weeks, print this same instant.
    pub(crate) anchor: String,
    pub(crate) source: String,
    pub(crate) current_cycle_start: String,
    pub(crate) current_cycle_end: String,
}

#[derive(Debug, Serialize)]
struct WeeklyResetClear {
    provider: String,
    provider_account_id: String,
    cleared: bool,
}

pub(crate) fn weekly_reset_view(
    account: &ProviderAccount,
    stored: &WeeklyResetAnchor,
    now: DateTime<Utc>,
) -> WeeklyResetView {
    let (start, end) = weekly_cycle_containing(stored.anchor, now);
    WeeklyResetView {
        provider: stored.provider.clone(),
        provider_account_id: account.provider_account_id.0.clone(),
        account: display_account_identity(account),
        anchor: format_utc(end),
        source: stored.source.clone(),
        current_cycle_start: format_utc(start),
        current_cycle_end: format_utc(end),
    }
}

pub(crate) fn parse_weekly_reset_timestamp(value: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value.trim())
        .context("--at must be an RFC3339 timestamp with an explicit offset or Z")?;
    let utc = parsed.with_timezone(&Utc);
    Ok(utc
        .with_nanosecond(0)
        .expect("clearing subseconds stays in range"))
}

pub(crate) fn resolve_manual_weekly_account(
    store: &Store,
    provider: &str,
    selector: &str,
) -> Result<ProviderAccount> {
    let provider = canonical_provider(provider)?;
    if provider != MANUAL_WEEKLY_PROVIDER {
        bail!("weekly reset anchors are only supported for claude_code");
    }
    resolve_existing_provider_account_selector(store, &provider, selector)
}

fn format_utc(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn weekly_reset_phases_match(left: i64, right: i64) -> bool {
    left.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS) == right.rem_euclid(WEEKLY_RESET_PERIOD_SECONDS)
}
