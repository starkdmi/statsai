use super::*;

impl Store {
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

    pub(crate) fn archive_conversation_with_binary(
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

    pub(crate) fn archive_items(&self, conversation_id: &str) -> Result<Vec<ArchiveItem>> {
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

    pub(crate) fn archive_content_parts(
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
