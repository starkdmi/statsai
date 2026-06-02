//! Provider adapters for local AI usage sources.

use ai_stats_core::{
    canonical_display, expand_home_path, hash_text, home_dir, semantic_event_id, summary_id,
    BillingPeriod, Confidence, EventSource, IdentitySource, LocationOrigin, ModelInfo,
    ParseEvidence, PrivacyInfo, PrivacyMode, ProjectInfo, SessionInfo, SourceKind, SourceLocation,
    SubscriptionStatus, SummaryMetadata, UsageCounts, UsageEvent, UsageSummary,
    USAGE_EVENT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
};
use ai_stats_pricing::{estimate_cost, normalize_model_name};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const CLAUDE_CODE_PROVIDER: &str = "claude_code";
pub const CODEX_PROVIDER: &str = "codex";
const SESSION_SCOPED_EVENT_KEY_VERSION: &str = "semantic_usage_event.v1";
const PATH_INDEPENDENT_EVENT_KEY_VERSION: &str = "semantic_usage_event.v2";
const SCAN_CACHE_SIGNATURE_VERSION: &str = "scan-cache.v1";

pub use ai_stats_core::{VerifiedSourceState, VerifiedSubscriptionState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDeduplication {
    SessionScoped,
    PathIndependent,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub device_id: String,
    pub selected_cache_keys: Option<HashSet<String>>,
}

impl ScanOptions {
    fn should_scan(&self, cache_key: &str) -> bool {
        self.selected_cache_keys
            .as_ref()
            .is_none_or(|selected| selected.contains(cache_key))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCandidateFile {
    pub path: PathBuf,
    pub cache_key: String,
    pub cache_signature: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanDiagnostics {
    pub files_scanned: u64,
    pub files_skipped_unchanged: u64,
    pub raw_rows: u64,
    pub candidate_usage_rows: u64,
    pub accepted_events: u64,
    pub duplicate_events: u64,
    pub skipped_zero_events: u64,
    pub invalid_rows: u64,
    pub timestamp_fallbacks: u64,
    pub model_fallbacks: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterScan {
    pub events: Vec<UsageEvent>,
    pub summaries: Vec<UsageSummary>,
    pub diagnostics: ScanDiagnostics,
    pub verified_source_state: Option<VerifiedSourceState>,
}

pub trait ProviderAdapter {
    fn id(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn provider(&self) -> &'static str;
    fn discover(&self) -> Vec<SourceLocation>;
    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>>;
    fn probe_verified_source_state(
        &self,
        _source: &SourceLocation,
    ) -> Result<Option<VerifiedSourceState>> {
        Ok(None)
    }
    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan>;
}

#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

impl ProviderAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code-local-jsonl"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        CLAUDE_CODE_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
            for root in split_paths(&value)
                .into_iter()
                .map(|path| normalize_claude_config_root(&path))
            {
                if root.join("projects").is_dir() && seen.insert(canonical_display(&root)) {
                    sources.push(claude_source_for_root(self, &root, LocationOrigin::Env));
                }
            }
            return sources;
        }

        if let Some(home) = home_dir() {
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            for root in [xdg.join("claude"), home.join(".claude")] {
                if root.join("projects").is_dir() && seen.insert(canonical_display(&root)) {
                    sources.push(claude_source_for_root(self, &root, LocationOrigin::Default));
                }
            }
        }

        sources
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        claude_scan_candidates(source, self.version())
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_claude_source(self, source, options)
    }
}

#[derive(Debug, Default)]
pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex-local-jsonl"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        CODEX_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(value) = std::env::var("CODEX_HOME") {
            for root in split_paths(&value) {
                if seen.insert(canonical_display(&root)) {
                    sources.push(codex_source_for_root(self, &root, LocationOrigin::Env));
                }
            }
            return sources;
        }

        if let Some(home) = home_dir() {
            let root = home.join(".codex");
            if root.exists() {
                sources.push(codex_source_for_root(self, &root, LocationOrigin::Default));
            }
        }

        sources
    }

    fn scan_candidates(&self, source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        codex_scan_candidates(source, self.version())
    }

    fn probe_verified_source_state(
        &self,
        source: &SourceLocation,
    ) -> Result<Option<VerifiedSourceState>> {
        let Some(root) = source_root_path(source) else {
            return Ok(None);
        };
        Ok(codex_auth_snapshot(&root))
    }

    fn scan(&self, source: &SourceLocation, options: &ScanOptions) -> Result<AdapterScan> {
        scan_codex_source(self, source, options)
    }
}

pub fn adapter_for_provider(provider: &str) -> Option<Box<dyn ProviderAdapter>> {
    match provider {
        CLAUDE_CODE_PROVIDER | "claude" | "claude-code" => Some(Box::new(ClaudeCodeAdapter)),
        CODEX_PROVIDER => Some(Box::new(CodexAdapter)),
        _ => None,
    }
}

pub fn default_adapters() -> Vec<Box<dyn ProviderAdapter>> {
    vec![Box::new(ClaudeCodeAdapter), Box::new(CodexAdapter)]
}

fn codex_source_for_root(
    adapter: &CodexAdapter,
    root: &Path,
    origin: LocationOrigin,
) -> SourceLocation {
    SourceLocation::local_adapter(
        adapter.provider(),
        adapter.id(),
        adapter.version(),
        root,
        origin,
    )
}

fn claude_source_for_root(
    adapter: &ClaudeCodeAdapter,
    root: &Path,
    origin: LocationOrigin,
) -> SourceLocation {
    let root = normalize_claude_config_root(root);
    SourceLocation::local_adapter(
        adapter.provider(),
        adapter.id(),
        adapter.version(),
        &root,
        origin,
    )
}

fn source_root_path(source: &SourceLocation) -> Option<PathBuf> {
    source.path_label.as_deref().map(PathBuf::from)
}

fn normalize_claude_config_root(root: &Path) -> PathBuf {
    if root.file_name().is_some_and(|name| name == "projects") {
        return root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
    }
    root.to_path_buf()
}

fn split_paths(value: &str) -> Vec<PathBuf> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(expand_home_path)
        .collect()
}

fn scan_claude_source(
    adapter: &ClaudeCodeAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(scan);
    };
    let root = normalize_claude_config_root(Path::new(path_label));
    if !root.exists() {
        return Ok(scan);
    }

    let projects = root.join("projects");
    let cache_namespace = scan_cache_namespace(source, adapter.version());
    let event_files = claude_jsonl_candidates(&projects, &cache_namespace)?;
    let mut seen = HashSet::new();
    {
        let mut ctx = FileParseContext {
            adapter,
            source,
            options,
            scan: &mut scan,
            seen: &mut seen,
        };
        for candidate in event_files {
            if !options.should_scan(&candidate.cache_key) {
                ctx.scan.diagnostics.files_skipped_unchanged += 1;
                continue;
            }
            ctx.scan.diagnostics.files_scanned += 1;
            parse_claude_file(&mut ctx, &projects, &candidate.path)?;
        }
    }

    if let Some(candidate) = claude_stats_cache_candidate(&root, &cache_namespace) {
        if options.should_scan(&candidate.cache_key) {
            scan.diagnostics.files_scanned += 1;
            parse_claude_stats_cache(adapter, source, options, &candidate.path, &mut scan)?;
        } else {
            scan.diagnostics.files_skipped_unchanged += 1;
        }
    }
    scan.diagnostics.accepted_events = scan.events.len() as u64;
    Ok(scan)
}

