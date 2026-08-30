//! Provider adapters for local AI usage sources.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use statsai_core::{
    account_identity_observation_id, account_plan_observation_id, branch_family, canonical_display,
    conversation_account_binding_id, expand_home_path, extract_issue_keys, hash_text, home_dir,
    normalize_email, normalize_plan_name, normalize_task_title, project_bucket_key,
    provider_account_id_from_identity, summarize_task_text, task_preview_from_prompt, task_span_id,
    task_title_from_prompt, task_title_is_generic, task_title_is_weak_signal,
    task_title_signal_score, title_topic_tokens, AccountEvidenceCheckpointV1, AccountEvidenceKind,
    AccountIdentityObservationV1, AccountPlanObservationV1, Confidence,
    ConversationAccountBindingV1, CostAccumulator, CostInfo, EventId, LatencySource,
    LocationOrigin, ModelInfo, ProjectInfo, ProviderAccountId, QuotaCreditsV1,
    QuotaObservationRecordV1, QuotaObservationV1, QuotaStatusV1, QuotaUsageLinkKind,
    QuotaWindowObservationV1, RuntimeInfo, SourceLocation, SummaryMetadata, SummaryMetrics,
    TaskSpan, UsageCounts, UsageEvent, UsageSummary, ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION,
    ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION, ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION,
    CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION, QUOTA_OBSERVATION_SCHEMA_VERSION,
    QUOTA_WINDOW_OBSERVATION_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION,
};
use statsai_pricing::{estimate_cost_at, normalize_model_name, unknown_cost};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

mod archive;
mod cache;
mod claude;
mod codex;
mod event;
mod grok;
mod json;
mod model;
mod opencode;
mod project;
mod sqlite;

pub(crate) use cache::{
    file_metadata_signature, scan_cache_namespaces, scan_candidate,
    scan_candidate_with_compatible_dependencies, ScanCacheNamespaces,
};
pub(crate) use event::{
    infer_missing_output, merge_adapter_scan, metadata_only_privacy, metadata_summary,
    metric_from_samples, metric_single_sample, push_deduped, subtract_usage_counts,
    sum_usage_counts, usage_event, usd_to_micro_usd, EventDeduplication, MetadataSummaryParts,
    ProviderEventParts,
};
pub(crate) use json::{
    file_modified_timestamp, number_at_any, read_bounded_jsonl_line, read_json_file,
    stats_cache_date_end, timestamp_from_millis, timestamp_from_nested_value,
    timestamp_from_number, timestamp_from_scalar, value_as_u64, BoundedLineRead,
    MAX_JSONL_RECORD_BYTES,
};
pub(crate) use model::{
    apply_reasoning_state, codex_reasoning_state_from_value, model_from_nested_value, model_info,
    model_info_with_reasoning, opencode_message_has_variant, opencode_message_model_info,
    opencode_model_info, reasoning_state_from_model, same_model_identity, with_reasoning_state,
    ModelReasoningState,
};
pub(crate) use project::{
    project_context_from_path_fallback, resolve_project_context, resolve_project_context_cached,
    ProjectContextCache,
};
pub(crate) use sqlite::{
    open_sqlite_readonly, sqlite_column_exists, sqlite_nonzero_u64, sqlite_table_exists,
};

pub(crate) use claude::claude_usage_counts_from_value;
pub use claude::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub(crate) use codex::{
    codex_project_context_from_value, codex_quota_observation, codex_usage_counts_from_value,
    codex_usage_roots,
};
pub(crate) use grok::grok_sessions_root;
pub use grok::GrokBuildAdapter;
pub(crate) use opencode::opencode_message_usage_counts;
pub use opencode::OpenCodeAdapter;

pub const CLAUDE_CODE_PROVIDER: &str = "claude_code";
pub const CODEX_PROVIDER: &str = "codex";
pub const OPENCODE_PROVIDER: &str = "opencode";
pub const GROK_BUILD_PROVIDER: &str = "grok_build";

