mod parse;
mod stats;
pub(crate) use parse::*;
pub(crate) use stats::*;

use crate::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::cache::GROK_BUILD_SCAN_CACHE_PARSER_REVISION;
#[cfg(test)]
use crate::tests::options;
#[cfg(test)]
use statsai_core::{display_path, path_hash};

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

#[test]
fn grok_request_level_pricing_upgrade_advances_parser_revision() {
    let revision = GROK_BUILD_SCAN_CACHE_PARSER_REVISION
        .rsplit_once(".v")
        .and_then(|(_, value)| value.parse::<u32>().ok())
        .expect("Grok parser revision");

    assert!(revision > 19);
}

#[test]
fn grok_build_session_summary_records_local_session_stats() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-1");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-1", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "num_messages": 12,
            "current_model_id": "grok-build",
            "chat_format_version": 1,
            "git_remotes": ["https://github.com/example/repo.git"],
            "head_branch": "main"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "sessionDurationSeconds": 60,
            "avgTimeToFirstTokenMs": 1200,
            "avgResponseTimeMs": 2400,
            "turnCount": 3,
            "userMessageCount": 3,
            "assistantMessageCount": 9,
            "contextTokensUsed": 42_000,
            "contextWindowTokens": 256_000
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("chat_history.jsonl"),
        [
            serde_json::json!({"type": "system", "content": "system"}).to_string(),
            serde_json::json!({"type": "user", "content": [{"type": "text", "text": "hello"}]})
                .to_string(),
            serde_json::json!({"type": "assistant", "content": "hi"}).to_string(),
            serde_json::json!({"type": "reasoning", "summary": "thinking"}).to_string(),
            serde_json::json!({"type": "tool_result", "content": "ok"}).to_string(),
        ]
        .join("\n"),
    )
    .expect("chat history");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 41_000}}})
                .to_string(),
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 45_000}}})
                .to_string(),
            serde_json::json!({"params": {"_meta": {"promptId": "p2", "totalTokens": 7_000}}})
                .to_string(),
            serde_json::json!({"params": {"update": {"tokens_used": 40_000}}}).to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        serde_json::json!({"type": "turn", "phase": "done"}).to_string(),
    )
    .expect("events");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.events.len(), 0);
    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.provider, GROK_BUILD_PROVIDER);
    assert_eq!(summary.metadata.total_sessions, Some(1));
    assert_eq!(summary.metadata.total_messages, Some(12));
    assert_eq!(summary.usage.input_tokens, Some(52_000));
    assert_eq!(summary.usage.total_tokens, Some(52_000));
    assert_eq!(summary.usage.requests, Some(3));
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(5));
    assert_eq!(summary.cost.confidence, Confidence::Low);
    let project = summary.project.as_ref().expect("project");
    assert_eq!(
        project.path_label.as_deref(),
        Some(display_path(dir.path()).as_str())
    );
    assert_eq!(
        project.path_hash.as_deref(),
        Some(path_hash(dir.path()).as_str())
    );
    assert_eq!(project.repo_label.as_deref(), Some("example/repo"));
    assert_eq!(project.branch_label.as_deref(), Some("main"));
    assert_eq!(
        summary
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.user_messages),
        Some(3)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("reasoning=1")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("chat_rows=5")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("prompts=2;prompt_context_tokens=52000")),
        Some(true)
    );
}

#[test]
fn grok_build_prefers_unified_log_inference_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-usage");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-usage", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "current_model_id": "grok-composer-2.5-fast",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("updates.jsonl"),
        serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 999_999}}})
            .to_string(),
    )
    .expect("updates");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-06-09T14:22:45.131Z",
                "sid": "session-usage",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 1_000_000,
                    "cached_prompt_tokens": 400_000,
                    "completion_tokens": 100_000,
                    "reasoning_tokens": 50_000,
                    "model_elapsed_ms": 3_000,
                    "ttft_ms": 1_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-06-09T14:22:48.525Z",
                "sid": "other-session",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 9_000_000,
                    "cached_prompt_tokens": 0,
                    "completion_tokens": 9_000_000
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.usage.input_tokens, Some(600_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(400_000));
    assert_eq!(summary.usage.cache_creation_tokens, None);
    assert_eq!(summary.usage.output_tokens, Some(100_000));
    assert_eq!(summary.usage.reasoning_tokens, Some(50_000));
    assert_eq!(summary.usage.requests, Some(1));
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(425));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary
            .cost
            .pricing_source
            .as_deref()
            .map(|value| value.contains("cursor_model_pricing:composer-2.5-fast")),
        Some(true)
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("inference_rows=1;usage_source=unified_log")),
        Some(true)
    );
}

