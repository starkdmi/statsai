use super::support::*;
use super::*;

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
fn scan_skips_files_when_legacy_codex_auth_signature_is_cached() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-legacy-auth-cache"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-legacy-auth-cache/session.jsonl";
    let current_candidate = ScanCandidateFile {
        path: PathBuf::from(file_path),
        cache_key: file_path.to_string(),
        cache_signature: "sig-current".to_string(),
        compatible_cache_signatures: vec!["sig-legacy-auth".to_string()],
    };
    store
        .record_scan_file_entries(
            &source.source_id,
            &[ScanFileStateEntry {
                cache_key: current_candidate.cache_key.clone(),
                cache_signature: "sig-legacy-auth".to_string(),
            }],
        )
        .expect("record legacy scan cache");

    let scan_calls = Arc::new(Mutex::new(0u64));
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![current_candidate],
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
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

    let stored_entries = store
        .scan_file_entries(&source.source_id)
        .expect("stored scan file entries");
    assert_eq!(
        stored_entries,
        vec![ScanFileStateEntry {
            cache_key: file_path.to_string(),
            cache_signature: "sig-current".to_string(),
        }]
    );

    let second_scan_calls = Arc::new(Mutex::new(0u64));
    let rotated_legacy_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![ScanCandidateFile {
            path: PathBuf::from(file_path),
            cache_key: file_path.to_string(),
            cache_signature: "sig-current".to_string(),
            compatible_cache_signatures: vec!["sig-legacy-auth-rotated".to_string()],
        }],
        scan_result: statsai_adapters::AdapterScan::default(),
        probe_result: None,
        scan_calls: Some(second_scan_calls.clone()),
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
        vec![Box::new(rotated_legacy_adapter)],
    )
    .expect("second scan");

    assert_eq!(*second_scan_calls.lock().expect("scan calls"), 0);
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
fn no_cache_scan_reselects_unchanged_files() {
    let store = Store::in_memory().expect("store");
    let source_id = statsai_core::SourceId("src-no-cache".to_string());
    let compatible_signatures = HashMap::new();
    let entries = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];

    let initial = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        false,
        false,
    )
    .expect("initial selection");
    assert_eq!(initial, entries);
    store
        .record_scan_file_entries(&source_id, &entries)
        .expect("record cache state");

    let default_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        false,
        false,
    )
    .expect("default selection");
    assert!(default_selection.is_empty());

    let no_cache_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        false,
        true,
        false,
    )
    .expect("no-cache selection");
    assert_eq!(no_cache_selection, entries);

    let replace_selection = select_scan_file_entries(
        &store,
        &source_id,
        &entries,
        &compatible_signatures,
        true,
        false,
        false,
    )
    .expect("replace selection");
    assert_eq!(replace_selection, entries);
}

#[test]
fn full_source_rescan_replaces_existing_source_records() {
    assert!(should_replace_source_records_for_scan(
        true, false, 0, 0, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, true, 0, 0, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, false, 2, 2, false
    ));
    assert!(should_replace_source_records_for_scan(
        false, false, 0, 0, true
    ));
    assert!(!should_replace_source_records_for_scan(
        false, false, 2, 1, false
    ));
    assert!(!should_replace_source_records_for_scan(
        false, false, 0, 0, false
    ));
}

#[test]
fn cache_invalidation_reconciles_quota_records_by_file() {
    assert!(!should_replace_all_source_quota_records(false, false));
    assert!(should_replace_all_source_quota_records(true, false));
    assert!(should_replace_all_source_quota_records(false, true));
}

#[test]
fn no_cache_rescan_reconciles_quota_records_instead_of_deleting_the_source() {
    // `--no-cache` rereads every file, so the file-level path already rewrites everything it
    // produces. Deleting the source first walked every observation and window on a store with
    // six figures of rows, which is the stall a documented flag must not have.
    assert!(!should_replace_all_source_quota_records(false, false));
    // The full reread still replaces the source's records, so the reconciliation branch --
    // the one that also retires rows outside the rescanned file set -- is the branch it takes.
    assert!(should_replace_source_records_for_scan(
        false, true, 0, 0, false
    ));
    // An explicit destructive rebuild keeps the blanket delete.
    assert!(should_replace_all_source_quota_records(true, false));
}

