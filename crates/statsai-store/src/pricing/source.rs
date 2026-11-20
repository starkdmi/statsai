//! Which pricing table a store prices with: the refresh setting, the copy of
//! the pricing file kept in the store, and adopting a newer file.
//!
//! The store keeps at most one pricing file, under [`STORED_PRICING_FILE_KEY`].
//! It is only ever replaced by a file that parses, has a format this binary
//! reads, and has a higher `version` than the copy already there. While the
//! refresh is on, a process prices with the newer of the file embedded in its
//! binary and that stored copy; while it is off, with the embedded file only.

use crate::{Store, PRICING_REFRESH_SETTING_KEY, STORED_PRICING_FILE_KEY};
use anyhow::Result;
use statsai_pricing::{
    active_pricing, embedded_pricing, install_pricing, refresh_disabled_by_env, PricingFileError,
    PricingTable, PRICING_FORMAT,
};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

/// Whether a process uses a refreshed pricing file, and if not, what turned
/// the refresh off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingRefreshState {
    /// The newer of the embedded file and the store's copy prices.
    On,
    /// The store's `pricing.refresh` setting is `off`.
    OffBySetting,
    /// `STATSAI_PRICING_REFRESH` turns the refresh off for this process.
    OffByEnvironment,
}

impl PricingRefreshState {
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, Self::On)
    }

    /// The state for a process whose environment does or does not turn the
    /// refresh off, against a store whose setting is on or off. The
    /// environment is reported first: it applies to every store.
    #[must_use]
    pub fn resolve(env_disabled: bool, setting_on: bool) -> Self {
        if env_disabled {
            Self::OffByEnvironment
        } else if setting_on {
            Self::On
        } else {
            Self::OffBySetting
        }
    }
}

/// The pricing file kept in a store, as this binary reads it.
#[derive(Debug, Clone)]
pub enum StoredPricing {
    /// No file is stored.
    None,
    /// The stored file parses and has a format this binary reads.
    Usable(Arc<PricingTable>),
    /// The stored file has a format this binary cannot read. Price-derived
    /// commands refuse such a store.
    UnknownFormat { format: u64, version: u64 },
    /// The stored file does not parse for another reason. It is ignored, and
    /// a newer valid file may replace it.
    Unreadable(String),
}

impl StoredPricing {
    /// The version this copy counts as when deciding whether another file is
    /// newer: a usable copy's, or an unknown format's, whose prices a newer
    /// binary may read. An unreadable copy does not count.
    #[must_use]
    pub fn counted_version(&self) -> Option<u64> {
        match self {
            Self::Usable(table) => Some(table.version()),
            Self::UnknownFormat { version, .. } => Some(*version),
            Self::None | Self::Unreadable(_) => None,
        }
    }
}

/// What [`Store::offer_pricing_file`] did with a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingOffer {
    /// The file was newer than every copy this process knows, and is now the
    /// store's copy.
    Stored { version: u64 },
    /// The file is not newer than `newest`, the higher of the embedded file's
    /// version and the stored copy's. Nothing changed.
    NotNewer { version: u64, newest: u64 },
    /// The file has a format this binary cannot read. Nothing changed.
    UnknownFormat { format: u64, version: u64 },
    /// The file failed validation. Nothing changed.
    Invalid(String),
}

/// A store keeps a pricing file whose format this binary cannot read, so it
/// cannot price consistently with the binary that stored it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedPricingFormat {
    /// The stored file's format.
    pub format: u64,
    /// The stored file's version.
    pub version: u64,
}

impl fmt::Display for UnsupportedPricingFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "this store's prices come from pricing file format {}, which this StatsAI binary cannot read (it reads format {PRICING_FORMAT}); upgrade StatsAI",
            self.format
        )
    }
}

impl std::error::Error for UnsupportedPricingFormat {}

/// The pricing table a store resolves to before the unknown-format policy of
/// the caller applies.
enum ResolvedPricing {
    Table(Arc<PricingTable>),
    Unsupported(UnsupportedPricingFormat),
}

