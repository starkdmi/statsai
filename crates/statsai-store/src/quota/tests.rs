use super::*;
use serde_json::json;
use statsai_core::{
    event_id, Confidence, CostInfo, EventSource, IdentitySource, LocationOrigin, PrivacyInfo,
    PrivacyMode, QuotaCreditsV1, QuotaStatusV1, SessionInfo, SourceAccountAssignment,
    SourceAccountAssignmentId, SourceKind, SourceLocation, UsageCounts, UsageEvent,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, USAGE_EVENT_SCHEMA_VERSION,
};
use std::collections::HashSet;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
fn sample_record(
    source_id: SourceId,
    observation_id: &str,
    semantic_fingerprint: &str,
    observed_at: DateTime<Utc>,
    reset_epoch: i64,
    slot: &str,
    window_minutes: u64,
    used_percent: f64,
) -> QuotaObservationRecordV1 {
    let raw_rate_limits = serde_json::json!({
        slot: {
            "window_minutes": window_minutes,
            "used_percent": used_percent,
            "resets_at": reset_epoch
        },
        "credits": {"balance": "5.00"}
    });
    let payload_hash = hash_text(&serde_json::to_string(&raw_rate_limits).expect("payload"));
    QuotaObservationRecordV1 {
        observation: QuotaObservationV1 {
            schema_version: "quota_observation.v1".to_string(),
            observation_id: observation_id.to_string(),
            semantic_fingerprint: semantic_fingerprint.to_string(),
            provider: "codex".to_string(),
            source_id,
            provider_account_id: None,
            observed_at,
            source_file_path_hash: format!("file-{observation_id}"),
            source_record_id: format!("record-{observation_id}"),
            source_line_number: 1,
            payload_hash,
            usage_sample: None,
            usage_event_id: None,
            usage_link_kind: QuotaUsageLinkKind::None,
            status: QuotaStatusV1 {
                plan_type: Some("pro".to_string()),
                credits: QuotaCreditsV1 {
                    balance: Some("5".to_string()),
                    balance_raw: Some(serde_json::json!("5.00")),
                    ..QuotaCreditsV1::default()
                },
                ..QuotaStatusV1::default()
            },
        },
        windows: vec![QuotaWindowObservationV1 {
            schema_version: "quota_window_observation.v1".to_string(),
            window_observation_id: format!("window-{observation_id}"),
            observation_id: observation_id.to_string(),
            provider_slot: slot.to_string(),
            limit_id: Some("subscription".to_string()),
            window_minutes,
            used_percent,
            resets_at: DateTime::from_timestamp(reset_epoch, 0).expect("reset"),
            resets_at_epoch_seconds: reset_epoch,
        }],
        raw_rate_limits,
    }
}

fn assigned_source(store: &Store, started_at: DateTime<Utc>) -> (SourceId, ProviderAccountId) {
    let source = SourceLocation::local_adapter(
        "codex",
        "codex-local-jsonl",
        "test",
        Path::new("/tmp/quota-source"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let account_id = ProviderAccountId("account-codex".to_string());
    let now = Utc::now();
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: SourceAccountAssignmentId("assignment-quota".to_string()),
            source_id: source.source_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::UserConfigured,
            verified_at: Some(now),
            created_at: now,
            updated_at: now,
        })
        .expect("assignment");
    (source.source_id, account_id)
}

