use super::*;

#[test]
fn http_sync_uses_configured_or_default_api_endpoint() {
    let previous = std::env::var("STATSAI_API_URL").ok();
    std::env::set_var("STATSAI_API_URL", "https://sync.example.com");
    let endpoint = http_sync_endpoint(&test_sync_command("http")).expect("http endpoint");
    if let Some(value) = previous {
        std::env::set_var("STATSAI_API_URL", value);
    } else {
        std::env::remove_var("STATSAI_API_URL");
    }

    assert_eq!(endpoint, "https://sync.example.com/api/sync/batches");
}

#[test]
fn http_rollup_sync_retries_smaller_batches_after_budget_rejection() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-retry"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..4)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days(index as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
            summary.metadata.summary_format = "daily_rollup.v1".to_string();
            sanitize_summary_for_sync(summary)
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_retry".to_string(),
        device_id: "device".to_string(),
        sources: vec![],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries,
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);

    send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
            observed_for_send
                .lock()
                .expect("observed lock")
                .push((chunk.batch_id.clone(), chunk.summaries.len()));
            if chunk.summaries.len() > 2 {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 413: {"error":"sync_batch_d1_query_budget_exceeded","estimatedQueries":53,"maxQueries":45}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
        })
        .expect("send");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            ("batch_retry".to_string(), 4),
            ("batch_retry_part_1_of_2".to_string(), 2),
            ("batch_retry_part_2_of_2".to_string(), 2),
        ]
    );
    let state = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, "batch_retry");
    let pending = store
        .pending_summaries_for_sync(
            "http",
            &endpoint,
            &batch
                .summaries
                .iter()
                .cloned()
                .map(sanitize_summary_for_sync)
                .collect::<Vec<_>>(),
        )
        .expect("pending summaries");
    assert!(pending.is_empty());
}

#[test]
fn http_rollup_sync_retries_smaller_batches_after_payload_too_large() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-too-large"),
        LocationOrigin::Configured,
    );
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let summaries: Vec<_> = (0..4)
        .map(|index| {
            let mut summary = test_summary(
                "codex",
                &source,
                now + Duration::days(index as i64),
                10,
                None,
            );
            summary.summary_id = statsai_core::SummaryId(format!("summary-too-large-{index}"));
            summary.metadata.summary_format = "daily_rollup.v1".to_string();
            summary
        })
        .collect();
    let batch = SyncBatch {
        schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
        batch_id: "batch_too_large".to_string(),
        device_id: "device".to_string(),
        sources: vec![],
        accounts: vec![],
        source_account_assignments: vec![],
        subscriptions: vec![],
        account_plan_observations: vec![],
        account_evidence_summaries: vec![],
        events: vec![],
        summaries,
        task_buckets: vec![],
        task_verifications: vec![],
        code_change_metrics: vec![],
        quota_cycle_contributions: vec![],
        authoritative_snapshot: None,
        created_at: now,
    };
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);

    send_http_rollup_chunk_with_retry_using(&batch, &|chunk| {
        observed_for_send
            .lock()
            .expect("observed lock")
            .push((chunk.batch_id.clone(), chunk.summaries.len()));
        if chunk.summaries.len() > 2 {
            return Err(anyhow::Error::msg(
                r#"sync endpoint returned HTTP 413: {"error":"sync_batch_too_large"}"#,
            ));
        }
        record_rollup_sync_chunk_success(&store, "http", &endpoint, &logical_batch_id, chunk)
    })
    .expect("send");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            ("batch_too_large".to_string(), 4),
            ("batch_too_large_part_1_of_2".to_string(), 2),
            ("batch_too_large_part_2_of_2".to_string(), 2),
        ]
    );
    let state = store
        .sync_state("http", &endpoint)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, batch.batch_id);
}

