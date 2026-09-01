use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::json;
use statsai_core::{
    normalize_email, normalize_provider_user_id, ProviderAccount, QuotaObservationRecordV1,
    QuotaWindowSyncProjectionV1, QuotaWindowV1,
};
use statsai_store::{QuotaQuery, Store};
use std::collections::BTreeMap;

use super::args::{QuotaCommand, QuotaSubcommand};
use super::format::{format_local_timestamp, format_u64, parse_date, print_json_lines};
use super::source::canonical_provider;

pub(crate) fn quota(command: QuotaCommand, store: &Store, device_id: &str) -> Result<()> {
    match command.command {
        QuotaSubcommand::Status { account, json } => {
            let query = quota_query(store, None, account.as_deref(), None, None, None)?;
            let status = store.quota_status(&query)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "quota observations: {}",
                    format_u64(status.total_observations)
                );
                println!(
                    "distinct: {} ({} copied duplicates collapsed)",
                    format_u64(status.distinct_observations),
                    format_u64(status.duplicate_observations)
                );
                println!(
                    "attributed: {}  range: {}",
                    format_u64(status.attributed_observations),
                    format_quota_range(status.attributed_range.as_ref())
                );
                println!(
                    "unattributed: {}  range: {}",
                    format_u64(status.unattributed_observations),
                    format_quota_range(status.unattributed_range.as_ref())
                );
                println!(
                    "weekly sync coverage: {}/{} ({:.1}%)",
                    format_u64(status.weekly_sync_eligible_observations),
                    format_u64(status.weekly_observations),
                    status.weekly_sync_eligible_coverage_percent
                );
                let discarded = &status.discarded;
                println!(
                    "reconstruction discarded: {} replayed observations, {} unused windows, {} bracketed schedules",
                    format_u64(discarded.replayed_observations),
                    format_u64(discarded.unused_windows),
                    format_u64(discarded.bracketed_schedules)
                );
                if status.unattributed_observations > 0 {
                    eprintln!(
                        "warning: historical quota observations remain unassigned; backdate a source connection with `statsai source connect --started-at ...`"
                    );
                }
                for warning in status.assignment_overlap_warnings {
                    eprintln!("warning: {warning}");
                }
            }
        }
        QuotaSubcommand::Current {
            account,
            all_scopes,
            include_overlaps,
            json,
        } => {
            let query = quota_query(store, None, account.as_deref(), None, None, None)?;
            let mut windows = select_current_quota_windows(
                store.quota_windows_without_usage_totals(&query)?,
                all_scopes,
                include_overlaps,
            );
            store.enrich_quota_window_usage_totals(&mut windows)?;
            print_quota_windows(&windows, json)?;
        }
        QuotaSubcommand::Windows {
            provider,
            account,
            from,
            to,
            limit_id,
            limit,
            json,
        } => {
            let query = quota_query(
                store,
                provider.as_deref(),
                account.as_deref(),
                from.as_deref(),
                to.as_deref(),
                limit_id,
            )?;
            let mut windows = store.quota_windows_without_usage_totals(&query)?;
            windows.truncate(limit.min(10_000));
            store.enrich_quota_window_usage_totals(&mut windows)?;
            print_quota_windows(&windows, json)?;
        }
        QuotaSubcommand::History {
            window_id,
            raw,
            json,
        } => {
            let query = QuotaQuery::default();
            let windows = store.quota_windows_without_usage_totals(&query)?;
            let window = window_id
                .as_deref()
                .map(|id| windows.iter().find(|window| window.window_id == id))
                .unwrap_or_else(|| windows.first())
                .with_context(|| {
                    window_id.map_or_else(
                        || "no quota windows found".to_string(),
                        |id| format!("quota window not found: {id}"),
                    )
                })?;
            if raw {
                let observations =
                    raw_observations_for_window(store.quota_observations(&query, false)?, window);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "schema_version": "quota_history.v1",
                            "window_id": window.window_id,
                            "raw": true,
                            "observations": observations,
                        }))?
                    );
                } else {
                    for record in observations {
                        println!("{}", serde_json::to_string_pretty(&record)?);
                    }
                }
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "quota_history.v1",
                        "window_id": window.window_id,
                        "raw": false,
                        "change_points": window.change_points,
                    }))?
                );
            } else {
                println!("{}", quota_window_heading(window));
                for point in &window.change_points {
                    println!(
                        "  {}  {:>6.2}%  reset {}  slot={}",
                        format_local_timestamp(point.observed_at),
                        point.used_percent,
                        format_local_timestamp(point.resets_at),
                        point.provider_slot,
                    );
                }
            }
        }
        QuotaSubcommand::Export { level, format } => {
            let query = QuotaQuery::default();
            match level.as_str() {
                "observations" => {
                    export_quota_observations(&store.quota_observations(&query, false)?, &format)?
                }
                "windows" => export_quota_windows(&store.quota_windows(&query)?, &format)?,
                "sync-windows" => {
                    let status = store.quota_status(&query)?;
                    if status.weekly_sync_eligible_observations < status.weekly_observations {
                        eprintln!(
                            "warning: only {}/{} weekly observations are attributed and eligible for sync projection export",
                            status.weekly_sync_eligible_observations,
                            status.weekly_observations
                        );
                    }
                    export_quota_projections(
                        &store.quota_sync_projections(&query, device_id)?,
                        &format,
                    )?;
                }
                _ => unreachable!("clap validates quota export level"),
            }
        }
    }
    Ok(())
}

