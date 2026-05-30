use ai_stats_adapters::{
    adapter_for_provider, default_adapters, ProviderAdapter, ScanCandidateFile, ScanDiagnostics,
    ScanOptions,
};
use ai_stats_core::{
    build_usage_report, hash_text, home_dir, provider_account_id, subscription_id, BillingPeriod,
    Confidence, IdentitySource, LocationOrigin, ProviderAccount, ReportPeriod, SourceKind,
    SourceLocation, Subscription, SubscriptionStatus, SyncBatch, UsageEvent, UsageReport,
    UsageSummary, UsageTotals, PROVIDER_ACCOUNT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION,
    SYNC_BATCH_SCHEMA_VERSION,
};
#[cfg(test)]
use ai_stats_core::{
    summary_id, CostInfo, EventSource, PrivacyInfo, PrivacyMode, SummaryMetadata, UsageCounts,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use ai_stats_sdk::{
    build_reported_usage_summary, ReportedUsageSummaryInput, ReportedUsageSummaryRecord,
};
use ai_stats_store::{ScanFileStateEntry, Store, SyncState};
use ai_stats_sync::{
    FileSink, FirestoreSendOptions, FirestoreSink, HttpSink, StdoutSink, SyncSink,
};
use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::Duration;
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

mod auth;

#[derive(Debug, Parser)]
#[command(
    name = "ai-stats",
    version,
    about = "Local-first AI usage stats CLI/SDK/daemon."
)]
struct Cli {
    #[arg(long, global = true, help = "Path to SQLite store")]
    store: Option<PathBuf>,
    #[arg(long, global = true, help = "Device identifier for multi-device sync")]
    device_id: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Scan local provider sources for usage events")]
    Scan(ScanCommand),
    #[command(about = "Show usage reports (weekly, monthly, all-time)")]
    Report(ReportCommand),
    #[command(about = "Manage configured source paths")]
    Source(SourceCommand),
    #[command(about = "Resolve provider accounts from sources")]
    Account(AccountCommand),
    #[command(about = "Manage subscription plans")]
    Subscription(SubscriptionCommand),
    #[command(about = "Import external usage summaries")]
    Import(ImportCommand),
    #[command(about = "Export stored events as JSON")]
    Export(ExportCommand),
    #[command(about = "Export a sync batch to a sink")]
    Sync(SyncCommand),
    #[command(about = "Print JSON schemas for backend-facing contracts")]
    Schema(SchemaCommand),
    #[command(about = "Start the loopback API daemon")]
    Daemon(DaemonCommand),
    #[command(about = "Show stored event and token counts")]
    Status,
    #[command(about = "Check environment and source paths")]
    Doctor,
    #[command(about = "Authenticate with Firebase production backend")]
    Auth(AuthCommand),
}

#[derive(Debug, Args)]
struct AuthCommand {
    #[command(subcommand)]
    command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
    #[command(about = "Log in with Google to get production credentials")]
    Login {
        #[arg(long, help = "Optional Google OAuth Client ID")]
        client_id: Option<String>,
    },
    #[command(about = "Check authentication status")]
    Status,
    #[command(about = "Log out and clear stored credentials")]
    Logout,
}

#[derive(Debug, Args)]
struct ScanCommand {
    #[arg(long, help = "Scan only this provider (claude, codex)")]
    provider: Option<String>,
    #[arg(long, help = "Preview without persisting to the store")]
    preview: bool,
    #[arg(
        long,
        help = "Ignore the scan file cache and reparse all candidate files"
    )]
    no_cache: bool,
    #[arg(
        long,
        help = "Replace existing events for scanned sources before inserting"
    )]
    replace: bool,
    #[arg(long, help = "Show detailed per-source diagnostics")]
    verbose: bool,
    #[arg(long, help = "Show parse evidence for each event")]
    explain: bool,
}

#[derive(Debug, Args)]
struct ReportCommand {
    #[command(subcommand)]
    command: ReportSubcommand,
}

#[derive(Debug, Subcommand)]
enum ReportSubcommand {
    #[command(about = "Show usage for the last 7 days")]
    Weekly {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
    },
    #[command(about = "Show usage for the last 30 days")]
    Monthly {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
    },
    #[command(about = "Show all stored usage")]
    AllTime {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
    },
}

#[derive(Debug, Args)]
struct SourceCommand {
    #[command(subcommand)]
    command: SourceSubcommand,
}

#[derive(Debug, Subcommand)]
enum SourceSubcommand {
    #[command(about = "Register a manual source path for a provider")]
    Add {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Path to the provider's local data directory")]
        path: PathBuf,
        #[arg(long, help = "Account label to attach to events from this source")]
        account: Option<String>,
    },
    #[command(about = "Enable a configured source")]
    Enable {
        #[arg(long, help = "Source identifier to enable")]
        source_id: String,
    },
    #[command(about = "Disable a configured source")]
    Disable {
        #[arg(long, help = "Source identifier to disable")]
        source_id: String,
    },
    #[command(about = "Remove a configured source")]
    Remove {
        #[arg(long, help = "Source identifier to remove")]
        source_id: String,
        #[arg(
            long,
            help = "Delete local events, summaries, rollups, and scan cache for this source"
        )]
        delete_data: bool,
    },
    #[command(about = "List all configured sources")]
    List,
}

#[derive(Debug, Args)]
struct AccountCommand {
    #[command(subcommand)]
    command: AccountSubcommand,
}

#[derive(Debug, Subcommand)]
enum AccountSubcommand {
    #[command(about = "Resolve provider accounts from configured sources")]
    Resolve {
        #[arg(long, help = "Provider to resolve accounts for (claude_code, codex)")]
        provider: String,
    },
}

#[derive(Debug, Args)]
struct SubscriptionCommand {
    #[command(subcommand)]
    command: SubscriptionSubcommand,
}

#[derive(Debug, Subcommand)]
enum SubscriptionSubcommand {
    #[command(about = "Register a subscription plan")]
    Add {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Account label to link this subscription to")]
        account: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: String,
        #[arg(long, help = "Monthly price in the given currency")]
        price: f64,
        #[arg(long, default_value = "USD", help = "Currency code")]
        currency: String,
        #[arg(long, help = "Date the subscription was paid (YYYY-MM-DD or RFC 3339)")]
        paid_at: Option<String>,
    },
    #[command(about = "List all registered subscriptions")]
    List,
}

#[derive(Debug, Args)]
struct ImportCommand {
    #[command(subcommand)]
    command: ImportSubcommand,
}

#[derive(Debug, Subcommand)]
enum ImportSubcommand {
    #[command(about = "Import a reported usage summary JSON file")]
    Summary {
        #[arg(long, help = "Path to reported_usage_summary_input JSON file")]
        path: PathBuf,
        #[arg(long, help = "Replace existing matching summaries before import")]
        replace: bool,
        #[arg(long, help = "Preview without persisting")]
        dry_run: bool,
        #[arg(long, help = "Show per-file import details")]
        verbose: bool,
    },
}

#[derive(Debug, Args)]
struct ExportCommand {
    #[arg(long, help = "Export all events as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct SyncCommand {
    #[arg(
        long,
        default_value = "stdout",
        help = "Sync sink (stdout, file, http, firestore)"
    )]
    sink: String,
    #[arg(long, help = "Output path for file sink")]
    output: Option<PathBuf>,
    #[arg(long, help = "HTTP endpoint for the http sink")]
    endpoint: Option<String>,
    #[arg(
        long,
        help = "Bearer token override for the http sink or Firestore sync"
    )]
    auth_token: Option<String>,
    #[arg(
        long,
        help = "Explicit Firebase UID namespace for Firestore sync when using a manual token"
    )]
    firebase_uid: Option<String>,
    #[arg(
        long,
        default_value = "ai-stats-fire",
        help = "Firebase project ID for the firestore sink"
    )]
    firebase_project: String,
    #[arg(
        long,
        default_value = "stats",
        help = "Firestore payload mode (stats, full [emulator/debug only])"
    )]
    firestore_mode: String,
    #[arg(
        long,
        help = "Rebuild local Firestore rollups from events and force all rollups dirty before sync"
    )]
    rebuild_rollups: bool,
    #[arg(
        long,
        default_value_t = 1000,
        help = "Max event+summary records per firestore sub-batch"
    )]
    firestore_records_per_batch: usize,
    #[arg(
        long,
        default_value_t = 200,
        help = "Max document writes per Firestore commit request (1..450)"
    )]
    firestore_commit_writes: usize,
    #[arg(
        long,
        default_value_t = 4,
        help = "Retry attempts for retryable Firestore errors"
    )]
    firestore_retries: u32,
    #[arg(
        long,
        default_value_t = 800,
        help = "Initial backoff in milliseconds for Firestore retries"
    )]
    firestore_backoff_ms: u64,
    #[arg(
        long,
        help = "Send only records after this sink target's last successful sync"
    )]
    since_last: bool,
    #[arg(long, help = "Show recorded sync state instead of sending")]
    status: bool,
    #[arg(
        long,
        help = "Inspect the resolved target and verify remote firestore access"
    )]
    verify: bool,
    #[arg(long, help = "Build the sync batch without writing")]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SchemaCommand {
    #[command(subcommand)]
    command: SchemaSubcommand,
}

#[derive(Debug, Subcommand)]
enum SchemaSubcommand {
    #[command(about = "Print the sync_batch.v1 JSON Schema")]
    SyncBatch,
}

#[derive(Debug, Args)]
struct DaemonCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:8765",
        help = "Loopback address to bind the API"
    )]
    api: String,
    #[arg(long, help = "Enable file watching for automatic rescans")]
    watch: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store_path = cli.store.unwrap_or_else(default_store_path);
    let device_id = cli.device_id.unwrap_or_else(default_device_id);

    match cli.command {
        Command::Schema(command) => schema(command),
        Command::Doctor => doctor(&store_path),
        Command::Auth(command) => auth(command),
        command => {
            let store = Store::open(&store_path)?;
            match command {
                Command::Scan(command) => scan(command, &store, &device_id),
                Command::Report(command) => report(command, &store),
                Command::Source(command) => source(command, &store),
                Command::Account(command) => account(command, &store),
                Command::Subscription(command) => subscription(command, &store),
                Command::Import(command) => import(command, &store, &device_id),
                Command::Export(command) => export(command, &store),
                Command::Sync(command) => sync(command, &store, &device_id),
                Command::Daemon(command) => daemon(command, store, &device_id),
                Command::Status => status(&store),
                Command::Schema(_) | Command::Doctor | Command::Auth(_) => {
                    unreachable!("handled before store open")
                }
            }
        }
    }
}

fn auth(command: AuthCommand) -> Result<()> {
    match command.command {
        AuthSubcommand::Login { client_id } => auth::login(client_id),
        AuthSubcommand::Status => auth::status(),
        AuthSubcommand::Logout => auth::logout(),
    }
}

