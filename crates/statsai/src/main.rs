use anyhow::Result;
#[cfg(test)]
use anyhow::{bail, Context};
#[cfg(test)]
use chrono::Utc;
use clap::Parser;
#[cfg(test)]
use serde_json::{json, Value};
#[cfg(test)]
use statsai_adapters::VerifiedSubscriptionState;
#[cfg(test)]
use statsai_adapters::{
    retain_accounts_referenced_by_account_evidence, AccountEvidenceScan, ProviderAdapter,
    ScanCandidateFile, ScanDiagnostics, ScanOptions, SourceIdentityInference,
    VerifiedSourceObservation, VerifiedSourceState,
};
#[cfg(test)]
use statsai_core::{
    account_plan_observation_id, build_usage_report, conversation_account_binding_id, home_dir,
    normalize_email, provider_account_id, provider_account_id_from_identity,
    sanitize_code_change_metric_for_sync, source_account_assignment_id, ArchiveContentKind,
    ArchiveConversation, LocationOrigin, ProviderAccountId, QuotaObservationRecordV1,
    QuotaWindowV1, ReportPeriod, SourceAccountAssignment, SourceAccountAssignmentId, SourceId,
    SourceLocation, SourceVerificationMode, SyncAuthoritativeSnapshot, SyncBatch, UsageEvent,
    SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION, SYNC_BATCH_SCHEMA_VERSION,
};
#[cfg(test)]
use statsai_sdk::{ReportedUsageSummaryInput, ReportedUsageSummaryRecord};
use statsai_store::Store;
#[cfg(test)]
use statsai_store::SyncPreferences;
#[cfg(test)]
use statsai_store::{
    apply_verified_source_state, reconcile_verified_source_state, verified_source_observation_hash,
    verified_source_state_hash,
};
#[cfg(test)]
use statsai_store::{upsert_provider_account, ScanFileStateEntry, UpsertProviderAccountInput};
#[cfg(test)]
use std::collections::{BTreeSet, HashMap, HashSet};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration as StdDuration;

use statsai::{default_device_id, default_store_path, snapshot};

mod cli;
use cli::account::*;
use cli::args::*;
use cli::auth::auth;
use cli::conversation::*;
use cli::daemon::daemon;
#[cfg(test)]
use cli::format::{subscription_json_value, usd_amount_json};
use cli::import::*;
use cli::quota::*;
use cli::report::*;
use cli::scan::*;
use cli::schema::schema;
use cli::service::service;
use cli::source::*;
use cli::status::{doctor, status};
use cli::store_admin::store_admin;
use cli::subscription::*;
use cli::sync::*;
use cli::task::*;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store_path = cli.store.unwrap_or_else(default_store_path);
    let device_id = cli.device_id.unwrap_or_else(default_device_id);

    match cli.command {
        Command::Schema(command) => schema(command),
        Command::Store(command) => store_admin(command, &store_path),
        Command::Doctor => doctor(&store_path),
        Command::Auth(command) => auth(command),
        Command::Service(command) => service(command),
        Command::Snapshot(command) => snapshot::run(command, &store_path, &device_id),
        command => {
            let store = if command_reprices_persisted_usage(&command) {
                statsai::open_operational_store(&store_path)?
            } else {
                Store::open(&store_path)?
            };
            match command {
                Command::Scan(command) => scan(command, &store, &device_id),
                Command::Report(command) => report(command, &store),
                Command::Source(command) => source(command, &store, &device_id),
                Command::Account(command) => account(command, &store),
                Command::Subscription(command) => subscription(command, &store),
                Command::Import(command) => import(command, &store, &device_id),
                Command::Export(command) => export(command, &store),
                Command::Task(command) => task(command, &store),
                Command::Conversation(command) => conversation(command, &store, &device_id),
                Command::Quota(command) => quota(command, &store, &device_id),
                Command::Privacy(command) => {
                    statsai::privacy_cli::run(command, &store, &store_path)
                }
                Command::Sync(command) => sync(command, &store, &device_id),
                Command::Daemon(command) => daemon(command, store, &device_id),
                Command::Status => status(&store),
                Command::Schema(_)
                | Command::Store(_)
                | Command::Doctor
                | Command::Auth(_)
                | Command::Service(_)
                | Command::Snapshot(_) => {
                    unreachable!("handled before store open")
                }
            }
        }
    }
}

