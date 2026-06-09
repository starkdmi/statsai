use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use statsai_core::home_dir;
use statsai_store::Store;
use std::path::{Path, PathBuf};

use crate::auth;
use crate::service;

const STALE_SYNC_HOURS: i64 = 12;
const DASHBOARD_OVERVIEW_CACHE_TTL_SECS: i64 = 300;

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
    pub tooltip: String,
    pub menu_layout: String,
    pub status_error: bool,
}

pub fn run(command: SnapshotCommand, store_path: &Path) -> Result<()> {
    let store = Store::open(store_path)?;
    let snapshot = collect(&store)?;

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
    let login = auth::login_snapshot()?;
    let _background = service::background_service_state()?;

    let http_target = http_sync_target();
    store.reconcile_sync_rollup_sync_hashes_if_needed()?;

    let http_sync = store
        .list_sync_states()?
        .into_iter()
        .find(|state| state.sink == "http" && state.target == http_target);

    let has_synced = http_sync.is_some();
    let last_sync_at = http_sync.as_ref().map(|state| state.last_success_at);
    let sync_failures = http_sync
        .as_ref()
        .map(|state| state.failure_count)
        .unwrap_or(0);

    let rollup_view =
        store.snapshot_rollup_view("http", &http_target, calendar_cutoff(7), calendar_cutoff(1))?;
    let has_pending_rollups = rollup_view.pending_count > 0;
    let pending_days = pending_upload_days(last_sync_at, has_pending_rollups);
    let mut today = period_stats_from_rollup(rollup_view.today);
    let pending_upload = login.logged_in
        && pending_upload_needed(has_pending_rollups, last_sync_at, today.requests > 0);

    let mut week = period_stats_from_rollup(rollup_view.week);
    if login.logged_in {
        match fetch_dashboard_period_stats_cached() {
            Ok((server_week, server_today)) => {
                week = server_week;
                today = server_today;
            }
            Err(error) => {
                eprintln!("snapshot: dashboard stats unavailable, using local rollups ({error:#})");
            }
        }
    }

    let backend_api = auth::cloudflare_api_url();
    let backend_web = auth::cloudflare_web_url();
    let using_local_dev = auth::is_local_backend();

    let tokens_today = today.tokens;
    let tokens_week = week.tokens;
    let sessions_week = week.requests;
    let cost_week_cents = week.cost_cents;

    let ui = build_ui(
        login.logged_in,
        has_synced,
        sync_failures,
        pending_upload,
        pending_days,
        week,
        today,
        last_sync_at,
        using_local_dev,
    );

    Ok(AppSnapshot {
        logged_in: login.logged_in,
        last_sync_at,
        sync_failures,
        has_synced,
        pending_upload,
        pending_days,
        unsynced_events: rollup_view.pending_count,
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
    last_sync_at: Option<DateTime<Utc>>,
    today_has_activity: bool,
) -> bool {
    if has_pending_rollups {
        return true;
    }
    let Some(last_sync_at) = last_sync_at else {
        return today_has_activity;
    };
    let stale = Utc::now().signed_duration_since(last_sync_at).num_hours() >= STALE_SYNC_HOURS;
    stale && today_has_activity
}

fn pending_upload_days(last_sync_at: Option<DateTime<Utc>>, has_pending_rollups: bool) -> u64 {
    if !has_pending_rollups {
        return 0;
    }
    let Some(last_sync_at) = last_sync_at else {
        return 1;
    };
    let hours = Utc::now()
        .signed_duration_since(last_sync_at)
        .num_hours()
        .max(0);
    if hours < 24 {
        return 0;
    }
    ((hours + 23) / 24) as u64
}

fn period_stats_from_rollup(usage: statsai_store::RollupPeriodStats) -> PeriodStats {
    PeriodStats {
        tokens: usage.tokens,
        requests: usage.requests,
        cost_cents: None,
    }
}

fn calendar_cutoff(days: u64) -> chrono::NaiveDate {
    let today = Utc::now().date_naive();
    today - Duration::days(days.saturating_sub(1) as i64)
}

#[derive(Debug, Deserialize)]
struct DashboardOverviewTotals {
    #[serde(rename = "totalTokens", default)]
    total_tokens: u64,
    #[serde(default)]
    requests: u64,
}

#[derive(Debug, Deserialize)]
struct DashboardOverviewDay {
    date: String,
    #[serde(rename = "totalTokens", default)]
    total_tokens: u64,
    #[serde(default)]
    requests: u64,
}

#[derive(Debug, Deserialize)]
struct DashboardOverviewResponse {
    totals: DashboardOverviewTotals,
    #[serde(rename = "daySeries", default)]
    day_series: Vec<DashboardOverviewDay>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DashboardOverviewCache {
    api_base_url: String,
    fetched_at: DateTime<Utc>,
    week_tokens: u64,
    week_requests: u64,
    today_tokens: u64,
    today_requests: u64,
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

fn load_dashboard_cache(
    api_base_url: &str,
    now: DateTime<Utc>,
) -> Option<(PeriodStats, PeriodStats)> {
    load_dashboard_cache_from_path(&dashboard_cache_path(), api_base_url, now)
}

fn load_dashboard_cache_from_path(
    path: &Path,
    api_base_url: &str,
    now: DateTime<Utc>,
) -> Option<(PeriodStats, PeriodStats)> {
    let file = std::fs::File::open(path).ok()?;
    let cache: DashboardOverviewCache = serde_json::from_reader(file).ok()?;
    if cache.api_base_url != api_base_url {
        return None;
    }
    if now.signed_duration_since(cache.fetched_at).num_seconds()
        >= DASHBOARD_OVERVIEW_CACHE_TTL_SECS
    {
        return None;
    }
    Some((
        PeriodStats {
            tokens: cache.week_tokens,
            requests: cache.week_requests,
            cost_cents: None,
        },
        PeriodStats {
            tokens: cache.today_tokens,
            requests: cache.today_requests,
            cost_cents: None,
        },
    ))
}

fn save_dashboard_cache(
    api_base_url: &str,
    fetched_at: DateTime<Utc>,
    week: &PeriodStats,
    today: &PeriodStats,
) -> Result<()> {
    let path = dashboard_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cache = DashboardOverviewCache {
        api_base_url: api_base_url.to_string(),
        fetched_at,
        week_tokens: week.tokens,
        week_requests: week.requests,
        today_tokens: today.tokens,
        today_requests: today.requests,
    };
    let file = std::fs::File::create(&path)?;
    serde_json::to_writer_pretty(file, &cache)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn fetch_dashboard_period_stats_cached() -> Result<(PeriodStats, PeriodStats)> {
    let api = auth::cloudflare_api_url();
    let now = Utc::now();
    if let Some(cached) = load_dashboard_cache(&api, now) {
        return Ok(cached);
    }
    let (week, today) = fetch_dashboard_period_stats()?;
    let _ = save_dashboard_cache(&api, now, &week, &today);
    Ok((week, today))
}

fn fetch_dashboard_period_stats() -> Result<(PeriodStats, PeriodStats)> {
    let token = auth::get_or_refresh_token()?
        .filter(|value| !value.trim().is_empty())
        .context("dashboard stats require login")?;
    let api = auth::cloudflare_api_url();
    let url = format!(
        "{}/api/dashboard/overview?range=7d&account=all",
        api.trim_end_matches('/')
    );
    let response = ureq::get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .with_context(|| format!("fetch dashboard overview from {url}"))?;
    let overview: DashboardOverviewResponse =
        response.into_json().context("decode dashboard overview")?;
    let today_key = Utc::now().format("%Y-%m-%d").to_string();
    let today = overview
        .day_series
        .iter()
        .find(|day| day.date == today_key)
        .map(|day| PeriodStats {
            tokens: day.total_tokens,
            requests: day.requests,
            cost_cents: None,
        })
        .unwrap_or(PeriodStats {
            tokens: 0,
            requests: 0,
            cost_cents: None,
        });
    Ok((
        PeriodStats {
            tokens: overview.totals.total_tokens,
            requests: overview.totals.requests,
            cost_cents: None,
        },
        today,
    ))
}

fn build_ui(
    logged_in: bool,
    has_synced: bool,
    sync_failures: u64,
    pending_upload: bool,
    pending_days: u64,
    week: PeriodStats,
    today: PeriodStats,
    last_sync_at: Option<DateTime<Utc>>,
    _using_local_dev: bool,
) -> UiCopy {
    let menu_stat_1 = format_week_line(&week);
    let menu_stat_2 = format_today_line(&today);

    if !logged_in {
        return UiCopy {
            menu_summary: "Your usage is tracked locally".to_string(),
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

    if week.requests == 0 {
        return UiCopy {
            menu_summary: "No usage found yet".to_string(),
            menu_stat_1: "This week · no requests yet".to_string(),
            menu_stat_2: "Today · no requests yet".to_string(),
            menu_stat_3: dashboard_line_synced(last_sync_at, has_synced, false),
            primary_action: PrimaryAction::None,
            tooltip: "StatsAI".to_string(),
            menu_layout: "no_data".to_string(),
        };
    }

    if pending_upload {
        let menu_summary = if pending_days > 1 {
            format!("Not uploaded in {pending_days} days")
        } else if pending_days == 1 {
            "Not uploaded since yesterday".to_string()
        } else {
            "New usage to upload".to_string()
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
        "This week · {} tokens · {} requests",
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
mod tests {
    use super::*;

    #[test]
    fn format_tokens_scales() {
        assert_eq!(format_tokens(853_000_000), "853M");
        assert_eq!(format_tokens(84_000), "84k");
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn stale_sync_with_today_activity_needs_upload() {
        assert!(pending_upload_needed(
            false,
            Some(Utc::now() - Duration::hours(45)),
            true,
        ));
    }

    #[test]
    fn fresh_sync_with_no_unsynced_does_not_need_upload() {
        assert!(!pending_upload_needed(
            false,
            Some(Utc::now() - Duration::hours(1)),
            true,
        ));
    }

    #[test]
    fn pending_rollups_need_upload() {
        assert!(pending_upload_needed(
            true,
            Some(Utc::now() - Duration::minutes(5)),
            false,
        ));
    }

    #[test]
    fn dashboard_cache_honors_ttl_and_api_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dashboard-overview-cache.json");
        let api = "http://127.0.0.1:8787";
        let now = Utc::now();
        let cache = DashboardOverviewCache {
            api_base_url: api.to_string(),
            fetched_at: now - Duration::seconds(30),
            week_tokens: 100,
            week_requests: 2,
            today_tokens: 10,
            today_requests: 1,
        };
        let file = std::fs::File::create(&path).expect("cache file");
        serde_json::to_writer_pretty(file, &cache).expect("write cache");

        assert!(load_dashboard_cache_from_path(&path, api, now).is_some());
        assert!(load_dashboard_cache_from_path(&path, "https://api.statsai.dev", now).is_none());
        assert!(load_dashboard_cache_from_path(
            &path,
            api,
            now + Duration::seconds(DASHBOARD_OVERVIEW_CACHE_TTL_SECS)
        )
        .is_none());
    }
}
