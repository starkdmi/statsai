use super::support::*;
use super::*;

#[test]
fn quota_cycle_contributions_select_weekly_attributed_cycles_only() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, observed_at - Duration::days(8));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "weekly",
                "weekly",
                observed_at,
                reset.timestamp(),
                "secondary",
                10_080,
                20.0,
            ),
            sample_record(
                source_id.clone(),
                "five-hour",
                "five-hour",
                observed_at,
                (reset - Duration::hours(4)).timestamp(),
                "primary",
                300,
                40.0,
            ),
            sample_record(
                source_id,
                "monthly",
                "monthly",
                observed_at,
                (reset + Duration::days(20)).timestamp(),
                "monthly",
                43_200,
                8.0,
            ),
        ])
        .expect("observations");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert_eq!(contributions.len(), 1);
    assert_eq!(contributions[0].window_minutes, 10_080);
    assert_eq!(
        contributions[0].schema_version,
        "quota_cycle_contribution.v1"
    );
    let json = serde_json::to_value(&contributions[0]).expect("json");
    assert!(json.get("source_id").is_none());
    assert!(json.get("device_id").is_none());
    assert!(json.get("change_points").is_none());
    assert!(json.get("sample_count").is_none());
    assert!(json.get("latest_status").is_none());
    assert_eq!(json.get("has_schedule_overlap"), Some(&json!(false)));
}

#[test]
fn a_cycle_that_has_only_just_begun_survives_at_zero() {
    let store = Store::in_memory().expect("store");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, reset - Duration::days(9));
    store
        .upsert_quota_observations(&[sample_record(
            source_id,
            "fresh",
            "fresh",
            reset - Duration::days(7) + Duration::minutes(5),
            reset.timestamp(),
            "secondary",
            10_080,
            0.0,
        )])
        .expect("observation");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 1, "the newest cycle is exempt");
    assert_eq!(windows[0].maximum_used_percent, 0.0);
}

#[test]
fn quota_cycle_contributions_report_locally_reconstructed_schedule_overlaps() {
    let store = Store::in_memory().expect("store");
    // An early reset restarts the weekly schedule three days in, so the two
    // reconstructed cycles overlap for the remaining four days.
    let first_reset = DateTime::from_timestamp(1_787_616_000, 0).expect("first reset");
    let second_reset = first_reset + Duration::days(3);
    let first_start = first_reset - Duration::days(7);
    let (source_id, _) = assigned_source(&store, first_start - Duration::days(1));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "weekly-first",
                "weekly-first",
                first_start + Duration::hours(1),
                first_reset.timestamp(),
                "secondary",
                10_080,
                62.0,
            ),
            sample_record(
                source_id,
                "weekly-second",
                "weekly-second",
                second_reset - Duration::days(7) + Duration::hours(1),
                second_reset.timestamp(),
                "secondary",
                10_080,
                4.0,
            ),
        ])
        .expect("observations");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert_eq!(contributions.len(), 2);
    assert!(
        contributions
            .iter()
            .all(|contribution| contribution.has_schedule_overlap),
        "both sides of a locally reconstructed overlap carry the flag"
    );
}

#[test]
fn quota_cycle_contributions_default_schedule_overlap_when_absent_on_the_wire() {
    let wire = json!({
        "schema_version": "quota_cycle_contribution.v1",
        "contribution_id": "quota_cycle_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "provider": "codex",
        "provider_account_id": "acct_aaaaaaaaaaaaaaaaaaaaaaaa",
        "limit_id": "codex",
        "window_minutes": 10_080,
        "representative_reset": "2026-08-25T15:00:00Z",
        "representative_reset_epoch_seconds": 1_787_670_000i64,
    });
    let contribution: QuotaCycleContributionV1 =
        serde_json::from_value(wire).expect("legacy payload deserializes");
    assert!(!contribution.has_schedule_overlap);
}

#[test]
fn quota_cycle_contributions_exclude_unattributed_cycles() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "unattributed",
        Path::new("/tmp/quota-unattributed-cycle"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    store
        .upsert_quota_observations(&[sample_record(
            source.source_id,
            "unattributed-weekly",
            "unattributed-weekly",
            observed_at,
            1_787_616_000,
            "secondary",
            10_080,
            15.0,
        )])
        .expect("observation");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert!(contributions.is_empty());
}