#[test]
fn scan_file_reconciliation_tracks_removed_candidates() {
    let store = Store::in_memory().expect("store");
    let source_id = statsai_core::SourceId("src-removed-cache".to_string());
    let tracked = vec![
        ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];
    store
        .record_scan_file_entries(&source_id, &tracked)
        .expect("record tracked cache state");

    let reconciliation = select_scan_file_reconciliation(
        &store,
        &source_id,
        &[ScanFileStateEntry {
            cache_key: "/tmp/b.jsonl".to_string(),
            cache_signature: "sig-b-1".to_string(),
        }],
        &HashMap::new(),
        false,
        false,
        false,
    )
    .expect("reconciliation");

    assert!(reconciliation.pending_entries.is_empty());
    assert_eq!(
        reconciliation.removed_entries,
        vec![ScanFileStateEntry {
            cache_key: "/tmp/a.jsonl".to_string(),
            cache_signature: "sig-a-1".to_string(),
        }]
    );
}

#[test]
fn partial_scan_removes_rows_that_disappear_from_changed_file() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-partial-rescan"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-partial-rescan/a.jsonl";
    let file_b = "/tmp/codex-partial-rescan/b.jsonl";
    let initial_candidates = vec![
        test_scan_candidate(file_a, "sig-a-1"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let next_candidates = vec![
        test_scan_candidate(file_a, "sig-a-2"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let a_started_at = Utc
        .with_ymd_and_hms(2026, 5, 1, 10, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 5, 2, 10, 0, 0)
        .single()
        .expect("b_started_at");
    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: initial_candidates,
        scan_result: statsai_adapters::AdapterScan {
            events: vec![
                test_scan_event(&source, file_a, a_started_at, "event-a", 100),
                test_scan_event(&source, file_b, b_started_at, "event-b", 200),
            ],
            summaries: vec![
                test_scan_summary(&source, file_a, a_started_at, "summary-a", 100),
                test_scan_summary(&source, file_b, b_started_at, "summary-b", 200),
            ],
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
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");

    assert_eq!(store.event_count().expect("event count"), 2);
    assert_eq!(store.summary_count().expect("summary count"), 2);
    assert_eq!(store.sync_rollup_count().expect("rollup count"), 2);

    let changed_only_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: next_candidates,
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
        vec![Box::new(changed_only_adapter)],
    )
    .expect("partial scan");

    let events = store.events_for_source(&source.source_id).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref()),
        Some("event-b")
    );
    let summaries = store
        .summaries_for_source(&source.source_id)
        .expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].summary_id,
        summary_id("codex", &source.source_id, "summary-b")
    );
    assert_eq!(store.sync_rollup_count().expect("rollup count"), 1);
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
fn scan_with_include_tasks_backfills_files_cached_without_tasks() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-backfill"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-backfill/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 38, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "event-task", 150);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "task-span-a",
        "Backfill local tasks",
        &event,
    );
    let candidate = test_scan_candidate(file_path, "sig-a");
    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![candidate.clone()],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event.clone()],
            task_spans: vec![task_span.clone()],
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
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");
    assert!(store.task_spans().expect("initial task spans").is_empty());

    let scan_calls = Arc::new(Mutex::new(0u64));
    let backfill_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![candidate],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event],
            task_spans: vec![task_span],
            ..statsai_adapters::AdapterScan::default()
        },
        probe_result: None,
        scan_calls: Some(scan_calls.clone()),
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
        vec![Box::new(backfill_adapter)],
    )
    .expect("task backfill scan");

    assert_eq!(*scan_calls.lock().expect("scan calls"), 1);
    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].title, "Backfill local tasks");
    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, "Backfill local tasks");
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

#[test]
fn scan_preview_does_not_persist_task_tables() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-preview"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-preview/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 9, 45, 0)
        .single()
        .expect("started_at");
    let event = test_scan_event(&source, file_path, started_at, "preview-event", 80);
    let task_span = test_task_span(
        &source,
        file_path,
        started_at,
        "preview-span",
        "Preview task collection",
        &event,
    );
    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![test_scan_candidate(file_path, "sig-preview")],
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
            preview: true,
            no_cache: false,
            replace: false,
            verbose: false,
            explain: false,
        },
        &store,
        "device-test",
        vec![Box::new(adapter)],
    )
    .expect("preview scan");

    assert_eq!(store.event_count().expect("event count"), 0);
    assert_eq!(store.summary_count().expect("summary count"), 0);
    assert!(store.task_spans().expect("task spans").is_empty());
    assert!(store.work_items().expect("work items").is_empty());
}

