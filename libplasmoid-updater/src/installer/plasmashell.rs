// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::Command;

use crate::{Error, Result, types::ComponentType};

fn get_user_id() -> String {
    std::env::var("UID").unwrap_or_else(|_| nix::unistd::Uid::current().as_raw().to_string())
}

/// Restarts the plasmashell service via systemd. The core never calls this;
/// the caller decides when (and whether) to.
///
/// # Errors
///
/// Returns [`Error::MissingTool`] when `systemctl` is not on `PATH`,
/// [`Error::Config`] naming the command when it cannot be spawned, and
/// [`Error::Command`] naming `systemctl` and its status when it exits nonzero.
pub fn restart_plasmashell() -> Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "restart", "plasma-plasmashell.service"]);

    let uid = get_user_id();

    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        );
    }

    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }

    let status = cmd.status().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::MissingTool { tool: "systemctl" },
        _ => Error::Config(format!("failed to run systemctl: {e}")),
    })?;

    if !status.success() {
        return Err(Error::Command {
            command: "systemctl",
            status,
        });
    }

    Ok(())
}

/// Returns `true` if this component type needs a plasmashell restart after
/// an update (widgets, plasma styles, global themes, splash screens and
/// KWin switchers).
pub(crate) fn requires_restart(component_type: ComponentType) -> bool {
    matches!(
        component_type,
        ComponentType::PlasmaWidget
            | ComponentType::PlasmaStyle
            | ComponentType::GlobalTheme
            | ComponentType::SplashScreen
            | ComponentType::KWinSwitcher
    )
}
