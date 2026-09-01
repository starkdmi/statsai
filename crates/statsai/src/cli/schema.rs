use anyhow::Result;
use statsai_core::{QuotaWindowSyncProjectionV1, SyncBatch};

use super::args::{SchemaCommand, SchemaSubcommand};

pub(crate) fn schema(command: SchemaCommand) -> Result<()> {
    match command.command {
        SchemaSubcommand::SyncBatch => {
            let schema = schemars::schema_for!(SyncBatch);
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        SchemaSubcommand::QuotaWindowProjection => {
            let schema = schemars::schema_for!(QuotaWindowSyncProjectionV1);
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
    }
    Ok(())
}
