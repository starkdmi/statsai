use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use statsai_core::{
    TaskSpan, TaskStatus, TaskVerdict, TaskVerification, TaskVerificationAction, WorkItem,
    WorkItemId,
};
use statsai_store::Store;
use std::collections::{BTreeSet, HashSet};

use super::args::{TaskCommand, TaskSubcommand, TaskVerifySubcommand};
use super::format::format_u64;

pub(crate) fn task(command: TaskCommand, store: &Store) -> Result<()> {
    match command.command {
        TaskSubcommand::List {
            provider,
            status,
            json,
        } => {
            let status_filter = status
                .as_deref()
                .map(parse_task_status_filter)
                .transpose()?;
            let selection =
                task_list_selection(store, provider.as_deref(), status_filter.as_ref())?;
            let items = selection.items;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                if items.is_empty() {
                    if status_filter.is_none() && selection.hidden_rejected_meta > 0 {
                        println!(
                            "no visible work items found; {} rejected meta items are hidden by default. Use `statsai task list --status rejected_meta` to inspect them.",
                            format_u64(selection.hidden_rejected_meta as u64)
                        );
                    } else {
                        println!(
                            "no work items found; run `statsai scan` to collect task spans, then `statsai task list` again"
                        );
                    }
                    return Ok(());
                }
                for item in items {
                    println!("{}", format_task_list_item(&item));
                }
                if status_filter.is_none() && selection.hidden_rejected_meta > 0 {
                    println!(
                        "hidden_rejected_meta={} use `statsai task list --status rejected_meta` to inspect",
                        format_u64(selection.hidden_rejected_meta as u64)
                    );
                }
            }
        }
        TaskSubcommand::Show {
            work_item_id,
            include_evidence,
            json,
        } => {
            let work_item_id = WorkItemId(work_item_id);
            let output = load_task_show_output(store, &work_item_id, include_evidence)?;
            if json {
                if include_evidence {
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&output.work_item)?);
                }
            } else {
                print_work_item(&output.work_item);
                if include_evidence {
                    for verification in &output.verifications {
                        println!(
                            "  verification={} updated_at={}",
                            format_task_verification(verification),
                            verification.updated_at.to_rfc3339()
                        );
                    }
                    for span in output.spans {
                        println!(
                            "  span={} provider={} start={} end={} tokens={} title={}",
                            span.span_id.0,
                            span.provider,
                            span.started_at.to_rfc3339(),
                            span.ended_at
                                .map(|value| value.to_rfc3339())
                                .unwrap_or_else(|| "open".to_string()),
                            format_u64(span.usage.computed_total()),
                            span.title
                        );
                        let repo_label = span
                            .project
                            .as_ref()
                            .and_then(|project| project.repo_label.as_deref())
                            .unwrap_or("-");
                        let branch_label = span
                            .project
                            .as_ref()
                            .and_then(|project| project.branch_label.as_deref())
                            .unwrap_or("-");
                        let session_id = span.session_id.as_deref().unwrap_or("-");
                        let thread_id = span.thread_id.as_deref().unwrap_or("-");
                        println!(
                            "    repo={} branch={} session={} thread={} issues={}",
                            repo_label,
                            branch_label,
                            session_id,
                            thread_id,
                            if span.issue_keys.is_empty() {
                                "-".to_string()
                            } else {
                                span.issue_keys.join(",")
                            }
                        );
                        if let Some(summary_preview) = span.summary_preview.as_deref() {
                            println!("    summary_preview={summary_preview}");
                        }
                    }
                }
            }
        }
        TaskSubcommand::Verify { command } => {
            let (verification, buckets) = match command {
                TaskVerifySubcommand::Accept { work_item_id } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Accept {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Reject {
                    work_item_id,
                    reason,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Reject {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                            reason: parse_task_verdict(&reason)?,
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Split {
                    work_item_id,
                    after_span,
                    left_title,
                    right_title,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    let spans = store.task_spans_for_work_item(&work_item_id)?;
                    let after_span_id = statsai_core::TaskSpanId(after_span);
                    let span_index = spans
                        .iter()
                        .position(|span| span.span_id == after_span_id)
                        .with_context(|| {
                            format!(
                                "span {} is not a member of work item {}",
                                after_span_id.0, work_item_id.0
                            )
                        })?;
                    if span_index + 1 >= spans.len() {
                        bail!("cannot split after the last span in a work item");
                    }
                    let before_span_id = spans[span_index + 1].span_id.clone();
                    let verification =
                        store.upsert_task_verification(TaskVerificationAction::Split {
                            after_span_id,
                            before_span_id: Some(before_span_id),
                            left_title,
                            right_title,
                        })?;
                    (
                        verification,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Merge {
                    left_work_item_id,
                    right_work_item_id,
                    title,
                } => {
                    let left_work_item_id = WorkItemId(left_work_item_id);
                    let right_work_item_id = WorkItemId(right_work_item_id);
                    let left = store
                        .work_item(&left_work_item_id)?
                        .with_context(|| format!("unknown work item {}", left_work_item_id.0))?;
                    let right = store
                        .work_item(&right_work_item_id)?
                        .with_context(|| format!("unknown work item {}", right_work_item_id.0))?;
                    if left.project_bucket != right.project_bucket {
                        bail!(
                            "cannot merge work items from different project buckets: {} vs {}",
                            left.project_bucket,
                            right.project_bucket
                        );
                    }
                    (
                        store.upsert_task_verification(TaskVerificationAction::Merge {
                            left_work_item_id: left_work_item_id.clone(),
                            right_work_item_id: right_work_item_id.clone(),
                            left_anchor_span_id: left.anchor_span_id.clone(),
                            right_anchor_span_id: right.anchor_span_id.clone(),
                            title,
                        })?,
                        BTreeSet::from([left.project_bucket.clone()]),
                    )
                }
                TaskVerifySubcommand::Rename {
                    work_item_id,
                    title,
                } => {
                    let work_item_id = WorkItemId(work_item_id);
                    let work_item = store
                        .work_item(&work_item_id)?
                        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
                    (
                        store.upsert_task_verification(TaskVerificationAction::Rename {
                            work_item_id: work_item_id.clone(),
                            anchor_span_id: work_item.anchor_span_id.clone(),
                            title,
                        })?,
                        BTreeSet::from([work_item.project_bucket.clone()]),
                    )
                }
            };
            let rebuilt = store.rebuild_task_work_items_for_project_buckets(&buckets)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "verification": verification,
                    "work_items_rebuilt": rebuilt
                }))?
            );
        }
        TaskSubcommand::Stats { json } => {
            let stats = store.task_stats()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stats_json_value(&stats))?
                );
            } else {
                println!(
                    "task stats: spans={} work_items={} verified={:.1}% no_git={:.1}% cross_provider={:.1}% rejected_meta={:.1}% avg_spans_per_item={:.2}",
                    format_u64(stats.total_spans),
                    format_u64(stats.total_work_items),
                    stats.verified_percentage,
                    stats.no_git_percentage,
                    stats.cross_provider_percentage,
                    stats.rejected_meta_percentage,
                    stats.average_spans_per_work_item
                );
            }
        }
        TaskSubcommand::Benchmark { json } => {
            let report = store.task_benchmark_report()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&benchmark_json_value(&report))?
                );
            } else {
                println!(
                    "benchmark: verified_spans={} adjacent_pairs={} has_ground_truth={} has_pairwise_ground_truth={} constraints_preserved={} beats_all_baselines={} shipping_gate_ready={}",
                    format_u64(report.verified_spans),
                    format_u64(report.verified_adjacent_pairs),
                    report.has_verified_ground_truth,
                    report.has_verified_pairwise_ground_truth,
                    report.manual_constraints_preserved,
                    report.beats_all_baselines,
                    report.shipping_gate_ready
                );
                if !report.gate_blockers.is_empty() {
                    println!("  gate_blockers={}", report.gate_blockers.join(","));
                }
                if !report.failing_baselines.is_empty() {
                    println!("  failing_baselines={}", report.failing_baselines.join(","));
                }
                if !report.has_verified_ground_truth {
                    println!(
                        "  note: no verified task ground truth yet; run `statsai task verify ...` before treating benchmark scores as a shipping gate"
                    );
                } else if !report.has_verified_pairwise_ground_truth {
                    println!(
                        "  note: verified labels exist, but no adjacent verified span pairs exist yet; verify a multi-span work item or record a split/merge before using the shipping gate"
                    );
                }
                print_benchmark_metrics("current", &report.current);
                for baseline in &report.baselines {
                    print_benchmark_metrics(&baseline.name, &baseline.metrics);
                }
            }
        }
        TaskSubcommand::Export { level, format } => {
            let level = level.to_ascii_lowercase();
            let format = format.to_ascii_lowercase();
            match (level.as_str(), format.as_str()) {
                ("work-item", "json") | ("work_item", "json") => {
                    println!("{}", serde_json::to_string_pretty(&store.work_items()?)?);
                }
                ("work-item", "jsonl") | ("work_item", "jsonl") => {
                    for item in store.work_items()? {
                        println!("{}", serde_json::to_string(&item)?);
                    }
                }
                ("span", "json") => {
                    println!("{}", serde_json::to_string_pretty(&store.task_spans()?)?);
                }
                ("span", "jsonl") => {
                    for span in store.task_spans()? {
                        println!("{}", serde_json::to_string(&span)?);
                    }
                }
                _ => bail!("unsupported export level/format: {level}/{format}"),
            }
        }
        TaskSubcommand::Rebuild {
            provider,
            source_id,
            all,
        } => {
            let report = if all || (provider.is_none() && source_id.is_none()) {
                store.rebuild_all_task_work_items_report()?
            } else {
                let buckets = selected_rebuild_project_buckets(
                    store,
                    provider.as_deref(),
                    source_id.as_deref(),
                )?;
                store.rebuild_task_work_items_for_project_buckets_report(&buckets)?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&task_rebuild_report_json_value(&report))?
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskListSelection {
    pub(crate) items: Vec<WorkItem>,
    pub(crate) hidden_rejected_meta: usize,
}

pub(crate) fn task_list_selection(
    store: &Store,
    provider: Option<&str>,
    status_filter: Option<&TaskStatus>,
) -> Result<TaskListSelection> {
    let items = store
        .work_items()?
        .into_iter()
        .filter(|item| {
            provider
                .is_none_or(|provider| item.providers.iter().any(|candidate| candidate == provider))
        })
        .collect::<Vec<_>>();
    if let Some(status) = status_filter {
        return Ok(TaskListSelection {
            items: items
                .into_iter()
                .filter(|item| &item.status == status)
                .collect::<Vec<_>>(),
            hidden_rejected_meta: 0,
        });
    }
    let hidden_rejected_meta = items
        .iter()
        .filter(|item| item.status == TaskStatus::RejectedMeta)
        .count();
    Ok(TaskListSelection {
        items: items
            .into_iter()
            .filter(|item| item.status != TaskStatus::RejectedMeta)
            .collect::<Vec<_>>(),
        hidden_rejected_meta,
    })
}

#[cfg(test)]
pub(crate) fn filtered_task_list_items(
    store: &Store,
    provider: Option<&str>,
    status_filter: Option<&TaskStatus>,
) -> Result<Vec<WorkItem>> {
    Ok(task_list_selection(store, provider, status_filter)?.items)
}

pub(crate) fn selected_rebuild_project_buckets(
    store: &Store,
    provider: Option<&str>,
    source_id: Option<&str>,
) -> Result<BTreeSet<String>> {
    Ok(store
        .task_spans()?
        .into_iter()
        .filter(|span| provider.is_none_or(|provider| span.provider == provider))
        .filter(|span| source_id.is_none_or(|source_id| span.source_id.0 == source_id))
        .map(|span| span.project_bucket)
        .collect::<BTreeSet<_>>())
}

#[derive(Debug, Serialize)]
pub(crate) struct TaskShowOutput {
    pub(crate) work_item: WorkItem,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) spans: Vec<TaskSpan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) verifications: Vec<TaskVerification>,
}