fn scan_codex_source(
    adapter: &CodexAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
) -> Result<AdapterScan> {
    let mut scan = AdapterScan::default();
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(scan);
    };
    let root = PathBuf::from(path_label);
    let cache_namespace = scan_cache_namespace(source, adapter.version());
    let mut seen = HashSet::new();
    {
        let mut ctx = FileParseContext {
            adapter,
            source,
            options,
            scan: &mut scan,
            seen: &mut seen,
        };
        for candidate in codex_jsonl_candidates(source, &root, &cache_namespace)? {
            if !options.should_scan(&candidate.cache_key) {
                ctx.scan.diagnostics.files_skipped_unchanged += 1;
                continue;
            }
            let usage_root = codex_usage_root_for_file(&root, &candidate.path);
            ctx.scan.diagnostics.files_scanned += 1;
            parse_codex_file(&mut ctx, &root, &usage_root, &candidate.path)?;
        }
    }
    scan.verified_source_state = codex_auth_snapshot(&root);
    scan.diagnostics.accepted_events = scan.events.len() as u64;
    Ok(scan)
}

fn collect_jsonl_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    Ok(files)
}

fn claude_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(Vec::new());
    };
    let root = normalize_claude_config_root(Path::new(path_label));
    if !root.exists() {
        return Ok(Vec::new());
    }
    let cache_namespace = scan_cache_namespace(source, adapter_version);

    let mut candidates = claude_jsonl_candidates(&root.join("projects"), &cache_namespace)?;
    if let Some(candidate) = claude_stats_cache_candidate(&root, &cache_namespace) {
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn claude_jsonl_candidates(root: &Path, cache_namespace: &str) -> Result<Vec<ScanCandidateFile>> {
    collect_jsonl_files(root)?
        .into_iter()
        .map(|path| Ok(scan_candidate(path, None, cache_namespace)))
        .collect()
}

fn claude_stats_cache_candidate(root: &Path, cache_namespace: &str) -> Option<ScanCandidateFile> {
    let path = root.join("stats-cache.json");
    path.is_file()
        .then(|| scan_candidate(path, None, cache_namespace))
}

fn codex_scan_candidates(
    source: &SourceLocation,
    adapter_version: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let Some(path_label) = source
        .path_label
        .as_deref()
        .filter(|label| !label.is_empty())
    else {
        return Ok(Vec::new());
    };
    let root = PathBuf::from(path_label);
    let cache_namespace = scan_cache_namespace(source, adapter_version);
    codex_jsonl_candidates(source, &root, &cache_namespace)
}

fn codex_jsonl_candidates(
    _source: &SourceLocation,
    root: &Path,
    cache_namespace: &str,
) -> Result<Vec<ScanCandidateFile>> {
    let mut roots = Vec::new();
    for child in ["sessions", "archived_sessions"] {
        let path = root.join(child);
        if path.is_dir() {
            roots.push(path);
        }
    }
    if roots.is_empty() && root.is_dir() {
        roots.push(root.to_path_buf());
    }

    let auth_dependency = Some(file_metadata_signature(&root.join("auth.json")));
    let dependency = auth_dependency.as_deref();
    let mut candidates = Vec::new();
    for usage_root in roots {
        for path in collect_jsonl_files(&usage_root)? {
            candidates.push(scan_candidate(path, dependency, cache_namespace));
        }
    }
    Ok(candidates)
}

fn codex_usage_root_for_file(root: &Path, path: &Path) -> PathBuf {
    for child in ["sessions", "archived_sessions"] {
        let usage_root = root.join(child);
        if path.starts_with(&usage_root) {
            return usage_root;
        }
    }
    root.to_path_buf()
}

fn scan_candidate(
    path: PathBuf,
    dependency_signature: Option<&str>,
    cache_namespace: &str,
) -> ScanCandidateFile {
    let cache_key = canonical_display(&path);
    let file_signature = file_metadata_signature(&path);
    let cache_signature = dependency_signature
        .map(|dependency| hash_text(&format!("{cache_namespace}:{file_signature}:{dependency}")))
        .unwrap_or_else(|| hash_text(&format!("{cache_namespace}:{file_signature}")));
    ScanCandidateFile {
        path,
        cache_key,
        cache_signature,
    }
}

fn scan_cache_namespace(source: &SourceLocation, adapter_version: &str) -> String {
    let adapter_id = source.adapter_id.as_deref().unwrap_or("");
    let path_hash = source.path_hash.as_deref().unwrap_or("");
    hash_text(&format!(
        "{SCAN_CACHE_SIGNATURE_VERSION}:{}:{:?}:{adapter_id}:{adapter_version}:{path_hash}",
        source.provider, source.source_kind
    ))
}

fn file_metadata_signature(path: &Path) -> String {
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
    hash_text(&format!(
        "meta.v2:{}:{}:{}:{}:{}",
        metadata.len(),
        seconds,
        nanos,
        created_seconds,
        created_nanos
    ))
}

struct FileParseContext<'a, A: ProviderAdapter + ?Sized> {
    adapter: &'a A,
    source: &'a SourceLocation,
    options: &'a ScanOptions,
    scan: &'a mut AdapterScan,
    seen: &'a mut HashSet<String>,
}

fn parse_claude_file(
    ctx: &mut FileParseContext<'_, ClaudeCodeAdapter>,
    projects: &Path,
    path: &Path,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let reader = BufReader::new(file);
    let fallback_timestamp = file_modified_timestamp(path).unwrap_or_else(Utc::now);

    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        ctx.scan.diagnostics.raw_rows += 1;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        };
        let Some(usage_value) = value
            .pointer("/message/usage")
            .or_else(|| value.get("usage"))
        else {
            continue;
        };
        ctx.scan.diagnostics.candidate_usage_rows += 1;
        let usage = claude_usage_counts_from_value(usage_value);
        if usage.computed_total() == 0 {
            ctx.scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        let (timestamp, timestamp_inferred) = timestamp_from_nested_value(&value)
            .map(|timestamp| (timestamp, false))
            .unwrap_or((fallback_timestamp, true));
        if timestamp_inferred {
            ctx.scan.diagnostics.timestamp_fallbacks += 1;
        }
        let model = model_from_nested_value(&value, None);
        let model_inferred = model.is_none();
        if model_inferred {
            ctx.scan.diagnostics.model_fallbacks += 1;
        }
        let session_raw = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| fallback_session_id(path));
        let project_key = project_key_from_path(projects, path);
        let event = usage_event(
            ctx.adapter,
            ctx.source,
            ctx.options,
            ProviderEventParts {
                timestamp,
                model,
                usage,
                session_raw,
                project_key,
                project_label: None,
                event_kind: "claude_message_usage",
                source_file: path,
                line_number: index + 1,
                model_inferred,
                timestamp_inferred,
                deduplication: EventDeduplication::SessionScoped,
            },
        );
        push_deduped(ctx.scan, ctx.seen, event);
    }

    Ok(())
}

