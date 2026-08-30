use anyhow::Result;
use statsai_adapters::default_adapters;
use statsai_store::Store;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::scan::{
    format_cache_key_sample, scan_candidate_compatible_signatures, scan_file_state_entries,
    scan_sources_for_adapter,
};
use crate::{location_origin_label, preview_path_label};

pub(crate) fn status(store: &Store) -> Result<()> {
    println!("stored all-time events: {}", store.event_count()?);
    println!("stored all-time tokens: {}", store.token_total()?);
    println!("stored usage summaries: {}", store.summary_count()?);
    let archive = store.archive_stats()?;
    println!("archived conversations: {}", archive.conversations);
    println!("archived conversation items: {}", archive.items);
    Ok(())
}

pub(crate) fn doctor(store_path: &Path) -> Result<()> {
    println!("store: {}", store_path.display());
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        println!("env CLAUDE_CONFIG_DIR: {}", value);
    }
    if let Ok(value) = std::env::var("CODEX_HOME") {
        println!("env CODEX_HOME: {}", value);
    }
    let store = Store::open(store_path)?;
    let configured = store.list_sources()?;
    for adapter in default_adapters() {
        let sources = scan_sources_for_adapter(adapter.as_ref(), &configured);
        let empty = sources
            .iter()
            .filter(|source| {
                source
                    .path_label
                    .as_deref()
                    .map(|path| !PathBuf::from(path).exists())
                    .unwrap_or(true)
            })
            .count();
        println!(
            "{} sources: {} configured/discovered, {} missing paths",
            adapter.provider(),
            sources.len(),
            empty
        );
        for source in sources {
            let candidates = adapter.scan_candidates(&source)?;
            let compatible_scan_signatures = scan_candidate_compatible_signatures(&candidates);
            let file_cache_entries = scan_file_state_entries(&candidates);
            let pending = store.pending_scan_file_entries_with_compatibility(
                &source.source_id,
                &file_cache_entries,
                &compatible_scan_signatures,
            )?;
            let pending_keys: BTreeSet<_> = pending
                .iter()
                .map(|entry| entry.cache_key.as_str())
                .collect();
            let cached: Vec<_> = candidates
                .iter()
                .filter(|candidate| !pending_keys.contains(candidate.cache_key.as_str()))
                .collect();
            println!(
                "  - {} origin={} files={} pending={} cached={}",
                preview_path_label(&source),
                location_origin_label(&source.location_origin),
                candidates.len(),
                pending.len(),
                cached.len()
            );
            if !pending.is_empty() {
                println!(
                    "    pending sample: {}",
                    format_cache_key_sample(pending.iter().map(|entry| entry.cache_key.as_str()))
                );
            }
            if !cached.is_empty() {
                println!(
                    "    cached sample: {}",
                    format_cache_key_sample(
                        cached.iter().map(|candidate| candidate.cache_key.as_str())
                    )
                );
            }
        }
    }
    println!("status: ok");
    Ok(())
}
