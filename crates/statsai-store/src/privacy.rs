use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::Store;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivacyFindingRecord {
    pub field_path: String,
    pub start: u64,
    pub end: u64,
    pub category: String,
    pub detector: String,
    pub confidence: Option<String>,
    pub replacement: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilteredConversationRecord {
    #[serde(skip_serializing)]
    pub conversation_id: String,
    pub dataset_key: String,
    pub input_fingerprint: String,
    pub policy_fingerprint: String,
    pub payload: String,
    pub finding_count: u64,
    pub succeeded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilteredConversationMetadata {
    pub conversation_id: String,
    pub dataset_key: String,
    pub input_fingerprint: String,
    pub policy_fingerprint: String,
    pub finding_count: u64,
    pub succeeded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivacyFailureRecord {
    pub conversation_id: String,
    pub input_fingerprint: String,
    pub policy_fingerprint: String,
    pub failed_stage: String,
    pub error_code: String,
    pub attempted_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivacyDatasetStatus {
    pub archived: u64,
    pub filtered: u64,
    pub current: u64,
    pub stale: u64,
    pub failed: u64,
}

impl Store {
    /// Serializes first-time privacy identity initialization across processes.
    ///
    /// The callback may access external key storage while SQLite holds the
    /// write reservation. Nested store writes join the same transaction.
    pub fn with_privacy_identity_initialization<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        self.with_immediate_transaction(|| operation(self))
    }

    fn require_privacy_key_verifier(&self) -> Result<()> {
        if self.privacy_key_verifier()?.is_none() {
            bail!("privacy dataset key verifier is not initialized")
        }
        Ok(())
    }

    pub fn privacy_pseudonym_count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM privacy_pseudonyms", [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
    }

    pub fn privacy_identity_state_exists(&self) -> Result<bool> {
        self.conn
            .query_row(
                r#"
                SELECT EXISTS(
                  SELECT 1 FROM privacy_pseudonyms
                  UNION ALL
                  SELECT 1 FROM filtered_conversations
                )
                "#,
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn privacy_key_verifier(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT key_verifier FROM privacy_dataset_identity WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn ensure_privacy_key_verifier(&self, verifier: &str) -> Result<()> {
        if verifier.len() != 64 || !verifier.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("privacy key verifier must be a 32-byte hexadecimal digest")
        }
        self.with_immediate_transaction(|| {
            if let Some(existing) = self.privacy_key_verifier()? {
                if existing != verifier {
                    bail!("privacy pseudonym key does not match the initialized dataset")
                }
                return Ok(());
            }
            if self.privacy_identity_state_exists()? {
                bail!("privacy pseudonym state exists without a key verifier")
            }
            self.conn.execute(
                r#"
                INSERT INTO privacy_dataset_identity (singleton, key_verifier, created_at)
                VALUES (1, ?1, ?2)
                "#,
                params![verifier, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn resolve_privacy_pseudonym(&self, category: &str, value_hmac: &str) -> Result<u64> {
        self.with_immediate_transaction(|| {
            self.require_privacy_key_verifier()?;
            if let Some(alias) = self
                .conn
                .query_row(
                    "SELECT alias FROM privacy_pseudonyms WHERE category = ?1 AND value_hmac = ?2",
                    params![category, value_hmac],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Ok(alias);
            }
            let alias: u64 = self.conn.query_row(
                "SELECT COALESCE(MAX(alias), 0) + 1 FROM privacy_pseudonyms WHERE category = ?1",
                params![category],
                |row| row.get(0),
            )?;
            self.conn.execute(
                r#"
                INSERT INTO privacy_pseudonyms (category, value_hmac, alias, created_at)
                VALUES (?1, ?2, ?3, ?4)
                "#,
                params![category, value_hmac, alias, Utc::now().to_rfc3339()],
            )?;
            Ok(alias)
        })
    }

    pub fn write_filtered_conversation(
        &self,
        record: &FilteredConversationRecord,
        findings: &[PrivacyFindingRecord],
    ) -> Result<()> {
        self.with_immediate_transaction(|| {
            self.require_privacy_key_verifier()?;
            self.conn.execute(
                r#"
                INSERT INTO filtered_conversations
                  (conversation_id, dataset_key, input_fingerprint, policy_fingerprint,
                   payload, finding_count, succeeded_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(conversation_id) DO UPDATE SET
                  dataset_key = excluded.dataset_key,
                  input_fingerprint = excluded.input_fingerprint,
                  policy_fingerprint = excluded.policy_fingerprint,
                  payload = excluded.payload,
                  finding_count = excluded.finding_count,
                  succeeded_at = excluded.succeeded_at
                "#,
                params![
                    &record.conversation_id,
                    &record.dataset_key,
                    &record.input_fingerprint,
                    &record.policy_fingerprint,
                    &record.payload,
                    record.finding_count,
                    record.succeeded_at.to_rfc3339(),
                ],
            )?;
            self.conn.execute(
                "DELETE FROM privacy_findings WHERE conversation_id = ?1",
                params![&record.conversation_id],
            )?;
            let mut statement = self.conn.prepare(
                r#"
                INSERT INTO privacy_findings
                  (conversation_id, field_path, start_offset, end_offset, category,
                   detector, confidence, replacement)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )?;
            for finding in findings {
                statement.execute(params![
                    &record.conversation_id,
                    &finding.field_path,
                    finding.start,
                    finding.end,
                    &finding.category,
                    &finding.detector,
                    &finding.confidence,
                    &finding.replacement,
                ])?;
            }
            self.conn.execute(
                "DELETE FROM privacy_filter_failures WHERE conversation_id = ?1",
                params![&record.conversation_id],
            )?;
            Ok(())
        })
    }

    pub fn record_privacy_failure(&self, failure: &PrivacyFailureRecord) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO privacy_filter_failures
              (conversation_id, input_fingerprint, policy_fingerprint, failed_stage,
               error_code, attempted_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                &failure.conversation_id,
                &failure.input_fingerprint,
                &failure.policy_fingerprint,
                &failure.failed_stage,
                &failure.error_code,
                failure.attempted_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn record_privacy_failures(&self, failures: &[PrivacyFailureRecord]) -> Result<()> {
        self.with_immediate_transaction(|| {
            for failure in failures {
                self.record_privacy_failure(failure)?;
            }
            Ok(())
        })
    }

    pub fn filtered_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<FilteredConversationRecord>> {
        self.conn
            .query_row(
                r#"
                SELECT conversation_id, dataset_key, input_fingerprint, policy_fingerprint,
                       payload, finding_count, succeeded_at
                FROM filtered_conversations WHERE conversation_id = ?1
                "#,
                params![conversation_id],
                filtered_conversation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn filtered_conversation_metadata(
        &self,
        conversation_id: &str,
    ) -> Result<Option<FilteredConversationMetadata>> {
        self.conn
            .query_row(
                r#"
                SELECT conversation_id, dataset_key, input_fingerprint, policy_fingerprint,
                       finding_count, succeeded_at
                FROM filtered_conversations WHERE conversation_id = ?1
                "#,
                params![conversation_id],
                filtered_conversation_metadata_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn filtered_conversation_payload(
        &self,
        metadata: &FilteredConversationMetadata,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                r#"
                SELECT payload FROM filtered_conversations
                WHERE conversation_id = ?1
                  AND dataset_key = ?2
                  AND input_fingerprint = ?3
                  AND policy_fingerprint = ?4
                  AND succeeded_at = ?5
                "#,
                params![
                    &metadata.conversation_id,
                    &metadata.dataset_key,
                    &metadata.input_fingerprint,
                    &metadata.policy_fingerprint,
                    metadata.succeeded_at.to_rfc3339(),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn filtered_conversation_has_newer_failure(
        &self,
        conversation_id: &str,
        succeeded_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.conn
            .query_row(
                r#"
                SELECT EXISTS(
                  SELECT 1 FROM privacy_filter_failures
                  WHERE conversation_id = ?1 AND attempted_at > ?2
                )
                "#,
                params![conversation_id, succeeded_at.to_rfc3339()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn privacy_dataset_status(&self, policy_fingerprint: &str) -> Result<PrivacyDatasetStatus> {
        self.conn
            .query_row(
                r#"
            SELECT
              (SELECT COUNT(*) FROM archive_conversations),
              (SELECT COUNT(*) FROM filtered_conversations),
              (SELECT COUNT(*) FROM filtered_conversations WHERE policy_fingerprint = ?1),
              (SELECT COUNT(*) FROM filtered_conversations WHERE policy_fingerprint <> ?1),
              (SELECT COUNT(DISTINCT conversation_id) FROM privacy_filter_failures)
            "#,
                params![policy_fingerprint],
                |row| {
                    Ok(PrivacyDatasetStatus {
                        archived: row.get(0)?,
                        filtered: row.get(1)?,
                        current: row.get(2)?,
                        stale: row.get(3)?,
                        failed: row.get(4)?,
                    })
                },
            )
            .map_err(Into::into)
    }
}

fn filtered_conversation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<FilteredConversationRecord> {
    let succeeded_at: String = row.get(6)?;
    Ok(FilteredConversationRecord {
        conversation_id: row.get(0)?,
        dataset_key: row.get(1)?,
        input_fingerprint: row.get(2)?,
        policy_fingerprint: row.get(3)?,
        payload: row.get(4)?,
        finding_count: row.get(5)?,
        succeeded_at: parse_timestamp(6, &succeeded_at)?,
    })
}

fn filtered_conversation_metadata_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<FilteredConversationMetadata> {
    let succeeded_at: String = row.get(5)?;
    Ok(FilteredConversationMetadata {
        conversation_id: row.get(0)?,
        dataset_key: row.get(1)?,
        input_fingerprint: row.get(2)?,
        policy_fingerprint: row.get(3)?,
        finding_count: row.get(4)?,
        succeeded_at: parse_timestamp(5, &succeeded_at)?,
    })
}

fn parse_timestamp(column: usize, value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

#[cfg(test)]
mod tests {
    use statsai_core::{
        ArchiveCompleteness, ArchiveConversation, SourceId, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
    };

    use super::*;

    fn archive(conversation_id: &str) -> ArchiveConversation {
        ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: conversation_id.to_string(),
            provider: "codex".to_string(),
            source_id: SourceId("src_test".to_string()),
            native_conversation_id: format!("native_{conversation_id}"),
            title: None,
            project: None,
            started_at: None,
            updated_at: None,
            completeness: ArchiveCompleteness::Complete,
            missing_content_count: 0,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items: Vec::new(),
        }
    }

    #[test]
    fn filtered_dataset_round_trips_without_original_values() {
        let store = Store::in_memory().expect("store");
        store
            .ensure_privacy_key_verifier(&"ab".repeat(32))
            .expect("initialize verifier");
        store
            .upsert_archive_conversations(&[archive("conv_test")])
            .expect("archive");
        let alias = store
            .resolve_privacy_pseudonym("email", "hmac-only")
            .expect("alias");
        assert_eq!(alias, 1);
        assert_eq!(
            store
                .resolve_privacy_pseudonym("email", "hmac-only")
                .expect("stable alias"),
            alias
        );
        let record = FilteredConversationRecord {
            conversation_id: "conv_test".to_string(),
            dataset_key: "dataset_1".to_string(),
            input_fingerprint: "input".to_string(),
            policy_fingerprint: "policy".to_string(),
            payload: r#"{"text":"[EMAIL_000001]"}"#.to_string(),
            finding_count: 1,
            succeeded_at: Utc::now(),
        };
        store
            .write_filtered_conversation(
                &record,
                &[PrivacyFindingRecord {
                    field_path: "items/0/text".to_string(),
                    start: 0,
                    end: 16,
                    category: "email".to_string(),
                    detector: "openai_privacy_filter".to_string(),
                    confidence: None,
                    replacement: "[EMAIL_000001]".to_string(),
                }],
            )
            .expect("write filtered");

        assert_eq!(
            store
                .filtered_conversation("conv_test")
                .expect("read")
                .expect("present")
                .payload,
            record.payload
        );
        let metadata = store
            .filtered_conversation_metadata("conv_test")
            .expect("read metadata")
            .expect("metadata present");
        assert_eq!(metadata.dataset_key, record.dataset_key);
        assert_eq!(
            store
                .filtered_conversation_payload(&metadata)
                .expect("read payload"),
            Some(record.payload.clone())
        );
        let mut stale_metadata = metadata;
        stale_metadata.input_fingerprint = "changed".to_string();
        assert_eq!(
            store
                .filtered_conversation_payload(&stale_metadata)
                .expect("reject stale metadata"),
            None
        );
        assert_eq!(
            store
                .privacy_dataset_status("policy")
                .expect("status")
                .current,
            1
        );
    }

    #[test]
    fn privacy_key_verifier_is_initialized_before_pseudonym_state() {
        let store = Store::in_memory().expect("store");
        let verifier = "ab".repeat(32);

        assert!(store
            .resolve_privacy_pseudonym("email", "hmac-before-verifier")
            .is_err());
        assert!(store
            .write_filtered_conversation(
                &FilteredConversationRecord {
                    conversation_id: "conv-before-verifier".to_string(),
                    dataset_key: "dataset-before-verifier".to_string(),
                    input_fingerprint: "input".to_string(),
                    policy_fingerprint: "policy".to_string(),
                    payload: "{}".to_string(),
                    finding_count: 0,
                    succeeded_at: Utc::now(),
                },
                &[],
            )
            .is_err());

        store
            .ensure_privacy_key_verifier(&verifier)
            .expect("initialize verifier");
        store
            .ensure_privacy_key_verifier(&verifier)
            .expect("same verifier is idempotent");
        assert_eq!(
            store.privacy_key_verifier().expect("read verifier"),
            Some(verifier)
        );
        assert!(store.ensure_privacy_key_verifier(&"cd".repeat(32)).is_err());

        let legacy = Store::in_memory().expect("legacy store");
        legacy
            .conn
            .execute(
                r#"
                INSERT INTO privacy_pseudonyms (category, value_hmac, alias, created_at)
                VALUES ('email', 'legacy-hmac', 1, ?1)
                "#,
                params![Utc::now().to_rfc3339()],
            )
            .expect("write legacy mapping directly");
        assert!(legacy
            .ensure_privacy_key_verifier(&"ef".repeat(32))
            .is_err());
    }

    #[test]
    fn privacy_identity_initialization_is_serialized_across_connections() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store_path = directory.path().join("statsai.sqlite");
        let first_store = Store::open(&store_path).expect("first store");
        let second_store = Store::open(&store_path).expect("second store");
        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let (second_entered_tx, second_entered_rx) = std::sync::mpsc::channel();

        let first = std::thread::spawn(move || {
            first_store
                .with_privacy_identity_initialization(|_| {
                    first_entered_tx.send(()).expect("signal first lock");
                    release_first_rx.recv().expect("release first lock");
                    Ok(())
                })
                .expect("first initialization lock");
        });
        first_entered_rx.recv().expect("first lock acquired");

        let second = std::thread::spawn(move || {
            second_store
                .with_privacy_identity_initialization(|_| {
                    second_entered_tx.send(()).expect("signal second lock");
                    Ok(())
                })
                .expect("second initialization lock");
        });
        assert!(second_entered_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());

        release_first_tx.send(()).expect("release first lock");
        first.join().expect("first initialization thread");
        second_entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("second lock acquired after first committed");
        second.join().expect("second initialization thread");
    }

    #[test]
    fn failed_retry_preserves_success_and_marks_it_non_exportable() {
        let store = Store::in_memory().expect("store");
        store
            .ensure_privacy_key_verifier(&"ab".repeat(32))
            .expect("initialize verifier");
        store
            .upsert_archive_conversations(&[archive("conv_test")])
            .expect("archive");
        let succeeded_at = Utc::now() - chrono::Duration::seconds(1);
        let record = FilteredConversationRecord {
            conversation_id: "conv_test".to_string(),
            dataset_key: "dataset_1".to_string(),
            input_fingerprint: "input".to_string(),
            policy_fingerprint: "policy".to_string(),
            payload: "{}".to_string(),
            finding_count: 0,
            succeeded_at,
        };
        store
            .write_filtered_conversation(&record, &[])
            .expect("write success");
        store
            .record_privacy_failure(&PrivacyFailureRecord {
                conversation_id: "conv_test".to_string(),
                input_fingerprint: "new-input".to_string(),
                policy_fingerprint: "policy".to_string(),
                failed_stage: "post_scan".to_string(),
                error_code: "residual_finding".to_string(),
                attempted_at: Utc::now(),
            })
            .expect("record failure");

        assert!(store
            .filtered_conversation("conv_test")
            .expect("read success")
            .is_some());
        assert!(store
            .filtered_conversation_has_newer_failure("conv_test", succeeded_at)
            .expect("failure status"));
    }

    #[test]
    fn deleting_an_archive_conversation_removes_derived_privacy_rows() {
        let store = Store::in_memory().expect("store");
        store
            .ensure_privacy_key_verifier(&"ab".repeat(32))
            .expect("initialize verifier");
        store
            .upsert_archive_conversations(&[archive("conv_test")])
            .expect("archive");
        store
            .write_filtered_conversation(
                &FilteredConversationRecord {
                    conversation_id: "conv_test".to_string(),
                    dataset_key: "dataset_1".to_string(),
                    input_fingerprint: "input".to_string(),
                    policy_fingerprint: "policy".to_string(),
                    payload: "{}".to_string(),
                    finding_count: 0,
                    succeeded_at: Utc::now(),
                },
                &[],
            )
            .expect("write filtered");
        store
            .record_privacy_failure(&PrivacyFailureRecord {
                conversation_id: "conv_test".to_string(),
                input_fingerprint: "input-2".to_string(),
                policy_fingerprint: "policy".to_string(),
                failed_stage: "filter".to_string(),
                error_code: "detector_timeout".to_string(),
                attempted_at: Utc::now(),
            })
            .expect("write failure");

        store
            .conn
            .execute(
                "DELETE FROM archive_conversations WHERE conversation_id = ?1",
                params!["conv_test"],
            )
            .expect("delete archive");

        assert!(store
            .filtered_conversation("conv_test")
            .expect("read filtered")
            .is_none());
        assert_eq!(
            store
                .privacy_dataset_status("policy")
                .expect("privacy status")
                .failed,
            0
        );
    }

    #[test]
    fn read_snapshot_is_stable_across_a_concurrent_archive_update() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store_path = directory.path().join("statsai.sqlite");
        let reader = Store::open(&store_path).expect("open reader");
        let writer = Store::open(&store_path).expect("open writer");
        let mut original = archive("conv_test");
        original.title = Some("before".to_string());
        writer
            .upsert_archive_conversations(std::slice::from_ref(&original))
            .expect("write original archive");

        reader
            .with_read_snapshot(|snapshot| {
                let before = snapshot
                    .archive_conversation_for_privacy("conv_test")?
                    .expect("archive before concurrent update");
                let mut updated = original.clone();
                updated.title = Some("after".to_string());
                writer.upsert_archive_conversations(&[updated])?;
                let after = snapshot
                    .archive_conversation_for_privacy("conv_test")?
                    .expect("archive after concurrent update");

                assert_eq!(before.title.as_deref(), Some("before"));
                assert_eq!(after.title, before.title);
                Ok(())
            })
            .expect("read snapshot");

        assert_eq!(
            reader
                .archive_conversation_for_privacy("conv_test")
                .expect("read committed archive")
                .expect("committed archive exists")
                .title
                .as_deref(),
            Some("after")
        );
    }
}
