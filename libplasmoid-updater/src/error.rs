// SPDX-License-Identifier: GPL-3.0-or-later

/// Errors that can occur during plasmoid-updater operations.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("unsupported operating system: {0}")]
    UnsupportedOS(String),

    #[error("KDE Plasma desktop environment not detected")]
    NotKDE,

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

    #[error("no updates available")]
    NoUpdatesAvailable,

    #[error("another plasmoid-updater instance is already running")]
    AlreadyRunning,
}

impl Error {
    /// Returns `true` for expected, non-error conditions (e.g., no updates found).
    pub fn is_skippable(&self) -> bool {
        matches!(self, Self::NoUpdatesAvailable | Self::ComponentNotFound(_) | Self::AlreadyRunning)
    }

    /// Returns `true` for temporary failures that may succeed on retry.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Network(_) | Self::RateLimited)
    }

    /// Returns `true` for permanent failures that require user intervention.
    pub fn is_fatal(&self) -> bool {
        !self.is_skippable() && !self.is_transient()
    }
}

macro_rules! error_ctor {
    ($($name:ident => $variant:ident),* $(,)?) => {
        $(
            pub(crate) fn $name(msg: impl Into<String>) -> Self {
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
        download => DownloadFailed,
        backup => BackupFailed,
        restart => RestartFailed,
    );

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    pub(crate) fn checksum(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::ChecksumMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}
