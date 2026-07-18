use std::net::IpAddr;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use url::{form_urlencoded, Url};

use crate::{
    DetectedSpan, DetectionConfidence, DetectorKind, DetectorMetadata, PrivacyCategory,
    PrivacyDetector, PrivacyError,
};

const DETERMINISTIC_VERSION: &str = "structural-v5";

static WINDOWS_HOME_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b[A-Z]:\\Users\\[^\s\\,;]+(?:\\[^\s\]\[(){}<>"',;]+)*"#)
        .expect("valid Windows path regex")
});
static URI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?|postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis)://[^\s<>"`]+"#)
        .expect("valid URI regex")
});
static IP_CANDIDATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\b(?:\d{1,3}\.){3}\d{1,3}\b|\b[0-9a-f]{0,4}:[0-9a-f:]+\b)")
        .expect("valid IP candidate regex")
});
static PRIVATE_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[a-z0-9][a-z0-9.-]*\.(?:local|internal|lan)\b")
        .expect("valid private host regex")
});
static PROVIDER_CALL_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:call-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}(?:-[0-9]+)?|call_[a-z0-9]{8,}|toolu_[a-z0-9]{8,})\b",
    )
    .expect("valid provider call ID regex")
});

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownPrivateValue {
    pub category: PrivacyCategory,
    pub value: String,
}

#[derive(Clone, Debug, Default)]
pub struct DeterministicDetector {
    known_values: Vec<KnownPrivateValue>,
}

impl DeterministicDetector {
    #[must_use]
    pub fn policy_metadata() -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::Deterministic,
            implementation_version: DETERMINISTIC_VERSION.to_string(),
            model_revision: None,
            offline: true,
        }
    }

    #[must_use]
    pub fn new(mut known_values: Vec<KnownPrivateValue>) -> Self {
        let repository_owners = known_values
            .iter()
            .filter(|known| known.category == PrivacyCategory::Repository)
            .filter_map(|known| repository_owner(&known.value))
            .filter(|owner| {
                !known_values.iter().any(|known| {
                    known.category == PrivacyCategory::Repository && known.value == *owner
                })
            })
            .map(|owner| KnownPrivateValue {
                category: PrivacyCategory::Repository,
                value: owner.to_string(),
            })
            .collect::<Vec<_>>();
        known_values.extend(repository_owners);
        Self { known_values }
    }
}

impl PrivacyDetector for DeterministicDetector {
    fn metadata(&self) -> DetectorMetadata {
        Self::policy_metadata()
    }

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        Ok(texts
            .iter()
            .map(|text| self.detect_structural(text))
            .collect())
    }
}

impl DeterministicDetector {
    fn detect_structural(&self, text: &str) -> Vec<DetectedSpan> {
        let mut spans = Vec::new();
        for known in &self.known_values {
            if !known_value_is_specific(known) {
                continue;
            }
            for (start, _) in text.match_indices(&known.value) {
                let end = start + known.value.len();
                if !known_match_has_boundaries(text, start, end, known.category) {
                    continue;
                }
                spans.push(deterministic_span(start, end, known.category));
            }
        }
        for (start, end) in unix_absolute_path_spans(text) {
            spans.push(deterministic_span(start, end, PrivacyCategory::Path));
        }
        for found in WINDOWS_HOME_PATH.find_iter(text) {
            spans.push(deterministic_span(
                found.start(),
                found.end(),
                PrivacyCategory::Path,
            ));
        }
        for found in URI.find_iter(text) {
            if uri_contains_credentials(found.as_str()) {
                spans.push(deterministic_span(
                    found.start(),
                    found.end(),
                    PrivacyCategory::Secret,
                ));
            }
        }
        for found in IP_CANDIDATE.find_iter(text) {
            let Ok(address) = found.as_str().parse::<IpAddr>() else {
                continue;
            };
            if !address.is_loopback() && !address.is_unspecified() {
                spans.push(deterministic_span(
                    found.start(),
                    found.end(),
                    PrivacyCategory::IpAddress,
                ));
            }
        }
        for found in PRIVATE_HOST.find_iter(text) {
            spans.push(deterministic_span(
                found.start(),
                found.end(),
                PrivacyCategory::Host,
            ));
        }
        for found in PROVIDER_CALL_ID.find_iter(text) {
            spans.push(deterministic_span(
                found.start(),
                found.end(),
                PrivacyCategory::Secret,
            ));
        }
        spans.sort_by_key(|span| (span.start, span.end, span.category));
        spans.dedup_by_key(|span| (span.start, span.end, span.category));
        spans
    }
}

