use std::path::Path;

pub use opf_mlx::ServerOptions as MlxServerOptions;
use opf_mlx::{Error as MlxError, FixedTraceShape, Label, PrivacyFilter};

use crate::{
    DetectedSpan, DetectorKind, DetectorMetadata, PrivacyCategory, PrivacyDetector, PrivacyError,
};

const CHUNKING_VERSION: &str = "statsai-fixed-b4-t1024-c1024-o256-v5";
const MLX_FIXED_TRACE_BATCH: usize = 4;
const MLX_FIXED_TRACE_TOKENS: usize = 1_024;
pub const MLX_FIXED_TRACE_PADDED_TOKENS: usize = MLX_FIXED_TRACE_BATCH * MLX_FIXED_TRACE_TOKENS;
const MLX_CHUNK_TOKENS: usize = MLX_FIXED_TRACE_TOKENS;
const MLX_CHUNK_OVERLAP_TOKENS: usize = 256;

pub struct MlxDetector {
    filter: PrivacyFilter,
    model_revision: String,
}

impl MlxDetector {
    #[must_use]
    pub fn metadata_for_revision(model_revision: impl Into<String>) -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::OpenAiPrivacyFilter,
            implementation_version: format!("{}+{CHUNKING_VERSION}", opf_mlx::CLIENT_VERSION),
            model_revision: Some(model_revision.into()),
            offline: true,
        }
    }

    fn new(filter: PrivacyFilter, model_revision: impl Into<String>) -> Self {
        Self {
            filter,
            model_revision: model_revision.into(),
        }
    }

    pub fn spawn(
        server_executable: impl AsRef<Path>,
        model: impl AsRef<Path>,
        options: MlxServerOptions,
        model_revision: impl Into<String>,
    ) -> Result<Self, PrivacyError> {
        let filter = PrivacyFilter::spawn_fixed(
            server_executable,
            model,
            options,
            FixedTraceShape {
                batch: MLX_FIXED_TRACE_BATCH,
                tokens: MLX_FIXED_TRACE_TOKENS,
            },
        )
        .map_err(map_mlx_error)?;
        Ok(Self::new(filter, model_revision))
    }
}

impl PrivacyDetector for MlxDetector {
    fn metadata(&self) -> DetectorMetadata {
        Self::metadata_for_revision(self.model_revision.clone())
    }

    fn detect_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        let detections = self
            .filter
            .detect_chunked_batch(texts, MLX_CHUNK_TOKENS, MLX_CHUNK_OVERLAP_TOKENS)
            .map_err(map_mlx_error)?;
        if detections.len() != texts.len() {
            return Err(PrivacyError::Protocol(
                "MLX result count differs from input count",
            ));
        }
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
        MlxError::Io(error) => PrivacyError::Io(error),
        MlxError::Server(_) => PrivacyError::Unavailable,
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
    use std::path::PathBuf;
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

        let io = map_mlx_error(MlxError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "test",
        )));
        assert!(matches!(io, PrivacyError::Io(_)));
        assert_eq!(io.code(), "detector_io");

        let server = map_mlx_error(MlxError::Server("test".to_string()));
        assert!(matches!(server, PrivacyError::Unavailable));
        assert_eq!(server.code(), "detector_unavailable");

        let provider_error = map_mlx_error(MlxError::Protocol("test".into()));
        assert!(matches!(
            provider_error,
            PrivacyError::OpenAiPrivacyFilter(_)
        ));
        assert_eq!(provider_error.code(), "openai_privacy_filter");
    }

    #[test]
    fn fixed_trace_metadata_records_the_execution_policy() {
        assert_eq!(MLX_FIXED_TRACE_PADDED_TOKENS, 4_096);
        assert_eq!(
            MlxDetector::metadata_for_revision("model").implementation_version,
            format!("{}+{CHUNKING_VERSION}", opf_mlx::CLIENT_VERSION)
        );
    }

    #[test]
    #[ignore = "requires OPF_MLX_SERVER and OPF_MLX_MODEL with Metal access"]
    fn fixed_trace_wrapper_handles_repeated_synthetic_input() {
        let server = PathBuf::from(std::env::var_os("OPF_MLX_SERVER").expect("OPF_MLX_SERVER"));
        let model = PathBuf::from(std::env::var_os("OPF_MLX_MODEL").expect("OPF_MLX_MODEL"));
        let options = MlxServerOptions {
            memory_limit_gb: Some(4.0),
            cache_limit_gb: Some(0.25),
            max_batch_tokens: MLX_FIXED_TRACE_PADDED_TOKENS,
            startup_timeout: Duration::from_secs(5 * 60),
            io_timeout: Duration::from_secs(5 * 60),
            log_memory_stats: true,
            ..MlxServerOptions::default()
        };
        let mut detector = MlxDetector::spawn(server, model, options, "integration-model")
            .expect("spawn fixed-trace detector");
        let text = "Contact Alice Smith at alice.smith@example.com. ".repeat(500);

        for _ in 0..2 {
            let findings = detector.detect(&text).expect("detect synthetic text");
            assert!(findings.iter().any(|span| span.start < span.end));
            for finding in findings {
                finding.validate_for(&text).expect("valid finding");
            }
        }
    }
}