fn parse_claude_stats_cache(
    adapter: &ClaudeCodeAdapter,
    source: &SourceLocation,
    options: &ScanOptions,
    path: &Path,
    scan: &mut AdapterScan,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(model_usage) = value.get("modelUsage").and_then(Value::as_object) else {
        scan.diagnostics.invalid_rows += 1;
        return Ok(());
    };

    let period_start = value
        .get("firstSessionDate")
        .and_then(timestamp_from_scalar);
    let period_end = value.get("lastComputedDate").and_then(stats_cache_date_end);
    let observed_at = period_end
        .or_else(|| file_modified_timestamp(path))
        .unwrap_or_else(Utc::now);
    let metadata = SummaryMetadata {
        summary_format: "claude_stats_cache".to_string(),
        summary_version: value
            .get("version")
            .and_then(value_as_u64)
            .map(|value| value.to_string()),
        total_sessions: value.get("totalSessions").and_then(value_as_u64),
        total_messages: value.get("totalMessages").and_then(value_as_u64),
        last_computed_at: period_end,
    };
    let file_path_hash = hash_text(&canonical_display(path));

    for (model_name, usage_value) in model_usage {
        scan.diagnostics.candidate_usage_rows += 1;
        let usage = claude_usage_counts_from_value(usage_value);
        if usage.computed_total() == 0 {
            scan.diagnostics.skipped_zero_events += 1;
            continue;
        }
        let model = model_info(model_name);
        let semantic_key = format!(
            "claude_stats_cache.v1:{}:{}:{}:{}:{}:{}:{}:{}",
            model_name,
            period_start
                .map(|date| date.to_rfc3339())
                .unwrap_or_else(|| "unknown_start".to_string()),
            period_end
                .map(|date| date.to_rfc3339())
                .unwrap_or_else(|| "unknown_end".to_string()),
            usage.input_tokens.unwrap_or(0),
            usage.cache_read_tokens.unwrap_or(0),
            usage.cache_creation_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
            usage.computed_total(),
        );
        let mut cost = estimate_cost(adapter.provider(), Some(&model), &usage);
        if let Some(provider_cost) = usage_value
            .get("costUSD")
            .and_then(Value::as_f64)
            .filter(|cost| *cost > 0.0)
        {
            cost.provider_reported_usd = Some(provider_cost);
            cost.pricing_source = Some("claude_stats_cache:costUSD".to_string());
            cost.confidence = Confidence::Medium;
        }
        scan.summaries.push(UsageSummary {
            schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
            summary_id: summary_id(adapter.provider(), &source.source_id, &semantic_key),
            device_id: options.device_id.clone(),
            provider: adapter.provider().to_string(),
            source_id: source.source_id.clone(),
            provider_account_id: None,
            source: EventSource {
                adapter_id: adapter.id().to_string(),
                adapter_version: adapter.version().to_string(),
                source_kind: SourceKind::LocalSummary,
                location_origin: Some(source.location_origin.clone()),
                source_type: "stats-cache.json".to_string(),
                source_path_hash: Some(file_path_hash.clone()),
                source_record_id: Some(format!("summary_key_{}", &hash_text(&semantic_key)[..32])),
                parse_confidence: Confidence::Medium,
            },
            model: Some(model),
            models: Vec::new(),
            usage,
            cost,
            parse_evidence: Some(ParseEvidence {
                event_key_version: "claude_stats_cache_summary.v1".to_string(),
                source_file_path_hash: Some(file_path_hash.clone()),
                source_line_number: None,
                source_record_id: Some(semantic_key),
                model_inferred: false,
                timestamp_inferred: period_start.is_none() || period_end.is_none(),
                account_identity_source: IdentitySource::Unresolved,
            }),
            privacy: metadata_only_privacy(),
            period_start,
            period_end,
            observed_at,
            metadata: metadata.clone(),
            imported_at: Utc::now(),
        });
    }

    Ok(())
}

fn parse_codex_file(
    ctx: &mut FileParseContext<'_, CodexAdapter>,
    root: &Path,
    usage_root: &Path,
    path: &Path,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let reader = BufReader::new(file);
    let fallback_timestamp = file_modified_timestamp(path).unwrap_or_else(Utc::now);
    let mut previous_totals: Option<UsageCounts> = None;
    let mut current_model: Option<String> = None;
    let mut current_model_is_fallback = false;
    let session_raw = codex_session_id(usage_root, path);

    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        ctx.scan.diagnostics.raw_rows += 1;
        if !codex_line_could_have_usage_or_context(&line) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            ctx.scan.diagnostics.invalid_rows += 1;
            continue;
        };

        if is_codex_turn_context(&value) {
            if let Some(model) = codex_model_from_value(&value, current_model.as_deref())
                .and_then(|model| model.normalized_name)
            {
                current_model = Some(model);
                current_model_is_fallback = false;
            }
            continue;
        }

        let is_token_count_event = is_codex_token_count(&value);
        let usage = if is_token_count_event {
            let info = value.pointer("/payload/info");
            let total_usage = info
                .and_then(|info| info.get("total_token_usage"))
                .map(codex_usage_counts_from_value);
            let usage = info
                .and_then(|info| info.get("last_token_usage"))
                .map(codex_usage_counts_from_value)
                .or_else(|| {
                    total_usage
                        .as_ref()
                        .map(|total| subtract_usage_counts(total, previous_totals.as_ref()))
                });
            if let Some(total) = total_usage {
                previous_totals = Some(total);
            }
            usage
        } else {
            codex_headless_usage_value(&value).map(codex_usage_counts_from_value)
        };

        let Some(usage) = usage else {
            continue;
        };
        ctx.scan.diagnostics.candidate_usage_rows += 1;
        if usage.computed_total() == 0 {
            ctx.scan.diagnostics.skipped_zero_events += 1;
            continue;
        }

        let (timestamp, timestamp_inferred) = timestamp_from_nested_value(&value)
            .map(|timestamp| (timestamp, false))
            .unwrap_or((fallback_timestamp, true));
        if timestamp_inferred {
            ctx.scan.diagnostics.timestamp_fallbacks += 1;
        }

        let parsed_model = codex_model_from_value(&value, current_model.as_deref());
        let mut model_inferred = false;
        let model = parsed_model.or_else(|| {
            model_inferred = true;
            current_model_is_fallback = true;
            Some(model_info("gpt-5"))
        });
        if let Some(model) = model
            .as_ref()
            .and_then(|model| model.normalized_name.clone())
        {
            if !model_inferred {
                current_model = Some(model);
                current_model_is_fallback = false;
            }
        }
        if current_model_is_fallback && !model_inferred {
            model_inferred = true;
        }
        if model_inferred {
            ctx.scan.diagnostics.model_fallbacks += 1;
        }

        let project_key = project_key_from_path(root, path);
        let event_session_raw =
            session_raw_from_value(&value).unwrap_or_else(|| session_raw.clone());
        let event = usage_event(
            ctx.adapter,
            ctx.source,
            ctx.options,
            ProviderEventParts {
                timestamp,
                model,
                usage,
                session_raw: event_session_raw,
                project_key,
                project_label: None,
                event_kind: if is_token_count_event {
                    "codex_token_count"
                } else {
                    "codex_headless_usage"
                },
                source_file: path,
                line_number: index + 1,
                model_inferred,
                timestamp_inferred,
                deduplication: if is_token_count_event {
                    EventDeduplication::PathIndependent
                } else {
                    EventDeduplication::SessionScoped
                },
            },
        );
        push_deduped(ctx.scan, ctx.seen, event);
    }

    Ok(())
}

fn push_deduped(scan: &mut AdapterScan, seen: &mut HashSet<String>, event: UsageEvent) {
    let key = event
        .parse_evidence
        .as_ref()
        .and_then(|evidence| evidence.source_record_id.clone())
        .unwrap_or_else(|| event.event_id.0.clone());
    if seen.insert(key) {
        scan.events.push(event);
    } else {
        scan.diagnostics.duplicate_events += 1;
    }
}