#[test]
fn quota_plan_evidence_speaks_for_the_moment_it_was_read() {
    // The provider names the plan while serving a request, so the reading
    // is evidence of the plan *now* even though it declares no billing
    // window. Without that, an account whose logs say "plus" every day
    // still read as `last_detected` once its last declared provider period
    // ran out, and it dropped off every current-plan surface.
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed at");
    let reset_at = observed_at + Duration::days(7);
    let (source_id, _) = assigned_source(&store, observed_at - Duration::days(1));
    let record = sample_record(
        source_id.clone(),
        "quota-current",
        "quota-current-v1",
        observed_at,
        reset_at.timestamp(),
        "primary",
        10_080,
        25.0,
    );
    store
        .upsert_quota_observations(std::slice::from_ref(&record))
        .expect("quota row");
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("plan rebuild");

    let plans = store.account_plan_observations().expect("plan evidence");
    assert_eq!(plans.len(), 1);
    assert!(plans[0].is_current_snapshot);
    // Still no invented billing period.
    assert_eq!(plans[0].active_from, None);
    assert_eq!(plans[0].active_until, None);
}

#[test]
fn source_quota_reads_do_not_deserialize_unrelated_rows() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed at");
    let target = SourceId("quota-target".to_string());
    let unrelated = SourceId("quota-unrelated".to_string());
    store
        .upsert_quota_observations(&[
            sample_record(
                target.clone(),
                "quota-target",
                "quota-target-v1",
                observed_at,
                (observed_at + Duration::days(7)).timestamp(),
                "primary",
                10_080,
                25.0,
            ),
            sample_record(
                unrelated,
                "quota-unrelated",
                "quota-unrelated-v1",
                observed_at,
                (observed_at + Duration::days(7)).timestamp(),
                "primary",
                10_080,
                50.0,
            ),
        ])
        .expect("quota rows");
    store
            .conn
            .execute(
                "UPDATE quota_observations SET payload = 'not-json' WHERE observation_id = 'quota-unrelated'",
                [],
            )
            .expect("corrupt unrelated row");

    let records = store
        .quota_observations_for_source(&target)
        .expect("read target source only");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].observation.observation_id, "quota-target");
}

#[test]
fn orphan_purge_retires_only_rows_no_rescanned_file_explains() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed at");
    let source_id = SourceId("quota-orphan-source".to_string());
    let other_source_id = SourceId("quota-other-source".to_string());
    let record = |source: SourceId, id: &str| {
        sample_record(
            source,
            id,
            &format!("{id}-v1"),
            observed_at,
            (observed_at + Duration::days(7)).timestamp(),
            "primary",
            10_080,
            25.0,
        )
    };
    let kept = record(source_id.clone(), "kept");
    let orphaned = record(source_id.clone(), "orphaned");
    let untouched = record(other_source_id.clone(), "other");
    store
        .upsert_quota_observations(&[kept, orphaned, untouched])
        .expect("seed quota rows");

    // What a full rescan sees: only `kept`'s file still exists.
    let deleted = store
        .delete_quota_observations_for_source_outside_file_hashes(
            &source_id,
            &["file-kept".to_string()],
        )
        .expect("purge orphaned quota rows");

    assert_eq!(deleted, 1, "only the row without a current file is retired");
    let remaining = store
        .quota_observations_for_source(&source_id)
        .expect("read source quota rows")
        .into_iter()
        .map(|record| record.observation.observation_id)
        .collect::<Vec<_>>();
    assert_eq!(remaining, vec!["kept".to_string()]);
    let orphaned_windows: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM quota_window_observations WHERE observation_id = 'orphaned'",
            [],
            |row| row.get(0),
        )
        .expect("count orphaned windows");
    assert_eq!(orphaned_windows, 0, "windows follow their observation");
    // A hash absent from one source's rescan says nothing about another source.
    assert_eq!(
        store
            .quota_observations_for_source(&other_source_id)
            .expect("read other source")
            .len(),
        1,
        "the purge is scoped to the rescanned source"
    );
}

