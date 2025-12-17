// SPDX-License-Identifier: MIT OR Apache-2.0

pub type Result<T> = std::result::Result<T, Error>;

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

    #[error("kpackagetool6 failed: {0}")]
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

    #[error("{0}")]
    Other(String),

    #[error("system operations require root privileges")]
    RequiresSudo,

    #[error("running as root requires --system flag")]
    SudoWithoutSystem,
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
        other => Other,
    );

    pub fn checksum(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::ChecksumMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}
