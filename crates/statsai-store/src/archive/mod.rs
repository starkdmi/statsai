mod content;
mod import;
mod read;
mod write;

pub(crate) use content::*;

use super::{ScanFileStateEntry, Store};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use statsai_core::{
    archive_artifact_metadata_signature, hash_text, ArchiveArtifactDependency, ArchiveCompleteness,
    ArchiveContentKind, ArchiveContentPart, ArchiveConversation, ArchiveItem, ArchiveItemKind,
    ArchiveRole, CoverageStatus, ModelInfo, ProjectInfo, QuotaObservationRecordV1, SourceId,
    TraceEdit, UsageCounts, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// Bumped whenever reconstruction changes what a re-import would produce, so
// already-imported files are read again instead of keeping stale results. v4
// backfilled local code-change fingerprints from original provider records; v6
// reclassified read-only shell calls and stopped treating echoed file content
// as a failed mutation; v7 reads whole-file creation from the tool result, so
// files a `Write` created become counted additions with matchable fingerprints
// instead of unclassified lines; v8 binds a trace edit to the conversation its
// file is written as, so edits a resumed session recorded against its parent
// stop being attributed to that parent; v9 backfills quota observations from
// immutable Codex archives.
pub(crate) const ARCHIVE_IMPORT_REVISION: &str = "archive.v9";
pub(crate) const UNSCOPED_MISSING_CONTENT_SCOPE: &str = "unscoped";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveWriteResult {
    pub conversations: u64,
    pub items: u64,
    pub content_parts: u64,
    pub binary_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveConversationSummary {
    pub conversation_id: String,
    pub provider: String,
    pub source_id: String,
    pub native_conversation_id: String,
    pub title: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub completeness: String,
    pub missing_content_count: u64,
    pub item_count: u64,
    pub content_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveSearchHit {
    pub conversation_id: String,
    pub item_id: String,
    pub provider: String,
    pub title: Option<String>,
    pub role: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveStats {
    pub conversations: u64,
    pub items: u64,
    pub text_parts: u64,
    pub binary_parts: u64,
    pub text_bytes: u64,
    pub binary_bytes: u64,
    pub missing_content: u64,
}

#[cfg(test)]
mod tests;
