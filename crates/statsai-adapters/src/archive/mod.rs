mod item;
mod mutations;
mod scan;

pub(crate) use item::*;
pub(crate) use scan::*;

use super::{
    canonical_display, claude_usage_counts_from_value, opencode_message_model_info,
    opencode_message_usage_counts, CLAUDE_CODE_PROVIDER, CODEX_PROVIDER, GROK_BUILD_PROVIDER,
    OPENCODE_PROVIDER,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use statsai_core::{
    archive_conversation_id, hash_text, ArchiveArtifactDependency, ArchiveCompleteness,
    ArchiveConversation, ArchiveItem, CoverageStatus, ProjectInfo, QuotaObservationRecordV1,
    SourceLocation, TraceEdit, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) const MAX_TOOL_CALL_TEXT_BYTES: usize = 32 * 1024;
pub(crate) const MAX_TOOL_RESULT_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const TOOL_RESULT_TAIL_BYTES: usize = 16 * 1024;
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveScanDiagnostics {
    pub files_scanned: u64,
    pub records_scanned: u64,
    pub conversations: u64,
    pub items: u64,
    pub content_parts: u64,
    pub binary_bytes: u64,
    pub missing_content: u64,
    pub invalid_records: u64,
    pub mutation_calls: u64,
    pub applied_mutations: u64,
    pub failed_mutations: u64,
    pub unsupported_mutations: u64,
    pub truncated_mutations: u64,
}

#[derive(Debug, Clone)]
pub struct ArchiveScan {
    pub conversations: Vec<ArchiveConversation>,
    pub artifact_dependencies: Vec<ArchiveArtifactDependency>,
    pub trace_edits: Vec<TraceEdit>,
    pub quota_observations: Vec<QuotaObservationRecordV1>,
    pub trace_coverage: CoverageStatus,
    pub diagnostics: ArchiveScanDiagnostics,
}

impl Default for ArchiveScan {
    fn default() -> Self {
        Self {
            conversations: Vec::new(),
            artifact_dependencies: Vec::new(),
            trace_edits: Vec::new(),
            quota_observations: Vec::new(),
            // A scan that reconstructed nothing has measured nothing. Only a
            // provider whose mutations this module actually parses may claim
            // complete trace coverage, so that a new adapter cannot silently
            // over-report by inheriting the default.
            trace_coverage: CoverageStatus::Unavailable,
            diagnostics: ArchiveScanDiagnostics::default(),
        }
    }
}

impl ArchiveScan {
    /// Empty scan for a provider whose tool mutations this module parses.
    ///
    /// Coverage starts complete and degrades as unmeasurable mutations are
    /// observed, so an archive with no edits at all reads as "nothing to
    /// measure" rather than "not measured".
    pub(crate) fn measured() -> Self {
        Self {
            trace_coverage: CoverageStatus::Complete,
            ..Self::default()
        }
    }
}

pub(crate) type ArtifactDependencyMap = BTreeMap<(String, PathBuf), String>;

#[derive(Debug)]
pub(crate) struct ConversationBuilder {
    pub(crate) provider: String,
    pub(crate) source_id: statsai_core::SourceId,
    pub(crate) native_id: String,
    pub(crate) title: Option<String>,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: Option<DateTime<Utc>>,
    pub(crate) missing_content: u64,
    pub(crate) missing_content_scope_id: String,
    pub(crate) discarded_source_record_ids: Vec<String>,
    pub(crate) superseded_conversation_ids: Vec<String>,
    pub(crate) items: Vec<ArchiveItem>,
}

impl ConversationBuilder {
    pub(crate) fn new(
        provider: &str,
        source: &SourceLocation,
        native_id: String,
        scope_path: &Path,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            native_id,
            title: None,
            project: None,
            started_at: None,
            updated_at: None,
            missing_content: 0,
            missing_content_scope_id: hash_text(&canonical_display(scope_path)),
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, item: ArchiveItem) {
        if let Some(created_at) = item.created_at {
            self.started_at = Some(
                self.started_at
                    .map_or(created_at, |current| current.min(created_at)),
            );
            self.updated_at = Some(
                self.updated_at
                    .map_or(created_at, |current| current.max(created_at)),
            );
        }
        self.items.push(item);
    }

    pub(crate) fn finish(mut self) -> ArchiveConversation {
        self.items
            .sort_by_key(|item| (item.ordinal, item.item_id.clone()));
        ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: archive_conversation_id(&self.provider, &self.native_id),
            provider: self.provider,
            source_id: self.source_id,
            native_conversation_id: self.native_id,
            title: self.title,
            project: self.project,
            started_at: self.started_at,
            updated_at: self.updated_at,
            completeness: if self.missing_content > 0 {
                ArchiveCompleteness::Partial
            } else if self.items.is_empty() {
                ArchiveCompleteness::MetadataOnly
            } else {
                ArchiveCompleteness::Complete
            },
            missing_content_count: self.missing_content,
            missing_content_scope_id: Some(self.missing_content_scope_id),
            discarded_source_record_ids: self.discarded_source_record_ids,
            superseded_conversation_ids: self.superseded_conversation_ids,
            items: self.items,
        }
    }
}

pub(super) fn collect_provider_archive(
    provider: &str,
    source: &SourceLocation,
    selected_cache_keys: Option<&HashSet<String>>,
) -> Result<ArchiveScan> {
    match provider {
        CODEX_PROVIDER => collect_codex(source, selected_cache_keys),
        CLAUDE_CODE_PROVIDER => collect_claude(source, selected_cache_keys),
        OPENCODE_PROVIDER => collect_opencode(source, selected_cache_keys),
        GROK_BUILD_PROVIDER => collect_grok(source, selected_cache_keys),
        _ => Ok(ArchiveScan::default()),
    }
}

pub(crate) fn selected_jsonl_paths<F>(
    selected_cache_keys: Option<&HashSet<String>>,
    all_paths: F,
) -> Vec<PathBuf>
where
    F: FnOnce() -> Vec<PathBuf>,
{
    match selected_cache_keys {
        Some(selected) => selected
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect(),
        None => all_paths(),
    }
}

pub(crate) fn finish_diagnostics(scan: &mut ArchiveScan) {
    if scan.trace_coverage != CoverageStatus::Unavailable {
        let diagnostic_coverage = if scan.diagnostics.failed_mutations > 0
            || scan.diagnostics.unsupported_mutations > 0
            || scan.diagnostics.truncated_mutations > 0
            || scan.diagnostics.invalid_records > 0
        {
            CoverageStatus::Partial
        } else {
            CoverageStatus::Complete
        };
        scan.trace_coverage = scan.trace_coverage.combine(diagnostic_coverage);
    }
    scan.diagnostics.conversations = scan.conversations.len() as u64;
    for conversation in &scan.conversations {
        scan.diagnostics.items += conversation.items.len() as u64;
        scan.diagnostics.missing_content += conversation.missing_content_count;
        for item in &conversation.items {
            scan.diagnostics.content_parts += item.parts.len() as u64;
            scan.diagnostics.binary_bytes += item
                .parts
                .iter()
                .filter(|part| part.data_base64.is_some())
                .map(|part| part.original_bytes)
                .sum::<u64>();
        }
    }
}

#[cfg(test)]
mod tests;
