use super::support::*;
use super::*;

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
