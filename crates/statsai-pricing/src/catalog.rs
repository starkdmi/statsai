use chrono::{NaiveDate, Utc};
use statsai_core::ModelInfo;

use crate::table::{active_pricing, FastRates, ModelEntry, PricingTable};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub cache_creation_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
}

impl ModelPricing {
    fn scaled(self, multiplier: f64) -> Self {
        Self {
            input_per_million: self.input_per_million * multiplier,
            cache_creation_per_million: self.cache_creation_per_million * multiplier,
            cached_input_per_million: self.cached_input_per_million * multiplier,
            output_per_million: self.output_per_million * multiplier,
        }
    }
}

/// The rates `entry` bills on `date` at the effective `speed`, and whether
/// they are fast-mode rates.
///
/// Effective `usage.speed` is authoritative: unsupported fast requests either
/// failed or reported `standard` after fallback, so a model with no fast rates
/// prices a fast request at standard.
pub(crate) fn pricing_for_effective_speed(
    entry: &ModelEntry,
    speed: Option<&str>,
    date: NaiveDate,
) -> (ModelPricing, bool) {
    let standard = *entry.prices.on(date);
    let is_fast = speed.is_some_and(|speed| speed.trim().eq_ignore_ascii_case("fast"));
    match (&entry.fast, is_fast) {
        (Some(fast), true) => {
            let rates = match &fast.rates {
                FastRates::Multiplier(multiplier) => standard.scaled(*multiplier),
                FastRates::Prices(prices) => *prices.on(date),
            };
            (rates, true)
        }
        _ => (standard, false),
    }
}

#[must_use]
pub fn pricing_for_model(model_name: &str) -> Option<ModelPricing> {
    pricing_for_model_on(model_name, Utc::now().date_naive())
}

pub(crate) fn pricing_for_model_on(
    model_name: &str,
    usage_date: NaiveDate,
) -> Option<ModelPricing> {
    active_pricing().pricing_for_model_on(model_name, usage_date)
}

/// Reports whether an aggregate usage period spans a known model-price change.
///
/// Aggregates without daily token allocation cannot be priced accurately across
/// these boundaries because the token share on each side is unknown.
#[must_use]
pub fn pricing_changes_between(
    model_name: &str,
    period_start: NaiveDate,
    period_end: NaiveDate,
) -> bool {
    active_pricing().pricing_changes_between(model_name, period_start, period_end)
}

impl PricingTable {
    /// The standard rates for `model` on the UTC day `date`, following an
    /// `equivalents` mapping when one applies.
    #[must_use]
    pub fn pricing_for_model_on(&self, model: &str, date: NaiveDate) -> Option<ModelPricing> {
        let lower = model.to_ascii_lowercase();
        self.priced_entry(&lower, date)
            .map(|(_, entry)| *entry.prices.on(date))
    }

    /// Reports whether an aggregate usage period spans a known model-price change.
    ///
    /// Aggregates without daily token allocation cannot be priced accurately
    /// across these boundaries because the token share on each side is unknown.
    #[must_use]
    pub fn pricing_changes_between(&self, model: &str, start: NaiveDate, end: NaiveDate) -> bool {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.price_boundaries(&self.normalize_model_name(model))
            .into_iter()
            .any(|boundary| start < boundary && boundary <= end)
    }

    pub(crate) fn priced_model_name(
        &self,
        model: &ModelInfo,
        usage_date: NaiveDate,
    ) -> Option<String> {
        // The observed name comes first because `normalized_name` only caches a
        // derivation of it. That cache is written once at scan time and goes stale
        // whenever the normalizer learns a name it used to fold into a coarser one,
        // and a stale cache that still resolves to a priced model would otherwise
        // shadow the correct match forever.
        let candidates = [
            model.name.as_deref(),
            model.provider_model_id.as_deref(),
            model.normalized_name.as_deref(),
        ];

        for candidate in candidates.into_iter().flatten() {
            let normalized = self.normalize_model_name(candidate);
            if self.pricing_for_model_on(&normalized, usage_date).is_some() {
                return Some(normalized);
            }
            if let Some((_, suffix)) = normalized.rsplit_once('/') {
                let suffix = self.normalize_model_name(suffix);
                if self.pricing_for_model_on(&suffix, usage_date).is_some() {
                    return Some(suffix);
                }
            }
        }

        None
    }
}
