use ai_stats_adapters::{
    adapter_for_provider, default_adapters, ProviderAdapter, ScanDiagnostics, ScanOptions,
};
use ai_stats_core::{
    build_usage_report, home_dir, provider_account_id, subscription_id, BillingPeriod, Confidence,
    CostInfo, IdentitySource, LocationOrigin, ProviderAccount, ReportPeriod, SourceKind,
    SourceLocation, Subscription, SubscriptionStatus, SyncBatch, UsageCounts, UsageEvent,
    UsageReport, UsageSummary, UsageTotals, PROVIDER_ACCOUNT_SCHEMA_VERSION,
    REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION, SUBSCRIPTION_SCHEMA_VERSION,
    SYNC_BATCH_SCHEMA_VERSION,
};
use ai_stats_sdk::{
    build_reported_usage_summary, ReportedUsageSummaryInput, ReportedUsageSummaryRecord,
};
use ai_stats_store::Store;
use ai_stats_sync::{FileSink, HttpSink, StdoutSink, SyncSink};
use anyhow::{bail, Context, Result};
#[cfg(test)]
use chrono::Duration;
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Parser)]
#[command(
    name = "ai-stats",
    version,
    about = "Local-first AI usage stats CLI/SDK/daemon."
)]
struct Cli {
    #[arg(long, global = true, help = "Path to SQLite store")]
    store: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        default_value = "local",
        help = "Device identifier for multi-device sync"
    )]
    device_id: String,
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
}