#[test]
fn grok_build_scan_tolerates_malformed_jsonl_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-malformed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-malformed", "cwd": dir.path()},
            "updated_at": "2026-06-09T13:53:52Z",
            "current_model_id": "grok-build"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "turnCount": 1,
            "contextTokensUsed": 999
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("chat_history.jsonl"),
        [
            serde_json::json!({"type": "user", "content": "hello"}).to_string(),
            "{\"type\":\"assistant\"".to_string(),
        ]
        .join("\n"),
    )
    .expect("chat");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({"params": {"_meta": {"promptId": "p1", "totalTokens": 123}}})
                .to_string(),
            "{\"params\":".to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        [
            serde_json::json!({"type": "turn"}).to_string(),
            "{".to_string(),
        ]
        .join("\n"),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            "{".to_string(),
            serde_json::json!({
                "sid": "session-malformed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.diagnostics.invalid_rows, 4);
    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(summary.usage.input_tokens, Some(90));
    assert_eq!(summary.usage.cache_read_tokens, Some(10));
    assert_eq!(summary.usage.output_tokens, Some(20));
    assert_eq!(summary.usage.total_tokens, None);
    assert_eq!(summary.usage.requests, Some(1));
}

#[test]
fn grok_summary_candidate_changes_when_session_siblings_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-1");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-1"},
            "updated_at": "2026-06-09T13:53:52Z"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(session.join("signals.json"), "{}").expect("signals");
    std::fs::write(session.join("chat_history.jsonl"), "").expect("chat");
    std::fs::write(session.join("updates.jsonl"), "").expect("updates");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = grok_build_scan_candidates(&source, "0").expect("before");
    std::fs::write(
        session.join("chat_history.jsonl"),
        serde_json::json!({"type": "user", "content": "hello"}).to_string(),
    )
    .expect("updated chat");
    let after = grok_build_scan_candidates(&source, "0").expect("after");

    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(
        before[0].path.file_name().and_then(|name| name.to_str()),
        Some("summary.json")
    );
    assert_ne!(before[0].cache_signature, after[0].cache_signature);
}

#[test]
fn grok_candidates_tolerate_malformed_unified_log_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-1");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-1"},
            "updated_at": "2026-06-09T13:53:52Z"
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(session.join("signals.json"), "{}").expect("signals");
    std::fs::write(session.join("chat_history.jsonl"), "").expect("chat");
    std::fs::write(session.join("updates.jsonl"), "").expect("updates");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            "{".to_string(),
            serde_json::json!({
                "sid": "session-1",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let candidates = grok_build_scan_candidates(&source, "0").expect("candidates");

    assert_eq!(candidates.len(), 1);
}

#[test]
fn grok_summary_candidate_changes_only_for_matching_unified_log_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_a = dir.path().join("sessions/%2Fworkspace/session-a");
    let session_b = dir.path().join("sessions/%2Fworkspace/session-b");
    std::fs::create_dir_all(&session_a).expect("session a");
    std::fs::create_dir_all(&session_b).expect("session b");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs");
    for (session_dir, session_id) in [(&session_a, "session-a"), (&session_b, "session-b")] {
        std::fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info": {"id": session_id},
                "updated_at": "2026-06-09T13:53:52Z"
            })
            .to_string(),
        )
        .expect("summary");
        std::fs::write(session_dir.join("signals.json"), "{}").expect("signals");
        std::fs::write(session_dir.join("chat_history.jsonl"), "").expect("chat");
        std::fs::write(session_dir.join("updates.jsonl"), "").expect("updates");
    }
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        serde_json::json!({
            "ts": "2026-06-09T14:22:45.131Z",
            "sid": "session-a",
            "msg": "shell.turn.inference_done",
            "ctx": {
                "prompt_tokens": 100,
                "cached_prompt_tokens": 10,
                "completion_tokens": 20
            }
        })
        .to_string(),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let before = grok_build_scan_candidates(&source, "0").expect("before");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-06-09T14:22:45.131Z",
                "sid": "session-a",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100,
                    "cached_prompt_tokens": 10,
                    "completion_tokens": 20
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-06-09T14:25:45.131Z",
                "sid": "session-b",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200,
                    "cached_prompt_tokens": 20,
                    "completion_tokens": 30
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("updated unified log");
    let after = grok_build_scan_candidates(&source, "0").expect("after");

    let before_a = before
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_a))
        .expect("candidate a");
    let before_b = before
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_b))
        .expect("candidate b");
    let after_a = after
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_a))
        .expect("candidate a after");
    let after_b = after
        .iter()
        .find(|candidate| candidate.path.starts_with(&session_b))
        .expect("candidate b after");

    assert_eq!(before_a.cache_signature, after_a.cache_signature);
    assert_ne!(before_b.cache_signature, after_b.cache_signature);
}

