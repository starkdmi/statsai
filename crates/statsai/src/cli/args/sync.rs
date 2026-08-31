use super::*;

#[derive(Debug, Args)]
pub(crate) struct SyncCommand {
    #[arg(
        long,
        default_value = "stdout",
        help = "Sync sink (stdout, file, http)"
    )]
    pub(crate) sink: String,
    #[arg(long, help = "Output path for file sink")]
    pub(crate) output: Option<PathBuf>,
    #[arg(
        long,
        help = "HTTP endpoint for the http sink (defaults to STATSAI_API_URL/api/sync/batches)"
    )]
    pub(crate) endpoint: Option<String>,
    #[arg(long, help = "Bearer token override for the http sink")]
    pub(crate) auth_token: Option<String>,
    #[arg(
        long,
        help = "Rebuild local daily rollups from events and force all rollups dirty before sync"
    )]
    pub(crate) rebuild_rollups: bool,
    #[arg(
        long,
        help = "Force a full HTTP rollup sync even when this target was synced before"
    )]
    pub(crate) full: bool,
    #[arg(
        long,
        help = "Send only records after this sink target's last successful sync"
    )]
    pub(crate) since_last: bool,
    #[arg(long, help = "Show recorded sync state instead of sending")]
    pub(crate) status: bool,
    #[arg(
        long,
        help = "Inspect the resolved Cloudflare sync target and verify remote device access"
    )]
    pub(crate) verify: bool,
    #[arg(
        long,
        help = "Delete mirrored hosted sync data for this paired device and clear local sync tracking (http only)"
    )]
    pub(crate) reset_remote: bool,
    #[arg(long, help = "Confirm destructive sync reset actions")]
    pub(crate) yes: bool,
    #[arg(long, help = "Build the sync batch without writing")]
    pub(crate) dry_run: bool,
    #[arg(
        long,
        help = "Enable project metadata sync for this device and future syncs"
    )]
    pub(crate) include_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["include_projects", "include_tasks"],
        help = "Disable project metadata sync for this device and future syncs"
    )]
    pub(crate) exclude_projects: bool,
    #[arg(
        long,
        conflicts_with_all = ["exclude_tasks", "exclude_projects"],
        help = "Enable hosted task sync for this device and future syncs (implies --include-projects)"
    )]
    pub(crate) include_tasks: bool,
    #[arg(
        long,
        conflicts_with = "include_tasks",
        help = "Disable hosted task sync for this device and future syncs"
    )]
    pub(crate) exclude_tasks: bool,
}