#[test]
fn quota_cycle_contributions_build_daily_envelopes_without_carry_forward() {
    let store = Store::in_memory().expect("store");
    let day_one = DateTime::from_timestamp(1_787_011_200, 0).expect("day one");
    let day_three = day_one + Duration::days(2) + Duration::hours(3);
    let reset = day_one + Duration::days(7);
    let (source_id, _) = assigned_source(&store, day_one - Duration::days(1));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "day-one-first",
                "day-one-first",
                day_one + Duration::hours(1),
                reset.timestamp(),
                "secondary",
                10_080,
                10.0,
            ),
            sample_record(
                source_id.clone(),
                "day-one-last",
                "day-one-last",
                day_one + Duration::hours(8),
                reset.timestamp(),
                "secondary",
                10_080,
                25.0,
            ),
            sample_record(
                source_id,
                "day-three",
                "day-three",
                day_three,
                reset.timestamp(),
                "secondary",
                10_080,
                40.0,
            ),
        ])
        .expect("observations");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert_eq!(contributions.len(), 1);
    let days = contributions[0]
        .daily_envelopes
        .iter()
        .map(|envelope| envelope.day.as_str())
        .collect::<Vec<_>>();
    assert_eq!(days, ["2026-08-18", "2026-08-20"]);
    assert_eq!(contributions[0].daily_envelopes[0].first_used_percent, 10.0);
    assert_eq!(contributions[0].daily_envelopes[0].last_used_percent, 25.0);
    assert_eq!(
        contributions[0].daily_envelopes[0].minimum_used_percent,
        10.0
    );
    assert_eq!(
        contributions[0].daily_envelopes[0].maximum_used_percent,
        25.0
    );
    assert_eq!(contributions[0].daily_envelopes[1].first_used_percent, 40.0);
    assert_eq!(contributions[0].daily_envelopes[1].last_used_percent, 40.0);
}

#[test]
fn quota_cycle_contributions_emit_exact_boundary_slices() {
    let store = Store::in_memory().expect("store");
    // Use a mid-day reset so start and end fall on partial UTC days.
    let reset = DateTime::from_timestamp(1_787_670_000, 0).expect("reset");
    let inferred_start = reset - Duration::minutes(10_080);
    let observed_at = inferred_start + Duration::hours(2);
    let (source_id, account_id) = assigned_source(&store, inferred_start - Duration::days(1));
    store
        .upsert_quota_observations(&[sample_record(
            source_id.clone(),
            "weekly-boundary",
            "weekly-boundary",
            observed_at,
            reset.timestamp(),
            "secondary",
            10_080,
            33.0,
        )])
        .expect("observation");
    store
        .insert_event(&sample_usage_event(
            &source_id,
            &account_id,
            inferred_start + Duration::minutes(30),
            "start-boundary",
            100,
            20,
            10,
            5,
            1_500,
        ))
        .expect("start event");
    store
        .insert_event(&sample_usage_event(
            &source_id,
            &account_id,
            utc_day_start(reset) + Duration::hours(1),
            "end-boundary",
            40,
            0,
            8,
            2,
            700,
        ))
        .expect("end event");
    store
        .insert_event(&sample_usage_event(
            &source_id,
            &account_id,
            inferred_start + Duration::days(1) + Duration::hours(2),
            "interior-day",
            9_999,
            0,
            0,
            0,
            99_000,
        ))
        .expect("interior event");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert_eq!(contributions.len(), 1);
    assert_eq!(contributions[0].boundary_slices.len(), 2);
    assert_eq!(
        contributions[0].boundary_slices[0].period_start,
        inferred_start
    );
    assert_eq!(
        contributions[0].boundary_slices[0].period_end,
        next_utc_day_start(inferred_start)
    );
    assert_eq!(contributions[0].boundary_slices[0].input_tokens, 100);
    assert_eq!(contributions[0].boundary_slices[0].cache_read_tokens, 20);
    assert_eq!(contributions[0].boundary_slices[0].output_tokens, 10);
    assert_eq!(contributions[0].boundary_slices[0].reasoning_tokens, 5);
    assert_eq!(contributions[0].boundary_slices[0].total_tokens, 135);
    assert_eq!(
        contributions[0].boundary_slices[0].estimated_cost_micro_usd,
        1_500
    );
    assert_eq!(
        contributions[0].boundary_slices[1].period_start,
        utc_day_start(reset)
    );
    assert_eq!(contributions[0].boundary_slices[1].period_end, reset);
    assert_eq!(contributions[0].boundary_slices[1].input_tokens, 40);
    assert_eq!(
        contributions[0].boundary_slices[1].estimated_cost_micro_usd,
        700
    );
    assert!(
        contributions[0]
            .boundary_slices
            .iter()
            .all(|slice| slice.input_tokens != 9_999),
        "complete utc days stay out of boundary slices"
    );
}

#[test]
fn quota_cycle_contribution_ids_are_stable_for_the_same_device_anchor() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, observed_at - Duration::days(8));
    store
        .upsert_quota_observations(&[sample_record(
            source_id,
            "stable-id",
            "stable-id",
            observed_at,
            reset.timestamp(),
            "secondary",
            10_080,
            18.0,
        )])
        .expect("observation");

    let first = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("first");
    let second = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("second");
    let peer = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-b")
        .expect("peer");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].contribution_id, second[0].contribution_id);
    assert_ne!(first[0].contribution_id, peer[0].contribution_id);
    assert!(first[0].contribution_id.starts_with("quota_cycle_"));
    assert_eq!(first[0].contribution_id.len(), "quota_cycle_".len() + 32);
}
