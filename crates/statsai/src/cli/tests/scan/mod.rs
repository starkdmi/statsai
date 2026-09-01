pub(super) use super::support::*;
pub(crate) use super::*;

mod files;
mod preview;

#[derive(Clone)]
struct AttributionBlockedTestAdapter {
    provider: &'static str,
    discovered: Vec<SourceLocation>,
    blocked_since: Option<DateTime<Utc>>,
}

#[derive(Clone)]
struct ClaudeProfileTestAdapter {
    source: SourceLocation,
    verified_state: VerifiedSourceState,
}

impl ProviderAdapter for ClaudeProfileTestAdapter {
    fn id(&self) -> &'static str {
        "claude-code-local-jsonl"
    }

    fn version(&self) -> &'static str {
        "0.3.3"
    }

    fn provider(&self) -> &'static str {
        "claude_code"
    }

    fn discover(&self) -> Vec<SourceLocation> {
        vec![self.source.clone()]
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(Vec::new())
    }

    fn probe_verified_source_state(
        &self,
        _source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        Ok(VerifiedSourceObservation::Inferred {
            identity: Box::new(self.verified_state.clone()),
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at: None,
        })
    }

    fn scan(
        &self,
        _source: &SourceLocation,
        _options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        Ok(statsai_adapters::AdapterScan::default())
    }
}

impl ProviderAdapter for AttributionBlockedTestAdapter {
    fn id(&self) -> &'static str {
        "attribution-blocked-test"
    }

    fn version(&self) -> &'static str {
        "0"
    }

    fn provider(&self) -> &'static str {
        self.provider
    }

    fn discover(&self) -> Vec<SourceLocation> {
        self.discovered.clone()
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(Vec::new())
    }

    fn probe_verified_source_state(
        &self,
        _source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        Ok(VerifiedSourceObservation::AttributionBlocked {
            blocked_since: self.blocked_since,
        })
    }

    fn scan(
        &self,
        _source: &SourceLocation,
        _options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        Ok(statsai_adapters::AdapterScan::default())
    }
}

#[test]
fn scan_applies_verified_source_state_even_when_source_files_are_unchanged() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-work-upgrade"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let legacy_start = Utc
        .with_ymd_and_hms(2026, 5, 24, 20, 10, 31)
        .single()
        .expect("legacy_start");
    let mut legacy_assignment = connect_source_to_account(
        &store,
        ConnectSourceToAccountInput {
            source_id: &source.source_id,
            provider_account_id_value: None,
            provider_user_id: None,
            email: Some("work"),
            label: Some("work".to_string()),
            started_at: legacy_start,
            ended_at: None,
        },
    )
    .expect("legacy work assignment");
    legacy_assignment.record_source = IdentitySource::Unknown;
    store
        .upsert_source_account_assignment(&legacy_assignment)
        .expect("legacy assignment");

    let started_at = Utc
        .with_ymd_and_hms(2026, 4, 30, 7, 43, 17)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 30, 7, 43, 18)
        .single()
        .expect("verified_at");
    let current_period_ends_at = Utc
        .with_ymd_and_hms(2026, 5, 30, 7, 43, 17)
        .single()
        .expect("current_period_ends_at");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan {
            diagnostics: ScanDiagnostics {
                files_skipped_unchanged: 1,
                ..ScanDiagnostics::default()
            },
            verified_source_state: Some(VerifiedSourceState {
                provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
                email: Some("verified@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(verified_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: Some(current_period_ends_at),
                    current_period_ends_at: Some(current_period_ends_at),
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let expected_account_id = provider_account_id_from_identity(
        "codex",
        Some("11111111-2222-4333-8444-555555555555"),
        Some("verified@example.com"),
    )
    .expect("expected account id");

    let accounts = store.list_accounts().expect("accounts");
    assert!(accounts.iter().any(|account| {
        account.provider_account_id == expected_account_id
            && account.email.as_deref() == Some("verified@example.com")
            && account.plan_name.is_none()
    }));

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, started_at);
    assert_eq!(assignments[0].ended_at, None);
    assert_eq!(assignments[0].provider_account_id, expected_account_id);
    assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);

    assert!(store
        .list_subscriptions()
        .expect("subscriptions")
        .is_empty());
    let stored_source = store
        .source(&source.source_id)
        .expect("source row")
        .expect("stored source");
    assert!(stored_source.verified_state_hash.is_some());
}

#[test]
fn scan_backfills_claude_profile_inference_without_changed_usage_files() {
    let store = Store::in_memory().expect("store");
    let authenticated_at = Utc
        .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
        .single()
        .expect("authenticated at");
    let usage_at = authenticated_at + Duration::hours(1);
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0.3.3",
        Path::new("/tmp/claude-broken-profile-scan-migration"),
        LocationOrigin::Default,
    );
    source.verified_state_hash =
        verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
            blocked_since: None,
        })
        .expect("blocked observation hash");
    store.upsert_source(&source).expect("source");
    store
        .insert_event(&test_event(
            "claude_code",
            &source,
            usage_at,
            None,
            TokenParts::total(15),
        ))
        .expect("unassigned event");
    let adapter = ClaudeProfileTestAdapter {
        source: source.clone(),
        verified_state: VerifiedSourceState {
            provider_user_id: Some("claude-account".to_string()),
            email: Some("claude@example.com".to_string()),
            account_label: None,
            plan_name: None,
            authenticated_at: Some(authenticated_at),
            verified_at: Some(authenticated_at),
            subscription: None,
        },
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let events = store
        .events_for_source(&source.source_id)
        .expect("reattributed events");
    assert!(events[0].provider_account_id.is_some());
    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].started_at, authenticated_at);
    let stored_source = store
        .source(&source.source_id)
        .expect("source row")
        .expect("stored source");
    assert!(stored_source
        .verified_state_hash
        .as_deref()
        .is_some_and(|hash| hash.starts_with("inferred_source.v1:")));
}