#[test]
fn preview_task_rebuild_counts_only_affected_work_items() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-preview-rebuild"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-task-preview-rebuild/a.jsonl";
    let file_b = "/tmp/codex-task-preview-rebuild/b.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 10, 30, 0)
        .single()
        .expect("started_at");
    let event_a = test_scan_event(&source, file_a, started_at, "preview-a", 90);
    let event_b = test_scan_event(
        &source,
        file_b,
        started_at + Duration::minutes(10),
        "preview-b",
        110,
    );
    let mut span_a = test_task_span(
        &source,
        file_a,
        started_at,
        "preview-span-a",
        "Preview rebuild task A",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source,
        file_b,
        started_at + Duration::minutes(10),
        "preview-span-b",
        "Preview rebuild task B",
        &event_b,
    );
    span_a.project = Some(ProjectInfo {
        project_id: "project-a".to_string(),
        project_label: Some("project-a".to_string()),
        repo_remote_hash: Some("repo-a".to_string()),
        repo_label: Some("owner/project-a".to_string()),
        branch_hash: Some("branch-a".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-a".to_string()),
        path_label: Some("/tmp/project-a".to_string()),
    });
    span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
    span_a.branch_family = branch_family(Some("main"));
    span_b.project = Some(ProjectInfo {
        project_id: "project-b".to_string(),
        project_label: Some("project-b".to_string()),
        repo_remote_hash: Some("repo-b".to_string()),
        repo_label: Some("owner/project-b".to_string()),
        branch_hash: Some("branch-b".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("path-b".to_string()),
        path_label: Some("/tmp/project-b".to_string()),
    });
    span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
    span_b.branch_family = branch_family(Some("main"));

    store
        .insert_events(&[event_a.clone(), event_b.clone()])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let mut updated_span_a = span_a.clone();
    updated_span_a.title = "Preview rebuild task A updated".to_string();
    updated_span_a.summary_preview = Some("Preview rebuild task A updated".to_string());
    updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

    let pending_entries = scan_file_state_entries(&[test_scan_candidate(file_a, "sig-a-2")]);
    let mut preview = PreviewTaskRebuild::default();
    let rebuilt = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_entries,
                removed_file_entries: &[],
                task_spans: &[updated_span_a],
            },
        )
        .expect("preview work items rebuilt");
    assert_eq!(rebuilt, 1);
    assert_eq!(store.task_spans().expect("task spans").len(), 2);
    assert_eq!(store.work_items().expect("work items").len(), 2);
}

#[test]
fn preview_task_rebuild_counts_shared_bucket_rebuilds_per_source_step() {
    let store = Store::in_memory().expect("store");
    let source_a = SourceLocation::local_adapter(
        "claude_code",
        "test-a",
        "0",
        Path::new("/tmp/preview-shared-a"),
        LocationOrigin::Configured,
    );
    let source_b = SourceLocation::local_adapter(
        "claude_code",
        "test-b",
        "0",
        Path::new("/tmp/preview-shared-b"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source_a).expect("source a");
    store.upsert_source(&source_b).expect("source b");

    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("started_at");
    let file_a = "/tmp/preview-shared-a/session.jsonl";
    let file_b = "/tmp/preview-shared-b/session.jsonl";
    let event_a = test_scan_event(&source_a, file_a, started_at, "shared-a", 120);
    let event_b = test_scan_event(
        &source_b,
        file_b,
        started_at + Duration::minutes(20),
        "shared-b",
        140,
    );
    let mut span_a = test_task_span(
        &source_a,
        file_a,
        started_at,
        "shared-span-a",
        "Shared bucket task",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source_b,
        file_b,
        started_at + Duration::minutes(20),
        "shared-span-b",
        "Shared bucket task",
        &event_b,
    );
    let shared_project = ProjectInfo {
        project_id: "shared-project".to_string(),
        project_label: Some("shared-project".to_string()),
        repo_remote_hash: Some("shared-repo".to_string()),
        repo_label: Some("owner/shared".to_string()),
        branch_hash: Some("shared-branch".to_string()),
        branch_label: Some("main".to_string()),
        path_hash: Some("shared-path".to_string()),
        path_label: Some("/tmp/shared-project".to_string()),
    };
    span_a.project = Some(shared_project.clone());
    span_b.project = Some(shared_project);
    span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
    span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
    span_a.branch_family = branch_family(Some("main"));
    span_b.branch_family = branch_family(Some("main"));

    store
        .insert_events(&[event_a.clone(), event_b.clone()])
        .expect("insert events");
    store
        .upsert_task_spans(&[span_a.clone(), span_b.clone()])
        .expect("insert spans");
    store
        .rebuild_all_task_work_items()
        .expect("initial rebuild");

    let mut updated_span_a = span_a.clone();
    updated_span_a.title = "Shared bucket task updated".to_string();
    updated_span_a.summary_preview = Some("Shared bucket task updated".to_string());
    updated_span_a.normalized_title = normalize_task_title(&updated_span_a.title);

    let mut updated_span_b = span_b.clone();
    updated_span_b.summary_preview = Some("Shared bucket task follow-up".to_string());

    let pending_a = scan_file_state_entries(&[test_scan_candidate(file_a, "shared-a-2")]);
    let pending_b = scan_file_state_entries(&[test_scan_candidate(file_b, "shared-b-2")]);
    let mut preview = PreviewTaskRebuild::default();
    let rebuilt_a = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source_a.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_a,
                removed_file_entries: &[],
                task_spans: &[updated_span_a],
            },
        )
        .expect("preview rebuild a");
    let rebuilt_b = preview
        .apply_source_changes(
            &store,
            SourceTaskChangeSet {
                source_id: &source_b.source_id,
                replace_source_records: false,
                touched_files: true,
                pending_file_entries: &pending_b,
                removed_file_entries: &[],
                task_spans: &[updated_span_b],
            },
        )
        .expect("preview rebuild b");

    assert_eq!(rebuilt_a, 1);
    assert_eq!(rebuilt_b, 1);
    assert_eq!(rebuilt_a + rebuilt_b, 2);
    assert_eq!(store.work_items().expect("work items").len(), 1);
}