pub(crate) fn load_task_show_output(
    store: &Store,
    work_item_id: &WorkItemId,
    include_evidence: bool,
) -> Result<TaskShowOutput> {
    let work_item = store
        .work_item(work_item_id)?
        .with_context(|| format!("unknown work item {}", work_item_id.0))?;
    let spans = if include_evidence {
        store.task_spans_for_work_item(work_item_id)?
    } else {
        Vec::new()
    };
    let verifications = if include_evidence {
        relevant_task_verifications(&store.task_verifications()?, &spans)
    } else {
        Vec::new()
    };
    Ok(TaskShowOutput {
        work_item,
        spans,
        verifications,
    })
}

pub(crate) fn relevant_task_verifications(
    verifications: &[TaskVerification],
    spans: &[TaskSpan],
) -> Vec<TaskVerification> {
    let span_ids = spans
        .iter()
        .map(|span| span.span_id.0.as_str())
        .collect::<HashSet<_>>();
    verifications
        .iter()
        .filter(|verification| {
            verification
                .action
                .span_ids()
                .into_iter()
                .any(|span_id| span_ids.contains(span_id.0.as_str()))
        })
        .cloned()
        .collect()
}

pub(crate) fn format_task_verification(verification: &TaskVerification) -> String {
    match &verification.action {
        TaskVerificationAction::Accept { anchor_span_id, .. } => {
            format!("accept(anchor={})", anchor_span_id.0)
        }
        TaskVerificationAction::Reject {
            anchor_span_id,
            reason,
            ..
        } => format!("reject(anchor={}, reason={:?})", anchor_span_id.0, reason),
        TaskVerificationAction::Rename {
            anchor_span_id,
            title,
            ..
        } => format!("rename(anchor={}, title={title})", anchor_span_id.0),
        TaskVerificationAction::Split {
            after_span_id,
            before_span_id,
            left_title,
            right_title,
        } => format!(
            "split(after={}, before={}, left_title={}, right_title={})",
            after_span_id.0,
            before_span_id
                .as_ref()
                .map(|span_id| span_id.0.as_str())
                .unwrap_or("-"),
            left_title.as_deref().unwrap_or("-"),
            right_title.as_deref().unwrap_or("-")
        ),
        TaskVerificationAction::Merge {
            left_anchor_span_id,
            right_anchor_span_id,
            title,
            ..
        } => format!(
            "merge(left={}, right={}, title={})",
            left_anchor_span_id.0,
            right_anchor_span_id.0,
            title.as_deref().unwrap_or("-")
        ),
    }
}

