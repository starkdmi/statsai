use clap::{Args, Parser, Subcommand};
use statsai::snapshot;
use std::path::PathBuf;
use std::str::FromStr;

mod account;
mod quota;
mod report;
mod scan;
mod source;
mod subscription;
mod sync;
mod task;

pub(crate) use account::*;
pub(crate) use quota::*;
pub(crate) use report::*;
pub(crate) use scan::*;
pub(crate) use source::*;
pub(crate) use subscription::*;
pub(crate) use sync::*;
pub(crate) use task::*;

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
pub(crate) struct ConversationCommand {
    #[command(subcommand)]
    pub(crate) command: ConversationSubcommand,
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
    #[command(about = "Migrate the store to this binary's schema and apply its pricing ruleset")]
    Migrate,
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