struct ProviderEventParts<'a> {
    timestamp: DateTime<Utc>,
    model: Option<ModelInfo>,
    usage: UsageCounts,
    session_raw: String,
    project_key: Option<String>,
    project_label: Option<String>,
    event_kind: &'static str,
    source_file: &'a Path,
    line_number: usize,
    model_inferred: bool,
    timestamp_inferred: bool,
    deduplication: EventDeduplication,
}

fn usage_event<A: ProviderAdapter + ?Sized>(
    adapter: &A,
    source: &SourceLocation,
    options: &ScanOptions,
    parts: ProviderEventParts<'_>,
) -> UsageEvent {
    let session_hash = hash_text(&parts.session_raw);
    let model_key = parts
        .model
        .as_ref()
        .and_then(|model| model.normalized_name.as_deref().or(model.name.as_deref()))
        .unwrap_or("unknown");
    let (event_key_version, semantic_key) = match parts.deduplication {
        EventDeduplication::SessionScoped => (
            SESSION_SCOPED_EVENT_KEY_VERSION,
            format!(
                "{SESSION_SCOPED_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                parts.event_kind,
                session_hash,
                parts.timestamp.timestamp_millis(),
                model_key,
                parts.usage.input_tokens.unwrap_or(0),
                parts.usage.cache_read_tokens.unwrap_or(0),
                parts.usage.output_tokens.unwrap_or(0),
                parts.usage.reasoning_tokens.unwrap_or(0),
                parts.usage.computed_total()
            ),
        ),
        EventDeduplication::PathIndependent => (
            PATH_INDEPENDENT_EVENT_KEY_VERSION,
            format!(
                "{PATH_INDEPENDENT_EVENT_KEY_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}",
                parts.event_kind,
                parts.timestamp.timestamp_millis(),
                model_key,
                parts.usage.input_tokens.unwrap_or(0),
                parts.usage.cache_read_tokens.unwrap_or(0),
                parts.usage.output_tokens.unwrap_or(0),
                parts.usage.reasoning_tokens.unwrap_or(0),
                parts.usage.computed_total()
            ),
        ),
    };
    let event_id = semantic_event_id(adapter.provider(), &source.source_id, &semantic_key);
    let file_path_hash = hash_text(&canonical_display(parts.source_file));
    let source_record_id = format!("usage_key_{}", &hash_text(&semantic_key)[..32]);
    let project = parts.project_key.map(|project_key| ProjectInfo {
        project_id: format!("project_{}", &hash_text(&project_key)[..24]),
        project_label: parts.project_label,
        repo_remote_hash: None,
        repo_label: None,
        branch_hash: None,
        branch_label: None,
    });

    let cost = estimate_cost(adapter.provider(), parts.model.as_ref(), &parts.usage);

    UsageEvent {
        schema_version: USAGE_EVENT_SCHEMA_VERSION.to_string(),
        event_id,
        device_id: options.device_id.clone(),
        provider: adapter.provider().to_string(),
        source_id: source.source_id.clone(),
        provider_account_id: None,
        subscription_id: None,
        source: EventSource {
            adapter_id: adapter.id().to_string(),
            adapter_version: adapter.version().to_string(),
            source_kind: source.source_kind.clone(),
            location_origin: Some(source.location_origin.clone()),
            source_type: "jsonl".to_string(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(source_record_id.clone()),
            parse_confidence: if parts.model_inferred || parts.timestamp_inferred {
                Confidence::Medium
            } else {
                Confidence::High
            },
        },
        session: SessionInfo {
            session_id: format!("session_{}", &session_hash[..24]),
            local_session_id_hash: Some(session_hash),
            title: None,
            started_at: parts.timestamp,
            ended_at: None,
            duration_seconds: None,
        },
        model: parts.model,
        runtime: None,
        cost,
        parse_evidence: Some(ParseEvidence {
            event_key_version: event_key_version.to_string(),
            source_file_path_hash: Some(file_path_hash),
            source_line_number: Some(parts.line_number as u64),
            source_record_id: Some(semantic_key),
            model_inferred: parts.model_inferred,
            timestamp_inferred: parts.timestamp_inferred,
            account_identity_source: IdentitySource::Unresolved,
        }),
        usage: parts.usage,
        project,
        git: None,
        privacy: metadata_only_privacy(),
        created_at: parts.timestamp,
        imported_at: Utc::now(),
    }
}

fn claude_usage_counts_from_value(value: &Value) -> UsageCounts {
    let input = number_at_any(
        value,
        &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
            "input",
        ],
    );
    let output = number_at_any(
        value,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
            "output",
        ],
    );
    let cache_creation = number_at_any(
        value,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cacheCreationTokens",
            "cache_creation_tokens",
        ],
    );
    let cache_read = number_at_any(
        value,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "cacheReadTokens",
            "cache_read_tokens",
            "cached_input_tokens",
        ],
    );
    let reasoning = number_at_any(
        value,
        &[
            "reasoning_tokens",
            "reasoningTokens",
            "reasoning_output_tokens",
            "reasoningOutputTokens",
        ],
    );
    let total = number_at_any(value, &["total_tokens", "totalTokens", "total"]);
    let output = output
        .or_else(|| infer_missing_output(total, input, cache_creation, cache_read, reasoning));

    UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_creation,
        cache_read_tokens: cache_read,
        reasoning_tokens: reasoning,
        total_tokens: total,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

fn codex_usage_counts_from_value(value: &Value) -> UsageCounts {
    let raw_input = number_at_any(value, &["input_tokens", "prompt_tokens", "input"]);
    let raw_output = number_at_any(value, &["output_tokens", "completion_tokens", "output"]);
    let raw_cache_creation = number_at_any(
        value,
        &[
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cache_creation_tokens",
            "cacheCreationTokens",
        ],
    );
    let raw_cache_read = number_at_any(
        value,
        &[
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cached_tokens",
        ],
    );
    let raw_reasoning = number_at_any(value, &["reasoning_output_tokens", "reasoning_tokens"]);
    let total = number_at_any(value, &["total_tokens", "total"]);

    normalize_codex_usage_counts(
        raw_input,
        raw_output,
        raw_cache_creation,
        raw_cache_read,
        raw_reasoning,
        total,
    )
}

fn infer_missing_output(
    total: Option<u64>,
    input: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
    reasoning: Option<u64>,
) -> Option<u64> {
    total.and_then(|total| {
        let known = input.unwrap_or(0)
            + cache_creation.unwrap_or(0)
            + cache_read.unwrap_or(0)
            + reasoning.unwrap_or(0);
        (total > known).then_some(total - known)
    })
}

