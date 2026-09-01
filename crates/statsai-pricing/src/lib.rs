//! Model pricing helpers for `statsai`.
//!
//! Provides static model pricing lookup and cost estimation
//! decoupled from any specific adapter.
//!
//! [`PRICING_RULESET_VERSION`] is the monotonic numeric identity of the compiled
//! pricing rules. Increment it on every semantic pricing-rule change: new
//! models, rate changes, date-boundary mappings, multiplier logic, or anything
//! else that can change an estimated cost. Do not order
//! [`PRICING_CATALOG_VERSION`] strings lexicographically.

/// Monotonic numeric identity of the compiled pricing ruleset.
///
/// Increment this constant on every semantic pricing-rule change so persisted
/// stores can reprice automatically. The descriptive catalog string is not an
/// ordering key.
pub const PRICING_RULESET_VERSION: u64 = 1;

/// Descriptive identifier for the compiled price list. Not an ordering key.
pub const PRICING_CATALOG_VERSION: &str = "official:2026-08-19";

mod catalog;
mod cost;
mod normalize;

pub use catalog::{pricing_changes_between, pricing_for_model, ModelPricing};
pub use cost::{estimate_cost, estimate_cost_at, overlay_estimated_cost, unknown_cost};
pub use normalize::normalize_model_name;

#[cfg(test)]
mod tests;
