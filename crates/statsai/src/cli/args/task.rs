use super::*;

#[derive(Debug, Args)]
pub(crate) struct TaskCommand {
    #[command(subcommand)]
    pub(crate) command: TaskSubcommand,
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
