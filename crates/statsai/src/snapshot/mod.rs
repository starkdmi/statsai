use anyhow::Result;
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use statsai_adapters::{
    default_adapters, CLAUDE_CODE_PROVIDER, CODEX_PROVIDER, GROK_BUILD_PROVIDER, OPENCODE_PROVIDER,
};
use statsai_core::home_dir;
use statsai_store::{SourceUsageTotals, Store};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::auth;
use crate::service;

#[derive(Debug, Args)]
pub struct SnapshotCommand {
    #[arg(long, help = "Print machine-readable JSON for the menu bar app")]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryAction {
    Link,
    UploadNow,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub logged_in: bool,
    #[serde(default)]
    pub first_run: bool,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub sync_failures: u64,
    pub has_synced: bool,
    pub pending_upload: bool,
    pub pending_days: u64,
    pub unsynced_events: u64,
    pub tokens_today: u64,
    pub tokens_week: u64,
    pub sessions_week: u64,
    pub cost_week_cents: Option<i64>,
    pub menu_summary: String,
    pub menu_stat_1: String,
    pub menu_stat_2: String,
    pub menu_stat_3: String,
    pub primary_action: PrimaryAction,
    pub backend_api: String,
    pub backend_web: String,
    pub using_local_dev: bool,
    #[serde(default)]
    pub background_tracking: SnapshotBackgroundStatus,
    #[serde(default)]
    pub sources: Vec<SnapshotSourceStatus>,
    #[serde(default)]
    pub last_scan_summary: Option<String>,
    #[serde(default)]
    pub help_url: String,
    #[serde(default)]
    pub setup_url: String,
    pub tooltip: String,
    pub menu_layout: String,
    pub status_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotBackgroundStatus {
    pub installed: bool,
    pub running: bool,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSourceStatus {
    pub provider: String,
    pub display_name: String,
    pub configured: bool,
    pub discovered: bool,
    pub enabled: bool,
    pub has_data: bool,
    pub event_count: u64,
    pub token_count: u64,
    pub estimated_cost_cents: Option<i64>,
    pub label: String,
    pub status: String,
}

pub fn run(command: SnapshotCommand, store_path: &Path, device_id: &str) -> Result<()> {
    let store = crate::open_operational_store(store_path)?;
    let snapshot = collect_for_device(&store, device_id)?;

    if command.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    println!("{}", snapshot.menu_summary);
    println!("{}", snapshot.menu_stat_1);
    println!("{}", snapshot.menu_stat_2);
    println!("{}", snapshot.menu_stat_3);
    Ok(())
}

pub fn collect(store: &Store) -> Result<AppSnapshot> {
    collect_for_device(store, &crate::default_device_id())
}

pub fn collect_for_device(store: &Store, device_id: &str) -> Result<AppSnapshot> {
    let login = auth::login_snapshot()?;
    let background = service::background_service_state()?;

    let http_target = http_sync_target();
    let sync_preferences = store.sync_preferences()?;
    store.reconcile_sync_rollup_sync_hashes_if_needed()?;
    let configured_sources = store.list_sources()?;
    let provider_totals = provider_totals_from_store(store, &configured_sources)?;
    let sources = build_source_statuses(&configured_sources, &provider_totals);

    let http_sync = store
        .list_sync_states()?
        .into_iter()
        .find(|state| state.sink == "http" && state.target == http_target);

    let has_synced = has_successful_sync(http_sync.as_ref());
    let last_sync_at = http_sync.as_ref().map(|state| state.last_success_at);
    let sync_failures = http_sync
        .as_ref()
        .map(|state| state.failure_count)
        .unwrap_or(0);

    let pending_sync = store.pending_http_sync_summary_counts_with_projects(
        &http_target,
        device_id,
        sync_preferences.include_projects,
    )?;
    let has_pending_upload = pending_sync.total > 0;
    let pending_days = pending_upload_days(last_sync_at, has_pending_upload, pending_sync.days);
    let local_day_start = local_period_start(1);
    let mut today =
        period_stats_from_rollup(store.usage_event_period_stats_since(local_day_start)?);
    today.add(period_stats_from_rollup(
        store.reportable_summary_period_stats_since(local_day_start)?,
    ));
    let pending_upload = login.logged_in
        && pending_upload_needed(has_pending_upload, last_sync_at, today.requests > 0);

    let local_week_start = local_period_start(7);
    let mut week =
        period_stats_from_rollup(store.usage_event_period_stats_since(local_week_start)?);
    week.add(period_stats_from_rollup(
        store.reportable_summary_period_stats_since(local_week_start)?,
    ));

    let backend_api = auth::cloudflare_api_url();
    let backend_web = auth::cloudflare_web_url();
    let using_local_dev = auth::is_local_backend();
    let help_url = help_url(&backend_web);
    let setup_url = setup_url(&backend_web);

    let tokens_today = today.tokens;
    let tokens_week = week.tokens;
    let sessions_week = week.requests;
    let cost_week_cents = week.cost_cents;
    let has_local_usage = week.requests > 0 || sources.iter().any(|source| source.has_data);
    let first_run = is_first_run(login.logged_in, has_synced, has_local_usage);
    let last_scan_summary = Some(format_scan_summary(&week, &today));

    let ui = build_ui(SnapshotUiInput {
        logged_in: login.logged_in,
        first_run,
        has_synced,
        sync_failures,
        pending_upload,
        pending_days,
        week,
        today,
        last_sync_at,
    });

    Ok(AppSnapshot {
        logged_in: login.logged_in,
        first_run,
        last_sync_at,
        sync_failures,
        has_synced,
        pending_upload,
        pending_days,
        unsynced_events: pending_sync.total,
        tokens_today,
        tokens_week,
        sessions_week,
        cost_week_cents,
        menu_summary: ui.menu_summary,
        menu_stat_1: ui.menu_stat_1,
        menu_stat_2: ui.menu_stat_2,
        menu_stat_3: ui.menu_stat_3,
        primary_action: ui.primary_action,
        backend_api,
        backend_web,
        using_local_dev,
        background_tracking: background_status(background),
        sources,
        last_scan_summary,
        help_url,
        setup_url,
        tooltip: ui.tooltip,
        menu_layout: ui.menu_layout,
        status_error: false,
    })
}

#[derive(Clone)]
struct PeriodStats {
    tokens: u64,
    requests: u64,
    cost_cents: Option<i64>,
}

impl PeriodStats {
    fn add(&mut self, other: PeriodStats) {
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.cost_cents = match (self.cost_cents, other.cost_cents) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };
    }
}

struct SnapshotUiInput {
    logged_in: bool,
    first_run: bool,
    has_synced: bool,
    sync_failures: u64,
    pending_upload: bool,
    pending_days: u64,
    week: PeriodStats,
    today: PeriodStats,
    last_sync_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct SourceStatusDraft {
    display_name: &'static str,
    configured: bool,
    discovered: bool,
    enabled: bool,
    event_count: u64,
    token_count: u64,
    estimated_cost_cents: Option<i64>,
}

fn background_status(background: service::BackgroundServiceState) -> SnapshotBackgroundStatus {
    let label = if background.stale {
        "Tracking needs restart".to_string()
    } else if background.launch_agent_loaded {
        "Tracking automatically".to_string()
    } else if background.plist_installed {
        "Tracking installed, starting up".to_string()
    } else {
        "Tracking setup needed".to_string()
    };
    SnapshotBackgroundStatus {
        installed: background.plist_installed || background.launch_agent_loaded,
        running: background.launch_agent_loaded && !background.stale,
        label,
    }
}

fn build_source_statuses(
    configured_sources: &[statsai_core::SourceLocation],
    provider_totals: &HashMap<String, SourceUsageTotals>,
) -> Vec<SnapshotSourceStatus> {
    let discovered_sources = default_adapters()
        .into_iter()
        .map(|adapter| (adapter.provider().to_string(), adapter.discover()))
        .collect::<Vec<_>>();
    build_source_statuses_with_discovered(configured_sources, provider_totals, discovered_sources)
}

fn build_source_statuses_with_discovered(
    configured_sources: &[statsai_core::SourceLocation],
    provider_totals: &HashMap<String, SourceUsageTotals>,
    discovered_sources: Vec<(String, Vec<statsai_core::SourceLocation>)>,
) -> Vec<SnapshotSourceStatus> {
    let mut drafts = supported_provider_drafts();
    let mut configured_source_ids = BTreeSet::new();

    for source in configured_sources {
        configured_source_ids.insert(source.source_id.0.clone());
        let draft = drafts
            .entry(source.provider.clone())
            .or_insert_with(|| SourceStatusDraft {
                display_name: provider_display_name(&source.provider),
                ..SourceStatusDraft::default()
            });
        draft.configured = true;
        draft.enabled |= source.enabled;
    }

    for (provider, sources) in discovered_sources {
        if sources.is_empty() {
            continue;
        }
        let draft = drafts
            .entry(provider.clone())
            .or_insert_with(|| SourceStatusDraft {
                display_name: provider_display_name(&provider),
                ..SourceStatusDraft::default()
            });
        draft.discovered = true;
        for source in sources {
            if !configured_source_ids.contains(&source.source_id.0) {
                draft.enabled = true;
            }
        }
    }

    for (provider, totals) in provider_totals {
        let draft = drafts
            .entry(provider.clone())
            .or_insert_with(|| SourceStatusDraft {
                display_name: provider_display_name(provider),
                ..SourceStatusDraft::default()
            });
        add_source_totals(draft, Some(totals));
    }

    let configured_providers = configured_sources
        .iter()
        .map(|source| source.provider.clone())
        .collect::<BTreeSet<_>>();

    let mut statuses = Vec::new();
    for provider in provider_order() {
        if let Some(draft) = drafts.remove(provider) {
            let status = source_status(provider, draft);
            if source_status_should_render(&status) {
                statuses.push(status);
            }
        }
    }
    for (provider, draft) in drafts {
        if configured_providers.contains(&provider) {
            let status = source_status(&provider, draft);
            if source_status_should_render(&status) {
                statuses.push(status);
            }
        }
    }
    statuses
}

fn source_status_should_render(status: &SnapshotSourceStatus) -> bool {
    status.has_data || status.configured || status.status == "disabled"
}

fn supported_provider_drafts() -> BTreeMap<String, SourceStatusDraft> {
    provider_order()
        .into_iter()
        .map(|provider| {
            (
                provider.to_string(),
                SourceStatusDraft {
                    display_name: provider_display_name(provider),
                    ..SourceStatusDraft::default()
                },
            )
        })
        .collect()
}

fn provider_order() -> Vec<&'static str> {
    vec![
        CODEX_PROVIDER,
        CLAUDE_CODE_PROVIDER,
        OPENCODE_PROVIDER,
        GROK_BUILD_PROVIDER,
    ]
}

fn source_status(provider: &str, draft: SourceStatusDraft) -> SnapshotSourceStatus {
    let has_data = draft.event_count > 0 || draft.token_count > 0;
    let status = if (draft.configured || draft.discovered) && !draft.enabled {
        "disabled"
    } else if has_data {
        "tracking"
    } else if draft.configured || draft.discovered {
        "found"
    } else {
        "not_found"
    };
    let label = match status {
        "disabled" => format!("{} · disabled", draft.display_name),
        "tracking" => format!(
            "{} · {} tokens · {}",
            draft.display_name,
            format_tokens(draft.token_count),
            format_source_cost(draft.estimated_cost_cents)
        ),
        "found" => format!("{} · 0 tokens · $0", draft.display_name),
        _ => draft.display_name.to_string(),
    };

    SnapshotSourceStatus {
        provider: provider.to_string(),
        display_name: draft.display_name.to_string(),
        configured: draft.configured,
        discovered: draft.discovered,
        enabled: draft.enabled,
        has_data,
        event_count: draft.event_count,
        token_count: draft.token_count,
        estimated_cost_cents: draft.estimated_cost_cents,
        label,
        status: status.to_string(),
    }
}

fn add_source_totals(draft: &mut SourceStatusDraft, totals: Option<&SourceUsageTotals>) {
    let Some(totals) = totals else {
        return;
    };
    draft.event_count = draft.event_count.saturating_add(totals.events);
    draft.token_count = draft.token_count.saturating_add(totals.tokens);
    draft.estimated_cost_cents = match (draft.estimated_cost_cents, totals.estimated_cost_cents) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    };
}

