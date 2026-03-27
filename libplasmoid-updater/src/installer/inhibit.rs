// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::{Child, Command, Stdio};

/// RAII guard that inhibits system idle/sleep/shutdown while held.
///
/// Uses a 3-tier fallback:
/// 1. DBus logind `Inhibit()` call (requires `inhibit` feature / `zbus`)
/// 2. `systemd-inhibit` subprocess holding a `cat` process
/// 3. No-op with a warning (non-systemd systems or missing tools)
pub(crate) enum InhibitGuard {
    #[cfg(feature = "inhibit")]
    #[allow(dead_code)] // fd kept open intentionally to hold the inhibit lock
    Dbus(zbus::zvariant::OwnedFd),
    Subprocess(Child),
    None,
}

impl InhibitGuard {
    /// Acquires an inhibit lock. Never fails — falls through to `None` on error.
    pub(crate) fn acquire() -> Self {
        #[cfg(feature = "inhibit")]
        if let Some(guard) = try_dbus_inhibit() {
            return guard;
        }

        if let Some(guard) = try_subprocess_inhibit() {
            return guard;
        }

        log::warn!(
            target: "inhibit",
            "could not inhibit system sleep; neither logind DBus nor systemd-inhibit available"
        );
        Self::None
    }
}

impl Drop for InhibitGuard {
    fn drop(&mut self) {
        match self {
            #[cfg(feature = "inhibit")]
            Self::Dbus(_) => {
                log::debug!(target: "inhibit", "releasing DBus inhibit lock");
                // OwnedFd closes automatically
            }
            Self::Subprocess(child) => {
                log::debug!(target: "inhibit", "killing systemd-inhibit subprocess");
                let _ = child.kill();
                let _ = child.wait();
            }
            Self::None => {}
        }
    }
}

#[cfg(feature = "inhibit")]
fn try_dbus_inhibit() -> Option<InhibitGuard> {
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedFd;

    let conn: Connection = match Connection::system() {
        Ok(c) => c,
        Err(e) => {
            log::debug!(target: "inhibit", "DBus connection failed: {e}");
            return None;
        }
    };

    let reply = match conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &(
            "idle:sleep:shutdown",
            "plasmoid-updater",
            "Installing updates",
            "block",
        ),
    ) {
        Ok(r) => r,
        Err(e) => {
            log::debug!(target: "inhibit", "logind Inhibit() call failed: {e}");
            return None;
        }
    };

    match reply.body().deserialize::<OwnedFd>() {
        Ok(fd) => {
            log::debug!(target: "inhibit", "acquired logind DBus inhibit lock");
            Some(InhibitGuard::Dbus(fd))
        }
        Err(e) => {
            log::debug!(target: "inhibit", "failed to deserialize inhibit fd: {e}");
            None
        }
    }
}

fn try_subprocess_inhibit() -> Option<InhibitGuard> {
    let child = Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep:shutdown",
            "--who=plasmoid-updater",
            "--why=Installing updates",
            "cat",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            log::debug!(target: "inhibit", "acquired systemd-inhibit subprocess lock");
            Some(InhibitGuard::Subprocess(child))
        }
        Err(e) => {
            log::debug!(target: "inhibit", "systemd-inhibit spawn failed: {e}");
            None
        }
    }
}
