use super::*;

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
