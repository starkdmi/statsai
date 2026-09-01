use anyhow::Result;
use statsai_store::{CURRENT_SCHEMA_VERSION, PRICING_RULESET_VERSION};
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
    }
}