#[test]
fn file_reconciliation_does_not_rewrite_retained_quota_rows() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed at");
    let source_id = SourceId("quota-retained".to_string());
    let mut record = sample_record(
        source_id.clone(),
        "quota-retained",
        "quota-retained-v1",
        observed_at,
        (observed_at + Duration::days(7)).timestamp(),
        "primary",
        10_080,
        25.0,
    );
    record.windows[0].window_observation_id = "z-window".to_string();
    record.windows[0].provider_slot = "secondary".to_string();
    let mut earlier_window = record.windows[0].clone();
    earlier_window.window_observation_id = "a-window".to_string();
    earlier_window.provider_slot = "primary".to_string();
    record.windows.push(earlier_window);
    store
        .upsert_quota_observations(std::slice::from_ref(&record))
        .expect("initial quota row");
    store
        .conn
        .execute_batch(
            r#"
                CREATE TABLE quota_write_audit (
                  observation_updates INTEGER NOT NULL,
                  window_deletes INTEGER NOT NULL,
                  window_inserts INTEGER NOT NULL
                );
                INSERT INTO quota_write_audit VALUES (0, 0, 0);
                CREATE TRIGGER count_quota_observation_updates
                AFTER UPDATE ON quota_observations
                BEGIN
                  UPDATE quota_write_audit
                  SET observation_updates = observation_updates + 1;
                END;
                CREATE TRIGGER count_quota_window_deletes
                AFTER DELETE ON quota_window_observations
                BEGIN
                  UPDATE quota_write_audit SET window_deletes = window_deletes + 1;
                END;
                CREATE TRIGGER count_quota_window_inserts
                AFTER INSERT ON quota_window_observations
                BEGIN
                  UPDATE quota_write_audit SET window_inserts = window_inserts + 1;
                END;
                "#,
        )
        .expect("audit triggers");

    store
        .replace_quota_observations_for_source_files(
            &source_id,
            std::slice::from_ref(&record.observation.source_file_path_hash),
            std::slice::from_ref(&record),
        )
        .expect("reconcile retained quota row");

    let writes = store
        .conn
        .query_row(
            "SELECT observation_updates, window_deletes, window_inserts FROM quota_write_audit",
            [],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )
        .expect("audit counts");
    assert_eq!(writes, (0, 0, 0));
}

#[test]
fn quota_plan_evidence_rebuild_replaces_corrected_plan_rows() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed at");
    let reset_at = observed_at + Duration::days(7);
    let (source_id, account_id) = assigned_source(&store, observed_at - Duration::days(1));
    let mut record = sample_record(
        source_id.clone(),
        "quota-plan",
        "quota-plan-v1",
        observed_at,
        reset_at.timestamp(),
        "primary",
        10_080,
        25.0,
    );
    store
        .upsert_quota_observations(std::slice::from_ref(&record))
        .expect("initial quota row");
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("initial plan rebuild");
    let initial = store.account_plan_observations().expect("initial plan");
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].raw_plan_name, "pro");
    assert_eq!(initial[0].provider_account_id, Some(account_id.clone()));

    record.observation.status.plan_type = Some("future_ultra".to_string());
    record.observation.semantic_fingerprint = "quota-plan-v2".to_string();
    store
        .upsert_quota_observations(&[record])
        .expect("corrected quota row");
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("corrected plan rebuild");

    let corrected = store.account_plan_observations().expect("corrected plan");
    assert_eq!(corrected.len(), 1);
    assert_eq!(corrected[0].raw_plan_name, "future_ultra");
    assert_eq!(corrected[0].plan_name, "Future Ultra");
    assert_eq!(corrected[0].provider_account_id, Some(account_id));
    assert_ne!(corrected[0].observation_id, initial[0].observation_id);
}