fn scan(command: ScanCommand, store: &Store, device_id: &str) -> Result<()> {
    let adapters: Vec<Box<dyn ProviderAdapter>> =
        if let Some(provider) = command.provider.as_deref() {
            vec![adapter_for_provider(provider)
                .with_context(|| format!("unsupported provider {provider}"))?]
        } else {
            default_adapters()
        };

    let mut event_count = 0u64;
    let mut summary_count = 0u64;
    let mut inserted_count = 0u64;
    let mut summary_written_count = 0u64;
    let mut replaced_event_count = 0u64;
    let mut replaced_summary_count = 0u64;
    let mut total_sources = 0u64;
    let mut total_log_rows = 0u64;
    let mut total_diagnostics = ScanDiagnostics::default();
    let mut total_usage = UsageTotals::default();
    let mut total_summary_usage = UsageTotals::default();

    let configured_sources = store.list_sources()?;

    for adapter in adapters {
        let sources = scan_sources_for_adapter(adapter.as_ref(), &configured_sources);

        for mut source in sources {
            if source.path_label.is_none() {
                source.path_label = path_label_from_hashless_source(&source);
            }
            let cache_candidates = adapter.scan_candidates(&source)?;
            let file_cache_entries = scan_file_state_entries(&cache_candidates);
            let pending_file_entries = select_scan_file_entries(
                store,
                &source.source_id,
                &file_cache_entries,
                command.replace,
                command.no_cache,
            )?;
            let options = ScanOptions {
                device_id: device_id.to_string(),
                selected_cache_keys: (!command.no_cache).then(|| {
                    pending_file_entries
                        .iter()
                        .map(|entry| entry.cache_key.clone())
                        .collect()
                }),
            };
            let mut scan = adapter.scan(&source, &options)?;
            apply_account_hint_to_events(&source, &mut scan.events);
            apply_account_hint_to_summaries(&source, &mut scan.summaries);
            let log_rows = scan.diagnostics.raw_rows;
            let mut source_usage = UsageTotals::default();
            for event in &scan.events {
                source_usage.add_event(event);
            }
            let mut source_summary_usage = UsageTotals::default();
            for summary in &scan.summaries {
                source_summary_usage.add_summary(summary);
            }
            let source_event_count = scan.events.len() as u64;
            let source_summary_count = scan.summaries.len() as u64;
            let touched_files = !pending_file_entries.is_empty();
            let has_scan_activity = source_event_count > 0
                || source_summary_count > 0
                || scan.diagnostics.files_scanned > 0
                || scan.diagnostics.files_skipped_unchanged > 0
                || log_rows > 0;
            let suppress_source_processing = !command.verbose
                && !command.explain
                && source_event_count == 0
                && source_summary_count == 0
                && !touched_files;

            if !has_scan_activity {
                continue;
            }

            total_sources += 1;
            total_log_rows += log_rows;
            event_count += source_event_count;
            summary_count += source_summary_count;
            total_usage.add_totals(&source_usage);
            total_summary_usage.add_totals(&source_summary_usage);
            add_diagnostics(&mut total_diagnostics, &scan.diagnostics);

            if suppress_source_processing {
                continue;
            }

            if command.preview {
                print_scan_preview_line(
                    &source,
                    source_event_count,
                    &source_usage,
                    source_summary_count,
                    &source_summary_usage,
                    &scan.diagnostics,
                    command.verbose || command.explain,
                );
                continue;
            }
            persist_source_after_preview(store, &source)?;
            if command.replace {
                replaced_event_count +=
                    store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
                replaced_summary_count +=
                    store.delete_summaries_for_sources(std::slice::from_ref(&source.source_id))?;
            }
            inserted_count += store.insert_events(&scan.events)?;
            summary_written_count += store.upsert_summaries(&scan.summaries)?;
            let cache_entries_to_record = if command.replace || command.no_cache {
                &file_cache_entries
            } else {
                &pending_file_entries
            };
            store.record_scan_file_entries(&source.source_id, cache_entries_to_record)?;
        }
    }

    if command.preview {
        if command.verbose {
            println!(
                "preview total: sources={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} log_rows={} written=0",
                format_u64(total_sources),
                format_u64(event_count),
                format_u64(summary_count),
                format_u64(total_usage.input_tokens),
                format_u64(total_usage.cache_creation_tokens),
                format_u64(total_usage.cached_input_tokens),
                format_u64(total_usage.output_tokens),
                format_u64(total_usage.total_tokens),
                format_cost(total_usage.estimated_cost_usd),
                format_u64(total_summary_usage.total_tokens),
                format_u64(total_log_rows)
            );
            print_scan_diagnostics_total(&total_diagnostics);
        } else {
            println!(
                "preview total: sources={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} written=0",
                format_u64(total_sources),
                format_u64(event_count),
                format_u64(summary_count),
                format_u64(total_usage.input_tokens),
                format_u64(total_usage.cache_creation_tokens),
                format_u64(total_usage.cached_input_tokens),
                format_u64(total_usage.output_tokens),
                format_u64(total_usage.total_tokens),
                format_cost(total_usage.estimated_cost_usd),
                format_u64(total_summary_usage.total_tokens)
            );
        }
    } else {
        println!(
            "scan complete: sources={} usage_events={} inserted={} summaries={} summaries_written={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} log_rows={}",
            format_u64(total_sources),
            format_u64(event_count),
            format_u64(inserted_count),
            format_u64(summary_count),
            format_u64(summary_written_count),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_cost(total_usage.estimated_cost_usd),
            format_u64(total_summary_usage.total_tokens),
            format_u64(total_log_rows)
        );
        if command.replace {
            println!(
                "replace removed: events={} summaries={}",
                format_u64(replaced_event_count),
                format_u64(replaced_summary_count)
            );
        }
        print_scan_diagnostics_total(&total_diagnostics);
    }
    Ok(())
}

fn source(command: SourceCommand, store: &Store) -> Result<()> {
    match command.command {
        SourceSubcommand::Add {
            provider,
            path,
            account,
        } => {
            let adapter = adapter_for_provider(&provider)
                .with_context(|| format!("unsupported provider {provider}"))?;
            let path = normalize_configured_source_path(adapter.provider(), &path)?;
            let mut source = SourceLocation::local_adapter(
                adapter.provider(),
                adapter.id(),
                adapter.version(),
                &path,
                LocationOrigin::Configured,
                account,
            );
            source.path_label = Some(path.to_string_lossy().to_string());
            store.upsert_source(&source)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Enable { source_id } => {
            let source_id = ai_stats_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, true)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Disable { source_id } => {
            let source_id = ai_stats_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, false)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Remove {
            source_id,
            delete_data,
        } => {
            let source_id = ai_stats_core::SourceId(source_id);
            let source = store
                .source(&source_id)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            let deleted_events = if delete_data {
                store.delete_events_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_summaries = if delete_data {
                store.delete_summaries_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted_scan_entries = if delete_data {
                store.delete_scan_file_entries_for_sources(std::slice::from_ref(&source_id))?
            } else {
                0
            };
            let deleted = store.delete_source(&source_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "source_id": source_id.0,
                    "deleted": deleted,
                    "delete_data": delete_data,
                    "deleted_events": deleted_events,
                    "deleted_summaries": deleted_summaries,
                    "deleted_scan_cache_entries": deleted_scan_entries,
                    "source": source
                }))?
            );
        }
        SourceSubcommand::List => {
            println!("{}", serde_json::to_string_pretty(&store.list_sources()?)?);
        }
    }
    Ok(())
}

fn account(command: AccountCommand, store: &Store) -> Result<()> {
    match command.command {
        AccountSubcommand::Resolve { provider } => {
            let provider = canonical_provider(&provider)?;
            let sources: Vec<_> = store
                .list_sources()?
                .into_iter()
                .filter(|source| provider_matches(&source.provider, &provider))
                .collect();
            if sources.is_empty() {
                println!("no configured sources for {provider}");
                return Ok(());
            }
            let mut accounts: BTreeMap<String, ProviderAccount> = BTreeMap::new();
            for source in sources {
                let stable = source
                    .account_hint
                    .as_deref()
                    .unwrap_or(&source.source_id.0);
                let id = provider_account_id(&source.provider, stable);
                if let Some(account) = accounts.get_mut(&id.0) {
                    account.source_ids.push(source.source_id.clone());
                    account
                        .source_ids
                        .sort_by(|left, right| left.0.cmp(&right.0));
                    account.source_ids.dedup();
                    account.updated_at = Utc::now();
                    continue;
                }
                let account = ProviderAccount {
                    schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
                    provider_account_id: id.clone(),
                    provider: source.provider.clone(),
                    identity_source: if source.account_hint.is_some() {
                        IdentitySource::UserConfigured
                    } else {
                        IdentitySource::Unknown
                    },
                    provider_user_id_hash: None,
                    email_hash: None,
                    org_id_hash: None,
                    account_label: source.account_hint.clone(),
                    plan_name: None,
                    confidence: if source.account_hint.is_some() {
                        Confidence::Medium
                    } else {
                        Confidence::Low
                    },
                    source_ids: vec![source.source_id.clone()],
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };
                accounts.insert(id.0, account);
            }
            for account in accounts.into_values() {
                store.upsert_account(&account)?;
                println!("{}", serde_json::to_string_pretty(&account)?);
            }
        }
    }
    Ok(())
}

fn subscription(command: SubscriptionCommand, store: &Store) -> Result<()> {
    match command.command {
        SubscriptionSubcommand::Add {
            provider,
            account,
            plan,
            price,
            currency,
            paid_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let account_id = account
                .as_deref()
                .map(|label| provider_account_id(&provider, label));
            let paid_at = paid_at.as_deref().map(parse_date).transpose()?;
            let subscription = Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(&provider, account_id.as_ref(), &plan),
                provider,
                provider_account_id: account_id,
                source_ids: Vec::new(),
                plan_name: plan,
                price,
                currency,
                billing_period: BillingPeriod::Monthly,
                paid_at,
                renewal_day: None,
                started_at: paid_at,
                ended_at: None,
                status: SubscriptionStatus::Active,
                notes: None,
            };
            store.upsert_subscription(&subscription)?;
            println!("{}", serde_json::to_string_pretty(&subscription)?);
        }
        SubscriptionSubcommand::List => println!(
            "{}",
            serde_json::to_string_pretty(&store.list_subscriptions()?)?
        ),
    }
    Ok(())
}

fn import(command: ImportCommand, store: &Store, device_id: &str) -> Result<()> {
    match command.command {
        ImportSubcommand::Summary {
            path,
            replace,
            dry_run,
            verbose,
        } => {
            let inputs = read_reported_summary_inputs(&path)?;
            let records = inputs
                .into_iter()
                .map(|input| build_reported_usage_summary(input, device_id))
                .collect::<Result<Vec<_>>>()?;
            import_reported_summary_records(
                store,
                &[ReportedImportReport {
                    path,
                    records,
                    warnings: Vec::new(),
                }],
                dry_run,
                verbose,
                replace,
            )?;
        }
    }
    Ok(())
}

