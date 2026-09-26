use super::*;
use statsai_core::{
    hash_text, task_title_is_generic, IdentitySource, RuntimeInfo, SessionTitleSource, TaskSpan,
    TaskSpanId, TASK_SPAN_SCHEMA_VERSION,
};

/// Gives the event a session and a project. Sessions with neither a project
/// nor a title are not kept, so tests of other behavior need one.
fn stamp_session(event: &mut UsageEvent, raw_id: &str) {
    let hash = hash_text(raw_id);
    event.session.session_id = format!("session_{}", &hash[..24]);
    event.session.local_session_id_hash = Some(hash);
    if event.project.is_none() {
        event.project = Some(session_test_project());
    }
}

fn session_test_project() -> ProjectInfo {
    ProjectInfo {
        project_id: "project-sessions".to_string(),
        project_label: None,
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
        path_hash: Some("path-sessions".to_string()),
        path_label: None,
    }
}

fn message_runtime(user: u64, assistant: u64) -> RuntimeInfo {
    RuntimeInfo {
        runtime_name: None,
        host_id: None,
        latency_ms: None,
        latency_source: None,
        time_to_first_token_ms: None,
        prompt_eval_duration_ms: None,
        eval_duration_ms: None,
        total_messages: Some(user + assistant),
        user_messages: Some(user),
        assistant_messages: Some(assistant),
        developer_messages: None,
    }
}

#[test]
fn session_rollup_sums_mixed_events() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-mixed"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 2, 9, 0, 0)
        .single()
        .expect("start");
    let later = start + chrono::Duration::minutes(12);

    let mut first = test_store_event(&source, start, "first");
    stamp_session(&mut first, "raw-session");
    first.session.ended_at = None;
    first.usage.requests = Some(2);
    first.usage.input_tokens = Some(10);
    first.usage.output_tokens = Some(4);
    first.usage.total_tokens = Some(14);
    first.model = Some(ModelInfo {
        normalized_name: Some("gpt-5".to_string()),
        ..ModelInfo::default()
    });
    first.runtime = Some(message_runtime(1, 1));
    first.cost.estimated_api_equivalent_micro_usd = Some(20_000);
    first.created_at = start;

    let mut second = test_store_event(&source, later, "second");
    stamp_session(&mut second, "raw-session");
    second.session.started_at = start;
    second.session.ended_at = Some(later);
    second.usage.requests = None;
    second.usage.input_tokens = Some(5);
    second.usage.output_tokens = Some(7);
    second.usage.total_tokens = Some(12);
    second.model = Some(ModelInfo {
        normalized_name: Some("gpt-5-mini".to_string()),
        ..ModelInfo::default()
    });
    second.usage.input_tokens = Some(30);
    second.usage.output_tokens = Some(1);
    second.usage.total_tokens = Some(31);
    second.runtime = Some(message_runtime(2, 1));
    second.cost.estimated_api_equivalent_micro_usd = Some(5_000);
    second.created_at = later;

    assert!(store.insert_event(&first).expect("insert first"));
    assert!(store.insert_event(&second).expect("insert second"));

    let rollups = store
        .session_rollups_in_period(
            None,
            later + chrono::Duration::days(1),
            &SessionFilter::default(),
            SessionSort::Started,
            10,
            0,
        )
        .expect("rollups");
    assert_eq!(rollups.len(), 1);
    let rollup = &rollups[0];
    assert_eq!(rollup.schema_version, "session_rollup.v1");
    assert_eq!(rollup.session_id, first.session.session_id);
    assert_eq!(rollup.usage.input_tokens, Some(40));
    assert_eq!(rollup.usage.output_tokens, Some(5));
    assert_eq!(rollup.usage.computed_total(), 45);
    assert_eq!(rollup.requests, 3);
    assert_eq!(rollup.cost.estimated_micro_usd(), Some(25_000));
    assert_eq!(rollup.user_messages, Some(3));
    assert_eq!(rollup.assistant_messages, Some(2));
    assert_eq!(rollup.total_messages, Some(5));
    assert_eq!(rollup.primary_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(rollup.started_at, start);
    assert_eq!(rollup.ended_at, later);
    assert_eq!(rollup.duration_seconds, Some(12 * 60));
    assert_eq!(rollup.models.len(), 2);
}