#[test]
fn many_quota_rows_naming_one_plan_collapse_into_one_observation() {
    let store = Store::in_memory().expect("store");
    let start = DateTime::from_timestamp(1_787_227_200, 0).expect("start");
    let (source_id, account_id) = assigned_source(&store, start - Duration::days(1));

    // Two hundred provider responses across two plans: "pro", then "team",
    // then "pro" again after a downgrade.
    let mut records = Vec::new();
    for index in 0..200i64 {
        let observed_at = start + Duration::minutes(index);
        let mut record = sample_record(
            source_id.clone(),
            &format!("quota-run-{index}"),
            &format!("quota-run-fingerprint-{index}"),
            observed_at,
            (observed_at + Duration::days(7)).timestamp(),
            "primary",
            10_080,
            25.0,
        );
        record.observation.status.plan_type = Some(
            match index {
                ..=99 => "pro",
                100..=149 => "team",
                _ => "pro",
            }
            .to_string(),
        );
        records.push(record);
    }
    store
        .upsert_quota_observations(&records)
        .expect("quota observations");
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("plan rebuild");

    let observations = store.account_plan_observations().expect("plan evidence");
    assert_eq!(
        observations.len(),
        3,
        "one observation per run of an unchanged plan, not one per quota row"
    );
    let mut runs = observations
        .iter()
        .map(|observation| {
            (
                observation.raw_plan_name.as_str(),
                observation.observed_at,
                observation.provider_account_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| run.1);
    assert_eq!(runs[0].0, "pro");
    assert_eq!(runs[0].1, start + Duration::minutes(99));
    assert_eq!(runs[1].0, "team");
    assert_eq!(runs[1].1, start + Duration::minutes(149));
    assert_eq!(runs[2].0, "pro");
    assert_eq!(runs[2].1, start + Duration::minutes(199));
    assert!(runs.iter().all(|run| run.2.as_ref() == Some(&account_id)));

    // Rebuilding without new data must not churn the ledger.
    let ids_before = observations
        .iter()
        .map(|observation| observation.observation_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("repeat plan rebuild");
    let ids_after = store
        .account_plan_observations()
        .expect("plan evidence")
        .iter()
        .map(|observation| observation.observation_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids_after, ids_before);

    // Extending the open run keeps its identity, so history stays put and
    // only the newest observation re-syncs.
    let extension = {
        let observed_at = start + Duration::minutes(200);
        let mut record = sample_record(
            source_id.clone(),
            "quota-run-200",
            "quota-run-fingerprint-200",
            observed_at,
            (observed_at + Duration::days(7)).timestamp(),
            "primary",
            10_080,
            26.0,
        );
        record.observation.status.plan_type = Some("pro".to_string());
        record
    };
    store
        .upsert_quota_observations(&[extension])
        .expect("extend run");
    store
        .rebuild_quota_plan_observations_for_source(&source_id)
        .expect("plan rebuild after extension");
    let extended = store.account_plan_observations().expect("plan evidence");
    assert_eq!(extended.len(), 3);
    assert_eq!(
        extended
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        ids_before,
        "extending a run must not mint a new observation and retire the old one"
    );
}

#[test]
fn quota_plan_run_collapse_preserves_account_switches() {
    let store = Store::in_memory().expect("store");
    let start = DateTime::from_timestamp(1_787_227_200, 0).expect("start");
    let source_id = SourceId("quota-account-switches".to_string());
    let mut records = [
        ("account-a-first", "account-a", 0),
        ("account-b", "account-b", 1),
        ("account-a-last", "account-a", 2),
    ]
    .into_iter()
    .map(|(observation_id, account_id, minute)| {
        let observed_at = start + Duration::minutes(minute);
        let mut record = sample_record(
            source_id.clone(),
            observation_id,
            &format!("fingerprint-{observation_id}"),
            observed_at,
            (observed_at + Duration::days(7)).timestamp(),
            "primary",
            10_080,
            25.0,
        );
        record.observation.provider_account_id = Some(ProviderAccountId(account_id.to_string()));
        record
    })
    .collect::<Vec<_>>();
    records.reverse();

    store
        .upsert_quota_plan_observations(&records)
        .expect("plan observations");

    let mut observations = store.account_plan_observations().expect("plan evidence");
    observations.sort_by_key(|observation| observation.observed_at);
    assert_eq!(observations.len(), 3);
    assert_eq!(
        observations
            .iter()
            .map(|observation| observation.provider_account_id.as_ref())
            .collect::<Vec<_>>(),
        vec![
            Some(&ProviderAccountId("account-a".to_string())),
            Some(&ProviderAccountId("account-b".to_string())),
            Some(&ProviderAccountId("account-a".to_string())),
        ]
    );
}

#[test]
fn quota_plan_label_never_identifies_an_unassigned_account() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_227_200, 0).expect("observed_at");
    let reset_at = observed_at.timestamp() + 3_600;
    let unassigned = sample_record(
        SourceId("unassigned-source".to_string()),
        "quota-plan-unassigned",
        "quota-plan-unassigned-fingerprint",
        observed_at,
        reset_at,
        "primary",
        300,
        25.0,
    );

    assert_eq!(
        store
            .upsert_quota_plan_observations(&[unassigned])
            .expect("unassigned plan label"),
        0
    );
    assert!(store
        .account_plan_observations()
        .expect("no plan observations")
        .is_empty());

    let (source_id, account_id) = assigned_source(&store, observed_at);
    let assigned = sample_record(
        source_id,
        "quota-plan-assigned",
        "quota-plan-assigned-fingerprint",
        observed_at,
        reset_at,
        "primary",
        300,
        25.0,
    );
    assert_eq!(
        store
            .upsert_quota_plan_observations(&[assigned])
            .expect("assigned plan label"),
        1
    );
    let observations = store.account_plan_observations().expect("plan observation");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].provider_account_id, Some(account_id));
    assert_eq!(observations[0].raw_plan_name, "pro");
}