#[test]
fn http_rollup_sync_restarts_full_snapshot_after_snapshot_failure() {
    let store = Store::in_memory().expect("store");
    let endpoint = "https://api.example.com/api/sync/batches".to_string();
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-http-rollup-resume"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let account_id = provider_account_id("codex", "personal");
    for index in 0..26 {
        let event = test_event(
            "codex",
            &source,
            now + Duration::days(index as i64),
            Some(account_id.clone()),
            TokenParts::total(10),
        );
        store.insert_event(&event).expect("event");
    }
    store.rebuild_sync_rollups().expect("rebuild");

    let command = SyncCommand {
        endpoint: Some(endpoint.clone()),
        ..test_sync_command("http")
    };
    let target = sync_target(&command).expect("target");
    let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
    assert_eq!(mode, SyncPayloadMode::Rollups);
    assert_eq!(batch.sources.len(), 1);
    assert_eq!(batch.summaries.len(), 26);
    let logical_batch_id = logical_http_rollup_batch_id(&batch.batch_id).to_string();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_send = Arc::clone(&observed);
    let mut observed_error = None;

    for chunk in split_http_rollup_sync_batches(&batch) {
        let result = send_http_rollup_chunk_with_retry_using(&chunk, &|chunk| {
            observed_for_send.lock().expect("observed lock").push((
                chunk.batch_id.clone(),
                chunk.sources.len(),
                chunk.summaries.len(),
                chunk.authoritative_snapshot.is_some(),
            ));
            if chunk.authoritative_snapshot.is_some() {
                return Err(anyhow::Error::msg(
                    r#"sync endpoint returned HTTP 429: {"error":"rate_limited","retryAfterSeconds":60}"#,
                ));
            }
            record_rollup_sync_chunk_success(&store, "http", &target, &logical_batch_id, chunk)
        });
        if let Err(send_error) = result {
            observed_error = Some(send_error);
            break;
        }
    }
    let error = observed_error.expect("rate limit should stop the snapshot request");
    assert!(error.to_string().contains("HTTP 429"));
    store
        .record_sync_failure("http", &target)
        .expect("record sync failure");

    let observed = observed.lock().expect("observed lock").clone();
    assert_eq!(
        observed,
        vec![
            (format!("{}_sources_1", batch.batch_id), 1, 0, false),
            (format!("{}_part_1_of_2", batch.batch_id), 0, 25, false),
            (format!("{}_part_2_of_2", batch.batch_id), 0, 1, false),
            (format!("{}_snapshot_1", batch.batch_id), 0, 0, true),
        ]
    );

    let sync_sources: Vec<_> = store
        .list_sources()
        .expect("sources")
        .into_iter()
        .map(sanitize_source_for_sync)
        .collect();
    assert!(store
        .pending_sources_for_sync("http", &target, &sync_sources)
        .expect("pending sources")
        .is_empty());

    let sync_rollups: Vec<_> = store
        .all_sync_rollup_summaries()
        .expect("rollups")
        .into_iter()
        .map(sanitize_summary_for_sync)
        .collect();
    let pending_rollups = store
        .pending_summaries_for_sync("http", &target, &sync_rollups)
        .expect("pending rollups");
    assert!(pending_rollups.is_empty());
    let state = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(state.last_batch_id, batch.batch_id);

    let (resume_batch, resume_mode) =
        build_sync_batch(&command, &store, "device", &target).expect("resume batch");
    assert_eq!(resume_mode, SyncPayloadMode::Rollups);
    assert!(resume_batch.sources.is_empty());
    assert_eq!(resume_batch.summaries.len(), 26);
    assert!(resume_batch.authoritative_snapshot.is_some());
    let state_after_build = store
        .sync_state("http", &target)
        .expect("sync state")
        .expect("present");
    assert_eq!(
        state_after_build.pending_resume_batch_id, state.pending_resume_batch_id,
        "building the replacement snapshot must not clear resume state"
    );

    let since_last_command = SyncCommand {
        endpoint: Some(endpoint),
        since_last: true,
        ..test_sync_command("http")
    };
    let (since_last_resume, _) = build_sync_batch(&since_last_command, &store, "device", &target)
        .expect("since-last resume batch");
    assert_eq!(since_last_resume.summaries.len(), 26);
    assert!(since_last_resume.authoritative_snapshot.is_some());
}

#[test]
fn http_rollup_chunk_is_resent_after_a_transient_endpoint_failure() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    // A restarted worker answers with a plain-text body, so the decision to
    // resend cannot depend on parsing an error code out of JSON.
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        if attempts.get() == 1 {
            Err(anyhow::anyhow!(
                "sync endpoint returned HTTP 503: Your worker restarted mid-request. \
                     Please try sending the request again. Only GET or HEAD requests are \
                     retried automatically."
            ))
        } else {
            Ok(())
        }
    };

    let delays = std::cell::RefCell::new(Vec::new());
    send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|delay| {
        delays.borrow_mut().push(delay)
    })
    .expect("transient failure is resent rather than aborting the run");
    assert_eq!(attempts.get(), 2);
    assert_eq!(delays.into_inner(), vec![StdDuration::from_secs(1)]);
}

