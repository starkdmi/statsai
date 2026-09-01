pub(crate) use super::*;

mod jsonl;
mod models;

#[test]
fn grok_request_level_pricing_upgrade_advances_parser_revision() {
    let revision = GROK_BUILD_SCAN_CACHE_PARSER_REVISION
        .rsplit_once(".v")
        .and_then(|(_, value)| value.parse::<u32>().ok())
        .expect("Grok parser revision");

    assert!(revision > 19);
}

#[test]
fn grok_inference_sample_costs_stay_unknown_when_unpriced() {
    let observed_at = Utc::now();
    let sample = GrokInferenceSample {
        usage: UsageCounts {
            input_tokens: Some(1_000),
            output_tokens: Some(100),
            requests: Some(1),
            ..UsageCounts::default()
        },
        observed_at: Some(observed_at),
    };

    let missing_model = estimate_grok_inference_sample_costs(
        GROK_BUILD_PROVIDER,
        None,
        std::slice::from_ref(&sample),
        &[],
        &[],
        &[],
        &observed_at,
    );
    let empty = estimate_grok_inference_sample_costs(
        GROK_BUILD_PROVIDER,
        Some(&ModelInfo {
            name: Some("grok-4.6".to_string()),
            normalized_name: Some("grok-4.6".to_string()),
            provider_model_id: Some("grok-4.6".to_string()),
            speed: None,
            reasoning_level: None,
            reasoning_level_raw: None,
        }),
        &[],
        &[],
        &[],
        &[],
        &observed_at,
    );

    assert_eq!(missing_model.estimated_api_equivalent_micro_usd, None);
    assert_eq!(missing_model.estimated_api_equivalent_usd, None);
    assert_eq!(missing_model.pricing_source.as_deref(), Some("unknown"));
    assert_eq!(empty.estimated_api_equivalent_micro_usd, None);
    assert_eq!(empty.pricing_source.as_deref(), Some("unknown"));
}