#[test]
fn semantic_collapse_prefers_attributed_linked_evidence() {
    let now = Utc::now();
    let base = QuotaObservationV1 {
        schema_version: "quota_observation.v1".to_string(),
        observation_id: "one".to_string(),
        semantic_fingerprint: "same".to_string(),
        provider: "codex".to_string(),
        source_id: SourceId("source-a".to_string()),
        provider_account_id: None,
        observed_at: now,
        source_file_path_hash: "file".to_string(),
        source_record_id: "record".to_string(),
        source_line_number: 1,
        payload_hash: "payload".to_string(),
        usage_sample: None,
        usage_event_id: None,
        usage_link_kind: QuotaUsageLinkKind::None,
        status: statsai_core::QuotaStatusV1::default(),
    };
    let mut better = base.clone();
    better.observation_id = "two".to_string();
    better.provider_account_id = Some(ProviderAccountId("account".to_string()));
    better.usage_event_id = Some(EventId("event".to_string()));
    let records = collapse_semantic_duplicates(vec![
        QuotaObservationRecordV1 {
            observation: base,
            windows: Vec::new(),
            raw_rate_limits: serde_json::json!({}),
        },
        QuotaObservationRecordV1 {
            observation: better,
            windows: Vec::new(),
            raw_rate_limits: serde_json::json!({}),
        },
    ]);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].observation.observation_id, "two");
}

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
fn semantic_collapse_drops_ambiguous_unattributed_copies() {
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let mut unassigned = sample_record(
        SourceId("source-unassigned".to_string()),
        "ambiguous-unassigned",
        "ambiguous-semantic",
        observed_at,
        1_787_500_000,
        "primary",
        10_080,
        20.0,
    );
    let mut account_a = unassigned.clone();
    account_a.observation.observation_id = "ambiguous-a".to_string();
    account_a.observation.provider_account_id = Some(ProviderAccountId("account-a".to_string()));
    let mut account_b = unassigned.clone();
    account_b.observation.observation_id = "ambiguous-b".to_string();
    account_b.observation.provider_account_id = Some(ProviderAccountId("account-b".to_string()));
    unassigned.observation.provider_account_id = None;

    let collapsed = collapse_semantic_duplicates(vec![unassigned, account_a, account_b]);

    assert_eq!(collapsed.len(), 2);
    assert!(collapsed
        .iter()
        .all(|record| record.observation.provider_account_id.is_some()));
}

