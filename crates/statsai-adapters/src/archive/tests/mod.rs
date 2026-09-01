pub(crate) use super::*;
pub(crate) use crate::{GrokBuildAdapter, OpenCodeAdapter, ProviderAdapter};
pub(crate) use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
pub(crate) use serde_json::Value;
pub(crate) use statsai_core::{ArchiveContentKind, ArchiveItemKind, ArchiveRole, LocationOrigin};
pub(crate) use std::fs::File;
pub(crate) use std::io::Write;
pub(crate) use tempfile::tempdir;
pub(crate) use url::Url;

fn source(provider: &str, path: &Path) -> SourceLocation {
    SourceLocation::local_adapter(provider, "test", "1", path, LocationOrigin::Configured)
}

mod item;
mod mutations;
mod scan;