/// The last stored file text this process classified, with the result.
///
/// The watch daemon resolves the store's pricing before every scan pass. The
/// stored copy rarely changes, so a text equal to the last one is not parsed
/// again. Keyed by the whole text, so two stores with different copies only
/// cost a parse each, never a wrong answer.
static LAST_STORED_PRICING: Mutex<Option<(String, StoredPricing)>> = Mutex::new(None);

/// The last warning printed about an unreadable stored copy, so a daemon does
/// not repeat it on every pass.
static LAST_UNREADABLE_WARNING: Mutex<Option<String>> = Mutex::new(None);

fn classify_stored_pricing(text: &str) -> StoredPricing {
    for known in [active_pricing(), embedded_pricing()] {
        if known.source_text() == text {
            return StoredPricing::Usable(known);
        }
    }
    if let Some((cached_text, classified)) = LAST_STORED_PRICING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        if cached_text == text {
            return classified.clone();
        }
    }
    let classified = match PricingTable::header(text) {
        Err(error) => StoredPricing::Unreadable(error.to_string()),
        Ok(header) => match PricingTable::parse(text) {
            Ok(table) => StoredPricing::Usable(Arc::new(table)),
            Err(PricingFileError::UnknownFormat { format, .. }) => StoredPricing::UnknownFormat {
                format,
                version: header.version,
            },
            Err(error @ PricingFileError::Invalid(_)) => {
                StoredPricing::Unreadable(error.to_string())
            }
        },
    };
    remember_stored_pricing(text, &classified);
    classified
}

fn remember_stored_pricing(text: &str, classified: &StoredPricing) {
    *LAST_STORED_PRICING
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((text.to_string(), classified.clone()));
}

fn warn_unreadable_stored_pricing(reason: &str) {
    let mut last = LAST_UNREADABLE_WARNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if last.as_deref() != Some(reason) {
        eprintln!("warning: ignoring the pricing file stored in this store: {reason}");
        *last = Some(reason.to_string());
    }
}

impl Store {
    /// The store's pricing refresh setting. Absent, or any value other than
    /// `off`, means on.
    pub fn pricing_refresh_setting(&self) -> Result<bool> {
        Ok(crate::snapshot::pricing_refresh_setting_is_on(
            self.metadata_value(PRICING_REFRESH_SETTING_KEY)?.as_deref(),
        ))
    }

    /// Turns the store's pricing refresh setting on or off.
    pub fn set_pricing_refresh_setting(&self, on: bool) -> Result<()> {
        self.set_metadata_value(PRICING_REFRESH_SETTING_KEY, if on { "on" } else { "off" })
    }

    /// Whether this process uses a refreshed pricing file with this store:
    /// `STATSAI_PRICING_REFRESH` first, then the store's setting.
    pub fn pricing_refresh_state(&self) -> Result<PricingRefreshState> {
        if refresh_disabled_by_env() {
            return Ok(PricingRefreshState::OffByEnvironment);
        }
        Ok(PricingRefreshState::resolve(
            false,
            self.pricing_refresh_setting()?,
        ))
    }

    /// The pricing file kept in the store, read and validated.
    pub fn stored_pricing(&self) -> Result<StoredPricing> {
        Ok(self
            .metadata_value(STORED_PRICING_FILE_KEY)?
            .map_or(StoredPricing::None, |text| classify_stored_pricing(&text)))
    }

    /// Keeps `text` as the store's pricing file when it is a valid file of a
    /// known format and newer than both the embedded file and the stored
    /// copy.
    ///
    /// It only stores the file: the applied version and every priced row stay
    /// as they are until the store is next repriced.
    pub fn offer_pricing_file(&self, text: &str) -> Result<PricingOffer> {
        let table = match PricingTable::parse(text) {
            Ok(table) => Arc::new(table),
            Err(PricingFileError::UnknownFormat { format, .. }) => {
                let version = PricingTable::header(text).map_or(0, |header| header.version);
                return Ok(PricingOffer::UnknownFormat { format, version });
            }
            Err(PricingFileError::Invalid(message)) => return Ok(PricingOffer::Invalid(message)),
        };
        let version = table.version();
        let newest = self.newest_known_pricing_version()?;
        if version <= newest {
            return Ok(PricingOffer::NotNewer { version, newest });
        }
        let offer = self.with_immediate_transaction(|| {
            let newest = self.newest_known_pricing_version()?;
            if version <= newest {
                return Ok(PricingOffer::NotNewer { version, newest });
            }
            self.set_metadata_value(STORED_PRICING_FILE_KEY, text)?;
            Ok(PricingOffer::Stored { version })
        })?;
        if matches!(offer, PricingOffer::Stored { .. }) {
            remember_stored_pricing(text, &StoredPricing::Usable(table));
        }
        Ok(offer)
    }

