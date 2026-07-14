use crate::{hash_text, ModelInfo, ProjectInfo, SourceId, UsageCounts};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const ARCHIVE_CONVERSATION_SCHEMA_VERSION: &str = "archive_conversation.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArtifactDependency {
    pub cache_key: String,
    pub path: PathBuf,
    pub metadata_signature: String,
}

#[must_use]
pub fn archive_artifact_metadata_signature(path: &Path) -> String {
    let Ok(metadata) = std::fs::metadata(path) else {
        return "missing".to_string();
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
    let (seconds, nanos) = modified
        .map(|value| (value.as_secs(), value.subsec_nanos()))
        .unwrap_or((0, 0));
    let created = metadata
        .created()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
    let (created_seconds, created_nanos) = created
        .map(|value| (value.as_secs(), value.subsec_nanos()))
        .unwrap_or((0, 0));
    let file_type = if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "other"
    };
    let canonical_path = std::fs::canonicalize(path)
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    };
    #[cfg(not(unix))]
    let mode = u32::from(metadata.permissions().readonly());
    hash_text(&format!(
        "artifact-meta.v1:{file_type}:{}:{seconds}:{nanos}:{created_seconds}:{created_nanos}:{mode}:{canonical_path}",
        metadata.len()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveCompleteness {
    Complete,
    Partial,
    MetadataOnly,
}

impl ArchiveCompleteness {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::MetadataOnly => "metadata_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveItemKind {
    Message,
    ToolCall,
    ToolResult,
    ReasoningSummary,
    Artifact,
}

impl ArchiveItemKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ReasoningSummary => "reasoning_summary",
            Self::Artifact => "artifact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveRole {
    User,
    Assistant,
    Developer,
    System,
    Tool,
    Unknown,
}

impl ArchiveRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Developer => "developer",
            Self::System => "system",
            Self::Tool => "tool",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "developer" => Self::Developer,
            "system" => Self::System,
            "tool" | "tool_result" => Self::Tool,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveContentKind {
    Text,
    Image,
    File,
    Audio,
    Json,
}

impl ArchiveContentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::File => "file",
            Self::Audio => "audio",
            Self::Json => "json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveConversation {
    pub schema_version: String,
    pub conversation_id: String,
    pub provider: String,
    pub source_id: SourceId,
    pub native_conversation_id: String,
    pub title: Option<String>,
    pub project: Option<ProjectInfo>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub completeness: ArchiveCompleteness,
    pub missing_content_count: u64,
    #[serde(skip)]
    pub missing_content_scope_id: Option<String>,
    #[serde(skip, default)]
    pub discarded_source_record_ids: Vec<String>,
    #[serde(skip, default)]
    pub superseded_conversation_ids: Vec<String>,
    pub items: Vec<ArchiveItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveItem {
    pub item_id: String,
    pub native_item_id: Option<String>,
    pub source_record_id: Option<String>,
    pub ordinal: u64,
    pub kind: ArchiveItemKind,
    pub role: Option<ArchiveRole>,
    pub created_at: Option<DateTime<Utc>>,
    pub model: Option<ModelInfo>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub status: Option<String>,
    pub usage: Option<UsageCounts>,
    #[serde(skip, default)]
    pub parts_authoritative: bool,
    pub parts: Vec<ArchiveContentPart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveContentPart {
    pub content_id: String,
    pub ordinal: u64,
    pub kind: ArchiveContentKind,
    pub mime_type: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub data_base64: Option<String>,
    pub external_uri: Option<String>,
    pub content_hash: String,
    pub original_bytes: u64,
    pub truncated: bool,
}

impl ArchiveContentPart {
    #[must_use]
    pub fn text(content_id: String, ordinal: u64, kind: ArchiveContentKind, text: String) -> Self {
        let original_bytes = text.len() as u64;
        let content_hash = hash_text(&text);
        Self {
            content_id,
            ordinal,
            kind,
            mime_type: None,
            name: None,
            text: Some(text),
            data_base64: None,
            external_uri: None,
            content_hash,
            original_bytes,
            truncated: false,
        }
    }

    pub fn binary(
        content_id: String,
        ordinal: u64,
        kind: ArchiveContentKind,
        mime_type: Option<String>,
        name: Option<String>,
        data_base64: String,
    ) -> Result<Self, base64::DecodeError> {
        let bytes = BASE64.decode(data_base64.as_bytes())?;
        Ok(Self::binary_bytes(
            content_id, ordinal, kind, mime_type, name, &bytes,
        ))
    }

    #[must_use]
    pub fn binary_bytes(
        content_id: String,
        ordinal: u64,
        kind: ArchiveContentKind,
        mime_type: Option<String>,
        name: Option<String>,
        bytes: &[u8],
    ) -> Self {
        Self {
            content_id,
            ordinal,
            kind,
            mime_type,
            name,
            text: None,
            data_base64: Some(BASE64.encode(bytes)),
            external_uri: None,
            content_hash: hash_archive_bytes(bytes),
            original_bytes: bytes.len() as u64,
            truncated: false,
        }
    }
}

#[must_use]
pub fn archive_conversation_id(provider: &str, native_conversation_id: &str) -> String {
    format!(
        "conv_{}",
        &hash_text(&format!("{provider}:{native_conversation_id}"))[..24]
    )
}

#[must_use]
pub fn archive_item_id(
    provider: &str,
    native_conversation_id: &str,
    native_item_id: Option<&str>,
    ordinal: u64,
    content_fingerprint: &str,
) -> String {
    let identity = native_item_id
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{ordinal}:{content_fingerprint}"));
    format!(
        "item_{}",
        &hash_text(&format!("{provider}:{native_conversation_id}:{identity}"))[..24]
    )
}

#[must_use]
pub fn archive_content_id(item_id: &str, ordinal: u64) -> String {
    format!(
        "content_{}",
        &hash_text(&format!("{item_id}:{ordinal}"))[..24]
    )
}

#[must_use]
pub fn hash_archive_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_content_round_trips_exact_bytes() {
        let bytes = [0, 1, 2, 127, 128, 255];
        let part = ArchiveContentPart::binary(
            "content_1".to_string(),
            0,
            ArchiveContentKind::Image,
            Some("image/png".to_string()),
            None,
            BASE64.encode(bytes),
        )
        .expect("valid base64");

        assert_eq!(BASE64.decode(part.data_base64.unwrap()).unwrap(), bytes);
        assert_eq!(part.original_bytes, bytes.len() as u64);
        assert!(!part.truncated);
    }

    #[test]
    fn archive_ids_ignore_device_and_source_paths() {
        assert_eq!(
            archive_conversation_id("codex", "thread-123"),
            archive_conversation_id("codex", "thread-123")
        );
        assert_ne!(
            archive_conversation_id("codex", "thread-123"),
            archive_conversation_id("claude_code", "thread-123")
        );
    }
}
