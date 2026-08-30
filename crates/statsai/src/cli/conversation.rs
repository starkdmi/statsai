use anyhow::{bail, ensure, Context, Result};
use statsai_adapters::{default_adapters, ArchiveScan, ProviderAdapter, ScanCandidateFile};
use statsai_core::{ArchiveContentKind, ArchiveConversation, SourceLocation};
use statsai_store::{ScanFileStateEntry, Store};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::args::{ConversationCommand, ConversationSubcommand};
use super::format::format_u64;
use super::scan::{scan_file_state_entries, scan_sources_for_adapter};
use super::source::{canonical_provider_name, preview_path_label};

pub(crate) fn conversation(
    command: ConversationCommand,
    store: &Store,
    device_id: &str,
) -> Result<()> {
    match command.command {
        ConversationSubcommand::Collect {
            provider,
            no_cache,
            verbose,
        } => collect_conversations(store, device_id, provider.as_deref(), no_cache, verbose),
        ConversationSubcommand::List {
            provider,
            limit,
            json,
        } => {
            let provider = canonical_conversation_provider_filter(provider.as_deref())?;
            let conversations =
                store.list_archive_conversations(provider, limit.clamp(1, 10_000))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&conversations)?);
            } else if conversations.is_empty() {
                println!("no archived conversations");
            } else {
                println!(
                    "{:<29} {:<13} {:>8} {:>10}  title",
                    "conversation", "provider", "items", "bytes"
                );
                for conversation in conversations {
                    println!(
                        "{:<29} {:<13} {:>8} {:>10}  {}{}",
                        conversation.conversation_id,
                        conversation.provider,
                        format_u64(conversation.item_count),
                        format_u64(conversation.content_bytes),
                        conversation.title.as_deref().unwrap_or("(untitled)"),
                        if conversation.missing_content_count > 0 {
                            " [partial]"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(())
        }
        ConversationSubcommand::Show {
            conversation_id,
            json,
        } => {
            let conversation = store
                .archive_conversation(&conversation_id)?
                .with_context(|| format!("archived conversation not found: {conversation_id}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&conversation)?);
            } else {
                print_archive_conversation(&conversation);
            }
            Ok(())
        }
        ConversationSubcommand::Search { query, limit, json } => {
            let hits = store.search_archive(&query, limit.clamp(1, 10_000))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("no archive matches");
            } else {
                for hit in hits {
                    let preview = compact_archive_preview(&hit.text, 220);
                    println!(
                        "{}  {}  {}\n  {}",
                        hit.conversation_id,
                        hit.role.as_deref().unwrap_or("unknown"),
                        hit.title.as_deref().unwrap_or("(untitled)"),
                        preview
                    );
                }
            }
            Ok(())
        }
        ConversationSubcommand::Stats { json } => {
            let stats = store.archive_stats()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!(
                    "archived conversations: {}",
                    format_u64(stats.conversations)
                );
                println!("archived items: {}", format_u64(stats.items));
                println!("text parts: {}", format_u64(stats.text_parts));
                println!("binary parts: {}", format_u64(stats.binary_parts));
                println!("text bytes: {}", format_u64(stats.text_bytes));
                println!("binary bytes: {}", format_u64(stats.binary_bytes));
                println!(
                    "missing artifacts/content: {}",
                    format_u64(stats.missing_content)
                );
            }
            Ok(())
        }
        ConversationSubcommand::Export {
            conversation_id,
            format,
        } => {
            let conversation = store
                .archive_conversation(&conversation_id)?
                .with_context(|| format!("archived conversation not found: {conversation_id}"))?;
            match format.as_str() {
                "json" => println!("{}", serde_json::to_string_pretty(&conversation)?),
                "markdown" | "md" => print_archive_markdown(&conversation),
                _ => bail!("unsupported conversation export format: {format}"),
            }
            Ok(())
        }
    }
}

