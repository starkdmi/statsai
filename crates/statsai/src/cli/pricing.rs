//! `statsai pricing`: the pricing file a store prices with, and its refresh.
//! See `docs/pricing.md`.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use statsai::log_repricing_report;
use statsai::pricing_refresh::{
    pricing_url, refresh_pricing_with, run_background_refresh, RefreshBookkeeping, RefreshOutcome,
    FETCH_TIMEOUT,
};
use statsai_store::{
    embedded_pricing, refresh_disabled_by_env, PricingRefreshState, Store, StoredPricing,
    UnsupportedPricingFormat, APPLIED_PRICING_CATALOG_VERSION_KEY, PRICING_REFRESH_ENV,
};
use std::path::Path;
use std::time::Duration;

use super::args::{PricingCommand, PricingSubcommand};
use super::format::format_local_timestamp;

pub(crate) fn pricing(command: PricingCommand, store_path: &Path) -> Result<()> {
    if let PricingSubcommand::Refresh { background: true } = command.command {
        run_background_pricing_refresh(store_path);
        return Ok(());
    }
    let store = Store::open(store_path)?;
    match command.command {
        PricingSubcommand::Status { json } => {
            let status = pricing_status_with(&store, refresh_disabled_by_env(), pricing_url())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                for line in status.lines() {
                    println!("{line}");
                }
            }
            Ok(())
        }
        // `--background` returned above.
        PricingSubcommand::Refresh { .. } => {
            let refresh = store.pricing_refresh_state()?;
            for line in run_pricing_refresh(&store, refresh, &pricing_url(), FETCH_TIMEOUT)? {
                println!("{line}");
            }
            Ok(())
        }
        PricingSubcommand::AutoRefresh { state } => {
            for line in set_auto_refresh(&store, state.is_on(), refresh_disabled_by_env())? {
                println!("{line}");
            }
            Ok(())
        }
    }
}

/// `statsai pricing refresh --background`, the process `statsai scan` and
/// `statsai sync` start ([`start_background_refresh`]): one refresh on the
/// daily schedule that prints nothing and never fails, a panic included, and
/// never reprices. It starts no other process.
///
/// [`start_background_refresh`]: statsai::pricing_refresh::start_background_refresh
fn run_background_pricing_refresh(store_path: &Path) {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| run_background_refresh(store_path));
}

