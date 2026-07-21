//! Local-only privacy detection primitives.
//!
//! Detectors return UTF-8 byte spans and never own persistence policy. The raw
//! conversation archive remains unchanged; merging, pseudonymization, and
//! filtered-dataset storage are separate layers.

mod dataset;
#[cfg(unix)]
mod kingfisher;
#[cfg(not(unix))]
#[path = "kingfisher_unsupported.rs"]
mod kingfisher;
#[cfg(unix)]
mod mlx;
#[cfg(not(unix))]
#[path = "mlx_unsupported.rs"]
mod mlx;
mod policy;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use dataset::{
    archive_privacy_input_fingerprint, filter_archive_conversation, privacy_policy_fingerprint,
    DetectorObservationSummary, FilterArchiveResult, FilteredConversation, FilteredDatasetManifest,
    FilteredFieldFinding, FILTERED_CONVERSATION_SCHEMA_VERSION, FILTERED_DATASET_SCHEMA_VERSION,
};
pub use kingfisher::{KingfisherDetector, KingfisherOptions};
pub use mlx::{MlxDetector, MlxServerOptions, MLX_FIXED_TRACE_PADDED_TOKENS};
pub use policy::{filter_text, normalize_private_value, FilteredText, PrivacyReplacement};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyCategory {
    AccountNumber,
    Address,
    Date,
    Email,
    Person,
    Phone,
    Url,
    Secret,
    Path,
    Host,
    IpAddress,
    Project,
    Repository,
    Branch,
    ToolCallId,
}

impl PrivacyCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccountNumber => "account_number",
            Self::Address => "address",
            Self::Date => "date",
            Self::Email => "email",
            Self::Person => "person",
            Self::Phone => "phone",
            Self::Url => "url",
            Self::Secret => "secret",
            Self::Path => "path",
            Self::Host => "host",
            Self::IpAddress => "ip_address",
            Self::Project => "project",
            Self::Repository => "repository",
            Self::Branch => "branch",
            Self::ToolCallId => "tool_call_id",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorKind {
    OpenAiPrivacyFilter,
    Kingfisher,
    Structured,
}

impl DetectorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiPrivacyFilter => "openai_privacy_filter",
            Self::Kingfisher => "kingfisher",
            Self::Structured => "structured",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionConfidence {
    Low,
    Medium,
    High,
}