pub use archive::{ArchiveScan, ArchiveScanDiagnostics};
pub use statsai_core::{
    SourceIdentityInference, VerifiedSourceObservation, VerifiedSourceState,
    VerifiedSubscriptionState,
};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub device_id: String,
    pub collect_tasks: bool,
    pub selected_cache_keys: Option<HashSet<String>>,
}

impl ScanOptions {
    pub(crate) fn should_scan(&self, cache_key: &str) -> bool {
        self.selected_cache_keys
            .as_ref()
            .is_none_or(|selected| selected.contains(cache_key))
    }

    pub(crate) fn should_collect_tasks(&self) -> bool {
        self.collect_tasks
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCandidateFile {
    pub path: PathBuf,
    pub cache_key: String,
    pub cache_signature: String,
    pub compatible_cache_signatures: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanDiagnostics {
    pub files_scanned: u64,
    pub files_skipped_unchanged: u64,
    pub raw_rows: u64,
    pub candidate_usage_rows: u64,
    pub accepted_events: u64,
    pub duplicate_events: u64,
    pub skipped_zero_events: u64,
    pub invalid_rows: u64,
    pub timestamp_fallbacks: u64,
    pub model_fallbacks: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterScan {
    pub events: Vec<UsageEvent>,
    pub summaries: Vec<UsageSummary>,
    pub task_spans: Vec<TaskSpan>,
    pub quota_observations: Vec<QuotaObservationRecordV1>,
    pub diagnostics: ScanDiagnostics,
    pub verified_source_state: Option<VerifiedSourceState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProviderAccount {
    pub provider_user_id: Option<String>,
    pub email: Option<String>,
    pub plan_name: Option<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct AccountEvidenceScan {
    pub accounts: Vec<ObservedProviderAccount>,
    pub identity_observations: Vec<AccountIdentityObservationV1>,
    pub plan_observations: Vec<AccountPlanObservationV1>,
    pub conversation_bindings: Vec<ConversationAccountBindingV1>,
    pub checkpoints: Vec<AccountEvidenceCheckpointV1>,
}

/// Rewrites collected account references and their account-dependent deterministic IDs.
///
/// Callers use this with already-known aliases before Store deduplication, and again after an
/// identity upsert when a newly discovered identity changes the canonical account reference.
pub fn remap_account_evidence_account_ids(
    evidence: &mut AccountEvidenceScan,
    canonical_ids: &HashMap<ProviderAccountId, ProviderAccountId>,
) {
    for observation in &mut evidence.identity_observations {
        if let Some(canonical_id) = observation
            .provider_account_id
            .as_ref()
            .and_then(|account_id| canonical_ids.get(account_id))
        {
            observation.provider_account_id = Some(canonical_id.clone());
        }
    }
    for observation in &mut evidence.plan_observations {
        if let Some(canonical_id) = observation
            .provider_account_id
            .as_ref()
            .and_then(|account_id| canonical_ids.get(account_id))
        {
            observation.provider_account_id = Some(canonical_id.clone());
            observation.observation_id = account_plan_observation_id(
                &observation.source_id,
                Some(canonical_id),
                &observation.raw_plan_name,
                observation.observed_at,
                observation.evidence_kind,
            );
        }
    }
    for binding in &mut evidence.conversation_bindings {
        if let Some(canonical_id) = canonical_ids.get(&binding.provider_account_id) {
            binding.provider_account_id = canonical_id.clone();
            binding.binding_id = conversation_account_binding_id(
                &binding.source_id,
                &binding.conversation_id_hash,
                binding.turn_id_hash.as_deref(),
                canonical_id,
            );
        }
    }
}

/// Drops discovered accounts whose evidence was already filtered out by the Store.
///
/// Call this after `Store::retain_unseen_account_evidence` so unrelated usage-file changes do not
/// refresh an unchanged account and mark it pending for cloud sync again.
pub fn retain_accounts_referenced_by_account_evidence(
    provider: &str,
    canonical_ids: &HashMap<ProviderAccountId, ProviderAccountId>,
    evidence: &mut AccountEvidenceScan,
) {
    let referenced_account_ids = evidence
        .identity_observations
        .iter()
        .filter_map(|observation| observation.provider_account_id.clone())
        .chain(
            evidence
                .plan_observations
                .iter()
                .filter_map(|observation| observation.provider_account_id.clone()),
        )
        .chain(
            evidence
                .conversation_bindings
                .iter()
                .map(|binding| binding.provider_account_id.clone()),
        )
        .collect::<HashSet<_>>();
    evidence.accounts.retain(|observed| {
        provider_account_id_from_identity(
            provider,
            observed.provider_user_id.as_deref(),
            observed.email.as_deref(),
        )
        .map(|account_id| {
            canonical_ids
                .get(&account_id)
                .cloned()
                .unwrap_or(account_id)
        })
        .is_some_and(|account_id| referenced_account_ids.contains(&account_id))
    });
}

pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn provider(&self) -> &'static str;
    fn discover(&self) -> Vec<SourceLocation>;
    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>>;
    fn archive_scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        self.scan_candidates(source)
    }
    /// Whether the source's archive root is present and can be enumerated.
    ///
    /// An empty candidate list means "this archive holds no files" only when
    /// the root exists. From an unmounted volume, a renamed home directory, or
    /// a `--source` pointing somewhere absent it means "unavailable", and
    /// callers must not read deletions into it.
    fn archive_root_available(&self, source: &SourceLocation) -> bool {
        source_root_path(source).is_some_and(|root| root.is_dir())
    }
    fn probe_verified_source_state(
        &self,
        _source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        Ok(VerifiedSourceObservation::Unavailable)
    }
    fn verification_dependency_paths(&self, _source: &SourceLocation) -> Vec<PathBuf> {
        Vec::new()
    }
    fn verification_dependency_paths_changed(
        &self,
        _source: &SourceLocation,
        _changed: &[PathBuf],
    ) -> bool {
        false
    }
    fn collect_account_evidence(
        &self,
        _source: &SourceLocation,
        _checkpoints: &[AccountEvidenceCheckpointV1],
    ) -> Result<AccountEvidenceScan> {
        Ok(AccountEvidenceScan::default())
    }
    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan>;

    fn collect_archive(
        &self,
        source: &SourceLocation,
        selected_cache_keys: Option<&HashSet<String>>,
    ) -> Result<ArchiveScan> {
        archive::collect_provider_archive(self.provider(), source, selected_cache_keys)
    }
}

pub fn adapter_for_provider(provider: &str) -> Option<Box<dyn ProviderAdapter>> {
    match provider {
        CLAUDE_CODE_PROVIDER | "claude" | "claude-code" => Some(Box::new(ClaudeCodeAdapter)),
        CODEX_PROVIDER => Some(Box::new(CodexAdapter)),
        OPENCODE_PROVIDER | "open-code" | "open_code" => Some(Box::new(OpenCodeAdapter)),
        GROK_BUILD_PROVIDER | "grok-build" | "grok" => Some(Box::new(GrokBuildAdapter)),
        _ => None,
    }
}

pub fn default_adapters() -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(ClaudeCodeAdapter),
        Box::new(CodexAdapter),
        Box::new(OpenCodeAdapter),
        Box::new(GrokBuildAdapter),
    ]
}

pub(crate) fn local_source_for_adapter<A: ProviderAdapter>(
    adapter: &A,
    root: &Path,
    origin: LocationOrigin,
) -> SourceLocation {
    SourceLocation::local_adapter(
        adapter.provider(),
        adapter.id(),
        adapter.version(),
        root,
        origin,
    )
}

pub(crate) fn discover_sources_from_env_or_defaults<A, F>(
    adapter: &A,
    env_keys: &[&str],
    default_suffixes: &[&str],
    is_source: F,
) -> Vec<SourceLocation>
where
    A: ProviderAdapter,
    F: Fn(&Path) -> bool,
{
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for key in env_keys {
        if let Ok(value) = std::env::var(key) {
            for root in split_paths(&value) {
                if is_source(&root) && seen.insert(canonical_display(&root)) {
                    sources.push(local_source_for_adapter(
                        adapter,
                        &root,
                        LocationOrigin::Env,
                    ));
                }
            }
            if !sources.is_empty() {
                return sources;
            }
        }
    }

    let Some(home) = home_dir() else {
        return sources;
    };
    for suffix in default_suffixes {
        let root = home.join(suffix);
        if is_source(&root) && seen.insert(canonical_display(&root)) {
            sources.push(local_source_for_adapter(
                adapter,
                &root,
                LocationOrigin::Default,
            ));
        }
    }
    sources
}

pub(crate) fn source_root_path(source: &SourceLocation) -> Option<PathBuf> {
    source.path_label.as_deref().map(PathBuf::from)
}

pub(crate) fn split_paths(value: &str) -> Vec<PathBuf> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(expand_home_path)
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionEventRollup {
    pub(crate) event_ids: Vec<EventId>,
    pub(crate) usage: UsageCounts,
    pub(crate) cost: CostAccumulator,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) project_conflict: bool,
}

impl SessionEventRollup {
    pub(crate) fn consistent_project(&self) -> Option<&ProjectInfo> {
        (!self.project_conflict)
            .then_some(self.project.as_ref())
            .flatten()
    }
}

pub(crate) fn session_event_rollups(events: &[UsageEvent]) -> HashMap<String, SessionEventRollup> {
    let mut rollups = HashMap::<String, SessionEventRollup>::new();
    for event in events {
        let Some(session_hash) = event.session.local_session_id_hash.as_ref() else {
            continue;
        };
        let rollup = rollups.entry(session_hash.clone()).or_default();
        rollup.event_ids.push(event.event_id.clone());
        rollup.usage = sum_usage_counts(&rollup.usage, &event.usage);
        rollup.cost.add_estimated(&event.cost);
        if let Some(project) = event.project.as_ref() {
            if rollup.project.as_ref().is_some_and(|existing| {
                project_bucket_key(Some(existing)) != project_bucket_key(Some(project))
            }) {
                rollup.project_conflict = true;
            } else if rollup.project.is_none() {
                rollup.project = Some(project.clone());
            }
        }
    }
    rollups
}

pub(crate) fn collect_jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    Ok(files)
}

