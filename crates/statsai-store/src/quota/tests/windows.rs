use super::support::*;
use super::*;

#[test]
fn quota_status_scopes_assignment_overlap_warnings_to_query() {
    let store = Store::in_memory().expect("store");
    let started_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    for (source, provider, account, prefix) in [
        ("source-a", "codex", "account-a", "a"),
        ("source-b", "codex", "account-b", "b"),
        ("source-c", "claude", "account-c", "c"),
    ] {
        for (suffix, offset) in [("one", 0), ("two", 10)] {
            store
                .upsert_source_account_assignment(&SourceAccountAssignment {
                    schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                    assignment_id: SourceAccountAssignmentId(format!(
                        "assignment-{prefix}-{suffix}"
                    )),
                    source_id: SourceId(source.to_string()),
                    provider: provider.to_string(),
                    provider_account_id: ProviderAccountId(account.to_string()),
                    started_at: started_at + Duration::seconds(offset),
                    ended_at: Some(started_at + Duration::seconds(offset + 20)),
                    record_source: IdentitySource::UserConfigured,
                    verified_at: Some(started_at),
                    created_at: started_at,
                    updated_at: started_at,
                })
                .expect("assignment");
        }
    }

    let account_status = store
        .quota_status(&QuotaQuery {
            provider_account_id: Some(ProviderAccountId("account-a".to_string())),
            ..QuotaQuery::default()
        })
        .expect("account status");
    assert_eq!(
        account_status.assignment_overlap_warnings,
        ["source source-a assignments assignment-a-one and assignment-a-two overlap"]
    );

    let provider_status = store
        .quota_status(&QuotaQuery {
            provider: Some("claude".to_string()),
            ..QuotaQuery::default()
        })
        .expect("provider status");
    assert_eq!(
        provider_status.assignment_overlap_warnings,
        ["source source-c assignments assignment-c-one and assignment-c-two overlap"]
    );

    let source_status = store
        .quota_status(&QuotaQuery {
            source_id: Some(SourceId("source-b".to_string())),
            ..QuotaQuery::default()
        })
        .expect("source status");
    assert_eq!(
        source_status.assignment_overlap_warnings,
        ["source source-b assignments assignment-b-one and assignment-b-two overlap"]
    );

    let later_status = store
        .quota_status(&QuotaQuery {
            from: Some(started_at + Duration::seconds(31)),
            ..QuotaQuery::default()
        })
        .expect("later status");
    assert!(later_status.assignment_overlap_warnings.is_empty());

    let earlier_status = store
        .quota_status(&QuotaQuery {
            to: Some(started_at - Duration::seconds(1)),
            ..QuotaQuery::default()
        })
        .expect("earlier status");
    assert!(earlier_status.assignment_overlap_warnings.is_empty());
}

#[test]
fn quota_windows_keep_identical_observations_for_distinct_accounts() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");

    for suffix in ["a", "b"] {
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            suffix,
            Path::new(&format!("/tmp/quota-source-{suffix}")),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: SourceAccountAssignmentId(format!("assignment-{suffix}")),
                source_id: source.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: ProviderAccountId(format!("account-{suffix}")),
                started_at: observed_at - Duration::seconds(1),
                ended_at: None,
                record_source: IdentitySource::UserConfigured,
                verified_at: Some(observed_at),
                created_at: observed_at,
                updated_at: observed_at,
            })
            .expect("assignment");
        let record = sample_record(
            source.source_id,
            &format!("observation-{suffix}"),
            "same-semantic-fingerprint",
            observed_at,
            1_787_500_000,
            "primary",
            10_080,
            20.0,
        );
        store
            .upsert_quota_observations(&[record])
            .expect("quota observation");
    }

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("quota windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(
        windows
            .iter()
            .filter_map(|window| window.provider_account_id.as_ref())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
}

