use super::*;

#[derive(Debug, Args)]
pub(crate) struct ReportCommand {
    #[command(subcommand)]
    pub(crate) command: ReportSubcommand,
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
    #[command(about = "Show prompt-cache reuse, suspected cache losses, and context per call")]
    Cache(CacheReportArgs),
}

#[derive(Debug, Args)]
pub(crate) struct CacheReportArgs {
    #[arg(
        long,
        help = "Range start (YYYY-MM-DD or RFC3339). Date-only values are UTC days. Defaults to the last 7 days"
    )]
    pub(crate) from: Option<String>,
    #[arg(long, help = "Range end (YYYY-MM-DD or RFC3339). Defaults to now")]
    pub(crate) to: Option<String>,
    #[arg(long, conflicts_with_all = ["from", "to"], help = "Report all stored history")]
    pub(crate) all: bool,
    #[arg(long, help = "Provider filter")]
    pub(crate) provider: Option<String>,
    #[arg(long, help = "Provider account id, or `unassigned`")]
    pub(crate) account: Option<String>,
    #[arg(long, help = "Session id (session_…) or a prefix of one")]
    pub(crate) session: Option<String>,
    #[arg(long, help = "Add local 10-minute buckets of calls")]
    pub(crate) timeline: bool,
    #[arg(long, help = "List every call with its verdict and evidence")]
    pub(crate) details: bool,
    #[arg(long, help = "Output as JSON")]
    pub(crate) json: bool,
}