#[test]
fn grok_build_prices_unified_log_inferences_independently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:52.141Z",
                "sid": "session-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:03.314Z",
                "sid": "session-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    let model = summary.model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("grok-4.6-build"));
    assert_eq!(model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(summary.usage.input_tokens, Some(180_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(120_000));
    assert_eq!(summary.usage.output_tokens, Some(20_000));
    assert_eq!(summary.usage.reasoning_tokens, None);
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(880_000)
    );
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(88));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:unified_log_inference_usage")
    );
    assert_eq!(
        summary
            .metadata
            .summary_version
            .as_deref()
            .map(|value| value.contains("inference_rows=2;usage_source=unified_log")),
        Some(true)
    );
}

#[test]
fn grok_build_prices_mixed_model_session_from_prompt_model_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed-models");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed-models", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("updates.jsonl"),
        [
            serde_json::json!({
                "timestamp": 1_786_905_120,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "user_message",
                        "content": {"type": "text", "text": "first"},
                        "_meta": {"modelId": "grok-4.5", "promptIndex": 0}
                    },
                    "_meta": {
                        "eventId": "prompt-0",
                        "agentTimestampMs": 1_786_905_120_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_130,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "assistant_chunk",
                        "content": {"type": "text", "text": "working"}
                    },
                    "_meta": {
                        "promptId": "req-4.5",
                        "totalTokens": 100_000,
                        "agentTimestampMs": 1_786_905_130_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_180,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "user_message",
                        "content": {"type": "text", "text": "/model grok-4.6"},
                        "_meta": {"modelId": "grok-4.6", "promptIndex": 1}
                    },
                    "_meta": {
                        "eventId": "prompt-1",
                        "agentTimestampMs": 1_786_905_180_000i64
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": 1_786_905_200,
                "method": "session/update",
                "params": {
                    "sessionId": "session-mixed-models",
                    "update": {
                        "sessionUpdate": "assistant_chunk",
                        "content": {"type": "text", "text": "switched"}
                    },
                    "_meta": {
                        "promptId": "req-4.6",
                        "totalTokens": 200_000,
                        "agentTimestampMs": 1_786_905_200_000i64
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:00Z",
                "type": "turn_started",
                "model_id": "grok-4.5",
                "turn_number": 0
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:32:00.100Z",
                "type": "loop_started",
                "loop_index": 0
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:00Z",
                "type": "turn_started",
                "model_id": "grok-4.6",
                "turn_number": 1
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-mixed-models",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-mixed-models",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000,
                    "reasoning_tokens": 0
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    let model = summary.model.as_ref().expect("model");
    assert_eq!(model.name.as_deref(), Some("grok-4.6-build"));
    assert_eq!(model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(summary.usage.input_tokens, Some(180_000));
    assert_eq!(summary.usage.cache_read_tokens, Some(120_000));
    assert_eq!(summary.usage.output_tokens, Some(20_000));
    assert_eq!(summary.usage.requests, Some(2));
    // grok-4.5 short (60k/40k/10k @ $2/$0.30/$6) + grok-4.6 long (120k/80k/10k
    // @ $4/$1/$12) = $0.192 + $0.680. Pricing both as current_model_id (4.6)
    // would be $0.880.
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(872_000)
    );
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(87));
    assert_eq!(summary.cost.confidence, Confidence::Medium);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:mixed:unified_log_inference_usage")
    );
}

#[test]
fn grok_build_keeps_unresolved_mixed_models_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-mixed-unknown");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-mixed-unknown", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-mixed-unknown",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-mixed-unknown",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6-build")
    );
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(summary.cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(summary.cost.estimated_api_equivalent_usd, None);
    assert_eq!(summary.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn grok_build_keeps_partial_observation_mixed_models_used_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-partial-mixed");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::create_dir_all(dir.path().join("logs")).expect("logs dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-partial-mixed", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6-build",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "modelsUsed": ["grok-4.5", "grok-4.6"],
            "primaryModelId": "grok-4.6",
            "turnCount": 2
        })
        .to_string(),
    )
    .expect("signals");
    std::fs::write(
        session.join("updates.jsonl"),
        serde_json::json!({
            "timestamp": 1_786_905_120,
            "method": "session/update",
            "params": {
                "sessionId": "session-partial-mixed",
                "update": {
                    "sessionUpdate": "user_message",
                    "content": {"type": "text", "text": "first"},
                    "_meta": {"modelId": "grok-4.5", "promptIndex": 0}
                },
                "_meta": {
                    "eventId": "prompt-0",
                    "agentTimestampMs": 1_786_905_120_000i64
                }
            }
        })
        .to_string(),
    )
    .expect("updates");
    std::fs::write(
        session.join("events.jsonl"),
        serde_json::json!({
            "ts": "2026-08-16T18:32:00Z",
            "type": "turn_started",
            "model_id": "grok-4.5",
            "turn_number": 0
        })
        .to_string(),
    )
    .expect("events");
    std::fs::write(
        dir.path().join("logs/unified.jsonl"),
        [
            serde_json::json!({
                "ts": "2026-08-16T18:32:10Z",
                "sid": "session-partial-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 1,
                    "prompt_tokens": 100_000,
                    "cached_prompt_tokens": 40_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
            serde_json::json!({
                "ts": "2026-08-16T18:33:20Z",
                "sid": "session-partial-mixed",
                "msg": "shell.turn.inference_done",
                "ctx": {
                    "loop_index": 2,
                    "prompt_tokens": 200_000,
                    "cached_prompt_tokens": 80_000,
                    "completion_tokens": 10_000
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .expect("unified log");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6-build")
    );
    assert_eq!(summary.usage.requests, Some(2));
    // A sole grok-4.5 prompt/turn observation must not price the second
    // request as 4.5 when modelsUsed also reports grok-4.6 (that would be
    // $0.840). Attribution is incomplete, so cost stays unknown.
    assert_eq!(summary.cost.estimated_api_equivalent_micro_usd, None);
    assert_eq!(summary.cost.estimated_api_equivalent_usd, None);
    assert_eq!(summary.cost.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn grok_build_keeps_aggregate_prompt_context_conservative() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir
        .path()
        .join("sessions")
        .join("%2Fworkspace")
        .join("session-aggregate");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(
        session.join("summary.json"),
        serde_json::json!({
            "info": {"id": "session-aggregate", "cwd": dir.path()},
            "updated_at": "2026-08-16T18:39:58Z",
            "current_model_id": "grok-4.6",
            "chat_format_version": 1
        })
        .to_string(),
    )
    .expect("summary");
    std::fs::write(
        session.join("signals.json"),
        serde_json::json!({
            "turnCount": 2,
            "contextTokensUsed": 300_000
        })
        .to_string(),
    )
    .expect("signals");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        dir.path(),
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(summary.usage.input_tokens, Some(300_000));
    assert_eq!(summary.usage.requests, Some(2));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(600_000)
    );
    assert_eq!(summary.cost.confidence, Confidence::Low);
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:prompt_context_token_footprint")
    );
}

#[test]
fn grok_fixture_session_keeps_grok_4_6_identity_and_is_priced() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/basic");
    let source = SourceLocation::local_adapter(
        GROK_BUILD_PROVIDER,
        "test",
        "0",
        &root,
        LocationOrigin::Configured,
    );

    let scan = scan_grok_build_source(&GrokBuildAdapter, &source, &options()).expect("scan");

    assert_eq!(scan.summaries.len(), 1);
    let summary = &scan.summaries[0];
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(
        summary
            .model
            .as_ref()
            .and_then(|model| model.normalized_name.as_deref()),
        Some("grok-4.6")
    );
    assert_eq!(summary.usage.input_tokens, Some(15_534));
    assert_eq!(summary.usage.cache_read_tokens, Some(42_624));
    assert_eq!(summary.usage.output_tokens, Some(917));
    assert_eq!(summary.usage.reasoning_tokens, Some(508));
    assert_eq!(summary.usage.requests, Some(3));
    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(60_930)
    );
    assert_eq!(
        summary.cost.pricing_source.as_deref(),
        Some("xai_api_pricing:grok-4.6:unified_log_inference_usage")
    );
}

#[test]
fn grok_inference_sample_costs_stay_unknown_when_unpriced() {
    let observed_at = Utc::now();
    let sample = GrokInferenceSample {
        usage: UsageCounts {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            requests: Some(1),
            ..UsageCounts::default()
        },
        observed_at: Some(observed_at),
    };

    let missing_model = estimate_grok_inference_sample_costs(
        GROK_BUILD_PROVIDER,
        None,
        std::slice::from_ref(&sample),
        &[],
        &[],
        &[],
        &observed_at,
    );
    let empty = estimate_grok_inference_sample_costs(
        GROK_BUILD_PROVIDER,
        Some(&ModelInfo {
            name: Some("grok-4.6".to_string()),
            normalized_name: Some("grok-4.6".to_string()),
            provider_model_id: Some("grok-4.6".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }),
        &[],
        &[],
        &[],
        &[],
        &observed_at,
    );

    assert_eq!(missing_model.estimated_api_equivalent_micro_usd, None);
    assert_eq!(missing_model.estimated_api_equivalent_usd, None);
    assert_eq!(missing_model.pricing_source.as_deref(), Some("unknown"));
    assert_eq!(empty.estimated_api_equivalent_micro_usd, None);
    assert_eq!(empty.pricing_source.as_deref(), Some("unknown"));
}

#[test]
fn grok_inference_model_resolution_joins_prompt_model_id_by_timestamp() {
    let first = DateTime::parse_from_rfc3339("2026-08-16T18:32:10Z")
        .expect("first")
        .with_timezone(&Utc);
    let second = DateTime::parse_from_rfc3339("2026-08-16T18:33:20Z")
        .expect("second")
        .with_timezone(&Utc);
    let prompt_models = [
        GrokModelObservation {
            model_id: "grok-4.5".to_string(),
            observed_at: Some(
                DateTime::parse_from_rfc3339("2026-08-16T18:32:00Z")
                    .expect("prompt 0")
                    .with_timezone(&Utc),
            ),
        },
        GrokModelObservation {
            model_id: "grok-4.6".to_string(),
            observed_at: Some(
                DateTime::parse_from_rfc3339("2026-08-16T18:33:00Z")
                    .expect("prompt 1")
                    .with_timezone(&Utc),
            ),
        },
    ];
    let current = model_info("grok-4.6-build");

    let first_model = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(first),
        },
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    )
    .expect("first model");
    let second_model = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(second),
        },
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    )
    .expect("second model");
    let unresolved = resolve_grok_inference_sample_model(
        &GrokInferenceSample {
            usage: UsageCounts::default(),
            observed_at: Some(first),
        },
        &[],
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    );

    assert_eq!(first_model.name.as_deref(), Some("grok-4.5"));
    assert_eq!(first_model.normalized_name.as_deref(), Some("grok-4.5"));
    assert_eq!(second_model.name.as_deref(), Some("grok-4.6"));
    assert_eq!(second_model.normalized_name.as_deref(), Some("grok-4.6"));
    assert_eq!(unresolved, None);
}

