//! The parsed pricing table, the copy compiled into this binary, and the
//! process-wide active table the free functions price with.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use chrono::NaiveDate;

use crate::catalog::ModelPricing;

/// Environment variable that turns the pricing-file refresh off.
pub const PRICING_REFRESH_ENV: &str = "STATSAI_PRICING_REFRESH";

const EMBEDDED_PRICING_TEXT: &str = include_str!("../pricing.json");

static EMBEDDED_PRICING: OnceLock<Arc<PricingTable>> = OnceLock::new();
static ACTIVE_PRICING: OnceLock<RwLock<Arc<PricingTable>>> = OnceLock::new();

/// A parsed and validated pricing file.
///
/// Built only by [`PricingTable::parse`], so every table in memory has passed
/// the same validation, whether it was compiled in, fetched, or read from a
/// store.
#[derive(Clone)]
pub struct PricingTable {
    pub(crate) text: String,
    pub(crate) format: u32,
    pub(crate) version: u64,
    pub(crate) catalog: String,
    pub(crate) models: HashMap<String, ModelEntry>,
    pub(crate) equivalents: HashMap<String, Periods<String>>,
    pub(crate) names: HashMap<String, String>,
    pub(crate) patterns: Vec<NamePattern>,
    pub(crate) reversed: HashMap<String, String>,
}

impl fmt::Debug for PricingTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PricingTable")
            .field("format", &self.format)
            .field("version", &self.version)
            .field("catalog", &self.catalog)
            .field("models", &self.models.len())
            .field("equivalents", &self.equivalents.len())
            .finish_non_exhaustive()
    }
}

/// A list of values, each in effect from its `from` UTC day until the next
/// period's. The first period has no start; later starts strictly increase.
#[derive(Debug, Clone)]
pub(crate) struct Periods<T>(pub(crate) Vec<Period<T>>);

#[derive(Debug, Clone)]
pub(crate) struct Period<T> {
    pub(crate) from: Option<NaiveDate>,
    pub(crate) value: T,
}

impl<T> Periods<T> {
    /// The value of the last period whose start is on or before `date`.
    pub(crate) fn on(&self, date: NaiveDate) -> &T {
        let period = self
            .0
            .iter()
            .rev()
            .find(|period| period.from.is_none_or(|from| from <= date))
            .unwrap_or(&self.0[0]);
        &period.value
    }