#[test]
fn two_device_projection_fixture_has_one_merge_scope_and_shared_point() {
    let fixture =
        include_str!("../../../../docs/fixtures/quota_window_projection.v1.two_devices.json");
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
fn quota_store_reuses_payloads_and_rescans_idempotently() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let source_id = SourceId("source-a".to_string());
    let record = sample_record(
        source_id,
        "observation-a",
        "semantic-a",
        observed_at,
        1_787_500_000,
        "primary",
        10_080,
        20.0,
    );
    store
        .upsert_quota_observations(std::slice::from_ref(&record))
        .expect("first upsert");
    store
        .upsert_quota_observations(std::slice::from_ref(&record))
        .expect("rescan");

    assert_eq!(
        store
            .quota_observations(&QuotaQuery::default(), false)
            .expect("observations")
            .len(),
        1
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM quota_payloads", [], |row| {
                row.get::<_, u64>(0)
            })
            .expect("payload count"),
        1
    );
}

#[test]
fn quota_upsert_preserves_usage_link_only_for_matching_positive_sample() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let mut linked = sample_record(
        SourceId("source-link".to_string()),
        "stable-line",
        "semantic-original",
        observed_at,
        1_787_500_000,
        "primary",
        10_080,
        20.0,
    );
    linked.observation.usage_sample = Some(statsai_core::UsageCounts {
        input_tokens: Some(10),
        total_tokens: Some(10),
        ..statsai_core::UsageCounts::default()
    });
    linked.observation.usage_event_id = Some(EventId("event-original".to_string()));
    linked.observation.usage_link_kind = QuotaUsageLinkKind::RecordEvent;
    store
        .upsert_quota_observations(std::slice::from_ref(&linked))
        .expect("linked observation");

    let mut unchanged = linked.clone();
    unchanged.observation.usage_event_id = None;
    unchanged.observation.usage_link_kind = QuotaUsageLinkKind::None;
    store
        .upsert_quota_observations(std::slice::from_ref(&unchanged))
        .expect("unchanged archive observation");
    let stored = store
        .quota_observations(&QuotaQuery::default(), false)
        .expect("stored observation");
    assert_eq!(
        stored[0].observation.usage_event_id,
        Some(EventId("event-original".to_string()))
    );

    let mut changed = unchanged.clone();
    changed.observation.semantic_fingerprint = "semantic-changed".to_string();
    changed.observation.usage_sample = Some(statsai_core::UsageCounts {
        input_tokens: Some(11),
        total_tokens: Some(11),
        ..statsai_core::UsageCounts::default()
    });
    store
        .upsert_quota_observations(std::slice::from_ref(&changed))
        .expect("changed archive observation");
    let stored = store
        .quota_observations(&QuotaQuery::default(), false)
        .expect("changed observation");
    assert_eq!(stored[0].observation.usage_event_id, None);
    assert_eq!(
        stored[0].observation.usage_link_kind,
        QuotaUsageLinkKind::None
    );

    store
        .upsert_quota_observations(std::slice::from_ref(&linked))
        .expect("restore linked observation");
    let mut missing = linked;
    missing.observation.usage_sample = None;
    missing.observation.usage_event_id = None;
    missing.observation.usage_link_kind = QuotaUsageLinkKind::None;
    store
        .upsert_quota_observations(std::slice::from_ref(&missing))
        .expect("missing archive sample");
    let stored = store
        .quota_observations(&QuotaQuery::default(), false)
        .expect("missing observation");
    assert_eq!(stored[0].observation.usage_event_id, None);
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
fn quota_store_handles_ten_thousand_observations_with_one_repeated_payload() {
    let store = Store::in_memory().expect("store");
    let observed_at = DateTime::from_timestamp(1_787_000_000, 0).expect("time");
    let base = sample_record(
        SourceId("source-scale".to_string()),
        "scale-0",
        "scale-0",
        observed_at,
        1_787_500_000,
        "primary",
        10_080,
        20.0,
    );
    let mut records = Vec::with_capacity(10_000);
    for index in 0..10_000 {
        let mut record = base.clone();
        record.observation.observation_id = format!("scale-{index}");
        record.observation.semantic_fingerprint = format!("semantic-scale-{index}");
        record.observation.source_record_id = format!("record-scale-{index}");
        record.observation.source_line_number = index as u64 + 1;
        record.observation.observed_at = observed_at + Duration::seconds(index as i64);
        record.windows[0].observation_id = record.observation.observation_id.clone();
        record.windows[0].window_observation_id = format!("window-scale-{index}");
        records.push(record);
    }
    store
        .upsert_quota_observations(&records)
        .expect("bulk observations");
    assert_eq!(
        store
            .quota_status(&QuotaQuery::default())
            .expect("status")
            .total_observations,
        10_000
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT COUNT(*) FROM quota_payloads", [], |row| {
                row.get::<_, u64>(0)
            })
            .expect("payload count"),
        1
    );
}

