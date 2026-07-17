use opf_mlx::{Error as MlxError, Label, PrivacyFilter};

use crate::{
    DetectedSpan, DetectorKind, DetectorMetadata, PrivacyCategory, PrivacyDetector, PrivacyError,
};

pub struct MlxDetector {
    filter: PrivacyFilter,
    model_revision: String,
}

impl MlxDetector {
    pub fn new(filter: PrivacyFilter, model_revision: impl Into<String>) -> Self {
        Self {
            filter,
            model_revision: model_revision.into(),
        }
    }
}

impl PrivacyDetector for MlxDetector {
    fn metadata(&self) -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::OpenAiPrivacyFilter,
            implementation_version: opf_mlx::CLIENT_VERSION.to_string(),
            model_revision: Some(self.model_revision.clone()),
            offline: true,
        }
    }

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        let detections = self.filter.detect_batch(texts).map_err(map_mlx_error)?;
        detections
            .into_iter()
            .zip(texts)
            .map(|(detection, text)| {
                detection
                    .spans
                    .into_iter()
                    .map(|span| {
                        let detected = DetectedSpan {
                            start: span.start,
                            end: span.end,
                            category: category(span.label),
                            detector: DetectorKind::OpenAiPrivacyFilter,
                            confidence: None,
                        };
                        detected.validate_for(text)?;
                        Ok(detected)
                    })
                    .collect()
            })
            .collect()
    }
}

fn map_mlx_error(error: MlxError) -> PrivacyError {
    match error {
        MlxError::Timeout { .. } => PrivacyError::Timeout,
        MlxError::Unavailable => PrivacyError::Unavailable,
        error => PrivacyError::OpenAiPrivacyFilter(error),
    }
}

const fn category(label: Label) -> PrivacyCategory {
    match label {
        Label::AccountNumber => PrivacyCategory::AccountNumber,
        Label::PrivateAddress => PrivacyCategory::Address,
        Label::PrivateDate => PrivacyCategory::Date,
        Label::PrivateEmail => PrivacyCategory::Email,
        Label::PrivatePerson => PrivacyCategory::Person,
        Label::PrivatePhone => PrivacyCategory::Phone,
        Label::PrivateUrl => PrivacyCategory::Url,
        Label::Secret => PrivacyCategory::Secret,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn maps_generic_process_errors_to_detector_independent_codes() {
        let timeout = map_mlx_error(MlxError::Timeout {
            operation: "test",
            timeout: Duration::from_secs(1),
        });
        assert!(matches!(timeout, PrivacyError::Timeout));
        assert_eq!(timeout.code(), "detector_timeout");

        let unavailable = map_mlx_error(MlxError::Unavailable);
        assert!(matches!(unavailable, PrivacyError::Unavailable));
        assert_eq!(unavailable.code(), "detector_unavailable");

        let provider_error = map_mlx_error(MlxError::Protocol("test".into()));
        assert!(matches!(
            provider_error,
            PrivacyError::OpenAiPrivacyFilter(_)
        ));
        assert_eq!(provider_error.code(), "openai_privacy_filter");
    }
}
