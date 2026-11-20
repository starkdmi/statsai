//! Which pricing version a selected build would price production with.
//!
//! Pricing is a data file with a `format` (its structure) and a `version` (its
//! prices). A build embeds one file and reads every format up to its own; a store
//! records the version its estimates were priced with and may hold a newer file
//! adopted by a refresh. A newer version never makes a build refuse a store; only
//! a format the build cannot read does.

use anyhow::{bail, Result};
use statsai_store::{DatabasePricingState, PricingRefreshState};

/// True when `STATSAI_PRICING_REFRESH` turns the pricing refresh off. Forwarded
/// builds inherit the variable from `statsai-dev`'s own environment, and both
/// read it with this same function.
pub(crate) use statsai_store::refresh_disabled_by_env;

/// Whether a build that reads pricing files would use `store`'s stored copy.
pub(crate) fn refresh_on(store: &DatabasePricingState, refresh_disabled_by_env: bool) -> bool {
    PricingRefreshState::resolve(refresh_disabled_by_env, store.refresh_setting_on).is_on()
}

/// The pricing version the selected build would price `store` with.
///
/// `build_format` is `None` for a legacy build, one that predates pricing files:
/// it prices with its embedded version only and ignores any stored copy. A build
/// that reads pricing files uses the newer of its embedded version and the
/// store's readable stored copy while the refresh is on, and its embedded version
/// alone while the refresh is off.
///
/// # Errors
///
/// Refuses when the store holds a pricing file in a format the build cannot read.
pub(crate) fn build_pricing_target(
    build_version: u64,
    build_format: Option<u32>,
    store: &DatabasePricingState,
    refresh_disabled_by_env: bool,
) -> Result<u64> {
    let Some(build_format) = build_format else {
        return Ok(build_version);
    };
    let Some(stored) = store.stored_file else {
        return Ok(build_version);
    };
    if stored.format > u64::from(build_format) {
        bail!(
            "production's prices come from pricing file format {}, which the selected build cannot read (it reads format {build_format}); select a newer build",
            stored.format
        );
    }
    if refresh_on(store, refresh_disabled_by_env) {
        Ok(build_version.max(stored.version))
    } else {
        Ok(build_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use statsai_store::StoredPricingFileHeader;

    fn store(applied: Option<u64>, stored: Option<(u64, u64)>) -> DatabasePricingState {
        DatabasePricingState {
            applied_version: applied,
            stored_file: stored
                .map(|(format, version)| StoredPricingFileHeader { format, version }),
            stored_file_unreadable: false,
            refresh_setting_on: true,
        }
    }

    #[test]
    fn the_refresh_variable_accepts_the_usual_spellings_of_off() {
        // `refresh_disabled_by_env` reads the variable with this function, the
        // one every StatsAI binary uses.
        for value in ["0", "off", "OFF", " false ", "No", "no\n"] {
            assert!(statsai_store::refresh_value_disables(value), "{value:?}");
        }
        for value in ["", "1", "on", "true", "yes", "disabled", "of f"] {
            assert!(!statsai_store::refresh_value_disables(value), "{value:?}");
        }
    }

    #[test]
    fn a_legacy_build_prices_with_its_embedded_version_only() {
        assert_eq!(
            build_pricing_target(7, None, &store(Some(9), Some((1, 9))), false)
                .expect("legacy target"),
            7
        );
        // It cannot read a stored file at all, so an unknown format means nothing to it.
        assert_eq!(
            build_pricing_target(7, None, &store(Some(7), Some((99, 12))), false)
                .expect("legacy target"),
            7
        );
    }

    #[test]
    fn a_build_uses_the_newer_of_its_embedded_and_the_stored_version() {
        assert_eq!(
            build_pricing_target(7, Some(1), &store(Some(7), Some((1, 9))), false)
                .expect("stored copy is newer"),
            9
        );
        assert_eq!(
            build_pricing_target(10, Some(1), &store(Some(9), Some((1, 9))), false)
                .expect("embedded copy is newer"),
            10
        );
        assert_eq!(
            build_pricing_target(7, Some(1), &store(Some(7), None), false).expect("nothing stored"),
            7
        );
    }

    #[test]
    fn with_the_refresh_off_a_build_ignores_the_stored_copy() {
        let mut setting_off = store(Some(9), Some((1, 9)));
        setting_off.refresh_setting_on = false;
        assert_eq!(
            build_pricing_target(7, Some(1), &setting_off, false).expect("setting off"),
            7
        );
        assert_eq!(
            build_pricing_target(7, Some(1), &store(Some(9), Some((1, 9))), true)
                .expect("variable off"),
            7
        );
    }

    #[test]
    fn an_unreadable_stored_copy_is_ignored() {
        let mut unreadable = store(Some(7), None);
        unreadable.stored_file_unreadable = true;
        assert_eq!(
            build_pricing_target(7, Some(1), &unreadable, false).expect("ignored"),
            7
        );
    }

    #[test]
    fn a_stored_format_the_build_cannot_read_refuses() {
        let error = build_pricing_target(7, Some(1), &store(Some(9), Some((2, 9))), false)
            .expect_err("unknown format must refuse");
        let message = error.to_string();
        assert!(message.contains("pricing file format 2"), "{message}");
        assert!(message.contains("it reads format 1"), "{message}");
        assert!(message.contains("select a newer build"), "{message}");

        // The format decides, not the refresh: a build that cannot read the store's
        // prices refuses whether or not it would have used them.
        assert!(build_pricing_target(7, Some(1), &store(Some(9), Some((2, 9))), true).is_err());
        assert_eq!(
            build_pricing_target(7, Some(2), &store(Some(9), Some((2, 9))), false)
                .expect("a build that reads format 2"),
            9
        );
    }
}
