use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::Duration;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::{json, Value};
#[cfg(test)]
use statsai_adapters::VerifiedSubscriptionState;
use statsai_adapters::{
    adapter_for_provider, default_adapters, ProviderAdapter, ScanCandidateFile, ScanDiagnostics,
    ScanOptions, VerifiedSourceState,
};
use statsai_core::{
    build_usage_report, display_account_identity, expand_home_path, hash_text, home_dir,
    normalize_email, normalize_provider_user_id, path_hash, periods_overlap,
    source_account_assignment_id, subscription_id, timestamp_in_period, BillingPeriod,
    IdentitySource, LocationOrigin, ProviderAccount, ProviderAccountId, ReportPeriod,
    SourceAccountAssignment, SourceAccountAssignmentId, SourceId, SourceKind, SourceLocation,
    SourceVerificationMode, Subscription, SubscriptionId, SubscriptionStatus, SyncBatch,
    UsageEvent, UsageReport, UsageSummary, UsageTotals, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
    SUBSCRIPTION_SCHEMA_VERSION, SYNC_BATCH_SCHEMA_VERSION,
};
#[cfg(test)]
use statsai_core::{
    provider_account_id, provider_account_id_from_identity, summary_id, Confidence, CostInfo,
    EventSource, PrivacyInfo, PrivacyMode, SummaryMetadata, UsageCounts,
    USAGE_SUMMARY_SCHEMA_VERSION,
};
use statsai_sdk::{
    build_reported_usage_summary, ReportedUsageSummaryInput, ReportedUsageSummaryRecord,
};
#[cfg(test)]
use statsai_store::apply_verified_source_state;
use statsai_store::{
    close_active_verified_source_linkages, effective_verified_source_state_is_missing,
    find_existing_provider_account, has_active_verified_source_assignment,
    reconcile_verified_source_state, upsert_provider_account, verified_source_state_hash,
    ScanFileStateEntry, Store, SyncState, UpsertProviderAccountInput,
};
use statsai_sync::{FileSink, HttpSink, StdoutSink, SyncSink};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

mod auth;

const HTTP_ROLLUP_SUMMARIES_PER_BATCH: usize = 25;
const HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH: usize = 20;
const HTTP_ROLLUP_D1_QUERY_BUDGET: usize = 45;
const HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE: usize = 90;
const HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY: usize = 7;

#[derive(Debug, Parser)]
#[command(
    name = "statsai",
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
    #[command(about = "List canonical provider accounts")]
    Account(AccountCommand),
    #[command(about = "Manage subscription periods")]
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
    #[command(about = "Authenticate with the hosted sync backend")]
    Auth(AuthCommand),
}

#[derive(Debug, Args)]
struct AuthCommand {
    #[command(subcommand)]
    command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
    #[command(about = "Log in to the hosted sync backend")]
    Login {
        #[arg(long, help = "Print the local-browser URL without opening it")]
        no_open: bool,
        #[arg(
            long,
            help = "Use cross-device login for SSH, servers, and headless shells"
        )]
        headless: bool,
        #[arg(long, help = "Friendly name to show for this device")]
        device_name: Option<String>,
    },
    #[command(about = "Check authentication status for the Better Auth device session")]
    Status,
    #[command(about = "Log out and clear stored Better Auth device credentials")]
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
        #[arg(long, help = "Include subscription-value rows")]
        subscriptions: bool,
    },
    #[command(about = "Show usage for the last 30 days")]
    Monthly {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
        #[arg(long, help = "Include subscription-value rows")]
        subscriptions: bool,
    },
    #[command(about = "Show all stored usage")]
    AllTime {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
        #[arg(long, help = "Include subscription-value rows")]
        subscriptions: bool,
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
    #[command(about = "Connect a source to an account for a time period")]
    Connect {
        #[arg(long, help = "Source identifier to attach")]
        source_id: Option<String>,
        #[arg(long, help = "Local source path to attach")]
        path: Option<PathBuf>,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Display label for this account")]
        label: Option<String>,
        #[arg(long, help = "Assignment start date/time (YYYY-MM-DD or RFC 3339)")]
        started_at: String,
        #[arg(long, help = "Assignment end date/time (exclusive)")]
        ended_at: Option<String>,
    },
    #[command(about = "Show source-to-account connection history")]
    History {
        #[arg(long, help = "Optional source identifier filter")]
        source_id: Option<String>,
        #[arg(long, help = "Optional local source path filter")]
        path: Option<PathBuf>,
    },
    #[command(about = "Set auth verification mode for a source")]
    Mode {
        #[arg(long, help = "Source identifier to update")]
        source_id: Option<String>,
        #[arg(long, help = "Local source path to update")]
        path: Option<PathBuf>,
        #[arg(long, help = "Verification mode (auto, manual_only, disabled)")]
        mode: String,
    },
    #[command(about = "End the active source connection and leave future usage unassigned")]
    Unassign {
        #[arg(long, help = "Source identifier to unassign")]
        source_id: Option<String>,
        #[arg(long, help = "Local source path to unassign")]
        path: Option<PathBuf>,
        #[arg(long, help = "Unassign from this timestamp forward (defaults to now)")]
        at: Option<String>,
    },
    #[command(about = "Explain how a source is currently attributed")]
    Explain {
        #[arg(long, help = "Source identifier to inspect")]
        source_id: Option<String>,
        #[arg(long, help = "Local source path to inspect")]
        path: Option<PathBuf>,
    },
    #[command(about = "End the active source-to-account connection")]
    Disconnect {
        #[arg(long, help = "Source identifier to disconnect")]
        source_id: Option<String>,
        #[arg(long, help = "Local source path to disconnect")]
        path: Option<PathBuf>,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(
            long,
            help = "End the current connection at this timestamp (exclusive)"
        )]
        ended_at: String,
    },
}

#[derive(Debug, Args)]
struct AccountCommand {
    #[command(subcommand)]
    command: AccountSubcommand,
}

#[derive(Debug, Subcommand)]
enum AccountSubcommand {
    #[command(about = "List canonical provider accounts")]
    List,
    #[command(about = "Merge a legacy/manual account into an existing canonical account")]
    Merge {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(
            long,
            help = "Source account identity (label, email, provider user id, or provider account id)"
        )]
        from: String,
        #[arg(
            long,
            help = "Destination account identity (label, email, provider user id, or provider account id)"
        )]
        to: String,
        #[arg(long, help = "Preview the cleanup without writing")]
        dry_run: bool,
    },
    #[command(about = "Remove an unreferenced account row")]
    Remove {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(
            long,
            help = "Account identity to delete (label, email, provider user id, or provider account id)"
        )]
        account: String,
        #[arg(long, help = "Preview the cleanup without writing")]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
struct SubscriptionCommand {
    #[command(subcommand)]
    command: SubscriptionSubcommand,
}

#[derive(Debug, Subcommand)]
enum SubscriptionSubcommand {
    #[command(about = "Register a subscription period")]
    Add {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Display label for this account")]
        label: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: String,
        #[arg(long, help = "Subscription price in the given currency")]
        price: f64,
        #[arg(long, default_value = "USD", help = "Currency code")]
        currency: String,
        #[arg(long, help = "Date the subscription was paid (YYYY-MM-DD or RFC 3339)")]
        paid_at: Option<String>,
        #[arg(long, help = "Subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: String,
        #[arg(long, help = "Subscription period end (exclusive)")]
        ended_at: Option<String>,
    },
    #[command(about = "Change to a new subscription period and close the current one")]
    Change {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Display label for this account")]
        label: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: String,
        #[arg(long, help = "Subscription price in the given currency")]
        price: f64,
        #[arg(long, default_value = "USD", help = "Currency code")]
        currency: String,
        #[arg(long, help = "Date the subscription was paid (YYYY-MM-DD or RFC 3339)")]
        paid_at: Option<String>,
        #[arg(long, help = "New subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: String,
    },
    #[command(about = "End the active subscription period")]
    End {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Subscription period end (exclusive, defaults to now)")]
        ended_at: Option<String>,
    },
    #[command(about = "Remove a subscription period")]
    Remove {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: String,
        #[arg(long, help = "Existing provider account identifier")]
        provider_account_id: Option<String>,
        #[arg(long, help = "Canonical provider user/account identifier")]
        provider_user_id: Option<String>,
        #[arg(long, help = "Provider email for this account")]
        email: Option<String>,
        #[arg(long, help = "Plan name (e.g. Pro, Max, Team)")]
        plan: Option<String>,
        #[arg(long, help = "Subscription period start (YYYY-MM-DD or RFC 3339)")]
        started_at: Option<String>,
        #[arg(long, help = "Remove the active subscription period")]
        current: bool,
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
        help = "Sync sink (stdout, file, http)"
    )]
    sink: String,
    #[arg(long, help = "Output path for file sink")]
    output: Option<PathBuf>,
    #[arg(
        long,
        help = "HTTP endpoint for the http sink (defaults to STATSAI_API_URL/api/sync/batches)"
    )]
    endpoint: Option<String>,
    #[arg(long, help = "Bearer token override for the http sink")]
    auth_token: Option<String>,
    #[arg(
        long,
        help = "Rebuild local daily rollups from events and force all rollups dirty before sync"
    )]
    rebuild_rollups: bool,
    #[arg(
        long,
        help = "Send only records after this sink target's last successful sync"
    )]
    since_last: bool,
    #[arg(long, help = "Show recorded sync state instead of sending")]
    status: bool,
    #[arg(
        long,
        help = "Inspect the resolved Cloudflare sync target and verify remote device access"
    )]
    verify: bool,
    #[arg(
        long,
        help = "Delete mirrored hosted sync data for the current user and clear local sync tracking (http only)"
    )]
    reset_remote: bool,
    #[arg(long, help = "Confirm destructive sync reset actions")]
    yes: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPayloadMode {
    Raw,
    Rollups,
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
        AuthSubcommand::Login {
            no_open,
            headless,
            device_name,
        } => auth::login(no_open, headless, device_name),
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

    scan_with_adapters(command, store, device_id, adapters)
}

