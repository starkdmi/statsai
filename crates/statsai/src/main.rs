use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
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
    project_contains_file_paths, project_has_stable_identity, source_account_assignment_id,
    source_id as statsai_source_id, subscription_id, timestamp_in_period, ArchiveContentKind,
    ArchiveConversation, BillingPeriod, EventId, IdentitySource, LocationOrigin, ProjectInfo,
    ProviderAccount, ProviderAccountId, ReportPeriod, SourceAccountAssignment,
    SourceAccountAssignmentId, SourceId, SourceKind, SourceLocation, SourceVerificationMode,
    Subscription, SubscriptionId, SubscriptionStatus, SyncAuthoritativeSnapshot, SyncBatch,
    TaskBucketSnapshot, TaskSpan, TaskStatus, TaskVerdict, TaskVerification,
    TaskVerificationAction, TaskVerificationCursor, UsageEvent, UsageReport, UsageSummary,
    UsageTotals, WorkItem, WorkItemId, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
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
    REPORTED_USAGE_IMPORT_ADAPTER_ID,
};
#[cfg(test)]
use statsai_store::apply_verified_source_state;
use statsai_store::{
    close_active_verified_source_linkages, derive_task_work_items, find_existing_provider_account,
    reconcile_verified_source_state, upsert_provider_account, verified_source_state_hash,
    ScanFileStateEntry, Store, SyncPreferences, SyncState, TaskRebuildReport,
    UpsertProviderAccountInput,
};
use statsai_sync::{
    validate_authenticated_http_endpoint, FileSink, HttpSink, StdoutSink, SyncSink,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use statsai::{auth, default_device_id, default_store_path, service, snapshot};

const HTTP_ROLLUP_SUMMARIES_PER_BATCH: usize = 25;
const HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH: usize = 20;
const HTTP_ROLLUP_D1_QUERY_BUDGET: usize = 45;
const HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE: usize = 90;
const HTTP_ROLLUP_DAILY_ROLLUP_ROWS_PER_QUERY: usize = 7;
const HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH: usize = 200;
const HTTP_REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(30);
const TASK_SYNC_SQL_MAX_ROWS_PER_CHUNK: usize = 200;
const TASK_SYNC_SQL_MAX_JSON_BYTES_PER_CHUNK: usize = 512 * 1024;
const MAX_SUBSCRIPTION_PRICE_CENTS: i64 = 100_000_000;

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
    #[command(about = "Review and rebuild local work items")]
    Task(TaskCommand),
    #[command(about = "Collect and explore durable local conversation archives")]
    Conversation(ConversationCommand),
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
    #[command(about = "Install or manage the background daemon service")]
    Service(ServiceCommand),
    #[command(about = "Show link, sync, and background collection status")]
    Snapshot(snapshot::SnapshotCommand),
}

#[derive(Debug, Args)]
struct ServiceCommand {
    #[command(subcommand)]
    command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
enum ServiceSubcommand {
    #[command(about = "Install a LaunchAgent that runs statsai daemon --watch")]
    Install,
    #[command(about = "Remove the background daemon LaunchAgent")]
    Uninstall,
    #[command(about = "Show LaunchAgent install and run state")]
    Status,
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
    #[arg(long, help = "Scan only this provider")]
    provider: Option<String>,
    #[arg(long, help = "Collect local task spans and rebuild work items")]
    include_tasks: bool,
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

#[derive(Debug, Args)]
struct TaskCommand {
    #[command(subcommand)]
    command: TaskSubcommand,
}

#[derive(Debug, Args)]
struct ConversationCommand {
    #[command(subcommand)]
    command: ConversationSubcommand,
}

#[derive(Debug, Subcommand)]
enum ConversationSubcommand {
    #[command(about = "Collect new or changed conversations from local provider sources")]
    Collect {
        #[arg(long, help = "Collect only this provider")]
        provider: Option<String>,
        #[arg(long, help = "Ignore the archive collection cache")]
        no_cache: bool,
        #[arg(long, help = "Show per-source collection diagnostics")]
        verbose: bool,
    },
    #[command(about = "List archived conversations")]
    List {
        #[arg(long, help = "Optional provider filter")]
        provider: Option<String>,
        #[arg(long, default_value_t = 50, help = "Maximum conversations to return")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Read one archived conversation")]
    Show {
        #[arg(help = "Canonical conversation identifier")]
        conversation_id: String,
        #[arg(long, help = "Output complete JSON, including base64 artifacts")]
        json: bool,
    },
    #[command(about = "Search archived conversation text using SQLite FTS5")]
    Search {
        #[arg(help = "FTS5 search expression")]
        query: String,
        #[arg(long, default_value_t = 50, help = "Maximum matches to return")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Show local archive coverage and storage statistics")]
    Stats {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Export one conversation with complete artifact payloads")]
    Export {
        #[arg(help = "Canonical conversation identifier")]
        conversation_id: String,
        #[arg(long, default_value = "json", help = "Export format: json or markdown")]
        format: String,
    },
}

#[derive(Debug, Subcommand)]
enum TaskSubcommand {
    #[command(about = "List derived work items")]
    List {
        #[arg(long, help = "Optional provider filter")]
        provider: Option<String>,
        #[arg(long, help = "Optional status filter")]
        status: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Show one derived work item")]
    Show {
        #[arg(help = "Work item identifier")]
        work_item_id: String,
        #[arg(long, help = "Include member spans and evidence")]
        include_evidence: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Record manual verification constraints")]
    Verify {
        #[command(subcommand)]
        command: TaskVerifySubcommand,
    },
    #[command(about = "Show local task collection stats")]
    Stats {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Benchmark the current grouper against simple baselines")]
    Benchmark {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Export local task spans or work items")]
    Export {
        #[arg(long, default_value = "work-item", help = "Export level")]
        level: String,
        #[arg(long, default_value = "json", help = "Export format")]
        format: String,
    },
    #[command(about = "Rebuild derived work items from stored task spans")]
    Rebuild {
        #[arg(long, help = "Optional provider filter")]
        provider: Option<String>,
        #[arg(long, help = "Optional source identifier filter")]
        source_id: Option<String>,
        #[arg(long, help = "Rebuild every project bucket")]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TaskVerifySubcommand {
    #[command(about = "Accept the current grouping for a work item")]
    Accept {
        #[arg(help = "Work item identifier")]
        work_item_id: String,
    },
    #[command(about = "Reject a work item as meta/system/noise")]
    Reject {
        #[arg(help = "Work item identifier")]
        work_item_id: String,
        #[arg(long, help = "Reject reason: meta, system, or noise")]
        reason: String,
    },
    #[command(about = "Split a work item after a specific span")]
    Split {
        #[arg(help = "Work item identifier")]
        work_item_id: String,
        #[arg(long, help = "Split boundary after this span")]
        after_span: String,
        #[arg(long, help = "Optional title for the left work item")]
        left_title: Option<String>,
        #[arg(long, help = "Optional title for the right work item")]
        right_title: Option<String>,
    },
    #[command(about = "Merge two work items")]
    Merge {
        #[arg(help = "Left work item identifier")]
        left_work_item_id: String,
        #[arg(help = "Right work item identifier")]
        right_work_item_id: String,
        #[arg(long, help = "Optional merged title override")]
        title: Option<String>,
    },
    #[command(about = "Rename a work item")]
    Rename {
        #[arg(help = "Work item identifier")]
        work_item_id: String,
        #[arg(long, help = "Canonical title override")]
        title: String,
    },
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
        #[arg(long, help = "Provider name")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubscriptionPrice(i64);

impl SubscriptionPrice {
    const fn cents(self) -> i64 {
        self.0
    }
}

impl FromStr for SubscriptionPrice {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        let (whole, fractional) = match value.split_once('.') {
            Some((whole, fractional)) => (whole, Some(fractional)),
            None => (value, None),
        };
        if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("price must be a non-negative decimal amount".to_string());
        }
        if fractional.is_some_and(|fractional| {
            fractional.is_empty()
                || fractional.len() > 2
                || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err("price must use at most two decimal places".to_string());
        }

        let whole = whole
            .parse::<u64>()
            .map_err(|_| "price is too large".to_string())?;
        let fractional_cents = match fractional {
            None => 0,
            Some(fractional) if fractional.len() == 1 => {
                fractional
                    .parse::<u64>()
                    .map_err(|_| "price is invalid".to_string())?
                    * 10
            }
            Some(fractional) => fractional
                .parse::<u64>()
                .map_err(|_| "price is invalid".to_string())?,
        };
        let cents = whole
            .checked_mul(100)
            .and_then(|cents| cents.checked_add(fractional_cents))
            .ok_or_else(|| "price is too large".to_string())?;
        if cents > MAX_SUBSCRIPTION_PRICE_CENTS as u64 {
            return Err("price must not exceed 1000000.00".to_string());
        }
        Ok(Self(cents as i64))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrencyCode(String);

impl CurrencyCode {
    fn into_string(self) -> String {
        self.0
    }
}

impl FromStr for CurrencyCode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err("currency must be a three-letter code such as USD".to_string());
        }
        Ok(Self(value.to_ascii_uppercase()))
    }
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
        #[arg(
            long,
            help = "Non-negative decimal subscription price (maximum 1000000.00)"
        )]
        price: SubscriptionPrice,
        #[arg(long, default_value = "USD", help = "Three-letter currency code")]
        currency: CurrencyCode,
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
        #[arg(
            long,
            help = "Non-negative decimal subscription price (maximum 1000000.00)"
        )]
        price: SubscriptionPrice,
        #[arg(long, default_value = "USD", help = "Three-letter currency code")]
        currency: CurrencyCode,
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
        help = "Force a full HTTP rollup sync even when this target was synced before"
    )]
    full: bool,
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
        help = "Delete mirrored hosted sync data for this paired device and clear local sync tracking (http only)"
    )]
    reset_remote: bool,
    #[arg(long, help = "Confirm destructive sync reset actions")]
    yes: bool,
    #[arg(long, help = "Build the sync batch without writing")]
    dry_run: bool,
    #[arg(
        long,
        help = "Enable project metadata sync for this device and future syncs"
    )]
    include_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["include_projects", "include_tasks"],
        help = "Disable project metadata sync for this device and future syncs"
    )]
    exclude_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["exclude_tasks", "exclude_projects"],
        help = "Enable hosted task sync for this device and future syncs (implies --include-projects)"
    )]
    include_tasks: bool,
    #[arg(
        long,
        conflicts_with = "include_tasks",
        help = "Disable hosted task sync for this device and future syncs"
    )]
    exclude_tasks: bool,
}

#[derive(Debug, Args)]
struct SchemaCommand {
    #[command(subcommand)]
    command: SchemaSubcommand,
}

#[derive(Debug, Subcommand)]
enum SchemaSubcommand {
    #[command(about = "Print the sync_batch.v2 JSON Schema")]
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
        Command::Service(command) => service(command),
        Command::Snapshot(command) => snapshot::run(command, &store_path),
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
                Command::Task(command) => task(command, &store),
                Command::Conversation(command) => conversation(command, &store),
                Command::Sync(command) => sync(command, &store, &device_id),
                Command::Daemon(command) => daemon(command, store, &device_id),
                Command::Status => status(&store),
                Command::Schema(_)
                | Command::Doctor
                | Command::Auth(_)
                | Command::Service(_)
                | Command::Snapshot(_) => {
                    unreachable!("handled before store open")
                }
            }
        }
    }
}

