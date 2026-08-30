mod auth;
mod parse;
mod tasks;

pub(crate) use auth::*;
pub(crate) use parse::*;
pub(crate) use tasks::*;

use crate::{
    collect_jsonl_files, file_metadata_signature, scan_cache_namespaces, scan_candidate,
    session_event_rollups, source_root_path, split_paths, AdapterScan, FileParseContext,
    ProviderAdapter, ScanCacheNamespaces, ScanCandidateFile, ScanOptions, SessionEventRollup,
    CLAUDE_CODE_PROVIDER,
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
    let mut seen = HashSet::new();
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

#[test]
fn claude_normalizes_projects_path_to_config_root() {
    let adapter = ClaudeCodeAdapter;
    let source = claude_source_for_root(
        &adapter,
        Path::new("/tmp/claude-home/projects"),
        LocationOrigin::Configured,
    );

    assert_eq!(source.provider, CLAUDE_CODE_PROVIDER);
    assert_eq!(source.path_label.as_deref(), Some("/tmp/claude-home"));
}

#[test]
fn claude_cached_profile_infers_account_when_durable_settings_are_clear() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "file-only-account",
                "emailAddress": "file-only@example.com",
                "profileFetchedAt": 1_786_104_000_000_i64
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let settings_path = root.join("settings.json");
    std::fs::write(&settings_path, r#"{"theme":"dark"}"#).expect("harmless settings");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state,
        basis,
        settings_modified_at,
    } = observation
    else {
        panic!("clean cached Claude profile must infer the account");
    };
    assert_eq!(basis, SourceIdentityInference::CachedLocalProfile);
    assert_eq!(settings_modified_at, file_modified_at(&settings_path));
    assert_eq!(state.provider_user_id.as_deref(), Some("file-only-account"));
    assert_eq!(state.email.as_deref(), Some("file-only@example.com"));
    assert_eq!(
        state.authenticated_at,
        Utc.timestamp_millis_opt(1_786_104_000_000_i64).single()
    );
}

#[test]
fn claude_default_profile_resolution_uses_home_sibling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        dir.path().join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "active-home-account",
                "emailAddress": "home@example.com"
            }
        })
        .to_string(),
    )
    .expect("home profile");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "stale-nested-account",
                "emailAddress": "stale@example.com"
            }
        })
        .to_string(),
    )
    .expect("stale nested profile");

    let observation =
        claude_cached_profile_observation(&root, &LocationOrigin::Default, Some(&root), None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("default Claude source must use the home profile");
    };
    assert_eq!(
        state.provider_user_id.as_deref(),
        Some("active-home-account")
    );
}

#[test]
fn claude_environment_profile_resolution_uses_nested_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("custom").join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "environment-account",
                "emailAddress": "environment@example.com"
            }
        })
        .to_string(),
    )
    .expect("nested profile");
    std::fs::write(
        root.parent().expect("custom parent").join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "unrelated-sibling-account",
                "emailAddress": "unrelated@example.com"
            }
        })
        .to_string(),
    )
    .expect("sibling profile");

    let observation = claude_cached_profile_observation(&root, &LocationOrigin::Env, None, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("CLAUDE_CONFIG_DIR source must use its nested profile");
    };
    assert_eq!(
        state.provider_user_id.as_deref(),
        Some("environment-account")
    );
}

#[test]
fn claude_configured_source_with_conflicting_profiles_is_blocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("mounted").join(".claude");
    std::fs::create_dir_all(&root).expect("config root");
    for (path, account) in [
        (root.join(".claude.json"), "nested-account"),
        (
            root.parent().expect("mounted parent").join(".claude.json"),
            "sibling-account",
        ),
    ] {
        std::fs::write(
            path,
            serde_json::json!({
                "oauthAccount": {
                    "accountUuid": account,
                    "emailAddress": format!("{account}@example.com")
                }
            })
            .to_string(),
        )
        .expect("profile");
    }

    assert_eq!(
        claude_cached_profile_observation(&root, &LocationOrigin::Configured, None, None),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_auth_dependencies_include_user_and_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let project_settings_root = workspace.join(".claude");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&project_settings_root).expect("project settings root");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");

    let dependencies = claude_auth_dependency_paths(&root, &LocationOrigin::Configured);

    assert!(dependencies.contains(&root.join(".claude.json")));
    assert!(dependencies.contains(&root.join("settings.json")));
    assert!(dependencies.contains(&root.join("settings.local.json")));
    assert!(dependencies.contains(&project_settings_root));
}

#[test]
fn claude_dependency_topology_changes_only_for_project_indexes_and_directories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let projects = root.join("projects");
    let project_store = projects.join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&projects)
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store)
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("sessions-index.json"))
    ));
    assert!(claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("session.jsonl"))
    ));
    std::fs::write(
        project_store.join("sessions-index.json"),
        r#"{"entries":[]}"#,
    )
    .expect("session index");
    assert!(!claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&project_store.join("session.jsonl"))
    ));
    assert!(!claude_verification_dependency_topology_changed(
        &root,
        std::slice::from_ref(&root.join("settings.json"))
    ));
}