#[test]
fn scan_reopens_existing_verified_assignment_when_auth_is_still_current() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-reopen-verified"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
        .single()
        .expect("started_at");
    let closed_at = Utc
        .with_ymd_and_hms(2026, 5, 24, 20, 10, 31)
        .single()
        .expect("closed_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
        .single()
        .expect("verified_at");

    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: Some("11111111-2222-4333-8444-555555555555"),
            email: Some("verified@example.com"),
            label: None,
            plan_name: Some("Plus".to_string()),
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: Some(verified_at),
        },
    )
    .expect("account");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &account.provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: account.provider_account_id.clone(),
            started_at,
            ended_at: Some(closed_at),
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(verified_at),
            created_at: started_at,
            updated_at: closed_at,
        })
        .expect("closed assignment");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan {
            diagnostics: ScanDiagnostics {
                files_skipped_unchanged: 1,
                ..ScanDiagnostics::default()
            },
            verified_source_state: Some(VerifiedSourceState {
                provider_user_id: Some("11111111-2222-4333-8444-555555555555".to_string()),
                email: Some("verified@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(verified_at),
                subscription: None,
            }),
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(
        assignments[0].provider_account_id,
        account.provider_account_id
    );
    assert_eq!(assignments[0].started_at, started_at);
    assert_eq!(assignments[0].ended_at, None);
}

#[test]
fn scan_skips_full_scan_when_usage_and_verified_state_are_unchanged() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-scan-skip"),
        LocationOrigin::Configured,
    );
    let verified_state = VerifiedSourceState {
        provider_user_id: Some("acct-verified".to_string()),
        email: Some("verified@example.com".to_string()),
        account_label: None,
        plan_name: Some("Plus".to_string()),
        authenticated_at: Some(
            Utc.with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
                .single()
                .expect("authenticated_at"),
        ),
        verified_at: Some(
            Utc.with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
                .single()
                .expect("verified_at"),
        ),
        subscription: None,
    };
    source.verified_state_hash = verified_source_observation_hash(
        &VerifiedSourceObservation::Verified(Box::new(verified_state.clone())),
    )
    .expect("verified state hash");
    store.upsert_source(&source).expect("source");

    let scan_calls = Arc::new(Mutex::new(0u64));
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: Some(verified_state),
        scan_calls: Some(scan_calls.clone()),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    assert_eq!(*scan_calls.lock().expect("scan calls"), 0);
}

#[test]
fn scan_preserves_verified_assignment_when_auto_source_auth_is_unavailable() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-unassign-on-missing-auth"),
        LocationOrigin::Configured,
    );
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
        .single()
        .expect("verified_at");
    let verified_state = VerifiedSourceState {
        provider_user_id: Some("acct-verified".to_string()),
        email: Some("verified@example.com".to_string()),
        account_label: None,
        plan_name: Some("Plus".to_string()),
        authenticated_at: Some(started_at),
        verified_at: Some(verified_at),
        subscription: None,
    };
    source.verified_state_hash =
        verified_source_state_hash(Some(&verified_state)).expect("verified state hash");
    store.upsert_source(&source).expect("source");

    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: verified_state.provider_user_id.as_deref(),
            email: verified_state.email.as_deref(),
            label: None,
            plan_name: verified_state.plan_name.clone(),
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: verified_state.verified_at,
        },
    )
    .expect("account");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &account.provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: account.provider_account_id.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(verified_at),
            created_at: started_at,
            updated_at: started_at,
        })
        .expect("assignment");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, None);
    let stored_source = store
        .source(&source.source_id)
        .expect("source row")
        .expect("stored source");
    assert_eq!(
        stored_source.verified_state_hash,
        source.verified_state_hash
    );
}

