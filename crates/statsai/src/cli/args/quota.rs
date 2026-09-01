use super::*;

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