#[test]
fn quota_windows_keep_identical_unattributed_observations_in_distinct_source_scopes() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");

    for suffix in ["a", "b"] {
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            suffix,
            Path::new(&format!("/tmp/quota-unattributed-source-{suffix}")),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        store
            .upsert_quota_observations(&[sample_record(
                source.source_id,
                &format!("unattributed-{suffix}"),
                "same-unattributed-semantic-fingerprint",
                observed_at,
                1_787_500_000,
                "primary",
                10_080,
                20.0,
            )])
            .expect("quota observation");
    }

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("quota windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(
        windows
            .iter()
            .map(|window| window.window_id.as_str())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(
        windows
            .iter()
            .filter_map(|window| window.source_id.as_ref())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert!(windows.iter().all(|window| window.sample_count == 1));
}

#[test]
fn time_filtered_windows_do_not_query_usage_for_filtered_out_clusters() {
    let store = Store::in_memory().expect("store");
    let old_observed_at = DateTime::from_timestamp(1_735_689_600, 0).expect("old observed");
    let old_reset = DateTime::from_timestamp(1_736_294_400, 0).expect("old reset");
    let recent_observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("recent observed");
    let recent_reset = DateTime::from_timestamp(1_787_500_000, 0).expect("recent reset");
    let (source_id, account_id) = assigned_source(&store, old_observed_at - Duration::seconds(1));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "old-window",
                "old-window",
                old_observed_at,
                old_reset.timestamp(),
                "primary",
                10_080,
                10.0,
            ),
            sample_record(
                source_id.clone(),
                "recent-window",
                "recent-window",
                recent_observed_at,
                recent_reset.timestamp(),
                "primary",
                10_080,
                20.0,
            ),
        ])
        .expect("quota observations");
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
                "invalid-old-event",
                "codex",
                &source_id.0,
                &account_id.0,
                (old_reset - Duration::days(1)).to_rfc3339(),
                0,
                "invalid-old-event",
                "not-json"
            ],
        )
        .expect("invalid legacy event");

    let windows = store
        .quota_windows(&QuotaQuery {
            from: Some(recent_observed_at),
            ..QuotaQuery::default()
        })
        .expect("recent windows");

    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].first_observed_at, recent_observed_at);

    let mut limited = store
        .quota_windows_without_usage_totals(&QuotaQuery::default())
        .expect("unenriched windows");
    assert_eq!(limited.len(), 2);
    limited.truncate(1);
    store
        .enrich_quota_window_usage_totals(&mut limited)
        .expect("enrich only the caller-selected recent window");
    assert_eq!(limited[0].first_observed_at, recent_observed_at);
}

#[test]
fn quota_window_identity_and_evidence_are_stable_across_time_filters() {
    let store = Store::in_memory().expect("store");
    let first_observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let second_observed_at = first_observed_at + Duration::minutes(5);
    let (source_id, _) = assigned_source(&store, first_observed_at - Duration::seconds(1));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "stable-first",
                "stable-first",
                first_observed_at,
                1_787_500_000,
                "primary",
                10_080,
                10.0,
            ),
            sample_record(
                source_id,
                "stable-second",
                "stable-second",
                second_observed_at,
                1_787_500_000,
                "secondary",
                10_080,
                20.0,
            ),
        ])
        .expect("observations");

    let full = store
        .quota_windows(&QuotaQuery::default())
        .expect("full windows");
    let filtered = store
        .quota_windows(&QuotaQuery {
            from: Some(second_observed_at),
            ..QuotaQuery::default()
        })
        .expect("filtered windows");

    assert_eq!(full.len(), 1);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].window_id, full[0].window_id);
    assert_eq!(filtered[0].sample_count, 2);
    assert_eq!(filtered[0].first_used_percent, 10.0);
    assert_eq!(filtered[0].change_points, full[0].change_points);
}

