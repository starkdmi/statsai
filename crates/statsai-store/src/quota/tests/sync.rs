use super::support::*;
use super::*;

#[test]
fn two_device_projection_fixture_has_one_merge_scope_and_shared_point() {
    let fixture =
        include_str!("../../../../../docs/fixtures/quota_window_projection.v1.two_devices.json");
    let raw: serde_json::Value = serde_json::from_str(fixture).expect("fixture JSON");
    let projections: Vec<QuotaWindowSyncProjectionV1> =
        serde_json::from_str(fixture).expect("projection fixture");

    assert_eq!(projections.len(), 2);
    assert_ne!(projections[0].device_id, projections[1].device_id);
    assert_ne!(projections[0].projection_id, projections[1].projection_id);
    assert_eq!(projections[0].provider, projections[1].provider);
    assert_eq!(
        projections[0].provider_account_id,
        projections[1].provider_account_id
    );
    assert_eq!(projections[0].limit_id, projections[1].limit_id);
    assert_eq!(projections[0].window_minutes, projections[1].window_minutes);
    assert!(
        (projections[0].representative_reset_epoch_seconds
            - projections[1].representative_reset_epoch_seconds)
            .abs()
            <= RESET_CLUSTER_TOLERANCE_SECONDS
    );
    assert_eq!(
        projections[0].change_points[0].point_fingerprint,
        projections[1].change_points[0].point_fingerprint
    );

    for contribution in raw.as_array().expect("fixture array") {
        for forbidden in [
            "source_id",
            "usage_totals",
            "total_tokens",
            "estimated_cost_micro_usd",
            "raw_rate_limits",
        ] {
            assert!(contribution.get(forbidden).is_none(), "found {forbidden}");
        }
    }
}

#[test]
fn quota_sync_projection_skips_local_usage_enrichment() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("observed");
    let reset = DateTime::from_timestamp(1_787_500_000, 0).expect("reset");
    let (source_id, account_id) = assigned_source(&store, observed_at - Duration::days(8));
    store
        .upsert_quota_observations(&[sample_record(
            source_id.clone(),
            "projection-window",
            "projection-window",
            observed_at,
            reset.timestamp(),
            "primary",
            10_080,
            20.0,
        )])
        .expect("quota observation");
    store
        .conn
        .execute(
            r#"
                INSERT INTO usage_events (
                  event_id, provider, source_id, provider_account_id, started_at,
                  total_tokens, semantic_fingerprint, payload
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            params![
                "invalid-projection-event",
                "codex",
                &source_id.0,
                &account_id.0,
                (reset - Duration::days(1)).to_rfc3339(),
                0,
                "invalid-projection-event",
                "not-json"
            ],
        )
        .expect("invalid legacy event");

    let projections = store
        .quota_sync_projections(&QuotaQuery::default(), "device-a")
        .expect("projection without local usage enrichment");

    assert_eq!(projections.len(), 1);
}

#[test]
fn a_quota_only_change_still_reports_pending_upload_work() {
    // Nothing here writes a summary: the pending count has to see the
    // contribution itself or the menubar claims there is nothing to send.
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_011_200, 0).expect("observed");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, observed_at - Duration::days(8));
    store
        .upsert_quota_observations(&[sample_record(
            source_id,
            "weekly",
            "weekly",
            observed_at,
            reset.timestamp(),
            "secondary",
            10_080,
            20.0,
        )])
        .expect("observations");

    let target = "https://api.example.com/api/sync/batches";
    let counts = store
        .pending_http_sync_summary_counts(target, "device-a")
        .expect("counts");
    assert_eq!(counts.rollups, 0);
    assert_eq!(counts.passthrough_summaries, 0);
    assert_eq!(counts.quota_cycle_contributions, 1);
    assert!(
        counts.total > 0,
        "a quota-only change is pending upload work"
    );

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    store
        .record_quota_cycle_contributions_synced("http", target, &contributions)
        .expect("record synced");

    let settled = store
        .pending_http_sync_summary_counts(target, "device-a")
        .expect("settled counts");
    assert_eq!(settled.quota_cycle_contributions, 0);
}