fn provider_totals_from_store(
    store: &Store,
    _configured_sources: &[statsai_core::SourceLocation],
) -> Result<HashMap<String, SourceUsageTotals>> {
    store.menu_usage_totals_by_provider()
}

fn provider_display_name(provider: &str) -> &'static str {
    match provider {
        CODEX_PROVIDER => "Codex",
        CLAUDE_CODE_PROVIDER => "Claude Code",
        OPENCODE_PROVIDER => "OpenCode",
        GROK_BUILD_PROVIDER => "Grok Build",
        _ => "Other",
    }
}

fn format_count(count: u64) -> String {
    if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

fn format_scan_summary(week: &PeriodStats, today: &PeriodStats) -> String {
    if week.requests == 0 {
        "Last scan found no requests yet".to_string()
    } else if today.requests > 0 {
        format!(
            "Last scan found {} requests today",
            format_count(today.requests)
        )
    } else {
        format!(
            "Last scan found {} requests in the last 7 days",
            format_count(week.requests)
        )
    }
}

fn help_url(web_base: &str) -> String {
    format!("{}/help/setup", web_base.trim_end_matches('/'))
}

fn setup_url(web_base: &str) -> String {
    format!("{}/dashboard/", web_base.trim_end_matches('/'))
}

struct UiCopy {
    menu_summary: String,
    menu_stat_1: String,
    menu_stat_2: String,
    menu_stat_3: String,
    primary_action: PrimaryAction,
    tooltip: String,
    menu_layout: String,
}

fn http_sync_target() -> String {
    format!(
        "{}/api/sync/batches",
        auth::cloudflare_api_url().trim_end_matches('/')
    )
}

fn pending_upload_needed(
    has_pending_rollups: bool,
    _last_sync_at: Option<DateTime<Utc>>,
    _today_has_activity: bool,
) -> bool {
    has_pending_rollups
}

fn has_successful_sync(sync: Option<&statsai_store::SyncState>) -> bool {
    sync.map(|state| !state.last_batch_id.trim().is_empty())
        .unwrap_or(false)
}

fn is_first_run(logged_in: bool, has_synced: bool, has_local_usage: bool) -> bool {
    !logged_in && !has_synced && !has_local_usage
}

fn pending_upload_days(
    last_sync_at: Option<DateTime<Utc>>,
    has_pending_upload: bool,
    pending_sync_days: u64,
) -> u64 {
    if !has_pending_upload {
        return 0;
    }
    let Some(last_sync_at) = last_sync_at else {
        return pending_sync_days.max(1);
    };
    let hours = Utc::now()
        .signed_duration_since(last_sync_at)
        .num_hours()
        .max(0);
    if hours < 24 {
        return 0;
    }
    pending_sync_days.max(((hours + 23) / 24) as u64)
}

fn period_stats_from_rollup(usage: statsai_store::RollupPeriodStats) -> PeriodStats {
    PeriodStats {
        tokens: usage.tokens,
        requests: usage.requests,
        cost_cents: None,
    }
}

fn local_period_start(days: u64) -> DateTime<Utc> {
    let now = Local::now();
    let start_day = now.date_naive() - Duration::days(days.saturating_sub(1) as i64);
    let Some(midnight) = start_day.and_hms_opt(0, 0, 0) else {
        return now.with_timezone(&Utc);
    };
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or(now)
        .with_timezone(&Utc)
}

pub fn invalidate_dashboard_cache() {
    let _ = std::fs::remove_file(dashboard_cache_path());
}

fn dashboard_cache_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".statsai")
        .join("dashboard-overview-cache.json")
}

