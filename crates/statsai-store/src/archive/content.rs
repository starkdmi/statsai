use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRetentionQuality {
    pub(crate) materialized: bool,
    pub(crate) external: bool,
    pub(crate) stored_bytes: u64,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetainedContentPart {
    /// Item the stored row belongs to. Retention is read for a whole
    /// conversation at once, so an authoritative item must be able to tell its
    /// own obsolete content apart from its neighbours'.
    pub(crate) item_id: String,
    pub(crate) content_hash: String,
    pub(crate) quality: ContentRetentionQuality,
    /// Everything persisted about the content that its hash does not cover.
    ///
    /// The hash is taken over the content alone, and identically over text and
    /// over bytes, so it says nothing about how the content is described or
    /// even which column holds it. Everything a read would reconstruct is
    /// compared here rather than inferred from what a provider happens to
    /// produce today.
    pub(crate) metadata: RetainedContentMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetainedContentMetadata {
    pub(crate) ordinal: u64,
    pub(crate) kind: String,
    pub(crate) mime_type: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) external_uri: Option<String>,
    pub(crate) original_bytes: u64,
    pub(crate) stored_as_text: bool,
    pub(crate) stored_as_binary: bool,
}

impl RetainedContentPart {
    /// Whether the stored row would read back as exactly this part.
    pub(crate) fn already_stores(
        &self,
        part: &ArchiveContentPart,
        quality: ContentRetentionQuality,
        item_id: &str,
    ) -> bool {
        self.item_id == item_id
            && self.content_hash == part.content_hash
            && self.quality == quality
            && self.metadata
                == RetainedContentMetadata {
                    ordinal: part.ordinal,
                    kind: part.kind.as_str().to_string(),
                    mime_type: part.mime_type.clone(),
                    name: part.name.clone(),
                    external_uri: part.external_uri.clone(),
                    original_bytes: part.original_bytes,
                    stored_as_text: part.text.is_some(),
                    stored_as_binary: part.data_base64.is_some(),
                }
    }
}

/// Rows per batched write.
///
/// Large enough to amortize the full-text index flush that SQLite performs at
/// every statement boundary, small enough to stay far inside the bound on host
/// parameters per statement.
pub(crate) const ARCHIVE_WRITE_BATCH_ROWS: usize = 64;
/// Content bytes after which a batch is issued regardless of its row count.
pub(crate) const ARCHIVE_WRITE_BATCH_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const ARCHIVE_DELETE_BATCH_ROWS: usize = 256;

/// Item columns that are derived rather than stored directly on [`ArchiveItem`].
pub(crate) struct EncodedItem {
    pub(crate) kind: &'static str,
    pub(crate) role: Option<&'static str>,
    pub(crate) created_at: Option<String>,
    pub(crate) model_json: Option<String>,
    pub(crate) usage_json: Option<String>,
}

pub(crate) struct PendingContentPart<'a> {
    pub(crate) part: &'a ArchiveContentPart,
    pub(crate) item_id: &'a str,
    pub(crate) kind: &'static str,
    pub(crate) binary_content: Option<Vec<u8>>,
}

impl PendingContentPart<'_> {
    pub(crate) fn stored_bytes(&self) -> usize {
        self.part.text.as_ref().map_or(0, String::len)
            + self.binary_content.as_ref().map_or(0, |bytes| bytes.len())
    }
}

/// Appends `rows` parenthesised groups of `columns` placeholders.
pub(crate) fn append_row_placeholders(sql: &mut String, rows: usize, columns: usize) {
    for row in 0..rows {
        if row > 0 {
            sql.push(',');
        }
        if columns == 1 {
            sql.push('?');
            continue;
        }
        sql.push('(');
        for column in 0..columns {
            if column > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');
    }
}

/// Byte length standard base64 decodes to, without decoding it.
///
/// Every encoded payload this store accepts is canonical: the archive types
/// re-encode the bytes they hashed, so the length follows from the encoding
/// alone and the decision to keep or replace a row can be made before paying
/// for the decode.
pub(crate) fn base64_decoded_len(encoded: &str) -> u64 {
    let padding = encoded
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count()
        .min(2) as u64;
    (encoded.len() as u64 / 4) * 3 - padding
}

pub(crate) fn incoming_content_should_replace(
    existing: &RetainedContentPart,
    incoming_hash: &str,
    incoming: ContentRetentionQuality,
) -> bool {
    if existing.content_hash != incoming_hash {
        return incoming.materialized || incoming.external;
    }
    match (existing.quality.materialized, incoming.materialized) {
        (true, false) => false,
        (false, true) => true,
        (true, true) if existing.quality.truncated != incoming.truncated => {
            existing.quality.truncated
        }
        (true, true) => incoming.stored_bytes >= existing.quality.stored_bytes,
        (false, false) if existing.quality.external != incoming.external => incoming.external,
        (false, false) => incoming.stored_bytes >= existing.quality.stored_bytes,
    }
}

pub(crate) fn parse_optional_timestamp(value: Option<String>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub(crate) fn archive_import_signature(source_signature: &str) -> String {
    statsai_core::hash_text(&format!("{ARCHIVE_IMPORT_REVISION}:{source_signature}"))
}

pub(crate) fn parse_completeness(value: &str) -> ArchiveCompleteness {
    match value {
        "complete" => ArchiveCompleteness::Complete,
        "metadata_only" => ArchiveCompleteness::MetadataOnly,
        _ => ArchiveCompleteness::Partial,
    }
}

pub(crate) fn parse_item_kind(value: &str) -> ArchiveItemKind {
    match value {
        "tool_call" => ArchiveItemKind::ToolCall,
        "tool_result" => ArchiveItemKind::ToolResult,
        "reasoning_summary" => ArchiveItemKind::ReasoningSummary,
        "artifact" => ArchiveItemKind::Artifact,
        _ => ArchiveItemKind::Message,
    }
}

pub(crate) fn parse_role(value: &str) -> ArchiveRole {
    ArchiveRole::parse(value)
}

pub(crate) fn parse_content_kind(value: &str) -> ArchiveContentKind {
    match value {
        "image" => ArchiveContentKind::Image,
        "file" => ArchiveContentKind::File,
        "audio" => ArchiveContentKind::Audio,
        "json" => ArchiveContentKind::Json,
        _ => ArchiveContentKind::Text,
    }
}
