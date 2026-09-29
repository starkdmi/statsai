use super::*;

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
    #[command(about = "Manage the manual weekly reset anchor for a Claude Code account")]
    WeeklyReset {
        #[command(subcommand)]
        command: WeeklyResetSubcommand,
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

#[derive(Debug, Subcommand)]
pub(crate) enum WeeklyResetSubcommand {
    #[command(about = "Store a manual weekly reset anchor")]
    Set {
        #[arg(long, help = "Provider name. Only claude_code is accepted")]
        provider: String,
        #[arg(
            long,
            help = "Account identity (label, email, provider user id, or provider account id)"
        )]
        account: String,
        #[arg(
            long,
            help = "Weekly reset instant as RFC3339 with an explicit offset or Z. Past and future instants are both accepted and stored as whole-second UTC"
        )]
        at: String,
    },
    #[command(about = "Show the manual weekly reset anchor and the cycle that contains now")]
    Show {
        #[arg(long, help = "Provider name. Only claude_code is accepted")]
        provider: String,
        #[arg(
            long,
            help = "Account identity (label, email, provider user id, or provider account id)"
        )]
        account: String,
    },
    #[command(about = "Remove the manual weekly reset anchor")]
    Clear {
        #[arg(long, help = "Provider name. Only claude_code is accepted")]
        provider: String,
        #[arg(
            long,
            help = "Account identity (label, email, provider user id, or provider account id)"
        )]
        account: String,
    },
}