fn scan_with_adapters(
    command: ScanCommand,
    store: &Store,
    device_id: &str,
    adapters: Vec<Box<dyn ProviderAdapter>>,
) -> Result<()> {
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
            let verification_mode = source_verification_mode(&source);
            let probed_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    None
                } else {
                    adapter.probe_verified_source_state(&source)?
                };
            let next_verified_state_hash =
                if matches!(verification_mode, SourceVerificationMode::Auto) {
                    verified_source_state_hash(probed_verified_source_state.as_ref())?
                } else {
                    None
                };
            let verified_state_changed = matches!(verification_mode, SourceVerificationMode::Auto)
                && source.verified_state_hash != next_verified_state_hash;
            let legacy_verified_state_needs_reconciliation =
                matches!(verification_mode, SourceVerificationMode::Auto)
                    && source.verified_state_hash.is_none()
                    && next_verified_state_hash.is_none()
                    && effective_verified_source_state_is_missing(&probed_verified_source_state)
                    && has_active_verified_source_assignment(store, &source.source_id)?;
            let should_run_full_scan =
                command.no_cache || command.replace || !pending_file_entries.is_empty();
            let mut scan = if should_run_full_scan {
                adapter.scan(&source, &options)?
            } else {
                statsai_adapters::AdapterScan {
                    diagnostics: ScanDiagnostics {
                        files_skipped_unchanged: file_cache_entries.len() as u64,
                        ..ScanDiagnostics::default()
                    },
                    ..statsai_adapters::AdapterScan::default()
                }
            };
            let effective_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    None
                } else if should_run_full_scan {
                    scan.verified_source_state
                        .take()
                        .or(probed_verified_source_state)
                } else {
                    probed_verified_source_state
                };
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
                || log_rows > 0
                || verified_state_changed
                || legacy_verified_state_needs_reconciliation;
            let suppress_source_processing = !command.verbose
                && !command.explain
                && source_event_count == 0
                && source_summary_count == 0
                && !touched_files
                && !verified_state_changed
                && !legacy_verified_state_needs_reconciliation;

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
            reconcile_verified_source_state(
                store,
                &mut source,
                effective_verified_source_state.as_ref(),
                next_verified_state_hash,
            )?;
            persist_source_after_preview(store, &source)?;
            apply_source_account_resolution(store, &source, &mut scan.events, &mut scan.summaries)?;
            let replace_source_records = should_replace_source_records_for_scan(
                command.replace,
                should_run_full_scan,
                file_cache_entries.len(),
                pending_file_entries.len(),
            );
            if replace_source_records {
                replaced_event_count +=
                    store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
                replaced_summary_count +=
                    store.delete_summaries_for_sources(std::slice::from_ref(&source.source_id))?;
            }
            inserted_count += store.insert_events(&scan.events)?;
            summary_written_count += store.upsert_summaries(&scan.summaries)?;
            let cache_entries_to_record = if replace_source_records || command.no_cache {
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
        if command.replace || replaced_event_count > 0 || replaced_summary_count > 0 {
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
        SourceSubcommand::Add { provider, path } => {
            let adapter = adapter_for_provider(&provider)
                .with_context(|| format!("unsupported provider {provider}"))?;
            let path = normalize_configured_source_path(adapter.provider(), &path)?;
            let mut source = SourceLocation::local_adapter(
                adapter.provider(),
                adapter.id(),
                adapter.version(),
                &path,
                LocationOrigin::Configured,
            );
            source.path_label = Some(path.to_string_lossy().to_string());
            store.upsert_source(&source)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Enable { source_id } => {
            let source_id = statsai_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, true)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Disable { source_id } => {
            let source_id = statsai_core::SourceId(source_id);
            let source = store
                .set_source_enabled(&source_id, false)?
                .with_context(|| format!("unknown source {}", source_id.0))?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Remove {
            source_id,
            delete_data,
        } => {
            let source_id = statsai_core::SourceId(source_id);
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
        SourceSubcommand::Connect {
            source_id,
            path,
            provider_account_id,
            provider_user_id,
            email,
            label,
            started_at,
            ended_at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let assignment = connect_source_to_account(
                store,
                ConnectSourceToAccountInput {
                    source_id: &source.source_id,
                    provider_account_id_value: provider_account_id.as_deref(),
                    provider_user_id: provider_user_id.as_deref(),
                    email: email.as_deref(),
                    label,
                    started_at: parse_date(&started_at)?,
                    ended_at: ended_at.as_deref().map(parse_date).transpose()?,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
        SourceSubcommand::History { source_id, path } => {
            let assignments = if source_id.is_some() || path.is_some() {
                let source =
                    resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
                store.list_source_account_assignments_for_source(&source.source_id)?
            } else {
                store.list_source_account_assignments()?
            };
            println!("{}", serde_json::to_string_pretty(&assignments)?);
        }
        SourceSubcommand::Mode {
            source_id,
            path,
            mode,
        } => {
            let mut source =
                resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            source.verification_mode = parse_source_verification_mode(&mode)?;
            if !matches!(source.verification_mode, SourceVerificationMode::Auto) {
                source.verified_state_hash = None;
            }
            if matches!(source.verification_mode, SourceVerificationMode::Disabled) {
                close_active_verified_source_linkages(store, &source.source_id, Utc::now())?;
            }
            source.updated_at = Utc::now();
            store.upsert_source(&source)?;
            println!("{}", serde_json::to_string_pretty(&source)?);
        }
        SourceSubcommand::Unassign {
            source_id,
            path,
            at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let ended_at = at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .unwrap_or_else(Utc::now);
            let assignment = disconnect_source_from_account(
                store,
                &source.source_id,
                None,
                None,
                None,
                ended_at,
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
        SourceSubcommand::Explain { source_id, path } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let explanation = explain_source(store, &source)?;
            println!("{}", serde_json::to_string_pretty(&explanation)?);
        }
        SourceSubcommand::Disconnect {
            source_id,
            path,
            provider_account_id,
            provider_user_id,
            email,
            ended_at,
        } => {
            let source = resolve_source_reference(store, source_id.as_deref(), path.as_deref())?;
            let assignment = disconnect_source_from_account(
                store,
                &source.source_id,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                parse_date(&ended_at)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&assignment)?);
        }
    }
    Ok(())
}

fn account(command: AccountCommand, store: &Store) -> Result<()> {
    match command.command {
        AccountSubcommand::List => {
            println!("{}", serde_json::to_string_pretty(&store.list_accounts()?)?);
        }
        AccountSubcommand::Merge {
            provider,
            from,
            to,
            dry_run,
        } => {
            let report = merge_provider_accounts(
                store,
                &canonical_provider(&provider)?,
                &from,
                &to,
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        AccountSubcommand::Remove {
            provider,
            account,
            dry_run,
        } => {
            let report = remove_orphan_provider_account(
                store,
                &canonical_provider(&provider)?,
                &account,
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct AccountReferenceCounts {
    source_account_assignments: usize,
    subscriptions: usize,
    events: usize,
    summaries: usize,
}

impl AccountReferenceCounts {
    fn total(&self) -> usize {
        self.source_account_assignments + self.subscriptions + self.events + self.summaries
    }
}

#[derive(Debug, Serialize)]
struct AccountMergeReport {
    provider: String,
    from: String,
    to: String,
    from_provider_account_id: String,
    to_provider_account_id: String,
    moved_source_account_assignments: usize,
    moved_subscriptions: usize,
    moved_events: usize,
    moved_summaries: usize,
    deleted_source_account: bool,
    remaining_references: AccountReferenceCounts,
    reset_local_sync_tracking: bool,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct AccountRemoveReport {
    provider: String,
    account: String,
    provider_account_id: String,
    deleted: bool,
    remaining_references: AccountReferenceCounts,
    reset_local_sync_tracking: bool,
    dry_run: bool,
}

fn merge_provider_accounts(
    store: &Store,
    provider: &str,
    from_selector: &str,
    to_selector: &str,
    dry_run: bool,
) -> Result<AccountMergeReport> {
    let from = resolve_existing_provider_account_selector(store, provider, from_selector)?;
    let to = resolve_existing_provider_account_selector(store, provider, to_selector)?;
    if from.provider_account_id == to.provider_account_id {
        bail!("source and destination accounts are the same");
    }

    let assignments_to_move: Vec<_> = store
        .list_source_account_assignments()?
        .into_iter()
        .filter(|assignment| assignment.provider == provider)
        .filter(|assignment| assignment.provider_account_id == from.provider_account_id)
        .collect();
    let subscriptions_to_move: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| subscription.provider == provider)
        .filter(|subscription| subscription.provider_account_id == from.provider_account_id)
        .collect();
    let direct_events_to_move = store
        .events()?
        .into_iter()
        .filter(|event| event.provider == provider)
        .filter(|event| event.provider_account_id.as_ref() == Some(&from.provider_account_id))
        .count();
    let direct_summaries_to_move = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider == provider)
        .filter(|summary| summary.provider_account_id.as_ref() == Some(&from.provider_account_id))
        .count();

    if !dry_run {
        for assignment in &assignments_to_move {
            connect_source_to_account(
                store,
                ConnectSourceToAccountInput {
                    source_id: &assignment.source_id,
                    provider_account_id_value: Some(&to.provider_account_id.0),
                    provider_user_id: None,
                    email: None,
                    label: None,
                    started_at: assignment.started_at,
                    ended_at: assignment.ended_at,
                },
            )?;
        }
        for subscription in &subscriptions_to_move {
            move_subscription_to_account(store, subscription, &to.provider_account_id)?;
        }
        move_direct_account_records(
            store,
            provider,
            &from.provider_account_id,
            &to.provider_account_id,
        )?;
    }

    let remaining_references =
        account_reference_counts(store, &from.provider_account_id, Some(provider))?;
    let deleted_source_account = if !dry_run && remaining_references.total() == 0 {
        store.delete_account(&from.provider_account_id)?
    } else {
        false
    };
    if !dry_run {
        store.clear_sync_tracking()?;
    }

    Ok(AccountMergeReport {
        provider: provider.to_string(),
        from: display_account_identity(&from),
        to: display_account_identity(&to),
        from_provider_account_id: from.provider_account_id.0,
        to_provider_account_id: to.provider_account_id.0,
        moved_source_account_assignments: assignments_to_move.len(),
        moved_subscriptions: subscriptions_to_move.len(),
        moved_events: direct_events_to_move,
        moved_summaries: direct_summaries_to_move,
        deleted_source_account,
        remaining_references,
        reset_local_sync_tracking: !dry_run,
        dry_run,
    })
}

fn remove_orphan_provider_account(
    store: &Store,
    provider: &str,
    selector: &str,
    dry_run: bool,
) -> Result<AccountRemoveReport> {
    let account = resolve_existing_provider_account_selector(store, provider, selector)?;
    let remaining_references =
        account_reference_counts(store, &account.provider_account_id, Some(provider))?;
    if remaining_references.total() > 0 {
        bail!(
            "account {} still has references: {} source assignments, {} subscriptions, {} events, {} summaries",
            display_account_identity(&account),
            remaining_references.source_account_assignments,
            remaining_references.subscriptions,
            remaining_references.events,
            remaining_references.summaries
        );
    }
    let deleted = if dry_run {
        false
    } else {
        store.delete_account(&account.provider_account_id)?
    };
    if !dry_run {
        store.clear_sync_tracking()?;
    }

    Ok(AccountRemoveReport {
        provider: provider.to_string(),
        account: display_account_identity(&account),
        provider_account_id: account.provider_account_id.0,
        deleted,
        remaining_references,
        reset_local_sync_tracking: !dry_run,
        dry_run,
    })
}

fn resolve_existing_provider_account_selector(
    store: &Store,
    provider: &str,
    selector: &str,
) -> Result<ProviderAccount> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("account selector cannot be empty");
    }
    let normalized_email = normalize_email(selector);
    let normalized_provider_user_id = normalize_provider_user_id(selector);
    let normalized_label = selector.to_ascii_lowercase();

    let matches: Vec<_> = store
        .list_accounts()?
        .into_iter()
        .filter(|account| account.provider == provider)
        .filter(|account| {
            account.provider_account_id.0 == selector
                || account.email.as_deref().map(normalize_email).as_deref()
                    == Some(normalized_email.as_str())
                || account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(normalized_provider_user_id.as_str())
                || account
                    .account_label
                    .as_deref()
                    .map(|label| label.trim().to_ascii_lowercase())
                    .as_deref()
                    == Some(normalized_label.as_str())
        })
        .collect();

    match matches.len() {
        0 => bail!("no {provider} account matched '{selector}'"),
        1 => Ok(matches.into_iter().next().expect("single account")),
        _ => bail!("multiple {provider} accounts matched '{selector}'"),
    }
}

fn move_subscription_to_account(
    store: &Store,
    subscription: &Subscription,
    target_provider_account_id: &ProviderAccountId,
) -> Result<Subscription> {
    let moved_subscription_id = subscription_id(
        &subscription.provider,
        target_provider_account_id,
        &subscription.plan_name,
        subscription.started_at,
    );
    if moved_subscription_id != subscription.subscription_id {
        if let Some(existing) = store.subscription(&moved_subscription_id)? {
            if existing.provider == subscription.provider
                && existing.provider_account_id == *target_provider_account_id
                && existing.plan_name == subscription.plan_name
                && existing.price == subscription.price
                && existing.currency == subscription.currency
                && existing.billing_period == subscription.billing_period
                && existing.paid_at == subscription.paid_at
                && existing.renewal_day == subscription.renewal_day
                && existing.started_at == subscription.started_at
                && existing.ended_at == subscription.ended_at
                && existing.current_period_ends_at == subscription.current_period_ends_at
                && existing.status == subscription.status
            {
                store.delete_subscription(&subscription.subscription_id)?;
                return Ok(existing);
            }
            bail!(
                "subscription {} would collide with existing subscription {} on {}",
                subscription.subscription_id.0,
                moved_subscription_id.0,
                target_provider_account_id.0
            );
        }
    }

    validate_subscription_overlap(
        store,
        &subscription.provider,
        target_provider_account_id,
        subscription.started_at,
        subscription.ended_at,
        Some(&subscription.subscription_id),
    )?;

    let moved = Subscription {
        subscription_id: moved_subscription_id,
        provider_account_id: target_provider_account_id.clone(),
        ..subscription.clone()
    };
    if moved.subscription_id != subscription.subscription_id {
        store.delete_subscription(&subscription.subscription_id)?;
    }
    store.upsert_subscription(&moved)?;
    Ok(moved)
}

fn move_direct_account_records(
    store: &Store,
    provider: &str,
    from_provider_account_id: &ProviderAccountId,
    target_provider_account_id: &ProviderAccountId,
) -> Result<()> {
    let mut events_to_move: Vec<_> = store
        .events()?
        .into_iter()
        .filter(|event| event.provider == provider)
        .filter(|event| event.provider_account_id.as_ref() == Some(from_provider_account_id))
        .collect();
    for event in &mut events_to_move {
        event.provider_account_id = Some(target_provider_account_id.clone());
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::UserConfigured;
        }
    }
    if !events_to_move.is_empty() {
        store.rewrite_events(&events_to_move)?;
    }

    let mut summaries_to_move: Vec<_> = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider == provider)
        .filter(|summary| summary.provider_account_id.as_ref() == Some(from_provider_account_id))
        .collect();
    for summary in &mut summaries_to_move {
        summary.provider_account_id = Some(target_provider_account_id.clone());
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::UserConfigured;
        }
    }
    if !summaries_to_move.is_empty() {
        store.rewrite_summaries(&summaries_to_move)?;
    }

    Ok(())
}

fn account_reference_counts(
    store: &Store,
    provider_account_id: &ProviderAccountId,
    provider: Option<&str>,
) -> Result<AccountReferenceCounts> {
    let provider_matches =
        |row_provider: &str| provider.map(|value| value == row_provider).unwrap_or(true);
    let source_account_assignments = store
        .list_source_account_assignments()?
        .into_iter()
        .filter(|assignment| assignment.provider_account_id == *provider_account_id)
        .filter(|assignment| provider_matches(&assignment.provider))
        .count();
    let subscriptions = store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| subscription.provider_account_id == *provider_account_id)
        .filter(|subscription| provider_matches(&subscription.provider))
        .count();
    let events = store
        .events()?
        .into_iter()
        .filter(|event| event.provider_account_id.as_ref() == Some(provider_account_id))
        .filter(|event| provider_matches(&event.provider))
        .count();
    let summaries = store
        .summaries()?
        .into_iter()
        .filter(|summary| summary.provider_account_id.as_ref() == Some(provider_account_id))
        .filter(|summary| provider_matches(&summary.provider))
        .count();

    Ok(AccountReferenceCounts {
        source_account_assignments,
        subscriptions,
        events,
        summaries,
    })
}

fn subscription(command: SubscriptionCommand, store: &Store) -> Result<()> {
    match command.command {
        SubscriptionSubcommand::Add {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            label,
            plan,
            price,
            currency,
            paid_at,
            started_at,
            ended_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let account = resolve_or_create_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                label,
            )?;
            let started_at = parse_date(&started_at)?;
            let ended_at = ended_at.as_deref().map(parse_date).transpose()?;
            validate_time_window(started_at, ended_at, "subscription")?;
            validate_subscription_overlap(
                store,
                &provider,
                &account.provider_account_id,
                started_at,
                ended_at,
                None,
            )?;
            let paid_at = paid_at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .or(Some(started_at));
            let price_cents = (price * 100.0).round() as i64;
            let subscription = Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    &provider,
                    &account.provider_account_id,
                    &plan,
                    started_at,
                ),
                provider: provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: plan,
                price: price_cents,
                currency,
                billing_period: BillingPeriod::Monthly,
                paid_at,
                renewal_day: paid_at.and_then(subscription_renewal_day),
                started_at,
                ended_at,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            };
            store.upsert_subscription(&subscription)?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::Change {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            label,
            plan,
            price,
            currency,
            paid_at,
            started_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                label,
            )?;
            let started_at = parse_date(&started_at)?;
            if close_active_subscription(
                store,
                &provider,
                &account.provider_account_id,
                started_at,
            )?
            .is_none()
            {
                bail!(
                    "subscription change requires an active subscription for account {} at {}",
                    account.provider_account_id.0,
                    started_at.to_rfc3339()
                );
            }
            let paid_at = paid_at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .or(Some(started_at));
            let price_cents = (price * 100.0).round() as i64;
            let subscription = Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    &provider,
                    &account.provider_account_id,
                    &plan,
                    started_at,
                ),
                provider: provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: plan,
                price: price_cents,
                currency,
                billing_period: BillingPeriod::Monthly,
                paid_at,
                renewal_day: paid_at.and_then(subscription_renewal_day),
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            };
            store.upsert_subscription(&subscription)?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::End {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            ended_at,
        } => {
            let provider = canonical_provider(&provider)?;
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                None,
            )?;
            let subscription = end_active_subscription(
                store,
                &provider,
                &account.provider_account_id,
                ended_at
                    .as_deref()
                    .map(parse_date)
                    .transpose()?
                    .unwrap_or_else(Utc::now),
            )?;
            print_subscription_json(&subscription)?;
        }
        SubscriptionSubcommand::Remove {
            provider,
            provider_account_id,
            provider_user_id,
            email,
            plan,
            started_at,
            current,
        } => {
            if current == started_at.is_some() {
                bail!("pass either --started-at or --current");
            }
            let provider = canonical_provider(&provider)?;
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                None,
            )?;
            let subscription = if current {
                active_subscription(
                    store,
                    &provider,
                    &account.provider_account_id,
                    plan.as_deref(),
                    Utc::now(),
                )?
            } else {
                let started_at = parse_date(
                    started_at
                        .as_deref()
                        .with_context(|| "missing --started-at")?,
                )?;
                subscription_for_period(
                    store,
                    &provider,
                    &account.provider_account_id,
                    started_at,
                    plan.as_deref(),
                )?
            };
            let deleted = store.delete_subscription(&subscription.subscription_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "subscription_id": subscription.subscription_id.0,
                    "deleted": deleted,
                    "subscription": subscription
                }))?
            );
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
) -> Result<Vec<statsai_core::SummaryId>> {
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
    let (period, json_output, verbose, include_subscriptions) = match command.command {
        ReportSubcommand::Weekly {
            json,
            verbose,
            subscriptions,
            ..
        } => (ReportPeriod::LastDays(7), json, verbose, subscriptions),
        ReportSubcommand::Monthly {
            json,
            verbose,
            subscriptions,
            ..
        } => (ReportPeriod::LastDays(30), json, verbose, subscriptions),
        ReportSubcommand::AllTime {
            json,
            verbose,
            subscriptions,
        } => (ReportPeriod::AllTime, json, verbose, subscriptions),
    };
    let report = build_usage_report(
        &store.events()?,
        &store.summaries()?,
        &store.list_sources()?,
        &store.list_accounts()?,
        &store.list_subscriptions()?,
        period,
        Utc::now(),
    );
    if json_output {
        print_report_json(&report, verbose, include_subscriptions)?;
    } else {
        print_report_table(&report, verbose, include_subscriptions);
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
    if command.reset_remote {
        if command.status || command.verify {
            bail!("--reset-remote cannot be combined with --status or --verify");
        }
        return sync_remote_reset(command, store);
    }

    if command.verify {
        return sync_verify(command, store, device_id);
    }

    if command.status {
        return sync_status(store);
    }

    let target = sync_target(&command)?;
    if command.sink == "http" {
        maybe_reset_http_sync_tracking_if_remote_changed(&command, store, &target)?;
    }
    let (batch, payload_mode) = build_sync_batch(&command, store, device_id, &target)?;

    if command.dry_run {
        eprintln!(
            "dry run: sink={} mode={} sources={} events={} summaries={}",
            command.sink,
            sync_payload_mode_name(payload_mode),
            batch.sources.len(),
            batch.events.len(),
            batch.summaries.len()
        );
        return Ok(());
    }

    let result = (|| -> Result<()> {
        match command.sink.as_str() {
            "stdout" => {
                StdoutSink.send(&batch)?;
                store.record_sync_success(
                    &command.sink,
                    &target,
                    &batch.batch_id,
                    &batch.events,
                    &batch.summaries,
                )?;
                Ok(())
            }
            "file" => {
                let output = command
                    .output
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("statsai-sync-batch.json"));
                FileSink::new(output).send(&batch)?;
                store.record_sync_success(
                    &command.sink,
                    &target,
                    &batch.batch_id,
                    &batch.events,
                    &batch.summaries,
                )?;
                Ok(())
            }
            "http" => {
                let endpoint = http_sync_endpoint(&command)?;
                let auth_token = resolve_http_auth_token(&command, false)?;
                send_http_sync_batch(
                    store,
                    &command.sink,
                    &target,
                    &endpoint,
                    auth_token,
                    &batch,
                    payload_mode,
                )?;
                Ok(())
            }
            other => bail!("unsupported sync sink {other}"),
        }
    })();

    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = store.record_sync_failure(&command.sink, &target);
            Err(error)
        }
    }
}

fn maybe_reset_http_sync_tracking_if_remote_changed(
    command: &SyncCommand,
    store: &Store,
    target: &str,
) -> Result<()> {
    let Some(local_state) = store.sync_state("http", target)? else {
        return Ok(());
    };
    if local_state.last_batch_id.trim().is_empty() {
        return Ok(());
    }

    let auth_token = resolve_http_auth_token(command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let remote = http_remote_verify(target, &auth_token)?;
    let local_verify = sync_local_verify(store, "http", target, Some(&local_state))?;
    let batch_mismatch = !remote_sync_batch_matches_local_state(&remote, &local_state);
    let metadata_gap = remote_metadata_gap_reason(&remote, &local_verify);
    if batch_mismatch || metadata_gap.is_some() {
        let remote_last_batch = remote_last_sync_batch_id(&remote).unwrap_or("none");
        let mut reasons = Vec::new();
        if batch_mismatch {
            reasons.push(format!(
                "remote last batch ({remote_last_batch}) no longer matches local last batch ({})",
                local_state.last_batch_id
            ));
        }
        if let Some(gap) = metadata_gap {
            reasons.push(format!("remote mirror is missing synced metadata ({gap})"));
        }
        eprintln!(
            "http rollup mode: {}; clearing local sync tracking for target {}",
            reasons.join("; "),
            target
        );
        store.clear_sync_tracking_for_target("http", target)?;
    }

    Ok(())
}

fn sync_remote_reset(command: SyncCommand, store: &Store) -> Result<()> {
    if command.sink != "http" {
        bail!("--reset-remote is currently supported only with --sink http");
    }

    let endpoint = http_sync_endpoint(&command)?;
    if command.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sink": command.sink,
                "target": endpoint,
                "endpoint": endpoint,
                "would_reset_remote_sync_data": true,
                "would_clear_local_sync_tracking": true,
                "dry_run": true,
            }))?
        );
        return Ok(());
    }

    if !command.yes {
        bail!("--reset-remote deletes mirrored hosted sync data; rerun with --yes");
    }

    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let remote = http_remote_reset(&endpoint, &auth_token)?;
    store.clear_sync_tracking()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "sink": command.sink,
            "target": endpoint,
            "endpoint": endpoint,
            "cleared_local_sync_tracking": true,
            "remote": remote,
        }))?
    );
    Ok(())
}