fn service(command: ServiceCommand) -> Result<()> {
    use service::ServiceAction;

    match command.command {
        ServiceSubcommand::Install => service::service(ServiceAction::Install),
        ServiceSubcommand::Uninstall => service::service(ServiceAction::Uninstall),
        ServiceSubcommand::Status => service::service(ServiceAction::Status),
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
    let scan_started_at = Instant::now();
    let mut preview_task_rebuild = PreviewTaskRebuild::default();
    let mut preview_work_item_rebuild_count = 0u64;
    let mut event_count = 0u64;
    let mut summary_count = 0u64;
    let mut task_span_count = 0u64;
    let mut inserted_count = 0u64;
    let mut summary_written_count = 0u64;
    let mut task_span_written_count = 0u64;
    let mut removed_event_count = 0u64;
    let mut removed_summary_count = 0u64;
    let mut removed_task_span_count = 0u64;
    let mut rebuilt_work_item_count = 0u64;
    let mut total_sources = 0u64;
    let mut total_log_rows = 0u64;
    let mut total_diagnostics = ScanDiagnostics::default();
    let mut total_usage = UsageTotals::default();
    let mut total_summary_usage = UsageTotals::default();
    let mut adapter_scan_duration_ms = 0u64;
    let mut preview_rebuild_duration_ms = 0u64;
    let mut delete_duration_ms = 0u64;
    let mut insert_events_duration_ms = 0u64;
    let mut upsert_summaries_duration_ms = 0u64;
    let mut upsert_task_spans_duration_ms = 0u64;
    let mut rebuild_work_items_duration_ms = 0u64;
    let mut rebuild_work_item_report = TaskRebuildReport::default();

    let configured_sources = store.list_sources()?;

    for adapter in adapters {
        let sources = scan_sources_for_adapter(adapter.as_ref(), &configured_sources);

        for mut source in sources {
            if source.path_label.is_none() {
                source.path_label = path_label_from_hashless_source(&source);
            }
            let cache_candidates = adapter.scan_candidates(&source)?;
            let compatible_scan_signatures =
                scan_candidate_compatible_signatures(&cache_candidates);
            let file_cache_entries = scan_file_state_entries(&cache_candidates);
            let file_reconciliation = select_scan_file_reconciliation(
                store,
                &source.source_id,
                &file_cache_entries,
                &compatible_scan_signatures,
                command.replace,
                command.no_cache,
                command.include_tasks,
            )?;
            let pending_file_entries = file_reconciliation.pending_entries;
            let compatible_entries_to_upgrade = file_reconciliation.compatible_entries_to_upgrade;
            let removed_file_entries = file_reconciliation.removed_entries;
            let touched_files =
                !pending_file_entries.is_empty() || !removed_file_entries.is_empty();
            let has_cache_entry_upgrades = !compatible_entries_to_upgrade.is_empty();
            let scan_all_current_files = !file_cache_entries.is_empty()
                && pending_file_entries.len() == file_cache_entries.len();
            let needs_legacy_full_reconcile = !command.replace
                && !command.no_cache
                && touched_files
                && !scan_all_current_files
                && store.source_records_missing_scan_file_hashes(&source.source_id)?;
            let replace_source_records = should_replace_source_records_for_scan(
                command.replace,
                command.no_cache,
                file_cache_entries.len(),
                pending_file_entries.len(),
                needs_legacy_full_reconcile,
            );
            let should_run_adapter_scan = if replace_source_records {
                !file_cache_entries.is_empty()
            } else {
                !pending_file_entries.is_empty()
            };
            let options = ScanOptions {
                device_id: device_id.to_string(),
                collect_tasks: command.include_tasks,
                selected_cache_keys: (should_run_adapter_scan
                    && !replace_source_records
                    && !command.no_cache)
                    .then(|| {
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
            let mut scan = if should_run_adapter_scan {
                let started_at = Instant::now();
                let scan = adapter.scan(&source, &options)?;
                adapter_scan_duration_ms += started_at.elapsed().as_millis() as u64;
                scan
            } else {
                statsai_adapters::AdapterScan {
                    diagnostics: ScanDiagnostics {
                        files_skipped_unchanged: (file_cache_entries
                            .len()
                            .saturating_sub(pending_file_entries.len()))
                            as u64,
                        ..ScanDiagnostics::default()
                    },
                    ..statsai_adapters::AdapterScan::default()
                }
            };
            if !command.include_tasks {
                scan.task_spans.clear();
            }
            let effective_verified_source_state =
                if matches!(verification_mode, SourceVerificationMode::Disabled) {
                    None
                } else if should_run_adapter_scan {
                    scan.verified_source_state
                        .take()
                        .or(probed_verified_source_state)
                } else {
                    probed_verified_source_state
                };
            // `None` means the local snapshot yielded no observation. It is not an
            // explicit sign-out or revocation signal, so preserve the last state.
            let next_verified_state_hash =
                if matches!(verification_mode, SourceVerificationMode::Auto) {
                    match effective_verified_source_state.as_ref() {
                        Some(verified_state) => verified_source_state_hash(Some(verified_state))?,
                        None => source.verified_state_hash.clone(),
                    }
                } else {
                    None
                };
            let verified_state_changed = matches!(verification_mode, SourceVerificationMode::Auto)
                && source.verified_state_hash != next_verified_state_hash;
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
            let source_task_span_count = scan.task_spans.len() as u64;
            let has_scan_activity = touched_files
                || (has_cache_entry_upgrades && !command.preview)
                || source_event_count > 0
                || source_summary_count > 0
                || source_task_span_count > 0
                || scan.diagnostics.files_scanned > 0
                || scan.diagnostics.files_skipped_unchanged > 0
                || log_rows > 0
                || verified_state_changed;
            let suppress_source_processing = !command.verbose
                && !command.explain
                && source_event_count == 0
                && source_summary_count == 0
                && source_task_span_count == 0
                && !touched_files
                && !has_cache_entry_upgrades
                && !verified_state_changed;

            if !has_scan_activity {
                continue;
            }

            total_sources += 1;
            total_log_rows += log_rows;
            event_count += source_event_count;
            summary_count += source_summary_count;
            task_span_count += source_task_span_count;
            total_usage.add_totals(&source_usage);
            total_summary_usage.add_totals(&source_summary_usage);
            add_diagnostics(&mut total_diagnostics, &scan.diagnostics);

            if suppress_source_processing {
                continue;
            }

            if command.preview {
                if command.include_tasks {
                    let rebuild_started_at = Instant::now();
                    preview_work_item_rebuild_count += preview_task_rebuild.apply_source_changes(
                        store,
                        SourceTaskChangeSet {
                            source_id: &source.source_id,
                            replace_source_records,
                            touched_files,
                            pending_file_entries: &pending_file_entries,
                            removed_file_entries: &removed_file_entries,
                            task_spans: &scan.task_spans,
                        },
                    )?;
                    preview_rebuild_duration_ms += rebuild_started_at.elapsed().as_millis() as u64;
                }
                print_scan_preview_line(ScanPreviewLine {
                    source: &source,
                    usage_events: source_event_count,
                    usage: &source_usage,
                    summaries: source_summary_count,
                    task_spans: source_task_span_count,
                    summary_usage: &source_summary_usage,
                    diagnostics: &scan.diagnostics,
                    verbose: command.verbose || command.explain,
                });
                continue;
            }
            let source_rebuild_report = store.apply_scan_update(|store| {
                reconcile_verified_source_state(
                    store,
                    &mut source,
                    effective_verified_source_state.as_ref(),
                    next_verified_state_hash,
                )?;
                persist_source_after_preview(store, &source)?;
                apply_source_account_resolution(
                    store,
                    &source,
                    &mut scan.events,
                    &mut scan.summaries,
                )?;
                let mut affected_project_buckets = if command.include_tasks {
                    scan.task_spans
                        .iter()
                        .map(|span| span.project_bucket.clone())
                        .collect::<BTreeSet<_>>()
                } else {
                    BTreeSet::new()
                };
                let mut deleted_task_spans = Vec::new();
                if replace_source_records {
                    let delete_started_at = Instant::now();
                    removed_event_count +=
                        store.delete_events_for_sources(std::slice::from_ref(&source.source_id))?;
                    removed_summary_count += store
                        .delete_summaries_for_sources(std::slice::from_ref(&source.source_id))?;
                    if command.include_tasks {
                        let deleted = store.delete_task_spans_for_sources(std::slice::from_ref(
                            &source.source_id,
                        ))?;
                        removed_task_span_count += deleted.deleted;
                        affected_project_buckets
                            .extend(deleted.affected_project_buckets.iter().cloned());
                        deleted_task_spans.extend(deleted.deleted_spans);
                    }
                    delete_duration_ms += delete_started_at.elapsed().as_millis() as u64;
                } else if touched_files {
                    let delete_started_at = Instant::now();
                    let reconciled_file_hashes = scan_file_hashes_for_reconciliation(
                        &pending_file_entries,
                        &removed_file_entries,
                    );
                    removed_event_count += store.delete_events_for_source_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                    removed_summary_count += store.delete_summaries_for_source_file_hashes(
                        &source.source_id,
                        &reconciled_file_hashes,
                    )?;
                    if command.include_tasks {
                        let deleted = store.delete_task_spans_for_source_file_hashes(
                            &source.source_id,
                            &reconciled_file_hashes,
                        )?;
                        removed_task_span_count += deleted.deleted;
                        affected_project_buckets
                            .extend(deleted.affected_project_buckets.iter().cloned());
                        deleted_task_spans.extend(deleted.deleted_spans);
                    }
                    delete_duration_ms += delete_started_at.elapsed().as_millis() as u64;
                }
                let insert_started_at = Instant::now();
                let insert_result = store.insert_events_with_resolution(&scan.events)?;
                inserted_count += insert_result.inserted;
                insert_events_duration_ms += insert_started_at.elapsed().as_millis() as u64;
                if command.include_tasks {
                    rewrite_task_span_linked_event_ids(
                        &mut scan.task_spans,
                        &insert_result.canonical_event_ids,
                    );
                    populate_task_span_rollups(
                        &mut scan.task_spans,
                        &scan.events,
                        &insert_result.canonical_event_ids,
                    );
                }
                let upsert_summaries_started_at = Instant::now();
                summary_written_count += store.upsert_summaries(&scan.summaries)?;
                upsert_summaries_duration_ms +=
                    upsert_summaries_started_at.elapsed().as_millis() as u64;

                let mut rebuild_project_buckets = BTreeSet::new();
                let mut rebuild_span_ids = BTreeSet::new();
                if command.include_tasks {
                    let upsert_task_spans_started_at = Instant::now();
                    task_span_written_count += store.upsert_task_spans(&scan.task_spans)?;
                    upsert_task_spans_duration_ms +=
                        upsert_task_spans_started_at.elapsed().as_millis() as u64;
                    rebuild_project_buckets.extend(
                        scan.task_spans
                            .iter()
                            .map(|span| span.project_bucket.clone()),
                    );
                    rebuild_span_ids
                        .extend(scan.task_spans.iter().map(|span| span.span_id.0.clone()));
                    rebuild_project_buckets.extend(affected_project_buckets);
                }

                let cache_entries_to_record = if replace_source_records || command.no_cache {
                    &file_cache_entries
                } else {
                    &pending_file_entries
                };
                store.record_scan_file_entries_with_tasks_collected(
                    &source.source_id,
                    cache_entries_to_record,
                    command.include_tasks,
                )?;
                store
                    .upgrade_scan_file_entries(&source.source_id, &compatible_entries_to_upgrade)?;
                let removed_cache_keys = scan_file_cache_keys(&removed_file_entries);
                store.delete_scan_file_entries(&source.source_id, &removed_cache_keys)?;

                if command.include_tasks
                    && !rebuild_project_buckets.is_empty()
                    && (!rebuild_span_ids.is_empty() || !deleted_task_spans.is_empty())
                {
                    let rebuild_started_at = Instant::now();
                    let report = store.rebuild_task_work_items_for_changes_report(
                        &rebuild_project_buckets,
                        &rebuild_span_ids,
                        &deleted_task_spans,
                    )?;
                    rebuild_work_items_duration_ms +=
                        rebuild_started_at.elapsed().as_millis() as u64;
                    Ok(report)
                } else {
                    Ok(TaskRebuildReport::default())
                }
            })?;
            rebuilt_work_item_count += source_rebuild_report.work_items_rebuilt;
            add_task_rebuild_report(&mut rebuild_work_item_report, &source_rebuild_report);
        }
    }

    if command.preview {
        if command.verbose {
            println!(
                "preview total: sources={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} log_rows={} written=0",
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
                format_cost(total_summary_usage.estimated_cost_usd),
                format_u64(total_log_rows)
            );
            println!(
                "preview tasks: spans={} work_items_rebuilt={}",
                format_u64(task_span_count),
                format_u64(preview_work_item_rebuild_count)
            );
            println!(
                "timings_ms: adapter_scan={} preview_rebuild={} total_wall={}",
                format_u64(adapter_scan_duration_ms),
                format_u64(preview_rebuild_duration_ms),
                format_u64(scan_started_at.elapsed().as_millis() as u64)
            );
            print_scan_diagnostics_total(&total_diagnostics);
        } else {
            println!(
                "preview total: sources={} usage_events={} summaries={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} written=0",
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
                format_cost(total_summary_usage.estimated_cost_usd)
            );
            println!(
                "preview tasks: spans={} work_items_rebuilt={}",
                format_u64(task_span_count),
                format_u64(preview_work_item_rebuild_count)
            );
        }
    } else {
        println!(
            "scan complete: sources={} usage_events={} inserted={} summaries={} summaries_written={} task_spans={} task_spans_written={} work_items_rebuilt={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} log_rows={}",
            format_u64(total_sources),
            format_u64(event_count),
            format_u64(inserted_count),
            format_u64(summary_count),
            format_u64(summary_written_count),
            format_u64(task_span_count),
            format_u64(task_span_written_count),
            format_u64(rebuilt_work_item_count),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.cache_creation_tokens),
            format_u64(total_usage.cached_input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            format_cost(total_usage.estimated_cost_usd),
            format_u64(total_summary_usage.total_tokens),
            format_cost(total_summary_usage.estimated_cost_usd),
            format_u64(total_log_rows)
        );
        if command.replace
            || removed_event_count > 0
            || removed_summary_count > 0
            || removed_task_span_count > 0
        {
            println!(
                "scan removed stale records: events={} summaries={} task_spans={}",
                format_u64(removed_event_count),
                format_u64(removed_summary_count),
                format_u64(removed_task_span_count)
            );
        }
        if command.verbose {
            println!(
                "timings_ms: adapter_scan={} delete={} insert_events={} upsert_summaries={} upsert_task_spans={} rebuild_work_items={} rebuild_delete={} rebuild_span_load={} rebuild_verifications={} rebuild_grouping={} rebuild_title_selection={} rebuild_insert={} total_wall={}",
                format_u64(adapter_scan_duration_ms),
                format_u64(delete_duration_ms),
                format_u64(insert_events_duration_ms),
                format_u64(upsert_summaries_duration_ms),
                format_u64(upsert_task_spans_duration_ms),
                format_u64(rebuild_work_items_duration_ms),
                format_u64(rebuild_work_item_report.timings.delete_ms),
                format_u64(rebuild_work_item_report.timings.span_load_ms),
                format_u64(rebuild_work_item_report.timings.verification_load_ms),
                format_u64(rebuild_work_item_report.timings.grouping_ms),
                format_u64(rebuild_work_item_report.timings.title_selection_ms),
                format_u64(rebuild_work_item_report.timings.insert_ms),
                format_u64(scan_started_at.elapsed().as_millis() as u64)
            );
        }
        print_scan_diagnostics_total(&total_diagnostics);
    }
    Ok(())
}

fn add_task_rebuild_report(total: &mut TaskRebuildReport, report: &TaskRebuildReport) {
    total.work_items_rebuilt = total
        .work_items_rebuilt
        .saturating_add(report.work_items_rebuilt);
    total.work_items_deleted = total
        .work_items_deleted
        .saturating_add(report.work_items_deleted);
    total.affected_bucket_count = total
        .affected_bucket_count
        .saturating_add(report.affected_bucket_count);
    total.affected_segment_count = total
        .affected_segment_count
        .saturating_add(report.affected_segment_count);
    total.touched_span_count = total
        .touched_span_count
        .saturating_add(report.touched_span_count);
    total.timings.delete_ms = total
        .timings
        .delete_ms
        .saturating_add(report.timings.delete_ms);
    total.timings.span_load_ms = total
        .timings
        .span_load_ms
        .saturating_add(report.timings.span_load_ms);
    total.timings.verification_load_ms = total
        .timings
        .verification_load_ms
        .saturating_add(report.timings.verification_load_ms);
    total.timings.grouping_ms = total
        .timings
        .grouping_ms
        .saturating_add(report.timings.grouping_ms);
    total.timings.title_selection_ms = total
        .timings
        .title_selection_ms
        .saturating_add(report.timings.title_selection_ms);
    total.timings.insert_ms = total
        .timings
        .insert_ms
        .saturating_add(report.timings.insert_ms);
}

#[derive(Debug, Default)]
struct PreviewTaskRebuild {
    projected_spans: Option<HashMap<String, TaskSpan>>,
    verifications: Option<Vec<TaskVerification>>,
}

struct SourceTaskChangeSet<'a> {
    source_id: &'a SourceId,
    replace_source_records: bool,
    touched_files: bool,
    pending_file_entries: &'a [ScanFileStateEntry],
    removed_file_entries: &'a [ScanFileStateEntry],
    task_spans: &'a [TaskSpan],
}

impl PreviewTaskRebuild {
    fn apply_source_changes(
        &mut self,
        store: &Store,
        changes: SourceTaskChangeSet<'_>,
    ) -> Result<u64> {
        if self.projected_spans.is_none() {
            self.projected_spans = Some(
                store
                    .task_spans()?
                    .into_iter()
                    .map(|span| (span.span_id.0.clone(), span))
                    .collect(),
            );
        }
        if self.verifications.is_none() {
            self.verifications = Some(store.task_verifications()?);
        }

        let projected_spans = self
            .projected_spans
            .as_mut()
            .expect("projected spans initialized");
        let verifications = self
            .verifications
            .as_ref()
            .expect("task verifications initialized");
        let mut affected_project_buckets = BTreeSet::new();
        if changes.replace_source_records {
            let removed_span_ids = projected_spans
                .iter()
                .filter(|(_, span)| span.source_id == *changes.source_id)
                .map(|(span_id, span)| {
                    affected_project_buckets.insert(span.project_bucket.clone());
                    span_id.clone()
                })
                .collect::<Vec<_>>();
            for span_id in removed_span_ids {
                projected_spans.remove(span_id.as_str());
            }
        } else if changes.touched_files {
            let reconciled_hashes = scan_file_hashes_for_reconciliation(
                changes.pending_file_entries,
                changes.removed_file_entries,
            )
            .into_iter()
            .collect::<HashSet<_>>();
            if !reconciled_hashes.is_empty() {
                let removed_span_ids = projected_spans
                    .iter()
                    .filter(|(_, span)| span.source_id == *changes.source_id)
                    .filter(|(_, span)| {
                        span.source_file_path_hash
                            .as_deref()
                            .is_some_and(|hash| reconciled_hashes.contains(hash))
                    })
                    .map(|(span_id, span)| {
                        affected_project_buckets.insert(span.project_bucket.clone());
                        span_id.clone()
                    })
                    .collect::<Vec<_>>();
                for span_id in removed_span_ids {
                    projected_spans.remove(span_id.as_str());
                }
            }
        }
        for span in changes.task_spans {
            if let Some(previous) = projected_spans.insert(span.span_id.0.clone(), span.clone()) {
                affected_project_buckets.insert(previous.project_bucket);
            }
            affected_project_buckets.insert(span.project_bucket.clone());
        }
        if affected_project_buckets.is_empty() {
            return Ok(0);
        }

        let preview_spans = projected_spans
            .values()
            .filter(|span| affected_project_buckets.contains(&span.project_bucket))
            .cloned()
            .collect::<Vec<_>>();
        let (work_items, _) = derive_task_work_items(preview_spans, verifications);
        Ok(work_items.len() as u64)
    }
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
            let deleted_task_spans: statsai_store::TaskDeletionImpact = if delete_data {
                store.delete_task_spans_for_sources(std::slice::from_ref(&source_id))?
            } else {
                Default::default()
            };
            let rebuilt_work_items =
                if delete_data && !deleted_task_spans.affected_project_buckets.is_empty() {
                    store.rebuild_task_work_items_for_project_buckets(
                        &deleted_task_spans.affected_project_buckets,
                    )?
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
                    "deleted_task_spans": deleted_task_spans.deleted,
                    "work_items_rebuilt": rebuilt_work_items,
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

fn task(command: TaskCommand, store: &Store) -> Result<()> {
    match command.command {
        TaskSubcommand::List {
            provider,
            status,
            json,
        } => {
            let status_filter = status
                .as_deref()
                .map(parse_task_status_filter)
                .transpose()?;
            let selection =
                task_list_selection(store, provider.as_deref(), status_filter.as_ref())?;
            let items = selection.items;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                if items.is_empty() {
                    if status_filter.is_none() && selection.hidden_rejected_meta > 0 {
                        println!(
                            "no visible work items found; {} rejected meta items are hidden by default. Use `statsai task list --status rejected_meta` to inspect them.",
                            format_u64(selection.hidden_rejected_meta as u64)
                        );
                    } else {
                        println!(
                            "no work items found; run `statsai scan` to collect task spans, then `statsai task list` again"
                        );
                    }
                    return Ok(());
                }
                for item in items {
                    println!("{}", format_task_list_item(&item));
                }
                if status_filter.is_none() && selection.hidden_rejected_meta > 0 {
                    println!(
                        "hidden_rejected_meta={} use `statsai task list --status rejected_meta` to inspect",
                        format_u64(selection.hidden_rejected_meta as u64)
                    );
                }
            }
        }
        TaskSubcommand::Show {
            work_item_id,
            include_evidence,
            json,
        } => {
            let work_item_id = WorkItemId(work_item_id);
            let output = load_task_show_output(store, &work_item_id, include_evidence)?;
            if json {
                if include_evidence {
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&output.work_item)?);
                }
            } else {
                print_work_item(&output.work_item);
                if include_evidence {
                    for verification in &output.verifications {
                        println!(
                            "  verification={} updated_at={}",
                            format_task_verification(verification),
                            verification.updated_at.to_rfc3339()
                        );
                    }
                    for span in output.spans {
                        println!(
                            "  span={} provider={} start={} end={} tokens={} title={}",
                            span.span_id.0,
                            span.provider,
                            span.started_at.to_rfc3339(),
                            span.ended_at
                                .map(|value| value.to_rfc3339())
                                .unwrap_or_else(|| "open".to_string()),
                            format_u64(span.usage.computed_total()),
                            span.title
                        );
                        let repo_label = span
                            .project
                            .as_ref()
                            .and_then(|project| project.repo_label.as_deref())
                            .unwrap_or("-");
                        let branch_label = span
                            .project
                            .as_ref()
                            .and_then(|project| project.branch_label.as_deref())
                            .unwrap_or("-");
                        let session_id = span.session_id.as_deref().unwrap_or("-");
                        let thread_id = span.thread_id.as_deref().unwrap_or("-");
                        println!(
                            "    repo={} branch={} session={} thread={} issues={}",
                            repo_label,
                            branch_label,
                            session_id,
                            thread_id,
                            if span.issue_keys.is_empty() {
                                "-".to_string()
                            } else {
                                span.issue_keys.join(",")
                            }
                        );
                        if let Some(summary_preview) = span.summary_preview.as_deref() {
                            println!("    summary_preview={summary_preview}");
                        }
                    }
                }
            }
        }
        TaskSubcommand::Verify { command } => {
            let (verification, buckets) = match command {
                TaskVerifySubcommand::Accept { work_item_id } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Accept {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Reject {
                    work_item_id,
                    reason,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Reject {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                            reason: parse_task_verdict(&reason)?,
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Split {
                    work_item_id,
                    after_span,
                    left_title,
                    right_title,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    let spans = store.task_spans_for_work_item(&work_item_id)?;
                    let after_span_id = statsai_core::TaskSpanId(after_span);
                    let span_index = spans
                        .iter()
                        .position(|span| span.span_id == after_span_id)
                        .with_context(|| {
                            format!(
                                "span {} is not a member of work item {}",
                                after_span_id.0, work_item_id.0
                            )
                        })?;
                    if span_index + 1 >= spans.len() {
                        bail!("cannot split after the last span in a work item");
                    }
                    let before_span_id = spans[span_index + 1].span_id.clone();
                    let verification =
                        store.upsert_task_verification(TaskVerificationAction::Split {
                            after_span_id,
                            before_span_id: Some(before_span_id),
                            left_title,
                            right_title,
                        })?;
                    (
                        verification,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Merge {
                    left_work_item_id,
                    right_work_item_id,
                    title,
                } => {
                    let left_work_item_id = WorkItemId(left_work_item_id);
                    let right_work_item_id = WorkItemId(right_work_item_id);
                    let left = store
                        .work_item(&left_work_item_id)?
                        .with_context(|| format!("unknown work item {}", left_work_item_id.0))?;
                    let right = store
                        .work_item(&right_work_item_id)?
                        .with_context(|| format!("unknown work item {}", right_work_item_id.0))?;
                    if left.project_bucket != right.project_bucket {
                        bail!(
                            "cannot merge work items from different project buckets: {} vs {}",
                            left.project_bucket,
                            right.project_bucket
                        );
                    }
                    (
                        store.upsert_task_verification(TaskVerificationAction::Merge {
                            left_work_item_id: left_work_item_id.clone(),
                            right_work_item_id: right_work_item_id.clone(),
                            left_anchor_span_id: left.anchor_span_id.clone(),
                            right_anchor_span_id: right.anchor_span_id.clone(),
                            title,
                        })?,
                        BTreeSet::from([left.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Rename {
                    work_item_id,
                    title,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Rename {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                            title,
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
            };
            let rebuilt = store.rebuild_task_work_items_for_project_buckets(&buckets)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "verification": verification,
                    "work_items_rebuilt": rebuilt
                }))?
            );
        }
        TaskSubcommand::Stats { json } => {
            let stats = store.task_stats()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stats_json_value(&stats))?
                );
            } else {
                println!(
                    "task stats: spans={} work_items={} verified={:.1}% no_git={:.1}% cross_provider={:.1}% rejected_meta={:.1}% avg_spans_per_item={:.2}",
                    format_u64(stats.total_spans),
                    format_u64(stats.total_work_items),
                    stats.verified_percentage,
                    stats.no_git_percentage,
                    stats.cross_provider_percentage,
                    stats.rejected_meta_percentage,
                    stats.average_spans_per_work_item
                );
            }
        }
        TaskSubcommand::Benchmark { json } => {
            let report = store.task_benchmark_report()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&benchmark_json_value(&report))?
                );
            } else {
                println!(
                    "benchmark: verified_spans={} adjacent_pairs={} has_ground_truth={} has_pairwise_ground_truth={} constraints_preserved={} beats_all_baselines={} shipping_gate_ready={}",
                    format_u64(report.verified_spans),
                    format_u64(report.verified_adjacent_pairs),
                    report.has_verified_ground_truth,
                    report.has_verified_pairwise_ground_truth,
                    report.manual_constraints_preserved,
                    report.beats_all_baselines,
                    report.shipping_gate_ready
                );
                if !report.gate_blockers.is_empty() {
                    println!("  gate_blockers={}", report.gate_blockers.join(","));
                }
                if !report.failing_baselines.is_empty() {
                    println!("  failing_baselines={}", report.failing_baselines.join(","));
                }
                if !report.has_verified_ground_truth {
                    println!(
                        "  note: no verified task ground truth yet; run `statsai task verify ...` before treating benchmark scores as a shipping gate"
                    );
                } else if !report.has_verified_pairwise_ground_truth {
                    println!(
                        "  note: verified labels exist, but no adjacent verified span pairs exist yet; verify a multi-span work item or record a split/merge before using the shipping gate"
                    );
                }
                print_benchmark_metrics("current", &report.current);
                for baseline in &report.baselines {
                    print_benchmark_metrics(&baseline.name, &baseline.metrics);
                }
            }
        }
        TaskSubcommand::Export { level, format } => {
            let level = level.to_ascii_lowercase();
            let format = format.to_ascii_lowercase();
            match (level.as_str(), format.as_str()) {
                ("work-item", "json") | ("work_item", "json") => {
                    println!("{}", serde_json::to_string_pretty(&store.work_items()?)?);
                }
                ("work-item", "jsonl") | ("work_item", "jsonl") => {
                    for item in store.work_items()? {
                        println!("{}", serde_json::to_string(&item)?);
                    }
                }
                ("span", "json") => {
                    println!("{}", serde_json::to_string_pretty(&store.task_spans()?)?);
                }
                ("span", "jsonl") => {
                    for span in store.task_spans()? {
                        println!("{}", serde_json::to_string(&span)?);
                    }
                }
                _ => bail!("unsupported export level/format: {level}/{format}"),
            }
        }
        TaskSubcommand::Rebuild {
            provider,
            source_id,
            all,
        } => {
            let report = if all || (provider.is_none() && source_id.is_none()) {
                store.rebuild_all_task_work_items_report()?
            } else {
                let buckets = selected_rebuild_project_buckets(
                    store,
                    provider.as_deref(),
                    source_id.as_deref(),
                )?;
                store.rebuild_task_work_items_for_project_buckets_report(&buckets)?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&task_rebuild_report_json_value(&report))?
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct TaskListSelection {
    items: Vec<WorkItem>,
    hidden_rejected_meta: usize,
}

fn task_list_selection(
    store: &Store,
    provider: Option<&str>,
    status_filter: Option<&TaskStatus>,
) -> Result<TaskListSelection> {
    let items = store
        .work_items()?
        .into_iter()
        .filter(|item| {
            provider
                .is_none_or(|provider| item.providers.iter().any(|candidate| candidate == provider))
        })
        .collect::<Vec<_>>();
    if let Some(status) = status_filter {
        return Ok(TaskListSelection {
            items: items
                .into_iter()
                .filter(|item| &item.status == status)
                .collect::<Vec<_>>(),
            hidden_rejected_meta: 0,
        });
    }
    let hidden_rejected_meta = items
        .iter()
        .filter(|item| item.status == TaskStatus::RejectedMeta)
        .count();
    Ok(TaskListSelection {
        items: items
            .into_iter()
            .filter(|item| item.status != TaskStatus::RejectedMeta)
            .collect::<Vec<_>>(),
        hidden_rejected_meta,
    })
}

#[cfg(test)]
fn filtered_task_list_items(
    store: &Store,
    provider: Option<&str>,
    status_filter: Option<&TaskStatus>,
) -> Result<Vec<WorkItem>> {
    Ok(task_list_selection(store, provider, status_filter)?.items)
}

fn selected_rebuild_project_buckets(
    store: &Store,
    provider: Option<&str>,
    source_id: Option<&str>,
) -> Result<BTreeSet<String>> {
    Ok(store
        .task_spans()?
        .into_iter()
        .filter(|span| provider.is_none_or(|provider| span.provider == provider))
        .filter(|span| source_id.is_none_or(|source_id| span.source_id.0 == source_id))
        .map(|span| span.project_bucket)
        .collect::<BTreeSet<_>>())
}

#[derive(Debug, Serialize)]
struct TaskShowOutput {
    work_item: WorkItem,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    spans: Vec<TaskSpan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    verifications: Vec<TaskVerification>,
}

fn load_task_show_output(
    store: &Store,
    work_item_id: &WorkItemId,
    include_evidence: bool,
) -> Result<TaskShowOutput> {
    let work_item = store
        .work_item(work_item_id)?
        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
    let spans = if include_evidence {
        store.task_spans_for_work_item(work_item_id)?
    } else {
        Vec::new()
    };
    let verifications = if include_evidence {
        relevant_task_verifications(&store.task_verifications()?, &spans)
    } else {
        Vec::new()
    };
    Ok(TaskShowOutput {
        work_item,
        spans,
        verifications,
    })
}

fn relevant_task_verifications(
    verifications: &[TaskVerification],
    spans: &[TaskSpan],
) -> Vec<TaskVerification> {
    let span_ids = spans
        .iter()
        .map(|span| span.span_id.0.as_str())
        .collect::<HashSet<_>>();
    verifications
        .iter()
        .filter(|verification| {
            verification
                .action
                .span_ids()
                .into_iter()
                .any(|span_id| span_ids.contains(span_id.0.as_str()))
        })
        .cloned()
        .collect()
}

fn format_task_verification(verification: &TaskVerification) -> String {
    match &verification.action {
        TaskVerificationAction::Accept { anchor_span_id, .. } => {
            format!("accept(anchor={})", anchor_span_id.0)
        }
        TaskVerificationAction::Reject {
            anchor_span_id,
            reason,
            ..
        } => format!("reject(anchor={}, reason={:?})", anchor_span_id.0, reason),
        TaskVerificationAction::Rename {
            anchor_span_id,
            title,
            ..
        } => format!("rename(anchor={}, title={title})", anchor_span_id.0),
        TaskVerificationAction::Split {
            after_span_id,
            before_span_id,
            left_title,
            right_title,
        } => format!(
            "split(after={}, before={}, left_title={}, right_title={})",
            after_span_id.0,
            before_span_id
                .as_ref()
                .map(|span_id| span_id.0.as_str())
                .unwrap_or("-"),
            left_title.as_deref().unwrap_or("-"),
            right_title.as_deref().unwrap_or("-")
        ),
        TaskVerificationAction::Merge {
            left_anchor_span_id,
            right_anchor_span_id,
            title,
            ..
        } => format!(
            "merge(left={}, right={}, title={})",
            left_anchor_span_id.0,
            right_anchor_span_id.0,
            title.as_deref().unwrap_or("-")
        ),
    }
}

fn parse_task_status_filter(value: &str) -> Result<TaskStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(TaskStatus::Auto),
        "needs_review" => Ok(TaskStatus::NeedsReview),
        "verified" => Ok(TaskStatus::Verified),
        "rejected_meta" => Ok(TaskStatus::RejectedMeta),
        _ => bail!("unsupported task status {value}"),
    }
}

fn parse_task_verdict(value: &str) -> Result<TaskVerdict> {
    match value.trim().to_ascii_lowercase().as_str() {
        "meta" => Ok(TaskVerdict::Meta),
        "system" => Ok(TaskVerdict::System),
        "noise" => Ok(TaskVerdict::Noise),
        _ => bail!("unsupported task verdict {value}"),
    }
}

fn print_work_item(work_item: &WorkItem) {
    println!(
        "{} status={:?} confidence={:?} spans={} events={} tokens={} providers={} title={}",
        work_item.work_item_id.0,
        work_item.status,
        work_item.confidence,
        work_item.span_count,
        work_item.event_count,
        format_u64(work_item.total_tokens),
        work_item.providers.join(","),
        work_item.title
    );
    println!(
        "  project_bucket={} started_at={} ended_at={} no_git={} cross_provider={}",
        work_item.project_bucket,
        work_item.started_at.to_rfc3339(),
        work_item.ended_at.to_rfc3339(),
        work_item.no_git,
        work_item.cross_provider
    );
    if !work_item.review_reasons.is_empty() {
        println!("  review_reasons={}", work_item.review_reasons.join(","));
    }
    if !work_item.continuation_reasons.is_empty() {
        println!(
            "  continuation_reasons={}",
            work_item.continuation_reasons.join(",")
        );
    }
}

fn format_task_list_item(work_item: &WorkItem) -> String {
    let mut line = format!(
        "{} status={:?} confidence={:?} spans={} tokens={} providers={} title={}",
        work_item.work_item_id.0,
        work_item.status,
        work_item.confidence,
        work_item.span_count,
        format_u64(work_item.total_tokens),
        work_item.providers.join(","),
        work_item.title
    );
    if !work_item.review_reasons.is_empty() {
        line.push_str(" review=");
        line.push_str(&work_item.review_reasons.join(","));
    }
    line
}

fn stats_json_value(stats: &statsai_store::TaskStats) -> Value {
    json!({
        "total_spans": stats.total_spans,
        "total_work_items": stats.total_work_items,
        "verified_percentage": stats.verified_percentage,
        "no_git_percentage": stats.no_git_percentage,
        "cross_provider_percentage": stats.cross_provider_percentage,
        "rejected_meta_percentage": stats.rejected_meta_percentage,
        "average_spans_per_work_item": stats.average_spans_per_work_item,
    })
}

fn task_rebuild_report_json_value(report: &statsai_store::TaskRebuildReport) -> Value {
    json!({
        "work_items_rebuilt": report.work_items_rebuilt,
        "work_items_deleted": report.work_items_deleted,
        "affected_bucket_count": report.affected_bucket_count,
        "affected_segment_count": report.affected_segment_count,
        "touched_span_count": report.touched_span_count,
        "timings_ms": {
            "delete": report.timings.delete_ms,
            "span_load": report.timings.span_load_ms,
            "verification_load": report.timings.verification_load_ms,
            "grouping": report.timings.grouping_ms,
            "title_selection": report.timings.title_selection_ms,
            "insert": report.timings.insert_ms,
        }
    })
}

fn benchmark_json_value(report: &statsai_store::TaskBenchmarkReport) -> Value {
    json!({
        "verified_adjacent_pairs": report.verified_adjacent_pairs,
        "verified_spans": report.verified_spans,
        "has_verified_ground_truth": report.has_verified_ground_truth,
        "has_verified_pairwise_ground_truth": report.has_verified_pairwise_ground_truth,
        "manual_constraints_preserved": report.manual_constraints_preserved,
        "beats_all_baselines": report.beats_all_baselines,
        "shipping_gate_ready": report.shipping_gate_ready,
        "failing_baselines": report.failing_baselines,
        "gate_blockers": report.gate_blockers,
        "current": benchmark_metrics_json_value(&report.current),
        "baselines": report.baselines.iter().map(|baseline| {
            json!({
                "name": baseline.name,
                "metrics": benchmark_metrics_json_value(&baseline.metrics),
            })
        }).collect::<Vec<_>>(),
    })
}

fn benchmark_metrics_json_value(metrics: &statsai_store::TaskBenchmarkMetrics) -> Value {
    json!({
        "adjacent_precision": metrics.adjacent_precision,
        "adjacent_recall": metrics.adjacent_recall,
        "adjacent_f1": metrics.adjacent_f1,
        "cluster_precision": metrics.cluster_precision,
        "cluster_recall": metrics.cluster_recall,
        "cluster_f1": metrics.cluster_f1,
        "meta_precision": metrics.meta_precision,
        "meta_recall": metrics.meta_recall,
        "meta_f1": metrics.meta_f1,
    })
}

fn print_benchmark_metrics(name: &str, metrics: &statsai_store::TaskBenchmarkMetrics) {
    println!(
        "  {} adjacent_f1={:.3} (p={:.3} r={:.3}) cluster_f1={:.3} (p={:.3} r={:.3}) meta_f1={:.3} (p={:.3} r={:.3})",
        name,
        metrics.adjacent_f1,
        metrics.adjacent_precision,
        metrics.adjacent_recall,
        metrics.cluster_f1,
        metrics.cluster_precision,
        metrics.cluster_recall,
        metrics.meta_f1,
        metrics.meta_precision,
        metrics.meta_recall
    );
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
            let price_cents = price.cents();
            let currency = currency.into_string();
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
            let price_cents = price.cents();
            let currency = currency.into_string();
            let account = resolve_existing_provider_account(
                store,
                &provider,
                provider_account_id.as_deref(),
                provider_user_id.as_deref(),
                email.as_deref(),
                label,
            )?;
            let started_at = parse_date(&started_at)?;
            let paid_at = paid_at
                .as_deref()
                .map(parse_date)
                .transpose()?
                .or(Some(started_at));
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
                .map(|input| build_reported_import_record(input, device_id))
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
            total_usage.add_summary(&record.record.summary);
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
            store.upsert_source(&record.record.source)?;
            written += store.upsert_summaries(std::slice::from_ref(&record.record.summary))?;
        }
    }
    migrate_legacy_reported_source_assignments(store, reports)?;
    if replace {
        delete_orphaned_legacy_reported_sources(store, reports)?;
    } else {
        delete_legacy_reported_alias_summaries(store, reports)?;
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
        .flat_map(reported_replace_keys)
        .collect();
    matching_reported_summary_ids_for_keys(store, &incoming_keys)
}

fn matching_legacy_reported_summary_ids(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<Vec<statsai_core::SummaryId>> {
    let incoming_keys: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .flat_map(reported_replace_keys)
        .collect();
    let summary_ids = store
        .summaries()?
        .into_iter()
        .filter(|summary| is_legacy_reported_summary_format(&summary.metadata.summary_format))
        .filter(|summary| {
            matches!(
                summary.source.source_kind,
                SourceKind::ExternalReport | SourceKind::Manual
            )
        })
        .filter(|summary| {
            reported_replace_keys_from_summary(summary)
                .iter()
                .any(|key| incoming_keys.contains(key))
        })
        .map(|summary| summary.summary_id)
        .collect();
    Ok(summary_ids)
}

fn matching_reported_summary_ids_for_keys(
    store: &Store,
    incoming_keys: &BTreeSet<ReportedReplaceKey>,
) -> Result<Vec<statsai_core::SummaryId>> {
    let summary_ids = store
        .summaries()?
        .into_iter()
        .filter(|summary| {
            matches!(
                summary.source.source_kind,
                SourceKind::ExternalReport | SourceKind::Manual
            )
        })
        .filter(|summary| {
            reported_replace_keys_from_summary(summary)
                .iter()
                .any(|key| incoming_keys.contains(key))
        })
        .map(|summary| summary.summary_id)
        .collect();
    Ok(summary_ids)
}

fn delete_legacy_reported_alias_summaries(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let summary_ids = matching_legacy_reported_summary_ids(store, reports)?;
    let deleted = store.delete_summaries(&summary_ids)?;
    delete_orphaned_legacy_reported_sources(store, reports)?;
    Ok(deleted)
}

fn migrate_legacy_reported_source_assignments(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let mut migrated = 0;
    let now = Utc::now();
    for record in reports.iter().flat_map(|report| report.records.iter()) {
        let canonical_source = &record.record.source;
        for legacy_source_id in &record.legacy_replacement_source_ids {
            let Some(legacy_source) = store.source(legacy_source_id)? else {
                continue;
            };
            if !is_reported_usage_source(&legacy_source) {
                continue;
            }
            for assignment in store.list_source_account_assignments_for_source(legacy_source_id)? {
                let migrated_assignment = SourceAccountAssignment {
                    schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                    assignment_id: source_account_assignment_id(
                        &canonical_source.source_id,
                        &assignment.provider_account_id,
                        assignment.started_at,
                    ),
                    source_id: canonical_source.source_id.clone(),
                    provider: canonical_source.provider.clone(),
                    provider_account_id: assignment.provider_account_id.clone(),
                    started_at: assignment.started_at,
                    ended_at: assignment.ended_at,
                    record_source: assignment.record_source.clone(),
                    verified_at: assignment.verified_at,
                    created_at: assignment.created_at,
                    updated_at: now,
                };
                let already_exists = store
                    .source_account_assignment(&migrated_assignment.assignment_id)?
                    .is_some();
                if already_exists {
                    continue;
                }
                store.upsert_source_account_assignment(&migrated_assignment)?;
                migrated += 1;
            }
        }
    }
    Ok(migrated)
}

fn delete_orphaned_legacy_reported_sources(
    store: &Store,
    reports: &[ReportedImportReport],
) -> Result<u64> {
    let mut deleted = 0;
    for source_id in legacy_reported_source_ids(reports) {
        let Some(source) = store.source(&source_id)? else {
            continue;
        };
        if !is_reported_usage_source(&source) {
            continue;
        }
        if !store.events_for_source(&source_id)?.is_empty()
            || !store.summaries_for_source(&source_id)?.is_empty()
        {
            continue;
        }
        if store.delete_source(&source_id)? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

fn is_reported_usage_source(source: &SourceLocation) -> bool {
    matches!(
        source.source_kind,
        SourceKind::ExternalReport | SourceKind::Manual
    ) && source.adapter_id.as_deref() == Some(REPORTED_USAGE_IMPORT_ADAPTER_ID)
}

fn legacy_reported_source_ids(reports: &[ReportedImportReport]) -> Vec<SourceId> {
    let source_ids: BTreeSet<_> = reports
        .iter()
        .flat_map(|report| report.records.iter())
        .flat_map(|record| {
            record
                .legacy_replacement_source_ids
                .iter()
                .map(|source_id| source_id.0.clone())
        })
        .collect();
    source_ids.into_iter().map(SourceId).collect()
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

fn reported_replace_keys(record: &ReportedImportRecord) -> Vec<ReportedReplaceKey> {
    let mut keys = vec![reported_replace_key_from_summary(&record.record.summary)];
    for source_id in &record.legacy_replacement_source_ids {
        let mut key = keys[0].clone();
        key.source_id = source_id.0.clone();
        keys.push(key);
    }
    keys
}

fn canonical_reported_summary_format(value: &str) -> &str {
    match value {
        "ccusage_daily" | "custom_daily" => "manual_daily",
        "custom_period_summary" => "manual_period_summary",
        _ => value,
    }
}

fn is_legacy_reported_summary_format(value: &str) -> bool {
    canonical_reported_summary_format(value) != value
}

fn reported_replace_keys_from_summary(summary: &UsageSummary) -> [ReportedReplaceKey; 1] {
    [reported_replace_key_from_summary(summary)]
}

fn reported_replace_key_from_summary(summary: &UsageSummary) -> ReportedReplaceKey {
    let summary_format = canonical_reported_summary_format(&summary.metadata.summary_format);
    ReportedReplaceKey {
        provider: summary.provider.clone(),
        provider_account_id: summary.provider_account_id.as_ref().map(|id| id.0.clone()),
        summary_format: summary_format.to_string(),
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

fn build_reported_import_record(
    input: ReportedUsageSummaryInput,
    device_id: &str,
) -> Result<ReportedImportRecord> {
    let legacy_replacement_source_ids = legacy_alias_replacement_source_ids(&input);
    let record = build_reported_usage_summary(input, device_id)?;
    Ok(ReportedImportRecord {
        record,
        legacy_replacement_source_ids,
    })
}

fn legacy_alias_replacement_source_ids(input: &ReportedUsageSummaryInput) -> Vec<SourceId> {
    if input.evidence_path.is_some() || input.evidence_id.is_some() {
        return Vec::new();
    }
    let canonical_format = canonical_reported_summary_format(&input.report_format);
    let legacy_formats: &[&str] = match canonical_format {
        "manual_daily" => &["ccusage_daily", "custom_daily"],
        "manual_period_summary" => &["custom_period_summary"],
        _ => &[],
    };
    if legacy_formats.is_empty() {
        return Vec::new();
    }

    let identity_key = reported_summary_identity_key(input);
    legacy_formats
        .iter()
        .map(|format| {
            let evidence_key = format!(
                "{}:{}:{}:{}",
                input.provider, input.source_name, identity_key, format
            );
            let source_path_hash = hash_text(&evidence_key);
            statsai_source_id(
                &input.provider,
                input.source_kind.clone(),
                &source_path_hash,
            )
        })
        .collect()
}

fn reported_summary_identity_key(input: &ReportedUsageSummaryInput) -> String {
    input
        .provider_account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            input
                .email
                .as_deref()
                .map(normalize_email)
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            input
                .provider_user_id
                .as_deref()
                .map(normalize_provider_user_id)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "unassigned".to_string())
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
    let now = Utc::now();
    let (events, summaries) = match period {
        ReportPeriod::LastDays(days) => (
            store.events_in_period(now - Duration::days(days), now)?,
            Vec::new(),
        ),
        ReportPeriod::AllTime => (store.events()?, store.summaries()?),
    };
    let report = build_usage_report(
        &events,
        &summaries,
        &store.list_sources()?,
        &store.list_accounts()?,
        &store.list_subscriptions()?,
        period,
        now,
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

fn effective_sync_preferences(store: &Store, command: &SyncCommand) -> Result<SyncPreferences> {
    let mut preferences = store.sync_preferences()?;
    if command.include_projects {
        preferences.include_projects = true;
    }
    if command.exclude_projects {
        preferences.include_projects = false;
        preferences.include_tasks = false;
    }
    if command.include_tasks {
        preferences.include_projects = true;
        preferences.include_tasks = true;
    }
    if command.exclude_tasks {
        preferences.include_tasks = false;
    }

    Ok(preferences.normalized())
}

fn apply_sync_preference_overrides(
    store: &Store,
    command: &SyncCommand,
) -> Result<SyncPreferences> {
    let original = store.sync_preferences()?;
    let preferences = effective_sync_preferences(store, command)?;
    if preferences != original {
        store.set_sync_preferences(preferences)?;
        eprintln!(
            "sync preferences updated: projects={} tasks={}",
            if preferences.include_projects {
                "enabled"
            } else {
                "disabled"
            },
            if preferences.include_tasks {
                "enabled"
            } else {
                "disabled"
            }
        );
        if (!original.include_projects && preferences.include_projects)
            || (!original.include_tasks && preferences.include_tasks)
        {
            eprintln!(
                "sync preferences changed privacy/backfill scope; the next sync may resend historical summaries to update the hosted mirror"
            );
        }
    }
    Ok(preferences)
}

fn sync(command: SyncCommand, store: &Store, device_id: &str) -> Result<()> {
    if command.since_last && (command.full || command.rebuild_rollups) {
        bail!("--since-last cannot be combined with --full or --rebuild-rollups");
    }

    let sync_preferences = effective_sync_preferences(store, &command)?;

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
        return sync_status(store, device_id);
    }

    let target = sync_target(&command)?;
    if command.sink == "http" && !command.dry_run {
        maybe_reset_http_sync_tracking_if_remote_changed(&command, store, &target)?;
    }
    let (mut batch, payload_mode) = build_sync_batch(&command, store, device_id, &target)?;
    let hosted_task_sync_enabled = maybe_disable_http_hosted_task_sync_payload(
        &command,
        &target,
        sync_preferences,
        &mut batch,
    )?;

    if command.dry_run {
        eprintln!(
            "dry run: sink={} mode={} include_projects={} include_tasks={} sources={} events={} summaries={} task_buckets={} task_verifications={}",
            command.sink,
            sync_payload_mode_name(payload_mode),
            sync_preferences.include_projects,
            sync_preferences.include_tasks,
            batch.sources.len(),
            batch.events.len(),
            batch.summaries.len()
            ,
            batch.task_buckets.len(),
            batch.task_verifications.len(),
        );
        return Ok(());
    }

    let persisted_sync_preferences = apply_sync_preference_overrides(store, &command)?;
    debug_assert_eq!(persisted_sync_preferences, sync_preferences);

    let result = (|| -> Result<()> {
        match command.sink.as_str() {
            "stdout" => {
                StdoutSink.send(&batch)?;
                record_sync_batch_success(store, &command.sink, &target, &batch)?;
                Ok(())
            }
            "file" => {
                let output = command
                    .output
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("statsai-sync-batch.json"));
                FileSink::new(output).send(&batch)?;
                record_sync_batch_success(store, &command.sink, &target, &batch)?;
                Ok(())
            }
            "http" => {
                let endpoint = http_sync_endpoint(&command)?;
                let auth_token = resolve_http_auth_token(&command, false)?;
                send_http_sync_batch(
                    store,
                    HttpSyncBatchRequest {
                        sink: &command.sink,
                        target: &target,
                        endpoint: &endpoint,
                        auth_token,
                        payload_mode,
                        hosted_task_sync_enabled,
                    },
                    &batch,
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

    let sync_preferences = effective_sync_preferences(store, command)?;
    let auth_token = resolve_http_auth_token(command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let remote = http_remote_preflight_status(target, &auth_token)?;
    let local_verify = sync_local_verify(
        store,
        "http",
        target,
        Some(&local_state),
        sync_preferences.include_projects,
    )?;
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
                "remote_reset_scope": "paired_device",
                "would_clear_local_sync_tracking": true,
                "dry_run": true,
            }))?
        );
        return Ok(());
    }

    if !command.yes {
        bail!(
            "--reset-remote deletes mirrored hosted sync data for this paired device; rerun with --yes"
        );
    }

    eprintln!(
        "warning: --reset-remote deletes mirrored hosted sync data for this paired device. Other paired devices are not affected."
    );

    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let remote = http_remote_reset(&endpoint, &auth_token)?;
    ensure_device_remote_reset_response(&remote)?;
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

fn ensure_device_remote_reset_response(response: &Value) -> Result<()> {
    if response.get("ok").and_then(Value::as_bool) != Some(true)
        || response.get("scope").and_then(Value::as_str) != Some("device_mirror")
        || response.get("device_id").and_then(Value::as_str).is_none()
    {
        bail!("remote reset returned an unexpected scope; local sync tracking was not cleared");
    }
    Ok(())
}

fn build_sync_batch(
    command: &SyncCommand,
    store: &Store,
    device_id: &str,
    target: &str,
) -> Result<(SyncBatch, SyncPayloadMode)> {
    let created_at = Utc::now();
    let batch_id = format!("batch_{}", created_at.timestamp_millis());
    let sync_preferences = effective_sync_preferences(store, command)?;
    let include_projects = sync_preferences.include_projects;
    let payload_mode = sync_payload_mode(command)?;
    let state = if command.sink == "http" || command.since_last {
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
            .map(|event| sanitize_event_for_sync_with_projects(event, include_projects))
            .collect()
    };
    let all_passthrough_summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        store
            .summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
            .filter(is_http_rollup_passthrough_summary)
            .collect()
    } else {
        Vec::new()
    };
    let mut summaries: Vec<_> = if payload_mode == SyncPayloadMode::Rollups {
        Vec::new()
    } else {
        store
            .summaries_after(summary_cursor)?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
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
    let snapshot_source_ids = all_sources
        .iter()
        .map(|source| source.source_id.clone())
        .collect::<Vec<_>>();
    let snapshot_provider_account_ids = all_accounts
        .iter()
        .map(|account| account.provider_account_id.clone())
        .collect::<Vec<_>>();
    let snapshot_assignment_ids = all_source_account_assignments
        .iter()
        .map(|assignment| assignment.assignment_id.clone())
        .collect::<Vec<_>>();
    let snapshot_subscription_ids = all_subscriptions
        .iter()
        .map(|subscription| subscription.subscription_id.clone())
        .collect::<Vec<_>>();
    let mut authoritative_snapshot = None;
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
    let (task_buckets, task_verifications) = if sync_preferences.include_tasks {
        let task_verification_cursor = if command.sink == "http" || command.since_last {
            store.sync_task_verification_cursor(&command.sink, target)?
        } else {
            None
        };
        let full_task_sync = command.full || state.is_none();
        let task_buckets = store.pending_task_bucket_snapshots_for_sync(
            &command.sink,
            target,
            device_id,
            full_task_sync,
            task_verification_cursor.clone(),
        )?;
        let task_verifications = if full_task_sync {
            store.task_verifications()?
        } else {
            store.pending_task_verifications_for_sync(&command.sink, target)?
        };
        (task_buckets, task_verifications)
    } else {
        (Vec::new(), Vec::new())
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

        let failed_without_resume = state.as_ref().is_some_and(|state| {
            state.failure_count > 0 && state.pending_resume_batch_id.is_none()
        });
        let has_pending_resume = state
            .as_ref()
            .and_then(|state| state.pending_resume_batch_id.as_deref())
            .is_some();
        let force_full_rollup_sync = command.full
            || command.rebuild_rollups
            || state.is_none()
            || has_pending_resume
            || (!command.since_last && failed_without_resume);
        let full_rollup_sync = force_full_rollup_sync;
        let all_rollups: Vec<_> = store
            .all_sync_rollup_summaries()?
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
            .collect();
        if full_rollup_sync {
            authoritative_snapshot = Some(SyncAuthoritativeSnapshot {
                snapshot_id: format!("{batch_id}_authoritative"),
                part_index: 0,
                part_count: 1,
                source_ids: snapshot_source_ids,
                provider_account_ids: snapshot_provider_account_ids,
                source_account_assignment_ids: snapshot_assignment_ids,
                subscription_ids: snapshot_subscription_ids,
                summary_ids: all_passthrough_summaries
                    .iter()
                    .chain(all_rollups.iter())
                    .map(|summary| summary.summary_id.clone())
                    .collect(),
            });
        }
        let rollups = if full_rollup_sync {
            all_rollups
        } else {
            store.pending_summaries_for_sync(&command.sink, target, &all_rollups)?
        };
        let passthrough_summaries = if full_rollup_sync {
            all_passthrough_summaries
        } else {
            store.pending_summaries_for_sync(&command.sink, target, &all_passthrough_summaries)?
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
        summaries.extend(passthrough_summaries);
        summaries.extend(
            rollups
                .into_iter()
                .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects)),
        );
    }

    Ok((
        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id,
            device_id: device_id.to_string(),
            sources,
            accounts,
            source_account_assignments,
            subscriptions,
            events,
            summaries,
            task_buckets,
            task_verifications,
            authoritative_snapshot,
            created_at,
        },
        payload_mode,
    ))
}

#[cfg(test)]
fn record_rollup_sync_success(
    store: &Store,
    sink: &str,
    target: &str,
    batch: &SyncBatch,
) -> Result<()> {
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    record_rollup_sync_chunk_success(store, sink, target, &logical_batch_id, batch)?;
    store.clear_pending_sync_resume(sink, target)?;
    Ok(())
}

fn record_rollup_sync_chunk_success(
    store: &Store,
    sink: &str,
    target: &str,
    logical_batch_id: &str,
    batch: &SyncBatch,
) -> Result<()> {
    store.record_rollup_chunk_sync_success(sink, target, logical_batch_id, batch)?;
    snapshot::invalidate_dashboard_cache();
    Ok(())
}

fn record_sync_batch_success(
    store: &Store,
    sink: &str,
    target: &str,
    batch: &SyncBatch,
) -> Result<()> {
    let task_verification_cursor = sync_batch_task_verification_cursor(batch);
    store.record_sync_success(
        sink,
        target,
        &batch.batch_id,
        &batch.events,
        &batch.summaries,
        task_verification_cursor.as_ref(),
    )?;
    store.record_task_bucket_snapshots_synced(
        sink,
        target,
        &batch.device_id,
        &batch.task_buckets,
    )?;
    store.record_task_verifications_synced(sink, target, &batch.task_verifications)?;
    snapshot::invalidate_dashboard_cache();
    Ok(())
}

fn maybe_disable_http_hosted_task_sync_payload(
    command: &SyncCommand,
    target: &str,
    sync_preferences: SyncPreferences,
    batch: &mut SyncBatch,
) -> Result<bool> {
    if command.sink != "http" {
        return Ok(sync_preferences.include_tasks);
    }
    if !sync_preferences.include_tasks {
        return Ok(false);
    }
    if http_remote_hosted_tasks_enabled(command, target)? {
        return Ok(true);
    }
    if batch.task_buckets.is_empty() && batch.task_verifications.is_empty() {
        return Ok(false);
    }
    eprintln!(
        "http sync: hosted task access is not enabled for this account; skipping {} task buckets and {} task verifications",
        batch.task_buckets.len(),
        batch.task_verifications.len()
    );
    batch.task_buckets.clear();
    batch.task_verifications.clear();
    Ok(false)
}

fn sync_batch_task_verification_cursor(batch: &SyncBatch) -> Option<TaskVerificationCursor> {
    batch
        .task_buckets
        .iter()
        .filter_map(|bucket| bucket.applied_verification_cursor.clone())
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.verification_id.0.cmp(&right.verification_id.0))
        })
}

struct HttpSyncBatchRequest<'a> {
    sink: &'a str,
    target: &'a str,
    endpoint: &'a str,
    auth_token: Option<String>,
    payload_mode: SyncPayloadMode,
    hosted_task_sync_enabled: bool,
}

fn send_http_sync_batch(
    store: &Store,
    request: HttpSyncBatchRequest<'_>,
    batch: &SyncBatch,
) -> Result<()> {
    let task_sync_auth_token = request.auth_token.clone();
    let http_sink = HttpSink::new(request.endpoint, request.auth_token)?;
    let batches = if request.payload_mode == SyncPayloadMode::Rollups {
        split_http_rollup_sync_batches(batch)
    } else {
        vec![batch.clone()]
    };
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();

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
        if request.payload_mode == SyncPayloadMode::Rollups {
            send_http_rollup_chunk_with_retry(&http_sink, chunk, &|synced_chunk| {
                record_rollup_sync_chunk_success(
                    store,
                    request.sink,
                    request.target,
                    &logical_batch_id,
                    synced_chunk,
                )
            })?;
        } else {
            let ack = http_sink.send_with_ack(chunk)?;
            println!("{}", serde_json::to_string_pretty(&ack)?);
            record_sync_batch_success(store, request.sink, request.target, batch)?;
        }
    }
    if request.payload_mode == SyncPayloadMode::Rollups {
        store.clear_pending_sync_resume(request.sink, request.target)?;
    }
    if request.hosted_task_sync_enabled {
        match pull_remote_task_verifications(
            store,
            request.sink,
            request.target,
            request.endpoint,
            task_sync_auth_token.as_deref(),
        ) {
            Ok(Some(cursor)) => {
                store.record_sync_success(
                    request.sink,
                    request.target,
                    &batch.batch_id,
                    &[],
                    &[],
                    Some(&cursor),
                )?;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "warning: sync upload succeeded, but pulling hosted task verifications failed: {error}"
                );
            }
        }
    }
    Ok(())
}

fn pull_remote_task_verifications(
    store: &Store,
    sink: &str,
    target: &str,
    endpoint: &str,
    auth_token: Option<&str>,
) -> Result<Option<TaskVerificationCursor>> {
    let Some(auth_token) = auth_token.filter(|token| !token.is_empty()) else {
        return Ok(None);
    };
    validate_authenticated_http_endpoint(endpoint)?;
    let Some(feed_url) = http_task_verification_feed_url(endpoint) else {
        return Ok(None);
    };
    let mut request = ureq::get(&feed_url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    if let Some(cursor) = store.sync_task_verification_cursor(sink, target)? {
        request = request
            .query("updatedAt", &cursor.updated_at.to_rfc3339())
            .query("verificationId", &cursor.verification_id.0);
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) if optional_task_verification_feed_status(code) => {
            return Ok(None);
        }
        Err(error) => return Err(http_request_error("pull task verifications", error)),
    };
    let feed: TaskVerificationFeedResponse = response
        .into_json()
        .context("parse task verification feed")?;
    let mut affected_buckets = BTreeSet::new();
    for verification in &feed.verifications {
        if store.merge_task_verification(verification)? {
            affected_buckets.extend(store.project_buckets_for_task_verification(verification)?);
        }
    }
    store.record_task_verifications_synced(sink, target, &feed.verifications)?;
    if !affected_buckets.is_empty() {
        store.rebuild_task_work_items_for_project_buckets(&affected_buckets)?;
        snapshot::invalidate_dashboard_cache();
    }
    Ok(feed.next_cursor)
}

fn optional_task_verification_feed_status(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
}

fn send_http_rollup_chunk_with_retry<F>(
    http_sink: &HttpSink,
    chunk: &SyncBatch,
    on_success: &F,
) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
{
    send_http_rollup_chunk_with_retry_using(chunk, &|chunk| {
        let ack = http_sink.send_with_ack(chunk)?;
        println!("{}", serde_json::to_string_pretty(&ack)?);
        on_success(chunk)?;
        Ok(())
    })
}

fn send_http_rollup_chunk_with_retry_using<F>(chunk: &SyncBatch, send_chunk: &F) -> Result<()>
where
    F: Fn(&SyncBatch) -> Result<()>,
{
    match send_chunk(chunk) {
        Ok(()) => Ok(()),
        Err(error) if should_retry_http_rollup_chunk_after_error(chunk, &error) => {
            let smaller_chunks = split_http_rollup_sync_batch_after_budget_error(chunk);
            if smaller_chunks.len() <= 1 {
                return Err(error);
            }
            eprintln!(
                "http rollup mode: {} rejected {}; retrying as {} smaller batches",
                http_rollup_retry_error_label(&error),
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

fn should_retry_http_rollup_chunk_after_error(chunk: &SyncBatch, error: &anyhow::Error) -> bool {
    if !(is_http_sync_error(error, 413, "sync_batch_d1_query_budget_exceeded")
        || is_http_sync_error(error, 413, "sync_batch_too_large"))
    {
        return false;
    }
    chunk.summaries.len() > 1
        || chunk.sources.len() > 1
        || chunk.accounts.len() > 1
        || chunk.source_account_assignments.len() > 1
        || chunk.subscriptions.len() > 1
        || chunk.task_buckets.len() > 1
        || chunk.task_verifications.len() > 1
        || (!chunk.task_buckets.is_empty() && !chunk.task_verifications.is_empty())
        || (http_rollup_metadata_count(chunk) > 0 && !chunk.summaries.is_empty())
}

fn http_rollup_retry_error_label(error: &anyhow::Error) -> &'static str {
    if is_http_sync_error(error, 413, "sync_batch_too_large") {
        "batch size"
    } else {
        "D1 budget"
    }
}

fn is_http_sync_error(error: &anyhow::Error, status: u16, error_code: &str) -> bool {
    parse_http_sync_error(error).is_some_and(|parsed| {
        parsed.status == status
            && parsed
                .body
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|value| value == error_code)
    })
}

#[derive(Debug)]
struct ParsedHttpSyncError {
    status: u16,
    body: Value,
}

fn parse_http_sync_error(error: &anyhow::Error) -> Option<ParsedHttpSyncError> {
    let message = error.to_string();
    let rest = message.strip_prefix("sync endpoint returned HTTP ")?;
    let (status_text, body_text) = rest.split_once(':')?;
    let status = status_text.trim().parse().ok()?;
    let body = serde_json::from_str(body_text.trim()).ok()?;
    Some(ParsedHttpSyncError { status, body })
}

fn split_http_rollup_sync_batches(batch: &SyncBatch) -> Vec<SyncBatch> {
    let mut data_batch = batch.clone();
    let authoritative_snapshot = data_batch.authoritative_snapshot.take();
    let mut chunks = split_http_rollup_sync_batches_without_snapshot(&data_batch);
    if let Some(authoritative_snapshot) = authoritative_snapshot {
        for snapshot in split_authoritative_snapshot(
            authoritative_snapshot,
            &data_batch.batch_id,
            HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH,
        ) {
            let mut snapshot_chunk = empty_http_rollup_chunk(
                &data_batch,
                &format!("snapshot_{}", snapshot.part_index + 1),
            );
            snapshot_chunk.authoritative_snapshot = Some(snapshot);
            chunks.push(snapshot_chunk);
        }
    }
    chunks
}

fn split_authoritative_snapshot(
    snapshot: SyncAuthoritativeSnapshot,
    batch_id: &str,
    max_ids: usize,
) -> Vec<SyncAuthoritativeSnapshot> {
    debug_assert!(max_ids > 0);
    let snapshot_id = if snapshot.snapshot_id.trim().is_empty() {
        format!("{batch_id}_authoritative")
    } else {
        snapshot.snapshot_id
    };
    let empty_part = || SyncAuthoritativeSnapshot {
        snapshot_id: snapshot_id.clone(),
        part_index: 0,
        part_count: 1,
        source_ids: Vec::new(),
        provider_account_ids: Vec::new(),
        source_account_assignment_ids: Vec::new(),
        subscription_ids: Vec::new(),
        summary_ids: Vec::new(),
    };
    let mut parts = Vec::new();
    let mut current = empty_part();

    macro_rules! append_ids {
        ($ids:expr, $field:ident) => {
            for id in $ids {
                if authoritative_snapshot_id_count(&current) == max_ids {
                    parts.push(std::mem::replace(&mut current, empty_part()));
                }
                current.$field.push(id);
            }
        };
    }

    append_ids!(snapshot.source_ids, source_ids);
    append_ids!(snapshot.provider_account_ids, provider_account_ids);
    append_ids!(
        snapshot.source_account_assignment_ids,
        source_account_assignment_ids
    );
    append_ids!(snapshot.subscription_ids, subscription_ids);
    append_ids!(snapshot.summary_ids, summary_ids);
    if authoritative_snapshot_id_count(&current) > 0 || parts.is_empty() {
        parts.push(current);
    }
    let part_count = u32::try_from(parts.len()).expect("snapshot part count fits u32");
    for (index, part) in parts.iter_mut().enumerate() {
        part.part_index = u32::try_from(index).expect("snapshot part index fits u32");
        part.part_count = part_count;
    }
    parts
}

fn authoritative_snapshot_id_count(snapshot: &SyncAuthoritativeSnapshot) -> usize {
    snapshot.source_ids.len()
        + snapshot.provider_account_ids.len()
        + snapshot.source_account_assignment_ids.len()
        + snapshot.subscription_ids.len()
        + snapshot.summary_ids.len()
}

fn split_http_rollup_sync_batches_without_snapshot(batch: &SyncBatch) -> Vec<SyncBatch> {
    let task_chunks = split_http_rollup_task_chunks(
        batch,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
    );
    let has_task_payload = !task_chunks.is_empty();
    let metadata_count = http_rollup_metadata_count(batch);
    let has_rollup_payload = metadata_count > 0 || !batch.summaries.is_empty();
    if !has_task_payload
        && batch.summaries.len() <= HTTP_ROLLUP_SUMMARIES_PER_BATCH
        && metadata_count <= HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH
    {
        return fit_http_rollup_batches_to_d1_budget(vec![batch.clone()]);
    }
    if has_task_payload && !has_rollup_payload {
        return fit_http_rollup_batches_to_d1_budget(task_chunks);
    }

    let total_chunks = batch
        .summaries
        .len()
        .div_ceil(HTTP_ROLLUP_SUMMARIES_PER_BATCH);
    let metadata_chunks = metadata_count.div_ceil(HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH);
    let mut chunks = Vec::with_capacity(total_chunks + metadata_chunks + task_chunks.len());

    chunks.extend(split_http_rollup_metadata_chunks(
        batch,
        HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH,
    ));
    chunks.extend(task_chunks);
    chunks.extend(split_http_rollup_summary_chunks(
        batch,
        HTTP_ROLLUP_SUMMARIES_PER_BATCH,
    ));

    fit_http_rollup_batches_to_d1_budget(chunks)
}

fn split_http_rollup_sync_batch_after_budget_error(batch: &SyncBatch) -> Vec<SyncBatch> {
    if !batch.task_buckets.is_empty() || !batch.task_verifications.is_empty() {
        if !batch.task_buckets.is_empty() && !batch.task_verifications.is_empty() {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len(),
                batch.task_verifications.len(),
            );
        }
        if batch.task_buckets.len() > 1 {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len().div_ceil(2),
                batch.task_verifications.len().max(1),
            );
        }
        if batch.task_verifications.len() > 1 {
            return split_http_rollup_task_chunks(
                batch,
                batch.task_buckets.len().max(1),
                batch.task_verifications.len().div_ceil(2),
            );
        }
    }

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

fn split_http_rollup_task_chunks(
    batch: &SyncBatch,
    task_bucket_chunk_size: usize,
    task_verification_chunk_size: usize,
) -> Vec<SyncBatch> {
    let mut chunks = Vec::new();
    let task_bucket_chunk_size = task_bucket_chunk_size.max(1);
    let task_verification_chunk_size = task_verification_chunk_size.max(1);

    chunks.extend(
        batch
            .task_buckets
            .chunks(task_bucket_chunk_size)
            .enumerate()
            .map(|(index, buckets)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("task_buckets_{}", index + 1));
                chunk.task_buckets = buckets.to_vec();
                chunk
            }),
    );
    chunks.extend(
        batch
            .task_verifications
            .chunks(task_verification_chunk_size)
            .enumerate()
            .map(|(index, verifications)| {
                let mut chunk =
                    empty_http_rollup_chunk(batch, &format!("task_verifications_{}", index + 1));
                chunk.task_verifications = verifications.to_vec();
                chunk
            }),
    );
    chunks
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
        + estimate_http_rollup_task_queries(batch)
        + final_sync_bookkeeping_queries
}

fn estimate_http_rollup_task_queries(batch: &SyncBatch) -> usize {
    let source_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let source_id = span.source_id.0.trim();
                (!source_id.is_empty()).then_some(source_id)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let project_lookup_queries = http_rollup_query_chunks(
        batch
            .task_buckets
            .iter()
            .flat_map(|bucket| bucket.spans.iter())
            .filter_map(|span| {
                let project = span.project.as_ref()?;
                Some(
                    [
                        Some(project.project_id.as_str()),
                        project.path_hash.as_deref(),
                        project.repo_remote_hash.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(":"),
                )
            })
            .filter(|descriptor| !descriptor.is_empty())
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let verification_span_lookup_queries = http_rollup_query_chunks(
        batch
            .task_verifications
            .iter()
            .flat_map(|verification| verification.action.span_ids())
            .filter_map(|span_id| {
                let value = span_id.0.trim();
                (!value.is_empty()).then_some(value)
            })
            .collect::<BTreeSet<_>>()
            .len(),
        HTTP_ROLLUP_D1_QUERY_CHUNK_SIZE,
    );
    let write_queries = batch
        .task_buckets
        .iter()
        .map(estimate_task_bucket_write_queries)
        .sum::<usize>()
        + batch.task_verifications.len();

    source_lookup_queries
        + project_lookup_queries
        + verification_span_lookup_queries
        + write_queries
}

fn estimate_task_bucket_write_queries(bucket: &TaskBucketSnapshot) -> usize {
    let member_work_item_ids_by_span_id = bucket
        .members
        .iter()
        .map(|member| (member.span_id.0.as_str(), member.work_item_id.0.as_str()))
        .collect::<HashMap<_, _>>();
    let spans_by_id = bucket
        .spans
        .iter()
        .map(|span| (span.span_id.0.as_str(), span))
        .collect::<HashMap<_, _>>();
    4 + count_task_sync_json_chunks(&bucket.spans, |span| {
        estimate_task_sync_span_insert_row_json(
            span,
            member_work_item_ids_by_span_id
                .get(span.span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.work_items, |work_item| {
        estimate_task_sync_work_item_insert_row_json(
            work_item,
            spans_by_id
                .get(work_item.anchor_span_id.0.as_str())
                .copied(),
        )
    }) + count_task_sync_json_chunks(&bucket.members, estimate_task_sync_member_insert_row_json)
}

fn count_task_sync_json_chunks<T>(rows: &[T], serialize_row: impl Fn(&T) -> String) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let mut chunk_count = 0usize;
    let mut current_row_count = 0usize;
    let mut current_bytes = 2usize;
    for row in rows {
        let serialized = serialize_row(row);
        let row_bytes = serialized.len();
        let next_bytes = if current_row_count == 0 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
        if current_row_count > 0
            && (current_row_count >= TASK_SYNC_SQL_MAX_ROWS_PER_CHUNK
                || next_bytes > TASK_SYNC_SQL_MAX_JSON_BYTES_PER_CHUNK)
        {
            chunk_count += 1;
            current_row_count = 0;
            current_bytes = 2;
        }
        current_row_count += 1;
        current_bytes = if current_row_count == 1 {
            2 + row_bytes
        } else {
            current_bytes + 1 + row_bytes
        };
    }
    chunk_count + usize::from(current_row_count > 0)
}

fn estimate_task_sync_project_fields(span: &TaskSpan) -> (Option<&str>, Option<String>) {
    let Some(project) = span.project.as_ref() else {
        return (None, None);
    };
    let project_location_id = [
        project.path_hash.as_deref(),
        project.repo_remote_hash.as_deref(),
        Some(project.project_id.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(":");
    (
        Some(project.project_id.as_str()),
        (!project_location_id.is_empty()).then_some(project_location_id),
    )
}

fn estimate_task_sync_span_insert_row_json(span: &TaskSpan, work_item_id: Option<&str>) -> String {
    let (project_id, project_location_id) = estimate_task_sync_project_fields(span);
    json!({
        "span_id": span.span_id.0,
        "work_item_id": work_item_id,
        "provider": span.provider,
        "provider_account_id": Value::Null,
        "project_id": project_id,
        "project_location_id": project_location_id,
        "started_at": span.started_at.to_rfc3339(),
        "ended_at": span.ended_at.map(|timestamp| timestamp.to_rfc3339()),
        "payload_json": serde_json::to_string(span).unwrap_or_default(),
    })
    .to_string()
}

fn estimate_task_sync_work_item_insert_row_json(
    work_item: &WorkItem,
    anchor_span: Option<&TaskSpan>,
) -> String {
    let (project_id, project_location_id) = anchor_span
        .map(estimate_task_sync_project_fields)
        .unwrap_or((None, None));
    json!({
        "work_item_id": work_item.work_item_id.0,
        "anchor_span_id": work_item.anchor_span_id.0,
        "status": work_item.status,
        "confidence": work_item.confidence,
        "started_at": work_item.started_at.to_rfc3339(),
        "ended_at": work_item.ended_at.to_rfc3339(),
        "project_id": project_id,
        "project_location_id": project_location_id,
        "payload_json": serde_json::to_string(work_item).unwrap_or_default(),
    })
    .to_string()
}

fn estimate_task_sync_member_insert_row_json(member: &statsai_core::WorkItemMember) -> String {
    json!({
        "work_item_id": member.work_item_id.0,
        "span_id": member.span_id.0,
        "ordinal": member.ordinal,
    })
    .to_string()
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
    if !project_has_stable_identity(project) {
        return None;
    }
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
    if !project_has_stable_identity(project) {
        return None;
    }
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
            chunk.task_buckets.clear();
            chunk.task_verifications.clear();
            chunk.authoritative_snapshot = None;
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
    chunk.task_buckets.clear();
    chunk.task_verifications.clear();
    chunk.authoritative_snapshot = None;
    chunk
}

fn sync_status(store: &Store, device_id: &str) -> Result<()> {
    let sync_preferences = store.sync_preferences()?;
    println!(
        "preferences projects={} tasks={}",
        if sync_preferences.include_projects {
            "enabled"
        } else {
            "disabled"
        },
        if sync_preferences.include_tasks {
            "enabled"
        } else {
            "disabled"
        }
    );
    let states = store.list_sync_states()?;
    if states.is_empty() {
        println!("no sync state recorded");
        return Ok(());
    }
    for state in states {
        let display_batch_id = logical_http_rollup_batch_id(&state.last_batch_id);
        let task_bucket_status =
            store.task_bucket_sync_status(&state.sink, &state.target, device_id)?;
        println!(
            "{} target={} last_success={} batch={} event_cursor={} summary_cursor={} task_verification_cursor={} task_bucket_backlog={}/{} failures={}",
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
            format_cursor(
                state
                    .last_task_verification_updated_at
                    .as_ref()
                    .map(DateTime::to_rfc3339),
                state.last_task_verification_id.as_deref()
            ),
            task_bucket_status.dirty,
            task_bucket_status.total,
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
    let sync_preferences = effective_sync_preferences(store, &command)?;
    let auth_token = resolve_http_auth_token(&command, true)?
        .context("device login required; run `statsai auth login` first")?;
    let report = HttpVerifyReport {
        sink: command.sink,
        target: endpoint.clone(),
        endpoint: endpoint.clone(),
        device_id: device_id.to_string(),
        local: sync_local_verify(
            store,
            "http",
            &endpoint,
            local_state.as_ref(),
            sync_preferences.include_projects,
        )?,
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
    include_projects: bool,
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
        .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
        .filter(is_http_rollup_passthrough_summary)
        .collect();
    let rollup_summaries: Vec<_> = store
        .all_sync_rollup_summaries()?
        .into_iter()
        .map(|summary| sanitize_summary_for_sync_with_projects(summary, include_projects))
        .collect();

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

#[derive(Debug, Clone, Deserialize)]
struct TaskVerificationFeedResponse {
    #[serde(default)]
    verifications: Vec<TaskVerification>,
    next_cursor: Option<TaskVerificationCursor>,
}

fn http_remote_verify(endpoint: &str, auth_token: &str) -> Result<Value> {
    validate_authenticated_http_endpoint(endpoint)?;
    let url = http_verify_status_url(endpoint)?;
    let request = ureq::get(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    match request.call() {
        Ok(response) => http_response_json(response, "verify sync status"),
        Err(error) => Err(http_request_error("verify sync status", error)),
    }
}

fn http_remote_preflight_status(endpoint: &str, auth_token: &str) -> Result<Value> {
    validate_authenticated_http_endpoint(endpoint)?;
    let url = http_preflight_status_url(endpoint)?;
    let request = ureq::get(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    match request.call() {
        Ok(response) => http_response_json(response, "load sync preflight status"),
        Err(error) => Err(http_request_error("load sync preflight status", error)),
    }
}

fn http_remote_reset(endpoint: &str, auth_token: &str) -> Result<Value> {
    validate_authenticated_http_endpoint(endpoint)?;
    let url = http_reset_url(endpoint)?;
    let body = serde_json::to_string(&json!({
        "confirm": "reset_synced_data",
        "scope": "device_mirror",
    }))?;
    let request = ureq::post(&url)
        .timeout(HTTP_REQUEST_TIMEOUT)
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

fn http_preflight_status_url(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Ok(format!("{prefix}/api/sync/status?view=preflight"));
    }
    bail!(
        "http preflight expects a Cloudflare sync endpoint ending in /api/sync/batches; got {}",
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

fn http_task_verification_feed_url(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim_end_matches('/');
    if let Some(prefix) = endpoint.strip_suffix("/api/sync/batches") {
        return Some(format!("{prefix}/api/task-sync/verifications"));
    }
    None
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

fn http_remote_hosted_tasks_enabled(command: &SyncCommand, endpoint: &str) -> Result<bool> {
    let Some(preflight_url) = http_preflight_status_url(endpoint).ok() else {
        return Ok(true);
    };
    let Some(auth_token) = resolve_http_auth_token(command, false)? else {
        return Ok(true);
    };
    validate_authenticated_http_endpoint(endpoint)?;
    let request = ureq::get(&preflight_url)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .set("Authorization", &format!("Bearer {auth_token}"));
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) if optional_http_sync_preflight_status(code) => {
            return Ok(true);
        }
        Err(error) => return Err(http_request_error("load sync preflight status", error)),
    };
    let remote = http_response_json(response, "load sync preflight status")?;
    Ok(remote_hosted_tasks_enabled(&remote))
}

fn remote_hosted_tasks_enabled(remote: &Value) -> bool {
    remote
        .pointer("/capabilities/hostedTasks")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn optional_http_sync_preflight_status(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
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
        "_task_buckets_",
        "_task_verifications_",
        "_snapshot_",
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
    if local_pending > 0 {
        return;
    }
    if let Some(remote_count) = remote_count {
        if remote_count != local_total as u64 {
            reasons.push(format!("{label} {remote_count}!={local_total}"));
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
    let auth_token = statsai::default_daemon_auth_token()?;
    if command.watch {
        statsai_daemon::watch_and_serve(&command.api, store, device_id, &auth_token)
    } else {
        statsai_daemon::run(&command.api, store, &auth_token)
    }
}

fn conversation(command: ConversationCommand, store: &Store) -> Result<()> {
    match command.command {
        ConversationSubcommand::Collect {
            provider,
            no_cache,
            verbose,
        } => collect_conversations(store, provider.as_deref(), no_cache, verbose),
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
    provider_filter: Option<&str>,
    no_cache: bool,
    verbose: bool,
) -> Result<()> {
    let canonical_provider_filter = canonical_conversation_provider_filter(provider_filter)?;
    let configured_sources = store.list_sources()?;
    let mut sources_collected = 0u64;
    let mut total_conversations = 0u64;
    let mut total_items = 0u64;
    let mut total_parts = 0u64;
    let mut total_binary_bytes = 0u64;
    let mut total_missing = 0u64;

    for adapter in default_adapters() {
        if canonical_provider_filter.is_some_and(|provider| provider != adapter.provider()) {
            continue;
        }
        for source in scan_sources_for_adapter(adapter.as_ref(), &configured_sources) {
            let candidates = adapter.archive_scan_candidates(&source)?;
            let entries = scan_file_state_entries(&candidates);
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
            sources_collected += 1;
            total_conversations += collected.conversations;
            total_items += collected.items;
            total_parts += collected.parts;
            total_binary_bytes += collected.binary_bytes;
            total_missing += collected.missing;
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
    println!(
        "archive collection: sources={} conversations={} items={} parts={} binary_bytes={} missing={}",
        sources_collected,
        total_conversations,
        total_items,
        total_parts,
        total_binary_bytes,
        total_missing,
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ArchiveSourceCollection {
    files: u64,
    conversations: u64,
    items: u64,
    parts: u64,
    binary_bytes: u64,
    missing: u64,
    invalid_records: u64,
}

fn collect_archive_source_entries(
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
    let mut collected = ArchiveSourceCollection::default();
    for (index, entry) in pending.iter().enumerate() {
        let candidate = candidates_by_key.get(entry.cache_key.as_str()).copied();
        let candidate_bytes = candidate
            .and_then(|candidate| std::fs::metadata(&candidate.path).ok())
            .map_or(0, |metadata| metadata.len());
        let report_candidate =
            verbose && (index == 0 || (index + 1) % 25 == 0 || candidate_bytes >= 16 * 1024 * 1024);
        if report_candidate {
            println!(
                "{} {}: collecting file {}/{} ({} bytes) {}",
                adapter.provider(),
                preview_path_label(source),
                index + 1,
                pending.len(),
                candidate_bytes,
                candidate
                    .and_then(|candidate| candidate.path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or(entry.cache_key.as_str()),
            );
        }

        let collect_started = Instant::now();
        let selected = HashSet::from([entry.cache_key.clone()]);
        let scan = adapter.collect_archive(source, Some(&selected))?;
        let collect_elapsed = collect_started.elapsed();
        let store_started = Instant::now();
        let write = store.store_archive_scan(
            &source.source_id,
            &scan.conversations,
            std::slice::from_ref(entry),
            &scan.artifact_dependencies,
        )?;
        let store_elapsed = store_started.elapsed();
        collected.files += scan.diagnostics.files_scanned;
        collected.conversations += write.conversations;
        collected.items += write.items;
        collected.parts += write.content_parts;
        collected.binary_bytes += write.binary_bytes;
        collected.missing += scan.diagnostics.missing_content;
        collected.invalid_records += scan.diagnostics.invalid_records;
        if report_candidate {
            println!(
                "{} {}: completed file {}/{} collect={:.1}s store={:.1}s",
                adapter.provider(),
                preview_path_label(source),
                index + 1,
                pending.len(),
                collect_elapsed.as_secs_f64(),
                store_elapsed.as_secs_f64(),
            );
        }
    }
    Ok(collected)
}

fn canonical_conversation_provider_filter(provider: Option<&str>) -> Result<Option<&'static str>> {
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

fn status(store: &Store) -> Result<()> {
    println!("stored all-time events: {}", store.event_count()?);
    println!("stored all-time tokens: {}", store.token_total()?);
    println!("stored usage summaries: {}", store.summary_count()?);
    let archive = store.archive_stats()?;
    println!("archived conversations: {}", archive.conversations);
    println!("archived conversation items: {}", archive.items);
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

#[derive(Debug, Clone)]
struct ReportedImportRecord {
    record: ReportedUsageSummaryRecord,
    legacy_replacement_source_ids: Vec<SourceId>,
}

#[derive(Debug, Clone)]
struct ReportedImportReport {
    path: PathBuf,
    records: Vec<ReportedImportRecord>,
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

fn normalize_configured_source_path(provider: &str, path: &Path) -> Result<PathBuf> {
    let mut path = expand_cli_path(path)?;
    if provider_matches(provider, "claude_code")
        && path.file_name().is_some_and(|name| name == "projects")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "codex")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "opencode")
        && path.file_name().is_some_and(|name| name == "opencode.db")
    {
        if let Some(parent) = path.parent() {
            path = parent.to_path_buf();
        }
    }
    if provider_matches(provider, "grok_build")
        && path.file_name().is_some_and(|name| name == "sessions")
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

struct ScanPreviewLine<'a> {
    source: &'a SourceLocation,
    usage_events: u64,
    usage: &'a UsageTotals,
    summaries: u64,
    task_spans: u64,
    summary_usage: &'a UsageTotals,
    diagnostics: &'a ScanDiagnostics,
    verbose: bool,
}

fn print_scan_preview_line(line: ScanPreviewLine<'_>) {
    if line.verbose {
        println!(
            "{} path={} usage_events={} summaries={} task_spans={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={} raw_rows={} candidates={} duplicates={} skipped_zero={} invalid={} files={} cached={} timestamp_fallbacks={} model_fallbacks={} origin={} source={}",
            line.source.provider,
            preview_path_label(line.source),
            line.usage_events,
            line.summaries,
            line.task_spans,
            format_u64(line.usage.input_tokens),
            format_u64(line.usage.cache_creation_tokens),
            format_u64(line.usage.cached_input_tokens),
            format_u64(line.usage.output_tokens),
            format_u64(line.usage.total_tokens),
            format_cost(line.usage.estimated_cost_usd),
            format_u64(line.summary_usage.total_tokens),
            format_cost(line.summary_usage.estimated_cost_usd),
            format_u64(line.diagnostics.raw_rows),
            format_u64(line.diagnostics.candidate_usage_rows),
            format_u64(line.diagnostics.duplicate_events),
            format_u64(line.diagnostics.skipped_zero_events),
            format_u64(line.diagnostics.invalid_rows),
            format_u64(line.diagnostics.files_scanned),
            format_u64(line.diagnostics.files_skipped_unchanged),
            format_u64(line.diagnostics.timestamp_fallbacks),
            format_u64(line.diagnostics.model_fallbacks),
            location_origin_label(&line.source.location_origin),
            line.source.source_id.0
        );
    } else {
        println!(
            "{} path={} usage_events={} summaries={} task_spans={} input={} cache_create={} cache_read={} output={} total={} est_cost={} summary_total={} summary_est_cost={}",
            line.source.provider,
            preview_path_label(line.source),
            line.usage_events,
            line.summaries,
            line.task_spans,
            format_u64(line.usage.input_tokens),
            format_u64(line.usage.cache_creation_tokens),
            format_u64(line.usage.cached_input_tokens),
            format_u64(line.usage.output_tokens),
            format_u64(line.usage.total_tokens),
            format_cost(line.usage.estimated_cost_usd),
            format_u64(line.summary_usage.total_tokens),
            format_cost(line.summary_usage.estimated_cost_usd)
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

fn scan_candidate_compatible_signatures(
    candidates: &[ScanCandidateFile],
) -> HashMap<String, Vec<String>> {
    candidates
        .iter()
        .filter(|candidate| !candidate.compatible_cache_signatures.is_empty())
        .map(|candidate| {
            (
                candidate.cache_key.clone(),
                candidate.compatible_cache_signatures.clone(),
            )
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanFileReconciliation {
    pending_entries: Vec<ScanFileStateEntry>,
    compatible_entries_to_upgrade: Vec<ScanFileStateEntry>,
    removed_entries: Vec<ScanFileStateEntry>,
}

fn select_scan_file_reconciliation(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<ScanFileReconciliation> {
    let selection = select_scan_file_state_entries_with_task_requirement_and_compatibility(
        store,
        source_id,
        file_cache_entries,
        compatible_signatures_by_key,
        replace,
        no_cache,
        require_tasks_collected,
    )?;
    let tracked_entries = store.scan_file_entries(source_id)?;
    let current_cache_keys: BTreeSet<_> = file_cache_entries
        .iter()
        .map(|entry| entry.cache_key.as_str())
        .collect();
    let removed_entries = tracked_entries
        .into_iter()
        .filter(|entry| !current_cache_keys.contains(entry.cache_key.as_str()))
        .collect();
    Ok(ScanFileReconciliation {
        pending_entries: selection.pending_entries,
        compatible_entries_to_upgrade: selection.compatible_entries_to_upgrade,
        removed_entries,
    })
}

fn select_scan_file_state_entries_with_task_requirement_and_compatibility(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<statsai_store::ScanFileStateSelection> {
    if replace || no_cache {
        return Ok(statsai_store::ScanFileStateSelection {
            pending_entries: file_cache_entries.to_vec(),
            compatible_entries_to_upgrade: Vec::new(),
        });
    }
    store.select_scan_file_state_entries_with_task_requirement_and_compatibility(
        source_id,
        file_cache_entries,
        require_tasks_collected,
        compatible_signatures_by_key,
    )
}

#[cfg(test)]
fn select_scan_file_entries(
    store: &Store,
    source_id: &statsai_core::SourceId,
    file_cache_entries: &[ScanFileStateEntry],
    compatible_signatures_by_key: &HashMap<String, Vec<String>>,
    replace: bool,
    no_cache: bool,
    require_tasks_collected: bool,
) -> Result<Vec<ScanFileStateEntry>> {
    Ok(
        select_scan_file_state_entries_with_task_requirement_and_compatibility(
            store,
            source_id,
            file_cache_entries,
            compatible_signatures_by_key,
            replace,
            no_cache,
            require_tasks_collected,
        )?
        .pending_entries,
    )
}

fn should_replace_source_records_for_scan(
    explicit_replace: bool,
    no_cache: bool,
    candidate_count: usize,
    pending_count: usize,
    legacy_full_reconcile: bool,
) -> bool {
    explicit_replace
        || no_cache
        || legacy_full_reconcile
        || (candidate_count > 0 && pending_count == candidate_count)
}

fn scan_file_hashes_for_reconciliation(
    pending_entries: &[ScanFileStateEntry],
    removed_entries: &[ScanFileStateEntry],
) -> Vec<String> {
    pending_entries
        .iter()
        .chain(removed_entries.iter())
        .map(|entry| hash_text(&entry.cache_key))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn scan_file_cache_keys(entries: &[ScanFileStateEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.cache_key.clone())
        .collect()
}

fn rewrite_task_span_linked_event_ids(
    task_spans: &mut [TaskSpan],
    canonical_event_ids: &HashMap<EventId, EventId>,
) {
    for span in task_spans {
        if span.linked_event_ids.is_empty() {
            continue;
        }
        let mut rewritten = Vec::with_capacity(span.linked_event_ids.len());
        let mut seen = HashSet::new();
        for event_id in &span.linked_event_ids {
            let canonical = canonical_event_ids
                .get(event_id)
                .cloned()
                .unwrap_or_else(|| event_id.clone());
            if seen.insert(canonical.clone()) {
                rewritten.push(canonical);
            }
        }
        span.linked_event_ids = rewritten;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TaskSpanRuntimeRollup {
    total_messages: u64,
    user_messages: u64,
    assistant_messages: u64,
    developer_messages: u64,
}

fn populate_task_span_rollups(
    task_spans: &mut [TaskSpan],
    events: &[UsageEvent],
    canonical_event_ids: &HashMap<EventId, EventId>,
) {
    let mut event_rollups = HashMap::<String, TaskSpanRuntimeRollup>::new();
    for event in events {
        let canonical_event_id = canonical_event_ids
            .get(&event.event_id)
            .unwrap_or(&event.event_id)
            .0
            .clone();
        event_rollups
            .entry(canonical_event_id)
            .or_insert_with(|| task_span_runtime_rollup(event));
    }

    for span in task_spans {
        let mut total_messages = 0u64;
        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut developer_messages = 0u64;
        let mut seen_event_ids = HashSet::<String>::new();
        for event_id in &span.linked_event_ids {
            if !seen_event_ids.insert(event_id.0.clone()) {
                continue;
            }
            let Some(rollup) = event_rollups.get(&event_id.0) else {
                continue;
            };
            total_messages = total_messages.saturating_add(rollup.total_messages);
            user_messages = user_messages.saturating_add(rollup.user_messages);
            assistant_messages = assistant_messages.saturating_add(rollup.assistant_messages);
            developer_messages = developer_messages.saturating_add(rollup.developer_messages);
        }
        span.event_count = span.event_count.max(seen_event_ids.len() as u64);
        span.has_usage_evidence = span.has_usage_evidence || span.event_count > 0;
        span.total_messages = span.total_messages.max(total_messages);
        span.user_messages = span.user_messages.max(user_messages);
        span.assistant_messages = span.assistant_messages.max(assistant_messages);
        span.developer_messages = span.developer_messages.max(developer_messages);
    }
}

fn task_span_runtime_rollup(event: &UsageEvent) -> TaskSpanRuntimeRollup {
    let Some(runtime) = event.runtime.as_ref() else {
        return TaskSpanRuntimeRollup::default();
    };
    let user_messages = runtime.user_messages.unwrap_or(0);
    let assistant_messages = runtime.assistant_messages.unwrap_or(0);
    let developer_messages = runtime.developer_messages.unwrap_or(0);
    let total_messages = runtime.total_messages.unwrap_or_else(|| {
        user_messages
            .saturating_add(assistant_messages)
            .saturating_add(developer_messages)
    });
    TaskSpanRuntimeRollup {
        total_messages,
        user_messages,
        assistant_messages,
        developer_messages,
    }
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
    let configured_sources = configured_sources
        .iter()
        .filter(|source| {
            provider_matches(&source.provider, adapter.provider())
                && source.source_kind == SourceKind::LocalAdapter
        })
        .cloned()
        .map(|mut source| {
            if source.path_label.is_none() {
                source.path_label = path_label_from_hashless_source(&source);
            }
            source
        })
        .collect::<Vec<_>>();
    let mut sources = BTreeMap::new();
    for mut source in adapter.discover() {
        if source.path_label.is_none() {
            source.path_label = path_label_from_hashless_source(&source);
        }
        if configured_sources
            .iter()
            .any(|configured| sources_refer_to_same_location(&source, configured))
        {
            continue;
        }
        sources.insert(source.source_id.0.clone(), source);
    }
    for source in configured_sources
        .into_iter()
        .filter(|source| source.enabled)
    {
        sources.insert(source.source_id.0.clone(), source);
    }
    dedupe_overlapping_sources(
        sources
            .into_values()
            .filter(|source| source.enabled)
            .collect(),
    )
}

fn sources_refer_to_same_location(left: &SourceLocation, right: &SourceLocation) -> bool {
    if left.source_kind != right.source_kind || !provider_matches(&left.provider, &right.provider) {
        return false;
    }
    if left.source_id == right.source_id
        || left
            .path_hash
            .as_deref()
            .zip(right.path_hash.as_deref())
            .is_some_and(|(left, right)| left == right)
    {
        return true;
    }
    match (comparable_source_path(left), comparable_source_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
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
                if !provider_shadowing_covers_nested_source(source, &source_path, &other_path) {
                    return false;
                }
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

fn provider_shadowing_covers_nested_source(
    source: &SourceLocation,
    source_path: &Path,
    other_path: &Path,
) -> bool {
    match canonical_provider_name(&source.provider) {
        Some("claude_code") => true,
        Some("codex") => codex_source_path_is_covered_by_parent(other_path, source_path),
        _ => false,
    }
}

fn codex_source_path_is_covered_by_parent(parent_path: &Path, child_path: &Path) -> bool {
    if parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "sessions" | "archived_sessions"))
    {
        return child_path.starts_with(parent_path);
    }

    child_path.starts_with(parent_path.join("sessions"))
        || child_path.starts_with(parent_path.join("archived_sessions"))
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
    event.project = event.project.and_then(sanitize_project_for_sync);
    if project_contains_file_paths(event.project.as_ref()) {
        event.privacy.contains_file_paths = true;
    }
    event
}

fn sanitize_event_for_sync_with_projects(event: UsageEvent, include_projects: bool) -> UsageEvent {
    let mut event = sanitize_event_for_sync(event);
    if !include_projects {
        event.project = None;
    }
    event
}

fn sanitize_project_for_sync(project: ProjectInfo) -> Option<ProjectInfo> {
    statsai_core::sanitize_project_for_sync(project)
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
    cache_creation_5m_tokens: u64,
    cache_creation_1h_tokens: u64,
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
                cache_creation_5m_tokens: 0,
                cache_creation_1h_tokens: 0,
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
        entry.cache_creation_5m_tokens = entry
            .cache_creation_5m_tokens
            .saturating_add(event.usage.cache_creation_5m_tokens.unwrap_or(0));
        entry.cache_creation_1h_tokens = entry
            .cache_creation_1h_tokens
            .saturating_add(event.usage.cache_creation_1h_tokens.unwrap_or(0));
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
                cache_creation_5m_tokens: Some(bucket.cache_creation_5m_tokens),
                cache_creation_1h_tokens: Some(bucket.cache_creation_1h_tokens),
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

fn sanitize_summary_for_sync(summary: UsageSummary) -> UsageSummary {
    statsai_core::sanitize_summary_for_sync(summary)
}

fn sanitize_summary_for_sync_with_projects(
    summary: UsageSummary,
    include_projects: bool,
) -> UsageSummary {
    let mut summary = sanitize_summary_for_sync(summary);
    if !include_projects {
        summary.project = None;
    }
    summary
}

fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = *end - *start;
    duration >= Duration::zero() && duration <= Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if is_daily_rollup_summary(summary) {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

fn sanitize_subscription_for_sync(mut subscription: Subscription) -> Subscription {
    subscription.notes = None;
    subscription
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone};
    use statsai_core::{
        branch_family, event_id, hash_text, normalize_task_title, project_bucket_key,
        subscription_id, summary_id, task_span_id, BillingPeriod, Confidence, CostInfo,
        EventSource, IdentitySource, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode,
        ProjectInfo, ProviderAccount, SessionInfo, SourceKind, Subscription, SubscriptionStatus,
        SummaryMetadata, TaskBucketSnapshot, TaskSpan, TaskSpanId, TaskStatus, TaskVerdict,
        TaskVerification, TaskVerificationAction, TaskVerificationId, UsageCounts, UsageSummary,
        WorkItem, WorkItemId, WorkItemMember, PROVIDER_ACCOUNT_SCHEMA_VERSION,
        SUBSCRIPTION_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION, TASK_VERIFICATION_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
    };
    use std::path::Path;

    #[derive(Clone)]
    struct TestAdapter {
        provider: &'static str,
        discovered: Vec<SourceLocation>,
        candidates: Vec<ScanCandidateFile>,
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
            Ok(self.candidates.clone())
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

    struct InterruptingArchiveAdapter;

    impl ProviderAdapter for InterruptingArchiveAdapter {
        fn id(&self) -> &'static str {
            "interrupting-archive-test"
        }

        fn version(&self) -> &'static str {
            "0"
        }

        fn provider(&self) -> &'static str {
            "archive_test"
        }

        fn discover(&self) -> Vec<SourceLocation> {
            Vec::new()
        }

        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            Ok(statsai_adapters::AdapterScan::default())
        }

        fn collect_archive(
            &self,
            _source: &SourceLocation,
            selected_cache_keys: Option<&HashSet<String>>,
        ) -> Result<statsai_adapters::ArchiveScan> {
            let selected = selected_cache_keys
                .and_then(|keys| keys.iter().next())
                .context("selected archive cache key")?;
            if selected == "second" {
                bail!("synthetic archive interruption");
            }
            let mut scan = statsai_adapters::ArchiveScan::default();
            scan.diagnostics.files_scanned = 1;
            Ok(scan)
        }
    }

    #[test]
    fn archive_collection_commits_each_candidate_before_the_next() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "archive_test",
            "interrupting-archive-test",
            "0",
            Path::new("/tmp/archive-test"),
            LocationOrigin::Configured,
        );
        let candidates = [
            ScanCandidateFile {
                path: PathBuf::from("first"),
                cache_key: "first".to_string(),
                cache_signature: "signature-first".to_string(),
                compatible_cache_signatures: Vec::new(),
            },
            ScanCandidateFile {
                path: PathBuf::from("second"),
                cache_key: "second".to_string(),
                cache_signature: "signature-second".to_string(),
                compatible_cache_signatures: Vec::new(),
            },
        ];
        let entries = scan_file_state_entries(&candidates);

        let result = collect_archive_source_entries(
            &store,
            &InterruptingArchiveAdapter,
            &source,
            &candidates,
            &entries,
            false,
        );
        assert!(result.is_err());

        let pending = store
            .pending_archive_import_entries(&source.source_id, &entries)
            .expect("pending archive entries");
        assert_eq!(pending, vec![entries[1].clone()]);
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
        assert_eq!(canonical_provider_name("claude-code"), Some("claude_code"));
        assert_eq!(canonical_provider_name("grok"), Some("grok_build"));
        assert_eq!(canonical_provider_name("open-code"), Some("opencode"));
        assert_eq!(
            canonical_conversation_provider_filter(Some("claude")).expect("archive provider"),
            Some("claude_code")
        );
        assert_eq!(
            canonical_conversation_provider_filter(Some("grok")).expect("archive provider"),
            Some("grok_build")
        );
        assert_eq!(
            canonical_conversation_provider_filter(Some("open-code")).expect("archive provider"),
            Some("opencode")
        );
        assert!(canonical_conversation_provider_filter(Some("unknown")).is_err());
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
        event.project = Some(ProjectInfo {
            project_id: "project-event-path-only".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("event-path-hash".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
        });
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
        summary.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });

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
        let event_project = event.project.expect("path-only event project");
        assert_eq!(
            event_project.path_label.as_deref(),
            Some("/Users/example/Documents/Codex/2026-05-29/hi")
        );
        assert!(event.privacy.contains_file_paths);

        assert!(summary.source.source_record_id.is_none());
        let summary_evidence = summary.parse_evidence.expect("summary evidence");
        assert!(summary_evidence.source_record_id.is_none());
        assert!(summary_evidence.source_line_number.is_none());
        assert_eq!(
            summary_evidence.source_file_path_hash.as_deref(),
            Some("hash")
        );
        let project = summary.project.expect("repo-backed project");
        assert_eq!(project.repo_remote_hash.as_deref(), Some("repo-hash"));
        assert_eq!(project.repo_label.as_deref(), Some("owner/repo"));
        assert_eq!(project.path_hash.as_deref(), Some("path-hash"));
        assert_eq!(
            project.path_label.as_deref(),
            Some("/Users/example/work/ai-stats")
        );
        assert!(summary.privacy.contains_file_paths);
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
            records: vec![ReportedImportRecord {
                record,
                legacy_replacement_source_ids: Vec::new(),
            }],
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
            records: vec![ReportedImportRecord {
                record: incoming,
                legacy_replacement_source_ids: Vec::new(),
            }],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![matching.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_matches_legacy_alias_formats_after_canonicalization() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: Some("screenshot:2025-07-11".to_string()),
            evidence_path: Some("/tmp/user-report.png".to_string()),
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let mut legacy = incoming.record.summary.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        store
            .upsert_source(&incoming.record.source)
            .expect("source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming.clone()],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![legacy.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_matches_legacy_alias_formats_without_evidence() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: None,
            evidence_path: None,
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let mut legacy = incoming.record.summary.clone();
        let legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:user_reported_usage:acct-personal:ccusage_daily",
            None,
        );
        assert_ne!(legacy_source.source_id, incoming.record.source.source_id);
        legacy.source_id = legacy_source.source_id.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        legacy.source.source_path_hash = legacy_source.path_hash.clone();
        store.upsert_source(&legacy_source).expect("legacy source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let other_legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:other_report:acct-personal:ccusage_daily",
            None,
        );
        let mut other_legacy = legacy.clone();
        other_legacy.summary_id = summary_id(
            "claude_code",
            &other_legacy_source.source_id,
            "other-source-same-period",
        );
        other_legacy.source_id = other_legacy_source.source_id.clone();
        other_legacy.source.source_path_hash = other_legacy_source.path_hash.clone();
        store
            .upsert_source(&other_legacy_source)
            .expect("other legacy source");
        store
            .upsert_summary(&other_legacy)
            .expect("other legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming.clone()],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![legacy.summary_id]);
        store
            .delete_summaries(&matches)
            .expect("delete legacy summary");
        assert_eq!(
            delete_orphaned_legacy_reported_sources(
                &store,
                &[ReportedImportReport {
                    path: PathBuf::from("reported-file-a.json"),
                    records: vec![incoming],
                    warnings: Vec::new(),
                }]
            )
            .expect("delete legacy source"),
            1
        );
        assert!(store
            .source(&legacy_source.source_id)
            .expect("legacy source")
            .is_none());
        assert!(store
            .source(&other_legacy_source.source_id)
            .expect("other legacy source")
            .is_some());
    }

    #[test]
    fn import_migrates_legacy_alias_summary_without_replace() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: None,
            evidence_path: None,
            report_format: "manual_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let canonical_source = incoming.record.source.clone();
        let canonical_source_id = incoming.record.source.source_id.clone();
        let canonical_summary_id = incoming.record.summary.summary_id.clone();
        let legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:user_reported_usage:acct-personal:ccusage_daily",
            None,
        );
        let mut legacy = incoming.record.summary.clone();
        legacy.summary_id = summary_id("claude_code", &legacy_source.source_id, "legacy-alias");
        legacy.source_id = legacy_source.source_id.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        legacy.source.source_path_hash = legacy_source.path_hash.clone();
        let provider_account_id = ProviderAccountId("acct-personal".to_string());
        let legacy_assignment = test_assignment(
            &legacy_source,
            &provider_account_id,
            Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0)
                .single()
                .expect("assignment start"),
            None,
            Utc.with_ymd_and_hms(2025, 7, 12, 0, 0, 0)
                .single()
                .expect("assignment updated"),
        );
        let mut existing_canonical_assignment = test_assignment(
            &canonical_source,
            &provider_account_id,
            legacy_assignment.started_at,
            Some(
                Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0)
                    .single()
                    .expect("canonical assignment end"),
            ),
            Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
                .single()
                .expect("canonical assignment updated"),
        );
        existing_canonical_assignment.record_source = IdentitySource::SourceConfig;
        existing_canonical_assignment.verified_at = Some(
            Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
                .single()
                .expect("canonical assignment verified"),
        );
        store
            .upsert_source(&canonical_source)
            .expect("canonical source");
        store
            .upsert_source_account_assignment(&existing_canonical_assignment)
            .expect("canonical assignment");
        store.upsert_source(&legacy_source).expect("legacy source");
        store.upsert_summary(&legacy).expect("legacy summary");
        store
            .upsert_source_account_assignment(&legacy_assignment)
            .expect("legacy assignment");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        };

        import_reported_summary_records(&store, &[report], false, false, false).expect("import");

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary_id, canonical_summary_id);
        assert_eq!(summaries[0].source_id, canonical_source_id);
        assert!(store
            .source(&legacy_source.source_id)
            .expect("legacy source")
            .is_none());
        assert!(store
            .source(&canonical_source.source_id)
            .expect("canonical source")
            .is_some());
        assert!(store
            .list_source_account_assignments_for_source(&legacy_source.source_id)
            .expect("legacy assignments")
            .is_empty());
        let canonical_assignments = store
            .list_source_account_assignments_for_source(&canonical_source.source_id)
            .expect("canonical assignments");
        assert_eq!(canonical_assignments.len(), 1);
        assert_eq!(
            canonical_assignments[0].provider_account_id,
            provider_account_id
        );
        assert_eq!(
            canonical_assignments[0].started_at,
            legacy_assignment.started_at
        );
        assert_eq!(
            canonical_assignments[0].ended_at,
            existing_canonical_assignment.ended_at
        );
        assert_eq!(
            canonical_assignments[0].record_source,
            existing_canonical_assignment.record_source
        );
        assert_eq!(
            canonical_assignments[0].verified_at,
            existing_canonical_assignment.verified_at
        );
    }

    #[test]
    fn import_migrates_evidence_backed_legacy_alias_summary_without_replace() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: Some("screenshot:2025-07-11".to_string()),
            evidence_path: Some("/tmp/user-report.png".to_string()),
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let canonical_summary_id = incoming.record.summary.summary_id.clone();
        let mut legacy = incoming.record.summary.clone();
        legacy.summary_id = summary_id(
            "claude_code",
            &incoming.record.source.source_id,
            "legacy-evidence-alias",
        );
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        store
            .upsert_source(&incoming.record.source)
            .expect("source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        };

        import_reported_summary_records(&store, &[report], false, false, false).expect("import");

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary_id, canonical_summary_id);
        assert_eq!(summaries[0].metadata.summary_format, "manual_daily");
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
    fn configured_codex_sessions_path_normalizes_to_codex_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");

        let normalized =
            normalize_configured_source_path("codex", &sessions).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn configured_opencode_db_path_normalizes_to_data_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("opencode.db");
        std::fs::write(&db, "").expect("db");

        let normalized =
            normalize_configured_source_path("opencode", &db).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn configured_grok_sessions_path_normalizes_to_home_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");

        let normalized =
            normalize_configured_source_path("grok-build", &sessions).expect("normalized path");

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
                    price: "20.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
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
    fn subscription_price_parses_exact_decimal_cents() {
        for (value, expected_cents) in [
            ("0", 0),
            ("20", 2_000),
            ("20.5", 2_050),
            ("20.05", 2_005),
            ("1000000.00", MAX_SUBSCRIPTION_PRICE_CENTS),
        ] {
            assert_eq!(
                value
                    .parse::<SubscriptionPrice>()
                    .expect("valid price")
                    .cents(),
                expected_cents
            );
        }
    }

    #[test]
    fn subscription_price_rejects_invalid_or_excessive_values() {
        for value in [
            "",
            "-1",
            "+1",
            "NaN",
            "inf",
            "1e3",
            ".50",
            "1.",
            "1.001",
            "1000000.01",
            "999999999999999999999999999999999999999999",
        ] {
            assert!(
                value.parse::<SubscriptionPrice>().is_err(),
                "price should be rejected: {value}"
            );
        }
    }

    #[test]
    fn subscription_currency_normalizes_three_letter_codes() {
        assert_eq!(
            "usd"
                .parse::<CurrencyCode>()
                .expect("currency")
                .into_string(),
            "USD"
        );
        for value in ["", "US", "USDD", "U1D", "💵"] {
            assert!(
                value.parse::<CurrencyCode>().is_err(),
                "currency should be rejected: {value}"
            );
        }
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
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
    }

    #[test]
    fn disabled_configured_source_suppresses_matching_discovered_source() {
        let matching = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-disabled"),
            LocationOrigin::Default,
        );
        let unrelated = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-enabled"),
            LocationOrigin::Default,
        );
        let mut disabled = SourceLocation::local_adapter(
            "claude",
            "test",
            "0",
            Path::new("/tmp/claude-disabled"),
            LocationOrigin::Configured,
        );
        disabled.enabled = false;
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![matching, unrelated.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[disabled]);

        assert_eq!(sources, vec![unrelated]);
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
            candidates: Vec::new(),
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
    fn codex_nested_source_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/.codex"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources =
            scan_sources_for_adapter(&adapter, std::slice::from_ref(&configured_child));
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/.codex")
        );
    }

    #[test]
    fn codex_nested_sessions_source_is_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/sessions"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured_child]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    }

    #[test]
    fn codex_source_under_nested_codex_root_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/.codex/sessions"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/.codex/sessions")
        );
    }

    #[test]
    fn codex_custom_named_nested_root_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/project-codex-home"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/project-codex-home")
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
            candidates: Vec::new(),
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
                    price: "200.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
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
            candidates: Vec::new(),
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
                include_tasks: false,
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
    fn scan_skips_files_when_legacy_codex_auth_signature_is_cached() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-legacy-auth-cache"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-legacy-auth-cache/session.jsonl";
        let current_candidate = ScanCandidateFile {
            path: PathBuf::from(file_path),
            cache_key: file_path.to_string(),
            cache_signature: "sig-current".to_string(),
            compatible_cache_signatures: vec!["sig-legacy-auth".to_string()],
        };
        store
            .record_scan_file_entries(
                &source.source_id,
                &[ScanFileStateEntry {
                    cache_key: current_candidate.cache_key.clone(),
                    cache_signature: "sig-legacy-auth".to_string(),
                }],
            )
            .expect("record legacy scan cache");

        let scan_calls = Arc::new(Mutex::new(0u64));
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![current_candidate],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: Some(scan_calls.clone()),
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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

        let stored_entries = store
            .scan_file_entries(&source.source_id)
            .expect("stored scan file entries");
        assert_eq!(
            stored_entries,
            vec![ScanFileStateEntry {
                cache_key: file_path.to_string(),
                cache_signature: "sig-current".to_string(),
            }]
        );

        let second_scan_calls = Arc::new(Mutex::new(0u64));
        let rotated_legacy_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![ScanCandidateFile {
                path: PathBuf::from(file_path),
                cache_key: file_path.to_string(),
                cache_signature: "sig-current".to_string(),
                compatible_cache_signatures: vec!["sig-legacy-auth-rotated".to_string()],
            }],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: Some(second_scan_calls.clone()),
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(rotated_legacy_adapter)],
        )
        .expect("second scan");

        assert_eq!(*second_scan_calls.lock().expect("scan calls"), 0);
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
            candidates: Vec::new(),
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
                include_tasks: false,
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
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: Some(verified_state),
            scan_calls: Some(scan_calls.clone()),
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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
    fn scan_preserves_verified_assignment_when_auto_source_auth_is_unavailable() {
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
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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
        assert_eq!(assignments[0].ended_at, None);
        let stored_source = store
            .source(&source.source_id)
            .expect("source row")
            .expect("stored source");
        assert_eq!(
            stored_source.verified_state_hash,
            source.verified_state_hash
        );
    }

    #[test]
    fn scan_preserves_legacy_verified_assignment_when_auth_is_unavailable() {
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
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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
        assert_eq!(assignments[0].ended_at, None);
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
            candidates: Vec::new(),
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
                include_tasks: false,
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
    fn source_remove_delete_data_clears_task_spans_and_rebuilds_surviving_work_items() {
        let store = Store::in_memory().expect("store");
        let source_a = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-remove-a"),
            LocationOrigin::Configured,
        );
        let source_b = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-remove-b"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source_a).expect("source a");
        store.upsert_source(&source_b).expect("source b");

        let started_at_a = Utc
            .with_ymd_and_hms(2026, 6, 1, 10, 0, 0)
            .single()
            .expect("started_at_a");
        let started_at_b = started_at_a + Duration::days(10);
        let event_a = test_scan_event(
            &source_a,
            "/tmp/codex-source-remove-a/session.jsonl",
            started_at_a,
            "event-a",
            100,
        );
        let event_b = test_scan_event(
            &source_b,
            "/tmp/codex-source-remove-b/session.jsonl",
            started_at_b,
            "event-b",
            120,
        );
        store.insert_event(&event_a).expect("event a");
        store.insert_event(&event_b).expect("event b");

        let mut span_a = test_task_span(
            &source_a,
            "/tmp/codex-source-remove-a/session.jsonl",
            started_at_a,
            "span-a",
            "Implement source delete cleanup alpha",
            &event_a,
        );
        span_a.session_id = Some("session-a".to_string());
        let mut span_b = test_task_span(
            &source_b,
            "/tmp/codex-source-remove-b/session.jsonl",
            started_at_b,
            "span-b",
            "Implement source delete cleanup beta",
            &event_b,
        );
        span_b.session_id = Some("session-b".to_string());
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("task spans");
        store
            .rebuild_task_work_items_for_project_buckets(&BTreeSet::from([span_a
                .project_bucket
                .clone()]))
            .expect("rebuild");

        assert_eq!(store.task_spans().expect("task spans before").len(), 2);
        assert_eq!(store.work_items().expect("work items before").len(), 2);

        source(
            SourceCommand {
                command: SourceSubcommand::Remove {
                    source_id: source_a.source_id.0.clone(),
                    delete_data: true,
                },
            },
            &store,
        )
        .expect("remove source");

        assert!(store
            .source(&source_a.source_id)
            .expect("source a lookup")
            .is_none());
        assert!(store
            .source(&source_b.source_id)
            .expect("source b lookup")
            .is_some());

        let spans = store.task_spans().expect("task spans after");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].source_id, source_b.source_id);
        assert_eq!(spans[0].span_id, span_b.span_id);

        let work_items = store.work_items().expect("work items after");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].anchor_span_id, span_b.span_id);
        assert_eq!(work_items[0].total_tokens, 120);
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
                    price: "20.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
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
                    price: "200.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
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
    fn dry_run_sync_does_not_persist_sync_preferences() {
        let store = Store::in_memory().expect("store");

        sync(
            SyncCommand {
                dry_run: true,
                include_projects: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert_eq!(
            store.sync_preferences().expect("sync preferences"),
            SyncPreferences::default()
        );
    }

    #[test]
    fn http_dry_run_does_not_require_auth_or_clear_sync_tracking() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        store
            .record_sync_success("http", &endpoint, "batch_local", &[], &[], None)
            .expect("sync success");
        let state_before = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");

        let previous_api_url = std::env::var("STATSAI_API_URL").ok();
        let previous_sync_token = std::env::var("STATSAI_SYNC_TOKEN").ok();
        std::env::set_var(
            "STATSAI_API_URL",
            format!("https://{}-dry-run-authless.invalid", std::process::id()),
        );
        std::env::remove_var("STATSAI_SYNC_TOKEN");

        let result = sync(
            SyncCommand {
                endpoint: Some(endpoint.clone()),
                dry_run: true,
                ..test_sync_command("http")
            },
            &store,
            "device",
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("STATSAI_API_URL", value);
        } else {
            std::env::remove_var("STATSAI_API_URL");
        }
        if let Some(value) = previous_sync_token {
            std::env::set_var("STATSAI_SYNC_TOKEN", value);
        } else {
            std::env::remove_var("STATSAI_SYNC_TOKEN");
        }

        result.expect("sync dry run");

        let state_after = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");
        assert_eq!(state_after, state_before);
    }

    #[test]
    fn status_sync_does_not_persist_sync_preferences() {
        let store = Store::in_memory().expect("store");

        sync(
            SyncCommand {
                status: true,
                include_tasks: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync status");

        assert_eq!(
            store.sync_preferences().expect("sync preferences"),
            SyncPreferences::default()
        );
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
    fn http_sync_excludes_non_daily_stats_cache_summaries_from_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-stats-cache"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.metadata.summary_format = "claude_stats_cache".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(batch.summaries.is_empty());
    }

    #[test]
    fn http_sync_keeps_grok_build_summary_only_sessions_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-build-http-rollup"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 8, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("grok_build", &source, now, 500, None);
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.source.source_type = "build-session.json".to_string();
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(
            batch.summaries[0].metadata.summary_format,
            "grok_build_session_summary"
        );
    }

    #[test]
    fn http_sync_excludes_multi_day_external_daily_summaries_from_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-multi-day"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 5, 14, 23, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-multi-day");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(batch.summaries.is_empty());
    }

    #[test]
    fn http_sync_keeps_one_day_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_offset_local_day_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-offset-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 7, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 5, 14, 6, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-offset-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_dst_fallback_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-dst-fallback-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 11, 1, 7, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 11, 2, 7, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "external-dst-fallback-daily",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "manual-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_end() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-end"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = None;
        summary.observed_at = observed_at;
        summary.summary_id =
            summary_id("claude_code", &source.source_id, "manual-daily-missing-end");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_start() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-start"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let period_end = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("period_end");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = None;
        summary.period_end = Some(period_end);
        summary.observed_at = observed_at;
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "manual-daily-missing-start",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_bounds() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-bounds"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 13, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = None;
        summary.period_end = None;
        summary.observed_at = observed_at;
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "manual-daily-missing-bounds",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_legacy_ccusage_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-ccusage-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "ccusage_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "ccusage-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "ccusage_daily");
    }

    #[test]
    fn http_sync_keeps_exact_manual_period_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-period"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2025, 9, 4, 0, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2025, 9, 9, 23, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_period_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "manual-period");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(
            batch.summaries[0].metadata.summary_format,
            "manual_period_summary"
        );
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

        let mut passthrough = test_summary(
            "grok_build",
            &source,
            started_at + Duration::minutes(30),
            70,
            Some(account_id.clone()),
        );
        passthrough.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
        passthrough.source.source_kind = SourceKind::LocalAdapter;
        passthrough.source.source_type = "build-session.json".to_string();
        passthrough.metadata.summary_format = "grok_build_session_summary".to_string();
        passthrough.period_start = Some(started_at);
        passthrough.period_end = Some(started_at + Duration::minutes(30));
        store
            .upsert_summary(&passthrough)
            .expect("passthrough summary");

        let local_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let local_target = sync_target(&local_command).expect("local target");
        let (local_batch, local_mode) =
            build_sync_batch(&local_command, &store, "device", &local_target)
                .expect("local initial batch");
        assert_eq!(local_mode, SyncPayloadMode::Rollups);
        assert_eq!(local_batch.summaries.len(), 2);
        assert!(local_batch.summaries.iter().any(is_daily_rollup_summary));
        assert!(local_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
        assert!(local_batch.authoritative_snapshot.is_some());
        record_rollup_sync_success(&store, "http", &local_target, &local_batch)
            .expect("record local sync");

        let (local_repeat_batch, local_repeat_mode) =
            build_sync_batch(&local_command, &store, "device", &local_target)
                .expect("local repeat batch");
        assert_eq!(local_repeat_mode, SyncPayloadMode::Rollups);
        assert!(
            local_repeat_batch.summaries.is_empty(),
            "plain HTTP sync should be incremental after a target was synced"
        );
        assert!(local_repeat_batch.authoritative_snapshot.is_none());

        let local_full_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            full: true,
            ..test_sync_command("http")
        };
        let (local_full_batch, local_full_mode) =
            build_sync_batch(&local_full_command, &store, "device", &local_target)
                .expect("local full batch");
        assert_eq!(local_full_mode, SyncPayloadMode::Rollups);
        assert_eq!(
            local_full_batch.summaries.len(),
            2,
            "--full should deliberately resend synced rollups and passthrough summaries"
        );
        assert!(local_full_batch
            .summaries
            .iter()
            .any(is_daily_rollup_summary));
        assert!(local_full_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
        assert!(local_full_batch.authoritative_snapshot.is_some());

        let local_incremental_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            since_last: true,
            ..test_sync_command("http")
        };
        let (local_incremental_batch, _) =
            build_sync_batch(&local_incremental_command, &store, "device", &local_target)
                .expect("local incremental batch");
        assert!(local_incremental_batch.summaries.is_empty());
        assert!(local_incremental_batch.authoritative_snapshot.is_none());

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
        assert_eq!(remote_batch.summaries.len(), 2);
        assert!(remote_batch.summaries.iter().any(is_daily_rollup_summary));
        assert!(remote_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
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
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
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
    fn http_rollup_sync_sends_authoritative_snapshot_after_data_chunks() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-snapshot"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_snapshot".to_string(),
            device_id: "device".to_string(),
            sources: vec![source.clone()],
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
                source_ids: vec![source.source_id.clone()],
                ..SyncAuthoritativeSnapshot::default()
            }),
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].batch_id, "batch_snapshot");
        assert_eq!(chunks[0].sources, vec![source.clone()]);
        assert!(chunks[0].authoritative_snapshot.is_none());
        assert_eq!(chunks[1].batch_id, "batch_snapshot_snapshot_1");
        assert!(chunks[1].sources.is_empty());
        let snapshot = chunks[1]
            .authoritative_snapshot
            .as_ref()
            .expect("snapshot chunk");
        assert_eq!(snapshot.snapshot_id, "batch_snapshot_authoritative");
        assert_eq!(snapshot.part_index, 0);
        assert_eq!(snapshot.part_count, 1);
        assert_eq!(snapshot.source_ids, vec![source.source_id]);
        assert_eq!(
            logical_http_rollup_batch_id(&chunks[1].batch_id),
            "batch_snapshot"
        );
    }

    #[test]
    fn http_rollup_sync_bounds_authoritative_snapshot_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summary_ids = (0..(HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH * 2 + 1))
            .map(|index| statsai_core::SummaryId(format!("summary-{index}")))
            .collect::<Vec<_>>();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_large_snapshot".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
                summary_ids,
                ..SyncAuthoritativeSnapshot::default()
            }),
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);
        let snapshot_chunks = chunks
            .iter()
            .filter_map(|chunk| chunk.authoritative_snapshot.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(snapshot_chunks.len(), 3);
        assert!(snapshot_chunks.iter().all(|snapshot| {
            snapshot.source_ids.len()
                + snapshot.provider_account_ids.len()
                + snapshot.source_account_assignment_ids.len()
                + snapshot.subscription_ids.len()
                + snapshot.summary_ids.len()
                <= HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH
        }));
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
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
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
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
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
                sanitize_summary_for_sync(summary)
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
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };
        let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_send = Arc::clone(&observed);

        send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
            observed_for_send
                .lock()
                .expect("observed lock")
                .push((chunk.batch_id.clone(), chunk.summaries.len()));
            if chunk.summaries.len() > 2 {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 413: {"error":"sync_batch_d1_query_budget_exceeded","estimatedQueries":53,"maxQueries":45}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
        })
        .expect("send");

        let observed = observed.lock().expect("observed lock").clone();
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
        let pending = store
            .pending_summaries_for_sync(
                "http",
                &endpoint,
                &batch
                    .summaries
                    .iter()
                    .cloned()
                    .map(sanitize_summary_for_sync)
                    .collect::<Vec<_>>(),
            )
            .expect("pending summaries");
        assert!(pending.is_empty());
    }

    #[test]
    fn http_rollup_sync_retries_smaller_batches_after_payload_too_large() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-too-large"),
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
                summary.summary_id = statsai_core::SummaryId(format!("summary-too-large-{index}"));
                summary.metadata.summary_format = "daily_rollup.v1".to_string();
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_too_large".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries,
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };
        let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_send = Arc::clone(&observed);

        send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
            observed_for_send
                .lock()
                .expect("observed lock")
                .push((chunk.batch_id.clone(), chunk.summaries.len()));
            if chunk.summaries.len() > 2 {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 413: {"error":"sync_batch_too_large"}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
        })
        .expect("send");

        let observed = observed.lock().expect("observed lock").clone();
        assert_eq!(
            observed,
            vec![
                ("batch_too_large".to_string(), 4),
                ("batch_too_large_part_1_of_2".to_string(), 2),
                ("batch_too_large_part_2_of_2".to_string(), 2),
            ]
        );
        let state = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");
        assert_eq!(state.last_batch_id, batch.batch_id);
    }

    #[test]
    fn http_rollup_sync_restarts_full_snapshot_after_snapshot_failure() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-resume"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let account_id = provider_account_id("codex", "personal");
        for index in 0..26 {
            let event = test_event(
                "codex",
                &source,
                now + Duration::days(index as i64),
                Some(account_id.clone()),
                TokenParts::total(10),
            );
            store.insert_event(&event).expect("event");
        }
        store.rebuild_sync_rollups().expect("rebuild");

        let command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert_eq!(batch.sources.len(), 1);
        assert_eq!(batch.summaries.len(), 26);
        let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_send = Arc::clone(&observed);
        let mut observed_error = None;

        for chunk in split_http_rollup_sync_batches(&batch) {
            let result = send_http_rollup_chunk_with_retry_using(&chunk, &|chunk| {
                observed_for_send.lock().expect("observed lock").push((
                    chunk.batch_id.clone(),
                    chunk.sources.len(),
                    chunk.summaries.len(),
                    chunk.authoritative_snapshot.is_some(),
                ));
                if chunk.authoritative_snapshot.is_some() {
                    return Err(anyhow::Error::msg(
                        r#"sync endpoint returned HTTP 429: {"error":"rate_limited","retryAfterSeconds":60}"#,
                    ));
                }
                record_rollup_sync_chunk_success(&store, "http", &target, &logical_batch_id, chunk)
            });
            if let Err(send_error) = result {
                observed_error = Some(send_error);
                break;
            }
        }
        let error = observed_error.expect("rate limit should stop the snapshot request");
        assert!(error.to_string().contains("HTTP 429"));
        store
            .record_sync_failure("http", &target)
            .expect("record sync failure");

        let observed = observed.lock().expect("observed lock").clone();
        assert_eq!(
            observed,
            vec![
                (format!("{}_sources_1", batch.batch_id), 1, 0, false),
                (format!("{}_part_1_of_2", batch.batch_id), 0, 25, false),
                (format!("{}_part_2_of_2", batch.batch_id), 0, 1, false),
                (format!("{}_snapshot_1", batch.batch_id), 0, 0, true),
            ]
        );

        let sync_sources: Vec<_> = store
            .list_sources()
            .expect("sources")
            .into_iter()
            .map(sanitize_source_for_sync)
            .collect();
        assert!(store
            .pending_sources_for_sync("http", &target, &sync_sources)
            .expect("pending sources")
            .is_empty());

        let sync_rollups: Vec<_> = store
            .all_sync_rollup_summaries()
            .expect("rollups")
            .into_iter()
            .map(sanitize_summary_for_sync)
            .collect();
        let pending_rollups = store
            .pending_summaries_for_sync("http", &target, &sync_rollups)
            .expect("pending rollups");
        assert!(pending_rollups.is_empty());
        let state = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(state.last_batch_id, batch.batch_id);

        let (resume_batch, resume_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("resume batch");
        assert_eq!(resume_mode, SyncPayloadMode::Rollups);
        assert!(resume_batch.sources.is_empty());
        assert_eq!(resume_batch.summaries.len(), 26);
        assert!(resume_batch.authoritative_snapshot.is_some());
        let state_after_build = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(
            state_after_build.pending_resume_batch_id, state.pending_resume_batch_id,
            "building the replacement snapshot must not clear resume state"
        );

        let since_last_command = SyncCommand {
            endpoint: Some(endpoint),
            since_last: true,
            ..test_sync_command("http")
        };
        let (since_last_resume, _) =
            build_sync_batch(&since_last_command, &store, "device", &target)
                .expect("since-last resume batch");
        assert_eq!(since_last_resume.summaries.len(), 26);
        assert!(since_last_resume.authoritative_snapshot.is_some());
    }

    #[test]
    fn failed_http_sync_without_ack_keeps_next_default_sync_full_history() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-no-partial-resume"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let event = test_event(
            "codex",
            &source,
            now,
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(10),
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
        assert_eq!(initial_batch.summaries.len(), 1);
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        store
            .record_sync_failure("http", &target)
            .expect("record failed sync");

        let state = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert!(state.pending_resume_batch_id.is_none());
        assert!(state.failure_count > 0);

        let (retry_batch, retry_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("retry batch");
        assert_eq!(retry_mode, SyncPayloadMode::Rollups);
        assert_eq!(retry_batch.summaries.len(), 1);

        let since_last_command = SyncCommand {
            endpoint: Some(endpoint),
            since_last: true,
            ..test_sync_command("http")
        };
        let (since_last_batch, since_last_mode) =
            build_sync_batch(&since_last_command, &store, "device", &target)
                .expect("since-last retry batch");
        assert_eq!(since_last_mode, SyncPayloadMode::Rollups);
        assert!(
            since_last_batch.summaries.is_empty(),
            "explicit --since-last should not force full history after an unacknowledged failure"
        );
    }

    #[test]
    fn full_dry_run_does_not_clear_pending_http_resume_state() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-full-dry-run-resume"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let event = test_event(
            "codex",
            &source,
            now,
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(10),
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let initial_command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&initial_command).expect("target");
        let (initial_batch, _) =
            build_sync_batch(&initial_command, &store, "device", &target).expect("initial batch");
        let expected_logical_batch_id = logical_http_rollup_batch_id(&initial_batch.batch_id);
        record_rollup_sync_chunk_success(
            &store,
            "http",
            &target,
            &expected_logical_batch_id,
            &initial_batch,
        )
        .expect("record partial sync state");

        let state = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(
            state.pending_resume_batch_id.as_deref(),
            Some(expected_logical_batch_id.as_str())
        );

        let full_dry_run_command = SyncCommand {
            endpoint: Some(endpoint),
            full: true,
            dry_run: true,
            ..test_sync_command("http")
        };
        let (dry_run_batch, dry_run_mode) =
            build_sync_batch(&full_dry_run_command, &store, "device", &target)
                .expect("full dry-run batch");
        assert_eq!(dry_run_mode, SyncPayloadMode::Rollups);
        assert_eq!(dry_run_batch.summaries.len(), 1);

        let state_after = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(
            state_after.pending_resume_batch_id, state.pending_resume_batch_id,
            "dry-run must not mutate pending resume state"
        );
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
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
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
    fn http_rollup_retry_splits_mixed_task_payloads() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_task_only_sync_batch(now, 1, 1);

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_too_large"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_buckets.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_verifications.len())
                .sum::<usize>(),
            1
        );
        assert!(chunks
            .iter()
            .all(|chunk| { chunk.task_buckets.is_empty() || chunk.task_verifications.is_empty() }));
    }

    #[test]
    fn http_rollup_retry_halves_task_only_bucket_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_task_only_sync_batch(now, 3, 0);

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_d1_query_budget_exceeded"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].task_buckets.len(), 2);
        assert_eq!(chunks[1].task_buckets.len(), 1);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.task_verifications.is_empty()));
    }

    #[test]
    fn record_sync_batch_success_marks_task_entities_synced_for_file_sink() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let store = Store::in_memory().expect("store");
        let batch = test_task_only_sync_batch(now, 1, 1);
        for bucket in &batch.task_buckets {
            store
                .replace_task_bucket_snapshot(bucket)
                .expect("seed task bucket snapshot");
        }
        for verification in &batch.task_verifications {
            store
                .merge_task_verification(verification)
                .expect("seed task verification");
        }

        record_sync_batch_success(&store, "file", "/tmp/statsai-sync-batch.json", &batch)
            .expect("record sync batch success");

        assert!(store
            .pending_task_bucket_snapshots_for_sync(
                "file",
                "/tmp/statsai-sync-batch.json",
                &batch.device_id,
                false,
                None,
            )
            .expect("pending task buckets")
            .is_empty());
        assert!(store
            .pending_task_verifications_for_sync("file", "/tmp/statsai-sync-batch.json")
            .expect("pending task verifications")
            .is_empty());
    }

    #[test]
    fn http_rollup_sends_metadata_before_task_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-metadata-before-task"),
            LocationOrigin::Configured,
        );
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata_before_task".to_string(),
            device_id: "device".to_string(),
            sources: vec![source],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries: vec![],
            task_buckets: test_task_only_sync_batch(now, 1, 0).task_buckets,
            task_verifications: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].sources.len(), 1);
        assert!(chunks[0].task_buckets.is_empty());
        assert!(chunks[1].sources.is_empty());
        assert_eq!(chunks[1].task_buckets.len(), 1);
    }

    #[test]
    fn custom_http_sinks_skip_task_verification_feed_derivation() {
        assert_eq!(
            http_task_verification_feed_url("https://example.com/custom-sync"),
            None
        );
        assert_eq!(
            http_task_verification_feed_url("https://api.example.com/api/sync/batches"),
            Some("https://api.example.com/api/task-sync/verifications".to_string())
        );
    }

    #[test]
    fn optional_task_verification_feed_statuses_do_not_fail_sync() {
        assert!(optional_task_verification_feed_status(404));
        assert!(optional_task_verification_feed_status(405));
        assert!(optional_task_verification_feed_status(501));
        assert!(!optional_task_verification_feed_status(400));
        assert!(!optional_task_verification_feed_status(429));
        assert!(!optional_task_verification_feed_status(500));
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
                    repo_remote_hash: Some(format!("repo-hash-{index}")),
                    repo_label: Some(format!("owner/repo-{index}")),
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
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
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
    fn http_rollup_project_counts_include_path_only_projects() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-path-only-project"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut summary = test_summary("codex", &source, now, 10, None);
        summary.project = Some(ProjectInfo {
            project_id: "project-path-only".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
        });
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_path_only_project".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            events: vec![],
            summaries: vec![summary],
            task_buckets: vec![],
            task_verifications: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };

        assert_eq!(http_rollup_project_count(&batch), 1);
        assert_eq!(http_rollup_project_location_count(&batch), 1);
    }

    fn test_task_only_sync_batch(
        now: DateTime<Utc>,
        bucket_count: usize,
        verification_count: usize,
    ) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-task-only"),
            LocationOrigin::Configured,
        );
        let task_buckets = (0..bucket_count)
            .map(|index| {
                let started_at = now + Duration::minutes(index as i64);
                let ended_at = started_at + Duration::minutes(5);
                let span_id = TaskSpanId(format!("span-task-{index}"));
                let work_item_id = WorkItemId(format!("work-task-{index}"));
                TaskBucketSnapshot {
                    project_bucket: format!("bucket-task-{index}"),
                    generated_at: ended_at,
                    applied_verification_cursor: None,
                    work_items: vec![WorkItem {
                        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                        work_item_id: work_item_id.clone(),
                        anchor_span_id: span_id.clone(),
                        tail_span_id: span_id.clone(),
                        project_bucket: format!("bucket-task-{index}"),
                        title: format!("Task {index}"),
                        normalized_title: format!("task {index}"),
                        status: TaskStatus::NeedsReview,
                        confidence: Confidence::Medium,
                        started_at,
                        ended_at,
                        duration_seconds: Some(300),
                        span_count: 1,
                        event_count: 1,
                        total_input_tokens: 10,
                        total_cache_creation_tokens: 0,
                        total_cache_read_tokens: 0,
                        total_output_tokens: 5,
                        total_reasoning_tokens: 0,
                        total_tokens: 15,
                        estimated_cost_usd: Some(25),
                        providers: vec!["codex".to_string()],
                        issue_keys: Vec::new(),
                        repo_label: Some("statsai/repo".to_string()),
                        branch_labels: vec!["main".to_string()],
                        path_label: Some("/workspace/statsai".to_string()),
                        summary_preview: None,
                        todo_excerpt: None,
                        no_git: false,
                        cross_provider: false,
                        continuation_reasons: Vec::new(),
                        review_reasons: vec!["needs_review".to_string()],
                    }],
                    members: vec![WorkItemMember {
                        work_item_id,
                        span_id: span_id.clone(),
                        ordinal: 0,
                    }],
                    spans: vec![TaskSpan {
                        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                        span_id,
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        span_kind: "codex_task".to_string(),
                        source_record_id: None,
                        source_file_path_hash: None,
                        summary_id: None,
                        session_id: Some(format!("session-task-{index}")),
                        thread_id: Some(format!("thread-task-{index}")),
                        title: format!("Task {index}"),
                        normalized_title: format!("task {index}"),
                        title_source: Some("thread_name".to_string()),
                        summary_preview: None,
                        todo_excerpt: None,
                        issue_keys: Vec::new(),
                        branch_family: Some("main".to_string()),
                        project_bucket: format!("bucket-task-{index}"),
                        project: None,
                        git: None,
                        usage: UsageCounts {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                            requests: Some(1),
                            ..UsageCounts::default()
                        },
                        estimated_cost_usd: Some(25),
                        event_count: 1,
                        has_usage_evidence: true,
                        total_messages: 2,
                        user_messages: 1,
                        assistant_messages: 1,
                        developer_messages: 0,
                        linked_event_ids: Vec::new(),
                        confidence: Confidence::High,
                        is_meta: false,
                        started_at,
                        ended_at: Some(ended_at),
                        duration_seconds: Some(300),
                    }],
                }
            })
            .collect::<Vec<_>>();
        let task_verifications = (0..verification_count)
            .map(|index| {
                let timestamp = now + Duration::minutes(index as i64);
                TaskVerification {
                    schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
                    verification_id: TaskVerificationId(format!("tvf-task-{index}")),
                    action_key: format!("status:span-task-{index}"),
                    action: TaskVerificationAction::Reject {
                        work_item_id: WorkItemId(format!("work-task-{index}")),
                        anchor_span_id: TaskSpanId(format!("span-task-{index}")),
                        reason: TaskVerdict::Meta,
                    },
                    created_at: timestamp,
                    updated_at: timestamp,
                }
            })
            .collect::<Vec<_>>();

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets,
            task_verifications,
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    fn test_dense_task_only_sync_batch(now: DateTime<Utc>, span_count: usize) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-dense-task-only"),
            LocationOrigin::Configured,
        );
        let spans = (0..span_count)
            .map(|index| {
                let started_at = now + Duration::minutes(index as i64);
                let ended_at = started_at + Duration::minutes(1);
                TaskSpan {
                    schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                    span_id: TaskSpanId(format!("dense-span-{index}")),
                    provider: "codex".to_string(),
                    source_id: source.source_id.clone(),
                    span_kind: "codex_task".to_string(),
                    source_record_id: None,
                    source_file_path_hash: None,
                    summary_id: None,
                    session_id: Some(format!("dense-session-{index}")),
                    thread_id: Some(format!("dense-thread-{index}")),
                    title: format!("Dense task {index}"),
                    normalized_title: format!("dense task {index}"),
                    title_source: Some("thread_name".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    issue_keys: Vec::new(),
                    branch_family: Some("main".to_string()),
                    project_bucket: "dense-bucket".to_string(),
                    project: Some(ProjectInfo {
                        project_id: "project-dense".to_string(),
                        project_label: Some("Dense".to_string()),
                        repo_remote_hash: Some("repo-dense".to_string()),
                        repo_label: Some("statsai/dense".to_string()),
                        branch_hash: Some("branch-dense".to_string()),
                        branch_label: Some("main".to_string()),
                        path_hash: Some("path-dense".to_string()),
                        path_label: Some("/workspace/dense".to_string()),
                    }),
                    git: None,
                    usage: UsageCounts {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(15),
                        requests: Some(1),
                        ..UsageCounts::default()
                    },
                    estimated_cost_usd: Some(25),
                    event_count: 1,
                    has_usage_evidence: true,
                    total_messages: 2,
                    user_messages: 1,
                    assistant_messages: 1,
                    developer_messages: 0,
                    linked_event_ids: Vec::new(),
                    confidence: Confidence::High,
                    is_meta: false,
                    started_at,
                    ended_at: Some(ended_at),
                    duration_seconds: Some(60),
                }
            })
            .collect::<Vec<_>>();
        let members = spans
            .iter()
            .enumerate()
            .map(|(index, span)| WorkItemMember {
                work_item_id: WorkItemId("dense-work-item".to_string()),
                span_id: span.span_id.clone(),
                ordinal: index,
            })
            .collect::<Vec<_>>();
        let last_span = spans.last().expect("last dense span");

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_dense_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: vec![TaskBucketSnapshot {
                project_bucket: "dense-bucket".to_string(),
                generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                applied_verification_cursor: None,
                work_items: vec![WorkItem {
                    schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                    work_item_id: WorkItemId("dense-work-item".to_string()),
                    anchor_span_id: spans.first().expect("first dense span").span_id.clone(),
                    tail_span_id: last_span.span_id.clone(),
                    project_bucket: "dense-bucket".to_string(),
                    title: "Dense task".to_string(),
                    normalized_title: "dense task".to_string(),
                    status: TaskStatus::NeedsReview,
                    confidence: Confidence::Medium,
                    started_at: spans.first().expect("first dense span").started_at,
                    ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                    duration_seconds: Some((span_count as u64).saturating_mul(60)),
                    span_count: span_count as u64,
                    event_count: span_count as u64,
                    total_input_tokens: (span_count as u64).saturating_mul(10),
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_output_tokens: (span_count as u64).saturating_mul(5),
                    total_reasoning_tokens: 0,
                    total_tokens: (span_count as u64).saturating_mul(15),
                    estimated_cost_usd: Some((span_count as i64).saturating_mul(25)),
                    providers: vec!["codex".to_string()],
                    issue_keys: Vec::new(),
                    repo_label: Some("statsai/dense".to_string()),
                    branch_labels: vec!["main".to_string()],
                    path_label: Some("/workspace/dense".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    no_git: false,
                    cross_provider: false,
                    continuation_reasons: Vec::new(),
                    review_reasons: vec!["needs_review".to_string()],
                }],
                members,
                spans,
            }],
            task_verifications: Vec::new(),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    fn test_multi_bucket_dense_task_only_sync_batch(
        now: DateTime<Utc>,
        bucket_count: usize,
        span_count_per_bucket: usize,
    ) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-multi-dense-task-only"),
            LocationOrigin::Configured,
        );
        let task_buckets = (0..bucket_count)
            .map(|bucket_index| {
                let project_bucket = format!("dense-bucket-{bucket_index}");
                let work_item_id = WorkItemId(format!("dense-work-item-{bucket_index}"));
                let spans = (0..span_count_per_bucket)
                    .map(|span_index| {
                        let offset_minutes =
                            (bucket_index * span_count_per_bucket + span_index) as i64;
                        let started_at = now + Duration::minutes(offset_minutes);
                        let ended_at = started_at + Duration::minutes(1);
                        TaskSpan {
                            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                            span_id: TaskSpanId(format!(
                                "dense-bucket-{bucket_index}-span-{span_index}"
                            )),
                            provider: "codex".to_string(),
                            source_id: source.source_id.clone(),
                            span_kind: "codex_task".to_string(),
                            source_record_id: None,
                            source_file_path_hash: None,
                            summary_id: None,
                            session_id: Some(format!(
                                "dense-bucket-{bucket_index}-session-{span_index}"
                            )),
                            thread_id: Some(format!(
                                "dense-bucket-{bucket_index}-thread-{span_index}"
                            )),
                            title: format!("Dense task {bucket_index}-{span_index}"),
                            normalized_title: format!("dense task {bucket_index}-{span_index}"),
                            title_source: Some("thread_name".to_string()),
                            summary_preview: None,
                            todo_excerpt: None,
                            issue_keys: Vec::new(),
                            branch_family: Some("main".to_string()),
                            project_bucket: project_bucket.clone(),
                            project: Some(ProjectInfo {
                                project_id: format!("project-dense-{bucket_index}"),
                                project_label: Some(format!("Dense {bucket_index}")),
                                repo_remote_hash: Some(format!("repo-dense-{bucket_index}")),
                                repo_label: Some(format!("statsai/dense-{bucket_index}")),
                                branch_hash: Some("branch-dense".to_string()),
                                branch_label: Some("main".to_string()),
                                path_hash: Some(format!("path-dense-{bucket_index}")),
                                path_label: Some(format!("/workspace/dense-{bucket_index}")),
                            }),
                            git: None,
                            usage: UsageCounts {
                                input_tokens: Some(10),
                                output_tokens: Some(5),
                                total_tokens: Some(15),
                                requests: Some(1),
                                ..UsageCounts::default()
                            },
                            estimated_cost_usd: Some(25),
                            event_count: 1,
                            has_usage_evidence: true,
                            total_messages: 2,
                            user_messages: 1,
                            assistant_messages: 1,
                            developer_messages: 0,
                            linked_event_ids: Vec::new(),
                            confidence: Confidence::High,
                            is_meta: false,
                            started_at,
                            ended_at: Some(ended_at),
                            duration_seconds: Some(60),
                        }
                    })
                    .collect::<Vec<_>>();
                let members = spans
                    .iter()
                    .enumerate()
                    .map(|(span_index, span)| WorkItemMember {
                        work_item_id: work_item_id.clone(),
                        span_id: span.span_id.clone(),
                        ordinal: span_index,
                    })
                    .collect::<Vec<_>>();
                let first_span = spans.first().expect("first dense span");
                let last_span = spans.last().expect("last dense span");
                TaskBucketSnapshot {
                    project_bucket: project_bucket.clone(),
                    generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                    applied_verification_cursor: None,
                    work_items: vec![WorkItem {
                        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                        work_item_id: work_item_id.clone(),
                        anchor_span_id: first_span.span_id.clone(),
                        tail_span_id: last_span.span_id.clone(),
                        project_bucket,
                        title: format!("Dense task bucket {bucket_index}"),
                        normalized_title: format!("dense task bucket {bucket_index}"),
                        status: TaskStatus::NeedsReview,
                        confidence: Confidence::Medium,
                        started_at: first_span.started_at,
                        ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                        duration_seconds: Some((span_count_per_bucket as u64).saturating_mul(60)),
                        span_count: span_count_per_bucket as u64,
                        event_count: span_count_per_bucket as u64,
                        total_input_tokens: (span_count_per_bucket as u64).saturating_mul(10),
                        total_cache_creation_tokens: 0,
                        total_cache_read_tokens: 0,
                        total_output_tokens: (span_count_per_bucket as u64).saturating_mul(5),
                        total_reasoning_tokens: 0,
                        total_tokens: (span_count_per_bucket as u64).saturating_mul(15),
                        estimated_cost_usd: Some((span_count_per_bucket as i64).saturating_mul(25)),
                        providers: vec!["codex".to_string()],
                        issue_keys: Vec::new(),
                        repo_label: Some(format!("statsai/dense-{bucket_index}")),
                        branch_labels: vec!["main".to_string()],
                        path_label: Some(format!("/workspace/dense-{bucket_index}")),
                        summary_preview: None,
                        todo_excerpt: None,
                        no_git: false,
                        cross_provider: false,
                        continuation_reasons: Vec::new(),
                        review_reasons: vec!["needs_review".to_string()],
                    }],
                    members,
                    spans,
                }
            })
            .collect::<Vec<_>>();

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_multi_dense_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets,
            task_verifications: Vec::new(),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    #[test]
    fn dense_single_task_bucket_stays_within_batched_d1_budget_estimate() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_dense_task_only_sync_batch(now, 240);

        assert!(
            estimate_http_rollup_d1_queries(&batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET,
            "dense single-bucket task sync should fit after batched task writes"
        );
    }

    #[test]
    fn multi_bucket_dense_task_sync_splits_to_fit_chunked_write_budget() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_multi_bucket_dense_task_only_sync_batch(now, 5, 600);

        let chunks = split_http_rollup_sync_batches(&batch);

        assert!(chunks.len() > 1);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_buckets.len())
                .sum::<usize>(),
            batch.task_buckets.len()
        );
        assert!(chunks
            .iter()
            .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
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
                None,
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
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_task_buckets_2"),
            "batch_1"
        );
        assert_eq!(
            logical_http_rollup_batch_id("batch_1_part_3_of_9_task_verifications_4"),
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
        let local_verify = sync_local_verify(&store, "http", &target, Some(&local_state), false)
            .expect("local verify");
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
            Some(
                "sources 0!=1, accounts 0!=1, source_account_assignments 0!=1, subscriptions 0!=1"
            )
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
    fn http_preflight_status_url_points_at_lightweight_worker_status_endpoint() {
        assert_eq!(
            http_preflight_status_url("https://api.example.com/api/sync/batches").expect("status"),
            "https://api.example.com/api/sync/status?view=preflight"
        );
    }

    #[test]
    fn remote_hosted_tasks_enabled_defaults_true_when_capability_missing() {
        assert!(remote_hosted_tasks_enabled(&json!({
            "device": {
                "last_sync_batch_id": "batch-1"
            }
        })));
    }

    #[test]
    fn remote_hosted_tasks_enabled_reads_explicit_false_capability() {
        assert!(!remote_hosted_tasks_enabled(&json!({
            "capabilities": {
                "hostedTasks": false
            }
        })));
    }

    #[test]
    fn optional_http_sync_preflight_statuses_do_not_disable_task_sync() {
        assert!(optional_http_sync_preflight_status(404));
        assert!(optional_http_sync_preflight_status(405));
        assert!(optional_http_sync_preflight_status(501));
        assert!(!optional_http_sync_preflight_status(400));
        assert!(!optional_http_sync_preflight_status(500));
    }

    #[test]
    fn http_reset_url_points_at_worker_reset_endpoint() {
        assert_eq!(
            http_reset_url("https://api.example.com/api/sync/batches").expect("reset"),
            "https://api.example.com/api/sync/reset"
        );
    }

    #[test]
    fn credentialed_http_helpers_reject_remote_plaintext_before_request() {
        let endpoint = "http://api.example.com/api/sync/batches";

        for result in [
            http_remote_verify(endpoint, "token"),
            http_remote_preflight_status(endpoint, "token"),
            http_remote_reset(endpoint, "token"),
        ] {
            let error = result.expect_err("remote plaintext must fail");
            assert!(error.to_string().contains("requires HTTPS"));
        }

        let command = SyncCommand {
            auth_token: Some("token".to_string()),
            ..test_sync_command("http")
        };
        let error = http_remote_hosted_tasks_enabled(&command, endpoint)
            .expect_err("remote plaintext preflight must fail");
        assert!(error.to_string().contains("requires HTTPS"));
    }

    #[test]
    fn device_remote_reset_response_requires_explicit_device_scope() {
        assert!(ensure_device_remote_reset_response(&json!({
            "ok": true,
            "scope": "device_mirror",
            "device_id": "device-1"
        }))
        .is_ok());
        assert!(ensure_device_remote_reset_response(&json!({
            "ok": true,
            "scope": "mirror"
        }))
        .is_err());
    }

    #[test]
    fn no_cache_scan_reselects_unchanged_files() {
        let store = Store::in_memory().expect("store");
        let source_id = statsai_core::SourceId("src-no-cache".to_string());
        let compatible_signatures = HashMap::new();
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

        let initial = select_scan_file_entries(
            &store,
            &source_id,
            &entries,
            &compatible_signatures,
            false,
            false,
            false,
        )
        .expect("initial selection");
        assert_eq!(initial, entries);
        store
            .record_scan_file_entries(&source_id, &entries)
            .expect("record cache state");

        let default_selection = select_scan_file_entries(
            &store,
            &source_id,
            &entries,
            &compatible_signatures,
            false,
            false,
            false,
        )
        .expect("default selection");
        assert!(default_selection.is_empty());

        let no_cache_selection = select_scan_file_entries(
            &store,
            &source_id,
            &entries,
            &compatible_signatures,
            false,
            true,
            false,
        )
        .expect("no-cache selection");
        assert_eq!(no_cache_selection, entries);

        let replace_selection = select_scan_file_entries(
            &store,
            &source_id,
            &entries,
            &compatible_signatures,
            true,
            false,
            false,
        )
        .expect("replace selection");
        assert_eq!(replace_selection, entries);
    }

    #[test]
    fn full_source_rescan_replaces_existing_source_records() {
        assert!(should_replace_source_records_for_scan(
            true, false, 0, 0, false
        ));
        assert!(should_replace_source_records_for_scan(
            false, true, 0, 0, false
        ));
        assert!(should_replace_source_records_for_scan(
            false, false, 2, 2, false
        ));
        assert!(should_replace_source_records_for_scan(
            false, false, 0, 0, true
        ));
        assert!(!should_replace_source_records_for_scan(
            false, false, 2, 1, false
        ));
        assert!(!should_replace_source_records_for_scan(
            false, false, 0, 0, false
        ));
    }

    #[test]
    fn scan_file_reconciliation_tracks_removed_candidates() {
        let store = Store::in_memory().expect("store");
        let source_id = statsai_core::SourceId("src-removed-cache".to_string());
        let tracked = vec![
            ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            },
            ScanFileStateEntry {
                cache_key: "/tmp/b.jsonl".to_string(),
                cache_signature: "sig-b-1".to_string(),
            },
        ];
        store
            .record_scan_file_entries(&source_id, &tracked)
            .expect("record tracked cache state");

        let reconciliation = select_scan_file_reconciliation(
            &store,
            &source_id,
            &[ScanFileStateEntry {
                cache_key: "/tmp/b.jsonl".to_string(),
                cache_signature: "sig-b-1".to_string(),
            }],
            &HashMap::new(),
            false,
            false,
            false,
        )
        .expect("reconciliation");

        assert!(reconciliation.pending_entries.is_empty());
        assert_eq!(
            reconciliation.removed_entries,
            vec![ScanFileStateEntry {
                cache_key: "/tmp/a.jsonl".to_string(),
                cache_signature: "sig-a-1".to_string(),
            }]
        );
    }

    #[test]
    fn partial_scan_removes_rows_that_disappear_from_changed_file() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-partial-rescan"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-partial-rescan/a.jsonl";
        let file_b = "/tmp/codex-partial-rescan/b.jsonl";
        let initial_candidates = vec![
            test_scan_candidate(file_a, "sig-a-1"),
            test_scan_candidate(file_b, "sig-b-1"),
        ];
        let next_candidates = vec![
            test_scan_candidate(file_a, "sig-a-2"),
            test_scan_candidate(file_b, "sig-b-1"),
        ];
        let a_started_at = Utc
            .with_ymd_and_hms(2026, 5, 1, 10, 0, 0)
            .single()
            .expect("a_started_at");
        let b_started_at = Utc
            .with_ymd_and_hms(2026, 5, 2, 10, 0, 0)
            .single()
            .expect("b_started_at");
        let initial_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: initial_candidates,
            scan_result: statsai_adapters::AdapterScan {
                events: vec![
                    test_scan_event(&source, file_a, a_started_at, "event-a", 100),
                    test_scan_event(&source, file_b, b_started_at, "event-b", 200),
                ],
                summaries: vec![
                    test_scan_summary(&source, file_a, a_started_at, "summary-a", 100),
                    test_scan_summary(&source, file_b, b_started_at, "summary-b", 200),
                ],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(initial_adapter)],
        )
        .expect("initial scan");

        assert_eq!(store.event_count().expect("event count"), 2);
        assert_eq!(store.summary_count().expect("summary count"), 2);
        assert_eq!(store.sync_rollup_count().expect("rollup count"), 2);

        let changed_only_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: next_candidates,
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(changed_only_adapter)],
        )
        .expect("partial scan");

        let events = store.events_for_source(&source.source_id).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .parse_evidence
                .as_ref()
                .and_then(|evidence| evidence.source_record_id.as_deref()),
            Some("event-b")
        );
        let summaries = store
            .summaries_for_source(&source.source_id)
            .expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].summary_id,
            summary_id("codex", &source.source_id, "summary-b")
        );
        assert_eq!(store.sync_rollup_count().expect("rollup count"), 1);
    }

    #[test]
    fn scan_persists_task_spans_and_rebuilds_work_items() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-spans"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-spans/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 9, 30, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
        let task_span = test_task_span(
            &source,
            file_path,
            started_at,
            "task-span-a",
            "Implement local task collection",
            &event,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_path, "sig-a")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: vec![task_span],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
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

        let spans = store.task_spans().expect("task spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].title, "Implement local task collection");

        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].title, "Implement local task collection");
        assert_eq!(work_items[0].span_count, 1);
        assert_eq!(work_items[0].total_tokens, 150);
    }

    #[test]
    fn scan_without_include_tasks_does_not_persist_task_tables() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-opt-in"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-opt-in/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 9, 35, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
        let task_span = test_task_span(
            &source,
            file_path,
            started_at,
            "task-span-a",
            "Implement local task collection",
            &event,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_path, "sig-a")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: vec![task_span],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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

        assert_eq!(store.event_count().expect("event count"), 1);
        assert!(store.task_spans().expect("task spans").is_empty());
        assert!(store.work_items().expect("work items").is_empty());
    }

    #[test]
    fn scan_with_include_tasks_backfills_files_cached_without_tasks() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-backfill"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-backfill/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 9, 38, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
        let task_span = test_task_span(
            &source,
            file_path,
            started_at,
            "task-span-a",
            "Backfill local tasks",
            &event,
        );
        let candidate = test_scan_candidate(file_path, "sig-a");
        let initial_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![candidate.clone()],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event.clone()],
                task_spans: vec![task_span.clone()],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(initial_adapter)],
        )
        .expect("initial scan");
        assert!(store.task_spans().expect("initial task spans").is_empty());

        let scan_calls = Arc::new(Mutex::new(0u64));
        let backfill_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![candidate],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: vec![task_span],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: Some(scan_calls.clone()),
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(backfill_adapter)],
        )
        .expect("task backfill scan");

        assert_eq!(*scan_calls.lock().expect("scan calls"), 1);
        let spans = store.task_spans().expect("task spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].title, "Backfill local tasks");
        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].title, "Backfill local tasks");
    }

    #[test]
    fn scan_without_include_tasks_preserves_existing_task_tables() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-preserve"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-preserve/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 9, 40, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
        let task_span = test_task_span(
            &source,
            file_path,
            started_at,
            "task-span-a",
            "Keep local tasks",
            &event,
        );
        store
            .upsert_task_spans(std::slice::from_ref(&task_span))
            .expect("insert task span");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_path, "sig-b")],
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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

        let spans = store.task_spans().expect("task spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].title, "Keep local tasks");

        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].title, "Keep local tasks");
    }

    #[test]
    fn scan_rebuild_prefers_real_work_item_title_over_metric_spans() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-title-quality"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-title-quality/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 10, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "event-a", 200);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(1),
            "event-b",
            220,
        );
        let event_c = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "event-c",
            240,
        );
        let span_metric = test_task_span(
            &source,
            file_path,
            started_at,
            "metric-a",
            "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
            &event_a,
        );
        let span_coverage = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(1),
            "metric-b",
            "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
            &event_b,
        );
        let span_intent = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "intent-c",
            "I want to choose the best adapters to average",
            &event_c,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_path, "sig-quality")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event_a, event_b, event_c],
                task_spans: vec![span_metric, span_coverage, span_intent],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
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

        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(
            work_items[0].title,
            "I want to choose the best adapters to average"
        );
    }

    #[test]
    fn scan_preview_does_not_persist_task_tables() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-preview"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-preview/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 9, 45, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "preview-event", 80);
        let task_span = test_task_span(
            &source,
            file_path,
            started_at,
            "preview-span",
            "Preview task collection",
            &event,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![test_scan_candidate(file_path, "sig-preview")],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event],
                task_spans: vec![task_span],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
                preview: true,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(adapter)],
        )
        .expect("preview scan");

        assert_eq!(store.event_count().expect("event count"), 0);
        assert_eq!(store.summary_count().expect("summary count"), 0);
        assert!(store.task_spans().expect("task spans").is_empty());
        assert!(store.work_items().expect("work items").is_empty());
    }

    #[test]
    fn preview_task_rebuild_counts_only_affected_work_items() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-preview-rebuild"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-task-preview-rebuild/a.jsonl";
        let file_b = "/tmp/codex-task-preview-rebuild/b.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 10, 30, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_a, started_at, "preview-a", 90);
        let event_b = test_scan_event(
            &source,
            file_b,
            started_at + Duration::minutes(10),
            "preview-b",
            110,
        );
        let mut span_a = test_task_span(
            &source,
            file_a,
            started_at,
            "preview-span-a",
            "Preview rebuild task A",
            &event_a,
        );
        let mut span_b = test_task_span(
            &source,
            file_b,
            started_at + Duration::minutes(10),
            "preview-span-b",
            "Preview rebuild task B",
            &event_b,
        );
        span_a.project = Some(ProjectInfo {
            project_id: "project-a".to_string(),
            project_label: Some("project-a".to_string()),
            repo_remote_hash: Some("repo-a".to_string()),
            repo_label: Some("owner/project-a".to_string()),
            branch_hash: Some("branch-a".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-a".to_string()),
            path_label: Some("/tmp/project-a".to_string()),
        });
        span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
        span_a.branch_family = branch_family(Some("main"));
        span_b.project = Some(ProjectInfo {
            project_id: "project-b".to_string(),
            project_label: Some("project-b".to_string()),
            repo_remote_hash: Some("repo-b".to_string()),
            repo_label: Some("owner/project-b".to_string()),
            branch_hash: Some("branch-b".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-b".to_string()),
            path_label: Some("/tmp/project-b".to_string()),
        });
        span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
        span_b.branch_family = branch_family(Some("main"));

        store
            .insert_events(&[event_a.clone(), event_b.clone()])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let mut updated_span_a = span_a.clone();
        updated_span_a.title = "Preview rebuild task A updated".to_string();
        updated_span_a.summary_preview = Some("Preview rebuild task A updated".to_string());
        updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

        let pending_entries = scan_file_state_entries(&[test_scan_candidate(file_a, "sig-a-2")]);
        let mut preview = PreviewTaskRebuild::default();
        let rebuilt = preview
            .apply_source_changes(
                &store,
                SourceTaskChangeSet {
                    source_id: &source.source_id,
                    replace_source_records: false,
                    touched_files: true,
                    pending_file_entries: &pending_entries,
                    removed_file_entries: &[],
                    task_spans: &[updated_span_a],
                },
            )
            .expect("preview work items rebuilt");
        assert_eq!(rebuilt, 1);
        assert_eq!(store.task_spans().expect("task spans").len(), 2);
        assert_eq!(store.work_items().expect("work items").len(), 2);
    }

    #[test]
    fn preview_task_rebuild_counts_shared_bucket_rebuilds_per_source_step() {
        let store = Store::in_memory().expect("store");
        let source_a = SourceLocation::local_adapter(
            "claude_code",
            "test-a",
            "0",
            Path::new("/tmp/preview-shared-a"),
            LocationOrigin::Configured,
        );
        let source_b = SourceLocation::local_adapter(
            "claude_code",
            "test-b",
            "0",
            Path::new("/tmp/preview-shared-b"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source_a).expect("source a");
        store.upsert_source(&source_b).expect("source b");

        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
            .single()
            .expect("started_at");
        let file_a = "/tmp/preview-shared-a/session.jsonl";
        let file_b = "/tmp/preview-shared-b/session.jsonl";
        let event_a = test_scan_event(&source_a, file_a, started_at, "shared-a", 120);
        let event_b = test_scan_event(
            &source_b,
            file_b,
            started_at + Duration::minutes(20),
            "shared-b",
            140,
        );
        let mut span_a = test_task_span(
            &source_a,
            file_a,
            started_at,
            "shared-span-a",
            "Shared bucket task",
            &event_a,
        );
        let mut span_b = test_task_span(
            &source_b,
            file_b,
            started_at + Duration::minutes(20),
            "shared-span-b",
            "Shared bucket task",
            &event_b,
        );
        let shared_project = ProjectInfo {
            project_id: "shared-project".to_string(),
            project_label: Some("shared-project".to_string()),
            repo_remote_hash: Some("shared-repo".to_string()),
            repo_label: Some("owner/shared".to_string()),
            branch_hash: Some("shared-branch".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("shared-path".to_string()),
            path_label: Some("/tmp/shared-project".to_string()),
        };
        span_a.project = Some(shared_project.clone());
        span_b.project = Some(shared_project);
        span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
        span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
        span_a.branch_family = branch_family(Some("main"));
        span_b.branch_family = branch_family(Some("main"));

        store
            .insert_events(&[event_a.clone(), event_b.clone()])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let mut updated_span_a = span_a.clone();
        updated_span_a.title = "Shared bucket task updated".to_string();
        updated_span_a.summary_preview = Some("Shared bucket task updated".to_string());
        updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

        let mut updated_span_b = span_b.clone();
        updated_span_b.summary_preview = Some("Shared bucket task follow-up".to_string());

        let pending_a = scan_file_state_entries(&[test_scan_candidate(file_a, "shared-a-2")]);
        let pending_b = scan_file_state_entries(&[test_scan_candidate(file_b, "shared-b-2")]);
        let mut preview = PreviewTaskRebuild::default();
        let rebuilt_a = preview
            .apply_source_changes(
                &store,
                SourceTaskChangeSet {
                    source_id: &source_a.source_id,
                    replace_source_records: false,
                    touched_files: true,
                    pending_file_entries: &pending_a,
                    removed_file_entries: &[],
                    task_spans: &[updated_span_a],
                },
            )
            .expect("preview rebuild a");
        let rebuilt_b = preview
            .apply_source_changes(
                &store,
                SourceTaskChangeSet {
                    source_id: &source_b.source_id,
                    replace_source_records: false,
                    touched_files: true,
                    pending_file_entries: &pending_b,
                    removed_file_entries: &[],
                    task_spans: &[updated_span_b],
                },
            )
            .expect("preview rebuild b");

        assert_eq!(rebuilt_a, 1);
        assert_eq!(rebuilt_b, 1);
        assert_eq!(rebuilt_a + rebuilt_b, 2);
        assert_eq!(store.work_items().expect("work items").len(), 1);
    }

    #[test]
    fn partial_scan_removes_stale_task_spans_and_rebuilds_work_items() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-partial-rescan"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-task-partial-rescan/a.jsonl";
        let file_b = "/tmp/codex-task-partial-rescan/b.jsonl";
        let initial_candidates = vec![
            test_scan_candidate(file_a, "sig-a-1"),
            test_scan_candidate(file_b, "sig-b-1"),
        ];
        let next_candidates = vec![
            test_scan_candidate(file_a, "sig-a-2"),
            test_scan_candidate(file_b, "sig-b-1"),
        ];
        let a_started_at = Utc
            .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
            .single()
            .expect("a_started_at");
        let b_started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
            .single()
            .expect("b_started_at");
        let event_a = test_scan_event(&source, file_a, a_started_at, "event-a", 100);
        let event_b = test_scan_event(&source, file_b, b_started_at, "event-b", 200);
        let mut span_a = test_task_span(
            &source,
            file_a,
            a_started_at,
            "span-a",
            "Implement task cleanup",
            &event_a,
        );
        let mut span_b = test_task_span(
            &source,
            file_b,
            b_started_at,
            "span-b",
            "Implement task benchmark reporting",
            &event_b,
        );
        span_a.session_id = Some("session-a".to_string());
        span_b.session_id = Some("session-b".to_string());

        let initial_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: initial_candidates,
            scan_result: statsai_adapters::AdapterScan {
                events: vec![event_a.clone(), event_b.clone()],
                task_spans: vec![span_a, span_b.clone()],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(initial_adapter)],
        )
        .expect("initial scan");

        assert_eq!(store.task_spans().expect("task spans").len(), 2);
        assert_eq!(store.work_items().expect("work items").len(), 2);

        let changed_only_adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: next_candidates,
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: true,
                preview: false,
                no_cache: false,
                replace: false,
                verbose: false,
                explain: false,
            },
            &store,
            "device-test",
            vec![Box::new(changed_only_adapter)],
        )
        .expect("partial scan");

        let spans = store.task_spans().expect("task spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].source_record_id.as_deref(), Some("span-b"));

        let work_items = store.work_items().expect("work items");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].title, span_b.title);
    }

    #[test]
    fn task_verify_split_merge_and_reject_survive_rebuilds() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-verify"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-verify/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 9, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "event-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(5),
            "event-b",
            120,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "span-a",
            "Implement task verification",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(5),
            "span-b",
            "Implement task verification",
            &event_b,
        );
        store
            .insert_events(&[event_a.clone(), event_b.clone()])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        assert_eq!(initial.len(), 1);

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Split {
                        work_item_id: initial[0].work_item_id.0.clone(),
                        after_span: span_a.span_id.0.clone(),
                        left_title: Some("Left investigation".to_string()),
                        right_title: Some("Right implementation".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("split verify");

        let split_items = store.work_items().expect("split work items");
        assert_eq!(split_items.len(), 2);
        assert!(split_items
            .iter()
            .all(|item| item.status == TaskStatus::Verified));
        assert!(split_items
            .iter()
            .any(|item| item.title == "Left investigation"));
        assert!(split_items
            .iter()
            .any(|item| item.title == "Right implementation"));

        let left = split_items
            .iter()
            .find(|item| item.title == "Left investigation")
            .expect("left work item");
        let right = split_items
            .iter()
            .find(|item| item.title == "Right implementation")
            .expect("right work item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Merge {
                        left_work_item_id: left.work_item_id.0.clone(),
                        right_work_item_id: right.work_item_id.0.clone(),
                        title: Some("Unified verification work".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("merge verify");

        let merged = store.work_items().expect("merged work items");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Unified verification work");
        assert_eq!(merged[0].status, TaskStatus::Verified);

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: merged[0].work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let rejected = store.work_items().expect("rejected work items");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].status, TaskStatus::RejectedMeta);
        assert_eq!(store.task_verifications().expect("verifications").len(), 3);
    }

    #[test]
    fn task_show_include_evidence_includes_spans_and_rename_verification() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-show"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-show/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "show-event", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "show-span",
            "Investigate work item evidence",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let initial_item = initial.first().expect("initial work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Rename {
                        work_item_id: initial_item.work_item_id.0.clone(),
                        title: "Verified evidence task".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("rename verify");

        let renamed = store.work_items().expect("renamed work items");
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].title, "Verified evidence task");
        assert_eq!(renamed[0].status, TaskStatus::Verified);

        let output = load_task_show_output(&store, &renamed[0].work_item_id, true)
            .expect("task show output");
        assert_eq!(output.work_item.title, "Verified evidence task");
        assert_eq!(output.spans.len(), 1);
        assert_eq!(output.verifications.len(), 1);
        assert!(matches!(
            output.verifications[0].action,
            TaskVerificationAction::Rename { .. }
        ));
    }

    #[test]
    fn rename_and_accept_coexist_for_same_anchor() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rename-accept"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-rename-accept/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "rename-accept-event", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "rename-accept-span",
            "Investigate rename and accept coexistence",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let work_item = initial.first().expect("initial work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Rename {
                        work_item_id: work_item.work_item_id.0.clone(),
                        title: "Hosted-verified task title".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("rename verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let rebuilt = store.work_items().expect("rebuilt work items");
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].status, TaskStatus::Verified);
        assert_eq!(rebuilt[0].title, "Hosted-verified task title");

        let verifications = store.task_verifications().expect("verifications");
        assert_eq!(verifications.len(), 2);
        assert!(verifications.iter().any(|verification| matches!(
            verification.action,
            TaskVerificationAction::Rename { .. }
        )));
        assert!(verifications.iter().any(|verification| matches!(
            verification.action,
            TaskVerificationAction::Accept { .. }
        )));
    }

    #[test]
    fn accept_after_reject_supersedes_manual_reject_for_same_anchor() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-verify-supersede"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-verify-supersede/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 12, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "supersede-event", 95);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "supersede-span",
            "Supersede conflicting verification actions",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let work_item = initial.first().expect("initial work item");
        let anchor_action_key = format!("status:{}", work_item.anchor_span_id.0);
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: work_item.work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let rebuilt = store.work_items().expect("rebuilt work items");
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].status, TaskStatus::Verified);
        assert!(!rebuilt[0]
            .review_reasons
            .iter()
            .any(|reason| reason.starts_with("manual_reject:")));

        let verifications = store.task_verifications().expect("verifications");
        assert_eq!(verifications.len(), 1);
        assert!(matches!(
            verifications[0].action,
            TaskVerificationAction::Accept { .. }
        ));
        assert_eq!(verifications[0].action_key, anchor_action_key);

        let output = load_task_show_output(&store, &rebuilt[0].work_item_id, true)
            .expect("task show output");
        assert_eq!(output.verifications.len(), 1);
        assert!(matches!(
            output.verifications[0].action,
            TaskVerificationAction::Accept { .. }
        ));
    }

    #[test]
    fn task_show_without_evidence_omits_spans_and_verifications() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-show-no-evidence"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-show-no-evidence/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "show-no-evidence", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "show-no-evidence-span",
            "Inspect task show output",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_item = store
            .work_items()
            .expect("work items")
            .into_iter()
            .next()
            .expect("work item");
        let output = load_task_show_output(&store, &work_item.work_item_id, false)
            .expect("task show output");
        assert_eq!(output.work_item.work_item_id, work_item.work_item_id);
        assert!(output.spans.is_empty());
        assert!(output.verifications.is_empty());
    }

    #[test]
    fn task_benchmark_reports_current_and_baselines() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 10, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "bench-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "bench-b",
            120,
        );
        let event_c = test_scan_event(
            &source,
            file_path,
            started_at + Duration::hours(30),
            "bench-c",
            20,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "bench-span-a",
            "Implement benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "bench-span-b",
            "Implement benchmark reporting",
            &event_b,
        );
        let span_c = test_task_span(
            &source,
            file_path,
            started_at + Duration::hours(30),
            "bench-span-c",
            "review uncommitted changes",
            &event_c,
        );
        store
            .insert_events(&[event_a, event_b, event_c])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_items = store.work_items().expect("work items");
        let implementation_item = work_items
            .iter()
            .find(|item| item.title == "Implement benchmark reporting")
            .expect("implementation item");
        let review_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_c.span_id)
            .expect("review item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: implementation_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: review_item.work_item_id.0.clone(),
                        reason: "noise".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.verified_spans >= 3);
        assert!(report.verified_adjacent_pairs >= 1);
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert_eq!(report.baselines.len(), 6);
        assert!(report.manual_constraints_preserved);
        assert_eq!(
            report.failing_baselines.is_empty(),
            report.beats_all_baselines
        );
        assert_eq!(report.shipping_gate_ready, report.gate_blockers.is_empty());
    }

    #[test]
    fn task_benchmark_scores_raw_grouper_not_manual_split_output() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-raw-grouper"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-raw-grouper/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "raw-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "raw-b",
            120,
        );
        let event_c = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(4),
            "raw-c",
            140,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "raw-span-a",
            "Implement benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "raw-span-b",
            "Implement benchmark reporting",
            &event_b,
        );
        let span_c = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(4),
            "raw-span-c",
            "Implement benchmark reporting",
            &event_c,
        );
        store
            .insert_events(&[event_a, event_b, event_c])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        assert_eq!(initial.len(), 1);
        let work_item = initial.first().expect("work item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Split {
                        work_item_id: work_item.work_item_id.0.clone(),
                        after_span: span_a.span_id.0.clone(),
                        left_title: Some("Investigate benchmark regression".to_string()),
                        right_title: Some("Implement benchmark reporting".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("split verify");

        let split_items = store.work_items().expect("split items");
        assert_eq!(split_items.len(), 2);

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(report.current.adjacent_f1 < 1.0);
    }

    #[test]
    fn task_benchmark_reports_missing_ground_truth_explicitly() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-empty"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-empty/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 12, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "bench-empty", 75);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "bench-empty-span",
            "Investigate benchmark readiness",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert_eq!(report.verified_spans, 0);
        assert_eq!(report.verified_adjacent_pairs, 0);
        assert!(!report.has_verified_ground_truth);
        assert!(!report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert!(report.failing_baselines.is_empty());
        assert_eq!(
            report.gate_blockers,
            vec!["missing_verified_ground_truth".to_string()]
        );

        let json = benchmark_json_value(&report);
        assert_eq!(json["has_verified_ground_truth"], json!(false));
        assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
        assert_eq!(json["shipping_gate_ready"], json!(false));
        assert_eq!(json["verified_spans"], json!(0));
        assert_eq!(json["failing_baselines"], json!([]));
        assert_eq!(
            json["gate_blockers"],
            json!(["missing_verified_ground_truth"])
        );
    }

    #[test]
    fn task_benchmark_reports_label_only_ground_truth_explicitly() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-label-only"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-task-benchmark-label-only/a.jsonl";
        let file_b = "/tmp/codex-task-benchmark-label-only/b.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 15, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_a, started_at, "label-only-a", 80);
        let event_b = test_scan_event(
            &source,
            file_b,
            started_at + Duration::minutes(30),
            "label-only-b",
            90,
        );
        let span_a = test_task_span(
            &source,
            file_a,
            started_at,
            "label-only-span-a",
            "Implement label-only benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_b,
            started_at + Duration::minutes(30),
            "label-only-span-b",
            "Clearing Conversation History",
            &event_b,
        );
        let mut span_a = span_a;
        let mut span_b = span_b;
        span_a.project = Some(ProjectInfo {
            project_id: "label-only-a".to_string(),
            project_label: Some("label-only-a".to_string()),
            repo_remote_hash: Some("repo-label-a".to_string()),
            repo_label: Some("owner/label-a".to_string()),
            branch_hash: Some("branch-label-a".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-label-a".to_string()),
            path_label: Some("/tmp/label-only-a".to_string()),
        });
        span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
        span_a.branch_family = branch_family(Some("main"));
        span_b.project = Some(ProjectInfo {
            project_id: "label-only-b".to_string(),
            project_label: Some("label-only-b".to_string()),
            repo_remote_hash: Some("repo-label-b".to_string()),
            repo_label: Some("owner/label-b".to_string()),
            branch_hash: Some("branch-label-b".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-label-b".to_string()),
            path_label: Some("/tmp/label-only-b".to_string()),
        });
        span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
        span_b.branch_family = branch_family(Some("main"));
        let span_a_id = span_a.span_id.clone();
        let span_b_id = span_b.span_id.clone();
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_items = store.work_items().expect("work items");
        let accepted_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_a_id)
            .expect("accepted item");
        let rejected_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_b_id)
            .expect("rejected item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: accepted_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: rejected_item.work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert_eq!(report.verified_spans, 2);
        assert_eq!(report.verified_adjacent_pairs, 0);
        assert!(report.has_verified_ground_truth);
        assert!(!report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert_eq!(report.failing_baselines, Vec::<String>::new());
        assert_eq!(
            report.gate_blockers,
            vec!["missing_pairwise_ground_truth".to_string()]
        );

        let json = benchmark_json_value(&report);
        assert_eq!(json["has_verified_ground_truth"], json!(true));
        assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
        assert_eq!(json["verified_spans"], json!(2));
        assert_eq!(json["verified_adjacent_pairs"], json!(0));
        assert_eq!(
            json["gate_blockers"],
            json!(["missing_pairwise_ground_truth"])
        );
    }

    #[test]
    fn task_benchmark_reports_failing_baselines_when_current_ties_them() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-baseline-tie"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-baseline-tie/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 16, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "tie-a", 80);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "tie-b",
            90,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "tie-span-a",
            "Implement benchmark blocking report",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "tie-span-b",
            "Implement benchmark blocking report",
            &event_b,
        );
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_item = store
            .work_items()
            .expect("work items")
            .into_iter()
            .next()
            .expect("work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert_eq!(
            report.failing_baselines,
            vec![
                "gap_only_2h".to_string(),
                "gap_only_6h".to_string(),
                "gap_only_12h".to_string(),
                "gap_only_24h".to_string(),
                "repo_plus_title".to_string(),
                "repo_plus_branch_plus_title".to_string(),
            ]
        );
        assert_eq!(
            report.gate_blockers,
            vec!["baseline_regressions".to_string()]
        );
    }

    #[test]
    fn task_list_filters_by_provider_and_status() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-list-filters"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-list-filters/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
            .single()
            .expect("started_at");
        let event_auto = test_scan_event(&source, file_path, started_at, "event-auto", 50);
        let event_review = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(10),
            "event-review",
            60,
        );
        let event_reject = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(20),
            "event-reject",
            70,
        );
        let mut span_auto = test_task_span(
            &source,
            file_path,
            started_at,
            "span-auto",
            "Implement task list filters",
            &event_auto,
        );
        let mut span_review = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(10),
            "span-review",
            "Review task list filtering behavior",
            &event_review,
        );
        let mut span_reject = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(20),
            "span-reject",
            "Noise task entry",
            &event_reject,
        );
        span_auto.project = Some(ProjectInfo {
            project_id: "project-auto".to_string(),
            project_label: Some("auto".to_string()),
            repo_remote_hash: Some("repo-auto".to_string()),
            repo_label: Some("owner/auto".to_string()),
            branch_hash: Some("branch-auto".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-auto".to_string()),
            path_label: Some("/tmp/project-auto".to_string()),
        });
        span_auto.project_bucket = project_bucket_key(span_auto.project.as_ref());
        span_auto.branch_family = branch_family(Some("main"));

        span_review.provider = "opencode".to_string();
        span_review.project = Some(ProjectInfo {
            project_id: "project-review".to_string(),
            project_label: Some("review".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-review".to_string()),
            path_label: Some("/tmp/project-review".to_string()),
        });
        span_review.project_bucket = project_bucket_key(span_review.project.as_ref());

        span_reject.project = Some(ProjectInfo {
            project_id: "project-reject".to_string(),
            project_label: Some("reject".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-reject".to_string()),
            path_label: Some("/tmp/project-reject".to_string()),
        });
        span_reject.project_bucket = project_bucket_key(span_reject.project.as_ref());

        store
            .insert_events(&[event_auto, event_review, event_reject])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_auto.clone(), span_review.clone(), span_reject.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("work items");
        let reject_item = initial
            .iter()
            .find(|item| item.anchor_span_id == span_reject.span_id)
            .expect("reject item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: reject_item.work_item_id.0.clone(),
                        reason: "noise".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let codex_items =
            filtered_task_list_items(&store, Some("codex"), None).expect("codex filtered items");
        assert_eq!(codex_items.len(), 1);
        assert!(codex_items
            .iter()
            .all(|item| item.providers.iter().any(|provider| provider == "codex")));
        assert!(codex_items
            .iter()
            .all(|item| item.status != TaskStatus::RejectedMeta));

        let auto_items = filtered_task_list_items(&store, None, Some(&TaskStatus::Auto))
            .expect("auto filtered items");
        assert_eq!(auto_items.len(), 1);
        assert_eq!(auto_items[0].anchor_span_id, span_auto.span_id);

        let rejected_items =
            filtered_task_list_items(&store, None, Some(&TaskStatus::RejectedMeta))
                .expect("rejected filtered items");
        assert_eq!(rejected_items.len(), 1);
        assert_eq!(rejected_items[0].anchor_span_id, span_reject.span_id);

        let default_selection = task_list_selection(&store, None, None).expect("default selection");
        assert_eq!(default_selection.items.len(), 2);
        assert_eq!(default_selection.hidden_rejected_meta, 1);
    }

    #[test]
    fn format_task_list_item_appends_review_reasons_when_present() {
        let ended_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut work_item = WorkItem {
            schema_version: "work_item.v1".to_string(),
            work_item_id: WorkItemId("work-review".to_string()),
            anchor_span_id: statsai_core::TaskSpanId("span-review".to_string()),
            tail_span_id: statsai_core::TaskSpanId("span-review".to_string()),
            project_bucket: "bucket".to_string(),
            title: "Reviewable item".to_string(),
            normalized_title: "reviewable item".to_string(),
            status: TaskStatus::NeedsReview,
            confidence: Confidence::Low,
            started_at: ended_at - Duration::minutes(5),
            ended_at,
            duration_seconds: Some(300),
            span_count: 1,
            event_count: 0,
            total_input_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_output_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: None,
            providers: vec!["claude_code".to_string()],
            issue_keys: Vec::new(),
            repo_label: None,
            branch_labels: Vec::new(),
            path_label: None,
            summary_preview: None,
            todo_excerpt: None,
            no_git: false,
            cross_provider: false,
            continuation_reasons: Vec::new(),
            review_reasons: vec!["no_usage_evidence".to_string(), "generic_title".to_string()],
        };

        let line = format_task_list_item(&work_item);
        assert!(line.contains("review=no_usage_evidence,generic_title"));

        work_item.review_reasons.clear();
        let clean_line = format_task_list_item(&work_item);
        assert!(!clean_line.contains("review="));
    }

    #[test]
    fn selected_rebuild_project_buckets_filter_by_provider_and_source() {
        let store = Store::in_memory().expect("store");
        let source_codex = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild-filter-a"),
            LocationOrigin::Configured,
        );
        let source_open = SourceLocation::local_adapter(
            "opencode",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild-filter-b"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source_codex).expect("codex source");
        store.upsert_source(&source_open).expect("opencode source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 15, 9, 0, 0)
            .single()
            .expect("started_at");
        let event_codex = test_scan_event(
            &source_codex,
            "/tmp/codex-task-rebuild-filter-a/session.jsonl",
            started_at,
            "event-codex",
            50,
        );
        let event_open = test_scan_event(
            &source_open,
            "/tmp/codex-task-rebuild-filter-b/session.jsonl",
            started_at + Duration::minutes(5),
            "event-open",
            60,
        );
        let span_codex = test_task_span(
            &source_codex,
            "/tmp/codex-task-rebuild-filter-a/session.jsonl",
            started_at,
            "span-codex",
            "Codex rebuild target",
            &event_codex,
        );
        let mut span_open = test_task_span(
            &source_open,
            "/tmp/codex-task-rebuild-filter-b/session.jsonl",
            started_at + Duration::minutes(5),
            "span-open",
            "OpenCode rebuild target",
            &event_open,
        );
        span_open.provider = "opencode".to_string();

        store
            .insert_events(&[event_codex, event_open])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_codex.clone(), span_open.clone()])
            .expect("insert spans");

        let codex_buckets =
            selected_rebuild_project_buckets(&store, Some("codex"), None).expect("codex buckets");
        assert_eq!(
            codex_buckets,
            BTreeSet::from([span_codex.project_bucket.clone()])
        );

        let open_buckets = selected_rebuild_project_buckets(&store, Some("opencode"), None)
            .expect("opencode buckets");
        assert_eq!(
            open_buckets,
            BTreeSet::from([span_open.project_bucket.clone()])
        );

        let source_buckets =
            selected_rebuild_project_buckets(&store, None, Some(&source_codex.source_id.0))
                .expect("source buckets");
        assert_eq!(
            source_buckets,
            BTreeSet::from([span_codex.project_bucket.clone()])
        );
    }

    #[test]
    fn task_status_and_verdict_parsers_reject_unknown_values() {
        assert_eq!(
            parse_task_status_filter("verified").expect("verified status"),
            TaskStatus::Verified
        );
        assert_eq!(
            parse_task_verdict("noise").expect("noise verdict"),
            TaskVerdict::Noise
        );
        assert!(parse_task_status_filter("mystery").is_err());
        assert!(parse_task_verdict("mystery").is_err());
    }

    #[test]
    fn stats_json_value_exposes_expected_fields() {
        let stats = statsai_store::TaskStats {
            total_spans: 10,
            total_work_items: 3,
            verified_percentage: 25.0,
            no_git_percentage: 50.0,
            cross_provider_percentage: 10.0,
            rejected_meta_percentage: 5.0,
            average_spans_per_work_item: 3.33,
        };

        let json = stats_json_value(&stats);
        assert_eq!(json["total_spans"], json!(10));
        assert_eq!(json["total_work_items"], json!(3));
        assert_eq!(json["verified_percentage"], json!(25.0));
        assert_eq!(json["no_git_percentage"], json!(50.0));
        assert_eq!(json["cross_provider_percentage"], json!(10.0));
        assert_eq!(json["rejected_meta_percentage"], json!(5.0));
        assert_eq!(json["average_spans_per_work_item"], json!(3.33));
    }

    #[test]
    fn sync_batch_serialization_excludes_local_task_entities() {
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch-test".to_string(),
            device_id: "device-test".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            authoritative_snapshot: None,
            created_at: Utc
                .with_ymd_and_hms(2026, 6, 14, 13, 0, 0)
                .single()
                .expect("created_at"),
        };

        let value = serde_json::to_value(&batch).expect("serialize sync batch");
        assert!(value.get("task_buckets").is_none());
        assert!(value.get("task_verifications").is_none());
    }

    #[test]
    fn task_rebuild_is_idempotent() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-rebuild/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "rebuild-a", 75);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(3),
            "rebuild-b",
            60,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "rebuild-span-a",
            "Rebuild task work items",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(3),
            "rebuild-span-b",
            "Rebuild task work items",
            &event_b,
        );
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let first = store.work_items().expect("first rebuild work items");
        task(
            TaskCommand {
                command: TaskSubcommand::Rebuild {
                    provider: None,
                    source_id: None,
                    all: true,
                },
            },
            &store,
        )
        .expect("first task rebuild");
        let second = store.work_items().expect("second rebuild work items");
        task(
            TaskCommand {
                command: TaskSubcommand::Rebuild {
                    provider: None,
                    source_id: None,
                    all: true,
                },
            },
            &store,
        )
        .expect("second task rebuild");
        let third = store.work_items().expect("third rebuild work items");

        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    #[test]
    fn partial_scan_with_legacy_rows_falls_back_to_full_source_reconcile() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-partial-legacy"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-partial-legacy/a.jsonl";
        let file_b = "/tmp/codex-partial-legacy/b.jsonl";
        let tracked_entries = vec![
            ScanFileStateEntry {
                cache_key: file_a.to_string(),
                cache_signature: "sig-a-1".to_string(),
            },
            ScanFileStateEntry {
                cache_key: file_b.to_string(),
                cache_signature: "sig-b-1".to_string(),
            },
        ];
        store
            .record_scan_file_entries(&source.source_id, &tracked_entries)
            .expect("record initial cache");

        let a_started_at = Utc
            .with_ymd_and_hms(2026, 5, 3, 10, 0, 0)
            .single()
            .expect("a_started_at");
        let b_started_at = Utc
            .with_ymd_and_hms(2026, 5, 4, 10, 0, 0)
            .single()
            .expect("b_started_at");
        let legacy_event_a =
            test_event("codex", &source, a_started_at, None, TokenParts::total(50));
        let legacy_event_b =
            test_event("codex", &source, b_started_at, None, TokenParts::total(75));
        let mut legacy_summary_a = test_summary("codex", &source, a_started_at, 50, None);
        legacy_summary_a.summary_id = summary_id("codex", &source.source_id, "legacy-summary-a");
        let mut legacy_summary_b = test_summary("codex", &source, b_started_at, 75, None);
        legacy_summary_b.summary_id = summary_id("codex", &source.source_id, "legacy-summary-b");
        store
            .insert_events(&[legacy_event_a, legacy_event_b])
            .expect("seed legacy events");
        store
            .upsert_summaries(&[legacy_summary_a, legacy_summary_b])
            .expect("seed legacy summaries");

        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: vec![
                test_scan_candidate(file_a, "sig-a-2"),
                test_scan_candidate(file_b, "sig-b-1"),
            ],
            scan_result: statsai_adapters::AdapterScan {
                events: vec![test_scan_event(
                    &source,
                    file_b,
                    b_started_at,
                    "event-b",
                    125,
                )],
                summaries: vec![test_scan_summary(
                    &source,
                    file_b,
                    b_started_at,
                    "summary-b",
                    125,
                )],
                ..statsai_adapters::AdapterScan::default()
            },
            probe_result: None,
            scan_calls: None,
        };

        scan_with_adapters(
            ScanCommand {
                provider: None,
                include_tasks: false,
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
        .expect("reconcile scan");

        let events = store.events_for_source(&source.source_id).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .parse_evidence
                .as_ref()
                .and_then(|evidence| evidence.source_record_id.as_deref()),
            Some("event-b")
        );
        let summaries = store
            .summaries_for_source(&source.source_id)
            .expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].summary_id,
            summary_id("codex", &source.source_id, "summary-b")
        );
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

        let local = sync_local_verify(&store, "http", &target, None, false).expect("local verify");
        assert_eq!(local.pending_sources, 0);
        assert_eq!(local.pending_accounts, 0);
        assert_eq!(local.pending_source_account_assignments, 0);
        assert_eq!(local.pending_subscriptions, 0);
        assert_eq!(local.total_passthrough_summaries, 0);
        assert_eq!(local.pending_passthrough_summaries, 0);
    }

    #[test]
    fn sync_local_verify_uses_sanitized_rollup_hashes() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sanitized-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let mut event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(42),
        );
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let target = "https://api.example.com/api/sync/batches".to_string();
        let rollups: Vec<_> = store
            .all_sync_rollup_summaries()
            .expect("rollups")
            .into_iter()
            .map(sanitize_summary_for_sync)
            .collect();
        assert_eq!(rollups.len(), 1);
        assert_eq!(
            rollups[0]
                .project
                .as_ref()
                .and_then(|project| project.path_label.as_deref()),
            Some("/Users/example/work/ai-stats")
        );
        assert!(rollups[0].privacy.contains_file_paths);
        store
            .record_summaries_synced("http", &target, &rollups)
            .expect("record rollups");

        let local = sync_local_verify(&store, "http", &target, None, true).expect("local verify");
        assert_eq!(local.total_rollups, 1);
        assert_eq!(local.pending_rollups, 0);
    }

    #[test]
    fn sync_local_verify_respects_project_sync_opt_in() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verify-project-opt-in"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let mut event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(42),
        );
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let target = "https://api.example.com/api/sync/batches".to_string();
        let rollups: Vec<_> = store
            .all_sync_rollup_summaries()
            .expect("rollups")
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, false))
            .collect();
        store
            .record_summaries_synced("http", &target, &rollups)
            .expect("record rollups");

        let hidden = sync_local_verify(&store, "http", &target, None, false)
            .expect("local verify without projects");
        let opted_in = sync_local_verify(&store, "http", &target, None, true)
            .expect("local verify with projects");

        assert_eq!(hidden.pending_rollups, 0);
        assert_eq!(opted_in.pending_rollups, 1);
    }

    #[test]
    fn build_sync_batch_respects_project_and_task_opt_ins() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-project-sync-opt-in"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
            .single()
            .expect("now");
        let mut event = test_event("codex", &source, now, None, TokenParts::total(120));
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");

        let mut summary = test_summary("codex", &source, now, 120, None);
        summary.project = event.project.clone();
        store.upsert_summary(&summary).expect("summary");

        let task_batch = test_task_only_sync_batch(now, 1, 1);
        for bucket in &task_batch.task_buckets {
            store
                .replace_task_bucket_snapshot(bucket)
                .expect("seed task bucket");
        }
        for verification in &task_batch.task_verifications {
            store
                .merge_task_verification(verification)
                .expect("seed task verification");
        }

        let default_command = test_sync_command("file");
        let default_target = sync_target(&default_command).expect("default target");
        let (default_batch, default_mode) =
            build_sync_batch(&default_command, &store, "device", &default_target)
                .expect("default batch");
        assert_eq!(default_mode, SyncPayloadMode::Raw);
        assert_eq!(default_batch.events.len(), 1);
        assert!(default_batch.events[0].project.is_none());
        assert_eq!(default_batch.summaries.len(), 1);
        assert!(default_batch.summaries[0].project.is_none());
        assert!(default_batch.task_buckets.is_empty());
        assert!(default_batch.task_verifications.is_empty());

        let project_opt_in_command = SyncCommand {
            include_projects: true,
            ..test_sync_command("file")
        };
        let project_opt_in_target =
            sync_target(&project_opt_in_command).expect("project opt-in target");
        let (project_opt_in_batch, project_opt_in_mode) = build_sync_batch(
            &project_opt_in_command,
            &store,
            "device",
            &project_opt_in_target,
        )
        .expect("project opt-in batch");
        assert_eq!(project_opt_in_mode, SyncPayloadMode::Raw);
        assert_eq!(project_opt_in_batch.events.len(), 1);
        assert!(project_opt_in_batch.events[0].project.is_some());
        assert_eq!(project_opt_in_batch.summaries.len(), 1);
        assert!(project_opt_in_batch.summaries[0].project.is_some());
        assert!(project_opt_in_batch.task_buckets.is_empty());
        assert!(project_opt_in_batch.task_verifications.is_empty());

        store
            .set_sync_preferences(SyncPreferences {
                include_projects: true,
                include_tasks: false,
            })
            .expect("persist sync preferences");
        let (persisted_batch, persisted_mode) =
            build_sync_batch(&default_command, &store, "device", &default_target)
                .expect("persisted batch");
        assert_eq!(persisted_mode, SyncPayloadMode::Raw);
        assert!(persisted_batch.events[0].project.is_some());
        assert!(persisted_batch.summaries[0].project.is_some());
        assert!(persisted_batch.task_buckets.is_empty());
        assert!(persisted_batch.task_verifications.is_empty());

        let task_opt_in_command = SyncCommand {
            include_tasks: true,
            ..test_sync_command("file")
        };
        let task_opt_in_target = sync_target(&task_opt_in_command).expect("task opt-in target");
        let (task_opt_in_batch, task_opt_in_mode) =
            build_sync_batch(&task_opt_in_command, &store, "device", &task_opt_in_target)
                .expect("task opt-in batch");
        assert_eq!(task_opt_in_mode, SyncPayloadMode::Raw);
        assert!(task_opt_in_batch.events[0].project.is_some());
        assert!(task_opt_in_batch.summaries[0].project.is_some());
        assert_eq!(task_opt_in_batch.task_buckets.len(), 1);
        assert_eq!(task_opt_in_batch.task_verifications.len(), 1);
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
            .record_sync_success("http", target, "batch_1", &[], &[], None)
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
                None,
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
            full: false,
            since_last: false,
            status: false,
            verify: false,
            reset_remote: false,
            yes: false,
            dry_run: false,
            include_projects: false,
            exclude_projects: false,
            include_tasks: false,
            exclude_tasks: false,
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
                reasoning_level: None,
                reasoning_level_raw: None,
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

    fn test_scan_candidate(path: &str, cache_signature: &str) -> ScanCandidateFile {
        ScanCandidateFile {
            path: PathBuf::from(path),
            cache_key: path.to_string(),
            cache_signature: cache_signature.to_string(),
            compatible_cache_signatures: Vec::new(),
        }
    }

    fn test_scan_event(
        source: &SourceLocation,
        file_path: &str,
        started_at: DateTime<Utc>,
        record_id: &str,
        total_tokens: u64,
    ) -> UsageEvent {
        let mut event = test_event(
            "codex",
            source,
            started_at,
            None,
            TokenParts::total(total_tokens),
        );
        event.source.source_record_id = Some(record_id.to_string());
        event.parse_evidence = Some(ParseEvidence {
            event_key_version: "test-scan.v1".to_string(),
            source_file_path_hash: Some(hash_text(file_path)),
            source_line_number: Some(1),
            source_record_id: Some(record_id.to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unresolved,
        });
        event
    }

    fn test_scan_summary(
        source: &SourceLocation,
        file_path: &str,
        observed_at: DateTime<Utc>,
        record_id: &str,
        total_tokens: u64,
    ) -> UsageSummary {
        let mut summary = test_summary("codex", source, observed_at, total_tokens, None);
        summary.summary_id = summary_id("codex", &source.source_id, record_id);
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.source.source_type = "jsonl".to_string();
        summary.source.source_record_id = Some(record_id.to_string());
        summary.parse_evidence = Some(ParseEvidence {
            event_key_version: "test-scan-summary.v1".to_string(),
            source_file_path_hash: Some(hash_text(file_path)),
            source_line_number: None,
            source_record_id: Some(record_id.to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unresolved,
        });
        summary
    }

    fn test_task_span(
        source: &SourceLocation,
        file_path: &str,
        started_at: DateTime<Utc>,
        record_id: &str,
        title: &str,
        event: &UsageEvent,
    ) -> TaskSpan {
        TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: task_span_id("codex", &source.source_id, record_id),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            span_kind: "codex_task".to_string(),
            source_record_id: Some(record_id.to_string()),
            source_file_path_hash: Some(hash_text(file_path)),
            summary_id: None,
            session_id: Some("session-test".to_string()),
            thread_id: None,
            title: title.to_string(),
            normalized_title: normalize_task_title(title),
            title_source: Some("thread_name".to_string()),
            summary_preview: Some(title.to_string()),
            todo_excerpt: None,
            issue_keys: Vec::new(),
            branch_family: None,
            project_bucket: project_bucket_key(event.project.as_ref()),
            project: event.project.clone(),
            git: None,
            usage: event.usage.clone(),
            estimated_cost_usd: event.cost.estimated_api_equivalent_usd,
            event_count: 1,
            has_usage_evidence: true,
            total_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.total_messages)
                .unwrap_or(0),
            user_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.user_messages)
                .unwrap_or(0),
            assistant_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.assistant_messages)
                .unwrap_or(0),
            developer_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.developer_messages)
                .unwrap_or(0),
            linked_event_ids: vec![event.event_id.clone()],
            confidence: Confidence::High,
            is_meta: false,
            started_at,
            ended_at: Some(started_at),
            duration_seconds: Some(0),
        }
    }

    #[test]
    fn scan_rewrites_task_span_links_to_canonical_event_ids() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-link-rewrite"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-link-rewrite/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 20, 12, 0, 0)
            .single()
            .expect("started_at");
        let existing_event = test_scan_event(&source, file_path, started_at, "existing", 100);
        store
            .insert_event(&existing_event)
            .expect("insert existing event");

        let mut duplicate_event = existing_event.clone();
        duplicate_event.event_id =
            event_id("codex", &source.source_id, "duplicate", None, started_at);
        duplicate_event.source.source_record_id = Some("duplicate".to_string());
        if let Some(parse_evidence) = duplicate_event.parse_evidence.as_mut() {
            parse_evidence.source_record_id = Some("duplicate".to_string());
        }
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "duplicate-span",
            "Rewrite canonical task links",
            &duplicate_event,
        );

        let insert_result = store
            .insert_events_with_resolution(&[duplicate_event])
            .expect("insert duplicate event");
        assert_eq!(insert_result.inserted, 0);

        let mut spans = vec![span];
        rewrite_task_span_linked_event_ids(&mut spans, &insert_result.canonical_event_ids);
        store.upsert_task_spans(&spans).expect("upsert spans");

        let stored_spans = store.task_spans().expect("task spans");
        assert_eq!(stored_spans.len(), 1);
        assert_eq!(
            stored_spans[0].linked_event_ids,
            vec![existing_event.event_id.clone()]
        );
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
