use super::support::*;
use super::*;
use crate::report::{preview_path_label, EventUsageSeries};
use chrono::Duration;
#[test]
fn report_empty_inputs_returns_zero_totals() {
    let now = mk_dt(2026, 5, 25);
    let report = build_usage_report(&[], &[], &[], &[], &[], ReportPeriod::AllTime, now);
    assert_eq!(report.total_events, 0);
    assert_eq!(report.total_usage.total_tokens, 0);
    assert!(report.rows.is_empty());
    assert!(report.summary_rows.is_empty());
}

#[test]
fn report_filters_events_by_period() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let recent = test_event("codex", &source, mk_dt(2026, 5, 24), 100, None);
    let old = test_event("codex", &source, mk_dt(2026, 5, 10), 200, None);

    let report = build_usage_report(
        &[recent, old],
        &[],
        &[source],
        &[],
        &[],
        ReportPeriod::LastDays(7),
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_filters_events_by_explicit_date_range() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let before = test_event("codex", &source, mk_dt(2026, 4, 30), 50, None);
    let inside = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(15),
        100,
        None,
    );
    let after = test_event("codex", &source, mk_dt(2026, 5, 20), 200, None);
    let period =
        report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now).expect("valid range");

    let report = build_usage_report(
        &[before, inside, after],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(report.label, "2026-05-01 to 2026-05-15");
    assert_eq!(report.since, Some(mk_dt(2026, 5, 1)));
    assert_eq!(
        report.until,
        parse_report_date_bound("2026-05-15", true).expect("end of day")
    );
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_date_only_to_includes_the_whole_utc_day() {
    let now = mk_dt(2026, 6, 1);
    let source = test_source("codex", "/tmp/codex");
    let late_on_end_day = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 15) + Duration::hours(23),
        75,
        None,
    );
    let next_day = test_event("codex", &source, mk_dt(2026, 5, 16), 25, None);
    let period = report_period_from_range(Some("2026-05-15"), Some("2026-05-15"), now)
        .expect("single-day range");

    let report = build_usage_report(
        &[late_on_end_day, next_day],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 75);
}

#[test]
fn report_range_from_only_defaults_until_to_now() {
    let now = mk_dt(2026, 5, 25);
    let period = report_period_from_range(Some("2026-05-01"), None, now).expect("from-only range");
    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 5, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: now,
                date_only: false,
            },
        }
    );
    assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T00:00:00+00:00");
}

#[test]
fn report_range_rejects_inverted_and_invalid_bounds() {
    let now = mk_dt(2026, 5, 25);
    assert_eq!(
        report_period_from_range(Some("2026-05-20"), Some("2026-05-10"), now),
        Err(ReportRangeError::InvertedRange {
            since: mk_dt(2026, 5, 20),
            until: parse_report_date_bound("2026-05-10", true).expect("end of day"),
        })
    );
    assert!(matches!(
        report_period_from_range(Some("last-week"), None, now),
        Err(ReportRangeError::InvalidDate { .. })
    ));
    assert_eq!(
        report_period_from_range(None, None, now),
        Err(ReportRangeError::MissingBound)
    );
}

#[test]
fn report_range_keeps_future_windows_and_returns_no_events() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let present = test_event("codex", &source, now, 50, None);
    let period = report_period_from_range(Some("2026-09-01"), Some("2026-09-30"), now)
        .expect("future range is valid");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 9, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: parse_report_date_bound("2026-09-30", true).expect("end of day"),
                date_only: true,
            },
        }
    );
    assert_eq!(period.label(now), "2026-09-01 to 2026-09-30 (empty)");
    assert_eq!(period.window(now), (Some(mk_dt(2026, 9, 1)), now));
    assert_eq!(period.published_window(now), (Some(now), now));

    let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
    assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn report_range_from_only_in_the_future_is_empty_not_inverted() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let present = test_event("codex", &source, now, 50, None);
    let period = report_period_from_range(Some("2026-09-01"), None, now).expect("future from-only");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 9, 1),
                date_only: true,
            }),
            until: ReportBound {
                timestamp: now,
                date_only: false,
            },
        }
    );
    assert_eq!(period.label(now), "from 2026-09-01 (empty)");
    assert_eq!(period.published_window(now), (Some(now), now));

    let report = build_usage_report(&[present], &[], &[source], &[], &[], period, now);
    assert_eq!(report.label, "from 2026-09-01 (empty)");
    assert_eq!(report.since, Some(now));
    assert_eq!(report.until, now);
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
}

