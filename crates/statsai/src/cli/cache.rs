use anyhow::Result;
use chrono::{Local, Utc};
use statsai_core::{
    micro_usd_to_cents_rounded, AnalyzedCacheCall, CacheBoundaryReason, CacheCallClass,
    CacheGapTiming, CacheLoss, CacheReport, CacheTokenTotals, CacheUnclassifiableReason,
    CacheWriteTtl, CACHE_LOSS_MIN_PERCENT, CACHE_LOSS_MIN_TOKENS,
};
use statsai_store::{CacheReportQuery, Store};

use super::args::CacheReportArgs;
use super::format::{format_cost, format_u64, truncate_label};
use super::source::canonical_provider;

pub(crate) fn cache_report(args: CacheReportArgs, store: &Store) -> Result<()> {
    let report = store.cache_report(&cache_report_query(&args, Utc::now())?, &Local)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_cache_report(&report);
    }
    Ok(())
}

pub(super) fn cache_report_query(
    args: &CacheReportArgs,
    now: chrono::DateTime<Utc>,
) -> Result<CacheReportQuery> {
    let mut query = if args.all {
        CacheReportQuery::all_time(now)
    } else {
        CacheReportQuery::for_range(args.from.as_deref(), args.to.as_deref(), now)?
    };
    query.provider = args
        .provider
        .as_deref()
        .map(canonical_provider)
        .transpose()?;
    query.account = args.account.clone();
    query.session = args.session.clone();
    query.timeline = args.timeline;
    query.details = args.details;
    Ok(query)
}

fn print_cache_report(report: &CacheReport) {
    println!("statsai cache report: {}", report.label);
    match report.since {
        Some(since) => println!(
            "range: {} to {}",
            since.to_rfc3339(),
            report.until.to_rfc3339()
        ),
        None => println!(
            "range: all stored events through {}",
            report.until.to_rfc3339()
        ),
    }
    println!();
    print_totals(report);
    println!();
    print_diagnostics(report);
    if !report.providers.is_empty() {
        println!();
        print_providers(report);
    }
    if let Some(timeline) = report.timeline.as_deref() {
        println!();
        print_timeline(timeline);
    }
    if let Some(details) = report.details.as_deref() {
        println!();
        print_details(details);
    }
}

fn print_totals(report: &CacheReport) {
    let totals = &report.totals;
    let unsplit = report.diagnostics.write_split_unavailable_input_tokens;
    println!(
        "{:<20}{} of {} logical input tokens",
        "cache hit",
        format_share(totals.cache_hit_ratio),
        format_u64(totals.logical_input_tokens)
    );
    let mut breakdown = format!(
        "cache reads {} · cache writes {} · ordinary input {}",
        format_u64(totals.cache_read_tokens),
        format_u64(totals.cache_creation_tokens),
        format_u64(totals.input_tokens.saturating_sub(unsplit))
    );
    if unsplit > 0 {
        breakdown.push_str(&format!(
            " · uncached input, writes not reported {}",
            format_u64(unsplit)
        ));
    }
    println!("{:<20}{breakdown}", "input breakdown");
    println!(
        "{:<20}{} calls · {} input tokens per call",
        "context processed",
        format_u64(totals.requests),
        format_average(totals)
    );
}

fn format_average(totals: &CacheTokenTotals) -> String {
    totals
        .avg_logical_input_per_request
        .map(|average| format_u64(average.round() as u64))
        .unwrap_or_else(|| "—".to_string())
}