fn repository_owner(value: &str) -> Option<&str> {
    let mut segments = value.trim().trim_matches('/').split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    if owner.is_empty()
        || repository.is_empty()
        || segments.next().is_some()
        || owner.contains(':')
        || owner.contains('@')
    {
        return None;
    }
    Some(owner)
}

fn uri_contains_credentials(value: &str) -> bool {
    let Ok(uri) = Url::parse(value) else {
        return false;
    };
    if !uri.username().is_empty() || uri.password().is_some() {
        return true;
    }
    if uri
        .query_pairs()
        .any(|(name, value)| credential_parameter(&name, &value))
    {
        return true;
    }
    uri.fragment().is_some_and(|fragment| {
        let parameters = fragment
            .rsplit_once('?')
            .map_or(fragment, |(_, parameters)| parameters);
        form_urlencoded::parse(parameters.as_bytes())
            .any(|(name, value)| credential_parameter(&name, &value))
    })
}

fn credential_parameter(name: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let name = name.trim().to_ascii_lowercase().replace('-', "_");
    matches!(
        name.as_str(),
        "access_token"
            | "api_key"
            | "apikey"
            | "auth"
            | "authorization_code"
            | "client_secret"
            | "code"
            | "credential"
            | "id_token"
            | "jwt"
            | "passwd"
            | "password"
            | "pwd"
            | "refresh_token"
            | "secret"
            | "session_token"
            | "sig"
            | "signature"
            | "token"
            | "x_amz_credential"
            | "x_amz_security_token"
            | "x_amz_signature"
    )
}

fn known_value_is_specific(known: &KnownPrivateValue) -> bool {
    let value = known.value.trim();
    if value.is_empty() {
        return false;
    }
    match known.category {
        PrivacyCategory::Path | PrivacyCategory::Project | PrivacyCategory::Repository => true,
        PrivacyCategory::Branch => !is_generic_known_value(value),
        _ => value.chars().count() >= 4 && !is_generic_known_value(value),
    }
}

fn is_generic_known_value(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "main" | "master" | "develop" | "development" | "test" | "testing" | "production"
    )
}

fn unix_absolute_path_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (start, character) in text.char_indices() {
        if character != '/' || !unix_path_has_start_boundary(text, start) {
            continue;
        }
        let remainder = &text[start + 1..];
        if remainder.is_empty() || remainder.starts_with('/') {
            continue;
        }
        let quote = text[..start]
            .chars()
            .next_back()
            .filter(|character| matches!(character, '\'' | '"' | '`'));
        let Some(end) = unix_path_end(text, start, quote) else {
            continue;
        };
        spans.push((start, end));
    }
    spans
}

fn unix_path_has_start_boundary(text: &str, start: usize) -> bool {
    let prefix = &text[..start];
    prefix.ends_with("file://")
        || prefix.chars().next_back().is_none_or(|character| {
            character.is_whitespace()
                || matches!(character, '\'' | '"' | '`' | '(' | '[' | '{' | '=' | ':')
        })
}

fn unix_path_end(text: &str, start: usize, quote: Option<char>) -> Option<usize> {
    let mut end = start + 1;
    for (offset, character) in text[start + 1..].char_indices() {
        let position = start + 1 + offset;
        if quote == Some(character) || unix_path_hard_delimiter(character, quote.is_some()) {
            break;
        }
        if character.is_whitespace()
            && (character != ' '
                || (quote.is_none() && !unix_path_has_later_separator(&text[position + 1..])))
        {
            break;
        }
        end = position + character.len_utf8();
    }
    (end > start + 1).then_some(end)
}

fn unix_path_has_later_separator(text: &str) -> bool {
    for character in text.chars() {
        if unix_path_hard_delimiter(character, false) || matches!(character, '\n' | '\r' | '\t') {
            return false;
        }
        if character == '/' {
            return true;
        }
    }
    false
}