fn command_reprices_persisted_usage(command: &Command) -> bool {
    matches!(
        command,
        Command::Scan(_)
            | Command::Report(_)
            | Command::Import(_)
            | Command::Export(_)
            | Command::Task(_)
            | Command::Sync(_)
            | Command::Daemon(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone};
    use statsai_core::{
        branch_family, event_id, hash_text, normalize_task_title, project_bucket_key,
        subscription_id, summary_id, task_span_id, BillingPeriod, Confidence, CostInfo,
        EventSource, IdentitySource, ModelInfo, ParseEvidence, PrivacyInfo, PrivacyMode,
        ProjectInfo, ProviderAccount, SessionInfo, SourceKind, Subscription, SubscriptionStatus,
        SummaryMetadata, TaskBucketSnapshot, TaskSpan, TaskSpanId, TaskStatus, TaskVerdict,
        TaskVerification, TaskVerificationAction, TaskVerificationId, UsageCounts, UsageSummary,
        WorkItem, WorkItemId, WorkItemMember, PROVIDER_ACCOUNT_SCHEMA_VERSION,
        SUBSCRIPTION_SCHEMA_VERSION, TASK_SPAN_SCHEMA_VERSION, TASK_VERIFICATION_SCHEMA_VERSION,
        USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION, WORK_ITEM_SCHEMA_VERSION,
    };
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[test]
    fn privacy_filter_preview_is_exposed_by_the_cli() {
        let cli = Cli::try_parse_from(["statsai", "privacy", "filter", "--preview"])
            .expect("parse privacy preview");
        assert!(matches!(cli.command, Command::Privacy(_)));
    }

    #[derive(Clone)]
    struct TestAdapter {
        provider: &'static str,
        discovered: Vec<SourceLocation>,
        candidates: Vec<ScanCandidateFile>,
        scan_result: statsai_adapters::AdapterScan,
        probe_result: Option<VerifiedSourceState>,
        scan_calls: Option<Arc<Mutex<u64>>>,
    }

    impl ProviderAdapter for TestAdapter {
        fn id(&self) -> &'static str {
            "test"
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
            Ok(self.candidates.clone())
        }

        fn probe_verified_source_state(
            &self,
            _source: &SourceLocation,
        ) -> Result<VerifiedSourceObservation> {
            Ok(self
                .probe_result
                .clone()
                .or_else(|| self.scan_result.verified_source_state.clone())
                .map(Box::new)
                .map(VerifiedSourceObservation::Verified)
                .unwrap_or(VerifiedSourceObservation::Unavailable))
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            if let Some(scan_calls) = &self.scan_calls {
                let mut calls = scan_calls.lock().expect("scan call mutex");
                *calls += 1;
            }
            Ok(self.scan_result.clone())
        }
    }

    struct AccountEvidenceTrackingAdapter {
        source: SourceLocation,
        candidate: ScanCandidateFile,
        event: UsageEvent,
        collect_calls: Arc<Mutex<u64>>,
    }

    impl ProviderAdapter for AccountEvidenceTrackingAdapter {
        fn id(&self) -> &'static str {
            "test-account-evidence"
        }

        fn version(&self) -> &'static str {
            "0"
        }

        fn provider(&self) -> &'static str {
            "codex"
        }

        fn discover(&self) -> Vec<SourceLocation> {
            vec![self.source.clone()]
        }

        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(vec![self.candidate.clone()])
        }

        fn collect_account_evidence(
            &self,
            _source: &SourceLocation,
            _checkpoints: &[statsai_core::AccountEvidenceCheckpointV1],
        ) -> Result<AccountEvidenceScan> {
            *self.collect_calls.lock().expect("collect calls") += 1;
            Ok(AccountEvidenceScan::default())
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            Ok(statsai_adapters::AdapterScan {
                events: vec![self.event.clone()],
                ..statsai_adapters::AdapterScan::default()
            })
        }
    }

    fn seed_test_account_evidence(
        store: &Store,
        source: &SourceLocation,
        observed_at: DateTime<Utc>,
    ) {
        let account_id = ProviderAccountId("account-source-cleanup".to_string());
        store
            .upsert_account_identity_observations(&[statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: "identity-source-cleanup".to_string(),
                provider: source.provider.clone(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(account_id.clone()),
                provider_user_id_hash: Some("a".repeat(64)),
                email_hash: None,
                conversation_id_hash: Some("b".repeat(64)),
                turn_id_hash: None,
                observed_at,
                evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
                confidence: Confidence::High,
                auth_mode: Some("chatgpt".to_string()),
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "c".repeat(64),
                record_fingerprint: "d".repeat(64),
            }])
            .expect("identity evidence");
        store
            .upsert_account_plan_observations(&[statsai_core::AccountPlanObservationV1 {
                schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: "plan-source-cleanup".to_string(),
                provider: source.provider.clone(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(account_id.clone()),
                raw_plan_name: "pro".to_string(),
                plan_name: "Pro".to_string(),
                observed_at,
                active_from: None,
                active_until: None,
                is_current_snapshot: false,
                evidence_kind: statsai_core::AccountEvidenceKind::QuotaStatus,
                confidence: Confidence::High,
                parser_version: "test.v1".to_string(),
                artifact_path_hash: "c".repeat(64),
                record_fingerprint: "e".repeat(64),
            }])
            .expect("plan evidence");
        store
            .upsert_conversation_account_bindings(&[statsai_core::ConversationAccountBindingV1 {
                schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION
                    .to_string(),
                binding_id: "binding-source-cleanup".to_string(),
                provider: source.provider.clone(),
                source_id: source.source_id.clone(),
                provider_account_id: account_id,
                conversation_id_hash: "b".repeat(64),
                turn_id_hash: None,
                observed_at,
                evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
                confidence: Confidence::High,
            }])
            .expect("conversation evidence");
    }

    #[test]
    fn canonicalization_skips_accounts_without_surviving_evidence() {
        let store = Store::in_memory().expect("store");
        let mut evidence = AccountEvidenceScan::default();
        evidence
            .accounts
            .push(statsai_adapters::ObservedProviderAccount {
                provider_user_id: Some("unchanged-provider-user".to_string()),
                email: Some("unchanged@example.test".to_string()),
                plan_name: None,
                observed_at: Utc
                    .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
                    .single()
                    .expect("date"),
            });

        retain_accounts_referenced_by_account_evidence("codex", &HashMap::new(), &mut evidence);
        canonicalize_account_evidence(&store, "codex", &mut evidence)
            .expect("canonicalize account evidence");

        assert!(evidence.accounts.is_empty());
        assert!(store.list_accounts().expect("accounts").is_empty());
    }

    fn plan_observation_fixture(
        source_id: &SourceId,
        account_id: Option<&ProviderAccountId>,
        plan: &str,
        observed_at: DateTime<Utc>,
        evidence_kind: statsai_core::AccountEvidenceKind,
    ) -> statsai_core::AccountPlanObservationV1 {
        statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_plan_observation_id(
                source_id,
                account_id,
                plan,
                observed_at,
                evidence_kind,
            ),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            provider_account_id: account_id.cloned(),
            raw_plan_name: plan.to_ascii_lowercase(),
            plan_name: plan.to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: false,
            evidence_kind,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "a".repeat(64),
            record_fingerprint: "b".repeat(64),
        }
    }

    #[test]
    fn account_plans_report_the_newest_observation_per_account() {
        let store = Store::in_memory().expect("store");
        let source_id = SourceId("plans-source".to_string());
        let first = ProviderAccountId("acct-first".to_string());
        let second = ProviderAccountId("acct-second".to_string());
        let at = |day: u32| {
            Utc.with_ymd_and_hms(2026, 8, day, 12, 0, 0)
                .single()
                .expect("date")
        };
        // An older snapshot that still claims to be current: the newest observation must win over
        // it, which is the whole reason this reports `latest_observation` and not a derived plan.
        let mut stale_current_snapshot = plan_observation_fixture(
            &source_id,
            Some(&first),
            "Free",
            at(1),
            statsai_core::AccountEvidenceKind::AuthSnapshot,
        );
        stale_current_snapshot.is_current_snapshot = true;
        // A legacy row that kept the subscription's own provider casing.
        let mut non_canonical_provider = plan_observation_fixture(
            &source_id,
            Some(&second),
            "Pro",
            at(5),
            statsai_core::AccountEvidenceKind::LegacyLocalAuth,
        );
        non_canonical_provider.provider = "Codex".to_string();
        store
            .upsert_account_plan_observations(&[
                stale_current_snapshot,
                plan_observation_fixture(
                    &source_id,
                    Some(&first),
                    "Plus",
                    at(9),
                    statsai_core::AccountEvidenceKind::QuotaStatus,
                ),
                non_canonical_provider,
                // Evidence that never resolved to an account still has to be visible: dropping it
                // would hide plan history the operator can still act on.
                plan_observation_fixture(
                    &source_id,
                    None,
                    "Team",
                    at(3),
                    statsai_core::AccountEvidenceKind::AuthSnapshot,
                ),
            ])
            .expect("seed plan observations");

        let report =
            account_plan_evidence_report(&store, Some("codex"), None, false).expect("plan report");

        assert_eq!(report.len(), 3);
        let plan_for = |account: Option<&str>| {
            report
                .iter()
                .find(|entry| entry["provider_account_id"].as_str() == account)
                .map(|entry| entry["latest_observation"]["plan_name"].as_str().unwrap())
        };
        // The newest observation wins over an older one that still claims to be the current
        // snapshot, which is what `latest_observation` promises and a derived plan would not.
        assert_eq!(plan_for(Some("acct-first")), Some("Plus"));
        // Reached only through case-insensitive matching: this account's sole observation is
        // stored as `Codex`.
        assert_eq!(plan_for(Some("acct-second")), Some("Pro"));
        assert_eq!(plan_for(None), Some("Team"));
        let second_entry = report
            .iter()
            .find(|entry| entry["provider_account_id"] == "acct-second")
            .expect("second account entry");
        assert_eq!(
            second_entry["provider"], "codex",
            "a non-canonical stored provider is reported under its canonical name"
        );
        assert_eq!(
            second_entry["observation_count"], 1,
            "the case variant groups with its canonical provider rather than forming its own entry"
        );
        let first_entry = report
            .iter()
            .find(|entry| entry["provider_account_id"] == "acct-first")
            .expect("first account entry");
        assert_eq!(first_entry["observation_count"], 2);
        // Without `--all` the payload stays a summary.
        assert!(first_entry.get("observations").is_none());

        let detailed = account_plan_evidence_report(&store, Some("codex"), Some(&first), true)
            .expect("detailed plan report");
        assert_eq!(detailed.len(), 1, "the account filter selects one account");
        let observations = detailed[0]["observations"]
            .as_array()
            .expect("observations array");
        assert_eq!(observations.len(), 2);
        // Oldest first, so the newest is what `latest_observation` reports.
        assert_eq!(observations[0]["plan_name"], "Free");
        assert_eq!(observations[1]["plan_name"], "Plus");

        assert!(
            account_plan_evidence_report(&store, Some("claude_code"), None, false)
                .expect("other provider")
                .is_empty(),
            "the provider filter excludes other providers"
        );
    }

    #[test]
    fn known_account_aliases_are_applied_before_evidence_deduplication() {
        let store = Store::in_memory().expect("store");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
            .single()
            .expect("date");
        upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("owner@example.test"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(observed_at),
            },
        )
        .expect("email-only account");
        let source_id = SourceId("alias-dedup-source".to_string());
        let raw_account_id = provider_account_id_from_identity(
            "codex",
            Some("provider-user-1"),
            Some("owner@example.test"),
        )
        .expect("detected account id");
        let raw_plan = statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_plan_observation_id(
                &source_id,
                Some(&raw_account_id),
                "plus",
                observed_at,
                statsai_core::AccountEvidenceKind::AuthSnapshot,
            ),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            provider_account_id: Some(raw_account_id.clone()),
            raw_plan_name: "plus".to_string(),
            plan_name: "Plus".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "a".repeat(64),
            record_fingerprint: "b".repeat(64),
        };
        let raw_binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: conversation_account_binding_id(
                &source_id,
                &"c".repeat(64),
                None,
                &raw_account_id,
            ),
            provider: "codex".to_string(),
            source_id: source_id.clone(),
            provider_account_id: raw_account_id,
            conversation_id_hash: "c".repeat(64),
            turn_id_hash: None,
            observed_at,
            evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
        };
        let raw_scan = AccountEvidenceScan {
            accounts: vec![statsai_adapters::ObservedProviderAccount {
                provider_user_id: Some("provider-user-1".to_string()),
                email: Some("owner@example.test".to_string()),
                plan_name: None,
                observed_at,
            }],
            plan_observations: vec![raw_plan],
            conversation_bindings: vec![raw_binding],
            ..AccountEvidenceScan::default()
        };

        let mut first_scan = raw_scan.clone();
        canonicalize_account_evidence(&store, "codex", &mut first_scan)
            .expect("canonicalize first scan");
        store
            .upsert_account_plan_observations(&first_scan.plan_observations)
            .expect("store canonical plan");
        store
            .upsert_conversation_account_bindings(&first_scan.conversation_bindings)
            .expect("store canonical binding");

        let mut repeated_scan = raw_scan;
        let known_account_aliases =
            canonicalize_known_account_evidence(&store, "codex", &mut repeated_scan)
                .expect("canonicalize known aliases");
        store
            .retain_unseen_account_evidence(
                &source_id,
                &mut repeated_scan.identity_observations,
                &mut repeated_scan.plan_observations,
                &mut repeated_scan.conversation_bindings,
            )
            .expect("filter canonical evidence");
        assert!(repeated_scan.plan_observations.is_empty());
        assert!(repeated_scan.conversation_bindings.is_empty());

        repeated_scan
            .identity_observations
            .push(statsai_core::AccountIdentityObservationV1 {
                schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION
                    .to_string(),
                observation_id: "identity-provider-user-enrichment".to_string(),
                provider: "codex".to_string(),
                source_id,
                provider_account_id: known_account_aliases.values().next().cloned(),
                provider_user_id_hash: Some("provider-user-hash".to_string()),
                email_hash: Some("email-hash".to_string()),
                conversation_id_hash: None,
                turn_id_hash: None,
                observed_at,
                evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
                confidence: Confidence::High,
                auth_mode: Some("chatgpt".to_string()),
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "auth_json".to_string(),
                artifact_path_hash: "d".repeat(64),
                record_fingerprint: "e".repeat(64),
            });
        retain_accounts_referenced_by_account_evidence(
            "codex",
            &known_account_aliases,
            &mut repeated_scan,
        );
        assert_eq!(
            repeated_scan.accounts.len(),
            1,
            "the account carrying a newly learned provider ID must survive canonical alias remapping"
        );
        canonicalize_account_evidence(&store, "codex", &mut repeated_scan)
            .expect("enrich canonical account");
        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].provider_user_id.as_deref(),
            Some("provider-user-1")
        );
    }

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

    struct InterruptingArchiveAdapter;

    impl ProviderAdapter for InterruptingArchiveAdapter {
        fn id(&self) -> &'static str {
            "interrupting-archive-test"
        }

        fn version(&self) -> &'static str {
            "0"
        }

        fn provider(&self) -> &'static str {
            "archive_test"
        }

        fn discover(&self) -> Vec<SourceLocation> {
            Vec::new()
        }

        fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
            Ok(Vec::new())
        }

        fn scan(
            &self,
            _source: &SourceLocation,
            _options: &ScanOptions,
        ) -> Result<statsai_adapters::AdapterScan> {
            Ok(statsai_adapters::AdapterScan::default())
        }

        fn collect_archive(
            &self,
            _source: &SourceLocation,
            selected_cache_keys: Option<&HashSet<String>>,
        ) -> Result<statsai_adapters::ArchiveScan> {
            let selected = selected_cache_keys
                .and_then(|keys| keys.iter().next())
                .context("selected archive cache key")?;
            if selected == "second" {
                bail!("synthetic archive interruption");
            }
            let mut scan = statsai_adapters::ArchiveScan::default();
            scan.diagnostics.files_scanned = 1;
            Ok(scan)
        }
    }

    #[test]
    fn archive_collection_commits_each_candidate_before_the_next() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "archive_test",
            "interrupting-archive-test",
            "0",
            Path::new("/tmp/archive-test"),
            LocationOrigin::Configured,
        );
        let candidates = [
            ScanCandidateFile {
                path: PathBuf::from("first"),
                cache_key: "first".to_string(),
                cache_signature: "signature-first".to_string(),
                compatible_cache_signatures: Vec::new(),
            },
            ScanCandidateFile {
                path: PathBuf::from("second"),
                cache_key: "second".to_string(),
                cache_signature: "signature-second".to_string(),
                compatible_cache_signatures: Vec::new(),
            },
        ];
        let entries = scan_file_state_entries(&candidates);

        let result = collect_archive_source_entries(
            &store,
            &InterruptingArchiveAdapter,
            &source,
            &candidates,
            &entries,
            false,
        );
        assert!(result.is_err());

        let pending = store
            .pending_archive_import_entries(&source.source_id, &entries)
            .expect("pending archive entries");
        assert_eq!(pending, vec![entries[1].clone()]);
    }

    /// Files are reconstructed on several threads but must be handed back in
    /// the order they were listed: two files can describe the same
    /// conversation, and which record wins must not depend on scheduling.
    #[test]
    fn archive_group_parsing_preserves_file_order() {
        struct OrderedArchiveAdapter;

        impl ProviderAdapter for OrderedArchiveAdapter {
            fn id(&self) -> &'static str {
                "ordered-archive-test"
            }
            fn version(&self) -> &'static str {
                "0"
            }
            fn provider(&self) -> &'static str {
                "archive_test"
            }
            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }
            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(Vec::new())
            }
            fn scan(
                &self,
                _source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                Ok(statsai_adapters::AdapterScan::default())
            }
            fn collect_archive(
                &self,
                _source: &SourceLocation,
                selected_cache_keys: Option<&HashSet<String>>,
            ) -> Result<statsai_adapters::ArchiveScan> {
                let selected = selected_cache_keys
                    .and_then(|keys| keys.iter().next())
                    .context("selected archive cache key")?;
                // The earlier files are the slow ones, so a run that returned
                // results as they arrived would reorder them.
                let index: u64 = selected.parse().context("cache key index")?;
                std::thread::sleep(std::time::Duration::from_millis(40 - index * 4));
                let mut scan = statsai_adapters::ArchiveScan::default();
                scan.diagnostics.files_scanned = index;
                Ok(scan)
            }
        }

        let source = SourceLocation::local_adapter(
            "archive_test",
            "ordered-archive-test",
            "0",
            Path::new("/tmp/archive-order-test"),
            LocationOrigin::Configured,
        );
        let entries = (0..10)
            .map(|index| ScanFileStateEntry {
                cache_key: index.to_string(),
                cache_signature: format!("signature-{index}"),
            })
            .collect::<Vec<_>>();

        let scans = parse_archive_group(
            &OrderedArchiveAdapter,
            &source,
            &entries,
            &vec![0; entries.len()],
        );

        let order = scans
            .into_iter()
            .map(|scan| scan.expect("collected archive").diagnostics.files_scanned)
            .collect::<Vec<_>>();
        assert_eq!(order, (0..10).collect::<Vec<_>>());
    }

    /// A transcript's size on disk says nothing about how much content it
    /// materializes, so reconstruction stops taking new files once it is
    /// holding enough. The files it did not reach must come back for the
    /// caller to collect next, never be reported as done.
    #[test]
    fn archive_group_parsing_stops_before_holding_too_much_content() {
        struct HeavyArchiveAdapter;

        impl ProviderAdapter for HeavyArchiveAdapter {
            fn id(&self) -> &'static str {
                "heavy-archive-test"
            }
            fn version(&self) -> &'static str {
                "0"
            }
            fn provider(&self) -> &'static str {
                "archive_test"
            }
            fn discover(&self) -> Vec<SourceLocation> {
                Vec::new()
            }
            fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
                Ok(Vec::new())
            }
            fn scan(
                &self,
                _source: &SourceLocation,
                _options: &ScanOptions,
            ) -> Result<statsai_adapters::AdapterScan> {
                Ok(statsai_adapters::AdapterScan::default())
            }
            fn collect_archive(
                &self,
                _source: &SourceLocation,
                _selected_cache_keys: Option<&HashSet<String>>,
            ) -> Result<statsai_adapters::ArchiveScan> {
                // A tiny record naming an artifact that materializes far
                // larger than the file it came from.
                let mut conversation = ArchiveConversation {
                    schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
                    conversation_id: "conv_heavy".to_string(),
                    provider: "archive_test".to_string(),
                    source_id: statsai_core::SourceId("heavy".to_string()),
                    native_conversation_id: "heavy".to_string(),
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
                };
                let item_id = "item_heavy".to_string();
                conversation.items.push(statsai_core::ArchiveItem {
                    item_id: item_id.clone(),
                    native_item_id: None,
                    source_record_id: None,
                    ordinal: 0,
                    kind: statsai_core::ArchiveItemKind::Message,
                    role: None,
                    created_at: None,
                    model: None,
                    tool_name: None,
                    tool_call_id: None,
                    status: None,
                    usage: None,
                    parts_authoritative: true,
                    parts: vec![statsai_core::ArchiveContentPart::text(
                        statsai_core::archive_content_id(&item_id, 0),
                        0,
                        ArchiveContentKind::Text,
                        "x".repeat(ARCHIVE_COLLECTION_RETAINED_BYTES / 4),
                    )],
                });
                // Held long enough that every worker has tried to claim before
                // any capacity is released.
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut scan = statsai_adapters::ArchiveScan::default();
                scan.conversations.push(conversation);
                Ok(scan)
            }
        }

        let source = SourceLocation::local_adapter(
            "archive_test",
            "heavy-archive-test",
            "0",
            Path::new("/tmp/archive-heavy-test"),
            LocationOrigin::Configured,
        );
        let entries = (0..ARCHIVE_COLLECTION_GROUP_FILES)
            .map(|index| ScanFileStateEntry {
                cache_key: index.to_string(),
                cache_signature: format!("signature-{index}"),
            })
            .collect::<Vec<_>>();
        // A quarter of the budget each, so only a few may be outstanding.
        let source_bytes =
            vec![ARCHIVE_COLLECTION_RETAINED_BYTES as u64 / 4; ARCHIVE_COLLECTION_GROUP_FILES];

        let scans = parse_archive_group(&HeavyArchiveAdapter, &source, &entries, &source_bytes);

        // Every worker reaches the budget before any of them finishes, which is
        // exactly when a check that does not reserve lets all of them through.
        assert!(!scans.is_empty(), "no file was reconstructed");
        assert!(
            scans.len() <= 4,
            "budget did not gate concurrent claims: {} files were taken",
            scans.len()
        );
    }

    #[test]
    fn provider_aliases_match_canonical_provider() {
        assert!(provider_matches("claude_code", "claude"));
        assert!(provider_matches("claude-code", "claude_code"));
        assert!(provider_matches("codex", "codex"));
        assert_eq!(
            canonical_provider("claude").expect("provider"),
            "claude_code"
        );
        assert_eq!(canonical_provider_name("claude-code"), Some("claude_code"));
        assert_eq!(canonical_provider_name("grok"), Some("grok_build"));
        assert_eq!(canonical_provider_name("open-code"), Some("opencode"));
        assert_eq!(
            canonical_conversation_provider_filter(Some("claude")).expect("archive provider"),
            Some("claude_code")
        );
        assert_eq!(
            canonical_conversation_provider_filter(Some("grok")).expect("archive provider"),
            Some("grok_build")
        );
        assert_eq!(
            canonical_conversation_provider_filter(Some("open-code")).expect("archive provider"),
            Some("opencode")
        );
        assert!(canonical_conversation_provider_filter(Some("unknown")).is_err());
    }

    #[test]
    fn sync_sanitization_removes_record_level_evidence() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/.codex"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let mut event = test_event("codex", &source, now, None, TokenParts::total(100));
        event.source.source_record_id = Some("/tmp/.codex/sessions/log.jsonl:12".to_string());
        event.project = Some(ProjectInfo {
            project_id: "project-event-path-only".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("event-path-hash".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
        });
        event.parse_evidence = Some(ParseEvidence {
            event_key_version: "test.v1".to_string(),
            source_file_path_hash: Some("hash".to_string()),
            source_line_number: Some(12),
            source_record_id: Some("/tmp/.codex/sessions/log.jsonl:12".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::ManualHint,
        });

        let mut summary = test_summary("codex", &source, now, 100, None);
        summary.source.source_record_id = Some("reported_jul11.json:daily:2025-07-11".to_string());
        summary.parse_evidence = event.parse_evidence.clone();
        summary.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });

        let event = sanitize_event_for_sync(event);
        let summary = sanitize_summary_for_sync(summary);

        assert!(event.source.source_record_id.is_none());
        let event_evidence = event.parse_evidence.expect("event evidence");
        assert!(event_evidence.source_record_id.is_none());
        assert!(event_evidence.source_line_number.is_none());
        assert_eq!(
            event_evidence.source_file_path_hash.as_deref(),
            Some("hash")
        );
        let event_project = event.project.expect("path-only event project");
        assert_eq!(
            event_project.path_label.as_deref(),
            Some("/Users/example/Documents/Codex/2026-05-29/hi")
        );
        assert!(event.privacy.contains_file_paths);

        assert!(summary.source.source_record_id.is_none());
        let summary_evidence = summary.parse_evidence.expect("summary evidence");
        assert!(summary_evidence.source_record_id.is_none());
        assert!(summary_evidence.source_line_number.is_none());
        assert_eq!(
            summary_evidence.source_file_path_hash.as_deref(),
            Some("hash")
        );
        let project = summary.project.expect("repo-backed project");
        assert_eq!(project.repo_remote_hash.as_deref(), Some("repo-hash"));
        assert_eq!(project.repo_label.as_deref(), Some("owner/repo"));
        assert_eq!(project.path_hash.as_deref(), Some("path-hash"));
        assert_eq!(
            project.path_label.as_deref(),
            Some("/Users/example/work/ai-stats")
        );
        assert!(summary.privacy.contains_file_paths);
    }

    #[test]
    fn replace_matching_summaries_targets_reported_imports_only() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "external-report",
            None,
        );
        let local_source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0",
            Path::new("/tmp/.claude"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let mut reported = test_summary("claude_code", &source, now, 100, None);
        reported.source.source_kind = SourceKind::ExternalReport;
        reported.metadata.summary_format = "external_daily".to_string();
        let mut local = test_summary("claude_code", &local_source, now, 200, None);
        local.source.source_kind = SourceKind::LocalSummary;
        local.metadata.summary_format = "external_daily".to_string();
        store.upsert_summary(&reported).expect("reported summary");
        store.upsert_summary(&local).expect("local summary");

        let record = ReportedUsageSummaryRecord {
            source,
            summary: reported.clone(),
        };
        let report = ReportedImportReport {
            path: PathBuf::from("reported_usage_summaries.json"),
            records: vec![ReportedImportRecord {
                record,
                legacy_replacement_source_ids: Vec::new(),
            }],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");
        assert_eq!(matches, vec![reported.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_is_scoped_to_source_and_period() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "reported-file-a",
            None,
        );
        let other_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::ExternalReport,
            "reported-usage-summary",
            "0",
            "reported-file-b",
            None,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");

        let mut matching = test_summary("claude_code", &source, now, 100, None);
        matching.source.source_kind = SourceKind::ExternalReport;
        matching.metadata.summary_format = "external_daily".to_string();
        matching.period_start = Some(now - Duration::days(1));
        matching.period_end = Some(now);

        let mut same_file_different_day = test_summary("claude_code", &source, now, 200, None);
        same_file_different_day.summary_id =
            summary_id("claude_code", &source.source_id, "other-day");
        same_file_different_day.source.source_kind = SourceKind::ExternalReport;
        same_file_different_day.metadata.summary_format = "external_daily".to_string();
        same_file_different_day.period_start = Some(now - Duration::days(2));
        same_file_different_day.period_end = Some(now - Duration::days(1));

        let mut same_period_different_file =
            test_summary("claude_code", &other_source, now, 300, None);
        same_period_different_file.source.source_kind = SourceKind::ExternalReport;
        same_period_different_file.metadata.summary_format = "external_daily".to_string();
        same_period_different_file.period_start = matching.period_start;
        same_period_different_file.period_end = matching.period_end;

        store.upsert_summary(&matching).expect("matching summary");
        store
            .upsert_summary(&same_file_different_day)
            .expect("same file different day");
        store
            .upsert_summary(&same_period_different_file)
            .expect("same period different file");

        let incoming = ReportedUsageSummaryRecord {
            source,
            summary: matching.clone(),
        };
        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![ReportedImportRecord {
                record: incoming,
                legacy_replacement_source_ids: Vec::new(),
            }],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![matching.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_matches_legacy_alias_formats_after_canonicalization() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: Some("screenshot:2025-07-11".to_string()),
            evidence_path: Some("/tmp/user-report.png".to_string()),
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let mut legacy = incoming.record.summary.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        store
            .upsert_source(&incoming.record.source)
            .expect("source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming.clone()],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![legacy.summary_id]);
    }

    #[test]
    fn replace_matching_summaries_matches_legacy_alias_formats_without_evidence() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: None,
            evidence_path: None,
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let mut legacy = incoming.record.summary.clone();
        let legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:user_reported_usage:acct-personal:ccusage_daily",
            None,
        );
        assert_ne!(legacy_source.source_id, incoming.record.source.source_id);
        legacy.source_id = legacy_source.source_id.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        legacy.source.source_path_hash = legacy_source.path_hash.clone();
        store.upsert_source(&legacy_source).expect("legacy source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let other_legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:other_report:acct-personal:ccusage_daily",
            None,
        );
        let mut other_legacy = legacy.clone();
        other_legacy.summary_id = summary_id(
            "claude_code",
            &other_legacy_source.source_id,
            "other-source-same-period",
        );
        other_legacy.source_id = other_legacy_source.source_id.clone();
        other_legacy.source.source_path_hash = other_legacy_source.path_hash.clone();
        store
            .upsert_source(&other_legacy_source)
            .expect("other legacy source");
        store
            .upsert_summary(&other_legacy)
            .expect("other legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming.clone()],
            warnings: Vec::new(),
        };

        let matches = matching_reported_summary_ids(&store, &[report]).expect("matches");

        assert_eq!(matches, vec![legacy.summary_id]);
        store
            .delete_summaries(&matches)
            .expect("delete legacy summary");
        assert_eq!(
            delete_orphaned_legacy_reported_sources(
                &store,
                &[ReportedImportReport {
                    path: PathBuf::from("reported-file-a.json"),
                    records: vec![incoming],
                    warnings: Vec::new(),
                }]
            )
            .expect("delete legacy source"),
            1
        );
        assert!(store
            .source(&legacy_source.source_id)
            .expect("legacy source")
            .is_none());
        assert!(store
            .source(&other_legacy_source.source_id)
            .expect("other legacy source")
            .is_some());
    }

    #[test]
    fn import_migrates_legacy_alias_summary_without_replace() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: None,
            evidence_path: None,
            report_format: "manual_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let canonical_source = incoming.record.source.clone();
        let canonical_source_id = incoming.record.source.source_id.clone();
        let canonical_summary_id = incoming.record.summary.summary_id.clone();
        let legacy_source = SourceLocation::reported_usage(
            "claude_code",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "claude_code:user_reported_usage:acct-personal:ccusage_daily",
            None,
        );
        let mut legacy = incoming.record.summary.clone();
        legacy.summary_id = summary_id("claude_code", &legacy_source.source_id, "legacy-alias");
        legacy.source_id = legacy_source.source_id.clone();
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        legacy.source.source_path_hash = legacy_source.path_hash.clone();
        let provider_account_id = ProviderAccountId("acct-personal".to_string());
        let legacy_assignment = test_assignment(
            &legacy_source,
            &provider_account_id,
            Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0)
                .single()
                .expect("assignment start"),
            None,
            Utc.with_ymd_and_hms(2025, 7, 12, 0, 0, 0)
                .single()
                .expect("assignment updated"),
        );
        let mut existing_canonical_assignment = test_assignment(
            &canonical_source,
            &provider_account_id,
            legacy_assignment.started_at,
            Some(
                Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0)
                    .single()
                    .expect("canonical assignment end"),
            ),
            Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
                .single()
                .expect("canonical assignment updated"),
        );
        existing_canonical_assignment.record_source = IdentitySource::SourceConfig;
        existing_canonical_assignment.verified_at = Some(
            Utc.with_ymd_and_hms(2025, 7, 20, 0, 0, 0)
                .single()
                .expect("canonical assignment verified"),
        );
        store
            .upsert_source(&canonical_source)
            .expect("canonical source");
        store
            .upsert_source_account_assignment(&existing_canonical_assignment)
            .expect("canonical assignment");
        store.upsert_source(&legacy_source).expect("legacy source");
        store.upsert_summary(&legacy).expect("legacy summary");
        store
            .upsert_source_account_assignment(&legacy_assignment)
            .expect("legacy assignment");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        };

        import_reported_summary_records(&store, &[report], false, false, false).expect("import");

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary_id, canonical_summary_id);
        assert_eq!(summaries[0].source_id, canonical_source_id);
        assert!(store
            .source(&legacy_source.source_id)
            .expect("legacy source")
            .is_none());
        assert!(store
            .source(&canonical_source.source_id)
            .expect("canonical source")
            .is_some());
        assert!(store
            .list_source_account_assignments_for_source(&legacy_source.source_id)
            .expect("legacy assignments")
            .is_empty());
        let canonical_assignments = store
            .list_source_account_assignments_for_source(&canonical_source.source_id)
            .expect("canonical assignments");
        assert_eq!(canonical_assignments.len(), 1);
        assert_eq!(
            canonical_assignments[0].provider_account_id,
            provider_account_id
        );
        assert_eq!(
            canonical_assignments[0].started_at,
            legacy_assignment.started_at
        );
        assert_eq!(
            canonical_assignments[0].ended_at,
            existing_canonical_assignment.ended_at
        );
        assert_eq!(
            canonical_assignments[0].record_source,
            existing_canonical_assignment.record_source
        );
        assert_eq!(
            canonical_assignments[0].verified_at,
            existing_canonical_assignment.verified_at
        );
    }

    #[test]
    fn import_migrates_evidence_backed_legacy_alias_summary_without_replace() {
        let store = Store::in_memory().expect("store");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "claude_code".to_string(),
            provider_account_id: Some("acct-personal".to_string()),
            provider_user_id: None,
            email: None,
            account_label: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: Some("screenshot:2025-07-11".to_string()),
            evidence_path: Some("/tmp/user-report.png".to_string()),
            report_format: "ccusage_daily".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(
                Utc.with_ymd_and_hms(2025, 7, 11, 23, 59, 59)
                    .single()
                    .expect("end"),
            ),
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        let canonical_summary_id = incoming.record.summary.summary_id.clone();
        let mut legacy = incoming.record.summary.clone();
        legacy.summary_id = summary_id(
            "claude_code",
            &incoming.record.source.source_id,
            "legacy-evidence-alias",
        );
        legacy.metadata.summary_format = "ccusage_daily".to_string();
        legacy.source.source_type = "ccusage_daily".to_string();
        store
            .upsert_source(&incoming.record.source)
            .expect("source");
        store.upsert_summary(&legacy).expect("legacy summary");

        let report = ReportedImportReport {
            path: PathBuf::from("reported-file-a.json"),
            records: vec![incoming],
            warnings: Vec::new(),
        };

        import_reported_summary_records(&store, &[report], false, false, false).expect("import");

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].summary_id, canonical_summary_id);
        assert_eq!(summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn import_overlays_estimated_cost_when_store_ruleset_is_already_current() {
        let store = Store::in_memory().expect("store");
        store
            .ensure_current_pricing()
            .expect("apply current ruleset");
        let noop = store.ensure_current_pricing().expect("already current");
        assert!(noop.already_current);

        let period_end = Utc
            .with_ymd_and_hms(2026, 7, 29, 23, 59, 59)
            .single()
            .expect("end");
        let input = ReportedUsageSummaryInput {
            schema_version: "reported_usage_summary_input.v1".to_string(),
            provider: "codex".to_string(),
            provider_account_id: None,
            provider_user_id: None,
            email: None,
            account_label: None,
            source_kind: SourceKind::ExternalReport,
            source_name: "legacy_catalog_export".to_string(),
            evidence_id: Some("legacy-catalog:2026-07-29".to_string()),
            evidence_path: None,
            report_format: "manual_period_summary".to_string(),
            report_version: Some("manual.v1".to_string()),
            period_start: Some(
                Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0)
                    .single()
                    .expect("start"),
            ),
            period_end: Some(period_end),
            observed_at: None,
            model: Some(ModelInfo {
                name: Some("codex-auto-review".to_string()),
                normalized_name: Some("codex-auto-review".to_string()),
                provider_model_id: Some("codex-auto-review".to_string()),
                speed: None,
                reasoning_level: None,
                reasoning_level_raw: None,
            }),
            usage: UsageCounts {
                input_tokens: Some(1_000_000),
                cache_creation_tokens: Some(1_000_000),
                cache_read_tokens: Some(1_000_000),
                output_tokens: Some(1_000_000),
                total_tokens: Some(4_000_000),
                ..UsageCounts::default()
            },
            cost: Some(CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: Some(1),
                provider_reported_usd: None,
                estimated_api_equivalent_micro_usd: Some(10_000),
                provider_reported_micro_usd: None,
                pricing_source: Some("official:stale".to_string()),
                pricing_version: Some("official:stale".to_string()),
                confidence: Confidence::Low,
            }),
            confidence: Some(Confidence::Medium),
        };

        let incoming = build_reported_import_record(input, "device").expect("incoming");
        assert_ne!(
            incoming.record.summary.cost.estimated_api_equivalent_usd,
            Some(1)
        );
        assert_eq!(
            incoming.record.summary.cost.pricing_version.as_deref(),
            Some(statsai_store::PRICING_CATALOG_VERSION)
        );
        assert!(incoming
            .record
            .summary
            .cost
            .estimated_api_equivalent_usd
            .is_some());
        let expected_usd = incoming.record.summary.cost.estimated_api_equivalent_usd;

        import_reported_summary_records(
            &store,
            &[ReportedImportReport {
                path: PathBuf::from("legacy-catalog.json"),
                records: vec![incoming],
                warnings: Vec::new(),
            }],
            false,
            false,
            false,
        )
        .expect("import");

        let stored = store.summaries().expect("summaries");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].cost.estimated_api_equivalent_usd, expected_usd);
        assert_eq!(
            stored[0].cost.pricing_version.as_deref(),
            Some(statsai_store::PRICING_CATALOG_VERSION)
        );
        let still_current = store.ensure_current_pricing().expect("still current");
        assert!(still_current.already_current);
        assert_eq!(
            store.summaries().expect("unchanged")[0]
                .cost
                .estimated_api_equivalent_usd,
            expected_usd
        );
    }

    #[test]
    fn configured_claude_projects_path_normalizes_to_config_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).expect("projects");

        let normalized =
            normalize_configured_source_path("claude_code", &projects).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn configured_codex_sessions_path_normalizes_to_codex_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");

        let normalized =
            normalize_configured_source_path("codex", &sessions).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn configured_opencode_db_path_normalizes_to_data_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("opencode.db");
        std::fs::write(&db, "").expect("db");

        let normalized =
            normalize_configured_source_path("opencode", &db).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn configured_grok_sessions_path_normalizes_to_home_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");

        let normalized =
            normalize_configured_source_path("grok-build", &sessions).expect("normalized path");

        assert_eq!(
            normalized,
            dir.path().canonicalize().expect("canonical dir")
        );
    }

    #[test]
    fn subscription_add_uses_canonical_provider_for_account_id() {
        let store = Store::in_memory().expect("store");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Add {
                    provider: "claude".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Pro".to_string(),
                    price: "20.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
                    paid_at: Some("2026-05-15".to_string()),
                    started_at: "2026-05-15".to_string(),
                    ended_at: None,
                },
            },
            &store,
        )
        .expect("subscription");

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].provider, "claude_code");
        assert_eq!(
            subscriptions[0].provider_account_id,
            provider_account_id_from_identity("claude_code", None, Some("personal@example.com"))
                .expect("account id")
        );
    }

    #[test]
    fn subscription_price_parses_exact_decimal_cents() {
        for (value, expected_cents) in [
            ("0", 0),
            ("20", 2_000),
            ("20.5", 2_050),
            ("20.05", 2_005),
            ("1000000.00", MAX_SUBSCRIPTION_PRICE_CENTS),
        ] {
            assert_eq!(
                value
                    .parse::<SubscriptionPrice>()
                    .expect("valid price")
                    .cents(),
                expected_cents
            );
        }
    }

    #[test]
    fn subscription_price_rejects_invalid_or_excessive_values() {
        for value in [
            "",
            "-1",
            "+1",
            "NaN",
            "inf",
            "1e3",
            ".50",
            "1.",
            "1.001",
            "1000000.01",
            "999999999999999999999999999999999999999999",
        ] {
            assert!(
                value.parse::<SubscriptionPrice>().is_err(),
                "price should be rejected: {value}"
            );
        }
    }

    #[test]
    fn subscription_currency_normalizes_three_letter_codes() {
        assert_eq!(
            "usd"
                .parse::<CurrencyCode>()
                .expect("currency")
                .into_string(),
            "USD"
        );
        for value in ["", "US", "USDD", "U1D", "💵"] {
            assert!(
                value.parse::<CurrencyCode>().is_err(),
                "currency should be rejected: {value}"
            );
        }
    }

    #[test]
    fn persist_source_upserts_into_store() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-preview-source"),
            LocationOrigin::Configured,
        );

        persist_source_after_preview(&store, &source).expect("persist");

        assert_eq!(store.list_sources().expect("sources").len(), 1);
    }

    #[test]
    fn configured_source_overrides_discovered_source_for_same_path() {
        let discovered = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-merge"),
            LocationOrigin::Default,
        );
        let configured = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-merge"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
    }

    #[test]
    fn disabled_configured_source_suppresses_matching_discovered_source() {
        let matching = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-disabled"),
            LocationOrigin::Default,
        );
        let unrelated = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-enabled"),
            LocationOrigin::Default,
        );
        let mut disabled = SourceLocation::local_adapter(
            "claude",
            "test",
            "0",
            Path::new("/tmp/claude-disabled"),
            LocationOrigin::Configured,
        );
        disabled.enabled = false;
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![matching, unrelated.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[disabled]);

        assert_eq!(sources, vec![unrelated]);
    }

    #[test]
    fn configured_parent_source_suppresses_discovered_child_source() {
        let discovered = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/statsai-claude/projects"),
            LocationOrigin::Default,
        );
        let configured = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/statsai-claude"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "claude_code",
            discovered: vec![discovered],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].location_origin, LocationOrigin::Configured);
        assert_eq!(
            sources[0].path_label.as_deref(),
            Some("/tmp/statsai-claude")
        );
    }

    #[test]
    fn codex_nested_source_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/.codex"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources =
            scan_sources_for_adapter(&adapter, std::slice::from_ref(&configured_child));
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/.codex")
        );
    }

    #[test]
    fn codex_nested_sessions_source_is_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/sessions"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources = scan_sources_for_adapter(&adapter, &[configured_child]);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
    }

    #[test]
    fn codex_source_under_nested_codex_root_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/.codex/sessions"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/.codex/sessions")
        );
    }

    #[test]
    fn codex_custom_named_nested_root_is_not_shadowed_by_parent_source() {
        let discovered_parent = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex"),
            LocationOrigin::Env,
        );
        let configured_child = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/statsai-codex/project-codex-home"),
            LocationOrigin::Configured,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![discovered_parent.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let mut sources = scan_sources_for_adapter(&adapter, &[configured_child]);
        sources.sort_by(|left, right| left.path_label.cmp(&right.path_label));

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path_label.as_deref(), Some("/tmp/statsai-codex"));
        assert_eq!(
            sources[1].path_label.as_deref(),
            Some("/tmp/statsai-codex/project-codex-home")
        );
    }

    #[test]
    fn non_local_sources_are_ignored_for_adapter_scans() {
        let configured_local = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-local"),
            LocationOrigin::Configured,
        );
        let configured_manual = SourceLocation::reported_usage(
            "codex",
            SourceKind::Manual,
            "reported-usage-summary",
            "0",
            "manual-note",
            None,
        );
        let adapter = TestAdapter {
            provider: "codex",
            discovered: Vec::new(),
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: None,
            scan_calls: None,
        };

        let sources =
            scan_sources_for_adapter(&adapter, &[configured_local.clone(), configured_manual]);

        assert_eq!(sources, vec![configured_local]);
    }

    #[test]
    fn connect_source_to_account_closes_existing_open_connection() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let first_start = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("first");
        let second_start = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("second");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("first@example.com"),
                label: None,
                started_at: first_start,
                ended_at: None,
            },
        )
        .expect("first connect");
        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("second@example.com"),
                label: None,
                started_at: second_start,
                ended_at: None,
            },
        )
        .expect("second connect");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].ended_at, Some(second_start));
        assert_eq!(assignments[1].started_at, second_start);
    }

    #[test]
    fn manual_source_reassignment_rebuilds_quota_plan_evidence() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-quota-plan-reassignment"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let mut quota = test_unattributed_quota_record(&source.source_id.0);
        quota.observation.status.plan_type = Some("pro".to_string());
        store
            .upsert_quota_observations(&[quota])
            .expect("quota observation");
        let first_start = Utc
            .with_ymd_and_hms(2026, 8, 1, 0, 0, 0)
            .single()
            .expect("first start");
        let second_start = Utc
            .with_ymd_and_hms(2026, 8, 15, 0, 0, 0)
            .single()
            .expect("second start");

        let first = connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("first-quota@example.com"),
                label: None,
                started_at: first_start,
                ended_at: None,
            },
        )
        .expect("first connection");
        store
            .rebuild_quota_plan_observations_for_source(&source.source_id)
            .expect("seed quota plan evidence");
        assert_eq!(
            store.account_plan_observations().expect("initial plan")[0].provider_account_id,
            Some(first.provider_account_id)
        );

        let second = connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("second-quota@example.com"),
                label: None,
                started_at: second_start,
                ended_at: None,
            },
        )
        .expect("second connection");

        let observations = store.account_plan_observations().expect("reassigned plan");
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].provider_account_id,
            Some(second.provider_account_id)
        );
        assert_eq!(
            observations[0].evidence_kind,
            statsai_core::AccountEvidenceKind::QuotaStatus
        );
    }

    #[test]
    fn connect_source_to_account_preserves_tail_when_replacing_finite_window() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect-tail"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let period_start = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("period start");
        let split_at = Utc
            .with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
            .single()
            .expect("split");
        let period_end = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("period end");
        let before_split = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .expect("before split");
        let after_split = Utc
            .with_ymd_and_hms(2026, 5, 20, 12, 0, 0)
            .single()
            .expect("after split");
        store
            .insert_event(&test_event(
                "codex",
                &source,
                before_split,
                None,
                TokenParts::total(1),
            ))
            .expect("before event");
        store
            .insert_event(&test_event(
                "codex",
                &source,
                after_split,
                None,
                TokenParts::total(1),
            ))
            .expect("after event");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("first@example.com"),
                label: None,
                started_at: period_start,
                ended_at: Some(period_end),
            },
        )
        .expect("first connect");
        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("second@example.com"),
                label: None,
                started_at: period_start,
                ended_at: Some(split_at),
            },
        )
        .expect("second connect");

        let first_account =
            provider_account_id_from_identity("codex", None, Some("first@example.com"))
                .expect("first account");
        let second_account =
            provider_account_id_from_identity("codex", None, Some("second@example.com"))
                .expect("second account");
        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 2);
        assert!(assignments.iter().any(|assignment| {
            assignment.provider_account_id == second_account
                && assignment.started_at == period_start
                && assignment.ended_at == Some(split_at)
        }));
        assert!(assignments.iter().any(|assignment| {
            assignment.provider_account_id == first_account
                && assignment.started_at == split_at
                && assignment.ended_at == Some(period_end)
        }));

        let events = store
            .events_for_source(&source.source_id)
            .expect("source events");
        assert_eq!(events.len(), 2);
        let before = events
            .iter()
            .find(|event| event.session.started_at == before_split)
            .expect("before event");
        let after = events
            .iter()
            .find(|event| event.session.started_at == after_split)
            .expect("after event");
        assert_eq!(before.provider_account_id, Some(second_account));
        assert_eq!(after.provider_account_id, Some(first_account));
    }

    #[test]
    fn connect_source_to_account_merges_same_account_and_backfills_boundary_events() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-connect-merge"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let original_start = Utc
            .with_ymd_and_hms(2026, 5, 28, 11, 31, 9)
            .single()
            .expect("original start");
        let extended_start = Utc
            .with_ymd_and_hms(2026, 5, 28, 0, 0, 0)
            .single()
            .expect("extended start");
        let boundary_event_at = Utc
            .with_ymd_and_hms(2026, 5, 28, 7, 23, 28)
            .single()
            .expect("boundary event");

        let event = test_event(
            "codex",
            &source,
            boundary_event_at,
            None,
            TokenParts::total(1),
        );
        store.insert_event(&event).expect("event");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("same-account@example.com"),
                label: None,
                started_at: original_start,
                ended_at: None,
            },
        )
        .expect("initial connect");

        connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("same-account@example.com"),
                label: None,
                started_at: extended_start,
                ended_at: None,
            },
        )
        .expect("extended connect");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].started_at, extended_start);

        let events = store
            .events_for_source(&source.source_id)
            .expect("source events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].provider_account_id,
            provider_account_id_from_identity("codex", None, Some("same-account@example.com"))
        );
    }

    #[test]
    fn apply_verified_source_state_reuses_existing_email_account() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verified-state"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let existing = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("existing@example.com"),
                label: Some("existing-alias".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("existing account");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let current_period_ends_at = Utc
            .with_ymd_and_hms(2026, 6, 29, 10, 12, 43)
            .single()
            .expect("current_period_ends_at");

        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("chatgpt-account-123".to_string()),
                email: Some("existing@example.com".to_string()),
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
        )
        .expect("apply verified state");

        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].provider_account_id,
            existing.provider_account_id
        );
        assert_eq!(
            accounts[0].provider_user_id.as_deref(),
            Some("chatgpt-account-123")
        );
        assert_eq!(accounts[0].plan_name, None);
        assert_eq!(accounts[0].verified_at, Some(verified_at));

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            existing.provider_account_id
        );
        assert_eq!(assignments[0].record_source, IdentitySource::LocalAuth);
        assert_eq!(assignments[0].verified_at, Some(verified_at));

        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
    }

    #[test]
    fn upsert_provider_account_rejects_conflicting_email_and_provider_user_id() {
        let store = Store::in_memory().expect("store");
        let email_account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("conflict@example.com"),
                label: Some("email".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("email account");
        let user_account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-conflict"),
                email: None,
                label: Some("user".to_string()),
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("user account");

        let error = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-conflict"),
                email: Some("conflict@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: None,
            },
        )
        .expect_err("conflicting identity");

        assert!(error
            .to_string()
            .contains("conflicting provider account identifiers"));
        let accounts = store.list_accounts().expect("accounts");
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|account| {
            account.provider_account_id == email_account.provider_account_id
                && account.provider_user_id.is_none()
                && account.email.as_deref() == Some("conflict@example.com")
        }));
        assert!(accounts.iter().any(|account| {
            account.provider_account_id == user_account.provider_account_id
                && account.provider_user_id.as_deref() == Some("acct-conflict")
                && account.email.is_none()
        }));
    }

    #[test]
    fn lookup_provider_account_does_not_create_orphans() {
        let store = Store::in_memory().expect("store");

        let error = resolve_existing_provider_account(
            &store,
            "codex",
            None,
            None,
            Some("typo@example.com"),
            None,
        )
        .expect_err("missing account");

        assert!(error
            .to_string()
            .contains("unknown provider account selector"));
        assert!(store.list_accounts().expect("accounts").is_empty());
    }

    #[test]
    fn provider_account_id_lookup_rejects_wrong_provider() {
        let store = Store::in_memory().expect("store");
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "claude_code",
                provider_user_id: None,
                email: Some("claude@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("account");

        let existing_error = resolve_existing_provider_account(
            &store,
            "codex",
            Some(&account.provider_account_id.0),
            None,
            None,
            None,
        )
        .expect_err("wrong existing provider");
        let create_error = resolve_or_create_provider_account(
            &store,
            "codex",
            Some(&account.provider_account_id.0),
            Some("codex-user"),
            None,
            None,
        )
        .expect_err("wrong create provider");

        assert!(existing_error
            .to_string()
            .contains("belongs to claude_code"));
        assert!(create_error.to_string().contains("belongs to claude_code"));
    }

    #[test]
    fn subscription_change_requires_active_existing_subscription() {
        let store = Store::in_memory().expect("store");
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: None,
                email: Some("change@example.com"),
                label: None,
                plan_name: None,
                identity_source: Some(IdentitySource::UserConfigured),
                verified_at: None,
            },
        )
        .expect("account");

        let error = subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Change {
                    provider: "codex".to_string(),
                    provider_account_id: Some(account.provider_account_id.0.clone()),
                    provider_user_id: None,
                    email: None,
                    label: None,
                    plan: "Pro".to_string(),
                    price: "200.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
                    paid_at: None,
                    started_at: "2026-06-01".to_string(),
                },
            },
            &store,
        )
        .expect_err("missing active subscription");

        assert!(error
            .to_string()
            .contains("subscription change requires an active subscription"));
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
        assert_eq!(store.list_accounts().expect("accounts").len(), 1);
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
    fn source_explain_distinguishes_inferred_blocked_and_unavailable_auth() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-source-explain-auth"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let blocked = explain_source_with_observation(
            &store,
            &source,
            Some(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            }),
        )
        .expect("blocked explanation");
        let unavailable = explain_source_with_observation(
            &store,
            &source,
            Some(&VerifiedSourceObservation::Unavailable),
        )
        .expect("unavailable explanation");
        let inferred = explain_source_with_observation(
            &store,
            &source,
            Some(&VerifiedSourceObservation::Inferred {
                identity: Box::new(VerifiedSourceState {
                    provider_user_id: Some("cached-account".to_string()),
                    email: Some("cached@example.com".to_string()),
                    account_label: None,
                    plan_name: None,
                    authenticated_at: None,
                    verified_at: None,
                    subscription: None,
                }),
                basis: SourceIdentityInference::CachedLocalProfile,
                settings_modified_at: None,
            }),
        )
        .expect("inferred explanation");

        assert_eq!(
            blocked.pointer("/detected_auth_state/status"),
            Some(&json!("attribution_blocked"))
        );
        assert_eq!(
            unavailable.pointer("/detected_auth_state/status"),
            Some(&json!("unavailable"))
        );
        assert_eq!(
            inferred.pointer("/detected_auth_state/status"),
            Some(&json!("inferred"))
        );
        assert_eq!(
            inferred.pointer("/detected_auth_state/state/basis"),
            Some(&json!("cached_local_profile"))
        );
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
    fn manual_only_source_ignores_verified_state_mutations() {
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-manual-only"),
            LocationOrigin::Configured,
        );
        source.verification_mode = SourceVerificationMode::ManualOnly;
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let verified_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 14, 56)
            .single()
            .expect("verified_at");
        let adapter = TestAdapter {
            provider: "codex",
            discovered: vec![source.clone()],
            candidates: Vec::new(),
            scan_result: statsai_adapters::AdapterScan::default(),
            probe_result: Some(VerifiedSourceState {
                provider_user_id: Some("acct-manual-only".to_string()),
                email: Some("manual-only@example.com".to_string()),
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
                    ended_at: None,
                    current_period_ends_at: None,
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
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

        assert!(store.list_accounts().expect("accounts").is_empty());
        assert!(store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments")
            .is_empty());
        assert!(store
            .list_subscriptions()
            .expect("subscriptions")
            .is_empty());
    }

    #[test]
    fn manual_only_source_does_not_collect_or_apply_account_evidence() {
        let store = Store::in_memory().expect("store");
        let source_root = "/tmp/codex-manual-only-evidence";
        let file_path = "/tmp/codex-manual-only-evidence/session.jsonl";
        let mut source = SourceLocation::local_adapter(
            "codex",
            "test-account-evidence",
            "0",
            Path::new(source_root),
            LocationOrigin::Configured,
        );
        source.verification_mode = SourceVerificationMode::ManualOnly;
        store.upsert_source(&source).expect("source");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 23, 10, 0, 0)
            .single()
            .expect("observed at");
        seed_test_account_evidence(&store, &source, observed_at);

        let mut event = test_scan_event(&source, file_path, observed_at, "manual-event", 100);
        event.session.local_session_id_hash = Some("b".repeat(64));
        let collect_calls = Arc::new(Mutex::new(0));
        let adapter = AccountEvidenceTrackingAdapter {
            source: source.clone(),
            candidate: test_scan_candidate(file_path, "manual-evidence-v1"),
            event,
            collect_calls: Arc::clone(&collect_calls),
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

        assert_eq!(*collect_calls.lock().expect("collect calls"), 0);
        let stored_event = store
            .events()
            .expect("events")
            .into_iter()
            .find(|item| item.source.source_record_id.as_deref() == Some("manual-event"))
            .expect("manual event");
        assert_eq!(stored_event.provider_account_id, None);
    }

    #[test]
    fn disabled_source_mode_closes_verified_linkages() {
        let store = Store::in_memory().expect("store");
        let mut source_location = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-disable-verification"),
            LocationOrigin::Configured,
        );
        source_location.verified_state_hash = Some("verified-state".to_string());
        store.upsert_source(&source_location).expect("source");
        let started_at = Utc::now() - Duration::days(1);
        let account = upsert_provider_account(
            &store,
            UpsertProviderAccountInput {
                provider: "codex",
                provider_user_id: Some("acct-disable"),
                email: Some("disable@example.com"),
                label: None,
                plan_name: Some("Plus".to_string()),
                identity_source: Some(IdentitySource::LocalAuth),
                verified_at: Some(started_at),
            },
        )
        .expect("account");
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: source_account_assignment_id(
                    &source_location.source_id,
                    &account.provider_account_id,
                    started_at,
                ),
                source_id: source_location.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                started_at,
                ended_at: None,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(started_at),
                created_at: started_at,
                updated_at: started_at,
            })
            .expect("assignment");
        store
            .upsert_subscription(&Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    "codex",
                    &account.provider_account_id,
                    "Plus",
                    started_at,
                ),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                renewal_day: None,
                started_at,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(started_at),
                notes: None,
            })
            .expect("subscription");
        seed_test_account_evidence(&store, &source_location, started_at);

        source(
            SourceCommand {
                command: SourceSubcommand::Mode {
                    source_id: Some(source_location.source_id.0.clone()),
                    path: None,
                    mode: "disabled".to_string(),
                },
            },
            &store,
            "device",
        )
        .expect("disable mode");

        let source = store
            .source(&source_location.source_id)
            .expect("source lookup")
            .expect("source exists");
        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        let subscriptions = store.list_subscriptions().expect("subscriptions");

        assert_eq!(source.verification_mode, SourceVerificationMode::Disabled);
        assert_eq!(source.verified_state_hash, None);
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].ended_at.is_some());
        assert_eq!(subscriptions.len(), 1);
        assert!(subscriptions[0].ended_at.is_some());
        assert!(store
            .account_identity_observations(Some(&source.source_id))
            .expect("identity evidence")
            .is_empty());
        assert!(store
            .account_plan_observations()
            .expect("plan evidence")
            .is_empty());
        assert!(store
            .conversation_account_bindings(Some(&source.source_id))
            .expect("conversation evidence")
            .is_empty());
    }

    #[test]
    fn apply_verified_source_state_does_not_override_conflicting_manual_connection() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-manual-wins"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let manual = connect_source_to_account(
            &store,
            ConnectSourceToAccountInput {
                source_id: &source.source_id,
                provider_account_id_value: None,
                provider_user_id: None,
                email: Some("manual@example.com"),
                label: Some("manual".to_string()),
                started_at,
                ended_at: None,
            },
        )
        .expect("manual connection");

        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("chatgpt-account-999".to_string()),
                email: Some("verified@example.com".to_string()),
                account_label: None,
                plan_name: Some("Plus".to_string()),
                authenticated_at: Some(started_at),
                verified_at: Some(started_at),
                subscription: Some(VerifiedSubscriptionState {
                    plan_name: "Plus".to_string(),
                    price: 2000,
                    currency: "USD".to_string(),
                    billing_period: BillingPeriod::Monthly,
                    paid_at: Some(started_at),
                    started_at,
                    ended_at: None,
                    current_period_ends_at: None,
                    status: SubscriptionStatus::Active,
                    verified_at: Some(started_at),
                }),
            }),
        )
        .expect("apply verified state");

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            manual.provider_account_id
        );
        assert_eq!(assignments[0].record_source, IdentitySource::UserConfigured);
    }

    #[test]
    fn source_remove_delete_data_retires_committed_metrics_without_any_traces() {
        let repository = tempfile::TempDir::new().expect("temporary repository");
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repository.path())
                .status()
                .expect("run git");
            assert!(status.success());
        }
        std::fs::write(repository.path().join("main.rs"), "fn main() {}\n").expect("write source");
        for args in [&["add", "main.rs"][..], &["commit", "-qm", "initial"]] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repository.path())
                .status()
                .expect("run git");
            assert!(status.success());
        }

        let store = Store::in_memory().expect("store");
        let committed_source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-committed-only"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&committed_source).expect("source");

        // Usage carries the project path, which is how committed churn is
        // discovered. This source has no archive and therefore no reconstructed
        // edits at all.
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 10, 0, 0)
            .single()
            .expect("now");
        seed_test_account_evidence(&store, &committed_source, now);
        let mut summary = test_summary("codex", &committed_source, now, 100, None);
        summary.project = Some(ProjectInfo {
            project_id: "project-committed-only".to_string(),
            project_label: None,
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: None,
            path_label: Some(repository.path().to_string_lossy().to_string()),
        });
        store.upsert_summaries(&[summary]).expect("summary");

        store
            .refresh_code_changes("device")
            .expect("measure committed churn");
        assert!(store.list_trace_edits().expect("traces").is_empty());
        assert_eq!(
            store
                .list_code_change_metrics(false)
                .expect("metrics before")
                .len(),
            1
        );

        // Removing the source deletes the usage that carried the project path,
        // so nothing references the repository any more. Rebuilding only when
        // traces were dropped left these metrics materialized and the
        // authoritative snapshot republishing them.
        source(
            SourceCommand {
                command: SourceSubcommand::Remove {
                    source_id: committed_source.source_id.0.clone(),
                    delete_data: true,
                },
            },
            &store,
            "device",
        )
        .expect("remove source");

        assert!(store
            .list_code_change_metrics(false)
            .expect("metrics after")
            .is_empty());
        assert!(store
            .account_identity_observations(Some(&committed_source.source_id))
            .expect("identity evidence after")
            .is_empty());
        assert!(store
            .account_plan_observations()
            .expect("plan evidence after")
            .is_empty());
        assert!(store
            .conversation_account_bindings(Some(&committed_source.source_id))
            .expect("conversation evidence after")
            .is_empty());
    }

    #[test]
    fn source_remove_delete_data_clears_task_spans_and_rebuilds_surviving_work_items() {
        let store = Store::in_memory().expect("store");
        let source_a = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-remove-a"),
            LocationOrigin::Configured,
        );
        let source_b = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-source-remove-b"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source_a).expect("source a");
        store.upsert_source(&source_b).expect("source b");

        let started_at_a = Utc
            .with_ymd_and_hms(2026, 6, 1, 10, 0, 0)
            .single()
            .expect("started_at_a");
        let started_at_b = started_at_a + Duration::days(10);
        let event_a = test_scan_event(
            &source_a,
            "/tmp/codex-source-remove-a/session.jsonl",
            started_at_a,
            "event-a",
            100,
        );
        let event_b = test_scan_event(
            &source_b,
            "/tmp/codex-source-remove-b/session.jsonl",
            started_at_b,
            "event-b",
            120,
        );
        store.insert_event(&event_a).expect("event a");
        store.insert_event(&event_b).expect("event b");

        let mut span_a = test_task_span(
            &source_a,
            "/tmp/codex-source-remove-a/session.jsonl",
            started_at_a,
            "span-a",
            "Implement source delete cleanup alpha",
            &event_a,
        );
        span_a.session_id = Some("session-a".to_string());
        let mut span_b = test_task_span(
            &source_b,
            "/tmp/codex-source-remove-b/session.jsonl",
            started_at_b,
            "span-b",
            "Implement source delete cleanup beta",
            &event_b,
        );
        span_b.session_id = Some("session-b".to_string());
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("task spans");
        store
            .rebuild_task_work_items_for_project_buckets(&BTreeSet::from([span_a
                .project_bucket
                .clone()]))
            .expect("rebuild");

        assert_eq!(store.task_spans().expect("task spans before").len(), 2);
        assert_eq!(store.work_items().expect("work items before").len(), 2);

        // Distinct days so each source contributes its own daily metric rather
        // than merging into one aggregate row.
        for (source_location, path, occurred_at) in [
            (
                &source_a,
                "/tmp/codex-source-remove-a/session.jsonl",
                started_at_a,
            ),
            (
                &source_b,
                "/tmp/codex-source-remove-b/session.jsonl",
                started_at_b,
            ),
        ] {
            let native_id = format!("thread-{}", source_location.source_id.0);
            let conversation_id = statsai_core::archive_conversation_id("codex", &native_id);
            let context = statsai_core::TraceEditContext {
                provider: "codex",
                source_id: &source_location.source_id,
                cache_key: path,
                conversation_id: &conversation_id,
                source_record_id: &format!("{path}:1"),
                occurred_at: Some(occurred_at),
                project: None,
                repository_path: None,
            };
            let edits = statsai_core::parse_full_file_write(
                &context,
                Path::new("src/lib.rs"),
                "one\ntwo\n",
                true,
            )
            .edits;
            store
                .store_archive_scan_with_code_changes(
                    &source_location.source_id,
                    &[statsai_core::ArchiveConversation {
                        schema_version: statsai_core::ARCHIVE_CONVERSATION_SCHEMA_VERSION
                            .to_string(),
                        conversation_id,
                        provider: "codex".to_string(),
                        source_id: source_location.source_id.clone(),
                        native_conversation_id: native_id,
                        title: None,
                        project: None,
                        started_at: Some(occurred_at),
                        updated_at: Some(occurred_at),
                        completeness: statsai_core::ArchiveCompleteness::Complete,
                        missing_content_count: 0,
                        missing_content_scope_id: None,
                        discarded_source_record_ids: Vec::new(),
                        superseded_conversation_ids: Vec::new(),
                        items: Vec::new(),
                    }],
                    &[statsai_store::ScanFileStateEntry {
                        cache_key: path.to_string(),
                        cache_signature: "signature".to_string(),
                    }],
                    &[],
                    &edits,
                    statsai_core::CoverageStatus::Complete,
                    &[],
                )
                .expect("seed trace edits");
        }
        store
            .refresh_code_changes("device")
            .expect("build code-change metrics");
        assert_eq!(store.list_trace_edits().expect("traces before").len(), 2);
        assert_eq!(
            store
                .list_code_change_metrics(false)
                .expect("metrics before")
                .len(),
            2
        );

        source(
            SourceCommand {
                command: SourceSubcommand::Remove {
                    source_id: source_a.source_id.0.clone(),
                    delete_data: true,
                },
            },
            &store,
            "device",
        )
        .expect("remove source");

        assert!(store
            .source(&source_a.source_id)
            .expect("source a lookup")
            .is_none());
        assert!(store
            .source(&source_b.source_id)
            .expect("source b lookup")
            .is_some());

        let spans = store.task_spans().expect("task spans after");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].source_id, source_b.source_id);
        assert_eq!(spans[0].span_id, span_b.span_id);

        let work_items = store.work_items().expect("work items after");
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].anchor_span_id, span_b.span_id);
        assert_eq!(work_items[0].total_tokens, 120);

        // The deleted source's reconstructed edits are gone, and the metrics
        // built from them are rebuilt now so the authoritative snapshot stops
        // republishing them.
        let traces = store.list_trace_edits().expect("traces after");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].source_id, source_b.source_id);
        // The import state goes with them, so re-adding the source imports
        // again rather than believing its files were already read.
        assert_eq!(
            store
                .archive_import_entry_count(&source_a.source_id)
                .expect("retired import state"),
            0
        );
        assert_eq!(
            store
                .archive_import_entry_count(&source_b.source_id)
                .expect("surviving import state"),
            1
        );
        // Deleting a source's data includes the archived copy of it.
        let conversations = store
            .list_archive_conversations(None, 10)
            .expect("remaining conversations");
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_id, source_b.source_id.0);
        assert_eq!(
            store
                .list_code_change_metrics(false)
                .expect("metrics after")
                .len(),
            1
        );
    }

    #[test]
    fn subscription_change_closes_existing_period() {
        let store = Store::in_memory().expect("store");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Add {
                    provider: "codex".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Plus".to_string(),
                    price: "20.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
                    paid_at: Some("2026-05-01".to_string()),
                    started_at: "2026-05-01".to_string(),
                    ended_at: None,
                },
            },
            &store,
        )
        .expect("subscription add");

        subscription(
            SubscriptionCommand {
                command: SubscriptionSubcommand::Change {
                    provider: "codex".to_string(),
                    provider_account_id: None,
                    provider_user_id: None,
                    email: Some("personal@example.com".to_string()),
                    label: None,
                    plan: "Pro".to_string(),
                    price: "200.00".parse().expect("price"),
                    currency: "USD".parse().expect("currency"),
                    paid_at: Some("2026-06-01".to_string()),
                    started_at: "2026-06-01".to_string(),
                },
            },
            &store,
        )
        .expect("subscription change");

        let subscriptions = store.list_subscriptions().expect("subscriptions");
        assert_eq!(subscriptions.len(), 2);
        assert!(subscriptions
            .iter()
            .any(|subscription| subscription.plan_name == "Plus"
                && subscription.ended_at
                    == Some(
                        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                            .single()
                            .expect("end")
                    )));
        assert!(
            subscriptions
                .iter()
                .any(|subscription| subscription.plan_name == "Pro"
                    && subscription.ended_at.is_none())
        );
    }

    #[test]
    fn active_subscription_treats_legacy_verified_cycle_rows_as_current_periods() {
        let store = Store::in_memory().expect("store");
        let account_id = provider_account_id("codex", "verified@example.com");
        let started_at = Utc
            .with_ymd_and_hms(2026, 4, 30, 7, 43, 17)
            .single()
            .expect("started_at");
        let period_end = Utc
            .with_ymd_and_hms(2026, 5, 30, 7, 43, 17)
            .single()
            .expect("period_end");
        store
            .upsert_subscription(&Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id("codex", &account_id, "Plus", started_at),
                provider: "codex".to_string(),
                provider_account_id: account_id.clone(),
                plan_name: "Plus".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: Some(started_at),
                renewal_day: Some(30),
                started_at,
                ended_at: Some(period_end),
                current_period_ends_at: Some(period_end),
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::LocalAuth,
                verified_at: Some(
                    Utc.with_ymd_and_hms(2026, 5, 3, 10, 54, 50)
                        .single()
                        .expect("verified_at"),
                ),
                notes: None,
            })
            .expect("legacy subscription");

        let active = active_subscription(
            &store,
            "codex",
            &account_id,
            Some("Plus"),
            Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
                .single()
                .expect("lookup"),
        )
        .expect("active subscription");

        assert_eq!(active.provider_account_id, account_id);
        assert_eq!(active.plan_name, "Plus");
    }

    #[test]
    fn preview_path_label_abbreviates_home_paths() {
        let Some(home) = home_dir() else {
            return;
        };
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            &home.join(".codex"),
            LocationOrigin::Default,
        );

        assert!(preview_path_label(&source).starts_with("~/.codex"));
    }

    #[test]
    fn dry_run_sync_does_not_write_file() {
        let store = Store::in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("batch.json");

        sync(
            SyncCommand {
                output: Some(output.clone()),
                dry_run: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert!(!output.exists());
    }

    #[test]
    fn dry_run_sync_does_not_persist_sync_preferences() {
        let store = Store::in_memory().expect("store");

        sync(
            SyncCommand {
                dry_run: true,
                include_projects: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync dry run");

        assert_eq!(
            store.sync_preferences().expect("sync preferences"),
            SyncPreferences::default()
        );
    }

    #[test]
    fn http_dry_run_does_not_require_auth_or_clear_sync_tracking() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        store
            .record_sync_success("http", &endpoint, "batch_local", &[], &[], None)
            .expect("sync success");
        let state_before = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");

        let previous_api_url = std::env::var("STATSAI_API_URL").ok();
        let previous_sync_token = std::env::var("STATSAI_SYNC_TOKEN").ok();
        std::env::set_var(
            "STATSAI_API_URL",
            format!("https://{}-dry-run-authless.invalid", std::process::id()),
        );
        std::env::remove_var("STATSAI_SYNC_TOKEN");

        let result = sync(
            SyncCommand {
                endpoint: Some(endpoint.clone()),
                dry_run: true,
                ..test_sync_command("http")
            },
            &store,
            "device",
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("STATSAI_API_URL", value);
        } else {
            std::env::remove_var("STATSAI_API_URL");
        }
        if let Some(value) = previous_sync_token {
            std::env::set_var("STATSAI_SYNC_TOKEN", value);
        } else {
            std::env::remove_var("STATSAI_SYNC_TOKEN");
        }

        result.expect("sync dry run");

        let state_after = store
            .sync_state("http", &endpoint)
            .expect("sync state")
            .expect("present");
        assert_eq!(state_after, state_before);
    }

    #[test]
    fn status_sync_does_not_persist_sync_preferences() {
        let store = Store::in_memory().expect("store");

        sync(
            SyncCommand {
                status: true,
                include_tasks: true,
                ..test_sync_command("file")
            },
            &store,
            "device",
        )
        .expect("sync status");

        assert_eq!(
            store.sync_preferences().expect("sync preferences"),
            SyncPreferences::default()
        );
    }

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
    fn http_sync_builds_rollup_batches_without_raw_events() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(!batch.summaries.is_empty());
        assert!(batch.summaries.iter().all(is_daily_rollup_summary));
    }

    #[test]
    fn http_sync_excludes_non_daily_stats_cache_summaries_from_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-stats-cache"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.metadata.summary_format = "claude_stats_cache".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(batch.summaries.is_empty());
    }

    #[test]
    fn http_sync_keeps_grok_build_summary_only_sessions_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "grok_build",
            "test",
            "0",
            Path::new("/tmp/grok-build-http-rollup"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 8, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("grok_build", &source, now, 500, None);
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.source.source_type = "build-session.json".to_string();
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(
            batch.summaries[0].metadata.summary_format,
            "grok_build_session_summary"
        );
    }

    #[test]
    fn http_sync_excludes_multi_day_external_daily_summaries_from_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-multi-day"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 5, 14, 23, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-multi-day");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert!(batch.summaries.is_empty());
    }

    #[test]
    fn http_sync_keeps_one_day_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_offset_local_day_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-offset-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 7, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 5, 14, 6, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "external-offset-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_dst_fallback_external_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-external-dst-fallback-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 11, 1, 7, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 11, 2, 7, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::ExternalReport;
        summary.metadata.summary_format = "external_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "external-dst-fallback-daily",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "external_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "manual-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_end() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-end"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = None;
        summary.observed_at = observed_at;
        summary.summary_id =
            summary_id("claude_code", &source.source_id, "manual-daily-missing-end");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_start() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-start"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let period_end = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("period_end");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 16, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = None;
        summary.period_end = Some(period_end);
        summary.observed_at = observed_at;
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "manual-daily-missing-start",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_one_day_manual_daily_summaries_without_period_bounds() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-daily-missing-bounds"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let observed_at = Utc
            .with_ymd_and_hms(2026, 5, 13, 12, 0, 0)
            .single()
            .expect("observed_at");
        let mut summary = test_summary("claude_code", &source, observed_at, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_daily".to_string();
        summary.period_start = None;
        summary.period_end = None;
        summary.observed_at = observed_at;
        summary.summary_id = summary_id(
            "claude_code",
            &source.source_id,
            "manual-daily-missing-bounds",
        );
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "manual_daily");
    }

    #[test]
    fn http_sync_keeps_legacy_ccusage_daily_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-ccusage-daily"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 13, 23, 59, 59)
            .single()
            .expect("now");
        let start = Utc
            .with_ymd_and_hms(2026, 5, 13, 0, 0, 0)
            .single()
            .expect("start");
        let mut summary = test_summary("claude_code", &source, now, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "ccusage_daily".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(now);
        summary.summary_id = summary_id("claude_code", &source.source_id, "ccusage-daily");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(batch.summaries[0].metadata.summary_format, "ccusage_daily");
    }

    #[test]
    fn http_sync_keeps_exact_manual_period_summaries_in_rollup_batches() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-rollup-manual-period"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2025, 9, 4, 0, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2025, 9, 9, 23, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("claude_code", &source, end, 500, None);
        summary.source.source_kind = SourceKind::Manual;
        summary.metadata.summary_format = "manual_period_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.summary_id = summary_id("claude_code", &source.source_id, "manual-period");
        store.upsert_summary(&summary).expect("summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert_eq!(
            batch.summaries[0].metadata.summary_format,
            "manual_period_summary"
        );
    }

    #[test]
    fn first_http_incremental_sync_sends_full_rollup_history_for_new_target() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-first-sync"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let existing_rollups = store
            .all_sync_rollup_summaries()
            .expect("all rollups for new target");
        assert_eq!(existing_rollups.len(), 1);

        store
            .mark_sync_rollups_synced(
                &existing_rollups
                    .iter()
                    .map(|summary| summary.summary_id.clone())
                    .collect::<Vec<_>>(),
            )
            .expect("clear dirty flags");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("dirty rollups")
            .is_empty());

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            since_last: true,
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");

        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert!(batch.events.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert!(is_daily_rollup_summary(&batch.summaries[0]));
    }

    #[test]
    fn incremental_http_sync_includes_repriced_rollups_without_full() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-reprice-sync"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 29, 12, 0, 0)
            .single()
            .expect("started_at");
        let mut event = test_event(
            "codex",
            &source,
            started_at,
            None,
            TokenParts {
                input: 1_000_000,
                cached_input: 1_000_000,
                output: 1_000_000,
                reasoning: 0,
                total: 4_000_000,
                cost: None,
            },
        );
        event.model = Some(ModelInfo {
            name: Some("codex-auto-review".to_string()),
            normalized_name: Some("codex-auto-review".to_string()),
            provider_model_id: Some("codex-auto-review".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        store.insert_event(&event).expect("legacy unpriced event");
        store.rebuild_sync_rollups().expect("rebuild");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        assert!(!command.full);
        assert!(!command.rebuild_rollups);
        let target = sync_target(&command).expect("target");
        let (initial_batch, initial_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_mode, SyncPayloadMode::Rollups);
        assert_eq!(initial_batch.summaries.len(), 1);
        assert!(initial_batch.summaries[0]
            .cost
            .estimated_api_equivalent_usd
            .is_none());
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        let (repeat_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("repeat batch");
        assert!(
            repeat_batch.summaries.is_empty(),
            "synced rollups must stay unpublished until pricing changes them"
        );

        let report = store.ensure_current_pricing().expect("automatic reprice");
        assert_eq!(report.changed_events, 1);
        assert_eq!(report.refreshed_rollups, 1);

        let (incremental_batch, incremental_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
        assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
        assert_eq!(incremental_batch.summaries.len(), 1);
        assert!(is_daily_rollup_summary(&incremental_batch.summaries[0]));
        assert_eq!(
            incremental_batch.summaries[0]
                .cost
                .estimated_api_equivalent_usd,
            store
                .events()
                .expect("repriced events")
                .into_iter()
                .next()
                .expect("one event")
                .cost
                .estimated_api_equivalent_usd
        );
        assert!(incremental_batch.summaries[0]
            .cost
            .estimated_api_equivalent_usd
            .is_some());
    }

    #[test]
    fn incremental_http_sync_includes_repriced_passthrough_summaries_without_full() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-reprice-passthrough"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let start = Utc
            .with_ymd_and_hms(2026, 7, 28, 0, 0, 0)
            .single()
            .expect("start");
        let end = Utc
            .with_ymd_and_hms(2026, 7, 29, 23, 59, 59)
            .single()
            .expect("end");
        let mut summary = test_summary("codex", &source, end, 4_000_000, None);
        summary.source.source_kind = SourceKind::LocalAdapter;
        summary.source.source_type = "build-session.json".to_string();
        summary.metadata.summary_format = "grok_build_session_summary".to_string();
        summary.period_start = Some(start);
        summary.period_end = Some(end);
        summary.model = Some(ModelInfo {
            name: Some("codex-auto-review".to_string()),
            normalized_name: Some("codex-auto-review".to_string()),
            provider_model_id: Some("codex-auto-review".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        });
        summary.usage = UsageCounts {
            input_tokens: Some(1_000_000),
            cache_creation_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            total_tokens: Some(4_000_000),
            ..UsageCounts::default()
        };
        store.upsert_summary(&summary).expect("passthrough summary");

        let command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        assert!(!command.full);
        let target = sync_target(&command).expect("target");
        let (initial_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_batch.summaries.len(), 1);
        assert!(!is_daily_rollup_summary(&initial_batch.summaries[0]));
        assert!(initial_batch.summaries[0]
            .cost
            .estimated_api_equivalent_usd
            .is_none());
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        let (repeat_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("repeat batch");
        assert!(repeat_batch.summaries.is_empty());

        let report = store.ensure_current_pricing().expect("automatic reprice");
        assert_eq!(report.changed_summaries, 1);

        let (incremental_batch, incremental_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
        assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
        assert_eq!(incremental_batch.summaries.len(), 1);
        assert!(!is_daily_rollup_summary(&incremental_batch.summaries[0]));
        assert!(incremental_batch.summaries[0]
            .cost
            .estimated_api_equivalent_usd
            .is_some());
    }

    #[test]
    fn http_incremental_rollups_are_tracked_per_target() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-targets"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let account_id = provider_account_id("codex", "personal");
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("started_at");
        let first = test_event(
            "codex",
            &source,
            started_at,
            Some(account_id.clone()),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&first).expect("first event");
        store.rebuild_sync_rollups().expect("rebuild");

        let mut passthrough = test_summary(
            "grok_build",
            &source,
            started_at + Duration::minutes(30),
            70,
            Some(account_id.clone()),
        );
        passthrough.summary_id = summary_id("grok_build", &source.source_id, "session-summary");
        passthrough.source.source_kind = SourceKind::LocalAdapter;
        passthrough.source.source_type = "build-session.json".to_string();
        passthrough.metadata.summary_format = "grok_build_session_summary".to_string();
        passthrough.period_start = Some(started_at);
        passthrough.period_end = Some(started_at + Duration::minutes(30));
        store
            .upsert_summary(&passthrough)
            .expect("passthrough summary");

        let local_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let local_target = sync_target(&local_command).expect("local target");
        let (local_batch, local_mode) =
            build_sync_batch(&local_command, &store, "device", &local_target)
                .expect("local initial batch");
        assert_eq!(local_mode, SyncPayloadMode::Rollups);
        assert_eq!(local_batch.summaries.len(), 2);
        assert!(local_batch.summaries.iter().any(is_daily_rollup_summary));
        assert!(local_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
        assert!(local_batch.authoritative_snapshot.is_some());
        record_rollup_sync_success(&store, "http", &local_target, &local_batch)
            .expect("record local sync");

        let (local_repeat_batch, local_repeat_mode) =
            build_sync_batch(&local_command, &store, "device", &local_target)
                .expect("local repeat batch");
        assert_eq!(local_repeat_mode, SyncPayloadMode::Rollups);
        assert!(
            local_repeat_batch.summaries.is_empty(),
            "plain HTTP sync should be incremental after a target was synced"
        );
        assert!(local_repeat_batch.authoritative_snapshot.is_none());

        let local_full_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            full: true,
            ..test_sync_command("http")
        };
        let (local_full_batch, local_full_mode) =
            build_sync_batch(&local_full_command, &store, "device", &local_target)
                .expect("local full batch");
        assert_eq!(local_full_mode, SyncPayloadMode::Rollups);
        assert_eq!(
            local_full_batch.summaries.len(),
            2,
            "--full should deliberately resend synced rollups and passthrough summaries"
        );
        assert!(local_full_batch
            .summaries
            .iter()
            .any(is_daily_rollup_summary));
        assert!(local_full_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
        assert!(local_full_batch.authoritative_snapshot.is_some());

        let local_incremental_command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            since_last: true,
            ..test_sync_command("http")
        };
        let (local_incremental_batch, _) =
            build_sync_batch(&local_incremental_command, &store, "device", &local_target)
                .expect("local incremental batch");
        assert!(local_incremental_batch.summaries.is_empty());
        assert!(local_incremental_batch.authoritative_snapshot.is_none());

        let second = test_event(
            "codex",
            &source,
            started_at + Duration::hours(1),
            Some(account_id),
            TokenParts {
                input: 20,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 25,
                cost: Some(20),
            },
        );
        store.insert_event(&second).expect("second event");
        assert_eq!(
            store
                .dirty_sync_rollup_summaries()
                .expect("dirty after second event")
                .len(),
            1
        );

        let remote_command = SyncCommand {
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let remote_target = sync_target(&remote_command).expect("remote target");
        let (remote_batch, remote_mode) =
            build_sync_batch(&remote_command, &store, "device", &remote_target)
                .expect("remote batch");
        assert_eq!(remote_mode, SyncPayloadMode::Rollups);
        assert_eq!(remote_batch.summaries.len(), 2);
        assert!(remote_batch.summaries.iter().any(is_daily_rollup_summary));
        assert!(remote_batch
            .summaries
            .iter()
            .any(|summary| summary.metadata.summary_format == "grok_build_session_summary"));
        record_rollup_sync_success(&store, "http", &remote_target, &remote_batch)
            .expect("record remote sync");
        assert!(store
            .dirty_sync_rollup_summaries()
            .expect("dirty after remote sync")
            .is_empty());

        let (local_catchup_batch, local_catchup_mode) =
            build_sync_batch(&local_incremental_command, &store, "device", &local_target)
                .expect("local catchup batch");
        assert_eq!(local_catchup_mode, SyncPayloadMode::Rollups);
        assert_eq!(local_catchup_batch.summaries.len(), 1);
        assert_eq!(
            local_catchup_batch.summaries[0].usage.total_tokens,
            Some(40)
        );
    }

    #[test]
    fn http_incremental_sync_sends_authoritative_snapshot_after_local_rollup_retirement() {
        let store = Store::in_memory().expect("store");
        let retired_source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-retired-rollup"),
            LocationOrigin::Configured,
        );
        let retained_source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-http-retained-rollup"),
            LocationOrigin::Configured,
        );
        store
            .upsert_source(&retired_source)
            .expect("retired source");
        store
            .upsert_source(&retained_source)
            .expect("retained source");

        let retired_event = test_event(
            "claude_code",
            &retired_source,
            Utc.with_ymd_and_hms(2026, 8, 2, 10, 0, 0)
                .single()
                .expect("started_at"),
            Some(provider_account_id("claude_code", "personal")),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 100,
                reasoning: 0,
                total: 115,
                cost: Some(10),
            },
        );
        let retained_event = test_event(
            "claude_code",
            &retained_source,
            Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0)
                .single()
                .expect("retained started_at"),
            Some(provider_account_id("claude_code", "personal")),
            TokenParts {
                input: 20,
                output: 10,
                cached_input: 200,
                reasoning: 0,
                total: 230,
                cost: Some(20),
            },
        );
        store.insert_event(&retired_event).expect("retired event");
        store.insert_event(&retained_event).expect("retained event");

        let command = SyncCommand {
            endpoint: Some("http://127.0.0.1:8787/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (initial_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_batch.summaries.len(), 2);
        assert!(initial_batch.authoritative_snapshot.is_some());
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        store
            .delete_events_for_sources(std::slice::from_ref(&retired_source.source_id))
            .expect("retire source events");
        assert_eq!(
            store
                .all_sync_rollup_summaries()
                .expect("remaining rollups")
                .len(),
            1
        );

        let (retirement_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("retirement batch");
        assert!(
            retirement_batch.summaries.is_empty(),
            "retirement-only reconciliation must not resend unchanged historical rollups"
        );
        assert!(
            retirement_batch.authoritative_snapshot.is_some(),
            "removing a previously synced rollup must send a server deletion signal"
        );
        record_rollup_sync_success(&store, "http", &target, &retirement_batch)
            .expect("record retirement sync");

        let (settled_batch, _) =
            build_sync_batch(&command, &store, "device", &target).expect("settled batch");
        assert!(settled_batch.summaries.is_empty());
        assert!(
            settled_batch.authoritative_snapshot.is_none(),
            "successful reconciliation must clear retired local sync tracking"
        );
    }

    #[test]
    fn http_rollup_sync_splits_large_summary_batches() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-chunks"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summaries: Vec<_> = (0..(HTTP_ROLLUP_SUMMARIES_PER_BATCH * 2 + 4))
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &source,
                    now + Duration::days(index as i64),
                    10,
                    None,
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_large".to_string(),
            device_id: "device".to_string(),
            sources: vec![source.clone()],
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

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].batch_id, "batch_large_sources_1");
        assert_eq!(chunks[1].batch_id, "batch_large_part_1_of_3");
        assert_eq!(chunks[2].batch_id, "batch_large_part_2_of_3");
        assert_eq!(chunks[3].batch_id, "batch_large_part_3_of_3");
        assert!(chunks[0].summaries.is_empty());
        assert_eq!(chunks[1].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert_eq!(chunks[2].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert_eq!(chunks[3].summaries.len(), 4);
        assert_eq!(chunks[0].sources.len(), 1);
        assert!(chunks[1].sources.is_empty());
        assert!(chunks[2].sources.is_empty());
        assert!(chunks[3].sources.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
    }

    #[test]
    fn http_rollup_sync_sends_authoritative_snapshot_after_data_chunks() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-snapshot"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_snapshot".to_string(),
            device_id: "device".to_string(),
            sources: vec![source.clone()],
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            account_plan_observations: Vec::new(),
            account_evidence_summaries: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: Vec::new(),
            task_verifications: Vec::new(),
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
                source_ids: vec![source.source_id.clone()],
                ..SyncAuthoritativeSnapshot::default()
            }),
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].batch_id, "batch_snapshot");
        assert_eq!(chunks[0].sources, vec![source.clone()]);
        assert!(chunks[0].authoritative_snapshot.is_none());
        assert_eq!(chunks[1].batch_id, "batch_snapshot_snapshot_1");
        assert!(chunks[1].sources.is_empty());
        let snapshot = chunks[1]
            .authoritative_snapshot
            .as_ref()
            .expect("snapshot chunk");
        assert_eq!(snapshot.snapshot_id, "batch_snapshot_authoritative");
        assert_eq!(snapshot.part_index, 0);
        assert_eq!(snapshot.part_count, 1);
        assert_eq!(snapshot.source_ids, vec![source.source_id]);
        assert_eq!(
            logical_http_rollup_batch_id(&chunks[1].batch_id),
            "batch_snapshot"
        );
    }

    #[test]
    fn http_rollup_sync_bounds_authoritative_snapshot_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summary_ids = (0..(HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH * 2 + 1))
            .map(|index| statsai_core::SummaryId(format!("summary-{index}")))
            .collect::<Vec<_>>();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_large_snapshot".to_string(),
            device_id: "device".to_string(),
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
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: Some(SyncAuthoritativeSnapshot {
                summary_ids,
                ..SyncAuthoritativeSnapshot::default()
            }),
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);
        let snapshot_chunks = chunks
            .iter()
            .filter_map(|chunk| chunk.authoritative_snapshot.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(snapshot_chunks.len(), 3);
        assert!(snapshot_chunks.iter().all(|snapshot| {
            snapshot.source_ids.len()
                + snapshot.provider_account_ids.len()
                + snapshot.source_account_assignment_ids.len()
                + snapshot.subscription_ids.len()
                + snapshot.summary_ids.len()
                <= HTTP_ROLLUP_SNAPSHOT_IDS_PER_BATCH
        }));
    }

    #[test]
    fn http_rollup_sync_splits_metadata_away_from_summaries() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let sources: Vec<_> = (0..17)
            .map(|index| {
                SourceLocation::local_adapter(
                    "codex",
                    format!("test-{index}"),
                    "0",
                    Path::new("/tmp/codex-http-metadata"),
                    LocationOrigin::Configured,
                )
            })
            .collect();
        let accounts: Vec<_> = (0..7)
            .map(|index| {
                test_account(
                    "codex",
                    Some(&format!("account-{index}")),
                    None,
                    None,
                    Some("Pro"),
                    now,
                )
            })
            .collect();
        let assignments: Vec<_> = (0..16)
            .map(|index| {
                test_assignment(
                    &sources[index],
                    &accounts[index % accounts.len()].provider_account_id,
                    now + Duration::days(index as i64),
                    None,
                    now,
                )
            })
            .collect();
        let subscriptions: Vec<_> = accounts
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, account)| Subscription {
                schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
                subscription_id: subscription_id(
                    "codex",
                    &account.provider_account_id,
                    &format!("pro-{index}"),
                    now,
                ),
                provider: "codex".to_string(),
                provider_account_id: account.provider_account_id.clone(),
                plan_name: "Pro".to_string(),
                price: 2000,
                currency: "USD".to_string(),
                billing_period: BillingPeriod::Monthly,
                paid_at: None,
                renewal_day: None,
                started_at: now,
                ended_at: None,
                current_period_ends_at: None,
                status: SubscriptionStatus::Active,
                record_source: IdentitySource::UserConfigured,
                verified_at: None,
                notes: None,
            })
            .collect();
        let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &sources[index % sources.len()],
                    now + Duration::days(index as i64),
                    10,
                    Some(accounts[index % accounts.len()].provider_account_id.clone()),
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-{index}"));
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata".to_string(),
            device_id: "device".to_string(),
            sources,
            accounts,
            source_account_assignments: assignments,
            subscriptions,
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

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].batch_id, "batch_metadata_sources_1");
        assert_eq!(chunks[1].batch_id, "batch_metadata_accounts_1");
        assert_eq!(chunks[2].batch_id, "batch_metadata_assignments_1");
        assert_eq!(chunks[3].batch_id, "batch_metadata_subscriptions_1");
        assert_eq!(chunks[4].batch_id, "batch_metadata_part_1_of_1");
        assert_eq!(chunks[0].sources.len(), 17);
        assert_eq!(chunks[1].accounts.len(), 7);
        assert_eq!(chunks[2].source_account_assignments.len(), 16);
        assert_eq!(chunks[3].subscriptions.len(), 3);
        assert_eq!(chunks[4].summaries.len(), HTTP_ROLLUP_SUMMARIES_PER_BATCH);
        assert!(chunks[..4].iter().all(|chunk| chunk.summaries.is_empty()));
        assert!(chunks[4].sources.is_empty());
        assert!(chunks[4].accounts.is_empty());
        assert!(chunks[4].source_account_assignments.is_empty());
        assert!(chunks[4].subscriptions.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
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
        let (since_last_resume, _) =
            build_sync_batch(&since_last_command, &store, "device", &target)
                .expect("since-last resume batch");
        assert_eq!(since_last_resume.summaries.len(), 26);
        assert!(since_last_resume.authoritative_snapshot.is_some());
    }

    #[test]
    fn failed_http_sync_without_ack_keeps_next_default_sync_full_history() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-no-partial-resume"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let event = test_event(
            "codex",
            &source,
            now,
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(10),
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (initial_batch, initial_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_mode, SyncPayloadMode::Rollups);
        assert_eq!(initial_batch.summaries.len(), 1);
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        store
            .record_sync_failure("http", &target)
            .expect("record failed sync");

        let state = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert!(state.pending_resume_batch_id.is_none());
        assert!(state.failure_count > 0);

        let (retry_batch, retry_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("retry batch");
        assert_eq!(retry_mode, SyncPayloadMode::Rollups);
        assert_eq!(retry_batch.summaries.len(), 1);

        let since_last_command = SyncCommand {
            endpoint: Some(endpoint),
            since_last: true,
            ..test_sync_command("http")
        };
        let (since_last_batch, since_last_mode) =
            build_sync_batch(&since_last_command, &store, "device", &target)
                .expect("since-last retry batch");
        assert_eq!(since_last_mode, SyncPayloadMode::Rollups);
        assert!(
            since_last_batch.summaries.is_empty(),
            "explicit --since-last should not force full history after an unacknowledged failure"
        );
    }

    #[test]
    fn full_dry_run_does_not_clear_pending_http_resume_state() {
        let store = Store::in_memory().expect("store");
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-full-dry-run-resume"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let event = test_event(
            "codex",
            &source,
            now,
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(10),
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let initial_command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&initial_command).expect("target");
        let (initial_batch, _) =
            build_sync_batch(&initial_command, &store, "device", &target).expect("initial batch");
        let expected_logical_batch_id = logical_http_rollup_batch_id(&initial_batch.batch_id);
        record_rollup_sync_chunk_success(
            &store,
            "http",
            &target,
            &expected_logical_batch_id,
            &initial_batch,
        )
        .expect("record partial sync state");

        let state = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(
            state.pending_resume_batch_id.as_deref(),
            Some(expected_logical_batch_id.as_str())
        );

        let full_dry_run_command = SyncCommand {
            endpoint: Some(endpoint),
            full: true,
            dry_run: true,
            ..test_sync_command("http")
        };
        let (dry_run_batch, dry_run_mode) =
            build_sync_batch(&full_dry_run_command, &store, "device", &target)
                .expect("full dry-run batch");
        assert_eq!(dry_run_mode, SyncPayloadMode::Rollups);
        assert_eq!(dry_run_batch.summaries.len(), 1);

        let state_after = store
            .sync_state("http", &target)
            .expect("sync state")
            .expect("present");
        assert_eq!(
            state_after.pending_resume_batch_id, state.pending_resume_batch_id,
            "dry-run must not mutate pending resume state"
        );
    }

    #[test]
    fn http_rollup_metadata_budget_retries_preserve_all_metadata_kinds() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let sources: Vec<_> = (0..4)
            .map(|index| {
                SourceLocation::local_adapter(
                    "codex",
                    format!("retry-source-{index}"),
                    "0",
                    Path::new("/tmp/codex-http-metadata-retry"),
                    LocationOrigin::Configured,
                )
            })
            .collect();
        let accounts: Vec<_> = (0..3)
            .map(|index| {
                test_account(
                    "codex",
                    Some(&format!("retry-account-{index}")),
                    None,
                    None,
                    Some("Pro"),
                    now,
                )
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata_retry".to_string(),
            device_id: "device".to_string(),
            sources: sources.clone(),
            accounts: accounts.clone(),
            source_account_assignments: vec![],
            subscriptions: vec![],
            account_plan_observations: vec![],
            account_evidence_summaries: vec![],
            events: vec![],
            summaries: vec![],
            task_buckets: vec![],
            task_verifications: vec![],
            code_change_metrics: vec![],
            quota_cycle_contributions: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.sources.len())
                .sum::<usize>(),
            sources.len()
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.accounts.len())
                .sum::<usize>(),
            accounts.len()
        );
        assert!(chunks
            .iter()
            .all(|chunk| chunk.source_account_assignments.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.subscriptions.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.summaries.is_empty()));
        assert!(chunks.iter().all(|chunk| chunk.events.is_empty()));
        assert!(chunks.iter().any(|chunk| !chunk.sources.is_empty()));
        assert!(chunks.iter().any(|chunk| !chunk.accounts.is_empty()));
    }

    fn test_quota_cycle_contributions(
        now: DateTime<Utc>,
        count: usize,
    ) -> Vec<statsai_core::QuotaCycleContributionV1> {
        (0..count)
            .map(|index| {
                let reset = now + chrono::Duration::days(7 * index as i64);
                statsai_core::QuotaCycleContributionV1 {
                    schema_version: statsai_core::QUOTA_CYCLE_CONTRIBUTION_SCHEMA_VERSION
                        .to_string(),
                    contribution_id: format!("quota_cycle_{index:032}"),
                    provider: "codex".to_string(),
                    provider_account_id: ProviderAccountId("acct".to_string()),
                    limit_id: Some("weekly".to_string()),
                    window_minutes: 10_080,
                    representative_reset: reset,
                    representative_reset_epoch_seconds: reset.timestamp(),
                    has_schedule_overlap: false,
                    daily_envelopes: Vec::new(),
                    boundary_slices: Vec::new(),
                }
            })
            .collect()
    }

    fn test_quota_only_sync_batch(now: DateTime<Utc>, count: usize) -> SyncBatch {
        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_quota_only".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            account_plan_observations: vec![],
            account_evidence_summaries: vec![],
            events: vec![],
            summaries: vec![],
            task_buckets: vec![],
            task_verifications: vec![],
            code_change_metrics: vec![],
            quota_cycle_contributions: test_quota_cycle_contributions(now, count),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    #[test]
    fn a_quota_only_batch_splits_into_strictly_smaller_chunks() {
        // Quota cycles carry nothing else, so the split has to make progress on
        // the quota collection itself. Counting them as metadata made
        // `has_non_quota_cycle_payload` true for this batch, so the splitter
        // peeled the quota off "the rest" and handed back the identical chunk
        // beside an empty one — which `should_retry_http_rollup_chunk_after_error`
        // then retried and split the same way, forever.
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_quota_only_sync_batch(now, 4);

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert!(chunks
            .iter()
            .all(|chunk| chunk.quota_cycle_contributions.len()
                < batch.quota_cycle_contributions.len()));
        assert!(chunks
            .iter()
            .all(|chunk| !chunk.quota_cycle_contributions.is_empty()));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.quota_cycle_contributions.len())
                .sum::<usize>(),
            batch.quota_cycle_contributions.len()
        );
    }

    #[test]
    fn splitting_sends_each_quota_cycle_exactly_once() {
        // Enough cycles to cross the metadata-per-batch limit once they were
        // wrongly counted as metadata. Past that point the metadata splitter
        // and the dedicated quota splitter both ran over the same batch, so
        // every contribution went out twice.
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut batch =
            test_quota_only_sync_batch(now, HTTP_ROLLUP_METADATA_RECORDS_PER_BATCH + 10);
        batch.sources = vec![SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-quota-once"),
            LocationOrigin::Configured,
        )];

        let sent = split_http_rollup_sync_batches(&batch)
            .iter()
            .flat_map(|chunk| chunk.quota_cycle_contributions.clone())
            .map(|contribution| contribution.contribution_id)
            .collect::<Vec<_>>();

        let unique = sent.iter().collect::<BTreeSet<_>>();
        assert_eq!(sent.len(), unique.len(), "sent: {sent:?}");
        assert_eq!(unique.len(), batch.quota_cycle_contributions.len());
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
    fn http_rollup_retry_splits_mixed_task_payloads() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_task_only_sync_batch(now, 1, 1);

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_too_large"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_buckets.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_verifications.len())
                .sum::<usize>(),
            1
        );
        assert!(chunks
            .iter()
            .all(|chunk| { chunk.task_buckets.is_empty() || chunk.task_verifications.is_empty() }));
    }

    #[test]
    fn http_rollup_retry_preserves_metrics_when_splitting_mixed_payloads() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut batch = test_task_only_sync_batch(now, 1, 1);
        batch.code_change_metrics = vec![test_code_change_metric(0, now)];

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_too_large"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.code_change_metrics.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_buckets.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_verifications.len())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn http_rollup_retry_splits_code_change_only_payloads() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut batch = test_task_only_sync_batch(now, 0, 0);
        batch.code_change_metrics = (0..3)
            .map(|index| test_code_change_metric(index, now))
            .collect();

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_d1_query_budget_exceeded"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.code_change_metrics.len())
                .sum::<usize>(),
            3
        );
    }

    #[test]
    fn http_rollup_retry_halves_task_only_bucket_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_task_only_sync_batch(now, 3, 0);

        assert!(should_retry_http_rollup_chunk_after_error(
            &batch,
            &anyhow::anyhow!(
                r#"sync endpoint returned HTTP 413: {{"error":"sync_batch_d1_query_budget_exceeded"}}"#
            ),
        ));

        let chunks = split_http_rollup_sync_batch_after_budget_error(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].task_buckets.len(), 2);
        assert_eq!(chunks[1].task_buckets.len(), 1);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.task_verifications.is_empty()));
    }

    #[test]
    fn record_sync_batch_success_marks_task_entities_synced_for_file_sink() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let store = Store::in_memory().expect("store");
        let batch = test_task_only_sync_batch(now, 1, 1);
        for bucket in &batch.task_buckets {
            store
                .replace_task_bucket_snapshot(bucket)
                .expect("seed task bucket snapshot");
        }
        for verification in &batch.task_verifications {
            store
                .merge_task_verification(verification)
                .expect("seed task verification");
        }

        record_sync_batch_success(&store, "file", "/tmp/statsai-sync-batch.json", &batch)
            .expect("record sync batch success");

        assert!(store
            .pending_task_bucket_snapshots_for_sync(
                "file",
                "/tmp/statsai-sync-batch.json",
                &batch.device_id,
                false,
                None,
            )
            .expect("pending task buckets")
            .is_empty());
        assert!(store
            .pending_task_verifications_for_sync("file", "/tmp/statsai-sync-batch.json")
            .expect("pending task verifications")
            .is_empty());
    }

    #[test]
    fn http_rollup_sends_metadata_before_task_chunks() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-metadata-before-task"),
            LocationOrigin::Configured,
        );
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_metadata_before_task".to_string(),
            device_id: "device".to_string(),
            sources: vec![source],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            account_plan_observations: vec![],
            account_evidence_summaries: vec![],
            events: vec![],
            summaries: vec![],
            task_buckets: test_task_only_sync_batch(now, 1, 0).task_buckets,
            task_verifications: vec![],
            code_change_metrics: vec![],
            quota_cycle_contributions: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].sources.len(), 1);
        assert!(chunks[0].task_buckets.is_empty());
        assert!(chunks[1].sources.is_empty());
        assert_eq!(chunks[1].task_buckets.len(), 1);
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
    fn http_rollup_sync_proactively_splits_batches_to_fit_d1_budget() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-budget"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let summaries: Vec<_> = (0..HTTP_ROLLUP_SUMMARIES_PER_BATCH)
            .map(|index| {
                let mut summary = test_summary(
                    "codex",
                    &source,
                    now + Duration::days((index * 31) as i64),
                    10,
                    None,
                );
                summary.summary_id = statsai_core::SummaryId(format!("summary-budget-{index}"));
                summary.project = Some(ProjectInfo {
                    project_id: format!("project-budget-{index}"),
                    project_label: Some(format!("Project {index}")),
                    repo_remote_hash: Some(format!("repo-hash-{index}")),
                    repo_label: Some(format!("owner/repo-{index}")),
                    branch_hash: None,
                    branch_label: None,
                    path_hash: Some(format!("path-hash-{index}")),
                    path_label: Some(format!("/tmp/project-{index}")),
                });
                summary
            })
            .collect();
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_budget".to_string(),
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

        let chunks = split_http_rollup_sync_batches(&batch);

        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.summaries.len())
                .sum::<usize>(),
            25
        );
        assert!(chunks.iter().all(|chunk| chunk.sources.is_empty()));
        assert!(chunks
            .iter()
            .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.summaries.len())
                .collect::<Vec<_>>(),
            vec![7, 6, 6, 6]
        );
    }

    #[test]
    fn code_change_metric_d1_estimate_matches_batched_backend_writes() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut one_metric = test_task_only_sync_batch(now, 0, 0);
        one_metric.code_change_metrics = vec![test_code_change_metric(0, now)];
        let mut many_metrics = one_metric.clone();
        many_metrics.code_change_metrics = (0..10_000)
            .map(|index| test_code_change_metric(index, now))
            .collect();

        assert_eq!(estimate_http_rollup_d1_queries(&one_metric), 7);
        assert_eq!(
            estimate_http_rollup_d1_queries(&many_metrics),
            estimate_http_rollup_d1_queries(&one_metric)
        );
    }

    #[test]
    fn v4_account_evidence_d1_estimate_includes_alias_lookup() {
        let now = Utc
            .with_ymd_and_hms(2026, 8, 23, 10, 12, 43)
            .single()
            .expect("date");
        let mut batch = test_task_only_sync_batch(now, 0, 0);
        let baseline = estimate_http_rollup_d1_queries(&batch);
        let account_id = ProviderAccountId("account-plan-estimate".to_string());
        batch.account_plan_observations = vec![statsai_core::AccountPlanProjectionV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_PROJECTION_SCHEMA_VERSION.to_string(),
            projection_id: "projection-plan-estimate".to_string(),
            semantic_fingerprint: "a".repeat(64),
            device_id: batch.device_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_id.clone(),
            raw_plan_name: "plus".to_string(),
            plan_name: "Plus".to_string(),
            observed_at: now,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
        }];
        batch.account_evidence_summaries = vec![statsai_core::AccountEvidenceSummaryV1 {
            schema_version: statsai_core::ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: "evidence-summary-estimate".to_string(),
            device_id: batch.device_id.clone(),
            provider: "codex".to_string(),
            provider_account_id: account_id,
            first_strong_observed_at: Some(now),
            last_strong_observed_at: Some(now),
            strong_observation_count: 1,
            directly_bound_conversations: 0,
            uncovered_gap_count: 0,
            conflict_count: 0,
            evidence_kinds: vec![statsai_core::AccountEvidenceKind::AuthSnapshot],
        }];

        assert_eq!(
            estimate_http_rollup_d1_queries(&batch),
            baseline + 5,
            "metadata, evidence-alias, ownership lookup, and possible cleanup must be budgeted"
        );
    }

    #[test]
    fn code_change_metrics_use_the_backends_batched_collection_limit() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut batch = test_task_only_sync_batch(now, 0, 0);
        batch.code_change_metrics = (0..1_000)
            .map(|index| test_code_change_metric(index, now))
            .collect();

        let chunks = split_http_rollup_sync_batches_without_snapshot(&batch);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].code_change_metrics.len(), 1_000);
    }

    #[test]
    fn http_rollup_project_counts_include_path_only_projects() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-path-only-project"),
            LocationOrigin::Configured,
        );
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let mut summary = test_summary("codex", &source, now, 10, None);
        summary.project = Some(ProjectInfo {
            project_id: "project-path-only".to_string(),
            project_label: Some("hi".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/Documents/Codex/2026-05-29/hi".to_string()),
        });
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_path_only_project".to_string(),
            device_id: "device".to_string(),
            sources: vec![],
            accounts: vec![],
            source_account_assignments: vec![],
            subscriptions: vec![],
            account_plan_observations: vec![],
            account_evidence_summaries: vec![],
            events: vec![],
            summaries: vec![summary],
            task_buckets: vec![],
            task_verifications: vec![],
            code_change_metrics: vec![],
            quota_cycle_contributions: vec![],
            authoritative_snapshot: None,
            created_at: now,
        };

        assert_eq!(http_rollup_project_count(&batch), 1);
        assert_eq!(http_rollup_project_location_count(&batch), 1);
    }

    fn test_task_only_sync_batch(
        now: DateTime<Utc>,
        bucket_count: usize,
        verification_count: usize,
    ) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-task-only"),
            LocationOrigin::Configured,
        );
        let task_buckets = (0..bucket_count)
            .map(|index| {
                let started_at = now + Duration::minutes(index as i64);
                let ended_at = started_at + Duration::minutes(5);
                let span_id = TaskSpanId(format!("span-task-{index}"));
                let work_item_id = WorkItemId(format!("work-task-{index}"));
                TaskBucketSnapshot {
                    project_bucket: format!("bucket-task-{index}"),
                    generated_at: ended_at,
                    applied_verification_cursor: None,
                    work_items: vec![WorkItem {
                        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                        work_item_id: work_item_id.clone(),
                        anchor_span_id: span_id.clone(),
                        tail_span_id: span_id.clone(),
                        project_bucket: format!("bucket-task-{index}"),
                        title: format!("Task {index}"),
                        normalized_title: format!("task {index}"),
                        status: TaskStatus::NeedsReview,
                        confidence: Confidence::Medium,
                        started_at,
                        ended_at,
                        duration_seconds: Some(300),
                        span_count: 1,
                        event_count: 1,
                        total_input_tokens: 10,
                        total_cache_creation_tokens: 0,
                        total_cache_read_tokens: 0,
                        total_output_tokens: 5,
                        total_reasoning_tokens: 0,
                        total_tokens: 15,
                        estimated_cost_usd: Some(25),
                        estimated_cost_micro_usd: Some(250_000),
                        providers: vec!["codex".to_string()],
                        issue_keys: Vec::new(),
                        repo_label: Some("statsai/repo".to_string()),
                        branch_labels: vec!["main".to_string()],
                        path_label: Some("/workspace/statsai".to_string()),
                        summary_preview: None,
                        todo_excerpt: None,
                        no_git: false,
                        cross_provider: false,
                        continuation_reasons: Vec::new(),
                        review_reasons: vec!["needs_review".to_string()],
                    }],
                    members: vec![WorkItemMember {
                        work_item_id,
                        span_id: span_id.clone(),
                        ordinal: 0,
                    }],
                    spans: vec![TaskSpan {
                        schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                        span_id,
                        provider: "codex".to_string(),
                        source_id: source.source_id.clone(),
                        span_kind: "codex_task".to_string(),
                        source_record_id: None,
                        source_file_path_hash: None,
                        summary_id: None,
                        session_id: Some(format!("session-task-{index}")),
                        thread_id: Some(format!("thread-task-{index}")),
                        title: format!("Task {index}"),
                        normalized_title: format!("task {index}"),
                        title_source: Some("thread_name".to_string()),
                        summary_preview: None,
                        todo_excerpt: None,
                        issue_keys: Vec::new(),
                        branch_family: Some("main".to_string()),
                        project_bucket: format!("bucket-task-{index}"),
                        project: None,
                        git: None,
                        usage: UsageCounts {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                            requests: Some(1),
                            ..UsageCounts::default()
                        },
                        estimated_cost_usd: Some(25),
                        estimated_cost_micro_usd: Some(250_000),
                        event_count: 1,
                        has_usage_evidence: true,
                        total_messages: 2,
                        user_messages: 1,
                        assistant_messages: 1,
                        developer_messages: 0,
                        linked_event_ids: Vec::new(),
                        confidence: Confidence::High,
                        is_meta: false,
                        started_at,
                        ended_at: Some(ended_at),
                        duration_seconds: Some(300),
                    }],
                }
            })
            .collect::<Vec<_>>();
        let task_verifications = (0..verification_count)
            .map(|index| {
                let timestamp = now + Duration::minutes(index as i64);
                TaskVerification {
                    schema_version: TASK_VERIFICATION_SCHEMA_VERSION.to_string(),
                    verification_id: TaskVerificationId(format!("tvf-task-{index}")),
                    action_key: format!("status:span-task-{index}"),
                    action: TaskVerificationAction::Reject {
                        work_item_id: WorkItemId(format!("work-task-{index}")),
                        anchor_span_id: TaskSpanId(format!("span-task-{index}")),
                        reason: TaskVerdict::Meta,
                    },
                    created_at: timestamp,
                    updated_at: timestamp,
                }
            })
            .collect::<Vec<_>>();

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            account_plan_observations: Vec::new(),
            account_evidence_summaries: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets,
            task_verifications,
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    fn test_code_change_metric(index: usize, now: DateTime<Utc>) -> statsai_core::CodeChangeMetric {
        statsai_core::CodeChangeMetric {
            schema_version: statsai_core::CODE_CHANGE_METRIC_SCHEMA_VERSION.to_string(),
            metric_id: format!("metric-retry-{index}"),
            device_id: "device".to_string(),
            day: now.date_naive(),
            project_id: None,
            repository_hash: None,
            commit_hash: None,
            kind: statsai_core::CodeChangeMetricKind::AgentEdit,
            counts: statsai_core::CodeLineCounts::default(),
            attribution_confidence: None,
            trace_coverage: statsai_core::CoverageStatus::Complete,
            git_coverage: statsai_core::CoverageStatus::Complete,
        }
    }

    fn test_dense_task_only_sync_batch(now: DateTime<Utc>, span_count: usize) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-dense-task-only"),
            LocationOrigin::Configured,
        );
        let spans = (0..span_count)
            .map(|index| {
                let started_at = now + Duration::minutes(index as i64);
                let ended_at = started_at + Duration::minutes(1);
                TaskSpan {
                    schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                    span_id: TaskSpanId(format!("dense-span-{index}")),
                    provider: "codex".to_string(),
                    source_id: source.source_id.clone(),
                    span_kind: "codex_task".to_string(),
                    source_record_id: None,
                    source_file_path_hash: None,
                    summary_id: None,
                    session_id: Some(format!("dense-session-{index}")),
                    thread_id: Some(format!("dense-thread-{index}")),
                    title: format!("Dense task {index}"),
                    normalized_title: format!("dense task {index}"),
                    title_source: Some("thread_name".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    issue_keys: Vec::new(),
                    branch_family: Some("main".to_string()),
                    project_bucket: "dense-bucket".to_string(),
                    project: Some(ProjectInfo {
                        project_id: "project-dense".to_string(),
                        project_label: Some("Dense".to_string()),
                        repo_remote_hash: Some("repo-dense".to_string()),
                        repo_label: Some("statsai/dense".to_string()),
                        branch_hash: Some("branch-dense".to_string()),
                        branch_label: Some("main".to_string()),
                        path_hash: Some("path-dense".to_string()),
                        path_label: Some("/workspace/dense".to_string()),
                    }),
                    git: None,
                    usage: UsageCounts {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(15),
                        requests: Some(1),
                        ..UsageCounts::default()
                    },
                    estimated_cost_usd: Some(25),
                    estimated_cost_micro_usd: Some(250_000),
                    event_count: 1,
                    has_usage_evidence: true,
                    total_messages: 2,
                    user_messages: 1,
                    assistant_messages: 1,
                    developer_messages: 0,
                    linked_event_ids: Vec::new(),
                    confidence: Confidence::High,
                    is_meta: false,
                    started_at,
                    ended_at: Some(ended_at),
                    duration_seconds: Some(60),
                }
            })
            .collect::<Vec<_>>();
        let members = spans
            .iter()
            .enumerate()
            .map(|(index, span)| WorkItemMember {
                work_item_id: WorkItemId("dense-work-item".to_string()),
                span_id: span.span_id.clone(),
                ordinal: index,
            })
            .collect::<Vec<_>>();
        let last_span = spans.last().expect("last dense span");

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_dense_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            account_plan_observations: Vec::new(),
            account_evidence_summaries: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets: vec![TaskBucketSnapshot {
                project_bucket: "dense-bucket".to_string(),
                generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                applied_verification_cursor: None,
                work_items: vec![WorkItem {
                    schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                    work_item_id: WorkItemId("dense-work-item".to_string()),
                    anchor_span_id: spans.first().expect("first dense span").span_id.clone(),
                    tail_span_id: last_span.span_id.clone(),
                    project_bucket: "dense-bucket".to_string(),
                    title: "Dense task".to_string(),
                    normalized_title: "dense task".to_string(),
                    status: TaskStatus::NeedsReview,
                    confidence: Confidence::Medium,
                    started_at: spans.first().expect("first dense span").started_at,
                    ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                    duration_seconds: Some((span_count as u64).saturating_mul(60)),
                    span_count: span_count as u64,
                    event_count: span_count as u64,
                    total_input_tokens: (span_count as u64).saturating_mul(10),
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_output_tokens: (span_count as u64).saturating_mul(5),
                    total_reasoning_tokens: 0,
                    total_tokens: (span_count as u64).saturating_mul(15),
                    estimated_cost_usd: Some((span_count as i64).saturating_mul(25)),
                    estimated_cost_micro_usd: Some((span_count as i64).saturating_mul(250_000)),
                    providers: vec!["codex".to_string()],
                    issue_keys: Vec::new(),
                    repo_label: Some("statsai/dense".to_string()),
                    branch_labels: vec!["main".to_string()],
                    path_label: Some("/workspace/dense".to_string()),
                    summary_preview: None,
                    todo_excerpt: None,
                    no_git: false,
                    cross_provider: false,
                    continuation_reasons: Vec::new(),
                    review_reasons: vec!["needs_review".to_string()],
                }],
                members,
                spans,
            }],
            task_verifications: Vec::new(),
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    fn test_multi_bucket_dense_task_only_sync_batch(
        now: DateTime<Utc>,
        bucket_count: usize,
        span_count_per_bucket: usize,
    ) -> SyncBatch {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-rollup-multi-dense-task-only"),
            LocationOrigin::Configured,
        );
        let task_buckets = (0..bucket_count)
            .map(|bucket_index| {
                let project_bucket = format!("dense-bucket-{bucket_index}");
                let work_item_id = WorkItemId(format!("dense-work-item-{bucket_index}"));
                let spans = (0..span_count_per_bucket)
                    .map(|span_index| {
                        let offset_minutes =
                            (bucket_index * span_count_per_bucket + span_index) as i64;
                        let started_at = now + Duration::minutes(offset_minutes);
                        let ended_at = started_at + Duration::minutes(1);
                        TaskSpan {
                            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
                            span_id: TaskSpanId(format!(
                                "dense-bucket-{bucket_index}-span-{span_index}"
                            )),
                            provider: "codex".to_string(),
                            source_id: source.source_id.clone(),
                            span_kind: "codex_task".to_string(),
                            source_record_id: None,
                            source_file_path_hash: None,
                            summary_id: None,
                            session_id: Some(format!(
                                "dense-bucket-{bucket_index}-session-{span_index}"
                            )),
                            thread_id: Some(format!(
                                "dense-bucket-{bucket_index}-thread-{span_index}"
                            )),
                            title: format!("Dense task {bucket_index}-{span_index}"),
                            normalized_title: format!("dense task {bucket_index}-{span_index}"),
                            title_source: Some("thread_name".to_string()),
                            summary_preview: None,
                            todo_excerpt: None,
                            issue_keys: Vec::new(),
                            branch_family: Some("main".to_string()),
                            project_bucket: project_bucket.clone(),
                            project: Some(ProjectInfo {
                                project_id: format!("project-dense-{bucket_index}"),
                                project_label: Some(format!("Dense {bucket_index}")),
                                repo_remote_hash: Some(format!("repo-dense-{bucket_index}")),
                                repo_label: Some(format!("statsai/dense-{bucket_index}")),
                                branch_hash: Some("branch-dense".to_string()),
                                branch_label: Some("main".to_string()),
                                path_hash: Some(format!("path-dense-{bucket_index}")),
                                path_label: Some(format!("/workspace/dense-{bucket_index}")),
                            }),
                            git: None,
                            usage: UsageCounts {
                                input_tokens: Some(10),
                                output_tokens: Some(5),
                                total_tokens: Some(15),
                                requests: Some(1),
                                ..UsageCounts::default()
                            },
                            estimated_cost_usd: Some(25),
                            estimated_cost_micro_usd: Some(250_000),
                            event_count: 1,
                            has_usage_evidence: true,
                            total_messages: 2,
                            user_messages: 1,
                            assistant_messages: 1,
                            developer_messages: 0,
                            linked_event_ids: Vec::new(),
                            confidence: Confidence::High,
                            is_meta: false,
                            started_at,
                            ended_at: Some(ended_at),
                            duration_seconds: Some(60),
                        }
                    })
                    .collect::<Vec<_>>();
                let members = spans
                    .iter()
                    .enumerate()
                    .map(|(span_index, span)| WorkItemMember {
                        work_item_id: work_item_id.clone(),
                        span_id: span.span_id.clone(),
                        ordinal: span_index,
                    })
                    .collect::<Vec<_>>();
                let first_span = spans.first().expect("first dense span");
                let last_span = spans.last().expect("last dense span");
                TaskBucketSnapshot {
                    project_bucket: project_bucket.clone(),
                    generated_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                    applied_verification_cursor: None,
                    work_items: vec![WorkItem {
                        schema_version: WORK_ITEM_SCHEMA_VERSION.to_string(),
                        work_item_id: work_item_id.clone(),
                        anchor_span_id: first_span.span_id.clone(),
                        tail_span_id: last_span.span_id.clone(),
                        project_bucket,
                        title: format!("Dense task bucket {bucket_index}"),
                        normalized_title: format!("dense task bucket {bucket_index}"),
                        status: TaskStatus::NeedsReview,
                        confidence: Confidence::Medium,
                        started_at: first_span.started_at,
                        ended_at: last_span.ended_at.expect("dense task bucket end timestamp"),
                        duration_seconds: Some((span_count_per_bucket as u64).saturating_mul(60)),
                        span_count: span_count_per_bucket as u64,
                        event_count: span_count_per_bucket as u64,
                        total_input_tokens: (span_count_per_bucket as u64).saturating_mul(10),
                        total_cache_creation_tokens: 0,
                        total_cache_read_tokens: 0,
                        total_output_tokens: (span_count_per_bucket as u64).saturating_mul(5),
                        total_reasoning_tokens: 0,
                        total_tokens: (span_count_per_bucket as u64).saturating_mul(15),
                        estimated_cost_usd: Some((span_count_per_bucket as i64).saturating_mul(25)),
                        estimated_cost_micro_usd: Some(
                            (span_count_per_bucket as i64).saturating_mul(250_000),
                        ),
                        providers: vec!["codex".to_string()],
                        issue_keys: Vec::new(),
                        repo_label: Some(format!("statsai/dense-{bucket_index}")),
                        branch_labels: vec!["main".to_string()],
                        path_label: Some(format!("/workspace/dense-{bucket_index}")),
                        summary_preview: None,
                        todo_excerpt: None,
                        no_git: false,
                        cross_provider: false,
                        continuation_reasons: Vec::new(),
                        review_reasons: vec!["needs_review".to_string()],
                    }],
                    members,
                    spans,
                }
            })
            .collect::<Vec<_>>();

        SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch_multi_dense_task_only".to_string(),
            device_id: "device".to_string(),
            sources: Vec::new(),
            accounts: Vec::new(),
            source_account_assignments: Vec::new(),
            subscriptions: Vec::new(),
            account_plan_observations: Vec::new(),
            account_evidence_summaries: Vec::new(),
            events: Vec::new(),
            summaries: Vec::new(),
            task_buckets,
            task_verifications: Vec::new(),
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: now,
        }
    }

    #[test]
    fn dense_single_task_bucket_stays_within_batched_d1_budget_estimate() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_dense_task_only_sync_batch(now, 240);

        assert!(
            estimate_http_rollup_d1_queries(&batch) <= HTTP_ROLLUP_D1_QUERY_BUDGET,
            "dense single-bucket task sync should fit after batched task writes"
        );
    }

    #[test]
    fn multi_bucket_dense_task_sync_splits_to_fit_chunked_write_budget() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let batch = test_multi_bucket_dense_task_only_sync_batch(now, 5, 600);

        let chunks = split_http_rollup_sync_batches(&batch);

        assert!(chunks.len() > 1);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.task_buckets.len())
                .sum::<usize>(),
            batch.task_buckets.len()
        );
        assert!(chunks
            .iter()
            .all(|chunk| estimate_http_rollup_d1_queries(chunk) <= HTTP_ROLLUP_D1_QUERY_BUDGET));
    }

    #[test]
    fn remote_sync_batch_match_requires_same_last_batch_id() {
        let store = Store::in_memory().expect("store");
        store
            .record_sync_success(
                "http",
                "https://api.example.com/api/sync/batches",
                "batch_1_part_2_of_2",
                &[],
                &[],
                None,
            )
            .expect("record sync success");
        let local_state = store
            .sync_state("http", "https://api.example.com/api/sync/batches")
            .expect("state")
            .expect("present");

        assert!(remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": "batch_1"
                }
            }),
            &local_state
        ));
        assert!(!remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": null
                }
            }),
            &local_state
        ));
        assert!(!remote_sync_batch_matches_local_state(
            &json!({
                "device": {
                    "last_sync_batch_id": "batch_2"
                }
            }),
            &local_state
        ));
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
    fn incremental_http_sync_sends_late_claude_assignment_without_full() {
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let store = Store::in_memory().expect("store");
        let mut source = SourceLocation::local_adapter(
            "claude_code",
            "claude-code-local-jsonl",
            "0.3.3",
            Path::new("/tmp/claude-http-late-assignment"),
            LocationOrigin::Configured,
        );
        source.verified_state_hash =
            verified_source_observation_hash(&VerifiedSourceObservation::AttributionBlocked {
                blocked_since: None,
            })
            .expect("blocked observation hash");
        store.upsert_source(&source).expect("source");

        let authenticated_at = Utc
            .with_ymd_and_hms(2026, 8, 7, 12, 0, 0)
            .single()
            .expect("authenticated_at");
        let event_at = authenticated_at + Duration::hours(1);
        store
            .insert_event(&test_event(
                "claude_code",
                &source,
                event_at,
                None,
                TokenParts::total(15),
            ))
            .expect("unassigned event");
        store.rebuild_sync_rollups().expect("initial rollups");

        let command = SyncCommand {
            endpoint: Some(endpoint),
            ..test_sync_command("http")
        };
        assert!(!command.full);
        let target = sync_target(&command).expect("target");
        let (initial_batch, initial_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_mode, SyncPayloadMode::Rollups);
        assert!(initial_batch.accounts.is_empty());
        assert!(initial_batch.source_account_assignments.is_empty());
        assert_eq!(initial_batch.summaries.len(), 1);
        let unassigned_summary_id = initial_batch.summaries[0].summary_id.clone();
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        let inferred_observation = VerifiedSourceObservation::Inferred {
            identity: Box::new(VerifiedSourceState {
                provider_user_id: Some("claude-account".to_string()),
                email: Some("claude@example.com".to_string()),
                account_label: None,
                plan_name: None,
                authenticated_at: Some(authenticated_at),
                verified_at: Some(authenticated_at),
                subscription: None,
            }),
            basis: SourceIdentityInference::CachedLocalProfile,
            settings_modified_at: None,
        };
        let inferred_hash = verified_source_observation_hash(&inferred_observation)
            .expect("inferred observation hash");
        reconcile_verified_source_state(&store, &mut source, &inferred_observation, inferred_hash)
            .expect("inferred Claude state");
        store.rebuild_sync_rollups().expect("reattributed rollups");

        let (incremental_batch, incremental_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("incremental batch");
        assert_eq!(incremental_mode, SyncPayloadMode::Rollups);
        assert_eq!(incremental_batch.accounts.len(), 1);
        assert_eq!(incremental_batch.source_account_assignments.len(), 1);
        assert_eq!(incremental_batch.summaries.len(), 1);
        assert!(incremental_batch.summaries[0].provider_account_id.is_some());
        let snapshot = incremental_batch
            .authoritative_snapshot
            .as_ref()
            .expect("retired unassigned rollup requires an authoritative snapshot");
        assert!(!snapshot.summary_ids.contains(&unassigned_summary_id));
        assert!(snapshot
            .summary_ids
            .contains(&incremental_batch.summaries[0].summary_id));
    }

    #[test]
    fn full_http_sync_resends_metadata_after_tracking_is_cleared() {
        let endpoint = "https://api.example.com/api/sync/batches".to_string();
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-http-reset-tracking"),
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
        apply_verified_source_state(
            &store,
            &source,
            Some(&VerifiedSourceState {
                provider_user_id: Some("acct-real".to_string()),
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
                    ended_at: None,
                    current_period_ends_at: Some(started_at + Duration::days(30)),
                    status: SubscriptionStatus::Active,
                    verified_at: Some(verified_at),
                }),
            }),
        )
        .expect("verified state");

        let account_id = store.list_accounts().expect("accounts")[0]
            .provider_account_id
            .clone();
        let event = test_event(
            "codex",
            &source,
            started_at + Duration::hours(1),
            Some(account_id),
            TokenParts {
                input: 10,
                output: 5,
                cached_input: 0,
                reasoning: 0,
                total: 15,
                cost: Some(10),
            },
        );
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let command = SyncCommand {
            endpoint: Some(endpoint.clone()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");

        let (initial_batch, initial_mode) =
            build_sync_batch(&command, &store, "device", &target).expect("initial batch");
        assert_eq!(initial_mode, SyncPayloadMode::Rollups);
        record_rollup_sync_success(&store, "http", &target, &initial_batch)
            .expect("record initial sync");

        let all_sources = store.list_sources().expect("sources");
        let all_accounts = store.list_accounts().expect("accounts");

        let sync_sources: Vec<_> = all_sources
            .iter()
            .cloned()
            .map(sanitize_source_for_sync)
            .collect();
        let sync_accounts: Vec<_> = all_accounts
            .iter()
            .cloned()
            .map(sanitize_account_for_sync)
            .collect();
        assert_eq!(
            store
                .pending_sources_for_sync("http", &target, &sync_sources)
                .expect("pending sources")
                .len(),
            0
        );
        assert_eq!(
            store
                .pending_accounts_for_sync("http", &target, &sync_accounts)
                .expect("pending accounts")
                .len(),
            0
        );

        let local_state = store
            .sync_state("http", &target)
            .expect("state")
            .expect("present");
        let local_verify = sync_local_verify(&store, "http", &target, Some(&local_state), false)
            .expect("local verify");
        assert_eq!(
            remote_metadata_gap_reason(
                &json!({
                    "device": {
                        "last_sync_batch_id": initial_batch.batch_id
                    },
                    "mirrorCounts": {
                        "sources": 0,
                        "accounts": 0,
                        "source_account_assignments": 0,
                        "subscriptions": 0,
                        "summaries": 0,
                        "sync_batches": 1
                    }
                }),
                &local_verify
            )
            .as_deref(),
            Some("sources 0!=1, accounts 0!=1, source_account_assignments 0!=1")
        );

        store
            .clear_sync_tracking_for_target("http", &target)
            .expect("clear tracking");

        let (batch, mode) = build_sync_batch(&command, &store, "device", &target).expect("batch");
        assert_eq!(mode, SyncPayloadMode::Rollups);
        assert_eq!(batch.sources.len(), 1);
        assert_eq!(batch.accounts.len(), 1);
        assert_eq!(batch.source_account_assignments.len(), 1);
        assert!(batch.subscriptions.is_empty());
        assert_eq!(batch.summaries.len(), 1);
        assert!(is_daily_rollup_summary(&batch.summaries[0]));
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
    fn code_change_dedup_warning_covers_only_unblinded_http_commit_uploads() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let agent_edit = test_code_change_metric(0, now);
        let mut committed = test_code_change_metric(1, now);
        committed.kind = statsai_core::CodeChangeMetricKind::Committed;

        assert!(
            code_change_dedup_warning("http", false, std::slice::from_ref(&committed)).is_some()
        );
        assert!(
            code_change_dedup_warning("http", true, std::slice::from_ref(&committed)).is_none()
        );
        assert!(
            code_change_dedup_warning("file", false, std::slice::from_ref(&committed)).is_none()
        );
        assert!(
            code_change_dedup_warning("http", false, std::slice::from_ref(&agent_edit)).is_none()
        );
        assert!(code_change_dedup_warning("http", false, &[]).is_none());
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

    #[test]
    fn device_remote_reset_response_requires_explicit_device_scope() {
        assert!(ensure_device_remote_reset_response(&json!({
            "ok": true,
            "scope": "device_mirror",
            "device_id": "device-1"
        }))
        .is_ok());
        assert!(ensure_device_remote_reset_response(&json!({
            "ok": true,
            "scope": "mirror"
        }))
        .is_err());
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
    fn task_verify_split_merge_and_reject_survive_rebuilds() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-verify"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-verify/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 9, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "event-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(5),
            "event-b",
            120,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "span-a",
            "Implement task verification",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(5),
            "span-b",
            "Implement task verification",
            &event_b,
        );
        store
            .insert_events(&[event_a.clone(), event_b.clone()])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        assert_eq!(initial.len(), 1);

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Split {
                        work_item_id: initial[0].work_item_id.0.clone(),
                        after_span: span_a.span_id.0.clone(),
                        left_title: Some("Left investigation".to_string()),
                        right_title: Some("Right implementation".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("split verify");

        let split_items = store.work_items().expect("split work items");
        assert_eq!(split_items.len(), 2);
        assert!(split_items
            .iter()
            .all(|item| item.status == TaskStatus::Verified));
        assert!(split_items
            .iter()
            .any(|item| item.title == "Left investigation"));
        assert!(split_items
            .iter()
            .any(|item| item.title == "Right implementation"));

        let left = split_items
            .iter()
            .find(|item| item.title == "Left investigation")
            .expect("left work item");
        let right = split_items
            .iter()
            .find(|item| item.title == "Right implementation")
            .expect("right work item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Merge {
                        left_work_item_id: left.work_item_id.0.clone(),
                        right_work_item_id: right.work_item_id.0.clone(),
                        title: Some("Unified verification work".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("merge verify");

        let merged = store.work_items().expect("merged work items");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Unified verification work");
        assert_eq!(merged[0].status, TaskStatus::Verified);

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: merged[0].work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let rejected = store.work_items().expect("rejected work items");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].status, TaskStatus::RejectedMeta);
        assert_eq!(store.task_verifications().expect("verifications").len(), 3);
    }

    #[test]
    fn task_show_include_evidence_includes_spans_and_rename_verification() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-show"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-show/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "show-event", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "show-span",
            "Investigate work item evidence",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let initial_item = initial.first().expect("initial work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Rename {
                        work_item_id: initial_item.work_item_id.0.clone(),
                        title: "Verified evidence task".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("rename verify");

        let renamed = store.work_items().expect("renamed work items");
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].title, "Verified evidence task");
        assert_eq!(renamed[0].status, TaskStatus::Verified);

        let output = load_task_show_output(&store, &renamed[0].work_item_id, true)
            .expect("task show output");
        assert_eq!(output.work_item.title, "Verified evidence task");
        assert_eq!(output.spans.len(), 1);
        assert_eq!(output.verifications.len(), 1);
        assert!(matches!(
            output.verifications[0].action,
            TaskVerificationAction::Rename { .. }
        ));
    }

    #[test]
    fn rename_and_accept_coexist_for_same_anchor() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rename-accept"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-rename-accept/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "rename-accept-event", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "rename-accept-span",
            "Investigate rename and accept coexistence",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let work_item = initial.first().expect("initial work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Rename {
                        work_item_id: work_item.work_item_id.0.clone(),
                        title: "Hosted-verified task title".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("rename verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let rebuilt = store.work_items().expect("rebuilt work items");
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].status, TaskStatus::Verified);
        assert_eq!(rebuilt[0].title, "Hosted-verified task title");

        let verifications = store.task_verifications().expect("verifications");
        assert_eq!(verifications.len(), 2);
        assert!(verifications.iter().any(|verification| matches!(
            verification.action,
            TaskVerificationAction::Rename { .. }
        )));
        assert!(verifications.iter().any(|verification| matches!(
            verification.action,
            TaskVerificationAction::Accept { .. }
        )));
    }

    #[test]
    fn accept_after_reject_supersedes_manual_reject_for_same_anchor() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-verify-supersede"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-verify-supersede/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 12, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "supersede-event", 95);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "supersede-span",
            "Supersede conflicting verification actions",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        let work_item = initial.first().expect("initial work item");
        let anchor_action_key = format!("status:{}", work_item.anchor_span_id.0);
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: work_item.work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let rebuilt = store.work_items().expect("rebuilt work items");
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].status, TaskStatus::Verified);
        assert!(!rebuilt[0]
            .review_reasons
            .iter()
            .any(|reason| reason.starts_with("manual_reject:")));

        let verifications = store.task_verifications().expect("verifications");
        assert_eq!(verifications.len(), 1);
        assert!(matches!(
            verifications[0].action,
            TaskVerificationAction::Accept { .. }
        ));
        assert_eq!(verifications[0].action_key, anchor_action_key);

        let output = load_task_show_output(&store, &rebuilt[0].work_item_id, true)
            .expect("task show output");
        assert_eq!(output.verifications.len(), 1);
        assert!(matches!(
            output.verifications[0].action,
            TaskVerificationAction::Accept { .. }
        ));
    }

    #[test]
    fn task_show_without_evidence_omits_spans_and_verifications() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-show-no-evidence"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-show-no-evidence/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 13, 11, 30, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "show-no-evidence", 90);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "show-no-evidence-span",
            "Inspect task show output",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_item = store
            .work_items()
            .expect("work items")
            .into_iter()
            .next()
            .expect("work item");
        let output = load_task_show_output(&store, &work_item.work_item_id, false)
            .expect("task show output");
        assert_eq!(output.work_item.work_item_id, work_item.work_item_id);
        assert!(output.spans.is_empty());
        assert!(output.verifications.is_empty());
    }

    #[test]
    fn task_benchmark_reports_current_and_baselines() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 10, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "bench-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "bench-b",
            120,
        );
        let event_c = test_scan_event(
            &source,
            file_path,
            started_at + Duration::hours(30),
            "bench-c",
            20,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "bench-span-a",
            "Implement benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "bench-span-b",
            "Implement benchmark reporting",
            &event_b,
        );
        let span_c = test_task_span(
            &source,
            file_path,
            started_at + Duration::hours(30),
            "bench-span-c",
            "review uncommitted changes",
            &event_c,
        );
        store
            .insert_events(&[event_a, event_b, event_c])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_items = store.work_items().expect("work items");
        let implementation_item = work_items
            .iter()
            .find(|item| item.title == "Implement benchmark reporting")
            .expect("implementation item");
        let review_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_c.span_id)
            .expect("review item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: implementation_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: review_item.work_item_id.0.clone(),
                        reason: "noise".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.verified_spans >= 3);
        assert!(report.verified_adjacent_pairs >= 1);
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert_eq!(report.baselines.len(), 6);
        assert!(report.manual_constraints_preserved);
        assert_eq!(
            report.failing_baselines.is_empty(),
            report.beats_all_baselines
        );
        assert_eq!(report.shipping_gate_ready, report.gate_blockers.is_empty());
    }

    #[test]
    fn task_benchmark_scores_raw_grouper_not_manual_split_output() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-raw-grouper"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-raw-grouper/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 11, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "raw-a", 100);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "raw-b",
            120,
        );
        let event_c = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(4),
            "raw-c",
            140,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "raw-span-a",
            "Implement benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "raw-span-b",
            "Implement benchmark reporting",
            &event_b,
        );
        let span_c = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(4),
            "raw-span-c",
            "Implement benchmark reporting",
            &event_c,
        );
        store
            .insert_events(&[event_a, event_b, event_c])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a.clone(), span_b.clone(), span_c.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("initial work items");
        assert_eq!(initial.len(), 1);
        let work_item = initial.first().expect("work item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Split {
                        work_item_id: work_item.work_item_id.0.clone(),
                        after_span: span_a.span_id.0.clone(),
                        left_title: Some("Investigate benchmark regression".to_string()),
                        right_title: Some("Implement benchmark reporting".to_string()),
                    },
                },
            },
            &store,
        )
        .expect("split verify");

        let split_items = store.work_items().expect("split items");
        assert_eq!(split_items.len(), 2);

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(report.current.adjacent_f1 < 1.0);
    }

    #[test]
    fn task_benchmark_reports_missing_ground_truth_explicitly() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-empty"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-empty/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 12, 0, 0)
            .single()
            .expect("started_at");
        let event = test_scan_event(&source, file_path, started_at, "bench-empty", 75);
        let span = test_task_span(
            &source,
            file_path,
            started_at,
            "bench-empty-span",
            "Investigate benchmark readiness",
            &event,
        );
        store.insert_events(&[event]).expect("insert events");
        store.upsert_task_spans(&[span]).expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert_eq!(report.verified_spans, 0);
        assert_eq!(report.verified_adjacent_pairs, 0);
        assert!(!report.has_verified_ground_truth);
        assert!(!report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert!(report.failing_baselines.is_empty());
        assert_eq!(
            report.gate_blockers,
            vec!["missing_verified_ground_truth".to_string()]
        );

        let json = benchmark_json_value(&report);
        assert_eq!(json["has_verified_ground_truth"], json!(false));
        assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
        assert_eq!(json["shipping_gate_ready"], json!(false));
        assert_eq!(json["verified_spans"], json!(0));
        assert_eq!(json["failing_baselines"], json!([]));
        assert_eq!(
            json["gate_blockers"],
            json!(["missing_verified_ground_truth"])
        );
    }

    #[test]
    fn task_benchmark_reports_label_only_ground_truth_explicitly() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-label-only"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_a = "/tmp/codex-task-benchmark-label-only/a.jsonl";
        let file_b = "/tmp/codex-task-benchmark-label-only/b.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 15, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_a, started_at, "label-only-a", 80);
        let event_b = test_scan_event(
            &source,
            file_b,
            started_at + Duration::minutes(30),
            "label-only-b",
            90,
        );
        let span_a = test_task_span(
            &source,
            file_a,
            started_at,
            "label-only-span-a",
            "Implement label-only benchmark reporting",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_b,
            started_at + Duration::minutes(30),
            "label-only-span-b",
            "Clearing Conversation History",
            &event_b,
        );
        let mut span_a = span_a;
        let mut span_b = span_b;
        span_a.project = Some(ProjectInfo {
            project_id: "label-only-a".to_string(),
            project_label: Some("label-only-a".to_string()),
            repo_remote_hash: Some("repo-label-a".to_string()),
            repo_label: Some("owner/label-a".to_string()),
            branch_hash: Some("branch-label-a".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-label-a".to_string()),
            path_label: Some("/tmp/label-only-a".to_string()),
        });
        span_a.project_bucket = project_bucket_key(span_a.project.as_ref());
        span_a.branch_family = branch_family(Some("main"));
        span_b.project = Some(ProjectInfo {
            project_id: "label-only-b".to_string(),
            project_label: Some("label-only-b".to_string()),
            repo_remote_hash: Some("repo-label-b".to_string()),
            repo_label: Some("owner/label-b".to_string()),
            branch_hash: Some("branch-label-b".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-label-b".to_string()),
            path_label: Some("/tmp/label-only-b".to_string()),
        });
        span_b.project_bucket = project_bucket_key(span_b.project.as_ref());
        span_b.branch_family = branch_family(Some("main"));
        let span_a_id = span_a.span_id.clone();
        let span_b_id = span_b.span_id.clone();
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_items = store.work_items().expect("work items");
        let accepted_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_a_id)
            .expect("accepted item");
        let rejected_item = work_items
            .iter()
            .find(|item| item.anchor_span_id == span_b_id)
            .expect("rejected item");

        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: accepted_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: rejected_item.work_item_id.0.clone(),
                        reason: "meta".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert_eq!(report.verified_spans, 2);
        assert_eq!(report.verified_adjacent_pairs, 0);
        assert!(report.has_verified_ground_truth);
        assert!(!report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert_eq!(report.failing_baselines, Vec::<String>::new());
        assert_eq!(
            report.gate_blockers,
            vec!["missing_pairwise_ground_truth".to_string()]
        );

        let json = benchmark_json_value(&report);
        assert_eq!(json["has_verified_ground_truth"], json!(true));
        assert_eq!(json["has_verified_pairwise_ground_truth"], json!(false));
        assert_eq!(json["verified_spans"], json!(2));
        assert_eq!(json["verified_adjacent_pairs"], json!(0));
        assert_eq!(
            json["gate_blockers"],
            json!(["missing_pairwise_ground_truth"])
        );
    }

    #[test]
    fn task_benchmark_reports_failing_baselines_when_current_ties_them() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-benchmark-baseline-tie"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-benchmark-baseline-tie/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 16, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "tie-a", 80);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "tie-b",
            90,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "tie-span-a",
            "Implement benchmark blocking report",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(2),
            "tie-span-b",
            "Implement benchmark blocking report",
            &event_b,
        );
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let work_item = store
            .work_items()
            .expect("work items")
            .into_iter()
            .next()
            .expect("work item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Accept {
                        work_item_id: work_item.work_item_id.0.clone(),
                    },
                },
            },
            &store,
        )
        .expect("accept verify");

        let report = store.task_benchmark_report().expect("benchmark report");
        assert!(report.has_verified_ground_truth);
        assert!(report.has_verified_pairwise_ground_truth);
        assert!(report.manual_constraints_preserved);
        assert!(!report.beats_all_baselines);
        assert!(!report.shipping_gate_ready);
        assert_eq!(
            report.failing_baselines,
            vec![
                "gap_only_2h".to_string(),
                "gap_only_6h".to_string(),
                "gap_only_12h".to_string(),
                "gap_only_24h".to_string(),
                "repo_plus_title".to_string(),
                "repo_plus_branch_plus_title".to_string(),
            ]
        );
        assert_eq!(
            report.gate_blockers,
            vec!["baseline_regressions".to_string()]
        );
    }

    #[test]
    fn task_list_filters_by_provider_and_status() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-list-filters"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-list-filters/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
            .single()
            .expect("started_at");
        let event_auto = test_scan_event(&source, file_path, started_at, "event-auto", 50);
        let event_review = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(10),
            "event-review",
            60,
        );
        let event_reject = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(20),
            "event-reject",
            70,
        );
        let mut span_auto = test_task_span(
            &source,
            file_path,
            started_at,
            "span-auto",
            "Implement task list filters",
            &event_auto,
        );
        let mut span_review = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(10),
            "span-review",
            "Review task list filtering behavior",
            &event_review,
        );
        let mut span_reject = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(20),
            "span-reject",
            "Noise task entry",
            &event_reject,
        );
        span_auto.project = Some(ProjectInfo {
            project_id: "project-auto".to_string(),
            project_label: Some("auto".to_string()),
            repo_remote_hash: Some("repo-auto".to_string()),
            repo_label: Some("owner/auto".to_string()),
            branch_hash: Some("branch-auto".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-auto".to_string()),
            path_label: Some("/tmp/project-auto".to_string()),
        });
        span_auto.project_bucket = project_bucket_key(span_auto.project.as_ref());
        span_auto.branch_family = branch_family(Some("main"));

        span_review.provider = "opencode".to_string();
        span_review.project = Some(ProjectInfo {
            project_id: "project-review".to_string(),
            project_label: Some("review".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-review".to_string()),
            path_label: Some("/tmp/project-review".to_string()),
        });
        span_review.project_bucket = project_bucket_key(span_review.project.as_ref());

        span_reject.project = Some(ProjectInfo {
            project_id: "project-reject".to_string(),
            project_label: Some("reject".to_string()),
            repo_remote_hash: None,
            repo_label: None,
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-reject".to_string()),
            path_label: Some("/tmp/project-reject".to_string()),
        });
        span_reject.project_bucket = project_bucket_key(span_reject.project.as_ref());

        store
            .insert_events(&[event_auto, event_review, event_reject])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_auto.clone(), span_review.clone(), span_reject.clone()])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let initial = store.work_items().expect("work items");
        let reject_item = initial
            .iter()
            .find(|item| item.anchor_span_id == span_reject.span_id)
            .expect("reject item");
        task(
            TaskCommand {
                command: TaskSubcommand::Verify {
                    command: TaskVerifySubcommand::Reject {
                        work_item_id: reject_item.work_item_id.0.clone(),
                        reason: "noise".to_string(),
                    },
                },
            },
            &store,
        )
        .expect("reject verify");

        let codex_items =
            filtered_task_list_items(&store, Some("codex"), None).expect("codex filtered items");
        assert_eq!(codex_items.len(), 1);
        assert!(codex_items
            .iter()
            .all(|item| item.providers.iter().any(|provider| provider == "codex")));
        assert!(codex_items
            .iter()
            .all(|item| item.status != TaskStatus::RejectedMeta));

        let auto_items = filtered_task_list_items(&store, None, Some(&TaskStatus::Auto))
            .expect("auto filtered items");
        assert_eq!(auto_items.len(), 1);
        assert_eq!(auto_items[0].anchor_span_id, span_auto.span_id);

        let rejected_items =
            filtered_task_list_items(&store, None, Some(&TaskStatus::RejectedMeta))
                .expect("rejected filtered items");
        assert_eq!(rejected_items.len(), 1);
        assert_eq!(rejected_items[0].anchor_span_id, span_reject.span_id);

        let default_selection = task_list_selection(&store, None, None).expect("default selection");
        assert_eq!(default_selection.items.len(), 2);
        assert_eq!(default_selection.hidden_rejected_meta, 1);
    }

    #[test]
    fn format_task_list_item_appends_review_reasons_when_present() {
        let ended_at = Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        let mut work_item = WorkItem {
            schema_version: "work_item.v1".to_string(),
            work_item_id: WorkItemId("work-review".to_string()),
            anchor_span_id: statsai_core::TaskSpanId("span-review".to_string()),
            tail_span_id: statsai_core::TaskSpanId("span-review".to_string()),
            project_bucket: "bucket".to_string(),
            title: "Reviewable item".to_string(),
            normalized_title: "reviewable item".to_string(),
            status: TaskStatus::NeedsReview,
            confidence: Confidence::Low,
            started_at: ended_at - Duration::minutes(5),
            ended_at,
            duration_seconds: Some(300),
            span_count: 1,
            event_count: 0,
            total_input_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_output_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: None,
            estimated_cost_micro_usd: None,
            providers: vec!["claude_code".to_string()],
            issue_keys: Vec::new(),
            repo_label: None,
            branch_labels: Vec::new(),
            path_label: None,
            summary_preview: None,
            todo_excerpt: None,
            no_git: false,
            cross_provider: false,
            continuation_reasons: Vec::new(),
            review_reasons: vec!["no_usage_evidence".to_string(), "generic_title".to_string()],
        };

        let line = format_task_list_item(&work_item);
        assert!(line.contains("review=no_usage_evidence,generic_title"));

        work_item.review_reasons.clear();
        let clean_line = format_task_list_item(&work_item);
        assert!(!clean_line.contains("review="));
    }

    #[test]
    fn selected_rebuild_project_buckets_filter_by_provider_and_source() {
        let store = Store::in_memory().expect("store");
        let source_codex = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild-filter-a"),
            LocationOrigin::Configured,
        );
        let source_open = SourceLocation::local_adapter(
            "opencode",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild-filter-b"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source_codex).expect("codex source");
        store.upsert_source(&source_open).expect("opencode source");

        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 15, 9, 0, 0)
            .single()
            .expect("started_at");
        let event_codex = test_scan_event(
            &source_codex,
            "/tmp/codex-task-rebuild-filter-a/session.jsonl",
            started_at,
            "event-codex",
            50,
        );
        let event_open = test_scan_event(
            &source_open,
            "/tmp/codex-task-rebuild-filter-b/session.jsonl",
            started_at + Duration::minutes(5),
            "event-open",
            60,
        );
        let span_codex = test_task_span(
            &source_codex,
            "/tmp/codex-task-rebuild-filter-a/session.jsonl",
            started_at,
            "span-codex",
            "Codex rebuild target",
            &event_codex,
        );
        let mut span_open = test_task_span(
            &source_open,
            "/tmp/codex-task-rebuild-filter-b/session.jsonl",
            started_at + Duration::minutes(5),
            "span-open",
            "OpenCode rebuild target",
            &event_open,
        );
        span_open.provider = "opencode".to_string();

        store
            .insert_events(&[event_codex, event_open])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_codex.clone(), span_open.clone()])
            .expect("insert spans");

        let codex_buckets =
            selected_rebuild_project_buckets(&store, Some("codex"), None).expect("codex buckets");
        assert_eq!(
            codex_buckets,
            BTreeSet::from([span_codex.project_bucket.clone()])
        );

        let open_buckets = selected_rebuild_project_buckets(&store, Some("opencode"), None)
            .expect("opencode buckets");
        assert_eq!(
            open_buckets,
            BTreeSet::from([span_open.project_bucket.clone()])
        );

        let source_buckets =
            selected_rebuild_project_buckets(&store, None, Some(&source_codex.source_id.0))
                .expect("source buckets");
        assert_eq!(
            source_buckets,
            BTreeSet::from([span_codex.project_bucket.clone()])
        );
    }

    #[test]
    fn task_status_and_verdict_parsers_reject_unknown_values() {
        assert_eq!(
            parse_task_status_filter("verified").expect("verified status"),
            TaskStatus::Verified
        );
        assert_eq!(
            parse_task_verdict("noise").expect("noise verdict"),
            TaskVerdict::Noise
        );
        assert!(parse_task_status_filter("mystery").is_err());
        assert!(parse_task_verdict("mystery").is_err());
    }

    #[test]
    fn stats_json_value_exposes_expected_fields() {
        let stats = statsai_store::TaskStats {
            total_spans: 10,
            total_work_items: 3,
            verified_percentage: 25.0,
            no_git_percentage: 50.0,
            cross_provider_percentage: 10.0,
            rejected_meta_percentage: 5.0,
            average_spans_per_work_item: 3.33,
        };

        let json = stats_json_value(&stats);
        assert_eq!(json["total_spans"], json!(10));
        assert_eq!(json["total_work_items"], json!(3));
        assert_eq!(json["verified_percentage"], json!(25.0));
        assert_eq!(json["no_git_percentage"], json!(50.0));
        assert_eq!(json["cross_provider_percentage"], json!(10.0));
        assert_eq!(json["rejected_meta_percentage"], json!(5.0));
        assert_eq!(json["average_spans_per_work_item"], json!(3.33));
    }

    #[test]
    fn sync_batch_serialization_excludes_local_task_entities() {
        let batch = SyncBatch {
            schema_version: SYNC_BATCH_SCHEMA_VERSION.to_string(),
            batch_id: "batch-test".to_string(),
            device_id: "device-test".to_string(),
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
            code_change_metrics: Vec::new(),
            quota_cycle_contributions: Vec::new(),
            authoritative_snapshot: None,
            created_at: Utc
                .with_ymd_and_hms(2026, 6, 14, 13, 0, 0)
                .single()
                .expect("created_at"),
        };

        let value = serde_json::to_value(&batch).expect("serialize sync batch");
        assert!(value.get("task_buckets").is_none());
        assert!(value.get("task_verifications").is_none());
    }

    #[test]
    fn task_rebuild_is_idempotent() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-task-rebuild"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let file_path = "/tmp/codex-task-rebuild/session.jsonl";
        let started_at = Utc
            .with_ymd_and_hms(2026, 6, 14, 14, 0, 0)
            .single()
            .expect("started_at");
        let event_a = test_scan_event(&source, file_path, started_at, "rebuild-a", 75);
        let event_b = test_scan_event(
            &source,
            file_path,
            started_at + Duration::minutes(3),
            "rebuild-b",
            60,
        );
        let span_a = test_task_span(
            &source,
            file_path,
            started_at,
            "rebuild-span-a",
            "Rebuild task work items",
            &event_a,
        );
        let span_b = test_task_span(
            &source,
            file_path,
            started_at + Duration::minutes(3),
            "rebuild-span-b",
            "Rebuild task work items",
            &event_b,
        );
        store
            .insert_events(&[event_a, event_b])
            .expect("insert events");
        store
            .upsert_task_spans(&[span_a, span_b])
            .expect("insert spans");
        store
            .rebuild_all_task_work_items()
            .expect("initial rebuild");

        let first = store.work_items().expect("first rebuild work items");
        task(
            TaskCommand {
                command: TaskSubcommand::Rebuild {
                    provider: None,
                    source_id: None,
                    all: true,
                },
            },
            &store,
        )
        .expect("first task rebuild");
        let second = store.work_items().expect("second rebuild work items");
        task(
            TaskCommand {
                command: TaskSubcommand::Rebuild {
                    provider: None,
                    source_id: None,
                    all: true,
                },
            },
            &store,
        )
        .expect("second task rebuild");
        let third = store.work_items().expect("third rebuild work items");

        assert_eq!(first, second);
        assert_eq!(second, third);
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
        let legacy_event_a =
            test_event("codex", &source, a_started_at, None, TokenParts::total(50));
        let legacy_event_b =
            test_event("codex", &source, b_started_at, None, TokenParts::total(75));
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

    #[test]
    fn http_verify_pending_counts_match_sanitized_sync_payloads() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verify-pending"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: provider_account_id("codex", "personal"),
            provider: "codex".to_string(),
            identity_source: IdentitySource::ManualHint,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: Some("Pro".to_string()),
            confidence: Confidence::High,
            verified_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.upsert_account(&account).expect("account");
        let started_at = Utc::now();

        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &account.provider_account_id,
                "pro",
                started_at,
            ),
            provider: "codex".to_string(),
            provider_account_id: account.provider_account_id.clone(),
            plan_name: "Pro".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: None,
            renewal_day: None,
            started_at,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            notes: Some("private note".to_string()),
        };
        store
            .upsert_subscription(&subscription)
            .expect("subscription");
        let summary = test_summary(
            "codex",
            &source,
            Utc::now(),
            42,
            Some(account.provider_account_id.clone()),
        );
        store.upsert_summary(&summary).expect("summary");

        let target = "https://api.example.com/api/sync/batches".to_string();
        store
            .record_sources_synced("http", &target, &[sanitize_source_for_sync(source.clone())])
            .expect("record sources");
        store
            .record_accounts_synced(
                "http",
                &target,
                &[sanitize_account_for_sync(account.clone())],
            )
            .expect("record accounts");
        store
            .record_subscriptions_synced(
                "http",
                &target,
                &[sanitize_subscription_for_sync(subscription.clone())],
            )
            .expect("record subscriptions");
        store
            .record_summaries_synced(
                "http",
                &target,
                &[sanitize_summary_for_sync(summary.clone())],
            )
            .expect("record summaries");

        let local = sync_local_verify(&store, "http", &target, None, false).expect("local verify");
        assert_eq!(local.pending_sources, 0);
        assert_eq!(local.pending_accounts, 0);
        assert_eq!(local.pending_source_account_assignments, 0);
        assert_eq!(local.pending_subscriptions, 0);
        assert_eq!(local.total_passthrough_summaries, 0);
        assert_eq!(local.pending_passthrough_summaries, 0);
    }

    #[test]
    fn sync_local_verify_uses_sanitized_rollup_hashes() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sanitized-rollups"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let mut event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(42),
        );
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let target = "https://api.example.com/api/sync/batches".to_string();
        let rollups: Vec<_> = store
            .all_sync_rollup_summaries()
            .expect("rollups")
            .into_iter()
            .map(sanitize_summary_for_sync)
            .collect();
        assert_eq!(rollups.len(), 1);
        assert_eq!(
            rollups[0]
                .project
                .as_ref()
                .and_then(|project| project.path_label.as_deref()),
            Some("/Users/example/work/ai-stats")
        );
        assert!(rollups[0].privacy.contains_file_paths);
        store
            .record_summaries_synced("http", &target, &rollups)
            .expect("record rollups");

        let local = sync_local_verify(&store, "http", &target, None, true).expect("local verify");
        assert_eq!(local.total_rollups, 1);
        assert_eq!(local.pending_rollups, 0);
    }

    #[test]
    fn sync_local_verify_respects_project_sync_opt_in() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-verify-project-opt-in"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let mut event = test_event(
            "codex",
            &source,
            Utc::now(),
            Some(provider_account_id("codex", "personal")),
            TokenParts::total(42),
        );
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: None,
            branch_label: None,
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");
        store.rebuild_sync_rollups().expect("rebuild");

        let target = "https://api.example.com/api/sync/batches".to_string();
        let rollups: Vec<_> = store
            .all_sync_rollup_summaries()
            .expect("rollups")
            .into_iter()
            .map(|summary| sanitize_summary_for_sync_with_projects(summary, false))
            .collect();
        store
            .record_summaries_synced("http", &target, &rollups)
            .expect("record rollups");

        let hidden = sync_local_verify(&store, "http", &target, None, false)
            .expect("local verify without projects");
        let opted_in = sync_local_verify(&store, "http", &target, None, true)
            .expect("local verify with projects");

        assert_eq!(hidden.pending_rollups, 0);
        assert_eq!(opted_in.pending_rollups, 1);
    }

    #[test]
    fn build_sync_batch_respects_project_and_task_opt_ins() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-project-sync-opt-in"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");

        let now = Utc
            .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
            .single()
            .expect("now");
        let mut event = test_event("codex", &source, now, None, TokenParts::total(120));
        event.project = Some(ProjectInfo {
            project_id: "project-repo-backed".to_string(),
            project_label: Some("ai-stats".to_string()),
            repo_remote_hash: Some("repo-hash".to_string()),
            repo_label: Some("owner/repo".to_string()),
            branch_hash: Some("branch-hash".to_string()),
            branch_label: Some("main".to_string()),
            path_hash: Some("path-hash".to_string()),
            path_label: Some("/Users/example/work/ai-stats".to_string()),
        });
        store.insert_event(&event).expect("event");

        let mut summary = test_summary("codex", &source, now, 120, None);
        summary.project = event.project.clone();
        store.upsert_summary(&summary).expect("summary");

        let mut task_batch = test_task_only_sync_batch(now, 1, 1);
        task_batch.task_buckets[0].spans[0].source_record_id =
            Some("codex_task_span.v1:raw-session:42".to_string());
        for bucket in &task_batch.task_buckets {
            store
                .replace_task_bucket_snapshot(bucket)
                .expect("seed task bucket");
        }
        for verification in &task_batch.task_verifications {
            store
                .merge_task_verification(verification)
                .expect("seed task verification");
        }

        let default_command = test_sync_command("file");
        let default_target = sync_target(&default_command).expect("default target");
        let (default_batch, default_mode) =
            build_sync_batch(&default_command, &store, "device", &default_target)
                .expect("default batch");
        assert_eq!(default_mode, SyncPayloadMode::Raw);
        assert_eq!(default_batch.events.len(), 1);
        assert!(default_batch.events[0].project.is_none());
        assert_eq!(default_batch.summaries.len(), 1);
        assert!(default_batch.summaries[0].project.is_none());
        assert!(default_batch.task_buckets.is_empty());
        assert!(default_batch.task_verifications.is_empty());

        let project_opt_in_command = SyncCommand {
            include_projects: true,
            ..test_sync_command("file")
        };
        let project_opt_in_target =
            sync_target(&project_opt_in_command).expect("project opt-in target");
        let (project_opt_in_batch, project_opt_in_mode) = build_sync_batch(
            &project_opt_in_command,
            &store,
            "device",
            &project_opt_in_target,
        )
        .expect("project opt-in batch");
        assert_eq!(project_opt_in_mode, SyncPayloadMode::Raw);
        assert_eq!(project_opt_in_batch.events.len(), 1);
        assert!(project_opt_in_batch.events[0].project.is_some());
        assert_eq!(project_opt_in_batch.summaries.len(), 1);
        assert!(project_opt_in_batch.summaries[0].project.is_some());
        assert!(project_opt_in_batch.task_buckets.is_empty());
        assert!(project_opt_in_batch.task_verifications.is_empty());

        store
            .set_sync_preferences(SyncPreferences {
                include_projects: true,
                include_tasks: false,
            })
            .expect("persist sync preferences");
        let (persisted_batch, persisted_mode) =
            build_sync_batch(&default_command, &store, "device", &default_target)
                .expect("persisted batch");
        assert_eq!(persisted_mode, SyncPayloadMode::Raw);
        assert!(persisted_batch.events[0].project.is_some());
        assert!(persisted_batch.summaries[0].project.is_some());
        assert!(persisted_batch.task_buckets.is_empty());
        assert!(persisted_batch.task_verifications.is_empty());

        let task_opt_in_command = SyncCommand {
            include_tasks: true,
            ..test_sync_command("file")
        };
        let task_opt_in_target = sync_target(&task_opt_in_command).expect("task opt-in target");
        let (task_opt_in_batch, task_opt_in_mode) =
            build_sync_batch(&task_opt_in_command, &store, "device", &task_opt_in_target)
                .expect("task opt-in batch");
        assert_eq!(task_opt_in_mode, SyncPayloadMode::Raw);
        assert!(task_opt_in_batch.events[0].project.is_some());
        assert!(task_opt_in_batch.summaries[0].project.is_some());
        assert_eq!(task_opt_in_batch.task_buckets.len(), 1);
        assert_eq!(task_opt_in_batch.task_verifications.len(), 1);
        let synced_span = &task_opt_in_batch.task_buckets[0].spans[0];
        assert!(synced_span.source_record_id.is_none());
        assert!(synced_span.session_id.is_none());
        assert!(synced_span.thread_id.is_none());
    }

    #[test]
    fn code_change_metric_project_ids_follow_sync_project_preferences() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
            .single()
            .expect("now");
        let mut seed_batch = test_task_only_sync_batch(now, 0, 0);
        let mut metric = test_code_change_metric(0, now);
        metric.project_id = Some("project-private".to_string());
        metric.repository_hash = Some("repository-private".to_string());
        seed_batch.code_change_metrics.push(metric);
        store
            .ingest_sync_batch(&seed_batch)
            .expect("seed code-change metric");

        let default_command = SyncCommand {
            dry_run: true,
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&default_command).expect("target");
        let (default_batch, _) =
            build_sync_batch(&default_command, &store, "device", &target).expect("default batch");
        assert_eq!(default_batch.code_change_metrics.len(), 1);
        assert!(default_batch.code_change_metrics[0].project_id.is_none());
        assert!(default_batch.code_change_metrics[0]
            .repository_hash
            .is_none());

        store
            .record_code_change_metrics_synced("http", &target, &default_batch.code_change_metrics)
            .expect("record sanitized metric");
        store
            .record_sync_success("http", &target, "batch_metric_default", &[], &[], None)
            .expect("record sync state");
        let (unchanged_batch, _) =
            build_sync_batch(&default_command, &store, "device", &target).expect("unchanged batch");
        assert!(unchanged_batch.code_change_metrics.is_empty());

        let include_command = SyncCommand {
            include_projects: true,
            dry_run: true,
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let (included_batch, _) =
            build_sync_batch(&include_command, &store, "device", &target).expect("included batch");
        assert_eq!(
            included_batch.code_change_metrics[0].project_id.as_deref(),
            Some("project-private")
        );
        assert!(included_batch.code_change_metrics[0]
            .repository_hash
            .is_some());

        store
            .set_sync_preferences(SyncPreferences {
                include_projects: true,
                include_tasks: false,
            })
            .expect("persist project opt-in");
        let exclude_command = SyncCommand {
            exclude_projects: true,
            full: true,
            dry_run: true,
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let (excluded_batch, _) =
            build_sync_batch(&exclude_command, &store, "device", &target).expect("excluded batch");
        assert_eq!(excluded_batch.code_change_metrics.len(), 1);
        assert!(excluded_batch.code_change_metrics[0].project_id.is_none());
        assert!(excluded_batch.code_change_metrics[0]
            .repository_hash
            .is_none());
    }

    #[test]
    fn code_change_metric_sync_sanitization_removes_raw_commit_hash() {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
            .single()
            .expect("now");
        let mut metric = test_code_change_metric(0, now);
        metric.commit_hash = Some("0123456789abcdef-public-commit".to_string());

        let sanitized = sanitize_code_change_metric_for_sync(metric, true);

        assert!(sanitized.commit_hash.is_none());
    }

    #[test]
    fn code_change_sync_excludes_metrics_owned_by_peer_devices() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 7, 7, 12, 0, 0)
            .single()
            .expect("now");
        let mut seed_batch = test_task_only_sync_batch(now, 0, 0);
        let mut local_metric = test_code_change_metric(0, now);
        local_metric.device_id = "local-device".to_string();
        let mut peer_metric = test_code_change_metric(1, now);
        peer_metric.device_id = "peer-device".to_string();
        seed_batch.code_change_metrics = vec![local_metric.clone(), peer_metric];
        store
            .ingest_sync_batch(&seed_batch)
            .expect("seed code-change metrics");
        let command = SyncCommand {
            dry_run: true,
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");

        let (batch, _) = build_sync_batch(&command, &store, "local-device", &target)
            .expect("build local sync batch");

        assert_eq!(batch.code_change_metrics, vec![local_metric.clone()]);
        assert_eq!(
            batch
                .authoritative_snapshot
                .expect("authoritative snapshot")
                .code_change_metric_ids,
            vec![local_metric.metric_id]
        );
    }

    #[test]
    fn quota_contributions_reach_the_batch_and_its_authoritative_ids() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-quota-v4-sync"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .single()
            .expect("observed at");
        let account_id = ProviderAccountId("account-quota-sync".to_string());
        store
            .upsert_source_account_assignment(&SourceAccountAssignment {
                schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
                assignment_id: SourceAccountAssignmentId("assignment-quota-sync".to_string()),
                source_id: source.source_id.clone(),
                provider: "codex".to_string(),
                provider_account_id: account_id,
                started_at: observed_at - Duration::days(1),
                ended_at: None,
                record_source: IdentitySource::UserConfigured,
                verified_at: Some(observed_at),
                created_at: observed_at,
                updated_at: observed_at,
            })
            .expect("assignment");
        let reset_at = observed_at + Duration::days(7);
        let quota_record: QuotaObservationRecordV1 = serde_json::from_value(json!({
            "observation": {
                "schema_version": "quota_observation.v1",
                "observation_id": "quota-observation-sync",
                "semantic_fingerprint": "quota-semantic-sync",
                "provider": "codex",
                "source_id": source.source_id,
                "provider_account_id": null,
                "observed_at": observed_at,
                "source_file_path_hash": "file-hash",
                "source_record_id": "record-id",
                "source_line_number": 1,
                "payload_hash": "payload-hash",
                "usage_sample": null,
                "usage_event_id": null,
                "usage_link_kind": "none",
                "status": {
                    "plan_type": "pro",
                    "individual_limit": {
                        "account_email": "private@example.com",
                        "nested": {"token": "provider-secret"}
                    },
                    "spend_control_state": null,
                    "reached_type": null,
                    "credits": {
                        "has_credits": false,
                        "unlimited": false,
                        "balance": null,
                        "balance_raw": null
                    }
                }
            },
            "windows": [{
                "schema_version": "quota_window_observation.v1",
                "window_observation_id": "quota-window-sync",
                "observation_id": "quota-observation-sync",
                "provider_slot": "primary",
                "limit_id": "subscription",
                "window_minutes": 10080,
                "used_percent": 25.0,
                "resets_at": reset_at,
                "resets_at_epoch_seconds": reset_at.timestamp()
            }],
            "raw_rate_limits": {"primary": {"used_percent": 25.0}}
        }))
        .expect("quota record");
        store
            .upsert_quota_observations(&[quota_record])
            .expect("quota observation");

        let command = SyncCommand {
            dry_run: true,
            endpoint: Some("https://api.example.com/api/sync/batches".to_string()),
            ..test_sync_command("http")
        };
        let target = sync_target(&command).expect("target");
        let (batch, _) =
            build_sync_batch(&command, &store, "device-quota", &target).expect("v4 quota batch");

        assert_eq!(
            batch.schema_version,
            statsai_core::SYNC_BATCH_V5_SCHEMA_VERSION
        );
        assert_eq!(batch.quota_cycle_contributions.len(), 1);
        // The uploaded contribution carries no provider status at all, so the
        // plan, credits, and `individual_limit` blob a window observation holds
        // cannot reach the backend even by accident.
        let uploaded = serde_json::to_value(&batch.quota_cycle_contributions[0])
            .expect("serialize contribution");
        assert_eq!(uploaded.get("latest_status"), None);
        let contribution_id = batch.quota_cycle_contributions[0].contribution_id.clone();
        assert_eq!(
            batch
                .authoritative_snapshot
                .as_ref()
                .expect("authoritative snapshot")
                .quota_cycle_contribution_ids,
            vec![contribution_id.clone()]
        );
        let chunks = split_http_rollup_sync_batches(&batch);
        assert!(chunks.iter().any(|chunk| {
            chunk
                .quota_cycle_contributions
                .iter()
                .any(|contribution| contribution.contribution_id == contribution_id)
        }));
        assert!(chunks.iter().any(|chunk| {
            chunk
                .authoritative_snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot
                        .quota_cycle_contribution_ids
                        .contains(&contribution_id)
                })
        }));

        record_rollup_sync_success(&store, "http", &target, &batch)
            .expect("record initial quota sync");
        let (unchanged, _) = build_sync_batch(&command, &store, "device-quota", &target)
            .expect("unchanged quota batch");
        assert!(unchanged.quota_cycle_contributions.is_empty());
        assert!(unchanged.authoritative_snapshot.is_none());

        store
            .delete_quota_observations_for_sources(std::slice::from_ref(&source.source_id))
            .expect("delete quota evidence");
        let (retirement, _) = build_sync_batch(&command, &store, "device-quota", &target)
            .expect("quota retirement batch");
        assert!(retirement.quota_cycle_contributions.is_empty());
        assert!(retirement
            .authoritative_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.quota_cycle_contribution_ids.is_empty()));
    }

    #[test]
    fn sanitize_account_for_sync_preserves_user_configured_label() {
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: provider_account_id("codex", "personal"),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: Some("provider-user-secret".to_string()),
            provider_user_id_hash: Some("a".repeat(64)),
            email: Some("private@example.com".to_string()),
            email_hash: Some("b".repeat(64)),
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: Some("Pro".to_string()),
            confidence: Confidence::Medium,
            verified_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let sanitized = sanitize_account_for_sync(account);
        assert_eq!(sanitized.account_label.as_deref(), Some("personal"));
        // The account's own identity travels: without it the dashboard can
        // only name an account by its `acct_` hash, and telling your own
        // accounts apart is why they sync in the first place.
        assert_eq!(
            sanitized.provider_user_id.as_deref(),
            Some("provider-user-secret")
        );
        assert_eq!(sanitized.email.as_deref(), Some("private@example.com"));
        assert_eq!(
            sanitized.provider_user_id_hash.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(
            sanitized.email_hash.as_deref(),
            Some("b".repeat(64).as_str())
        );
        // A plan is evidence now, not an account attribute.
        assert_eq!(sanitized.plan_name, None);
    }

    #[test]
    fn sync_rollup_stats_summaries_roll_up_events_by_day_and_account() {
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-sync-rollup-stats"),
            LocationOrigin::Configured,
        );
        let account = provider_account_id("codex", "personal");
        let day1_a = Utc
            .with_ymd_and_hms(2026, 5, 20, 10, 0, 0)
            .single()
            .expect("day1a");
        let day1_b = Utc
            .with_ymd_and_hms(2026, 5, 20, 11, 0, 0)
            .single()
            .expect("day1b");
        let day2 = Utc
            .with_ymd_and_hms(2026, 5, 21, 9, 0, 0)
            .single()
            .expect("day2");

        let summaries = build_sync_rollup_stats_summaries(
            &[
                test_event(
                    "codex",
                    &source,
                    day1_a,
                    Some(account.clone()),
                    TokenParts {
                        input: 10,
                        output: 5,
                        cached_input: 0,
                        reasoning: 0,
                        total: 15,
                        cost: Some(10),
                    },
                ),
                test_event(
                    "codex",
                    &source,
                    day1_b,
                    Some(account.clone()),
                    TokenParts {
                        input: 20,
                        output: 10,
                        cached_input: 0,
                        reasoning: 0,
                        total: 30,
                        cost: Some(30),
                    },
                ),
                test_event(
                    "codex",
                    &source,
                    day2,
                    Some(account),
                    TokenParts {
                        input: 7,
                        output: 3,
                        cached_input: 0,
                        reasoning: 0,
                        total: 10,
                        cost: Some(5),
                    },
                ),
            ],
            "device",
        );

        assert_eq!(summaries.len(), 2);
        let total_tokens: u64 = summaries
            .iter()
            .map(|summary| summary.usage.total_tokens.unwrap_or(0))
            .sum();
        assert_eq!(total_tokens, 55);
        assert!(summaries
            .iter()
            .all(|summary| summary.metadata.summary_format == "daily_rollup.v1"));
    }

    #[test]
    fn merge_provider_accounts_moves_source_records_and_prunes_alias() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-work"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("work"), None, None, None, now);
        let canonical = test_account(
            "codex",
            None,
            Some("verified@example.com"),
            Some("11111111-2222-4333-8444-555555555555"),
            Some("Plus"),
            now,
        );
        store.upsert_account(&alias).expect("alias account");
        store.upsert_account(&canonical).expect("canonical account");
        let assignment = test_assignment(
            &source,
            &alias.provider_account_id,
            now - Duration::days(40),
            None,
            now,
        );
        store
            .upsert_source_account_assignment(&assignment)
            .expect("assignment");

        let mut event = test_event(
            "codex",
            &source,
            now - Duration::days(2),
            Some(alias.provider_account_id.clone()),
            TokenParts::total(120),
        );
        event.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("event".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        let mut summary = test_summary(
            "codex",
            &source,
            now,
            300,
            Some(alias.provider_account_id.clone()),
        );
        summary.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("summary".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        store.insert_event(&event).expect("event");
        store.upsert_summary(&summary).expect("summary");
        let identity = statsai_core::AccountIdentityObservationV1 {
            schema_version: statsai_core::ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "identity-merge-alias".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(alias.provider_account_id.clone()),
            provider_user_id_hash: Some("a".repeat(64)),
            email_hash: None,
            conversation_id_hash: Some("b".repeat(64)),
            turn_id_hash: None,
            observed_at: now,
            evidence_kind: statsai_core::AccountEvidenceKind::TelemetryIdentity,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "d".repeat(64),
        };
        let plan = statsai_core::AccountPlanObservationV1 {
            schema_version: statsai_core::ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: account_plan_observation_id(
                &source.source_id,
                Some(&alias.provider_account_id),
                "plus",
                now,
                statsai_core::AccountEvidenceKind::AuthSnapshot,
            ),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(alias.provider_account_id.clone()),
            raw_plan_name: "plus".to_string(),
            plan_name: "Plus".to_string(),
            observed_at: now,
            active_from: None,
            active_until: None,
            is_current_snapshot: true,
            evidence_kind: statsai_core::AccountEvidenceKind::AuthSnapshot,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "e".repeat(64),
        };
        let binding = statsai_core::ConversationAccountBindingV1 {
            schema_version: statsai_core::CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: conversation_account_binding_id(
                &source.source_id,
                &"b".repeat(64),
                None,
                &alias.provider_account_id,
            ),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: alias.provider_account_id.clone(),
            conversation_id_hash: "b".repeat(64),
            turn_id_hash: None,
            observed_at: now,
            evidence_kind: statsai_core::AccountEvidenceKind::ResetHistory,
            confidence: Confidence::High,
        };
        store
            .upsert_account_identity_observations(std::slice::from_ref(&identity))
            .expect("identity evidence");
        store
            .upsert_account_plan_observations(std::slice::from_ref(&plan))
            .expect("plan evidence");
        store
            .upsert_conversation_account_bindings(std::slice::from_ref(&binding))
            .expect("conversation binding");

        let target = "https://api.example.com/api/sync/batches";
        store
            .record_sources_synced("http", target, &[sanitize_source_for_sync(source.clone())])
            .expect("sync source");
        store
            .record_accounts_synced(
                "http",
                target,
                &[
                    sanitize_account_for_sync(alias.clone()),
                    sanitize_account_for_sync(canonical.clone()),
                ],
            )
            .expect("sync accounts");
        store
            .record_source_account_assignments_synced(
                "http",
                target,
                &[sanitize_source_account_assignment_for_sync(
                    assignment.clone(),
                )],
            )
            .expect("sync assignments");
        store
            .record_sync_success("http", target, "batch_1", &[], &[], None)
            .expect("sync success");

        let report =
            merge_provider_accounts(&store, "codex", "work", "verified@example.com", false)
                .expect("merge");

        assert_eq!(report.moved_source_account_assignments, 1);
        assert_eq!(report.moved_subscriptions, 0);
        assert_eq!(report.moved_events, 1);
        assert_eq!(report.moved_summaries, 1);
        assert_eq!(report.moved_identity_observations, 1);
        assert_eq!(report.moved_plan_observations, 1);
        assert_eq!(report.moved_conversation_bindings, 1);
        assert!(report.deleted_source_account);
        assert!(report.reset_local_sync_tracking);
        assert_eq!(report.remaining_references.total(), 0);

        let accounts = store.list_accounts().expect("accounts");
        assert!(!accounts
            .iter()
            .any(|account| account.provider_account_id == alias.provider_account_id));
        assert!(accounts
            .iter()
            .any(|account| account.provider_account_id == canonical.provider_account_id));

        let assignments = store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].provider_account_id,
            canonical.provider_account_id
        );

        let events = store.events_for_source(&source.source_id).expect("events");
        assert_eq!(
            events[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );
        let summaries = store
            .summaries_for_source(&source.source_id)
            .expect("summaries");
        assert_eq!(
            summaries[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );
        assert!(store
            .account_identity_observations(None)
            .expect("identity evidence")
            .iter()
            .all(|observation| {
                observation.provider_account_id.as_ref() == Some(&canonical.provider_account_id)
            }));
        let plans = store.account_plan_observations().expect("plan evidence");
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].provider_account_id.as_ref(),
            Some(&canonical.provider_account_id)
        );
        assert_ne!(plans[0].observation_id, plan.observation_id);
        let bindings = store
            .conversation_account_bindings(None)
            .expect("conversation bindings");
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].provider_account_id,
            canonical.provider_account_id
        );
        assert_ne!(bindings[0].binding_id, binding.binding_id);

        assert!(store.list_sync_states().expect("sync states").is_empty());
        let sync_accounts: Vec<_> = store
            .list_accounts()
            .expect("accounts after merge")
            .into_iter()
            .map(sanitize_account_for_sync)
            .collect();
        let pending = store
            .pending_accounts_for_sync("http", target, &sync_accounts)
            .expect("pending accounts");
        assert_eq!(pending.len(), sync_accounts.len());
    }

    #[test]
    fn merge_provider_accounts_moves_orphan_summary_rows() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-legacy-alias"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("legacy-alias"), None, None, None, now);
        let canonical = test_account(
            "codex",
            None,
            Some("canonical@example.com"),
            Some("stable-provider-id"),
            Some("Plus"),
            now,
        );
        store.upsert_account(&alias).expect("alias account");
        store.upsert_account(&canonical).expect("canonical account");

        let mut summary = test_summary(
            "codex",
            &source,
            now - Duration::days(10),
            512,
            Some(alias.provider_account_id.clone()),
        );
        summary.parse_evidence = Some(statsai_core::ParseEvidence {
            event_key_version: "test".to_string(),
            source_file_path_hash: source.path_hash.clone(),
            source_line_number: None,
            source_record_id: Some("summary".to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unknown,
        });
        store.upsert_summary(&summary).expect("summary");

        let report = merge_provider_accounts(
            &store,
            "codex",
            "legacy-alias",
            "canonical@example.com",
            false,
        )
        .expect("merge");

        assert_eq!(report.moved_source_account_assignments, 0);
        assert_eq!(report.moved_subscriptions, 0);
        assert_eq!(report.moved_events, 0);
        assert_eq!(report.moved_summaries, 1);
        assert!(report.deleted_source_account);
        assert_eq!(report.remaining_references.total(), 0);

        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].provider_account_id,
            Some(canonical.provider_account_id.clone())
        );
        assert_eq!(
            summaries[0]
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.account_identity_source.clone()),
            Some(IdentitySource::UserConfigured)
        );
        assert!(store
            .list_accounts()
            .expect("accounts")
            .into_iter()
            .all(|account| account.provider_account_id != alias.provider_account_id));
    }

    #[test]
    fn remove_orphan_provider_account_rejects_referenced_account() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "codex-local-jsonl",
            "0",
            Path::new("/tmp/.codex-existing-alias"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("existing-alias"), None, None, None, now);
        store.upsert_account(&alias).expect("alias account");
        let assignment = test_assignment(
            &source,
            &alias.provider_account_id,
            now - Duration::days(1),
            None,
            now,
        );
        store
            .upsert_source_account_assignment(&assignment)
            .expect("assignment");

        let error = remove_orphan_provider_account(&store, "codex", "existing-alias", false)
            .expect_err("referenced account should fail");
        assert!(error.to_string().contains("still has references"));
    }

    #[test]
    fn remove_orphan_provider_account_deletes_account_and_clears_sync_tracking() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("now");
        let alias = test_account("codex", Some("orphan-alias"), None, None, None, now);
        store.upsert_account(&alias).expect("alias account");
        store
            .record_accounts_synced(
                "http",
                "https://api.example.com/api/sync/batches",
                &[sanitize_account_for_sync(alias.clone())],
            )
            .expect("sync account");
        store
            .record_sync_success(
                "http",
                "https://api.example.com/api/sync/batches",
                "batch_1",
                &[],
                &[],
                None,
            )
            .expect("sync success");

        let report =
            remove_orphan_provider_account(&store, "codex", "orphan-alias", false).expect("remove");
        assert!(report.deleted);
        assert!(report.reset_local_sync_tracking);
        assert!(store.list_sync_states().expect("sync states").is_empty());
        assert!(store
            .list_accounts()
            .expect("accounts")
            .into_iter()
            .all(|account| account.provider_account_id != alias.provider_account_id));
    }

    fn test_sync_command(sink: &str) -> SyncCommand {
        SyncCommand {
            sink: sink.to_string(),
            output: None,
            endpoint: None,
            auth_token: None,
            rebuild_rollups: false,
            full: false,
            since_last: false,
            status: false,
            verify: false,
            reset_remote: false,
            yes: false,
            dry_run: false,
            include_projects: false,
            exclude_projects: false,
            include_tasks: false,
            exclude_tasks: false,
        }
    }

    #[test]
    fn report_range_cli_requires_from_or_to() {
        let error =
            Cli::try_parse_from(["statsai", "report", "range"]).expect_err("range without bounds");
        let message = error.to_string();
        assert!(
            message.contains("--from") || message.contains("--to"),
            "{message}"
        );
    }

    #[test]
    fn supported_store_schema_version_is_available_without_opening_a_store() {
        let cli = Cli::try_parse_from(["statsai", "store", "supported-schema-version"])
            .expect("parse supported schema query");

        assert!(matches!(
            cli.command,
            Command::Store(StoreAdminCommand {
                command: StoreAdminSubcommand::SupportedSchemaVersion,
            })
        ));

        let directory = tempfile::tempdir().expect("temporary directory");
        let store_path = directory.path().join("must-not-be-created.sqlite");
        store_admin(
            StoreAdminCommand {
                command: StoreAdminSubcommand::SupportedSchemaVersion,
            },
            &store_path,
        )
        .expect("print supported schema");
        assert!(!store_path.exists());
    }

    #[test]
    fn supported_pricing_ruleset_version_is_available_without_opening_a_store() {
        let cli = Cli::try_parse_from(["statsai", "store", "supported-pricing-ruleset-version"])
            .expect("parse supported pricing query");

        assert!(matches!(
            cli.command,
            Command::Store(StoreAdminCommand {
                command: StoreAdminSubcommand::SupportedPricingRulesetVersion,
            })
        ));

        let directory = tempfile::tempdir().expect("temporary directory");
        let store_path = directory.path().join("must-not-be-created.sqlite");
        store_admin(
            StoreAdminCommand {
                command: StoreAdminSubcommand::SupportedPricingRulesetVersion,
            },
            &store_path,
        )
        .expect("print supported pricing ruleset");
        assert!(!store_path.exists());
    }

    #[test]
    fn price_derived_commands_reprice_and_diagnostic_commands_do_not() {
        let reprice = |args: &[&str]| {
            let cli = Cli::try_parse_from(args).expect("parse");
            command_reprices_persisted_usage(&cli.command)
        };
        assert!(reprice(&["statsai", "scan"]));
        assert!(reprice(&["statsai", "report", "monthly"]));
        assert!(reprice(&["statsai", "sync"]));
        assert!(reprice(&["statsai", "export", "--json"]));
        assert!(reprice(&["statsai", "task", "list"]));
        assert!(!reprice(&["statsai", "status"]));
        assert!(!reprice(&["statsai", "quota", "status"]));
        assert!(!reprice(&["statsai", "conversation", "list"]));
        assert!(!reprice(&["statsai", "account", "list"]));
        assert!(!reprice(&["statsai", "source", "list"]));
        let doctor = Cli::try_parse_from(["statsai", "doctor"]).expect("parse doctor");
        assert!(!command_reprices_persisted_usage(&doctor.command));
    }

    #[test]
    fn report_range_cli_parses_from_and_to() {
        let cli = Cli::try_parse_from([
            "statsai",
            "report",
            "range",
            "--from",
            "2026-01-01",
            "--to",
            "2026-03-31",
            "--json",
        ])
        .expect("parse report range");
        assert!(matches!(
            cli.command,
            Command::Report(ReportCommand {
                command: ReportSubcommand::Range {
                    from: Some(ref from),
                    to: Some(ref to),
                    json: true,
                    verbose: false,
                    subscriptions: false,
                },
            }) if from == "2026-01-01" && to == "2026-03-31"
        ));
    }

    #[test]
    fn report_range_cli_rfc3339_midnight_keeps_timestamp_label() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: Some("2026-05-01T00:00:00Z".to_string()),
                to: Some("2026-05-15".to_string()),
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) =
            usage_report_from_command(command, &store, now).expect("rfc3339 midnight from");
        assert_eq!(report.label, "2026-05-01T00:00:00+00:00 to 2026-05-15");
    }

    #[test]
    fn report_range_cli_filters_stored_events() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-range"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let before = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
                .single()
                .expect("before"),
            None,
            TokenParts::total(50),
        );
        let inside = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
                .single()
                .expect("inside"),
            None,
            TokenParts::total(100),
        );
        let after = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 5, 20, 9, 0, 0)
                .single()
                .expect("after"),
            None,
            TokenParts::total(200),
        );
        store
            .insert_events(&[before, inside, after])
            .expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: Some("2026-05-01".to_string()),
                to: Some("2026-05-15".to_string()),
                json: true,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, json, verbose, subscriptions) =
            usage_report_from_command(command, &store, now).expect("range report");

        assert!(json);
        assert!(!verbose);
        assert!(!subscriptions);
        assert_eq!(report.label, "2026-05-01 to 2026-05-15");
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_range_cli_from_only_includes_events_through_now() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-from-only"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let before = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
                .single()
                .expect("before"),
            None,
            TokenParts::total(50),
        );
        let inside = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
                .single()
                .expect("inside"),
            None,
            TokenParts::total(100),
        );
        store
            .insert_events(&[before, inside])
            .expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: Some("2026-05-01".to_string()),
                to: None,
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) = usage_report_from_command(command, &store, now).expect("from-only");
        assert_eq!(report.label, "2026-05-01 to 2026-05-25T12:00:00+00:00");
        assert_eq!(report.until, now);
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_range_cli_to_only_includes_events_through_end_date() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-to-only"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let inside = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 5, 10, 18, 0, 0)
                .single()
                .expect("inside"),
            None,
            TokenParts::total(100),
        );
        let after = test_event(
            "codex",
            &source,
            Utc.with_ymd_and_hms(2026, 5, 20, 9, 0, 0)
                .single()
                .expect("after"),
            None,
            TokenParts::total(200),
        );
        store
            .insert_events(&[inside, after])
            .expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: None,
                to: Some("2026-05-15".to_string()),
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) = usage_report_from_command(command, &store, now).expect("to-only");
        assert_eq!(report.label, "through 2026-05-15");
        assert_eq!(report.since, None);
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
    }

    #[test]
    fn report_range_cli_to_only_includes_pre_unix_events() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-pre-unix"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let pre_unix = Utc
            .with_ymd_and_hms(1969, 12, 31, 12, 0, 0)
            .single()
            .expect("pre-unix");
        store
            .insert_events(&[test_event(
                "codex",
                &source,
                pre_unix,
                None,
                TokenParts::total(40),
            )])
            .expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: None,
                to: Some("1969-12-31".to_string()),
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) = usage_report_from_command(command, &store, now).expect("pre-unix range");
        assert_eq!(report.label, "through 1969-12-31");
        assert_eq!(report.since, None);
        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 40);
    }

    #[test]
    fn report_range_cli_future_window_is_empty_not_an_error() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-future"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let present = test_event("codex", &source, now, None, TokenParts::total(50));
        store.insert_events(&[present]).expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: Some("2026-09-01".to_string()),
                to: Some("2026-09-30".to_string()),
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) = usage_report_from_command(command, &store, now).expect("future range");
        assert_eq!(report.label, "2026-09-01 to 2026-09-30 (empty)");
        assert_eq!(report.since, Some(now));
        assert_eq!(report.until, now);
        assert!(report.since.is_some_and(|since| since <= report.until));
        assert_eq!(report.total_events, 0);
    }

    #[test]
    fn report_range_cli_future_from_only_is_empty_not_an_error() {
        let store = Store::in_memory().expect("store");
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("now");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-future-from"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let present = test_event("codex", &source, now, None, TokenParts::total(50));
        store.insert_events(&[present]).expect("insert events");

        let command = ReportCommand {
            command: ReportSubcommand::Range {
                from: Some("2026-09-01".to_string()),
                to: None,
                json: false,
                verbose: false,
                subscriptions: false,
            },
        };
        let (report, ..) =
            usage_report_from_command(command, &store, now).expect("future from-only");
        assert_eq!(report.label, "from 2026-09-01 (empty)");
        assert_eq!(report.since, Some(now));
        assert_eq!(report.until, now);
        assert!(report.since.is_some_and(|since| since <= report.until));
        assert_eq!(report.total_events, 0);
    }

    #[test]
    fn usage_report_filters_period_and_groups_by_canonical_account() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report"),
            LocationOrigin::Configured,
        );
        let account_id = provider_account_id("codex", "personal@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: Some("personal@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: None,
            confidence: Confidence::High,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let recent = test_event(
            "codex",
            &source,
            now - Duration::days(1),
            Some(account_id.clone()),
            TokenParts {
                input: 70,
                cached_input: 20,
                output: 25,
                reasoning: 5,
                total: 100,
                cost: Some(1),
            },
        );
        let old = test_event(
            "codex",
            &source,
            now - Duration::days(10),
            Some(account_id),
            TokenParts {
                input: 120,
                cached_input: 30,
                output: 50,
                reasoning: 0,
                total: 200,
                cost: Some(1),
            },
        );

        let report = build_usage_report(
            &[recent, old],
            &[],
            &[source],
            &[account],
            &[],
            ReportPeriod::LastDays(7),
            now,
        );

        assert_eq!(report.total_events, 1);
        assert_eq!(report.total_usage.total_tokens, 100);
        assert_eq!(report.total_usage.input_tokens, 70);
        assert_eq!(report.total_usage.cached_input_tokens, 20);
        assert_eq!(report.total_usage.output_tokens, 25);
        assert_eq!(report.total_usage.reasoning_tokens, 5);
        assert_eq!(report.total_usage.estimated_cost_usd, Some(1));
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "personal");
    }

    #[test]
    fn usage_report_uses_account_registry_label() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/codex-report-account"),
            LocationOrigin::Configured,
        );
        let account_id = provider_account_id("codex", "stable-provider-id");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "codex".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: None,
            email_hash: None,
            org_id_hash: None,
            account_label: Some("work".to_string()),
            plan_name: None,
            confidence: Confidence::Medium,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let event = test_event(
            "codex",
            &source,
            now,
            Some(account_id),
            TokenParts::total(50),
        );

        let report = build_usage_report(
            &[event],
            &[],
            &[source],
            &[account],
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].account, "work");
        assert_eq!(report.rows[0].usage.total_tokens, 50);
    }

    #[test]
    fn usage_report_keeps_summary_cache_separate_from_event_totals() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-report-summary"),
            LocationOrigin::Configured,
        );
        let account_id = provider_account_id("claude_code", "personal@example.com");
        let account = ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id: account_id.clone(),
            provider: "claude_code".to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: None,
            provider_user_id_hash: None,
            email: Some("personal@example.com".to_string()),
            email_hash: None,
            org_id_hash: None,
            account_label: Some("personal".to_string()),
            plan_name: None,
            confidence: Confidence::High,
            verified_at: None,
            created_at: now,
            updated_at: now,
        };
        let event = test_event(
            "claude_code",
            &source,
            now,
            Some(account_id.clone()),
            TokenParts::total(100),
        );
        let summary = test_summary("claude_code", &source, now, 500, Some(account_id.clone()));

        let report = build_usage_report(
            &[event],
            &[summary],
            std::slice::from_ref(&source),
            std::slice::from_ref(&account),
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.total_usage.total_tokens, 100);
        assert_eq!(report.total_summary_usage.total_tokens, 500);
        assert_eq!(report.summary_rows.len(), 1);
        assert_eq!(report.summary_rows[0].account, "personal");
        assert_eq!(report.summary_rows[0].direct_event_usage.total_tokens, 100);

        let weekly = build_usage_report(
            &[],
            &[test_summary(
                "claude_code",
                &source,
                now,
                500,
                Some(account_id),
            )],
            std::slice::from_ref(&source),
            std::slice::from_ref(&account),
            &[],
            ReportPeriod::LastDays(7),
            now,
        );
        assert!(weekly.summary_rows.is_empty());
    }

    #[test]
    fn usage_report_keeps_summary_formats_separate() {
        let now = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .expect("date");
        let source = SourceLocation::local_adapter(
            "claude_code",
            "test",
            "0",
            Path::new("/tmp/claude-report-summary-kinds"),
            LocationOrigin::Configured,
        );
        let account_id = provider_account_id("claude_code", "personal@example.com");
        let mut stats_cache =
            test_summary("claude_code", &source, now, 500, Some(account_id.clone()));
        stats_cache.metadata.summary_format = "claude_stats_cache".to_string();
        let mut external = test_summary("claude_code", &source, now, 300, Some(account_id));
        external.summary_id = summary_id("claude_code", &source.source_id, "external");
        external.metadata.summary_format = "external_daily".to_string();

        let report = build_usage_report(
            &[],
            &[stats_cache, external],
            std::slice::from_ref(&source),
            &[],
            &[],
            ReportPeriod::AllTime,
            now,
        );

        assert_eq!(report.summary_rows.len(), 2);
        assert!(report
            .summary_rows
            .iter()
            .any(|row| row.kind == "claude_stats_cache" && row.usage.total_tokens == 500));
        assert!(report
            .summary_rows
            .iter()
            .any(|row| row.kind == "external_daily" && row.usage.total_tokens == 300));
    }

    struct TokenParts {
        input: u64,
        cached_input: u64,
        output: u64,
        reasoning: u64,
        total: u64,
        cost: Option<i64>, // cents
    }

    impl TokenParts {
        fn total(total: u64) -> Self {
            Self {
                input: 0,
                cached_input: 0,
                output: 0,
                reasoning: 0,
                total,
                cost: None,
            }
        }
    }

    fn test_account(
        provider: &str,
        label: Option<&str>,
        email: Option<&str>,
        provider_user_id: Option<&str>,
        plan_name: Option<&str>,
        now: DateTime<Utc>,
    ) -> ProviderAccount {
        let provider_account_id =
            provider_account_id_from_identity(provider, provider_user_id, email)
                .unwrap_or_else(|| provider_account_id(provider, label.expect("label")));
        let normalized_email = email.map(normalize_email);
        ProviderAccount {
            schema_version: PROVIDER_ACCOUNT_SCHEMA_VERSION.to_string(),
            provider_account_id,
            provider: provider.to_string(),
            identity_source: IdentitySource::UserConfigured,
            provider_user_id: provider_user_id.map(ToOwned::to_owned),
            provider_user_id_hash: provider_user_id.map(hash_text),
            email_hash: normalized_email.as_deref().map(hash_text),
            email: normalized_email,
            org_id_hash: None,
            account_label: label.map(ToOwned::to_owned),
            plan_name: plan_name.map(ToOwned::to_owned),
            confidence: if email.is_some() || provider_user_id.is_some() {
                Confidence::High
            } else {
                Confidence::Medium
            },
            verified_at: email.map(|_| now),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_assignment(
        source: &SourceLocation,
        provider_account_id: &statsai_core::ProviderAccountId,
        started_at: DateTime<Utc>,
        ended_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> SourceAccountAssignment {
        SourceAccountAssignment {
            schema_version: SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION.to_string(),
            assignment_id: source_account_assignment_id(
                &source.source_id,
                provider_account_id,
                started_at,
            ),
            source_id: source.source_id.clone(),
            provider: source.provider.clone(),
            provider_account_id: provider_account_id.clone(),
            started_at,
            ended_at,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_event(
        provider: &str,
        source: &SourceLocation,
        started_at: DateTime<Utc>,
        provider_account_id: Option<statsai_core::ProviderAccountId>,
        tokens: TokenParts,
    ) -> UsageEvent {
        UsageEvent {
            schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
            event_id: event_id(
                provider,
                &source.source_id,
                &started_at.to_rfc3339(),
                None,
                started_at,
            ),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id,
            subscription_id: None,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalAdapter,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "jsonl".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some(started_at.to_rfc3339()),
                parse_confidence: Confidence::High,
            },
            session: SessionInfo {
                session_id: "session".to_string(),
                local_session_id_hash: None,
                title: None,
                started_at,
                ended_at: None,
                duration_seconds: None,
            },
            model: None,
            usage: UsageCounts {
                input_tokens: (tokens.input > 0).then_some(tokens.input),
                output_tokens: (tokens.output > 0).then_some(tokens.output),
                cache_read_tokens: (tokens.cached_input > 0).then_some(tokens.cached_input),
                reasoning_tokens: (tokens.reasoning > 0).then_some(tokens.reasoning),
                total_tokens: Some(tokens.total),
                ..UsageCounts::default()
            },
            runtime: None,
            cost: CostInfo {
                currency: "USD".to_string(),
                estimated_api_equivalent_usd: tokens.cost,
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
            created_at: started_at,
            imported_at: started_at,
        }
    }

    fn test_summary(
        provider: &str,
        source: &SourceLocation,
        now: DateTime<Utc>,
        total: u64,
        provider_account_id: Option<statsai_core::ProviderAccountId>,
    ) -> UsageSummary {
        UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(provider, &source.source_id, "summary"),
            device_id: "device".to_string(),
            provider: provider.to_string(),
            source_id: source.source_id.clone(),
            provider_account_id,
            source: EventSource {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(LocationOrigin::Configured),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: source.path_hash.clone(),
                source_record_id: Some("summary".to_string()),
                parse_confidence: Confidence::Medium,
            },
            model: Some(ModelInfo {
                name: Some("claude-test".to_string()),
                normalized_name: Some("claude-test".to_string()),
                provider_model_id: Some("claude-test".to_string()),
                speed: None,
                reasoning_level: None,
                reasoning_level_raw: None,
            }),
            models: Vec::new(),
            usage: UsageCounts {
                input_tokens: Some(total),
                total_tokens: Some(total),
                ..UsageCounts::default()
            },
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
            privacy: PrivacyInfo {
                mode: PrivacyMode::MetadataOnly,
                contains_prompt_text: false,
                contains_response_text: false,
                contains_file_paths: false,
            },
            metrics: None,
            period_start: Some(now - Duration::days(30)),
            period_end: Some(now),
            observed_at: now,
            metadata: SummaryMetadata {
                summary_format: "test".to_string(),
                summary_version: Some("1".to_string()),
                total_sessions: Some(1),
                total_messages: Some(2),
                last_computed_at: Some(now),
            },
            imported_at: now,
        }
    }

    fn test_scan_candidate(path: &str, cache_signature: &str) -> ScanCandidateFile {
        ScanCandidateFile {
            path: PathBuf::from(path),
            cache_key: path.to_string(),
            cache_signature: cache_signature.to_string(),
            compatible_cache_signatures: Vec::new(),
        }
    }

    fn test_scan_event(
        source: &SourceLocation,
        file_path: &str,
        started_at: DateTime<Utc>,
        record_id: &str,
        total_tokens: u64,
    ) -> UsageEvent {
        let mut event = test_event(
            "codex",
            source,
            started_at,
            None,
            TokenParts::total(total_tokens),
        );
        event.source.source_record_id = Some(record_id.to_string());
        event.parse_evidence = Some(ParseEvidence {
            event_key_version: "test-scan.v1".to_string(),
            source_file_path_hash: Some(hash_text(file_path)),
            source_line_number: Some(1),
            source_record_id: Some(record_id.to_string()),
            model_inferred: false,
            timestamp_inferred: false,
            account_identity_source: IdentitySource::Unresolved,
        });
        event
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

    fn test_task_span(
        source: &SourceLocation,
        file_path: &str,
        started_at: DateTime<Utc>,
        record_id: &str,
        title: &str,
        event: &UsageEvent,
    ) -> TaskSpan {
        TaskSpan {
            schema_version: TASK_SPAN_SCHEMA_VERSION.to_string(),
            span_id: task_span_id("codex", &source.source_id, record_id),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            span_kind: "codex_task".to_string(),
            source_record_id: Some(record_id.to_string()),
            source_file_path_hash: Some(hash_text(file_path)),
            summary_id: None,
            session_id: Some("session-test".to_string()),
            thread_id: None,
            title: title.to_string(),
            normalized_title: normalize_task_title(title),
            title_source: Some("thread_name".to_string()),
            summary_preview: Some(title.to_string()),
            todo_excerpt: None,
            issue_keys: Vec::new(),
            branch_family: None,
            project_bucket: project_bucket_key(event.project.as_ref()),
            project: event.project.clone(),
            git: None,
            usage: event.usage.clone(),
            estimated_cost_usd: event.cost.estimated_api_equivalent_usd,
            estimated_cost_micro_usd: event.cost.estimated_api_equivalent_micro_usd,
            event_count: 1,
            has_usage_evidence: true,
            total_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.total_messages)
                .unwrap_or(0),
            user_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.user_messages)
                .unwrap_or(0),
            assistant_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.assistant_messages)
                .unwrap_or(0),
            developer_messages: event
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.developer_messages)
                .unwrap_or(0),
            linked_event_ids: vec![event.event_id.clone()],
            confidence: Confidence::High,
            is_meta: false,
            started_at,
            ended_at: Some(started_at),
            duration_seconds: Some(0),
        }
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
        duplicate_event.event_id =
            event_id("codex", &source.source_id, "duplicate", None, started_at);
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

    #[test]
    fn usd_amount_json_uses_major_units() {
        assert_eq!(usd_amount_json(Some(125)), json!(1.25));
        assert_eq!(usd_amount_json(None), Value::Null);
    }

    #[test]
    fn subscription_json_value_preserves_major_unit_price() {
        let started_at = Utc
            .with_ymd_and_hms(2026, 5, 29, 10, 12, 43)
            .single()
            .expect("date");
        let subscription = Subscription {
            schema_version: SUBSCRIPTION_SCHEMA_VERSION.to_string(),
            subscription_id: subscription_id(
                "codex",
                &provider_account_id("codex", "acct-test"),
                "Plus",
                started_at,
            ),
            provider: "codex".to_string(),
            provider_account_id: provider_account_id("codex", "acct-test"),
            plan_name: "Plus".to_string(),
            price: 2000,
            currency: "USD".to_string(),
            billing_period: BillingPeriod::Monthly,
            paid_at: Some(started_at),
            renewal_day: Some(29),
            started_at,
            ended_at: None,
            current_period_ends_at: None,
            status: SubscriptionStatus::Active,
            record_source: IdentitySource::UserConfigured,
            verified_at: None,
            notes: None,
        };

        let value = subscription_json_value(&subscription);

        assert_eq!(value["price"], json!(20.0));
        assert_eq!(value["price_cents"], json!(2000));
    }

    fn test_unattributed_quota_window(source_id: &str, window_id: &str) -> QuotaWindowV1 {
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .single()
            .expect("observed at");
        let reset = observed_at + Duration::days(7);
        QuotaWindowV1 {
            schema_version: "quota_window.v1".to_string(),
            window_id: window_id.to_string(),
            provider: "codex".to_string(),
            provider_account_id: None,
            source_id: Some(SourceId(source_id.to_string())),
            limit_id: Some("subscription".to_string()),
            window_minutes: 10_080,
            inferred_start: reset - Duration::days(7),
            representative_reset: reset,
            representative_reset_epoch_seconds: reset.timestamp(),
            reset_min: reset,
            reset_min_epoch_seconds: reset.timestamp(),
            reset_max: reset,
            reset_max_epoch_seconds: reset.timestamp(),
            first_observed_at: observed_at,
            last_observed_at: observed_at,
            sample_count: 1,
            first_used_percent: 20.0,
            latest_used_percent: 20.0,
            minimum_used_percent: 20.0,
            maximum_used_percent: 20.0,
            transition: statsai_core::QuotaTransitionKind::Initial,
            has_schedule_overlap: false,
            change_points: Vec::new(),
            latest_status: statsai_core::QuotaStatusV1::default(),
            usage_totals: None,
        }
    }

    fn test_unattributed_quota_record(source_id: &str) -> QuotaObservationRecordV1 {
        let observed_at = Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .single()
            .expect("observed at");
        let reset = observed_at + Duration::days(7);
        QuotaObservationRecordV1 {
            observation: statsai_core::QuotaObservationV1 {
                schema_version: "quota_observation.v1".to_string(),
                observation_id: format!("observation-{source_id}"),
                semantic_fingerprint: format!("semantic-{source_id}"),
                provider: "codex".to_string(),
                source_id: SourceId(source_id.to_string()),
                provider_account_id: None,
                observed_at,
                source_file_path_hash: format!("file-{source_id}"),
                source_record_id: format!("record-{source_id}"),
                source_line_number: 1,
                payload_hash: format!("payload-{source_id}"),
                usage_sample: None,
                usage_event_id: None,
                usage_link_kind: statsai_core::QuotaUsageLinkKind::None,
                status: statsai_core::QuotaStatusV1::default(),
            },
            windows: vec![statsai_core::QuotaWindowObservationV1 {
                schema_version: "quota_window_observation.v1".to_string(),
                window_observation_id: format!("window-observation-{source_id}"),
                observation_id: format!("observation-{source_id}"),
                provider_slot: "primary".to_string(),
                limit_id: Some("subscription".to_string()),
                window_minutes: 10_080,
                used_percent: 20.0,
                resets_at: reset,
                resets_at_epoch_seconds: reset.timestamp(),
            }],
            raw_rate_limits: json!({}),
        }
    }

    #[test]
    fn current_quota_windows_keep_unattributed_source_scopes_separate() {
        let selected = select_current_quota_windows(
            vec![
                test_unattributed_quota_window("source-a", "window-a"),
                test_unattributed_quota_window("source-b", "window-b"),
            ],
            false,
            false,
        );

        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected
                .iter()
                .filter_map(|window| window.source_id.as_ref())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn raw_quota_history_isolated_to_unattributed_window_source() {
        let window = test_unattributed_quota_window("source-a", "window-a");
        let observations = raw_observations_for_window(
            vec![
                test_unattributed_quota_record("source-a"),
                test_unattributed_quota_record("source-b"),
            ],
            &window,
        );

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].observation.source_id.0, "source-a");
    }
}