#[derive(Debug, Args)]
struct ScanCommand {
    #[arg(long, help = "Scan only this provider (claude, codex)")]
    provider: Option<String>,
    #[arg(long, help = "Preview without persisting to the store")]
    preview: bool,
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
    #[command(about = "Import ccusage text report")]
    Ccusage {
        #[arg(long, help = "Path to ccusage text report file or directory")]
        path: PathBuf,
        #[arg(long, default_value = "claude_code", help = "Provider to import as")]
        provider: String,
        #[arg(long, help = "Account label to attach")]
        account: Option<String>,
        #[arg(long, help = "Year for month/day-only summaries (e.g. 2026)")]
        year: Option<i32>,
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
    #[arg(long, help = "HTTP endpoint for the http sink")]
    endpoint: Option<String>,
    #[arg(long, help = "Bearer token for the http sink")]
    auth_token: Option<String>,
    #[arg(
        long,
        help = "Send only records after this sink target's last successful sync"
    )]
    since_last: bool,
    #[arg(long, help = "Show recorded sync state instead of sending")]
    status: bool,
    #[arg(long, help = "Preview the sync batch without writing")]
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

    match cli.command {
        Command::Schema(command) => schema(command),
        Command::Doctor => doctor(&store_path),
        command => {
            let store = Store::open(&store_path)?;
            match command {
                Command::Scan(command) => scan(command, &store, &cli.device_id),
                Command::Report(command) => report(command, &store),
                Command::Source(command) => source(command, &store),
                Command::Account(command) => account(command, &store),
                Command::Subscription(command) => subscription(command, &store),
                Command::Import(command) => import(command, &store, &cli.device_id),
                Command::Export(command) => export(command, &store),
                Command::Sync(command) => sync(command, &store, &cli.device_id),
                Command::Daemon(command) => daemon(command, store, &cli.device_id),
                Command::Status => status(&store),
                Command::Schema(_) | Command::Doctor => unreachable!("handled before store open"),
            }
        }
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
            let options = ScanOptions {
                device_id: device_id.to_string(),
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

            if !command.verbose
                && !command.explain
                && source_event_count == 0
                && source_summary_count == 0
            {
                continue;
            }

            total_sources += 1;
            total_log_rows += log_rows;
            event_count += source_event_count;
            summary_count += source_summary_count;
            total_usage.add_totals(&source_usage);
            total_summary_usage.add_totals(&source_summary_usage);
            add_diagnostics(&mut total_diagnostics, &scan.diagnostics);

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
        ImportSubcommand::Ccusage {
            path,
            provider,
            account,
            year,
            replace,
            dry_run,
            verbose,
        } => {
            let provider = canonical_provider(&provider)?;
            let reports =
                parse_ccusage_text_reports(&path, &provider, account.as_deref(), year, device_id)?;
            import_reported_summary_records(store, &reports, dry_run, verbose, replace)?;
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
    if command.status {
        return sync_status(store);
    }

    let target = sync_target(&command);
    let state = if command.since_last {
        store.sync_state(&command.sink, &target)?
    } else {
        None
    };
    let event_cursor = state.as_ref().and_then(|state| {
        state
            .last_event_started_at
            .as_ref()
            .zip(state.last_event_id.as_deref())
    });
    let summary_cursor = state.as_ref().and_then(|state| {
        state
            .last_summary_observed_at
            .as_ref()
            .zip(state.last_summary_id.as_deref())
    });
    let events: Vec<_> = store
        .events_after(event_cursor)?
        .into_iter()
        .map(sanitize_event_for_sync)
        .collect();
    let summaries: Vec<_> = store
        .summaries_after(summary_cursor)?
        .into_iter()
        .map(sanitize_summary_for_sync)
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: format!("batch_{}", Utc::now().timestamp_millis()),
        device_id: device_id.to_string(),
        sources: store
            .list_sources()?
            .into_iter()
            .map(sanitize_source_for_sync)
            .collect(),
        accounts: store
            .list_accounts()?
            .into_iter()
            .map(sanitize_account_for_sync)
            .collect(),
        subscriptions: store
            .list_subscriptions()?
            .into_iter()
            .map(sanitize_subscription_for_sync)
            .collect(),
        events,
        summaries,
        created_at: Utc::now(),
    };

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
                    .or_else(|| std::env::var("AI_STATS_SYNC_TOKEN").ok());
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

fn sync_target(command: &SyncCommand) -> String {
    match command.sink.as_str() {
        "http" => command
            .endpoint
            .clone()
            .unwrap_or_else(|| "http".to_string()),
        "file" => command
            .output
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "ai-stats-sync-batch.json".to_string()),
        other => other.to_string(),
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

fn parse_ccusage_text_reports(
    path: &Path,
    provider: &str,
    account: Option<&str>,
    default_year: Option<i32>,
    device_id: &str,
) -> Result<Vec<ReportedImportReport>> {
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("txt") {
                files.push(path);
            }
        }
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }

    let mut reports = Vec::new();
    for file in files {
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        let mut warnings = Vec::new();
        let mut inputs = parse_ccusage_daily_rows(&text, provider, account, &file);
        if inputs.is_empty() {
            inputs.extend(parse_ccusage_simple_summary(
                &text,
                provider,
                account,
                default_year,
                &file,
                &mut warnings,
            ));
        }
        if inputs.is_empty()
            && text.contains("Claude Code Token Usage Report - Session Blocks")
            && text.contains('…')
        {
            warnings.push(
                "session-block table contains truncated token values; skipped to avoid importing approximate totals"
                    .to_string(),
            );
        }
        let records = inputs
            .into_iter()
            .map(|input| build_reported_usage_summary(input, device_id))
            .collect::<Result<Vec<_>>>()?;
        reports.push(ReportedImportReport {
            path: file,
            records,
            warnings,
        });
    }
    Ok(reports)
}

fn parse_ccusage_daily_rows(
    text: &str,
    provider: &str,
    account: Option<&str>,
    evidence_path: &Path,
) -> Vec<ReportedUsageSummaryInput> {
    text.lines()
        .filter_map(|line| {
            let columns = table_columns(line);
            let date_text = columns.first()?;
            let date = NaiveDate::parse_from_str(date_text, "%Y-%m-%d").ok()?;
            if columns.len() < 8 {
                return None;
            }
            let usage = UsageCounts {
                input_tokens: parse_u64_column(&columns[2]),
                output_tokens: parse_u64_column(&columns[3]),
                cache_creation_tokens: parse_u64_column(&columns[4]),
                cache_read_tokens: parse_u64_column(&columns[5]),
                reasoning_tokens: None,
                total_tokens: parse_u64_column(&columns[6]),
                requests: None,
                local_prompt_eval_tokens: None,
                local_eval_tokens: None,
            };
            (usage.computed_total() > 0).then(|| {
                let period_start = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
                let period_end = date.and_hms_opt(23, 59, 59).unwrap().and_utc();
                reported_summary_input(ReportedSummaryParams {
                    provider: provider.to_string(),
                    account: account.map(ToOwned::to_owned),
                    format: "ccusage_daily".to_string(),
                    evidence_id: Some(format!("ccusage_daily:{}", date.format("%Y-%m-%d"))),
                    evidence_path: Some(evidence_path.to_string_lossy().to_string()),
                    period_start: Some(period_start),
                    period_end: Some(period_end),
                    usage,
                    cost_usd: parse_cost_column(&columns[7]),
                    model_name: None,
                })
            })
        })
        .collect()
}

fn parse_ccusage_simple_summary(
    text: &str,
    provider: &str,
    account: Option<&str>,
    default_year: Option<i32>,
    evidence_path: &Path,
    warnings: &mut Vec<String>,
) -> Vec<ReportedUsageSummaryInput> {
    let input = prefixed_number(text, "Input tokens:");
    let output = prefixed_number(text, "Output tokens:");
    let cache_create = prefixed_number(text, "Cache Create tokens:");
    let cache_read = prefixed_number(text, "Cache Read tokens:");
    let total = prefixed_number(text, "Total tokens:");
    let cost = prefixed_cost(text, "Total cost:");
    let usage = UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_create,
        cache_read_tokens: cache_read,
        reasoning_tokens: None,
        total_tokens: total,
        requests: None,
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    };
    if usage.computed_total() == 0 {
        return Vec::new();
    }

    let (period_start, period_end) = match (default_year, summary_month_day_range(text)) {
        (Some(year), Some((month, start_day, end_day))) => {
            let start = NaiveDate::from_ymd_opt(year, month, start_day)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc());
            let end = NaiveDate::from_ymd_opt(year, month, end_day)
                .and_then(|date| date.and_hms_opt(23, 59, 59))
                .map(|date| date.and_utc());
            (start, end)
        }
        (None, Some(_)) => {
            warnings.push(
                "summary has a month/day range but no year; pass --year to preserve the period"
                    .to_string(),
            );
            (None, None)
        }
        _ => (None, None),
    };

    vec![reported_summary_input(ReportedSummaryParams {
        provider: provider.to_string(),
        account: account.map(ToOwned::to_owned),
        format: "ccusage_summary".to_string(),
        evidence_id: Some("ccusage_summary".to_string()),
        evidence_path: Some(evidence_path.to_string_lossy().to_string()),
        period_start,
        period_end,
        usage,
        cost_usd: cost,
        model_name: None,
    })]
}