#[test]
fn claude_project_auth_override_and_dependencies_include_repository_ancestors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let repository = dir.path().join("workspace");
    let nested_project = repository.join("crates").join("app");
    let repository_settings_root = repository.join(".claude");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(repository.join(".git")).expect("git directory");
    std::fs::create_dir_all(&nested_project).expect("nested project");
    std::fs::create_dir_all(&repository_settings_root).expect("project settings directory");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": nested_project,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        repository_settings_root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "repository-api-key"}
        })
        .to_string(),
    )
    .expect("repository settings");

    let dependencies = claude_auth_dependency_paths(&root, &LocationOrigin::Configured);

    assert_eq!(
        claude_source_settings_have_auth_override(&root, &root),
        Some(true)
    );
    assert!(dependencies.contains(&repository_settings_root));
}

#[test]
fn claude_cached_profile_is_not_used_when_settings_select_auth_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let settings_path = root.join("settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com",
                "organizationUuid": "cached-organization"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "configured-token"}
        })
        .to_string(),
    )
    .expect("claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_custom_base_url_blocks_attribution_when_cached_profile_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let settings_path = root.join("settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://gateway.example.com/anthropic"}
        })
        .to_string(),
    )
    .expect("claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_default_base_url_without_cached_profile_is_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com/"}
        })
        .to_string(),
    )
    .expect("claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(observation, VerifiedSourceObservation::Unavailable);
}

#[test]
fn claude_cached_profile_is_not_used_when_project_settings_select_api_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let transcript = project_store.join("session.jsonl");
    let settings_path = workspace.join(".claude").join("settings.local.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(&transcript, "{}\n").expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": [{
                "sessionId": "session",
                "fullPath": transcript,
                "projectPath": workspace
            }]
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "project-api-key"}
        })
        .to_string(),
    )
    .expect("project settings");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_index_override_is_detected_without_transcript_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    let settings_path = workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("project settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_without_session_index_still_uses_cached_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    let VerifiedSourceObservation::Inferred {
        identity: state, ..
    } = observation
    else {
        panic!("missing optional session index must not disable account detection");
    };
    assert_eq!(state.provider_user_id.as_deref(), Some("cached-account"));
    assert_eq!(state.email.as_deref(), Some("cached@example.com"));
}

#[test]
fn claude_project_without_session_index_checks_recovered_project_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    let workspace = dir.path().join("workspace");
    let settings_path = workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(settings_path.parent().expect("settings parent"))
        .expect("project settings directory");
    std::fs::write(
        project_store.join("session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": workspace})),
    )
    .expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "env": {"ANTHROPIC_API_KEY": "project-api-key"}
        })
        .to_string(),
    )
    .expect("project settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&settings_path),
        }
    );
}

#[test]
fn claude_project_without_index_or_project_metadata_blocks_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("unknown-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::write(project_store.join("session.jsonl"), "{}\n").expect("transcript");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_missing_index_checks_every_nested_transcript_project_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("mixed-workspaces");
    let oauth_workspace = dir.path().join("oauth-workspace");
    let api_workspace = dir.path().join("api-workspace");
    let api_settings = api_workspace.join(".claude").join("settings.json");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(api_settings.parent().expect("settings parent"))
        .expect("api project settings");
    std::fs::write(
        project_store.join("a-oauth-session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": oauth_workspace})),
    )
    .expect("oauth transcript");
    std::fs::write(
        project_store.join("z-api-session.jsonl"),
        format!("{}\n", serde_json::json!({"cwd": api_workspace})),
    )
    .expect("api parent transcript");
    let subagent = project_store
        .join("z-api-session")
        .join("subagents")
        .join("agent-a.jsonl");
    std::fs::create_dir_all(subagent.parent().expect("subagent parent"))
        .expect("subagent directory");
    std::fs::write(&subagent, "{}\n").expect("api subagent transcript");
    std::fs::write(
        &api_settings,
        serde_json::json!({"env": {"ANTHROPIC_API_KEY": "project-api-key"}}).to_string(),
    )
    .expect("api project settings");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("cached profile");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&api_settings),
        }
    );
}

#[test]
fn claude_cached_profile_is_not_used_when_settings_select_api_key_helper() {
    let settings = serde_json::json!({"apiKeyHelper": "/usr/local/bin/credential-helper"});

    assert!(claude_settings_value_has_auth_override(&settings));
}

#[test]
fn claude_local_settings_clear_lower_precedence_auth_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("settings.json"),
        serde_json::json!({
            "apiKeyHelper": "/usr/local/bin/stale-helper",
            "env": {"ANTHROPIC_API_KEY": "stale-key"}
        })
        .to_string(),
    )
    .expect("lower-precedence settings");
    std::fs::write(
        dir.path().join("settings.local.json"),
        serde_json::json!({
            "apiKeyHelper": "",
            "env": {"ANTHROPIC_API_KEY": ""}
        })
        .to_string(),
    )
    .expect("higher-precedence settings");

    assert_eq!(claude_settings_have_auth_override(dir.path()), Some(false));
}

