use super::*;

#[test]
fn sync_rollups_export_path_only_project_metadata() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-path-only-projects"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let day = Utc
        .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
        .single()
        .expect("day");
    let account_id = statsai_core::provider_account_id("codex", "personal");

    let mut first = test_store_event(&source, day, "path-only-project-a");
    first.provider_account_id = Some(account_id.clone());
    first.usage.total_tokens = Some(10);
    first.project = Some(ProjectInfo {
        project_id: "project-path-a".to_string(),
        project_label: Some("hi".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-a".to_string()),
        path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
    });

    let mut second = test_store_event(
        &source,
        day + chrono::Duration::hours(1),
        "path-only-project-b",
    );
    second.provider_account_id = Some(account_id);
    second.usage.total_tokens = Some(20);
    second.project = Some(ProjectInfo {
        project_id: "project-path-b".to_string(),
        project_label: Some("hi".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-b".to_string()),
        path_label: Some("/Users/example/Documents/Codex/2026-05-28/hi".to_string()),
    });

    assert!(store.insert_event(&first).expect("insert first"));
    assert!(store.insert_event(&second).expect("insert second"));

    let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
    assert_eq!(dirty.len(), 2);
    let projects = dirty
        .iter()
        .map(|summary| summary.project.as_ref().expect("project metadata"))
        .collect::<Vec<_>>();
    assert!(projects
        .iter()
        .all(|project| project.repo_remote_hash.is_none()));
    assert_eq!(
        projects
            .iter()
            .filter_map(|project| project.path_label.as_deref())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "/Users/example/Documents/Codex/2026-05-28/hi",
            "/Users/example/Documents/Codex/2026-05-29/hi",
        ])
    );
    assert_eq!(
        dirty
            .iter()
            .map(|summary| summary.usage.total_tokens.unwrap_or(0))
            .sum::<u64>(),
        30
    );
}

#[test]
fn daily_rollup_saturates_imported_usage_and_costs() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-rollup-overflow"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let mut first = test_store_event(&source, now, "overflow-a");
    first.usage = UsageCounts {
        input_tokens: Some(u64::MAX),
        cache_creation_tokens: Some(u64::MAX),
        cache_read_tokens: Some(u64::MAX),
        output_tokens: Some(u64::MAX),
        reasoning_tokens: Some(u64::MAX),
        total_tokens: Some(u64::MAX),
        ..UsageCounts::default()
    };
    first.cost.estimated_api_equivalent_usd = Some(i64::MAX);
    let mut second = first.clone();
    second.event_id = event_id(
        "codex",
        &source.source_id,
        "overflow-b",
        None,
        now + chrono::Duration::seconds(1),
    );
    second.session.started_at = now + chrono::Duration::seconds(1);
    second.source.source_record_id = Some("overflow-b".to_string());
    store.insert_events(&[first, second]).expect("events");

    let rollup = store
        .compute_daily_rollup(&now.format("%Y-%m-%d").to_string(), "device")
        .expect("rollup");

    assert_eq!(rollup.total_input_tokens, u64::MAX);
    assert_eq!(rollup.total_cache_creation_tokens, u64::MAX);
    assert_eq!(rollup.total_cache_read_tokens, u64::MAX);
    assert_eq!(rollup.total_output_tokens, u64::MAX);
    assert_eq!(rollup.total_reasoning_tokens, u64::MAX);
    assert_eq!(rollup.total_tokens, u64::MAX);
    assert_eq!(rollup.total_events, 2);
    assert_eq!(rollup.estimated_cost_usd, Some(i64::MAX));
    let by_provider: serde_json::Value =
        serde_json::from_str(rollup.by_provider.as_deref().expect("provider totals"))
            .expect("provider JSON");
    assert_eq!(by_provider["codex"]["tokens"].as_u64(), Some(u64::MAX));
}

#[test]
fn sync_rollup_sums_micro_usd_before_rounding_to_cents() {
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-micro-usd-rollup"),
        LocationOrigin::Configured,
    );
    let day = Utc
        .with_ymd_and_hms(2026, 5, 28, 9, 0, 0)
        .single()
        .expect("day");
    let mut event = test_store_event(&source, day, "record-a");
    event.cost.set_estimated_micro_usd(2_250);
    let events = vec![event; 1_000];

    let summary = build_sync_rollup_summary(&events);

    assert_eq!(
        summary.cost.estimated_api_equivalent_micro_usd,
        Some(2_250_000)
    );
    assert_eq!(summary.cost.estimated_api_equivalent_usd, Some(225));
}

