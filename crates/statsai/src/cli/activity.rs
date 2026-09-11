use anyhow::Result;
use serde_json::json;
use statsai_core::ActivityKind;
use statsai_store::Store;

use super::args::{ActivityCommand, ActivitySubcommand};
use super::format::{format_local_timestamp, format_u64};
use super::source::canonical_provider;

pub(crate) fn activity(command: ActivityCommand, store: &Store) -> Result<()> {
    match command.command {
        ActivitySubcommand::Status {
            provider,
            kind,
            json,
        } => {
            let provider = provider.as_deref().map(canonical_provider).transpose()?;
            let kind = kind
                .as_deref()
                .map(|value| {
                    ActivityKind::parse(value)
                        .ok_or_else(|| anyhow::anyhow!("unsupported activity kind {value}"))
                })
                .transpose()?;
            let include_activity = store.sync_preferences()?.include_activity;
            let status = store.activity_status(provider.as_deref(), kind, include_activity)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": "activity_status.v1",
                        "totals": {
                            "toolCalls": status.tool_calls,
                            "mcpCalls": status.mcp_calls,
                            "skillLoads": status.skill_loads,
                            "succeeded": status.succeeded,
                            "failed": status.failed,
                            "unknown": status.unknown,
                        },
                        "dateRange": status.first_seen.zip(status.last_seen).map(|(from, to)| json!({
                            "from": from.to_rfc3339(),
                            "to": to.to_rfc3339(),
                        })),
                        "topTools": status.top_tools.iter().map(status_row_json).collect::<Vec<_>>(),
                        "topMcp": status.top_mcp.iter().map(status_row_json).collect::<Vec<_>>(),
                        "topSkills": status.top_skills.iter().map(status_row_json).collect::<Vec<_>>(),
                        "coverage": status.coverage.iter().map(|row| json!({
                            "provider": row.provider,
                            "kind": row.kind.as_str(),
                            "level": row.level.as_str(),
                            "evidence": row.evidence,
                            "day": row.day,
                        })).collect::<Vec<_>>(),
                        "includeActivity": status.include_activity,
                    }))?
                );
            } else {
                println!(
                    "activity: tools={} mcp={} skill_loads={} succeeded={} failed={} unknown={}",
                    format_u64(status.tool_calls),
                    format_u64(status.mcp_calls),
                    format_u64(status.skill_loads),
                    format_u64(status.succeeded),
                    format_u64(status.failed),
                    format_u64(status.unknown)
                );
                match (status.first_seen, status.last_seen) {
                    (Some(from), Some(to)) => println!(
                        "range: {} .. {}",
                        format_local_timestamp(from),
                        format_local_timestamp(to)
                    ),
                    _ => println!("range: none"),
                }
                println!(
                    "sync include_activity={}",
                    if status.include_activity {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                print_top("tools", &status.top_tools);
                print_top("mcp", &status.top_mcp);
                print_top("observed skill loads", &status.top_skills);
                if status.coverage.is_empty() {
                    println!("coverage: none");
                } else {
                    println!("coverage:");
                    for row in &status.coverage {
                        println!(
                            "  {} {} {} {} {}",
                            row.provider,
                            row.kind.as_str(),
                            row.level.as_str(),
                            row.day,
                            row.evidence
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

fn print_top(label: &str, rows: &[statsai_store::ActivityStatusRow]) {
    if rows.is_empty() {
        println!("top {label}: none");
        return;
    }
    println!("top {label}:");
    for row in rows {
        println!(
            "  {} {} calls={} succeeded={} failed={} unknown={}",
            row.provider,
            row.display_name,
            format_u64(row.calls),
            format_u64(row.succeeded),
            format_u64(row.failed),
            format_u64(row.unknown)
        );
    }
}

fn status_row_json(row: &statsai_store::ActivityStatusRow) -> serde_json::Value {
    json!({
        "provider": row.provider,
        "kind": row.kind.as_str(),
        "displayName": row.display_name,
        "family": row.family.as_str(),
        "calls": row.calls,
        "succeeded": row.succeeded,
        "failed": row.failed,
        "unknown": row.unknown,
    })
}