#[test]
fn scan_closes_verified_assignment_when_source_auth_is_explicitly_blocked() {
    let store = Store::in_memory().expect("store");
    let mut source = SourceLocation::local_adapter(
        "claude_code",
        "test",
        "0",
        Path::new("/tmp/claude-explicit-auth-override"),
        LocationOrigin::Configured,
    );
    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let blocked_since = Utc
        .with_ymd_and_hms(2026, 5, 30, 8, 45, 0)
        .single()
        .expect("blocked_since");
    let verified_state = VerifiedSourceState {
        provider_user_id: Some("oauth-account".to_string()),
        email: Some("oauth@example.com".to_string()),
        account_label: None,
        plan_name: None,
        authenticated_at: Some(started_at),
        verified_at: Some(started_at),
        subscription: None,
    };
    source.verified_state_hash =
        verified_source_state_hash(Some(&verified_state)).expect("verified state hash");
    store.upsert_source(&source).expect("source");
    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "claude_code",
            provider_user_id: verified_state.provider_user_id.as_deref(),
            email: verified_state.email.as_deref(),
            label: None,
            plan_name: None,
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: verified_state.verified_at,
        },
    )
    .expect("account");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &account.provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: account.provider_account_id,
            started_at,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(started_at),
            created_at: started_at,
            updated_at: started_at,
        })
        .expect("assignment");

    let adapter = AttributionBlockedTestAdapter {
        provider: "claude_code",
        discovered: vec![source.clone()],
        blocked_since: Some(blocked_since),
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, Some(blocked_since));
}

#[test]
fn scan_preserves_legacy_verified_assignment_when_auth_is_unavailable() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-legacy-unassign-on-missing-auth"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let started_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
        .single()
        .expect("started_at");
    let verified_at = Utc
        .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
        .single()
        .expect("verified_at");
    let account = upsert_provider_account(
        &store,
        UpsertProviderAccountInput {
            provider: "codex",
            provider_user_id: Some("acct-legacy-verified"),
            email: Some("legacy-verified@example.com"),
            label: None,
            plan_name: Some("Plus".to_string()),
            identity_source: Some(IdentitySource::LocalAuth),
            verified_at: Some(verified_at),
        },
    )
    .expect("account");
    store
        .upsert_source_account_assignment(&SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                &account.provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: account.provider_account_id.clone(),
            started_at,
            ended_at: None,
            record_source: IdentitySource::LocalAuth,
            verified_at: Some(verified_at),
            created_at: started_at,
            updated_at: started_at,
        })
        .expect("assignment");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: Vec::new(),
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let assignments = store
        .list_source_account_assignments_for_source(&source.source_id)
        .expect("assignments");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].ended_at, None);
}

#[test]
fn scan_persists_task_spans_and_rebuilds_work_items() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-spans"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-spans/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 30, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "task-span-a",
        "Implement local task collection",
        &event,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-a")],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event],
            task_spans: vec![task_span],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].title, "Implement local task collection");

    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, "Implement local task collection");
    assert_eq!(work_items[0].span_count, 1);
    assert_eq!(work_items[0].total_tokens, 150);
}

#[test]
fn scan_without_include_tasks_does_not_persist_task_tables() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-opt-in"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-opt-in/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 35, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "task-span-a",
        "Implement local task collection",
        &event,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-a")],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event],
            task_spans: vec![task_span],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    assert_eq!(store.event_count().expect("event count"), 1);
    assert!(store.task_spans().expect("task spans").is_empty());
    assert!(store.work_items().expect("work items").is_empty());
}

#[test]
fn scan_without_include_tasks_preserves_existing_task_tables() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-preserve"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-preserve/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 40, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "task-span-a",
        "Keep local tasks",
        &event,
    );
    store
        .upsert_task_spans(std::slice::from_ref(&task_span))
        .expect("insert task span");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-b")],
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: false,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].title, "Keep local tasks");

    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, "Keep local tasks");
}

#[test]
fn scan_rebuild_prefers_real_work_item_title_over_metric_spans() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-title-quality"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-title-quality/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 10, 0, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_path, started_at, "event-a", 200);
    let event_b = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(1),
        "event-b",
        220,
    );
    let event_c = test_scan_event(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "event-c",
        240,
    );
    let span_metric = test_task_span(
        &source,
        file_path,
        started_at,
        "metric-a",
        "Qwen3.5 8bit ckpt2400: F1_overlap=49.19 Avg_TIoU=74.88 MAE=1.85 TitleF1=39.34",
        &event_a,
    );
    let span_coverage = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(1),
        "metric-b",
        "coverage=1.000 (100/100) F1@0.5=67.10 F1@0.7=51.60 MAE=2.230",
        &event_b,
    );
    let span_intent = test_task_span(
        &source,
        file_path,
        started_at + Duration::minutes(2),
        "intent-c",
        "I want to choose the best adapters to average",
        &event_c,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-quality")],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event_a, event_b, event_c],
            task_spans: vec![span_metric, span_coverage, span_intent],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: None,
    };

    scan_with_adapters(
        ScanCommand {
            provider: None,
            include_tasks: true,
            preview: false,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("scan");

    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(
        work_items[0].title,
        "I want to choose the best adapters to average"
    );
}