#[test]
fn http_rollup_chunk_stops_resending_a_transient_failure_that_never_clears() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        Err(anyhow::anyhow!(
            "sync endpoint returned HTTP 502: Bad gateway"
        ))
    };

    let delays = std::cell::RefCell::new(Vec::new());
    let error = send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|delay| {
        delays.borrow_mut().push(delay)
    })
    .expect_err("an endpoint that never recovers still fails the run");

    // The original failure is reported rather than a retry-shaped summary of
    // it, and the run gives up instead of resending forever.
    assert!(error.to_string().contains("502"));
    assert_eq!(attempts.get(), 4);
    // Each attempt waits twice as long as the one before it.
    assert_eq!(
        delays.into_inner(),
        vec![
            StdDuration::from_secs(1),
            StdDuration::from_secs(2),
            StdDuration::from_secs(4),
        ]
    );
}

#[test]
fn http_rollup_chunk_does_not_resend_a_decided_rejection() {
    let now = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("date");
    let batch = test_task_only_sync_batch(now, 1, 1);
    let attempts = std::cell::Cell::new(0_usize);
    // The endpoint decided about this batch. Sending it again could only be
    // rejected the same way, and a conflict repeated on a schedule is worse
    // than one reported immediately.
    let send = |_: &SyncBatch| -> Result<()> {
        attempts.set(attempts.get() + 1);
        Err(anyhow::anyhow!(
            r#"sync endpoint returned HTTP 409: {{"error":"batch_id_payload_conflict"}}"#
        ))
    };

    let error = send_http_rollup_chunk_with_retry_using_sleep(&batch, &send, &|_| {
        panic!("a decided rejection must not wait to be resent")
    })
    .expect_err("conflict is reported");

    assert!(error.to_string().contains("batch_id_payload_conflict"));
    assert_eq!(attempts.get(), 1);
}

#[test]
fn http_rollup_rate_limit_is_left_to_the_endpoints_own_retry_after() {
    // 429 carries a `Retry-After` this backoff cannot read, so resending on
    // our own schedule would ignore the delay the endpoint asked for.
    assert!(!is_transient_http_sync_error(&anyhow::anyhow!(
        r#"sync endpoint returned HTTP 429: {{"error":"sync_write_user"}}"#
    )));
    assert!(is_transient_http_sync_error(&anyhow::anyhow!(
        "sync endpoint returned HTTP 503: Your worker restarted mid-request."
    )));
    // A body that is not JSON at all must still yield its status.
    assert_eq!(
        http_sync_error_status(&anyhow::anyhow!(
            "sync endpoint returned HTTP 504: Gateway timeout"
        )),
        Some(504)
    );
    // Anything that is not a sync endpoint failure has no status to read.
    assert_eq!(
        http_sync_error_status(&anyhow::anyhow!("connection reset by peer")),
        None
    );
}

#[test]
fn custom_http_sinks_skip_task_verification_feed_derivation() {
    assert_eq!(
        http_task_verification_feed_url("https://example.com/custom-sync"),
        None
    );
    assert_eq!(
        http_task_verification_feed_url("https://api.example.com/api/sync/batches"),
        Some("https://api.example.com/api/task-sync/verifications".to_string())
    );
}

#[test]
fn optional_task_verification_feed_statuses_do_not_fail_sync() {
    assert!(optional_task_verification_feed_status(404));
    assert!(optional_task_verification_feed_status(405));
    assert!(optional_task_verification_feed_status(501));
    assert!(!optional_task_verification_feed_status(400));
    assert!(!optional_task_verification_feed_status(429));
    assert!(!optional_task_verification_feed_status(500));
}

#[test]
fn logical_http_rollup_batch_id_strips_known_chunk_suffixes() {
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_11_of_11"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_11_of_11_part_1_of_2"),
        "batch_1"
    );
    assert_eq!(logical_http_rollup_batch_id("batch_1_sources_1"), "batch_1");
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_3_of_9_sources_1"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_subscriptions_2"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_task_buckets_2"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_3_of_9_task_verifications_4"),
        "batch_1"
    );
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_code_changes_3"),
        "batch_1"
    );
    assert_eq!(logical_http_rollup_batch_id("batch_1"), "batch_1");
    assert_eq!(
        logical_http_rollup_batch_id("batch_1_part_final"),
        "batch_1_part_final"
    );
}