#[test]
fn sync_rollups_preserve_cache_creation_lifetimes() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-sync-rollup-cache-ttl"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 7, 12, 10, 0, 0)
        .single()
        .expect("now");
    let mut event = test_store_event(&source, now, "record-cache-ttl");
    event.usage.cache_creation_tokens = Some(30);
    event.usage.cache_creation_5m_tokens = Some(18);
    event.usage.cache_creation_1h_tokens = Some(12);

    assert!(store.insert_event(&event).expect("insert event"));
    let rollups = store.dirty_sync_rollup_summaries().expect("dirty rollups");

    assert_eq!(rollups.len(), 1);
    assert_eq!(rollups[0].usage.cache_creation_tokens, Some(30));
    assert_eq!(rollups[0].usage.cache_creation_5m_tokens, Some(18));
    assert_eq!(rollups[0].usage.cache_creation_1h_tokens, Some(12));
    assert_eq!(rollups[0].models.len(), 1);
    assert_eq!(
        rollups[0].models[0].usage.cache_creation_5m_tokens,
        Some(18)
    );
    assert_eq!(
        rollups[0].models[0].usage.cache_creation_1h_tokens,
        Some(12)
    );
}

#[test]
fn sync_rollups_split_same_model_by_reasoning_level() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-rollups-reasoning"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 5, 29, 9, 0, 0)
        .single()
        .expect("day");
    let mut low = test_store_event(&source, day, "record-low");
    low.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });
    low.usage.total_tokens = Some(15);

    let mut high = test_store_event(&source, day + chrono::Duration::hours(1), "record-high");
    high.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });
    high.usage.total_tokens = Some(25);

    assert!(store.insert_event(&low).expect("insert low"));
    assert!(store.insert_event(&high).expect("insert high"));

    let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].models.len(), 2);
    assert!(dirty[0].models.iter().any(|entry| {
        entry.model.reasoning_level == Some(ReasoningLevel::Low)
            && entry.usage.total_tokens == Some(15)
    }));
    assert!(dirty[0].models.iter().any(|entry| {
        entry.model.reasoning_level == Some(ReasoningLevel::High)
            && entry.usage.total_tokens == Some(25)
    }));
}

#[test]
fn sync_rollups_split_same_model_by_speed() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-sync-rollups-speed"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 8, 1, 9, 0, 0)
        .single()
        .expect("day");
    let mut standard = test_store_event(&source, day, "record-standard");
    standard.model = Some(ModelInfo {
        name: Some("claude-opus-5".to_string()),
        normalized_name: Some("claude-opus-5".to_string()),
        provider_model_id: Some("claude-opus-5".to_string()),
        speed: Some("standard".to_string()),
        reasoning_level: Some(ReasoningLevel::Medium),
        reasoning_level_raw: Some("medium".to_string()),
    });
    standard.usage.total_tokens = Some(15);

    let mut fast = test_store_event(&source, day + chrono::Duration::hours(1), "record-fast");
    fast.model = Some(ModelInfo {
        name: Some("claude-opus-5".to_string()),
        normalized_name: Some("claude-opus-5".to_string()),
        provider_model_id: Some("claude-opus-5".to_string()),
        speed: Some("fast".to_string()),
        reasoning_level: Some(ReasoningLevel::Medium),
        reasoning_level_raw: Some("medium".to_string()),
    });
    fast.usage.total_tokens = Some(25);

    assert!(store.insert_event(&standard).expect("insert standard"));
    assert!(store.insert_event(&fast).expect("insert fast"));

    let dirty = store.dirty_sync_rollup_summaries().expect("dirty rollups");
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].models.len(), 2);
    assert!(dirty[0].models.iter().any(|entry| {
        entry.model.speed.as_deref() == Some("standard") && entry.usage.total_tokens == Some(15)
    }));
    assert!(dirty[0].models.iter().any(|entry| {
        entry.model.speed.as_deref() == Some("fast") && entry.usage.total_tokens == Some(25)
    }));
}