fn build_sync_batch(
    command: &SyncCommand,
    store: &Store,
    device_id: &str,
    target: &str,
) -> Result<(SyncBatch, SyncPayloadMode)> {
    let payload_mode = sync_payload_mode(command)?;
    let state = if command.since_last {
        store.sync_state(&command.sink, target)?
    } else {
        None
    };
    let event_cursor = if payload_mode == SyncPayloadMode::Rollups {
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
    let events: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        Vec::new()
    } else {
        store
            .events_after(event_cursor)?
            .into_iter()
            .map(sanitize_event_for_sync)
            .collect()
    };
    let passthrough_summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        store
            .summaries()?
            .into_iter()
            .map(sanitize_summary_for_sync)
            .filter(|summary| !is_daily_rollup_summary(summary))
            .collect()
    } else {
        Vec::new()
    };
    let mut summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_summaries_for_sync(&command.sink, target, &passthrough_summaries)?
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
    let all_source_account_assignments: Vec<_> = store
        .list_source_account_assignments()?
        .into_iter()
        .map(sanitize_source_account_assignment_for_sync)
        .collect();
    let all_subscriptions: Vec<_> = store
        .list_subscriptions()?
        .into_iter()
        .map(sanitize_subscription_for_sync)
        .collect();
    let sources = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_sources_for_sync(&command.sink, target, &all_sources)?
    } else {
        all_sources
    };
    let accounts = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_accounts_for_sync(&command.sink, target, &all_accounts)?
    } else {
        all_accounts
    };
    let source_account_assignments = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_source_account_assignments_for_sync(
            &command.sink,
            target,
            &all_source_account_assignments,
        )?
    } else {
        all_source_account_assignments
    };
    let subscriptions = if payload_mode == SyncPayloadMode::Rollups {
        store.pending_subscriptions_for_sync(&command.sink, target, &all_subscriptions)?
    } else {
        all_subscriptions
    };

    if payload_mode == SyncPayloadMode::Rollups {
        let label = rollup_mode_label(command);
        let should_bootstrap =
            !command.dry_run && store.sync_rollup_count()? == 0 && store.event_count()? > 0;
        if !command.dry_run && command.rebuild_rollups {
            let rebuilt = store.rebuild_sync_rollups()?;
            let marked_dirty = store.mark_all_sync_rollups_dirty()?;
            eprintln!(
                "{label}: rebuilt {} local daily summaries and marked {} dirty for full sync",
                rebuilt, marked_dirty
            );
        } else if should_bootstrap {
            let rebuilt = store.rebuild_sync_rollups()?;
            eprintln!(
                "{label}: bootstrapped {} local daily summaries from existing events",
                rebuilt
            );
        }

        let full_rollup_sync = !command.since_last || state.is_none();
        let rollups = if full_rollup_sync {
            store.all_sync_rollup_summaries()?
        } else {
            let all_rollups = store.all_sync_rollup_summaries()?;
            store.pending_summaries_for_sync(&command.sink, target, &all_rollups)?
        };
        eprintln!(
            "{label}: prepared {} local daily summaries for {} sync",
            rollups.len(),
            if full_rollup_sync {
                "full-history"
            } else {
                "incremental"
            }
        );
        summaries.extend(rollups.into_iter().map(sanitize_summary_for_sync));
    }

    Ok((
        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: format!("batch_{}", Utc::now().timestamp_millis()),
            device_id: device_id.to_string(),
            sources,
            accounts,
            source_account_assignments,
            subscriptions,
            events,
            summaries,
            created_at: Utc::now(),
        },
        payload_mode,
    ))
}

fn record_rollup_sync_success(
    store: &Store,
    sink: &str,
    target: &str,
    batch: &SyncBatch,
) -> Result<()> {
    let passthrough_summaries: Vec<_> = batch
        .summaries
        .iter()
        .filter(|summary| !is_daily_rollup_summary(summary))
        .cloned()
        .collect();
    let rollup_summary_ids: Vec<_> = batch
        .summaries
        .iter()
        .filter(|summary| is_daily_rollup_summary(summary))
        .map(|summary| summary.summary_id.clone())
        .collect();

    store.record_sync_success(
        sink,
        target,
        &batch.batch_id,
        &batch.events,
        &passthrough_summaries,
    )?;
    store.mark_sync_rollups_synced(&rollup_summary_ids)?;
    store.record_summaries_synced(sink, target, &batch.summaries)?;
    store.record_sources_synced(sink, target, &batch.sources)?;
    store.record_accounts_synced(sink, target, &batch.accounts)?;
    store.record_source_account_assignments_synced(
        sink,
        target,
        &batch.source_account_assignments,
    )?;
    store.record_subscriptions_synced(sink, target, &batch.subscriptions)?;
    Ok(())
}

fn send_http_sync_batch(
    store: &Store,
    sink: &str,
    target: &str,
    endpoint: &str,
    auth_token: Option<String>,
    batch: &SyncBatch,
    payload_mode: SyncPayloadMode,
) -> Result<()> {
    let http_sink = HttpSink::new(endpoint, auth_token)?;
    let batches = if payload_mode == SyncPayloadMode::Rollups {
        split_http_rollup_sync_batches(batch)
    } else {
        vec![batch.clone()]
    };

    if batches.len() > 1 {
        eprintln!(
            "http rollup mode: split sync into {} batches of at most {} summaries",
            batches.len(),
            HTTP_ROLLUP_SUMMARIES_PER_BATCH
        );
    }

    for (index, chunk) in batches.iter().enumerate() {
        if batches.len() > 1 {
            eprintln!(
                "http rollup mode: sending batch {}/{} ({})",
                index + 1,
                batches.len(),
                chunk.batch_id
            );
        }
        send_http_rollup_chunk_with_retry(&http_sink, chunk)?;
    }

    if payload_mode == SyncPayloadMode::Rollups {
        record_rollup_sync_success(store, sink, target, batch)?;
    } else {
        store.record_sync_success(
            sink,
            target,
            &batch.batch_id,
            &batch.events,
            &batch.summaries,
        )?;
    }
    Ok(())
}

fn send_http_rollup_chunk_with_retry(http_sink: &HttpSink, chunk: &SyncBatch) -> Result<()> {
    send_http_rollup_chunk_with_retry_using(chunk, &|chunk| {
        let ack = http_sink.send_with_ack(chunk)?;
        println!("{}", serde_json::to_string_pretty(&ack)?);
        Ok(())
    })
}

fn send_http_rollup_chunk_with_retry_using<F>(chunk: &SyncBatch, send_chunk: &F) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
{
    match send_chunk(chunk) {
        Ok(()) => Ok(()),
        Err(error) if should_retry_http_rollup_budget_error(chunk, &error) => {
            let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(chunk);
            if smaller_chunks.len() <= 1 {
                return Err(error);
            }
            eprintln!(
                "http rollup mode: D1 budget rejected {}; retrying as {} smaller batches",
                chunk.batch_id,
                smaller_chunks.len()
            );
            for smaller_chunk in &smaller_chunks {
                send_http_rollup_chunk_with_retry_using(smaller_chunk, send_chunk)?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn should_retry_http_rollup_budget_error(chunk: &SyncBatch, error: &anyhow::Error) -> bool {
    let message = error.to_string();
    if !(message.contains("HTTP 413") && message.contains("sync_batch_d1_query_budget_exceeded")) {
        return false;
    }
    chunk.summaries.len() > 1
        || chunk.sources.len() > 1
        || chunk.accounts.len() > 1
        || chunk.source_account_assignments.len() > 1
        || chunk.subscriptions.len() > 1
        || (http_rollup_metadata_count(chunk) > 0 && !chunk.summaries.is_empty())
}

fn split_http_rollup_sync_batches(batch: &SyncBatch) -> Vec<SyncBatch> {
    let metadata_count = http_rollup_metadata_count(batch);
    if batch.summaries.len() <= HTTP_ROLLUP_SUMMARIES_PER_BATCH
        && metadata_count <= HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH
    {
        return fit_http_rollup_batches_to_d1_budget(vec![batch.clone()]);
    }

    let total_chunks = batch
        .summaries
        .len()
        .div_ceil(HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    let metadata_chunks = metadata_count.div_ceil(HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH);
    let mut chunks = Vec::with_capacity(total_chunks + metadata_chunks);

    chunks.extend(split_http_rollup_metadata_chunks(
        batch,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
    ));
    chunks.extend(split_http_rollup_summary_chunks(
        batch,
        HTTP_ROLLUP_SUMMARIES_PER_BATCH,
    ));

    fit_http_rollup_batches_to_d1_budget(chunks)
}

fn split_http_rollup_sync_batch_after_budget_error(batch: &SyncBatch) -> Vec<SyncBatch> {
    if http_rollup_metadata_count(batch) > 0 && !batch.summaries.is_empty() {
        let mut chunks =
            split_http_rollup_metadata_chunks(batch, HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH);
        chunks.extend(split_http_rollup_summary_chunks(
            batch,
            batch.summaries.len(),
        ));
        return chunks;
    }

    if batch.summaries.len() > 1 {
        return split_http_rollup_summary_chunks(batch, batch.summaries.len().div_ceil(2));
    }

    if batch.sources.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.sources.len().div_ceil(2));
    }
    if batch.accounts.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.accounts.len().div_ceil(2));
    }
    if batch.source_account_assignments.len() > 1 {
        return split_http_rollup_metadata_chunks(
            batch,
            batch.source_account_assignments.len().div_ceil(2),
        );
    }
    if batch.subscriptions.len() > 1 {
        return split_http_rollup_metadata_chunks(batch, batch.subscriptions.len().div_ceil(2));
    }

    vec![batch.clone()]
}

fn http_rollup_metadata_count(batch: &SyncBatch) -> usize {
    batch.sources.len()
        + batch.accounts.len()
        + batch.source_account_assignments.len()
        + batch.subscriptions.len()
}

fn fit_http_rollup_batches_to_d1_budget(chunks: Vec<SyncBatch>) -> Vec<SyncBatch> {
    let mut fitted = Vec::new();
    for chunk in chunks {
        fitted.extend(fit_http_rollup_batch_to_d1_budget(&chunk));
    }
    fitted
}

fn fit_http_rollup_batch_to_d1_budget(batch: &SyncBatch) -> Vec<SyncBatch> {
    if estimate_http_rollup_d1_queries(batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET {
        return vec![batch.clone()];
    }

    let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(batch);
    if smaller_chunks.len() <= 1 {
        return vec![batch.clone()];
    }

    fit_http_rollup_batches_to_d1_budget(smaller_chunks)
}

fn estimate_http_rollup_d1_queries(batch: &SyncBatch) -> usize {
    let authenticated_device_queries = 2;
    let existing_batch_lookup_queries = 1;
    let final_sync_bookkeeping_queries = 2;
    let account_alias_lookup_queries = usize::from(!batch.accounts.is_empty());
    let semantic_lookup_queries =
        http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .source_account_assignments
                    .iter()
                    .map(|assignment| assignment.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(
                batch
                    .subscriptions
                    .iter()
                    .map(|subscription| subscription.provider_account_id.0.as_str()),
            )
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        ) + http_rollup_query_chunks(
            unique_non_empty_provider_account_ids(batch.summaries.iter().filter_map(|summary| {
                summary.provider_account_id.as_ref().map(|id| id.0.as_str())
            }))
            .len(),
            HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
        );
    let existing_summary_state_queries =
        http_rollup_query_chunks(batch.summaries.len(), HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE);
    let project_location_lookup_queries = http_rollup_query_chunks(
        http_rollup_project_location_count(batch),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let metadata_write_queries = batch.sources.len()
        + batch.accounts.len()
        + batch.source_account_assignments.len()
        + batch.subscriptions.len();
    let project_write_queries =
        http_rollup_project_count(batch) + http_rollup_project_location_count(batch);
    let daily_rollup_write_queries = http_rollup_query_chunks(
        batch.summaries.len(),
        HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY,
    );
    let monthly_rollup_queries = http_rollup_summary_month_count(batch);
    let dashboard_snapshot_queries = usize::from(!batch.summaries.is_empty());

    authenticated_device_queries
        + existing_batch_lookup_queries
        + account_alias_lookup_queries
        + semantic_lookup_queries
        + existing_summary_state_queries
        + project_location_lookup_queries
        + metadata_write_queries
        + project_write_queries
        + daily_rollup_write_queries
        + monthly_rollup_queries
        + dashboard_snapshot_queries
        + final_sync_bookkeeping_queries
}

fn http_rollup_query_chunks(item_count: usize, chunk_size: usize) -> usize {
    if item_count == 0 {
        0
    } else {
        item_count.div_ceil(chunk_size.max(1))
    }
}

fn unique_non_empty_provider_account_ids<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'a str> {
    values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn http_rollup_summary_month_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .map(http_rollup_summary_month_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_month_key(summary: &UsageSummary) -> String {
    let anchor = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    format!("{:04}-{:02}", anchor.year(), anchor.month())
}

fn http_rollup_project_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_project_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}"));
    }
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    Some(format!("project:{}", project.project_id))
}

fn http_rollup_project_location_count(batch: &SyncBatch) -> usize {
    batch
        .summaries
        .iter()
        .filter_map(http_rollup_summary_project_location_key)
        .collect::<BTreeSet<_>>()
        .len()
}

fn http_rollup_summary_project_location_key(summary: &UsageSummary) -> Option<String> {
    let project = summary.project.as_ref()?;
    if let Some(path_hash) = project.path_hash.as_deref() {
        return Some(format!("path:{path_hash}"));
    }
    if let Some(repo_remote_hash) = project.repo_remote_hash.as_deref() {
        return Some(format!("repo:{repo_remote_hash}:{}", project.project_id));
    }
    Some(format!("project:{}", project.project_id))
}

fn split_http_rollup_metadata_chunks(batch: &SyncBatch, chunk_size: usize) -> Vec<SyncBatch> {
    let mut chunks = Vec::new();
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch, "sources", chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch, "accounts", chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "assignments",
        chunk_size,
    ));
    chunks.extend(split_http_rollup_single_metadata_kind(
        batch,
        "subscriptions",
        chunk_size,
    ));
    chunks
}

fn split_http_rollup_single_metadata_kind(
    batch: &SyncBatch,
    kind: &str,
    chunk_size: usize,
) -> Vec<SyncBatch> {
    let chunk_size = chunk_size.max(1);
    match kind {
        "sources" => batch
            .sources
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk = empty_http_rollup_chunk(batch, &format!("sources_{}", index + 1));
                chunk.sources = records.to_vec();
                chunk
            })
            .collect(),
        "accounts" => batch
            .accounts
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk = empty_http_rollup_chunk(batch, &format!("accounts_{}", index + 1));
                chunk.accounts = records.to_vec();
                chunk
            })
            .collect(),
        "assignments" => batch
            .source_account_assignments
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("assignments_{}", index + 1));
                chunk.source_account_assignments = records.to_vec();
                chunk
            })
            .collect(),
        "subscriptions" => batch
            .subscriptions
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, records)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("subscriptions_{}", index + 1));
                chunk.subscriptions = records.to_vec();
                chunk
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn split_http_rollup_summary_chunks(batch: &SyncBatch, chunk_size: usize) -> Vec<SyncBatch> {
    let chunk_size = chunk_size.max(1);
    let total_chunks = batch.summaries.len().div_ceil(chunk_size);
    batch
        .summaries
        .chunks(chunk_size)
        .enumerate()
        .map(|(index, summaries)| {
            let mut chunk = batch.clone();
            chunk.batch_id = format!("{}_part_{}_of_{}", batch.batch_id, index + 1, total_chunks);
            chunk.sources.clear();
            chunk.accounts.clear();
            chunk.source_account_assignments.clear();
            chunk.subscriptions.clear();
            chunk.events.clear();
            chunk.summaries = summaries.to_vec();
            chunk
        })
        .collect()
}

fn empty_http_rollup_chunk(batch: &SyncBatch, suffix: &str) -> SyncBatch {
    let mut chunk = batch.clone();
    chunk.batch_id = format!("{}_{}", batch.batch_id, suffix);
    chunk.sources.clear();
    chunk.accounts.clear();
    chunk.source_account_assignments.clear();
    chunk.subscriptions.clear();
    chunk.events.clear();
    chunk.summaries.clear();
    chunk
}

fn sync_status(store: &Store) -> Result<()> {
    let states = store.list_sync_states()?;
    if states.is_empty() {
        println!("no sync state recorded");
        return Ok(());
    }
    for state in states {
        let display_batch_id = logical_http_rollup_batch_id(&state.last_batch_id);
        println!(
            "{} target={} last_success={} batch={} event_cursor={} summary_cursor={} failures={}",
            state.sink,
            state.target,
            state.last_success_at.to_rfc3339(),
            display_batch_id,
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
    if command.sink != "http" {
        bail!("--verify is currently supported only with --sink http");
    }
    sync_http_verify(command, store, device_id)
}

fn sync_http_verify(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    let endpoint = http_sync_endpoint(&command)?;
    let local_state = store.sync_state("http", &endpoint)?;
    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let report = HttpVerifyReport {
        sink: command.sink,
        target: endpoint.clone(),
        endpoint: endpoint.clone(),
        device_id: device_id.to_string(),
        local: sync_local_verify(store, "http", &endpoint, local_state.as_ref())?,
        remote: http_remote_verify(&endpoint, &auth_token)?,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn sync_local_verify(
    store: &Store,
    sink: &str,
    target: &str,
    local_state: Option<&SyncState>,
) -> Result<SyncLocalVerify> {
    let all_sources = store.list_sources()?;
    let all_accounts = store.list_accounts()?;
    let all_source_account_assignments = store.list_source_account_assignments()?;
    let all_subscriptions = store.list_subscriptions()?;
    let sync_sources: Vec<_> = all_sources
        .iter()
        .cloned()
        .map(sanitize_source_for_sync)
        .collect();
    let sync_accounts: Vec<_> = all_accounts
        .iter()
        .cloned()
        .map(sanitize_account_for_sync)
        .collect();
    let sync_source_account_assignments: Vec<_> = all_source_account_assignments
        .iter()
        .cloned()
        .map(sanitize_source_account_assignment_for_sync)
        .collect();
    let sync_subscriptions: Vec<_> = all_subscriptions
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
    let rollup_summaries = store.all_sync_rollup_summaries()?;

    Ok(SyncLocalVerify {
        sync_state: local_state.map(sync_state_report),
        total_sources: all_sources.len(),
        enabled_sources: all_sources.iter().filter(|source| source.enabled).count(),
        pending_sources: store
            .pending_sources_for_sync(sink, target, &sync_sources)?
            .len(),
        total_accounts: all_accounts.len(),
        pending_accounts: store
            .pending_accounts_for_sync(sink, target, &sync_accounts)?
            .len(),
        total_source_account_assignments: all_source_account_assignments.len(),
        pending_source_account_assignments: store
            .pending_source_account_assignments_for_sync(
                sink,
                target,
                &sync_source_account_assignments,
            )?
            .len(),
        total_subscriptions: all_subscriptions.len(),
        pending_subscriptions: store
            .pending_subscriptions_for_sync(sink, target, &sync_subscriptions)?
            .len(),
        total_passthrough_summaries: passthrough_summaries.len(),
        pending_passthrough_summaries: store
            .pending_summaries_for_sync(sink, target, &passthrough_summaries)?
            .len(),
        total_rollups: rollup_summaries.len(),
        pending_rollups: store
            .pending_summaries_for_sync(sink, target, &rollup_summaries)?
            .len(),
        dirty_rollups: store.dirty_sync_rollup_summaries()?.len(),
    })
}

fn sync_target(command: &SyncCommand) -> Result<String> {
    match command.sink.as_str() {
        "http" => http_sync_endpoint(command),
        "file" => Ok(command
            .output
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "statsai-sync-batch.json".to_string())),
        other => Ok(other.to_string()),
    }
}

fn http_sync_endpoint(command: &SyncCommand) -> Result<String> {
    if let Some(endpoint) = command
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(endpoint.to_string());
    }
    Ok(format!(
        "{}/api/sync/batches",
        auth::cloudflare_api_url().trim_end_matches('/')
    ))
}

fn resolve_http_auth_token(command: &SyncCommand, required: bool) -> Result<Option<String>> {
    if let Some(token) = command
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token.to_string()));
    }

    if let Some(token) = std::env::var("STATSAI_SYNC_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token));
    }

    let token = auth::get_or_refresh_token()?;
    if required {
        Ok(Some(token.context(
            "device login required; run `statsai auth login` first",
        )?))
    } else {
        Ok(token)
    }
}

