use crate::privacy::{
    dataset_key, hmac_digest, inspect_runtime, load_or_create_pseudonym_key, load_pseudonym_key,
    load_runtime, pseudonym_key_verifier, pseudonym_namespace, runtime_config_path, save_runtime,
    verify_pseudonym_key, PrivacyRuntimeConfig,
};
use anyhow::{bail, Context, Result};
use chrono::{Datelike, Utc};
use clap::{Args, Subcommand};
use serde::Serialize;
use statsai_adapters::{adapter_for_provider, default_adapters};
use statsai_core::ArchiveCompleteness;
use statsai_privacy::{
    archive_privacy_input_fingerprint, filter_archive_conversation, normalize_private_value,
    privacy_policy_fingerprint, FilteredDatasetManifest, KingfisherDetector, KingfisherOptions,
    MlxDetector, MlxServerOptions, PrivacyCategory, PrivacyDetector, PrivacyDetectorSet,
    PrivacyError, FILTERED_CONVERSATION_SCHEMA_VERSION, FILTERED_DATASET_SCHEMA_VERSION,
};
use statsai_store::{
    FilteredConversationMetadata, FilteredConversationRecord, PrivacyFailureRecord,
    PrivacyFindingRecord, Store,
};
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

mod export;
mod filter;

pub(crate) use export::*;
pub(crate) use filter::*;

#[derive(Debug, Args)]
pub struct PrivacyCommand {
    #[command(subcommand)]
    command: PrivacySubcommand,
}

#[derive(Debug, Subcommand)]
enum PrivacySubcommand {
    #[command(about = "Register and verify local privacy detector assets")]
    Setup {
        #[arg(long)]
        mlx_server: PathBuf,
        #[arg(long)]
        mlx_model: PathBuf,
        #[arg(long)]
        kingfisher_helper: PathBuf,
        #[arg(long, default_value_t = crate::privacy::DEFAULT_MLX_MEMORY_LIMIT_MIB)]
        mlx_memory_limit_mib: u64,
        #[arg(long, default_value_t = crate::privacy::DEFAULT_MLX_CACHE_LIMIT_MIB)]
        mlx_cache_limit_mib: u64,
        #[arg(long, default_value_t = crate::privacy::DEFAULT_MLX_MAX_BATCH_TOKENS)]
        mlx_max_batch_tokens: usize,
    },
    #[command(about = "Show local privacy runtime and filtered-dataset coverage")]
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Create or preview filtered local conversation records")]
    Filter {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        conversation: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(
            long,
            help = "Run detectors without writing mappings or filtered records"
        )]
        preview: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        verbose: bool,
    },
    #[command(about = "Show one filtered conversation")]
    Show {
        conversation_id: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Export the complete current filtered dataset as deterministic JSONL")]
    Export {
        #[arg(long, default_value = "jsonl")]
        format: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        provider: Option<String>,
    },
}

pub fn run(command: PrivacyCommand, store: &Store, store_path: &Path) -> Result<()> {
    match command.command {
        PrivacySubcommand::Setup {
            mlx_server,
            mlx_model,
            kingfisher_helper,
            mlx_memory_limit_mib,
            mlx_cache_limit_mib,
            mlx_max_batch_tokens,
        } => setup(
            store_path,
            &mlx_server,
            &mlx_model,
            &kingfisher_helper,
            mlx_memory_limit_mib,
            mlx_cache_limit_mib,
            mlx_max_batch_tokens,
        ),
        PrivacySubcommand::Status { json } => status(store, store_path, json),
        PrivacySubcommand::Filter {
            provider,
            conversation,
            force,
            preview,
            json,
            verbose,
        } => filter(
            store,
            store_path,
            provider.as_deref(),
            conversation.as_deref(),
            force,
            preview,
            json,
            verbose,
        ),
        PrivacySubcommand::Show {
            conversation_id,
            json,
        } => show(store, store_path, &conversation_id, json),
        PrivacySubcommand::Export {
            format,
            output,
            provider,
        } => export(store, store_path, &format, &output, provider.as_deref()),
    }
}

fn setup(
    store_path: &Path,
    mlx_server: &Path,
    mlx_model: &Path,
    kingfisher_helper: &Path,
    mlx_memory_limit_mib: u64,
    mlx_cache_limit_mib: u64,
    mlx_max_batch_tokens: usize,
) -> Result<()> {
    eprintln!("privacy setup: inspecting and hashing runtime files");
    let mut config = inspect_runtime(mlx_server, mlx_model, kingfisher_helper)?;
    config.mlx_memory_limit_mib = mlx_memory_limit_mib;
    config.mlx_cache_limit_mib = mlx_cache_limit_mib;
    config.mlx_max_batch_tokens = mlx_max_batch_tokens;
    crate::privacy::validate_runtime_limits(&config)?;
    eprintln!("privacy setup: starting bounded MLX runtime");
    let mut mlx = MlxDetector::spawn(
        &config.mlx_server,
        &config.mlx_model,
        mlx_server_options(&config, false),
        config.model_revision(),
    )?;
    eprintln!("privacy setup: validating Kingfisher helper");
    let kingfisher =
        KingfisherDetector::spawn(&config.kingfisher_helper, KingfisherOptions::default())?;
    eprintln!("privacy setup: running bounded MLX validation");
    mlx.detect("privacy runtime validation")?;
    drop((mlx, kingfisher));
    save_runtime(store_path, &config)?;
    println!(
        "privacy runtime configured: {}",
        runtime_config_path(store_path)?.display()
    );
    Ok(())
}

