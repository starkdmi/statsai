use super::*;

pub(crate) fn opencode_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(root) = source_root_path(source) else {
        return Ok(Vec::new());
    };
    let db_path = root.join("opencode.db");
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);
    Ok(vec![scan_candidate(
        db_path,
        opencode_sqlite_dependency_signature(&root.join("opencode.db")).as_deref(),
        &cache_namespaces,
    )])
}

pub(crate) fn opencode_sqlite_dependency_signature(db_path: &Path) -> Option<String> {
    let db_path = db_path.to_string_lossy();
    // The shared-memory sidecar reflects SQLite coordination state, not durable content.
    let signatures = ["-wal", "-journal"]
        .into_iter()
        .map(|suffix| file_metadata_signature(Path::new(&format!("{db_path}{suffix}"))))
        .collect::<Vec<_>>();
    Some(hash_text(&signatures.join(":")))
}

pub(crate) fn opencode_root_is_source(path: &Path) -> bool {
    path.join("opencode.db").is_file()
}
