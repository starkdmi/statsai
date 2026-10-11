//! The form local records take in a sync batch.
//!
//! `statsai sync` builds every batch through these functions, so a program
//! that reads the store and applies them removes and rewrites the same fields
//! a sync does. Some things stay outside this module:
//!
//! - Selection by sync state (what is pending for a target) belongs to the
//!   store's sync ledger.
//! - Upkeep a sync runs before reading, such as rebuilding stale daily rollups
//!   or refreshing code-change metrics.
//! - The device's opt-ins and the target's capabilities. The caller passes
//!   them in as `include_projects` and `accepts_cache_health`. A function
//!   that takes `include_projects` drops the record's project unless it is
//!   set. The task sanitizers take no such flag and keep projects, because a
//!   sync sends tasks only when projects are included.
//!
//! The summary, session and task transforms show path labels under the home
//! directory as `~/...`, relative to the home directory of the process that
//! calls them. The event sanitizer leaves an event's path labels as stored.
//!
//! The sanitizers for summaries, sessions, tasks and code-change metrics live
//! in `statsai-core` with their types, and are re-exported here so that this
//! module lists every transform a batch applies.

use chrono::Duration;
use statsai_core::{
    project_contains_file_paths, IdentitySource, ProviderAccount, SessionRollupV1,
    SourceAccountAssignment, SourceKind, SourceLocation, Subscription, UsageEvent, UsageSummary,
};

pub use statsai_core::{
    collapse_summary_home_for_sync, sanitize_code_change_metric_for_sync,
    sanitize_project_for_sync, sanitize_session_rollup_for_sync,
    sanitize_session_rollup_for_sync_with_home, sanitize_summary_for_sync,
    sanitize_task_bucket_for_sync, sanitize_task_bucket_for_sync_with_home,
    sanitize_task_span_for_sync, sanitize_task_span_for_sync_with_home,
    sanitize_work_item_for_sync, sanitize_work_item_for_sync_with_home,
    TASK_BUCKET_SYNC_SANITIZER_VERSION,
};

/// A summary as a sync batch carries it to a target:
/// [`sanitize_summary_for_sync_with_projects`], then
/// [`summary_for_sync_target`].
#[must_use]
pub fn summary_for_sync(
    summary: UsageSummary,
    include_projects: bool,
    accepts_cache_health: bool,
) -> UsageSummary {
    summary_for_sync_target(
        sanitize_summary_for_sync_with_projects(summary, include_projects),
        accepts_cache_health,
    )
}

/// The part of [`summary_for_sync`] that does not depend on the target: the
/// summary sanitized, without a project unless projects are included, and
/// with a project path label under the home directory shown as `~/...`.
/// Never stored locally.
#[must_use]
pub fn sanitize_summary_for_sync_with_projects(
    summary: UsageSummary,
    include_projects: bool,
) -> UsageSummary {
    let mut summary = sanitize_summary_for_sync(summary);
    if !include_projects {
        summary.project = None;
    }
    collapse_summary_home_for_sync(summary, statsai_core::home_dir().as_deref())
}

/// A summary in the form a target stores it. Selection and the recorded
/// payload hash both see this form, so a receiver without `cache_health`
/// support is not re-sent summaries that differ only by it.
#[must_use]
pub fn summary_for_sync_target(summary: UsageSummary, accepts_cache_health: bool) -> UsageSummary {
    if accepts_cache_health {
        summary
    } else {
        summary.without_cache_health()
    }
}

/// Whether the summary is one of the store's daily rollups, which an HTTP
/// rollup batch sends for every day with usage.
#[must_use]
pub fn is_daily_rollup_summary(summary: &UsageSummary) -> bool {
    summary.metadata.summary_format == "daily_rollup.v1"
}

/// Whether an HTTP rollup batch sends this stored summary alongside the daily
/// rollups.
///
/// - Daily rollups, the Claude stats cache and summaries from a
///   `local_summary` source never go.
/// - Any other summary from a `local_adapter` source always goes.
/// - A daily report (`manual_daily`, `external_daily`, `custom_daily`,
///   `ccusage_daily`) goes when it covers one report day: its bounds fall on
///   one UTC date, or are at most 25 hours apart.
/// - A period report (`manual_period_summary`, `custom_period_summary`) goes
///   when its bounds fall on different UTC dates.
/// - Other formats never go.
///
/// A missing bound falls back to the other bound, then to `observed_at`.
/// Bounds that run backwards are not swapped, so a daily report whose end
/// precedes its start on an earlier date is not sent.
#[must_use]
pub fn is_http_rollup_passthrough_summary(summary: &UsageSummary) -> bool {
    if is_daily_rollup_summary(summary) {
        return false;
    }
    if summary.metadata.summary_format == "claude_stats_cache" {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalSummary {
        return false;
    }
    if summary.source.source_kind == SourceKind::LocalAdapter {
        return true;
    }
    (is_exact_daily_passthrough_summary(summary) && summary_fits_single_daily_report_day(summary))
        || (is_exact_period_passthrough_summary(summary) && !summary_spans_single_day(summary))
}

fn summary_spans_single_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    start.date_naive() == end.date_naive()
}

