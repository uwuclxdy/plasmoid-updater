// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

/// Errors that mean a run could not start or finish.
///
/// A problem with one component (a failed download, a checksum mismatch, a
/// failed install) is not an [`Error`]: it is a [`Failure`](crate::Failure)
/// row inside the returned [`UpdateResult`](crate::UpdateResult), and the rest
/// of the run continues.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The host is not Linux, the only platform this library supports.
    #[error("unsupported operating system: linux only")]
    UnsupportedOs,

    /// No KDE Plasma installation was detected.
    #[error("KDE Plasma desktop environment not detected")]
    NotKde,

    /// Another instance of this library holds the update lock.
    #[error("another plasmoid-updater instance is already running")]
    AlreadyRunning,

    /// A required external tool is not installed or not on `PATH`.
    #[error("missing required tool: {tool}")]
    MissingTool {
        /// Name of the missing executable (e.g. `bsdtar`).
        tool: &'static str,
    },

    /// A network request to the KDE Store or a download server failed.
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),

    /// The KDE Store asked the caller to slow down (HTTP 429) and no
    /// `Retry-After` hint was available.
    #[error("api rate limited, retry after backoff")]
    RateLimited,

    /// The KDE Store returned a non-success status.
    #[error("kde store returned error status {status}")]
    Api {
        /// The HTTP or OCS status code.
        status: u16,
    },

    /// A payload could not be parsed.
    #[error("failed to parse {what}: {detail}")]
    Parse {
        /// What failed to parse (e.g. `"ocs response"`).
        what: &'static str,
        /// The underlying parse error.
        detail: String,
    },

    /// A filesystem operation failed. Both the path and the underlying
    /// `std::io::Error` (with its errno) are preserved.
    #[error("{path}: {source}")]
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A configuration or environment problem the caller can fix.
    #[error("configuration error: {0}")]
    Config(String),
}