fn sync_payload_mode(command: &SyncCommand) -> Result<SyncPayloadMode> {
    match command.sink.as_str() {
        "http" => Ok(SyncPayloadMode::Rollups),
        _ => Ok(SyncPayloadMode::Raw),
    }
}

fn sync_payload_mode_name(mode: SyncPayloadMode) -> &'static str {
    match mode {
        SyncPayloadMode::Raw => "raw",
        SyncPayloadMode::Rollups => "rollups",
    }
}

fn rollup_mode_label(command: &SyncCommand) -> &'static str {
    let _ = command;
    "http rollup mode"
}

#[derive(Debug, Serialize)]
struct HttpVerifyReport {
    sink: String,
    target: String,
    endpoint: String,
    device_id: String,
    local: SyncLocalVerify,
    remote: Value,
}

#[derive(Debug, Serialize)]
struct SyncLocalVerify {
    sync_state: Option<SyncStateReport>,
    total_sources: usize,
    enabled_sources: usize,
    pending_sources: usize,
    total_accounts: usize,
    pending_accounts: usize,
    total_source_account_assignments: usize,
    pending_source_account_assignments: usize,
    total_subscriptions: usize,
    pending_subscriptions: usize,
    total_passthrough_summaries: usize,
    pending_passthrough_summaries: usize,
    total_rollups: usize,
    pending_rollups: usize,
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

fn http_remote_verify(endpoint: &str, auth_token: &str) -> Result<Value> {
    let url = http_verify_status_url(endpoint)?;
    let request = ureq::get(&url).set("Authorization", &format!("Bearer {auth_token}"));
    match request.call() {
        Ok(response) => http_response_json(response, "verify sync status"),
        Err(error) => Err(http_request_error("verify sync status", error)),
    }
}

fn http_remote_reset(endpoint: &str, auth_token: &str) -> Result<Value> {
    let url = http_reset_url(endpoint)?;
    let body = serde_json::to_string(&json!({
        "confirm": "reset_synced_data",
    }))?;
    let request = ureq::post(&url)
        .set("Authorization", &format!("Bearer {auth_token}"))
        .set("Content-Type", "application/json");
    match request.send_string(&body) {
        Ok(response) => http_response_json(response, "reset remote sync data"),
        Err(error) => Err(http_request_error("reset remote sync data", error)),
    }
}

fn http_verify_status_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Ok(format!("{prefix}/api/sync/status"));
    }
    bail!(
        "http verify expects a Cloudflare sync endpoint ending in /api/sync/batches; got {}",
        endpoint
    )
}

fn http_reset_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Ok(format!("{prefix}/api/sync/reset"));
    }
    bail!(
        "http reset expects a Cloudflare sync endpoint ending in /api/sync/batches; got {}",
        endpoint
    )
}

fn http_request_error(action: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {action} failed (HTTP {code}): {body}")
        }
        other => anyhow::anyhow!("HTTP {action} failed: {other}"),
    }
}

fn http_response_json(response: ureq::Response, action: &str) -> Result<Value> {
    let body = response
        .into_string()
        .with_context(|| format!("read HTTP {action} response body"))?;
    serde_json::from_str(&body).with_context(|| format!("parse HTTP {action} response JSON"))
}

fn remote_sync_batch_matches_local_state(
    remote: &Value,
    local_state: &statsai_store::SyncState,
) -> bool {
    remote_last_sync_batch_id(remote)
        .map(|batch_id| {
            logical_http_rollup_batch_id(batch_id)
                == logical_http_rollup_batch_id(&local_state.last_batch_id)
        })
        .unwrap_or(false)
}

fn logical_http_rollup_batch_id(batch_id: &str) -> String {
    let mut current = batch_id.to_string();
    loop {
        let next = strip_one_http_rollup_batch_suffix(&current);
        if next == current {
            return current;
        }
        current = next;
    }
}

fn strip_one_http_rollup_batch_suffix(batch_id: &str) -> String {
    if let Some(index) = batch_id.rfind("_part_") {
        let suffix = &batch_id[(index + "_part_".len())..];
        if let Some((part, total)) = suffix.split_once("_of_") {
            if part.parse::<usize>().is_ok() && total.parse::<usize>().is_ok() {
                return batch_id[..index].to_string();
            }
        }
    }

    for marker in [
        "_sources_",
        "_accounts_",
        "_assignments_",
        "_subscriptions_",
    ] {
        if let Some(index) = batch_id.rfind(marker) {
            let suffix = &batch_id[(index + marker.len())..];
            if suffix.parse::<usize>().is_ok() {
                return batch_id[..index].to_string();
            }
        }
    }

    batch_id.to_string()
}

fn remote_last_sync_batch_id(remote: &Value) -> Option<&str> {
    remote
        .pointer("/device/last_sync_batch_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn remote_metadata_gap_reason(remote: &Value, local: &SyncLocalVerify) -> Option<String> {
    let mut reasons = Vec::new();
    push_remote_metadata_gap(
        &mut reasons,
        "sources",
        remote
            .pointer("/mirrorCounts/sources")
            .and_then(Value::as_u64),
        local.total_sources,
        local.pending_sources,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "accounts",
        remote
            .pointer("/mirrorCounts/accounts")
            .and_then(Value::as_u64),
        local.total_accounts,
        local.pending_accounts,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "source_account_assignments",
        remote
            .pointer("/mirrorCounts/source_account_assignments")
            .and_then(Value::as_u64),
        local.total_source_account_assignments,
        local.pending_source_account_assignments,
    );
    push_remote_metadata_gap(
        &mut reasons,
        "subscriptions",
        remote
            .pointer("/mirrorCounts/subscriptions")
            .and_then(Value::as_u64),
        local.total_subscriptions,
        local.pending_subscriptions,
    );

    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join(", "))
    }
}

fn push_remote_metadata_gap(
    reasons: &mut Vec<String>,
    label: &str,
    remote_count: Option<u64>,
    local_total: usize,
    local_pending: usize,
) {
    if local_total == 0 || local_pending > 0 {
        return;
    }
    if let Some(remote_count) = remote_count {
        if remote_count < local_total as u64 {
            reasons.push(format!("{label} {remote_count}<{local_total}"));
        }
    }
}

fn sync_state_report(state: &statsai_store::SyncState) -> SyncStateReport {
    SyncStateReport {
        last_success_at: state.last_success_at.to_rfc3339(),
        last_batch_id: logical_http_rollup_batch_id(&state.last_batch_id),
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
        statsai_daemon::watch_and_serve(&command.api, store, device_id)
    } else {
        statsai_daemon::run(&command.api, store)
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

fn print_report_table(report: &UsageReport, verbose: bool, include_subscriptions: bool) {
    println!("statsai report: {}", report.label);
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

    if include_subscriptions {
        print_subscription_report_table(report, verbose);
    }

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

fn print_subscription_report_table(report: &UsageReport, verbose: bool) {
    if report.subscription_rows.is_empty() {
        println!("subscription value: no matching subscription periods");
        return;
    }
    println!("subscription value:");
    println!(
        "{:<14} {:<16} {:<14} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "provider", "account", "plan", "events", "total", "value_usd", "price", "ratio"
    );
    for row in &report.subscription_rows {
        println!(
            "{:<14} {:<16} {:<14} {:>10} {:>12} {:>12} {:>12} {:>12}",
            row.provider,
            truncate_label(&row.account, 16),
            truncate_label(&row.plan_name, 14),
            format_u64(row.events),
            format_u64(row.usage.total_tokens),
            format_cost(row.usage.estimated_cost_usd),
            format_subscription_price(row.price, &row.currency),
            format_ratio(row.value_to_price_ratio)
        );
        if verbose {
            println!("  subscription_id: {}", row.subscription_id.0);
            println!("  provider_account_id: {}", row.provider_account_id.0);
            println!("  started_at: {}", row.started_at.to_rfc3339());
            if let Some(ended_at) = row.ended_at {
                println!("  ended_at: {}", ended_at.to_rfc3339());
            }
            println!("  status: {}", subscription_status_label(&row.status));
            if let Some(delta) = row.value_minus_price_usd {
                println!("  value_minus_price_usd: {}", format_cost(Some(delta)));
            }
        }
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

fn print_report_json(
    report: &UsageReport,
    verbose: bool,
    include_subscriptions: bool,
) -> Result<()> {
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
            "estimated_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_cost_usd_cents": row.usage.estimated_cost_usd,
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
            "estimated_or_reported_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": row.usage.estimated_cost_usd,
        });
        if verbose {
            value["sources"] = json!(row.sources.iter().cloned().collect::<Vec<_>>());
            value["paths"] = json!(row.paths.iter().cloned().collect::<Vec<_>>());
        }
        value
    });
    let subscription_rows = report.subscription_rows.iter().map(|row| {
        json!({
            "subscription_id": row.subscription_id.0,
            "provider": row.provider,
            "provider_account_id": row.provider_account_id.0,
            "account": row.account,
            "plan_name": row.plan_name,
            "price": major_unit_amount(row.price),
            "price_cents": row.price,
            "currency": row.currency,
            "billing_period": format!("{:?}", row.billing_period).to_ascii_lowercase(),
            "started_at": row.started_at.to_rfc3339(),
            "ended_at": row.ended_at.map(|date| date.to_rfc3339()),
            "status": subscription_status_label(&row.status),
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
            "estimated_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_cost_usd_cents": row.usage.estimated_cost_usd,
            "value_minus_price_usd": usd_amount_json(row.value_minus_price_usd),
            "value_minus_price_usd_cents": row.value_minus_price_usd,
            "value_to_price_ratio": row.value_to_price_ratio,
        })
    });
    let summary_direct_total: u64 = report
        .summary_rows
        .iter()
        .map(|row| row.direct_event_usage.total_tokens)
        .sum();
    let mut known_usage = report.total_usage.clone();
    known_usage.add_totals(&report.total_summary_usage);
    let mut value = json!({
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
        "total_estimated_cost_usd": usd_amount_json(report.total_usage.estimated_cost_usd),
        "total_estimated_cost_usd_cents": report.total_usage.estimated_cost_usd,
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
            "estimated_or_reported_cost_usd": usd_amount_json(known_usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": known_usage.estimated_cost_usd,
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
            "estimated_or_reported_cost_usd": usd_amount_json(report.total_summary_usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": report.total_summary_usage.estimated_cost_usd,
            "uncovered_total_tokens": report.total_summary_usage.total_tokens.saturating_sub(summary_direct_total),
            "rows": summary_rows.collect::<Vec<_>>(),
        },
        "rows": rows.collect::<Vec<_>>(),
    });
    if include_subscriptions {
        value["subscription_value"] = json!({
            "rows": subscription_rows.collect::<Vec<_>>(),
        });
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn resolve_or_create_provider_account(
    store: &Store,
    provider: &str,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    label: Option<String>,
) -> Result<ProviderAccount> {
    if let Some(provider_account_id_value) = provider_account_id_value {
        let provider_account_id = ProviderAccountId(provider_account_id_value.to_string());
        if let Some(account) = store.account(&provider_account_id)? {
            ensure_account_matches_provider(&account, provider)?;
            return Ok(account);
        }
        if provider_user_id.is_none() && email.is_none() {
            bail!("unknown provider account {provider_account_id_value}");
        }
    }
    upsert_provider_account(
        store,
        UpsertProviderAccountInput {
            provider,
            provider_user_id,
            email,
            label,
            plan_name: None,
            identity_source: Some(IdentitySource::UserConfigured),
            verified_at: None,
        },
    )
}

fn resolve_existing_provider_account(
    store: &Store,
    provider: &str,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    label: Option<String>,
) -> Result<ProviderAccount> {
    if let Some(provider_account_id_value) = provider_account_id_value {
        let provider_account_id = ProviderAccountId(provider_account_id_value.to_string());
        let account = store
            .account(&provider_account_id)?
            .with_context(|| format!("unknown provider account {provider_account_id_value}"))?;
        ensure_account_matches_provider(&account, provider)?;
        return Ok(account);
    }

    if let Some(account) = find_existing_provider_account(store, provider, provider_user_id, email)?
    {
        return Ok(account);
    }

    let normalized_label = label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty());
    if let Some(label) = normalized_label {
        let mut matches = store.list_accounts()?.into_iter().filter(|account| {
            account.provider == provider && account.account_label.as_deref() == Some(label)
        });
        let Some(account) = matches.next() else {
            bail!("unknown provider account label {label} for {provider}");
        };
        if matches.next().is_some() {
            bail!("provider account label {label} is ambiguous for {provider}");
        }
        return Ok(account);
    }

    bail!("unknown provider account selector for {provider}")
}

fn ensure_account_matches_provider(account: &ProviderAccount, provider: &str) -> Result<()> {
    if account.provider != provider {
        bail!(
            "provider account {} belongs to {}, not {}",
            account.provider_account_id.0,
            account.provider,
            provider
        );
    }
    Ok(())
}

fn is_verified_subscription_source(source: &IdentitySource) -> bool {
    matches!(
        source,
        IdentitySource::LocalAuth
            | IdentitySource::ProviderAuth
            | IdentitySource::ProviderApi
            | IdentitySource::CookieOauth
            | IdentitySource::CliProbe
    )
}

fn parse_source_verification_mode(value: &str) -> Result<SourceVerificationMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(SourceVerificationMode::Auto),
        "manual_only" | "manual-only" => Ok(SourceVerificationMode::ManualOnly),
        "disabled" => Ok(SourceVerificationMode::Disabled),
        _ => bail!("unsupported verification mode {value}; use auto, manual_only, or disabled"),
    }
}

fn resolve_source_reference(
    store: &Store,
    source_id: Option<&str>,
    path: Option<&Path>,
) -> Result<SourceLocation> {
    match (source_id, path) {
        (Some(_), Some(_)) => bail!("pass either --source-id or --path, not both"),
        (Some(source_id), None) => store
            .source(&SourceId(source_id.to_string()))?
            .with_context(|| format!("source {source_id} not found")),
        (None, Some(path)) => {
            let normalized = expand_home_path(&path.to_string_lossy());
            let target_hash = path_hash(&normalized);
            let mut matches = store
                .list_sources()?
                .into_iter()
                .filter(|source| source.path_hash.as_deref() == Some(target_hash.as_str()));
            let Some(source) = matches.next() else {
                bail!("no source found for path {}", normalized.display());
            };
            if matches.next().is_some() {
                bail!(
                    "multiple sources match path {}; use --source-id instead",
                    normalized.display()
                );
            }
            Ok(source)
        }
        (None, None) => bail!("pass either --source-id or --path"),
    }
}

fn source_verification_mode(source: &SourceLocation) -> SourceVerificationMode {
    source.verification_mode.clone()
}

fn probe_source_verified_state(source: &SourceLocation) -> Result<Option<VerifiedSourceState>> {
    if matches!(
        source_verification_mode(source),
        SourceVerificationMode::Disabled
    ) {
        return Ok(None);
    }
    let Some(adapter) = adapter_for_provider(&source.provider) else {
        return Ok(None);
    };
    adapter.probe_verified_source_state(source)
}

fn explain_source(store: &Store, source: &SourceLocation) -> Result<Value> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    let detected_auth_state = if matches!(
        source_verification_mode(source),
        SourceVerificationMode::Disabled
    ) {
        None
    } else {
        probe_source_verified_state(source)?
            .map(serde_json::to_value)
            .transpose()?
    };
    let now = Utc::now();
    let current_assignment = assignment_for_timestamp(&assignments, now).cloned();
    let current_subscription = current_assignment
        .as_ref()
        .and_then(|assignment| {
            active_subscription(
                store,
                &source.provider,
                &assignment.provider_account_id,
                None,
                now,
            )
            .ok()
        })
        .map(serde_json::to_value)
        .transpose()?;
    Ok(json!({
        "source": source,
        "verification_mode": source.verification_mode,
        "verified_state_hash": source.verified_state_hash,
        "detected_auth_state": detected_auth_state,
        "current_assignment": current_assignment,
        "current_subscription": current_subscription,
        "history": assignments,
        "explanation": {
            "usage_is_primary": true,
            "subscriptions_are_secondary": true,
            "unassigned_means": "usage without an active source-to-account connection"
        }
    }))
}

struct ConnectSourceToAccountInput<'a> {
    source_id: &'a SourceId,
    provider_account_id_value: Option<&'a str>,
    provider_user_id: Option<&'a str>,
    email: Option<&'a str>,
    label: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