#[test]
fn claude_settings_precedence_preserves_first_uninterrupted_enabled_timestamp() {
    let first_enabled_at = Utc
        .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
        .single()
        .expect("first enabled timestamp");
    let repeated_enabled_at = Utc
        .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
        .single()
        .expect("repeated enabled timestamp");
    let reenabled_at = Utc
        .with_ymd_and_hms(2026, 5, 20, 0, 0, 0)
        .single()
        .expect("re-enabled timestamp");
    let enabled = serde_json::json!({
        "env": {"ANTHROPIC_API_KEY": "configured-key"}
    });
    let cleared = serde_json::json!({
        "env": {"ANTHROPIC_API_KEY": ""}
    });
    let mut state = ClaudeAuthOverrideState::default();

    state.apply(&enabled, false, Some(first_enabled_at));
    state.apply(&enabled, false, Some(repeated_enabled_at));

    assert_eq!(
        state.probe(),
        ClaudeAuthOverrideProbe::Blocked(ClaudeAuthBlock {
            blocked_since: Some(first_enabled_at),
        })
    );

    state.apply(&cleared, false, Some(repeated_enabled_at));
    assert_eq!(state.probe(), ClaudeAuthOverrideProbe::Clear);
    state.apply(&enabled, false, Some(reenabled_at));

    assert_eq!(
        state.probe(),
        ClaudeAuthOverrideProbe::Blocked(ClaudeAuthBlock {
            blocked_since: Some(reenabled_at),
        })
    );
}

#[test]
fn claude_project_local_settings_clear_user_auth_override_for_only_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let project_store = root.join("projects").join("workspace");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(workspace.join(".claude")).expect("project settings directory");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "stale-user-token"}
        })
        .to_string(),
    )
    .expect("user settings");
    std::fs::write(
        project_store.join("sessions-index.json"),
        serde_json::json!({
            "version": 1,
            "originalPath": workspace,
            "entries": []
        })
        .to_string(),
    )
    .expect("session index");
    std::fs::write(
        workspace.join(".claude").join("settings.local.json"),
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": ""}
        })
        .to_string(),
    )
    .expect("project local settings");

    assert_eq!(
        claude_source_settings_have_auth_override(&root, &root),
        Some(false)
    );
}

#[test]
fn claude_settings_detect_every_higher_precedence_credential_override() {
    for name in CLAUDE_SETTINGS_AUTH_OVERRIDE_KEYS {
        let mut environment = serde_json::Map::new();
        environment.insert(
            (*name).to_owned(),
            Value::String(
                if claude_setting_is_provider_selector(name) {
                    "1"
                } else {
                    "configured-value"
                }
                .to_owned(),
            ),
        );
        let settings = serde_json::json!({"env": environment});

        assert!(
            claude_settings_value_has_auth_override(&settings),
            "expected {name} to suppress cached OAuth identity"
        );
    }
}

#[test]
fn claude_settings_detect_current_non_oauth_provider_selectors() {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ] {
        for value in [
            Value::String("1".to_owned()),
            Value::String("true".to_owned()),
            Value::Bool(true),
            Value::Number(1.into()),
        ] {
            let mut environment = serde_json::Map::new();
            environment.insert(name.to_owned(), value);
            let settings = serde_json::json!({"env": environment});

            assert!(
                claude_settings_value_has_auth_override(&settings),
                "expected enabled {name} value to suppress cached OAuth identity"
            );
        }
    }
}

#[test]
fn claude_disabled_provider_selector_values_do_not_suppress_cached_oauth() {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_USE_MANTLE",
        "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    ] {
        for value in [
            Value::String("0".to_owned()),
            Value::String("false".to_owned()),
            Value::Bool(false),
            Value::Number(0.into()),
        ] {
            let mut environment = serde_json::Map::new();
            environment.insert(name.to_owned(), value);
            let settings = serde_json::json!({"env": environment});

            assert!(
                !claude_settings_value_has_auth_override(&settings),
                "expected disabled {name} value to preserve cached OAuth identity"
            );
        }
    }
}

#[test]
fn claude_settings_detect_descriptor_injected_credentials() {
    for name in [
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    ] {
        let mut environment = serde_json::Map::new();
        environment.insert(name.to_owned(), Value::String("3".to_owned()));
        let settings = serde_json::json!({"env": environment});

        assert!(
            claude_settings_value_has_auth_override(&settings),
            "expected {name} to suppress cached OAuth identity"
        );
    }
}

#[test]
fn claude_managed_settings_provider_override_suppresses_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    let managed_settings_root = dir.path().join("managed");
    let managed_settings_path = managed_settings_root.join("managed-settings.json");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::create_dir_all(&managed_settings_root).expect("managed settings root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(
        &managed_settings_path,
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_MANTLE": "1"}
        })
        .to_string(),
    )
    .expect("managed settings");

    let observation = claude_auth_snapshot_with_probe_context(
        &root,
        &LocationOrigin::Configured,
        Some(&managed_settings_root),
    );

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: file_modified_at(&managed_settings_path),
        }
    );
}

