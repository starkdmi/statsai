use crate::{
    ScanCandidateFile, CLAUDE_CODE_PROVIDER, CODEX_PROVIDER, GROK_BUILD_PROVIDER, OPENCODE_PROVIDER,
};
use statsai_core::{canonical_display, hash_text, SourceLocation};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(crate) const SCAN_CACHE_SIGNATURE_VERSION: &str = "scan-cache.v1";
// Invalidate unchanged-file scan cache entries whenever provider parsing semantics change,
// so historical sessions get rescanned for runtime, pricing, and project context updates.
// session-identity.v28: usage events adopt the session_meta id (the telemetry
// `conversation.id`) as their session identity; cached files must reparse or
// conversation-to-account bindings can never reach previously scanned events.
pub(crate) const CODEX_SCAN_CACHE_PARSER_REVISION: &str = "session-identity.v28";
// streaming-usage-snapshot.v24: repeated Claude records for one provider request
// keep the final cumulative usage snapshot instead of the first partial one, so
// unchanged historical JSONL files must be reparsed to correct undercounted
// output tokens and estimated cost.
pub(crate) const CLAUDE_SCAN_CACHE_PARSER_REVISION: &str = "streaming-usage-snapshot.v24";
pub(crate) const OPENCODE_SCAN_CACHE_PARSER_REVISION: &str = "task-spans.v15";
pub(crate) const GROK_BUILD_SCAN_CACHE_PARSER_REVISION: &str = "task-spans.v20";

pub(crate) fn scan_candidate(
    path: PathBuf,
    dependency_signature: Option<&str>,
    cache_namespaces: &ScanCacheNamespaces,
) -> ScanCandidateFile {
    scan_candidate_with_compatible_dependencies(path, dependency_signature, &[], cache_namespaces)
}

pub(crate) fn scan_candidate_with_compatible_dependencies(
    path: PathBuf,
    dependency_signature: Option<&str>,
    compatible_dependency_signatures: &[String],
    cache_namespaces: &ScanCacheNamespaces,
) -> ScanCandidateFile {
    let cache_key = canonical_display(&path);
    let file_signature = file_metadata_signature(&path);
    let cache_signature = build_scan_cache_signature(
        &cache_namespaces.current,
        &file_signature,
        dependency_signature,
    );
    let mut compatible_cache_signatures = Vec::new();
    for dependency in compatible_dependency_signatures {
        push_compatible_cache_signature(
            &mut compatible_cache_signatures,
            &cache_signature,
            build_scan_cache_signature(
                &cache_namespaces.current,
                &file_signature,
                Some(dependency.as_str()),
            ),
        );
    }
    for namespace in &cache_namespaces.compatible {
        push_compatible_cache_signature(
            &mut compatible_cache_signatures,
            &cache_signature,
            build_scan_cache_signature(namespace, &file_signature, dependency_signature),
        );
        for dependency in compatible_dependency_signatures {
            push_compatible_cache_signature(
                &mut compatible_cache_signatures,
                &cache_signature,
                build_scan_cache_signature(namespace, &file_signature, Some(dependency.as_str())),
            );
        }
    }
    ScanCandidateFile {
        path,
        cache_key,
        cache_signature,
        compatible_cache_signatures,
    }
}

pub(crate) fn push_compatible_cache_signature(
    compatible: &mut Vec<String>,
    current: &str,
    candidate: String,
) {
    if candidate != current && !compatible.contains(&candidate) {
        compatible.push(candidate);
    }
}

pub(crate) fn build_scan_cache_signature(
    cache_namespace: &str,
    file_signature: &str,
    dependency_signature: Option<&str>,
) -> String {
    dependency_signature
        .map(|dependency| hash_text(&format!("{cache_namespace}:{file_signature}:{dependency}")))
        .unwrap_or_else(|| hash_text(&format!("{cache_namespace}:{file_signature}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanCacheNamespaces {
    pub(crate) current: String,
    pub(crate) compatible: Vec<String>,
}

pub(crate) fn scan_cache_namespaces(
    source: &SourceLocation,
    adapter_version: &str,
) -> ScanCacheNamespaces {
    let adapter_id = source.adapter_id.as_deref().unwrap_or("");
    let path_hash = source.path_hash.as_deref().unwrap_or("");
    let parser_revision = scan_cache_parser_revision(source);
    let current = hash_text(&format!(
        "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{path_hash}:{parser_revision}",
        source.provider, source.source_kind,
    ));
    let versioned = hash_text(&format!(
        "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{adapter_version}:{path_hash}:{parser_revision}",
        source.provider, source.source_kind,
    ));
    ScanCacheNamespaces {
        current,
        compatible: vec![versioned],
    }
}

pub(crate) fn scan_cache_parser_revision(source: &SourceLocation) -> &'static str {
    match source.provider.as_str() {
        CODEX_PROVIDER => CODEX_SCAN_CACHE_PARSER_REVISION,
        CLAUDE_CODE_PROVIDER => CLAUDE_SCAN_CACHE_PARSER_REVISION,
        OPENCODE_PROVIDER => OPENCODE_SCAN_CACHE_PARSER_REVISION,
        GROK_BUILD_PROVIDER => GROK_BUILD_SCAN_CACHE_PARSER_REVISION,
        _ => "default",
    }
}

pub(crate) fn file_metadata_signature(path: &Path) -> String {
    let Ok(metadata) = std::fs::metadata(path) else {
        return "missing".to_string();
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
    let (seconds, nanos) = modified
        .map(|value| (value.as_secs(), value.subsec_nanos()))
        .unwrap_or((0, 0));
    let created = metadata
        .created()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
    let (created_seconds, created_nanos) = created
        .map(|value| (value.as_secs(), value.subsec_nanos()))
        .unwrap_or((0, 0));
    hash_text(&format!(
        "meta.v2:{}:{}:{}:{}:{}",
        metadata.len(),
        seconds,
        nanos,
        created_seconds,
        created_nanos
    ))
}