#[test]
fn session_rollup_orders_inverted_event_times() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-inverted"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 2, 9, 0, 0)
        .single()
        .expect("start");
    let later = start + chrono::Duration::minutes(12);
    let mut event = test_store_event(&source, later, "inverted");
    stamp_session(&mut event, "raw-inverted");
    event.session.started_at = later;
    event.session.ended_at = Some(start);
    event.created_at = start;
    assert!(store.insert_event(&event).expect("insert"));

    let rollups = store
        .session_rollups_in_period(
            None,
            later + chrono::Duration::days(1),
            &SessionFilter::default(),
            SessionSort::Started,
            10,
            0,
        )
        .expect("rollups");
    assert_eq!(rollups.len(), 1);
    assert_eq!(rollups[0].started_at, later);
    assert_eq!(rollups[0].ended_at, later);
    assert_eq!(rollups[0].duration_seconds, Some(0));
}

#[test]
fn session_rollup_refreshes_on_insert_and_delete() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-refresh"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 3, 9, 0, 0)
        .single()
        .expect("start");
    let mut kept = test_store_event(&source, start, "kept");
    stamp_session(&mut kept, "refresh-session");
    kept.usage.total_tokens = Some(10);
    kept.parse_evidence = Some(ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: Some("file-kept".to_string()),
        source_line_number: None,
        source_record_id: None,
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    let mut dropped = test_store_event(&source, start + chrono::Duration::minutes(1), "dropped");
    stamp_session(&mut dropped, "refresh-session");
    dropped.session.started_at = start;
    dropped.usage.total_tokens = Some(7);
    dropped.parse_evidence = Some(ParseEvidence {
        event_key_version: "test".to_string(),
        source_file_path_hash: Some("file-dropped".to_string()),
        source_line_number: None,
        source_record_id: None,
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    store.insert_event(&kept).expect("kept");
    store.insert_event(&dropped).expect("dropped");
    let before = store.dirty_session_rollups().expect("dirty");
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].usage.computed_total(), 17);

    let impact = store
        .delete_events_for_source_file_hashes(&source.source_id, &["file-dropped".to_string()])
        .expect("delete");
    assert_eq!(impact.deleted, 1);
    let after = store.dirty_session_rollups().expect("after");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].usage.computed_total(), 10);

    store
        .delete_events_for_sources(std::slice::from_ref(&source.source_id))
        .expect("delete source");
    assert!(store.dirty_session_rollups().expect("gone").is_empty());
}

#[test]
fn session_titles_prefer_event_then_task_span_then_archive() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-titles"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 4, 9, 0, 0)
        .single()
        .expect("start");
    let raw_id = "provider-session-9";
    let mut event = test_store_event(&source, start, "titled");
    stamp_session(&mut event, raw_id);
    store.insert_event(&event).expect("insert");

    let generic = "hello";
    assert!(task_title_is_generic(Some(generic)));
    store
        .upsert_task_spans(&[task_span(&source, raw_id, generic, start)])
        .expect("generic span");
    let unlabeled = store.dirty_session_rollups().expect("unlabeled");
    assert_eq!(unlabeled[0].title, None);

    let title = "Repair the session indexer";
    assert!(!task_title_is_generic(Some(title)));
    store
        .upsert_task_spans(&[task_span(&source, raw_id, title, start)])
        .expect("span");
    let from_span = store.dirty_session_rollups().expect("span title");
    assert_eq!(from_span[0].title.as_deref(), Some(title));
    assert_eq!(
        from_span[0].title_source,
        Some(SessionTitleSource::TaskSpan)
    );

    event.session.title = Some("Event title wins".to_string());
    store.insert_event(&event).expect("retitle");
    let from_event = store.dirty_session_rollups().expect("event title");
    assert_eq!(from_event[0].title.as_deref(), Some("Event title wins"));
    assert_eq!(from_event[0].title_source, Some(SessionTitleSource::Event));
}

