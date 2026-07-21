use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{DetectedSpan, DetectionConfidence, DetectorKind, PrivacyCategory, PrivacyError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivacyReplacement {
    pub start: usize,
    pub end: usize,
    pub category: PrivacyCategory,
    pub detector: DetectorKind,
    pub confidence: Option<DetectionConfidence>,
    pub replacement: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilteredText {
    pub text: String,
    pub replacements: Vec<PrivacyReplacement>,
}

pub fn filter_text(
    text: &str,
    mut spans: Vec<DetectedSpan>,
    mut alias: impl FnMut(PrivacyCategory, &str) -> Result<u64, PrivacyError>,
) -> Result<FilteredText, PrivacyError> {
    for span in &spans {
        span.validate_for(text)?;
    }
    spans.sort_by_key(|span| (span.start, span.end, std::cmp::Reverse(span_priority(span))));
    let merged = merge_overlapping(spans);
    let mut output = String::with_capacity(text.len());
    let mut replacements = Vec::with_capacity(merged.len());
    let mut cursor = 0;
    for span in merged {
        output.push_str(&text[cursor..span.start]);
        let replacement = if span.category == PrivacyCategory::Secret {
            "[SECRET]".to_string()
        } else {
            let number = alias(span.category, &text[span.start..span.end])?;
            format!("[{}_{number:06}]", category_name(span.category))
        };
        output.push_str(&replacement);
        replacements.push(PrivacyReplacement {
            start: span.start,
            end: span.end,
            category: span.category,
            detector: span.detector,
            confidence: span.confidence,
            replacement,
        });
        cursor = span.end;
    }
    output.push_str(&text[cursor..]);
    Ok(FilteredText {
        text: output,
        replacements,
    })
}

fn merge_overlapping(spans: Vec<DetectedSpan>) -> Vec<DetectedSpan> {
    let mut merged: Vec<DetectedSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        let Some(previous) = merged.last_mut() else {
            merged.push(span);
            continue;
        };
        if span.start >= previous.end {
            merged.push(span);
            continue;
        }
        previous.end = previous.end.max(span.end);
        if span_priority(&span) > span_priority(previous) {
            previous.category = span.category;
            previous.detector = span.detector;
            previous.confidence = span.confidence;
        }
    }
    merged
}

fn span_priority(span: &DetectedSpan) -> (u8, u8) {
    let category = match span.category {
        PrivacyCategory::Secret => 3,
        PrivacyCategory::Path
        | PrivacyCategory::Host
        | PrivacyCategory::IpAddress
        | PrivacyCategory::Project
        | PrivacyCategory::Repository
        | PrivacyCategory::Branch
        | PrivacyCategory::ToolCallId => 2,
        _ => 1,
    };
    let detector = match span.detector {
        DetectorKind::Kingfisher => 3,
        DetectorKind::Structured => 2,
        DetectorKind::OpenAiPrivacyFilter => 1,
    };
    (category, detector)
}

#[must_use]
pub fn normalize_private_value(category: PrivacyCategory, value: &str) -> String {
    if category == PrivacyCategory::ToolCallId {
        return value.to_string();
    }
    let normalized = value.nfkc().collect::<String>();
    let folded = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    match category {
        PrivacyCategory::Email | PrivacyCategory::Host => folded.to_lowercase(),
        PrivacyCategory::Phone => folded.chars().filter(char::is_ascii_digit).collect(),
        PrivacyCategory::IpAddress => folded
            .parse::<IpAddr>()
            .map_or_else(|_| folded.to_lowercase(), |address| address.to_string()),
        PrivacyCategory::Path => folded.replace('\\', "/"),
        _ => folded,
    }
}

const fn category_name(category: PrivacyCategory) -> &'static str {
    match category {
        PrivacyCategory::AccountNumber => "ACCOUNT",
        PrivacyCategory::Address => "ADDRESS",
        PrivacyCategory::Date => "DATE",
        PrivacyCategory::Email => "EMAIL",
        PrivacyCategory::Person => "PERSON",
        PrivacyCategory::Phone => "PHONE",
        PrivacyCategory::Url => "URL",
        PrivacyCategory::Secret => "SECRET",
        PrivacyCategory::Path => "PATH",
        PrivacyCategory::Host => "HOST",
        PrivacyCategory::IpAddress => "IP",
        PrivacyCategory::Project => "PROJECT",
        PrivacyCategory::Repository => "REPOSITORY",
        PrivacyCategory::Branch => "BRANCH",
        PrivacyCategory::ToolCallId => "TOOL_CALL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_merges_overlaps_with_secret_precedence() {
        let text = "token=user@example.com";
        let spans = vec![
            DetectedSpan {
                start: 6,
                end: text.len(),
                category: PrivacyCategory::Email,
                detector: DetectorKind::OpenAiPrivacyFilter,
                confidence: None,
            },
            DetectedSpan {
                start: 0,
                end: text.len(),
                category: PrivacyCategory::Secret,
                detector: DetectorKind::Kingfisher,
                confidence: None,
            },
        ];

        let filtered = filter_text(text, spans, |_, _| Ok(1)).expect("filter text");

        assert_eq!(filtered.text, "[SECRET]");
        assert_eq!(
            filtered.replacements,
            vec![PrivacyReplacement {
                start: 0,
                end: text.len(),
                category: PrivacyCategory::Secret,
                detector: DetectorKind::Kingfisher,
                confidence: None,
                replacement: "[SECRET]".to_string(),
            }]
        );
    }

    #[test]
    fn normalization_is_category_specific_and_unicode_stable() {
        assert_eq!(
            normalize_private_value(PrivacyCategory::Email, "  USER@Example.COM  "),
            "user@example.com"
        );
        assert_eq!(
            normalize_private_value(PrivacyCategory::Phone, "+1 (415) 555-0199"),
            "14155550199"
        );
        assert_eq!(
            normalize_private_value(PrivacyCategory::Person, "Ａｌｉｃｅ"),
            "Alice"
        );
        assert_eq!(
            normalize_private_value(PrivacyCategory::ToolCallId, "call  Ａ"),
            "call  Ａ"
        );
    }
}
