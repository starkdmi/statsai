mod auth;
mod parse;
mod tasks;

pub(crate) use auth::*;
pub(crate) use parse::*;
pub(crate) use tasks::*;

use crate::{
    collect_jsonl_files, file_metadata_signature, scan_cache_namespaces, scan_candidate,
    session_event_rollups, source_root_path, split_paths, AdapterScan, EventDedupIndex,
    FileParseContext, ProviderAdapter, ScanCacheNamespaces, ScanCandidateFile, ScanOptions,
    SessionEventRollup, CLAUDE_CODE_PROVIDER,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use statsai_core::{
    branch_family, canonical_display, extract_issue_keys, hash_text, home_dir,
    normalize_task_title, project_bucket_key, task_span_id, task_title_is_generic, Confidence,
    LocationOrigin, ProjectInfo, SourceLocation, TaskSpan, VerifiedSourceObservation,
    TASK_SPAN_SCHEMA_VERSION,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::cache::SCAN_CACHE_SIGNATURE_VERSION;
#[cfg(test)]
use crate::file_modified_at;
#[cfg(test)]
use crate::tests::{options, options_without_tasks, write_git_fixture};
#[cfg(test)]
use crate::ProjectContextCache;
#[cfg(test)]
use chrono::TimeZone;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use statsai_core::{ReasoningLevel, SourceIdentityInference};
#[cfg(test)]
use std::fs::File;
#[cfg(test)]
use std::io::Write;

#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

impl ProviderAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code-local-jsonl"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        CLAUDE_CODE_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
            for root in split_paths(&value)
                .into_iter()
                .map(|path| normalize_claude_config_root(&path))
            {
                if root.join("projects").is_dir() && seen.insert(canonical_display(&root)) {
                    sources.push(claude_source_for_root(self, &root, LocationOrigin::Env));
                }
            }
            return sources;
        }

        if let Some(home) = home_dir() {
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            for root in [xdg.join("claude"), home.join(".claude")] {
                if root.join("projects").is_dir() && seen.insert(canonical_display(&root)) {
                    sources.push(claude_source_for_root(self, &root, LocationOrigin::Default));
                }
            }
        }

        sources
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        claude_scan_candidates(source, self.version())
    }

    fn archive_scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        let mut candidates = claude_scan_candidates(source, self.version())?;
        for candidate in &mut candidates {
            candidate.cache_signature =
                hash_text(&format!("claude-archive.v4:{}", candidate.cache_signature));
            candidate.compatible_cache_signatures.clear();
        }
        Ok(candidates)
    }

    fn probe_verified_source_state(
        &self,
        source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        let Some(root) = source_root_path(source) else {
            return Ok(VerifiedSourceObservation::Unavailable);
        };
        let root = normalize_claude_config_root(&root);
        Ok(claude_auth_snapshot(&root, &source.location_origin))
    }

    fn verification_dependency_paths(&self, source: &SourceLocation) -> Vec<PathBuf> {
        let Some(root) = source_root_path(source) else {
            return Vec::new();
        };
        let root = normalize_claude_config_root(&root);
        claude_auth_dependency_paths(&root, &source.location_origin)
    }

    fn verification_dependency_paths_changed(
        &self,
        source: &SourceLocation,
        changed: &[PathBuf],
    ) -> bool {
        let Some(root) = source_root_path(source) else {
            return false;
        };
        let root = normalize_claude_config_root(&root);
        claude_verification_dependency_topology_changed(&root, changed)
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_claude_source(self, source, options)
    }
}

pub(crate) fn claude_source_for_root(
    adapter: &ClaudeCodeAdapter,
    root: &Path,
    origin: LocationOrigin,
) -> SourceLocation {
    let root = normalize_claude_config_root(root);
    SourceLocation::local_adapter(
        adapter.provider(),
        adapter.id(),
        adapter.version(),
        &root,
        origin,
    )
}

pub(crate) fn normalize_claude_config_root(root: &Path) -> PathBuf {
    if root.file_name().is_some_and(|name| name == "projects") {
        return root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
    }
    root.to_path_buf()
}