#[test]
fn report_range_label_uses_applied_until_when_to_is_in_the_future() {
    let now = mk_dt(2026, 5, 25) + Duration::hours(12);
    let period = report_period_from_range(Some("2026-05-01"), Some("2026-12-31"), now)
        .expect("open-ended future to");
    assert_eq!(period.label(now), "2026-05-01 to 2026-05-25T12:00:00+00:00");
    assert_eq!(period.window(now), (Some(mk_dt(2026, 5, 1)), now));
}

#[test]
fn report_filters_events_by_rfc3339_clock_bounds() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let before = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(14),
        50,
        None,
    );
    let inside = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(15) + Duration::minutes(30),
        100,
        None,
    );
    let after = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 10) + Duration::hours(16) + Duration::minutes(1),
        200,
        None,
    );
    let period = report_period_from_range(
        Some("2026-05-10T15:00:00Z"),
        Some("2026-05-10T16:00:00Z"),
        now,
    )
    .expect("rfc3339 range");

    let report = build_usage_report(
        &[before, inside, after],
        &[],
        &[source],
        &[],
        &[],
        period,
        now,
    );

    assert_eq!(
        report.label,
        "2026-05-10T15:00:00+00:00 to 2026-05-10T16:00:00+00:00"
    );
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 100);
}

#[test]
fn report_range_rfc3339_midnight_keeps_timestamp_label() {
    let now = mk_dt(2026, 5, 25);
    let period = report_period_from_range(
        Some("2026-05-01T00:00:00Z"),
        Some("2026-05-15T23:59:59.999999999Z"),
        now,
    )
    .expect("rfc3339 midnight range");

    assert_eq!(
        period,
        ReportPeriod::Range {
            since: Some(ReportBound {
                timestamp: mk_dt(2026, 5, 1),
                date_only: false,
            }),
            until: ReportBound {
                timestamp: parse_report_date_bound("2026-05-15", true).expect("end of day"),
                date_only: false,
            },
        }
    );
    assert_eq!(
        period.label(now),
        "2026-05-01T00:00:00+00:00 to 2026-05-15T23:59:59.999999999+00:00"
    );
}

#[test]
fn report_range_to_today_clamps_until_to_now() {
    let now = mk_dt(2026, 5, 25) + Duration::hours(12);
    let source = test_source("codex", "/tmp/codex");
    let morning = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 25) + Duration::hours(8),
        75,
        None,
    );
    let evening = test_event(
        "codex",
        &source,
        mk_dt(2026, 5, 25) + Duration::hours(18),
        25,
        None,
    );
    let period =
        report_period_from_range(Some("2026-05-25"), Some("2026-05-25"), now).expect("today");

    let report = build_usage_report(&[morning, evening], &[], &[source], &[], &[], period, now);
    assert_eq!(report.until, now);
    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 75);
}

#[test]
fn report_filters_out_future_events() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex");
    let future = test_event("codex", &source, mk_dt(2026, 6, 1), 100, None);
    let present = test_event("codex", &source, now, 50, None);

    let report = build_usage_report(
        &[future, present],
        &[],
        &[source],
        &[],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.total_usage.total_tokens, 50);
}

#[test]
fn report_groups_events_by_provider_and_account() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let e1 = test_event("codex", &src, now, 100, None);
    let e2 = test_event("codex", &src, now, 200, None);

    let report = build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].provider, "codex");
    assert_eq!(report.rows[0].events, 2);
    assert_eq!(report.rows[0].usage.total_tokens, 300);
}

#[test]
fn report_keeps_summaries_separate_from_events() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("claude_code", "/tmp/claude");
    let event = test_event("claude_code", &src, now, 100, None);
    let summary = test_summary(
        "claude_code",
        &src,
        now,
        mk_dt(2026, 5, 1),
        mk_dt(2026, 5, 25),
        500,
    );

    let report = build_usage_report(
        &[event],
        &[summary],
        &[src],
        &[],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.total_usage.total_tokens, 100);
    assert_eq!(report.total_summary_usage.total_tokens, 500);
    assert_eq!(report.summary_rows.len(), 1);
    // Direct event usage within summary period
    assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);
}

