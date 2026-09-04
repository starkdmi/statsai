use anyhow::{Context, Result};
use statsai_store::{
    database_applied_pricing_ruleset_version, database_schema_version, CURRENT_SCHEMA_VERSION,
    PRICING_RULESET_VERSION,
};
use std::fmt::Display;
use std::path::Path;

use super::args::{StoreAdminCommand, StoreAdminSubcommand};

pub(crate) fn store_admin(command: StoreAdminCommand, source: &Path) -> Result<()> {
    match command.command {
        StoreAdminSubcommand::CloneTo { destination } => {
            let cloned = statsai_store::clone_database_to(source, &destination)?;
            println!(
                "Cloned {} to {} (schema {}, {} logical bytes)",
                source.display(),
                destination.display(),
                cloned.schema_version,
                cloned.logical_size
            );
            Ok(())
        }
        StoreAdminSubcommand::SupportedSchemaVersion => {
            println!("{CURRENT_SCHEMA_VERSION}");
            Ok(())
        }
        StoreAdminSubcommand::SupportedPricingRulesetVersion => {
            println!("{PRICING_RULESET_VERSION}");
            Ok(())
        }
        StoreAdminSubcommand::Migrate => migrate(source),
    }
}

/// Brings a store up to this binary's schema and pricing ruleset, and reports
/// what changed.
///
/// Opening a store is what migrates it, and price-derived commands additionally
/// apply the compiled ruleset on the way to doing something else. This command
/// exists so that upgrade can be asked for on its own -- a launcher that must
/// decide whether an old database is safe to open needs a way to say "only
/// migrate", not "scan, and migrate as a side effect".
fn migrate(source: &Path) -> Result<()> {
    let schema_before = database_schema_version(source)?;
    let pricing_before = database_applied_pricing_ruleset_version(source)?;
    drop(
        statsai::open_operational_store(source)
            .with_context(|| format!("migrate store {}", source.display()))?,
    );
    let schema_after = database_schema_version(source)?;
    let pricing_after = database_applied_pricing_ruleset_version(source)?;
    println!(
        "Migrated {} (schema {} -> {}, pricing ruleset {} -> {})",
        source.display(),
        version_label(schema_before),
        version_label(schema_after),
        version_label(pricing_before),
        version_label(pricing_after)
    );
    Ok(())
}

fn version_label<T: Display>(version: Option<T>) -> String {
    version
        .map(|version| version.to_string())
        .unwrap_or_else(|| "none".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_stamps_the_compiled_schema_and_pricing_ruleset() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("statsai.sqlite");

        migrate(&path).expect("migrate a store this binary supports");

        assert_eq!(
            database_schema_version(&path).expect("read schema"),
            Some(CURRENT_SCHEMA_VERSION)
        );
        assert_eq!(
            database_applied_pricing_ruleset_version(&path).expect("read pricing ruleset"),
            Some(PRICING_RULESET_VERSION)
        );
    }

    #[test]
    fn absent_versions_are_reported_rather_than_hidden() {
        assert_eq!(version_label(None::<i64>), "none");
        assert_eq!(version_label(Some(23)), "23");
    }
}