#[test]
fn grok_inference_model_resolution_rejects_partial_observation_when_models_used_is_mixed() {
    let observed_at = DateTime::parse_from_rfc3339("2026-08-16T18:32:10Z")
        .expect("observed")
        .with_timezone(&Utc);
    let sample = GrokInferenceSample {
        usage: UsageCounts::default(),
        observed_at: Some(observed_at),
    };
    let prompt_models = [GrokModelObservation {
        model_id: "grok-4.5".to_string(),
        observed_at: Some(
            DateTime::parse_from_rfc3339("2026-08-16T18:32:00Z")
                .expect("prompt")
                .with_timezone(&Utc),
        ),
    }];
    let current = model_info("grok-4.6-build");

    let mixed = resolve_grok_inference_sample_model(
        &sample,
        &prompt_models,
        &[],
        &["grok-4.5".to_string(), "grok-4.6".to_string()],
        Some(&current),
    );
    let matching = resolve_grok_inference_sample_model(
        &sample,
        &prompt_models,
        &[],
        &["grok-4.5".to_string()],
        Some(&current),
    )
    .expect("matching modelsUsed");
    let empty_used =
        resolve_grok_inference_sample_model(&sample, &prompt_models, &[], &[], Some(&current))
            .expect("empty modelsUsed");

    assert_eq!(mixed, None);
    assert_eq!(matching.name.as_deref(), Some("grok-4.5"));
    assert_eq!(empty_used.name.as_deref(), Some("grok-4.5"));
}
