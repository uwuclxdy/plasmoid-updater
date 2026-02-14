// SPDX-License-Identifier: MIT OR Apache-2.0

/// A specialized `Result` type for libplasmoid-updater operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during plasmoid-updater operations.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),

    #[error("api rate limited, retry after backoff")]
    RateLimited,

    #[error("api returned error status: {0}")]
    ApiError(u16),

    #[error("failed to parse xml: {0}")]
    XmlParse(String),

    #[error("failed to parse metadata.json: {0}")]
    MetadataParse(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("component not found: {0}")]
    ComponentNotFound(String),

    #[error("extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("installation failed: {0}")]
    InstallFailed(String),

    #[error("could not resolve content id for: {0}")]
    IdResolutionFailed(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("invalid version: {0}")]
    InvalidVersion(String),

    #[error("download failed: {0}")]
    DownloadFailed(String),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("metadata not found in package")]
    MetadataNotFound,

    #[error("backup failed: {0}")]
    BackupFailed(String),

    #[error("restart failed: {0}")]
    RestartFailed(String),

    #[error("{0}")]
    Other(String),

    #[error("system operations require root privileges")]
    RequiresSudo,

    #[error("running as root requires --system flag")]
    SudoWithoutSystem,

    #[error("no updates available")]
    NoUpdatesAvailable,
}

macro_rules! error_ctor {
    ($($name:ident => $variant:ident),* $(,)?) => {
        $(
            pub fn $name(msg: impl Into<String>) -> Self {
                Self::$variant(msg.into())
            }
        )*
    };
}

impl Error {
    error_ctor!(
        xml_parse => XmlParse,
        extraction => ExtractionFailed,
        install => InstallFailed,
        id_resolution => IdResolutionFailed,
        config => Config,
        download => DownloadFailed,
        backup => BackupFailed,
        restart => RestartFailed,
        other => Other,
    );

    pub fn checksum(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::ChecksumMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Returns true if this error represents an expected condition that can be safely skipped.
    ///
    /// Examples: NoUpdatesAvailable, ComponentNotFound
    ///
    /// # Usage in topgrade
    ///
    /// ```rust,no_run
    /// # use libplasmoid_updater::{run_default, Error};
    /// match run_default(false) {
    ///     Ok(summary) => {
    ///         println!("Updated: {}", summary.succeeded.len());
    ///     }
    ///     Err(e) if e.is_skippable() => {
    ///         println!("Skipping: {}", e);
    ///         // Return SkipStep in topgrade
    ///     }
    ///     Err(e) => {
    ///         eprintln!("Fatal error: {}", e);
    ///         // Return StepFailed in topgrade
    ///     }
    /// }
    /// ```
    pub fn is_skippable(&self) -> bool {
        matches!(
            self,
            Error::NoUpdatesAvailable | Error::ComponentNotFound(_)
        )
    }

    /// Returns true if this error is transient and might succeed on retry.
    ///
    /// Examples: Network errors, rate limiting
    ///
    /// Automation tools can use this to decide whether to retry the operation
    /// after a backoff period.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Network(_) | Error::RateLimited)
    }

    /// Returns true if this error represents a fatal condition.
    ///
    /// Examples: Permission errors, IO errors, installation failures
    ///
    /// Fatal errors indicate permanent failures that won't be resolved by retrying.
    pub fn is_fatal(&self) -> bool {
        !self.is_skippable() && !self.is_transient()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skippable_errors() {
        assert!(Error::NoUpdatesAvailable.is_skippable());
        assert!(Error::ComponentNotFound("test".to_string()).is_skippable());

        // Verify skippable errors are not transient or fatal
        assert!(!Error::NoUpdatesAvailable.is_transient());
        assert!(!Error::NoUpdatesAvailable.is_fatal());
    }

    #[test]
    fn test_transient_errors() {
        assert!(Error::RateLimited.is_transient());

        // Network errors (using a mock reqwest error would be complex,
        // so we'll trust the match pattern is correct)

        // Verify transient errors are not skippable
        assert!(!Error::RateLimited.is_skippable());
        assert!(!Error::RateLimited.is_fatal());
    }

    #[test]
    fn test_fatal_errors() {
        let fatal_errors = vec![
            Error::RequiresSudo,
            Error::SudoWithoutSystem,
            Error::InstallFailed("test".to_string()),
            Error::ExtractionFailed("test".to_string()),
            Error::DownloadFailed("test".to_string()),
            Error::BackupFailed("test".to_string()),
            Error::RestartFailed("test".to_string()),
            Error::Config("test".to_string()),
            Error::InvalidVersion("test".to_string()),
            Error::ChecksumMismatch {
                expected: "abc".to_string(),
                actual: "def".to_string(),
            },
            Error::MetadataNotFound,
            Error::IdResolutionFailed("test".to_string()),
            Error::ApiError(500),
            Error::XmlParse("test".to_string()),
            Error::Other("test".to_string()),
        ];

        for error in fatal_errors {
            assert!(
                error.is_fatal(),
                "Expected {:?} to be fatal",
                error
            );
            assert!(
                !error.is_skippable(),
                "Expected {:?} to not be skippable",
                error
            );
            assert!(
                !error.is_transient(),
                "Expected {:?} to not be transient",
                error
            );
        }
    }

    #[test]
    fn test_error_categorization_mutually_exclusive() {
        // Test that each error is in exactly one category
        let all_errors = vec![
            Error::NoUpdatesAvailable,
            Error::ComponentNotFound("test".to_string()),
            Error::RateLimited,
            Error::RequiresSudo,
            Error::InstallFailed("test".to_string()),
        ];

        for error in all_errors {
            let skippable = error.is_skippable();
            let transient = error.is_transient();
            let fatal = error.is_fatal();

            // Each error should be in exactly one category
            let count = [skippable, transient, fatal].iter().filter(|&&x| x).count();
            assert_eq!(
                count, 1,
                "Error {:?} should be in exactly one category: skippable={}, transient={}, fatal={}",
                error, skippable, transient, fatal
            );
        }
    }
}