#[test]
fn session_queries_filter_period_and_sort() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/session-query"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let early = Utc
        .with_ymd_and_hms(2026, 6, 1, 8, 0, 0)
        .single()
        .expect("early");
    let late = Utc
        .with_ymd_and_hms(2026, 6, 10, 8, 0, 0)
        .single()
        .expect("late");
    let mut small = test_store_event(&source, early, "small");
    stamp_session(&mut small, "small-session");
    small.provider = "claude_code".to_string();
    small.usage.total_tokens = Some(5);
    small.session.ended_at = Some(early + chrono::Duration::seconds(30));
    let mut large = test_store_event(&source, late, "large");
    stamp_session(&mut large, "large-session");
    large.provider = "claude_code".to_string();
    large.usage.total_tokens = Some(50);
    large.session.ended_at = Some(late + chrono::Duration::seconds(90));
    store.insert_event(&small).expect("small");
    store.insert_event(&large).expect("large");

    let filter = SessionFilter {
        provider: Some("claude_code".to_string()),
        ..SessionFilter::default()
    };
    let page = store
        .session_rollups_in_period(
            Some(Utc.with_ymd_and_hms(2026, 6, 5, 0, 0, 0).unwrap()),
            late + chrono::Duration::days(1),
            &filter,
            SessionSort::Tokens,
            10,
            0,
        )
        .expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].usage.computed_total(), 50);

    let sorted = store
        .session_rollups_in_period(None, late, &filter, SessionSort::Tokens, 10, 0)
        .expect("sorted");
    assert_eq!(sorted.len(), 2);
    assert_eq!(sorted[0].usage.computed_total(), 50);
    assert_eq!(sorted[1].usage.computed_total(), 5);
    let stats = store
        .session_stats_in_period(None, late, &filter)
        .expect("stats");
    assert_eq!(stats.sessions, 2);
    assert_eq!(stats.total_tokens, 55);
    assert_eq!(stats.avg_tokens, Some(27));
    assert_eq!(stats.median_tokens, Some(27));
    assert_eq!(stats.median_duration_seconds, Some(60));
}

fn task_span(
    source: &statsai_core::SourceLocation,
    raw_session_id: &str,
    title: &str,
    started_at: chrono::DateTime<Utc>,
) -> TaskSpan {
    TaskSpan {
        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
        span_id: TaskSpanId(format!("span-{raw_session_id}-{title}")),
        provider: source.provider.clone(),
        source_id: source.source_id.clone(),
        span_kind: "test".to_string(),
        source_record_id: None,
        source_file_path_hash: None,
        summary_id: None,
        session_id: Some(raw_session_id.to_string()),
        thread_id: None,
        title: title.to_string(),
        normalized_title: statsai_core::normalize_task_title(title),
        title_source: Some("test".to_string()),
        summary_preview: None,
        todo_excerpt: None,
        issue_keys: Vec::new(),
        branch_family: None,
        project_bucket: "bucket".to_string(),
        project: None,
        git: None,
        usage: UsageCounts::default(),
        estimated_cost_usd: None,
        estimated_cost_micro_usd: None,
        event_count: 0,
        has_usage_evidence: false,
        total_messages: 0,
        user_messages: 0,
        assistant_messages: 0,
        developer_messages: 0,
        linked_event_ids: Vec::new(),
        confidence: Confidence::Medium,
        is_meta: task_title_is_generic(Some(title)),
        started_at,
        ended_at: Some(started_at),
        duration_seconds: Some(0),
    }
}

