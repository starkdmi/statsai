use super::support::*;
use super::*;

#[test]
fn retiring_an_archive_file_removes_its_edits_whatever_the_record_id_looks_like() {
    let store = Store::in_memory().expect("open store");
    let source_id = statsai_core::SourceId("source".to_string());
    let edit = |cache_key: &str, source_record_id: &str, trace_edit_id: &str| TraceEdit {
        schema_version: statsai_core::TRACE_EDIT_SCHEMA_VERSION.to_string(),
        trace_edit_id: trace_edit_id.to_string(),
        provider: "codex".to_string(),
        source_id: source_id.clone(),
        cache_key: cache_key.to_string(),
        conversation_id: "conversation".to_string(),
        // Deliberately unlike `{cache_key}:{ordinal}` — the shape a future
        // provider might use, which the old prefix delete relied on.
        source_record_id: source_record_id.to_string(),
        occurred_at: None,
        project_id: None,
        repository_path: None,
        relative_path: PathBuf::from("src/lib.rs"),
        category: CodeCategory::Source,
        mutation_kind: statsai_core::MutationKind::StructuredEdit,
        counts: CodeLineCounts::classified(CodeCategory::Source, 1, 0),
        added_line_fingerprints: Vec::new(),
        deleted_line_fingerprints: Vec::new(),
    };
    let entry = |cache_key: &str| crate::ScanFileStateEntry {
        cache_key: cache_key.to_string(),
        cache_signature: "signature".to_string(),
    };
    store
        .upsert_archive_conversations(&[statsai_core::ArchiveConversation {
            schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: "conversation".to_string(),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            native_conversation_id: "thread".to_string(),
            title: None,
            project: None,
            started_at: None,
            updated_at: None,
            completeness: statsai_core::ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        }])
        .expect("seed owning conversation");
    store
        .replace_archive_trace_edits_inner(
            &source_id,
            &[
                entry("/archive/first.jsonl"),
                entry("/archive/second.jsonl"),
            ],
            &[
                edit("/archive/first.jsonl", "record|1", "edit-first"),
                edit("/archive/second.jsonl", "record|2", "edit-second"),
            ],
            CoverageStatus::Complete,
        )
        .expect("seed trace edits");
    assert_eq!(store.list_trace_edits().expect("seeded").len(), 2);

    store
        .delete_archive_trace_entry_inner(&source_id, "/archive/first.jsonl")
        .expect("retire the first archive file");

    let remaining = store.list_trace_edits().expect("remaining");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].trace_edit_id, "edit-second");
}

