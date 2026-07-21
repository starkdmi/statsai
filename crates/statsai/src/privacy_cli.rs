use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

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

use crate::privacy::{
    dataset_key, hmac_digest, inspect_runtime, load_or_create_pseudonym_key, load_pseudonym_key,
    load_runtime, pseudonym_key_verifier, pseudonym_namespace, runtime_config_path, save_runtime,
    verify_pseudonym_key, PrivacyRuntimeConfig,
};

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

#[derive(Debug, Serialize)]
struct FilterSummary {
    selected: u64,
    filtered: u64,
    unchanged: u64,
    failed: u64,
    unprocessed: u64,
    findings: u64,
    replacements: BTreeMap<String, u64>,
    detector_findings: BTreeMap<String, u64>,
    cross_detector_overlaps: u64,
    detection_passes: u64,
    preview: bool,
}

struct FilterCandidate {
    conversation_id: String,
    input_fingerprint: String,
}

struct ExportRecord {
    metadata: FilteredConversationMetadata,
    day: Option<String>,
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

#[allow(clippy::too_many_arguments)]
fn filter(
    store: &Store,
    store_path: &Path,
    provider: Option<&str>,
    conversation_id: Option<&str>,
    force: bool,
    preview: bool,
    json_output: bool,
    verbose: bool,
) -> Result<()> {
    let provider = canonical_provider(provider)?;
    eprintln!("privacy runtime: verifying configured files");
    let config = load_runtime(store_path)?;
    let metadata = policy_metadata(&config);
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);
    let summaries = if let Some(conversation_id) = conversation_id {
        let conversation = store
            .archive_conversation_for_privacy(conversation_id)?
            .with_context(|| format!("archived conversation not found: {conversation_id}"))?;
        if provider.is_some_and(|provider| provider != conversation.provider) {
            bail!("conversation does not match the provider filter")
        }
        vec![conversation_id.to_string()]
    } else {
        store
            .list_archive_conversations(provider, usize::MAX)?
            .into_iter()
            .map(|summary| summary.conversation_id)
            .collect()
    };
    let mut summary = FilterSummary {
        selected: summaries.len() as u64,
        filtered: 0,
        unchanged: 0,
        failed: 0,
        unprocessed: 0,
        findings: 0,
        replacements: BTreeMap::new(),
        detector_findings: BTreeMap::new(),
        cross_detector_overlaps: 0,
        detection_passes: 0,
        preview,
    };
    if summaries.is_empty() {
        print_filter_summary(&summary, json_output)?;
        return Ok(());
    }
    let key = if preview {
        [0u8; 32]
    } else {
        load_or_initialize_pseudonym_key(store, store_path)?
    };
    let mut candidates = Vec::with_capacity(summaries.len());
    for conversation_id in summaries {
        let conversation = store
            .archive_conversation_for_privacy(&conversation_id)?
            .context("archived conversation disappeared during privacy preflight")?;
        let input_fingerprint = archive_privacy_input_fingerprint(&conversation)?;
        if conversation.completeness != ArchiveCompleteness::Complete {
            summary.failed += 1;
            if !preview {
                store.record_privacy_failure(&PrivacyFailureRecord {
                    conversation_id,
                    input_fingerprint,
                    policy_fingerprint: policy_fingerprint.clone(),
                    failed_stage: "input".to_string(),
                    error_code: "archive_partial".to_string(),
                    attempted_at: Utc::now(),
                })?;
            }
            continue;
        }
        if !force
            && filtered_conversation_is_current(
                store,
                &conversation_id,
                &input_fingerprint,
                &policy_fingerprint,
            )?
        {
            summary.unchanged += 1;
            continue;
        }
        candidates.push(FilterCandidate {
            conversation_id,
            input_fingerprint,
        });
    }
    if candidates.is_empty() {
        return finish_filter_summary(&mut summary, json_output);
    }
    eprintln!("privacy runtime: starting local detectors");
    let mut detectors = match detector_set(&config, verbose) {
        Ok(detectors) => detectors,
        Err(error) => {
            summary.failed += candidates.len() as u64;
            if !preview {
                let error_code = startup_error_code(&error).to_string();
                record_candidate_failures(
                    store,
                    &candidates,
                    &policy_fingerprint,
                    "detector_startup",
                    &error_code,
                )?;
            }
            summary.unprocessed = summary
                .selected
                .saturating_sub(summary.filtered + summary.unchanged + summary.failed);
            print_filter_summary(&summary, json_output)?;
            return Err(error.context("start local privacy detectors"));
        }
    };
    eprintln!("privacy runtime: detectors ready");
    let mut preview_aliases = BTreeMap::<(PrivacyCategory, String), u64>::new();
    let mut preview_counts = BTreeMap::<PrivacyCategory, u64>::new();
    let candidate_count = candidates.len();
    for (index, candidate) in candidates.iter().enumerate() {
        let conversation_id = &candidate.conversation_id;
        let report_progress =
            verbose || candidate_count == 1 || index % 25 == 0 || index + 1 == candidate_count;
        let started = Instant::now();
        if report_progress {
            eprintln!(
                "privacy filtering {}/{}: {}",
                index + 1,
                candidate_count,
                conversation_id
            );
        }
        let conversation = store
            .archive_conversation_for_privacy(conversation_id)?
            .context("archived conversation disappeared during filtering")?;
        if conversation.completeness != ArchiveCompleteness::Complete {
            summary.failed += 1;
            if !preview {
                store.record_privacy_failure(&PrivacyFailureRecord {
                    conversation_id: conversation_id.clone(),
                    input_fingerprint: archive_privacy_input_fingerprint(&conversation)?,
                    policy_fingerprint: policy_fingerprint.clone(),
                    failed_stage: "input".to_string(),
                    error_code: "archive_partial".to_string(),
                    attempted_at: Utc::now(),
                })?;
            }
            if report_progress {
                eprintln!(
                    "privacy failed {}/{} in {:.1}s: archive is partial",
                    index + 1,
                    candidate_count,
                    started.elapsed().as_secs_f64()
                );
            }
            continue;
        }
        let input_fingerprint = archive_privacy_input_fingerprint(&conversation)?;
        let result = if preview {
            filter_archive_conversation(
                &conversation,
                dataset_key(&key, conversation_id),
                &mut detectors,
                |category, value| {
                    let normalized = normalize_private_value(category, value);
                    let digest = hmac_digest(&key, category.as_str(), &normalized);
                    let lookup = (category, digest);
                    if let Some(alias) = preview_aliases.get(&lookup) {
                        return Ok(*alias);
                    }
                    let next = preview_counts.entry(category).or_default();
                    *next += 1;
                    preview_aliases.insert(lookup, *next);
                    Ok(*next)
                },
            )
        } else {
            filter_archive_conversation(
                &conversation,
                dataset_key(&key, conversation_id),
                &mut detectors,
                |category, value| {
                    let normalized = normalize_private_value(category, value);
                    let digest = hmac_digest(&key, category.as_str(), &normalized);
                    store
                        .resolve_privacy_pseudonym(category.as_str(), &digest)
                        .map_err(|_| PrivacyError::PseudonymStore)
                },
            )
        };
        match result {
            Ok(result) => {
                summary.filtered += 1;
                summary.findings += result.findings.len() as u64;
                summary.cross_detector_overlaps +=
                    result.detector_observations.cross_detector_overlaps;
                summary.detection_passes += result.detector_observations.detection_passes;
                for (detector, count) in result.detector_observations.findings_by_detector {
                    *summary
                        .detector_findings
                        .entry(detector.as_str().to_string())
                        .or_default() += count;
                }
                for finding in &result.findings {
                    *summary
                        .replacements
                        .entry(finding.category.as_str().to_string())
                        .or_default() += 1;
                }
                if !preview {
                    let payload = serde_json::to_string(&result.conversation)?;
                    let records = result
                        .findings
                        .into_iter()
                        .map(|finding| PrivacyFindingRecord {
                            field_path: finding.field_path,
                            start: finding.start,
                            end: finding.end,
                            category: finding.category.as_str().to_string(),
                            detector: finding.detector.as_str().to_string(),
                            confidence: finding
                                .confidence
                                .map(|confidence| confidence.as_str().to_string()),
                            replacement: finding.replacement,
                        })
                        .collect::<Vec<_>>();
                    store.write_filtered_conversation(
                        &FilteredConversationRecord {
                            conversation_id: conversation_id.clone(),
                            dataset_key: result.conversation.dataset_key,
                            input_fingerprint: result.input_fingerprint,
                            policy_fingerprint: policy_fingerprint.clone(),
                            payload,
                            finding_count: records.len() as u64,
                            succeeded_at: Utc::now(),
                        },
                        &records,
                    )?;
                }
                if report_progress {
                    eprintln!(
                        "privacy filtered {}/{} in {:.1}s",
                        index + 1,
                        candidate_count,
                        started.elapsed().as_secs_f64()
                    );
                }
            }
            Err(error) => {
                let detector_unavailable = matches!(
                    &error,
                    PrivacyError::Io(_) | PrivacyError::Timeout | PrivacyError::Unavailable
                );
                summary.failed += 1;
                if !preview {
                    store.record_privacy_failure(&PrivacyFailureRecord {
                        conversation_id: conversation_id.clone(),
                        input_fingerprint,
                        policy_fingerprint: policy_fingerprint.clone(),
                        failed_stage: "filter".to_string(),
                        error_code: error.code().to_string(),
                        attempted_at: Utc::now(),
                    })?;
                }
                if report_progress {
                    eprintln!(
                        "privacy failed {}/{} in {:.1}s: {}",
                        index + 1,
                        candidate_count,
                        started.elapsed().as_secs_f64(),
                        error.code()
                    );
                }
                if verbose {
                    eprintln!("privacy detector detail: {error:?}");
                }
                if detector_unavailable {
                    let remaining = &candidates[index + 1..];
                    summary.failed += remaining.len() as u64;
                    if !preview && !remaining.is_empty() {
                        record_candidate_failures(
                            store,
                            remaining,
                            &policy_fingerprint,
                            "filter",
                            error.code(),
                        )?;
                    }
                    break;
                }
            }
        }
    }
    finish_filter_summary(&mut summary, json_output)
}