#[test]
fn claude_managed_settings_drop_in_provider_override_suppresses_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let drop_ins = dir.path().join("managed-settings.d");
    std::fs::create_dir_all(&drop_ins).expect("managed settings drop-ins");
    std::fs::write(
        drop_ins.join("20-provider.json"),
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_ANTHROPIC_AWS": "1"}
        })
        .to_string(),
    )
    .expect("managed settings drop-in");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(true)
    );
}

#[test]
fn claude_managed_drop_ins_clear_base_auth_overrides_in_filename_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let drop_ins = dir.path().join("managed-settings.d");
    std::fs::create_dir_all(&drop_ins).expect("managed settings drop-ins");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "apiKeyHelper": "/usr/local/bin/stale-helper",
            "policyHelper": {"path": "/usr/local/bin/stale-policy"},
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("managed settings base");
    std::fs::write(
        drop_ins.join("10-keep-enabled.json"),
        serde_json::json!({
            "env": {"CLAUDE_CODE_USE_VERTEX": "1"}
        })
        .to_string(),
    )
    .expect("earlier managed settings drop-in");
    std::fs::write(
        drop_ins.join("20-clear-auth.json"),
        serde_json::json!({
            "apiKeyHelper": "",
            "policyHelper": null,
            "env": {"CLAUDE_CODE_USE_VERTEX": ""}
        })
        .to_string(),
    )
    .expect("later managed settings drop-in");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(false)
    );
}

#[test]
fn claude_managed_policy_helper_suppresses_cached_oauth_without_execution() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "policyHelper": {"path": "/usr/local/bin/managed-claude-policy"}
        })
        .to_string(),
    )
    .expect("managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(true)
    );
}

#[test]
fn claude_unrelated_managed_settings_do_not_suppress_cached_oauth() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("managed-settings.json"),
        serde_json::json!({
            "permissions": {"deny": ["Read(//etc/secrets/**)"]}
        })
        .to_string(),
    )
    .expect("managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        Some(false)
    );
}

#[test]
fn claude_malformed_managed_settings_block_cached_profile_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(dir.path().join("managed-settings.json"), "{not valid json")
        .expect("malformed managed settings");

    assert_eq!(
        claude_managed_settings_have_auth_override_in(dir.path()),
        None
    );
    assert_eq!(
        claude_auth_snapshot_with_probe_context(
            &root,
            &LocationOrigin::Configured,
            Some(dir.path()),
        ),
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_malformed_settings_block_cached_profile_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(&root).expect("config root");
    std::fs::write(
        root.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "cached-account",
                "emailAddress": "cached@example.com"
            }
        })
        .to_string(),
    )
    .expect("claude profile");
    std::fs::write(root.join("settings.json"), "{not valid json")
        .expect("malformed claude settings");

    let observation =
        claude_auth_snapshot_with_probe_context(&root, &LocationOrigin::Configured, None);

    assert_eq!(
        observation,
        VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        }
    );
}

#[test]
fn claude_extracts_project_path_and_git_metadata_from_sessions_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("example-workspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"abc\",\"fullPath\":\"{}\",\"gitBranch\":\"main\",\"projectPath\":\"{}\"}}]}}",
            session_path.display(),
            workspace.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(project.branch_label.as_deref(), Some("main"));
}