#[test]
fn event_usage_series_preserves_legacy_cent_fallback_in_range_differences() {
    let source = test_source("codex", "/tmp/codex");
    let before_range_at = mk_dt(2026, 5, 1);
    let inside_range_at = mk_dt(2026, 5, 2);
    let legacy_cents = i64::MAX / MICRO_USD_PER_CENT + 1;
    let before_range = test_event(
        "codex",
        &source,
        before_range_at,
        100,
        Some(legacy_cents + 7),
    );
    let inside_range = test_event(
        "codex",
        &source,
        inside_range_at,
        200,
        Some(legacy_cents + 11),
    );
    let series = EventUsageSeries::from_events(vec![&before_range, &inside_range]);

    let (events, usage) =
        series.usage_between(inside_range_at, inside_range_at + Duration::days(1), false);

    assert_eq!(events, 1);
    assert_eq!(usage.total_tokens, 200);
    assert_eq!(usage.estimated_cost_usd, Some(legacy_cents + 11));
    assert_eq!(usage.estimated_cost_micro_usd, None);
}

#[test]
fn report_hides_summaries_in_non_alltime_periods() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("claude_code", "/tmp/claude");
    let summary = test_summary(
        "claude_code",
        &src,
        now,
        mk_dt(2026, 5, 1),
        mk_dt(2026, 5, 25),
        500,
    );

    let report = build_usage_report(
        &[],
        &[summary],
        &[src],
        &[],
        &[],
        ReportPeriod::LastDays(7),
        now,
    );

    assert!(report.summary_rows.is_empty());
}

#[test]
fn subscription_rows_respect_past_end_time() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut before_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
    before_end.provider_account_id = Some(account_id.clone());
    let mut after_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
    after_end.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: Some(mk_dt(2026, 5, 30)),
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Cancelled,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };

    let report = build_usage_report(
        &[before_end, after_end],
        &[],
        &[src],
        &[account],
        &[subscription],
        ReportPeriod::LastDays(30),
        now,
    );

    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].account, account_id.0);
    assert_eq!(
        report.subscription_rows[0].ended_at,
        Some(mk_dt(2026, 5, 30))
    );
    assert_eq!(report.subscription_rows[0].events, 1);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(100)
    );
}

#[test]
fn subscription_rows_respect_historical_range_until() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex-range-sub");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut inside = test_event("codex", &src, mk_dt(2026, 5, 10), 100, Some(100));
    inside.provider_account_id = Some(account_id.clone());
    let mut after_range = test_event("codex", &src, mk_dt(2026, 5, 20), 200, Some(200));
    after_range.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id.clone(),
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: None,
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };
    let period = report_period_from_range(Some("2026-05-01"), Some("2026-05-15"), now)
        .expect("historical range");

    let report = build_usage_report(
        &[inside, after_range],
        &[],
        &[src],
        &[account],
        &[subscription],
        period,
        now,
    );

    assert_eq!(report.total_events, 1);
    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].events, 1);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 100);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(100)
    );
}

#[test]
fn subscription_rows_exclude_future_range() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex-future-sub");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut present = test_event("codex", &src, now, 100, Some(100));
    present.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id,
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: None,
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::UserConfigured,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };
    let period = report_period_from_range(Some("2026-09-01"), Some("2026-09-30"), now)
        .expect("future range");

    let report = build_usage_report(
        &[present],
        &[],
        &[src],
        &[account],
        &[subscription],
        period,
        now,
    );

    assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
    assert!(report.since.is_some_and(|since| since <= report.until));
    assert_eq!(report.total_events, 0);
    assert!(report.subscription_rows.is_empty());
}