#[test]
fn quota_attribution_uses_exact_interval_boundary_and_reattributes_history() {
    let store = Store::in_memory().expect("store");
    let boundary = DateTime::from_timestamp(1_787_000_000, 0).expect("boundary");
    let (source_id, account_id) = assigned_source(&store, boundary);
    let before = sample_record(
        source_id.clone(),
        "before",
        "before",
        boundary - Duration::seconds(1),
        1_787_500_000,
        "primary",
        10_080,
        10.0,
    );
    let at = sample_record(
        source_id.clone(),
        "at",
        "at",
        boundary,
        1_787_500_100,
        "secondary",
        10_080,
        11.0,
    );
    store
        .upsert_quota_observations(&[before, at])
        .expect("upsert");
    let observations = store
        .quota_observations(&QuotaQuery::default(), false)
        .expect("observations");
    assert!(observations[0].observation.provider_account_id.is_none());
    assert_eq!(
        observations[1].observation.provider_account_id,
        Some(account_id.clone())
    );
    let mut assignment = store
        .list_source_account_assignments_for_source(&source_id)
        .expect("assignments")
        .remove(0);
    assignment.started_at = boundary - Duration::minutes(1);
    assignment.updated_at = Utc::now();
    store
        .upsert_source_account_assignment(&assignment)
        .expect("backdate assignment");
    store
        .reattribute_quota_observations(&source_id)
        .expect("reattribute");
    assert!(store
        .quota_observations(&QuotaQuery::default(), false)
        .expect("reattributed observations")
        .iter()
        .all(|record| record.observation.provider_account_id == Some(account_id.clone())));
}

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

#[allow(clippy::too_many_arguments)]
fn sample_usage_event(
    source_id: &SourceId,
    account_id: &ProviderAccountId,
    started_at: DateTime<Utc>,
    record_id: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    estimated_cost_micro_usd: i64,
) -> UsageEvent {
    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: event_id("codex", source_id, record_id, None, started_at),
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        provider_account_id: Some(account_id.clone()),
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: Some("quota-event".to_string()),
            source_record_id: Some(record_id.to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: record_id.to_string(),
            local_session_id_hash: Some(record_id.to_string()),
            title: None,
            started_at,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            cache_read_tokens: Some(cache_read_tokens),
            reasoning_tokens: Some(reasoning_tokens),
            total_tokens: Some(input_tokens + cache_read_tokens + output_tokens + reasoning_tokens),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: Some(estimated_cost_micro_usd),
            provider_reported_micro_usd: None,
            pricing_source: Some("test".to_string()),
            pricing_version: None,
            confidence: Confidence::High,
        },
        parse_evidence: None,
        project: None,
        git: None,
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        created_at: started_at,
        imported_at: started_at,
    }
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
