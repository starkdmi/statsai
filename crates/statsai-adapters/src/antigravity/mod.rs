//! Google Antigravity: usage from the local conversation SQLite files.
//!
//! Antigravity 2.x keeps each conversation in
//! `~/.gemini/antigravity/conversations/<id>.db`, with token usage recorded on
//! every model request. Older conversations are encrypted `<id>.pb` files that
//! cannot be read offline; they are counted, not read.
//!
//! `~/.gemini/antigravity-ide/` and `~/.gemini/antigravity-backup/` can hold
//! copies of the same conversations. They are never discovered: an event id
//! includes its source, so a second source would count every copy again.

mod conversation;
mod models;
mod proto;

use crate::*;
use anyhow::Result;
use conversation::{read_conversation, CallKind, ConversationScan, ModelCall};
use models::{
    conversation_file_signature, remember_file_names, resolve_model_info, ModelNames,
    StoreModelNames,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
pub(crate) use crate::tests::options;

#[derive(Debug, Default)]
pub struct AntigravityAdapter;

impl ProviderAdapter for AntigravityAdapter {
    fn id(&self) -> &'static str {
        "antigravity-local-sqlite"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        ANTIGRAVITY_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        // `antigravity-cli` is where the `agy` CLI is reported to keep its
        // conversations. No install has been checked; it is read when present
        // and expected to share the app's schema.
        discover_sources_from_env_or_defaults(
            self,
            &["ANTIGRAVITY_DATA_DIRS"],
            &[".gemini/antigravity", ".gemini/antigravity-cli"],
            antigravity_root_is_source,
        )
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        antigravity_scan_candidates(source, self.version())
    }

    fn unreadable_history_files(&self, source: &SourceLocation) -> Result<u64> {
        source_root_path(source).map_or(Ok(0), |root| encrypted_conversation_count(&root))
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_antigravity_source(self, source, options)
    }
}

/// The folder that holds `conversations/`, given it or `conversations/` itself.
pub(crate) fn antigravity_root(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(|name| name == "conversations") {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn antigravity_conversations_dir(path: &Path) -> PathBuf {
    antigravity_root(path).join("conversations")
}

pub(crate) fn antigravity_root_is_source(path: &Path) -> bool {
    antigravity_conversations_dir(path).is_dir()
}

/// The conversation files directly in `conversations/` with `extension`,
/// without following links, sorted by path.
fn conversation_files(root: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let dir = antigravity_conversations_dir(root);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some(extension)
        {
            files.push(path);
        }
    }
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    Ok(files)
}

pub(crate) fn antigravity_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(root) = source_root_path(source) else {
        return Ok(Vec::new());
    };
    let cache_namespaces = scan_cache_namespaces(source, adapter_version);
    Ok(conversation_files(&root, "db")?
        .into_iter()
        .map(|path| {
            let journals = antigravity_journal_signature(&path);
            scan_candidate(path, Some(&journals), &cache_namespaces)
        })
        .collect())
}

/// Newer conversations use a rollback journal and some older ones WAL. Pages in
/// either may not have reached the database file yet, so both sign it. The
/// shared-memory sidecar is coordination state and does not.
pub(crate) fn antigravity_journal_signature(db_path: &Path) -> String {
    let path = db_path.to_string_lossy();
    let signatures = ["-wal", "-journal"]
        .into_iter()
        .map(|suffix| file_metadata_signature(Path::new(&format!("{path}{suffix}"))))
        .collect::<Vec<_>>();
    hash_text(&signatures.join(":"))
}

/// Encrypted `.pb` conversations with no readable `.db` of the same id.
fn encrypted_conversation_count(root: &Path) -> Result<u64> {
    Ok(conversation_files(root, "pb")?
        .iter()
        .filter(|path| !path.with_extension("db").is_file())
        .count() as u64)
}

pub(crate) fn scan_antigravity_source(
    adapter: &AntigravityAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    if source_root_path(source).is_none() {
        return Ok(scan);
    }
    scan.diagnostics.unreadable_history_files = adapter.unreadable_history_files(source)?;
    let candidates = antigravity_scan_candidates(source, adapter.version())?;
    let mut store_names = StoreModelNames::new(candidates.iter().map(|c| c.path.clone()).collect());
    let mut projects = ProjectContextCache::new();
    let mut seen = EventDedupIndex::new();
    for candidate in &candidates {
        if !options.should_scan(&candidate.cache_key) {
            scan.diagnostics.files_skipped_unchanged += 1;
            continue;
        }
        scan.diagnostics.files_scanned += 1;
        // The signature is taken before reading, so a write that lands during
        // the read leaves a stale signature and the next scan reads it again.
        let signature = conversation_file_signature(&candidate.path);
        let conversation = match open_sqlite_readonly(&candidate.path)
            .and_then(|connection| read_conversation(&connection, &candidate.path, &mut projects))
        {
            Ok(conversation) => conversation,
            // Antigravity holding the file mid-commit is a reason to read it
            // again, not to record it read with nothing in it.
            Err(error) if sqlite_busy(&error) => return Err(error),
            // One damaged file must not stop the scan of every other
            // conversation.
            Err(_) => {
                scan.diagnostics.invalid_rows += 1;
                continue;
            }
        };
        let names = Arc::new(conversation.names.clone());
        remember_file_names(&candidate.path, signature, names);
        emit_conversation(
            &mut FileParseContext {
                adapter,
                source,
                options,
                scan: &mut scan,
                seen: &mut seen,
            },
            &candidate.path,
            conversation,
            &mut store_names,
        )?;
    }
    Ok(scan)
}