pub(crate) struct FileParseContext<'a, A: ProviderAdapter + ?Sized> {
    pub(crate) adapter: &'a A,
    pub(crate) source: &'a SourceLocation,
    pub(crate) options: &'a ScanOptions,
    pub(crate) scan: &'a mut AdapterScan,
    pub(crate) seen: &'a mut HashSet<String>,
}

pub(crate) fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

pub(crate) fn string_at_any(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| {
            if key.starts_with('/') {
                value.pointer(key)
            } else {
                value.get(*key)
            }
        })
        .find_map(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn timestamp_at_any(value: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter()
        .filter_map(|key| {
            if key.starts_with('/') {
                value.pointer(key)
            } else {
                value.get(*key)
            }
        })
        .find_map(parse_timestamp_value)
}

pub(crate) fn parse_timestamp_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|parsed| parsed.with_timezone(&Utc)),
        Value::Number(number) => number
            .as_i64()
            .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single()),
        _ => None,
    }
}

pub(crate) fn fallback_session_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cache::{
        CLAUDE_SCAN_CACHE_PARSER_REVISION, CODEX_SCAN_CACHE_PARSER_REVISION,
        GROK_BUILD_SCAN_CACHE_PARSER_REVISION, OPENCODE_SCAN_CACHE_PARSER_REVISION,
    };
    use crate::codex::scan_codex_source;
    use statsai_core::normalize_git_remote;
    use std::io::Write;

    pub(crate) fn options() -> ScanOptions {
        ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: None,
        }
    }

    pub(crate) fn options_without_tasks() -> ScanOptions {
        ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: false,
            selected_cache_keys: None,
        }
    }

    pub(crate) fn write_git_fixture(repo_root: &Path, remote: &str, branch: &str) {
        let git_dir = repo_root.join(".git");
        std::fs::create_dir_all(&git_dir).expect("git dir");
        std::fs::write(
            git_dir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = {remote}\n"
            ),
        )
        .expect("git config");
        std::fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))
            .expect("git head");
    }

    #[test]
    fn git_remote_normalization_merges_ssh_and_https() {
        assert_eq!(
            normalize_git_remote("git@github.com:Owner/Repo.git"),
            normalize_git_remote("https://github.com/Owner/Repo.git")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com/Owner/Repo.git"),
            Some("github.com/owner/repo".to_string())
        );
    }

    #[test]
    fn exact_cost_payload_upgrade_advances_every_provider_parser_revision() {
        fn revision_number(revision: &str) -> u32 {
            revision
                .rsplit_once(".v")
                .and_then(|(_, value)| value.parse().ok())
                .expect("task-span parser revision")
        }

        assert!(revision_number(CODEX_SCAN_CACHE_PARSER_REVISION) > 25);
        assert!(revision_number(CLAUDE_SCAN_CACHE_PARSER_REVISION) > 15);
        assert!(revision_number(OPENCODE_SCAN_CACHE_PARSER_REVISION) > 14);
        assert!(revision_number(GROK_BUILD_SCAN_CACHE_PARSER_REVISION) > 19);
    }

    #[test]
    fn aggregate_pricing_boundary_upgrade_advances_claude_parser_revision() {
        let revision = CLAUDE_SCAN_CACHE_PARSER_REVISION
            .rsplit_once(".v")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .expect("Claude parser revision");

        assert!(revision > 16);
    }

    #[test]
    fn model_metadata_upgrade_advances_claude_parser_revision() {
        let revision = CLAUDE_SCAN_CACHE_PARSER_REVISION
            .rsplit_once(".v")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .expect("Claude parser revision");

        assert!(revision > 17);
    }

    #[test]
    fn fast_mode_pricing_upgrade_advances_claude_parser_revision() {
        let revision = CLAUDE_SCAN_CACHE_PARSER_REVISION
            .rsplit_once(".v")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .expect("Claude parser revision");

        assert!(revision > 18);
    }

    #[test]
    fn jsonl_project_context_upgrade_advances_claude_parser_revision() {
        let revision = CLAUDE_SCAN_CACHE_PARSER_REVISION
            .rsplit_once(".v")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .expect("Claude parser revision");

        assert!(revision > 21);
    }

    #[test]
    fn provider_request_deduplication_upgrade_advances_claude_parser_revision() {
        let revision = CLAUDE_SCAN_CACHE_PARSER_REVISION
            .rsplit_once(".v")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .expect("Claude parser revision");

        assert!(revision > 22);
    }

    #[test]
    fn duplicated_semantic_events_are_deduped_within_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        for name in ["a.jsonl", "b.jsonl"] {
            let mut file = File::create(sessions.join(name)).expect("fixture");
            writeln!(
                file,
                "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"session_id\":\"same\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
            )
            .expect("write");
        }
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.duplicate_events, 1);
    }

    #[test]
    fn new_provider_aliases_resolve_to_adapters() {
        assert_eq!(
            adapter_for_provider("opencode")
                .expect("opencode")
                .provider(),
            OPENCODE_PROVIDER
        );
        assert_eq!(
            adapter_for_provider("grok-build").expect("grok").provider(),
            GROK_BUILD_PROVIDER
        );
    }
}