#[test]
fn sync_rollups_split_same_day_usage_by_project_location() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-projects"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
        .single()
        .expect("day");
    let account_id = statsai_core::provider_account_id("codex", "personal");

    let mut first = test_store_event(&source, day, "record-project-a");
    first.provider_account_id = Some(account_id.clone());
    first.usage.total_tokens = Some(10);
    first.project = Some(statsai_core::ProjectInfo {
        project_id: "project-a".to_string(),
        project_label: Some("Project A".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-a".to_string()),
        path_label: Some("/tmp/project-a".to_string()),
    });

    let mut second = test_store_event(
        &source,
        day + chrono::Duration::hours(1),
        "record-project-b",
    );
    second.provider_account_id = Some(account_id);
    second.usage.total_tokens = Some(20);
    second.project = Some(statsai_core::ProjectInfo {
        project_id: "project-b".to_string(),
        project_label: Some("Project B".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-b".to_string()),
        path_label: Some("/tmp/project-b".to_string()),
    });

    assert!(store.insert_event(&first).expect("insert first"));
    assert!(store.insert_event(&second).expect("insert second"));

    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after project split");
    assert_eq!(dirty.len(), 2);
    assert_ne!(dirty[0].summary_id, dirty[1].summary_id);
    assert_ne!(dirty[0].project, dirty[1].project);
    assert_eq!(
        dirty
            .iter()
            .map(|summary| summary.usage.total_tokens.unwrap_or(0))
            .sum::<u64>(),
        30
    );
}

#[test]
fn sync_rollups_split_same_day_usage_by_branch() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-branches"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
        .single()
        .expect("day");
    let account_id = statsai_core::provider_account_id("codex", "personal");

    let mut first = test_store_event(&source, day, "record-branch-main");
    first.provider_account_id = Some(account_id.clone());
    first.usage.total_tokens = Some(10);
    first.project = Some(statsai_core::ProjectInfo {
        project_id: "project-shared".to_string(),
        project_label: Some("Project".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-shared".to_string()),
        path_label: Some("/tmp/project".to_string()),
    });

    let mut second = test_store_event(
        &source,
        day + chrono::Duration::hours(1),
        "record-branch-feature",
    );
    second.provider_account_id = Some(account_id);
    second.usage.total_tokens = Some(20);
    second.project = Some(statsai_core::ProjectInfo {
        project_id: "project-shared".to_string(),
        project_label: Some("Project".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-feature".to_string()),
        branch_label: Some("feature-x".to_string()),
        path_hash: Some("path-shared".to_string()),
        path_label: Some("/tmp/project".to_string()),
    });

    assert!(store.insert_event(&first).expect("insert first"));
    assert!(store.insert_event(&second).expect("insert second"));

    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after branch split");
    assert_eq!(dirty.len(), 2);

    let mut branches = dirty
        .iter()
        .map(|summary| {
            summary
                .project
                .as_ref()
                .and_then(|project| project.branch_label.clone())
                .expect("branch")
        })
        .collect::<Vec<_>>();
    branches.sort();

    assert_eq!(branches, vec!["feature-x".to_string(), "main".to_string()]);
}

#[test]
fn path_independent_codex_events_keep_distinct_branches() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-branch-dedupe"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut main = test_store_event(&source, now, "branch-main");
    main.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("main-hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some(
                "semantic_usage_event.v4:codex_turn_usage:repo:repo-hash|path:path-shared|branch:branch-main:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });
    main.model = Some(ModelInfo {
        name: Some("gpt-5".to_string()),
        normalized_name: Some("gpt-5".to_string()),
        provider_model_id: Some("gpt-5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    main.project = Some(ProjectInfo {
        project_id: "project-shared".to_string(),
        project_label: Some("Project".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-main".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-shared".to_string()),
        path_label: Some("/tmp/project".to_string()),
    });

    let mut feature = main.clone();
    feature.event_id = event_id("codex", &source.source_id, "branch-feature", None, now);
    feature.source.source_record_id = Some("branch-feature".to_string());
    feature.project = Some(ProjectInfo {
        project_id: "project-shared".to_string(),
        project_label: Some("Project".to_string()),
        repo_remote_hash: Some("repo-hash".to_string()),
        repo_label: Some("owner/repo".to_string()),
        branch_hash: Some("branch-feature".to_string()),
        branch_label: Some("feature-x".to_string()),
        path_hash: Some("path-shared".to_string()),
        path_label: Some("/tmp/project".to_string()),
    });
    feature.parse_evidence = Some(ParseEvidence {
            event_key_version: "semantic_usage_event.v4".to_string(),
            source_file_path_hash: Some("feature-hash".to_string()),
            source_line_number: Some(18),
            source_record_id: Some(
                "semantic_usage_event.v4:codex_turn_usage:repo:repo-hash|path:path-shared|branch:branch-feature:1715510400000:1715510405000:gpt-5:12:0:3:0:15"
                    .to_string(),
            ),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: statsai_core::IdentitySource::Unresolved,
        });

    assert!(store.insert_event(&main).expect("insert main"));
    assert!(store.insert_event(&feature).expect("insert feature"));
    assert_eq!(store.event_count().expect("count"), 2);
}

#[test]
fn sync_rollups_capture_daily_runtime_metrics() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-metrics"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 5, 29, 9, 0, 0)
        .single()
        .expect("day");
    let mut first = test_store_event(&source, day, "metrics-a");
    first.session.session_id = "session-a".to_string();
    first.session.local_session_id_hash = Some("session-a".to_string());
    first.model = Some(ModelInfo {
        name: Some("gpt-5.6-sol".to_string()),
        normalized_name: Some("gpt-5.6-sol".to_string()),
        provider_model_id: Some("gpt-5.6-sol".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });
    first.usage = UsageCounts {
        input_tokens: Some(60),
        output_tokens: Some(30),
        cache_read_tokens: Some(20),
        reasoning_tokens: Some(10),
        total_tokens: Some(120),
        requests: Some(1),
        ..UsageCounts::default()
    };
    first.runtime = Some(statsai_core::RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: Some(5000),
        latency_source: Some(LatencySource::Explicit),
        time_to_first_token_ms: Some(1200),
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(2),
        user_messages: Some(1),
        assistant_messages: Some(1),
        developer_messages: Some(0),
    });

    let mut second = test_store_event(&source, day + chrono::Duration::minutes(2), "metrics-b");
    second.session.session_id = "session-b".to_string();
    second.session.local_session_id_hash = Some("session-b".to_string());
    second.model = Some(ModelInfo {
        name: Some("codex-auto-review".to_string()),
        normalized_name: Some("codex-auto-review".to_string()),
        provider_model_id: Some("codex-auto-review".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });
    second.usage = UsageCounts {
        input_tokens: Some(40),
        output_tokens: Some(20),
        cache_read_tokens: Some(10),
        reasoning_tokens: Some(0),
        total_tokens: Some(70),
        requests: Some(1),
        ..UsageCounts::default()
    };
    second.runtime = Some(statsai_core::RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: Some(3000),
        latency_source: Some(LatencySource::Explicit),
        time_to_first_token_ms: Some(800),
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(3),
        user_messages: Some(1),
        assistant_messages: Some(2),
        developer_messages: Some(0),
    });

    assert!(store.insert_event(&first).expect("insert first"));
    assert!(store.insert_event(&second).expect("insert second"));

    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after metrics");
    assert_eq!(dirty.len(), 1);
    assert_eq!(
        dirty[0].metadata.summary_version.as_deref(),
        Some(SYNC_ROLLUP_SUMMARY_VERSION)
    );
    assert_eq!(dirty[0].metadata.total_sessions, Some(2));
    assert_eq!(dirty[0].metadata.total_messages, Some(5));
    let metrics = dirty[0].metrics.as_ref().expect("metrics");
    assert_eq!(metrics.active_seconds, Some(8.0));
    assert_eq!(metrics.tracked_requests, Some(2));
    assert_eq!(metrics.tracked_output_tokens, Some(50));
    assert_eq!(metrics.tracked_reasoning_tokens, Some(10));
    assert_eq!(metrics.total_messages, Some(5));
    assert_eq!(metrics.user_messages, Some(2));
    assert_eq!(metrics.assistant_messages, Some(3));
    assert_eq!(
        metrics.latency_ms.as_ref().map(|value| value.samples),
        Some(2)
    );
    assert_eq!(
        metrics.latency_ms.as_ref().and_then(|value| value.min),
        Some(3000.0)
    );
    assert_eq!(
        metrics.latency_ms.as_ref().and_then(|value| value.max),
        Some(5000.0)
    );
    assert_eq!(
        metrics
            .time_to_first_token_ms
            .as_ref()
            .and_then(|value| value.avg),
        Some(1000.0)
    );
    assert_eq!(
        metrics.generated_tps.as_ref().and_then(|value| value.min),
        Some(20.0 / 3.0)
    );
    assert_eq!(metrics.overall_generated_tps, Some(7.5));
    assert_eq!(metrics.overall_visible_tps, Some(6.25));
    assert_eq!(dirty[0].models.len(), 2);
    let primary = dirty[0]
        .models
        .iter()
        .find(|entry| entry.model.normalized_name.as_deref() == Some("gpt-5.6-sol"))
        .expect("primary model metrics");
    assert_eq!(
        primary
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.generated_tps.as_ref()),
        Some(&SummaryMetricTotals {
            samples: 1,
            sum: 8.0,
        })
    );
    let reviewer = dirty[0]
        .models
        .iter()
        .find(|entry| entry.model.normalized_name.as_deref() == Some("codex-auto-review"))
        .expect("review model metrics");
    assert_eq!(
        reviewer
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.generated_tps.as_ref()),
        Some(&SummaryMetricTotals {
            samples: 1,
            sum: 20.0 / 3.0,
        })
    );
}