fn status(store: &Store, store_path: &Path, json_output: bool) -> Result<()> {
    eprintln!("privacy runtime: verifying configured files");
    let config = load_runtime(store_path)?;
    let metadata = policy_metadata(&config);
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);
    let verifier = store.privacy_key_verifier()?;
    let loaded_key = load_pseudonym_key(store_path, verifier.as_deref())?;
    validate_pseudonym_key_state(store, loaded_key.as_ref())?;
    let key_available = loaded_key.is_some();
    let mut status = store.privacy_dataset_status(&policy_fingerprint)?;
    status.current = 0;
    status.stale = 0;
    for summary in store.list_archive_conversations(None, usize::MAX)? {
        let Some(record) = store.filtered_conversation(&summary.conversation_id)? else {
            continue;
        };
        let conversation = store
            .archive_conversation_for_privacy(&summary.conversation_id)?
            .context("archived conversation disappeared during privacy status")?;
        let input = archive_privacy_input_fingerprint(&conversation)?;
        let current = conversation.completeness == ArchiveCompleteness::Complete
            && record.policy_fingerprint == policy_fingerprint
            && record.input_fingerprint == input
            && !store.filtered_conversation_has_newer_failure(
                &record.conversation_id,
                record.succeeded_at,
            )?;
        if current {
            status.current += 1;
        } else {
            status.stale += 1;
        }
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "runtime": config,
                "policy_fingerprint": policy_fingerprint,
                "pseudonym_key_available": key_available,
                "dataset": status,
            }))?
        );
    } else {
        println!("privacy runtime: configured");
        println!(
            "pseudonym key: {}",
            if key_available {
                "available"
            } else {
                "missing"
            }
        );
        println!("archived conversations: {}", status.archived);
        println!("filtered conversations: {}", status.filtered);
        println!("current conversations: {}", status.current);
        println!("stale conversations: {}", status.stale);
        println!("failed conversations: {}", status.failed);
    }
    Ok(())
}

fn show(store: &Store, store_path: &Path, conversation_id: &str, json_output: bool) -> Result<()> {
    let verifier = store.privacy_key_verifier()?;
    let key = load_pseudonym_key(store_path, verifier.as_deref())?
        .context("privacy pseudonym key is unavailable; filtered data cannot be verified")?;
    validate_pseudonym_key_state(store, Some(&key))?;
    let record = store
        .filtered_conversation(conversation_id)?
        .with_context(|| format!("filtered conversation not found: {conversation_id}"))?;
    let payload: serde_json::Value = serde_json::from_str(&record.payload)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Filtered conversation ({})", record.dataset_key);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    Ok(())
}

fn detector_set(config: &PrivacyRuntimeConfig, verbose: bool) -> Result<PrivacyDetectorSet> {
    let mlx = MlxDetector::spawn(
        &config.mlx_server,
        &config.mlx_model,
        mlx_server_options(config, verbose),
        config.model_revision(),
    )?;
    let kingfisher =
        KingfisherDetector::spawn(&config.kingfisher_helper, KingfisherOptions::default())?;
    Ok(PrivacyDetectorSet::new(vec![
        Box::new(mlx),
        Box::new(kingfisher),
    ]))
}

fn policy_metadata(config: &PrivacyRuntimeConfig) -> Vec<statsai_privacy::DetectorMetadata> {
    let mut kingfisher = KingfisherDetector::qualified_metadata();
    kingfisher.implementation_version = format!(
        "{}+binary.{}",
        kingfisher.implementation_version, config.kingfisher_sha256
    );
    vec![
        MlxDetector::metadata_for_revision(config.model_revision()),
        kingfisher,
    ]
}

fn mlx_server_options(config: &PrivacyRuntimeConfig, log_memory_stats: bool) -> MlxServerOptions {
    MlxServerOptions {
        memory_limit_gb: Some(config.mlx_memory_limit_mib as f64 / 1024.0),
        cache_limit_gb: Some(config.mlx_cache_limit_mib as f64 / 1024.0),
        max_batch_tokens: config.mlx_max_batch_tokens,
        log_memory_stats,
        ..MlxServerOptions::default()
    }
}

fn canonical_provider(provider: Option<&str>) -> Result<Option<&'static str>> {
    provider
        .map(|provider| {
            adapter_for_provider(provider)
                .map(|adapter| adapter.provider())
                .with_context(|| {
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

fn load_or_initialize_pseudonym_key(store: &Store, store_path: &Path) -> Result<[u8; 32]> {
    if let Some(verifier) = store.privacy_key_verifier()? {
        let key = load_or_create_pseudonym_key(store_path, true, Some(&verifier))?;
        verify_pseudonym_key(&key, &verifier)?;
        return Ok(key);
    }
    store.with_privacy_identity_initialization(|store| {
        let verifier = store.privacy_key_verifier()?;
        let identity_exists = store.privacy_identity_state_exists()? || verifier.is_some();
        let key = load_or_create_pseudonym_key(store_path, identity_exists, verifier.as_deref())?;
        if let Some(verifier) = verifier {
            verify_pseudonym_key(&key, &verifier)?;
        } else {
            store.ensure_privacy_key_verifier(&pseudonym_key_verifier(&key))?;
        }
        Ok(key)
    })
}

fn validate_pseudonym_key_state(store: &Store, key: Option<&[u8; 32]>) -> Result<()> {
    match (store.privacy_key_verifier()?, key) {
        (Some(verifier), Some(key)) => verify_pseudonym_key(key, &verifier),
        (Some(_), None) => Ok(()),
        (None, _) if store.privacy_identity_state_exists()? => {
            bail!("privacy pseudonym state exists without a key verifier")
        }
        (None, _) => Ok(()),
    }
}
