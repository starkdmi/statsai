use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{DetectedSpan, DetectorKind, DetectorMetadata, PrivacyDetector, PrivacyError};

const OPF_MLX_CLIENT_VERSION: &str = "0.1.2";
const CHUNKING_VERSION: &str = "statsai-fixed-b4-t1024-c1024-o256-v5";
pub const MLX_FIXED_TRACE_PADDED_TOKENS: usize = 4 * 1_024;

#[derive(Clone, Debug)]
pub struct MlxServerOptions {
    pub memory_limit_gb: Option<f64>,
    pub cache_limit_gb: Option<f64>,
    pub max_batch_tokens: usize,
    pub viterbi_biases: Option<[f32; 6]>,
    pub viterbi_calibration_path: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub io_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub log_memory_stats: bool,
}

impl Default for MlxServerOptions {
    fn default() -> Self {
        Self {
            memory_limit_gb: None,
            cache_limit_gb: Some(0.5),
            max_batch_tokens: 32_768,
            viterbi_biases: None,
            viterbi_calibration_path: None,
            startup_timeout: Duration::from_secs(60),
            io_timeout: Duration::from_secs(5 * 60),
            shutdown_timeout: Duration::from_secs(2),
            log_memory_stats: false,
        }
    }
}

#[derive(Debug)]
pub struct MlxDetector {
    model_revision: String,
}

impl MlxDetector {
    #[must_use]
    pub fn metadata_for_revision(model_revision: impl Into<String>) -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::OpenAiPrivacyFilter,
            implementation_version: format!("{OPF_MLX_CLIENT_VERSION}+{CHUNKING_VERSION}"),
            model_revision: Some(model_revision.into()),
            offline: true,
        }
    }

    pub fn spawn(
        _server_executable: impl AsRef<Path>,
        _model: impl AsRef<Path>,
        _options: MlxServerOptions,
        _model_revision: impl Into<String>,
    ) -> Result<Self, PrivacyError> {
        Err(PrivacyError::UnsupportedPlatform)
    }
}

impl PrivacyDetector for MlxDetector {
    fn metadata(&self) -> DetectorMetadata {
        Self::metadata_for_revision(self.model_revision.clone())
    }

    fn detect_batch(&mut self, _texts: &[&str]) -> Result<Vec<Vec<DetectedSpan>>, PrivacyError> {
        Err(PrivacyError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_platform_fails_closed() {
        let error = MlxDetector::spawn(
            "opf-mlx-server",
            "model.mlxfn",
            MlxServerOptions::default(),
            "model-revision",
        )
        .expect_err("unsupported detector must fail");

        assert!(matches!(error, PrivacyError::UnsupportedPlatform));
        assert_eq!(error.code(), "unsupported_platform");
    }
}
