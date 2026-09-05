//! Manual import of Cursor's dashboard usage-event CSV export.
//!
//! Cursor exposes no local usage detail and no personal-account API, so the
//! only route into statsai is the dashboard's CSV export. This module parses a
//! downloaded file; it is deliberately *not* a registered [`ProviderAdapter`],
//! so nothing discovers or rescans it.
//!
//! Two properties of the export drive the design:
//!
//! 1. A row is a running meter, not a request. Cursor opens one row per
//!    (session, model) at first use and increments it until the session ends,
//!    with `Date` pinned to the start. Re-exporting mid-session returns the
//!    same row with larger token counts.
//! 2. No column combination is a unique key. Rows that share every immutable
//!    field but differ in token counts do occur (parallel calls in the same
//!    millisecond), and their order within an export is not stable.
//!
//! So identity comes from the immutable fields only, and rows sharing that
//! identity are summed into one event. Summing is order-independent and grows
//! monotonically, which is what the store's provider-record dedupe needs:
//! [`EventDeduplication::ProviderRecord`] keeps the key free of token values,
//! and the store keeps whichever snapshot is larger. Import order therefore
//! does not matter - a 7-day export and a 30-day export converge either way.

use crate::*;
use anyhow::{bail, Context, Result};
use statsai_core::{ReasoningLevel, SourceKind};
use std::collections::{BTreeMap, HashMap};

pub const CURSOR_PROVIDER: &str = "cursor";

const CURSOR_IMPORT_ADAPTER_ID: &str = "cursor-usage-events-csv";
const CURSOR_EVENT_KIND: &str = "cursor_usage_event";
const CURSOR_SOURCE_TYPE: &str = "cursor_usage_events_csv";

/// Stable across files and paths, so the same logical row imported from two
/// different exports resolves to the same source and therefore the same event.
const CURSOR_SOURCE_EVIDENCE_KEY: &str = "cursor-usage-export";

/// Result of parsing one exported CSV file.
#[derive(Debug, Clone)]
pub struct CursorImportReport {
    pub path: PathBuf,
    pub source: SourceLocation,
    pub events: Vec<UsageEvent>,
    pub warnings: Vec<String>,
    pub rows_read: u64,
    pub rows_skipped: u64,
    pub range: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Satisfies [`usage_event`], which needs only `provider`/`id`/`version`.
///
/// Never registered in `default_adapters()` or `adapter_for_provider()`: there
/// is nothing on disk to discover, and the CSV arrives by explicit import.
#[derive(Debug, Default)]
struct CursorCsvImport;

impl ProviderAdapter for CursorCsvImport {
    fn id(&self) -> &'static str {
        CURSOR_IMPORT_ADAPTER_ID
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn provider(&self) -> &'static str {
        CURSOR_PROVIDER
    }

    fn discover(&self) -> Vec<SourceLocation> {
        Vec::new()
    }

    fn scan_candidates(&self, _source: &SourceLocation) -> Result<Vec<ScanCandidateFile>> {
        Ok(Vec::new())
    }

    fn scan(&self, _source: &SourceLocation, _options: &ScanOptions) -> Result<AdapterScan> {
        Ok(AdapterScan::default())
    }
}

/// The one source every Cursor import writes to.
#[must_use]
pub fn cursor_import_source() -> SourceLocation {
    SourceLocation::reported_usage(
        CURSOR_PROVIDER,
        SourceKind::Manual,
        CURSOR_IMPORT_ADAPTER_ID,
        env!("CARGO_PKG_VERSION"),
        CURSOR_SOURCE_EVIDENCE_KEY,
        None,
    )
}

/// Immutable fields of a row. Rows sharing this are summed into one event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct RowIdentity {
    date: String,
    cloud_agent_id: String,
    automation_id: String,
    kind: String,
    model: String,
    max_mode: String,
}

