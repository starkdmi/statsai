use super::event::{CostInfo, ProjectInfo, UsageCounts};
use super::summary::SummaryModelUsage;
use crate::ids::{ProviderAccountId, SourceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SESSION_ROLLUP_SCHEMA_VERSION: &str = "session_rollup.v1";

/// Where a session title was resolved from. Event titles win over task spans,
/// which win over archived conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionTitleSource {
    Event,
    TaskSpan,
    Archive,
}

/// One local session, aggregated from the usage events that share its hashed id.
///
/// `session_id` is the hashed `session_…` identity already stored on each event.
/// Raw provider session ids never appear on this record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRollupV1 {
    pub schema_version: String,
    pub session_id: String,
    pub device_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub provider_account_id: Option<ProviderAccountId>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_seconds: Option<u64>,
    pub usage: UsageCounts,
    pub requests: u64,
    pub cost: CostInfo,
    pub models: Vec<SummaryModelUsage>,
    pub primary_model: Option<String>,
    pub total_messages: Option<u64>,
    pub user_messages: Option<u64>,
    pub assistant_messages: Option<u64>,
    pub developer_messages: Option<u64>,
    pub project: Option<ProjectInfo>,
    pub title: Option<String>,
    pub title_source: Option<SessionTitleSource>,
    pub updated_at: DateTime<Utc>,
}
