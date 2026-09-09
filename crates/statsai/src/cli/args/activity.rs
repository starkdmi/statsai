use super::*;

#[derive(Debug, Args)]
pub(crate) struct ActivityCommand {
    #[command(subcommand)]
    pub(crate) command: ActivitySubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ActivitySubcommand {
    #[command(about = "Show observed tool, MCP, and skill activity")]
    Status {
        #[arg(long, help = "Provider filter")]
        provider: Option<String>,
        #[arg(long, value_parser = ["tool", "mcp", "skill"], help = "Kind filter")]
        kind: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
}