pub(crate) fn parse_task_status_filter(value: &str) -> Result<TaskStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(TaskStatus::Auto),
        "needs_review" => Ok(TaskStatus::NeedsReview),
        "verified" => Ok(TaskStatus::Verified),
        "rejected_meta" => Ok(TaskStatus::RejectedMeta),
        _ => bail!("unsupported task status {value}"),
    }
}

pub(crate) fn parse_task_verdict(value: &str) -> Result<TaskVerdict> {
    match value.trim().to_ascii_lowercase().as_str() {
        "meta" => Ok(TaskVerdict::Meta),
        "system" => Ok(TaskVerdict::System),
        "noise" => Ok(TaskVerdict::Noise),
        _ => bail!("unsupported task verdict {value}"),
    }
}

pub(crate) fn print_work_item(work_item: &WorkItem) {
    println!(
        "{} status={:?} confidence={:?} spans={} events={} tokens={} providers={} title={}",
        work_item.work_item_id.0,
        work_item.status,
        work_item.confidence,
        work_item.span_count,
        work_item.event_count,
        format_u64(work_item.total_tokens),
        work_item.providers.join(","),
        work_item.title
    );
    println!(
        "  project_bucket={} started_at={} ended_at={} no_git={} cross_provider={}",
        work_item.project_bucket,
        work_item.started_at.to_rfc3339(),
        work_item.ended_at.to_rfc3339(),
        work_item.no_git,
        work_item.cross_provider
    );
    if !work_item.review_reasons.is_empty() {
        println!("  review_reasons={}", work_item.review_reasons.join(","));
    }
    if !work_item.continuation_reasons.is_empty() {
        println!(
            "  continuation_reasons={}",
            work_item.continuation_reasons.join(",")
        );
    }
}

