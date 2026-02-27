/// Internal errors that can occur during plasmoid-updater operations.
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

    #[error("no updates available")]
    NoUpdatesAvailable,
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