fn import_reported_summary_records(
    store: &Store,
    reports: &[ReportedImportReport],
    dry_run: bool,
    verbose: bool,
    replace: bool,
) -> Result<()> {
    let total_summaries: usize = reports.iter().map(|report| report.records.len()).sum();
    let mut total_usage = UsageTotals::default();
    for report in reports {
        for record in &report.records {
            total_usage.add_summary(&record.summary);
        }
    }

    if verbose || dry_run {
        for report in reports {
            println!(
                "reported source path={} summaries={} warnings={}",
                abbreviate_home(report.path.to_string_lossy().as_ref()),
                report.records.len(),
                report.warnings.len()
            );
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
    }

    if dry_run {
        let replace_count = if replace {
            matching_reported_summary_ids(store, reports)?.len() as u64
        } else {
            0
        };
        println!(
            "import preview: sources={} summaries={} replace_existing={} input={} cache_create={} cache_read={} output={} total={} cost={} written=0",
            format_u64(reports.len() as u64),
            format_u64(total_summaries as u64),
            format_u64(replace_count),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_cost(total_usage.estimated_cost_usd)
        );
        return Ok(());
    }

    let replaced = if replace {
        let summary_ids = matching_reported_summary_ids(store, reports)?;
        store.delete_summaries(&summary_ids)?
    } else {
        0
    };
    let mut written = 0u64;
    for report in reports {
        for record in &report.records {
            store.upsert_source(&record.source)?;
            written += store.upsert_summaries(std::slice::from_ref(&record.summary))?;
        }
    }
    println!(
        "import complete: sources={} summaries={} replaced={} summaries_written={} input={} cache_create={} cache_read={} output={} total={} cost={}",
        format_u64(reports.len() as u64),
        format_u64(total_summaries as u64),
        format_u64(replaced),
        format_u64(written),
        format_u64(total_usage.input_tokens),
        format_u64(total_usage.cache_creation_tokens),
        format_u64(total_usage.cached_input_tokens),
        format_u64(total_usage.output_tokens),
        format_u64(total_usage.total_tokens),
        format_cost(total_usage.estimated_cost_usd)
    );
    Ok(())
}

fn matching_reported_summary_ids(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<Vec<ai_stats_core::SummaryId>> {
    let incoming_keys: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .map(reported_replace_key)
        .collect();

    let summary_ids = store
        .summaries()?
        .into_iter()
        .filter(|summary| {
            matches!(
                summary.source.source_kind,
                SourceKind::ExternalReport | SourceKind::Manual
            )
        })
        .filter(|summary| incoming_keys.contains(&reported_replace_key_from_summary(summary)))
        .map(|summary| summary.summary_id)
        .collect();
    Ok(summary_ids)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReportedReplaceKey {
    provider: String,
    provider_account_id: Option<String>,
    summary_format: String,
    source_id: String,
    period_start: Option<DateTime<Utc>>,
    period_end: Option<DateTime<Utc>>,
    source_record_id: Option<String>,
}

fn reported_replace_key(record: &ReportedUsageSummaryRecord) -> ReportedReplaceKey {
    reported_replace_key_from_summary(&record.summary)
}

fn reported_replace_key_from_summary(summary: &UsageSummary) -> ReportedReplaceKey {
    ReportedReplaceKey {
        provider: summary.provider.clone(),
        provider_account_id: summary.provider_account_id.as_ref().map(|id| id.0.clone()),
        summary_format: summary.metadata.summary_format.clone(),
        source_id: summary.source_id.0.clone(),
        period_start: summary.period_start,
        period_end: summary.period_end,
        source_record_id: stable_reported_record_id(summary),
    }
}

fn stable_reported_record_id(summary: &UsageSummary) -> Option<String> {
    summary
        .source
        .source_record_id
        .as_deref()
        .filter(|record_id| !record_id.starts_with("summary_key_"))
        .map(ToOwned::to_owned)
}

fn report(command: ReportCommand, store: &Store) -> Result<()> {
    let (period, json_output, verbose) = match command.command {
        ReportSubcommand::Weekly { json, verbose, .. } => {
            (ReportPeriod::LastDays(7), json, verbose)
        }
        ReportSubcommand::Monthly { json, verbose, .. } => {
            (ReportPeriod::LastDays(30), json, verbose)
        }
        ReportSubcommand::AllTime { json, verbose } => (ReportPeriod::AllTime, json, verbose),
    };
    let report = build_usage_report(
        &store.events()?,
        &store.summaries()?,
        &store.list_sources()?,
        &store.list_accounts()?,
        period,
        Utc::now(),
    );
    if json_output {
        print_report_json(&report, verbose)?;
    } else {
        print_report_table(&report, verbose);
    }
    Ok(())
}

fn export(command: ExportCommand, store: &Store) -> Result<()> {
    if !command.json {
        bail!("only --json export is supported");
    }
    println!("{}", serde_json::to_string_pretty(&store.events()?)?);
    Ok(())
}

fn sync(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    if command.verify {
        return sync_verify(command, store, device_id);
    }

    if command.status {
        return sync_status(store);
    }

    if hosted_firestore_full_mode_disabled(&command) {
        bail!(
            "hosted Firestore full mode is disabled; use --firestore-mode stats for production sync, the emulator for raw event testing, or set AI_STATS_ENABLE_HOSTED_FIRESTORE_FULL=1 to bypass this guardrail"
        );
    }

    let firestore_auth = if command.sink == "firestore" {
        Some(resolve_firestore_auth_context(&command)?)
    } else {
        None
    };
    let target = firestore_auth
        .as_ref()
        .map(|auth| firestore_sync_target(&command.firebase_project, &auth.uid))
        .unwrap_or_else(|| sync_target(&command));
    let firestore_stats_mode =
        command.sink == "firestore" && command.firestore_mode.eq_ignore_ascii_case("stats");
    let state = if command.since_last {
        store.sync_state(&command.sink, &target)?
    } else {
        None
    };
    let event_cursor = if firestore_stats_mode {
        None
    } else {
        state.as_ref().and_then(|state| {
            state
                .last_event_started_at
                .as_ref()
                .zip(state.last_event_id.as_deref())
        })
    };
    let summary_cursor = state.as_ref().and_then(|state| {
        state
            .last_summary_observed_at
            .as_ref()
            .zip(state.last_summary_id.as_deref())
    });
    let events: Vec<_> = if firestore_stats_mode {
        Vec::new()
    } else {
        store
            .events_after(event_cursor)?
            .into_iter()
            .map(sanitize_event_for_sync)
            .collect()
    };
    let summaries: Vec<_> = if firestore_stats_mode {
        let passthrough_summaries: Vec<_> = store
            .summaries()?
            .into_iter()
            .map(sanitize_summary_for_sync)
            .filter(|summary| !is_daily_rollup_summary(summary))
            .collect();
        store.pending_summaries_for_sync(&command.sink, &target, &passthrough_summaries)?
    } else {
        store
            .summaries_after(summary_cursor)?
            .into_iter()
            .map(sanitize_summary_for_sync)
            .collect()
    };
    let all_sources: Vec<_> = store
        .list_sources()?
        .into_iter()
        .map(sanitize_source_for_sync)
        .collect();
    let all_accounts: Vec<_> = store
        .list_accounts()?
        .into_iter()
        .map(sanitize_account_for_sync)
        .collect();
    let all_subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .map(sanitize_subscription_for_sync)
        .collect();
    let sources = if command.sink == "firestore" {
        store.pending_sources_for_sync(&command.sink, &target, &all_sources)?
    } else {
        all_sources
    };
    let accounts = if command.sink == "firestore" {
        store.pending_accounts_for_sync(&command.sink, &target, &all_accounts)?
    } else {
        all_accounts
    };
    let subscriptions = if command.sink == "firestore" {
        store.pending_subscriptions_for_sync(&command.sink, &target, &all_subscriptions)?
    } else {
        all_subscriptions
    };
    let mut batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: format!("batch_{}", Utc::now().timestamp_millis()),
        device_id: device_id.to_string(),
        sources,
        accounts,
        subscriptions,
        events,
        summaries,
        created_at: Utc::now(),
    };

    if firestore_stats_mode {
        let should_bootstrap =
            !command.dry_run && store.firestore_rollup_count()? == 0 && store.event_count()? > 0;
        if !command.dry_run && command.rebuild_rollups {
            let rebuilt = store.rebuild_firestore_rollups()?;
            let marked_dirty = store.mark_all_firestore_rollups_dirty()?;
            eprintln!(
                "firestore stats mode: rebuilt {} local daily summaries and marked {} dirty for full sync",
                rebuilt, marked_dirty
            );
        } else if should_bootstrap {
            let rebuilt = store.rebuild_firestore_rollups()?;
            eprintln!(
                "firestore stats mode: bootstrapped {} local daily summaries from existing events",
                rebuilt
            );
        }

        let rollups = store.dirty_firestore_rollup_summaries()?;
        eprintln!(
            "firestore stats mode: prepared {} local daily summaries for sync",
            rollups.len()
        );
        batch
            .summaries
            .extend(rollups.into_iter().map(sanitize_summary_for_sync));
    } else if command.sink == "firestore" {
        let mode = command.firestore_mode.to_ascii_lowercase();
        match mode.as_str() {
            "full" => {}
            other => bail!("unsupported firestore mode {other}; expected stats or full"),
        }
    }

    if command.dry_run {
        eprintln!(
            "dry run: sink={} sources={} events={} summaries={}",
            command.sink,
            batch.sources.len(),
            batch.events.len(),
            batch.summaries.len()
        );
        return Ok(());
    }

    if command.sink == "firestore" {
        let auth = firestore_auth.context("missing Firestore auth context")?;
        return sync_firestore(command, store, device_id, target, batch, auth);
    }

    let result = match command.sink.as_str() {
        "stdout" => StdoutSink.send(&batch).map(|()| None),
        "file" => {
            let output = command
                .output
                .unwrap_or_else(|| PathBuf::from("ai-stats-sync-batch.json"));
            FileSink::new(output).send(&batch).map(|()| None)
        }
        "http" => {
            let send_http = || -> Result<_> {
                let endpoint = command
                    .endpoint
                    .as_deref()
                    .context("--endpoint is required when --sink http")?;
                let auth_token = command
                    .auth_token
                    .or_else(|| std::env::var("AI_STATS_SYNC_TOKEN").ok())
                    .or_else(|| auth::get_or_refresh_token().ok().flatten());
                let ack = HttpSink::new(endpoint, auth_token)?.send_with_ack(&batch)?;
                println!("{}", serde_json::to_string_pretty(&ack)?);
                Ok(Some(ack))
            };
            send_http()
        }
        other => bail!("unsupported sync sink {other}"),
    };

    match result {
        Ok(_) => {
            store.record_sync_success(
                &command.sink,
                &target,
                &batch.batch_id,
                &batch.events,
                &batch.summaries,
            )?;
            Ok(())
        }
        Err(error) => {
            let _ = store.record_sync_failure(&command.sink, &target);
            Err(error)
        }
    }
}

fn sync_firestore(
    command: SyncCommand,
    store: &Store,
    device_id: &str,
    target: String,
    batch: SyncBatch,
    auth: FirestoreAuthContext,
) -> Result<()> {
    let sink = FirestoreSink::new(&command.firebase_project, auth.uid, auth.auth_token);
    let options = FirestoreSendOptions {
        commit_chunk_size: command.firestore_commit_writes.clamp(1, 450),
        max_retries: command.firestore_retries,
        initial_backoff: StdDuration::from_millis(command.firestore_backoff_ms.max(1)),
        progress: true,
    };

    if batch.sources.is_empty()
        && batch.accounts.is_empty()
        && batch.subscriptions.is_empty()
        && batch.events.is_empty()
        && batch.summaries.is_empty()
    {
        eprintln!("firestore sync: nothing to send");
        return Ok(());
    }

    let records_per_batch = command.firestore_records_per_batch.max(1);
    let total_records = batch.events.len() + batch.summaries.len();
    let total_writes =
        total_records + batch.sources.len() + batch.accounts.len() + batch.subscriptions.len() + 2;
    let sub_batches = if total_records == 0 {
        1
    } else {
        total_records.div_ceil(records_per_batch)
    };

    eprintln!(
        "firestore sync: {} event/summary records, {} estimated writes, {} sub-batches (commit writes <= {})",
        total_records, total_writes, sub_batches, options.commit_chunk_size
    );

    let mut sent_events = 0usize;
    let mut sent_summaries = 0usize;
    let mut include_metadata = true;
    let mut aggregate_ack = ai_stats_core::SyncAck {
        schema_version: ai_stats_core::SYNC_ACK_SCHEMA_VERSION.to_string(),
        batch_id: batch.batch_id.clone(),
        accepted: ai_stats_core::SyncEntityCounts {
            sources: 0,
            accounts: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
        },
        duplicates: ai_stats_core::SyncEntityCounts {
            sources: 0,
            accounts: 0,
            subscriptions: 0,
            events: 0,
            summaries: 0,
        },
        rejected: Vec::new(),
    };

    for index in 0..sub_batches {
        let source_count = if include_metadata {
            batch.sources.len()
        } else {
            0
        };
        let account_count = if include_metadata {
            batch.accounts.len()
        } else {
            0
        };
        let subscription_count = if include_metadata {
            batch.subscriptions.len()
        } else {
            0
        };

        let event_end = (sent_events + records_per_batch).min(batch.events.len());
        let event_slice = &batch.events[sent_events..event_end];
        let remaining = records_per_batch.saturating_sub(event_slice.len());
        let summary_end = (sent_summaries + remaining).min(batch.summaries.len());
        let summary_slice = &batch.summaries[sent_summaries..summary_end];

        let sub_batch = SyncBatch {
            schema_version: batch.schema_version.clone(),
            batch_id: format!("{}_{}", batch.batch_id, index + 1),
            device_id: device_id.to_string(),
            sources: if include_metadata {
                batch.sources.clone()
            } else {
                Vec::new()
            },
            accounts: if include_metadata {
                batch.accounts.clone()
            } else {
                Vec::new()
            },
            subscriptions: if include_metadata {
                batch.subscriptions.clone()
            } else {
                Vec::new()
            },
            events: event_slice.to_vec(),
            summaries: summary_slice.to_vec(),
            created_at: batch.created_at,
        };

        eprintln!(
            "firestore sync: sending sub-batch {}/{} (sources={}, accounts={}, subscriptions={}, events={}, summaries={})",
            index + 1,
            sub_batches,
            source_count,
            account_count,
            subscription_count,
            sub_batch.events.len(),
            sub_batch.summaries.len()
        );

        match sink.send_with_ack_and_options(&sub_batch, &options) {
            Ok(ack) => {
                aggregate_ack.accepted.sources += ack.accepted.sources;
                aggregate_ack.accepted.accounts += ack.accepted.accounts;
                aggregate_ack.accepted.subscriptions += ack.accepted.subscriptions;
                aggregate_ack.accepted.events += ack.accepted.events;
                aggregate_ack.accepted.summaries += ack.accepted.summaries;
                aggregate_ack.duplicates.sources += ack.duplicates.sources;
                aggregate_ack.duplicates.accounts += ack.duplicates.accounts;
                aggregate_ack.duplicates.subscriptions += ack.duplicates.subscriptions;
                aggregate_ack.duplicates.events += ack.duplicates.events;
                aggregate_ack.duplicates.summaries += ack.duplicates.summaries;
                aggregate_ack.rejected.extend(ack.rejected);

                let passthrough_summaries: Vec<_> = sub_batch
                    .summaries
                    .iter()
                    .filter(|summary| !is_daily_rollup_summary(summary))
                    .cloned()
                    .collect();
                let rollup_summary_ids: Vec<_> = sub_batch
                    .summaries
                    .iter()
                    .filter(|summary| is_daily_rollup_summary(summary))
                    .map(|summary| summary.summary_id.clone())
                    .collect();

                store.record_sync_success(
                    &command.sink,
                    &target,
                    &sub_batch.batch_id,
                    &sub_batch.events,
                    &passthrough_summaries,
                )?;
                store.mark_firestore_rollups_synced(&rollup_summary_ids)?;
                store.record_summaries_synced(&command.sink, &target, &passthrough_summaries)?;
                store.record_sources_synced(&command.sink, &target, &sub_batch.sources)?;
                store.record_accounts_synced(&command.sink, &target, &sub_batch.accounts)?;
                store.record_subscriptions_synced(
                    &command.sink,
                    &target,
                    &sub_batch.subscriptions,
                )?;
                sent_events = event_end;
                sent_summaries = summary_end;
                include_metadata = false;
            }
            Err(error) => {
                let _ = store.record_sync_failure(&command.sink, &target);
                return Err(error);
            }
        }
    }

    println!("{}", serde_json::to_string_pretty(&aggregate_ack)?);
    Ok(())
}

fn hosted_firestore_full_mode_disabled(command: &SyncCommand) -> bool {
    command.sink == "firestore"
        && command.firestore_mode.eq_ignore_ascii_case("full")
        && std::env::var("FIRESTORE_EMULATOR_HOST")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        && std::env::var("AI_STATS_ENABLE_HOSTED_FIRESTORE_FULL")
            .ok()
            .is_none_or(|value| value.trim() != "1")
}

fn sync_status(store: &Store) -> Result<()> {
    let states = store.list_sync_states()?;
    if states.is_empty() {
        println!("no sync state recorded");
        return Ok(());
    }
    for state in states {
        println!(
            "{} target={} last_success={} batch={} event_cursor={} summary_cursor={} failures={}",
            state.sink,
            state.target,
            state.last_success_at.to_rfc3339(),
            state.last_batch_id,
            format_cursor(
                state
                    .last_event_started_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_event_id.as_deref()
            ),
            format_cursor(
                state
                    .last_summary_observed_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_summary_id.as_deref()
            ),
            state.failure_count
        );
    }
    Ok(())
}

fn sync_verify(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    if command.sink != "firestore" {
        bail!("--verify is currently supported only with --sink firestore");
    }

    let auth = resolve_firestore_auth_context(&command)?;
    let target = firestore_sync_target(&command.firebase_project, &auth.uid);
    let local_state = store.sync_state("firestore", &target)?;
    let report = FirestoreVerifyReport {
        sink: command.sink.clone(),
        target: target.clone(),
        project: command.firebase_project.clone(),
        uid: auth.uid.clone(),
        using_emulator: firestore_emulator_host().is_some(),
        device_id: device_id.to_string(),
        local: firestore_local_verify(store, &target, local_state.as_ref())?,
        remote: firestore_remote_verify(
            &command.firebase_project,
            &auth.uid,
            &auth.auth_token,
            device_id,
        )?,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn firestore_local_verify(
    store: &Store,
    target: &str,
    local_state: Option<&SyncState>,
) -> Result<FirestoreLocalVerify> {
    let all_sources = store.list_sources()?;
    let all_accounts = store.list_accounts()?;
    let all_subscriptions = store.list_subscriptions()?;
    let firestore_sources: Vec<_> = all_sources
        .iter()
        .cloned()
        .map(sanitize_source_for_sync)
        .collect();
    let firestore_accounts: Vec<_> = all_accounts
        .iter()
        .cloned()
        .map(sanitize_account_for_sync)
        .collect();
    let firestore_subscriptions: Vec<_> = all_subscriptions
        .iter()
        .cloned()
        .map(sanitize_subscription_for_sync)
        .collect();
    let passthrough_summaries: Vec<_> = store
        .summaries()?
        .into_iter()
        .map(sanitize_summary_for_sync)
        .filter(|summary| !is_daily_rollup_summary(summary))
        .collect();

    Ok(FirestoreLocalVerify {
        sync_state: local_state.map(sync_state_report),
        total_sources: all_sources.len(),
        enabled_sources: all_sources.iter().filter(|source| source.enabled).count(),
        pending_sources: store
            .pending_sources_for_sync("firestore", target, &firestore_sources)?
            .len(),
        total_accounts: all_accounts.len(),
        pending_accounts: store
            .pending_accounts_for_sync("firestore", target, &firestore_accounts)?
            .len(),
        total_subscriptions: all_subscriptions.len(),
        pending_subscriptions: store
            .pending_subscriptions_for_sync("firestore", target, &firestore_subscriptions)?
            .len(),
        total_passthrough_summaries: passthrough_summaries.len(),
        pending_passthrough_summaries: store
            .pending_summaries_for_sync("firestore", target, &passthrough_summaries)?
            .len(),
        total_rollups: store.firestore_rollup_count()? as usize,
        dirty_rollups: store.dirty_firestore_rollup_summaries()?.len(),
    })
}

fn sync_target(command: &SyncCommand) -> String {
    match command.sink.as_str() {
        "http" => command
            .endpoint
            .clone()
            .unwrap_or_else(|| "http".to_string()),
        "firestore" => format!("firestore:{}", command.firebase_project),
        "file" => command
            .output
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "ai-stats-sync-batch.json".to_string()),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone)]
struct FirestoreAuthContext {
    auth_token: String,
    uid: String,
}

#[derive(Debug, Serialize)]
struct FirestoreVerifyReport {
    sink: String,
    target: String,
    project: String,
    uid: String,
    using_emulator: bool,
    device_id: String,
    local: FirestoreLocalVerify,
    remote: FirestoreRemoteVerify,
}

#[derive(Debug, Serialize)]
struct FirestoreLocalVerify {
    sync_state: Option<SyncStateReport>,
    total_sources: usize,
    enabled_sources: usize,
    pending_sources: usize,
    total_accounts: usize,
    pending_accounts: usize,
    total_subscriptions: usize,
    pending_subscriptions: usize,
    total_passthrough_summaries: usize,
    pending_passthrough_summaries: usize,
    total_rollups: usize,
    dirty_rollups: usize,
}

#[derive(Debug, Serialize)]
struct SyncStateReport {
    last_success_at: String,
    last_batch_id: String,
    event_cursor: String,
    summary_cursor: String,
    failure_count: u64,
}

#[derive(Debug, Serialize)]
struct FirestoreRemoteVerify {
    device: Option<FirestoreDeviceSnapshot>,
    recent_batches: Vec<FirestoreBatchSnapshot>,
    collections: Vec<FirestoreCollectionSnapshot>,
}

#[derive(Debug, Serialize)]
struct FirestoreDeviceSnapshot {
    document_id: String,
    last_batch_id: Option<String>,
    last_synced_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct FirestoreBatchSnapshot {
    document_id: String,
    batch_id: Option<String>,
    device_id: Option<String>,
    synced_at: Option<String>,
    counts: Option<Value>,
}

#[derive(Debug, Serialize)]
struct FirestoreCollectionSnapshot {
    collection: String,
    returned_docs: usize,
    has_more: bool,
    sample_doc_ids: Vec<String>,
}

fn resolve_firestore_auth_context(command: &SyncCommand) -> Result<FirestoreAuthContext> {
    let using_emulator = std::env::var("FIRESTORE_EMULATOR_HOST")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let manual_auth_token = command
        .auth_token
        .clone()
        .or_else(|| std::env::var("AI_STATS_SYNC_TOKEN").ok());
    let auth_token = manual_auth_token
        .clone()
        .or_else(|| auth::get_or_refresh_token().ok().flatten())
        .or_else(|| using_emulator.then(|| "owner".to_string()))
        .context("Firebase login required; run `ai-stats auth login` first")?;
    let uid = command
        .firebase_uid
        .clone()
        .or_else(|| auth::user_id_from_token(&auth_token).ok().flatten())
        .or_else(|| {
            if manual_auth_token.is_none() {
                auth::user_id().ok().flatten()
            } else {
                None
            }
        })
        .or_else(|| {
            using_emulator.then(|| {
                std::env::var("AI_STATS_FIRESTORE_TEST_UID")
                    .unwrap_or_else(|_| "local-test-user".to_string())
            })
        })
        .context(
            "Firebase UID required for Firestore sync; rerun `ai-stats auth login`, provide --firebase-uid, or use a Firebase ID token whose UID can be derived locally",
        )?;
    Ok(FirestoreAuthContext { auth_token, uid })
}

fn firestore_sync_target(project: &str, uid: &str) -> String {
    format!("firestore:{project}:{uid}")
}

fn sync_state_report(state: &ai_stats_store::SyncState) -> SyncStateReport {
    SyncStateReport {
        last_success_at: state.last_success_at.to_rfc3339(),
        last_batch_id: state.last_batch_id.clone(),
        event_cursor: format_cursor(
            state
                .last_event_started_at
                .as_ref()
                .map(DateTime::to_rfc3339),
            state.last_event_id.as_deref(),
        ),
        summary_cursor: format_cursor(
            state
                .last_summary_observed_at
                .as_ref()
                .map(DateTime::to_rfc3339),
            state.last_summary_id.as_deref(),
        ),
        failure_count: state.failure_count,
    }
}

fn firestore_remote_verify(
    project: &str,
    uid: &str,
    auth_token: &str,
    device_id: &str,
) -> Result<FirestoreRemoteVerify> {
    let device = firestore_get_document(
        project,
        &format!(
            "users/{}/devices/{}",
            sanitize_firestore_document_id(uid),
            sanitize_firestore_document_id(device_id)
        ),
        auth_token,
    )?
    .map(|document| FirestoreDeviceSnapshot {
        document_id: firestore_document_id(&document),
        last_batch_id: firestore_string_field(
            &document,
            &["fields", "last_batch_id", "stringValue"],
        ),
        last_synced_at: firestore_string_field(
            &document,
            &["fields", "last_synced_at", "stringValue"],
        ),
    });

    let batches = firestore_list_collection(project, uid, "syncBatches", 5, auth_token)?;
    let mut recent_batches: Vec<_> = batches
        .documents
        .iter()
        .map(|document| FirestoreBatchSnapshot {
            document_id: firestore_document_id(document),
            batch_id: firestore_string_field(document, &["fields", "batch_id", "stringValue"]),
            device_id: firestore_string_field(document, &["fields", "device_id", "stringValue"]),
            synced_at: firestore_string_field(document, &["fields", "synced_at", "stringValue"]),
            counts: document.pointer("/fields/counts/mapValue/fields").cloned(),
        })
        .collect();
    recent_batches.sort_by(|left, right| right.synced_at.cmp(&left.synced_at));

    let mut collections = Vec::new();
    for (name, page_size) in [
        ("devices", 3usize),
        ("syncBatches", 5usize),
        ("sources", 3usize),
        ("accounts", 3usize),
        ("subscriptions", 3usize),
        ("events", 1usize),
        ("summaries", 3usize),
    ] {
        let listing = firestore_list_collection(project, uid, name, page_size, auth_token)?;
        collections.push(FirestoreCollectionSnapshot {
            collection: name.to_string(),
            returned_docs: listing.documents.len(),
            has_more: listing.next_page_token.is_some(),
            sample_doc_ids: listing
                .documents
                .iter()
                .map(firestore_document_id)
                .collect(),
        });
    }

    Ok(FirestoreRemoteVerify {
        device,
        recent_batches,
        collections,
    })
}

struct FirestoreListResponse {
    documents: Vec<Value>,
    next_page_token: Option<String>,
}

fn firestore_get_document(
    project: &str,
    relative_path: &str,
    auth_token: &str,
) -> Result<Option<Value>> {
    let url = firestore_document_get_url(project, relative_path);
    let mut request = ureq::get(&url);
    if firestore_emulator_host().is_none() {
        request = request.set("Authorization", &format!("Bearer {auth_token}"));
    }
    match request.call() {
        Ok(response) => Ok(Some(firestore_response_json(response, "read document")?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(error) => Err(firestore_request_error("read document", error)),
    }
}

fn firestore_list_collection(
    project: &str,
    uid: &str,
    collection: &str,
    page_size: usize,
    auth_token: &str,
) -> Result<FirestoreListResponse> {
    let url = firestore_collection_list_url(project, uid, collection, page_size);
    let mut request = ureq::get(&url);
    if firestore_emulator_host().is_none() {
        request = request.set("Authorization", &format!("Bearer {auth_token}"));
    }
    let value = match request.call() {
        Ok(response) => {
            firestore_response_json(response, &format!("list collection {collection}"))?
        }
        Err(ureq::Error::Status(404, _)) => Value::Object(Default::default()),
        Err(error) => {
            return Err(firestore_request_error(
                &format!("list collection {collection}"),
                error,
            ))
        }
    };
    Ok(FirestoreListResponse {
        documents: value
            .get("documents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        next_page_token: value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn firestore_request_error(action: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            anyhow::anyhow!("Firestore {action} failed (HTTP {code}): {body}")
        }
        other => anyhow::anyhow!("Firestore {action} failed: {other}"),
    }
}

fn firestore_response_json(response: ureq::Response, action: &str) -> Result<Value> {
    let body = response
        .into_string()
        .with_context(|| format!("read Firestore {action} response body"))?;
    serde_json::from_str(&body).with_context(|| format!("parse Firestore {action} response JSON"))
}

fn firestore_collection_list_url(
    project: &str,
    uid: &str,
    collection: &str,
    page_size: usize,
) -> String {
    let prefix = firestore_document_path_prefix(project, uid);
    format!("{prefix}/{collection}?pageSize={page_size}")
}

fn firestore_document_get_url(project: &str, relative_path: &str) -> String {
    let base = firestore_api_base(project);
    format!("{base}/{relative_path}")
}

fn firestore_document_path_prefix(project: &str, uid: &str) -> String {
    let base = firestore_api_base(project);
    format!("{base}/users/{}", sanitize_firestore_document_id(uid))
}

fn firestore_api_base(project: &str) -> String {
    if let Some(host) = firestore_emulator_host() {
        return format!("http://{host}/v1/projects/{project}/databases/(default)/documents");
    }
    format!("https://firestore.googleapis.com/v1/projects/{project}/databases/(default)/documents")
}

fn firestore_emulator_host() -> Option<String> {
    std::env::var("FIRESTORE_EMULATOR_HOST")
        .ok()
        .map(|value| {
            value
                .trim()
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

fn sanitize_firestore_document_id(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if safe && value != "." && value != ".." && !value.starts_with("__") && !value.ends_with("__") {
        return value.to_string();
    }

    let mut output = String::with_capacity(4 + value.len() * 2);
    output.push_str("hex_");
    for byte in value.as_bytes() {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn firestore_document_id(document: &Value) -> String {
    document
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name.rsplit('/').next())
        .unwrap_or("unknown")
        .to_string()
}

fn firestore_string_field(document: &Value, path: &[&str]) -> Option<String> {
    let mut value = document;
    for segment in path {
        value = value.get(*segment)?;
    }
    value.as_str().map(ToOwned::to_owned)
}

fn format_cursor(date: Option<String>, id: Option<&str>) -> String {
    match (date, id) {
        (Some(date), Some(id)) => format!("{date}/{id}"),
        _ => "none".to_string(),
    }
}

fn schema(command: SchemaCommand) -> Result<()> {
    match command.command {
        SchemaSubcommand::SyncBatch => {
            let schema = schemars::schema_for!(SyncBatch);
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
    }
    Ok(())
}

fn daemon(command: DaemonCommand, store: Store, device_id: &str) -> Result<()> {
    let store = Arc::new(Mutex::new(store));
    if command.watch {
        ai_stats_daemon::watch_and_serve(&command.api, store, device_id)
    } else {
        ai_stats_daemon::run(&command.api, store)
    }
}

fn status(store: &Store) -> Result<()> {
    println!("stored all-time events: {}", store.event_count()?);
    println!("stored all-time tokens: {}", store.token_total()?);
    println!("stored usage summaries: {}", store.summary_count()?);
    Ok(())
}

fn doctor(store_path: &Path) -> Result<()> {
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
            let file_cache_entries = scan_file_state_entries(&candidates);
            let pending =
                store.pending_scan_file_entries(&source.source_id, &file_cache_entries)?;
            let pending_keys: BTreeSet<_> = pending
                .iter()
                .map(|entry| entry.cache_key.as_str())
                .collect();
            let cached: Vec<_> = candidates
                .iter()
                .filter(|candidate| !pending_keys.contains(candidate.cache_key.as_str()))
                .collect();
            println!(
                "  - {} account={} origin={} files={} pending={} cached={}",
                preview_path_label(&source),
                source.account_hint.as_deref().unwrap_or("unmapped"),
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

fn parse_date(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(date.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")?;
    let datetime = date
        .and_hms_opt(0, 0, 0)
        .context("failed to build midnight timestamp")?;
    Ok(datetime.and_utc())
}

#[derive(Debug)]
struct ReportedImportReport {
    path: PathBuf,
    records: Vec<ReportedUsageSummaryRecord>,
    warnings: Vec<String>,
}

fn read_reported_summary_inputs(path: &Path) -> Result<Vec<ReportedUsageSummaryInput>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(input) = serde_json::from_str::<ReportedUsageSummaryInput>(&text) {
        return Ok(vec![input]);
    }
    let inputs = serde_json::from_str::<Vec<ReportedUsageSummaryInput>>(&text)
        .with_context(|| format!("parse reported usage summary JSON {}", path.display()))?;
    Ok(inputs)
}

fn print_report_table(report: &UsageReport, verbose: bool) {
    println!("ai-stats report: {}", report.label);
    if let Some(since) = report.since {
        println!(
            "range: {} to {}",
            since.to_rfc3339(),
            report.until.to_rfc3339()
        );
    } else {
        println!(
            "range: all stored events through {}",
            report.until.to_rfc3339()
        );
    }
    println!(
        "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "provider",
        "account",
        "events",
        "input",
        "cache_create",
        "cache_read",
        "output",
        "total",
        "est_cost"
    );
    for row in &report.rows {
        println!(
            "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            row.provider,
            row.account,
            format_u64(row.events),
            format_u64(row.usage.input_tokens),
            format_u64(row.usage.cache_creation_tokens),
            format_u64(row.usage.cached_input_tokens),
            format_u64(row.usage.output_tokens),
            format_u64(row.usage.total_tokens),
            format_cost(row.usage.estimated_cost_usd)
        );
        if verbose {
            println!("  reasoning: {}", format_u64(row.usage.reasoning_tokens));
            println!(
                "  sources: {}",
                row.sources.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            if !row.paths.is_empty() {
                println!(
                    "  paths: {}",
                    row.paths.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }
    }
    println!(
        "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "total",
        "",
        format_u64(report.total_events),
        format_u64(report.total_usage.input_tokens),
        format_u64(report.total_usage.cache_creation_tokens),
        format_u64(report.total_usage.cached_input_tokens),
        format_u64(report.total_usage.output_tokens),
        format_u64(report.total_usage.total_tokens),
        format_cost(report.total_usage.estimated_cost_usd)
    );

    if !report.summary_rows.is_empty() {
        let summary_direct_total: u64 = report
            .summary_rows
            .iter()
            .map(|row| row.direct_event_usage.total_tokens)
            .sum();
        println!(
            "reported/manual summaries (separate provenance, included in known gross totals):"
        );
        println!(
            "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "provider",
            "account",
            "kind",
            "summaries",
            "input",
            "cache_create",
            "cache_read",
            "output",
            "total",
            "cost",
            "uncovered"
        );
        for row in &report.summary_rows {
            let uncovered = row
                .usage
                .total_tokens
                .saturating_sub(row.direct_event_usage.total_tokens);
            println!(
                "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                row.provider,
                row.account,
                row.kind,
                format_u64(row.summaries),
                format_u64(row.usage.input_tokens),
                format_u64(row.usage.cache_creation_tokens),
                format_u64(row.usage.cached_input_tokens),
                format_u64(row.usage.output_tokens),
                format_u64(row.usage.total_tokens),
                format_cost(row.usage.estimated_cost_usd),
                format_u64(uncovered)
            );
            if verbose {
                if let Some(observed_at) = row.observed_at {
                    println!("  observed_at: {}", observed_at.to_rfc3339());
                }
                println!(
                    "  direct_overlap_total: {}",
                    format_u64(row.direct_event_usage.total_tokens)
                );
                if row.exact_overlap_summaries > 0 {
                    println!(
                        "  exact_overlap_summaries: {}",
                        format_u64(row.exact_overlap_summaries)
                    );
                }
                println!(
                    "  sources: {}",
                    row.sources.iter().cloned().collect::<Vec<_>>().join(", ")
                );
                if !row.paths.is_empty() {
                    println!(
                        "  paths: {}",
                        row.paths.iter().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
        println!(
            "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "summary total",
            "",
            "",
            format_u64(report.summary_rows.iter().map(|row| row.summaries).sum()),
            format_u64(report.total_summary_usage.input_tokens),
            format_u64(report.total_summary_usage.cache_creation_tokens),
            format_u64(report.total_summary_usage.cached_input_tokens),
            format_u64(report.total_summary_usage.output_tokens),
            format_u64(report.total_summary_usage.total_tokens),
            format_cost(report.total_summary_usage.estimated_cost_usd),
            format_u64(
                report
                    .total_summary_usage
                    .total_tokens
                    .saturating_sub(summary_direct_total)
            )
        );
        print_known_usage_table(report);
    }
}

fn print_known_usage_table(report: &UsageReport) {
    let mut direct_by_provider: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for row in &report.rows {
        direct_by_provider
            .entry(row.provider.clone())
            .or_default()
            .add_totals(&row.usage);
    }
    let mut reported_by_provider: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for row in &report.summary_rows {
        reported_by_provider
            .entry(row.provider.clone())
            .or_default()
            .add_totals(&row.usage);
    }
    let providers: BTreeSet<_> = direct_by_provider
        .keys()
        .chain(reported_by_provider.keys())
        .cloned()
        .collect();
    println!("known gross totals by provider (direct + reported/manual, no overlap deduction):");
    println!(
        "{:<14} {:>14} {:>14} {:>14} {:>12} {:>12} {:>12}",
        "provider",
        "direct",
        "reported",
        "known_gross",
        "direct_cost",
        "reported_cost",
        "known_cost"
    );
    for provider in providers {
        let direct = direct_by_provider
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        let reported = reported_by_provider
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        let mut known = direct.clone();
        known.add_totals(&reported);
        println!(
            "{:<14} {:>14} {:>14} {:>14} {:>12} {:>12} {:>12}",
            provider,
            format_u64(direct.total_tokens),
            format_u64(reported.total_tokens),
            format_u64(known.total_tokens),
            format_cost(direct.estimated_cost_usd),
            format_cost(reported.estimated_cost_usd),
            format_cost(known.estimated_cost_usd)
        );
    }
}

fn print_report_json(report: &UsageReport, verbose: bool) -> Result<()> {
    let rows = report.rows.iter().map(|row| {
        let mut value = json!({
            "provider": row.provider,
            "account": row.account,
            "events": row.events,
            "tokens": {
                "input": row.usage.input_tokens,
                "cache_creation": row.usage.cache_creation_tokens,
                "cache_read": row.usage.cached_input_tokens,
                "cached_input": row.usage.cached_input_tokens,
                "output": row.usage.output_tokens,
                "reasoning": row.usage.reasoning_tokens,
                "total": row.usage.total_tokens,
            },
            "estimated_cost_usd": row.usage.estimated_cost_usd,
        });
        if verbose {
            value["sources"] = json!(row.sources.iter().cloned().collect::<Vec<_>>());
            value["paths"] = json!(row.paths.iter().cloned().collect::<Vec<_>>());
        }
        value
    });
    let summary_rows = report.summary_rows.iter().map(|row| {
        let mut value = json!({
            "provider": row.provider,
            "account": row.account,
            "kind": row.kind,
            "summaries": row.summaries,
            "tokens": {
                "input": row.usage.input_tokens,
                "cache_creation": row.usage.cache_creation_tokens,
                "cache_read": row.usage.cached_input_tokens,
                "cached_input": row.usage.cached_input_tokens,
                "output": row.usage.output_tokens,
                "reasoning": row.usage.reasoning_tokens,
                "total": row.usage.total_tokens,
            },
            "direct_overlap_total_tokens": row.direct_event_usage.total_tokens,
            "uncovered_total_tokens": row.usage.total_tokens.saturating_sub(row.direct_event_usage.total_tokens),
            "exact_overlap_summaries": row.exact_overlap_summaries,
            "observed_at": row.observed_at.map(|date| date.to_rfc3339()),
            "estimated_or_reported_cost_usd": row.usage.estimated_cost_usd,
        });
        if verbose {
            value["sources"] = json!(row.sources.iter().cloned().collect::<Vec<_>>());
            value["paths"] = json!(row.paths.iter().cloned().collect::<Vec<_>>());
        }
        value
    });
    let summary_direct_total: u64 = report
        .summary_rows
        .iter()
        .map(|row| row.direct_event_usage.total_tokens)
        .sum();
    let mut known_usage = report.total_usage.clone();
    known_usage.add_totals(&report.total_summary_usage);
    let value = json!({
        "label": report.label,
        "since": report.since.map(|date| date.to_rfc3339()),
        "until": report.until.to_rfc3339(),
        "total_events": report.total_events,
        "total_tokens": {
            "input": report.total_usage.input_tokens,
            "cache_creation": report.total_usage.cache_creation_tokens,
            "cache_read": report.total_usage.cached_input_tokens,
            "cached_input": report.total_usage.cached_input_tokens,
            "output": report.total_usage.output_tokens,
            "reasoning": report.total_usage.reasoning_tokens,
            "total": report.total_usage.total_tokens,
        },
        "total_estimated_cost_usd": report.total_usage.estimated_cost_usd,
        "known_gross": {
            "description": "direct events plus reported/manual summaries, without overlap deduction",
            "total_tokens": {
                "input": known_usage.input_tokens,
                "cache_creation": known_usage.cache_creation_tokens,
                "cache_read": known_usage.cached_input_tokens,
                "cached_input": known_usage.cached_input_tokens,
                "output": known_usage.output_tokens,
                "reasoning": known_usage.reasoning_tokens,
                "total": known_usage.total_tokens,
            },
            "estimated_or_reported_cost_usd": known_usage.estimated_cost_usd,
        },
        "summary_reports": {
            "included_in_event_totals": false,
            "included_in_known_gross_totals": true,
            "total_tokens": {
                "input": report.total_summary_usage.input_tokens,
                "cache_creation": report.total_summary_usage.cache_creation_tokens,
                "cache_read": report.total_summary_usage.cached_input_tokens,
                "cached_input": report.total_summary_usage.cached_input_tokens,
                "output": report.total_summary_usage.output_tokens,
                "reasoning": report.total_summary_usage.reasoning_tokens,
                "total": report.total_summary_usage.total_tokens,
            },
            "estimated_or_reported_cost_usd": report.total_summary_usage.estimated_cost_usd,
            "uncovered_total_tokens": report.total_summary_usage.total_tokens.saturating_sub(summary_direct_total),
            "rows": summary_rows.collect::<Vec<_>>(),
        },
        "rows": rows.collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn default_store_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ai-stats")
        .join("ai-stats.sqlite")
}

fn default_device_id() -> String {
    if let Ok(value) = std::env::var("AI_STATS_DEVICE_ID") {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let path = device_id_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }

    // Persist a stable opaque ID instead of leaking hostnames to the backend.
    let device_id = generate_device_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{device_id}\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    device_id
}

fn device_id_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ai-stats")
        .join("device-id")
}

fn generate_device_id() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(read_hostname)
        .unwrap_or_else(|| "unknown-host".to_string());
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown-user".to_string());
    let home = home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let seed = format!(
        "{}:{}:{}:{}:{}",
        host,
        user,
        home,
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    format!("dev_{}", &hash_text(&seed)[..16])
}

fn read_hostname() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8(output.stdout).ok()?;
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}

fn normalize_configured_source_path(provider: &str, path: &Path) -> Result<PathBuf> {
    let mut path = expand_cli_path(path)?;
    if provider_matches(provider, "claude_code")
        && path.file_name().is_some_and(|name| name == "projects")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn expand_cli_path(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().context("HOME is not set");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home_dir().context("HOME is not set")?.join(rest));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("read current directory")?
        .join(path))
}

fn path_label_from_hashless_source(source: &SourceLocation) -> Option<String> {
    let home = home_dir()?;
    match (source.provider.as_str(), source.location_origin.clone()) {
        ("claude_code", LocationOrigin::Default) if source.path_hash.is_some() => {
            let a = home.join(".config/claude/projects");
            let b = home.join(".claude/projects");
            let hash = source.path_hash.as_ref()?;
            for path in [a, b] {
                if ai_stats_core::path_hash(&path) == *hash {
                    return Some(path.to_string_lossy().to_string());
                }
            }
            None
        }
        ("codex", LocationOrigin::Default) if source.path_hash.is_some() => {
            let root = home.join(".codex");
            let hash = source.path_hash.as_ref()?;
            if ai_stats_core::path_hash(&root) == *hash {
                return Some(root.to_string_lossy().to_string());
            }
            None
        }
        _ => None,
    }
}

fn print_scan_preview_line(
    source: &SourceLocation,
    usage_events: u64,
    usage: &UsageTotals,
    summaries: u64,
    summary_usage: &UsageTotals,
    diagnostics: &ScanDiagnostics,
    verbose: bool,
) {
    if verbose {
        println!(
            "{} account={} path={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} files={} cached={} timestamp_fallbacks={} model_fallbacks={} origin={} source={}",
            source.provider,
            source.account_hint.as_deref().unwrap_or("unmapped"),
            preview_path_label(source),
            usage_events,
            summaries,
            format_u64(usage.input_tokens),
            format_u64(usage.cache_creation_tokens),
            format_u64(usage.cached_input_tokens),
            format_u64(usage.output_tokens),
            format_u64(usage.total_tokens),
            format_cost(usage.estimated_cost_usd),
            format_u64(summary_usage.total_tokens),
            format_u64(diagnostics.raw_rows),
            format_u64(diagnostics.candidate_usage_rows),
            format_u64(diagnostics.duplicate_events),
            format_u64(diagnostics.skipped_zero_events),
            format_u64(diagnostics.invalid_rows),
            format_u64(diagnostics.files_scanned),
            format_u64(diagnostics.files_skipped_unchanged),
            format_u64(diagnostics.timestamp_fallbacks),
            format_u64(diagnostics.model_fallbacks),
            location_origin_label(&source.location_origin),
            source.source_id.0
        );
    } else {
        println!(
            "{} account={} path={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={}",
            source.provider,
            source.account_hint.as_deref().unwrap_or("unmapped"),
            preview_path_label(source),
            usage_events,
            summaries,
            format_u64(usage.input_tokens),
            format_u64(usage.cache_creation_tokens),
            format_u64(usage.cached_input_tokens),
            format_u64(usage.output_tokens),
            format_u64(usage.total_tokens),
            format_cost(usage.estimated_cost_usd),
            format_u64(summary_usage.total_tokens)
        );
    }
}

fn add_diagnostics(target: &mut ScanDiagnostics, source: &ScanDiagnostics) {
    target.files_scanned += source.files_scanned;
    target.files_skipped_unchanged += source.files_skipped_unchanged;
    target.raw_rows += source.raw_rows;
    target.candidate_usage_rows += source.candidate_usage_rows;
    target.accepted_events += source.accepted_events;
    target.duplicate_events += source.duplicate_events;
    target.skipped_zero_events += source.skipped_zero_events;
    target.invalid_rows += source.invalid_rows;
    target.timestamp_fallbacks += source.timestamp_fallbacks;
    target.model_fallbacks += source.model_fallbacks;
}

fn apply_account_hint_to_events(source: &SourceLocation, events: &mut [UsageEvent]) {
    let Some(account_hint) = source.account_hint.as_deref() else {
        return;
    };
    for event in events {
        event.provider_account_id = Some(provider_account_id(&source.provider, account_hint));
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::ManualHint;
        }
    }
}

fn apply_account_hint_to_summaries(source: &SourceLocation, summaries: &mut [UsageSummary]) {
    let Some(account_hint) = source.account_hint.as_deref() else {
        return;
    };
    for summary in summaries {
        summary.provider_account_id = Some(provider_account_id(&source.provider, account_hint));
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::ManualHint;
        }
    }
}

fn print_scan_diagnostics_total(diagnostics: &ScanDiagnostics) {
    println!(
        "diagnostics: files={} cached={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} timestamp_fallbacks={} model_fallbacks={}",
        format_u64(diagnostics.files_scanned),
        format_u64(diagnostics.files_skipped_unchanged),
        format_u64(diagnostics.raw_rows),
        format_u64(diagnostics.candidate_usage_rows),
        format_u64(diagnostics.duplicate_events),
        format_u64(diagnostics.skipped_zero_events),
        format_u64(diagnostics.invalid_rows),
        format_u64(diagnostics.timestamp_fallbacks),
        format_u64(diagnostics.model_fallbacks)
    );
}

fn scan_file_state_entries(candidates: &[ScanCandidateFile]) -> Vec<ScanFileStateEntry> {
    candidates
        .iter()
        .map(|candidate| ScanFileStateEntry {
            cache_key: candidate.cache_key.clone(),
            cache_signature: candidate.cache_signature.clone(),
        })
        .collect()
}

fn select_scan_file_entries(
    store: &Store,
    source_id: &ai_stats_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    replace: bool,
    no_cache: bool,
) -> Result<Vec<ScanFileStateEntry>> {
    if replace || no_cache {
        return Ok(file_cache_entries.to_vec());
    }
    store.pending_scan_file_entries(source_id, file_cache_entries)
}

fn format_cache_key_sample<'a>(keys: impl IntoIterator<Item = &'a str>) -> String {
    let values: Vec<_> = keys.into_iter().map(abbreviate_home).collect();
    if values.is_empty() {
        return "none".to_string();
    }
    let sample: Vec<_> = values.iter().take(3).cloned().collect();
    let remaining = values.len().saturating_sub(sample.len());
    if remaining == 0 {
        sample.join(", ")
    } else {
        format!("{} (+{} more)", sample.join(", "), remaining)
    }
}

fn scan_sources_for_adapter(
    adapter: &dyn ProviderAdapter,
    configured_sources: &[SourceLocation],
) -> Vec<SourceLocation> {
    let mut sources = BTreeMap::new();
    for mut source in adapter.discover() {
        if source.path_label.is_none() {
            source.path_label = path_label_from_hashless_source(&source);
        }
        sources.insert(source.source_id.0.clone(), source);
    }
    for mut source in configured_sources
        .iter()
        .filter(|source| {
            source.enabled
                && provider_matches(&source.provider, adapter.provider())
                && source.source_kind == SourceKind::LocalAdapter
        })
        .cloned()
    {
        if source.path_label.is_none() {
            source.path_label = path_label_from_hashless_source(&source);
        }
        sources.insert(source.source_id.0.clone(), source);
    }
    dedupe_overlapping_sources(sources.into_values().collect())
}

fn dedupe_overlapping_sources(sources: Vec<SourceLocation>) -> Vec<SourceLocation> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            let Some(source_path) = comparable_source_path(source) else {
                return Some(source.clone());
            };
            let shadowed = sources.iter().enumerate().any(|(other_index, other)| {
                if index == other_index || !provider_matches(&source.provider, &other.provider) {
                    return false;
                }
                let Some(other_path) = comparable_source_path(other) else {
                    return false;
                };
                other_path != source_path
                    && source_path.starts_with(&other_path)
                    && source_preference_rank(other) >= source_preference_rank(source)
            });
            (!shadowed).then(|| source.clone())
        })
        .collect()
}

fn comparable_source_path(source: &SourceLocation) -> Option<PathBuf> {
    let path = PathBuf::from(source.path_label.as_deref()?);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn source_preference_rank(source: &SourceLocation) -> u8 {
    match source.location_origin {
        LocationOrigin::Configured | LocationOrigin::Env => 3,
        LocationOrigin::Discovered => 2,
        LocationOrigin::Default => 1,
    }
}

fn provider_matches(left: &str, right: &str) -> bool {
    match (
        canonical_provider_name(left),
        canonical_provider_name(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right || left.replace('-', "_") == right || left.replace('_', "-") == right,
    }
}

fn canonical_provider(provider: &str) -> Result<String> {
    canonical_provider_name(provider)
        .map(str::to_string)
        .with_context(|| format!("unsupported provider {provider}"))
}

fn canonical_provider_name(provider: &str) -> Option<&'static str> {
    adapter_for_provider(provider).map(|adapter| adapter.provider())
}

fn persist_source_after_preview(store: &Store, source: &SourceLocation) -> Result<()> {
    store.upsert_source(source)
}

fn location_origin_label(origin: &LocationOrigin) -> &'static str {
    match origin {
        LocationOrigin::Default => "default",
        LocationOrigin::Configured => "configured",
        LocationOrigin::Env => "env",
        LocationOrigin::Discovered => "discovered",
    }
}

fn preview_path_label(source: &SourceLocation) -> String {
    source
        .path_label
        .as_deref()
        .map(abbreviate_home)
        .unwrap_or_else(|| "unknown".to_string())
}

fn abbreviate_home(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    path.strip_prefix(home.as_ref())
        .map(|rest| format!("~{rest}"))
        .unwrap_or_else(|| path.to_string())
}

fn format_u64(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_cost(cost: Option<f64>) -> String {
    cost.map(|cost| format!("${cost:.2}"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn sanitize_source_for_sync(mut source: SourceLocation) -> SourceLocation {
    source.path_label = None;
    source.account_hint = None;
    source
}

fn sanitize_account_for_sync(mut account: ProviderAccount) -> ProviderAccount {
    account.account_label = None;
    account.plan_name = None;
    account
}

fn sanitize_event_for_sync(mut event: UsageEvent) -> UsageEvent {
    event.source.source_record_id = None;
    if let Some(evidence) = event.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    event
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct FirestoreStatsAccumulator {
    provider: String,
    source_id: ai_stats_core::SourceId,
    provider_account_id: Option<ai_stats_core::ProviderAccountId>,
    source: EventSource,
    period_start: DateTime<Utc>,
    period_end: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    account_key: String,
    day_key: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    events: u64,
    estimated_cost_usd: f64,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FirestoreStatsBucketKey {
    provider: String,
    source_id: String,
    account_key: String,
    day_key: String,
}

#[cfg(test)]
fn firestore_stats_bucket_key(event: &UsageEvent) -> FirestoreStatsBucketKey {
    FirestoreStatsBucketKey {
        provider: event.provider.clone(),
        source_id: event.source_id.0.clone(),
        account_key: event
            .provider_account_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| "unlinked".to_string()),
        day_key: event.session.started_at.date_naive().to_string(),
    }
}

#[cfg(test)]
fn build_firestore_stats_summaries(events: &[UsageEvent], device_id: &str) -> Vec<UsageSummary> {
    let mut buckets: BTreeMap<String, FirestoreStatsAccumulator> = BTreeMap::new();
    for event in events {
        let key = firestore_stats_bucket_key(event);
        let day = event.session.started_at.date_naive();
        let start = day
            .and_hms_opt(0, 0, 0)
            .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
            .unwrap_or(event.session.started_at);
        let end = start + chrono::Duration::days(1);
        let entry = buckets
            .entry(format!(
                "{}|{}|{}|{}",
                key.provider, key.source_id, key.account_key, key.day_key
            ))
            .or_insert_with(|| FirestoreStatsAccumulator {
                provider: event.provider.clone(),
                source_id: event.source_id.clone(),
                provider_account_id: event.provider_account_id.clone(),
                source: event.source.clone(),
                period_start: start,
                period_end: end,
                observed_at: event.session.started_at,
                account_key: key.account_key.clone(),
                day_key: key.day_key.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                events: 0,
                estimated_cost_usd: 0.0,
            });
        entry.input_tokens = entry
            .input_tokens
            .saturating_add(event.usage.input_tokens.unwrap_or(0));
        entry.output_tokens = entry
            .output_tokens
            .saturating_add(event.usage.output_tokens.unwrap_or(0));
        entry.cache_creation_tokens = entry
            .cache_creation_tokens
            .saturating_add(event.usage.cache_creation_tokens.unwrap_or(0));
        entry.cache_read_tokens = entry
            .cache_read_tokens
            .saturating_add(event.usage.cache_read_tokens.unwrap_or(0));
        entry.reasoning_tokens = entry
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens.unwrap_or(0));
        entry.total_tokens = entry
            .total_tokens
            .saturating_add(event.usage.computed_total());
        entry.events = entry.events.saturating_add(1);
        entry.estimated_cost_usd += event.cost.estimated_api_equivalent_usd.unwrap_or(0.0);
        if event.session.started_at > entry.observed_at {
            entry.observed_at = event.session.started_at;
        }
    }

    buckets
        .into_values()
        .map(|bucket| UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(
                &bucket.provider,
                &bucket.source_id,
                &format!("daily_stats:{}:{}", bucket.day_key, bucket.account_key),
            ),
            device_id: device_id.to_string(),
            provider: bucket.provider,
            source_id: bucket.source_id,
            provider_account_id: bucket.provider_account_id,
            source: EventSource {
                source_record_id: None,
                ..bucket.source
            },
            model: None,
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(bucket.input_tokens),
                output_tokens: Some(bucket.output_tokens),
                cache_creation_tokens: Some(bucket.cache_creation_tokens),
                cache_read_tokens: Some(bucket.cache_read_tokens),
                reasoning_tokens: Some(bucket.reasoning_tokens),
                total_tokens: Some(bucket.total_tokens),
                requests: Some(bucket.events),
                local_prompt_eval_tokens: None,
                local_eval_tokens: None,
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: Some(bucket.estimated_cost_usd),
                provider_reported_usd: None,
                pricing_source: Some("local_rollup".to_string()),
                pricing_version: None,
                confidence: Confidence::Medium,
            },
            parse_evidence: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            period_start: Some(bucket.period_start),
            period_end: Some(bucket.period_end),
            observed_at: bucket.observed_at,
            metadata: SummaryMetadata {
                summary_format: "daily_rollup.v1".to_string(),
                summary_version: Some("2".to_string()),
                total_sessions: None,
                total_messages: None,
                last_computed_at: Some(Utc::now()),
            },
            imported_at: Utc::now(),
        })
        .collect()
}

fn sanitize_summary_for_sync(mut summary: UsageSummary) -> UsageSummary {
    summary.source.source_record_id = None;
    if let Some(evidence) = summary.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    summary
}

fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

fn sanitize_subscription_for_sync(mut subscription: Subscription) -> Subscription {
    subscription.notes = None;
    subscription
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_stats_core::{
        event_id, subscription_id, summary_id, BillingPeriod, CostInfo, EventSource,
        IdentitySource, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode, ProviderAccount,
        SessionInfo, SourceKind, Subscription, SubscriptionStatus, SummaryMetadata, UsageCounts,
        UsageSummary, PROVIDER_ACCOUNT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use chrono::TimeZone;
    use std::path::Path;

    #[derive(Clone)]
    struct TestAdapter {
        provider: &'static str,
        discovered: Vec<SourceLocation>,
    }

    impl ProviderAdapter for TestAdapter {
        fn id(&self) -> &'static str {
            "test"
        }

        fn version(&self) -> &'static str {
            "0"
        }

        fn provider(&self) -> &'static str {
            self.provider
        }

        fn discover(&self) -> Vec<SourceLocation> {
            self.discovered.clone()
        }

        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<ai_stats_adapters::AdapterScan> {
            Ok(ai_stats_adapters::AdapterScan::default())
        }
    }

    #[test]
    fn provider_aliases_match_canonical_provider() {
        assert!(provider_matches("claude_code", "claude"));
        assert!(provider_matches("claude-code", "claude_code"));
        assert!(provider_matches("codex", "codex"));
        assert_eq!(
            canonical_provider("claude").expect("provider"),
            "claude_code"
        );
    }

    #[test]
    fn sync_sanitization_removes_record_level_evidence() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/.codex"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let mut event = test_event("codex", &source, now, None, TokenParts::total(100));
        event.source.source_record_id = Some("/tmp/.codex/sessions/log.jsonl:12".to_string());
        event.parse_evidence = Some(ParseEvidence {
            event_key_version: "test.v1".to_string(),
            source_file_path_hash: Some("hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some("/tmp/.codex/sessions/log.jsonl:12".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::ManualHint,
        });

        let mut summary = test_summary("codex", &source, now, 100);
        summary.source.source_record_id = Some("reported_jul11.json:daily:2025-07-11".to_string());
        summary.parse_evidence = event.parse_evidence.clone();

        let event = sanitize_event_for_sync(event);
        let summary = sanitize_summary_for_sync(summary);

        assert!(event.source.source_record_id.is_none());
        let event_evidence = event.parse_evidence.expect("event evidence");
        assert!(event_evidence.source_record_id.is_none());
        assert!(event_evidence.source_line_number.is_none());
        assert_eq!(
            event_evidence.source_file_path_hash.as_deref(),
            Some("hash")
        );

        assert!(summary.source.source_record_id.is_none());
        let summary_evidence = summary.parse_evidence.expect("summary evidence");
        assert!(summary_evidence.source_record_id.is_none());
        assert!(summary_evidence.source_line_number.is_none());
        assert_eq!(
            summary_evidence.source_file_path_hash.as_deref(),
            Some("hash")
        );
    }

    #[test]
    fn replace_matching_summaries_targets_reported_imports_only() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "external-report",
            None,
            Some("personal".to_string()),
        );
        let local_source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0",
            Path::new("/tmp/.claude"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let mut reported = test_summary("claude_code", &source, now, 100);
        reported.source.source_kind = SourceKind::ExternalReport;
        reported.metadata.summary_format = "external_daily".to_string();
        let mut local = test_summary("claude_code", &local_source, now, 200);
        local.source.source_kind = SourceKind::LocalSummary;
        local.metadata.summary_format = "external_daily".to_string();
        store.upsert_summary(&reported).expect("reported summary");
        store.upsert_summary(&local).expect("local summary");

        let record = ReportedUsageSummaryRecord {
            source,
            summary: reported.clone(),
        };
        let report = ReportedImportReport {
            path: PathBuf::from("reported_usage_summaries.json"),
            records: vec![record],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");
        assert_eq!(matches, vec![reported.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_is_scoped_to_source_and_period() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "reported-file-a",
            None,
            Some("personal".to_string()),
        );
        let other_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "reported-file-b",
            None,
            Some("personal".to_string()),
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");

        let mut matching = test_summary("claude_code", &source, now, 100);
        matching.source.source_kind = SourceKind::ExternalReport;
        matching.metadata.summary_format = "external_daily".to_string();
        matching.period_start = Some(now - Duration::days(1));
        matching.period_end = Some(now);

        let mut same_file_different_day = test_summary("claude_code", &source, now, 200);
        same_file_different_day.summary_id =
            summary_id("claude_code", &source.source_id, "other-day");
        same_file_different_day.source.source_kind = SourceKind::ExternalReport;
        same_file_different_day.metadata.summary_format = "external_daily".to_string();
        same_file_different_day.period_start = Some(now - Duration::days(2));
        same_file_different_day.period_end = Some(now - Duration::days(1));

        let mut same_period_different_file = test_summary("claude_code", &other_source, now, 300);
        same_period_different_file.source.source_kind = SourceKind::ExternalReport;
        same_period_different_file.metadata.summary_format = "external_daily".to_string();
        same_period_different_file.period_start = matching.period_start;
        same_period_different_file.period_end = matching.period_end;

        store.upsert_summary(&matching).expect("matching summary");
        store
            .upsert_summary(&same_file_different_day)
            .expect("same file different day");
        store
            .upsert_summary(&same_period_different_file)
            .expect("same period different file");

        let incoming = ReportedUsageSummaryRecord {
            source,
            summary: matching.clone(),
        };
        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![matching.summary_id]);
    }

    #[test]
    fn configured_claude_projects_path_normalizes_to_config_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).expect("projects");

        let normalized =
            normalize_configured_source_path("claude_code", &projects).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn account_resolve_merges_sources_with_same_account_hint() {
        let store = Store::in_memory().expect("store");
        let source_a = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-a"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let source_b = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-b"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        store.upsert_source(&source_a).expect("source a");
        store.upsert_source(&source_b).expect("source b");

        account(
            AccountCommand {
                command: AccountSubcommand::Resolve {
                    provider: "claude".to_string(),
                },
            },
            &store,
        )
        .expect("resolve");

        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].provider, "claude_code");
        assert_eq!(accounts[0].source_ids.len(), 2);
    }

    #[test]
    fn subscription_add_uses_canonical_provider_for_account_id() {
        let store = Store::in_memory().expect("store");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Add {
                    provider: "claude".to_string(),
                    account: Some("personal".to_string()),
                    plan: "Pro".to_string(),
                    price: 20.0,
                    currency: "USD".to_string(),
                    paid_at: Some("2026-05-15".to_string()),
                },
            },
            &store,
        )
        .expect("subscription");

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].provider, "claude_code");
        assert_eq!(
            subscriptions[0].provider_account_id,
            Some(provider_account_id("claude_code", "personal"))
        );
    }

    #[test]
    fn persist_source_upserts_into_store() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/ai-stats-preview-source"),
            LocationOrigin::Configured,
            None,
        );

        persist_source_after_preview(&store, &source).expect("persist");

        assert_eq!(store.list_sources().expect("sources").len(), 1);
    }

    #[test]
    fn configured_source_overrides_discovered_source_for_same_path() {
        let discovered = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-merge"),
            LocationOrigin::Default,
            None,
        );
        let configured = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-merge"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered],
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
        assert_eq!(sources[0].account_hint.as_deref(), Some("personal"));
    }

    #[test]
    fn configured_parent_source_suppresses_discovered_child_source() {
        let discovered = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/ai-stats-claude/projects"),
            LocationOrigin::Default,
            None,
        );
        let configured = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/ai-stats-claude"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![discovered],
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
        assert_eq!(sources[0].account_hint.as_deref(), Some("personal"));
        assert_eq!(
            sources[0].path_label.as_deref(),
            Some("/tmp/ai-stats-claude")
        );
    }

    #[test]
    fn non_local_sources_are_ignored_for_adapter_scans() {
        let configured_local = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-local"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let configured_manual = SourceLocation::reported_usage(
            "codex",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "manual-note",
            None,
            Some("personal".to_string()),
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: Vec::new(),
        };

        let sources =
            scan_sources_for_adapter(&adapter, &[configured_local.clone(), configured_manual]);

        assert_eq!(sources, vec![configured_local]);
    }

    #[test]
    fn apply_account_hint_attaches_manual_identity() {
        let adapter = adapter_for_provider("codex").expect("adapter");
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::write(
            sessions.join("session.jsonl"),
            "{\"timestamp\":\"2026-05-24T00:00:00Z\",\"session_id\":\"session\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n",
        )
        .expect("fixture");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );

        let mut scan = adapter
            .scan(
                &source,
                &ScanOptions {
                    device_id: "device".to_string(),
                    selected_cache_keys: None,
                },
            )
            .expect("scan");
        apply_account_hint_to_events(&source, &mut scan.events);

        assert_eq!(scan.events.len(), 1);
        assert_eq!(
            scan.events[0].provider_account_id,
            Some(provider_account_id("codex", "personal"))
        );
        assert_eq!(
            scan.events[0]
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.account_identity_source.clone()),
            Some(IdentitySource::ManualHint)
        );
        assert_eq!(scan.events[0].usage.computed_total(), 15);
    }

    #[test]
    fn preview_path_label_abbreviates_home_paths() {
        let Some(home) = home_dir() else {
            return;
        };
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            &home.join(".codex"),
            LocationOrigin::Default,
            None,
        );

        assert!(preview_path_label(&source).starts_with("~/.codex"));
    }

    #[test]
    fn dry_run_sync_does_not_write_file() {
        let store = Store::in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("batch.json");

        sync(
            SyncCommand {
                sink: "file".to_string(),
                output: Some(output.clone()),
                endpoint: None,
                auth_token: None,
                firebase_uid: None,
                firebase_project: "ai-stats-fire".to_string(),
                firestore_mode: "stats".to_string(),
                rebuild_rollups: false,
                firestore_records_per_batch: 1000,
                firestore_commit_writes: 200,
                firestore_retries: 4,
                firestore_backoff_ms: 800,
                since_last: false,
                status: false,
                verify: false,
                dry_run: true,
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert!(!output.exists());
    }

    #[test]
    fn no_cache_scan_reselects_unchanged_files() {
        let store = Store::in_memory().expect("store");
        let source_id = ai_stats_core::SourceId("src-no-cache".to_string());
        let entries = vec![
            ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            },
            ScanFileStateEntry {
                cache_key: "/tmp/b.jsonl".to_string(),
                cache_signature: "sig-b-1".to_string(),
            },
        ];

        let initial = select_scan_file_entries(&store, &source_id, &entries, false, false)
            .expect("initial selection");
        assert_eq!(initial, entries);
        store
            .record_scan_file_entries(&source_id, &entries)
            .expect("record cache state");

        let default_selection =
            select_scan_file_entries(&store, &source_id, &entries, false, false)
                .expect("default selection");
        assert!(default_selection.is_empty());

        let no_cache_selection =
            select_scan_file_entries(&store, &source_id, &entries, false, true)
                .expect("no-cache selection");
        assert_eq!(no_cache_selection, entries);

        let replace_selection = select_scan_file_entries(&store, &source_id, &entries, true, false)
            .expect("replace selection");
        assert_eq!(replace_selection, entries);
    }

    #[test]
    fn firestore_sync_target_is_scoped_to_uid() {
        assert_eq!(
            firestore_sync_target("ai-stats-fire", "uid-123"),
            "firestore:ai-stats-fire:uid-123"
        );
    }

    #[test]
    fn firestore_verify_pending_counts_match_sanitized_sync_payloads() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verify-pending"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        store.upsert_source(&source).expect("source");

        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: provider_account_id("codex", "personal"),
            provider: "codex".to_string(),
            identity_source: IdentitySource::ManualHint,
            provider_user_id_hash: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: Some("Pro".to_string()),
            confidence: Confidence::High,
            source_ids: vec![source.source_id.clone()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_account(&account).expect("account");

        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id("codex", Some(&account.provider_account_id), "pro"),
            provider: "codex".to_string(),
            provider_account_id: Some(account.provider_account_id.clone()),
            source_ids: vec![source.source_id.clone()],
            plan_name: "Pro".to_string(),
            price: 20.0,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: None,
            renewal_day: None,
            started_at: None,
            ended_at: None,
            status: SubscriptionStatus::Active,
            notes: Some("private note".to_string()),
        };
        store
            .upsert_subscription(&subscription)
            .expect("subscription");
        let summary = test_summary("codex", &source, Utc::now(), 42);
        store.upsert_summary(&summary).expect("summary");

        let target = firestore_sync_target("ai-stats-fire", "uid-test");
        store
            .record_sources_synced(
                "firestore",
                &target,
                &[sanitize_source_for_sync(source.clone())],
            )
            .expect("record sources");
        store
            .record_accounts_synced(
                "firestore",
                &target,
                &[sanitize_account_for_sync(account.clone())],
            )
            .expect("record accounts");
        store
            .record_subscriptions_synced(
                "firestore",
                &target,
                &[sanitize_subscription_for_sync(subscription.clone())],
            )
            .expect("record subscriptions");
        store
            .record_summaries_synced(
                "firestore",
                &target,
                &[sanitize_summary_for_sync(summary.clone())],
            )
            .expect("record summaries");

        let local = firestore_local_verify(&store, &target, None).expect("local verify");
        assert_eq!(local.pending_sources, 0);
        assert_eq!(local.pending_accounts, 0);
        assert_eq!(local.pending_subscriptions, 0);
        assert_eq!(local.total_passthrough_summaries, 1);
        assert_eq!(local.pending_passthrough_summaries, 0);
    }

    #[test]
    fn firestore_stats_summaries_roll_up_events_by_day_and_account() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-firestore-stats"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let account = provider_account_id("codex", "personal");
        let day1_a = Utc
            .with_ymd_and_hms(2026, 5, 20, 10, 0, 0)
            .single()
            .expect("day1a");
        let day1_b = Utc
            .with_ymd_and_hms(2026, 5, 20, 11, 0, 0)
            .single()
            .expect("day1b");
        let day2 = Utc
            .with_ymd_and_hms(2026, 5, 21, 9, 0, 0)
            .single()
            .expect("day2");

        let summaries = build_firestore_stats_summaries(
            &[
                test_event(
                    "codex",
                    &source,
                    day1_a,
                    Some(account.clone()),
                    TokenParts {
                        input: 10,
                        output: 5,
                        cached_input: 0,
                        reasoning: 0,
                        total: 15,
                        cost: Some(0.10),
                    },
                ),
                test_event(
                    "codex",
                    &source,
                    day1_b,
                    Some(account.clone()),
                    TokenParts {
                        input: 20,
                        output: 10,
                        cached_input: 0,
                        reasoning: 0,
                        total: 30,
                        cost: Some(0.30),
                    },
                ),
                test_event(
                    "codex",
                    &source,
                    day2,
                    Some(account),
                    TokenParts {
                        input: 7,
                        output: 3,
                        cached_input: 0,
                        reasoning: 0,
                        total: 10,
                        cost: Some(0.05),
                    },
                ),
            ],
            "device",
        );

        assert_eq!(summaries.len(), 2);
        let total_tokens: u64 = summaries
            .iter()
            .map(|summary| summary.usage.total_tokens.unwrap_or(0))
            .sum();
        assert_eq!(total_tokens, 55);
        assert!(summaries
            .iter()
            .all(|summary| summary.metadata.summary_format == "daily_rollup.v1"));
    }

    #[test]
    fn usage_report_filters_period_and_groups_by_source_account_hint() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let recent = test_event(
            "codex",
            &source,
            now - Duration::days(1),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 70,
                cached_input: 20,
                output: 25,
                reasoning: 5,
                total: 100,
                cost: Some(0.0004),
            },
        );
        let old = test_event(
            "codex",
            &source,
            now - Duration::days(10),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 120,
                cached_input: 30,
                output: 50,
                reasoning: 0,
                total: 200,
                cost: Some(0.0008),
            },
        );

        let report = build_usage_report(
            &[recent, old],
            &[],
            &[source],
            &[],
            ReportPeriod::LastDays(7),
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
        assert_eq!(report.total_usage.input_tokens, 70);
        assert_eq!(report.total_usage.cached_input_tokens, 20);
        assert_eq!(report.total_usage.output_tokens, 25);
        assert_eq!(report.total_usage.reasoning_tokens, 5);
        assert_eq!(report.total_usage.estimated_cost_usd, Some(0.0004));
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "personal");
    }

    #[test]
    fn usage_report_uses_account_registry_label() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-account"),
            LocationOrigin::Configured,
            None,
        );
        let account_id = provider_account_id("codex", "stable-provider-id");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id_hash: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("work".to_string()),
            plan_name: None,
            confidence: Confidence::Medium,
            source_ids: vec![source.source_id.clone()],
            created_at: now,
            updated_at: now,
        };
        let event = test_event(
            "codex",
            &source,
            now,
            Some(account_id),
            TokenParts::total(50),
        );

        let report = build_usage_report(
            &[event],
            &[],
            &[source],
            &[account],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "work");
        assert_eq!(report.rows[0].usage.total_tokens, 50);
    }

    #[test]
    fn usage_report_keeps_summary_cache_separate_from_event_totals() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-report-summary"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let event = test_event(
            "claude_code",
            &source,
            now,
            Some(provider_account_id("claude_code", "personal")),
            TokenParts::total(100),
        );
        let summary = test_summary("claude_code", &source, now, 500);

        let report = build_usage_report(
            &[event],
            &[summary],
            std::slice::from_ref(&source),
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.total_usage.total_tokens, 100);
        assert_eq!(report.total_summary_usage.total_tokens, 500);
        assert_eq!(report.summary_rows.len(), 1);
        assert_eq!(report.summary_rows[0].account, "personal");
        assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);

        let weekly = build_usage_report(
            &[],
            &[test_summary("claude_code", &source, now, 500)],
            std::slice::from_ref(&source),
            &[],
            ReportPeriod::LastDays(7),
            now,
        );
        assert!(weekly.summary_rows.is_empty());
    }

    #[test]
    fn usage_report_keeps_summary_formats_separate() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-report-summary-kinds"),
            LocationOrigin::Configured,
            Some("personal".to_string()),
        );
        let mut stats_cache = test_summary("claude_code", &source, now, 500);
        stats_cache.metadata.summary_format = "claude_stats_cache".to_string();
        let mut external = test_summary("claude_code", &source, now, 300);
        external.summary_id = summary_id("claude_code", &source.source_id, "external");
        external.metadata.summary_format = "external_daily".to_string();

        let report = build_usage_report(
            &[],
            &[stats_cache, external],
            std::slice::from_ref(&source),
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.summary_rows.len(), 2);
        assert!(report
            .summary_rows
            .iter()
            .any(|row| row.kind == "claude_stats_cache" && row.usage.total_tokens == 500));
        assert!(report
            .summary_rows
            .iter()
            .any(|row| row.kind == "external_daily" && row.usage.total_tokens == 300));
    }

    struct TokenParts {
        input: u64,
        cached_input: u64,
        output: u64,
        reasoning: u64,
        total: u64,
        cost: Option<f64>,
    }

    impl TokenParts {
        fn total(total: u64) -> Self {
            Self {
                input: 0,
                cached_input: 0,
                output: 0,
                reasoning: 0,
                total,
                cost: None,
            }
        }
    }

    fn test_event(
        provider: &str,
        source: &SourceLocation,
        started_at: DateTime<Utc>,
        provider_account_id: Option<ai_stats_core::ProviderAccountId>,
        tokens: TokenParts,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id(
                provider,
                &source.source_id,
                &started_at.to_rfc3339(),
                None,
                started_at,
            ),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some(started_at.to_rfc3339()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "session".to_string(),
                local_session_id_hash: None,
                title: None,
                started_at,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                input_tokens: (tokens.input > 0).then_some(tokens.input),
                output_tokens: (tokens.output > 0).then_some(tokens.output),
                cache_read_tokens: (tokens.cached_input > 0).then_some(tokens.cached_input),
                reasoning_tokens: (tokens.reasoning > 0).then_some(tokens.reasoning),
                total_tokens: Some(tokens.total),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: tokens.cost,
                provider_reported_usd: None,
                pricing_source: Some("unknown".to_string()),
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            project: None,
            git: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            created_at: started_at,
            imported_at: started_at,
        }
    }

    fn test_summary(
        provider: &str,
        source: &SourceLocation,
        now: DateTime<Utc>,
        total: u64,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(provider, &source.source_id, "summary"),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: source
                .account_hint
                .as_deref()
                .map(|hint| provider_account_id(provider, hint)),
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some("summary".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: Some(ModelInfo {
                name: Some("claude-test".to_string()),
                normalized_name: Some("claude-test".to_string()),
                provider_model_id: Some("claude-test".to_string()),
            }),
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(total),
                total_tokens: Some(total),
                ..UsageCounts::default()
            },
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: None,
                provider_reported_usd: None,
                pricing_source: Some("unknown".to_string()),
                pricing_version: None,
                confidence: Confidence::Low,
            },
            parse_evidence: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            period_start: Some(now - Duration::days(30)),
            period_end: Some(now),
            observed_at: now,
            metadata: SummaryMetadata {
                summary_format: "test".to_string(),
                summary_version: Some("1".to_string()),
                total_sessions: Some(1),
                total_messages: Some(2),
                last_computed_at: Some(now),
            },
            imported_at: now,
        }
    }
}
