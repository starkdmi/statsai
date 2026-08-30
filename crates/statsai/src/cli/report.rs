use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde_json::json;
use statsai_core::{
    build_usage_report, report_period_from_range, ReportPeriod, UsageReport, UsageTotals,
};
use statsai_store::Store;
use std::collections::{BTreeMap, BTreeSet};

use super::args::{ExportCommand, ReportCommand, ReportSubcommand};
use super::format::{
    format_cost, format_ratio, format_subscription_price, format_u64, major_unit_amount,
    subscription_status_label, truncate_label, usd_amount_json,
};

pub(crate) fn report(command: ReportCommand, store: &Store) -> Result<()> {
    let now = Utc::now();
    let (report, json_output, verbose, include_subscriptions) =
        usage_report_from_command(command, store, now)?;
    if json_output {
        print_report_json(&report, verbose, include_subscriptions)?;
    } else {
        print_report_table(&report, verbose, include_subscriptions);
    }
    Ok(())
}

pub(crate) fn usage_report_from_command(
    command: ReportCommand,
    store: &Store,
    now: DateTime<Utc>,
) -> Result<(UsageReport, bool, bool, bool)> {
    let (period, json_output, verbose, include_subscriptions) = match command.command {
        ReportSubcommand::Weekly {
            json,
            verbose,
            subscriptions,
            ..
        } => (ReportPeriod::LastDays(7), json, verbose, subscriptions),
        ReportSubcommand::Monthly {
            json,
            verbose,
            subscriptions,
            ..
        } => (ReportPeriod::LastDays(30), json, verbose, subscriptions),
        ReportSubcommand::AllTime {
            json,
            verbose,
            subscriptions,
        } => (ReportPeriod::AllTime, json, verbose, subscriptions),
        ReportSubcommand::Range {
            from,
            to,
            json,
            verbose,
            subscriptions,
        } => (
            report_period_from_range(from.as_deref(), to.as_deref(), now)?,
            json,
            verbose,
            subscriptions,
        ),
    };
    let (since, until) = period.window(now);
    let (events, summaries) = match period {
        ReportPeriod::AllTime => (store.events()?, store.summaries()?),
        _ => (store.events_in_period(since, until)?, Vec::new()),
    };
    let report = build_usage_report(
        &events,
        &summaries,
        &store.list_sources()?,
        &store.list_accounts()?,
        &store.list_subscriptions()?,
        period,
        now,
    );
    Ok((report, json_output, verbose, include_subscriptions))
}

pub(crate) fn export(command: ExportCommand, store: &Store) -> Result<()> {
    if !command.json {
        bail!("only --json export is supported");
    }
    println!("{}", serde_json::to_string_pretty(&store.events()?)?);
    Ok(())
}