#[test]
fn claude_subagent_transcripts_inherit_project_path_from_sessions_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("example-workspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "feature/example-subagent-fix",
    );

    let session_file = project_store.join("session-123.jsonl");
    let subagent_dir = project_store.join("session-123").join("subagents");
    std::fs::create_dir_all(&subagent_dir).expect("subagent dir");
    let subagent_file = subagent_dir.join("agent-a.jsonl");
    std::fs::write(
        &subagent_file,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("subagent session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-123\",\"fullPath\":\"{}\",\"gitBranch\":\"feature/example-subagent-fix\",\"projectPath\":\"{}\"}}]}}",
            session_file.display(),
            workspace.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(
        project.branch_label.as_deref(),
        Some("feature/example-subagent-fix")
    );
}

#[test]
fn claude_project_store_root_falls_back_to_original_path_when_session_index_misses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let projects = root.join("projects");
    let project_store = projects.join("-home-example-src-ExampleWorkspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let subagent_dir = project_store.join("unindexed-session").join("subagents");
    std::fs::create_dir_all(&subagent_dir).expect("subagent dir");
    let subagent_file = subagent_dir.join("agent-a.jsonl");
    std::fs::write(
        &subagent_file,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("subagent session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"originalPath\":\"{}\",\"entries\":[{{\"sessionId\":\"indexed-session\",\"fullPath\":\"{}\",\"gitBranch\":\"main\",\"projectPath\":\"{}\"}}]}}",
            workspace.display(),
            project_store.join("indexed-session.jsonl").display(),
            workspace.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
}

#[test]
fn claude_extracts_project_context_from_jsonl_when_session_index_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root
        .join("projects")
        .join("-home-example-src-ExampleWorkspace");
    let workspace = root.join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        format!(
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"cwd\":\"{}\",\"gitBranch\":\"feature/jsonl-project\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n",
            workspace.display()
        ),
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("ExampleWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/example-workspace")
    );
    assert_eq!(
        project.branch_label.as_deref(),
        Some("feature/jsonl-project")
    );
}

#[test]
fn claude_falls_back_to_valid_project_path_when_cwd_is_invalid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("workspace").join("ExampleWorkspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write_git_fixture(
        &workspace,
        "https://github.com/example-org/example-workspace.git",
        "main",
    );

    for invalid_cwd in [Value::Null, serde_json::json!(42), serde_json::json!("   ")] {
        let value = serde_json::json!({
            "cwd": invalid_cwd,
            "projectPath": workspace.to_string_lossy(),
            "gitBranch": "main"
        });
        let mut cache = ProjectContextCache::new();
        let project = claude_project_context_from_value(&value, None, &mut cache)
            .expect("projectPath fallback");

        assert_eq!(
            project.path_label.as_deref(),
            Some(workspace.to_string_lossy().as_ref())
        );
        assert_eq!(
            project.repo_label.as_deref(),
            Some("example-org/example-workspace")
        );
    }
}

#[test]
fn claude_jsonl_project_context_overrides_stale_session_index_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    let stale_workspace = root.join("workspace").join("OldWorkspace");
    let current_workspace = root.join("workspace").join("CurrentWorkspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    std::fs::create_dir_all(&stale_workspace).expect("stale workspace");
    std::fs::create_dir_all(&current_workspace).expect("current workspace");
    write_git_fixture(
        &stale_workspace,
        "https://github.com/example-org/old-workspace.git",
        "old-branch",
    );
    write_git_fixture(
        &current_workspace,
        "https://github.com/example-org/current-workspace.git",
        "main",
    );

    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        format!(
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"abc\",\"cwd\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n",
            current_workspace.display()
        ),
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"abc\",\"fullPath\":\"{}\",\"gitBranch\":\"old-branch\",\"projectPath\":\"{}\"}}]}}",
            session_path.display(),
            stale_workspace.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let project = scan.events[0].project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(current_workspace.to_string_lossy().as_ref())
    );
    assert_eq!(project.project_label.as_deref(), Some("CurrentWorkspace"));
    assert_eq!(
        project.repo_label.as_deref(),
        Some("example-org/current-workspace")
    );
    assert_eq!(project.branch_label.as_deref(), Some("main"));

    assert_eq!(scan.task_spans.len(), 1);
    let task = &scan.task_spans[0];
    assert_eq!(task.project.as_ref(), Some(project));
    assert_eq!(task.project_bucket, project_bucket_key(Some(project)));
    assert_eq!(task.linked_event_ids, vec![scan.events[0].event_id.clone()]);
}

#[test]
fn claude_source_scans_projects_child_when_config_root_is_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let transcripts = dir.path().join("transcripts");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::create_dir_all(&transcripts).expect("transcripts");

    let mut project_file = File::create(projects.join("session.jsonl")).expect("project file");
    writeln!(
        project_file,
        "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}"
    )
    .expect("write project");
    let mut transcript_file =
        File::create(transcripts.join("transcript.jsonl")).expect("transcript file");
    writeln!(
        transcript_file,
        "{{\"message\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}}}"
    )
    .expect("write transcript");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");
    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.raw_rows, 1);
    assert_eq!(scan.events[0].usage.computed_total(), 3);
}

#[test]
fn claude_deduplicates_repeated_usage_by_message_and_request_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");
    let mut file = File::create(projects.join("session.jsonl")).expect("session");

    for (timestamp, uuid) in [
        ("2026-08-05T14:51:09.702Z", "record-1"),
        ("2026-08-05T14:51:09.710Z", "record-2"),
        ("2026-08-05T14:51:11.102Z", "record-3"),
    ] {
        writeln!(
            file,
            r#"{{"timestamp":"{timestamp}","sessionId":"session-1","uuid":"{uuid}","requestId":"request-1","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":746016,"cache_read_input_tokens":16038,"output_tokens":1479}}}}}}"#
        )
        .expect("repeated request");
    }
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-05T14:51:14.209Z","sessionId":"session-1","uuid":"record-4","requestId":"request-2","message":{{"id":"message-1","model":"claude-opus-5","usage":{{"input_tokens":2,"cache_creation_input_tokens":746016,"cache_read_input_tokens":16038,"output_tokens":1479}}}}}}"#
    )
    .expect("distinct request");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 2);
    assert_eq!(scan.diagnostics.duplicate_events, 2);
    assert_eq!(
        scan.events
            .iter()
            .map(|event| event.usage.computed_total())
            .sum::<u64>(),
        1_527_070
    );
}