fn record_evidence(record_id: &str) -> ParseEvidence {
    ParseEvidence {
        event_key_version: "provider_record_usage_event.v1".to_string(),
        source_file_path_hash: None,
        source_line_number: None,
        source_record_id: Some(record_id.to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    }
}

#[test]
fn record_keyed_events_do_not_form_sessions() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "cursor",
        "test",
        "0",
        Path::new("/tmp/session-cursor"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 5, 9, 0, 0)
        .single()
        .expect("start");

    // A local Cursor row is keyed by its own record, so its session hash is
    // the record hash.
    let mut row = test_store_event(&source, start, "cursor-row");
    stamp_session(&mut row, "cursor-row-identity");
    let row_hash = hash_text("cursor-row-identity");
    row.parse_evidence = Some(record_evidence(&format!(
        "provider_record_usage_event.v1:cursor_usage_event:{row_hash}"
    )));
    store.insert_event(&row).expect("row");

    // A cloud agent groups its rows under one agent id.
    let mut agent = test_store_event(&source, start, "cursor-agent-row");
    stamp_session(&mut agent, "cursor_agent:agent-1");
    let record_hash = hash_text("agent-row-identity");
    agent.parse_evidence = Some(record_evidence(&format!(
        "provider_record_usage_event.v1:cursor_usage_event:{record_hash}"
    )));
    store.insert_event(&agent).expect("agent");

    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, agent.session.session_id);

    assert_eq!(store.rebuild_session_rollups().expect("rebuild"), 1);
    let rebuilt = store.dirty_session_rollups().expect("rebuilt");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].session_id, agent.session.session_id);
}

#[test]
fn single_instant_sessions_have_unknown_duration() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-instant"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 6, 9, 0, 0)
        .single()
        .expect("start");
    let mut lone = test_store_event(&source, start, "lone");
    stamp_session(&mut lone, "lone-session");
    lone.created_at = start;
    store.insert_event(&lone).expect("lone");

    let mut spanned = test_store_event(&source, start, "spanned-first");
    stamp_session(&mut spanned, "spanned-session");
    spanned.created_at = start;
    store.insert_event(&spanned).expect("first");
    let mut later = test_store_event(
        &source,
        start + chrono::Duration::minutes(3),
        "spanned-last",
    );
    stamp_session(&mut later, "spanned-session");
    later.created_at = start + chrono::Duration::minutes(3);
    store.insert_event(&later).expect("last");

    let sessions = store.dirty_session_rollups().expect("sessions");
    let duration = |session_id: &str| {
        sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .expect("session")
            .duration_seconds
    };
    assert_eq!(duration(&lone.session.session_id), None);
    assert_eq!(duration(&spanned.session.session_id), Some(180));
    let stats = store
        .session_stats_in_period(
            None,
            start + chrono::Duration::days(1),
            &SessionFilter::default(),
        )
        .expect("stats");
    assert_eq!(stats.median_duration_seconds, Some(180));
}

#[test]
fn codex_rollout_stem_span_titles_join_uuid_sessions() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-codex-stem"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 7, 9, 0, 0)
        .single()
        .expect("start");
    let uuid = "019c537d-7d4b-7c33-bfc9-785a18e58f8c";
    let mut event = test_store_event(&source, start, "codex-stem");
    stamp_session(&mut event, uuid);
    store.insert_event(&event).expect("insert");

    let title = "Repair the rollout title join";
    store
        .upsert_task_spans(&[task_span(
            &source,
            &format!("2026/02/12/rollout-2026-02-12T23-14-18-{uuid}"),
            title,
            start,
        )])
        .expect("span");
    store.rebuild_session_rollups().expect("rebuild");
    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions[0].title.as_deref(), Some(title));
}