#[test]
fn refresh_retains_committed_metrics_older_than_the_git_scan_window() {
    let repository = TempDir::new().expect("temporary repository");
    run_test_git(repository.path(), &["init", "-q"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_test_git(repository.path(), &["config", "user.name", "Test"]);
    fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
    run_test_git(repository.path(), &["add", "main.rs"]);
    run_test_git(repository.path(), &["commit", "-qm", "initial"]);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
    let aged_metric = |metric_id: &str, day: NaiveDate| CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: metric_id.to_string(),
        device_id: "device".to_string(),
        day,
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("old-commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 7, 2),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Unavailable,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics(
            "device",
            &[],
            &[
                aged_metric(
                    "ccm_historical",
                    observation_start_day.pred_opt().expect("historical day"),
                ),
                // The scan's cutoff is an instant on this day, so commits
                // earlier in it are no longer rescanned either.
                aged_metric("ccm_boundary", observation_start_day),
            ],
        )
        .expect("seed aged metrics");

    store
        .refresh_code_changes("device")
        .expect("refresh after the commits aged out");

    let stored = store.list_code_change_metrics(false).expect("metrics");
    for metric_id in ["ccm_historical", "ccm_boundary"] {
        let retained = stored
            .iter()
            .find(|metric| metric.metric_id == metric_id)
            .unwrap_or_else(|| panic!("{metric_id} survives the rolling window"));
        assert_eq!(retained.counts.source_additions, 7);
    }
    assert!(stored.iter().any(|metric| {
        !metric.metric_id.starts_with("ccm_")
            || (metric.metric_id != "ccm_historical" && metric.metric_id != "ccm_boundary")
    }));
}

#[test]
fn retained_committed_metrics_keep_the_commit_identity_their_payload_omits() {
    let store = Store::in_memory().expect("open store");
    let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
    let aged = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "ccm_aged".to_string(),
        device_id: "device".to_string(),
        day: observation_start_day.pred_opt().expect("historical day"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("aged-commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 5, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Unavailable,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&aged))
        .expect("seed aged metric");

    // Two refreshes: the first carries the metric forward, the second reads
    // back what the first one wrote.
    for _ in 0..2 {
        store
            .refresh_code_changes("device")
            .expect("refresh after the commit aged out");
    }

    let stored_commit_hash: Option<String> = store
        .conn
        .query_row(
            "SELECT commit_hash FROM code_change_metrics WHERE metric_id = 'ccm_aged'",
            [],
            |row| row.get(0),
        )
        .expect("retained metric row");
    assert_eq!(stored_commit_hash.as_deref(), Some("aged-commit"));
}

#[test]
fn trace_matched_metrics_are_not_carried_past_the_git_scan_window() {
    let store = Store::in_memory().expect("open store");
    let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
    let aged_match = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "aged-attribution".to_string(),
        device_id: "device".to_string(),
        day: observation_start_day.pred_opt().expect("historical day"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("aged-commit".to_string()),
        kind: CodeChangeMetricKind::TraceMatchedCommitted,
        counts: CodeLineCounts::classified(CodeCategory::Source, 4, 0),
        attribution_confidence: Some(AttributionConfidence::High),
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&aged_match))
        .expect("seed aged attribution");

    store
        .refresh_code_changes("device")
        .expect("refresh after the commit aged out");

    assert!(
        store
            .list_code_change_metrics(false)
            .expect("metrics")
            .is_empty(),
        "an attribution that can no longer be reverified is retired, not frozen"
    );
}

#[test]
fn aged_committed_metrics_are_retired_once_their_repository_is_unreferenced() {
    let repository = TempDir::new().expect("temporary repository");
    init_test_repository(repository.path());

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    seed_aged_committed_metric(&store, "ccm_unreferenced", &stored_repository_hash(&store));
    store
        .refresh_code_changes("device")
        .expect("refresh carries the aged metric");
    assert!(metric_exists(&store, "ccm_unreferenced"));

    // Nothing references the repository any more. Its aged metrics are past
    // the scan window, so no rebuild can retire them; carrying them forward
    // would republish them in every authoritative snapshot indefinitely.
    store
        .conn
        .execute("DELETE FROM usage_summaries", [])
        .expect("remove project evidence");
    store
        .refresh_code_changes("device")
        .expect("refresh without references");

    assert!(!metric_exists(&store, "ccm_unreferenced"));
}

#[test]
fn aged_committed_metrics_survive_a_repository_identity_change() {
    let repository = TempDir::new().expect("temporary repository");
    init_test_repository(repository.path());

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    let original_hash = stored_repository_hash(&store);
    seed_aged_committed_metric(&store, "ccm_reidentified", &original_hash);

    // Adding an origin remote re-keys the repository, so its aged metrics
    // are left under a hash no scan uses. The repository is still referenced
    // and the work is still the user's, so retirement must follow the
    // repository root rather than the absence of its old hash.
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    store
        .refresh_code_changes("device")
        .expect("refresh after identity change");

    assert_ne!(stored_repository_hash(&store), original_hash);
    assert!(metric_exists(&store, "ccm_reidentified"));
}

#[test]
fn aged_committed_metrics_survive_a_repository_that_moved() {
    let parent = TempDir::new().expect("temporary parent");
    let original = parent.path().join("original");
    fs::create_dir_all(&original).expect("create repository");
    init_test_repository(&original);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, &original, "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    let repository_hash = stored_repository_hash(&store);
    seed_aged_committed_metric(&store, "ccm_moved", &repository_hash);
    store
        .refresh_code_changes("device")
        .expect("refresh carries the aged metric");
    assert!(metric_exists(&store, "ccm_moved"));

    // Repository identity is derived from the root commits, never the
    // location, so moving a worktree keeps the hash and only rewrites the
    // stored path. Deciding retirement from the path alone would read a live
    // repository as gone and prune days no rebuild can reach.
    let moved = parent.path().join("moved");
    fs::rename(&original, &moved).expect("move the repository");
    repoint_project_evidence(&store, &moved, "summary-moved");
    store
        .refresh_code_changes("device")
        .expect("refresh after the move");

    assert_eq!(stored_repository_hash(&store), repository_hash);
    assert!(metric_exists(&store, "ccm_moved"));
}

#[test]
fn aged_committed_metrics_are_retired_after_a_rekey_then_an_unreference() {
    let repository = TempDir::new().expect("temporary repository");
    init_test_repository(repository.path());

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, repository.path(), "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    seed_aged_committed_metric(&store, "ccm_rekeyed", &stored_repository_hash(&store));

    // Re-keyed while still referenced: the metric has to survive, but it is
    // now filed under a hash the repository has stopped using.
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    store
        .refresh_code_changes("device")
        .expect("refresh after the re-key");
    assert!(metric_exists(&store, "ccm_rekeyed"));

    // Unreferenced afterwards. Retirement compares against the hashes the
    // repository is known by now, so a metric left under a superseded hash
    // would match nothing, survive every later refresh, and be republished in
    // the authoritative snapshot forever.
    store
        .conn
        .execute("DELETE FROM usage_summaries", [])
        .expect("remove project evidence");
    store
        .refresh_code_changes("device")
        .expect("refresh without references");

    assert!(!metric_exists(&store, "ccm_rekeyed"));
}

#[test]
fn aged_committed_metrics_survive_a_simultaneous_move_and_rekey() {
    let parent = TempDir::new().expect("temporary parent");
    let original = parent.path().join("original");
    fs::create_dir_all(&original).expect("create repository");
    init_test_repository(&original);

    let store = Store::in_memory().expect("open store");
    insert_project_evidence(&store, &original, "project", "summary");
    store.refresh_code_changes("device").expect("first refresh");
    seed_aged_committed_metric(&store, "ccm_both", &stored_repository_hash(&store));
    store
        .refresh_code_changes("device")
        .expect("refresh carries the aged metric");

    // Both names change at once, so neither identifies the repository any
    // more. The commits do: they are the same objects, and a commit hash is
    // globally unique, so the fresh scan is recognised as the same repository
    // and claims the history stored under the old hash.
    let moved = parent.path().join("moved");
    fs::rename(&original, &moved).expect("move the repository");
    run_test_git(
        &moved,
        &["remote", "add", "origin", "https://example.com/renamed.git"],
    );
    repoint_project_evidence(&store, &moved, "summary-moved");
    store
        .refresh_code_changes("device")
        .expect("refresh after the move and re-key");

    assert!(metric_exists(&store, "ccm_both"));
}

#[test]
fn replacing_identical_metrics_is_idempotent_and_does_not_redirty_them() {
    let store = Store::in_memory().expect("open store");
    let metric = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "metric-1".to_string(),
        device_id: "device".to_string(),
        day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };

    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
        .expect("first replace");
    store
        .mark_code_change_metrics_synced(std::slice::from_ref(&metric.metric_id))
        .expect("mark synced");
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
        .expect("second replace");

    assert_eq!(store.list_code_change_metrics(false).expect("all").len(), 1);
    assert!(store
        .list_code_change_metrics(true)
        .expect("dirty")
        .is_empty());
}

#[test]
fn local_refresh_preserves_metrics_ingested_from_another_device() {
    let store = Store::in_memory().expect("open store");
    let remote_metric = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "remote-metric".to_string(),
        device_id: "remote-device".to_string(),
        day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
        project_id: None,
        repository_hash: Some("remote-repository".to_string()),
        commit_hash: Some("remote-commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .ingest_sync_batch(&SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "remote-batch".to_string(),
            device_id: "remote-device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            account_plan_observations: Vec::new(),
            account_evidence_summaries: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            code_change_metrics: vec![remote_metric.clone()],
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: Utc::now(),
        })
        .expect("ingest remote batch");
    let mut stale_local_metric = remote_metric.clone();
    stale_local_metric.metric_id = "stale-local-metric".to_string();
    stale_local_metric.device_id = "local-device".to_string();
    store
        .replace_matches_and_metrics(
            "local-device",
            &[],
            std::slice::from_ref(&stale_local_metric),
        )
        .expect("store stale local metric");

    store
        .refresh_code_changes("local-device")
        .expect("refresh local metrics");

    // The Git object ID survives the round trip even though the payload
    // omits it, so no reader sees a metric that under-reports itself.
    assert_eq!(
        store.list_code_change_metrics(false).expect("metrics"),
        vec![remote_metric]
    );
}

#[test]
fn pending_metrics_are_tracked_independently_per_sync_target() {
    let store = Store::in_memory().expect("open store");
    let metric = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "metric-per-target".to_string(),
        device_id: "device".to_string(),
        day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
        project_id: None,
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
        .expect("store metric");

    assert_eq!(
        store
            .pending_code_change_metrics_for_sync("http", "target-a", std::slice::from_ref(&metric))
            .expect("target a pending")
            .len(),
        1
    );
    store
        .record_code_change_metrics_synced("http", "target-a", std::slice::from_ref(&metric))
        .expect("record target a");
    assert!(store
        .pending_code_change_metrics_for_sync("http", "target-a", std::slice::from_ref(&metric))
        .expect("target a settled")
        .is_empty());
    assert_eq!(
        store
            .pending_code_change_metrics_for_sync("http", "target-b", std::slice::from_ref(&metric))
            .expect("target b pending")
            .len(),
        1
    );
}

#[test]
fn ingesting_corrected_metric_replaces_stale_columns_and_payload() {
    let store = Store::in_memory().expect("open store");
    let original = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "corrected-metric".to_string(),
        device_id: "device".to_string(),
        day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("original date"),
        project_id: None,
        repository_hash: None,
        commit_hash: None,
        kind: CodeChangeMetricKind::AgentEdit,
        counts: CodeLineCounts::classified(CodeCategory::Source, 1, 0),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Partial,
        git_coverage: CoverageStatus::Partial,
    };
    let corrected = CodeChangeMetric {
        day: NaiveDate::from_ymd_opt(2026, 8, 2).expect("corrected date"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        kind: CodeChangeMetricKind::TraceMatchedCommitted,
        counts: CodeLineCounts::classified(CodeCategory::Source, 5, 2),
        attribution_confidence: Some(AttributionConfidence::High),
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
        ..original.clone()
    };

    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&original))
        .expect("ingest original metric");
    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&corrected))
        .expect("ingest corrected metric");

    assert_eq!(
        store.list_code_change_metrics(false).expect("metrics"),
        vec![corrected]
    );
    let stored_columns = store
        .conn
        .query_row(
            r#"
                SELECT day, project_id, repository_hash, kind, dirty
                FROM code_change_metrics
                WHERE metric_id = 'corrected-metric'
                "#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )
        .expect("stored columns");
    assert_eq!(
        stored_columns,
        (
            "2026-08-02".to_string(),
            Some("project".to_string()),
            Some("repository".to_string()),
            "trace_matched_committed".to_string(),
            0,
        )
    );
}

#[test]
fn ingesting_identical_metric_preserves_existing_dirty_state() {
    let store = Store::in_memory().expect("open store");
    let metric = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "unchanged-metric".to_string(),
        device_id: "device".to_string(),
        day: NaiveDate::from_ymd_opt(2026, 8, 1).expect("date"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: None,
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 3, 1),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Complete,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&metric))
        .expect("store dirty local metric");

    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&metric))
        .expect("ingest identical metric");

    assert_eq!(
        store.list_code_change_metrics(true).expect("dirty metrics"),
        vec![metric]
    );
}
