use super::{
    assignment_for_timestamp, deserialize_subscription_payload, is_verified_source_assignment,
    reattribute_source_records, validate_time_window, Store,
};
use anyhow::Result;
use rusqlite::params;
use statsai_core::{
    account_plan_observation_id, conversation_account_binding_id, hash_text, normalize_plan_name,
    periods_overlap, plan_projection_from_observation, source_account_assignment_id,
    AccountEvidenceCheckpointV1, AccountEvidenceKind, AccountEvidenceSummaryV1,
    AccountIdentityObservationV1, AccountPlanObservationV1, AccountPlanProjectionV1, Confidence,
    ConversationAccountBindingV1, IdentitySource, ProviderAccountId, QuotaObservationRecordV1,
    SourceAccountAssignment, SourceId, UsageEvent, ACCOUNT_EVIDENCE_SUMMARY_SCHEMA_VERSION,
    ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION, SOURCE_ACCOUNT_ASSIGNMENT_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

mod evidence;
mod observations;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountEvidenceReferenceCounts {
    pub identity_observations: usize,
    pub plan_observations: usize,
    pub conversation_bindings: usize,
}

impl AccountEvidenceReferenceCounts {
    #[must_use]
    pub const fn total(self) -> usize {
        self.identity_observations + self.plan_observations + self.conversation_bindings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use statsai_core::{
        LocationOrigin, SourceLocation, ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION,
        ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION, CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION,
    };
    use std::path::Path;

    #[test]
    fn source_evidence_cleanup_removes_identity_plan_and_conversation_rows() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/account-evidence-cleanup"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let observed_at = Utc::now();
        let account_id = ProviderAccountId("account-cleanup".to_string());
        let identity = AccountIdentityObservationV1 {
            schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "identity-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            provider_user_id_hash: Some("a".repeat(64)),
            email_hash: None,
            conversation_id_hash: Some("b".repeat(64)),
            turn_id_hash: None,
            observed_at,
            evidence_kind: AccountEvidenceKind::TelemetryIdentity,
            confidence: Confidence::High,
            auth_mode: Some("chatgpt".to_string()),
            application_version: None,
            parser_version: "test.v1".to_string(),
            artifact_kind: "test".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "d".repeat(64),
        };
        let plan = AccountPlanObservationV1 {
            schema_version: ACCOUNT_PLAN_OBSERVATION_SCHEMA_VERSION.to_string(),
            observation_id: "plan-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: Some(account_id.clone()),
            raw_plan_name: "pro".to_string(),
            plan_name: "Pro".to_string(),
            observed_at,
            active_from: None,
            active_until: None,
            is_current_snapshot: false,
            evidence_kind: AccountEvidenceKind::QuotaStatus,
            confidence: Confidence::High,
            parser_version: "test.v1".to_string(),
            artifact_path_hash: "c".repeat(64),
            record_fingerprint: "e".repeat(64),
        };
        let binding = ConversationAccountBindingV1 {
            schema_version: CONVERSATION_ACCOUNT_BINDING_SCHEMA_VERSION.to_string(),
            binding_id: "binding-cleanup".to_string(),
            provider: "codex".to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: account_id,
            conversation_id_hash: "b".repeat(64),
            turn_id_hash: None,
            observed_at,
            evidence_kind: AccountEvidenceKind::ResetHistory,
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
            .expect("conversation evidence");
        let checkpoint = AccountEvidenceCheckpointV1 {
            schema_version: ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION.to_string(),
            source_id: source.source_id.clone(),
            artifact_path_hash: "f".repeat(64),
            parser_version: "test.v1".to_string(),
            maximum_row_id: 42,
            checkpoint_row_fingerprint: Some("1".repeat(64)),
            database_size: 100,
            database_modified_nanos: 200,
            wal_size: 10,
            wal_modified_nanos: 300,
        };
        store
            .upsert_account_evidence_checkpoints(std::slice::from_ref(&checkpoint))
            .expect("checkpoint");

        let mut identities = vec![identity];
        let mut plans = vec![plan];
        let mut bindings = vec![binding];
        store
            .retain_unseen_account_evidence(
                &source.source_id,
                &mut identities,
                &mut plans,
                &mut bindings,
            )
            .expect("filter known evidence");
        assert!(identities.is_empty() && plans.is_empty() && bindings.is_empty());

        assert_eq!(
            store
                .delete_account_evidence_for_sources(std::slice::from_ref(&source.source_id))
                .expect("delete source evidence"),
            4
        );
        assert!(store
            .account_identity_observations(Some(&source.source_id))
            .expect("identities")
            .is_empty());
        assert!(store.account_plan_observations().expect("plans").is_empty());
        assert!(store
            .conversation_account_bindings(Some(&source.source_id))
            .expect("bindings")
            .is_empty());
        assert!(store
            .account_evidence_checkpoints(&source.source_id)
            .expect("checkpoints")
            .is_empty());
    }

    #[test]
    fn incremental_identity_upserts_slide_run_endpoints_across_scans() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/account-evidence-endpoint-slide"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let base = Utc::now();
        let make = |id: &str, minutes: i64, account: &str, kind: AccountEvidenceKind| {
            AccountIdentityObservationV1 {
                schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: id.to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(ProviderAccountId(account.to_string())),
                provider_user_id_hash: None,
                email_hash: None,
                conversation_id_hash: None,
                turn_id_hash: None,
                observed_at: base + chrono::Duration::minutes(minutes),
                evidence_kind: kind,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "c".repeat(64),
                record_fingerprint: id.to_string(),
            }
        };
        let ledger_ids = || {
            let mut observations = store
                .account_identity_observations(Some(&source.source_id))
                .expect("identities");
            observations.sort_by_key(|observation| observation.observed_at);
            observations
                .into_iter()
                .map(|observation| observation.observation_id)
                .collect::<Vec<_>>()
        };
        let telemetry = AccountEvidenceKind::TelemetryIdentity;

        // First scan persists a collapsed run's endpoints.
        store
            .upsert_account_identity_observations(&[
                make("a-0", 0, "acct-a", telemetry),
                make("a-10", 10, "acct-a", telemetry),
            ])
            .expect("first scan");
        // Each later incremental scan sees only rows past its checkpoint; the
        // new point must replace the persisted endpoint, not stack behind it.
        store
            .upsert_account_identity_observations(&[make("a-20", 20, "acct-a", telemetry)])
            .expect("second scan");
        assert_eq!(ledger_ids(), vec!["a-0", "a-20"]);

        // An account switch always survives: the differing row breaks the run.
        store
            .upsert_account_identity_observations(&[
                make("a-30", 30, "acct-a", telemetry),
                make("b-40", 40, "acct-b", telemetry),
                make("b-50", 50, "acct-b", telemetry),
            ])
            .expect("third scan");
        store
            .upsert_account_identity_observations(&[make("b-60", 60, "acct-b", telemetry)])
            .expect("fourth scan");
        assert_eq!(ledger_ids(), vec!["a-0", "a-30", "b-40", "b-60"]);

        // Replaying a persisted endpoint (full rescan) must not consume it.
        store
            .upsert_account_identity_observations(&[make("b-60", 60, "acct-b", telemetry)])
            .expect("replayed endpoint");
        // Non-collapsible evidence never slides an endpoint.
        store
            .upsert_account_identity_observations(&[make(
                "snap-70",
                70,
                "acct-b",
                AccountEvidenceKind::AuthSnapshot,
            )])
            .expect("auth snapshot");
        assert_eq!(ledger_ids(), vec!["a-0", "a-30", "b-40", "b-60", "snap-70"]);
    }

    #[test]
    fn auth_reload_confirmation_cannot_cross_an_account_boundary() {
        let store = Store::in_memory().expect("store");
        let source = SourceLocation::local_adapter(
            "codex",
            "test",
            "0",
            Path::new("/tmp/account-evidence-boundary-confirmation"),
            LocationOrigin::Configured,
        );
        store.upsert_source(&source).expect("source");
        let base = Utc::now();
        let make = |id: &str, minutes: i64, account: &str, evidence_kind: AccountEvidenceKind| {
            AccountIdentityObservationV1 {
                schema_version: ACCOUNT_IDENTITY_OBSERVATION_SCHEMA_VERSION.to_string(),
                observation_id: id.to_string(),
                provider: "codex".to_string(),
                source_id: source.source_id.clone(),
                provider_account_id: Some(ProviderAccountId(account.to_string())),
                provider_user_id_hash: None,
                email_hash: None,
                conversation_id_hash: None,
                turn_id_hash: None,
                observed_at: base + chrono::Duration::minutes(minutes),
                evidence_kind,
                confidence: Confidence::High,
                auth_mode: None,
                application_version: None,
                parser_version: "test.v1".to_string(),
                artifact_kind: "test".to_string(),
                artifact_path_hash: "path".to_string(),
                record_fingerprint: id.to_string(),
            }
        };
        store
            .upsert_account_identity_observations(&[
                make("reload-a", 1, "account-a", AccountEvidenceKind::AuthReload),
                make("reload-b", 2, "account-b", AccountEvidenceKind::AuthReload),
                make(
                    "telemetry-a",
                    3,
                    "account-a",
                    AccountEvidenceKind::TelemetryIdentity,
                ),
            ])
            .expect("identity evidence");

        assert_eq!(
            store
                .reconcile_source_account_evidence_assignments(&source.source_id)
                .expect("reconcile evidence"),
            0
        );
        assert!(store
            .list_source_account_assignments_for_source(&source.source_id)
            .expect("assignments")
            .is_empty());
    }

    #[test]
    fn account_evidence_checkpoint_persists_and_rolls_back_transactionally() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("statsai.sqlite");
        let source_id = SourceId("checkpoint-source".to_string());
        let checkpoint = AccountEvidenceCheckpointV1 {
            schema_version: ACCOUNT_EVIDENCE_CHECKPOINT_SCHEMA_VERSION.to_string(),
            source_id: source_id.clone(),
            artifact_path_hash: "a".repeat(64),
            parser_version: "test.v1".to_string(),
            maximum_row_id: 84,
            checkpoint_row_fingerprint: Some("2".repeat(64)),
            database_size: 200,
            database_modified_nanos: 300,
            wal_size: 20,
            wal_modified_nanos: 400,
        };
        {
            let store = Store::open(&path).expect("open store");
            store
                .upsert_account_evidence_checkpoints(std::slice::from_ref(&checkpoint))
                .expect("persist checkpoint");
        }
        let store = Store::open(&path).expect("reopen store");
        assert_eq!(
            store
                .account_evidence_checkpoints(&source_id)
                .expect("load checkpoint"),
            vec![checkpoint.clone()]
        );

        let replacement = AccountEvidenceCheckpointV1 {
            maximum_row_id: 100,
            ..checkpoint.clone()
        };
        let error = store
            .apply_scan_update(|store| -> Result<()> {
                store.upsert_account_evidence_checkpoints(std::slice::from_ref(&replacement))?;
                anyhow::bail!("force rollback")
            })
            .expect_err("transaction must fail");
        assert_eq!(error.to_string(), "force rollback");
        assert_eq!(
            store
                .account_evidence_checkpoints(&source_id)
                .expect("load checkpoint after rollback"),
            vec![checkpoint]
        );
    }
}