impl RowIdentity {
    fn provider_record_id(&self) -> String {
        format!(
            "{CURSOR_EVENT_KIND}:{}|{}|{}|{}|{}|{}",
            self.date,
            self.cloud_agent_id,
            self.automation_id,
            self.kind,
            self.model,
            self.max_mode
        )
    }
}

#[derive(Debug, Default, Clone)]
struct RowTotals {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
    total: u64,
    /// Summed provider-reported charge, in micro-USD, when Cursor reports one.
    charged_micro_usd: Option<i64>,
    first_line: u64,
}

/// Column indices, resolved by header name so column order and optional
/// columns can change without breaking the parser.
struct Columns {
    date: usize,
    cloud_agent_id: Option<usize>,
    automation_id: Option<usize>,
    kind: Option<usize>,
    model: usize,
    max_mode: Option<usize>,
    input_with_cache_write: Option<usize>,
    input_without_cache_write: Option<usize>,
    cache_read: Option<usize>,
    output_tokens: Option<usize>,
    total_tokens: usize,
    cost: Option<usize>,
}

impl Columns {
    fn from_header(header: &csv::StringRecord) -> Result<Self> {
        let index: HashMap<&str, usize> = header
            .iter()
            .enumerate()
            .map(|(position, name)| (name.trim(), position))
            .collect();
        let required = |name: &str| -> Result<usize> {
            index
                .get(name)
                .copied()
                .with_context(|| format!("Cursor usage CSV is missing the {name:?} column"))
        };
        Ok(Self {
            date: required("Date")?,
            cloud_agent_id: index.get("Cloud Agent ID").copied(),
            automation_id: index.get("Automation ID").copied(),
            kind: index.get("Kind").copied(),
            model: required("Model")?,
            max_mode: index.get("Max Mode").copied(),
            input_with_cache_write: index.get("Input (w/ Cache Write)").copied(),
            input_without_cache_write: index.get("Input (w/o Cache Write)").copied(),
            cache_read: index.get("Cache Read").copied(),
            output_tokens: index.get("Output Tokens").copied(),
            total_tokens: required("Total Tokens")?,
            cost: index.get("Cost").copied(),
        })
    }

    fn field<'a>(&self, record: &'a csv::StringRecord, position: usize) -> &'a str {
        record.get(position).unwrap_or_default().trim()
    }

    fn optional<'a>(&self, record: &'a csv::StringRecord, position: Option<usize>) -> &'a str {
        position
            .and_then(|position| record.get(position))
            .unwrap_or_default()
            .trim()
    }
}