fn build_ui(input: SnapshotUiInput) -> UiCopy {
    let SnapshotUiInput {
        logged_in,
        first_run,
        has_synced,
        sync_failures,
        pending_upload,
        pending_days,
        week,
        today,
        last_sync_at,
    } = input;

    let menu_stat_1 = format_week_line(&week);
    let menu_stat_2 = format_today_line(&today);

    if !logged_in {
        let menu_summary = if first_run {
            "StatsAI is tracking locally".to_string()
        } else {
            "Your usage is tracked locally".to_string()
        };
        return UiCopy {
            menu_summary,
            menu_stat_1,
            menu_stat_2,
            menu_stat_3: "Dashboard · not connected".to_string(),
            primary_action: PrimaryAction::Link,
            tooltip: "StatsAI".to_string(),
            menu_layout: "unlinked".to_string(),
        };
    }

    if sync_failures > 0 {
        return UiCopy {
            menu_summary: "Last upload didn't finish".to_string(),
            menu_stat_1,
            menu_stat_2,
            menu_stat_3: dashboard_line_synced(last_sync_at, has_synced, false),
            primary_action: PrimaryAction::UploadNow,
            tooltip: "StatsAI — upload needs attention".to_string(),
            menu_layout: "upload_issue".to_string(),
        };
    }

    if pending_upload {
        let menu_summary = if pending_days > 1 {
            format!("Dashboard sync {pending_days} days behind")
        } else if pending_days == 1 {
            "Dashboard sync behind since yesterday".to_string()
        } else {
            "Dashboard sync available".to_string()
        };
        return UiCopy {
            menu_summary,
            menu_stat_1,
            menu_stat_2,
            menu_stat_3: dashboard_line_synced(last_sync_at, has_synced, false),
            primary_action: PrimaryAction::UploadNow,
            tooltip: "StatsAI — ready to upload".to_string(),
            menu_layout: "pending_upload".to_string(),
        };
    }

    if week.requests == 0 {
        return UiCopy {
            menu_summary: "No usage found yet".to_string(),
            menu_stat_1: "Last 7 days · no requests yet".to_string(),
            menu_stat_2: "Today · no requests yet".to_string(),
            menu_stat_3: dashboard_line_synced(last_sync_at, has_synced, false),
            primary_action: PrimaryAction::None,
            tooltip: "StatsAI".to_string(),
            menu_layout: "no_data".to_string(),
        };
    }

    UiCopy {
        menu_summary: "Everything looks good".to_string(),
        menu_stat_1,
        menu_stat_2,
        menu_stat_3: dashboard_line_synced(last_sync_at, has_synced, false),
        primary_action: PrimaryAction::None,
        tooltip: "StatsAI — up to date".to_string(),
        menu_layout: "ready".to_string(),
    }
}