fn quota_query(
    store: &Store,
    provider: Option<&str>,
    account: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    limit_id: Option<String>,
) -> Result<QuotaQuery> {
    let provider = provider.map(canonical_provider).transpose()?;
    let provider_account_id = account
        .map(|selector| resolve_quota_account_selector(store, provider.as_deref(), selector))
        .transpose()?
        .map(|account| account.provider_account_id);
    Ok(QuotaQuery {
        provider,
        provider_account_id,
        source_id: None,
        from: from.map(parse_date).transpose()?,
        to: to.map(parse_quota_range_end).transpose()?,
        limit_id,
    })
}

fn parse_quota_range_end(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")?;
    let next_day = date
        .succ_opt()
        .context("quota range end is outside the supported calendar")?;
    Ok(next_day
        .and_hms_opt(0, 0, 0)
        .context("failed to build quota range end")?
        .and_utc()
        - Duration::nanoseconds(1))
}

fn resolve_quota_account_selector(
    store: &Store,
    provider: Option<&str>,
    selector: &str,
) -> Result<ProviderAccount> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("account selector cannot be empty");
    }
    let normalized_email = normalize_email(selector);
    let normalized_provider_user_id = normalize_provider_user_id(selector);
    let normalized_label = selector.to_ascii_lowercase();
    let mut matches = store
        .list_accounts()?
        .into_iter()
        .filter(|account| provider.is_none_or(|provider| account.provider == provider))
        .filter(|account| {
            account.provider_account_id.0 == selector
                || account.email.as_deref().map(normalize_email).as_deref()
                    == Some(normalized_email.as_str())
                || account
                    .provider_user_id
                    .as_deref()
                    .map(normalize_provider_user_id)
                    .as_deref()
                    == Some(normalized_provider_user_id.as_str())
                || account
                    .account_label
                    .as_deref()
                    .map(|label| label.trim().to_ascii_lowercase())
                    .as_deref()
                    == Some(normalized_label.as_str())
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|account| account.provider_account_id.0.clone());
    matches.dedup_by(|left, right| left.provider_account_id == right.provider_account_id);
    match matches.len() {
        0 => bail!("no account matched '{selector}'"),
        1 => Ok(matches.remove(0)),
        _ => bail!(
            "multiple provider accounts matched '{selector}'; use a stable provider account id"
        ),
    }
}