fn connect_source_to_account(
    store: &Store,
    input: ConnectSourceToAccountInput<'_>,
) -> Result<SourceAccountAssignment> {
    let ConnectSourceToAccountInput {
        source_id,
        provider_account_id_value,
        provider_user_id,
        email,
        label,
        started_at,
        ended_at,
    } = input;
    let source = store
        .source(source_id)?
        .with_context(|| format!("unknown source {}", source_id.0))?;
    let account = resolve_or_create_provider_account(
        store,
        &source.provider,
        provider_account_id_value,
        provider_user_id,
        email,
        label,
    )?;
    validate_time_window(started_at, ended_at, "source connection")?;

    let overlaps: Vec<_> = store
        .list_source_account_assignments_for_source(&source.source_id)?
        .into_iter()
        .filter(|assignment| {
            periods_overlap(
                started_at,
                ended_at,
                assignment.started_at,
                assignment.ended_at,
            )
        })
        .collect();

    if overlaps.len() > 1 {
        bail!(
            "source {} has multiple overlapping account connections around {}",
            source.source_id.0,
            started_at.to_rfc3339()
        );
    }

    if let Some(existing) = overlaps.first() {
        if existing.provider_account_id == account.provider_account_id {
            let merged_started_at = existing.started_at.min(started_at);
            let merged_ended_at = match (existing.ended_at, ended_at) {
                (None, _) | (_, None) => None,
                (Some(left), Some(right)) => Some(left.max(right)),
            };

            if existing.started_at == merged_started_at && existing.ended_at == merged_ended_at {
                return Ok(existing.clone());
            }

            let previous_assignment_id = existing.assignment_id.clone();
            let now = Utc::now();
            let merged = SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source.source_id,
                    &account.provider_account_id,
                    merged_started_at,
                ),
                source_id: source.source_id.clone(),
                provider: source.provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                started_at: merged_started_at,
                ended_at: merged_ended_at,
                record_source: IdentitySource::UserConfigured,
                verified_at: existing.verified_at,
                created_at: existing.created_at,
                updated_at: now,
            };
            if previous_assignment_id != merged.assignment_id {
                store.delete_source_account_assignment(&previous_assignment_id)?;
            }
            store.upsert_source_account_assignment(&merged)?;
            reattribute_source_records(store, &source.source_id)?;
            return Ok(merged);
        }

        preserve_non_overlapping_source_assignment_segments(
            store, &source, existing, started_at, ended_at,
        )?;
    }

    validate_source_assignment_overlap(
        store,
        &source.source_id,
        &account.provider_account_id,
        started_at,
        ended_at,
        None,
    )?;
    let now = Utc::now();
    let assignment = SourceAccountAssignment {
        schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
        assignment_id: source_account_assignment_id(
            &source.source_id,
            &account.provider_account_id,
            started_at,
        ),
        source_id: source.source_id.clone(),
        provider: source.provider.clone(),
        provider_account_id: account.provider_account_id,
        started_at,
        ended_at,
        record_source: IdentitySource::UserConfigured,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(assignment)
}

fn preserve_non_overlapping_source_assignment_segments(
    store: &Store,
    source: &SourceLocation,
    existing: &SourceAccountAssignment,
    replacement_started_at: DateTime<Utc>,
    replacement_ended_at: Option<DateTime<Utc>>,
) -> Result<()> {
    let now = Utc::now();
    let preserve_before = existing.started_at < replacement_started_at;
    let preserve_after = replacement_ended_at
        .map(|replacement_ended_at| {
            existing
                .ended_at
                .map(|existing_ended_at| existing_ended_at > replacement_ended_at)
                .unwrap_or(true)
        })
        .unwrap_or(false);

    if preserve_before {
        let mut before = existing.clone();
        before.ended_at = Some(replacement_started_at);
        before.updated_at = now;
        validate_time_window(before.started_at, before.ended_at, "source connection")?;
        store.upsert_source_account_assignment(&before)?;
    } else {
        store.delete_source_account_assignment(&existing.assignment_id)?;
    }

    if preserve_after {
        let tail_started_at =
            replacement_ended_at.expect("preserve_after requires finite replacement end");
        let tail = SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &existing.provider_account_id,
                tail_started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: existing.provider_account_id.clone(),
            started_at: tail_started_at,
            ended_at: existing.ended_at,
            record_source: existing.record_source.clone(),
            verified_at: existing.verified_at,
            created_at: now,
            updated_at: now,
        };
        validate_time_window(tail.started_at, tail.ended_at, "source connection")?;
        store.upsert_source_account_assignment(&tail)?;
    }

    Ok(())
}

fn active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    plan: Option<&str>,
    timestamp: DateTime<Utc>,
) -> Result<Subscription> {
    store
        .list_subscriptions()?
        .into_iter()
        .filter(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && plan
                    .map(|plan_name| subscription.plan_name.eq_ignore_ascii_case(plan_name))
                    .unwrap_or(true)
                && timestamp_in_period(
                    timestamp,
                    subscription.started_at,
                    effective_subscription_ended_at(subscription),
                )
        })
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
        .with_context(|| {
            let plan_suffix = plan
                .map(|plan_name| format!(" plan {}", plan_name))
                .unwrap_or_default();
            format!(
                "no active{} subscription found for account {} at {}",
                plan_suffix,
                provider_account_id.0,
                timestamp.to_rfc3339()
            )
        })
}

fn disconnect_source_from_account(
    store: &Store,
    source_id: &SourceId,
    provider_account_id_value: Option<&str>,
    provider_user_id: Option<&str>,
    email: Option<&str>,
    ended_at: DateTime<Utc>,
) -> Result<SourceAccountAssignment> {
    let source = store
        .source(source_id)?
        .with_context(|| format!("unknown source {}", source_id.0))?;
    let account_filter =
        if provider_account_id_value.is_some() || provider_user_id.is_some() || email.is_some() {
            Some(
                resolve_existing_provider_account(
                    store,
                    &source.provider,
                    provider_account_id_value,
                    provider_user_id,
                    email,
                    None,
                )?
                .provider_account_id,
            )
        } else {
            None
        };
    let mut active: Vec<_> = store
        .list_source_account_assignments_for_source(&source.source_id)?
        .into_iter()
        .filter(|assignment| {
            timestamp_in_period(ended_at, assignment.started_at, assignment.ended_at)
        })
        .filter(|assignment| {
            account_filter
                .as_ref()
                .map(|account_id| &assignment.provider_account_id == account_id)
                .unwrap_or(true)
        })
        .collect();
    let Some(mut assignment) = active.pop() else {
        bail!(
            "no active source connection found for {} at {}",
            source.source_id.0,
            ended_at.to_rfc3339()
        );
    };
    validate_time_window(assignment.started_at, Some(ended_at), "source connection")?;
    assignment.ended_at = Some(ended_at);
    assignment.updated_at = Utc::now();
    store.upsert_source_account_assignment(&assignment)?;
    reattribute_source_records(store, &source.source_id)?;
    Ok(assignment)
}

fn subscription_renewal_day(timestamp: DateTime<Utc>) -> Option<u8> {
    u8::try_from(timestamp.day()).ok()
}

fn subscription_for_period(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    plan: Option<&str>,
) -> Result<Subscription> {
    store
        .list_subscriptions()?
        .into_iter()
        .find(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && subscription.started_at == started_at
                && plan
                    .map(|plan_name| subscription.plan_name == plan_name)
                    .unwrap_or(true)
        })
        .with_context(|| {
            format!(
                "unknown subscription period for account {} starting {}",
                provider_account_id.0,
                started_at.to_rfc3339()
            )
        })
}

fn close_active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    ended_at: DateTime<Utc>,
) -> Result<Option<Subscription>> {
    let active = store
        .list_subscriptions()?
        .into_iter()
        .find(|subscription| {
            subscription.provider == provider
                && subscription.provider_account_id == *provider_account_id
                && timestamp_in_period(
                    ended_at,
                    subscription.started_at,
                    effective_subscription_ended_at(subscription),
                )
        });
    let Some(mut subscription) = active else {
        return Ok(None);
    };
    validate_time_window(subscription.started_at, Some(ended_at), "subscription")?;
    subscription.ended_at = Some(ended_at);
    store.upsert_subscription(&subscription)?;
    Ok(Some(subscription))
}

fn effective_subscription_ended_at(subscription: &Subscription) -> Option<DateTime<Utc>> {
    if is_verified_subscription_source(&subscription.record_source)
        && subscription.status == SubscriptionStatus::Active
        && subscription.ended_at.is_some()
        && subscription.ended_at == subscription.current_period_ends_at
    {
        return None;
    }
    subscription.ended_at
}

fn end_active_subscription(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    ended_at: DateTime<Utc>,
) -> Result<Subscription> {
    close_active_subscription(store, provider, provider_account_id, ended_at)?.with_context(|| {
        format!(
            "no active subscription found for account {} at {}",
            provider_account_id.0,
            ended_at.to_rfc3339()
        )
    })
}

fn validate_time_window(
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    label: &str,
) -> Result<()> {
    if ended_at.is_some_and(|ended_at| ended_at <= started_at) {
        bail!("{label} ended_at must be after started_at");
    }
    Ok(())
}

fn validate_source_assignment_overlap(
    store: &Store,
    source_id: &SourceId,
    _provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_assignment_id: Option<&SourceAccountAssignmentId>,
) -> Result<()> {
    for assignment in store.list_source_account_assignments_for_source(source_id)? {
        if ignore_assignment_id == Some(&assignment.assignment_id) {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            assignment.started_at,
            assignment.ended_at,
        ) {
            bail!(
                "source connection overlaps an existing connection for source {}",
                source_id.0
            );
        }
    }
    Ok(())
}

fn validate_subscription_overlap(
    store: &Store,
    provider: &str,
    provider_account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    ignore_subscription_id: Option<&SubscriptionId>,
) -> Result<()> {
    for subscription in store.list_subscriptions()? {
        if ignore_subscription_id == Some(&subscription.subscription_id) {
            continue;
        }
        if subscription.provider != provider {
            continue;
        }
        if &subscription.provider_account_id != provider_account_id {
            continue;
        }
        if periods_overlap(
            started_at,
            ended_at,
            subscription.started_at,
            subscription.ended_at,
        ) {
            bail!(
                "subscription overlaps existing subscription {} for account {}",
                subscription.subscription_id.0,
                provider_account_id.0
            );
        }
    }
    Ok(())
}

fn reattribute_source_records(store: &Store, source_id: &SourceId) -> Result<()> {
    if store.source(source_id)?.is_none() {
        return Ok(());
    }
    let assignments = store.list_source_account_assignments_for_source(source_id)?;
    let mut events = store.events_for_source(source_id)?;
    let mut summaries = store.summaries_for_source(source_id)?;
    for event in &mut events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in &mut summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    store.rewrite_events(&events)?;
    store.rewrite_summaries(&summaries)?;
    Ok(())
}

fn apply_source_account_resolution(
    store: &Store,
    source: &SourceLocation,
    events: &mut [UsageEvent],
    summaries: &mut [UsageSummary],
) -> Result<()> {
    let assignments = store.list_source_account_assignments_for_source(&source.source_id)?;
    for event in events {
        apply_account_resolution_to_event(&assignments, event);
    }
    for summary in summaries {
        apply_account_resolution_to_summary(&assignments, summary);
    }
    Ok(())
}

fn apply_account_resolution_to_event(
    assignments: &[SourceAccountAssignment],
    event: &mut UsageEvent,
) {
    if keep_detected_account_identity(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let assignment = assignment_for_timestamp(assignments, event.session.started_at);
    if let Some(assignment) = assignment {
        event.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        event.provider_account_id.as_ref(),
        event
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        event.provider_account_id = None;
        if let Some(evidence) = event.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn apply_account_resolution_to_summary(
    assignments: &[SourceAccountAssignment],
    summary: &mut UsageSummary,
) {
    if keep_detected_account_identity(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        return;
    }
    let timestamp = summary.period_start.unwrap_or(summary.observed_at);
    let assignment = assignment_for_timestamp(assignments, timestamp);
    if let Some(assignment) = assignment {
        summary.provider_account_id = Some(assignment.provider_account_id.clone());
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::SourceConfig;
        }
    } else if should_clear_resolved_account(
        summary.provider_account_id.as_ref(),
        summary
            .parse_evidence
            .as_ref()
            .map(|evidence| &evidence.account_identity_source),
    ) {
        summary.provider_account_id = None;
        if let Some(evidence) = summary.parse_evidence.as_mut() {
            evidence.account_identity_source = IdentitySource::Unresolved;
        }
    }
}

fn keep_detected_account_identity(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    let Some(identity_source) = identity_source else {
        return false;
    };
    !matches!(
        identity_source,
        IdentitySource::SourceConfig
            | IdentitySource::UserConfigured
            | IdentitySource::ManualHint
            | IdentitySource::Unknown
            | IdentitySource::Unresolved
    )
}

fn should_clear_resolved_account(
    provider_account_id: Option<&ProviderAccountId>,
    identity_source: Option<&IdentitySource>,
) -> bool {
    let Some(provider_account_id) = provider_account_id else {
        return false;
    };
    if provider_account_id.0.trim().is_empty() {
        return false;
    }
    matches!(
        identity_source,
        None | Some(
            IdentitySource::SourceConfig
                | IdentitySource::UserConfigured
                | IdentitySource::ManualHint
                | IdentitySource::Unknown
                | IdentitySource::Unresolved
        )
    )
}

fn assignment_for_timestamp(
    assignments: &[SourceAccountAssignment],
    timestamp: DateTime<Utc>,
) -> Option<&SourceAccountAssignment> {
    assignments
        .iter()
        .filter(|assignment| {
            timestamp_in_period(timestamp, assignment.started_at, assignment.ended_at)
        })
        .max_by(|left, right| left.started_at.cmp(&right.started_at))
}

fn default_store_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
        .join("statsai.sqlite")
}

fn default_device_id() -> String {
    if let Ok(value) = std::env::var("STATSAI_DEVICE_ID") {
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
        .join(".statsai")
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
                if statsai_core::path_hash(&path) == *hash {
                    return Some(path.to_string_lossy().to_string());
                }
            }
            None
        }
        ("codex", LocationOrigin::Default) if source.path_hash.is_some() => {
            let root = home.join(".codex");
            let hash = source.path_hash.as_ref()?;
            if statsai_core::path_hash(&root) == *hash {
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
            "{} path={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} files={} cached={} timestamp_fallbacks={} model_fallbacks={} origin={} source={}",
            source.provider,
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
            "{} path={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={}",
            source.provider,
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
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    replace: bool,
    no_cache: bool,
) -> Result<Vec<ScanFileStateEntry>> {
    if replace || no_cache {
        return Ok(file_cache_entries.to_vec());
    }
    store.pending_scan_file_entries(source_id, file_cache_entries)
}

fn should_replace_source_records_for_scan(
    explicit_replace: bool,
    ran_scan: bool,
    candidate_count: usize,
    pending_count: usize,
) -> bool {
    explicit_replace || (ran_scan && candidate_count > 0 && pending_count == candidate_count)
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

fn major_unit_amount(cents: i64) -> f64 {
    cents as f64 / 100.0
}

fn usd_amount_json(cost: Option<i64>) -> Value {
    cost.map_or(Value::Null, |cents| json!(major_unit_amount(cents)))
}

fn format_cost(cost: Option<i64>) -> String {
    cost.map(|cents| {
        let dollars = major_unit_amount(cents);
        format!("${dollars:.2}")
    })
    .unwrap_or_else(|| "unknown".to_string())
}

fn format_subscription_price(price_cents: i64, currency: &str) -> String {
    let price = major_unit_amount(price_cents);
    if currency.eq_ignore_ascii_case("USD") {
        format!("${price:.2}")
    } else {
        format!("{price:.2} {currency}")
    }
}

fn subscription_json_value(subscription: &Subscription) -> Value {
    let mut value = serde_json::to_value(subscription).expect("serialize subscription");
    value["price_cents"] = json!(subscription.price);
    value["price"] = json!(major_unit_amount(subscription.price));
    value
}

fn print_subscription_json(subscription: &Subscription) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&subscription_json_value(subscription))?
    );
    Ok(())
}

fn format_ratio(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}x"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn truncate_label(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn subscription_status_label(status: &SubscriptionStatus) -> &'static str {
    match status {
        SubscriptionStatus::Active => "active",
        SubscriptionStatus::Paused => "paused",
        SubscriptionStatus::Cancelled => "cancelled",
    }
}

fn sanitize_source_for_sync(mut source: SourceLocation) -> SourceLocation {
    source.path_label = None;
    source
}

fn sanitize_account_for_sync(mut account: ProviderAccount) -> ProviderAccount {
    if !matches!(account.identity_source, IdentitySource::UserConfigured) {
        account.account_label = None;
    }
    account.plan_name = None;
    account
}