impl DetectionConfidence {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetectedSpan {
    pub start: usize,
    pub end: usize,
    pub category: PrivacyCategory,
    pub detector: DetectorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<DetectionConfidence>,
}

impl DetectedSpan {
    pub fn validate_for(&self, text: &str) -> Result<(), PrivacyError> {
        if self.start >= self.end
            || self.end > text.len()
            || !text.is_char_boundary(self.start)
            || !text.is_char_boundary(self.end)
        {
            return Err(PrivacyError::InvalidSpan);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetectorMetadata {
    pub kind: DetectorKind,
    pub implementation_version: String,
    pub model_revision: Option<String>,
    pub offline: bool,
}

pub trait PrivacyDetector {
    fn metadata(&self) -> DetectorMetadata;

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError>;

    fn detect(&mut self, text: &str) -> Result<Vec<DetectedSpan>, PrivacyError> {
        take_single_result(
            self.detect_batch(&[text])?,
            "detector result count differs from single input",
        )
    }
}

fn take_single_result<T>(
    mut results: Vec<T>,
    mismatch_message: &'static str,
) -> Result<T, PrivacyError> {
    if results.len() != 1 {
        return Err(PrivacyError::Protocol(mismatch_message));
    }
    results
        .pop()
        .ok_or(PrivacyError::Protocol(mismatch_message))
}

#[derive(Default)]
pub struct PrivacyDetectorSet {
    detectors: Vec<Box<dyn PrivacyDetector>>,
}

impl PrivacyDetectorSet {
    pub fn new(detectors: Vec<Box<dyn PrivacyDetector>>) -> Self {
        Self { detectors }
    }

    pub fn push(&mut self, detector: impl PrivacyDetector + 'static) {
        self.detectors.push(Box::new(detector));
    }

    pub fn metadata(&self) -> Vec<DetectorMetadata> {
        self.detectors
            .iter()
            .map(|detector| detector.metadata())
            .collect()
    }

    pub fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        let mut combined = vec![Vec::new(); texts.len()];
        for detector in &mut self.detectors {
            let detected = detector.detect_batch(texts)?;
            if detected.len() != texts.len() {
                return Err(PrivacyError::Protocol(
                    "detector result count differs from input count",
                ));
            }
            for ((spans, additions), text) in combined.iter_mut().zip(detected).zip(texts) {
                for span in &additions {
                    span.validate_for(text)?;
                }
                spans.extend(additions);
            }
        }
        for spans in &mut combined {
            spans.sort_by_key(|span| {
                (
                    span.start,
                    span.end,
                    span.detector,
                    span.category,
                    span.confidence,
                )
            });
        }
        Ok(combined)
    }

    pub fn detect(&mut self, text: &str) -> Result<Vec<DetectedSpan>, PrivacyError> {
        take_single_result(
            self.detect_batch(&[text])?,
            "detector set result count differs from single input",
        )
    }
}

#[derive(Debug, Error)]
pub enum PrivacyError {
    #[error("privacy detector I/O failed")]
    Io(#[source] std::io::Error),
    #[error("privacy detector protocol failed: {0}")]
    Protocol(&'static str),
    #[error("privacy detector process timed out")]
    Timeout,
    #[error("privacy detector process is unavailable")]
    Unavailable,
    #[error("privacy detector is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("privacy detector returned an invalid UTF-8 span")]
    InvalidSpan,
    #[error("privacy pseudonym storage failed")]
    PseudonymStore,
    #[error(
        "filtered output still contains a privacy finding in {field_path} at {start}..{end} ({detector:?}/{category:?})"
    )]
    ResidualFinding {
        field_path: String,
        start: usize,
        end: usize,
        detector: DetectorKind,
        category: PrivacyCategory,
    },
    #[cfg(unix)]
    #[error("OpenAI Privacy Filter failed")]
    OpenAiPrivacyFilter(#[source] opf_mlx::Error),
}

impl PrivacyError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "detector_io",
            Self::Protocol(_) => "detector_protocol",
            Self::Timeout => "detector_timeout",
            Self::Unavailable => "detector_unavailable",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::InvalidSpan => "invalid_span",
            Self::PseudonymStore => "pseudonym_store",
            Self::ResidualFinding { .. } => "residual_finding",
            #[cfg(unix)]
            Self::OpenAiPrivacyFilter(_) => "openai_privacy_filter",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedDetector {
        kind: DetectorKind,
        span: DetectedSpan,
    }

    impl PrivacyDetector for FixedDetector {
        fn metadata(&self) -> DetectorMetadata {
            DetectorMetadata {
                kind: self.kind,
                implementation_version: "test".to_string(),
                model_revision: None,
                offline: true,
            }
        }

        fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
            Ok(texts.iter().map(|_| vec![self.span.clone()]).collect())
        }
    }

    struct ExtraResultDetector;

    impl PrivacyDetector for ExtraResultDetector {
        fn metadata(&self) -> DetectorMetadata {
            DetectorMetadata {
                kind: DetectorKind::Structured,
                implementation_version: "test".to_string(),
                model_revision: None,
                offline: true,
            }
        }

        fn detect_batch(&mut self, _: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
            Ok(vec![Vec::new(), Vec::new()])
        }
    }

    #[test]
    fn single_detection_rejects_extra_results() {
        let error = ExtraResultDetector.detect("text").unwrap_err();
        assert!(matches!(error, PrivacyError::Protocol(_)));
    }

    #[test]
    fn detector_set_preserves_independent_overlapping_findings() {
        let mut detectors = PrivacyDetectorSet::default();
        detectors.push(FixedDetector {
            kind: DetectorKind::OpenAiPrivacyFilter,
            span: DetectedSpan {
                start: 1,
                end: 4,
                category: PrivacyCategory::Person,
                detector: DetectorKind::OpenAiPrivacyFilter,
                confidence: None,
            },
        });
        detectors.push(FixedDetector {
            kind: DetectorKind::Kingfisher,
            span: DetectedSpan {
                start: 2,
                end: 5,
                category: PrivacyCategory::Secret,
                detector: DetectorKind::Kingfisher,
                confidence: Some(DetectionConfidence::High),
            },
        });

        let spans = detectors.detect("abcdef").unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].detector, DetectorKind::OpenAiPrivacyFilter);
        assert_eq!(spans[1].detector, DetectorKind::Kingfisher);
    }

    #[test]
    fn invalid_utf8_boundary_fails_closed() {
        let mut detectors = PrivacyDetectorSet::new(vec![Box::new(FixedDetector {
            kind: DetectorKind::Kingfisher,
            span: DetectedSpan {
                start: 1,
                end: 2,
                category: PrivacyCategory::Secret,
                detector: DetectorKind::Kingfisher,
                confidence: Some(DetectionConfidence::Medium),
            },
        })]);
        let error = detectors.detect("é").unwrap_err();
        assert!(matches!(error, PrivacyError::InvalidSpan));
    }
}
