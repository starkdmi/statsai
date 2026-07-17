use std::path::Path;
use std::time::Duration;

use crate::{DetectedSpan, DetectorKind, DetectorMetadata, PrivacyDetector, PrivacyError};

const HELPER_VERSION: &str = "0.2.0";
const KINGFISHER_VERSION: &str = "1.106.0";
const KINGFISHER_REVISION: &str = "8fa4f142bcd32664ac0feb16fc8aabc67637660d";

#[derive(Clone, Debug)]
pub struct KingfisherOptions {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for KingfisherOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Debug)]
pub struct KingfisherDetector;

impl KingfisherDetector {
    #[must_use]
    pub fn qualified_metadata() -> DetectorMetadata {
        DetectorMetadata {
            kind: DetectorKind::Kingfisher,
            implementation_version: format!(
                "statsai-kingfisher/{HELPER_VERSION}; kingfisher/{KINGFISHER_VERSION}"
            ),
            model_revision: Some(KINGFISHER_REVISION.to_string()),
            offline: true,
        }
    }

    pub fn spawn(
        _helper_executable: impl AsRef<Path>,
        _options: KingfisherOptions,
    ) -> Result<Self, PrivacyError> {
        Err(PrivacyError::UnsupportedPlatform)
    }
}

impl PrivacyDetector for KingfisherDetector {
    fn metadata(&self) -> DetectorMetadata {
        Self::qualified_metadata()
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
        let error = KingfisherDetector::spawn("kingfisher", KingfisherOptions::default())
            .expect_err("unsupported detector must fail");

        assert!(matches!(error, PrivacyError::UnsupportedPlatform));
        assert_eq!(error.code(), "unsupported_platform");
    }
}