/// Parses a Cursor usage-events CSV into events ready for the store.
pub fn parse_cursor_usage_csv(path: &Path, options: &ScanOptions) -> Result<CursorImportReport> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("reading Cursor usage CSV at {}", path.display()))?;
    let header = reader
        .headers()
        .with_context(|| format!("reading header of {}", path.display()))?
        .clone();
    let columns = Columns::from_header(&header)?;

    let mut warnings = Vec::new();
    let mut grouped: BTreeMap<RowIdentity, RowTotals> = BTreeMap::new();
    let mut rows_read = 0u64;
    let mut rows_skipped = 0u64;

    for (offset, record) in reader.records().enumerate() {
        // Header is line 1, so the first data row is line 2.
        let line = offset as u64 + 2;
        let record = match record {
            Ok(record) => record,
            Err(error) => {
                rows_skipped += 1;
                warnings.push(format!("line {line}: unreadable row ({error})"));
                continue;
            }
        };
        rows_read += 1;

        let date_raw = columns.field(&record, columns.date);
        let Some(date) = parse_cursor_timestamp(date_raw) else {
            rows_skipped += 1;
            warnings.push(format!("line {line}: unparseable date {date_raw:?}"));
            continue;
        };
        let model = columns.field(&record, columns.model);
        if model.is_empty() {
            rows_skipped += 1;
            warnings.push(format!("line {line}: missing model"));
            continue;
        }

        let counts = [
            columns.optional(&record, columns.input_without_cache_write),
            columns.optional(&record, columns.input_with_cache_write),
            columns.optional(&record, columns.cache_read),
            columns.optional(&record, columns.output_tokens),
            columns.field(&record, columns.total_tokens),
        ];
        let Some([input, cache_creation, cache_read, output, total]) = parse_token_counts(&counts)
        else {
            // Real exports contain a small number of rows with blank numeric
            // cells. They carry no usage, so dropping them loses nothing.
            rows_skipped += 1;
            warnings.push(format!("line {line}: unreadable token counts"));
            continue;
        };

        let identity = RowIdentity {
            date: date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cloud_agent_id: columns
                .optional(&record, columns.cloud_agent_id)
                .to_string(),
            automation_id: columns.optional(&record, columns.automation_id).to_string(),
            kind: columns.optional(&record, columns.kind).to_string(),
            model: model.to_string(),
            max_mode: columns.optional(&record, columns.max_mode).to_string(),
        };
        let charged = parse_cursor_cost(columns.optional(&record, columns.cost));

        let totals = grouped.entry(identity).or_insert(RowTotals {
            first_line: line,
            ..RowTotals::default()
        });
        totals.input = totals.input.saturating_add(input);
        totals.cache_creation = totals.cache_creation.saturating_add(cache_creation);
        totals.cache_read = totals.cache_read.saturating_add(cache_read);
        totals.output = totals.output.saturating_add(output);
        totals.total = totals.total.saturating_add(total);
        if let Some(charged) = charged {
            totals.charged_micro_usd = Some(
                totals
                    .charged_micro_usd
                    .unwrap_or(0)
                    .saturating_add(charged),
            );
        }
    }

    let adapter = CursorCsvImport;
    let source = cursor_import_source();
    let mut events = Vec::with_capacity(grouped.len());
    let mut range: Option<(DateTime<Utc>, DateTime<Utc>)> = None;

    for (identity, totals) in grouped {
        let Some(timestamp) = parse_cursor_timestamp(&identity.date) else {
            continue;
        };
        range = Some(match range {
            Some((start, end)) => (start.min(timestamp), end.max(timestamp)),
            None => (timestamp, timestamp),
        });

        let model = cursor_model_info(&identity.model);
        let usage = UsageCounts {
            input_tokens: Some(totals.input),
            cache_creation_tokens: Some(totals.cache_creation),
            cache_read_tokens: Some(totals.cache_read),
            output_tokens: Some(totals.output),
            total_tokens: Some(totals.total),
            ..UsageCounts::default()
        };
        // A cloud agent groups every row it produced; a local request stands
        // alone, and Cursor gives it no identifier to group by.
        let session_raw = if identity.cloud_agent_id.is_empty() {
            identity.provider_record_id()
        } else {
            format!("cursor_agent:{}", identity.cloud_agent_id)
        };

        let mut event = usage_event(
            &adapter,
            &source,
            options,
            ProviderEventParts {
                timestamp,
                session_started_at: None,
                session_ended_at: None,
                duration_seconds: None,
                model: Some(model),
                usage,
                runtime: None,
                session_raw,
                project: None,
                event_kind: CURSOR_EVENT_KIND,
                source_file: path,
                source_line_number: Some(totals.first_line as usize),
                source_type: CURSOR_SOURCE_TYPE,
                model_inferred: false,
                timestamp_inferred: false,
                deduplication: EventDeduplication::ProviderRecord(identity.provider_record_id()),
                dedupe_salt: None,
            },
        );

        // Included and free rows report no price. A usage-based row past the
        // monthly quota does, and that is a real charge rather than an
        // estimate, so it is recorded as provider-reported.
        if let Some(charged_micro_usd) = totals.charged_micro_usd {
            event
                .cost
                .set_provider_reported_micro_usd(charged_micro_usd);
            event.cost.pricing_source = Some("cursor.usage_event.cost".to_string());
            event.cost.confidence = Confidence::High;
        }

        events.push(event);
    }

    Ok(CursorImportReport {
        path: path.to_path_buf(),
        source,
        events,
        warnings,
        rows_read,
        rows_skipped,
        range,
    })
}

