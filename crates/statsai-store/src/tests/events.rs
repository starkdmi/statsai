use super::support::*;
use super::*;

#[test]
fn inserts_events_idempotently() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let id = event_id("codex", &source.source_id, "record", None, now);
    let mut event = UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id: id,
        device_id: "device".to_string(),
        provider: "codex".to_string(),
        source_id: source.source_id,
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: "test".to_string(),
            adapter_version: "0".to_string(),
            source_kind: SourceKind::LocalAdapter,
            location_origin: Some(LocationOrigin::Configured),
            source_type: "jsonl".to_string(),
            source_path_hash: None,
            source_record_id: Some("record".to_string()),
            parse_confidence: Confidence::High,
        },
        session: SessionInfo {
            session_id: "session".to_string(),
            local_session_id_hash: None,
            title: None,
            started_at: now,
            ended_at: None,
            duration_seconds: None,
        },
        model: None,
        usage: UsageCounts {
            total_tokens: Some(10),
            ..UsageCounts::default()
        },
        runtime: None,
        cost: CostInfo {
            currency: "USD".to_string(),
            estimated_api_equivalent_usd: None,
            provider_reported_usd: None,
            estimated_api_equivalent_micro_usd: None,
            provider_reported_micro_usd: None,
            pricing_source: Some("unknown".to_string()),
            pricing_version: None,
            confidence: Confidence::Low,
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
        created_at: now,
        imported_at: now,
    };

    assert!(store.insert_event(&event).expect("insert"));
    assert!(!store.insert_event(&event).expect("dedupe"));
    assert_eq!(store.event_count().expect("count"), 1);

    event.usage.input_tokens = Some(12);
    event.usage.output_tokens = Some(3);
    event.usage.total_tokens = Some(15);
    event.cost.estimated_api_equivalent_usd = Some(1);

    assert!(!store.insert_event(&event).expect("refresh duplicate"));
    assert_eq!(store.event_count().expect("count after refresh"), 1);
    assert_eq!(store.token_total().expect("tokens after refresh"), 15);

    let events = store.events().expect("events");
    assert_eq!(events[0].usage.input_tokens, Some(12));
    assert_eq!(events[0].cost.estimated_api_equivalent_usd, Some(1));
}

#[test]
fn store_strips_bare_project_identity_from_events_and_rollups() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-bare-project"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
        .single()
        .expect("now");
    let mut event = test_store_event(&source, now, "bare-project");
    event.project = Some(ProjectInfo {
        project_id: "project_bare".to_string(),
        project_label: Some("Bare".to_string()),
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: None,
        path_label: None,
    });

    assert!(store.insert_event(&event).expect("insert"));
    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].project, None);

    let rollups = store.dirty_sync_rollup_summaries().expect("rollups");
    assert_eq!(rollups.len(), 1);
    assert_eq!(rollups[0].project, None);
}

#[test]
fn insert_events_keeps_existing_reasoning_variants_distinct() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-batch"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut low = test_store_event(&source, now, "low-record");
    low.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });
    assert!(store.insert_event(&low).expect("insert low"));

    let mut high = low.clone();
    high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
    high.source.source_record_id = Some("high-record".to_string());
    high.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });

    assert_eq!(
        store.insert_events(&[high]).expect("insert batched high"),
        1
    );
    assert_eq!(store.event_count().expect("count"), 2);
}

#[test]
fn insert_events_preserves_existing_reasoning_on_less_enriched_duplicate() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-batch-preserve"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut enriched = test_store_event(&source, now, "enriched-record");
    enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });
    assert!(store.insert_event(&enriched).expect("insert enriched"));

    let mut less_enriched = enriched.clone();
    less_enriched.event_id = event_id("codex", &source.source_id, "less-enriched", None, now);
    less_enriched.source.source_record_id = Some("less-enriched".to_string());
    less_enriched.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });

    assert_eq!(
        store
            .insert_events(&[less_enriched])
            .expect("insert batched less-enriched duplicate"),
        0
    );

    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level),
        Some(ReasoningLevel::High)
    );
    assert_eq!(
        events[0]
            .model
            .as_ref()
            .and_then(|model| model.reasoning_level_raw.as_deref()),
        Some("high")
    );
}