#[test]
fn subscription_rows_keep_legacy_verified_cycle_rows_open() {
    let now = mk_dt(2026, 6, 1);
    let src = test_source("codex", "/tmp/codex");
    let account_id = provider_account_id("codex", "email:verified@example.com");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: account_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::LocalAuth,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: Some("verified@example.com".to_string()),
        email_hash: None,
        org_id_hash: None,
        account_label: None,
        plan_name: Some("Plus".to_string()),
        confidence: Confidence::High,
        verified_at: Some(mk_dt(2026, 5, 3)),
        created_at: mk_dt(2026, 5, 3),
        updated_at: mk_dt(2026, 5, 3),
    };
    let mut before_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 29), 100, Some(100));
    before_cycle_end.provider_account_id = Some(account_id.clone());
    let mut after_cycle_end = test_event("codex", &src, mk_dt(2026, 5, 31), 200, Some(200));
    after_cycle_end.provider_account_id = Some(account_id.clone());
    let subscription = Subscription {
        schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
        subscription_id: subscription_id("codex", &account_id, "Plus", mk_dt(2026, 4, 30)),
        provider: "codex".to_string(),
        provider_account_id: account_id,
        plan_name: "Plus".to_string(),
        price: 2000,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at: Some(mk_dt(2026, 4, 30)),
        renewal_day: Some(30),
        started_at: mk_dt(2026, 4, 30),
        ended_at: Some(mk_dt(2026, 5, 30)),
        current_period_ends_at: Some(mk_dt(2026, 5, 30)),
        status: SubscriptionStatus::Active,
        record_source: IdentitySource::LocalAuth,
        verified_at: Some(mk_dt(2026, 5, 3)),
        notes: None,
    };

    let report = build_usage_report(
        &[before_cycle_end, after_cycle_end],
        &[],
        &[src],
        &[account],
        &[subscription],
        ReportPeriod::LastDays(30),
        now,
    );

    assert_eq!(report.subscription_rows.len(), 1);
    assert_eq!(report.subscription_rows[0].ended_at, None);
    assert_eq!(report.subscription_rows[0].events, 2);
    assert_eq!(report.subscription_rows[0].usage.total_tokens, 300);
    assert_eq!(
        report.subscription_rows[0].usage.estimated_cost_usd,
        Some(300)
    );
}

#[test]
fn report_uses_account_label_from_registry() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let acct_id = provider_account_id("codex", "stable");
    let account = ProviderAccount {
        schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
        provider_account_id: acct_id.clone(),
        provider: "codex".to_string(),
        identity_source: IdentitySource::UserConfigured,
        provider_user_id: None,
        provider_user_id_hash: None,
        email: None,
        email_hash: None,
        org_id_hash: None,
        account_label: Some("work".to_string()),
        plan_name: None,
        confidence: Confidence::Medium,
        verified_at: None,
        created_at: now,
        updated_at: now,
    };
    let mut event = test_event("codex", &src, now, 50, None);
    event.provider_account_id = Some(acct_id);

    let report = build_usage_report(
        &[event],
        &[],
        &[src],
        &[account],
        &[],
        ReportPeriod::AllTime,
        now,
    );

    assert_eq!(report.rows[0].account, "work");
}

#[test]
fn usage_totals_accumulate_cost() {
    let now = mk_dt(2026, 5, 25);
    let src = test_source("codex", "/tmp/codex");
    let e1 = test_event("codex", &src, now, 100, Some(1));
    let e2 = test_event("codex", &src, now, 200, Some(2));

    let report = build_usage_report(&[e1, e2], &[], &[src], &[], &[], ReportPeriod::AllTime, now);

    assert_eq!(report.total_usage.estimated_cost_usd, Some(3));
}
#[test]
fn usage_totals_saturate_imported_counters_and_costs() {
    let now = mk_dt(2026, 5, 25);
    let source = test_source("codex", "/tmp/codex-overflow");
    let mut event = test_event("codex", &source, now, u64::MAX, Some(i64::MAX));
    event.usage = UsageCounts {
        input_tokens: Some(u64::MAX),
        cache_creation_tokens: Some(u64::MAX),
        cache_read_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        reasoning_tokens: Some(u64::MAX),
        total_tokens: Some(u64::MAX),
        ..UsageCounts::default()
    };

    let mut totals = UsageTotals::default();
    totals.add_event(&event);
    totals.add_event(&event);

    assert_eq!(totals.input_tokens, u64::MAX);
    assert_eq!(totals.cache_creation_tokens, u64::MAX);
    assert_eq!(totals.cached_input_tokens, u64::MAX);
    assert_eq!(totals.output_tokens, u64::MAX);
    assert_eq!(totals.reasoning_tokens, u64::MAX);
    assert_eq!(totals.total_tokens, u64::MAX);
    assert_eq!(totals.estimated_cost_usd, Some(i64::MAX));
}
#[test]
fn preview_path_label_uses_display_label() {
    let mut source = test_source("codex", "/tmp/codex");
    source.path_label = Some("/home/testuser/work/codex".to_string());
    let preview = preview_path_label(&source);
    // if home matches, abbreviates; else full
    assert!(preview.contains("codex") || preview.contains("work"));
}
