//! Public Rust SDK facade for `ai-stats`.
//!
//! This crate intentionally re-exports the stable surface from the internal
//! crates so embedders can depend on one package while the workspace matures.

pub use ai_stats_adapters as adapters;
pub use ai_stats_core as core;
pub use ai_stats_store as store;
pub use ai_stats_sync as sync;

use ai_stats_core::{
    canonical_display, hash_text, provider_account_id, summary_id, Confidence, CostInfo,
    EventSource, IdentitySource, LocationOrigin, ModelInfo, ParseEvidence, PrivacyInfo,
    PrivacyMode, SourceKind, SourceLocation, SummaryMetadata, UsageCounts, UsageSummary,
    REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION, USAGE_SUMMARY_SCHEMA_VERSION,
};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REPORTED_USAGE_IMPORT_ADAPTER_ID: &str = "reported-usage-summary";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportedUsageSummaryInput {
    pub schema_version: String,
    pub provider: String,
    pub account_hint: Option<String>,
    pub source_kind: SourceKind,
    pub source_name: String,
    pub evidence_id: Option<String>,
    pub evidence_path: Option<String>,
    pub report_format: String,
    pub report_version: Option<String>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
    pub model: Option<ModelInfo>,
    pub usage: UsageCounts,
    pub cost: Option<CostInfo>,
    pub confidence: Option<Confidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportedUsageSummaryRecord {
    pub source: SourceLocation,
    pub summary: UsageSummary,
}

pub fn build_reported_usage_summary(
    input: ReportedUsageSummaryInput,
    device_id: &str,
) -> Result<ReportedUsageSummaryRecord> {
    if input.schema_version != REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION {
        bail!(
            "unsupported reported usage input schema {}",
            input.schema_version
        );
    }
    if !matches!(
        input.source_kind,
        SourceKind::ExternalReport | SourceKind::Manual
    ) {
        bail!("reported usage source_kind must be external_report or manual");
    }
    if input.usage.computed_total() == 0 {
        bail!("reported usage summary has zero total tokens");
    }

    let evidence_key = input
        .evidence_path
        .as_deref()
        .or(input.evidence_id.as_deref())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}:{}:{}:{}",
                input.provider,
                input.source_name,
                input.account_hint.as_deref().unwrap_or("unmapped"),
                input.report_format
            )
        });
    let path_label = input.evidence_path.clone();
    let source = SourceLocation::reported_usage(
        input.provider.clone(),
        input.source_kind.clone(),
        REPORTED_USAGE_IMPORT_ADAPTER_ID,
        env!("CARGO_PKG_VERSION"),
        &evidence_key,
        path_label.clone(),
        input.account_hint.clone(),
    );

    let period_start_text = input
        .period_start
        .as_ref()
        .map(DateTime::to_rfc3339)
        .unwrap_or_else(|| "unknown_start".to_string());
    let period_end_text = input
        .period_end
        .as_ref()
        .map(DateTime::to_rfc3339)
        .unwrap_or_else(|| "unknown_end".to_string());
    let cost_key = input
        .cost
        .as_ref()
        .and_then(|cost| {
            cost.provider_reported_usd
                .or(cost.estimated_api_equivalent_usd)
        })
        .map(|cost| format!("{cost:.4}"))
        .unwrap_or_else(|| "unknown_cost".to_string());
    let semantic_key = format!(
        "reported_summary.v1:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        source.source_id.0,
        input.report_format,
        period_start_text,
        period_end_text,
        input.usage.input_tokens.unwrap_or(0),
        input.usage.cache_creation_tokens.unwrap_or(0),
        input.usage.cache_read_tokens.unwrap_or(0),
        input.usage.output_tokens.unwrap_or(0),
        input
            .usage
            .total_tokens
            .unwrap_or_else(|| input.usage.computed_total()),
        cost_key,
    );
    let imported_at = Utc::now();
    let observed_at = input
        .observed_at
        .or(input.period_end)
        .unwrap_or(imported_at);
    let source_file_path_hash = input
        .evidence_path
        .as_deref()
        .map(|path| hash_text(&canonical_display(Path::new(path))));
    let parse_confidence = input.confidence.clone().unwrap_or(Confidence::Medium);
    let cost = input.cost.unwrap_or(CostInfo {
        currency: "USD".to_string(),
        estimated_api_equivalent_usd: None,
        provider_reported_usd: None,
        pricing_source: Some("manual".to_string()),
        pricing_version: None,
        confidence: Confidence::Low,
    });

    let summary = UsageSummary {
        schema_version: USAGE_SUMMARY_SCHEMA_VERSION.to_string(),
        summary_id: summary_id(&input.provider, &source.source_id, &semantic_key),
        device_id: device_id.to_string(),
        provider: input.provider.clone(),
        source_id: source.source_id.clone(),
        provider_account_id: input
            .account_hint
            .as_deref()
            .map(|account| provider_account_id(&input.provider, account)),
        source: EventSource {
            adapter_id: REPORTED_USAGE_IMPORT_ADAPTER_ID.to_string(),
            adapter_version: env!("CARGO_PKG_VERSION").to_string(),
            source_kind: input.source_kind.clone(),
            location_origin: Some(LocationOrigin::Configured),
            source_type: input.report_format.clone(),
            source_path_hash: source.path_hash.clone(),
            source_record_id: Some(
                input
                    .evidence_id
                    .clone()
                    .unwrap_or_else(|| format!("summary_key_{}", &hash_text(&semantic_key)[..32])),
            ),
            parse_confidence,
        },
        model: input.model,
        usage: input.usage,
        cost,
        parse_evidence: Some(ParseEvidence {
            event_key_version: "reported_usage_summary.v1".to_string(),
            source_file_path_hash,
            source_line_number: None,
            source_record_id: Some(semantic_key),
            model_inferred: false,
            timestamp_inferred: input.period_start.is_none() || input.period_end.is_none(),
            account_identity_source: if input.account_hint.is_some() {
                IdentitySource::ManualHint
            } else {
                IdentitySource::Unresolved
            },
        }),
        privacy: PrivacyInfo {
            mode: PrivacyMode::MetadataOnly,
            contains_prompt_text: false,
            contains_response_text: false,
            contains_file_paths: false,
        },
        period_start: input.period_start,
        period_end: input.period_end,
        observed_at,
        metadata: SummaryMetadata {
            summary_format: input.report_format,
            summary_version: input.report_version,
            total_sessions: None,
            total_messages: None,
            last_computed_at: None,
        },
        imported_at,
    };

    Ok(ReportedUsageSummaryRecord { source, summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_stats_core::REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION;
    use chrono::TimeZone;

    #[test]
    fn builds_manual_reported_summary_with_stable_ids() {
        let input = ReportedUsageSummaryInput {
            schema_version: REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION.to_string(),
            provider: "claude_code".to_string(),
            account_hint: Some("personal".to_string()),
            source_kind: SourceKind::Manual,
            source_name: "user_reported_usage".to_string(),
            evidence_id: Some("screenshot:2025-07-11".to_string()),
            evidence_path: Some("/tmp/ccusage.png".to_string()),
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
                input_tokens: Some(10),
                cache_creation_tokens: Some(20),
                cache_read_tokens: Some(30),
                output_tokens: Some(40),
                total_tokens: Some(100),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: Some(Confidence::Medium),
        };

        let first = build_reported_usage_summary(input.clone(), "device").expect("first");
        let second = build_reported_usage_summary(input, "device").expect("second");

        assert_eq!(first.source.source_kind, SourceKind::Manual);
        assert_eq!(first.source.source_id, second.source.source_id);
        assert_eq!(first.summary.summary_id, second.summary.summary_id);
        assert_eq!(
            first.summary.provider_account_id,
            Some(provider_account_id("claude_code", "personal"))
        );
        assert_eq!(first.summary.metadata.summary_format, "manual_daily");
        assert_eq!(first.summary.usage.computed_total(), 100);
    }

    #[test]
    fn rejects_non_reported_source_kind() {
        let input = ReportedUsageSummaryInput {
            schema_version: REPORTED_USAGE_SUMMARY_INPUT_SCHEMA_VERSION.to_string(),
            provider: "claude_code".to_string(),
            account_hint: None,
            source_kind: SourceKind::LocalAdapter,
            source_name: "bad".to_string(),
            evidence_id: None,
            evidence_path: None,
            report_format: "manual".to_string(),
            report_version: None,
            period_start: None,
            period_end: None,
            observed_at: None,
            model: None,
            usage: UsageCounts {
                total_tokens: Some(1),
                ..UsageCounts::default()
            },
            cost: None,
            confidence: None,
        };

        assert!(build_reported_usage_summary(input, "device").is_err());
    }
}