#[test]
fn insert_events_with_resolution_returns_canonical_duplicate_event_ids() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-batch-resolution"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let existing = test_store_event(&source, now, "existing-record");
    let mut duplicate = existing.clone();
    duplicate.event_id = event_id("codex", &source.source_id, "duplicate-record", None, now);
    duplicate.source.source_record_id = Some("duplicate-record".to_string());
    duplicate.parse_evidence = None;

    assert!(store.insert_event(&existing).expect("insert existing"));
    let result = store
        .insert_events_with_resolution(&[duplicate.clone()])
        .expect("insert duplicate");

    assert_eq!(result.inserted, 0);
    assert_eq!(
        result.canonical_event_ids.get(&duplicate.event_id),
        Some(&existing.event_id)
    );
    assert_eq!(store.event_count().expect("count"), 1);
}

#[test]
fn insert_events_refreshes_preloaded_conflicts_before_matching_new_reasoning_variant() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reasoning-batch-refresh"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();

    let mut legacy = test_store_event(&source, now, "legacy-record");
    legacy.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: None,
        reasoning_level_raw: None,
    });
    assert!(store.insert_event(&legacy).expect("insert legacy"));

    let mut low = legacy.clone();
    low.event_id = event_id("codex", &source.source_id, "low-record", None, now);
    low.source.source_record_id = Some("low-record".to_string());
    low.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::Low),
        reasoning_level_raw: Some("low".to_string()),
    });

    let mut high = low.clone();
    high.event_id = event_id("codex", &source.source_id, "high-record", None, now);
    high.source.source_record_id = Some("high-record".to_string());
    high.model = Some(ModelInfo {
        name: Some("gpt-5.5".to_string()),
        normalized_name: Some("gpt-5.5".to_string()),
        provider_model_id: Some("gpt-5.5".to_string()),
        speed: None,
        reasoning_level: Some(ReasoningLevel::High),
        reasoning_level_raw: Some("high".to_string()),
    });

    assert_eq!(
        store
            .insert_events(&[low, high])
            .expect("insert batched variants"),
        1
    );

    let events = store.events().expect("events");
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|event| {
        event.model.as_ref().and_then(|model| model.reasoning_level) == Some(ReasoningLevel::Low)
    }));
    assert!(events.iter().any(|event| {
        event.model.as_ref().and_then(|model| model.reasoning_level) == Some(ReasoningLevel::High)
    }));
}

#[test]
fn insert_events_batches_in_one_transaction() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-batch"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let events = vec![
        test_store_event(&source, now, "record-a"),
        test_store_event(&source, now + chrono::Duration::seconds(1), "record-b"),
    ];

    assert_eq!(store.insert_events(&events).expect("batch"), 2);
    assert_eq!(store.insert_events(&events).expect("batch duplicate"), 0);
    assert_eq!(store.event_count().expect("count"), 2);
}

#[test]
fn usage_event_period_stats_since_counts_recent_events() {
    let store = Store::in_memory().expect("store");
    let source = statsai_core::SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-period-stats"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc::now();
    let recent = test_store_event(&source, now - chrono::Duration::minutes(5), "recent");
    let old = test_store_event(&source, now - chrono::Duration::days(2), "old");
    store.insert_events(&[recent, old]).expect("insert events");

    let stats = store
        .usage_event_period_stats_since(now - chrono::Duration::hours(1))
        .expect("period stats");

    assert_eq!(stats.requests, 1);
    assert_eq!(stats.tokens, 15);
}

#[test]
fn events_in_period_without_since_includes_pre_unix_history() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-pre-unix"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let pre_unix = Utc
        .with_ymd_and_hms(1969, 12, 31, 12, 0, 0)
        .single()
        .expect("pre-unix");
    let after = Utc
        .with_ymd_and_hms(1970, 1, 2, 12, 0, 0)
        .single()
        .expect("after");
    store
        .insert_events(&[
            test_store_event(&source, pre_unix, "pre-unix"),
            test_store_event(&source, after, "after"),
        ])
        .expect("insert events");
    let until = Utc
        .with_ymd_and_hms(1969, 12, 31, 23, 59, 59)
        .single()
        .expect("end of 1969");

    let unbounded = store
        .events_in_period(None, until)
        .expect("unbounded through 1969");
    assert_eq!(unbounded.len(), 1);
    assert_eq!(unbounded[0].session.started_at, pre_unix);

    let epoch_floor = store
        .events_in_period(Some(DateTime::<Utc>::UNIX_EPOCH), until)
        .expect("epoch floor");
    assert!(epoch_floor.is_empty());
}