struct ReportedSummaryParams {
    provider: String,
    account: Option<String>,
    format: String,
    evidence_id: Option<String>,
    evidence_path: Option<String>,
    period_start: Option<DateTime<Utc>>,
    period_end: Option<DateTime<Utc>>,
    usage: UsageCounts,
    cost_usd: Option<f64>,
    model_name: Option<String>,
}

fn reported_summary_input(params: ReportedSummaryParams) -> ReportedUsageSummaryInput {
    let model = params.model_name.map(|name| ai_stats_core::ModelInfo {
        name: Some(name.clone()),
        normalized_name: Some(name.clone()),
        provider_model_id: Some(name),
    });
    ReportedUsageSummaryInput {
        schema_version: REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION.to_string(),
        provider: params.provider,
        account_hint: params.account,
        source_kind: SourceKind::ExternalReport,
        source_name: "ccusage".to_string(),
        evidence_id: params.evidence_id,
        evidence_path: params.evidence_path,
        report_format: params.format,
        report_version: Some("ccusage_text.v1".to_string()),
        period_start: params.period_start,
        period_end: params.period_end,
        observed_at: params.period_end,
        model,
        usage: params.usage,
        cost: Some(CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: params.cost_usd,
            pricing_source: Some("ccusage_report".to_string()),
            pricing_version: None,
            confidence: if params.cost_usd.is_some() {
                Confidence::Medium
            } else {
                Confidence::Low
            },
        }),
        confidence: Some(Confidence::Medium),
    }
}

