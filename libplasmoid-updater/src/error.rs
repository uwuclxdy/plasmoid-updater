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
    #[error(
        "KDE Plasma desktop environment not detected: pass --skip-plasma-detection to run anyway"
    )]
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

    /// A command the library ran exited nonzero.
    #[error("{}", command_failure(.command, .status))]
    Command {
        /// The command that failed (e.g. `systemctl`).
        command: &'static str,
        /// The exit status the command reported.
        status: std::process::ExitStatus,
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

/// Renders a failed command's exit status the way an error line names it: the
/// code it exited with, or the signal that killed it. Shared with the
/// installers that spawn external commands, so every site reads the same.
pub(crate) fn command_failure(command: &str, status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => format!("{command} exited with status {code}"),
        (None, Some(signal)) => match nix::sys::signal::Signal::try_from(signal) {
            Ok(name) => format!("{command} was killed by signal {signal} ({name:?})"),
            Err(_) => format!("{command} was killed by signal {signal}"),
        },
        (None, None) => format!("{command} exited without a status code"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::Error;

    #[test]
    fn a_command_error_names_an_exit_status_code() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(1 << 8),
        };

        assert_eq!(error.to_string(), "systemctl exited with status 1");
    }

    #[test]
    fn a_command_error_names_a_signal_death() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(9),
        };

        assert_eq!(
            error.to_string(),
            "systemctl was killed by signal 9 (SIGKILL)"
        );
    }

    #[test]
    fn a_command_error_names_a_missing_status() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(0x7f),
        };

        assert_eq!(error.to_string(), "systemctl exited without a status code");
    }
}