#[test]
fn rebuild_resends_unchanged_sessions_without_restamping() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-rebuild"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 8, 9, 0, 0)
        .single()
        .expect("start");
    let mut event = test_store_event(&source, start, "rebuild");
    stamp_session(&mut event, "rebuild-session");
    store.insert_event(&event).expect("insert");
    let ids = store
        .dirty_session_rollups()
        .expect("dirty")
        .into_iter()
        .map(|session| session.session_id)
        .collect::<Vec<_>>();
    let before = store.dirty_session_rollups().expect("before");
    store.mark_session_rollups_synced(&ids).expect("synced");

    store.rebuild_session_rollups().expect("rebuild");
    let resent = store.dirty_session_rollups().expect("resent");
    assert_eq!(resent, before);
}

#[test]
fn provider_named_sessions_keep_plain_names_but_drop_placeholders() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-provider-titles"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 9, 9, 0, 0)
        .single()
        .expect("start");
    let plain = "Review uncommitted changes";
    assert!(task_title_is_generic(Some(plain)));
    let mut review = test_store_event(&source, start, "review");
    stamp_session(&mut review, "review-session");
    review.session.title = Some(plain.to_string());
    store.insert_event(&review).expect("review");

    let mut placeholder = test_store_event(&source, start, "placeholder");
    stamp_session(&mut placeholder, "placeholder-session");
    placeholder.session.title = Some("New session - 2026-04-30T16:41:41.413Z".to_string());
    store.insert_event(&placeholder).expect("placeholder");

    let mut bare = test_store_event(&source, start, "bare-placeholder");
    stamp_session(&mut bare, "bare-placeholder-session");
    bare.session.title = Some("New session".to_string());
    store.insert_event(&bare).expect("bare placeholder");

    let sessions = store.dirty_session_rollups().expect("sessions");
    let title = |event: &UsageEvent| {
        sessions
            .iter()
            .find(|session| session.session_id == event.session.session_id)
            .expect("session")
            .title
            .clone()
    };
    assert_eq!(title(&review).as_deref(), Some(plain));
    assert_eq!(title(&placeholder), None);
    assert_eq!(title(&bare), None);
}

#[test]
fn session_backfill_resends_every_session_to_each_target() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-backfill-resend"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 10, 9, 0, 0)
        .single()
        .expect("start");
    let mut event = test_store_event(&source, start, "backfill");
    stamp_session(&mut event, "backfill-session");
    store.insert_event(&event).expect("insert");
    let sessions = store.all_session_rollups().expect("sessions");
    store
        .record_session_rollups_synced("http", "https://api.example.test", &sessions)
        .expect("record");
    assert!(store
        .pending_session_rollups_for_sync("http", "https://api.example.test", &sessions)
        .expect("pending")
        .is_empty());

    store
        .conn
        .execute(
            "DELETE FROM local_metadata WHERE key LIKE 'session_rollups_backfilled_%'",
            [],
        )
        .expect("forget backfill");
    store
        .backfill_session_rollups_if_needed()
        .expect("backfill");

    let pending = store
        .pending_session_rollups_for_sync("http", "https://api.example.test", &sessions)
        .expect("pending after backfill");
    assert_eq!(pending, sessions);

    // The acknowledgement survives, so a session gone from the next snapshot
    // is still retired on the target.
    let snapshot = statsai_core::SyncAuthoritativeSnapshot {
        session_rollup_ids: Some(Vec::new()),
        ..statsai_core::SyncAuthoritativeSnapshot::default()
    };
    assert!(store
        .sync_target_has_retired_entities("http", "https://api.example.test", &snapshot)
        .expect("retired"));
}

#[test]
fn sessions_without_project_or_title_are_not_kept() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "cursor",
        "test",
        "0",
        Path::new("/tmp/session-unidentifiable"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 11, 9, 0, 0)
        .single()
        .expect("start");
    let mut agent = test_store_event(&source, start, "agent");
    stamp_session(&mut agent, "cursor_agent:agent-2");
    agent.project = None;
    store.insert_event(&agent).expect("agent");
    assert!(store.dirty_session_rollups().expect("sessions").is_empty());

    agent.session.title = Some("Upgrade the importer".to_string());
    store.insert_event(&agent).expect("titled agent");
    assert_eq!(store.dirty_session_rollups().expect("titled").len(), 1);

    assert_eq!(store.rebuild_session_rollups().expect("rebuild"), 1);
}