fn unix_path_hard_delimiter(character: char, quoted: bool) -> bool {
    if matches!(character, '\n' | '\r' | '\t') {
        return true;
    }
    if quoted {
        return false;
    }
    matches!(
        character,
        '\'' | '"' | '`' | '[' | ']' | '(' | ')' | '{' | '}' | '<' | '>' | ',' | ';'
    )
}

fn known_match_has_boundaries(
    text: &str,
    start: usize,
    end: usize,
    category: PrivacyCategory,
) -> bool {
    if category == PrivacyCategory::Path {
        return true;
    }
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|character| !character.is_alphanumeric())
        && after.is_none_or(|character| !character.is_alphanumeric())
}

fn deterministic_span(start: usize, end: usize, category: PrivacyCategory) -> DetectedSpan {
    DetectedSpan {
        start,
        end,
        category,
        detector: DetectorKind::Deterministic,
        confidence: Some(DetectionConfidence::High),
    }
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
        | PrivacyCategory::Branch => 2,
        _ => 1,
    };
    let detector = match span.detector {
        DetectorKind::Kingfisher => 3,
        DetectorKind::Deterministic => 2,
        DetectorKind::OpenAiPrivacyFilter => 1,
    };
    (category, detector)
}

#[must_use]
pub fn normalize_private_value(category: PrivacyCategory, value: &str) -> String {
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
    fn deterministic_detection_finds_paths_and_ips_but_not_bare_uuids() {
        let text = "open /Users/alice/private/file.txt from 10.0.0.8 id 123e4567-e89b-12d3-a456-426614174000";
        let mut detector = DeterministicDetector::default();
        let findings = detector.detect(text).expect("detect");

        assert!(findings
            .iter()
            .any(|finding| finding.category == PrivacyCategory::Path));
        assert!(findings
            .iter()
            .any(|finding| finding.category == PrivacyCategory::IpAddress));
        assert!(findings.iter().all(|finding| {
            &text[finding.start..finding.end] != "123e4567-e89b-12d3-a456-426614174000"
        }));
    }

    #[test]
    fn deterministic_detection_derives_repository_owner_aliases() {
        let text = "related starkdmi/statsai repository, not starkdmitest/example";
        let mut detector = DeterministicDetector::new(vec![KnownPrivateValue {
            category: PrivacyCategory::Repository,
            value: "starkdmi/statsai-api".to_string(),
        }]);

        let findings = detector.detect(text).expect("detect repository owner");
        let values = findings
            .iter()
            .map(|finding| &text[finding.start..finding.end])
            .collect::<Vec<_>>();

        assert!(values.contains(&"starkdmi"));
        assert!(!values.contains(&"starkdmitest"));
    }

    #[test]
    fn deterministic_detection_removes_provider_call_ids_but_keeps_bare_uuids() {
        let call_id = "call-47adef28-2702-46fa-bb37-e71d87169a58-2";
        let bare_uuid = "123e4567-e89b-12d3-a456-426614174000";
        let text = format!("tool_call_id={call_id} correlation={bare_uuid}");
        let mut detector = DeterministicDetector::default();

        let findings = detector.detect(&text).expect("detect provider call ID");
        let values = findings
            .iter()
            .map(|finding| &text[finding.start..finding.end])
            .collect::<Vec<_>>();

        assert!(values.contains(&call_id));
        assert!(!values.contains(&bare_uuid));
    }

    #[test]
    fn deterministic_detection_covers_cross_platform_structural_values() {
        let text = concat!(
            "open /home/alice/private/file.txt and C:\\Users\\Alice\\private.txt; ",
            "connect https://user:password@db.internal/app from 192.168.10.8; ",
            "ignore 127.0.0.1 and use AcmePrivate, not main"
        );
        let mut detector = DeterministicDetector::new(vec![
            KnownPrivateValue {
                category: PrivacyCategory::Project,
                value: "AcmePrivate".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Branch,
                value: "main".to_string(),
            },
        ]);

        let findings = detector.detect(text).expect("detect structural values");
        let values = findings
            .iter()
            .map(|finding| (finding.category, &text[finding.start..finding.end]))
            .collect::<Vec<_>>();

        assert!(values.contains(&(PrivacyCategory::Path, "/home/alice/private/file.txt")));
        assert!(values.contains(&(PrivacyCategory::Path, "C:\\Users\\Alice\\private.txt")));
        assert!(values.iter().any(|(category, value)| {
            *category == PrivacyCategory::Secret
                && value.starts_with("https://user:password@db.internal/app")
        }));
        assert!(values.contains(&(PrivacyCategory::Host, "db.internal")));
        assert!(values.contains(&(PrivacyCategory::IpAddress, "192.168.10.8")));
        assert!(values.contains(&(PrivacyCategory::Project, "AcmePrivate")));
        assert!(!values.iter().any(|(_, value)| *value == "127.0.0.1"));
        assert!(!values.iter().any(|(_, value)| *value == "main"));
    }

    #[test]
    fn deterministic_detection_covers_short_authoritative_identifiers() {
        let text = "projects AI and go use dev";
        let mut detector = DeterministicDetector::new(vec![
            KnownPrivateValue {
                category: PrivacyCategory::Project,
                value: "AI".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Repository,
                value: "go".to_string(),
            },
            KnownPrivateValue {
                category: PrivacyCategory::Branch,
                value: "dev".to_string(),
            },
        ]);

        let findings = detector.detect(text).expect("detect short identifiers");
        let values = findings
            .iter()
            .map(|finding| (finding.category, &text[finding.start..finding.end]))
            .collect::<Vec<_>>();

        assert!(values.contains(&(PrivacyCategory::Project, "AI")));
        assert!(values.contains(&(PrivacyCategory::Repository, "go")));
        assert!(values.contains(&(PrivacyCategory::Branch, "dev")));
    }

    #[test]
    fn deterministic_detection_finds_bounded_absolute_unix_paths() {
        let text = concat!(
            "open /private/tmp/customer/data; mount /Volumes/Acme/file, ",
            "then /Users/alice/My Project/file next; ",
            "quoted \"/private/tmp/My File.txt\"; ",
            "uri file:///private/customer/from-uri; field cwd:/private/customer/from-field; ",
            "ignore https://example.com/public/path"
        );
        let mut detector = DeterministicDetector::default();

        let findings = detector.detect(text).expect("detect absolute paths");
        let paths = findings
            .iter()
            .filter(|finding| finding.category == PrivacyCategory::Path)
            .map(|finding| &text[finding.start..finding.end])
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![
                "/private/tmp/customer/data",
                "/Volumes/Acme/file",
                "/Users/alice/My Project/file",
                "/private/tmp/My File.txt",
                "/private/customer/from-uri",
                "/private/customer/from-field",
            ]
        );
    }

    #[test]
    fn deterministic_detection_marks_credential_urls_as_secrets() {
        let text = concat!(
            "reset https://example.com/reset?token=hunter2; ",
            "callback https://example.com/#/callback?%61ccess_token=abc123; ",
            "signed https://bucket.example/file?X-Amz-Signature=deadbeef; ",
            "public https://example.com/search?page=2"
        );
        let mut detector = DeterministicDetector::default();

        let findings = detector.detect(text).expect("detect credential URLs");
        let secret_values = findings
            .iter()
            .filter(|finding| finding.category == PrivacyCategory::Secret)
            .map(|finding| &text[finding.start..finding.end])
            .collect::<Vec<_>>();

        assert_eq!(secret_values.len(), 3);
        assert!(secret_values.iter().any(|value| value.contains("token=")));
        assert!(secret_values
            .iter()
            .any(|value| value.contains("%61ccess_token=")));
        assert!(secret_values
            .iter()
            .any(|value| value.contains("X-Amz-Signature=")));
        assert!(secret_values.iter().all(|value| !value.contains("page=2")));
    }

    #[test]
    fn credential_url_redaction_keeps_legal_value_punctuation() {
        let text = "open https://example.com/reset?token=abc;def,ghi next";
        let mut detector = DeterministicDetector::default();
        let findings = detector.detect(text).expect("detect credential URL");

        assert_eq!(findings.len(), 1);
        assert_eq!(
            &text[findings[0].start..findings[0].end],
            "https://example.com/reset?token=abc;def,ghi"
        );
        assert_eq!(
            filter_text(text, findings, |_, _| Ok(1))
                .expect("filter credential URL")
                .text,
            "open [SECRET] next"
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
    }
}
