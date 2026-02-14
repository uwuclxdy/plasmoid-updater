// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::exit_code::ExitCode;

pub fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

pub fn escalate_with_sudo() -> Result<ExitCode, libplasmoid_updater::Error> {
    let args: Vec<String> = std::env::args().collect();
    let mut cmd = std::process::Command::new("sudo");
    cmd.args(&args);

    preserve_user_environment(&mut cmd);

    let status = cmd
        .status()
        .map_err(|e| libplasmoid_updater::Error::other(format!("failed to run sudo: {e}")))?;

    Ok(exit_code_from_status(status.code()))
}

fn preserve_user_environment(cmd: &mut std::process::Command) {
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        cmd.env("XDG_DATA_HOME", data_home);
    }
    if let Ok(cache_home) = std::env::var("XDG_CACHE_HOME") {
        cmd.env("XDG_CACHE_HOME", cache_home);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("SUDO_USER_HOME", home);
    }
}

fn exit_code_from_status(code: Option<i32>) -> ExitCode {
    match code.unwrap_or(ExitCode::FatalError.as_i32()) {
        0 => ExitCode::Success,
        1 => ExitCode::PartialFailure,
        _ => ExitCode::FatalError,
    }
}