#[test]
fn sync_rollups_exclude_inferred_latency_from_per_turn_sample_metrics() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-sync-inferred-latency"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let day = Utc
        .with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
        .single()
        .expect("day");

    let mut explicit = test_store_event(&source, day, "explicit-runtime");
    explicit.session.session_id = "session-explicit".to_string();
    explicit.session.local_session_id_hash = Some("session-explicit".to_string());
    explicit.usage = UsageCounts {
        output_tokens: Some(30),
        reasoning_tokens: Some(10),
        total_tokens: Some(40),
        requests: Some(1),
        ..UsageCounts::default()
    };
    explicit.runtime = Some(statsai_core::RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: Some(5000),
        latency_source: Some(LatencySource::Explicit),
        time_to_first_token_ms: Some(1200),
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(2),
        user_messages: Some(1),
        assistant_messages: Some(1),
        developer_messages: Some(0),
    });

    let mut inferred = test_store_event(
        &source,
        day + chrono::Duration::minutes(5),
        "inferred-runtime",
    );
    inferred.session.session_id = "session-inferred".to_string();
    inferred.session.local_session_id_hash = Some("session-inferred".to_string());
    inferred.usage = UsageCounts {
        output_tokens: Some(700),
        reasoning_tokens: Some(300),
        total_tokens: Some(1000),
        requests: Some(1),
        ..UsageCounts::default()
    };
    inferred.runtime = Some(statsai_core::RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: Some(100),
        latency_source: Some(LatencySource::Inferred),
        time_to_first_token_ms: None,
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(2),
        user_messages: Some(1),
        assistant_messages: Some(1),
        developer_messages: Some(0),
    });

    assert!(store.insert_event(&explicit).expect("insert explicit"));
    assert!(store.insert_event(&inferred).expect("insert inferred"));

    let dirty = store
        .dirty_sync_rollup_summaries()
        .expect("dirty rollups after inferred metrics");
    assert_eq!(dirty.len(), 1);
    let metrics = dirty[0].metrics.as_ref().expect("metrics");
    assert_eq!(metrics.active_seconds, Some(5.1));
    assert_eq!(metrics.tracked_requests, Some(2));
    assert_eq!(metrics.tracked_output_tokens, Some(730));
    assert_eq!(metrics.tracked_reasoning_tokens, Some(310));
    assert_eq!(
        metrics.latency_ms.as_ref().map(|value| value.samples),
        Some(1)
    );
    assert_eq!(
        metrics.generated_tps.as_ref().map(|value| value.samples),
        Some(1)
    );
    assert_eq!(
        metrics.generated_tps.as_ref().and_then(|value| value.avg),
        Some(8.0)
    );
    assert_eq!(
        metrics.visible_tps.as_ref().and_then(|value| value.avg),
        Some(6.0)
    );
    assert_eq!(metrics.overall_generated_tps, Some(1040.0 / 5.1));
    assert_eq!(metrics.overall_visible_tps, Some(730.0 / 5.1));
}