#[test]
fn http_verify_status_url_points_at_worker_status_endpoint() {
    assert_eq!(
        http_verify_status_url("https://api.example.com/api/sync/batches").expect("status"),
        "https://api.example.com/api/sync/status"
    );
}

#[test]
fn http_preflight_status_url_points_at_lightweight_worker_status_endpoint() {
    assert_eq!(
        http_preflight_status_url("https://api.example.com/api/sync/batches").expect("status"),
        "https://api.example.com/api/sync/status?view=preflight"
    );
}

#[test]
fn only_the_configured_hosted_endpoint_requires_a_device_login() {
    let hosted = "https://api.example.com/api/sync/batches";
    assert!(http_endpoint_requires_authentication(hosted, hosted));
    assert!(http_endpoint_requires_authentication(
        "https://api.example.com/api/sync/batches/",
        hosted
    ));
    // A self-hosted deployment serves the same route and may accept
    // unauthenticated batches, so the path shape must not imply a login.
    assert!(!http_endpoint_requires_authentication(
        "https://sync.example.com/api/sync/batches",
        hosted
    ));
    assert!(!http_endpoint_requires_authentication(
        "https://sync.example.com/custom/batch-ingest",
        hosted
    ));
}

#[test]
fn custom_http_endpoint_skips_optional_remote_preflight() {
    let command = SyncCommand {
        auth_token: Some("token".to_string()),
        ..test_sync_command("http")
    };

    let preflight =
        load_http_sync_preflight(&command, "https://sync.example.com/custom/batch-ingest")
            .expect("custom endpoint preflight");

    assert_eq!(preflight.auth_token.as_deref(), Some("token"));
    assert!(preflight.remote.is_none());
}

#[test]
fn remote_hosted_tasks_enabled_defaults_true_when_capability_missing() {
    assert!(remote_hosted_tasks_enabled(&json!({
        "device": {
            "last_sync_batch_id": "batch-1"
        }
    })));
}

#[test]
fn remote_hosted_tasks_enabled_reads_explicit_false_capability() {
    assert!(!remote_hosted_tasks_enabled(&json!({
        "capabilities": {
            "hostedTasks": false
        }
    })));
}

#[test]
fn remote_code_change_identity_key_reads_account_scoped_blinding_key() {
    let encoded = "ab".repeat(32);
    assert_eq!(
        remote_code_change_identity_key(&json!({
            "capabilities": {
                "codeChangeIdentityKey": encoded
            }
        }))
        .expect("identity key"),
        Some([0xab; 32])
    );
    assert_eq!(
        remote_code_change_identity_key(&json!({ "capabilities": {} }))
            .expect("missing identity key"),
        None
    );
}

#[test]
fn remote_code_change_identity_key_rejects_malformed_keys() {
    for value in [json!("not-hex"), json!("ab"), json!(42)] {
        assert!(remote_code_change_identity_key(&json!({
            "capabilities": {
                "codeChangeIdentityKey": value
            }
        }))
        .is_err());
    }
}

#[test]
fn optional_http_sync_preflight_statuses_do_not_disable_task_sync() {
    assert!(optional_http_sync_preflight_status(404));
    assert!(optional_http_sync_preflight_status(405));
    assert!(optional_http_sync_preflight_status(501));
    assert!(!optional_http_sync_preflight_status(400));
    assert!(!optional_http_sync_preflight_status(500));
}

#[test]
fn http_reset_url_points_at_worker_reset_endpoint() {
    assert_eq!(
        http_reset_url("https://api.example.com/api/sync/batches").expect("reset"),
        "https://api.example.com/api/sync/reset"
    );
}

#[test]
fn credentialed_http_helpers_reject_remote_plaintext_before_request() {
    let endpoint = "http://api.example.com/api/sync/batches";

    for result in [
        http_remote_verify(endpoint, "token"),
        http_remote_reset(endpoint, "token"),
    ] {
        let error = result.expect_err("remote plaintext must fail");
        assert!(error.to_string().contains("requires HTTPS"));
    }

    let command = SyncCommand {
        auth_token: Some("token".to_string()),
        ..test_sync_command("http")
    };
    let error = load_http_sync_preflight(&command, endpoint)
        .expect_err("remote plaintext preflight must fail");
    assert!(error.to_string().contains("requires HTTPS"));
}
