//! Reads one Antigravity conversation database.
//!
//! Field numbers come from the descriptors embedded in Antigravity 2.34's
//! `language_server` binary (`CortexStepMetadata`, `ModelUsageStats`, and the
//! cortex `RetryInfo`).

use super::models::{read_model_names, ModelNames};
use super::proto::Message;
use crate::{sqlite_table_exists, ProjectContextCache};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use statsai_core::{ProjectInfo, UsageCounts};
use std::path::{Path, PathBuf};

/// `CortexStepType.PLANNER_RESPONSE`: one model request of the agent loop.
pub(crate) const STEP_PLANNER_RESPONSE: i64 = 15;
/// `CortexStepType.CHECKPOINT`: a conversation-summary model request.
pub(crate) const STEP_CHECKPOINT: i64 = 23;

// CortexStepMetadata
const META_CREATED_AT: u32 = 1;
const META_VIEWABLE_AT: u32 = 6;
const META_FINISHED_GENERATING_AT: u32 = 7;
const META_COMPLETED_AT: u32 = 8;
const META_MODEL_USAGE: u32 = 9;
const META_RETRY_INFOS: u32 = 28;
const META_STARTED_AT: u32 = 32;
// RetryInfo
const RETRY_USAGE: u32 = 2;
// ModelUsageStats
const USAGE_MODEL: u32 = 1;
const USAGE_INPUT_TOKENS: u32 = 2;
const USAGE_OUTPUT_TOKENS: u32 = 3;
const USAGE_CACHE_WRITE_TOKENS: u32 = 4;
const USAGE_CACHE_READ_TOKENS: u32 = 5;
const USAGE_THINKING_OUTPUT_TOKENS: u32 = 9;
const USAGE_RESPONSE_OUTPUT_TOKENS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallKind {
    PlannerResponse,
    /// The `attempt` is the position in the step's `retry_infos`.
    Checkpoint {
        attempt: usize,
    },
}