fn collect_conversations(
    store: &Store,
    device_id: &str,
    provider_filter: Option<&str>,
    no_cache: bool,
    verbose: bool,
) -> Result<()> {
    let canonical_provider_filter = canonical_conversation_provider_filter(provider_filter)?;
    let configured_sources = store.list_sources()?;
    // Reduced durability covers the imports and nothing after them. An import
    // rebuilds from the provider's own files, so a commit lost to a power cut
    // costs a re-collect. The code-change refresh that follows carries metrics
    // forward for commits too old for Git to be rescanned for, and writes them
    // across several transactions, so it keeps the store's normal durability.
    // Anything added below belongs after this block unless it is as reproducible
    // as an import.
    let totals = {
        let _durability = store.relax_durability_for_bulk_import()?;
        collect_archive_sources(
            store,
            canonical_provider_filter,
            &configured_sources,
            no_cache,
            verbose,
        )?
    };
    println!(
        "archive collection: sources={} conversations={} items={} parts={} binary_bytes={} missing={}",
        totals.sources,
        totals.conversations,
        totals.items,
        totals.parts,
        totals.binary_bytes,
        totals.missing,
    );
    let code_changes = store.refresh_code_changes(device_id)?;
    println!(
        "code changes: trace_edits={} repositories={} commits={} trace_matched={} metrics={} trace_coverage={:?} git_coverage={:?}",
        code_changes.trace_edits,
        code_changes.repositories,
        code_changes.commits,
        code_changes.matches,
        code_changes.metrics,
        code_changes.trace_coverage,
        code_changes.git_coverage,
    );
    Ok(())
}

/// What one `conversation collect` run imported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ArchiveCollectionTotals {
    sources: u64,
    conversations: u64,
    items: u64,
    parts: u64,
    binary_bytes: u64,
    missing: u64,
}