    /// The start dates of every period after the first.
    pub(crate) fn boundaries(&self) -> impl Iterator<Item = NaiveDate> + '_ {
        self.0.iter().filter_map(|period| period.from)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ModelEntry {
    /// Pricing-source prefix; `None` means `{provider}_api_pricing`.
    pub(crate) source: Option<String>,
    pub(crate) prices: Periods<ModelPricing>,
    pub(crate) long_context: Option<LongContext>,
    pub(crate) fast: Option<FastPricing>,
}

impl ModelEntry {
    /// Every date on which this model's standard or fast rates change.
    pub(crate) fn boundaries(&self) -> Vec<NaiveDate> {
        let mut boundaries: Vec<NaiveDate> = self.prices.boundaries().collect();
        if let Some(FastPricing {
            rates: FastRates::Prices(prices),
            ..
        }) = &self.fast
        {
            boundaries.extend(prices.boundaries());
        }
        boundaries
    }
}

/// A whole-request surcharge for one known request with a large prompt.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LongContext {
    pub(crate) threshold: TokenThreshold,
    /// Multiplies input, cache-write and cache-read rates.
    pub(crate) input: f64,
    /// Multiplies output and reasoning rates.
    pub(crate) output: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TokenThreshold {
    Above(u64),
    AtLeast(u64),
}

impl TokenThreshold {
    pub(crate) fn reached_by(self, prompt_tokens: u64) -> bool {
        match self {
            Self::Above(threshold) => prompt_tokens > threshold,
            Self::AtLeast(threshold) => prompt_tokens >= threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FastPricing {
    pub(crate) rates: FastRates,
    /// Fast mode does not serve one known request whose prompt exceeds this.
    pub(crate) max_prompt: Option<u64>,
    /// Replaces the model's long-context surcharge while fast rates apply.
    pub(crate) long_context: Option<LongContext>,
}

#[derive(Debug, Clone)]
pub(crate) enum FastRates {
    /// Every standard rate of the period in effect that day, times this.
    Multiplier(f64),
    Prices(Periods<ModelPricing>),
}

/// One ordered substring rule of the proxy-wrapped name normalizer.
#[derive(Debug, Clone)]
pub(crate) struct NamePattern {
    pub(crate) contains: Vec<String>,
    pub(crate) model: String,
    /// The match must not be followed by an ASCII digit.
    pub(crate) whole_version: bool,
}

impl PricingTable {
    /// The structure version of the file this table was parsed from.
    #[must_use]
    pub fn format(&self) -> u32 {
        self.format
    }

    /// The file's monotonic price version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The file's descriptive catalog identifier, written to
    /// `CostInfo.pricing_version`. Not an ordering key.
    #[must_use]
    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    /// The exact text this table was parsed from.
    #[must_use]
    pub fn source_text(&self) -> &str {
        &self.text
    }

    /// The catalog model that prices `name` (already lowercased) on `date`,
    /// following an `equivalents` mapping when one applies.
    pub(crate) fn priced_entry<'a>(
        &'a self,
        name: &'a str,
        date: NaiveDate,
    ) -> Option<(&'a str, &'a ModelEntry)> {
        let name = self
            .equivalents
            .get(name)
            .map_or(name, |periods| periods.on(date).as_str());
        self.models
            .get_key_value(name)
            .map(|(name, entry)| (name.as_str(), entry))
    }

    /// Every date on which the price of `name` (already normalized) changes.
    ///
    /// For an equivalent name these are its own mapping dates plus each
    /// target's dates that fall strictly inside the window where it applies.
    pub(crate) fn price_boundaries(&self, name: &str) -> Vec<NaiveDate> {
        let Some(periods) = self.equivalents.get(name) else {
            return self
                .models
                .get(name)
                .map(ModelEntry::boundaries)
                .unwrap_or_default();
        };
        let mut boundaries = Vec::new();
        for (index, period) in periods.0.iter().enumerate() {
            boundaries.extend(period.from);
            let until = periods.0.get(index + 1).and_then(|next| next.from);
            let Some(target) = self.models.get(&period.value) else {
                continue;
            };
            boundaries.extend(target.boundaries().into_iter().filter(|boundary| {
                period.from.is_none_or(|from| *boundary > from)
                    && until.is_none_or(|until| *boundary < until)
            }));
        }
        boundaries
    }
}

/// The text of the pricing file compiled into this binary.
#[must_use]
pub fn embedded_pricing_text() -> &'static str {
    EMBEDDED_PRICING_TEXT
}

/// The pricing file compiled into this binary.
///
/// # Panics
///
/// Panics if the compiled-in file is invalid; a test guarantees it is not.
#[must_use]
pub fn embedded_pricing() -> Arc<PricingTable> {
    Arc::clone(
        EMBEDDED_PRICING.get_or_init(|| match PricingTable::parse(EMBEDDED_PRICING_TEXT) {
            Ok(table) => Arc::new(table),
            Err(error) => panic!("the embedded pricing.json is invalid: {error}"),
        }),
    )
}

fn active_slot() -> &'static RwLock<Arc<PricingTable>> {
    ACTIVE_PRICING.get_or_init(|| RwLock::new(embedded_pricing()))
}

/// The table the free functions price with. Starts as [`embedded_pricing`].
#[must_use]
pub fn active_pricing() -> Arc<PricingTable> {
    Arc::clone(&active_slot().read().unwrap_or_else(PoisonError::into_inner))
}

/// Makes `table` the process's active table when its version is higher than
/// the active one. Returns whether it replaced it. Never lowers the version.
pub fn install_pricing(table: Arc<PricingTable>) -> bool {
    let mut active = active_slot()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    if table.version > active.version {
        *active = table;
        true
    } else {
        false
    }
}

/// True when `STATSAI_PRICING_REFRESH` is `0`, `off`, `false` or `no`
/// (trimmed, case-insensitive).
#[must_use]
pub fn refresh_disabled_by_env() -> bool {
    std::env::var_os(PRICING_REFRESH_ENV)
        .is_some_and(|value| value.to_str().is_some_and(refresh_value_disables))
}

pub(crate) fn refresh_value_disables(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}