#[test]
fn claude_stats_cache_is_parsed_as_summary_not_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    let mut file = File::create(dir.path().join("stats-cache.json")).expect("stats cache");
    writeln!(
        file,
        r#"{{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-01-21T17:21:43.119Z",
          "totalSessions": 61,
          "totalMessages": 15679,
          "modelUsage": {{
            "claude-opus-4-5-thinking": {{
              "inputTokens": 113622256,
              "outputTokens": 387,
              "cacheReadInputTokens": 282480618,
              "cacheCreationInputTokens": 10,
              "costUSD": 12.5
            }},
            "unknown/zero-usage-empty": {{
              "inputTokens": 0,
              "outputTokens": 0,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0
            }}
          }}
        }}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(scan.diagnostics.skipped_zero_events, 1);
    assert_eq!(
        scan.summaries[0]
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("claude-opus-4-5-thinking")
    );
    assert_eq!(scan.summaries[0].usage.input_tokens, Some(113622256));
    assert_eq!(scan.summaries[0].usage.cache_read_tokens, Some(282480618));
    assert_eq!(scan.summaries[0].usage.cache_creation_tokens, Some(10));
    assert_eq!(scan.summaries[0].usage.output_tokens, Some(387));
    assert_eq!(scan.summaries[0].cost.provider_reported_usd, Some(1250));
    assert_eq!(scan.summaries[0].metadata.total_sessions, Some(61));
    assert_eq!(scan.summaries[0].metadata.total_messages, Some(15679));
}

#[test]
fn claude_stats_cache_zero_cost_family_alias_still_estimates() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    let mut file = File::create(dir.path().join("stats-cache.json")).expect("stats cache");
    writeln!(
        file,
        r#"{{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-01-21T17:21:43.119Z",
          "totalSessions": 1,
          "totalMessages": 10,
          "modelUsage": {{
            "claude-opus-4-6-thinking": {{
              "inputTokens": 1000000,
              "outputTokens": 1000000,
              "cacheReadInputTokens": 1000000,
              "cacheCreationInputTokens": 0,
              "costUSD": 0
            }}
          }}
        }}"#
    )
    .expect("write");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(
        scan.summaries[0]
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("claude-opus-4-6")
    );
    assert_eq!(scan.summaries[0].cost.provider_reported_usd, None);
    assert_eq!(
        scan.summaries[0].cost.estimated_api_equivalent_usd,
        Some(3050)
    );
}

#[test]
fn claude_stats_cache_does_not_estimate_aggregate_across_pricing_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
    std::fs::write(
        dir.path().join("stats-cache.json"),
        r#"{
          "version": 2,
          "lastComputedDate": "2026-09-01",
          "firstSessionDate": "2026-08-31T00:00:00Z",
          "totalSessions": 2,
          "totalMessages": 4,
          "modelUsage": {
            "claude-sonnet-5": {
              "inputTokens": 1000000,
              "outputTokens": 1000000,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0
            }
          }
        }"#,
    )
    .expect("stats cache");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    assert_eq!(
        scan.summaries[0].cost.estimated_api_equivalent_micro_usd,
        None
    );
    assert_eq!(scan.summaries[0].cost.estimated_api_equivalent_usd, None);
    assert_eq!(
        scan.summaries[0].cost.pricing_source.as_deref(),
        Some("unknown")
    );
}

#[test]
fn claude_scan_respects_selected_cache_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(&projects).expect("projects");

    let first = projects.join("a.jsonl");
    let second = projects.join("b.jsonl");
    std::fs::write(
        &first,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("first");
    std::fs::write(
        &second,
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n",
    )
    .expect("second");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let selected = [canonical_display(&first)].into_iter().collect();
    let scan = scan_claude_source(
        &ClaudeCodeAdapter,
        &source,
        &ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: Some(selected),
        },
    )
    .expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.diagnostics.files_scanned, 1);
    assert_eq!(scan.diagnostics.files_skipped_unchanged, 1);
    assert_eq!(scan.events[0].usage.computed_total(), 3);
}

#[test]
fn claude_partial_jsonl_scan_only_emits_selected_task_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let first = project_store.join("first.jsonl");
    let second = project_store.join("second.jsonl");
    std::fs::write(
        &first,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("first");
    std::fs::write(
        &second,
        "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"sessionId\":\"session-b\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n",
    )
    .expect("second");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            concat!(
                "{{\"version\":1,\"entries\":[",
                "{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Fix parser bug\"}},",
                "{{\"sessionId\":\"session-b\",\"fullPath\":\"{}\",\"summary\":\"Review release notes\"}}",
                "]}}"
            ),
            first.display(),
            second.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let selected = [canonical_display(&first)].into_iter().collect();
    let scan = scan_claude_source(
        &ClaudeCodeAdapter,
        &source,
        &ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: Some(selected),
        },
    )
    .expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert_eq!(scan.task_spans.len(), 1);
    assert_eq!(scan.task_spans[0].session_id.as_deref(), Some("session-a"));
    assert_eq!(scan.task_spans[0].title, "Fix parser bug");
    assert_eq!(scan.task_spans[0].usage.computed_total(), 3);
    assert_eq!(scan.task_spans[0].linked_event_ids.len(), 1);
}

