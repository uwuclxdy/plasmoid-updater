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

impl Error {
    pub fn xml_parse(msg: impl Into<String>) -> Self {
        Self::XmlParse(msg.into())
    }

    pub fn extraction(msg: impl Into<String>) -> Self {
        Self::ExtractionFailed(msg.into())
    }

    pub fn install(msg: impl Into<String>) -> Self {
        Self::InstallFailed(msg.into())
    }

    pub fn id_resolution(name: impl Into<String>) -> Self {
        Self::IdResolutionFailed(name.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn download(msg: impl Into<String>) -> Self {
        Self::DownloadFailed(msg.into())
    }

    pub fn checksum(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::ChecksumMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    pub fn backup(msg: impl Into<String>) -> Self {
        Self::BackupFailed(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
