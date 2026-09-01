use super::support::*;
use super::*;

#[test]
fn reconstruction_clusters_reset_drift_ignores_slot_and_projects_weekly_windows() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let (source_id, account_id) = assigned_source(&store, observed_at - Duration::hours(1));
    let records = vec![
        sample_record(
            source_id.clone(),
            "one",
            "one",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            10.0,
        ),
        sample_record(
            source_id,
            "two",
            "two",
            observed_at + Duration::minutes(1),
            1_787_500_240,
            "secondary",
            10_080,
            12.0,
        ),
    ];
    store.upsert_quota_observations(&records).expect("upsert");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].sample_count, 2);
    assert_eq!(windows[0].reset_min_epoch_seconds, 1_787_500_000);
    assert_eq!(windows[0].reset_max_epoch_seconds, 1_787_500_240);
    let projections = store
        .quota_sync_projections(&QuotaQuery::default(), "device-a")
        .expect("projections");
    let peer_projections = store
        .quota_sync_projections(&QuotaQuery::default(), "device-b")
        .expect("peer projections");
    assert_eq!(projections.len(), 1);
    assert_eq!(peer_projections.len(), 1);
    assert_eq!(projections[0].provider_account_id, account_id);
    assert_ne!(
        projections[0].projection_id,
        peer_projections[0].projection_id
    );
    assert_eq!(
        projections[0].change_points[0].point_fingerprint,
        peer_projections[0].change_points[0].point_fingerprint
    );
    let projection_json = serde_json::to_value(&projections[0]).expect("json");
    assert!(projection_json.get("source_id").is_none());
    assert!(projection_json.get("total_tokens").is_none());
    assert!(projection_json.get("estimated_cost").is_none());
}

#[test]
fn a_window_that_never_left_zero_is_not_a_cycle() {
    let store = Store::in_memory().expect("store");
    let real = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    // The provider briefly reported a window resetting 40 minutes earlier,
    // at zero, before settling on the one that went on to be used.
    let stub_reset = real - Duration::minutes(40);
    let (source_id, _) = assigned_source(&store, real - Duration::days(9));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "stub",
                "stub",
                stub_reset - Duration::days(7) + Duration::minutes(5),
                stub_reset.timestamp(),
                "secondary",
                10_080,
                0.0,
            ),
            sample_record(
                source_id,
                "real",
                "real",
                real - Duration::days(6),
                real.timestamp(),
                "secondary",
                10_080,
                64.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(
        windows.len(),
        1,
        "the zero-only window is not its own cycle"
    );
    assert_eq!(windows[0].representative_reset, real);
}

#[test]
fn status_reports_what_reconstruction_threw_away() {
    let store = Store::in_memory().expect("store");
    let live_reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let start = live_reset - Duration::days(7);
    let (source_id, _) = assigned_source(&store, start - Duration::days(2));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "live-1",
                "live-1",
                start + Duration::hours(1),
                live_reset.timestamp(),
                "secondary",
                10_080,
                21.0,
            ),
            // Bracketed by the live schedule, so not a cycle of its own.
            sample_record(
                source_id.clone(),
                "phantom",
                "phantom",
                start + Duration::hours(2),
                (live_reset + Duration::days(1)).timestamp(),
                "secondary",
                10_080,
                1.0,
            ),
            sample_record(
                source_id.clone(),
                "live-2",
                "live-2",
                start + Duration::hours(3),
                live_reset.timestamp(),
                "secondary",
                10_080,
                44.0,
            ),
            // Recorded a day after the window it describes had reset.
            sample_record(
                source_id,
                "replayed",
                "replayed",
                live_reset + Duration::days(1),
                live_reset.timestamp(),
                "secondary",
                10_080,
                44.0,
            ),
        ])
        .expect("observations");

    let status = store.quota_status(&QuotaQuery::default()).expect("status");
    assert_eq!(status.discarded.replayed_observations, 1);
    assert_eq!(status.discarded.bracketed_schedules, 1);
}

#[test]
fn an_interleaved_schedule_the_provider_never_switched_to_is_not_a_cycle() {
    let store = Store::in_memory().expect("store");
    let live_reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let start = live_reset - Duration::days(7);
    // Codex answered two turns mid-cycle with a blank snapshot: a token
    // percentage against a reset a day later than the one it kept
    // reporting either side.
    let phantom_reset = live_reset + Duration::days(1);
    let (source_id, _) = assigned_source(&store, start - Duration::days(2));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "live-1",
                "live-1",
                start + Duration::hours(1),
                live_reset.timestamp(),
                "secondary",
                10_080,
                21.0,
            ),
            sample_record(
                source_id.clone(),
                "phantom-1",
                "phantom-1",
                start + Duration::hours(2),
                phantom_reset.timestamp(),
                "secondary",
                10_080,
                0.0,
            ),
            sample_record(
                source_id.clone(),
                "phantom-2",
                "phantom-2",
                start + Duration::hours(3),
                phantom_reset.timestamp(),
                "secondary",
                10_080,
                1.0,
            ),
            sample_record(
                source_id,
                "live-2",
                "live-2",
                start + Duration::hours(4),
                live_reset.timestamp(),
                "secondary",
                10_080,
                44.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(
        windows.len(),
        1,
        "a schedule reported between two readings of the one that outran it did not reset"
    );
    assert_eq!(windows[0].representative_reset, live_reset);
    assert_eq!(windows[0].maximum_used_percent, 44.0);
}

#[test]
fn an_early_reset_that_took_over_stays_a_cycle() {
    let store = Store::in_memory().expect("store");
    let first_reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let start = first_reset - Duration::days(7);
    // A redeemed reset zeroes the window and issues a fresh schedule. The
    // schedule it replaced is never reported again, so nothing brackets it.
    let redeemed_reset = first_reset + Duration::days(4);
    let (source_id, _) = assigned_source(&store, start - Duration::days(2));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "before-1",
                "before-1",
                start + Duration::hours(1),
                first_reset.timestamp(),
                "secondary",
                10_080,
                21.0,
            ),
            sample_record(
                source_id.clone(),
                "before-2",
                "before-2",
                start + Duration::hours(2),
                first_reset.timestamp(),
                "secondary",
                10_080,
                64.0,
            ),
            sample_record(
                source_id.clone(),
                "after-1",
                "after-1",
                start + Duration::hours(3),
                redeemed_reset.timestamp(),
                "secondary",
                10_080,
                0.0,
            ),
            sample_record(
                source_id,
                "after-2",
                "after-2",
                start + Duration::hours(4),
                redeemed_reset.timestamp(),
                "secondary",
                10_080,
                9.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(
        windows.len(),
        2,
        "an early reset that the provider switched to is its own cycle"
    );
    assert!(windows
        .iter()
        .any(|window| window.representative_reset == redeemed_reset));
}

#[test]
fn a_superseded_window_that_spent_anything_stays_a_cycle() {
    let store = Store::in_memory().expect("store");
    let real = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let earlier_reset = real - Duration::minutes(40);
    let (source_id, _) = assigned_source(&store, real - Duration::days(9));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "spent-a-little",
                "spent-a-little",
                earlier_reset - Duration::days(7) + Duration::minutes(5),
                earlier_reset.timestamp(),
                "secondary",
                10_080,
                3.0,
            ),
            sample_record(
                source_id,
                "real",
                "real",
                real - Duration::days(6),
                real.timestamp(),
                "secondary",
                10_080,
                64.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 2, "3% spent is still a real cycle");
}
