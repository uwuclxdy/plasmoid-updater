// SPDX-License-Identifier: GPL-3.0-or-later

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Nothing failed: the command did what it was asked to.
    Success = 0,
    /// At least one component failed, or the check could not settle one.
    Failure = 1,
    /// The command line named something the run cannot act on.
    Usage = 2,
    /// The run could not start or finish.
    Fatal = 3,
    /// `check` found updates to install.
    UpdatesAvailable = 100,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}