fn table_columns(line: &str) -> Vec<String> {
    line.split('│')
        .skip(1)
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect()
}

fn prefixed_number(text: &str, prefix: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(prefix).and_then(parse_u64_column))
}

fn prefixed_cost(text: &str, prefix: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(prefix).and_then(parse_cost_column))
}

fn parse_u64_column(value: &str) -> Option<u64> {
    let text: String = value.chars().filter(|ch| ch.is_ascii_digit()).collect();
    (!text.is_empty()).then(|| text.parse().ok()).flatten()
}

fn parse_cost_column(value: &str) -> Option<f64> {
    let text: String = value
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    (!text.is_empty()).then(|| text.parse().ok()).flatten()
}

fn summary_month_day_range(text: &str) -> Option<(u32, u32, u32)> {
    let start = text.find('(')?;
    let end = text[start..].find(')')? + start;
    let value = text[start + 1..end].trim();
    let mut parts = value.split_whitespace();
    let month = month_number(parts.next()?)?;
    let range = parts.next()?;
    let (start_day, end_day) = range.split_once('-')?;
    Some((month, start_day.parse().ok()?, end_day.parse().ok()?))
}

fn month_number(value: &str) -> Option<u32> {
    match value.to_ascii_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
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
            "{} account={} path={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} files={} timestamp_fallbacks={} model_fallbacks={} origin={} source={}",
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
        "diagnostics: files={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} timestamp_fallbacks={} model_fallbacks={}",
        format_u64(diagnostics.files_scanned),
        format_u64(diagnostics.raw_rows),
        format_u64(diagnostics.candidate_usage_rows),
        format_u64(diagnostics.duplicate_events),
        format_u64(diagnostics.skipped_zero_events),
        format_u64(diagnostics.invalid_rows),
        format_u64(diagnostics.timestamp_fallbacks),
        format_u64(diagnostics.model_fallbacks)
    );
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
        .filter(|source| source.enabled && provider_matches(&source.provider, adapter.provider()))
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

fn sanitize_summary_for_sync(mut summary: UsageSummary) -> UsageSummary {
    summary.source.source_record_id = None;
    if let Some(evidence) = summary.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    summary
}

