// SPDX-License-Identifier: MIT OR Apache-2.0

use std::process::Command;

use crate::{AvailableUpdate, ComponentType, Error, InstalledComponent, Result};

fn get_user_id() -> Option<String> {
    std::env::var("UID").ok().or_else(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    })
}

/// Restarts the plasmashell service via systemd.
pub fn restart_plasmashell() -> Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "restart", "plasma-plasmashell.service"]);

    let uid = get_user_id();

    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err()
        && let Some(ref uid) = uid
    {
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        );
    }

    if std::env::var("XDG_RUNTIME_DIR").is_err()
        && let Some(ref uid) = uid
    {
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }

    let status = cmd
        .status()
        .map_err(|e| Error::restart(format!("failed to run systemctl: {e}")))?;

    if !status.success() {
        return Err(Error::restart(format!(
            "systemctl exited with status {status}"
        )));
    }

    Ok(())
}

/// Returns `true` if the component type requires a plasmashell restart after updating.
pub fn requires_plasmashell_restart(component: &InstalledComponent) -> bool {
    matches!(
        component.component_type,
        ComponentType::PlasmaWidget
            | ComponentType::PlasmaStyle
            | ComponentType::GlobalTheme
            | ComponentType::SplashScreen
            | ComponentType::KWinSwitcher
    )
}

/// Returns `true` if any of the updates require a plasmashell restart.
pub fn any_requires_restart(updates: &[AvailableUpdate]) -> bool {
    updates
        .iter()
        .any(|u| requires_plasmashell_restart(&u.installed))
}