fn format_week_line(week: &PeriodStats) -> String {
    let mut line = format!(
        "Last 7 days · {} tokens · {} requests",
        format_tokens(week.tokens),
        week.requests
    );
    if let Some(cost) = format_cost(week.cost_cents) {
        line.push_str(" · ");
        line.push_str(&cost);
    }
    line
}

fn format_today_line(today: &PeriodStats) -> String {
    format!(
        "Today · {} tokens · {} requests",
        format_tokens(today.tokens),
        today.requests
    )
}

fn dashboard_line_synced(
    last_sync_at: Option<DateTime<Utc>>,
    has_synced: bool,
    upload_recommended: bool,
) -> String {
    let base = if let Some(at) = last_sync_at {
        format!("Dashboard · updated {}", format_relative_time(at))
    } else if has_synced {
        "Dashboard · updated".to_string()
    } else {
        "Dashboard · not uploaded yet".to_string()
    };
    if upload_recommended {
        format!("{base} · upload recommended")
    } else {
        base
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        format!("{:.0}B", tokens as f64 / 1_000_000_000.0)
    } else if tokens >= 1_000_000 {
        format!("{:.0}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_cost(cents: Option<i64>) -> Option<String> {
    let cents = cents?;
    if cents <= 0 {
        return None;
    }
    if cents % 100 == 0 {
        Some(format!("${}", cents / 100))
    } else {
        Some(format!("${:.2}", cents as f64 / 100.0))
    }
}

fn format_source_cost(cents: Option<i64>) -> String {
    format_cost(cents).unwrap_or_else(|| "cost n/a".to_string())
}

fn format_relative_time(at: DateTime<Utc>) -> String {
    let ago = Utc::now().signed_duration_since(at);
    if ago.num_seconds() < 60 {
        return "just now".to_string();
    }
    if ago.num_minutes() < 60 {
        return format!("{} min ago", ago.num_minutes());
    }
    if ago.num_hours() < 48 {
        return format!("{} hr ago", ago.num_hours());
    }
    format!("on {}", at.format("%b %-d"))
}

#[cfg(test)]
mod tests;
