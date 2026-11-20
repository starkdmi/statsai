//! Model pricing helpers for `statsai`.
//!
//! Provides model pricing lookup and cost estimation decoupled from any
//! specific adapter.
//!
//! Every rate, effective date, model alias and tier or fast-mode parameter
//! lives in the pricing data file, `pricing.json` at the root of this crate.
//! A copy is compiled into the binary ([`embedded_pricing`]); a newer copy can
//! be parsed at run time with [`PricingTable::parse`] and made the process's
//! active table with [`install_pricing`]. The free functions such as
//! [`estimate_cost_at`] price with the active table ([`active_pricing`]).
//!
//! The file's `version` is the monotonic identity of the prices: it goes up on
//! every change that can change an estimated cost, so persisted stores can
//! reprice automatically. Its `catalog` string is descriptive and is not an
//! ordering key. The mechanics that interpret the data (tier and multiplier
//! arithmetic, request gating, Cursor's own Grok 4.7 tier, Claude's 1-hour
//! cache writes) stay in code.

mod catalog;
mod cost;
mod file;
mod normalize;
mod table;

pub use catalog::{pricing_changes_between, pricing_for_model, ModelPricing};
pub use cost::{
    estimate_cost, estimate_cost_at, estimate_priced_from_source_records, overlay_estimated_cost,
    unknown_cost, MESSAGE_USAGE_PRICING_SUFFIX,
};
pub use file::{PricingFileError, PricingFileHeader, PRICING_FORMAT};
pub use normalize::{
    model_with_refreshed_normalization, normalize_model_name, normalize_qualified_model_name,
};
pub use table::{
    active_pricing, embedded_pricing, embedded_pricing_text, install_pricing,
    refresh_disabled_by_env, refresh_value_disables, PricingTable, PRICING_REFRESH_ENV,
};

#[cfg(test)]
mod tests;
