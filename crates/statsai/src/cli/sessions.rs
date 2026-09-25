use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;
use statsai_core::{
    micro_usd_to_cents_rounded, report_period_from_range, ReportPeriod, SessionRollupV1,
};
use statsai_store::{SessionFilter, SessionStats, Store};

use super::args::SessionsCommand;
use super::format::{format_cost, format_u64, truncate_label};
use super::source::canonical_provider;

pub(crate) struct SessionsView {
    pub(crate) label: String,
    pub(crate) stats: SessionStats,
    pub(crate) sessions: Vec<SessionRollupV1>,
}

pub(crate) fn sessions(command: SessionsCommand, store: &Store) -> Result<()> {
    let view = sessions_view(&command, store, Utc::now())?;
    if command.json {
        println!("{}", serde_json::to_string_pretty(&sessions_json(&view))?);
    } else {
        print_sessions_table(&view);
    }
    Ok(())
}

pub(crate) fn sessions_view(
    command: &SessionsCommand,
    store: &Store,
    now: DateTime<Utc>,
) -> Result<SessionsView> {
    let period = match (&command.from, &command.to) {
        (None, None) => ReportPeriod::LastDays(7),
        (from, to) => report_period_from_range(from.as_deref(), to.as_deref(), now)?,
    };
    let (since, until) = period.published_window(now);
    let provider = command
        .provider
        .as_deref()
        .map(canonical_provider)
        .transpose()?;
    let filter = SessionFilter {
        provider,
        project_key: command.project.clone(),
        account: None,
    };
    let stats = store.session_stats_in_period(since, until, &filter)?;
    let sessions = store.session_rollups_in_period(
        since,
        until,
        &filter,
        command.sort.store_sort(),
        command.limit,
        0,
    )?;
    Ok(SessionsView {
        label: period.label(now),
        stats,
        sessions,
    })
}

fn sessions_json(view: &SessionsView) -> serde_json::Value {
    json!({
        "label": view.label,
        "stats": view.stats,
        "sessions": view.sessions,
    })
}

fn print_sessions_table(view: &SessionsView) {
    let stats = &view.stats;
    println!("statsai sessions: {}", view.label);
    println!("sessions: {}", format_u64(stats.sessions));
    println!("tokens: {}", format_u64(stats.total_tokens));
    println!(
        "avg tokens/session: {}",
        stats
            .avg_tokens
            .map(format_u64)
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "median duration: {}",
        format_duration(stats.median_duration_seconds)
    );
    println!("messages: {}", format_u64(stats.total_messages));
    println!(
        "est. cost: {}",
        format_cost(Some(micro_usd_to_cents_rounded(stats.total_cost_micro_usd)))
    );
    println!(
        "top model: {}",
        stats.top_model.as_deref().unwrap_or("unknown")
    );
    println!();
    println!(
        "{:<16} {:<12} {:<18} {:<24} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "started",
        "provider",
        "model",
        "title",
        "msgs",
        "input",
        "output",
        "total",
        "est_cost",
        "duration"
    );
    for session in &view.sessions {
        println!(
            "{:<16} {:<12} {:<18} {:<24} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
            session.started_at.format("%Y-%m-%d %H:%M"),
            truncate_label(&session.provider, 12),
            truncate_label(session.primary_model.as_deref().unwrap_or("unknown"), 18),
            truncate_label(session.title.as_deref().unwrap_or("—"), 24),
            session
                .total_messages
                .map(format_u64)
                .unwrap_or_else(|| "—".to_string()),
            format_u64(session.usage.input_tokens.unwrap_or(0)),
            format_u64(session.usage.output_tokens.unwrap_or(0)),
            format_u64(session.usage.computed_total()),
            format_cost(
                session
                    .cost
                    .estimated_micro_usd()
                    .map(micro_usd_to_cents_rounded)
            ),
            format_duration(session.duration_seconds)
        );
    }
}

fn format_duration(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "unknown".to_string();
    };
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}
