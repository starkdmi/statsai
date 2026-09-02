use super::*;
use statsai_adapters::ClaudeCodeAdapter;

/// Runs the real Claude adapter, but discovers only the synthetic source under
/// test so the pipeline never reads the developer's own Claude installation.
struct SyntheticClaudeSourceAdapter {
    inner: ClaudeCodeAdapter,
    source: SourceLocation,
}

impl ProviderAdapter for SyntheticClaudeSourceAdapter {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn version(&self) -> &'static str {
        self.inner.version()
    }

    fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    fn discover(&self) -> Vec<SourceLocation> {
        vec![self.source.clone()]
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        self.inner.scan_candidates(source)
    }

    fn probe_verified_source_state(
        &self,
        source: &SourceLocation,
    ) -> Result<VerifiedSourceObservation> {
        self.inner.probe_verified_source_state(source)
    }

    fn collect_account_evidence(
        &self,
        source: &SourceLocation,
        checkpoints: &[statsai_core::AccountEvidenceCheckpointV1],
    ) -> Result<AccountEvidenceScan> {
        self.inner.collect_account_evidence(source, checkpoints)
    }

    fn scan(
        &self,
        source: &SourceLocation,
        options: &ScanOptions,
    ) -> Result<statsai_adapters::AdapterScan> {
        self.inner.scan(source, options)
    }
}

fn claude_plan_profile(rate_limit_tier: &str, profile_fetched_at: i64) -> String {
    serde_json::json!({
        "oauthAccount": {
            "accountUuid": "integration-account-uuid",
            "emailAddress": "integration-account@example.test",
            "profileFetchedAt": profile_fetched_at,
            "organizationType": "claude_max",
            "organizationRateLimitTier": rate_limit_tier,
            // Ignored cache-lifecycle metadata: it must not gate detection.
            "hasAvailableSubscription": false,
            "seatTier": "team_standard"
        }
    })
    .to_string()
}

fn scan_synthetic_claude_source(store: &Store, source: &SourceLocation) {
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
        store,
        "device-test",
        vec![Box::new(SyntheticClaudeSourceAdapter {
            inner: ClaudeCodeAdapter,
            source: source.clone(),
        })],
    )
    .expect("scan");
}

fn claude_plan_report(store: &Store) -> Vec<Value> {
    account_plan_evidence_report(store, Some("claude_code"), None, true).expect("plan report")
}

#[test]
fn claude_scan_persists_cached_plan_evidence_and_stays_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(root.join("projects")).expect("projects root");
    let profile_path = root.join(".claude.json");
    let first_fetched_at_millis = 1_786_104_000_000_i64;
    std::fs::write(
        &profile_path,
        claude_plan_profile("default_claude_max_5x", first_fetched_at_millis),
    )
    .expect("claude profile");
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );

    scan_synthetic_claude_source(&store, &source);

    let expected_account_id = provider_account_id_from_identity(
        "claude_code",
        Some("integration-account-uuid"),
        Some("integration-account@example.test"),
    )
    .expect("account id");
    let accounts = store.list_accounts().expect("accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].provider_account_id, expected_account_id);
    assert_eq!(
        accounts[0].email.as_deref(),
        Some("integration-account@example.test")
    );

    let report = claude_plan_report(&store);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0]["provider_account_id"], expected_account_id.0);
    assert_eq!(report[0]["latest_observation"]["plan_name"], "Max 5x");
    assert_eq!(
        report[0]["latest_observation"]["raw_plan_name"],
        "claude_max"
    );
    assert_eq!(report[0]["latest_observation"]["confidence"], "medium");
    assert_eq!(
        report[0]["latest_observation"]["evidence_kind"],
        "auth_snapshot"
    );
    assert!(report[0]["latest_observation"]["is_current_snapshot"]
        .as_bool()
        .expect("is_current_snapshot"));
    assert!(report[0]["latest_observation"]["active_from"].is_null());
    assert!(report[0]["latest_observation"]["active_until"].is_null());
    assert_eq!(report[0]["observation_count"], 1);

    // Rescanning an unchanged profile must not add a second observation.
    scan_synthetic_claude_source(&store, &source);
    let report = claude_plan_report(&store);
    assert_eq!(report[0]["observation_count"], 1);

    // A later cached fetch that upgrades the Max tier keeps both observations in
    // order and projects the newest one.
    let second_fetched_at_millis = first_fetched_at_millis + 86_400_000;
    std::fs::write(
        &profile_path,
        claude_plan_profile("default_claude_max_20x", second_fetched_at_millis),
    )
    .expect("upgraded claude profile");
    scan_synthetic_claude_source(&store, &source);

    let report = claude_plan_report(&store);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0]["observation_count"], 2);
    assert_eq!(report[0]["latest_observation"]["plan_name"], "Max 20x");
    let observations = report[0]["observations"]
        .as_array()
        .expect("observations array");
    assert_eq!(observations[0]["plan_name"], "Max 5x");
    assert_eq!(observations[1]["plan_name"], "Max 20x");
    assert_eq!(
        serde_json::from_value::<DateTime<Utc>>(observations[0]["observed_at"].clone())
            .expect("observed at"),
        Utc.timestamp_millis_opt(first_fetched_at_millis)
            .single()
            .expect("first fetch"),
        "the observation is dated strictly by the cached profile fetch time"
    );

    // A durable auth override suppresses new evidence without rewriting history:
    // no cancellation and no end time may be manufactured for what was observed.
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({"env": {"ANTHROPIC_API_KEY": "configured-key"}}).to_string(),
    )
    .expect("claude settings");
    std::fs::write(
        &profile_path,
        claude_plan_profile(
            "default_claude_max_20x",
            second_fetched_at_millis + 3_600_000,
        ),
    )
    .expect("later claude profile");
    scan_synthetic_claude_source(&store, &source);

    let report = claude_plan_report(&store);
    assert_eq!(report[0]["observation_count"], 2);
    assert_eq!(report[0]["latest_observation"]["plan_name"], "Max 20x");
    assert!(report[0]["latest_observation"]["active_until"].is_null());
}

#[test]
fn claude_scan_persists_a_tier_change_that_reuses_the_cached_fetch_time() {
    // The store deduplicates plan evidence by observation id and inserts with
    // ON CONFLICT DO NOTHING, so a tier that changes without the cached fetch
    // time moving is only kept if the canonical plan reaches that id.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("claude-config");
    std::fs::create_dir_all(root.join("projects")).expect("projects root");
    let profile_path = root.join(".claude.json");
    let fetched_at_millis = 1_786_104_000_000_i64;
    std::fs::write(
        &profile_path,
        claude_plan_profile("default_claude_max_5x", fetched_at_millis),
    )
    .expect("claude profile");
    let store = Store::in_memory().expect("store");
    let source = SourceLocation::local_adapter(
        "claude_code",
        "claude-code-local-jsonl",
        "0",
        &root,
        LocationOrigin::Configured,
    );

    scan_synthetic_claude_source(&store, &source);
    std::fs::write(
        &profile_path,
        claude_plan_profile("default_claude_max_20x", fetched_at_millis),
    )
    .expect("retiered claude profile");
    scan_synthetic_claude_source(&store, &source);

    let stored = store
        .account_plan_observations()
        .expect("plan observations")
        .into_iter()
        .map(|observation| observation.plan_name)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        stored,
        BTreeSet::from(["Max 5x".to_string(), "Max 20x".to_string()]),
        "the refined plan must survive persistence, not be discarded as a duplicate"
    );
}
