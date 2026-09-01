use super::support::*;
use super::*;

#[test]
fn retained_committed_metrics_are_rekeyed_once_an_account_identity_key_exists() {
    let store = Store::in_memory().expect("open store");
    let observation_start_day = statsai_core::git_observation_start_day(Utc::now());
    // Materialized before hosted login: a random ID no other device derives.
    let keyless = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "ccm_keyless".to_string(),
        device_id: "device".to_string(),
        day: observation_start_day.pred_opt().expect("historical day"),
        project_id: Some("project".to_string()),
        repository_hash: Some("repository".to_string()),
        commit_hash: Some("aged-commit".to_string()),
        kind: CodeChangeMetricKind::Committed,
        counts: CodeLineCounts::classified(CodeCategory::Source, 9, 3),
        attribution_confidence: None,
        trace_coverage: CoverageStatus::Unavailable,
        git_coverage: CoverageStatus::Complete,
    };
    store
        .replace_matches_and_metrics("device", &[], std::slice::from_ref(&keyless))
        .expect("seed keyless retained metric");

    let identity_key = [7_u8; 32];
    store
        .refresh_code_changes_with_identity_key("device", &identity_key)
        .expect("refresh after login");

    let expected_id = blinded_committed_metric_id(&identity_key, "repository", "aged-commit");
    let stored = store.list_code_change_metrics(false).expect("metrics");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].metric_id, expected_id);
    assert_eq!(stored[0].counts.source_additions, 9);
    assert!(
        !stored
            .iter()
            .any(|metric| metric.metric_id == "ccm_keyless"),
        "the underivable identity is retired rather than kept alongside"
    );

    // A second device on the same account converges on that identity.
    let other = Store::in_memory().expect("second store");
    let mut other_metric = keyless.clone();
    other_metric.metric_id = "ccm_other_random".to_string();
    other_metric.device_id = "device-b".to_string();
    other
        .replace_matches_and_metrics("device-b", &[], std::slice::from_ref(&other_metric))
        .expect("seed second device");
    other
        .refresh_code_changes_with_identity_key("device-b", &identity_key)
        .expect("refresh second device");
    assert_eq!(
        other.list_code_change_metrics(false).expect("metrics")[0].metric_id,
        expected_id
    );
}

#[test]
fn ingesting_duplicate_cross_device_commit_metrics_deduplicates_by_metric_id() {
    let store = Store::in_memory().expect("open store");
    let mut first = CodeChangeMetric {
        schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
        metric_id: "repository-plus-commit".to_string(),
        device_id: "device-a".to_string(),
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
        .ingest_code_change_metrics_inner(std::slice::from_ref(&first))
        .expect("ingest first");
    first.device_id = "device-b".to_string();
    store
        .ingest_code_change_metrics_inner(std::slice::from_ref(&first))
        .expect("ingest duplicate");

    let stored = store.list_code_change_metrics(false).expect("metrics");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].device_id, "device-b");
}

#[test]
fn committed_metric_ids_are_stable_per_user_across_stores() {
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

    let first = Store::in_memory().expect("first store");
    let legacy_scan =
        scan_local_git_repository_cached(repository.path(), None, &[], &BTreeSet::new())
            .expect("scan legacy commit");
    let legacy_commit = legacy_scan.commits.first().expect("legacy commit");
    let legacy_metric_id = legacy_commit.deduplication_id.clone();
    first
        .replace_matches_and_metrics(
            "device-a",
            &[],
            &[CodeChangeMetric {
                schema_version: CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
                metric_id: legacy_metric_id.clone(),
                device_id: "device-a".to_string(),
                day: legacy_commit.committed_at.date_naive(),
                project_id: Some("project".to_string()),
                repository_hash: Some(legacy_commit.repository_hash.clone()),
                commit_hash: Some(legacy_commit.commit_hash.clone()),
                kind: CodeChangeMetricKind::Committed,
                counts: CodeLineCounts::default(),
                attribution_confidence: None,
                trace_coverage: CoverageStatus::Unavailable,
                git_coverage: CoverageStatus::Complete,
            }],
        )
        .expect("seed legacy derivable metric id");
    insert_project_evidence(&first, repository.path(), "project", "first-summary");
    let user_identity_key = [7_u8; 32];
    first
        .refresh_code_changes_with_identity_key("device-a", &user_identity_key)
        .expect("first refresh");
    let first_id = first
        .list_code_change_metrics(false)
        .expect("first metrics")
        .into_iter()
        .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .expect("first committed metric")
        .metric_id;
    first
        .refresh_code_changes_with_identity_key("device-a", &user_identity_key)
        .expect("repeat refresh");
    let repeated_id = first
        .list_code_change_metrics(false)
        .expect("repeated metrics")
        .into_iter()
        .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .expect("repeated committed metric")
        .metric_id;

    let second = Store::in_memory().expect("second store");
    insert_project_evidence(&second, repository.path(), "project", "second-summary");
    second
        .refresh_code_changes_with_identity_key("device-b", &user_identity_key)
        .expect("second refresh");
    let second_id = second
        .list_code_change_metrics(false)
        .expect("second metrics")
        .into_iter()
        .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .expect("second committed metric")
        .metric_id;

    assert_eq!(first_id, repeated_id);
    assert_ne!(first_id, legacy_metric_id);
    assert_eq!(first_id, second_id);
    assert!(first_id.starts_with("ccm_"));
    assert!(second_id.starts_with("ccm_"));

    let third = Store::in_memory().expect("third store");
    insert_project_evidence(&third, repository.path(), "project", "third-summary");
    third
        .refresh_code_changes_with_identity_key("device-c", &[9_u8; 32])
        .expect("third refresh");
    let third_id = third
        .list_code_change_metrics(false)
        .expect("third metrics")
        .into_iter()
        .find(|metric| metric.kind == CodeChangeMetricKind::Committed)
        .expect("third committed metric")
        .metric_id;
    assert_ne!(first_id, third_id);
}