fn sanitize_source_account_assignment_for_sync(
    assignment: SourceAccountAssignment,
) -> SourceAccountAssignment {
    assignment
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
struct SyncRollupStatsAccumulator {
    provider: String,
    source_id: statsai_core::SourceId,
    provider_account_id: Option<statsai_core::ProviderAccountId>,
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
    estimated_cost_usd: i64, // cents USD
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SyncRollupStatsBucketKey {
    provider: String,
    source_id: String,
    account_key: String,
    day_key: String,
}

#[cfg(test)]
fn sync_rollup_stats_bucket_key(event: &UsageEvent) -> SyncRollupStatsBucketKey {
    SyncRollupStatsBucketKey {
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
fn build_sync_rollup_stats_summaries(events: &[UsageEvent], device_id: &str) -> Vec<UsageSummary> {
    let mut buckets: BTreeMap<String, SyncRollupStatsAccumulator> = BTreeMap::new();
    for event in events {
        let key = sync_rollup_stats_bucket_key(event);
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
            .or_insert_with(|| SyncRollupStatsAccumulator {
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
                estimated_cost_usd: 0,
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
        entry.estimated_cost_usd += event.cost.estimated_api_equivalent_usd.unwrap_or(0);
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
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(bucket.period_start),
            period_end: Some(bucket.period_end),
            observed_at: bucket.observed_at,
            metadata: SummaryMetadata {
                summary_format: "daily_rollup.v1".to_string(),
                summary_version: Some("6".to_string()),
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
    use chrono::TimeZone;
    use statsai_core::{
        event_id, subscription_id, summary_id, BillingPeriod, CostInfo, EventSource,
        IdentitySource, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo,
        ProviderAccount, SessionInfo, SourceKind, Subscription, SubscriptionStatus,
        SummaryMetadata, UsageCounts, UsageSummary, PROVIDER_ACCOUNT_SCHEMA_VERSION,
        SUBSCRIPTION_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
    };
    use std::path::Path;
    use std::sync::mpsc;
    use tiny_http::{Header, Method, Response, Server};

    #[derive(Clone)]
    struct TestAdapter {
        provider: &'static str,
        discovered: Vec<SourceLocation>,
        scan_result: statsai_adapters::AdapterScan,
        probe_result: Option<VerifiedSourceState>,
        scan_calls: Option<Arc<Mutex<u64>>>,
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

        fn probe_verified_source_state(
            &self,
            _source: &SourceLocation,
        ) -> Result<Option<VerifiedSourceState>> {
            Ok(self
                .probe_result
                .clone()
                .or_else(|| self.scan_result.verified_source_state.clone()))
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            if let Some(scan_calls) = &self.scan_calls {
                let mut calls = scan_calls.lock().expect("scan call mutex");
                *calls += 1;
            }
            Ok(self.scan_result.clone())
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

        let mut summary = test_summary("codex", &source, now, 100, None);
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
        );
        let local_source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0",
            Path::new("/tmp/.claude"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let mut reported = test_summary("claude_code", &source, now, 100, None);
        reported.source.source_kind = SourceKind::ExternalReport;
        reported.metadata.summary_format = "external_daily".to_string();
        let mut local = test_summary("claude_code", &local_source, now, 200, None);
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
        );
        let other_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "reported-file-b",
            None,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");

        let mut matching = test_summary("claude_code", &source, now, 100, None);
        matching.source.source_kind = SourceKind::ExternalReport;
        matching.metadata.summary_format = "external_daily".to_string();
        matching.period_start = Some(now - Duration::days(1));
        matching.period_end = Some(now);

        let mut same_file_different_day = test_summary("claude_code", &source, now, 200, None);
        same_file_different_day.summary_id =
            summary_id("claude_code", &source.source_id, "other-day");
        same_file_different_day.source.source_kind = SourceKind::ExternalReport;
        same_file_different_day.metadata.summary_format = "external_daily".to_string();
        same_file_different_day.period_start = Some(now - Duration::days(2));
        same_file_different_day.period_end = Some(now - Duration::days(1));

        let mut same_period_different_file =
            test_summary("claude_code", &other_source, now, 300, None);
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
    fn subscription_add_uses_canonical_provider_for_account_id() {
        let store = Store::in_memory().expect("store");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Add {
                    provider: "claude".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Pro".to_string(),
                    price: 20.0,
                    currency: "USD".to_string(),
                    paid_at: Some("2026-05-15".to_string()),
                    started_at: "2026-05-15".to_string(),
                    ended_at: None,
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
            provider_account_id_from_identity("claude_code", None, Some("personal@example.com"))
                .expect("account id")
        );
    }

    #[test]
    fn persist_source_upserts_into_store() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-preview-source"),
            LocationOrigin::Configured,
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
        );
        let configured = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-merge"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
    }

    #[test]
    fn configured_parent_source_suppresses_discovered_child_source() {
        let discovered = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/statsai-claude/projects"),
            LocationOrigin::Default,
        );
        let configured = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/statsai-claude"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![discovered],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
        assert_eq!(
            sources[0].path_label.as_deref(),
            Some("/tmp/statsai-claude")
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
        );
        let configured_manual = SourceLocation::reported_usage(
            "codex",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "manual-note",
            None,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources =
            scan_sources_for_adapter(&adapter, &[configured_local.clone(), configured_manual]);

        assert_eq!(sources, vec![configured_local]);
    }

    #[test]
    fn connect_source_to_account_closes_existing_open_connection() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let first_start = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("first");
        let second_start = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("second");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("first@example.com"),
                label: None,
                started_at: first_start,
                ended_at: None,
            },
        )
        .expect("first connect");
        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("second@example.com"),
                label: None,
                started_at: second_start,
                ended_at: None,
            },
        )
        .expect("second connect");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].ended_at, Some(second_start));
        assert_eq!(assignments[1].started_at, second_start);
    }

    #[test]
    fn connect_source_to_account_preserves_tail_when_replacing_finite_window() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect-tail"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let period_start = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("period start");
        let split_at = Utc
            .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
            .single()
            .expect("split");
        let period_end = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("period end");
        let before_split = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .expect("before split");
        let after_split = Utc
            .with_ymd_and_hms(2026, 5, 20, 12, 0, 0)
            .single()
            .expect("after split");
        store
            .insert_event(&test_event(
                "codex",
                &source,
                before_split,
                None,
                TokenParts::total(1),
            ))
            .expect("before event");
        store
            .insert_event(&test_event(
                "codex",
                &source,
                after_split,
                None,
                TokenParts::total(1),
            ))
            .expect("after event");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("first@example.com"),
                label: None,
                started_at: period_start,
                ended_at: Some(period_end),
            },
        )
        .expect("first connect");
        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("second@example.com"),
                label: None,
                started_at: period_start,
                ended_at: Some(split_at),
            },
        )
        .expect("second connect");

        let first_account =
            provider_account_id_from_identity("codex", None, Some("first@example.com"))
                .expect("first account");
        let second_account =
            provider_account_id_from_identity("codex", None, Some("second@example.com"))
                .expect("second account");
        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        assert!(assignments.iter().any(|assignment| {
            assignment.provider_account_id == second_account
                && assignment.started_at == period_start
                && assignment.ended_at == Some(split_at)
        }));
        assert!(assignments.iter().any(|assignment| {
            assignment.provider_account_id == first_account
                && assignment.started_at == split_at
                && assignment.ended_at == Some(period_end)
        }));

        let events = store
            .events_for_source(&source.source_id)
            .expect("source events");
        assert_eq!(events.len(), 2);
        let before = events
            .iter()
            .find(|event| event.session.started_at == before_split)
            .expect("before event");
        let after = events
            .iter()
            .find(|event| event.session.started_at == after_split)
            .expect("after event");
        assert_eq!(before.provider_account_id, Some(second_account));
        assert_eq!(after.provider_account_id, Some(first_account));
    }

    #[test]
    fn connect_source_to_account_merges_same_account_and_backfills_boundary_events() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect-merge"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let original_start = Utc
            .with_ymd_and_hms(2026, 5, 28, 11, 31, 9)
            .single()
            .expect("original start");
        let extended_start = Utc
            .with_ymd_and_hms(2026, 5, 28, 0, 0, 0)
            .single()
            .expect("extended start");
        let boundary_event_at = Utc
            .with_ymd_and_hms(2026, 5, 28, 7, 23, 28)
            .single()
            .expect("boundary event");

        let event = test_event(
            "codex",
            &source,
            boundary_event_at,
            None,
            TokenParts::total(1),
        );
        store.insert_event(&event).expect("event");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("same-account@example.com"),
                label: None,
                started_at: original_start,
                ended_at: None,
            },
        )
        .expect("initial connect");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("same-account@example.com"),
                label: None,
                started_at: extended_start,
                ended_at: None,
            },
        )
        .expect("extended connect");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, extended_start);

        let events = store
            .events_for_source(&source.source_id)
            .expect("source events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].provider_account_id,
            provider_account_id_from_identity("codex", None, Some("same-account@example.com"))
        );
    }

    #[test]
    fn apply_verified_source_state_reuses_existing_email_account() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verified-state"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let existing = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("existing@example.com"),
                label: Some("existing-alias".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("existing account");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let current_period_ends_at = Utc
            .with_ymd_and_hms(2026, 6, 29, 10, 12, 43)
            .single()
            .expect("current_period_ends_at");

        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("chatgpt-account-123".to_string()),
                email: Some("existing@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(verified_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: Some(current_period_ends_at),
                    current_period_ends_at: Some(current_period_ends_at),
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
        )
        .expect("apply verified state");

        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].provider_account_id,
            existing.provider_account_id
        );
        assert_eq!(
            accounts[0].provider_user_id.as_deref(),
            Some("chatgpt-account-123")
        );
        assert_eq!(accounts[0].plan_name.as_deref(), Some("Plus"));
        assert_eq!(accounts[0].verified_at, Some(verified_at));

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            existing.provider_account_id
        );
        assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);
        assert_eq!(assignments[0].verified_at, Some(verified_at));

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(
            subscriptions[0].provider_account_id,
            existing.provider_account_id
        );
        assert_eq!(subscriptions[0].record_source, IdentitySource::LocalAuth);
        assert_eq!(
            subscriptions[0].current_period_ends_at,
            Some(current_period_ends_at)
        );
        assert_eq!(subscriptions[0].ended_at, None);
    }

    #[test]
    fn upsert_provider_account_rejects_conflicting_email_and_provider_user_id() {
        let store = Store::in_memory().expect("store");
        let email_account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("conflict@example.com"),
                label: Some("email".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("email account");
        let user_account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-conflict"),
                email: None,
                label: Some("user".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("user account");

        let error = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-conflict"),
                email: Some("conflict@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: None,
            },
        )
        .expect_err("conflicting identity");

        assert!(error
            .to_string()
            .contains("conflicting provider account identifiers"));
        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|account| {
            account.provider_account_id == email_account.provider_account_id
                && account.provider_user_id.is_none()
                && account.email.as_deref() == Some("conflict@example.com")
        }));
        assert!(accounts.iter().any(|account| {
            account.provider_account_id == user_account.provider_account_id
                && account.provider_user_id.as_deref() == Some("acct-conflict")
                && account.email.is_none()
        }));
    }

    #[test]
    fn lookup_provider_account_does_not_create_orphans() {
        let store = Store::in_memory().expect("store");

        let error = resolve_existing_provider_account(
            &store,
            "codex",
            None,
            None,
            Some("typo@example.com"),
            None,
        )
        .expect_err("missing account");

        assert!(error
            .to_string()
            .contains("unknown provider account selector"));
        assert!(store.list_accounts().expect("accounts").is_empty());
    }

    #[test]
    fn provider_account_id_lookup_rejects_wrong_provider() {
        let store = Store::in_memory().expect("store");
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "claude_code",
                provider_user_id: None,
                email: Some("claude@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("account");

        let existing_error = resolve_existing_provider_account(
            &store,
            "codex",
            Some(&account.provider_account_id.0),
            None,
            None,
            None,
        )
        .expect_err("wrong existing provider");
        let create_error = resolve_or_create_provider_account(
            &store,
            "codex",
            Some(&account.provider_account_id.0),
            Some("codex-user"),
            None,
            None,
        )
        .expect_err("wrong create provider");

        assert!(existing_error
            .to_string()
            .contains("belongs to claude_code"));
        assert!(create_error.to_string().contains("belongs to claude_code"));
    }

    #[test]
    fn subscription_change_requires_active_existing_subscription() {
        let store = Store::in_memory().expect("store");
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("change@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("account");

        let error = subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Change {
                    provider: "codex".to_string(),
                    provider_account_id: Some(account.provider_account_id.0.clone()),
                    provider_user_id: None,
                    email: None,
                    label: None,
                    plan: "Pro".to_string(),
                    price: 200.0,
                    currency: "USD".to_string(),
                    paid_at: None,
                    started_at: "2026-06-01".to_string(),
                },
            },
            &store,
        )
        .expect_err("missing active subscription");

        assert!(error
            .to_string()
            .contains("subscription change requires an active subscription"));
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
        assert_eq!(store.list_accounts().expect("accounts").len(), 1);
    }

    #[test]
    fn scan_applies_verified_source_state_even_when_source_files_are_unchanged() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-work-upgrade"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let legacy_start = Utc
            .with_ymd_and_hms(2026, 5, 24, 20, 10, 31)
            .single()
            .expect("legacy_start");
        let mut legacy_assignment = connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("work"),
                label: Some("work".to_string()),
                started_at: legacy_start,
                ended_at: None,
            },
        )
        .expect("legacy work assignment");
        legacy_assignment.record_source = IdentitySource::Unknown;
        store
            .upsert_source_account_assignment(&legacy_assignment)
            .expect("legacy assignment");

        let started_at = Utc
            .with_ymd_and_hms(2026, 4, 30, 7, 43, 17)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 30, 7, 43, 18)
            .single()
            .expect("verified_at");
        let current_period_ends_at = Utc
            .with_ymd_and_hms(2026, 5, 30, 7, 43, 17)
            .single()
            .expect("current_period_ends_at");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan {
                diagnostics: ScanDiagnostics {
                    files_skipped_unchanged: 1,
                    ..ScanDiagnostics::default()
                },
                verified_source_state: Some(VerifiedSourceState {
                    provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
                    email: Some("verified@example.com".to_string()),
                    account_label: None,
                    plan_name: Some("Plus".to_string()),
                    authenticated_at: Some(started_at),
                    verified_at: Some(verified_at),
                    subscription: Some(VerifiedSubscriptionState {
                        plan_name: "Plus".to_string(),
                        price: 2000,
                        currency: "USD".to_string(),
                        billing_period: BillingPeriod::Monthly,
                        paid_at: Some(started_at),
                        started_at,
                        ended_at: Some(current_period_ends_at),
                        current_period_ends_at: Some(current_period_ends_at),
                        status: SubscriptionStatus::Active,
                        verified_at: Some(verified_at),
                    }),
                }),
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        let expected_account_id = provider_account_id_from_identity(
            "codex",
            Some("11111111-2222-4333-8444-555555555555"),
            Some("verified@example.com"),
        )
        .expect("expected account id");

        let accounts = store.list_accounts().expect("accounts");
        assert!(accounts.iter().any(|account| {
            account.provider_account_id == expected_account_id
                && account.email.as_deref() == Some("verified@example.com")
                && account.plan_name.as_deref() == Some("Plus")
        }));

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, started_at);
        assert_eq!(assignments[0].ended_at, None);
        assert_eq!(assignments[0].provider_account_id, expected_account_id);
        assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].provider_account_id, expected_account_id);
        assert_eq!(subscriptions[0].record_source, IdentitySource::LocalAuth);
        assert_eq!(subscriptions[0].ended_at, None);
        assert_eq!(
            subscriptions[0].current_period_ends_at,
            Some(current_period_ends_at)
        );
        let stored_source = store
            .source(&source.source_id)
            .expect("source row")
            .expect("stored source");
        assert!(stored_source.verified_state_hash.is_some());
    }

    #[test]
    fn scan_reopens_existing_verified_assignment_when_auth_is_still_current() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-reopen-verified"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
            .single()
            .expect("started_at");
        let closed_at = Utc
            .with_ymd_and_hms(2026, 5, 24, 20, 10, 31)
            .single()
            .expect("closed_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
            .single()
            .expect("verified_at");

        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("11111111-2222-4333-8444-555555555555"),
                email: Some("verified@example.com"),
                label: None,
                plan_name: Some("Plus".to_string()),
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(verified_at),
            },
        )
        .expect("account");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source.source_id,
                    &account.provider_account_id,
                    started_at,
                ),
                source_id: source.source_id.clone(),
                provider: source.provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                started_at,
                ended_at: Some(closed_at),
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(verified_at),
                created_at: started_at,
                updated_at: closed_at,
            })
            .expect("closed assignment");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan {
                diagnostics: ScanDiagnostics {
                    files_skipped_unchanged: 1,
                    ..ScanDiagnostics::default()
                },
                verified_source_state: Some(VerifiedSourceState {
                    provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
                    email: Some("verified@example.com".to_string()),
                    account_label: None,
                    plan_name: Some("Plus".to_string()),
                    authenticated_at: Some(started_at),
                    verified_at: Some(verified_at),
                    subscription: None,
                }),
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            account.provider_account_id
        );
        assert_eq!(assignments[0].started_at, started_at);
        assert_eq!(assignments[0].ended_at, None);
    }

    #[test]
    fn scan_skips_full_scan_when_usage_and_verified_state_are_unchanged() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-scan-skip"),
            LocationOrigin::Configured,
        );
        let verified_state = VerifiedSourceState {
            provider_user_id: Some("acct-verified".to_string()),
            email: Some("verified@example.com".to_string()),
            account_label: None,
            plan_name: Some("Plus".to_string()),
            authenticated_at: Some(
                Utc.with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
                    .single()
                    .expect("authenticated_at"),
            ),
            verified_at: Some(
                Utc.with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
                    .single()
                    .expect("verified_at"),
            ),
            subscription: None,
        };
        source.verified_state_hash =
            verified_source_state_hash(Some(&verified_state)).expect("verified state hash");
        store.upsert_source(&source).expect("source");

        let scan_calls = Arc::new(Mutex::new(0u64));
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: Some(verified_state),
            scan_calls: Some(scan_calls.clone()),
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        assert_eq!(*scan_calls.lock().expect("scan calls"), 0);
    }

    #[test]
    fn scan_closes_verified_assignment_when_auto_source_loses_auth() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-unassign-on-missing-auth"),
            LocationOrigin::Configured,
        );
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let verified_state = VerifiedSourceState {
            provider_user_id: Some("acct-verified".to_string()),
            email: Some("verified@example.com".to_string()),
            account_label: None,
            plan_name: Some("Plus".to_string()),
            authenticated_at: Some(started_at),
            verified_at: Some(verified_at),
            subscription: None,
        };
        source.verified_state_hash =
            verified_source_state_hash(Some(&verified_state)).expect("verified state hash");
        store.upsert_source(&source).expect("source");

        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: verified_state.provider_user_id.as_deref(),
                email: verified_state.email.as_deref(),
                label: None,
                plan_name: verified_state.plan_name.clone(),
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: verified_state.verified_at,
            },
        )
        .expect("account");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source.source_id,
                    &account.provider_account_id,
                    started_at,
                ),
                source_id: source.source_id.clone(),
                provider: source.provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                started_at,
                ended_at: None,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(verified_at),
                created_at: started_at,
                updated_at: started_at,
            })
            .expect("assignment");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].ended_at.is_some());
        let stored_source = store
            .source(&source.source_id)
            .expect("source row")
            .expect("stored source");
        assert_eq!(stored_source.verified_state_hash, None);
    }

    #[test]
    fn scan_closes_legacy_verified_assignment_without_state_hash_when_auth_is_missing() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-legacy-unassign-on-missing-auth"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-legacy-verified"),
                email: Some("legacy-verified@example.com"),
                label: None,
                plan_name: Some("Plus".to_string()),
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(verified_at),
            },
        )
        .expect("account");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source.source_id,
                    &account.provider_account_id,
                    started_at,
                ),
                source_id: source.source_id.clone(),
                provider: source.provider.clone(),
                provider_account_id: account.provider_account_id.clone(),
                started_at,
                ended_at: None,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(verified_at),
                created_at: started_at,
                updated_at: started_at,
            })
            .expect("assignment");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].ended_at.is_some());
    }

    #[test]
    fn manual_only_source_ignores_verified_state_mutations() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-manual-only"),
            LocationOrigin::Configured,
        );
        source.verification_mode = SourceVerificationMode::ManualOnly;
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: Some(VerifiedSourceState {
                provider_user_id: Some("acct-manual-only".to_string()),
                email: Some("manual-only@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(verified_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: None,
                    current_period_ends_at: None,
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("scan");

        assert!(store.list_accounts().expect("accounts").is_empty());
        assert!(store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments")
            .is_empty());
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
    }

    #[test]
    fn disabled_source_mode_closes_verified_linkages() {
        let store = Store::in_memory().expect("store");
        let mut source_location = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-disable-verification"),
            LocationOrigin::Configured,
        );
        source_location.verified_state_hash = Some("verified-state".to_string());
        store.upsert_source(&source_location).expect("source");
        let started_at = Utc::now() - Duration::days(1);
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-disable"),
                email: Some("disable@example.com"),
                label: None,
                plan_name: Some("Plus".to_string()),
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(started_at),
            },
        )
        .expect("account");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source_location.source_id,
                    &account.provider_account_id,
                    started_at,
                ),
                source_id: source_location.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                started_at,
                ended_at: None,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(started_at),
                created_at: started_at,
                updated_at: started_at,
            })
            .expect("assignment");
        store
            .upsert_subscription(&Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    "codex",
                    &account.provider_account_id,
                    "Plus",
                    started_at,
                ),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                renewal_day: None,
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(started_at),
                notes: None,
            })
            .expect("subscription");

        source(
            SourceCommand {
                command: SourceSubcommand::Mode {
                    source_id: Some(source_location.source_id.0.clone()),
                    path: None,
                    mode: "disabled".to_string(),
                },
            },
            &store,
        )
        .expect("disable mode");

        let source = store
            .source(&source_location.source_id)
            .expect("source lookup")
            .expect("source exists");
        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        let subscriptions = store.list_subscriptions().expect("subscriptions");

        assert_eq!(source.verification_mode, SourceVerificationMode::Disabled);
        assert_eq!(source.verified_state_hash, None);
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].ended_at.is_some());
        assert_eq!(subscriptions.len(), 1);
        assert!(subscriptions[0].ended_at.is_some());
    }

    #[test]
    fn apply_verified_source_state_does_not_override_conflicting_manual_connection() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-manual-wins"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let manual = connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("manual@example.com"),
                label: Some("manual".to_string()),
                started_at,
                ended_at: None,
            },
        )
        .expect("manual connection");

        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("chatgpt-account-999".to_string()),
                email: Some("verified@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(started_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: None,
                    current_period_ends_at: None,
                    status: SubscriptionStatus::Active,
                    verified_at: Some(started_at),
                }),
            }),
        )
        .expect("apply verified state");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            manual.provider_account_id
        );
        assert_eq!(assignments[0].record_source, IdentitySource::UserConfigured);
    }

    #[test]
    fn subscription_change_closes_existing_period() {
        let store = Store::in_memory().expect("store");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Add {
                    provider: "codex".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Plus".to_string(),
                    price: 20.0,
                    currency: "USD".to_string(),
                    paid_at: Some("2026-05-01".to_string()),
                    started_at: "2026-05-01".to_string(),
                    ended_at: None,
                },
            },
            &store,
        )
        .expect("subscription add");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Change {
                    provider: "codex".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Pro".to_string(),
                    price: 200.0,
                    currency: "USD".to_string(),
                    paid_at: Some("2026-06-01".to_string()),
                    started_at: "2026-06-01".to_string(),
                },
            },
            &store,
        )
        .expect("subscription change");

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 2);
        assert!(subscriptions
            .iter()
            .any(|subscription| subscription.plan_name == "Plus"
                && subscription.ended_at
                    == Some(
                        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                            .single()
                            .expect("end")
                    )));
        assert!(
            subscriptions
                .iter()
                .any(|subscription| subscription.plan_name == "Pro"
                    && subscription.ended_at.is_none())
        );
    }

    #[test]
    fn active_subscription_treats_legacy_verified_cycle_rows_as_current_periods() {
        let store = Store::in_memory().expect("store");
        let account_id = provider_account_id("codex", "verified@example.com");
        let started_at = Utc
            .with_ymd_and_hms(2026, 4, 30, 7, 43, 17)
            .single()
            .expect("started_at");
        let period_end = Utc
            .with_ymd_and_hms(2026, 5, 30, 7, 43, 17)
            .single()
            .expect("period_end");
        store
            .upsert_subscription(&Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id("codex", &account_id, "Plus", started_at),
                provider: "codex".to_string(),
                provider_account_id: account_id.clone(),
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                renewal_day: Some(30),
                started_at,
                ended_at: Some(period_end),
                current_period_ends_at: Some(period_end),
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(
                    Utc.with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
                        .single()
                        .expect("verified_at"),
                ),
                notes: None,
            })
            .expect("legacy subscription");

        let active = active_subscription(
            &store,
            "codex",
            &account_id,
            Some("Plus"),
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                .single()
                .expect("lookup"),
        )
        .expect("active subscription");

        assert_eq!(active.provider_account_id, account_id);
        assert_eq!(active.plan_name, "Plus");
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
                output: Some(output.clone()),
                dry_run: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert!(!output.exists());
    }

    #[test]
    fn http_sync_uses_configured_or_default_api_endpoint() {
        let previous = std::env::var("STATSAI_API_URL").ok();
        std::env::set_var("STATSAI_API_URL", "https://sync.example.com");
        let endpoint = http_sync_endpoint(&test_sync_command("http")).expect("http endpoint");
        if let Some(value) = previous {
            std::env::set_var("STATSAI_API_URL", value);
        } else {
            std::env::remove_var("STATSAI_API_URL");
        }

        assert_eq!(endpoint, "https://sync.example.com/api/sync/batches");
    }

    #[test]
    fn http_sync_builds_rollup_batches_without_raw_events() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(!batch.summaries.is_empty());
        assert!(batch.summaries.iter().all(is_daily_rollup_summary));
    }

    #[test]
    fn first_http_incremental_sync_sends_full_rollup_history_for_new_target() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-first-sync"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let existing_rollups = store
            .all_sync_rollup_summaries()
            .expect("all rollups for new target");
        assert_eq!(existing_rollups.len(), 1);

        store
            .mark_sync_rollups_synced(
                &existing_rollups
                    .iter()
                    .map(|summary| summary.summary_id.clone())
                    .collect::<Vec<_>>(),
            )
            .expect("clear dirty flags");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups")
            .is_empty());

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            since_last: true,
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert!(is_daily_rollup_summary(&batch.summaries[0]));
    }

    #[test]
    fn http_incremental_rollups_are_tracked_per_target() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-targets"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let account_id = provider_account_id("codex", "personal");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let first = test_event(
            "codex",
            &source,
            started_at,
            Some(account_id.clone()),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&first).expect("first event");
        store.rebuild_sync_rollups().expect("rebuild");

        let local_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let local_target = sync_target(&local_command).expect("local target");
        let (local_batch, local_mode) =
            build_sync_batch(&local_command, &store, "device", &local_target)
                .expect("local initial batch");
        assert_eq!(local_mode, SyncPayloadMode::Rollups);
        assert_eq!(local_batch.summaries.len(), 1);
        record_rollup_sync_success(&store, "http", &local_target, &local_batch)
            .expect("record local sync");

        let local_incremental_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            since_last: true,
            ..test_sync_command("http")
        };
        let (local_incremental_batch, _) =
            build_sync_batch(&local_incremental_command, &store, "device", &local_target)
                .expect("local incremental batch");
        assert!(local_incremental_batch.summaries.is_empty());

        let second = test_event(
            "codex",
            &source,
            started_at + Duration::hours(1),
            Some(account_id),
            TokenParts {
                input: 20,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 25,
                cost: Some(20),
            },
        );
        store.insert_event(&second).expect("second event");
        assert_eq!(
            store
                .dirty_sync_rollup_summaries()
                .expect("dirty after second event")
                .len(),
            1
        );

        let remote_command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let remote_target = sync_target(&remote_command).expect("remote target");
        let (remote_batch, remote_mode) =
            build_sync_batch(&remote_command, &store, "device", &remote_target)
                .expect("remote batch");
        assert_eq!(remote_mode, SyncPayloadMode::Rollups);
        assert_eq!(remote_batch.summaries.len(), 1);
        record_rollup_sync_success(&store, "http", &remote_target, &remote_batch)
            .expect("record remote sync");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("dirty after remote sync")
            .is_empty());

        let (local_catchup_batch, local_catchup_mode) =
            build_sync_batch(&local_incremental_command, &store, "device", &local_target)
                .expect("local catchup batch");
        assert_eq!(local_catchup_mode, SyncPayloadMode::Rollups);
        assert_eq!(local_catchup_batch.summaries.len(), 1);
        assert_eq!(
            local_catchup_batch.summaries[0].usage.total_tokens,
            Some(40)
        );
    }

    #[test]
    fn http_rollup_sync_splits_large_summary_batches() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-chunks"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summaries: Vec<_> = (0..(HTTP_ROLLUP_SUMMARIES_PER_BATCH * 2 + 4))
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &source,
                    now + Duration::days(index as i64),
                    10,
                    None,
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_large".to_string(),
            device_id: "device".to_string(),
            sources: vec![source.clone()],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].batch_id, "batch_large_sources_1");
        assert_eq!(chunks[1].batch_id, "batch_large_part_1_of_3");
        assert_eq!(chunks[2].batch_id, "batch_large_part_2_of_3");
        assert_eq!(chunks[3].batch_id, "batch_large_part_3_of_3");
        assert!(chunks[0].summaries.is_empty());
        assert_eq!(chunks[1].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert_eq!(chunks[2].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert_eq!(chunks[3].summaries.len(), 4);
        assert_eq!(chunks[0].sources.len(), 1);
        assert!(chunks[1].sources.is_empty());
        assert!(chunks[2].sources.is_empty());
        assert!(chunks[3].sources.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
    }

    #[test]
    fn http_rollup_sync_splits_metadata_away_from_summaries() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let sources: Vec<_> = (0..17)
            .map(|index| {
                SourceLocation::local_adapter(
                    "codex",
                    format!("test-{index}"),
                    "0",
                    Path::new("/tmp/codex-http-metadata"),
                    LocationOrigin::Configured,
                )
            })
            .collect();
        let accounts: Vec<_> = (0..7)
            .map(|index| {
                test_account(
                    "codex",
                    Some(&format!("account-{index}")),
                    None,
                    None,
                    Some("Pro"),
                    now,
                )
            })
            .collect();
        let assignments: Vec<_> = (0..16)
            .map(|index| {
                test_assignment(
                    &sources[index],
                    &accounts[index % accounts.len()].provider_account_id,
                    now + Duration::days(index as i64),
                    None,
                    now,
                )
            })
            .collect();
        let subscriptions: Vec<_> = accounts
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, account)| Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    "codex",
                    &account.provider_account_id,
                    &format!("pro-{index}"),
                    now,
                ),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: "Pro".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: None,
                renewal_day: None,
                started_at: now,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            })
            .collect();
        let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &sources[index % sources.len()],
                    now + Duration::days(index as i64),
                    10,
                    Some(accounts[index % accounts.len()].provider_account_id.clone()),
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata".to_string(),
            device_id: "device".to_string(),
            sources,
            accounts,
            source_account_assignments: assignments,
            subscriptions,
            events: vec![],
            summaries,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].batch_id, "batch_metadata_sources_1");
        assert_eq!(chunks[1].batch_id, "batch_metadata_accounts_1");
        assert_eq!(chunks[2].batch_id, "batch_metadata_assignments_1");
        assert_eq!(chunks[3].batch_id, "batch_metadata_subscriptions_1");
        assert_eq!(chunks[4].batch_id, "batch_metadata_part_1_of_1");
        assert_eq!(chunks[0].sources.len(), 17);
        assert_eq!(chunks[1].accounts.len(), 7);
        assert_eq!(chunks[2].source_account_assignments.len(), 16);
        assert_eq!(chunks[3].subscriptions.len(), 3);
        assert_eq!(chunks[4].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert!(chunks[..4].iter().all(|chunk| chunk.summaries.is_empty()));
        assert!(chunks[4].sources.is_empty());
        assert!(chunks[4].accounts.is_empty());
        assert!(chunks[4].source_account_assignments.is_empty());
        assert!(chunks[4].subscriptions.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
    }

    #[test]
    fn http_rollup_sync_retries_smaller_batches_after_budget_rejection() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let endpoint = format!("http://{}/api/sync/batches", server.server_addr());
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let mut request = server.recv().expect("request");
                assert_eq!(request.method(), &Method::Post);
                assert_eq!(request.url(), "/api/sync/batches");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).expect("body");
                let payload: Value = serde_json::from_str(&body).expect("payload json");
                let batch_id = payload
                    .get("batch_id")
                    .and_then(Value::as_str)
                    .expect("batch id")
                    .to_string();
                let summary_count = payload
                    .get("summaries")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                tx.send((batch_id.clone(), summary_count))
                    .expect("observed request");

                let response = if summary_count > 2 {
                    Response::from_string(
                        r#"{"error":"sync_batch_d1_query_budget_exceeded","estimatedQueries":53,"maxQueries":45}"#,
                    )
                    .with_status_code(413)
                } else {
                    Response::from_string(test_sync_ack_json(&batch_id))
                }
                .with_header(Header::from_bytes("content-type", "application/json").unwrap());
                request.respond(response).expect("respond");
            }
        });

        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-retry"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summaries: Vec<_> = (0..4)
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &source,
                    now + Duration::days(index as i64),
                    10,
                    None,
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
                summary.metadata.summary_format = "daily_rollup.v1".to_string();
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_retry".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries,
            created_at: now,
        };

        send_http_sync_batch(
            &store,
            "http",
            &endpoint,
            &endpoint,
            None,
            &batch,
            SyncPayloadMode::Rollups,
        )
        .expect("send");

        handle.join().expect("server thread");
        let observed: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            observed,
            vec![
                ("batch_retry".to_string(), 4),
                ("batch_retry_part_1_of_2".to_string(), 2),
                ("batch_retry_part_2_of_2".to_string(), 2),
            ]
        );
        let state = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");
        assert_eq!(state.last_batch_id, "batch_retry");
    }

    #[test]
    fn http_rollup_metadata_budget_retries_preserve_all_metadata_kinds() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let sources: Vec<_> = (0..4)
            .map(|index| {
                SourceLocation::local_adapter(
                    "codex",
                    format!("retry-source-{index}"),
                    "0",
                    Path::new("/tmp/codex-http-metadata-retry"),
                    LocationOrigin::Configured,
                )
            })
            .collect();
        let accounts: Vec<_> = (0..3)
            .map(|index| {
                test_account(
                    "codex",
                    Some(&format!("retry-account-{index}")),
                    None,
                    None,
                    Some("Pro"),
                    now,
                )
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata_retry".to_string(),
            device_id: "device".to_string(),
            sources: sources.clone(),
            accounts: accounts.clone(),
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries: vec![],
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.sources.len())
                .sum::<usize>(),
            sources.len()
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.accounts.len())
                .sum::<usize>(),
            accounts.len()
        );
        assert!(chunks
            .iter()
            .all(|chunk| chunk.source_account_assignments.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.subscriptions.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.summaries.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
        assert!(chunks.iter().any(|chunk| !chunk.sources.is_empty()));
        assert!(chunks.iter().any(|chunk| !chunk.accounts.is_empty()));
    }

    #[test]
    fn http_rollup_sync_proactively_splits_batches_to_fit_d1_budget() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-budget"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &source,
                    now + Duration::days((index * 31) as i64),
                    10,
                    None,
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-budget-{index}"));
                summary.project = Some(ProjectInfo {
                    project_id: format!("project-budget-{index}"),
                    project_label: Some(format!("Project {index}")),
                    repo_remote_hash: None,
                    repo_label: None,
                    branch_hash: None,
                    branch_label: None,
                    path_hash: Some(format!("path-hash-{index}")),
                    path_label: Some(format!("/tmp/project-{index}")),
                });
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_budget".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.summaries.len())
                .sum::<usize>(),
            25
        );
        assert!(chunks.iter().all(|chunk| chunk.sources.is_empty()));
        assert!(chunks
            .iter()
            .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.summaries.len())
                .collect::<Vec<_>>(),
            vec![7, 6, 6, 6]
        );
    }

    #[test]
    fn remote_sync_batch_match_requires_same_last_batch_id() {
        let store = Store::in_memory().expect("store");
        store
            .record_sync_success(
                "http",
                "https://api.example.com/api/sync/batches",
                "batch_1_part_2_of_2",
                &[],
                &[],
            )
            .expect("record sync success");
        let local_state = store
            .sync_state("http", "https://api.example.com/api/sync/batches")
            .expect("state")
            .expect("present");

        assert!(remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": "batch_1"
                }
            }),
            &local_state
        ));
        assert!(!remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": null
                }
            }),
            &local_state
        ));
        assert!(!remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": "batch_2"
                }
            }),
            &local_state
        ));
    }

    #[test]
    fn logical_http_rollup_batch_id_strips_known_chunk_suffixes() {
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_part_11_of_11"),
            "batch_1"
        );
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_part_11_of_11_part_1_of_2"),
            "batch_1"
        );
        assert_eq!(logical_http_rollup_batch_id("batch_1_sources_1"), "batch_1");
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_part_3_of_9_sources_1"),
            "batch_1"
        );
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_subscriptions_2"),
            "batch_1"
        );
        assert_eq!(logical_http_rollup_batch_id("batch_1"), "batch_1");
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_part_final"),
            "batch_1_part_final"
        );
    }

    #[test]
    fn full_http_sync_resends_metadata_after_tracking_is_cleared() {
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-reset-tracking"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("acct-real".to_string()),
                email: Some("verified@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(verified_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: None,
                    current_period_ends_at: Some(started_at + Duration::days(30)),
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
        )
        .expect("verified state");

        let account_id = store.list_accounts().expect("accounts")[0]
            .provider_account_id
            .clone();
        let event = test_event(
            "codex",
            &source,
            started_at + Duration::hours(1),
            Some(account_id),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");

        let (initial_batch, initial_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_mode, SyncPayloadMode::Rollups);
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        let all_sources = store.list_sources().expect("sources");
        let all_accounts = store.list_accounts().expect("accounts");

        let sync_sources: Vec<_> = all_sources
            .iter()
            .cloned()
            .map(sanitize_source_for_sync)
            .collect();
        let sync_accounts: Vec<_> = all_accounts
            .iter()
            .cloned()
            .map(sanitize_account_for_sync)
            .collect();
        assert_eq!(
            store
                .pending_sources_for_sync("http", &target, &sync_sources)
                .expect("pending sources")
                .len(),
            0
        );
        assert_eq!(
            store
                .pending_accounts_for_sync("http", &target, &sync_accounts)
                .expect("pending accounts")
                .len(),
            0
        );

        let local_state = store
            .sync_state("http", &target)
            .expect("state")
            .expect("present");
        let local_verify =
            sync_local_verify(&store, "http", &target, Some(&local_state)).expect("local verify");
        assert_eq!(
            remote_metadata_gap_reason(
                &json!({
                    "device": {
                        "last_sync_batch_id": initial_batch.batch_id
                    },
                    "mirrorCounts": {
                        "sources": 0,
                        "accounts": 0,
                        "source_account_assignments": 0,
                        "subscriptions": 0,
                        "summaries": 0,
                        "sync_batches": 1
                    }
                }),
                &local_verify
            )
            .as_deref(),
            Some("sources 0<1, accounts 0<1, source_account_assignments 0<1, subscriptions 0<1")
        );

        store
            .clear_sync_tracking_for_target("http", &target)
            .expect("clear tracking");

        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert_eq!(batch.sources.len(), 1);
        assert_eq!(batch.accounts.len(), 1);
        assert_eq!(batch.source_account_assignments.len(), 1);
        assert_eq!(batch.subscriptions.len(), 1);
        assert_eq!(batch.summaries.len(), 1);
        assert!(is_daily_rollup_summary(&batch.summaries[0]));
    }

    #[test]
    fn http_verify_status_url_points_at_worker_status_endpoint() {
        assert_eq!(
            http_verify_status_url("https://api.example.com/api/sync/batches").expect("status"),
            "https://api.example.com/api/sync/status"
        );
    }

    #[test]
    fn http_reset_url_points_at_worker_reset_endpoint() {
        assert_eq!(
            http_reset_url("https://api.example.com/api/sync/batches").expect("reset"),
            "https://api.example.com/api/sync/reset"
        );
    }

    #[test]
    fn no_cache_scan_reselects_unchanged_files() {
        let store = Store::in_memory().expect("store");
        let source_id = statsai_core::SourceId("src-no-cache".to_string());
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
    fn full_source_rescan_replaces_existing_source_records() {
        assert!(should_replace_source_records_for_scan(false, true, 2, 2));
        assert!(should_replace_source_records_for_scan(true, false, 0, 0));
        assert!(!should_replace_source_records_for_scan(false, true, 2, 1));
        assert!(!should_replace_source_records_for_scan(false, true, 0, 0));
        assert!(!should_replace_source_records_for_scan(false, false, 2, 2));
    }

    #[test]
    fn http_verify_pending_counts_match_sanitized_sync_payloads() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verify-pending"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: provider_account_id("codex", "personal"),
            provider: "codex".to_string(),
            identity_source: IdentitySource::ManualHint,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: Some("Pro".to_string()),
            confidence: Confidence::High,
            verified_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_account(&account).expect("account");
        let started_at = Utc::now();

        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &account.provider_account_id,
                "pro",
                started_at,
            ),
            provider: "codex".to_string(),
            provider_account_id: account.provider_account_id.clone(),
            plan_name: "Pro".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: None,
            renewal_day: None,
            started_at,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            notes: Some("private note".to_string()),
        };
        store
            .upsert_subscription(&subscription)
            .expect("subscription");
        let summary = test_summary(
            "codex",
            &source,
            Utc::now(),
            42,
            Some(account.provider_account_id.clone()),
        );
        store.upsert_summary(&summary).expect("summary");

        let target = "https://api.example.com/api/sync/batches".to_string();
        store
            .record_sources_synced("http", &target, &[sanitize_source_for_sync(source.clone())])
            .expect("record sources");
        store
            .record_accounts_synced(
                "http",
                &target,
                &[sanitize_account_for_sync(account.clone())],
            )
            .expect("record accounts");
        store
            .record_subscriptions_synced(
                "http",
                &target,
                &[sanitize_subscription_for_sync(subscription.clone())],
            )
            .expect("record subscriptions");
        store
            .record_summaries_synced(
                "http",
                &target,
                &[sanitize_summary_for_sync(summary.clone())],
            )
            .expect("record summaries");

        let local = sync_local_verify(&store, "http", &target, None).expect("local verify");
        assert_eq!(local.pending_sources, 0);
        assert_eq!(local.pending_accounts, 0);
        assert_eq!(local.pending_source_account_assignments, 0);
        assert_eq!(local.pending_subscriptions, 0);
        assert_eq!(local.total_passthrough_summaries, 1);
        assert_eq!(local.pending_passthrough_summaries, 0);
    }

    #[test]
    fn sanitize_account_for_sync_preserves_user_configured_label() {
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: provider_account_id("codex", "personal"),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: Some("Pro".to_string()),
            confidence: Confidence::Medium,
            verified_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let sanitized = sanitize_account_for_sync(account);
        assert_eq!(sanitized.account_label.as_deref(), Some("personal"));
        assert_eq!(sanitized.plan_name, None);
    }

    #[test]
    fn sync_rollup_stats_summaries_roll_up_events_by_day_and_account() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-rollup-stats"),
            LocationOrigin::Configured,
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

        let summaries = build_sync_rollup_stats_summaries(
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
                        cost: Some(10),
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
                        cost: Some(30),
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
                        cost: Some(5),
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
    fn merge_provider_accounts_moves_source_records_and_prunes_alias() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-work"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("work"), None, None, None, now);
        let canonical = test_account(
            "codex",
            None,
            Some("verified@example.com"),
            Some("11111111-2222-4333-8444-555555555555"),
            Some("Plus"),
            now,
        );
        store.upsert_account(&alias).expect("alias account");
        store.upsert_account(&canonical).expect("canonical account");
        let assignment = test_assignment(
            &source,
            &alias.provider_account_id,
            now - Duration::days(40),
            None,
            now,
        );
        store
            .upsert_source_account_assignment(&assignment)
            .expect("assignment");

        let mut event = test_event(
            "codex",
            &source,
            now - Duration::days(2),
            Some(alias.provider_account_id.clone()),
            TokenParts::total(120),
        );
        event.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("event".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        let mut summary = test_summary(
            "codex",
            &source,
            now,
            300,
            Some(alias.provider_account_id.clone()),
        );
        summary.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("summary".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        store.insert_event(&event).expect("event");
        store.upsert_summary(&summary).expect("summary");

        let target = "https://api.example.com/api/sync/batches";
        store
            .record_sources_synced("http", target, &[sanitize_source_for_sync(source.clone())])
            .expect("sync source");
        store
            .record_accounts_synced(
                "http",
                target,
                &[
                    sanitize_account_for_sync(alias.clone()),
                    sanitize_account_for_sync(canonical.clone()),
                ],
            )
            .expect("sync accounts");
        store
            .record_source_account_assignments_synced(
                "http",
                target,
                &[sanitize_source_account_assignment_for_sync(
                    assignment.clone(),
                )],
            )
            .expect("sync assignments");
        store
            .record_sync_success("http", target, "batch_1", &[], &[])
            .expect("sync success");

        let report =
            merge_provider_accounts(&store, "codex", "work", "verified@example.com", false)
                .expect("merge");

        assert_eq!(report.moved_source_account_assignments, 1);
        assert_eq!(report.moved_subscriptions, 0);
        assert_eq!(report.moved_events, 1);
        assert_eq!(report.moved_summaries, 1);
        assert!(report.deleted_source_account);
        assert!(report.reset_local_sync_tracking);
        assert_eq!(report.remaining_references.total(), 0);

        let accounts = store.list_accounts().expect("accounts");
        assert!(!accounts
            .iter()
            .any(|account| account.provider_account_id == alias.provider_account_id));
        assert!(accounts
            .iter()
            .any(|account| account.provider_account_id == canonical.provider_account_id));

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            canonical.provider_account_id
        );

        let events = store.events_for_source(&source.source_id).expect("events");
        assert_eq!(
            events[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );
        let summaries = store
            .summaries_for_source(&source.source_id)
            .expect("summaries");
        assert_eq!(
            summaries[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );

        assert!(store.list_sync_states().expect("sync states").is_empty());
        let sync_accounts: Vec<_> = store
            .list_accounts()
            .expect("accounts after merge")
            .into_iter()
            .map(sanitize_account_for_sync)
            .collect();
        let pending = store
            .pending_accounts_for_sync("http", target, &sync_accounts)
            .expect("pending accounts");
        assert_eq!(pending.len(), sync_accounts.len());
    }

    #[test]
    fn merge_provider_accounts_moves_orphan_summary_rows() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-legacy-alias"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("legacy-alias"), None, None, None, now);
        let canonical = test_account(
            "codex",
            None,
            Some("canonical@example.com"),
            Some("stable-provider-id"),
            Some("Plus"),
            now,
        );
        store.upsert_account(&alias).expect("alias account");
        store.upsert_account(&canonical).expect("canonical account");

        let mut summary = test_summary(
            "codex",
            &source,
            now - Duration::days(10),
            512,
            Some(alias.provider_account_id.clone()),
        );
        summary.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("summary".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        store.upsert_summary(&summary).expect("summary");

        let report = merge_provider_accounts(
            &store,
            "codex",
            "legacy-alias",
            "canonical@example.com",
            false,
        )
        .expect("merge");

        assert_eq!(report.moved_source_account_assignments, 0);
        assert_eq!(report.moved_subscriptions, 0);
        assert_eq!(report.moved_events, 0);
        assert_eq!(report.moved_summaries, 1);
        assert!(report.deleted_source_account);
        assert_eq!(report.remaining_references.total(), 0);

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );
        assert_eq!(
            summaries[0]
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.account_identity_source.clone()),
            Some(IdentitySource::UserConfigured)
        );
        assert!(store
            .list_accounts()
            .expect("accounts")
            .into_iter()
            .all(|account| account.provider_account_id != alias.provider_account_id));
    }

    #[test]
    fn remove_orphan_provider_account_rejects_referenced_account() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-existing-alias"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("existing-alias"), None, None, None, now);
        store.upsert_account(&alias).expect("alias account");
        let assignment = test_assignment(
            &source,
            &alias.provider_account_id,
            now - Duration::days(1),
            None,
            now,
        );
        store
            .upsert_source_account_assignment(&assignment)
            .expect("assignment");

        let error = remove_orphan_provider_account(&store, "codex", "existing-alias", false)
            .expect_err("referenced account should fail");
        assert!(error.to_string().contains("still has references"));
    }

    #[test]
    fn remove_orphan_provider_account_deletes_account_and_clears_sync_tracking() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("orphan-alias"), None, None, None, now);
        store.upsert_account(&alias).expect("alias account");
        store
            .record_accounts_synced(
                "http",
                "https://api.example.com/api/sync/batches",
                &[sanitize_account_for_sync(alias.clone())],
            )
            .expect("sync account");
        store
            .record_sync_success(
                "http",
                "https://api.example.com/api/sync/batches",
                "batch_1",
                &[],
                &[],
            )
            .expect("sync success");

        let report =
            remove_orphan_provider_account(&store, "codex", "orphan-alias", false).expect("remove");
        assert!(report.deleted);
        assert!(report.reset_local_sync_tracking);
        assert!(store.list_sync_states().expect("sync states").is_empty());
        assert!(store
            .list_accounts()
            .expect("accounts")
            .into_iter()
            .all(|account| account.provider_account_id != alias.provider_account_id));
    }

    fn test_sync_command(sink: &str) -> SyncCommand {
        SyncCommand {
            sink: sink.to_string(),
            output: None,
            endpoint: None,
            auth_token: None,
            rebuild_rollups: false,
            since_last: false,
            status: false,
            verify: false,
            reset_remote: false,
            yes: false,
            dry_run: false,
        }
    }

    #[test]
    fn usage_report_filters_period_and_groups_by_canonical_account() {
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
        );
        let account_id = provider_account_id("codex", "personal@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: Some("personal@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: None,
            confidence: Confidence::High,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let recent = test_event(
            "codex",
            &source,
            now - Duration::days(1),
            Some(account_id.clone()),
            TokenParts {
                input: 70,
                cached_input: 20,
                output: 25,
                reasoning: 5,
                total: 100,
                cost: Some(1),
            },
        );
        let old = test_event(
            "codex",
            &source,
            now - Duration::days(10),
            Some(account_id),
            TokenParts {
                input: 120,
                cached_input: 30,
                output: 50,
                reasoning: 0,
                total: 200,
                cost: Some(1),
            },
        );

        let report = build_usage_report(
            &[recent, old],
            &[],
            &[source],
            &[account],
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
        assert_eq!(report.total_usage.estimated_cost_usd, Some(1));
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
        );
        let account_id = provider_account_id("codex", "stable-provider-id");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("work".to_string()),
            plan_name: None,
            confidence: Confidence::Medium,
            verified_at: None,
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
            &[],
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
        );
        let account_id = provider_account_id("claude_code", "personal@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "claude_code".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: Some("personal@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: None,
            confidence: Confidence::High,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let event = test_event(
            "claude_code",
            &source,
            now,
            Some(account_id.clone()),
            TokenParts::total(100),
        );
        let summary = test_summary("claude_code", &source, now, 500, Some(account_id.clone()));

        let report = build_usage_report(
            &[event],
            &[summary],
            std::slice::from_ref(&source),
            std::slice::from_ref(&account),
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
            &[test_summary(
                "claude_code",
                &source,
                now,
                500,
                Some(account_id),
            )],
            std::slice::from_ref(&source),
            std::slice::from_ref(&account),
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
        );
        let account_id = provider_account_id("claude_code", "personal@example.com");
        let mut stats_cache =
            test_summary("claude_code", &source, now, 500, Some(account_id.clone()));
        stats_cache.metadata.summary_format = "claude_stats_cache".to_string();
        let mut external = test_summary("claude_code", &source, now, 300, Some(account_id));
        external.summary_id = summary_id("claude_code", &source.source_id, "external");
        external.metadata.summary_format = "external_daily".to_string();

        let report = build_usage_report(
            &[],
            &[stats_cache, external],
            std::slice::from_ref(&source),
            &[],
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
        cost: Option<i64>, // cents
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

    fn test_account(
        provider: &str,
        label: Option<&str>,
        email: Option<&str>,
        provider_user_id: Option<&str>,
        plan_name: Option<&str>,
        now: DateTime<Utc>,
    ) -> ProviderAccount {
        let provider_account_id =
            provider_account_id_from_identity(provider, provider_user_id, email)
                .unwrap_or_else(|| provider_account_id(provider, label.expect("label")));
        let normalized_email = email.map(normalize_email);
        ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id,
            provider: provider.to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: provider_user_id.map(ToOwned::to_owned),
            provider_user_id_hash: provider_user_id.map(hash_text),
            email_hash: normalized_email.as_deref().map(hash_text),
            email: normalized_email,
            org_id_hash: None,
            account_label: label.map(ToOwned::to_owned),
            plan_name: plan_name.map(ToOwned::to_owned),
            confidence: if email.is_some() || provider_user_id.is_some() {
                Confidence::High
            } else {
                Confidence::Medium
            },
            verified_at: email.map(|_| now),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_assignment(
        source: &SourceLocation,
        provider_account_id: &statsai_core::ProviderAccountId,
        started_at: DateTime<Utc>,
        ended_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> SourceAccountAssignment {
        SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: provider_account_id.clone(),
            started_at,
            ended_at,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_event(
        provider: &str,
        source: &SourceLocation,
        started_at: DateTime<Utc>,
        provider_account_id: Option<statsai_core::ProviderAccountId>,
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
        provider_account_id: Option<statsai_core::ProviderAccountId>,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(provider, &source.source_id, "summary"),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id,
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
            project: None,
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
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

    fn test_sync_ack_json(batch_id: &str) -> String {
        format!(
            r#"{{
              "schema_version":"sync_ack.v1",
              "batch_id":"{batch_id}",
              "accepted":{{"sources":0,"accounts":0,"source_account_assignments":0,"subscriptions":0,"events":0,"summaries":0}},
              "duplicates":{{"sources":0,"accounts":0,"source_account_assignments":0,"subscriptions":0,"events":0,"summaries":0}},
              "rejected":[]
            }}"#
        )
    }

    #[test]
    fn usd_amount_json_uses_major_units() {
        assert_eq!(usd_amount_json(Some(125)), json!(1.25));
        assert_eq!(usd_amount_json(None), Value::Null);
    }

    #[test]
    fn subscription_json_value_preserves_major_unit_price() {
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &provider_account_id("codex", "acct-test"),
                "Plus",
                started_at,
            ),
            provider: "codex".to_string(),
            provider_account_id: provider_account_id("codex", "acct-test"),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(started_at),
            renewal_day: Some(29),
            started_at,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            notes: None,
        };

        let value = subscription_json_value(&subscription);

        assert_eq!(value["price"], json!(20.0));
        assert_eq!(value["price_cents"], json!(2000));
    }
}