/// Imports every archive source the filter admits.
fn collect_archive_sources(
    store: &Store,
    canonical_provider_filter: Option<&str>,
    configured_sources: &[SourceLocation],
    no_cache: bool,
    verbose: bool,
) -> Result<ArchiveCollectionTotals> {
    let mut totals = ArchiveCollectionTotals::default();
    for adapter in default_adapters() {
        if canonical_provider_filter.is_some_and(|provider| provider != adapter.provider()) {
            continue;
        }
        for source in scan_sources_for_adapter(adapter.as_ref(), configured_sources) {
            let candidates = adapter.archive_scan_candidates(&source)?;
            let entries = scan_file_state_entries(&candidates);
            // An empty candidate list from an unreachable root — an unmounted
            // volume, a renamed home directory, a `--source` pointing somewhere
            // absent — is not evidence that the archive was emptied.
            // Reconciling it would delete every imported conversation, trace
            // edit, and artifact dependency for the source and retire the
            // derived metrics remotely, so it is reported instead of erased.
            if entries.is_empty() && !adapter.archive_root_available(&source) {
                let imported = store.archive_import_entry_count(&source.source_id)?;
                if imported > 0 {
                    eprintln!(
                        "{} {}: archive root is unavailable while {imported} imported file(s) are on record; keeping them and skipping this source",
                        adapter.provider(),
                        preview_path_label(&source),
                    );
                }
                continue;
            }
            store.reconcile_archive_import_entries(&source.source_id, &entries)?;
            let pending = if no_cache {
                entries
            } else {
                store.pending_archive_import_entries(&source.source_id, &entries)?
            };
            if pending.is_empty() {
                if verbose && !candidates.is_empty() {
                    println!(
                        "{} {}: archive unchanged ({} files)",
                        adapter.provider(),
                        preview_path_label(&source),
                        candidates.len()
                    );
                }
                continue;
            }
            let collected = collect_archive_source_entries(
                store,
                adapter.as_ref(),
                &source,
                &candidates,
                &pending,
                verbose,
            )?;
            totals.sources += 1;
            totals.conversations += collected.conversations;
            totals.items += collected.items;
            totals.parts += collected.parts;
            totals.binary_bytes += collected.binary_bytes;
            totals.missing += collected.missing;
            if verbose {
                println!(
                    "{} {}: files={} conversations={} items={} parts={} binary_bytes={} missing={} invalid_records={}",
                    adapter.provider(),
                    preview_path_label(&source),
                    collected.files,
                    collected.conversations,
                    collected.items,
                    collected.parts,
                    collected.binary_bytes,
                    collected.missing,
                    collected.invalid_records,
                );
            }
        }
    }
    Ok(totals)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ArchiveSourceCollection {
    files: u64,
    conversations: u64,
    items: u64,
    parts: u64,
    binary_bytes: u64,
    missing: u64,
    invalid_records: u64,
}

pub(crate) fn collect_archive_source_entries(
    store: &Store,
    adapter: &dyn ProviderAdapter,
    source: &SourceLocation,
    candidates: &[ScanCandidateFile],
    pending: &[ScanFileStateEntry],
    verbose: bool,
) -> Result<ArchiveSourceCollection> {
    let candidates_by_key = candidates
        .iter()
        .map(|candidate| (candidate.cache_key.as_str(), candidate))
        .collect::<HashMap<_, _>>();
    // Sized once: grouping and the reconstruction budget both ask for this, and
    // a source can hold thousands of files.
    let source_bytes = pending
        .iter()
        .map(|entry| {
            candidates_by_key
                .get(entry.cache_key.as_str())
                .and_then(|candidate| std::fs::metadata(&candidate.path).ok())
                .map_or(0, |metadata| metadata.len())
        })
        .collect::<Vec<_>>();
    let mut collected = ArchiveSourceCollection::default();
    let mut index = 0;
    while index < pending.len() {
        let group = archive_collection_group(&source_bytes, index);
        let group_entries = &pending[index..index + group];

        // Reading and reconstructing a transcript is independent per file and
        // is what the wall clock is mostly spent on, so the group is parsed on
        // several threads. The results are written in file order on this
        // thread: two files can describe the same conversation, and the record
        // that wins must not depend on which thread finished first.
        let collect_started = Instant::now();
        let scans = parse_archive_group(
            adapter,
            source,
            group_entries,
            &source_bytes[index..index + group],
        );
        let collect_elapsed = collect_started.elapsed();
        // Reconstruction stops early when it is already holding enough content,
        // so the run advances by what came back rather than what was offered.
        // Advancing by anything else would step over a file that was never
        // stored, and a run that reconstructed nothing has to say so rather
        // than skip the file or ask for it forever.
        let group = scans.len();
        ensure!(
            group > 0,
            "reconstructed no archive files from {} at file {}",
            preview_path_label(source),
            index + 1,
        );

        let store_started = Instant::now();
        // One transaction per file: a file's rows and the cache entry that
        // records it are committed together, and a file that fails stops the
        // run only after every file before it has been stored.
        for (entry, scan) in group_entries.iter().zip(scans) {
            let scan = scan?;
            let write = store.store_archive_scan_with_code_changes(
                &source.source_id,
                &scan.conversations,
                std::slice::from_ref(entry),
                &scan.artifact_dependencies,
                &scan.trace_edits,
                scan.trace_coverage,
                &scan.quota_observations,
            )?;
            collected.files += scan.diagnostics.files_scanned;
            collected.conversations += write.conversations;
            collected.items += write.items;
            collected.parts += write.content_parts;
            collected.binary_bytes += write.binary_bytes;
            collected.missing += scan.diagnostics.missing_content;
            collected.invalid_records += scan.diagnostics.invalid_records;
        }
        let store_elapsed = store_started.elapsed();
        if verbose {
            println!(
                "{} {}: collected files {}-{}/{} collect={:.1}s store={:.1}s",
                adapter.provider(),
                preview_path_label(source),
                index + 1,
                index + group,
                pending.len(),
                collect_elapsed.as_secs_f64(),
                store_elapsed.as_secs_f64(),
            );
        }
        index += group;
    }
    Ok(collected)
}

/// Files parsed together before their results are written.
pub(crate) const ARCHIVE_COLLECTION_GROUP_FILES: usize = 16;
/// Source bytes a group stops growing at.
///
/// A group is held in memory in full, and one transcript can be tens of
/// megabytes that expand further once reconstructed, so the bound is on the
/// input size rather than the file count alone.
const ARCHIVE_COLLECTION_GROUP_BYTES: u64 = 64 * 1024 * 1024;

/// Number of files to take for the group starting at `index`, always at least
/// one so that a single oversized file still makes progress.
fn archive_collection_group(source_bytes: &[u64], index: usize) -> usize {
    let mut group = 0;
    let mut bytes = 0u64;
    while index + group < source_bytes.len() && group < ARCHIVE_COLLECTION_GROUP_FILES {
        bytes = bytes.saturating_add(source_bytes[index + group]);
        group += 1;
        if bytes >= ARCHIVE_COLLECTION_GROUP_BYTES {
            break;
        }
    }
    group.max(1)
}

/// Reconstructed content a group holds before its results are stored.
///
/// Source size does not predict this: a one-line transcript can name a local
/// artifact of tens of megabytes, which is materialized and carried as base64.
/// Workers therefore stop taking new files once the group has this much
/// outstanding, and the files they did not reach are simply the next group.
pub(crate) const ARCHIVE_COLLECTION_RETAINED_BYTES: usize = 192 * 1024 * 1024;
/// Files reconstructed at once.
///
/// A file's real cost is only known once it has been read: a one-line
/// transcript may name artifacts many times its own size, so no estimate taken
/// beforehand can bound it. What can be bounded is how many files are being
/// read at once, which is what keeps the worst case a small multiple of the
/// largest single file rather than a multiple of the core count.
const ARCHIVE_COLLECTION_IN_FLIGHT: usize = 4;
/// Least a file is charged against the budget while it is being reconstructed.
///
/// Set so that the budget alone admits [`ARCHIVE_COLLECTION_IN_FLIGHT`] files
/// of unknown size, and fewer once one of them is known to be large.
const ARCHIVE_COLLECTION_FILE_RESERVE: usize =
    ARCHIVE_COLLECTION_RETAINED_BYTES / ARCHIVE_COLLECTION_IN_FLIGHT;

/// How much of the budget a group has outstanding, and which file is next.
///
/// Both move together under one lock: a worker that decided to take a file
/// having seen the budget empty, while its peers were deciding the same thing,
/// is how a bound on retained content stops being one.
struct ArchiveGroupClaim {
    next: usize,
    retained: usize,
}

/// Reconstructs a leading run of `entries`, in parallel, preserving their order.
///
/// Returns one result per file reconstructed, which may be fewer than were
/// offered; the caller advances by however many came back. A file that cannot
/// be read is reported in its own position rather than aborting the run, so the
/// caller can still store every file that precedes it. Collection is resumable
/// precisely because a file that was stored stays stored when a later one
/// fails.
pub(crate) fn parse_archive_group(
    adapter: &dyn ProviderAdapter,
    source: &SourceLocation,
    entries: &[ScanFileStateEntry],
    source_bytes: &[u64],
) -> Vec<Result<ArchiveScan>> {
    let collect_one = |entry: &ScanFileStateEntry| {
        let selected = HashSet::from([entry.cache_key.clone()]);
        adapter.collect_archive(source, Some(&selected))
    };
    if entries.len() == 1 {
        return vec![collect_one(&entries[0])];
    }
    let workers = std::thread::available_parallelism()
        .map_or(ARCHIVE_COLLECTION_IN_FLIGHT, std::num::NonZeroUsize::get)
        .min(entries.len())
        .min(ARCHIVE_COLLECTION_IN_FLIGHT);
    let claim = std::sync::Mutex::new(ArchiveGroupClaim {
        next: 0,
        retained: 0,
    });
    let results = (0..entries.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect::<Vec<_>>();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let (index, reserved) = {
                    let mut claim = claim.lock().expect("archive group claim");
                    let index = claim.next;
                    if index >= entries.len() {
                        return;
                    }
                    let reserved = usize::try_from(source_bytes.get(index).copied().unwrap_or(0))
                        .unwrap_or(usize::MAX)
                        .max(ARCHIVE_COLLECTION_FILE_RESERVE);
                    // The first file is always taken, so one file larger than
                    // the whole budget still makes progress.
                    if index > 0
                        && claim.retained.saturating_add(reserved)
                            > ARCHIVE_COLLECTION_RETAINED_BYTES
                    {
                        return;
                    }
                    claim.next = index + 1;
                    claim.retained = claim.retained.saturating_add(reserved);
                    (index, reserved)
                };
                let scan = collect_one(&entries[index]);
                // What the file actually costs replaces what it was charged.
                let actual = scan.as_ref().map_or(0, archive_scan_retained_bytes);
                {
                    let mut claim = claim.lock().expect("archive group claim");
                    claim.retained = claim
                        .retained
                        .saturating_sub(reserved)
                        .saturating_add(actual);
                }
                *results[index].lock().expect("archive scan slot") = Some(scan);
            });
        }
    });
    // Indices are handed out in order and every one handed out is reconstructed,
    // so the results form a leading run. Stopping at the first gap keeps that
    // true whatever the workers did.
    results
        .into_iter()
        .map_while(|slot| slot.into_inner().expect("archive scan slot"))
        .collect()
}

