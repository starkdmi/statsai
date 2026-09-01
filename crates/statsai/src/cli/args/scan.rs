use super::*;

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