fn filtered_conversation_is_current(
    store: &Store,
    conversation_id: &str,
    input_fingerprint: &str,
    policy_fingerprint: &str,
) -> Result<bool> {
    let Some(record) = store.filtered_conversation(conversation_id)? else {
        return Ok(false);
    };
    Ok(record.input_fingerprint == input_fingerprint
        && record.policy_fingerprint == policy_fingerprint
        && !store.filtered_conversation_has_newer_failure(conversation_id, record.succeeded_at)?)
}

fn startup_error_code(error: &anyhow::Error) -> &str {
    error
        .downcast_ref::<PrivacyError>()
        .map_or("detector_startup", PrivacyError::code)
}

fn record_candidate_failures(
    store: &Store,
    candidates: &[FilterCandidate],
    policy_fingerprint: &str,
    failed_stage: &str,
    error_code: &str,
) -> Result<()> {
    let attempted_at = Utc::now();
    let failures = candidates
        .iter()
        .map(|candidate| PrivacyFailureRecord {
            conversation_id: candidate.conversation_id.clone(),
            input_fingerprint: candidate.input_fingerprint.clone(),
            policy_fingerprint: policy_fingerprint.to_string(),
            failed_stage: failed_stage.to_string(),
            error_code: error_code.to_string(),
            attempted_at,
        })
        .collect::<Vec<_>>();
    store.record_privacy_failures(&failures)
}