// Codex reports cached input and reasoning output as subsets of the top-level
// input/output counters. Normalize that inclusive provider shape into the
// additive contract used everywhere else in ai-stats.
fn normalize_codex_usage_counts(
    raw_input: Option<u64>,
    raw_output: Option<u64>,
    raw_cache_creation: Option<u64>,
    raw_cache_read: Option<u64>,
    raw_reasoning: Option<u64>,
    total: Option<u64>,
) -> UsageCounts {
    let cache_creation = match (raw_input, raw_cache_creation) {
        (Some(input), Some(cache_creation)) => Some(cache_creation.min(input)),
        _ => raw_cache_creation,
    };
    let cache_read = match (raw_input, raw_cache_read) {
        (Some(input), Some(cache_read)) => Some(cache_read.min(input)),
        _ => raw_cache_read,
    };
    let reasoning = match (raw_output, raw_reasoning) {
        (Some(output), Some(reasoning)) => Some(reasoning.min(output)),
        _ => raw_reasoning,
    };
    let input = raw_input.map(|input| {
        input
            .saturating_sub(cache_creation.unwrap_or(0))
            .saturating_sub(cache_read.unwrap_or(0))
    });
    let output = raw_output
        .map(|output| output.saturating_sub(reasoning.unwrap_or(0)))
        .or_else(|| infer_missing_output(total, input, cache_creation, cache_read, reasoning));
    let total = total.or_else(|| {
        (input.is_some()
            || output.is_some()
            || cache_creation.is_some()
            || cache_read.is_some()
            || reasoning.is_some())
        .then_some(
            input
                .unwrap_or(0)
                .saturating_add(output.unwrap_or(0))
                .saturating_add(cache_creation.unwrap_or(0))
                .saturating_add(cache_read.unwrap_or(0))
                .saturating_add(reasoning.unwrap_or(0)),
        )
    });

    UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_creation,
        cache_read_tokens: cache_read,
        reasoning_tokens: reasoning,
        total_tokens: total,
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

fn subtract_usage_counts(current: &UsageCounts, previous: Option<&UsageCounts>) -> UsageCounts {
    let subtract = |left: Option<u64>, right: Option<u64>| {
        let value = left.unwrap_or(0).saturating_sub(right.unwrap_or(0));
        (value > 0).then_some(value)
    };
    UsageCounts {
        input_tokens: subtract(
            current.input_tokens,
            previous.and_then(|usage| usage.input_tokens),
        ),
        output_tokens: subtract(
            current.output_tokens,
            previous.and_then(|usage| usage.output_tokens),
        ),
        cache_creation_tokens: subtract(
            current.cache_creation_tokens,
            previous.and_then(|usage| usage.cache_creation_tokens),
        ),
        cache_read_tokens: subtract(
            current.cache_read_tokens,
            previous.and_then(|usage| usage.cache_read_tokens),
        ),
        reasoning_tokens: subtract(
            current.reasoning_tokens,
            previous.and_then(|usage| usage.reasoning_tokens),
        ),
        total_tokens: subtract(
            current.total_tokens,
            previous.and_then(|usage| usage.total_tokens),
        ),
        requests: Some(1),
        local_prompt_eval_tokens: None,
        local_eval_tokens: None,
    }
}

fn number_at_any(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(value_as_u64))
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_i64()
                .and_then(|value| (value >= 0).then_some(value as u64))
        })
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
}

fn timestamp_from_nested_value(value: &Value) -> Option<DateTime<Utc>> {
    for candidate in [
        value.get("timestamp"),
        value.get("created_at"),
        value.get("createdAt"),
        value.get("time"),
        value.pointer("/message/timestamp"),
        value.pointer("/data/timestamp"),
        value.pointer("/result/timestamp"),
        value.pointer("/response/timestamp"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(timestamp) = timestamp_from_scalar(candidate) {
            return Some(timestamp);
        }
    }
    None
}

fn timestamp_from_scalar(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Ok(millis) = text.parse::<i64>() {
            return timestamp_from_number(millis);
        }
    }
    value.as_i64().and_then(timestamp_from_number)
}

fn stats_cache_date_end(value: &Value) -> Option<DateTime<Utc>> {
    timestamp_from_scalar(value).or_else(|| {
        let text = value.as_str()?;
        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        Some(date.and_hms_opt(23, 59, 59)?.and_utc())
    })
}

fn timestamp_from_number(value: i64) -> Option<DateTime<Utc>> {
    if value > 10_000_000_000 {
        Utc.timestamp_millis_opt(value).single()
    } else {
        Utc.timestamp_opt(value, 0).single()
    }
}

fn file_modified_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(DateTime::<Utc>::from)
}

fn model_from_nested_value(value: &Value, fallback: Option<&str>) -> Option<ModelInfo> {
    let model = [
        value.get("model"),
        value.get("model_name"),
        value.pointer("/metadata/model"),
        value.pointer("/message/model"),
        value.pointer("/usage/model"),
        value.pointer("/request/model"),
        value.pointer("/data/model"),
        value.pointer("/data/model_name"),
        value.pointer("/data/metadata/model"),
        value.pointer("/result/model"),
        value.pointer("/result/model_name"),
        value.pointer("/result/metadata/model"),
        value.pointer("/response/model"),
        value.pointer("/response/model_name"),
        value.pointer("/response/metadata/model"),
        value.pointer("/payload/model"),
        value.pointer("/payload/model_name"),
        value.pointer("/payload/metadata/model"),
        value.pointer("/payload/info/model"),
        value.pointer("/payload/info/model_name"),
        value.pointer("/payload/info/metadata/model"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .or(fallback)?;
    Some(model_info(model))
}

fn codex_model_from_value(value: &Value, fallback: Option<&str>) -> Option<ModelInfo> {
    model_from_nested_value(value, fallback)
}

fn model_info(model: &str) -> ModelInfo {
    let normalized = normalize_model_name(model);
    ModelInfo {
        name: Some(model.to_string()),
        normalized_name: Some(normalized),
        provider_model_id: Some(model.to_string()),
    }
}

fn is_codex_turn_context(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("turn_context")
}

fn is_codex_token_count(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
}

fn codex_line_could_have_usage_or_context(line: &str) -> bool {
    line.contains("\"turn_context\"")
        || line.contains("\"token_count\"")
        || line.contains("\"usage\"")
        || line.contains("\"input_tokens\"")
        || line.contains("\"prompt_tokens\"")
}

fn codex_headless_usage_value(value: &Value) -> Option<&Value> {
    [
        value.get("usage"),
        value.pointer("/data/usage"),
        value.pointer("/result/usage"),
        value.pointer("/response/usage"),
        value.get("token_count"),
        value.pointer("/event_msg/token_count"),
    ]
    .into_iter()
    .flatten()
    .next()
}

fn codex_auth_snapshot(root: &Path) -> Option<VerifiedSourceState> {
    let auth_path = root.join("auth.json");
    let value = std::fs::read_to_string(&auth_path).ok()?;
    let value: Value = serde_json::from_str(&value).ok()?;
    let payload = string_at_any(
        &value,
        &["id_token", "idToken", "/tokens/id_token", "/tokens/idToken"],
    )
    .and_then(|token| jwt_payload_value(&token));
    let auth = payload
        .as_ref()
        .and_then(|payload| payload.pointer("/https:~1~1api.openai.com~1auth"))
        .or_else(|| value.pointer("/https:~1~1api.openai.com~1auth"));

    let provider_user_id = auth
        .and_then(|auth| string_at_any(auth, &["chatgpt_account_id", "chatgpt_user_id", "user_id"]))
        .or_else(|| {
            string_at_any(
                &value,
                &[
                    "account_id",
                    "accountId",
                    "chatgpt_account_id",
                    "chatgpt_user_id",
                    "/tokens/account_id",
                    "/tokens/accountId",
                ],
            )
        });
    let email = payload
        .as_ref()
        .and_then(|payload| {
            string_at_any(
                payload,
                &["email", "/https:~1~1api.openai.com~1profile~1email"],
            )
        })
        .or_else(|| string_at_any(&value, &["email", "user_email"]))
        .map(|email| email.to_ascii_lowercase());
    if provider_user_id.is_none() && email.is_none() {
        return None;
    }

    let plan_type = auth.and_then(|auth| string_at_any(auth, &["chatgpt_plan_type"]));
    let plan_name = plan_type.as_deref().map(display_codex_plan_name);
    let authenticated_at = payload
        .as_ref()
        .and_then(|payload| timestamp_at_any(payload, &["auth_time", "iat"]))
        .or_else(|| file_modified_at(&auth_path));
    let verified_at = auth
        .and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_last_checked"]))
        .or(authenticated_at);
    let paid_at =
        auth.and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_active_start"]));
    let current_period_ends_at =
        auth.and_then(|auth| timestamp_at_any(auth, &["chatgpt_subscription_active_until"]));
    let subscription = plan_type.as_deref().and_then(|plan_type| {
        codex_verified_subscription(plan_type, paid_at, current_period_ends_at, verified_at)
    });

    Some(VerifiedSourceState {
        provider_user_id,
        email,
        account_label: None,
        plan_name,
        authenticated_at,
        verified_at,
        subscription,
    })
}

fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

fn string_at_any(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| {
            if key.starts_with('/') {
                value.pointer(key)
            } else {
                value.get(*key)
            }
        })
        .find_map(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn timestamp_at_any(value: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter()
        .filter_map(|key| {
            if key.starts_with('/') {
                value.pointer(key)
            } else {
                value.get(*key)
            }
        })
        .find_map(parse_timestamp_value)
}

fn parse_timestamp_value(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|parsed| parsed.with_timezone(&Utc)),
        Value::Number(number) => number
            .as_i64()
            .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single()),
        _ => None,
    }
}