pub(crate) fn select_current_quota_windows(
    windows: Vec<QuotaWindowV1>,
    all_scopes: bool,
    include_overlaps: bool,
) -> Vec<QuotaWindowV1> {
    let mut by_scope = BTreeMap::<
        (String, Option<String>, Option<String>, Option<String>, u64),
        Vec<QuotaWindowV1>,
    >::new();
    for window in windows {
        by_scope
            .entry((
                window.provider.clone(),
                window.provider_account_id.as_ref().map(|id| id.0.clone()),
                window.source_id.as_ref().map(|id| id.0.clone()),
                window.limit_id.clone(),
                window.window_minutes,
            ))
            .or_default()
            .push(window);
    }
    let mut selected = Vec::new();
    for mut scope_windows in by_scope.into_values() {
        scope_windows.sort_by_key(|window| window.first_observed_at);
        let newest = scope_windows.pop().expect("scope contains a window");
        if include_overlaps {
            selected.extend(scope_windows.into_iter().filter(|older| {
                older.inferred_start < newest.representative_reset
                    && older.representative_reset > newest.inferred_start
            }));
        }
        selected.push(newest);
    }
    if !all_scopes {
        let mut longest =
            BTreeMap::<(String, Option<String>, Option<String>, Option<String>), u64>::new();
        for window in &selected {
            let key = (
                window.provider.clone(),
                window.provider_account_id.as_ref().map(|id| id.0.clone()),
                window.source_id.as_ref().map(|id| id.0.clone()),
                window.limit_id.clone(),
            );
            longest
                .entry(key)
                .and_modify(|duration| *duration = (*duration).max(window.window_minutes))
                .or_insert(window.window_minutes);
        }
        selected.retain(|window| {
            longest.get(&(
                window.provider.clone(),
                window.provider_account_id.as_ref().map(|id| id.0.clone()),
                window.source_id.as_ref().map(|id| id.0.clone()),
                window.limit_id.clone(),
            )) == Some(&window.window_minutes)
        });
    }
    selected.sort_by(|left, right| {
        right
            .window_minutes
            .cmp(&left.window_minutes)
            .then_with(|| right.first_observed_at.cmp(&left.first_observed_at))
    });
    selected
}

fn print_quota_windows(windows: &[QuotaWindowV1], json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(windows)?);
        return Ok(());
    }
    if windows.is_empty() {
        println!("no quota windows");
        return Ok(());
    }
    for window in windows {
        println!("{}", quota_window_heading(window));
        println!(
            "  observed {} to {}; samples={} transition={:?}",
            format_local_timestamp(window.first_observed_at),
            format_local_timestamp(window.last_observed_at),
            format_u64(window.sample_count),
            window.transition,
        );
        if let Some(usage_totals) = &window.usage_totals {
            println!(
                "  usage events={} tokens={} cost_micro_usd={}",
                format_u64(usage_totals.event_count),
                format_u64(usage_totals.total_tokens),
                usage_totals
                    .estimated_cost_micro_usd
                    .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            );
        } else {
            println!("  usage unavailable until the quota history is attributed to an account");
        }
        if window.has_schedule_overlap {
            eprintln!(
                "warning: {} overlaps another reconstructed epoch in the same quota scope",
                window.window_id
            );
        }
    }
    Ok(())
}

fn quota_window_heading(window: &QuotaWindowV1) -> String {
    let identity = window.provider_account_id.as_ref().map_or_else(
        || {
            window.source_id.as_ref().map_or_else(
                || "unassigned".to_string(),
                |id| format!("unassigned@{}", id.0),
            )
        },
        |id| id.0.clone(),
    );
    format!(
        "{}  {}  {}m  {:.2}%  reset {}  id={}",
        window.provider,
        identity,
        window.window_minutes,
        window.latest_used_percent,
        format_local_timestamp(window.representative_reset),
        window.window_id,
    )
}

