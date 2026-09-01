//! Core schemas and ID helpers for `statsai`.

mod account_plan;
mod archive;
mod code_changes;
mod ids;
mod paths;
mod quota;
mod report;
mod tasks;
mod types;

pub use account_plan::*;
pub use archive::*;
pub use code_changes::*;
pub use ids::*;
pub use paths::*;
pub use quota::*;
pub use report::*;
pub use tasks::*;
pub use types::*;

pub const USAGE_EVENT_SCHEMA_VERSION: &str = "usage_event.v1";
pub const USAGE_SUMMARY_SCHEMA_VERSION: &str = "usage_summary.v1";
pub const REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION: &str = "reported_usage_summary_input.v1";
pub const SOURCE_LOCATION_SCHEMA_VERSION: &str = "source_location.v1";
pub const PROVIDER_ACCOUNT_SCHEMA_VERSION: &str = "provider_account.v1";
pub const SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION: &str = "source_account_assignment.v1";
pub const SUBSCRIPTION_SCHEMA_VERSION: &str = "subscription.v1";
pub const DAILY_ROLLUP_SCHEMA_VERSION: &str = "daily_rollup.v1";
pub const SYNC_BATCH_V1_SCHEMA_VERSION: &str = "sync_batch.v1";
pub const SYNC_BATCH_V2_SCHEMA_VERSION: &str = "sync_batch.v2";
pub const SYNC_BATCH_V3_SCHEMA_VERSION: &str = "sync_batch.v3";
pub const SYNC_BATCH_V4_SCHEMA_VERSION: &str = "sync_batch.v4";
pub const SYNC_BATCH_V5_SCHEMA_VERSION: &str = "sync_batch.v5";
pub const SYNC_ACK_V1_SCHEMA_VERSION: &str = "sync_ack.v1";
pub const SYNC_ACK_V2_SCHEMA_VERSION: &str = "sync_ack.v2";
pub const SYNC_ACK_V3_SCHEMA_VERSION: &str = "sync_ack.v3";
pub const SYNC_ACK_V4_SCHEMA_VERSION: &str = "sync_ack.v4";
pub const SYNC_ACK_V5_SCHEMA_VERSION: &str = "sync_ack.v5";
pub const SYNC_BATCH_SCHEMA_VERSION: &str = SYNC_BATCH_V5_SCHEMA_VERSION;
pub const SYNC_ACK_SCHEMA_VERSION: &str = SYNC_ACK_V5_SCHEMA_VERSION;

#[cfg(test)]
mod tests;
