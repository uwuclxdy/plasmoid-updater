// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    PartialFailure = 1,
    FatalError = 2,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> i32 {
        code as i32
    }
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}