/// Sums tokens per model for events that carry no cost.
///
/// Cursor bills several models it does not publish a price for, so a run's
/// token coverage is worth surfacing. Callers pass the events they will
/// actually persist: computing this per file would double-count any row that
/// appears in two overlapping exports.
#[must_use]
pub fn unpriced_model_tokens(events: &[UsageEvent]) -> BTreeMap<String, u64> {
    let mut unpriced: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        if event.cost.estimated_api_equivalent_micro_usd.is_some()
            || event.cost.provider_reported_micro_usd.is_some()
        {
            continue;
        }
        let model = event
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref())
            .unwrap_or("unknown");
        *unpriced.entry(model.to_string()).or_default() += event.usage.computed_total();
    }
    unpriced
}

/// Lists the CSV exports a `--path` argument refers to.
///
/// A file is taken as-is; a directory contributes its `.csv` children, sorted
/// so output is deterministic. The extension is the only filter: pointing at a
/// directory is an explicit instruction, so a renamed export should still
/// import, and nothing here ever reads a directory the user did not name.
pub fn cursor_csv_paths(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        bail!("no such file or directory: {}", path.display());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("reading directory {}", path.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && has_csv_extension(path))
        .collect();
    paths.sort();
    if paths.is_empty() {
        bail!("no .csv exports found in {}", path.display());
    }
    Ok(paths)
}

fn has_csv_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csv"))
}

fn cursor_model_info(raw: &str) -> ModelInfo {
    let mut model = model_info(raw);
    let lower = raw.to_ascii_lowercase();
    if lower.ends_with("-fast") {
        model.speed = Some("fast".to_string());
    }
    if let Some(reasoning) = cursor_reasoning_level(&lower) {
        model.reasoning_level = ReasoningLevel::parse(reasoning);
        model.reasoning_level_raw = Some(reasoning.to_string());
    }
    model
}

/// Reads Cursor's reasoning-effort suffix, e.g. `cursor-grok-4.6-high-fast`.
fn cursor_reasoning_level(lower: &str) -> Option<&'static str> {
    let stem = lower.strip_suffix("-fast").unwrap_or(lower);
    for level in ["ultracode", "max", "xhigh", "high", "medium", "low"] {
        if stem.ends_with(&format!("-{level}")) {
            return Some(match level {
                "ultracode" => "ultracode",
                "max" => "max",
                "xhigh" => "xhigh",
                "high" => "high",
                "medium" => "medium",
                _ => "low",
            });
        }
    }
    None
}

fn parse_cursor_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn parse_token_counts(raw: &[&str; 5]) -> Option<[u64; 5]> {
    let mut parsed = [0u64; 5];
    for (slot, value) in parsed.iter_mut().zip(raw.iter()) {
        let cleaned = value.trim().replace(',', "");
        if cleaned.is_empty() {
            return None;
        }
        *slot = cleaned.parse::<u64>().ok()?;
    }
    Some(parsed)
}

/// Reads the `Cost` column as a charge, in micro-USD.
///
/// `Included` and `Free` are labels rather than prices, and an unrecognised
/// value is treated the same way: a new Cursor vocabulary must not fail an
/// import.
fn parse_cursor_cost(raw: &str) -> Option<i64> {
    let cleaned = raw
        .trim()
        .trim_start_matches('$')
        .replace(['$', ',', '"'], "");
    if cleaned.is_empty() {
        return None;
    }
    let amount = cleaned.parse::<f64>().ok()?;
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }
    usd_to_micro_usd(amount)
}

#[cfg(test)]
mod tests;