fn summary_fits_single_daily_report_day(summary: &UsageSummary) -> bool {
    let start = summary
        .period_start
        .as_ref()
        .or(summary.period_end.as_ref())
        .unwrap_or(&summary.observed_at);
    let end = summary
        .period_end
        .as_ref()
        .or(summary.period_start.as_ref())
        .unwrap_or(&summary.observed_at);
    if start.date_naive() == end.date_naive() {
        return true;
    }
    let duration = *end - *start;
    duration >= Duration::zero() && duration <= Duration::hours(25)
}

fn is_exact_daily_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "external_daily" | "manual_daily" | "custom_daily" | "ccusage_daily"
    )
}

fn is_exact_period_passthrough_summary(summary: &UsageSummary) -> bool {
    matches!(
        summary.metadata.summary_format.as_str(),
        "manual_period_summary" | "custom_period_summary"
    )
}

/// A usage event as a raw sync batch carries it. Per-call timing, sub-agent
/// identity and record locators stay on the device; a project without a
/// stable identity is dropped.
#[must_use]
pub fn sanitize_event_for_sync(mut event: UsageEvent) -> UsageEvent {
    // Per-call timing and sub-agent identity stay on the device. Daily
    // summaries carry the counts derived from them.
    event.context = None;
    event.source.source_record_id = None;
    if let Some(evidence) = event.parse_evidence.as_mut() {
        evidence.source_line_number = None;
        evidence.source_record_id = None;
    }
    event.project = event.project.and_then(sanitize_project_for_sync);
    if project_contains_file_paths(event.project.as_ref()) {
        event.privacy.contains_file_paths = true;
    }
    event
}

/// [`sanitize_event_for_sync`], without the project unless projects are
/// included.
#[must_use]
pub fn sanitize_event_for_sync_with_projects(
    event: UsageEvent,
    include_projects: bool,
) -> UsageEvent {
    let mut event = sanitize_event_for_sync(event);
    if !include_projects {
        event.project = None;
    }
    event
}

/// [`sanitize_session_rollup_for_sync`], without the project unless projects
/// are included.
#[must_use]
pub fn sanitize_session_rollup_for_sync_with_projects(
    rollup: SessionRollupV1,
    include_projects: bool,
) -> SessionRollupV1 {
    let mut rollup = sanitize_session_rollup_for_sync(rollup);
    if !include_projects {
        rollup.project = None;
    }
    rollup
}

/// A source without its local path label.
#[must_use]
pub fn sanitize_source_for_sync(mut source: SourceLocation) -> SourceLocation {
    source.path_label = None;
    source
}

/// An account without its plan, and without its label unless the user set
/// it.
#[must_use]
pub fn sanitize_account_for_sync(mut account: ProviderAccount) -> ProviderAccount {
    if !matches!(account.identity_source, IdentitySource::UserConfigured) {
        account.account_label = None;
    }
    // The email and provider user id stay. They are how a person tells one of
    // their own accounts from another in the dashboard, which is the whole
    // point of syncing accounts at all; stripping them left every account
    // showing as a bare `acct_` hash. The new evidence types are the ones that
    // must never carry them -- those travel as hashes and are covered by their
    // own contracts. `plan_name` still goes, because a plan is now evidence
    // rather than an account attribute.
    account.plan_name = None;
    account
}

/// A source-account assignment, which is sent unchanged.
#[must_use]
pub fn sanitize_source_account_assignment_for_sync(
    assignment: SourceAccountAssignment,
) -> SourceAccountAssignment {
    assignment
}

/// A subscription without its notes. Check [`is_syncable_subscription`]
/// first: some subscriptions are never sent.
#[must_use]
pub fn sanitize_subscription_for_sync(mut subscription: Subscription) -> Subscription {
    subscription.notes = None;
    subscription
}

/// Local-auth subscriptions are evidence this device derived for itself and are
/// never uploaded, so every count that is compared against the remote mirror has
/// to drop them the same way the batch does. Counting them as pending would keep
/// the subscription mirror check permanently disabled; counting them in the total
/// would report a gap the remote can never close.
#[must_use]
pub fn is_syncable_subscription(subscription: &Subscription) -> bool {
    subscription.record_source != IdentitySource::LocalAuth
}