/// Reconstructed content a scan is holding in memory.
fn archive_scan_retained_bytes(scan: &ArchiveScan) -> usize {
    scan.conversations
        .iter()
        .flat_map(|conversation| &conversation.items)
        .flat_map(|item| &item.parts)
        .map(|part| {
            part.text.as_ref().map_or(0, String::len)
                + part.data_base64.as_ref().map_or(0, String::len)
        })
        .sum()
}

pub(crate) fn canonical_conversation_provider_filter(
    provider: Option<&str>,
) -> Result<Option<&'static str>> {
    provider
        .map(|provider| {
            canonical_provider_name(provider).with_context(|| {
                format!(
                    "unknown provider {provider}; available providers: {}",
                    default_adapters()
                        .into_iter()
                        .map(|adapter| adapter.provider())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .transpose()
}

fn print_archive_conversation(conversation: &ArchiveConversation) {
    println!(
        "{} ({})",
        conversation
            .title
            .as_deref()
            .unwrap_or("Untitled conversation"),
        conversation.conversation_id
    );
    println!(
        "provider={} items={} completeness={:?} missing={}",
        conversation.provider,
        conversation.items.len(),
        conversation.completeness,
        conversation.missing_content_count
    );
    for item in &conversation.items {
        println!();
        println!(
            "[{}{}]{}",
            item.role
                .map(|role| format!("{role:?}").to_ascii_lowercase())
                .unwrap_or_else(|| format!("{:?}", item.kind).to_ascii_lowercase()),
            item.created_at
                .map(|value| format!(" {}", value.to_rfc3339()))
                .unwrap_or_default(),
            item.tool_name
                .as_deref()
                .map(|name| format!(" {name}"))
                .unwrap_or_default()
        );
        for part in &item.parts {
            if let Some(text) = part.text.as_deref() {
                println!("{text}");
            } else if part.data_base64.is_some() {
                println!(
                    "[{} {} {} bytes sha256={}]",
                    part.kind.as_str(),
                    part.mime_type
                        .as_deref()
                        .unwrap_or("application/octet-stream"),
                    part.original_bytes,
                    part.content_hash
                );
            } else if let Some(uri) = part.external_uri.as_deref() {
                println!("[missing {} artifact: {uri}]", part.kind.as_str());
            }
        }
    }
}

fn print_archive_markdown(conversation: &ArchiveConversation) {
    println!(
        "# {}\n",
        conversation
            .title
            .as_deref()
            .unwrap_or("Untitled conversation")
    );
    println!("- Provider: `{}`", conversation.provider);
    println!("- Conversation: `{}`", conversation.conversation_id);
    println!("- Completeness: `{:?}`\n", conversation.completeness);
    for item in &conversation.items {
        let label = item
            .role
            .map(|role| format!("{role:?}"))
            .unwrap_or_else(|| format!("{:?}", item.kind));
        println!("## {label}\n");
        for part in &item.parts {
            if let Some(text) = part.text.as_deref() {
                println!("{text}\n");
            } else if let Some(data) = part.data_base64.as_deref() {
                let mime = part
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                if part.kind == ArchiveContentKind::Image {
                    println!(
                        "![{}](data:{};base64,{})\n",
                        part.name.as_deref().unwrap_or("archived image"),
                        mime,
                        data
                    );
                } else {
                    println!(
                        "[Embedded {}: {} bytes, sha256 `{}`]\n",
                        mime, part.original_bytes, part.content_hash
                    );
                }
            } else if let Some(uri) = part.external_uri.as_deref() {
                println!("[Unavailable external artifact]({uri})\n");
            }
        }
    }
}

fn compact_archive_preview(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    compact.chars().take(max_chars).collect::<String>() + "..."
}
