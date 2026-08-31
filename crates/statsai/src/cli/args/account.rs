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