    /// The higher of the embedded file's version and the stored copy's.
    fn newest_known_pricing_version(&self) -> Result<u64> {
        let embedded = embedded_pricing().version();
        Ok(self
            .stored_pricing()?
            .counted_version()
            .map_or(embedded, |stored| stored.max(embedded)))
    }

    /// Writes `table`'s text as the store's pricing file when it is newer than
    /// the stored copy, or when nothing usable is stored, so other binaries
    /// sharing the store price with it too.
    pub(crate) fn store_pricing_file_if_newer(&self, table: &PricingTable) -> Result<bool> {
        if self
            .stored_pricing()?
            .counted_version()
            .is_some_and(|stored| stored >= table.version())
        {
            return Ok(false);
        }
        self.set_metadata_value(STORED_PRICING_FILE_KEY, table.source_text())?;
        Ok(true)
    }

    /// The pricing table this process prices the store with, given the
    /// refresh state from [`Store::pricing_refresh_state`].
    pub fn current_pricing_table(&self) -> Result<Arc<PricingTable>> {
        self.current_pricing_table_with(self.pricing_refresh_state()?)
    }

    /// The pricing table this process prices the store with: while `refresh`
    /// is on, the newer of the embedded file and a usable stored copy; while it
    /// is off, the embedded file.
    ///
    /// # Errors
    ///
    /// [`UnsupportedPricingFormat`] when the stored copy has a format this
    /// binary cannot read, whatever the refresh state. An unreadable stored
    /// copy is ignored with a warning.
    pub fn current_pricing_table_with(
        &self,
        refresh: PricingRefreshState,
    ) -> Result<Arc<PricingTable>> {
        match self.resolve_pricing_table(refresh)? {
            ResolvedPricing::Table(table) => Ok(table),
            ResolvedPricing::Unsupported(error) => Err(error.into()),
        }
    }

    /// Makes the table this process prices the store with the process's
    /// active table, without repricing anything. For commands that read
    /// costs but do not reprice the store.
    ///
    /// A stored copy whose format this binary cannot read is not an error
    /// here: it warns and uses the embedded file.
    pub fn use_current_pricing(&self) -> Result<Arc<PricingTable>> {
        let table = match self.resolve_pricing_table(self.pricing_refresh_state()?)? {
            ResolvedPricing::Table(table) => table,
            ResolvedPricing::Unsupported(error) => {
                eprintln!("warning: {error}; pricing with the file built into this binary");
                embedded_pricing()
            }
        };
        install_pricing(Arc::clone(&table));
        Ok(table)
    }

    fn resolve_pricing_table(&self, refresh: PricingRefreshState) -> Result<ResolvedPricing> {
        let embedded = embedded_pricing();
        Ok(match self.stored_pricing()? {
            StoredPricing::UnknownFormat { format, version } => {
                ResolvedPricing::Unsupported(UnsupportedPricingFormat { format, version })
            }
            StoredPricing::Unreadable(reason) => {
                warn_unreadable_stored_pricing(&reason);
                ResolvedPricing::Table(embedded)
            }
            StoredPricing::Usable(stored)
                if refresh.is_on() && stored.version() > embedded.version() =>
            {
                ResolvedPricing::Table(stored)
            }
            StoredPricing::Usable(_) | StoredPricing::None => ResolvedPricing::Table(embedded),
        })
    }
}