fn format_quota_range(range: Option<&statsai_store::QuotaDateRange>) -> String {
    range.map_or_else(
        || "none".to_string(),
        |range| {
            format!(
                "{} to {}",
                format_local_timestamp(range.first),
                format_local_timestamp(range.last)
            )
        },
    )
}

pub(crate) fn raw_observations_for_window(
    records: Vec<QuotaObservationRecordV1>,
    window: &QuotaWindowV1,
) -> Vec<QuotaObservationRecordV1> {
    records
        .into_iter()
        .filter(|record| {
            record.observation.provider == window.provider
                && record.observation.provider_account_id == window.provider_account_id
                && window
                    .source_id
                    .as_ref()
                    .is_none_or(|source_id| &record.observation.source_id == source_id)
                && record.windows.iter().any(|candidate| {
                    candidate.limit_id == window.limit_id
                        && candidate.window_minutes == window.window_minutes
                        && candidate.resets_at_epoch_seconds >= window.reset_min_epoch_seconds
                        && candidate.resets_at_epoch_seconds <= window.reset_max_epoch_seconds
                })
        })
        .collect()
}

fn export_quota_observations(records: &[QuotaObservationRecordV1], format: &str) -> Result<()> {
    match format {
        "json" => println!("{}", serde_json::to_string_pretty(records)?),
        "jsonl" => print_json_lines(records)?,
        "csv" => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer.write_record([
                "schema_version",
                "observation_id",
                "semantic_fingerprint",
                "provider",
                "source_id",
                "provider_account_id",
                "observed_at",
                "source_file_path_hash",
                "source_record_id",
                "usage_event_id",
                "usage_link_kind",
                "payload_hash",
                "usage_total_tokens",
                "provider_slot",
                "limit_id",
                "window_minutes",
                "used_percent",
                "resets_at",
                "resets_at_epoch_seconds",
            ])?;
            for record in records {
                let observation = &record.observation;
                let base = vec![
                    observation.schema_version.clone(),
                    observation.observation_id.clone(),
                    observation.semantic_fingerprint.clone(),
                    observation.provider.clone(),
                    observation.source_id.0.clone(),
                    observation
                        .provider_account_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                    observation.observed_at.to_rfc3339(),
                    observation.source_file_path_hash.clone(),
                    observation.source_record_id.clone(),
                    observation
                        .usage_event_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                    serde_json::to_value(observation.usage_link_kind)?
                        .as_str()
                        .unwrap_or("none")
                        .to_string(),
                    observation.payload_hash.clone(),
                    observation
                        .usage_sample
                        .as_ref()
                        .map(|usage| usage.computed_total().to_string())
                        .unwrap_or_default(),
                ];
                if record.windows.is_empty() {
                    let mut row = base;
                    row.extend(std::iter::repeat_n(String::new(), 6));
                    writer.write_record(row)?;
                    continue;
                }
                for window in &record.windows {
                    let mut row = base.clone();
                    row.extend([
                        window.provider_slot.clone(),
                        window.limit_id.clone().unwrap_or_default(),
                        window.window_minutes.to_string(),
                        window.used_percent.to_string(),
                        window.resets_at.to_rfc3339(),
                        window.resets_at_epoch_seconds.to_string(),
                    ]);
                    writer.write_record(row)?;
                }
            }
            writer.flush()?;
        }
        _ => unreachable!("clap validates quota export format"),
    }
    Ok(())
}