fn print_diagnostics(report: &CacheReport) {
    let health = &report.diagnostics;
    println!("diagnostics (detector v{})", report.detector_version);
    if health.requests == 0 {
        println!("  no Claude Code or Codex calls in this range");
        return;
    }
    println!(
        "{:<20}{} analyzed · {} unclassifiable · {} timing-estimated",
        "coverage",
        format_u64(health.analyzed()),
        format_u64(health.unclassifiable),
        format_u64(health.timing_estimated)
    );
    println!(
        "{:<20}{} streams · {} cold",
        "first observed",
        format_u64(health.first_observed),
        format_u64(health.first_cold)
    );
    let boundaries = &report.boundaries;
    println!(
        "{:<20}{} (model change {} · compaction {} · evidence gap {})",
        "restarts",
        format_u64(health.boundaries),
        format_u64(boundaries.model_change),
        format_u64(boundaries.compaction),
        format_u64(boundaries.evidence_gap)
    );
    println!(
        "{:<20}{} · {} suspected losses ({} full · {} partial)",
        "comparable calls",
        format_u64(health.comparable),
        format_u64(health.losses),
        format_u64(health.full_losses),
        format_u64(health.losses.saturating_sub(health.full_losses))
    );
    if health.losses > 0 {
        let logical = report.totals.logical_input_tokens;
        println!(
            "{:<20}{} reusable tokens not read from the cache ({} of logical input)",
            "missed reuse",
            format_u64(health.missed_tokens),
            format_share((logical > 0).then(|| health.missed_tokens as f64 / logical as f64))
        );
        let mut cost = format!(
            "{} extra at API prices",
            format_cost(Some(micro_usd_to_cents_rounded(
                i64::try_from(health.missed_cost_micro_usd).unwrap_or(i64::MAX)
            )))
        );
        if health.unpriced_losses > 0 {
            cost.push_str(&format!(
                ", leaving out {} losses on models without known pricing",
                format_u64(health.unpriced_losses)
            ));
        }
        println!("{:<20}{cost}", "missed reuse cost");
    }
    if health.uncached > 0 {
        println!(
            "{:<20}{} calls neither read nor wrote the cache, so are not judged",
            "cache not used",
            format_u64(health.uncached)
        );
    }
    if health.below_threshold > 0 {
        println!(
            "{:<20}{} calls followed less than {} reusable tokens",
            "too small to judge",
            format_u64(health.below_threshold),
            format_u64(CACHE_LOSS_MIN_TOKENS)
        );
    }
    println!();
    println!(
        "{:<26} {:>12} {:>10} {:>10} {:>16}",
        "gap since previous call", "comparable", "losses", "loss rate", "missed tokens"
    );
    for row in &report.gap_histogram {
        println!(
            "{:<26} {:>12} {:>10} {:>10} {:>16}",
            row.label,
            format_u64(row.comparable),
            format_u64(row.losses),
            format_share((row.comparable > 0).then(|| row.losses as f64 / row.comparable as f64)),
            format_u64(row.missed_tokens)
        );
    }
    println!();
    println!(
        "A suspected loss is a call whose cached reads fell at least {}% and {} tokens below",
        CACHE_LOSS_MIN_PERCENT,
        format_u64(CACHE_LOSS_MIN_TOKENS)
    );
    println!("the context the previous call left reusable. It is read from token counts, not");
    println!("confirmed by the provider. Timing-estimated gaps run between recorded responses");
    println!("and include the call's own run time.");
}

fn print_providers(report: &CacheReport) {
    println!(
        "{:<14} {:>10} {:>8} {:>16} {:>12} {:>8}",
        "provider", "calls", "hit", "input/call", "comparable", "losses"
    );
    for row in &report.providers {
        let health = row.diagnostics.as_ref();
        println!(
            "{:<14} {:>10} {:>8} {:>16} {:>12} {:>8}",
            truncate_label(&row.provider, 14),
            format_u64(row.totals.requests),
            format_share(row.totals.cache_hit_ratio),
            format_average(&row.totals),
            optional_count(health.map(|health| health.comparable)),
            optional_count(health.map(|health| health.losses)),
        );
    }
}

fn print_timeline(timeline: &[statsai_core::CacheTimelineRow]) {
    println!(
        "{:<26} {:>8} {:>14} {:>8} {:>7} {:>9} {:>7}",
        "local 10-minute bucket", "calls", "context", "hit", "first", "restarts", "losses"
    );
    for row in timeline {
        println!(
            "{:<26} {:>8} {:>14} {:>8} {:>7} {:>9} {:>7}",
            row.start,
            format_u64(row.requests),
            format_u64(row.logical_input_tokens),
            format_share(row.cache_hit_ratio),
            format_u64(row.first_observed),
            format_u64(row.boundaries),
            format_u64(row.losses)
        );
    }
}