pub(crate) fn print_report_table(report: &UsageReport, verbose: bool, include_subscriptions: bool) {
    println!("statsai report: {}", report.label);
    if let Some(since) = report.since {
        println!(
            "range: {} to {}",
            since.to_rfc3339(),
            report.until.to_rfc3339()
        );
    } else {
        println!(
            "range: all stored events through {}",
            report.until.to_rfc3339()
        );
    }
    println!(
        "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "provider",
        "account",
        "events",
        "input",
        "cache_create",
        "cache_read",
        "output",
        "total",
        "est_cost"
    );
    for row in &report.rows {
        println!(
            "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            row.provider,
            row.account,
            format_u64(row.events),
            format_u64(row.usage.input_tokens),
            format_u64(row.usage.cache_creation_tokens),
            format_u64(row.usage.cached_input_tokens),
            format_u64(row.usage.output_tokens),
            format_u64(row.usage.total_tokens),
            format_cost(row.usage.estimated_cost_usd)
        );
        if verbose {
            println!("  reasoning: {}", format_u64(row.usage.reasoning_tokens));
            println!(
                "  sources: {}",
                row.sources.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            if !row.paths.is_empty() {
                println!(
                    "  paths: {}",
                    row.paths.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }
    }
    println!(
        "{:<14} {:<16} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "total",
        "",
        format_u64(report.total_events),
        format_u64(report.total_usage.input_tokens),
        format_u64(report.total_usage.cache_creation_tokens),
        format_u64(report.total_usage.cached_input_tokens),
        format_u64(report.total_usage.output_tokens),
        format_u64(report.total_usage.total_tokens),
        format_cost(report.total_usage.estimated_cost_usd)
    );

    if include_subscriptions {
        print_subscription_report_table(report, verbose);
    }

    if !report.summary_rows.is_empty() {
        let summary_direct_total: u64 = report
            .summary_rows
            .iter()
            .map(|row| row.direct_event_usage.total_tokens)
            .sum();
        println!(
            "reported/manual summaries (separate provenance, included in known gross totals):"
        );
        println!(
            "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "provider",
            "account",
            "kind",
            "summaries",
            "input",
            "cache_create",
            "cache_read",
            "output",
            "total",
            "cost",
            "uncovered"
        );
        for row in &report.summary_rows {
            let uncovered = row
                .usage
                .total_tokens
                .saturating_sub(row.direct_event_usage.total_tokens);
            println!(
                "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                row.provider,
                row.account,
                row.kind,
                format_u64(row.summaries),
                format_u64(row.usage.input_tokens),
                format_u64(row.usage.cache_creation_tokens),
                format_u64(row.usage.cached_input_tokens),
                format_u64(row.usage.output_tokens),
                format_u64(row.usage.total_tokens),
                format_cost(row.usage.estimated_cost_usd),
                format_u64(uncovered)
            );
            if verbose {
                if let Some(observed_at) = row.observed_at {
                    println!("  observed_at: {}", observed_at.to_rfc3339());
                }
                println!(
                    "  direct_overlap_total: {}",
                    format_u64(row.direct_event_usage.total_tokens)
                );
                if row.exact_overlap_summaries > 0 {
                    println!(
                        "  exact_overlap_summaries: {}",
                        format_u64(row.exact_overlap_summaries)
                    );
                }
                println!(
                    "  sources: {}",
                    row.sources.iter().cloned().collect::<Vec<_>>().join(", ")
                );
                if !row.paths.is_empty() {
                    println!(
                        "  paths: {}",
                        row.paths.iter().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
        println!(
            "{:<14} {:<16} {:<18} {:>10} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
            "summary total",
            "",
            "",
            format_u64(report.summary_rows.iter().map(|row| row.summaries).sum()),
            format_u64(report.total_summary_usage.input_tokens),
            format_u64(report.total_summary_usage.cache_creation_tokens),
            format_u64(report.total_summary_usage.cached_input_tokens),
            format_u64(report.total_summary_usage.output_tokens),
            format_u64(report.total_summary_usage.total_tokens),
            format_cost(report.total_summary_usage.estimated_cost_usd),
            format_u64(
                report
                    .total_summary_usage
                    .total_tokens
                    .saturating_sub(summary_direct_total)
            )
        );
        print_known_usage_table(report);
    }
}

pub(crate) fn print_subscription_report_table(report: &UsageReport, verbose: bool) {
    if report.subscription_rows.is_empty() {
        println!("subscription value: no matching subscription periods");
        return;
    }
    println!("subscription value:");
    println!(
        "{:<14} {:<16} {:<14} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "provider", "account", "plan", "events", "total", "value_usd", "price", "ratio"
    );
    for row in &report.subscription_rows {
        println!(
            "{:<14} {:<16} {:<14} {:>10} {:>12} {:>12} {:>12} {:>12}",
            row.provider,
            truncate_label(&row.account, 16),
            truncate_label(&row.plan_name, 14),
            format_u64(row.events),
            format_u64(row.usage.total_tokens),
            format_cost(row.usage.estimated_cost_usd),
            format_subscription_price(row.price, &row.currency),
            format_ratio(row.value_to_price_ratio)
        );
        if verbose {
            println!("  subscription_id: {}", row.subscription_id.0);
            println!("  provider_account_id: {}", row.provider_account_id.0);
            println!("  started_at: {}", row.started_at.to_rfc3339());
            if let Some(ended_at) = row.ended_at {
                println!("  ended_at: {}", ended_at.to_rfc3339());
            }
            println!("  status: {}", subscription_status_label(&row.status));
            if let Some(delta) = row.value_minus_price_usd {
                println!("  value_minus_price_usd: {}", format_cost(Some(delta)));
            }
        }
    }
}

pub(crate) fn print_known_usage_table(report: &UsageReport) {
    let mut direct_by_provider: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for row in &report.rows {
        direct_by_provider
            .entry(row.provider.clone())
            .or_default()
            .add_totals(&row.usage);
    }
    let mut reported_by_provider: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for row in &report.summary_rows {
        reported_by_provider
            .entry(row.provider.clone())
            .or_default()
            .add_totals(&row.usage);
    }
    let providers: BTreeSet<_> = direct_by_provider
        .keys()
        .chain(reported_by_provider.keys())
        .cloned()
        .collect();
    println!("known gross totals by provider (direct + reported/manual, no overlap deduction):");
    println!(
        "{:<14} {:>14} {:>14} {:>14} {:>12} {:>12} {:>12}",
        "provider",
        "direct",
        "reported",
        "known_gross",
        "direct_cost",
        "reported_cost",
        "known_cost"
    );
    for provider in providers {
        let direct = direct_by_provider
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        let reported = reported_by_provider
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        let mut known = direct.clone();
        known.add_totals(&reported);
        println!(
            "{:<14} {:>14} {:>14} {:>14} {:>12} {:>12} {:>12}",
            provider,
            format_u64(direct.total_tokens),
            format_u64(reported.total_tokens),
            format_u64(known.total_tokens),
            format_cost(direct.estimated_cost_usd),
            format_cost(reported.estimated_cost_usd),
            format_cost(known.estimated_cost_usd)
        );
    }
}

pub(crate) fn print_report_json(
    report: &UsageReport,
    verbose: bool,
    include_subscriptions: bool,
) -> Result<()> {
    let rows = report.rows.iter().map(|row| {
        let mut value = json!({
            "provider": row.provider,
            "account": row.account,
            "events": row.events,
            "tokens": {
                "input": row.usage.input_tokens,
                "cache_creation": row.usage.cache_creation_tokens,
                "cache_read": row.usage.cached_input_tokens,
                "cached_input": row.usage.cached_input_tokens,
                "output": row.usage.output_tokens,
                "reasoning": row.usage.reasoning_tokens,
                "total": row.usage.total_tokens,
            },
            "estimated_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_cost_usd_cents": row.usage.estimated_cost_usd,
        });
        if verbose {
            value["sources"] = json!(row.sources.iter().cloned().collect::<Vec<_>>());
            value["paths"] = json!(row.paths.iter().cloned().collect::<Vec<_>>());
        }
        value
    });
    let summary_rows = report.summary_rows.iter().map(|row| {
        let mut value = json!({
            "provider": row.provider,
            "account": row.account,
            "kind": row.kind,
            "summaries": row.summaries,
            "tokens": {
                "input": row.usage.input_tokens,
                "cache_creation": row.usage.cache_creation_tokens,
                "cache_read": row.usage.cached_input_tokens,
                "cached_input": row.usage.cached_input_tokens,
                "output": row.usage.output_tokens,
                "reasoning": row.usage.reasoning_tokens,
                "total": row.usage.total_tokens,
            },
            "direct_overlap_total_tokens": row.direct_event_usage.total_tokens,
            "uncovered_total_tokens": row.usage.total_tokens.saturating_sub(row.direct_event_usage.total_tokens),
            "exact_overlap_summaries": row.exact_overlap_summaries,
            "observed_at": row.observed_at.map(|date| date.to_rfc3339()),
            "estimated_or_reported_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": row.usage.estimated_cost_usd,
        });
        if verbose {
            value["sources"] = json!(row.sources.iter().cloned().collect::<Vec<_>>());
            value["paths"] = json!(row.paths.iter().cloned().collect::<Vec<_>>());
        }
        value
    });
    let subscription_rows = report.subscription_rows.iter().map(|row| {
        json!({
            "subscription_id": row.subscription_id.0,
            "provider": row.provider,
            "provider_account_id": row.provider_account_id.0,
            "account": row.account,
            "plan_name": row.plan_name,
            "price": major_unit_amount(row.price),
            "price_cents": row.price,
            "currency": row.currency,
            "billing_period": format!("{:?}", row.billing_period).to_ascii_lowercase(),
            "started_at": row.started_at.to_rfc3339(),
            "ended_at": row.ended_at.map(|date| date.to_rfc3339()),
            "status": subscription_status_label(&row.status),
            "events": row.events,
            "tokens": {
                "input": row.usage.input_tokens,
                "cache_creation": row.usage.cache_creation_tokens,
                "cache_read": row.usage.cached_input_tokens,
                "cached_input": row.usage.cached_input_tokens,
                "output": row.usage.output_tokens,
                "reasoning": row.usage.reasoning_tokens,
                "total": row.usage.total_tokens,
            },
            "estimated_cost_usd": usd_amount_json(row.usage.estimated_cost_usd),
            "estimated_cost_usd_cents": row.usage.estimated_cost_usd,
            "value_minus_price_usd": usd_amount_json(row.value_minus_price_usd),
            "value_minus_price_usd_cents": row.value_minus_price_usd,
            "value_to_price_ratio": row.value_to_price_ratio,
        })
    });
    let summary_direct_total: u64 = report
        .summary_rows
        .iter()
        .map(|row| row.direct_event_usage.total_tokens)
        .sum();
    let mut known_usage = report.total_usage.clone();
    known_usage.add_totals(&report.total_summary_usage);
    let mut value = json!({
        "label": report.label,
        "since": report.since.map(|date| date.to_rfc3339()),
        "until": report.until.to_rfc3339(),
        "total_events": report.total_events,
        "total_tokens": {
            "input": report.total_usage.input_tokens,
            "cache_creation": report.total_usage.cache_creation_tokens,
            "cache_read": report.total_usage.cached_input_tokens,
            "cached_input": report.total_usage.cached_input_tokens,
            "output": report.total_usage.output_tokens,
            "reasoning": report.total_usage.reasoning_tokens,
            "total": report.total_usage.total_tokens,
        },
        "total_estimated_cost_usd": usd_amount_json(report.total_usage.estimated_cost_usd),
        "total_estimated_cost_usd_cents": report.total_usage.estimated_cost_usd,
        "known_gross": {
            "description": "direct events plus reported/manual summaries, without overlap deduction",
            "total_tokens": {
                "input": known_usage.input_tokens,
                "cache_creation": known_usage.cache_creation_tokens,
                "cache_read": known_usage.cached_input_tokens,
                "cached_input": known_usage.cached_input_tokens,
                "output": known_usage.output_tokens,
                "reasoning": known_usage.reasoning_tokens,
                "total": known_usage.total_tokens,
            },
            "estimated_or_reported_cost_usd": usd_amount_json(known_usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": known_usage.estimated_cost_usd,
        },
        "summary_reports": {
            "included_in_event_totals": false,
            "included_in_known_gross_totals": true,
            "total_tokens": {
                "input": report.total_summary_usage.input_tokens,
                "cache_creation": report.total_summary_usage.cache_creation_tokens,
                "cache_read": report.total_summary_usage.cached_input_tokens,
                "cached_input": report.total_summary_usage.cached_input_tokens,
                "output": report.total_summary_usage.output_tokens,
                "reasoning": report.total_summary_usage.reasoning_tokens,
                "total": report.total_summary_usage.total_tokens,
            },
            "estimated_or_reported_cost_usd": usd_amount_json(report.total_summary_usage.estimated_cost_usd),
            "estimated_or_reported_cost_usd_cents": report.total_summary_usage.estimated_cost_usd,
            "uncovered_total_tokens": report.total_summary_usage.total_tokens.saturating_sub(summary_direct_total),
            "rows": summary_rows.collect::<Vec<_>>(),
        },
        "rows": rows.collect::<Vec<_>>(),
    });
    if include_subscriptions {
        value["subscription_value"] = json!({
            "rows": subscription_rows.collect::<Vec<_>>(),
        });
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