#[test]
fn claude_partial_stats_cache_scan_does_not_emit_unscanned_task_spans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Investigate cache issue\"}}]}}",
            session.display()
        ),
    )
    .expect("session index");
    std::fs::write(
        root.join("stats-cache.json"),
        r#"{
          "version": 2,
          "lastComputedDate": "2026-05-13",
          "firstSessionDate": "2026-05-01T00:00:00Z",
          "totalSessions": 1,
          "totalMessages": 2,
          "modelUsage": {
            "claude-opus-4-5-thinking": {
              "inputTokens": 11,
              "outputTokens": 7,
              "cacheReadInputTokens": 0,
              "cacheCreationInputTokens": 0,
              "costUSD": 0.12
            }
          }
        }"#,
    )
    .expect("stats cache");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let selected = [canonical_display(&root.join("stats-cache.json"))]
        .into_iter()
        .collect();
    let scan = scan_claude_source(
        &ClaudeCodeAdapter,
        &source,
        &ScanOptions {
            device_id: "device".to_string(),
            collect_tasks: true,
            selected_cache_keys: Some(selected),
        },
    )
    .expect("scan");

    assert!(scan.events.is_empty());
    assert_eq!(scan.summaries.len(), 1);
    assert!(scan.task_spans.is_empty());
}

#[test]
fn claude_task_entry_matches_scanned_file_handles_jsonl_suffix_mismatch() {
    let path = Path::new("/tmp/example-session");
    let scanned = [canonical_display(&path.with_extension("jsonl"))]
        .into_iter()
        .collect();

    assert!(claude_task_entry_matches_scanned_file(path, &scanned));
    assert!(claude_task_entry_matches_scanned_file(
        &path.with_extension("jsonl"),
        &scanned
    ));
}

#[test]
fn claude_task_spans_use_reconciliation_hash_for_suffix_mismatched_index_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session-a.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Investigate cleanup mismatch\"}}]}}",
            session.with_extension("").display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.task_spans.len(), 1);
    assert_eq!(
        scan.task_spans[0].source_file_path_hash.as_deref(),
        Some(hash_text(&canonical_display(&session)).as_str())
    );
}

#[test]
fn claude_scan_skips_task_entries_when_task_collection_is_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let project_store = root.join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");

    let session = project_store.join("session-a.jsonl");
    std::fs::write(
        &session,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"sessionId\":\"session-a\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    )
    .expect("session");
    std::fs::write(
        project_store.join("sessions-index.json"),
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-a\",\"fullPath\":\"{}\",\"summary\":\"Skip task collection\"}}]}}",
            session.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        root,
        LocationOrigin::Configured,
    );
    let scan =
        scan_claude_source(&ClaudeCodeAdapter, &source, &options_without_tasks()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    assert!(scan.task_spans.is_empty());
}

#[test]
fn claude_scan_candidates_change_when_sessions_index_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let project_store = projects.join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");
    let sessions_index = project_store.join("sessions-index.json");
    std::fs::write(
        &sessions_index,
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-1\",\"fullPath\":\"{}\",\"projectPath\":\"/tmp/workspace-a\"}}]}}",
            session_path.display()
        ),
    )
    .expect("session index");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let first = claude_scan_candidates(&source, "test-adapter").expect("first candidates");
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(
        &sessions_index,
        format!(
            "{{\"version\":1,\"entries\":[{{\"sessionId\":\"session-1\",\"fullPath\":\"{}\",\"projectPath\":\"/tmp/workspace-b\"}}]}}",
            session_path.display()
        ),
    )
    .expect("updated session index");

    let second = claude_scan_candidates(&source, "test-adapter").expect("second candidates");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].cache_key, canonical_display(&session_path));
    assert_eq!(second[0].cache_key, canonical_display(&session_path));
    assert_ne!(first[0].cache_signature, second[0].cache_signature);
}

#[test]
fn claude_scan_candidates_invalidate_legacy_cache_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects");
    let project_store = projects.join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
    )
    .expect("session");

    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let legacy_namespace = {
        let adapter_id = source.adapter_id.as_deref().unwrap_or("");
        let path_hash = source.path_hash.as_deref().unwrap_or("");
        hash_text(&format!(
            "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{}:{path_hash}:{}",
            source.provider, source.source_kind, "test-adapter", "project-context.v1"
        ))
    };
    let legacy_namespaces = ScanCacheNamespaces {
        current: legacy_namespace,
        compatible: Vec::new(),
    };
    let legacy_candidate = scan_candidate(session_path.clone(), None, &legacy_namespaces);
    let current = claude_scan_candidates(&source, "test-adapter").expect("current candidates");

    assert_eq!(current.len(), 1);
    assert_eq!(current[0].cache_key, canonical_display(&session_path));
    assert_ne!(legacy_candidate.cache_signature, current[0].cache_signature);
}

