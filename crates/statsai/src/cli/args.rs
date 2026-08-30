use clap::{Args, Parser, Subcommand};
use statsai::snapshot;
use std::path::PathBuf;
use std::str::FromStr;

pub(crate) const MAX_SUBSCRIPTION_PRICE_CENTS: i64 = 100_000_000;

#[derive(Debug, Parser)]
#[command(
    name = "statsai",
    version,
    about = "Local-first AI usage stats CLI/SDK/daemon."
)]
pub(crate) struct Cli {
    #[arg(long, global = true, help = "Path to SQLite store")]
    pub(crate) store: Option<PathBuf>,
    #[arg(long, global = true, help = "Device identifier for multi-device sync")]
    pub(crate) device_id: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    #[command(about = "Scan local provider sources for usage events")]
    Scan(ScanCommand),
    #[command(about = "Show usage reports (weekly, monthly, all-time, or a date range)")]
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
    #[command(about = "Inspect reconstructed provider quota history")]
    Quota(QuotaCommand),
    #[command(about = "Build and inspect the local privacy-filtered dataset")]
    Privacy(statsai::privacy_cli::PrivacyCommand),
    #[command(about = "Export a sync batch to a sink")]
    Sync(SyncCommand),
    #[command(about = "Print JSON schemas for backend-facing contracts")]
    Schema(SchemaCommand),
    #[command(about = "Manage the local SQLite store")]
    Store(StoreAdminCommand),
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
pub(crate) struct ServiceCommand {
    #[command(subcommand)]
    pub(crate) command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceSubcommand {
    #[command(about = "Install a LaunchAgent that runs statsai daemon --watch")]
    Install,
    #[command(about = "Remove the background daemon LaunchAgent")]
    Uninstall,
    #[command(about = "Show LaunchAgent install and run state")]
    Status,
}

#[derive(Debug, Args)]
pub(crate) struct AuthCommand {
    #[command(subcommand)]
    pub(crate) command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AuthSubcommand {
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
pub(crate) struct ScanCommand {
    #[arg(long, help = "Scan only this provider")]
    pub(crate) provider: Option<String>,
    #[arg(long, help = "Collect local task spans and rebuild work items")]
    pub(crate) include_tasks: bool,
    #[arg(long, help = "Preview without persisting to the store")]
    pub(crate) preview: bool,
    #[arg(
        long,
        help = "Ignore the scan file cache and reparse all candidate files"
    )]
    pub(crate) no_cache: bool,
    #[arg(
        long,
        help = "Replace existing events for scanned sources before inserting"
    )]
    pub(crate) replace: bool,
    #[arg(long, help = "Show detailed per-source diagnostics")]
    pub(crate) verbose: bool,
    #[arg(long, help = "Show parse evidence for each event")]
    pub(crate) explain: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ReportCommand {
    #[command(subcommand)]
    pub(crate) command: ReportSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct TaskCommand {
    #[command(subcommand)]
    pub(crate) command: TaskSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct ConversationCommand {
    #[command(subcommand)]
    pub(crate) command: ConversationSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct QuotaCommand {
    #[command(subcommand)]
    pub(crate) command: QuotaSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum QuotaSubcommand {
    #[command(about = "Show quota collection and account-attribution coverage")]
    Status {
        #[arg(long, help = "Account id, email, provider id, or label")]
        account: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Show the newest activated quota window")]
    Current {
        #[arg(long, help = "Account id, email, provider id, or label")]
        account: Option<String>,
        #[arg(long, help = "Include shorter quota scopes")]
        all_scopes: bool,
        #[arg(long, help = "Include older epochs that overlap the selected window")]
        include_overlaps: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "List reconstructed quota windows")]
    Windows {
        #[arg(long, help = "Provider filter")]
        provider: Option<String>,
        #[arg(long, help = "Account id, email, provider id, or label")]
        account: Option<String>,
        #[arg(long, help = "Observation range start (date or RFC 3339)")]
        from: Option<String>,
        #[arg(long, help = "Observation range end (date or RFC 3339)")]
        to: Option<String>,
        #[arg(long, help = "Provider limit identity")]
        limit_id: Option<String>,
        #[arg(long, default_value_t = 50, help = "Maximum windows to return")]
        limit: usize,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Show quota change points for one reconstructed window")]
    History {
        #[arg(long, help = "Reconstructed quota window id; newest when omitted")]
        window_id: Option<String>,
        #[arg(long, help = "Include source observations and raw provider payloads")]
        raw: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(about = "Export quota observations, windows, or weekly sync projections")]
    Export {
        #[arg(long, value_parser = ["observations", "windows", "sync-windows"])]
        level: String,
        #[arg(long, value_parser = ["csv", "json", "jsonl"])]
        format: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConversationSubcommand {
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
pub(crate) enum TaskSubcommand {
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
pub(crate) enum TaskVerifySubcommand {
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
pub(crate) enum ReportSubcommand {
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
    #[command(about = "Show usage for an explicit date range")]
    Range {
        #[arg(
            long,
            required_unless_present = "to",
            help = "Range start (YYYY-MM-DD or RFC3339). Date-only values are UTC calendar days starting at 00:00:00 UTC"
        )]
        from: Option<String>,
        #[arg(
            long,
            required_unless_present = "from",
            help = "Range end (YYYY-MM-DD or RFC3339). Date-only values are UTC calendar days through 23:59:59 UTC. Defaults to now"
        )]
        to: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Show source paths and reasoning tokens")]
        verbose: bool,
        #[arg(long, help = "Include subscription-value rows")]
        subscriptions: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct SourceCommand {
    #[command(subcommand)]
    pub(crate) command: SourceSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SourceSubcommand {
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
pub(crate) struct AccountCommand {
    #[command(subcommand)]
    pub(crate) command: AccountSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AccountSubcommand {
    #[command(about = "List canonical provider accounts")]
    List,
    #[command(about = "Show detected plan evidence per account")]
    Plans {
        #[arg(long, help = "Provider name (claude_code, codex)")]
        provider: Option<String>,
        #[arg(
            long,
            help = "Account identity to show (label, email, provider user id, or provider account id)"
        )]
        account: Option<String>,
        #[arg(long, help = "Include every stored observation, not just the newest")]
        all: bool,
    },
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
pub(crate) struct SubscriptionCommand {
    #[command(subcommand)]
    pub(crate) command: SubscriptionSubcommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubscriptionPrice(i64);

impl SubscriptionPrice {
    pub(crate) const fn cents(self) -> i64 {
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
pub(crate) struct CurrencyCode(String);

impl CurrencyCode {
    pub(crate) fn into_string(self) -> String {
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
pub(crate) enum SubscriptionSubcommand {
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
pub(crate) struct ImportCommand {
    #[command(subcommand)]
    pub(crate) command: ImportSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ImportSubcommand {
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
pub(crate) struct ExportCommand {
    #[arg(long, help = "Export all events as JSON")]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SyncCommand {
    #[arg(
        long,
        default_value = "stdout",
        help = "Sync sink (stdout, file, http)"
    )]
    pub(crate) sink: String,
    #[arg(long, help = "Output path for file sink")]
    pub(crate) output: Option<PathBuf>,
    #[arg(
        long,
        help = "HTTP endpoint for the http sink (defaults to STATSAI_API_URL/api/sync/batches)"
    )]
    pub(crate) endpoint: Option<String>,
    #[arg(long, help = "Bearer token override for the http sink")]
    pub(crate) auth_token: Option<String>,
    #[arg(
        long,
        help = "Rebuild local daily rollups from events and force all rollups dirty before sync"
    )]
    pub(crate) rebuild_rollups: bool,
    #[arg(
        long,
        help = "Force a full HTTP rollup sync even when this target was synced before"
    )]
    pub(crate) full: bool,
    #[arg(
        long,
        help = "Send only records after this sink target's last successful sync"
    )]
    pub(crate) since_last: bool,
    #[arg(long, help = "Show recorded sync state instead of sending")]
    pub(crate) status: bool,
    #[arg(
        long,
        help = "Inspect the resolved Cloudflare sync target and verify remote device access"
    )]
    pub(crate) verify: bool,
    #[arg(
        long,
        help = "Delete mirrored hosted sync data for this paired device and clear local sync tracking (http only)"
    )]
    pub(crate) reset_remote: bool,
    #[arg(long, help = "Confirm destructive sync reset actions")]
    pub(crate) yes: bool,
    #[arg(long, help = "Build the sync batch without writing")]
    pub(crate) dry_run: bool,
    #[arg(
        long,
        help = "Enable project metadata sync for this device and future syncs"
    )]
    pub(crate) include_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["include_projects", "include_tasks"],
        help = "Disable project metadata sync for this device and future syncs"
    )]
    pub(crate) exclude_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["exclude_tasks", "exclude_projects"],
        help = "Enable hosted task sync for this device and future syncs (implies --include-projects)"
    )]
    pub(crate) include_tasks: bool,
    #[arg(
        long,
        conflicts_with = "include_tasks",
        help = "Disable hosted task sync for this device and future syncs"
    )]
    pub(crate) exclude_tasks: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SchemaCommand {
    #[command(subcommand)]
    pub(crate) command: SchemaSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct StoreAdminCommand {
    #[command(subcommand)]
    pub(crate) command: StoreAdminSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum StoreAdminSubcommand {
    #[command(about = "Create a consistent APFS clone of the SQLite store")]
    CloneTo {
        #[arg(value_name = "PATH", help = "Destination database path")]
        destination: PathBuf,
    },
    #[command(about = "Print the store schema version supported by this binary")]
    SupportedSchemaVersion,
    #[command(about = "Print the pricing ruleset version supported by this binary")]
    SupportedPricingRulesetVersion,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SchemaSubcommand {
    #[command(about = "Print the sync_batch.v4 JSON Schema")]
    SyncBatch,
    #[command(about = "Print the quota_window_sync_projection.v1 JSON Schema")]
    QuotaWindowProjection,
}

#[derive(Debug, Args)]
pub(crate) struct DaemonCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:8765",
        help = "Loopback address to bind the API"
    )]
    pub(crate) api: String,
    #[arg(long, help = "Enable file watching for automatic rescans")]
    pub(crate) watch: bool,
}