/// What `statsai pricing status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PricingStatus {
    /// The file built into this binary.
    pub(crate) embedded: PricingFileStatus,
    /// The file kept in the store.
    pub(crate) stored: StoredPricingStatus,
    /// The store's applied pricing version
    /// ([`statsai_store::Store::applied_pricing_ruleset_version`]).
    pub(crate) applied_version: Option<u64>,
    /// That version's catalog.
    pub(crate) applied_catalog: Option<String>,
    /// The file commands run in this environment price with.
    pub(crate) prices_with: PricesWith,
    pub(crate) refresh: RefreshStatus,
    /// The last completed refresh attempt.
    pub(crate) last_checked_at: Option<DateTime<Utc>>,
    /// The last started refresh attempt.
    pub(crate) last_attempt_at: Option<DateTime<Utc>>,
    /// Whether an `ETag` is stored for the next request.
    pub(crate) etag_stored: bool,
    /// Where the refresh fetches the file.
    pub(crate) url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PricingFileStatus {
    pub(crate) version: u64,
    pub(crate) format: u32,
    pub(crate) catalog: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum StoredPricingStatus {
    None,
    Usable {
        version: u64,
        format: u32,
        catalog: String,
    },
    UnknownFormat {
        version: u64,
        format: u64,
    },
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PricesWith {
    /// Commands price with this file: `embedded` or `stored`.
    File {
        version: u64,
        catalog: String,
        source: &'static str,
    },
    /// Price-derived commands refuse the store.
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RefreshStatus {
    pub(crate) on: bool,
    /// What turned it off: `setting` or `environment`.
    pub(crate) off_by: Option<&'static str>,
    /// The store's `pricing.refresh` setting.
    pub(crate) setting_on: bool,
    /// Whether `STATSAI_PRICING_REFRESH` turns it off for this process.
    pub(crate) disabled_by_environment: bool,
}

/// The pricing status of `store` for a process whose environment does or does
/// not turn the refresh off, fetching from `url`.
pub(crate) fn pricing_status_with(
    store: &Store,
    disabled_by_env: bool,
    url: String,
) -> Result<PricingStatus> {
    let embedded = embedded_pricing();
    let setting_on = store.pricing_refresh_setting()?;
    let refresh = PricingRefreshState::resolve(disabled_by_env, setting_on);
    let stored = match store.stored_pricing()? {
        StoredPricing::None => StoredPricingStatus::None,
        StoredPricing::Usable(table) => StoredPricingStatus::Usable {
            version: table.version(),
            format: table.format(),
            catalog: table.catalog().to_string(),
        },
        StoredPricing::UnknownFormat { format, version } => {
            StoredPricingStatus::UnknownFormat { version, format }
        }
        StoredPricing::Unreadable(reason) => StoredPricingStatus::Unreadable { reason },
    };
    let prices_with = match store.current_pricing_table_with(refresh) {
        Ok(table) => PricesWith::File {
            version: table.version(),
            catalog: table.catalog().to_string(),
            source: if table.version() > embedded.version() {
                "stored"
            } else {
                "embedded"
            },
        },
        Err(error) => match error.downcast_ref::<UnsupportedPricingFormat>() {
            Some(unsupported) => PricesWith::Refused {
                reason: unsupported.to_string(),
            },
            None => return Err(error),
        },
    };
    let bookkeeping = RefreshBookkeeping::read(store)?;
    Ok(PricingStatus {
        embedded: PricingFileStatus {
            version: embedded.version(),
            format: embedded.format(),
            catalog: embedded.catalog().to_string(),
        },
        stored,
        applied_version: store.applied_pricing_ruleset_version()?,
        applied_catalog: store.metadata_value(APPLIED_PRICING_CATALOG_VERSION_KEY)?,
        prices_with,
        refresh: RefreshStatus {
            on: refresh.is_on(),
            off_by: match refresh {
                PricingRefreshState::On => None,
                PricingRefreshState::OffBySetting => Some("setting"),
                PricingRefreshState::OffByEnvironment => Some("environment"),
            },
            setting_on,
            disabled_by_environment: disabled_by_env,
        },
        last_checked_at: bookkeeping.checked_at,
        last_attempt_at: bookkeeping.attempt_at,
        etag_stored: bookkeeping.etag.is_some(),
        url,
    })
}

impl PricingStatus {
    /// The human-readable report, one line each.
    pub(crate) fn lines(&self) -> Vec<String> {
        let embedded = &self.embedded;
        let stored = match &self.stored {
            StoredPricingStatus::None => "none".to_string(),
            StoredPricingStatus::Usable {
                version,
                format,
                catalog,
            } => format!("version {version}, format {format} ({catalog})"),
            StoredPricingStatus::UnknownFormat { version, format } => format!(
                "version {version}, format {format}, which this binary cannot read (it reads format {}); upgrade StatsAI",
                embedded.format
            ),
            StoredPricingStatus::Unreadable { reason } => format!("unreadable, ignored: {reason}"),
        };
        let applied = match (self.applied_version, self.applied_catalog.as_deref()) {
            (Some(version), Some(catalog)) => format!("version {version} ({catalog})"),
            (Some(version), None) => format!("version {version}"),
            (None, _) => "not priced yet".to_string(),
        };
        let prices_with = match &self.prices_with {
            PricesWith::File {
                version,
                catalog,
                source,
            } => {
                let source = if *source == "stored" {
                    "the stored file"
                } else {
                    "the file built into this binary"
                };
                format!("version {version} ({catalog}), {source}")
            }
            PricesWith::Refused { reason } => format!("none: {reason}"),
        };
        let refresh = match self.refresh.off_by {
            None => "on".to_string(),
            Some("setting") => {
                "off: this store's setting is off; `statsai pricing auto-refresh on` turns it on"
                    .to_string()
            }
            Some(_) => format!(
                "off: {PRICING_REFRESH_ENV} turns it off in this environment; unset it to use the store's setting ({})",
                if self.refresh.setting_on { "on" } else { "off" }
            ),
        };
        let time = |time: Option<DateTime<Utc>>| {
            time.map_or_else(|| "never".to_string(), format_local_timestamp)
        };
        vec![
            format!(
                "embedded pricing file: version {}, format {} ({})",
                embedded.version, embedded.format, embedded.catalog
            ),
            format!("stored pricing file: {stored}"),
            format!("store's applied pricing: {applied}"),
            format!("commands price with: {prices_with}"),
            format!("refresh: {refresh}"),
            format!("last check: {}", time(self.last_checked_at)),
            format!("last attempt: {}", time(self.last_attempt_at)),
            format!(
                "etag stored: {}",
                if self.etag_stored { "yes" } else { "no" }
            ),
            format!("url: {}", self.url),
        ]
    }
}

/// Why `statsai pricing refresh` will not run, and how to turn the refresh on.
fn refresh_off_message(refresh: PricingRefreshState) -> Option<String> {
    match refresh {
        PricingRefreshState::On => None,
        PricingRefreshState::OffBySetting => Some(
            "the pricing refresh is off for this store; run `statsai pricing auto-refresh on` to turn it on".to_string(),
        ),
        PricingRefreshState::OffByEnvironment => Some(format!(
            "the pricing refresh is off because {PRICING_REFRESH_ENV} turns it off in this environment; unset {PRICING_REFRESH_ENV} to turn it on"
        )),
    }
}

/// `statsai pricing refresh`: fetches the file from `url` now, ignoring the
/// daily schedule, and brings the store's estimates to the newest pricing it
/// can use. Returns the lines to print.
///
/// # Errors
///
/// When the refresh is off, the file is refused, or the request fails, with
/// the reason.
pub(crate) fn run_pricing_refresh(
    store: &Store,
    refresh: PricingRefreshState,
    url: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    if let Some(message) = refresh_off_message(refresh) {
        bail!(message);
    }
    let outcome = refresh_pricing_with(store, url, Utc::now(), true, refresh, timeout)?;
    let current = |detail: String| -> Result<Vec<String>> {
        let report = store.ensure_current_pricing_with(refresh)?;
        log_repricing_report(&report);
        Ok(vec![format!(
            "Pricing is current at version {} ({}); {detail}.",
            report.version, report.catalog
        )])
    };
    match outcome {
        RefreshOutcome::Adopted { version } => {
            let report = store.ensure_current_pricing_with(refresh)?;
            Ok(vec![
                format!("Stored pricing file version {version} from {url}."),
                report.to_string(),
            ])
        }
        RefreshOutcome::NotModified => current(format!(
            "the file at {url} has not changed since the last check"
        )),
        RefreshOutcome::NotNewer { version, .. } => {
            current(format!("the file at {url} is version {version}"))
        }
        RefreshOutcome::Rejected(reason) => {
            bail!("did not use the pricing file from {url}: {reason}")
        }
        RefreshOutcome::Failed(reason) => {
            bail!("could not fetch the pricing file from {url}: {reason}")
        }
        RefreshOutcome::Disabled(state) => {
            bail!(refresh_off_message(state)
                .unwrap_or_else(|| "the pricing refresh is off".to_string()))
        }
        RefreshOutcome::NotDue => bail!("the pricing refresh is not due"),
    }
}

/// `statsai pricing auto-refresh <on|off>`: sets the store's setting and
/// returns the lines to print, noting when the environment overrides it.
pub(crate) fn set_auto_refresh(
    store: &Store,
    on: bool,
    disabled_by_env: bool,
) -> Result<Vec<String>> {
    store.set_pricing_refresh_setting(on)?;
    let mut lines = vec![if on {
        "Pricing auto-refresh is on for this store: StatsAI fetches the pricing file from GitHub at most once a day and prices with the newest copy it can read.".to_string()
    } else {
        "Pricing auto-refresh is off for this store: StatsAI does not fetch the pricing file and prices with the file built into the binary. A store already priced with a newer stored file keeps that file, so its prices never go back.".to_string()
    }];
    if disabled_by_env && on {
        lines.push(format!(
            "Note: {PRICING_REFRESH_ENV} turns the refresh off in this environment, which overrides the setting; unset it to refresh."
        ));
    }
    Ok(lines)
}