#[test]
fn partial_scan_removes_stale_task_spans_and_rebuilds_work_items() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-partial-rescan"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-task-partial-rescan/a.jsonl";
    let file_b = "/tmp/codex-task-partial-rescan/b.jsonl";
    let initial_candidates = vec![
        test_scan_candidate(file_a, "sig-a-1"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let next_candidates = vec![
        test_scan_candidate(file_a, "sig-a-2"),
        test_scan_candidate(file_b, "sig-b-1"),
    ];
    let a_started_at = Utc
        .with_ymd_and_hms(2026, 6, 12, 11, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
        .single()
        .expect("b_started_at");
    let event_a = test_scan_event(&source, file_a, a_started_at, "event-a", 100);
    let event_b = test_scan_event(&source, file_b, b_started_at, "event-b", 200);
    let mut span_a = test_task_span(
        &source,
        file_a,
        a_started_at,
        "span-a",
        "Implement task cleanup",
        &event_a,
    );
    let mut span_b = test_task_span(
        &source,
        file_b,
        b_started_at,
        "span-b",
        "Implement task benchmark reporting",
        &event_b,
    );
    span_a.session_id = Some("session-a".to_string());
    span_b.session_id = Some("session-b".to_string());

    let initial_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: initial_candidates,
        scan_result: statsai_adapters::AdapterScan {
            events: vec![event_a.clone(), event_b.clone()],
            task_spans: vec![span_a, span_b.clone()],
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
        vec![Box::new(initial_adapter)],
    )
    .expect("initial scan");

    assert_eq!(store.task_spans().expect("task spans").len(), 2);
    assert_eq!(store.work_items().expect("work items").len(), 2);

    let changed_only_adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: next_candidates,
        scan_result: statsai_adapters::AdapterScan::default(),
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
        vec![Box::new(changed_only_adapter)],
    )
    .expect("partial scan");

    let spans = store.task_spans().expect("task spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].source_record_id.as_deref(), Some("span-b"));

    let work_items = store.work_items().expect("work items");
    assert_eq!(work_items.len(), 1);
    assert_eq!(work_items[0].title, span_b.title);
}

#[test]
fn partial_scan_with_legacy_rows_falls_back_to_full_source_reconcile() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-partial-legacy"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_a = "/tmp/codex-partial-legacy/a.jsonl";
    let file_b = "/tmp/codex-partial-legacy/b.jsonl";
    let tracked_entries = vec![
        ScanFileStateEntry {
            cache_key: file_a.to_string(),
            cache_signature: "sig-a-1".to_string(),
        },
        ScanFileStateEntry {
            cache_key: file_b.to_string(),
            cache_signature: "sig-b-1".to_string(),
        },
    ];
    store
        .record_scan_file_entries(&source.source_id, &tracked_entries)
        .expect("record initial cache");

    let a_started_at = Utc
        .with_ymd_and_hms(2026, 5, 3, 10, 0, 0)
        .single()
        .expect("a_started_at");
    let b_started_at = Utc
        .with_ymd_and_hms(2026, 5, 4, 10, 0, 0)
        .single()
        .expect("b_started_at");
    let legacy_event_a = test_event("codex", &source, a_started_at, None, TokenParts::total(50));
    let legacy_event_b = test_event("codex", &source, b_started_at, None, TokenParts::total(75));
    let mut legacy_summary_a = test_summary("codex", &source, a_started_at, 50, None);
    legacy_summary_a.summary_id = summary_id("codex", &source.source_id, "legacy-summary-a");
    let mut legacy_summary_b = test_summary("codex", &source, b_started_at, 75, None);
    legacy_summary_b.summary_id = summary_id("codex", &source.source_id, "legacy-summary-b");
    store
        .insert_events(&[legacy_event_a, legacy_event_b])
        .expect("seed legacy events");
    store
        .upsert_summaries(&[legacy_summary_a, legacy_summary_b])
        .expect("seed legacy summaries");

    let adapter = TestAdapter {
        provider: "codex",
        discovered: vec![source.clone()],
        candidates: vec![
            test_scan_candidate(file_a, "sig-a-2"),
            test_scan_candidate(file_b, "sig-b-1"),
        ],
        scan_result: statsai_adapters::AdapterScan {
            events: vec![test_scan_event(
                &source,
                file_b,
                b_started_at,
                "event-b",
                125,
            )],
            summaries: vec![test_scan_summary(
                &source,
                file_b,
                b_started_at,
                "summary-b",
                125,
            )],
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
    .expect("reconcile scan");

    let events = store.events_for_source(&source.source_id).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .parse_evidence
            .as_ref()
            .and_then(|evidence| evidence.source_record_id.as_deref()),
        Some("event-b")
    );
    let summaries = store
        .summaries_for_source(&source.source_id)
        .expect("summaries");
    assert_eq!(summaries.len(), 1);
    assert_eq!(
        summaries[0].summary_id,
        summary_id("codex", &source.source_id, "summary-b")
    );
}

fn test_scan_summary(
    source: &SourceLocation,
    file_path: &str,
    observed_at: DateTime<Utc>,
    record_id: &str,
    total_tokens: u64,
) -> UsageSummary {
    let mut summary = test_summary("codex", source, observed_at, total_tokens, None);
    summary.summary_id = summary_id("codex", &source.source_id, record_id);
    summary.source.source_kind = SourceKind::LocalAdapter;
    summary.source.source_type = "jsonl".to_string();
    summary.source.source_record_id = Some(record_id.to_string());
    summary.parse_evidence = Some(ParseEvidence {
        event_key_version: "test-scan-summary.v1".to_string(),
        source_file_path_hash: Some(hash_text(file_path)),
        source_line_number: None,
        source_record_id: Some(record_id.to_string()),
        model_inferred: false,
        timestamp_inferred: false,
        account_identity_source: IdentitySource::Unresolved,
    });
    summary
}

#[test]
fn scan_rewrites_task_span_links_to_canonical_event_ids() {
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "codex",
        "test",
        "0",
        Path::new("/tmp/codex-task-link-rewrite"),
        LocationOrigin::Configured,
    );
    store.upsert_source(&source).expect("source");

    let file_path = "/tmp/codex-task-link-rewrite/session.jsonl";
    let started_at = Utc
        .with_ymd_and_hms(2026, 6, 20, 12, 0, 0)
        .single()
        .expect("started_at");
    let existing_event = test_scan_event(&source, file_path, started_at, "existing", 100);
    store
        .insert_event(&existing_event)
        .expect("insert existing event");

    let mut duplicate_event = existing_event.clone();
    duplicate_event.event_id = event_id("codex", &source.source_id, "duplicate", None, started_at);
    duplicate_event.source.source_record_id = Some("duplicate".to_string());
    if let Some(parse_evidence) = duplicate_event.parse_evidence.as_mut() {
        parse_evidence.source_record_id = Some("duplicate".to_string());
    }
    let span = test_task_span(
        &source,
        file_path,
        started_at,
        "duplicate-span",
        "Rewrite canonical task links",
        &duplicate_event,
    );

    let insert_result = store
        .insert_events_with_resolution(&[duplicate_event])
        .expect("insert duplicate event");
    assert_eq!(insert_result.inserted, 0);

    let mut spans = vec![span];
    rewrite_task_span_linked_event_ids(&mut spans, &insert_result.canonical_event_ids);
    store.upsert_task_spans(&spans).expect("upsert spans");

    let stored_spans = store.task_spans().expect("task spans");
    assert_eq!(stored_spans.len(), 1);
    assert_eq!(
        stored_spans[0].linked_event_ids,
        vec![existing_event.event_id.clone()]
    );
}