fn display_codex_plan_name(plan_type: &str) -> String {
    match plan_type.trim().to_ascii_lowercase().as_str() {
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "free" => "Free".to_string(),
        other => other
            .split(['_', '-', ' '])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                let Some(first) = chars.next() else {
                    return String::new();
                };
                format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str().to_ascii_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn codex_verified_subscription(
    plan_type: &str,
    paid_at: Option<DateTime<Utc>>,
    current_period_ends_at: Option<DateTime<Utc>>,
    verified_at: Option<DateTime<Utc>>,
) -> Option<VerifiedSubscriptionState> {
    let started_at = paid_at?;
    let (plan_name, price) = match plan_type.trim().to_ascii_lowercase().as_str() {
        "plus" => ("Plus".to_string(), 20.0),
        "pro" => ("Pro".to_string(), 200.0),
        _ => return None,
    };
    Some(VerifiedSubscriptionState {
        plan_name,
        price,
        currency: "USD".to_string(),
        billing_period: BillingPeriod::Monthly,
        paid_at,
        started_at,
        ended_at: None,
        current_period_ends_at,
        status: SubscriptionStatus::Active,
        verified_at,
    })
}

fn jwt_payload_value(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_base64_url(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut bit_count = 0u8;
    let mut out = Vec::new();
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let six = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        bits = (bits << 6) | six;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    Some(out)
}

fn fallback_session_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn session_raw_from_value(value: &Value) -> Option<String> {
    [
        value.get("session_id"),
        value.get("sessionId"),
        value.pointer("/message/sessionId"),
        value.pointer("/message/session_id"),
        value.pointer("/data/session_id"),
        value.pointer("/result/session_id"),
        value.pointer("/response/session_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(ToOwned::to_owned)
}

fn codex_session_id(usage_root: &Path, path: &Path) -> String {
    path.strip_prefix(usage_root)
        .unwrap_or(path)
        .with_extension("")
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

fn project_key_from_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_only_privacy() -> PrivacyInfo {
    PrivacyInfo {
        mode: PrivacyMode::MetadataOnly,
        contains_prompt_text: false,
        contains_response_text: false,
        contains_file_paths: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn options() -> ScanOptions {
        ScanOptions {
            device_id: "device".to_string(),
            selected_cache_keys: None,
        }
    }

    #[test]
    fn codex_discovers_one_logical_source_per_home() {
        let adapter = CodexAdapter;
        let source = codex_source_for_root(
            &adapter,
            Path::new("/tmp/codex-home"),
            LocationOrigin::Configured,
        );

        assert_eq!(source.provider, CODEX_PROVIDER);
        assert_eq!(source.path_label.as_deref(), Some("/tmp/codex-home"));
    }

    #[test]
    fn claude_normalizes_projects_path_to_config_root() {
        let adapter = ClaudeCodeAdapter;
        let source = claude_source_for_root(
            &adapter,
            Path::new("/tmp/claude-home/projects"),
            LocationOrigin::Configured,
        );

        assert_eq!(source.provider, CLAUDE_CODE_PROVIDER);
        assert_eq!(source.path_label.as_deref(), Some("/tmp/claude-home"));
    }

    #[test]
    fn claude_source_scans_projects_child_when_config_root_is_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("projects");
        let transcripts = dir.path().join("transcripts");
        std::fs::create_dir_all(&projects).expect("projects");
        std::fs::create_dir_all(&transcripts).expect("transcripts");

        let mut project_file = File::create(projects.join("session.jsonl")).expect("project file");
        writeln!(
            project_file,
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}"
        )
        .expect("write project");
        let mut transcript_file =
            File::create(transcripts.join("transcript.jsonl")).expect("transcript file");
        writeln!(
            transcript_file,
            "{{\"message\":{{\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}}}"
        )
        .expect("write transcript");

        let source = SourceLocation::local_adapter(
            CLAUDE_CODE_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.raw_rows, 1);
        assert_eq!(scan.events[0].usage.computed_total(), 3);
    }

    #[test]
    fn claude_stats_cache_is_parsed_as_summary_not_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("projects")).expect("projects");
        let mut file = File::create(dir.path().join("stats-cache.json")).expect("stats cache");
        writeln!(
            file,
            r#"{{
              "version": 2,
              "lastComputedDate": "2026-05-13",
              "firstSessionDate": "2026-01-21T17:21:43.119Z",
              "totalSessions": 61,
              "totalMessages": 15679,
              "modelUsage": {{
                "claude-opus-4-5-thinking": {{
                  "inputTokens": 113622256,
                  "outputTokens": 387,
                  "cacheReadInputTokens": 282480618,
                  "cacheCreationInputTokens": 10,
                  "costUSD": 12.5
                }},
                "google/antigravity-empty": {{
                  "inputTokens": 0,
                  "outputTokens": 0,
                  "cacheReadInputTokens": 0,
                  "cacheCreationInputTokens": 0
                }}
              }}
            }}"#
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CLAUDE_CODE_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_claude_source(&ClaudeCodeAdapter, &source, &options()).expect("scan");

        assert!(scan.events.is_empty());
        assert_eq!(scan.summaries.len(), 1);
        assert_eq!(scan.diagnostics.skipped_zero_events, 1);
        assert_eq!(
            scan.summaries[0]
                .model
                .as_ref()
                .and_then(|model| model.name.as_deref()),
            Some("claude-opus-4-5-thinking")
        );
        assert_eq!(scan.summaries[0].usage.input_tokens, Some(113622256));
        assert_eq!(scan.summaries[0].usage.cache_read_tokens, Some(282480618));
        assert_eq!(scan.summaries[0].usage.cache_creation_tokens, Some(10));
        assert_eq!(scan.summaries[0].usage.output_tokens, Some(387));
        assert_eq!(scan.summaries[0].cost.provider_reported_usd, Some(12.5));
        assert_eq!(scan.summaries[0].metadata.total_sessions, Some(61));
        assert_eq!(scan.summaries[0].metadata.total_messages, Some(15679));
    }

    #[test]
    fn claude_scan_respects_selected_cache_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).expect("projects");

        let first = projects.join("a.jsonl");
        let second = projects.join("b.jsonl");
        std::fs::write(
            &first,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
        )
        .expect("first");
        std::fs::write(
            &second,
            "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n",
        )
        .expect("second");

        let source = SourceLocation::local_adapter(
            CLAUDE_CODE_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );
        let selected = [canonical_display(&first)].into_iter().collect();
        let scan = scan_claude_source(
            &ClaudeCodeAdapter,
            &source,
            &ScanOptions {
                device_id: "device".to_string(),
                selected_cache_keys: Some(selected),
            },
        )
        .expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.files_scanned, 1);
        assert_eq!(scan.diagnostics.files_skipped_unchanged, 1);
        assert_eq!(scan.events[0].usage.computed_total(), 3);
    }

    #[test]
    fn codex_source_scans_sessions_and_archived_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        let archived = dir.path().join("archived_sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::create_dir_all(&archived).expect("archived");

        let mut active_file = File::create(sessions.join("active.jsonl")).expect("active fixture");
        writeln!(
            active_file,
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
        )
        .expect("write active");
        let mut archived_file = File::create(archived.join("old.jsonl")).expect("archived fixture");
        writeln!(
            archived_file,
            "{{\"timestamp\":\"2026-05-02T00:00:00Z\",\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}"
        )
        .expect("write archived");

        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
        assert_eq!(scan.events.len(), 2);
        assert_eq!(scan.diagnostics.raw_rows, 2);
    }

    #[test]
    fn codex_scan_respects_selected_cache_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let first = sessions.join("a.jsonl");
        let second = sessions.join("b.jsonl");
        std::fs::write(
            &first,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
        )
        .expect("first");
        std::fs::write(
            &second,
            "{\"timestamp\":\"2026-05-01T00:01:00Z\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n",
        )
        .expect("second");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let selected = [canonical_display(&second)].into_iter().collect();
        let scan = scan_codex_source(
            &CodexAdapter,
            &source,
            &ScanOptions {
                device_id: "device".to_string(),
                selected_cache_keys: Some(selected),
            },
        )
        .expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.files_scanned, 1);
        assert_eq!(scan.diagnostics.files_skipped_unchanged, 1);
        assert_eq!(scan.events[0].usage.computed_total(), 7);
    }

    #[test]
    fn codex_scan_candidates_change_when_auth_json_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let session_path = sessions.join("session.jsonl");
        std::fs::write(
            &session_path,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
        )
        .expect("session");
        std::fs::write(
            dir.path().join("auth.json"),
            "{\"chatgpt_account_id\":\"acct-one\"}\n",
        )
        .expect("auth one");

        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let first = codex_scan_candidates(&source, "test-adapter").expect("first candidates");
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            dir.path().join("auth.json"),
            "{\"chatgpt_account_id\":\"acct-two\"}\n",
        )
        .expect("auth two");
        let second = codex_scan_candidates(&source, "test-adapter").expect("second candidates");

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].cache_key, canonical_display(&session_path));
        assert_ne!(first[0].cache_signature, second[0].cache_signature);
    }

    #[test]
    fn codex_scan_candidates_are_stable_for_same_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let session_path = sessions.join("session.jsonl");
        std::fs::write(
            &session_path,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
        )
        .expect("session");

        let hinted = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );
        let remapped = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let first = codex_scan_candidates(&hinted, "test-adapter").expect("first candidates");
        let second = codex_scan_candidates(&remapped, "test-adapter").expect("second candidates");

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].cache_key, canonical_display(&session_path));
        assert_eq!(first[0].cache_signature, second[0].cache_signature);
    }

    #[test]
    fn codex_dedupes_copied_branch_history_and_keeps_branch_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");

        let mut parent =
            File::create(sessions.join("2026-05-12T08-00-00-parent.jsonl")).expect("parent");
        writeln!(
            parent,
            r#"{{"timestamp":"2026-05-12T08:00:00.000Z","type":"turn_context","payload":{{"model":"gpt-5.2"}}}}"#
        )
        .expect("write parent context");
        writeln!(
            parent,
            r#"{{"timestamp":"2026-05-12T08:01:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":20,"total_tokens":1200}}}}}}}}"#
        )
        .expect("write parent tokens");

        let mut branch =
            File::create(sessions.join("2026-05-12T08-02-00-branch.jsonl")).expect("branch");
        writeln!(
            branch,
            r#"{{"timestamp":"2026-05-12T08:00:00.000Z","type":"turn_context","payload":{{"model":"gpt-5.2"}}}}"#
        )
        .expect("write branch context");
        writeln!(
            branch,
            r#"{{"timestamp":"2026-05-12T08:01:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1000,"cached_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":20,"total_tokens":1200}}}}}}}}"#
        )
        .expect("write branch copied parent tokens");
        writeln!(
            branch,
            r#"{{"timestamp":"2026-05-12T08:02:00.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1600,"cached_input_tokens":300,"output_tokens":450,"reasoning_output_tokens":40,"total_tokens":2050}}}}}}}}"#
        )
        .expect("write branch delta tokens");

        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 2);
        assert_eq!(scan.diagnostics.duplicate_events, 1);

        assert_eq!(scan.events[0].usage.input_tokens, Some(900));
        assert_eq!(scan.events[0].usage.cache_read_tokens, Some(100));
        assert_eq!(scan.events[0].usage.output_tokens, Some(180));
        assert_eq!(scan.events[0].usage.reasoning_tokens, Some(20));
        assert_eq!(scan.events[0].usage.total_tokens, Some(1200));

        assert_eq!(scan.events[1].usage.input_tokens, Some(400));
        assert_eq!(scan.events[1].usage.cache_read_tokens, Some(200));
        assert_eq!(scan.events[1].usage.output_tokens, Some(230));
        assert_eq!(scan.events[1].usage.reasoning_tokens, Some(20));
        assert_eq!(scan.events[1].usage.total_tokens, Some(850));
    }

    #[test]
    fn codex_prefers_active_session_copy_over_archived_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        let archived = dir.path().join("archived_sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::create_dir_all(&archived).expect("archived");

        let active_path = sessions.join("dup.jsonl");
        let archived_path = archived.join("dup.jsonl");
        std::fs::write(
            &active_path,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
        )
        .expect("active write");
        std::fs::write(
            &archived_path,
            "{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n",
        )
        .expect("archived write");

        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");
        let active_hash = hash_text(&canonical_display(&active_path));

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.duplicate_events, 1);
        assert_eq!(
            scan.events[0]
                .parse_evidence
                .as_ref()
                .and_then(|evidence| evidence.source_file_path_hash.as_deref()),
            Some(active_hash.as_str())
        );
    }

    #[test]
    fn codex_uses_last_token_usage_not_cumulative_total() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
        writeln!(
            file,
            r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5-codex","total_token_usage":{{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":50,"total_tokens":1000}},"last_token_usage":{{"input_tokens":90,"cached_input_tokens":30,"output_tokens":10,"reasoning_output_tokens":5,"total_tokens":100}}}}}}}}"#
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].usage.input_tokens, Some(60));
        assert_eq!(scan.events[0].usage.output_tokens, Some(5));
        assert_eq!(scan.events[0].usage.computed_total(), 100);
        assert_eq!(scan.events[0].usage.cache_read_tokens, Some(30));
        assert_eq!(scan.events[0].usage.reasoning_tokens, Some(5));
        assert!(scan.events[0].cost.estimated_api_equivalent_usd.is_some());
    }

    #[test]
    fn codex_subtracts_cumulative_total_usage_when_last_usage_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
        writeln!(
            file,
            r#"{{"timestamp":"2026-05-01T00:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5","total_token_usage":{{"input_tokens":100,"cached_input_tokens":10,"output_tokens":50,"total_tokens":150}}}}}}}}"#
        )
        .expect("write first");
        writeln!(
            file,
            r#"{{"timestamp":"2026-05-01T00:01:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model":"gpt-5","total_token_usage":{{"input_tokens":250,"cached_input_tokens":30,"output_tokens":75,"total_tokens":325}}}}}}}}"#
        )
        .expect("write second");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 2);
        assert_eq!(scan.events[0].usage.input_tokens, Some(90));
        assert_eq!(scan.events[1].usage.input_tokens, Some(130));
        assert_eq!(scan.events[1].usage.cache_read_tokens, Some(20));
        assert_eq!(scan.events[1].usage.output_tokens, Some(25));
        assert_eq!(scan.events[1].usage.total_tokens, Some(175));
    }

    #[test]
    fn codex_headless_usage_shapes_are_parsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let mut file = File::create(sessions.join("exec.jsonl")).expect("fixture");
        writeln!(
            file,
            r#"{{"data":{{"timestamp":"2026-05-01T00:00:00Z","model":"gpt-5","usage":{{"prompt_tokens":10,"completion_tokens":5,"cached_tokens":3}}}}}}"#
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].usage.input_tokens, Some(7));
        assert_eq!(scan.events[0].usage.output_tokens, Some(5));
        assert_eq!(scan.events[0].usage.cache_read_tokens, Some(3));
    }

    #[test]
    fn duplicated_semantic_events_are_deduped_within_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        for name in ["a.jsonl", "b.jsonl"] {
            let mut file = File::create(sessions.join(name)).expect("fixture");
            writeln!(
                file,
                "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"session_id\":\"same\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
            )
            .expect("write");
        }
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.diagnostics.duplicate_events, 1);
    }

    #[test]
    fn codex_auth_json_exposes_verified_source_state_without_stamping_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::json!({
                "email": "existing@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-real",
                    "chatgpt_plan_type": "plus",
                    "chatgpt_subscription_active_start": "2026-05-29T10:12:43+00:00",
                    "chatgpt_subscription_active_until": "2026-06-29T10:12:43+00:00",
                    "chatgpt_subscription_last_checked": "2026-05-29T10:14:56.058278+00:00"
                }
            })
            .to_string(),
        )
        .expect("auth");
        let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
        writeln!(
            file,
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        let verified = scan
            .verified_source_state
            .as_ref()
            .expect("verified source state");
        assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
        assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
        assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
        assert!(verified.authenticated_at.is_some());
        assert_eq!(
            verified.verified_at.map(|value| value.to_rfc3339()),
            Some("2026-05-29T10:14:56.058278+00:00".to_string())
        );
        let subscription = verified.subscription.as_ref().expect("subscription");
        assert_eq!(subscription.plan_name, "Plus");
        assert_eq!(subscription.price, 20.0);
        assert_eq!(
            subscription.started_at.to_rfc3339(),
            "2026-05-29T10:12:43+00:00"
        );
        assert_eq!(
            subscription
                .current_period_ends_at
                .map(|value| value.to_rfc3339()),
            Some("2026-06-29T10:12:43+00:00".to_string())
        );
        assert_eq!(subscription.ended_at, None);
        assert_eq!(scan.events[0].provider_account_id, None);
        assert_ne!(
            scan.events[0]
                .parse_evidence
                .as_ref()
                .map(|evidence| evidence.account_identity_source.clone()),
            Some(IdentitySource::LocalAuth)
        );
    }

    #[test]
    fn codex_auth_json_reads_nested_tokens_id_token_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ImV4aXN0aW5nQGV4YW1wbGUuY29tIiwiaWF0IjoxNzQ4NTEzNTYzLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1yZWFsIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIiwiY2hhdGdwdF9zdWJzY3JpcHRpb25fYWN0aXZlX3N0YXJ0IjoiMjAyNi0wNS0yOVQxMDoxMjo0MyswMDowMCIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6IjIwMjYtMDYtMjlUMTA6MTI6NDMrMDA6MDAiLCJjaGF0Z3B0X3N1YnNjcmlwdGlvbl9sYXN0X2NoZWNrZWQiOiIyMDI2LTA1LTI5VDEwOjE0OjU2LjA1ODI3OCswMDowMCJ9fQ.",
                    "access_token": "unused",
                    "refresh_token": "unused",
                    "account_id": "41412a8c-6e19-4d33-9b67-6fb4b4dc0734"
                },
                "last_refresh": "2026-05-19T19:56:03.481816Z"
            })
            .to_string(),
        )
        .expect("auth");
        let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
        writeln!(
            file,
            "{{\"timestamp\":\"2026-05-01T00:00:00Z\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}"
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        let verified = scan
            .verified_source_state
            .as_ref()
            .expect("verified source state");
        assert_eq!(verified.provider_user_id.as_deref(), Some("acct-real"));
        assert_eq!(verified.email.as_deref(), Some("existing@example.com"));
        assert_eq!(verified.plan_name.as_deref(), Some("Plus"));
        assert!(verified.authenticated_at.is_some());
        assert_eq!(
            verified.verified_at.map(|value| value.to_rfc3339()),
            Some("2026-05-29T10:14:56.058278+00:00".to_string())
        );
        let subscription = verified.subscription.as_ref().expect("subscription");
        assert_eq!(subscription.plan_name, "Plus");
        assert_eq!(subscription.price, 20.0);
        assert_eq!(
            subscription.started_at.to_rfc3339(),
            "2026-05-29T10:12:43+00:00"
        );
        assert_eq!(
            subscription
                .current_period_ends_at
                .map(|value| value.to_rfc3339()),
            Some("2026-06-29T10:12:43+00:00".to_string())
        );
        assert_eq!(subscription.ended_at, None);
        assert_eq!(scan.events[0].provider_account_id, None);
    }

    #[test]
    fn usage_counts_support_common_shapes() {
        let value: Value = serde_json::json!({
            "inputTokens": 10,
            "outputTokens": 20,
            "cacheCreationInputTokens": 2,
            "cacheReadInputTokens": 3
        });
        let usage = claude_usage_counts_from_value(&value);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_creation_tokens, Some(2));
        assert_eq!(usage.cache_read_tokens, Some(3));
        assert_eq!(usage.computed_total(), 35);
    }

    #[test]
    fn codex_usage_counts_normalize_inclusive_subtotals() {
        let value: Value = serde_json::json!({
            "input_tokens": 100,
            "cached_input_tokens": 30,
            "output_tokens": 10,
            "reasoning_output_tokens": 5,
            "total_tokens": 110
        });

        let usage = codex_usage_counts_from_value(&value);

        assert_eq!(usage.input_tokens, Some(70));
        assert_eq!(usage.cache_read_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.reasoning_tokens, Some(5));
        assert_eq!(usage.computed_total(), 110);
    }

    #[test]
    fn codex_caps_cached_input_to_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let mut file = File::create(sessions.join("session.jsonl")).expect("fixture");
        writeln!(
            file,
            r#"{{"timestamp":"2026-05-01T00:00:00Z","usage":{{"input_tokens":10,"cached_input_tokens":30,"output_tokens":5}}}}"#
        )
        .expect("write");
        let source = SourceLocation::local_adapter(
            CODEX_PROVIDER,
            "test",
            "0",
            dir.path(),
            LocationOrigin::Configured,
        );

        let scan = scan_codex_source(&CodexAdapter, &source, &options()).expect("scan");

        assert_eq!(scan.events[0].usage.input_tokens, Some(0));
        assert_eq!(scan.events[0].usage.cache_read_tokens, Some(10));
    }
}
