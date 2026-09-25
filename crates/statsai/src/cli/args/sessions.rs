use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionSortArg {
    Started,
    Tokens,
    Cost,
    Duration,
    Messages,
}

impl SessionSortArg {
    pub(crate) fn store_sort(self) -> statsai_store::SessionSort {
        match self {
            Self::Started => statsai_store::SessionSort::Started,
            Self::Tokens => statsai_store::SessionSort::Tokens,
            Self::Cost => statsai_store::SessionSort::Cost,
            Self::Duration => statsai_store::SessionSort::Duration,
            Self::Messages => statsai_store::SessionSort::Messages,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct SessionsCommand {
    #[arg(
        long,
        help = "Range start (YYYY-MM-DD or RFC3339). Defaults to the last 7 days"
    )]
    pub(crate) from: Option<String>,
    #[arg(long, help = "Range end (YYYY-MM-DD or RFC3339). Defaults to now")]
    pub(crate) to: Option<String>,
    #[arg(long, help = "Provider filter")]
    pub(crate) provider: Option<String>,
    #[arg(long, help = "Project id, key, or label")]
    pub(crate) project: Option<String>,
    #[arg(long, value_enum, default_value = "started", help = "Sort descending")]
    pub(crate) sort: SessionSortArg,
    #[arg(long, default_value_t = 50, help = "Maximum sessions to print")]
    pub(crate) limit: usize,
    #[arg(long, help = "Output as JSON")]
    pub(crate) json: bool,
}