fn sqlite_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

fn emit_conversation(
    context: &mut FileParseContext<'_, AntigravityAdapter>,
    db_path: &Path,
    conversation: ConversationScan,
    store_names: &mut StoreModelNames,
) -> Result<()> {
    let diagnostics = &mut context.scan.diagnostics;
    diagnostics.raw_rows += conversation.request_steps;
    diagnostics.invalid_rows += conversation.invalid_steps;
    diagnostics.skipped_zero_events += conversation.requests_without_usage;
    if let Some(parent) = conversation.trajectory.parent_conversation_id.as_deref() {
        let label = conversation
            .trajectory
            .agent_name
            .as_deref()
            .unwrap_or("sub-agent");
        if let Some(title) = statsai_core::provider_session_name(Some(label), 40) {
            context.scan.session_names.push(statsai_core::SessionName {
                local_session_id_hash: hash_text(&conversation.conversation_id),
                title,
                parent_local_session_id_hash: Some(hash_text(parent)),
            });
        }
    }
    for call in &conversation.calls {
        if call.usage.computed_total() == 0 {
            context.scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        context.scan.diagnostics.candidate_usage_rows += 1;
        let event = call_event(context, db_path, &conversation, call, store_names)?;
        push_deduped(
            context.scan,
            context.seen,
            event,
            DuplicateSelection::KeepFirst,
        );
    }
    Ok(())
}

fn call_event(
    context: &mut FileParseContext<'_, AntigravityAdapter>,
    db_path: &Path,
    conversation: &ConversationScan,
    call: &ModelCall,
    store_names: &mut StoreModelNames,
) -> Result<UsageEvent> {
    let (model, model_named) = match call.model_id {
        Some(model_id) => {
            let (model, synthetic) = model_info_for(model_id, &conversation.names, store_names)?;
            (Some(model), !synthetic)
        }
        None => (None, false),
    };
    if !model_named {
        context.scan.diagnostics.model_fallbacks += 1;
    }
    let (timestamp, timestamp_inferred) = match call.created_at {
        Some(created_at) => (created_at, false),
        None => {
            context.scan.diagnostics.timestamp_fallbacks += 1;
            (
                call.fallback_at
                    .or_else(|| file_modified_at(db_path))
                    .unwrap_or_else(Utc::now),
                true,
            )
        }
    };
    let runtime =
        (call.latency_ms.is_some() || call.time_to_first_token_ms.is_some()).then(|| RuntimeInfo {
            runtime_name: None,
            host_id: None,
            latency_ms: call.latency_ms,
            latency_source: call.latency_ms.map(|_| LatencySource::Explicit),
            time_to_first_token_ms: call.time_to_first_token_ms,
            prompt_eval_duration_ms: None,
            eval_duration_ms: None,
            total_messages: None,
            user_messages: None,
            assistant_messages: None,
            developer_messages: None,
        });
    let (event_kind, record) = match call.kind {
        CallKind::PlannerResponse => (
            "antigravity_planner_response",
            format!("{}:{}", conversation.conversation_id, call.step_index),
        ),
        CallKind::Checkpoint { attempt } => (
            "antigravity_checkpoint",
            format!(
                "{}:{}:{attempt}",
                conversation.conversation_id, call.step_index
            ),
        ),
    };
    Ok(usage_event(
        context.adapter,
        context.source,
        context.options,
        ProviderEventParts {
            timestamp,
            session_started_at: None,
            session_ended_at: None,
            duration_seconds: None,
            model_inferred: model.is_none(),
            model,
            usage: call.usage.clone(),
            runtime,
            session_raw: conversation.conversation_id.clone(),
            project: conversation.trajectory.project.clone(),
            event_kind,
            source_file: db_path,
            source_line_number: usize::try_from(call.step_index).ok(),
            source_type: "sqlite:steps",
            timestamp_inferred,
            // Built from the conversation and step, never from `response_id`,
            // whose shape differs by provider.
            deduplication: EventDeduplication::ProviderRecord(record),
            dedupe_salt: None,
        },
    ))
}

/// The model a request used: from the conversation's own names when they name
/// it fully, otherwise from every conversation in the store.
fn model_info_for(
    model_id: u64,
    local: &ModelNames,
    store_names: &mut StoreModelNames,
) -> Result<(ModelInfo, bool)> {
    if local.resolves(model_id) {
        Ok(resolve_model_info(model_id, &[local]))
    } else {
        Ok(resolve_model_info(model_id, &[local, store_names.get()?]))
    }
}

#[cfg(test)]
mod tests;
