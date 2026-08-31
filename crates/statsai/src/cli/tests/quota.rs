use super::support::*;
use super::*;

fn test_unattributed_quota_window(source_id: &str, window_id: &str) -> QuotaWindowV1 {
    let observed_at = Utc
        .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
        .single()
        .expect("observed at");
    let reset = observed_at + Duration::days(7);
    QuotaWindowV1 {
        schema_version: "quota_window.v1".to_string(),
        window_id: window_id.to_string(),
        provider: "codex".to_string(),
        provider_account_id: None,
        source_id: Some(SourceId(source_id.to_string())),
        limit_id: Some("subscription".to_string()),
        window_minutes: 10_080,
        inferred_start: reset - Duration::days(7),
        representative_reset: reset,
        representative_reset_epoch_seconds: reset.timestamp(),
        reset_min: reset,
        reset_min_epoch_seconds: reset.timestamp(),
        reset_max: reset,
        reset_max_epoch_seconds: reset.timestamp(),
        first_observed_at: observed_at,
        last_observed_at: observed_at,
        sample_count: 1,
        first_used_percent: 20.0,
        latest_used_percent: 20.0,
        minimum_used_percent: 20.0,
        maximum_used_percent: 20.0,
        transition: statsai_core::QuotaTransitionKind::Initial,
        has_schedule_overlap: false,
        change_points: Vec::new(),
        latest_status: statsai_core::QuotaStatusV1::default(),
        usage_totals: None,
    }
}

#[test]
fn current_quota_windows_keep_unattributed_source_scopes_separate() {
    let selected = select_current_quota_windows(
        vec![
            test_unattributed_quota_window("source-a", "window-a"),
            test_unattributed_quota_window("source-b", "window-b"),
        ],
        false,
        false,
    );

    assert_eq!(selected.len(), 2);
    assert_eq!(
        selected
            .iter()
            .filter_map(|window| window.source_id.as_ref())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
}

#[test]
fn raw_quota_history_isolated_to_unattributed_window_source() {
    let window = test_unattributed_quota_window("source-a", "window-a");
    let observations = raw_observations_for_window(
        vec![
            test_unattributed_quota_record("source-a"),
            test_unattributed_quota_record("source-b"),
        ],
        &window,
    );

    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].observation.source_id.0, "source-a");
}