fn sanitize_subscription_for_sync(mut subscription: Subscription) -> Subscription {
    subscription.notes = None;
    subscription
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_stats_core::{
        event_id, summary_id, CostInfo, EventSource, IdentitySource, ModelInfo, ParseEvidence,
        PrivacyInfo, PrivacyMode, SessionInfo, SourceKind, SummaryMetadata, UsageCounts,
        UsageSummary, USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
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
        summary.source.source_record_id = Some("ccusage_jul11.txt:daily:2025-07-11".to_string());
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
            "ccusage",
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
        reported.metadata.summary_format = "ccusage_daily".to_string();
        let mut local = test_summary("claude_code", &local_source, now, 200);
        local.source.source_kind = SourceKind::LocalSummary;
        local.metadata.summary_format = "ccusage_daily".to_string();
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
            "ccusage-file-a",
            None,
            Some("personal".to_string()),
        );
        let other_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "ccusage-file-b",
            None,
            Some("personal".to_string()),
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");

        let mut matching = test_summary("claude_code", &source, now, 100);
        matching.source.source_kind = SourceKind::ExternalReport;
        matching.metadata.summary_format = "ccusage_daily".to_string();
        matching.period_start = Some(now - Duration::days(1));
        matching.period_end = Some(now);

        let mut same_file_different_day = test_summary("claude_code", &source, now, 200);
        same_file_different_day.summary_id =
            summary_id("claude_code", &source.source_id, "other-day");
        same_file_different_day.source.source_kind = SourceKind::ExternalReport;
        same_file_different_day.metadata.summary_format = "ccusage_daily".to_string();
        same_file_different_day.period_start = Some(now - Duration::days(2));
        same_file_different_day.period_end = Some(now - Duration::days(1));

        let mut same_period_different_file = test_summary("claude_code", &other_source, now, 300);
        same_period_different_file.source.source_kind = SourceKind::ExternalReport;
        same_period_different_file.metadata.summary_format = "ccusage_daily".to_string();
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
            path: PathBuf::from("ccusage-file-a.txt"),
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
                since_last: false,
                status: false,
                dry_run: true,
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert!(!output.exists());
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
    fn ccusage_daily_report_imports_exact_token_splits() {
        let evidence_path = Path::new("/tmp/ccusage.txt");
        let text = "│ 2025-07-11 │ claude-sonnet │ 276,719 │ 1,075,310 │ 86,144,908 │ 1,558,827,137 │ 1,646,324,074 │ $2493.54 │";

        let inputs = parse_ccusage_daily_rows(text, "claude_code", Some("personal"), evidence_path);

        assert_eq!(inputs.len(), 1);
        let record =
            build_reported_usage_summary(inputs[0].clone(), "device").expect("reported summary");
        let summary = &record.summary;
        assert_eq!(summary.metadata.summary_format, "ccusage_daily");
        assert_eq!(summary.usage.input_tokens, Some(276_719));
        assert_eq!(summary.usage.output_tokens, Some(1_075_310));
        assert_eq!(summary.usage.cache_creation_tokens, Some(86_144_908));
        assert_eq!(summary.usage.cache_read_tokens, Some(1_558_827_137));
        assert_eq!(summary.usage.total_tokens, Some(1_646_324_074));
        assert_eq!(summary.cost.provider_reported_usd, Some(2493.54));
        assert_eq!(
            summary
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.account_identity_source.clone()),
            Some(IdentitySource::ManualHint)
        );
    }

    #[test]
    fn ccusage_simple_summary_uses_year_for_period() {
        let evidence_path = Path::new("/tmp/claude-code-sep.txt");
        let text = "\
Claude Code Pro Summary (SEP 4-9)
Input tokens: 46,127
Output tokens: 213,223
Cache Create tokens: 13,338,573
Cache Read tokens: 165,270,503
Total tokens: 178,868,426
Total cost: $102.94
";
        let mut warnings = Vec::new();

        let inputs = parse_ccusage_simple_summary(
            text,
            "claude_code",
            Some("personal"),
            Some(2025),
            evidence_path,
            &mut warnings,
        );

        assert!(warnings.is_empty());
        assert_eq!(inputs.len(), 1);
        let record =
            build_reported_usage_summary(inputs[0].clone(), "device").expect("reported summary");
        let summary = &record.summary;
        assert_eq!(summary.metadata.summary_format, "ccusage_summary");
        assert_eq!(summary.usage.input_tokens, Some(46_127));
        assert_eq!(summary.usage.output_tokens, Some(213_223));
        assert_eq!(summary.usage.cache_creation_tokens, Some(13_338_573));
        assert_eq!(summary.usage.cache_read_tokens, Some(165_270_503));
        assert_eq!(summary.usage.total_tokens, Some(178_868_426));
        assert_eq!(summary.cost.provider_reported_usd, Some(102.94));
        assert_eq!(
            summary.period_start,
            Some(
                Utc.with_ymd_and_hms(2025, 9, 4, 0, 0, 0)
                    .single()
                    .expect("start")
            )
        );
        assert_eq!(
            summary.period_end,
            Some(
                Utc.with_ymd_and_hms(2025, 9, 9, 23, 59, 59)
                    .single()
                    .expect("end")
            )
        );
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
        let mut ccusage = test_summary("claude_code", &source, now, 300);
        ccusage.summary_id = summary_id("claude_code", &source.source_id, "ccusage");
        ccusage.metadata.summary_format = "ccusage_daily".to_string();

        let report = build_usage_report(
            &[],
            &[stats_cache, ccusage],
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
            .any(|row| row.kind == "ccusage_daily" && row.usage.total_tokens == 300));
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