#[test]
fn stale_concurrent_readings_never_walk_the_daily_closing_backwards() {
    let store = Store::in_memory().expect("store");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let day_one = reset - Duration::days(3);
    let day_two = day_one + Duration::days(1);
    let (source_id, _) = assigned_source(&store, reset - Duration::days(9));
    // Two sessions poll together; the slower one still reports the older
    // figure, and on the second day it lands last.
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "d1-low",
                "d1-low",
                day_one,
                reset.timestamp(),
                "secondary",
                10_080,
                27.0,
            ),
            sample_record(
                source_id.clone(),
                "d1-high",
                "d1-high",
                day_one + Duration::hours(2),
                reset.timestamp(),
                "secondary",
                10_080,
                59.0,
            ),
            sample_record(
                source_id,
                "d2-stale",
                "d2-stale",
                day_two,
                reset.timestamp(),
                "secondary",
                10_080,
                56.0,
            ),
        ])
        .expect("observations");

    let contributions = store
        .quota_cycle_contributions(&QuotaQuery::default(), "device-a")
        .expect("contributions");
    assert_eq!(contributions.len(), 1);
    let envelopes = &contributions[0].daily_envelopes;
    assert_eq!(envelopes.len(), 2);
    // The stale 56% must not read as a drop from the 59% already spent.
    assert_eq!(envelopes[0].last_used_percent, 59.0);
    assert_eq!(envelopes[1].last_used_percent, 59.0);
    assert_eq!(envelopes[1].first_used_percent, 59.0);
    // The raw extremes still record that a 56% reading arrived.
    assert_eq!(envelopes[1].minimum_used_percent, 56.0);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].last_used_percent >= pair[0].last_used_percent),
        "daily closings never decrease inside a cycle"
    );
}

#[test]
fn replayed_observations_do_not_extend_a_window_that_already_reset() {
    let store = Store::in_memory().expect("store");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let live = reset - Duration::days(2);
    // A re-import three weeks later re-reads the same historical window and
    // stamps it with the import time.
    let replay = reset + Duration::days(21);
    let (source_id, _) = assigned_source(&store, live - Duration::days(8));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "live",
                "live",
                live,
                reset.timestamp(),
                "secondary",
                10_080,
                26.0,
            ),
            sample_record(
                source_id,
                "replayed",
                "replayed",
                replay,
                reset.timestamp(),
                "secondary",
                10_080,
                58.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 1);
    let days = windows[0]
        .change_points
        .iter()
        .map(|point| point.observed_at.date_naive().to_string())
        .collect::<HashSet<_>>();
    assert_eq!(
        days,
        HashSet::from([live.date_naive().to_string()]),
        "the closed window keeps only evidence recorded while it was open"
    );
    assert_eq!(windows[0].maximum_used_percent, 26.0);
}

#[test]
fn an_observation_within_recomputation_lag_still_belongs_to_its_window() {
    let store = Store::in_memory().expect("store");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, reset - Duration::days(8));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "before",
                "before",
                reset - Duration::hours(1),
                reset.timestamp(),
                "secondary",
                10_080,
                90.0,
            ),
            // The provider has not recomputed the window yet, so it keeps
            // reporting the reset that elapsed 45 minutes ago.
            sample_record(
                source_id,
                "lagging",
                "lagging",
                reset + Duration::minutes(45),
                reset.timestamp(),
                "secondary",
                10_080,
                100.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].maximum_used_percent, 100.0);
    assert_eq!(windows[0].sample_count, 2);
}

#[test]
fn an_observation_beyond_recomputation_lag_is_treated_as_a_replay() {
    let store = Store::in_memory().expect("store");
    let reset = DateTime::from_timestamp(1_787_616_000, 0).expect("reset");
    let (source_id, _) = assigned_source(&store, reset - Duration::days(8));
    store
        .upsert_quota_observations(&[
            sample_record(
                source_id.clone(),
                "live",
                "live",
                reset - Duration::hours(2),
                reset.timestamp(),
                "secondary",
                10_080,
                90.0,
            ),
            sample_record(
                source_id,
                "past-lag",
                "past-lag",
                reset + Duration::minutes(61),
                reset.timestamp(),
                "secondary",
                10_080,
                100.0,
            ),
        ])
        .expect("observations");

    let windows = store
        .quota_windows(&QuotaQuery::default())
        .expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].sample_count, 1);
    assert_eq!(windows[0].maximum_used_percent, 90.0);
}