/// One model request and the usage it reported.
#[derive(Debug, Clone)]
pub(crate) struct ModelCall {
    pub(crate) step_index: i64,
    pub(crate) kind: CallKind,
    pub(crate) model_id: Option<u64>,
    pub(crate) usage: UsageCounts,
    pub(crate) created_at: Option<DateTime<Utc>>,
    /// The step's other timestamps, for a step that lacks `created_at`.
    pub(crate) fallback_at: Option<DateTime<Utc>>,
    pub(crate) time_to_first_token_ms: Option<u64>,
    pub(crate) latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationTrajectory {
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) parent_conversation_id: Option<String>,
    pub(crate) agent_name: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ConversationScan {
    pub(crate) conversation_id: String,
    pub(crate) trajectory: ConversationTrajectory,
    pub(crate) names: ModelNames,
    pub(crate) calls: Vec<ModelCall>,
    /// Model-request steps read, with or without usage.
    pub(crate) request_steps: u64,
    /// Planner steps that recorded no usage.
    pub(crate) requests_without_usage: u64,
    /// Steps whose metadata is not a protobuf message.
    pub(crate) invalid_steps: u64,
}

pub(crate) fn read_conversation(
    connection: &Connection,
    db_path: &Path,
    projects: &mut ProjectContextCache,
) -> Result<ConversationScan> {
    let mut scan = ConversationScan {
        conversation_id: conversation_id(connection, db_path)?,
        trajectory: read_trajectory(connection, projects)?,
        names: read_model_names(connection)?,
        ..ConversationScan::default()
    };
    if !sqlite_table_exists(connection, "steps")? {
        return Ok(scan);
    }
    let mut statement = connection.prepare(
        "SELECT idx, step_type, metadata FROM steps WHERE step_type IN (?1, ?2) ORDER BY idx",
    )?;
    let mut rows = statement.query([STEP_PLANNER_RESPONSE, STEP_CHECKPOINT])?;
    while let Some(row) = rows.next()? {
        scan.request_steps += 1;
        let step_index: i64 = row.get(0)?;
        let step_type: i64 = row.get(1)?;
        let metadata = match row.get_ref(2)? {
            ValueRef::Blob(bytes) => bytes,
            ValueRef::Null => &[],
            _ => {
                scan.invalid_steps += 1;
                continue;
            }
        };
        let Some(metadata) = Message::parse(metadata) else {
            scan.invalid_steps += 1;
            continue;
        };
        let timing = StepTiming::from_metadata(&metadata);
        if step_type == STEP_PLANNER_RESPONSE {
            let Some(usage) = metadata.message(META_MODEL_USAGE) else {
                scan.requests_without_usage += 1;
                continue;
            };
            scan.calls.push(model_call(
                step_index,
                CallKind::PlannerResponse,
                &usage,
                &timing,
            ));
        } else {
            push_checkpoint_calls(&mut scan.calls, step_index, &metadata, &timing);
        }
    }
    Ok(scan)
}

/// A checkpoint's usage is in `retry_infos[].usage`, one entry per attempt.
/// Older files also copy it into `model_usage`; that copy is read only for a
/// checkpoint with no `retry_infos` at all, so a request is never counted twice.
fn push_checkpoint_calls(
    calls: &mut Vec<ModelCall>,
    step_index: i64,
    metadata: &Message<'_>,
    timing: &StepTiming,
) {
    let mut has_retry_infos = false;
    for (attempt, retry) in metadata.messages(META_RETRY_INFOS).enumerate() {
        has_retry_infos = true;
        if let Some(usage) = retry.message(RETRY_USAGE) {
            calls.push(model_call(
                step_index,
                CallKind::Checkpoint { attempt },
                &usage,
                timing,
            ));
        }
    }
    if !has_retry_infos {
        if let Some(usage) = metadata.message(META_MODEL_USAGE) {
            calls.push(model_call(
                step_index,
                CallKind::Checkpoint { attempt: 0 },
                &usage,
                timing,
            ));
        }
    }
}

fn model_call(
    step_index: i64,
    kind: CallKind,
    usage: &Message<'_>,
    timing: &StepTiming,
) -> ModelCall {
    let (time_to_first_token_ms, latency_ms) = match kind {
        CallKind::PlannerResponse => (timing.time_to_first_token_ms(), timing.latency_ms()),
        // A checkpoint records no output timestamps of its own.
        CallKind::Checkpoint { .. } => (None, None),
    };
    ModelCall {
        step_index,
        kind,
        model_id: usage.varint(USAGE_MODEL).filter(|model| *model > 0),
        usage: usage_counts(usage),
        created_at: timing.created_at,
        fallback_at: timing.started_at.or(timing.completed_at),
        time_to_first_token_ms,
        latency_ms,
    }
}

/// `ModelUsageStats` as usage counts.
///
/// `output_tokens` (field 3) is the sum of thinking (9) and response (10)
/// output. Cost adds output and reasoning, so output is the response alone;
/// field 3 stands in only when field 10 is missing, less its thinking share.
/// Cache writes (4) are never set in observed data: Claude on Vertex counts
/// them in the uncached input (2).
pub(crate) fn usage_counts(usage: &Message<'_>) -> UsageCounts {
    let nonzero = |field| usage.varint(field).filter(|value| *value > 0);
    let reasoning = nonzero(USAGE_THINKING_OUTPUT_TOKENS);
    let output = nonzero(USAGE_RESPONSE_OUTPUT_TOKENS).or_else(|| {
        nonzero(USAGE_OUTPUT_TOKENS)
            .map(|output| output.saturating_sub(reasoning.unwrap_or(0)))
            .filter(|output| *output > 0)
    });
    UsageCounts {
        input_tokens: nonzero(USAGE_INPUT_TOKENS),
        output_tokens: output,
        cache_creation_tokens: nonzero(USAGE_CACHE_WRITE_TOKENS),
        cache_creation_5m_tokens: None,
        cache_creation_1h_tokens: None,
        cache_read_tokens: nonzero(USAGE_CACHE_READ_TOKENS),
        reasoning_tokens: reasoning,
        total_tokens: None,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

/// The timestamps a step records.
///
/// What each difference measures is inferred: `created_at` is when the
/// request was made, `viewable_at` when its first output was visible, and
/// `finished_generating_at` when generation ended. Their differences match
/// the first-token and generation durations `gen_metadata` stores beside each
/// request. `started_at` is not the start of the request: in every observed
/// planner step it equals `finished_generating_at`.
#[derive(Debug, Clone, Default)]
struct StepTiming {
    created_at: Option<DateTime<Utc>>,
    viewable_at: Option<DateTime<Utc>>,
    finished_generating_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
}

impl StepTiming {
    fn from_metadata(metadata: &Message<'_>) -> Self {
        Self {
            created_at: metadata.timestamp(META_CREATED_AT),
            viewable_at: metadata.timestamp(META_VIEWABLE_AT),
            finished_generating_at: metadata.timestamp(META_FINISHED_GENERATING_AT),
            completed_at: metadata.timestamp(META_COMPLETED_AT),
            started_at: metadata.timestamp(META_STARTED_AT),
        }
    }

    fn time_to_first_token_ms(&self) -> Option<u64> {
        elapsed_ms(self.created_at?, self.viewable_at?)
    }

    fn latency_ms(&self) -> Option<u64> {
        elapsed_ms(self.created_at?, self.finished_generating_at?)
    }
}

fn elapsed_ms(start: DateTime<Utc>, end: DateTime<Utc>) -> Option<u64> {
    u64::try_from(end.signed_duration_since(start).num_milliseconds())
        .ok()
        .filter(|elapsed| *elapsed > 0)
}

/// The conversation id: the cascade the database's trajectory belongs to,
/// which is also the file's name.
fn conversation_id(connection: &Connection, db_path: &Path) -> Result<String> {
    if sqlite_table_exists(connection, "trajectory_meta")? {
        let mut statement = connection.prepare(
            "SELECT DISTINCT cascade_id FROM trajectory_meta \
             WHERE cascade_id IS NOT NULL AND cascade_id <> '' LIMIT 2",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if let [id] = ids.as_slice() {
            return Ok(id.trim().to_string());
        }
    }
    Ok(crate::fallback_session_id(db_path))
}

// trajectory_metadata_blob
const TRAJECTORY_WORKSPACE: u32 = 1;
const TRAJECTORY_AGENT: u32 = 4;
const TRAJECTORY_PARENT_CONVERSATION: u32 = 5;
const TRAJECTORY_WORKSPACE_URI: u32 = 7;
const TRAJECTORY_SUBAGENT: u32 = 8;
// The workspace message in field 1
const WORKSPACE_URI: u32 = 1;
const WORKSPACE_GIT_ROOT_URI: u32 = 2;
const WORKSPACE_REPOSITORY: u32 = 3;
const WORKSPACE_BRANCH: u32 = 4;

/// The project, git context, and parent conversation the trajectory records.
fn read_trajectory(
    connection: &Connection,
    projects: &mut ProjectContextCache,
) -> Result<ConversationTrajectory> {
    if !sqlite_table_exists(connection, "trajectory_metadata_blob")? {
        return Ok(ConversationTrajectory::default());
    }
    let mut statement = connection.prepare(
        "SELECT data FROM trajectory_metadata_blob ORDER BY (id = 'main') DESC, rowid LIMIT 1",
    )?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else {
        return Ok(ConversationTrajectory::default());
    };
    let ValueRef::Blob(data) = row.get_ref(0)? else {
        return Ok(ConversationTrajectory::default());
    };
    let Some(trajectory) = Message::parse(data) else {
        return Ok(ConversationTrajectory::default());
    };
    Ok(trajectory_from_message(&trajectory, projects))
}

pub(crate) fn trajectory_from_message(
    trajectory: &Message<'_>,
    projects: &mut ProjectContextCache,
) -> ConversationTrajectory {
    let workspace = trajectory.message(TRAJECTORY_WORKSPACE);
    let path = workspace
        .and_then(|workspace| workspace.string(WORKSPACE_URI))
        .or_else(|| trajectory.string(TRAJECTORY_WORKSPACE_URI))
        .or_else(|| workspace.and_then(|workspace| workspace.string(WORKSPACE_GIT_ROOT_URI)))
        .and_then(workspace_path);
    // The repository message holds the repo slug and the remote URL; only the
    // remote identifies the project, so take whichever value reads as one.
    let remote = workspace
        .and_then(|workspace| workspace.message(WORKSPACE_REPOSITORY))
        .and_then(|repository| {
            [1, 2]
                .into_iter()
                .filter_map(|field| repository.string(field))
                .find(|value| looks_like_git_remote(value))
        })
        .map(ToOwned::to_owned);
    let branch = workspace
        .and_then(|workspace| workspace.string(WORKSPACE_BRANCH))
        .map(ToOwned::to_owned);
    let project = (path.is_some() || remote.is_some())
        .then(|| crate::resolve_project_context_cached(path, remote, branch, projects))
        .flatten();
    let agent_name = trajectory
        .message(TRAJECTORY_SUBAGENT)
        .and_then(|subagent| subagent.string(1))
        .or_else(|| {
            trajectory
                .message(TRAJECTORY_AGENT)
                .and_then(|agent| agent.string(1))
        })
        .map(ToOwned::to_owned);
    ConversationTrajectory {
        project,
        parent_conversation_id: trajectory
            .string(TRAJECTORY_PARENT_CONVERSATION)
            .map(ToOwned::to_owned),
        agent_name,
    }
}

/// A local path from a workspace URI. Remote workspaces have none.
fn workspace_path(uri: &str) -> Option<PathBuf> {
    if uri.starts_with('/') {
        return Some(PathBuf::from(uri));
    }
    let url = url::Url::parse(uri).ok()?;
    (url.scheme() == "file")
        .then(|| url.to_file_path().ok())
        .flatten()
}

fn looks_like_git_remote(value: &str) -> bool {
    if value.contains("://") {
        return true;
    }
    // scp-like syntax: user@host:path
    value
        .split_once('@')
        .and_then(|(_, rest)| rest.split_once(':'))
        .is_some_and(|(host, path)| !host.is_empty() && !host.contains('/') && !path.is_empty())
}