#[test]
fn active_time_counts_turns_and_skips_idle_gaps() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/session-active"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let at = |hour: u32, minute: u32| {
        Utc.with_ymd_and_hms(2026, 6, 12, hour, minute, 0)
            .single()
            .expect("time")
    };
    // Two turns of a Claude-style session, the second resumed hours later:
    // each event carries the prompt that started its turn.
    for (record, prompt, message) in [
        ("first-a", at(10, 0), at(10, 1)),
        ("first-b", at(10, 0), at(10, 5)),
        ("second", at(14, 0), at(14, 2)),
    ] {
        let mut event = test_store_event(&source, message, record);
        stamp_session(&mut event, "active-session");
        event.created_at = message;
        event.session.turn_started_at = Some(prompt);
        store.insert_event(&event).expect("insert");
    }

    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].started_at, at(10, 0));
    // Wall-clock span keeps the idle afternoon; active time does not.
    assert_eq!(sessions[0].duration_seconds, Some(4 * 3600 + 2 * 60));
    assert_eq!(sessions[0].active_seconds, Some(7 * 60));
}

#[test]
fn active_time_merges_overlapping_turn_intervals() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-active-overlap"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let at = |minute: u32| {
        Utc.with_ymd_and_hms(2026, 6, 13, 9, minute, 0)
            .single()
            .expect("time")
    };
    // Codex-style turns carry their own start and completion. A record
    // without an interval adds nothing.
    for (record, start, end) in [
        ("turn-a", at(0), Some(at(10))),
        ("turn-b", at(5), Some(at(12))),
        ("turn-c", at(30), Some(at(31))),
        ("record", at(40), None),
    ] {
        let mut event = test_store_event(&source, start, record);
        stamp_session(&mut event, "overlap-session");
        event.created_at = end.unwrap_or(start);
        event.session.ended_at = end;
        store.insert_event(&event).expect("insert");
    }

    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions[0].active_seconds, Some(13 * 60));
    assert_eq!(sessions[0].duration_seconds, Some(40 * 60));
}

#[test]
fn session_aggregate_events_do_not_count_as_active_time() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "opencode",
        "test",
        "0",
        Path::new("/tmp/session-aggregate"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 6, 14, 9, 0, 0)
        .single()
        .expect("start");
    let end = start + chrono::Duration::days(3);
    let mut aggregate = test_store_event(&source, end, "aggregate");
    stamp_session(&mut aggregate, "aggregate-session");
    aggregate.source.source_type = "sqlite:session".to_string();
    aggregate.session.started_at = start;
    aggregate.session.ended_at = Some(end);
    aggregate.created_at = end;
    store.insert_event(&aggregate).expect("insert");

    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions[0].duration_seconds, Some(3 * 86_400));
    assert_eq!(sessions[0].active_seconds, None);
}

#[test]
fn reported_turn_duration_bounds_a_late_completion() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/session-late-completion"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let start = Utc
        .with_ymd_and_hms(2026, 7, 23, 17, 16, 0)
        .single()
        .expect("start");
    // Codex reported 21 minutes of work but wrote the completion when the
    // thread resumed four days later.
    let completed = start + chrono::Duration::days(4);
    let mut turn = test_store_event(&source, start, "late-turn");
    stamp_session(&mut turn, "late-session");
    turn.session.ended_at = Some(completed);
    turn.session.duration_seconds = Some(21 * 60);
    turn.created_at = completed;
    store.insert_event(&turn).expect("insert");

    let sessions = store.dirty_session_rollups().expect("sessions");
    assert_eq!(sessions[0].active_seconds, Some(21 * 60));
    assert_eq!(sessions[0].duration_seconds, Some(4 * 86_400));
}
