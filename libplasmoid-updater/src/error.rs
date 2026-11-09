// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

/// Errors that mean a run could not start or finish.
///
/// A problem with one component (a failed download, a checksum mismatch, a
/// failed install) is not an [`Error`]: it is a [`Failure`](crate::Failure)
/// row inside the returned [`UpdateResult`](crate::UpdateResult), and the rest
/// of the run continues.
///
/// The enum is `#[non_exhaustive]`: a new variant is not a breaking change, so
/// match with a wildcard arm. The struct-like variants that carry fields are
/// `#[non_exhaustive]` as well, so a field added to one later ships in a minor
/// release: an outside match names the fields it uses and ends with `..`. The
/// tuple variants ([`Error::Network`], [`Error::Config`], [`Error::Restart`])
/// stay exhaustive: their payload never grows, and a new payload shape arrives
/// as a new variant, which the enum's own `#[non_exhaustive]` already allows.
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
    #[non_exhaustive]
    MissingTool {
        /// Name of the missing executable (e.g. `bsdtar`).
        tool: &'static str,
    },

    /// A command the library ran exited nonzero.
    #[error("{}", command_error_line(.command, .status, .stderr))]
    #[non_exhaustive]
    Command {
        /// The command that failed (e.g. `systemctl`).
        command: &'static str,
        /// The exit status the command reported.
        status: std::process::ExitStatus,
        /// What the command printed on stderr, decoded lossily as UTF-8.
        stderr: String,
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
    #[non_exhaustive]
    Api {
        /// The HTTP or OCS status code.
        status: u16,
    },

    /// A payload could not be parsed.
    #[error("failed to parse {what}: {detail}")]
    #[non_exhaustive]
    Parse {
        /// What failed to parse (e.g. `"ocs response"`).
        what: &'static str,
        /// The underlying parse error.
        detail: String,
    },

    /// A filesystem operation failed. Both the path and the underlying
    /// `std::io::Error` (with its errno) are preserved.
    #[error("{path}: {source}")]
    #[non_exhaustive]
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A configuration or environment problem the caller can fix.
    #[error("configuration error: {0}")]
    Config(String),

    /// plasmashell could not be restarted safely, or no new plasmashell came up
    /// after the restart.
    #[error("{0}")]
    Restart(String),
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

/// Renders what a command printed on stderr as one line: each line trimmed,
/// blank lines dropped, the rest joined by one space. All-blank output renders
/// empty.
pub(crate) fn stderr_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`command_failure`], followed by the reason the command printed on stderr
/// rendered by [`stderr_line`]. A command that printed no reason keeps the
/// bare line.
fn command_error_line(command: &str, status: &std::process::ExitStatus, stderr: &str) -> String {
    let failure = command_failure(command, status);
    let reason = stderr_line(stderr);

    if reason.is_empty() {
        failure
    } else {
        format!("{failure}: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::{Error, stderr_line};

    #[test]
    fn stderr_renders_as_one_line_of_its_trimmed_non_blank_lines() {
        assert_eq!(
            stderr_line("  Job failed.  \n\n\t See the journal.\n \nfor details\n"),
            "Job failed. See the journal. for details"
        );
    }

    #[test]
    fn all_blank_stderr_renders_empty() {
        assert_eq!(stderr_line(" \n\t\n  \n"), "");
    }

    #[test]
    fn a_command_error_names_an_exit_status_code() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(1 << 8),
            stderr: String::new(),
        };

        assert_eq!(error.to_string(), "systemctl exited with status 1");
    }

    #[test]
    fn a_command_error_names_a_signal_death() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(9),
            stderr: String::new(),
        };

        assert_eq!(
            error.to_string(),
            "systemctl was killed by signal 9 (SIGKILL)"
        );
    }

    #[test]
    fn a_command_error_ends_with_the_reason_the_command_printed() {
        let error = Error::Command {
            command: "busctl",
            status: ExitStatus::from_raw(1 << 8),
            stderr: "Call failed: Access denied\n".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "busctl exited with status 1: Call failed: Access denied"
        );
    }

    #[test]
    fn a_command_error_names_a_signal_death_and_its_reason() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(9),
            stderr: "Terminated by the watchdog.\n".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "systemctl was killed by signal 9 (SIGKILL): Terminated by the watchdog."
        );
    }

    #[test]
    fn a_command_error_with_blank_stderr_keeps_the_bare_status_line() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(1 << 8),
            stderr: " \n\t\n  ".to_owned(),
        };

        assert_eq!(error.to_string(), "systemctl exited with status 1");
    }

    #[test]
    fn a_restart_error_renders_its_message() {
        let error = Error::Restart("no new plasmashell came up".to_owned());

        assert_eq!(error.to_string(), "no new plasmashell came up");
    }

    #[test]
    fn a_command_error_names_a_missing_status() {
        let error = Error::Command {
            command: "systemctl",
            status: ExitStatus::from_raw(0x7f),
            stderr: String::new(),
        };

        assert_eq!(error.to_string(), "systemctl exited without a status code");
    }
}