fn finish_filter_summary(summary: &mut FilterSummary, json_output: bool) -> Result<()> {
    summary.unprocessed = summary
        .selected
        .saturating_sub(summary.filtered + summary.unchanged + summary.failed);
    print_filter_summary(summary, json_output)?;
    if summary.failed > 0 {
        bail!(
            "privacy filtering failed closed for {} conversation(s)",
            summary.failed
        )
    }
    Ok(())
}

fn print_filter_summary(summary: &FilterSummary, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(summary)?);
    } else {
        println!(
            "privacy filtering: selected={} filtered={} unchanged={} failed={} unprocessed={} findings={}{}",
            summary.selected,
            summary.filtered,
            summary.unchanged,
            summary.failed,
            summary.unprocessed,
            summary.findings,
            if summary.preview { " preview" } else { "" }
        );
        if !summary.replacements.is_empty() {
            println!(
                "replacements: {}",
                summary
                    .replacements
                    .iter()
                    .map(|(category, count)| format!("{category}={count}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if !summary.detector_findings.is_empty() {
            println!(
                "detector findings before merge: {} overlaps={} passes={}",
                summary
                    .detector_findings
                    .iter()
                    .map(|(detector, count)| format!("{detector}={count}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                summary.cross_detector_overlaps,
                summary.detection_passes,
            );
        }
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

fn export(
    store: &Store,
    store_path: &Path,
    format: &str,
    output: &Path,
    provider: Option<&str>,
) -> Result<()> {
    if format != "jsonl" {
        bail!("unsupported privacy export format: {format}")
    }
    let provider = canonical_provider(provider)?;
    eprintln!("privacy runtime: verifying configured files");
    let config = load_runtime(store_path)?;
    let metadata = policy_metadata(&config);
    let policy_fingerprint = privacy_policy_fingerprint(&metadata);
    let verifier = store.privacy_key_verifier()?;
    let key = load_pseudonym_key(store_path, verifier.as_deref())?
        .context("privacy pseudonym key is unavailable; filter the selected conversations first")?;
    validate_pseudonym_key_state(store, Some(&key))?;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let conversation_count = store.with_read_snapshot(|store| {
        let summaries = store.list_archive_conversations(provider, usize::MAX)?;
        let mut records = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let record = store
                .filtered_conversation_metadata(&summary.conversation_id)?
                .with_context(|| {
                    format!("conversation is not filtered: {}", summary.conversation_id)
                })?;
            let conversation = store
                .archive_conversation_for_privacy(&summary.conversation_id)?
                .context("archived conversation disappeared during export")?;
            if conversation.completeness != ArchiveCompleteness::Complete {
                bail!(
                    "archived conversation is partial: {}",
                    summary.conversation_id
                )
            }
            let input = archive_privacy_input_fingerprint(&conversation)?;
            if record.policy_fingerprint != policy_fingerprint || record.input_fingerprint != input
            {
                bail!(
                    "filtered conversation is stale: {}",
                    summary.conversation_id
                )
            }
            if store.filtered_conversation_has_newer_failure(
                &summary.conversation_id,
                record.succeeded_at,
            )? {
                bail!(
                    "filtered conversation has a newer failed attempt: {}",
                    summary.conversation_id
                )
            }
            records.push(ExportRecord {
                metadata: record,
                day: summary.started_at.or(summary.updated_at).map(|timestamp| {
                    format!(
                        "{:04}-{:02}-{:02}",
                        timestamp.year(),
                        timestamp.month(),
                        timestamp.day()
                    )
                }),
            });
        }
        records.sort_by(|left, right| {
            (&left.day, &left.metadata.dataset_key).cmp(&(&right.day, &right.metadata.dataset_key))
        });
        let manifest = FilteredDatasetManifest {
            schema_version: FILTERED_DATASET_SCHEMA_VERSION.to_string(),
            policy_fingerprint: policy_fingerprint.clone(),
            conversation_schema: FILTERED_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversations: records.len() as u64,
            pseudonym_namespace: pseudonym_namespace(&key),
            detectors: metadata.clone(),
        };
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(&mut writer, &manifest)?;
        writer.write_all(b"\n")?;
        for record in &records {
            let payload = store
                .filtered_conversation_payload(&record.metadata)?
                .with_context(|| {
                    format!(
                        "filtered conversation changed during export: {}",
                        record.metadata.conversation_id
                    )
                })?;
            writer.write_all(payload.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        Ok(records.len())
    })?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    println!(
        "exported {} filtered conversations to {}",
        conversation_count,
        output.display()
    );
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