pub(crate) fn format_task_list_item(work_item: &WorkItem) -> String {
    let mut line = format!(
        "{} status={:?} confidence={:?} spans={} tokens={} providers={} title={}",
        work_item.work_item_id.0,
        work_item.status,
        work_item.confidence,
        work_item.span_count,
        format_u64(work_item.total_tokens),
        work_item.providers.join(","),
        work_item.title
    );
    if !work_item.review_reasons.is_empty() {
        line.push_str(" review=");
        line.push_str(&work_item.review_reasons.join(","));
    }
    line
}

pub(crate) fn stats_json_value(stats: &statsai_store::TaskStats) -> Value {
    json!({
        "total_spans": stats.total_spans,
        "total_work_items": stats.total_work_items,
        "verified_percentage": stats.verified_percentage,
        "no_git_percentage": stats.no_git_percentage,
        "cross_provider_percentage": stats.cross_provider_percentage,
        "rejected_meta_percentage": stats.rejected_meta_percentage,
        "average_spans_per_work_item": stats.average_spans_per_work_item,
    })
}

pub(crate) fn task_rebuild_report_json_value(report: &statsai_store::TaskRebuildReport) -> Value {
    json!({
        "work_items_rebuilt": report.work_items_rebuilt,
        "work_items_deleted": report.work_items_deleted,
        "affected_bucket_count": report.affected_bucket_count,
        "affected_segment_count": report.affected_segment_count,
        "touched_span_count": report.touched_span_count,
        "timings_ms": {
            "delete": report.timings.delete_ms,
            "span_load": report.timings.span_load_ms,
            "verification_load": report.timings.verification_load_ms,
            "grouping": report.timings.grouping_ms,
            "title_selection": report.timings.title_selection_ms,
            "insert": report.timings.insert_ms,
        }
    })
}