fn export_quota_windows(windows: &[QuotaWindowV1], format: &str) -> Result<()> {
    match format {
        "json" => println!("{}", serde_json::to_string_pretty(windows)?),
        "jsonl" => print_json_lines(windows)?,
        "csv" => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer.write_record([
                "schema_version",
                "window_id",
                "provider",
                "provider_account_id",
                "source_id",
                "limit_id",
                "window_minutes",
                "inferred_start",
                "representative_reset",
                "representative_reset_epoch_seconds",
                "reset_min_epoch_seconds",
                "reset_max_epoch_seconds",
                "first_observed_at",
                "last_observed_at",
                "sample_count",
                "first_used_percent",
                "latest_used_percent",
                "minimum_used_percent",
                "maximum_used_percent",
                "event_count",
                "total_tokens",
                "estimated_cost_micro_usd",
            ])?;
            for window in windows {
                writer.write_record([
                    window.schema_version.clone(),
                    window.window_id.clone(),
                    window.provider.clone(),
                    window
                        .provider_account_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                    window
                        .source_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                    window.limit_id.clone().unwrap_or_default(),
                    window.window_minutes.to_string(),
                    window.inferred_start.to_rfc3339(),
                    window.representative_reset.to_rfc3339(),
                    window.representative_reset_epoch_seconds.to_string(),
                    window.reset_min_epoch_seconds.to_string(),
                    window.reset_max_epoch_seconds.to_string(),
                    window.first_observed_at.to_rfc3339(),
                    window.last_observed_at.to_rfc3339(),
                    window.sample_count.to_string(),
                    window.first_used_percent.to_string(),
                    window.latest_used_percent.to_string(),
                    window.minimum_used_percent.to_string(),
                    window.maximum_used_percent.to_string(),
                    window
                        .usage_totals
                        .as_ref()
                        .map(|totals| totals.event_count.to_string())
                        .unwrap_or_default(),
                    window
                        .usage_totals
                        .as_ref()
                        .map(|totals| totals.total_tokens.to_string())
                        .unwrap_or_default(),
                    window
                        .usage_totals
                        .as_ref()
                        .and_then(|totals| totals.estimated_cost_micro_usd)
                        .map(|cost| cost.to_string())
                        .unwrap_or_default(),
                ])?;
            }
            writer.flush()?;
        }
        _ => unreachable!("clap validates quota export format"),
    }
    Ok(())
}

fn export_quota_projections(
    projections: &[QuotaWindowSyncProjectionV1],
    format: &str,
) -> Result<()> {
    match format {
        "json" => println!("{}", serde_json::to_string_pretty(projections)?),
        "jsonl" => print_json_lines(projections)?,
        "csv" => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer.write_record([
                "schema_version",
                "projection_id",
                "device_id",
                "provider",
                "provider_account_id",
                "limit_id",
                "window_minutes",
                "inferred_start",
                "representative_reset",
                "representative_reset_epoch_seconds",
                "reset_min_epoch_seconds",
                "reset_max_epoch_seconds",
                "first_observed_at",
                "last_observed_at",
                "sample_count",
                "latest_used_percent",
                "change_points_json",
                "status_json",
            ])?;
            for projection in projections {
                writer.write_record([
                    projection.schema_version.clone(),
                    projection.projection_id.clone(),
                    projection.device_id.clone(),
                    projection.provider.clone(),
                    projection.provider_account_id.0.clone(),
                    projection.limit_id.clone().unwrap_or_default(),
                    projection.window_minutes.to_string(),
                    projection.inferred_start.to_rfc3339(),
                    projection.representative_reset.to_rfc3339(),
                    projection.representative_reset_epoch_seconds.to_string(),
                    projection.reset_min_epoch_seconds.to_string(),
                    projection.reset_max_epoch_seconds.to_string(),
                    projection.first_observed_at.to_rfc3339(),
                    projection.last_observed_at.to_rfc3339(),
                    projection.sample_count.to_string(),
                    projection.latest_used_percent.to_string(),
                    serde_json::to_string(&projection.change_points)?,
                    serde_json::to_string(&projection.latest_status)?,
                ])?;
            }
            writer.flush()?;
        }
        _ => unreachable!("clap validates quota export format"),
    }
    Ok(())
}
