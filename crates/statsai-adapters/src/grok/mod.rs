mod parse;
mod stats;
pub(crate) use parse::*;
pub(crate) use stats::*;

use crate::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) use crate::cache::GROK_BUILD_SCAN_CACHE_PARSER_REVISION;
#[cfg(test)]
pub(crate) use crate::tests::options;
#[cfg(test)]
pub(crate) use statsai_core::{display_path, path_hash};

#[derive(Debug, Default)]
pub struct GrokBuildAdapter;

impl ProviderAdapter for GrokBuildAdapter {
    fn id(&self) -> &'static str {
        "grok-build-local-sessions"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        GROK_BUILD_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        discover_sources_from_env_or_defaults(
            self,
            &["GROK_DATA_DIRS", "GROK_HOME"],
            &[".grok"],
            grok_build_root_is_source,
        )
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        grok_build_scan_candidates(source, self.version())
    }

    fn archive_scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        grok_archive_scan_candidates(source, self.version())
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_grok_build_source(self, source, options)
    }
}

pub(crate) fn scan_grok_build_source(
    adapter: &GrokBuildAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(root) = source_root_path(source) else {
        return Ok(scan);
    };
    let sessions_root = grok_sessions_root(&root);
    if !sessions_root.is_dir() {
        return Ok(scan);
    }

    let (unified_log_index, invalid_unified_rows) =
        parse_grok_unified_log_with_invalid_rows(&root)?;
    scan.diagnostics.invalid_rows += invalid_unified_rows;
    for candidate in
        grok_build_scan_candidates_with_unified_log(source, adapter.version(), &unified_log_index)?
    {
        if !options.should_scan(&candidate.cache_key) {
            scan.diagnostics.files_skipped_unchanged += 1;
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        parse_grok_summary(
            adapter,
            source,
            options,
            &candidate.path,
            &unified_log_index.session_stats,
            &mut scan,
        )?;
    }
    Ok(scan)
}

pub(crate) fn grok_build_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(root) = source_root_path(source) else {
        return Ok(Vec::new());
    };
    let unified_log_index = parse_grok_unified_log(&root)?;
    grok_build_scan_candidates_with_unified_log(source, adapter_version, &unified_log_index)
}

pub(crate) fn grok_archive_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(root) = source_root_path(source) else {
        return Ok(Vec::new());
    };
    let sessions_root = grok_sessions_root(&root);
    if !sessions_root.is_dir() {
        return Ok(Vec::new());
    }
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);
    let mut candidates = Vec::new();
    for entry in WalkDir::new(sessions_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.file_name() != "chat_history.jsonl" {
            continue;
        }
        let summary_signature = entry
            .path()
            .parent()
            .map(|parent| file_metadata_signature(&parent.join("summary.json")));
        candidates.push(scan_candidate(
            entry.path().to_path_buf(),
            summary_signature.as_deref(),
            &cache_namespaces,
        ));
    }
    candidates.sort_by_cached_key(|candidate| candidate.path.to_string_lossy().into_owned());
    Ok(candidates)
}

pub(crate) fn grok_build_scan_candidates_with_unified_log(
    source: &SourceLocation,
    adapter_version: &str,
    unified_log_index: &GrokUnifiedLogIndex,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(root) = source_root_path(source) else {
        return Ok(Vec::new());
    };
    let sessions_root = grok_sessions_root(&root);
    if !sessions_root.is_dir() {
        return Ok(Vec::new());
    }
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);
    let mut candidates = Vec::new();
    for entry in WalkDir::new(sessions_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() == "summary.json" {
            let dependency = grok_summary_dependency_signature(
                entry.path(),
                grok_session_id_from_summary_path(entry.path())
                    .as_deref()
                    .and_then(|session_id| unified_log_index.session_signatures.get(session_id))
                    .map(String::as_str),
            );
            candidates.push(scan_candidate(
                entry.path().to_path_buf(),
                dependency.as_deref(),
                &cache_namespaces,
            ));
        }
    }
    candidates.sort_by_cached_key(|candidate| candidate.path.to_string_lossy().into_owned());
    Ok(candidates)
}

pub(crate) fn grok_summary_dependency_signature(
    summary_path: &Path,
    unified_log_signature: Option<&str>,
) -> Option<String> {
    let session_dir = summary_path.parent()?;
    let mut signatures = [
        "signals.json",
        "chat_history.jsonl",
        "updates.jsonl",
        "events.jsonl",
    ]
    .into_iter()
    .map(|name| file_metadata_signature(&session_dir.join(name)))
    .collect::<Vec<_>>();
    signatures.push(unified_log_signature.unwrap_or("missing").to_string());
    let signatures = signatures.join(":");
    Some(hash_text(&signatures))
}

pub(crate) fn grok_build_root_is_source(path: &Path) -> bool {
    grok_sessions_root(path).is_dir()
}

pub(crate) fn grok_sessions_root(root: &Path) -> PathBuf {
    if root.file_name().is_some_and(|name| name == "sessions") {
        root.to_path_buf()
    } else {
        root.join("sessions")
    }
}

pub(crate) fn grok_unified_log_path(root: &Path) -> PathBuf {
    root.join("logs/unified.jsonl")
}

#[cfg(test)]
mod tests;