fn print_details(details: &[AnalyzedCacheCall]) {
    println!(
        "{:<19} {:<12} {:<16} {:<18} {:>10} {:>10} {:>9} {:>10} {:>8}  verdict",
        "completed (local)",
        "provider",
        "session",
        "model",
        "input",
        "read",
        "write",
        "baseline",
        "gap"
    );
    for call in details {
        let agent = call
            .agent_hash
            .as_deref()
            .map(|hash| format!("/{}", &hash[..hash.len().min(6)]))
            .unwrap_or_default();
        println!(
            "{:<19} {:<12} {:<16} {:<18} {:>10} {:>10} {:>9} {:>10} {:>8}  {}",
            call.completed_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S"),
            truncate_label(&call.provider, 12),
            truncate_label(&format!("{}{agent}", call.session_id), 16),
            truncate_label(call.model.as_deref().unwrap_or("unknown"), 18),
            format_u64(call.input_tokens),
            format_u64(call.cache_read_tokens),
            call.cache_creation_tokens
                .map(format_u64)
                .unwrap_or_else(|| "—".to_string()),
            call.verdict
                .baseline_tokens
                .map(format_u64)
                .unwrap_or_else(|| "—".to_string()),
            format_gap(call),
            verdict_label(call)
        );
    }
}

fn format_gap(call: &AnalyzedCacheCall) -> String {
    let Some(seconds) = call.verdict.gap_seconds else {
        return "—".to_string();
    };
    let text = if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    };
    match call.verdict.gap_timing {
        Some(CacheGapTiming::RecordedEvents) => format!("~{text}"),
        _ => text,
    }
}

fn verdict_label(call: &AnalyzedCacheCall) -> String {
    let verdict = &call.verdict;
    let mut label = match verdict.class {
        CacheCallClass::FirstObserved if call.cache_read_tokens == 0 => {
            "first observed, cold".to_string()
        }
        CacheCallClass::FirstObserved => "first observed".to_string(),
        CacheCallClass::Boundary => match verdict.boundary {
            Some(CacheBoundaryReason::ModelChange) => format!(
                "restart: model changed from {}",
                verdict.previous_model.as_deref().unwrap_or("unknown")
            ),
            Some(CacheBoundaryReason::Compaction) => "restart: after compaction".to_string(),
            Some(CacheBoundaryReason::EvidenceGap) | None => {
                "restart: after unclassifiable usage".to_string()
            }
        },
        CacheCallClass::BelowThreshold => "too small to judge".to_string(),
        CacheCallClass::Comparable => match verdict.loss {
            Some(CacheLoss::Full) => "suspected loss, nothing read".to_string(),
            Some(CacheLoss::Partial) => "suspected loss, partial reuse".to_string(),
            None => "reused".to_string(),
        },
        CacheCallClass::Uncached => "cache not used".to_string(),
        CacheCallClass::Unclassifiable => format!(
            "unclassifiable: {}",
            match verdict.unclassifiable {
                Some(CacheUnclassifiableReason::TimestampMissing) => "no timestamp",
                Some(CacheUnclassifiableReason::Aggregated) => "calls recorded as one total",
                Some(CacheUnclassifiableReason::NoInputTelemetry) => "no input counts",
                Some(CacheUnclassifiableReason::AmbiguousOrder) => "order not recorded",
                None => "unknown",
            }
        ),
    };
    if let Some(ttl) = verdict.previous_write_ttl {
        label.push_str(match ttl {
            CacheWriteTtl::FiveMinutes => " · previous writes 5m",
            CacheWriteTtl::OneHour => " · previous writes 1h",
            CacheWriteTtl::Mixed => " · previous writes 5m+1h",
        });
    }
    label
}

fn format_share(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.1}%", value * 100.0))
        .unwrap_or_else(|| "—".to_string())
}

fn optional_count(value: Option<u64>) -> String {
    value.map(format_u64).unwrap_or_else(|| "—".to_string())
}