pub(crate) fn scan_claude_source(
    adapter: &ClaudeCodeAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(scan);
    };
    let root = normalize_claude_config_root(Path::new(path_label));
    if !root.exists() {
        return Ok(scan);
    }

    let projects = root.join("projects");
    let session_projects = load_claude_session_projects(&projects);
    let cache_namespaces = scan_cache_namespaces(source, adapter.version());
    let event_files = claude_jsonl_candidates(&projects, &cache_namespaces)?;
    let mut scanned_event_cache_keys = HashSet::new();
    let mut seen = EventDedupIndex::new();
    {
        let mut ctx = FileParseContext {
            adapter,
            source,
            options,
            scan: &mut scan,
            seen: &mut seen,
        };
        for candidate in event_files {
            if !options.should_scan(&candidate.cache_key) {
                ctx.scan.diagnostics.files_skipped_unchanged += 1;
                continue;
            }
            ctx.scan.diagnostics.files_scanned += 1;
            scanned_event_cache_keys.insert(candidate.cache_key.clone());
            parse_claude_file(&mut ctx, &projects, &session_projects, &candidate.path)?;
        }
    }

    if let Some(candidate) = claude_stats_cache_candidate(&root, &cache_namespaces) {
        if options.should_scan(&candidate.cache_key) {
            scan.diagnostics.files_scanned += 1;
            parse_claude_stats_cache(adapter, source, options, &candidate.path, &mut scan)?;
        } else {
            scan.diagnostics.files_skipped_unchanged += 1;
        }
    }
    if options.should_collect_tasks() {
        let event_rollups = session_event_rollups(&scan.events);
        for entry in load_claude_task_entries(&projects) {
            let event_rollup = event_rollups.get(&hash_text(&entry.session_id));
            if !should_emit_claude_task_entry(
                options,
                &scanned_event_cache_keys,
                &entry,
                event_rollup,
            ) {
                continue;
            }
            let project = event_rollup
                .and_then(SessionEventRollup::consistent_project)
                .cloned()
                .or_else(|| entry.project.clone());
            let title = entry
                .title
                .clone()
                .unwrap_or_else(|| "Claude session".to_string());
            let issue_keys = extract_issue_keys(&[
                title.as_str(),
                entry.summary_preview.as_deref().unwrap_or(""),
                project
                    .as_ref()
                    .and_then(|project| project.branch_label.as_deref())
                    .unwrap_or(""),
            ]);
            scan.task_spans.push(TaskSpan {
                schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                span_id: task_span_id(
                    adapter.provider(),
                    &source.source_id,
                    &format!(
                        "claude_task_span.v1:{}:{}",
                        entry.session_id,
                        entry.ended_at.to_rfc3339()
                    ),
                ),
                provider: adapter.provider().to_string(),
                source_id: source.source_id.clone(),
                span_kind: "claude_session".to_string(),
                source_record_id: Some(entry.session_id.clone()),
                source_file_path_hash: entry
                    .source_path
                    .as_deref()
                    .map(claude_task_entry_source_file_path_hash),
                summary_id: None,
                session_id: Some(entry.session_id.clone()),
                thread_id: None,
                title: title.clone(),
                normalized_title: normalize_task_title(&title),
                title_source: Some(entry.title_source.to_string()),
                summary_preview: entry.summary_preview.clone(),
                todo_excerpt: None,
                issue_keys,
                branch_family: branch_family(
                    project
                        .as_ref()
                        .and_then(|project| project.branch_label.as_deref()),
                ),
                project_bucket: project_bucket_key(project.as_ref()),
                project,
                git: None,
                usage: event_rollup
                    .map(|rollup| rollup.usage.clone())
                    .unwrap_or_default(),
                estimated_cost_usd: event_rollup.and_then(|rollup| rollup.cost.cents_rounded()),
                estimated_cost_micro_usd: event_rollup.and_then(|rollup| rollup.cost.micro_usd()),
                event_count: event_rollup
                    .map(|rollup| rollup.event_ids.len() as u64)
                    .unwrap_or(0),
                has_usage_evidence: event_rollup.is_some_and(|rollup| !rollup.event_ids.is_empty()),
                total_messages: 0,
                user_messages: 0,
                assistant_messages: 0,
                developer_messages: 0,
                linked_event_ids: event_rollup
                    .map(|rollup| rollup.event_ids.clone())
                    .unwrap_or_default(),
                confidence: if entry.title_source == "summary"
                    && !task_title_is_generic(Some(title.as_str()))
                {
                    Confidence::High
                } else if entry.summary_preview.is_some() {
                    Confidence::Medium
                } else {
                    Confidence::Low
                },
                is_meta: task_title_is_generic(Some(title.as_str())),
                started_at: entry.started_at,
                ended_at: Some(entry.ended_at),
                duration_seconds: entry
                    .ended_at
                    .signed_duration_since(entry.started_at)
                    .num_seconds()
                    .try_into()
                    .ok(),
            });
        }
    }
    scan.diagnostics.accepted_events = scan.events.len() as u64;
    Ok(scan)
}