pub(crate) fn benchmark_json_value(report: &statsai_store::TaskBenchmarkReport) -> Value {
    json!({
        "verified_adjacent_pairs": report.verified_adjacent_pairs,
        "verified_spans": report.verified_spans,
        "has_verified_ground_truth": report.has_verified_ground_truth,
        "has_verified_pairwise_ground_truth": report.has_verified_pairwise_ground_truth,
        "manual_constraints_preserved": report.manual_constraints_preserved,
        "beats_all_baselines": report.beats_all_baselines,
        "shipping_gate_ready": report.shipping_gate_ready,
        "failing_baselines": report.failing_baselines,
        "gate_blockers": report.gate_blockers,
        "current": benchmark_metrics_json_value(&report.current),
        "baselines": report.baselines.iter().map(|baseline| {
            json!({
                "name": baseline.name,
                "metrics": benchmark_metrics_json_value(&baseline.metrics),
            })
        }).collect::<Vec<_>>(),
    })
}

pub(crate) fn benchmark_metrics_json_value(metrics: &statsai_store::TaskBenchmarkMetrics) -> Value {
    json!({
        "adjacent_precision": metrics.adjacent_precision,
        "adjacent_recall": metrics.adjacent_recall,
        "adjacent_f1": metrics.adjacent_f1,
        "cluster_precision": metrics.cluster_precision,
        "cluster_recall": metrics.cluster_recall,
        "cluster_f1": metrics.cluster_f1,
        "meta_precision": metrics.meta_precision,
        "meta_recall": metrics.meta_recall,
        "meta_f1": metrics.meta_f1,
    })
}

pub(crate) fn print_benchmark_metrics(name: &str, metrics: &statsai_store::TaskBenchmarkMetrics) {
    println!(
        "  {} adjacent_f1={:.3} (p={:.3} r={:.3}) cluster_f1={:.3} (p={:.3} r={:.3}) meta_f1={:.3} (p={:.3} r={:.3})",
        name,
        metrics.adjacent_f1,
        metrics.adjacent_precision,
        metrics.adjacent_recall,
        metrics.cluster_f1,
        metrics.cluster_precision,
        metrics.cluster_recall,
        metrics.meta_f1,
        metrics.meta_precision,
        metrics.meta_recall
    );
}
