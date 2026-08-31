mod content;

pub(crate) use content::*;

use super::{ScanFileStateEntry, Store};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use statsai_core::{
    archive_artifact_metadata_signature, hash_text, ArchiveArtifactDependency, ArchiveCompleteness,
    ArchiveContentKind, ArchiveContentPart, ArchiveConversation, ArchiveItem, ArchiveItemKind,
    ArchiveRole, CoverageStatus, ModelInfo, ProjectInfo, QuotaObservationRecordV1, SourceId,
    TraceEdit, UsageCounts, ARCHIVE_CONVERSATION_SCHEMA_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// Bumped whenever reconstruction changes what a re-import would produce, so
// already-imported files are read again instead of keeping stale results. v4
// backfilled local code-change fingerprints from original provider records; v6
// reclassified read-only shell calls and stopped treating echoed file content
// as a failed mutation; v7 reads whole-file creation from the tool result, so
// files a `Write` created become counted additions with matchable fingerprints
// instead of unclassified lines; v8 binds a trace edit to the conversation its
// file is written as, so edits a resumed session recorded against its parent
// stop being attributed to that parent; v9 backfills quota observations from
// immutable Codex archives.
const ARCHIVE_IMPORT_REVISION: &str = "archive.v9";
const UNSCOPED_MISSING_CONTENT_SCOPE: &str = "unscoped";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveWriteResult {
    pub conversations: u64,
    pub items: u64,
    pub content_parts: u64,
    pub binary_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveConversationSummary {
    pub conversation_id: String,
    pub provider: String,
    pub source_id: String,
    pub native_conversation_id: String,
    pub title: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub completeness: String,
    pub missing_content_count: u64,
    pub item_count: u64,
    pub content_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveSearchHit {
    pub conversation_id: String,
    pub item_id: String,
    pub provider: String,
    pub title: Option<String>,
    pub role: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ArchiveStats {
    pub conversations: u64,
    pub items: u64,
    pub text_parts: u64,
    pub binary_parts: u64,
    pub text_bytes: u64,
    pub binary_bytes: u64,
    pub missing_content: u64,
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn store_archive_scan_with_code_changes(
        &self,
        source_id: &SourceId,
        conversations: &[ArchiveConversation],
        imported_entries: &[ScanFileStateEntry],
        artifact_dependencies: &[ArchiveArtifactDependency],
        trace_edits: &[TraceEdit],
        trace_coverage: CoverageStatus,
        quota_observations: &[QuotaObservationRecordV1],
    ) -> Result<ArchiveWriteResult> {
        self.with_immediate_transaction(|| {
            let result = self.upsert_archive_conversations(conversations)?;
            self.replace_archive_trace_edits_inner(
                source_id,
                imported_entries,
                trace_edits,
                trace_coverage,
            )?;
            self.record_archive_import_entries(source_id, imported_entries)?;
            self.replace_archive_artifact_dependencies(
                source_id,
                imported_entries,
                artifact_dependencies,
            )?;
            let source_file_path_hashes = imported_entries
                .iter()
                .map(|entry| hash_text(&entry.cache_key))
                .collect::<Vec<_>>();
            self.replace_quota_observations_for_source_files_inner(
                source_id,
                &source_file_path_hashes,
                quota_observations,
            )?;
            Ok(result)
        })
    }

    pub fn pending_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<Vec<ScanFileStateEntry>> {
        let mut statement = self.conn.prepare(
            "SELECT cache_signature FROM archive_import_state WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        let mut dependency_statement = self.conn.prepare(
            r#"
            SELECT artifact_path, metadata_signature
            FROM archive_artifact_dependencies
            WHERE source_id = ?1 AND cache_key = ?2
            "#,
        )?;
        let mut pending = Vec::new();
        for entry in entries {
            let existing = statement
                .query_row(params![&source_id.0, &entry.cache_key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?;
            let expected = archive_import_signature(&entry.cache_signature);
            if existing.as_deref() != Some(expected.as_str()) {
                pending.push(entry.clone());
                continue;
            }
            let dependencies = dependency_statement
                .query_map(params![&source_id.0, &entry.cache_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            for dependency in dependencies {
                let (path, stored_signature) = dependency?;
                if archive_artifact_metadata_signature(Path::new(&path)) != stored_signature {
                    pending.push(entry.clone());
                    break;
                }
            }
        }
        Ok(pending)
    }

    /// Number of archive files already imported from a source.
    pub fn archive_import_entry_count(&self, source_id: &SourceId) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM archive_import_state WHERE source_id = ?1",
            [&source_id.0],
            |row| row.get::<_, u64>(0),
        )?)
    }

    pub fn reconcile_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<u64> {
        let current_cache_keys = entries
            .iter()
            .map(|entry| entry.cache_key.as_str())
            .collect::<HashSet<_>>();
        self.with_immediate_transaction(|| {
            let mut statement = self.conn.prepare(
                "SELECT cache_key FROM archive_import_state WHERE source_id = ?1",
            )?;
            let stored_cache_keys = statement
                .query_map([&source_id.0], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let removed_cache_keys = stored_cache_keys
                .into_iter()
                .filter(|cache_key| !current_cache_keys.contains(cache_key.as_str()))
                .collect::<Vec<_>>();
            for cache_key in &removed_cache_keys {
                self.delete_archive_trace_entry_inner(source_id, cache_key)?;
                self.conn.execute(
                    "DELETE FROM archive_artifact_dependencies WHERE source_id = ?1 AND cache_key = ?2",
                    params![&source_id.0, cache_key],
                )?;
                self.conn.execute(
                    "DELETE FROM archive_import_state WHERE source_id = ?1 AND cache_key = ?2",
                    params![&source_id.0, cache_key],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_window_observations WHERE observation_id IN (SELECT observation_id FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2)",
                    params![&source_id.0, hash_text(cache_key)],
                )?;
                self.conn.execute(
                    "DELETE FROM quota_observations WHERE source_id = ?1 AND source_file_path_hash = ?2",
                    params![&source_id.0, hash_text(cache_key)],
                )?;
            }
            self.conn.execute(
                "DELETE FROM quota_payloads WHERE payload_hash NOT IN (SELECT payload_hash FROM quota_observations)",
                [],
            )?;
            Ok(removed_cache_keys.len() as u64)
        })
    }

    pub fn upsert_archive_conversations(
        &self,
        conversations: &[ArchiveConversation],
    ) -> Result<ArchiveWriteResult> {
        self.with_immediate_transaction(|| self.upsert_archive_conversations_inner(conversations))
    }

    fn upsert_archive_conversations_inner(
        &self,
        conversations: &[ArchiveConversation],
    ) -> Result<ArchiveWriteResult> {
        let imported_at = Utc::now().to_rfc3339();
        let mut result = ArchiveWriteResult::default();
        for conversation in conversations {
            let incoming_external_missing = conversation
                .items
                .iter()
                .flat_map(|item| &item.parts)
                .filter(|part| part.external_uri.is_some())
                .count() as u64;
            let incoming_non_materialized_missing = conversation
                .missing_content_count
                .saturating_sub(incoming_external_missing);
            let project_json = conversation
                .project
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            self.conn.execute(
                r#"
                INSERT INTO archive_conversations
                  (conversation_id, provider, source_id, native_conversation_id, title,
                   project_json, started_at, updated_at, completeness,
                   missing_content_count, imported_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(conversation_id) DO UPDATE SET
                  source_id = excluded.source_id,
                  title = COALESCE(excluded.title, archive_conversations.title),
                  project_json = COALESCE(excluded.project_json, archive_conversations.project_json),
                  started_at = CASE
                    WHEN archive_conversations.started_at IS NULL THEN excluded.started_at
                    WHEN excluded.started_at IS NULL THEN archive_conversations.started_at
                    WHEN excluded.started_at < archive_conversations.started_at THEN excluded.started_at
                    ELSE archive_conversations.started_at
                  END,
                  updated_at = CASE
                    WHEN archive_conversations.updated_at IS NULL THEN excluded.updated_at
                    WHEN excluded.updated_at IS NULL THEN archive_conversations.updated_at
                    WHEN excluded.updated_at > archive_conversations.updated_at THEN excluded.updated_at
                    ELSE archive_conversations.updated_at
                  END,
                  completeness = excluded.completeness,
                  missing_content_count = excluded.missing_content_count,
                  imported_at = excluded.imported_at
                "#,
                params![
                    &conversation.conversation_id,
                    &conversation.provider,
                    &conversation.source_id.0,
                    &conversation.native_conversation_id,
                    &conversation.title,
                    project_json,
                    conversation.started_at.map(|value| value.to_rfc3339()),
                    conversation.updated_at.map(|value| value.to_rfc3339()),
                    conversation.completeness.as_str(),
                    conversation.missing_content_count,
                    &imported_at,
                ],
            )?;
            result.conversations += 1;

            // Reconciliation, item writes, and content writes each run as a
            // handful of statements for the whole conversation rather than a
            // few per item. Once a transaction has touched the full-text index,
            // SQLite flushes that index at every statement boundary, so the
            // cost of an import followed the number of statements it issued far
            // more closely than the amount of content that had actually
            // changed.
            self.stage_conversation_records(conversation)?;
            self.delete_replaced_source_records(conversation)?;
            let mut retained_parts = self.staged_content_retention()?;
            self.insert_archive_items(conversation, &mut result)?;
            self.write_archive_content_parts(conversation, &mut retained_parts, &mut result)?;

            for superseded_id in &conversation.superseded_conversation_ids {
                if superseded_id == &conversation.conversation_id {
                    continue;
                }
                self.conn.execute(
                    r#"
                    DELETE FROM archive_missing_content_state
                    WHERE conversation_id = ?1
                      AND EXISTS (
                        SELECT 1
                        FROM archive_conversations
                        WHERE conversation_id = ?1
                          AND provider = ?2
                          AND source_id = ?3
                      )
                      AND NOT EXISTS (
                        SELECT 1 FROM archive_items WHERE conversation_id = ?1
                      )
                    "#,
                    params![
                        superseded_id,
                        &conversation.provider,
                        &conversation.source_id.0,
                    ],
                )?;
                self.conn.execute(
                    r#"
                    DELETE FROM archive_conversations
                    WHERE conversation_id = ?1
                      AND provider = ?2
                      AND source_id = ?3
                      AND NOT EXISTS (
                        SELECT 1 FROM archive_items WHERE conversation_id = ?1
                      )
                    "#,
                    params![
                        superseded_id,
                        &conversation.provider,
                        &conversation.source_id.0,
                    ],
                )?;
            }

            let missing_content_scope_id = conversation
                .missing_content_scope_id
                .as_deref()
                .unwrap_or(UNSCOPED_MISSING_CONTENT_SCOPE);
            if incoming_non_materialized_missing == 0 {
                self.conn.execute(
                    r#"
                    DELETE FROM archive_missing_content_state
                    WHERE conversation_id = ?1 AND scope_id = ?2
                    "#,
                    params![&conversation.conversation_id, missing_content_scope_id],
                )?;
            } else {
                self.conn.execute(
                    r#"
                    INSERT INTO archive_missing_content_state
                      (conversation_id, scope_id, missing_content_count, updated_at)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(conversation_id, scope_id) DO UPDATE SET
                      missing_content_count = excluded.missing_content_count,
                      updated_at = excluded.updated_at
                    "#,
                    params![
                        &conversation.conversation_id,
                        missing_content_scope_id,
                        incoming_non_materialized_missing,
                        &imported_at,
                    ],
                )?;
            }

            let (stored_item_count, stored_external_missing, non_materialized_missing) =
                self.conn.query_row(
                    r#"
                SELECT COUNT(DISTINCT i.item_id),
                       COUNT(CASE WHEN p.external_uri IS NOT NULL THEN 1 END),
                       (SELECT COALESCE(SUM(missing_content_count), 0)
                        FROM archive_missing_content_state
                        WHERE conversation_id = ?1)
                FROM archive_items i
                LEFT JOIN archive_content_parts p ON p.item_id = i.item_id
                WHERE i.conversation_id = ?1
                "#,
                    params![&conversation.conversation_id],
                    |row| {
                        Ok((
                            row.get::<_, u64>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, u64>(2)?,
                        ))
                    },
                )?;
            let missing_content_count = stored_external_missing + non_materialized_missing;
            let completeness = if missing_content_count > 0 {
                ArchiveCompleteness::Partial
            } else if stored_item_count > 0 {
                ArchiveCompleteness::Complete
            } else {
                ArchiveCompleteness::MetadataOnly
            };
            self.conn.execute(
                r#"
                UPDATE archive_conversations
                SET completeness = ?2, missing_content_count = ?3
                WHERE conversation_id = ?1
                "#,
                params![
                    &conversation.conversation_id,
                    completeness.as_str(),
                    missing_content_count,
                ],
            )?;
        }
        Ok(result)
    }

    /// Stages the source records and item identifiers this conversation brings.
    ///
    /// Reconciliation needs to ask "which stored items does this import
    /// replace" for every incoming record at once. Staging the batch turns two
    /// statements per item into two statements per conversation.
    ///
    /// A discarded record stages an empty item identifier, which no real item
    /// can hold, so every stored copy of that record is replaced by nothing.
    /// An item without a source record stages a NULL, which joins to nothing
    /// and so replaces nothing, while still carrying its identifier for the
    /// content lookup.
    fn stage_conversation_records(&self, conversation: &ArchiveConversation) -> Result<()> {
        self.conn.execute("DELETE FROM incoming_records", [])?;
        const DISCARDED: &str = "";
        let staged = conversation
            .discarded_source_record_ids
            .iter()
            .map(|source_record_id| (Some(source_record_id), DISCARDED))
            .chain(
                conversation
                    .items
                    .iter()
                    .map(|item| (item.source_record_id.as_ref(), item.item_id.as_str())),
            )
            .collect::<Vec<_>>();
        for chunk in staged.chunks(ARCHIVE_WRITE_BATCH_ROWS) {
            let mut sql =
                String::from("INSERT INTO incoming_records (source_record_id, item_id) VALUES ");
            append_row_placeholders(&mut sql, chunk.len(), 2);
            let mut bindings: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 2);
            for (source_record_id, item_id) in chunk {
                bindings.push(source_record_id);
                bindings.push(item_id);
            }
            self.conn
                .prepare_cached(&sql)?
                .execute(bindings.as_slice())?;
        }
        Ok(())
    }

    /// Removes the stored items that the staged records supersede.
    ///
    /// The joins are written as `CROSS JOIN` to pin the order: the staged
    /// records are the small side and must drive, so each one probes the stored
    /// items by source record. Left to its own estimates the planner walks
    /// every item recorded for the source instead, which turns importing a
    /// source into quadratic work.
    fn delete_replaced_source_records(&self, conversation: &ArchiveConversation) -> Result<()> {
        for table in ["archive_content_parts", "archive_items"] {
            self.conn
                .prepare_cached(&format!(
                    r#"
                DELETE FROM {table}
                WHERE item_id IN (
                  SELECT stored.item_id
                  FROM incoming_records incoming
                  CROSS JOIN archive_items stored
                    ON stored.source_record_id = incoming.source_record_id
                  CROSS JOIN archive_conversations conversation
                    ON conversation.conversation_id = stored.conversation_id
                  WHERE stored.item_id <> incoming.item_id
                    AND conversation.provider = ?1
                    AND conversation.source_id = ?2
                )
                "#
                ))?
                .execute(params![&conversation.provider, &conversation.source_id.0])?;
        }
        Ok(())
    }

    /// Content already stored for the staged items.
    fn staged_content_retention(&self) -> Result<HashMap<String, RetainedContentPart>> {
        let mut statement = self.conn.prepare_cached(
            r#"
            SELECT part.content_id,
                   part.text_content IS NOT NULL OR part.binary_content IS NOT NULL,
                   part.external_uri IS NOT NULL,
                   COALESCE(length(CAST(part.text_content AS BLOB)), 0)
                     + COALESCE(length(part.binary_content), 0),
                   part.truncated,
                   part.content_hash,
                   part.item_id,
                   part.kind,
                   part.mime_type,
                   part.name,
                   part.original_bytes,
                   part.ordinal,
                   part.external_uri,
                   part.text_content IS NOT NULL,
                   part.binary_content IS NOT NULL
            FROM incoming_records incoming
            CROSS JOIN archive_content_parts part ON part.item_id = incoming.item_id
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                RetainedContentPart {
                    quality: ContentRetentionQuality {
                        materialized: row.get(1)?,
                        external: row.get(2)?,
                        stored_bytes: row.get(3)?,
                        truncated: row.get(4)?,
                    },
                    content_hash: row.get(5)?,
                    item_id: row.get(6)?,
                    metadata: RetainedContentMetadata {
                        kind: row.get(7)?,
                        mime_type: row.get(8)?,
                        name: row.get(9)?,
                        original_bytes: row.get(10)?,
                        ordinal: row.get(11)?,
                        external_uri: row.get(12)?,
                        stored_as_text: row.get(13)?,
                        stored_as_binary: row.get(14)?,
                    },
                },
            ))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    fn insert_archive_items(
        &self,
        conversation: &ArchiveConversation,
        result: &mut ArchiveWriteResult,
    ) -> Result<()> {
        let encoded = conversation
            .items
            .iter()
            .map(|item| {
                Ok(EncodedItem {
                    kind: item.kind.as_str(),
                    role: item.role.map(ArchiveRole::as_str),
                    created_at: item.created_at.map(|value| value.to_rfc3339()),
                    model_json: item.model.as_ref().map(serde_json::to_string).transpose()?,
                    usage_json: item.usage.as_ref().map(serde_json::to_string).transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        for (items, encoded) in conversation
            .items
            .chunks(ARCHIVE_WRITE_BATCH_ROWS)
            .zip(encoded.chunks(ARCHIVE_WRITE_BATCH_ROWS))
        {
            let mut sql = String::from(
                "INSERT INTO archive_items
                   (item_id, conversation_id, native_item_id, source_record_id, ordinal,
                    kind, role, created_at, model_json, tool_name, tool_call_id, status,
                    usage_json) VALUES ",
            );
            append_row_placeholders(&mut sql, items.len(), 13);
            sql.push_str(
                " ON CONFLICT(item_id) DO UPDATE SET
                    conversation_id = excluded.conversation_id,
                    native_item_id = COALESCE(excluded.native_item_id, archive_items.native_item_id),
                    source_record_id = COALESCE(excluded.source_record_id, archive_items.source_record_id),
                    ordinal = excluded.ordinal,
                    kind = excluded.kind,
                    role = COALESCE(excluded.role, archive_items.role),
                    created_at = COALESCE(excluded.created_at, archive_items.created_at),
                    model_json = COALESCE(excluded.model_json, archive_items.model_json),
                    tool_name = COALESCE(excluded.tool_name, archive_items.tool_name),
                    tool_call_id = COALESCE(excluded.tool_call_id, archive_items.tool_call_id),
                    status = COALESCE(excluded.status, archive_items.status),
                    usage_json = COALESCE(excluded.usage_json, archive_items.usage_json)",
            );
            let mut bindings: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(items.len() * 13);
            for (item, encoded) in items.iter().zip(encoded) {
                bindings.push(&item.item_id);
                bindings.push(&conversation.conversation_id);
                bindings.push(&item.native_item_id);
                bindings.push(&item.source_record_id);
                bindings.push(&item.ordinal);
                bindings.push(&encoded.kind);
                bindings.push(&encoded.role);
                bindings.push(&encoded.created_at);
                bindings.push(&encoded.model_json);
                bindings.push(&item.tool_name);
                bindings.push(&item.tool_call_id);
                bindings.push(&item.status);
                bindings.push(&encoded.usage_json);
            }
            self.conn
                .prepare_cached(&sql)?
                .execute(bindings.as_slice())?;
        }
        result.items += conversation.items.len() as u64;
        Ok(())
    }

    /// Writes the content this import adds or improves, and drops what an
    /// authoritative item no longer lists.
    fn write_archive_content_parts(
        &self,
        conversation: &ArchiveConversation,
        retained_parts: &mut HashMap<String, RetainedContentPart>,
        result: &mut ArchiveWriteResult,
    ) -> Result<()> {
        let mut replaced = Vec::new();
        let mut obsolete = Vec::new();
        let mut pending = Vec::new();
        let mut pending_bytes = 0usize;
        for item in &conversation.items {
            let incoming_part_ids = item
                .parts
                .iter()
                .map(|part| part.content_id.as_str())
                .collect::<HashSet<_>>();
            for part in &item.parts {
                // Decoding is deferred until the row is known to need writing:
                // on a re-import most parts are already stored byte for byte,
                // and decoding them only to discard the result is the most
                // expensive thing this loop can do to an archive of images.
                let incoming_quality = ContentRetentionQuality {
                    materialized: part.text.is_some() || part.data_base64.is_some(),
                    external: part.external_uri.is_some(),
                    stored_bytes: part.text.as_ref().map_or(0, |text| text.len() as u64)
                        + part.data_base64.as_deref().map_or(0, base64_decoded_len),
                    truncated: part.truncated,
                };
                match retained_parts.get(&part.content_id) {
                    // The stored row already is the incoming row, down to the
                    // metadata the content hash does not cover. Rewriting it
                    // would delete and reinsert identical bytes and drag the
                    // full-text index through both.
                    Some(existing)
                        if existing.already_stores(part, incoming_quality, &item.item_id) =>
                    {
                        continue
                    }
                    Some(existing)
                        if !incoming_content_should_replace(
                            existing,
                            &part.content_hash,
                            incoming_quality,
                        ) =>
                    {
                        continue
                    }
                    _ => {}
                }
                let binary_content = part
                    .data_base64
                    .as_deref()
                    .map(|encoded| {
                        BASE64
                            .decode(encoded)
                            .with_context(|| format!("decode archive content {}", part.content_id))
                    })
                    .transpose()?;
                if retained_parts.contains_key(&part.content_id) {
                    replaced.push(part.content_id.clone());
                }
                result.binary_bytes += binary_content
                    .as_ref()
                    .map_or(0, |bytes| bytes.len() as u64);
                result.content_parts += 1;
                retained_parts.insert(
                    part.content_id.clone(),
                    RetainedContentPart {
                        item_id: item.item_id.clone(),
                        content_hash: part.content_hash.clone(),
                        quality: incoming_quality,
                        metadata: RetainedContentMetadata {
                            ordinal: part.ordinal,
                            kind: part.kind.as_str().to_string(),
                            mime_type: part.mime_type.clone(),
                            name: part.name.clone(),
                            external_uri: part.external_uri.clone(),
                            original_bytes: part.original_bytes,
                            stored_as_text: part.text.is_some(),
                            stored_as_binary: part.data_base64.is_some(),
                        },
                    },
                );
                pending_bytes += binary_content.as_ref().map_or(0, Vec::len);
                pending.push(PendingContentPart {
                    part,
                    item_id: item.item_id.as_str(),
                    kind: part.kind.as_str(),
                    binary_content,
                });
                // Decoded content is written out as it accumulates rather than
                // once the conversation has been walked: a conversation can
                // reference artifacts far larger than the transcript naming
                // them, and holding every decoded copy at once is what makes
                // peak memory a property of the conversation instead of the
                // batch.
                if pending.len() >= ARCHIVE_WRITE_BATCH_ROWS
                    || pending_bytes >= ARCHIVE_WRITE_BATCH_BYTES
                {
                    self.delete_content_parts(&replaced)?;
                    self.insert_content_parts(&pending)?;
                    replaced.clear();
                    pending.clear();
                    pending_bytes = 0;
                }
            }
            if item.parts_authoritative {
                obsolete.extend(
                    retained_parts
                        .iter()
                        .filter(|(content_id, retained)| {
                            retained.item_id == item.item_id
                                && !incoming_part_ids.contains(content_id.as_str())
                        })
                        .map(|(content_id, _)| content_id.clone()),
                );
            }
        }
        // Each batch deletes the rows it replaces before writing them, so a
        // replacement never races the insert that supersedes it. Content this
        // import did not list is only dropped once every row that survives is
        // in, which keeps an interrupted transaction from being the one that
        // removed content without writing its replacement.
        self.delete_content_parts(&replaced)?;
        self.insert_content_parts(&pending)?;
        self.delete_content_parts(&obsolete)?;
        for content_id in &obsolete {
            retained_parts.remove(content_id);
        }
        Ok(())
    }

    fn delete_content_parts(&self, content_ids: &[String]) -> Result<()> {
        for chunk in content_ids.chunks(ARCHIVE_DELETE_BATCH_ROWS) {
            let mut sql = String::from("DELETE FROM archive_content_parts WHERE content_id IN (");
            append_row_placeholders(&mut sql, chunk.len(), 1);
            sql.push(')');
            let bindings = chunk
                .iter()
                .map(|content_id| content_id as &dyn rusqlite::ToSql)
                .collect::<Vec<_>>();
            self.conn
                .prepare_cached(&sql)?
                .execute(bindings.as_slice())?;
        }
        Ok(())
    }

    fn insert_content_parts(&self, pending: &[PendingContentPart<'_>]) -> Result<()> {
        let mut start = 0;
        while start < pending.len() {
            // Batches are bounded by content size as well as row count so that
            // a run of large artifacts cannot build an unbounded parameter list.
            let mut end = start;
            let mut batch_bytes = 0usize;
            while end < pending.len() && end - start < ARCHIVE_WRITE_BATCH_ROWS {
                batch_bytes += pending[end].stored_bytes();
                end += 1;
                if batch_bytes >= ARCHIVE_WRITE_BATCH_BYTES {
                    break;
                }
            }
            let chunk = &pending[start..end];
            let mut sql = String::from(
                "INSERT INTO archive_content_parts
                   (content_id, item_id, ordinal, kind, mime_type, name, text_content,
                    binary_content, external_uri, content_hash, original_bytes, truncated)
                 VALUES ",
            );
            append_row_placeholders(&mut sql, chunk.len(), 12);
            let mut bindings: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 12);
            for entry in chunk {
                bindings.push(&entry.part.content_id);
                bindings.push(&entry.item_id);
                bindings.push(&entry.part.ordinal);
                bindings.push(&entry.kind);
                bindings.push(&entry.part.mime_type);
                bindings.push(&entry.part.name);
                bindings.push(&entry.part.text);
                bindings.push(&entry.binary_content);
                bindings.push(&entry.part.external_uri);
                bindings.push(&entry.part.content_hash);
                bindings.push(&entry.part.original_bytes);
                bindings.push(&entry.part.truncated);
            }
            self.conn
                .prepare_cached(&sql)?
                .execute(bindings.as_slice())?;
            start = end;
        }
        Ok(())
    }

    pub fn list_archive_conversations(
        &self,
        provider: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArchiveConversationSummary>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT c.conversation_id, c.provider, c.source_id, c.native_conversation_id,
                   c.title, c.started_at, c.updated_at, c.completeness,
                   c.missing_content_count, COUNT(DISTINCT i.item_id),
                   COALESCE(SUM(
                     COALESCE(length(CAST(p.text_content AS BLOB)), 0)
                     + COALESCE(length(p.binary_content), 0)
                   ), 0)
            FROM archive_conversations c
            LEFT JOIN archive_items i ON i.conversation_id = c.conversation_id
            LEFT JOIN archive_content_parts p ON p.item_id = i.item_id
            WHERE (?1 IS NULL OR c.provider = ?1)
            GROUP BY c.conversation_id
            ORDER BY COALESCE(c.updated_at, c.started_at) DESC, c.conversation_id
            LIMIT ?2
            "#,
        )?;
        let sqlite_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![provider, sqlite_limit], |row| {
            Ok(ArchiveConversationSummary {
                conversation_id: row.get(0)?,
                provider: row.get(1)?,
                source_id: row.get(2)?,
                native_conversation_id: row.get(3)?,
                title: row.get(4)?,
                started_at: parse_optional_timestamp(row.get(5)?),
                updated_at: parse_optional_timestamp(row.get(6)?),
                completeness: row.get(7)?,
                missing_content_count: row.get(8)?,
                item_count: row.get(9)?,
                content_bytes: row.get(10)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn archive_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ArchiveConversation>> {
        self.archive_conversation_with_binary(conversation_id, true)
    }

    pub fn archive_conversation_for_privacy(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ArchiveConversation>> {
        self.archive_conversation_with_binary(conversation_id, false)
    }

    fn archive_conversation_with_binary(
        &self,
        conversation_id: &str,
        include_binary: bool,
    ) -> Result<Option<ArchiveConversation>> {
        let conversation = self
            .conn
            .query_row(
                r#"
                SELECT provider, source_id, native_conversation_id, title, project_json,
                       started_at, updated_at, completeness, missing_content_count
                FROM archive_conversations WHERE conversation_id = ?1
                "#,
                params![conversation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, u64>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            provider,
            source_id,
            native_id,
            title,
            project_json,
            started_at,
            updated_at,
            completeness,
            missing_content_count,
        )) = conversation
        else {
            return Ok(None);
        };
        let project = project_json
            .as_deref()
            .map(serde_json::from_str::<ProjectInfo>)
            .transpose()?;
        let mut items = self.archive_items(conversation_id)?;
        for item in &mut items {
            item.parts = self.archive_content_parts(&item.item_id, include_binary)?;
        }
        Ok(Some(ArchiveConversation {
            schema_version: ARCHIVE_CONVERSATION_SCHEMA_VERSION.to_string(),
            conversation_id: conversation_id.to_string(),
            provider,
            source_id: SourceId(source_id),
            native_conversation_id: native_id,
            title,
            project,
            started_at: parse_optional_timestamp(started_at),
            updated_at: parse_optional_timestamp(updated_at),
            completeness: parse_completeness(&completeness),
            missing_content_count,
            missing_content_scope_id: None,
            discarded_source_record_ids: Vec::new(),
            superseded_conversation_ids: Vec::new(),
            items,
        }))
    }

    pub fn search_archive(&self, query: &str, limit: usize) -> Result<Vec<ArchiveSearchHit>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT c.conversation_id, i.item_id, c.provider, c.title, i.role, i.created_at,
                   p.text_content
            FROM archive_content_fts f
            JOIN archive_content_parts p ON p.rowid = f.rowid
            JOIN archive_items i ON i.item_id = p.item_id
            JOIN archive_conversations c ON c.conversation_id = i.conversation_id
            WHERE archive_content_fts MATCH ?1
            ORDER BY rank, COALESCE(i.created_at, c.started_at) DESC
            LIMIT ?2
            "#,
        )?;
        let rows = statement.query_map(params![query, limit as u64], |row| {
            Ok(ArchiveSearchHit {
                conversation_id: row.get(0)?,
                item_id: row.get(1)?,
                provider: row.get(2)?,
                title: row.get(3)?,
                role: row.get(4)?,
                created_at: parse_optional_timestamp(row.get(5)?),
                text: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn archive_stats(&self) -> Result<ArchiveStats> {
        self.conn
            .query_row(
                r#"
                SELECT
                  (SELECT COUNT(*) FROM archive_conversations),
                  (SELECT COUNT(*) FROM archive_items),
                  SUM(CASE WHEN text_content IS NOT NULL THEN 1 ELSE 0 END),
                  SUM(CASE WHEN binary_content IS NOT NULL THEN 1 ELSE 0 END),
                  COALESCE(SUM(length(CAST(text_content AS BLOB))), 0),
                  COALESCE(SUM(length(binary_content)), 0),
                  (SELECT COALESCE(SUM(missing_content_count), 0) FROM archive_conversations)
                FROM archive_content_parts
                "#,
                [],
                |row| {
                    Ok(ArchiveStats {
                        conversations: row.get(0)?,
                        items: row.get(1)?,
                        text_parts: row.get::<_, Option<u64>>(2)?.unwrap_or(0),
                        binary_parts: row.get::<_, Option<u64>>(3)?.unwrap_or(0),
                        text_bytes: row.get(4)?,
                        binary_bytes: row.get(5)?,
                        missing_content: row.get(6)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    fn record_archive_import_entries(
        &self,
        source_id: &SourceId,
        entries: &[ScanFileStateEntry],
    ) -> Result<()> {
        let collected_at = Utc::now().to_rfc3339();
        let mut statement = self.conn.prepare(
            r#"
            INSERT INTO archive_import_state (source_id, cache_key, cache_signature, collected_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(source_id, cache_key) DO UPDATE SET
              cache_signature = excluded.cache_signature,
              collected_at = excluded.collected_at
            "#,
        )?;
        for entry in entries {
            statement.execute(params![
                &source_id.0,
                &entry.cache_key,
                archive_import_signature(&entry.cache_signature),
                &collected_at,
            ])?;
        }
        Ok(())
    }

    fn replace_archive_artifact_dependencies(
        &self,
        source_id: &SourceId,
        imported_entries: &[ScanFileStateEntry],
        dependencies: &[ArchiveArtifactDependency],
    ) -> Result<()> {
        let imported_cache_keys = imported_entries
            .iter()
            .map(|entry| entry.cache_key.as_str())
            .collect::<HashSet<_>>();
        let mut delete_statement = self.conn.prepare(
            "DELETE FROM archive_artifact_dependencies WHERE source_id = ?1 AND cache_key = ?2",
        )?;
        for entry in imported_entries {
            delete_statement.execute(params![&source_id.0, &entry.cache_key])?;
        }

        let mut insert_statement = self.conn.prepare(
            r#"
            INSERT INTO archive_artifact_dependencies
              (source_id, cache_key, artifact_path, metadata_signature)
            VALUES (?1, ?2, ?3, ?4)
            "#,
        )?;
        for dependency in dependencies {
            ensure!(
                imported_cache_keys.contains(dependency.cache_key.as_str()),
                "archive artifact dependency does not match an imported cache entry: {}",
                dependency.cache_key
            );
            insert_statement.execute(params![
                &source_id.0,
                &dependency.cache_key,
                dependency.path.to_string_lossy().as_ref(),
                &dependency.metadata_signature,
            ])?;
        }
        Ok(())
    }

    fn archive_items(&self, conversation_id: &str) -> Result<Vec<ArchiveItem>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT item_id, native_item_id, source_record_id, ordinal, kind, role,
                   created_at, model_json, tool_name, tool_call_id, status, usage_json
            FROM archive_items
            WHERE conversation_id = ?1
            ORDER BY ordinal, item_id
            "#,
        )?;
        let rows = statement.query_map(params![conversation_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (
                item_id,
                native_item_id,
                source_record_id,
                ordinal,
                kind,
                role,
                created_at,
                model_json,
                tool_name,
                tool_call_id,
                status,
                usage_json,
            ) = row?;
            items.push(ArchiveItem {
                item_id,
                native_item_id,
                source_record_id,
                ordinal,
                kind: parse_item_kind(&kind),
                role: role.as_deref().map(parse_role),
                created_at: parse_optional_timestamp(created_at),
                model: model_json
                    .as_deref()
                    .map(serde_json::from_str::<ModelInfo>)
                    .transpose()?,
                tool_name,
                tool_call_id,
                status,
                usage: usage_json
                    .as_deref()
                    .map(serde_json::from_str::<UsageCounts>)
                    .transpose()?,
                parts_authoritative: true,
                parts: Vec::new(),
            });
        }
        Ok(items)
    }

    fn archive_content_parts(
        &self,
        item_id: &str,
        include_binary: bool,
    ) -> Result<Vec<ArchiveContentPart>> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT content_id, ordinal, kind, mime_type, name, text_content,
                   CASE WHEN ?2 THEN binary_content ELSE NULL END,
                   external_uri, content_hash, original_bytes, truncated
            FROM archive_content_parts
            WHERE item_id = ?1
            ORDER BY ordinal, content_id
            "#,
        )?;
        let rows = statement.query_map(params![item_id, include_binary], |row| {
            let binary: Option<Vec<u8>> = row.get(6)?;
            Ok(ArchiveContentPart {
                content_id: row.get(0)?,
                ordinal: row.get(1)?,
                kind: parse_content_kind(&row.get::<_, String>(2)?),
                mime_type: row.get(3)?,
                name: row.get(4)?,
                text: row.get(5)?,
                data_base64: binary.map(|bytes| BASE64.encode(bytes)),
                external_uri: row.get(7)?,
                content_hash: row.get(8)?,
                original_bytes: row.get(9)?,
                truncated: row.get(10)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;
