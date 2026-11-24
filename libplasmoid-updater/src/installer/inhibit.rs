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
    Dbus(zbus::OwnedFd),
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
    use zbus::Connection;
    use zbus::block_on;

    let conn: Connection = match block_on(Connection::system()) {
        Ok(c) => c,
        Err(e) => {
            log::debug!(target: "inhibit", "DBus connection failed: {e}");
            return None;
        }
    };

    let reply = match block_on(conn.call_method(
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
    )) {
        Ok(r) => r,
        Err(e) => {
            log::debug!(target: "inhibit", "logind Inhibit() call failed: {e}");
            return None;
        }
    };

    match reply.body().deserialize::<zbus::OwnedFd>() {
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

/// How long a spawned `systemd-inhibit` has to prove it failed before the spawn
/// counts as holding the inhibit.
///
/// A `systemd-inhibit` that cannot reach logind prints its error and exits;
/// one that holds the inhibit runs until it is killed. The wait costs the
/// first install about this long.
const INHIBIT_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

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

    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            log::debug!(target: "inhibit", "systemd-inhibit spawn failed: {e}");
            return None;
        }
    };

    let deadline = std::time::Instant::now() + INHIBIT_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                log::debug!(
                    target: "inhibit",
                    "systemd-inhibit exited within the grace ({status}); no inhibit held"
                );
                return None;
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                log::debug!(target: "inhibit", "acquired systemd-inhibit subprocess lock");
                return Some(InhibitGuard::Subprocess(child));
            }
            Ok(None) => std::thread::sleep(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .min(std::time::Duration::from_millis(10)),
            ),
            Err(e) => {
                log::debug!(target: "inhibit", "failed to check systemd-inhibit: {e}");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::installer::install::tests::{run_child, write_script};
    use crate::testutil::logs::captured_logs_while;

    const CHILD: &str =
        "installer::inhibit::tests::a_child_that_exits_at_once_warns_and_a_held_one_does_not";

    const WARNING_LINE: &str = "inhibit: could not inhibit system sleep; neither logind DBus nor systemd-inhibit available";

    /// A `systemd-inhibit` that exits within [`INHIBIT_GRACE`] holds no inhibit:
    /// it draws the existing warning and does not count as acquired. One that
    /// stays up is kept and draws none. A dead system bus address keeps the
    /// DBus tier out, so the subprocess tier is the one under test.
    #[test]
    fn a_child_that_exits_at_once_warns_and_a_held_one_does_not() {
        if std::env::var_os("PU_TEST_INHIBIT_CHILD").is_some() {
            let bin = std::env::var("PU_TEST_INHIBIT_BIN").unwrap();
            let fake = Path::new(&bin).join("systemd-inhibit");
            let done = std::env::var("PU_TEST_INHIBIT_DONE").unwrap();

            write_script(&fake, "#!/bin/sh\nexit 1\n");
            let started = Instant::now();
            let mut no_inhibit = false;
            let warnings = captured_logs_while(|| {
                no_inhibit = matches!(InhibitGuard::acquire(), InhibitGuard::None);
            });
            let elapsed = started.elapsed();
            assert!(
                no_inhibit,
                "a child that exited at once was counted as a held inhibit: {warnings:?}"
            );
            assert!(
                elapsed < INHIBIT_GRACE,
                "a failed inhibit child delayed the first install past the grace: {elapsed:?}"
            );
            assert!(
                warnings.iter().any(|line| line == WARNING_LINE),
                "a child that exited at once drew no inhibit warning: {warnings:?}"
            );

            write_script(&fake, "#!/bin/sh\nexec cat\n");
            let started = Instant::now();
            let mut held = false;
            let warnings = captured_logs_while(|| {
                held = matches!(InhibitGuard::acquire(), InhibitGuard::Subprocess(_));
            });
            let elapsed = started.elapsed();
            assert!(
                held,
                "a systemd-inhibit that stays up was not kept: {warnings:?}"
            );
            assert!(
                elapsed < INHIBIT_GRACE + Duration::from_millis(250),
                "a held inhibit delayed the first install past the grace: {elapsed:?}"
            );
            assert!(
                !warnings.iter().any(|line| line == WARNING_LINE),
                "a held inhibit warned: {warnings:?}"
            );
            std::fs::write(done, b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("systemd-inhibit"), "#!/bin/sh\nexit 1\n");
        let done = dir.path().join("child-done");
        let dead_bus = format!("unix:path={}", dir.path().join("no-such-bus").display());

        let output = run_child(
            CHILD,
            &bin,
            &[
                ("PU_TEST_INHIBIT_CHILD", "1"),
                ("PU_TEST_INHIBIT_BIN", bin.to_str().unwrap()),
                ("PU_TEST_INHIBIT_DONE", done.to_str().unwrap()),
                ("DBUS_SYSTEM_BUS_ADDRESS", &dead_bus),
            ],
        );
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