#[test]
fn claude_archive_candidates_use_a_scoped_parser_revision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project_store = dir.path().join("projects").join("example-workspace");
    std::fs::create_dir_all(&project_store).expect("project store");
    let session_path = project_store.join("session.jsonl");
    std::fs::write(
        &session_path,
        "{\"sessionId\":\"session-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    )
    .expect("session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );
    let adapter = ClaudeCodeAdapter;

    let usage = adapter.scan_candidates(&source).expect("usage candidates");
    let archive = adapter
        .archive_scan_candidates(&source)
        .expect("archive candidates");

    assert_eq!(usage.len(), 1);
    assert_eq!(archive.len(), 1);
    assert_eq!(archive[0].cache_key, usage[0].cache_key);
    assert_ne!(archive[0].cache_signature, usage[0].cache_signature);
    assert!(archive[0].compatible_cache_signatures.is_empty());
}

#[test]
fn usage_counts_support_common_shapes() {
    let value: Value = serde_json::json!({
        "inputTokens": 10,
        "outputTokens": 20,
        "cacheCreationInputTokens": 2,
        "cacheReadInputTokens": 3
    });
    let usage = claude_usage_counts_from_value(&value);
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.cache_creation_tokens, Some(2));
    assert_eq!(usage.cache_read_tokens, Some(3));
    assert_eq!(usage.computed_total(), 35);
}

#[test]
fn claude_usage_counts_preserve_cache_creation_lifetimes() {
    let value: Value = serde_json::json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "cache_creation_input_tokens": 248,
        "cache_creation": {
            "ephemeral_5m_input_tokens": 148,
            "ephemeral_1h_input_tokens": 100
        }
    });

    let usage = claude_usage_counts_from_value(&value);

    assert_eq!(usage.cache_creation_tokens, Some(248));
    assert_eq!(usage.cache_creation_5m_tokens, Some(148));
    assert_eq!(usage.cache_creation_1h_tokens, Some(100));
    assert_eq!(usage.computed_total(), 278);
}

#[test]
fn claude_usage_counts_derive_combined_cache_creation_tokens() {
    let value: Value = serde_json::json!({
        "cache_creation": {
            "ephemeral_5m_input_tokens": 8,
            "ephemeral_1h_input_tokens": 5
        }
    });

    let usage = claude_usage_counts_from_value(&value);

    assert_eq!(usage.cache_creation_tokens, Some(13));
    assert_eq!(usage.cache_creation_5m_tokens, Some(8));
    assert_eq!(usage.cache_creation_1h_tokens, Some(5));
}

#[test]
fn claude_adapter_does_not_infer_reasoning_level_from_thinking_model_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        serde_json::json!({
            "timestamp": "2026-05-01T00:00:00Z",
            "sessionId": "session-thinking",
            "model": "claude-opus-4-5-thinking",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20
            }
        })
        .to_string()
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("claude-opus-4-5-thinking"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw, None);
}

#[test]
fn claude_collects_effort_and_effective_speed_but_ignores_service_tier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        serde_json::json!({
            "timestamp": "2026-08-01T00:00:00Z",
            "sessionId": "session-fast",
            "type": "assistant",
            "effort": "medium",
            "message": {
                "role": "assistant",
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "speed": "fast",
                    "service_tier": "priority"
                }
            }
        })
        .to_string()
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.speed.as_deref(), Some("fast"));
    assert_eq!(model.reasoning_level, Some(ReasoningLevel::Medium));
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("medium"));
    assert_eq!(
        scan.events[0].cost.estimated_api_equivalent_micro_usd,
        Some(2_000)
    );
    assert_eq!(
        scan.events[0].cost.pricing_source.as_deref(),
        Some("claude_code_api_pricing:claude-opus-5:fast")
    );
    assert!(!serde_json::to_value(model)
        .expect("serialize model")
        .to_string()
        .contains("service_tier"));
    assert!(scan.events[0].runtime.is_none());
}

#[test]
fn claude_carries_max_thinking_tokens_forward_as_raw_reasoning_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let projects = dir.path().join("projects/workspace");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::write(
        projects.join("session.jsonl"),
        [
            serde_json::json!({
                "timestamp": "2026-05-01T00:00:00Z",
                "sessionId": "session-thinking-budget",
                "type": "user",
                "thinkingMetadata": {
                    "maxThinkingTokens": 31999
                },
                "message": {
                    "role": "user",
                    "content": "hello"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-01T00:00:02Z",
                "sessionId": "session-thinking-budget",
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-5-thinking",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20
                    }
                }
            })
            .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .expect("write session");
    let source = SourceLocation::local_adapter(
        CLAUDE_CODE_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 1);
    let model = scan.events[0].model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("claude-opus-4-5-thinking"));
    assert_eq!(model.reasoning_level, None);
    assert_eq!(model.reasoning_level_raw.as_deref(), Some("31999"));
}