pub(crate) fn should_emit_claude_task_entry(
    options: &ScanOptions,
    scanned_event_cache_keys: &HashSet<String>,
    entry: &ClaudeTaskEntry,
    event_rollup: Option<&SessionEventRollup>,
) -> bool {
    if options.selected_cache_keys.is_none() {
        return true;
    }

    if event_rollup.is_some() {
        return true;
    }

    entry
        .source_path
        .as_deref()
        .is_some_and(|path| claude_task_entry_matches_scanned_file(path, scanned_event_cache_keys))
}

pub(crate) fn claude_task_entry_matches_scanned_file(
    path: &Path,
    scanned_event_cache_keys: &HashSet<String>,
) -> bool {
    let canonical_path = claude_task_entry_source_cache_key(path);
    if scanned_event_cache_keys.contains(&canonical_path) {
        return true;
    }

    let canonical_path = Path::new(&canonical_path);
    match canonical_path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => scanned_event_cache_keys
            .contains(&canonical_display(&canonical_path.with_extension(""))),
        None => scanned_event_cache_keys
            .contains(&canonical_display(&canonical_path.with_extension("jsonl"))),
        Some(_) => false,
    }
}

pub(crate) fn claude_task_entry_source_file_path_hash(path: &Path) -> String {
    hash_text(&claude_task_entry_source_cache_key(path))
}

pub(crate) fn claude_task_entry_source_cache_key(path: &Path) -> String {
    canonical_display(&claude_task_entry_source_path(path))
}

pub(crate) fn claude_task_entry_source_path(path: &Path) -> PathBuf {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => {
            if path.is_file() {
                path.to_path_buf()
            } else {
                let without_extension = path.with_extension("");
                if without_extension.is_file() {
                    without_extension
                } else {
                    path.to_path_buf()
                }
            }
        }
        None => {
            let jsonl_path = path.with_extension("jsonl");
            if jsonl_path.is_file() {
                jsonl_path
            } else {
                path.to_path_buf()
            }
        }
        Some(_) => path.to_path_buf(),
    }
}

pub(crate) fn claude_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(Vec::new());
    };
    let root = normalize_claude_config_root(Path::new(path_label));
    if !root.exists() {
        return Ok(Vec::new());
    }
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);

    let mut candidates = claude_jsonl_candidates(&root.join("projects"), &cache_namespaces)?;
    if let Some(candidate) = claude_stats_cache_candidate(&root, &cache_namespaces) {
        candidates.push(candidate);
    }
    Ok(candidates)
}

pub(crate) fn claude_jsonl_candidates(
    root: &Path,
    cache_namespaces: &ScanCacheNamespaces,
) -> Result<Vec<ScanCandidateFile>> {
    collect_jsonl_files(root)?
        .into_iter()
        .map(|path| {
            let dependency = claude_session_index_dependency(root, &path);
            Ok(scan_candidate(
                path,
                dependency.as_deref(),
                cache_namespaces,
            ))
        })
        .collect()
}

pub(crate) fn claude_stats_cache_candidate(
    root: &Path,
    cache_namespaces: &ScanCacheNamespaces,
) -> Option<ScanCandidateFile> {
    let path = root.join("stats-cache.json");
    path.is_file()
        .then(|| scan_candidate(path, None, cache_namespaces))
}

pub(crate) fn claude_session_index_dependency(root: &Path, path: &Path) -> Option<String> {
    path.ancestors()
        .take_while(|ancestor| ancestor.starts_with(root))
        .skip(1)
        .find_map(|ancestor| {
            let session_index = ancestor.join("sessions-index.json");
            session_index
                .is_file()
                .then(|| file_metadata_signature(&session_index))
        })
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeSessionProjectMetadata {
    pub(crate) project_path: Option<PathBuf>,
    pub(crate) git_branch: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeTaskEntry {
    pub(crate) session_id: String,
    pub(crate) title: Option<String>,
    pub(crate) title_source: &'static str,
    pub(crate) summary_preview: Option<String>,
    pub(crate) project: Option<ProjectInfo>,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) ended_at: DateTime<Utc>,
    pub(crate) source_path: Option<PathBuf>,
}

#[cfg(test)]
mod tests;
