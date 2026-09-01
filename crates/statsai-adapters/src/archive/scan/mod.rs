use super::super::{
    canonical_display, codex_project_context_from_value, codex_quota_observation,
    codex_usage_counts_from_value, codex_usage_roots, collect_jsonl_files, expand_home_path,
    file_modified_timestamp, grok_sessions_root, model_from_nested_value, open_sqlite_readonly,
    read_bounded_jsonl_line, resolve_project_context, source_root_path, subtract_usage_counts,
    timestamp_from_nested_value, BoundedLineRead, ProjectContextCache, CLAUDE_CODE_PROVIDER,
    CODEX_PROVIDER, GROK_BUILD_PROVIDER, MAX_JSONL_RECORD_BYTES, OPENCODE_PROVIDER,
};
use super::mutations::{
    finish_original_mutation, mark_unresolved_mutations, record_unmeasurable_mutation,
    remember_original_mutation, MutationCompletion, MutationInvocation,
};
use super::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use serde_json::Value;
use statsai_core::{
    ArchiveItemKind, ArchiveRole, CoverageStatus, ModelInfo, ProjectInfo, QuotaObservationRecordV1,
    SourceLocation, UsageCounts,
};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

mod claude;
mod codex;
mod grok;
mod opencode;

pub(crate) use claude::*;
pub(crate) use codex::*;
pub(crate) use grok::*;
pub(crate) use opencode::*;
